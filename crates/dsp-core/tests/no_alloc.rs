//! Every real-time entry point of dsp-core (`process`, `push`, `reset`,
//! `select`) must not touch the heap. A counting global allocator, armed
//! per thread around the calls under test, enforces it.

use audivane_dsp_core::Stage;
use audivane_dsp_core::adaptive::{AdaptiveCore, Segment, Step};
use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec, Side};
use audivane_dsp_core::conv::Convolver;
use audivane_dsp_core::dc_block::DcBlock;
use audivane_dsp_core::dither::Quantizer;
use audivane_dsp_core::drift::{
    DriftLoop, EpsilonTrack, FRAME, LoopConfig, PhaseDetector, PositionMap, RateEstimator, Reading,
};
use audivane_dsp_core::loudness::{AWeightedMeter, LufsMeter};
use audivane_dsp_core::pitch::PitchShifter;
use audivane_dsp_core::resample::{Upsampler, UpsamplerDesign};
use audivane_dsp_core::tempo::{TempoSpec, TempoStretch};
use audivane_dsp_core::varispeed::{MAX_FRAMES, Varispeed, VarispeedSpec};
use audivane_protocol::{
    Band, ChainConfig, ChannelEq, ChebyshevLayer, ConvolutionConfig, CrossfeedConfig,
    CrossfeedMode, DcBlockConfig, DitherKind, EqProfile, FilterPhase, FilterType,
    HARMONICS_DEFAULT_SIGNS, HarmonicsBias, HarmonicsConfig, HarmonicsLayer, HarmonicsSag,
    OutputConfig, PitchConfig, PitchQuality, RateTarget, TiltConfig, TiltPivot, UpsamplerConfig,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    /// A channel worker's thread: everything it does after its start hook
    /// counts in [`WORKER_COUNT`].
    static WORKER: Cell<bool> = const { Cell::new(false) };
}

/// Heap operations on channel worker threads since their start.
static WORKER_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn note() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
    let _ = WORKER.try_with(|worker| {
        if worker.get() {
            WORKER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

/// A pool whose workers count their heap operations.
fn counted_pool(workers: usize) -> std::sync::Arc<audivane_dsp_core::workers::LanePool> {
    let pool =
        audivane_dsp_core::workers::LanePool::spawn(workers, |_| WORKER.with(|w| w.set(true)))
            .unwrap();
    // Started: the lanes go to the workers, not inline.
    assert!(pool.wait_started(std::time::Duration::from_secs(10)));
    pool
}

/// [`assert_no_alloc`] on this thread and on every channel worker.
fn assert_no_alloc_with_workers<R>(what: &str, f: impl FnOnce() -> R) -> R {
    use std::sync::atomic::Ordering;
    let before = WORKER_COUNT.load(Ordering::SeqCst);
    let result = assert_no_alloc(what, f);
    let count = WORKER_COUNT.load(Ordering::SeqCst) - before;
    assert_eq!(
        count, 0,
        "{what}: {count} heap operations on channel workers"
    );
    result
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn assert_no_alloc<R>(what: &str, f: impl FnOnce() -> R) -> R {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let result = f();
    ARMED.with(|a| a.set(false));
    let count = COUNT.with(Cell::get);
    assert_eq!(
        count, 0,
        "{what}: {count} heap operations on the audio path"
    );
    result
}

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
}

/// Varying call sizes up to `max`, including 0 and 1.
fn sizes(max: usize, total: usize) -> Vec<usize> {
    let pattern = [max, 1, 0, max / 2 + 1, 37 % max.max(1), max - 1];
    let mut out = Vec::new();
    let mut sum = 0;
    let mut i = 0;
    while sum < total {
        let n = pattern[i % pattern.len()].min(max);
        out.push(n);
        sum += n;
        i += 1;
    }
    out
}

fn full_config(dither: DitherKind) -> ChainConfig {
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
                Band {
                    kind: FilterType::Peaking,
                    freq_hz: 1000.0,
                    gain_db: 3.0,
                    q: 1.0,
                    enabled: true,
                },
                Band {
                    kind: FilterType::LowShelf,
                    freq_hz: 100.0,
                    gain_db: -2.0,
                    q: 0.7,
                    enabled: true,
                },
            ],
            left: Some(ChannelEq {
                enabled: true,
                preamp_db: -1.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 250.0,
                    gain_db: -1.5,
                    q: 1.5,
                    enabled: true,
                }],
            }),
            right: Some(ChannelEq {
                enabled: true,
                preamp_db: -2.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 500.0,
                    gain_db: 2.0,
                    q: 1.0,
                    enabled: true,
                }],
            }),
            // A two-section tilt: bass below 1 kHz, treble above 4 kHz.
            tilt: Some(TiltConfig {
                enabled: true,
                slope_db_per_oct: Some(-0.7),
                total_db: None,
                start_hz: 20.0,
                end_hz: 1000.0,
                pivot: TiltPivot::End,
            }),
            tilt_high: Some(TiltConfig {
                enabled: true,
                slope_db_per_oct: Some(1.0),
                total_db: None,
                start_hz: 4000.0,
                end_hz: 20_000.0,
                pivot: TiltPivot::Start,
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
            target: RateTarget::Ratio(2),
            taps: 4096,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap(),
        pitch: PitchConfig {
            enabled: true,
            cents: -31.77,
            ..Default::default()
        },
        // Every order up to H9, so the whole term-by-term loop runs, a
        // tilted layer (the filtered loop), plus two flux layers.
        harmonics: Some(HarmonicsConfig {
            system: None,
            enabled: true,
            layers: vec![
                HarmonicsLayer::Chebyshev(ChebyshevLayer {
                    enabled: true,
                    preset: None,
                    levels_db: [
                        Some(-40.0),
                        Some(-58.0),
                        Some(-70.0),
                        Some(-75.0),
                        Some(-80.0),
                        Some(-85.0),
                        Some(-90.0),
                        Some(-95.0),
                    ],
                    signs: HARMONICS_DEFAULT_SIGNS,
                    invert_even: false,
                    drive_db: -3.0,
                    amount: 0.8,
                    tilt: None,
                    sag: None,
                    bias: None,
                }),
                // A layer with sag and bias (runs on its own).
                HarmonicsLayer::Chebyshev(ChebyshevLayer {
                    sag: Some(HarmonicsSag {
                        depth_db: 3.0,
                        attack_ms: 1.0,
                        release_ms: 20.0,
                    }),
                    bias: Some(HarmonicsBias {
                        depth: -0.1,
                        attack_ms: 5.0,
                        release_ms: 50.0,
                    }),
                    ..match HarmonicsConfig::from_preset("se_triode_high_mu")
                        .unwrap()
                        .layers
                        .remove(0)
                    {
                        HarmonicsLayer::Chebyshev(l) => l,
                        _ => unreachable!(),
                    }
                }),
                // A tilted layer with sag and bias (per-order filters in
                // its own run).
                HarmonicsLayer::Chebyshev(ChebyshevLayer {
                    sag: Some(HarmonicsSag {
                        depth_db: 2.0,
                        attack_ms: 1.0,
                        release_ms: 20.0,
                    }),
                    bias: Some(HarmonicsBias {
                        depth: 0.1,
                        attack_ms: 5.0,
                        release_ms: 50.0,
                    }),
                    ..match HarmonicsConfig::from_preset("pp_pentode_feedback")
                        .unwrap()
                        .layers
                        .remove(0)
                    {
                        HarmonicsLayer::Chebyshev(l) => l,
                        _ => unreachable!(),
                    }
                }),
                HarmonicsConfig::from_preset("solid_state_feedback")
                    .unwrap()
                    .layers[0]
                    .clone(),
                HarmonicsConfig::from_preset("output_transformer")
                    .unwrap()
                    .layers[0]
                    .clone(),
                HarmonicsConfig::from_preset("tape").unwrap().layers[0].clone(),
                HarmonicsConfig::from_preset("tape_studio").unwrap().layers[0].clone(),
            ],
            dc_block: true,
            routing: Default::default(),
        }),
        output: OutputConfig {
            bits: 16,
            dither,
            headroom_db: 1.5,
        },
        limiter: Default::default(),
    }
}

