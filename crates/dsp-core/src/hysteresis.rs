//! Tape magnetic hysteresis (Jiles-Atherton model): the core of a tape layer
//! of the harmonics stage ([`crate::harmonics`]).
//!
//! Re-derived from J. Chowdhury, *"Real-time physical modelling for analog tape
//! machines"* (DAFx-19), in dimensionless magnetization `m = M / M_s ∈ (-1, 1)`
//! and normalized applied field `h = H / a = g · x`:
//!
//! ```text
//! q      = h + α̃ m
//! m_an   = L(q) = coth(q) − 1/q
//! Δm     = m_an − m
//! δ      = sgn(dh)
//! δ_M    = 1  if δ · Δm > 0, else 0
//! χ_q    = c · L'(q) + δ_M · (Δm / (δ k̃))
//! dm/dh  = χ_q / (1 − α̃ χ_q)
//! ```
//!
//! Using the consistent chain-rule form `dm/dq = c L'(q) + δ_M (m_an − m)/(δ k̃)`
//! with `dq = dh + α̃ dm` avoids the `(1 − c) k̃ − α̃ |Δm|` singularity near
//! `c → 1` on discrete full-scale steps while keeping identical initial
//! susceptibility `χ₀ = (c / 3) / (1 − c α̃ / 3)`, identical Rayleigh low-field
//! hysteresis, and identical `c = 0` pinning dynamics.
//!
//! Over each sample interval `[n − 1, n]`, the applied field ramps linearly
//! `h(s) = h[n − 1] + s · Δh` (`Δh = h[n] − h[n − 1]`, constant sample-difference
//! derivative `dh/dt = Δh / T`), so the sample period cancels out of
//! `dm/ds = (dm/dh) · Δh`. Integration uses one classical 4th-order Runge-Kutta
//! (RK4) step per sample whenever `|Δh| <= min(0.5 k̃, 0.75)` (which covers
//! smooth audio across 44.1–768 kHz); larger discontinuous jumps (such as
//! full-scale digital steps or square waves at high drive) automatically
//! subdivide the linear ramp into equal RK4 substeps so the integrator remains
//! strictly stable and bounded for every input.
//!
//! Distortion-only output structure:
//!
//! ```text
//! z[n] = P(x_c)[n]                         core input (P = 1 without the bass EQ)
//! h[n] = g · z[n] + h_b                    applied field, h_b = asymmetry (knee units)
//! u[n] = (m[n] − m_b) / (χ_b g)
//! d[n] = clamp_D(u[n] − z[n])              residual against the core's own input
//! p[n] = clamp_Y,Q(amount · P⁻¹(HPF₅Hz(d))[n])
//! y[n] = s · (x_c[n] + p[n]) + s · (x[n] − x_c[n])
//! ```
//!
//! where `x_c = clamp(x, -1, 1)`. Without a field offset `m_b = 0` and
//! `χ_b = χ₀`; with one, `(h_b, m_b)` is the core's rest point (the
//! anhysteretic magnetization at the offset: where an AC bias field leaves
//! the tape) and `χ_b` its small-signal susceptibility, so the state starts
//! there (stored relative to it: silence stays exact zero, no DC step) and
//! the offset adds even orders with a phase that holds across levels. Because
//! `u[n] = z[n] + O(z²)` as `z → 0`, the residual has zero linear term: small
//! signals pass flat from 20 Hz to 20 kHz through the unfiltered direct path
//! with gain `s`, whatever `P` is. The **bass EQ pair** is a record/playback
//! pair: a 12 dB/oct low shelf `P` (unity at 1 kHz) in front of the core and
//! its exact inverse on the residual, so only the distortion sees it (bass
//! tones drive the core harder, harmonic `k` leaves through `P⁻¹(k f)`). The
//! 5 Hz first-order Direct Form I high-pass filter sits only on `d[n]` so
//! magnetic remanence after asymmetric transients (and the even orders' DC)
//! decays cleanly to exact zero during silence without altering the music's
//! frequency response.
//!
//! **Calibration.** The drive `g` is solved on the symmetric core
//! (`h_b = 0`): a 0 dBFS sine at the calibration frequency (probed at most
//! at 0.16 of the rate) gives the target H3. The offset then goes on in
//! absolute field units, `h_b = asymmetry` with the Langevin knee (`h = 1`,
//! where `L` has bent to 0.31 of saturation) as the unit, so its size
//! against the loop does not move with the drive and the drive does not move
//! with the offset. A layer is refused at build (and by the daemon, at every
//! engine rate) when a full-scale jump would need more than 4 × 128 stable
//! RK4 steps (each sample takes at most 128 substeps, so they are at most 4×
//! the stable step, still inside RK4's stability region), or when the offset
//! makes it expand: the calibration tone's fundamental more than 1 dB above
//! the small-signal gain at any level from −40 to 0 dBFS (an offset deep in
//! saturation, where the loop's slope grows with level).
//!
//! **Peak bound.** At build the layer's whole path, `p` as above (EQ, core,
//! residual clamp, blocker, inverse EQ, amount), runs from rest on probe
//! signals (`path_probe`: full-scale sines from 20 Hz to 20 kHz and
//! −6 dBFS bass sines, full-scale squares, held steps between the extremes,
//! random ramps, jumps and holds, and a search for the path's worst case:
//! held at an extreme, a swing toward the other and part of the way back
//! within a few milliseconds, and a jump back, where the inverse EQ
//! overshoots; probed at 48 kHz at most (above it, on the layer calibrated
//! there: the loop is rate independent, and so are the probed values to
//! 0.1 %)). The runtime then holds
//!
//! ```text
//! |p[n]| <= Y = min(1.5 · max |p|, amount · ‖p⁻¹‖₁ · 2 b₀ D),
//! |x_c[n] + p[n]| <= Q = max(1, max |x_c + 1.1 · p|)
//! ```
//!
//! for every input by clamping `p` to `[max(−Q − x_c, −Y), min(Q − x_c, Y)]`
//! (an interval holding 0, since `Q >= 1 >= |x_c|`), so the bound holds by
//! construction: `|x_c + p| <= min(Q, 1 + Y) = B`, and scaling by
//! `s = min(1, 1 / B)` (with a 1 ppm margin) keeps full-scale inputs within
//! `[-1, 1]`. The second term of `Y` is certified on its own: the blocker
//! `HPF(d) = b₀ (d − e)` removes an estimate `e` that is a convex mix of
//! past residuals (`e[n] = (1 + a₁) d[n−1] − a₁ e[n−1]`, `−a₁ ∈ (0, 1)`), so
//! `|HPF(d)| <= 2 b₀ D`. `Q`'s margin is small because the probe searches
//! for the worst case: random ramps, jumps and holds reach 1/1.5–1/1.7 of
//! its excess over full scale. Real signals never touch the clamps (checked
//! on random ramps, jumps and holds, the readout tones and music-like noise,
//! at 44.1, 96 and 384 kHz). In a harmonics stage the two inequalities are
//! the splits `(0, Y)` and `(1, Q)` of the path (`HysteresisCore::split`).
//! The residual's own clamp `|d| <= D` (1.5× the largest residual
//! `residual_probe` finds on the first magnetization curve, the major loop,
//! and jumps and reversal ramps from states on it, at most the analytic
//! bound from `|m| <= m*`, `m*` the fixed point of
//! `m* = L(g ‖p‖₁ + |h_b| + α̃ m*)`) keeps the blocker's state bounded.
//! Inter-sample overs pass at unity slope times `s`, matching
//! [`crate::harmonics::Harmonics`].

use crate::harmonics_layer::Split;
use crate::harmonics_tilt::{TiltFilter, TiltState};
use crate::{Complex64, db_to_lin, lin_to_db};
pub use audivane_protocol::{
    HYSTERESIS_MAX_ASYMMETRY, HYSTERESIS_MAX_CALIBRATION_HZ, HYSTERESIS_MAX_EQ_DB,
    HYSTERESIS_MAX_EQ_HZ, HYSTERESIS_MAX_H3_DB, HYSTERESIS_MIN_CALIBRATION_HZ,
    HYSTERESIS_MIN_CLEAN_RATE_HZ, HYSTERESIS_MIN_EQ_HZ, HYSTERESIS_MIN_H3_DB,
};
use audivane_protocol::{HysteresisLayer, OrderTilt, TiltShelf};
use std::f64::consts::PI;
use std::fmt;

/// Dimensionless mean-field inter-domain coupling `α̃ = α M_s / a`, matching
/// typical ferric oxide tape (~`5e-3`). Keeps `1 − α̃ χ_q > 0.97` unconditionally
/// across the full parameter range.
const ALPHA_TILDE: f64 = 5.0e-3;

/// First-order DC blocker corner frequency in Hz on the distortion residual.
const DC_BLOCK_HZ: f64 = 5.0;

/// Minimum dimensionless pinning field `k̃` (`saturation = 0.0`).
const K_TILDE_MIN: f64 = 0.5;
/// Ratio `k̃_max / k̃_min` across `saturation ∈ [0, 1]` (`0.5 ..= 8.0`).
const K_TILDE_SPAN: f64 = 16.0;
/// Reversibility span: `c = 1.0 - C_SPAN * width` (`width = 0 -> c = 1.0`,
/// `width = 1 -> c = 0.5`).
const C_SPAN: f64 = 0.5;

/// Maximum RK4 step in dimensionless field `h` relative to the Langevin knee
/// scale (`1.0`). Combined with `0.5 * k̃` to stay well inside RK4's stability
/// region (`|λ| <= 2.785`) and accurately resolve the Langevin curvature.
const MAX_LANGEVIN_STEP_DH: f64 = 0.75;

/// State flush threshold (`2^-100`, about -602 dBFS, matching `dc_block.rs`
/// and `flux.rs`), preventing IEEE-754 subnormals during silence.
const FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31;

/// Switch from Padé `[5/5]` rational evaluation of `L(q)` and `L'(q)` to the
/// exact `exp(-2|q|)` branch above this `|q|`. Across `|q| <= 3.0`, Padé `[5/5]`
/// matches `L(q)` to `< 5e-15` and `L'(q)` to `< 4e-14` while avoiding `exp()`
/// on all typical audio signals.
const LANGEVIN_PADE_THRESHOLD: f64 = 3.0;
/// Above this `|q|`, `exp(-2|q|) < 4.3e-18`, so `coth(q) = sgn(q)` and
/// `csch²(q) = 0` to `f64` precision (and avoids subnormal `exp` outputs).
const LANGEVIN_ASYMPTOTIC_THRESHOLD: f64 = 20.0;

/// Exact `[5/5]` Padé approximant numerator coefficients of `L(q) / q` in
/// `x = q²`: `L(q) / q ≈ P(x) / Q(x)` with `P(x) = sum_{k=0..=5} a_k x^k`.
const L_PADE_A: [f64; 6] = [
    1.0 / 3.0,
    2.608_695_652_173_913e-2,
    5.521_048_999_309_869e-4,
    4.035_854_531_659_261e-6,
    9.249_475_626_415_419e-9,
    3.162_213_889_372_792_6e-12,
];

/// Exact `[5/5]` Padé approximant denominator coefficients of `L(q) / q` in
/// `x = q²`: `Q(x) = 1 + sum_{k=1..=5} b_k x^k`.
const L_PADE_B: [f64; 6] = [
    1.0,
    1.449_275_362_318_840_6e-1,
    4.968_944_099_378_882e-3,
    5.811_630_525_589_335_4e-5,
    2.374_032_077_446_624e-7,
    2.466_526_833_710_778e-10,
];

/// Minimum periodic samples per evaluation in build-time calibration.
const CAL_MIN_POINTS: usize = 256;
/// Highest calibration probe frequency, as a fraction of the output rate
/// (its H3 stays below Nyquist): a higher `calibration_hz` is calibrated
/// here.
const CAL_MAX_PROBE: f64 = 0.16;

