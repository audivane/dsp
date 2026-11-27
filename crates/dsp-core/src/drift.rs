//! Drift tracking for a follower DAC.
//!
//! A follower DAC runs on its own crystal. Its stage resamples the source
//! with the step `Δ = s · (1 + ε)`, where `s` is the master's speed and ε
//! the follower's clock error relative to the master. This module works ε
//! out from the two devices' positions, in four pieces:
//!
//! - [`PositionMap`]: which source position each device frame carries,
//!   recorded at each period start and interpolated in between.
//! - [`PhaseDetector`]: the master's and the follower's playback positions
//!   in source frames, from `(frames written, delay, timestamp)` readings
//!   ([`Reading`]), the master's moved to the follower's instant at the
//!   nominal rate. Their difference is the phase error.
//! - [`DriftLoop`]: a type-2 loop. The plant is an integrator
//!   (`de/dt = −g (δ + ε)`, `g` the source rate in frames per second, δ
//!   the follower's clock offset). The controller is a PI behind a cascade
//!   of identical one-pole low-passes, all scaled to one bandwidth `b`.
//!   `b` starts wide (acquisition) and narrows hyperbolically to the
//!   tracking bandwidth (`d ln b / dt = −2π k b`, the gear). The continuous
//!   system is propagated exactly over each update interval, so ε and its
//!   slope are known at the end of the coming period. Errors beyond a
//!   threshold are not slewed: the follower skips (or repeats) whole source
//!   frames while muted, and the loop keeps its rate state.
//! - **Placement.** A loop whose rate state is good (it locked before, or
//!   was seeded with a remembered rate) does not acquire again after a
//!   phase jump (the master's stream restarted, a skip from lock). It
//!   averages the readings robustly (their interquartile mean), skips by it
//!   as soon as the follower is then within the placement floor with
//!   confidence, and is heard at once, the loop correcting the residual
//!   while audible. It mutes again only when the error is outside the
//!   floor.
//! - [`EpsilonTrack`]: ε applied through the core's C¹ pieces
//!   ([`Segment::Piece`]): each period is one cubic Hermite of the step from
//!   the previous end (value and slope) to the loop's end values, combined
//!   with the master's own envelope segment.
//!
//! [`RateEstimator`] gives an open-loop rate estimate (a least-squares slope
//! of the raw phase, the error plus the integrated correction) for status
//! and as a check on the loop's own rate state.
//!
//! **Real time.** Everything here is fixed-size: `new` may run anywhere,
//! and every other call is allocation-free, lock-free and bounded. The
//! offline simulation is in [`sim`] (it allocates; not for the audio path).

use crate::adaptive::{Segment, Step};
use std::f64::consts::PI;

pub mod readings;
pub mod sim;

/// ε is clamped to ±this (the master speed is capped at `2 / (1 +
/// EPS_MAX)` while followers play).
pub const EPS_MAX: f64 = 2e-3;
/// Entries a [`PositionMap`] keeps (two per period: enough for any device
/// buffer of up to half this many periods).
pub const MAP_LEN: usize = 64;
/// Most low-pass poles in a [`DriftLoop`].
pub const MAX_POLES: usize = 16;
/// Readings a [`RateEstimator`] keeps.
pub const RATE_LEN: usize = 512;
/// Readings a placement looks at, at most (the newest).
pub const PLACE_LEN: usize = 64;
/// How far ahead of the loop's delay a placed follower's error is
/// predicted, seconds (its fade and a margin): a drift mutes it before it
/// crosses the floor at the device.
const PLACE_AHEAD_S: f64 = 0.1;
/// A loop seeded at the nominal rate jumps to the readings' slope this
/// many times at most: once the slope is known within `NOMINAL_SE` (rate
/// units; or the readings held are full) and stands `NOMINAL_SIGMAS`
/// standard errors clear of zero. (When it is known, not when it first
/// looks clear: that is when noise has pushed it out, and it overshoots.)
const NOMINAL_JUMPS: u32 = 3;
/// What a first start (a pair never seen) is heard within until its rate
/// is taken and its error is within the placement floor (or its loop has
/// locked), seconds: ±100 ppm stays inside it until then.
const FIRST_FLOOR_S: f64 = 0.25e-3;
const NOMINAL_SE: f64 = 12.5e-6;
/// The least span of a full set of readings that tells the slope without
/// [`NOMINAL_SE`], seconds.
const NOMINAL_SPAN_S: f64 = 0.5;
const NOMINAL_SIGMAS: f64 = 3.0;
/// One source frame in the 64.64 fixed point of [`PositionMap`].
pub const FRAME: i128 = 1 << 64;
const FRAME_F: f64 = 18_446_744_073_709_551_616.0;

/// One device reading: how many frames the device has been given, how many
/// of those are still queued ahead of the DAC, and when that was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Device frames written since the stream started.
    pub written: u64,
    /// Device frames queued ahead of the DAC (the ALSA delay).
    pub delay: i64,
    /// When the delay was read, ns, on one clock for every device
    /// (`CLOCK_MONOTONIC`).
    pub time_ns: i64,
}

impl Reading {
    /// The device frame at the DAC (`written − delay`), or `None` when the
    /// delay is negative or more than written (an xrun).
    pub fn played(&self) -> Option<u64> {
        u64::try_from(self.delay)
            .ok()
            .and_then(|d| self.written.checked_sub(d))
    }
}

/// Source positions of device frames: two entries per period written, its
/// first and its last frame with the source positions (64.64 source
/// frames) they carry, linear in between. The curvature ε adds within one
/// period is below 1e-7 frames. Keeping the last frame too means a skip
/// (a jump in source position between periods) never falls inside a span,
/// and every frame written is covered without extrapolation.
#[derive(Clone, Debug)]
pub struct PositionMap {
    frames: [u64; MAP_LEN],
    pos: [i128; MAP_LEN],
    /// Entries held (at most `MAP_LEN`).
    len: usize,
    /// Index the next entry goes to.
    head: usize,
}

impl Default for PositionMap {
    fn default() -> Self {
        Self::new()
    }
}

impl PositionMap {
    pub fn new() -> Self {
        Self {
            frames: [0; MAP_LEN],
            pos: [0; MAP_LEN],
            len: 0,
            head: 0,
        }
    }

    /// Forget every entry (a reopen or a reset).
    pub fn clear(&mut self) {
        self.len = 0;
        self.head = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `i`-th newest entry (0 is the newest).
    fn entry(&self, i: usize) -> (u64, i128) {
        let j = (self.head + MAP_LEN - 1 - i) % MAP_LEN;
        (self.frames[j], self.pos[j])
    }

    /// Device frame `frame` carries source position `pos` (64.64 source
    /// frames). Frames must increase; an entry at or before the newest one
    /// replaces the history (a discontinuity).
    pub fn push(&mut self, frame: u64, pos: i128) {
        if self.len > 0 && frame <= self.entry(0).0 {
            self.clear();
        }
        self.frames[self.head] = frame;
        self.pos[self.head] = pos;
        self.head = (self.head + 1) % MAP_LEN;
        self.len = (self.len + 1).min(MAP_LEN);
    }

    /// Shift every recorded position by `delta` (a skip of whole source
    /// frames: what was recorded plays `delta` later in the source).
    pub fn shift(&mut self, delta: i128) {
        for p in &mut self.pos {
            *p += delta;
        }
    }

    /// The source position of device frame `frame` (64.64): interpolated
    /// between the entries around it (the newest entry itself included),
    /// `None` outside the entries.
    pub fn source_at(&self, frame: u64) -> Option<i128> {
        if self.len == 0 {
            return None;
        }
        let (f1, p1) = self.entry(0);
        if frame == f1 {
            return Some(p1);
        }
        if frame > f1 {
            return None;
        }
        for i in 1..self.len {
            let (fa, pa) = self.entry(i);
            if frame >= fa {
                let (fb, pb) = self.entry(i - 1);
                return Some(lerp(fa, pa, fb, pb, frame));
            }
        }
        None
    }
}

/// `pa + (pb − pa) (x − fa) / (fb − fa)`, rounded, in `i128`.
fn lerp(fa: u64, pa: i128, fb: u64, pb: i128, x: u64) -> i128 {
    let span = i128::from(fb - fa);
    let dx = i128::from(x) - i128::from(fa);
    // (pb − pa) is at most a period of source frames (< 2^80), dx < 2^40.
    pa + ((pb - pa) * dx + span / 2).div_euclid(span)
}

/// The phase error between the master and a follower, in source frames:
/// positive when the follower plays behind the master.
#[derive(Clone, Debug)]
pub struct PhaseDetector {
    /// Master source frames per second (the nominal rate times the speed).
    master_rate: f64,
    /// The latest master position and its time.
    master: Option<(i128, i64)>,
    /// The follower's intended lag behind the master, source frames (a
    /// trim: the devices' own latencies, speaker distance).
    offset: f64,
    /// Readings older than this are not used, ns.
    max_age_ns: i64,
}

impl PhaseDetector {
    /// A detector for a master playing `master_rate` source frames per
    /// second.
    pub fn new(master_rate: f64) -> Self {
        Self {
            master_rate,
            master: None,
            offset: 0.0,
            max_age_ns: 1_000_000_000,
        }
    }

    /// The master's source rate changed (a speed change).
    pub fn set_master_rate(&mut self, rate: f64) {
        self.master_rate = rate;
    }

    /// The follower's intended lag behind the master, source frames.
    pub fn set_offset(&mut self, frames: f64) {
        self.offset = frames;
    }

    /// Forget the master's position (its stream restarted).
    pub fn reset(&mut self) {
        self.master = None;
    }

