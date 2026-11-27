//! Presets: parts of chain A's signal path, stored by the daemon.
//!
//! One JSON file per preset next to `config.json`
//! (`$XDG_CONFIG_HOME/audivane-dsp/presets/`, or the directory of a
//! `--config` override). Global presets sit in `presets/`, each page's own in
//! `presets/<part>/` (`presets/eq/`, `presets/upsampling/`, …):
//!
//! ```json
//! { "version": 1, "name": "HD 650", "parts": ["eq", "upsampling"], "chain": { "eq": { … }, "upsampler": { … } } }
//! { "version": 1, "name": "HD 650", "part": "eq", "chain": { "eq": { … } } }
//! ```
//!
//! A file holds only the chain keys of its parts; a global preset without
//! `parts` holds every part. The directory decides the page: a file's own
//! `part` is overridden by it.
//!
//! The file name is a safe slug of the name (lowercase ascii, digits, `-`);
//! the authoritative name is the `name` field, so two names that slug the same
//! get distinct files. Writes are atomic, exactly like `config.json` (temp
//! file + fsync + rename + directory fsync) and synchronous, because saving a
//! preset is an explicit user action.
//!
//! The chain is treated as an opaque value: this module serializes, compares
//! and validates it but never interprets its stages (beyond counting EQ bands
//! and convolution entries for `PresetInfo`).

use crate::control::config::{self, ConfigStore, write_atomic};
use audivane_protocol::ChainConfig;
use audivane_protocol::ipc::{Preset, PresetInfo, PresetPart};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Longest accepted preset name, in characters (after trimming).
pub const MAX_NAME_CHARS: usize = 64;
/// Longest slug we generate for a file name.
const MAX_SLUG_CHARS: usize = 48;

#[derive(Debug)]
pub enum PresetError {
    NotFound(String),
    Exists(String),
    InvalidName(String),
    Io(io::Error),
}

impl fmt::Display for PresetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(name) => write!(f, "no preset named {name:?}"),
            Self::Exists(name) => write!(
                f,
                "a preset named {name:?} already exists; pass overwrite to replace it"
            ),
            Self::InvalidName(message) => write!(f, "{message}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PresetError {}

impl From<io::Error> for PresetError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Trim and validate a preset name: 1–64 characters, no control characters.
pub fn normalize_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("preset name must not be empty".into());
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "preset name is longer than {MAX_NAME_CHARS} characters"
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err("preset name must not contain control characters".into());
    }
    Ok(trimmed.to_string())
}

