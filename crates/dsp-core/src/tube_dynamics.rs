//! Dynamic tube behaviour on a Chebyshev curve: supply sag and bias shift.
//!
//! The sag and bias options of a Chebyshev layer
//! ([`audivane_protocol::ChebyshevLayer::sag`] and `bias`). `DynamicsCore`
//! is the layer kind the harmonics stage runs for such a layer
//! (`harmonics_layer`): built at an output rate, one sample at a time, its
//! bound split into a linear part plus a remainder, its envelopes presettled
//! for readouts; [`TubeDynamics`] wraps one core as a [`Stage`] for tests and
//! benches (overs, and an output scale `1/B` that puts the bound at full
//! scale, so the tests can check the bound against 0 dBFS; the harmonics
//! stage itself plays unscaled).
//!
//! # Model
//!
//! The static curve is the one a Chebyshev layer applies,
//! `c(v) = v + S(v) − S(0)` with `S(v) = Σ_{k=2..9} a_k T_k(v)` and the
//! signed amplitudes of [`layer_amplitudes`]. Its small-signal gain is
//! `L = c'(0) = 1 + Σ_odd a_k T_k'(0)`. Two envelope followers of the
//! rectified input `r = |x_c|` (`x_c` = the input clamped to ±1) move the
//! operating point on that curve:
//!
//! ```text
//! e[n] = e[n−1] + α (r[n] − e[n−1]),  α = α_att if r[n] > e[n−1] else α_rel
//! α    = 1 − exp(−1 / (τ f_s))        (τ the time constant, f_s the rate)
//! d    = 1 − (1 − 10^(−D/20)) e_s     sag drive, in [10^(−D/20), 1]
//! b    = β e_b                        bias point, between 0 and β
//! u    = d x_c
//! y    = L u + δ(u; b),   δ(u; b) = S(b + u) − S(b) − S'(b) u
//! ```
//!
//! `δ` is the curve at `b + u` minus its value and its tangent at the
//! operating point: the curve's pure distortion around `b`. Both envelopes
//! move the operating point of the same curve; no order gets a gain of its
//! own.
//!
//! **Sag** (`D` dB, attack and release in ms) lowers the drive `d` on loud
//! passages. Drive is the input gain in front of the curve, as a layer's
//! static drive is (moving a full-scale sine down to level `d` on the
//! curve), and it is not renormalised: the direct path is `L d x`. A
//! sagging supply lowers the stage's gain, so the fundamental drops by up to
//! `D` dB (gentle compression), and term `k` falls relative to the
//! fundamental as `d^(k−1)` (higher terms mix into lower orders below full
//! scale), so peaks lean toward the low orders. The envelope's ripple (it
//! follows `|x|` within a cycle) modulates the gain a little, which adds
//! faint odd products at low frequencies, as a real supply's ripple does.
//! The drive only goes down: the curve is specified (and its bound certified)
//! on `|v| <= 1`, and drive above 0 dB would need a knee beyond full scale.
//! Renormalising (`c(d x)/d`) would cancel the compression and make loud
//! passages cleaner, the opposite of sag.
//!
//! **Bias shift** (`β` in ±0.25 of full scale, attack and release in ms, a
//! slower envelope) evaluates the curve at `b + u` minus its value at `b`,
//! so odd orders produce even harmonics and the even/odd balance shifts on
//! sustained loud passages. The slope at the operating point, `c'(b)`,
//! differs from `L` by about `Σ_even a_k T_k''(0) b` (`4 a_2 b`: 0.03 dB for
//! H2 at −40 dB and `b = 0.1`, 0.7 dB at −20 dB). It is compensated (the
//! `− S'(b) u` term): the direct path stays exactly `L u` at every `b`, so
//! the slow envelope changes the harmonic balance and never the level (a
//! level drift tied to it would be a slow compressor or expander depending
//! on the sign of `a_2 β`); the distortion terms keep no linear part, the
//! harmonics stage's rule, so per-order filters on them never change the
//! music's response; and the split into linear part and remainder is exact.
//! It costs one more lane (`U_k(b)`, as `T_k' = k U_{k−1}`) in the same
//! recurrence.
//!
//! **DC.** `δ(0; b) = 0` exactly for every `b`, so `b` moving in silence
//! never makes a DC step. Under signal only the part of `δ` even in `u`,
//! `δ_e = (S(b+u) + S(b−u))/2 − S(b)`, makes DC; it goes through the 5 Hz
//! first-order DC blocker of the harmonics stage, and the odd part
//! `δ_o = (S(b+u) − S(b−u))/2 − S'(b) u` and the direct path pass
//! unfiltered (as in [`crate::harmonics`]). The DC of `δ_e` drifts with `b`
//! at the bias envelope's pace (hundreds of ms to seconds), far slower than
//! the blocker's 32 ms time constant, which follows it with a residual of
//! about `32 ms × d(DC)/dt`.
//!
//! # Bound
//!
//! The output depends on `(u, b)` only, with `|u| <= 1` for every drive, so
//! sag never raises the bound. The bias range `[min(0, β), max(0, β)]` is
//! certified on a grid of `b`: at each grid point the power form of
//! `L u + δ_o + b_0 δ_e` and of `δ_e` in `u` goes through the harmonics
//! stage's certified range (grid in `u` plus a second-derivative margin),
//! and between grid points a second margin `h_b²/8 · max|∂²/∂b²|` covers
//! the gap. With the blocker, `y = L u + δ_o + b_0 δ_e − b_0 w`, `w` a
//! positive average of past `δ_e` over any reachable `(u, b)`, as in
//! [`crate::harmonics`].
//!
//! The curve is evaluated beyond full scale by up to `|β|`, where Chebyshev
//! terms grow fast (`T_9(1.25) ≈ 256`). So the bias reach is cut where the
//! bound would exceed the static curve's (at least 1) by
//! [`BIAS_BOUND_BUDGET_DB`]: the `b` grid is walked outward from 0 and stops
//! before the first cell that would pass it, and `b = β e_b` is clamped to
//! the certified reach ([`TubeDynamics::bias_reach`],
//! [`TubeDynamics::bias_limited`] for a warning). Hi-fi curves keep the full
//! ±0.25; every order at −20 dB keeps only a few thousandths. The stage
//! multiplies by `s = min(1, 1/B)`, so inputs within full scale never exceed
//! it; overs pass at unity slope times `s`.
//!
//! With both depths 0 the core runs the harmonics stage's term-by-term
//! evaluation and certified bound, so it matches a one-layer
//! [`crate::harmonics::Harmonics`] bit for bit (while that stage's blocker
//! state stays above `2^-100`; below it this one flushes to 0).
//!
//! # Tilt
//!
//! A tilted layer's filtered orders (`harmonics_tilt`: the orders whose
//! filter `G_k` is not unity) leave the curve `S` above, which keeps the
//! unfiltered ones; `L` keeps every order's slope. Each filtered order is
//! split as the curve is, from the same recurrence (`T_k' = k U_{k−1}`,
//! `U_{k−1}(b)` the fourth lane before step `k`):
//!
//! ```text
//! e_k = (T_k(b+u) + T_k(b−u))/2 − T_k(b)          even in u (its DC)
//! o_k = (T_k(b+u) − T_k(b−u))/2 − k U_{k−1}(b) u   odd in u, no linear part
//! z_k = o_k + HPF₅Hz(e_k)                          its own one-state blocker
//! y   = L u + δ_o + HPF₅Hz(δ_e) + Σ_k a_k G_k(z_k)
//! ```
//!
//! So each order keeps its own corners, the low shelf on the odd orders
//! only, and its per-order override, as on a static layer (one filter on the
//! total distortion would lose them). At rest (`b = 0`, `d = 1`) an odd
//! order is all `o_k = D_k` and an even one all `e_k = D_k`, so the output
//! is the static tilted layer's (the blocker and the filter swap places on
//! an even order: equal up to rounding), and the static readouts, which
//! are the curves at rest in closed form, stay valid. Without a filtered
//! order nothing of this runs: the evaluation is the untilted one, bit for
//! bit.
//!
//! **Bound.** At each grid point `b` the certified ranges of `o_k + b_0 e_k`
//! and of `e_k` in `u` (their power forms, the same `b` margin from their
//! own second derivatives) widen each order's running ranges over
//! `[0, b]`. With the blocker `z_k = o_k + b_0 e_k − b_0 w_k`, `w_k` a
//! positive average of past `e_k`, so `z_k` lies in
//! `[lo, hi] = [min(o + b_0 e) − b_0 max e, max(o + b_0 e) − b_0 min e]`
//! (without it, the range of `o_k + e_k`), widened to hold 0 (the filter's
//! state at rest). Every past input of `G_k` lies there too, so with
//! `m, r` its middle and radius
//! `|a_k G_k(z_k)| <= |a_k| (|m G_k(1)| + r ‖g_k‖₁)`, `‖g_k‖₁` the filter's
//! certified L1 norm. Each order adds that to the curve's bound and to its
//! remainder (the triangle inequality over the parts). At `b = 0` it is
//! the static tilted stage's per-term bound, which this uses there, so a
//! tilted layer with sag alone has exactly the static tilted bound; the
//! reach is cut at [`BIAS_BOUND_BUDGET_DB`] over that bound. The per-order
//! filter and blocker states flush their decaying tails once per block
//! (`DynamicsCore::end_block`).
//!
//! # In the harmonics stage
//!
//! A Chebyshev layer with sag or bias cannot merge into the stage's
//! Chebyshev bank (its operating point moves per sample), so it runs on its
//! own, with its own envelopes and DC blocker per channel. Both depths
//! scale with the layer's amount (sag `depth_db × amount` dB, bias
//! `depth × amount`), so amount 0 (or both depths 0) keeps the layer in the
//! bank, the static stage bit for bit. The stage's sag/bias layers share
//! one supply (`harmonics_layer::Supply`): each curve's distortion `δ_j`
//! sees its own drive (`DynamicsCore::tick_parts`), and the dry signal with
//! their linear parts is turned down once, by the smallest drive, so stacked
//! sag layers never sum their turn-downs. Alone, its path is its curve minus
//! the dry signal, `p = y − x_c = (L d − 1) x_c + δ`. Its bound is a
//! `Split::Between` from leaving the supply's direct gain on the bank
//! (residual `Σ R_j` plus half the direct gain's range) to taking the dry
//! signal (alone: its own certified bound `DynamicsCore::peak_bound`, which
//! keeps the curve's joint cancellations), certified over every envelope
//! value (proof in `harmonics_layer`, "Bound"); alone in the stage, the
//! composed bound is exactly its own. It has no closed-form tone (the envelopes
//! ripple with it): readouts simulate it, presettling its envelopes to the
//! tone's periodic steady state (`DynamicsCore::presettle`) so second-long
//! envelopes cost no settling. A tilt on the layer runs in it ("Tilt"
//! above), not in the bank's filtered terms.

