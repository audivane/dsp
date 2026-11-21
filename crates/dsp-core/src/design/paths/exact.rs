//! The `exact_*` arithmetic paths: prototypes for the Chebyshev, Kaiser
//! and Nuttall phase presets, built so that their last bits are defined by
//! the recipe.
//!
//! The cepstral transform amplifies tiny stopband differences: moving every
//! tap of an f64 prototype by one ulp moves the minimum-phase taps by up to
//! -96 dB (Kaiser) or -20 dB (Gaussian) of the peak. So these recipes fix
//! the arithmetic of their prototypes too:
//!
//! - **Chebyshev and Kaiser designs** ([`Exact`]): an f64 sinc with a fixed
//!   operation order ([`SincForm`]), times the window rounded to f64, then
//!   a sequential branch sum. The Chebyshev window is computed exactly (in
//!   double-double) and rounded; the Kaiser window is evaluated with x87
//!   extended-precision (64-bit significand) rounding after every step,
//!   which is what defines its last bits.
//! - **The short leaky design**: the same f64 sinc form, times the symmetric
//!   Nuttall window in f64, normalised after the transform with the
//!   pairwise sum.
//! - Gaussian designs: [`super::double_double`]. Flat-top and Lanczos
//!   designs use the f64 builders of [`crate::design::paths::fir`]
//!   unchanged.

use crate::design::ddouble::Dd;
use crate::design::paths::fir::{Recipe, Window, windows};
use std::f64::consts::PI;

/// How an f64 sinc forms its argument `x = pi fc k / L` (and divides by the
/// same `x`): the rounding order is part of the recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SincForm {
    /// `(k * (fc / L)) * pi`.
    CutoffOverRatio,
    /// `k * ((fc * pi) * (1 / L))`; for odd N the last tap uses
    /// `((fc * pi) * k) * (1 / L)`.
    PiCutoffTimesInverseRatio,
}

/// An f64-arithmetic prototype: sinc in `form`, window rounded to f64.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Exact {
    pub form: SincForm,
}

/// `sin(x) / x` over the kernel, centre `N / 2`, in the given form.
pub fn sinc_f64(n: usize, fc: f64, ratio: usize, form: SincForm) -> Vec<f64> {
    let centre = (n / 2) as i64;
    let l = ratio as f64;
    let fcd = fc / l;
    let c = (fc * PI) * (1.0 / l);
    (0..n)
        .map(|i| {
            let k = i as i64 - centre;
            if k == 0 {
                return 1.0;
            }
            let kf = k as f64;
            let x = match form {
                SincForm::CutoffOverRatio => (kf * fcd) * PI,
                SincForm::PiCutoffTimesInverseRatio if n % 2 == 1 && i == n - 1 => {
                    ((fc * PI) * kf) * (1.0 / l)
                }
                SincForm::PiCutoffTimesInverseRatio => kf * c,
            };
            x.sin() / x
        })
        .collect()
}

/// The prototype of `r` built with f64 arithmetic in `e`'s form, or `None`
/// for windows that have no such definition.
pub fn exact(r: &Recipe, ratio: usize, e: Exact) -> Option<Vec<f64>> {
    let n = ratio * r.taps;
    let w = match r.window {
        Window::ChebyshevSymmetric { r } => chebyshev_f64(n, r),
        Window::KaiserPeriodic { beta } => kaiser_periodic_x87(n, beta / PI),
        Window::NuttallSymmetric => windows::nuttall_symmetric(n),
        _ => return None,
    };
    let mut h = sinc_f64(n, r.cutoff, ratio, e.form);
    for (v, w) in h.iter_mut().zip(&w) {
        *v *= w;
    }
    Some(h)
}

/// Scale `h` so that branch `ratio / 2` sums to 1: a left-to-right sum, then
/// a multiply by its reciprocal.
pub fn normalise_sequential(h: &mut [f64], ratio: usize) {
    let mut g = 0.0;
    for v in h.iter().skip(ratio / 2).step_by(ratio) {
        g += v;
    }
    let s = 1.0 / g;
    for v in h.iter_mut() {
        *v *= s;
    }
}

/// [`windows::kaiser_periodic`] with `beta = pi * alpha`,
/// every step rounded to a 64-bit significand:
///
/// ```text
/// beta = pi * alpha, h = beta / 2, c = N / 2
/// I0(z) = sum_{k=0..64} (z^k / k!)^2      (k! an f64 product, z^k rounded)
/// x = (1/c) (n - c), z = h sqrt(1 - x^2)
/// w[n] = (1 / I0(h)) * I0(z), rounded to f64
/// ```
fn kaiser_periodic_x87(n: usize, alpha: f64) -> Vec<f64> {
    let r = Dd::round_x87;
    // k! as a running f64 product
    let mut fact = [1.0f64; 65];
    for k in 1..65 {
        fact[k] = fact[k - 1] * k as f64;
    }
    let i0 = |z: Dd| {
        let mut pow = Dd::ONE; // z^k, carried exactly, rounded per use
        let mut sum = Dd::ZERO;
        for &f in &fact {
            let t = r(r(pow).div_f64(f));
            sum = r(r(t * t) + sum);
            pow = pow * z;
        }
        sum
    };
    let pi = r(Dd::PI);
    let beta = r(pi.mul_f64(alpha));
    let half = r(beta.scale(0.5));
    let inv_i0 = r(Dd::ONE / i0(half));
    let c = n as f64 * 0.5;
    let inv_c = r(Dd::ONE.div_f64(c));
    (0..n)
        .map(|i| {
            let x = r(inv_c.mul_f64(i as f64 - c));
            let z = r(half * r(r(Dd::ONE - r(x * x)).sqrt()));
            r(inv_i0 * i0(z)).to_f64()
        })
        .collect()
}