fn long_irs(channels: usize, taps: usize) -> Vec<Vec<f64>> {
    let mut seed = 3;
    (0..channels)
        .map(|_| {
            (0..taps)
                .map(|i| lcg(&mut seed) * (-(i as f64) / 20_000.0).exp() * 0.01)
                .collect()
        })
        .collect()
}

#[test]
fn chain_and_ab_chain_do_not_allocate() {
    let channels = 2;
    let max_block = 256;
    let spec = |dither| ChainSpec {
        config: full_config(dither),
        sample_rate: 48_000,
        channels,
        max_block,
        // Long enough for every non-uniform stage, including the capped one.
        irs: Some(long_irs(channels, 100_000).into()),
    };
    let mut chain = Chain::new(spec(DitherKind::Shaped));
    // Analytic K offset and auto headroom, as the daemon builds a pair.
    let level = audivane_dsp_core::ab_level::AbLevel::from_config(
        &audivane_protocol::ipc::LevelMatch::default(),
        audivane_protocol::ipc::AbHeadroom::Auto,
    );
    let mut b_config = full_config(DitherKind::Shaped);
    b_config.eq = None;
    // B runs the same harmonics layers in series.
    if let Some(h) = &mut b_config.harmonics {
        h.routing = audivane_protocol::HarmonicsRouting::Series;
    }
    let mut ab = AbChain::new(
        spec(DitherKind::Shaped),
        ChainSpec {
            config: b_config,
            ..spec(DitherKind::Shaped)
        },
        level,
    )
    .unwrap();
    let mut seed = 9;
    let input: Vec<f32> = (0..max_block * channels)
        .map(|_| (lcg(&mut seed) * 0.5) as f32)
        .collect();
    let mut out = vec![0i32; max_block * channels * 2];

    // ~2.2 s of audio: several periods of the largest convolution stage.
    let plan = sizes(max_block, 105_000);
    assert_no_alloc("Chain::process_f32", || {
        for &n in &plan {
            chain.process_f32(&input[..n * channels], &mut out);
        }
        chain.reset();
        for &n in &plan[..200] {
            chain.process_f32(&input[..n * channels], &mut out);
        }
    });
    assert_no_alloc("AbChain::process_f32", || {
        for (i, &n) in plan.iter().enumerate() {
            if i % 300 == 0 {
                let side = if (i / 300) % 2 == 0 { Side::B } else { Side::A };
                ab.select(side, 480);
            }
            if i == 450 {
                // A level change ramps on the audio thread.
                ab.set_level_offset_db(-1.0);
            }
            // A measured level match: start, a new epoch, stop.
            match i {
                100 => ab.set_measure_epoch(1),
                400 => ab.set_measure_epoch(2),
                800 => ab.set_measure_epoch(0),
                _ => {}
            }
            ab.process_f32(&input[..n * channels], &mut out);
            while ab.pop_measure_hop().is_some() {}
        }
        let _ = ab.overs();
        ab.reset();
    });
    // An output compare: B's own quantizer (16-bit, a fixed-coefficient
    // shaper) and the cut between the two quantized streams, with fades and
    // hard cuts landing inside blocks.
    let mut b_output = full_config(DitherKind::Resonant5);
    b_output.eq = None;
    b_output.output.bits = 16;
    let mut ab = AbChain::new(
        spec(DitherKind::Shaped),
        ChainSpec {
            config: b_output,
            ..spec(DitherKind::Shaped)
        },
        level,
    )
    .unwrap();
    assert!(ab.compares_output());
    assert_no_alloc("AbChain::process_f32 with two quantizers", || {
        for (i, &n) in plan.iter().enumerate() {
            if i % 100 == 0 {
                let side = if (i / 100) % 2 == 0 { Side::B } else { Side::A };
                ab.select(side, if (i / 100) % 3 == 0 { 0 } else { 333 });
            }
            ab.process_f32(&input[..n * channels], &mut out);
        }
        let _ = ab.overs();
        ab.reset();
        ab.process_f32(&input[..max_block * channels], &mut out);
    });
    // A NaN reaching the meter clears its filters on the audio thread.
    let mut meter = audivane_dsp_core::ab_measure::AbHopMeter::new(48_000.0, 2);
    meter.set_epoch(1);
    let clean = vec![vec![0.25; 4800]; 2];
    let mut bad = clean.clone();
    bad[1][100] = f64::NAN;
    assert_no_alloc("AbHopMeter with a NaN", || {
        for b in [&clean, &bad, &clean] {
            meter.push(&clean, b, 4800);
            while meter.pop().is_some() {}
        }
    });

    // The bit-exact bypass path too.
    let mut bypass = Chain::new(ChainSpec {
        config: ChainConfig::neutral(),
        sample_rate: 48_000,
        channels,
        max_block,
        irs: None,
    });
    assert_no_alloc("Chain bypass", || {
        for &n in &plan[..100] {
            bypass.process_f32(&input[..n * channels], &mut out);
        }
    });
}

