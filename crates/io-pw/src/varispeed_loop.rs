//! Varispeed's steps in the RT loop, apart from the loop so an offline
//! harness drives the same code:
//!
//! - [`poll_trigger`]: one scheduler poll, and the trigger it asks for,
//!   with the clock `delay` worked out at that trigger ([`VsDelay`]);
//! - [`fill_until`]: priming and refills, triggering on the schedule's
//!   pace until the ring holds what a pull needs;
//! - [`handover`]: at a DAC wakeup, start, retune or restart the schedule,
//!   or hand the graph back to the pacer once the ring holds the first raw
//!   pull;
//! - [`run_period`]: one period's pulls and blocks, split into stage calls
//!   of at most [`VarispeedPort::max_frames`];
//! - [`VsDrain`] and [`drain_deadline_ns`]: the tail drain.
//!
//! The graph, the clock and the waits come through [`VsGraph`]: PipeWire,
//! `data_efd` and the monotonic clock in the driver, a simulation in tests.

use crate::engine_driver::EngineAudioProcessor;
use crate::varispeed_rt::{Pull, VarispeedCommandSlot, VarispeedDriver, VarispeedPort};
use crate::varispeed_sched::{PullKind, Timing};
use audivane_protocol::RateRatio;
use audivane_protocol::ipc::SinkChoice;

/// What the varispeed steps need from the graph and the clock.
pub trait VsGraph {
    /// Monotonic time, ns.
    fn now_ns(&self) -> u64;
    /// The DSP sink's completed graph cycles (any base).
    fn completed(&self) -> u64;
    /// Trigger one graph cycle of `quantum` frames at `speed`, with the
    /// clock's downstream `delay` in graph frames.
    fn trigger(&mut self, rate: u32, quantum: u64, delay: i64, speed: f64);
    /// Sleep until a cycle completes or `until_ns`, whichever comes first.
    fn wait_until(&mut self, until_ns: u64);
    /// The driver is stopping: give up waiting.
    fn stopping(&self) -> bool {
        false
    }
    /// Drop what the other sink's ring holds (it fills too while priming).
    fn drain_other(&mut self) {}
    /// A point where the audio thread's overload guard runs (after a wait,
    /// between stage calls): the loops here can run for a while without
    /// returning to the RT loop's own checkpoint.
    fn checkpoint(&mut self) {}
}

/// The DSP ring's fill, frames.
#[inline]
pub fn ring_fill(ring: &rtrb::Consumer<f32>, channels: usize) -> usize {
    ring.slots() / channels.max(1)
}

/// The clock's downstream `delay` while varispeed runs, worked out
/// at each trigger: the graph frames between the cycle being triggered and
/// the listener.
///
/// - the ring's fill, read at the trigger (graph frames, unscaled);
/// - the frames taken for the period being processed and not yet written
///   (nominal frames);
/// - the DAC's queue, tracked from its last read: the frames written since
///   are added, and while it runs the frames played since are taken off;
/// - the chain's latency (the stage's included).
///
/// Everything but the ring is in nominal frames and counts `s` times, as
/// each nominal frame consumes `s` graph frames.
#[derive(Clone, Copy, Debug)]
pub struct VsDelay {
    ratio: RateRatio,
    out_rate: u32,
    dac_out: i64,
    dac_ns: u64,
    running: bool,
    written_out: u64,
    pending: usize,
    latency_out: usize,
}

impl Default for VsDelay {
    fn default() -> Self {
        Self::new(RateRatio::new(1, 1), 48_000)
    }
}

impl VsDelay {
    /// For a device at `ratio` from `graph_rate`, nothing queued yet.
    pub fn new(ratio: RateRatio, graph_rate: u32) -> Self {
        Self {
            ratio,
            out_rate: ratio.output_rate(graph_rate).unwrap_or(graph_rate).max(1),
            dac_out: 0,
            dac_ns: 0,
            running: false,
            written_out: 0,
            pending: 0,
            latency_out: 0,
        }
    }

    /// The negotiated geometry (`ratio` from `graph_rate`), set at every
    /// open and every live handover: what is tracked is kept.
    pub fn set_geometry(&mut self, ratio: RateRatio, graph_rate: u32) {
        self.ratio = ratio;
        self.out_rate = ratio.output_rate(graph_rate).unwrap_or(graph_rate).max(1);
    }

