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
    swap_tx: rtrb::Producer<PendingSwap>,
    vs_tx: rtrb::Producer<VarispeedRtCmd>,
    _vs_recycle: rtrb::Consumer<Box<Varispeed>>,
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
fn an_oversized_rational_period_plays_through_the_stage() {
    // 192 → 44.1 kHz at a quantum of 8192: periods of 1 882 output frames
    // take 8 190 and 8 194 graph frames; the stage takes at most 8 192 a
    // call. Before, the second period's plan failed and the block played
    // raw while the driver reported varispeed on.
    let ratio = RateRatio::from_rates(192_000, 44_100);
    let mut c = eq_config();
    c.upsampler = audivane_dsp_core::resample::ManualConfig {
        target: audivane_protocol::RateTarget::Rate(44_100),
        taps: 32,
        taps_unit: audivane_protocol::TapsUnit::Input,
        cutoff: 0.95,
        ..Default::default()
    }
    .into_upsampler_config()
    .unwrap();
    let mut d = Drv::new(192_000, ratio, chain(c, 192_000), 8192);
    d.install();
    d.slot.send(1.25, 0.0);
    d.prime(1_882, 2);
    for _ in 0..12 {
        d.period(1_882);
    }
    let a = d.applied();
    assert!(a.active, "{a:?}");
    assert_eq!(a.refused_plans, 0);
    assert_eq!(a.speed, 1.25);
    assert!(
        d.calls.iter().any(|&(_, c)| c == 2),
        "an 8 194-frame period runs as two calls: {:?}",
        d.calls
    );
    assert!(d.calls.iter().all(|&(_, c)| c <= 2));
    // Steady output after the stage's delay: no gap.
    let settled = &d.left[4 * 1_882..];
    assert!(longest_quiet(settled) < 4, "{}", longest_quiet(settled));
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
fn a_disable_at_half_speed_refills_before_the_raw_block() {
    // P = 1024, Q = 128, s = 0.5: the watermark is 643 frames, short of
    // the 1 024 the first raw block takes. The disable waits for the
    // driver to size the next pull for unity, the stage's last call ends at
    // its fade, and the raw block plays the rest of that period.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 128);
    d.install();
    d.slot.send(0.5, 0.0);
    d.prime(1024, 2);
    for _ in 0..30 {
        d.period(1024);
    }
    assert_eq!(d.applied().speed, 0.5);
    assert!(d.vs.scheduler().watermark() < 1024);
    assert!(d.vs_tx.push(VarispeedRtCmd::Disable).is_ok());
    let start = d.left.len() - 64;
    for _ in 0..12 {
        d.period(1024);
    }
    assert_eq!(d.raw_padded, 0, "the raw path never ran short");
    assert!(!d.vs.is_active());
    let a = d.applied();
    assert!(!a.enabled && !a.active);
    assert_eq!(a.retired, 1);
    // The stage's last period: a call to the fade's end, then raw.
    assert!(d.calls.iter().any(|&(_, c)| c >= 2), "{:?}", d.calls);
    // No click, and no silence longer than the swap from the stage's
    // delayed output to the raw block.
    let tail = &d.left[start..];
    let steps = max_step(tail);
    assert!(steps < step_bound(1.0, 48_000), "{steps}");
    assert!(longest_quiet(tail) < 8, "{}", longest_quiet(tail));
}

#[test]
fn an_enable_while_idle_primes_at_the_commanded_speed() {
    // The stage installs while nothing plays; a command for 1.5 is posted
    // before the device starts. The very first prime block plays at 1.5:
    // before, the command was read after the first plan, so priming began
    // at unity and ramped.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 1024);
    d.install();
    d.slot.send(1.5, 200.0);
    d.proc
        .poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, false);
    let p = 1024;
    let port = d
        .proc
        .varispeed(SinkChoice::Dsp, RateRatio::ONE, 48_000, CH)
        .unwrap();
    d.vs.start_priming(Timing::new(48_000, p, 1024), port, &d.slot);
    d.delay.dac_read(d.graph.now, 0, false);
    let run = d.run(1024, 1024, PullKind::Play, Some(80_000_000)).unwrap();
    let a = d.applied();
    assert_eq!(a.speed, 1.5, "{a:?}");
    assert!(!a.ramping);
    assert!((1535..=1537).contains(&run.pulled), "{}", run.pulled);
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
fn dsp_toggles_with_the_chains_apply_sends_fade_the_stage() {
    // What an edit sends at x1: DSP off is the toggle and a passthrough
    // chain (`ActiveChain::Bypass`, which has no place for the stage); DSP
    // on is the toggle and the DSP chain, both crossfading. The passthrough
    // waits for the stage's fade-out (installed at once it would cut the
    // stage mid-signal and strand the toggle), and DSP on comes back
    // through the stage with no gap.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install();
    d.prime(1024, 2);
    for _ in 0..20 {
        d.period(1024);
    }
    assert!(d.applied().active);
    assert!(d.cmd_tx.push(RtDspCmd::SetDspEnabled(false)).is_ok());
    let swap = |chain, swap_seq| PendingSwap {
        chain,
        crossfade: true,
        generation: swap_seq,
        swap_seq,
    };
    assert!(d.swap_tx.push(swap(ActiveChain::Bypass, 1)).is_ok());
    let start = d.left.len() - 64;
    for _ in 0..10 {
        d.period(1024);
    }
    assert_eq!(
        d.proc.mode(SinkChoice::Dsp, RateRatio::ONE),
        crate::engine::processor::PlayMode::Bypass
    );
    assert!(!d.applied().active);
    let tail = &d.left[start..];
    let raw_bound = AMP * 2.0 * std::f64::consts::PI * FREQ / 48_000.0 * 1.1 + 0.005;
    let steps = max_step(tail);
    assert!(steps < raw_bound, "{steps}");
    let bend = max_bend(tail);
    assert!(bend < bend_bound(48_000), "a splice: {bend}");
    assert_eq!(d.raw_padded, 0);

    assert!(d.cmd_tx.push(RtDspCmd::SetDspEnabled(true)).is_ok());
    assert!(d.swap_tx.push(swap(chain(eq_config(), 48_000), 2)).is_ok());
    let start = d.left.len() - 64;
    for _ in 0..20 {
        d.period(1024);
    }
    assert!(d.applied().active, "{:?}", d.applied());
    let tail = &d.left[start..];
    let steps = max_step(tail);
    assert!(steps < raw_bound, "{steps}");
    let quiet = longest_quiet(tail);
    assert!(quiet < 8, "a gap of {quiet} frames");
}

#[test]
fn a_chain_swap_never_eats_the_raw_fade_in() {
    // A disable while a chain swap fades: the raw block still fades in
    // (before, the swap's phase kept the processor from starting it).
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install();
    d.prime(1024, 2);
    for _ in 0..20 {
        d.period(1024);
    }
    // The disable waits a period for the driver; the swap lands in the
    // period the stage fades out in, so its crossfade still runs when the
    // stage stops.
    assert!(d.vs_tx.push(VarispeedRtCmd::Disable).is_ok());
    let start = d.left.len() - 64;
    d.period(1024);
    let mut c = eq_config();
    c.eq.as_mut().unwrap().preamp_db = -1.5;
    assert!(
        d.swap_tx
            .push(PendingSwap {
                chain: chain(c, 48_000),
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .is_ok()
    );
    for _ in 0..10 {
        d.period(1024);
    }
    assert_eq!(d.applied().retired, 1);
    let steps = max_step(&d.left[start..]);
    assert!(steps < step_bound(1.0, 48_000), "{steps}");
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
fn a_reset_publishes_the_stopped_state() {
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 1024);
    d.install();
    d.slot.send(1.25, 0.0);
    d.prime(1024, 2);
    for _ in 0..4 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(a.active && a.speed == 1.25, "{a:?}");
    // The idle stop resets the DSP state: the status says so at once, with
    // no block processed after it.
    d.proc.reset_dsp_state();
    let a = d.applied();
    assert!(!a.active, "{a:?}");
    assert_eq!(a.speed, 1.0);
    assert_eq!(a.step_bits, 1u128 << 64);
    assert_eq!(a.latency_frames, 0.0);
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
fn running() -> Drv {
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install();
    d.prime(1024, 2);
    for _ in 0..5 {
        d.period(1024);
    }
    d
}

fn queue_toggle(d: &mut Drv, enabled: bool, seq: u64) {
    d.cmd_tx
        .push(RtDspCmd::SetDspEnabled(enabled))
        .ok()
        .unwrap();
    d.swap_tx
        .push(PendingSwap {
            chain: if enabled {
                chain(eq_config(), 48_000)
            } else {
                ActiveChain::Bypass
            },
            crossfade: true,
            generation: seq,
            swap_seq: seq,
        })
        .ok()
        .unwrap();
}

#[test]
fn a_coalesced_toggle_reaches_the_latest_chain() {
    let mut d = running();
    queue_toggle(&mut d, false, 1);
    queue_toggle(&mut d, true, 2);
    for _ in 0..24 {
        d.period(1024);
    }
    assert_eq!(d.proc.active_generation(), 2, "latest chain remains queued");
}

#[test]
fn reopening_while_the_fade_is_pending_consumes_the_toggle() {
    let mut d = running();
    queue_toggle(&mut d, false, 1);
    d.proc.poll_updates(SinkChoice::Dsp, d.rate, d.ratio, true);
    // The RT Resume branch polls without streaming, resets only the stage
    // port, then polls again before checking the resume gate.
    // The poll without streaming settles the toggle (the passthrough
    // chain is in, so no port is left to reset).
    d.proc.poll_updates(SinkChoice::Dsp, d.rate, d.ratio, false);
    if let Some(port) = d.proc.varispeed(SinkChoice::Dsp, d.ratio, d.rate, CH) {
        port.reset();
    }
    d.proc.poll_updates(SinkChoice::Dsp, d.rate, d.ratio, false);
    assert_eq!(
        d.proc.mode(SinkChoice::Dsp, d.ratio),
        crate::engine::processor::PlayMode::Bypass
    );
}

#[test]
fn idle_disable_is_published_at_the_update() {
    let mut d = running();
    d.proc.reset_dsp_state();
    assert!(d.applied().enabled);
    d.vs_tx.push(VarispeedRtCmd::Disable).ok().unwrap();
    d.proc.poll_updates(SinkChoice::Dsp, d.rate, d.ratio, false);
    assert!(
        !d.applied().enabled,
        "snapshot still shows an installed stage"
    );
}

#[test]
fn dsp_off_fades_the_audible_end_of_a_delayed_chain() {
    let mut cfg = eq_config();
    cfg.convolution = vec![audivane_protocol::ConvolutionConfig {
        name: None,
        enabled: true,
        ir_path: "unused".into(),
        gain_db: 0.0,
    }];
    let mut ir = vec![0.0; 1024];
    ir[800] = 1.0;
    let chain = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: cfg,
        sample_rate: 48_000,
        channels: CH,
        max_block: 512,
        irs: Some(vec![ir]),
    })));
    let mut d = Drv::new(48_000, RateRatio::ONE, chain, 256);
    d.graph.source = Box::new(|_, _| 0.5);
    d.install();
    d.prime(1024, 2);
    for _ in 0..8 {
        d.period(1024);
    }
    let start = d.left.len() - 64;
    queue_toggle(&mut d, false, 1);
    for _ in 0..4 {
        d.period(1024);
    }
    let jump = max_step(&d.left[start..]);
    assert!(jump < 0.02, "an abrupt jump by {jump:.3} full scale");
}

