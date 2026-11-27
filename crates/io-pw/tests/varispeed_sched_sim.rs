//! Test 12 of the adaptive resampler plan: the varispeed scheduler against
//! simulated graph timing.
//!
//! An event-driven simulation of one driver: DAC wakeups every period (with
//! jitter), graph cycles that complete after a random latency, the driver
//! noticing completions late, timers firing late, a speed envelope (raised
//! cosine in log speed, cubic Hermite retargets) and a stage that pulls
//! exactly what its trajectory needs plus at most one frame. It checks:
//!
//! - no underfeed for every period and quantum, at every speed the cap
//!   allows, while cycles complete within the cycle bound;
//! - underfeed counted, never hidden, when they do not;
//! - the ring fill stays within the derived capacity;
//! - xruns, reopens, drains, short cycles and starved sources.
//!
//! Run with `--nocapture` for the measured figures.

use audivane_io_pw::varispeed_sched::{
    MAX_SPEED, MIN_SPEED, PullKind, Timing, VarispeedScheduler, pull_bound,
};

// ---------------------------------------------------------------- helpers

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform in `0..=max`.
    fn upto(&mut self, max: u64) -> u64 {
        if max == 0 { 0 } else { self.next() % (max + 1) }
    }
}

/// Speed as a function of output time (nominal frames): a raised cosine in
/// log speed from rest, a cubic Hermite from the current speed and slope on
/// a retarget, the duration at least `200 ms · sqrt(|Δ ln s| / ln 4)` and
/// one period. Clamped to the cap, as the stage's planner must.
#[derive(Clone, Copy)]
struct Envelope {
    rate: f64,
    cap: f64,
    t0: f64,
    dur: f64,
    l0: f64,
    m0: f64,
    l1: f64,
}

impl Envelope {
    fn constant(rate: u32, speed: f64, cap: f64) -> Self {
        let l = speed.ln();
        Self {
            rate: f64::from(rate),
            cap,
            t0: 0.0,
            dur: 0.0,
            l0: l,
            m0: 0.0,
            l1: l,
        }
    }

    fn u(&self, t: f64) -> f64 {
        if self.dur <= 0.0 {
            1.0
        } else {
            ((t - self.t0) / self.dur).clamp(0.0, 1.0)
        }
    }

    fn log_at(&self, t: f64) -> f64 {
        let u = self.u(t);
        if u >= 1.0 {
            return self.l1;
        }
        if self.m0 == 0.0 {
            self.l0 + (self.l1 - self.l0) * (1.0 - (std::f64::consts::PI * u).cos()) / 2.0
        } else {
            let (u2, u3) = (u * u, u * u * u);
            let h00 = 2.0 * u3 - 3.0 * u2 + 1.0;
            let h10 = u3 - 2.0 * u2 + u;
            let h01 = -2.0 * u3 + 3.0 * u2;
            h00 * self.l0 + h10 * self.dur * self.m0 + h01 * self.l1
        }
    }

    fn slope_at(&self, t: f64) -> f64 {
        let h = 0.5;
        (self.log_at(t + h) - self.log_at(t - h)) / (2.0 * h)
    }

    fn speed_at(&self, t: f64) -> f64 {
        self.log_at(t).exp().clamp(MIN_SPEED, self.cap)
    }

    fn target(&self) -> f64 {
        self.l1.exp()
    }

    fn command(&mut self, t: f64, target: f64, ramp_frames: f64, period: f64) {
        let l = self.log_at(t);
        let m = if self.u(t) < 1.0 {
            self.slope_at(t)
        } else {
            0.0
        };
        let l1 = target.ln();
        let t_min = (0.2 * self.rate * ((l1 - l).abs() / 4f64.ln()).sqrt()).max(period);
        *self = Self {
            t0: t,
            dur: ramp_frames.max(t_min),
            l0: l,
            m0: m,
            l1,
            ..*self
        };
    }

    /// Highest speed over `[t, t + n]`.
    fn max_over(&self, t: f64, n: f64) -> f64 {
        (0..=32)
            .map(|i| self.speed_at(t + n * f64::from(i) / 32.0))
            .fold(0.0, f64::max)
    }

