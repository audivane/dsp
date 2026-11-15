//! `avdsp nulltest` — sample-by-sample comparison of two WAV files.

use crate::NullTestArgs;
use crate::wavio::{self, WavData};
use anyhow::{Result, bail};

pub fn run(args: NullTestArgs) -> Result<i32> {
    let a = wavio::read_wav(&args.a)?;
    let b = wavio::read_wav(&args.b)?;
    compare(&a, &b, args.align, args.max_lag)?;
    Ok(0)
}

fn compare(a: &WavData, b: &WavData, align: bool, max_lag: usize) -> Result<()> {
    if a.channels != b.channels {
        bail!("channel counts differ: {} vs {}", a.channels, b.channels);
    }
    if a.rate != b.rate {
        println!(
            "warning: sample rates differ: {} Hz vs {} Hz",
            a.rate, b.rate
        );
    }

    let identical = bit_identical(a, b);
    let lag = if align {
        let mono_a = downmix(a);
        let mono_b = downmix(b);
        best_lag(&mono_a, &mono_b, max_lag)
    } else {
        0
    };
    let diff = aligned_diff(a, b, lag);

    println!("bit_identical: {}", if identical { "yes" } else { "no" });
    println!("frames: {} vs {}", a.frames(), b.frames());
    println!("rate: {} vs {}", a.rate, b.rate);
    println!("channels: {} vs {}", a.channels, b.channels);
    if align {
        println!("alignment: B lags A by {lag} frames");
    }
    println!("max_abs_diff: {:.12}", diff.max_abs);
    println!("rms_diff_db: {:.4}", rms_db(diff.rms));
    match diff.first {
        Some(index) => println!(
            "first_diff: sample {index} (frame {}, channel {})",
            index / a.channels,
            index % a.channels
        ),
        None => println!("first_diff: none"),
    }
    println!("compared_samples: {}", diff.count);
    Ok(())
}

fn bit_identical(a: &WavData, b: &WavData) -> bool {
    if a.rate != b.rate || a.channels != b.channels || a.samples.len() != b.samples.len() {
        return false;
    }
    match (&a.ints, &b.ints) {
        (Some(x), Some(y)) => x == y,
        _ => a
            .samples
            .iter()
            .zip(&b.samples)
            .all(|(x, y)| x.to_bits() == y.to_bits()),
    }
}

fn downmix(wav: &WavData) -> Vec<f64> {
    let channels = wav.channels;
    let inv = 1.0 / channels as f64;
    (0..wav.frames())
        .map(|frame| {
            let base = frame * channels;
            let sum: f64 = wav.samples[base..base + channels]
                .iter()
                .map(|&s| f64::from(s))
                .sum();
            sum * inv
        })
        .collect()
}

/// Least-squares lag by normalized cross-correlation. Positive means B lags A.
fn best_lag(a: &[f64], b: &[f64], max_lag: usize) -> isize {
    if a.is_empty() || b.is_empty() || max_lag == 0 {
        return 0;
    }
    let step = 32usize.min(max_lag);
    let a_dec: Vec<f64> = a.iter().step_by(step).copied().collect();
    let b_dec: Vec<f64> = b.iter().step_by(step).copied().collect();
    let max_lag_dec = (max_lag / step) as isize;
    let mut best = 0isize;
    let mut best_score = f64::NEG_INFINITY;
    for lag in -max_lag_dec..=max_lag_dec {
        let score = correlation(&a_dec, &b_dec, lag);
        if score > best_score {
            best_score = score;
            best = lag;
        }
    }
    let coarse = (best * step as isize).clamp(-(max_lag as isize), max_lag as isize);
    let lo = (coarse - step as isize).max(-(max_lag as isize));
    let hi = (coarse + step as isize).min(max_lag as isize);
    let mut best = coarse;
    let mut best_score = f64::NEG_INFINITY;
    for lag in lo..=hi {
        let score = correlation(a, b, lag);
        if score > best_score {
            best_score = score;
            best = lag;
        }
    }
    best
}

fn correlation(a: &[f64], b: &[f64], lag: isize) -> f64 {
    let (a_start, b_start) = if lag >= 0 {
        (0, lag as usize)
    } else {
        ((-lag) as usize, 0)
    };
    if a_start >= a.len() || b_start >= b.len() {
        return f64::NEG_INFINITY;
    }
    let len = (a.len() - a_start).min(b.len() - b_start);
    let mut dot = 0.0;
    let mut energy_a = 0.0;
    let mut energy_b = 0.0;
    for i in 0..len {
        let x = a[a_start + i];
        let y = b[b_start + i];
        dot += x * y;
        energy_a += x * x;
        energy_b += y * y;
    }
    let scale = (energy_a * energy_b).sqrt();
    if scale > 0.0 { dot / scale } else { 0.0 }
}

struct Diff {
    max_abs: f64,
    rms: f64,
    first: Option<usize>,
    count: usize,
}

fn aligned_diff(a: &WavData, b: &WavData, lag: isize) -> Diff {
    // The lag is in frames; sample offsets are interleaved.
    let (a_start, b_start) = if lag >= 0 {
        (0, lag as usize)
    } else {
        ((-lag) as usize, 0)
    };
    let a_off = a_start * a.channels;
    let b_off = b_start * b.channels;
    let samples = a
        .samples
        .len()
        .saturating_sub(a_off)
        .min(b.samples.len().saturating_sub(b_off));
    let mut max_abs = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut first = None;
    for i in 0..samples {
        let x = f64::from(a.samples[a_off + i]);
        let y = f64::from(b.samples[b_off + i]);
        let d = (x - y).abs();
        if d > max_abs {
            max_abs = d;
        }
        sum_sq += d * d;
        if first.is_none() && x.to_bits() != y.to_bits() {
            first = Some(a_off + i);
        }
    }
    let rms = if samples > 0 {
        (sum_sq / samples as f64).sqrt()
    } else {
        0.0
    };
    Diff {
        max_abs,
        rms,
        first,
        count: samples,
    }
}

fn rms_db(rms: f64) -> f64 {
    if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    }
}
