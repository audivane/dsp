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

#[cfg(test)]
mod step_tests {
    use super::*;

    /// Slewing on the hardware mixer writes it in whole dB steps, never
    /// ahead of the rise rate.
    #[test]
    fn hardware_slew_steps_in_whole_db_and_keeps_the_rate() {
        let guard = VolumeGuard {
            mode: GuardMode::Limit,
            ..VolumeGuard::default()
        };
        let t0 = Instant::now();
        let mut g = LevelGuard::default();
        let d = g.decide(&guard, 0.0, -30.0, -10.0, t0);
        assert_eq!(d.blocked, Some(BlockReason::Slewed));
        let mut level = -30.0;
        let mut writes = 0;
        for i in 1..=100u32 {
            let now = t0 + Duration::from_millis(50 * u64::from(i));
            if let Some(next) = g.tick(&guard, level, HW_SLEW_STEP_DB, now) {
                let elapsed = now.duration_since(t0).as_secs_f64();
                assert!(next <= -30.0 + guard.max_rise_db_per_s * elapsed + EPS_DB);
                assert!(next - level >= HW_SLEW_STEP_DB - EPS_DB || next == -10.0);
                level = next;
                writes += 1;
            }
        }
        assert_eq!(level, -10.0);
        // 20 dB in whole steps, not one write per 50 ms tick (67).
        assert!(writes <= 21, "{writes} writes");
        assert!(!g.is_slewing());
        assert_eq!(
            slew_step_db(audivane_protocol::ipc::VolumeMode::Hybrid, true),
            0.0
        );
        assert_eq!(
            slew_step_db(audivane_protocol::ipc::VolumeMode::Auto, true),
            HW_SLEW_STEP_DB
        );
        assert_eq!(
            slew_step_db(audivane_protocol::ipc::VolumeMode::Auto, false),
            0.0
        );
    }
}

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

    #[test]
    fn streams_settle_then_lock() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        assert_eq!(s.observe(&g, 7, 0.0, t0, t0), None);
        // WirePlumber restores 30 % within the settle window: accepted.
        let t = t0 + Duration::from_millis(200);
        assert_eq!(s.observe(&g, 7, -31.4, t0, t), None);
        s.tick(&g, t0 + STREAM_SETTLE);
        // The app jumps to 100 %: reverted.
        let t = t0 + Duration::from_secs(3);
        let w = s.observe(&g, 7, 0.0, t0, t).unwrap();
        assert_eq!(w.volume_db, -31.4);
        assert_eq!(w.reason, Some(BlockReason::Locked));
        // Our own write echoing back is no change.
        assert_eq!(s.observe(&g, 7, -31.4, t0, t), None);
        // Down is accepted (the new held level): lock blocks rises only.
        let t = t0 + Duration::from_secs(4);
        assert_eq!(s.observe(&g, 7, -40.0, t0, t), None);
        // And back up is a rise from there: reverted.
        let w = s.observe(&g, 7, -31.4, t0, t).unwrap();
        assert_eq!(w.volume_db, -40.0);

        // A stream already playing when the guard comes on is held at once.
        assert_eq!(s.seed(&g, 9, -20.0, t0, t0), None);
        let w = s.observe(&g, 9, 0.0, t0, t0).unwrap();
        assert_eq!(w.volume_db, -20.0);
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

    /// A stream first seen at its app's master 0 is seeded at silence
    /// (see the graph's `level_for_guard`), so its first rise under limit
    /// slews instead of jumping straight to the cap.
    #[test]
    fn a_stream_first_seen_silent_slews_its_first_rise() {
        let g = guard(GuardMode::Limit);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        let seen = t0 + Duration::from_secs(10); // long settled
        assert_eq!(s.observe(&g, 5, VOLUME_MIN_DB, t0, seen), None);
        let w = s.observe(&g, 5, 0.0, t0, seen).unwrap();
        assert_eq!(w.reason, Some(BlockReason::Slewed));
        assert!(w.volume_db < -60.0, "{}", w.volume_db);
    }

    /// A stream first seen long after it appeared (it was
    /// linked to our sink late, or moved onto it) is settled at once.
    #[test]
    fn a_stream_seen_late_is_judged_by_its_age() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let appeared = Instant::now();
        let later = appeared + Duration::from_secs(300);
        assert_eq!(s.observe(&g, 5, -20.0, appeared, later), None);
        let w = s.observe(&g, 5, 3.5, appeared, later).unwrap();
        assert_eq!((w.volume_db, w.reason), (-20.0, Some(BlockReason::Locked)));
    }

    /// Lock caps a stream during its settle window too.
    #[test]
    fn lock_caps_while_settling() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        // 150 %: +3.5 dB.
        let w = s.observe(&g, 4, 3.5, t0, t0).unwrap();
        assert_eq!((w.volume_db, w.reason), (0.0, Some(BlockReason::Capped)));
        // And it is held there, not at 150 %.
        let t = t0 + Duration::from_secs(2);
        s.tick(&g, t);
        let w = s.observe(&g, 4, 3.5, t0, t).unwrap();
        assert_eq!(w.volume_db, 0.0);
        // Seeding above the cap is capped too.
        let w = s.seed(&g, 8, 3.5, t0, t).unwrap();
        assert_eq!(w.volume_db, 0.0);
    }

    /// An app fighting the guard is answered at a bounded
    /// rate, and the last held-back revert is never lost.
    #[test]
    fn fighting_apps_are_answered_at_a_bounded_rate() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        s.seed(&g, 1, -20.0, t0, t0);
        let mut writes = 0;
        let mut t = t0;
        for i in 0..1000 {
            t = t0 + Duration::from_millis(i);
            if s.observe(&g, 1, 0.0, t0, t).is_some() {
                writes += 1;
            }
            if i % 50 == 0 {
                writes += s.tick(&g, t).len();
            }
        }
        assert!(writes <= 21, "{writes} writes in a second");
        let flushed = s.tick(&g, t + STREAM_WRITE_INTERVAL);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].volume_db, -20.0);
    }

    /// Switching between lock and limit keeps a new stream's settle second:
    /// its app's start-up volume or a restore is not reverted.
    #[test]
    fn a_mode_switch_keeps_the_settle_window() {
        let lock = guard(GuardMode::Lock);
        let limit = guard(GuardMode::Limit);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        assert_eq!(s.observe(&lock, 4, -20.0, t0, t0), None);
        let t1 = t0 + Duration::from_millis(300);
        s.engage(&limit);
        assert_eq!(s.seed(&limit, 4, -20.0, t0, t1), None);
        // The restore lands within the second: accepted, not slewed.
        let t2 = t0 + Duration::from_millis(600);
        assert_eq!(s.observe(&limit, 4, -6.0, t0, t2), None);
        s.engage(&lock);
        assert_eq!(s.seed(&lock, 4, -6.0, t0, t2), None);
        assert_eq!(s.observe(&lock, 4, -3.0, t0, t2), None);
        // After the second, lock holds.
        let t3 = t0 + STREAM_SETTLE + Duration::from_millis(10);
        assert!(s.observe(&lock, 4, 0.0, t0, t3).is_some());
    }

    /// Off forgets; re-enabling starts from the streams as
    /// they are then.
    #[test]
    fn off_forgets_stream_state() {
        let lock = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        s.seed(&lock, 2, -30.0, t0, t0);
        s.engage(&guard(GuardMode::Off));
        assert_eq!(s.observe(&guard(GuardMode::Off), 2, -5.0, t0, t0), None);
        s.engage(&lock);
        // Seeded at -5 now, not the stale -30.
        assert_eq!(s.seed(&lock, 2, -5.0, t0, t0), None);
        assert_eq!(s.observe(&lock, 2, -5.0, t0, t0), None);
    }

    #[test]
    fn streams_are_capped_and_slewed_under_limit() {
        let mut g = guard(GuardMode::Limit);
        g.stream_max_db = Some(-10.0);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        // Appears at 100 %: capped at once.
        let w = s.observe(&g, 3, 0.0, t0, t0).unwrap();
        assert_eq!((w.volume_db, w.reason), (-10.0, Some(BlockReason::Capped)));
        assert_eq!(s.observe(&g, 3, -10.0, t0, t0), None);
        let t1 = t0 + Duration::from_secs(2);
        s.tick(&g, t1);
        assert_eq!(s.observe(&g, 3, -40.0, t0, t1), None, "down passes");
        // Up from -40 to -10: held, then 6 dB/s.
        let t1 = t1 + Duration::from_secs(1);
        let w = s.observe(&g, 3, -10.0, t0, t1).unwrap();
        assert_eq!((w.volume_db, w.reason), (-40.0, Some(BlockReason::Slewed)));
        assert!(s.is_slewing());
        let mut level = -40.0;
        let mut t = t1;
        while s.is_slewing() {
            t += SLEW_TICK;
            // The app re-asserts its level every tick: the rise goes on.
            let _ = s.observe(&g, 3, -10.0, t0, t);
            for step in s.tick(&g, t) {
                assert!(step.volume_db - level <= 0.3 + 1e-9);
                level = step.volume_db;
                assert_eq!(s.observe(&g, 3, level, t0, t), None, "echo");
            }
            assert!(t - t1 < Duration::from_secs(10), "rise stalled at {level}");
        }
        assert_eq!(level, -10.0);
        let secs = (t - t1).as_secs_f64();
        assert!((4.9..=5.2).contains(&secs), "{secs}");
        // Off: anything goes.
        assert_eq!(s.observe(&guard(GuardMode::Off), 3, 6.0, t0, t), None);
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

    /// Under `limit` the user's raise is not slowed: adopting it drops the
    /// rise in progress, and its echo from PipeWire is no change.
    #[test]
    fn limit_does_not_slow_a_user_raise() {
        let g = guard(GuardMode::Limit);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(s.observe(&g, 5, -30.0, t0, t1), None);
        // The app jumps up: slowed.
        let w = s.observe(&g, 5, -6.0, t0, t1).unwrap();
        assert_eq!(w.reason, Some(BlockReason::Slewed));
        assert!(s.is_slewing());
        // The user raises it to -3 dB: taken as it is.
        let t2 = t1 + Duration::from_millis(10);
        s.adopt(&g, 5, -3.0, t0, t2);
        assert!(!s.is_slewing());
        assert_eq!(s.observe(&g, 5, -3.0, t0, t2), None, "echo");
        assert_eq!(s.observe(&g, 5, -3.0005, t0, t2), None, "f32 echo");
        assert!(s.tick(&g, t2 + SLEW_TICK).is_empty(), "nothing held back");
        // Later app rises are judged from the user's level.
        let t3 = t2 + Duration::from_secs(1);
        let w = s.observe(&g, 5, 0.0, t0, t3).unwrap();
        assert_eq!(w.volume_db, -3.0);
        assert_eq!(w.reason, Some(BlockReason::Slewed));
    }

    /// Under `lock` the user's level becomes the held level, up or down,
    /// so the guard never reverts it; an app rise over it still is.
    #[test]
    fn lock_holds_the_user_level() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(s.observe(&g, 9, -20.0, t0, t1), None);
        // A raise through the call: held, not reverted.
        s.adopt(&g, 9, -6.0, t0, t1);
        assert_eq!(s.observe(&g, 9, -6.0, t0, t1), None);
        // The app rises above it: back to the user's level.
        let t2 = t1 + Duration::from_secs(1);
        let w = s.observe(&g, 9, -1.0, t0, t2).unwrap();
        assert_eq!(w.volume_db, -6.0);
        assert_eq!(w.reason, Some(BlockReason::Locked));
        // A lowering through the call is held too.
        let t3 = t2 + Duration::from_secs(1);
        s.adopt(&g, 9, -25.0, t0, t3);
        let t4 = t3 + Duration::from_secs(1);
        let w = s.observe(&g, 9, -6.0, t0, t4).unwrap();
        assert_eq!(w.volume_db, -25.0);
        // A stream in its settle second is settled by the user's level:
        // the app's own later rise is not taken as its starting level.
        let t5 = Instant::now();
        s.adopt(&g, 11, -18.0, t5, t5);
        let w = s
            .observe(&g, 11, -2.0, t5, t5 + Duration::from_millis(100))
            .unwrap();
        assert_eq!(w.volume_db, -18.0);
        // Off keeps nothing.
        s.adopt(&guard(GuardMode::Off), 9, -3.0, t0, t4);
        assert!(!s.knows(9));
    }

    /// A guard write held back by the write interval does not go out over
    /// the user's level afterwards.
    #[test]
    fn adopt_drops_a_held_back_guard_write() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(s.observe(&g, 4, -20.0, t0, t1), None);
        assert!(s.observe(&g, 4, -10.0, t0, t1).is_some());
        // Within the interval: held back.
        let t2 = t1 + Duration::from_millis(10);
        assert_eq!(s.observe(&g, 4, -9.0, t0, t2), None);
        s.adopt(&g, 4, -8.0, t0, t2 + Duration::from_millis(1));
        assert!(s.tick(&g, t2 + Duration::from_millis(200)).is_empty());
    }

    /// Lock: a late PipeWire echo of an earlier guard write (-24 dB) that
    /// arrives after the user set -12 dB is our own stale state, not the
    /// app lowering the stream: it neither becomes the held level nor gets
    /// the user's own echo reverted.
    #[test]
    fn lock_ignores_a_stale_echo_after_a_user_set() {
        let g = guard(GuardMode::Lock);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(s.observe(&g, 3, -24.0, t0, t1), None);
        // The app rises: reverted to -24, and the watcher records the write.
        let w = s.observe(&g, 3, -10.0, t0, t1).unwrap();
        assert_eq!(w.volume_db, -24.0);
        s.wrote(3, -24.0, t1);
        // The user sets -12 before the echo of -24 came back.
        let t2 = t1 + Duration::from_millis(20);
        s.adopt(&g, 3, -12.0, t0, t2);
        let t3 = t2 + Duration::from_millis(100);
        assert_eq!(s.observe(&g, 3, -24.0, t0, t3), None, "stale echo");
        assert_eq!(s.observe(&g, 3, -12.0, t0, t3), None, "the user's echo");
        // The user's level is held.
        let t4 = t3 + Duration::from_millis(100);
        let w = s.observe(&g, 3, -5.0, t0, t4).unwrap();
        assert_eq!(w.volume_db, -12.0);
        assert_eq!(w.reason, Some(BlockReason::Locked));
    }

    /// Limit: a late echo of a rise step does not restart the rise the
    /// user's lower level cancelled.
    #[test]
    fn limit_ignores_a_stale_step_echo_after_a_user_set() {
        let g = guard(GuardMode::Limit);
        let mut s = StreamGuards::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(s.observe(&g, 6, -30.0, t0, t1), None);
        assert!(s.observe(&g, 6, -10.0, t0, t1).is_some());
        let t2 = t1 + Duration::from_millis(100);
        let steps = s.tick(&g, t2);
        assert_eq!(steps.len(), 1);
        let step = steps[0].volume_db;
        assert!(step > -30.0 && step < -10.0, "{step}");
        s.wrote(6, step, t2);
        // The user turns it down to -40 before the step's echo.
        let t3 = t2 + Duration::from_millis(10);
        s.adopt(&g, 6, -40.0, t0, t3);
        assert!(!s.is_slewing());
        let t4 = t3 + Duration::from_millis(50);
        assert_eq!(s.observe(&g, 6, step, t0, t4), None, "stale step echo");
        assert!(!s.is_slewing(), "the rise must stay cancelled");
        assert_eq!(s.observe(&g, 6, -40.0, t0, t4), None);
        assert!(s.tick(&g, t4 + SLEW_TICK).is_empty());
    }

    /// Only writes older than the newest count as stale, and only within
    /// the echo window: the newest write's echo clears the older ones, and
    /// an app later choosing a level we once wrote is judged as a change.
    #[test]
    fn stale_echoes_are_bounded() {
        let mut e = Echoes::default();
        let t0 = Instant::now();
        assert!(!e.is_stale(-6.0, t0), "nothing written");
        e.push(-24.0, t0);
        e.push(-12.0, t0);
        assert!(e.is_stale(-24.0, t0));
        assert!(!e.is_stale(-18.0, t0), "an app level");
        assert!(!e.is_stale(-12.0, t0), "the newest write's echo");
        assert!(!e.is_stale(-24.0, t0), "older writes are behind it now");
        e.push(-30.0, t0);
        e.push(-20.0, t0);
        let late = t0 + ECHO_WINDOW + Duration::from_millis(1);
        assert!(!e.is_stale(-30.0, late), "out of the window");
        // More writes than are kept: the oldest fall off.
        for i in 0..20 {
            e.push(-f64::from(i), t0);
        }
        assert!(!e.is_stale(-1.0 - f64::from(ECHOES_KEPT as u32 + 2), t0));
        assert!(e.is_stale(-12.0, t0));
    }

    #[test]
    fn logging_is_rate_limited_per_source() {
        let log = VolumeEventLog::default();
        let t0 = Instant::now();
        assert_eq!(log.should_log("a", t0), Some(0));
        for i in 1..100 {
            assert_eq!(log.should_log("a", t0 + Duration::from_millis(i)), None);
        }
        assert_eq!(log.should_log("b", t0), Some(0));
        assert_eq!(log.should_log("a", t0 + LOG_INTERVAL), Some(99));
        // Counting is not rate limited.
        for _ in 0..50 {
            log.record(VolumeSource::Control, 0.0, -10.0, BlockReason::Capped);
        }
        assert_eq!(log.snapshot().0, 50);
    }

    #[test]
    fn log_counts_and_keeps_the_last_change() {
        let log = VolumeEventLog::default();
        assert_eq!(log.snapshot(), (0, None));
        log.record(VolumeSource::Control, 0.0, -10.0, BlockReason::Capped);
        log.record(
            VolumeSource::Stream {
                node: 42,
                app: Some("Player".into()),
            },
            0.0,
            -20.0,
            BlockReason::Locked,
        );
        let (count, last) = log.snapshot();
        assert_eq!(count, 2);
        let last = last.unwrap();
        assert_eq!(last.reason, BlockReason::Locked);
        assert_eq!(last.applied_db, -20.0);
        assert!(last.at_ms > 0);
    }
}