    /// A master reading, with the master's position map. Returns false when
    /// it could not be placed (an xrun's delay, no map yet).
    pub fn master(&mut self, reading: Reading, map: &PositionMap) -> bool {
        match reading.played().and_then(|f| map.source_at(f)) {
            Some(p) => {
                self.master = Some((p, reading.time_ns));
                true
            }
            None => false,
        }
    }

    /// The master's source position at its DAC (64.64) at `time_ns`, as
    /// the master's own thread works it out from its reading and map and
    /// publishes it.
    pub fn master_position(&mut self, pos: i128, time_ns: i64) {
        self.master = Some((pos, time_ns));
    }

    /// The phase error at a follower reading, source frames: the master's
    /// position moved to the reading's time at the nominal rate, minus the
    /// follower's, minus the offset. `None` without a recent master
    /// position or when the reading cannot be placed.
    pub fn follower(&self, reading: Reading, map: &PositionMap) -> Option<f64> {
        let (pm, tm) = self.master?;
        let pf = map.source_at(reading.played()?)?;
        let dt = reading.time_ns.checked_sub(tm)?;
        if dt.abs() > self.max_age_ns {
            return None;
        }
        let diff = (pm - pf) as f64 / FRAME_F;
        Some(diff + self.master_rate * dt as f64 * 1e-9 - self.offset)
    }
}

/// The loop's constants. Bandwidths are crossover frequencies of the open
/// loop, Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopConfig {
    /// Bandwidth at the start (and the most after a skip).
    pub acquire_hz: f64,
    /// Bandwidth once the gear has finished.
    pub track_hz: f64,
    /// Gear rate: `d ln b / dt = −2π k b`.
    pub gear_k: f64,
    /// Bandwidth after a skip, if lower than the current one: a skip is a
    /// phase jump, the rate state is still good.
    pub reacquire_hz: f64,
    /// Crossover over the PI zero.
    pub zero_ratio: f64,
    /// Low-pass corner over the crossover.
    pub pole_ratio: f64,
    /// Low-pass poles, 1 to [`MAX_POLES`].
    pub poles: usize,
    /// The loop's own delay (the device buffer plus a period): the
    /// bandwidth is held to `max_delay_phase / (2π delay)`.
    pub max_delay_phase: f64,
    /// ε is clamped to ±this.
    pub eps_max: f64,
    /// An error beyond this much playback time (seconds of source at the
    /// loop's rate: frames are `seconds × rate`) is skipped, not slewed...
    pub skip_s: f64,
    /// ...and beyond this much while unlocked (muted: a skip is free).
    pub acquire_skip_s: f64,
    /// Both thresholds are at least this many times the measured noise
    /// (the mean absolute deviation of the error from its filtered value).
    pub skip_noise_ratio: f64,
    /// A skip needs this many readings in a row beyond the threshold, and
    /// skips by their mean.
    pub skip_readings: usize,
    /// The outlier gate: once the gear has started, a reading
    /// further than this from the filtered error (seconds, as the skips) is
    /// replaced by the filtered error, at most [`Self::gate_readings`] in a
    /// row (the next goes in as it is, so a real phase jump still skips).
    /// 0: no gate.
    pub gate_s: f64,
    pub gate_readings: usize,
    /// Locked once the filtered error stays within this much playback time
    /// (seconds)...
    pub lock_error_s: f64,
    /// ...for this many seconds, with the bandwidth down to `lock_hz`.
    pub lock_s: f64,
    pub lock_hz: f64,
    /// Unlocked when the filtered error leaves this much (seconds).
    pub unlock_error_s: f64,
    /// The placement floor (seconds): a placed follower is heard once its
    /// error is within this with confidence, and muted when it leaves it.
    pub place_error_s: f64,
    /// Readings a placement needs, at least.
    pub place_readings: usize,
    /// The confidence: the estimate's standard errors added to its
    /// distance from the follower's position (the readings come in pairs
    /// sharing a master reading, so this counts each about twice).
    pub place_sigmas: f64,
    /// The least reading noise a placement assumes (seconds, a standard
    /// deviation): quantised readings can agree exactly.
    pub place_noise_s: f64,
}

impl Default for LoopConfig {
    /// The constants the simulation picked.
    fn default() -> Self {
        Self {
            acquire_hz: 0.3,
            track_hz: 0.015,
            gear_k: 0.2,
            reacquire_hz: 0.3,
            zero_ratio: 4.0,
            pole_ratio: 24.0,
            poles: 12,
            max_delay_phase: 0.2,
            eps_max: EPS_MAX,
            // The frame thresholds at 48 kHz (256, 16, 0.5 and 8 frames), in
            // time, so 192 kHz gets four times as many.
            skip_s: 256.0 / 48_000.0,
            acquire_skip_s: 16.0 / 48_000.0,
            skip_noise_ratio: 8.0,
            skip_readings: 3,
            gate_s: 0.5e-3,
            gate_readings: 2,
            lock_error_s: 0.5 / 48_000.0,
            lock_s: 2.0,
            lock_hz: 0.1,
            unlock_error_s: 8.0 / 48_000.0,
            // 0.05 ms: 2.4 frames at 48 kHz, a couple of degrees at a
            // 100 Hz crossover.
            place_error_s: 0.05e-3,
            place_readings: 12,
            place_sigmas: 4.0,
            place_noise_s: 0.5 / 48_000.0,
        }
    }
}

/// Why a loop was not built.
#[derive(Clone, Debug, PartialEq)]
pub enum LoopError {
    /// A constant is out of range (named).
    Config(&'static str),
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(what) => write!(f, "drift loop: bad {what}"),
        }
    }
}

impl std::error::Error for LoopError {}

/// What one loop update asks of the follower.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Correction {
    /// ε at the end of the coming period.
    pub eps: f64,
    /// dε/dt there, per second.
    pub eps_slope: f64,
    /// Whole source frames to skip before the coming period (negative:
    /// repeat). A skip unlocks the loop.
    pub skip: i64,
    /// The follower may be heard (it fades in when this turns true and out
    /// when it turns false).
    pub locked: bool,
}

/// The drift loop (module docs). One [`DriftLoop::update`] per follower
/// period, with the phase error measured at the period's wakeup.
#[derive(Clone, Debug)]
pub struct DriftLoop {
    cfg: LoopConfig,
    /// Plant gain: source frames per second per unit ε.
    gain: f64,
    /// The loop's own delay, seconds.
    delay_s: f64,
    /// Current bandwidth, Hz.
    bw: f64,
    /// Low-pass stage outputs, frames.
    x: [f64; MAX_POLES],
    /// The input held over the last interval, frames.
    input: f64,
    /// The PI's integral, ε units.
    integ: f64,
    /// Gains at the current bandwidth.
    kp: f64,
    ki: f64,
    /// Pole rate, 1/s.
    a: f64,
    started: bool,
    /// Mean absolute deviation of the error from the filtered error.
    noise: f64,
    /// Readings in a row beyond the skip threshold, and their sum.
    over: usize,
    over_sum: f64,
    /// The gear has run since the alignment (the gate is armed), and the
    /// readings in a row beyond the gate.
    geared: bool,
    gated: usize,
    /// Readings the gate replaced, in all.
    gated_total: u64,
    locked: bool,
    /// Seconds the lock condition has held.
    lock_run: f64,
    /// The rate state is good (a lock, or a seed): a phase jump places
    /// the follower rather than acquiring again.
    trusted: bool,
    /// Locked by a placement: heard, the error checked against the floor
    /// until the lock condition has held (then the usual lock).
    placed: bool,
    /// Placements in a row that did not hold.
    place_fails: u32,
    /// The newest readings since the last placement or skip (frames), a
    /// ring of `place_n` of them.
    place: [f64; PLACE_LEN],
    /// When each was taken (the loop's own time, seconds).
    place_t: [f64; PLACE_LEN],
    /// The loop's own correction played by each (`played` then).
    place_p: [f64; PLACE_LEN],
    /// The correction ε has played since the readings were cleared
    /// (frames): what a reading's error would have grown by more without
    /// it.
    played: f64,
    place_n: usize,
    place_head: usize,
    /// The loop's time (the update intervals summed), seconds.
    now: f64,
    /// What the readings' bias may still be off from its long-run mean
    /// (frames, [`Self::set_bias_bound`]): placement, the lock and the
    /// out-of-floor check count it.
    bias: f64,
    /// How far the readings may stray from the follower's place without it
    /// being out of it (frames, [`Self::set_spread`]): a coarse grid not
    /// yet measured.
    spread: f64,
    /// Seeded at the nominal rate: jumps to the readings' slope left (the
    /// rate taken from them at once rather than through the integral),
    /// and the loop's time of the last (the next looks at readings since).
    nominal: u32,
    nominal_since: f64,
    /// Since it was armed, the slope has been looked at once (a jump, or
    /// known within its precision and nothing to jump): placed from here.
    nominal_looked: bool,
    /// Seeded for a pair never seen (a first start): until its rate is
    /// taken (or looked at) and its error is within the placement floor and
    /// not drifting out of it, or the loop has locked, heard within
    /// `FIRST_FLOOR_S` rather than the floor.
    first_start: bool,
}

