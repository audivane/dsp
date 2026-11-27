//! Which physical USB device an ALSA card is, and the pins that keep the
//! engine from opening a card by name when the name now belongs to another
//! device.
//!
//! Identical DACs without serial numbers get card ids in probe order
//! (`USB`, `USB_1`); a replug or a reboot can swap them. What stays put is
//! the USB serial (when there is one) and the port: `/proc/asound/cardN`
//! gives the USB vendor:product, `/sys/class/sound/cardN/device` leads to
//! the USB interface, whose parent is the device (`5-1.4.3`: bus 5, port
//! chain `1.4.3`) under its controller (`0000:00:14.0`).
//!
//! Nothing is opened: everything here reads `/proc` and `/sys`, cheap
//! enough to poll.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use audivane_protocol::ipc::DeviceIdentity;

/// One sound card as `/proc/asound` and sysfs see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardFacts {
    pub index: u32,
    /// ALSA card id (`USB_1`).
    pub id: String,
    /// USB vendor:product, lowercase hex; `None` for a card that is not USB.
    pub usb_id: Option<String>,
    pub serial: Option<String>,
    /// `pci-0000:00:14.0-usb-0:1.4.3` (see [`DeviceIdentity::port`]).
    pub port: Option<String>,
}

impl CardFacts {
    /// The identity of a USB card (`None` for other cards).
    pub fn identity(&self) -> Option<DeviceIdentity> {
        Some(DeviceIdentity {
            card: self.id.clone(),
            usb_id: self.usb_id.clone()?,
            serial: self.serial.clone(),
            port: self.port.clone(),
        })
    }
}

/// Where the card facts are read from (the real system, or a test tree).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roots {
    /// `/proc/asound`.
    pub asound: PathBuf,
    /// `/sys`.
    pub sys: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Self {
            asound: PathBuf::from("/proc/asound"),
            sys: PathBuf::from("/sys"),
        }
    }
}

/// Every card in `roots`, by index.
pub fn scan_cards(roots: &Roots) -> Vec<CardFacts> {
    let Ok(entries) = std::fs::read_dir(&roots.asound) else {
        return Vec::new();
    };
    let mut cards: Vec<CardFacts> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let index = name.to_str()?.strip_prefix("card")?.parse::<u32>().ok()?;
            card_facts(roots, index)
        })
        .collect();
    cards.sort_by_key(|c| c.index);
    cards
}

/// One card's facts (`None` when the card is not there).
pub fn card_facts(roots: &Roots, index: u32) -> Option<CardFacts> {
    let dir = roots.asound.join(format!("card{index}"));
    let id = std::fs::read_to_string(dir.join("id"))
        .ok()?
        .trim()
        .to_string();
    if id.is_empty() {
        return None;
    }
    let device = std::fs::canonicalize(
        roots
            .sys
            .join("class/sound")
            .join(format!("card{index}"))
            .join("device"),
    )
    .ok();
    let usb = device.as_deref().and_then(usb_device_dir);
    let read = |p: PathBuf| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let usb_id = read(dir.join("usbid"))
        .or_else(|| {
            let usb = usb.as_ref()?;
            Some(format!(
                "{}:{}",
                read(usb.join("idVendor"))?,
                read(usb.join("idProduct"))?
            ))
        })
        .map(|s| s.to_ascii_lowercase());
    let serial = usb.as_ref().and_then(|u| read(u.join("serial")));
    let port = usb.as_deref().and_then(port_path);
    Some(CardFacts {
        index,
        id,
        usb_id,
        serial,
        port,
    })
}

/// The USB device directory on a card's sysfs path: the deepest component
/// named `<bus>-<port chain>` (`5-1.4.3`; the interface `5-1.4.3:1.0` is
/// below it).
fn usb_device_dir(device: &Path) -> Option<PathBuf> {
    let mut found = None;
    let mut at = PathBuf::new();
    for part in device.components() {
        at.push(part);
        if part.as_os_str().to_str().is_some_and(is_usb_device_name) {
            found = Some(at.clone());
        }
    }
    found
}