#[test]
fn slow_large_quantum_prefill_has_time_for_the_next_graph_cycle() {
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 4096);
    d.install();
    d.slot.send(0.5, 0.0);
    d.proc.poll_updates(SinkChoice::Dsp, d.rate, d.ratio, false);
    let port = d
        .proc
        .varispeed(SinkChoice::Dsp, d.ratio, d.rate, CH)
        .unwrap();
    d.vs.start_priming(Timing::new(d.rate, 4096, 4096), port, &d.slot);
    assert!(
        d.run(4096, 4096, PullKind::Play, Some(80_000_000))
            .is_some()
    );
    assert!(
        d.run(4096, 4096, PullKind::Play, Some(60_000_000))
            .is_some()
    );
    assert!(
        d.run(4096, 4096, PullKind::Play, Some(60_000_000))
            .is_some(),
        "third prefill block falls back to silence before the next cycle is due"
    );
}

#[test]
#[ignore = "pre-existing (engine-features drains without flushing the chain's own tail either); \
            out of scope for the stage, see processor_tests::the_drain_bound_covers_a_slowdown_in_progress"]
fn the_drain_flushes_the_chain_after_the_stage() {
    let mut cfg = eq_config();
    cfg.convolution = vec![audivane_protocol::ConvolutionConfig {
        name: None,
        enabled: true,
        ir_path: "unused".into(),
        gain_db: 0.0,
    }];
    let mut ir = vec![0.0; 4096];
    ir[3000] = 1.0;
    let chain = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: cfg,
        sample_rate: 48_000,
        channels: CH,
        max_block: 512,
        irs: Some(vec![ir]),
    })));
    let mut d = Drv::new(48_000, RateRatio::ONE, chain, 256);
    d.graph.source = Box::new(|_, _| 0.25);
    d.install();
    d.prime(1024, 2);
    for _ in 0..8 {
        d.period(1024);
    }
    d.graph.source_len = Some(d.graph.produced);
    d.graph.advance(d.graph.now + 50_000_000);
    let tail = d
        .proc
        .varispeed(SinkChoice::Dsp, d.ratio, d.rate, CH)
        .unwrap()
        .tail_frames();
    let mut drain = VsDrain::default();
    while drain.more(vsl::ring_fill(&d.ring, CH), tail) {
        assert!(d.run(1024, 1024, PullKind::Drain, None).is_some());
        drain.pulled(1024);
    }
    assert!(
        d.left.last().unwrap().abs() < 1e-6,
        "drain ended while chain was still at {}",
        d.left.last().unwrap()
    );
}

