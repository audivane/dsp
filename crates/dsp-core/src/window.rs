//! Window functions and Kaiser design helpers.

/// Zeroth-order modified Bessel function of the first kind.
///
/// Uses the convergent power series
/// `I0(x) = sum_{k=0..inf} ((x/2)^k / k!)^2`
/// until terms fall below machine precision, achieving ~1e-14 relative accuracy
/// across the finite f64 range of `I0(x)`.
pub fn bessel_i0(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    let ax = x.abs();
    if ax == 0.0 {
        return 1.0;
    }
    if ax.is_infinite() {
        return f64::INFINITY;
    }

    let half_x = 0.5 * ax;
    let mut sum = 1.0_f64;
    let mut term = 1.0_f64;
    let mut k = 1.0_f64;

    loop {
        let ratio = half_x / k;
        term *= ratio * ratio;
        sum += term;
        if term <= sum * (f64::EPSILON * 0.25) || !sum.is_finite() {
            break;
        }
        k += 1.0;
    }

    sum
}

/// Symmetric Kaiser window of length `n`.
pub fn kaiser(n: usize, beta: f64) -> Vec<f64> {
    match n {
        0 => Vec::new(),
        1 => vec![1.0],
        _ => {
            let mut w = vec![0.0; n];
            let denom = bessel_i0(beta);
            let m = (n - 1) as f64;
            let half = n.div_ceil(2);
            for k in 0..half {
                let r = (2.0 * k as f64 - m) / m;
                let arg = (1.0 - r * r).max(0.0).sqrt();
                let val = bessel_i0(beta * arg) / denom;
                w[k] = val;
                w[n - 1 - k] = val;
            }
            w
        }
    }
}

/// Kaiser beta for a stopband attenuation in dB (Kaiser's formula).
pub fn kaiser_beta_for_attenuation(atten_db: f64) -> f64 {
    if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    }
}

/// How far under the target stopband the Gaussian window's truncation edge
/// sits (dB). Cutting the Gaussian at `w = 10^{-(A + margin)/20}` keeps the
/// truncation sidelobes under `-A`.
pub const GAUSSIAN_EDGE_MARGIN_DB: f64 = 10.0;

/// Standard deviation (in taps) of the Gaussian window of `n` taps that
/// targets a stopband of `atten_db`: the window is cut where it has fallen to
/// `-(atten_db + GAUSSIAN_EDGE_MARGIN_DB)` dB.
pub fn gaussian_sigma(n: usize, atten_db: f64) -> f64 {
    let half = (n.max(1) - 1) as f64 / 2.0;
    let depth = (atten_db + GAUSSIAN_EDGE_MARGIN_DB) / 20.0 * std::f64::consts::LN_10;
    half / (2.0 * depth).sqrt()
}

/// Gaussian window of `n` taps for a stopband of `atten_db` (see
/// [`gaussian_sigma`]). A Gaussian-windowed sinc is the ideal low-pass
/// convolved with a Gaussian: its transition is an erf curve with no
/// sidelobes of its own, and its impulse response decays like a Gaussian.
pub fn gaussian(n: usize, atten_db: f64) -> Vec<f64> {
    match n {
        0 => Vec::new(),
        1 => vec![1.0],
        _ => {
            let sigma = gaussian_sigma(n, atten_db);
            let c = (n - 1) as f64 / 2.0;
            (0..n)
                .map(|k| {
                    let t = (k as f64 - c) / sigma;
                    (-0.5 * t * t).exp()
                })
                .collect()
        }
    }
}

/// Tukey window of `n` taps: 1 over the middle, a raised-cosine taper over
/// the outer `taper` fraction of each half (`taper` 1 is a Hann window, 0 a
/// rectangle).
pub fn tukey(n: usize, taper: f64) -> Vec<f64> {
    match n {
        0 => Vec::new(),
        1 => vec![1.0],
        _ => {
            let half = (n - 1) as f64 / 2.0;
            let edge = taper.clamp(0.0, 1.0) * half;
            (0..n)
                .map(|k| {
                    // Distance from the nearer end, in taps.
                    let d = (k as f64).min((n - 1 - k) as f64);
                    if edge <= 0.0 || d >= edge {
                        1.0
                    } else {
                        0.5 * (1.0 - (std::f64::consts::PI * d / edge).cos())
                    }
                })
                .collect()
        }
    }
}

/// Complementary error function for `x >= 0` (relative error ~1e-15):
/// Taylor series of erf below 2, Lentz continued fraction above.
pub fn erfc(x: f64) -> f64 {
    use std::f64::consts::PI;
    if x < 0.0 {
        return 2.0 - erfc(-x);
    }
    if x < 2.0 {
        // erf(x) = 2/sqrt(pi) sum (-1)^n x^(2n+1) / (n! (2n+1))
        let x2 = x * x;
        let (mut term, mut sum) = (x, x);
        for n in 1..200 {
            term *= -x2 / n as f64;
            let add = term / (2 * n + 1) as f64;
            sum += add;
            if add.abs() < 1e-17 * sum.abs() {
                break;
            }
        }
        return 1.0 - 2.0 / PI.sqrt() * sum;
    }
    // erfc(x) = exp(-x^2)/sqrt(pi) * 1/(x + (1/2)/(x + 1/(x + (3/2)/(x + ...))))
    let tiny = 1e-300;
    let mut f = x;
    let mut c = x;
    let mut d = 0.0;
    for n in 1..500 {
        let a = n as f64 / 2.0;
        d = x + a * d;
        d = if d.abs() < tiny { tiny } else { d };
        c = x + a / c;
        c = if c.abs() < tiny { tiny } else { c };
        d = 1.0 / d;
        let delta = c * d;
        f *= delta;
        if (delta - 1.0).abs() < 1e-16 {
            break;
        }
    }
    (-x * x).exp() / PI.sqrt() / f
}

