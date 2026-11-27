//! Varispeed in the driver's RT loop: the glue between the DAC wakeups,
//! the [`VarispeedScheduler`] and the stage, which lives in the processor
//! and is reached through [`VarispeedPort`].
//!
//! The RT loop owns one [`VarispeedDriver`]. While the processor runs a
//! stage for the sink that plays ([`crate::EngineAudioProcessor::varispeed`]
//! returns a port), the driver uses the watermark schedule instead of the
//! one-cycle-per-quantum pacer:
//!
//! 1. [`VarispeedDriver::pull`]: the stage's `plan(n)`, then the
//!    scheduler's pull (frames to take, zeros to pad, underfeed counted);
//!    the processor hears how much of the block is real
//!    ([`VarispeedPort::set_fill`]) so it can fade over a gap.
//! 2. [`VarispeedDriver::after_pull`]: the speed command stashed since the
//!    last wakeup (a latest-wins [`VarispeedCommandSlot`]) goes to the
//!    scheduler, then the cap, the speed and the ramp go to the stage (they
//!    apply from its next plan), then the next pull's bound.
//! 3. [`VarispeedDriver::poll`] before and after processing, on each cycle
//!    completion (`data_efd`) and at the scheduler's wake time.
//! 4. [`VarispeedDriver::note_load`] after processing: the period's
//!    processing time feeds `rt_load_max` and the processing budget the
//!    speed cap is derated for ([`LoadBudget`]).
//!
//! Everything here is allocation-free and lock-free on the RT side.

use crate::varispeed_sched::{
    CapReason, Counters, HeldReason, MAX_SPEED, MIN_SPEED, Poll, PullKind, Timing,
    VarispeedScheduler, pull_bound,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};

pub use crate::varispeed_sched::Pull;

/// The processor's varispeed stage as the driver sees it. Every method runs
/// on the RT thread and must not allocate, lock or block.
pub trait VarispeedPort {
    /// Latch a block of `n` nominal frames and return the source frames it
    /// takes (`n` when the stage cannot run this block; it then plays the
    /// raw block).
    fn plan(&mut self, n: usize) -> usize;
    /// Of the planned source frames, the first `valid` come from the ring;
    /// the rest are zeros. `underfeed`: the zeros are a gap (fade over it),
    /// not the tail drain's padding.
    fn set_fill(&mut self, valid: usize, underfeed: bool);
    /// The scheduler's cap on the speed (applies from the next plan).
    fn set_speed_limit(&mut self, max: f64);
    /// The speed to head for, and the ramp (applies from the next plan).
    fn set_speed(&mut self, speed: f64, ramp_ms: f64);
    /// The highest speed the stage applies before it settles, if no further
    /// command arrives.
    fn peak_until_settled(&self) -> f64;
    /// The applied speed (at the next output).
    fn speed(&self) -> f64;
    /// The stage's delay, nominal frames.
    fn latency_frames(&self) -> f64;
    /// Reopen or prime: clear the history and hold at the target (the
    /// last [`Self::set_speed`] under the last [`Self::set_speed_limit`]),
    /// no ramp.
    fn reset(&mut self);
    /// The most nominal frames one [`Self::plan`] takes; the driver splits
    /// a longer period into calls of at most this.
    fn max_frames(&self) -> usize {
        usize::MAX
    }
    /// The stage stops running after this block or the next (a disable or
    /// a DSP toggle is fading it out): the period after it pulls raw, one
    /// source frame per frame, so the next bound covers unity.
    fn raw_next(&self) -> bool {
        false
    }
    /// Nominal frames of padding after the last real source frame until
    /// the stage's whole output has played (its filters' full spans and
    /// any source it holds back), for the tail drain.
    fn tail_frames(&self) -> usize {
        self.latency_frames().ceil() as usize
    }
    /// Nominal frames the stage still plays before it stops under a fade,
    /// or before a call must start (a command scheduled there; `None`:
    /// neither). The driver ends a call there, so the raw block takes over
    /// within the period, with no silence after the fade.
    fn frames_until_stop(&self) -> Option<usize> {
        None
    }
    /// The driver finished a pull and sized the next one ([`VarispeedDriver::after_pull`]):
    /// a stop requested before it may start fading from the next plan
    /// (the ring is refilled for a raw pull by then).
    fn pulled(&mut self) {}
    /// The speed the drain's tail and deadline are worked out at: the
    /// slowest the stage plays at before it settles.
    fn drain_speed(&self) -> f64 {
        self.speed()
    }
}