    /// The DAC's queue read at `now_ns` (`delay_out` output frames);
    /// `running`: it plays (it does not while priming).
    pub fn dac_read(&mut self, now_ns: u64, delay_out: i64, running: bool) {
        self.dac_out = delay_out.max(0);
        self.dac_ns = now_ns;
        self.running = running;
        self.written_out = 0;
    }

    /// The chain's latency (stage included), output frames.
    pub fn set_latency(&mut self, latency_out: usize) {
        self.latency_out = latency_out;
    }

    /// `nominal` frames taken from the ring for the block in work.
    pub fn took(&mut self, nominal: usize) {
        self.pending = self.pending.saturating_add(nominal);
    }

    /// The block in work (`nominal` frames) was written: `out` frames.
    pub fn wrote(&mut self, out: usize, nominal: usize) {
        self.written_out = self.written_out.saturating_add(out as u64);
        self.pending = self.pending.saturating_sub(nominal);
    }

    /// Nothing in work (a reset or a block not written).
    pub fn clear_pending(&mut self) {
        self.pending = 0;
    }

    /// The DAC's queue at `now_ns`, output frames.
    pub fn dac_at(&self, now_ns: u64) -> i64 {
        let mut q = self.dac_out.saturating_add(self.written_out as i64);
        if self.running {
            let played = (now_ns.saturating_sub(self.dac_ns) as u128 * u128::from(self.out_rate)
                / 1_000_000_000) as i64;
            q = q.saturating_sub(played);
        }
        q.max(0)
    }

    /// The `delay` for a trigger at `now_ns` with `ring_fill` frames in the
    /// ring at speed `speed`.
    pub fn at(&self, now_ns: u64, ring_fill: usize, speed: f64) -> i64 {
        let s = if speed.is_finite() && speed > 0.0 {
            speed
        } else {
            1.0
        };
        let to_in = |out: u64| {
            f64::from(
                self.ratio
                    .scale_to_input(out.min(u64::from(u32::MAX)) as u32),
            )
        };
        let nominal = to_in(self.dac_at(now_ns) as u64)
            + to_in(self.latency_out as u64)
            + self.pending as f64;
        (nominal * s).round() as i64 + ring_fill as i64
    }
}

/// One scheduler poll on the DSP ring, and the trigger it asks for (the
/// fill is read before the completed-cycle counter). Returns when to poll
/// again.
#[allow(clippy::too_many_arguments)]
pub fn poll_trigger<G: VsGraph + ?Sized>(
    vs: &mut VarispeedDriver,
    graph: &mut G,
    ring: &rtrb::Consumer<f32>,
    channels: usize,
    rate: u32,
    quantum: u64,
    delay: &VsDelay,
    speed: f64,
) -> Option<u64> {
    let fill = ring_fill(ring, channels);
    let completed = graph.completed();
    let now = graph.now_ns();
    let p = vs.poll(now, fill, completed);
    if p.trigger {
        graph.trigger(rate, quantum, delay.at(now, fill, speed), speed);
    }
    p.wake_at_ns
}

/// The longest [`fill_until`] waits for the cycles a fill needs (its
/// stall allowance aside).
pub const MAX_FILL_WAIT_NS: u64 = 1_000_000_000;

/// Trigger cycles on the schedule's pace (priming's while it primes) until
/// the ring holds `want` frames, the wait runs out or the driver stops.
/// The wait is the cycles the fill needs at the schedule's spacing (a slow
/// prime at a large quantum spaces them far apart) plus `timeout_ns`, the
/// allowance for a stalled cycle; at most [`MAX_FILL_WAIT_NS`] in all.
/// Returns whether the ring holds them. The other sink's ring is drained
/// meanwhile.
#[allow(clippy::too_many_arguments)]
pub fn fill_until<G: VsGraph + ?Sized>(
    vs: &mut VarispeedDriver,
    graph: &mut G,
    ring: &rtrb::Consumer<f32>,
    channels: usize,
    rate: u32,
    quantum: u64,
    delay: &VsDelay,
    speed: f64,
    want: usize,
    timeout_ns: u64,
) -> bool {
    let need = want.saturating_sub(ring_fill(ring, channels)) as u64;
    let cycles = need.div_ceil(quantum.max(1));
    let wait = cycles
        .saturating_mul(vs.scheduler().cycle_spacing_ns())
        .saturating_add(timeout_ns)
        .min(MAX_FILL_WAIT_NS.max(timeout_ns));
    let deadline = graph.now_ns().saturating_add(wait);
    loop {
        graph.drain_other();
        if ring_fill(ring, channels) >= want {
            return true;
        }
        let now = graph.now_ns();
        if now >= deadline || graph.stopping() {
            return false;
        }
        let wake = poll_trigger(vs, graph, ring, channels, rate, quantum, delay, speed)
            .unwrap_or(deadline)
            .min(deadline);
        graph.wait_until(wake.max(graph.now_ns().saturating_add(50_000)));
        graph.checkpoint();
    }
}

