//! Outputs follow their USB device, not the ALSA card name.
//!
//! Identical DACs without serial numbers are named in probe order (`USB`,
//! `USB_1`); after a replug or a reboot the names can swap, and the master
//! would play the sub's filters. The master and each follower record the
//! [`DeviceIdentity`] of the card they were set to; [`reconcile`] runs at
//! start-up, on every change in the cards, and after every edit of the
//! outputs: it finds each identity among the cards present and renames the
//! output's device to the card that is it.
//!
//! - The only card of its model, for the only output of that model: it,
//!   whatever its name.
//! - Otherwise by serial, else by USB port. One output left over with one
//!   card of its model left over: that card (it moved ports).
//! - More left over than that (two identical DACs without serials both
//!   moved): nothing is guessed. Those outputs are held closed
//!   ([`Pin::Hold`]) and listed in [`Outcome::unresolved`] until the user
//!   says which is which (`assign_devices`).
//!
//! Each output's card name is pinned to its device
//! ([`audivane_io_alsa::identity::set_pins`]), so the engine does not open
//! a name that now belongs to another DAC in the moment before this runs.

use std::collections::{BTreeMap, HashMap, HashSet};

use audivane_io_alsa::identity::{CardFacts, Pin, card_token};
use audivane_protocol::ipc::{DeviceIdentity, DeviceInfo, EngineConfig, UnresolvedDevice};

/// The master's name in [`UnresolvedDevice::output`] and `assign_devices`.
pub const MASTER: &str = "master";

/// What one look at the cards decided for a config.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// What changed and why (one log line each).
    pub notes: Vec<String>,
    pub unresolved: Vec<UnresolvedDevice>,
    pub pins: Vec<(String, Pin)>,
}

struct Output {
    device: String,
    identity: Option<DeviceIdentity>,
    master: bool,
}

impl Output {
    fn label(&self) -> String {
        if self.master {
            "main output".into()
        } else {
            format!("follower {}", self.device)
        }
    }
}

/// The card `device` names now (by id, or by index for `hw:N`).
pub fn card_named<'a>(cards: &'a [CardFacts], device: &str) -> Option<&'a CardFacts> {
    let token = card_token(device)?;
    if token.bytes().all(|b| b.is_ascii_digit()) {
        let index: u32 = token.parse().ok()?;
        cards.iter().find(|c| c.index == index)
    } else {
        cards.iter().find(|c| c.id.eq_ignore_ascii_case(&token))
    }
}

/// The identity of the USB card `device` names now.
pub fn identity_of(cards: &[CardFacts], device: &str) -> Option<DeviceIdentity> {
    card_named(cards, device)?.identity()
}

/// `device` on the card `id`, keeping its device and subdevice numbers:
/// `hw:CARD=USB,DEV=0` → `hw:CARD=USB_1,DEV=0`, `hw:2,1` → `hw:CARD=X,DEV=1`.
pub fn renamed(device: &str, id: &str) -> String {
    let trimmed = device.trim();
    let (prefix, rest) = match trimmed.strip_prefix("plughw:") {
        Some(rest) => ("plughw:", rest),
        None => ("hw:", trimmed.strip_prefix("hw:").unwrap_or(trimmed)),
    };
    let mut dev = None;
    let mut sub = None;
    for (i, part) in rest.split(',').map(str::trim).enumerate().skip(1) {
        let upper = part.to_ascii_uppercase();
        if let Some(n) = upper.strip_prefix("DEV=") {
            dev = Some(n.to_string());
        } else if let Some(n) = upper.strip_prefix("SUBDEV=") {
            sub = Some(n.to_string());
        } else if i == 1 {
            dev = Some(part.to_string());
        } else if i == 2 {
            sub = Some(part.to_string());
        }
    }
    let mut name = format!("{prefix}CARD={id},DEV={}", dev.as_deref().unwrap_or("0"));
    if let Some(sub) = sub {
        name.push_str(&format!(",SUBDEV={sub}"));
    }
    name
}

/// `USB_1` → `USB`: the name ALSA gives the first card of a model.
fn base_id(id: &str) -> &str {
    match id.rsplit_once('_') {
        Some((base, n))
            if !base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => id,
    }
}

