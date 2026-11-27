//! The drift simulation:
//! a master and a follower DAC on their own clocks, read the way ALSA reads
//! them, with the follower corrected by the real loop through the core's
//! real accumulator. Offline only (it allocates).
//!
//! **Model.** System time `t` (seconds) is the reference.
//!
//! - **Device clocks.** Each device plays `rate · (1 + δ(t))` frames per
//!   second: a fixed offset, a sinusoidal wander and an exponential
//!   warm-up ([`ClockModel`]). The frame at the DAC at `t` is
//!   `rate · (t − start + ∫δ)`.
//! - **Readings.** A device wakes when it has played a period, plus a
//!   random latency. With USB packets the wakeup waits for the next packet
//!   boundary (packets on the bus clock, which has its own offset) and the
//!   pointer only moves at boundaries of its own grid (the packets, or
//!   the 125 µs microframes of high-speed USB), so a reading sees the
//!   frame at the DAC at the last one. A few readings are outliers whose
//!   delay reads long (measured on real devices). Without packets the
//!   pointer moves in steps of `granule` frames. The reading's timestamp has
//!   Gaussian jitter. Every reading is `(written, delay, timestamp)`.
//! - **The follower.** At each wakeup the follower reads itself, the
//!   detector takes the master's latest reading, the loop updates, and the
//!   next period is planned on an [`Accumulator`] with the piece from
//!   [`EpsilonTrack`] (cut into calls of at most
//!   [`crate::varispeed::PIECE_MAX`] core outputs, as the stage does). A
//!   skip moves the follower's source position by whole frames.
//! - **Truth.** The follower's real playback position comes from the
//!   accumulator's exact positions at the frame at the DAC; the master's is
//!   its own frame count. Their difference is the true phase error.
//!
//! **Measures** ([`Outcome`]): the lock time (from when the true error
//! stays within 1 frame of its steady mean), the steady-state error (peak
//! and RMS about that mean, and the mean itself: the bias of the
//! measurement, which no loop can see), and the phase modulation the error
//! puts on a tone, as sidebands in dBc ([`sidebands`]).

use super::{
    DriftLoop, EpsilonTrack, FRAME, LoopConfig, PhaseDetector, PositionMap, Reading, hermite,
};
use crate::adaptive::{Accumulator, Segment, Step, position_after};
use crate::varispeed::PIECE_MAX;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex64;
use std::f64::consts::PI;

const FRAME_F: f64 = 18_446_744_073_709_551_616.0;

/// A device clock's offset from nominal over time, as a fraction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockModel {
    /// Fixed offset, ppm.
    pub ppm: f64,
    /// Sinusoidal wander: amplitude (ppm), period (s) and phase (rad).
    pub wander_ppm: f64,
    pub wander_period_s: f64,
    pub wander_phase: f64,
    /// Warm-up: the offset moves by this much (ppm) with time constant
    /// `warmup_tau_s`.
    pub warmup_ppm: f64,
    pub warmup_tau_s: f64,
}

impl ClockModel {
    pub fn fixed(ppm: f64) -> Self {
        Self {
            ppm,
            wander_ppm: 0.0,
            wander_period_s: 600.0,
            wander_phase: 0.0,
            warmup_ppm: 0.0,
            warmup_tau_s: 300.0,
        }
    }

    /// δ at `t`.
    pub fn delta(&self, t: f64) -> f64 {
        let w = 2.0 * PI / self.wander_period_s;
        1e-6 * (self.ppm
            + self.wander_ppm * (w * t + self.wander_phase).sin()
            + self.warmup_ppm * (1.0 - (-t / self.warmup_tau_s).exp()))
    }

    /// `∫₀ᵗ δ`.
    pub fn integral(&self, t: f64) -> f64 {
        let w = 2.0 * PI / self.wander_period_s;
        let tau = self.warmup_tau_s;
        1e-6 * (self.ppm * t
            + self.wander_ppm / w * (self.wander_phase.cos() - (w * t + self.wander_phase).cos())
            + self.warmup_ppm * (t - tau * (1.0 - (-t / tau).exp())))
    }
}

