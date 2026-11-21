//! Graph-cycle scheduling for varispeed: the watermark schedule, the speed
//! cap a negotiated period and quantum can sustain, and the driver side of
//! the varispeed stage's pull contract.
//!
//! With varispeed the driver pulls about `s` times more source frames per
//! DAC period than the period holds, so one graph cycle per period no longer
//! keeps up. This module decides when to trigger graph cycles:
//!
//! - **Watermark.** After each pull the driver triggers cycles while
//!   `ring fill + Q · in_flight < W_low`, with `W_low = D + Q` and `D` the
//!   largest pull the next period can make (see [`pull_bound`]). Nothing is
//!   counted or capped: the deficit is recomputed from the ring every time.
//! - **Slots.** A cycle is triggered only after the previous one completed,
//!   and on a slot grid of spacing `σ = (T_p − B) / (c + 1)`, where `B` is
//!   the RT thread's processing budget (it cannot trigger while it
//!   processes) and `c` the most cycles one period can need (see
//!   [`SpeedCap::cycles_per_period`]).
//! - **Speed cap.** A period and quantum sustain a speed `s` when
//!   `σ ≥ σ_min`, the bound on a cycle's completion, and when the low
//!   watermark fits the ring. [`max_speed`] derives the slot limit; the
//!   scheduler clamps every command to the cap and reports why
//!   ([`CapReason`]).
//! - **Loss.** A pull that finds fewer frames than planned is an underfeed:
//!   zero-padded, counted, and after [`UNDERFEED_HOLD_EVENTS`] events within
//!   [`UNDERFEED_HOLD_WINDOW_NS`] the speed is held at unity until the next
//!   command.
//!
//! This is a pure, deterministic state machine: it reads no clock, does no
//! I/O and never allocates (the caller passes the times, the ring fill and
//! the graph's completed-cycle count).

/// Slowest speed on the wire.
pub const MIN_SPEED: f64 = 0.5;
/// Fastest speed on the wire (single device).
pub const MAX_SPEED: f64 = 2.0;

/// Default `σ_min`: the bound on the time from triggering a graph cycle to
/// the driver seeing it completed (the cycle itself plus the driver's wake
/// latency). Graph validation confirms it.
pub const DEFAULT_CYCLE_BOUND_NS: u64 = 1_000_000;

/// Default processing budget, as a divisor of the period: the RT thread may
/// spend up to `T_p / 2` processing a period (the plan's `rt_load_max`
/// limit), and triggers no cycle meanwhile.
pub const DEFAULT_PROC_BUDGET_DIV: u64 = 2;

/// Underfeed events that hold the speed at unity when they fall within
/// [`UNDERFEED_HOLD_WINDOW_NS`].
pub const UNDERFEED_HOLD_EVENTS: usize = 3;
/// The window for [`UNDERFEED_HOLD_EVENTS`].
pub const UNDERFEED_HOLD_WINDOW_NS: u64 = 1_000_000_000;

/// Largest nominal period, in graph frames, that the ring allocation
/// carries at every speed.
pub const MAX_PERIOD_FRAMES: u32 = 8192;
/// Largest graph quantum the ring allocation carries.
pub const MAX_QUANTUM_FRAMES: u32 = 8192;

/// Ring frames allocated once, before the thread turns real-time. Every
/// period and quantum up to the maxima fits at speed 2; larger ones get a
/// lower cap ([`CapReason::RingSize`]).
pub const RING_ALLOC_FRAMES: usize = ring_capacity(MAX_PERIOD_FRAMES, MAX_QUANTUM_FRAMES);

/// A trigger may run late by up to this fraction of a slot and still keep
/// its slot (the next slot stays where it was). Later than that, the slot
/// grid restarts at the trigger.
const SLIP_ALLOWANCE_DIV: u64 = 4;

const _: () = assert!(UNDERFEED_HOLD_EVENTS >= 1);

/// Ring capacity, in graph frames, for a nominal period `period` and a
/// quantum `quantum`: the low watermark at speed 2
/// (`pull_bound(2, P) + Q = 2P + 3 + Q`), one quantum for the cycle that
/// crosses it and one quantum of producer headroom, rounded up to whole
/// quanta. The ring actually allocated is [`RING_ALLOC_FRAMES`];
/// [`Timing::ring_capacity`] clamps to it.
pub const fn ring_capacity(period: u32, quantum: u32) -> usize {
    let q = if quantum == 0 { 1 } else { quantum as usize };
    let need = 2 * period as usize + 3 + 3 * q;
    need.div_ceil(q) * q
}

/// Upper bound on the source frames the stage pulls for `frames` nominal
/// frames when the speed stays at or below `max_speed` over that period:
/// `ceil(frames · s) + 2` (the stage's bound), computed so f64 rounding can
/// only raise it. A zero call pulls nothing.
pub fn pull_bound(max_speed: f64, frames: u32) -> usize {
    if frames == 0 {
        return 0;
    }
    let s = if max_speed.is_finite() {
        max_speed.clamp(0.0, MAX_SPEED)
    } else {
        MAX_SPEED
    };
    // floor(x) + 3 ≥ ceil(x_exact) + 2 for any rounding error below one frame.
    (s * f64::from(frames)).floor() as usize + 3
}

/// `frames` graph frames in ns at `graph_rate` (floor).
fn frames_to_ns(frames: u64, graph_rate: u32) -> u64 {
    let ns = u128::from(frames) * 1_000_000_000 / u128::from(graph_rate.max(1));
    u64::try_from(ns).unwrap_or(u64::MAX)
}

/// The part of a period the driver can trigger cycles in: `T_p − B`, ns.
fn trigger_window_ns(period: u32, graph_rate: u32, proc_budget_ns: u64) -> u64 {
    frames_to_ns(u64::from(period), graph_rate).saturating_sub(proc_budget_ns)
}

