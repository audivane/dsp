//! Volume guard: what a volume change from outside Audivane is allowed to do.
//!
//! Pure decision logic for the sink volume (`sink_volume: follow`) and app
//! streams, plus the log of blocked changes the UI shows. The service and
//! the graph watcher apply the decisions; nothing here touches PipeWire or
//! the engine.

use audivane_protocol::ipc::{
    BlockReason, BlockedVolumeChange, GuardMode, VOLUME_MAX_DB, VOLUME_MIN_DB, VolumeConfig,
    VolumeGuard, VolumeSource,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Changes smaller than this are no change (an `f32` round trip, rounding).
pub const EPS_DB: f64 = 0.002;
/// How often a pending slew takes a step.
pub const SLEW_TICK: Duration = Duration::from_millis(50);

/// Blocked/reduced changes since start, for `Status.volume`.
#[derive(Debug, Default)]
pub struct VolumeEventLog {
    inner: Mutex<(u64, Option<BlockedVolumeChange>)>,
    /// Per source: when it was last logged, and how many since then were not.
    logged: Mutex<HashMap<String, (Instant, u64)>>,
}

/// One log line per source at most this often; the rest are counted.
pub const LOG_INTERVAL: Duration = Duration::from_secs(1);

impl VolumeEventLog {
    pub fn record(
        &self,
        source: VolumeSource,
        requested_db: f64,
        applied_db: f64,
        reason: BlockReason,
    ) {
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let what = describe(&source);
        if let Some(skipped) = self.should_log(&what, Instant::now()) {
            let more = if skipped > 0 {
                format!("; {skipped} more since the last report")
            } else {
                String::new()
            };
            log::info!(
                "volume guard: {what} asked for {requested_db:.2} dB, applied {applied_db:.2} dB ({}{more})",
                reason_text(reason)
            );
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.0 += 1;
            inner.1 = Some(BlockedVolumeChange {
                source,
                requested_db,
                applied_db,
                reason,
                at_ms,
            });
        }
    }

    /// `Some(skipped since the last line)` when `source` may be logged now.
    fn should_log(&self, source: &str, now: Instant) -> Option<u64> {
        let mut logged = self.logged.lock().ok()?;
        match logged.get_mut(source) {
            Some((at, skipped)) if now.saturating_duration_since(*at) < LOG_INTERVAL => {
                *skipped += 1;
                None
            }
            Some((at, skipped)) => {
                let n = *skipped;
                *at = now;
                *skipped = 0;
                Some(n)
            }
            None => {
                if logged.len() > 256 {
                    logged.clear();
                }
                logged.insert(source.to_string(), (now, 0));
                Some(0)
            }
        }
    }

    /// `(count, last)`.
    pub fn snapshot(&self) -> (u64, Option<BlockedVolumeChange>) {
        self.inner
            .lock()
            .map(|inner| inner.clone())
            .unwrap_or_default()
    }
}

fn describe(source: &VolumeSource) -> String {
    match source {
        VolumeSource::Sink { sink } => format!("sink {sink:?}"),
        VolumeSource::Stream { node, app } => {
            format!(
                "stream {node} ({})",
                app.as_deref().unwrap_or("unknown app")
            )
        }
        VolumeSource::Control => "set_volume".into(),
    }
}

fn reason_text(reason: BlockReason) -> &'static str {
    match reason {
        BlockReason::Locked => "locked",
        BlockReason::Capped => "capped",
        BlockReason::Slewed => "rising gradually",
        BlockReason::DirectBitPerfect => "Direct stays at unity",
    }
}

/// The ceiling for the master volume.
pub fn ceiling_db(cfg: &VolumeConfig) -> f64 {
    cfg.max_volume_db
        .unwrap_or(VOLUME_MAX_DB)
        .min(VOLUME_MAX_DB)
}

/// What to do with one requested level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decision {
    /// Level to apply now.
    pub apply_db: f64,
    /// Why it differs from the request, if it does.
    pub blocked: Option<BlockReason>,
}

/// A rise in progress.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Slew {
    target_db: f64,
    last: Instant,
}

/// Guard state for one controlled level (the sink, or one stream).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LevelGuard {
    slew: Option<Slew>,
}

impl LevelGuard {
    pub fn is_slewing(&self) -> bool {
        self.slew.is_some()
    }

    pub fn cancel(&mut self) {
        self.slew = None;
    }