use crate::harmonics::{
    DC_BLOCK_HZ, HarmonicsError, LEN, Poly, READOUT_FLOOR_DB, T_AT_0, T_SLOPE_0, certified_range,
    chebyshev_bound, chebyshev_power_forms, harmonic_amplitudes, layer_amplitudes, tilt_term_bound,
};
use crate::harmonics_tilt::{TiltFilter, TiltState};
use crate::{Complex64, Stage, db_to_lin, lin_to_db};
use audivane_protocol::{
    ChebyshevLayer, HARMONICS_BIAS_MAX_DEPTH, HARMONICS_ENVELOPE_MAX_MS, HARMONICS_ENVELOPE_MIN_MS,
    HARMONICS_ORDERS, HARMONICS_SAG_MAX_DB, HarmonicsBias, HarmonicsSag, HarmonicsTilt,
};
use std::f64::consts::PI;
use std::fmt;

/// Largest sag depth, dB of drive at a full envelope.
pub const SAG_MAX_DEPTH_DB: f64 = HARMONICS_SAG_MAX_DB;
/// Largest bias shift, fraction of full scale (either sign).
pub const BIAS_MAX_DEPTH: f64 = HARMONICS_BIAS_MAX_DEPTH;
/// Envelope time constants, ms.
pub const ENVELOPE_MIN_MS: f64 = HARMONICS_ENVELOPE_MIN_MS;
pub const ENVELOPE_MAX_MS: f64 = HARMONICS_ENVELOPE_MAX_MS;
/// Cost of simulating one sag/bias layer in a harmonics readout, in tape
/// layers (measured on the readout tones at 96 kHz, presettling included:
/// 0.20 of `tape_master`'s simulation, which is 1.07 by the substep count).
pub(crate) const READOUT_COST: f64 = 0.2;
/// Most Newton steps when presettling an envelope (it converges in a few:
/// the loop's map is piecewise linear).
const PRESETTLE_STEPS: usize = 60;
/// Largest `|a_k|` accepted (a −0 dB term; layers stay far below).
const MAX_AMPLITUDE: f64 = 1.0;
/// Most the bias may raise the certified peak bound over the static
/// curve's, dB: its reach is cut where the bound would grow more.
pub const BIAS_BOUND_BUDGET_DB: f64 = 1.0;
/// Cells of the bias grid in the certification.
const BIAS_GRID: usize = 64;
/// Relative margin on the general bound, for rounding in the differences.
const ROUNDING_MARGIN: f64 = 1e-9;
/// Top of the audio band, for the order cap (as in [`crate::harmonics`]).
const AUDIO_BAND_HZ: f64 = 20_000.0;
/// State below this (`2^-100`, as in `dc_block`) is snapped to exact zero,
/// so silence never leaves subnormal state.
const FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31;
/// Highest rate the readout tones are simulated at (as the harmonics
/// readout).
const READOUT_MAX_SIM_RATE: f64 = 96_000.0;
/// Settling of a readout tone: this many of the longest time constant, at
/// least `READOUT_MIN_SETTLE_S` and at most `READOUT_MAX_SETTLE_S`.
const READOUT_SETTLE_TAUS: f64 = 8.0;
const READOUT_MIN_SETTLE_S: f64 = 0.5;
const READOUT_MAX_SETTLE_S: f64 = 30.0;
/// Measured length of a readout tone, seconds.
const READOUT_SECONDS: f64 = 0.25;

/// Why a [`TubeDynamics`] stage or core could not be built.
#[derive(Clone, Debug, PartialEq)]
pub enum TubeDynamicsError {
    ZeroChannels,
    InvalidSampleRate,
    InvalidParameter(String),
    /// The Chebyshev layer it was built from is invalid.
    Layer(HarmonicsError),
}

impl fmt::Display for TubeDynamicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => write!(f, "tube dynamics requires at least 1 channel"),
            Self::InvalidSampleRate => write!(f, "tube dynamics requires a positive sample rate"),
            Self::InvalidParameter(msg) => write!(f, "tube dynamics: {msg}"),
            Self::Layer(e) => write!(f, "tube dynamics: {e}"),
        }
    }
}

impl std::error::Error for TubeDynamicsError {}

/// Supply sag: lowers the drive by up to `depth_db` as the envelope of
/// `|x|` rises to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SagConfig {
    /// Drive reduction at a full envelope, dB (0 = off), up to
    /// [`SAG_MAX_DEPTH_DB`].
    pub depth_db: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
}

impl Default for SagConfig {
    fn default() -> Self {
        Self {
            depth_db: 1.0,
            attack_ms: 20.0,
            release_ms: 200.0,
        }
    }
}

/// Bias shift: moves the operating point to `depth × envelope`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiasConfig {
    /// Operating point at a full envelope, fraction of full scale (0 = off;
    /// the sign picks the side), up to ±[`BIAS_MAX_DEPTH`].
    pub depth: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
}

impl Default for BiasConfig {
    fn default() -> Self {
        Self {
            depth: 0.1,
            attack_ms: 300.0,
            release_ms: 2_000.0,
        }
    }
}

impl From<HarmonicsSag> for SagConfig {
    fn from(s: HarmonicsSag) -> Self {
        Self {
            depth_db: s.depth_db,
            attack_ms: s.attack_ms,
            release_ms: s.release_ms,
        }
    }
}

impl From<HarmonicsBias> for BiasConfig {
    fn from(b: HarmonicsBias) -> Self {
        Self {
            depth: b.depth,
            attack_ms: b.attack_ms,
            release_ms: b.release_ms,
        }
    }
}

/// A Chebyshev curve with sag and bias shift.
#[derive(Clone, Debug, PartialEq)]
pub struct TubeDynamicsConfig {
    pub enabled: bool,
    /// Signed `[a2, …, a9]` as [`layer_amplitudes`] gives them (drive and
    /// amount applied).
    pub amplitudes: [f64; HARMONICS_ORDERS],
    /// The 5 Hz DC blocker on the even part (as the harmonics stage).
    pub dc_block: bool,
    pub sag: SagConfig,
    pub bias: BiasConfig,
    /// The layer's tilt (`None` or flat: every order unfiltered).
    pub tilt: Option<HarmonicsTilt>,
}

impl Default for TubeDynamicsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            amplitudes: [0.0; HARMONICS_ORDERS],
            dc_block: true,
            sag: SagConfig::default(),
            bias: BiasConfig::default(),
            tilt: None,
        }
    }
}

impl TubeDynamicsConfig {
    /// The curve of a Chebyshev layer (its levels, signs, drive, amount and
    /// tilt) with `sag` and `bias`.
    pub fn from_layer(
        layer: &ChebyshevLayer,
        dc_block: bool,
        sag: SagConfig,
        bias: BiasConfig,
    ) -> Result<Self, TubeDynamicsError> {
        Ok(Self {
            enabled: layer.enabled,
            amplitudes: layer_amplitudes(layer).map_err(TubeDynamicsError::Layer)?,
            dc_block,
            sag,
            bias,
            tilt: layer.tilt.as_deref().cloned(),
        })
    }

    /// The curve of a Chebyshev layer with its own sag and bias (absent:
    /// depth 0), as the harmonics stage runs it: both depths times the
    /// layer's amount (sag `depth_db × amount` dB, bias `depth × amount`),
    /// as its terms are, so amount 0 is the dry signal and half the amount
    /// half the sag in dB.
    pub fn from_dynamic_layer(
        layer: &ChebyshevLayer,
        dc_block: bool,
    ) -> Result<Self, TubeDynamicsError> {
        let sag = layer.sag.map_or(
            SagConfig {
                depth_db: 0.0,
                ..SagConfig::default()
            },
            |s| SagConfig {
                depth_db: s.depth_db * layer.amount,
                ..SagConfig::from(s)
            },
        );
        let bias = layer.bias.map_or(
            BiasConfig {
                depth: 0.0,
                ..BiasConfig::default()
            },
            |b| BiasConfig {
                depth: b.depth * layer.amount,
                ..BiasConfig::from(b)
            },
        );
        Self::from_layer(layer, dc_block, sag, bias)
    }

    fn validate(&self) -> Result<(), TubeDynamicsError> {
        let bad = |msg: String| Err(TubeDynamicsError::InvalidParameter(msg));
        for (i, a) in self.amplitudes.iter().enumerate() {
            if !a.is_finite() || a.abs() > MAX_AMPLITUDE {
                return bad(format!("a{} must be finite and within ±1 (got {a})", i + 2));
            }
        }
        let d = self.sag.depth_db;
        if !d.is_finite() || !(0.0..=SAG_MAX_DEPTH_DB).contains(&d) {
            return bad(format!(
                "sag depth_db must be in 0..={SAG_MAX_DEPTH_DB} (got {d})"
            ));
        }
        let b = self.bias.depth;
        if !b.is_finite() || b.abs() > BIAS_MAX_DEPTH {
            return bad(format!(
                "bias depth must be within ±{BIAS_MAX_DEPTH} (got {b})"
            ));
        }
        if let Some(t) = &self.tilt {
            t.validate().map_err(TubeDynamicsError::InvalidParameter)?;
        }
        for (name, ms) in [
            ("sag attack_ms", self.sag.attack_ms),
            ("sag release_ms", self.sag.release_ms),
            ("bias attack_ms", self.bias.attack_ms),
            ("bias release_ms", self.bias.release_ms),
        ] {
            if !ms.is_finite() || !(ENVELOPE_MIN_MS..=ENVELOPE_MAX_MS).contains(&ms) {
                return bad(format!(
                    "{name} must be in {ENVELOPE_MIN_MS}..={ENVELOPE_MAX_MS} (got {ms})"
                ));
            }
        }
        Ok(())
    }