/// Inverse of [`erfc`] for `0 < y < 1` (Newton on `ln erfc`).
pub fn erfc_inv(y: f64) -> f64 {
    use std::f64::consts::PI;
    assert!(y > 0.0 && y < 1.0, "erfc_inv domain: {y}");
    let ln_y = y.ln();
    // erfc(x) ~ exp(-x^2) / (x sqrt(pi)) for large x.
    let mut x = (-ln_y).sqrt().max(0.1);
    for _ in 0..60 {
        let e = erfc(x);
        let g = e.ln() - ln_y;
        let dg = -2.0 * (-x * x).exp() / (PI.sqrt() * e);
        let step = g / dg;
        x -= step;
        if step.abs() < 1e-14 * x.abs().max(1.0) {
            break;
        }
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erfc_matches_reference_values() {
        // scipy.special.erfc
        for (x, want) in [
            (0.0, 1.0),
            (0.5, 0.479_500_122_186_953_5),
            (1.0, 0.157_299_207_050_285_13),
            (1.9, 0.007_209_570_764_742_524),
            (2.0, 0.004_677_734_981_047_266),
            (3.0, 2.209_049_699_858_544e-5),
            (5.0, 1.537_459_794_428_034_8e-12),
            (8.0, 1.122_429_717_298_292_7e-29),
        ] {
            let got = erfc(x);
            assert!(
                ((got - want) / want).abs() < 1e-13,
                "erfc({x}) = {got:e}, want {want:e}"
            );
        }
        for y in [1e-3, 1e-7, 2e-12, 0.3] {
            assert!(((erfc(erfc_inv(y)) - y) / y).abs() < 1e-12, "{y}");
        }
    }

    #[test]
    fn tukey_window_is_flat_in_the_middle_and_tapers_at_the_ends() {
        let w = tukey(1001, 0.25);
        // 125 taps of taper on each side, the rest exactly 1.
        assert_eq!(w[0], 0.0);
        assert_eq!(w[1000], 0.0);
        assert!(w[124] < 1.0 && w[125] == 1.0 && w[875] == 1.0 && w[876] < 1.0);
        assert!((w[62] - 0.5).abs() < 0.02);
        assert!(tukey(9, 0.0).iter().all(|&v| v == 1.0));
        // taper 1: Hann.
        let hann = tukey(9, 1.0);
        assert!((hann[2] - 0.5).abs() < 1e-12 && hann[4] == 1.0);
    }

    #[test]
    fn gaussian_window_edges_sit_under_the_target() {
        let w = gaussian(8191, 140.0);
        let edge_db = 20.0 * w[0].log10();
        assert!((edge_db + 150.0).abs() < 1e-9, "{edge_db}");
        assert_eq!(w[4095], 1.0);
        assert_eq!(w[0], w[8190]);
    }

    #[test]
    fn bessel_i0_matches_scipy_reference() {
        let cases = [
            (0.0, 1.0),
            (-0.0, 1.0),
            (1.0, 1.2660658777520082),
            (-1.0, 1.2660658777520082),
            (5.0, 27.239871823604442),
            (14.0, 129418.56270064856),
            (50.0, 2.9325537838493355e20),
        ];
        for (x, expected) in cases {
            let got = bessel_i0(x);
            let rel_err = (got - expected).abs() / expected;
            assert!(
                rel_err < 1e-14,
                "bessel_i0({x}): got {got}, expected {expected}, rel_err {rel_err}"
            );
        }
    }

    #[test]
    fn kaiser_9_14_matches_scipy() {
        // Reference: python3 -c "import scipy.signal as s; print(s.windows.kaiser(9,14).tolist())"
        let expected = [
            7.726866835270368e-6,
            0.01080066848590914,
            0.16493218754795197,
            0.6517418592472894,
            1.0,
            0.6517418592472894,
            0.16493218754795197,
            0.01080066848590914,
            7.726866835270368e-6,
        ];
        let got = kaiser(9, 14.0);
        assert_eq!(got.len(), expected.len());
        for (i, (&g, &e)) in got.iter().zip(expected.iter()).enumerate() {
            let err = (g - e).abs();
            assert!(err < 1e-14, "idx {i}: got {g}, expected {e}, err {err}");
        }
    }

    #[test]
    fn kaiser_edge_cases_and_symmetry() {
        assert!(kaiser(0, 14.0).is_empty());
        assert_eq!(kaiser(1, 14.0), vec![1.0]);

        let rect = kaiser(8, 0.0);
        assert!(rect.iter().all(|&v| (v - 1.0).abs() < 1e-15));

        for n in [2, 7, 8, 64, 129] {
            let w = kaiser(n, 14.0);
            assert_eq!(w.len(), n);
            for k in 0..n {
                assert_eq!(w[k], w[n - 1 - k], "not symmetric at n={n}, k={k}");
            }
            if n % 2 == 1 {
                assert_eq!(w[n / 2], 1.0);
            }
        }
    }

    #[test]
    fn kaiser_beta_matches_scipy() {
        let cases = [
            (15.0, 0.0),
            (21.0, 0.0),
            (30.0, 2.1166248611409806),
            (50.0, 4.533514120981248),
            (60.0, 5.65326),
            (120.0, 12.26526),
            (140.0, 14.469260000000002),
        ];
        for (atten, expected) in cases {
            let got = kaiser_beta_for_attenuation(atten);
            assert!(
                (got - expected).abs() < 1e-12,
                "atten={atten}: got {got}, expected {expected}"
            );
        }
    }
}
