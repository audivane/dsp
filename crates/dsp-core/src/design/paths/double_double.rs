//! The `double_double` arithmetic path: the Gaussian phase presets'
//! prototype, with window and sinc in double-double, rounded once (the
//! cepstral transform reacts to the last bits of the prototype, see
//! [`super::exact`]).

use crate::design::ddouble::{Dd, two_prod};
use crate::design::paths::fir::{Recipe, Window};

/// The Gaussian recipe's prototype in double-double, rounded once
/// (length `ratio * taps`, not normalised); `None` for other windows.
pub fn gaussian(r: &Recipe, ratio: usize) -> Option<Vec<f64>> {
    gaussian_len(r, ratio * r.taps, ratio)
}

/// [`gaussian`] of `n` taps (any length) with the sinc divided by `ratio`.
pub fn gaussian_len(r: &Recipe, n: usize, ratio: usize) -> Option<Vec<f64>> {
    let Window::Gaussian { alpha } = r.window else {
        return None;
    };
    let centre = (n / 2) as i64;
    let w = gaussian_window(n, alpha);
    Some(
        w.iter()
            .enumerate()
            .map(|(i, &wi)| (sinc_dd(i as i64 - centre, r.cutoff, ratio) * wi).to_f64())
            .collect(),
    )
}

/// `sinc(fc k / L)` with `sinc(x) = sin(pi x) / (pi x)`, in double-double.
fn sinc_dd(k: i64, fc: f64, l: usize) -> Dd {
    if k == 0 {
        return Dd::ONE;
    }
    let (p, e) = two_prod(k as f64, fc);
    let x = Dd { hi: p, lo: e }.div_f64(l as f64);
    let (s, _) = x.sin_cos_pi();
    s / (Dd::PI * x)
}

/// [`crate::design::paths::fir::windows::gaussian`] in double-double: centre
/// `(N - 1) / 2` rounded down.
fn gaussian_window(n: usize, alpha: f64) -> Vec<Dd> {
    if n == 0 {
        return Vec::new();
    }
    let c = ((n - 1) / 2) as f64;
    if c == 0.0 {
        return vec![Dd::ONE; n];
    }
    let (p, e) = two_prod(alpha, c);
    let sigma = Dd { hi: p, lo: e };
    (0..n)
        .map(|i| {
            let x = Dd::new(i as f64 - c) / sigma;
            (-x.sqr().scale(0.5)).exp()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::paths::fir::windows;
    use std::f64::consts::PI;

    #[test]
    fn window_matches_the_f64_definition() {
        for &n in &[1016usize, 2040, 2041] {
            let g: Vec<f64> = gaussian_window(n, 0.128)
                .iter()
                .map(|v| v.to_f64())
                .collect();
            let d = g
                .iter()
                .zip(&windows::gaussian(n, 0.128))
                .map(|(x, y)| (x - y).abs())
                .fold(0.0, f64::max);
            assert!(d < 1e-15);
        }
    }

    #[test]
    fn sinc() {
        // fc 27/32 at x8: k = 256 puts sinc on its 27th zero.
        assert_eq!(sinc_dd(256, 27.0 / 32.0, 8).to_f64(), 0.0);
        let v = sinc_dd(3, 0.5, 2).to_f64();
        let want = (0.75 * PI).sin() / (0.75 * PI);
        assert!((v - want).abs() < 1e-16);
    }
}
