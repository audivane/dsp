//! The stopband model of each window on the preset paths: how deep a
//! windowed sinc with a given window can go (its nominal attenuation), and
//! how far past
//! the cutoff its response stays within an error of `-a` dB (its half
//! transition). The band and stop-at-Nyquist rules take their lengths and
//! cutoffs from it, the automatic search its start, the reflected phase
//! its contract.
//!
//! Distances are in window bins (`1 / N` cycles per grid sample for an
//! N-tap kernel): a cosine-sum window's spectrum has the same shape in bins
//! at every length, so one table holds at every N, ratio and cutoff. Every
//! constant is measured (the `calibration` tests below, `--ignored`): the
//! largest relative error past a distance from the cutoff, the worst over
//! lengths, ratios, cutoffs and the window's samplings.
//!
//! | Window | Nominal attenuation | Half transition |
//! |---|---|---|
//! | Kaiser beta (2 ..= 30) | `beta / 0.1102 + 8.7 - 14` dB | the main lobe, `sqrt(beta^2 + pi^2) / pi` bins |
//! | Gaussian alpha | `10 / (alpha^2 ln 10) + 15` dB (the truncation edge's depth + 15), at most 270 | `1.1 sqrt(2) / (pi alpha) erfcinv(2 delta)` bins |
//! | Chebyshev r, `symmetric` | `-20 log10 r + 2` dB | the main lobe, exactly: `N acos(1 / x0) / pi`, `x0 = cosh(acosh(1/r) / (N - 1))` |
//! | Chebyshev r, `periodic` | as symmetric, less `0.17 (S - 150)` above `S = 150` dB sidelobes | as symmetric |
//! | Hann, Nuttall, flat-top | 90, 120, 210 / 230 / 250 dB | measured table ([`TABLES`]) |
//!
//! Tukey has no model (no calibrated stopband), nor has the `standard`
//! path's Gaussian (its own formulas, [`crate::resample`]).

use super::{FlattopSet, Sampling, Window};
use std::f64::consts::PI;

/// How far short of Kaiser's formula `beta / 0.1102 + 8.7` a Kaiser-windowed
/// sinc's error is at the edge of the window's main lobe (measured: up to
/// 13.9 dB, beta 30, symmetric; 12.2 at beta 20).
pub const KAISER_SHORTFALL_DB: f64 = 14.0;

/// The Kaiser beta range the model is measured over.
pub const KAISER_BETA_RANGE: (f64, f64) = (2.0, 30.0);

/// The Gaussian model's factor over the analytic transition (measured at
/// most 1.067, alpha 0.05, up to 15 dB past the truncation edge).
const GAUSSIAN_FACTOR: f64 = 1.1;

/// How far past its truncation edge a Gaussian-windowed sinc's model holds
/// (measured: the floor is 46 to 47 dB past it, and the model's factor
/// holds to 1.04 up to 15 dB past it).
const GAUSSIAN_EDGE_DB: f64 = 15.0;

/// Deepest attenuation of any model: an f64 kernel's floor (measured 283
/// to 286 dB for the narrowest Gaussians).
const F64_FLOOR_DB: f64 = 270.0;