/// What [`handover`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handover {
    /// Nothing to change.
    Unchanged,
    /// The schedule took over (a stage started while the DAC runs).
    Started,
    /// The quantum changed: new timing, the stage got the new cap.
    Retuned,
    /// Another period: the schedule restarted.
    Restarted,
    /// No stage runs any more: the pacer takes over. `refilled`: the ring
    /// held the first raw pull by then.
    HandedBack { refilled: bool },
}

/// At a DAC wakeup: start the schedule when a stage runs (`port`) and it
/// does not yet (no priming: the ring holds what the pacer left), retune it
/// when the quantum changed without a reopen, restart it on another period,
/// and hand back to the pacer when no stage runs. Before handing back it
/// waits (up to `wait_ns`) until the ring holds `raw_pull` frames, the
/// first raw pull's, on the schedule's own pace: a stage that ran slow
/// left the ring at its own low watermark.
#[allow(clippy::too_many_arguments)]
pub fn handover<G: VsGraph + ?Sized>(
    vs: &mut VarispeedDriver,
    graph: &mut G,
    ring: &rtrb::Consumer<f32>,
    port: Option<&mut dyn VarispeedPort>,
    channels: usize,
    graph_rate: u32,
    period: u32,
    quantum: u32,
    raw_pull: usize,
    delay: &VsDelay,
    wait_ns: u64,
) -> Handover {
    let q = quantum.max(1);
    match port {
        Some(port) => {
            if !vs.is_active() {
                vs.start(Timing::new(graph_rate, period, q), false, port);
                Handover::Started
            } else if !vs.runs(graph_rate, period, q) {
                if vs.runs(graph_rate, period, vs.timing().quantum) {
                    vs.update_quantum(q, port);
                    Handover::Retuned
                } else {
                    vs.start(Timing::new(graph_rate, period, q), false, port);
                    Handover::Restarted
                }
            } else {
                Handover::Unchanged
            }
        }
        None => {
            if !vs.is_active() {
                return Handover::Unchanged;
            }
            vs.cover_pull(raw_pull);
            let refilled = fill_until(
                vs,
                graph,
                ring,
                channels,
                graph_rate,
                u64::from(q),
                delay,
                1.0,
                raw_pull,
                wait_ns,
            );
            vs.stop();
            Handover::HandedBack { refilled }
        }
    }
}

/// `n` nominal frames as calls of at most `max`, as even as possible:
/// `(calls, size of the first, how many take one frame more)`.
pub fn split_calls(n: usize, max: usize) -> (usize, usize, usize) {
    let max = max.max(1);
    let calls = n.div_ceil(max).max(1);
    (calls, n / calls, n % calls)
}

/// The buffers a period works in.
pub struct PeriodBufs<'a> {
    /// The DSP ring.
    pub ring: &'a mut rtrb::Consumer<f32>,
    /// A stage call's source frames (interleaved), at least
    /// `2 · max_frames + 2` frames.
    pub dsp_in: &'a mut [f32],
    /// What the processor gets as the Direct block (unused on the DSP sink).
    pub direct_in: &'a [f32],
    /// The period's output, interleaved.
    pub out: &'a mut [i32],
}

/// What [`run_period`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeriodRun {
    /// Source frames taken from the ring.
    pub taken: usize,
    /// Source frames the blocks held (taken and padded).
    pub pulled: usize,
    /// Stage calls (and blocks) the period took.
    pub calls: usize,
    /// The ring was short while playing.
    pub underfeed: bool,
    /// When to poll the schedule again (the last poll's answer, with
    /// [`PeriodPlan::poll`]).
    pub wake_at_ns: Option<u64>,
}