    /// Longest time constant of an envelope that is on, seconds (0: none).
    fn longest_tau_s(&self) -> f64 {
        let mut t: f64 = 0.0;
        if self.sag.depth_db > 0.0 {
            t = t.max(self.sag.attack_ms).max(self.sag.release_ms);
        }
        if self.bias.depth != 0.0 {
            t = t.max(self.bias.attack_ms).max(self.bias.release_ms);
        }
        t * 1e-3
    }
}

/// One-pole coefficient `1 − exp(−1/(τ f_s))` for a time constant in ms.
fn one_pole(ms: f64, rate: f64) -> f64 {
    -(-1.0 / (ms * 1e-3 * rate)).exp_m1()
}

/// Per-channel state of a [`DynamicsCore`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DynamicsState {
    /// Sag envelope `e_s` in `[0, 1]`.
    sag: f64,
    /// Bias envelope `e_b` in `[0, 1]`.
    bias: f64,
    /// DC blocker state.
    dc_z1: f64,
    /// Filtered orders (index `k − 2`): each one's DC blocker on its even
    /// part and its tilt filter.
    tilt_dc: [f64; HARMONICS_ORDERS],
    tilt: [TiltState; HARMONICS_ORDERS],
}

impl DynamicsState {
    /// Flush the filtered orders' decaying tails to zero (once per block,
    /// as [`TiltState::flush`]).
    #[inline]
    fn flush_tilt(&mut self) {
        for z in &mut self.tilt_dc {
            if z.abs() < FLUSH_BELOW {
                *z = 0.0;
            }
        }
        for st in &mut self.tilt {
            st.flush();
        }
    }
}

/// One Chebyshev curve with sag and bias shift, without the output scale or
/// overs: what a harmonics stage would sum per layer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DynamicsCore {
    /// Direct-path gain `s·L` (every order's slope, filtered or not).
    linear: f64,
    /// `s·a_k` by order `k` on the unfiltered path (0 below 2, for absent
    /// terms and for filtered orders).
    coef: [f64; LEN],
    /// `s·(k+1)·a_{k+1}` by `k`: the weights of `U_k(b)` in `s·S'(b)` (the
    /// unfiltered orders').
    dcoef: [f64; LEN],
    /// Highest order with an unfiltered term (1: none).
    top: usize,
    /// The filtered orders' tilt filters by order `k`, weighted by `s·a_k`
    /// (module docs, "Tilt").
    tilt: [Option<TiltFilter>; LEN],
    /// Whether an order is filtered (the tilted evaluation runs).
    tilted: bool,
    /// Highest order with a term, filtered or not (1: none).
    top_all: usize,
    dc_block: bool,
    hpf_b0: f64,
    hpf_a1: f64,
    sag_on: bool,
    /// `1 − 10^(−D/20)`: the drive is `1 − sag_k · e_s`.
    sag_k: f64,
    sag_att: f64,
    sag_rel: f64,
    bias_on: bool,
    bias_depth: f64,
    /// The certified reach: `b = β e_b` clamped to `[bias_lo, bias_hi]`.
    bias_lo: f64,
    bias_hi: f64,
    bias_att: f64,
    bias_rel: f64,
    /// Unscaled `L`.
    small_signal: f64,
    /// Unscaled certified bound of the output over `|x| <= 1` and every
    /// reachable envelope, and of the remainder `y − L u`.
    bound: f64,
    remainder: f64,
    amps: [f64; HARMONICS_ORDERS],
}

impl DynamicsCore {
    /// Build at `output_rate` (validated; off the audio thread: the bound is
    /// certified here).
    pub(crate) fn try_new(
        config: &TubeDynamicsConfig,
        output_rate: f64,
    ) -> Result<Self, TubeDynamicsError> {
        if !output_rate.is_finite() || output_rate <= 0.0 {
            return Err(TubeDynamicsError::InvalidSampleRate);
        }
        config.validate()?;
        let mut a: Poly = [0.0; LEN];
        a[2..].copy_from_slice(&config.amplitudes);
        // As the harmonics stage computes it (same order of operations).
        let linear = 1.0
            + (3..LEN)
                .step_by(2)
                .map(|k| a[k] * T_SLOPE_0[k])
                .sum::<f64>();
        // Orders with a filter leave the unfiltered curve (their slopes stay
        // in `linear`).
        let mut tilt = [None; LEN];
        let mut terms = Vec::new();
        if let Some(t) = &config.tilt {
            for k in 2..LEN {
                if a[k] == 0.0 {
                    continue;
                }
                if let Some(f) = TiltFilter::new(&t.order_shelves(k), k, output_rate) {
                    tilt[k] = Some(f.scaled(a[k]));
                    terms.push(CertTerm::new(k, f, a[k]));
                    a[k] = 0.0;
                }
            }
        }
        let top = (2..LEN).rev().find(|&k| a[k] != 0.0).unwrap_or(1);
        let top_all = terms.iter().map(|t| t.k).fold(top, usize::max);
        let mut dcoef = [0.0; LEN];
        for j in 1..LEN - 1 {
            dcoef[j] = (j + 1) as f64 * a[j + 1];
        }

        let fc = DC_BLOCK_HZ.min(0.1 * output_rate);
        let k = (PI * fc / output_rate).tan();
        let hpf_b0 = 1.0 / (1.0 + k);
        let hpf_a1 = (k - 1.0) / (1.0 + k);

        let beta = config.bias.depth;
        let cert = certify(&a, &terms, linear, config.dc_block, hpf_b0, beta);
        let sag_k = -(-config.sag.depth_db / 20.0 * std::f64::consts::LN_10).exp_m1();
        Ok(Self {
            linear,
            coef: a,
            dcoef,
            top,
            tilted: !terms.is_empty(),
            tilt,
            top_all,
            dc_block: config.dc_block,
            hpf_b0,
            hpf_a1,
            sag_on: config.sag.depth_db > 0.0,
            sag_k,
            sag_att: one_pole(config.sag.attack_ms, output_rate),
            sag_rel: one_pole(config.sag.release_ms, output_rate),
            bias_on: cert.reach != 0.0,
            bias_depth: beta,
            bias_lo: cert.reach.min(0.0),
            bias_hi: cert.reach.max(0.0),
            bias_att: one_pole(config.bias.attack_ms, output_rate),
            bias_rel: one_pole(config.bias.release_ms, output_rate),
            small_signal: linear,
            bound: cert.bound,
            remainder: cert.remainder,
            amps: config.amplitudes,
        })
    }

    /// The same core with its output multiplied by `s` (baked into the
    /// coefficients, as the harmonics stage does).
    pub(crate) fn scaled(mut self, s: f64) -> Self {
        self.linear *= s;
        self.coef = self.coef.map(|c| c * s);
        self.dcoef = self.dcoef.map(|c| c * s);
        self.tilt = self.tilt.map(|f| f.map(|f| f.scaled(s)));
        self
    }

    /// Unscaled small-signal gain `L` with the envelopes at rest.
    pub(crate) fn small_signal(&self) -> f64 {
        self.small_signal
    }

    /// Certified bound `B` of the (unscaled) output over `|x| <= 1` and
    /// every envelope value.
    pub(crate) fn peak_bound(&self) -> f64 {
        self.bound
    }

    /// `(L_min, L_max, R)`: the output is `L_d · x_c` with the sag's direct
    /// gain `L_d = L d` between `L_min` and `L_max`, plus a remainder of at
    /// most `R` (unscaled, inputs within full scale, any envelope).
    pub(crate) fn split(&self) -> (f64, f64, f64) {
        let lo = self.small_signal * (1.0 - self.sag_k);
        let hi = self.small_signal;
        (lo.min(hi), lo.max(hi), self.remainder)
    }

    /// Orders with a term that fold back into the audio band at
    /// `output_rate` (the harmonics stage's rule: `k × 20 kHz > fs − 20 kHz`).
    /// Sag and bias never raise the degree; the sag's gain modulation adds
    /// only narrow sidebands around each harmonic.
    pub(crate) fn aliased_orders(&self, output_rate: f64) -> Vec<usize> {
        (2..LEN)
            .filter(|&k| self.amps[k - 2] != 0.0)
            .filter(|&k| k as f64 * AUDIO_BAND_HZ > output_rate - AUDIO_BAND_HZ)
            .collect()
    }

    /// `(depth, reach)` when the certified reach is short of the bias depth.
    pub(crate) fn bias_limit(&self) -> Option<(f64, f64)> {
        let reach = if self.bias_on {
            self.bias_lo + self.bias_hi
        } else {
            0.0
        };
        (reach.abs() < self.bias_depth.abs()).then_some((self.bias_depth, reach))
    }

    /// `(drive d, operating point b)` of state `st` now: what the sag and
    /// the bias envelope hold (a read, nothing steps).
    #[inline]
    pub(crate) fn operating_point(&self, st: &DynamicsState) -> (f64, f64) {
        let d = if self.sag_on {
            1.0 - self.sag_k * st.sag
        } else {
            1.0
        };
        let b = if self.bias_on {
            (self.bias_depth * st.bias).clamp(self.bias_lo, self.bias_hi)
        } else {
            0.0
        };
        (d, b)
    }

    /// Set both envelopes of `st` to their periodic steady state for the
    /// clamped input `xc` looped forever (their value at the loop's start),
    /// leaving the DC blocker as it is. Off the audio thread: a few passes
    /// of the followers over `xc`.
    pub(crate) fn presettle(&self, st: &mut DynamicsState, xc: &[f64]) {
        if self.sag_on {
            st.sag = steady_envelope(xc, self.sag_att, self.sag_rel);
        }
        if self.bias_on {
            st.bias = steady_envelope(xc, self.bias_att, self.bias_rel);
        }
    }

    /// One sample for the clamped input `xc`.
    #[inline(always)]
    pub(crate) fn tick(&self, st: &mut DynamicsState, xc: f64) -> f64 {
        if self.tilted {
            self.tick_as::<true>(st, xc)
        } else {
            self.tick_as::<false>(st, xc)
        }
    }

    /// [`Self::tick`] with the tilt known ([`Self::tilted`]): a loop over
    /// samples picks it once, so an untilted core's state stays in
    /// registers.
    #[inline(always)]
    pub(crate) fn tick_as<const TILTED: bool>(&self, st: &mut DynamicsState, xc: f64) -> f64 {
        let (u, _, o, e) = self.step::<TILTED>(st, xc);
        self.linear * u + o + e
    }

