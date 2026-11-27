//! Tests of the adaptive core: the table's measurement, the core against a
//! slow reference, the pull accounting and the accumulator over long runs and
//! ramps. The no-allocation test is in `tests/no_alloc.rs`.

use super::*;

fn lcg(seed: &mut u64) -> f64 {
    *seed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
}

/// Uniform in [0, 1).
fn unit(seed: &mut u64) -> f64 {
    0.5 * (lcg(seed) + 1.0)
}

fn below(seed: &mut u64, n: usize) -> usize {
    ((unit(seed) * n as f64) as usize).min(n.saturating_sub(1))
}

fn db(x: f64) -> f64 {
    20.0 * x.abs().max(1e-300).log10()
}

fn step(s: f64) -> Step {
    Step::from_f64(s).unwrap()
}

/// A random segment for a call of `n` outputs from step `v0`: holds,
/// jumps, and pieces with slopes that may reverse the ramp's direction.
fn random_segment(seed: &mut u64, v0: f64, n: usize) -> Segment {
    let r = unit(seed);
    if r < 0.2 {
        return Segment::Hold;
    }
    if r < 0.35 {
        return Segment::Jump(step(0.5 + 1.5 * unit(seed)));
    }
    let end = if unit(seed) < 0.5 {
        0.5 + 1.5 * unit(seed)
    } else {
        (v0 * (1.0 + 0.05 * lcg(seed))).clamp(0.5, 2.0)
    };
    let scale = (end - v0).abs().max(1e-3) / n.max(1) as f64;
    Segment::Piece {
        slope0: 3.0 * lcg(seed) * scale,
        end: step(end),
        slope1: 3.0 * lcg(seed) * scale,
    }
}

/// Plan `seg`, falling back to holding when a random piece leaves the
/// step range (checking that the refusal left nothing behind).
fn plan_or_hold(core: &mut AdaptiveCore, n: usize, seg: Segment) -> (usize, Segment) {
    let before = core.accumulator().clone();
    match core.plan(n, seg) {
        Ok(need) => (need, seg),
        Err(e) => {
            assert!(matches!(e, PlanError::StepOutOfRange { .. }), "{e}");
            assert_eq!(core.planned(), None);
            let mut after = core.accumulator().clone();
            after.pending = None;
            let mut b = before;
            b.pending = None;
            assert_eq!(after, b, "a refused plan changed the state");
            (core.plan(n, Segment::Hold).unwrap(), Segment::Hold)
        }
    }
}

#[test]
fn table_is_measured_at_build_and_meets_its_targets() {
    let t = CoreTable::shared().unwrap();
    let r = t.report();
    println!(
        "core table: {} taps, beta {:.4}, P {PHASES}: passband {:.2e} dB, stopband {:.1} dB \
         (continuous kernel {:.1} dB), interpolation error {:.1} dB",
        t.taps(),
        t.beta(),
        r.passband_ripple_db,
        r.stopband_db,
        r.exact_stopband_db,
        r.interpolation_error_db
    );
    assert_eq!(t.taps(), 25);
    assert_eq!(t.delay(), 12.0);
    assert_eq!(t.beta(), kaiser_beta_for_attenuation(172.0));
    assert!(r.stopband_db <= -TARGET_DB);
    assert!(r.exact_stopband_db <= -TARGET_DB);
    assert!(r.interpolation_error_db <= -(TARGET_DB + INTERPOLATION_MARGIN_DB));
    assert!(r.passband_ripple_db <= MAX_PASSBAND_RIPPLE_DB);
    // The cutoff is the input Nyquist, so phase 0 is the identity.
    let rows = t.rows_at(0);
    for j in 0..t.taps() {
        let want = if j == t.taps() - 1 - 12 { 1.0 } else { 0.0 };
        assert_eq!(rows[t.taps() + j], want, "tap {j}");
    }
    // Every phase has unity DC gain to the passband ripple.
    for ph in 0..PHASES + 3 {
        let s: f64 = t.rows[ph * t.taps()..(ph + 1) * t.taps()].iter().sum();
        assert!((s - 1.0).abs() < 1e-8, "row {ph}: {s}");
    }
    // The transfer function the stage predicts with.
    assert!((t.response(0.0) - 1.0).abs() < 1e-8);
    assert!(db(t.response(0.25)).abs() < 1e-5);
    assert!(db(t.response(0.8)) < -170.0);
}