/// The chains of [`chain_and_ab_chain_do_not_allocate`] on channel workers:
/// the handoff, the waits, the lanes, a late worker's silence and the
/// recovery reset allocate nothing, on the audio thread or the workers.
#[test]
fn chains_on_channel_workers_do_not_allocate() {
    let channels = 2;
    let max_block = 256;
    let workers = counted_pool(1);
    let spec = |config| ChainSpec {
        config,
        sample_rate: 48_000,
        channels,
        max_block,
        irs: Some(long_irs(channels, 100_000).into()),
    };
    let mut chain = Chain::new(spec(full_config(DitherKind::Shaped)));
    assert!(chain.attach_workers(&workers));
    let level = audivane_dsp_core::ab_level::AbLevel::from_config(
        &audivane_protocol::ipc::LevelMatch::default(),
        audivane_protocol::ipc::AbHeadroom::Auto,
    );
    let mut b_config = full_config(DitherKind::Resonant5);
    b_config.eq = None;
    // Series here.
    if let Some(h) = &mut b_config.harmonics {
        h.routing = audivane_protocol::HarmonicsRouting::Series;
    }
    b_config.output.bits = 16;
    let mut ab =
        AbChain::new(spec(full_config(DitherKind::Shaped)), spec(b_config), level).unwrap();
    assert!(ab.attach_workers(&workers));
    let mut seed = 19;
    let input: Vec<f32> = (0..max_block * channels)
        .map(|_| (lcg(&mut seed) * 0.5) as f32)
        .collect();
    let mut out = vec![0i32; max_block * channels * 2];
    let plan = sizes(max_block, 105_000);
    assert_no_alloc_with_workers("Chain on workers", || {
        for &n in &plan {
            chain.process_f32(&input[..n * channels], &mut out);
        }
        chain.reset();
        for &n in &plan[..200] {
            chain.process_f32(&input[..n * channels], &mut out);
        }
    });
    assert_no_alloc_with_workers("AbChain on workers", || {
        for (i, &n) in plan.iter().enumerate() {
            if i % 300 == 0 {
                let side = if (i / 300) % 2 == 0 { Side::B } else { Side::A };
                ab.select(side, 480);
            }
            ab.process_f32(&input[..n * channels], &mut out);
        }
        ab.reset();
    });
    // A late worker: silence, then the reset when its lane is back.
    workers.stall_next(0, std::time::Duration::from_millis(40));
    assert_no_alloc_with_workers("Chain with a late worker", || {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_millis(80) {
            chain.process_f32(&input[..max_block * channels], &mut out);
        }
        for _ in 0..50 {
            chain.process_f32(&input[..max_block * channels], &mut out);
        }
    });
    assert!(
        workers
            .stats()
            .late_blocks
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );
}

/// The live harmonics tap ([`Chain::set_harmonics_tap`]): the copies of
/// the stage's input and output, the sag/bias reads and the tap's view, on
/// one thread and on channel workers, in a single chain and chain A of a
/// pair, allocate nothing.
#[test]
fn harmonics_tap_does_not_allocate() {
    let channels = 2;
    let max_block = 256;
    let spec = |config| ChainSpec {
        config,
        sample_rate: 48_000,
        channels,
        max_block,
        irs: Some(long_irs(channels, 100_000).into()),
    };
    let level = audivane_dsp_core::ab_level::AbLevel::from_config(
        &audivane_protocol::ipc::LevelMatch::default(),
        audivane_protocol::ipc::AbHeadroom::Auto,
    );
    let mut seed = 29;
    let input: Vec<f32> = (0..max_block * channels)
        .map(|_| (lcg(&mut seed) * 0.5) as f32)
        .collect();
    let mut out = vec![0i32; max_block * channels * 2];
    let plan = sizes(max_block, 30_000);
    let workers = counted_pool(1);
    for on_workers in [false, true] {
        let mut chain = Chain::new(spec(full_config(DitherKind::Shaped)));
        let mut series = full_config(DitherKind::Shaped);
        if let Some(h) = &mut series.harmonics {
            h.routing = audivane_protocol::HarmonicsRouting::Series;
        }
        let mut ab =
            AbChain::new(spec(series), spec(full_config(DitherKind::Shaped)), level).unwrap();
        if on_workers {
            assert!(chain.attach_workers(&workers));
            assert!(ab.attach_workers(&workers));
        }
        assert!(chain.has_harmonics() && ab.has_harmonics());
        let mut seen = 0usize;
        assert_no_alloc_with_workers("harmonics tap", || {
            for (i, &n) in plan.iter().enumerate() {
                // On and off again mid-stream.
                let on = (i / 100) % 3 != 2;
                chain.set_harmonics_tap(on);
                ab.set_harmonics_tap(on);
                // The pair's tap follows the side playing.
                if i % 150 == 0 {
                    ab.select(if (i / 150) % 2 == 0 { Side::B } else { Side::A }, 480);
                }
                chain.process_f32(&input[..n * channels], &mut out);
                ab.process_f32(&input[..n * channels], &mut out);
                for tap in [chain.harmonics_tap(), ab.harmonics_tap()]
                    .into_iter()
                    .flatten()
                {
                    seen += tap.input.len() + tap.output.len() + tap.layers.len();
                }
            }
        });
        assert!(seen > 0);
    }
}