    /// Whether an order is filtered.
    #[inline(always)]
    pub(crate) fn tilted(&self) -> bool {
        self.tilted
    }

    /// One sample split for the harmonics stage's shared supply: the
    /// curve's pure distortion `δ_o + δ_e` at its own drive (DC blocker
    /// included) and that drive `d`; the direct path `L d x` is left to the
    /// stage ([`crate::harmonics_layer`], "Shared supply").
    #[inline(always)]
    pub(crate) fn tick_parts(&self, st: &mut DynamicsState, xc: f64) -> (f64, f64) {
        if self.tilted {
            self.tick_parts_as::<true>(st, xc)
        } else {
            self.tick_parts_as::<false>(st, xc)
        }
    }

    /// [`Self::tick_parts`] with the tilt known (as [`Self::tick_as`]).
    #[inline(always)]
    pub(crate) fn tick_parts_as<const TILTED: bool>(
        &self,
        st: &mut DynamicsState,
        xc: f64,
    ) -> (f64, f64) {
        let (_, d, o, e) = self.step::<TILTED>(st, xc);
        (o + e, d)
    }

    /// `(u, d, δ_o, δ_e)` of one sample (envelopes and DC blocker stepped).
    #[inline(always)]
    fn step<const TILTED: bool>(&self, st: &mut DynamicsState, xc: f64) -> (f64, f64, f64, f64) {
        let r = xc.abs();
        let d = if self.sag_on {
            st.sag = follow(st.sag, r, self.sag_att, self.sag_rel);
            1.0 - self.sag_k * st.sag
        } else {
            1.0
        };
        let u = d * xc;
        let b = if self.bias_on {
            st.bias = follow(st.bias, r, self.bias_att, self.bias_rel);
            Some((self.bias_depth * st.bias).clamp(self.bias_lo, self.bias_hi))
        } else {
            None
        };
        let (o, e) = self.parts::<TILTED>(st, u, b);
        (u, d, o, e)
    }

    /// `(δ_o, δ_e)` at drive-scaled input `u` and operating point `b`
    /// (`None`: 0, the centred evaluation), the filtered orders in `δ_o`,
    /// `δ_e` through the DC blocker.
    #[inline(always)]
    fn parts<const TILTED: bool>(
        &self,
        st: &mut DynamicsState,
        u: f64,
        b: Option<f64>,
    ) -> (f64, f64) {
        let (o, e) = match b {
            Some(b) if TILTED => self.shifted_tilted(st, u, b),
            Some(b) => self.shifted(u, b),
            None if TILTED => self.centred_tilted(st, u),
            None => self.centred(u),
        };
        let e = if self.dc_block {
            // TDF-II: y = b0 e + z1; z1 = -b0 e - a1 y.
            let y = self.hpf_b0 * e + st.dc_z1;
            st.dc_z1 = -self.hpf_b0 * e - self.hpf_a1 * y;
            if st.dc_z1.abs() < FLUSH_BELOW {
                st.dc_z1 = 0.0;
            }
            y
        } else {
            e
        };
        (o, e)
    }

    /// End of a block: the filtered orders' states flush their decaying
    /// tails (nothing to do without tilt).
    #[inline]
    pub(crate) fn end_block(&self, st: &mut DynamicsState) {
        if self.tilted {
            st.flush_tilt();
        }
    }

    /// Its cost per sample relative to an untilted core (measured at 48 and
    /// 768 kHz: about 1.9× with a tilt on the preset curves' orders, 2.3×
    /// on all eight).
    pub(crate) fn cost_factor(&self) -> f64 {
        if self.tilted { 2.0 } else { 1.0 }
    }

    /// Lowest drive the sag can reach, `10^(−D/20)` (1 without sag).
    pub(crate) fn min_drive(&self) -> f64 {
        1.0 - self.sag_k
    }

    /// Unscaled certified bound of the remainder `y − L u` (the distortion
    /// at any drive and bias point, DC blocker included).
    pub(crate) fn remainder_bound(&self) -> f64 {
        self.remainder
    }

    /// `(δ_o, δ_e)` at `b = 0`, term by term exactly as the harmonics stage
    /// evaluates them: `D_k = T_k − T_k'(0) u − T_k(0)`.
    #[inline(always)]
    fn centred(&self, u: f64) -> (f64, f64) {
        let two_x = 2.0 * u;
        let (mut t_prev, mut t) = (1.0, u);
        let mut acc = [0.0; 2];
        for k in 2..=self.top {
            let t_next = two_x * t - t_prev;
            t_prev = t;
            t = t_next;
            let d = t - T_SLOPE_0[k] * u - T_AT_0[k];
            acc[k & 1] += self.coef[k] * d;
        }
        let [e, o] = acc;
        (o, e)
    }

    /// `(δ_o, δ_e)` at operating point `b`: one Chebyshev recurrence on four
    /// lanes, `T_k(b + u)`, `T_k(b − u)`, `T_k(b)` and `U_k(b)`, giving
    /// `S(b ± u)`, `S(b)` and `S'(b) = Σ (k+1) a_{k+1} U_k(b)`.
    #[inline(always)]
    fn shifted(&self, u: f64, b: f64) -> (f64, f64) {
        let x = [b + u, b - u, b, b];
        let two = x.map(|v| 2.0 * v);
        let mut t_prev = [1.0; 4];
        // T_1 on three lanes, U_1 = 2b on the last.
        let mut t = [x[0], x[1], x[2], two[3]];
        let mut acc = [0.0, 0.0, 0.0, self.dcoef[1] * t[3]];
        for k in 2..=self.top {
            let t_next: [f64; 4] = std::array::from_fn(|i| two[i] * t[i] - t_prev[i]);
            t_prev = t;
            t = t_next;
            let (c, dc) = (self.coef[k], self.dcoef[k]);
            let w = [c, c, c, dc];
            for ((s, w), t) in acc.iter_mut().zip(w).zip(t) {
                *s += w * t;
            }
        }
        let [s_plus, s_minus, s_b, ds_b] = acc;
        let o = 0.5 * (s_plus - s_minus) - ds_b * u;
        let e = 0.5 * (s_plus + s_minus) - s_b;
        (o, e)
    }

    /// One step of a filtered order's DC blocker (TDF-II, as the core's).
    #[inline(always)]
    fn order_dc_step(&self, e: f64, z1: &mut f64) -> f64 {
        let y = self.hpf_b0 * e + *z1;
        *z1 = -self.hpf_b0 * e - self.hpf_a1 * y;
        y
    }

    /// [`Self::centred`] with filtered orders: each filtered `D_k` (an even
    /// order's through its own DC blocker) through its tilt filter, their
    /// sum added to `δ_o` (at `b = 0` an odd order is all odd part and an
    /// even one all even part).
    #[inline(always)]
    fn centred_tilted(&self, st: &mut DynamicsState, u: f64) -> (f64, f64) {
        let two_x = 2.0 * u;
        let (mut t_prev, mut t) = (1.0, u);
        let mut acc = [0.0; 2];
        let mut filtered = 0.0;
        for k in 2..=self.top_all {
            let t_next = two_x * t - t_prev;
            t_prev = t;
            t = t_next;
            let d = t - T_SLOPE_0[k] * u - T_AT_0[k];
            acc[k & 1] += self.coef[k] * d;
            if let Some(f) = &self.tilt[k] {
                let i = k - 2;
                let z = if self.dc_block && k & 1 == 0 {
                    self.order_dc_step(d, &mut st.tilt_dc[i])
                } else {
                    d
                };
                filtered += f.tick(&mut st.tilt[i], z);
            }
        }
        let [e, o] = acc;
        (o + filtered, e)
    }

    /// [`Self::shifted`] with filtered orders: order `k`'s own parts at `b`,
    ///
    /// ```text
    /// e_k = (T_k(b+u) + T_k(b−u))/2 − T_k(b)
    /// o_k = (T_k(b+u) − T_k(b−u))/2 − k U_{k−1}(b) u
    /// ```
    ///
    /// (`U_{k−1}(b)` is the fourth lane before step `k`), go through
    /// `G_k(o_k + HPF(e_k))`, its own DC blocker and tilt filter; their sum
    /// is added to `δ_o`.
    #[inline(always)]
    fn shifted_tilted(&self, st: &mut DynamicsState, u: f64, b: f64) -> (f64, f64) {
        let x = [b + u, b - u, b, b];
        let two = x.map(|v| 2.0 * v);
        let mut t_prev = [1.0; 4];
        let mut t = [x[0], x[1], x[2], two[3]];
        let mut acc = [0.0, 0.0, 0.0, self.dcoef[1] * t[3]];
        let mut filtered = 0.0;
        for k in 2..=self.top_all {
            let u_prev = t[3];
            let t_next: [f64; 4] = std::array::from_fn(|i| two[i] * t[i] - t_prev[i]);
            t_prev = t;
            t = t_next;
            let (c, dc) = (self.coef[k], self.dcoef[k]);
            let w = [c, c, c, dc];
            for ((s, w), t) in acc.iter_mut().zip(w).zip(t) {
                *s += w * t;
            }
            if let Some(f) = &self.tilt[k] {
                let i = k - 2;
                let e = 0.5 * (t[0] + t[1]) - t[2];
                let o = 0.5 * (t[0] - t[1]) - ORDER[k] * u_prev * u;
                let e = if self.dc_block {
                    self.order_dc_step(e, &mut st.tilt_dc[i])
                } else {
                    e
                };
                filtered += f.tick(&mut st.tilt[i], o + e);
            }
        }
        let [s_plus, s_minus, s_b, ds_b] = acc;
        let o = 0.5 * (s_plus - s_minus) - ds_b * u;
        let e = 0.5 * (s_plus + s_minus) - s_b;
        (o + filtered, e)
    }
}

/// `k` as a float, by `k`.
const ORDER: [f64; LEN] = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0];

/// One step of an attack/release follower of `r` in `[0, 1]`.
#[inline(always)]
fn follow(e: f64, r: f64, att: f64, rel: f64) -> f64 {
    let a = if r > e { att } else { rel };
    let e = e + a * (r - e);
    // `e` stays in [0, 1] (a convex step); snap the decay tail.
    if e < FLUSH_BELOW { 0.0 } else { e.min(1.0) }
}

