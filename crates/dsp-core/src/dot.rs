//! Dot products for the polyphase runtimes, chosen once per runtime: AVX2 +
//! FMA when the CPU has them, portable code otherwise.
//!
//! - [`Kernels::dot1`] / [`Kernels::dot2`]: one coefficient row against one
//!   or two channels (the rational resampler, [`crate::rational`]).
//! - [`Kernels::dot4`] / [`Kernels::dot4x2`]: four adjacent coefficient
//!   rows against one or two channels, combined with four weights (the
//!   adaptive core's cubic interpolation between table phases,
//!   [`crate::adaptive`]). The four rows share the input loads.
//!
//! Every two-channel form sums each channel in exactly the order of its
//! one-channel form, so a stereo runtime's left channel equals the same
//! signal through a mono runtime, bit for bit. The portable and the SIMD
//! forms agree to rounding, not bit for bit.

pub(crate) type Dot1 = fn(&[f64], &[f64]) -> f64;
pub(crate) type Dot2 = fn(&[f64], &[f64], &[f64]) -> (f64, f64);
/// `(rows, taps, weights, x)`: `rows` holds four rows of `taps` coefficients.
pub(crate) type Dot4 = fn(&[f64], usize, &[f64; 4], &[f64]) -> f64;
pub(crate) type Dot4x2 = fn(&[f64], usize, &[f64; 4], &[f64], &[f64]) -> (f64, f64);

/// Dot products on the real-time path, chosen once per runtime.
#[derive(Clone, Copy)]
pub(crate) struct Kernels {
    pub(crate) dot1: Dot1,
    pub(crate) dot2: Dot2,
    pub(crate) dot4: Dot4,
    pub(crate) dot4x2: Dot4x2,
}

impl Kernels {
    pub(crate) fn detect() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("fma")
            {
                return Self {
                    dot1: simd::dot1_entry,
                    dot2: simd::dot2_entry,
                    dot4: simd::dot4_entry,
                    dot4x2: simd::dot4x2_entry,
                };
            }
        }
        Self::generic()
    }

    /// The portable forms, whatever the CPU.
    pub(crate) fn generic() -> Self {
        Self {
            dot1: dot1_generic,
            dot2: dot2_generic,
            dot4: dot4_generic,
            dot4x2: dot4x2_generic,
        }
    }
}

/// Eight running partial sums, combined pairwise at the end.
#[inline]
pub(crate) fn dot1_generic(h: &[f64], x: &[f64]) -> f64 {
    debug_assert_eq!(h.len(), x.len());
    let mut acc = [0.0f64; 8];
    let (hc, hr) = h.as_chunks::<8>();
    let (xc, xr) = x.as_chunks::<8>();
    for (a, b) in hc.iter().zip(xc) {
        for k in 0..8 {
            acc[k] += a[k] * b[k];
        }
    }
    let mut s = ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]));
    for (a, b) in hr.iter().zip(xr) {
        s += a * b;
    }
    s
}

/// [`dot1_generic`] for two channels, in exactly its order per channel, so
/// a stereo chain's left channel equals the same signal through a mono one.
#[inline]
pub(crate) fn dot2_generic(h: &[f64], x: &[f64], y: &[f64]) -> (f64, f64) {
    debug_assert!(h.len() == x.len() && h.len() == y.len());
    let mut ax = [0.0f64; 8];
    let mut ay = [0.0f64; 8];
    let (hc, hr) = h.as_chunks::<8>();
    let (xc, xr) = x.as_chunks::<8>();
    let (yc, yr) = y.as_chunks::<8>();
    for ((a, b), c) in hc.iter().zip(xc).zip(yc) {
        for k in 0..8 {
            ax[k] += a[k] * b[k];
            ay[k] += a[k] * c[k];
        }
    }
    let fold = |acc: &[f64; 8]| {
        ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]))
    };
    let (mut sx, mut sy) = (fold(&ax), fold(&ay));
    for ((a, b), c) in hr.iter().zip(xr).zip(yr) {
        sx += a * b;
        sy += a * c;
    }
    (sx, sy)
}

/// The four row sums combined with the weights, in one fixed order (shared
/// by every form, so the stereo forms match the mono ones).
#[inline(always)]
fn combine4(w: &[f64; 4], s: &[f64; 4]) -> f64 {
    w[0] * s[0] + w[1] * s[1] + w[2] * s[2] + w[3] * s[3]
}

/// One row against `x`: four running partial sums, then the tail.
#[inline(always)]
fn row_sum_generic(row: &[f64], x: &[f64]) -> f64 {
    let mut acc = [0.0f64; 4];
    let (hc, hr) = row.as_chunks::<4>();
    let (xc, xr) = x.as_chunks::<4>();
    for (a, b) in hc.iter().zip(xc) {
        for k in 0..4 {
            acc[k] += a[k] * b[k];
        }
    }
    let mut s = (acc[0] + acc[2]) + (acc[1] + acc[3]);
    for (a, b) in hr.iter().zip(xr) {
        s += a * b;
    }
    s
}