/// The measured half transitions of the cosine-sum windows: `(a, x)`, every
/// error under `-a` dB from `x` bins past the cutoff (the worst over both
/// samplings), every 10 dB up to the nominal attenuation (the last row).
pub const TABLES: [(&str, &[(f64, f64)]); 5] = [
    (
        "hann",
        &[
            (20.0, 0.98),
            (30.0, 1.32),
            (40.0, 1.53),
            (50.0, 2.39),
            (60.0, 4.17),
            (70.0, 7.31),
            (80.0, 8.12),
            (90.0, 12.08),
        ],
    ),
    (
        "nuttall",
        &[
            (20.0, 1.42),
            (30.0, 2.02),
            (40.0, 2.47),
            (50.0, 2.83),
            (60.0, 3.12),
            (70.0, 3.35),
            (80.0, 3.54),
            (90.0, 3.69),
            (100.0, 3.81),
            (110.0, 7.59),
            (120.0, 13.09),
        ],
    ),
    (
        "hft196d",
        &[
            (20.0, 2.84),
            (30.0, 3.84),
            (40.0, 4.60),
            (50.0, 5.21),
            (60.0, 5.73),
            (70.0, 6.17),
            (80.0, 6.56),
            (90.0, 6.90),
            (100.0, 7.21),
            (110.0, 7.48),
            (120.0, 7.72),
            (130.0, 7.94),
            (140.0, 8.13),
            (150.0, 8.30),
            (160.0, 8.45),
            (170.0, 8.57),
            (180.0, 8.69),
            (190.0, 8.78),
            (200.0, 8.85),
            (210.0, 9.47),
        ],
    ),
    (
        "hft223d",
        &[
            (20.0, 2.97),
            (30.0, 4.04),
            (40.0, 4.83),
            (50.0, 5.48),
            (60.0, 6.03),
            (70.0, 6.51),
            (80.0, 6.93),
            (90.0, 7.31),
            (100.0, 7.64),
            (110.0, 7.94),
            (120.0, 8.21),
            (130.0, 8.46),
            (140.0, 8.68),
            (150.0, 8.88),
            (160.0, 9.06),
            (170.0, 9.22),
            (180.0, 9.36),
            (190.0, 9.49),
            (200.0, 9.60),
            (210.0, 9.69),
            (220.0, 9.76),
            (230.0, 9.82),
        ],
    ),
    (
        "hft248d",
        &[
            (20.0, 3.15),
            (30.0, 4.28),
            (40.0, 5.13),
            (50.0, 5.82),
            (60.0, 6.41),
            (70.0, 6.92),
            (80.0, 7.38),
            (90.0, 7.79),
            (100.0, 8.16),
            (110.0, 8.49),
            (120.0, 8.79),
            (130.0, 9.07),
            (140.0, 9.32),
            (150.0, 9.55),
            (160.0, 9.76),
            (170.0, 9.95),
            (180.0, 10.12),
            (190.0, 10.28),
            (200.0, 10.41),
            (210.0, 10.54),
            (220.0, 10.64),
            (230.0, 10.74),
            (240.0, 10.82),
            (250.0, 10.88),
        ],
    ),
];

/// Margin on a table's half transition (the tables hold the worst measured
/// value; another sampling or length moves it by up to 1 %).
const TABLE_MARGIN: f64 = 1.02;

fn table(w: &Window) -> Option<&'static [(f64, f64)]> {
    let name = match w {
        Window::Hann { .. } => "hann",
        Window::Nuttall { .. } => "nuttall",
        Window::Flattop { set, .. } => match set {
            FlattopSet::Hft196d => "hft196d",
            FlattopSet::Hft223d => "hft223d",
            FlattopSet::Hft248d => "hft248d",
        },
        _ => return None,
    };
    TABLES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// The sidelobe level of a Chebyshev ratio, dB (positive).
pub fn chebyshev_sidelobe_db(r: f64) -> f64 {
    -20.0 * r.log10()
}

/// The depth of a Gaussian window's truncation edge, dB (positive):
/// `exp(-1 / (2 alpha^2))`.
fn gaussian_edge_db(alpha: f64) -> f64 {
    1.0 / (2.0 * alpha * alpha) * 20.0 / std::f64::consts::LN_10
}

/// The window's nominal attenuation, dB: the deepest error a windowed sinc
/// with it keeps past its half transition. `None` for a window without a
/// model (Tukey, the `standard` path's Gaussian, a Kaiser window without a
/// beta or with one outside [`KAISER_BETA_RANGE`]).
pub fn nominal_db(w: &Window) -> Option<f64> {
    Some(match *w {
        Window::Kaiser {
            beta: Some(beta), ..
        } if (KAISER_BETA_RANGE.0..=KAISER_BETA_RANGE.1).contains(&beta) => {
            crate::resample::kaiser_attenuation_db(beta) - KAISER_SHORTFALL_DB
        }
        Window::Gaussian { alpha, .. } if alpha > 0.0 => {
            (gaussian_edge_db(alpha) + GAUSSIAN_EDGE_DB).min(F64_FLOOR_DB)
        }
        Window::Chebyshev {
            sidelobe_ratio,
            sampling,
        } if sidelobe_ratio > 0.0 && sidelobe_ratio < 1.0 => {
            let s = chebyshev_sidelobe_db(sidelobe_ratio);
            let periodic = if sampling == Sampling::Periodic {
                0.17 * (s - 150.0).max(0.0)
            } else {
                0.0
            };
            (s + 2.0 - periodic).min(F64_FLOOR_DB)
        }
        _ => table(w)?.last()?.0,
    })
}