/// The slow reference: each output straight from the continuous kernel at
/// the exact position (`pos`, absolute 64.64).
fn reference(table: &CoreTable, pos: u128, taps: &mut [f64]) -> i64 {
    let i = (pos >> 64) as i64;
    let f = (pos as u64) as f64 / ONE_F;
    for (j, w) in taps.iter_mut().enumerate() {
        *w = table.kernel(j as f64 + f - table.delay());
    }
    i
}

/// The core against the slow reference, random speeds, jumps
/// and ramps, n in {0, 1, odd, max}, 1 to 8 channels; channel 0 of every
/// layout equals a mono core bit for bit.
#[test]
fn core_matches_the_continuous_kernel_at_the_exact_positions() {
    let table = CoreTable::shared().unwrap();
    let taps = table.taps();
    let max_out = 1024;
    let mut worst_all = 0.0f64;
    for (ci, channels) in [1usize, 2, 3, 8].into_iter().enumerate() {
        let mut seed = 100 + ci as u64;
        let calls = 160;
        let len = calls * (2 * max_out + 8);
        let x: Vec<Vec<f64>> = (0..channels)
            .map(|_| (0..len).map(|_| lcg(&mut seed)).collect())
            .collect();
        let mut core = AdaptiveCore::new(channels, max_out, step(1.0)).unwrap();
        let mut mono = AdaptiveCore::new(1, max_out, step(1.0)).unwrap();
        let mut fed = 0usize;
        let mut out = vec![vec![0.0; max_out]; channels];
        let mut out_mono = vec![0.0; max_out];
        let mut w = vec![0.0; taps];
        let mut worst = 0.0f64;
        let mut outputs = 0usize;
        for call in 0..calls {
            let n = match call % 5 {
                0 => 0,
                1 => 1,
                2 => max_out,
                _ => 2 * below(&mut seed, max_out / 2) + 1,
            };
            let v0 = core.step().to_f64();
            let seg = random_segment(&mut seed, v0, n);
            let (need, seg) = plan_or_hold(&mut core, n, seg);
            assert_eq!(mono.plan(n, seg).unwrap(), need);
            let n_in = if need > 0 && unit(&mut seed) < 0.5 {
                need + 1
            } else {
                need
            };
            let planned = core.planned().unwrap();
            let p0 = core.accumulator().position();
            {
                let input: Vec<&[f64]> = x.iter().map(|c| &c[fed..fed + n_in]).collect();
                let mut output: Vec<&mut [f64]> = out.iter_mut().map(|o| &mut o[..n]).collect();
                core.process(&input, &mut output);
                mono.process(&[&x[0][fed..fed + n_in]], &mut [&mut out_mono[..n]]);
            }
            fed += n_in;
            // The positions, by the per-output adds, from the plan.
            let mut p = p0;
            let mut st = planned.step as i128;
            let [mut d1, mut d2, d3] = planned.d;
            for j in 0..n {
                let i = reference(&table, p, &mut w);
                for c in 0..channels {
                    let mut want = 0.0;
                    for (k, wk) in w.iter().enumerate() {
                        let idx = i - k as i64;
                        if idx >= 0 {
                            assert!((idx as usize) < fed, "reference reads past the input");
                            want += x[c][idx as usize] * wk;
                        }
                    }
                    worst = worst.max((out[c][j] - want).abs());
                }
                assert_eq!(
                    out[0][j].to_bits(),
                    out_mono[j].to_bits(),
                    "{channels} ch, call {call}"
                );
                p += st as u128;
                st += d1;
                d1 += d2;
                d2 += d3;
            }
            assert_eq!(core.accumulator().position(), p);
            assert_eq!(core.step().bits(), planned.end);
            assert!(core.retained() <= MAX_RETAINED);
            outputs += n;
        }
        println!(
            "{channels} channel(s): {outputs} outputs, largest error against the continuous \
             kernel {:.1} dBFS",
            db(worst)
        );
        worst_all = worst_all.max(worst);
    }
    assert!(db(worst_all) <= -180.0, "{:.1} dB", db(worst_all));
}

