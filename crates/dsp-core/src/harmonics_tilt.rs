//! Per-order post-filters of a Chebyshev harmonics layer ("tilt").
//!
//! Each order `k` of a tilted layer sends its distortion term `D_k` (no
//! offset, no linear part) through its own filter `G_k`: a low shelf and a
//! high shelf, with corners at `k ×` the configured fundamental-frequency
//! corners, scaled to unity at `k × 1 kHz`. Harmonic `k` of a sine at `f`
//! then sits at `h_k + 20 log10 |G_k(k f)|` relative to the input level, and
//! every order follows the same curve against the fundamental's frequency.
//!
//! A shelf of order `n` (slope `6n` dB/oct: 1..=4; by default 2 for the low
//! shelf and 1 for the high one) is a Butterworth-style shelf, the ratio of
//! two Butterworth polynomials of order `n` at the near corner `c` (the one
//! next to the unity band) and the far corner `c / ρ` (low shelf) or `c ρ`
//! (high shelf), `ρ = 10^{|db|/(20 n)}`:
//!
//! ```text
//! |S(jω)|² ∝ (1 + (ω/z)^{2n}) / (1 + (ω/p)^{2n})
//! ```
//!
//! (zeros at the near corner and poles at the far one for a rise, the
//! other way round for a cut). It factors into a first-order section when
//! `n` is odd and `⌊n/2⌋` second-order sections with damping `2 cos θ_i`
//! (the Butterworth pole angles), each section taking the same share
//! `ρ` of the shelf's ratio, so the full rise is `db` whatever `n`. In
//! log frequency the order-`n` shelf is the first-order curve squeezed `n`
//! times toward the near corner (`ω ↦ c (ω/c)^n`), so `6n` dB/oct is its
//! asymptote, reached only by a large rise: its steepest slope is `6.02 n
//! (R − 1)/(R + 1)` dB/oct at the corners' geometric middle, `R =
//! 10^{|db|/20}` (0.88 of `6n` at ±24 dB, 0.60 at ±12, 0.33 at ±6; a +6 dB
//! shelf at 18 dB/oct is never steeper than 6 dB/oct, it just rises over a
//! third of the octaves). The defaults are a low shelf at 12 dB/oct and a
//! high shelf at 6.
//!
//! Every section is a bilinear transform of its analog prototype with the
//! near corner prewarped and the far one at the exact ratio from it, so
//! each shelf's full rise is exactly its `db` (a capped corner just below
//! Nyquist never inflates it).
//!
//! [`TiltFilter::l1_norm`] is a certified upper bound of `Σ |g[n]|`: the head
//! of the impulse response summed, plus a closed-form bound of the tail from
//! every section's poles. The stage bound uses it (see `crate::harmonics`).

use crate::Complex64;
use audivane_protocol::{OrderTilt, TILT_HIGH_DEFAULT_SLOPE, TILT_LOW_DEFAULT_SLOPE, TiltShelf};
use std::f64::consts::{PI, SQRT_2};

/// Highest prewarped corner, as a fraction of the sample rate.
const MAX_CORNER: f64 = 0.4995;
/// The fundamental frequency every order is unity at, Hz.
pub(crate) const UNITY_HZ: f64 = 1_000.0;
/// Target of the certified tail bound, relative to the filter's gain.
const TAIL_TOL: f64 = 1e-9;
/// Most impulse-response samples summed for one L1 norm (the tail bound
/// covers the rest, however loose).
const MAX_HEAD: usize = 20_000_000;
/// Filter states below this are flushed to zero (no subnormals after
/// silence), as in `crate::flux`.
const FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31;
/// Damping `2 cos θ` of the fourth-order Butterworth sections:
/// `√(2 + √2)` and `√(2 − √2)` (θ = π/8, 3π/8).
const BW4: [f64; 2] = [1.847_759_065_022_573_5, 0.765_366_864_730_179_6];

/// `H(z) = (b0 + b1 z⁻¹ + b2 z⁻²) / (1 + a1 z⁻¹ + a2 z⁻²)`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

/// `H(z) = (b0 + b1 z⁻¹) / (1 + a1 z⁻¹)`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FirstOrder {
    b0: f64,
    b1: f64,
    a1: f64,
}

/// Per-channel state of one [`TiltFilter`] (TDF-II): up to four values per
/// shelf (an order-4 shelf's two biquads; an order-3 shelf's first-order
/// section and biquad).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TiltState {
    low: [f64; 4],
    high: [f64; 4],
}

impl TiltState {
    /// Flush a decaying tail to zero before it reaches subnormals. Once per
    /// block is enough: a state that is still above the threshold at the
    /// end of one block cannot fall from there into subnormals in the next
    /// unless its pole is fast, and then it reaches zero within a few
    /// samples anyway.
    #[inline]
    pub(crate) fn flush(&mut self) {
        for v in self.low.iter_mut().chain(self.high.iter_mut()) {
            if v.abs() < FLUSH_BELOW {
                *v = 0.0;
            }
        }
    }
}

/// One shelf as a cascade, by order, run first to last: a first-order
/// section for odd orders, then the biquads (an enum keeps the run to one
/// dispatch per shelf).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shelf {
    O1(FirstOrder),
    O2(Biquad),
    O3(FirstOrder, Biquad),
    O4(Biquad, Biquad),
}

/// One section of a cascade, in run order.
#[derive(Clone, Copy)]
enum Section<'a> {
    First(&'a FirstOrder),
    Quad(&'a Biquad),
}

/// One order's filter: low shelf then high shelf, either of which may be
/// absent (never both).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TiltFilter {
    low: Option<Shelf>,
    high: Option<Shelf>,
}

/// Prewarped analog frequency `tan(π f / fs)` (bilinear `s = (1 − z⁻¹)/(1 + z⁻¹)`).
fn warp(hz: f64, rate: f64) -> f64 {
    (PI * (hz / rate).min(MAX_CORNER)).tan()
}

/// `e^{−j w}` powers for a response at digital frequency `w`.
fn z1(w: f64) -> Complex64 {
    Complex64::from_polar(1.0, -w)
}