/// How [`run_period`] runs.
#[derive(Clone, Copy, Debug)]
pub struct PeriodPlan {
    pub sink: SinkChoice,
    pub ratio: RateRatio,
    pub graph_rate: u32,
    pub channels: usize,
    /// The planner's input position at the period's start.
    pub pos: u64,
    /// Nominal frames (input to the chains) and output frames.
    pub n_in: usize,
    pub n_out: usize,
    pub kind: PullKind,
    /// Priming: wait for each call's source frames, the cycles they need
    /// on the priming pace plus this stall allowance (ns), [`fill_until`].
    pub prime_wait_ns: Option<u64>,
    /// Poll the schedule around each block (the steady loop).
    pub poll: bool,
    pub quantum: u64,
}

/// One period through the stage: for each call (at most
/// [`VarispeedPort::max_frames`] frames, split evenly, and ending where a
/// stopping stage's fade ends, [`VarispeedPort::frames_until_stop`]), the
/// stage's plan, the
/// scheduler's pull, the frames from the ring, and the processor's block
/// into its share of `out` on the rational frame grid. The command slot is
/// read after the period's last plan, so a command never raises a call's
/// speed above the bound the watermark was sized for. A stage that retires
/// within the period (a fade-out ending) leaves the rest to raw blocks.
///
/// Returns `None` when no stage runs (the caller plays the raw period) or
/// when priming found no source frames for the first call (nothing was
/// taken; the caller primes silence).
#[allow(clippy::too_many_arguments)]
pub fn run_period<P: EngineAudioProcessor + ?Sized, G: VsGraph + ?Sized>(
    vs: &mut VarispeedDriver,
    graph: &mut G,
    processor: &mut P,
    bufs: PeriodBufs<'_>,
    slot: &VarispeedCommandSlot,
    plan: PeriodPlan,
    delay: &mut VsDelay,
) -> Option<PeriodRun> {
    let ch = plan.channels.max(1);
    processor.varispeed(plan.sink, plan.ratio, plan.graph_rate, ch)?;
    let mut run = PeriodRun::default();
    let mut off_in = 0usize;
    let mut off_out = 0usize;
    let mut k = 0usize;
    while off_in < plan.n_in {
        let left = plan.n_in - off_in;
        let (n_k, stage_runs) =
            match processor.varispeed(plan.sink, plan.ratio, plan.graph_rate, ch) {
                Some(port) => {
                    let (_, base, extra) = split_calls(left, port.max_frames());
                    let even = base + usize::from(extra > 0);
                    let n = match port.frames_until_stop() {
                        Some(u) => even.min(u.max(1)),
                        None => even,
                    };
                    (n, true)
                }
                // The stage stopped inside the period: one raw block for the
                // rest (the next bound covered it, `VarispeedPort::raw_next`).
                None => (left, false),
            };
        let pos = plan.pos + off_in as u64;
        let out_k = plan.ratio.frames_out(pos, n_k);
        let last = off_in + n_k >= plan.n_in;
        let mut speed = 1.0;
        let port = if stage_runs {
            processor.varispeed(plan.sink, plan.ratio, plan.graph_rate, ch)
        } else {
            None
        };
        let (len, took) = match port {
            Some(port) => {
                let n_src = port.plan(n_k);
                if let Some(wait) = plan.prime_wait_ns {
                    let s = port.speed();
                    let ok = fill_until(
                        vs,
                        graph,
                        bufs.ring,
                        ch,
                        plan.graph_rate,
                        plan.quantum,
                        delay,
                        s,
                        n_src,
                        wait,
                    );
                    if !ok && k == 0 {
                        return None;
                    }
                }
                let fill = ring_fill(bufs.ring, ch);
                let pull = vs.take_planned(port, graph.now_ns(), n_src, fill, plan.kind);
                if last {
                    vs.after_pull(port, slot);
                }
                speed = port.speed();
                run.underfeed |= pull.underfeed;
                (take(bufs.ring, bufs.dsp_in, pull, ch), pull.take)
            }
            None => {
                // The stage retired inside the period: the rest is raw.
                let pull = Pull {
                    take: n_k.min(ring_fill(bufs.ring, ch)),
                    pad: n_k - n_k.min(ring_fill(bufs.ring, ch)),
                    underfeed: false,
                };
                run.underfeed |= pull.pad > 0 && plan.kind == PullKind::Play;
                (take(bufs.ring, bufs.dsp_in, pull, ch), pull.take)
            }
        };
        run.pulled += len / ch;
        run.taken += took;
        delay.took(n_k);
        if plan.poll {
            poll_trigger(
                vs,
                graph,
                bufs.ring,
                ch,
                plan.graph_rate,
                plan.quantum,
                delay,
                speed,
            );
        }
        let direct = &bufs.direct_in[..len.min(bufs.direct_in.len())];
        processor.process_block(
            plan.sink,
            &bufs.dsp_in[..len],
            direct,
            &mut bufs.out[off_out * ch..(off_out + out_k) * ch],
            ch,
            plan.ratio,
            plan.graph_rate,
            pos,
        );
        graph.checkpoint();
        if plan.poll {
            run.wake_at_ns = poll_trigger(
                vs,
                graph,
                bufs.ring,
                ch,
                plan.graph_rate,
                plan.quantum,
                delay,
                speed,
            );
        }
        off_in += n_k;
        off_out += out_k;
        k += 1;
    }
    run.calls = k;
    debug_assert_eq!(off_out, plan.n_out, "the calls' outputs fill the period");
    Some(run)
}