/// Four rows of `t` coefficients against one channel, then the weights.
pub(crate) fn dot4_generic(rows: &[f64], t: usize, w: &[f64; 4], x: &[f64]) -> f64 {
    debug_assert!(rows.len() == 4 * t && x.len() == t);
    let mut s = [0.0f64; 4];
    for (r, sr) in s.iter_mut().enumerate() {
        *sr = row_sum_generic(&rows[r * t..(r + 1) * t], x);
    }
    combine4(w, &s)
}

/// [`dot4_generic`] for two channels, in exactly its order per channel.
pub(crate) fn dot4x2_generic(
    rows: &[f64],
    t: usize,
    w: &[f64; 4],
    x: &[f64],
    y: &[f64],
) -> (f64, f64) {
    (dot4_generic(rows, t, w, x), dot4_generic(rows, t, w, y))
}

#[cfg(target_arch = "x86_64")]
mod simd {
    use std::arch::x86_64::*;

    pub(super) fn dot1_entry(h: &[f64], x: &[f64]) -> f64 {
        // SAFETY: only installed after AVX2 and FMA were detected.
        unsafe { dot1(h, x) }
    }

    pub(super) fn dot2_entry(h: &[f64], x: &[f64], y: &[f64]) -> (f64, f64) {
        // SAFETY: only installed after AVX2 and FMA were detected.
        unsafe { dot2(h, x, y) }
    }

    pub(super) fn dot4_entry(rows: &[f64], t: usize, w: &[f64; 4], x: &[f64]) -> f64 {
        assert!(rows.len() >= 4 * t && x.len() >= t);
        // SAFETY: only installed after AVX2 and FMA were detected; the
        // lengths are checked above.
        unsafe { dot4(rows, t, w, x) }
    }

    pub(super) fn dot4x2_entry(
        rows: &[f64],
        t: usize,
        w: &[f64; 4],
        x: &[f64],
        y: &[f64],
    ) -> (f64, f64) {
        assert!(rows.len() >= 4 * t && x.len() >= t && y.len() >= t);
        // SAFETY: as in `dot4_entry`.
        unsafe { dot4x2(rows, t, w, x, y) }
    }

    #[inline]
    #[target_feature(enable = "avx2,fma")]
    fn hsum(v: __m256d) -> f64 {
        let lo = _mm256_castpd256_pd128(v);
        let hi = _mm256_extractf128_pd::<1>(v);
        let s = _mm_add_pd(lo, hi);
        let t = _mm_unpackhi_pd(s, s);
        _mm_cvtsd_f64(_mm_add_sd(s, t))
    }

    /// Four 4-lane accumulators (16 partial sums).
    #[target_feature(enable = "avx2,fma")]
    fn dot1(h: &[f64], x: &[f64]) -> f64 {
        let n = h.len().min(x.len());
        let n16 = n - n % 16;
        let hp = h.as_ptr();
        let xp = x.as_ptr();
        let mut a0 = _mm256_setzero_pd();
        let mut a1 = _mm256_setzero_pd();
        let mut a2 = _mm256_setzero_pd();
        let mut a3 = _mm256_setzero_pd();
        let mut i = 0;
        while i < n16 {
            // SAFETY: i + 16 <= n, within both slices.
            unsafe {
                a0 = _mm256_fmadd_pd(_mm256_loadu_pd(hp.add(i)), _mm256_loadu_pd(xp.add(i)), a0);
                a1 = _mm256_fmadd_pd(
                    _mm256_loadu_pd(hp.add(i + 4)),
                    _mm256_loadu_pd(xp.add(i + 4)),
                    a1,
                );
                a2 = _mm256_fmadd_pd(
                    _mm256_loadu_pd(hp.add(i + 8)),
                    _mm256_loadu_pd(xp.add(i + 8)),
                    a2,
                );
                a3 = _mm256_fmadd_pd(
                    _mm256_loadu_pd(hp.add(i + 12)),
                    _mm256_loadu_pd(xp.add(i + 12)),
                    a3,
                );
            }
            i += 16;
        }
        let mut s = hsum(_mm256_add_pd(_mm256_add_pd(a0, a1), _mm256_add_pd(a2, a3)));
        while i < n {
            s = h[i].mul_add(x[i], s);
            i += 1;
        }
        s
    }

