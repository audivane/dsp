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

/// `w` sampled symmetric (what a reflected phase takes).
pub(crate) fn symmetric(w: Window) -> Window {
    let s = Sampling::Symmetric;
    match w {
        Window::Kaiser { beta, .. } => Window::Kaiser { beta, sampling: s },
        Window::Gaussian { alpha, .. } => Window::Gaussian { alpha, sampling: s },
        Window::Flattop { set, .. } => Window::Flattop { set, sampling: s },
        Window::Chebyshev { sidelobe_ratio, .. } => Window::Chebyshev {
            sidelobe_ratio,
            sampling: s,
        },
        Window::Nuttall { .. } => Window::Nuttall { sampling: s },
        Window::Hann { .. } => Window::Hann { sampling: s },
        w => w,
    }
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

/// The band length keeps its attenuation on every modelled window, at an
/// integer, a rational and a decimating ratio.
#[test]
fn band_lengths_keep_their_promise() {
    for (name, w) in windows() {
        let a = match name {
            "hann" => 70.0,
            "nuttall" => 100.0,
            "kaiser" => 110.0,
            "dpss" => 130.0,
            _ => 140.0,
        };
        let k = sinc(w, Length::Band, band(0.9, 1.0, a), Phase::Linear);
        for (src, ratio) in [
            (44_100, RateRatio::integer(4)),
            (44_100, RateRatio::from_rates(44_100, 48_000)),
            (96_000, RateRatio::from_rates(96_000, 44_100)),
        ] {
            let p = ok(&k, src, ratio);
            let m = measured(
                &p,
                Claim {
                    pass: 0.9,
                    stop: 1.0,
                    db: a,
                },
            );
            assert!(m >= a, "{name} at {ratio}: {m} dB");
        }
    }
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

/// The automatic length keeps its budget, on its own grid, with its phase.
#[test]
fn automatic_lengths_keep_their_budget() {
    for (name, w) in windows() {
        let a: f64 = match name {
            "hann" => 60.0,
            "nuttall" => 90.0,
            _ => 120.0,
        };
        // The cepstral transform keeps a passband to about 95 to 165 dB
        // (the calibration below): its budget stays above that floor.
        for (phase, a) in [
            (Phase::Linear, a),
            (Phase::Minimum(PhaseMethod::Cepstral), a.min(90.0)),
            (Phase::Minimum(PhaseMethod::Reflect), a),
        ] {
            let reflect = matches!(phase, Phase::Minimum(PhaseMethod::Reflect));
            let w = if reflect { symmetric(w) } else { w };
            let k = sinc(
                w,
                Length::Auto(FilterErrorBudget::Db(a)),
                band(0.9, 1.0, a),
                phase,
            );
            for ratio in [RateRatio::integer(8), RateRatio::from_rates(44_100, 48_000)] {
                let d = FilterDesign::Kernel(k.clone());
                let p = match prepare(&d, &cx(44_100, ratio)) {
                    Ok(p) => p,
                    // The reflection's contour method cannot separate a
                    // long Chebyshev kernel's zeros (a known limitation).
                    Err(DesignError::Phase(_)) if reflect && name.starts_with("chebyshev") => {
                        continue;
                    }
                    Err(e) => panic!("{name} {phase:?} at {ratio}: {e}"),
                };
                let m = measured(
                    &p,
                    Claim {
                        pass: 0.9,
                        stop: 1.0,
                        db: a,
                    },
                );
                assert!(m >= a, "{name} {phase:?} at {ratio}: {m} dB");
            }
        }
    }
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

/// Reflected minimum and intermediate phases on the preset paths, with
/// each window's contract.
#[test]
fn reflected_phases_on_every_window() {
    let fc = Cutoff::Fraction(Stepped::Fixed(0.9));
    let mut designs: Vec<(String, KernelDesign)> = windows()
        .into_iter()
        .filter(|(n, _)| !n.ends_with("_auto_beta") && !n.ends_with("_auto_nw"))
        .flat_map(|(name, w)| {
            let fc = fc.clone();
            [
                Phase::Minimum(PhaseMethod::Reflect),
                Phase::Intermediate {
                    mix: 0.5,
                    method: PhaseMethod::Reflect,
                },
            ]
            .map(move |phase| {
                (
                    format!("{name} {phase:?}"),
                    sinc(
                        symmetric(w),
                        Length::PerInput(Stepped::Fixed(63)),
                        fc.clone(),
                        phase,
                    ),
                )
            })
        })
        .collect();
    let mut lanczos = sinc(
        Window::Hann {
            sampling: Sampling::Symmetric,
        },
        Length::PerInput(Stepped::Fixed(1)),
        fc,
        Phase::Minimum(PhaseMethod::Reflect),
    );
    lanczos.shape = Shape::Lanczos { lobes: 23 };
    lanczos.rates.accept = Accept::Upsampling;
    designs.push(("lanczos".into(), lanczos));
    for (name, k) in designs {
        for ratio in [RateRatio::integer(4), RateRatio::from_rates(44_100, 48_000)] {
            match prepare(&FilterDesign::Kernel(k.clone()), &cx(44_100, ratio)) {
                Ok(p) => assert!(p.kernel.unwrap().taps.len() >= 63, "{name} at {ratio}"),
                // A short intermediate blend can need more than four
                // times its length (the reflection's own refusal).
                Err(DesignError::Phase(e)) if name.contains("Intermediate") => {
                    assert!(e.to_string().contains("more than 4 times"), "{name}: {e}");
                }
                Err(e) => panic!("{name} at {ratio}: {e}"),
            }
        }
    }
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

/// The grid-cutoff and decimation pairings at decimating ratios: the
/// kernel's branches sum to 1, the claim holds, and `design_ratio` with a
/// scaled length is refused.
#[test]
fn grid_cutoff_and_decimation_pairings() {
    let w = Window::Kaiser {
        beta: Some(14.0),
        sampling: Sampling::Periodic,
    };
    for (grid_cutoff, decimation) in [
        (GridCutoff::DesignRatio, Decimation::Keep),
        (GridCutoff::Scaled, Decimation::Keep),
    ] {
        let mut k = sinc(w, Length::Band, band(0.9, 1.0, 110.0), Phase::Linear);
        k.numerics.grid_cutoff = grid_cutoff;
        k.decimation = decimation;
        for (src, ratio) in [
            (96_000, RateRatio::from_rates(96_000, 48_000)),
            (48_000, RateRatio::from_rates(48_000, 44_100)),
            (44_100, RateRatio::integer(4)),
        ] {
            ok(&k, src, ratio);
        }
    }
    let mut k = sinc(w, Length::Band, band(0.9, 1.0, 110.0), Phase::Linear);
    k.numerics.grid_cutoff = GridCutoff::DesignRatio;
    let e = prepare(
        &FilterDesign::Kernel(k),
        &cx(96_000, RateRatio::from_rates(96_000, 48_000)),
    )
    .unwrap_err();
    assert!(matches!(e, DesignError::Field(_)), "{e:?}");
}

/// A cepstral blend between the exact ones, a mix of 1 (the linear kernel)
/// and 0 (the minimum phase), and a fixed length.
#[test]
fn cepstral_blends_and_fixed_lengths() {
    let w = Window::Chebyshev {
        sidelobe_ratio: 1e-8,
        sampling: Sampling::Symmetric,
    };
    let fc = Cutoff::Fraction(Stepped::Fixed(0.9));
    let at = |mix| {
        let k = sinc(
            w,
            Length::PerInput(Stepped::Fixed(63)),
            fc.clone(),
            Phase::Intermediate {
                mix,
                method: PhaseMethod::Cepstral,
            },
        );
        ok(&k, 44_100, RateRatio::integer(4)).kernel.unwrap()
    };
    assert_eq!(at(0.3).taps.len(), 4 * 252);
    assert_eq!(at(1.0).taps.len(), 252);
    assert_eq!(at(0.0).taps.len(), 252);
    let k = sinc(w, Length::Fixed(1000), fc, Phase::Linear);
    let p = ok(&k, 44_100, RateRatio::integer(4));
    assert_eq!(p.kernel.unwrap().taps.len(), 1000);
}

/// Calibration: how deep a cepstral or reflected phase keeps a deep
/// linear design (its protected error), per window and length.
#[test]
#[ignore]
fn calibrate_phase_floors() {
    let fc = band(0.9, 1.0, 100.0);
    for (name, w) in windows() {
        if name.ends_with("_auto_beta") || name.ends_with("_auto_nw") {
            continue;
        }
        for t in [31usize, 127, 511] {
            for ratio in [2u32, 8] {
                let mut row = Vec::new();
                for phase in [
                    Phase::Linear,
                    Phase::Minimum(PhaseMethod::Cepstral),
                    Phase::Intermediate {
                        mix: 0.5,
                        method: PhaseMethod::Cepstral,
                    },
                    Phase::Minimum(PhaseMethod::Reflect),
                ] {
                    let mut k = sinc(w, Length::PerInput(Stepped::Fixed(t)), fc.clone(), phase);
                    k.shape = Shape::WindowedSinc {
                        window: w,
                        length: Length::PerInput(Stepped::Fixed(t)),
                        cutoff: Cutoff::Fraction(Stepped::Fixed(0.95)),
                    };
                    let grid = Grid::new(44_100, RateRatio::integer(ratio));
                    let g = recipe(&k, 44_100, &grid, None).unwrap();
                    let db = match kernel(&k, &g, &grid) {
                        Ok(h) => {
                            // Passband only: the error up to 0.85 of the Nyquist.
                            let m = crate::design::claim::measure(
                                &mut realfft::RealFftPlanner::new(),
                                &h.taps,
                                &Claim {
                                    pass: 0.85,
                                    stop: 1.0e9,
                                    db: 400.0,
                                },
                                &grid,
                            );
                            format!("{:.1}", m.db)
                        }
                        Err(e) => format!("err({e})"),
                    };
                    row.push(db);
                }
                println!(
                    "{name} T {t} x{ratio}: lin/cep-min/cep-ip/reflect {}",
                    row.join(" ")
                );
            }
        }
    }
}

#[test]
#[ignore]
fn probe_reflect() {
    for (name, w) in windows() {
        if name.ends_with("_auto_beta") || name.ends_with("_auto_nw") {
            continue;
        }
        let w = symmetric(w);
        for (t, l) in [(127usize, 8u32), (255, 8), (1023, 4), (4095, 2), (8191, 8)] {
            for phase in [
                Phase::Minimum(PhaseMethod::Reflect),
                Phase::Intermediate {
                    mix: 0.5,
                    method: PhaseMethod::Reflect,
                },
            ] {
                let k = sinc(
                    w,
                    Length::PerInput(Stepped::Fixed(t)),
                    Cutoff::Fraction(Stepped::Fixed(0.9)),
                    phase,
                );
                let r = prepare(&FilterDesign::Kernel(k), &cx(44_100, RateRatio::integer(l)));
                println!(
                    "{name} {phase:?} {} taps: {}",
                    t * l as usize,
                    r.map(|_| "ok".to_string())
                        .unwrap_or_else(|e| e.to_string())
                );
            }
        }
    }
}

/// Calibration of the automatic search's start: the shortest measured
/// length over the window model's estimate.
#[test]
#[ignore]
fn calibrate_search_start() {
    for (name, w) in windows() {
        let mut lo = f64::INFINITY;
        let mut hi = 0.0f64;
        let max = window_model::nominal_db(&match w {
            Window::Kaiser {
                beta: None,
                sampling,
            } => Window::Kaiser {
                beta: Some(20.0),
                sampling,
            },
            Window::Dpss { nw: None, sampling } => Window::Dpss {
                nw: Some(6.37),
                sampling,
            },
            w => w,
        })
        .unwrap();
        for a in [60.0, 90.0, 120.0, 150.0, 180.0] {
            if !name.contains("_auto_") && a > max {
                continue;
            }
            for (pe, se) in [(0.9, 1.0), (0.8, 1.0), (0.95, 1.05)] {
                for ratio in [
                    RateRatio::integer(2),
                    RateRatio::integer(8),
                    RateRatio::integer(16),
                ] {
                    let k = sinc(
                        w,
                        Length::Auto(FilterErrorBudget::Db(a)),
                        band(pe, se, a),
                        Phase::Linear,
                    );
                    let grid = Grid::new(44_100, ratio);
                    let est = recipe(&k, 44_100, &grid, None).unwrap().taps();
                    match search::search(&k, &cx(44_100, ratio)) {
                        Ok(t) => {
                            let f = t as f64 / est as f64;
                            lo = lo.min(f);
                            hi = hi.max(f);
                        }
                        Err(e) => println!("{name} {a} {pe}-{se} {ratio}: {e}"),
                    }
                }
            }
        }
        println!("{name}: found / estimate {lo:.3} ..= {hi:.3}");
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

/// A Kaiser window without a beta takes the beta of its target, whose
/// model depth comes back a few ulps under the target: ordinary targets
/// build (x2 over 0.9-1.0 at 57, 77 and 89 dB) and keep the
/// target itself.
#[test]
fn a_derived_kaiser_beta_reaches_its_target() {
    for a in [57.0, 77.0, 89.0] {
        for sampling in [Sampling::Symmetric, Sampling::Periodic] {
            let w = Window::Kaiser {
                beta: None,
                sampling,
            };
            for length in [Length::Band, Length::Auto(FilterErrorBudget::Db(a))] {
                let k = sinc(w, length, band(0.9, 1.0, a), Phase::Linear);
                let p = ok(&k, 44_100, RateRatio::integer(2));
                let claim = Claim {
                    pass: 0.9,
                    stop: 1.0,
                    db: a,
                };
                let got = measured(&p, claim);
                assert!(got >= a, "{k:?}: {got} dB");
            }
        }
    }
}

/// The length rule alone sets the grid taps, whatever ratio
/// the sinc is built at (96 to 48 kHz, `per_input` 255 with
/// `keep`, built 510 taps with `design_ratio`; `fixed` 4095 at x8 built
/// 4096).
#[test]
fn the_length_rule_sets_the_grid_taps() {
    let fc = Cutoff::Fraction(Stepped::Fixed(0.9));
    let w = Window::Kaiser {
        beta: Some(12.0),
        sampling: Sampling::Symmetric,
    };
    let taps = |k: &KernelDesign, src: u32, ratio: RateRatio| {
        let p = ok(k, src, ratio);
        let n = p.kernel.as_ref().unwrap().taps.len();
        assert_eq!(p.plan.grid_taps, n);
        n
    };
    for gc in [GridCutoff::Scaled, GridCutoff::DesignRatio] {
        let mut k = sinc(
            w,
            Length::PerInput(Stepped::Fixed(255)),
            fc.clone(),
            Phase::Linear,
        );
        k.decimation = Decimation::Keep;
        k.numerics.grid_cutoff = gc;
        assert_eq!(taps(&k, 96_000, RateRatio::new(1, 2)), 255, "{gc:?}");
        assert_eq!(taps(&k, 44_100, RateRatio::integer(4)), 255 * 4, "{gc:?}");
        for path in [ArithmeticPath::Fir, ArithmeticPath::ExactPiInverseAfter] {
            let mut f = sinc(w, Length::Fixed(4095), fc.clone(), Phase::Linear);
            f.decimation = Decimation::Keep;
            f.numerics = Numerics {
                path,
                grid_cutoff: gc,
            };
            assert_eq!(
                taps(&f, 44_100, RateRatio::integer(8)),
                4095,
                "{path:?} {gc:?}"
            );
            assert_eq!(
                taps(&f, 96_000, RateRatio::new(1, 2)),
                8190,
                "{path:?} {gc:?}"
            );
            assert_eq!(
                taps(&f, 44_100, RateRatio::new(160, 147)),
                4095 * 147,
                "{path:?} {gc:?}"
            );
        }
    }
    // The double-double path's Gaussian at a length its ratio does not
    // divide.
    let mut d = sinc(
        Window::Gaussian {
            alpha: 0.2,
            sampling: Sampling::IntegerCentre,
        },
        Length::Fixed(4095),
        fc,
        Phase::Linear,
    );
    d.numerics.path = ArithmeticPath::DoubleDouble;
    assert_eq!(taps(&d, 44_100, RateRatio::integer(8)), 4095);
}

/// An optimal design of `method` on the fir path.
fn optimal(method: crate::design::OptimalMethod, length: Length, phase: Phase) -> KernelDesign {
    KernelDesign {
        shape: Shape::Optimal {
            method,
            length,
            passband_end: 0.9,
            stopband_start: 1.0,
        },
        ..sinc(
            Window::Hann {
                sampling: Sampling::Symmetric,
            },
            Length::PerInput(Stepped::Fixed(1)),
            Cutoff::Fraction(Stepped::Fixed(1.0)),
            phase,
        )
    }
}

/// The optimal designs build at integer, rational and decimating ratios
/// with every phase, their automatic lengths keep the budget, and a
/// length or depth past their limits is refused.
#[test]
fn optimal_designs_build_and_keep_their_budget() {
    use crate::design::OptimalMethod::*;
    let phases = [
        Phase::Linear,
        Phase::Minimum(PhaseMethod::Cepstral),
        Phase::Minimum(PhaseMethod::Reflect),
        Phase::Intermediate {
            mix: 0.5,
            method: PhaseMethod::Cepstral,
        },
        Phase::Intermediate {
            mix: 0.5,
            method: PhaseMethod::Reflect,
        },
    ];
    for method in [Equiripple, LeastSquares] {
        for phase in phases {
            let k = optimal(method, Length::PerInput(Stepped::Fixed(96)), phase);
            // 44.1 to 48 kHz puts 160 samples per input on the grid: an
            // equiripple design that fits its tap limit there is too
            // shallow for the safety gate (a known limit). 32 to 48 kHz
            // is 3/2.
            for (src, ratio) in [
                (44_100, RateRatio::integer(8)),
                (32_000, RateRatio::from_rates(32_000, 48_000)),
                (96_000, RateRatio::from_rates(96_000, 48_000)),
            ] {
                let p = match prepare(&FilterDesign::Kernel(k.clone()), &cx(src, ratio)) {
                    Ok(p) => p,
                    // A short reflected blend can need more than four times
                    // its length (the reflection's own refusal).
                    Err(DesignError::Phase(e))
                        if matches!(phase, Phase::Intermediate { .. })
                            && e.to_string().contains("more than 4 times") =>
                    {
                        continue;
                    }
                    Err(e) => panic!("{method:?} {phase:?} at {ratio}: {e}"),
                };
                let h = p.kernel.unwrap().taps;
                assert!(
                    h.len() >= 95,
                    "{method:?} {phase:?} at {ratio}: {}",
                    h.len()
                );
                // The linear kernel is odd and symmetric.
                if phase == Phase::Linear {
                    assert_eq!(h.len() % 2, 1);
                    assert_eq!(h[0], h[h.len() - 1]);
                }
            }
        }
        // Automatic: the budget kept, linear and minimum phase.
        let a = match method {
            Equiripple => 140.0,
            LeastSquares => 120.0,
        };
        for phase in [Phase::Linear, Phase::Minimum(PhaseMethod::Reflect)] {
            let k = optimal(method, Length::Auto(FilterErrorBudget::Db(a)), phase);
            for (src, ratio) in [
                (44_100, RateRatio::integer(8)),
                (32_000, RateRatio::from_rates(32_000, 48_000)),
            ] {
                let p = prepare(&FilterDesign::Kernel(k.clone()), &cx(src, ratio))
                    .unwrap_or_else(|e| panic!("{method:?} {phase:?} at {ratio}: {e}"));
                let m = measured(
                    &p,
                    Claim {
                        pass: 0.9,
                        stop: 1.0,
                        db: a,
                    },
                );
                assert!(m >= a, "{method:?} {phase:?} at {ratio}: {m} dB");
            }
        }
    }
    // Past the limits: refused, before anything is designed.
    let long = optimal(
        Equiripple,
        Length::PerInput(Stepped::Fixed(4096)),
        Phase::Linear,
    );
    let e = crate::design::feasibility(
        &FilterDesign::Kernel(long),
        &cx(44_100, RateRatio::integer(8)),
    )
    .unwrap_err();
    assert!(
        e.to_string()
            .contains("is built (its cost grows with the square of the length) up to 4095"),
        "{e}"
    );
    // A fixed length past what f64 levels, or (equiripple) short of the
    // depth the branches' DC needs: refused cheaply, as the build would.
    let refused = |k: KernelDesign, ratio: u32, want: &str| {
        let e = crate::design::feasibility(
            &FilterDesign::Kernel(k),
            &cx(44_100, RateRatio::integer(ratio)),
        )
        .unwrap_err();
        assert!(e.to_string().contains(want), "{e}");
    };
    refused(
        optimal(Equiripple, Length::Fixed(1279), Phase::Linear),
        2,
        "past the 160 dB f64 levels its ripple to",
    );
    refused(
        optimal(
            Equiripple,
            Length::PerInput(Stepped::Fixed(64)),
            Phase::Linear,
        ),
        8,
        "adds up in each of the 8 polyphase branches",
    );
    refused(
        optimal(
            LeastSquares,
            Length::PerInput(Stepped::Fixed(512)),
            Phase::Linear,
        ),
        2,
        "far past its 137 to 153 dB floor",
    );
    refused(
        optimal(
            LeastSquares,
            Length::PerInput(Stepped::Fixed(32)),
            Phase::Linear,
        ),
        16,
        "too shallow for the 16 polyphase branches",
    );
    // A shallow automatic equiripple design grows until its branches pass
    // the gate.
    for ratio in [2, 8, 16] {
        let k = optimal(
            Equiripple,
            Length::Auto(FilterErrorBudget::Db(55.0)),
            Phase::Linear,
        );
        prepare(
            &FilterDesign::Kernel(k),
            &cx(44_100, RateRatio::integer(ratio)),
        )
        .unwrap_or_else(|e| panic!("55 dB at x{ratio}: {e}"));
    }
    let deep = optimal(
        LeastSquares,
        Length::Auto(FilterErrorBudget::Db(150.0)),
        Phase::Linear,
    );
    let e = crate::design::feasibility(
        &FilterDesign::Kernel(deep),
        &cx(44_100, RateRatio::integer(8)),
    )
    .unwrap_err();
    assert!(e.to_string().contains("length.budget"), "{e}");
    // Not on the standard path, nor with preserved samples.
    let mut std_path = optimal(
        Equiripple,
        Length::PerInput(Stepped::Fixed(64)),
        Phase::Linear,
    );
    std_path.numerics.path = ArithmeticPath::Standard;
    std_path.numerics.grid_cutoff = GridCutoff::DesignRatio;
    std_path.decimation = Decimation::Keep;
    let e = crate::design::feasibility(
        &FilterDesign::Kernel(std_path),
        &cx(44_100, RateRatio::integer(8)),
    )
    .unwrap_err();
    assert!(e.to_string().contains("numerics.path"), "{e}");
}

/// The new designs against Kaiser: the automatic length each needs for a
/// budget over 0.9 ..= 1.0 of the Nyquist, and at one length the error over
/// that band (Kaiser with its best beta, cutoff midway).
#[test]
#[ignore]
fn compare_with_kaiser() {
    use crate::design::OptimalMethod::*;
    let sym = Sampling::Symmetric;
    let claim = |a| Claim {
        pass: 0.9,
        stop: 1.0,
        db: a,
    };
    let auto_len = |k: KernelDesign, ratio| {
        let t = std::time::Instant::now();
        let r = prepare(&FilterDesign::Kernel(k), &cx(44_100, ratio));
        let s = t.elapsed().as_secs_f64();
        match r {
            Ok(p) => format!("{} taps ({s:.2} s)", p.kernel.unwrap().taps.len()),
            Err(e) => format!("refused: {e}"),
        }
    };
    for ratio in [RateRatio::integer(8), RateRatio::integer(64)] {
        for a in [100.0, 120.0, 140.0, 160.0] {
            let budget = Length::Auto(FilterErrorBudget::Db(a));
            let band = band(0.9, 1.0, a);
            let kaiser = sinc(
                Window::Kaiser {
                    beta: None,
                    sampling: sym,
                },
                budget.clone(),
                band.clone(),
                Phase::Linear,
            );
            let dpss = sinc(
                Window::Dpss {
                    nw: None,
                    sampling: sym,
                },
                budget.clone(),
                band.clone(),
                Phase::Linear,
            );
            println!(
                "{ratio} {a} dB: kaiser {} | dpss {} | equiripple {} | least squares {}",
                auto_len(kaiser, ratio),
                auto_len(dpss, ratio),
                auto_len(optimal(Equiripple, budget.clone(), Phase::Linear), ratio),
                if a <= check::optimal_max_db(LeastSquares) {
                    auto_len(optimal(LeastSquares, budget.clone(), Phase::Linear), ratio)
                } else {
                    "over its floor".into()
                },
            );
        }
    }
    // One length: the error over the band.
    let ratio = RateRatio::integer(8);
    for t in [64usize, 96, 128, 160, 192] {
        let per = Length::PerInput(Stepped::Fixed(t));
        let err = |k: KernelDesign| match prepare(&FilterDesign::Kernel(k), &cx(44_100, ratio)) {
            Ok(p) => format!("{:.1} dB", measured(&p, claim(0.0))),
            Err(e) => format!("refused ({e})"),
        };
        let fc = Cutoff::Fraction(Stepped::Fixed(0.95));
        let best = |w: &dyn Fn(f64) -> Window, lo: f64, hi: f64| {
            let mut best = (f64::NEG_INFINITY, 0.0);
            let mut x = lo;
            while x <= hi {
                let k = sinc(w(x), per.clone(), fc.clone(), Phase::Linear);
                if let Ok(p) = prepare(&FilterDesign::Kernel(k), &cx(44_100, ratio)) {
                    let m = measured(&p, claim(0.0));
                    if m > best.0 {
                        best = (m, x);
                    }
                }
                x += (hi - lo) / 80.0;
            }
            best
        };
        let (kd, kb) = best(
            &|b| Window::Kaiser {
                beta: Some(b),
                sampling: sym,
            },
            2.0,
            30.0,
        );
        let (dd, dn) = best(
            &|nw| Window::Dpss {
                nw: Some(nw),
                sampling: sym,
            },
            1.0,
            9.5,
        );
        println!(
            "x8 {t} per input ({} taps): kaiser {kd:.1} dB (beta {kb:.2}) | dpss {dd:.1} dB (nw \
             {dn:.2}) | equiripple {} | least squares {}",
            t * 8 - 1,
            err(optimal(Equiripple, per.clone(), Phase::Linear)),
            err(optimal(LeastSquares, per.clone(), Phase::Linear)),
        );
    }
}

/// A band with an absolute passband end (`passband_end_hz`) keeps that
/// frequency at every source rate: the same fraction at 44.1 kHz (the
/// fraction field, so the kernel's bits do not move), a shorter kernel at a
/// higher rate, and the fraction where the frequency reaches the stopband.
#[test]
fn a_pinned_passband_does_not_scale_with_the_source_rate() {
    let spec = |hz: Option<f64>| BandSpec {
        passband_end: 20_000.0 / 22_050.0,
        passband_end_hz: hz,
        stopband_start: 1.0,
        attenuation_db: 150.0,
    };
    let design = |hz| {
        sinc(
            Window::Kaiser {
                beta: None,
                sampling: Sampling::Symmetric,
            },
            Length::Band,
            Cutoff::Band(spec(hz)),
            Phase::Minimum(PhaseMethod::Reflect),
        )
    };
    let taps = |rate: u32| {
        let c = cx(rate, RateRatio::integer(2));
        let p = prepare(&FilterDesign::Kernel(design(Some(20_000.0))), &c).unwrap();
        p.kernel.unwrap().taps.len()
    };
    assert_eq!(taps(44_100), 477);
    assert!(taps(96_000) < 477);
    // 30 kHz is past the 22.05 kHz Nyquist at 44.1 kHz: not inside the
    // band, so the fraction stands there, bit for bit.
    let at = |hz| {
        prepare(
            &FilterDesign::Kernel(design(hz)),
            &cx(44_100, RateRatio::integer(2)),
        )
        .unwrap()
        .kernel
        .unwrap()
        .taps
    };
    assert_eq!(at(Some(30_000.0)), at(None));
    // Without the Hz field the fraction stands, as every custom design's.
    let e = prepare(
        &FilterDesign::Kernel(design(None)),
        &cx(44_100, RateRatio::integer(2)),
    );
    assert!(e.is_ok());
}
