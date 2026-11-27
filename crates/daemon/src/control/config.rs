//! Persistent daemon configuration.
//!
//! The file lives at `$XDG_CONFIG_HOME/audivane-dsp/config.json`
//! (default `~/.config/audivane-dsp/config.json`; override with
//! `audivane-dsp --config PATH`).
//!
//! Format: a small versioned envelope around [`EngineConfig`]:
//!
//! ```json
//! { "version": 1, "config": { "device": "hw:0,0", ... } }
//! ```
//!
//! A bare [`EngineConfig`] object (no envelope) is also accepted, so a hand
//! written config keeps working. Unknown fields are ignored at both levels.
//! Writes are atomic: a temporary file in the same directory is written,
//! fsynced and renamed over the old one, so a crash never leaves a truncated
//! config behind.
//!
//! A file that cannot be parsed or validated is not silently discarded: it is
//! copied to `config.json.corrupt-<unix-seconds>` and the built-in defaults are
//! used (the daemon logs both facts).

use audivane_dsp_core::design::{
    self, BuildContext, FilterDesign, Length, Shape, StoredDesign, catalogue,
    upsampler::preset_error,
};
use audivane_dsp_core::minphase::INTERMEDIATE_MAX_GROWTH;
use audivane_dsp_core::resample::{ManualConfig, band_design_for};
use audivane_protocol::ipc::{
    AB_HEADROOM_MAX_DB, AbB, AbConfig, AbHeadroom, BALANCE_MAX_DB, DeviceInfo, EngineConfig,
    FOLLOWED_MAX_SPEED, FOLLOWER_MAX_TRIM_MS, FOLLOWER_PERIOD_MS_RANGE, LevelMatch, MAX_FOLLOWERS,
    MAX_QUANTUM, MIN_QUANTUM, OutputChannels, RatePolicy, RoutingMode, VOLUME_MAX_DB,
    VOLUME_MIN_DB,
};
use audivane_protocol::{
    CROSSFEED_CUTOFF_MAX_HZ, CROSSFEED_CUTOFF_MIN_HZ, CROSSFEED_FEED_MAX_DB, CROSSFEED_FEED_MIN_DB,
    ChainConfig, CrossfeedConfig, DC_BLOCK_CUTOFF_MAX_HZ, DC_BLOCK_CUTOFF_MIN_HZ, DcBlockConfig,
    DitherKind, FilterPhase, HARMONICS_MAX_LAYERS, HarmonicsConfig, OutputConfig, PitchConfig,
    RateRatio, RateTarget, STEREO_WIDTH_CORNER_MAX_HZ, STEREO_WIDTH_CORNER_MIN_HZ,
    STEREO_WIDTH_MAX, STEREO_WIDTH_MIN, SampleFormat, TILT_MAX_HZ, TILT_MIN_HZ, TONE_MAX_DB,
    TiltConfig, TiltPivot, UpsamplerConfig, check_tilt_sections, resolve_rate_ratio,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// On-disk envelope version. Bumped only when the meaning of existing fields
/// changes; new optional fields do not need a bump.
pub const CONFIG_VERSION: u32 = 1;

/// Sample rates the engine may run at (source rates, before upsampling).
/// The 44.1 kHz and 48 kHz families up to 768k (see `docs/architecture.md`).
pub use audivane_protocol::ALLOWED_RATES;

/// Kernel length limits of a Custom design ([`design::budget`]): 4M =
/// 2^22 at most, so every length step in the UI is a power of two.
pub use audivane_dsp_core::design::budget::{MAX_KERNEL_TAPS as MAX_TAPS, MIN_TAPS};
pub const MAX_BANDS: usize = 4096;
/// Largest output headroom, `output.headroom_db` (dB).
pub const MAX_HEADROOM_DB: f64 = 12.0;

#[derive(Serialize, Deserialize)]
struct ConfigFile {
    version: u32,
    config: EngineConfig,
}

/// Outcome of loading the config: always yields a usable config plus an
/// optional human-readable warning (missing file, corrupt file, no device).
#[derive(Debug)]
pub struct Loaded {
    pub config: EngineConfig,
    pub warning: Option<String>,
}

/// A config file location. Cheap to clone and share.
#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `$XDG_CONFIG_HOME/audivane-dsp/config.json`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from(".config"));
        base.join("audivane-dsp").join("config.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the config without side effects: `None` when it is missing,
    /// unreadable or invalid (nothing is moved aside, no device is filled
    /// in). For `audivane-dsp --volume-failsafe`, which must not change
    /// what the next engine start finds.
    pub fn read_only(&self) -> Option<EngineConfig> {
        let text = fs::read_to_string(&self.path).ok()?;
        let (mut cfg, _) = parse_document(&text).ok()?;
        normalize_config(&mut cfg);
        (validate(&cfg).is_ok() && !cfg.device.is_empty()).then_some(cfg)
    }

    /// Load the config, falling back to defaults (and preserving a corrupted
    /// file) when anything is wrong.
    pub fn load(&self) -> Loaded {
        let mut warnings = Vec::new();
        let mut migrated = false;
        let mut config = match fs::read_to_string(&self.path) {
            Ok(text) => match parse_migrating(&text) {
                Ok((mut cfg, dropped, was_migrated)) => {
                    migrated = was_migrated;
                    if let Some(note) = dropped {
                        warnings.push(format!("{}: {note}", self.path.display()));
                    }
                    // Chain B follows chain A; an older file may still carry a
                    // chain B from before the upsampler/output changed, which
                    // would otherwise look "invalid" and be moved aside.
                    if let Some(note) = normalize_config(&mut cfg) {
                        warnings.push(format!("{}: {note}", self.path.display()));
                    }
                    // A room calibration whose files are gone is dropped on
                    // its own (the followers' settings from before it put
                    // back), not the whole config with it.
                    for w in super::calibration::drop_if_unreadable(&mut cfg) {
                        warnings.push(format!("{}: {w}", self.path.display()));
                    }
                    // A file can parse and still be unusable (e.g. an empty
                    // device copied from another machine). Repair what we can,
                    // otherwise fall back to defaults like a corrupt file.
                    if let Err(err) = validate(&cfg) {
                        let mut repaired = cfg;
                        if !repair(&mut repaired) || validate(&repaired).is_err() {
                            warnings.push(format!(
                                "{} is invalid ({err}); using defaults{}",
                                self.path.display(),
                                match self.backup_corrupt() {
                                    Some(p) => format!(" (moved to {})", p.display()),
                                    None => String::new(),
                                }
                            ));
                            migrated = false;
                            EngineConfig::default()
                        } else {
                            warnings.push(format!(
                                "{}: filled in a missing device; it is saved on the next change",
                                self.path.display()
                            ));
                            repaired
                        }
                    } else {
                        cfg
                    }
                }
                Err(err) => {
                    warnings.push(format!(
                        "{} is invalid ({err}); using defaults{}",
                        self.path.display(),
                        match self.backup_corrupt() {
                            Some(p) => format!(" (moved to {})", p.display()),
                            None => String::new(),
                        }
                    ));
                    EngineConfig::default()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => first_run_config(),
            Err(err) => {
                warnings.push(format!(
                    "cannot read {}: {err}; using defaults",
                    self.path.display()
                ));
                EngineConfig::default()
            }
        };

        if config.device.is_empty() {
            match default_playback_device() {
                Some(device) => config.device = device,
                None => warnings.push(
                    "no ALSA playback device found; set `device` in the config file".to_string(),
                ),
            }
        }
        // A migrated file is written back once, so the next start reads the
        // new format (best effort: a failed write migrates again next time).
        if migrated {
            match self.save(&config) {
                Ok(()) => log::info!("config: {} migrated and saved", self.path.display()),
                Err(err) => log::warn!(
                    "config: {} migrated but not saved ({err}); it migrates again next start",
                    self.path.display()
                ),
            }
        }

        Loaded {
            config,
            warning: if warnings.is_empty() {
                None
            } else {
                Some(warnings.join("; "))
            },
        }
    }

    /// Write `cfg` atomically (temp file + fsync + rename).
    pub fn save(&self, cfg: &EngineConfig) -> io::Result<()> {
        let doc = ConfigFile {
            version: CONFIG_VERSION,
            config: cfg.clone(),
        };
        let mut json = serde_json::to_vec_pretty(&doc)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        json.push(b'\n');
        write_atomic(&self.path, &json)
    }

    fn backup_corrupt(&self) -> Option<PathBuf> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut candidate = self.path.with_extension(format!("json.corrupt-{secs}"));
        if candidate.exists() {
            candidate = self
                .path
                .with_extension(format!("json.corrupt-{secs}-{}", std::process::id()));
        }
        match fs::rename(&self.path, &candidate) {
            Ok(()) => Some(candidate),
            Err(err) => {
                log::warn!(
                    "cannot move corrupt config {} aside: {err}",
                    self.path.display()
                );
                None
            }
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.json".to_string());
    path.with_file_name(format!(".{name}.tmp.{}", std::process::id()))
}

/// Write `bytes` to `path` atomically: a temp file in the same directory is
/// written and fsynced, then renamed over the target (and the directory is
/// fsynced, best effort). Shared by `config.json` and the preset files.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => {}
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
    }
    // Best effort: make the rename durable. Directory fsync is not supported
    // everywhere (and not needed for correctness of the file contents), so a
    // failure here is not fatal.
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Parse the versioned envelope, or a bare [`EngineConfig`] for older/hand
/// written files. The note says what was dropped to read it.
fn parse_document(text: &str) -> Result<(EngineConfig, Option<String>), String> {
    parse_migrating(text).map(|(cfg, note, _)| (cfg, note))
}

/// [`parse_document`], and whether a format migration changed what was
/// read (the caller writes it back once).
fn parse_migrating(text: &str) -> Result<(EngineConfig, Option<String>, bool), String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("not valid JSON: {err}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "top level must be a JSON object".to_string())?;
    let version = object
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    if version > u64::from(CONFIG_VERSION) {
        log::warn!(
            "config version {version} is newer than this daemon understands ({}); \
             reading it anyway",
            CONFIG_VERSION
        );
    }
    let mut cfg_value = object
        .get("config")
        .cloned()
        .unwrap_or_else(|| value.clone());
    let migrated = migrate_follow_volume(&mut cfg_value);
    let migrated = migrate_safety_modes(&mut cfg_value) || migrated;
    match serde_json::from_value::<EngineConfig>(cfg_value.clone()) {
        Ok(cfg) => Ok((cfg, None, migrated)),
        Err(err) => {
            // An A/B setup from an earlier version (0.9.x wrote `chain_b`)
            // must not cost the rest of the settings: drop only that section.
            let dropped = cfg_value
                .as_object_mut()
                .and_then(|o| o.remove("ab"))
                .is_some_and(|ab| !ab.is_null());
            if dropped && let Ok(cfg) = serde_json::from_value::<EngineConfig>(cfg_value) {
                return Ok((
                    cfg,
                    Some(format!(
                        "the A/B setup could not be read ({err}) and was dropped"
                    )),
                    migrated,
                ));
            }
            Err(format!("bad config: {err}"))
        }
    }
}

/// A config from before `follow_volume` (a follower without the field):
/// the followers a room calibration calibrates follow the master's volume
/// (a subwoofer must keep its level to the mains), the others keep their
/// own, as they did. Their reference is the master's level now
/// ([`normalize_followers`]). Returns whether it changed anything (the
/// file is then written back once, [`ConfigStore::load`]).
fn migrate_follow_volume(cfg: &mut serde_json::Value) -> bool {
    let calibrated: Vec<String> = cfg
        .pointer("/calibration/followers")
        .and_then(serde_json::Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("device")?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some(list) = cfg
        .pointer_mut("/outputs/followers")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for f in list {
        let Some(o) = f.as_object_mut() else {
            continue;
        };
        if o.contains_key("follow_volume") {
            continue;
        }
        let device = o.get("device").and_then(|d| d.as_str()).unwrap_or("");
        let on = calibrated.iter().any(|d| d == device);
        log::info!(
            "config: follower {device} {}",
            if on {
                "is calibrated: it follows the main volume"
            } else {
                "keeps its own volume"
            }
        );
        o.insert("follow_volume".into(), serde_json::Value::Bool(on));
        changed = true;
    }
    changed
}

/// Drop the level modes that only cut the level for safety, now the
/// limiter's and the output headroom's job: a crossfeed's `level` (`matched`
/// or `peak_safe`), since the stage always plays at its matched level, and
/// the harmonics routing `series_makeup`, which becomes `series` (with no
/// trims the two are the same chain). A 0.10 harmonics stage becomes layers
/// (`migrate_legacy_harmonics`). Walks the whole document, so the main
/// chain, chain B, followers' patches and presets are all covered. Returns
/// whether it changed anything (the file is then written back once).
pub(crate) fn migrate_safety_modes(value: &mut serde_json::Value) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::Object(o) => {
            if let Some(level) = o
                .get_mut("crossfeed")
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|xf| xf.remove("level"))
            {
                if level.as_str() == Some("peak_safe") {
                    log::info!(
                        "config: crossfeed `peak_safe` level dropped; it plays at its matched level (the limiter and output headroom handle peaks)"
                    );
                }
                changed = true;
            }
            if let Some(routing) = o
                .get_mut("harmonics")
                .and_then(serde_json::Value::as_object_mut)
                .and_then(|h| h.get_mut("routing"))
                .filter(|r| r.as_str() == Some("series_makeup"))
            {
                log::info!("config: harmonics routing `series_makeup` is now `series`");
                *routing = serde_json::Value::String("series".into());
                changed = true;
            }
            if let Some(h) = o
                .get_mut("harmonics")
                .and_then(serde_json::Value::as_object_mut)
            {
                changed |= migrate_legacy_harmonics(h);
            }
            for v in o.values_mut() {
                changed |= migrate_safety_modes(v);
            }
        }
        serde_json::Value::Array(a) => {
            for v in a {
                changed |= migrate_safety_modes(v);
            }
        }
        _ => {}
    }
    changed
}

/// A 0.10 harmonics stage (`character` with `h2_db`..`h5_db`, no `layers`)
/// becomes one Chebyshev layer with the same levels and signs (H3
/// negative, the others positive). The old fields are dropped. Returns
/// whether it changed anything.
fn migrate_legacy_harmonics(h: &mut serde_json::Map<String, serde_json::Value>) -> bool {
    const OLD: [&str; 5] = ["character", "h2_db", "h3_db", "h4_db", "h5_db"];
    if h.contains_key("layers") || !OLD.iter().any(|k| h.contains_key(*k)) {
        return false;
    }
    let num = |k: &str| h.get(k).and_then(serde_json::Value::as_f64);
    let (h2, h3, h4, h5) = match h.get("character").and_then(|c| c.as_str()) {
        Some("pentode") => (Some(-56.0), Some(-42.0), None, Some(-66.0)),
        Some("custom") => (
            Some(num("h2_db").unwrap_or(-40.0)),
            Some(num("h3_db").unwrap_or(-58.0)),
            num("h4_db"),
            num("h5_db"),
        ),
        _ => (Some(-40.0), Some(-58.0), Some(-72.0), None),
    };
    let mut levels = vec![serde_json::Value::Null; audivane_protocol::HARMONICS_ORDERS];
    for (slot, db) in levels.iter_mut().zip([h2, h3, h4, h5]) {
        if let Some(db) = db {
            *slot = serde_json::json!(db.clamp(-120.0, -20.0));
        }
    }
    for k in OLD {
        h.remove(k);
    }
    h.insert(
        "layers".into(),
        serde_json::json!([{ "kind": "chebyshev", "levels_db": levels }]),
    );
    log::info!(
        "config: the 0.10 harmonics setting is now one Chebyshev layer with the same levels"
    );
    true
}

/// Fill in a missing device. Returns true when something was changed.
fn repair(cfg: &mut EngineConfig) -> bool {
    if cfg.device.is_empty()
        && let Some(device) = default_playback_device()
    {
        cfg.device = device;
        return true;
    }
    false
}

// ---------------------------------------------------------------- defaults

/// First `hw:` playback device that is not HDMI, named by card id
/// (`hw:CARD=<id>,DEV=<n>`) so it survives card numbers moving between boots.
/// Loopback cards are only chosen when nothing else is available (the test
/// rig uses them heavily).
pub fn default_playback_device() -> Option<String> {
    let pcm = fs::read_to_string("/proc/asound/pcm").ok()?;
    let cards = fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    default_device_from_listing(&pcm, &cards)
}

/// Pick a default device from the contents of `/proc/asound/pcm` and
/// `/proc/asound/cards` (see [`devices_from_listing`]).
pub fn default_device_from_listing(pcm: &str, cards: &str) -> Option<String> {
    let devices = devices_from_listing(pcm, cards);
    devices
        .iter()
        .find(|d| !d.hdmi && !d.loopback)
        .or_else(|| devices.iter().find(|d| !d.hdmi))
        .map(|d| d.id.clone())
}

/// `hw:<index>[,<dev>[,<subdev>]]` rewritten with the card's id
/// (`hw:CARD=<id>,DEV=<dev>[,SUBDEV=<subdev>]`) while that card exists, so
/// the name keeps pointing at the same hardware after a reboot reorders the
/// cards. `None` for names that already use an id, are not `hw:`, or whose
/// card is not present.
pub fn stable_device_name(device: &str) -> Option<String> {
    let cards = fs::read_to_string("/proc/asound/cards").ok()?;
    stable_device_name_in(device, &cards)
}

/// [`stable_device_name`] against a given `/proc/asound/cards` listing.
pub fn stable_device_name_in(device: &str, cards: &str) -> Option<String> {
    let args = device.trim().strip_prefix("hw:")?;
    let parts: Vec<&str> = args.split(',').map(str::trim).collect();
    if parts.len() > 3 || parts.iter().any(|p| p.contains('=')) {
        return None;
    }
    let index: u32 = parts.first()?.parse().ok()?;
    let numbers: Vec<u32> = parts[1..]
        .iter()
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let (id, _) = parse_cards(cards).remove(&index)?;
    let mut name = format!("hw:CARD={id},DEV={}", numbers.first().copied().unwrap_or(0));
    if let Some(sub) = numbers.get(1) {
        name.push_str(&format!(",SUBDEV={sub}"));
    }
    Some(name)
}

/// Whether `device` names a playback device that exists right now (any
/// spelling [`mark_active`] understands).
///
/// A device whose card name is pinned to another USB device
/// ([`super::device_identity`]) does not count.
pub fn device_present(device: &str) -> bool {
    let mut devices = playback_devices();
    mark_active(&mut devices, device) && audivane_io_alsa::device_present(device)
}

// ---------------------------------------------------------------- devices

/// Every ALSA playback device, from `/proc/asound` only.
///
/// Nothing is opened to enumerate: `/proc/asound/pcm` lists the PCM devices
/// and `/proc/asound/cards` maps card indexes to the card ids that `hw:CARD=`
/// names use.
pub fn playback_devices() -> Vec<DeviceInfo> {
    let pcm = fs::read_to_string("/proc/asound/pcm").unwrap_or_default();
    let cards = fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let mut devices = devices_from_listing(&pcm, &cards);
    let index_of = parse_card_indexes(&cards);
    for d in &mut devices {
        let card = index_of.get(&d.card.to_ascii_lowercase()).copied();
        let dev =
            d.id.rsplit_once("DEV=")
                .and_then(|(_, n)| n.parse::<u32>().ok())
                .unwrap_or(0);
        if let Some(card) = card
            && let Ok(text) = fs::read_to_string(format!("/proc/asound/card{card}/stream{dev}"))
        {
            d.channels = usb_playback_channels(&text);
        }
    }
    devices
}

/// The playback channel counts a USB audio stream file
/// (`/proc/asound/cardN/streamM`) lists, ascending: the `Channels:` lines
/// of its `Playback:` section.
pub fn usb_playback_channels(stream: &str) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    let mut in_playback = false;
    for line in stream.lines() {
        if !line.starts_with(char::is_whitespace) && !line.is_empty() {
            in_playback = line.trim_end() == "Playback:";
            continue;
        }
        if in_playback
            && let Some(n) = line.trim().strip_prefix("Channels:")
            && let Ok(n) = n.trim().parse::<u32>()
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Parse `/proc/asound/pcm` (`00-00: ALC892 Analog : ... : playback 1`) and
/// `/proc/asound/cards` (` 0 [PCH ]: HDA-Intel - HDA Intel PCH`) into
/// playback devices.
pub fn devices_from_listing(pcm: &str, cards: &str) -> Vec<DeviceInfo> {
    let card_names = parse_cards(cards);
    let mut devices = Vec::new();
    for line in pcm.lines() {
        let mut fields = line.split(':');
        let Some(ident) = fields.next() else { continue };
        let rest: Vec<&str> = fields.collect();
        let playback_ok = rest.iter().any(|f| {
            let f = f.trim();
            f.strip_prefix("playback")
                .and_then(|n| n.trim().parse::<u32>().ok())
                .is_some_and(|n| n > 0)
        });
        if !playback_ok {
            continue;
        }
        let Some((card, device)) = ident.trim().split_once('-') else {
            continue;
        };
        let (Ok(card_index), Ok(device_index)) =
            (card.trim().parse::<u32>(), device.trim().parse::<u32>())
        else {
            continue;
        };
        let pcm_name = rest.first().map(|s| s.trim()).unwrap_or("");
        let (card_id, card_name) = card_names
            .get(&card_index)
            .cloned()
            .unwrap_or_else(|| (card_index.to_string(), String::new()));
        let display_name = if pcm_name.is_empty() {
            card_name.clone()
        } else {
            pcm_name.to_string()
        };
        let lower = format!("{display_name} {card_name} {card_id}").to_ascii_lowercase();
        devices.push(DeviceInfo {
            id: format!("hw:CARD={card_id},DEV={device_index}"),
            card: card_id,
            name: display_name,
            hdmi: lower.contains("hdmi"),
            loopback: lower.contains("loopback"),
            active: false,
            suffix: None,
            identity: None,
            channels: Vec::new(),
        });
    }
    devices
}

/// Mark the entry matching the configured device (`hw:0,0`, `hw:DAC,0`,
/// `hw:CARD=DAC,DEV=0`, `hw:CARD=DAC`, any of them with a subdevice such as
/// `hw:Loopback,0,0`, …) as active. Returns true when one entry matched.
pub fn mark_active(devices: &mut [DeviceInfo], configured: &str) -> bool {
    let cards = fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    mark_active_in(devices, configured, &cards)
}

/// [`mark_active`] against a given `/proc/asound/cards` listing (testable).
pub fn mark_active_in(devices: &mut [DeviceInfo], configured: &str, cards: &str) -> bool {
    let index_of = parse_card_indexes(cards);
    let needle = without_subdevice(configured.trim());
    let mut found = false;
    for device in devices.iter_mut() {
        let index = index_of
            .get(&device.card.to_ascii_lowercase())
            .copied()
            .or_else(|| device.card.parse::<u32>().ok())
            .unwrap_or(u32::MAX);
        let dev = device
            .id
            .rsplit_once("DEV=")
            .and_then(|(_, n)| n.parse::<u32>().ok())
            .unwrap_or(0);
        let mut spellings = vec![
            device.id.clone(),
            format!("hw:{index},{dev}"),
            format!("hw:{},{dev}", device.card),
            format!("hw:CARD={},DEV={}", device.card, dev),
        ];
        if dev == 0 {
            spellings.push(format!("hw:{index}"));
            spellings.push(format!("hw:{}", device.card));
            spellings.push(format!("hw:CARD={}", device.card));
        }
        let active = spellings
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&needle));
        device.active = active;
        found |= active;
    }
    found
}