/// The residual bound is the probed largest residual times this, when
/// that is below the analytic bound ([`residual_probe`]); the output and DC
/// estimate bounds add this fraction of it (`(PROBE_MARGIN − 1) ×` the
/// probed residual) to what the probe reached.
const PROBE_MARGIN: f64 = 1.5;
/// States on the probe's last major-loop turn that it jumps from.
const PROBE_STATES: usize = 16;
/// The path's output clamp `Q` holds `|x_c + p| <= Q` with `p` this much
/// above what [`path_probe`] reaches: smaller than [`PROBE_MARGIN`], since
/// the probe includes a search for the path's worst case (which reaches
/// 1.5–1.7× the excess over full scale random ramps, jumps and holds do).
const WORST_CASE_MARGIN: f64 = 1.1;
/// The field offset's unit: `asymmetry = 1` offsets the core's field by
/// the Langevin knee (`h = 1`, where `L` has bent to 0.31 of saturation),
/// whatever the drive.
const ASYMMETRY_FIELD: f64 = 1.0;
/// Most RK4 substeps one sample takes; a jump needing more takes this many
/// longer ones, at most [`STEP_STRETCH`] times the stable step (configs that
/// would need longer ones are refused).
const MAX_SUBSTEPS: usize = 128;
const STEP_STRETCH: f64 = 4.0;
/// A layer whose fundamental at the calibration frequency rises more than
/// this above its small-signal gain at any of these levels expands (a field
/// offset deep in saturation): refused.
const MAX_EXPANSION_DB: f64 = 1.0;
const EXPANSION_LEVELS_DB: [f64; 6] = [-40.0, -30.0, -20.0, -12.0, -6.0, 0.0];
/// Highest rate the layer's path is probed at ([`path_probe`]); above it
/// the probe runs on the layer calibrated there (the loop is rate
/// independent; this keeps the build short and the clamps the same at
/// every higher rate).
const PATH_PROBE_MAX_RATE: f64 = 48_000.0;

/// Why a tape layer's core could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HysteresisError {
    InvalidSampleRate,
    InvalidParameter(String),
    Uncalibratable(String),
}

impl fmt::Display for HysteresisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => {
                write!(f, "hysteresis requires a positive finite sample rate")
            }
            Self::InvalidParameter(msg) => write!(f, "hysteresis: {msg}"),
            Self::Uncalibratable(msg) => write!(f, "hysteresis calibration failed: {msg}"),
        }
    }
}

impl std::error::Error for HysteresisError {}

/// Configuration of a tape layer's Jiles-Atherton core.
#[derive(Clone, Debug, PartialEq)]
pub struct HysteresisConfig {
    /// Whether the stage processes audio.
    pub enabled: bool,
    /// Saturation character in `0.0..=1.0`. Lower values emphasize Rayleigh
    /// pinning hysteresis across all levels; higher values raise the pinning
    /// scale so calibration drives the core deeper into the soft Langevin
    /// saturation knee near 0 dBFS.
    pub saturation: f64,
    /// Hysteresis loop width in `0.0..=1.0`. `0.0` is pure anhysteretic
    /// Langevin saturation (zero loop area); `1.0` gives a wide magnetic loop
    /// (`c = 0.5`).
    pub width: f64,
    /// Distortion-only wet amount in `0.0..=1.0`. `0.0` is clean bypass; `1.0`
    /// applies the full calibrated distortion.
    pub amount: f64,
    /// Target 3rd harmonic level in dBc for a 0 dBFS sine at `calibration_hz`
    /// (at `amount = 1.0`).
    pub target_h3_db: f64,
    /// Reference sine frequency in Hz for `target_h3_db`.
    pub calibration_hz: f64,
    /// Field offset in the Langevin knee's units (`h = 1`), whatever the
    /// drive (`-1.0..=1.0`, [`HYSTERESIS_MAX_ASYMMETRY`]; 0: symmetric).
    /// Adds even orders; the drive is calibrated without it.
    pub asymmetry: f64,
    /// Bass EQ pair `(corner Hz, boost dB)` around the core (`None`: off).
    pub bass_eq: Option<(f64, f64)>,
}

impl Default for HysteresisConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            saturation: 0.5,
            width: 0.5,
            amount: 1.0,
            target_h3_db: -40.0,
            calibration_hz: 1_000.0,
            asymmetry: 0.0,
            bass_eq: None,
        }
    }
}

impl HysteresisConfig {
    pub fn is_active(&self) -> bool {
        self.enabled && self.amount > 0.0
    }
}

impl From<&HysteresisLayer> for HysteresisConfig {
    fn from(l: &HysteresisLayer) -> Self {
        Self {
            enabled: l.enabled,
            saturation: l.saturation,
            width: l.width,
            amount: l.amount,
            target_h3_db: l.target_h3_db,
            calibration_hz: l.calibration_hz,
            asymmetry: l.asymmetry(),
            bass_eq: l.active_bass_eq().map(|e| (e.hz, e.db)),
        }
    }
}

/// Cold path for `|q| > LANGEVIN_PADE_THRESHOLD`, kept out-of-line so the hot
/// Padé path has no `exp()` call clobbering SIMD registers.
#[cold]
#[inline(never)]
fn langevin_and_deriv_large(q: f64, aq: f64) -> (f64, f64) {
    if aq >= LANGEVIN_ASYMPTOTIC_THRESHOLD {
        let inv_q = 1.0 / q;
        let l = 1.0_f64.copysign(q) - inv_q;
        let lp = inv_q * inv_q;
        (l, lp)
    } else {
        let e = (-2.0 * aq).exp();
        let inv_1me = 1.0 / (1.0 - e);
        let coth_aq = (1.0 + e) * inv_1me;
        let inv_q = 1.0 / q;
        let l = coth_aq.copysign(q) - inv_q;
        let lp = inv_q * inv_q - 4.0 * e * (inv_1me * inv_1me);
        (l, lp)
    }
}

/// Pure rational Padé `[5/5]` evaluation of `(L(q), L'(q))` for `|q| <= 3.0`.
#[inline(always)]
fn langevin_and_deriv_pade(q: f64) -> (f64, f64) {
    let x = q * q;
    let x2 = x * x;
    let x4 = x2 * x2;

    let p01 = L_PADE_A[1] * x + L_PADE_A[0];
    let p23 = L_PADE_A[3] * x + L_PADE_A[2];
    let p45 = L_PADE_A[5] * x + L_PADE_A[4];
    let p = p45 * x4 + (p23 * x2 + p01);

    let d01 = L_PADE_B[1] * x + L_PADE_B[0];
    let d23 = L_PADE_B[3] * x + L_PADE_B[2];
    let d45 = L_PADE_B[5] * x + L_PADE_B[4];
    let d = d45 * x4 + (d23 * x2 + d01);

    let pp01 = (2.0 * L_PADE_A[2]) * x + L_PADE_A[1];
    let pp23 = (4.0 * L_PADE_A[4]) * x + 3.0 * L_PADE_A[3];
    let pp = (5.0 * L_PADE_A[5]) * x4 + (pp23 * x2 + pp01);

    let dp01 = (2.0 * L_PADE_B[2]) * x + L_PADE_B[1];
    let dp23 = (4.0 * L_PADE_B[4]) * x + 3.0 * L_PADE_B[3];
    let dp = (5.0 * L_PADE_B[5]) * x4 + (dp23 * x2 + dp01);

    let inv_d = 1.0 / d;
    let f = p * inv_d;
    let l = q * f;
    let lp = f + (2.0 * x * inv_d) * (pp - f * dp);
    (l, lp)
}

/// Pure rational Padé `[5/5]` evaluation for two independent channels in
/// lockstep, compiling to packed 2-lane SSE2/NEON `f64x2` instructions while
/// staying bit-for-bit identical to [`langevin_and_deriv_pade`].
#[inline(always)]
fn langevin_and_deriv_pade_pair(q: [f64; 2]) -> ([f64; 2], [f64; 2]) {
    let x = [q[0] * q[0], q[1] * q[1]];
    let x2 = [x[0] * x[0], x[1] * x[1]];
    let x4 = [x2[0] * x2[0], x2[1] * x2[1]];

    let p01 = [
        L_PADE_A[1] * x[0] + L_PADE_A[0],
        L_PADE_A[1] * x[1] + L_PADE_A[0],
    ];
    let p23 = [
        L_PADE_A[3] * x[0] + L_PADE_A[2],
        L_PADE_A[3] * x[1] + L_PADE_A[2],
    ];
    let p45 = [
        L_PADE_A[5] * x[0] + L_PADE_A[4],
        L_PADE_A[5] * x[1] + L_PADE_A[4],
    ];
    let p = [
        p45[0] * x4[0] + (p23[0] * x2[0] + p01[0]),
        p45[1] * x4[1] + (p23[1] * x2[1] + p01[1]),
    ];

    let d01 = [
        L_PADE_B[1] * x[0] + L_PADE_B[0],
        L_PADE_B[1] * x[1] + L_PADE_B[0],
    ];
    let d23 = [
        L_PADE_B[3] * x[0] + L_PADE_B[2],
        L_PADE_B[3] * x[1] + L_PADE_B[2],
    ];
    let d45 = [
        L_PADE_B[5] * x[0] + L_PADE_B[4],
        L_PADE_B[5] * x[1] + L_PADE_B[4],
    ];
    let d = [
        d45[0] * x4[0] + (d23[0] * x2[0] + d01[0]),
        d45[1] * x4[1] + (d23[1] * x2[1] + d01[1]),
    ];

    let pp01 = [
        (2.0 * L_PADE_A[2]) * x[0] + L_PADE_A[1],
        (2.0 * L_PADE_A[2]) * x[1] + L_PADE_A[1],
    ];
    let pp23 = [
        (4.0 * L_PADE_A[4]) * x[0] + 3.0 * L_PADE_A[3],
        (4.0 * L_PADE_A[4]) * x[1] + 3.0 * L_PADE_A[3],
    ];
    let pp = [
        (5.0 * L_PADE_A[5]) * x4[0] + (pp23[0] * x2[0] + pp01[0]),
        (5.0 * L_PADE_A[5]) * x4[1] + (pp23[1] * x2[1] + pp01[1]),
    ];

    let dp01 = [
        (2.0 * L_PADE_B[2]) * x[0] + L_PADE_B[1],
        (2.0 * L_PADE_B[2]) * x[1] + L_PADE_B[1],
    ];
    let dp23 = [
        (4.0 * L_PADE_B[4]) * x[0] + 3.0 * L_PADE_B[3],
        (4.0 * L_PADE_B[4]) * x[1] + 3.0 * L_PADE_B[3],
    ];
    let dp = [
        (5.0 * L_PADE_B[5]) * x4[0] + (dp23[0] * x2[0] + dp01[0]),
        (5.0 * L_PADE_B[5]) * x4[1] + (dp23[1] * x2[1] + dp01[1]),
    ];

    let inv_d = [1.0 / d[0], 1.0 / d[1]];
    let f = [p[0] * inv_d[0], p[1] * inv_d[1]];
    let l = [q[0] * f[0], q[1] * f[1]];
    let lp = [
        f[0] + (2.0 * x[0] * inv_d[0]) * (pp[0] - f[0] * dp[0]),
        f[1] + (2.0 * x[1] * inv_d[1]) * (pp[1] - f[1] * dp[1]),
    ];
    (l, lp)
}

#[cold]
#[inline(never)]
fn langevin_and_deriv_pair_fallback(q: [f64; 2]) -> ([f64; 2], [f64; 2]) {
    let (l0, lp0) = langevin_and_deriv(q[0]);
    let (l1, lp1) = langevin_and_deriv(q[1]);
    ([l0, l1], [lp0, lp1])
}

#[inline(always)]
fn langevin_and_deriv_pair(q: [f64; 2]) -> ([f64; 2], [f64; 2]) {
    if q[0].abs() <= LANGEVIN_PADE_THRESHOLD && q[1].abs() <= LANGEVIN_PADE_THRESHOLD {
        langevin_and_deriv_pade_pair(q)
    } else {
        langevin_and_deriv_pair_fallback(q)
    }
}

/// Evaluate the Langevin function `L(q) = coth(q) - 1/q` and its derivative
/// `L'(q) = 1/q² - csch²(q)` accurately without overflow, cancellation, or
/// subnormal intermediates for any finite `q`.
#[inline(always)]
fn langevin_and_deriv(q: f64) -> (f64, f64) {
    let aq = q.abs();
    if aq <= LANGEVIN_PADE_THRESHOLD {
        langevin_and_deriv_pade(q)
    } else {
        langevin_and_deriv_large(q, aq)
    }
}

