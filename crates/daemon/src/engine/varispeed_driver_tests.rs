//! The driver's varispeed steps (`audivane_io_pw::varispeed_loop`: the
//! priming, the hand-back, the period split, the drain) around the real
//! processor and stage, on a simulated graph and DAC: no PipeWire, the same
//! code the RT loop runs.

use super::{VarispeedAppliedSnapshot, VarispeedRtCmd};
use crate::engine::processor::{ActiveChain, DaemonProcessor, MeterBank, PendingSwap, RtDspCmd};
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_core::tempo::{TempoSpec, TempoStretch};
use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
use audivane_io_pw::EngineAudioProcessor;
use audivane_io_pw::varispeed_loop::sim::{SimGraph, idle_until};
use audivane_io_pw::varispeed_loop::{
    self as vsl, Handover, PeriodBufs, PeriodPlan, VsDelay, VsDrain,
};
use audivane_io_pw::varispeed_rt::{VarispeedCommandSlot, VarispeedDriver};
use audivane_io_pw::varispeed_sched::{PullKind, RING_ALLOC_FRAMES, Timing};
use audivane_protocol::ipc::SinkChoice;
use audivane_protocol::{ChainConfig, DitherKind, EqProfile, OutputConfig, RateRatio};
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

const AMP: f64 = 0.5;
const FREQ: f64 = 1000.0;
const CH: usize = 2;

fn eq_config() -> ChainConfig {
    ChainConfig {
        eq: Some(EqProfile {
            name: "eq".into(),
            preamp_db: -1.0,
            ..Default::default()
        }),
        output: OutputConfig {
            bits: 32,
            dither: DitherKind::None,
            headroom_db: 0.0,
        },
        ..ChainConfig::neutral()
    }
}

fn chain(config: ChainConfig, rate: u32) -> ActiveChain {
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config,
        sample_rate: rate,
        channels: CH,
        max_block: 16_384,
        irs: None,
    })))
}

fn stage(rate: u32) -> Box<Varispeed> {
    Box::new(
        Varispeed::new(VarispeedSpec {
            sample_rate: rate,
            channels: CH,
            max_frames: audivane_dsp_core::varispeed::MAX_FRAMES,
            speed: 1.0,
        })
        .unwrap(),
    )
}

fn tempo_stage(rate: u32) -> Box<TempoStretch> {
    Box::new(
        TempoStretch::new(TempoSpec {
            sample_rate: rate,
            channels: CH,
            max_frames: audivane_dsp_core::tempo::MAX_FRAMES,
            speed: 1.0,
        })
        .unwrap(),
    )
}

/// The driver around the processor: a simulated graph feeding the DSP
/// ring with a 1 kHz tone, a DAC waking once a period with two periods
/// queued, and between wakeups the RT loop's `ppoll` polling (with the
/// pacer's one cycle per quantum after a hand-back).
struct Drv {
    rate: u32,
    ratio: RateRatio,
    proc: DaemonProcessor,
    cmd_tx: rtrb::Producer<RtDspCmd>,
    #[allow(dead_code)] // held so the other end stays connected
    swap_tx: rtrb::Producer<PendingSwap>,
    vs_tx: rtrb::Producer<VarispeedRtCmd>,
    _vs_recycle: rtrb::Consumer<Box<Varispeed>>,
    #[allow(dead_code)] // held so the other end stays connected
    stretch_recycle: rtrb::Consumer<Box<TempoStretch>>,
    recycle: rtrb::Consumer<ActiveChain>,
    vs: VarispeedDriver,
    graph: SimGraph,
    ring: rtrb::Consumer<f32>,
    slot: VarispeedCommandSlot,
    delay: VsDelay,
    dsp_in: Vec<f32>,
    direct_in: Vec<f32>,
    out: Vec<i32>,
    /// Left channel of every period, normalised.
    left: Vec<f64>,
    pos: u64,
    q: u64,
    wake: Option<u64>,
    /// Zero frames the raw path padded (an empty ring under a raw pull).
    raw_padded: usize,
    /// `(period, calls)` of every period the stage ran.
    calls: Vec<(usize, usize)>,
    periods: usize,
}

