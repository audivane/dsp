//! Double-double arithmetic: a value is `hi + lo` with `|lo| <= ulp(hi)/2`,
//! about 106 bits of mantissa. Used at design time only, where the phase
//! transform and the prototypes it starts from need more than f64.
//!
//! The elementary functions are accurate to a few units of 2^-104 over the
//! ranges the recipes use; none of them is fast, and none needs to be.

use std::ops::{Add, Div, Mul, Neg, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Dd {
    pub hi: f64,
    pub lo: f64,
}

/// `a + b` exactly, as `(sum, error)`.
#[inline(always)]
pub fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

/// `a + b` exactly when `|a| >= |b|`.
#[inline(always)]
fn quick_two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    (s, b - (s - a))
}

/// `a * b` exactly, as `(product, error)`. Without hardware FMA the
/// Veltkamp split needs `|a|, |b| < 2^996` (far beyond the kernels' range).
#[inline(always)]
pub fn two_prod(a: f64, b: f64) -> (f64, f64) {
    let p = a * b;
    #[cfg(target_feature = "fma")]
    {
        (p, a.mul_add(b, -p))
    }
    #[cfg(not(target_feature = "fma"))]
    {
        // Dekker's product with Veltkamp splitting.
        #[inline(always)]
        fn split(x: f64) -> (f64, f64) {
            let c = 134_217_729.0 * x; // 2^27 + 1
            let h = c - (c - x);
            (h, x - h)
        }
        let (ah, al) = split(a);
        let (bh, bl) = split(b);
        (p, ((ah * bh - p) + ah * bl + al * bh) + al * bl)
    }
}

impl Dd {
    pub const ZERO: Dd = Dd { hi: 0.0, lo: 0.0 };
    pub const ONE: Dd = Dd { hi: 1.0, lo: 0.0 };
    pub const PI: Dd = Dd {
        hi: std::f64::consts::PI,
        lo: 1.224_646_799_147_353_2e-16,
    };
    pub const LN2: Dd = Dd {
        hi: std::f64::consts::LN_2,
        lo: 2.319_046_813_846_299_6e-17,
    };

    #[inline(always)]
    pub const fn new(x: f64) -> Dd {
        Dd { hi: x, lo: 0.0 }
    }

    #[inline(always)]
    pub fn to_f64(self) -> f64 {
        self.hi + self.lo
    }

    /// Sum with a bound relative to `|a| + |b|` rather than `|a + b|`
    /// (cheaper; enough inside an FFT, whose error is bounded in norm).
    #[inline(always)]
    pub fn add_fast(self, b: Dd) -> Dd {
        let (s, e) = two_sum(self.hi, b.hi);
        let (hi, lo) = quick_two_sum(s, e + (self.lo + b.lo));
        Dd { hi, lo }
    }

    #[inline(always)]
    pub fn sub_fast(self, b: Dd) -> Dd {
        self.add_fast(-b)
    }

    #[inline(always)]
    pub fn mul_f64(self, b: f64) -> Dd {
        let (p, e) = two_prod(self.hi, b);
        let (hi, lo) = quick_two_sum(p, e + self.lo * b);
        Dd { hi, lo }
    }

    /// Multiply by a power of two (exact).
    #[inline(always)]
    pub fn scale(self, s: f64) -> Dd {
        Dd {
            hi: self.hi * s,
            lo: self.lo * s,
        }
    }

    pub fn div_f64(self, b: f64) -> Dd {
        let q1 = self.hi / b;
        let r = self - Dd::new(b).mul_f64(q1);
        let q2 = r.hi / b;
        let r = r - Dd::new(b).mul_f64(q2);
        let q3 = r.hi / b;
        let (hi, lo) = quick_two_sum(q1, q2);
        Dd { hi, lo } + Dd::new(q3)
    }

    pub fn sqr(self) -> Dd {
        self * self
    }