impl Biquad {
    /// `(s² + c z s + z²) / (s² + c p s + p²)` through the bilinear map.
    fn from_analog(z: f64, p: f64, c: f64) -> Self {
        let n = [
            1.0 + c * z + z * z,
            2.0 * (z * z - 1.0),
            1.0 - c * z + z * z,
        ];
        let d = [
            1.0 + c * p + p * p,
            2.0 * (p * p - 1.0),
            1.0 - c * p + p * p,
        ];
        Self {
            b0: n[0] / d[0],
            b1: n[1] / d[0],
            b2: n[2] / d[0],
            a1: d[1] / d[0],
            a2: d[2] / d[0],
        }
    }

    fn response(&self, w: f64) -> Complex64 {
        let u = z1(w);
        let u2 = u * u;
        (self.b0 + self.b1 * u + self.b2 * u2) / (1.0 + self.a1 * u + self.a2 * u2)
    }

    fn dc(&self) -> f64 {
        (self.b0 + self.b1 + self.b2) / (1.0 + self.a1 + self.a2)
    }

    fn scaled(self, g: f64) -> Self {
        Self {
            b0: self.b0 * g,
            b1: self.b1 * g,
            b2: self.b2 * g,
            ..self
        }
    }

    /// `(N, C)`: from sample `N` on, `Σ_{n≥N} |h[n]| ≤ C`, with `C` at most
    /// `tol` (unless `N` hit [`MAX_HEAD`]). `h[n] = Σ A_i p_i^n` for `n ≥ 1`.
    fn tail(&self, tol: f64) -> (usize, f64) {
        // h[1], h[2] fix the residues.
        let h1 = self.b1 - self.a1 * self.b0;
        let h2 = self.b2 - self.a1 * h1 - self.a2 * self.b0;
        let disc = self.a1 * self.a1 - 4.0 * self.a2;
        // Terms `c_i r_i^n`: the tail from N is Σ c_i r_i^N / (1 − r_i).
        let terms: Vec<(f64, f64)> = if disc < 0.0 {
            // h[n] = 2 Re(α p^n), p = r e^{jθ}.
            let (pr, pi) = (-0.5 * self.a1, 0.5 * (-disc).sqrt());
            let r2 = pr * pr + pi * pi;
            let (qr, qi) = (pr * pr - pi * pi, 2.0 * pr * pi);
            // [pr −pi; qr −qi] [x; y] = [h1/2; h2/2].
            let det = -pr * qi + pi * qr;
            let x = (-(0.5 * h1) * qi + pi * (0.5 * h2)) / det;
            let y = (pr * (0.5 * h2) - qr * (0.5 * h1)) / det;
            vec![(2.0 * x.hypot(y), r2.sqrt())]
        } else {
            let s = disc.sqrt();
            let (p1, p2) = (0.5 * (-self.a1 + s), 0.5 * (-self.a1 - s));
            if (p1 - p2).abs() > 1e-6 * p1.abs().max(p2.abs()).max(1e-300) {
                // h1 = A1 p1 + A2 p2, h2 = A1 p1² + A2 p2².
                let det = p1 * p2 * (p2 - p1);
                let a1 = (h1 * p2 * p2 - h2 * p2) / det;
                let a2 = (h2 * p1 - h1 * p1 * p1) / det;
                vec![(a1.abs(), p1.abs()), (a2.abs(), p2.abs())]
            } else {
                // A (near) double pole: |h[n]| <= (|h1| + |h2|) n r^n is not
                // needed for shelves (their poles are complex); be safe.
                let r = p1.abs().max(p2.abs());
                return (
                    0,
                    (self.b0.abs() + self.b1.abs() + self.b2.abs()) / ((1.0 - r) * (1.0 - r)),
                );
            }
        };
        let bound = |n: usize| -> f64 {
            terms
                .iter()
                .map(|&(c, r)| c * r.powi(n as i32) / (1.0 - r))
                .sum()
        };
        let r_max = terms.iter().fold(0.0_f64, |m, &(_, r)| m.max(r));
        let c_sum: f64 = terms.iter().map(|&(c, r)| c / (1.0 - r)).sum();
        let mut n = if c_sum <= tol || r_max <= 0.0 {
            1
        } else {
            ((tol / c_sum).ln() / r_max.ln()).ceil().max(1.0) as usize
        };
        n = n.clamp(1, MAX_HEAD);
        // `powi` takes an i32: N stays far below.
        (n, bound(n))
    }

    #[inline(always)]
    fn tick(&self, x: f64, s1: &mut f64, s2: &mut f64) -> f64 {
        let y = self.b0 * x + *s1;
        *s1 = self.b1 * x - self.a1 * y + *s2;
        *s2 = self.b2 * x - self.a2 * y;
        y
    }
}

impl FirstOrder {
    /// `g (s + z) / (s + p)` through the bilinear map.
    fn from_analog(g: f64, z: f64, p: f64) -> Self {
        Self {
            b0: g * (1.0 + z) / (1.0 + p),
            b1: g * (z - 1.0) / (1.0 + p),
            a1: (p - 1.0) / (1.0 + p),
        }
    }

    fn response(&self, w: f64) -> Complex64 {
        let u = z1(w);
        (self.b0 + self.b1 * u) / (1.0 + self.a1 * u)
    }

    fn dc(&self) -> f64 {
        (self.b0 + self.b1) / (1.0 + self.a1)
    }

    fn scaled(self, g: f64) -> Self {
        Self {
            b0: self.b0 * g,
            b1: self.b1 * g,
            ..self
        }
    }

    /// Exact `Σ |h[n]|`: `h[0] = b0`, `h[n] = (b1 − a1 b0)(−a1)^{n−1}`.
    fn l1_norm(&self) -> f64 {
        self.b0.abs() + (self.b1 - self.a1 * self.b0).abs() / (1.0 - self.a1.abs())
    }

    /// `(N, C)` as [`Biquad::tail`].
    fn tail(&self, tol: f64) -> (usize, f64) {
        let c = (self.b1 - self.a1 * self.b0).abs();
        let r = self.a1.abs();
        let bound = |n: usize| c * r.powi(n as i32 - 1) / (1.0 - r);
        if c == 0.0 || r == 0.0 {
            return (1, bound(1));
        }
        let n = (((tol * (1.0 - r) / c).ln() / r.ln()).ceil() + 1.0).clamp(1.0, MAX_HEAD as f64)
            as usize;
        (n, bound(n))
    }

    #[inline(always)]
    fn tick(&self, x: f64, z1: &mut f64) -> f64 {
        let y = self.b0 * x + *z1;
        *z1 = self.b1 * x - self.a1 * y;
        y
    }
}