/// A file-name-safe form of `name`: lowercase ascii letters, digits and `-`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let shortened: String = out.chars().take(MAX_SLUG_CHARS).collect();
    let trimmed = shortened.trim_matches('-');
    if trimmed.is_empty() {
        "preset".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `(EQ bands, convolution impulse responses)` in `chain`, for `PresetInfo`.
///
/// The convolution stage's IRs are counted through serialization, which accepts
/// either a single IR or a list.
pub fn chain_stats(chain: &ChainConfig) -> (u32, u32) {
    let bands = chain.eq.as_ref().map_or(0, |eq| {
        (eq.bands.len()
            + eq.left.as_ref().map_or(0, |l| l.bands.len())
            + eq.right.as_ref().map_or(0, |r| r.bands.len())) as u32
    });
    let convolution = serde_json::to_value(chain)
        .ok()
        .and_then(|value| value.get("convolution").cloned())
        .map(|value| match value {
            serde_json::Value::Null => 0,
            serde_json::Value::Array(items) => items.len() as u32,
            _ => 1,
        })
        .unwrap_or(0);
    (bands, convolution)
}

/// The chain JSON of `preset` reduced to the keys of the parts it holds.
fn stripped_chain(preset: &Preset) -> io::Result<serde_json::Value> {
    use serde_json::{Map, Value};
    let full = serde_json::to_value(&preset.chain)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let Value::Object(full) = full else {
        return Ok(full);
    };
    let mut out = Map::new();
    for part in preset.covers() {
        for (key, field) in part.json_paths() {
            let Some(value) = full.get(*key) else {
                continue;
            };
            match field {
                None => {
                    out.insert((*key).to_string(), value.clone());
                }
                Some(field) => {
                    if let Some(inner) = value.get(*field) {
                        let entry = out
                            .entry((*key).to_string())
                            .or_insert_with(|| Value::Object(Map::new()));
                        if let Value::Object(entry) = entry {
                            entry.insert((*field).to_string(), inner.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(Value::Object(out))
}

/// A preset directory: the global presets (`part: None`) or one page's.
#[derive(Clone, Debug)]
pub struct PresetStore {
    dir: PathBuf,
    part: Option<PresetPart>,
}

impl PresetStore {
    /// The global presets in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            part: None,
        }
    }

    /// `presets/` in the directory that holds `config.json`, so `--config`
    /// moves both.
    pub fn alongside(store: &ConfigStore) -> Self {
        let parent = store.path().parent().unwrap_or(Path::new("."));
        Self::new(parent.join("presets"))
    }

    /// The store for `part`'s presets (`<dir>/<part>/`), or the global one.
    /// Call it on the global store.
    pub fn scoped(&self, part: Option<PresetPart>) -> Self {
        match part {
            None => self.clone(),
            Some(part) => Self {
                dir: self.dir.join(part.name()),
                part: Some(part),
            },
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every preset: the global ones, then each part's, each sorted by name.
    /// Call it on the global store.
    pub fn list_all(&self) -> Vec<PresetInfo> {
        let mut all = self.list();
        for part in PresetPart::ALL {
            all.extend(self.scoped(Some(part)).list());
        }
        all
    }

    /// Every readable preset, sorted by name. Unreadable files are logged and
    /// skipped rather than failing the whole list.
    pub fn list(&self) -> Vec<PresetInfo> {
        let mut infos: Vec<PresetInfo> = self
            .entries()
            .into_iter()
            .map(|(_, _, info)| info)
            .collect();
        infos.sort_by(|a, b| {
            (a.name.to_ascii_lowercase(), &a.name).cmp(&(b.name.to_ascii_lowercase(), &b.name))
        });
        infos
    }

    /// The preset named `name`, or `NotFound`.
    pub fn load(&self, name: &str) -> Result<Preset, PresetError> {
        let name = normalize_name(name).map_err(PresetError::InvalidName)?;
        self.find(&name).map(|(_, preset)| preset)
    }

    /// Write `preset` (creating `presets/` if needed) and return its info.
    pub fn save(&self, preset: &Preset) -> Result<PresetInfo, PresetError> {
        let name = normalize_name(&preset.name).map_err(PresetError::InvalidName)?;
        let mut preset = preset.clone();
        preset.name = name.clone();
        preset.part = self.part;
        if self.part.is_some() {
            preset.parts = None;
        }
        let path = match self.find_path(&name) {
            Some(path) => path,
            None => self.free_path(&name),
        };
        let mut value = serde_json::to_value(&preset)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        value["chain"] = stripped_chain(&preset)?;
        let mut json = serde_json::to_vec_pretty(&value)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        json.push(b'\n');
        write_atomic(&path, &json)?;
        Ok(info_with_mtime(&preset, &path))
    }

    /// Remove `name`. Fails with `NotFound` when it does not exist.
    pub fn delete(&self, name: &str) -> Result<(), PresetError> {
        let name = normalize_name(name).map_err(PresetError::InvalidName)?;
        let (path, _) = self.find(&name)?;
        fs::remove_file(&path).map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                PresetError::NotFound(name.clone())
            } else {
                PresetError::Io(err)
            }
        })
    }

    /// Rename `from` to `to`, keeping the same chain.
    pub fn rename(&self, from: &str, to: &str) -> Result<PresetInfo, PresetError> {
        let from = normalize_name(from).map_err(PresetError::InvalidName)?;
        let to = normalize_name(to).map_err(PresetError::InvalidName)?;
        let (path, mut preset) = self.find(&from)?;
        if from == to {
            return Ok(info_with_mtime(&preset, &path));
        }
        if self.find_path(&to).is_some() {
            return Err(PresetError::Exists(to));
        }
        preset.name = to;
        let info = self.save(&preset)?;
        fs::remove_file(&path).map_err(PresetError::Io)?;
        Ok(info)
    }

    /// Files in the directory, sorted by file name, with their parsed preset.
    fn entries(&self) -> Vec<(PathBuf, Preset, PresetInfo)> {
        let read = match fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                log::warn!("cannot read {}: {err}", self.dir.display());
                return Vec::new();
            }
        };
        let mut entries = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()))
                .and_then(|text| {
                    let mut value: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|err| format!("{} is not a preset: {err}", path.display()))?;
                    let migrated = config::migrate_safety_modes(&mut value);
                    let preset = serde_json::from_value::<Preset>(value.clone())
                        .map_err(|err| format!("{} is not a preset: {err}", path.display()))?;
                    if migrated {
                        // Written back once, best effort (it migrates again
                        // on the next read otherwise).
                        let written = serde_json::to_vec_pretty(&value)
                            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
                            .and_then(|mut json| {
                                json.push(b'\n');
                                write_atomic(&path, &json)
                            });
                        match written {
                            Ok(()) => log::info!("preset {} migrated and saved", path.display()),
                            Err(err) => log::warn!(
                                "preset {} migrated but not saved ({err})",
                                path.display()
                            ),
                        }
                    }
                    Ok(preset)
                }) {
                Ok(mut preset) => {
                    // The directory decides the page.
                    preset.part = self.part;
                    let info = info_with_mtime(&preset, &path);
                    entries.push((path, preset, info));
                }
                Err(err) => log::warn!("{err}"),
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    fn find(&self, name: &str) -> Result<(PathBuf, Preset), PresetError> {
        self.entries()
            .into_iter()
            .find(|(_, preset, _)| preset.name == name)
            .map(|(path, preset, _)| (path, preset))
            .ok_or_else(|| PresetError::NotFound(name.to_string()))
    }

    fn find_path(&self, name: &str) -> Option<PathBuf> {
        self.entries()
            .into_iter()
            .find(|(_, preset, _)| preset.name == name)
            .map(|(path, _, _)| path)
    }

    /// First free `<slug>.json` (or `-2`, `-3`, …) for a new preset.
    fn free_path(&self, name: &str) -> PathBuf {
        let base = slug(name);
        let mut candidate = self.dir.join(format!("{base}.json"));
        let mut suffix = 1;
        while candidate.exists() {
            suffix += 1;
            candidate = self.dir.join(format!("{base}-{suffix}.json"));
        }
        candidate
    }
}

fn info_for(preset: &Preset, modified_unix: u64) -> PresetInfo {
    let parts = preset.covers();
    let (bands, convolution) = chain_stats(&preset.chain);
    let bands = if parts.contains(&PresetPart::Eq) {
        bands
    } else {
        0
    };
    let convolution = if parts.contains(&PresetPart::Convolution) {
        convolution
    } else {
        0
    };
    PresetInfo {
        name: preset.name.clone(),
        part: preset.part,
        parts,
        modified_unix,
        bands,
        convolution,
    }
}

fn info_with_mtime(preset: &Preset, path: &Path) -> PresetInfo {
    let modified_unix = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0)
        });
    info_for(preset, modified_unix)
}

