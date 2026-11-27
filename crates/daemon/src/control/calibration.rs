//! Room-calibration bundles (`bundle_load`, `bundle_unload`, `bundle_show`,
//! `bundle_ab`): one manifest that installs a room calibration across the
//! devices, loaded as one config edit.
//!
//! A bundle maps onto what exists:
//! - the master's part is one convolution matrix item in chain A, tagged
//!   `calibration`, so loading or unloading replaces only that item;
//! - a follower's part is that follower's whole `chain_patch.convolution`
//!   (one tagged item) and its `trim_ms` (a follower the config lacks is
//!   added);
//! - each part's `gain_db` is its item's gain, so the boost cap counts it.
//!
//! The installed state lives in `EngineConfig.calibration`, with what the
//! bundle replaced on each follower. [`normalize`] keeps the tagged items in
//! place on every config edit (a preset load, `set_convolution`, A/B side B),
//! so the calibration stays with the room whatever else changes; only
//! [`uninstall`] removes it.

use audivane_protocol::ipc::{
    AbCalibration, CalibrationConfig, CalibrationDeviceStatus, CalibrationFollower,
    CalibrationRestore, CalibrationStatus, EngineConfig, FollowerConfig, FollowerState,
    OutputsStatus,
};
use audivane_protocol::{ConvolutionConfig, MatrixEntry};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The manifest versions this daemon reads.
pub const BUNDLE_VERSION: u32 = 1;
/// The largest manifest read, bytes.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
/// The longest label, characters.
const MAX_LABEL: usize = 128;

/// A bundle manifest as the room calibration tool writes it.
#[derive(Deserialize, Debug)]
struct Manifest {
    bundle_version: u32,
    label: String,
    design_rate_hz: u32,
    #[serde(default)]
    notes: Option<String>,
    devices: Vec<ManifestDevice>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Role {
    Master,
    Follower,
}

#[derive(Deserialize, Debug)]
struct ManifestDevice {
    role: Role,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    gain_db: f64,
    #[serde(default)]
    trim_ms: f64,
    matrix: Vec<ManifestEntry>,
}

#[derive(Deserialize, Debug)]
struct ManifestEntry {
    #[serde(rename = "in")]
    input: usize,
    #[serde(rename = "out")]
    output: usize,
    /// The IR file, relative to the manifest (or absolute).
    ir: PathBuf,
    /// Which channel of the WAV (0-based); left out, the file must be mono.
    #[serde(default)]
    channel: Option<usize>,
    #[serde(default)]
    gain_db: f64,
}

/// A bundle read and checked: the calibration to install (restore fields
/// still empty) and notes for the reply.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub calibration: CalibrationConfig,
    pub notes: Vec<String>,
}

/// Read and check the manifest at `path` against `cfg` (the main device,
/// the channels): its shape, every IR file (present, a WAV, the channel it
/// names), the trims. The engine checks the rest when the edit applies (the
/// filters, the boost cap and the CPU admission per device).
pub fn read(path: &str, cfg: &EngineConfig, channels: usize) -> Result<Loaded, String> {
    let path = Path::new(path.trim());
    if path.as_os_str().is_empty() {
        return Err("path is empty".into());
    }
    let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "{}: larger than {} KiB; not a bundle manifest",
            path.display(),
            MAX_MANIFEST_BYTES >> 10
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let m: Manifest =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    check(m, path, &dir, cfg, channels)
}

