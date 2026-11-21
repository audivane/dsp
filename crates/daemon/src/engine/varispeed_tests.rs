//! Test 13 of the adaptive resampler plan: the processor with the
//! varispeed stage. The f64 handoff (bit-identical to the plain processor
//! with the stage off), enable, disable, Direct takeover and reopen with
//! fades, retirement off the RT thread, the applied state, the latency, an
//! underfeed gap and a stage for another rate. The tests drive the
//! processor the way the driver does: `varispeed()` → `plan(n)` → the
//! planned source frames → `process_block`, then the command forwarding.

use super::{VarispeedRt, VarispeedRtCmd};
use crate::engine::processor::{
    ActiveChain, DaemonProcessor, MeterBank, PendingSwap, PlayMode, RtDspCmd,
};
use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec};
use audivane_dsp_core::varispeed::{Varispeed, VarispeedSpec};
use audivane_io_pw::EngineAudioProcessor;
use audivane_protocol::ipc::SinkChoice;
use audivane_protocol::{ChainConfig, DitherKind, EqProfile, OutputConfig, RateRatio};
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

const RATE: u32 = 48_000;
const AMP: f64 = 0.5;
const FREQ: f64 = 1000.0;

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

fn eq_chain(rate: u32) -> ActiveChain {
    ActiveChain::Single(Box::new(Chain::new(ChainSpec {
        config: eq_config(),
        sample_rate: rate,
        channels: 2,
        max_block: 8192,
        irs: None,
    })))
}

fn stage(rate: u32) -> Box<Varispeed> {
    Box::new(
        Varispeed::new(VarispeedSpec {
            sample_rate: rate,
            channels: 2,
            max_frames: 8192,
            speed: 1.0,
        })
        .unwrap(),
    )
}

/// The source tone at source frame `i`, as f32 (what PipeWire delivers).
fn tone(i: u64) -> f32 {
    (AMP * (2.0 * std::f64::consts::PI * FREQ * i as f64 / f64::from(RATE)).sin()) as f32
}

struct Rig {
    proc: DaemonProcessor,
    vs_tx: rtrb::Producer<VarispeedRtCmd>,
    vs_recycle: rtrb::Consumer<Box<Varispeed>>,
    recycle: rtrb::Consumer<ActiveChain>,
    /// Source frames consumed.
    src: u64,
    /// Nominal frames (the chains' grid).
    pos: u64,
    /// Left channel of every block, normalised.
    left: Vec<f64>,
    /// What the driver forwards after each plan.
    speed: f64,
    ramp_ms: f64,
    limit: f64,
    /// Planned source frames of the last DSP block (0: no stage).
    last_src: usize,
    cmd_tx: rtrb::Producer<RtDspCmd>,
}

impl Rig {
    fn new(chain: ActiveChain, recycle_slots: usize) -> Self {
        let (cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (_swap_tx, swap_rx) = rtrb::RingBuffer::<PendingSwap>::new(8);
        let (recycle_tx, recycle) = rtrb::RingBuffer::new(8);
        let (vs_tx, vs_rx) = rtrb::RingBuffer::new(8);
        let (vs_recycle_tx, vs_recycle) = rtrb::RingBuffer::new(recycle_slots);
        let proc = DaemonProcessor::new(
            chain,
            true,
            2,
            8192,
            16,
            RATE,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        )
        .with_varispeed(vs_rx, vs_recycle_tx, 2);
        Self {
            proc,
            vs_tx,
            vs_recycle,
            recycle,
            src: 0,
            pos: 0,
            left: Vec::new(),
            speed: 1.0,
            ramp_ms: 0.0,
            limit: 2.0,
            last_src: 0,
            cmd_tx,
        }
    }

    fn send(&mut self, cmd: VarispeedRtCmd) {
        assert!(self.vs_tx.push(cmd).is_ok());
    }

    /// One driver period of `n` nominal frames on `sink`.
    fn block(&mut self, sink: SinkChoice, n: usize) -> Vec<i32> {
        let ratio = RateRatio::ONE;
        self.proc.poll_updates(sink, RATE, ratio, true);
        let mut n_src = n;
        self.last_src = 0;
        if let Some(port) = self.proc.varispeed(sink, ratio, RATE, 2) {
            n_src = port.plan(n);
            self.last_src = n_src;
            port.set_fill(n_src, false);
            port.set_speed_limit(self.limit);
            port.set_speed(self.speed, self.ramp_ms);
            let _ = port.peak_until_settled();
            // As the driver: the next pull is sized, a stop arms.
            port.pulled();
        }
        let mut input = Vec::with_capacity(n_src * 2);
        for i in 0..n_src as u64 {
            let x = tone(self.src + i);
            input.push(x);
            input.push(x);
        }
        let mut out = vec![0_i32; n * 2];
        self.proc
            .process_block(sink, &input, &input, &mut out, 2, ratio, RATE, self.pos);
        self.src += n_src as u64;
        self.pos += n as u64;
        self.left.extend(
            out.iter()
                .step_by(2)
                .map(|&s| f64::from(s) / 2_147_483_648.0),
        );
        while self.recycle.pop().is_ok() {}
        out
    }

    fn applied(&self) -> super::VarispeedAppliedSnapshot {
        self.proc.varispeed_applied().snapshot()
    }

    fn port_speed(&mut self, sink: SinkChoice) -> Option<f64> {
        self.proc
            .varispeed(sink, RateRatio::ONE, RATE, 2)
            .map(|p| p.speed())
    }
}

/// The largest step between consecutive output samples in `x`.
fn max_step(x: &[f64]) -> f64 {
    x.windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f64::max)
}