impl Drv {
    fn new(rate: u32, ratio: RateRatio, chain: ActiveChain, q: u64) -> Self {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (swap_tx, swap_rx) = rtrb::RingBuffer::<PendingSwap>::new(8);
        let (recycle_tx, recycle) = rtrb::RingBuffer::new(8);
        let (vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
        let (vs_recycle_tx, vs_recycle) = rtrb::RingBuffer::new(8);
        let (stretch_recycle_tx, stretch_recycle) = rtrb::RingBuffer::new(8);
        let proc = DaemonProcessor::new(
            chain,
            true,
            CH,
            16_384,
            16,
            rate,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(CH),
            cmd_rx,
            swap_rx,
            recycle_tx,
        )
        .with_varispeed(vs_rx, vs_recycle_tx, CH)
        .with_stretch_recycle(stretch_recycle_tx);
        let (prod, ring) = rtrb::RingBuffer::new(RING_ALLOC_FRAMES * CH);
        let mut graph = SimGraph::new(prod, CH, 400_000);
        let r = rate;
        graph.source = Box::new(move |i, c| {
            let x = AMP * (2.0 * std::f64::consts::PI * FREQ * i as f64 / f64::from(r)).sin();
            (if c == 0 { x } else { -0.5 * x }) as f32
        });
        Self {
            rate,
            ratio,
            proc,
            cmd_tx,
            swap_tx,
            vs_tx,
            _vs_recycle: vs_recycle,
            stretch_recycle,
            recycle,
            vs: VarispeedDriver::new(),
            graph,
            ring,
            slot: VarispeedCommandSlot::default(),
            delay: VsDelay::new(ratio, rate),
            dsp_in: vec![0.0; (2 * 16_384 + 2) * CH],
            direct_in: vec![0.0; (2 * 16_384 + 2) * CH],
            out: vec![0; 16_384 * CH],
            left: Vec::new(),
            pos: 0,
            q,
            wake: None,
            raw_padded: 0,
            calls: Vec::new(),
            periods: 0,
        }
    }

    fn applied(&self) -> VarispeedAppliedSnapshot {
        self.proc.varispeed_applied().snapshot()
    }

    fn install(&mut self) {
        assert!(
            self.vs_tx
                .push(VarispeedRtCmd::Install(stage(self.rate)))
                .is_ok()
        );
    }

    /// Keep pitch: the stretch in varispeed's place.
    fn install_stretch(&mut self) {
        assert!(
            self.vs_tx
                .push(VarispeedRtCmd::InstallStretch(tempo_stage(self.rate)))
                .is_ok()
        );
    }

    /// Source frames the stage has taken from the ring so far.
    fn consumed(&self) -> u64 {
        self.graph.produced - vsl::ring_fill(&self.ring, CH) as u64
    }

    fn period_frames(&self, n_out: usize) -> (usize, usize) {
        let n_in = self.ratio.frames_in_for(self.pos, n_out);
        (n_in, self.ratio.frames_out(self.pos, n_in))
    }

    fn period_ns(&self, n_in: usize) -> u64 {
        n_in as u64 * 1_000_000_000 / u64::from(self.rate)
    }

    /// Prime: the schedule starts (the latest command first), `periods`
    /// prime periods through the stage, then the cushion to the watermark.
    fn prime(&mut self, n_out: usize, periods: usize) {
        self.proc
            .poll_updates(SinkChoice::Dsp, self.rate, self.ratio, false);
        let (n_in, _) = self.period_frames(n_out);
        let p = vsl_period(self.ratio, n_out, self.pos);
        let port = self
            .proc
            .varispeed(SinkChoice::Dsp, self.ratio, self.rate, CH)
            .expect("a stage");
        self.vs
            .start_priming(Timing::new(self.rate, p, self.q as u32), port, &self.slot);
        self.delay = VsDelay::new(self.ratio, self.rate);
        self.delay.dac_read(self.graph.now, 0, false);
        for _ in 0..periods {
            let (n_in, n_out) = self.period_frames(n_out);
            let run = self.run(n_in, n_out, PullKind::Play, Some(80_000_000));
            assert!(run.is_some(), "the prime found its source");
            self.delay.wrote(n_out, n_in);
        }
        let wm = self.vs.scheduler().watermark();
        let speed = self.speed();
        vsl::fill_until(
            &mut self.vs,
            &mut self.graph,
            &self.ring,
            CH,
            self.rate,
            self.q,
            &self.delay,
            speed,
            wm,
            500_000_000,
        );
        self.vs.finish_priming();
        let _ = n_in;
    }

    fn speed(&mut self) -> f64 {
        self.proc
            .varispeed(SinkChoice::Dsp, self.ratio, self.rate, CH)
            .map_or(1.0, |p| {
                audivane_io_pw::varispeed_rt::VarispeedPort::speed(p)
            })
    }

    fn run(
        &mut self,
        n_in: usize,
        n_out: usize,
        kind: PullKind,
        prime_wait_ns: Option<u64>,
    ) -> Option<vsl::PeriodRun> {
        let run = vsl::run_period(
            &mut self.vs,
            &mut self.graph,
            &mut self.proc,
            PeriodBufs {
                ring: &mut self.ring,
                dsp_in: &mut self.dsp_in,
                direct_in: &self.direct_in,
                out: &mut self.out[..n_out * CH],
            },
            &self.slot,
            PeriodPlan {
                sink: SinkChoice::Dsp,
                ratio: self.ratio,
                graph_rate: self.rate,
                channels: CH,
                pos: self.pos,
                n_in,
                n_out,
                kind,
                prime_wait_ns,
                poll: prime_wait_ns.is_none() && kind == PullKind::Play,
                quantum: self.q,
            },
            &mut self.delay,
        );
        if let Some(r) = run {
            self.calls.push((self.periods, r.calls));
            self.record(n_out);
            self.pos += n_in as u64;
            if let Some(w) = r.wake_at_ns {
                self.wake = Some(w);
            }
        }
        while self.recycle.pop().is_ok() {}
        run
    }

    fn record(&mut self, n_out: usize) {
        self.left.extend(
            self.out[..n_out * CH]
                .iter()
                .step_by(CH)
                .map(|&s| f64::from(s) / 2_147_483_648.0),
        );
    }

    /// One DAC wakeup and the idle time after it, as the RT loop does them.
    fn period(&mut self, n_out: usize) {
        self.proc
            .poll_updates(SinkChoice::Dsp, self.rate, self.ratio, true);
        let (n_in, n_out) = self.period_frames(n_out);
        let t = self.graph.now;
        self.delay.dac_read(t, 2 * n_out as i64, true);
        self.delay.clear_pending();
        let p = vsl_period(self.ratio, n_out, self.pos);
        let wait_ns = self.period_ns(n_in) / 2;
        let port = self
            .proc
            .varispeed(SinkChoice::Dsp, self.ratio, self.rate, CH);
        let h = vsl::handover(
            &mut self.vs,
            &mut self.graph,
            &self.ring,
            port,
            CH,
            self.rate,
            p,
            self.q as u32,
            n_in,
            &self.delay,
            wait_ns,
        );
        let mut raw = !self.vs.is_active();
        if !raw && self.run(n_in, n_out, PullKind::Play, None).is_none() {
            raw = true;
        }
        if raw {
            // The raw path: the planner's frames from the ring, zero-padded
            // when short.
            let have = vsl::ring_fill(&self.ring, CH).min(n_in);
            let pull = audivane_io_pw::varispeed_rt::Pull {
                take: have,
                pad: n_in - have,
                underfeed: false,
            };
            self.raw_padded += pull.pad;
            let len = vsl::take(&mut self.ring, &mut self.dsp_in, pull, CH);
            self.proc.process_block(
                SinkChoice::Dsp,
                &self.dsp_in[..len],
                &self.direct_in[..len],
                &mut self.out[..n_out * CH],
                CH,
                self.ratio,
                self.rate,
                self.pos,
            );
            self.record(n_out);
            self.pos += n_in as u64;
            if matches!(h, Handover::HandedBack { .. }) {
                self.wake = None;
            }
        }
        self.delay.wrote(n_out, n_in);
        let until = t + self.period_ns(n_in);
        if self.vs.is_active() {
            let speed = self.speed();
            idle_until(
                &mut self.vs,
                &mut self.graph,
                &self.ring,
                CH,
                self.rate,
                self.q,
                &self.delay,
                speed,
                &mut self.wake,
                until,
            );
        } else {
            // The pacer: one cycle per quantum consumed.
            for _ in 0..(n_in as u64).div_ceil(self.q) {
                audivane_io_pw::varispeed_loop::VsGraph::trigger(
                    &mut self.graph,
                    self.rate,
                    self.q,
                    0,
                    1.0,
                );
            }
            self.graph.advance(until);
        }
        self.periods += 1;
    }
}

/// The scheduler's period `P` for a period of `n_out` output frames (as the
/// driver's `vs_period_frames`).
fn vsl_period(ratio: RateRatio, n_out: usize, _pos: u64) -> u32 {
    if ratio.is_integer() {
        (n_out as u32 / ratio.up).max(1)
    } else {
        (n_out as u64 * u64::from(ratio.down)).div_ceil(u64::from(ratio.up)) as u32 + 1
    }
}

/// The largest second difference in `x`: a steady tone's is its
/// curvature; a splice (a jump in phase or slope) stands out even where
/// the step itself happens to be small.
fn max_bend(x: &[f64]) -> f64 {
    x.windows(3)
        .map(|w| (w[2] - 2.0 * w[1] + w[0]).abs())
        .fold(0.0, f64::max)
}

/// The bend of a steady full-level tone at `rate`, with room for the 5 ms
/// fades.
fn bend_bound(rate: u32) -> f64 {
    let w = 2.0 * std::f64::consts::PI * FREQ / f64::from(rate);
    AMP * w * w * 1.1 + 0.004
}

fn max_step(x: &[f64]) -> f64 {
    x.windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f64::max)
}