/// Dimensionless Jiles-Atherton parameters shared by the runtime step and
/// build-time calibration solver.
#[derive(Clone, Copy, Debug)]
struct JaParams {
    /// Reversibility ratio `c ∈ [0.5, 1.0]`.
    c: f64,
    /// Reciprocal of dimensionless pinning field `1 / k̃`.
    inv_k: f64,
    /// Maximum single RK4 step in dimensionless field `h`.
    max_step_dh: f64,
    /// Initial normal susceptibility `χ₀ = (c / 3) / (1 − c α̃ / 3)`.
    chi0: f64,
}

impl JaParams {
    fn from_config(config: &HysteresisConfig) -> Self {
        let c = 1.0 - C_SPAN * config.width;
        let k_tilde = K_TILDE_MIN * K_TILDE_SPAN.powf(config.saturation);
        let max_step_dh = (0.5 * k_tilde).min(MAX_LANGEVIN_STEP_DH);
        let chi0 = (c / 3.0) / (1.0 - c * ALPHA_TILDE / 3.0);
        Self {
            c,
            inv_k: 1.0 / k_tilde,
            max_step_dh,
            chi0,
        }
    }

    /// Evaluate `Δh · (dm / dh)` at `(h, m)` for field step `dh` with direction
    /// `delta = sgn(dh) ∈ {-1, +1}`. Branchless on `δ_M` and division-free on
    /// `1 / (1 − α̃ χ_q)` (since `u = α̃ χ_q <= 0.022`, `(1+u)(1+u²)(1+u⁴)` matches
    /// `1/(1-u)` to `< 1e-13` worst-case and `< 1e-16` on normal audio).
    #[inline(always)]
    fn rhs(&self, h: f64, m: f64, dh: f64, delta: f64) -> f64 {
        let q = h + ALPHA_TILDE * m;
        let (l, lp) = langevin_and_deriv(q);
        let irrev = (delta * (l - m)).max(0.0) * self.inv_k;
        let chi_q = self.c * lp + irrev;
        let u = ALPHA_TILDE * chi_q;
        let u2 = u * u;
        let inv_denom = (1.0 + u) * (1.0 + u2) * (1.0 + u2 * u2);
        dh * chi_q * inv_denom
    }

    /// Two-lane lockstep evaluation of [`Self::rhs`], bit-identical per lane.
    #[inline(always)]
    fn rhs_pair(&self, h: [f64; 2], m: [f64; 2], dh: [f64; 2], delta: [f64; 2]) -> [f64; 2] {
        let q = [h[0] + ALPHA_TILDE * m[0], h[1] + ALPHA_TILDE * m[1]];
        let (l, lp) = langevin_and_deriv_pair(q);
        let irrev = [
            (delta[0] * (l[0] - m[0])).max(0.0) * self.inv_k,
            (delta[1] * (l[1] - m[1])).max(0.0) * self.inv_k,
        ];
        let chi_q = [self.c * lp[0] + irrev[0], self.c * lp[1] + irrev[1]];
        let u = [ALPHA_TILDE * chi_q[0], ALPHA_TILDE * chi_q[1]];
        let u2 = [u[0] * u[0], u[1] * u[1]];
        let inv_denom = [
            (1.0 + u[0]) * (1.0 + u2[0]) * (1.0 + u2[0] * u2[0]),
            (1.0 + u[1]) * (1.0 + u2[1]) * (1.0 + u2[1] * u2[1]),
        ];
        [
            dh[0] * chi_q[0] * inv_denom[0],
            dh[1] * chi_q[1] * inv_denom[1],
        ]
    }

    /// One classical RK4 step across `[h0, h0 + dh]` with fixed direction `delta`.
    #[inline(always)]
    fn rk4_substep(&self, h0: f64, m: f64, dh: f64, delta: f64) -> f64 {
        let h_mid = h0 + 0.5 * dh;
        let h_end = h0 + dh;
        let k1 = self.rhs(h0, m, dh, delta);
        let k2 = self.rhs(h_mid, m + 0.5 * k1, dh, delta);
        let k3 = self.rhs(h_mid, m + 0.5 * k2, dh, delta);
        let k4 = self.rhs(h_end, m + k3, dh, delta);
        m + (k1 + 2.0 * (k2 + k3) + k4) * (1.0 / 6.0)
    }

    /// Two-lane lockstep RK4 step on the hot path, falling back per-lane if
    /// either channel takes a discontinuous jump `|dh| > max_step_dh`.
    #[inline(always)]
    fn step_rk4_pair(&self, h0: [f64; 2], h1: [f64; 2], m: [f64; 2], m_star: f64) -> [f64; 2] {
        let dh = [h1[0] - h0[0], h1[1] - h0[1]];
        if dh[0].abs() <= self.max_step_dh && dh[1].abs() <= self.max_step_dh {
            let delta = [1.0_f64.copysign(dh[0]), 1.0_f64.copysign(dh[1])];
            let h_mid = [h0[0] + 0.5 * dh[0], h0[1] + 0.5 * dh[1]];
            let h_end = [h0[0] + dh[0], h0[1] + dh[1]];
            let k1 = self.rhs_pair(h0, m, dh, delta);
            let k2 = self.rhs_pair(h_mid, [m[0] + 0.5 * k1[0], m[1] + 0.5 * k1[1]], dh, delta);
            let k3 = self.rhs_pair(h_mid, [m[0] + 0.5 * k2[0], m[1] + 0.5 * k2[1]], dh, delta);
            let k4 = self.rhs_pair(h_end, [m[0] + k3[0], m[1] + k3[1]], dh, delta);
            [
                (m[0] + (k1[0] + 2.0 * (k2[0] + k3[0]) + k4[0]) * (1.0 / 6.0))
                    .clamp(-m_star, m_star),
                (m[1] + (k1[1] + 2.0 * (k2[1] + k3[1]) + k4[1]) * (1.0 / 6.0))
                    .clamp(-m_star, m_star),
            ]
        } else {
            [
                self.step_rk4(h0[0], h1[0], m[0], m_star),
                self.step_rk4(h0[1], h1[1], m[1], m_star),
            ]
        }
    }

    /// Cold path for discontinuous jumps `|dh| > max_step_dh`:
    /// `ceil(|dh| / max_step_dh)` substeps, at most [`MAX_SUBSTEPS`] (then
    /// each is up to [`STEP_STRETCH`] times the step, still inside RK4's
    /// stability region: the step is at most half the pinning field `k̃`, and
    /// the irreversible term's stiffness is `1/k̃`; longer ones are refused at
    /// build). So a sample costs at most 128× the normal step. Band-limited
    /// audio at high rates never takes it; full-scale noise or glitches near
    /// Nyquist at 44.1 kHz can cost tens of percent of a core while they
    /// last (bounded, never unbounded).
    #[cold]
    #[inline(never)]
    fn step_rk4_subdivided(&self, h0: f64, dh: f64, delta: f64, m: f64) -> f64 {
        let n = ((dh.abs() / self.max_step_dh).ceil() as usize).min(MAX_SUBSTEPS);
        let step_dh = dh / n as f64;
        let mut h = h0;
        let mut cur_m = m;
        for _ in 0..n {
            cur_m = self.rk4_substep(h, cur_m, step_dh, delta);
            h += step_dh;
        }
        cur_m
    }

    /// Advance magnetization `m` from `h0` to `h1` using one RK4 step on the hot
    /// path (`|h1 - h0| <= max_step_dh`) and automatic ramp subdivision on large
    /// discontinuous jumps, clamping to the exact invariant `[-m_star, m_star]`.
    #[inline(always)]
    fn step_rk4(&self, h0: f64, h1: f64, m: f64, m_star: f64) -> f64 {
        let dh = h1 - h0;
        let abs_dh = dh.abs();
        let delta = 1.0_f64.copysign(dh);
        let next_m = if abs_dh <= self.max_step_dh {
            self.rk4_substep(h0, m, dh, delta)
        } else {
            self.step_rk4_subdivided(h0, dh, delta, m)
        };
        next_m.clamp(-m_star, m_star)
    }
}

/// Unique positive fixed point `m* ∈ [0, 1)` of `m* = L(g + α̃ m*)`.
/// Exists and is unique because `α̃ L'(q) <= α̃ / 3 << 1`.
fn saturation_fixed_point(g: f64) -> f64 {
    if g <= 0.0 {
        return 0.0;
    }
    let mut m = langevin_and_deriv(g).0;
    for _ in 0..16 {
        let q = g + ALPHA_TILDE * m;
        let (l, lp) = langevin_and_deriv(q);
        let err = m - l;
        m -= err / (1.0 - ALPHA_TILDE * lp);
        if err.abs() <= 1e-15 * m.abs().max(1e-15) {
            break;
        }
    }
    m.clamp(0.0, 1.0)
}

/// The core's rest point with the field offset `h_bias`: the anhysteretic
/// magnetization there, `m_b = L(h_b + α̃ m_b)` (the state an AC bias field
/// leaves the tape in around the offset), and the small-signal (reversible)
/// susceptibility there, `χ_b = c L'(q_b) / (1 − α̃ c L'(q_b))`. At `m = m_an`
/// the irreversible term is 0, so small signals around the rest point see
/// exactly `χ_b`: the residual `(m − m_b)/(χ_b g) − z` keeps no linear part.
/// Without an offset: `(0, χ₀)`.
fn rest_point(ja: JaParams, h_bias: f64) -> (f64, f64) {
    if h_bias == 0.0 {
        return (0.0, ja.chi0);
    }
    let m = saturation_fixed_point(h_bias.abs()).copysign(h_bias);
    let lp = langevin_and_deriv(h_bias + ALPHA_TILDE * m).1;
    (m, ja.c * lp / (1.0 - ALPHA_TILDE * ja.c * lp))
}

/// The bass EQ pair: a 12 dB/oct low shelf `P` (unity at 1 kHz, the tilt
/// shelf of [`crate::harmonics_tilt`]) in front of the core and its exact
/// inverse after the distortion residual. The residual is taken against the
/// core's own (pre-emphasized) input, so the music's path stays flat; only
/// the distortion sees the EQ.
#[derive(Clone, Copy, Debug)]
struct BassEq {
    /// `P`.
    pre: TiltFilter,
    /// `P⁻¹` (the shelf with its zeros and poles swapped and the inverse
    /// scale).
    post: TiltFilter,
    /// Certified `‖p‖₁`: the core's input stays within `±‖p‖₁` for inputs
    /// within full scale.
    z_max: f64,
    /// Certified `‖p⁻¹‖₁`.
    post_l1: f64,
}

impl BassEq {
    fn new((hz, db): (f64, f64), rate: f64) -> Option<Self> {
        let shelf = |db: f64| OrderTilt {
            low: TiltShelf::new(hz, db),
            high: TiltShelf::new(crate::harmonics_tilt::UNITY_HZ, 0.0),
        };
        let pre = TiltFilter::new(&shelf(db), 1, rate)?;
        let post = TiltFilter::new(&shelf(-db), 1, rate)?;
        Some(Self {
            pre,
            post,
            z_max: pre.l1_norm(),
            post_l1: post.l1_norm(),
        })
    }

    /// `|P(f)|`.
    fn gain(&self, freq: f64, rate: f64) -> f64 {
        self.pre.response(2.0 * PI * freq / rate).norm()
    }
}