/// `hw:1,0,0` → `hw:1,0`, `hw:CARD=X,DEV=0,SUBDEV=0` → `hw:CARD=X,DEV=0`:
/// a subdevice still names the same playback device.
fn without_subdevice(device: &str) -> String {
    let Some(args) = device.strip_prefix("hw:") else {
        return device.to_string();
    };
    let mut parts: Vec<&str> = args
        .split(',')
        .filter(|p| !p.trim().to_ascii_uppercase().starts_with("SUBDEV="))
        .collect();
    if parts.len() == 3 && !parts.iter().any(|p| p.contains('=')) {
        parts.pop();
    }
    format!("hw:{}", parts.join(","))
}

/// Card index → card id, from `/proc/asound/cards`.
fn parse_cards(text: &str) -> HashMap<u32, (String, String)> {
    let mut cards = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some((index, rest)) = trimmed.split_once(' ') else {
            continue;
        };
        let Ok(index) = index.parse::<u32>() else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some((id, rest)) = rest.split_once(']') else {
            continue;
        };
        let id = id.trim().to_string();
        if id.is_empty() {
            continue;
        }
        // `: HDA-Intel - HDA Intel PCH` → the name after the last `-`.
        let name = rest
            .trim_start()
            .strip_prefix(':')
            .unwrap_or(rest)
            .split_once('-')
            .map(|(_, name)| name.trim().to_string())
            .unwrap_or_default();
        cards.insert(index, (id, name));
    }
    cards
}

fn parse_card_indexes(text: &str) -> HashMap<String, u32> {
    parse_cards(text)
        .into_iter()
        .map(|(index, (id, _))| (id.to_ascii_lowercase(), index))
        .collect()
}

/// Validate a complete engine config. Errors are user-facing strings that name
/// the offending field; the IPC layer turns them into `INVALID_PARAMS`.
pub fn validate(cfg: &EngineConfig) -> Result<(), String> {
    if cfg.device.trim().is_empty() {
        return Err("device must not be empty (e.g. \"hw:0,0\")".into());
    }
    if !cfg.device.starts_with("hw:") {
        return Err(format!(
            "device {:?} must be an ALSA `hw:` device such as \"hw:0,0\"",
            cfg.device
        ));
    }
    if cfg.formats.is_empty() {
        return Err("formats must list at least one sample format".into());
    }
    if let Some(e) = cfg.output_channels.and_then(|p| p.error()) {
        return Err(e);
    }
    if !(MIN_QUANTUM..=MAX_QUANTUM).contains(&cfg.quantum) || !cfg.quantum.is_power_of_two() {
        return Err(format!(
            "quantum must be a power of two between {MIN_QUANTUM} and {MAX_QUANTUM} (got {})",
            cfg.quantum
        ));
    }
    validate_rate_policy(&cfg.rate)?;
    cfg.convolution_safety.validate()?;
    if let Some(name) = &cfg.active_preset {
        crate::control::presets::normalize_name(name)
            .map_err(|message| format!("active_preset: {message}"))?;
    }
    for (part, name) in &cfg.active_part_presets {
        crate::control::presets::normalize_name(name)
            .map_err(|message| format!("active_part_presets.{}: {message}", part.name()))?;
    }
    validate_chain_with(
        "chain",
        &cfg.chain,
        &cfg.rate.allowed_rates,
        rational_pipelines(cfg.ab.is_some()),
        Some(cfg),
    )?;
    if let Some(ab) = &cfg.ab {
        validate_ab(ab, &cfg.chain, &cfg.rate)?;
    }
    match &cfg.routing {
        RoutingMode::System => {}
        RoutingMode::Apps { rules } => {
            for (i, rule) in rules.iter().enumerate() {
                if rule.app.trim().is_empty() {
                    return Err(format!("routing.rules[{i}].app must not be empty"));
                }
            }
        }
    }
    if !cfg.volume.volume_db.is_finite()
        || !(VOLUME_MIN_DB..=VOLUME_MAX_DB).contains(&cfg.volume.volume_db)
    {
        return Err(format!(
            "volume.volume_db must be between {VOLUME_MIN_DB} and {VOLUME_MAX_DB} dB (got {})",
            cfg.volume.volume_db
        ));
    }
    if !cfg.volume.balance_db.is_finite()
        || !(-BALANCE_MAX_DB..=BALANCE_MAX_DB).contains(&cfg.volume.balance_db)
    {
        return Err(format!(
            "volume.balance_db must be between -{BALANCE_MAX_DB} and {BALANCE_MAX_DB} dB (got {})",
            cfg.volume.balance_db
        ));
    }
    validate_volume_extras(&cfg.volume)?;
    validate_varispeed(&cfg.varispeed)?;
    validate_varispeed_budget(cfg)?;
    validate_outputs(cfg)
}

/// Validate the followers (`outputs.followers`, multi-device): at most
/// [`MAX_FOLLOWERS`], each on its own `hw:` device and not the master's,
/// the trim within ±[`FOLLOWER_MAX_TRIM_MS`], the volume within
/// `VOLUME_MIN_DB..=0`; while there are any, an enabled varispeed's speed
/// at most [`FOLLOWED_MAX_SPEED`], and the devices within the CPU budget
/// together ([`validate_followers_budget`]).
pub fn validate_outputs(cfg: &EngineConfig) -> Result<(), String> {
    let followers = &cfg.outputs.followers;
    if followers.len() > MAX_FOLLOWERS {
        return Err(format!(
            "outputs.followers: at most {MAX_FOLLOWERS} followers (got {})",
            followers.len()
        ));
    }
    for (i, f) in followers.iter().enumerate() {
        let at = format!("outputs.followers[{i}]");
        if !f.device.starts_with("hw:") || f.device.trim() != f.device {
            return Err(format!(
                "{at}.device {:?} must be an ALSA `hw:` device such as \"hw:CARD=USB,DEV=0\"",
                f.device
            ));
        }
        if f.device == cfg.device {
            return Err(format!(
                "{at}.device {} is the main output device; a follower plays another one",
                f.device
            ));
        }
        if followers[..i].iter().any(|g| g.device == f.device) {
            return Err(format!("{at}.device {} is listed twice", f.device));
        }
        if !f.trim_ms.is_finite() || f.trim_ms.abs() > FOLLOWER_MAX_TRIM_MS {
            return Err(format!(
                "{at}.trim_ms must be between -{FOLLOWER_MAX_TRIM_MS} and {FOLLOWER_MAX_TRIM_MS} \
                 ms (got {})",
                f.trim_ms
            ));
        }
        if !f.volume_db.is_finite() || !(VOLUME_MIN_DB..=0.0).contains(&f.volume_db) {
            return Err(format!(
                "{at}.volume_db must be between {VOLUME_MIN_DB} and 0 dB (got {})",
                f.volume_db
            ));
        }
        if let Some(e) = f.output_channels.and_then(|p| p.error()) {
            return Err(format!("{at}.{e}"));
        }
        if let Some(p) = f.period_ms
            && (!p.is_finite() || !FOLLOWER_PERIOD_MS_RANGE.contains(&p))
        {
            return Err(format!(
                "{at}.period_ms must be between {} and {} ms (got {p})",
                FOLLOWER_PERIOD_MS_RANGE.start(),
                FOLLOWER_PERIOD_MS_RANGE.end()
            ));
        }
        if f.chain_patch.is_some() {
            // Its own chain, checked as chain A is (its filter at every
            // allowed rate, with the fallbacks of a custom design).
            let own = super::follower_chain::chain(cfg, f, i)?;
            let mut view = cfg.clone();
            view.ab = None;
            view.chain = own;
            validate_chain_with(
                &format!("{at}.chain"),
                &view.chain,
                &cfg.rate.allowed_rates,
                rational_pipelines(false),
                Some(&view),
            )?;
        }
    }
    if followers.is_empty() {
        return Ok(());
    }
    if cfg.varispeed.enabled && cfg.varispeed.speed > FOLLOWED_MAX_SPEED {
        return Err(format!(
            "varispeed.speed is at most {FOLLOWED_MAX_SPEED} while followers play (got {}): \
             each follower corrects its clock on top of it",
            cfg.varispeed.speed
        ));
    }
    validate_followers_budget(cfg, available_cores())
}

fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// The static budget across devices, at every allowed
/// rate: each follower runs its own thread with a varispeed stage (at speed 2
/// in the worst case) and chain A's resampling at the master's ratio (twice
/// during a swap), which must fit one thread's budget
/// ([`check_varispeed_budget`]'s); master and followers together must fit
/// `cores` times it.
pub fn validate_followers_budget(cfg: &EngineConfig, cores: usize) -> Result<(), String> {
    use design::budget::MAX_RATIONAL_MACS_PER_SECOND;
    let n = cfg.outputs.followers.len();
    if n == 0 {
        return Ok(());
    }
    // Each follower's own chain (chain A without a patch).
    let views: Vec<EngineConfig> = cfg
        .outputs
        .followers
        .iter()
        .map(|f| super::follower_chain::config(cfg, f))
        .collect();
    let threads = (n + 1).min(cores.max(1)) as f64;
    for &rate in &cfg.rate.allowed_rates {
        let ratio = resolve_rate_ratio(cfg.chain.upsampler.target, rate);
        let stage = audivane_dsp_core::varispeed::worst_case_macs_per_second(rate)
            .map_err(|e| format!("varispeed: {e}"))?;
        let mut followers = 0.0;
        for view in &views {
            let ratio = resolve_rate_ratio(view.chain.upsampler.target, rate);
            let follower = stage + 2.0 * rational_side_cost(view, None, rate, ratio);
            if follower > MAX_RATIONAL_MACS_PER_SECOND {
                return Err(format!(
                    "a follower at {rate} Hz needs {:.0} M multiply-adds per second per channel \
                     (its clock correction and the chain's {ratio} resampling), more than the \
                     {:.0} M budget; use a shorter filter or a lower output rate",
                    follower / 1e6,
                    MAX_RATIONAL_MACS_PER_SECOND / 1e6
                ));
            }
            followers += follower;
        }
        let sides = if cfg.ab.is_some() { 2.0 } else { 1.0 };
        let master = rational_side_cost(cfg, None, rate, ratio) * sides
            + if cfg.varispeed.enabled { stage } else { 0.0 };
        let total = master + followers;
        if total > MAX_RATIONAL_MACS_PER_SECOND * threads {
            return Err(format!(
                "the main output and {n} follower(s) at {rate} Hz need {:.0} M multiply-adds per \
                 second per channel together, more than the {:.0} M of the {threads} core(s) they \
                 can run on; remove a follower, or use a shorter filter or a lower output rate",
                total / 1e6,
                MAX_RATIONAL_MACS_PER_SECOND * threads / 1e6
            ));
        }
    }
    Ok(())
}

/// Validate varispeed parameters (`speed` and `ramp_ms`). Small quanta stay
/// allowed: the scheduler caps the speed range to what the negotiated period and
/// quantum sustain.
pub fn validate_varispeed(v: &audivane_protocol::ipc::VarispeedConfig) -> Result<(), String> {
    use audivane_dsp_core::varispeed::{MAX_RAMP_MS, MAX_SPEED, MIN_SPEED};
    if !v.speed.is_finite() || !(MIN_SPEED..=MAX_SPEED).contains(&v.speed) {
        return Err(format!(
            "varispeed.speed must be between {MIN_SPEED} and {MAX_SPEED} (got {})",
            v.speed
        ));
    }
    if !v.ramp_ms.is_finite() || !(0.0..=MAX_RAMP_MS).contains(&v.ramp_ms) {
        return Err(format!(
            "varispeed.ramp_ms must be between 0 and {MAX_RAMP_MS} ms (got {})",
            v.ramp_ms
        ));
    }
    Ok(())
}

/// Check the static varispeed budget ([`check_varispeed_budget`]) across
/// `rate.allowed_rates` while varispeed is enabled, including every fractional
/// and decimating landing candidate the driver can pick for
/// [`RateTarget::MaxRateAny`].
pub fn validate_varispeed_budget(cfg: &EngineConfig) -> Result<(), String> {
    if !cfg.varispeed.enabled {
        return Ok(());
    }
    let max_any = match cfg.chain.upsampler.target {
        RateTarget::MaxRateAny(cap) if cfg.dsp_enabled => {
            Some((cap, engine_buildable_rate_pairs(cfg)))
        }
        _ => None,
    };
    for &rate in &cfg.rate.allowed_rates {
        let ratio = resolve_rate_ratio(cfg.chain.upsampler.target, rate);
        check_varispeed_budget(cfg, rate, ratio)?;
        if let Some((cap, buildable)) = max_any {
            for candidate in audivane_io_pw::lifecycle::max_rate_any_ladder(
                rate,
                cap,
                &audivane_io_pw::lifecycle::MASK_OUTPUT_RATES,
                audivane_protocol::MAX_INTEGER_RATIO,
                buildable,
            ) {
                if !candidate.is_integer() {
                    check_varispeed_budget(cfg, rate, candidate)?;
                }
            }
        }
    }
    Ok(())
}

/// Largest fixed hardware level accepted (the mixer clamps to its own
/// range): 0 dB, like the volume. In `software` the fixed level is not part
/// of the volume shown, so a positive one would play louder than it.
pub const HARDWARE_DB_MAX: f64 = VOLUME_MAX_DB;
/// Fastest guard rise accepted, dB per second.
pub const GUARD_RISE_MAX_DB_PER_S: f64 = 1000.0;

fn validate_volume_extras(v: &audivane_protocol::ipc::VolumeConfig) -> Result<(), String> {
    let in_range = |name: &str, value: Option<f64>, lo: f64, hi: f64| match value {
        Some(x) if !x.is_finite() || !(lo..=hi).contains(&x) => {
            Err(format!("{name} must be between {lo} and {hi} dB (got {x})"))
        }
        _ => Ok(()),
    };
    in_range(
        "volume.hardware_db",
        v.hardware_db,
        VOLUME_MIN_DB,
        HARDWARE_DB_MAX,
    )?;
    if v.mode == audivane_protocol::ipc::VolumeMode::Hybrid && v.hardware_db.is_none() {
        return Err(
            "volume.mode \"hybrid\" needs volume.hardware_db (the level the hardware stays at)"
                .into(),
        );
    }
    in_range(
        "volume.max_volume_db",
        v.max_volume_db,
        VOLUME_MIN_DB,
        VOLUME_MAX_DB,
    )?;
    in_range(
        "volume.guard.stream_max_db",
        v.guard.stream_max_db,
        VOLUME_MIN_DB,
        VOLUME_MAX_DB,
    )?;
    let rise = v.guard.max_rise_db_per_s;
    if !rise.is_finite() || !(0.0..=GUARD_RISE_MAX_DB_PER_S).contains(&rise) {
        return Err(format!(
            "volume.guard.max_rise_db_per_s must be between 0 and {GUARD_RISE_MAX_DB_PER_S} (got {rise})"
        ));
    }
    Ok(())
}

fn validate_rate_policy(rate: &RatePolicy) -> Result<(), String> {
    if rate.allowed_rates.is_empty() {
        return Err("rate.allowed_rates must not be empty".into());
    }
    for &r in &rate.allowed_rates {
        if !ALLOWED_RATES.contains(&r) {
            return Err(format!(
                "rate.allowed_rates: {r} Hz is not supported (allowed: {})",
                allowed_rates_text()
            ));
        }
    }
    if !rate.allowed_rates.contains(&rate.default_rate) {
        return Err(format!(
            "rate.default_rate {} must be one of rate.allowed_rates",
            rate.default_rate
        ));
    }
    Ok(())
}