impl DriftLoop {
    /// A loop for a follower playing `rate` source frames per second, with
    /// a loop delay of `delay_s` (the device buffer plus a period).
    pub fn new(cfg: LoopConfig, rate: f64, delay_s: f64) -> Result<Self, LoopError> {
        let pos = |v: f64| v.is_finite() && v > 0.0;
        let nonneg = |v: f64| v.is_finite() && v >= 0.0;
        if !pos(cfg.acquire_hz) || !pos(cfg.track_hz) || cfg.track_hz > cfg.acquire_hz {
            return Err(LoopError::Config("bandwidths"));
        }
        if !pos(cfg.reacquire_hz) || !nonneg(cfg.gear_k) {
            return Err(LoopError::Config("gear"));
        }
        if !pos(cfg.zero_ratio) || !pos(cfg.pole_ratio) || !(1..=MAX_POLES).contains(&cfg.poles) {
            return Err(LoopError::Config("loop shape"));
        }
        if !pos(cfg.max_delay_phase) || !pos(rate) || !nonneg(delay_s) {
            return Err(LoopError::Config("rate or delay"));
        }
        if !pos(cfg.eps_max)
            || cfg.eps_max > 0.01
            || !pos(cfg.skip_s)
            || !pos(cfg.acquire_skip_s)
            || !nonneg(cfg.skip_noise_ratio)
            || cfg.skip_readings == 0
            || !nonneg(cfg.gate_s)
        {
            return Err(LoopError::Config("limits"));
        }
        if !pos(cfg.lock_error_s) || !nonneg(cfg.lock_s) || !pos(cfg.lock_hz) {
            return Err(LoopError::Config("lock"));
        }
        if !nonneg(cfg.unlock_error_s - cfg.lock_error_s) {
            return Err(LoopError::Config("unlock"));
        }
        if !pos(cfg.place_error_s)
            || !(4..=PLACE_LEN).contains(&cfg.place_readings)
            || !nonneg(cfg.place_sigmas)
            || !pos(cfg.place_noise_s)
        {
            return Err(LoopError::Config("placement"));
        }
        let mut l = Self {
            cfg,
            gain: rate,
            delay_s,
            bw: 0.0,
            x: [0.0; MAX_POLES],
            input: 0.0,
            integ: 0.0,
            kp: 0.0,
            ki: 0.0,
            a: 0.0,
            started: false,
            noise: 0.0,
            over: 0,
            over_sum: 0.0,
            geared: false,
            gated: 0,
            gated_total: 0,
            locked: false,
            lock_run: 0.0,
            trusted: false,
            placed: false,
            place_fails: 0,
            place: [0.0; PLACE_LEN],
            place_t: [0.0; PLACE_LEN],
            place_p: [0.0; PLACE_LEN],
            played: 0.0,
            place_n: 0,
            place_head: 0,
            now: 0.0,
            bias: 0.0,
            spread: 0.0,
            nominal: 0,
            nominal_since: f64::NEG_INFINITY,
            nominal_looked: true,
            first_start: false,
        };
        l.set_gains(l.max_bandwidth(cfg.acquire_hz));
        Ok(l)
    }

    /// `hz`, held to what the loop delay allows.
    fn max_bandwidth(&self, hz: f64) -> f64 {
        let cap = if self.delay_s > 0.0 {
            self.cfg.max_delay_phase / (2.0 * PI * self.delay_s)
        } else {
            f64::INFINITY
        };
        hz.min(cap).max(self.cfg.track_hz)
    }

    /// Move to bandwidth `hz` without a jump in ε: the integral takes up
    /// the proportional term's change, so the correction the loop is making
    /// carries over into the narrower loop (without it, every narrowing step
    /// drops part of the correction and the narrow loop has to win it back
    /// slowly). Not while ε is clamped: the integral would wind up.
    fn set_bandwidth(&mut self, hz: f64) {
        let eps = self.eps();
        self.set_gains(hz);
        if eps.abs() <= self.cfg.eps_max {
            self.integ = eps - self.kp * self.x[self.cfg.poles - 1];
        }
    }

    /// The gains for bandwidth `hz`, the states left as they are.
    pub(crate) fn set_gains(&mut self, hz: f64) {
        let wc = 2.0 * PI * hz;
        self.bw = hz;
        self.kp = wc / self.gain;
        self.ki = self.kp * wc / self.cfg.zero_ratio;
        self.a = wc * self.cfg.pole_ratio;
    }

    /// The follower's source rate changed (a speed change): the plant gain.
    /// ε stays where it is.
    pub fn set_rate(&mut self, rate: f64) {
        if rate.is_finite() && rate > 0.0 {
            self.gain = rate;
            self.set_bandwidth(self.bw);
        }
    }

    /// Back to the state of [`Self::new`] (a new follower stream on another
    /// device). Keep the loop across xruns: [`Self::update`] skips.
    pub fn reset(&mut self) {
        self.x = [0.0; MAX_POLES];
        self.input = 0.0;
        self.integ = 0.0;
        self.kp = 0.0;
        self.started = false;
        self.noise = 0.0;
        self.over = 0;
        self.over_sum = 0.0;
        self.geared = false;
        self.gated = 0;
        self.locked = false;
        self.lock_run = 0.0;
        self.trusted = false;
        self.placed = false;
        self.place_fails = 0;
        self.nominal = 0;
        self.first_start = false;
        self.place_clear();
        self.set_gains(self.max_bandwidth(self.cfg.acquire_hz));
    }

    /// A fresh loop with a remembered rate state (`eps`, the follower's
    /// last locked ε on this device): its first readings place it, at the
    /// lock bandwidth, without an acquisition. A rate out of range is not
    /// taken.
    pub fn seed(&mut self, eps: f64) {
        if !eps.is_finite() || eps.abs() > self.cfg.eps_max {
            return;
        }
        self.reset();
        self.integ = eps;
        self.trusted = true;
        self.set_gains(self.max_bandwidth(self.cfg.lock_hz.min(self.cfg.acquire_hz)));
    }

    /// A fresh loop for a pair never seen before: placed from its first
    /// readings at the nominal rate (ε 0), heard at once, at the
    /// acquisition bandwidth so it pulls the real rate in while heard.
    /// Until its rate is taken and its error is within the placement floor
    /// (or it has locked), heard within `FIRST_FLOOR_S` rather than that
    /// floor: an unknown
    /// clock drifts it meanwhile (±100 ppm: 4.8 frames a second at 48 kHz),
    /// and the sub is heard from the first second. Repeated failed
    /// placements fall back to an acquisition.
    pub fn seed_nominal(&mut self) {
        self.reset();
        self.trusted = true;
        self.nominal = NOMINAL_JUMPS;
        self.nominal_since = f64::NEG_INFINITY;
        self.nominal_looked = false;
        self.first_start = true;
        // Twice the acquisition's bandwidth (the loop delay allowing): an
        // unknown rate pulled in while heard stays nearer its place.
        self.set_gains(self.max_bandwidth(2.0 * self.cfg.acquire_hz));
    }

    /// What the readings' bias may still be off from its long-run mean,
    /// frames (the devices' short-window reading bias, where it is not
    /// taken out of the readings): placement waits for the floor less it,
    /// and so does the lock.
    pub fn set_bias_bound(&mut self, frames: f64) {
        // (Infinite: not known, nothing is placed.)
        self.bias = if frames.is_nan() {
            0.0
        } else {
            frames.max(0.0)
        };
    }

    /// How far the readings may stray from the follower's place (frames): a
    /// device reading on a coarse grid whose bias is not measured yet. The
    /// follower is placed and heard regardless (it may sit that far off
    /// until the bias is measured), and the gate, the skip threshold and
    /// the out-of-floor checks allow for it, so the loop averages the
    /// readings' sweep rather than chasing it or muting on it.
    pub fn set_spread(&mut self, frames: f64) {
        let spread = if frames.is_finite() {
            frames.max(0.0)
        } else {
            0.0
        };
        if self.spread > 0.0 && spread == 0.0 && self.trusted {
            // The bias is known now. The readings move by what it was
            // (up to half a packet): placed again from them (a skip, not a
            // step through the filters). The rate state followed their
            // sweep until here: taken from their slope again, the rest
            // pulled in at the lock bandwidth (as a seeded loop's).
            self.realign();
            self.nominal = NOMINAL_JUMPS;
            self.nominal_since = self.now;
            self.nominal_looked = false;
            // (A first start is long over: placed by the floor.)
            self.first_start = false;
            let lock = self.max_bandwidth(self.cfg.lock_hz);
            if self.bw < lock {
                self.set_gains(lock);
            }
        }
        self.spread = spread;
    }

    /// What may move the readings away from the follower's place (frames):
    /// the bias bound or the spread, the wider.
    fn wide(&self) -> f64 {
        self.bias.max(self.spread)
    }

    /// The floor the follower is heard within (frames): the placement
    /// floor, or `FIRST_FLOOR_S` in a first start.
    fn heard_floor(&self) -> f64 {
        self.frames(if self.first_start {
            FIRST_FLOOR_S
        } else {
            self.cfg.place_error_s
        })
    }

    /// The floor a placement is judged against (frames): the readings'
    /// spread on top (a coarse grid not yet measured is heard regardless).
    fn place_floor(&self) -> f64 {
        self.heard_floor() + self.spread
    }

    /// A rate taken from the readings is armed and has not been looked at
    /// yet: not placed before it has (the rate state may be far off).
    /// (Not while a coarse grid's sweep keeps the slope from it; a first
    /// start is placed within its wider floor meanwhile.)
    fn rate_pending(&self) -> bool {
        self.nominal > 0 && !self.nominal_looked && self.spread == 0.0 && !self.first_start
    }

    /// How far ahead the readings' line is checked (seconds): what the
    /// loop does now reaches the device after its delay.
    fn ahead(&self) -> f64 {
        PLACE_AHEAD_S + self.delay_s
    }