/// Complex frequency response of the 1st-order bilinear HPF
/// `H(z) = b0 (1 - z⁻¹) / (1 + a1 z⁻¹)` at `freq_hz`.
fn hpf_response(b0: f64, a1: f64, freq_hz: f64, sample_rate: f64) -> Complex64 {
    let w = 2.0 * PI * freq_hz / sample_rate;
    let (sin_w, cos_w) = w.sin_cos();
    let nr = b0 * (1.0 - cos_w);
    let ni = b0 * sin_w;
    let dr = 1.0 + a1 * cos_w;
    let di = -a1 * sin_w;
    let den = dr * dr + di * di;
    Complex64::new((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
}

/// Design the 5 Hz 1st-order bilinear HPF `(b0, a1)` at `output_rate`.
fn design_dc_blocker(output_rate: f64) -> (f64, f64) {
    let fc = DC_BLOCK_HZ.min(0.1 * output_rate);
    let k = (PI * fc / output_rate).tan();
    let b0 = 1.0 / (1.0 + k);
    let a1 = (k - 1.0) / (1.0 + k);
    (b0, a1)
}

/// Precomputed coherent cosine table for steady-state periodic H3 evaluation
/// via Poincaré fixed-point iteration.
struct PeriodicProbe {
    ja: JaParams,
    hpf_1: Complex64,
    hpf_3: Complex64,
    /// Field offset `h_b`.
    h_bias: f64,
    /// `|P(f)|` and `|P(3f)|` of the bass EQ pair (1 without one): the core
    /// sees the tone at `|P(f)|` times its level, and its harmonic `k` leaves
    /// through `P⁻¹(k f)`.
    eq_1: f64,
    eq_3: f64,
    cos1: Vec<f64>,
    sin1: Vec<f64>,
    cos3: Vec<f64>,
    sin3: Vec<f64>,
}

impl PeriodicProbe {
    fn new(
        ja: JaParams,
        hpf_b0: f64,
        hpf_a1: f64,
        freq_hz: f64,
        sample_rate: f64,
        h_bias: f64,
        eq: Option<&BassEq>,
    ) -> Self {
        let hpf_1 = hpf_response(hpf_b0, hpf_a1, freq_hz, sample_rate);
        let hpf_3 = hpf_response(hpf_b0, hpf_a1, 3.0 * freq_hz, sample_rate);
        let (eq_1, eq_3) = eq.map_or((1.0, 1.0), |e| {
            (
                e.gain(freq_hz, sample_rate),
                e.gain(3.0 * freq_hz, sample_rate),
            )
        });
        let period = sample_rate / freq_hz;
        let (cycles, n) = if period >= CAL_MIN_POINTS as f64 {
            (1usize, CAL_MIN_POINTS)
        } else {
            let cyc = (CAL_MIN_POINTS as f64 / period).ceil().max(1.0) as usize;
            let pts = (cyc as f64 * period).round().max(16.0) as usize;
            (cyc, pts)
        };
        let mut cos1 = Vec::with_capacity(n);
        let mut sin1 = Vec::with_capacity(n);
        let mut cos3 = Vec::with_capacity(n);
        let mut sin3 = Vec::with_capacity(n);
        let w1 = 2.0 * PI * cycles as f64 / n as f64;
        let w3 = 3.0 * w1;
        for i in 0..n {
            let (s1, c1) = (w1 * i as f64).sin_cos();
            let (s3, c3) = (w3 * i as f64).sin_cos();
            cos1.push(c1);
            sin1.push(s1);
            cos3.push(c3);
            sin3.push(s3);
        }
        Self {
            ja,
            hpf_1,
            hpf_3,
            h_bias,
            eq_1,
            eq_3,
            cos1,
            sin1,
            cos3,
            sin3,
        }
    }

    /// Run one full periodic orbit of the field `h_b + amp_h cos` starting
    /// from `(h = h_b + amp_h * cos1[0], m = m0)` and return the ending
    /// magnetization `Psi(m0)`.
    fn orbit_end(&self, h_b: f64, amp_h: f64, m0: f64, m_star: f64) -> f64 {
        let n = self.cos1.len();
        let mut h_prev = h_b + amp_h * self.cos1[0];
        let mut m = m0;
        for i in 1..=n {
            let h = h_b + amp_h * self.cos1[i % n];
            m = self.ja.step_rk4(h_prev, h, m, m_star);
            h_prev = h;
        }
        m
    }

    /// Find the steady-state periodic initial magnetization `m0 = Psi(m0)`
    /// for the field `h_b + amp_h cos` via Aitken/secant acceleration
    /// (typically 3–5 orbits even at tiny `amp_h`), starting near the rest
    /// point `(m_b, χ_b)`.
    fn steady_m0(&self, h_b: f64, amp_h: f64, m_b: f64, chi_b: f64, m_star: f64) -> f64 {
        let mut m0 = (m_b + chi_b * amp_h).clamp(-m_star, m_star);
        let mut m1 = self.orbit_end(h_b, amp_h, m0, m_star);
        let mut f0 = m1 - m0;
        for _ in 0..12 {
            let tol = 1e-13 * m1.abs().max(1e-12);
            if f0.abs() <= tol {
                return m1;
            }
            let m2 = self.orbit_end(h_b, amp_h, m1, m_star);
            let f1 = m2 - m1;
            let denom = f1 - f0;
            if denom.abs() <= 1e-20 {
                return m2;
            }
            let cand = (m1 - f1 * (m1 - m0) / denom).clamp(-m_star, m_star);
            let m_cand = self.orbit_end(h_b, amp_h, cand, m_star);
            let f_cand = m_cand - cand;
            if f_cand.abs() < f1.abs() {
                m0 = cand;
                m1 = m_cand;
                f0 = f_cand;
            } else {
                m0 = m1;
                m1 = m2;
                f0 = f1;
            }
        }
        m1
    }

    /// Steady-state H3 in dBc for a sine of peak amplitude `level ∈ (0, 1]`,
    /// input field drive `drive > 0`, and wet mix `amount ∈ (0, 1]`.
    fn h3_dbc(&self, drive: f64, level: f64, amount: f64) -> f64 {
        if drive <= 0.0 || level <= 0.0 || amount <= 0.0 {
            return f64::NEG_INFINITY;
        }
        let (y1_mag, y3_mag) = self.tone(drive, level, amount);
        if y1_mag <= 0.0 || y3_mag <= 0.0 {
            return f64::NEG_INFINITY;
        }
        lin_to_db(y3_mag / y1_mag)
    }

    /// The steady output's fundamental relative to the input's (1 for
    /// small signals: the residual has no linear part).
    fn fundamental_gain(&self, drive: f64, level: f64) -> f64 {
        self.tone(drive, level, 1.0).0 / level
    }

    /// `(|H1|, |H3|)` of the steady output for the sine `level`.
    fn tone(&self, drive: f64, level: f64, amount: f64) -> (f64, f64) {
        let h_b = self.h_bias;
        let (m_b, chi_b) = rest_point(self.ja, h_b);
        // The tone at the core's input (after `P`).
        let z_amp = level * self.eq_1;
        let amp_h = drive * z_amp;
        let m_star = saturation_fixed_point(h_b.abs() + amp_h);
        let m0 = self.steady_m0(h_b, amp_h, m_b, chi_b, m_star);
        let inv_chi_g = 1.0 / (chi_b * drive);

        let n = self.cos1.len();
        let mut h_prev = h_b + amp_h * self.cos1[0];
        let mut m = m0;
        let mut d1_re = 0.0;
        let mut d1_im = 0.0;
        let mut d3_re = 0.0;
        let mut d3_im = 0.0;
        for i in 0..n {
            let c1 = self.cos1[i];
            let s1 = self.sin1[i];
            let c3 = self.cos3[i];
            let s3 = self.sin3[i];
            if i > 0 {
                let h = h_b + amp_h * c1;
                m = self.ja.step_rk4(h_prev, h, m, m_star);
                h_prev = h;
            }
            let z = z_amp * c1;
            let d = (m - m_b) * inv_chi_g - z;
            d1_re += d * c1;
            d1_im -= d * s1;
            d3_re += d * c3;
            d3_im -= d * s3;
        }
        let norm = 2.0 / n as f64;
        let d1 = Complex64::new(d1_re * norm, d1_im * norm);
        let d3 = Complex64::new(d3_re * norm, d3_im * norm);

        // In the core input's phase: the input tone is `level` there up to
        // `P(f)`'s phase, and `P⁻¹(f)` times that phase is `1/|P(f)|`.
        let y1 = Complex64::new(level, 0.0) + self.hpf_1 * (d1 * (amount / self.eq_1));
        let y3 = self.hpf_3 * (d3 * (amount / self.eq_3));
        (y1.norm(), y3.norm())
    }
}

/// Solve dimensionless input field drive `g > 0` so a 0 dBFS sine at
/// `calibration_hz` (at `amount = 1.0`) produces `target_h3_db` dBc of H3 at
/// `output_rate`. A calibration frequency above [`CAL_MAX_PROBE`] of the rate
/// is probed there (the loop is rate independent, so only the DC blocker and
/// the EQ, both flat up there, tell the two apart).
fn solve_drive(
    ja: JaParams,
    hpf_b0: f64,
    hpf_a1: f64,
    config: &HysteresisConfig,
    eq: Option<&BassEq>,
    output_rate: f64,
) -> Result<f64, HysteresisError> {
    let target_h3_db = config.target_h3_db;
    let probe_hz = config.calibration_hz.min(CAL_MAX_PROBE * output_rate);
    // Symmetric: the field offset goes on after (see `Calibrated`).
    let probe = PeriodicProbe::new(ja, hpf_b0, hpf_a1, probe_hz, output_rate, 0.0, eq);
    let target_lin = db_to_lin(target_h3_db);

    // Combine the cubic Langevin estimate (|H3/H1| ≈ g² / 60) and the linear
    // Rayleigh hysteresis estimate (|H3/H1| ≈ (4 / 15π) ((1 - c) / (c k̃)) g).
    let g_lan = (60.0 * target_lin).sqrt();
    let g_ray = if ja.c < 1.0 {
        (15.0 * PI / 4.0) * (ja.c / ((1.0 - ja.c) * ja.inv_k)) * target_lin
    } else {
        f64::INFINITY
    };
    let g0 = g_lan.min(g_ray).clamp(1e-8, 100.0);

    let mut lo = g0 * 0.25;
    let mut h_lo = probe.h3_dbc(lo, 1.0, 1.0);
    for _ in 0..48 {
        if h_lo <= target_h3_db {
            break;
        }
        lo *= 0.25;
        h_lo = probe.h3_dbc(lo, 1.0, 1.0);
    }
    if h_lo > target_h3_db {
        return Err(HysteresisError::Uncalibratable(format!(
            "could not bracket lower bound for {target_h3_db:.2} dBc"
        )));
    }

    let mut hi = g0 * 2.0;
    let mut prev_h = h_lo;
    let mut reached = false;
    for _ in 0..48 {
        let h_hi = probe.h3_dbc(hi, 1.0, 1.0);
        if h_hi >= target_h3_db {
            reached = true;
            break;
        }
        if hi > 1e4 || (h_hi <= prev_h && hi > 10.0) {
            return Err(HysteresisError::Uncalibratable(format!(
                "target {target_h3_db:.2} dBc exceeds maximum achievable H3 (~{:.2} dBc)",
                prev_h.max(h_hi)
            )));
        }
        prev_h = h_hi;
        hi *= 2.0;
    }
    if !reached {
        return Err(HysteresisError::Uncalibratable(format!(
            "target {target_h3_db:.2} dBc could not be bracketed"
        )));
    }

    let (mut l_lo, mut l_hi) = (lo.ln(), hi.ln());
    for _ in 0..52 {
        let l_mid = 0.5 * (l_lo + l_hi);
        let h_mid = probe.h3_dbc(l_mid.exp(), 1.0, 1.0);
        if h_mid < target_h3_db {
            l_lo = l_mid;
        } else {
            l_hi = l_mid;
        }
    }
    let drive = (0.5 * (l_lo + l_hi)).exp();
    let achieved = probe.h3_dbc(drive, 1.0, 1.0);
    if !drive.is_finite() || drive <= 0.0 || (achieved - target_h3_db).abs() > 1e-2 {
        return Err(HysteresisError::Uncalibratable(format!(
            "solver converged to {achieved:.4} dBc for target {target_h3_db:.4} dBc"
        )));
    }
    Ok(drive)
}

/// Certified per-sample bound of the distortion residual
/// `d[n] = m[n] / (χ₀ g) − x_c[n]` over all inputs `|x_c[n]| <= 1`.
fn certified_residual_bound(ja: JaParams, drive: f64, m_star: f64) -> f64 {
    if drive <= 0.0 {
        return 0.0;
    }
    let chi0_g = ja.chi0 * drive;
    let u_max = m_star / chi0_g;
    let l_g = langevin_and_deriv(drive).0;
    let d_an =
        (1.0 - (ja.c * l_g) / chi0_g).max(0.0) + (ja.c * ALPHA_TILDE * m_star) / (3.0 * chi0_g);
    (u_max + 1.0).min(d_an + (1.0 - ja.c) * u_max)
}

/// The core's operating point, shared by the runtime, the residual probe and
/// the bounds: field `h = drive · z + h_bias` for the core input `z`,
/// magnetization stored relative to `m_bias`, residual
/// `(m − m_b) · inv_chi_g − z`.
#[derive(Clone, Copy, Debug)]
struct Operating {
    ja: JaParams,
    drive: f64,
    h_bias: f64,
    m_bias: f64,
    inv_chi_g: f64,
    m_star: f64,
}

impl Operating {
    /// Magnetization (relative to `m_b`) after the field moves from core
    /// input `z0` to `z1`, from relative magnetization `m`.
    fn step(&self, z0: f64, z1: f64, m: f64) -> f64 {
        let h0 = self.drive * z0 + self.h_bias;
        let h1 = self.drive * z1 + self.h_bias;
        self.ja.step_rk4(h0, h1, m + self.m_bias, self.m_star) - self.m_bias
    }

    fn residual(&self, z: f64, m: f64) -> f64 {
        m * self.inv_chi_g - z
    }
}

/// The largest residual `|d|` the core reaches over
/// core inputs within `±z_max` on a set of probing paths: the first
/// magnetization curve from rest to `z_max`, three turns of the major loop
/// (triangle `±z_max`), and from 16 states spread over the last turn,
/// single jumps (one sample, as the runtime takes them) and reversal ramps
/// to `{±z_max, ±z_max/2, 0}`, each followed by a jump and a ramp to
/// `{±z_max, 0}`. Ramps move in steps well inside one RK4 step and are
/// watched all along. Not a proof: the runtime clamps at [`PROBE_MARGIN`]
/// times what this reaches (where that is below the analytic bound), so
/// the bound holds by construction and the probe only decides whether real
/// signals could ever touch the clamp (tested: never).
fn residual_probe(op: &Operating, z_max: f64) -> f64 {
    if op.drive <= 0.0 {
        return 0.0;
    }
    let dz = 0.25 * op.ja.max_step_dh / op.drive;
    let mut p = 0.0_f64;
    let note = |p: &mut f64, z: f64, m: f64| *p = p.max(op.residual(z, m).abs());
    // Ramp `(z, m)` to `to`, watching every step; `keep` collects states.
    let ramp =
        |p: &mut f64, (z, m): (f64, f64), to: f64, mut keep: Option<&mut Vec<(f64, f64)>>| {
            let n = ((to - z).abs() / dz).ceil().max(1.0) as usize;
            let every = (n / (PROBE_STATES / 2)).max(1);
            let (mut z_now, mut m_now) = (z, m);
            for i in 1..=n {
                let next = if i == n {
                    to
                } else {
                    z + (to - z) * (i as f64 / n as f64)
                };
                m_now = op.step(z_now, next, m_now);
                z_now = next;
                note(p, z_now, m_now);
                if let Some(k) = keep.as_deref_mut()
                    && i % every == 0
                {
                    k.push((z_now, m_now));
                }
            }
            (z_now, m_now)
        };
    let jump = |p: &mut f64, (z, m): (f64, f64), to: f64| {
        let m = op.step(z, to, m);
        note(p, to, m);
        (to, m)
    };
    let mut at = ramp(&mut p, (0.0, 0.0), z_max, None);
    let mut states: Vec<(f64, f64)> = Vec::with_capacity(PROBE_STATES + 2);
    for turn in 0..3 {
        let keep = turn == 2;
        at = ramp(&mut p, at, -z_max, keep.then_some(&mut states));
        at = ramp(&mut p, at, z_max, keep.then_some(&mut states));
    }
    let levels = [-z_max, -0.5 * z_max, 0.0, 0.5 * z_max, z_max];
    let ends = [-z_max, 0.0, z_max];
    for &state in &states {
        for &t1 in &levels {
            for first in [jump(&mut p, state, t1), ramp(&mut p, state, t1, None)] {
                // A minor loop's reversal, then out to the ends.
                for &t2 in &levels {
                    let second = ramp(&mut p, first, t2, None);
                    for &t3 in &ends {
                        jump(&mut p, second, t3);
                        ramp(&mut p, second, t3, None);
                    }
                }
            }
        }
    }
    p
}

/// A layer calibrated and checked at one rate, before the probes.
struct Calibrated {
    ja: JaParams,
    hpf_b0: f64,
    hpf_a1: f64,
    eq: Option<BassEq>,
    drive: f64,
    h_bias: f64,
    m_bias: f64,
    chi_b: f64,
    /// Bound of the core's input: 1, or `‖p‖₁` with the EQ.
    z_max: f64,
    m_star: f64,
}

impl Calibrated {
    /// Validate `config`, solve the drive on the symmetric core (the H3
    /// target holds without the offset), add the field offset, and refuse
    /// what the runtime could not run well: a full-scale jump needing more
    /// than [`MAX_SUBSTEPS`] stretched steps, or a layer that expands.
    fn new(config: &HysteresisConfig, rate: f64) -> Result<Self, HysteresisError> {
        if !rate.is_finite() || rate <= 0.0 {
            return Err(HysteresisError::InvalidSampleRate);
        }
        validate_config(config)?;
        let ja = JaParams::from_config(config);
        let (hpf_b0, hpf_a1) = design_dc_blocker(rate);
        let eq = config.bass_eq.and_then(|e| BassEq::new(e, rate));
        let drive = solve_drive(ja, hpf_b0, hpf_a1, config, eq.as_ref(), rate)?;
        let h_bias = config.asymmetry * ASYMMETRY_FIELD;
        let (m_bias, chi_b) = rest_point(ja, h_bias);
        let z_max = eq.map_or(1.0, |e| e.z_max);
        let m_star = saturation_fixed_point(drive * z_max + h_bias.abs());
        let steps = 2.0 * drive * z_max / ja.max_step_dh;
        if steps > STEP_STRETCH * MAX_SUBSTEPS as f64 {
            return Err(HysteresisError::InvalidParameter(format!(
                "too hot for the loop: a full-scale jump needs {steps:.0} integration steps (at most {:.0}); lower target_h3_db or the bass EQ boost, or raise saturation",
                STEP_STRETCH * MAX_SUBSTEPS as f64
            )));
        }
        if h_bias != 0.0 {
            let probe_hz = config.calibration_hz.min(CAL_MAX_PROBE * rate);
            let probe = PeriodicProbe::new(ja, hpf_b0, hpf_a1, probe_hz, rate, h_bias, eq.as_ref());
            for level_db in EXPANSION_LEVELS_DB {
                let gain = lin_to_db(probe.fundamental_gain(drive, db_to_lin(level_db)));
                if !gain.is_finite() || gain > MAX_EXPANSION_DB {
                    return Err(HysteresisError::InvalidParameter(format!(
                        "asymmetry {} makes the loop expand ({gain:+.1} dB at {level_db} dBFS); lower it or target_h3_db",
                        config.asymmetry
                    )));
                }
            }
        }
        Ok(Self {
            ja,
            hpf_b0,
            hpf_a1,
            eq,
            drive,
            h_bias,
            m_bias,
            chi_b,
            z_max,
            m_star,
        })
    }

    fn operating(&self) -> Operating {
        Operating {
            ja: self.ja,
            drive: self.drive,
            h_bias: self.h_bias,
            m_bias: self.m_bias,
            inv_chi_g: 1.0 / (self.chi_b * self.drive),
            m_star: self.m_star,
        }
    }

    /// The core with no clamps yet (for the probes).
    fn core_unprobed(&self, amount: f64) -> HysteresisCore {
        HysteresisCore {
            ja: self.ja,
            drive: self.drive,
            m_star: self.m_star,
            inv_chi0_g: 1.0 / (self.chi_b * self.drive),
            amount,
            hpf_b0: self.hpf_b0,
            hpf_a1: self.hpf_a1,
            d_max: f64::INFINITY,
            y_max: f64::INFINITY,
            q_max: f64::INFINITY,
            #[cfg(test)]
            d_analytic: f64::INFINITY,
            #[cfg(test)]
            d_probe: 0.0,
            h_bias: self.h_bias,
            m_bias: self.m_bias,
            eq: self.eq,
        }
    }
}

/// The largest `|p|` and `|x_c + WORST_CASE_MARGIN · p|` of a layer's whole
/// path (EQ, core, residual clamp, blocker, inverse EQ, amount) on probe
/// signals at `rate`: from rest, full-scale sines from 20 Hz to 20 kHz (and
/// at −6 dBFS in the bass), full-scale squares, held steps between the
/// extremes and random ramps, jumps and holds; and the path's worst case:
/// held at an extreme, a jump part or all of the way to the other, a jump
/// or a ramp over up to 6 ms part or all of the way back, and a jump back
/// (the inverse EQ's overshoot after a saturated swing, and the blocker's
/// estimate left behind by a quick minor loop; a random search over that
/// family, repeated swings included, finds no more). Not a proof: the
/// runtime clamps at these, which makes the bound hold by construction; the
/// probe only decides whether real signals could ever touch the clamps
/// (tested: never).
fn path_probe(core: &HysteresisCore, rate: f64) -> (f64, f64) {
    let mut worst = (0.0_f64, 0.0_f64);
    // Runs `x` from `st`; returns the largest `|x_c + p|` on the way.
    let mut run = |st: &mut ChannelState, x: &mut dyn Iterator<Item = f64>| -> f64 {
        let mut out = 0.0_f64;
        for xc in x {
            let p = core.tick(st, xc);
            worst.0 = worst.0.max(p.abs());
            worst.1 = worst.1.max((xc + WORST_CASE_MARGIN * p).abs());
            out = out.max((xc + p).abs());
        }
        out
    };
    let rest = ChannelState::default;
    let n = (0.12 * rate) as usize;
    let w = |f: f64| 2.0 * PI * f / rate;
    for f in [
        20.0, 31.5, 40.0, 63.0, 100.0, 160.0, 250.0, 500.0, 1e3, 2e3, 5e3, 1e4, 2e4,
    ] {
        if f < 0.45 * rate {
            run(&mut rest(), &mut (0..n).map(|i| (w(f) * i as f64).sin()));
        }
    }
    for f in [20.0, 40.0, 100.0] {
        run(
            &mut rest(),
            &mut (0..n).map(|i| 0.5 * (w(f) * i as f64).sin()),
        );
    }
    for f in [20.0, 40.0, 100.0, 1e3] {
        run(
            &mut rest(),
            &mut (0..n).map(|i| (w(f) * i as f64).sin().signum()),
        );
    }
    let hold = n / 2;
    run(
        &mut rest(),
        &mut std::iter::repeat_n(1.0, hold)
            .chain(std::iter::repeat_n(-1.0, hold))
            .chain(std::iter::repeat_n(1.0, hold))
            .chain(std::iter::repeat_n(0.0, hold)),
    );
    let mut seed: u64 = 0x7A9E_5EED;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut x = Vec::with_capacity(4 * n);
    let mut at = 0.0_f64;
    while x.len() < 4 * n {
        let to = if rnd() < 0.3 {
            (rnd() - 0.5).signum()
        } else {
            2.0 * rnd() - 1.0
        };
        let len = 1 + (rnd() * 0.005 * rate) as usize;
        match (rnd() * 3.0) as u32 {
            0 => x.extend((1..=len).map(|i| at + (to - at) * i as f64 / len as f64)),
            1 => x.push(to),
            _ => x.extend(std::iter::repeat_n(to, len)),
        }
        at = to;
    }
    run(&mut rest(), &mut x.into_iter());

    // The path's worst case: held at an extreme `x0` (the blocker adapted),
    // a jump a fraction `out` of the way to the other, a jump or a ramp of
    // `len` samples a fraction `back` of the way back, and a jump back (the
    // inverse EQ's overshoot after a saturated swing, the blocker's
    // estimate left behind by a quick minor loop). A grid, then a finer one
    // around its worst.
    let ms = |t: f64| (t * 1e-3 * rate).round() as usize;
    for x0 in [-1.0, 1.0] {
        // On the major loop: from the other extreme.
        let mut held = rest();
        run(
            &mut held,
            &mut std::iter::repeat_n(-x0, ms(30.0)).chain(std::iter::repeat_n(x0, ms(400.0))),
        );
        let mut swing = |out: f64, back: f64, len: usize| -> f64 {
            let a = x0 - 2.0 * x0 * out;
            let b = a + (x0 - a) * back;
            run(
                &mut held.clone(),
                &mut std::iter::once(a)
                    .chain((1..=len).map(|i| a + (b - a) * i as f64 / len as f64))
                    .chain(std::iter::once(b))
                    .chain(std::iter::repeat_n(x0, ms(1.5).max(1))),
            )
        };
        let mut best = (f64::NEG_INFINITY, 1.0, 0.5, 0);
        for out in [0.2, 0.4, 0.6, 0.8, 1.0] {
            for back in (1..=10).map(|k| 0.1 * f64::from(k)) {
                for len in [0.0, 1.0, 2.0, 3.0, 4.0, 6.0].map(ms) {
                    let v = swing(out, back, len);
                    if v > best.0 {
                        best = (v, out, back, len);
                    }
                }
            }
        }
        let (_, out0, back0, len0) = best;
        for d_out in [-0.1, -0.05, 0.0, 0.05, 0.1] {
            for d_back in [-0.05, -0.025, 0.0, 0.025, 0.05] {
                for d_len in [-1.0, -0.5, 0.0, 0.5, 1.0] {
                    let out = (out0 + d_out).clamp(0.05, 1.0);
                    let back = (back0 + d_back).clamp(0.025, 1.0);
                    let len = (len0 as f64 + d_len * 1e-3 * rate).max(0.0) as usize;
                    swing(out, back, len);
                }
            }
        }
    }
    worst
}

/// Per-channel processing state, relative to the core's rest point (all
/// zero at rest, so silence stays exactly zero with a field offset too).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ChannelState {
    /// Previous field relative to the offset, `g · z[n-1]`.
    h_prev: f64,
    /// Magnetization relative to the rest point, `m[n-1] − m_b`.
    m: f64,
    /// Previous distortion residual `d[n-1]` for the Direct Form I DC blocker.
    dc_x1: f64,
    /// Previous DC blocker output `d_hp[n-1]`.
    dc_y1: f64,
    /// The bass EQ pair's states (unused without one).
    pre: TiltState,
    post: TiltState,
}

/// The distortion path of a tape hysteresis layer,
/// `amount · P⁻¹ HPF₅Hz(clamp((m − m_b)/(χ_b g) − z))` with `z = P x_c`
/// (`P = 1` without the bass EQ, `m_b = 0` and `χ_b = χ₀` without an
/// offset), without the direct path or output scale, as
/// [`crate::harmonics::Harmonics`] runs it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HysteresisCore {
    ja: JaParams,
    drive: f64,
    m_star: f64,
    /// `1 / (χ_b g)`.
    inv_chi0_g: f64,
    amount: f64,
    hpf_b0: f64,
    hpf_a1: f64,
    /// The residual's clamp.
    d_max: f64,
    /// The path's clamps and bounds: `|p| <= Y`, `|x_c + p| <= Q`.
    y_max: f64,
    q_max: f64,
    /// The analytic residual bound and the probed largest residual (for
    /// tests).
    #[cfg(test)]
    d_analytic: f64,
    #[cfg(test)]
    d_probe: f64,
    /// The field offset `h_b` (in the Langevin knee's units) and the rest
    /// magnetization `m_b`.
    h_bias: f64,
    m_bias: f64,
    eq: Option<BassEq>,
}