/// One device and how it is read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceModel {
    pub clock: ClockModel,
    /// Nominal rate, frames per second (the source rate: no chain ratio).
    pub rate: f64,
    /// Period, frames.
    pub period: usize,
    /// Periods queued ahead of the DAC after a wakeup's write.
    pub buffer_periods: usize,
    /// USB packet length, seconds (0: no packets, the pointer moves in
    /// `granule` frames).
    pub packet_s: f64,
    /// The bus clock's offset, ppm, and the packet grid's phase (0 to 1).
    pub bus_ppm: f64,
    pub bus_phase: f64,
    /// Pointer step without packets, frames.
    pub granule: u64,
    /// With packets: the grid the pointer moves on, seconds (0: the packet
    /// grid). A high-speed USB device wakes once per 1 ms of packets but
    /// reports its position per 125 µs microframe.
    pub pointer_s: f64,
    /// A reading is an outlier with this probability: its delay reads
    /// this much too long, seconds (uniform), so the position reads late
    /// (late USB wakeups).
    pub outlier_rate: f64,
    pub outlier_s: (f64, f64),
    /// Wakeup latency, uniform, seconds.
    pub latency_s: (f64, f64),
    /// Timestamp jitter, standard deviation, seconds.
    pub timestamp_jitter_s: f64,
    /// When the device starts playing its frame 0, seconds.
    pub start_s: f64,
    /// Wakeups on the device's own period, not on the bus's packets (a
    /// timer-driven reader): the pointer grid's phase against them then
    /// sweeps with the device's clock against the bus's (a reading bias of
    /// up to a grid step that moves slowly).
    pub free_wake: bool,
}

impl DeviceModel {
    /// A high-speed USB DAC at 48 kHz, 1024-frame periods, two queued,
    /// woken per 1 ms of packets, its pointer on the 125 µs microframes,
    /// 30–150 µs wakeup latency, 5 µs timestamp jitter, and one reading in
    /// 200 an outlier of 0.5 to 10 ms (measured).
    pub fn usb(ppm: f64) -> Self {
        Self {
            clock: ClockModel::fixed(ppm),
            rate: 48_000.0,
            period: 1024,
            buffer_periods: 2,
            packet_s: 1e-3,
            bus_ppm: 20.0,
            bus_phase: 0.3,
            granule: 1,
            pointer_s: 125e-6,
            outlier_rate: 0.005,
            outlier_s: (0.5e-3, 10e-3),
            latency_s: (30e-6, 150e-6),
            timestamp_jitter_s: 5e-6,
            start_s: 0.0,
            free_wake: false,
        }
    }

    /// A card with a fine pointer (PCI, HDA): no packets, 1-frame steps,
    /// no outliers.
    pub fn pci(ppm: f64) -> Self {
        Self {
            packet_s: 0.0,
            pointer_s: 0.0,
            outlier_rate: 0.0,
            ..Self::usb(ppm)
        }
    }

    /// The simplest model: the pointer on the 1 ms packets, no outliers.
    pub fn usb_ideal(ppm: f64) -> Self {
        Self {
            pointer_s: 0.0,
            outlier_rate: 0.0,
            ..Self::usb(ppm)
        }
    }

    /// The frame at the DAC at `t` (fractional; negative before the start).
    pub fn frame_at(&self, t: f64) -> f64 {
        self.rate
            * ((t - self.start_s) + self.clock.integral(t) - self.clock.integral(self.start_s))
    }

    /// When frame `n` is at the DAC.
    pub fn time_of(&self, n: f64) -> f64 {
        let mut t = self.start_s + n / self.rate;
        for _ in 0..4 {
            t = self.start_s + n / self.rate
                - (self.clock.integral(t) - self.clock.integral(self.start_s));
        }
        t
    }

    fn bus_packet(&self) -> f64 {
        self.packet_s / (1.0 + self.bus_ppm * 1e-6)
    }

    /// The last packet boundary at or before `t`.
    fn boundary_before(&self, t: f64) -> f64 {
        let p = self.bus_packet();
        let phase = self.bus_phase * p;
        ((t - phase) / p).floor() * p + phase
    }

    /// The last pointer-grid boundary at or before `t` (the packet grid,
    /// subdivided).
    fn pointer_boundary_before(&self, t: f64) -> f64 {
        if self.pointer_s <= 0.0 {
            return self.boundary_before(t);
        }
        let p = self.pointer_s / (1.0 + self.bus_ppm * 1e-6);
        let phase = self.bus_phase * self.bus_packet();
        ((t - phase) / p).floor() * p + phase
    }