/// Human-readable list of supported rates, for error messages.
pub fn allowed_rates_text() -> String {
    ALLOWED_RATES
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_eq_channel(
    prefix: &str,
    preamp_db: f64,
    bands: &[audivane_protocol::Band],
) -> Result<(), String> {
    if !preamp_db.is_finite() {
        return Err(format!("{prefix}.preamp_db must be finite"));
    }
    if !(-120.0..=60.0).contains(&preamp_db) {
        return Err(format!(
            "{prefix}.preamp_db must be between -120 and 60 dB (got {preamp_db})"
        ));
    }
    if bands.len() > MAX_BANDS {
        return Err(format!(
            "{prefix}.bands: {} bands is more than the {MAX_BANDS} supported",
            bands.len()
        ));
    }
    for (i, band) in bands.iter().enumerate() {
        if !band.freq_hz.is_finite() || band.freq_hz <= 0.0 || band.freq_hz >= 1_536_000.0 {
            return Err(format!(
                "{prefix}.bands[{i}].freq_hz must be between 0 and 1536000 Hz (got {})",
                band.freq_hz
            ));
        }
        if !band.q.is_finite() || band.q <= 0.0 {
            return Err(format!(
                "{prefix}.bands[{i}].q must be greater than 0 (got {})",
                band.q
            ));
        }
        if !band.gain_db.is_finite() || !(-120.0..=120.0).contains(&band.gain_db) {
            return Err(format!(
                "{prefix}.bands[{i}].gain_db must be between -120 and 120 dB (got {})",
                band.gain_db
            ));
        }
    }
    Ok(())
}

fn validate_eq_tilt(prefix: &str, tilt: &TiltConfig) -> Result<(), String> {
    if !(TILT_MIN_HZ..=TILT_MAX_HZ).contains(&tilt.start_hz) {
        return Err(format!(
            "{prefix}.start_hz must be between {TILT_MIN_HZ} and {TILT_MAX_HZ} Hz (got {})",
            tilt.start_hz
        ));
    }
    if !tilt.end_hz.is_finite() || tilt.end_hz <= tilt.start_hz || tilt.end_hz > TILT_MAX_HZ {
        return Err(format!(
            "{prefix}.end_hz must be between {} and {TILT_MAX_HZ} Hz (got {})",
            tilt.start_hz, tilt.end_hz
        ));
    }
    if (tilt.end_hz / tilt.start_hz).log2() < 1.0 / 6.0 {
        return Err(format!("{prefix}: band must span at least 1/6 octave"));
    }
    // These are rate-independent limits. The engine and chain builder also
    // validate the effective band against the actual graph rate.
    match (tilt.slope_db_per_oct, tilt.total_db) {
        (Some(s), None) if s.is_finite() => {
            if s.abs() > 6.0 {
                return Err(format!(
                    "{prefix}.slope_db_per_oct must be between -6 and 6 dB/oct (got {s})"
                ));
            }
        }
        (None, Some(t)) if t.is_finite() => {
            if t.abs() > 24.0 {
                return Err(format!(
                    "{prefix}.total_db must be between -24 and 24 dB (got {t})"
                ));
            }
        }
        (Some(_), Some(_)) | (None, None) => {
            return Err(format!(
                "{prefix}: specify exactly one of slope_db_per_oct or total_db"
            ));
        }
        _ => {
            return Err(format!(
                "{prefix}: slope_db_per_oct / total_db must be finite"
            ));
        }
    }
    if let Some(slope) = tilt.effective_slope_db_per_oct()
        && slope.abs() > 6.0 + 1e-9
    {
        return Err(format!(
            "{prefix}: effective slope {slope:.3} dB/oct exceeds +/-6 dB/oct"
        ));
    }
    if let Some(total) = tilt.effective_total_db()
        && total.abs() > 24.0 + 1e-9
    {
        return Err(format!(
            "{prefix}: effective total {total:.3} dB exceeds +/-24 dB"
        ));
    }
    if let TiltPivot::Hz(hz) = tilt.pivot
        && (!hz.is_finite() || hz < tilt.start_hz || hz > tilt.end_hz)
    {
        return Err(format!(
            "{prefix}.pivot_hz must be between {} and {} Hz (got {hz})",
            tilt.start_hz, tilt.end_hz
        ));
    }
    Ok(())
}

/// Validate one upsampler config (taps, Kaiser beta, cutoff, band, phase,
/// headroom, target). `field` is the full prefix used in messages, e.g.
/// `"chain.upsampler"`. Shared by chain validation and the measured-response
/// call, so an explicit `UpsamplerResponse { config }` cannot bypass the
/// kernel limits. Lengths are checked at the largest ratio the target reaches
/// (the longest kernel); rational ratios at every rate in [`ALLOWED_RATES`]
/// (see [`validate_upsampler_at`]).
pub fn validate_upsampler(field: &str, up: &UpsamplerConfig) -> Result<(), String> {
    validate_upsampler_at(field, up, ALLOWED_RATES, rational_pipelines(false))
}

/// Rational pipelines that can run at once for a config: a swap or
/// crossfade runs the old and the new chain, A/B runs two chains.
pub fn rational_pipelines(ab: bool) -> usize {
    design::budget::RATIONAL_SWAP_PIPELINES * if ab { 2 } else { 1 }
}

/// [`validate_upsampler`], checking rational ratios only at the source rates
/// in `rates` (the rate policy's, or the one a measurement runs at). A
/// rational kernel's cost grows with the input rate over the output rate, so
/// a design fine from 96 kHz can be far too expensive from 768 kHz: only the
/// rates the graph can run at matter. Integer ratios keep the check over
/// every engine rate.
pub fn validate_upsampler_at(
    field: &str,
    up: &UpsamplerConfig,
    rates: &[u32],
    pipelines: usize,
) -> Result<(), String> {
    validate_upsampler_variants(field, std::slice::from_ref(up), rates, pipelines)
}

/// What is wrong with `target` (the rest of a message after `.target`), or
/// `None`: its bounds, and the ratio terms at the source rates in `rates`.
fn target_error(target: RateTarget, rates: &[u32]) -> Option<String> {
    match target {
        RateTarget::Off => {}
        RateTarget::Ratio(ratio) => {
            if !(1..=64).contains(&ratio) {
                return Some(format!(" ratio must be between 1 and 64 (got {ratio})"));
            }
        }
        RateTarget::MaxRate(max) | RateTarget::MaxRateAny(max) => {
            let top = audivane_protocol::MAX_OUTPUT_RATE;
            if !(8_000..=top).contains(&max) {
                return Some(format!(
                    " max rate must be between 8000 and {top} Hz (got {max})"
                ));
            }
            if max < 44_100 {
                return Some(format!(
                    " max rate {max} Hz is below the lowest source rate"
                ));
            }
        }
        RateTarget::Rate(rate) => {
            if !audivane_protocol::is_output_rate(rate) {
                return Some(format!(
                    " rate must be one of {ALLOWED_RATES:?} or {:?} Hz (got {rate})",
                    audivane_protocol::HIGH_OUTPUT_RATES
                ));
            }
        }
    }
    for (rate, r) in rational_ratios(target) {
        let limit = audivane_io_pw::lifecycle::MAX_RATIONAL_TERM;
        if rates.contains(&rate) && (r.up > limit || r.down > limit) {
            return Some(format!(
                ": at {rate} Hz the ratio {r} has a term above {limit}"
            ));
        }
    }
    None
}

/// [`validate_upsampler_at`] for the resolved forms of one config (the
/// same config with `match_output` resolved against each output it can run
/// with, see [`runtime_outputs`]). Only automatic taps differ between them;
/// the other checks use the first.
fn validate_upsampler_variants(
    field: &str,
    variants: &[UpsamplerConfig],
    rates: &[u32],
    pipelines: usize,
) -> Result<(), String> {
    let up = &variants[0];
    if let Some(e) = target_error(up.target, rates) {
        return Err(format!("{field}.target{e}"));
    }
    let ratio = worst_case_ratio(up.target);
    let rational: Vec<_> = rational_ratios(up.target)
        .into_iter()
        .filter(|(rate, _)| rates.contains(rate))
        .collect();
    let named = design::Named::of(up).map_err(|e| format!("{field}: {e}"))?;
    if let Some(preset) = named.preset {
        for &rate in ALLOWED_RATES {
            let r = resolve_rate_ratio(up.target, rate);
            if r.is_unity() || (!r.is_integer() && !rates.contains(&rate)) {
                continue;
            }
            if r.is_integer() {
                design::feasibility(&named.design, &design_context(rate, r, pipelines)).map_err(
                    |e| format!("{field}.preset: at {rate} Hz, {}", preset_error(preset, e)),
                )?;
            } else {
                check_rational_at(field, up, rate, r, pipelines)?;
            }
        }
        return Ok(());
    }

    // Per-parameter bounds for every custom design (and `recommended`).
    design::check::check(&named.design).map_err(|e| format!("{field}.{e}"))?;

    let is_auto = matches!(
        &named.design,
        FilterDesign::Kernel(k)
            if matches!(
                k.shape,
                Shape::WindowedSinc { length: Length::Auto(_), .. }
                    | Shape::Optimal { length: Length::Auto(_), .. }
            )
    );
    if is_auto {
        validate_automatic(field, variants, pipelines)?;
        // Each pair runs its own search (a novel design's): in parallel,
        // the first refusal in order reported.
        let jobs: Vec<_> = variants
            .iter()
            .flat_map(|v| rational.iter().map(move |&(rate, r)| (v, rate, r)))
            .collect();
        let results = run_parallel(&jobs, |&(v, rate, r)| {
            check_rational_at(field, v, rate, r, pipelines)
        });
        for result in results {
            result.unwrap_or_else(|| Err(format!("{field}: the check did not complete")))?;
        }
        return Ok(());
    }

    // Check NyquistStop calibrated range and preserve_samples decimation rules.
    if let FilterDesign::Kernel(k) = &named.design {
        if k.preserve_samples
            && let Some(&(rate, r)) = rational.iter().find(|(_, r)| r.is_decimating())
        {
            return Err(format!(
                "{field}.design.preserve_samples needs an output rate at or above the input rate \
                 (at {rate} Hz the target resamples by {r})"
            ));
        }
        if let Shape::WindowedSinc {
            cutoff: audivane_protocol::design::Cutoff::NyquistStop,
            ..
        } = &k.shape
            && let Ok(manual) = design::paths::standard::legacy_config(k, ALLOWED_RATES[0])
        {
            if ratio > 1 {
                let eff_taps = match manual.taps_unit {
                    audivane_protocol::TapsUnit::Output => manual.taps,
                    audivane_protocol::TapsUnit::Input => {
                        manual.taps.saturating_mul(ratio as usize)
                    }
                };
                audivane_dsp_core::resample::nyquist_stop_check_for(
                    manual.window,
                    eff_taps,
                    ratio as usize,
                    manual.kaiser_beta,
                )
                .map_err(|e| format!("{field}.design.cutoff: {e}"))?;
            }
            for &(rate, r) in &rational {
                let grid_ratio = design::paths::standard::design_ratio(r);
                let grid_taps = match manual.taps_unit {
                    audivane_protocol::TapsUnit::Output => {
                        manual.taps.saturating_mul(r.down as usize)
                    }
                    audivane_protocol::TapsUnit::Input => manual.taps.saturating_mul(r.up as usize),
                };
                audivane_dsp_core::resample::nyquist_stop_check_for(
                    manual.window,
                    grid_taps,
                    grid_ratio,
                    manual.kaiser_beta,
                )
                .map_err(|e| format!("{field}.design.cutoff: at {rate} Hz ({r}), {e}"))?;
            }
        }
    }

    // Integer ratios the target reaches: check every source rate so stepped
    // values and output-span rules are checked everywhere they run.
    let mut checked_integer = false;
    for &rate in ALLOWED_RATES {
        let r = resolve_rate_ratio(up.target, rate);
        if r.is_unity() || !r.is_integer() {
            continue;
        }
        checked_integer = true;
        design::feasibility(&named.design, &design_context(rate, r, pipelines))
            .map_err(|e| refused_at(field, &named.design, &format!("{rate} Hz ({r})"), &e))?;
    }
    if !checked_integer && ratio > 1 {
        design::feasibility(
            &named.design,
            &design_context(ALLOWED_RATES[0], RateRatio::integer(ratio), pipelines),
        )
        .map_err(|e| {
            let at = format!("{} Hz (x{ratio})", ALLOWED_RATES[0]);
            refused_at(field, &named.design, &at, &e)
        })?;
    }

    // Rational ratios: check on the intermediate grid with the cost budget.
    for &(rate, r) in &rational {
        check_rational_at(field, up, rate, r, pipelines)?;
    }
    Ok(())
}

/// Whether a custom design (not a preset) is checked per rate in chain A:
/// one that builds at some of its allowed rates is applied there, and the
/// others play Recommended ([`super::cpu_budget::plan_rates`]).
pub fn falls_back_per_rate(up: &UpsamplerConfig) -> bool {
    up.preset.is_none() && up.design.is_some() && up.target != RateTarget::Off
}

/// [`validate_upsampler_variants`] for chain A of `cfg`: a custom design
/// ([`falls_back_per_rate`]) that fails the check passes when it builds at
/// one of the landings the driver's ladder picks at least
/// ([`chain_a_verdict`]: the same landings and checks as `check_design` and
/// the per-rate plan); refused with plain words when it builds at none
/// ([`plain_refusal`]).
fn validate_chain_a_upsampler(
    field: &str,
    variants: &[UpsamplerConfig],
    rates: &[u32],
    pipelines: usize,
    cfg: &EngineConfig,
) -> Result<(), String> {
    let strict = validate_upsampler_variants(field, variants, rates, pipelines);
    let up = &variants[0];
    let Err(e) = strict else {
        return Ok(());
    };
    // A target the graph cannot run is the target's error, not the design's.
    if !falls_back_per_rate(up) || target_error(up.target, rates).is_some() {
        return Err(e);
    }
    // The per-field bounds hold at every rate.
    let named = design::Named::of(up).map_err(|e| format!("{field}: {e}"))?;
    if design::check::check(&named.design).is_err() {
        return Err(e);
    }
    match chain_a_verdict(cfg, &cfg.chain.upsampler, rates) {
        Ok(_) => Ok(()),
        Err(cases) => Err(plain_refusal(field, up, cfg, &cases).unwrap_or(e)),
    }
}

/// `cfg` with chain A's upsampler `up` and DSP on: where it would land.
fn with_chain_a(cfg: &EngineConfig, up: &UpsamplerConfig) -> EngineConfig {
    let mut c = cfg.clone();
    c.dsp_enabled = true;
    c.chain.upsampler = up.clone();
    c
}

/// The landings of chain A's upsampler `up` in `cfg` at `rates`, as the
/// driver's ladder picks them without a device probe (the design's
/// buildable pairs, Recommended's from a rate it builds at no reachable
/// rung of: [`engine_buildable_rate_pairs`]); the target's own ratio at a
/// rate the ladder does not offer. x1 left out.
pub fn chain_a_landings(
    cfg: &EngineConfig,
    up: &UpsamplerConfig,
    rates: &[u32],
) -> Vec<(u32, RateRatio)> {
    let ladder =
        crate::engine::predicted_landings(&with_chain_a(cfg, up), None).unwrap_or_default();
    rates
        .iter()
        .map(|&rate| {
            let r = ladder
                .iter()
                .find(|l| l.0 == rate)
                .map_or_else(|| resolve_rate_ratio(up.target, rate), |l| l.1);
            (rate, r)
        })
        .filter(|(_, r)| !r.is_unity())
        .collect()
}

/// Whether chain A's custom design `up` is applied in `cfg`: it builds
/// ([`chain_a_builds_at`]) at one of its landings at `rates` at least
/// ([`chain_a_landings`]); `Ok` with the landings, or the landings with
/// their refusals (for [`design::explain`]) when it builds at none. What
/// the config validation and `check_design` both decide on.
pub fn chain_a_verdict(
    cfg: &EngineConfig,
    up: &UpsamplerConfig,
    rates: &[u32],
) -> Result<Vec<(u32, RateRatio)>, Vec<design::explain::Case>> {
    let c = with_chain_a(cfg, up);
    let ladder = chain_a_landings(cfg, up, rates);
    // Where the ladder upsamples nowhere though the target does (neither
    // the design nor Recommended builds there), the target's own ratios
    // decide: a design that plays nowhere is refused.
    let landings = if ladder.is_empty() {
        rates
            .iter()
            .map(|&rate| (rate, resolve_rate_ratio(up.target, rate)))
            .filter(|(_, r)| !r.is_unity())
            .collect()
    } else {
        ladder.clone()
    };
    if landings.is_empty() {
        return Ok(ladder);
    }
    let results = run_parallel(&landings, |&(rate, r)| chain_a_builds_at(&c, rate, r).err());
    if results.iter().any(|r| matches!(r, Some(None))) {
        return Ok(ladder);
    }
    let pipelines = rational_pipelines(cfg.ab.is_some());
    Err(landings
        .iter()
        .zip(results)
        .map(|(&(rate, r), e)| design::explain::Case {
            cx: BuildContext {
                output: cfg.chain.output,
                ..design_context(rate, r, pipelines)
            },
            error: Some(e.flatten().map_or_else(
                || design::DesignError::Failed("the check did not complete".into()),
                ChainAFailure::into_design_error,
            )),
        })
        .collect())
}

/// A one-click fix's check: `d` as chain A's custom design of `cfg` builds
/// at the case's rate and ratio, as the config validation checks it
/// ([`design_builds_at`]), so applying it is never refused there.
fn fix_builds(cfg: &EngineConfig) -> impl Fn(&FilterDesign, &BuildContext) -> bool + '_ {
    move |d, cx| design_builds_at(cfg, d, cx.source_rate, cx.ratio)
}

/// The refusal of chain A's custom design `up` in `cfg` that builds at none
/// of its landings (`cases`, [`chain_a_verdict`]), in plain words
/// ([`design::explain`]), under the field its first landing's refusal is
/// about, with the engine's own words there last:
/// `chain.upsampler.design.length: 1,279 taps is too short at ... (At
/// 44.1k ×16: an equiripple design of 1279 taps ...)`.
fn plain_refusal(
    field: &str,
    up: &UpsamplerConfig,
    cfg: &EngineConfig,
    cases: &[design::explain::Case],
) -> Option<String> {
    let d = up.design.as_ref()?;
    let c = with_chain_a(cfg, up);
    let x = design::explain::explain_with(d, cases, false, &fix_builds(&c));
    let first = x.rates.first()?;
    // The engine's own words at the first rate, after the plain ones (the
    // error has no details disclosure; `check_design` lists every rate's).
    Some(format!(
        "{field}.{}: {} (At {} {}: {}.)",
        first.path,
        x.message,
        design::explain::rate_label(first.source_rate),
        design::explain::ratio_label(first.ratio),
        first.detail
    ))
}

/// Whether `up` can be built at the non-integer ratio `r` from `rate`: the
/// design's ratio rules, length and cost limits with `pipelines` copies
/// running at once ([`rational_pipelines`]). `field` prefixes the messages.
pub fn check_rational_at(
    field: &str,
    up: &UpsamplerConfig,
    rate: u32,
    r: audivane_protocol::RateRatio,
    pipelines: usize,
) -> Result<(), String> {
    let hint = format!("(or remove {rate} Hz from rate.allowed_rates)");
    let named = design::Named::of(up).map_err(|e| format!("{field}: {e}"))?;
    if let Some(preset) = named.preset {
        return design::feasibility(&named.design, &design_context(rate, r, pipelines))
            .map(|_| ())
            .map_err(|e| match e {
                e @ (design::DesignError::Ratio { .. } | design::DesignError::TooLong { .. }) => {
                    format!("{field}.preset: at {rate} Hz, {}", preset_error(preset, e))
                }
                e => format!(
                    "{field}.preset: at {rate} Hz, {}: use a shorter preset or a lower output \
                     rate {hint}",
                    preset_error(preset, e)
                ),
            });
    }
    if let FilterDesign::Kernel(k) = &named.design
        && k.preserve_samples
        && r.is_decimating()
    {
        return Err(format!(
            "{field}.design.preserve_samples needs an output rate at or above the input rate \
             (at {rate} Hz the target resamples by {r})"
        ));
    }
    // An automatic standard design over the limits is refused from its cheap
    // lower bound before running the search.
    if let FilterDesign::Kernel(k) = &named.design
        && k.numerics.path == design::ArithmeticPath::Standard
        && matches!(
            k.shape,
            Shape::WindowedSinc {
                length: Length::Auto(_),
                ..
            }
        )
        && let Ok(manual) = design::paths::standard::legacy_config_for(k, rate, r)
    {
        let lower = audivane_dsp_core::filter_design::automatic_taps_lower_bound(&manual, r)
            .map_err(|e| format!("{field}.design.length: at {rate} Hz ({r}), {e}"))?;
        let mixed_phase = manual.phase == FilterPhase::Intermediate
            && manual.phase_mix > 0.0
            && manual.phase_mix < 1.0;
        let growth = if mixed_phase {
            INTERMEDIATE_MAX_GROWTH
        } else {
            1.0
        };
        design::budget::check_rational(
            (lower as f64 * growth) as usize,
            r,
            rate,
            design::budget::MAX_RATIONAL_GRID_TAPS,
            pipelines,
        )
        .map_err(|e| {
            format!(
                "{field}.design.length: at {rate} Hz, {e}: widen the transition or lower the target, \
                 or use a preset {hint}"
            )
        })?;
    }
    design::searched_feasibility(&named.design, &design_context(rate, r, pipelines))
        .map(|_| ())
        .map_err(|e| {
            let at = format!("{rate} Hz ({r})");
            format!("{} {hint}", refused_at(field, &named.design, &at, &e))
        })
}