fn check(
    m: Manifest,
    path: PathBuf,
    dir: &Path,
    cfg: &EngineConfig,
    channels: usize,
) -> Result<Loaded, String> {
    if m.bundle_version != BUNDLE_VERSION {
        return Err(format!(
            "bundle_version {} is not supported (this daemon reads {BUNDLE_VERSION})",
            m.bundle_version
        ));
    }
    let label = m.label.trim().to_string();
    if label.is_empty() || label.chars().count() > MAX_LABEL || label.chars().any(char::is_control)
    {
        return Err(format!(
            "label must be 1 to {MAX_LABEL} characters, without control characters"
        ));
    }
    if !(8_000..=audivane_protocol::MAX_OUTPUT_RATE).contains(&m.design_rate_hz) {
        return Err(format!(
            "design_rate_hz {} is out of range",
            m.design_rate_hz
        ));
    }
    if m.devices.is_empty() {
        return Err("devices is empty".into());
    }
    let mut notes = Vec::new();
    let mut master = None;
    let mut followers: Vec<CalibrationFollower> = Vec::new();
    for (i, d) in m.devices.iter().enumerate() {
        let at = format!("devices[{i}]");
        if !d.gain_db.is_finite() || !(-60.0..=24.0).contains(&d.gain_db) {
            return Err(format!(
                "{at}.gain_db must be between -60 and 24 dB (got {})",
                d.gain_db
            ));
        }
        if d.matrix.is_empty() {
            return Err(format!("{at}.matrix is empty"));
        }
        let mut entries = Vec::with_capacity(d.matrix.len());
        for (j, e) in d.matrix.iter().enumerate() {
            let at = format!("{at}.matrix[{j}]");
            entries.push(entry(e, dir, channels, &at)?);
        }
        for (j, e) in entries.iter().enumerate() {
            if entries[..j]
                .iter()
                .any(|f| (f.input, f.output) == (e.input, e.output))
            {
                return Err(format!(
                    "{at}.matrix[{j}]: in {} out {} is listed twice",
                    e.input, e.output
                ));
            }
        }
        let item = ConvolutionConfig {
            name: Some(format!("Room calibration: {label}")),
            enabled: true,
            ir_path: PathBuf::new(),
            gain_db: d.gain_db,
            matrix: Some(entries),
            calibration: true,
        };
        match d.role {
            Role::Master => {
                if master.is_some() {
                    return Err(format!("{at}: a second master part"));
                }
                if d.trim_ms != 0.0 {
                    return Err(format!(
                        "{at}.trim_ms must be 0 on the master (it is the reference: put the \
                         relative delays in the followers' trim_ms or in the IRs)"
                    ));
                }
                if let Some(dev) = d.device.as_deref().map(str::trim)
                    && !dev.is_empty()
                    && dev != "master"
                    && dev != cfg.device
                {
                    notes.push(format!(
                        "the bundle's master is {dev}; it plays on the main device, {}",
                        cfg.device
                    ));
                }
                master = Some(item);
            }
            Role::Follower => {
                let Some(dev) = d.device.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
                    return Err(format!(
                        "{at}.device is missing (a follower names its device)"
                    ));
                };
                if dev == cfg.device {
                    return Err(format!(
                        "{at}.device {dev} is the main device; a follower part names a follower"
                    ));
                }
                if followers.iter().any(|f| f.device == dev) {
                    return Err(format!("{at}.device {dev} is listed twice"));
                }
                let max = crate::engine::follower::MAX_TRIM_MS;
                if !d.trim_ms.is_finite() || d.trim_ms.abs() > max {
                    return Err(format!(
                        "{at}.trim_ms must be between -{max} and {max} ms (got {})",
                        d.trim_ms
                    ));
                }
                followers.push(CalibrationFollower {
                    device: dev.to_string(),
                    item,
                    trim_ms: d.trim_ms,
                    manifest_gain_db: None,
                    restore: CalibrationRestore::default(),
                });
            }
        }
    }
    Ok(Loaded {
        calibration: CalibrationConfig {
            path,
            label,
            design_rate_hz: m.design_rate_hz,
            enabled: true,
            notes: m.notes.filter(|n| !n.trim().is_empty()),
            master,
            master_manifest_gain_db: None,
            followers,
        },
        notes,
    })
}