/// Roundoff allowed where a target meets a model's depth: a Kaiser beta
/// derived from a target (Kaiser's formula inverted, then the model's
/// shortfall) gives the target back only to a few ulps, a hair under it.
/// The finished kernel is still measured at the target itself.
pub const MODEL_ROUNDOFF_DB: f64 = 1e-9;

/// Whether `w` can promise `a` dB: its nominal attenuation reaches it
/// (within [`MODEL_ROUNDOFF_DB`]).
pub fn reaches(w: &Window, a: f64) -> bool {
    nominal_db(w).is_some_and(|n| a <= n + MODEL_ROUNDOFF_DB)
}

/// Bins past the cutoff from which the error of an `n`-tap windowed sinc
/// with `w` stays under `-a` dB; `None` when `a` is deeper than the
/// window's nominal attenuation or the window has no model.
pub fn half_width_bins(w: &Window, a: f64, n: usize) -> Option<f64> {
    let nominal = nominal_db(w)?;
    if !a.is_finite() || a > nominal + MODEL_ROUNDOFF_DB {
        return None;
    }
    let a = a.min(nominal);
    match *w {
        Window::Kaiser { beta: Some(b), .. } => Some((b * b + PI * PI).sqrt() / PI),
        Window::Gaussian { alpha, .. } => {
            let delta = 10f64.powf(-a.max(1.0) / 20.0);
            Some(
                GAUSSIAN_FACTOR * std::f64::consts::SQRT_2 / (PI * alpha)
                    * crate::window::erfc_inv(2.0 * delta),
            )
        }
        Window::Chebyshev { sidelobe_ratio, .. } => {
            if n < 2 {
                return None;
            }
            let x0 = ((1.0 / sidelobe_ratio).acosh() / (n - 1) as f64).cosh();
            Some((1.0 / x0).acos() / PI * n as f64)
        }
        _ => {
            // The first tabled level at or past `a` (the error only falls
            // with the distance).
            let t = table(w)?;
            t.iter()
                .find(|&&(level, _)| level >= a)
                .map(|&(_, x)| x * TABLE_MARGIN)
        }
    }
}

/// The half transition of an `n`-tap kernel with `w` on a grid of `r`
/// samples per lower-rate sample, as a fraction of the lower Nyquist.
pub fn half_width(w: &Window, a: f64, n: usize, r: usize) -> Option<f64> {
    half_width_bins(w, a, n).map(|x| x * 2.0 * r.max(1) as f64 / n.max(1) as f64)
}