    /// The delay a reading adds as an outlier, frames (0 for most).
    pub fn outlier(&self, rng: &mut Rng) -> i64 {
        // Draw only when outliers are modelled, so models without them
        // keep their random sequences.
        if self.outlier_rate <= 0.0 || rng.unit() >= self.outlier_rate {
            return 0;
        }
        (rng.uniform(self.outlier_s.0, self.outlier_s.1) * self.rate).round() as i64
    }

    /// The wakeup after the device has played `n` frames.
    pub fn wake_after(&self, n: f64, rng: &mut Rng) -> f64 {
        let mut t = self.time_of(n);
        if self.packet_s > 0.0 && !self.free_wake {
            let b = self.boundary_before(t);
            if b < t {
                t = b + self.bus_packet();
            }
        }
        t + rng.uniform(self.latency_s.0, self.latency_s.1)
    }

    /// The pointer (the frame the device reports at the DAC) read at `t`.
    pub fn pointer_at(&self, t: f64) -> u64 {
        let f = if self.packet_s > 0.0 {
            self.frame_at(self.pointer_boundary_before(t))
        } else {
            self.frame_at(t)
        };
        let f = f.max(0.0).floor() as u64;
        f - f % self.granule.max(1)
    }
}

/// One simulated run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scenario {
    pub master: DeviceModel,
    pub follower: DeviceModel,
    pub cfg: LoopConfig,
    pub duration_s: f64,
    /// The follower starts this many source frames behind the master.
    pub initial_offset: f64,
    /// The steady-state window starts here, seconds.
    pub steady_from_s: f64,
    /// The tone the sidebands are measured on, Hz.
    pub tone_hz: f64,
    pub seed: u64,
    /// An xrun on the follower: at this time (s from its start) its
    /// playback jumps by this many source frames, unannounced.
    pub xrun: Option<(f64, i64)>,
}

impl Scenario {
    /// Two USB DACs at 48 kHz; the follower +60 ppm, warming up by 3 ppm
    /// (time constant 10 minutes) with 0.5 ppm of wander over 20 minutes;
    /// starting 40 frames behind.
    pub fn typical() -> Self {
        let mut follower = DeviceModel::usb(60.0);
        follower.clock.wander_ppm = 0.5;
        follower.clock.wander_period_s = 1200.0;
        follower.clock.warmup_ppm = 3.0;
        follower.clock.warmup_tau_s = 600.0;
        follower.bus_phase = 0.71;
        follower.start_s = 0.0137;
        Self {
            master: DeviceModel::usb(0.0),
            follower,
            cfg: LoopConfig::default(),
            duration_s: 600.0,
            initial_offset: 40.0,
            steady_from_s: 120.0,
            tone_hz: 10_000.0,
            seed: 1,
            xrun: None,
        }
    }
}

/// The simulation's scenarios, by name.
pub fn scenarios() -> Vec<(&'static str, Scenario)> {
    let base = Scenario::typical();
    let mut v = vec![("usb pair, +60 ppm", base)];
    let mut sc = base;
    sc.follower.clock.wander_ppm = 2.0;
    sc.follower.clock.wander_period_s = 600.0;
    sc.follower.clock.warmup_tau_s = 300.0;
    v.push(("fast wander", sc));
    let mut sc = base;
    sc.follower.clock.ppm = -100.0;
    sc.initial_offset = 300.0;
    v.push(("cheap, -100 ppm, 300 behind", sc));
    let mut sc = base;
    sc.master.latency_s = (0.0, 1e-3);
    sc.follower.latency_s = (0.0, 1e-3);
    v.push(("worst quantisation", sc));
    let mut sc = base;
    sc.timestamp_jitter(50e-6);
    v.push(("50 us timestamp jitter", sc));
    let mut sc = base;
    for d in [&mut sc.master, &mut sc.follower] {
        d.rate = 44_100.0;
        d.period = 8192;
    }
    v.push(("44.1k, period 8192", sc));
    let mut sc = base;
    for d in [&mut sc.master, &mut sc.follower] {
        d.period = 256;
        d.buffer_periods = 3;
    }
    v.push(("period 256", sc));
    let mut sc = base;
    sc.master.period = 512;
    sc.follower.period = 1024;
    sc.master.rate = 48_000.0;
    v.push(("master period 512", sc));
    let mut sc = base;
    sc.master = DeviceModel::pci(-15.0);
    v.push(("pci master", sc));
    let mut sc = base;
    sc.follower.clock = ClockModel::fixed(20.0);
    v.push(("no wander, +20 ppm", sc));
    let mut sc = base;
    sc.xrun = Some((150.0, 1024));
    sc.steady_from_s = 240.0;
    v.push(("xrun at 150 s, 1024 frames", sc));
    let mut sc = base;
    sc.master.clock.wander_ppm = 1.5;
    sc.master.clock.wander_period_s = 420.0;
    sc.master.clock.warmup_ppm = -2.0;
    v.push(("both wander", sc));
    v
}