/// Take a pull from the ring: exactly `take` frames, then `pad` zero
/// frames. Returns the samples in the block.
pub fn take(ring: &mut rtrb::Consumer<f32>, dst: &mut [f32], pull: Pull, channels: usize) -> usize {
    let want = pull.take * channels;
    let len = (pull.take + pull.pad) * channels;
    let n = pop_zero_pad(ring, &mut dst[..want]);
    dst[n..len].fill(0.0);
    len
}

fn pop_zero_pad(ring: &mut rtrb::Consumer<f32>, dst: &mut [f32]) -> usize {
    let n = dst.len().min(ring.slots());
    if n > 0
        && let Ok(chunk) = ring.read_chunk(n)
    {
        let (a, b) = chunk.as_slices();
        dst[..a.len()].copy_from_slice(a);
        dst[a.len()..a.len() + b.len()].copy_from_slice(b);
        chunk.commit_all();
    }
    dst[n..].fill(0.0);
    n
}

/// The varispeed side of the tail drain: pull until the ring is empty,
/// then the stage's whole tail ([`VarispeedPort::tail_frames`]) as padding.
#[derive(Clone, Copy, Debug, Default)]
pub struct VsDrain {
    flush_left: Option<usize>,
}

impl VsDrain {
    /// Whether another period must be pulled, with `ring_fill` frames left
    /// and a tail of `tail` frames.
    pub fn more(&mut self, ring_fill: usize, tail: usize) -> bool {
        if ring_fill > 0 && self.flush_left.is_none() {
            return true;
        }
        *self.flush_left.get_or_insert(tail) > 0
    }

    /// A period of `n` nominal frames was pulled.
    pub fn pulled(&mut self, n: usize) {
        if let Some(left) = self.flush_left.as_mut() {
            *left = left.saturating_sub(n);
        }
    }
}

/// Least time a tail drain gets, ns.
pub const DRAIN_MIN_NS: u64 = 250_000_000;

/// How long the tail drain may take: the ring's `queued` source frames at
/// `speed`, the `tail` frames of padding, the device's `delay_out` output
/// frames, and two periods to spare, at `graph_rate` through `ratio`; at
/// least [`DRAIN_MIN_NS`].
pub fn drain_deadline_ns(
    queued: usize,
    speed: f64,
    tail: usize,
    delay_out: i64,
    ratio: RateRatio,
    graph_rate: u32,
    period_out: u32,
) -> u64 {
    let s = if speed.is_finite() && speed > 0.0 {
        speed
    } else {
        1.0
    };
    let g = f64::from(graph_rate.max(1));
    let out_rate = f64::from(ratio.output_rate(graph_rate).unwrap_or(graph_rate).max(1));
    let secs = (queued as f64 / s + tail as f64) / g
        + (delay_out.max(0) as f64 + 2.0 * f64::from(period_out)) / out_rate;
    ((secs * 1e9) as u64).max(DRAIN_MIN_NS)
}

/// An offline harness for the steps above (tests only; not used by the
/// driver): a simulated graph whose cycles complete a fixed time after
/// their trigger and push a quantum of source into the DSP ring, and a
/// simulated DAC that wakes once a period.
#[doc(hidden)]
pub mod sim {
    use super::*;
    use std::collections::VecDeque;