    /// Decide a request from `current_db` to `requested_db`, bounded by
    /// `cap_db`.
    pub fn decide(
        &mut self,
        guard: &VolumeGuard,
        cap_db: f64,
        current_db: f64,
        requested_db: f64,
        now: Instant,
    ) -> Decision {
        let capped = requested_db > cap_db + EPS_DB;
        let wanted = requested_db.clamp(VOLUME_MIN_DB, cap_db.max(VOLUME_MIN_DB));
        let cap_reason = capped.then_some(BlockReason::Capped);
        match guard.mode {
            GuardMode::Off => {
                self.slew = None;
                Decision {
                    apply_db: wanted,
                    blocked: cap_reason,
                }
            }
            GuardMode::Lock => {
                self.slew = None;
                let changed = (requested_db - current_db).abs() > EPS_DB;
                Decision {
                    apply_db: current_db,
                    blocked: changed.then_some(BlockReason::Locked),
                }
            }
            GuardMode::Limit => {
                let rate = guard.max_rise_db_per_s;
                if (wanted - current_db).abs() <= EPS_DB {
                    // No change (e.g. our own step echoing back): a rise in
                    // progress continues. A request above the ceiling while
                    // already at it is still capped: the sink, which shows
                    // the request, must be put back to the level in force.
                    return Decision {
                        apply_db: current_db,
                        blocked: cap_reason,
                    };
                }
                if wanted <= current_db || rate <= 0.0 {
                    self.slew = None;
                    return Decision {
                        apply_db: wanted,
                        blocked: cap_reason,
                    };
                }
                // A re-assert while rising only moves the target: resetting
                // the step clock would stall the rise.
                let last = self.slew.map_or(now, |s| s.last);
                self.slew = Some(Slew {
                    target_db: wanted,
                    last,
                });
                Decision {
                    apply_db: current_db,
                    blocked: Some(cap_reason.unwrap_or(BlockReason::Slewed)),
                }
            }
        }
    }

    /// Next step of a rise, at most `max_rise_db_per_s` since the last step.
    /// A step smaller than `min_step_db` (short of the target) waits for the
    /// next tick, so a rise on a hardware mixer moves in whole card steps
    /// instead of rewriting the mixer every tick. Waiting never runs ahead
    /// of the rate: the step is still measured from the last one applied.
    pub fn tick(
        &mut self,
        guard: &VolumeGuard,
        current_db: f64,
        min_step_db: f64,
        now: Instant,
    ) -> Option<f64> {
        let slew = self.slew.as_mut()?;
        let dt = now.saturating_duration_since(slew.last).as_secs_f64();
        if dt <= 0.0 {
            return None;
        }
        // Rate 0 ("only the cap", possibly set while this rise was under
        // way): finish at once instead of stalling at the current level.
        let next = if guard.max_rise_db_per_s <= 0.0 {
            slew.target_db
        } else {
            (current_db + guard.max_rise_db_per_s * dt).min(slew.target_db)
        };
        if next >= slew.target_db - EPS_DB {
            let target = slew.target_db;
            self.slew = None;
            return Some(target);
        }
        if next - current_db < min_step_db - EPS_DB {
            return None;
        }
        slew.last = now;
        Some(next)
    }
}

/// Smallest step of a guarded rise: modes that move the card's mixer step
/// in whole dB (a typical mixer step is 0.5 or 1 dB, and each step is a
/// mixer write); digital volume is ramped by the engine, so any step will
/// do.
pub fn slew_step_db(mode: audivane_protocol::ipc::VolumeMode, hardware: bool) -> f64 {
    use audivane_protocol::ipc::VolumeMode;
    match mode {
        VolumeMode::Auto | VolumeMode::Hardware if hardware => HW_SLEW_STEP_DB,
        _ => 0.0,
    }
}

/// See [`slew_step_db`].
pub const HW_SLEW_STEP_DB: f64 = 1.0;

/// How long a new stream may settle (its app's own initial volume, a
/// WirePlumber restore) before the guard treats changes as sudden.
pub const STREAM_SETTLE: Duration = Duration::from_secs(1);

/// The loudest an app stream may be under `limit`.
pub fn stream_cap_db(guard: &VolumeGuard) -> f64 {
    guard.stream_max_db.unwrap_or(0.0).min(0.0)
}

