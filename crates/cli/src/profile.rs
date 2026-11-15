//! Loading and saving EQ profiles: Equalizer APO text or protocol JSON.

use anyhow::{Context, Result};
use audivane_protocol::EqProfile;
use std::fs;
use std::path::Path;

/// Load a profile from an APO text file or a JSON file. JSON is detected by
/// extension or by a leading `{`.
pub fn load(path: &Path) -> Result<EqProfile> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read profile {}", path.display()))?;
    let looks_json = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        || text.trim_start().starts_with('{');
    if looks_json {
        let profile: EqProfile = serde_json::from_str(&text)
            .with_context(|| format!("cannot parse JSON profile {}", path.display()))?;
        return Ok(profile);
    }
    let parsed = audivane_dsp_core::eq_apo::parse(&text)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    for warning in &parsed.warnings {
        eprintln!("{}: {warning}", path.display());
    }
    Ok(parsed.profile)
}

/// Write a profile as pretty JSON.
pub fn save_json(path: &Path, profile: &EqProfile) -> Result<()> {
    let json = serde_json::to_string_pretty(profile).context("cannot serialize profile")?;
    fs::write(path, json + "\n").with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}
