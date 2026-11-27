//! Adversarial tests of the adaptive core (`audivane_dsp_core::adaptive`):
//! extreme slopes, ends and call sizes, bad constructor arguments, the
//! trajectory bound at its worst, the largest call at the largest step,
//! position overflow and the sign of zero.

use audivane_dsp_core::adaptive::{Accumulator, AdaptiveCore, MAX_CALL_OUTPUTS, Segment, Step};

const ONE_F: f64 = 18_446_744_073_709_551_616.0;

/// Fuzzed: every slope magnitude from subnormal to `f64::MAX`
/// (and NaN, infinities), both signs and mixed, against every end and
/// start and many call sizes. `plan` never panics; whatever it accepts
/// keeps the step positive and within the input provisioned, and runs.
#[test]
fn extreme_slopes_ends_and_sizes_never_panic() {
    let mut mags: Vec<f64> = vec![
        0.0,
        f64::MIN_POSITIVE,
        5e-324,
        f64::MAX,
        f64::INFINITY,
        f64::NAN,
    ];
    for e in (-1074..=1023).step_by(7) {
        mags.push(2f64.powi(e));
    }
    // Around the steepness limit (16 / n) for the sizes below.
    for n in [1.0, 3.0, 4.0, 16384.0] {
        for f in [0.5, 0.999, 1.0, 1.001, 2.0] {
            mags.push(f * 16.0 / n);
        }
    }
    let starts = [Step::MIN, Step::ONE, Step::MAX];
    let ends = [
        Step::MIN,
        Step::from_f64(0.5).unwrap(),
        Step::ONE,
        Step::from_f64(1.999_999).unwrap(),
        Step::MAX,
    ];
    let sizes = [1usize, 2, 3, 4, 5, 7, 255, 4096, 16_383, MAX_CALL_OUTPUTS];
    let mut accepted = 0usize;
    let mut tried = 0usize;
    let mut seed = 17u64;
    let mut pick = |len: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize % len
    };
    for &start in &starts {
        let mut core = AdaptiveCore::new(1, MAX_CALL_OUTPUTS, start).unwrap();
        let input = vec![0.5; core.max_in()];
        let mut output = vec![0.0; MAX_CALL_OUTPUTS];
        for _ in 0..6000 {
            let n = sizes[pick(sizes.len())];
            let sign = |b: usize| if b == 0 { 1.0 } else { -1.0 };
            let m0 = sign(pick(2)) * mags[pick(mags.len())];
            let m1 = sign(pick(2)) * mags[pick(mags.len())];
            let end = ends[pick(ends.len())];
            let seg = Segment::Piece {
                slope0: m0,
                end,
                slope1: m1,
            };
            let before = core.accumulator().clone();
            tried += 1;
            match core.plan(n, seg) {
                Err(_) => {
                    assert_eq!(core.planned(), None);
                    assert_eq!(core.accumulator(), &before);
                }
                Ok(need) => {
                    accepted += 1;
                    assert!(need < core.max_in(), "need {need}");
                    let plan = core.planned().unwrap();
                    // the integer trajectory stays positive and in range
                    let mut st = plan.step as i128;
                    let [mut d1, mut d2, d3] = plan.d;
                    for _ in 0..n {
                        let v = st as f64 / ONE_F;
                        assert!(v > 0.25 * (1.0 - 2e-6) && v < 2.0 * (1.0 + 2e-6), "{v}");
                        st += d1;
                        d1 += d2;
                        d2 += d3;
                    }
                    core.process(&[&input[..need]], &mut [&mut output[..n]]);
                    assert_eq!(core.step(), end);
                    assert!(output[..n].iter().all(|x| x.is_finite()));
                    // back to the start for the next case
                    core.reset(start);
                }
            }
        }
    }
    println!("{tried} extreme pieces, {accepted} accepted (and run), none panicked");
    assert!(accepted > 0);
}

/// The analytic deviation bound of `Segment::Piece` at output `k` of `n`
/// (units of 2^-64), plus one unit for the f64 coefficients.
fn deviation_bound(k: usize, n: usize) -> f64 {
    let (k, n) = (k as f64, n as f64);
    k * (n - k) / 4.0 + k * (n - k) * (2.0 * k - n).abs() / 24.0 + k / 2.0 + 1.0
}

