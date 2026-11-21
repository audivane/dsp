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

#[test]
fn the_design_margin_is_needed() {
    // Kaiser's beta for 170 dB at 25 taps measures short of 170 dB (about
    // 168.7 dB): the table is designed for 172 dB.
    let mut t = CoreTable::build().unwrap();
    t.beta = kaiser_beta_for_attenuation(TARGET_DB);
    t.i0_beta = bessel_i0(t.beta);
    t.rows = tabulate(t.taps, t.beta, t.fc).unwrap();
    let r = t.measure();
    println!("beta for 170 dB: stopband {:.1} dB", r.exact_stopband_db);
    assert!(r.exact_stopband_db > -TARGET_DB);
    // The safety gate refuses that table.
    let e = crate::design::safety::check_interpolation_table(
        &r,
        TARGET_DB,
        INTERPOLATION_MARGIN_DB,
        MAX_PASSBAND_RIPPLE_DB,
    )
    .unwrap_err();
    assert!(e.to_string().contains("misses its targets"), "{e}");
    // A measurement that is not a number fails too.
    let nan = TableReport {
        stopband_db: f64::NAN,
        ..CoreTable::shared().unwrap().report()
    };
    assert!(
        crate::design::safety::check_interpolation_table(
            &nan,
            TARGET_DB,
            INTERPOLATION_MARGIN_DB,
            MAX_PASSBAND_RIPPLE_DB
        )
        .is_err()
    );
}

/// The table from the `standard` path against the rows sampled straight
/// from the continuous kernel (how the table was built before): the same
/// kernel to rounding and a normalisation, and the same measurements.
#[test]
fn table_from_the_standard_path_matches_the_continuous_kernel() {
    let t = CoreTable::shared().unwrap();
    let taps = t.taps();
    let delay = t.delay();
    let mut sampled = t.rows.clone();
    for (r, row) in sampled.chunks_exact_mut(taps).enumerate() {
        let mu = (r as f64 - 1.0) / PHASES as f64;
        for j in 0..taps {
            row[taps - 1 - j] = t.kernel(j as f64 + mu - delay);
        }
    }
    let diff = t
        .rows
        .iter()
        .zip(&sampled)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    let old = CoreTable {
        taps,
        beta: t.beta,
        half: t.half,
        fc: t.fc,
        i0_beta: t.i0_beta,
        rows: sampled,
        report: t.report(),
    }
    .measure();
    let new = t.report();
    println!(
        "largest row difference {diff:.2e}; sampled: passband {:.2e} dB, stopband {:.2} dB, \
         interpolation error {:.2} dB; standard path: passband {:.2e} dB, stopband {:.2} dB, \
         interpolation error {:.2} dB",
        old.passband_ripple_db,
        old.stopband_db,
        old.interpolation_error_db,
        new.passband_ripple_db,
        new.stopband_db,
        new.interpolation_error_db
    );
    assert!(diff < 1e-12, "{diff:e}");
    assert!((old.stopband_db - new.stopband_db).abs() < 0.1);
    assert!((old.interpolation_error_db - new.interpolation_error_db).abs() < 0.5);
}