/// `cos` for arguments up to about 1e6 in magnitude, accurate to a few
/// units of 2^-100: quadrant reduction, then a table of `cos/sin(m/64)` and
/// short Taylor series for the remainder (`|d| <= 1/128`).
struct FastCos {
    /// `(sin(m/64), cos(m/64))` for `m = -51 ..= 51`.
    table: Vec<(Dd, Dd)>,
}

impl FastCos {
    const HALF_PI: Dd = Dd {
        hi: std::f64::consts::FRAC_PI_2,
        lo: 6.123_233_995_736_766e-17,
    };

    fn new() -> FastCos {
        let table = (-51..=51)
            .map(|m| Dd::new(m as f64 / 64.0).sin_cos())
            .collect();
        FastCos { table }
    }

    fn cos(&self, x: Dd) -> Dd {
        self.sin_cos(x).1
    }

    fn sin_cos(&self, x: Dd) -> (Dd, Dd) {
        let j = (x.hi / std::f64::consts::FRAC_PI_2).round();
        let r = x - FastCos::HALF_PI.mul_f64(j);
        let m = (r.hi * 64.0).round();
        let d = r - Dd::new(m / 64.0);
        let d2 = d.sqr();
        // sin d = d (1 - d^2/6 (1 - d^2/20 (1 - d^2/42 (1 - d^2/72))))
        let mut sd = Dd::ONE;
        for q in [72.0, 42.0, 20.0, 6.0] {
            sd = Dd::ONE - (d2 * sd).div_f64(q);
        }
        let sd = d * sd;
        // cos d = 1 - d^2/2 (1 - d^2/12 (1 - d^2/30 (1 - d^2/56 (1 - d^2/90))))
        let mut cd = Dd::ONE;
        for q in [90.0, 56.0, 30.0, 12.0, 2.0] {
            cd = Dd::ONE - (d2 * cd).div_f64(q);
        }
        let (sm, cm) = self.table[(m as i64 + 51) as usize];
        let c = cm * cd - sm * sd;
        let s = sm * cd + cm * sd;
        match (j as i64).rem_euclid(4) {
            0 => (s, c),
            1 => (c, -s),
            2 => (-s, -c),
            _ => (-c, s),
        }
    }
}

/// [`windows::chebyshev_symmetric`] evaluated in x87
/// extended precision (every step rounded to a 64-bit significand), stored
/// as f64, then scaled by the reciprocal of its f64 peak:
///
/// ```text
/// x0      = cosh(acosh(1/r) / (N-1)),   pi_N = pi * (1/N)
/// T_k     = T(x0 cos(k pi_N)),  T(x) = cosh((N-1) acosh x) or cos((N-1) acos x)
/// theta_n = (2n + 1 - N) pi_N
/// s_n     = sum_{k=1..N/2} T_k cos(k theta_n)            (in order of k)
/// w[n]    = (1 + 2r s_n) * (1/N as f64)
/// ```
///
/// The rounded angles (`pi_N`, `k theta_n`) shift the taps by up to about
/// 1e-14 relative against the exact window; that shift is part of the
/// recipe.
///
/// Above [`windows::CHEBYSHEV_DIRECT_MAX`] taps (only on a
/// rational ratio's grid) the O(N^2) extended-precision sum is far too
/// slow; the f64 FFT evaluation of the same window is used instead.
fn chebyshev_f64(n: usize, r: f64) -> Vec<f64> {
    if n <= 1 {
        return vec![1.0; n];
    }
    if n > windows::CHEBYSHEV_DIRECT_MAX {
        return windows::chebyshev_symmetric(n, r);
    }
    let x87 = Dd::round_x87;
    let fast = FastCos::new();
    let cosl = |x: Dd| x87(fast.cos(x));
    let rl = Dd::new(r);
    let nm1 = Dd::new((n - 1) as f64);
    let x0 = x87(x87(x87(Dd::ONE / rl).acosh()) / nm1).cosh();
    let x0 = x87(x0);
    let pi = x87(Dd::PI);
    let pi_n = x87(pi * x87(Dd::ONE.div_f64(n as f64)));
    let t: Vec<Dd> = (1..=n / 2)
        .map(|k| {
            let x = x87(x0 * cosl(x87(pi_n.mul_f64(k as f64))));
            if x.hi > 1.0 || (x.hi == 1.0 && x.lo > 0.0) {
                x87(x87(nm1 * x87(x.acosh())).cosh())
            } else {
                cosl(x87(nm1 * x87(x.acos())))
            }
        })
        .collect();
    let two_r = Dd::new(2.0 * r);
    let inv_n = 1.0 / n as f64;
    // One row: s_n with cos(round(k theta)) as cos(k theta) - d sin(k theta),
    // d = round(k theta) - k theta (|d| < 1e-15, so d^2 is far below the
    // 64-bit step), and cos/sin(k theta) by an exact rotation recurrence.
    let row = |i: usize| {
        let theta = x87(pi_n.mul_f64((2 * i as i64 + 1 - n as i64) as f64));
        let (s1, c1) = fast.sin_cos(theta);
        let (mut sk, mut ck) = (s1, c1);
        let mut s = Dd::ZERO;
        for (k, &tk) in t.iter().enumerate() {
            let kt = theta.mul_f64((k + 1) as f64);
            let d = (x87(kt) - kt).to_f64();
            let c = x87(ck - Dd::new(d * sk.hi));
            s = x87(s + x87(tk * c));
            (ck, sk) = (ck * c1 - sk * s1, sk * c1 + ck * s1);
        }
        x87(x87(Dd::ONE + x87(two_r * s)).mul_f64(inv_n)).to_f64()
    };
    // symmetric: theta changes sign exactly between n and N - 1 - n
    let half = n.div_ceil(2);
    let mut w = vec![0.0; n];
    par_fill(&mut w[..half], row);
    for i in 0..n / 2 {
        w[n - 1 - i] = w[i];
    }
    let peak = w.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let scale = 1.0 / peak;
    w.iter().map(|v| v * scale).collect()
}