impl HysteresisCore {
    /// No distortion (an inactive layer).
    #[cfg(test)]
    fn off(ja: JaParams, hpf_b0: f64, hpf_a1: f64) -> Self {
        Self {
            ja,
            drive: 0.0,
            m_star: 0.0,
            inv_chi0_g: 0.0,
            amount: 0.0,
            hpf_b0,
            hpf_a1,
            d_max: 0.0,
            y_max: 0.0,
            q_max: 1.0,
            d_analytic: 0.0,
            d_probe: 0.0,
            h_bias: 0.0,
            m_bias: 0.0,
            eq: None,
        }
    }

    /// Calibrate a hysteresis layer at `output_rate` (validated; off the audio
    /// thread). Always validates and solves the drive at `amount = 1`, even
    /// when `config.amount == 0`, so unreachable targets fail at build time.
    pub(crate) fn try_new(
        config: &HysteresisConfig,
        output_rate: f64,
    ) -> Result<Self, HysteresisError> {
        let cal = Calibrated::new(config, output_rate)?;
        let mut core = cal.core_unprobed(config.amount);
        let op = cal.operating();
        // The residual: its analytic bound, tightened by the probe.
        let d_an = if cal.h_bias == 0.0 {
            // The field `g z` with `|z| <= Z` is `(g Z)(z / Z)`: the bound
            // at drive `g Z`, times `Z`.
            certified_residual_bound(cal.ja, cal.drive * cal.z_max, cal.m_star) * cal.z_max
        } else {
            // `|m − m_b| <= m* + |m_b|`.
            (cal.m_star + cal.m_bias.abs()) * op.inv_chi_g + cal.z_max
        };
        let d_probe = residual_probe(&op, cal.z_max);
        core.d_max = d_an.min(PROBE_MARGIN * d_probe);
        #[cfg(test)]
        {
            core.d_analytic = d_an;
            core.d_probe = d_probe;
        }
        // The path: probed through the whole layer (EQ, blocker, inverse) at
        // up to PATH_PROBE_MAX_RATE.
        let probe_rate = output_rate.min(PATH_PROBE_MAX_RATE);
        let (p_max, xp_max) = if probe_rate == output_rate {
            path_probe(&core, probe_rate)
        } else {
            let mut at = Calibrated::new(config, probe_rate)?.core_unprobed(config.amount);
            at.d_max = core.d_max;
            path_probe(&at, probe_rate)
        };
        // Or, where tighter, the path's certified bound from the residual's
        // clamp: the blocker's estimate is a convex mix of past residuals,
        // so `|HPF(d)| <= 2 b0 D`, and the inverse EQ adds its `‖p⁻¹‖₁`.
        let y_an =
            config.amount * cal.eq.map_or(1.0, |e| e.post_l1) * 2.0 * cal.hpf_b0 * core.d_max;
        core.y_max = (PROBE_MARGIN * p_max).min(y_an);
        core.q_max = xp_max.max(1.0);
        Ok(core)
    }