/// The speed command from the control side: the last one wins, and the RT
/// thread reads it at the next DAC wakeup, after that wakeup's plan
/// (a command waits up to one period). A seqlock, so the RT read never
/// blocks.
#[derive(Debug, Default)]
pub struct VarispeedCommandSlot {
    seq: AtomicU64,
    speed: AtomicU64,
    ramp_ms: AtomicU64,
    writer: Mutex<()>,
    /// The overload fallback's hold and its sequence, one word:
    /// `seq << 8 | hold` ([`VarispeedHold`] as `u8`). Each change of the
    /// hold takes the next sequence, which the RT thread acknowledges
    /// once it applies it ([`VarispeedDriver::sync_hold`]).
    hold: AtomicU64,
}

/// What the overload fallback holds varispeed to, mildest first
/// (`control::overload`): each one keeps the ones before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum VarispeedHold {
    /// Nothing held.
    #[default]
    None,
    /// The processing budget pinned at its ceiling (0.5 of the period),
    /// whatever the load measures: the lowest speed cap the timing gives.
    Budget,
    /// The speed capped at unity as well.
    Unity,
    /// The stage retired (the request is kept; the control side does
    /// this, the driver sees no stage).
    Off,
}

impl VarispeedHold {
    fn to_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Budget => 1,
            Self::Unity => 2,
            Self::Off => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Budget,
            2 => Self::Unity,
            3 => Self::Off,
            _ => Self::None,
        }
    }

    /// Stable name for status and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Budget => "budget",
            Self::Unity => "unity",
            Self::Off => "off",
        }
    }
}

impl VarispeedCommandSlot {
    /// Post a command (control side; never the RT thread).
    pub fn send(&self, speed: f64, ramp_ms: f64) {
        let _w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.speed.store(speed.to_bits(), Ordering::Relaxed);
        self.ramp_ms.store(ramp_ms.to_bits(), Ordering::Relaxed);
        self.seq.store(s + 2, Ordering::Release);
    }

    /// Set the overload fallback's hold (control side). The RT thread
    /// applies it at its next iteration. Returns the hold's sequence: a
    /// new one when the hold changes, the current one when it does not.
    pub fn set_hold(&self, hold: VarispeedHold) -> u64 {
        let _w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let (cur, seq) = self.hold_seq();
        if cur == hold {
            return seq;
        }
        let seq = seq + 1;
        self.hold
            .store(seq << 8 | u64::from(hold.to_u8()), Ordering::Release);
        seq
    }

    /// The hold in effect.
    pub fn hold(&self) -> VarispeedHold {
        self.hold_seq().0
    }

    /// The hold in effect and its sequence (0 before any change).
    pub fn hold_seq(&self) -> (VarispeedHold, u64) {
        let w = self.hold.load(Ordering::Acquire);
        (VarispeedHold::from_u8((w & 0xff) as u8), w >> 8)
    }

    /// The command posted after `*seen`, if any (RT side).
    pub fn take_new(&self, seen: &mut u64) -> Option<(f64, f64)> {
        for _ in 0..8 {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 == *seen {
                return None;
            }
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let speed = f64::from_bits(self.speed.load(Ordering::Relaxed));
            let ramp = f64::from_bits(self.ramp_ms.load(Ordering::Relaxed));
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                *seen = s1;
                return Some((speed, ramp));
            }
        }
        None
    }
}

/// What the driver publishes about varispeed scheduling (atomics written by
/// the RT thread, read by status).
#[derive(Debug, Default)]
pub struct VarispeedDriverStatus {
    active: AtomicBool,
    requested: AtomicU64,
    target: AtomicU64,
    cap: AtomicU64,
    sustainable: AtomicU64,
    cap_reason: AtomicU8,
    held: AtomicU8,
    overload_hold: AtomicU8,
    /// The sequence of the hold the RT thread applies.
    hold_ack: AtomicU64,
    budget: AtomicU64,
    watermark: AtomicU64,
    underfeed_events: AtomicU64,
    underfeed_frames: AtomicU64,
    drain_frames: AtomicU64,
    cycles_triggered: AtomicU64,
    stalled_cycles: AtomicU64,
    holds: AtomicU64,
    short_cycles: AtomicU64,
    cycles_timed: AtomicU64,
    cycle_sum_ns: AtomicU64,
    cycle_max_ns: AtomicU64,
    cycles_over_bound: AtomicU64,
}