    /// The nearest integer (ties away from zero), as a double-double.
    pub fn round(self) -> Dd {
        let hi = self.hi.round();
        if hi == self.hi {
            // hi is an integer: round lo, then renormalise.
            let lo = self.lo.round();
            let (hi, lo) = quick_two_sum(hi, lo);
            Dd { hi, lo }
        } else if (hi - self.hi).abs() == 0.5 && self.lo != 0.0 {
            // a tie on hi broken by lo
            let hi = if (hi - self.hi).signum() == self.lo.signum() {
                hi
            } else {
                hi - (hi - self.hi).signum()
            };
            Dd::new(hi)
        } else {
            Dd::new(hi)
        }
    }

    /// Round to a 64-bit significand, nearest with ties to even: one x87
    /// extended-precision rounding step.
    ///
    /// For `|value| >= 2^-900` (the windows' range); smaller values are
    /// returned unchanged.
    pub fn round_x87(self) -> Dd {
        let v = self.hi;
        if v == 0.0 || !v.is_finite() {
            return Dd::new(v);
        }
        let bits = v.abs().to_bits();
        let mut e = ((bits >> 52) & 0x7ff) as i32 - 1023;
        if e < -900 {
            return self;
        }
        if bits & ((1u64 << 52) - 1) == 0 && self.lo != 0.0 && self.lo.signum() != v.signum() {
            e -= 1; // |hi| is a power of two and the value lies just below it
        }
        let q = f64::from_bits(((e - 63 + 1023) as u64) << 52); // 2^(e-63)
        let lo = (self.lo / q).round_ties_even() * q;
        let (hi, lo) = quick_two_sum(v, lo);
        Dd { hi, lo }
    }

    pub fn sqrt(self) -> Dd {
        if self.hi <= 0.0 {
            return Dd::ZERO;
        }
        // Karp's trick: one Newton step from the f64 root.
        let x = 1.0 / self.hi.sqrt();
        let ax = self.hi * x;
        let r = (self - Dd::new(ax).sqr()).hi * (x * 0.5);
        let (hi, lo) = two_sum(ax, r);
        Dd { hi, lo }
    }

    pub fn exp(self) -> Dd {
        if self.hi > 709.0 {
            return Dd::new(f64::INFINITY);
        }
        if self.hi < -745.0 {
            return Dd::ZERO;
        }
        // x = m ln2 + r, |r| <= ln2 / 2; e^r = (e^(r/512))^512.
        let m = (self.hi / std::f64::consts::LN_2).round();
        let r = (self - Dd::LN2.mul_f64(m)).scale(1.0 / 512.0);
        // e^r - 1 by Taylor (|r| < 7e-4: 10 terms reach 1e-36).
        let mut s = r;
        let mut t = r;
        for i in 2..=11 {
            t = (t * r).div_f64(i as f64);
            s = s + t;
            if t.hi.abs() < 1e-36 {
                break;
            }
        }
        // (e^r - 1) -> (e^2r - 1) = s (s + 2), nine times.
        for _ in 0..9 {
            s = s * (s + Dd::new(2.0));
        }
        let e = s + Dd::ONE;
        let p = 2f64.powi(m as i32);
        e.scale(p)
    }

    /// Natural logarithm (`self > 0`): one Newton step on `exp`.
    pub fn ln(self) -> Dd {
        let y = Dd::new(self.hi.ln());
        y + self * (-y).exp() - Dd::ONE
    }

    pub fn cosh(self) -> Dd {
        let e = self.exp();
        (e + Dd::ONE / e).scale(0.5)
    }

    /// `acosh(x)` for `x >= 1`.
    pub fn acosh(self) -> Dd {
        (self + (self.sqr() - Dd::ONE).sqrt()).ln()
    }

    /// `(sin(pi x), cos(pi x))`. The argument is reduced exactly (by
    /// halves), so zeros at integer `x` and the values at simple fractions
    /// come out exact or nearly so.
    pub fn sin_cos_pi(self) -> (Dd, Dd) {
        let q = self.scale(2.0).round(); // nearest multiple of 1/2
        let r = self - q.scale(0.5); // |r| <= 1/4, exact
        // quadrant from q mod 4 (q is an integer-valued double-double)
        let qm = (q.hi.rem_euclid(4.0) + q.lo.rem_euclid(4.0)).rem_euclid(4.0) as u32;
        let (s, c) = sin_cos_small(Dd::PI * r);
        match qm {
            0 => (s, c),
            1 => (c, -s),
            2 => (-s, -c),
            _ => (-c, s),
        }
    }