/// Damping `2 cos θ` of the biquads of a Butterworth polynomial of order
/// `n` (1..=4; odd orders add a first-order factor).
fn butterworth(n: usize) -> &'static [f64] {
    match n {
        1 => &[],
        2 => &[SQRT_2],
        3 => &[1.0],
        _ => &BW4,
    }
}

impl Shelf {
    /// The order-`n` Butterworth shelf with zeros at `z` and poles at `p`
    /// (prewarped analog corners), every section at HF gain 1 (a low shelf)
    /// or, with `dc_unity`, at DC gain 1 (a high shelf: a first-order
    /// section times `p/z`, a biquad times `(p/z)²`).
    fn butterworth(z: f64, p: f64, n: usize, dc_unity: bool) -> Self {
        let damping = butterworth(n);
        // The share of one first-order factor: p/z (DC 1) or 1 (HF 1).
        let r = if dc_unity { p / z } else { 1.0 };
        let first = || FirstOrder::from_analog(r, z, p);
        let quad = |i: usize| {
            let b = Biquad::from_analog(z, p, damping[i]);
            if dc_unity { b.scaled(r * r) } else { b }
        };
        match n {
            1 => Self::O1(first()),
            2 => Self::O2(quad(0)),
            3 => Self::O3(first(), quad(0)),
            _ => Self::O4(quad(0), quad(1)),
        }
    }

    /// The sections in run order.
    fn sections(&self) -> impl Iterator<Item = Section<'_>> {
        use Section::{First, Quad};
        match self {
            Self::O1(f) => [Some(First(f)), None],
            Self::O2(b) => [Some(Quad(b)), None],
            Self::O3(f, b) => [Some(First(f)), Some(Quad(b))],
            Self::O4(a, b) => [Some(Quad(a)), Some(Quad(b))],
        }
        .into_iter()
        .flatten()
    }

    /// The shelf times `g` (its first section scaled).
    fn scaled(self, g: f64) -> Self {
        match self {
            Self::O1(f) => Self::O1(f.scaled(g)),
            Self::O2(b) => Self::O2(b.scaled(g)),
            Self::O3(f, b) => Self::O3(f.scaled(g), b),
            Self::O4(a, b) => Self::O4(a.scaled(g), b),
        }
    }

    #[inline(always)]
    fn tick(&self, x: f64, st: &mut [f64; 4]) -> f64 {
        let [s0, s1, s2, s3] = st;
        // Plain branches on the default orders first (cheaper than a jump
        // table when terms of mixed orders alternate).
        if let Self::O2(b) = self {
            return b.tick(x, s0, s1);
        }
        if let Self::O1(f) = self {
            return f.tick(x, s0);
        }
        match self {
            Self::O3(f, b) => b.tick(f.tick(x, s2), s0, s1),
            Self::O4(a, b) => b.tick(a.tick(x, s0, s1), s2, s3),
            Self::O1(f) => f.tick(x, s0),
            Self::O2(b) => b.tick(x, s0, s1),
        }
    }
}

impl Section<'_> {
    fn response(&self, w: f64) -> Complex64 {
        match self {
            Section::First(f) => f.response(w),
            Section::Quad(b) => b.response(w),
        }
    }

    fn dc(&self) -> f64 {
        match self {
            Section::First(f) => f.dc(),
            Section::Quad(b) => b.dc(),
        }
    }

    fn tail(&self, tol: f64) -> (usize, f64) {
        match self {
            Section::First(f) => f.tail(tol),
            Section::Quad(b) => b.tail(tol),
        }
    }

    /// An upper bound of `Σ |h[n]|` given its tail `(n, t)` from
    /// [`Self::tail`]: exact for a first-order section, else the first `n`
    /// samples summed plus `t`.
    fn l1_norm(&self, (n, t): (usize, f64)) -> f64 {
        match self {
            Section::First(f) => f.l1_norm(),
            Section::Quad(b) => {
                let (mut s1, mut s2) = (0.0, 0.0);
                let mut head = 0.0;
                for i in 0..n {
                    head += b
                        .tick(if i == 0 { 1.0 } else { 0.0 }, &mut s1, &mut s2)
                        .abs();
                }
                head + t
            }
        }
    }
}

/// The low shelf of order `k` (`None` at 0 dB): `slope` dB/oct below
/// `k × hz`, reaching `db` (HF gain 1).
fn low_shelf(s: &TiltShelf, k: usize, rate: f64) -> Option<Shelf> {
    if s.db == 0.0 {
        return None;
    }
    let n = s.order(TILT_LOW_DEFAULT_SLOPE);
    let near = warp(k as f64 * s.hz, rate);
    let far = near / f64::powf(10.0, s.db.abs() / (20.0 * n as f64));
    Some(if s.db > 0.0 {
        Shelf::butterworth(near, far, n, false)
    } else {
        Shelf::butterworth(far, near, n, false)
    })
}

/// The high shelf of order `k` (`None` at 0 dB): `slope` dB/oct above
/// `k × hz`, reaching `db` (DC gain 1).
fn high_shelf(s: &TiltShelf, k: usize, rate: f64) -> Option<Shelf> {
    if s.db == 0.0 {
        return None;
    }
    let n = s.order(TILT_HIGH_DEFAULT_SLOPE);
    let near = warp(k as f64 * s.hz, rate);
    let far = near * f64::powf(10.0, s.db.abs() / (20.0 * n as f64));
    Some(if s.db > 0.0 {
        Shelf::butterworth(near, far, n, true)
    } else {
        Shelf::butterworth(far, near, n, true)
    })
}

impl TiltFilter {
    /// Order `k`'s filter at `rate`, unity at `k × 1 kHz`; `None` when both
    /// shelves are at 0 dB (the order stays on the unfiltered path).
    pub(crate) fn new(shelves: &OrderTilt, k: usize, rate: f64) -> Option<Self> {
        let f = Self {
            low: low_shelf(&shelves.low, k, rate),
            high: high_shelf(&shelves.high, k, rate),
        };
        if f.low.is_none() && f.high.is_none() {
            return None;
        }
        let g = 1.0 / f.response(2.0 * PI * k as f64 * UNITY_HZ / rate).norm();
        Some(f.scaled(g))
    }