/// `k = floor((T_p − B) / σ_min) − 2`: the most cycles a period may average
/// (rounded up) under the slot rule.
fn slot_k(period: u32, graph_rate: u32, cycle_bound_ns: u64, proc_budget_ns: u64) -> u64 {
    let window = trigger_window_ns(period, graph_rate, proc_budget_ns);
    let fit = window.checked_div(cycle_bound_ns).unwrap_or(u64::MAX);
    fit.saturating_sub(2)
}

/// The largest speed the slot rule sustains for a nominal period `period`,
/// a quantum `quantum` and a graph rate `graph_rate`, when a cycle completes
/// within `cycle_bound_ns` and the RT thread spends up to `proc_budget_ns`
/// of each period processing, derated for processing): `k · Q / P`,
/// capped at [`MAX_SPEED`], where `k = floor((T_p − B) / σ_min) − 2`. The
/// result can be below [`MIN_SPEED`], or 0 when even one cycle per period
/// does not fit.
///
/// This is the slot limit only; [`Timing::speed_cap`] also applies the ring
/// allocation and the unity fallback.
pub fn max_speed(
    period: u32,
    quantum: u32,
    graph_rate: u32,
    cycle_bound_ns: u64,
    proc_budget_ns: u64,
) -> f64 {
    let p = period.max(1);
    let q = quantum.max(1);
    let k = slot_k(p, graph_rate, cycle_bound_ns, proc_budget_ns);
    if k == 0 {
        return 0.0;
    }
    (k as f64 * f64::from(q) / f64::from(p)).min(MAX_SPEED)
}

/// Why the speed range is narrower than [`MIN_SPEED`]..=[`MAX_SPEED`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapReason {
    /// The period, quantum and processing budget sustain less than
    /// [`MAX_SPEED`] (but at least unity): the range is capped at
    /// [`SpeedCap::max_speed`].
    PeriodQuantum,
    /// The period, quantum and processing budget do not guarantee even
    /// unity (the quantum is too small for the period's cycle slots). The
    /// range is capped at unity, which is what plays without varispeed
    /// anyway; underfeed is possible if cycles take as long as `σ_min`, and
    /// is counted.
    QuantumTooSmall,
    /// The low watermark at a higher speed would not fit the ring
    /// allocation ([`RING_ALLOC_FRAMES`]): a period or quantum above the
    /// supported maxima. Capped at what fits, or at unity (with the
    /// watermark clamped to the ring, so priming still ends and underfeed is
    /// counted) when not even unity fits.
    RingSize,
}

impl CapReason {
    /// Stable name for status and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PeriodQuantum => "period_quantum",
            Self::QuantumTooSmall => "quantum_too_small",
            Self::RingSize => "ring_size",
        }
    }
}

/// Why the speed is held at unity whatever the command says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldReason {
    /// Repeated underfeed ([`UNDERFEED_HOLD_EVENTS`] within
    /// [`UNDERFEED_HOLD_WINDOW_NS`]), typically a live source that cannot
    /// run ahead of real time. Cleared by the next speed command.
    Underfeed,
}

impl HeldReason {
    /// Stable name for status and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Underfeed => "underfeed",
        }
    }
}

/// The speed range a negotiated timing allows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeedCap {
    /// Commands are clamped to `MIN_SPEED..=max_speed`. Never below 1.
    pub max_speed: f64,
    /// What the slot rule and the ring sustain (may be below 1, or 0).
    pub sustainable: f64,
    /// `c`: the most graph cycles one period can need at `max_speed`.
    pub cycles_per_period: u32,
    /// `None` when the full range is available.
    pub reason: Option<CapReason>,
}

/// The negotiated timing the scheduler works with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Graph rate `G`.
    pub graph_rate: u32,
    /// Largest nominal period `P`, in graph frames (the period planner's
    /// largest count on the chains' grid).
    pub period: u32,
    /// The quantum `Q` PipeWire runs.
    pub quantum: u32,
    /// `σ_min`, see [`DEFAULT_CYCLE_BOUND_NS`].
    pub cycle_bound_ns: u64,
    /// `B`: the most the RT thread spends processing one period (it
    /// triggers no cycle meanwhile), see [`DEFAULT_PROC_BUDGET_DIV`].
    pub proc_budget_ns: u64,
}

impl Timing {
    /// Timing with the default cycle bound and processing budget.
    pub fn new(graph_rate: u32, period: u32, quantum: u32) -> Self {
        Self {
            graph_rate,
            period,
            quantum,
            cycle_bound_ns: DEFAULT_CYCLE_BOUND_NS,
            proc_budget_ns: frames_to_ns(u64::from(period.max(1)), graph_rate)
                / DEFAULT_PROC_BUDGET_DIV,
        }
    }

    /// The same timing with another cycle bound.
    pub fn with_cycle_bound(self, cycle_bound_ns: u64) -> Self {
        Self {
            cycle_bound_ns,
            ..self
        }
    }

    /// The same timing with another processing budget, ns.
    pub fn with_proc_budget(self, proc_budget_ns: u64) -> Self {
        Self {
            proc_budget_ns,
            ..self
        }
    }

    /// The same timing with another quantum (the processing budget, which
    /// depends on the period only, is kept).
    pub fn with_quantum(self, quantum: u32) -> Self {
        Self { quantum, ..self }
    }

    fn q(&self) -> u32 {
        self.quantum.max(1)
    }

    fn p(&self) -> u32 {
        self.period.max(1)
    }

    /// `frames` graph frames in ns (floor).
    pub fn frames_ns(&self, frames: u64) -> u64 {
        frames_to_ns(frames, self.graph_rate)
    }