    /// Source frames consumed over `[t, t + n]` (Simpson).
    fn integral(&self, t: f64, n: f64) -> f64 {
        let k = 16;
        let h = n / f64::from(k);
        let mut acc = self.speed_at(t) + self.speed_at(t + n);
        for i in 1..k {
            let w = if i % 2 == 1 { 4.0 } else { 2.0 };
            acc += w * self.speed_at(t + h * f64::from(i));
        }
        acc * h / 3.0
    }
}

/// The stage's pull contract, modelled: it pulls through the newest frame
/// its last output needs, plus at most one frame of over-pull.
struct Stage {
    t_out: f64,
    x: f64,
    pulled: u64,
}

impl Stage {
    fn new() -> Self {
        Self {
            t_out: 0.0,
            x: 0.0,
            pulled: 0,
        }
    }

    fn plan(&mut self, env: &Envelope, n: u32, rng: &mut Rng) -> usize {
        if n == 0 {
            return 0;
        }
        let dx = env.integral(self.t_out, f64::from(n));
        self.x += dx;
        let need = self.x.floor() as u64 + 1 + rng.upto(1);
        let n_src = need.saturating_sub(self.pulled);
        self.pulled = self.pulled.max(need);
        self.t_out += f64::from(n);
        n_src as usize
    }
}

#[derive(Clone, Copy)]
struct Jitter {
    /// Trigger to completion (frames land in the ring).
    complete_ns: u64,
    /// Completion to the driver seeing it.
    observe_ns: u64,
    /// Timer wakeups late by up to this.
    timer_ns: u64,
    /// DAC wakeups late by up to this (not cumulative).
    dac_ns: u64,
    /// Every latency at its maximum, and DAC wakeups alternately late and
    /// on time (the shortest DAC intervals), instead of uniform draws.
    worst: bool,
}

impl Jitter {
    /// At the cycle bound: completion plus observation reach `σ_min`.
    fn at_bound(t: &Timing) -> Self {
        let b = t.cycle_bound_ns;
        Self {
            complete_ns: b - b / 10,
            observe_ns: b / 10,
            timer_ns: 100_000,
            dac_ns: 500_000,
            worst: false,
        }
    }

    fn worst(self) -> Self {
        Self {
            worst: true,
            ..self
        }
    }