#[test]
fn stages_and_meters_do_not_allocate() {
    let max_block = 512;
    let mut seed = 5;
    let mut a: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 0.3).collect();
    let mut b = a.clone();

    let mut dc = DcBlock::try_new(&DcBlockConfig::default(), 48_000.0, 2).unwrap();
    let mut dc_next = DcBlock::try_new(&DcBlockConfig::default(), 48_000.0, 2).unwrap();
    let plan = sizes(max_block, 140_000);
    assert_no_alloc("DcBlock::process", || {
        for &n in &plan {
            dc.process(&mut [&mut a[..n], &mut b[..n]]);
        }
        assert!(dc_next.inherit_state(&dc));
        dc.reset();
    });

    let mut conv = Convolver::new(&long_irs(2, 200_000), 2, max_block);
    assert_no_alloc("Convolver::process", || {
        for &n in &plan {
            conv.process(&mut [&mut a[..n], &mut b[..n]]);
        }
        conv.reset();
    });

    let mut m = audivane_dsp_core::matrix::FilterMatrix::new(2);
    let taps = long_irs(2, 100_000);
    m.set(0, 0, taps[0].clone());
    m.set(0, 1, taps[1].clone());
    m.set(1, 1, taps[0].clone());
    let mut matrix = audivane_dsp_core::matrix::MatrixConvolver::new(&m, max_block);
    assert_no_alloc("MatrixConvolver::process", || {
        for &n in &plan {
            matrix.process(&mut [&mut a[..n], &mut b[..n]]);
        }
        matrix.reset();
    });

    let design = UpsamplerDesign {
        ratio: 4,
        taps: 8192,
        phase: FilterPhase::Minimum,
        kaiser_beta: 12.0,
        cutoff: 0.95,
        phase_mix: 1.0,
        preserve_samples: false,
        window: audivane_protocol::WindowKind::Kaiser,
        taper: 0.25,
    };
    let mut up = Upsampler::new(&design, 2, max_block);
    let mut o0 = vec![0.0; max_block * 4];
    let mut o1 = vec![0.0; max_block * 4];
    assert_no_alloc("Upsampler::process", || {
        for &n in &plan[..200] {
            up.process(
                &[&a[..n], &b[..n]],
                &mut [&mut o0[..n * 4], &mut o1[..n * 4]],
            );
        }
        up.reset();
    });

    // A long kernel: non-uniform partitions (four-step tail stages up to
    // 32768 frames), over a few periods of the largest stage, then the
    // channels split as the workers run them.
    let kernel = long_irs(1, 4 * 70_000).pop().unwrap();
    let mut up = Upsampler::from_kernel(&kernel, 4, 0, 2, max_block);
    assert!(up.stages().len() > 2, "{:?}", up.stages());
    assert_no_alloc("Upsampler::process (tail stages)", || {
        for &n in &plan {
            up.process(
                &[&a[..n], &b[..n]],
                &mut [&mut o0[..n * 4], &mut o1[..n * 4]],
            );
        }
        up.reset();
    });
    let mut lanes = up.split_channels();
    assert_no_alloc("split Upsampler::process (tail stages)", || {
        for &n in &plan[..400] {
            lanes[1].process(&[&b[..n]], &mut [&mut o1[..n * 4]]);
        }
        lanes[1].reset();
    });

    for kind in [
        DitherKind::None,
        DitherKind::Tpdf,
        DitherKind::Shaped,
        DitherKind::HeadroomTpdf,
        DitherKind::Rectangular,
        DitherKind::Gaussian,
        DitherKind::HighpassTpdf,
        DitherKind::Feedback1,
        DitherKind::Feedback5,
        DitherKind::Resonant5,
        DitherKind::Resonant9,
        DitherKind::Integrator15,
    ] {
        let mut q = Quantizer::with_sample_rate(
            &OutputConfig {
                bits: 24,
                dither: kind,
                headroom_db: 0.0,
            },
            2,
            1,
            96_000,
        );
        let mut out = vec![0i32; max_block * 2];
        assert_no_alloc("Quantizer::process", || {
            for &n in &plan[..200] {
                q.process(&[&a[..n], &b[..n]], &mut out[..n * 2]);
            }
            q.reset();
        });
    }

    // Two minutes of loudness metering: 1200 gating blocks.
    let mut lufs = LufsMeter::new(48_000.0, 2);
    let mut aw = AWeightedMeter::new(48_000.0, 2);
    let minute = sizes(max_block, 48_000 * 120);
    assert_no_alloc("LufsMeter::push", || {
        for &n in &minute {
            lufs.push(&[&a[..n], &b[..n]]);
        }
        let _ = lufs.integrated_lufs();
        lufs.reset();
    });
    assert_no_alloc("AWeightedMeter::push", || {
        for &n in &minute[..2000] {
            aw.push(&[&a[..n], &b[..n]]);
        }
        let _ = aw.rms_db();
        aw.reset();
    });
}

#[test]
fn volume_gain_on_the_direct_path_does_not_allocate() {
    use audivane_dsp_core::gain::StereoGainRamp;
    let input = vec![0.25_f32; 512 * 2];
    let mut out = vec![0_i32; 512 * 4];
    let mut ramp = StereoGainRamp::unity(48_000);
    ramp.set_volume_targets(-20.0, 3.0, false, false, false, true);
    let mut tpdf = audivane_dsp_core::dither::TpdfDither::new(1);
    assert_no_alloc("StereoGainRamp::process_f32_to_i32_repeat", || {
        ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 2, None);
        ramp.process_f32_to_i32_repeat(&input, &mut out[..512 * 2], 2, 1, None);
        ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 2, Some((&mut tpdf, 16)));
        ramp.process_f32_to_i32_repeat(&input, &mut out[..512 * 2], 2, 1, Some((&mut tpdf, 24)));
    });
}

#[test]
#[should_panic(expected = "heap operations on the audio path")]
fn detector_catches_an_allocation() {
    assert_no_alloc("Vec::with_capacity", || {
        std::hint::black_box(Vec::<f64>::with_capacity(64))
    });
}