    /// A loop seeded at the nominal rate takes its rate from the readings'
    /// slope once it is clear of their noise (an unknown rate pulled in by
    /// the integral drifts the follower out of its floor first). Returns
    /// whether it moved.
    fn nominal_rate(&mut self) -> bool {
        // (A coarse grid's sweep is in the readings' slope until its bias
        // is known: not the rate.)
        if self.nominal == 0 || self.spread > 0.0 || self.place_n < 2 * self.cfg.place_readings {
            return false;
        }
        let least = 2 * self.cfg.place_readings;
        // The readings with the loop's own correction added back grow by
        // the follower's rate against the master's (frames per second; ε
        // in rate units): a placed loop's correction moves meanwhile.
        let (Some(l), Some(c)) = (
            self.place_line_since(least, self.nominal_since, false),
            self.place_line_since(least, self.nominal_since, true),
        ) else {
            return false;
        };
        let slope_se = (c.s2 / c.stt).sqrt();
        // (Or a full set of readings since the last look over half a second
        // at least: some of the oldest have no correction history for the
        // line, and a short period's set spans a fraction of a second,
        // its slope too rough to take.)
        let at = |i: usize| (self.place_head + PLACE_LEN - self.place_n + i) % PLACE_LEN;
        let full = self.place_t[at(0)] > self.nominal_since
            && self.place_n >= PLACE_LEN
            && self.place_t[at(self.place_n - 1)] - self.place_t[at(0)] >= NOMINAL_SPAN_S;
        let known = slope_se <= NOMINAL_SE * self.gain || full;
        if !known {
            return false;
        }
        self.nominal_looked = true;
        let eps = c.slope / self.gain;
        if ((eps - self.integ) * self.gain).abs() > NOMINAL_SIGMAS * slope_se {
            if eps.abs() <= self.cfg.eps_max {
                self.integ = eps;
            }
            self.nominal -= 1;
            self.nominal_since = self.now;
            if !self.started {
                // The held readings as at the new rate (their line flat
                // from here): they still count for the placement and the
                // next look. (A placed loop's readings are its own.)
                for i in 0..self.place_n {
                    let at = (self.place_head + PLACE_LEN - self.place_n + i) % PLACE_LEN;
                    self.place[at] -= l.slope * (self.place_t[at] - self.now);
                }
            }
            true
        } else {
            // As near as the readings tell: the loop's from here.
            self.nominal = 0;
            false
        }
    }

    /// The follower's phase may have jumped (the master's stream
    /// restarted): readings from here place it anew. With a good rate state
    /// (locked, or seeded), muted until the placement, the rate state and
    /// the bandwidth kept; otherwise nothing changes (an acquisition skips
    /// a jump by itself). Returns whether it places.
    pub fn realign(&mut self) -> bool {
        if self.locked && !self.placed {
            self.trusted = true;
        }
        if !self.trusted {
            return false;
        }
        self.started = false;
        self.locked = false;
        self.placed = false;
        self.lock_run = 0.0;
        self.over = 0;
        self.over_sum = 0.0;
        self.gated = 0;
        self.place_clear();
        self.place_fails = 0;
        // ε holds the rate state until the placement (the filters held the
        // old phase's error: its proportional part would drift the
        // follower meanwhile, and a rate taken from the readings then
        // would count that drift).
        self.x = [0.0; MAX_POLES];
        self.input = 0.0;
        true
    }

    /// The readings held for a placement no longer compare with the next
    /// ones (a device's reading bias is taken out of them now, or no
    /// longer): they go.
    pub fn forget_readings(&mut self) {
        self.place_clear();
    }

    /// The rate state is good: a phase jump places the follower.
    pub fn trusted(&self) -> bool {
        self.trusted
    }

    /// Locked by a placement, not yet by the lock condition.
    pub fn placed(&self) -> bool {
        self.placed
    }

    /// The rate state, ε units (what [`Self::seed`] takes).
    pub fn rate_eps(&self) -> f64 {
        self.integ
    }

    /// A placement failed (muted again, or a jump while placing): after a
    /// few in a row the rate state is not trusted, and the loop acquires
    /// (at the reacquisition bandwidth, as after a skip).
    fn place_failed(&mut self) {
        // (Not the rate's fault while the readings' bias alone keeps it
        // from the floor.)
        if self.wide() > self.frames(self.cfg.place_error_s) {
            return;
        }
        self.place_fails += 1;
        if self.place_fails > 3 {
            self.trusted = false;
            self.placed = false;
            self.set_gains(self.bw.max(self.max_bandwidth(self.cfg.reacquire_hz)));
        }
    }

    fn place_clear(&mut self) {
        self.place_n = 0;
        self.place_head = 0;
        self.played = 0.0;
    }

    fn place_push(&mut self, e: f64) {
        self.place_t[self.place_head] = self.now;
        self.place[self.place_head] = e;
        self.place_p[self.place_head] = self.played;
        self.place_head = (self.place_head + 1) % PLACE_LEN;
        self.place_n = (self.place_n + 1).min(PLACE_LEN);
    }

    /// The placement estimate from the newest `newest` held readings (at
    /// most): their interquartile mean and its standard error (frames),
    /// `None` with too few.
    fn place_estimate(&self, newest: usize) -> Option<(f64, f64)> {
        let n = self.place_n.min(newest);
        if n < self.cfg.place_readings {
            return None;
        }
        let mut v = [0.0; PLACE_LEN];
        for (i, x) in v[..n].iter_mut().enumerate() {
            *x = self.place[(self.place_head + PLACE_LEN - n + i) % PLACE_LEN];
        }
        let v = &mut v[..n];
        v.sort_unstable_by(f64::total_cmp);
        let (lo, hi) = (n / 4, n - n / 4);
        let mid = &v[lo..hi];
        let mean = mid.iter().sum::<f64>() / mid.len() as f64;
        // σ from the interquartile range (as for a normal spread; a
        // uniform one is narrower), at least the floor's noise.
        let iqr = v[(3 * n) / 4 - 1] - v[n / 4];
        let sigma = (iqr / 1.349).max(self.frames(self.cfg.place_noise_s));
        // The interquartile mean's efficiency is about 0.83 of the mean's.
        let se = 1.1 * sigma / (n as f64).sqrt();
        Some((mean, se))
    }

    /// The error `ahead` seconds from now, from a straight line through the
    /// held readings (those within 4 σ of their median: an outlier does not
    /// tip it), and its standard error (frames). `None` with fewer than
    /// `least`.
    fn place_predict(&self, ahead: f64, least: usize) -> Option<(f64, f64)> {
        let l = self.place_line(least)?;
        let dt = ahead - l.tm;
        let pred = l.ym + l.slope * dt;
        let se = (l.s2 * (1.0 / l.k + dt * dt / l.stt)).sqrt();
        Some((pred, se))
    }

    /// The straight line through the held readings (those within 4 σ of
    /// their median), `None` with fewer than `least`.
    fn place_line(&self, least: usize) -> Option<PlaceLine> {
        self.place_line_since(least, f64::NEG_INFINITY, false)
    }

    /// [`Self::place_line`] through the readings taken after `since` (the
    /// loop's time) only; with `played`, each with the loop's correction
    /// since added back (the line of the follower's rate against the
    /// master's).
    fn place_line_since(&self, least: usize, since: f64, played: bool) -> Option<PlaceLine> {
        let n = self.place_n;
        if n < least.max(3) {
            return None;
        }
        let at = |i: usize| (self.place_head + PLACE_LEN - n + i) % PLACE_LEN;
        let mut v = [0.0; PLACE_LEN];
        for (i, x) in v[..n].iter_mut().enumerate() {
            *x = self.place[at(i)];
        }
        v[..n].sort_unstable_by(f64::total_cmp);
        let med = v[n / 2];
        let sigma =
            ((v[(3 * n) / 4 - 1] - v[n / 4]) / 1.349).max(self.frames(self.cfg.place_noise_s));
        // The readings used (outliers told from the readings themselves)
        // and their values: with `played`, the correction played by a
        // loop's delay before each (what has reached it), interpolated
        // between the readings' times; one without that much history is
        // not used.
        let mut y = [0.0; PLACE_LEN];
        let mut used = [false; PLACE_LEN];
        let mut j = 0;
        for i in 0..n {
            let a = at(i);
            used[i] = self.place_t[a] > since && (self.place[a] - med).abs() <= 4.0 * sigma;
            y[i] = self.place[a];
            if played && used[i] {
                let back = self.place_t[a] - self.delay_s;
                while j + 1 < n && self.place_t[at(j + 1)] <= back {
                    j += 1;
                }
                let (t0, p0) = (self.place_t[at(j)], self.place_p[at(j)]);
                if t0 > back {
                    used[i] = false;
                } else if j + 1 < n && self.place_t[at(j + 1)] > t0 {
                    let (t1, p1) = (self.place_t[at(j + 1)], self.place_p[at(j + 1)]);
                    y[i] += p0 + (p1 - p0) * ((back - t0) / (t1 - t0)).min(1.0);
                } else {
                    y[i] += p0;
                }
            }
        }
        let (mut k, mut st, mut sy) = (0.0, 0.0, 0.0);
        for i in 0..n {
            if used[i] {
                k += 1.0;
                st += self.place_t[at(i)] - self.now;
                sy += y[i];
            }
        }
        if k < least.max(3) as f64 {
            return None;
        }
        let (tm, ym) = (st / k, sy / k);
        let (mut stt, mut sty) = (0.0, 0.0);
        for i in 0..n {
            if used[i] {
                let t = self.place_t[at(i)] - self.now;
                stt += (t - tm) * (t - tm);
                sty += (t - tm) * (y[i] - ym);
            }
        }
        if stt <= 0.0 {
            return None;
        }
        let slope = sty / stt;
        let mut ss = 0.0;
        for i in 0..n {
            if used[i] {
                let t = self.place_t[at(i)] - self.now;
                let r = y[i] - ym - slope * (t - tm);
                ss += r * r;
            }
        }
        let s2 = (ss / (k - 2.0).max(1.0)).max(self.frames(self.cfg.place_noise_s).powi(2));
        Some(PlaceLine {
            ym,
            tm,
            slope,
            s2,
            k,
            stt,
        })
    }

    /// `s` seconds of playback in source frames at the loop's rate.
    fn frames(&self, s: f64) -> f64 {
        s * self.gain
    }