/// The fixed point `e₀ = F(e₀)` of the follower run over `xc` (`F` maps
/// the envelope at the loop's start to its value after the loop). `F` is
/// nondecreasing and piecewise linear with slope `Π(1 − α)` below 1, so
/// `F(e) − e` falls strictly and changes sign once in `[0, 1]`: Newton steps
/// on it, kept inside a bracket (bisection when a step leaves it), find the
/// root in a few passes.
fn steady_envelope(xc: &[f64], att: f64, rel: f64) -> f64 {
    let run = |e0: f64| -> (f64, f64) {
        let (mut e, mut slope) = (e0, 1.0);
        for &x in xc {
            let r = x.abs();
            slope *= 1.0 - if r > e { att } else { rel };
            e = follow(e, r, att, rel);
        }
        (e, slope)
    };
    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
    let mut e = 0.0;
    for _ in 0..PRESETTLE_STEPS {
        let (f, slope) = run(e);
        let g = f - e;
        if g.abs() <= 1e-14 {
            return e;
        }
        if g > 0.0 {
            lo = e;
        } else {
            hi = e;
        }
        if hi - lo <= 1e-15 {
            break;
        }
        let next = e + g / (1.0 - slope);
        e = if next > lo && next < hi {
            next
        } else {
            0.5 * (lo + hi)
        };
    }
    e.clamp(0.0, 1.0)
}

fn binomials() -> [[f64; LEN]; LEN] {
    let mut c = [[0.0; LEN]; LEN];
    for m in 0..LEN {
        c[m][0] = 1.0;
        for j in 1..=m {
            c[m][j] = c[m - 1][j - 1] + if j < m { c[m - 1][j] } else { 0.0 };
        }
    }
    c
}

/// Power form of `S = Σ_{k>=2} a_k T_k` (`a` by order).
fn power_form(a: &Poly) -> Poly {
    let t = chebyshev_power_forms();
    let mut p: Poly = [0.0; LEN];
    for k in 2..LEN {
        for m in 0..LEN {
            p[m] += a[k] * t[k][m];
        }
    }
    p
}

/// Taylor coefficients of `p` at `b`: `q_j = Σ_m p_m C(m, j) b^(m−j)`.
fn taylor(p: &Poly, b: f64, c: &[[f64; LEN]; LEN]) -> Poly {
    std::array::from_fn(|j| {
        let mut s = 0.0;
        let mut bp = 1.0;
        for m in j..LEN {
            s += p[m] * c[m][j] * bp;
            bp *= b;
        }
        s
    })
}

/// Power forms in `u` of the output's odd part `L u + δ_o` and even part
/// `δ_e` at operating point `b`.
fn parts_at(p: &Poly, b: f64, linear: f64, c: &[[f64; LEN]; LEN]) -> (Poly, Poly) {
    let q = taylor(p, b, c);
    let mut odd: Poly = [0.0; LEN];
    let mut even: Poly = [0.0; LEN];
    odd[1] = linear;
    for j in 2..LEN {
        if j % 2 == 0 {
            even[j] = q[j];
        } else {
            odd[j] = q[j];
        }
    }
    (odd, even)
}

/// The certified range of the output: bounds over `|u| <= 1`, every `b`
/// the bias can reach, and any past (DC blocker included).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Certified {
    /// Bound of the output.
    bound: f64,
    /// Bound of the remainder `y − L u`.
    remainder: f64,
    /// The bias reach: `b` stays between 0 and this (same sign as `β`).
    reach: f64,
}

/// One filtered order in the certification (module docs, "Tilt").
#[derive(Clone, Debug)]
struct CertTerm {
    k: usize,
    /// `|a_k|`.
    weight: f64,
    filter: TiltFilter,
    /// `G_k(1)` and a certified bound of `‖g_k‖₁` (unit weight).
    dc_gain: f64,
    l1: f64,
    /// Power form of `T_k`.
    unit: Poly,
}

impl CertTerm {
    fn new(k: usize, filter: TiltFilter, a: f64) -> Self {
        let mut e: Poly = [0.0; LEN];
        e[k] = 1.0;
        Self {
            k,
            weight: a.abs(),
            filter,
            dc_gain: filter.dc_gain(),
            l1: filter.l1_norm(),
            unit: power_form(&e),
        }
    }

    /// Bound of `|a_k| G_k(z)` for `z` within `[lo, hi]` (widened to hold
    /// 0, the filter's state at rest): the middle at the DC gain plus the
    /// radius times the L1 norm.
    fn filtered(&self, (lo, hi): (f64, f64)) -> f64 {
        let (lo, hi) = (lo.min(0.0), hi.max(0.0));
        let (mid, rad) = (0.5 * (lo + hi), 0.5 * (hi - lo));
        self.weight * ((mid * self.dc_gain).abs() + rad * self.l1)
    }
}

/// Certify the output over `b` between 0 and `beta`, for the unfiltered
/// curve `a` and the filtered orders `terms`. The reach is cut where the
/// bound would exceed the static curve's (at least 1) by
/// [`BIAS_BOUND_BUDGET_DB`]: the grid is walked outward from `b = 0`, and the
/// bound over `[0, b_i]` only grows with `i`.
fn certify(
    a: &Poly,
    terms: &[CertTerm],
    linear: f64,
    dc_block: bool,
    b0: f64,
    beta: f64,
) -> Certified {
    // The static tilted curve: the per-term tilt bound (its filtered terms
    // in the same order). The static stage takes the tighter joint bound
    // where it can, so a tilted layer with sag or bias is trimmed somewhat
    // more than the same layer without.
    let own: f64 = terms
        .iter()
        .map(|t| t.weight * tilt_term_bound(t.k, &t.filter, dc_block, b0))
        .sum();
    let fixed = chebyshev_bound(a, linear, dc_block, b0) + own;
    let at_rest = Certified {
        bound: fixed,
        remainder: chebyshev_bound(a, 0.0, dc_block, b0) + own,
        reach: 0.0,
    };
    if beta == 0.0 {
        return at_rest;
    }
    let limit = fixed.max(1.0) * db_to_lin(BIAS_BOUND_BUDGET_DB);
    let c = binomials();
    let p = power_form(a);
    // |q_j(b)| <= Q_j for |b| <= |β|.
    let r = beta.abs();
    // ∂²q_j/∂b² = (j+1)(j+2) q_{j+2}; |u| <= 1 and the weights are <= 1, so
    // between grid points the output moves at most h²/8 · m2 off the chord.
    let m2_of = |p: &Poly| -> f64 {
        let q_max = |j: usize| -> f64 {
            if j >= LEN {
                return 0.0;
            }
            (j..LEN)
                .map(|m| p[m].abs() * c[m][j] * r.powi((m - j) as i32))
                .sum()
        };
        (2..LEN)
            .map(|j| ((j + 1) * (j + 2)) as f64 * q_max(j + 2))
            .sum()
    };
    let h = r / BIAS_GRID as f64;
    let margin = h * h / 8.0 * m2_of(&p);
    let term_margins: Vec<f64> = terms.iter().map(|t| h * h / 8.0 * m2_of(&t.unit)).collect();

    let w = if dc_block { b0 } else { 1.0 };
    let empty = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut full, mut rem, mut even_r) = (empty, empty, empty);
    let widen = |acc: &mut (f64, f64), (lo, hi): (f64, f64)| {
        acc.0 = acc.0.min(lo);
        acc.1 = acc.1.max(hi);
    };
    // The range of `f − b0 w` (`f` the part through the blocker's direct
    // gain, `w` an average of past `e`), or of `f` without the blocker.
    let range = |(lo, hi): (f64, f64), even_r: (f64, f64), margin: f64| -> (f64, f64) {
        let (lo, hi) = (lo - margin, hi + margin);
        if dc_block {
            let (e_lo, e_hi) = (even_r.0 - margin, even_r.1 + margin);
            (lo - b0 * e_hi, hi - b0 * e_lo)
        } else {
            (lo, hi)
        }
    };
    let peak = |(lo, hi): (f64, f64)| hi.max(-lo);
    // Each filtered order's `(o_k + w e_k, e_k)` ranges so far.
    let mut term_r = vec![(empty, empty); terms.len()];
    let mut best = at_rest;
    for i in 0..=BIAS_GRID {
        let b = if i == BIAS_GRID {
            beta
        } else {
            beta * i as f64 / BIAS_GRID as f64
        };
        let (odd, even) = parts_at(&p, b, linear, &c);
        let mut f: Poly = std::array::from_fn(|j| odd[j] + w * even[j]);
        widen(&mut full, certified_range(&f));
        f[1] = 0.0;
        widen(&mut rem, certified_range(&f));
        if dc_block {
            widen(&mut even_r, certified_range(&even));
        }
        // z_k = o_k + HPF(e_k) lies in its range, so |a_k G_k(z_k)| within
        // the term's bound.
        let mut filtered = 0.0;
        for ((t, zr), &m) in terms.iter().zip(term_r.iter_mut()).zip(&term_margins) {
            let (odd, even) = parts_at(&t.unit, b, 0.0, &c);
            let f: Poly = std::array::from_fn(|j| odd[j] + w * even[j]);
            widen(&mut zr.0, certified_range(&f));
            if dc_block {
                widen(&mut zr.1, certified_range(&even));
            }
            filtered += t.filtered(range(zr.0, zr.1, m));
        }
        let bound = (peak(range(full, even_r, margin)) + filtered) * (1.0 + ROUNDING_MARGIN);
        if bound > limit {
            break;
        }
        if i > 0 {
            best = Certified {
                bound,
                remainder: (peak(range(rem, even_r, margin)) + filtered) * (1.0 + ROUNDING_MARGIN),
                reach: b,
            };
        }
    }
    best
}

/// Sag and bias shift on one Chebyshev curve as a stage:
/// `y = s · core(x_c) + s · (x − x_c)`.
#[derive(Clone, Debug)]
pub struct TubeDynamics {
    enabled: bool,
    channels: usize,
    /// Scaled by `s`.
    core: DynamicsCore,
    scale: f64,
    state: Vec<DynamicsState>,
}