    /// One trigger as the graph saw it.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Trigger {
        pub at_ns: u64,
        pub delay: i64,
        pub speed: f64,
        /// The ring's fill then, frames.
        pub fill: usize,
        pub quantum: u64,
    }

    /// The simulated graph.
    pub struct SimGraph {
        pub now: u64,
        /// Trigger to completion, ns.
        pub latency_ns: u64,
        pub channels: usize,
        prod: rtrb::Producer<f32>,
        pending: VecDeque<(u64, u64)>,
        pub completed: u64,
        pub triggers: Vec<Trigger>,
        /// Source frames delivered so far.
        pub produced: u64,
        /// The source ends after this many frames (cycles then deliver
        /// nothing).
        pub source_len: Option<u64>,
        /// Sample `ch` of source frame `i`.
        pub source: Box<dyn FnMut(u64, usize) -> f32 + Send>,
        /// [`VsGraph::checkpoint`] calls.
        pub checkpoints: u64,
    }

    impl SimGraph {
        /// A graph feeding `prod`, cycles taking `latency_ns`, the source a
        /// ramp of frame indices (`i / 2^20`, so every frame is distinct).
        pub fn new(prod: rtrb::Producer<f32>, channels: usize, latency_ns: u64) -> Self {
            Self {
                now: 1_000_000_000,
                latency_ns,
                channels,
                prod,
                pending: VecDeque::new(),
                completed: 0,
                triggers: Vec::new(),
                produced: 0,
                source_len: None,
                source: Box::new(|i, _| (i % (1 << 20)) as f32 / (1 << 20) as f32),
                checkpoints: 0,
            }
        }

        /// The ring's fill, frames.
        pub fn fill(&self) -> usize {
            (self.prod.buffer().capacity() - self.prod.slots()) / self.channels.max(1)
        }

        /// The next completion's time.
        pub fn next_completion(&self) -> Option<u64> {
            self.pending.front().map(|p| p.0)
        }

        /// Move the clock to `to`, completing the cycles due by then.
        pub fn advance(&mut self, to: u64) {
            while let Some(&(at, q)) = self.pending.front() {
                if at > to {
                    break;
                }
                self.now = self.now.max(at);
                self.pending.pop_front();
                let mut n = q;
                if let Some(len) = self.source_len {
                    n = n.min(len.saturating_sub(self.produced));
                }
                for _ in 0..n {
                    if self.prod.slots() < self.channels {
                        break;
                    }
                    for c in 0..self.channels {
                        let x = (self.source)(self.produced, c);
                        let _ = self.prod.push(x);
                    }
                    self.produced += 1;
                }
                self.completed += 1;
            }
            self.now = self.now.max(to);
        }
    }

    impl VsGraph for SimGraph {
        fn now_ns(&self) -> u64 {
            self.now
        }

        fn completed(&self) -> u64 {
            self.completed
        }

        fn trigger(&mut self, _rate: u32, quantum: u64, delay: i64, speed: f64) {
            self.triggers.push(Trigger {
                at_ns: self.now,
                delay,
                speed,
                fill: self.fill(),
                quantum,
            });
            let at = self.now + self.latency_ns;
            self.pending.push_back((at, quantum));
        }

        fn wait_until(&mut self, until_ns: u64) {
            let t = self
                .next_completion()
                .map_or(until_ns, |c| c.min(until_ns))
                .max(self.now);
            self.advance(t);
        }

        fn checkpoint(&mut self) {
            self.checkpoints += 1;
        }
    }

    /// Between two DAC wakeups: what the RT loop's `ppoll` does, polling the
    /// schedule on each completion and at its wake time, until `until_ns`.
    #[allow(clippy::too_many_arguments)]
    pub fn idle_until(
        vs: &mut VarispeedDriver,
        graph: &mut SimGraph,
        ring: &rtrb::Consumer<f32>,
        channels: usize,
        rate: u32,
        quantum: u64,
        delay: &VsDelay,
        speed: f64,
        wake: &mut Option<u64>,
        until_ns: u64,
    ) {
        loop {
            let next = [Some(until_ns), *wake, graph.next_completion()]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(until_ns)
                .max(graph.now);
            graph.advance(next.min(until_ns));
            if graph.now >= until_ns {
                return;
            }
            *wake = poll_trigger(vs, graph, ring, channels, rate, quantum, delay, speed)
                .map(|w| w.max(graph.now + 50_000));
        }
    }
}

#[cfg(test)]
#[path = "varispeed_loop_tests.rs"]
mod tests;