    /// Readings the outlier gate replaced since the loop was built.
    pub fn gated(&self) -> u64 {
        self.gated_total
    }

    /// ε now (the model's value at the last update's end).
    pub fn eps(&self) -> f64 {
        self.kp * self.x[self.cfg.poles - 1] + self.integ
    }

    /// The loop's rate estimate: the follower's clock offset relative to
    /// the master, ppm (`−ε` to first order).
    pub fn ppm(&self) -> f64 {
        -self.integ * 1e6
    }

    /// The filtered phase error, frames.
    pub fn filtered_error(&self) -> f64 {
        self.x[self.cfg.poles - 1]
    }

    /// The current bandwidth, Hz.
    pub fn bandwidth(&self) -> f64 {
        self.bw
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// The first readings have aligned the follower (its first skip).
    pub fn aligned(&self) -> bool {
        self.started
    }

    /// Readings beyond the skip threshold are counting towards a skip (the
    /// follower may be far from where it should be).
    pub fn skip_pending(&self) -> bool {
        self.started && self.over > 0
    }

    /// Unlock (the follower is muted from here): a phase step that is
    /// coming (the master's alignment changed) is then skipped at the
    /// acquisition threshold rather than slewed. The rate state is kept.
    pub fn unlock(&mut self) {
        self.locked = false;
        self.lock_run = 0.0;
    }

    /// One update: `error` (source frames, from [`PhaseDetector`]) was
    /// measured now; the coming period lasts `h` seconds. Returns ε and its
    /// slope at that period's end, and any skip. A non-finite error or `h`
    /// holds ε where it is.
    pub fn update(&mut self, error: f64, h: f64) -> Correction {
        if h.is_finite() && h > 0.0 {
            self.now += h;
            // (What ε moved the follower by over the period just played.)
            self.played += self.frames(h) * self.eps().clamp(-self.cfg.eps_max, self.cfg.eps_max);
        }
        if !error.is_finite() || !(h.is_finite() && h > 0.0) {
            return self.hold();
        }
        let m = self.cfg.poles;
        let mut u = error;
        let mut skip = 0;
        if !self.started && self.trusted {
            // Placement: skip by the readings' estimate as soon as the
            // follower is then within the floor with confidence (heard at
            // once), or, with the readings held full, skip by it muted.
            self.place_push(u);
            if self.nominal_rate() {
                return self.hold();
            }
            let Some((mean, se)) = self.place_estimate(PLACE_LEN) else {
                return self.hold();
            };
            let k = mean.round();
            let floor = self.place_floor();
            // (Not drifting out either: the readings' line past the loop's
            // delay.)
            let sure = !self.rate_pending()
                && (mean - k).abs() + (self.cfg.place_sigmas * se).hypot(self.bias) <= floor
                && self
                    .place_predict(self.ahead(), self.cfg.place_readings)
                    .is_some_and(|(p, pse)| (p - k).abs() + pse.hypot(self.bias) <= floor);
            if !sure && self.place_n < PLACE_LEN {
                return self.hold();
            }
            skip = k as i64;
            u = mean - k;
            self.x[..m].fill(u);
            self.place_clear();
            self.over = 0;
            self.over_sum = 0.0;
            self.started = true;
            self.locked = sure;
            self.placed = sure;
            self.lock_run = 0.0;
            return Correction {
                eps: self.eps().clamp(-self.cfg.eps_max, self.cfg.eps_max),
                eps_slope: 0.0,
                skip,
                locked: self.locked,
            };
        } else if !self.started {
            // Alignment: average the first readings, skip by their mean,
            // and start the filters at the residual (no step through them).
            self.over += 1;
            self.over_sum += u;
            if self.over < self.cfg.skip_readings {
                return self.hold();
            }
            let mean = self.over_sum / self.over as f64;
            skip = mean.round() as i64;
            u = mean - skip as f64;
            self.x[..m].fill(u);
            self.over = 0;
            self.over_sum = 0.0;
            self.started = true;
        } else {
            // The outlier gate: a reading far from the filtered
            // error is replaced by it, a few in a row at most. (The
            // readings' bias may spread them by twice its bound: a sweep
            // across a coarse grid is averaged, not gated or skipped.)
            let spread = if self.wide().is_finite() {
                3.0 * self.wide()
            } else {
                0.0
            };
            let gate = self.frames(self.cfg.gate_s).max(spread);
            if self.geared && gate > 0.0 && (u - self.x[m - 1]).abs() > gate {
                self.gated += 1;
                if self.gated <= self.cfg.gate_readings {
                    u = self.x[m - 1];
                    self.gated_total += 1;
                }
            } else {
                self.gated = 0;
            }
            if self.trusted {
                self.place_push(error);
                let ahead = self.place_predict(self.delay_s, 2 * self.cfg.place_readings);
                if self.nominal_rate()
                    && let Some((p, _)) = ahead
                {
                    // Placed, the rate taken: the phase error the old rate
                    // left skipped out (faded; a delay-bound loop pulls it
                    // in over seconds, past the floor and back), as it
                    // stands once what is queued has played.
                    skip = p.round() as i64;
                    u = p - skip as f64;
                    self.x[..m].fill(u);
                    self.place_clear();
                    self.lock_run = 0.0;
                }
            }
            let limit = self
                .frames(if self.locked && !self.placed {
                    self.cfg.skip_s
                } else {
                    self.cfg.acquire_skip_s
                })
                .max(self.cfg.skip_noise_ratio * self.noise)
                .max(spread);
            // The noise, from deviations short of the threshold (a real
            // offset is not noise), over about a second.
            let dev = (u - self.x[m - 1]).abs().min(limit);
            self.noise += (1.0 - (-h).exp()) * (dev - self.noise);
            if u.abs() > limit {
                self.over += 1;
                self.over_sum += u;
            } else {
                self.over = 0;
                self.over_sum = 0.0;
            }
            if self.over >= self.cfg.skip_readings && self.trusted {
                // A phase jump with a good rate state: placed anew from the
                // readings that follow (muted), at most a few times in a
                // row; then an acquisition.
                let fails = self.place_fails;
                self.realign();
                self.place_fails = fails;
                self.place_failed();
                return self.hold();
            }
            if self.over >= self.cfg.skip_readings {
                skip = (self.over_sum / self.over as f64).round() as i64;
                self.over = 0;
                self.over_sum = 0.0;
                u -= skip as f64;
                // The filters restart at the residual; the integral (the
                // rate state) is kept, so ε drops back to the rate
                // estimate.
                self.x[..m].fill(u);
                self.locked = false;
                self.lock_run = 0.0;
                self.set_gains(self.bw.max(self.max_bandwidth(self.cfg.reacquire_hz)));
            } else if self.over > 0 {
                // A reading that may start a skip does not move the loop.
                return self.hold();
            }
        }

        // The gear: 1/b grows by 2πk per second.
        if self.bw > self.cfg.track_hz {
            let next = 1.0 / (1.0 / self.bw + 2.0 * PI * self.cfg.gear_k * h);
            self.set_bandwidth(next.max(self.cfg.track_hz));
        }

        let integ = self.integ;
        self.propagate(u, h);
        self.geared = true;

        // Clamp; the integral does not move while clamped (no windup).
        let mut eps = self.eps();
        let mut slope = self.slope();
        if eps.abs() > self.cfg.eps_max {
            self.integ = integ;
            eps = eps.clamp(-self.cfg.eps_max, self.cfg.eps_max);
            slope = 0.0;
        }

        // Lock: the filtered error small for long enough, the bandwidth
        // down to `lock_hz`.
        let fe = self.x[m - 1].abs();
        // (A first start's wider floor until its rate is taken.)
        let floor = self.heard_floor();
        // (Not while the readings' bias may put the follower outside the
        // floor.)
        let regular = fe < self.frames(self.cfg.lock_error_s)
            && self.bw <= self.cfg.lock_hz
            && self.bias <= self.frames(self.cfg.place_error_s);
        if self.locked && self.placed {
            // Heard by a placement: muted when the error is surely outside
            // the floor (the newest readings' estimate beyond three
            // standard errors: a drift shows within about half a second;
            // fewer readings or a narrower margin muted a follower in place
            // on noise; or the filtered error); the usual lock once its
            // condition has held.
            // A drift is caught before it crosses: the readings' line, past
            // the loop's delay. (A coarse grid's spread is allowed for.)
            let wide = self.wide();
            let out = fe > floor + self.spread
                || self
                    .place_estimate(2 * self.cfg.place_readings)
                    .is_some_and(|(e, se)| e.abs() - (3.0 * se).hypot(wide) > floor)
                || self
                    .place_predict(self.ahead(), 2 * self.cfg.place_readings)
                    .is_some_and(|(e, se)| e.abs() - (3.0 * se).hypot(wide) > floor);
            let place = self.frames(self.cfg.place_error_s);
            if self.first_start
                && !out
                && (self.nominal == 0 || self.nominal_looked)
                && fe <= place
                && self
                    .place_estimate(2 * self.cfg.place_readings)
                    .is_some_and(|(e, se)| e.abs() + (3.0 * se).hypot(wide) <= place)
                && self
                    .place_predict(self.ahead(), 2 * self.cfg.place_readings)
                    .is_some_and(|(e, se)| e.abs() + (3.0 * se).hypot(wide) <= place)
            {
                // A first start's rate is taken (or looked at), its error
                // within the placement floor and not drifting out of it:
                // the placement floor from here.
                self.first_start = false;
            }
            if out {
                self.locked = false;
                self.placed = false;
                self.lock_run = 0.0;
                self.place_failed();
            } else if regular {
                self.lock_run += h;
                if self.lock_run >= self.cfg.lock_s {
                    self.placed = false;
                    self.place_fails = 0;
                    self.nominal = 0;
                    self.first_start = false;
                }
            } else {
                self.lock_run = 0.0;
            }
        } else if self.locked {
            if fe > self.frames(self.cfg.unlock_error_s).max(3.0 * self.spread) {
                self.locked = false;
                self.lock_run = 0.0;
            }
        } else if self.trusted
            && !self.rate_pending()
            && let Some((e, se)) = self.place_estimate(PLACE_LEN)
            && e.abs() + (self.cfg.place_sigmas * se).hypot(self.bias) <= self.place_floor()
            && fe <= self.place_floor()
            && self
                .place_predict(self.ahead(), self.cfg.place_readings)
                .is_some_and(|(p, pse)| p.abs() + pse.hypot(self.bias) <= self.place_floor())
        {
            // Placed (muted) and now within the floor with confidence, and
            // not drifting out of it (the readings' line past the loop's
            // delay).
            self.locked = true;
            self.placed = true;
            self.lock_run = 0.0;
        } else if self.trusted
            && self.wide() <= floor
            && let Some((e, se)) = self.place_estimate(PLACE_LEN)
            && e.abs() - (self.cfg.place_sigmas * se).hypot(self.bias) > floor
        {
            // Muted and surely outside the floor: placed again from the
            // readings that follow. (Not while their bias is too wide to
            // place by: a placement would follow a sweep's phase; the loop
            // averages it.)
            let fails = self.place_fails;
            self.realign();
            self.place_fails = fails;
            self.place_failed();
        } else if regular {
            self.lock_run += h;
            if self.lock_run >= self.cfg.lock_s {
                self.locked = true;
                self.trusted = true;
                self.placed = false;
                self.place_fails = 0;
                self.nominal = 0;
                self.first_start = false;
            }
        } else {
            self.lock_run = 0.0;
        }
        Correction {
            eps,
            eps_slope: slope,
            skip,
            locked: self.locked,
        }
    }

    /// Hold ε where the model is, for a reading not used (the model does
    /// not advance; the next piece flattens out to it).
    fn hold(&self) -> Correction {
        Correction {
            eps: self.eps().clamp(-self.cfg.eps_max, self.cfg.eps_max),
            eps_slope: 0.0,
            skip: 0,
            locked: self.locked,
        }
    }

    /// dε/dt of the model now.
    fn slope(&self) -> f64 {
        let m = self.cfg.poles;
        let xm = self.x[m - 1];
        let before = if m >= 2 { self.x[m - 2] } else { self.input };
        let d = self.a * (before - xm);
        self.kp * d + self.ki * xm
    }

    /// Advance the model `h` seconds with the input held at `u`: exact for
    /// a cascade of identical one-poles and the PI's integral.
    fn propagate(&mut self, u: f64, h: f64) {
        self.input = u;
        let m = self.cfg.poles;
        let x = self.a * h;
        let decay = (-x).exp();
        let mut z = [0.0; MAX_POLES];
        for (zi, xi) in z.iter_mut().zip(&self.x[..m]) {
            *zi = xi - u;
        }
        // ∫ z_m over the interval: Σ_j z_j P(m − j + 1, x) / a.
        let mut area = 0.0;
        for (j, zj) in z[..m].iter().enumerate() {
            area += zj * lower_gamma_p(m - j, x);
        }
        area /= self.a;
        // z_i(h) = e^{−x} Σ_{j ≤ i} z_j x^{i−j} / (i−j)!.
        let mut out = [0.0; MAX_POLES];
        for i in 0..m {
            let mut acc = 0.0;
            let mut term = 1.0;
            for k in 0..=i {
                // term = x^k / k!, applied to z_{i−k}.
                acc += z[i - k] * term;
                term *= x / (k + 1) as f64;
            }
            out[i] = decay * acc;
        }
        self.integ += self.ki * (u * h + area);
        for (xi, oi) in self.x[..m].iter_mut().zip(&out[..m]) {
            *xi = u + oi;
        }
    }
}

/// The regularised lower incomplete gamma function `P(n, x)` for a whole
/// `n ≥ 1`: `1 − e^{−x} Σ_{k<n} x^k / k!`, by its series where that
/// difference would cancel.
pub fn lower_gamma_p(n: usize, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x < n as f64 + 1.0 {
        // e^{−x} Σ_{k ≥ n} x^k / k!
        let mut term = 1.0;
        for k in 1..=n {
            term *= x / k as f64;
        }
        let mut sum = 0.0;
        let mut k = n;
        while term > sum * 1e-17 && k < n + 200 {
            sum += term;
            k += 1;
            term *= x / k as f64;
        }
        (-x).exp() * sum
    } else {
        let mut term = 1.0;
        let mut sum = 0.0;
        for k in 0..n {
            sum += term;
            term *= x / (k + 1) as f64;
        }
        1.0 - (-x).exp() * sum
    }
}

/// The cubic Hermite from `from = (value, slope)` to `to` over `h`
/// seconds, at `t` (0 to `h`): its value and slope. The applied ε within a
/// period; a caller that cuts a period into several core calls takes each
/// call's end from here.
pub fn hermite(from: (f64, f64), to: (f64, f64), h: f64, t: f64) -> (f64, f64) {
    let u = (t / h).clamp(0.0, 1.0);
    let (u2, u3) = (u * u, u * u * u);
    let v = (2.0 * u3 - 3.0 * u2 + 1.0) * from.0
        + (u3 - 2.0 * u2 + u) * h * from.1
        + (-2.0 * u3 + 3.0 * u2) * to.0
        + (u3 - u2) * h * to.1;
    let d = ((6.0 * u2 - 6.0 * u) * from.0 + (-6.0 * u2 + 6.0 * u) * to.0) / h
        + (3.0 * u2 - 4.0 * u + 1.0) * from.1
        + (3.0 * u2 - 2.0 * u) * to.1;
    (v, d)
}

/// ε applied through the core's pieces: remembers ε and its slope at the
/// end of the last piece, so each period's piece starts where the last one
/// ended (C¹ across periods).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EpsilonTrack {
    eps: f64,
    slope: f64,
    /// Core outputs per second (twice the follower's nominal rate).
    outputs_per_second: f64,
}