/// A steady tone at speed `s` through the -1 dB EQ moves at most this
/// much per sample; fades add a little.
fn step_bound(s: f64) -> f64 {
    let a = AMP * 10f64.powf(-1.0 / 20.0);
    a * 2.0 * std::f64::consts::PI * FREQ * s / f64::from(RATE) * 1.1 + 0.005
}

#[test]
fn with_the_stage_off_the_handoff_is_bit_identical() {
    // The processor with a slot but no stage, against one without a slot,
    // over EQ, A/B and rational chains, blocks of varying length.
    type Make = fn() -> ActiveChain;
    let makes: [(Make, RateRatio); 3] = [
        (|| eq_chain(RATE), RateRatio::ONE),
        (
            || {
                let spec = |c: ChainConfig| ChainSpec {
                    config: c,
                    sample_rate: RATE,
                    channels: 2,
                    max_block: 8192,
                    irs: None,
                };
                let mut b = eq_config();
                b.eq.as_mut().unwrap().preamp_db = -3.0;
                ActiveChain::Ab(Box::new(
                    AbChain::new(
                        spec(eq_config()),
                        spec(b),
                        audivane_dsp_core::ab_level::AbLevel::fixed(-0.5),
                    )
                    .unwrap(),
                ))
            },
            RateRatio::ONE,
        ),
        (
            || {
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
                ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                    config: c,
                    sample_rate: RATE,
                    channels: 2,
                    max_block: 8192,
                    irs: None,
                })))
            },
            RateRatio::new(147, 160),
        ),
    ];
    for (make, ratio) in makes {
        let mut with = Rig::new(make(), 8);
        let (_c, cmd_rx) = rtrb::RingBuffer::new(8);
        let (_s, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _r) = rtrb::RingBuffer::new(8);
        let mut plain = DaemonProcessor::new(
            make(),
            true,
            2,
            8192,
            16,
            RATE,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let mut pos = 0u64;
        let mut src = 0u64;
        for (k, n) in [1usize, 512, 37, 1024, 0, 333, 4096]
            .iter()
            .cycle()
            .take(30)
            .enumerate()
        {
            let input: Vec<f32> = (0..*n as u64)
                .flat_map(|i| [tone(src + i), -tone(src + i) * 0.5])
                .collect();
            let n_out = ratio.frames_out(pos, *n);
            let (mut a, mut b) = (vec![0; n_out * 2], vec![0; n_out * 2]);
            with.proc.poll_updates(SinkChoice::Dsp, RATE, ratio, true);
            plain.poll_updates(SinkChoice::Dsp, RATE, ratio, true);
            assert!(
                with.proc
                    .varispeed(SinkChoice::Dsp, ratio, RATE, 2)
                    .is_none()
            );
            with.proc
                .process_block(SinkChoice::Dsp, &input, &input, &mut a, 2, ratio, RATE, pos);
            plain.process_block(SinkChoice::Dsp, &input, &input, &mut b, 2, ratio, RATE, pos);
            assert_eq!(a, b, "block {k}");
            pos += *n as u64;
            src += *n as u64;
        }
        assert_eq!(
            with.proc.latency_out_frames(SinkChoice::Dsp),
            plain.latency_out_frames(SinkChoice::Dsp)
        );
    }
}