/// Validate a preset read from disk or received over IPC before it is loaded:
/// the name and its chain (same rules as `config.json`, prefixed with
/// `preset`). Rational upsampler ratios depend on the rate policy a preset
/// is loaded under, so they are checked when it is loaded into the config.
pub fn validate(preset: &Preset) -> Result<(), String> {
    normalize_name(&preset.name)?;
    if preset.covers().is_empty() {
        return Err("a preset must hold at least one part".into());
    }
    config::validate_chain_at("preset.chain", &preset.chain, &[], 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{Band, EqProfile, FilterType, UpsamplerConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-presets-{}-{name}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chain(bands: usize) -> ChainConfig {
        ChainConfig {
            eq: Some(EqProfile {
                name: "test".into(),
                preamp_db: -1.0,
                bands: (0..bands)
                    .map(|n| Band {
                        kind: FilterType::Peaking,
                        freq_hz: 100.0 * (n as f64 + 1.0),
                        gain_db: 1.0,
                        q: 1.0,
                        enabled: true,
                    })
                    .collect(),
                ..Default::default()
            }),
            upsampler: UpsamplerConfig::default(),
            ..ChainConfig::neutral()
        }
    }

    #[test]
    fn names_are_trimmed_and_validated() {
        assert_eq!(normalize_name("  HD 650 \n").unwrap(), "HD 650");
        assert!(normalize_name("   ").is_err());
        assert!(normalize_name(&"a".repeat(65)).is_err());
        assert_eq!(normalize_name(&"a".repeat(64)).unwrap().len(), 64);
        assert!(normalize_name("bad\u{7}name").is_err());
    }

    #[test]
    fn slugs_are_safe_and_unique() {
        assert_eq!(slug("HD 650"), "hd-650");
        assert_eq!(slug("Rock/Pop * Live!"), "rock-pop-live");
        assert_eq!(slug("日本語"), "preset");
        assert_eq!(slug(&"x".repeat(200)).len(), MAX_SLUG_CHARS);
    }

    #[test]
    fn round_trip_list_load_delete() {
        let dir = temp_dir("roundtrip");
        let store = PresetStore::new(dir.join("presets"));
        assert!(store.list().is_empty());

        let a = store.save(&Preset::new("Zebra", chain(2))).unwrap();
        assert_eq!(a.name, "Zebra");
        assert_eq!(a.bands, 2);
        assert_eq!(a.convolution, 0);
        assert!(a.modified_unix > 0);
        let b = store.save(&Preset::new("alpha", chain(1))).unwrap();
        assert_eq!(b.name, "alpha");

        let names: Vec<String> = store.list().into_iter().map(|info| info.name).collect();
        assert_eq!(names, vec!["alpha", "Zebra"], "sorted by name");

        let loaded = store.load("Zebra").unwrap();
        assert_eq!(loaded.name, "Zebra");
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.chain, chain(2));
        assert!(matches!(
            store.load("missing"),
            Err(PresetError::NotFound(name)) if name == "missing"
        ));

        // Overwriting keeps the same file; names that slug alike get distinct
        // files and stay distinct presets.
        let path_before = dir.join("presets").join("zebra.json");
        assert!(path_before.exists());
        store.save(&Preset::new("Zebra", chain(3))).unwrap();
        assert!(path_before.exists());
        assert_eq!(store.load("Zebra").unwrap().chain, chain(3));
        store.save(&Preset::new("ZEBRA", chain(1))).unwrap();
        assert_eq!(store.list().len(), 3);
        assert_eq!(store.load("ZEBRA").unwrap().chain, chain(1));

        store.delete("Zebra").unwrap();
        assert!(store.load("Zebra").is_err());
        assert!(matches!(
            store.delete("Zebra"),
            Err(PresetError::NotFound(_))
        ));

        // No temp files left behind.
        assert!(fs::read_dir(dir.join("presets")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        }));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rename_moves_the_file_and_refuses_collisions() {
        let dir = temp_dir("rename");
        let store = PresetStore::new(dir.join("presets"));
        store.save(&Preset::new("One", chain(1))).unwrap();
        store.save(&Preset::new("Two", chain(2))).unwrap();

        let info = store.rename("One", "First").unwrap();
        assert_eq!(info.name, "First");
        assert_eq!(info.bands, 1);
        assert!(store.load("One").is_err());
        assert_eq!(store.load("First").unwrap().chain, chain(1));

        assert!(matches!(
            store.rename("First", "Two"),
            Err(PresetError::Exists(name)) if name == "Two"
        ));
        // An unknown source is a NotFound, and the target is untouched.
        assert!(matches!(
            store.rename("Nope", "Two"),
            Err(PresetError::NotFound(_))
        ));
        assert_eq!(store.load("Two").unwrap().chain, chain(2));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A preset saved with a safety-only mode reads as the natural one and
    /// is written back once.
    #[test]
    fn a_preset_with_a_safety_mode_migrates() {
        let dir = temp_dir("safety-preset");
        let store = PresetStore::new(dir.join("presets"));
        fs::create_dir_all(dir.join("presets")).unwrap();
        let path = dir.join("presets").join("old.json");
        let old = serde_json::json!({
            "name": "Old",
            "chain": {
                "crossfeed": {"mode": "bs2b_default", "level": "peak_safe"},
                "harmonics": {"enabled": true, "routing": "series_makeup", "layers": [
                    {"kind": "chebyshev", "preset": "triode", "levels_db": [-40.0, -58.0, null, null, null, null, null, null]}
                ]}
            }
        });
        fs::write(&path, old.to_string()).unwrap();
        let preset = store.load("Old").unwrap();
        assert_eq!(
            preset.chain.harmonics.as_ref().unwrap().routing,
            audivane_protocol::HarmonicsRouting::Series
        );
        assert!(preset.chain.crossfeed.is_some());
        let written = fs::read_to_string(&path).unwrap();
        assert!(!written.contains("peak_safe") && !written.contains("series_makeup"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn part_presets_live_in_their_own_directory_and_hold_their_part_only() {
        let dir = temp_dir("parts");
        let root = PresetStore::new(dir.join("presets"));
        let eq = root.scoped(Some(PresetPart::Eq));
        // Even a preset built as global becomes the directory's part.
        let info = eq.save(&Preset::new("Warm", chain(2))).unwrap();
        assert_eq!(info.part, Some(PresetPart::Eq));
        assert_eq!(info.parts, vec![PresetPart::Eq]);
        let path = dir.join("presets").join("eq").join("warm.json");
        let file: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file["part"], "eq");
        assert!(file.get("parts").is_none());
        assert_eq!(
            file["chain"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["eq"]
        );

        // A global one with two parts: the output part holds the output-side
        // stages (DC block and the off crossfeed/harmonics are left out).
        let mut global = Preset::new("Desk", chain(1));
        global.parts = Some(vec![PresetPart::Output, PresetPart::Eq]);
        let info = root.save(&global).unwrap();
        assert_eq!(info.part, None);
        assert_eq!(info.parts, vec![PresetPart::Eq, PresetPart::Output]);
        let file: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(dir.join("presets").join("desk.json")).unwrap(),
        )
        .unwrap();
        let mut keys: Vec<&String> = file["chain"].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["eq", "limiter", "output", "pitch"]);

        // Each store sees its own; `list_all` sees both, globals first.
        assert_eq!(root.list().len(), 1);
        assert_eq!(eq.list().len(), 1);
        let all: Vec<(Option<PresetPart>, String)> = root
            .list_all()
            .into_iter()
            .map(|info| (info.part, info.name))
            .collect();
        assert_eq!(
            all,
            vec![
                (None, "Desk".to_string()),
                (Some(PresetPart::Eq), "Warm".to_string())
            ]
        );
        assert!(root.load("Warm").is_err());
        assert_eq!(eq.load("Warm").unwrap().chain.eq, chain(2).eq);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn legacy_preset_migrates_and_counts_layered_bands() {
        let dir = temp_dir("legacy-preset");
        let presets_dir = dir.join("presets");
        fs::create_dir_all(&presets_dir).unwrap();
        let legacy = serde_json::json!({
            "version": 1,
            "name": "LegacySplit",
            "chain": {
                "eq": {
                    "name": "LegacySplit",
                    "preamp_db": -2.0,
                    "bands": [{"kind":"peaking","freq_hz":200.0,"gain_db":1.5,"q":1.0,"enabled":true}],
                    "right": {
                        "preamp_db": -1.0,
                        "bands": [{"kind":"peaking","freq_hz":500.0,"gain_db":-2.0,"q":1.0,"enabled":true}]
                    }
                }
            }
        });
        fs::write(
            presets_dir.join("legacysplit.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let store = PresetStore::new(&presets_dir);
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].bands, 2);

        let loaded = store.load("LegacySplit").unwrap();
        let eq = loaded.chain.eq.as_ref().unwrap();
        assert_eq!(eq.version, audivane_protocol::EQ_PROFILE_VERSION);
        assert!(eq.bands.is_empty());
        assert_eq!(eq.left.as_ref().unwrap().bands.len(), 1);
        assert_eq!(eq.right.as_ref().unwrap().bands.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }
}