/// One matrix entry: its IR resolved against the manifest's folder and
/// checked (a WAV with the channel it names).
fn entry(e: &ManifestEntry, dir: &Path, channels: usize, at: &str) -> Result<MatrixEntry, String> {
    if e.input >= channels || e.output >= channels {
        return Err(format!(
            "{at}: in {} out {}: the engine plays {channels} channels (0 to {})",
            e.input,
            e.output,
            channels - 1
        ));
    }
    if !e.gain_db.is_finite() || !(-60.0..=24.0).contains(&e.gain_db) {
        return Err(format!(
            "{at}.gain_db must be between -60 and 24 dB (got {})",
            e.gain_db
        ));
    }
    if e.ir.as_os_str().is_empty() {
        return Err(format!("{at}.ir is empty"));
    }
    let ir = if e.ir.is_absolute() {
        e.ir.clone()
    } else {
        dir.join(&e.ir)
    };
    let wav =
        hound::WavReader::open(&ir).map_err(|err| format!("{at}.ir {}: {err}", ir.display()))?;
    let ch = usize::from(wav.spec().channels);
    match e.channel {
        Some(c) if c >= ch => {
            return Err(format!(
                "{at}.channel {c}: {} has {ch} channel{}",
                ir.display(),
                if ch == 1 { "" } else { "s" }
            ));
        }
        None if ch != 1 => {
            return Err(format!(
                "{at}: {} has {ch} channels: name one with `channel`",
                ir.display()
            ));
        }
        _ => {}
    }
    Ok(MatrixEntry {
        input: e.input,
        output: e.output,
        ir_path: ir,
        channel: e.channel,
        gain_db: e.gain_db,
    })
}

// ---------------------------------------------------------------------------
// Installing and removing.

/// Install `cal` on `cfg`, replacing the loaded calibration (which is
/// removed first, as [`uninstall`] does): each follower part records what
/// it replaces (its trim, its patch's `convolution`, whether the config had
/// it) and sets its trim; [`normalize`] then puts the items in place.
/// A follower the replaced calibration added and this one calibrates too
/// stays as it is (its entry, level and following; only its convolution
/// and trim change), still the calibration's to remove. A calibrated
/// follower follows the main volume: one that does not yet starts from the
/// level it plays at, against `master_db` (the master's played level).
pub fn install(cfg: &mut EngineConfig, mut cal: CalibrationConfig, master_db: f64) {
    let carried: Vec<String> = cfg
        .calibration
        .as_ref()
        .map(|old| {
            old.followers
                .iter()
                .filter(|p| p.restore.added && cal.followers.iter().any(|n| n.device == p.device))
                .map(|p| p.device.clone())
                .collect()
        })
        .unwrap_or_default();
    let _ = uninstall_keeping(cfg, &carried);
    for part in &mut cal.followers {
        let f = match cfg
            .outputs
            .followers
            .iter_mut()
            .position(|f| f.device == part.device)
        {
            Some(i) => &mut cfg.outputs.followers[i],
            None => {
                cfg.outputs
                    .followers
                    .push(FollowerConfig::new(part.device.clone()));
                part.restore.added = true;
                cfg.outputs.followers.last_mut().expect("just pushed")
            }
        };
        part.restore.trim_ms = f.trim_ms;
        part.restore.convolution = f
            .chain_patch
            .as_ref()
            .and_then(|p| p.get("convolution"))
            .cloned();
        f.trim_ms = part.trim_ms;
        if carried.contains(&part.device) {
            part.restore.added = true;
        }
        // A calibrated follower keeps its level to the master's: the main
        // volume moves it too (from the level it has now).
        if !f.follow_volume {
            f.follow_volume = true;
            f.follow_ref_db = Some(master_db);
        }
    }
    cfg.calibration = Some(cal);
    normalize(cfg);
}

/// The level of one part of the loaded calibration (`bundle_gain`): its
/// item's gain, `gain_db` (`None`: the manifest's). `device` names a
/// follower part; `None` or `"master"` the master's. The manifest's gain
/// is kept beside it while they differ, so a reset finds it; a new load
/// starts from the manifest again. [`normalize`] then puts the item in
/// place (chain A, or that follower's patch only).
pub fn set_gain(
    cfg: &mut EngineConfig,
    device: Option<&str>,
    gain_db: Option<f64>,
) -> Result<(), String> {
    if let Some(g) = gain_db
        && (!g.is_finite() || !(-60.0..=24.0).contains(&g))
    {
        return Err(format!("gain_db must be between -60 and 24 dB (got {g})"));
    }
    let Some(cal) = cfg.calibration.as_mut() else {
        return Err("no room calibration is loaded".into());
    };
    let device = device.map(str::trim).filter(|d| !d.is_empty());
    let main = device.is_none_or(|d| d == "master" || d == cfg.device);
    let (item, manifest) = if main {
        let Some(item) = cal.master.as_mut() else {
            return Err("the loaded calibration has no master part".into());
        };
        (item, &mut cal.master_manifest_gain_db)
    } else {
        let d = device.unwrap_or_default();
        let Some(p) = cal.followers.iter_mut().find(|p| p.device == d) else {
            return Err(format!("{d} is not a part of the loaded calibration"));
        };
        (&mut p.item, &mut p.manifest_gain_db)
    };
    let base = manifest.unwrap_or(item.gain_db);
    item.gain_db = gain_db.unwrap_or(base);
    *manifest = (item.gain_db != base).then_some(base);
    normalize(cfg);
    Ok(())
}