    /// The filter times `g` (its first section scaled).
    pub(crate) fn scaled(self, g: f64) -> Self {
        match self.low {
            Some(s) => Self {
                low: Some(s.scaled(g)),
                ..self
            },
            None => Self {
                high: self.high.map(|s| s.scaled(g)),
                ..self
            },
        }
    }

    /// The sections in run order.
    fn sections(&self) -> impl Iterator<Item = Section<'_>> {
        self.low
            .iter()
            .chain(self.high.iter())
            .flat_map(Shelf::sections)
    }

    /// Complex response at digital frequency `w` (rad/sample): the product
    /// of the sections'.
    pub(crate) fn response(&self, w: f64) -> Complex64 {
        let mut h = Complex64::new(1.0, 0.0);
        for s in self.sections() {
            h *= s.response(w);
        }
        h
    }

    /// Gain at DC, `Σ g[n]`.
    pub(crate) fn dc_gain(&self) -> f64 {
        self.sections().fold(1.0, |g, s| g * s.dc())
    }

    /// Certified upper bound of `Σ |g[n]|`.
    ///
    /// The filter is a cascade `g = h_1 * … * h_M` of first- and
    /// second-order sections. For each, [`Biquad::tail`] or
    /// [`FirstOrder::tail`] give `N_i` and a closed-form `T_i ≥ Σ_{m≥N_i}
    /// |h_i[m]|` from its poles (a first-order section's tail is a
    /// geometric series; a biquad's `h[m] = Σ A_j p_j^m` for `m ≥ 1`, so
    /// `Σ_{m≥N} |h[m]| ≤ Σ |A_j| r_j^N / (1 − r_j)`), and `L_i ≥ ‖h_i‖₁`
    /// (exact for a first-order section, else its first `N_i` samples
    /// summed plus `T_i`). Let `N = Σ N_i − (M − 1)`. Then
    ///
    /// ```text
    /// ‖g‖₁ ≤ Σ_{n<N} |g[n]| + Σ_i T_i Π_{j≠i} L_j.
    /// ```
    ///
    /// Proof: `g[n] = Σ_{n_1+…+n_M=n} Π_i h_i[n_i]`. For `n ≥ N` every such
    /// index tuple has some `n_i ≥ N_i` (else `n = Σ n_i ≤ Σ (N_i − 1) =
    /// N − 1`), so `Σ_{n≥N} |g[n]|` is at most the sum of `Π |h_i[n_i]|`
    /// over all tuples with `n_1 ≥ N_1`, plus those with `n_2 ≥ N_2`, …;
    /// the `i`-th of these sums factors into `T_i Π_{j≠i} ‖h_j‖₁`. The head
    /// is summed from the cascade itself. A lone first-order section is
    /// summed exactly.
    pub(crate) fn l1_norm(&self) -> f64 {
        self.cascade_l1(None)
    }

    /// Certified upper bound of `Σ |(g * h)[n]|`, `h` the first-order
    /// section `(b0 + b1 z⁻¹) / (1 + a1 z⁻¹)` run after the filter (the
    /// even terms' DC blocker): [`Self::l1_norm`] with `h` as one more
    /// section of the cascade (the same proof).
    pub(crate) fn l1_norm_then(&self, (b0, b1, a1): (f64, f64, f64)) -> f64 {
        self.cascade_l1(Some(&FirstOrder { b0, b1, a1 }))
    }

    fn cascade_l1(&self, extra: Option<&FirstOrder>) -> f64 {
        let tol = TAIL_TOL * self.dc_gain().abs().max(1.0);
        let all = || self.sections().chain(extra.map(Section::First));
        let mut sections = all();
        let (Some(s0), rest) = (sections.next(), sections.next()) else {
            return 1.0;
        };
        if let (Section::First(f), None) = (s0, rest) {
            return f.l1_norm() * (1.0 + 1e-12);
        }
        // Up to seven sections: (tail N_i, T_i, L_i).
        debug_assert!(all().count() <= 7, "more sections than the L1 proof holds");
        let mut parts = [(0usize, 0.0, 0.0); 7];
        let mut m = 0;
        for (p, s) in parts.iter_mut().zip(all()) {
            let tail = s.tail(tol);
            *p = (tail.0, tail.1, s.l1_norm(tail));
            m += 1;
        }
        let parts = &parts[..m];
        let n = (parts.iter().map(|p| p.0).sum::<usize>() + 1).saturating_sub(m);
        let mut st = TiltState::default();
        let mut z = 0.0;
        let mut head = 0.0;
        for i in 0..n {
            let y = self.tick(&mut st, if i == 0 { 1.0 } else { 0.0 });
            head += match extra {
                Some(h) => h.tick(y, &mut z),
                None => y,
            }
            .abs();
        }
        let mut total = head;
        for (i, &(_, t, _)) in parts.iter().enumerate() {
            let others = parts
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .fold(1.0, |acc, (_, p)| acc * p.2);
            total += t * others;
        }
        // Rounding in the sums (relative, far below this margin).
        total * (1.0 + 1e-9)
    }

    /// One sample (call [`TiltState::flush`] once per block).
    #[inline(always)]
    pub(crate) fn tick(&self, st: &mut TiltState, x: f64) -> f64 {
        // The default slopes first (the presets' filters).
        if let (Some(Shelf::O2(b)), Some(Shelf::O1(f))) = (&self.low, &self.high) {
            let [s0, s1, ..] = &mut st.low;
            let y = b.tick(x, s0, s1);
            return f.tick(y, &mut st.high[0]);
        }
        let mut y = x;
        if let Some(s) = &self.low {
            y = s.tick(y, &mut st.low);
        }
        if let Some(s) = &self.high {
            y = s.tick(y, &mut st.high);
        }
        y
    }
}

// ------------------------------------------------------------ joint bound

/// Highest Chebyshev order of the joint bound.
const KMAX: usize = audivane_protocol::HARMONICS_MAX_ORDER;
/// Points of the joint bound's grid in `θ` (`u = cos θ`, `θ ∈ [0, π]`).
const JOINT_GRID: usize = 129;
/// The joint bound stops at the first lag where the certified rest of the
/// sum is below this share of the bound.
const JOINT_REST: f64 = 1e-5;
/// Most lags the joint bound sums one by one (the certified rest covers the
/// others, however loose).
const JOINT_MAX_LAGS: usize = 1 << 23;

