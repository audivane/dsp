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

use audivane_dsp_core::minphase::{MAX_MINIMUM_PHASE_BETA, MIN_MINIMUM_PHASE_BETA};
use audivane_dsp_core::resample::nyquist_stop_check;
use audivane_protocol::ipc::{
    AbConfig, BALANCE_MAX_DB, DeviceInfo, EngineConfig, LevelMatch, RatePolicy, RoutingMode,
    VOLUME_MAX_DB, VOLUME_MIN_DB,
};
use audivane_protocol::{
    ChainConfig, CutoffMode, DitherKind, FilterPhase, RateTarget, UpsamplerConfig, resolve_ratio,
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

pub const MIN_TAPS: usize = 1;
/// 4M = 2^22, so every length step in the UI is a power of two.
pub const MAX_TAPS: usize = 4_194_304;
pub const MAX_BANDS: usize = 4096;

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

    /// Load the config, falling back to defaults (and preserving a corrupted
    /// file) when anything is wrong.
    pub fn load(&self) -> Loaded {
        let mut warnings = Vec::new();
        let mut config = match fs::read_to_string(&self.path) {
            Ok(text) => match parse_document(&text) {
                Ok(mut cfg) => {
                    // Chain B follows chain A; an older file may still carry a
                    // chain B from before the upsampler/output changed, which
                    // would otherwise look "invalid" and be moved aside.
                    if let Some(note) = normalize_config(&mut cfg) {
                        warnings.push(format!("{}: {note}", self.path.display()));
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
            Err(err) if err.kind() == io::ErrorKind::NotFound => EngineConfig::default(),
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
/// written files.
fn parse_document(text: &str) -> Result<EngineConfig, String> {
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
    let cfg_value = object
        .get("config")
        .cloned()
        .unwrap_or_else(|| value.clone());
    serde_json::from_value::<EngineConfig>(cfg_value).map_err(|err| format!("bad config: {err}"))
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
/// (`hw:CARD=<id>,DEV=<n>`), so it survives card numbers moving around
/// between boots.
///
/// Loopback cards are used heavily by the test rig, so they are only chosen
/// when nothing else is available.
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
pub fn device_present(device: &str) -> bool {
    let mut devices = playback_devices();
    mark_active(&mut devices, device)
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
    devices_from_listing(&pcm, &cards)
}

/// Parse `/proc/asound/pcm` + `/proc/asound/cards` into playback devices.
///
/// `pcm` lines look like:
/// ```text
/// 00-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1
/// 00-03: HDMI 0 : HDMI 0 : playback 1
/// ```
/// and `cards` lines like:
/// ```text
///  0 [PCH            ]: HDA-Intel - HDA Intel PCH
///                       HDA Intel PCH at 0x... irq 30
/// ```
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
    if !(16..=8192).contains(&cfg.quantum) || !cfg.quantum.is_power_of_two() {
        return Err(format!(
            "quantum must be a power of two between 16 and 8192 (got {})",
            cfg.quantum
        ));
    }
    validate_rate_policy(&cfg.rate)?;
    if let Some(name) = &cfg.active_preset {
        crate::control::presets::normalize_name(name)
            .map_err(|message| format!("active_preset: {message}"))?;
    }
    validate_chain("chain", &cfg.chain)?;
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

/// Validate one upsampler config (taps, Kaiser beta, cutoff, target).
///
/// `field` is the full prefix used in messages, e.g. `"chain.upsampler"`.
/// This is shared by chain validation and the measured-response call, so an
/// explicit `UpsamplerResponse { config }` cannot bypass the kernel limits.
pub fn validate_upsampler(field: &str, up: &UpsamplerConfig) -> Result<(), String> {
    if !(MIN_TAPS..=MAX_TAPS).contains(&up.taps) {
        return Err(format!(
            "{field}.taps must be between {MIN_TAPS} and {MAX_TAPS} (got {})",
            up.taps
        ));
    }
    if !up.kaiser_beta.is_finite() || !(0.0..=40.0).contains(&up.kaiser_beta) {
        return Err(format!(
            "{field}.kaiser_beta must be between 0 and 40 (got {})",
            up.kaiser_beta
        ));
    }
    if !up.cutoff.is_finite() || !(0.0 < up.cutoff && up.cutoff <= 1.0) {
        return Err(format!(
            "{field}.cutoff must be in (0, 1] (got {})",
            up.cutoff
        ));
    }
    match up.target {
        RateTarget::Off => {}
        RateTarget::Ratio(ratio) => {
            if !(1..=64).contains(&ratio) {
                return Err(format!(
                    "{field}.target ratio must be between 1 and 64 (got {ratio})"
                ));
            }
        }
        RateTarget::MaxRate(max) => {
            if !(8_000..=1_536_000).contains(&max) {
                return Err(format!(
                    "{field}.target max rate must be between 8000 and 1536000 Hz (got {max})"
                ));
            }
            if max < 44_100 {
                return Err(format!(
                    "{field}.target max rate {max} Hz is below the lowest source rate"
                ));
            }
        }
    }
    if up.phase == FilterPhase::Minimum
        && !(MIN_MINIMUM_PHASE_BETA..=MAX_MINIMUM_PHASE_BETA).contains(&up.kaiser_beta)
    {
        return Err(format!(
            "{field}: minimum phase needs a Kaiser beta between {MIN_MINIMUM_PHASE_BETA} and \
             {MAX_MINIMUM_PHASE_BETA} (got {})",
            up.kaiser_beta
        ));
    }
    if up.cutoff_mode == CutoffMode::NyquistStop {
        // The largest ratio this target can reach (lowest source rate) is
        // the hardest case: the guarantee must hold at every source rate.
        let ratio = worst_case_ratio(up.target) as usize;
        nyquist_stop_check(up.taps, ratio, up.kaiser_beta)
            .map_err(|e| format!("{field}.cutoff_mode: {e}"))?;
    }
    Ok(())
}

/// Largest upsampling ratio `target` resolves to over [`ALLOWED_RATES`].
pub fn worst_case_ratio(target: RateTarget) -> u32 {
    ALLOWED_RATES
        .iter()
        .map(|&rate| resolve_ratio(target, rate))
        .max()
        .unwrap_or(1)
}

/// Validate a single DSP chain. `field` prefixes messages for nested configs.
pub fn validate_chain(field: &str, chain: &ChainConfig) -> Result<(), String> {
    if let Some(eq) = &chain.eq {
        validate_eq_channel(&format!("{field}.eq"), eq.preamp_db, &eq.bands)?;
        if let Some(left) = &eq.left {
            validate_eq_channel(&format!("{field}.eq.left"), left.preamp_db, &left.bands)?;
        }
        if let Some(right) = &eq.right {
            validate_eq_channel(&format!("{field}.eq.right"), right.preamp_db, &right.bands)?;
        }
    }
    for (i, conv) in chain.convolution.iter().enumerate() {
        if !conv.gain_db.is_finite() {
            return Err(format!("{field}.convolution[{i}].gain_db must be finite"));
        }
        if conv.enabled {
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
    validate_upsampler(&format!("{field}.upsampler"), &chain.upsampler)?;
    if !(16..=32).contains(&chain.output.bits) {
        return Err(format!(
            "{field}.output.bits must be between 16 and 32 (got {})",
            chain.output.bits
        ));
    }
    Ok(())
}

fn validate_ab(ab: &AbConfig, a: &ChainConfig, rate: &RatePolicy) -> Result<(), String> {
    validate_chain("ab.chain_b", &ab.chain_b)?;
    for &r in rate
        .allowed_rates
        .iter()
        .chain(std::iter::once(&rate.default_rate))
    {
        let ra = resolve_ratio(a.upsampler.target, r);
        let rb = resolve_ratio(ab.chain_b.upsampler.target, r);
        if ra != rb {
            return Err(format!(
                "ab.chain_b upsampler resolves to x{rb} at {r} Hz but chain A resolves to x{ra}; \
                 A/B needs identical ratios (use the same target for both)"
            ));
        }
    }
    if a.output.bits != ab.chain_b.output.bits {
        return Err(format!(
            "ab.chain_b.output.bits ({}) must match chain A ({})",
            ab.chain_b.output.bits, a.output.bits
        ));
    }
    if a.output.dither != ab.chain_b.output.dither {
        return Err(
            "ab.chain_b.output.dither must match chain A; both sides must be quantized the same way"
                .into(),
        );
    }
    if let LevelMatch::Manual { offset_db } = ab.level
        && (!offset_db.is_finite() || !(-60.0..=60.0).contains(&offset_db))
    {
        return Err(format!(
            "ab.level offset_db must be between -60 and 60 dB (got {offset_db})"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- normalization

/// Everything every config edit (and the config file) goes through before
/// validation: [`normalize_rates`] and [`normalize_ab`]. Returns the notes
/// of what had to change.
pub fn normalize_config(cfg: &mut EngineConfig) -> Option<String> {
    let notes: Vec<String> = [normalize_rates(&mut cfg.rate), normalize_ab(cfg)]
        .into_iter()
        .flatten()
        .collect();
    (!notes.is_empty()).then(|| notes.join("; "))
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

/// Realign chain B with chain A wherever they must agree.
///
/// A/B needs both sides to resolve to the same upsampling ratio at every
/// allowed rate and to quantize identically, so chain B's `upsampler` and
/// `output` are not its own: they always follow chain A. Every config edit
/// (and the config file) is normalized *before* validation, so raising the
/// upsampler, turning it off, or changing the dither never leaves chain B
/// behind and never fails on a stale chain B.
///
/// Returns a human-readable note when something had to be changed.
pub fn normalize_ab(cfg: &mut EngineConfig) -> Option<String> {
    let ab = cfg.ab.as_mut()?;
    let a = &cfg.chain;
    let mut changed = Vec::new();
    if ab.chain_b.upsampler != a.upsampler {
        ab.chain_b.upsampler = a.upsampler.clone();
        changed.push("ab.chain_b.upsampler");
    }
    if ab.chain_b.output != a.output {
        ab.chain_b.output = a.output;
        changed.push("ab.chain_b.output");
    }
    if changed.is_empty() {
        return None;
    }
    Some(format!(
        "{} now follow{} chain A ({}; {}-bit {} dither)",
        changed.join(" and "),
        if changed.len() == 1 { "s" } else { "" },
        describe_target(a.upsampler.target),
        a.output.bits,
        dither_name(a.output.dither),
    ))
}

fn describe_target(target: RateTarget) -> String {
    match target {
        RateTarget::Off => "upsampling off".to_string(),
        RateTarget::Ratio(ratio) => format!("upsampling x{ratio}"),
        RateTarget::MaxRate(max) => format!("upsampling up to {max} Hz"),
    }
}

fn dither_name(dither: DitherKind) -> &'static str {
    match dither {
        DitherKind::None => "no",
        DitherKind::Tpdf => "TPDF",
        DitherKind::Shaped => "shaped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::ipc::{AbConfig, AppRule, SinkChoice};
    use audivane_protocol::{DitherKind, EqProfile, OutputConfig, RateTarget, UpsamplerConfig};

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
    fn rate_policy_is_normalized_not_rejected() {
        let mut rate = RatePolicy::default();
        assert_eq!(normalize_rates(&mut rate), None);

        // Disabling the default moves it within its family first.
        rate.allowed_rates = vec![192_000, 44_100, 88_200, 96_000, 44_100];
        rate.default_rate = 48_000;
        assert!(normalize_rates(&mut rate).unwrap().contains("using 96000"));
        assert_eq!(rate.allowed_rates, vec![44_100, 88_200, 96_000, 192_000]);
        assert_eq!(rate.default_rate, 96_000);

        // No rate of the same family left: the closest one.
        rate.allowed_rates = vec![44_100, 88_200];
        rate.default_rate = 48_000;
        normalize_rates(&mut rate);
        assert_eq!(rate.default_rate, 44_100);

        // The high rates are ordinary entries.
        rate.allowed_rates = vec![352_800, 768_000];
        rate.default_rate = 705_600;
        normalize_rates(&mut rate);
        assert_eq!(rate.default_rate, 352_800);
        let mut cfg = valid();
        cfg.rate = rate.clone();
        assert!(validate(&cfg).is_ok());

        // Nothing usable is left for validation to reject.
        rate.allowed_rates = vec![];
        assert_eq!(normalize_rates(&mut rate), None);
        rate.allowed_rates = vec![123_456];
        normalize_rates(&mut rate);
        assert_eq!(rate.default_rate, 352_800);
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

        cfg.chain.upsampler.taps = 0;
        assert!(validate(&cfg).unwrap_err().contains("taps"));
        cfg.chain.upsampler.taps = MAX_TAPS + 1;
        assert!(validate(&cfg).unwrap_err().contains("taps"));
        cfg.chain.upsampler.taps = 1024;

        cfg.chain.upsampler.cutoff = 1.5;
        assert!(validate(&cfg).unwrap_err().contains("cutoff"));
        cfg.chain.upsampler.cutoff = 1.0;

        cfg.chain.upsampler.target = RateTarget::Ratio(0);
        assert!(validate(&cfg).unwrap_err().contains("ratio"));
        cfg.chain.upsampler.target = RateTarget::MaxRate(1_000);
        assert!(validate(&cfg).unwrap_err().contains("max rate"));
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Minimum phase is only designed for Kaiser beta 2..=20.
        cfg.chain.upsampler.phase = FilterPhase::Minimum;
        cfg.chain.upsampler.kaiser_beta = 22.0;
        assert!(validate(&cfg).unwrap_err().contains("minimum phase"));
        cfg.chain.upsampler.kaiser_beta = 1.0;
        assert!(validate(&cfg).unwrap_err().contains("minimum phase"));
        cfg.chain.upsampler.kaiser_beta = 20.0;
        assert!(validate(&cfg).is_ok());
        // The UI's beta 24 preset: fine for linear phase, not for minimum.
        cfg.chain.upsampler.kaiser_beta = 24.0;
        assert!(validate(&cfg).unwrap_err().contains("minimum phase"));
        cfg.chain.upsampler.phase = FilterPhase::Linear;
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Every filter length the UI offers is accepted.
        for taps in [
            16_384, 32_768, 65_536, 131_072, 262_144, 524_288, 1_048_576, 2_097_152, 4_194_304,
        ] {
            cfg.chain.upsampler.taps = taps;
            assert!(validate(&cfg).is_ok(), "taps {taps}");
        }
        cfg.chain.upsampler = UpsamplerConfig::default();

        // Stop at Nyquist: designs outside the calibrated range are rejected,
        // judged at the largest ratio the target reaches over the allowed
        // source rates.
        cfg.chain.upsampler.cutoff_mode = CutoffMode::NyquistStop;
        cfg.chain.upsampler.target = RateTarget::Ratio(16);
        cfg.chain.upsampler.kaiser_beta = 20.0;
        cfg.chain.upsampler.taps = 64;
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.contains("cutoff_mode") && err.contains("only guaranteed from"),
            "{err}"
        );
        cfg.chain.upsampler.taps = 16_384;
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler.kaiser_beta = 25.0;
        assert!(validate(&cfg).unwrap_err().contains("beta"));
        cfg.chain.upsampler.kaiser_beta = 14.0;
        let min16 = audivane_dsp_core::resample::nyquist_stop_min_taps(16, 14.0).unwrap();
        cfg.chain.upsampler.target = RateTarget::MaxRate(768_000);
        assert_eq!(worst_case_ratio(cfg.chain.upsampler.target), 16);
        cfg.chain.upsampler.taps = min16 - 1;
        assert!(
            validate(&cfg).is_err(),
            "x16 at 44.1/48 kHz must be checked"
        );
        cfg.chain.upsampler.taps = min16;
        assert!(validate(&cfg).is_ok());
        cfg.chain.upsampler.target = RateTarget::Off;
        cfg.chain.upsampler.taps = 1;
        assert!(validate(&cfg).is_ok(), "no upsampling, nothing to stop");
        cfg.chain.upsampler = UpsamplerConfig::default();

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

        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn convolution_may_be_disabled_without_a_file() {
        let mut cfg = valid();
        cfg.chain.convolution = vec![audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: false,
            ir_path: "/nonexistent/t5b.ir.wav".into(),
            gain_db: 0.0,
        }];
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn ab_requires_matching_ratio_and_output() {
        let mut cfg = valid();
        cfg.chain.upsampler.target = RateTarget::MaxRate(768_000);
        cfg.ab = Some(AbConfig {
            chain_b: ChainConfig {
                upsampler: UpsamplerConfig {
                    target: RateTarget::Ratio(4),
                    ..UpsamplerConfig::default()
                },
                output: OutputConfig {
                    bits: 24,
                    dither: DitherKind::Tpdf,
                },
                ..Default::default()
            },
            level: LevelMatch::Analytic,
        });
        assert!(validate(&cfg).unwrap_err().contains("ratio"));

        cfg.ab.as_mut().unwrap().chain_b.upsampler.target = RateTarget::MaxRate(768_000);
        cfg.ab.as_mut().unwrap().chain_b.output.bits = 16;
        assert!(validate(&cfg).unwrap_err().contains("output.bits"));

        cfg.ab.as_mut().unwrap().chain_b.output.bits = 24;
        cfg.ab.as_mut().unwrap().chain_b.output.dither = DitherKind::Shaped;
        assert!(validate(&cfg).unwrap_err().contains("dither"));

        cfg.ab.as_mut().unwrap().chain_b.output.dither = DitherKind::Tpdf;
        cfg.ab.as_mut().unwrap().level = LevelMatch::Manual { offset_db: 80.0 };
        assert!(validate(&cfg).unwrap_err().contains("offset_db"));
        cfg.ab.as_mut().unwrap().level = LevelMatch::Manual { offset_db: -3.5 };
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn ab_chain_b_follows_chain_a() {
        let mut cfg = valid();
        cfg.chain.upsampler.target = RateTarget::Ratio(4);
        cfg.chain.output.bits = 24;
        cfg.chain.output.dither = DitherKind::Tpdf;
        cfg.ab = Some(AbConfig {
            chain_b: ChainConfig {
                upsampler: UpsamplerConfig {
                    target: RateTarget::Off,
                    taps: 4096,
                    ..UpsamplerConfig::default()
                },
                output: OutputConfig {
                    bits: 16,
                    dither: DitherKind::Shaped,
                },
                ..Default::default()
            },
            level: LevelMatch::Analytic,
        });
        // The raw config is invalid ...
        assert!(validate(&cfg).is_err());

        // ... and normalization makes chain B follow A, before validation.
        let note = normalize_ab(&mut cfg).expect("a note");
        assert!(note.contains("upsampler"), "{note}");
        assert!(note.contains("output"), "{note}");
        assert!(note.contains("x4"), "{note}");
        let b = &cfg.ab.as_ref().unwrap().chain_b;
        assert_eq!(b.upsampler, cfg.chain.upsampler);
        assert_eq!(b.output, cfg.chain.output);
        assert!(validate(&cfg).is_ok());

        // Already aligned: nothing to do, no note.
        assert!(normalize_ab(&mut cfg).is_none());
    }

    #[test]
    fn ab_is_not_touched_without_ab() {
        let mut cfg = valid();
        cfg.chain.upsampler.target = RateTarget::Ratio(8);
        assert!(normalize_ab(&mut cfg).is_none());
        assert!(cfg.ab.is_none());
    }

    #[test]
    fn a_stale_chain_b_in_the_file_is_realigned_not_discarded() {
        let dir = temp_dir("stale-ab");
        let path = dir.join("config.json");
        // Chain A upsamples x4 but chain B was left off; the old file would
        // fail validation and be treated as corrupt.
        fs::write(
            &path,
            r#"{"device":"hw:0,0","chain":{"upsampler":{"target":{"ratio":4}}},"ab":{"chain_b":{"output":{"bits":24,"dither":"tpdf"}}}}"#,
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
    fn default_device_prefers_real_cards() {
        let text = "\
00-00: HDMI 0 : HDMI 0 : playback 1
01-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1
02-00: Loopback : Loopback PCM : playback 8 : capture 8
";
        let cards = "\
 0 [NVidia         ]: HDA-Intel - HDA NVidia
                      HDA NVidia at 0xdc080000 irq 137
 1 [PCH            ]: HDA-Intel - HDA Intel PCH
                      HDA Intel PCH at 0x1 irq 30
 2 [Loopback       ]: Loopback - Loopback
                      Loopback 1
10 [Loopback_1     ]: Loopback - Loopback
                      Loopback 1
";
        // Named by card id, not by its (boot-order dependent) number.
        assert_eq!(
            default_device_from_listing(text, cards),
            Some("hw:CARD=PCH,DEV=0".into())
        );

        let only_loopback = "10-00: Loopback : Loopback PCM : playback 8 : capture 8\n";
        assert_eq!(
            default_device_from_listing(only_loopback, cards),
            Some("hw:CARD=Loopback_1,DEV=0".into())
        );

        let capture_only = "00-00: HDMI 0 : HDMI 0 : capture 1\n";
        assert_eq!(default_device_from_listing(capture_only, cards), None);
    }

    #[test]
    fn numeric_device_names_get_the_card_id() {
        let cards = "\
 0 [NVidia         ]: HDA-Intel - HDA NVidia
                      HDA NVidia at 0xdc080000 irq 137
 2 [S3             ]: USB-Audio - S3
                      Some DAC at usb-0000:00:14.0-2, high speed
";
        assert_eq!(
            stable_device_name_in("hw:2,0", cards).as_deref(),
            Some("hw:CARD=S3,DEV=0")
        );
        assert_eq!(
            stable_device_name_in("hw:2", cards).as_deref(),
            Some("hw:CARD=S3,DEV=0")
        );
        assert_eq!(
            stable_device_name_in(" hw:2,1,0", cards).as_deref(),
            Some("hw:CARD=S3,DEV=1,SUBDEV=0")
        );
        // Already stable, missing card, or not hw: left alone.
        assert_eq!(stable_device_name_in("hw:CARD=S3,DEV=0", cards), None);
        assert_eq!(stable_device_name_in("hw:S3,0", cards), None);
        assert_eq!(stable_device_name_in("hw:5,0", cards), None);
        assert_eq!(stable_device_name_in("plughw:2,0", cards), None);
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
    fn old_config_file_loads_with_single_convolution_and_no_right_eq() {
        let dir = temp_dir("legacy");
        let ir = dir.join("room.wav");
        fs::write(&ir, b"stub").unwrap();
        let path = dir.join("config.json");
        let legacy = serde_json::json!({
            "version": 1,
            "config": {
                "device": "hw:0,0",
                "chain": {
                    "eq": { "name": "hp", "preamp_db": -2.0, "bands": [] },
                    "convolution": { "enabled": true, "ir_path": ir, "gain_db": -1.5 }
                }
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();
        let loaded = ConfigStore::new(&path).load();
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert_eq!(loaded.config.chain.eq.as_ref().unwrap().right, None);
        assert_eq!(loaded.config.chain.convolution.len(), 1);
        assert_eq!(loaded.config.chain.convolution[0].gain_db, -1.5);
        assert_eq!(loaded.config.chain.convolution[0].name, None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_split_eq_config_migrates_to_layered_v2() {
        let dir = temp_dir("legacy-split");
        let path = dir.join("config.json");
        let legacy = serde_json::json!({
            "version": 1,
            "config": {
                "device": "hw:0,0",
                "chain": {
                    "eq": {
                        "name": "split",
                        "preamp_db": -3.0,
                        "bands": [{"kind":"peaking","freq_hz":200.0,"gain_db":2.0,"q":1.0,"enabled":true}],
                        "right": {
                            "preamp_db": -1.5,
                            "bands": [{"kind":"peaking","freq_hz":500.0,"gain_db":3.0,"q":1.0,"enabled":true}]
                        }
                    }
                }
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();
        let store = ConfigStore::new(&path);
        let loaded = store.load();
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        let eq = loaded.config.chain.eq.as_ref().expect("eq loaded");
        assert_eq!(eq.version, audivane_protocol::EQ_PROFILE_VERSION);
        assert_eq!(eq.preamp_db, 0.0);
        assert!(eq.bands.is_empty());
        let left = eq.left.as_ref().expect("left migrated");
        assert!(left.enabled);
        assert_eq!(left.preamp_db, -3.0);
        assert_eq!(left.bands.len(), 1);
        let right = eq.right.as_ref().expect("right kept");
        assert!(right.enabled);
        assert_eq!(right.preamp_db, -1.5);
        assert_eq!(right.bands.len(), 1);

        // Saving writes version 2 so reload does not migrate again.
        store.save(&loaded.config).unwrap();
        let reloaded = store.load();
        assert_eq!(reloaded.config, loaded.config);
        fs::remove_dir_all(&dir).unwrap();
    }
}
