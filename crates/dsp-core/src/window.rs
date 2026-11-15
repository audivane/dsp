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

#[cfg(test)]
mod tests {
    use super::*;

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