/// One term of the bank for [`JointBound`]: `weight · D_k` through `filter`
/// (`None`: unfiltered), then through the DC blocker when `dc_block`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JointTerm {
    pub(crate) k: usize,
    pub(crate) weight: f64,
    pub(crate) filter: Option<TiltFilter>,
    pub(crate) dc_block: bool,
}

/// The joint per-lag bound of a Chebyshev bank with filtered terms.
///
/// Every part of the bank is linear and time-invariant after its
/// memoryless term `D_k(x_c)`: with `h_t` term `t`'s impulse response (its
/// tilt filter, then the DC blocker if it is even and the blocker is on;
/// `δ` for an unfiltered odd term), the output before the stage scale is
///
/// ```text
/// y[n] = g x_c[n] + Σ_{m≥0} F_m(x_c[n − m]),   F_m(u) = Σ_t w_t h_t[m] D_{k_t}(u),
/// ```
///
/// `g` the direct gain and `x_c[n] ∈ [−1, 1]` the clamped input (every
/// state the bank reaches is the response to such a past, and `D_k(0) = 0`
/// stands for the samples before it). Each lag reads its own sample, so
/// over all inputs within full scale
///
/// ```text
/// sup y[n] = sup_u (g u + F_0(u)) + Σ_{m≥1} sup_u F_m(u)
/// ```
///
/// and the same for `−y`: the bound is the larger of the two. It is the
/// exact worst case of the whole bank, the sequence that puts every lag at
/// its own maximum reaching it; the per-term bound (each term at
/// `|w| R ‖h‖₁` on its own, [`crate::harmonics::tilt_term_bound`]) sums
/// the terms' worst cases as if they never had to share an input sample.
///
/// Certification, per lag: `F_m(cos θ) = Σ c_k (cos kθ − T_k'(0) cos θ −
/// T_k(0))` is evaluated on a uniform grid of `θ` (both ends included),
/// and `|d²F/dθ²| <= M = Σ |c_k| (k² + |T_k'(0)|)` (plus `|g|` at lag 0),
/// so between two grid points (spacing `h`) `F` exceeds the larger of them
/// by at most `M h²/8` (an interior maximum has `F' = 0`, and a grid point
/// lies within `h/2` of it). The lags are summed one by one up to `N`; the
/// rest is at most `Σ_t |w_t| R_t Σ_{m≥N} |h_t[m]|`, `R_t` a certified
/// `max |D_k|` on `[−1, 1]`, with `Σ_{m≥N} |h_t[m]| <= ‖h_t‖₁ − Σ_{m<N}
/// |h_t[m]|` from the certified L1 norm ([`TiltFilter::l1_norm`],
/// [`TiltFilter::l1_norm_then`]). `N` is the first multiple of 256 where
/// that rest falls below [`JOINT_REST`] of the bound; past the first lags,
/// most of them are bounded in blocks from the block's two ends
/// ([`LagSum`]), never below their grid values. Rounding: the sums carry a
/// relative `1e-9` and each rest an extra `1e-8 ‖h_t‖₁`.
#[derive(Clone, Debug)]
pub(crate) struct JointBound {
    /// `c_k` at lag 0, by order (index 1 unused).
    lag0: [f64; KMAX + 1],
    /// `Σ_{m≥1} sup F_m` and `Σ_{m≥1} sup (−F_m)`, the rest included.
    rest: [f64; 2],
    grid: Box<Grid>,
}

/// `D_k(cos θ_j)` by order (row 1: `cos θ_j`, the direct path).
#[derive(Clone, Debug)]
struct Grid {
    d: [[f64; JOINT_GRID]; KMAX + 1],
    /// `h²/8 · (k² + |T_k'(0)|)` by order (row 1: `h²/8`).
    margin: [f64; KMAX + 1],
}

impl Grid {
    fn new() -> Self {
        let h = PI / (JOINT_GRID - 1) as f64;
        let mut d = [[0.0; JOINT_GRID]; KMAX + 1];
        let mut margin = [0.0; KMAX + 1];
        margin[1] = h * h / 8.0;
        for (j, theta) in (0..JOINT_GRID).map(|j| (j, h * j as f64)) {
            d[1][j] = theta.cos();
        }
        for k in 2..=KMAX {
            // T_k(0) and T_k'(0): cos(kπ/2) and k sin(kπ/2).
            let (at0, slope0) = match k % 4 {
                0 => (1.0, 0.0),
                1 => (0.0, k as f64),
                2 => (-1.0, 0.0),
                _ => (0.0, -(k as f64)),
            };
            for (j, v) in d[k].iter_mut().enumerate() {
                let theta = h * j as f64;
                *v = (k as f64 * theta).cos() - slope0 * theta.cos() - at0;
            }
            margin[k] = h * h / 8.0 * ((k * k) as f64 + f64::abs(slope0));
        }
        Self { d, margin }
    }

    /// Certified `(upper bound of sup, lower bound of inf)` of `Σ c_k D_k`
    /// on `[−1, 1]` (`c_1`: the direct path), and the grid points of the
    /// two.
    fn range(&self, c: &[f64; KMAX + 1]) -> (f64, f64, usize, usize) {
        let mut v = [0.0; JOINT_GRID];
        let mut m = 0.0;
        for k in 1..=KMAX {
            let ck = c[k];
            if ck == 0.0 {
                continue;
            }
            for (vj, dj) in v.iter_mut().zip(&self.d[k]) {
                *vj += ck * dj;
            }
            m += ck.abs() * self.margin[k];
        }
        let (mut hi, mut lo, mut ih, mut il) = (f64::NEG_INFINITY, f64::INFINITY, 0, 0);
        for (j, &x) in v.iter().enumerate() {
            if x > hi {
                hi = x;
                ih = j;
            }
            if x < lo {
                lo = x;
                il = j;
            }
        }
        (hi + m, lo - m, ih, il)
    }

    /// Certified `max |D_k|` on `[−1, 1]`.
    fn reach(&self, k: usize) -> f64 {
        self.d[k].iter().fold(0.0, |a: f64, x| a.max(x.abs())) + self.margin[k]
    }

    fn u(j: usize) -> f64 {
        (PI / (JOINT_GRID - 1) as f64 * j as f64).cos()
    }
}

