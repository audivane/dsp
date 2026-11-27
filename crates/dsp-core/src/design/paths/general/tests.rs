//! The new combinations: each builds through `prepare` (its budgets, its
//! promise measured, the safety gate), or is refused with a typed error.

use super::*;
use crate::design::{Accept, BandSpec, FlattopSet, Numerics, Rates, Sampling, Stepped, prepare};
use audivane_protocol::{OutputConfig, RateRatio};

pub(crate) fn cx(source_rate: u32, ratio: RateRatio) -> BuildContext {
    BuildContext {
        source_rate,
        ratio,
        channels: 1,
        max_block: 256,
        pipelines: budget::RATIONAL_SWAP_PIPELINES,
        output: OutputConfig::default(),
        workspace: budget::DESIGN_WORKSPACE_FLOOR,
    }
}

pub(crate) fn sinc(window: Window, length: Length, cutoff: Cutoff, phase: Phase) -> KernelDesign {
    KernelDesign {
        shape: Shape::WindowedSinc {
            window,
            length,
            cutoff,
        },
        decimation: Decimation::Scale {
            odd: crate::design::OddRule::Down,
            base_taps: None,
        },
        phase,
        preserve_samples: false,
        rates: Rates {
            accept: Accept::Any,
            fallback_window: None,
        },
        numerics: Numerics {
            path: ArithmeticPath::Fir,
            grid_cutoff: GridCutoff::Scaled,
        },
    }
}

pub(crate) fn windows() -> Vec<(&'static str, Window)> {
    use Sampling::*;
    vec![
        (
            "kaiser",
            Window::Kaiser {
                beta: Some(14.0),
                sampling: Symmetric,
            },
        ),
        (
            "kaiser_auto_beta",
            Window::Kaiser {
                beta: None,
                sampling: Periodic,
            },
        ),
        (
            "gaussian",
            Window::Gaussian {
                alpha: 0.15,
                sampling: Symmetric,
            },
        ),
        (
            "flattop",
            Window::Flattop {
                set: FlattopSet::Hft196d,
                sampling: Symmetric,
            },
        ),
        (
            "chebyshev",
            Window::Chebyshev {
                sidelobe_ratio: 1e-8,
                sampling: Symmetric,
            },
        ),
        (
            "chebyshev_periodic",
            Window::Chebyshev {
                sidelobe_ratio: 1e-9,
                sampling: Periodic,
            },
        ),
        (
            "dpss",
            Window::Dpss {
                nw: Some(5.0),
                sampling: Symmetric,
            },
        ),
        (
            "dpss_auto_nw",
            Window::Dpss {
                nw: None,
                sampling: Symmetric,
            },
        ),
        ("nuttall", Window::Nuttall { sampling: Periodic }),
        (
            "hann",
            Window::Hann {
                sampling: Symmetric,
            },
        ),
    ]
}

fn band(pe: f64, se: f64, a: f64) -> Cutoff {
    Cutoff::Band(BandSpec {
        passband_end: pe,
        stopband_start: se,
        passband_end_hz: None,
        attenuation_db: a,
    })
}

fn ok(k: &KernelDesign, src: u32, ratio: RateRatio) -> crate::design::Prepared {
    prepare(&FilterDesign::Kernel(k.clone()), &cx(src, ratio))
        .unwrap_or_else(|e| panic!("{k:?} at {ratio}: {e}"))
}

/// The attenuation of a prepared design over its promise, measured
/// independently of [`claim`]: [`independent_error`] on its kernel.
fn measured(p: &crate::design::Prepared, claim: Claim) -> f64 {
    let k = p.kernel.as_ref().unwrap();
    let grid = &p.plan.grid;
    let edge = 2.0 * grid.design_ratio.max(1) as f64;
    let err = independent_error(
        &k.taps,
        grid.up.max(1) as f64,
        claim.pass / edge,
        (claim.stop / edge).min(0.5),
    );
    -20.0 * err.log10()
}

