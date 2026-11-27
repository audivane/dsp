//! Where the master volume goes: hardware mixer, digital `f64` gain, or both.
//!
//! [`plan`] is pure; [`apply`] runs it against a mixer function (the real
//! ALSA mixer, or a mock in tests) on the builder thread, never on the audio
//! thread.

use audivane_io_alsa::MixerApplyOutcome;
use audivane_protocol::ipc::{VOLUME_MAX_DB, VOLUME_MIN_DB, VolumeConfig, VolumeMode};

/// `(device, dB, mute)`: set the hardware control (dB clamped to its range;
/// `mute: None` leaves the switch alone). Returns what was applied, or a
/// digital outcome when the card has no usable control.
pub type MixerFn = dyn Fn(&str, f64, Option<bool>) -> MixerApplyOutcome + Send + Sync;

/// Level differences below this are no change.
pub const EPS_DB: f64 = 0.001;

/// One hardware write.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HardwareWrite {
    /// Requested level, dB; clamped by the mixer. `INFINITY` = its maximum.
    pub db: f64,
    pub mute: Option<bool>,
}

/// What [`plan`] wants done before the hardware answers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// Master volume after the ceiling.
    pub target_db: f64,
    pub hardware: Option<HardwareWrite>,
    /// The hardware takes the whole volume (no digital residue).
    pub hardware_is_volume: bool,
}

/// What is actually in force after [`apply`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Applied {
    /// Effective master volume (target, or the quantized hardware level).
    pub volume_db: f64,
    /// Digital master gain on the DSP path, dB (0 = unity).
    pub software_db: f64,
    /// Level the hardware is at, when it was written.
    pub hardware_db: Option<f64>,
    /// The hardware carries (part of) the master volume.
    pub hardware_volume: bool,
    /// The hardware switch carries the mute.
    pub hardware_mute: bool,
    /// `hardware`/`hybrid` asked for, but the card has no usable control.
    pub hardware_unavailable: bool,
    pub min_db: f64,
    pub max_db: f64,
}

/// The master volume after the ceiling.
pub fn target_db(cfg: &VolumeConfig) -> f64 {
    let v = cfg.volume_db.clamp(VOLUME_MIN_DB, VOLUME_MAX_DB);
    cfg.max_volume_db.map_or(v, |c| v.min(c))
}

/// The mixer level to leave behind when the engine lets go of the DAC (device
/// release, shutdown, and `audivane-dsp --volume-failsafe` from the unit's
/// `ExecStopPost=` after a crash), from the effective gain split: `software`
/// with a fixed level and `hybrid` keep the card above what is heard and
/// attenuate digitally; `auto`/`hardware` below the card's floor leave it at its
/// minimum with the rest digital. This brings the card down to the effective
/// volume (the mixer clamps it into range, so below the range it ends at the
/// card's minimum, and [`leave_safe`] then mutes the switch). `card_db`: the
/// level the card is at, on its quietest channel. Every mode needs it: a card at
/// or below the heard level, or one whose level cannot be read, is left alone,
/// since the write could raise it (the caller logs
/// [`failsafe_blocked_by_unknown_level`]). `None` when the mixer already carries
/// the volume or is not ours to touch.
pub fn failsafe_write(cfg: &VolumeConfig, card_db: Option<f64>) -> Option<HardwareWrite> {
    let plan = plan(cfg);
    let hw = plan.hardware?;
    // The write below can only lower the card: it is above the target, and
    // the mixer clamps the target at most up to its floor.
    if card_db? <= plan.target_db + HW_READBACK_TOL_DB {
        return None;
    }
    if !plan.hardware_is_volume && hw.db <= plan.target_db + EPS_DB {
        return None;
    }
    Some(HardwareWrite {
        db: plan.target_db,
        mute: None,
    })
}

/// Result of reading a card's hardware volume control for the failsafe.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardLevel {
    /// The card has a usable volume control; its quietest playback channel (dB).
    Known(f64),
    /// The card has no usable hardware volume control (nothing to lower).
    NoControl,
    /// A control may exist, but its level could not be read.
    Unknown,
}

impl CardLevel {
    /// The level in dB, when known.
    pub fn db(self) -> Option<f64> {
        match self {
            Self::Known(db) => Some(db),
            Self::NoControl | Self::Unknown => None,
        }
    }
}

