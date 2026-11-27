//! Transformer/tape core-saturation ("flux"): the core of a flux layer of the
//! harmonics stage ([`crate::harmonics`]).
//!
//! Structure:
//!
//! ```text
//! y = s · ( x + P⁻¹( ADAA₁(D)(P x) ) )
//! ```
//!
//! where:
//! - `P` is a first-order low shelf (leaky integrator) with high-frequency gain
//!   `1`, DC gain `G = 10^(max_boost_db / 20)`, and upper corner `corner_hz`:
//!   `P(s) = (s + ω_c) / (s + ω_c / G)`, discretized via the bilinear transform
//!   with prewarping at `corner_hz`.
//! - `P⁻¹` is the exact digital inverse of `P` (`P⁻¹(z) P(z) = 1`), minimum-phase
//!   and strictly stable for all `0 < corner_hz < fs / 2` and `G >= 1`.
//! - `D(u) = S(u) - u`, with odd saturator `S(u) = tanh(g u) / g`. Because `D`
//!   has no linear term (`D(0) = D'(0) = 0`), small signals pass through with
//!   flat frequency response (`y ≈ s · x`).
//! - First-order antiderivative anti-aliasing (ADAA1) is applied to `D` itself
//!   (antiderivative `F(u) = (ln cosh(g u) - (g u)²/2) / g²`), never to `S`
//!   before subtracting `u`, so ADAA's half-sample delay leaves no linear
//!   residual.
//!
//! Calibration: given a target 3rd harmonic level in dBc for a 0 dBFS sine at
//! `calibration_hz`, the drive `g` is solved numerically at build time against
//! the discrete-time ADAA1 periodic response at `output_rate`.
//!
//! Peak bound: `B` certifies the layer's output for any input within
//! `[-1, 1]` (the harmonics stage's worst-case peak gain; nothing scales the
//! output down). Samples beyond full scale (inter-sample overs) are clamped
//! before `P` and the excess passes with unity slope, matching
//! [`crate::harmonics::Harmonics`].

use crate::{Complex64, db_to_lin, lin_to_db};
use audivane_protocol::FluxLayer;
pub use audivane_protocol::{
    FLUX_MAX_BOOST_DB, FLUX_MAX_CALIBRATION_HZ, FLUX_MAX_CORNER_HZ, FLUX_MAX_H3_DB,
    FLUX_MIN_CALIBRATION_HZ, FLUX_MIN_CORNER_HZ, FLUX_MIN_H3_DB,
};
use std::f64::consts::{LN_2, PI};
use std::fmt;

/// Switch to Taylor polynomial evaluation of `d0` and `F0` below this `|t|`
/// to avoid catastrophic cancellation of the linear / quadratic terms.
const TAYLOR_THRESHOLD: f64 = 0.25;
/// Above this `|t|`, compute ADAA via `(lncosh(t1) - lncosh(t0))/dt - t_avg`
/// to avoid squaring large arguments in `F0(t) = lncosh(t) - t²/2`.
const LARGE_T_THRESHOLD: f64 = 20.0;
/// Relative step threshold for the ill-conditioned ADAA1 midpoint fallback.
const ADAA_REL_TOL: f64 = 1e-5;
/// Below this `|t0| + |t1|`, avoid subnormal underflow in `t^4` by using the
/// midpoint evaluation directly.
const ADAA_UNDERFLOW_TOL: f64 = 1e-70;

/// Exact Taylor coefficients of `d0(t) = tanh(t) - t = sum_{k>=0} c_k t^(2k+3)`,
/// derived from Bernoulli numbers. Accurate to machine precision for `|t| < 0.25`.
const D0_TAYLOR: [f64; 9] = [
    -1.0 / 3.0,
    2.0 / 15.0,
    -17.0 / 315.0,
    62.0 / 2_835.0,
    -1_382.0 / 155_925.0,
    21_844.0 / 6_081_075.0,
    -929_569.0 / 638_512_875.0,
    6_404_582.0 / 10_854_718_875.0,
    -443_861_162.0 / 1_856_156_927_625.0,
];

/// Exact Taylor coefficients of `F0(t) = ln(cosh(t)) - t²/2 = sum_{k>=0} c_k t^(2k+4)`.
/// Term `k` is `D0_TAYLOR[k] / (2k + 4)`. Accurate to machine precision for `|t| < 0.25`.
const F0_TAYLOR: [f64; 9] = [
    D0_TAYLOR[0] / 4.0,
    D0_TAYLOR[1] / 6.0,
    D0_TAYLOR[2] / 8.0,
    D0_TAYLOR[3] / 10.0,
    D0_TAYLOR[4] / 12.0,
    D0_TAYLOR[5] / 14.0,
    D0_TAYLOR[6] / 16.0,
    D0_TAYLOR[7] / 18.0,
    D0_TAYLOR[8] / 20.0,
];

/// Half-cycle DFT points for calibration and steady-state harmonic prediction
/// (full cycle = `2 * CAL_HALF_POINTS = 256` points using odd symmetry).
const CAL_HALF_POINTS: usize = 128;

/// Why a flux layer's core could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FluxError {
    InvalidSampleRate,
    InvalidParameter(String),
    Uncalibratable(String),
}

impl fmt::Display for FluxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => write!(f, "flux requires a positive finite sample rate"),
            Self::InvalidParameter(msg) => write!(f, "flux: {msg}"),
            Self::Uncalibratable(msg) => write!(f, "flux calibration failed: {msg}"),
        }
    }
}

impl std::error::Error for FluxError {}

/// Configuration of a flux layer's core.
#[derive(Clone, Debug, PartialEq)]
pub struct FluxConfig {
    /// Whether the stage processes audio.
    pub enabled: bool,
    /// Pre-emphasis low-shelf corner frequency in Hz (boost begins below this).
    pub corner_hz: f64,
    /// Maximum low-frequency boost of `P` in dB (`0..=40`).
    pub max_boost_db: f64,
    /// Target 3rd harmonic level in dBc for a 0 dBFS sine at `calibration_hz`.
    pub target_h3_db: f64,
    /// Reference sine frequency in Hz for `target_h3_db`.
    pub calibration_hz: f64,
}

impl Default for FluxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            corner_hz: 100.0,
            max_boost_db: 24.0,
            target_h3_db: -40.0,
            calibration_hz: 40.0,
        }
    }
}

impl FluxConfig {
    pub fn is_active(&self) -> bool {
        self.enabled
    }
}

impl From<&FluxLayer> for FluxConfig {
    fn from(l: &FluxLayer) -> Self {
        Self {
            enabled: l.enabled,
            corner_hz: l.corner_hz,
            max_boost_db: l.max_boost_db,
            target_h3_db: l.target_h3_db,
            calibration_hz: l.calibration_hz,
        }
    }
}

/// First-order rational section `H(z) = (b0 + b1 z⁻¹) / (1 + a1 z⁻¹)`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FirstOrder {
    b0: f64,
    b1: f64,
    a1: f64,
}

