//! Windows for the preset recipes, each with its exact sampling convention
//! (n = 0 .. N-1). "Periodic" divides by N, "symmetric" by N - 1. The
//! conventions matter: a preset is defined by these exact samples, so the
//! general-purpose windows in [`crate::window`] (which centre every window
//! on the kernel) are not interchangeable with these.

use std::f64::consts::PI;

/// Gaussian, `exp(-0.5 * ((n - c) / (alpha * c))^2)` with the integer
/// centre `c = (N - 1) / 2` (rounded down), so `alpha` is sigma as a
/// fraction of the half length. For even N the centre is half a sample
/// before the kernel's centre, which makes the kernel very slightly
/// asymmetric (part of these recipes).
pub fn gaussian(n: usize, alpha: f64) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let c = ((n - 1) / 2) as f64;
    if c == 0.0 {
        return vec![1.0; n];
    }
    (0..n)
        .map(|i| {
            let x = (i as f64 - c) / (alpha * c);
            (-0.5 * x * x).exp()
        })
        .collect()
}

/// I0 by its power series, `sum_k ((x/2)^k / k!)^2`, 65 terms.
pub fn bessel_i0_series(x: f64) -> f64 {
    let half = x / 2.0;
    let mut term = 1.0_f64; // (x/2)^k / k!
    let mut sum = 1.0_f64;
    for k in 1..65 {
        term *= half / k as f64;
        sum += term * term;
    }
    sum
}

/// Periodic Kaiser, `I0(beta * sqrt(1 - (2n/N - 1)^2)) / I0(beta)`.
pub fn kaiser_periodic(n: usize, beta: f64) -> Vec<f64> {
    let denom = bessel_i0_series(beta);
    (0..n)
        .map(|i| {
            let t = 2.0 * i as f64 / n as f64 - 1.0;
            let r = (1.0 - t * t).max(0.0).sqrt();
            bessel_i0_series(beta * r) / denom
        })
        .collect()
}

/// Coefficients of the HFT flat-top windows (Heinzel, Rüdiger and Schilling,
/// "Spectrum and spectral density estimation by the DFT", 2002).
pub const HFT196D: &[f64] = &[
    1.0,
    1.979280420,
    1.710288951,
    1.081629853,
    0.448734314,
    0.112376628,
    0.015122992,
    0.000871252,
    0.000011896,
];
pub const HFT223D: &[f64] = &[
    1.0,
    1.98298997309,
    1.75556083063,
    1.19037717712,
    0.56155440797,
    0.17296769663,
    0.03233247087,
    0.00324954578,
    0.00013801040,
    0.00000132725,
];
pub const HFT248D: &[f64] = &[
    1.0,
    1.985844164102,
    1.791176438506,
    1.282075284005,
    0.667777530266,
    0.240160796576,
    0.056656381764,
    0.008134974479,
    0.000624544650,
    0.000019808998,
    0.000000132974,
];

/// Periodic flat-top window `sum_k (-1)^k c_k cos(k * 2 pi n / N)`,
/// normalised to a peak of 1.
pub fn flattop_periodic(n: usize, coeffs: &[f64]) -> Vec<f64> {
    let mut w: Vec<f64> = (0..n)
        .map(|i| {
            let z = 2.0 * PI * i as f64 / n as f64;
            let mut acc = 0.0;
            for (k, &c) in coeffs.iter().enumerate() {
                let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
                acc += sign * c * (k as f64 * z).cos();
            }
            acc
        })
        .collect();
    normalise_peak(&mut w);
    w
}

/// Symmetric 4-term Nuttall window.
pub fn nuttall_symmetric(n: usize) -> Vec<f64> {
    if n == 1 {
        return vec![1.0];
    }
    (0..n)
        .map(|i| {
            let z = 2.0 * PI * i as f64 / (n - 1) as f64;
            0.355768 - 0.487396 * z.cos() + 0.144232 * (2.0 * z).cos() - 0.012604 * (3.0 * z).cos()
        })
        .collect()
}

/// Periodic Hann, `0.5 * (1 - cos(2 pi n / N))`.
pub fn hann_periodic(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f64 / n as f64).cos()))
        .collect()
}