    /// The period's duration `T_p`, ns.
    pub fn period_ns(&self) -> u64 {
        self.frames_ns(u64::from(self.p()))
    }

    /// The slot limit, see [`max_speed`].
    pub fn max_speed(&self) -> f64 {
        max_speed(
            self.period,
            self.quantum,
            self.graph_rate,
            self.cycle_bound_ns,
            self.proc_budget_ns,
        )
    }

    /// The speed range this timing allows, and why it is narrower.
    pub fn speed_cap(&self) -> SpeedCap {
        let (p, q) = (u64::from(self.p()), u64::from(self.q()));
        // `a`: the cycles a period averages at the cap, rounded up; c = a + 1.
        let full = (2 * p).div_ceil(q);
        // Slot limit.
        let k = slot_k(
            self.p(),
            self.graph_rate,
            self.cycle_bound_ns,
            self.proc_budget_ns,
        );
        let (s_slot, a_slot) = if k >= full {
            (MAX_SPEED, full)
        } else {
            (self.max_speed(), k)
        };
        // Ring limit: W_low = floor(s·P) + 3 + Q must stay within the ring
        // less two quanta, so floor(s·P) ≤ ring − 3Q − 3.
        let room = (self.ring_capacity() as u64).saturating_sub(3 * q + 3);
        let (s_ring, a_ring) = if room >= 2 * p {
            (MAX_SPEED, full)
        } else {
            (room as f64 / p as f64, room.div_ceil(q))
        };
        let ring_binds = s_ring < s_slot;
        let (sustainable, a) = if ring_binds {
            (s_ring, a_ring)
        } else {
            (s_slot, a_slot)
        };
        let (max, a, reason) = if sustainable >= MAX_SPEED {
            (MAX_SPEED, full, None)
        } else {
            let why = if ring_binds {
                CapReason::RingSize
            } else if sustainable >= 1.0 {
                CapReason::PeriodQuantum
            } else {
                CapReason::QuantumTooSmall
            };
            if sustainable >= 1.0 {
                (sustainable, a, Some(why))
            } else {
                (1.0, p.div_ceil(q), Some(why))
            }
        };
        SpeedCap {
            max_speed: max,
            sustainable,
            cycles_per_period: u32::try_from(a.saturating_add(1)).unwrap_or(u32::MAX),
            reason,
        }
    }

    /// The slot spacing `σ = (T_p − B) / (c + 1)` for the cap's `c`, ns.
    pub fn slot_ns(&self) -> u64 {
        let slots = u64::from(self.speed_cap().cycles_per_period).saturating_add(1);
        (trigger_window_ns(self.p(), self.graph_rate, self.proc_budget_ns) / slots).max(1)
    }

    /// The ring this timing may use: [`ring_capacity`] clamped to the
    /// allocation, [`RING_ALLOC_FRAMES`].
    pub fn ring_capacity(&self) -> usize {
        ring_capacity(self.p(), self.q()).min(RING_ALLOC_FRAMES)
    }

    /// The low watermark `W_low = D + Q` for a next pull of at most
    /// `next_bound` frames, kept two quanta below the ring (and at least one
    /// frame, so the schedule never stops on a degenerate quantum).
    pub fn watermark(&self, next_bound: usize) -> usize {
        let q = self.q() as usize;
        let limit = self.ring_capacity().saturating_sub(2 * q).max(1);
        next_bound.saturating_add(q).min(limit)
    }
}

/// What a pull hands the stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullKind {
    /// Steady playback or prefill: a shortfall is underfeed.
    Play,
    /// The tail drain after the source ended: a shortfall is padding, not
    /// underfeed.
    Drain,
}

/// The driver's share of one pull.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pull {
    /// Frames to take from the ring.
    pub take: usize,
    /// Zero frames to append, so the stage gets exactly the planned count.
    pub pad: usize,
    /// Whether the padding counts as underfeed (the processor fades over it).
    pub underfeed: bool,
}

/// What to do after [`VarispeedScheduler::poll`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// Trigger one graph cycle now.
    pub trigger: bool,
    /// Call `poll` again no later than this (besides DAC wakeups and cycle
    /// completions). `None`: nothing to do until one of those.
    pub wake_at_ns: Option<u64>,
}

/// Loss and scheduling counters. They survive resets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Pulls with `n_src > 0`.
    pub pulls: u64,
    /// Pulls that found fewer frames than planned (not drains).
    pub underfeed_events: u64,
    /// Frames zero-padded by those pulls.
    pub underfeed_frames: u64,
    /// Frames zero-padded by drain pulls.
    pub drain_frames: u64,
    /// Graph cycles triggered.
    pub cycles_triggered: u64,
    /// Cycles not seen completing within a period, then given up on.
    pub stalled_cycles: u64,
    /// Times the speed was held at unity for underfeed.
    pub holds: u64,
}

/// Everything status needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SchedulerStatus {
    /// The last commanded speed, as given.
    pub requested: f64,
    /// The speed the stage should run toward (clamped, or 1 when held).
    pub target: f64,
    /// The cap from the negotiated timing.
    pub cap: SpeedCap,
    /// Why the speed is held at unity, if it is.
    pub held: Option<HeldReason>,
    /// The current low watermark, frames.
    pub watermark: usize,
    /// The slot spacing, ns.
    pub slot_ns: u64,
    /// Still priming (the DAC has not started).
    pub priming: bool,
    /// The counters.
    pub counters: Counters,
}