impl Scenario {
    /// Set both devices' timestamp jitter.
    pub fn timestamp_jitter(&mut self, s: f64) {
        self.master.timestamp_jitter_s = s;
        self.follower.timestamp_jitter_s = s;
    }
}

/// What a run measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome {
    /// From when the true error stays within 1 frame of its steady mean,
    /// seconds from the follower's start (`INFINITY`: never).
    pub lock_s: f64,
    /// When the loop first said it was locked, seconds from the follower's
    /// start (`INFINITY`: never).
    pub lock_flag_s: f64,
    /// Mean true error over the steady window, frames.
    pub bias: f64,
    /// Peak and RMS of the true error about that mean, frames.
    pub steady_peak: f64,
    pub steady_rms: f64,
    /// Worst sideband 1 to 100 Hz from the tone, dBc, and where.
    pub sideband_dbc: f64,
    pub sideband_hz: f64,
    /// The same, 0.1 to 1 Hz (reported only: no target).
    pub sideband_low_dbc: f64,
    /// The worst sideband 1 to 100 Hz over the first analysis segment
    /// after the loop said it was locked (the follower fades in there,
    /// while the gear is still narrowing).
    pub sideband_at_lock_dbc: f64,
    /// When the gear reached the tracking bandwidth, seconds from the
    /// follower's start.
    pub gear_end_s: f64,
    /// When the loop last turned locked, seconds from the follower's start
    /// (after an xrun: when it came back).
    pub relock_flag_s: f64,
    /// ε at the end against the true value, ppm.
    pub eps_error_ppm: f64,
    /// Skips made, and the largest |ε| applied after lock.
    pub skips: usize,
    pub loop_unlocks: usize,
}

/// A small deterministic generator (SplitMix64).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    pub fn gauss(&mut self) -> f64 {
        let u1 = self.unit().max(1e-300);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
    }
}

/// One planned core call of the follower: its first device frame, its
/// length in device frames, the accumulator's absolute position and
/// trajectory, and the source offset (skips) it plays with.
#[derive(Clone, Copy, Debug)]
struct Call {
    frame: u64,
    frames: u64,
    pos: u128,
    step: u128,
    d: [i128; 3],
    offset: i128,
}

impl Call {
    /// Source position (frames) of device frame `frame + m` (fractional
    /// `m`), relative to `base` (64.64 source frames).
    fn source(&self, m: f64, base: i128) -> f64 {
        let j = 2.0 * m;
        let k = (j.floor() as usize).min(2 * self.frames as usize);
        let at = |k: usize| {
            let p = position_after(self.pos, self.step, self.d, k).expect("position");
            // Core positions are on the 2G grid.
            (p / 2) as i128 + self.offset - base
        };
        let p0 = at(k) as f64 / FRAME_F;
        let p1 = at(k + 1) as f64 / FRAME_F;
        p0 + (p1 - p0) * (j - k as f64)
    }
}

/// What a run recorded, for plots and diagnosis.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    /// The true error at 1 kHz from the follower's start, frames.
    pub error: Vec<f64>,
    /// Per follower update: time (s from the follower's start), measured
    /// error (frames), ε, bandwidth (Hz).
    pub updates: Vec<[f64; 4]>,
}

/// Run a scenario.
pub fn run(sc: &Scenario) -> Outcome {
    run_traced(sc).0
}

