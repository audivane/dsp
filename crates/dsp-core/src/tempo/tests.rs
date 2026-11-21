//! Tests of the tempo stage: its measurements (a 1 kHz sine, a multi-tone, a
//! click train, and the latency, at 44.1 kHz in 128-frame calls), ramps and
//! the cap, and the pull contract. The no-allocation test is in
//! `tests/no_alloc.rs`.

#![allow(clippy::needless_range_loop)]

use super::*;
use std::f64::consts::PI;

const G: u32 = 44_100;
const BLOCK: usize = 128;
/// The measured run: 1 034 calls of 128 frames, the second half measured.
const N: usize = 1034 * BLOCK;

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

/// A click train of `0.9` every `period` source frames.
fn clicks(period: f64) -> impl Fn(usize) -> f64 {
    move |i| {
        let k = (i as f64 / period).round();
        if (k * period).round() as usize == i {
            0.9
        } else {
            0.0
        }
    }
}

/// The click figures over the second half of `out` (output period
/// 4 410): the mean peak relative to 0.9, the mean pre-echo (energy 40 to
/// 5 ms before each peak against ±2 ms around it, dB), and the mean offset
/// of the peaks from where the latency puts them, frames.
fn click_figures(out: &[f64], latency: f64) -> (f64, f64, f64) {
    let p = 4410;
    let w = (0.002 * f64::from(G)) as usize;
    let (pre_a, pre_b) = (
        (0.040 * f64::from(G)) as usize,
        (0.005 * f64::from(G)) as usize,
    );
    let (mut peak, mut pre, mut off, mut count) = (0.0, 0.0, 0.0, 0);
    let mut start = out.len() / 2 + p / 2;
    while start + p < out.len() {
        let (pk, pv) = (start..start + p)
            .map(|k| (k, out[k].abs()))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        let near = energy(&out[pk - w..=pk + w]);
        let before = energy(&out[pk - pre_a..pk - pre_b]);
        // The click at source q = j·4410·s is heard at q / s + latency.
        let j = ((pk as f64 - latency) / p as f64).round();
        off += pk as f64 - (j * p as f64 + latency);
        peak += pv / 0.9;
        pre += before / near;
        count += 1;
        start += p;
    }
    let c = f64::from(count);
    (peak / c, db(pre / c), off / c)
}

/// The measured table for the stretch route: speed, sine residual,
/// multi-tone residual, click peak, pre-echo, latency (frames).
const MEASURED: [(f64, f64, f64, f64, f64, usize); 9] = [
    (0.50, -108.5, -63.5, 0.97, -28.0, 8600),
    (0.80, -126.9, -67.4, 0.88, -31.8, 6616),
    (0.90, -122.4, -61.3, 0.80, -34.7, 6248),
    (0.95, -138.7, -69.0, 0.79, -34.8, 6093),
    (1.05, -131.7, -58.9, 0.76, -35.5, 5828),
    (1.10, -121.8, -65.2, 0.71, -35.0, 5713),
    (1.25, -119.7, -64.6, 0.69, -35.9, 5425),
    (1.50, -113.2, -67.1, 0.57, -34.5, 5072),
    (2.00, -108.6, -69.0, 0.52, -37.6, 4631),
];