/// The varispeed graph-cycle scheduler. See the module docs.
///
/// Driver contract:
/// - **Open or restart:** [`reset`](Self::reset) (ring flushed), poll until
///   [`is_primed`](Self::is_primed), then [`finish_priming`](Self::finish_priming)
///   when the DAC starts.
/// - **Each DAC wakeup:**
///   1. `n_src = stage.plan(n)`, then [`on_pull`](Self::on_pull) with the
///      ring fill and `n_src`; take `pull.take` frames from the ring and
///      append `pull.pad` zeros.
///   2. Forward pending speed commands and [`target`](Self::target) to the
///      stage (they apply from its next plan), so it knows the next period's
///      trajectory, then [`set_next_bound`](Self::set_next_bound) with
///      `D = pull_bound(s_next, n_max)`.
///   3. [`poll`](Self::poll) right away, before processing; after
///      processing; at `wake_at_ns`; on each cycle completion.
/// - **Quantum change without a reopen:** [`update_timing`](Self::update_timing).
#[derive(Clone, Debug)]
pub struct VarispeedScheduler {
    timing: Timing,
    cap: SpeedCap,
    slot_ns: u64,
    prime_spacing_ns: u64,
    stall_ns: u64,
    requested: f64,
    held: Option<HeldReason>,
    next_bound: usize,
    watermark: usize,
    priming: bool,
    in_flight: bool,
    completed_seen: Option<u64>,
    last_due_ns: Option<u64>,
    last_trigger_ns: u64,
    uf_times: [u64; UNDERFEED_HOLD_EVENTS],
    uf_len: usize,
    uf_next: usize,
    counters: Counters,
}

impl VarispeedScheduler {
    /// A scheduler for `timing`, at unity until commanded, priming.
    pub fn new(timing: Timing) -> Self {
        let mut s = Self {
            timing,
            cap: timing.speed_cap(),
            slot_ns: timing.slot_ns(),
            prime_spacing_ns: 0,
            stall_ns: 0,
            requested: 1.0,
            held: None,
            next_bound: 0,
            watermark: 0,
            priming: true,
            in_flight: false,
            completed_seen: None,
            last_due_ns: None,
            last_trigger_ns: 0,
            uf_times: [0; UNDERFEED_HOLD_EVENTS],
            uf_len: 0,
            uf_next: 0,
            counters: Counters::default(),
        };
        s.reset(timing);
        s
    }

    /// Reopen, xrun restart or DSP reset: new timing, the ring flushed,
    /// priming again. The stage restarts at the target with no ramp, so the
    /// first pull's bound is `pull_bound(target, P)` under the new cap; the
    /// watermark is derived from it here.
    ///
    /// Keeps the requested speed, the hold and the counters, and keeps
    /// tracking a cycle still in flight (the graph finishes it whatever the
    /// DAC does), so no trigger can overlap it.
    pub fn reset(&mut self, timing: Timing) {
        self.apply_timing(timing);
        self.priming = true;
        self.set_next_bound(pull_bound(self.target(), timing.period));
    }

    /// The quantum (or another timing value) changed while playing, without
    /// a reopen: new cap, slots and pacing, the watermark recomputed for the
    /// current next-pull bound. Keeps priming state and in-flight tracking.
    /// Read [`target`](Self::target) after it: the cap may have moved.
    pub fn update_timing(&mut self, timing: Timing) {
        self.apply_timing(timing);
        self.set_next_bound(self.next_bound);
    }

    fn apply_timing(&mut self, timing: Timing) {
        self.timing = timing;
        self.cap = timing.speed_cap();
        self.slot_ns = timing.slot_ns();
        self.stall_ns = timing
            .period_ns()
            .max(timing.cycle_bound_ns.saturating_mul(2))
            .max(self.slot_ns);
        self.update_prime_spacing();
    }

    /// The DAC started (`alsa.start()`): leave the priming pace for the slot
    /// grid.
    pub fn finish_priming(&mut self) {
        self.priming = false;
    }

    /// Priming paces cycles at the target speed (`Q / (s · G)` apart), never
    /// closer than a slot.
    fn update_prime_spacing(&mut self) {
        let t = &self.timing;
        let s = self.target();
        let ns = f64::from(t.q()) * 1e9 / (f64::from(t.graph_rate.max(1)) * s);
        self.prime_spacing_ns = (ns as u64).max(self.slot_ns);
    }

    /// A speed command. Returns the target the stage should ramp to: the
    /// command clamped to `MIN_SPEED..=cap` (non-finite means unity). Clears
    /// an underfeed hold.
    pub fn command(&mut self, speed: f64) -> f64 {
        self.requested = speed;
        self.held = None;
        self.uf_len = 0;
        self.uf_next = 0;
        self.update_prime_spacing();
        self.target()
    }

    /// The speed the stage should run toward.
    pub fn target(&self) -> f64 {
        if self.held.is_some() || !self.requested.is_finite() {
            return 1.0;
        }
        self.requested.clamp(MIN_SPEED, self.cap.max_speed)
    }

    /// The cap for the current timing.
    pub fn cap(&self) -> SpeedCap {
        self.cap
    }

    /// Why the speed is held at unity, if it is.
    pub fn held(&self) -> Option<HeldReason> {
        self.held
    }

    /// The current low watermark, frames.
    pub fn watermark(&self) -> usize {
        self.watermark
    }

    /// The largest pull the next period can make (what the watermark was
    /// set from).
    pub fn next_bound(&self) -> usize {
        self.next_bound
    }

    /// The timing in use.
    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// The counters.
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The spacing between triggered cycles now: the priming pace while
    /// priming, the slot otherwise (ns).
    pub fn cycle_spacing_ns(&self) -> u64 {
        if self.priming {
            self.prime_spacing_ns
        } else {
            self.slot_ns
        }
    }

    /// Still priming: [`finish_priming`](Self::finish_priming) not called
    /// since the last reset.
    pub fn is_priming(&self) -> bool {
        self.priming
    }