/// Run a scenario and keep its trace.
pub fn run_traced(sc: &Scenario) -> (Outcome, Trace) {
    let mut updates = Vec::new();
    let mut rng = Rng::new(sc.seed);
    let (m, f) = (sc.master, sc.follower);
    let src_rate = m.rate;

    // The master: frame n carries source frame n.
    let mut map_m = PositionMap::new();
    let mut det = PhaseDetector::new(src_rate);
    // The follower.
    let mut map_f = PositionMap::new();
    let delay_s = (f.buffer_periods as f64 + 0.5) * f.period as f64 / f.rate;
    let mut lp = DriftLoop::new(sc.cfg, src_rate, delay_s).expect("loop config");
    let core_rate = 2.0 * f.rate;
    let mut track = EpsilonTrack::new(core_rate);
    let mut acc = Accumulator::new(Step::ONE);
    let mut calls: Vec<Call> = Vec::new();

    // Positions are kept relative to `base`, the master's source frame at
    // the follower's start, so f64 differences stay small.
    let base_f = m.frame_at(f.start_s);
    let base = (base_f.floor() as i128) * FRAME;
    let start_frac = base_f - base_f.floor();
    // The follower's first frame carries the master's frame at its start,
    // less the initial offset.
    let mut offset: i128 = base + ((start_frac - sc.initial_offset) * FRAME_F).round() as i128;

    let period_f = f.period as u64;
    let period_s = f.period as f64 / f.rate;
    let mut written_f: u64 = 0;
    // Plan one period with ε going to `to`: cut into calls of at most
    // PIECE_MAX core outputs.
    let plan_period = |written: u64,
                       to: (f64, f64),
                       track: &mut EpsilonTrack,
                       acc: &mut Accumulator,
                       calls: &mut Vec<Call>,
                       offset: i128,
                       map: &mut PositionMap| {
        let from = track.state();
        let outputs = 2 * f.period;
        let parts = outputs.div_ceil(PIECE_MAX);
        let mut done = 0usize;
        for part in 0..parts {
            let next = outputs * (part + 1) / parts;
            let n = next - done;
            let t_end = period_s * next as f64 / outputs as f64;
            let end = hermite(from, to, period_s, t_end);
            let seg = track
                .piece(Segment::Hold, Step::ONE, end)
                .expect("step in range");
            let pos = acc.position();
            let need = acc.plan(n, seg).expect("plan");
            let pl = acc.planned().expect("planned");
            calls.push(Call {
                frame: written + done as u64 / 2,
                frames: n as u64 / 2,
                pos,
                step: pl.step,
                d: pl.d,
                offset,
            });
            acc.commit(need);
            done = next;
        }
        let c = calls[calls.len() - parts];
        map.push(written, (c.pos / 2) as i128 + c.offset);
        let c = calls[calls.len() - 1];
        let k = 2 * (c.frames as usize - 1);
        let p = position_after(c.pos, c.step, c.d, k).expect("position");
        map.push(c.frame + c.frames - 1, (p / 2) as i128 + c.offset);
    };

    // Prefill: the follower's buffer at its start.
    for _ in 0..f.buffer_periods {
        plan_period(
            written_f,
            (0.0, 0.0),
            &mut track,
            &mut acc,
            &mut calls,
            offset,
            &mut map_f,
        );
        written_f += period_f;
    }
    // The master's buffer.
    let period_m = m.period as u64;
    let mut written_m: u64 = 0;
    let push_master = |map: &mut PositionMap, w: u64| {
        map.push(w, i128::from(w) * FRAME);
        map.push(w + period_m - 1, i128::from(w + period_m - 1) * FRAME);
    };
    for _ in 0..m.buffer_periods {
        push_master(&mut map_m, written_m);
        written_m += period_m;
    }

    let mut k_m: u64 = 1;
    let mut k_f: u64 = 1;
    let mut t_m = m.wake_after((k_m * period_m) as f64, &mut rng);
    let mut t_f = f.wake_after((k_f * period_f) as f64, &mut rng);
    let mut skips = 0usize;
    let mut unlocks = 0usize;
    let mut was_locked = false;
    let mut lock_flag_s = f64::INFINITY;
    let mut gear_end_s = f64::INFINITY;
    let mut relock_flag_s = f64::INFINITY;
    let mut xrun = sc.xrun;
    let end = f.start_s + sc.duration_s;
    while t_f < end {
        if t_m <= t_f {
            let ptr = m.pointer_at(t_m);
            let r = Reading {
                written: written_m,
                delay: written_m as i64 - ptr as i64 + m.outlier(&mut rng),
                time_ns: ((t_m + m.timestamp_jitter_s * rng.gauss()) * 1e9).round() as i64,
            };
            det.master(r, &map_m);
            push_master(&mut map_m, written_m);
            written_m += period_m;
            k_m += 1;
            t_m = m.wake_after((k_m * period_m) as f64, &mut rng).max(t_m);
            continue;
        }
        if let Some((at, jump)) = xrun
            && t_f - f.start_s >= at
        {
            // The device lost (or repeated) frames: what it plays from
            // now on is `jump` source frames further on, and the loop is
            // not told.
            xrun = None;
            offset += i128::from(jump) * FRAME;
            for c in calls.iter_mut().rev().take(f.buffer_periods * 8) {
                c.offset += i128::from(jump) * FRAME;
            }
            map_f.shift(i128::from(jump) * FRAME);
        }
        let ptr = f.pointer_at(t_f);
        let r = Reading {
            written: written_f,
            delay: written_f as i64 - ptr as i64 + f.outlier(&mut rng),
            time_ns: ((t_f + f.timestamp_jitter_s * rng.gauss()) * 1e9).round() as i64,
        };
        // Positions in the maps are absolute; the detector's error is a
        // difference, so the base cancels.
        let e = det.follower(r, &map_f).unwrap_or(f64::NAN);
        let c = lp.update(e, period_s);
        updates.push([t_f - f.start_s, e, c.eps, lp.bandwidth()]);
        if lp.bandwidth() <= sc.cfg.track_hz && gear_end_s.is_infinite() {
            gear_end_s = t_f - f.start_s;
        }
        if c.skip != 0 {
            // What is queued keeps its positions; the next period starts
            // `skip` frames further on in the source.
            skips += 1;
            offset += i128::from(c.skip) * FRAME;
        }
        if c.locked && !was_locked {
            lock_flag_s = lock_flag_s.min(t_f - f.start_s);
            relock_flag_s = t_f - f.start_s;
        }
        if !c.locked && was_locked {
            unlocks += 1;
        }
        was_locked = c.locked;
        plan_period(
            written_f,
            (c.eps, c.eps_slope),
            &mut track,
            &mut acc,
            &mut calls,
            offset,
            &mut map_f,
        );
        written_f += period_f;
        k_f += 1;
        t_f = f.wake_after((k_f * period_f) as f64, &mut rng).max(t_f);
    }

    // Truth, sampled at 1 kHz.
    let fs = 1000.0;
    let t0 = f.start_s;
    let n = ((sc.duration_s - 1.0) * fs) as usize;
    let mut err = Vec::with_capacity(n);
    let mut ci = 0usize;
    for i in 0..n {
        let t = t0 + i as f64 / fs;
        let nf = f.frame_at(t);
        let nm = m.frame_at(t) - base_f.floor();
        while ci + 1 < calls.len() && (calls[ci + 1].frame as f64) <= nf {
            ci += 1;
        }
        let c = calls[ci];
        let pf = c.source(nf - c.frame as f64, base);
        err.push(nm - pf);
    }
    let steady0 = ((sc.steady_from_s * fs) as usize).min(n);
    let steady = &err[steady0..];
    let bias = steady.iter().sum::<f64>() / steady.len().max(1) as f64;
    let mut peak: f64 = 0.0;
    let mut ss = 0.0;
    for e in steady {
        peak = peak.max((e - bias).abs());
        ss += (e - bias).powi(2);
    }
    let rms = (ss / steady.len().max(1) as f64).sqrt();
    let lock_s = err
        .iter()
        .rposition(|e| (e - bias).abs() >= 1.0)
        .map_or(0.0, |i| (i + 1) as f64 / fs);
    let lock_s = if lock_s >= sc.duration_s - 2.0 {
        f64::INFINITY
    } else {
        lock_s
    };
    let band = sidebands(steady, fs, sc.tone_hz, src_rate, (1.0, 100.0));
    let low = sidebands(steady, fs, sc.tone_hz, src_rate, (0.1, 1.0));
    let at_lock = if lock_flag_s.is_finite() {
        let i = ((lock_flag_s * fs) as usize).min(n);
        let j = (i + SEGMENT + 2).min(n);
        sidebands(&err[i..j], fs, sc.tone_hz, src_rate, (1.0, 100.0)).0
    } else {
        f64::NAN
    };
    let t_end = end;
    let eps_true = (1.0 + m.clock.delta(t_end)) / (1.0 + f.clock.delta(t_end)) - 1.0;
    let out = Outcome {
        lock_s,
        lock_flag_s,
        bias,
        steady_peak: peak,
        steady_rms: rms,
        sideband_dbc: band.0,
        sideband_hz: band.1,
        sideband_low_dbc: low.0,
        sideband_at_lock_dbc: at_lock,
        gear_end_s,
        relock_flag_s,
        eps_error_ppm: (lp.eps() - eps_true) * 1e6,
        skips,
        loop_unlocks: unlocks,
    };
    (
        out,
        Trace {
            error: err,
            updates,
        },
    )
}