/// A snapshot of [`VarispeedDriverStatus`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VarispeedDriverSnapshot {
    /// The watermark schedule drives the graph (a stage runs for the sink
    /// that plays).
    pub active: bool,
    /// The last speed command, as given (1 before any).
    pub requested: f64,
    /// What the stage heads for: the request under the cap, or 1 when held.
    pub target: f64,
    /// The speed range's top for the negotiated timing.
    pub cap: f64,
    /// What the slot rule and the ring sustain (may be below 1).
    pub sustainable: f64,
    /// Why the range is narrower than 0.5 to 2.
    pub cap_reason: Option<CapReason>,
    /// Why the speed is held at unity.
    pub held: Option<HeldReason>,
    /// The overload fallback's hold.
    pub overload_hold: VarispeedHold,
    /// The sequence of the hold the RT thread applies
    /// ([`VarispeedCommandSlot::set_hold`]).
    pub hold_ack: u64,
    /// The processing budget the cap is derated for, share of the period.
    pub proc_budget: f64,
    /// The low watermark, frames.
    pub watermark: u64,
    pub underfeed_events: u64,
    pub underfeed_frames: u64,
    pub drain_frames: u64,
    pub cycles_triggered: u64,
    pub stalled_cycles: u64,
    pub holds: u64,
    /// Graph cycles that delivered fewer than a quantum while the schedule
    /// ran (`0 < frames < Q`).
    pub short_cycles: u64,
    /// Cycles timed from the trigger to the poll that saw them complete
    /// (the quantity `σ_min` bounds, wake latency included).
    pub cycles_timed: u64,
    pub cycle_mean_ns: u64,
    pub cycle_max_ns: u64,
    /// Timed cycles seen later than the cycle bound `σ_min`.
    pub cycles_over_bound: u64,
}

fn reason_to_u8(r: Option<CapReason>) -> u8 {
    match r {
        None => 0,
        Some(CapReason::PeriodQuantum) => 1,
        Some(CapReason::QuantumTooSmall) => 2,
        Some(CapReason::RingSize) => 3,
    }
}

fn reason_from_u8(v: u8) -> Option<CapReason> {
    match v {
        1 => Some(CapReason::PeriodQuantum),
        2 => Some(CapReason::QuantumTooSmall),
        3 => Some(CapReason::RingSize),
        _ => None,
    }
}