    /// Validate `config` and calibrate it at `output_rate` without the
    /// probes (what the daemon checks at every engine rate).
    pub(crate) fn check(
        config: &HysteresisConfig,
        output_rate: f64,
    ) -> Result<(), HysteresisError> {
        Calibrated::new(config, output_rate).map(drop)
    }

    /// The layer's contribution to the stage bound. The runtime holds the
    /// path within `|p| <= Y` and `|x_c + p| <= Q` (both probed through the
    /// whole layer with margin, and enforced for every input), so
    ///
    /// ```text
    /// θ = 0:  p = 0 · x + p,          |p| <= Y
    /// θ = 1:  p = −x + (x + p),       |x + p| <= Q
    /// ```
    ///
    /// both hold at every sample, and so does the segment between them: a
    /// [`Split::Between`] `{0, Y, 1, Q}`. Alone in the stage the bound is
    /// `min(Q, 1 + Y) = Q`.
    pub(crate) fn split(&self) -> Split {
        if self.drive <= 0.0 || self.amount <= 0.0 {
            return Split::Fixed {
                linear: 0.0,
                delayed: 0.0,
                residual: 0.0,
            };
        }
        Split::Between {
            g0: 0.0,
            r0: self.y_max,
            g1: 1.0,
            r1: self.q_max,
        }
    }

    /// Mean RK4 substeps per sample over the harmonics readout's tones
    /// simulated at `rate` (1: one step per sample; big jumps take
    /// `ceil(|Δh| / max_step_dh)`): its readout cost in tape layers. A sine
    /// `A cos(wn)` (at the core: `|P(f)| A`) jumps
    /// `|Δh| = 2 g |P(f)| A sin(w/2) |sin φ|` per sample at an even spread of
    /// phases `φ`.
    pub(crate) fn readout_substeps(&self, rate: f64) -> f64 {
        use crate::harmonics::{READOUT_FREQS_HZ, READOUT_LEVELS_DBFS, READOUT_SWEEP_FREQS_HZ};
        if self.drive <= 0.0 || rate.is_nan() || rate <= 0.0 {
            return 1.0;
        }
        const PHASES: usize = 256;
        let tone = |freq: f64, level_db: f64| -> f64 {
            let eq = self.eq.map_or(1.0, |e| e.gain(freq, rate));
            let jump = 2.0
                * self.drive
                * eq
                * crate::db_to_lin(level_db)
                * (std::f64::consts::PI * freq / rate).sin().abs();
            (0..PHASES)
                .map(|i| {
                    let phi = std::f64::consts::PI * (i as f64 + 0.5) / PHASES as f64;
                    (jump * phi.sin() / self.ja.max_step_dh).ceil().max(1.0)
                })
                .sum::<f64>()
                / PHASES as f64
        };
        let mut sum = 0.0;
        let mut tones = 0usize;
        for &level in &READOUT_LEVELS_DBFS {
            for &freq in READOUT_SWEEP_FREQS_HZ.iter().chain(&READOUT_FREQS_HZ) {
                sum += tone(freq, level);
                tones += 1;
            }
        }
        for &freq in &READOUT_FREQS_HZ {
            for step in -20..=0 {
                sum += tone(freq, 2.0 * f64::from(step));
                tones += 1;
            }
        }
        sum / tones as f64
    }

    /// The residual clamped to `|d| <= D`.
    #[inline(always)]
    fn clamp_residual(&self, d: f64) -> f64 {
        // `max`/`min`, not `clamp`: never a panic on the audio thread.
        d.max(-self.d_max).min(self.d_max)
    }

    /// The layer's path `p` at clamped input `xc` held to `|p| <= Y` and
    /// `|xc + p| <= Q` (`Q >= 1 >= |xc|`, so the range holds 0).
    #[inline(always)]
    fn clamp_path(&self, p: f64, xc: f64) -> f64 {
        p.max((-self.q_max - xc).max(-self.y_max))
            .min((self.q_max - xc).min(self.y_max))
    }

    /// The 5 Hz Direct Form I DC blocker on the residual `d` (previous input
    /// `x1`, output `y1`).
    #[inline(always)]
    fn dc_block(&self, d: f64, x1: f64, y1: f64) -> f64 {
        self.hpf_b0 * (d - x1) - self.hpf_a1 * y1
    }

    /// One sample of the distortion path for clamped input `xc ∈ [-1, 1]`.
    #[inline(always)]
    pub(crate) fn tick(&self, st: &mut ChannelState, xc: f64) -> f64 {
        let z = match &self.eq {
            Some(eq) => {
                let z = eq.pre.tick(&mut st.pre, xc);
                st.pre.flush();
                z
            }
            None => xc,
        };
        let h = self.drive * z;
        let mut m = self.ja.step_rk4(
            st.h_prev + self.h_bias,
            h + self.h_bias,
            st.m + self.m_bias,
            self.m_star,
        ) - self.m_bias;
        let h_store = if h.abs() < FLUSH_BELOW { 0.0 } else { h };
        if m.abs() < FLUSH_BELOW {
            m = 0.0;
        }
        st.h_prev = h_store;
        st.m = m;

        // Distortion-only residual against the core's own input, clamped to
        // its bounds, through the 5 Hz Direct Form I DC blocker (the direct
        // path stays unfiltered).
        let d = self.clamp_residual(m * self.inv_chi0_g - z);
        let d_store = if d.abs() < FLUSH_BELOW { 0.0 } else { d };
        let mut d_hp = self.dc_block(d, st.dc_x1, st.dc_y1);
        st.dc_x1 = d_store;
        if d_hp.abs() < FLUSH_BELOW && st.dc_y1.abs() < FLUSH_BELOW {
            d_hp = 0.0;
            st.dc_y1 = 0.0;
        } else {
            st.dc_y1 = d_hp;
        }

        let y = match &self.eq {
            Some(eq) => {
                let y = eq.post.tick(&mut st.post, d_hp);
                st.post.flush();
                y
            }
            None => d_hp,
        };
        self.clamp_path(self.amount * y, xc)
    }

