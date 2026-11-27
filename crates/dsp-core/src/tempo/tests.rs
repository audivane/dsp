//! Tests of the tempo stage: its measurements (a 1 kHz sine, a multi-tone, a
//! click train, and the latency, at 44.1 kHz in 128-frame calls), ramps and
//! the cap, and the pull contract. The no-allocation test is in
//! `tests/no_alloc.rs`.

#![allow(clippy::needless_range_loop)]

use super::*;
use std::f64::consts::PI;

const G: u32 = 44_100;
const BLOCK: usize = 128;

fn stage(rate: u32, channels: usize, speed: f64) -> TempoStretch {
    TempoStretch::new(TempoSpec {
        sample_rate: rate,
        channels,
        max_frames: MAX_FRAMES,
        speed,
    })
    .unwrap()
}

/// Pull `n_out` frames in calls of `block` from `src` (source frame index
/// to sample, the same on every channel), calling `cmd` before each call
/// with the frames written so far. Returns channel 0, the source frames
/// taken per call, and the largest difference of another channel from
/// channel 0.
fn pull(
    st: &mut TempoStretch,
    src: &dyn Fn(usize) -> f64,
    n_out: usize,
    block: usize,
    mut cmd: impl FnMut(&mut TempoStretch, usize),
) -> (Vec<f64>, Vec<usize>, f64) {
    let ch = st.channels();
    let mut out = vec![0.0; n_out];
    let mut taken = Vec::new();
    let mut src_pos = 0usize;
    let mut ins = vec![vec![0.0; 2 * MAX_FRAMES + 1]; ch];
    let mut outs = vec![vec![0.0; MAX_FRAMES]; ch];
    let mut done = 0;
    let mut spread = 0.0f64;
    while done < n_out {
        cmd(st, done);
        let n = block.min(n_out - done);
        let bound = st.max_source_frames(n);
        let n_src = st.plan(n).unwrap();
        assert_eq!(n_src, bound, "max_source_frames agrees with plan");
        for c in 0..ch {
            for k in 0..n_src {
                ins[c][k] = src(src_pos + k);
            }
        }
        let i: Vec<&[f64]> = ins.iter().map(|x| &x[..n_src]).collect();
        let mut o: Vec<&mut [f64]> = outs.iter_mut().map(|x| &mut x[..n]).collect();
        st.process(&i, &mut o);
        for c in 1..ch {
            for k in 0..n {
                spread = spread.max((outs[c][k] - outs[0][k]).abs());
            }
        }
        out[done..done + n].copy_from_slice(&outs[0][..n]);
        src_pos += n_src;
        taken.push(n_src);
        done += n;
    }
    (out, taken, spread)
}

fn energy(x: &[f64]) -> f64 {
    x.iter().map(|v| v * v).sum()
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.max(1e-300).log10()
}

