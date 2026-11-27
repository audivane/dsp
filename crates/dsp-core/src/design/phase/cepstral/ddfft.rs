//! The magnitude spectrum of a real kernel, from a double-double FFT.
//!
//! The minimum phase is a Hilbert transform of `log|H|`, so it depends on
//! the deep stopband: a bin 260 dB below the peak must still come out with
//! a small *relative* error. An f64 FFT's absolute error (about 1e-16 of
//! the peak) swamps such bins; this one's (about 1e-31) does not. Only the
//! magnitudes need this: everything after the logarithm is well
//! conditioned in f64.

use crate::design::ddouble::Dd;

#[derive(Clone, Copy, Debug, Default)]
struct C {
    re: Dd,
    im: Dd,
}

impl C {
    #[inline(always)]
    fn add(self, b: C) -> C {
        C {
            re: self.re.add_fast(b.re),
            im: self.im.add_fast(b.im),
        }
    }

    #[inline(always)]
    fn sub(self, b: C) -> C {
        C {
            re: self.re.sub_fast(b.re),
            im: self.im.sub_fast(b.im),
        }
    }

    #[inline(always)]
    fn mul(self, b: C) -> C {
        C {
            re: (self.re * b.re).sub_fast(self.im * b.im),
            im: (self.re * b.im).add_fast(self.im * b.re),
        }
    }

    #[inline(always)]
    fn conj(self) -> C {
        C {
            re: self.re,
            im: -self.im,
        }
    }
}

/// `e^(-2 pi i k / m)` for `k` in `0 .. m/2`, from a quarter table.
struct Twiddles {
    quarter: Vec<C>,
}

impl Twiddles {
    /// `m` a power of two, at least 4.
    fn new(m: usize) -> Twiddles {
        let q = m / 4;
        let unit = |k: usize| {
            // e^(-i pi x) with x = 2k/m exact
            let (s, c) = Dd::new(2.0 * k as f64 / m as f64).sin_cos_pi();
            C { re: c, im: -s }
        };
        // Two-level table: every entry is one product of a coarse and a
        // fine root, each computed directly.
        let bits = q.trailing_zeros();
        let s = 1usize << (bits / 2);
        let fine: Vec<C> = (0..s).map(unit).collect();
        let coarse: Vec<C> = (0..q.div_ceil(s)).map(|j| unit(j * s)).collect();
        let quarter = (0..q).map(|k| coarse[k / s].mul(fine[k % s])).collect();
        Twiddles { quarter }
    }

    #[inline(always)]
    fn get(&self, k: usize) -> C {
        let q = self.quarter.len();
        if k < q {
            self.quarter[k]
        } else {
            // e^(-2 pi i (k' + m/4) / m) = -i e^(-2 pi i k' / m)
            let w = self.quarter[k - q];
            C {
                re: w.im,
                im: -w.re,
            }
        }
    }
}

/// `|H_k|`, `k = 0 ..= m/2`, of `h` zero-padded to `m` (a power of two,
/// at least `2 * h.len()` and at least 4).
pub fn magnitudes(h: &[f64], m: usize) -> Vec<f64> {
    assert!(m.is_power_of_two() && m >= 4 && h.len() <= m);
    let half = m / 2;
    let tw = Twiddles::new(m);

    // Pack the real sequence as half-length complex z_j = x_2j + i x_2j+1,
    // straight into bit-reversed order (the input is mostly zero padding).
    let bits = half.trailing_zeros();
    let mut a = vec![C::default(); half];
    for (j, pair) in h.chunks(2).enumerate() {
        let rev = if bits == 0 {
            0
        } else {
            j.reverse_bits() >> (usize::BITS - bits)
        };
        a[rev] = C {
            re: Dd::new(pair[0]),
            im: Dd::new(pair.get(1).copied().unwrap_or(0.0)),
        };
    }

    // Iterative radix-2 decimation in time.
    let mut len = 2;
    while len <= half {
        let step = m / len; // e^(-2 pi i j / len) = tw[j * step]
        let hl = len / 2;
        for block in a.chunks_exact_mut(len) {
            let (lo, hi) = block.split_at_mut(hl);
            for (j, (u, v)) in lo.iter_mut().zip(hi.iter_mut()).enumerate() {
                let t = if j == 0 { *v } else { v.mul(tw.get(j * step)) };
                let x = *u;
                *u = x.add(t);
                *v = x.sub(t);
            }
        }
        len *= 2;
    }

    // Unpack: X_k = E_k + e^(-2 pi i k / m) O_k with
    // E_k = (Z_k + conj Z_(half-k)) / 2, O_k = -i (Z_k - conj Z_(half-k)) / 2.
    (0..=half)
        .map(|k| {
            let zk = a[k % half];
            let zc = a[(half - k) % half].conj();
            let e = zk.add(zc);
            let d = zk.sub(zc);
            let o = C {
                re: d.im,
                im: -d.re,
            };
            let w = if k == half {
                C {
                    re: Dd::new(-1.0),
                    im: Dd::ZERO,
                }
            } else {
                tw.get(k)
            };
            let x = e.add(w.mul(o));
            // the factor 1/2 is exact
            (0.5 * x.re.to_f64()).hypot(0.5 * x.im.to_f64())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Direct DFT magnitude in double-double.
    fn direct(h: &[f64], m: usize) -> Vec<f64> {
        (0..=m / 2)
            .map(|k| {
                let mut re = Dd::ZERO;
                let mut im = Dd::ZERO;
                for (n, &x) in h.iter().enumerate() {
                    let idx = (k * n) % m;
                    let (s, c) = Dd::new(2.0 * idx as f64 / m as f64).sin_cos_pi();
                    re = re + c.mul_f64(x);
                    im = im - s.mul_f64(x);
                }
                re.to_f64().hypot(im.to_f64())
            })
            .collect()
    }

    #[test]
    fn matches_a_direct_dft() {
        let h: Vec<f64> = (0..37)
            .map(|i| ((i as f64 * 0.7).sin() + 0.3) * 1e-3f64.powi(i % 3))
            .collect();
        for m in [128usize, 256] {
            let a = magnitudes(&h, m);
            let b = direct(&h, m);
            for (k, (x, y)) in a.iter().zip(&b).enumerate() {
                assert!((x - y).abs() <= 4e-16 * y, "m {m} k {k}: {x} vs {y}");
            }
        }
    }
}