#[test]
fn enable_ramp_and_disable_fade_and_retire_off_the_rt_thread() {
    let mut rig = Rig::new(eq_chain(RATE), 8);
    let n = 256;
    for _ in 0..20 {
        rig.block(SinkChoice::Dsp, n);
    }
    let chain_latency = rig.proc.latency_out_frames(SinkChoice::Dsp);
    assert!(!rig.applied().enabled);
    // Enable while playing, then ask for 1.25.
    rig.send(VarispeedRtCmd::Install(stage(RATE)));
    let start = rig.left.len();
    rig.block(SinkChoice::Dsp, n);
    let a = rig.applied();
    assert!(a.enabled && a.active, "{a:?}");
    assert_eq!(a.speed, 1.0, "starts at unity");
    assert_eq!(a.installs, 1);
    rig.speed = 1.25;
    rig.ramp_ms = 100.0;
    for _ in 0..60 {
        rig.block(SinkChoice::Dsp, n);
    }
    let a = rig.applied();
    assert_eq!(a.speed, 1.25, "{a:?}");
    assert!(!a.ramping);
    assert!(a.latency_exact);
    // The stage's delay plus its 5 ms source look-ahead (240 source
    // frames, 192 nominal at 1.25).
    assert!((a.latency_frames - (136.75 / 1.25 + 130.75 + 240.0 / 1.25)).abs() < 1e-9);
    assert_eq!(a.step_bits, 5u128 << 62);
    // At 1.25 the stage pulls 1.25 times the period.
    assert!((319..=321).contains(&rig.last_src), "{}", rig.last_src);
    // The stage counts in the latency: 136.75/1.25 + 130.75 + 192 =
    // 432.15.
    assert_eq!(
        rig.proc.latency_out_frames(SinkChoice::Dsp),
        chain_latency + 432
    );
    // No click anywhere: the enable is a fade, the ramp is smooth.
    let steps = max_step(&rig.left[start..]);
    assert!(steps < step_bound(1.25), "{steps}");
    // No silence: the raw block plays on through the stage's delay (its
    // look-ahead included) and fades out as the stage's output arrives.
    // The two are ~507 frames apart, so the tone partly cancels mid-fade
    // (to ~0.27 here), but never drops out.
    let envelope = rig.left[start..start + 4 * n]
        .chunks(24)
        .map(|w| w.iter().fold(0.0_f64, |m, x| m.max(x.abs())))
        .fold(f64::INFINITY, f64::min);
    assert!(envelope > 0.05, "a gap: {envelope}");

    // Disable: the stage's output fades out, the raw block fades in.
    rig.send(VarispeedRtCmd::Disable);
    let start = rig.left.len();
    for _ in 0..20 {
        rig.block(SinkChoice::Dsp, n);
    }
    let steps = max_step(&rig.left[start..]);
    assert!(steps < step_bound(1.25), "{steps}");
    let a = rig.applied();
    assert!(!a.enabled && !a.active, "{a:?}");
    assert_eq!(a.speed, 1.0);
    assert_eq!(a.retired, 1);
    assert_eq!(rig.last_src, 0, "the raw block again");
    assert_eq!(rig.proc.latency_out_frames(SinkChoice::Dsp), chain_latency);
    // The stage went to the reaper's queue, not dropped here.
    assert!(rig.vs_recycle.pop().is_ok());
    assert!(rig.vs_recycle.pop().is_err());
    assert_eq!(rig.proc.varispeed_stash_len(), 0);
}

