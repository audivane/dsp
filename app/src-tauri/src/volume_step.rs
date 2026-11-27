//! Volume keys and the tray wheel while the main window is closed to the
//! tray (its webview, and the store that normally makes the write, are
//! gone then).
//!
//! The same rules as the store's `volumeKey` (`app/src/lib/volumeKeys.ts`):
//! mute toggles only the mute, up and down move only the volume, kept in
//! the engine's range and ceiling and on the 0.1 dB grid, nothing is sent
//! when nothing would change, and the write is `set_volume_guarded` on the
//! newest level known (a status event or this module's own last echo), so
//! quick presses add up and no old level can land. A refused write (the
//! level changed under it) reads the status again and tries once more from
//! there; an engine without guarded writes gets no volume writes at all.

use crate::client::CallError;
pub use crate::shortcuts::KeyAction as Action;
use serde_json::{Value, json};

/// The protocol's volume bounds (`VOLUME_MIN_DB`, `VOLUME_MAX_DB`).
const MIN_DB: f64 = -120.0;
const MAX_DB: f64 = 0.0;
/// `RpcError` codes (docs/control-plane.md).
const INVALID_REQUEST: i32 = -32600;
const STALE_INSTANCE: i32 = 3;
const STALE_REVISION: i32 = 4;

/// A daemon call (the client's `call`).
pub type Call<'a> = dyn Fn(&str, Option<Value>) -> Result<Value, CallError> + 'a;

/// A `VolumeStatus` the app may write from: it has an instance and a
/// revision (an engine that predates them gets no volume writes).
#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    pub instance: String,
    pub revision: u64,
    pub volume_db: f64,
    pub muted: bool,
    pub min_db: f64,
    pub max_db: f64,
    /// The DAC has no hardware volume (`hardware: false`).
    pub digital: bool,
}

impl Level {
    pub fn from_volume_status(v: &Value) -> Option<Self> {
        let instance = v["instance"].as_str().filter(|s| !s.is_empty())?;
        Some(Self {
            instance: instance.to_string(),
            revision: v["revision"].as_u64()?,
            volume_db: v["volume_db"].as_f64()?,
            muted: v["muted"].as_bool()?,
            min_db: v["min_db"].as_f64().unwrap_or(MIN_DB),
            max_db: v["max_db"].as_f64().unwrap_or(MAX_DB),
            digital: !v["hardware"].as_bool().unwrap_or(true),
        })
    }

    /// `volumeRange`: what the engine reports, within the protocol bounds.
    fn range(&self) -> (f64, f64) {
        let lo = self.min_db.max(MIN_DB);
        let hi = self.max_db.min(MAX_DB);
        (lo, hi.max(lo))
    }

    /// What the on-screen display shows for this level.
    pub fn osd(&self, note: Option<&str>) -> Value {
        let (lo, hi) = self.range();
        json!({"volume_db": self.volume_db, "muted": self.muted, "min_db": lo, "max_db": hi, "note": note})
    }
}

/// The newest level seen: status events and our own write echoes, the
/// higher revision of the same engine instance winning (an echo can arrive
/// before or after the status event of the same change).
#[derive(Default, Debug)]
pub struct Tracker {
    level: Option<Level>,
    /// From the last status: the Direct sink plays, or DSP is off (Bypass).
    bit_perfect_path: bool,
}

impl Tracker {
    /// A `status` event's data (or a `status` reply).
    pub fn observe_status(&mut self, status: &Value) {
        self.bit_perfect_path =
            status["active_sink"] == "direct" || !status["dsp_enabled"].as_bool().unwrap_or(true);
        self.observe_volume(&status["volume"]);
    }

    /// A `VolumeStatus` (a status's `volume`, or a write's echo).
    pub fn observe_volume(&mut self, v: &Value) {
        let Some(new) = Level::from_volume_status(v) else {
            // An engine without revisions: nothing to write from.
            if v.is_object() {
                self.level = None;
            }
            return;
        };
        let keep_old = self
            .level
            .as_ref()
            .is_some_and(|old| old.instance == new.instance && old.revision > new.revision);
        if !keep_old {
            self.level = Some(new);
        }
    }

    /// The connection went down: the next engine may be another one.
    pub fn clear(&mut self) {
        self.level = None;
    }

    pub fn level(&self) -> Option<&Level> {
        self.level.as_ref()
    }
}

/// The change one press makes (`keyPatch`): `{"muted": …}` or
/// `{"volume_db": …}`, or None when nothing would change.
pub fn key_patch(level: &Level, action: Action, step_db: f64) -> Option<Value> {
    if action == Action::Mute {
        return Some(json!({"muted": !level.muted}));
    }
    let (lo, hi) = level.range();
    let raw = level.volume_db
        + if action == Action::Up {
            step_db
        } else {
            -step_db
        };
    let next = (raw.clamp(lo, hi) * 10.0).round() / 10.0;
    // A level outside the range (an older engine's report) only moves inward.
    let moved_wrong = (action == Action::Up && next < level.volume_db)
        || (action == Action::Down && next > level.volume_db);
    if next == level.volume_db || moved_wrong {
        return None;
    }
    Some(json!({"volume_db": next}))
}