/// The fewest taps (at least 2, at most `max`) on a grid of `r` samples per
/// lower-rate sample whose half transition at `-a` dB fits in `w`
/// (fraction of the lower Nyquist). `None` when no length does.
pub fn taps_for(win: &Window, a: f64, r: usize, w: f64, max: usize) -> Option<usize> {
    if w.is_nan() || w <= 0.0 {
        return None;
    }
    let fits = |n: usize| half_width(win, a, n, r).is_some_and(|h| h <= w);
    if max < 2 {
        return None;
    }
    if fits(2) {
        return Some(2);
    }
    // The half width falls with the length: doubling from a failing
    // length, then bisection.
    let mut lo = 2usize;
    let mut hi = 4usize;
    loop {
        if hi >= max {
            if !fits(max) {
                return None;
            }
            hi = max;
            break;
        }
        if fits(hi) {
            break;
        }
        lo = hi;
        hi *= 2;
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

/// The Kaiser beta of a reflected phase's contract
/// ([`crate::minphase::Contract`]) for a nominal attenuation of `a` dB:
/// Kaiser's formula inverted, so the contract's attenuation is `a`.
pub fn contract_beta(a: f64) -> f64 {
    (a - 8.7) * 0.1102
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models() {
        let hann = Window::Hann {
            sampling: Sampling::Periodic,
        };
        assert_eq!(nominal_db(&hann), Some(90.0));
        assert_eq!(half_width_bins(&hann, 55.0, 100), Some(4.17 * TABLE_MARGIN));
        assert_eq!(half_width_bins(&hann, 95.0, 100), None);
        let cheb = Window::Chebyshev {
            sidelobe_ratio: 2f64.powi(-32),
            sampling: Sampling::Symmetric,
        };
        let a = nominal_db(&cheb).unwrap();
        assert!((a - 194.66).abs() < 0.01, "{a}");
        // The main lobe tends to acosh(1/r) / pi bins.
        let x = half_width_bins(&cheb, 150.0, 1 << 20).unwrap();
        assert!((x - (2f64.powi(32)).acosh() / PI).abs() < 1e-3, "{x}");
        let tukey = Window::Tukey {
            taper: 0.5,
            sampling: Sampling::Symmetric,
        };
        assert_eq!(nominal_db(&tukey), None);
        // Shorter kernels need more: taps_for is the smallest that fits.
        let n = taps_for(&cheb, 150.0, 8, 0.05, 1 << 24).unwrap();
        assert!(half_width(&cheb, 150.0, n, 8).unwrap() <= 0.05);
        assert!(half_width(&cheb, 150.0, n - 1, 8).unwrap() > 0.05);
        assert_eq!(taps_for(&cheb, 150.0, 8, 0.05, 100), None);
    }
}

#[cfg(test)]
mod calibration {
    use crate::design::paths::fir::{self, Recipe, Window, windows};
    use realfft::RealFftPlanner;

    /// (distance from the cutoff in window bins, relative error) at every
    /// point of a 32 N-point FFT of `h` on a grid of `r` samples per
    /// lower-rate sample, cutoff `fc`.
    fn profile(h: &[f64], r: usize, fc: f64) -> Vec<(f64, f64)> {
        let n = h.len();
        let nfft = (32 * n).next_power_of_two();
        let mut p = RealFftPlanner::<f64>::new();
        let f = p.plan_fft_forward(nfft);
        let mut x = f.make_input_vec();
        x[..n].copy_from_slice(h);
        let mut s = f.make_output_vec();
        f.process(&mut x, &mut s).unwrap();
        let g: f64 = h.iter().sum();
        s.iter()
            .enumerate()
            .map(|(k, c)| {
                let nu = k as f64 / nfft as f64 * 2.0 * r as f64;
                let a = c.norm() / g;
                let err = if nu < fc { (a - 1.0).abs() } else { a };
                ((nu - fc).abs() * n as f64 / (2.0 * r as f64), err)
            })
            .collect()
    }

    /// Bins from the cutoff past which every error is under `-a` dB.
    fn x_for(p: &[(f64, f64)], a: f64) -> f64 {
        let d = 10f64.powf(-a / 20.0);
        p.iter()
            .filter(|(_, e)| *e > d)
            .map(|(x, _)| *x)
            .fold(0.0, f64::max)
    }

    fn proto(w: Window, t: usize, r: usize, fc: f64) -> Vec<f64> {
        fir::prototype(
            &Recipe {
                taps: t,
                cutoff: fc,
                window: w,
            },
            r,
        )
    }

    const CASES: &[(usize, usize)] = &[(16, 8), (64, 2), (64, 8), (255, 8), (255, 16), (1023, 4)];

    /// Worst attenuation at `x` bins and beyond.
    fn att_at(p: &[(f64, f64)], x: f64) -> f64 {
        let e = p
            .iter()
            .filter(|(d, _)| *d >= x)
            .map(|(_, e)| *e)
            .fold(0.0, f64::max);
        -20.0 * e.log10()
    }

    #[test]
    #[ignore]
    fn calibrate_chebyshev_gaussian() {
        for db in [
            40.0, 60.0, 96.3, 120.0, 144.5, 170.0, 192.7, 200.0, 210.0, 220.0,
        ] {
            let r = 10f64.powf(-db / 20.0);
            let mut worst = [f64::INFINITY; 2];
            for &(t, l) in CASES {
                for &fc in &[0.9, 0.95, 1.0] {
                    let n = t * l;
                    let x0 = ((1.0 / r).acosh() / (n - 1) as f64).cosh();
                    let b = (1.0 / x0).acos() / std::f64::consts::PI * n as f64;
                    for (j, w) in [
                        Window::ChebyshevSymmetric { r },
                        Window::Other(crate::design::Window::Chebyshev {
                            sidelobe_ratio: r,
                            sampling: crate::design::Sampling::Periodic,
                        }),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let p = profile(&proto(w, t, l, fc), l, fc);
                        worst[j] = worst[j].min(att_at(&p, b) - db);
                    }
                }
            }
            println!(
                "cheb {db}: attenuation at the main-lobe edge - sidelobe dB: sym {:.2} per {:.2}",
                worst[0], worst[1]
            );
        }
        for alpha in [
            0.05, 0.08, 0.1, 0.125, 0.15, 0.175, 0.2, 0.25, 0.3, 0.4, 0.5,
        ] {
            let mut floor = f64::INFINITY;
            let mut k_worst = [0.0f64; 3];
            let edge = 1.0 / (2.0 * alpha * alpha) * 20.0 / std::f64::consts::LN_10;
            for &(t, l) in CASES {
                for &fc in &[0.9, 0.95, 1.0] {
                    for w in [
                        Window::Gaussian { alpha },
                        Window::Other(crate::design::Window::Gaussian {
                            alpha,
                            sampling: crate::design::Sampling::Symmetric,
                        }),
                    ] {
                        let p = profile(&proto(w, t, l, fc), l, fc);
                        floor = floor.min(att_at(&p, 40.0));
                        // Model x(a) = sqrt2 / (pi alpha) erfcinv(2 delta):
                        // the factor it needs, up to 10 dB under the floor.
                        for (j, m) in [25.0, 30.0, 35.0].into_iter().enumerate() {
                            for a in (4..28).map(|k| 10.0 * k as f64) {
                                if a > (edge + m).min(270.0) {
                                    break;
                                }
                                let d = 10f64.powf(-a / 20.0);
                                let model = std::f64::consts::SQRT_2
                                    / (std::f64::consts::PI * alpha)
                                    * crate::window::erfc_inv(2.0 * d);
                                k_worst[j] = k_worst[j].max(x_for(&p, a) / model);
                            }
                        }
                    }
                }
            }
            println!(
                "gauss {alpha}: edge {edge:.1} dB, floor {floor:.1} dB, model factor (+25/30/35) {k_worst:.3?}"
            );
        }
        use crate::design::{FlattopSet, Sampling, Window as W};
        for (name, w) in [
            (
                "hann sym",
                W::Hann {
                    sampling: Sampling::Symmetric,
                },
            ),
            (
                "nuttall per",
                W::Nuttall {
                    sampling: Sampling::Periodic,
                },
            ),
            (
                "hft196d sym",
                W::Flattop {
                    set: FlattopSet::Hft196d,
                    sampling: Sampling::Symmetric,
                },
            ),
            (
                "hft248d sym",
                W::Flattop {
                    set: FlattopSet::Hft248d,
                    sampling: Sampling::Symmetric,
                },
            ),
        ] {
            let levels: Vec<f64> = (2..=26).map(|k| 10.0 * k as f64).collect();
            let mut worst = vec![0.0f64; levels.len()];
            for &(t, l) in CASES {
                for &fc in &[0.9, 0.95, 1.0] {
                    let p = profile(&proto(Window::Other(w), t, l, fc), l, fc);
                    for (i, &a) in levels.iter().enumerate() {
                        worst[i] = worst[i].max(x_for(&p, a));
                    }
                }
            }
            let row: Vec<String> = levels
                .iter()
                .zip(&worst)
                .map(|(a, x)| format!("{a:.0}:{x:.2}"))
                .collect();
            println!("{name} | {}", row.join(" "));
        }
    }

    #[test]
    #[ignore]
    fn calibrate_kaiser_gaussian_fine() {
        use crate::design::{Sampling, Window as W};
        for beta in [
            2.0, 3.5, 5.0, 6.5, 8.0, 10.0, 12.0, 14.0, 16.0, 18.0, 20.0, 22.0, 24.0, 27.5, 30.0,
        ] {
            let a = crate::resample::kaiser_attenuation_db(beta);
            let mut worst = [f64::INFINITY; 2];
            let mut at_b = [f64::INFINITY; 2];
            let mut fac = [0.0f64; 2];
            let b = (beta * beta + std::f64::consts::PI.powi(2)).sqrt() / std::f64::consts::PI;
            for &(t, l) in CASES {
                for &fc in &[0.9, 0.95, 1.0] {
                    let n = t * l;
                    let w = crate::resample::half_transition(
                        audivane_protocol::WindowKind::Kaiser,
                        n,
                        l,
                        beta,
                    );
                    let x = w * n as f64 / (2.0 * l as f64);
                    for (j, s) in [Sampling::Periodic, Sampling::Symmetric]
                        .into_iter()
                        .enumerate()
                    {
                        let win = Window::Other(W::Kaiser {
                            beta: Some(beta),
                            sampling: s,
                        });
                        let p = profile(&proto(win, t, l, fc), l, fc);
                        worst[j] = worst[j].min(att_at(&p, x) - a);
                        at_b[j] = at_b[j].min(att_at(&p, b) - a);
                        fac[j] = fac[j].max(x_for(&p, a - 1.0) / b);
                    }
                }
            }
            println!(
                "kaiser {beta}: A {a:.1}, at the formula's edge - A: per {:.2} sym {:.2}; at the main lobe {b:.2} bins - A: {:.2} {:.2}; x(A-1)/B {:.3} {:.3}",
                worst[0], worst[1], at_b[0], at_b[1], fac[0], fac[1]
            );
        }
        for alpha in [0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.35, 0.4, 0.45, 0.5] {
            let edge = 1.0 / (2.0 * alpha * alpha) * 20.0 / std::f64::consts::LN_10;
            let mut k_worst = [0.0f64; 3];
            for &(t, l) in CASES {
                for &fc in &[0.9, 0.95, 1.0] {
                    for s in [
                        Sampling::IntegerCentre,
                        Sampling::Symmetric,
                        Sampling::Periodic,
                    ] {
                        let win = Window::Other(W::Gaussian { alpha, sampling: s });
                        let p = profile(&proto(win, t, l, fc), l, fc);
                        for (j, m) in [15.0, 20.0, 25.0].into_iter().enumerate() {
                            for a in (8..55).map(|k| 5.0 * k as f64) {
                                if a > (edge + m).min(270.0) {
                                    break;
                                }
                                let d = 10f64.powf(-a / 20.0);
                                let model = std::f64::consts::SQRT_2
                                    / (std::f64::consts::PI * alpha)
                                    * crate::window::erfc_inv(2.0 * d);
                                k_worst[j] = k_worst[j].max(x_for(&p, a) / model);
                            }
                        }
                    }
                }
            }
            println!("gauss {alpha}: edge {edge:.1} dB, factor (+15/20/25) {k_worst:.3?}");
        }
    }

    #[test]
    #[ignore]
    fn calibrate_transition_tables() {
        let wins: Vec<(&str, Window)> = vec![
            ("hann", Window::HannPeriodic),
            ("nuttall", Window::NuttallSymmetric),
            ("hft196d", Window::Flattop(windows::HFT196D)),
            ("hft223d", Window::Flattop(windows::HFT223D)),
            ("hft248d", Window::Flattop(windows::HFT248D)),
            ("cheb96", Window::ChebyshevSymmetric { r: 2f64.powi(-16) }),
            ("cheb144", Window::ChebyshevSymmetric { r: 2f64.powi(-24) }),
            ("cheb192", Window::ChebyshevSymmetric { r: 2f64.powi(-32) }),
            ("cheb200", Window::ChebyshevSymmetric { r: 1e-10 }),
            ("gauss0.125", Window::Gaussian { alpha: 0.125 }),
            ("gauss0.175", Window::Gaussian { alpha: 0.175 }),
            ("gauss0.3", Window::Gaussian { alpha: 0.3 }),
            (
                "kaiser8.75pi",
                Window::KaiserPeriodic {
                    beta: 8.75 * std::f64::consts::PI,
                },
            ),
            ("kaiser10", Window::KaiserPeriodic { beta: 10.0 }),
        ];
        let levels: Vec<f64> = (2..=26).map(|k| 10.0 * k as f64).collect();
        for (name, w) in wins {
            let mut worst = vec![0.0f64; levels.len()];
            let mut floor = f64::INFINITY;
            for &(t, r) in CASES {
                for &fc in &[0.9, 0.95, 1.0] {
                    let h = proto(w, t, r, fc);
                    let p = profile(&h, r, fc);
                    for (i, &a) in levels.iter().enumerate() {
                        worst[i] = worst[i].max(x_for(&p, a));
                    }
                    let max_far = p
                        .iter()
                        .filter(|(x, _)| *x > 40.0)
                        .map(|(_, e)| *e)
                        .fold(0.0, f64::max);
                    floor = floor.min(-20.0 * max_far.log10());
                }
            }
            let row: Vec<String> = levels
                .iter()
                .zip(&worst)
                .map(|(a, x)| format!("{a:.0}:{x:.2}"))
                .collect();
            println!("{name} far-floor {floor:.1} | {}", row.join(" "));
        }
    }
}