impl EpsilonTrack {
    pub fn new(outputs_per_second: f64) -> Self {
        Self {
            eps: 0.0,
            slope: 0.0,
            outputs_per_second,
        }
    }

    /// ε and dε/dt (per second) at the end of the last piece.
    pub fn state(&self) -> (f64, f64) {
        (self.eps, self.slope)
    }

    /// Restart at `eps` with zero slope (a reset; the step jumps under a
    /// fade).
    pub fn reset(&mut self, eps: f64) {
        self.eps = eps;
        self.slope = 0.0;
    }

    /// The piece for the coming call: the master's own segment `seg`
    /// (from the master envelope at step `start`, the step at the call's
    /// first output) with `1 + ε` applied, ε running from
    /// the last end to `to = (ε, dε/dt)`. The step is `v (1 + ε)` and its
    /// slope `v′ (1 + ε) + v ε′` at both ends. `None` (and nothing kept)
    /// when the step leaves the core's range; [`Segment::Jump`] is passed
    /// through with the end ε applied.
    pub fn piece(&mut self, seg: Segment, start: Step, to: (f64, f64)) -> Option<Segment> {
        let r = self.outputs_per_second;
        let (e0, e0d) = (self.eps, self.slope / r);
        let (e1, e1d) = (to.0, to.1 / r);
        let v0 = start.to_f64();
        let (v1, vs0, vs1) = match seg {
            Segment::Hold => (v0, 0.0, 0.0),
            Segment::Piece {
                slope0,
                end,
                slope1,
            } => (end.to_f64(), slope0, slope1),
            Segment::Jump(s) => {
                let end = Step::from_f64(s.to_f64() * (1.0 + e1))?;
                self.eps = e1;
                self.slope = 0.0;
                return Some(Segment::Jump(end));
            }
        };
        let end = Step::from_f64(v1 * (1.0 + e1))?;
        let slope0 = vs0 * (1.0 + e0) + v0 * e0d;
        let slope1 = vs1 * (1.0 + e1) + v1 * e1d;
        self.eps = e1;
        self.slope = to.1;
        Some(Segment::Piece {
            slope0,
            end,
            slope1,
        })
    }
}

/// The fit a [`BiasTracker`] holds a device's readings against, seconds:
/// longer than any reading pattern, shorter than a clock's wander.
pub const BIAS_FIT_S: f64 = 30.0;
/// The window the line's slope (the device's clock against the system's)
/// is fit over, seconds.
const BIAS_SLOPE_S: f64 = 120.0;
/// The window a reading bias is judged over (what a placement averages),
/// seconds, and how long its RMS is averaged over.
const BIAS_WINDOW_S: f64 = 0.25;
const BIAS_RMS_S: f64 = 60.0;