/// Analysis segment length, samples (16.4 s at 1 kHz: 0.061 Hz bins).
pub const SEGMENT: usize = 1 << 14;

/// The worst sideband a phase error puts on a tone, within `band` (Hz):
/// `error` is the timing error in source frames, sampled at `fs`; the tone
/// is `tone_hz` at `src_rate`. The error is pre-whitened by a second
/// difference (so the tracked wander, frames at millihertz, cannot leak
/// into the band through the window), then each segment of [`SEGMENT`]
/// samples (half overlapping) gets a 4-term Blackman–Harris window, and
/// each bin is divided by the difference's gain `(2 sin(π f / fs))²`. A
/// sideband is half the phase deviation's amplitude in a bin,
/// `20 log10(β / 2)`, so a pure sinusoidal modulation of index β reads as
/// itself. Returns the worst level over every segment and bin, dBc, and
/// its frequency.
pub fn sidebands(
    error: &[f64],
    fs: f64,
    tone_hz: f64,
    src_rate: f64,
    band: (f64, f64),
) -> (f64, f64) {
    let n = SEGMENT;
    if error.len() < n + 2 {
        return (f64::NAN, f64::NAN);
    }
    let diff: Vec<f64> = error.windows(3).map(|w| w[2] - 2.0 * w[1] + w[0]).collect();
    let window: Vec<f64> = (0..n)
        .map(|i| {
            let x = 2.0 * PI * i as f64 / n as f64;
            0.35875 - 0.48829 * x.cos() + 0.14128 * (2.0 * x).cos() - 0.01168 * (3.0 * x).cos()
        })
        .collect();
    let wsum: f64 = window.iter().sum();
    let fft = FftPlanner::new().plan_fft_forward(n);
    let mut buf = vec![Complex64::new(0.0, 0.0); n];
    let scale = 2.0 * PI * tone_hz / src_rate;
    let lo = ((band.0 * n as f64 / fs).ceil() as usize).max(1);
    let hi = ((band.1 * n as f64 / fs).floor() as usize).min(n / 2);
    let (mut worst, mut worst_hz) = (f64::NEG_INFINITY, f64::NAN);
    let mut start = 0;
    while start + n <= diff.len() {
        let seg = &diff[start..start + n];
        let mean = seg.iter().sum::<f64>() / n as f64;
        for (b, (x, w)) in buf.iter_mut().zip(seg.iter().zip(&window)) {
            *b = Complex64::new((x - mean) * w * scale, 0.0);
        }
        fft.process(&mut buf);
        for (k, b) in buf.iter().enumerate().take(hi + 1).skip(lo) {
            let g = (2.0 * (PI * k as f64 / n as f64).sin()).powi(2);
            let beta = 2.0 * b.norm() / wsum / g;
            let db = 20.0 * (beta / 2.0).max(1e-300).log10();
            if db > worst {
                worst = db;
                worst_hz = k as f64 * fs / n as f64;
            }
        }
        start += n / 2;
    }
    (worst, worst_hz)
}