    fn draw(&self, rng: &mut Rng, max: u64) -> u64 {
        if self.worst { max } else { rng.upto(max) }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Source {
    /// A pulled player: every cycle delivers a quantum.
    Pulled,
}

#[derive(Clone)]
struct Scenario {
    timing: Timing,
    jitter: Jitter,
    seed: u64,
    seconds: f64,
    min_periods: u64,
    max_periods: u64,
    /// The RT thread is busy (processing, writing) for this share of a
    /// period after each DAC wakeup, and polls nothing meanwhile.
    busy_frac: f64,
    /// Random speed commands (up to `MAX_SPEED`, clamped by the cap).
    commands: bool,
    /// Initial command.
    start_speed: f64,
    /// Probability that a cycle delivers fewer than Q frames.
    short_prob: f64,
    /// Nominal frames vary between `period - 1` and `period`.
    vary_period: bool,
    /// An xrun at period `.0`: the driver recovers for `.1` ns, flushes the
    /// ring, resets the scheduler and primes again, as the driver's restart
    /// path does. A cycle in flight lands after the flush.
    xrun: Option<(u64, u64)>,
    /// Reopen into `.1` at period `.0` (same restart path, new timing).
    reopen: Option<(u64, Timing)>,
    /// The quantum changes to `.1` at period `.0`, without a reopen.
    requantum: Option<(u64, u32)>,
    /// The source ends at period `.0`; later pulls drain.
    drain_at: Option<u64>,
    source: Source,
    /// A command at this time (ns) after the run starts.
    late_command: Option<(u64, f64)>,
}

impl Scenario {
    fn new(timing: Timing, seed: u64) -> Self {
        Self {
            timing,
            jitter: Jitter::at_bound(&timing),
            seed,
            seconds: 1.0,
            min_periods: 40,
            max_periods: 20_000,
            busy_frac: 0.0,
            commands: true,
            start_speed: 1.0,
            short_prob: 0.0,
            vary_period: false,
            xrun: None,
            reopen: None,
            requantum: None,
            drain_at: None,
            source: Source::Pulled,
            late_command: None,
        }
    }

    /// The RT thread busy for `frac` of each period, and the scheduler's
    /// processing budget set to match.
    fn loaded(mut self, frac: f64) -> Self {
        self.busy_frac = frac;
        self.timing = with_load(self.timing, frac);
        self
    }
}

/// `t` with a processing budget of `frac` of its period.
fn with_load(t: Timing, frac: f64) -> Timing {
    t.with_proc_budget((t.period_ns() as f64 * frac) as u64)
}

#[derive(Debug, Default)]
struct Outcome {
    periods: u64,
    /// Pulls that found fewer frames than planned (the simulation's count).
    underfeed_events: u64,
    underfeed_frames: u64,
    drain_frames: u64,
    /// Smallest `fill - n_src` at a non-drain pull, frames.
    min_margin: i64,
    max_fill: usize,
    capacity: usize,
    /// Most cycles triggered between two DAC wakeups.
    max_cycles_per_period: u64,
    /// Smallest spacing between two triggers outside priming, ns.
    min_spacing_ns: u64,
    max_speed_seen: f64,
    held_seen: bool,
    pulls_over_bound: u64,
    stalled: u64,
    short_cycles: u64,
    restarts: u64,
    /// Cycles that landed after a flush (in flight across a reset).
    landed_after_flush: u64,
}

/// Runs a scenario. Panics if the scheduler's counters disagree with what
/// the simulation saw (underfeed must never be hidden), if two cycles
/// overlap, if the ring overflows or if a pull exceeds its bound.
fn run(sc: &Scenario) -> Outcome {
    let mut rng = Rng(sc.seed);
    let mut timing = sc.timing;
    let mut sched = VarispeedScheduler::new(timing);
    let target = sched.command(sc.start_speed);
    sched.reset(timing);
    let mut env = Envelope::constant(timing.graph_rate, target, sched.cap().max_speed);
    let mut stage = Stage::new();
    let mut cap_limit = timing.ring_capacity();
    let mut out = Outcome {
        min_margin: i64::MAX,
        min_spacing_ns: u64::MAX,
        capacity: cap_limit,
        ..Outcome::default()
    };

    let mut now: u64 = 0;
    let mut fill: usize = 0;
    let mut completed: u64 = 0;
    let mut land_at: Option<u64> = None;
    let mut land_frames = 0usize;
    let mut land_flushed = false;
    let mut observe_at: Option<u64> = None;
    let mut timer_at: Option<u64> = Some(0);
    // DAC: `None` while priming; else (base ns, next index).
    let mut dac: Option<(u64, u64)> = None;
    let mut dac_next: Option<u64> = None;
    // The RT thread is busy until then; its wakeups wait.
    let mut busy_until = 0u64;
    let mut busy_end: Option<u64> = None;
    // Recovering (xrun or reopen) until `.0`, then restarting into `.1`.
    let mut restart: Option<(u64, Timing)> = None;
    let mut bound = pull_bound(target, timing.period);
    let mut cycles_this_period = 0u64;
    let mut last_trigger: Option<u64> = None;
    let mut draining = false;
    let mut late_command = sc.late_command;
    let mut next_command_ns = 0u64;

    let period_ns = timing.period_ns();
    let periods = ((sc.seconds * 1e9 / period_ns as f64) as u64)
        .max(sc.min_periods)
        .min(sc.max_periods);

    loop {
        // Next event. Driver wakeups (completion eventfd, timer, DAC) wait
        // while the RT thread is busy; landings do not.
        let held = |t: Option<u64>| t.map(|t| t.max(busy_until));
        let next = [
            land_at,
            held(observe_at),
            held(timer_at),
            held(dac_next),
            busy_end,
            restart.map(|r| r.0),
        ]
        .into_iter()
        .flatten()
        .min()
        .expect("the simulation always has a next event");
        now = now.max(next);
        if land_at.is_some_and(|t| t <= now) {
            fill += land_frames;
            completed += 1;
            land_at = None;
            if land_flushed {
                out.landed_after_flush += 1;
                land_flushed = false;
            }
            out.max_fill = out.max_fill.max(fill);
            assert!(
                fill <= cap_limit,
                "ring overflow: {fill} > {cap_limit} ({timing:?})"
            );
        }
        let due = |t: Option<u64>| t.is_some_and(|t| t.max(busy_until) <= now);
        let mut woke = false;
        if due(observe_at) {
            observe_at = None;
            woke = true;
        }
        if due(timer_at) {
            timer_at = None;
            woke = true;
        }
        if busy_end.is_some_and(|t| t <= now) {
            busy_end = None;
            woke = true;
        }
        if let Some((at, new_timing)) = restart {
            if at > now {
                // Recovering: the driver is in its restart path.
                continue;
            }
            // The restart path: flush, reset, prime again.
            restart = None;
            out.restarts += 1;
            timing = new_timing;
            fill = 0;
            if land_at.is_some() {
                land_flushed = true;
            }
            sched.reset(timing);
            dac = None;
            dac_next = None;
            env = Envelope::constant(timing.graph_rate, sched.target(), sched.cap().max_speed);
            stage = Stage::new();
            bound = pull_bound(sched.target(), timing.period);
            cap_limit = cap_limit.max(timing.ring_capacity());
            out.capacity = cap_limit;
            last_trigger = None;
            woke = true;
        }

        let mut dac_fired = false;
        if due(dac_next) {
            woke = true;
            dac_fired = true;
            dac_next = None;
            let (base, k) = dac.expect("running");
            out.periods += 1;
            out.max_cycles_per_period = out.max_cycles_per_period.max(cycles_this_period);
            cycles_this_period = 0;

            if sc.drain_at.is_some_and(|d| out.periods >= d) {
                draining = true;
            }
            let n = if sc.vary_period && k % 3 == 1 {
                timing.period - 1
            } else {
                timing.period
            };
            let n_src = stage.plan(&env, n, &mut rng);
            if n_src > bound {
                out.pulls_over_bound += 1;
            }
            let kind = if draining {
                PullKind::Drain
            } else {
                PullKind::Play
            };
            if kind == PullKind::Play {
                out.min_margin = out.min_margin.min(fill as i64 - n_src as i64);
            }
            let pull = sched.on_pull(now, fill, n_src, kind);
            assert_eq!(pull.take + pull.pad, n_src);
            assert_eq!(pull.take, n_src.min(fill));
            if pull.pad > 0 {
                if draining {
                    out.drain_frames += pull.pad as u64;
                } else {
                    out.underfeed_events += 1;
                    out.underfeed_frames += pull.pad as u64;
                }
            }
            fill -= pull.take;

            // A quantum change without a reopen.
            if let Some((at, q)) = sc.requantum
                && out.periods == at
            {
                timing = timing.with_quantum(q);
                sched.update_timing(timing);
                cap_limit = cap_limit.max(timing.ring_capacity());
                out.capacity = cap_limit;
            }
            // Commands, holds and caps reach the stage for the next period.
            if sc.commands && now >= next_command_ns {
                let span = (periods as f64 * period_ns as f64 / 6.0) as u64;
                next_command_ns = now + span / 2 + rng.upto(span);
                let s = MIN_SPEED * (MAX_SPEED / MIN_SPEED).powf(rng.unit());
                let s = if rng.upto(3) == 0 { MAX_SPEED } else { s };
                sched.command(s);
                let ramp = rng.unit() * 0.4 * f64::from(timing.graph_rate);
                env.command(stage.t_out, sched.target(), ramp, f64::from(timing.period));
            }
            if let Some((at, s)) = late_command
                && now >= at
            {
                sched.command(s);
                late_command = None;
            }
            if sched.held().is_some() {
                out.held_seen = true;
            }
            env.cap = sched.cap().max_speed;
            if (env.target() - sched.target()).abs() > 1e-12 {
                env.command(stage.t_out, sched.target(), 0.0, f64::from(timing.period));
            }
            let s_next = env.max_over(stage.t_out, f64::from(timing.period));
            out.max_speed_seen = out.max_speed_seen.max(s_next);
            bound = pull_bound(s_next, timing.period);
            sched.set_next_bound(bound);

            // The next wakeup, or the restart path.
            let k = k + 1;
            if let Some((at, gap)) = sc.xrun
                && out.periods == at
            {
                restart = Some((now + gap, timing));
                dac = None;
            } else if let Some((at, to)) = sc.reopen
                && out.periods == at
            {
                restart = Some((now + 5_000_000, to));
                dac = None;
            } else if out.periods >= periods {
                break;
            } else {
                dac = Some((base, k));
                let nominal = base + timing.frames_ns(k * u64::from(timing.period));
                let late = if sc.jitter.worst {
                    if k % 2 == 1 { sc.jitter.dac_ns } else { 0 }
                } else {
                    rng.upto(sc.jitter.dac_ns)
                };
                dac_next = Some(nominal + late);
            }
        }

        // Recovering: no polls, except right after the xrun wakeup's pull
        // (the driver triggers before it processes and hits the xrun).
        if !woke || (restart.is_some() && !dac_fired) {
            continue;
        }
        // The driver polls on every wakeup (after a DAC pull: before
        // processing, then once processing ends).
        let p = sched.poll(now, fill, completed);
        if p.trigger {
            assert!(land_at.is_none(), "overlapping cycles ({timing:?})");
            if dac.is_some()
                && let Some(prev) = last_trigger
            {
                out.min_spacing_ns = out.min_spacing_ns.min(now - prev);
            }
            last_trigger = dac.map(|_| now);
            cycles_this_period += 1;
            let q = timing.quantum as usize;
            land_frames = match sc.source {
                _ if draining => 0,
                _ if sc.short_prob > 0.0 && rng.unit() < sc.short_prob => {
                    out.short_cycles += 1;
                    rng.upto(q as u64 - 1) as usize
                }
                _ => q,
            };
            let t_land = now + sc.jitter.draw(&mut rng, sc.jitter.complete_ns);
            land_at = Some(t_land);
            observe_at = Some(t_land + sc.jitter.draw(&mut rng, sc.jitter.observe_ns));
        }
        let timer_late = sc.jitter.draw(&mut rng, sc.jitter.timer_ns);
        timer_at = p.wake_at_ns.map(|t| t + timer_late);
        // The observe wakeup is the driver's eventfd; it supersedes the
        // stall timer while a cycle is in flight.
        if observe_at.is_some() && timer_at.is_some_and(|t| t > observe_at.unwrap()) {
            timer_at = None;
        }
        if dac_fired && sc.busy_frac > 0.0 {
            busy_until = now + (period_ns as f64 * sc.busy_frac) as u64;
            busy_end = Some(busy_until);
        }
        if dac.is_none() && restart.is_none() && land_at.is_none() && sched.is_primed(fill) {
            // Primed: the DAC starts (`alsa.start()`); its first wakeup
            // pulls now, later ones follow the period grid.
            sched.finish_priming();
            dac = Some((now, 0));
            dac_next = Some(now);
            cycles_this_period = 0;
        }
        assert!(
            dac.is_some() || restart.is_some() || timer_at.is_some() || observe_at.is_some(),
            "priming stalled ({timing:?})"
        );
    }

    let c = sched.counters();
    assert_eq!(
        (c.underfeed_events, c.underfeed_frames, c.drain_frames),
        (out.underfeed_events, out.underfeed_frames, out.drain_frames),
        "the scheduler's counters must match what the simulation saw"
    );
    out.stalled = c.stalled_cycles;
    assert_eq!(out.pulls_over_bound, 0, "a pull exceeded its bound");
    out
}

const RATES: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];
const POW2: [u32; 9] = [32, 64, 128, 256, 512, 1024, 2048, 4096, 8192];
const LOADS: [f64; 3] = [0.0, 0.25, 0.5];

/// Every (G, P, Q) the grid covers: powers of two from 32 to 8192, the odd
/// periods and quanta the driver meets (scaled periods at 44.1 kHz
/// families, forced quanta), and the exact `σ = σ_min` boundaries at no
/// processing budget. The flag marks periods whose nominal count varies.
fn all_timings() -> Vec<(Timing, bool)> {
    let mut v = Vec::new();
    for &g in &RATES {
        for &p in &POW2 {
            for &q in &POW2 {
                v.push((Timing::new(g, p, q), false));
            }
        }
    }
    for (g, p, q) in [
        (88_200, 1881, 1024),
        (176_400, 3763, 2048),
        (48_000, 1024, 1500),
        (44_100, 941, 1024),
        (44_100, 1000, 333),
        (96_000, 3000, 256),
    ] {
        v.push((Timing::new(g, p, q), true));
    }
    for (g, p, q) in BOUNDARIES {
        v.push((Timing::new(g, p, q), false));
    }
    v
}

/// Timings where `σ = σ_min` exactly at no processing budget.
const BOUNDARIES: [(u32, u32, u32); 4] = [
    (48_000, 192, 192), // T_p = 4 ms, c = 3, σ = 1 ms
    (48_000, 288, 144), // T_p = 6 ms, c = 5
    (48_000, 144, 288), // T_p = 3 ms, c = 2
    (48_000, 1920, 64), // T_p = 40 ms, cap 1.2667, c = 39
];

/// Whether the cap guarantees its range (no unity fallback).
fn guaranteed(t: &Timing) -> bool {
    t.speed_cap().sustainable >= 1.0
}

// ------------------------------------------------------------------ tests

/// Wherever the cap guarantees a speed range, no underfeed at any speed it
/// allows: cycles at the cycle bound, timers and the DAC late, and the RT
/// thread busy for 0, 25 and 50 % of each period (with the processing
/// budget set to match), once with random latencies and once with every
/// latency at its maximum.
#[test]
fn no_underfeed_wherever_the_rule_holds() {
    for load in LOADS {
        for worst in [false, true] {
            let (mut n, mut capped, mut periods) = (0, 0, 0u64);
            let mut worst_margin_q = f64::MAX;
            let mut worst_fill = 0.0f64;
            let mut worst_spacing = f64::MAX;
            let mut most_cycles = 0.0f64;
            for (i, (t, vary)) in all_timings().into_iter().enumerate() {
                let t = with_load(t, load);
                if !guaranteed(&t) {
                    continue;
                }
                let cap = t.speed_cap();
                n += 1;
                if cap.reason.is_some() {
                    capped += 1;
                }
                let mut sc = Scenario::new(t, 0x5eed_0000 + i as u64).loaded(load);
                sc.vary_period = vary;
                sc.start_speed = MAX_SPEED;
                if worst {
                    sc.jitter = sc.jitter.worst();
                }
                let o = run(&sc);
                assert_eq!(o.underfeed_events, 0, "{t:?}, cap {cap:?}: {o:?}");
                assert!(o.max_fill <= o.capacity);
                assert!(o.max_speed_seen <= cap.max_speed + 1e-9);
                periods += o.periods;
                worst_margin_q = worst_margin_q.min(o.min_margin as f64 / f64::from(t.quantum));
                worst_fill = worst_fill.max(o.max_fill as f64 / o.capacity as f64);
                worst_spacing = worst_spacing.min(o.min_spacing_ns as f64 / t.slot_ns() as f64);
                let slots = f64::from(cap.cycles_per_period + 1);
                most_cycles = most_cycles.max(o.max_cycles_per_period as f64 / slots);
            }
            eprintln!(
                "rule holds, load {:.0} %, {}: {n} timings ({capped} capped below 2), \
                 {periods} periods; min margin {worst_margin_q:.3} Q, max fill {:.1} % of C, \
                 min trigger spacing {worst_spacing:.3} σ, most cycles in a period \
                 {most_cycles:.3} × slots",
                load * 100.0,
                if worst {
                    "worst-case latencies"
                } else {
                    "random latencies"
                },
                worst_fill * 100.0
            );
            assert!(n >= 150, "{n}");
            assert!(worst_margin_q >= 0.0);
            assert!(worst_spacing >= 0.75 - 1e-6);
        }
    }
}

/// The exact `σ = σ_min` boundaries hold at worst-case latencies.
#[test]
fn the_slot_boundaries_hold() {
    for (i, (g, p, q)) in BOUNDARIES.into_iter().enumerate() {
        let t = Timing::new(g, p, q).with_proc_budget(0);
        assert_eq!(t.slot_ns(), t.cycle_bound_ns, "{t:?} is a boundary");
        for worst in [false, true] {
            let mut sc = Scenario::new(t, 0xb0 + i as u64);
            sc.start_speed = MAX_SPEED;
            sc.seconds = 2.0;
            if worst {
                sc.jitter = sc.jitter.worst();
            }
            let o = run(&sc);
            assert_eq!(o.underfeed_events, 0, "{t:?}: {o:?}");
        }
    }
}

fn representative() -> Timing {
    Timing::new(48_000, 1024, 1024)
}

/// An xrun takes the driver's restart path: the ring is flushed, the
/// scheduler reset and primed again, with a cycle still in flight across
/// the reset (it must not overlap the next one, and lands in the new ring).
#[test]
fn an_xrun_flushes_reprimes_and_never_overlaps() {
    let mut in_flight = 0;
    for (i, t) in [
        representative(),
        Timing::new(44_100, 1024, 256),
        Timing::new(192_000, 4096, 1024),
        Timing::new(48_000, 1024, 128),
    ]
    .into_iter()
    .enumerate()
    {
        for gap in [0, 300_000, 5_000_000, 50_000_000] {
            for worst in [false, true] {
                let mut sc = Scenario::new(t, 0x0e1 + i as u64).loaded(0.5);
                sc.start_speed = MAX_SPEED;
                sc.xrun = Some((10, gap));
                if worst {
                    sc.jitter = sc.jitter.worst();
                }
                let o = run(&sc);
                assert_eq!(o.restarts, 1);
                assert_eq!(o.underfeed_events, 0, "{t:?}, gap {gap}: {o:?}");
                assert!(o.max_fill <= o.capacity);
                in_flight += o.landed_after_flush;
            }
        }
    }
    eprintln!("xrun: {in_flight} runs had a cycle in flight across the reset");
    assert!(
        in_flight > 0,
        "some run must carry a cycle across the reset"
    );
}

/// The derived ring capacity: what the simulation fills, against `C`.
#[test]
fn ring_fill_stays_within_the_derived_capacity() {
    let mut worst = (0.0f64, None);
    for (i, (t, _)) in all_timings().into_iter().enumerate() {
        let t = t.with_proc_budget(0);
        if !guaranteed(&t) {
            continue;
        }
        let mut sc = Scenario::new(t, 0xf111 + i as u64);
        sc.commands = false;
        sc.start_speed = MAX_SPEED;
        sc.seconds = 0.3;
        sc.min_periods = 20;
        let o = run(&sc);
        let r = o.max_fill as f64 / o.capacity as f64;
        // The bound: W_low + Q, W_low = D + Q at the cap.
        let wmax = pull_bound(t.speed_cap().max_speed, t.period) + 2 * t.quantum as usize;
        assert!(o.max_fill <= wmax, "{t:?}: {} > {wmax}", o.max_fill);
        if r > worst.0 {
            worst = (r, Some(t));
        }
    }
    eprintln!(
        "max ring fill {:.1} % of the derived capacity ({:?})",
        worst.0 * 100.0,
        worst.1
    );
}
