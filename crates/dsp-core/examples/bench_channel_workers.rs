//! Process time per block of a heavy chain, on the audio thread alone and
//! with channel workers ([`audivane_dsp_core::chain::Chain::attach_workers`]),
//! and a check that both outputs are bit for bit the same.
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example bench_channel_workers -- \
//!     [ratio_or_rate] [kernel_taps] [ir_taps] [seconds] [block] [ab] [separate]
//! ```
//!
//! `ratio_or_rate`: an integer ratio (16, 1 for none) or an output rate in
//! Hz (768000).
//! Defaults: 16 1048576 0 20 1024 0 1, from 44.1 kHz, stereo, EQ and a manual
//! linear-phase kernel; `ir_taps` > 0 adds stereo convolution, `ab` 1 runs
//! an A/B pair (B with a shorter kernel); `separate` 0 alternates the two
//! chains block by block instead of running one after the other. Prints mean, 99th percentile and
//! max time per block and the load (time over the block's duration).

use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec};
use audivane_dsp_core::gain::StereoGainRamp;
use audivane_dsp_core::workers::LanePool;
use audivane_protocol::{
    Band, ChainConfig, ConvolutionConfig, DitherKind, EqProfile, FilterType, OutputConfig,
    RateTarget,
};
use std::sync::Arc;
use std::time::Instant;

const RATE: u32 = 44_100;

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
}

enum Runner {
    Single(Box<Chain>),
    Ab(Box<AbChain>),
}