/// `check_design`: `design`'s field bounds and, at each source rate it would
/// run at, its ratio rules and budgets ([`design::feasibility`]; no kernel
/// designed, no search run). The rest of the context comes from `cfg`:
/// `output` (chain A's, dither bits capped at the negotiated format) resolves
/// `match_output`; the rate policy's rates (or only `source_rate`); the A/B
/// pipeline count; the engine block at each rate. `target` defaults to chain
/// A's. Plans are priced with `rates`. With `landings` (the device was
/// probed: [`crate::engine::preview_landings`]), each rate's plan is for
/// the ratio the engine would open the device at, and a rate it does not
/// offer there gets none; the refusals stay at the target's own ratio.
/// `notes` holds one entry per played rate where a band's passband end in
/// Hz does not fit and the band's fraction stands in
/// ([`design::band_pin_note`]).
pub fn check_design(
    stored: &StoredDesign,
    target: Option<RateTarget>,
    source_rate: Option<u32>,
    cfg: &EngineConfig,
    output: &OutputConfig,
    rates: &audivane_dsp_core::cpu_bench::KernelRates,
    landings: Option<&[(u32, RateRatio)]>,
) -> audivane_protocol::ipc::DesignCheck {
    use audivane_protocol::design::FieldError;
    use audivane_protocol::ipc::{
        DesignAdvice, DesignCheck, DesignFix, RateDetail, RateNote, RatePlan,
    };
    let mut out = DesignCheck {
        errors: Vec::new(),
        plans: Vec::new(),
        notes: Vec::new(),
        advice: None,
    };
    let fail = |out: &mut DesignCheck, path: &str, message: String| {
        out.errors.push(FieldError {
            path: path.into(),
            message,
        })
    };
    let target = target.unwrap_or(cfg.chain.upsampler.target);
    let policy = &cfg.rate.allowed_rates;
    let planned: Vec<u32> = match source_rate {
        Some(rate) => {
            let top = audivane_protocol::MAX_OUTPUT_RATE;
            if !(8_000..=top).contains(&rate) {
                fail(
                    &mut out,
                    "source_rate",
                    format!("must be between 8000 and {top} Hz (got {rate})"),
                );
                return out;
            }
            vec![rate]
        }
        None => policy.clone(),
    };
    if let Some(e) = target_error(target, &planned) {
        let e = e.trim_start_matches([':', ' ']).to_string();
        fail(&mut out, "target", e);
    }
    let d = stored.design.with_output(output);
    out.errors.extend(design::check::check_all(&d));
    if !out.errors.is_empty() {
        return out;
    }
    let pipelines = rational_pipelines(cfg.ab.is_some());
    let context = |rate: u32, ratio: RateRatio| BuildContext {
        source_rate: rate,
        ratio,
        channels: 2,
        max_block: audivane_io_pw::scaled_quantum(cfg.quantum, rate).clamp(512, 8192) as usize,
        pipelines,
        output: *output,
        workspace: design::budget::design_workspace(),
    };
    let up = UpsamplerConfig {
        target,
        preset: None,
        design: Some(stored.clone()),
    };
    let c = with_chain_a(cfg, &up);
    let verify = fix_builds(&c);
    let explain_at = |cases: &[design::explain::Case], applied: bool| {
        let x = design::explain::explain_with(&stored.design, cases, applied, &verify);
        let advice = DesignAdvice {
            message: x.message.clone(),
            applied,
            details: x
                .rates
                .iter()
                .map(|r| RateDetail {
                    source_rate: r.source_rate,
                    ratio: r.ratio,
                    path: r.path.clone(),
                    message: r.detail.clone(),
                })
                .collect(),
            fixes: x
                .fixes
                .iter()
                .map(|f| DesignFix {
                    label: f.label.clone(),
                    design: StoredDesign {
                        based_on: stored.based_on.clone(),
                        design: f.design.clone(),
                    },
                })
                .collect(),
        };
        (x, advice)
    };
    // Whether the design is applied, as the config validation decides it
    // ([`chain_a_verdict`]: at the landings the ladder picks): refused, in
    // plain words, when it builds at none of them.
    let ladder = match chain_a_verdict(cfg, &up, &planned) {
        Ok(ladder) => ladder,
        Err(cases) => {
            let (x, advice) = explain_at(&cases, false);
            let path = x.rates.first().map_or("design", |r| r.path.as_str());
            fail(&mut out, path, x.message);
            out.advice = Some(advice);
            return out;
        }
    };
    let push_plan = |out: &mut DesignCheck, rate: u32, ratio: RateRatio, plan: design::Plan| {
        let load = plan
            .callback
            .seconds(rates, rate, context(rate, ratio).max_block)
            .ok()
            .map(|secs| super::cpu_budget::thread_load(secs, cfg));
        out.plans.push(RatePlan {
            source_rate: rate,
            ratio,
            plan: plan.wire(load, None),
        });
    };
    // Each rate where the engine opens it (on a probed device, `landings`;
    // `None` there: it does not offer the rate, PipeWire converts it; else
    // where the ladder lands it): the design's plan, or its substitute's
    // where it does not build there ([`super::cpu_budget::played_at`], the
    // per-rate plan's choice).
    let mut played: Vec<design::explain::Case> = Vec::new();
    let mut plays: Vec<(u32, &'static str)> = Vec::new();
    for &rate in &planned {
        let landing = match landings {
            Some(l) => l.iter().find(|x| x.0 == rate).map(|x| x.1),
            None => ladder.iter().find(|x| x.0 == rate).map(|x| x.1),
        };
        let Some(q) = landing.filter(|q| !q.is_unity()) else {
            continue;
        };
        let error = chain_a_builds_at(&c, rate, q)
            .err()
            .map(ChainAFailure::into_design_error);
        match &error {
            None => {
                if let Ok(plan) = design::feasibility(&d, &context(rate, q)) {
                    push_plan(&mut out, rate, q, plan);
                }
                // A pinned passband end that falls back here: at the
                // landing, whose lower rate the band is of.
                if let Some(message) = design::band_pin_note(&d, rate, q) {
                    out.notes.push(RateNote {
                        source_rate: rate,
                        path: "design.cutoff.passband_end_hz".into(),
                        message,
                        plays: None,
                    });
                }
            }
            Some(_) => {
                if let Some(sub) =
                    super::cpu_budget::design_substitute(Some(rates), &c, None, rate, q)
                    && let Ok(named) = design::Named::of(&sub.up)
                    && let Ok(plan) =
                        design::feasibility(&named.design.with_output(output), &context(rate, q))
                {
                    push_plan(&mut out, rate, q, plan);
                    plays.push((rate, plays_name(sub.action)));
                }
            }
        }
        played.push(design::explain::Case {
            cx: context(rate, q),
            error,
        });
    }
    if played.iter().any(|c| c.error.is_some()) {
        let (x, advice) = explain_at(&played, true);
        for r in &x.rates {
            let name = plays
                .iter()
                .find(|p| p.0 == r.source_rate)
                .map_or(design::RECOMMENDED_PRESET, |p| p.1);
            out.notes.push(RateNote {
                source_rate: r.source_rate,
                path: r.path.clone(),
                message: r.short.clone(),
                plays: Some(name.to_string()),
            });
        }
        out.notes.sort_by_key(|n| n.source_rate);
        out.advice = Some(advice);
    }
    out
}

/// [`audivane_protocol::ipc::RateNote::plays`] for a substitute:
/// `recommended`, or `recommended_light` for a lighter form of it.
fn plays_name(action: audivane_protocol::ipc::OverloadAction) -> &'static str {
    match action {
        audivane_protocol::ipc::OverloadAction::LightUpsampler => "recommended_light",
        _ => design::RECOMMENDED_PRESET,
    }
}

/// The build context the daemon checks a design in: `rate` at `r` with
/// `pipelines` rational pipelines (the chain's channels and output do not
/// change a feasibility verdict; an automatic budget is resolved by the
/// caller).
fn design_context(rate: u32, r: RateRatio, pipelines: usize) -> BuildContext {
    BuildContext {
        source_rate: rate,
        ratio: r,
        channels: 2,
        max_block: 1024,
        pipelines,
        output: OutputConfig::default(),
        workspace: design::budget::design_workspace(),
    }
}

/// The library's feasibility of a design at `r` from `rate`.
fn custom_feasibility_at(
    field: &str,
    up: &UpsamplerConfig,
    rate: u32,
    r: RateRatio,
    pipelines: usize,
    searched: bool,
) -> Result<(), String> {
    let named = design::Named::of(up).map_err(|e| format!("{field}: {e}"))?;
    nyquist_stop_at(&named.design, r).map_err(|e| format!("{field}.{e}"))?;
    let cx = design_context(rate, r, pipelines);
    if searched {
        design::searched_feasibility(&named.design, &cx)
    } else {
        design::feasibility(&named.design, &cx)
    }
    .map(|_| ())
    .map_err(|e| refused_at(field, &named.design, &format!("{rate} Hz ({r})"), &e))
}

/// A refusal of `d` at `at` (a rate and ratio), naming the field it is
/// about (`chain.upsampler.design.length: at 44100 Hz (x8), ...`).
fn refused_at(field: &str, d: &FilterDesign, at: &str, e: &design::DesignError) -> String {
    let f = design::check::refusal(e, d);
    format!("{field}.{}: at {at}, {}", f.path, f.message)
}

/// The static budget with varispeed:
/// the stage's worst case (speed 2) once, on top of the rational pipelines the
/// chains can run at once at `ratio` from `graph_rate`, within
/// [`design::budget::MAX_RATIONAL_MACS_PER_SECOND`] per channel. Integer
/// upsampling (FFT) is not counted, as in the rational check.
///
/// Without a negotiated `format` each side counts its most expensive design
/// over the formats it can run with (`MatchOutput` resolves per format); with
/// one, the design that format builds. With `outgoing`, a swap's pair is the
/// outgoing chain and the new one, whichever is dearer counting twice.
pub fn check_varispeed_budget(
    cfg: &EngineConfig,
    graph_rate: u32,
    ratio: audivane_protocol::RateRatio,
) -> Result<(), String> {
    check_varispeed_budget_for(cfg, None, None, graph_rate, ratio)
}

/// [`check_varispeed_budget`] for the chains a negotiated `format` builds,
/// and a swap from `outgoing`.
pub fn check_varispeed_budget_for(
    cfg: &EngineConfig,
    outgoing: Option<&EngineConfig>,
    format: Option<SampleFormat>,
    graph_rate: u32,
    ratio: audivane_protocol::RateRatio,
) -> Result<(), String> {
    check_stage_budget_for(cfg, outgoing, format, graph_rate, ratio, false)
}

/// [`check_varispeed_budget_for`] for the stage that runs: with
/// `keep_pitch`, the key-locked stretch's cost
/// ([`audivane_dsp_core::tempo::macs_per_second`]) in place of
/// varispeed's.
pub fn check_stage_budget_for(
    cfg: &EngineConfig,
    outgoing: Option<&EngineConfig>,
    format: Option<SampleFormat>,
    graph_rate: u32,
    ratio: audivane_protocol::RateRatio,
    keep_pitch: bool,
) -> Result<(), String> {
    use design::budget::MAX_RATIONAL_MACS_PER_SECOND;
    let stage = if keep_pitch {
        audivane_dsp_core::tempo::macs_per_second(graph_rate).map_err(|e| format!("tempo: {e}"))?
    } else {
        audivane_dsp_core::varispeed::worst_case_macs_per_second(graph_rate)
            .map_err(|e| format!("varispeed: {e}"))?
    };
    let (what, at) = if keep_pitch {
        ("varispeed with Keep pitch", "at its slowest speed")
    } else {
        ("varispeed", "at speed 2")
    };
    if keep_pitch && stage > MAX_RATIONAL_MACS_PER_SECOND {
        return Err(format!(
            "Keep pitch is not available at {graph_rate} Hz: the time stretch alone needs \
             {:.0} M multiply-adds per second per channel {at}, more than the {:.0} M budget; \
             play at a lower rate or turn Keep pitch off",
            stage / 1e6,
            MAX_RATIONAL_MACS_PER_SECOND / 1e6
        ));
    }
    let sides = |c: &EngineConfig| if c.ab.is_some() { 2.0 } else { 1.0 };
    // The chains that run here: chain A's custom design where it builds,
    // its per-rate substitute where it does not (the outgoing one too).
    let rates = super::cpu_budget::measured();
    let played = |c: &EngineConfig| {
        if ratio.is_integer() {
            // No rational cost: nothing to select.
            return c.clone();
        }
        super::cpu_budget::played_at(rates, c, None, graph_rate, ratio).into_owned()
    };
    let cfg = &played(cfg);
    let new = rational_side_cost(cfg, format, graph_rate, ratio) * sides(cfg);
    let old = outgoing.map_or(new, |o| {
        let o = played(o);
        rational_side_cost(&o, format, graph_rate, ratio) * sides(&o)
    });
    let chains = new + new.max(old);
    if stage + chains > MAX_RATIONAL_MACS_PER_SECOND {
        return Err(format!(
            "{what} at {graph_rate} Hz needs {:.0} M multiply-adds per second per channel {at}, \
             and the chains' {ratio} resampling {:.0} M with the pipelines that can run at once: \
             {:.0} M, more than the {:.0} M budget; use a shorter filter or a lower output rate",
            stage / 1e6,
            chains / 1e6,
            (stage + chains) / 1e6,
            MAX_RATIONAL_MACS_PER_SECOND / 1e6
        ));
    }
    Ok(())
}

/// The most expensive side's rational cost (multiply-adds per second and
/// channel) of `cfg` at `ratio` from `graph_rate`: 0 with DSP off or at an
/// integer ratio. With `format`, the design that format builds.
fn rational_side_cost(
    cfg: &EngineConfig,
    format: Option<SampleFormat>,
    graph_rate: u32,
    ratio: audivane_protocol::RateRatio,
) -> f64 {
    if !cfg.dsp_enabled || ratio.is_integer() {
        return 0.0;
    }
    let Some(out) = ratio.output_rate(graph_rate) else {
        return 0.0;
    };
    let mut worst = 0.0_f64;
    for mut side in super::ab_side::designed_chains(cfg) {
        side.upsampler.target = RateTarget::Rate(out);
        let variants = match format {
            Some(f) => {
                let output = audivane_protocol::OutputConfig {
                    bits: audivane_protocol::effective_dither_bits(side.output.bits, Some(f)),
                    ..side.output
                };
                vec![resolve_upsampler_for_output(&side.upsampler, &output)]
            }
            None => upsampler_variants(&side),
        };
        for up in variants {
            if let Some(cost) = design_cost(&up, graph_rate, ratio) {
                worst = worst.max(cost);
            }
        }
    }
    worst
}

/// One rational design's cost, multiply-adds per second and channel
/// (`None`: it does not build): its [`design::budget::plan`] for one
/// pipeline, the direct polyphase sum over its grid taps (a blended
/// intermediate Custom phase at four times its design length).
fn design_cost(
    up: &audivane_protocol::UpsamplerConfig,
    graph_rate: u32,
    ratio: audivane_protocol::RateRatio,
) -> Option<f64> {
    let named = design::Named::of(up).ok()?;
    let plan = design::budget::plan(&named.design, &design_context(graph_rate, ratio, 1)).ok()?;
    Some(plan.macs_per_second)
}

/// The most taps per input sample (`taps_unit: "input"`) a manual design
/// may have at the non-integer `r` from `rate` within the rational length
/// and cost limits for `pipelines` pipelines (0 when not even one fits).
pub fn max_rational_taps_per_input(
    rate: u32,
    r: audivane_protocol::RateRatio,
    pipelines: usize,
    intermediate: bool,
) -> usize {
    use design::budget::MAX_RATIONAL_MACS_PER_SECOND;
    let growth = if intermediate {
        INTERMEDIATE_MAX_GROWTH
    } else {
        1.0
    };
    let by_size = design::budget::MAX_RATIONAL_GRID_TAPS as f64 / f64::from(r.up);
    let out_rate = f64::from(rate) * r.as_f64();
    let by_cost = MAX_RATIONAL_MACS_PER_SECOND / (out_rate * pipelines.max(1) as f64);
    (by_size.min(by_cost) / growth).floor().min(MAX_TAPS as f64) as usize
}

/// The rate pairs every upsampler `cfg` runs can be built for: chain A and,
/// with A/B, chain B ([`buildable_rate_pairs`] with the pipelines the config
/// runs, intersected). This is what the ratio ladders follow.
pub fn engine_buildable_rate_pairs(cfg: &EngineConfig) -> audivane_io_pw::RatePairMask {
    let pipelines = rational_pipelines(cfg.ab.is_some());
    let mut mask = audivane_io_pw::RatePairMask::ALL;
    let mut seen: Vec<UpsamplerConfig> = Vec::new();
    for (i, chain) in super::ab_side::designed_chains(cfg).into_iter().enumerate() {
        // Every budget `match_output` can resolve to on the device.
        for up in upsampler_variants(&chain) {
            if !seen.contains(&up) {
                let mut own = buildable_rate_pairs(&up, pipelines);
                // Chain A's custom design plays Recommended from a rate it
                // does not build at: the ladder follows Recommended there.
                // Recommended as it builds (its `match_output` budget
                // resolved for every output the device may open in), only
                // needed where the design reaches no pair.
                if i == 0 && falls_back_per_rate(&up) {
                    let reach = reachable_rate_pairs(up.target);
                    let reaches = |input: u32, mask: audivane_io_pw::RatePairMask| {
                        audivane_io_pw::lifecycle::MASK_OUTPUT_RATES
                            .iter()
                            .any(|&output| {
                                output != input
                                    && reach.allows(input, output)
                                    && mask.allows(input, output)
                            })
                    };
                    let stranded = ALLOWED_RATES.iter().any(|&input| {
                        reaches(input, audivane_io_pw::RatePairMask::ALL) && !reaches(input, own)
                    });
                    if stranded {
                        let rec = ChainConfig {
                            upsampler: recommended_upsampler(&up),
                            ..chain.clone()
                        };
                        let rec = upsampler_variants(&rec)
                            .iter()
                            .fold(audivane_io_pw::RatePairMask::ALL, |m, v| {
                                m.intersect(buildable_rate_pairs(v, pipelines))
                            });
                        own = own.or_where_none(rec, reach);
                    }
                }
                mask = mask.intersect(own);
                seen.push(up);
            }
        }
    }
    mask
}

/// Recommended at `up`'s target: what a custom design plays at a rate it
/// does not build at ([`falls_back_per_rate`]).
pub fn recommended_upsampler(up: &UpsamplerConfig) -> UpsamplerConfig {
    UpsamplerConfig {
        target: up.target,
        preset: Some(catalogue::RECOMMENDED.to_string()),
        design: None,
    }
}

/// Whether chain A's upsampler in `cfg` builds from `rate` at `r`, for a
/// custom design ([`falls_back_per_rate`]); `Ok` at x1, with DSP or the
/// upsampler off, or for a preset (checked by the variant validation).
/// [`chain_builds_at`] does the check.
pub fn chain_a_builds_at(cfg: &EngineConfig, rate: u32, r: RateRatio) -> Result<(), ChainAFailure> {
    if r.is_unity() || !cfg.dsp_enabled || !falls_back_per_rate(&cfg.chain.upsampler) {
        return Ok(());
    }
    chain_builds_at(cfg, rate, r)
}

/// Whether chain A's upsampler in `cfg`, whatever it is, builds from `rate`
/// at `r` (every form `match_output` resolves to), with the checks the
/// config validation applies: the design's refusal there as
/// [`design::searched_feasibility`] gives it (an automatic length
/// searched), a kept-samples design at a decimating ratio, and at a
/// rational ratio the rational check ([`check_rational_at`]).
pub fn chain_builds_at(cfg: &EngineConfig, rate: u32, r: RateRatio) -> Result<(), ChainAFailure> {
    if r.is_unity() || cfg.chain.upsampler.target == RateTarget::Off {
        return Ok(());
    }
    let pipelines = rational_pipelines(cfg.ab.is_some());
    for v in upsampler_variants(&cfg.chain) {
        let named = design::Named::of(&v).map_err(|e| ChainAFailure::Message(e.to_string()))?;
        let d = &named.design;
        if matches!(d, FilterDesign::Kernel(k) if k.preserve_samples) && r.is_decimating() {
            return Err(ChainAFailure::Design(design::DesignError::Ratio {
                ratio: r,
                need: "an output rate at or above the input rate (it keeps the original samples)",
            }));
        }
        nyquist_stop_at(d, r).map_err(ChainAFailure::Message)?;
        let cx = design_context(rate, r, pipelines);
        if let Err(e) = design::searched_feasibility(d, &cx) {
            return Err(ChainAFailure::Design(e));
        }
        if !r.is_integer() {
            check_rational_at("upsampler", &v, rate, r, pipelines)
                .map_err(ChainAFailure::Message)?;
        }
    }
    Ok(())
}

/// Stop at Nyquist's calibrated range at the ratio `r` (on its design
/// grid), as [`validate_upsampler_variants`] checks it at every rate.
fn nyquist_stop_at(d: &FilterDesign, r: RateRatio) -> Result<(), String> {
    let FilterDesign::Kernel(k) = d else {
        return Ok(());
    };
    if !matches!(
        k.shape,
        Shape::WindowedSinc {
            cutoff: audivane_protocol::design::Cutoff::NyquistStop,
            ..
        }
    ) {
        return Ok(());
    }
    let Ok(manual) = design::paths::standard::legacy_config(k, ALLOWED_RATES[0]) else {
        return Ok(());
    };
    let grid_ratio = design::paths::standard::design_ratio(r);
    let taps = match manual.taps_unit {
        audivane_protocol::TapsUnit::Output => manual.taps.saturating_mul(r.down as usize),
        audivane_protocol::TapsUnit::Input => manual.taps.saturating_mul(r.up as usize),
    };
    audivane_dsp_core::resample::nyquist_stop_check_for(
        manual.window,
        taps,
        grid_ratio,
        manual.kaiser_beta,
    )
    .map(|_| ())
    .map_err(|e| format!("design.cutoff: {e}"))
}

