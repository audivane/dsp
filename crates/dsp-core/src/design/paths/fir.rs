//! The `fir` arithmetic path: windowed sinc and Lanczos kernels in f64.
//!
//! Every recipe is a kernel at the output rate of length `N = L * T`
//! (L the ratio, T taps per input sample):
//!
//! ```text
//! h[n] = sinc(fc * (n - floor(N/2)) / L) * w[n]      sinc(x) = sin(pi x) / (pi x)
//! ```
//!
//! with `fc` the -6 dB point as a fraction of the source Nyquist and `w`
//! one of the windows of [`windows`], each with its own sampling
//! convention. The sinc centre `floor(N/2)` is the kernel's delay. The
//! symmetric windows (Gaussian, Nuttall, Chebyshev) are not centred on it
//! for even N, which leaves those kernels very slightly asymmetric; that is
//! part of the recipes. Lanczos kernels replace the windowed sinc by
//! `sinc(x) sinc(x / a)` over exactly `+-a` lobes.
//!
//! Normalisation scales the kernel so that one fixed polyphase branch,
//! `h[floor(L/2) + j L]`, sums to exactly 1 (whatever the parity of T); the
//! other branches then sum to 1 to within about 1e-5 for the shortest
//! kernels and much closer for the long ones.

pub mod windows;

use std::f64::consts::PI;

/// Window of a recipe, with the exact sampling conventions of
/// [`windows`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Window {
    /// [`windows::gaussian`].
    Gaussian { alpha: f64 },
    /// [`windows::kaiser_periodic`].
    KaiserPeriodic { beta: f64 },
    /// [`windows::flattop_periodic`] with these coefficients.
    Flattop(&'static [f64]),
    /// [`windows::nuttall_symmetric`].
    NuttallSymmetric,
    /// [`windows::chebyshev_symmetric`] with sidelobe ratio `r`.
    ChebyshevSymmetric { r: f64 },
    /// [`windows::hann_periodic`].
    HannPeriodic,
    /// Not a windowed sinc: the Lanczos kernel with `a` lobes.
    Lanczos { a: usize },
    /// Not a windowed sinc: an optimal low-pass ([`super::optimal`]) with
    /// its band edges as fractions of the build grid's lower Nyquist (the
    /// sinc cutoff's scale), built by the general path only.
    Optimal {
        method: super::optimal::Method,
        pass: f64,
        stop: f64,
        /// Retry a length the exchange does not level 2 and 4 taps shorter
        /// (always, but the automatic search's bracket, which tries other
        /// lengths first).
        retry: bool,
    },
    /// Any other design window and sampling ([`windows::sample`]): the new
    /// combinations. Made by [`Window::other`] only.
    Other(crate::design::Window),
}

impl Window {
    /// [`Window::Other`] for `w`, when [`windows::sample`] samples it.
    pub(crate) fn other(w: crate::design::Window) -> Option<Self> {
        windows::sample(w, 1).map(|_| Self::Other(w))
    }
}

/// One kernel recipe at a given source rate and grid
/// ([`super::recipe`] evaluates a design into it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Recipe {
    /// Taps per input sample (per polyphase branch); the kernel has
    /// `ratio * taps` taps.
    pub taps: usize,
    /// -6 dB point as a fraction of the source Nyquist (unused by Lanczos).
    pub cutoff: f64,
    pub window: Window,
}

/// `sin(pi x) / (pi x)`, 1 at 0.
pub(crate) fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let y = PI * x;
        y.sin() / y
    }
}

/// The window samples for a kernel of `n` taps (not for Lanczos).
pub(crate) fn window(w: Window, n: usize) -> Vec<f64> {
    match w {
        Window::Gaussian { alpha } => windows::gaussian(n, alpha),
        Window::KaiserPeriodic { beta } => windows::kaiser_periodic(n, beta),
        Window::Flattop(c) => windows::flattop_periodic(n, c),
        Window::NuttallSymmetric => windows::nuttall_symmetric(n),
        Window::ChebyshevSymmetric { r } => windows::chebyshev_symmetric(n, r),
        Window::HannPeriodic => windows::hann_periodic(n),
        // `Window::other` checked that it is sampled.
        Window::Other(w) => windows::sample(w, n).unwrap_or_else(|| vec![1.0; n]),
        Window::Lanczos { .. } => unreachable!("Lanczos has no window"),
        Window::Optimal { .. } => unreachable!("an optimal design has no window"),
    }
}

/// The recipe's linear-phase kernel at the output rate, before
/// normalisation (length `ratio * r.taps`).
pub(crate) fn prototype(r: &Recipe, ratio: usize) -> Vec<f64> {
    prototype_len(r, ratio * r.taps, ratio)
}