/// Symmetric Dolph-Chebyshev window with sidelobes at `20 log10(r)` dB,
/// normalised to a peak of 1, by the direct cosine sum:
///
/// ```text
/// x0   = cosh(acosh(1/r) / (N-1))
/// w[n] = (1/N) (1 + 2r sum_{k=1..N/2} T(x0 cos(k pi/N)) cos(k (2n+1-N) pi/N))
/// T(x) = cosh((N-1) acosh x) for |x| > 1, else cos((N-1) acos x)
/// ```
///
/// The sum is evaluated directly, in this order, up to
/// [`CHEBYSHEV_DIRECT_MAX`] taps (every integer-ratio recipe), and with an
/// FFT above that (the long kernels on a rational ratio's grid), where the
/// O(N^2) sum would take seconds.
pub fn chebyshev_symmetric(n: usize, r: f64) -> Vec<f64> {
    if n <= 1 {
        return vec![1.0; n];
    }
    let t = chebyshev_terms(n, r);
    if n > CHEBYSHEV_DIRECT_MAX {
        return chebyshev_fft(n, r, &t);
    }
    let pi_n = PI * (1.0 / n as f64);
    let inv_n = 1.0 / n as f64;
    let mut w: Vec<f64> = (0..n)
        .map(|i| {
            let theta = (2.0 * i as f64 + 1.0 - n as f64) * pi_n;
            let mut s = 0.0;
            for (k, &tk) in t.iter().enumerate() {
                s += tk * ((k + 1) as f64 * theta).cos();
            }
            (1.0 + 2.0 * r * s) * inv_n
        })
        .collect();
    normalise_peak(&mut w);
    w
}

/// Longest Dolph-Chebyshev window evaluated by the direct cosine sum.
pub const CHEBYSHEV_DIRECT_MAX: usize = 8192;

/// `T(x0 cos(k pi / N))` for `k = 1 ..= N/2`.
fn chebyshev_terms(n: usize, r: f64) -> Vec<f64> {
    let nm1 = (n - 1) as f64;
    let x0 = ((1.0 / r).acosh() / nm1).cosh();
    let pi_n = PI * (1.0 / n as f64);
    (1..=n / 2)
        .map(|k| {
            let x = x0 * (k as f64 * pi_n).cos();
            // x >= 0 here (k <= N/2), so |x| > 1 means x > 1.
            if x > 1.0 {
                (nm1 * x.acosh()).cosh()
            } else {
                (nm1 * x.acos()).cos()
            }
        })
        .collect()
}

/// The cosine sum of [`chebyshev_symmetric`] as one length-N inverse DFT:
/// `sum_k r T_k cos(k theta_n) = Re sum_k a_k e^(2 pi i k n / N)` with
/// `a_k = r T_k e^(-2 pi i k c / N)`, `c = (N - 1) / 2`, peak-normalised.
pub(crate) fn chebyshev_fft(n: usize, r: f64, t: &[f64]) -> Vec<f64> {
    use rustfft::FftPlanner;
    use rustfft::num_complex::Complex64;
    let mut a = vec![Complex64::new(0.0, 0.0); n];
    for (k, &tk) in t.iter().enumerate() {
        let k = k + 1;
        // Reduce k c mod N exactly before the angle (k c can be ~1e9).
        let kc2 = (k as u128 * (n as u128 - 1)) % (2 * n as u128);
        let phi = -PI * kc2 as f64 / n as f64;
        a[k] = Complex64::from_polar(r * tk, phi);
    }
    FftPlanner::<f64>::new().plan_fft_inverse(n).process(&mut a);
    let inv_n = 1.0 / n as f64;
    let mut w: Vec<f64> = a.iter().map(|x| (1.0 + 2.0 * x.re) * inv_n).collect();
    normalise_peak(&mut w);
    w
}

/// Symmetric flat-top window (divides by N - 1), normalised to a peak of 1.
pub fn flattop_symmetric(n: usize, coeffs: &[f64]) -> Vec<f64> {
    if n <= 1 {
        return vec![1.0; n];
    }
    let mut w = flattop_periodic(n - 1, coeffs);
    // The symmetric window of N taps is the periodic one of N - 1 taps with
    // its first sample repeated at the end.
    w.push(w[0]);
    normalise_peak(&mut w);
    w
}

/// Periodic 4-term Nuttall window (divides by N).
pub fn nuttall_periodic(n: usize) -> Vec<f64> {
    let mut w = nuttall_symmetric(n + 1);
    w.truncate(n);
    w
}

/// Symmetric Hann, `0.5 * (1 - cos(2 pi n / (N - 1)))`.
pub fn hann_symmetric(n: usize) -> Vec<f64> {
    if n <= 1 {
        return vec![1.0; n];
    }
    (0..n)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f64 / (n - 1) as f64).cos()))
        .collect()
}

/// Periodic Dolph-Chebyshev window (the symmetric one of N + 1 taps without
/// its last sample), normalised to a peak of 1.
pub fn chebyshev_periodic(n: usize, r: f64) -> Vec<f64> {
    let mut w = chebyshev_symmetric(n + 1, r);
    w.truncate(n);
    normalise_peak(&mut w);
    w
}

/// Gaussian `exp(-0.5 ((n - c) / (alpha c))^2)` with the exact centre: `c =
/// (N - 1) / 2` (`symmetric`) or `N / 2` (`periodic`), so `alpha` is sigma
/// as a fraction of the half length, as in [`gaussian`].
pub fn gaussian_centred(n: usize, alpha: f64, periodic: bool) -> Vec<f64> {
    let c = if periodic {
        n as f64 / 2.0
    } else {
        n.saturating_sub(1) as f64 / 2.0
    };
    if c == 0.0 {
        return vec![1.0; n];
    }
    (0..n)
        .map(|i| {
            let x = (i as f64 - c) / (alpha * c);
            (-0.5 * x * x).exp()
        })
        .collect()
}