impl VarispeedDriverStatus {
    /// Whether the schedule runs (the process callback counts short cycles
    /// only then).
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// A graph cycle delivered fewer frames than a quantum.
    pub fn note_short_cycle(&self) {
        self.short_cycles.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> VarispeedDriverSnapshot {
        let f = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
        let u = |a: &AtomicU64| a.load(Ordering::Relaxed);
        VarispeedDriverSnapshot {
            active: self.active.load(Ordering::Relaxed),
            requested: f(&self.requested),
            target: f(&self.target),
            cap: f(&self.cap),
            sustainable: f(&self.sustainable),
            cap_reason: reason_from_u8(self.cap_reason.load(Ordering::Relaxed)),
            held: (self.held.load(Ordering::Relaxed) == 1).then_some(HeldReason::Underfeed),
            overload_hold: VarispeedHold::from_u8(self.overload_hold.load(Ordering::Relaxed)),
            hold_ack: self.hold_ack.load(Ordering::Acquire),
            proc_budget: f(&self.budget),
            watermark: u(&self.watermark),
            underfeed_events: u(&self.underfeed_events),
            underfeed_frames: u(&self.underfeed_frames),
            drain_frames: u(&self.drain_frames),
            cycles_triggered: u(&self.cycles_triggered),
            stalled_cycles: u(&self.stalled_cycles),
            holds: u(&self.holds),
            short_cycles: u(&self.short_cycles),
            cycles_timed: u(&self.cycles_timed),
            cycle_mean_ns: u(&self.cycle_sum_ns)
                .checked_div(u(&self.cycles_timed))
                .unwrap_or(0),
            cycle_max_ns: u(&self.cycle_max_ns),
            cycles_over_bound: u(&self.cycles_over_bound),
        }
    }
}

/// Shortest window the load peak covers before the budget may drop, ns.
pub const LOAD_WINDOW_NS: u64 = 1_000_000_000;
/// Budget steps, share of the period: changes are quantised to this, so
/// small load wobbles never move the cap.
pub const BUDGET_STEP: f64 = 1.0 / 16.0;
/// The lowest budget: a period's load can double from one period to the
/// next (a swap warms a second chain up), and the first period after a
/// rise is late before the measurement sees it.
pub const BUDGET_MIN: f64 = 0.25;
/// The highest budget: the plan's `rt_load_max` limit, and the
/// scheduler's default before anything is measured.
pub const BUDGET_MAX: f64 = 0.5;
/// The budget is `MARGIN · peak load + HEADROOM`, rounded up to a step.
pub const BUDGET_MARGIN: f64 = 1.5;
pub const BUDGET_HEADROOM: f64 = 1.0 / 16.0;

/// The processing budget `B` (share of the period) the speed cap is
/// derated for, from the measured load, with hysteresis.
///
/// - The load is the RT thread's processing time per period over the
///   period's length. Its peak is kept over two one-second buckets, so it
///   covers the last one to two seconds (that peak is `rt_load_max`).
/// - The wanted budget is `1.5 · peak + 1/16`, rounded up to sixteenths,
///   within [`BUDGET_MIN`]..=[`BUDGET_MAX`].
/// - A higher budget (a lower cap) applies at once. A lower one only once
///   it is at least two steps lower, and the peak behind it covers a full
///   second: the range opens up slowly and never flaps on one step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoadBudget {
    budget: f64,
    cur_max: f64,
    prev_max: f64,
    bucket_start_ns: Option<u64>,
    covered: bool,
}

impl Default for LoadBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadBudget {
    pub fn new() -> Self {
        Self {
            budget: BUDGET_MAX,
            cur_max: 0.0,
            prev_max: 0.0,
            bucket_start_ns: None,
            covered: false,
        }
    }

    /// The budget, share of the period.
    pub fn budget(&self) -> f64 {
        self.budget
    }

    /// The load peak over the last one to two seconds (`rt_load_max`).
    pub fn peak(&self) -> f64 {
        self.cur_max.max(self.prev_max)
    }

    /// The budget the current peak asks for (before hysteresis).
    pub fn wanted(&self) -> f64 {
        let raw = BUDGET_MARGIN * self.peak() + BUDGET_HEADROOM;
        ((raw / BUDGET_STEP).ceil() * BUDGET_STEP).clamp(BUDGET_MIN, BUDGET_MAX)
    }

    /// One period's load at `now_ns`. Returns the new budget when it moved.
    pub fn observe(&mut self, now_ns: u64, load: f64) -> Option<f64> {
        let load = if load.is_finite() { load.max(0.0) } else { 1.0 };
        match self.bucket_start_ns {
            None => self.bucket_start_ns = Some(now_ns),
            Some(start) if now_ns.saturating_sub(start) >= LOAD_WINDOW_NS => {
                self.prev_max = self.cur_max;
                self.cur_max = 0.0;
                self.bucket_start_ns = Some(now_ns);
                self.covered = true;
            }
            _ => {}
        }
        self.cur_max = self.cur_max.max(load);
        let wanted = self.wanted();
        if wanted > self.budget || (self.covered && wanted <= self.budget - 2.0 * BUDGET_STEP) {
            self.budget = wanted;
            return Some(wanted);
        }
        None
    }
}

/// The RT loop's varispeed state (see the module docs).
#[derive(Clone, Debug)]
pub struct VarispeedDriver {
    sched: VarispeedScheduler,
    /// The timing without the processing budget.
    base: Timing,
    budget: LoadBudget,
    active: bool,
    seen_cmd: u64,
    speed: f64,
    ramp_ms: f64,
    /// The overload fallback's hold, as last read from the slot, and its
    /// sequence.
    hold: VarispeedHold,
    hold_seq: u64,
    /// The last trigger's time and the completed count then, until a poll
    /// sees it complete.
    timing_cycle: Option<(u64, u64)>,
    cycles_timed: u64,
    cycle_sum_ns: u64,
    cycle_max_ns: u64,
    cycles_over_bound: u64,
}