/// The failsafe would have lowered the card but its level is unknown (its
/// readback failed): nothing was written, which the caller should say.
/// Returns `false` when the card has no usable volume control
/// ([`CardLevel::NoControl`]): there is nothing to protect.
pub fn failsafe_blocked_by_unknown_level(cfg: &VolumeConfig, card: CardLevel) -> bool {
    matches!(card, CardLevel::Unknown) && failsafe_write(cfg, Some(f64::INFINITY)).is_some()
}

/// After writing a failsafe level: the card stopped at its minimum above
/// the heard level, so its switch should mute it (when it has one).
pub fn failsafe_needs_mute(write_db: f64, outcome: &MixerApplyOutcome) -> bool {
    outcome.hardware_volume && outcome.volume_db > write_db + HW_READBACK_TOL_DB
}

/// Write the failsafe level with `mixer`, muting the switch when the card cannot
/// go that low. `card_db` is the card's level, when known (see
/// [`failsafe_write`]). `switch_on` is the switch as it was before (`None`:
/// unknown or no switch): only a switch that was on is muted, so a mute is only
/// ever recorded (and later undone) when we caused it. Returns the level
/// written, the outcome, and whether we muted the switch.
///
/// `note_mute` is called with `Some(level)` just before the switch is muted, so
/// the mute is recorded before it can happen (a crash in between leaves a
/// record, never an unrecorded mute; without the card's state it is never
/// undone, see [`super::failsafe::MutedCard::left`]), and with `None` when the
/// mute then did not take, to forget it. The caller records the card's state
/// once the mute took.
pub fn leave_safe<F>(
    cfg: &VolumeConfig,
    device: &str,
    mixer: &F,
    card_db: Option<f64>,
    switch_on: Option<bool>,
    note_mute: &mut dyn FnMut(Option<f64>),
) -> Option<(f64, MixerApplyOutcome, bool)>
where
    F: Fn(&str, f64, Option<bool>) -> MixerApplyOutcome + ?Sized,
{
    let write = failsafe_write(cfg, card_db)?;
    let outcome = mixer(device, write.db, write.mute);
    if switch_on == Some(true) && failsafe_needs_mute(write.db, &outcome) {
        note_mute(Some(write.db));
        let muted = mixer(device, write.db, Some(true));
        if muted.hardware_mute {
            return Some((write.db, muted, true));
        }
        note_mute(None);
    }
    Some((write.db, outcome, false))
}

pub fn plan(cfg: &VolumeConfig) -> Plan {
    let target_db = target_db(cfg);
    let (hardware, hardware_is_volume) = match cfg.mode {
        VolumeMode::Auto | VolumeMode::Hardware => (
            Some(HardwareWrite {
                db: target_db,
                mute: Some(cfg.muted),
            }),
            true,
        ),
        VolumeMode::Software => (
            cfg.hardware_db.map(|db| HardwareWrite { db, mute: None }),
            false,
        ),
        // The hardware sits at its preferred level for every volume at or
        // below it, and only goes up for louder ones.
        VolumeMode::Hybrid => (
            Some(HardwareWrite {
                // Validation requires `hardware_db` for hybrid; without one
                // (a hand-edited file) the hardware takes the whole volume.
                db: cfg.hardware_db.unwrap_or(target_db).max(target_db),
                mute: Some(cfg.muted),
            }),
            false,
        ),
    };
    Plan {
        target_db,
        hardware,
        hardware_is_volume,
    }
}

/// Turn a plan plus the mixer's answer into what is in force.
pub fn resolve(cfg: &VolumeConfig, plan: &Plan, hw: Option<MixerApplyOutcome>) -> Applied {
    let ceiling_max = cfg
        .max_volume_db
        .unwrap_or(VOLUME_MAX_DB)
        .min(VOLUME_MAX_DB);
    let hw = hw.filter(|o| o.hardware_volume);
    let wants_hardware = matches!(cfg.mode, VolumeMode::Hardware | VolumeMode::Hybrid);
    let digital = |target: f64| Applied {
        volume_db: target,
        software_db: target,
        hardware_db: None,
        hardware_volume: false,
        hardware_mute: false,
        hardware_unavailable: wants_hardware,
        min_db: VOLUME_MIN_DB,
        max_db: ceiling_max,
    };
    match (cfg.mode, hw) {
        // Below the control's range (or rounded above the target) the
        // digital part takes the rest: never louder than asked.
        (VolumeMode::Auto | VolumeMode::Hardware, Some(o)) => Applied {
            volume_db: o.volume_db + residue_db(plan.target_db, o.volume_db),
            software_db: residue_db(plan.target_db, o.volume_db),
            hardware_db: Some(o.volume_db),
            hardware_volume: true,
            hardware_mute: o.hardware_mute,
            hardware_unavailable: false,
            // Below the card's floor the digital part goes on: the range
            // the volume can take starts at the engine's minimum, never at
            // the card's (a step down must never be clamped up to it).
            min_db: VOLUME_MIN_DB,
            max_db: o.max_db.min(ceiling_max),
        },
        (VolumeMode::Software, Some(o)) => Applied {
            hardware_db: Some(o.volume_db),
            hardware_unavailable: false,
            ..digital(plan.target_db)
        },
        (VolumeMode::Hybrid, Some(o)) => {
            // Never boost digitally: when even the hardware maximum is below
            // the target, the target is out of reach.
            let software_db = residue_db(plan.target_db, o.volume_db);
            Applied {
                volume_db: o.volume_db + software_db,
                software_db,
                hardware_db: Some(o.volume_db),
                hardware_volume: true,
                hardware_mute: o.hardware_mute,
                hardware_unavailable: false,
                min_db: VOLUME_MIN_DB,
                max_db: ceiling_max,
            }
        }
        (_, None) => digital(plan.target_db),
    }
}