/// Whether `d` builds from `rate` at `r` as chain A's custom design of
/// `cfg` (its target, output and pipelines): [`chain_builds_at`]. What a
/// one-click fix is held to before it is offered.
pub fn design_builds_at(cfg: &EngineConfig, d: &FilterDesign, rate: u32, r: RateRatio) -> bool {
    let mut c = cfg.clone();
    c.chain.upsampler = UpsamplerConfig {
        target: c.chain.upsampler.target,
        preset: None,
        design: Some(StoredDesign::new(d.clone())),
    };
    chain_builds_at(&c, rate, r).is_ok()
}

/// Why [`chain_a_builds_at`] says no.
#[derive(Debug)]
pub enum ChainAFailure {
    Design(design::DesignError),
    Message(String),
}

impl ChainAFailure {
    /// As a design error, for [`design::explain`].
    pub fn into_design_error(self) -> design::DesignError {
        match self {
            ChainAFailure::Design(e) => e,
            ChainAFailure::Message(m) => design::DesignError::Failed(m),
        }
    }
}

/// A new install's config (no config file yet): the defaults, upsampling
/// off with the Recommended filter picked for when it is turned on.
pub fn first_run_config() -> EngineConfig {
    let mut cfg = EngineConfig::default();
    cfg.chain.upsampler.preset = Some(catalogue::RECOMMENDED.to_string());
    cfg
}

/// The design the engine bypass resampler runs (Direct and DSP off at a
/// rate the DAC does not play natively, `rate.bypass_resampler: engine`):
/// the `recommended` catalogue design. No target: the chain is built for the
/// ratio the device opened at.
pub fn bypass_upsampler() -> UpsamplerConfig {
    UpsamplerConfig {
        preset: Some(catalogue::RECOMMENDED.to_string()),
        ..UpsamplerConfig::default()
    }
}

/// Resolve an output-dependent `match_output` budget in `up` (whether explicit
/// in `up.design` or implied by `recommended`) against `output`.
pub fn resolve_upsampler_for_output(
    up: &UpsamplerConfig,
    output: &OutputConfig,
) -> UpsamplerConfig {
    if let Ok(named) = design::Named::of(up)
        && named.design.has_match_output()
    {
        return UpsamplerConfig {
            target: up.target,
            preset: None,
            design: Some(StoredDesign {
                based_on: up
                    .design
                    .as_ref()
                    .and_then(|d| d.based_on.clone())
                    .or_else(|| up.preset.clone()),
                design: named.design.with_output(output),
            }),
        };
    }
    up.with_output(output)
}

/// The output section the engine bypass resampler uses: the chain's,
/// without its headroom (a DSP-chain setting; Direct and DSP off never
/// take it).
pub fn bypass_output(output: OutputConfig) -> OutputConfig {
    OutputConfig {
        headroom_db: 0.0,
        ..output
    }
}

/// The chain the engine bypass resampler plays from `graph_rate` at
/// `ratio`: the [`bypass_upsampler`] alone, then the digital volume and
/// dither to `output` (the chain's output section, capped at the format,
/// without headroom: [`bypass_output`]). Nothing else of the DSP chain
/// runs.
pub fn bypass_chain_config(output: OutputConfig, graph_rate: u32, ratio: RateRatio) -> ChainConfig {
    let output = bypass_output(output);
    let mut upsampler = bypass_upsampler();
    upsampler.target = if let Some(k) = ratio.as_integer() {
        RateTarget::Ratio(k)
    } else {
        RateTarget::Rate(ratio.output_rate(graph_rate).unwrap_or(graph_rate))
    };
    ChainConfig {
        upsampler,
        output,
        ..ChainConfig::neutral()
    }
}

/// Whether the resample-only chain fits from `input` to `output` for a chain
/// output of `output_cfg` ([`bypass_chain_config`]): at a non-integer ratio,
/// the calibrated band length at the output's error budget within the
/// rational runtime's length and cost limits for one pipeline and its swap.
/// A formula, not the automatic design (designing every pair would take
/// seconds); the chain build runs the exact check ([`check_bypass_design`])
/// and a pair that fails there falls back to PipeWire at run time. Integer
/// ratios always fit.
fn bypass_pair_builds(output_cfg: &OutputConfig, input: u32, output: u32) -> bool {
    let r = RateRatio::from_rates(input, output);
    if r.is_integer() {
        return true;
    }
    // Recommended's band (a passband end in Hz converted at the lower
    // rate, as the build does), at the output's budget.
    let Some(rec) = design::band_of(&catalogue::recommended()).copied() else {
        return false;
    };
    let band = audivane_protocol::BandSpec {
        passband_end: design::band_passband_end(&rec, input.min(output)),
        passband_end_hz: None,
        attenuation_db: audivane_protocol::filter_error_budget_db(output_cfg),
        ..rec
    };
    band_design_for(
        audivane_protocol::WindowKind::Kaiser,
        &band,
        design::paths::standard::design_ratio(r),
    )
    .is_ok_and(|d| {
        design::budget::check_rational(
            d.taps,
            r,
            input,
            design::budget::MAX_RATIONAL_GRID_TAPS,
            rational_pipelines(false),
        )
        .is_ok()
    })
}

/// The exact check of the engine bypass resampler's design from `input` at
/// `r` for a chain output of `output_cfg` (the automatic design, cached,
/// and the rational limits for one pipeline and its swap; integer ratios:
/// the design). Run when its chain is built.
pub fn check_bypass_design(
    output_cfg: &OutputConfig,
    input: u32,
    r: RateRatio,
) -> Result<(), String> {
    let up = resolve_upsampler_for_output(&bypass_upsampler(), output_cfg);
    if r.is_integer() {
        return custom_feasibility_at(
            "bypass resampler",
            &up,
            input,
            r,
            rational_pipelines(false),
            true,
        );
    }
    check_rational_at("bypass resampler", &up, input, r, rational_pipelines(false))
}

/// The rate pairs the engine bypass resampler's chain can be built for: graph
/// rates to engine rates (a native rate the graph could move to counts), for
/// every output the chain can run with ([`runtime_outputs`]). The bypass mask:
/// a pair outside it falls back to PipeWire. Empty with
/// `rate.bypass_resampler: pipewire`. The designs are cached, as the mask only
/// depends on the output section.
pub fn bypass_buildable_rate_pairs(cfg: &EngineConfig) -> audivane_io_pw::RatePairMask {
    use std::sync::{Mutex, OnceLock};
    type Cache = Mutex<Vec<(OutputConfig, audivane_io_pw::RatePairMask)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    if cfg.rate.bypass_resampler != audivane_protocol::ipc::BypassResampler::Engine {
        return audivane_io_pw::RatePairMask::NONE;
    }
    let output = cfg.chain.output;
    let compute = || {
        let outputs = runtime_outputs(&output);
        audivane_io_pw::RatePairMask::from_fn(|input, out| {
            ALLOWED_RATES.contains(&out)
                && outputs.iter().all(|o| bypass_pair_builds(o, input, out))
        })
    };
    // Held while computing, so concurrent callers design it once.
    let Ok(mut cache) = CACHE.get_or_init(|| Mutex::new(Vec::new())).lock() else {
        return compute();
    };
    if let Some((_, mask)) = cache.iter().find(|(o, _)| *o == output) {
        return *mask;
    }
    let mask = compute();
    cache.push((output, mask));
    mask
}

/// The upsampler of `chain` as the engine builds it: resolved against
/// each output the chain can run with ([`runtime_outputs`]) when its
/// automatic budget follows the output, else as configured.
pub fn upsampler_variants(chain: &ChainConfig) -> Vec<UpsamplerConfig> {
    if design::Named::of(&chain.upsampler).is_ok_and(|n| n.design.has_match_output()) {
        runtime_outputs(&chain.output)
            .iter()
            .map(|o| resolve_upsampler_for_output(&chain.upsampler, o))
            .collect()
    } else {
        vec![chain.upsampler.clone()]
    }
}

/// The `(input, output)` pairs `target`'s ladder can land at (x1 aside):
/// its integer rungs, and the fractional outputs it reaches.
pub fn reachable_rate_pairs(target: RateTarget) -> audivane_io_pw::RatePairMask {
    let term = audivane_io_pw::lifecycle::MAX_RATIONAL_TERM;
    audivane_io_pw::RatePairMask::from_fn(|input, output| {
        let r = audivane_protocol::RateRatio::from_rates(input, output);
        match (target, r.as_integer()) {
            (RateTarget::Off, _) => false,
            (RateTarget::Rate(rate), _) => output == rate,
            (RateTarget::Ratio(k), _) => u64::from(output) == u64::from(input) * u64::from(k),
            (RateTarget::MaxRate(cap), Some(_)) | (RateTarget::MaxRateAny(cap), Some(_)) => {
                output <= cap
            }
            (RateTarget::MaxRateAny(cap), None) => output <= cap && r.up <= term && r.down <= term,
            (RateTarget::MaxRate(_), None) => false,
        }
    })
}

/// The `(input, output)` rate pairs `up` can be built for, for the ratio
/// ladders.
pub fn buildable_rate_pairs(
    up: &UpsamplerConfig,
    pipelines: usize,
) -> audivane_io_pw::RatePairMask {
    let reachable = |output: u32| match up.target {
        RateTarget::Rate(rate) => output == rate,
        RateTarget::MaxRateAny(cap) => output <= cap,
        RateTarget::Off | RateTarget::Ratio(_) | RateTarget::MaxRate(_) => false,
    };
    // The integer rungs the target can pick.
    let rung = |input: u32, output: u32| match up.target {
        RateTarget::Rate(rate) => output == rate,
        RateTarget::Ratio(k) => u64::from(output) == u64::from(input) * u64::from(k),
        RateTarget::MaxRate(cap) | RateTarget::MaxRateAny(cap) => output <= cap,
        RateTarget::Off => false,
    };
    let term = audivane_io_pw::lifecycle::MAX_RATIONAL_TERM;
    let Ok(named) = design::Named::of(up) else {
        return audivane_io_pw::RatePairMask::NONE;
    };
    let pair_builds = |input: u32, output: u32| {
        let r = audivane_protocol::RateRatio::from_rates(input, output);
        match (r.as_integer(), named.preset) {
            (Some(k), Some(_)) => design::feasibility(
                &named.design,
                &design_context(input, RateRatio::integer(k), pipelines),
            )
            .is_ok(),
            (Some(k), None) => {
                let r = RateRatio::integer(k);
                let searched = rung(input, output);
                custom_feasibility_at("upsampler", up, input, r, pipelines, searched).is_ok()
            }
            (None, _) => {
                r.up <= term
                    && r.down <= term
                    && reachable(output)
                    && check_rational_at("upsampler", up, input, r, pipelines).is_ok()
            }
        }
    };
    if named.preset.is_some() {
        return audivane_io_pw::RatePairMask::from_fn(pair_builds);
    }
    // A custom design searches each pair's length: on the validation
    // workers, one pair per ratio first (the other source rates at that
    // ratio then find its prototypes designed).
    let mut first = Vec::new();
    let mut rest = Vec::new();
    for &input in ALLOWED_RATES {
        for output in audivane_io_pw::lifecycle::MASK_OUTPUT_RATES {
            if input == output {
                continue;
            }
            let r = audivane_protocol::RateRatio::from_rates(input, output);
            if first.iter().any(|&(i, o)| RateRatio::from_rates(i, o) == r) {
                rest.push((input, output));
            } else {
                first.push((input, output));
            }
        }
    }
    let mut builds = std::collections::HashMap::new();
    for wave in [first, rest] {
        let results = run_parallel(&wave, |&(i, o)| pair_builds(i, o));
        for (&(i, o), ok) in wave.iter().zip(results) {
            builds.insert((i, o), ok.unwrap_or_else(|| pair_builds(i, o)));
        }
    }
    audivane_io_pw::RatePairMask::from_fn(|input, output| {
        builds.get(&(input, output)) == Some(&true)
    })
}

/// Largest integer upsampling ratio `target` resolves to over
/// [`ALLOWED_RATES`] (1 when it resolves to none).
pub fn worst_case_ratio(target: RateTarget) -> u32 {
    ALLOWED_RATES
        .iter()
        .filter_map(|&rate| resolve_rate_ratio(target, rate).as_integer())
        .max()
        .unwrap_or(1)
}

/// The probe's view for `cfg`'s output pair and formats
/// ([`audivane_protocol::ipc::DeviceCapabilities::for_output`]): the
/// channel count the engine would open the device at for that pair.
pub fn caps_view(
    cfg: &EngineConfig,
    caps: &audivane_protocol::ipc::DeviceCapabilities,
) -> audivane_protocol::ipc::DeviceCapabilities {
    let min = cfg
        .output_channels
        .map_or(2, |p| p.min_device_channels().max(2));
    caps.for_output(min, &cfg.formats)
}

/// The source rates in [`ALLOWED_RATES`] at which `target` resolves to a
/// non-integer (fractional or decimating) ratio, with that ratio.
pub fn rational_ratios(target: RateTarget) -> Vec<(u32, audivane_protocol::RateRatio)> {
    ALLOWED_RATES
        .iter()
        .map(|&rate| (rate, resolve_rate_ratio(target, rate)))
        .filter(|(_, r)| !r.is_integer())
        .collect()
}