/// The construction that puts d3's rounding error at its largest (zero
/// slopes, the end `n^3 / 24 - 4096` units above the start, so the ideal d3
/// sits just off a half). Its deviation reached 1.91e-9 (n = 8192) and
/// 1.53e-8 (n = 16384) relative at step 0.5; with d3's error folded into d2
/// it stays within the analytic bound.
#[test]
fn deviation_at_the_rounding_boundary_is_within_the_bound() {
    for &start in &[Step::MIN, Step::from_f64(0.5).unwrap()] {
        for &n in &[8192usize, MAX_CALL_OUTPUTS] {
            let mut acc = Accumulator::new(start);
            let delta = (n as u128).pow(3) / 24 - 4096;
            let end = Step::from_bits(start.bits() + delta).unwrap();
            let seg = Segment::Piece {
                slope0: 0.0,
                end,
                slope1: 0.0,
            };
            let need = acc.plan(n, seg).unwrap();
            let plan = acc.planned().unwrap();
            let mut step = plan.step as i128;
            let [mut d1, mut d2, d3] = plan.d;
            let (mut peak_abs, mut peak_rel) = (0.0f64, 0.0f64);
            for k in 0..n {
                let t = k as f64 / n as f64;
                let want =
                    start.to_f64() + (end.to_f64() - start.to_f64()) * t * t * (3.0 - 2.0 * t);
                let actual = step as f64 / ONE_F;
                let err = (actual - want).abs();
                assert!(
                    err <= deviation_bound(k, n) / ONE_F + 1e-15,
                    "n {n} k {k}: {err:e}"
                );
                peak_abs = peak_abs.max(err);
                peak_rel = peak_rel.max(err / want);
                step += d1;
                d1 += d2;
                d2 += d3;
            }
            assert!((step - end.bits() as i128).unsigned_abs() <= n as u128 / 2);
            acc.commit(need);
            assert_eq!(acc.step(), end);
            let bound = (0..=n).map(|k| deviation_bound(k, n)).fold(0.0, f64::max) / ONE_F;
            println!(
                "n = {n}, start {}: peak deviation {peak_abs:.3e} frames per output \
                 ({peak_rel:.3e} relative), bound {bound:.3e}",
                start.to_f64()
            );
        }
    }
    // Beyond the call limit the plan is refused, not approximated.
    let mut acc = Accumulator::new(Step::ONE);
    assert!(acc.plan(1 << 20, Segment::Hold).is_err());
}

/// In-range pieces at the call limit fit the provisioned input, including
/// one that peaks at the range tolerance above step 2 and a slow-rising end
/// slope.
#[test]
fn largest_calls_at_the_largest_step_fit_the_input() {
    let n = MAX_CALL_OUTPUTS;
    let nf = n as f64;
    let tol = 1e-6;
    let cases = [
        // a rising end slope
        Segment::Piece {
            slope0: 0.0,
            end: Step::MAX,
            slope1: 0.499 * nf * nf / (6.0 * ONE_F),
        },
        // v = 2 + a k - a k^2 / n peaks at 2 + a n / 4, just inside
        Segment::Piece {
            slope0: 8.0 * tol * 0.999 / nf,
            end: Step::MAX,
            slope1: -8.0 * tol * 0.999 / nf,
        },
        Segment::Hold,
    ];
    for seg in cases {
        for over in [false, true] {
            let mut core = AdaptiveCore::new(2, n, Step::MAX).unwrap();
            // the next output as far ahead as it gets: after a call that
            // leaves it two frames past the received ones
            let mut fed = 0;
            let x = vec![0.1; core.max_in()];
            let mut o = vec![vec![0.0; n]; 2];
            for _ in 0..3 {
                let need = core.plan(n, seg).unwrap_or_else(|e| panic!("{seg:?}: {e}"));
                assert!(need <= 2 * n + 2, "need {need}");
                let n_in = need + usize::from(over && need > 0);
                assert!(
                    n_in <= core.max_in(),
                    "need {need}, max_in {}",
                    core.max_in()
                );
                let (a, b) = o.split_at_mut(1);
                core.process(&[&x[..n_in], &x[..n_in]], &mut [&mut a[0], &mut b[0]]);
                fed += n_in;
            }
            assert!(fed > 0);
        }
    }
}