impl Default for VarispeedDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl VarispeedDriver {
    pub fn new() -> Self {
        let base = Timing::new(48_000, 1024, 1024);
        Self {
            sched: VarispeedScheduler::new(base),
            base,
            budget: LoadBudget::new(),
            active: false,
            seen_cmd: 0,
            speed: 1.0,
            ramp_ms: 0.0,
            hold: VarispeedHold::None,
            hold_seq: 0,
            timing_cycle: None,
            cycles_timed: 0,
            cycle_sum_ns: 0,
            cycle_max_ns: 0,
            cycles_over_bound: 0,
        }
    }

    /// Whether the schedule drives the graph.
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn scheduler(&self) -> &VarispeedScheduler {
        &self.sched
    }

    pub fn load(&self) -> &LoadBudget {
        &self.budget
    }

    /// The timing in use, with its processing budget.
    pub fn timing(&self) -> Timing {
        self.sched.timing()
    }

    fn with_budget(&self, base: Timing) -> Timing {
        let b = (base.period_ns() as f64 * self.proc_budget()) as u64;
        base.with_proc_budget(b)
    }

    /// The processing budget in use: the measured one, or its ceiling
    /// while the overload fallback holds it.
    fn proc_budget(&self) -> f64 {
        if self.hold >= VarispeedHold::Budget {
            BUDGET_MAX
        } else {
            self.budget.budget()
        }
    }

    /// The speed the schedule and the stage head for: the request, at most
    /// unity while the overload fallback caps it.
    fn held_speed(&self) -> f64 {
        if self.hold >= VarispeedHold::Unity {
            self.speed.min(1.0)
        } else {
            self.speed
        }
    }

    /// The overload hold in effect (as last read from the slot).
    pub fn overload_hold(&self) -> VarispeedHold {
        self.hold
    }

    /// The sequence of the hold in effect (the RT thread's
    /// acknowledgement, [`VarispeedCommandSlot::set_hold`]).
    pub fn hold_seq(&self) -> u64 {
        self.hold_seq
    }

    /// Once per RT loop iteration: apply a new hold from the slot (the
    /// budget, the cap and the speed follow it), whether a stage runs or
    /// not, and acknowledge its sequence in `out`. One atomic load when
    /// nothing changed.
    pub fn sync_hold(&mut self, slot: &VarispeedCommandSlot, out: &VarispeedDriverStatus) {
        let (hold, seq) = slot.hold_seq();
        if seq == self.hold_seq {
            return;
        }
        self.apply_hold(hold, seq);
        out.overload_hold
            .store(self.hold.to_u8(), Ordering::Relaxed);
        out.hold_ack.store(seq, Ordering::Release);
    }

    fn apply_hold(&mut self, hold: VarispeedHold, seq: u64) {
        self.hold_seq = seq;
        if hold != self.hold {
            self.hold = hold;
            self.sched.update_timing(self.with_budget(self.base));
            self.sched.command(self.held_speed());
        }
    }

    /// Take the schedule over: a (re)prime (`priming`, the ring just
    /// flushed; the budget starts again at its default) or a stage that
    /// starts while the DAC runs (the ring holds what the pacer left).
    pub fn start(&mut self, base: Timing, priming: bool, port: &mut dyn VarispeedPort) {
        if priming {
            self.budget = LoadBudget::new();
        }
        self.base = base;
        self.sched.reset(self.with_budget(base));
        if !priming {
            self.sched.finish_priming();
        }
        self.active = true;
        self.forward(port);
    }

    /// Take the schedule over for a (re)prime: the latest command first,
    /// then the timing for this geometry (and so its cap), then the stage
    /// reset at the capped target, so the first prime block already plays
    /// at it (a reopen's new cap applies before the reset). The budget
    /// starts again at its default.
    pub fn start_priming(
        &mut self,
        base: Timing,
        port: &mut dyn VarispeedPort,
        slot: &VarispeedCommandSlot,
    ) {
        self.take_command(slot);
        self.budget = LoadBudget::new();
        self.base = base;
        self.sched.reset(self.with_budget(base));
        self.active = true;
        self.forward(port);
        port.reset();
        // The bound again, from the held target.
        self.forward(port);
    }