/// Longest block of lags [`LagSum`] bounds from its two ends.
const MAX_BLOCK: usize = 4096;
/// Most a block of [`LagSum`] may add to its lags' sum, relative; it
/// lengthens its blocks while they stay under a quarter of it.
const BLOCK_REST: f64 = 1e-5;

/// `Σ sup F_m` and `Σ sup (−F_m)` over the lags `m ≥ 1`, lag by lag on
/// the grid, or (`compress`) in blocks bounded from their two ends.
///
/// `φ(c) = sup_u Σ c_k D_k(u)` is positively homogeneous and subadditive
/// in the coefficients `c` (a supremum of linear functions of `c`), so for
/// `α, β >= 0` and `δ = c − α a − β b`,
///
/// ```text
/// φ(c) <= α φ(a) + β φ(b) + Σ_k |δ_k| R_k,
/// ```
///
/// `R_k` a certified `max |D_k|` (the same for `φ(−c)`). Within a block,
/// every lag is fitted to the block's two ends `a`, `b` (both on the grid):
/// a weighted least-squares fit clamped to `α, β >= 0`, the residual
/// counted in full, so any fit is safe and a good one is tight (the
/// impulse responses are smooth sums of decaying modes, so a lag is close
/// to a mix of its block's ends). A block is evaluated lag by lag instead
/// where its residuals exceed [`BLOCK_REST`] of its sum, or where the fit
/// at its middle lag exceeds that lag's grid value by more than that share
/// (the ends' maxima at different points leave slack the residual does not
/// show); the block length adapts. These checks only keep the bound
/// tight: any block is an upper bound.
struct LagSum<'g> {
    grid: &'g Grid,
    reach: [f64; KMAX + 1],
    compress: bool,
    /// The last lag on the grid and its `(φ(c), φ(−c))`.
    anchor: Option<([f64; KMAX + 1], [f64; 2])>,
    pending: Vec<[f64; KMAX + 1]>,
    block: usize,
    sum: [f64; 2],
}

impl<'g> LagSum<'g> {
    fn new(grid: &'g Grid, compress: bool) -> Self {
        let mut reach = [0.0; KMAX + 1];
        for (k, r) in reach.iter_mut().enumerate().skip(2) {
            *r = grid.reach(k);
        }
        Self {
            grid,
            reach,
            compress,
            anchor: None,
            pending: Vec::new(),
            block: 1,
            sum: [0.0; 2],
        }
    }

    /// `(φ(c), φ(−c))` on the grid, and the grid points of the two.
    fn exact(&self, c: &[f64; KMAX + 1]) -> ([f64; 2], usize, usize) {
        let (hi, lo, ih, il) = self.grid.range(c);
        ([hi, -lo], ih, il)
    }

    /// Adds lag `c`; its grid points when it was evaluated on its own.
    fn push(&mut self, c: &[f64; KMAX + 1]) -> Option<(usize, usize)> {
        if !self.compress || self.anchor.is_none() {
            let (e, ih, il) = self.exact(c);
            self.sum[0] += e[0];
            self.sum[1] += e[1];
            self.anchor = Some((*c, e));
            return Some((ih, il));
        }
        self.pending.push(*c);
        if self.pending.len() >= self.block {
            self.flush();
        }
        None
    }

    /// Bounds the pending lags (module docs above).
    fn flush(&mut self) {
        let Some(b) = self.pending.pop() else {
            return;
        };
        let (Some((a, ea)), (eb, _, _)) = (self.anchor, self.exact(&b)) else {
            return;
        };
        let r = &self.reach;
        let dot = |x: &[f64; KMAX + 1], y: &[f64; KMAX + 1]| -> f64 {
            (2..=KMAX).map(|k| x[k] * y[k] * r[k] * r[k]).sum()
        };
        let (aa, ab, bb) = (dot(&a, &a), dot(&a, &b), dot(&b, &b));
        let det = aa * bb - ab * ab;
        let full = self.pending.len() + 1 == self.block;
        let (mut est, mut resid) = (0.0, 0.0);
        let mut add = [0.0; 2];
        // The fit's slack at the middle lag against its grid value: the
        // ends' maxima at different points make the mix loose even where
        // the residual is small.
        let mid = self.pending.len() / 2;
        let mut gap = 0.0;
        for (i, c) in self.pending.iter().enumerate() {
            let (ca, cb) = (dot(c, &a), dot(c, &b));
            let (alpha, beta) = if det > 1e-12 * aa * bb {
                ((ca * bb - cb * ab) / det, (cb * aa - ca * ab) / det)
            } else if bb > 0.0 {
                (0.0, cb / bb)
            } else {
                (0.0, 0.0)
            };
            let (alpha, beta) = (alpha.max(0.0), beta.max(0.0));
            let delta: f64 = (2..=KMAX)
                .map(|k| (c[k] - alpha * a[k] - beta * b[k]).abs() * r[k])
                .sum();
            for (s, (x, y)) in add.iter_mut().zip(ea.iter().zip(&eb)) {
                *s += alpha * x + beta * y + delta;
            }
            let here = alpha * ea[0].max(ea[1]) + beta * eb[0].max(eb[1]);
            if i == mid {
                let (e, _, _) = self.exact(c);
                let fit = [
                    alpha * ea[0] + beta * eb[0] + delta,
                    alpha * ea[1] + beta * eb[1] + delta,
                ];
                gap = (fit[0] - e[0]).max(fit[1] - e[1]) / here.max(f64::MIN_POSITIVE);
            }
            est += here;
            resid += delta;
        }
        if resid <= BLOCK_REST * est && gap <= BLOCK_REST {
            self.sum[0] += add[0];
            self.sum[1] += add[1];
            if full && 4.0 * resid <= BLOCK_REST * est && 4.0 * gap <= BLOCK_REST {
                self.block = (2 * self.block).min(MAX_BLOCK);
            }
        } else {
            // Too loose: every lag on the grid, and shorter blocks.
            for c in &self.pending {
                let (e, _, _) = self.exact(c);
                self.sum[0] += e[0];
                self.sum[1] += e[1];
            }
            self.block = (self.block / 4).max(1);
        }
        self.sum[0] += eb[0];
        self.sum[1] += eb[1];
        self.anchor = Some((b, eb));
        self.pending.clear();
    }
}

impl JointBound {
    /// The bound of `terms` with the DC blocker `(b0, a1)` (module docs of
    /// [`JointBound`]).
    pub(crate) fn new(terms: &[JointTerm], hpf: (f64, f64)) -> Self {
        Self::build(terms, hpf, None)
    }