#[test]
fn direct_takeover_leaves_the_stage_alone_and_restarts_it_faded() {
    let mut rig = Rig::new(eq_chain(RATE), 8);
    let n = 512;
    rig.send(VarispeedRtCmd::Install(stage(RATE)));
    rig.speed = 1.5;
    for _ in 0..40 {
        rig.block(SinkChoice::Dsp, n);
    }
    assert_eq!(rig.applied().speed, 1.5);
    // Direct plays: the samples unchanged, no stage.
    for _ in 0..10 {
        assert!(
            rig.proc
                .varispeed(SinkChoice::Direct, RateRatio::ONE, RATE, 2)
                .is_none()
        );
        let src0 = rig.src;
        let out = rig.block(SinkChoice::Direct, n);
        for (i, pair) in out.chunks(2).enumerate() {
            let want = (f64::from(tone(src0 + i as u64)) * 2_147_483_648.0).round() as i32;
            assert_eq!(pair[0], want);
        }
        let a = rig.applied();
        assert!(a.enabled && !a.active);
        assert_eq!(a.speed, 1.0, "unity is audible");
    }
    // DSP again: the stage restarts at unity, its source faded in, and
    // ramps to the request.
    assert_eq!(rig.port_speed(SinkChoice::Dsp), Some(1.0));
    rig.block(SinkChoice::Dsp, n);
    let first = rig.left.len() - n;
    let head = rig.left[first..first + 200]
        .iter()
        .fold(0.0_f64, |m, x| m.max(x.abs()));
    assert!(head < 0.01, "stale history played: {head}");
    for _ in 0..40 {
        rig.block(SinkChoice::Dsp, n);
    }
    assert_eq!(rig.applied().speed, 1.5);
    let steps = max_step(&rig.left[first..]);
    assert!(steps < step_bound(1.5), "{steps}");
}

#[test]
fn a_stage_cut_off_mid_stop_never_strands_the_dsp_toggle() {
    // DSP off at x1 fades the chain's output (the stage's with it) before
    // the switch. When the DSP chain stops playing first (here Direct
    // takes over mid-fade), the fade ends there and the toggle applies:
    // nothing waits on a fade that cannot run.
    let mut rig = Rig::new(eq_chain(RATE), 8);
    let n = 512;
    rig.send(VarispeedRtCmd::Install(stage(RATE)));
    for _ in 0..20 {
        rig.block(SinkChoice::Dsp, n);
    }
    assert!(rig.applied().active);
    assert!(rig.cmd_tx.push(RtDspCmd::SetDspEnabled(false)).is_ok());
    // 64 frames of the 240-frame fade.
    rig.block(SinkChoice::Dsp, 64);
    assert_eq!(
        rig.proc.mode(SinkChoice::Dsp, RateRatio::ONE),
        PlayMode::Full
    );
    rig.block(SinkChoice::Direct, n);
    assert_eq!(
        rig.proc.mode(SinkChoice::Dsp, RateRatio::ONE),
        PlayMode::Bypass,
        "the toggle waited on a stage that no longer runs"
    );
    let a = rig.applied();
    assert!(a.enabled && !a.active, "{a:?}");
}

#[test]
fn a_reopen_resets_at_the_target_and_a_dsp_reset_too() {
    let mut rig = Rig::new(eq_chain(RATE), 8);
    rig.send(VarispeedRtCmd::Install(stage(RATE)));
    rig.speed = 1.2;
    for _ in 0..40 {
        rig.block(SinkChoice::Dsp, 512);
    }
    assert_eq!(rig.applied().speed, 1.2);
    // The driver resets the stage at a reopen: it holds at the target.
    if let Some(port) = rig.proc.varispeed(SinkChoice::Dsp, RateRatio::ONE, RATE, 2) {
        port.reset();
        assert_eq!(port.speed(), 1.2);
        assert_eq!(port.peak_until_settled(), 1.2);
    }
    rig.block(SinkChoice::Dsp, 512);
    assert_eq!(rig.applied().speed, 1.2);
    // A DSP reset (idle stop) does the same, also mid-disable.
    rig.send(VarispeedRtCmd::Disable);
    // The fade waits a block for the driver to size the next pull for a
    // raw one.
    rig.block(SinkChoice::Dsp, 64);
    assert_eq!(rig.applied().phase, 2, "waiting for the driver");
    rig.block(SinkChoice::Dsp, 64);
    assert_eq!(rig.applied().phase, 3, "disabling");
    rig.proc.reset_dsp_state();
    rig.block(SinkChoice::Dsp, 64);
    assert_eq!(rig.applied().phase, 0);
    assert!(rig.vs_recycle.pop().is_ok());
}