#[test]
fn cascade_presets_do_not_allocate() {
    use audivane_dsp_core::design::{BuildContext, Named, upsampler};
    use audivane_dsp_core::presets::FilterPreset::*;

    let max_block = 257;
    let mut seed = 23;
    let left: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 0.2).collect();
    let right: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 0.2).collect();
    let plan = sizes(max_block, 4000);
    for preset in [
        CascadeElliptic,
        CascadeInverseChebyshev,
        CascadeFir,
        CascadeFirAsym,
        CascadeFirMp,
        CascadeFft,
    ] {
        for ratio in [2, 6, 18, 32, 64] {
            let cx = BuildContext {
                source_rate: 44_100,
                ratio: audivane_protocol::RateRatio::integer(ratio as u32),
                channels: 2,
                max_block,
                pipelines: 2,
                output: Default::default(),
                workspace: audivane_dsp_core::design::budget::DESIGN_WORKSPACE_FLOOR,
            };
            let mut up = upsampler(&Named::preset(preset), &cx, 0.0)
                .unwrap()
                .unwrap();
            let mut o0 = vec![0.0; max_block * ratio];
            let mut o1 = vec![0.0; max_block * ratio];
            assert_no_alloc(preset.name(), || {
                up.scale_output(1.0);
                for &n in &plan {
                    up.process(
                        &[&left[..n], &right[..n]],
                        &mut [&mut o0[..n * ratio], &mut o1[..n * ratio]],
                    );
                }
                up.reset();
                up.process(
                    &[&left[..1], &right[..1]],
                    &mut [&mut o0[..ratio], &mut o1[..ratio]],
                );
                up.reset();
            });
        }
    }
}

#[test]
fn rational_chains_do_not_allocate() {
    use audivane_dsp_core::rational::RationalResampler;
    use audivane_protocol::{RateRatio, TapsUnit};

    let channels = 2;
    let max_block = 300;
    let mut seed = 31;
    let input: Vec<f32> = (0..max_block * channels)
        .map(|_| (lcg(&mut seed) * 0.5) as f32)
        .collect();
    let plan = sizes(max_block, 40_000);
    // Every stage (pitch, crossfeed, EQ with a two-section tilt, convolution, harmonics
    // at the output rate, the limiter and the noise shapers at the real
    // output rate) at fractional and decimating ratios.
    for (rate, target, dither) in [
        (44_100, RateTarget::Rate(48_000), DitherKind::Shaped),
        (48_000, RateTarget::Rate(44_100), DitherKind::Resonant9),
        (96_000, RateTarget::Rate(48_000), DitherKind::Integrator15),
        (192_000, RateTarget::Rate(44_100), DitherKind::Feedback5),
    ] {
        let mut config = full_config(dither);
        config.limiter = audivane_protocol::LimiterConfig {
            mode: audivane_protocol::LimiterMode::AutoHeadroom,
            ..Default::default()
        };
        config.upsampler = audivane_dsp_core::resample::ManualConfig {
            target,
            taps: 64,
            taps_unit: TapsUnit::Input,
            phase: FilterPhase::Minimum,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let spec = || ChainSpec {
            config: config.clone(),
            sample_rate: rate,
            channels,
            max_block,
            irs: Some(long_irs(channels, 5_000).into()),
        };
        let mut chain = Chain::new(spec());
        let mut ab = AbChain::new(
            spec(),
            spec(),
            audivane_dsp_core::ab_level::AbLevel::fixed(-0.5),
        )
        .unwrap();
        let mut out = vec![0i32; chain.max_output_frames() * channels];
        assert_no_alloc("rational Chain::process_f32", || {
            for &n in &plan {
                let _ = chain.output_frames(n);
                chain.process_f32(&input[..n * channels], &mut out);
            }
            chain.reset_to(17);
        });
        assert_no_alloc("rational AbChain::process_f32", || {
            for (i, &n) in plan.iter().enumerate() {
                if i % 50 == 0 {
                    ab.select(if i % 100 == 0 { Side::B } else { Side::A }, 100);
                }
                ab.process_f32(&input[..n * channels], &mut out);
            }
            ab.reset_to(5);
        });
    }

    let ratio = RateRatio::new(160, 147);
    let kernel: Vec<f64> = (0..160 * 31).map(|_| lcg(&mut seed)).collect();
    for ch in [1usize, 2, 3] {
        let mut rs = RationalResampler::from_kernel(&kernel, ratio, 0.0, ch, max_block);
        let x: Vec<Vec<f64>> = (0..ch).map(|_| vec![0.1; max_block]).collect();
        let mut y: Vec<Vec<f64>> = (0..ch)
            .map(|_| vec![0.0; ratio.max_frames_out(max_block)])
            .collect();
        assert_no_alloc("RationalResampler::process", || {
            for &n in &plan {
                let k = rs.output_frames(n);
                let ins: [&[f64]; 3] =
                    std::array::from_fn(|c| if c < ch { &x[c][..n] } else { &[] });
                let mut it = y.iter_mut();
                let mut outs: [&mut [f64]; 3] = std::array::from_fn(|_| match it.next() {
                    Some(v) => &mut v[..k],
                    None => &mut [],
                });
                rs.process(&ins[..ch], &mut outs[..ch]);
            }
            rs.reset_to(1);
        });
    }
}

#[test]
fn pitch_shifter_does_not_allocate() {
    let max_block = 512;
    let mut seed = 99;
    let mut l: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 0.25).collect();
    let mut r: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 0.25).collect();
    let plan = sizes(max_block, 16_384);

    for quality in [PitchQuality::Low, PitchQuality::High, PitchQuality::Max] {
        let mut shifter = PitchShifter::try_new(48_000, 2, max_block, -31.77, quality).unwrap();
        assert_no_alloc("PitchShifter::process", || {
            for &n in &plan {
                shifter.process(&mut [&mut l[..n], &mut r[..n]]);
            }
            shifter.set_cents(100.0).unwrap();
            shifter.process(&mut [&mut l[..64], &mut r[..64]]);
            shifter.reset();
            shifter.process(&mut [&mut l[..1], &mut r[..1]]);
        });
    }
}