impl TubeDynamics {
    /// Build at `output_rate` for `channels` (off the audio thread).
    pub fn try_new(
        config: &TubeDynamicsConfig,
        output_rate: f64,
        channels: usize,
    ) -> Result<Self, TubeDynamicsError> {
        Self::build(config, output_rate, channels, true)
    }

    /// [`Self::try_new`] without the output scale: the layer as the
    /// harmonics stage runs it.
    pub fn try_unscaled(
        config: &TubeDynamicsConfig,
        output_rate: f64,
        channels: usize,
    ) -> Result<Self, TubeDynamicsError> {
        Self::build(config, output_rate, channels, false)
    }

    fn build(
        config: &TubeDynamicsConfig,
        output_rate: f64,
        channels: usize,
        scaled: bool,
    ) -> Result<Self, TubeDynamicsError> {
        if channels == 0 {
            return Err(TubeDynamicsError::ZeroChannels);
        }
        let core = DynamicsCore::try_new(config, output_rate)?;
        let bound = core.peak_bound();
        let scale = if scaled && bound > 1.0 {
            1.0 / bound
        } else {
            1.0
        };
        Ok(Self {
            enabled: config.enabled,
            channels,
            core: core.scaled(scale),
            scale,
            state: vec![DynamicsState::default(); channels],
        })
    }

    /// Certified bound of the unscaled output over `|x| <= 1` and every
    /// envelope value.
    pub fn peak_bound(&self) -> f64 {
        self.core.peak_bound()
    }

    /// Output scale `s = min(1, 1/B)`.
    pub fn scale(&self) -> f64 {
        self.scale
    }

    /// Output scale in dB (<= 0); 0 when off.
    pub fn peak_trim_db(&self) -> f64 {
        if self.enabled {
            lin_to_db(self.scale)
        } else {
            0.0
        }
    }

    /// Gain for small signals with the envelopes at rest, dB: `s·L`
    /// (floored as the harmonics readouts are).
    pub fn small_signal_gain_db(&self) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        let db = lin_to_db(self.scale * self.core.small_signal);
        if db.is_finite() {
            db.max(READOUT_FLOOR_DB)
        } else {
            READOUT_FLOOR_DB
        }
    }

    /// The bias reach: `b` stays between 0 and this. It equals the depth
    /// unless the curve's terms would cost more than
    /// [`BIAS_BOUND_BUDGET_DB`] of output scale there (a warning).
    pub fn bias_reach(&self) -> f64 {
        if self.core.bias_on {
            self.core.bias_lo + self.core.bias_hi
        } else {
            0.0
        }
    }

    /// Whether the bias reach was cut short of the configured depth.
    pub fn bias_limited(&self) -> bool {
        self.bias_reach().abs() < self.core.bias_depth.abs()
    }

    /// `(L_min, L_max, R)` of the core's split (unscaled).
    pub fn split(&self) -> (f64, f64, f64) {
        self.core.split()
    }

    /// Orders that fold back into the audio band at `output_rate` (the
    /// harmonics stage warns of them too).
    pub fn aliased_orders(&self, output_rate: f64) -> Vec<usize> {
        if self.enabled {
            self.core.aliased_orders(output_rate)
        } else {
            Vec::new()
        }
    }

    /// `(drive d, operating point b)` of `channel` now.
    pub fn operating_point(&self, channel: usize) -> (f64, f64) {
        self.core.operating_point(&self.state[channel])
    }

    /// `(sag envelope, bias envelope)` of `channel`, each in `[0, 1]`.
    pub fn envelopes(&self, channel: usize) -> (f64, f64) {
        let st = &self.state[channel];
        (st.sag, st.bias)
    }
}

/// One channel of [`TubeDynamics`] in place, the tilt known.
#[inline(always)]
fn run_channel<const TILTED: bool>(
    core: &DynamicsCore,
    st: &mut DynamicsState,
    scale: f64,
    chan: &mut [f64],
) {
    let mut s = *st;
    for v in chan.iter_mut() {
        let x = if v.is_finite() { *v } else { 0.0 };
        let xc = x.clamp(-1.0, 1.0);
        let over = (x - xc) * scale;
        *v = core.tick_as::<TILTED>(&mut s, xc) + over;
    }
    *st = s;
}

impl Stage for TubeDynamics {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if !self.enabled {
            return;
        }
        let nch = chans.len().min(self.channels);
        let core = self.core;
        let scale = self.scale;
        for (c, chan) in chans.iter_mut().take(nch).enumerate() {
            let st = &mut self.state[c];
            if core.tilted {
                run_channel::<true>(&core, st, scale, chan);
            } else {
                run_channel::<false>(&core, st, scale, chan);
            }
            core.end_block(st);
        }
    }

    fn reset(&mut self) {
        self.state.fill(DynamicsState::default());
    }
}

// ---------------------------------------------------------------- readout

/// One "under load" point: a steady sine after the envelopes settle, next
/// to the static curve at the same level.
#[derive(Clone, Debug, PartialEq)]
pub struct UnderLoadPoint {
    pub input_dbfs: f64,
    pub freq_hz: f64,
    /// Mean sag drive over the measured window, dB (<= 0).
    pub sag_drive_db: f64,
    /// Mean operating point over the window, fraction of full scale.
    pub bias: f64,
    /// Fundamental under load minus the static curve's, dB (the sag's level
    /// law; both with this stage's output scale).
    pub fundamental_change_db: f64,
    /// H2–H9 of the static curve at this level, dBc (closed form).
    pub static_db: [f64; HARMONICS_ORDERS],
    /// H2–H9 under load, dBc (simulated).
    pub loaded_db: [f64; HARMONICS_ORDERS],
}

/// Under-load readout at `output_rate` for sines at `freq_hz` and each of
/// `levels_dbfs`: the envelopes settle on the steady sine (8 of the longest
/// time constant), then a coherent window is measured. Simulated at most at
/// 96 kHz, as the harmonics readout. Off the audio thread (allocates; tens
/// of ms per point with second-long envelopes).
pub fn under_load(
    config: &TubeDynamicsConfig,
    output_rate: f64,
    freq_hz: f64,
    levels_dbfs: &[f64],
) -> Result<Vec<UnderLoadPoint>, TubeDynamicsError> {
    let rate = output_rate.min(READOUT_MAX_SIM_RATE);
    let stage = TubeDynamics::try_new(
        &TubeDynamicsConfig {
            enabled: true,
            ..config.clone()
        },
        rate,
        1,
    )?;
    if !freq_hz.is_finite() || freq_hz <= 0.0 || 2.0 * freq_hz >= rate {
        return Err(TubeDynamicsError::InvalidParameter(format!(
            "readout frequency {freq_hz} Hz must be below half of {rate} Hz"
        )));
    }
    Ok(levels_dbfs
        .iter()
        .map(|&db| under_load_point(&stage, config, rate, freq_hz, db))
        .collect())
}

fn under_load_point(
    stage: &TubeDynamics,
    config: &TubeDynamicsConfig,
    rate: f64,
    freq: f64,
    level_db: f64,
) -> UnderLoadPoint {
    let amp = db_to_lin(level_db);
    let settle_s = (READOUT_SETTLE_TAUS * config.longest_tau_s())
        .clamp(READOUT_MIN_SETTLE_S, READOUT_MAX_SETTLE_S);
    let tone = simulate_tone(stage, rate, freq, amp, settle_s);
    let h = &tone.harmonics;
    let h1 = h[1].norm();
    let dbc = |v: f64, h1: f64| {
        let db = lin_to_db(v / h1);
        if db.is_finite() {
            db.max(READOUT_FLOOR_DB)
        } else {
            READOUT_FLOOR_DB
        }
    };
    let st = harmonic_amplitudes(config.amplitudes, amp);
    let st1 = st[1].abs();
    UnderLoadPoint {
        input_dbfs: level_db,
        freq_hz: freq,
        sag_drive_db: lin_to_db(tone.mean_drive),
        bias: tone.mean_bias,
        fundamental_change_db: lin_to_db(h1 / (stage.scale * st1)),
        static_db: std::array::from_fn(|i| dbc(st[i + 2].abs(), st1)),
        loaded_db: std::array::from_fn(|i| dbc(h[i + 2].norm(), h1)),
    }
}

/// A simulated tone: complex `[DC, H1, …, H9]` and the mean envelopes.
pub(crate) struct Tone {
    pub(crate) harmonics: [Complex64; LEN],
    pub(crate) mean_drive: f64,
    pub(crate) mean_bias: f64,
}