#[test]
fn an_underfeed_fades_over_the_gap() {
    let mut rig = Rig::new(eq_chain(RATE), 8);
    rig.send(VarispeedRtCmd::Install(stage(RATE)));
    for _ in 0..20 {
        rig.block(SinkChoice::Dsp, 512);
    }
    let start = rig.left.len();
    // Half the planned frames arrive: zeros after them, under a fade.
    let n_src_before = rig.src;
    rig.block_with_gap(512, 256);
    assert!(rig.src > n_src_before);
    for _ in 0..10 {
        rig.block(SinkChoice::Dsp, 512);
    }
    let steps = max_step(&rig.left[start..]);
    assert!(steps < step_bound(1.0), "{steps}");
    assert_eq!(rig.applied().gap_fades, 1);
}

impl Rig {
    /// A DSP block whose planned source frames are only partly there: the
    /// rest is zeros, as the driver pads an underfeed.
    fn block_with_gap(&mut self, n: usize, valid: usize) {
        let ratio = RateRatio::ONE;
        self.proc.poll_updates(SinkChoice::Dsp, RATE, ratio, true);
        let port = self
            .proc
            .varispeed(SinkChoice::Dsp, ratio, RATE, 2)
            .expect("a stage");
        let n_src = port.plan(n);
        port.set_fill(valid, true);
        let mut input = Vec::with_capacity(n_src * 2);
        for i in 0..n_src as u64 {
            let x = if (i as usize) < valid {
                tone(self.src + i)
            } else {
                0.0
            };
            input.push(x);
            input.push(x);
        }
        let mut out = vec![0_i32; n * 2];
        self.proc.process_block(
            SinkChoice::Dsp,
            &input,
            &input,
            &mut out,
            2,
            ratio,
            RATE,
            self.pos,
        );
        self.src += valid as u64;
        self.pos += n as u64;
        self.left.extend(
            out.iter()
                .step_by(2)
                .map(|&s| f64::from(s) / 2_147_483_648.0),
        );
    }
}

#[test]
fn a_stage_for_another_rate_never_runs() {
    let mut rig = Rig::new(eq_chain(RATE), 8);
    rig.send(VarispeedRtCmd::Install(stage(44_100)));
    rig.block(SinkChoice::Dsp, 512);
    assert!(
        rig.proc
            .varispeed(SinkChoice::Dsp, RateRatio::ONE, RATE, 2)
            .is_none()
    );
    assert!(!rig.applied().active);
    assert_eq!(rig.applied().stage_rate, 44_100);
    // DSP off: never.
    let mut rig = Rig::new(eq_chain(RATE), 8);
    rig.send(VarispeedRtCmd::Install(stage(RATE)));
    rig.block(SinkChoice::Dsp, 512);
    assert!(rig.applied().active);
    rig.proc.set_dsp_enabled_for_test(false);
    assert!(
        rig.proc
            .varispeed(SinkChoice::Dsp, RateRatio::ONE, RATE, 2)
            .is_none()
    );
}

#[test]
fn retired_stages_wait_in_the_stash_when_the_reaper_is_behind() {
    // A one-slot recycle queue nobody drains: installs keep a slot to
    // retire into, so nothing is ever dropped on the RT thread.
    let mut rig = Rig::new(eq_chain(RATE), 1);
    for _ in 0..6 {
        rig.send(VarispeedRtCmd::Install(stage(RATE)));
        rig.block(SinkChoice::Dsp, 256);
    }
    let a = rig.applied();
    // One in the queue, two in the stash; the other installs wait.
    assert_eq!(rig.proc.varispeed_stash_len(), 2);
    assert_eq!(a.retired, 3);
    assert_eq!(a.installs, 4);
    // The reaper catches up: the rest go through.
    for _ in 0..8 {
        while rig.vs_recycle.pop().is_ok() {}
        rig.block(SinkChoice::Dsp, 256);
    }
    while rig.vs_recycle.pop().is_ok() {}
    assert_eq!(rig.applied().installs, 6);
    assert_eq!(rig.applied().retired, 5);
    assert_eq!(rig.proc.varispeed_stash_len(), 0);
}

#[test]
fn the_inert_slot_has_no_buffers_and_no_port() {
    let mut vs = VarispeedRt::inert();
    assert!(vs.port(RATE, 2).is_none());
    assert!(vs.take_planned().is_none());
    vs.publish();
    let a = vs.applied().snapshot();
    assert!(!a.enabled && !a.active);
    // Used only through the swap queue's types: keep them linked.
    let _ = |c: ActiveChain| PendingSwap {
        chain: c,
        crossfade: false,
        generation: 0,
        swap_seq: 0,
    };
}

