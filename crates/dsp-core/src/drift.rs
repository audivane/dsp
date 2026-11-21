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
        self.set_gains(self.max_bandwidth(self.cfg.acquire_hz));
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
        if !error.is_finite() || !(h.is_finite() && h > 0.0) {
            return self.hold();
        }
        let m = self.cfg.poles;
        let mut u = error;
        let mut skip = 0;
        if !self.started {
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
            // error is replaced by it, a few in a row at most.
            let gate = self.frames(self.cfg.gate_s);
            if self.geared && gate > 0.0 && (u - self.x[m - 1]).abs() > gate {
                self.gated += 1;
                if self.gated <= self.cfg.gate_readings {
                    u = self.x[m - 1];
                    self.gated_total += 1;
                }
            } else {
                self.gated = 0;
            }
            let limit = self
                .frames(if self.locked {
                    self.cfg.skip_s
                } else {
                    self.cfg.acquire_skip_s
                })
                .max(self.cfg.skip_noise_ratio * self.noise);
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
        if self.locked {
            if fe > self.frames(self.cfg.unlock_error_s) {
                self.locked = false;
                self.lock_run = 0.0;
            }
        } else if fe < self.frames(self.cfg.lock_error_s) && self.bw <= self.cfg.lock_hz {
            self.lock_run += h;
            if self.lock_run >= self.cfg.lock_s {
                self.locked = true;
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