#[test]
fn retuning_does_not_arm_a_disable_before_refill() {
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 128);
    d.install();
    d.slot.send(0.5, 0.0);
    d.prime(1024, 2);
    for _ in 0..8 {
        d.period(1024);
    }
    d.vs_tx.push(VarispeedRtCmd::Disable).ok().unwrap();
    d.proc.poll_updates(SinkChoice::Dsp, d.rate, d.ratio, true);
    d.q = 64;
    let p = d.proc.varispeed(SinkChoice::Dsp, d.ratio, d.rate, CH);
    assert_eq!(
        vsl::handover(
            &mut d.vs,
            &mut d.graph,
            &d.ring,
            p,
            CH,
            d.rate,
            1024,
            64,
            1024,
            &d.delay,
            10_000_000
        ),
        Handover::Retuned
    );
    let run = d.run(1024, 1024, PullKind::Play, None).unwrap();
    assert!(
        !run.underfeed,
        "mixed period took {} of {} frames",
        run.taken, run.pulled
    );
}

#[test]
fn enabling_dsp_at_half_speed_keeps_raw_audio_until_delayed_output_arrives() {
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 128);
    d.graph.source = Box::new(|_, _| 0.5);
    d.install();
    d.slot.send(0.5, 0.0);
    d.prime(128, 3);
    for _ in 0..50 {
        d.period(128);
    }
    assert_eq!(d.applied().speed, 0.5);
    queue_toggle(&mut d, false, 1);
    for _ in 0..20 {
        d.period(128);
    }
    let before = d.left.len();
    queue_toggle(&mut d, true, 2);
    for _ in 0..12 {
        d.period(128);
    }
    let output = &d.left[before..];
    let gap = longest_quiet(output);
    let jump = max_step(output);
    assert!(
        gap == 0 && jump < 0.1,
        "quiet for {gap} frames, max jump {jump:.3}"
    );
}

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