    pub fn sin_cos(self) -> (Dd, Dd) {
        (self / Dd::PI).sin_cos_pi()
    }

    /// `asin(x)` for `|x| <= 0.75`: Newton on `sin` from the f64 value.
    fn asin_small(self) -> Dd {
        let mut y = Dd::new(self.hi.asin());
        for _ in 0..2 {
            let (s, c) = y.sin_cos();
            y = y + (self - s) / c;
        }
        y
    }

    /// `acos(x)` for `0 <= x <= 1`.
    pub fn acos(self) -> Dd {
        if self.hi > 1.0 || (self.hi == 1.0 && self.lo >= 0.0) {
            return Dd::ZERO;
        }
        if self.hi > 0.5 {
            // acos x = 2 asin(sqrt((1 - x) / 2)), well conditioned near 1
            (Dd::ONE - self).scale(0.5).sqrt().asin_small().scale(2.0)
        } else {
            Dd::PI.scale(0.5) - self.asin_small()
        }
    }
}

/// `(sin t, cos t)` for `|t| <= pi/4` by Taylor series.
fn sin_cos_small(t: Dd) -> (Dd, Dd) {
    let mut s = t;
    let mut c = Dd::ONE;
    let mut term = t; // t^k / k!, k odd at the top of the loop
    let mut k = 1usize;
    while term.hi.abs() > 1e-36 && k < 60 {
        // t^(k+1) / (k+1)! goes to cos, t^(k+2) / (k+2)! to sin
        term = (term * t).div_f64((k + 1) as f64);
        c = if (k + 1) % 4 == 2 { c - term } else { c + term };
        term = (term * t).div_f64((k + 2) as f64);
        s = if (k + 2) % 4 == 3 { s - term } else { s + term };
        k += 2;
    }
    (s, c)
}

impl Neg for Dd {
    type Output = Dd;
    #[inline(always)]
    fn neg(self) -> Dd {
        Dd {
            hi: -self.hi,
            lo: -self.lo,
        }
    }
}

impl Add for Dd {
    type Output = Dd;
    /// IEEE-style sum, accurate relative to the result.
    #[inline(always)]
    fn add(self, b: Dd) -> Dd {
        let (s1, s2) = two_sum(self.hi, b.hi);
        let (t1, t2) = two_sum(self.lo, b.lo);
        let (s1, s2) = quick_two_sum(s1, s2 + t1);
        let (hi, lo) = quick_two_sum(s1, s2 + t2);
        Dd { hi, lo }
    }
}

impl Sub for Dd {
    type Output = Dd;
    #[inline(always)]
    fn sub(self, b: Dd) -> Dd {
        self + (-b)
    }
}

impl Mul for Dd {
    type Output = Dd;
    #[inline(always)]
    fn mul(self, b: Dd) -> Dd {
        let (p, e) = two_prod(self.hi, b.hi);
        let (hi, lo) = quick_two_sum(p, e + (self.hi * b.lo + self.lo * b.hi));
        Dd { hi, lo }
    }
}

impl Div for Dd {
    type Output = Dd;
    fn div(self, b: Dd) -> Dd {
        let q1 = self.hi / b.hi;
        let r = self - b.mul_f64(q1);
        let q2 = r.hi / b.hi;
        let r = r - b.mul_f64(q2);
        let q3 = r.hi / b.hi;
        let (hi, lo) = quick_two_sum(q1, q2);
        Dd { hi, lo } + Dd::new(q3)
    }
}