/// The level a user request for a stream (`set_stream_volume`) is set to,
/// and whether the stream cap (`stream_max_db`) lowered it. The cap holds
/// in every guard mode, `off` included; nothing else limits a user request
/// here (no rise rate, no lock).
pub fn user_stream_level(guard: &VolumeGuard, requested_db: f64) -> (f64, bool) {
    let cap = stream_cap_db(guard);
    (
        requested_db.clamp(VOLUME_MIN_DB, cap.max(VOLUME_MIN_DB)),
        requested_db > cap + EPS_DB,
    )
}

/// Shortest time between two guard writes on one stream. An app that
/// fights the guard is answered at this rate, not once per write.
pub const STREAM_WRITE_INTERVAL: Duration = Duration::from_millis(50);

/// How long a level we wrote can still come back as PipeWire's echo.
pub const ECHO_WINDOW: Duration = Duration::from_secs(1);
const ECHOES_KEPT: usize = 8;

/// The levels we wrote on one stream (guard steps and user sets), oldest
/// first, so a late echo of an earlier write is not taken for the app's
/// own change. PipeWire reports a node's `Props` in the order they were
/// set: once the newest write echoes, the earlier ones are behind it.
#[derive(Clone, Copy, Debug, Default)]
struct Echoes {
    written: [Option<(f64, Instant)>; ECHOES_KEPT],
}

impl Echoes {
    fn push(&mut self, level_db: f64, now: Instant) {
        self.written.rotate_left(1);
        self.written[ECHOES_KEPT - 1] = Some((level_db, now));
    }

    /// True when `level_db` is the echo of a write older than the newest
    /// (within [`ECHO_WINDOW`]): our own stale state, not a change.
    fn is_stale(&mut self, level_db: f64, now: Instant) -> bool {
        for slot in &mut self.written {
            if slot.is_some_and(|(_, at)| now.saturating_duration_since(at) > ECHO_WINDOW) {
                *slot = None;
            }
        }
        let same = |slot: &Option<(f64, Instant)>| {
            slot.is_some_and(|(db, _)| (db - level_db).abs() <= EPS_DB)
        };
        let Some(newest) = self.written.iter().rposition(Option::is_some) else {
            return false;
        };
        if same(&self.written[newest]) {
            // The newest write has landed: nothing older can echo after it.
            for slot in &mut self.written[..newest] {
                *slot = None;
            }
            return false;
        }
        self.written[..newest].iter().any(same)
    }
}

#[derive(Clone, Copy, Debug)]
struct StreamState {
    /// Levels written to the stream, for telling stale echoes apart.
    echoes: Echoes,
    appeared: Instant,
    settled: bool,
    /// The level the guard last accepted (or wrote).
    accepted_db: f64,
    level: LevelGuard,
    last_write: Option<Instant>,
    /// A write held back by [`STREAM_WRITE_INTERVAL`].
    pending: Option<StreamWrite>,
}

impl StreamState {
    fn new(appeared: Instant, now: Instant, accepted_db: f64) -> Self {
        Self {
            appeared,
            settled: now.saturating_duration_since(appeared) >= STREAM_SETTLE,
            accepted_db,
            level: LevelGuard::default(),
            last_write: None,
            pending: None,
            echoes: Echoes::default(),
        }
    }

    /// Issue `write` now, or hold it until the interval has passed.
    fn emit(&mut self, write: StreamWrite, now: Instant) -> Option<StreamWrite> {
        if self
            .last_write
            .is_some_and(|t| now.saturating_duration_since(t) < STREAM_WRITE_INTERVAL)
        {
            // Keep the reason of the first blocked write of the burst.
            let reason = self.pending.and_then(|p| p.reason).or(write.reason);
            self.pending = Some(StreamWrite { reason, ..write });
            return None;
        }
        self.pending = None;
        self.last_write = Some(now);
        Some(write)
    }
}

/// A level to write on a stream, and why.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamWrite {
    pub node: u32,
    pub volume_db: f64,
    /// `None` for the steps of a rise already logged.
    pub reason: Option<BlockReason>,
    /// What the stream asked for.
    pub requested_db: f64,
}

/// Guard state of every app stream on our sinks (graph watcher thread).
#[derive(Debug, Default)]
pub struct StreamGuards {
    streams: HashMap<u32, StreamState>,
}