#[test]
fn limiter_and_post_volume_chain_do_not_allocate() {
    use audivane_dsp_core::limiter::Limiter;
    use audivane_protocol::{LimiterConfig, LimiterMode};
    let mut left = vec![1.5; 257];
    let mut right = vec![-0.5; 257];
    for ratio in [1, 2, 4, 8, 16, 32, 64] {
        for mode in [LimiterMode::Limit, LimiterMode::AutoHeadroom] {
            let config = LimiterConfig {
                mode,
                ..Default::default()
            };
            let mut limiter = Limiter::new(&config, 48_000 * ratio, 2).unwrap();
            assert_no_alloc("Limiter::process/reset/status", || {
                for n in [0, 1, 257, 63, 0, 17] {
                    limiter.process(&mut [&mut left[..n], &mut right[..n]]);
                    std::hint::black_box(limiter.take_status());
                }
                limiter.reset();
            });
        }
    }
    // Volume changes and a headroom carry-over.
    let config = LimiterConfig {
        mode: LimiterMode::AutoHeadroom,
        ..Default::default()
    };
    let mut old = Limiter::new(&config, 48_000, 2).unwrap();
    let mut new = Limiter::new(&config, 48_000, 2).unwrap();
    let mut l = vec![1.5; 4800];
    let mut r = vec![1.5; 4800];
    old.process(&mut [&mut l, &mut r]);
    l.fill(0.1);
    r.fill(0.1);
    assert_no_alloc("Limiter volume changes and inherit_headroom", || {
        for step in 0..150 {
            old.set_volume_gain(if step % 2 == 0 { 0.5 } else { 1.0 });
            old.process(&mut [&mut l, &mut r]);
        }
        assert!(new.inherit_headroom(&old));
        std::hint::black_box(new.take_status());
    });
    let config = ChainConfig {
        limiter: LimiterConfig {
            mode: LimiterMode::AutoHeadroom,
            ..Default::default()
        },
        ..ChainConfig::neutral()
    };
    let spec = ChainSpec {
        config,
        sample_rate: 48_000,
        channels: 2,
        max_block: 257,
        irs: None,
    };
    let mut chain = Chain::new(spec.clone());
    let mut ab = AbChain::new(
        spec.clone(),
        spec,
        audivane_dsp_core::ab_level::AbLevel::fixed(6.0),
    )
    .unwrap();
    // Limit mode with a hot B: onsets and quantizer clips are attributed
    // to the sides through the limiter's delay.
    let limit = |preamp_db: f64| ChainSpec {
        config: ChainConfig {
            limiter: LimiterConfig {
                mode: LimiterMode::Limit,
                ..Default::default()
            },
            eq: Some(audivane_protocol::EqProfile {
                preamp_db,
                ..Default::default()
            }),
            ..Default::default()
        },
        sample_rate: 48_000,
        channels: 2,
        max_block: 257,
        irs: None,
    };
    let mut limited_ab = AbChain::new(
        limit(0.0),
        limit(6.0),
        audivane_dsp_core::ab_level::AbLevel::fixed(0.0),
    )
    .unwrap();
    let input = vec![1.5_f32; 514];
    let pulses: Vec<f32> = (0..514)
        .map(|i| if i % 90 < 2 { 0.8 } else { 0.0 })
        .collect();
    let mut output = vec![0; 514];
    assert_no_alloc("limited Chain/AbChain", || {
        for n in [0, 1, 257, 63, 17] {
            chain.process_f32(&input[..n * 2], &mut output[..n * 2]);
            ab.process_f32(&input[..n * 2], &mut output[..n * 2]);
        }
        for (k, n) in [257, 1, 63, 257, 17, 200, 257].into_iter().enumerate() {
            if k % 2 == 1 {
                limited_ab.select(if k % 4 == 1 { Side::B } else { Side::A }, 96);
            }
            limited_ab.process_f32(&pulses[..n * 2], &mut output[..n * 2]);
        }
        std::hint::black_box(limited_ab.overs());
        chain.reset();
        ab.reset();
        limited_ab.reset();
    });
}

#[test]
fn automatically_designed_chain_does_not_allocate() {
    let mut config = ChainConfig {
        upsampler: audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(2),
            phase: FilterPhase::Intermediate,
            phase_mix: 0.5,
            auto_taps: Some(audivane_protocol::FilterErrorBudget::MatchOutput),
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap(),
        ..ChainConfig::neutral()
    };
    config.output.bits = 16;
    let mut chain = Chain::try_new(ChainSpec {
        config,
        sample_rate: 44_100,
        channels: 1,
        max_block: 257,
        irs: None,
    })
    .unwrap();
    let input = [0.25_f32; 257];
    let mut out = [0_i32; 514];
    let plan = sizes(257, 4000);
    assert_no_alloc("automatic kernel process/reset", || {
        for &n in &plan {
            chain.process_f32(&input[..n], &mut out);
        }
        chain.reset();
    });
}

/// The top ratio, 44.1 kHz x64 = 2.8224 MHz: the full chain (every stage
/// after the upsampler runs at the output rate, the limiter included), A/B,
/// and the presets of each runtime kind, at any block size.
#[test]
fn x64_chains_do_not_allocate() {
    use audivane_dsp_core::presets::FilterPreset;
    use audivane_protocol::{LimiterConfig, LimiterMode};

    let channels = 2;
    let max_block = 512;
    let mut seed = 31;
    let input: Vec<f32> = (0..max_block * channels)
        .map(|_| (lcg(&mut seed) * 0.5) as f32)
        .collect();
    let plan = sizes(max_block, 8_000);
    let spec = |upsampler: UpsamplerConfig| {
        let mut config = full_config(DitherKind::Shaped);
        config.upsampler = upsampler;
        config.limiter = LimiterConfig {
            mode: LimiterMode::Limit,
            ..Default::default()
        };
        ChainSpec {
            config,
            sample_rate: 44_100,
            channels,
            max_block,
            irs: Some(long_irs(channels, 4_096).into()),
        }
    };
    let manual = audivane_dsp_core::resample::ManualConfig {
        target: RateTarget::Ratio(64),
        taps: 65_536,
        ..Default::default()
    }
    .into_upsampler_config()
    .unwrap();
    let mut chain = Chain::try_new(spec(manual.clone())).unwrap();
    assert_eq!(chain.ratio().as_integer(), Some(64));
    let mut out = vec![0i32; chain.max_output_frames() * channels];
    assert_no_alloc("x64 Chain::process_f32", || {
        for &n in &plan {
            assert_eq!(chain.output_frames(n), n * 64);
            chain.process_f32(&input[..n * channels], &mut out);
        }
        chain.reset();
    });
    let mut ab = AbChain::new(
        spec(manual.clone()),
        spec(manual),
        audivane_dsp_core::ab_level::AbLevel::fixed(-0.5),
    )
    .unwrap();
    assert_no_alloc("x64 AbChain::process_f32", || {
        for (i, &n) in plan.iter().enumerate() {
            if i % 40 == 0 {
                ab.select(if (i / 40) % 2 == 0 { Side::B } else { Side::A }, 4_800);
            }
            ab.process_f32(&input[..n * channels], &mut out);
        }
        ab.reset();
    });

    for preset in [
        FilterPreset::GaussianLong,
        FilterPreset::KaiserSteepMp,
        FilterPreset::CascadeFir,
        FilterPreset::BlockFftFast,
        FilterPreset::Nos,
        FilterPreset::CatmullRom,
    ] {
        let mut chain = Chain::try_new(spec(UpsamplerConfig {
            target: RateTarget::MaxRate(audivane_protocol::MAX_OUTPUT_RATE),
            preset: Some(preset.name().to_string()),
            ..Default::default()
        }))
        .unwrap_or_else(|e| panic!("{} at x64: {e}", preset.name()));
        assert_eq!(chain.ratio().as_integer(), Some(64), "{}", preset.name());
        let mut out = vec![0i32; chain.max_output_frames() * channels];
        assert_no_alloc(preset.name(), || {
            for &n in &plan {
                chain.process_f32(&input[..n * channels], &mut out);
            }
            chain.reset();
        });
    }
}