/// The protected error of `h` (against unity to `pass`, zero from `stop`,
/// cycles per sample; `gain` the passband gain), by a method of its own:
/// a 16-points-per-tap FFT grid finds the lobes, and every lobe peak
/// within 3 % of the largest is found by golden-section search over one
/// grid step either side, evaluating the response directly (Horner); the
/// band edges are evaluated exactly.
pub(crate) fn independent_error(h: &[f64], gain: f64, pass: f64, stop: f64) -> f64 {
    use rustfft::num_complex::Complex64;
    let direct = |f: f64| -> f64 {
        let z = Complex64::from_polar(1.0, -2.0 * std::f64::consts::PI * f);
        let v = h
            .iter()
            .rev()
            .fold(Complex64::new(0.0, 0.0), |a, &t| a * z + t);
        let a = v.norm() / gain;
        if f <= pass { (a - 1.0).abs() } else { a }
    };
    let n = (h.len().max(2) * 16).next_power_of_two();
    let mut planner = realfft::RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let mut time = fft.make_input_vec();
    time[..h.len()].copy_from_slice(h);
    let mut spec = fft.make_output_vec();
    fft.process(&mut time, &mut spec).unwrap();
    let e: Vec<Option<f64>> = spec
        .iter()
        .enumerate()
        .map(|(k, c)| {
            let f = k as f64 / n as f64;
            let a = c.norm() / gain;
            if f <= pass {
                Some((a - 1.0).abs())
            } else if f >= stop {
                Some(a)
            } else {
                None
            }
        })
        .collect();
    let mut worst = direct(pass).max(direct(stop));
    let grid_max = e.iter().flatten().fold(0.0f64, |m, &v| m.max(v));
    worst = worst.max(grid_max);
    let mut peaks: Vec<(f64, usize)> = (0..e.len())
        .filter_map(|k| {
            let v = e[k]?;
            let left = k.checked_sub(1).and_then(|j| e[j]).unwrap_or(0.0);
            let right = e.get(k + 1).copied().flatten().unwrap_or(0.0);
            (v >= left && v >= right && v >= grid_max / 1.03).then_some((v, k))
        })
        .collect();
    peaks.sort_by(|a, b| b.0.total_cmp(&a.0));
    let step = 1.0 / n as f64;
    for &(_, k) in peaks.iter().take(64) {
        let f0 = k as f64 * step;
        let (lo, hi) = if f0 <= pass { (0.0, pass) } else { (stop, 0.5) };
        let (mut a, mut b) = ((f0 - step).max(lo), (f0 + step).min(hi));
        let g = 0.5 * (5f64.sqrt() - 1.0);
        let (mut x1, mut x2) = (b - g * (b - a), a + g * (b - a));
        let (mut y1, mut y2) = (direct(x1), direct(x2));
        for _ in 0..24 {
            if y1 > y2 {
                b = x2;
                (x2, y2) = (x1, y1);
                x1 = b - g * (b - a);
                y1 = direct(x1);
            } else {
                a = x1;
                (x1, y1) = (x2, y2);
                x2 = a + g * (b - a);
                y2 = direct(x2);
            }
        }
        worst = worst.max(y1).max(y2);
    }
    worst
}

/// Every window and sampling builds a linear windowed sinc on every
/// preset path that takes it.
#[test]
fn windows_and_samplings_on_every_path() {
    use ArithmeticPath::*;
    let fc = Cutoff::Fraction(Stepped::Fixed(0.9));
    for (name, w) in windows() {
        if name.ends_with("_auto_beta") || name.ends_with("_auto_nw") {
            continue;
        }
        for path in [
            Fir,
            FirAfter,
            ExactPiInverseAfter,
            ExactOverRatioBefore,
            ExactPiInversePairwise,
        ] {
            let mut k = sinc(
                w,
                Length::PerInput(Stepped::Fixed(64)),
                fc.clone(),
                Phase::Linear,
            );
            k.numerics.path = path;
            let p = ok(&k, 44_100, RateRatio::integer(4));
            assert_eq!(p.kernel.unwrap().taps.len(), 256, "{name} {path:?}");
        }
    }
    // The Tukey window, periodic.
    let k = sinc(
        Window::Tukey {
            taper: 0.5,
            sampling: Sampling::Periodic,
        },
        Length::PerInput(Stepped::Fixed(64)),
        Cutoff::Fraction(Stepped::Fixed(0.9)),
        Phase::Linear,
    );
    ok(&k, 44_100, RateRatio::integer(4));
}

/// A budget deeper than the window's sidelobes is refused before anything
/// is designed.
#[test]
fn deeper_than_the_window_is_refused() {
    let k = sinc(
        Window::Chebyshev {
            sidelobe_ratio: 1e-6,
            sampling: Sampling::Symmetric,
        },
        Length::Auto(FilterErrorBudget::Db(150.0)),
        band(0.9, 1.0, 150.0),
        Phase::Linear,
    );
    let e =
        crate::design::feasibility(&FilterDesign::Kernel(k), &cx(44_100, RateRatio::integer(4)))
            .unwrap_err();
    assert!(
        matches!(&e, DesignError::Claim(m) if m.starts_with("the window's sidelobes (-122.0 dB) are above the budget")),
        "{e}"
    );
}