/// The longest near-silent run in `x` and where it starts.
fn quiet_run(x: &[f64]) -> (usize, usize) {
    let (mut best, mut at, mut cur) = (0, 0, 0);
    for (i, v) in x.iter().enumerate() {
        if v.abs() < 1e-4 {
            cur += 1;
            if cur > best {
                best = cur;
                at = i + 1 - cur;
            }
        } else {
            cur = 0;
        }
    }
    (best, at)
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

#[test]
fn keep_pitch_runs_the_stretch_at_unity() {
    // With Keep pitch on, the stretch stays in the path at exactly 1.0 (no
    // latency jump when the slider crosses it).
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 1024);
    d.install_stretch();
    d.prime(1024, 2);
    for _ in 0..30 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(a.active && a.stretch && a.speed == 1.0, "{a:?}");
    assert!(a.latency_frames > 6_000.0, "{a:?}");
    let tail = &d.left[d.left.len() - 24_000..];
    assert!((tone_freq(tail, 48_000) - FREQ).abs() < 0.2);
    assert!(longest_quiet(tail) < 4);
}

/// Toggle Keep pitch on a playing stage (`stretch`: to the stretch), run
/// `periods`, and return the gap (frames) and the largest step around it.
fn toggle(d: &mut Drv, stretch: bool, periods: usize) -> (usize, f64) {
    let start = d.left.len() - 64;
    let cmd = if stretch {
        VarispeedRtCmd::InstallStretch(tempo_stage(d.rate))
    } else {
        VarispeedRtCmd::Install(stage(d.rate))
    };
    assert!(d.vs_tx.push(cmd).is_ok());
    for _ in 0..periods {
        d.period(1024);
    }
    let seg = &d.left[start..];
    (quiet_run(seg).0, max_step(seg))
}