fn is_usb_device_name(name: &str) -> bool {
    let Some((bus, chain)) = name.split_once('-') else {
        return false;
    };
    !bus.is_empty()
        && bus.bytes().all(|b| b.is_ascii_digit())
        && !chain.is_empty()
        && chain
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `…/0000:00:14.0/usb5/5-1/5-1.4/5-1.4.3` → `pci-0000:00:14.0-usb-0:1.4.3`
/// (udev's `path_id`, without the interface).
fn port_path(usb: &Path) -> Option<String> {
    let name = usb.file_name()?.to_str()?;
    let (_, chain) = name.split_once('-')?;
    let parts: Vec<&str> = usb
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let root_hub = parts.iter().rposition(|p| {
        p.strip_prefix("usb")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    });
    let controller = root_hub
        .and_then(|i| i.checked_sub(1))
        .map(|i| parts[i])
        .map(|c| {
            if is_pci_address(c) {
                format!("pci-{c}")
            } else {
                format!("platform-{c}")
            }
        });
    Some(match controller {
        Some(c) => format!("{c}-usb-0:{chain}"),
        None => format!("usb-0:{chain}"),
    })
}

/// `0000:00:14.0`.
fn is_pci_address(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 12
        && b[4] == b':'
        && b[7] == b':'
        && b[10] == b'.'
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10) || c.is_ascii_hexdigit())
}

/// The card token of an ALSA device name: `hw:CARD=USB,DEV=0` → `usb`,
/// `hw:2,0` → `2` (lowercase; `None` for names that are not `hw:`/`plughw:`).
pub fn card_token(device: &str) -> Option<String> {
    let rest = device
        .trim()
        .strip_prefix("hw:")
        .or_else(|| device.trim().strip_prefix("plughw:"))?;
    let card = rest.split(',').next()?.trim();
    let card = card.strip_prefix("CARD=").unwrap_or(card).trim();
    (!card.is_empty()).then(|| card.to_ascii_lowercase())
}

// ---------------------------------------------------------------- pins

/// What a card named by an output's device must be before the engine
/// counts it as present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pin {
    /// Each field that is `Some` must match the card's.
    Is {
        usb_id: String,
        serial: Option<String>,
        port: Option<String>,
    },
    /// Never present: the daemon cannot tell which card this output is.
    Hold,
}

impl Pin {
    pub fn allows(&self, card: &CardFacts) -> bool {
        match self {
            Pin::Hold => false,
            Pin::Is {
                usb_id,
                serial,
                port,
            } => {
                card.usb_id.as_deref() == Some(usb_id.as_str())
                    && serial
                        .as_ref()
                        .is_none_or(|s| card.serial.as_ref() == Some(s))
                    && port.as_ref().is_none_or(|p| card.port.as_ref() == Some(p))
            }
        }
    }
}

/// Card token ([`card_token`]) → its pin.
static PINS: Mutex<Option<HashMap<String, Pin>>> = Mutex::new(None);

/// Replace every pin (the daemon's view after each look at the cards).
pub fn set_pins(pins: impl IntoIterator<Item = (String, Pin)>) {
    let map: HashMap<String, Pin> = pins.into_iter().collect();
    *PINS.lock().unwrap_or_else(|e| e.into_inner()) = (!map.is_empty()).then_some(map);
}

/// The pin on `device`'s card, if any.
pub fn pin_for(device: &str) -> Option<Pin> {
    let token = card_token(device)?;
    PINS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&token)
        .cloned()
}

/// Whether `device`'s card is the device its pin names (true without a
/// pin). `card_dir` is the card's `/proc/asound/cardN`.
pub(crate) fn pinned_card_allowed(roots: &Roots, device: &str, card_dir: &Path) -> bool {
    let Some(pin) = pin_for(device) else {
        return true;
    };
    let index = card_dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("card"))
        .and_then(|n| n.parse::<u32>().ok());
    match index.and_then(|i| card_facts(roots, i)) {
        Some(card) => pin.allows(&card),
        None => false,
    }
}