/// The measured stretch run with one stage per signal: the sine's pitch
/// error and residual, the multi-tone residual, the click figures and
/// the latency, against the table above. The multi-tone figure here
/// is a joint fit of the five tones; sequential fits leave
/// each tone's leakage into the next fit (about −60 dB), which is where
/// the −58 to −69 dB figures came from.
#[test]
fn measurements_match_the_reference() {
    const TONES: [f64; 5] = [220.0, 440.0, 880.0, 1760.0, 3520.0];
    for (s, n_sine, _, n_peak, n_pre, n_lat) in MEASURED {
        let sine = |i: usize| 0.5 * (2.0 * PI * 1000.0 * i as f64 / f64::from(G)).sin();
        let mut st = stage(G, 2, s);
        let lat = st.latency_at(s);
        let (out, _, spread) = pull(&mut st, &sine, N, BLOCK, |_, _| {});
        let (err, res) = sine_error(&out[N / 2..], 1000.0);

        let multi_src = |i: usize| -> f64 {
            TONES
                .iter()
                .map(|f| 0.16 * (2.0 * PI * f * i as f64 / f64::from(G)).sin())
                .sum()
        };
        let mut st = stage(G, 2, s);
        let (out, _, _) = pull(&mut st, &multi_src, N, BLOCK, |_, _| {});
        let multi = fit_residual_db(&out[N / 2..], &TONES);

        let mut st = stage(G, 2, s);
        let (out, _, _) = pull(&mut st, &clicks(4410.0 * s), N, BLOCK, |_, _| {});
        let (peak, pre, off) = click_figures(&out, lat);
        eprintln!(
            "s={s:.2}: sine {err:+.4} c / {res:.1} dB (note {n_sine}), multi {multi:.1} dB, \
             clicks {peak:.2} / {pre:.1} dB (note {n_peak} / {n_pre}), latency {lat:.1} \
             (note {n_lat}), click offset {off:+.1}, channel spread {spread:.1e}"
        );
        assert!(err.abs() < 0.01, "pitch error {err} c at {s}");
        assert!(res < n_sine + 3.0, "sine residual {res} dB at {s}");
        let multi_max = if s < 0.8 { -75.0 } else { -105.0 };
        assert!(multi < multi_max, "multi-tone residual {multi} dB at {s}");
        assert!((peak - n_peak).abs() < 0.08, "click peak {peak} at {s}");
        assert!(pre < n_pre + 3.0, "pre-echo {pre} dB at {s}");
        // The clicks land where the latency puts them.
        assert!(off.abs() < 16.0, "click offset {off} at {s}");
        assert_eq!(lat.round() as usize, n_lat, "latency at {s}");
        // Identical channels stay together (the library locks their
        // phases, not bit for bit).
        assert!(spread < 1e-3, "channels spread {spread} at {s}");
    }
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

#[test]
fn construction_and_command_errors() {
    let spec = TempoSpec {
        sample_rate: G,
        channels: 2,
        max_frames: 512,
        speed: 1.0,
    };
    let bad = |s: TempoSpec| TempoStretch::new(s).err().unwrap();
    assert_eq!(
        bad(TempoSpec {
            sample_rate: 0,
            ..spec
        }),
        TempoError::Rate(0)
    );
    assert_eq!(
        bad(TempoSpec {
            channels: 0,
            ..spec
        }),
        TempoError::Channels(0)
    );
    assert_eq!(
        bad(TempoSpec {
            channels: MAX_CHANNELS + 1,
            ..spec
        }),
        TempoError::Channels(MAX_CHANNELS + 1)
    );
    assert_eq!(
        bad(TempoSpec {
            max_frames: 0,
            ..spec
        }),
        TempoError::MaxFrames(0)
    );
    assert_eq!(
        bad(TempoSpec {
            max_frames: MAX_FRAMES + 1,
            ..spec
        }),
        TempoError::MaxFrames(MAX_FRAMES + 1)
    );
    for s in [0.49, 2.01, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            bad(TempoSpec { speed: s, ..spec }),
            TempoError::Speed(_)
        ));
    }
    let mut st = TempoStretch::new(spec).unwrap();
    for s in [0.0, 0.49, 2.01, f64::NAN] {
        assert!(matches!(st.set_speed(s, 0.0), Err(TempoError::Speed(_))));
    }
    for r in [-1.0, MAX_RAMP_MS + 1.0, f64::NAN] {
        assert!(matches!(st.set_speed(1.1, r), Err(TempoError::Ramp(_))));
    }
    assert_eq!(st.command_seq(), 0, "refused commands change nothing");
    assert_eq!(
        st.plan(513),
        Err(TempoError::TooManyFrames { n: 513, max: 512 })
    );
}