impl FirstOrder {
    const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        a1: 0.0,
    };

    /// Design the leaky-integrator low shelf `P(s) = (s + ω_c) / (s + ω_c / G)`
    /// and its exact digital inverse `P⁻¹(z)` via prewarped bilinear transform.
    fn leaky_integrator_pair(corner_hz: f64, max_boost_db: f64, sample_rate: f64) -> (Self, Self) {
        if max_boost_db == 0.0 {
            return (Self::IDENTITY, Self::IDENTITY);
        }
        let g = db_to_lin(max_boost_db);
        let k = (PI * corner_hz / sample_rate).tan();
        let kg = k / g;

        let a0_p = 1.0 + kg;
        let pre = Self {
            b0: (1.0 + k) / a0_p,
            b1: (k - 1.0) / a0_p,
            a1: (kg - 1.0) / a0_p,
        };

        let a0_i = 1.0 + k;
        let de = Self {
            b0: (1.0 + kg) / a0_i,
            b1: (kg - 1.0) / a0_i,
            a1: (k - 1.0) / a0_i,
        };
        (pre, de)
    }

    /// Complex frequency response `H(e^{j 2π f / fs})`.
    fn response(&self, freq_hz: f64, sample_rate: f64) -> Complex64 {
        let w = 2.0 * PI * freq_hz / sample_rate;
        let (sin_w, cos_w) = w.sin_cos();
        let nr = self.b0 + self.b1 * cos_w;
        let ni = -self.b1 * sin_w;
        let dr = 1.0 + self.a1 * cos_w;
        let di = -self.a1 * sin_w;
        let den = dr * dr + di * di;
        Complex64::new((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
    }

    /// Exact `ℓ₁` norm `∑_{n>=0} |h[n]|` of a stable causal first-order IIR:
    /// `h[0] = b0`, `h[n] = (b1 - b0 a1) (-a1)^{n-1}` for `n >= 1`.
    fn l1_norm(&self) -> f64 {
        self.b0.abs() + (self.b1 - self.b0 * self.a1).abs() / (1.0 - self.a1.abs())
    }
}

/// Dimensionless `d0(t) = tanh(t) - t`, cancellation-free near 0 and finite for
/// all finite `t`.
#[inline]
fn d0_tanh_diff(t: f64) -> f64 {
    if t.abs() < TAYLOR_THRESHOLD {
        let t2 = t * t;
        let mut s = D0_TAYLOR[D0_TAYLOR.len() - 1];
        for &c in D0_TAYLOR[..D0_TAYLOR.len() - 1].iter().rev() {
            s = s * t2 + c;
        }
        s * t * t2
    } else {
        t.tanh() - t
    }
}

/// Dimensionless antiderivative `F0(t) = ln(cosh(t)) - t²/2` for `|t| < LARGE_T_THRESHOLD`,
/// cancellation-free near 0.
#[inline]
fn f0_tanh_diff(t: f64) -> f64 {
    if t.abs() < TAYLOR_THRESHOLD {
        let t2 = t * t;
        let mut s = F0_TAYLOR[F0_TAYLOR.len() - 1];
        for &c in F0_TAYLOR[..F0_TAYLOR.len() - 1].iter().rev() {
            s = s * t2 + c;
        }
        s * t2 * t2
    } else {
        lncosh(t) - 0.5 * t * t
    }
}

/// Overflow-free `ln(cosh(t)) = |t| - ln(2) + ln(1 + exp(-2|t|))`.
#[inline]
fn lncosh(t: f64) -> f64 {
    let at = t.abs();
    at - LN_2 + (-2.0 * at).exp().ln_1p()
}

/// First-order ADAA of `d0(t) = tanh(t) - t` between `t_prev` and `t`.
///
/// Uses the midpoint fallback `d0((t + t_prev)/2)` when `|t - t_prev|` is small
/// relative to `|t + t_prev|/2` (avoiding cancellation in `(F0(t) - F0(t_prev))/dt`)
/// or when both arguments are near subnormal underflow. For large `|t|`, splits
/// out the exact analytic linear part `-0.5*(t + t_prev)` so `t²` is never formed.
#[inline]
fn adaa1_d0_stateless(t_prev: f64, t: f64) -> f64 {
    let dt = t - t_prev;
    let t_avg = 0.5 * t + 0.5 * t_prev;
    if dt.abs() <= ADAA_REL_TOL * t_avg.abs() || t.abs() + t_prev.abs() < ADAA_UNDERFLOW_TOL {
        d0_tanh_diff(t_avg)
    } else if t.abs() >= LARGE_T_THRESHOLD || t_prev.abs() >= LARGE_T_THRESHOLD {
        (lncosh(t) - lncosh(t_prev)) / dt - t_avg
    } else {
        (f0_tanh_diff(t) - f0_tanh_diff(t_prev)) / dt
    }
}

/// Per-channel processing state.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ChannelState {
    /// TDF-II state of `P`.
    pre_z1: f64,
    /// TDF-II state of `P⁻¹`.
    de_z1: f64,
    /// Previous scaled flux `t[n-1] = g * u[n-1]`.
    t_prev: f64,
    /// Cached `F0(t_prev)` whenever `|t_prev| < LARGE_T_THRESHOLD`.
    f0_prev: f64,
}

/// Precomputed unit-circle tables for steady-state periodic H3 evaluation at a
/// fixed `(freq_hz, sample_rate)`.
struct PeriodicProbe {
    pf: f64,
    p3f: f64,
    cos_cur: [f64; CAL_HALF_POINTS],
    cos_prev: [f64; CAL_HALF_POINTS],
    cos1: [f64; CAL_HALF_POINTS],
    sin1: [f64; CAL_HALF_POINTS],
    cos3: [f64; CAL_HALF_POINTS],
    sin3: [f64; CAL_HALF_POINTS],
}

impl PeriodicProbe {
    fn new(pre: &FirstOrder, freq_hz: f64, sample_rate: f64) -> Self {
        let pf = pre.response(freq_hz, sample_rate).norm();
        let p3f = pre.response(3.0 * freq_hz, sample_rate).norm();
        let w = 2.0 * PI * freq_hz / sample_rate;
        let m_full = (2 * CAL_HALF_POINTS) as f64;
        let mut cos_cur = [0.0; CAL_HALF_POINTS];
        let mut cos_prev = [0.0; CAL_HALF_POINTS];
        let mut cos1 = [0.0; CAL_HALF_POINTS];
        let mut sin1 = [0.0; CAL_HALF_POINTS];
        let mut cos3 = [0.0; CAL_HALF_POINTS];
        let mut sin3 = [0.0; CAL_HALF_POINTS];
        for i in 0..CAL_HALF_POINTS {
            let th = 2.0 * PI * i as f64 / m_full;
            let (s1, c1) = th.sin_cos();
            let (s3, c3) = (3.0 * th).sin_cos();
            cos_cur[i] = c1;
            cos_prev[i] = (th - w).cos();
            cos1[i] = c1;
            sin1[i] = s1;
            cos3[i] = c3;
            sin3[i] = s3;
        }
        Self {
            pf,
            p3f,
            cos_cur,
            cos_prev,
            cos1,
            sin1,
            cos3,
            sin3,
        }
    }

    /// Exact steady-state H3 in dBc of `y = x + P⁻¹(ADAA₁(D)(P x))` for an
    /// input sine of linear peak amplitude `level` and saturator drive `drive`.
    fn h3_dbc(&self, drive: f64, level: f64) -> f64 {
        if drive <= 0.0 || level <= 0.0 {
            return f64::NEG_INFINITY;
        }
        let g_au = drive * level * self.pf;
        let inv_g = 1.0 / drive;
        let mut d1_re = 0.0;
        let mut d1_im = 0.0;
        let mut d3_re = 0.0;
        let mut d3_im = 0.0;
        for i in 0..CAL_HALF_POINTS {
            let t1 = g_au * self.cos_cur[i];
            let t0 = g_au * self.cos_prev[i];
            let d = adaa1_d0_stateless(t0, t1) * inv_g;
            d1_re += d * self.cos1[i];
            d1_im -= d * self.sin1[i];
            d3_re += d * self.cos3[i];
            d3_im -= d * self.sin3[i];
        }
        // Full-cycle Fourier coefficient (using odd half-wave symmetry):
        // D_k = (2 / (2N)) * 2 * sum_{0..N} = (2 / N) * sum_{0..N}.
        let norm = 2.0 / CAL_HALF_POINTS as f64;
        d1_re *= norm;
        d1_im *= norm;
        d3_re *= norm;
        d3_im *= norm;

        // Fundamental: Y_1 = level + D_1 / |P(f)| (phase cancels in P⁻¹(f) P(f) = 1).
        let y1_re = level + d1_re / self.pf;
        let y1_im = d1_im / self.pf;
        let y1_mag = y1_re.hypot(y1_im);
        // 3rd harmonic: |Y_3| = |D_3| / |P(3f)|.
        let y3_mag = d3_re.hypot(d3_im) / self.p3f;
        if y3_mag <= 0.0 || y1_mag <= 0.0 {
            return f64::NEG_INFINITY;
        }
        lin_to_db(y3_mag / y1_mag)
    }
}

/// Solve saturator drive `g > 0` so a 0 dBFS sine at `calibration_hz` yields
/// `target_h3_db` dBc of 3rd harmonic at `output_rate`.
fn solve_drive(
    pre: &FirstOrder,
    calibration_hz: f64,
    target_h3_db: f64,
    output_rate: f64,
) -> Result<f64, FluxError> {
    let probe = PeriodicProbe::new(pre, calibration_hz, output_rate);
    if !probe.pf.is_finite() || probe.pf <= 0.0 || !probe.p3f.is_finite() || probe.p3f <= 0.0 {
        return Err(FluxError::Uncalibratable(
            "non-finite pre-emphasis response at calibration frequency".into(),
        ));
    }

    // Small-signal cubic approximation: H3 ≈ (g² / 12) * |P(f)|³ / |P(3f)|.
    let target_lin = db_to_lin(target_h3_db);
    let g0 = (12.0 * target_lin * probe.p3f / probe.pf.powi(3)).sqrt();
    if !g0.is_finite() || g0 <= 0.0 {
        return Err(FluxError::Uncalibratable(
            "invalid initial drive estimate".into(),
        ));
    }

    // Bracket on the monotonically rising branch of H3(g).
    let mut lo = g0 * 0.25;
    let mut h_lo = probe.h3_dbc(lo, 1.0);
    for _ in 0..64 {
        if h_lo <= target_h3_db {
            break;
        }
        lo *= 0.25;
        h_lo = probe.h3_dbc(lo, 1.0);
    }
    if h_lo > target_h3_db {
        return Err(FluxError::Uncalibratable(format!(
            "could not bracket lower bound for {target_h3_db:.2} dBc"
        )));
    }

    let mut hi = g0 * 1.5;
    let mut prev_h = h_lo;
    let mut reached = false;
    for _ in 0..64 {
        let h_hi = probe.h3_dbc(hi, 1.0);
        if h_hi >= target_h3_db {
            reached = true;
            break;
        }
        // ADAA's half-sample residual causes H3(g) to peak and turn over at
        // extreme saturation; stop if we passed the peak.
        if h_hi <= prev_h || hi * probe.pf > 1e4 {
            return Err(FluxError::Uncalibratable(format!(
                "target {target_h3_db:.2} dBc exceeds maximum achievable H3 (~{:.2} dBc)",
                prev_h.max(h_hi)
            )));
        }
        prev_h = h_hi;
        hi *= 2.0;
    }
    if !reached {
        return Err(FluxError::Uncalibratable(format!(
            "target {target_h3_db:.2} dBc could not be bracketed"
        )));
    }

    // Bisect in log(g) to well below 1e-6 dB.
    let (mut l_lo, mut l_hi) = (lo.ln(), hi.ln());
    for _ in 0..64 {
        let l_mid = 0.5 * (l_lo + l_hi);
        let h_mid = probe.h3_dbc(l_mid.exp(), 1.0);
        if h_mid < target_h3_db {
            l_lo = l_mid;
        } else {
            l_hi = l_mid;
        }
    }
    let drive = (0.5 * (l_lo + l_hi)).exp();
    let achieved = probe.h3_dbc(drive, 1.0);
    if !drive.is_finite() || drive <= 0.0 || (achieved - target_h3_db).abs() > 1e-3 {
        return Err(FluxError::Uncalibratable(format!(
            "solver converged to {achieved:.4} dBc for target {target_h3_db:.4} dBc"
        )));
    }
    Ok(drive)
}

