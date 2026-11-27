//! Time per callback of the FFT polyphase upsampler, the uniformly
//! partitioned engine (`uniform`) against the non-uniformly partitioned one
//! (`nupc`), on one channel and on stereo with channel workers (lane 1 on a
//! worker, lane 0 on this thread, as the chain runs them).
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example bench_upsampler_nupc -- \
//!     [seconds] [taps,taps,..] [rate:ratio,rate:ratio,..] [engines] [modes]
//! ```
//!
//! Defaults: 4 s of audio per case after a warm-up of two periods of the
//! largest stage, taps 1048576,4194304,8388608, cases
//! 44100:16,48000:16,96000:8,192000:4, engines `uniform,nupc`, modes
//! `mono,stereo`. The callback is the chain block the engine runs at that
//! rate with the default quantum of 1024 (512 frames at 44.1 kHz, 1024 at
//! 48 kHz, 2048 at 96 kHz, 4096 at 192 kHz). Load is time over the
//! callback's duration: mean, 99th percentile, 99.9th and max.

use audivane_dsp_core::resample::{Upsampler, UpsamplerDesign, design_kernel};
use audivane_dsp_core::workers::{Lane, LanePool, LaneWork};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Work {
    up: Upsampler,
    input: Vec<f64>,
    output: Vec<f64>,
}

impl LaneWork for Work {
    fn run(&mut self) {
        self.up.process(&[&self.input], &mut [&mut self.output]);
    }
}

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
}

/// `scaled_quantum(1024, rate)`: the chain block at `rate`.
fn chain_block(rate: u32) -> usize {
    let raw = (1024 * u64::from(rate) / 48_000).clamp(32, 8192) as usize;
    (1usize << raw.ilog2()).clamp(512, 8192)
}

struct Stats {
    mean: f64,
    p99: f64,
    p999: f64,
    max: f64,
}

fn stats(mut t: Vec<f64>) -> Stats {
    let mean = t.iter().sum::<f64>() / t.len() as f64;
    t.sort_by(f64::total_cmp);
    let at = |q: f64| t[((t.len() as f64 * q) as usize).min(t.len() - 1)];
    Stats {
        mean,
        p99: at(0.99),
        p999: at(0.999),
        max: *t.last().unwrap(),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let secs: f64 = a.first().map_or(4.0, |s| s.parse().unwrap());
    let list = |i: usize, d: &str| -> Vec<String> {
        a.get(i)
            .map_or(d, String::as_str)
            .split(',')
            .map(str::to_string)
            .collect()
    };
    let taps: Vec<usize> = list(1, "1048576,4194304,8388608")
        .iter()
        .map(|s| s.parse().unwrap())
        .collect();
    let cases: Vec<(u32, usize)> = list(2, "44100:16,48000:16,96000:8,192000:4")
        .iter()
        .map(|s| {
            let (r, x) = s.split_once(':').unwrap();
            (r.parse().unwrap(), x.parse().unwrap())
        })
        .collect();
    let engines = list(3, "uniform,nupc");
    let modes = list(4, "mono,stereo");
    let pool = LanePool::spawn(1, |_| ()).expect("worker");
    assert!(pool.wait_started(Duration::from_secs(5)));

    println!(
        "| taps | source | ratio | block | engine | mode | stages | IR MiB | state MiB/ch | mean % | p99 % | p99.9 % | max % |"
    );
    println!("|---:|---:|---:|---:|---|---|---|---:|---:|---:|---:|---:|---:|");
    for &n in &taps {
        let mut ratios: Vec<usize> = cases.iter().map(|c| c.1).collect();
        ratios.sort_unstable();
        ratios.dedup();
        for ratio in ratios {
            let d = UpsamplerDesign::linear(ratio, n, 14.0, 1.0);
            let kernel = design_kernel(&d);
            let delay = (kernel.len() - 1) / 2;
            for &(rate, _) in cases.iter().filter(|c| c.1 == ratio) {
                let block = chain_block(rate);
                let period = block as f64 / f64::from(rate);
                for engine in &engines {
                    for mode in &modes {
                        let channels = if mode == "stereo" { 2 } else { 1 };
                        let mut up = match engine.as_str() {
                            "uniform" => Upsampler::from_kernel_uniform(
                                &kernel, ratio, delay, channels, block,
                            ),
                            _ => Upsampler::from_kernel(&kernel, ratio, delay, channels, block),
                        };
                        let stages = up
                            .stages()
                            .iter()
                            .map(|(b, p)| format!("{b}x{p}"))
                            .collect::<Vec<_>>()
                            .join("+");
                        let (ir, state) = up.resident_bytes();
                        let largest = up.stages().iter().map(|s| s.0).max().unwrap();
                        let warm = 2 * largest.div_ceil(block.min(1024)).max(1);
                        let calls = warm + (secs / period).ceil() as usize;
                        let mut seed = 1u64;
                        let x: Vec<Vec<f64>> = (0..64)
                            .map(|_| (0..block).map(|_| lcg(&mut seed)).collect())
                            .collect();
                        let mut times = Vec::with_capacity(calls);
                        if channels == 1 {
                            let mut out = vec![0.0; block * ratio];
                            for i in 0..calls {
                                let t0 = Instant::now();
                                up.process(&[&x[i % 64]], &mut [&mut out]);
                                times.push(t0.elapsed().as_secs_f64());
                            }
                            std::hint::black_box(&out);
                        } else {
                            let mut lanes = up.split_channels().into_iter();
                            let mut w0 = Work {
                                up: lanes.next().unwrap(),
                                input: vec![0.0; block],
                                output: vec![0.0; block * ratio],
                            };
                            let mut lane1 = Lane::new(Work {
                                up: lanes.next().unwrap(),
                                input: vec![0.0; block],
                                output: vec![0.0; block * ratio],
                            });
                            for i in 0..calls {
                                let t0 = Instant::now();
                                lane1.get_mut().unwrap().input.copy_from_slice(&x[i % 64]);
                                w0.input.copy_from_slice(&x[(i + 7) % 64]);
                                assert!(pool.submit(0, &mut lane1));
                                w0.run();
                                let deadline = t0 + Duration::from_secs(5);
                                assert!(pool.wait(0, &lane1, deadline));
                                times.push(t0.elapsed().as_secs_f64());
                            }
                            std::hint::black_box(&w0.output);
                        }
                        let measured = times.split_off(warm);
                        if std::env::var_os("PHASES").is_some() {
                            // Mean time by position within the largest stage's period.
                            let per = (largest / block.min(1024) * block.min(1024) / block).max(1);
                            let mut sum = vec![0.0; per];
                            let mut cnt = vec![0usize; per];
                            for (i, t) in measured.iter().enumerate() {
                                sum[(i + warm) % per] += t;
                                cnt[(i + warm) % per] += 1;
                            }
                            let line: Vec<String> = sum
                                .iter()
                                .zip(&cnt)
                                .map(|(s, c)| format!("{:.1}", s / *c as f64 / period * 100.0))
                                .collect();
                            eprintln!("phases ({per}): {}", line.join(" "));
                        }
                        let s = stats(measured);
                        let pct = |t: f64| t / period * 100.0;
                        println!(
                            "| {n} | {rate} | x{ratio} | {block} | {engine} | {mode} | {stages} | {:.1} | {:.1} | {:.2} | {:.2} | {:.2} | {:.2} |",
                            ir as f64 / 1048576.0,
                            state as f64 / 1048576.0,
                            pct(s.mean),
                            pct(s.p99),
                            pct(s.p999),
                            pct(s.max)
                        );
                    }
                }
            }
        }
    }
    drop(Arc::clone(&pool));
}
