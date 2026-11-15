//! Every real-time entry point of dsp-core (`process`, `push`, `reset`,
//! `select`) must not touch the heap. A counting global allocator, armed
//! per thread around the calls under test, enforces it.

use audivane_dsp_core::Stage;
use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec, Side};
use audivane_dsp_core::conv::Convolver;
use audivane_dsp_core::dither::Quantizer;
use audivane_dsp_core::loudness::{AWeightedMeter, LufsMeter};
use audivane_dsp_core::resample::{Upsampler, UpsamplerDesign};
use audivane_protocol::{
    Band, ChainConfig, ChannelEq, ConvolutionConfig, DitherKind, EqProfile, FilterPhase,
    FilterType, OutputConfig, RateTarget, UpsamplerConfig,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
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
            ..Default::default()
        }),
        convolution: vec![ConvolutionConfig {
            name: Some("test-ir".into()),
            enabled: true,
            ir_path: "unused.wav".into(),
            gain_db: -1.0,
        }],
        upsampler: UpsamplerConfig {
            target: RateTarget::Ratio(2),
            taps: 4096,
            ..Default::default()
        },
        output: OutputConfig { bits: 16, dither },
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
        irs: Some(long_irs(channels, 100_000)),
    };
    let mut chain = Chain::new(spec(DitherKind::Shaped));
    let mut ab = AbChain::new(spec(DitherKind::Shaped), spec(DitherKind::Shaped), -0.5).unwrap();
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
            ab.process_f32(&input[..n * channels], &mut out);
        }
        ab.set_level_offset_db(-1.0);
        ab.reset();
    });

    // The bit-exact bypass path too.
    let mut bypass = Chain::new(ChainSpec {
        config: ChainConfig::default(),
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

#[test]
fn stages_and_meters_do_not_allocate() {
    let max_block = 512;
    let mut seed = 5;
    let mut a: Vec<f64> = (0..max_block).map(|_| lcg(&mut seed) * 0.3).collect();
    let mut b = a.clone();

    let mut conv = Convolver::new(&long_irs(2, 200_000), 2, max_block);
    let plan = sizes(max_block, 140_000);
    assert_no_alloc("Convolver::process", || {
        for &n in &plan {
            conv.process(&mut [&mut a[..n], &mut b[..n]]);
        }
        conv.reset();
    });

    let design = UpsamplerDesign {
        ratio: 4,
        taps: 8192,
        phase: FilterPhase::Minimum,
        kaiser_beta: 12.0,
        cutoff: 0.95,
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

    for kind in [DitherKind::None, DitherKind::Tpdf, DitherKind::Shaped] {
        let mut q = Quantizer::with_sample_rate(
            &OutputConfig {
                bits: 24,
                dither: kind,
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
#[should_panic(expected = "heap operations on the audio path")]
fn detector_catches_an_allocation() {
    assert_no_alloc("Vec::with_capacity", || {
        std::hint::black_box(Vec::<f64>::with_capacity(64))
    });
}