    /// As [`Self::new`], recording each lag's arguments of `sup F_m` and
    /// `sup (−F_m)` for the first `record.1` lags (lag 0's are
    /// [`Self::lag0_args`]).
    fn build(
        terms: &[JointTerm],
        (b0, a1): (f64, f64),
        mut record: Option<(&mut Vec<[f64; 2]>, usize)>,
    ) -> Self {
        let grid = Box::new(Grid::new());
        let hpf = FirstOrder { b0, b1: -b0, a1 };
        struct Run {
            k: usize,
            w: f64,
            filter: Option<TiltFilter>,
            dc_block: bool,
            st: TiltState,
            z: f64,
            /// `R` (a certified `max |D_k|`), `|w| R ‖h‖₁` and `|w| R
            /// Σ_{m<N} |h[m]|`.
            reach: f64,
            l1: f64,
            head: f64,
        }
        let mut runs: Vec<Run> = terms
            .iter()
            .filter(|t| t.weight != 0.0 && (2..=KMAX).contains(&t.k))
            .map(|t| {
                let l1 = match (&t.filter, t.dc_block) {
                    (None, false) => 1.0,
                    (None, true) => hpf.l1_norm() * (1.0 + 1e-12),
                    (Some(f), false) => f.l1_norm(),
                    (Some(f), true) => f.l1_norm_then((hpf.b0, hpf.b1, hpf.a1)),
                };
                let reach = grid.reach(t.k);
                Run {
                    reach,
                    k: t.k,
                    w: t.weight,
                    filter: t.filter,
                    dc_block: t.dc_block,
                    st: TiltState::default(),
                    z: 0.0,
                    l1: t.weight.abs() * reach * l1,
                    head: 0.0,
                }
            })
            .collect();
        let rest_of = |runs: &[Run]| -> f64 {
            runs.iter()
                .map(|r| (r.l1 - r.head).max(0.0) + 1e-8 * r.l1)
                .sum()
        };
        let mut lag0 = [0.0; KMAX + 1];
        let mut lag0_reach = 0.0;
        let mut lags = LagSum::new(&grid, record.is_none());
        for m in 0..JOINT_MAX_LAGS {
            let x = if m == 0 { 1.0 } else { 0.0 };
            let mut c = [0.0; KMAX + 1];
            for r in runs.iter_mut() {
                let mut y = match &r.filter {
                    Some(f) => f.tick(&mut r.st, x),
                    None => x,
                };
                if r.dc_block {
                    y = hpf.tick(y, &mut r.z);
                }
                c[r.k] += r.w * y;
                r.head += (r.w * y).abs() * r.reach;
            }
            if m == 0 {
                lag0 = c;
                lag0_reach = (2..=KMAX).map(|k| lag0[k].abs() * grid.reach(k)).sum();
            } else {
                let args = lags.push(&c);
                if let Some((rec, n)) = record.as_mut()
                    && rec.len() < *n
                    && let Some((ih, il)) = args
                {
                    rec.push([Grid::u(ih), Grid::u(il)]);
                }
            }
            if m % 256 == 255 {
                let bound = 1.0 + lags.sum[0].max(lags.sum[1]) + lag0_reach;
                if rest_of(&runs) <= JOINT_REST * bound
                    && record.as_ref().is_none_or(|(rec, n)| rec.len() >= *n)
                {
                    break;
                }
            }
        }
        lags.flush();
        let rest = lags.sum;
        let left = rest_of(&runs);
        Self {
            lag0,
            rest: [
                (rest[0] + left) * (1.0 + 1e-9),
                (rest[1] + left) * (1.0 + 1e-9),
            ],
            grid,
        }
    }

    /// Certified bound of `|y|` with direct gain `linear`.
    pub(crate) fn bound(&self, linear: f64) -> f64 {
        let mut c = self.lag0;
        c[1] = linear;
        let (hi, lo, _, _) = self.grid.range(&c);
        ((hi + self.rest[0]).max(self.rest[1] - lo)) * (1.0 + 1e-9)
    }

    /// Lag 0's arguments of `sup (g u + F_0)` and `sup −(g u + F_0)`.
    #[cfg(test)]
    fn lag0_args(&self, linear: f64) -> [f64; 2] {
        let mut c = self.lag0;
        c[1] = linear;
        let (_, _, ih, il) = self.grid.range(&c);
        [Grid::u(ih), Grid::u(il)]
    }