/// A fake `/proc/asound` + `/sys` tree for tests (here and in the daemon).
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// A fake `/proc/asound` + `/sys` tree under the temp dir.
    pub struct FakeTree {
        pub roots: Roots,
        base: PathBuf,
    }

    impl FakeTree {
        pub fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("avdsp-identity-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let roots = Roots {
                asound: base.join("proc/asound"),
                sys: base.join("sys"),
            };
            std::fs::create_dir_all(&roots.asound).unwrap();
            std::fs::create_dir_all(roots.sys.join("class/sound")).unwrap();
            Self { roots, base }
        }

        /// A USB card `index` named `id` on `chain` (`1.4.3`) of bus 5.
        pub fn usb(&self, index: u32, id: &str, usb_id: &str, chain: &str, serial: Option<&str>) {
            let card = self.roots.asound.join(format!("card{index}"));
            std::fs::create_dir_all(card.join("pcm0p")).unwrap();
            std::fs::write(card.join("id"), format!("{id}\n")).unwrap();
            std::fs::write(card.join("usbid"), format!("{usb_id}\n")).unwrap();
            std::fs::write(card.join("usbbus"), "005/007\n").unwrap();
            let mut dev = self.roots.sys.join("devices/pci0000:00/0000:00:14.0/usb5");
            let mut acc = String::new();
            for part in chain.split('.') {
                if !acc.is_empty() {
                    acc.push('.');
                }
                acc.push_str(part);
                dev.push(format!("5-{acc}"));
            }
            std::fs::create_dir_all(&dev).unwrap();
            std::fs::write(dev.join("serial"), serial.unwrap_or("")).unwrap();
            let iface = dev.join(format!("5-{chain}:1.0"));
            std::fs::create_dir_all(&iface).unwrap();
            let class = self.roots.sys.join(format!("class/sound/card{index}"));
            std::fs::create_dir_all(&class).unwrap();
            let link = class.join("device");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&iface, &link).unwrap();
        }

        /// A card that is not USB (an onboard codec, the loopback).
        pub fn plain(&self, index: u32, id: &str) {
            let card = self.roots.asound.join(format!("card{index}"));
            std::fs::create_dir_all(card.join("pcm0p")).unwrap();
            std::fs::write(card.join("id"), format!("{id}\n")).unwrap();
        }

        /// Unplug every card (the tree is rebuilt for the next "boot").
        pub fn clear(&self) {
            let _ = std::fs::remove_dir_all(&self.base);
            std::fs::create_dir_all(&self.roots.asound).unwrap();
            std::fs::create_dir_all(self.roots.sys.join("class/sound")).unwrap();
        }
    }

    impl Drop for FakeTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeTree;
    use super::*;

    #[test]
    fn usb_cards_have_their_vid_pid_serial_and_port() {
        let t = FakeTree::new("facts");
        t.plain(0, "PCH");
        t.usb(1, "USB", "2522:0007", "1.4.3", None);
        t.usb(2, "USB_1", "2522:0007", "1.4.1.4", Some("ABC123"));
        let cards = scan_cards(&t.roots);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].usb_id, None);
        assert_eq!(cards[0].identity(), None);
        assert_eq!(cards[1].id, "USB");
        assert_eq!(cards[1].usb_id.as_deref(), Some("2522:0007"));
        assert_eq!(cards[1].serial, None);
        assert_eq!(
            cards[1].port.as_deref(),
            Some("pci-0000:00:14.0-usb-0:1.4.3")
        );
        assert_eq!(cards[2].serial.as_deref(), Some("ABC123"));
        assert_eq!(
            cards[2].port.as_deref(),
            Some("pci-0000:00:14.0-usb-0:1.4.1.4")
        );
        let id = cards[2].identity().unwrap();
        assert_eq!(id.port_chain(), Some("1.4.1.4"));
    }

    #[test]
    fn names_and_tokens() {
        assert!(is_usb_device_name("5-1.4.3"));
        assert!(is_usb_device_name("1-2"));
        assert!(!is_usb_device_name("5-1.4.3:1.0"));
        assert!(!is_usb_device_name("usb5"));
        assert!(!is_usb_device_name("avdsp-identity-x-12"));
        assert!(is_pci_address("0000:67:00.3"));
        assert!(!is_pci_address("pci0000:00"));
        assert_eq!(card_token("hw:CARD=USB,DEV=0").as_deref(), Some("usb"));
        assert_eq!(card_token("hw:USB_1").as_deref(), Some("usb_1"));
        assert_eq!(card_token("hw:2,0").as_deref(), Some("2"));
        assert_eq!(card_token("default"), None);
    }

    #[test]
    fn a_pin_keeps_a_card_name_from_counting_for_another_device() {
        let t = FakeTree::new("pins");
        t.usb(1, "USB", "2522:0007", "1.4.3", None);
        let present = |d: &str| crate::reserve::device_present_gated_in(&t.roots, d);
        assert!(present("hw:CARD=USB,DEV=0"));
        set_pins([(
            "usb".to_string(),
            Pin::Is {
                usb_id: "2522:0007".into(),
                serial: None,
                port: Some("pci-0000:00:14.0-usb-0:1.4.1.4".into()),
            },
        )]);
        assert!(!present("hw:CARD=USB,DEV=0"));
        set_pins([(
            "usb".to_string(),
            Pin::Is {
                usb_id: "2522:0007".into(),
                serial: None,
                port: Some("pci-0000:00:14.0-usb-0:1.4.3".into()),
            },
        )]);
        assert!(present("hw:CARD=USB,DEV=0"));
        set_pins([("usb".to_string(), Pin::Hold)]);
        assert!(!present("hw:CARD=USB,DEV=0"));
        // By index too: the pin is on the name the output uses.
        assert!(present("hw:1,0"));
        set_pins([]);
        assert!(present("hw:CARD=USB,DEV=0"));
    }
}