/// Run a cosine at about `freq` (made coherent with the window) and peak
/// `amp` through a copy of `stage` (reset) for `settle_s`, then measure a
/// window of about [`READOUT_SECONDS`].
pub(crate) fn simulate_tone(
    stage: &TubeDynamics,
    rate: f64,
    freq: f64,
    amp: f64,
    settle_s: f64,
) -> Tone {
    let mut stage = stage.clone();
    stage.reset();
    let cycles = (freq * READOUT_SECONDS).round().max(4.0);
    let n = (cycles * rate / freq).round() as usize;
    let w = 2.0 * PI * cycles / n as f64;
    let settle = (settle_s * rate) as usize;
    // Settle in blocks (no buffer of the whole settling time).
    let mut buf = vec![0.0; 4096];
    let mut i = 0;
    while i < settle {
        let len = buf.len().min(settle - i);
        for (j, s) in buf[..len].iter_mut().enumerate() {
            *s = amp * (w * (i + j) as f64).cos();
        }
        stage.process(&mut [&mut buf[..len]]);
        i += len;
    }
    let (mut sum_d, mut sum_b) = (0.0, 0.0);
    let mut tail = vec![0.0; n];
    for (j, y) in tail.iter_mut().enumerate() {
        let mut s = [amp * (w * (settle + j) as f64).cos()];
        stage.process(&mut [&mut s]);
        *y = s[0];
        let (d, b) = stage.operating_point(0);
        sum_d += d;
        sum_b += b;
    }
    // The window starts at phase w·settle: rotate back to cosine phase.
    let phase0 = w * settle as f64;
    let mut out = [Complex64::new(0.0, 0.0); LEN];
    for (k, slot) in out.iter_mut().enumerate() {
        let kw = w * k as f64;
        let mut acc = Complex64::new(0.0, 0.0);
        for (j, &y) in tail.iter().enumerate() {
            acc += Complex64::from_polar(y, -kw * j as f64);
        }
        let rot = Complex64::from_polar(1.0, -(k as f64) * phase0);
        *slot = acc * rot * if k == 0 { 1.0 } else { 2.0 } / n as f64;
    }
    Tone {
        harmonics: out,
        mean_drive: sum_d / n as f64,
        mean_bias: sum_b / n as f64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harmonics::Harmonics;
    use audivane_protocol::{
        HARMONICS_DEFAULT_SIGNS, HarmonicSign, HarmonicsConfig, HarmonicsLayer, OrderTilt,
        TiltShelf,
    };

    const P: HarmonicSign = HarmonicSign::Positive;
    const N: HarmonicSign = HarmonicSign::Negative;

    fn layer(levels: [Option<f64>; 8], signs: [HarmonicSign; 8]) -> ChebyshevLayer {
        ChebyshevLayer {
            enabled: true,
            preset: None,
            levels_db: levels,
            signs,
            invert_even: false,
            drive_db: 0.0,
            amount: 1.0,
            tilt: None,
            sag: None,
            bias: None,
        }
    }

    fn preset_layer(id: &str) -> ChebyshevLayer {
        match &HarmonicsConfig::from_preset(id).unwrap().layers[0] {
            HarmonicsLayer::Chebyshev(l) => l.clone(),
            _ => panic!("{id} is not a Chebyshev preset"),
        }
    }

    fn off_sag() -> SagConfig {
        SagConfig {
            depth_db: 0.0,
            ..SagConfig::default()
        }
    }

    fn off_bias() -> BiasConfig {
        BiasConfig {
            depth: 0.0,
            ..BiasConfig::default()
        }
    }

    fn sag(depth_db: f64, attack_ms: f64, release_ms: f64) -> SagConfig {
        SagConfig {
            depth_db,
            attack_ms,
            release_ms,
        }
    }

    fn bias(depth: f64, attack_ms: f64, release_ms: f64) -> BiasConfig {
        BiasConfig {
            depth,
            attack_ms,
            release_ms,
        }
    }

    fn config(l: &ChebyshevLayer, s: SagConfig, b: BiasConfig) -> TubeDynamicsConfig {
        TubeDynamicsConfig::from_layer(l, true, s, b).unwrap()
    }

    /// Layers with strong terms, including high orders and mixed signs.
    fn strong_layers() -> Vec<ChebyshevLayer> {
        vec![
            layer([Some(-20.0); 8], [P, N, N, P, N, P, P, N]),
            layer([Some(-20.0); 8], [N; 8]),
            layer([Some(-20.0); 8], [P; 8]),
            layer(
                [Some(-20.0), Some(-20.0), None, None, None, None, None, None],
                HARMONICS_DEFAULT_SIGNS,
            ),
            layer(
                [None, None, None, None, Some(-20.0), None, Some(-20.0), None],
                [P, P, P, P, N, P, P, P],
            ),
            preset_layer("se_triode_high_mu"),
            preset_layer("pentode"),
        ]
    }

    /// Process `buf` in odd block sizes.
    fn run(stage: &mut dyn Stage, buf: &mut [f64]) {
        let mut i = 0;
        let mut b = 1;
        while i < buf.len() {
            let e = (i + b).min(buf.len());
            stage.process(&mut [&mut buf[i..e]]);
            i = e;
            b = b % 1021 + 31;
        }
    }

    fn peak(buf: &[f64]) -> f64 {
        buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()))
    }

    fn db(v: f64) -> f64 {
        lin_to_db(v)
    }

    fn silence(rate: f64, seconds: f64) -> impl Iterator<Item = f64> {
        std::iter::repeat_n(0.0, (seconds * rate) as usize)
    }

    /// Adversarial inputs for the bound, at `rate`, with envelope times of
    /// the config.
    fn adversarial(rate: f64, cfg: &TubeDynamicsConfig) -> Vec<Vec<f64>> {
        let t = |ms: f64| ms * 1e-3;
        let (sa, sr) = (t(cfg.sag.attack_ms), t(cfg.sag.release_ms));
        let (ba, br) = (t(cfg.bias.attack_ms), t(cfg.bias.release_ms));
        let mut out = Vec::new();
        // Bursts after silence.
        let mut v: Vec<f64> = silence(rate, 0.2).collect();
        for f in [1_000.0, 100.0, 5_000.0] {
            v.extend(sine(rate, f, 1.0, 0.05));
            v.extend(silence(rate, 0.1));
        }
        out.push(v);
        // Sustained full scale, then silence, then a full-scale spike and a
        // full-scale low tone.
        let hold = (5.0 * ba.max(sa)).min(3.0);
        let mut v: Vec<f64> = sine(rate, 1_000.0, 1.0, hold).collect();
        v.extend(silence(rate, (3.0 * sr).min(1.0)));
        v.extend([1.0, -1.0, 1.0]);
        v.extend(sine(rate, 40.0, 1.0, 0.1));
        v.extend(std::iter::repeat_n(-1.0, 200));
        out.push(v);
        // Loud and quiet in turn at the envelopes' time scales.
        for (on, off) in [(sa, sr), (ba, br), (sr, sa)] {
            let (on, off) = (on.min(0.5), off.min(0.5));
            let mut v = Vec::new();
            for _ in 0..4 {
                v.extend(sine(rate, 700.0, 1.0, on));
                v.extend(sine(rate, 700.0, 0.03, off));
            }
            out.push(v);
        }
        // Sines 20 Hz to 20 kHz at full scale.
        let mut v = Vec::new();
        for f in [20.0, 50.0, 200.0, 1_000.0, 5_000.0, 12_000.0, 20_000.0] {
            if f < 0.45 * rate {
                v.extend(sine(rate, f, 1.0, 0.15));
            }
        }
        out.push(v);
        out
    }

    fn shelves(low: (f64, f64), high: (f64, f64), low_even: bool) -> HarmonicsTilt {
        HarmonicsTilt {
            low: TiltShelf {
                hz: low.0,
                db: low.1,
                slope: None,
            },
            high: TiltShelf {
                hz: high.0,
                db: high.1,
                slope: None,
            },
            low_even,
            ..HarmonicsTilt::default()
        }
    }

    /// Tilted curves: the two tilted presets, a hot curve with strong
    /// shelves on every order, and one with mixed signs and a per-order
    /// override.
    fn tilted_layers() -> Vec<ChebyshevLayer> {
        let mut hot = layer(
            [
                Some(-30.0),
                Some(-36.0),
                Some(-50.0),
                Some(-55.0),
                None,
                None,
                None,
                None,
            ],
            HARMONICS_DEFAULT_SIGNS,
        );
        hot.tilt = Some(Box::new(shelves((100.0, 12.0), (2_000.0, 12.0), true)));
        let mut mixed = layer([Some(-26.0); 8], [P, N, N, P, N, P, P, N]);
        let mut t = shelves((60.0, 24.0), (1_000.0, 24.0), false);
        t.orders[1] = Some(OrderTilt {
            low: TiltShelf::new(2_000.0, -24.0),
            high: TiltShelf::new(200.0, 0.0),
        });
        mixed.tilt = Some(Box::new(t));
        vec![
            preset_layer("pp_pentode_feedback"),
            preset_layer("solid_state_feedback"),
            hot,
            mixed,
        ]
    }

    /// [`adversarial`] plus sign flips after settling (the envelopes up on
    /// one sign, then the other at full scale).
    fn tilted_adversarial(rate: f64, cfg: &TubeDynamicsConfig) -> Vec<Vec<f64>> {
        let mut out = adversarial(rate, cfg);
        let hold = (5.0 * cfg.bias.attack_ms.max(cfg.sag.attack_ms) * 1e-3).clamp(0.05, 1.0);
        for (from, to) in [(1.0, -1.0), (-1.0, 1.0), (1.0, 0.0), (0.8, -1.0)] {
            let mut v = vec![from; (hold * rate) as usize];
            v.extend(std::iter::repeat_n(to, (0.02 * rate) as usize));
            v.extend(std::iter::repeat_n(from, 64));
            out.push(v);
        }
        out
    }

    /// Tilted layers through the envelope range, with and without the DC
    /// blocker, at both rate extremes: no adversarial input reaches past
    /// full scale. One thread per layer and envelope pair.
    #[test]
    fn tilted_output_stays_within_full_scale_over_the_envelope_range() {
        let envelopes = [
            (sag(6.0, 0.1, 1.0), bias(0.25, 0.1, 5.0)),
            (sag(6.0, 0.1, 1.0), bias(-0.25, 0.1, 5.0)),
            (sag(3.0, 20.0, 200.0), bias(0.1, 300.0, 2_000.0)),
            (off_sag(), bias(-0.25, 30.0, 200.0)),
            (sag(3.0, 10.0, 100.0), off_bias()),
        ];
        let layers = tilted_layers();
        let closest = std::thread::scope(|scope| {
            let runs: Vec<_> = layers
                .iter()
                .flat_map(|l| envelopes.iter().map(move |&e| (l, e)))
                .map(|(l, (s, b))| {
                    scope.spawn(move || {
                        let mut closest: f64 = 0.0;
                        for dc_block in [true, false] {
                            let cfg = TubeDynamicsConfig::from_layer(l, dc_block, s, b).unwrap();
                            for rate in [44_100.0, 768_000.0] {
                                let st = TubeDynamics::try_new(&cfg, rate, 1).unwrap();
                                for (i, mut v) in
                                    tilted_adversarial(rate, &cfg).into_iter().enumerate()
                                {
                                    let mut st = st.clone();
                                    run(&mut st, &mut v);
                                    let p = peak(&v);
                                    assert!(
                                        p <= 1.0,
                                        "{:?} {s:?} {b:?} {dc_block} {rate}: signal {i} peak {p}",
                                        l.levels_db
                                    );
                                    closest = closest.max(p);
                                }
                            }
                        }
                        closest
                    })
                })
                .collect();
            runs.into_iter()
                .map(|r| r.join().unwrap())
                .fold(0.0_f64, f64::max)
        });
        eprintln!("closest to the bound: {:.2} dB under", -db(closest));
    }

    fn sine(rate: f64, freq: f64, amp: f64, seconds: f64) -> impl Iterator<Item = f64> {
        let n = (seconds * rate) as usize;
        (0..n).map(move |i| amp * (2.0 * PI * freq * i as f64 / rate).sin())
    }

    #[test]
    fn depth_zero_matches_the_static_layer_bit_for_bit() {
        let mut layers = strong_layers();
        for id in [
            "triode",
            "se_triode_low_mu",
            "se_pentode",
            "diff_pair",
            "pp_triode",
        ] {
            layers.push(preset_layer(id));
        }
        let mut driven = preset_layer("se_triode_high_mu");
        driven.drive_db = -6.0;
        driven.amount = 0.5;
        layers.push(driven);
        // Loud noise beyond full scale, a sine, steps, overs, non-finite.
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut input: Vec<f64> = (0..96_000)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let w = (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.4 - 1.2;
                if i % 3 == 0 {
                    (i as f64 * 0.031).sin()
                } else {
                    w
                }
            })
            .collect();
        input.extend([1.0, -1.0, 0.0, 1.5, -1.5, f64::NAN, f64::INFINITY, 0.3]);
        input.extend(std::iter::repeat_n(0.7, 2000));
        for l in &layers {
            for dc_block in [true, false] {
                for rate in [44_100.0, 192_000.0] {
                    let h = HarmonicsConfig {
                        system: None,
                        enabled: true,
                        layers: vec![HarmonicsLayer::Chebyshev(l.clone())],
                        dc_block,
                        routing: Default::default(),
                    };
                    let mut st = Harmonics::try_new(&h, rate, 1).unwrap();
                    let cfg = TubeDynamicsConfig::from_layer(
                        l,
                        dc_block,
                        sag(0.0, 0.1, 0.1),
                        bias(0.0, 0.1, 0.1),
                    )
                    .unwrap();
                    let mut dynamic = TubeDynamics::try_unscaled(&cfg, rate, 1).unwrap();
                    assert_eq!(
                        st.small_signal_gain_db().to_bits(),
                        dynamic.small_signal_gain_db().to_bits()
                    );
                    let mut want = input.clone();
                    run(&mut st, &mut want);
                    let mut got = input.clone();
                    run(&mut dynamic, &mut got);
                    for (i, (w, g)) in want.iter().zip(&got).enumerate() {
                        assert_eq!(w.to_bits(), g.to_bits(), "{l:?} sample {i}: {w} vs {g}");
                    }
                }
            }
        }
    }

    #[test]
    fn envelopes_follow_their_time_constants_at_any_rate() {
        let l = preset_layer("se_triode_high_mu");
        let cfg = config(&l, sag(2.0, 10.0, 100.0), bias(0.1, 200.0, 1_000.0));
        let one_minus_inv_e = 1.0 - (-1.0_f64).exp();
        for rate in [44_100.0, 768_000.0] {
            let mut st = TubeDynamics::try_new(&cfg, rate, 1).unwrap();
            let at = |ms: f64| (ms * 1e-3 * rate).round() as usize;
            // Full-scale square (|x| = 1) from silence.
            let mut n = 0;
            let mut seen = (0.0, 0.0);
            while n < at(1_500.0) {
                let mut s = [if n % 2 == 0 { 1.0 } else { -1.0 }];
                st.process(&mut [&mut s]);
                n += 1;
                if n == at(10.0) {
                    seen.0 = st.envelopes(0).0;
                }
                if n == at(200.0) {
                    seen.1 = st.envelopes(0).1;
                }
            }
            assert!(
                (seen.0 - one_minus_inv_e).abs() < 1e-3,
                "{rate}: sag attack {}",
                seen.0
            );
            assert!(
                (seen.1 - one_minus_inv_e).abs() < 1e-3,
                "{rate}: bias attack {}",
                seen.1
            );
            let (s0, b0) = st.envelopes(0);
            // Release into silence.
            let mut n = 0;
            while n < at(1_000.0) {
                let mut s = [0.0];
                st.process(&mut [&mut s]);
                n += 1;
                if n == at(100.0) {
                    seen.0 = st.envelopes(0).0 / s0;
                }
            }
            seen.1 = st.envelopes(0).1 / b0;
            let inv_e = (-1.0_f64).exp();
            assert!(
                (seen.0 - inv_e).abs() < 1e-3,
                "{rate}: sag release {}",
                seen.0
            );
            assert!(
                (seen.1 - inv_e).abs() < 1e-3,
                "{rate}: bias release {}",
                seen.1
            );
        }
    }

    /// Settle on `x0` (both envelopes at `|x0|`), pause `pause` samples of
    /// silence, then step to `x1`; returns the peak of the step and the
    /// largest remainder `|y/s − L d x|` seen.
    fn settle_step(st: &TubeDynamics, x0: f64, settle: usize, pause: usize, x1: f64) -> (f64, f64) {
        let mut st = st.clone();
        let s = st.scale();
        let l = st.core.small_signal;
        let mut rem: f64 = 0.0;
        let mut peak_out: f64 = 0.0;
        let total = settle + pause + 64;
        for i in 0..total {
            let x = if i < settle {
                x0
            } else if i < settle + pause {
                0.0
            } else {
                x1
            };
            let mut v = [x];
            st.process(&mut [&mut v]);
            let (d, _) = st.operating_point(0);
            rem = rem.max((v[0] / s - l * d * x).abs());
            if i >= settle + pause {
                peak_out = peak_out.max(v[0].abs());
            }
        }
        (peak_out, rem)
    }

    #[test]
    fn worst_settle_then_step_stays_within_the_bound_and_comes_close() {
        let rate = 48_000.0;
        let grid: Vec<f64> = (0..=16).map(|i| -1.0 + i as f64 / 8.0).collect();
        let mut tightest: f64 = 0.0;
        for l in strong_layers() {
            for b in [0.25, -0.25] {
                // Fast bias and sag release, slow sag attack: the pause brings
                // the drive back to 1 with the bias still up, and a step then
                // reaches b + u at the edge of the range.
                let cfg = config(&l, sag(6.0, 50.0, 0.5), bias(b, 0.1, 50.0));
                let st = TubeDynamics::try_new(&cfg, rate, 1).unwrap();
                let fixed = TubeDynamics::try_new(&config(&l, off_sag(), off_bias()), rate, 1)
                    .unwrap()
                    .peak_trim_db();
                let (_, _, r) = st.split();
                let mut best: f64 = 0.0;
                for &x0 in &grid {
                    for pause in [0, 240] {
                        for &x1 in &grid {
                            let (p, rem) = settle_step(&st, x0, 4_800, pause, x1);
                            assert!(
                                p <= 1.0,
                                "{l:?} b {b}: {x0} -> {x1} (pause {pause}) peak {p}"
                            );
                            assert!(rem <= r, "{l:?} b {b}: remainder {rem} > R {r}");
                            best = best.max(p);
                        }
                    }
                }
                tightest = tightest.max(best);
                eprintln!(
                    "trim {:6.2} dB (static {fixed:6.2}), reach {:+.3}, worst step peak {best:.4} ({:?}, b {b})",
                    st.peak_trim_db(),
                    st.bias_reach(),
                    &l.levels_db[..3]
                );
                assert!(st.peak_trim_db() >= fixed.min(0.0) - BIAS_BOUND_BUDGET_DB - 1e-9);
            }
        }
        // The certification is not wildly conservative.
        assert!(tightest > 0.97, "{tightest}");
    }

    #[test]
    fn overs_pass_at_unity_slope_and_non_finite_input_maps_to_zero() {
        let cfg = config(
            &preset_layer("se_triode_high_mu"),
            sag(3.0, 1.0, 10.0),
            bias(0.2, 1.0, 10.0),
        );
        let st = TubeDynamics::try_new(&cfg, 48_000.0, 1).unwrap();
        let s = st.scale();
        let mut a = st.clone();
        let mut b = st.clone();
        let mut warm: Vec<f64> = sine(48_000.0, 300.0, 0.9, 0.2).collect();
        let mut warm2 = warm.clone();
        a.process(&mut [&mut warm]);
        b.process(&mut [&mut warm2]);
        let mut x = [1.5, -1.25, f64::NAN, f64::NEG_INFINITY];
        let mut y = [1.0, -1.0, 0.0, 0.0];
        a.process(&mut [&mut x]);
        b.process(&mut [&mut y]);
        assert!((x[0] - (y[0] + 0.5 * s)).abs() < 1e-15);
        assert!((x[1] - (y[1] - 0.25 * s)).abs() < 1e-15);
        assert_eq!(x[2].to_bits(), y[2].to_bits());
        assert_eq!(x[3].to_bits(), y[3].to_bits());
    }

    #[test]
    fn silence_is_exact_zero_while_the_bias_decays_and_no_state_goes_subnormal() {
        let rate = 44_100.0;
        let l = layer(
            [
                Some(-20.0),
                Some(-20.0),
                Some(-30.0),
                None,
                None,
                None,
                None,
                None,
            ],
            HARMONICS_DEFAULT_SIGNS,
        );
        for dc_block in [false, true] {
            let cfg = TubeDynamicsConfig::from_layer(
                &l,
                dc_block,
                sag(3.0, 1.0, 10.0),
                bias(0.25, 1.0, 20.0),
            )
            .unwrap();
            let mut st = TubeDynamics::try_new(&cfg, rate, 1).unwrap();
            let mut v: Vec<f64> = sine(rate, 200.0, 1.0, 0.3).collect();
            st.process(&mut [&mut v]);
            assert!(st.bias_reach() > 0.0);
            assert!(st.operating_point(0).1 > 0.8 * st.bias_reach());
            for i in 0..(8.0 * rate) as usize {
                let mut x = [0.0];
                st.process(&mut [&mut x]);
                let s = st.state[0];
                for (name, v) in [("sag", s.sag), ("bias", s.bias), ("dc", s.dc_z1)] {
                    assert!(!v.is_subnormal(), "{name} subnormal at {i}: {v:e}");
                }
                if !dc_block {
                    // b moves (decays) while the output stays exactly 0.
                    assert_eq!(x[0], 0.0, "sample {i}");
                }
            }
            assert_eq!(st.state[0], DynamicsState::default());
            let mut x = [0.0];
            st.process(&mut [&mut x]);
            assert_eq!(x[0], 0.0);
        }
    }

    // ------------------------------------------------------------ tilt
}