#[test]
fn keep_pitch_toggles_through_one_gap_both_ways() {
    // Varispeed at 1.1 (the tone at 1.1 kHz), Keep pitch on (1 kHz), off
    // again. Each toggle fades the playing stage out, starts the other at
    // the same speed from silence and lets its output fade in: one gap,
    // the new stage's delay, no click and nothing repeated.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install();
    d.slot.send(1.1, 0.0);
    d.prime(1024, 2);
    for _ in 0..30 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(a.active && !a.stretch && a.speed == 1.1, "{a:?}");
    let f = tone_freq(&d.left[d.left.len() - 24_000..], 48_000);
    assert!((f - 1100.0).abs() < 0.5, "{f}");
    let bound = step_bound(1.1, 48_000);

    let core_latency = a.latency_frames;
    let (gap, step) = toggle(&mut d, true, 40);
    let a = d.applied();
    assert!(a.active && a.stretch && a.speed == 1.1, "{a:?}");
    assert_eq!(a.retired, 1, "the varispeed stage retired");
    assert!(d.stretch_recycle.pop().is_err(), "no stretch retired yet");
    let f = tone_freq(&d.left[d.left.len() - 24_000..], 48_000);
    println!(
        "to the stretch: gap {gap} frames ({:.1} ms), stretch delay {:.0}, max step {step:.4}",
        gap as f64 / 48.0,
        a.latency_frames
    );
    assert!((f - FREQ).abs() < 0.2, "pitch not held: {f}");
    assert!(step < bound, "a click: {step} (bound {bound})");
    // The gap is the stretch's delay (its window's leading half fades in
    // ahead of it), never more.
    assert!(gap as f64 > 0.5 * a.latency_frames, "{gap}");
    assert!((gap as f64) < a.latency_frames + 480.0, "{gap}");
    assert!(longest_quiet(&d.left[d.left.len() - 24_000..]) < 4);

    let (gap, step) = toggle(&mut d, false, 40);
    let a = d.applied();
    assert!(a.active && !a.stretch && a.speed == 1.1, "{a:?}");
    assert!(d.stretch_recycle.pop().is_ok(), "the stretch retired");
    let f = tone_freq(&d.left[d.left.len() - 24_000..], 48_000);
    println!(
        "back to varispeed: gap {gap} frames ({:.1} ms), its delay {core_latency:.0}, max step \
         {step:.4}",
        gap as f64 / 48.0
    );
    assert!((f - 1100.0).abs() < 0.5, "{f}");
    assert!(step < bound, "a click: {step}");
    assert!((gap as f64) < core_latency + 480.0, "{gap}");
    assert_eq!(a.refused_plans, 0);
    assert_eq!(a.gap_fades, 0, "a switch never underfeeds");
}