    /// Two-lane lockstep evaluation of [`Self::tick`] for two independent
    /// channels, bit-for-bit identical per lane to [`Self::tick`].
    #[inline(always)]
    pub(crate) fn tick_pair(
        &self,
        st0: &mut ChannelState,
        st1: &mut ChannelState,
        xc0: f64,
        xc1: f64,
    ) -> (f64, f64) {
        let (z0, z1) = match &self.eq {
            Some(eq) => {
                let z0 = eq.pre.tick(&mut st0.pre, xc0);
                let z1 = eq.pre.tick(&mut st1.pre, xc1);
                st0.pre.flush();
                st1.pre.flush();
                (z0, z1)
            }
            None => (xc0, xc1),
        };
        let h0 = self.drive * z0;
        let h1 = self.drive * z1;
        let [m0, m1] = self.ja.step_rk4_pair(
            [st0.h_prev + self.h_bias, st1.h_prev + self.h_bias],
            [h0 + self.h_bias, h1 + self.h_bias],
            [st0.m + self.m_bias, st1.m + self.m_bias],
            self.m_star,
        );
        let mut m0 = m0 - self.m_bias;
        let mut m1 = m1 - self.m_bias;
        st0.h_prev = if h0.abs() < FLUSH_BELOW { 0.0 } else { h0 };
        st1.h_prev = if h1.abs() < FLUSH_BELOW { 0.0 } else { h1 };
        if m0.abs() < FLUSH_BELOW {
            m0 = 0.0;
        }
        if m1.abs() < FLUSH_BELOW {
            m1 = 0.0;
        }
        st0.m = m0;
        st1.m = m1;

        let d0 = self.clamp_residual(m0 * self.inv_chi0_g - z0);
        let d1 = self.clamp_residual(m1 * self.inv_chi0_g - z1);
        let mut d_hp0 = self.dc_block(d0, st0.dc_x1, st0.dc_y1);
        let mut d_hp1 = self.dc_block(d1, st1.dc_x1, st1.dc_y1);
        st0.dc_x1 = if d0.abs() < FLUSH_BELOW { 0.0 } else { d0 };
        st1.dc_x1 = if d1.abs() < FLUSH_BELOW { 0.0 } else { d1 };
        if d_hp0.abs() < FLUSH_BELOW && st0.dc_y1.abs() < FLUSH_BELOW {
            d_hp0 = 0.0;
            st0.dc_y1 = 0.0;
        } else {
            st0.dc_y1 = d_hp0;
        }
        if d_hp1.abs() < FLUSH_BELOW && st1.dc_y1.abs() < FLUSH_BELOW {
            d_hp1 = 0.0;
            st1.dc_y1 = 0.0;
        } else {
            st1.dc_y1 = d_hp1;
        }

        let (y0, y1) = match &self.eq {
            Some(eq) => {
                let y0 = eq.post.tick(&mut st0.post, d_hp0);
                let y1 = eq.post.tick(&mut st1.post, d_hp1);
                st0.post.flush();
                st1.post.flush();
                (y0, y1)
            }
            None => (d_hp0, d_hp1),
        };
        (
            self.clamp_path(self.amount * y0, xc0),
            self.clamp_path(self.amount * y1, xc1),
        )
    }
}