/// `out[i] = f(i)`, split over a few threads for long rows.
fn par_fill<F>(out: &mut [f64], f: F)
where
    F: Fn(usize) -> f64 + Sync,
{
    let threads = if out.len() < 256 {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, |t| t.get())
            .clamp(1, 8)
    };
    let per = out.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (chunk, part) in out.chunks_mut(per).enumerate() {
            let f = &f;
            scope.spawn(move || {
                for (j, v) in part.iter_mut().enumerate() {
                    *v = f(chunk * per + j);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn max_diff(a: &[f64], b: &[f64]) -> f64 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f64::max)
    }

    #[test]
    fn windows_match_the_f64_definitions() {
        for &n in &[1016usize, 2040, 2041] {
            let beta = 8.75 * PI;
            let k = kaiser_periodic_x87(n, beta / PI);
            assert!(max_diff(&k, &windows::kaiser_periodic(n, beta)) < 1e-14);
        }
        for &n in &[568usize, 936, 1656, 35] {
            let r = 2f64.powi(-32);
            let d = max_diff(&chebyshev_f64(n, r), &windows::chebyshev_symmetric(n, r));
            // the f64 sum cancels several digits
            assert!(d < 1e-11, "n {n}: {d}");
        }
    }

    #[test]
    fn fast_cos_matches_the_series() {
        let f = FastCos::new();
        for x in [0.0, 0.3, -1.0, 2.5, 100.25, -2600.123, 31415.9] {
            let x = Dd::new(x);
            let d = (f.cos(x) - x.sin_cos().1).to_f64().abs();
            assert!(d < 1e-28, "{x:?}: {d}");
        }
    }

    #[test]
    fn kaiser_alpha_round_trips() {
        assert_eq!(8.75 * PI / PI, 8.75);
    }

    #[test]
    fn chebyshev_is_symmetric_and_peaks_at_one() {
        let w = chebyshev_f64(1656, 2f64.powi(-32));
        for i in 0..w.len() / 2 {
            assert_eq!(w[i], w[w.len() - 1 - i]);
        }
        assert!((w[827] - 1.0).abs() <= f64::EPSILON);
    }

    #[test]
    fn sinc_forms() {
        for form in [
            SincForm::CutoffOverRatio,
            SincForm::PiCutoffTimesInverseRatio,
        ] {
            let s = sinc_f64(9, 0.5, 3, form);
            assert_eq!(s[4], 1.0);
            let want = (PI / 6.0).sin() / (PI / 6.0);
            assert!((s[5] - want).abs() < 1e-16 && (s[3] - want).abs() < 1e-16);
        }
        // the two forms round differently somewhere
        let a = sinc_f64(1656, 0.930_272_108_843_537_4, 8, SincForm::CutoffOverRatio);
        let b = sinc_f64(
            1656,
            0.930_272_108_843_537_4,
            8,
            SincForm::PiCutoffTimesInverseRatio,
        );
        assert!(a != b && max_diff(&a, &b) < 1e-15);
    }

    #[test]
    fn sequential_normalisation() {
        let mut h = vec![0.25, 1.0, 0.5, 1.0, 0.25, 1.0];
        normalise_sequential(&mut h, 2);
        assert_eq!(
            h,
            vec![
                1.0 / 12.0,
                1.0 / 3.0,
                1.0 / 6.0,
                1.0 / 3.0,
                1.0 / 12.0,
                1.0 / 3.0
            ]
        );
    }
}