    /// Make the next pull's bound at least `n` source frames (a raw pull
    /// of `n` frames follows the schedule, so the watermark must cover it).
    pub fn cover_pull(&mut self, n: usize) {
        if self.sched.next_bound() < n {
            self.sched.set_next_bound(n);
        }
    }

    fn take_command(&mut self, slot: &VarispeedCommandSlot) {
        let (hold, seq) = slot.hold_seq();
        let new = slot.take_new(&mut self.seen_cmd);
        if let Some((speed, ramp)) = new {
            self.speed = speed;
            self.ramp_ms = ramp;
        }
        let moved = hold != self.hold;
        // The budget (and so the cap) and the speed follow the hold.
        self.apply_hold(hold, seq);
        if new.is_some() && !moved {
            self.sched.command(self.held_speed());
        }
    }

    /// The pacer takes over again (no stage runs).
    pub fn stop(&mut self) {
        self.active = false;
    }

    /// The DAC started.
    pub fn finish_priming(&mut self) {
        self.sched.finish_priming();
    }

    /// Whether the schedule's timing is `(G, P, Q)`.
    pub fn runs(&self, graph_rate: u32, period: u32, quantum: u32) -> bool {
        self.base.graph_rate == graph_rate
            && self.base.period == period
            && self.base.quantum == quantum
    }

    /// A DAC wakeup's pull of `n` nominal frames, with `ring_fill` frames in
    /// the ring: the stage's plan, then the scheduler's share.
    pub fn pull(
        &mut self,
        port: &mut dyn VarispeedPort,
        now_ns: u64,
        n: usize,
        ring_fill: usize,
        kind: PullKind,
    ) -> Pull {
        let n_src = port.plan(n);
        self.take_planned(port, now_ns, n_src, ring_fill, kind)
    }

    /// [`Self::pull`] for a block the stage already planned (`n_src` source
    /// frames), as priming does: it plans, fills the ring, then pulls.
    pub fn take_planned(
        &mut self,
        port: &mut dyn VarispeedPort,
        now_ns: u64,
        n_src: usize,
        ring_fill: usize,
        kind: PullKind,
    ) -> Pull {
        let pull = self.sched.on_pull(now_ns, ring_fill, n_src, kind);
        port.set_fill(pull.take, pull.underfeed);
        pull
    }

    /// After the pull: the stashed command, then the cap, speed and ramp to
    /// the stage, then the next pull's bound.
    pub fn after_pull(&mut self, port: &mut dyn VarispeedPort, slot: &VarispeedCommandSlot) {
        self.take_command(slot);
        self.forward(port);
        port.pulled();
    }

    /// Cap, speed and ramp to the stage, and the bound for its next pull.
    fn forward(&mut self, port: &mut dyn VarispeedPort) {
        let cap = self.sched.cap().max_speed;
        port.set_speed_limit(if self.hold >= VarispeedHold::Unity {
            cap.min(1.0)
        } else {
            cap
        });
        let (speed, ramp) = if self.sched.held().is_some() || !self.speed.is_finite() {
            (1.0, 0.0)
        } else {
            let ramp = if self.ramp_ms.is_finite() {
                self.ramp_ms.clamp(0.0, 10_000.0)
            } else {
                0.0
            };
            (self.held_speed().clamp(MIN_SPEED, MAX_SPEED), ramp)
        };
        port.set_speed(speed, ramp);
        let mut s_next = port.peak_until_settled().max(port.speed());
        if port.raw_next() {
            // The pull after the stage stops is raw (unity).
            s_next = s_next.max(1.0);
        }
        self.sched
            .set_next_bound(pull_bound(s_next, self.base.period));
    }

    /// Whether to trigger a cycle now.
    pub fn poll(&mut self, now_ns: u64, ring_fill: usize, completed: u64) -> Poll {
        if let Some((t, seen)) = self.timing_cycle
            && seen != completed
        {
            let dt = now_ns.saturating_sub(t);
            self.cycles_timed += 1;
            self.cycle_sum_ns = self.cycle_sum_ns.saturating_add(dt);
            self.cycle_max_ns = self.cycle_max_ns.max(dt);
            if dt > self.sched.timing().cycle_bound_ns {
                self.cycles_over_bound += 1;
            }
            self.timing_cycle = None;
        }
        let p = self.sched.poll(now_ns, ring_fill, completed);
        if p.trigger {
            self.timing_cycle = Some((now_ns, completed));
        }
        p
    }