/// The adaptive core: `plan`, `process` and `reset`, with jumps, ramps
/// (pieces), holds, zero calls, abandoned and refused plans between calls.
#[test]
fn adaptive_core_does_not_allocate() {
    let max_out = 16_384;
    for channels in [1usize, 2, 3] {
        let mut core = AdaptiveCore::new(channels, max_out, Step::ONE).unwrap();
        let mut seed = 31;
        let len = core.max_in();
        let input: Vec<Vec<f64>> = (0..channels)
            .map(|_| (0..len).map(|_| lcg(&mut seed)).collect())
            .collect();
        let mut out = vec![vec![0.0; max_out]; channels];
        let plan = sizes(max_out, 200_000);
        let s = |v: f64| Step::from_f64(v).unwrap();
        assert_no_alloc("AdaptiveCore", || {
            for (i, &n) in plan.iter().enumerate() {
                let seg = match i % 6 {
                    0 => Segment::Jump(s(0.5 + 1.5 * ((i * 37) % 100) as f64 / 100.0)),
                    1 | 2 => Segment::Piece {
                        slope0: 0.0,
                        end: s(1.0 + 0.5 * ((i * 13) % 10) as f64 / 10.0),
                        slope1: 0.0,
                    },
                    3 => {
                        // abandoned, then refused, then the real one
                        let _ = core.plan(n / 2, Segment::Jump(s(2.0)));
                        let _ = core.plan(max_out + 1, Segment::Hold);
                        Segment::Hold
                    }
                    _ => Segment::Hold,
                };
                let need = core.plan(n, seg).unwrap();
                let n_in = need + usize::from(need > 0 && i % 2 == 0);
                let mut ins = [&input[0][..0]; 3];
                for (c, x) in input.iter().enumerate() {
                    ins[c] = &x[..n_in];
                }
                let mut outs: [&mut [f64]; 3] = [&mut [], &mut [], &mut []];
                for (o, b) in outs.iter_mut().zip(out.iter_mut()) {
                    *o = &mut b[..n];
                }
                core.process(&ins[..channels], &mut outs[..channels]);
                if i % 50 == 49 {
                    core.reset(s(0.75));
                }
            }
        });
    }
}

/// The varispeed stage: `plan`, `process`, `reset`, `max_source_frames`
/// and the commands (speeds with ramps, retargets mid-ramp, speed limits),
/// with zero calls, abandoned plans and every call size up to the largest.
#[test]
fn varispeed_stage_does_not_allocate() {
    // Plain, and in follower mode (ε on every step).
    for (channels, follower) in [(1usize, false), (2, false), (3, false), (2, true)] {
        let mut vs = Varispeed::new(VarispeedSpec {
            sample_rate: 48_000,
            channels,
            max_frames: MAX_FRAMES,
            speed: 1.0,
        })
        .unwrap();
        vs.set_follower(follower);
        let mut seed = 41;
        let input: Vec<Vec<f64>> = (0..channels)
            .map(|_| (0..2 * MAX_FRAMES + 1).map(|_| lcg(&mut seed)).collect())
            .collect();
        let mut out = vec![vec![0.0; MAX_FRAMES]; channels];
        let plan = sizes(MAX_FRAMES, 400_000);
        assert_no_alloc("Varispeed", || {
            for (i, &n) in plan.iter().enumerate() {
                match i % 7 {
                    0 => vs
                        .set_speed(
                            0.5 + ((i * 37) % 150) as f64 / 100.0,
                            [0.0, 50.0, 400.0][i % 3],
                        )
                        .unwrap(),
                    3 => vs.set_speed_limit(if i % 2 == 0 { 1.3 } else { f64::INFINITY }),
                    5 => {
                        let _ = vs.plan(n / 2); // abandoned
                        let _ = vs.plan(MAX_FRAMES + 1); // refused
                    }
                    _ => {}
                }
                vs.set_epsilon_target(1e-4 * ((i % 13) as f64 - 6.0), 1e-6);
                let bound = vs.max_source_frames(n);
                let n_src = vs.plan(n).unwrap();
                assert!(n_src <= bound);
                let mut ins = [&input[0][..0]; 3];
                for (c, x) in input.iter().enumerate() {
                    ins[c] = &x[..n_src];
                }
                let mut outs: [&mut [f64]; 3] = [&mut [], &mut [], &mut []];
                for (o, b) in outs.iter_mut().zip(out.iter_mut()) {
                    *o = &mut b[..n];
                }
                vs.process(&ins[..channels], &mut outs[..channels]);
                if i % 61 == 60 {
                    vs.reset();
                }
            }
        });
        assert_eq!(vs.fallbacks(), 0);
    }
}