/// Solve `a x = b` (Gaussian elimination, partial pivoting).
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Vec<f64> {
    let n = b.len();
    for i in 0..n {
        let p = (i..n)
            .max_by(|&r, &s| a[r][i].abs().total_cmp(&a[s][i].abs()))
            .unwrap();
        a.swap(i, p);
        b.swap(i, p);
        for r in i + 1..n {
            let f = a[r][i] / a[i][i];
            for c in i..n {
                a[r][c] -= f * a[i][c];
            }
            b[r] -= f * b[i];
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let s: f64 = (i + 1..n).map(|c| a[i][c] * x[c]).sum();
        x[i] = (b[i] - s) / a[i][i];
    }
    x
}

/// Residual of `x` after a joint least-squares fit of a sine and cosine
/// at each of `freqs` (Hz) and a constant, relative to `x`'s energy, dB.
fn fit_residual_db(x: &[f64], freqs: &[f64]) -> f64 {
    let m = 2 * freqs.len() + 1;
    let basis = |i: usize, row: &mut [f64]| {
        for (j, f) in freqs.iter().enumerate() {
            let (s, c) = (2.0 * PI * f * i as f64 / f64::from(G)).sin_cos();
            row[2 * j] = c;
            row[2 * j + 1] = s;
        }
        row[m - 1] = 1.0;
    };
    let mut a = vec![vec![0.0; m]; m];
    let mut b = vec![0.0; m];
    let mut row = vec![0.0; m];
    for (i, &v) in x.iter().enumerate() {
        basis(i, &mut row);
        for r in 0..m {
            b[r] += row[r] * v;
            for c in 0..m {
                a[r][c] += row[r] * row[c];
            }
        }
    }
    let z = solve(a, b);
    let mut res = 0.0;
    for (i, &v) in x.iter().enumerate() {
        basis(i, &mut row);
        let fit: f64 = row.iter().zip(&z).map(|(r, z)| r * z).sum();
        res += (v - fit) * (v - fit);
    }
    db(res / energy(x))
}

/// The best-fitting frequency of a single sine near `f0` (within half a
/// cent), as an error in cents, and its residual in dB.
fn sine_error(x: &[f64], f0: f64) -> (f64, f64) {
    let cents = |c: f64| f0 * 2f64.powf(c / 1200.0);
    let (mut lo, mut hi) = (-0.5, 0.5);
    for _ in 0..40 {
        let a = lo + (hi - lo) * 0.382;
        let b = lo + (hi - lo) * 0.618;
        if fit_residual_db(x, &[cents(a)]) < fit_residual_db(x, &[cents(b)]) {
            hi = b;
        } else {
            lo = a;
        }
    }
    let c = 0.5 * (lo + hi);
    (c, fit_residual_db(x, &[cents(c)]))
}

/// At exactly 1× the stretch still runs and passes a
/// signal through delayed by `latency_at(1)`.
#[test]
fn unity_passes_through_at_the_latency() {
    let src = |i: usize| {
        [220.0, 1000.0, 5000.0]
            .iter()
            .map(|f| 0.25 * (2.0 * PI * f * i as f64 / f64::from(G)).sin())
            .sum::<f64>()
    };
    let mut st = stage(G, 2, 1.0);
    let lat = st.latency_at(1.0);
    assert_eq!(lat, 5954.0);
    let (out, taken, _) = pull(&mut st, &src, G as usize * 2, BLOCK, |_, _| {});
    assert!(taken[..taken.len() - 1].iter().all(|&t| t == BLOCK));
    let d = lat as usize;
    let tail = &out[G as usize..];
    let err: f64 = tail
        .iter()
        .enumerate()
        .map(|(k, v)| (v - src(G as usize + k - d)).powi(2))
        .sum();
    let r = db(err / energy(tail));
    eprintln!("unity: {r:.1} dB against the input delayed by {d}");
    assert!(r < -100.0, "unity residual {r} dB");
}

/// At constant speed the source taken is exact for the quantised step,
/// whatever the call sizes: after `m` frames the stage has taken
/// `floor((m - 1) s) + 1` frames.
#[test]
fn source_frames_are_exact_for_any_call_sizes() {
    for s in [0.5, 0.8, 1.0, 1.0123, 1.25, 1.999, 2.0] {
        let step = Step::from_f64(s).unwrap().bits();
        let mut st = stage(G, 1, s);
        let sizes = [128, 1, 0, 37, MAX_FRAMES, 4097, 2, 640];
        let (mut done, mut taken) = (0u128, 0u128);
        for i in 0..200 {
            let n = sizes[i % sizes.len()];
            let n_src = st.plan(n).unwrap();
            let input = vec![0.0; n_src];
            let mut out = vec![0.0; n];
            st.process(&[&input], &mut [&mut out]);
            done += n as u128;
            taken += n_src as u128;
            if done > 0 {
                assert_eq!(taken, (((done - 1) * step) >> 64) + 1, "s={s} after {done}");
            }
            assert_eq!(st.accumulator().frames_in() as u128, taken);
        }
    }
}

/// Every output frame's step, from the latched call's exact positions
/// (the first frame's from the position before the call).
fn steps_of(st: &TempoStretch, prev: &mut Option<u128>, steps: &mut Vec<f64>) {
    st.planned_positions(|q| {
        if let Some(p) = *prev {
            steps.push((q - p) as f64 / 18_446_744_073_709_551_616.0);
        }
        *prev = Some(q);
    });
}

/// A ramp follows the envelope (C¹, no faster than asked, exactly on its
/// target), takes source frames that match its positions exactly, and
/// holds the pitch while the tempo moves.
#[test]
fn ramps_hold_the_pitch() {
    let sine = |i: usize| 0.5 * (2.0 * PI * 1000.0 * i as f64 / f64::from(G)).sin();
    let start = 344 * BLOCK; // the command, after 1 s
    for (from, to, ramp_ms) in [(1.0, 1.25, 200.0), (1.0, 0.8, 500.0), (0.9, 1.5, 0.0)] {
        let mut st = stage(G, 2, from);
        let mut prev = None;
        let mut steps = Vec::new();
        let total = 3 * G as usize;
        let (out, taken, _) = pull(&mut st, &sine, total, BLOCK, |st, done| {
            if done == start {
                st.set_speed(to, ramp_ms).unwrap();
            }
            // Peek at the call's steps on a copy of its plan.
            if st.plan(BLOCK).is_ok() {
                steps_of(st, &mut prev, &mut steps);
            }
        });
        assert_eq!(st.speed(), to);
        assert_eq!(st.step(), Step::from_f64(to).unwrap(), "lands on the bits");
        assert!(!st.ramping());
        assert_eq!(st.fallbacks(), 0);
        // The source taken matches the positions.
        let total_src: usize = taken.iter().sum();
        assert_eq!(st.accumulator().frames_in() as usize, total_src);
        // The ramp: when it moves, how far per frame, how long.
        let moving: Vec<usize> = (1..steps.len())
            .filter(|&k| steps[k] != steps[k - 1])
            .collect();
        let (a, b) = (moving[0], *moving.last().unwrap());
        let len_ms = (b - a) as f64 * 1e3 / f64::from(G);
        let max_dstep = (1..steps.len())
            .map(|k| (steps[k] - steps[k - 1]).abs())
            .fold(0.0, f64::max);
        let max_d2 = (2..steps.len())
            .map(|k| (steps[k] - 2.0 * steps[k - 1] + steps[k - 2]).abs())
            .fold(0.0, f64::max);
        // The pitch, in 50 ms windows over the ramp (and the latency after
        // it), against 1 kHz.
        let win = 2205;
        let lat = st.latency_at(from.min(to)) as usize;
        let mut worst = (0.0f64, f64::NEG_INFINITY);
        let mut k = a + lat / 2;
        while k + win < b + lat + win && k + win < total {
            let (e, r) = sine_error(&out[k..k + win], 1000.0);
            worst = (worst.0.max(e.abs()), worst.1.max(r));
            k += win;
        }
        eprintln!(
            "{from} -> {to} over {ramp_ms} ms: moved {len_ms:.1} ms from {a}, max dstep {max_dstep:.2e}, \
             max d2 {max_d2:.2e}, worst window {:.3} c / {:.1} dB",
            worst.0, worst.1
        );
        assert!(a >= start && a <= start + BLOCK, "starts at the next call");
        assert!(len_ms >= ramp_ms.max(20.0) - 1.0, "ramp {len_ms} ms");
        // C¹: the step moves by tiny amounts per frame, and its slope
        // changes smoothly.
        assert!(max_dstep < 2e-4, "step jumps by {max_dstep}");
        assert!(max_d2 < 1e-6, "slope jumps by {max_d2}");
        assert!(worst.0 < 0.5, "pitch moves {} c during the ramp", worst.0);
        assert!(worst.1 < -60.0, "residual {} dB during the ramp", worst.1);
        assert!(out.iter().all(|v| v.abs() < 0.6), "bounded");
    }
}

/// A sound that stops plays out its whole tail at every speed: the
/// library's silence shortcut (after two windows of silent input) waits
/// until the synthesis has played it out. The reference keeps the library
/// from ever taking it (a -120 dB noise after the sound): the tails match.
/// Without the lift, 1.5× and 2× would cut the output while the overlap-add
/// still holds the sound.
#[test]
fn silence_after_a_sound_plays_the_tail_out() {
    for s in [0.5, 1.0, 1.5, 2.0] {
        let end = 40_000usize;
        // The tone ends under a 10 ms fade (a source step would be a step).
        let tone = |i: usize| {
            let x = 0.5 * (2.0 * PI * 1000.0 * i as f64 / f64::from(G)).sin();
            let left = end as f64 - i as f64;
            if left <= 0.0 {
                0.0
            } else if left < 441.0 {
                x * (0.5 - 0.5 * (PI * left / 441.0).cos())
            } else {
                x
            }
        };
        let noise = |i: usize| {
            let t = tone(i);
            if i >= end {
                t + 1e-6 * ((((i * 7919) % 101) as f64 / 101.0) - 0.5)
            } else {
                t
            }
        };
        let mut st = stage(G, 2, s);
        let n_out = (end as f64 / s) as usize + 4 * st.tail_frames_at(s);
        let (out, _, _) = pull(&mut st, &tone, n_out, BLOCK, |_, _| {});
        let mut reference = stage(G, 2, s);
        let (want, _, _) = pull(&mut reference, &noise, n_out, BLOCK, |_, _| {});
        let (worst, at) = out
            .iter()
            .zip(&want)
            .enumerate()
            .map(|(k, (a, b))| ((a - b).abs(), k))
            .fold((0.0, 0), |m, x| if x.0 > m.0 { x } else { m });
        let tail_peak = want[(end as f64 / s + st.latency_at(s)) as usize..]
            .iter()
            .fold(0.0f64, |m, x| m.max(x.abs()));
        eprintln!("s={s}: worst difference {worst:.1e} at {at}, the tail's peak {tail_peak:.3}");
        assert!(worst < 2e-6, "the tail is cut at {s}: {worst} at {at}");
        // And then digital silence (the shortcut).
        assert!(out[n_out - 4096..].iter().all(|&x| x.abs() < 1e-7), "{s}");
    }
}
