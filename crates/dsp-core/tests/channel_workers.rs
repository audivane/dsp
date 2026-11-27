//! Channel workers ([`Chain::attach_workers`]): the output is bit for bit
//! the single-threaded chain's, for every kind of stage after crossfeed and
//! every upsampler runtime; a late worker makes the chain silent (and the
//! wait bounded), then it resets and plays on exactly as a reset chain.

use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec, Side};
use audivane_dsp_core::gain::StereoGainRamp;
use audivane_dsp_core::presets::FilterPreset;
use audivane_dsp_core::workers::LanePool;
use audivane_protocol::{
    Band, ChainConfig, ChannelEq, ConvolutionConfig, CrossfeedConfig, CrossfeedMode, DcBlockConfig,
    DitherKind, EqProfile, FilterType, HarmonicsBias, HarmonicsConfig, HarmonicsLayer,
    HarmonicsRouting, HarmonicsSag, LimiterConfig, LimiterMode, OutputConfig, PitchConfig,
    RateTarget, TiltConfig, TiltPivot, UpsamplerConfig,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RATE: u32 = 44_100;
const MAX_BLOCK: usize = 512;

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
}

/// A pool that waits up to a minute for a lane: the output tests do not
/// depend on how busy the machine is (the late-worker test uses
/// [`late_pool`]).
fn pool(workers: usize) -> Arc<LanePool> {
    let p = late_pool(workers);
    p.set_min_wait(Duration::from_secs(60));
    p
}

/// A pool with the engine's deadline (two blocks), its workers started.
fn late_pool(workers: usize) -> Arc<LanePool> {
    let p = LanePool::spawn(workers, |_| {}).unwrap();
    assert!(p.wait_started(Duration::from_secs(10)));
    p
}

fn band(kind: FilterType, freq_hz: f64, gain_db: f64, q: f64) -> Band {
    Band {
        kind,
        freq_hz,
        gain_db,
        q,
        enabled: true,
    }
}