/// One device's position readings against its own smooth clock: how far
/// each reading is from the long-run line through them (its reading bias
/// now less its long-run mean: the packet grid's phase, the wakeup's
/// latency), and how far a short window's mean of that strays (its RMS).
///
/// A device whose readings step on a coarse grid that sweeps slowly
/// against its wakeups (a USB pointer on 125 µs microframes or 1 ms
/// packets) gives short-window means that stray from the long-run one: a
/// placement from a fraction of a second of readings carries that. Once
/// the fit has run [`BIAS_FIT_S`] that RMS is measured, and once it has run
/// twice that each reading's deviation is taken out of the phase error
/// (the loop then sees the long-run mean); until then the RMS bounds what
/// the placement may be off. A bias that never moves (a
/// fixed wakeup phase) is a constant: no loop can see it (the trim).
///
/// Fixed-size and allocation-free.
#[derive(Clone, Debug, Default)]
pub struct BiasTracker {
    started: bool,
    t0: f64,
    p0: f64,
    /// Exponentially weighted least-squares sums, times relative to the
    /// last reading.
    s0: f64,
    st: f64,
    stt: f64,
    sy: f64,
    sty: f64,
    /// The same for the offset's mean (a shorter window).
    i0: f64,
    it: f64,
    iy: f64,
    last_t: f64,
    /// Seconds since the first reading.
    age: f64,
    /// Mean absolute residual, seconds (the outlier clip).
    noise: f64,
    /// The short-window mean of the deviations, and the mean square of
    /// that, seconds (the RMS is kept over a reset: a property of the
    /// device as it is read).
    win: f64,
    ms: f64,
    /// Seconds of that measured (a remembered RMS counts as
    /// [`BIAS_RMS_KNOWN_S`]).
    ms_age: f64,
    /// Outliers in a row: a run of them is a jump, and the fit starts
    /// again.
    outliers: u32,
    /// The last position, and how often the steps between readings fall
    /// on a whole number of 1 ms packets (a coarse pointer grid), with the
    /// steps counted.
    last_pos: f64,
    on_grid: f64,
    steps: u32,
    /// A finer pointer than the packet grid has been seen (a 125 µs
    /// microframe's wrap: steps off the grid often). A device whose
    /// wakeups fall on whole packets for a while steps on them while its
    /// pointer's phase holds; it is not a coarse reader for that.
    fine: bool,
    /// The line's slope (the device's clock against the system's), kept
    /// over a new stream: the line's offset is learnt again from there
    /// ([`Self::rebase`]), for `base_age` seconds until a whole fit has run
    /// again.
    slope: f64,
    kept_slope: bool,
    base_age: f64,
    /// On a coarse grid: the last reading's `y`, and where its phase
    /// wrapped (the readings either side of a packet's edge: their mean is
    /// on the long-run line, to a reading's step), `(t, y)` in this
    /// stream's frame, a ring (readings dithering across an edge are one
    /// wrap, their crossings averaged), the last crossing's time; and the
    /// line's slope from an earlier stream's wraps.
    last_y: f64,
    wraps: [(f64, f64); WRAP_LEN],
    wrap_last_t: f64,
    /// The crossings of the wrap being seen: their times and midpoints
    /// summed, their count, and the reading before them `(t, y)` (a wrap
    /// once the level has moved by a packet, along the clock's slope; an
    /// outlier and the reading back is not one).
    wrap_open: Option<(f64, f64, f64, (f64, f64))>,
    wrap_n: usize,
    wrap_head: usize,
    coarse_slope: Option<f64>,
}

/// Wraps of a coarse grid's phase a [`BiasTracker`] keeps, and how far
/// apart (seconds) the oldest and the newest must be for a line through
/// them: a whole sweep, not a reading dithering about one edge.
const WRAP_LEN: usize = 16;
const WRAP_SPAN_S: f64 = 2.0;
/// Crossings closer than this (seconds) are one wrap.
const WRAP_MERGE_S: f64 = 1.0;

/// A straight line through the held readings: their mean `ym` at time
/// `tm` (seconds from now), its `slope` (frames per second), the residuals'
/// variance `s2`, their count `k` and the times' spread `stt`.
#[derive(Clone, Copy, Debug)]
struct PlaceLine {
    ym: f64,
    tm: f64,
    slope: f64,
    s2: f64,
    k: f64,
    stt: f64,
}

/// The coarse pointer grid a [`BiasTracker`] looks for, seconds: a USB
/// packet (a reader stamped there reads up to that much behind).
const BIAS_GRID_S: f64 = 1e-3;
/// Steps between readings a [`BiasTracker`] looks at before it tells a
/// fine grid (twice that before a coarse one).
const BIAS_GRID_STEPS: u32 = 8;

/// Outliers in a row a [`BiasTracker`] takes for a jump, and the seconds
/// at its start that learn the readings' spread.
const BIAS_JUMP_READINGS: u32 = 5;
const BIAS_WARMUP_S: f64 = 2.0;
/// Seconds of short-window bias measured before its RMS bounds anything.
const BIAS_RMS_KNOWN_S: f64 = 10.0;