/// Stop at Nyquist: the stopband from the Nyquist up at the window's
/// nominal attenuation.
#[test]
fn nyquist_stop_keeps_the_nominal_stopband() {
    for (name, w) in windows() {
        if name.ends_with("_auto_beta") || name.ends_with("_auto_nw") {
            continue;
        }
        let k = sinc(
            w,
            Length::PerInput(Stepped::Fixed(255)),
            Cutoff::NyquistStop,
            Phase::Linear,
        );
        let p = ok(&k, 44_100, RateRatio::integer(8));
        let a = window_model::nominal_db(&w).unwrap();
        let m = measured(
            &p,
            Claim {
                pass: 0.0,
                stop: 1.0,
                db: a,
            },
        );
        assert!(m >= a, "{name}: {m} dB");
    }
    // Too short for its window: refused with the length it needs.
    let k = sinc(
        Window::Flattop {
            set: FlattopSet::Hft248d,
            sampling: Sampling::Periodic,
        },
        Length::PerInput(Stepped::Fixed(15)),
        Cutoff::NyquistStop,
        Phase::Linear,
    );
    let e = prepare(&FilterDesign::Kernel(k), &cx(44_100, RateRatio::integer(8))).unwrap_err();
    assert!(
        e.to_string().starts_with("stop at Nyquist needs at least"),
        "{e}"
    );
}

/// The input samples come through a `preserve_samples` kernel unchanged.
#[test]
fn preserve_samples_keeps_the_input() {
    for (name, w) in windows() {
        if name.ends_with("_auto_beta") || name.ends_with("_auto_nw") {
            continue;
        }
        let mut k = sinc(
            w,
            Length::PerInput(Stepped::Fixed(64)),
            Cutoff::Fraction(Stepped::Fixed(1.0)),
            Phase::Linear,
        );
        k.preserve_samples = true;
        for l in [2usize, 3, 4] {
            let p = ok(&k, 44_100, RateRatio::integer(l as u32));
            let h = &p.kernel.unwrap();
            let c = h.delay;
            assert_eq!(h.taps[c], 1.0, "{name} x{l}");
            assert!(
                h.taps
                    .iter()
                    .skip(c % l)
                    .step_by(l)
                    .enumerate()
                    .all(|(i, &v)| i == c / l || v == 0.0),
                "{name} x{l}"
            );
        }
    }
}

/// A peak between the grid's points (48 to 96 kHz, a periodic Chebyshev
/// window of -70 dB sidelobes): the 30-tap kernel's
/// grid reads 71.8758 dB, its response 71.8698 dB, under the 71.873 dB it
/// promises. The check measures the peak itself, and what builds keeps the
/// promise by an independent evaluation.
#[test]
fn a_peak_between_grid_points_is_measured() {
    let w = Window::Chebyshev {
        sidelobe_ratio: 10f64.powf(-3.5),
        sampling: Sampling::Periodic,
    };
    let a = 71.873;
    let c = cx(48_000, RateRatio::integer(2));
    let grid = c.grid();
    for length in [Length::Band, Length::Auto(FilterErrorBudget::Db(a))] {
        let k = sinc(w, length, band(0.58, 1.373454106280193, a), Phase::Linear);
        let g = recipe(&k, 48_000, &grid, Some(30)).unwrap();
        let claim = g.claim.unwrap();
        let h = kernel(&k, &g, &grid).unwrap();
        let (pass, stop) = (claim.pass / 4.0, claim.stop / 4.0);
        let (on_grid, _) = crate::design::auto::protected_error_bounded(
            &mut realfft::RealFftPlanner::new(),
            &h.taps,
            2.0,
            pass,
            stop,
            1.0,
        );
        let truth = -20.0 * independent_error(&h.taps, 2.0, pass, stop).log10();
        assert!(-20.0 * on_grid.log10() > a && truth < a, "{truth}");
        let m = claim::measure(&mut realfft::RealFftPlanner::new(), &h.taps, &claim, &grid);
        assert!(!m.keeps(&claim), "{m:?}");
        assert!((m.db - truth).abs() < 1e-4, "{m:?} against {truth}");
        match prepare(&FilterDesign::Kernel(k.clone()), &c) {
            Ok(p) => {
                let got = measured(&p, claim);
                assert!(got >= a, "{k:?}: {got} dB");
            }
            Err(DesignError::Claim(_)) => {}
            Err(e) => panic!("{e}"),
        }
    }
}

/// Every candidate of the automatic search is admitted with the design's
/// own limits before it is built (a 70 dB minimum-phase
/// cepstral Kaiser design over 0.90-0.92 at 2560/147, whose first
/// real-grid candidate is over the rational tap limit and its transform
/// over the workspace cap).
#[test]
fn automatic_candidates_are_admitted_before_they_are_built() {
    let k = sinc(
        Window::Kaiser {
            beta: None,
            sampling: Sampling::Symmetric,
        },
        Length::Auto(FilterErrorBudget::Db(70.0)),
        band(0.90, 0.92, 70.0),
        Phase::Minimum(PhaseMethod::Cepstral),
    );
    let c = cx(44_100, RateRatio::new(2560, 147));
    let e = crate::design::feasibility(&FilterDesign::Kernel(k), &c).unwrap_err();
    assert!(matches!(e, DesignError::Budget(_)), "{e:?}");
}