/// Certified upper bound on `max_{|t| <= U} |S(t) - (1 - lambda) t|` where
/// `S(t) = tanh(g t) / g`, for `lambda in [0, 1]`.
fn residual_max(g: f64, u_max: f64, lambda: f64) -> f64 {
    if g <= 0.0 || u_max <= 0.0 {
        return 0.0;
    }
    let gu = g * u_max;
    let th_u = gu.tanh();
    let tanh2_u = th_u * th_u;
    let inv_g = 1.0 / g;
    if lambda <= 0.0 {
        // S(t) - t = d0(g t)/g, monotone decreasing on [0, U].
        return -d0_tanh_diff(gu) * inv_g;
    }
    let end = ((th_u * inv_g) - (1.0 - lambda) * u_max).abs();
    if lambda >= tanh2_u {
        // S'(t) >= sech²(g U) = 1 - tanh²(g U) >= 1 - lambda on [0, U],
        // so S(t) - (1 - lambda) t is monotone increasing on [0, U].
        return end;
    }
    // Interior maximum at tanh(g t*) = sqrt(lambda):
    // f(t*) = (sqrt(lambda) - (1 - lambda) atanh(sqrt(lambda))) / g.
    let s = lambda.sqrt();
    let peak_dimless = if s < 1e-2 {
        // Series: s - (1 - s²) atanh(s) = 2 * sum_{k>=1} s^(2k+1) / ((2k-1)(2k+1))
        let s2 = s * s;
        2.0 * s
            * s2
            * (1.0 / 3.0 + s2 * (1.0 / 15.0 + s2 * (1.0 / 35.0 + s2 * (1.0 / 63.0 + s2 / 99.0))))
    } else {
        s - (1.0 - lambda) * s.atanh()
    };
    end.max(peak_dimless * inv_g)
}

/// Certified peak output bound `B >= 1` for `y = x + P⁻¹(ADAA₁(D)(P x))` over
/// all inputs `|x[n]| <= 1` starting from zero filter state.
///
/// Proof sketch:
/// For any `λ ∈ [0, 1]`, split `D(t) = -λ t + R_λ(t)` where `R_λ(t) = S(t) - (1 - λ) t`.
/// Both the ADAA1 integral branch and the midpoint fallback satisfy
/// `d[n] = -λ (u[n] + u[n-1]) / 2 + r[n]`, where `|r[n]| <= M(λ) = max_{|t|<=U} |R_λ(t)|`
/// and `U = ‖p‖₁`. Applying the exact inverse `P⁻¹` (which maps `u -> x` and
/// `z⁻¹ u -> z⁻¹ x` from zero initial state) yields
///
/// ```text
/// y[n] = (1 - λ/2) x[n] - (λ/2) x[n-1] + (P⁻¹ r)[n].
/// ```
///
/// Since `(1 - λ/2) + (λ/2) = 1`, the linear part has magnitude `<= 1`, giving
/// the universal bound `B₁(λ) = 1 + ‖p⁻¹‖₁ · M(λ)` for all `0 < K < ∞`.
///
/// When `K = tan(π f_c / fs) <= 1` (i.e. `f_c <= fs / 4`), `‖p‖₁ = G` and
/// `P⁻¹` decomposes exactly as
///
/// ```text
/// (P⁻¹ r)[n] = (1/G) r[n] + ((1 - 1/G) / (1 + K)) (1 - α) ∑_{k>=1} α^{k-1} (r[n] - r[n-k])
/// ```
///
/// with `α = (1 - K) / (1 + K) ∈ [0, 1)`. Because `R_λ` has Lipschitz constant
/// `L(λ) = max(λ, |tanh²(g G) - λ|)` on `[-G, G]` and every convex chord point
/// `u_t = (1-t) u[n-1] + t u[n]` satisfies `|u_t[n] - u_t[n-k]| <= ‖(1 - z⁻ᵏ) P‖₁`,
/// the α-weighted average of `‖(1 - z⁻ᵏ) P‖₁` evaluates in closed form to
/// `4 G / (G + 1)` independently of `K`, giving the tighter uniform bound
///
/// ```text
/// B₂(λ) = 1 + M(λ)/G + ((1 - 1/G) / (1 + K)) · min(2 M(λ), L(λ) · 4G/(G+1) + 2 δ_max),
/// ```
///
/// where `δ_max` bounds the midpoint-rule discrepancy on the ADAA fallback
/// branch. Optimizing over `λ ∈ [0, 1]` yields a tight certified bound that
/// stays `<= 3.5` for arbitrary boost and drive, while approaching `1` as
/// `g -> 0` or `g -> ∞`.
///
/// Returns the best split `(λ, B(λ))`, without the rounding margin (see
/// [`FluxCore::peak_bound`]). `B(λ) − 1` bounds the residual path
/// `|(P⁻¹ r)[n]|`, so the distortion path alone is `−(λ/2)(x[n] + x[n−1])`
/// plus at most `B(λ) − 1`.
fn certified_split(
    pre: &FirstOrder,
    de: &FirstOrder,
    corner_hz: f64,
    max_boost_db: f64,
    output_rate: f64,
    drive: f64,
) -> (f64, f64) {
    if drive <= 0.0 {
        return (0.0, 1.0);
    }
    let u_max = pre.l1_norm();
    let pinv_l1 = de.l1_norm();
    let g_shelf = db_to_lin(max_boost_db);
    let k = (PI * corner_hz / output_rate).tan();
    let gu = drive * u_max;
    let th_u = gu.tanh();
    let tanh2_u = th_u * th_u;

    // Upper bound on |d_midpoint - d_integral| when the fallback branch triggers:
    // |dt| <= ADAA_REL_TOL * g * U => |du| <= ADAA_REL_TOL * U,
    // |d0''(t)| = 2 |tanh(t)| sech²(t) <= min(2 g U, 4 / (3 sqrt(3))).
    let d0_d2_max = (2.0 * gu).min(4.0 / (3.0 * 3.0_f64.sqrt()));
    let delta_max = (drive * d0_d2_max / 24.0) * (ADAA_REL_TOL * u_max).powi(2);

    let use_lipschitz = max_boost_db > 0.0 && k <= 1.0;
    let factor = (1.0 - 1.0 / g_shelf) / (1.0 + k);
    let e_du = 4.0 * g_shelf / (g_shelf + 1.0);

    let eval_bound = |lambda: f64| -> f64 {
        let lam = lambda.clamp(0.0, 1.0);
        let m = residual_max(drive, u_max, lam);
        let b_l1 = 1.0 + pinv_l1 * m;
        if use_lipschitz {
            let lip = lam.max((tanh2_u - lam).abs());
            let diff_bound = (2.0 * m).min(lip * e_du + 2.0 * delta_max);
            let b_lip = 1.0 + m / g_shelf + factor * diff_bound;
            b_l1.min(b_lip)
        } else {
            b_l1
        }
    };

    // Sample a fine grid plus the analytic Lipschitz midpoint, then refine with
    // golden-section search around the best grid point. Every sample is a valid
    // certified upper bound, so the minimum is strictly certified.
    const GRID: usize = 256;
    let mut best_lam = 0.0;
    let mut best_b = eval_bound(0.0);
    for lam in [1.0, 0.5 * tanh2_u] {
        let b = eval_bound(lam);
        if b < best_b {
            best_b = b;
            best_lam = lam;
        }
    }
    for i in 1..GRID {
        let lam = i as f64 / GRID as f64;
        let b = eval_bound(lam);
        if b < best_b {
            best_b = b;
            best_lam = lam;
        }
    }

    let step = 1.0 / GRID as f64;
    let mut a = (best_lam - step).max(0.0);
    let mut c = (best_lam + step).min(1.0);
    let inv_phi = 0.5 * (5.0_f64.sqrt() - 1.0);
    let mut lam_out = best_lam;
    for _ in 0..32 {
        let x1 = c - inv_phi * (c - a);
        let x2 = a + inv_phi * (c - a);
        let b1 = eval_bound(x1);
        let b2 = eval_bound(x2);
        if b1 < best_b {
            best_b = b1;
            lam_out = x1;
        }
        if b2 < best_b {
            best_b = b2;
            lam_out = x2;
        }
        if b1 < b2 {
            c = x2;
        } else {
            a = x1;
        }
    }
    (lam_out.clamp(0.0, 1.0), best_b)
}

