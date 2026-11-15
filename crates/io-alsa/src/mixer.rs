//! ALSA simple mixer selection and hardware playback volume/mute control.
//!
//! Runs strictly off the real-time thread: inspects the card of the configured
//! `hw:` device, picks the preferred playback volume control with a valid dB
//! range, and applies dB volume (plus playback switch mute when available).

use crate::error::Result;
use crate::reserve::card_index_from_device;
use alsa::Round;
use alsa::mixer::{MilliBel, Mixer, Selem, SelemChannelId, SelemId};
use audivane_protocol::ipc::{VOLUME_MAX_DB, VOLUME_MIN_DB};

/// Simple mixer element names tried in priority order before falling back to
/// a card's sole playback-volume element.
pub const PREFERRED_PLAYBACK_CONTROLS: &[&str] = &["Master", "PCM", "Headphone", "Speaker"];

/// Metadata for a simple mixer element needed to select a hardware volume control.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerElementCandidate {
    pub name: String,
    pub index: u32,
    pub has_playback_volume: bool,
    pub has_playback_switch: bool,
    pub db_range: Option<(f64, f64)>,
}

impl MixerElementCandidate {
    pub fn is_eligible(&self) -> bool {
        self.has_playback_volume
            && self
                .db_range
                .is_some_and(|(min, max)| min.is_finite() && max.is_finite() && min < max)
    }
}

/// Result of attempting to apply playback volume/mute to a card's ALSA mixer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerApplyOutcome {
    /// True when an ALSA playback volume element in dB was found and set.
    pub hardware_volume: bool,
    /// True when the chosen element also has a playback switch and it was set.
    pub hardware_mute: bool,
    /// Effective volume in dB (clamped/quantized to hardware range if hardware).
    pub volume_db: f64,
    pub min_db: f64,
    pub max_db: f64,
}

impl MixerApplyOutcome {
    pub fn digital(volume_db: f64) -> Self {
        Self {
            hardware_volume: false,
            hardware_mute: false,
            volume_db,
            min_db: VOLUME_MIN_DB,
            max_db: VOLUME_MAX_DB,
        }
    }
}

/// Pure mixer-element selection on a slice of candidates:
/// 1. Try `Master`, `PCM`, `Headphone`, `Speaker` in order (lowest mixer index first).
/// 2. Otherwise, if there is exactly one element with playback volume in dB, use it.
/// 3. Otherwise, return `None` (caller falls back to digital volume).
pub fn select_playback_element(candidates: &[MixerElementCandidate]) -> Option<usize> {
    for &preferred in PREFERRED_PLAYBACK_CONTROLS {
        if let Some((idx, _)) = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_eligible() && c.name == preferred)
            .min_by_key(|(_, c)| c.index)
        {
            return Some(idx);
        }
    }

    let mut eligible = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.is_eligible());
    let (only_idx, _) = eligible.next()?;
    if eligible.next().is_some() {
        return None;
    }
    Some(only_idx)
}

fn inspect_selem(selem: &Selem<'_>) -> Option<(SelemId, MixerElementCandidate)> {
    let id = selem.get_id();
    let name = id.get_name().ok()?.to_string();
    let index = id.get_index();
    let has_playback_volume = selem.has_playback_volume();
    let has_playback_switch = selem.has_playback_switch();

    let db_range = if has_playback_volume {
        let (min_mb, max_mb) = selem.get_playback_db_range();
        let (vmin, vmax) = selem.get_playback_volume_range();
        if min_mb.0 < max_mb.0 && vmin < vmax && selem.ask_playback_vol_db(vmin).is_ok() {
            Some(((min_mb.0 as f64) / 100.0, (max_mb.0 as f64) / 100.0))
        } else {
            None
        }
    } else {
        None
    };

    Some((
        id,
        MixerElementCandidate {
            name,
            index,
            has_playback_volume,
            has_playback_switch,
            db_range,
        },
    ))
}

fn first_playback_channel(selem: &Selem<'_>) -> SelemChannelId {
    for ch in SelemChannelId::all() {
        if selem.has_playback_channel(*ch) {
            return *ch;
        }
    }
    SelemChannelId::FrontLeft
}