impl Runner {
    fn attach(&mut self, pool: &Arc<LanePool>) -> bool {
        match self {
            Self::Single(c) => c.attach_workers(pool),
            Self::Ab(c) => c.attach_workers(pool),
        }
    }
    fn out_frames(&self, n: usize) -> usize {
        match self {
            Self::Single(c) => c.output_frames(n),
            Self::Ab(c) => c.output_frames(n),
        }
    }
    fn run(&mut self, x: &[f32], out: &mut [i32], gain: &mut StereoGainRamp) {
        match self {
            Self::Single(c) => {
                c.process_f32_with_gain(x, out, gain);
            }
            Self::Ab(c) => {
                c.process_f32_with_gain(x, out, gain);
            }
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, d: u64| a.get(i).map_or(d, |s| s.parse().expect("number"));
    let target = match arg(0, 16) {
        1 => RateTarget::Off,
        r if r <= 256 => RateTarget::Ratio(r as u32),
        hz => RateTarget::Rate(hz as u32),
    };
    let taps = arg(1, 1 << 20) as usize;
    let ir_taps = arg(2, 0) as usize;
    let secs = arg(3, 20) as usize;
    let block = arg(4, 1024) as usize;
    let ab = arg(5, 0) != 0;
    // 0: the two chains block by block, alternating; 1: all blocks of the
    // single-threaded chain, then all of the other (each thread's duty cycle
    // as in playback, which the CPU's frequency governor follows).
    let separate = arg(6, 1) != 0;

    let config = |taps: usize| ChainConfig {
        eq: Some(EqProfile {
            name: "bench".into(),
            preamp_db: -3.0,
            bands: (0..10)
                .map(|i| Band {
                    kind: FilterType::Peaking,
                    freq_hz: 40.0 * 2f64.powi(i),
                    gain_db: if i % 2 == 0 { 2.0 } else { -2.0 },
                    q: 1.0,
                    enabled: true,
                })
                .collect(),
            ..Default::default()
        }),
        convolution: if ir_taps > 0 {
            vec![ConvolutionConfig {
                name: Some("bench".into()),
                enabled: true,
                ir_path: "unused.wav".into(),
                gain_db: 0.0,
                matrix: None,
                calibration: false,
            }]
        } else {
            Vec::new()
        },
        upsampler: {
            let mut u = audivane_dsp_core::resample::ManualConfig {
                target: if target == RateTarget::Off {
                    RateTarget::Ratio(2)
                } else {
                    target
                },
                taps,
                ..Default::default()
            }
            .into_upsampler_config()
            .expect("upsampler");
            u.target = target;
            u
        },
        output: OutputConfig {
            bits: 24,
            dither: DitherKind::Tpdf,
            headroom_db: 1.0,
        },
        ..ChainConfig::neutral()
    };
    let irs = (ir_taps > 0).then(|| {
        let mut seed = 5;
        (0..2)
            .map(|_| {
                (0..ir_taps)
                    .map(|i| lcg(&mut seed) * (-(i as f64) / (ir_taps as f64 / 6.0)).exp() * 0.01)
                    .collect::<Vec<f64>>()
            })
            .collect::<Vec<_>>()
    });
    let spec = |taps| ChainSpec {
        config: config(taps),
        sample_rate: RATE,
        channels: 2,
        max_block: block,
        irs: irs.clone().map(Into::into),
    };
    let build = || {
        if ab {
            let level = audivane_dsp_core::ab_level::AbLevel::from_config(
                &audivane_protocol::ipc::LevelMatch::default(),
                audivane_protocol::ipc::AbHeadroom::Auto,
            );
            Runner::Ab(Box::new(
                AbChain::new(spec(taps), spec((taps / 4).max(64)), level).expect("A/B"),
            ))
        } else {
            Runner::Single(Box::new(Chain::try_new(spec(taps)).expect("chain")))
        }
    };

    let t0 = Instant::now();
    let mut single = build();
    let mut multi = build();
    println!("built two chains in {:.1} s", t0.elapsed().as_secs_f64());
    let pool = LanePool::spawn(1, |_| {}).unwrap();
    assert!(pool.wait_started(std::time::Duration::from_secs(10)));
    assert!(multi.attach(&pool), "the chain splits");

    let mut seed = 11;
    let x: Vec<f32> = (0..block * 2 * 64)
        .map(|_| (lcg(&mut seed) * 0.4) as f32)
        .collect();
    let max_out = single.out_frames(block) + 2;
    let (mut o1, mut o2) = (vec![0i32; max_out * 2], vec![0i32; max_out * 2]);
    let blocks = RATE as usize * secs / block;
    let block_s = block as f64 / f64::from(RATE);
    let mut g1 = StereoGainRamp::unity(RATE);
    let mut g2 = StereoGainRamp::unity(RATE);
    let (mut t1, mut t2) = (Vec::with_capacity(blocks), Vec::with_capacity(blocks));
    let mut same = true;
    let mut out1: Vec<i32> = Vec::new();
    for i in 0..blocks {
        let k = i % 64;
        let input = &x[k * block * 2..(k + 1) * block * 2];
        let n = single.out_frames(block) * 2;
        // Alternate which runs first, so neither always finds a warm cache.
        let (r1, r2) = if separate {
            let a = Instant::now();
            single.run(input, &mut o1, &mut g1);
            (a.elapsed().as_secs_f64(), 0.0)
        } else if i % 2 == 0 {
            let a = Instant::now();
            single.run(input, &mut o1, &mut g1);
            let ta = a.elapsed().as_secs_f64();
            let b = Instant::now();
            multi.run(input, &mut o2, &mut g2);
            (ta, b.elapsed().as_secs_f64())
        } else {
            let b = Instant::now();
            multi.run(input, &mut o2, &mut g2);
            let tb = b.elapsed().as_secs_f64();
            let a = Instant::now();
            single.run(input, &mut o1, &mut g1);
            (a.elapsed().as_secs_f64(), tb)
        };
        t1.push(r1);
        if separate {
            // The outputs are compared in the second pass.
            out1.extend_from_slice(&o1[..n]);
        } else {
            t2.push(r2);
            same &= o1[..n] == o2[..n];
        }
    }
    if separate {
        let mut at = 0;
        for i in 0..blocks {
            let k = i % 64;
            let input = &x[k * block * 2..(k + 1) * block * 2];
            let n = multi.out_frames(block) * 2;
            let b = Instant::now();
            multi.run(input, &mut o2, &mut g2);
            t2.push(b.elapsed().as_secs_f64());
            same &= out1[at..at + n] == o2[..n];
            at += n;
        }
    }
    let report = |name: &str, t: &mut Vec<f64>| {
        // The first 5 % warm the caches and the convolution's tail stages.
        let t = &mut t[blocks / 20..];
        t.sort_by(f64::total_cmp);
        let mean = t.iter().sum::<f64>() / t.len() as f64;
        let p99 = t[(t.len() * 99) / 100];
        let max = t[t.len() - 1];
        println!(
            "{name:>14}: mean {:7.3} ms  p99 {:7.3} ms  max {:7.3} ms  load {:5.1}% (p99 {:5.1}%)",
            mean * 1e3,
            p99 * 1e3,
            max * 1e3,
            mean / block_s * 100.0,
            p99 / block_s * 100.0
        );
        mean
    };
    println!(
        "44.1 kHz -> {target:?}, kernel {taps} taps, IR {ir_taps} taps, block {block} ({:.2} ms), {}",
        block_s * 1e3,
        if ab { "A/B pair" } else { "single chain" }
    );
    let m1 = report("audio thread", &mut t1);
    let m2 = report("with a worker", &mut t2);
    println!(
        "speed-up {:.2}x, outputs bit-identical: {same}, late blocks {}",
        m1 / m2,
        pool.stats()
            .late_blocks
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}