/// The distortion path of a flux layer, `P⁻¹(ADAA₁(D)(P x))`, without the
/// direct path or output scale: what the harmonics stage sums per layer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FluxCore {
    pre: FirstOrder,
    de: FirstOrder,
    drive: f64,
    inv_drive: f64,
    /// `(λ, B(λ))` of [`certified_split`]: the distortion path is
    /// `−(λ/2)(x[n] + x[n−1])` plus a residual of at most `B(λ) − 1` for
    /// inputs within full scale.
    split: (f64, f64),
}

impl FluxCore {
    /// No distortion (a disabled layer).
    #[cfg(test)]
    const OFF: Self = Self {
        pre: FirstOrder::IDENTITY,
        de: FirstOrder::IDENTITY,
        drive: 0.0,
        inv_drive: 0.0,
        split: (0.0, 1.0),
    };

    /// Calibrate a flux layer at `output_rate` (validated; off the audio
    /// thread).
    pub(crate) fn try_new(config: &FluxConfig, output_rate: f64) -> Result<Self, FluxError> {
        if !output_rate.is_finite() || output_rate <= 0.0 {
            return Err(FluxError::InvalidSampleRate);
        }
        validate_config(config, output_rate)?;
        let (pre, de) =
            FirstOrder::leaky_integrator_pair(config.corner_hz, config.max_boost_db, output_rate);
        let drive = solve_drive(
            &pre,
            config.calibration_hz,
            config.target_h3_db,
            output_rate,
        )?;
        let split = certified_split(
            &pre,
            &de,
            config.corner_hz,
            config.max_boost_db,
            output_rate,
            drive,
        );
        Ok(Self {
            pre,
            de,
            drive,
            inv_drive: 1.0 / drive,
            split,
        })
    }

    /// Certified peak bound `B >= 1` of `x + P⁻¹(ADAA₁(D)(P x))` alone
    /// (as [`certified_peak_bound`]).
    #[cfg(test)]
    fn peak_bound(&self) -> f64 {
        let b = self.split.1;
        // Small safety margin for IEEE-754 rounding in the TDF-II recurrence.
        if b > 1.0 { b * (1.0 + 1e-12) } else { 1.0 }
    }

    /// `(λ, R)`: the distortion path is `−(λ/2)(x[n] + x[n−1])` plus a
    /// residual of at most `R` for inputs within full scale.
    pub(crate) fn split(&self) -> (f64, f64) {
        (self.split.0, (self.split.1 - 1.0).max(0.0))
    }

    /// One sample of the distortion path for the clamped input `xc`.
    #[inline(always)]
    pub(crate) fn tick(&self, st: &mut ChannelState, xc: f64) -> f64 {
        let (pre, de) = (&self.pre, &self.de);
        let g = self.drive;
        // Pre-emphasis low shelf P (TDF-II).
        let u = pre.b0 * xc + st.pre_z1;
        st.pre_z1 = pre.b1 * xc - pre.a1 * u;

        // First-order ADAA applied directly to D(u) = S(u) - u.
        let t = g * u;
        let t_prev = st.t_prev;
        let f0_prev = st.f0_prev;
        let f0_cur = if t.abs() < LARGE_T_THRESHOLD {
            f0_tanh_diff(t)
        } else {
            0.0
        };
        st.t_prev = t;
        st.f0_prev = f0_cur;

        let dt = t - t_prev;
        let t_avg = 0.5 * t + 0.5 * t_prev;
        let d0 = if dt.abs() <= ADAA_REL_TOL * t_avg.abs()
            || t.abs() + t_prev.abs() < ADAA_UNDERFLOW_TOL
        {
            d0_tanh_diff(t_avg)
        } else if t.abs() >= LARGE_T_THRESHOLD || t_prev.abs() >= LARGE_T_THRESHOLD {
            (lncosh(t) - lncosh(t_prev)) / dt - t_avg
        } else {
            (f0_cur - f0_prev) / dt
        };
        let d = d0 * self.inv_drive;

        // De-emphasis exact inverse shelf P⁻¹ (TDF-II).
        let p_inv_d = de.b0 * d + st.de_z1;
        st.de_z1 = de.b1 * d - de.a1 * p_inv_d;

        // A decaying tail after silence never enters subnormals.
        if st.pre_z1.abs() < FLUSH_BELOW {
            st.pre_z1 = 0.0;
        }
        if st.de_z1.abs() < FLUSH_BELOW {
            st.de_z1 = 0.0;
        }
        p_inv_d
    }
}

impl FluxCore {
    /// Steady-state complex amplitudes `[DC, H1, …, H15]` of the distortion
    /// path alone for the input `level · cos(2π f n / fs)` (cosine phase, as
    /// the harmonics readouts measure), evaluated over one period as the
    /// calibration does ([`PeriodicProbe`]): the ADAA output `d` of the
    /// pre-emphasised sine, its harmonics taken by the half-wave symmetry,
    /// each through `P⁻¹` at its own frequency. Harmonics past Nyquist are
    /// those of the alias. Off the audio thread.
    pub(crate) fn tone_harmonics(
        &self,
        freq_hz: f64,
        rate: f64,
        level: f64,
    ) -> crate::harmonics::ToneSpectrum {
        let mut out = [Complex64::new(0.0, 0.0); crate::harmonics::READOUT_LEN];
        if self.drive <= 0.0 || level <= 0.0 || !freq_hz.is_finite() || freq_hz <= 0.0 {
            return out;
        }
        let p1 = self.pre.response(freq_hz, rate);
        let pf = p1.norm();
        // The pre-emphasised sine is `level |P| cos(θ + φ)`; d is a function
        // of ψ = θ + φ, so harmonic k at θ picks up e^{j k φ}.
        let phase = p1 / pf;
        let w = 2.0 * PI * freq_hz / rate;
        let g_au = self.drive * level * pf;
        let m_full = (2 * CAL_HALF_POINTS) as f64;
        let mut acc = [Complex64::new(0.0, 0.0); crate::harmonics::READOUT_LEN];
        for i in 0..CAL_HALF_POINTS {
            let th = 2.0 * PI * i as f64 / m_full;
            let d = adaa1_d0_stateless(g_au * (th - w).cos(), g_au * th.cos()) * self.inv_drive;
            // Odd half-wave symmetry: only odd harmonics.
            for (k, a) in acc.iter_mut().enumerate().skip(1).step_by(2) {
                *a += d * Complex64::from_polar(1.0, -(k as f64) * th);
            }
        }
        let norm = 2.0 / CAL_HALF_POINTS as f64;
        for (k, o) in out.iter_mut().enumerate().skip(1).step_by(2) {
            let de = self.de.response(k as f64 * freq_hz, rate);
            *o = acc[k] * norm * phase.powi(k as i32) * de;
        }
        out
    }
}

fn validate_config(config: &FluxConfig, output_rate: f64) -> Result<(), FluxError> {
    if !config.corner_hz.is_finite()
        || !(FLUX_MIN_CORNER_HZ..=FLUX_MAX_CORNER_HZ).contains(&config.corner_hz)
        || config.corner_hz >= 0.45 * output_rate
    {
        return Err(FluxError::InvalidParameter(format!(
            "corner_hz must be in {FLUX_MIN_CORNER_HZ}..={FLUX_MAX_CORNER_HZ} Hz and below 0.45 * fs (got {})",
            config.corner_hz
        )));
    }
    if !config.max_boost_db.is_finite() || !(0.0..=FLUX_MAX_BOOST_DB).contains(&config.max_boost_db)
    {
        return Err(FluxError::InvalidParameter(format!(
            "max_boost_db must be in 0..={FLUX_MAX_BOOST_DB} dB (got {})",
            config.max_boost_db
        )));
    }
    if !config.target_h3_db.is_finite()
        || !(FLUX_MIN_H3_DB..=FLUX_MAX_H3_DB).contains(&config.target_h3_db)
    {
        return Err(FluxError::InvalidParameter(format!(
            "target_h3_db must be in {FLUX_MIN_H3_DB}..={FLUX_MAX_H3_DB} dBc (got {})",
            config.target_h3_db
        )));
    }
    if !config.calibration_hz.is_finite()
        || !(FLUX_MIN_CALIBRATION_HZ..=FLUX_MAX_CALIBRATION_HZ).contains(&config.calibration_hz)
        || 3.0 * config.calibration_hz >= 0.45 * output_rate
    {
        return Err(FluxError::InvalidParameter(format!(
            "calibration_hz must be in {FLUX_MIN_CALIBRATION_HZ}..={FLUX_MAX_CALIBRATION_HZ} Hz with 3 * calibration_hz < 0.45 * fs (got {})",
            config.calibration_hz
        )));
    }
    Ok(())
}