    /// Whether the start cushion is in: the DAC may start.
    pub fn is_primed(&self, ring_fill: usize) -> bool {
        ring_fill >= self.watermark
    }

    /// Status snapshot.
    pub fn status(&self) -> SchedulerStatus {
        SchedulerStatus {
            requested: self.requested,
            target: self.target(),
            cap: self.cap,
            held: self.held,
            watermark: self.watermark,
            slot_ns: self.slot_ns,
            priming: self.priming,
            counters: self.counters,
        }
    }

    /// A DAC wakeup's pull. `ring_fill` is the ring's fill before taking,
    /// `n_src` the stage's plan for this period. May start an underfeed
    /// hold, so read [`target`](Self::target) after it. A zero call changes
    /// nothing.
    pub fn on_pull(&mut self, now_ns: u64, ring_fill: usize, n_src: usize, kind: PullKind) -> Pull {
        if n_src == 0 {
            return Pull::default();
        }
        self.counters.pulls += 1;
        let take = n_src.min(ring_fill);
        let pad = n_src - take;
        let underfeed = pad > 0 && kind == PullKind::Play;
        if pad > 0 {
            match kind {
                PullKind::Play => {
                    self.counters.underfeed_events += 1;
                    self.counters.underfeed_frames += pad as u64;
                    self.note_underfeed(now_ns);
                }
                PullKind::Drain => self.counters.drain_frames += pad as u64,
            }
        }
        Pull {
            take,
            pad,
            underfeed,
        }
    }

    /// The largest pull the next period can make ([`pull_bound`] at the
    /// next period's highest speed, after this wakeup's commands and any
    /// hold reached the stage). Sets the low watermark.
    pub fn set_next_bound(&mut self, next_bound: usize) {
        self.next_bound = next_bound;
        self.watermark = self.timing.watermark(next_bound);
    }

    fn note_underfeed(&mut self, now_ns: u64) {
        self.uf_times[self.uf_next] = now_ns;
        self.uf_next = (self.uf_next + 1) % UNDERFEED_HOLD_EVENTS;
        self.uf_len = (self.uf_len + 1).min(UNDERFEED_HOLD_EVENTS);
        if self.uf_len < UNDERFEED_HOLD_EVENTS || self.held.is_some() {
            return;
        }
        // uf_next now points at the oldest of the last N events.
        let oldest = self.uf_times[self.uf_next];
        if now_ns.saturating_sub(oldest) <= UNDERFEED_HOLD_WINDOW_NS {
            self.held = Some(HeldReason::Underfeed);
            self.counters.holds += 1;
            self.update_prime_spacing();
        }
    }