fn validate_config(config: &HysteresisConfig) -> Result<(), HysteresisError> {
    if !config.saturation.is_finite() || !(0.0..=1.0).contains(&config.saturation) {
        return Err(HysteresisError::InvalidParameter(format!(
            "saturation must be in 0..=1 (got {})",
            config.saturation
        )));
    }
    if !config.width.is_finite() || !(0.0..=1.0).contains(&config.width) {
        return Err(HysteresisError::InvalidParameter(format!(
            "width must be in 0..=1 (got {})",
            config.width
        )));
    }
    if !config.amount.is_finite() || !(0.0..=1.0).contains(&config.amount) {
        return Err(HysteresisError::InvalidParameter(format!(
            "amount must be in 0..=1 (got {})",
            config.amount
        )));
    }
    if !config.target_h3_db.is_finite()
        || !(HYSTERESIS_MIN_H3_DB..=HYSTERESIS_MAX_H3_DB).contains(&config.target_h3_db)
    {
        return Err(HysteresisError::InvalidParameter(format!(
            "target_h3_db must be in {HYSTERESIS_MIN_H3_DB}..={HYSTERESIS_MAX_H3_DB} dBc (got {})",
            config.target_h3_db
        )));
    }
    if !config.calibration_hz.is_finite()
        || !(HYSTERESIS_MIN_CALIBRATION_HZ..=HYSTERESIS_MAX_CALIBRATION_HZ)
            .contains(&config.calibration_hz)
    {
        return Err(HysteresisError::InvalidParameter(format!(
            "calibration_hz must be in {HYSTERESIS_MIN_CALIBRATION_HZ}..={HYSTERESIS_MAX_CALIBRATION_HZ} Hz (got {})",
            config.calibration_hz
        )));
    }
    if !config.asymmetry.is_finite() || config.asymmetry.abs() > HYSTERESIS_MAX_ASYMMETRY {
        return Err(HysteresisError::InvalidParameter(format!(
            "asymmetry must be in -{HYSTERESIS_MAX_ASYMMETRY}..={HYSTERESIS_MAX_ASYMMETRY} (got {})",
            config.asymmetry
        )));
    }
    if let Some((hz, db)) = config.bass_eq {
        if !hz.is_finite() || !(HYSTERESIS_MIN_EQ_HZ..=HYSTERESIS_MAX_EQ_HZ).contains(&hz) {
            return Err(HysteresisError::InvalidParameter(format!(
                "bass_eq.hz must be in {HYSTERESIS_MIN_EQ_HZ}..={HYSTERESIS_MAX_EQ_HZ} Hz (got {hz})"
            )));
        }
        if !db.is_finite() || !(0.0..=HYSTERESIS_MAX_EQ_DB).contains(&db) {
            return Err(HysteresisError::InvalidParameter(format!(
                "bass_eq.db must be in 0..={HYSTERESIS_MAX_EQ_DB} dB (got {db})"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::tests_support::Lone;
    use super::*;

    const ALL_RATES: [f64; 6] = [
        44_100.0, 48_000.0, 96_000.0, 192_000.0, 384_000.0, 768_000.0,
    ];

    struct Measured {
        /// `[DC, H1, H2, H3, H4, H5]` linear amplitudes.
        amp: [f64; 6],
        /// The largest magnitude after the settling time.
        peak: f64,
    }

    impl Measured {
        fn dbc(&self, k: usize) -> f64 {
            lin_to_db(self.amp[k] / self.amp[1])
        }
    }

    /// Worst-case inputs at a low and a high rate for every kind of curve:
    /// nothing scales the output, and it stays within the stage's bound.
    /// One thread per rate and config.
    #[test]
    fn output_stays_within_full_scale_for_worst_case_inputs() {
        let configs = [
            HysteresisConfig {
                saturation: 0.5,
                width: 0.0,
                amount: 1.0,
                target_h3_db: -20.0,
                ..Default::default()
            },
            HysteresisConfig {
                saturation: 0.0,
                width: 0.5,
                amount: 1.0,
                target_h3_db: -25.0,
                ..Default::default()
            },
            HysteresisConfig {
                saturation: 0.5,
                width: 0.5,
                amount: 1.0,
                target_h3_db: -15.0,
                ..Default::default()
            },
            HysteresisConfig {
                saturation: 1.0,
                width: 1.0,
                amount: 1.0,
                target_h3_db: -15.0,
                ..Default::default()
            },
            HysteresisConfig {
                saturation: 0.5,
                width: 1.0,
                amount: 0.5,
                target_h3_db: -30.0,
                ..Default::default()
            },
        ];

        let jobs: Vec<(f64, &HysteresisConfig)> = [44_100.0, 192_000.0]
            .into_iter()
            .flat_map(|rate| configs.iter().map(move |cfg| (rate, cfg)))
            .collect();
        std::thread::scope(|scope| {
            for &(rate, cfg) in &jobs {
                scope.spawn(move || {
                    // Nothing scales the output: it stays within the bound.
                    let b = db_to_lin(Lone::try_new(cfg, rate, 1).unwrap().peak_gain_db());
                    // 1. Full-scale sines across 20 Hz–20 kHz.
                    for f in [20.0, 1_000.0, 20_000.0] {
                        let m = measure_sine(cfg, rate, f, 1.0, 0.15);
                        assert!(
                            m.peak <= b,
                            "rate={rate} {cfg:?} sine {f} Hz peak {} > {b}",
                            m.peak
                        );
                    }

                    // 2. Settled steps (+1 -> -1, -1 -> +1, 0 -> +1).
                    let settle_n = (rate * 0.25) as usize;
                    for (from, to) in [(1.0, -1.0), (-1.0, 1.0), (0.0, 1.0), (0.0, -1.0)] {
                        let mut st = Lone::try_new(cfg, rate, 1).unwrap();
                        let mut buf = vec![from; settle_n];
                        buf.extend(std::iter::repeat_n(to, 2048));
                        st.process(&mut [&mut buf]);
                        let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                        assert!(
                            peak <= b,
                            "rate={rate} {cfg:?} step {from}->{to} peak {peak} > {b}"
                        );
                    }

                    // 3. Alternating Nyquist pulses (+/-1).
                    {
                        let mut st = Lone::try_new(cfg, rate, 1).unwrap();
                        let mut buf: Vec<f64> = (0..4096)
                            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
                            .collect();
                        st.process(&mut [&mut buf]);
                        let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                        assert!(
                            peak <= b,
                            "rate={rate} {cfg:?} Nyquist pulses peak {peak} > {b}"
                        );
                    }

                    // 4. Low-frequency full-scale square waves.
                    for f_sq in [20.0, 100.0] {
                        let mut st = Lone::try_new(cfg, rate, 1).unwrap();
                        let period = (rate / f_sq).round() as usize;
                        let half = period / 2;
                        let mut buf: Vec<f64> = (0..(rate * 0.25) as usize)
                            .map(|i| if (i % period) < half { 1.0 } else { -1.0 })
                            .collect();
                        st.process(&mut [&mut buf]);
                        let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                        assert!(
                            peak <= b,
                            "rate={rate} {cfg:?} square {f_sq} Hz peak {peak} > {b}"
                        );
                    }

                    // 5. Adversarial AC-dithered DC ratchet -> opposite held DC -> opposite ratchet.
                    {
                        let mut st = Lone::try_new(cfg, rate, 1).unwrap();
                        let mut buf = Vec::with_capacity(settle_n + 4500);
                        for i in 0..4000 {
                            buf.push(if i % 2 == 0 { -1.0 } else { -0.8 });
                        }
                        buf.extend(std::iter::repeat_n(1.0, settle_n));
                        for i in 0..500 {
                            buf.push(if i % 2 == 0 { 1.0 } else { 0.0 });
                        }
                        st.process(&mut [&mut buf]);
                        let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                        assert!(
                            peak <= b,
                            "rate={rate} {cfg:?} adversarial ratchet peak {peak} > {b}"
                        );
                    }
                });
            }
        });
    }

    /// Measure a coherent sine at `target_hz` at peak amplitude `amp` after
    /// `settle_s` seconds of settling, processing in varying odd block sizes.
    fn measure_sine(
        cfg: &HysteresisConfig,
        rate: f64,
        target_hz: f64,
        amp: f64,
        settle_s: f64,
    ) -> Measured {
        let mut stage = Lone::try_new(cfg, rate, 1).unwrap();
        let settle = (rate * settle_s).ceil() as usize;
        let cycles = (target_hz * 0.25).round().max(8.0) as usize;
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
    fn langevin_accuracy_and_continuity() {
        // Check exact values, derivative by central difference, and continuity
        // across both thresholds (3.0 Padé -> exp and 20.0 exp -> asymptotic),
        // plus huge inputs.
        assert_eq!(langevin_and_deriv(0.0), (0.0, 1.0 / 3.0));
        for &q in &[
            1e-12, 1e-6, 1e-3, 0.1, 0.5, 1.0, 2.5, 2.99, 3.0, 3.01, 5.0, 19.99, 20.0, 20.01, 1e4,
            1e12,
        ] {
            let (l_pos, lp_pos) = langevin_and_deriv(q);
            let (l_neg, lp_neg) = langevin_and_deriv(-q);
            assert!(l_pos.is_finite() && lp_pos.is_finite());
            assert!((l_pos + l_neg).abs() < 1e-15, "odd symmetry at q={q}");
            assert!((lp_pos - lp_neg).abs() < 1e-15, "even symmetry at q={q}");
            assert!((0.0..=1.0).contains(&l_pos));
            assert!((0.0..=1.0 / 3.0 + 1e-15).contains(&lp_pos));
            if (1e-4..=100.0).contains(&q) {
                let eps = q * 1e-6;
                let fd =
                    (langevin_and_deriv(q + eps).0 - langevin_and_deriv(q - eps).0) / (2.0 * eps);
                assert!(
                    (lp_pos - fd).abs() < 1e-8 * lp_pos.max(1e-12),
                    "deriv mismatch at q={q}: {lp_pos} vs {fd}"
                );
            }
        }
    }

    #[test]
    fn calibration_h3_matches_target_within_0_1_db() {
        let cases = [
            (44_100.0, 0.5, 0.5, 1_000.0, -40.0),
            (48_000.0, 0.5, 0.5, 1_000.0, -30.0),
            (48_000.0, 0.0, 0.5, 400.0, -35.0),
            (48_000.0, 1.0, 0.5, 400.0, -35.0),
            (48_000.0, 0.5, 0.0, 1_000.0, -40.0),
            (48_000.0, 0.5, 1.0, 1_000.0, -25.0),
            (96_000.0, 0.5, 0.5, 1_000.0, -20.0),
            (192_000.0, 0.5, 0.5, 400.0, -50.0),
            (384_000.0, 0.5, 0.5, 1_000.0, -40.0),
            (768_000.0, 0.5, 0.5, 1_000.0, -40.0),
        ];
        for (rate, saturation, width, calibration_hz, target_h3_db) in cases {
            let cfg = HysteresisConfig {
                enabled: true,
                saturation,
                width,
                amount: 1.0,
                target_h3_db,
                calibration_hz,
                asymmetry: 0.0,
                bass_eq: None,
            };
            let m = measure_sine(&cfg, rate, calibration_hz, 1.0, 0.5);
            let h3 = m.dbc(3);
            assert!(
                (h3 - target_h3_db).abs() < 0.1,
                "rate={rate} sat={saturation} width={width} cal={calibration_hz} target={target_h3_db}: got H3={h3:.4} dBc"
            );
            // Symmetric magnetic loop: even harmonics and DC stay at noise floor.
            assert!(
                m.dbc(2) < -100.0,
                "unexpected H2={:.2} dBc for {cfg:?}",
                m.dbc(2)
            );
            assert!(
                m.dbc(4) < -100.0,
                "unexpected H4={:.2} dBc for {cfg:?}",
                m.dbc(4)
            );
            assert!(m.amp[0] < 1e-6, "unexpected DC={:e} for {cfg:?}", m.amp[0]);
        }
    }

    #[test]
    fn stable_and_finite_across_all_rates_and_extreme_inputs() {
        let cfg = HysteresisConfig {
            saturation: 1.0,
            width: 1.0,
            amount: 1.0,
            target_h3_db: -15.0,
            calibration_hz: 1_000.0,
            asymmetry: 0.0,
            bass_eq: None,
            enabled: true,
        };
        for rate in ALL_RATES {
            let mut st = Lone::try_new(&cfg, rate, 2).unwrap();
            let mut l = vec![0.0; 2048];
            let mut r = vec![0.0; 2048];
            for i in 0..2048 {
                l[i] = match i % 7 {
                    0 => 1.0,
                    1 => -1.0,
                    2 => f64::NAN,
                    3 => f64::INFINITY,
                    4 => f64::NEG_INFINITY,
                    5 => 2.5,
                    _ => -2.5,
                };
                r[i] = if (i / 16) % 2 == 0 { 1.0 } else { -1.0 };
            }
            st.process(&mut [&mut l, &mut r]);
            assert!(
                l.iter().all(|x| x.is_finite()),
                "non-finite output at {rate} Hz"
            );
            assert!(
                r.iter().all(|x| x.is_finite() && x.abs() <= 1.0),
                "out-of-bound square output at {rate} Hz"
            );
        }
    }

    #[test]
    fn non_finite_input_maps_to_zero_without_poisoning_state() {
        let cfg = HysteresisConfig::default();
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
    fn silence_after_impulse_decays_to_exact_zero_with_no_subnormals() {
        for rate in [48_000.0, 768_000.0] {
            let mut st = Lone::try_new(&HysteresisConfig::default(), rate, 1).unwrap();
            let mut buf = vec![0.0; 8192];
            buf[0] = 1.0;
            // 3 seconds is > 90 time constants of the 5 Hz DC blocker
            // (exp(-90) < 1e-39 < FLUSH_BELOW).
            let blocks = ((rate * 3.0) / 8192.0).ceil() as usize;
            for blk in 0..blocks {
                if blk > 0 {
                    buf.fill(0.0);
                }
                st.process(&mut [&mut buf[..]]);
                let ch = st.core_state(0);
                for v in [ch.h_prev, ch.m, ch.dc_x1, ch.dc_y1, buf[8191]] {
                    assert!(!v.is_subnormal(), "subnormal {v:e} at {rate} Hz blk={blk}");
                }
            }
            // Output has settled to exact 0.0 despite any magnetic remanence in `m`.
            assert_eq!(buf[8191], 0.0, "output did not settle to 0 at {rate} Hz");
            assert_eq!(st.core_state(0).dc_y1, 0.0);
        }
    }

    #[test]
    fn disabled_or_zero_amount_is_passthrough() {
        for cfg in [
            HysteresisConfig {
                enabled: false,
                ..Default::default()
            },
            HysteresisConfig {
                amount: 0.0,
                ..Default::default()
            },
        ] {
            let mut st = Lone::try_new(&cfg, 48_000.0, 1).unwrap();
            assert_eq!(st.peak_gain_db(), 0.0);
            assert_eq!(st.small_signal_gain_db(), 0.0);
            assert_eq!(Lone::loudness_change_db(&cfg).unwrap(), 0.0);
            let orig = vec![0.1, -0.7, 1.3];
            let mut buf = orig.clone();
            st.process(&mut [&mut buf]);
            assert_eq!(buf, orig);
        }
    }
}

#[cfg(test)]
mod tape_tests {
    //! The bass EQ pair, the field offset and the runtime clamps (residual
    //! `D`, path `Y` and `Q`; real signals never touch them), across the
    //! tape presets and corners of the parameter space.
    use super::tests_support::Lone;
    use super::*;

    /// The tape presets and generic corners of the parameter space, with
    /// and without the bass EQ and the field offset.
    pub(super) fn configs() -> Vec<(String, HysteresisConfig)> {
        let mut out: Vec<(String, HysteresisConfig)> =
            ["tape_master", "tape_studio", "tape_vintage"]
                .iter()
                .map(|id| {
                    let p = audivane_protocol::harmonics_preset(id).unwrap();
                    let audivane_protocol::HarmonicsLayer::Hysteresis(l) = &p.layer else {
                        panic!("{id} is a tape preset")
                    };
                    (id.to_string(), HysteresisConfig::from(l))
                })
                .collect();
        let plain = |saturation, width, target_h3_db, asymmetry, bass_eq| HysteresisConfig {
            saturation,
            width,
            target_h3_db,
            asymmetry,
            bass_eq,
            ..Default::default()
        };
        out.extend([
            (
                "symmetric master".into(),
                HysteresisConfig {
                    asymmetry: 0.0,
                    bass_eq: None,
                    ..out[0].1.clone()
                },
            ),
            (
                "wide loop, offset -0.5, EQ 200 Hz 8 dB".into(),
                plain(0.1, 0.5, -15.0, -0.5, Some((200.0, 8.0))),
            ),
            (
                "widest loop, EQ 50 Hz 12 dB".into(),
                plain(1.0, 1.0, -15.0, 0.0, Some((50.0, 12.0))),
            ),
            (
                "no loop, offset 1".into(),
                plain(0.0, 0.0, -20.0, 1.0, None),
            ),
        ]);
        out
    }

    #[test]
    fn bound_holds_on_hold_and_jump_paths_with_offset_and_eq() {
        // The ascending branch up to `x*`, a hold, then a jump, for every
        // config: the output stays within the bound.
        for (name, cfg) in configs() {
            for rate in [44_100.0, 192_000.0] {
                let mut st = Lone::try_new(&cfg, rate, 1).unwrap();
                let b = db_to_lin(st.peak_gain_db());
                let hold = (0.25 * rate) as usize;
                for x_star in [0.3, 0.7, 1.0] {
                    for to in [-1.0, 0.0, 1.0] {
                        st.reset();
                        let ramp = (0.01 * rate) as usize;
                        let mut buf: Vec<f64> = (0..ramp)
                            .map(|i| -1.0 + (1.0 + x_star) * i as f64 / ramp as f64)
                            .collect();
                        buf.extend(std::iter::repeat_n(x_star, hold));
                        buf.extend(std::iter::repeat_n(to, 2048));
                        st.process(&mut [&mut buf]);
                        let peak = buf.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
                        assert!(
                            peak <= b,
                            "{name} {rate}: ramp to {x_star}, hold, jump to {to}: {peak}"
                        );
                    }
                }
                // Full-scale squares and sines through the EQ'd bass.
                for f in [20.0, 40.0, 100.0] {
                    let n = (0.3 * rate) as usize;
                    for square in [false, true] {
                        st.reset();
                        let mut buf: Vec<f64> = (0..n)
                            .map(|i| {
                                let v = (2.0 * PI * f * i as f64 / rate).sin();
                                if square { v.signum() } else { v }
                            })
                            .collect();
                        st.process(&mut [&mut buf]);
                        let peak = buf.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
                        assert!(peak <= b, "{name} {rate}: {f} Hz square={square}: {peak}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;
    use crate::Stage;
    use crate::harmonics::{Harmonics, HarmonicsError};
    use audivane_protocol::{HarmonicsConfig, HarmonicsLayer, HysteresisBassEq};

    /// The harmonics config with `cfg` as its only layer.
    pub(super) fn lone_config(cfg: &HysteresisConfig) -> HarmonicsConfig {
        HarmonicsConfig {
            enabled: true,
            layers: vec![HarmonicsLayer::Hysteresis(HysteresisLayer {
                enabled: cfg.enabled,
                preset: None,
                saturation: cfg.saturation,
                width: cfg.width,
                amount: cfg.amount,
                target_h3_db: cfg.target_h3_db,
                calibration_hz: cfg.calibration_hz,
                bass_eq: cfg.bass_eq.map(|(hz, db)| HysteresisBassEq {
                    enabled: true,
                    hz,
                    db,
                }),
                asymmetry: (cfg.asymmetry != 0.0).then_some(cfg.asymmetry),
            })],
            ..Default::default()
        }
    }

    /// A lone tape layer of `cfg` in the harmonics stage (as the layer
    /// ships), with its core for the core's own numbers.
    #[derive(Clone)]
    #[allow(dead_code)]
    pub(super) struct Lone {
        stage: Harmonics,
        pub(super) core: HysteresisCore,
        active: bool,
        rate: f64,
        state: Vec<ChannelState>,
    }

    impl Lone {
        pub(super) fn try_new(
            cfg: &HysteresisConfig,
            rate: f64,
            channels: usize,
        ) -> Result<Self, HarmonicsError> {
            let stage = Harmonics::try_new(&lone_config(cfg), rate, channels)?;
            let active = cfg.is_active();
            let core = if active {
                HysteresisCore::try_new(cfg, rate).map_err(|e| HarmonicsError::Hysteresis(0, e))?
            } else {
                let (b0, a1) = design_dc_blocker(rate);
                HysteresisCore::off(JaParams::from_config(cfg), b0, a1)
            };
            Ok(Self {
                stage,
                core,
                active,
                rate,
                state: vec![ChannelState::default(); channels],
            })
        }

        pub(super) fn process(&mut self, chans: &mut [&mut [f64]]) {
            self.stage.process(chans);
        }

        pub(super) fn reset(&mut self) {
            self.stage.reset();
        }

        pub(super) fn core_state(&self, channel: usize) -> &ChannelState {
            &self.state[channel]
        }

        pub(super) fn peak_gain_db(&self) -> f64 {
            self.stage.peak_gain_db()
        }

        pub(super) fn small_signal_gain_db(&self) -> f64 {
            self.stage.small_signal_gain_db()
        }

        pub(super) fn loudness_change_db(cfg: &HysteresisConfig) -> Result<f64, HarmonicsError> {
            Harmonics::loudness_change_db(&lone_config(cfg))
        }
    }
}