/// Every stage on, the EQ different per channel, a manual kernel at `target`.
fn full(target: RateTarget, taps: usize) -> ChainConfig {
    ChainConfig {
        dc_block: Some(DcBlockConfig::default()),
        crossfeed: Some(CrossfeedConfig {
            enabled: true,
            mode: CrossfeedMode::Bs2bDefault,
            ..Default::default()
        }),
        eq: Some(EqProfile {
            name: "test".into(),
            preamp_db: -3.0,
            bands: vec![
                band(FilterType::Peaking, 1000.0, 3.0, 1.0),
                band(FilterType::LowShelf, 100.0, -2.0, 0.7),
            ],
            left: Some(ChannelEq {
                enabled: true,
                preamp_db: -1.0,
                bands: vec![band(FilterType::Peaking, 250.0, -1.5, 1.5)],
            }),
            right: Some(ChannelEq {
                enabled: true,
                preamp_db: -2.0,
                bands: vec![band(FilterType::Peaking, 500.0, 2.0, 1.0)],
            }),
            tilt: Some(TiltConfig {
                enabled: true,
                slope_db_per_oct: Some(-0.7),
                total_db: None,
                start_hz: 20.0,
                end_hz: 1000.0,
                pivot: TiltPivot::End,
            }),
            ..Default::default()
        }),
        convolution: vec![ConvolutionConfig {
            name: Some("test-ir".into()),
            enabled: true,
            ir_path: "unused.wav".into(),
            gain_db: -1.0,
            matrix: None,
            calibration: false,
        }],
        upsampler: audivane_dsp_core::resample::ManualConfig {
            target,
            taps,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap(),
        harmonics: Some({
            let mut h = HarmonicsConfig::default();
            h.layers.push(
                HarmonicsConfig::from_preset("output_transformer")
                    .unwrap()
                    .layers[0]
                    .clone(),
            );
            h.layers
                .push(HarmonicsConfig::from_preset("tape_studio").unwrap().layers[0].clone());
            // A layer with sag and bias (its own envelope state per channel).
            let mut dynamic = HarmonicsConfig::from_preset("pentode").unwrap().layers[0].clone();
            if let HarmonicsLayer::Chebyshev(l) = &mut dynamic {
                l.sag = Some(HarmonicsSag {
                    depth_db: 3.0,
                    attack_ms: 2.0,
                    release_ms: 30.0,
                });
                l.bias = Some(HarmonicsBias {
                    depth: 0.1,
                    attack_ms: 5.0,
                    release_ms: 80.0,
                });
            }
            h.layers.push(dynamic);
            // A tilted layer with sag and bias (per-order filter state).
            let mut tilted = HarmonicsConfig::from_preset("pp_pentode_feedback")
                .unwrap()
                .layers[0]
                .clone();
            if let HarmonicsLayer::Chebyshev(l) = &mut tilted {
                assert!(l.tilt.is_some());
                l.sag = Some(HarmonicsSag {
                    depth_db: 2.0,
                    attack_ms: 1.0,
                    release_ms: 20.0,
                });
                l.bias = Some(HarmonicsBias {
                    depth: -0.1,
                    attack_ms: 3.0,
                    release_ms: 60.0,
                });
            }
            h.layers.push(tilted);
            h
        }),
        output: OutputConfig {
            bits: 24,
            dither: DitherKind::Shaped,
            headroom_db: 1.5,
        },
        limiter: LimiterConfig {
            mode: LimiterMode::Limit,
            ..Default::default()
        },
        ..ChainConfig::neutral()
    }
}

/// Per-channel IRs (different), or one mono IR for both.
fn irs(channels: usize, taps: usize, seed: u64) -> Vec<Vec<f64>> {
    let mut seed = seed;
    (0..channels)
        .map(|_| {
            (0..taps)
                .map(|i| lcg(&mut seed) * (-(i as f64) / 8_000.0).exp() * 0.02)
                .collect()
        })
        .collect()
}

fn spec(config: ChainConfig, irs: Option<Vec<Vec<f64>>>) -> ChainSpec {
    ChainSpec {
        config,
        sample_rate: RATE,
        channels: 2,
        max_block: MAX_BLOCK,
        irs: irs.map(Into::into),
    }
}

/// Uneven call sizes up to the max block, 0 and 1 included.
fn blocks(total: usize) -> Vec<usize> {
    let pattern = [MAX_BLOCK, 1, 0, 300, 37, MAX_BLOCK - 1, 128, 511, 2];
    let mut out = Vec::new();
    let mut sum = 0;
    for i in 0.. {
        if sum >= total {
            break;
        }
        let n = pattern[i % pattern.len()];
        out.push(n);
        sum += n;
    }
    out
}

/// Loud stereo noise with a different signal per channel, some samples
/// over full scale (the limiter and the quantizer's clips run).
fn input(frames: usize) -> Vec<f32> {
    let mut seed = 77;
    (0..frames * 2)
        .map(|i| {
            let x = lcg(&mut seed) * 0.6;
            let burst = if (i / 2) % 9000 < 300 { 2.2 } else { 1.0 };
            (x * burst) as f32
        })
        .collect()
}

/// Something that processes interleaved f32 blocks.
trait Run {
    fn run(&mut self, input: &[f32], out: &mut [i32], tap: &mut [f32]) -> String;
    fn out_frames(&self, frames: usize) -> usize;
}

impl Run for Chain {
    fn run(&mut self, input: &[f32], out: &mut [i32], tap: &mut [f32]) -> String {
        let mut gain = audivane_dsp_core::gain::StereoGainRamp::unity(self.output_rate());
        format!(
            "{:?}",
            self.process_f32_with_gain_tapped(input, out, &mut gain, Some(tap))
        )
    }
    fn out_frames(&self, frames: usize) -> usize {
        self.output_frames(frames)
    }
}

impl Run for AbChain {
    fn run(&mut self, input: &[f32], out: &mut [i32], tap: &mut [f32]) -> String {
        let mut gain = audivane_dsp_core::gain::StereoGainRamp::unity(self.output_rate());
        format!(
            "{:?}",
            self.process_f32_with_gain_tapped(input, out, &mut gain, Some(tap))
        )
    }
    fn out_frames(&self, frames: usize) -> usize {
        self.output_frames(frames)
    }
}

/// Run both over the same blocks; every output sample, tap sample and
/// clip statistic must agree. `each` runs before every block on both.
fn assert_identical<C: Run>(
    what: &str,
    single: &mut C,
    multi: &mut C,
    total: usize,
    mut each: impl FnMut(usize, &mut C),
) {
    let x = input(total);
    let max_out = single.out_frames(MAX_BLOCK).max(MAX_BLOCK) * 64;
    let (mut o1, mut o2) = (vec![0i32; max_out * 2], vec![0i32; max_out * 2]);
    let (mut t1, mut t2) = (vec![0f32; max_out * 2], vec![0f32; max_out * 2]);
    let mut pos = 0;
    let mut nonzero = false;
    for (i, n) in blocks(total).into_iter().enumerate() {
        let n = n.min(total - pos);
        each(i, single);
        each(i, multi);
        let k = single.out_frames(n);
        assert_eq!(k, multi.out_frames(n), "{what}: output frames, block {i}");
        let block = &x[pos * 2..(pos + n) * 2];
        let s1 = single.run(block, &mut o1, &mut t1);
        let s2 = multi.run(block, &mut o2, &mut t2);
        assert_eq!(s1, s2, "{what}: clip statistics, block {i}");
        if let Some(j) = (0..k * 2).find(|&j| o1[j] != o2[j]) {
            panic!(
                "{what}: block {i}, sample {j} (channel {}): {} vs {}",
                j % 2,
                o1[j],
                o2[j]
            );
        }
        let bits = |t: &[f32]| t[..k * 2].iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        assert!(bits(&t1) == bits(&t2), "{what}: tap differs, block {i}");
        nonzero |= o1[..k * 2].iter().any(|&v| v != 0);
        pos += n;
    }
    assert!(nonzero, "{what}: the test signal reached the output");
}

fn pair(
    config: ChainConfig,
    irs: Option<Vec<Vec<f64>>>,
    workers: &Arc<LanePool>,
) -> (Chain, Chain) {
    let single = Chain::new(spec(config.clone(), irs.clone()));
    let mut multi = Chain::new(spec(config, irs));
    assert!(multi.attach_workers(workers), "the chain splits");
    assert!(multi.uses_workers() && !single.uses_workers());
    (single, multi)
}

#[test]
fn every_stage_after_crossfeed_is_bit_identical_on_workers() {
    let workers = pool(1);
    type Irs = Option<Vec<Vec<f64>>>;
    let cases: Vec<(&str, ChainConfig, Irs)> = vec![
        (
            "x4 kernel, stereo IRs",
            full(RateTarget::Ratio(4), 8191),
            Some(irs(2, 40_000, 5)),
        ),
        (
            "x2 kernel, mono IR",
            full(RateTarget::Ratio(2), 4096),
            Some(irs(1, 20_000, 6)),
        ),
        (
            // 40000 taps per branch: non-uniform partitions (direct and
            // four-step tail stages, sliced over many blocks), every stage
            // heard within the run.
            "x4 long kernel (tail stages)",
            {
                let mut c = full(RateTarget::Ratio(4), 160_000);
                c.convolution.clear();
                c
            },
            None,
        ),
        (
            "rational 44.1 -> 96 kHz",
            full(RateTarget::Rate(96_000), 4096),
            Some(irs(2, 3_000, 7)),
        ),
        (
            "x1 (no upsampler)",
            {
                let mut c = full(RateTarget::Ratio(2), 64);
                c.upsampler.target = RateTarget::Off;
                c
            },
            Some(irs(2, 5_000, 8)),
        ),
        (
            // The same harmonics layers in series: each layer its own
            // stage, split per channel.
            "harmonics in series",
            {
                let mut c = full(RateTarget::Ratio(4), 1024);
                if let Some(h) = &mut c.harmonics {
                    h.routing = HarmonicsRouting::Series;
                }
                c.convolution.clear();
                c
            },
            None,
        ),
        (
            "pitch before the split",
            {
                let mut c = full(RateTarget::Ratio(2), 1024);
                c.pitch = PitchConfig {
                    enabled: true,
                    cents: 23.0,
                    ..Default::default()
                };
                c.convolution.clear();
                c
            },
            None,
        ),
    ];
    for (what, config, irs) in cases {
        let (mut single, mut multi) = pair(config, irs, &workers);
        assert_identical(what, &mut single, &mut multi, 40_000, |_, _| {});
        // A reset in the middle (a seek) on both.
        single.reset();
        multi.reset();
        assert_identical(what, &mut single, &mut multi, 6_000, |_, _| {});
    }
    assert_eq!(
        workers
            .stats()
            .late_blocks
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

/// A filter matrix couples the channels: it and the EQ before it stay
/// whole (on the audio thread), the stages after it split; the output is
/// bit for bit the single-threaded chain's.
#[test]
fn a_filter_matrix_chain_is_bit_identical_on_workers() {
    use audivane_dsp_core::matrix::{ConvIrs, FilterMatrix};
    let workers = pool(1);
    let f = irs(2, 3_000, 9);
    let g = irs(2, 1_500, 10);
    let mut m = FilterMatrix::new(2);
    m.set(0, 0, f[0].clone());
    m.set(0, 1, g[0].clone());
    m.set(1, 1, f[1].clone());
    let mut sub = FilterMatrix::new(2);
    sub.set(0, 0, g[1].clone());
    sub.set(0, 1, f[0].clone());
    for (what, config, m) in [
        ("x4 kernel, 2x2 matrix", full(RateTarget::Ratio(4), 4096), m),
        (
            "x2 kernel, 2 -> 1 sum",
            full(RateTarget::Ratio(2), 1024),
            sub,
        ),
    ] {
        let irs = Some(ConvIrs::Matrix(m));
        let mk = |irs: Option<ConvIrs>| ChainSpec {
            config: config.clone(),
            sample_rate: RATE,
            channels: 2,
            max_block: MAX_BLOCK,
            irs,
        };
        let mut single = Chain::new(mk(irs.clone()));
        let mut multi = Chain::new(mk(irs));
        assert!(multi.attach_workers(&workers), "{what}: the chain splits");
        assert_identical(what, &mut single, &mut multi, 30_000, |_, _| {});
        single.reset();
        multi.reset();
        assert_identical(what, &mut single, &mut multi, 6_000, |_, _| {});
    }
}

#[test]
fn every_upsampler_runtime_is_bit_identical_on_workers() {
    use FilterPreset::*;
    let workers = pool(1);
    for (preset, ratio) in [
        (BlockFftFast, 4),
        (BlockFft, 2),
        (CascadeElliptic, 4),
        (CascadeInverseChebyshev, 6),
        (CascadeFir, 8),
        (CascadeFft, 4),
        (Nos, 4),
        (Cosine, 4),
        (CatmullRom, 2),
        (TableSinc, 4),
        (GaussianShort, 8),
        (KaiserSteep, 2),
    ] {
        let mut config = full(RateTarget::Ratio(ratio), 64);
        config.upsampler = UpsamplerConfig {
            target: RateTarget::Ratio(ratio),
            preset: Some(preset.name().to_string()),
            ..Default::default()
        };
        config.convolution.clear();
        let (mut single, mut multi) = pair(config, None, &workers);
        // BlockFft's blocks are 16 k frames: run a few of them.
        assert_identical(preset.name(), &mut single, &mut multi, 70_000, |_, _| {});
    }
}

#[test]
fn ab_pairs_are_bit_identical_on_workers() {
    let workers = pool(1);
    let level = audivane_dsp_core::ab_level::AbLevel::from_config(
        &audivane_protocol::ipc::LevelMatch::default(),
        audivane_protocol::ipc::AbHeadroom::Auto,
    );
    let a = full(RateTarget::Ratio(4), 8191);
    let mut b = full(RateTarget::Ratio(4), 2047);
    b.eq = None;
    b.harmonics = None;
    b.convolution.clear();
    // B's own quantizer: the output compare's cut.
    b.output.bits = 16;
    b.output.dither = DitherKind::Tpdf;
    let build = || {
        AbChain::new(
            spec(a.clone(), Some(irs(2, 20_000, 9))),
            spec(b.clone(), None),
            level,
        )
        .unwrap()
    };
    let (mut single, mut multi) = (build(), build());
    assert!(multi.attach_workers(&workers) && multi.uses_workers());
    assert_identical(
        "A/B",
        &mut single,
        &mut multi,
        60_000,
        |i, ab: &mut AbChain| {
            if i % 40 == 5 {
                let side = if (i / 40) % 2 == 0 { Side::B } else { Side::A };
                ab.select(side, 700);
            }
        },
    );
}

#[test]
fn single_channel_and_passthrough_chains_stay_single_threaded() {
    let workers = pool(1);
    let mut mono_config = full(RateTarget::Ratio(2), 1024);
    mono_config.crossfeed = None;
    mono_config.convolution.clear();
    let mut mono = Chain::new(ChainSpec {
        channels: 1,
        ..spec(mono_config, None)
    });
    assert!(!mono.attach_workers(&workers));
    let mut bypass = Chain::new(spec(ChainConfig::neutral(), None));
    assert!(!bypass.attach_workers(&workers));
    let none = LanePool::spawn(0, |_| {}).unwrap();
    let mut config = full(RateTarget::Ratio(2), 1024);
    config.convolution.clear();
    let mut chain = Chain::new(spec(config, None));
    assert!(!chain.attach_workers(&none));
    assert!(chain.attach_workers(&workers));
    assert!(!chain.attach_workers(&workers), "only once");
}

/// A late worker: the block is silent and the wait bounded; the chain stays
/// silent while the lane is out, then resets and plays on exactly as a
/// chain reset at the same point.
#[test]
fn a_late_worker_gives_silence_then_a_reset_never_a_hang() {
    let workers = late_pool(1);
    let config = full(RateTarget::Ratio(4), 4096);
    let ir = Some(irs(2, 10_000, 3));
    let mut chain = Chain::new(spec(config.clone(), ir.clone()));
    assert!(chain.attach_workers(&workers));
    let x = input(200 * MAX_BLOCK);
    let n = MAX_BLOCK;
    let block = |i: usize| &x[(i % 200) * n * 2..(i % 200 + 1) * n * 2];
    let out_n = chain.output_frames(n) * 2;
    let mut out = vec![0i32; out_n];
    let mut tap = vec![0f32; out_n];
    for i in 0..20 {
        chain.run(block(i), &mut out, &mut tap);
    }
    assert!(out.iter().any(|&v| v != 0));

    // The worker sleeps 300 ms before its next lane: far past the
    // deadline (two 512-frame blocks at 44.1 kHz, 23 ms).
    workers.stall_next(0, Duration::from_millis(300));
    let t0 = Instant::now();
    chain.run(block(20), &mut out, &mut tap);
    let waited = t0.elapsed();
    assert!(
        waited < Duration::from_millis(150),
        "the audio thread waited {waited:?}"
    );
    assert!(out.iter().all(|&v| v == 0), "a late block is silent");
    assert!(tap.iter().all(|&v| v == 0.0));
    let late = |p: &LanePool| {
        p.stats()
            .late_blocks
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    assert_eq!(late(&workers), 1);

    // While the lane is out: silence, without waiting.
    let mut i = 21;
    while t0.elapsed() < Duration::from_millis(200) {
        let t = Instant::now();
        chain.run(block(i), &mut out, &mut tap);
        assert!(t.elapsed() < Duration::from_millis(20));
        assert!(out.iter().all(|&v| v == 0));
        i += 1;
    }
    // Wait for the stalled job to end; then the chain is back, reset at
    // its grid position: it matches a fresh chain from here on.
    std::thread::sleep(Duration::from_millis(200));
    // From here on the comparison must not depend on the machine's load.
    workers.set_min_wait(Duration::from_secs(60));
    let mut fresh = Chain::new(spec(config, ir));
    for j in i..i + 60 {
        let mut o2 = vec![0i32; out_n];
        let mut t2 = vec![0f32; out_n];
        chain.run(block(j), &mut out, &mut tap);
        fresh.run(block(j), &mut o2, &mut t2);
        assert_eq!(out, o2, "block {j} after the recovery");
    }
    assert!(out.iter().any(|&v| v != 0));
    assert_eq!(late(&workers), 1);
}

/// A worker still busy when a block starts gets no lane: the audio thread
/// runs it, and the output is still exact.
#[test]
fn a_busy_worker_is_skipped_and_the_lane_runs_on_the_audio_thread() {
    use audivane_dsp_core::workers::{Lane, LaneWork};
    struct Sleep(Duration);
    impl LaneWork for Sleep {
        fn run(&mut self) {
            std::thread::sleep(self.0);
        }
    }
    let workers = pool(1);
    let mut config = full(RateTarget::Ratio(2), 2048);
    config.convolution.clear();
    let (mut single, mut multi) = pair(config, None, &workers);
    // Another job holds the worker for the whole comparison.
    let mut busy = Lane::new(Sleep(Duration::from_secs(3)));
    assert!(workers.submit(0, &mut busy));
    assert!(!workers.is_idle(0));
    let inline = |p: &LanePool| {
        p.stats()
            .inline_runs
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let before = inline(&workers);
    assert_identical("busy worker", &mut single, &mut multi, 3_000, |_, _| {});
    assert!(
        inline(&workers) > before,
        "the lane ran on the audio thread"
    );
    assert!(workers.wait(0, &busy, Instant::now() + Duration::from_secs(10)));
}

fn stat(p: &LanePool, which: fn(&audivane_dsp_core::workers::PoolStats) -> u64) -> u64 {
    which(p.stats())
}

fn panics(s: &audivane_dsp_core::workers::PoolStats) -> u64 {
    s.panics.load(std::sync::atomic::Ordering::Relaxed)
}

/// A lane that panics on its worker: that block and every later one are
/// digital silence, never the lane's stale or half-written buffers (which
/// still hold the last block's sound).
#[test]
fn a_panicking_lane_gives_silence_never_its_buffers() {
    let workers = pool(1);
    let mut config = full(RateTarget::Ratio(2), 2048);
    config.convolution.clear();
    let mut chain = Chain::new(spec(config, None));
    assert!(chain.attach_workers(&workers));
    let x = input(40 * MAX_BLOCK);
    let n = MAX_BLOCK;
    let block = |i: usize| &x[(i % 40) * n * 2..(i % 40 + 1) * n * 2];
    let out_n = chain.output_frames(n) * 2;
    let (mut out, mut tap) = (vec![0i32; out_n], vec![0f32; out_n]);
    for i in 0..10 {
        chain.run(block(i), &mut out, &mut tap);
    }
    assert!(out.iter().any(|&v| v != 0));
    workers.panic_next(0);
    for i in 10..30 {
        chain.run(block(i), &mut out, &mut tap);
        assert!(out.iter().all(|&v| v == 0), "block {i} after the panic");
        assert!(tap.iter().all(|&v| v == 0.0), "tap, block {i}");
    }
    assert_eq!(stat(&workers, panics), 1);
}

/// A late lane that panics: when it comes back the chain does not recover
/// (which would play it), it stays silent.
#[test]
fn a_late_lane_that_panicked_stays_silent_when_it_is_back() {
    let workers = late_pool(1);
    let mut config = full(RateTarget::Ratio(2), 2048);
    config.convolution.clear();
    let mut chain = Chain::new(spec(config, None));
    assert!(chain.attach_workers(&workers));
    let x = input(40 * MAX_BLOCK);
    let n = MAX_BLOCK;
    let block = |i: usize| &x[(i % 40) * n * 2..(i % 40 + 1) * n * 2];
    let out_n = chain.output_frames(n) * 2;
    let (mut out, mut tap) = (vec![0i32; out_n], vec![0f32; out_n]);
    for i in 0..10 {
        chain.run(block(i), &mut out, &mut tap);
    }
    assert!(out.iter().any(|&v| v != 0));
    workers.stall_next(0, Duration::from_millis(150));
    workers.panic_next(0);
    chain.run(block(10), &mut out, &mut tap);
    assert!(out.iter().all(|&v| v == 0), "the late block");
    let t = Instant::now();
    while stat(&workers, panics) == 0 || !workers.is_idle(0) {
        assert!(t.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1));
    }
    for i in 11..30 {
        chain.run(block(i), &mut out, &mut tap);
        assert!(
            out.iter().all(|&v| v == 0),
            "block {i} after the late panic"
        );
    }
}

/// Persistent-gain runs of a chain or an A/B pair.
trait RunGain {
    fn run_gain(&mut self, input: &[f32], out: &mut [i32], gain: &mut StereoGainRamp);
    fn rate(&self) -> u32;
    fn frames_out(&self, frames: usize) -> usize;
}

impl RunGain for Chain {
    fn run_gain(&mut self, input: &[f32], out: &mut [i32], gain: &mut StereoGainRamp) {
        self.process_f32_with_gain(input, out, gain);
    }
    fn rate(&self) -> u32 {
        self.output_rate()
    }
    fn frames_out(&self, frames: usize) -> usize {
        self.output_frames(frames)
    }
}

impl RunGain for AbChain {
    fn run_gain(&mut self, input: &[f32], out: &mut [i32], gain: &mut StereoGainRamp) {
        self.process_f32_with_gain(input, out, gain);
    }
    fn rate(&self) -> u32 {
        self.output_rate()
    }
    fn frames_out(&self, frames: usize) -> usize {
        self.output_frames(frames)
    }
}

/// A mute set while a late worker keeps the chain silent: the volume ramp
/// runs through the silent blocks, so the first block back is already
/// muted (not at the old volume, ramping down only then).
fn mute_during_a_stall_holds<C: RunGain>(what: &str, chain: &mut C, workers: &LanePool) {
    let x = input(40 * MAX_BLOCK);
    let n = MAX_BLOCK;
    let block = |i: usize| &x[(i % 40) * n * 2..(i % 40 + 1) * n * 2];
    let mut out = vec![0i32; chain.frames_out(n) * 2];
    let mut gain = StereoGainRamp::unity(chain.rate());
    for i in 0..10 {
        chain.run_gain(block(i), &mut out, &mut gain);
    }
    assert!(out.iter().any(|&v| v != 0), "{what}: plays");
    workers.stall_next(0, Duration::from_millis(250));
    chain.run_gain(block(10), &mut out, &mut gain);
    assert!(out.iter().all(|&v| v == 0), "{what}: the late block");
    gain.set_targets([0.0; 3], true);
    // Silent blocks while the lane is out (more than the ramp's length).
    let mut i = 11;
    while !workers.is_idle(0) {
        chain.run_gain(block(i), &mut out, &mut gain);
        assert!(out.iter().all(|&v| v == 0), "{what}: block {i}, stalled");
        i += 1;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(i > 14, "{what}: the stall lasted a few blocks");
    assert_eq!(
        gain.current(),
        [0.0; 3],
        "{what}: the ramp ran in the silence"
    );
    for j in i..i + 10 {
        chain.run_gain(block(j), &mut out, &mut gain);
        assert!(
            out.iter().all(|&v| v == 0),
            "{what}: block {j}, back and muted"
        );
    }
}

#[test]
fn a_mute_during_a_stall_is_done_when_sound_comes_back() {
    let workers = late_pool(1);
    let mut config = full(RateTarget::Ratio(2), 2048);
    config.convolution.clear();
    // No dither: a muted block is exact zeros.
    config.output.dither = DitherKind::None;
    let mut chain = Chain::new(spec(config.clone(), None));
    assert!(chain.attach_workers(&workers));
    mute_during_a_stall_holds("single", &mut chain, &workers);

    let level = audivane_dsp_core::ab_level::AbLevel::from_config(
        &audivane_protocol::ipc::LevelMatch::default(),
        audivane_protocol::ipc::AbHeadroom::Auto,
    );
    let mut b = config.clone();
    b.eq = None;
    let mut ab = AbChain::new(spec(config, None), spec(b, None), level).unwrap();
    assert!(ab.attach_workers(&workers));
    mute_during_a_stall_holds("A/B", &mut ab, &workers);
}

/// The live harmonics tap only reads: the output is bit for bit the
/// untapped chain's, on one thread and on workers, and the two taps agree
/// (to the rounding of the mono sum's order) frame for frame.
#[test]
fn harmonics_tap_changes_nothing_and_agrees_on_workers() {
    let config = full(RateTarget::Ratio(2), 1024);
    assert!(config.harmonics.is_some());
    let irs = Some(irs(2, 4096, 5));
    let mut plain = Chain::new(spec(config.clone(), irs.clone()));
    let mut tapped = Chain::new(spec(config.clone(), irs.clone()));
    let mut split = Chain::new(spec(config, irs));
    let workers = pool(1);
    assert!(split.attach_workers(&workers));
    tapped.set_harmonics_tap(true);
    split.set_harmonics_tap(true);
    let x = input(40_000);
    let mut at = 0;
    let mut frames = 0;
    let (mut a, mut b, mut c) = (
        vec![0i32; MAX_BLOCK * 4],
        vec![0i32; MAX_BLOCK * 4],
        vec![0i32; MAX_BLOCK * 4],
    );
    for n in blocks(40_000) {
        let n = n.min(40_000 - at);
        let block = &x[at * 2..(at + n) * 2];
        at += n;
        plain.process_f32(block, &mut a);
        tapped.process_f32(block, &mut b);
        split.process_f32(block, &mut c);
        let out = plain.output_frames(0).max(n * 2) * 2;
        assert_eq!(a[..out], b[..out]);
        assert_eq!(a[..out], c[..out]);
        assert!(plain.harmonics_tap().is_none());
        match (tapped.harmonics_tap(), split.harmonics_tap()) {
            (Some(t), Some(s)) => {
                assert_eq!(t.input.len(), n * 2);
                assert_eq!(t.input.len(), s.input.len());
                assert_eq!(t.linear_gain, s.linear_gain);
                assert_eq!(t.layers, s.layers);
                for (u, v) in t
                    .input
                    .iter()
                    .zip(s.input)
                    .chain(t.output.iter().zip(s.output))
                {
                    assert!((u - v).abs() <= 1e-6, "{u} {v}");
                }
                frames += n;
            }
            (None, None) => assert_eq!(n, 0),
            _ => panic!("the taps disagree on block of {n}"),
        }
    }
    assert!(frames > 30_000);
    // Off: nothing.
    tapped.set_harmonics_tap(false);
    tapped.process_f32(&x[..512], &mut b);
    assert!(tapped.harmonics_tap().is_none());
}

/// The pair's tap follows the side playing, one side per block, and only
/// reads: the output is the untapped pair's bit for bit.
#[test]
fn ab_harmonics_tap_follows_the_side_and_changes_nothing() {
    let a = full(RateTarget::Ratio(2), 1024);
    let mut b = a.clone();
    if let Some(h) = &mut b.harmonics {
        h.routing = HarmonicsRouting::Series;
    }
    let level =
        audivane_dsp_core::ab_level::AbLevel::from_config(&Default::default(), Default::default());
    let irs = Some(irs(2, 4096, 5));
    let pair = || {
        AbChain::new(
            spec(a.clone(), irs.clone()),
            spec(b.clone(), irs.clone()),
            level,
        )
        .unwrap()
    };
    let (mut plain, mut tapped) = (pair(), pair());
    tapped.set_harmonics_tap(true);
    let x = input(20_000);
    let (mut o1, mut o2) = (vec![0i32; MAX_BLOCK * 4], vec![0i32; MAX_BLOCK * 4]);
    let mut sides = Vec::new();
    for (i, block) in x.chunks(MAX_BLOCK * 2).enumerate() {
        if i == 15 {
            plain.select(Side::B, 300);
            tapped.select(Side::B, 300);
        }
        plain.process_f32(block, &mut o1);
        tapped.process_f32(block, &mut o2);
        let n = block.len() / 2 * 2 * 2;
        assert_eq!(o1[..n], o2[..n]);
        let t = tapped.harmonics_tap().expect("a tap every block");
        sides.push(t.side);
        assert_eq!(t.input.len(), block.len() / 2 * 2);
    }
    assert!(sides[..15].iter().all(|&s| s == 0));
    assert!(sides[15..].iter().all(|&s| s == 1));
}