/// Switch the loaded calibration on or off (`bundle_enable`): the placed
/// items follow on [`normalize`], the stored parts as they were. Off, each
/// calibrated follower is also muted at runtime
/// ([`crate::engine::follower::muted_by_calibration`]). `None` loaded: an
/// error.
pub fn set_enabled(cfg: &mut EngineConfig, on: bool) -> Result<(), String> {
    let Some(cal) = cfg.calibration.as_mut() else {
        return Err("no room calibration is loaded".into());
    };
    cal.enabled = on;
    normalize(cfg);
    Ok(())
}

/// Remove the loaded calibration from `cfg`: exactly what it installed
/// (chain A's tagged item, each follower's patch `convolution` and trim
/// put back, the followers it added removed). What it would put back but
/// is no longer valid is skipped, with a warning each: a convolution item
/// whose IR is missing or unreadable, a trim out of range. A follower's
/// level and following are left as they are (a follower that follows keeps
/// following, the master's mute and balance with it): unloading never
/// moves or unmutes it. `None`: none was loaded.
pub fn uninstall(cfg: &mut EngineConfig) -> Option<Vec<String>> {
    uninstall_keeping(cfg, &[])
}

/// [`uninstall`], leaving the followers in `keep` that it added exactly as
/// they are (a replacing calibration takes them over).
fn uninstall_keeping(cfg: &mut EngineConfig, keep: &[String]) -> Option<Vec<String>> {
    let cal = cfg.calibration.take()?;
    let mut warnings = Vec::new();
    for part in &cal.followers {
        let Some(i) = cfg
            .outputs
            .followers
            .iter()
            .position(|f| f.device == part.device)
        else {
            continue;
        };
        if part.restore.added {
            if !keep.contains(&part.device) {
                cfg.outputs.followers.remove(i);
            }
            continue;
        }
        let f = &mut cfg.outputs.followers[i];
        let max = crate::engine::follower::MAX_TRIM_MS;
        if part.restore.trim_ms.is_finite() && part.restore.trim_ms.abs() <= max {
            f.trim_ms = part.restore.trim_ms;
        } else {
            warnings.push(format!(
                "{}: its trim before the calibration ({} ms) is out of range; kept {} ms",
                part.device, part.restore.trim_ms, f.trim_ms
            ));
        }
        let mut patch = match f.chain_patch.take() {
            Some(Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        match &part.restore.convolution {
            Some(v) => {
                let v = valid_items(v, &part.device, &mut warnings);
                match v {
                    Some(v) => patch.insert("convolution".into(), v),
                    None => patch.remove("convolution"),
                };
            }
            None => {
                patch.remove("convolution");
            }
        }
        f.chain_patch = super::follower_chain::normalise(Some(Value::Object(patch)));
    }
    normalize(cfg);
    Some(warnings)
}

/// A saved patch `convolution` with the items whose IR files are gone or
/// unreadable left out (warned). `None`: nothing valid is left (the key
/// goes: the follower follows chain A's convolution).
fn valid_items(v: &Value, device: &str, warnings: &mut Vec<String>) -> Option<Value> {
    let Value::Array(items) = v else {
        return Some(v.clone());
    };
    if items.is_empty() {
        return Some(v.clone());
    }
    let mut kept = Vec::new();
    for item in items {
        match serde_json::from_value::<ConvolutionConfig>(item.clone()) {
            Ok(c) => match ir_files_readable(&c) {
                Ok(()) => kept.push(item.clone()),
                Err(e) => warnings.push(format!(
                    "{device}: its convolution before the calibration is not put back: {e}"
                )),
            },
            Err(e) => warnings.push(format!(
                "{device}: its convolution before the calibration is not put back: {e}"
            )),
        }
    }
    (!kept.is_empty()).then_some(Value::Array(kept))
}

/// Every IR file `c` names opens as a WAV.
pub fn ir_files_readable(c: &ConvolutionConfig) -> Result<(), String> {
    let mut paths: Vec<&Path> = Vec::new();
    if !c.ir_path.as_os_str().is_empty() {
        paths.push(&c.ir_path);
    }
    for e in c.matrix.iter().flatten() {
        paths.push(&e.ir_path);
    }
    for p in paths {
        hound::WavReader::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

/// The loaded calibration's IR files are all readable (`Err`: the first
/// that is not).
pub fn check_files(cal: &CalibrationConfig) -> Result<(), String> {
    for c in cal
        .master
        .iter()
        .chain(cal.followers.iter().map(|p| &p.item))
    {
        ir_files_readable(c)?;
    }
    Ok(())
}

/// At start-up: a calibration whose IR files are not all readable is
/// dropped (unloaded, the followers' settings from before it put back
/// where valid), the rest of the config kept. The warnings say why.
pub fn drop_if_unreadable(cfg: &mut EngineConfig) -> Vec<String> {
    let Some(cal) = &cfg.calibration else {
        return Vec::new();
    };
    let Err(e) = check_files(cal) else {
        return Vec::new();
    };
    let label = cal.label.clone();
    let mut warnings = vec![format!(
        "the room calibration {label:?} is not loaded: {e}; load it again once its files are back"
    )];
    warnings.extend(uninstall(cfg).unwrap_or_default());
    warnings
}

/// Keep the calibration's record in step with an edit of the followers
/// (`cfg`, before [`normalize`]): a calibrated follower that is gone drops
/// out of the calibration (its record is stale: a follower added again by
/// hand is the user's); one whose patch `convolution` the edit set to
/// something other than the calibration's item keeps playing the item, and
/// unloading puts back what the edit set instead (`None`: no key).
pub fn follow_edits(cfg: &mut EngineConfig) {
    let Some(cal) = cfg.calibration.as_mut() else {
        return;
    };
    let followers = &cfg.outputs.followers;
    cal.followers
        .retain(|p| followers.iter().any(|f| f.device == p.device));
    for part in &mut cal.followers {
        let Some(f) = followers.iter().find(|f| f.device == part.device) else {
            continue;
        };
        let conv = f.chain_patch.as_ref().and_then(|p| p.get("convolution"));
        let mut item = part.item.clone();
        item.calibration = true;
        let ours = serde_json::to_value(vec![item]).ok();
        if conv != ours.as_ref() {
            part.restore.convolution = conv.cloned();
        }
    }
}

/// Put `item` in `list` where the calibration's item is (the first tagged
/// one), or at the end; every other tagged item goes. `None` removes them.
/// The copy placed is tagged, and plays only while the calibration is on
/// (`enabled`).
pub fn place(list: &mut Vec<ConvolutionConfig>, item: Option<&ConvolutionConfig>, enabled: bool) {
    let at = list.iter().position(|c| c.calibration);
    list.retain(|c| !c.calibration);
    if let Some(item) = item {
        let mut item = item.clone();
        item.calibration = true;
        item.enabled = enabled;
        let at = at.unwrap_or(list.len()).min(list.len());
        list.insert(at, item);
    }
}

/// Keep the calibration's items where they belong, after any edit: chain
/// A's tagged item is the master part (none without one); each bundled
/// follower's patch `convolution` is its part; a tagged item anywhere else
/// goes. Chain A's copy (and B's, [`place_b`]) plays only while the
/// calibration is on; a follower's part always plays (off mutes the
/// follower). Side B is
/// placed by [`place_b`] once materialised.
pub fn normalize(cfg: &mut EngineConfig) {
    let cal = cfg.calibration.clone();
    let on = cal.as_ref().is_none_or(|c| c.enabled);
    place(
        &mut cfg.chain.convolution,
        cal.as_ref().and_then(|c| c.master.as_ref()),
        on,
    );
    for f in &mut cfg.outputs.followers {
        let part = cal
            .as_ref()
            .and_then(|c| c.followers.iter().find(|p| p.device == f.device));
        match part {
            Some(p) => {
                let mut item = p.item.clone();
                item.calibration = true;
                // A follower's part keeps playing while the calibration is
                // off: the follower is muted instead
                // (`muted_by_calibration`), so its chain never changes and
                // a sub never plays without its low-pass, not even while a
                // chain is rebuilt.
                let mut patch = match f.chain_patch.take() {
                    Some(Value::Object(m)) => m,
                    _ => serde_json::Map::new(),
                };
                patch.insert(
                    "convolution".into(),
                    serde_json::to_value(vec![item]).unwrap_or(Value::Null),
                );
                f.chain_patch = Some(Value::Object(patch));
            }
            None => strip_patch(f),
        }
    }
}

/// A follower outside the calibration keeps no tagged item in its patch (a
/// hand-edited config).
fn strip_patch(f: &mut FollowerConfig) {
    let Some(Value::Object(patch)) = f.chain_patch.as_mut() else {
        return;
    };
    let Some(Value::Array(list)) = patch.get_mut("convolution") else {
        return;
    };
    let before = list.len();
    list.retain(|v| v.get("calibration").and_then(Value::as_bool) != Some(true));
    if list.len() != before && list.is_empty() {
        patch.remove("convolution");
        let p = f.chain_patch.take();
        f.chain_patch = super::follower_chain::normalise(p);
    }
}

/// Side B's convolution once materialised: B's own calibration
/// (`calibration_b`: another bundle's master part, or none), else chain
/// A's, in the place of the tagged item. B with its own bundle keeps that
/// item's state; B with A's calibration follows A's switch.
pub fn place_b(
    chain_b: &mut audivane_protocol::ChainConfig,
    cal: Option<&CalibrationConfig>,
    b: Option<&AbCalibration>,
) {
    let (item, on) = match b {
        Some(b) => (
            b.master.as_ref(),
            b.master.as_ref().is_none_or(|m| m.enabled),
        ),
        None => (
            cal.and_then(|c| c.master.as_ref()),
            cal.is_none_or(|c| c.enabled),
        ),
    };
    place(&mut chain_b.convolution, item, on);
}

/// Side B for `bundle_ab`: the bundle's master part (`None`: without a
/// calibration).
pub fn ab_side(loaded: Option<&Loaded>) -> AbCalibration {
    match loaded {
        Some(l) => AbCalibration {
            path: Some(l.calibration.path.clone()),
            label: Some(l.calibration.label.clone()),
            master: l.calibration.master.clone(),
        },
        None => AbCalibration::default(),
    }
}

// ---------------------------------------------------------------------------
// Status.

/// The loaded calibration as it plays now, with warnings about the rates:
/// the graph rate against the design rate (the IRs are resampled), and
/// devices whose DACs run at different rates (the DACs' own delay depends
/// on their rate; the latency compensation does not cover it).
pub fn status(
    cfg: &EngineConfig,
    graph_rate: u32,
    master_rate: u32,
    outputs: &OutputsStatus,
) -> Option<CalibrationStatus> {
    let cal = cfg.calibration.as_ref()?;
    let mut devices = Vec::new();
    let count = |c: &ConvolutionConfig| c.matrix.as_ref().map_or(0, Vec::len) as u32;
    if let Some(m) = &cal.master {
        devices.push(CalibrationDeviceStatus {
            role: "master".into(),
            device: cfg.device.clone(),
            entries: count(m),
            gain_db: m.gain_db,
            manifest_gain_db: cal.master_manifest_gain_db.unwrap_or(m.gain_db),
            trim_ms: 0.0,
            installed: cfg.chain.convolution.iter().any(|c| c.calibration),
        });
    }
    for p in &cal.followers {
        let f = cfg.outputs.followers.iter().find(|f| f.device == p.device);
        devices.push(CalibrationDeviceStatus {
            role: "follower".into(),
            device: p.device.clone(),
            entries: count(&p.item),
            gain_db: p.item.gain_db,
            manifest_gain_db: p.manifest_gain_db.unwrap_or(p.item.gain_db),
            trim_ms: f.map_or(p.trim_ms, |f| f.trim_ms),
            installed: f.is_some(),
        });
    }
    let mut warnings = Vec::new();
    if graph_rate != 0 && graph_rate != cal.design_rate_hz {
        warnings.push(format!(
            "the graph runs at {graph_rate} Hz; the calibration was designed at {} Hz: its \
             filters are resampled",
            cal.design_rate_hz
        ));
    }
    for p in &cal.followers {
        let live = outputs.followers.iter().find(|s| {
            s.device == p.device
                && s.device_rate != 0
                && !matches!(s.state, FollowerState::Off | FollowerState::Lost)
        });
        match live {
            Some(s) if master_rate != 0 => {
                if s.device_rate != master_rate {
                    warnings.push(format!(
                        "{} runs at {} Hz, the main device at {master_rate} Hz: the DACs' own \
                         delays differ with the rate (not compensated); keep them at one rate \
                         or measure the offset",
                        p.device, s.device_rate
                    ));
                }
            }
            _ => {
                // Not playing: what the configs ask for.
                let Some((i, f)) = cfg
                    .outputs
                    .followers
                    .iter()
                    .enumerate()
                    .find(|(_, f)| f.device == p.device)
                else {
                    continue;
                };
                let own = super::follower_chain::chain(cfg, f, i);
                if own.is_ok_and(|c| c.upsampler.target != cfg.chain.upsampler.target) {
                    warnings.push(format!(
                        "{}'s own chain asks for another upsampling target than the main \
                         device: the DACs would run at different rates, and their own delays \
                         differ with the rate (not compensated)",
                        p.device
                    ));
                }
            }
        }
    }
    Some(CalibrationStatus {
        label: cal.label.clone(),
        path: cal.path.display().to_string(),
        design_rate_hz: cal.design_rate_hz,
        enabled: cal.enabled,
        notes: cal.notes.clone(),
        devices,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The master's level as the tests' engine plays it: as configured.
    fn master(c: &EngineConfig) -> f64 {
        crate::engine::volume::target_db(&c.volume)
    }

    /// [`install`] and [`uninstall`] with the master at its configured level.
    fn put(c: &mut EngineConfig, cal: CalibrationConfig) {
        let m = master(c);
        install(c, cal, m);
    }

    fn take(c: &mut EngineConfig) -> Option<Vec<String>> {
        uninstall(c)
    }
    use serde_json::json;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("audivane-cal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(crate) fn write_ir(path: &Path, channels: u16, taps: &[f32]) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for &t in taps {
            for _ in 0..channels {
                w.write_sample(t).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn cfg() -> EngineConfig {
        serde_json::from_value(json!({"device": "hw:CARD=USB,DEV=0"})).unwrap()
    }

    fn manifest(d: &Path, body: Value) -> String {
        let p = d.join("bundle.json");
        std::fs::write(&p, serde_json::to_string(&body).unwrap()).unwrap();
        p.display().to_string()
    }

    fn bundle() -> Value {
        json!({
            "bundle_version": 1, "label": "desk", "design_rate_hz": 48000,
            "devices": [
                { "role": "master", "gain_db": -1.0,
                  "matrix": [ { "in": 0, "out": 0, "ir": "ll.wav" }, { "in": 1, "out": 1, "ir": "rr.wav" } ] },
                { "role": "follower", "device": "hw:CARD=USB_1,DEV=0", "trim_ms": 1.5,
                  "matrix": [ { "in": 0, "out": 0, "ir": "sub.wav", "channel": 0 },
                              { "in": 1, "out": 0, "ir": "sub.wav", "channel": 1 } ] } ],
            "notes": "test"
        })
    }

    fn irs(d: &Path) {
        write_ir(&d.join("ll.wav"), 1, &[0.5]);
        write_ir(&d.join("rr.wav"), 1, &[0.5]);
        write_ir(&d.join("sub.wav"), 2, &[0.25]);
    }

    #[test]
    fn bad_manifests_say_where() {
        let d = dir("bad");
        irs(&d);
        let err = |f: &dyn Fn(&mut Value)| {
            let mut b = bundle();
            f(&mut b);
            read(&manifest(&d, b), &cfg(), 2).unwrap_err()
        };
        assert!(err(&|b| b["bundle_version"] = json!(2)).contains("bundle_version 2"));
        assert!(err(&|b| b["label"] = json!(" ")).contains("label"));
        let e = err(&|b| b["devices"][1]["matrix"][0]["ir"] = json!("nope.wav"));
        assert!(
            e.starts_with("devices[1].matrix[0].ir") && e.contains("nope.wav"),
            "{e}"
        );
        let e = err(&|b| b["devices"][1]["matrix"][0]["channel"] = json!(2));
        assert!(e.contains("devices[1].matrix[0].channel 2"), "{e}");
        let e = err(&|b| b["devices"][0]["matrix"][0]["out"] = json!(2));
        assert!(e.contains("2 channels"), "{e}");
        let e = err(&|b| b["devices"][0]["trim_ms"] = json!(1.0));
        assert!(e.contains("devices[0].trim_ms must be 0"), "{e}");
        let e = err(&|b| b["devices"][1]["trim_ms"] = json!(80.0));
        assert!(e.contains("devices[1].trim_ms"), "{e}");
        let e = err(&|b| b["devices"][1]["device"] = json!("hw:CARD=USB,DEV=0"));
        assert!(e.contains("main device"), "{e}");
        let e = err(&|b| {
            b["devices"][1]["matrix"][1]["in"] = json!(0);
        });
        assert!(e.contains("listed twice"), "{e}");
        let e = err(&|b| b["devices"][0]["matrix"][0]["ir"] = json!("sub.wav"));
        assert!(e.contains("name one with `channel`"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Install records what it replaced; uninstall puts exactly that back.
    #[test]
    fn uninstall_restores_what_install_replaced() {
        let d = dir("install");
        irs(&d);
        let l = read(&manifest(&d, bundle()), &cfg(), 2).unwrap();
        let mut c = cfg();
        c.chain.convolution = vec![ConvolutionConfig {
            name: Some("mine".into()),
            enabled: true,
            ir_path: "/x.wav".into(),
            gain_db: 0.0,
            matrix: None,
            calibration: false,
        }];
        let mut other = FollowerConfig::new("hw:CARD=X,DEV=0");
        other.trim_ms = 2.0;
        c.outputs.followers.push(other);
        let before = c.clone();
        put(&mut c, l.calibration.clone());
        assert_eq!(c.chain.convolution.len(), 2);
        assert_eq!(c.chain.convolution[0], before.chain.convolution[0]);
        assert!(c.chain.convolution[1].calibration);
        let f = &c.outputs.followers[1];
        assert_eq!((f.device.as_str(), f.trim_ms), ("hw:CARD=USB_1,DEV=0", 1.5));
        let conv = &f.chain_patch.as_ref().unwrap()["convolution"];
        assert_eq!(conv.as_array().unwrap().len(), 1);
        assert_eq!(conv[0]["calibration"], json!(true));
        assert!(c.calibration.as_ref().unwrap().followers[0].restore.added);
        // A calibrated follower follows the main volume from its level now;
        // the other one keeps its own.
        let m = master(&c);
        assert!(f.follow_volume && f.follow_ref_db == Some(m), "{f:?}");
        assert!(!c.outputs.followers[0].follow_volume);
        assert!(take(&mut c).unwrap().is_empty());
        assert_eq!(c, before);
        // An existing follower with its own patch gets it back, and its trim.
        let mut c = cfg();
        let mut f = FollowerConfig::new("hw:CARD=USB_1,DEV=0");
        f.trim_ms = -3.0;
        let own = d.join("own.wav");
        write_ir(&own, 1, &[0.5]);
        f.chain_patch =
            Some(json!({"convolution": [{"ir_path": own}], "limiter": {"mode": "limit"}}));
        c.outputs.followers.push(f);
        let before = c.clone();
        put(&mut c, l.calibration.clone());
        let patch = c.outputs.followers[0].chain_patch.as_ref().unwrap();
        assert_eq!(patch["limiter"], json!({"mode": "limit"}));
        assert_eq!(patch["convolution"][0]["calibration"], json!(true));
        assert!(c.outputs.followers[0].follow_volume);
        // Loading again replaces it, and still restores the first state
        // (but the following it turned on: a follower keeps its level).
        put(&mut c, l.calibration.clone());
        assert!(take(&mut c).unwrap().is_empty());
        let mut want = before;
        want.outputs.followers[0].follow_volume = true;
        want.outputs.followers[0].follow_ref_db = Some(master(&want));
        assert_eq!(c, want);
        assert!(take(&mut c).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }
}