/// Run `core` for one call; returns the outputs of channel 0.
fn run_call(
    core: &mut AdaptiveCore,
    x: &[Vec<f64>],
    fed: &mut usize,
    n: usize,
    seg: Segment,
    over: bool,
) -> Vec<f64> {
    let ch = core.channels();
    let need = core.plan(n, seg).unwrap();
    let n_in = need + usize::from(over && need > 0);
    let mut out = vec![vec![0.0; n]; ch];
    let input: Vec<&[f64]> = x.iter().map(|c| &c[*fed..*fed + n_in]).collect();
    let mut output: Vec<&mut [f64]> = out.iter_mut().map(|o| &mut o[..]).collect();
    core.process(&input, &mut output);
    *fed += n_in;
    out.swap_remove(0)
}

/// A raised cosine in log speed (the speed envelope) run as one Hermite
/// piece per call of `n` outputs, from `s0` to `s1` over `total` outputs;
/// every piece lands on its end bits.
fn run_cosine(acc: &mut Accumulator, s0: f64, s1: f64, total: usize, n: usize) {
    use std::f64::consts::PI;
    let (l0, l1) = (s0.ln(), s1.ln());
    let at = |k: f64| -> (f64, f64) {
        let u = (k / total as f64).clamp(0.0, 1.0);
        let l = l0 + (l1 - l0) * 0.5 * (1.0 - (PI * u).cos());
        let dl = (l1 - l0) * 0.5 * PI * (PI * u).sin() / total as f64;
        (l.exp(), l.exp() * dl)
    };
    let mut k = 0usize;
    while k < total {
        let m = n.min(total - k);
        let (_, m0) = at(k as f64);
        let (v1, m1) = at((k + m) as f64);
        let end = if k + m == total { step(s1) } else { step(v1) };
        let need = acc
            .plan(
                m,
                Segment::Piece {
                    slope0: m0,
                    end,
                    slope1: if k + m == total { 0.0 } else { m1 },
                },
            )
            .unwrap();
        acc.commit(need);
        assert_eq!(acc.step(), end);
        k += m;
    }
}