    /// Two channels against one branch: the coefficients are loaded once.
    /// Each channel is summed in exactly [`dot1`]'s order (four 4-lane
    /// accumulators, the same fold and tail), so a stereo chain's left
    /// channel equals the same signal through a mono chain.
    #[target_feature(enable = "avx2,fma")]
    fn dot2(h: &[f64], x: &[f64], y: &[f64]) -> (f64, f64) {
        let n = h.len().min(x.len()).min(y.len());
        let n16 = n - n % 16;
        let hp = h.as_ptr();
        let xp = x.as_ptr();
        let yp = y.as_ptr();
        let z = _mm256_setzero_pd();
        let (mut x0, mut x1, mut x2, mut x3) = (z, z, z, z);
        let (mut y0, mut y1, mut y2, mut y3) = (z, z, z, z);
        let mut i = 0;
        while i < n16 {
            // SAFETY: i + 16 <= n, within all three slices.
            unsafe {
                let h0 = _mm256_loadu_pd(hp.add(i));
                x0 = _mm256_fmadd_pd(h0, _mm256_loadu_pd(xp.add(i)), x0);
                y0 = _mm256_fmadd_pd(h0, _mm256_loadu_pd(yp.add(i)), y0);
                let h1 = _mm256_loadu_pd(hp.add(i + 4));
                x1 = _mm256_fmadd_pd(h1, _mm256_loadu_pd(xp.add(i + 4)), x1);
                y1 = _mm256_fmadd_pd(h1, _mm256_loadu_pd(yp.add(i + 4)), y1);
                let h2 = _mm256_loadu_pd(hp.add(i + 8));
                x2 = _mm256_fmadd_pd(h2, _mm256_loadu_pd(xp.add(i + 8)), x2);
                y2 = _mm256_fmadd_pd(h2, _mm256_loadu_pd(yp.add(i + 8)), y2);
                let h3 = _mm256_loadu_pd(hp.add(i + 12));
                x3 = _mm256_fmadd_pd(h3, _mm256_loadu_pd(xp.add(i + 12)), x3);
                y3 = _mm256_fmadd_pd(h3, _mm256_loadu_pd(yp.add(i + 12)), y3);
            }
            i += 16;
        }
        let mut sx = hsum(_mm256_add_pd(_mm256_add_pd(x0, x1), _mm256_add_pd(x2, x3)));
        let mut sy = hsum(_mm256_add_pd(_mm256_add_pd(y0, y1), _mm256_add_pd(y2, y3)));
        while i < n {
            sx = h[i].mul_add(x[i], sx);
            sy = h[i].mul_add(y[i], sy);
            i += 1;
        }
        (sx, sy)
    }

    /// Four rows against one channel: one 4-lane accumulator per row, the
    /// input loaded once per lane group and shared by the rows. Each row is
    /// folded, its tail added by FMA, then the weights applied.
    ///
    /// # Safety
    /// `rows.len() >= 4 t` and `x.len() >= t`.
    #[target_feature(enable = "avx2,fma")]
    unsafe fn dot4(rows: &[f64], t: usize, w: &[f64; 4], x: &[f64]) -> f64 {
        let n4 = t - t % 4;
        let rp = rows.as_ptr();
        let xp = x.as_ptr();
        let z = _mm256_setzero_pd();
        let mut a = [z; 4];
        let mut i = 0;
        while i < n4 {
            // SAFETY: i + 4 <= t, and every row r has t coefficients at r t.
            unsafe {
                let vx = _mm256_loadu_pd(xp.add(i));
                a[0] = _mm256_fmadd_pd(_mm256_loadu_pd(rp.add(i)), vx, a[0]);
                a[1] = _mm256_fmadd_pd(_mm256_loadu_pd(rp.add(t + i)), vx, a[1]);
                a[2] = _mm256_fmadd_pd(_mm256_loadu_pd(rp.add(2 * t + i)), vx, a[2]);
                a[3] = _mm256_fmadd_pd(_mm256_loadu_pd(rp.add(3 * t + i)), vx, a[3]);
            }
            i += 4;
        }
        let mut s = [hsum(a[0]), hsum(a[1]), hsum(a[2]), hsum(a[3])];
        for (r, sr) in s.iter_mut().enumerate() {
            for k in n4..t {
                *sr = rows[r * t + k].mul_add(x[k], *sr);
            }
        }
        super::combine4(w, &s)
    }