/// How far above the target the card may read back from rounding alone:
/// the mixer asks for the target rounded to 0.01 dB (up to 0.005 dB high),
/// rounds down to a card step (never higher), and reports in 0.01 dB units
/// (a 1/256 dB USB step reads up to 0.01 dB high). A coarse step cannot put
/// the card above the target, so half a step is not needed here.
pub const HW_READBACK_TOL_DB: f64 = 0.02;

/// The digital part left after the card's level: never a boost, and a
/// residue within [`HW_READBACK_TOL_DB`] is the mixer's rounding, not a
/// level, so it stays unity (Direct bit-perfect).
fn residue_db(target_db: f64, hardware_db: f64) -> f64 {
    let residue = (target_db - hardware_db).min(0.0);
    if residue > -HW_READBACK_TOL_DB {
        0.0
    } else {
        residue
    }
}

/// Plan, write the hardware (when the plan wants it), and resolve.
pub fn apply<F>(cfg: &VolumeConfig, device: &str, mixer: &F) -> Applied
where
    F: Fn(&str, f64, Option<bool>) -> MixerApplyOutcome + ?Sized,
{
    let plan = plan(cfg);
    let outcome = plan.hardware.map(|w| mixer(device, w.db, w.mute));
    resolve(cfg, &plan, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A card with a `-64..=0` dB control in 0.5 dB steps (rounding down)
    /// and a switch; records every write.
    fn card(
        log: &Mutex<Vec<(f64, Option<bool>)>>,
    ) -> impl Fn(&str, f64, Option<bool>) -> MixerApplyOutcome + '_ {
        move |_dev, db, mute| {
            log.lock().unwrap().push((db, mute));
            let clamped = db.clamp(-64.0, 0.0);
            MixerApplyOutcome {
                hardware_volume: true,
                hardware_mute: mute.is_some(),
                volume_db: (clamped * 2.0).floor() / 2.0,
                min_db: -64.0,
                max_db: 0.0,
            }
        }
    }

    fn none(_dev: &str, db: f64, _mute: Option<bool>) -> MixerApplyOutcome {
        MixerApplyOutcome::digital(db)
    }

    fn cfg(mode: VolumeMode, volume_db: f64) -> VolumeConfig {
        VolumeConfig {
            volume_db,
            mode,
            ..VolumeConfig::default()
        }
    }

    #[test]
    fn hybrid_without_a_level_never_uses_the_card_maximum() {
        let log = Mutex::new(Vec::new());
        let a = apply(&cfg(VolumeMode::Hybrid, -40.0), "hw:0", &card(&log));
        assert_eq!(a.hardware_db, Some(-40.0));
        assert_eq!(a.software_db, 0.0);
    }

    #[test]
    fn auto_uses_hardware_then_digital() {
        let log = Mutex::new(Vec::new());
        let a = apply(&cfg(VolumeMode::Auto, -20.3), "hw:0", &card(&log));
        assert!(a.hardware_volume && a.hardware_mute);
        assert_eq!(a.volume_db, -20.5);
        assert_eq!(a.software_db, 0.0);
        assert_eq!(log.lock().unwrap()[0], (-20.3, Some(false)));

        let a = apply(&cfg(VolumeMode::Auto, -20.3), "hw:0", &none);
        assert!(!a.hardware_volume && !a.hardware_unavailable);
        assert_eq!(a.software_db, -20.3);
    }

    /// Every hybrid transition is never louder than the louder of the two
    /// settings, whatever order the hardware and the (later-arriving)
    /// digital part take effect in.
    #[test]
    fn hybrid_transitions_never_overshoot() {
        let log = Mutex::new(Vec::new());
        let mixer = card(&log);
        let mut levels = vec![-120.0, -60.0, -30.0, -10.5, -10.0, -9.5, -4.0, -0.5, 0.0];
        levels.extend((0..40).map(|i| -f64::from(i) * 1.37));
        for &pref in &[-10.0, -3.0, 0.0] {
            for &from in &levels {
                for &to in &levels {
                    let mut c = cfg(VolumeMode::Hybrid, from);
                    c.hardware_db = Some(pref);
                    let a = apply(&c, "hw:0", &mixer);
                    c.volume_db = to;
                    let b = apply(&c, "hw:0", &mixer);
                    let loudest = a.volume_db.max(b.volume_db) + 1e-9;
                    let hw_first = b.hardware_db.unwrap() + a.software_db;
                    let sw_first = a.hardware_db.unwrap() + b.software_db;
                    assert!(
                        hw_first <= loudest && sw_first <= loudest,
                        "pref {pref}: {from} -> {to}: {hw_first} / {sw_first} > {loudest}"
                    );
                }
            }
        }
    }

    #[test]
    fn ceiling_bounds_every_mode() {
        let log = Mutex::new(Vec::new());
        for mode in [
            VolumeMode::Auto,
            VolumeMode::Software,
            VolumeMode::Hardware,
            VolumeMode::Hybrid,
        ] {
            let mut c = cfg(mode, -2.0);
            c.max_volume_db = Some(-15.0);
            c.hardware_db = Some(-6.0);
            for a in [apply(&c, "hw:0", &card(&log)), apply(&c, "hw:0", &none)] {
                assert!(a.volume_db <= -15.0, "{mode:?}: {}", a.volume_db);
                assert!(a.max_db <= -15.0, "{mode:?}: max {}", a.max_db);
            }
        }
    }

    /// `auto`/`hardware`: the card carries the volume, except below its
    /// floor, where it is louder than what is heard. Only then is there
    /// something to do (the write clamps at the floor, never a raise; the
    /// switch gets muted). An unknown card level is left alone.
    #[test]
    fn failsafe_in_hardware_modes_acts_only_below_the_floor() {
        for mode in [VolumeMode::Auto, VolumeMode::Hardware] {
            let c = cfg(mode, -90.0);
            assert_eq!(failsafe_write(&c, None), None, "{mode:?}");
            assert_eq!(failsafe_write(&c, Some(-90.0)), None, "{mode:?}");
            let w = failsafe_write(&c, Some(-64.0)).unwrap();
            assert_eq!((w.db, w.mute), (-90.0, None), "{mode:?}");
            // The mixer's rounding is not a floor.
            assert_eq!(failsafe_write(&c, Some(-89.99)), None, "{mode:?}");
            let log = Mutex::new(Vec::new());
            let (db, outcome, muted) = leave_safe(
                &c,
                "hw:0",
                &card(&log),
                Some(-64.0),
                Some(true),
                &mut |_| {},
            )
            .unwrap();
            assert_eq!(db, -90.0);
            assert_eq!(outcome.volume_db, -64.0);
            assert!(muted, "{mode:?}");
            // The card already quieter than the stored level (a lag in
            // persisting it): never raised.
            assert_eq!(failsafe_write(&cfg(mode, -20.0), Some(-40.0)), None);
        }
        // Every mode: a card known to be (on its quietest channel) at or
        // below the heard level is not written; the write would raise it.
        for mode in [VolumeMode::Software, VolumeMode::Hybrid] {
            let mut c = cfg(mode, -20.0);
            c.hardware_db = Some(0.0);
            assert_eq!(failsafe_write(&c, Some(-45.0)), None, "{mode:?}");
            assert_eq!(failsafe_write(&c, Some(-10.0)).unwrap().db, -20.0);
            // The card's level unknown (its readback failed): it might be
            // below the heard level already, so nothing is written.
            assert_eq!(failsafe_write(&c, None), None, "{mode:?}");
            let log = Mutex::new(Vec::new());
            assert_eq!(
                leave_safe(&c, "hw:0", &card(&log), None, Some(true), &mut |_| {}),
                None
            );
            assert!(log.lock().unwrap().is_empty(), "{mode:?}: card written");
        }
    }
}
