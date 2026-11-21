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

    /// A daemon at one level: answers status and guarded writes like the real one.
    struct FakeDaemon {
        level: RefCell<(String, u64, f64, bool)>,
        calls: RefCell<Vec<(String, Option<Value>)>>,
        guarded: bool,
    }

    impl FakeDaemon {
        fn new(db: f64) -> Self {
            Self {
                level: RefCell::new(("A".into(), 1, db, false)),
                calls: RefCell::default(),
                guarded: true,
            }
        }
        fn now(&self) -> Value {
            let l = self.level.borrow();
            vs(&l.0, l.1, l.2, l.3)
        }
        fn call(&self, method: &str, params: Option<Value>) -> Result<Value, CallError> {
            self.calls
                .borrow_mut()
                .push((method.into(), params.clone()));
            match method {
                "status" => Ok(status(self.now())),
                "set_volume_guarded" if !self.guarded => Err(CallError {
                    code: INVALID_REQUEST,
                    message: "unknown method".into(),
                }),
                "set_volume_guarded" => {
                    let p = params.unwrap();
                    let mut l = self.level.borrow_mut();
                    if p["if_instance"] != json!(l.0) {
                        return Err(CallError {
                            code: STALE_INSTANCE,
                            message: "stale".into(),
                        });
                    }
                    if p["if_revision"] != json!(l.1) {
                        return Err(CallError {
                            code: STALE_REVISION,
                            message: "stale".into(),
                        });
                    }
                    if let Some(db) = p["volume_db"].as_f64() {
                        l.2 = db;
                    }
                    if let Some(m) = p["muted"].as_bool() {
                        l.3 = m;
                    }
                    l.1 += 1;
                    drop(l);
                    Ok(self.now())
                }
                _ => panic!("unexpected {method}"),
            }
        }
        fn writes(&self) -> Vec<Value> {
            self.calls
                .borrow()
                .iter()
                .filter(|(m, _)| m == "set_volume_guarded")
                .map(|(_, p)| p.clone().unwrap())
                .collect()
        }
    }

    #[test]
    fn patch_follows_the_store_rules() {
        let l = Level::from_volume_status(&vs("A", 1, -20.0, false)).unwrap();
        assert_eq!(
            key_patch(&l, Action::Up, 1.0),
            Some(json!({"volume_db": -19.0}))
        );
        assert_eq!(
            key_patch(&l, Action::Down, 0.5),
            Some(json!({"volume_db": -20.5}))
        );
        assert_eq!(
            key_patch(&l, Action::Mute, 1.0),
            Some(json!({"muted": true}))
        );
        // The ceiling (max_db −6) caps a step, and at the ceiling nothing is sent.
        let top = Level {
            volume_db: -6.5,
            ..l.clone()
        };
        assert_eq!(
            key_patch(&top, Action::Up, 3.0),
            Some(json!({"volume_db": -6.0}))
        );
        let at = Level {
            volume_db: -6.0,
            ..l.clone()
        };
        assert_eq!(key_patch(&at, Action::Up, 1.0), None);
        // A level above the range only moves inward.
        let above = Level {
            volume_db: -3.0,
            ..l.clone()
        };
        assert_eq!(key_patch(&above, Action::Up, 1.0), None);
        assert_eq!(
            key_patch(&above, Action::Down, 1.0),
            Some(json!({"volume_db": -6.0}))
        );
        // The 0.1 dB grid.
        let odd = Level {
            volume_db: -20.04,
            ..l
        };
        assert_eq!(
            key_patch(&odd, Action::Up, 1.0),
            Some(json!({"volume_db": -19.0}))
        );
    }

    #[test]
    fn quick_presses_add_up_on_the_echo() {
        let d = FakeDaemon::new(-20.0);
        let mut t = Tracker::default();
        let call = |m: &str, p| d.call(m, p);
        t.observe_status(&status(d.now()));
        for _ in 0..3 {
            step(&mut t, &call, Action::Up, 1.0).unwrap();
        }
        let w = d.writes();
        assert_eq!(w.len(), 3);
        assert_eq!(w[2]["volume_db"], json!(-17.0));
        assert_eq!(w[2]["if_revision"], json!(3));
        // A status event older than our echo does not take the level back.
        t.observe_status(&status(vs("A", 2, -19.0, false)));
        assert_eq!(t.level().unwrap().volume_db, -17.0);
        // Mute writes only the mute.
        let osd = step(&mut t, &call, Action::Mute, 1.0).unwrap();
        assert_eq!(osd["muted"], json!(true));
        assert!(d.writes()[3].get("volume_db").is_none());
    }

    #[test]
    fn a_refused_write_reads_the_level_again_and_retries_once() {
        let d = FakeDaemon::new(-20.0);
        let mut t = Tracker::default();
        let call = |m: &str, p| d.call(m, p);
        t.observe_status(&status(d.now()));
        // The desktop moved the level meanwhile; we have not seen it.
        *d.level.borrow_mut() = ("A".into(), 5, -30.0, false);
        let osd = step(&mut t, &call, Action::Up, 1.0).unwrap();
        assert_eq!(osd["volume_db"], json!(-29.0));
        let w = d.writes();
        assert_eq!(w.len(), 2);
        assert_eq!(w[1]["if_revision"], json!(5));
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

    #[test]
    fn the_retry_rechecks_the_audio_path() {
        // Refused as stale; meanwhile the Direct sink took over on a DAC
        // without hardware volume: the retry is not sent, the note is shown.
        let mut digital = vs("A", 5, -20.0, false);
        digital["hardware"] = json!(false);
        let direct = json!({"active_sink": "direct", "dsp_enabled": true, "volume": digital});
        let (call, log) = scripted(vec![Err(STALE_REVISION), Ok(direct)]);
        let mut t = Tracker::default();
        let mut before = vs("A", 4, -20.0, false);
        before["hardware"] = json!(false);
        t.observe_status(&status(before));
        let osd = step(&mut t, &call, Action::Up, 1.0).unwrap();
        assert_eq!(osd["note"], json!("Bit-perfect: set the volume on the DAC"));
        assert_eq!(guarded_writes(&log), 1);
        // The same with Bypass (DSP off).
        let mut digital = vs("A", 5, -20.0, false);
        digital["hardware"] = json!(false);
        let bypass = json!({"active_sink": "dsp", "dsp_enabled": false, "volume": digital});
        let (call, log) = scripted(vec![Err(STALE_REVISION), Ok(bypass)]);
        let mut t = Tracker::default();
        let mut before = vs("A", 4, -20.0, false);
        before["hardware"] = json!(false);
        t.observe_status(&status(before));
        assert!(step(&mut t, &call, Action::Down, 1.0).unwrap()["note"].is_string());
        assert_eq!(guarded_writes(&log), 1);
    }

    #[test]
    fn a_second_refusal_drops_the_press() {
        let (call, log) = scripted(vec![
            Err(STALE_REVISION),
            Ok(status(vs("A", 5, -20.0, false))),
            Err(STALE_REVISION),
            Ok(status(vs("A", 6, -25.0, false))),
        ]);
        let mut t = Tracker::default();
        t.observe_status(&status(vs("A", 4, -20.0, false)));
        assert!(step(&mut t, &call, Action::Up, 1.0).is_none());
        assert_eq!(guarded_writes(&log), 2);
        // The level read last is the base for the next press.
        assert_eq!(t.level().unwrap().revision, 6);
    }

    #[test]
    fn no_level_no_write() {
        let d = FakeDaemon::new(-20.0);
        let call = |m: &str, p| d.call(m, p);
        // Nothing seen yet: the status is read first.
        let mut t = Tracker::default();
        assert!(step(&mut t, &call, Action::Down, 1.0).is_some());
        assert_eq!(d.writes().len(), 1);
        // An engine without revisions: nothing is written.
        let mut t = Tracker::default();
        t.observe_status(&json!({"volume": {"volume_db": -20.0, "muted": false}}));
        assert!(t.level().is_none());
        let old = |m: &str, _p: Option<Value>| -> Result<Value, CallError> {
            assert_eq!(m, "status");
            Ok(json!({"volume": {"volume_db": -20.0, "muted": false}}))
        };
        assert!(step(&mut t, &old, Action::Up, 1.0).is_none());
        // An engine without guarded writes: refused, never an unconditional write.
        let d = FakeDaemon {
            guarded: false,
            ..FakeDaemon::new(-20.0)
        };
        let call = |m: &str, p| d.call(m, p);
        let mut t = Tracker::default();
        assert!(step(&mut t, &call, Action::Up, 1.0).is_none());
        assert!(d.calls.borrow().iter().all(|(m, _)| m != "set_volume"));
    }

    #[test]
    fn bit_perfect_path_on_a_digital_dac_only_shows_the_note() {
        let d = FakeDaemon::new(-20.0);
        let call = |m: &str, p| d.call(m, p);
        let mut t = Tracker::default();
        let mut v = d.now();
        v["hardware"] = json!(false);
        t.observe_status(&json!({"active_sink": "direct", "dsp_enabled": true, "volume": v}));
        let osd = step(&mut t, &call, Action::Up, 1.0).unwrap();
        assert_eq!(osd["note"], json!("Bit-perfect: set the volume on the DAC"));
        assert!(d.writes().is_empty());
        // Mute still works there.
        step(&mut t, &call, Action::Mute, 1.0).unwrap();
        assert_eq!(d.writes().len(), 1);
    }

    #[test]
    fn another_engine_replaces_the_level() {
        let mut t = Tracker::default();
        t.observe_volume(&vs("A", 9, -20.0, false));
        t.observe_volume(&vs("B", 1, -40.0, false));
        assert_eq!(t.level().unwrap().instance, "B");
        t.clear();
        assert!(t.level().is_none());
    }
}