impl From<f64> for Dd {
    fn from(x: f64) -> Dd {
        Dd::new(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Dd, b: Dd, tol: f64) -> bool {
        let d = (a - b).to_f64().abs();
        d <= tol * b.to_f64().abs().max(1e-300)
    }

    #[test]
    fn constants_and_roots() {
        let two = Dd::new(2.0);
        let s = two.sqrt();
        assert!(close(s * s, two, 1e-31));
        // e, from its known double-double value
        let e = Dd {
            hi: std::f64::consts::E,
            lo: 1.445_646_891_729_250_2e-16,
        };
        assert!(close(Dd::ONE.exp(), e, 1e-31));
        assert!(close(two.ln(), Dd::LN2, 1e-31));
        assert!(close(Dd::new(1e-3).exp().ln(), Dd::new(1e-3), 1e-30));
    }

    #[test]
    fn sin_cos_pi_exact_points() {
        let (s, _) = Dd::new(1.0 / 6.0).sin_cos_pi();
        // 1/6 is not exact in f64, so sin is 1/2 only to first order
        assert!((s.to_f64() - 0.5).abs() < 1e-15);
        let (s, c2) = Dd::new(0.25).sin_cos_pi();
        assert!(close(s * s, Dd::new(0.5), 1e-31));
        assert!(close(c2 * c2, Dd::new(0.5), 1e-31));
        for k in [-7.0, -1.0, 0.0, 3.0, 1000.0] {
            let (s, _) = Dd::new(k).sin_cos_pi();
            assert_eq!(s.to_f64(), 0.0, "sin(pi {k})");
        }
        let (s, c) = Dd::new(123.456).sin_cos_pi();
        assert!(close(s.sqr() + c.sqr(), Dd::ONE, 1e-30));
        // sin(pi/3) = sqrt(3)/2: pi/3 as a double-double angle
        let (s, c) = (Dd::PI / Dd::new(3.0)).sin_cos();
        assert!(close(s, Dd::new(3.0).sqrt().scale(0.5), 1e-30));
        assert!(close(c, Dd::new(0.5), 1e-30));
    }

    #[test]
    fn inverse_functions() {
        for x in [0.1, 0.4999, 0.5, 0.75, 0.999_999, 1.0 - 1e-12] {
            let x = Dd::new(x);
            let y = x.acos();
            assert!(close(y.sin_cos().1, x, 1e-29), "acos {x:?}");
        }
        for x in [1.0001, 1.5, 4294967296.0] {
            let x = Dd::new(x);
            assert!(close(x.acosh().cosh(), x, 1e-29), "acosh {x:?}");
        }
        assert!(close(
            Dd::new(10.0) / Dd::new(3.0) * Dd::new(3.0),
            Dd::new(10.0),
            1e-31
        ));
        assert!(close(
            Dd::new(10.0).div_f64(3.0).mul_f64(3.0),
            Dd::new(10.0),
            1e-31
        ));
    }

    #[test]
    fn x87_rounding() {
        // pi to 64 bits is 0xC90FDAA22168C235 * 2^-62
        let pi = Dd::PI.round_x87();
        let m = (pi - Dd::new(3.0)).scale(2f64.powi(62));
        // the significand minus 3 * 2^62 is an exact integer
        assert_eq!(
            m.to_f64(),
            (0xC90F_DAA2_2168_C235u64 - 3 * (1u64 << 62)) as f64
        );
        // ties to even at 64 bits
        let one = Dd {
            hi: 1.0,
            lo: 2f64.powi(-64),
        };
        assert_eq!(one.round_x87(), Dd::ONE);
        let odd = Dd {
            hi: 1.0 + 2f64.powi(-52),
            lo: 2f64.powi(-64) + 2f64.powi(-63),
        };
        assert_eq!(odd.round_x87().lo, 2f64.powi(-62));
        // just below a power of two the step halves
        let below = Dd {
            hi: 2.0,
            lo: -2f64.powi(-70),
        };
        assert_eq!(below.round_x87(), Dd::new(2.0));
        let below = Dd {
            hi: 2.0,
            lo: -3.0 * 2f64.powi(-65),
        };
        assert_eq!(
            below.round_x87(),
            Dd {
                hi: 2.0,
                lo: -2f64.powi(-63)
            }
        );
    }

    #[test]
    fn rounding() {
        assert_eq!(Dd::new(2.5).round().hi, 3.0);
        assert_eq!(Dd::new(-2.5).round().hi, -3.0);
        let a = Dd {
            hi: 2.5,
            lo: -1e-20,
        };
        assert_eq!(a.round().hi, 2.0);
        let b = Dd { hi: 1e17, lo: 0.75 };
        assert_eq!(b.round(), Dd { hi: 1e17, lo: 1.0 });
    }
}