/// Periodic Tukey window: the symmetric one ([`crate::window::tukey`]) of
/// N + 1 taps without its last sample.
pub fn tukey_periodic(n: usize, taper: f64) -> Vec<f64> {
    let mut w = crate::window::tukey(n + 1, taper);
    w.truncate(n);
    w
}

/// The samples of the design window `w` over `n` taps, for the windows and
/// samplings the preset paths build besides their legacy ones: every window
/// except the `standard` path's Gaussian, sampled
/// `periodic` or `symmetric` (`integer_centre`: the Gaussian only; DPSS:
/// `symmetric` only). `None` for the others.
pub(crate) fn sample(w: crate::design::Window, n: usize) -> Option<Vec<f64>> {
    use crate::design::{FlattopSet, Sampling::*, Window as W};
    let set = |s| match s {
        FlattopSet::Hft196d => HFT196D,
        FlattopSet::Hft223d => HFT223D,
        FlattopSet::Hft248d => HFT248D,
    };
    Some(match w {
        W::Kaiser {
            beta: Some(beta),
            sampling: Periodic,
        } => kaiser_periodic(n, beta),
        W::Kaiser {
            beta: Some(beta),
            sampling: Symmetric,
        } => crate::window::kaiser(n, beta),
        W::Gaussian {
            alpha,
            sampling: IntegerCentre,
        } => gaussian(n, alpha),
        W::Gaussian { alpha, sampling } if sampling != IntegerCentre => {
            gaussian_centred(n, alpha, sampling == Periodic)
        }
        W::Flattop {
            set: s,
            sampling: Periodic,
        } => flattop_periodic(n, set(s)),
        W::Flattop {
            set: s,
            sampling: Symmetric,
        } => flattop_symmetric(n, set(s)),
        W::Chebyshev {
            sidelobe_ratio,
            sampling: Symmetric,
        } => chebyshev_symmetric(n, sidelobe_ratio),
        W::Chebyshev {
            sidelobe_ratio,
            sampling: Periodic,
        } => chebyshev_periodic(n, sidelobe_ratio),
        W::Nuttall {
            sampling: Symmetric,
        } => nuttall_symmetric(n),
        W::Nuttall { sampling: Periodic } => nuttall_periodic(n),
        W::Hann { sampling: Periodic } => hann_periodic(n),
        W::Hann {
            sampling: Symmetric,
        } => hann_symmetric(n),
        W::Tukey {
            taper,
            sampling: Symmetric,
        } => crate::window::tukey(n, taper),
        W::Tukey {
            taper,
            sampling: Periodic,
        } => tukey_periodic(n, taper),
        W::Dpss {
            nw: Some(nw),
            sampling: Symmetric,
        } => crate::window::dpss(n, nw),
        _ => return None,
    })
}

fn normalise_peak(w: &mut [f64]) {
    let peak = w.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if peak > 0.0 {
        for v in w {
            *v /= peak;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chebyshev_fft_matches_the_direct_sum() {
        for &n in &[1656usize, 4095, 8160] {
            let r = 2f64.powi(-32);
            let direct = chebyshev_symmetric(n, r);
            let fft = chebyshev_fft(n, r, &chebyshev_terms(n, r));
            let err = direct
                .iter()
                .zip(&fft)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(err < 1e-12, "n {n}: {err}");
        }
        // Above the direct limit the FFT path runs, symmetric and peaked.
        let w = chebyshev_symmetric(40_800, 2f64.powi(-32));
        let peak = w.iter().copied().fold(0.0, f64::max);
        assert_eq!(peak, 1.0);
        for i in 0..w.len() / 2 {
            assert!((w[i] - w[w.len() - 1 - i]).abs() < 1e-12);
        }
    }

    #[test]
    fn chebyshev_is_symmetric_and_peaks_at_one() {
        for &n in &[35usize, 255, 1656] {
            let w = chebyshev_symmetric(n, 2f64.powi(-32));
            let peak = w.iter().copied().fold(0.0, f64::max);
            assert!((peak - 1.0).abs() < 1e-15);
            for i in 0..n / 2 {
                assert!((w[i] - w[n - 1 - i]).abs() < 1e-12, "n {n} i {i}");
            }
        }
    }

    #[test]
    fn kaiser_series_matches_reference_i0() {
        for &x in &[0.0, 1.0, 7.0 * PI, 8.75 * PI, 14.96454265] {
            let a = bessel_i0_series(x);
            let b = crate::window::bessel_i0(x);
            assert!(((a - b) / b).abs() < 1e-13, "x {x}: {a} vs {b}");
        }
    }

    #[test]
    fn flattop_peak_is_one() {
        let w = flattop_periodic(4088, HFT248D);
        assert!((w.iter().copied().fold(0.0, f64::max) - 1.0).abs() < 1e-15);
    }
}