#[test]
fn keep_pitch_toggled_back_before_the_fade_keeps_the_stage() {
    // On, then off again before the driver's next plan: the playing stage
    // stays, with no gap.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install();
    d.slot.send(1.1, 0.0);
    d.prime(1024, 2);
    for _ in 0..20 {
        d.period(1024);
    }
    let start = d.left.len() - 64;
    assert!(
        d.vs_tx
            .push(VarispeedRtCmd::InstallStretch(tempo_stage(48_000)))
            .is_ok()
    );
    assert!(d.vs_tx.push(VarispeedRtCmd::Install(stage(48_000))).is_ok());
    for _ in 0..10 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(a.active && !a.stretch && a.speed == 1.1, "{a:?}");
    assert_eq!(
        a.retired, 2,
        "both new stages retired, the playing one kept"
    );
    assert!(longest_quiet(&d.left[start..]) < 4);
}

#[test]
fn keep_pitch_ramps_live() {
    // With the stretch, a speed change rides the same envelope: no gap, no
    // click, and the pitch holds before, during and after.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install_stretch();
    d.prime(1024, 2);
    for _ in 0..30 {
        d.period(1024);
    }
    let start = d.left.len();
    d.slot.send(1.25, 200.0);
    for _ in 0..30 {
        d.period(1024);
    }
    d.slot.send(0.8, 500.0);
    for _ in 0..40 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(a.stretch && a.speed == 0.8 && !a.ramping, "{a:?}");
    let seg = &d.left[start..];
    assert!(longest_quiet(seg) < 4, "{}", longest_quiet(seg));
    let step = max_step(seg);
    assert!(step < step_bound(1.0, 48_000), "a click: {step}");
    for w in seg.chunks(9_600) {
        let f = tone_freq(w, 48_000);
        assert!((f - FREQ).abs() < 1.0, "pitch moved during the ramps: {f}");
    }
}

#[test]
fn keep_pitch_disable_and_dsp_off_fade_the_stretch() {
    // DSP off with the stretch playing: it fades out, the raw block fades
    // in. DSP on again: the raw block fades out at once and the stretch's
    // output arrives after its delay (no cross-fade over its 135 ms: that
    // would repeat the music). Then varispeed off: the stretch retires
    // under its fade into its own recycle queue.
    let mut d = Drv::new(48_000, RateRatio::ONE, chain(eq_config(), 48_000), 256);
    d.install_stretch();
    d.prime(1024, 2);
    for _ in 0..20 {
        d.period(1024);
    }
    let start = d.left.len() - 64;
    assert!(d.cmd_tx.push(RtDspCmd::SetDspEnabled(false)).is_ok());
    for _ in 0..10 {
        d.period(1024);
    }
    assert!(!d.applied().active);
    let seg = &d.left[start..];
    let bound = step_bound(1.0, 48_000);
    assert!(max_step(seg) < bound, "{}", max_step(seg));
    assert!(longest_quiet(seg) < 400, "{}", longest_quiet(seg));

    let start = d.left.len() - 64;
    assert!(d.cmd_tx.push(RtDspCmd::SetDspEnabled(true)).is_ok());
    for _ in 0..20 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(a.active && a.stretch, "{a:?}");
    let seg = &d.left[start..];
    let (gap, _) = quiet_run(seg);
    println!("DSP on: gap {gap} frames, max step {:.4}", max_step(seg));
    assert!(max_step(seg) < bound, "{}", max_step(seg));
    assert!((gap as f64) < a.latency_frames + 480.0, "{gap}");

    let start = d.left.len() - 64;
    assert!(d.vs_tx.push(VarispeedRtCmd::Disable).is_ok());
    for _ in 0..10 {
        d.period(1024);
    }
    let a = d.applied();
    assert!(!a.enabled && !a.active && !a.stretch, "{a:?}");
    assert!(d.stretch_recycle.pop().is_ok(), "retired to its queue");
    assert!(max_step(&d.left[start..]) < bound);
    assert_eq!(d.raw_padded, 0);
}