/// Find every output's device among `cards` and rename `cfg`'s devices to
/// match (see the module docs). Outputs without an identity that name a
/// USB card present gain its identity (a config from before identities, a
/// device just chosen). Cards that are not USB, and outputs on them, are
/// left exactly as they are.
pub fn reconcile(cfg: &mut EngineConfig, cards: &[CardFacts]) -> Outcome {
    let mut out = Outcome::default();
    let mut outputs: Vec<Output> = std::iter::once(Output {
        device: cfg.device.clone(),
        identity: cfg.device_identity.clone(),
        master: true,
    })
    .chain(cfg.outputs.followers.iter().map(|f| Output {
        device: f.device.clone(),
        identity: f.identity.clone(),
        master: false,
    }))
    .collect();
    let before: Vec<String> = outputs.iter().map(|o| o.device.clone()).collect();

    for o in outputs.iter_mut().filter(|o| o.identity.is_none()) {
        if let Some(id) = identity_of(cards, &o.device) {
            out.notes
                .push(format!("{}: {} is {id}; recorded", o.label(), o.device));
            o.identity = Some(id);
        }
    }

    // Outputs and cards by model.
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, o) in outputs.iter().enumerate() {
        if let Some(id) = &o.identity {
            groups.entry(id.usb_id.clone()).or_default().push(i);
        }
    }
    let mut assigned: Vec<Option<usize>> = vec![None; outputs.len()];
    let mut held = vec![false; outputs.len()];
    let mut candidates_of: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut shared: HashSet<String> = HashSet::new();
    let mut claimed = vec![false; cards.len()];
    for (usb_id, members) in &groups {
        let cand: Vec<usize> = (0..cards.len())
            .filter(|&c| cards[c].usb_id.as_deref() == Some(usb_id.as_str()))
            .collect();
        if members.len() == 1 && cand.len() <= 1 {
            if let Some(&c) = cand.first() {
                assigned[members[0]] = Some(c);
                claimed[c] = true;
            }
            continue;
        }
        shared.insert(usb_id.clone());
        let ident = |i: usize| outputs[i].identity.as_ref().expect("grouped by identity");
        // By serial, then (serial-less devices) by port.
        for &i in members {
            let id = ident(i);
            let Some(serial) = &id.serial else { continue };
            if let Some(&c) = cand
                .iter()
                .find(|&&c| !claimed[c] && cards[c].serial.as_ref() == Some(serial))
            {
                assigned[i] = Some(c);
                claimed[c] = true;
            }
        }
        for &i in members {
            let id = ident(i);
            if assigned[i].is_some() || id.serial.is_some() || id.port.is_none() {
                continue;
            }
            if let Some(&c) = cand
                .iter()
                .find(|&&c| !claimed[c] && cards[c].serial.is_none() && cards[c].port == id.port)
            {
                assigned[i] = Some(c);
                claimed[c] = true;
            }
        }
        let left: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&i| assigned[i].is_none() && ident(i).serial.is_none())
            .collect();
        let free: Vec<usize> = cand
            .iter()
            .copied()
            .filter(|&c| !claimed[c] && cards[c].serial.is_none())
            .collect();
        match (left.as_slice(), free.as_slice()) {
            ([], _) | (_, []) => {}
            ([i], [c]) => {
                let id = ident(*i);
                out.notes.push(format!(
                    "{}: {} moved from USB port {} to {}",
                    outputs[*i].label(),
                    id.card,
                    id.port_chain().unwrap_or("?"),
                    cards[*c]
                        .identity()
                        .as_ref()
                        .and_then(DeviceIdentity::port_chain)
                        .unwrap_or("?"),
                ));
                assigned[*i] = Some(*c);
                claimed[*c] = true;
            }
            _ => {
                for &i in &left {
                    held[i] = true;
                    candidates_of.insert(i, free.clone());
                }
            }
        }
    }

    // Rename the found ones to their card's current name.
    for (i, o) in outputs.iter_mut().enumerate() {
        let Some(c) = assigned[i] else { continue };
        let card = &cards[c];
        let name = renamed(&o.device, &card.id);
        if !name.eq_ignore_ascii_case(&o.device) {
            out.notes.push(format!(
                "{}: {} is now {name} (the same DAC: {})",
                o.label(),
                o.device,
                o.identity
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            ));
            o.device = name;
        }
        o.identity = card.identity();
    }
    // The others (unplugged, or held) must not keep a name that is now a
    // card that is not them: they take one no card or output has, the
    // name ALSA gives the next card of the model.
    // A held output keeps its name unless a found one took it (its pin
    // holds it closed meanwhile).
    let card_ids: HashSet<String> = cards.iter().map(|c| c.id.to_ascii_lowercase()).collect();
    let mut taken: HashSet<String> = HashSet::new();
    for (i, o) in outputs.iter().enumerate() {
        if assigned[i].is_some() || o.identity.is_none() {
            taken.extend(card_token(&o.device));
        }
    }
    for i in 0..outputs.len() {
        if assigned[i].is_some() || outputs[i].identity.is_none() {
            continue;
        }
        let token = card_token(&outputs[i].device);
        let collides = token
            .as_ref()
            .is_some_and(|t| taken.contains(t) || (!held[i] && card_ids.contains(t)));
        if collides {
            let taken_or_card = |id: &str| {
                let id = id.to_ascii_lowercase();
                taken.contains(&id) || card_ids.contains(&id)
            };
            let base = base_id(&outputs[i].identity.as_ref().expect("checked").card).to_string();
            let id = std::iter::once(base.clone())
                .chain((1..).map(|n| format!("{base}_{n}")))
                .find(|id| !taken_or_card(id))
                .expect("an unbounded list");
            let name = renamed(&outputs[i].device, &id);
            out.notes.push(format!(
                "{}: {} is another device now; it waits for its own DAC as {name}",
                outputs[i].label(),
                outputs[i].device
            ));
            outputs[i].device = name;
        }
        taken.extend(card_token(&outputs[i].device));
    }

    // Pins, and the outputs held.
    for (i, o) in outputs.iter().enumerate() {
        let (Some(id), Some(token)) = (&o.identity, card_token(&o.device)) else {
            continue;
        };
        let pin = if held[i] {
            Pin::Hold
        } else {
            let shared = shared.contains(&id.usb_id);
            Pin::Is {
                usb_id: id.usb_id.clone(),
                serial: id.serial.clone().filter(|_| shared),
                port: id.port.clone().filter(|_| shared && id.serial.is_none()),
            }
        };
        out.pins.push((token, pin));
        if held[i] {
            out.unresolved.push(UnresolvedDevice {
                output: if o.master {
                    MASTER.into()
                } else {
                    o.device.clone()
                },
                device: o.device.clone(),
                identity: id.clone(),
                candidates: candidates_of
                    .get(&i)
                    .into_iter()
                    .flatten()
                    .map(|&c| renamed(&o.device, &cards[c].id))
                    .collect(),
            });
        }
    }
    if !out.unresolved.is_empty() {
        out.notes.push(format!(
            "identical DACs changed USB ports; not playing on {} until told which is which",
            out.unresolved
                .iter()
                .map(|u| u.device.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // Write back; a calibration's follower parts follow their follower.
    let mut outputs = outputs.into_iter();
    let master = outputs.next().expect("the master is first");
    cfg.device = master.device;
    cfg.device_identity = master.identity;
    let mut moved: HashMap<String, String> = HashMap::new();
    for ((f, o), old) in cfg
        .outputs
        .followers
        .iter_mut()
        .zip(outputs)
        .zip(&before[1..])
    {
        if o.device != *old {
            moved.insert(old.clone(), o.device.clone());
        }
        f.device = o.device;
        f.identity = o.identity;
    }
    if let Some(cal) = cfg.calibration.as_mut() {
        for part in &mut cal.followers {
            if let Some(new) = moved.get(&part.device) {
                part.device = new.clone();
            }
        }
    }
    out
}

/// Start-up, before the engine opens anything: find the outputs' devices
/// among the cards now (an old config gains its identities here), pin
/// them, and say whether the config changed (to be saved).
pub fn reconcile_at_start(cfg: &mut EngineConfig) -> bool {
    let cards = audivane_io_alsa::identity::scan_cards(&Default::default());
    let before = cfg.clone();
    let outcome = reconcile(cfg, &cards);
    for note in &outcome.notes {
        log::info!("{note}");
    }
    audivane_io_alsa::identity::set_pins(outcome.pins);
    *cfg != before
}

/// After an edit: an output whose device did not change keeps the identity
/// it had (a client's copy of the config cannot change it); one set to
/// another device takes the identity of the card it names now (`None`
/// when that is not a USB card, or not there).
pub fn carry(old: &EngineConfig, next: &mut EngineConfig, cards: &[CardFacts]) {
    next.device_identity = if next.device == old.device {
        old.device_identity.clone()
    } else {
        identity_of(cards, &next.device)
    };
    for f in &mut next.outputs.followers {
        f.identity = match old.outputs.followers.iter().find(|g| g.device == f.device) {
            Some(g) => g.identity.clone(),
            None => identity_of(cards, &f.device),
        };
    }
}

/// [`carry`] for the daemon's own edits: an output whose device did not
/// change keeps the identity the edit left it (`assign_devices` sets it),
/// one set to another device takes the identity of the card it names now.
pub fn fill(old: &EngineConfig, next: &mut EngineConfig, cards: &[CardFacts]) {
    if next.device != old.device {
        next.device_identity = identity_of(cards, &next.device);
    }
    for f in &mut next.outputs.followers {
        if !old.outputs.followers.iter().any(|g| g.device == f.device) {
            f.identity = identity_of(cards, &f.device);
        }
    }
}

/// Whether an edit changed the outputs' devices or identities.
pub fn outputs_changed(old: &EngineConfig, next: &EngineConfig) -> bool {
    devices_changed(old, next)
        || old.device_identity != next.device_identity
        || old
            .outputs
            .followers
            .iter()
            .zip(&next.outputs.followers)
            .any(|(a, b)| a.identity != b.identity)
}

/// Whether an edit changed which devices the outputs name.
pub fn devices_changed(old: &EngineConfig, next: &EngineConfig) -> bool {
    old.device != next.device
        || old.outputs.followers.len() != next.outputs.followers.len()
        || old
            .outputs
            .followers
            .iter()
            .zip(&next.outputs.followers)
            .any(|(a, b)| a.device != b.device)
}

/// Each listed device's identity, and what tells it apart from another
/// card of its model: its serial when it has one, else its USB port chain
/// without the hubs the cards share (`1.4.3` and `1.4.1.4` → `USB port 3`,
/// `USB port 1.4`).
pub fn annotate(devices: &mut [DeviceInfo], cards: &[CardFacts]) {
    let mut by_model: HashMap<&str, Vec<&CardFacts>> = HashMap::new();
    for c in cards {
        if let Some(u) = c.usb_id.as_deref() {
            by_model.entry(u).or_default().push(c);
        }
    }
    let mut suffix: HashMap<u32, String> = HashMap::new();
    for group in by_model.values().filter(|g| g.len() > 1) {
        let chains: Vec<Vec<&str>> = group
            .iter()
            .map(|c| {
                c.port
                    .as_deref()
                    .map(|p| p.rsplit_once("-usb-0:").map_or(p, |(_, chain)| chain))
                    .unwrap_or("")
                    .split('.')
                    .collect()
            })
            .collect();
        let shortest = chains.iter().map(Vec::len).min().unwrap_or(0);
        let common = (0..shortest.saturating_sub(1))
            .take_while(|&k| chains.iter().all(|ch| ch[k] == chains[0][k]))
            .count();
        let short: Vec<String> = chains.iter().map(|ch| ch[common..].join(".")).collect();
        let distinct = short.iter().collect::<HashSet<_>>().len() == short.len();
        for (k, c) in group.iter().enumerate() {
            let text = match (&c.serial, &c.port) {
                (Some(serial), _) => format!("serial {serial}"),
                (None, Some(_)) if distinct => format!("USB port {}", short[k]),
                (None, Some(port)) => format!("USB {port}"),
                (None, None) => format!("card {}", c.id),
            };
            suffix.insert(c.index, text);
        }
    }
    for d in devices {
        let Some(card) = cards.iter().find(|c| c.id.eq_ignore_ascii_case(&d.card)) else {
            continue;
        };
        d.identity = card.identity();
        d.suffix = suffix.get(&card.index).cloned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_io_alsa::identity::scan_cards;
    use audivane_io_alsa::identity::testing::FakeTree;
    use audivane_protocol::ipc::FollowerConfig;

    const USB: &str = "2522:0007";
    const PORT_A: &str = "1.4.3";
    const PORT_B: &str = "1.4.1.4";

    fn config(master: &str, follower: &str) -> EngineConfig {
        let mut cfg = EngineConfig {
            device: master.into(),
            ..EngineConfig::default()
        };
        cfg.outputs.followers.push(FollowerConfig::new(follower));
        cfg
    }

    fn port(chain: &str) -> Option<String> {
        Some(format!("pci-0000:00:14.0-usb-0:{chain}"))
    }

    #[test]
    fn swapped_names_between_boots_follow_the_ports() {
        let t = FakeTree::new("swap");
        // First boot: the mains' DAC on port A probed first.
        t.usb(1, "USB", USB, PORT_A, None);
        t.usb(2, "USB_1", USB, PORT_B, None);
        let mut cfg = config("hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0");
        let first = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert!(first.unresolved.is_empty());
        assert_eq!(cfg.device, "hw:CARD=USB,DEV=0");
        assert_eq!(cfg.device_identity.as_ref().unwrap().port, port(PORT_A));
        assert_eq!(
            cfg.outputs.followers[0].identity.as_ref().unwrap().port,
            port(PORT_B)
        );
        // Next boot: port B probed first, the names swap.
        t.clear();
        t.usb(1, "USB", USB, PORT_B, None);
        t.usb(2, "USB_1", USB, PORT_A, None);
        let next = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert!(next.unresolved.is_empty());
        assert_eq!(cfg.device, "hw:CARD=USB_1,DEV=0");
        assert_eq!(cfg.outputs.followers[0].device, "hw:CARD=USB,DEV=0");
        assert!(
            next.notes
                .iter()
                .any(|n| n.contains("is now hw:CARD=USB_1,DEV=0"))
        );
        // The pins hold each name to its port.
        let pin = |name: &str| next.pins.iter().find(|(t, _)| t == name).unwrap().1.clone();
        assert_eq!(
            pin("usb_1"),
            Pin::Is {
                usb_id: USB.into(),
                serial: None,
                port: port(PORT_A)
            }
        );
        let cards = scan_cards(&t.roots);
        assert!(pin("usb").allows(card_named(&cards, "hw:USB").unwrap()));
        assert!(!pin("usb").allows(card_named(&cards, "hw:USB_1").unwrap()));
        // Nothing changes on another look.
        let before = cfg.clone();
        let again = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert_eq!(cfg, before);
        assert!(again.notes.is_empty());
    }

    #[test]
    fn a_serial_wins_over_the_port() {
        let t = FakeTree::new("serial");
        t.usb(1, "USB", USB, PORT_A, Some("AAA"));
        t.usb(2, "USB_1", USB, PORT_B, Some("BBB"));
        let mut cfg = config("hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0");
        reconcile(&mut cfg, &scan_cards(&t.roots));
        assert_eq!(
            cfg.device_identity.as_ref().unwrap().serial.as_deref(),
            Some("AAA")
        );
        // Both moved, and swapped ports and names: the serials tell them
        // apart.
        t.clear();
        t.usb(1, "USB", USB, "2", Some("BBB"));
        t.usb(2, "USB_1", USB, PORT_A, Some("AAA"));
        let out = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert!(out.unresolved.is_empty());
        assert_eq!(cfg.device, "hw:CARD=USB_1,DEV=0");
        assert_eq!(cfg.outputs.followers[0].device, "hw:CARD=USB,DEV=0");
        assert!(out.pins.iter().any(|(t, p)| t == "usb_1"
            && *p
                == Pin::Is {
                    usb_id: USB.into(),
                    serial: Some("AAA".into()),
                    port: None
                }));
    }

    #[test]
    fn a_unique_card_is_found_by_its_model_as_before() {
        let t = FakeTree::new("unique");
        t.plain(0, "PCH");
        t.usb(1, "USB2", "1234:5678", PORT_A, None);
        let mut cfg = config("hw:CARD=USB2,DEV=0", "hw:CARD=PCH,DEV=0");
        reconcile(&mut cfg, &scan_cards(&t.roots));
        let recorded = cfg.clone();
        assert!(cfg.outputs.followers[0].identity.is_none());
        // Moved to another port: still it, no rename, no hold.
        t.clear();
        t.plain(0, "PCH");
        t.usb(1, "USB2", "1234:5678", "2", None);
        let out = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert!(out.unresolved.is_empty());
        assert_eq!(cfg.device, recorded.device);
        assert_eq!(cfg.outputs.followers[0], recorded.outputs.followers[0]);
        assert_eq!(
            out.pins,
            vec![(
                "usb2".to_string(),
                Pin::Is {
                    usb_id: "1234:5678".into(),
                    serial: None,
                    port: None
                }
            )]
        );
        // Unplugged: the config stays, the name waits for it.
        t.clear();
        t.plain(0, "PCH");
        let before = cfg.clone();
        reconcile(&mut cfg, &scan_cards(&t.roots));
        assert_eq!(cfg, before);
    }

    #[test]
    fn two_identical_dacs_that_both_moved_are_held_not_guessed() {
        let t = FakeTree::new("ambiguous");
        t.usb(1, "USB", USB, PORT_A, None);
        t.usb(2, "USB_1", USB, PORT_B, None);
        let mut cfg = config("hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0");
        reconcile(&mut cfg, &scan_cards(&t.roots));
        t.clear();
        t.usb(1, "USB", USB, "2", None);
        t.usb(2, "USB_1", USB, "3", None);
        let before = cfg.clone();
        let out = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert_eq!(cfg, before, "nothing is renamed");
        assert_eq!(out.unresolved.len(), 2);
        assert_eq!(out.unresolved[0].output, MASTER);
        assert_eq!(out.unresolved[1].output, "hw:CARD=USB_1,DEV=0");
        assert_eq!(
            out.unresolved[0].candidates,
            vec!["hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0"]
        );
        assert!(out.pins.iter().all(|(_, p)| *p == Pin::Hold));
        // One of them back on its port: the other is the one left.
        t.clear();
        t.usb(1, "USB", USB, "3", None);
        t.usb(2, "USB_1", USB, PORT_A, None);
        let out = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert!(out.unresolved.is_empty());
        assert_eq!(cfg.device, "hw:CARD=USB_1,DEV=0");
        assert_eq!(cfg.outputs.followers[0].device, "hw:CARD=USB,DEV=0");
        assert_eq!(
            cfg.outputs.followers[0].identity.as_ref().unwrap().port,
            port("3")
        );
    }

    #[test]
    fn an_old_config_gains_the_identity_of_the_card_it_names() {
        let t = FakeTree::new("migrate");
        t.plain(0, "Loopback");
        t.usb(1, "USB", USB, PORT_A, None);
        t.usb(2, "USB_1", USB, PORT_B, None);
        // A config from before identities.
        let mut cfg: EngineConfig = serde_json::from_value(serde_json::json!({
            "device": "hw:CARD=USB_1,DEV=0",
            "outputs": {"followers": [{"device": "hw:CARD=USB,DEV=0"}, {"device": "hw:CARD=Loopback,DEV=0"}]},
        }))
        .unwrap();
        let out = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert_eq!(cfg.device, "hw:CARD=USB_1,DEV=0");
        assert_eq!(cfg.device_identity.as_ref().unwrap().port, port(PORT_B));
        assert_eq!(
            cfg.outputs.followers[0].identity.as_ref().unwrap().port,
            port(PORT_A)
        );
        assert_eq!(cfg.outputs.followers[1].identity, None);
        assert_eq!(
            out.notes.iter().filter(|n| n.contains("recorded")).count(),
            2
        );
        // It is saved with them.
        let saved = serde_json::to_value(&cfg).unwrap();
        assert_eq!(saved["device_identity"]["usb_id"], USB);
        assert_eq!(saved["outputs"]["followers"][0]["identity"]["card"], "USB");
        assert!(saved["outputs"]["followers"][1].get("identity").is_none());
    }

    #[test]
    fn an_unplugged_output_gives_its_name_up_to_the_dac_that_took_it() {
        let t = FakeTree::new("collide");
        t.usb(1, "USB", USB, PORT_A, None);
        t.usb(2, "USB_1", USB, PORT_B, None);
        let mut cfg = config("hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0");
        reconcile(&mut cfg, &scan_cards(&t.roots));
        // Only the sub's DAC (port B) is back; it is USB now.
        t.clear();
        t.usb(1, "USB", USB, PORT_B, None);
        let out = reconcile(&mut cfg, &scan_cards(&t.roots));
        assert!(out.unresolved.is_empty());
        assert_eq!(cfg.outputs.followers[0].device, "hw:CARD=USB,DEV=0");
        assert_eq!(cfg.device, "hw:CARD=USB_1,DEV=0");
        crate::control::config::validate_outputs(&cfg).unwrap();
        // The mains' DAC comes back as USB_1 on port A: it plays.
        t.usb(2, "USB_1", USB, PORT_A, None);
        let cards = scan_cards(&t.roots);
        let out = reconcile(&mut cfg, &cards);
        assert_eq!(cfg.device, "hw:CARD=USB_1,DEV=0");
        let (_, pin) = out.pins.iter().find(|(t, _)| t == "usb_1").unwrap();
        assert!(pin.allows(card_named(&cards, "hw:USB_1").unwrap()));
    }

    #[test]
    fn a_renamed_follower_keeps_its_settings_and_calibration_part() {
        use audivane_protocol::ipc::OutputChannels;
        let t = FakeTree::new("bundle");
        t.usb(1, "USB", USB, PORT_A, None);
        t.usb(2, "USB_1", USB, PORT_B, None);
        let mut cfg = config("hw:CARD=USB,DEV=0", "hw:CARD=USB_1,DEV=0");
        cfg.output_channels = Some(OutputChannels([2, 3]));
        {
            let f = &mut cfg.outputs.followers[0];
            f.output_channels = Some(OutputChannels([4, 5]));
            f.follow_volume = true;
            f.follow_ref_db = Some(-20.0);
            f.volume_db = -6.0;
            f.trim_ms = 1.0;
        }
        cfg.calibration = Some(
            serde_json::from_value(serde_json::json!({
                "path": "/x/manifest.json", "label": "room", "design_rate_hz": 48000,
                "followers": [{
                    "device": "hw:CARD=USB_1,DEV=0",
                    "item": {"ir_path": "", "calibration": true},
                    "trim_ms": 1.0,
                    "manifest_gain_db": -3.5,
                    "restore": {"trim_ms": 0.0},
                }],
            }))
            .unwrap(),
        );
        reconcile(&mut cfg, &scan_cards(&t.roots));
        t.clear();
        t.usb(1, "USB", USB, PORT_B, None);
        t.usb(2, "USB_1", USB, PORT_A, None);
        reconcile(&mut cfg, &scan_cards(&t.roots));
        let cal = cfg.calibration.as_ref().unwrap();
        assert_eq!(cal.followers[0].device, "hw:CARD=USB,DEV=0");
        assert_eq!(cal.followers[0].manifest_gain_db, Some(-3.5));
        assert_eq!(cal.followers[0].trim_ms, 1.0);
        let f = &cfg.outputs.followers[0];
        assert_eq!(f.device, "hw:CARD=USB,DEV=0");
        assert_eq!(f.output_channels, Some(OutputChannels([4, 5])));
        assert_eq!((f.follow_volume, f.follow_ref_db), (true, Some(-20.0)));
        assert_eq!((f.volume_db, f.trim_ms), (-6.0, 1.0));
        assert_eq!(cfg.device, "hw:CARD=USB_1,DEV=0");
        assert_eq!(cfg.output_channels, Some(OutputChannels([2, 3])));
    }

    #[test]
    fn identical_cards_are_told_apart_in_the_list() {
        let t = FakeTree::new("annotate");
        t.plain(0, "PCH");
        t.usb(1, "USB", USB, PORT_A, None);
        t.usb(2, "USB_1", USB, PORT_B, None);
        t.usb(3, "USB2", "1234:5678", "2", None);
        let cards = scan_cards(&t.roots);
        let dev = |card: &str| DeviceInfo {
            id: format!("hw:CARD={card},DEV=0"),
            card: card.into(),
            name: "USB Audio".into(),
            hdmi: false,
            loopback: false,
            active: false,
            channels: Vec::new(),
            suffix: None,
            identity: None,
        };
        let mut devices = vec![dev("PCH"), dev("USB"), dev("USB_1"), dev("USB2")];
        annotate(&mut devices, &cards);
        assert_eq!(devices[0].suffix, None);
        assert_eq!(devices[0].identity, None);
        assert_eq!(devices[1].suffix.as_deref(), Some("USB port 3"));
        assert_eq!(devices[2].suffix.as_deref(), Some("USB port 1.4"));
        assert_eq!(devices[3].suffix, None);
        assert_eq!(devices[3].identity.as_ref().unwrap().usb_id, "1234:5678");
    }

    #[test]
    fn renames_keep_the_device_numbers() {
        assert_eq!(renamed("hw:CARD=USB,DEV=0", "USB_1"), "hw:CARD=USB_1,DEV=0");
        assert_eq!(renamed("hw:USB,1", "X"), "hw:CARD=X,DEV=1");
        assert_eq!(renamed("hw:2,0,1", "X"), "hw:CARD=X,DEV=0,SUBDEV=1");
        assert_eq!(renamed("hw:CARD=A", "B"), "hw:CARD=B,DEV=0");
        assert_eq!(base_id("USB_1"), "USB");
        assert_eq!(base_id("USB"), "USB");
        assert_eq!(base_id("_1"), "_1");
    }
}