impl StreamGuards {
    /// A stream on one of our sinks reports `volume_db` (its loudest
    /// channel); it appeared (as a node) at `appeared`. Call it on every
    /// `Props` change and whenever the stream is linked to one of our sinks.
    pub fn observe(
        &mut self,
        guard: &VolumeGuard,
        node: u32,
        volume_db: f64,
        appeared: Instant,
        now: Instant,
    ) -> Option<StreamWrite> {
        if guard.mode == GuardMode::Off {
            self.streams.remove(&node);
            return None;
        }
        let cap = stream_cap_db(guard);
        let state = self
            .streams
            .entry(node)
            .or_insert_with(|| StreamState::new(appeared, now, volume_db.min(cap)));
        if !state.settled && now.saturating_duration_since(state.appeared) >= STREAM_SETTLE {
            state.settled = true;
        }
        if state.echoes.is_stale(volume_db, now) {
            // A late echo of an earlier write of ours (a guard step, or the
            // level before a user's set): judging it would undo the newer
            // write.
            return None;
        }
        let write = StreamWrite {
            node,
            volume_db,
            reason: None,
            requested_db: volume_db,
        };
        let decided = if !state.settled {
            // Its starting level (the app's own, a WirePlumber restore) is
            // no sudden change; only the cap applies, in both modes.
            state.accepted_db = volume_db.min(cap);
            (volume_db > cap + EPS_DB).then_some(StreamWrite {
                volume_db: cap,
                reason: Some(BlockReason::Capped),
                ..write
            })
        } else if guard.mode == GuardMode::Lock {
            // Lock blocks rises only. Holding a stream up against the app
            // turning it down would have to raise its channel volumes over
            // what the app asked for, and when the app then restores its own
            // master the stream jumps above the held level: so a fall is the
            // new held level.
            if volume_db < state.accepted_db - EPS_DB {
                state.accepted_db = volume_db;
                None
            } else {
                (volume_db > state.accepted_db + EPS_DB).then_some(StreamWrite {
                    volume_db: state.accepted_db,
                    reason: Some(BlockReason::Locked),
                    ..write
                })
            }
        } else {
            let decision = state
                .level
                .decide(guard, cap, state.accepted_db, volume_db, now);
            state.accepted_db = decision.apply_db;
            ((decision.apply_db - volume_db).abs() > EPS_DB).then_some(StreamWrite {
                volume_db: decision.apply_db,
                reason: decision.blocked,
                ..write
            })
        };
        match decided {
            Some(w) => state.emit(w, now),
            None => {
                // The stream is where the guard wants it: nothing held back.
                state.pending = None;
                None
            }
        }
    }

    /// Settle new streams, flush held-back writes and step rises.
    pub fn tick(&mut self, guard: &VolumeGuard, now: Instant) -> Vec<StreamWrite> {
        let mut out = Vec::new();
        for (&node, state) in &mut self.streams {
            if !state.settled && now.saturating_duration_since(state.appeared) >= STREAM_SETTLE {
                state.settled = true;
            }
            if let Some(pending) = state.pending
                && let Some(w) = state.emit(pending, now)
            {
                out.push(w);
                continue;
            }
            if guard.mode != GuardMode::Limit {
                state.level.cancel();
                continue;
            }
            if let Some(next) = state.level.tick(guard, state.accepted_db, 0.0, now) {
                state.accepted_db = next;
                state.last_write = Some(now);
                out.push(StreamWrite {
                    node,
                    volume_db: next,
                    reason: None,
                    requested_db: next,
                });
            }
        }
        out
    }

    /// The guard mode changed: forget everything when it is off; otherwise
    /// the streams are taken as they are now (call [`Self::seed`] for each).
    /// A change of parameters only (rise rate, cap) does not call this.
    pub fn engage(&mut self, guard: &VolumeGuard) {
        if guard.mode == GuardMode::Off {
            self.streams.clear();
            return;
        }
        // Off forgets every stream, so a stream still known here was seen
        // under lock or limit: one still in its settle second keeps it.
        for state in self.streams.values_mut() {
            state.level.cancel();
            state.pending = None;
        }
    }

    /// A stream already playing when the guard was turned on: held at its
    /// current level (capped) from now on, settled or not by its age. A
    /// stream the guard already knew (a lock <-> limit switch) keeps its
    /// settle state.
    pub fn seed(
        &mut self,
        guard: &VolumeGuard,
        node: u32,
        volume_db: f64,
        appeared: Instant,
        now: Instant,
    ) -> Option<StreamWrite> {
        let settled = self.streams.remove(&node).is_none_or(|s| s.settled);
        let mut state = StreamState::new(appeared, now, volume_db.min(stream_cap_db(guard)));
        state.settled = settled;
        self.streams.insert(node, state);
        self.observe(guard, node, volume_db, appeared, now)
    }