/// A steady tone at speed `s` through the -1 dB EQ moves at most this much
/// per sample at `rate`; fades add a little.
fn step_bound(s: f64, rate: u32) -> f64 {
    let a = AMP * 10f64.powf(-1.0 / 20.0);
    a * 2.0 * std::f64::consts::PI * FREQ * s / f64::from(rate) * 1.1 + 0.005
}

/// The longest run of (near) silence in `x`.
fn longest_quiet(x: &[f64]) -> usize {
    let (mut best, mut cur) = (0, 0);
    for v in x {
        if v.abs() < 1e-4 {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

#[test]
fn a_refused_plan_is_counted_not_played_raw() {
    // The guard behind the split: a plan longer than one call plays
    // silence, counted, never the raw block under a reported speed.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 1024);
    d.install();
    d.prime(1024, 2);
    d.period(1024);
    let n = audivane_dsp_core::varispeed::MAX_FRAMES + 2;
    let port = d
        .proc
        .varispeed(SinkChoice::Dsp, RateRatio::ONE, 48_000, CH)
        .unwrap();
    assert_eq!(port.plan(n), n);
    let input = vec![0.25_f32; n * CH];
    let mut out = vec![0_i32; n * CH];
    d.proc.process_block(
        SinkChoice::Dsp,
        &input,
        &input,
        &mut out,
        CH,
        RateRatio::ONE,
        48_000,
        d.pos,
    );
    assert!(out.iter().all(|&x| x == 0));
    assert_eq!(d.applied().refused_plans, 1);
    assert!(!d.applied().active);
}

#[test]
fn dsp_off_at_unity_fades_from_the_stage_to_the_raw_block() {
    // At x1, DSP off switches to the bit-perfect path. The stage's output
    // is ~270 frames late: straight to the raw block would jump. The toggle
    // waits for the stage's fade-out; the raw block fades in.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install();
    d.prime(1024, 2);
    for _ in 0..20 {
        d.period(1024);
    }
    assert!(d.applied().active);
    assert!(d.cmd_tx.push(RtDspCmd::SetDspEnabled(false)).is_ok());
    let start = d.left.len() - 64;
    for _ in 0..10 {
        d.period(1024);
    }
    assert_eq!(
        d.proc.mode(SinkChoice::Dsp, RateRatio::ONE),
        crate::engine::processor::PlayMode::Bypass
    );
    assert!(!d.applied().active);
    assert!(d.applied().enabled, "varispeed stays enabled");
    let tail = &d.left[start..];
    // The raw path skips the -1 dB EQ: the bound at full level.
    let steps = max_step(tail);
    let raw_bound = AMP * 2.0 * std::f64::consts::PI * FREQ / 48_000.0 * 1.1 + 0.005;
    assert!(steps < raw_bound, "{steps}");
    let bend = max_bend(tail);
    assert!(bend < bend_bound(48_000), "a splice: {bend}");
    assert_eq!(d.raw_padded, 0);
    // DSP on again: the raw block fades out under the stage's fade-in.
    assert!(d.cmd_tx.push(RtDspCmd::SetDspEnabled(true)).is_ok());
    let start = d.left.len() - 64;
    for _ in 0..10 {
        d.period(1024);
    }
    assert!(d.applied().active);
    let tail = &d.left[start..];
    let steps = max_step(tail);
    assert!(steps < raw_bound, "{steps}");
    // The toggle itself switches from 0 dB to the -1 dB EQ at once (as it
    // does without varispeed): that level step, and no more. A cut from
    // the raw block to the stage's silent start would bend by ~0.4.
    let level_step = AMP * (1.0 - 10f64.powf(-1.0 / 20.0));
    let bend = max_bend(tail);
    assert!(bend < level_step + bend_bound(48_000), "a splice: {bend}");
}

#[test]
fn the_drain_plays_the_whole_tail() {
    // The source ends; the drain pulls the ring empty, then the stage's
    // whole tail (not just its group delay) as padding.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 1024);
    d.install();
    d.prime(1024, 2);
    for _ in 0..8 {
        d.period(1024);
    }
    // Stop the source where it is; let the cycles in flight land.
    d.graph.source_len = Some(d.graph.produced);
    d.graph.advance(d.graph.now + 50_000_000);
    let (latency, tail) = {
        let port = d
            .proc
            .varispeed(SinkChoice::Dsp, RateRatio::ONE, 48_000, CH)
            .unwrap();
        (port.latency_frames(), port.tail_frames())
    };
    assert!(tail as f64 > latency + 100.0, "{tail} vs {latency}");
    let start = d.left.len() - 64;
    let mut drain = VsDrain::default();
    while drain.more(vsl::ring_fill(&d.ring, CH), tail) {
        assert!(d.run(1024, 1024, PullKind::Drain, None).is_some());
        drain.pulled(1024);
    }
    let out = &d.left[start..];
    // The stage's output ends inside what was drained: the last sample is
    // silent, and the ring's frames all played.
    assert!(out.last().unwrap().abs() < 1e-9);
    assert_eq!(vsl::ring_fill(&d.ring, CH), 0);
    let last = out.iter().rposition(|x| x.abs() > 1e-9).unwrap();
    assert!(last + 16 < out.len(), "{last} of {}", out.len());
    // A flush of only the delay would have stopped before the response did.
    let ring_out = out.len() - drain_tail_periods(tail);
    assert!(
        last > ring_out + latency.ceil() as usize,
        "{last} {ring_out}"
    );
}

/// Periods the tail took (1 024 frames each).
fn drain_tail_periods(tail: usize) -> usize {
    tail.div_ceil(1024) * 1024
}

#[test]
fn an_empty_pull_after_a_full_one_fades_out() {
    // A full call, then one whose ring is empty (valid = 0) at the very
    // start: the source fades out over the frames the slot held back, not
    // a cut from the tone to zero.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 1024);
    d.install();
    d.prime(1024, 2);
    for _ in 0..6 {
        d.period(1024);
    }
    // Empty the ring and stop the source, then play one period.
    let n = vsl::ring_fill(&d.ring, CH);
    let mut sink = vec![0.0; n * CH];
    vsl::take(
        &mut d.ring,
        &mut sink,
        audivane_io_pw::varispeed_rt::Pull {
            take: n,
            pad: 0,
            underfeed: false,
        },
        CH,
    );
    d.graph.source_len = Some(d.graph.produced);
    let start = d.left.len().saturating_sub(64);
    let run = d.run(1024, 1024, PullKind::Play, None).unwrap();
    assert!(run.underfeed && run.taken == 0);
    assert_eq!(d.applied().gap_fades, 1);
    let steps = max_step(&d.left[start..]);
    assert!(steps < step_bound(1.0, 48_000), "{steps}");
}

// A driver already playing a period or two, for the cases below.

// Keep pitch: the stretch in the slot, through the driver.

/// The tone's frequency in `x` from its rising zero crossings (linearly
/// interpolated), Hz.
fn tone_freq(x: &[f64], rate: u32) -> f64 {
    let mut first = None;
    let mut last = 0.0;
    let mut count = 0usize;
    for (i, w) in x.windows(2).enumerate() {
        if w[0] < 0.0 && w[1] >= 0.0 {
            let t = i as f64 + w[0] / (w[0] - w[1]);
            first.get_or_insert(t);
            last = t;
            count += 1;
        }
    }
    let first = first.expect("crossings");
    (count - 1) as f64 / (last - first) * f64::from(rate)
}

#[test]
fn keep_pitch_plays_the_speed_at_the_source_pitch() {
    // The stretch at 1.25 through the driver: it takes 1.25 source frames
    // per output frame, the tone keeps its 1 kHz, nothing is refused and
    // nothing drops out once its delay has passed.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 1024);
    d.install_stretch();
    d.slot.send(1.25, 0.0);
    d.prime(1024, 2);
    let (c0, o0) = (d.consumed(), d.left.len());
    for _ in 0..80 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(a.active && a.stretch, "{a:?}");
    assert_eq!(a.speed, 1.25);
    assert_eq!(a.refused_plans, 0);
    let rate = (d.consumed() - c0) as f64 / (d.left.len() - o0) as f64;
    assert!((rate - 1.25).abs() < 0.01, "source per output {rate}");
    let tail = &d.left[d.left.len() - 48_000..];
    assert!(longest_quiet(tail) < 4, "{}", longest_quiet(tail));
    let f = tone_freq(tail, 48_000);
    assert!((f - FREQ).abs() < 0.2, "pitch moved: {f} Hz");
    // The delay reported is the stretch's at 1.25 and the look-ahead.
    assert!((5_000.0..7_000.0).contains(&a.latency_frames), "{a:?}");
}