/// Offline `rt_load` figures (release build, `--ignored --nocapture`): the
/// processor's time per period over the period's length, with an EQ chain
/// (and an A/B pair), the stage off and at speeds 1, 1.0123 and 2.
#[test]
#[ignore = "measurement"]
fn measure_rt_load() {
    fn ab_chain(rate: u32) -> ActiveChain {
        let spec = |c: ChainConfig| ChainSpec {
            config: c,
            sample_rate: rate,
            channels: 2,
            max_block: 8192,
            irs: None,
        };
        ActiveChain::Ab(Box::new(
            AbChain::new(
                spec(eq_config()),
                spec(eq_config()),
                audivane_dsp_core::ab_level::AbLevel::fixed(0.0),
            )
            .unwrap(),
        ))
    }
    for (rate, period) in [
        (44_100u32, 1024usize),
        (48_000, 256),
        (96_000, 2048),
        (768_000, 8192),
    ] {
        for ab in [false, true] {
            for speed in [0.0, 1.0, 1.0123, 2.0] {
                let chain = if ab { ab_chain(rate) } else { eq_chain(rate) };
                let mut rig = Rig::new(chain, 8);
                if speed > 0.0 {
                    rig.send(VarispeedRtCmd::Install(stage(rate)));
                    rig.speed = speed;
                }
                let ratio = RateRatio::ONE;
                let input: Vec<f32> = (0..(2 * period + 4) * 2)
                    .map(|i| (0.3 * (i as f64 * 0.013).sin()) as f32)
                    .collect();
                let mut out = vec![0_i32; period * 2];
                let mut worst = 0.0_f64;
                let mut total = 0.0_f64;
                let blocks = (rate as usize * 3 / period).max(50);
                for b in 0..blocks {
                    rig.proc.poll_updates(SinkChoice::Dsp, rate, ratio, true);
                    let t0 = std::time::Instant::now();
                    let mut n_src = period;
                    if let Some(port) = rig.proc.varispeed(SinkChoice::Dsp, ratio, rate, 2) {
                        n_src = port.plan(period);
                        port.set_fill(n_src, false);
                        port.set_speed_limit(2.0);
                        port.set_speed(speed, 0.0);
                    }
                    rig.proc.process_block(
                        SinkChoice::Dsp,
                        &input[..n_src * 2],
                        &input[..n_src * 2],
                        &mut out,
                        2,
                        ratio,
                        rate,
                        (b * period) as u64,
                    );
                    let dt = t0.elapsed().as_secs_f64();
                    let load = dt * f64::from(rate) / period as f64;
                    if b > blocks / 3 {
                        worst = worst.max(load);
                        total += load;
                    }
                }
                let mean = total / (blocks - blocks / 3 - 1) as f64;
                eprintln!(
                    "RTLOAD rate {rate} P {period} ab {ab} speed {speed}: mean {:.4} max {:.4} (applied {})",
                    mean,
                    worst,
                    rig.applied().speed
                );
            }
        }
    }
}

#[test]
fn the_drain_bound_covers_a_slowdown_in_progress() {
    // Mid-ramp from 1.0 to 0.5 the drain sizes the stage's tail and its
    // deadline at 0.5, the slowest speed until the ramp settles, not at the
    // speed applied when it starts.
    let mut rig = Rig::new(eq_chain(RATE), 8);
    rig.send(VarispeedRtCmd::Install(stage(RATE)));
    for _ in 0..20 {
        rig.block(SinkChoice::Dsp, 512);
    }
    rig.speed = 0.5;
    rig.ramp_ms = 100.0;
    rig.block(SinkChoice::Dsp, 512);
    let port = rig
        .proc
        .varispeed(SinkChoice::Dsp, RateRatio::ONE, RATE, 2)
        .expect("a stage");
    assert!(port.speed() > 0.9, "{}", port.speed());
    assert_eq!(port.drain_speed(), 0.5);
    let ramping = port.tail_frames();
    for _ in 0..40 {
        rig.block(SinkChoice::Dsp, 512);
    }
    let port = rig
        .proc
        .varispeed(SinkChoice::Dsp, RateRatio::ONE, RATE, 2)
        .expect("a stage");
    assert_eq!(port.speed(), 0.5);
    assert_eq!(ramping, port.tail_frames());
}