    /// The quantum changed without a reopen.
    pub fn update_quantum(&mut self, quantum: u32, port: &mut dyn VarispeedPort) {
        self.base = self.base.with_quantum(quantum);
        self.sched.update_timing(self.with_budget(self.base));
        self.forward(port);
    }

    /// One period's processing time against its length. Moves the budget
    /// (and so the cap) when the load asks for it.
    pub fn note_load(
        &mut self,
        now_ns: u64,
        load: f64,
        port: Option<&mut dyn VarispeedPort>,
    ) -> bool {
        if self.budget.observe(now_ns, load).is_none() || !self.active {
            return false;
        }
        self.sched.update_timing(self.with_budget(self.base));
        if let Some(port) = port {
            self.forward(port);
        }
        true
    }

    /// Publish the scheduler's state.
    pub fn publish(&self, out: &VarispeedDriverStatus) {
        let st = self.sched.status();
        let c: Counters = st.counters;
        let r = Ordering::Relaxed;
        out.active.store(self.active, r);
        out.requested.store(st.requested.to_bits(), r);
        out.target.store(st.target.to_bits(), r);
        out.cap.store(st.cap.max_speed.to_bits(), r);
        out.sustainable.store(st.cap.sustainable.to_bits(), r);
        out.cap_reason.store(reason_to_u8(st.cap.reason), r);
        out.held.store(u8::from(st.held.is_some()), r);
        out.overload_hold.store(self.hold.to_u8(), r);
        out.hold_ack.store(self.hold_seq, Ordering::Release);
        out.budget.store(self.proc_budget().to_bits(), r);
        out.watermark.store(st.watermark as u64, r);
        out.underfeed_events.store(c.underfeed_events, r);
        out.underfeed_frames.store(c.underfeed_frames, r);
        out.drain_frames.store(c.drain_frames, r);
        out.cycles_triggered.store(c.cycles_triggered, r);
        out.stalled_cycles.store(c.stalled_cycles, r);
        out.holds.store(c.holds, r);
        out.cycles_timed.store(self.cycles_timed, r);
        out.cycle_sum_ns.store(self.cycle_sum_ns, r);
        out.cycle_max_ns.store(self.cycle_max_ns, r);
        out.cycles_over_bound.store(self.cycles_over_bound, r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// A stage stand-in: plans `ceil(n · s)` and applies speeds at once.
    #[derive(Default)]
    struct Fake {
        speed: f64,
        limit: f64,
        ramp: f64,
        planned: usize,
        valid: Option<(usize, bool)>,
        resets: usize,
    }

    impl VarispeedPort for Fake {
        fn plan(&mut self, n: usize) -> usize {
            self.planned = (n as f64 * self.speed.max(0.5)).ceil() as usize;
            self.planned
        }
        fn set_fill(&mut self, valid: usize, underfeed: bool) {
            self.valid = Some((valid, underfeed));
        }
        fn set_speed_limit(&mut self, max: f64) {
            self.limit = max;
        }
        fn set_speed(&mut self, speed: f64, ramp_ms: f64) {
            self.speed = speed.min(self.limit);
            self.ramp = ramp_ms;
        }
        fn peak_until_settled(&self) -> f64 {
            self.speed
        }
        fn speed(&self) -> f64 {
            self.speed
        }
        fn latency_frames(&self) -> f64 {
            267.5
        }
        fn reset(&mut self) {
            self.resets += 1;
        }
    }

    #[test]
    fn the_command_slot_is_latest_wins_and_read_once() {
        let slot = VarispeedCommandSlot::default();
        let mut seen = 0;
        assert_eq!(slot.take_new(&mut seen), None);
        slot.send(1.5, 200.0);
        slot.send(1.25, 50.0);
        assert_eq!(slot.take_new(&mut seen), Some((1.25, 50.0)));
        assert_eq!(slot.take_new(&mut seen), None);
        slot.send(1.25, 50.0);
        assert_eq!(slot.take_new(&mut seen), Some((1.25, 50.0)));
    }

    #[test]
    fn underfeed_reaches_the_stage_and_holds_at_unity() {
        let mut d = VarispeedDriver::new();
        let mut port = Fake {
            speed: 1.0,
            limit: 2.0,
            ..Fake::default()
        };
        let slot = VarispeedCommandSlot::default();
        d.start(Timing::new(48_000, 1024, 1024), true, &mut port);
        slot.send(1.5, 0.0);
        d.pull(&mut port, 0, 1024, 5000, PullKind::Play);
        d.after_pull(&mut port, &slot);
        for i in 0..3u64 {
            let p = d.pull(&mut port, i * 100 * MS, 1024, 10, PullKind::Play);
            assert!(p.underfeed);
            assert_eq!(port.valid, Some((10, true)));
            d.after_pull(&mut port, &slot);
        }
        assert_eq!(d.scheduler().held(), Some(HeldReason::Underfeed));
        assert_eq!((port.speed, port.ramp), (1.0, 0.0));
        let status = VarispeedDriverStatus::default();
        d.publish(&status);
        let snap = status.snapshot();
        assert_eq!(snap.held, Some(HeldReason::Underfeed));
        assert_eq!(snap.underfeed_events, 3);
        assert!(snap.active);
        // Drain padding is no underfeed.
        let p = d.pull(&mut port, 400 * MS, 1024, 0, PullKind::Drain);
        assert!(!p.underfeed);
        assert_eq!(port.valid, Some((0, false)));
        // The next command releases the hold.
        slot.send(1.2, 100.0);
        d.pull(&mut port, 500 * MS, 1024, 5000, PullKind::Play);
        d.after_pull(&mut port, &slot);
        assert_eq!((port.speed, port.ramp), (1.2, 100.0));
    }

    #[test]
    fn the_overload_hold_pins_the_budget_then_caps_at_unity() {
        // 48 kHz, P = 1024, Q = 128 at a measured 10 % load: cap 1.75 at
        // budget 0.25. The overload fallback's first step pins the budget
        // at 0.5 (cap 1.0) whatever the load says; the next one caps the
        // speed at unity where the timing allows more (Q = 1024).
        let mut d = VarispeedDriver::new();
        let mut port = Fake {
            speed: 1.0,
            limit: 2.0,
            ..Fake::default()
        };
        let slot = VarispeedCommandSlot::default();
        d.start(Timing::new(48_000, 1024, 128), true, &mut port);
        slot.send(1.5, 0.0);
        d.after_pull(&mut port, &slot);
        let mut t = 0;
        for _ in 0..200 {
            d.note_load(t, 0.10, Some(&mut port));
            t += 21 * MS;
        }
        assert_eq!(port.limit, 1.75);
        assert_eq!(port.speed, 1.5);
        slot.set_hold(VarispeedHold::Budget);
        d.after_pull(&mut port, &slot);
        assert_eq!(d.overload_hold(), VarispeedHold::Budget);
        assert_eq!(port.limit, 1.0);
        assert_eq!(port.speed, 1.0);
        // The load measured keeps it pinned.
        for _ in 0..200 {
            d.note_load(t, 0.10, Some(&mut port));
            t += 21 * MS;
        }
        assert_eq!(port.limit, 1.0);
        let status = VarispeedDriverStatus::default();
        d.publish(&status);
        let snap = status.snapshot();
        assert_eq!(snap.proc_budget, 0.5);
        assert_eq!(snap.overload_hold, VarispeedHold::Budget);
        // Released: the measured budget again, and the request.
        slot.set_hold(VarispeedHold::None);
        d.after_pull(&mut port, &slot);
        assert_eq!(port.limit, 1.75);
        assert_eq!(port.speed, 1.5);

        // Q = 1024: the timing sustains 2.0, the unity hold caps it.
        d.update_quantum(1024, &mut port);
        assert_eq!(port.limit, 2.0);
        slot.set_hold(VarispeedHold::Unity);
        d.after_pull(&mut port, &slot);
        assert_eq!(port.limit, 1.0);
        assert_eq!(port.speed, 1.0);
        assert_eq!(d.scheduler().watermark(), pull_bound(1.0, 1024) + 1024);
        // A new command under the hold stays capped.
        slot.send(1.8, 0.0);
        d.after_pull(&mut port, &slot);
        assert_eq!(port.speed, 1.0);
        slot.set_hold(VarispeedHold::None);
        d.after_pull(&mut port, &slot);
        assert_eq!(port.speed, 1.8);
    }
}