impl BiasTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new device or clock: the fit starts again (what was measured of
    /// how the device reads is kept).
    pub fn reset(&mut self) {
        *self = Self {
            ms: self.ms,
            ms_age: self.ms_age,
            on_grid: self.on_grid,
            fine: self.fine,
            steps: self.steps,
            ..Self::default()
        };
    }

    /// A new stream on the same clock (a restart, a step, a jump): the
    /// positions start again, the clock's rate does not. A fit that has run
    /// half a window keeps its slope and spread and learns its offset
    /// again (once established, its deviations are taken out from the
    /// third reading, and [`Self::left_s`] says what the offset may still
    /// be off); a younger one starts again. On a coarse grid the slope of
    /// the line through its wraps is kept, and the next wrap places it.
    pub fn rebase(&mut self) {
        if self.coarse_grid_s().is_some() {
            let coarse_slope = self.wrap_line().map(|(b, _, _)| b).or(self.coarse_slope);
            *self = Self {
                age: self.age,
                noise: self.noise,
                ms: self.ms,
                ms_age: self.ms_age,
                on_grid: self.on_grid,
                fine: self.fine,
                steps: self.steps,
                coarse_slope,
                ..Self::default()
            };
            return;
        }
        if !(self.started && self.age >= 0.5 * BIAS_FIT_S) {
            self.reset();
            return;
        }
        *self = Self {
            age: self.age,
            noise: self.noise,
            ms: self.ms,
            ms_age: self.ms_age,
            on_grid: self.on_grid,
            fine: self.fine,
            steps: self.steps,
            slope: self.slope,
            kept_slope: true,
            ..Self::default()
        };
    }

    /// What the line's offset may be off by after a [`Self::rebase`]
    /// (the RMS of the mean over the readings since, seconds); 0 once a
    /// whole fit has run.
    pub fn left_s(&self) -> f64 {
        if self.coarse_grid_s().is_some() {
            // (Through the wraps: to a reading's step.)
            0.0
        } else if self.kept_slope {
            self.rms_s() * (BIAS_WINDOW_S / self.base_age.max(BIAS_WINDOW_S)).sqrt()
        } else {
            0.0
        }
    }

    /// The readings step on a coarse grid (whole 1 ms packets): its step,
    /// seconds. A device on such a grid whose bias has not been measured
    /// may read anywhere within a step.
    pub fn coarse_grid_s(&self) -> Option<f64> {
        (self.steps >= 2 * BIAS_GRID_STEPS && self.on_grid > 0.9 && !self.fine)
            .then_some(BIAS_GRID_S)
    }

    /// Enough steps seen to tell a coarse grid ([`Self::coarse_grid_s`]):
    /// a fine step or a step off the grid within them, or twice as many on
    /// it (a fine pointer whose wakeups fall on whole packets for a while
    /// is not taken for a coarse one).
    pub fn grid_known(&self) -> bool {
        self.steps >= 2 * BIAS_GRID_STEPS
            || (self.steps >= BIAS_GRID_STEPS && (self.fine || self.on_grid <= 0.9))
    }

    /// The fit has run a whole window: its slope holds over a new stream,
    /// and the short-window bias against it is measured. On a coarse grid:
    /// its line through the wraps is known.
    pub fn measuring(&self) -> bool {
        if self.coarse_grid_s().is_some() {
            return self.started && self.wrap_line().is_some();
        }
        self.started && self.age >= BIAS_FIT_S && self.s0 > 2.0
    }

    /// The fit has run two windows (a younger one can be off by frames
    /// where the readings sweep, and the loop would follow it as it
    /// settles): its deviations are taken out. On a coarse grid, once its
    /// phase has wrapped a whole sweep apart (a fit to part of a slow
    /// sweep takes the sawtooth for the clock's slope, and the loop would
    /// run off with it): the line through the wraps.
    pub fn established(&self) -> bool {
        if self.coarse_grid_s().is_some() {
            return self.measuring();
        }
        self.measuring() && self.age >= 2.0 * BIAS_FIT_S
    }

    /// The short-window bias's RMS has been measured long enough to bound
    /// a placement (or was remembered). Never on a coarse grid before its
    /// line is known (what it would say is part of a sweep).
    pub fn rms_known(&self) -> bool {
        if self.coarse_grid_s().is_some() && !self.established() {
            return false;
        }
        self.ms_age >= BIAS_RMS_KNOWN_S
    }

    /// The long-run line through a coarse grid's wraps: its slope, and a
    /// point on it `(t, y)` (this stream's frame). With wraps a whole sweep
    /// apart, their own line; with one since a rebase, the kept slope.
    fn wrap_line(&self) -> Option<(f64, f64, f64)> {
        let n = self.wrap_n;
        if n == 0 {
            return None;
        }
        let at = |i: usize| self.wraps[(self.wrap_head + WRAP_LEN - n + i) % WRAP_LEN];
        let (mut st, mut sy) = (0.0, 0.0);
        for i in 0..n {
            st += at(i).0;
            sy += at(i).1;
        }
        let (tm, ym) = (st / n as f64, sy / n as f64);
        let span = at(n - 1).0 - at(0).0;
        if n >= 2 && span >= WRAP_SPAN_S {
            let (mut stt, mut sty) = (0.0, 0.0);
            for i in 0..n {
                let (t, y) = at(i);
                stt += (t - tm) * (t - tm);
                sty += (t - tm) * (y - ym);
            }
            return Some((sty / stt, tm, ym));
        }
        self.coarse_slope.map(|b| (b, tm, ym))
    }

    /// The RMS of a short window's mean bias against the long-run one,
    /// seconds.
    pub fn rms_s(&self) -> f64 {
        self.ms.sqrt()
    }

    /// A remembered RMS (seconds) for a device not measured yet this run.
    pub fn remember_rms_s(&mut self, s: f64) {
        if s.is_finite() && s > 0.0 {
            self.ms = self.ms.max(s * s);
            self.ms_age = self.ms_age.max(BIAS_RMS_KNOWN_S);
        }
    }

    /// A reading: position `pos` (frames at `rate` per second) at `t`
    /// seconds. Returns the reading's deviation from the long-run line,
    /// frames (negative: it reads a position behind the line, late), once
    /// established and when it is not an outlier; `None` otherwise (a
    /// repeated time included).
    pub fn push(&mut self, pos: f64, t: f64, rate: f64) -> Option<f64> {
        if !(pos.is_finite() && t.is_finite() && rate.is_finite() && rate > 0.0) {
            return None;
        }
        if !self.started {
            self.started = true;
            self.t0 = t;
            self.p0 = pos;
            self.last_t = t;
            self.last_pos = pos;
        }
        let dt = t - self.last_t;
        if dt < 0.0 || (dt == 0.0 && self.i0 > 0.0) {
            return None;
        }
        // Move the origin to now, then forget: windows growing to theirs.
        self.stt += -2.0 * dt * self.st + dt * dt * self.s0;
        self.sty -= dt * self.sy;
        self.st -= dt * self.s0;
        self.it -= dt * self.i0;
        let decay = (-dt / self.base_age.clamp(1.0, BIAS_SLOPE_S)).exp();
        self.s0 *= decay;
        self.st *= decay;
        self.stt *= decay;
        self.sy *= decay;
        self.sty *= decay;
        let decay = (-dt / self.base_age.clamp(1.0, BIAS_FIT_S)).exp();
        self.i0 *= decay;
        self.it *= decay;
        self.iy *= decay;
        let y = (pos - self.p0) / rate - (t - self.t0);
        if self.coarse_grid_s().is_some() && self.last_y.is_finite() && dt > 0.0 && dt < 0.2 {
            // A wrap: the phase crossed a packet's edge between this reading
            // and the last (they straddle it).
            let d = (y - self.last_y).abs();
            let tc = t - self.t0 - 0.5 * dt;
            if let Some((ts, ys, k, (tb, yb))) = self.wrap_open
                && tc - self.wrap_last_t >= WRAP_MERGE_S
            {
                // The crossings are over: a wrap if the level moved by a
                // packet across them (along the clock's slope: a slow
                // sweep's crossings can last many seconds).
                self.wrap_open = None;
                let last_t = self.last_t - self.t0;
                let slope = self.wrap_line().map_or(self.slope, |(b, _, _)| b);
                let net = self.last_y - yb - slope * (last_t - tb);
                if (0.5 * BIAS_GRID_S..=1.5 * BIAS_GRID_S).contains(&net.abs()) {
                    self.wraps[self.wrap_head] = (ts / k, ys / k);
                    self.wrap_head = (self.wrap_head + 1) % WRAP_LEN;
                    self.wrap_n = (self.wrap_n + 1).min(WRAP_LEN);
                }
            }
            if (d - BIAS_GRID_S).abs() <= 0.2 * BIAS_GRID_S {
                let yc = 0.5 * (y + self.last_y);
                self.wrap_open = Some(match self.wrap_open {
                    Some((ts, ys, k, before)) => (ts + tc, ys + yc, k + 1.0, before),
                    None => (tc, yc, 1.0, (self.last_t - self.t0, self.last_y)),
                });
                self.wrap_last_t = tc;
            }
        }
        self.last_y = y;
        let det = self.s0 * self.stt - self.st * self.st;
        if !self.kept_slope && self.s0 > 2.0 && det > 0.0 {
            self.slope = (self.s0 * self.sty - self.st * self.sy) / det;
        }
        // The slope (kept, or the long fit's) through the mean of the
        // window's readings: a mean damps a slow sweep several times more
        // than a line's newest end does.
        let fit = match self.wrap_line() {
            Some((b, tm, ym)) if self.coarse_grid_s().is_some() => {
                Some(ym + b * (t - self.t0 - tm))
            }
            _ => (self.i0 > 2.0 && (self.kept_slope || det > 0.0))
                .then(|| (self.iy - self.slope * self.it) / self.i0),
        };
        let r = fit.map_or(0.0, |f| y - f);
        // (On a coarse grid a wrap of its phase is a packet's step, not a
        // jump.)
        let grid = if self.coarse_grid_s().is_some() {
            1.5 * BIAS_GRID_S
        } else {
            0.0
        };
        let clip = (4.0 * self.noise).max(2.0 / rate).max(grid);
        // (The first seconds learn the spread, unclipped.)
        let warm = self.age < BIAS_WARMUP_S;
        let outlier = fit.is_some() && !warm && r.abs() > clip;
        if outlier {
            self.outliers += 1;
            if self.outliers >= BIAS_JUMP_READINGS {
                // A step the caller did not flag: start again from here.
                self.rebase();
                return self.push(pos, t, rate);
            }
        } else {
            self.outliers = 0;
        }
        let used = if outlier {
            y - r + r.clamp(-clip, clip)
        } else {
            y
        };
        if !outlier && dt > 0.0 {
            // On whole packets, give or take an async packet's frame.
            let q = BIAS_GRID_S * rate;
            let step = pos - self.last_pos;
            let off = step - q * (step / q).round();
            let hit = if off.abs() <= (0.1 * q).max(1.0) {
                1.0
            } else {
                0.0
            };
            self.on_grid += (hit - self.on_grid) / f64::from(self.steps.min(63) + 1);
            self.steps = self.steps.saturating_add(1);
            // (Off the grid often, over enough steps to have seen it: a
            // finer pointer, for good. A coarse reader's late reading, off
            // and back, is now and then.)
            self.fine |= self.steps >= 2 * BIAS_GRID_STEPS && self.on_grid <= 0.9;
        }
        self.last_pos = pos;
        self.s0 += 1.0;
        self.sy += used;
        self.i0 += 1.0;
        self.iy += used;
        if warm {
            let a = 1.0 - (-dt / (self.age + dt).max(0.05)).exp();
            self.noise += a * (r.abs() - self.noise);
        } else {
            let a = 1.0 - (-dt / 5.0).exp();
            self.noise += a * (r.abs().min(clip) - self.noise);
        }
        self.age += dt;
        self.base_age += dt;
        if self.kept_slope && self.base_age >= BIAS_FIT_S {
            // A whole fit since: its own slope again.
            self.kept_slope = false;
        }
        self.last_t = t;
        if !self.measuring() || outlier {
            return None;
        }
        self.win += (1.0 - (-dt / BIAS_WINDOW_S).exp()) * (r - self.win);
        // (A mean over what was measured, up to the last BIAS_RMS_S.)
        self.ms_age += dt;
        let a = (dt / self.ms_age.min(BIAS_RMS_S)).min(1.0);
        self.ms += a * (self.win * self.win - self.ms);
        self.established().then_some(r * rate)
    }
}

/// An open-loop rate estimate: the least-squares slope of the raw phase
/// (the error plus `g ∫ ε dt`, which removes the loop's own correction)
/// over the last [`RATE_LEN`] updates.
#[derive(Clone, Debug)]
pub struct RateEstimator {
    t: [f64; RATE_LEN],
    y: [f64; RATE_LEN],
    len: usize,
    head: usize,
    /// `∫ ε dt` so far, seconds.
    eps_integral: f64,
    /// Seconds since the first reading.
    now: f64,
}

impl Default for RateEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl RateEstimator {
    pub fn new() -> Self {
        Self {
            t: [0.0; RATE_LEN],
            y: [0.0; RATE_LEN],
            len: 0,
            head: 0,
            eps_integral: 0.0,
            now: 0.0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// One reading: the phase error `error` (frames) now, with `rate` the
    /// source frames per second.
    pub fn push(&mut self, error: f64, rate: f64) {
        if !error.is_finite() {
            return;
        }
        self.t[self.head] = self.now;
        self.y[self.head] = error + rate * self.eps_integral;
        self.head = (self.head + 1) % RATE_LEN;
        self.len = (self.len + 1).min(RATE_LEN);
    }

    /// ε was applied for `h` seconds from `(e0, e0')` to `(e1, e1')` (a
    /// cubic Hermite: its exact integral).
    pub fn advance(&mut self, h: f64, from: (f64, f64), to: (f64, f64)) {
        if h.is_finite() && h > 0.0 {
            self.eps_integral += h * (from.0 + to.0) / 2.0 + h * h * (from.1 - to.1) / 12.0;
            self.now += h;
        }
    }

    /// A skip of `frames` source frames moved the follower: the raw phase
    /// carries it.
    pub fn skipped(&mut self, frames: f64) {
        for y in &mut self.y[..self.len] {
            *y -= frames;
        }
    }

    /// The follower's clock offset relative to the master, ppm, and its
    /// standard error from the fit's residuals; `None` with fewer than 8
    /// readings.
    pub fn ppm(&self, rate: f64) -> Option<(f64, f64)> {
        let n = self.len;
        if n < 8 {
            return None;
        }
        let nf = n as f64;
        let tm = self.t[..n].iter().sum::<f64>() / nf;
        let ym = self.y[..n].iter().sum::<f64>() / nf;
        let (mut stt, mut sty) = (0.0, 0.0);
        for i in 0..n {
            let dt = self.t[i] - tm;
            stt += dt * dt;
            sty += dt * (self.y[i] - ym);
        }
        if stt <= 0.0 {
            return None;
        }
        let slope = sty / stt;
        let mut ss = 0.0;
        for i in 0..n {
            let r = self.y[i] - ym - slope * (self.t[i] - tm);
            ss += r * r;
        }
        let se = (ss / (nf - 2.0) / stt).sqrt();
        // d(raw)/dt = −g δ.
        Some((-slope / rate * 1e6, se / rate * 1e6))
    }
}

#[cfg(test)]
mod tests;