    /// [`dot4`] for two channels: each coefficient vector is loaded once
    /// for both. Each channel is summed in exactly [`dot4`]'s order.
    ///
    /// # Safety
    /// `rows.len() >= 4 t`, `x.len() >= t` and `y.len() >= t`.
    #[target_feature(enable = "avx2,fma")]
    unsafe fn dot4x2(rows: &[f64], t: usize, w: &[f64; 4], x: &[f64], y: &[f64]) -> (f64, f64) {
        let n4 = t - t % 4;
        let rp = rows.as_ptr();
        let xp = x.as_ptr();
        let yp = y.as_ptr();
        let z = _mm256_setzero_pd();
        let mut a = [z; 4];
        let mut b = [z; 4];
        let mut i = 0;
        while i < n4 {
            // SAFETY: as in `dot4`, and y has t frames too.
            unsafe {
                let vx = _mm256_loadu_pd(xp.add(i));
                let vy = _mm256_loadu_pd(yp.add(i));
                let h0 = _mm256_loadu_pd(rp.add(i));
                a[0] = _mm256_fmadd_pd(h0, vx, a[0]);
                b[0] = _mm256_fmadd_pd(h0, vy, b[0]);
                let h1 = _mm256_loadu_pd(rp.add(t + i));
                a[1] = _mm256_fmadd_pd(h1, vx, a[1]);
                b[1] = _mm256_fmadd_pd(h1, vy, b[1]);
                let h2 = _mm256_loadu_pd(rp.add(2 * t + i));
                a[2] = _mm256_fmadd_pd(h2, vx, a[2]);
                b[2] = _mm256_fmadd_pd(h2, vy, b[2]);
                let h3 = _mm256_loadu_pd(rp.add(3 * t + i));
                a[3] = _mm256_fmadd_pd(h3, vx, a[3]);
                b[3] = _mm256_fmadd_pd(h3, vy, b[3]);
            }
            i += 4;
        }
        let mut sx = [hsum(a[0]), hsum(a[1]), hsum(a[2]), hsum(a[3])];
        let mut sy = [hsum(b[0]), hsum(b[1]), hsum(b[2]), hsum(b[3])];
        for r in 0..4 {
            for k in n4..t {
                let h = rows[r * t + k];
                sx[r] = h.mul_add(x[k], sx[r]);
                sy[r] = h.mul_add(y[k], sy[r]);
            }
        }
        (super::combine4(w, &sx), super::combine4(w, &sy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
    }

    #[test]
    fn simd_and_generic_dots_agree() {
        let mut seed = 7;
        for n in [0usize, 1, 3, 4, 7, 8, 15, 16, 17, 33, 4095] {
            let h: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
            let x: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
            let y: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
            let k = Kernels::detect();
            let exact: f64 = h.iter().zip(&x).map(|(a, b)| a * b).sum();
            assert!(((k.dot1)(&h, &x) - exact).abs() < 1e-12);
            assert!((dot1_generic(&h, &x) - exact).abs() < 1e-12);
            let (a, b) = (k.dot2)(&h, &x, &y);
            let (c, d) = dot2_generic(&h, &x, &y);
            assert!((a - c).abs() < 1e-12 && (b - d).abs() < 1e-12);
            assert!((a - exact).abs() < 1e-12);
            // Each channel of the fused stereo path is summed exactly like
            // the mono one, on either implementation.
            assert_eq!(a.to_bits(), (k.dot1)(&h, &x).to_bits(), "n {n}");
            assert_eq!(b.to_bits(), (k.dot1)(&h, &y).to_bits(), "n {n}");
            assert_eq!(c.to_bits(), dot1_generic(&h, &x).to_bits(), "n {n}");
            assert_eq!(d.to_bits(), dot1_generic(&h, &y).to_bits(), "n {n}");
        }
    }

    #[test]
    fn four_row_dots_agree_and_stereo_matches_mono() {
        let mut seed = 19;
        for t in [1usize, 3, 4, 5, 8, 24, 25, 33, 232] {
            let rows: Vec<f64> = (0..4 * t).map(|_| lcg(&mut seed)).collect();
            let x: Vec<f64> = (0..t).map(|_| lcg(&mut seed)).collect();
            let y: Vec<f64> = (0..t).map(|_| lcg(&mut seed)).collect();
            let w = [
                lcg(&mut seed),
                lcg(&mut seed),
                lcg(&mut seed),
                lcg(&mut seed),
            ];
            let exact = |v: &[f64]| -> f64 {
                (0..4)
                    .map(|r| w[r] * (0..t).map(|k| rows[r * t + k] * v[k]).sum::<f64>())
                    .sum()
            };
            for k in [Kernels::detect(), Kernels::generic()] {
                let a = (k.dot4)(&rows, t, &w, &x);
                let (b, c) = (k.dot4x2)(&rows, t, &w, &x, &y);
                assert!((a - exact(&x)).abs() < 1e-12, "t {t}");
                assert!((c - exact(&y)).abs() < 1e-12, "t {t}");
                assert_eq!(a.to_bits(), b.to_bits(), "t {t}");
                assert_eq!(c.to_bits(), (k.dot4)(&rows, t, &w, &y).to_bits(), "t {t}");
            }
        }
    }
}