/// One press. `call` is the daemon client's call. Returns the OSD payload,
/// or None while no writable level is known (nothing is sent then).
///
/// A write refused as `STALE_REVISION` (the level moved on the same
/// engine) reads the status and tries once more, but only against that
/// same engine instance and only if the audio path still takes a digital
/// level. `STALE_INSTANCE` (the engine was replaced) drops the press, like
/// the store drops a refused gesture: a mute must never toggle another
/// daemon.
pub fn step(tracker: &mut Tracker, call: &Call<'_>, action: Action, step_db: f64) -> Option<Value> {
    if tracker.level.is_none() {
        refresh(tracker, call);
    }
    let instance = tracker.level.as_ref()?.instance.clone();
    for attempt in 0..2 {
        let level = tracker.level.clone()?;
        if level.instance != instance {
            // Replaced between the refusal and the re-read: not this press's engine.
            return None;
        }
        if action != Action::Mute && level.digital && tracker.bit_perfect_path {
            return Some(level.osd(Some("Bit-perfect: set the volume on the DAC")));
        }
        let Some(patch) = key_patch(&level, action, step_db) else {
            return Some(level.osd(None));
        };
        let mut params = patch;
        params["if_instance"] = json!(level.instance);
        params["if_revision"] = json!(level.revision);
        match call("set_volume_guarded", Some(params)) {
            Ok(echo) => {
                tracker.observe_volume(&echo);
                return tracker.level.as_ref().map(|l| l.osd(None));
            }
            Err(e) if e.code == STALE_REVISION && attempt == 0 => {
                // The level moved under us: start again from the current one.
                refresh(tracker, call);
            }
            Err(e) if e.code == STALE_INSTANCE || e.code == STALE_REVISION => {
                // Dropped; the level is read again for the next press.
                refresh(tracker, call);
                return None;
            }
            Err(e) => {
                if e.code != INVALID_REQUEST {
                    log::warn!("volume key: {}", e.message);
                }
                // Never an unconditional write; read the level again next time.
                tracker.level = None;
                return None;
            }
        }
    }
    None
}

fn refresh(tracker: &mut Tracker, call: &Call<'_>) {
    match call("status", None) {
        Ok(st) => tracker.observe_status(&st),
        Err(_) => tracker.level = None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn vs(instance: &str, revision: u64, db: f64, muted: bool) -> Value {
        json!({"instance": instance, "revision": revision, "volume_db": db, "muted": muted,
               "min_db": -120.0, "max_db": -6.0, "hardware": true, "balance_db": 0.0})
    }

    fn status(v: Value) -> Value {
        json!({"active_sink": "dsp", "dsp_enabled": true, "volume": v})
    }

    type CallLog = std::rc::Rc<RefCell<Vec<(String, Option<Value>)>>>;

    /// Scripted replies, in order; records every call.
    fn scripted(
        replies: Vec<Result<Value, i32>>,
    ) -> (
        impl Fn(&str, Option<Value>) -> Result<Value, CallError>,
        CallLog,
    ) {
        let log = std::rc::Rc::new(RefCell::new(Vec::new()));
        let replies = RefCell::new(std::collections::VecDeque::from(replies));
        let l = log.clone();
        let call = move |m: &str, p: Option<Value>| {
            l.borrow_mut().push((m.to_string(), p));
            replies
                .borrow_mut()
                .pop_front()
                .expect("an unexpected call")
                .map_err(|code| CallError {
                    code,
                    message: "refused".into(),
                })
        };
        (call, log)
    }

    fn guarded_writes(log: &RefCell<Vec<(String, Option<Value>)>>) -> usize {
        log.borrow()
            .iter()
            .filter(|(m, _)| m == "set_volume_guarded")
            .count()
    }

    #[test]
    fn a_replaced_daemon_never_gets_the_retry() {
        // Refused as stale on engine A, and the re-read finds engine B: the
        // mute is dropped instead of toggling B.
        let (call, log) = scripted(vec![
            Err(STALE_REVISION),
            Ok(status(vs("B", 1, -20.0, false))),
        ]);
        let mut t = Tracker::default();
        t.observe_status(&status(vs("A", 4, -20.0, false)));
        assert!(step(&mut t, &call, Action::Mute, 1.0).is_none());
        assert_eq!(guarded_writes(&log), 1);
        assert_eq!(t.level().unwrap().instance, "B");
        // Refused as another instance outright: no retry either.
        let (call, log) = scripted(vec![
            Err(STALE_INSTANCE),
            Ok(status(vs("B", 1, -20.0, false))),
        ]);
        let mut t = Tracker::default();
        t.observe_status(&status(vs("A", 4, -20.0, false)));
        assert!(step(&mut t, &call, Action::Up, 1.0).is_none());
        assert_eq!(guarded_writes(&log), 1);
    }
}