/// The pitch stage's `high` geometry, stepped down to `low` above
/// 192 kHz; the latencies are the library's.
#[test]
fn geometry_steps_down_at_high_rates() {
    let st = stage(G, 2, 1.0);
    assert_eq!(st.geometry(), (5292, 662));
    assert_eq!((st.input_latency(), st.output_latency()), (2646, 3308));
    for rate in [48_000, 96_000, 192_000] {
        let st = stage(rate, 2, 1.0);
        assert_eq!(st.geometry(), stft_geometry(rate, PitchQuality::High));
    }
    let st = stage(384_000, 2, 1.0);
    assert_eq!(st.geometry(), stft_geometry(384_000, PitchQuality::Low));
    assert_eq!(st.geometry().1, 11_520, "30 ms hop");
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

/// The cap: a request above it is held at it, exactly; a cap dropped
/// under the applied speed jumps the step at the next call without a gap
/// or a click; raising it again ramps back to the request.
#[test]
fn speed_cap_jumps_without_a_gap() {
    let sine = |i: usize| 0.5 * (2.0 * PI * 1000.0 * i as f64 / f64::from(G)).sin();
    let cap = Step::from_f64(1.2).unwrap();
    let mut st = stage(G, 2, 1.0);
    st.set_speed_limit(1.2);
    st.set_speed(1.5, 0.0).unwrap();
    assert_eq!(st.target(), 1.2);
    assert!(st.peak_until_settled() <= 1.2);
    let mut prev = None;
    let mut steps = Vec::new();
    let (_, _, _) = pull(&mut st, &sine, G as usize, BLOCK, |st, _| {
        assert!(st.peak_until_settled() <= 1.2);
        st.plan(BLOCK).unwrap();
        steps_of(st, &mut prev, &mut steps);
    });
    assert!(steps.iter().all(|&s| s <= cap.to_f64()));
    assert_eq!(st.step(), cap);
    assert_eq!(st.requested(), 1.5);

    // Up to 1.5, then a cap at 1.1 under it.
    st.set_speed_limit(f64::INFINITY);
    let mut cut = 0;
    let mut speeds = Vec::new();
    let total = 3 * G as usize;
    let (out, _, _) = pull(&mut st, &sine, total, BLOCK, |st, done| {
        if done == 689 * BLOCK {
            st.set_speed_limit(1.1);
            assert_eq!(st.peak_until_settled(), 1.1, "the next call is at the cap");
            cut = done;
        }
        speeds.push(st.speed());
    });
    let i = cut / BLOCK;
    assert_eq!(speeds[i - 1], 1.5);
    assert_eq!(speeds[i + 1], 1.1, "jumped within the call");
    assert_eq!(st.step(), Step::from_f64(1.1).unwrap());
    assert_eq!(st.fallbacks(), 0);
    // No gap or click: the output keeps a clean 1 kHz through the jump.
    let max_slew = (cut..total - 1)
        .map(|k| (out[k + 1] - out[k]).abs())
        .fold(0.0, f64::max);
    let natural = 0.5 * 2.0 * PI * 1000.0 / f64::from(G);
    let (e, r) = sine_error(&out[cut..cut + 3 * 4410], 1000.0);
    let min_env = (cut..total - 441)
        .step_by(441)
        .map(|k| out[k..k + 441].iter().fold(0.0f64, |m, v| m.max(v.abs())))
        .fold(f64::INFINITY, f64::min);
    eprintln!(
        "cap drop: slew {max_slew:.4} (natural {natural:.4}), {e:.3} c / {r:.1} dB, min 10 ms peak {min_env:.3}"
    );
    assert!(max_slew < 1.1 * natural, "click: slew {max_slew}");
    assert!(min_env > 0.45, "gap: a 10 ms peak of {min_env}");
    assert!(e.abs() < 0.5 && r < -60.0, "{e} c, {r} dB");
}

/// A zero call and an abandoned plan change nothing; a command waits for
/// the next call with frames.
#[test]
fn zero_calls_and_abandoned_plans() {
    let mut a = stage(G, 1, 1.0);
    let mut b = stage(G, 1, 1.0);
    a.set_speed(1.3, 100.0).unwrap();
    b.set_speed(1.3, 100.0).unwrap();
    assert_eq!(a.plan(0), Ok(0));
    a.process(&[&[]], &mut [&mut []]);
    assert!(a.ramping() && a.speed() == 1.0, "waits for frames");
    let _ = a.plan(4000); // abandoned
    let src: Vec<f64> = (0..20_000).map(|i| (i as f64 * 0.01).sin()).collect();
    let mut pos = 0;
    for _ in 0..40 {
        let na = a.plan(256).unwrap();
        let nb = b.plan(256).unwrap();
        assert_eq!(na, nb);
        let (mut oa, mut ob) = (vec![0.0; 256], vec![0.0; 256]);
        a.process(&[&src[pos..pos + na]], &mut [&mut oa]);
        b.process(&[&src[pos..pos + nb]], &mut [&mut ob]);
        assert_eq!(oa, ob);
        assert_eq!(a.step(), b.step());
        pos += na;
    }
}

/// A reset holds at the capped request from position 0 with empty
/// windows.
#[test]
fn reset_clears_everything() {
    let mut st = stage(G, 2, 1.0);
    st.set_speed(1.4, 300.0).unwrap();
    let noise = |i: usize| ((i * 7919) % 101) as f64 / 101.0 - 0.5;
    pull(&mut st, &noise, 10_000, BLOCK, |_, _| {});
    assert!(st.ramping());
    st.set_speed_limit(1.3);
    st.reset();
    assert!(!st.ramping());
    assert_eq!(st.speed(), 1.3);
    assert_eq!(st.accumulator().position(), 0);
    assert_eq!(st.accumulator().frames_in(), 0);
    let (out, _, _) = pull(&mut st, &|_| 0.0, 20_000, BLOCK, |_, _| {});
    assert!(out.iter().all(|&v| v == 0.0));
}

/// High rates, at the ends of the range: finite, bounded and at level.
#[test]
fn high_rates_run() {
    for rate in [96_000u32, 192_000, 384_000] {
        for s in [0.5, 2.0] {
            let sine = move |i: usize| 0.5 * (2.0 * PI * 1000.0 * i as f64 / f64::from(rate)).sin();
            let mut st = stage(rate, 2, s);
            let n = 2 * rate as usize;
            let (out, _, _) = pull(&mut st, &sine, n, 1024, |_, _| {});
            // The sine switching on from silence overshoots a little.
            let max = out.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            assert!(
                out.iter().all(|v| v.is_finite()) && max < 0.8,
                "{rate} Hz at {s}: max {max}"
            );
            let tail = &out[n / 2..];
            let peak = tail.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            let low = tail
                .chunks(rate as usize / 100)
                .map(|c| c.iter().fold(0.0f64, |m, v| m.max(v.abs())))
                .fold(1.0, f64::min);
            // Known: the stepped-down `low` geometry (above 192 kHz) loses
            // 0.45 dB at 0.5× (0.4749 of 0.5); 0.55× and up are flat.
            let floor = if rate > 192_000 && s < 0.55 {
                0.47
            } else {
                0.49
            };
            assert!(
                peak < 0.51 && low > floor,
                "{rate} Hz at {s}: {low}..{peak}"
            );
        }
    }
}

/// Near 1× the hop's input interval is within a frame of the nominal
/// one. Upstream the library then skips the re-analysis and the pitch
/// follows the speed (up to 2.6 c), and where hops alternate between the
/// two cases a sine warbles (−14 to −19 dB at 0.997 to 1.003). The
/// vendored copy re-analyses whenever the interval differs (see the
/// header's local change), so the key lock holds there too.
#[test]
fn near_unity_holds_the_pitch() {
    let sine = |i: usize| 0.5 * (2.0 * PI * 1000.0 * i as f64 / f64::from(G)).sin();
    for s in [0.997, 0.9985, 0.9995, 1.0005, 1.001, 1.0015, 1.002, 1.003] {
        let mut st = stage(G, 2, s);
        let (out, _, _) = pull(&mut st, &sine, N, BLOCK, |_, _| {});
        let (err, res) = sine_error(&out[N / 2..], 1000.0);
        eprintln!("s={s}: {err:+.4} c, {res:.1} dB");
        assert!(err.abs() < 0.01, "pitch error {err} c at {s}");
        assert!(res < -100.0, "sine residual {res} dB at {s}");
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

/// The bridge's lift grows the first sample's magnitude whatever its sign:
/// a quiet call (below the noise floor) whose first sample is −2.5e-8
/// stayed under the floor with +5e-8 added, and the shortcut cut the tail
/// at 2× (0.0068 FS). Mono, so that one sample decides the call's energy.
#[test]
fn a_negative_quiet_sample_still_holds_the_tail() {
    let s = 2.0;
    let end = 40_000usize;
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
    let n_out = (end as f64 / s) as usize + 4 * stage(G, 1, s).tail_frames_at(s);
    // Where each call's source starts (the plan is the same every run).
    let (_, taken, _) = pull(&mut stage(G, 1, s), &tone, n_out, BLOCK, |_, _| {});
    let starts: std::collections::HashSet<usize> = taken
        .iter()
        .scan(0, |at, &n| {
            let first = *at;
            *at += n;
            Some(first)
        })
        .collect();
    let quiet = |i: usize| {
        if i >= end && starts.contains(&i) {
            -2.5e-8
        } else {
            tone(i)
        }
    };
    let noise = |i: usize| {
        if i >= end {
            tone(i) + 1e-6 * ((((i * 7919) % 101) as f64 / 101.0) - 0.5)
        } else {
            tone(i)
        }
    };
    let (out, _, _) = pull(&mut stage(G, 1, s), &quiet, n_out, BLOCK, |_, _| {});
    let (want, _, _) = pull(&mut stage(G, 1, s), &noise, n_out, BLOCK, |_, _| {});
    let worst = out
        .iter()
        .zip(&want)
        .fold(0.0f64, |m, (a, b)| m.max((a - b).abs()));
    eprintln!("negative quiet samples at 2x: worst difference {worst:.1e}");
    assert!(worst < 2e-6, "the tail is cut: {worst}");
}