    /// The input that drives `y` to its bound with direct gain `linear`,
    /// from rest (`positive`: toward `+`), over `n` lags: oldest first, the
    /// peak at its last sample.
    #[cfg(test)]
    pub(crate) fn worst_input(
        terms: &[JointTerm],
        hpf: (f64, f64),
        linear: f64,
        n: usize,
        positive: bool,
    ) -> Vec<f64> {
        let mut rec = Vec::new();
        let jb = Self::build(terms, hpf, Some((&mut rec, n.saturating_sub(1))));
        let i = usize::from(!positive);
        let mut v: Vec<f64> = rec.iter().rev().map(|a| a[i]).collect();
        v.push(jb.lag0_args(linear)[i]);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lin_to_db;

    fn shelves(low: (f64, f64), high: (f64, f64)) -> OrderTilt {
        OrderTilt {
            low: TiltShelf::new(low.0, low.1),
            high: TiltShelf::new(high.0, high.1),
        }
    }

    /// As [`shelves`], with slopes (dB/oct; `None`: the default).
    fn sloped(low: (f64, f64, Option<u8>), high: (f64, f64, Option<u8>)) -> OrderTilt {
        OrderTilt {
            low: TiltShelf {
                hz: low.0,
                db: low.1,
                slope: low.2,
            },
            high: TiltShelf {
                hz: high.0,
                db: high.1,
                slope: high.2,
            },
        }
    }

    fn gain_db(f: &TiltFilter, hz: f64, rate: f64) -> f64 {
        lin_to_db(f.response(2.0 * PI * hz / rate).norm())
    }

    /// Brute-force `Σ |g[n]|` over `n` samples.
    fn brute_l1(f: &TiltFilter, n: usize) -> f64 {
        let mut st = TiltState::default();
        (0..n)
            .map(|i| f.tick(&mut st, if i == 0 { 1.0 } else { 0.0 }).abs())
            .sum()
    }

    #[test]
    fn shelves_reach_their_amount_and_are_unity_at_k_khz() {
        let rate = 192_000.0;
        for k in [2, 3, 5, 9] {
            let f = TiltFilter::new(&shelves((100.0, 12.0), (2_000.0, 0.0)), k, rate).unwrap();
            assert!(gain_db(&f, k as f64 * 1000.0, rate).abs() < 1e-9);
            // Full rise well below the corner (100 Hz / 2 = 50 Hz, × k).
            let lf = gain_db(&f, k as f64 * 5.0, rate);
            assert!((lf - 12.0).abs() < 0.05, "k {k}: {lf}");
            // 12 dB/oct between the corners.
            let a = gain_db(&f, k as f64 * 60.0, rate);
            let b = gain_db(&f, k as f64 * 30.0, rate);
            assert!(b > a && b - a < 12.5, "{a} {b}");
            let f = TiltFilter::new(&shelves((100.0, 0.0), (1_000.0, 20.0)), k, rate).unwrap();
            let hf = gain_db(&f, k as f64 * 8_000.0, rate);
            // 6 dB/oct: about +16 dB three octaves above the corner, less
            // the 3 dB the shelf has at the corner (the unity point); more
            // near Nyquist (bilinear warping), never past the amount.
            assert!(
                hf > 12.0 && hf < if k < 5 { 14.0 } else { 20.0 },
                "k {k}: {hf}"
            );
            // Cuts mirror boosts.
            let f = TiltFilter::new(&shelves((100.0, -12.0), (20_000.0, -6.0)), k, rate).unwrap();
            let lf = gain_db(&f, k as f64 * 5.0, rate);
            assert!((lf + 12.0).abs() < 0.1, "k {k}: {lf}");
            assert!(gain_db(&f, k as f64 * 1000.0, rate).abs() < 1e-9);
        }
        assert!(TiltFilter::new(&shelves((100.0, 0.0), (2_000.0, 0.0)), 3, rate).is_none());
    }

    #[test]
    fn l1_norm_is_a_tight_upper_bound() {
        for rate in [44_100.0, 192_000.0, 768_000.0] {
            for (low, high) in [
                ((20.0, 24.0), (2_000.0, 0.0)),
                ((100.0, 12.0), (2_000.0, 20.0)),
                ((2_000.0, -24.0), (200.0, -24.0)),
                ((100.0, 0.0), (500.0, 24.0)),
                ((300.0, 6.0), (20_000.0, -12.0)),
            ] {
                for k in [2, 3, 9] {
                    let f = TiltFilter::new(&shelves(low, high), k, rate).unwrap();
                    let l1 = f.l1_norm();
                    // The longest pole here decays within 2 s at any rate.
                    let brute = brute_l1(&f, (4.0 * rate) as usize);
                    assert!(l1 >= brute, "{rate} {low:?} {high:?} H{k}: {l1} < {brute}");
                    assert!(
                        l1 <= brute * (1.0 + 1e-6) + 1e-9,
                        "{rate} {low:?} {high:?} H{k}: {l1} vs {brute}"
                    );
                    assert!(l1 >= f.dc_gain().abs());
                }
            }
        }
    }

    const SLOPES: [Option<u8>; 5] = [None, Some(6), Some(12), Some(18), Some(24)];

    #[test]
    fn the_default_slopes_are_the_unsloped_design_bit_for_bit() {
        for rate in [44_100.0, 768_000.0] {
            for (low, high) in [
                ((60.0, 12.0), (6_000.0, 6.0)),
                ((300.0, -9.0), (200.0, -24.0)),
            ] {
                for k in [2, 5, 9] {
                    let a = TiltFilter::new(&shelves(low, high), k, rate).unwrap();
                    let b = TiltFilter::new(
                        &sloped((low.0, low.1, Some(12)), (high.0, high.1, Some(6))),
                        k,
                        rate,
                    )
                    .unwrap();
                    assert_eq!(a, b);
                    assert_eq!(a.l1_norm().to_bits(), b.l1_norm().to_bits());
                }
            }
        }
    }

    #[test]
    fn sloped_l1_norms_are_tight_upper_bounds() {
        for rate in [44_100.0, 192_000.0, 768_000.0] {
            for slope in SLOPES {
                for (low, high) in [
                    ((20.0, 24.0), (2_000.0, 0.0)),
                    ((100.0, 12.0), (2_000.0, 20.0)),
                    ((2_000.0, -24.0), (200.0, -24.0)),
                    ((100.0, 0.0), (500.0, 24.0)),
                    ((300.0, 6.0), (20_000.0, -12.0)),
                ] {
                    // The slope on both shelves, and on one only.
                    for (sl, sh) in [(slope, slope), (slope, Some(24)), (Some(6), slope)] {
                        for k in [2, 9] {
                            let f = TiltFilter::new(
                                &sloped((low.0, low.1, sl), (high.0, high.1, sh)),
                                k,
                                rate,
                            )
                            .unwrap();
                            let l1 = f.l1_norm();
                            // The slowest pole here (a 6 dB/oct cut of 24
                            // dB from H2 of 20 Hz, 2.5 Hz) decays within 2 s.
                            let brute = brute_l1(&f, (4.0 * rate) as usize);
                            let what = format!("{rate} {low:?}/{sl:?} {high:?}/{sh:?} H{k}");
                            assert!(l1 >= brute, "{what}: {l1} < {brute}");
                            assert!(l1 <= brute * (1.0 + 1e-6) + 1e-9, "{what}: {l1} vs {brute}");
                            assert!(l1 >= f.dc_gain().abs());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sloped_filters_flush_and_stay_finite() {
        let f = TiltFilter::new(
            &sloped((20.0, 24.0, Some(18)), (2_000.0, -24.0, Some(24))),
            9,
            48_000.0,
        )
        .unwrap();
        let mut st = TiltState::default();
        f.tick(&mut st, 1.0);
        for _ in 0..200 {
            for _ in 0..512 {
                let y = f.tick(&mut st, 0.0);
                assert!(y.is_finite());
            }
            st.flush();
        }
        for _ in 0..2_000 {
            for _ in 0..512 {
                f.tick(&mut st, 0.0);
            }
            st.flush();
        }
        assert_eq!(st, TiltState::default());
    }
}
