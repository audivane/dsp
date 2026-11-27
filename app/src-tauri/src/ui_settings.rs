//! The desktop app's own settings (not the engine's): what a left click on
//! the tray icon does, the volume keys and the on-screen display. Kept in
//! `$XDG_CONFIG_HOME/audivane-dsp/app-settings.json`. Start-at-login is not
//! stored here: it is whether the autostart entry exists (`autostart.rs`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The volume-key steps the setting offers (dB), and the default.
pub const STEPS_DB: [f64; 7] = [0.1, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0];
pub const STEP_DEFAULT_DB: f64 = 1.0;

/// What a left click on the tray icon does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayClick {
    /// The small volume popup.
    #[default]
    Popup,
    /// The tray menu.
    Menu,
}

/// When the on-screen display shows the volume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsdMode {
    Off,
    /// After a volume key.
    #[default]
    Keys,
    /// After any change of the level (keys, the desktop, the CLI, a guard
    /// step), except while one of the app's own windows has the focus.
    Any,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub tray_click: TrayClick,
    /// Volume up/down/mute as global shortcuts. Off by default: the
    /// desktop's own binding must be removed first, or both act.
    pub volume_keys: bool,
    /// dB per volume-key press, 0.5 to 3.
    pub volume_step_db: f64,
    pub osd: OsdMode,
    /// Turn the wheel over the tray icon the other way round (for a tray
    /// host whose direction the app guesses wrong).
    pub reverse_wheel: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings {
            tray_click: TrayClick::default(),
            volume_keys: false,
            volume_step_db: STEP_DEFAULT_DB,
            osd: OsdMode::default(),
            reverse_wheel: false,
        }
    }
}

impl UiSettings {
    /// Bring every field into range (a hand-edited file, an old one).
    pub fn normalized(mut self) -> Self {
        let s = self.volume_step_db;
        self.volume_step_db = if s.is_finite() {
            // The nearest step the setting offers.
            STEPS_DB
                .into_iter()
                .min_by(|a, b| (a - s).abs().total_cmp(&(b - s).abs()))
                .unwrap_or(STEP_DEFAULT_DB)
        } else {
            STEP_DEFAULT_DB
        };
        self
    }
}

/// `<config home>/audivane-dsp/app-settings.json`.
pub fn path() -> Result<PathBuf, String> {
    crate::paths::config_home()
        .map(|h| crate::paths::config_dir_under(&h, "app-settings.json"))
        .ok_or_else(|| {
            "neither XDG_CONFIG_HOME nor HOME is set, so the app's settings cannot be kept".into()
        })
}

/// The stored settings; defaults when there is no file or it cannot be read.
pub fn load(path: &Path) -> UiSettings {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<UiSettings>(&bytes) {
            Ok(s) => s.normalized(),
            Err(e) => {
                log::warn!("{}: {e}; using the default app settings", path.display());
                UiSettings::default()
            }
        },
        Err(_) => UiSettings::default(),
    }
}

/// Write atomically (a temporary file renamed over the old one).
pub fn save(path: &Path, s: &UiSettings) -> Result<(), String> {
    let dir = path.parent().ok_or("the settings path has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| format!("cannot write in {}: {e}", dir.display()))?;
    serde_json::to_writer_pretty(&mut tmp, s).map_err(|e| e.to_string())?;
    tmp.persist(path)
        .map_err(|e| format!("cannot save {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_unknown_fields_fall_back() {
        let s: UiSettings = serde_json::from_str(r#"{"volume_keys": true, "future": 1}"#).unwrap();
        assert!(s.volume_keys);
        assert_eq!(s.tray_click, TrayClick::Popup);
        let s: UiSettings =
            serde_json::from_str(r#"{"tray_click": "menu", "osd": "any"}"#).unwrap();
        assert_eq!(s.tray_click, TrayClick::Menu);
        assert_eq!(s.osd, OsdMode::Any);
    }
}