#[test]
fn steps_from_f64_are_exact_in_range() {
    let mut seed = 3;
    for _ in 0..10_000 {
        let s = 0.25 + 1.75 * unit(&mut seed);
        let q = Step::from_f64(s).unwrap();
        assert_eq!(q.to_f64(), s);
    }
    assert_eq!(Step::from_f64(1.0), Some(Step::ONE));
    assert_eq!(Step::from_f64(2.0), Some(Step::MAX));
    assert_eq!(Step::from_f64(0.25), Some(Step::MIN));
    for bad in [0.0, 0.2, 2.000_001, f64::NAN, f64::INFINITY, -1.0] {
        assert_eq!(Step::from_f64(bad), None, "{bad}");
    }
    assert_eq!(Step::from_bits(Step::MAX.bits() + 1), None);
    assert_eq!(Step::from_bits(Step::MIN.bits() - 1), None);
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

/// Steps over `k` outputs by the per-output adds: the positions of
/// outputs `0..=k`, and the step after them.
fn simulate(pos: u128, step: u128, d: [i128; 3], k: usize) -> (Vec<u128>, i128) {
    let mut p = pos;
    let mut st = step as i128;
    let [mut d1, mut d2, d3] = d;
    let mut out = Vec::with_capacity(k + 1);
    for _ in 0..k {
        out.push(p);
        assert!(st > 0);
        p += st as u128;
        st += d1;
        d1 += d2;
        d2 += d3;
    }
    out.push(p);
    (out, st)
}

/// The closed form against the per-output loop for
/// random positions, steps and differences: pieces (ramps crossing many
/// integers, some reversing direction inside the call) and raw random
/// differences.
#[test]
fn closed_form_positions_equal_the_per_output_loop() {
    let mut seed = 5;
    let mut cases = 0;
    while cases < 400 {
        let n = match cases % 4 {
            0 => 1 + below(&mut seed, 8),
            1 => 1 + below(&mut seed, MAX_CALL_OUTPUTS),
            2 => 8192,
            _ => 1 + below(&mut seed, 3000),
        };
        let pos = ((below(&mut seed, 3) as u128) << 64) | (unit(&mut seed) * ONE_F) as u64 as u128;
        let s0 = step(0.5 + 1.5 * unit(&mut seed)).bits();
        let v0 = s0 as f64 / ONE_F;
        let (d, seg) = if cases % 3 == 2 {
            // raw differences small enough to keep the step in range
            let nn = n as f64;
            let r = |seed: &mut u64, scale: f64| (lcg(seed) * scale * ONE_F) as i128;
            let d = [
                r(&mut seed, 0.2 / nn),
                r(&mut seed, 0.2 / (nn * nn)),
                r(&mut seed, 0.2 / (nn * nn * nn)),
            ];
            (d, None)
        } else {
            let seg = random_segment(&mut seed, v0, n);
            let Segment::Piece {
                slope0,
                end,
                slope1,
            } = seg
            else {
                continue;
            };
            match piece_differences(s0, slope0, end.bits(), slope1, n) {
                Ok(d) => (d, Some(seg)),
                Err(_) => continue,
            }
        };
        let (ps, st) = simulate(pos, s0, d, n);
        if ps.windows(2).any(|w| w[1] <= w[0]) || st <= 0 {
            continue; // the raw differences left the range
        }
        for (k, p) in ps.iter().enumerate() {
            if k < 64 || k % 97 == 0 || k + 2 >= ps.len() {
                assert_eq!(
                    position_after(pos, s0, d, k),
                    Some(*p),
                    "case {cases} k {k}"
                );
            }
        }
        // The plan's count for a piece: the last output's newest frame,
        // minus what is retained (positions count from the first retained
        // frame, so frames are retained only when it is the newest).
        if let Some(seg) = seg {
            for retained in 0..=MAX_RETAINED {
                if retained > 0 && pos >> 64 != 0 {
                    continue;
                }
                let mut acc = Accumulator {
                    pos,
                    step: s0,
                    retained,
                    base: 0,
                    pending: None,
                };
                let want = ((ps[n - 1] >> 64) as usize + 1).saturating_sub(retained);
                assert_eq!(acc.plan(n, seg).unwrap(), want);
                assert_eq!(acc.planned().unwrap().d, d);
            }
        }
        cases += 1;
    }
}

/// Random pulls through the accumulator, each rounded
/// up to whole source frames before a ×2 (`2 ceil(need / 2)` frames):
/// the over-pull is at most 1, at most 2 frames are retained, every output's
/// newest frame was received, and `need` is the least that does it.
#[test]
fn pull_accounting_over_pull_and_retained_bounds() {
    let mut seed = 77;
    let mut acc = Accumulator::new(step(1.0));
    let mut received = 0u64;
    let (mut max_over, mut max_ret) = (0usize, 0usize);
    for _ in 0..4000 {
        let n = if unit(&mut seed) < 0.05 {
            0
        } else {
            below(&mut seed, 2 * 8192 + 1)
        };
        let v0 = acc.step().to_f64();
        let seg = random_segment(&mut seed, v0, n);
        let need = match acc.plan(n, seg) {
            Ok(need) => need,
            Err(_) => acc.plan(n, Segment::Hold).unwrap(),
        };
        let planned = acc.planned().unwrap();
        let n_in = 2 * need.div_ceil(2);
        max_over = max_over.max(n_in - need);
        let p0 = acc.position();
        let before = received;
        acc.commit(n_in);
        received += n_in as u64;
        max_ret = max_ret.max(acc.retained());
        assert_eq!(acc.frames_in(), received);
        if n > 0 {
            let last = position_after(p0, planned.step, planned.d, n - 1).unwrap();
            let newest = (last >> 64) as u64;
            assert!(newest < received, "starved");
            if need > 0 {
                // one frame fewer would have starved the last output
                assert_eq!(newest, before + need as u64 - 1);
            }
            let next = (acc.position() >> 64) as u64;
            assert_eq!(acc.retained() as u64, received.saturating_sub(next));
            assert_eq!(acc.step().bits(), planned.end);
        }
    }
    println!("4000 pulls: over-pull at most {max_over}, retained at most {max_ret}");
    assert!(max_over <= MAX_OVERPULL);
    assert!(max_ret <= MAX_RETAINED);
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

/// Zero calls (with any segment) change nothing, and
/// an abandoned plan (or a refused one) leaves no trace.
#[test]
fn zero_calls_and_abandoned_plans_leave_no_trace() {
    let mut seed = 9;
    let x: Vec<Vec<f64>> = (0..2)
        .map(|_| (0..400_000).map(|_| lcg(&mut seed)).collect())
        .collect();
    let mut a = AdaptiveCore::new(2, 2048, step(0.9)).unwrap();
    let mut b = AdaptiveCore::new(2, 2048, step(0.9)).unwrap();
    let (mut fa, mut fb) = (0, 0);
    for _ in 0..150 {
        let n = 1 + below(&mut seed, 2048);
        let seg = random_segment(&mut seed, a.step().to_f64(), n);
        let seg = if a.plan(n, seg).is_ok() {
            seg
        } else {
            Segment::Hold
        };
        let over = unit(&mut seed) < 0.5;
        // `a` gets zero calls with stray segments, abandoned plans and a
        // refused plan first; `b` only the real call.
        for _ in 0..below(&mut seed, 3) {
            let stray = random_segment(&mut seed, a.step().to_f64(), 7);
            assert_eq!(a.plan(0, stray).unwrap(), 0);
            if unit(&mut seed) < 0.7 {
                a.process(&[&[], &[]], &mut [&mut [], &mut []]);
            }
            let m = 1 + below(&mut seed, 2048);
            let other = random_segment(&mut seed, a.step().to_f64(), m);
            let _ = a.plan(m, other); // abandoned
            let _ = a.plan(2049, Segment::Hold); // refused: too many
            let _ = a.plan(
                m,
                Segment::Piece {
                    slope0: 1.0,
                    end: Step::MAX,
                    slope1: 0.0,
                },
            ); // refused: out of range (or abandoned)
        }
        let ya = run_call(&mut a, &x, &mut fa, n, seg, over);
        let yb = run_call(&mut b, &x, &mut fb, n, seg, over);
        assert_eq!(fa, fb);
        assert_eq!(a.accumulator(), b.accumulator());
        for (p, q) in ya.iter().zip(&yb) {
            assert_eq!(p.to_bits(), q.to_bits());
        }
    }
}

/// The analytic bound on a piece's deviation from its ideal cubic at
/// output `k` of `n`, in units of 2^-64 (see `Segment::Piece`): with
/// rounding errors |e3|, |ρ2|, |r| <= 1/2 (d3, the compensated d2, d1),
/// `E(k) = k(k-n) (ρ2/2 + e3 (2k-n)/12) + k r`. One unit of slack for the
/// f64 coefficients.
fn deviation_bound(k: usize, n: usize) -> f64 {
    let (k, n) = (k as f64, n as f64);
    k * (n - k) / 4.0 + k * (n - k) * (2.0 * k - n).abs() / 24.0 + k / 2.0 + 1.0
}

/// The largest deviation bound over a piece of `n` outputs.
fn deviation_bound_max(n: usize) -> f64 {
    (0..=n).map(|k| deviation_bound(k, n)).fold(0.0, f64::max)
}

/// A piece's per-output steps follow the cubic Hermite through its end
/// values and slopes, and land on the end bits (the endpoint rule).
#[test]
fn pieces_follow_the_hermite_cubic_and_land_on_their_end() {
    let mut seed = 21;
    let mut worst_land = 0u128;
    let mut worst_dev = [0.0f64; 2];
    let mut tested = 0;
    while tested < 300 {
        let n = [1usize, 2, 3, 17, 1024, 2048, 8191, 16_384][tested % 8];
        let v0 = 0.5 + 1.5 * unit(&mut seed);
        let v1 = 0.5 + 1.5 * unit(&mut seed);
        let scale = (v1 - v0).abs() / n as f64;
        let (m0, m1) = (lcg(&mut seed) * scale, lcg(&mut seed) * scale);
        let s0 = step(v0).bits();
        let Ok(d) = piece_differences(s0, m0, step(v1).bits(), m1, n) else {
            continue;
        };
        let (_, st) = simulate(0, s0, d, n);
        let land = (st - step(v1).bits() as i128).unsigned_abs();
        assert!(land <= (n as u128).div_ceil(2), "n {n}: {land}");
        worst_land = worst_land.max(land);
        // the step at output k against the cubic
        let nf = n as f64;
        let c = (3.0 * (v1 - v0) - (2.0 * m0 + m1) * nf) / (nf * nf);
        let dd = (2.0 * (v0 - v1) + (m0 + m1) * nf) / (nf * nf * nf);
        let mut stp = s0 as i128;
        let [mut d1, mut d2, d3] = d;
        for k in 0..n {
            let kf = k as f64;
            let want = v0 + m0 * kf + c * kf * kf + dd * kf * kf * kf;
            let dev = (stp as f64 / ONE_F - want).abs();
            let bound = deviation_bound(k, n) / ONE_F + 1e-15;
            assert!(dev <= bound, "n {n} k {k}: {dev:e} > {bound:e}");
            worst_dev[usize::from(n > 8192)] = worst_dev[usize::from(n > 8192)].max(dev / want);
            stp += d1;
            d1 += d2;
            d2 += d3;
        }
        tested += 1;
    }
    println!(
        "pieces land within {worst_land} units of 2^-64 ({:.1e} frames per output) before the \
         snap; inside, random pieces measured {:.1e} (n <= 8192) and {:.1e} (n = 16384) relative \
         of the cubic; the analytic bound is {:.2e} and {:.2e} frames per output",
        worst_land as f64 / ONE_F,
        worst_dev[0],
        worst_dev[1],
        deviation_bound_max(8192) / ONE_F,
        deviation_bound_max(16_384) / ONE_F
    );
    // through the accumulator: the step after the piece is the end, exactly
    let mut acc = Accumulator::new(step(1.0));
    acc.plan(
        8192,
        Segment::Piece {
            slope0: 0.0,
            end: step(1.08),
            slope1: 0.0,
        },
    )
    .unwrap();
    acc.commit(acc.planned().unwrap().need);
    assert_eq!(acc.step(), step(1.08));
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

/// `reset` puts the core back to a fresh one.
#[test]
fn reset_equals_a_fresh_core() {
    let mut seed = 4;
    let x: Vec<Vec<f64>> = vec![(0..50_000).map(|_| lcg(&mut seed)).collect()];
    let mut a = AdaptiveCore::new(1, 512, step(1.3)).unwrap();
    let mut fa = 0;
    for _ in 0..20 {
        run_call(&mut a, &x, &mut fa, 333, Segment::Jump(step(0.7)), true);
    }
    a.plan(100, Segment::Hold).unwrap(); // pending, then reset
    a.reset(step(1.1));
    let mut b = AdaptiveCore::new(1, 512, step(1.1)).unwrap();
    assert_eq!(a.accumulator(), b.accumulator());
    let (mut f1, mut f2) = (0, 0);
    for n in [1usize, 512, 77, 0, 300] {
        let ya = run_call(&mut a, &x, &mut f1, n, Segment::Hold, false);
        let yb = run_call(&mut b, &x, &mut f2, n, Segment::Hold, false);
        assert_eq!(ya, yb);
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

/// The absolute counters (frames received, the absolute
/// position) wrap modulo 2^64 frames instead of overflowing.
#[test]
fn absolute_counters_wrap() {
    let mut core = AdaptiveCore::new(1, 4, Step::ONE).unwrap();
    core.acc.base = u64::MAX;
    assert_eq!(core.plan(1, Segment::Hold).unwrap(), 1);
    core.process(&[&[0.25]], &mut [&mut [0.0]]);
    assert_eq!(core.accumulator().frames_in(), 0);
    assert_eq!(core.accumulator().position(), 0);
    assert_eq!(core.plan(2, Segment::Hold).unwrap(), 2);
    let mut out = [0.0; 2];
    core.process(&[&[0.5, 0.75]], &mut [&mut out]);
    assert_eq!(core.accumulator().frames_in(), 2);
    assert_eq!(core.accumulator().position(), 2 << 64);
}