/// Inspect the ALSA mixer on the card of `device` and, if a suitable playback
/// volume control exists, apply `volume_db` (and `muted` via its playback
/// switch if present).
///
/// Must NEVER be called on the real-time audio thread.
pub fn apply_playback_volume(
    device: &str,
    volume_db: f64,
    muted: bool,
) -> Result<MixerApplyOutcome> {
    let card = card_index_from_device(device)?;
    let mixer = Mixer::new(&format!("hw:{card}"), false)?;

    let mut ids = Vec::new();
    let mut candidates = Vec::new();
    for elem in mixer.iter() {
        let Some(selem) = Selem::new(elem) else {
            continue;
        };
        if let Some((id, cand)) = inspect_selem(&selem) {
            ids.push(id);
            candidates.push(cand);
        }
    }

    let Some(chosen_idx) = select_playback_element(&candidates) else {
        return Ok(MixerApplyOutcome::digital(volume_db));
    };

    let cand = &candidates[chosen_idx];
    let (min_db, max_db) = cand.db_range.expect("eligible candidate has db_range");
    let Some(selem) = mixer.find_selem(&ids[chosen_idx]) else {
        return Ok(MixerApplyOutcome::digital(volume_db));
    };

    let target_db = volume_db.clamp(min_db, max_db);
    let target_mb = MilliBel((target_db * 100.0).round() as i64);
    selem.set_playback_db_all(target_mb, Round::Floor)?;

    let ch = first_playback_channel(&selem);
    let applied_db = selem
        .get_playback_vol_db(ch)
        .map(|mb| ((mb.0 as f64) / 100.0).clamp(min_db, max_db))
        .unwrap_or(target_db);

    let hardware_mute = if cand.has_playback_switch {
        selem.set_playback_switch_all(if muted { 0 } else { 1 })?;
        true
    } else {
        false
    };

    Ok(MixerApplyOutcome {
        hardware_volume: true,
        hardware_mute,
        volume_db: applied_db,
        min_db,
        max_db,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elem(
        name: &str,
        index: u32,
        has_playback_volume: bool,
        has_playback_switch: bool,
        db_range: Option<(f64, f64)>,
    ) -> MixerElementCandidate {
        MixerElementCandidate {
            name: name.to_string(),
            index,
            has_playback_volume,
            has_playback_switch,
            db_range,
        }
    }

    #[test]
    fn selection_prefers_master_then_pcm_headphone_speaker() {
        let list = vec![
            elem("Speaker", 0, true, true, Some((-60.0, 0.0))),
            elem("Headphone", 0, true, true, Some((-60.0, 0.0))),
            elem("PCM", 0, true, false, Some((-51.0, 0.0))),
            elem("Master", 0, true, true, Some((-64.0, 0.0))),
        ];
        assert_eq!(select_playback_element(&list), Some(3));

        // Without Master -> PCM.
        let no_master = list[..3].to_vec();
        assert_eq!(select_playback_element(&no_master), Some(2));

        // Master present without dB volume is skipped in favor of Headphone.
        let master_no_db = vec![
            elem("Master", 0, true, true, None),
            elem("Speaker", 0, true, true, Some((-60.0, 0.0))),
            elem("Headphone", 0, true, true, Some((-60.0, 0.0))),
        ];
        assert_eq!(select_playback_element(&master_no_db), Some(2));

        // Lowest index wins when multiple elements share a preferred name.
        let multi_pcm = vec![
            elem("PCM", 1, true, false, Some((-51.0, 0.0))),
            elem("PCM", 0, true, false, Some((-51.0, 0.0))),
        ];
        assert_eq!(select_playback_element(&multi_pcm), Some(1));
    }

    #[test]
    fn selection_falls_back_to_sole_playback_element_or_none_when_ambiguous() {
        // Capture-only element ignored; sole playback DAC control wins.
        let single_dac = vec![
            elem("Mic", 0, false, false, None),
            elem("DAC Digital", 0, true, true, Some((-127.0, 0.0))),
        ];
        assert_eq!(select_playback_element(&single_dac), Some(1));

        // Two non-preferred playback volume elements -> ambiguous -> digital fallback (None).
        let ambiguous = vec![
            elem("Front", 0, true, true, Some((-60.0, 0.0))),
            elem("Surround", 0, true, true, Some((-60.0, 0.0))),
        ];
        assert_eq!(select_playback_element(&ambiguous), None);

        // Playback switch without playback volume is not eligible.
        let switch_only = vec![elem("Master", 0, false, true, None)];
        assert_eq!(select_playback_element(&switch_only), None);

        // Degenerate min == max dB range is not eligible.
        let flat_db = vec![elem("PCM", 0, true, false, Some((0.0, 0.0)))];
        assert_eq!(select_playback_element(&flat_db), None);
    }
}