/// Filter states below this (`2^-100`, about -602 dBFS, as in `dc_block`)
/// are snapped to exact zero, so silence never leaves subnormal state.
const FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stage;
    use crate::harmonics::{Harmonics, HarmonicsError};
    use crate::loudness::STAGE_LOUDNESS_RATE;
    use audivane_protocol::{HarmonicsConfig, HarmonicsLayer};
    use rustfft::FftPlanner;

    /// A lone flux layer of `cfg` in the harmonics stage (as the layer
    /// ships), with its core for the core's own numbers.
    #[derive(Clone)]
    struct Lone {
        stage: Harmonics,
        core: FluxCore,
        enabled: bool,
        state: Vec<ChannelState>,
    }

    /// The harmonics config with `cfg` as its only layer.
    fn lone_config(cfg: &FluxConfig) -> HarmonicsConfig {
        HarmonicsConfig {
            enabled: true,
            layers: vec![HarmonicsLayer::Flux(FluxLayer {
                enabled: cfg.enabled,
                preset: None,
                corner_hz: cfg.corner_hz,
                max_boost_db: cfg.max_boost_db,
                target_h3_db: cfg.target_h3_db,
                calibration_hz: cfg.calibration_hz,
            })],
            ..Default::default()
        }
    }

    impl Lone {
        fn try_new(cfg: &FluxConfig, rate: f64, channels: usize) -> Result<Self, HarmonicsError> {
            let stage = Harmonics::try_new(&lone_config(cfg), rate, channels)?;
            let core = if cfg.enabled {
                FluxCore::try_new(cfg, rate).map_err(|e| HarmonicsError::Flux(0, e))?
            } else {
                FluxCore::OFF
            };
            Ok(Self {
                stage,
                core,
                enabled: cfg.enabled,
                state: vec![ChannelState::default(); channels],
            })
        }

        fn process(&mut self, chans: &mut [&mut [f64]]) {
            self.stage.process(chans);
        }

        fn reset(&mut self) {
            self.stage.reset();
        }

        fn drive(&self) -> f64 {
            self.core.drive
        }

        /// The core's certified peak bound (1 when disabled).
        fn peak_bound(&self) -> f64 {
            if self.enabled {
                self.core.peak_bound()
            } else {
                1.0
            }
        }

        fn peak_gain_db(&self) -> f64 {
            self.stage.peak_gain_db()
        }

        fn small_signal_gain_db(&self) -> f64 {
            self.stage.small_signal_gain_db()
        }

        fn loudness_change_db(cfg: &FluxConfig) -> Result<f64, HarmonicsError> {
            Harmonics::loudness_change_db(&lone_config(cfg))
        }

        /// The layer's arithmetic without ADAA1, for measuring aliasing
        /// suppression on identical filter coefficients and drive.
        fn process_without_adaa(&mut self, chans: &mut [&mut [f64]]) {
            if !self.enabled {
                return;
            }
            let pre = self.core.pre;
            let de = self.core.de;
            let g = self.core.drive;
            let inv_g = self.core.inv_drive;
            for (st, chan) in self.state.iter_mut().zip(chans.iter_mut()) {
                for s in chan.iter_mut() {
                    let x = if s.is_finite() { *s } else { 0.0 };
                    let xc = x.clamp(-1.0, 1.0);
                    let over = x - xc;
                    let u = pre.b0 * xc + st.pre_z1;
                    st.pre_z1 = pre.b1 * xc - pre.a1 * u;
                    let d = d0_tanh_diff(g * u) * inv_g;
                    let p_inv_d = de.b0 * d + st.de_z1;
                    st.de_z1 = de.b1 * d - de.a1 * p_inv_d;
                    *s = xc + p_inv_d + over;
                }
            }
        }
    }

    struct Measured {
        /// `[DC, H1, H2, H3, H4, H5]` linear amplitudes.
        amp: [f64; 6],
        peak: f64,
    }

    impl Measured {
        fn dbc(&self, k: usize) -> f64 {
            lin_to_db(self.amp[k] / self.amp[1])
        }
    }

    /// Measure a coherent sine at `target_hz` (exact to `< 0.002 Hz`) at peak
    /// amplitude `amp` after `settle_s` seconds of settling, processing in
    /// varying odd block sizes.
    fn measure_sine(
        cfg: &FluxConfig,
        rate: f64,
        target_hz: f64,
        amp: f64,
        settle_s: f64,
    ) -> Measured {
        let mut stage = Lone::try_new(cfg, rate, 1).unwrap();
        let settle = (rate * settle_s).ceil() as usize;
        // Pick an integer number of cycles spanning >= 0.5 s (and >= 10 cycles)
        // so the coherent bin frequency is within < 0.002 Hz of `target_hz`
        // (and exact for integer Hz at 44.1 / 48 / 192 / 768 kHz).
        let cycles = (target_hz * 0.5).round().max(10.0) as usize;
        let n = ((cycles as f64) * rate / target_hz).round() as usize;
        let f = cycles as f64 * rate / n as f64;
        let mut buf: Vec<f64> = (0..settle + n)
            .map(|i| amp * (2.0 * PI * f * i as f64 / rate).cos())
            .collect();
        let mut i = 0;
        let mut b = 1;
        while i < buf.len() {
            let e = (i + b).min(buf.len());
            stage.process(&mut [&mut buf[i..e]]);
            i = e;
            b = b % 1021 + 31;
        }
        let tail = &buf[settle..];
        let peak = tail.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
        // Exact coherent DFT at bins `k * cycles` for k = 0..6.
        let mut out = [0.0; 6];
        out[0] = tail.iter().sum::<f64>().abs() / n as f64;
        for (k, slot) in out.iter_mut().enumerate().skip(1) {
            let bin = cycles * k;
            if bin * 2 < n {
                let w = 2.0 * PI * bin as f64 / n as f64;
                let (mut re, mut im) = (0.0, 0.0);
                for (idx, &s) in tail.iter().enumerate() {
                    let (sin_a, cos_a) = (w * idx as f64).sin_cos();
                    re += s * cos_a;
                    im -= s * sin_a;
                }
                *slot = 2.0 * re.hypot(im) / n as f64;
            }
        }
        Measured { amp: out, peak }
    }

    #[test]
    fn shelf_and_inverse_cancel_to_machine_precision() {
        for rate in [44_100.0, 192_000.0, 768_000.0] {
            for (fc, boost) in [(20.0, 30.0), (100.0, 24.0), (500.0, 12.0), (100.0, 0.0)] {
                let (pre, de) = FirstOrder::leaky_integrator_pair(fc, boost, rate);
                for f in [0.0, 10.0, 40.0, fc, 1000.0, 10_000.0, 0.45 * rate] {
                    let prod = pre.response(f, rate) * de.response(f, rate);
                    assert!(
                        (prod - Complex64::new(1.0, 0.0)).norm() < 1e-11,
                        "rate={rate} fc={fc} boost={boost} f={f}: prod={prod}"
                    );
                }
                let dc_p = pre.response(0.0, rate).norm();
                let want_g = db_to_lin(boost);
                assert!(
                    (dc_p - want_g).abs() < 1e-9 * want_g,
                    "DC gain {dc_p} vs {want_g}"
                );
            }
        }
    }

    #[test]
    fn calibration_h3_matches_target_within_0_1_db() {
        let cases = [
            (44_100.0, 100.0, 24.0, 40.0, -40.0),
            (48_000.0, 100.0, 24.0, 40.0, -30.0),
            (48_000.0, 200.0, 30.0, 40.0, -40.0),
            (48_000.0, 100.0, 0.0, 40.0, -40.0),
            (192_000.0, 80.0, 18.0, 20.0, -50.0),
            (192_000.0, 150.0, 24.0, 50.0, -20.0),
            (768_000.0, 100.0, 24.0, 40.0, -40.0),
        ];
        for (rate, corner_hz, max_boost_db, calibration_hz, target_h3_db) in cases {
            let cfg = FluxConfig {
                enabled: true,
                corner_hz,
                max_boost_db,
                target_h3_db,
                calibration_hz,
            };
            let m = measure_sine(&cfg, rate, calibration_hz, 1.0, 0.6);
            let h3 = m.dbc(3);
            assert!(
                (h3 - target_h3_db).abs() < 0.1,
                "rate={rate} fc={corner_hz} boost={max_boost_db} cal={calibration_hz} target={target_h3_db}: got H3={h3:.4} dBc"
            );
            // Odd saturator: even harmonics and DC must stay at numerical noise floor.
            assert!(
                m.dbc(2) < -130.0,
                "unexpected H2={} dBc for {cfg:?}",
                m.dbc(2)
            );
            assert!(
                m.dbc(4) < -130.0,
                "unexpected H4={} dBc for {cfg:?}",
                m.dbc(4)
            );
            assert!(m.amp[0] < 1e-10, "unexpected DC={} for {cfg:?}", m.amp[0]);
        }
    }

    #[test]
    fn h3_rises_about_12_db_per_octave_below_corner() {
        let cfg = FluxConfig {
            enabled: true,
            corner_hz: 200.0,
            max_boost_db: 30.0,
            target_h3_db: -40.0,
            calibration_hz: 40.0,
        };
        let rate = 192_000.0;
        let h3_20 = measure_sine(&cfg, rate, 20.0, 1.0, 1.0).dbc(3);
        let h3_40 = measure_sine(&cfg, rate, 40.0, 1.0, 0.6).dbc(3);
        let h3_80 = measure_sine(&cfg, rate, 80.0, 1.0, 0.5).dbc(3);
        let h3_160 = measure_sine(&cfg, rate, 160.0, 1.0, 0.5).dbc(3);
        let h3_1000 = measure_sine(&cfg, rate, 1000.0, 1.0, 0.5).dbc(3);

        let slope_20_40 = h3_20 - h3_40;
        let slope_40_80 = h3_40 - h3_80;
        let slope_80_160 = h3_80 - h3_160;
        let avg_slope = (h3_20 - h3_160) / 3.0;

        assert!(
            (11.0..=13.5).contains(&slope_20_40),
            "20-40 Hz slope {slope_20_40:.2} dB/oct (h3: {h3_20:.2} -> {h3_40:.2})"
        );
        assert!(
            (11.0..=13.5).contains(&slope_40_80),
            "40-80 Hz slope {slope_40_80:.2} dB/oct (h3: {h3_40:.2} -> {h3_80:.2})"
        );
        assert!(
            (11.0..=13.5).contains(&slope_80_160),
            "80-160 Hz slope {slope_80_160:.2} dB/oct (h3: {h3_80:.2} -> {h3_160:.2})"
        );
        assert!(
            (avg_slope - 12.0).abs() < 0.5,
            "avg slope 20-160 Hz = {avg_slope:.2} dB/oct"
        );
        assert!(
            h3_1000 < h3_160 - 8.0,
            "H3 should drop further above corner: 160 Hz {h3_160:.2} vs 1 kHz {h3_1000:.2}"
        );
    }

    #[test]
    fn h3_rises_about_2_db_per_db_of_level() {
        let cfg = FluxConfig {
            enabled: true,
            corner_hz: 150.0,
            max_boost_db: 24.0,
            target_h3_db: -40.0,
            calibration_hz: 40.0,
        };
        let rate = 192_000.0;
        let levels_db = [0.0, -6.0, -12.0, -20.0, -30.0];
        let h3s: Vec<f64> = levels_db
            .iter()
            .map(|&lvl| measure_sine(&cfg, rate, 40.0, db_to_lin(lvl), 0.6).dbc(3))
            .collect();
        for w in 0..levels_db.len() - 1 {
            let dl = levels_db[w] - levels_db[w + 1];
            let dh = h3s[w] - h3s[w + 1];
            let slope = dh / dl;
            assert!(
                (slope - 2.0).abs() < 0.08,
                "level {} -> {} dBFS: H3 {:.2} -> {:.2} dBc, slope {:.3} dB/dB",
                levels_db[w],
                levels_db[w + 1],
                h3s[w],
                h3s[w + 1],
                slope
            );
        }
    }

    #[test]
    fn small_signal_sine_is_flat_across_frequency_to_0_01_db() {
        let cfg = FluxConfig {
            enabled: true,
            corner_hz: 150.0,
            max_boost_db: 24.0,
            target_h3_db: -25.0,
            calibration_hz: 40.0,
        };
        let amp = db_to_lin(-60.0);
        for rate in [44_100.0, 192_000.0] {
            let st = Lone::try_new(&cfg, rate, 1).unwrap();
            let want_gain_db = st.small_signal_gain_db();
            let mut min_g = f64::INFINITY;
            let mut max_g = f64::NEG_INFINITY;
            for f in [
                20.0, 40.0, 80.0, 150.0, 300.0, 1000.0, 4000.0, 10_000.0, 18_000.0,
            ] {
                let m = measure_sine(&cfg, rate, f, amp, 0.5);
                let gain_db = lin_to_db(m.amp[1] / amp);
                min_g = min_g.min(gain_db);
                max_g = max_g.max(gain_db);
                assert!(
                    (gain_db - want_gain_db).abs() < 0.01,
                    "rate={rate} f={f} Hz: gain {gain_db:.5} dB vs expected {want_gain_db:.5} dB"
                );
            }
            assert!(
                max_g - min_g < 0.01,
                "rate={rate}: small-signal ripple {:.5} dB exceeds 0.01 dB",
                max_g - min_g
            );
        }
    }

    #[test]
    fn output_stays_within_full_scale_for_sines_steps_and_low_freq_square() {
        let configs = [
            FluxConfig {
                enabled: true,
                corner_hz: 100.0,
                max_boost_db: 0.0,
                target_h3_db: -20.0,
                calibration_hz: 40.0,
            },
            FluxConfig {
                enabled: true,
                corner_hz: 100.0,
                max_boost_db: 12.0,
                target_h3_db: -30.0,
                calibration_hz: 40.0,
            },
            FluxConfig {
                enabled: true,
                corner_hz: 100.0,
                max_boost_db: 24.0,
                target_h3_db: -40.0,
                calibration_hz: 40.0,
            },
            FluxConfig {
                enabled: true,
                corner_hz: 200.0,
                max_boost_db: 30.0,
                target_h3_db: -20.0,
                calibration_hz: 40.0,
            },
            FluxConfig {
                enabled: true,
                corner_hz: 500.0,
                max_boost_db: 24.0,
                target_h3_db: -15.0,
                calibration_hz: 100.0,
            },
        ];
        let rate = 48_000.0;
        for cfg in &configs {
            // Nothing scales the output: it stays within the bound.
            let b = db_to_lin(Lone::try_new(cfg, rate, 1).unwrap().peak_gain_db());
            // 1. Full-scale sines across frequency.
            for f in [20.0, 40.0, 100.0, 1000.0, 10_000.0] {
                let m = measure_sine(cfg, rate, f, 1.0, 0.5);
                assert!(m.peak <= b, "{cfg:?} sine {f} Hz peak {} > {b}", m.peak);
            }

            // 2. Settled steps (+1 -> -1, -1 -> +1, 0 -> +1) and saturate-then-Nyquist pulse.
            for (from, to) in [(1.0, -1.0), (-1.0, 1.0), (0.0, 1.0), (0.0, -1.0)] {
                let mut st = Lone::try_new(cfg, rate, 1).unwrap();
                let mut buf = vec![from; (rate * 0.6) as usize];
                buf.extend(std::iter::repeat_n(to, 4096));
                st.process(&mut [&mut buf]);
                let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                assert!(peak <= b, "{cfg:?} step {from}->{to} peak {peak} > {b}");
            }
            {
                let mut st = Lone::try_new(cfg, rate, 1).unwrap();
                let mut buf = vec![1.0; (rate * 0.6) as usize];
                for i in 0..1024 {
                    buf.push(if i % 2 == 0 { -1.0 } else { 1.0 });
                }
                st.process(&mut [&mut buf]);
                let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                assert!(peak <= b, "{cfg:?} saturate-then-Nyquist peak {peak} > {b}");
            }

            // 3. Low-frequency full-scale square waves.
            for f_sq in [10.0, 20.0, 40.0, 100.0] {
                let mut st = Lone::try_new(cfg, rate, 1).unwrap();
                let period = (rate / f_sq).round() as usize;
                let half = period / 2;
                let mut buf: Vec<f64> = (0..(rate * 0.8) as usize)
                    .map(|i| if (i % period) < half { 1.0 } else { -1.0 })
                    .collect();
                st.process(&mut [&mut buf]);
                let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                assert!(peak <= b, "{cfg:?} square {f_sq} Hz peak {peak} > {b}");
            }
        }

        // Verify the certified bound is substantially tighter than the naive
        // `1 + ‖p⁻¹‖₁ |D(‖p‖₁)|` bound at high boost.
        let cfg = FluxConfig::default();
        let st = Lone::try_new(&cfg, 48_000.0, 1).unwrap();
        let u_max = st.core.pre.l1_norm();
        let naive =
            1.0 + st.core.de.l1_norm() * (-d0_tanh_diff(st.core.drive * u_max) / st.core.drive);
        assert!(
            st.peak_bound() < 0.3 * naive,
            "bound {} should beat naive {naive}",
            st.peak_bound()
        );
    }

    #[test]
    fn adaa_reduces_aliasing_at_44100_hz() {
        let rate = 44_100.0;
        let cfg = FluxConfig {
            enabled: true,
            corner_hz: 1000.0,
            max_boost_db: 18.0,
            target_h3_db: -20.0,
            calibration_hz: 200.0,
        };
        let n = 1usize << 13;
        let settle = 4096;
        // Odd bin near 4.5 kHz so all odd harmonics fold to distinct bins.
        let mut bin = ((4500.0 * n as f64) / rate).round() as usize;
        if bin.is_multiple_of(2) {
            bin += 1;
        }
        let f = bin as f64 * rate / n as f64;
        let input: Vec<f64> = (0..settle + n)
            .map(|i| (2.0 * PI * f * i as f64 / rate).cos())
            .collect();

        let alias_dbc = |use_adaa: bool| -> f64 {
            let mut st = Lone::try_new(&cfg, rate, 1).unwrap();
            let mut buf = input.clone();
            if use_adaa {
                st.process(&mut [&mut buf]);
            } else {
                st.process_without_adaa(&mut [&mut buf]);
            }
            let mut spec: Vec<Complex64> = buf[settle..]
                .iter()
                .map(|&s| Complex64::new(s, 0.0))
                .collect();
            FftPlanner::new().plan_fft_forward(n).process(&mut spec);
            let h1 = 2.0 * spec[bin].norm() / n as f64;
            // Sum all energy outside DC and the unaliased odd harmonics below Nyquist.
            let mut unaliased = vec![false; n / 2 + 1];
            unaliased[0] = true;
            let mut k = 1usize;
            while k * bin <= n / 2 {
                unaliased[k * bin] = true;
                k += 2;
            }
            let mut alias_pow = 0.0;
            for (b, c) in spec.iter().enumerate().take(n / 2 + 1) {
                if !unaliased[b] {
                    let a = 2.0 * c.norm() / n as f64;
                    alias_pow += a * a;
                }
            }
            lin_to_db(alias_pow.sqrt() / h1)
        };

        let no_adaa = alias_dbc(false);
        let with_adaa = alias_dbc(true);
        assert!(
            with_adaa < no_adaa - 5.0,
            "ADAA should cut aliasing at 44.1 kHz by > 5 dB: without={no_adaa:.2} dBc, with={with_adaa:.2} dBc"
        );
    }

    #[test]
    fn adaa_ill_conditioned_and_large_inputs_are_accurate_and_finite() {
        // Continuity across the midpoint fallback threshold and exact equality.
        for &t0 in &[
            0.0, 1e-12, 1e-6, 1e-3, 0.24, 0.26, 1.0, 10.0, 100.0, 1e6, -1e6,
        ] {
            let exact = d0_tanh_diff(t0);
            assert_eq!(adaa1_d0_stateless(t0, t0), exact);
            assert!(exact.is_finite());

            for &rel_dt in &[1e-12, 1e-8, 5e-6, 2e-5, 1e-3] {
                let dt = rel_dt * t0.abs().max(1e-6);
                let t1 = t0 + dt;
                let y = adaa1_d0_stateless(t0, t1);
                assert!(y.is_finite(), "non-finite for t0={t0}, t1={t1}");
                let mid = d0_tanh_diff(0.5 * t0 + 0.5 * t1);
                let tol = 1e-5 * mid.abs().max(1e-12) + 1e-10;
                assert!(
                    (y - mid).abs() < tol,
                    "t0={t0} t1={t1}: adaa={y} vs mid={mid}"
                );
            }
        }

        // Large inputs: finite, no overflow in cosh or t², matches sgn(t) - t.
        for &(t0, t1) in &[
            (100.0, 200.0),
            (1e6, 1e6 + 10.0),
            (1e12, 1e12 + 100.0),
            (-1e12, -1e12 - 100.0),
            (-1e6, 1e6),
            (1e100, 1e100 + 1e95),
        ] {
            let y = adaa1_d0_stateless(t0, t1);
            assert!(y.is_finite(), "large input ({t0}, {t1}) gave {y}");
        }
        // Symmetric large step [-L, +L] must integrate to exact 0.
        assert_eq!(adaa1_d0_stateless(-1e6, 1e6), 0.0);
        // Same-sign large step [100, 200]: tanh(t) == 1, so average of (1 - t) is 1 - 150 = -149.
        assert!((adaa1_d0_stateless(100.0, 200.0) + 149.0).abs() < 1e-12);
    }

    #[test]
    fn non_finite_input_does_not_poison_state() {
        let cfg = FluxConfig::default();
        let mut st = Lone::try_new(&cfg, 48_000.0, 1).unwrap();
        let mut fresh = st.clone();
        let mut a: Vec<f64> = (0..512).map(|i| (i as f64 * 0.05).sin() * 0.8).collect();
        let mut b = a.clone();
        a[7] = f64::NAN;
        a[9] = f64::NEG_INFINITY;
        a[11] = f64::INFINITY;
        b[7] = 0.0;
        b[9] = 0.0;
        b[11] = 0.0;
        st.process(&mut [&mut a]);
        fresh.process(&mut [&mut b]);
        assert!(a.iter().all(|x| x.is_finite()));
        assert_eq!(a, b);
    }

    #[test]
    fn silence_after_an_impulse_leaves_no_subnormal_state() {
        for rate in [48_000.0, 768_000.0] {
            let mut f = Lone::try_new(&FluxConfig::default(), rate, 1).unwrap();
            let mut buf = vec![0.0; 8192];
            buf[0] = 1.0;
            for _ in 0..(rate as usize * 40 / 8192) {
                f.process(&mut [&mut buf[..]]);
                buf.fill(0.0);
                let st = &f.state[0];
                for v in [st.pre_z1, st.de_z1, buf[8191]] {
                    assert!(!v.is_subnormal(), "subnormal {v:e} at {rate} Hz");
                }
            }
        }
    }

    #[test]
    fn overs_pass_with_unity_slope() {
        let cfg = FluxConfig::default();
        let mut st = Lone::try_new(&cfg, 48_000.0, 1).unwrap();
        let mut ref_st = st.clone();
        let mut a = vec![1.25, -1.4, 0.5];
        let mut b = vec![1.0, -1.0, 0.5];
        st.process(&mut [&mut a]);
        ref_st.process(&mut [&mut b]);
        assert!((a[0] - b[0] - 0.25).abs() < 1e-12);
        assert!((a[1] - b[1] + 0.4).abs() < 1e-12);
        // State after overs must match clamped state identically!
        assert!((a[2] - b[2]).abs() < 1e-15);
    }

    #[test]
    fn silence_outputs_exact_zero() {
        let cfg = FluxConfig::default();
        let mut st = Lone::try_new(&cfg, 192_000.0, 2).unwrap();
        let mut l = vec![0.0; 256];
        let mut r = vec![0.0; 256];
        st.process(&mut [&mut l, &mut r]);
        assert!(l.iter().all(|&x| x == 0.0));
        assert!(r.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn per_channel_state_is_independent_and_reset_clears_it() {
        let cfg = FluxConfig::default();
        let mut stereo = Lone::try_new(&cfg, 48_000.0, 2).unwrap();
        let mut mono_l = Lone::try_new(&cfg, 48_000.0, 1).unwrap();
        let mut mono_r = Lone::try_new(&cfg, 48_000.0, 1).unwrap();
        let orig_l: Vec<f64> = (0..512).map(|i| (i as f64 * 0.02).sin()).collect();
        let orig_r: Vec<f64> = (0..512).map(|i| 0.5 * (i as f64 * 0.07).cos()).collect();
        let (mut sl, mut sr) = (orig_l.clone(), orig_r.clone());
        let (mut ml, mut mr) = (orig_l.clone(), orig_r.clone());
        stereo.process(&mut [&mut sl, &mut sr]);
        mono_l.process(&mut [&mut ml]);
        mono_r.process(&mut [&mut mr]);
        assert_eq!(sl, ml);
        assert_eq!(sr, mr);

        stereo.reset();
        let (mut sl2, mut sr2) = (orig_l, orig_r);
        stereo.process(&mut [&mut sl2, &mut sr2]);
        assert_eq!(sl2, ml);
        assert_eq!(sr2, mr);
    }

    #[test]
    fn disabled_stage_is_passthrough_and_reports_zero_level_change() {
        let cfg = FluxConfig {
            enabled: false,
            ..Default::default()
        };
        let mut st = Lone::try_new(&cfg, 48_000.0, 1).unwrap();
        assert_eq!(st.peak_gain_db(), 0.0);
        assert_eq!(st.small_signal_gain_db(), 0.0);
        assert_eq!(Lone::loudness_change_db(&cfg).unwrap(), 0.0);
        let orig = vec![0.1, -0.7, 1.3];
        let mut buf = orig.clone();
        st.process(&mut [&mut buf]);
        assert_eq!(buf, orig);
    }

    #[test]
    fn loudness_change_db_is_the_compression_alone() {
        let cfg = FluxConfig {
            enabled: true,
            corner_hz: 100.0,
            max_boost_db: 12.0,
            target_h3_db: -40.0,
            calibration_hz: 40.0,
        };
        let st = Lone::try_new(&cfg, STAGE_LOUDNESS_RATE, 1).unwrap();
        let lufs_delta = Lone::loudness_change_db(&cfg).unwrap();
        // On -14 dBFS pink noise, nothing trims the level: the change is a
        // small compression shift, while the worst case goes over.
        assert!(
            lufs_delta.is_finite() && lufs_delta.abs() < 0.5,
            "{lufs_delta:.3}"
        );
        assert!(st.peak_gain_db() > 0.0, "{:.3}", st.peak_gain_db());
    }

    #[test]
    fn validation_rejects_invalid_configs() {
        let base = FluxConfig::default();
        assert_eq!(
            Lone::try_new(&base, 48_000.0, 0).err().unwrap(),
            HarmonicsError::ZeroChannels
        );
        assert_eq!(
            Lone::try_new(&base, 0.0, 2).err().unwrap(),
            HarmonicsError::InvalidSampleRate
        );
        assert_eq!(
            Lone::try_new(&base, f64::NAN, 2).err().unwrap(),
            HarmonicsError::InvalidSampleRate
        );
        for bad in [
            FluxConfig {
                corner_hz: 0.0,
                ..base.clone()
            },
            FluxConfig {
                corner_hz: 25_000.0,
                ..base.clone()
            },
            FluxConfig {
                max_boost_db: -1.0,
                ..base.clone()
            },
            FluxConfig {
                max_boost_db: 50.0,
                ..base.clone()
            },
            FluxConfig {
                target_h3_db: -130.0,
                ..base.clone()
            },
            FluxConfig {
                target_h3_db: -5.0,
                ..base.clone()
            },
            FluxConfig {
                calibration_hz: 0.0,
                ..base.clone()
            },
            FluxConfig {
                calibration_hz: 8000.0,
                ..base.clone()
            },
        ] {
            assert!(
                matches!(
                    Lone::try_new(&bad, 48_000.0, 1),
                    Err(HarmonicsError::Flux(0, FluxError::InvalidParameter(_)))
                ),
                "expected InvalidParameter for {bad:?}"
            );
        }
    }

    /// Prints measured calibration, frequency slope, level law, flatness, bounds
    /// and ADAA aliasing reduction.
    /// `cargo test -p audivane-dsp-core flux::tests::report -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn report() {
        let cfg = FluxConfig {
            enabled: true,
            corner_hz: 200.0,
            max_boost_db: 30.0,
            target_h3_db: -40.0,
            calibration_hz: 40.0,
        };
        let rate = 192_000.0;
        let st = Lone::try_new(&cfg, rate, 1).unwrap();
        let u_max = st.core.pre.l1_norm();
        let naive =
            1.0 + st.core.de.l1_norm() * (-d0_tanh_diff(st.core.drive * u_max) / st.core.drive);
        println!(
            "Flux fc={} Hz boost={} dB cal={} Hz @ {} dBc:",
            cfg.corner_hz, cfg.max_boost_db, cfg.calibration_hz, cfg.target_h3_db
        );
        println!(
            "  drive={:.6}, bound={:.4} (peak gain {:+.3} dB) vs naive={:.4} ({:+.3} dB), loudness_change={:+.3} dB",
            st.drive(),
            st.peak_bound(),
            st.peak_gain_db(),
            naive,
            lin_to_db(naive),
            Lone::loudness_change_db(&cfg).unwrap()
        );
        println!("  H3 vs frequency (0 dBFS, 192 kHz):");
        let mut prev: Option<(f64, f64)> = None;
        for f in [20.0, 40.0, 80.0, 160.0, 320.0, 640.0, 1000.0] {
            let h3 = measure_sine(&cfg, rate, f, 1.0, 0.8).dbc(3);
            let slope = prev
                .map(|(pf, ph)| format!(" ({:+.2} dB/oct)", (ph - h3) / (f / pf).log2()))
                .unwrap_or_default();
            println!("    {:6.1} Hz: H3 = {:7.3} dBc{slope}", f, h3);
            prev = Some((f, h3));
        }
        println!("  H3 vs level (40 Hz, 192 kHz):");
        for lvl in [0.0, -6.0, -12.0, -20.0, -30.0] {
            let h3 = measure_sine(&cfg, rate, 40.0, db_to_lin(lvl), 0.6).dbc(3);
            println!("    {:5.1} dBFS: H3 = {:7.3} dBc", lvl, h3);
        }

        // Calibration accuracy across rates/settings.
        let mut max_cal_err = 0.0_f64;
        for (r, fc, boost, cal, target) in [
            (44_100.0, 100.0, 24.0, 40.0, -40.0),
            (48_000.0, 100.0, 24.0, 40.0, -30.0),
            (192_000.0, 80.0, 18.0, 20.0, -50.0),
            (192_000.0, 150.0, 24.0, 50.0, -20.0),
            (768_000.0, 100.0, 24.0, 40.0, -40.0),
        ] {
            let c = FluxConfig {
                enabled: true,
                corner_hz: fc,
                max_boost_db: boost,
                target_h3_db: target,
                calibration_hz: cal,
            };
            let h3 = measure_sine(&c, r, cal, 1.0, 0.6).dbc(3);
            max_cal_err = max_cal_err.max((h3 - target).abs());
        }
        println!("  Max calibration error across 5 rates/configs: {max_cal_err:.6} dB");

        // Small-signal flatness (-60 dBFS, 20 Hz..18 kHz).
        let flat_cfg = FluxConfig {
            enabled: true,
            corner_hz: 150.0,
            max_boost_db: 24.0,
            target_h3_db: -25.0,
            calibration_hz: 40.0,
        };
        let amp = db_to_lin(-60.0);
        for r in [44_100.0, 192_000.0] {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for f in [20.0, 40.0, 80.0, 150.0, 1000.0, 10_000.0, 18_000.0] {
                let g = lin_to_db(measure_sine(&flat_cfg, r, f, amp, 0.5).amp[1] / amp);
                lo = lo.min(g);
                hi = hi.max(g);
            }
            println!(
                "  Small-signal (-60 dBFS) ripple at {r:.0} Hz: {:.6} dB",
                hi - lo
            );
        }

        // ADAA aliasing at 44.1 kHz (4.5 kHz sine).
        let alias_cfg = FluxConfig {
            enabled: true,
            corner_hz: 1000.0,
            max_boost_db: 18.0,
            target_h3_db: -20.0,
            calibration_hz: 200.0,
        };
        let r = 44_100.0;
        let n = 1usize << 13;
        let settle = 4096;
        let mut bin = ((4500.0 * n as f64) / r).round() as usize;
        if bin.is_multiple_of(2) {
            bin += 1;
        }
        let f = bin as f64 * r / n as f64;
        let input: Vec<f64> = (0..settle + n)
            .map(|i| (2.0 * PI * f * i as f64 / r).cos())
            .collect();
        let alias = |adaa: bool| -> f64 {
            let mut st = Lone::try_new(&alias_cfg, r, 1).unwrap();
            let mut buf = input.clone();
            if adaa {
                st.process(&mut [&mut buf]);
            } else {
                st.process_without_adaa(&mut [&mut buf]);
            }
            let mut spec: Vec<Complex64> = buf[settle..]
                .iter()
                .map(|&s| Complex64::new(s, 0.0))
                .collect();
            FftPlanner::new().plan_fft_forward(n).process(&mut spec);
            let h1 = 2.0 * spec[bin].norm() / n as f64;
            let mut unaliased = vec![false; n / 2 + 1];
            unaliased[0] = true;
            let mut k = 1;
            while k * bin <= n / 2 {
                unaliased[k * bin] = true;
                k += 2;
            }
            let p: f64 = spec
                .iter()
                .enumerate()
                .take(n / 2 + 1)
                .filter(|(b, _)| !unaliased[*b])
                .map(|(_, c)| (2.0 * c.norm() / n as f64).powi(2))
                .sum();
            lin_to_db(p.sqrt() / h1)
        };
        let no_a = alias(false);
        let wi_a = alias(true);
        println!(
            "  Aliasing at 44.1 kHz (4.5 kHz sine): no ADAA = {no_a:.2} dBc, with ADAA = {wi_a:.2} dBc (reduction {:.2} dB)",
            no_a - wi_a
        );
    }
}