/// The tempo stage: `plan`, `process`, `reset`, `max_source_frames`,
/// `peak_until_settled` and the commands (speeds with ramps, retargets
/// mid-ramp, cap drops under the applied speed), with zero calls,
/// abandoned plans and every call size up to the largest, at 1 to 3
/// channels and at a rate where the geometry steps down.
#[test]
fn tempo_stage_does_not_allocate() {
    for (rate, channels) in [(48_000, 1usize), (48_000, 2), (384_000, 3)] {
        let mut st = TempoStretch::new(TempoSpec {
            sample_rate: rate,
            channels,
            max_frames: MAX_FRAMES,
            speed: 1.0,
        })
        .unwrap();
        let mut seed = 43;
        let input: Vec<Vec<f64>> = (0..channels)
            .map(|_| (0..2 * MAX_FRAMES + 1).map(|_| lcg(&mut seed)).collect())
            .collect();
        let mut out = vec![vec![0.0; MAX_FRAMES]; channels];
        let plan = sizes(MAX_FRAMES, 300_000);
        assert_no_alloc("TempoStretch", || {
            for (i, &n) in plan.iter().enumerate() {
                match i % 7 {
                    0 => st
                        .set_speed(
                            0.5 + ((i * 37) % 150) as f64 / 100.0,
                            [0.0, 50.0, 400.0][i % 3],
                        )
                        .unwrap(),
                    3 => st.set_speed_limit(if i % 2 == 0 { 0.9 } else { f64::INFINITY }),
                    5 => {
                        let _ = st.plan(n / 2); // abandoned
                        let _ = st.plan(MAX_FRAMES + 1); // refused
                    }
                    _ => {}
                }
                let _ = st.peak_until_settled();
                let bound = st.max_source_frames(n);
                let n_src = st.plan(n).unwrap();
                assert!(n_src <= bound);
                let mut ins = [&input[0][..0]; 3];
                for (c, x) in input.iter().enumerate() {
                    ins[c] = &x[..n_src];
                }
                let mut outs: [&mut [f64]; 3] = [&mut [], &mut [], &mut []];
                for (o, b) in outs.iter_mut().zip(out.iter_mut()) {
                    *o = &mut b[..n];
                }
                st.process(&ins[..channels], &mut outs[..channels]);
                if i % 61 == 60 {
                    st.reset();
                }
            }
        });
        assert_eq!(st.fallbacks(), 0);
    }
}

/// The drift estimator's per-period calls: the maps, the detector, the
/// loop (with skips, clamps and holds), the pieces and the rate estimate.
#[test]
fn drift_estimator_does_not_allocate() {
    let mut mm = PositionMap::new();
    let mut fm = PositionMap::new();
    let mut det = PhaseDetector::new(48_000.0);
    let mut lp = DriftLoop::new(LoopConfig::default(), 48_000.0, 0.05).unwrap();
    let mut track = EpsilonTrack::new(96_000.0);
    let mut est = RateEstimator::new();
    let mut seed = 5;
    assert_no_alloc("drift", || {
        for k in 0..5_000u64 {
            let w = k * 1024;
            mm.push(w, i128::from(w) * FRAME);
            mm.push(w + 1023, i128::from(w + 1023) * FRAME);
            fm.push(w, i128::from(w + 7) * FRAME);
            fm.push(w + 1023, i128::from(w + 1030) * FRAME);
            let t = (k as i64) * 21_333_333;
            let m = Reading {
                written: w + 1024,
                delay: 2000,
                time_ns: t,
            };
            det.master(m, &mm);
            let f = Reading {
                written: w + 1024,
                delay: 2000 + (lcg(&mut seed) * 30.0) as i64,
                time_ns: t + 100_000,
            };
            let e = det.follower(f, &fm).unwrap_or(f64::NAN);
            let e = if k % 997 == 0 { e + 5000.0 } else { e };
            let from = track.state();
            let c = lp.update(e, 0.021_333);
            let seg = track
                .piece(Segment::Hold, Step::ONE, (c.eps, c.eps_slope))
                .unwrap_or(Segment::Hold);
            let _ = seg;
            est.push(e, 48_000.0);
            est.advance(0.021_333, from, track.state());
            let _ = est.ppm(48_000.0);
            if k % 1500 == 1499 {
                lp.reset();
                fm.clear();
                det.reset();
                est.reset();
            }
        }
    });
}

#[test]
fn tube_dynamics_does_not_allocate() {
    use audivane_dsp_core::tube_dynamics::{
        BiasConfig, SagConfig, TubeDynamics, TubeDynamicsConfig,
    };

    let max_block = 512;
    let mut seed = 131;
    let mut l: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 1.1).collect();
    let mut r: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 1.1).collect();
    let plan = sizes(max_block, 100_000);
    let layer = ChebyshevLayer {
        enabled: true,
        preset: None,
        levels_db: [
            Some(-30.0),
            Some(-40.0),
            Some(-50.0),
            None,
            None,
            None,
            None,
            Some(-80.0),
        ],
        signs: HARMONICS_DEFAULT_SIGNS,
        invert_even: false,
        drive_db: 0.0,
        amount: 1.0,
        tilt: None,
        sag: None,
        bias: None,
    };
    for (sag, bias) in [
        (SagConfig::default(), BiasConfig::default()),
        (
            SagConfig::default(),
            BiasConfig {
                depth: 0.0,
                ..BiasConfig::default()
            },
        ),
    ] {
        // Untilted, and tilted (per-order filters).
        let tilted = ChebyshevLayer {
            tilt: Some(Box::new(audivane_protocol::HarmonicsTilt {
                low: audivane_protocol::TiltShelf::new(80.0, 12.0),
                high: audivane_protocol::TiltShelf::new(4_000.0, 6.0),
                low_even: true,
                ..Default::default()
            })),
            ..layer.clone()
        };
        for (curve, rate) in [&layer, &tilted]
            .into_iter()
            .flat_map(|c| [44_100.0, 192_000.0, 768_000.0].map(|r| (c, r)))
        {
            let cfg = TubeDynamicsConfig::from_layer(curve, true, sag, bias).unwrap();
            let mut st = TubeDynamics::try_new(&cfg, rate, 2).unwrap();
            assert_no_alloc("TubeDynamics::process/reset", || {
                for &n in &plan {
                    st.process(&mut [&mut l[..n], &mut r[..n]]);
                }
                st.reset();
                st.process(&mut [&mut l[..1], &mut r[..1]]);
            });
        }
    }
}