/// [`prototype`] of `n` taps (any length) with the sinc divided by `ratio`.
pub(crate) fn prototype_len(r: &Recipe, n: usize, ratio: usize) -> Vec<f64> {
    let centre = (n / 2) as i64;
    match r.window {
        Window::Lanczos { a } => lanczos(n, centre, a),
        Window::Optimal { .. } => unreachable!("the general path builds optimal designs"),
        w => {
            // The window buffer becomes the kernel, so the design holds a
            // single full-length copy.
            let mut h = window(w, n);
            let l = ratio as f64;
            for (i, v) in h.iter_mut().enumerate() {
                let k = (i as i64 - centre) as f64;
                *v *= sinc(r.cutoff * k / l);
            }
            h
        }
    }
}

/// Lanczos kernel `sinc(x) sinc(x / a)` for `|x| < a`, with the argument
/// `x = (n - centre) a / centre`: the kernel spans exactly `+-a` over its
/// length, so its zero crossings are compressed by `a / (a + 1/2)` against
/// a plain one-per-input-sample step (effective cutoff about
/// `2a / (2a + 1)` of Nyquist, -13 dB at Nyquist, input samples not kept).
fn lanczos(n: usize, centre: i64, a: usize) -> Vec<f64> {
    let af = a as f64;
    let half = centre as f64;
    (0..n)
        .map(|i| {
            let x = ((i as i64 - centre) * a as i64) as f64 / half;
            if x.abs() < af {
                sinc(x) * sinc(x / af)
            } else {
                0.0
            }
        })
        .collect()
}

/// Pairwise sum of `x[start], x[start + step], ...` (blocks of up to 128
/// values summed with 8 running partials), which keeps the rounding error
/// of the million-tap kernels' branch sums at O(log n) ulps.
pub(crate) fn pairwise_sum(x: &[f64], start: usize, step: usize, count: usize) -> f64 {
    if count < 8 {
        let mut s = 0.0;
        for j in 0..count {
            s += x[start + j * step];
        }
        s
    } else if count <= 128 {
        let mut r = [0.0; 8];
        for (j, v) in r.iter_mut().enumerate() {
            *v = x[start + j * step];
        }
        let whole = count - count % 8;
        let mut i = 8;
        while i < whole {
            for (j, v) in r.iter_mut().enumerate() {
                *v += x[start + (i + j) * step];
            }
            i += 8;
        }
        let mut s = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        for j in whole..count {
            s += x[start + j * step];
        }
        s
    } else {
        let mut half = count / 2;
        half -= half % 8;
        pairwise_sum(x, start, step, half)
            + pairwise_sum(x, start + half * step, step, count - half)
    }
}

/// Scale `h` so that its normalised polyphase branch sums to exactly 1.
pub(crate) fn normalise(h: &mut [f64], ratio: usize) {
    let start = ratio / 2;
    if h.len() <= start {
        return;
    }
    let count = (h.len() - start).div_ceil(ratio);
    let sum = pairwise_sum(h, start, ratio, count);
    for v in h.iter_mut() {
        *v /= sum;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bit-exact against NumPy's pairwise summation (what the reference
    /// rebuild uses), on contiguous and strided slices across the 8-value,
    /// 128-value and recursive-split boundaries. A plain sequential sum
    /// differs in the last bits on every case from 128 values up.
    #[test]
    fn pairwise_sum_matches_numpy_bits() {
        const SCALE: [f64; 7] = [1e-3, 1e-2, 1e-1, 1.0, 1e1, 1e2, 1e3];
        let x: Vec<f64> = (0..10_007usize)
            .map(|i| (((i * 37) % 101) as f64).sin() * SCALE[i % 7])
            .collect();
        for (start, step, count, bits) in [
            (0, 1, 7, 0x408ed6ceef8cf897),
            (0, 1, 8, 0x408ed6cfd43a0377),
            (0, 1, 9, 0x408ed6cace665eea),
            (0, 1, 128, 0x409f45bc60d76284),
            (0, 1, 129, 0x409f4617ec6d6fac),
            (0, 1, 1000, 0x40b7f08b5915f3a0),
            (1, 3, 3336, 0x409897cd0b752b66),
            (4, 8, 1251, 0x407edd722f6b60c8),
            (0, 1, 10007, 0x40a75af16acc8df0),
        ] {
            let got = pairwise_sum(&x, start, step, count);
            assert_eq!(
                got.to_bits(),
                bits,
                "start {start} step {step} count {count}: {got:e} vs {:e}",
                f64::from_bits(bits)
            );
        }
    }
}