    /// The user set this stream to `volume_db` (`set_stream_volume`, already
    /// capped and written): that is the level the guard accepts from now on.
    /// Under `lock` it becomes the held level, so it is not reverted; under
    /// `limit` it is not slowed down (a rise in progress is dropped). A
    /// stream in its settle second counts as settled: the app's own later
    /// changes are judged against the user's level. Nothing held back from
    /// before is sent. `off` keeps no state.
    pub fn adopt(
        &mut self,
        guard: &VolumeGuard,
        node: u32,
        volume_db: f64,
        appeared: Instant,
        now: Instant,
    ) {
        if guard.mode == GuardMode::Off {
            self.streams.remove(&node);
            return;
        }
        let level = volume_db.min(stream_cap_db(guard));
        let state = self
            .streams
            .entry(node)
            .or_insert_with(|| StreamState::new(appeared, now, level));
        state.settled = true;
        state.accepted_db = level;
        state.level.cancel();
        state.pending = None;
        // It was a write to the stream: the guard's own writes keep their
        // interval from it.
        state.last_write = Some(now);
        state.echoes.push(level, now);
    }

    /// A level was written on `node` (a guard write): its echo is ours,
    /// and so is a late echo of it after a newer write. Pass the level the
    /// stream plays at after the write, as it will be observed.
    pub fn wrote(&mut self, node: u32, level_db: f64, now: Instant) {
        if let Some(state) = self.streams.get_mut(&node) {
            state.echoes.push(level_db, now);
        }
    }

    /// Whether the guard has state for `node`.
    pub fn knows(&self, node: u32) -> bool {
        self.streams.contains_key(&node)
    }

    pub fn remove(&mut self, node: u32) {
        self.streams.remove(&node);
    }