    /// Decide whether to trigger a graph cycle now. `ring_fill` is the
    /// ring's fill in frames, read **before** `cycles_completed`, the
    /// graph's running count of completed cycles (any base).
    pub fn poll(&mut self, now_ns: u64, ring_fill: usize, cycles_completed: u64) -> Poll {
        match self.completed_seen {
            Some(seen) if seen != cycles_completed => self.in_flight = false,
            _ => {}
        }
        self.completed_seen = Some(cycles_completed);
        if self.in_flight {
            let deadline = self.last_trigger_ns.saturating_add(self.stall_ns);
            if now_ns < deadline {
                return Poll {
                    trigger: false,
                    wake_at_ns: Some(deadline),
                };
            }
            self.in_flight = false;
            self.counters.stalled_cycles += 1;
        }
        if ring_fill >= self.watermark {
            return Poll::default();
        }
        let spacing = self.cycle_spacing_ns();
        let due = self
            .last_due_ns
            .map_or(now_ns, |d| d.saturating_add(spacing));
        if now_ns < due {
            return Poll {
                trigger: false,
                wake_at_ns: Some(due),
            };
        }
        // Keep the slot grid when this trigger is only a little late, so
        // timer latency never accumulates across a period; restart it here
        // when later than that (after processing, an idle stretch) and
        // always while priming.
        let late = now_ns - due;
        let keep = !self.priming && late <= spacing / SLIP_ALLOWANCE_DIV;
        self.last_due_ns = Some(if keep { due } else { now_ns });
        self.last_trigger_ns = now_ns;
        self.in_flight = true;
        self.counters.cycles_triggered += 1;
        Poll {
            trigger: true,
            wake_at_ns: Some(now_ns.saturating_add(self.stall_ns)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// The timing with no processing budget (the plan's original rule).
    fn t0(g: u32, p: u32, q: u32) -> Timing {
        Timing::new(g, p, q).with_proc_budget(0)
    }

    #[test]
    fn ring_capacity_matches_the_derivation() {
        // P = Q: 5Q + 3 rounded up to 6 quanta.
        for q in [32u32, 256, 1024, 8192] {
            assert_eq!(ring_capacity(q, q), 6 * q as usize);
        }
        assert_eq!(RING_ALLOC_FRAMES, 49_152);
        // Q ≥ 2P: 2P + 3 + 3Q rounds up to 4 quanta.
        assert_eq!(ring_capacity(1024, 8192), 4 * 8192);
        // Q < P: 2P + 3 + 3Q rounded up.
        assert_eq!(ring_capacity(1024, 256), 2048 + 768 + 256);
        // The watermark at speed 2 always leaves two quanta.
        for p in [32u32, 100, 1024, 1881, 8192] {
            for q in [32u32, 333, 1024, 8192] {
                let t = t0(48_000, p, q);
                let w = pull_bound(2.0, p) + q as usize;
                assert!(w + 2 * q as usize <= t.ring_capacity(), "{p}/{q}");
                assert_eq!(t.watermark(pull_bound(2.0, p)), w);
            }
        }
    }

    #[test]
    fn pull_bound_never_rounds_low() {
        assert_eq!(pull_bound(2.0, 0), 0);
        assert_eq!(pull_bound(1.0, 1024), 1027);
        assert_eq!(pull_bound(2.0, 1024), 2051);
        // ceil(1024 · 1.0123) + 2 = 1039.
        assert_eq!(pull_bound(1.0123, 1024), 1039);
        assert_eq!(pull_bound(f64::NAN, 10), 23);
        for n in 1..2000u32 {
            for s in [0.5, 0.93, 1.0, 1.000_013, 1.0123, 1.08, 1.5, 2.0] {
                let exact = (s * f64::from(n)).ceil() as usize + 2;
                assert!(pull_bound(s, n) >= exact);
                assert!(pull_bound(s, n) <= exact + 1);
            }
        }
    }

    #[test]
    fn max_speed_follows_the_slot_rule() {
        let ms = |p, q, b| max_speed(p, q, 48_000, MS, b);
        // No processing budget: the plan's rule.
        // P = Q = 1024: T_p = 21.3 ms, 21 bounds, k = 19 → 2.
        assert_eq!(ms(1024, 1024, 0), 2.0);
        // P = Q = 192: T_p = 4 ms exactly, k = 2 → 2 ("Q ≥ 192 at 48 kHz").
        assert_eq!(ms(192, 192, 0), 2.0);
        // P = Q = 160: k = 1 → 1. P = Q = 128: k = 0.
        assert_eq!(ms(160, 160, 0), 1.0);
        assert_eq!(ms(128, 128, 0), 0.0);
        assert_eq!(ms(1024, 128, 0), 2.0);
        // P = 256, Q = 64: T_p = 5.33 ms, k = 3 → 0.75.
        assert_eq!(ms(256, 64, 0), 0.75);
        // P = 2Q: T_p ≥ 6 ms for speed 2. Q ≥ 2P: T_p ≥ 3 ms.
        assert_eq!(ms(288, 144, 0), 2.0);
        assert!(ms(284, 142, 0) < 2.0);
        assert_eq!(ms(144, 8192, 0), 2.0);
        assert_eq!(ms(143, 8192, 0), 0.0);
        // A faster graph widens the range.
        assert_eq!(max_speed(256, 64, 48_000, MS / 2, 0), 2.0);

        // Processing derates it: the cycles must fit in T_p − B.
        let half = |p: u32| Timing::new(48_000, p, p).proc_budget_ns;
        assert_eq!(half(1024), 10_666_666);
        // P = 1024, Q = 128, B = T_p/2: 10.67 ms, k = 8 → 1.0.
        assert_eq!(ms(1024, 128, half(1024)), 1.0);
        // B = T_p/4: 16 ms, k = 14 → 1.75.
        assert_eq!(ms(1024, 128, half(1024) / 2), 1.75);
        // P = Q = 192 at half load: 2 ms, k = 0.
        assert_eq!(ms(192, 192, half(192)), 0.0);
        // A budget beyond the period leaves nothing.
        assert_eq!(ms(1024, 1024, 100 * MS), 0.0);
        // A zero cycle bound never limits.
        assert_eq!(max_speed(32, 32, 768_000, 0, 0), 2.0);
    }

    #[test]
    fn speed_cap_and_reasons() {
        let full = t0(48_000, 1024, 1024).speed_cap();
        assert_eq!(full.max_speed, 2.0);
        assert_eq!(full.reason, None);
        assert_eq!(full.cycles_per_period, 3);
        // The default budget (T_p / 2) still allows the full range at P = Q.
        assert_eq!(Timing::new(48_000, 1024, 1024).speed_cap(), full);
        assert_eq!(Timing::new(768_000, 8192, 8192).speed_cap().reason, None);

        // P = 512, Q = 128: T_p = 10.67 ms, k = 8, full = 8 → 2.
        let t = t0(48_000, 512, 128).speed_cap();
        assert_eq!((t.max_speed, t.reason, t.cycles_per_period), (2.0, None, 9));

        // P = 512, Q = 64: k = 8 → 1.0, capped.
        let t = t0(48_000, 512, 64).speed_cap();
        assert_eq!(t.max_speed, 1.0);
        assert_eq!(t.reason, Some(CapReason::PeriodQuantum));
        assert_eq!(t.cycles_per_period, 9);

        // P = 1024, Q = 64: T_p = 21.3 ms, k = 19 → 1.1875.
        let t = t0(48_000, 1024, 64).speed_cap();
        assert_eq!(t.max_speed, 1.1875);
        assert_eq!(t.reason, Some(CapReason::PeriodQuantum));
        assert_eq!(t.reason.unwrap().as_str(), "period_quantum");
        // At the default budget (half the period) it does not sustain unity.
        let t = Timing::new(48_000, 1024, 64).speed_cap();
        assert_eq!((t.max_speed, t.sustainable), (1.0, 0.5));
        assert_eq!(t.reason, Some(CapReason::QuantumTooSmall));
        assert_eq!(t.cycles_per_period, 17);

        // P = 256, Q = 64: sustainable 0.75 < 1 → capped at unity.
        let t = t0(48_000, 256, 64).speed_cap();
        assert_eq!(t.max_speed, 1.0);
        assert_eq!(t.sustainable, 0.75);
        assert_eq!(t.reason, Some(CapReason::QuantumTooSmall));
        assert_eq!(t.cycles_per_period, 5);
    }

    #[test]
    fn periods_beyond_the_allocation_get_a_ring_cap() {
        // P = 16384, Q = 8192: the slots allow 2, the ring does not.
        let t = t0(48_000, 16_384, 8192);
        assert_eq!(t.ring_capacity(), RING_ALLOC_FRAMES);
        let cap = t.speed_cap();
        assert_eq!(cap.reason, Some(CapReason::RingSize));
        assert_eq!(cap.reason.unwrap().as_str(), "ring_size");
        let room = RING_ALLOC_FRAMES - 3 * 8192 - 3;
        assert_eq!(cap.max_speed, room as f64 / 16_384.0);
        assert!(cap.max_speed > 1.49 && cap.max_speed < 1.5);
        // The watermark at the cap fits, so priming ends.
        let mut s = VarispeedScheduler::new(t);
        assert_eq!(s.command(2.0), cap.max_speed);
        s.reset(t);
        let w = s.watermark();
        assert_eq!(w, pull_bound(cap.max_speed, 16_384) + 8192);
        assert!(w + 2 * 8192 <= RING_ALLOC_FRAMES);
        assert!(s.is_primed(w));

        // P = 32768: not even unity fits; capped at 1, the watermark clamped
        // to the ring so priming still ends (underfeed is then counted).
        let t = t0(48_000, 32_768, 8192);
        let cap = t.speed_cap();
        assert_eq!(
            (cap.max_speed, cap.reason),
            (1.0, Some(CapReason::RingSize))
        );
        assert!(cap.sustainable < 1.0);
        let s = VarispeedScheduler::new(t);
        assert_eq!(s.watermark(), RING_ALLOC_FRAMES - 2 * 8192);
        assert!(s.is_primed(RING_ALLOC_FRAMES - 2 * 8192));

        // A degenerate quantum larger than the allocation still primes.
        let t = t0(48_000, 1024, 65_536);
        assert_eq!(t.watermark(pull_bound(1.0, 1024)), 1);
    }

    #[test]
    fn commands_are_clamped_and_holds_clear() {
        let t = t0(48_000, 1024, 64); // cap 1.1875
        let mut s = VarispeedScheduler::new(t);
        assert_eq!(s.target(), 1.0);
        assert_eq!(s.command(1.5), 1.1875);
        assert_eq!(s.command(0.1), MIN_SPEED);
        assert_eq!(s.command(f64::NAN), 1.0);
        assert_eq!(s.command(1.1), 1.1);
        // A reopen to a roomier quantum lifts the cap on the same request.
        s.command(1.5);
        s.reset(t0(48_000, 1024, 1024));
        assert_eq!(s.target(), 1.5);
    }

    #[test]
    fn reset_derives_the_watermark_from_the_new_cap() {
        // Requested 1.5 under a 1.1875 cap, then a reopen that allows it: the
        // first pull runs at 1.5, so the watermark must be for 1.5.
        let mut s = VarispeedScheduler::new(t0(48_000, 1024, 64));
        assert_eq!(s.command(1.5), 1.1875);
        // The driver sets the bound once the stage has the command.
        s.set_next_bound(pull_bound(1.1875, 1024));
        assert_eq!(s.watermark(), pull_bound(1.1875, 1024) + 64);
        let wide = t0(48_000, 1024, 128);
        s.reset(wide);
        assert_eq!(s.target(), 1.5);
        assert_eq!(s.watermark(), pull_bound(1.5, 1024) + 128);
        assert!(s.is_priming());
    }

    #[test]
    fn reset_keeps_tracking_a_cycle_in_flight() {
        let t = t0(48_000, 1024, 1024);
        let mut s = VarispeedScheduler::new(t);
        assert!(s.poll(0, 0, 5).trigger);
        s.reset(t);
        // Still in flight after the reset: no overlapping trigger.
        let p = s.poll(MS, 0, 5);
        assert!(!p.trigger);
        assert_eq!(p.wake_at_ns, Some(t.period_ns()));
        // Its completion lets the next one go at the priming pace (Q / G).
        assert!(s.poll(t.period_ns(), 0, 6).trigger);
        assert_eq!(s.counters().stalled_cycles, 0);
    }

    #[test]
    fn update_timing_recaps_without_repriming() {
        let t = t0(48_000, 1024, 1024);
        let mut s = VarispeedScheduler::new(t);
        s.command(1.5);
        s.reset(t);
        s.finish_priming();
        s.set_next_bound(pull_bound(1.5, 1024));
        assert!(s.poll(0, 0, 0).trigger);
        // A client forces Q = 64: the cap drops to 1.1875 at once, the
        // watermark follows the new quantum, the cycle stays in flight.
        let small = t.with_quantum(64);
        s.update_timing(small);
        assert_eq!(s.target(), 1.1875);
        assert_eq!(s.cap().reason, Some(CapReason::PeriodQuantum));
        assert_eq!(s.watermark(), pull_bound(1.5, 1024) + 64);
        assert!(!s.is_priming());
        assert!(!s.poll(1, 0, 0).trigger, "the cycle is still in flight");
        assert_eq!(s.status().slot_ns, small.slot_ns());
        // And back.
        s.update_timing(t);
        assert_eq!(s.target(), 1.5);
        assert_eq!(s.watermark(), pull_bound(1.5, 1024) + 1024);
    }

    #[test]
    fn three_underfeeds_within_a_second_hold_at_unity() {
        let t = t0(48_000, 1024, 1024);
        let mut s = VarispeedScheduler::new(t);
        s.command(1.5);
        let sec = 1_000_000_000;
        // Two events, then one 1.1 s after the first: no hold.
        let p = s.on_pull(0, 100, 1536, PullKind::Play);
        assert_eq!(
            p,
            Pull {
                take: 100,
                pad: 1436,
                underfeed: true
            }
        );
        s.on_pull(sec / 2, 0, 1536, PullKind::Play);
        s.on_pull(sec + sec / 10, 0, 1536, PullKind::Play);
        assert_eq!(s.held(), None);
        // Drains never count.
        for i in 0..5 {
            let p = s.on_pull(sec + sec / 5 + i, 0, 1536, PullKind::Drain);
            assert!(!p.underfeed);
        }
        assert_eq!(s.held(), None);
        // A third within one second of the second: held.
        s.on_pull(sec + sec / 2, 1000, 1536, PullKind::Play);
        assert_eq!(s.held(), Some(HeldReason::Underfeed));
        assert_eq!(s.target(), 1.0);
        let c = s.counters();
        assert_eq!(c.underfeed_events, 4);
        assert_eq!(c.underfeed_frames, 1436 + 1536 + 1536 + 536);
        assert_eq!(c.drain_frames, 5 * 1536);
        assert_eq!(c.holds, 1);
        // A full pull is not underfeed; a reset keeps the hold.
        assert!(!s.on_pull(2 * sec, 5000, 1536, PullKind::Play).underfeed);
        s.reset(t);
        assert_eq!(s.held(), Some(HeldReason::Underfeed));
        // The next command releases it and starts a fresh window.
        assert_eq!(s.command(1.5), 1.5);
        assert_eq!(s.held(), None);
        s.on_pull(3 * sec, 0, 1536, PullKind::Play);
        s.on_pull(3 * sec + 1, 0, 1536, PullKind::Play);
        assert_eq!(s.held(), None);
    }

    #[test]
    fn a_zero_call_changes_nothing() {
        let t = t0(48_000, 1024, 1024);
        let mut s = VarispeedScheduler::new(t);
        let before = s.status();
        assert_eq!(s.on_pull(0, 0, 0, PullKind::Play), Pull::default());
        assert_eq!(s.status(), before);
        assert!(s.is_priming(), "pulls never end priming");
        s.finish_priming();
        assert!(!s.is_priming());
    }

    #[test]
    fn slots_space_cycles_and_wait_for_completion() {
        // P = 1024, Q = 256 at 48 kHz, no budget: c = 9, σ = T_p / 10.
        let t = t0(48_000, 1024, 256);
        let slot = t.slot_ns();
        assert_eq!(t.speed_cap().cycles_per_period, 9);
        assert_eq!(slot, 21_333_333 / 10);
        // The default budget halves the window: σ = (T_p / 2) / 10.
        assert_eq!(Timing::new(48_000, 1024, 256).slot_ns(), 10_666_667 / 10);
        let mut s = VarispeedScheduler::new(t);
        s.finish_priming();
        s.set_next_bound(pull_bound(2.0, 1024));
        let w = s.watermark();
        assert_eq!(w, 2051 + 256);
        let mut done = 0u64;
        assert!(s.poll(0, 0, done).trigger);
        // In flight: no second trigger, even when a slot passes.
        assert!(!s.poll(slot, 0, done).trigger);
        done += 1;
        // Completed but before the next slot: wait for it.
        let p = s.poll(slot / 2, 256, done);
        assert_eq!(
            p,
            Poll {
                trigger: false,
                wake_at_ns: Some(slot)
            }
        );
        // A little late: triggers, and the grid stays put.
        let late = slot + slot / 8;
        assert!(s.poll(late, 256, done).trigger);
        done += 1;
        assert_eq!(s.poll(late + 1, 512, done).wake_at_ns, Some(2 * slot));
        // Much later (say after processing): the grid restarts there.
        let after = 5 * slot;
        assert!(s.poll(after, 512, done).trigger);
        done += 1;
        assert_eq!(s.poll(after + 1, 768, done).wake_at_ns, Some(after + slot));
        // Full: nothing to do until the next pull.
        assert_eq!(s.poll(after + slot, w, done), Poll::default());
    }

    #[test]
    fn a_cycle_that_never_completes_is_given_up_after_a_period() {
        let t = t0(48_000, 1024, 1024);
        let mut s = VarispeedScheduler::new(t);
        assert!(s.poll(0, 0, 7).trigger);
        let p = s.poll(1, 0, 7);
        assert_eq!(p.wake_at_ns, Some(t.period_ns()));
        assert!(s.poll(t.period_ns(), 0, 7).trigger);
        assert_eq!(s.counters().stalled_cycles, 1);
        assert_eq!(s.counters().cycles_triggered, 2);
    }

    #[test]
    fn priming_is_paced_at_the_target_speed_without_slip() {
        let t = Timing::new(48_000, 1024, 1024);
        let mut s = VarispeedScheduler::new(t);
        s.command(2.0);
        s.reset(t);
        assert!(s.poll(0, 0, 0).trigger);
        let p = s.poll(1, 1024, 1);
        // Q / (2 · G) = 10.67 ms.
        let gap = 10_666_666;
        assert_eq!(p.wake_at_ns, Some(gap));
        // Late by a little: priming restarts its pace at the trigger.
        assert!(s.poll(gap + 1000, 1024, 1).trigger);
        assert_eq!(s.poll(gap + 1001, 2048, 2).wake_at_ns, Some(2 * gap + 1000));
        assert!(!s.is_primed(2048));
        assert!(s.is_primed(s.watermark()));
    }

    #[test]
    fn extreme_inputs_do_not_overflow() {
        let t = Timing {
            graph_rate: 1,
            period: u32::MAX,
            quantum: 1,
            cycle_bound_ns: u64::MAX,
            proc_budget_ns: u64::MAX,
        };
        let cap = t.speed_cap();
        assert_eq!(cap.max_speed, 1.0);
        let mut s = VarispeedScheduler::new(t);
        s.poll(u64::MAX, 0, 0);
        s.poll(u64::MAX, 0, 1);
        let t = Timing::new(768_000, 1, u32::MAX).with_cycle_bound(1);
        let _ = t.speed_cap();
        let _ = VarispeedScheduler::new(t);
    }
}