/// Check an explicit output rate against what the open device reports: it must
/// play that rate in one of the configured formats (the check ALSA's open plan
/// makes, [`audivane_io_alsa::plan_open`]; empty formats mean the driver's
/// preference). `caps` is the probe of the device the engine has open (`None`:
/// anything [`validate`] accepts passes).
pub fn validate_against_device(
    cfg: &EngineConfig,
    caps: Option<&audivane_protocol::ipc::DeviceCapabilities>,
) -> Result<(), String> {
    let Some(caps) = caps.filter(|c| c.device == cfg.device) else {
        return Ok(());
    };
    // What `cfg`'s own output pair plays, not the pair the probe was
    // viewed for: a return to a pair with more rates is never refused.
    let caps = &caps_view(cfg, caps);
    let formats: &[audivane_protocol::SampleFormat] = if cfg.formats.is_empty() {
        &audivane_io_alsa::FORMAT_PREFERENCE
    } else {
        &cfg.formats
    };
    let plays = |rate: u32| formats.iter().any(|&f| caps.supports(rate, f));
    for (field, chain) in std::iter::once(("chain", &cfg.chain))
        .chain(cfg.ab.as_ref().map(|ab| ("ab.chain_b", &ab.chain_b)))
    {
        let RateTarget::Rate(rate) = chain.upsampler.target else {
            continue;
        };
        if plays(rate) {
            continue;
        }
        let names = |fs: &[audivane_protocol::SampleFormat]| {
            fs.iter()
                .map(|f| format!("{f:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if let Some(at) = caps
            .rates
            .iter()
            .find(|c| c.rate == rate && !c.formats.is_empty())
        {
            let there: Vec<_> = at.formats.iter().map(|f| f.format).collect();
            return Err(format!(
                "{field}.upsampler.target: {} plays {rate} Hz only in {}, not in the \
                 configured formats ({}): add one of those to formats, or pick another rate",
                caps.device,
                names(&there),
                names(formats)
            ));
        }
        let list: Vec<String> = caps.rates.iter().map(|c| c.rate.to_string()).collect();
        return Err(format!(
            "{field}.upsampler.target: {} does not play {rate} Hz (it supports {} Hz)",
            caps.device,
            list.join(", ")
        ));
    }
    Ok(())
}

/// Automatic designs run at most this many at a time during validation.
const AUTO_VALIDATION_THREADS: usize = 4;

/// Reject an automatic-taps config here rather than with a chain error after
/// a later rate or format change: for every budget it can resolve to and every
/// ratio it can run at ([`design_ratios`]), first the cheap length estimate,
/// then the design itself (in parallel). Designs are cached per process, so
/// the chain builds that follow and the rate-pair mask
/// ([`buildable_rate_pairs`]) reuse them.
///
/// Integer ratios must design. Rational ones are designed here to warm the
/// cache; the ones the target resolves to at the allowed rates are cost-checked
/// by the caller ([`check_rational_at`]), and pairs only a `max_rate_any`
/// ladder can pick stay off it when they do not design (the mask runs the same
/// check).
fn validate_automatic(
    field: &str,
    variants: &[UpsamplerConfig],
    pipelines: usize,
) -> Result<(), String> {
    use audivane_dsp_core::filter_design::{
        automatic_design_for, automatic_estimate_for, automatic_target,
    };
    let mut standard_variants: Vec<(UpsamplerConfig, ManualConfig)> = Vec::new();
    let mut novel_variants: Vec<(&UpsamplerConfig, FilterDesign)> = Vec::new();
    for up in variants {
        let named = design::Named::of(up).map_err(|e| format!("{field}: {e}"))?;
        if let FilterDesign::Kernel(k) = &named.design
            && k.numerics.path == design::ArithmeticPath::Standard
            && let Ok(manual) = design::paths::standard::legacy_config(k, ALLOWED_RATES[0])
        {
            standard_variants.push((up.clone(), manual));
        } else {
            novel_variants.push((up, named.design));
        }
    }
    // Novel (step 4) automatic designs: check via searched_feasibility, the
    // ratios in parallel (longest first), the smallest refused reported.
    let mut novel_jobs: Vec<(&FilterDesign, audivane_protocol::RateRatio)> = novel_variants
        .iter()
        .flat_map(|(up, d)| {
            design_ratios(up.target)
                .into_iter()
                .filter(|r| r.is_integer())
                .map(move |r| (d, r))
        })
        .collect();
    novel_jobs.sort_by_key(|&(_, r)| std::cmp::Reverse(r.up));
    let results = run_parallel(&novel_jobs, |&(d, r)| {
        design::searched_feasibility(d, &design_context(ALLOWED_RATES[0], r, pipelines)).map(|_| ())
    });
    for (&(d, r), result) in novel_jobs.iter().zip(results).rev() {
        match result {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(refused_at(field, d, &r.to_string(), &e)),
            None => return Err(format!("{field}: the design check did not complete")),
        }
    }
    if standard_variants.is_empty() {
        return Ok(());
    }
    let mut jobs = Vec::new();
    for (idx, (up, manual)) in standard_variants.iter().enumerate() {
        let a = automatic_target(manual).map_err(|e| format!("{field}.design.length: {e}"))?;
        for r in design_ratios(up.target) {
            if r.is_integer() {
                automatic_estimate_for(manual, r)
                    .map_err(|e| format!("{field}.design.length: {e}"))?;
            } else if automatic_estimate_for(manual, r).is_err() {
                continue;
            }
            jobs.push((idx, r, a));
        }
    }
    // Integer ratios first, longest designs first, so the total is close
    // to the slowest single design. They must all design; the smallest
    // failing ratio is reported before anything rational runs.
    let (mut integer, rational): (Vec<_>, Vec<_>) =
        jobs.into_iter().partition(|&(_, r, _)| r.is_integer());
    integer.sort_by_key(|&(_, r, _)| std::cmp::Reverse(r.up));
    let results = run_parallel(&integer, |&(idx, r, _)| {
        automatic_design_for(&standard_variants[idx].1, r).map(|_| ())
    });
    for (&(_, r, a), result) in integer.iter().zip(results).rev() {
        result
            .unwrap_or_else(|| Err("design did not complete".into()))
            .map_err(|e| format!("{field}.design.length: at {r} ({a:.2} dB): {e}"))?;
    }

    // A rational ratio is designed through check_rational_at from the
    // lowest engine rate it occurs at (its cheapest pair), which refuses a
    // design over the size or cost limits from its lower bound before the
    // real-grid check of a minimum or intermediate phase design runs.
    // Failures are not errors here (see above).
    let cheapest_input = |r: RateRatio| {
        ALLOWED_RATES.iter().copied().find(|&input| {
            r.output_rate(input)
                .is_some_and(|out| audivane_io_pw::lifecycle::MASK_OUTPUT_RATES.contains(&out))
        })
    };
    let mut rational: Vec<_> = rational
        .into_iter()
        .filter_map(|(idx, r, _)| cheapest_input(r).map(|input| (idx, r, input)))
        .collect();
    // One ratio per grid scale goes first (ratios of one scale share the
    // real-grid check, which would otherwise run several times at once),
    // the cheapest per output frame first; the rest follow, mostly cache
    // hits.
    let cost =
        |&(_, r, input): &(usize, RateRatio, u32)| u64::from(input) * 4096 / u64::from(r.down);
    rational.sort_by_key(|job| (design::paths::standard::design_ratio(job.1), cost(job)));
    let mut firsts = Vec::new();
    let mut rest = Vec::new();
    for job in rational {
        let scale = design::paths::standard::design_ratio(job.1);
        let seen = firsts.iter().any(|f: &(usize, RateRatio, u32)| {
            f.0 == job.0 && design::paths::standard::design_ratio(f.1) == scale
        });
        if seen {
            rest.push(job)
        } else {
            firsts.push(job)
        }
    }
    // Largest scales (the longest real-grid checks) first.
    firsts.reverse();
    for wave in [firsts, rest] {
        run_parallel(&wave, |&(idx, r, input)| {
            check_rational_at(field, &standard_variants[idx].0, input, r, pipelines)
        });
    }
    Ok(())
}

/// Run `f` on every job on up to [`AUTO_VALIDATION_THREADS`] workers (the
/// calling thread is one of them; if the OS refuses more threads it does
/// more of the work), returning the results in job order (`None` for a
/// job that did not complete).
fn run_parallel<T: Sync, R: Send>(jobs: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<Option<R>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<R>>> =
        jobs.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let work = || {
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(job) = jobs.get(i) else {
                break;
            };
            let result = f(job);
            if let Ok(mut slot) = results[i].lock() {
                *slot = Some(result);
            }
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..AUTO_VALIDATION_THREADS.min(jobs.len()) {
            if std::thread::Builder::new()
                .name("auto-taps-validate".into())
                .spawn_scoped(scope, work)
                .is_err()
            {
                break;
            }
        }
        work();
    });
    results
        .into_iter()
        .map(|slot| slot.into_inner().ok().flatten())
        .collect()
}

/// Every ratio other than 1 the engine can build a chain with `target` at: the
/// ratio it asks for at each of [`ALLOWED_RATES`]
/// ([`crate::engine::engine_ratio`], clamped to the engine's limit) and, below
/// each, the ratios the output falls back to when the device lacks that rate
/// ([`audivane_io_alsa::ratio_ladder`]) and that land on a rate the driver can
/// open ([`audivane_io_pw::lifecycle::MASK_OUTPUT_RATES`]: 44.1 kHz x48 falls
/// back to x32 without stopping at x47 .. x33, which are no device rate); for
/// [`RateTarget::MaxRateAny`] also every fractional or decimating pair a device
/// can offer its ladder. Integer ratios first, ascending, then the rational
/// ones; unique.
pub fn design_ratios(target: RateTarget) -> Vec<RateRatio> {
    let opens = |rate: u32, r: RateRatio| {
        r.output_rate(rate)
            .is_some_and(|out| audivane_io_pw::lifecycle::MASK_OUTPUT_RATES.contains(&out))
    };
    let mut ratios: Vec<RateRatio> = ALLOWED_RATES
        .iter()
        .flat_map(|&rate| {
            let desired = crate::engine::engine_ratio(target, rate);
            audivane_io_alsa::ratio_ladder(desired)
                .into_iter()
                .filter(move |&r| r == desired || opens(rate, r))
        })
        .filter(|r| !r.is_unity())
        .collect();
    if let RateTarget::MaxRateAny(cap) = target {
        let limit = audivane_io_pw::lifecycle::MAX_RATIONAL_TERM;
        for &input in ALLOWED_RATES {
            for output in audivane_io_pw::lifecycle::MASK_OUTPUT_RATES {
                let r = RateRatio::from_rates(input, output);
                if output <= cap.min(audivane_protocol::MAX_OUTPUT_RATE)
                    && !r.is_integer()
                    && r.up <= limit
                    && r.down <= limit
                {
                    ratios.push(r);
                }
            }
        }
    }
    ratios.sort_unstable_by_key(|r| (!r.is_integer(), r.up, r.down));
    ratios.dedup();
    ratios
}

/// The outputs a chain can run with: the configured dither and its bits
/// capped to each sample container (S16, S24, S32), as the engine does
/// before building. Unique, in container order.
pub fn runtime_outputs(output: &OutputConfig) -> Vec<OutputConfig> {
    let mut outputs: Vec<OutputConfig> = Vec::new();
    for format in [SampleFormat::S16, SampleFormat::S24, SampleFormat::S32] {
        let o = OutputConfig {
            bits: audivane_protocol::effective_dither_bits(output.bits, Some(format)),
            ..*output
        };
        if !outputs.contains(&o) {
            outputs.push(o);
        }
    }
    outputs
}

/// A convolution item's filter matrix: its shape
/// ([`audivane_dsp_core::matrix::check_item_shape`], the rule `avdsp
/// process` applies too) and, when enabled, every file present.
fn validate_conv_matrix(
    field: &str,
    conv: &audivane_protocol::ConvolutionConfig,
    matrix: &[audivane_protocol::MatrixEntry],
) -> Result<(), String> {
    audivane_dsp_core::matrix::check_item_shape(conv, crate::engine::DEFAULT_CHANNELS)
        .map_err(|e| format!("{field}{e}"))?;
    if conv.enabled {
        for (k, e) in matrix.iter().enumerate() {
            let at = format!("{field}.matrix[{k}]");
            if e.ir_path.as_os_str().is_empty() {
                return Err(format!("{at}.ir_path must not be empty"));
            }
            if !e.ir_path.is_file() {
                return Err(format!(
                    "{at}.ir_path does not exist or is not a file: {}",
                    e.ir_path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Validate a pitch-shift configuration.
pub fn validate_pitch(field: &str, pitch: &PitchConfig) -> Result<(), String> {
    if !pitch.cents.is_finite() || !(-1200.0..=1200.0).contains(&pitch.cents) {
        return Err(format!(
            "{field}.cents must be finite and in -1200..=1200 (got {})",
            pitch.cents
        ));
    }
    if let Some(st) = pitch.semitones
        && (!st.is_finite() || !(-12.0..=12.0).contains(&st))
    {
        return Err(format!(
            "{field}.semitones must be finite and in -12..=12 (got {st})"
        ));
    }
    match (pitch.reference_hz, pitch.target_hz) {
        (Some(r), Some(t)) => {
            if !r.is_finite() || r <= 0.0 {
                return Err(format!("{field}.reference_hz must be finite and > 0"));
            }
            if !t.is_finite() || t <= 0.0 {
                return Err(format!("{field}.target_hz must be finite and > 0"));
            }
            let derived = 1200.0 * (t / r).log2();
            if !(-1200.0..=1200.0).contains(&derived) {
                return Err(format!(
                    "{field} derived shift ({derived:.2} cents) must be in -1200..=1200"
                ));
            }
        }
        (None, None) => {}
        _ => {
            return Err(format!(
                "{field}.reference_hz and {field}.target_hz must both be set"
            ));
        }
    }
    let count = usize::from(pitch.cents != 0.0)
        + usize::from(pitch.semitones.is_some())
        + usize::from(pitch.reference_hz.is_some());
    if count > 1 {
        return Err(format!(
            "{field}: specify at most one of cents, semitones, or reference_hz/target_hz"
        ));
    }
    Ok(())
}

/// Validate DC-blocking high-pass filter parameters.
pub fn validate_dc_block(field: &str, dc: &DcBlockConfig) -> Result<(), String> {
    if !dc.cutoff_hz.is_finite()
        || !(DC_BLOCK_CUTOFF_MIN_HZ..=DC_BLOCK_CUTOFF_MAX_HZ).contains(&dc.cutoff_hz)
    {
        return Err(format!(
            "{field}.cutoff_hz must be between {DC_BLOCK_CUTOFF_MIN_HZ} and {DC_BLOCK_CUTOFF_MAX_HZ} Hz (got {})",
            dc.cutoff_hz
        ));
    }
    Ok(())
}

/// Validate crossfeed parameters.
pub fn validate_crossfeed(field: &str, cf: &CrossfeedConfig) -> Result<(), String> {
    if !cf.cutoff_hz.is_finite()
        || !(CROSSFEED_CUTOFF_MIN_HZ..=CROSSFEED_CUTOFF_MAX_HZ).contains(&cf.cutoff_hz)
    {
        return Err(format!(
            "{field}.cutoff_hz must be between {CROSSFEED_CUTOFF_MIN_HZ} and {CROSSFEED_CUTOFF_MAX_HZ} Hz (got {})",
            cf.cutoff_hz
        ));
    }
    if !cf.feed_db.is_finite()
        || !(CROSSFEED_FEED_MIN_DB..=CROSSFEED_FEED_MAX_DB).contains(&cf.feed_db)
    {
        return Err(format!(
            "{field}.feed_db must be between {CROSSFEED_FEED_MIN_DB} and {CROSSFEED_FEED_MAX_DB} dB (got {})",
            cf.feed_db
        ));
    }
    if !cf.width.is_finite() || !(STEREO_WIDTH_MIN..=STEREO_WIDTH_MAX).contains(&cf.width) {
        return Err(format!(
            "{field}.width must be between {STEREO_WIDTH_MIN} and {STEREO_WIDTH_MAX} (got {})",
            cf.width
        ));
    }
    if !cf.width_corner_hz.is_finite()
        || !(STEREO_WIDTH_CORNER_MIN_HZ..=STEREO_WIDTH_CORNER_MAX_HZ).contains(&cf.width_corner_hz)
    {
        return Err(format!(
            "{field}.width_corner_hz must be between {STEREO_WIDTH_CORNER_MIN_HZ} and {STEREO_WIDTH_CORNER_MAX_HZ} Hz (got {})",
            cf.width_corner_hz
        ));
    }
    Ok(())
}

/// Validate harmonic distortion parameters: at most
/// [`HARMONICS_MAX_LAYERS`] layers, each checked by its kind
/// ([`audivane_dsp_core::harmonics::validate_layer`]: every layer, enabled
/// or not; flux and hysteresis layers are also calibrated once at 96 kHz,
/// so a target they cannot reach fails here rather than when the chain
/// builds). Series routing needs no check of its own: its peak bound is
/// composed from each layer's certified one (each layer scaled by its own
/// trim), so a series config is valid wherever its layers are.
pub fn validate_harmonics(field: &str, h: &HarmonicsConfig) -> Result<(), String> {
    if h.layers.len() > HARMONICS_MAX_LAYERS {
        return Err(format!(
            "{field}.layers holds at most {HARMONICS_MAX_LAYERS} layers (got {})",
            h.layers.len()
        ));
    }
    for (i, layer) in h.layers.iter().enumerate() {
        audivane_dsp_core::harmonics::validate_layer(&format!("{field}.layers[{i}]"), layer)?;
    }
    Ok(())
}

/// Validate a single DSP chain. `field` prefixes messages for nested configs.
/// Rational upsampler ratios are checked at every engine rate (see
/// [`validate_chain_at`]).
pub fn validate_chain(field: &str, chain: &ChainConfig) -> Result<(), String> {
    validate_chain_at(field, chain, ALLOWED_RATES, rational_pipelines(false))
}

/// [`validate_chain`], checking rational upsampler ratios only at the source
/// rates in `rates` ([`validate_upsampler_at`]).
pub fn validate_chain_at(
    field: &str,
    chain: &ChainConfig,
    rates: &[u32],
    pipelines: usize,
) -> Result<(), String> {
    validate_chain_with(field, chain, rates, pipelines, None)
}

/// [`validate_chain_at`]; with `chain_a` (the config it is chain A of),
/// its custom design may fall back per rate ([`validate_chain_a_upsampler`]).
fn validate_chain_with(
    field: &str,
    chain: &ChainConfig,
    rates: &[u32],
    pipelines: usize,
    chain_a: Option<&EngineConfig>,
) -> Result<(), String> {
    if let Some(dc) = &chain.dc_block {
        validate_dc_block(&format!("{field}.dc_block"), dc)?;
    }
    if let Some(cf) = &chain.crossfeed {
        validate_crossfeed(&format!("{field}.crossfeed"), cf)?;
    }
    if let Some(harm) = &chain.harmonics {
        validate_harmonics(&format!("{field}.harmonics"), harm)?;
    }
    chain
        .limiter
        .validate()
        .map_err(|e| format!("{field}.{e}"))?;
    if let Some(eq) = &chain.eq {
        validate_eq_channel(&format!("{field}.eq"), eq.preamp_db, &eq.bands)?;
        if let Some(left) = &eq.left {
            validate_eq_channel(&format!("{field}.eq.left"), left.preamp_db, &left.bands)?;
        }
        if let Some(right) = &eq.right {
            validate_eq_channel(&format!("{field}.eq.right"), right.preamp_db, &right.bands)?;
        }
        if let Some(tilt) = &eq.tilt {
            validate_eq_tilt(&format!("{field}.eq.tilt"), tilt)?;
        }
        if let Some(high) = &eq.tilt_high {
            validate_eq_tilt(&format!("{field}.eq.tilt_high"), high)?;
        }
        check_tilt_sections(eq.tilt.as_ref(), eq.tilt_high.as_ref())
            .map_err(|e| format!("{field}.eq: {e}"))?;
        if let Some(tone) = &eq.tone {
            for (name, db) in [("bass_db", tone.bass_db), ("treble_db", tone.treble_db)] {
                if !db.is_finite() || db.abs() > TONE_MAX_DB {
                    return Err(format!(
                        "{field}.eq.tone.{name} ({db}) must be within ±{TONE_MAX_DB} dB"
                    ));
                }
            }
        }
    }
    for (i, conv) in chain.convolution.iter().enumerate() {
        if !conv.gain_db.is_finite() {
            return Err(format!("{field}.convolution[{i}].gain_db must be finite"));
        }
        if let Some(matrix) = &conv.matrix {
            validate_conv_matrix(&format!("{field}.convolution[{i}]"), conv, matrix)?;
        } else if conv.enabled {
            if conv.ir_path.as_os_str().is_empty() {
                return Err(format!(
                    "{field}.convolution[{i}].ir_path must not be empty"
                ));
            }
            if !conv.ir_path.is_file() {
                return Err(format!(
                    "{field}.convolution[{i}].ir_path does not exist or is not a file: {}",
                    conv.ir_path.display()
                ));
            }
        }
    }
    validate_pitch(&format!("{field}.pitch"), &chain.pitch)?;
    // Before the upsampler: a `match_output` budget resolves against it.
    if !(16..=32).contains(&chain.output.bits) {
        return Err(format!(
            "{field}.output.bits must be between 16 and 32 (got {})",
            chain.output.bits
        ));
    }
    let headroom = chain.output.headroom_db;
    if !headroom.is_finite() || !(0.0..=MAX_HEADROOM_DB).contains(&headroom) {
        return Err(format!(
            "{field}.output.headroom_db must be between 0 and {MAX_HEADROOM_DB} dB (got {headroom})"
        ));
    }
    let mut variants: Vec<UpsamplerConfig> = Vec::new();
    for up in upsampler_variants(chain) {
        if !variants.contains(&up) {
            variants.push(up);
        }
    }
    let field = format!("{field}.upsampler");
    if let Some(cfg) = chain_a {
        validate_chain_a_upsampler(&field, &variants, rates, pipelines, cfg)?;
    } else {
        validate_upsampler_variants(&field, &variants, rates, pipelines)?;
    }
    Ok(())
}

fn validate_ab(ab: &AbConfig, a: &ChainConfig, rate: &RatePolicy) -> Result<(), String> {
    // B's own bits (its quantizer); its filter is checked as built, with
    // A's bits and dither.
    if !(16..=32).contains(&ab.chain_b.output.bits) {
        return Err(format!(
            "ab.chain_b.output.bits must be between 16 and 32 (got {})",
            ab.chain_b.output.bits
        ));
    }
    validate_chain_at(
        "ab.chain_b",
        &super::ab_side::design_view(&ab.chain_b, a),
        &rate.allowed_rates,
        rational_pipelines(true),
    )?;
    if a.limiter != ab.chain_b.limiter {
        return Err("ab.chain_b.limiter must match chain A".into());
    }
    for &r in rate
        .allowed_rates
        .iter()
        .chain(std::iter::once(&rate.default_rate))
    {
        let ra = resolve_rate_ratio(a.upsampler.target, r);
        let rb = resolve_rate_ratio(ab.chain_b.upsampler.target, r);
        if ra != rb {
            return Err(format!(
                "ab.chain_b upsampler resolves to {rb} at {r} Hz but chain A resolves to {ra}; \
                 A/B needs identical ratios (use the same target for both)"
            ));
        }
    }
    if a.upsampler.target != ab.chain_b.upsampler.target {
        return Err(
            "ab.chain_b.upsampler.target must match chain A (both sides open the device at \
             the same rate)"
                .into(),
        );
    }
    // Bits and dither may differ: B then has its own quantizer.
    if a.output.headroom_db != ab.chain_b.output.headroom_db {
        return Err("ab.chain_b.output.headroom_db must match chain A".into());
    }
    if let LevelMatch::Manual { offset_db } | LevelMatch::Measured { offset_db, .. } = ab.level
        && (!offset_db.is_finite() || !(-60.0..=60.0).contains(&offset_db))
    {
        return Err(format!(
            "ab.level offset_db must be between -60 and 60 dB (got {offset_db})"
        ));
    }
    if let LevelMatch::Measured { seconds, .. } = ab.level
        && (!seconds.is_finite() || seconds < 0.0)
    {
        return Err(format!(
            "ab.level seconds must be finite and >= 0 (got {seconds})"
        ));
    }
    if let AbHeadroom::Fixed { db } = ab.headroom
        && (!db.is_finite() || !(0.0..=AB_HEADROOM_MAX_DB).contains(&db))
    {
        return Err(format!(
            "ab.headroom db must be between 0 and {AB_HEADROOM_MAX_DB} dB (got {db})"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- normalization

/// Everything every config edit (and the config file) goes through before
/// validation: [`normalize_rates`], [`normalize_ab`] and
/// [`normalize_volume`]. Returns the notes of what had to change.
pub fn normalize_config(cfg: &mut EngineConfig) -> Option<String> {
    // The room calibration's items first: side B is materialised from A.
    super::calibration::normalize(cfg);
    // `[0, 1]` is the default pair: stored as no setting.
    cfg.output_channels = OutputChannels::normalized(cfg.output_channels);
    for f in &mut cfg.outputs.followers {
        f.output_channels = OutputChannels::normalized(f.output_channels);
    }
    let notes: Vec<String> = [
        normalize_rates(&mut cfg.rate),
        normalize_ab(cfg),
        normalize_volume(&mut cfg.volume),
    ]
    .into_iter()
    .flatten()
    .collect();
    normalize_followers(cfg);
    (!notes.is_empty()).then(|| notes.join("; "))
}

/// The stored volume never sits above the ceiling: a config (or an edit)
/// that sets both is clamped, so status, the sinks and a later mode change
/// all see the level that actually plays. Values outside the protocol range
/// are left for [`validate`] to reject.
pub fn normalize_volume(volume: &mut audivane_protocol::ipc::VolumeConfig) -> Option<String> {
    let ceiling = volume.max_volume_db?;
    let in_range = |db: f64| (VOLUME_MIN_DB..=VOLUME_MAX_DB).contains(&db);
    if !in_range(ceiling) || !in_range(volume.volume_db) || volume.volume_db <= ceiling {
        return None;
    }
    let note = format!(
        "volume {:.2} dB is above the ceiling, stored as {ceiling:.2} dB",
        volume.volume_db
    );
    volume.volume_db = ceiling;
    Some(note)
}

/// A follower that follows the master's volume has the master level its own
/// `volume_db` is relative to (`follow_ref_db`): one without it (a hand
/// edit, an older file) takes the master's configured level now, so it
/// keeps the level it has; one that does not follow has none.
pub fn normalize_followers(cfg: &mut EngineConfig) {
    let master = crate::engine::volume::target_db(&cfg.volume);
    for f in &mut cfg.outputs.followers {
        f.follow_ref_db = if !f.follow_volume {
            None
        } else {
            Some(
                f.follow_ref_db
                    .filter(|r| r.is_finite())
                    .map_or(master, |r| r.clamp(VOLUME_MIN_DB, VOLUME_MAX_DB)),
            )
        };
    }
}

/// Keep the rate policy consistent instead of rejecting it: `allowed_rates`
/// sorted without duplicates, and a `default_rate` that is not allowed moves
/// to the nearest allowed rate (same 44.1k/48k family first, then the
/// closest, 48 kHz on a tie: io-pw's `nearest_rate`, which the app mirrors).
/// An empty or unsupported list is left for [`validate`] to reject.
pub fn normalize_rates(rate: &mut RatePolicy) -> Option<String> {
    let mut sorted = rate.allowed_rates.clone();
    sorted.sort_unstable();
    sorted.dedup();
    let mut changed = sorted != rate.allowed_rates;
    rate.allowed_rates = sorted;
    let mut note = None;
    if !rate.allowed_rates.contains(&rate.default_rate) {
        let usable: Vec<u32> = rate
            .allowed_rates
            .iter()
            .copied()
            .filter(|r| ALLOWED_RATES.contains(r))
            .collect();
        if let Some(next) = audivane_io_pw::rate_caps::nearest_rate(rate.default_rate, &usable) {
            note = Some(format!(
                "rate.default_rate {} is not allowed; using {next}",
                rate.default_rate
            ));
            rate.default_rate = next;
            changed = true;
        }
    }
    if changed && note.is_none() {
        note = Some("rate.allowed_rates sorted".to_string());
    }
    note
}

/// Materialise chain B from `ab.b` and chain A.
///
/// A patch B is re-applied to the current chain A, so B follows every edit to A
/// outside the patched fields. A chain B (a stored preset) keeps its own stages
/// and filter design, and its upsampler target, limiter and output are
/// realigned with A (stored that way, with a note). Every config edit (and the
/// config file) is normalized *before* validation, so raising the upsampler or
/// changing the dither never fails on a stale chain B.
///
/// A B that no longer applies (a patch value of the wrong type in a hand-edited
/// file) turns A/B off with a note: a chain A edit never fails because of side
/// B. Clients that set B check it first ([`super::ab_side::materialize`]).
pub fn normalize_ab(cfg: &mut EngineConfig) -> Option<String> {
    let ab = cfg.ab.as_mut()?;
    let a = &cfg.chain;
    let mut note = None;
    if let AbB::Chain(chain) = &mut ab.b {
        let changed = super::ab_side::align_chain(chain, a);
        if !changed.is_empty() {
            note = Some(format!(
                "side B's {} now follow{} chain A ({}; {}-bit {} dither, {} dB output \
                 headroom)",
                changed.join(" and "),
                if changed.len() == 1 { "s" } else { "" },
                describe_target(a.upsampler.target),
                a.output.bits,
                dither_name(a.output.dither),
                a.output.headroom_db,
            ));
        }
    }
    match super::ab_side::materialize(a, &ab.b) {
        Ok(mut chain_b) => {
            super::calibration::place_b(
                &mut chain_b,
                cfg.calibration.as_ref(),
                ab.calibration_b.as_ref(),
            );
            ab.chain_b = chain_b;
            note
        }
        Err(err) => {
            cfg.ab = None;
            let off = format!("A/B turned off: side B no longer applies to chain A ({err})");
            Some(match note {
                Some(n) => format!("{n}; {off}"),
                None => off,
            })
        }
    }
}

fn dither_name(dither: DitherKind) -> &'static str {
    match dither {
        DitherKind::None => "no",
        DitherKind::Tpdf => "TPDF",
        DitherKind::Shaped => "shaped",
        DitherKind::HeadroomTpdf => "headroom_tpdf",
        DitherKind::Rectangular => "rectangular",
        DitherKind::Gaussian => "gaussian",
        DitherKind::HighpassTpdf => "highpass_tpdf",
        DitherKind::Feedback1 => "feedback1",
        DitherKind::Feedback5 => "feedback5",
        DitherKind::Resonant5 => "resonant5",
        DitherKind::Resonant9 => "resonant9",
        DitherKind::Integrator15 => "integrator15",
    }
}

fn describe_target(target: RateTarget) -> String {
    match target {
        RateTarget::Off => "upsampling off".to_string(),
        RateTarget::Ratio(ratio) => format!("upsampling x{ratio}"),
        RateTarget::MaxRate(max) => format!("upsampling up to {max} Hz"),
        RateTarget::MaxRateAny(max) => format!("upsampling up to {max} Hz (any rate family)"),
        RateTarget::Rate(rate) => format!("resampling to {rate} Hz"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use audivane_protocol::ipc::{AbConfig, AppRule, SinkChoice};
    use audivane_protocol::{
        DitherKind, EqProfile, OutputConfig, RateTarget, UpsamplerConfig, WindowKind,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-cfg-test-{}-{name}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_only_never_moves_an_invalid_config_aside() {
        let dir = temp_dir("read-only");
        let path = dir.join("config.json");
        let store = ConfigStore::new(path.clone());
        assert_eq!(store.read_only(), None);
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(store.read_only(), None);
        let names: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "something was moved or written");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
        fs::write(&path, serde_json::to_string(&valid()).unwrap()).unwrap();
        assert_eq!(store.read_only().unwrap().device, "hw:0,0");
        let _ = fs::remove_dir_all(&dir);
    }

    fn valid() -> EngineConfig {
        EngineConfig {
            device: "hw:0,0".into(),
            ..EngineConfig::default()
        }
    }

    #[test]
    fn round_trip_is_versioned_and_atomic() {
        let dir = temp_dir("roundtrip");
        let store = ConfigStore::new(dir.join("config.json"));
        let mut cfg = valid();
        cfg.quantum = 512;
        cfg.dsp_enabled = false;
        store.save(&cfg).unwrap();
        let text = fs::read_to_string(store.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["config"]["device"], "hw:0,0");
        let loaded = store.load();
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert_eq!(loaded.config, cfg);
        // No temp file left behind.
        assert!(
            fs::read_dir(&dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp."))
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bare_config_is_accepted_and_unknown_fields_ignored() {
        let dir = temp_dir("bare");
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{"device":"hw:1,2","quantum":256,"future_field":{"x":1}}"#,
        )
        .unwrap();
        let loaded = ConfigStore::new(&path).load();
        assert_eq!(loaded.config.device, "hw:1,2");
        assert_eq!(loaded.config.quantum, 256);
        assert!(loaded.warning.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let dir = temp_dir("corrupt");
        let path = dir.join("config.json");
        fs::write(&path, "{ this is not json").unwrap();
        let loaded = ConfigStore::new(&path).load();
        assert!(!path.exists());
        assert!(
            loaded.warning.as_deref().unwrap().contains("invalid"),
            "{:?}",
            loaded.warning
        );
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1, "{backups:?}");
        assert!(!loaded.config.device.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_values_are_reported_with_field_names() {
        let mut cfg = valid();
        cfg.device = "default".into();
        assert!(validate(&cfg).unwrap_err().contains("hw:"));
        cfg.device = "hw:0,0".into();

        cfg.quantum = 1000;
        assert!(validate(&cfg).unwrap_err().contains("quantum"));
        cfg.quantum = 1024;

        cfg.rate.allowed_rates = vec![44_100, 123_456];
        assert!(validate(&cfg).unwrap_err().contains("123456"));
        cfg.rate.allowed_rates = vec![44_100];
        assert!(validate(&cfg).unwrap_err().contains("default_rate"));
        cfg.rate = RatePolicy::default();

        let set_m = |cfg: &mut EngineConfig, m: &ManualConfig| {
            cfg.chain.upsampler = m.clone().into_upsampler_config().unwrap();
        };

        let mut m = ManualConfig {
            target: RateTarget::Ratio(4),
            ..Default::default()
        };
        m.taps = 0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("taps"));
        m.taps = MAX_TAPS + 1;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("taps"));
        m.taps = 1024;

        m.cutoff = 1.5;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("cutoff"));

        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(0),
            ..Default::default()
        };
        assert!(validate(&cfg).unwrap_err().contains("ratio"));
        cfg.chain.upsampler.target = RateTarget::MaxRate(1_000);
        assert!(validate(&cfg).unwrap_err().contains("max rate"));
        // Setting both preset and design is refused; unknown preset is refused.
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            preset: Some("flattop_long".into()),
            design: Some(StoredDesign::new(catalogue::recommended())),
        };
        assert!(validate(&cfg).unwrap_err().contains("not both"));
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(4),
            preset: Some("no_such_preset".into()),
            design: None,
        };
        assert!(
            validate(&cfg)
                .unwrap_err()
                .contains("unknown filter preset")
        );
        // `recommended` preset is accepted.
        cfg.chain.upsampler = UpsamplerConfig {
            target: RateTarget::MaxRate(768_000),
            preset: Some("recommended".into()),
            design: None,
        };
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Minimum phase is only designed for Kaiser beta 2..=20.
        let mut m = ManualConfig {
            target: RateTarget::Ratio(4),
            phase: FilterPhase::Minimum,
            kaiser_beta: 22.0,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        m.kaiser_beta = 1.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        m.kaiser_beta = 20.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.kaiser_beta = 24.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        m.phase = FilterPhase::Linear;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Every filter length the UI offers up to MAX_TAPS is accepted.
        for taps in [
            16_384, 32_768, 65_536, 131_072, 262_144, 524_288, 1_048_576, 2_097_152, 4_194_304,
        ] {
            let m = ManualConfig {
                target: RateTarget::Ratio(2),
                taps,
                ..Default::default()
            };
            set_m(&mut cfg, &m);
            assert!(validate(&cfg).is_ok(), "taps {taps}");
        }
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Stop at Nyquist: designs outside the calibrated range are rejected.
        let mut m = ManualConfig {
            cutoff_mode: audivane_protocol::CutoffMode::NyquistStop,
            target: RateTarget::Ratio(16),
            kaiser_beta: 20.0,
            taps: 64,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("only guaranteed from"), "{err}");
        m.taps = 16_384;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.kaiser_beta = 25.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("beta"));
        m.kaiser_beta = 14.0;
        let min16 = audivane_dsp_core::resample::nyquist_stop_min_taps(16, 14.0).unwrap();
        m.target = RateTarget::MaxRate(768_000);
        assert_eq!(worst_case_ratio(m.target), 16);
        m.taps = min16 - 1;
        set_m(&mut cfg, &m);
        // x16 at 44.1/48 kHz is checked: the ladder skips it, and the
        // design plays at x8 there.
        validate(&cfg).unwrap();
        let mask = engine_buildable_rate_pairs(&cfg);
        assert!(!mask.allows(44_100, 705_600) && !mask.allows(48_000, 768_000));
        assert!(mask.allows(44_100, 352_800) && mask.allows(48_000, 384_000));
        m.taps = min16;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Taps per input sample are checked at the largest ratio.
        let mut m = ManualConfig {
            target: RateTarget::MaxRate(768_000),
            taps_unit: audivane_protocol::TapsUnit::Input,
            taps: 65_535,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.taps = MAX_TAPS / 16 + 1;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Band designs: edges, attenuation, and the length they need.
        let mut m = ManualConfig {
            target: RateTarget::Ratio(8),
            cutoff_mode: audivane_protocol::CutoffMode::Band,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok(), "default band spec");
        m.band.passband_end = 1.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        m.band.passband_end = 0.9;
        m.band.attenuation_db = 300.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("attenuation"));
        m.band.attenuation_db = 225.0;
        m.band.passband_end = 0.99999;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        m.band.passband_end = 0.9;
        // Minimum phase stops at 190 dB.
        m.phase = FilterPhase::Minimum;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        m.band.attenuation_db = 190.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Intermediate phase: beta 2..=20, mix 0..=1, workspace bound.
        let mut m = ManualConfig {
            target: RateTarget::Ratio(8),
            phase: FilterPhase::Intermediate,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.kaiser_beta = 22.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        m.kaiser_beta = 14.0;
        m.phase_mix = 1.5;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("mix"));
        m.phase_mix = 0.5;
        m.taps = MAX_TAPS / 4;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.taps = MAX_TAPS / 4 + 1;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_err());
        // A reflected minimum phase longer than 2^21 taps is over the design workspace limit.
        {
            let mut m = ManualConfig {
                target: RateTarget::Ratio(2),
                phase: FilterPhase::Minimum,
                taps: 1 << 21,
                ..Default::default()
            };
            let mut cfg = cfg.clone();
            set_m(&mut cfg, &m);
            assert!(validate(&cfg).is_ok());
            m.taps = (1 << 21) + 1;
            set_m(&mut cfg, &m);
            let err = validate(&cfg).unwrap_err();
            assert!(err.contains("GB design limit"), "{err}");
        }
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Keeping the original samples: linear phase, cutoff exactly 1.0.
        let mut m = ManualConfig {
            target: RateTarget::Ratio(8),
            preserve_samples: true,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.cutoff = 0.95;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("preserve_samples"));
        m.cutoff = 1.0;
        m.phase = FilterPhase::Minimum;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("preserve_samples"));
        m.phase = FilterPhase::Linear;
        m.cutoff_mode = audivane_protocol::CutoffMode::NyquistStop;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("preserve_samples"));
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Gaussian window: same rules, its own length for band designs and Stop at Nyquist.
        let mut m = ManualConfig {
            target: RateTarget::Ratio(8),
            window: WindowKind::Gaussian,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.cutoff_mode = audivane_protocol::CutoffMode::NyquistStop;
        m.taps = 64;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("only guaranteed from"));
        m.taps = 65_536;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.cutoff_mode = audivane_protocol::CutoffMode::Band;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Tukey window: fraction mode and linear phase only, taper in (0, 1].
        let mut m = ManualConfig {
            target: RateTarget::Ratio(8),
            window: WindowKind::Tukey,
            preserve_samples: true,
            ..Default::default()
        };
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        m.taper = 0.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).unwrap_err().contains("taper"));
        m.taper = 1.0;
        set_m(&mut cfg, &m);
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Output headroom 0..=12 dB.
        cfg.chain.output.headroom_db = 3.5;
        assert!(validate(&cfg).is_ok());
        cfg.chain.output.headroom_db = -1.0;
        assert!(validate(&cfg).unwrap_err().contains("output.headroom_db"));
        cfg.chain.output.headroom_db = 12.5;
        assert!(validate(&cfg).unwrap_err().contains("output.headroom_db"));
        cfg.chain.output.headroom_db = f64::NAN;
        assert!(validate(&cfg).unwrap_err().contains("output.headroom_db"));
        cfg.chain.output.headroom_db = 0.0;

        cfg.chain.output.bits = 8;
        assert!(validate(&cfg).unwrap_err().contains("output.bits"));
        cfg.chain.output = OutputConfig::default();

        cfg.chain.eq = Some(EqProfile {
            bands: vec![audivane_protocol::Band {
                kind: audivane_protocol::FilterType::Peaking,
                freq_hz: 0.0,
                gain_db: 1.0,
                q: 1.0,
                enabled: true,
            }],
            ..Default::default()
        });
        assert!(validate(&cfg).unwrap_err().contains("freq_hz"));
        cfg.chain.eq = Some(EqProfile {
            left: Some(audivane_protocol::ChannelEq {
                enabled: true,
                preamp_db: -200.0,
                bands: Vec::new(),
            }),
            ..Default::default()
        });
        assert!(validate(&cfg).unwrap_err().contains("eq.left.preamp_db"));
        cfg.chain.eq = Some(EqProfile {
            right: Some(audivane_protocol::ChannelEq {
                enabled: true,
                preamp_db: 100.0,
                bands: Vec::new(),
            }),
            ..Default::default()
        });
        assert!(validate(&cfg).unwrap_err().contains("eq.right.preamp_db"));
        cfg.chain.eq = None;

        cfg.chain.convolution = vec![audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: "/nonexistent/t5b.ir.wav".into(),
            gain_db: 0.0,
            matrix: None,
            calibration: false,
        }];
        assert!(validate(&cfg).unwrap_err().contains("ir_path"));
        cfg.chain.convolution.clear();

        cfg.routing = RoutingMode::Apps {
            rules: vec![AppRule {
                app: "  ".into(),
                sink: SinkChoice::Dsp,
            }],
        };
        assert!(validate(&cfg).unwrap_err().contains("rules[0]"));
        cfg.routing = RoutingMode::System;

        cfg.volume.volume_db = -121.0;
        assert!(validate(&cfg).unwrap_err().contains("volume.volume_db"));
        cfg.volume.volume_db = 0.5;
        assert!(validate(&cfg).unwrap_err().contains("volume.volume_db"));
        cfg.volume.volume_db = f64::NAN;
        assert!(validate(&cfg).unwrap_err().contains("volume.volume_db"));
        cfg.volume.volume_db = -12.0;

        cfg.volume.balance_db = -25.0;
        assert!(validate(&cfg).unwrap_err().contains("volume.balance_db"));
        cfg.volume.balance_db = 24.5;
        assert!(validate(&cfg).unwrap_err().contains("volume.balance_db"));
        cfg.volume.balance_db = f64::INFINITY;
        assert!(validate(&cfg).unwrap_err().contains("volume.balance_db"));
        cfg.volume.balance_db = -3.0;

        cfg.volume.hardware_db = Some(30.0);
        assert!(validate(&cfg).unwrap_err().contains("volume.hardware_db"));
        // A fixed level above 0 dB would play louder than the volume shown.
        cfg.volume.hardware_db = Some(0.5);
        assert!(validate(&cfg).unwrap_err().contains("volume.hardware_db"));
        cfg.volume.hardware_db = Some(0.0);
        assert!(validate(&cfg).is_ok());
        cfg.volume.hardware_db = Some(-6.0);
        cfg.volume.max_volume_db = Some(1.0);
        assert!(validate(&cfg).unwrap_err().contains("volume.max_volume_db"));
        cfg.volume.max_volume_db = Some(f64::NAN);
        assert!(validate(&cfg).unwrap_err().contains("volume.max_volume_db"));
        cfg.volume.max_volume_db = Some(-20.0);
        cfg.volume.guard.max_rise_db_per_s = -1.0;
        assert!(validate(&cfg).unwrap_err().contains("max_rise_db_per_s"));
        cfg.volume.guard.max_rise_db_per_s = 3.0;
        cfg.volume.guard.stream_max_db = Some(-130.0);
        assert!(validate(&cfg).unwrap_err().contains("stream_max_db"));
        cfg.volume.guard.stream_max_db = Some(-10.0);
        assert!(validate(&cfg).is_ok());
        cfg.volume.mode = audivane_protocol::ipc::VolumeMode::Hybrid;
        cfg.volume.hardware_db = None;
        assert!(validate(&cfg).unwrap_err().contains("hardware_db"));
        cfg.volume.hardware_db = Some(-10.0);
        assert!(validate(&cfg).is_ok());

        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn ab_requires_matching_ratio_and_output() {
        let mut cfg = valid();
        cfg.chain.upsampler.target = RateTarget::MaxRate(768_000);
        cfg.ab = Some(AbConfig {
            level: LevelMatch::default(),
            headroom: Default::default(),
            ..AbConfig::chain(ChainConfig {
                upsampler: UpsamplerConfig {
                    target: RateTarget::Ratio(4),
                    ..UpsamplerConfig::default()
                },
                output: cfg.chain.output,
                limiter: cfg.chain.limiter.clone(),
                ..ChainConfig::neutral()
            })
        });
        assert!(validate(&cfg).unwrap_err().contains("ratio"));

        cfg.ab.as_mut().unwrap().chain_b.upsampler.target = RateTarget::MaxRate(768_000);
        // B's own bits and dither: an output compare.
        cfg.ab.as_mut().unwrap().chain_b.output.bits = 16;
        cfg.ab.as_mut().unwrap().chain_b.output.dither = DitherKind::Shaped;
        assert!(validate(&cfg).is_ok());

        cfg.ab.as_mut().unwrap().chain_b.output.headroom_db = 0.0;
        assert!(validate(&cfg).unwrap_err().contains("headroom_db"));

        cfg.ab.as_mut().unwrap().chain_b.output.headroom_db = cfg.chain.output.headroom_db;
        cfg.ab.as_mut().unwrap().level = LevelMatch::Manual { offset_db: 80.0 };
        assert!(validate(&cfg).unwrap_err().contains("offset_db"));
        cfg.ab.as_mut().unwrap().level = LevelMatch::Manual { offset_db: -3.5 };
        assert!(validate(&cfg).is_ok());
        cfg.ab.as_mut().unwrap().headroom = AbHeadroom::Fixed { db: 13.0 };
        assert!(validate(&cfg).unwrap_err().contains("ab.headroom"));
        cfg.ab.as_mut().unwrap().headroom = AbHeadroom::Fixed { db: 3.0 };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn a_stale_chain_b_in_the_file_is_realigned_not_discarded() {
        let dir = temp_dir("stale-ab");
        let path = dir.join("config.json");
        // Chain A upsamples x4 but chain B was left off; the old file would
        // fail validation and be treated as corrupt.
        fs::write(
            &path,
            r#"{"device":"hw:0,0","chain":{"upsampler":{"target":{"ratio":4}}},"ab":{"b":{"chain":{"output":{"bits":24,"dither":"tpdf"}}}}}"#,
        )
        .unwrap();
        let loaded = ConfigStore::new(&path).load();
        assert!(
            loaded
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("chain A")),
            "{:?}",
            loaded.warning
        );
        assert!(path.exists(), "the config must not be moved aside");
        assert!(validate(&loaded.config).is_ok());
        assert_eq!(
            loaded.config.ab.unwrap().chain_b.upsampler.target,
            RateTarget::Ratio(4)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn devices_come_from_the_proc_listing() {
        let pcm = "\
00-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1
00-03: HDMI 0 : HDMI 0 : playback 1
01-00: Loopback : Loopback PCM : playback 8 : capture 8
02-00: USB DAC : USB Audio : playback 1
00-04: ALC892 Alt Analog : ALC892 Alt Analog : capture 1
";
        let cards = "\
 0 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0xf7f10000 irq 30
 1 [Loopback       ]: Loopback - Loopback
 2 [DAC            ]: USB-Audio - USB DAC
";
        let mut devices = devices_from_listing(pcm, cards);
        // Capture-only PCMs are skipped.
        assert_eq!(devices.len(), 4, "{devices:?}");
        assert_eq!(devices[0].id, "hw:CARD=PCH,DEV=0");
        assert_eq!(devices[0].card, "PCH");
        assert_eq!(devices[0].name, "ALC892 Analog");
        assert!(!devices[0].hdmi && !devices[0].loopback);
        assert!(devices[1].hdmi);
        assert_eq!(devices[1].id, "hw:CARD=PCH,DEV=3");
        assert!(devices[2].loopback);
        assert_eq!(devices[2].id, "hw:CARD=Loopback,DEV=0");
        assert_eq!(devices[3].id, "hw:CARD=DAC,DEV=0");
        assert_eq!(devices[3].name, "USB DAC");
        assert!(devices.iter().all(|device| !device.active));

        // Index, CARD= and short forms all name the same device.
        for configured in ["hw:2,0", "hw:CARD=DAC,DEV=0", "hw:CARD=DAC", "hw:2"] {
            assert!(
                mark_active_in(&mut devices, configured, cards),
                "{configured} did not match"
            );
            let active: Vec<&DeviceInfo> = devices.iter().filter(|d| d.active).collect();
            assert_eq!(active.len(), 1, "{configured}: {devices:?}");
            assert_eq!(active[0].id, "hw:CARD=DAC,DEV=0");
        }
        for configured in [
            "hw:DAC,0",
            "hw:DAC",
            "hw:2,0,0",
            "hw:DAC,0,0",
            "hw:CARD=DAC,DEV=0,SUBDEV=0",
        ] {
            assert!(
                mark_active_in(&mut devices, configured, cards),
                "{configured} did not match"
            );
            assert_eq!(
                devices.iter().filter(|d| d.active).count(),
                1,
                "{configured}"
            );
        }
        assert!(mark_active_in(&mut devices, "hw:Loopback,0,0", cards));
        let active: Vec<&DeviceInfo> = devices.iter().filter(|d| d.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "hw:CARD=Loopback,DEV=0");
        assert!(!mark_active_in(&mut devices, "hw:DAC,1", cards));
        assert!(!mark_active_in(&mut devices, "hw:9,0", cards));
        assert!(devices.iter().all(|device| !device.active));
    }

    #[test]
    fn missing_file_yields_defaults_with_a_device() {
        let dir = temp_dir("missing");
        let store = ConfigStore::new(dir.join("config.json"));
        let loaded = store.load();
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert!(!loaded.config.device.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn followers_are_validated() {
        use audivane_protocol::ipc::FollowerConfig;
        let mut cfg = valid();
        let f = |d: &str| FollowerConfig::new(d);
        cfg.outputs.followers = vec![f("hw:CARD=A,DEV=0"), f("hw:CARD=B,DEV=0")];
        validate(&cfg).unwrap();
        let refused_from = |base: &EngineConfig, edit: &dyn Fn(&mut EngineConfig), want: &str| {
            let mut c = base.clone();
            edit(&mut c);
            let e = validate(&c).unwrap_err();
            assert!(e.contains(want), "{e}");
        };
        refused_from(
            &cfg,
            &|c| {
                c.outputs
                    .followers
                    .extend([f("hw:CARD=C,DEV=0"), f("hw:CARD=D,DEV=0")]);
            },
            "at most 3",
        );
        refused_from(
            &cfg,
            &|c| c.outputs.followers[1].device = "hw:CARD=A,DEV=0".into(),
            "twice",
        );
        refused_from(
            &cfg,
            &|c| c.outputs.followers[0].device = "hw:0,0".into(),
            "main output",
        );
        refused_from(
            &cfg,
            &|c| c.outputs.followers[0].device = "default".into(),
            "`hw:`",
        );
        refused_from(&cfg, &|c| c.outputs.followers[0].trim_ms = 50.5, "trim_ms");
        refused_from(
            &cfg,
            &|c| c.outputs.followers[0].trim_ms = f64::NAN,
            "trim_ms",
        );
        refused_from(
            &cfg,
            &|c| c.outputs.followers[0].volume_db = 1.0,
            "volume_db",
        );
        // The speed cap while followers play, only when varispeed runs.
        cfg.varispeed.speed = 2.0;
        validate(&cfg).unwrap();
        refused_from(&cfg, &|c| c.varispeed.enabled = true, "at most 1.996");
        cfg.varispeed.enabled = true;
        cfg.varispeed.speed = FOLLOWED_MAX_SPEED;
        validate(&cfg).unwrap();
        cfg.outputs.followers.clear();
        cfg.varispeed.speed = 2.0;
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_checks_varispeed_bounds_budget_and_allows_small_quanta() {
        let mut cfg = valid();
        cfg.varispeed.enabled = true;
        cfg.varispeed.speed = 0.5;
        cfg.varispeed.ramp_ms = 0.0;
        validate(&cfg).unwrap();

        cfg.varispeed.speed = 2.0;
        cfg.varispeed.ramp_ms = 10_000.0;
        validate(&cfg).unwrap();

        // Small quanta down to MIN_QUANTUM (32) stay allowed (the scheduler
        // caps the speed range instead of refusing the config); below
        // MIN_QUANTUM (e.g. 16) is rejected.
        for q in [32, 64, 128, 256] {
            cfg.quantum = q;
            validate(&cfg).unwrap();
        }
        cfg.quantum = 16;
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("between 32 and 8192"), "{err}");
        cfg.quantum = 1024;

        for bad_speed in [0.49, 2.01, 0.0, -1.0, f64::NAN, f64::INFINITY] {
            cfg.varispeed.speed = bad_speed;
            let err = validate(&cfg).unwrap_err();
            assert!(err.contains("varispeed.speed"), "{bad_speed}: {err}");
        }
        cfg.varispeed.speed = 1.0;

        for bad_ramp in [-1.0, 10_000.1, f64::NAN, f64::INFINITY] {
            cfg.varispeed.ramp_ms = bad_ramp;
            let err = validate(&cfg).unwrap_err();
            assert!(err.contains("varispeed.ramp_ms"), "{bad_ramp}: {err}");
        }
        cfg.varispeed.ramp_ms = 200.0;

        // Static budget: 768 → 705.6 kHz with 64 taps fits without varispeed,
        // is refused when varispeed is enabled, and passes with 16 taps.
        cfg.rate.allowed_rates = vec![768_000];
        cfg.rate.default_rate = 768_000;
        let per_in = |target, taps| {
            ManualConfig {
                target,
                taps,
                taps_unit: audivane_protocol::TapsUnit::Input,
                cutoff: 0.95,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap()
        };
        cfg.chain.upsampler = per_in(RateTarget::Rate(705_600), 64);
        cfg.varispeed.enabled = false;
        validate(&cfg).unwrap();
        cfg.varispeed.enabled = true;
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("varispeed at 768000 Hz"), "{err}");
        cfg.chain.upsampler = per_in(RateTarget::Rate(705_600), 16);
        validate(&cfg).unwrap();

        // MaxRateAny(768_000): the nominal integer ratio at 768 kHz is 1:1,
        // but the driver's ladder includes decimating landings below 768 kHz
        // (720 kHz = 15/16, 705.6 kHz = 147/160) and at 705.6 kHz the
        // fractional 705.6 → 768 kHz landing (160/147). All must be priced at
        // admission.
        cfg.chain.upsampler = per_in(RateTarget::MaxRateAny(768_000), 64);
        cfg.rate.allowed_rates = vec![768_000];
        cfg.rate.default_rate = 768_000;
        cfg.varispeed.enabled = false;
        validate(&cfg).unwrap();
        cfg.varispeed.enabled = true;
        let err = validate(&cfg).unwrap_err();
        assert!(err.contains("varispeed at 768000 Hz"), "{err}");

        // Cap at 705.6 kHz so the highest decimating landing is 147/160.
        cfg.chain.upsampler.target = RateTarget::MaxRateAny(705_600);
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.contains("varispeed at 768000 Hz") && err.contains("147/160"),
            "{err}"
        );

        // Fractional upsampling pair (705.6 → 768 kHz = 160/147) is priced too
        // (96 taps per input frame fits alone, but 674 M stage + 147 M chains
        // exceeds 800 M).
        cfg.chain.upsampler = per_in(RateTarget::MaxRateAny(768_000), 96);
        cfg.rate.allowed_rates = vec![705_600];
        cfg.rate.default_rate = 705_600;
        cfg.varispeed.enabled = false;
        validate(&cfg).unwrap();
        cfg.varispeed.enabled = true;
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.contains("varispeed at 705600 Hz") && err.contains("160/147"),
            "{err}"
        );

        // A 16-tap chain fits both 768 → 705.6 and 705.6 → 768 kHz landings.
        cfg.chain.upsampler = ManualConfig {
            target: RateTarget::MaxRateAny(768_000),
            taps: 16,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        cfg.rate.allowed_rates = vec![705_600, 768_000];
        cfg.rate.default_rate = 768_000;
        validate(&cfg).unwrap();
    }

    #[test]
    fn bypass_mask_covers_the_native_moves_and_follows_the_mode() {
        let cfg = EngineConfig::default();
        assert_eq!(
            cfg.rate.bypass_resampler,
            audivane_protocol::ipc::BypassResampler::Engine
        );
        let t = std::time::Instant::now();
        let mask = bypass_buildable_rate_pairs(&cfg);
        let first = t.elapsed();
        let t = std::time::Instant::now();
        assert_eq!(bypass_buildable_rate_pairs(&cfg), mask);
        let cached = t.elapsed();
        eprintln!("bypass mask: first {first:?}, cached {cached:?}");
        let mut built = Vec::new();
        let mut refused = Vec::new();
        for &i in ALLOWED_RATES {
            for &o in ALLOWED_RATES {
                if i != o {
                    if mask.allows(i, o) {
                        built.push((i, o));
                    } else {
                        refused.push((i, o));
                    }
                }
            }
        }
        eprintln!("bypass mask refuses {refused:?}");
        // The moves PipeWire makes for DACs without the source's family.
        for (i, o) in [
            (44_100, 48_000),
            (88_200, 96_000),
            (176_400, 192_000),
            (48_000, 44_100),
            (96_000, 48_000),
            (192_000, 96_000),
            (44_100, 88_200),
        ] {
            assert!(mask.allows(i, o), "{i} -> {o}");
        }
        // Never to a rate outside the engine's graph rates.
        assert!(!mask.allows(44_100, 132_300));
        let pipewire = EngineConfig {
            rate: RatePolicy {
                bypass_resampler: audivane_protocol::ipc::BypassResampler::Pipewire,
                ..RatePolicy::default()
            },
            ..EngineConfig::default()
        };
        assert_eq!(
            bypass_buildable_rate_pairs(&pipewire),
            audivane_io_pw::RatePairMask::NONE
        );
    }

    /// An equiripple design of 1,279 taps over 0.9..1.0: too short at x16,
    /// too deep at x4 and x2, fine at x8.
    /// 1,279 fixed equiripple taps: too short at x16, builds at x8.
    pub(crate) fn equiripple_1279(target: RateTarget) -> UpsamplerConfig {
        use audivane_protocol::design::{ArithmeticPath, OptimalMethod, Phase};
        let mut d = catalogue::recommended();
        if let FilterDesign::Kernel(k) = &mut d {
            k.shape = Shape::Optimal {
                method: OptimalMethod::Equiripple,
                length: Length::Fixed(1279),
                passband_end: 0.9,
                stopband_start: 1.0,
            };
            k.phase = Phase::Linear;
            k.numerics.path = ArithmeticPath::Fir;
        }
        UpsamplerConfig {
            target,
            preset: None,
            design: Some(StoredDesign::new(d)),
        }
    }

    fn at_rates(up: UpsamplerConfig, rates: &[u32]) -> EngineConfig {
        let mut cfg = valid();
        cfg.chain.upsampler = up;
        cfg.rate.allowed_rates = rates.to_vec();
        cfg.rate.default_rate = rates[0];
        cfg
    }

    /// Acceptance and the design check decide at the landings the ladder
    /// picks: under a 768 kHz cap from 44.1 and 48 kHz, the ladder skips the
    /// x16 rung the design does not build at and lands both at x8, where it
    /// builds. Applied, with nothing falling back.
    #[test]
    fn a_design_that_builds_at_the_ladders_landings_is_applied() {
        let cfg = at_rates(
            equiripple_1279(RateTarget::MaxRate(768_000)),
            &[44_100, 48_000],
        );
        validate(&cfg).unwrap();
        let check = check_design(
            cfg.chain.upsampler.design.as_ref().unwrap(),
            None,
            None,
            &cfg,
            &cfg.chain.output,
            &audivane_dsp_core::cpu_bench::KernelRates::floor(),
            None,
        );
        assert!(check.errors.is_empty(), "{:?}", check.errors);
        assert!(check.advice.is_none(), "{:?}", check.advice);
        assert!(
            check.plans.iter().all(|p| p.ratio == RateRatio::integer(8)),
            "{:?}",
            check.plans
        );
        // Where no rung builds (x16 fixed), refused in the same words.
        let x16 = at_rates(equiripple_1279(RateTarget::Ratio(16)), &[44_100, 48_000]);
        let err = validate(&x16).unwrap_err();
        assert!(
            err.starts_with("chain.upsampler.design.length: 1,279 taps is too short"),
            "{err}"
        );
    }

    /// What the rate mask publishes builds: every pair
    /// [`buildable_rate_pairs`] allows, with A/B off and on, passes
    /// `prepare` (the budgets and the safety gate) for a sample of Custom
    /// designs: each design from two source rates, one with A/B off and one
    /// with it on, so every source rate and both pipeline counts are
    /// covered. The presets' mask is their feasibility at every pair (here),
    /// and the dsp-core fingerprint tiers hold that to the same invariant
    /// over every preset and pair.
    #[test]
    fn what_the_mask_allows_builds() {
        use audivane_dsp_core::design::{Named, prepare};
        use audivane_io_pw::lifecycle::MASK_OUTPUT_RATES;
        let pairs: Vec<(u32, u32)> = ALLOWED_RATES
            .iter()
            .flat_map(|&i| MASK_OUTPUT_RATES.iter().map(move |&o| (i, o)))
            .filter(|&(i, o)| i != o)
            .collect();
        // A debug build gates the many-branch rational grids slowly: the
        // tasks run on every core.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let customs = mask_customs();
        let n = ALLOWED_RATES.len();
        let tasks: Vec<(usize, u32, bool)> = (0..customs.len())
            .flat_map(|d| {
                [
                    (d, ALLOWED_RATES[(2 * d) % n], d % 2 == 0),
                    (d, ALLOWED_RATES[(2 * d + 1) % n], d % 2 == 1),
                ]
            })
            .collect();
        let sources: std::collections::HashSet<u32> = tasks.iter().map(|t| t.1).collect();
        assert_eq!(sources.len(), n, "every source rate is sampled");
        let checks: usize = tasks
            .iter()
            .map(|&(_, i, _)| pairs.iter().filter(|p| p.0 == i).count())
            .sum();
        // One job per (task, output rate): a single pair can take most of a
        // second to prepare, so the jobs spread over every core.
        let jobs: Vec<(usize, u32, bool, u32)> = tasks
            .iter()
            .flat_map(|&(d, input, ab)| {
                pairs
                    .iter()
                    .filter(move |p| p.0 == input)
                    .map(move |&(_, output)| (d, input, ab, output))
            })
            .collect();
        let masks: Vec<[_; 2]> = customs
            .iter()
            .map(|up| [false, true].map(|ab| buildable_rate_pairs(up, rational_pipelines(ab))))
            .collect();
        let designs: Vec<_> = customs
            .iter()
            .map(|up| Named::of(up).unwrap().design)
            .collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let (next, built, refused) = (
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        );
        let failures = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    while let Some(&(d, i, ab, output)) =
                        jobs.get(next.fetch_add(1, Ordering::Relaxed))
                    {
                        if !masks[d][usize::from(ab)].allows(i, output) {
                            refused.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let pipelines = rational_pipelines(ab);
                        let r = RateRatio::from_rates(i, output);
                        match prepare(&designs[d], &design_context(i, r, pipelines)) {
                            Ok(_) => {
                                built.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => failures.lock().unwrap().push(format!(
                                "{:?} with {pipelines} pipelines: the mask allows \
                                 {i} -> {output}, prepare refuses: {e}",
                                customs[d]
                            )),
                        }
                    }
                });
            }
        });
        let failures = failures.into_inner().unwrap();
        assert!(failures.is_empty(), "{failures:#?}");
        let (built, refused) = (built.into_inner(), refused.into_inner());
        assert_eq!(built + refused, checks);
        eprintln!("{built} allowed pairs build, {refused} not allowed");
        assert!(built > 100 && refused > 0, "{built} {refused}");
        // The presets' mask is their feasibility, within the driver's term
        // limit (the fingerprint tiers prepare every such pair).
        let term = audivane_io_pw::lifecycle::MAX_RATIONAL_TERM;
        for &p in audivane_protocol::FilterPreset::ALL {
            let up = UpsamplerConfig {
                preset: Some(p.name().to_string()),
                target: RateTarget::MaxRateAny(audivane_protocol::MAX_OUTPUT_RATE),
                ..UpsamplerConfig::default()
            };
            for pipelines in [rational_pipelines(false), rational_pipelines(true)] {
                let mask = buildable_rate_pairs(&up, pipelines);
                for &(input, output) in &pairs {
                    let r = RateRatio::from_rates(input, output);
                    let ok = r.up <= term
                        && r.down <= term
                        && design::feasibility(
                            &catalogue::design(p),
                            &design_context(input, r, pipelines),
                        )
                        .is_ok();
                    assert_eq!(mask.allows(input, output), ok, "{p:?} {input} -> {output}");
                }
            }
        }
        // 44.1 kHz to 3.072 MHz is 10240/147: past the term limit, never
        // offered (halfband_tiny is feasible there, and its kernel fails
        // the gate's DC check).
        let tiny = audivane_dsp_core::resample::ManualConfig {
            preset: Some(audivane_protocol::FilterPreset::HalfbandTiny),
            target: RateTarget::MaxRateAny(audivane_protocol::MAX_OUTPUT_RATE),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        assert!(!buildable_rate_pairs(&tiny, rational_pipelines(false)).allows(44_100, 3_072_000));
    }

    /// Custom designs over the length rules, phases and windows the mask
    /// sees, short enough to design at every pair.
    fn mask_customs() -> Vec<UpsamplerConfig> {
        use audivane_protocol::{BandSpec, CutoffMode, TapsUnit, WindowKind};
        let custom = ManualConfig {
            preset: None,
            target: RateTarget::MaxRateAny(audivane_protocol::MAX_OUTPUT_RATE),
            taps: 24,
            taps_unit: TapsUnit::Input,
            kaiser_beta: 10.0,
            cutoff: 0.95,
            ..Default::default()
        };
        let band = BandSpec {
            passband_end: 0.8,
            passband_end_hz: None,
            stopband_start: 1.0,
            attenuation_db: 90.0,
        };
        let to_up = |m: ManualConfig| m.into_upsampler_config().unwrap();
        vec![
            to_up(custom.clone()),
            to_up(ManualConfig {
                taps: 255,
                taps_unit: TapsUnit::Output,
                ..custom.clone()
            }),
            to_up(ManualConfig {
                phase: FilterPhase::Minimum,
                ..custom.clone()
            }),
            to_up(ManualConfig {
                phase: FilterPhase::Intermediate,
                phase_mix: 0.5,
                ..custom.clone()
            }),
            to_up(ManualConfig {
                phase: FilterPhase::Intermediate,
                phase_mix: 1.0,
                ..custom.clone()
            }),
            to_up(ManualConfig {
                cutoff_mode: CutoffMode::NyquistStop,
                ..custom.clone()
            }),
            to_up(ManualConfig {
                cutoff_mode: CutoffMode::Band,
                band,
                ..custom.clone()
            }),
            to_up(ManualConfig {
                window: WindowKind::Tukey,
                cutoff: 1.0,
                ..custom.clone()
            }),
            // Two taps at x2: all zeros, never offered.
            to_up(ManualConfig {
                window: WindowKind::Tukey,
                cutoff: 1.0,
                taps: 2,
                taps_unit: TapsUnit::Output,
                ..custom.clone()
            }),
            to_up(ManualConfig {
                window: WindowKind::Gaussian,
                ..custom.clone()
            }),
        ]
    }
}