    pub fn is_slewing(&self) -> bool {
        self.streams.values().any(|s| s.level.is_slewing())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(mode: GuardMode) -> VolumeGuard {
        VolumeGuard {
            mode,
            max_rise_db_per_s: 6.0,
            stream_max_db: None,
        }
    }

    #[test]
    fn off_passes_everything_below_the_cap() {
        let mut g = LevelGuard::default();
        let t = Instant::now();
        let d = g.decide(&guard(GuardMode::Off), 0.0, -30.0, -3.0, t);
        assert_eq!(d.apply_db, -3.0);
        assert_eq!(d.blocked, None);
        let d = g.decide(&guard(GuardMode::Off), -10.0, -30.0, 10.6, t);
        assert_eq!(d.apply_db, -10.0);
        assert_eq!(d.blocked, Some(BlockReason::Capped));
        let d = g.decide(&guard(GuardMode::Off), 0.0, -30.0, -200.0, t);
        assert_eq!(d.apply_db, VOLUME_MIN_DB);
    }

    /// Already at the ceiling, a request above it is still capped (in
    /// every mode that caps), so the caller puts the sink back; the same
    /// level again is no change.
    #[test]
    fn a_request_above_the_ceiling_at_the_ceiling_is_capped() {
        let t = Instant::now();
        for rate in [0.0, 6.0] {
            for mode in [GuardMode::Off, GuardMode::Limit] {
                let cfg = VolumeGuard {
                    max_rise_db_per_s: rate,
                    ..guard(mode)
                };
                let mut g = LevelGuard::default();
                let d = g.decide(&cfg, -3.0, -3.0, -1.61, t);
                assert_eq!(d.apply_db, -3.0);
                assert_eq!(d.blocked, Some(BlockReason::Capped), "{mode:?} {rate}");
                assert!(!g.is_slewing());
                let d = g.decide(&cfg, -3.0, -3.0, -3.0005, t);
                assert_eq!(d.blocked, None, "{mode:?} {rate}");
            }
        }
    }

    #[test]
    fn lock_reverts_any_change() {
        let mut g = LevelGuard::default();
        let t = Instant::now();
        let d = g.decide(&guard(GuardMode::Lock), 0.0, -30.0, 0.0, t);
        assert_eq!(d.apply_db, -30.0);
        assert_eq!(d.blocked, Some(BlockReason::Locked));
        let d = g.decide(&guard(GuardMode::Lock), 0.0, -30.0, -40.0, t);
        assert_eq!(d.apply_db, -30.0);
        // Same value (a re-assert) is no change.
        let d = g.decide(&guard(GuardMode::Lock), 0.0, -30.0, -30.0005, t);
        assert_eq!(d.blocked, None);
    }

    #[test]
    fn limit_slews_rises_and_passes_falls() {
        let cfg = guard(GuardMode::Limit);
        let mut g = LevelGuard::default();
        let t0 = Instant::now();
        // Fall: at once.
        let d = g.decide(&cfg, 0.0, -10.0, -30.0, t0);
        assert_eq!(d.apply_db, -30.0);
        assert_eq!(d.blocked, None);
        assert!(!g.is_slewing());

        // Rise of 18 dB at 6 dB/s: three seconds, never faster.
        let d = g.decide(&cfg, 0.0, -30.0, -12.0, t0);
        assert_eq!(d.apply_db, -30.0);
        assert_eq!(d.blocked, Some(BlockReason::Slewed));
        let mut level = -30.0;
        let mut t = t0;
        let mut steps = 0;
        while let Some(next) = {
            t += SLEW_TICK;
            g.tick(&cfg, level, 0.0, t)
        } {
            assert!(next - level <= 6.0 * SLEW_TICK.as_secs_f64() + 1e-9);
            assert!(next <= -12.0);
            level = next;
            steps += 1;
            if !g.is_slewing() {
                break;
            }
        }
        assert_eq!(level, -12.0);
        assert!((59..=61).contains(&steps), "{steps} steps");

        // A fall during a rise cancels it.
        g.decide(&cfg, 0.0, -30.0, 0.0, t);
        assert!(g.is_slewing());
        let d = g.decide(&cfg, 0.0, -28.0, -40.0, t);
        assert_eq!(d.apply_db, -40.0);
        assert!(!g.is_slewing());

        // A rise above the cap is capped and slewed to the cap.
        let d = g.decide(&cfg, -20.0, -30.0, 0.0, t);
        assert_eq!(d.blocked, Some(BlockReason::Capped));
        let mut level = -30.0;
        while let Some(next) = {
            t += Duration::from_secs(1);
            g.tick(&cfg, level, 0.0, t)
        } {
            level = next;
        }
        assert_eq!(level, -20.0);

        // Rate 0: only the cap.
        let mut fast = cfg;
        fast.max_rise_db_per_s = 0.0;
        let d = g.decide(&fast, 0.0, -30.0, -3.0, t);
        assert_eq!(d.apply_db, -3.0);

        // Switching to rate 0 during a rise finishes it on the next tick.
        let d = g.decide(&cfg, -10.0, -30.0, 0.0, t);
        assert!(d.apply_db < -10.0 && g.is_slewing());
        t += Duration::from_millis(50);
        assert_eq!(g.tick(&fast, -29.0, 0.0, t), Some(-10.0));
        assert!(!g.is_slewing());
    }

    /// An app turning its own master down and then back up never makes
    /// the stream louder than the level lock held: the fall is accepted
    /// (no channel volume is raised to make up for it), so the restore is
    /// a rise, and blocked.
    #[test]
    fn lock_master_down_then_up_never_exceeds_the_held_level() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        let held = -20.0;
        assert_eq!(s.seed(&g, 3, held, t0, t0), None);
        let t = t0 + Duration::from_secs(2);
        // Master 1.0 -> 0.1: -40 dB. Nothing is written back up.
        assert_eq!(s.observe(&g, 3, -40.0, t0, t), None);
        // Master back to 1.0: -20 dB asked; what plays is never above held.
        let w = s.observe(&g, 3, -20.0, t0, t).unwrap();
        assert!(w.volume_db <= held + EPS_DB);
        assert_eq!(w.volume_db, -40.0);
    }

    /// A user request (`set_stream_volume`) is capped by `stream_max_db` in
    /// every mode, `off` included, and only by that.
    #[test]
    fn user_stream_levels_are_capped_in_every_mode() {
        for mode in [GuardMode::Off, GuardMode::Lock, GuardMode::Limit] {
            let g = VolumeGuard {
                stream_max_db: Some(-12.0),
                ..guard(mode)
            };
            assert_eq!(user_stream_level(&g, -3.0), (-12.0, true), "{mode:?}");
            assert_eq!(user_stream_level(&g, -12.0), (-12.0, false), "{mode:?}");
            assert_eq!(user_stream_level(&g, -30.0), (-30.0, false), "{mode:?}");
            assert_eq!(user_stream_level(&g, -500.0), (VOLUME_MIN_DB, false));
            // Without a cap, 0 dB (100 %) is the top.
            assert_eq!(user_stream_level(&guard(mode), 0.0), (0.0, false));
        }
    }
}