/// The accumulator alone over 100 hours at 768 kHz. At
/// `s = 1 + 2^-20` (exact in f64 and in 64.64) the frames consumed equal
/// the exact rational count; at `s = 1.0123` they equal the count for the
/// quantised step exactly (the offset to the decimal is reported). Ramps
/// end on the target bits.
#[test]
fn accumulator_is_exact_over_100_hours_at_768_khz() {
    let outputs: u128 = 100 * 3600 * 768_000;
    for (s, decimal) in [
        (1.0 + 2f64.powi(-20), None),
        (1.0123, Some((10_123u128, 10_000u128))),
    ] {
        let q = step(s);
        let mut acc = Accumulator::new(q);
        let mut received = 0u128;
        let mut done = 0u128;
        let mut seed = 1;
        while done < outputs {
            // mostly long calls, sometimes odd ones
            let n = if unit(&mut seed) < 0.01 {
                below(&mut seed, 8192)
            } else {
                MAX_CALL_OUTPUTS
            };
            let n = (n as u128).min(outputs - done) as usize;
            let need = acc.plan(n, Segment::Hold).unwrap();
            acc.commit(need);
            received += need as u128;
            done += n as u128;
        }
        // From position 0: output k sits at k Δ, so N outputs need
        // floor((N - 1) Δ) + 1 frames, and the next one sits at N Δ.
        let exact = (((outputs - 1) * q.bits()) >> 64) + 1;
        assert_eq!(received, exact, "s = {s}");
        assert_eq!(acc.position(), outputs * q.bits());
        if q.bits() == (1u128 << 64) + (1u128 << 44) {
            // the rational count: (N - 1)(1 + 2^-20)
            let n1 = outputs - 1;
            assert_eq!(received, n1 + (n1 >> 20) + 1);
        }
        match decimal {
            None => println!("s = 1 + 2^-20: {received} frames after {outputs} outputs, exact"),
            Some((num, den)) => {
                // (N Δ_q - N num/den 2^64) in frames
                let off = outputs as f64 * ((q.bits() * den) as i128 - (num << 64) as i128) as f64
                    / (den as f64 * ONE_F);
                println!(
                    "s = 1.0123: {received} frames after {outputs} outputs, exact for the \
                     quantised step; the quantised step is {off:+.3e} frames from the decimal \
                     1.0123 after 100 h at 768 kHz"
                );
            }
        }
    }
    // Ramps end on the target bits, then hold them.
    let mut acc = Accumulator::new(step(1.0));
    for (s0, s1, total, n) in [
        (1.0, 1.08, 2 * 48_000 / 20, 1024),
        (1.08, 0.5, 2 * 9600, 333),
        (0.5, 2.0, 2 * 19_200, 8192),
        (2.0, 1.000_013, 2 * 960, 4096),
    ] {
        acc.reset(step(s0));
        run_cosine(&mut acc, s0, s1, total, n);
        assert_eq!(acc.step().bits(), step(s1).bits());
        let need = acc.plan(5000, Segment::Hold).unwrap();
        let p = acc.position();
        acc.commit(need);
        assert_eq!(acc.position(), p + 5000 * step(s1).bits());
    }
}

/// At step 1 from position 0 the core is an exact delay of 12 frames
/// (phase 0 is the identity, bit for bit), and at constant steps a band-limited tone comes
/// out at the kernel's transfer function.
#[test]
fn unity_step_is_a_delay_and_tones_pass() {
    // Signed zeros included: phase 0 copies the frame, so -0.0 survives.
    let x: Vec<Vec<f64>> = vec![
        (0..4000)
            .map(|i| match i % 7 {
                0 => -0.0,
                1 => 0.0,
                _ => ((i * 7919) % 1000) as f64 / 1000.0 - 0.5,
            })
            .collect(),
    ];
    let mut core = AdaptiveCore::new(1, 1000, Step::ONE).unwrap();
    let mut fed = 0;
    let mut y = Vec::new();
    for _ in 0..3 {
        y.extend(run_call(
            &mut core,
            &x,
            &mut fed,
            1000,
            Segment::Hold,
            false,
        ));
    }
    for (i, v) in y.iter().enumerate() {
        let want = if i >= 12 { x[0][i - 12] } else { 0.0 };
        assert_eq!(v.to_bits(), want.to_bits(), "frame {i}");
    }
    // A tone at 0.2 cycles per input frame at s = 1.37: amplitude to the
    // passband ripple, phase from the exact positions.
    let f = 0.2;
    let w = 2.0 * std::f64::consts::PI * f;
    let x: Vec<Vec<f64>> = vec![(0..40_000).map(|i| 0.5 * (w * i as f64).sin()).collect()];
    let mut core = AdaptiveCore::new(1, 1000, step(1.37)).unwrap();
    let mut fed = 0;
    let mut worst = 0.0f64;
    let mut k = 0u128;
    for _ in 0..25 {
        let y = run_call(&mut core, &x, &mut fed, 1000, Segment::Hold, false);
        for v in y {
            let t = (k * step(1.37).bits()) as f64 / ONE_F - 12.0;
            if t > 30.0 {
                worst = worst.max((v - 0.5 * (w * t).sin()).abs());
            }
            k += 1;
        }
    }
    println!(
        "tone at 0.2 cycles per frame, s = 1.37: error {:.1} dBFS",
        db(worst)
    );
    assert!(db(worst) < -150.0);
}
