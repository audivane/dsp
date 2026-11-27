//! Harmonic distortion ("tube" / THD) waveshaper stage.
//!
//! Placed **after** the upsampler (at the output sample rate) and before
//! volume/quantizer, so the harmonics (2nd–9th) sit below Nyquist when
//! upsampling is high enough ([`aliasing`] measures what folds back). The
//! shaper is a sum of signed Chebyshev terms `a_k T_k(x)`, which makes
//! `|a_k|` the exact k-th harmonic amplitude relative to the fundamental at
//! full scale.
//!
//! The terms are evaluated one by one (the Chebyshev recurrence) as pure
//! distortion, `D_k(x) = T_k(x) − T_k'(0)·x − T_k(0)`: no offset and no
//! linear part. The linear parts go on the direct path,
//! `L = 1 + Σ_odd a_k T_k'(0)`, so a filter after one term (a later step)
//! never changes the music's frequency response. Only the even terms can
//! create DC, so only their sum goes through the 5 Hz first-order DC
//! blocker; the odd terms and the direct path carry the music unfiltered.
//!
//! The stage plays at its natural level: there is no cut for safety. `B`, a
//! certified bound of its output over `|x| <= 1`, is its worst-case peak
//! gain ([`Harmonics::peak_gain_db`]); the limiter and the output headroom
//! after it handle what goes over full scale, as for every stage. Samples
//! beyond full scale (inter-sample overs from the upsampler) are shaped at
//! `±1` and the excess passes with unity slope, so the quantizer still sees
//! and counts them.
//!
//! The small-signal gain is `L` (`T_k'(0) = k (−1)^((k−1)/2)` for odd `k`);
//! a full-scale fundamental also carries the odd terms' compression.
//!
//! # Layers
//!
//! The config is a list of layers added together (parallel sum). Chebyshev
//! layers are static polynomials, so they merge exactly into one table of
//! amplitudes, the **Chebyshev bank**: each layer's levels with its signs,
//! read at its drive ([`layer_amplitudes`]) and scaled by its amount, summed
//! order by order. Every other layer kind runs on its own through the common
//! layer interface (`harmonics_layer`): build at a rate, per-sample tick with
//! per-channel state, bound split, warnings, readout hooks.
//! Flux layers ([`crate::flux`]) add their distortion path
//! `P⁻¹(ADAA₁(D)(P x))` with its own filter state; they carry no linear part
//! and no DC. Hysteresis layers ([`crate::hysteresis`]) add their
//! distortion-only Jiles-Atherton path `w · P⁻¹ HPF₅Hz(u − z)` (`z = P x_c`
//! the core's input through the optional bass EQ, `u` its normalized
//! magnetization around the rest point of an optional field offset) with its
//! own magnetic, EQ and DC-blocker state; they likewise carry no linear part
//! and decay to zero DC in silence. A Chebyshev layer with sag or bias
//! ([`crate::tube_dynamics`]) moves its operating point per sample, so it
//! leaves the bank and adds its distortion at its own drive,
//! `δ(d x_c; b)`, with its own envelopes and DC blocker (and its tilt, if
//! any: each filtered order through its own blocker and filter); the sag/bias
//! layers share one supply, which turns the dry signal and their linear
//! parts down once, by the deepest sag (`(D Λ − 1) x_c`,
//! `harmonics_layer`, "Shared supply"). With both depths 0, or
//! amount 0 (the depths scale with it), it stays in the bank. Layers of one
//! kind are summed together and their sum is added to the bank's output
//! (flux, then hysteresis, then sag/bias layers).
//!
//! One bound covers the whole stage. A flux layer's path is
//! `−(λ/2)(x[n] + x[n−1])` plus a residual of at most `R` (its certified
//! split). A hysteresis layer's path is held by its runtime within
//! `|p| <= Y` and `|x_c + p| <= Q` (both probed through the whole layer
//! with margin and enforced for every input: [`crate::hysteresis`], "Peak
//! bound"), so for any split weight `θ ∈ [0, 1]`,
//! `p[n] = −θ x[n] + ρ_θ[n]` with `|ρ_θ| <= (1 − θ) Y + θ Q`. Filtered
//! (tilted) Chebyshev terms carry no linear part and are bounded on their
//! own (`T_t` each, see "Tilt" below). The stage output is the unfiltered
//! Chebyshev part with its direct gain lowered by `Σ λ_j/2 + Σ θ_i`,
//! plus everything else, so
//!
//! ```text
//! B = B_cheb(L − Σ_j λ_j/2 − Σ_i θ_i)
//!   + Σ_j (λ_j/2 + R_j)                       flux layers
//!   + Σ_i ((1 − θ_i) Y_i + θ_i Q_i)           hysteresis layers
//!   + Σ_t T_t                                 filtered Chebyshev terms
//! ```
//!
//! with `B_cheb(g)` the certified grid bound of the unfiltered terms with
//! direct gain `g` (DC blocker included) and `L` the direct gain of every
//! Chebyshev term, filtered or not. Choosing `θ_i ∈ [0, 1]` greedily so the
//! direct gain is not driven below zero (and taking the tighter of that
//! split and `θ_i = 0`) recovers the core's own hysteresis bound exactly for
//! a single hysteresis layer, the flux core's own bound for a single flux
//! layer, and the exact Chebyshev bound when none of flux, hysteresis and
//! tilt is present. Each part keeps its own certification; the margins are
//! 1 ppm with hysteresis (its RK4 step), else 1e-12 with flux or tilt. For
//! these kinds it is the generic composition of the layers' splits
//! (`harmonics_layer`, "Bound"): a hysteresis layer is a `Split::Between`
//! segment `{0, Y, 1, Q}`;
//! a sag/bias layer adds one too (its proof is there), and its bias reach is
//! cut where it would raise the bound more than 1 dB over its static
//! curve's ([`Harmonics::bias_limits`]).
//!
//! # Tilt
//!
//! A Chebyshev layer's tilt ([`audivane_protocol::HarmonicsTilt`]) sends
//! each order's term `a_k D_k` through its own filter `G_k`
//! (`harmonics_tilt`), unity at `k × 1 kHz`, so harmonic `k` of a sine at `f`
//! is `a_k G_k(k f)` relative to the input. The linear parts stay
//! on the direct path, so small signals keep a flat response; but `D_k` of
//! an odd order also carries fundamental-frequency content (the curve's
//! compression), which `G_k(f)` shapes: a full-scale fundamental's level
//! changes with frequency (the readouts and the level change count it).
//! Orders whose filter is unity stay in the merged table above, so a layer
//! without tilt (or with a flat one) runs as the untilted stage. Filtered
//! terms of different layers that share order and filter share one filter;
//! they stay in the bank (one recurrence, one DC blocker for every even term).
//! A tilted layer with sag or bias runs its filters in its own run instead
//! ([`crate::tube_dynamics`], "Tilt"), with the same per-term bound at rest.
//!
//! Bound with filters: the bank is bounded jointly, lag by lag
//! (`JointBound`). Every term is a memoryless `D_k(x_c)` through a linear
//! filter (its tilt filter, then the DC blocker for an even term), so the
//! output is `g x_c[n] + Σ_m F_m(x_c[n − m])` with `F_m` a polynomial whose
//! coefficients are the terms' impulse responses at lag `m`; each lag reads
//! its own input sample, so the supremum over all inputs within full scale is
//! the sum over lags of each `F_m`'s own maximum on `[−1, 1]`: the exact
//! worst case, certified on a grid with a curvature margin and a certified
//! rest. The stage also has the per-term bound (each filtered term at
//! `T = |w| (|m G(1)| + r ‖g‖₁)` on its own, `[m − r, m + r]` the range of
//! `D_k` on `[−1, 1]`, `‖g‖₁` a certified L1 norm, `2 b0 r ‖g‖₁` through the
//! blocker) and takes the smaller of the two. Strong tilts (say ±24 dB on
//! loud levels) raise the peak gain by what their true worst case needs. A
//! tilted layer with sag or bias keeps the per-term bound
//! ([`crate::tube_dynamics`]).
//!
//! # Series
//!
//! With [`HarmonicsRouting::Series`] and two or more active layers (enabled,
//! amount above 0) the layers run one after the other, in list order: each
//! layer is its own device, layer `j` alone as a parallel stage `S_j`
//! (`only_layer`), with its own calibration (as if alone at full-scale
//! input), its own certified bound `B_j`, its own sag/bias supply (nothing
//! shared across layers) and its own DC blockers. The blockers sit at each
//! layer's output, like the coupling capacitor between two real stages, so
//! one layer's DC never moves the next one's operating point. With `P_j`
//! layer `j`'s output (the parallel stage's, dry signal included):
//!
//! ```text
//! y_0 = x,    y_j = P_j(y_{j−1}),    y = y_n
//! ```
//!
//! **Gain staging.** Every layer feeds the next at the level it puts out:
//! later layers are driven at the music's level, as chained gear is, and
//! the levels compound (the readouts show the real result). Every layer
//! kind is certified for `|x| <= 1` only, and beyond it does what the
//! parallel stage does: it clamps its input to full scale and passes the
//! excess at unity slope. That holds for the Chebyshev recurrence, the flux
//! saturator, the tape core and its bass EQ, the tilt filters and the bias
//! operating point, which all see the clamped input. So where a layer's
//! output goes past full scale (worst-case inputs, or loud passages
//! through a layer that adds a lot), the next layer shapes only the part
//! within full scale and passes the rest unshaped.
//!
//! **Bound.** The parallel composition `B_bank(L − Σ g) + …` assumes every
//! part sees the same `x`; in series it is composed stage by stage. For an
//! input within `±R` (`R >= 1`) layer `j`'s output is within
//!
//! ```text
//! C_j(R) = B_j + (R − 1)
//! ```
//!
//! (`B_j` bounds the shaped part for every clamped input sequence from any
//! state the layer reaches, its margin included; the excess passes at unity
//! slope). By induction from `R_0 = 1`, `R_j = C_j(max(R_{j−1}, 1))` (times
//! `1 + 1e-12` for rounding), and `R_n` is the stage's certified peak gain
//! for every input within full scale ([`peak_reach`]). A series
//! config is refused only where one of its layers is (and, defensively,
//! where a layer's bound is not finite and positive:
//! [`HarmonicsError::Series`]).
//!
//! With one active layer the series stage is built as the parallel one
//! (bit for bit). On the audio thread each layer runs over the whole block
//! in turn, in place: no allocation and no locks; the channel split gives
//! each channel its part of every layer.
//!
//! **Readouts** are not separable in series: every tone runs through the
//! whole stage at the simulation rate (`SeriesSim`), each layer's
//! envelopes presettled on the steady state of its own input. The tones and
//! the level-change path are cached as one unit (every layer in order).
//! Cross terms reach orders above H9 (up to the product of the layers'
//! orders, at the product of their levels); the readouts show H2–H15, and
//! the aliasing ([`aliasing`]) is measured through the whole chain at the
//! output rate, every order included.

pub use crate::harmonics_alias::{ALIAS_TONE_HZ, ALIAS_WARN_DB, aliasing};
use crate::harmonics_layer::{Built, LayerGroup, build_layer, compose_bound, prebuild_slow_cores};
pub use crate::harmonics_layer::{LayerWarnings, layer_warnings, validate_layer};
pub use crate::harmonics_sim::clear_readout_caches;
use crate::harmonics_sim::{
    MAX_THREADS, READOUT_CACHE, ReadoutCache, SimUnit, ToneKey, tone_key, unit_key,
};
pub use crate::harmonics_spectrum::{SPECTRUM_POINTS, WAVE_POINTS, spectrum};
use crate::harmonics_tilt::{JointBound, JointTerm, TiltFilter, TiltState};
use crate::loudness::{STAGE_LOUDNESS_RATE, stage_loudness_change, stage_loudness_reference};
use crate::{Stage, db_to_lin, lin_to_db};
use audivane_protocol::ipc::{
    HarmonicsAlias, HarmonicsBiasLimit, HarmonicsCleanRate, HarmonicsReadout, HarmonicsReadoutPoint,
};
use audivane_protocol::{
    ChebyshevLayer, HARMONICS_MAX_DB, HARMONICS_MAX_DRIVE_DB, HARMONICS_MAX_ORDER,
    HARMONICS_MIN_DB, HARMONICS_MIN_DRIVE_DB, HARMONICS_ORDERS, HARMONICS_READOUT_MAX_ORDER,
    HarmonicsConfig, HarmonicsLayer, HarmonicsRouting,
};
use std::f64::consts::PI;
use std::fmt;

pub(crate) const DC_BLOCK_HZ: f64 = 5.0;
/// The DC blocker's state is flushed to zero below this (2^-100, about
/// -602 dB, as `crate::dc_block` and `crate::flux`), so a decaying tail
/// never enters subnormals.
const DC_FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31;
/// Grid cells for the certified extremum search.
const GRID: usize = 4096;
/// Samples per block when per-layer kinds run: the bank's output and the
/// clamped input of one block stay on the stack while each layer adds its
/// path.
const CHUNK: usize = 128;

/// Polynomial degree, and the length of per-order tables indexed by `k`.
pub(crate) const DEG: usize = HARMONICS_MAX_ORDER;
pub(crate) const LEN: usize = DEG + 1;
/// Length of a readout tone's complex amplitudes `[DC, H1, …, H15]`
/// ([`HARMONICS_READOUT_MAX_ORDER`]): series cross terms and tape layers
/// reach past the curves' degree.
pub(crate) const READOUT_LEN: usize = HARMONICS_READOUT_MAX_ORDER + 1;
/// A readout tone's complex amplitudes, `[DC, H1, …, H15]`.
pub type ToneSpectrum = [crate::Complex64; READOUT_LEN];

/// `T_k(0)`: 0 for odd `k`, `(−1)^(k/2)` for even `k`.
pub(crate) const T_AT_0: [f64; LEN] = [1.0, 0.0, -1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 1.0, 0.0];
/// `T_k'(0)`: 0 for even `k`, `k (−1)^((k−1)/2)` for odd `k`.
pub(crate) const T_SLOPE_0: [f64; LEN] = [0.0, 1.0, 0.0, -3.0, 0.0, 5.0, 0.0, -7.0, 0.0, 9.0];

/// Why a [`Harmonics`] stage could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HarmonicsError {
    ZeroChannels,
    InvalidSampleRate,
    InvalidLevel(String),
    /// A flux layer (its index in `layers`) failed to build.
    Flux(usize, crate::flux::FluxError),
    /// A hysteresis layer (its index in `layers`) failed to build.
    Hysteresis(usize, crate::hysteresis::HysteresisError),
    /// A Chebyshev layer with sag or bias (its index) failed to build.
    Dynamics(usize, String),
    /// A layer (its index) could not be certified in series.
    Series(usize, String),
}

impl fmt::Display for HarmonicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => write!(f, "harmonics requires at least 1 channel"),
            Self::InvalidSampleRate => write!(f, "harmonics requires a positive sample rate"),
            Self::InvalidLevel(msg) => write!(f, "harmonics: {msg}"),
            Self::Flux(i, e) => write!(f, "harmonics layer {i}: {e}"),
            Self::Hysteresis(i, e) => write!(f, "harmonics layer {i}: {e}"),
            Self::Dynamics(i, e) => write!(f, "harmonics layer {i}: {e}"),
            Self::Series(i, e) => write!(f, "harmonics layer {i}: {e}"),
        }
    }
}

impl std::error::Error for HarmonicsError {}

/// Check a Chebyshev layer's parameters (levels, drive, amount, tilt);
/// messages name the parameter (`levels_db[5] (H7) must be …`).
pub(crate) fn check_chebyshev(layer: &ChebyshevLayer) -> Result<(), String> {
    for (i, level) in layer.levels_db.iter().enumerate() {
        if let Some(db) = *level
            && (!db.is_finite() || !(HARMONICS_MIN_DB..=HARMONICS_MAX_DB).contains(&db))
        {
            return Err(format!(
                "levels_db[{i}] (H{}) must be between {HARMONICS_MIN_DB} and {HARMONICS_MAX_DB} dB (got {db})",
                i + 2
            ));
        }
    }
    let d = layer.drive_db;
    if !d.is_finite() || !(HARMONICS_MIN_DRIVE_DB..=HARMONICS_MAX_DRIVE_DB).contains(&d) {
        return Err(format!(
            "drive_db must be between {HARMONICS_MIN_DRIVE_DB} and {HARMONICS_MAX_DRIVE_DB} dB (got {d})"
        ));
    }
    if !layer.amount.is_finite() || !(0.0..=1.0).contains(&layer.amount) {
        return Err(format!(
            "amount must be between 0 and 1 (got {})",
            layer.amount
        ));
    }
    if let Some(t) = &layer.tilt {
        t.validate()?;
    }
    if let Some(sag) = &layer.sag {
        sag.validate()?;
    }
    if let Some(bias) = &layer.bias {
        bias.validate()?;
    }
    Ok(())
}

/// Signed Chebyshev amplitudes `[a2, …, a9]` of one layer as the stage
/// applies it: the levels with their signs (and `invert_even`), read at the
/// drive, times the amount. Drive `d < 1` moves a full-scale sine to level
/// `d` on the curve and renormalises its fundamental to 1, so the result is
/// the curve's harmonics at level `d` relative to its fundamental there
/// (exact: [`harmonic_amplitudes`]). Validates the layer.
pub fn layer_amplitudes(layer: &ChebyshevLayer) -> Result<[f64; HARMONICS_ORDERS], HarmonicsError> {
    check_chebyshev(layer).map_err(HarmonicsError::InvalidLevel)?;
    let signs = layer.effective_signs();
    let mut a = [0.0; HARMONICS_ORDERS];
    for (i, (level, sign)) in layer.levels_db.iter().zip(signs).enumerate() {
        if let Some(db) = *level {
            a[i] = sign.factor() * db_to_lin(db);
        }
    }
    let drive = layer.drive_db;
    if drive < 0.0 {
        let d = db_to_lin(drive);
        let h = harmonic_amplitudes(a, d);
        // Opposite-sign high odd orders can cancel the fundamental at some
        // drive; dividing by it would give huge or non-finite terms.
        if h[1].is_nan() || h[1].abs() < MIN_DRIVEN_FUNDAMENTAL * d {
            return Err(HarmonicsError::InvalidLevel(format!(
                "drive_db {drive}: these levels cancel the fundamental at this drive"
            )));
        }
        for (i, ak) in a.iter_mut().enumerate() {
            *ak = h[i + 2] / h[1];
        }
    }
    Ok(a.map(|ak| ak * layer.amount))
}

/// Signed Chebyshev amplitudes `[a2, …, a9]` of the stage's bank: the sum
/// of its enabled static Chebyshev layers ([`layer_amplitudes`]; layers
/// with sag or bias run on their own); 0 = no term.
pub fn amplitudes(config: &HarmonicsConfig) -> Result<[f64; HARMONICS_ORDERS], HarmonicsError> {
    let mut sum = [0.0; HARMONICS_ORDERS];
    for layer in config.active_layers() {
        if let HarmonicsLayer::Chebyshev(l) = layer
            && !l.is_dynamic()
        {
            for (s, a) in sum.iter_mut().zip(layer_amplitudes(l)?) {
                *s += a;
            }
        }
    }
    Ok(sum)
}

/// One filtered Chebyshev term: order `k` through `filter` (its weight
/// folded in).
#[derive(Clone, Copy, Debug)]
struct TiltTerm {
    k: usize,
    filter: TiltFilter,
}

/// The stage's Chebyshev terms at `rate`: the amplitudes `[a2, …, a9]` of
/// the orders on the unfiltered path (summed as [`amplitudes`] sums them),
/// and the filtered orders of tilted layers as `(k, filter, weight)`, one per
/// order and filter (layers with the same filter for an order share it).
type SplitTerms = ([f64; HARMONICS_ORDERS], Vec<(usize, TiltFilter, f64)>);

fn split_terms(config: &HarmonicsConfig, rate: f64) -> Result<SplitTerms, HarmonicsError> {
    let mut flat = [0.0; HARMONICS_ORDERS];
    let mut filtered: Vec<(usize, TiltFilter, f64)> = Vec::new();
    for layer in config.active_layers() {
        if let HarmonicsLayer::Chebyshev(l) = layer
            && !l.is_dynamic()
        {
            for (i, a) in layer_amplitudes(l)?.into_iter().enumerate() {
                let k = i + 2;
                let filter = l
                    .tilt
                    .as_ref()
                    .and_then(|t| TiltFilter::new(&t.order_shelves(k), k, rate));
                match filter {
                    None => flat[i] += a,
                    Some(_) if a == 0.0 => {}
                    Some(f) => match filtered.iter_mut().find(|t| t.0 == k && t.1 == f) {
                        Some(t) => t.2 += a,
                        None => filtered.push((k, f, a)),
                    },
                }
            }
        }
    }
    Ok((flat, filtered))
}

/// Certified bound of a stage's output for every input within `±r`, from
/// its layers' bounds in order (one in parallel; [`Harmonics::peak_bounds`];
/// module docs, "Series", "Bound"): each shapes the part within full scale
/// within its bound and passes the excess at unity slope. `r` for none.
pub fn peak_reach(bounds: &[f64], r: f64) -> f64 {
    // Within full scale the bound is the layer's own (its margin in it);
    // past it, the excess path rounds (one sum per sample).
    bounds.iter().fold(r, |r, &b| {
        if r <= 1.0 {
            b
        } else {
            (b + r - 1.0) * (1.0 + 1e-12)
        }
    })
}

/// The certified peak gain of a series chain for inputs within full scale:
/// `R_n`, each layer's reach composed in list order.
fn series_reach(series: &[Harmonics]) -> f64 {
    let bounds: Vec<f64> = series.iter().map(|l| l.bound).collect();
    peak_reach(&bounds, 1.0)
}

/// The bank's terms for its joint bound ([`JointBound`]): the unfiltered
/// orders of `a`, then the filtered ones, even orders through the DC
/// blocker when it is on.
fn joint_terms(a: &Poly, filtered: &[(usize, TiltFilter, f64)], dc_block: bool) -> Vec<JointTerm> {
    let even = |k: usize| dc_block && k.is_multiple_of(2);
    (2..LEN)
        .filter(|&k| a[k] != 0.0)
        .map(|k| JointTerm {
            k,
            weight: a[k],
            filter: None,
            dc_block: even(k),
        })
        .chain(filtered.iter().map(|&(k, f, w)| JointTerm {
            k,
            weight: w,
            filter: Some(f),
            dc_block: even(k),
        }))
        .collect()
}

/// Bound of one filtered term `g * D_k` (unit weight) over `|x| <= 1`: see
/// the module docs ("Tilt").
pub(crate) fn tilt_term_bound(k: usize, filter: &TiltFilter, dc_block: bool, hpf_b0: f64) -> f64 {
    let l1 = filter.l1_norm();
    if k.is_multiple_of(2) {
        // D_k = T_k − T_k(0) spans [0, 2] or [−2, 0]: middle −T_k(0), radius 1.
        if dc_block {
            2.0 * hpf_b0 * l1
        } else {
            (T_AT_0[k] * filter.dc_gain()).abs() + l1
        }
    } else {
        // D_k is odd: middle 0, radius max |D_k|.
        let t = chebyshev_power_forms();
        let mut d = t[k];
        d[1] = 0.0;
        let (lo, hi) = certified_range(&d);
        hi.max(-lo) * l1
    }
}

/// `(layer index, clean rate)` of each enabled, non-zero layer of `config`
/// that aliases significantly into the audio band at `output_rate`, below
/// the rate it needs (tape hysteresis below 4×, 176.4 kHz;
/// [`LayerWarnings::clean_rate_hz`]). Empty when the stage is off.
pub fn below_clean_rate(config: &HarmonicsConfig, output_rate: f64) -> Vec<(usize, f64)> {
    if !config.enabled {
        return Vec::new();
    }
    config
        .layers
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            layer_warnings(l, output_rate)
                .clean_rate_hz
                .map(|hz| (i, hz))
        })
        .collect()
}

/// `config` with only layer `index` enabled, in parallel: that layer's stage
/// in series (module docs, "Series"). Layer indices are kept, so errors and
/// warnings name the config's layer.
fn only_layer(config: &HarmonicsConfig, index: usize) -> HarmonicsConfig {
    let mut c = config.clone();
    c.routing = HarmonicsRouting::Parallel;
    for (j, layer) in c.layers.iter_mut().enumerate() {
        if j != index {
            match layer {
                HarmonicsLayer::Chebyshev(l) => l.enabled = false,
                HarmonicsLayer::Flux(l) => l.enabled = false,
                HarmonicsLayer::Hysteresis(l) => l.enabled = false,
            }
        }
    }
    c
}

/// Polynomial in power form, `c[m] x^m`, m = 0..=9.
pub(crate) type Poly = [f64; LEN];

/// Power-form coefficients of `T_0..=T_9`.
pub(crate) fn chebyshev_power_forms() -> [Poly; LEN] {
    chebyshev_power_forms_to()
}

/// Power-form coefficients of `T_0..T_N`.
fn chebyshev_power_forms_to<const N: usize>() -> [[f64; N]; N] {
    let mut t = [[0.0; N]; N];
    t[0][0] = 1.0;
    t[1][1] = 1.0;
    for k in 2..N {
        // T_k = 2x T_{k−1} − T_{k−2}.
        for m in 0..N {
            let up = if m > 0 { 2.0 * t[k - 1][m - 1] } else { 0.0 };
            t[k][m] = up - t[k - 2][m];
        }
    }
    t
}

/// Exact harmonic amplitudes `[DC, H1, …, H9]` (signed, cosine phase) of
/// the static shaper `x + Σ a_k (T_k(x) − T_k(0))` (before scaling and DC
/// removal) for an input `A cos θ`, at any level `A`. `a` is `[a2, …, a9]`.
pub fn harmonic_amplitudes(a: [f64; HARMONICS_ORDERS], level: f64) -> [f64; LEN] {
    let t = chebyshev_power_forms();
    let mut p: Poly = [0.0; LEN];
    p[1] = 1.0;
    for (i, &ak) in a.iter().enumerate() {
        let k = i + 2;
        for m in 1..LEN {
            p[m] += ak * t[k][m];
        }
    }
    poly_harmonics(&p, level)
}

/// Harmonic amplitudes `[DC, H1, …, H9]` (cosine phase) of `p(A cos θ)` for
/// a power-form polynomial `p`.
fn poly_harmonics(p: &Poly, level: f64) -> [f64; LEN] {
    // cosⁿθ = 2⁻ⁿ Σ_j C(n, j) cos((n − 2j)θ).
    let mut out = [0.0; LEN];
    let mut an = 1.0;
    for (n, &pn) in p.iter().enumerate() {
        if n > 0 {
            an *= level;
        }
        if pn == 0.0 {
            continue;
        }
        let w = pn * an / f64::powi(2.0, n as i32);
        let mut binom = 1.0;
        for j in 0..=n {
            let m = n.abs_diff(2 * j);
            // Each cos(mθ), m > 0, appears twice (j and n − j); DC once.
            if 2 * j <= n {
                out[m] += if m == 0 { w * binom } else { 2.0 * w * binom };
            }
            binom = binom * (n - j) as f64 / (j + 1) as f64;
        }
    }
    out
}

pub(crate) fn eval(c: &Poly, x: f64) -> f64 {
    c.iter().rev().fold(0.0, |acc, &cm| acc * x + cm)
}

/// Certified `(lower bound of min, upper bound of max)` of `c` on `[-1, 1]`:
/// on each grid cell `f` lies within `h²/8 · max|f''|` of its chord.
pub(crate) fn certified_range(c: &Poly) -> (f64, f64) {
    let h = 2.0 / GRID as f64;
    let m2: f64 = (2..LEN).map(|m| c[m].abs() * (m * (m - 1)) as f64).sum();
    let margin = h * h / 8.0 * m2;
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for i in 0..=GRID {
        let y = eval(c, -1.0 + h * i as f64);
        lo = lo.min(y);
        hi = hi.max(y);
    }
    (lo - margin, hi + margin)
}

fn add(a: &Poly, b: &Poly, kb: f64) -> Poly {
    std::array::from_fn(|i| a[i] + kb * b[i])
}

/// The Chebyshev bank: every static Chebyshev layer merged into one table
/// of terms (module docs, "Layers" and "Tilt"), the even terms through one
/// DC blocker, plus the direct path and the overs.
#[derive(Clone, Debug)]
struct ChebyshevBank {
    amps: [f64; HARMONICS_ORDERS],
    /// Direct-path gain `L`.
    linear: f64,
    /// Term weights `a_k`, indexed by order `k` (0 for `k < 2` and off).
    coef: [f64; LEN],
    /// Highest order with a term (1: none).
    top: usize,
    dc_block: bool,
    hpf_b0: f64,
    hpf_a1: f64,
    /// One per channel.
    dc_state: Vec<f64>,
    /// The filtered Chebyshev terms of tilted layers (empty: none, and the
    /// bank runs the untilted loop).
    tilt: Vec<TiltTerm>,
    /// Highest order of a filtered term (1: none).
    tilt_top: usize,
    /// Their state, `channels × tilt.len()`, channel-major.
    tilt_state: Vec<TiltState>,
}

impl ChebyshevBank {
    /// The bank's output for channel `c`, in place.
    #[inline(always)]
    fn run(&mut self, c: usize, chan: &mut [f64]) {
        if self.tilt.is_empty() {
            self.run_flat(c, chan);
        } else {
            self.run_tilted(c, chan);
        }
    }

    /// Without filtered terms.
    #[inline(always)]
    fn run_flat(&mut self, c: usize, chan: &mut [f64]) {
        let Some(&z) = self.dc_state.get(c) else {
            return;
        };
        let mut z1 = z;
        for s in chan.iter_mut() {
            *s = self.flat_sample(*s, &mut z1);
        }
        if let Some(z) = self.dc_state.get_mut(c) {
            *z = z1;
        }
    }

    /// One sample of the untilted bank.
    #[inline(always)]
    fn flat_sample(&self, raw: f64, z1: &mut f64) -> f64 {
        let (linear, coef, top) = (self.linear, &self.coef, self.top);
        let x = if raw.is_finite() { raw } else { 0.0 };
        let xc = x.clamp(-1.0, 1.0);
        let over = x - xc;
        if top >= 2 {
            // Term by term: T_k by the recurrence, D_k = T_k − T_k'(0) xc − T_k(0);
            // acc[0] sums the even terms, acc[1] the odd ones.
            let two_x = 2.0 * xc;
            let (mut t_prev, mut t) = (1.0, xc);
            let mut acc = [0.0; 2];
            for k in 2..=top {
                let t_next = two_x * t - t_prev;
                t_prev = t;
                t = t_next;
                let d = t - T_SLOPE_0[k] * xc - T_AT_0[k];
                acc[k & 1] += coef[k] * d;
            }
            let [e, o] = acc;
            let e = if self.dc_block {
                self.dc_step(e, z1)
            } else {
                e
            };
            linear * xc + o + e + over
        } else {
            linear * xc + over
        }
    }

    /// One step of the even terms' DC blocker (TDF-II: y = b0 e + z1;
    /// z1 = −b0 e − a1 y).
    #[inline(always)]
    fn dc_step(&self, e: f64, z1: &mut f64) -> f64 {
        let (b0, a1) = (self.hpf_b0, self.hpf_a1);
        let y = b0 * e + *z1;
        *z1 = -b0 * e - a1 * y;
        // A decaying tail after silence never enters subnormals.
        if z1.abs() < DC_FLUSH_BELOW {
            *z1 = 0.0;
        }
        y
    }

    /// With filtered terms: as [`Self::run_flat`], plus each filtered `D_k`
    /// through its filter (even ones before the DC blocker).
    #[inline(always)]
    fn run_tilted(&mut self, c: usize, chan: &mut [f64]) {
        let Some(&z) = self.dc_state.get(c) else {
            return;
        };
        let mut z1 = z;
        let ntilt = self.tilt.len();
        let Some(tst) = self.tilt_state.get_mut(c * ntilt..(c + 1) * ntilt) else {
            return;
        };
        let (linear, coef, top) = (self.linear, &self.coef, self.top);
        let top_all = top.max(self.tilt_top);
        let (b0, a1) = (self.hpf_b0, self.hpf_a1);
        let dc_block = self.dc_block;
        let tilt = &self.tilt[..];
        for s in chan.iter_mut() {
            let x = if s.is_finite() { *s } else { 0.0 };
            let xc = x.clamp(-1.0, 1.0);
            let over = x - xc;
            let two_x = 2.0 * xc;
            let (mut t_prev, mut t) = (1.0, xc);
            let mut d = [0.0; LEN];
            for (k, dk) in d.iter_mut().enumerate().take(top_all + 1).skip(2) {
                let t_next = two_x * t - t_prev;
                t_prev = t;
                t = t_next;
                *dk = t - T_SLOPE_0[k] * xc - T_AT_0[k];
            }
            let mut acc = [0.0; 2];
            for k in 2..=top {
                acc[k & 1] += coef[k] * d[k];
            }
            for (term, st) in tilt.iter().zip(tst.iter_mut()) {
                acc[term.k & 1] += term.filter.tick(st, d[term.k]);
            }
            let [e, o] = acc;
            let e = if dc_block {
                let y = b0 * e + z1;
                z1 = -b0 * e - a1 * y;
                if z1.abs() < DC_FLUSH_BELOW {
                    z1 = 0.0;
                }
                y
            } else {
                e
            };
            *s = linear * xc + o + e + over;
        }
        if let Some(z) = self.dc_state.get_mut(c) {
            *z = z1;
        }
    }

    /// End of a block on channel `c`: the filter states flush their
    /// decaying tails (once per block, [`TiltState::flush`]).
    #[inline]
    fn end_block(&mut self, c: usize) {
        let ntilt = self.tilt.len();
        if let Some(tst) = self.tilt_state.get_mut(c * ntilt..(c + 1) * ntilt) {
            for st in tst {
                st.flush();
            }
        }
    }

    fn reset(&mut self) {
        self.dc_state.fill(0.0);
        self.tilt_state.fill(TiltState::default());
    }

    /// One single-channel bank per channel, from its state; `self` is left
    /// without channel states.
    fn split_channels(&mut self) -> Vec<ChebyshevBank> {
        let states = std::mem::take(&mut self.dc_state);
        let tilt_states = std::mem::take(&mut self.tilt_state);
        let ntilt = self.tilt.len();
        states
            .into_iter()
            .enumerate()
            .map(|(c, z)| ChebyshevBank {
                dc_state: vec![z],
                tilt_state: tilt_states[c * ntilt..(c + 1) * ntilt].to_vec(),
                ..self.clone()
            })
            .collect()
    }
}

/// The live state of one sag/bias layer ([`Harmonics::live_layers`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LiveLayer {
    /// Its index in the config's layers.
    pub layer: usize,
    /// How far its sag has turned the drive down now, dB (0 at rest, up to
    /// the sag depth).
    pub sag_db: f64,
    /// Its operating point now, fraction of full scale (0 at rest).
    pub bias: f64,
}

/// The layered harmonics stage: the Chebyshev bank plus each per-layer
/// kind's path (`harmonics_layer`).
#[derive(Clone, Debug)]
pub struct Harmonics {
    enabled: bool,
    channels: usize,
    bank: ChebyshevBank,
    /// The per-layer kinds, one group per kind in rank order (config order
    /// within one).
    groups: Vec<LayerGroup>,
    /// Whether a group runs channel pairs in lockstep.
    paired: bool,
    /// Series routing (module docs, "Series"): the active layers as their
    /// own stages, in list order (empty: parallel, the bank and groups
    /// above run). The bank and groups of a series stage are empty.
    series: Vec<Harmonics>,
    /// The certified bound of the stage's output for `|x| <= 1` (in
    /// series, the chain's: [`series_reach`]).
    bound: f64,
}

impl Harmonics {
    /// Build the stage at `output_rate` for `channels` (off the audio
    /// thread: flux and hysteresis layers are calibrated here).
    pub fn try_new(
        config: &HarmonicsConfig,
        output_rate: f64,
        channels: usize,
    ) -> Result<Self, HarmonicsError> {
        if channels == 0 {
            return Err(HarmonicsError::ZeroChannels);
        }
        if !output_rate.is_finite() || output_rate <= 0.0 {
            return Err(HarmonicsError::InvalidSampleRate);
        }
        if config.enabled && config.routing.is_series() {
            let series = Self::series_layers(config, output_rate, channels)?;
            if series.len() >= 2 {
                return Self::try_series(config, output_rate, channels, series);
            }
            // One active layer (or none): exactly the parallel stage.
        }
        Self::try_parallel(config, output_rate, channels)
    }

    /// The parallel stage (module docs, "Layers").
    fn try_parallel(
        config: &HarmonicsConfig,
        output_rate: f64,
        channels: usize,
    ) -> Result<Self, HarmonicsError> {
        let amps = amplitudes(config)?;
        let (flat, filtered) = split_terms(config, output_rate)?;
        let mut layers = Vec::new();
        for (i, layer) in config.layers.iter().enumerate() {
            match build_layer(layer, i, output_rate, channels, config.dc_block)? {
                Built::Layer(l) => layers.push(l),
                Built::Bank | Built::Silent => {}
            }
        }
        // Stable: config order within a kind.
        layers.sort_by_key(LayerGroup::rank);
        let mut groups: Vec<LayerGroup> = Vec::new();
        for l in layers {
            let rest = match groups.last_mut() {
                Some(g) => g.merge(l),
                None => Some(l),
            };
            groups.extend(rest);
        }

        // The unfiltered terms (all of them without tilt).
        let mut a = [0.0; LEN];
        a[2..].copy_from_slice(&flat);
        let top = (2..LEN).rev().find(|&k| a[k] != 0.0).unwrap_or(1);
        // Every term's linear part, filtered or not, is on the direct path.
        let mut total = [0.0; LEN];
        total[2..].copy_from_slice(&amps);
        let linear = 1.0
            + (3..LEN)
                .step_by(2)
                .map(|k| total[k] * T_SLOPE_0[k])
                .sum::<f64>();

        // 1st-order HPF at DC_BLOCK_HZ via bilinear transform:
        // H(z) = b0 (1 - z^-1) / (1 + a1 z^-1).
        let fc = DC_BLOCK_HZ.min(0.1 * output_rate);
        let k = (PI * fc / output_rate).tan();
        let hpf_b0 = 1.0 / (1.0 + k);
        let hpf_a1 = (k - 1.0) / (1.0 + k);

        // The filtered terms are bounded on their own, whatever the split of
        // the direct gain, or jointly with the unfiltered ones (the tighter
        // of the two, module docs "Tilt"); every per-layer kind adds its
        // split.
        let own: f64 = filtered
            .iter()
            .map(|(k, f, w)| w.abs() * tilt_term_bound(*k, f, config.dc_block, hpf_b0))
            .sum();
        let splits = groups.iter().flat_map(LayerGroup::splits);
        let raw_bound = if filtered.is_empty() {
            compose_bound(
                linear,
                |g| chebyshev_bound(&a, g, config.dc_block, hpf_b0),
                own,
                splits,
            )
        } else {
            let terms = joint_terms(&a, &filtered, config.dc_block);
            let joint = JointBound::new(&terms, (hpf_b0, hpf_a1));
            compose_bound(
                linear,
                |g| (chebyshev_bound(&a, g, config.dc_block, hpf_b0) + own).min(joint.bound(g)),
                0.0,
                splits,
            )
        };
        // Rounding in the tilt filters (as the flux stage); the per-layer
        // kinds' own margins.
        let bank_margin = if filtered.is_empty() { 0.0 } else { 1e-12 };
        let margin = groups
            .iter()
            .map(LayerGroup::margin)
            .fold(bank_margin, f64::max);
        let bound = raw_bound * (1.0 + margin);
        let tilt: Vec<TiltTerm> = filtered
            .into_iter()
            .map(|(k, f, w)| TiltTerm {
                k,
                filter: f.scaled(w),
            })
            .collect();
        let tilt_top = tilt.iter().map(|t| t.k).max().unwrap_or(1);
        let tilt_state = vec![TiltState::default(); channels * tilt.len()];

        Ok(Self {
            enabled: config.enabled,
            channels,
            bank: ChebyshevBank {
                amps,
                linear,
                coef: a,
                top,
                dc_block: config.dc_block,
                hpf_b0,
                hpf_a1,
                dc_state: vec![0.0; channels],
                tilt,
                tilt_top,
                tilt_state,
            },
            paired: groups.iter().any(LayerGroup::paired),
            groups,
            series: Vec::new(),
            bound,
        })
    }

    /// Each active layer of `config` (enabled, amount above 0) built alone
    /// as a parallel stage, in list order: its own calibration, bound,
    /// supply and DC blockers. Every enabled layer is built
    /// (validated, calibrated) even at amount 0, as in parallel.
    fn series_layers(
        config: &HarmonicsConfig,
        output_rate: f64,
        channels: usize,
    ) -> Result<Vec<Harmonics>, HarmonicsError> {
        let mut out = Vec::new();
        for (i, layer) in config.layers.iter().enumerate() {
            if !layer.enabled() {
                continue;
            }
            let stage = Self::try_parallel(&only_layer(config, i), output_rate, channels)?;
            // Defensive: every layer's bound is certified finite; one that
            // is not would make the chain's bound meaningless.
            if !(stage.bound.is_finite() && stage.bound > 0.0) {
                return Err(HarmonicsError::Series(
                    i,
                    format!(
                        "its peak bound could not be certified for series (bound {})",
                        stage.bound
                    ),
                ));
            }
            if layer.amount() > 0.0 {
                out.push(stage);
            }
        }
        Ok(out)
    }

    /// The series stage from its layers' stages (module docs, "Series").
    fn try_series(
        config: &HarmonicsConfig,
        output_rate: f64,
        channels: usize,
        series: Vec<Harmonics>,
    ) -> Result<Self, HarmonicsError> {
        // No layer of its own: the identity bank, never run.
        let empty = HarmonicsConfig {
            layers: Vec::new(),
            routing: HarmonicsRouting::Parallel,
            ..config.clone()
        };
        let mut stage = Self::try_parallel(&empty, output_rate, channels)?;
        stage.bound = series_reach(&series);
        stage.series = series;
        Ok(stage)
    }

    /// Whether the layers run in series (two or more active layers with
    /// series routing).
    pub fn is_series(&self) -> bool {
        !self.series.is_empty()
    }

    /// One single-channel stage per channel, each running its channel
    /// exactly as this one does (bit for bit), from this one's state. Off
    /// the real-time thread; `self` is left without channels.
    pub fn split_channels(&mut self) -> Vec<Harmonics> {
        let mut series: Vec<Vec<Harmonics>> = Vec::new();
        let n_layers = self.series.len();
        for layer in &mut self.series {
            for (c, part) in layer.split_channels().into_iter().enumerate() {
                if series.len() <= c {
                    series.push(Vec::with_capacity(n_layers));
                }
                series[c].push(part);
            }
        }
        let banks = self.bank.split_channels();
        let mut groups: Vec<Vec<LayerGroup>> =
            vec![Vec::with_capacity(self.groups.len()); banks.len()];
        for group in &mut self.groups {
            for (part, g) in groups.iter_mut().zip(group.split_channels()) {
                part.push(g);
            }
        }
        self.channels = 0;
        let mut series = series.into_iter();
        banks
            .into_iter()
            .zip(groups)
            .map(|(bank, groups)| Harmonics {
                enabled: self.enabled,
                channels: 1,
                bank,
                groups,
                paired: self.paired,
                series: series.next().unwrap_or_default(),
                bound: self.bound,
            })
            .collect()
    }

    /// Signed Chebyshev amplitudes `[a2, …, a9]` of the merged layers (0 =
    /// no term).
    /// In series, the sum over the layers (each layer's own table).
    pub fn amplitudes(&self) -> [f64; HARMONICS_ORDERS] {
        let mut a = self.bank.amps;
        for layer in &self.series {
            for (s, l) in a.iter_mut().zip(layer.amplitudes()) {
                *s += l;
            }
        }
        a
    }

    /// `f` of this stage, or summed over its layers in series.
    fn count(&self, f: impl Fn(&Harmonics) -> usize + Copy) -> usize {
        f(self) + self.series.iter().map(|l| l.count(f)).sum::<usize>()
    }

    /// Number of flux layers applied.
    pub fn flux_layers(&self) -> usize {
        self.count(Self::own_flux_layers)
    }

    fn own_flux_layers(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| matches!(g, LayerGroup::Flux(_)))
            .map(LayerGroup::len)
            .sum()
    }

    /// Number of filtered (tilted) Chebyshev terms applied.
    pub fn tilted_terms(&self) -> usize {
        self.count(|s| s.bank.tilt.len())
    }

    /// Number of active hysteresis layers applied.
    pub fn hysteresis_layers(&self) -> usize {
        self.count(Self::own_hysteresis_layers)
    }

    fn own_hysteresis_layers(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| matches!(g, LayerGroup::Hysteresis(_)))
            .map(LayerGroup::len)
            .sum()
    }

    /// Number of Chebyshev layers with sag or bias applied (each runs on
    /// its own, outside the merged table).
    pub fn dynamic_layers(&self) -> usize {
        self.count(Self::own_dynamic_layers)
    }

    fn own_dynamic_layers(&self) -> usize {
        self.groups
            .iter()
            .filter(|g| matches!(g, LayerGroup::Dynamic(_)))
            .map(LayerGroup::len)
            .sum()
    }

    /// `(layer index, depth, reach)` of each sag/bias layer whose bias reach
    /// the bound cut short of its depth (the peak budget, see
    /// [`crate::tube_dynamics`]), in config order.
    pub fn bias_limits(&self) -> Vec<(usize, f64, f64)> {
        let mut out: Vec<_> = self
            .groups
            .iter()
            .flat_map(LayerGroup::bias_limits)
            .chain(self.series.iter().flat_map(Harmonics::bias_limits))
            .collect();
        out.sort_by_key(|l| l.0);
        out
    }

    /// Put every layer's slow state (sag and bias envelopes) where `input`,
    /// looped forever on every channel, would leave it at the loop's start.
    /// In series each layer is presettled on its input: `input` through the
    /// layers before it (presettled, from rest otherwise). Off the audio
    /// thread (allocates).
    pub(crate) fn presettle(&mut self, input: &[f64]) {
        if self.is_series() {
            let mut x = input.to_vec();
            let last = self.series.len() - 1;
            for (j, layer) in self.series.iter_mut().enumerate() {
                layer.presettle(&x);
                if j < last {
                    let mut probe = layer.clone();
                    probe.process(&mut [x.as_mut_slice()]);
                }
            }
            return;
        }
        let mut xc = vec![0.0; input.len()];
        clamp_input(input, &mut xc);
        for c in 0..self.channels {
            for group in &mut self.groups {
                group.presettle(c, &xc);
            }
        }
    }

    /// Certified worst-case peak gain in dB for inputs within full scale:
    /// how far over 0 dBFS the stage can take them (0 when off). Nothing
    /// cuts it; the limiter and the output headroom after the stage handle
    /// what goes over.
    pub fn peak_gain_db(&self) -> f64 {
        if self.enabled {
            lin_to_db(peak_reach(&self.peak_bounds(), 1.0))
        } else {
            0.0
        }
    }

    /// Each layer's certified bound for inputs within full scale, in the
    /// order they run (one in parallel, empty when off): what
    /// [`peak_reach`] composes.
    pub fn peak_bounds(&self) -> Vec<f64> {
        if !self.enabled {
            Vec::new()
        } else if self.is_series() {
            self.series.iter().map(|l| l.bound).collect()
        } else {
            vec![self.bound]
        }
    }

    /// Loudness change of the stage on music, dB: the BS.1770 (K-weighted)
    /// loudness of a reference signal after the stage minus before. The
    /// reference is 4 s of deterministic pink noise at -14 dBFS RMS (a loud
    /// master, peaks near full scale), so it counts the odd terms'
    /// compression on loud passages, the flux layers' and the
    /// added harmonics' energy. Simulated at 48 kHz. 0 when off. Off the
    /// audio thread: it allocates.
    pub fn loudness_change_db(config: &HarmonicsConfig) -> Result<f64, HarmonicsError> {
        if !config.is_active() {
            return Ok(0.0);
        }
        Self::loudness_change_db_from(config, &READOUT_CACHE, MAX_THREADS)
    }

    /// [`Self::loudness_change_db`] with the slow units' paths from
    /// `cache`.
    fn loudness_change_db_from(
        config: &HarmonicsConfig,
        cache: &ReadoutCache,
        threads: usize,
    ) -> Result<f64, HarmonicsError> {
        if !config.is_active() {
            return Ok(0.0);
        }
        let stage = Self::try_new(config, STAGE_LOUDNESS_RATE, 1)?;
        if stage.is_series() {
            let sim = SeriesSim::new(&stage);
            return Ok(sim.loudness_change_db(config, cache, threads));
        }
        Ok(settled_loudness_change_db(&stage, config, cache, threads))
    }

    /// The linear part of the stage for the live view
    /// ([`crate::chain::HarmonicsTap`]): its small-signal gain, linear
    /// (sag/bias layers at rest; 1 when the stage is off). Every order's
    /// slope at 0 is in it; the DC blockers and tilt
    /// filters are not (they sit on the distortion terms only, so the
    /// music's own path is this one gain with no delay).
    pub fn linear_gain(&self) -> f64 {
        if self.enabled {
            self.small_signal_gain()
        } else {
            1.0
        }
    }

    /// The sag and bias state of every sag/bias layer now, merged over the
    /// channels (the deepest sag, the bias point farthest from 0), into
    /// `out` by config index; returns how many entries it filled (at most
    /// `out.len()`; in series a layer's index is its place in the list).
    /// Allocation-free: a read of the envelopes, for the audio thread.
    pub fn live_layers(&self, out: &mut [LiveLayer]) -> usize {
        let mut n = 0;
        self.live_layers_into(out, &mut n);
        n
    }

    /// The state of the even terms' DC blocker on channel `c` now (the
    /// TDF-II state the next sample adds), for the live view's split
    /// ([`live::LiveCurve::eval_with`]): `None` in series (one per layer),
    /// with the blocker off, or without the channel. A read, for the audio
    /// thread.
    #[inline]
    pub fn live_dc(&self, c: usize) -> Option<f64> {
        if !self.enabled || self.is_series() || !self.bank.dc_block {
            return None;
        }
        self.bank.dc_state.get(c).copied()
    }

    /// [`Self::live_layers`], appending after the first `n` entries
    /// (merging into an entry with the same index).
    pub fn live_layers_into(&self, out: &mut [LiveLayer], n: &mut usize) {
        if !self.enabled {
            return;
        }
        for layer in &self.series {
            layer.live_layers_into(out, n);
        }
        for group in &self.groups {
            for c in 0..self.channels {
                group.live(c, |index, d, b| {
                    let sag_db = if d > 0.0 { -20.0 * d.log10() } else { 0.0 };
                    let filled = &mut out[..*n];
                    if let Some(e) = filled.iter_mut().find(|e| e.layer == index) {
                        e.sag_db = e.sag_db.max(sag_db);
                        if b.abs() > e.bias.abs() {
                            e.bias = b;
                        }
                    } else if let Some(e) = out.get_mut(*n) {
                        *e = LiveLayer {
                            layer: index,
                            sag_db,
                            bias: b,
                        };
                        *n += 1;
                    }
                });
            }
        }
    }

    /// Gain for small signals in dB: `L = 1 + Σ_odd a_k T_k'(0)`
    /// over every Chebyshev layer (sag/bias layers with their envelopes at
    /// rest; flux and hysteresis layers pass small signals at unity).
    /// In series, the product of the layers' gains.
    pub fn small_signal_gain_db(&self) -> f64 {
        if self.enabled {
            let linear = self.small_signal_gain();
            // Floored as the readouts are (stacked layers can cancel the
            // direct path exactly; -inf would not survive JSON).
            let db = lin_to_db(linear);
            if db.is_finite() {
                db.max(READOUT_FLOOR_DB)
            } else {
                READOUT_FLOOR_DB
            }
        } else {
            0.0
        }
    }

    /// [`Self::small_signal_gain_db`], linear (not floored).
    fn small_signal_gain(&self) -> f64 {
        if self.is_series() {
            return self
                .series
                .iter()
                .map(Harmonics::small_signal_gain)
                .product();
        }
        let rest: f64 = self.groups.iter().map(LayerGroup::rest_gain).sum();
        if rest == 0.0 {
            self.bank.linear
        } else {
            self.bank.linear + rest
        }
    }

    /// Cost of simulating the stage in a readout at `rate`, in tape layers
    /// (0: every layer has a closed form). In series every layer is
    /// simulated: its own cost plus [`SERIES_LAYER_COST`].
    fn readout_cost(&self, rate: f64) -> f64 {
        if self.is_series() {
            return self
                .series
                .iter()
                .map(|l| l.readout_cost(rate) + SERIES_LAYER_COST)
                .sum();
        }
        self.groups.iter().map(|g| g.readout_cost(rate)).sum()
    }

    /// Whether the stage runs (enabled with a layer on).
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether [`Self::steady_tone`] is exact at every order: a parallel
    /// stage of Chebyshev layers only (no per-layer kinds), whose curve has
    /// degree 9 at most.
    pub(crate) fn alias_closed_form(&self) -> bool {
        !self.is_series() && self.groups.is_empty()
    }

    /// Whether a layer has no closed-form tone (in series: always).
    fn simulated(&self) -> bool {
        self.is_series() || self.groups.iter().any(LayerGroup::simulated)
    }

    /// The longest settle a layer needs in a simulated readout tone (in
    /// series at least [`READOUT_SETTLE_SECONDS`], for the DC blockers
    /// between the layers).
    pub(crate) fn readout_settle_s(&self) -> f64 {
        if self.is_series() {
            return self
                .series
                .iter()
                .map(Harmonics::readout_settle_s)
                .fold(READOUT_SETTLE_SECONDS, f64::max);
        }
        self.groups
            .iter()
            .map(LayerGroup::readout_settle_s)
            .fold(0.0, f64::max)
    }

    /// Its slow parts a readout simulates on their own (none: every layer
    /// has a closed form). The stage is a parallel sum, so it is separable
    /// ([`crate::harmonics_sim`]); a series stage is not, and is simulated
    /// whole instead ([`SeriesSim`]).
    pub(crate) fn sim_units(&self) -> Vec<SimUnit> {
        self.groups.iter().flat_map(LayerGroup::sim_units).collect()
    }

    /// The stage without the layers [`Self::sim_units`] simulates: the bank
    /// and the flux layers.
    pub(crate) fn without_simulated(&self) -> Harmonics {
        let mut rest = self.clone();
        rest.groups.retain(|g| !g.simulated());
        rest.paired = rest.groups.iter().any(LayerGroup::paired);
        rest
    }
}

/// The stage loudness change ([`crate::loudness::stage_loudness_change`])
/// with the sag/bias envelopes presettled on the reference looped
/// ([`Harmonics::presettle`]): the level of music that has been playing a
/// while, however slow the envelopes (the reference's first half second is
/// skipped for the faster states).
///
/// Separable ([`crate::harmonics_sim`]): the reference through the stage
/// without its slow units, plus each unit's path on it, from `cache`
/// or simulated (the whole stage's output up to the order of the additions).
fn settled_loudness_change_db(
    stage: &Harmonics,
    config: &HarmonicsConfig,
    cache: &ReadoutCache,
    threads: usize,
) -> f64 {
    let input = stage_loudness_reference();
    let units = stage.sim_units();
    let mut output = input.clone();
    let mut rest = stage.without_simulated();
    rest.process(&mut [output.as_mut_slice()]);
    if !units.is_empty() {
        let mut xc = vec![0.0; input.len()];
        clamp_input(&input, &mut xc);
        let keys: Vec<String> = units.iter().map(|u| unit_key(config, &u.indices)).collect();
        let paths = cache.paths(&keys, STAGE_LOUDNESS_RATE, threads, |u| {
            units[u].group.path(&xc, xc.len())
        });
        for path in paths {
            for (y, &p) in output.iter_mut().zip(path.iter()) {
                *y += p;
            }
        }
    }
    stage_loudness_change(&input, &output)
}

/// Certified bound of the Chebyshev part with direct gain `linear` (the
/// terms `a`, indexed by order) over `|x| <= 1`, DC blocker included.
pub(crate) fn chebyshev_bound(a: &Poly, linear: f64, dc_block: bool, hpf_b0: f64) -> f64 {
    // Power forms: odd part o(x) = linear·x + Σ_odd a_k D_k(x) and even
    // part e(x) = Σ_even a_k D_k(x).
    let t = chebyshev_power_forms();
    let mut odd: Poly = [0.0; LEN];
    let mut even: Poly = [0.0; LEN];
    odd[1] = linear;
    for k in 2..LEN {
        let part = if k % 2 == 0 { &mut even } else { &mut odd };
        // D_k: T_k without its constant (even k) or linear (odd k) term.
        for m in 2..LEN {
            part[m] += a[k] * t[k][m];
        }
    }
    // With the blocker, y = o(x) + b0 e(x) − b0 w, where w is a positive
    // weighted average of past e (the blocker's tail sums to −b0).
    if dc_block {
        let (e_lo, e_hi) = certified_range(&even);
        let (s_lo, s_hi) = certified_range(&add(&odd, &even, hpf_b0));
        (s_hi - hpf_b0 * e_lo).max(hpf_b0 * e_hi - s_lo)
    } else {
        let (lo, hi) = certified_range(&add(&odd, &even, 1.0));
        hi.max(-lo)
    }
}

// ---------------------------------------------------------------- readouts

/// Input levels of the readout points, dBFS: full scale and typical music.
pub const READOUT_LEVELS_DBFS: [f64; 2] = [0.0, -18.0];
/// Test tones of the readout points, Hz: mid band, and low bass (where flux
/// layers distort).
pub const READOUT_FREQS_HZ: [f64; 2] = [1_000.0, 40.0];
/// Floor of the readout levels, dB (an absent harmonic).
pub(crate) const READOUT_FLOOR_DB: f64 = -200.0;

/// The optional level sweep: sines from `SWEEP_STEPS.start() × SWEEP_STEP_DB`
/// (-40 dBFS) to 0 dBFS.
const SWEEP_STEPS: std::ops::RangeInclusive<i32> = -20..=0;
const SWEEP_STEP_DB: f64 = 2.0;
/// Smallest fundamental a driven Chebyshev layer may keep, relative to its
/// input level; below it the levels cancel the fundamental.
const MIN_DRIVEN_FUNDAMENTAL: f64 = 1e-3;
/// Highest rate the readout tones are simulated at.
pub(crate) const READOUT_MAX_SIM_RATE: f64 = 96_000.0;
/// Settling time and measured length of a simulated readout tone with one
/// tape layer, seconds ([`ReadoutPlan`]; a layer may ask for more,
/// [`crate::harmonics_layer::LayerKind::readout_settle_s`]).
pub(crate) const READOUT_SETTLE_SECONDS: f64 = 0.5;
const READOUT_SECONDS: f64 = 0.25;
/// Frequencies of the readout against frequency (`freq_sweep`), Hz: the
/// third-octave series from 20 Hz to 20 kHz.
pub const READOUT_SWEEP_FREQS_HZ: [f64; 31] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1_000.0, 1_250.0, 1_600.0, 2_000.0, 2_500.0, 3_150.0, 4_000.0, 5_000.0,
    6_300.0, 8_000.0, 10_000.0, 12_500.0, 16_000.0, 20_000.0,
];

/// How a readout samples its tones: every `level_step`-th level of the
/// sweep, every `freq_step`-th frequency of the sweep against frequency,
/// and (simulated tones) the settling and measured lengths.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ReadoutPlan {
    level_step: usize,
    freq_step: usize,
    settle_s: f64,
    measure_s: f64,
}

impl ReadoutPlan {
    /// Closed form: every point (the tones cost microseconds).
    const CLOSED_FORM: Self = Self {
        level_step: 1,
        freq_step: 1,
        settle_s: 0.0,
        measure_s: 0.0,
    };
    /// Simulated, cheapest last: every level and every third frequency
    /// (octave steps) for one tape layer, then fewer points and shorter
    /// tones as the cost grows.
    const SIMULATED: [Self; 4] = [
        Self {
            level_step: 1,
            freq_step: 3,
            settle_s: READOUT_SETTLE_SECONDS,
            measure_s: READOUT_SECONDS,
        },
        Self {
            level_step: 2,
            freq_step: 3,
            settle_s: READOUT_SETTLE_SECONDS,
            measure_s: READOUT_SECONDS,
        },
        // The settle stays at the full 0.5 s in every plan: a shorter one
        // leaves the low tones unsettled and puts false even harmonics into
        // the 20-40 Hz points (10-35 dB too high with three tape layers).
        // Cost is cut with fewer points and a shorter measurement instead.
        Self {
            level_step: 2,
            freq_step: 6,
            settle_s: READOUT_SETTLE_SECONDS,
            measure_s: 0.25,
        },
        Self {
            level_step: 4,
            freq_step: 6,
            settle_s: READOUT_SETTLE_SECONDS,
            measure_s: 0.125,
        },
    ];
    /// A series stage's plans: its level sweep keeps every 2 dB step (series
    /// curves bend with level; the cross terms rise steeply) and the
    /// measurement shortens first, then the frequencies thin out, at about
    /// the cost of the parallel plans (the same budget picks a plan as cheap
    /// as the parallel one it replaces, or within 20 %).
    const SERIES: [Self; 4] = [
        Self::SIMULATED[0],
        Self {
            level_step: 1,
            freq_step: 3,
            settle_s: READOUT_SETTLE_SECONDS,
            measure_s: 0.125,
        },
        Self {
            level_step: 1,
            freq_step: 6,
            settle_s: READOUT_SETTLE_SECONDS,
            measure_s: 0.125,
        },
        Self::SIMULATED[3],
    ];
    /// Most a simulated readout with sweeps may cost, relative to one tape
    /// layer at the first simulated plan.
    const BUDGET: f64 = 1.5;

    /// Seconds of tone a readout with sweeps simulates.
    fn seconds(&self) -> f64 {
        let levels = SWEEP_STEPS.clone().step_by(self.level_step).count();
        let freqs = READOUT_SWEEP_FREQS_HZ
            .iter()
            .step_by(self.freq_step)
            .count();
        let tones = READOUT_LEVELS_DBFS.len() * READOUT_FREQS_HZ.len()
            + READOUT_FREQS_HZ.len() * levels
            + READOUT_LEVELS_DBFS.len() * freqs;
        tones as f64 * (self.settle_s + self.measure_s)
    }

    /// Seconds of tone the under-load level sweep simulates.
    fn load_seconds(&self) -> f64 {
        let levels = SWEEP_STEPS.clone().step_by(self.level_step).count();
        (READOUT_FREQS_HZ.len() * levels) as f64 * (self.settle_s + self.measure_s)
    }

    /// The plan for a readout whose static tones cost `cost` tape layers to
    /// simulate (0: closed form) and whose under-load sweep costs `load`
    /// (0: none): the first that fits the budget, else the cheapest
    /// ([`Self::SERIES`] for a series stage).
    fn for_costs(cost: f64, load: f64, series: bool) -> Self {
        if cost <= 0.0 && load <= 0.0 {
            return Self::CLOSED_FORM;
        }
        let plans = if series {
            Self::SERIES
        } else {
            Self::SIMULATED
        };
        let budget = Self::BUDGET * Self::SIMULATED[0].seconds();
        plans
            .into_iter()
            .find(|p| p.seconds() * cost + p.load_seconds() * load <= budget)
            .unwrap_or(plans[plans.len() - 1])
    }
}

/// Where a readout's sweeps run (each `None`: the fixed tones).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReadoutSweeps {
    /// The level sweep's (and the under-load sweep's) one tone, Hz, in
    /// [`MIN_SWEEP_HZ`]..=[`MAX_SWEEP_HZ`] (default: each of
    /// [`READOUT_FREQS_HZ`]).
    pub level_sweep_hz: Option<f64>,
    /// The sweep against frequency's one level, dBFS, in
    /// [`MIN_SWEEP_DBFS`]..=0 (default: each of [`READOUT_LEVELS_DBFS`]).
    pub freq_sweep_dbfs: Option<f64>,
}

/// Range of [`ReadoutSweeps::level_sweep_hz`], Hz.
pub const MIN_SWEEP_HZ: f64 = 10.0;
pub const MAX_SWEEP_HZ: f64 = 20_000.0;
/// Lowest [`ReadoutSweeps::freq_sweep_dbfs`], dBFS.
pub const MIN_SWEEP_DBFS: f64 = -120.0;

impl ReadoutSweeps {
    fn validate(&self) -> Result<(), HarmonicsError> {
        if let Some(f) = self.level_sweep_hz
            && !(MIN_SWEEP_HZ..=MAX_SWEEP_HZ).contains(&f)
        {
            return Err(HarmonicsError::InvalidLevel(format!(
                "sweep_freq_hz must be in {MIN_SWEEP_HZ}..={MAX_SWEEP_HZ} (got {f})"
            )));
        }
        if let Some(l) = self.freq_sweep_dbfs
            && !(MIN_SWEEP_DBFS..=0.0).contains(&l)
        {
            return Err(HarmonicsError::InvalidLevel(format!(
                "sweep_level_dbfs must be in {MIN_SWEEP_DBFS}..=0 (got {l})"
            )));
        }
        Ok(())
    }

    fn level_freqs(&self) -> Vec<f64> {
        self.level_sweep_hz
            .map_or_else(|| READOUT_FREQS_HZ.to_vec(), |f| vec![f])
    }

    fn freq_levels(&self) -> Vec<f64> {
        self.freq_sweep_dbfs
            .map_or_else(|| READOUT_LEVELS_DBFS.to_vec(), |l| vec![l])
    }
}

/// `config` with every layer's sag and bias off: the curves at rest.
fn at_rest(config: &HarmonicsConfig) -> HarmonicsConfig {
    let mut c = config.clone();
    for layer in &mut c.layers {
        if let HarmonicsLayer::Chebyshev(l) = layer {
            l.sag = None;
            l.bias = None;
        }
    }
    c
}

/// The stage's readouts at `output_rate`: level change on music (as A/B
/// matching counts it), peak gain, small-signal gain, aliased orders,
/// layers below their clean rate and bias reaches cut by the bound, and
/// harmonics, THD and GedLee `Gm` for sines at [`READOUT_LEVELS_DBFS`] ×
/// [`READOUT_FREQS_HZ`]; with `sweep`, also at every 2 dB from -40 to
/// 0 dBFS at each of [`READOUT_FREQS_HZ`], and at [`READOUT_LEVELS_DBFS`] ×
/// [`READOUT_SWEEP_FREQS_HZ`].
///
/// Those tones are the curves at rest (sag and bias off). When every layer
/// has a closed form they are the stage's steady state in closed form at the
/// real rate ([`Harmonics::steady_tone`], checked against simulating them
/// through the stage); otherwise (tape or sag/bias layers) the slow layers'
/// paths are simulated alone (steady state, coherent DFT) at the output rate
/// or 96 kHz, whichever is lower, and added to the closed form of the rest
/// (`harmonics_sim`: the whole stage's tone within 0.001 dB). The
/// sweeps thin out with the layers' simulation cost (every third frequency
/// with one tape layer; every 4 dB, shorter tones and fewer points with
/// several; a series stage keeps every 2 dB and shortens its tones first).
/// Either way flux layers, tilt and the DC blocker count; for Chebyshev
/// layers alone the levels match [`harmonic_amplitudes`].
///
/// With `sweep` and a sag/bias layer on, `under_load` is the level sweep
/// through the real stage, each tone after its envelopes settle
/// (presettled to their steady state, so envelopes up to 10 s cost nothing
/// extra). The slow layers' tones, level-change paths and built cores are
/// cached process-wide, so editing a static layer next to a tape layer costs
/// closed-form time. Off the audio thread (milliseconds when cached or in
/// closed form; a few hundred milliseconds for a slow layer the cache has
/// not seen, on up to four threads; allocates).
pub fn readout(
    config: &HarmonicsConfig,
    output_rate: f64,
    sweep: bool,
) -> Result<HarmonicsReadout, HarmonicsError> {
    readout_with(config, output_rate, sweep, &READOUT_CACHE)
}

/// [`readout`] with its sweeps at `sweeps` (the level sweep at one tone,
/// the sweep against frequency at one level, as given; the readout points
/// stay at the fixed tones). The thinning plan is the default sweeps', so
/// one tone costs at most as much as the default readout. Off the audio
/// thread, cached like [`readout`].
pub fn readout_sweeps(
    config: &HarmonicsConfig,
    output_rate: f64,
    sweeps: ReadoutSweeps,
) -> Result<HarmonicsReadout, HarmonicsError> {
    sweeps.validate()?;
    readout_cached(config, output_rate, Some(sweeps), &READOUT_CACHE)
}

/// [`readout`] with the simulated tones and paths from `cache`: the slow
/// layers' cores built first on the worker threads (for the three rates the
/// readout builds the stage at), then the level change on its own thread
/// while the tones run on the workers.
fn readout_with(
    config: &HarmonicsConfig,
    output_rate: f64,
    sweep: bool,
    cache: &ReadoutCache,
) -> Result<HarmonicsReadout, HarmonicsError> {
    readout_cached(
        config,
        output_rate,
        sweep.then(ReadoutSweeps::default),
        cache,
    )
}

/// [`readout_with`] with the sweeps at `sweeps` (`None`: no sweeps).
fn readout_cached(
    config: &HarmonicsConfig,
    output_rate: f64,
    sweep: Option<ReadoutSweeps>,
    cache: &ReadoutCache,
) -> Result<HarmonicsReadout, HarmonicsError> {
    prebuild_slow_cores(
        config,
        &[
            output_rate,
            output_rate.min(READOUT_MAX_SIM_RATE),
            STAGE_LOUDNESS_RATE,
        ],
    );
    std::thread::scope(|s| {
        let level = s.spawn(|| {
            let stage = Harmonics::try_new(config, output_rate, 1)?;
            // The aliasing first: its own thread's work at the output rate
            // (closed form or cached in most edits), then the level change.
            let aliasing = crate::harmonics_alias::measure_cached(config, &stage, output_rate);
            Harmonics::loudness_change_db_from(config, cache, 1).map(|l| (l, aliasing))
        });
        let mut r = readout_tones(config, output_rate, sweep, cache)?;
        (r.level_change_db, r.aliasing) = level
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
        Ok(r)
    })
}

/// [`readout_with`] without the level change (0).
fn readout_tones(
    config: &HarmonicsConfig,
    output_rate: f64,
    sweeps: Option<ReadoutSweeps>,
    cache: &ReadoutCache,
) -> Result<HarmonicsReadout, HarmonicsError> {
    let sweep = sweeps.is_some();
    let sweeps = sweeps.unwrap_or_default();
    let (level_freqs, freq_levels) = (sweeps.level_freqs(), sweeps.freq_levels());
    let stage = Harmonics::try_new(config, output_rate, 1)?;
    let dynamic = stage.dynamic_layers() > 0;
    // The static tones: the curves at rest.
    let rest_config = dynamic.then(|| at_rest(config));
    let rest_config = rest_config.as_ref().unwrap_or(config);
    let rest = if dynamic {
        Harmonics::try_new(rest_config, output_rate, 1)?
    } else {
        stage.clone()
    };
    // Chebyshev (tilted or not) and flux layers have a closed-form steady
    // state ([`Harmonics::steady_tone`]): computed at the real rate in
    // microseconds. Hysteresis and sag/bias have none, so their paths are
    // simulated alone ([`crate::harmonics_sim`]), at most at
    // READOUT_MAX_SIM_RATE: the levels of H2-H9 of 40 Hz and 1 kHz don't
    // depend on the rate above it, and a 768 kHz sweep would take over a
    // second per call (the app asks after every edit).
    let sim_rate = output_rate.min(READOUT_MAX_SIM_RATE);
    let at_sim = |cfg: &HarmonicsConfig, built: &Harmonics| -> Result<Harmonics, HarmonicsError> {
        if sim_rate >= output_rate {
            Ok(built.clone())
        } else {
            Harmonics::try_new(cfg, sim_rate, 1)
        }
    };
    let rest_sim = if rest.simulated() {
        Some(at_sim(rest_config, &rest)?)
    } else {
        None
    };
    let load_sim = if dynamic && sweep {
        Some(at_sim(config, &stage)?)
    } else {
        None
    };
    let plan = ReadoutPlan::for_costs(
        rest_sim.as_ref().map_or(0.0, |s| s.readout_cost(sim_rate)),
        load_sim.as_ref().map_or(0.0, |s| s.readout_cost(sim_rate)),
        stage.is_series(),
    );
    // Every point when the static tones are closed form.
    let static_plan = if rest_sim.is_some() {
        plan
    } else {
        ReadoutPlan::CLOSED_FORM
    };
    let tones = Tones {
        output_rate,
        sim_rate,
        plan,
        cache,
    };
    fn source<'a>(
        built: &'a Harmonics,
        sim: Option<&Harmonics>,
        cfg: &HarmonicsConfig,
    ) -> ToneSource<'a> {
        match sim.filter(|s| s.is_series()) {
            Some(sim) => ToneSource::series(sim, cfg),
            None => ToneSource::new(built, sim, cfg),
        }
    }
    let rest_source = source(&rest, rest_sim.as_ref(), rest_config);
    let mut static_tones: Vec<(f64, f64)> = Vec::new();
    for &level_db in &READOUT_LEVELS_DBFS {
        for &freq in &READOUT_FREQS_HZ {
            static_tones.push((freq, level_db));
        }
    }
    let n_points = static_tones.len();
    if sweep {
        for &freq in &level_freqs {
            for step in SWEEP_STEPS.clone().step_by(static_plan.level_step) {
                static_tones.push((freq, f64::from(step) * SWEEP_STEP_DB));
            }
        }
    }
    let n_sweep = static_tones.len() - n_points;
    if sweep {
        for &level_db in &freq_levels {
            for &freq in READOUT_SWEEP_FREQS_HZ.iter().step_by(static_plan.freq_step) {
                static_tones.push((freq, level_db));
            }
        }
    }
    let mut points = tones.points(&rest_source, &static_tones);
    let freq_points = points.split_off(n_points + n_sweep);
    let sweep_points = points.split_off(n_points);
    let load_points = match &load_sim {
        Some(sim) => {
            let mut load_tones = Vec::new();
            for &freq in &level_freqs {
                for step in SWEEP_STEPS.clone().step_by(plan.level_step) {
                    load_tones.push((freq, f64::from(step) * SWEEP_STEP_DB));
                }
            }
            tones.points(&source(&stage, Some(sim), config), &load_tones)
        }
        None => Vec::new(),
    };
    Ok(HarmonicsReadout {
        rate_hz: output_rate,
        level_change_db: 0.0,
        peak_gain_db: stage.peak_gain_db(),
        small_signal_gain_db: stage.small_signal_gain_db(),
        aliasing: crate::harmonics_alias::none(output_rate),
        below_clean_rate: below_clean_rate(config, output_rate)
            .into_iter()
            .map(|(layer, clean_rate_hz)| HarmonicsCleanRate {
                layer: layer as u32,
                clean_rate_hz,
            })
            .collect(),
        bias_limited: stage
            .bias_limits()
            .into_iter()
            .map(|(layer, depth, reach)| HarmonicsBiasLimit {
                layer: layer as u32,
                depth,
                reach,
            })
            .collect(),
        points,
        sweep: sweep_points,
        freq_sweep: freq_points,
        under_load: load_points,
    })
}

/// Where harmonic `k` of a tone at `freq` lands at `rate`, Hz: `k × freq`
/// folded into `0..=rate / 2`.
fn folded_hz(k: usize, freq: f64, rate: f64) -> f64 {
    let f = (k as f64 * freq).rem_euclid(rate);
    f.min(rate - f)
}

/// A readout point from the complex amplitudes `h` (`[DC, H1, …, H15]`) of
/// the response to a cosine at `freq` of peak `amp`, at the output rate
/// `rate`. Orders above Nyquist land elsewhere (not on a harmonic of the
/// tone): they are listed folded to where they land, and left out of the
/// harmonics, THD and `Gm`.
fn spectrum_point(
    level_db: f64,
    freq: f64,
    amp: f64,
    h: &ToneSpectrum,
    rate: f64,
) -> HarmonicsReadoutPoint {
    let h1 = h[1].norm();
    let floor = |v: f64| {
        let db = lin_to_db(v / h1);
        if db.is_finite() {
            db.max(READOUT_FLOOR_DB)
        } else {
            READOUT_FLOOR_DB
        }
    };
    let below_nyquist: [bool; READOUT_LEN] = std::array::from_fn(|k| k as f64 * freq <= 0.5 * rate);
    let power: f64 = (2..READOUT_LEN)
        .filter(|&k| below_nyquist[k])
        .map(|k| h[k].norm_sqr())
        .sum();
    // Each harmonic's level, with the sign of its cosine-phase part
    // relative to the fundamental (exact for a static curve; flux
    // layers shift the phase a little).
    let u = h[1] / h1;
    let mut signed = [0.0; READOUT_LEN];
    signed[1] = 1.0;
    for k in (2..READOUT_LEN).filter(|&k| below_nyquist[k]) {
        let re = (h[k] * u.conj().powi(k as i32)).re;
        signed[k] = h[k].norm() / h1 * if re < 0.0 { -1.0 } else { 1.0 };
    }
    HarmonicsReadoutPoint {
        input_dbfs: level_db,
        freq_hz: freq,
        thd_db: floor(power.sqrt()),
        gedlee_gm: gedlee_gm(&signed),
        harmonics_db: std::array::from_fn(|i| {
            if below_nyquist[i + 2] {
                floor(h[i + 2].norm())
            } else {
                READOUT_FLOOR_DB
            }
        }),
        aliased: (2..READOUT_LEN)
            .filter(|&k| !below_nyquist[k])
            .map(|k| HarmonicsAlias {
                order: k as u32,
                freq_hz: folded_hz(k, freq, rate),
                level_db: floor(h[k].norm()),
            })
            .filter(|a| a.level_db > READOUT_FLOOR_DB)
            .collect(),
        gain_db: {
            let db = lin_to_db(h1 / amp);
            if db.is_finite() {
                db.max(READOUT_FLOOR_DB)
            } else {
                READOUT_FLOOR_DB
            }
        },
    }
}

impl Harmonics {
    /// Steady-state complex amplitudes `[DC, H1, …, H15]` (cosine phase, as
    /// `coherent_dft` measures them) of the response to `amp · cos(2π f
    /// n / rate)`, in closed form: each Chebyshev term's harmonics through
    /// its filter and (even orders) the DC blocker at their own frequency,
    /// the direct path, and each per-layer kind's closed form (layers
    /// without one, hysteresis, are left out). `rate` is the rate the stage
    /// was built at. Harmonics past Nyquist are those of the alias. A series
    /// stage has no closed form (readouts simulate it, `SeriesSim`): this
    /// is for parallel stages. Off the audio thread.
    pub fn steady_tone(&self, rate: f64, freq: f64, amp: f64) -> ToneSpectrum {
        use crate::Complex64;
        let mut y = [Complex64::new(0.0, 0.0); READOUT_LEN];
        if !self.enabled {
            y[1] = Complex64::new(amp, 0.0);
            return y;
        }
        let bank = &self.bank;
        let w = 2.0 * PI * freq / rate;
        let hpf = |m: usize| -> Complex64 {
            let u = Complex64::from_polar(1.0, -w * m as f64);
            bank.hpf_b0 * (1.0 - u) / (1.0 + bank.hpf_a1 * u)
        };
        // The cosine-phase harmonics of D_k(A cos θ), per order.
        let t = chebyshev_power_forms();
        let terms: [[f64; LEN]; LEN] = std::array::from_fn(|k| {
            if k < 2 {
                return [0.0; LEN];
            }
            let mut d = t[k];
            d[0] = 0.0;
            d[1] = 0.0;
            poly_harmonics(&d, amp)
        });
        y[1] += bank.linear * amp;
        let mut add = |k: usize, gain: &dyn Fn(usize) -> Complex64| {
            for (m, ym) in y.iter_mut().enumerate().take(LEN) {
                let c = terms[k][m];
                if c != 0.0 {
                    let blocker = if bank.dc_block && k.is_multiple_of(2) {
                        hpf(m)
                    } else {
                        Complex64::new(1.0, 0.0)
                    };
                    *ym += c * gain(m) * blocker;
                }
            }
        };
        for (k, &c) in bank.coef.iter().enumerate().take(bank.top + 1).skip(2) {
            if c != 0.0 {
                add(k, &|_| Complex64::new(c, 0.0));
            }
        }
        for term in &bank.tilt {
            add(term.k, &|m| term.filter.response(w * m as f64));
        }
        for group in &self.groups {
            group.add_steady_tone(&mut y, rate, freq, amp);
        }
        y
    }
}

/// Where a readout's tones come from.
enum ToneSource<'a> {
    /// Separable ([`crate::harmonics_sim`]): the closed form of the stage as
    /// built at the output rate plus `s ×` each slow unit's path, simulated
    /// alone at the simulation rate.
    Parallel {
        built: &'a Harmonics,
        units: Vec<SimUnit>,
        keys: Vec<String>,
    },
    /// A series stage, simulated whole ([`SeriesSim`]), cached as one unit.
    Series { sim: SeriesSim, key: String },
}

impl<'a> ToneSource<'a> {
    /// `built` at the output rate; `sim` the same config at the simulation
    /// rate when a layer has no closed form.
    fn new(built: &'a Harmonics, sim: Option<&Harmonics>, config: &HarmonicsConfig) -> Self {
        let units = sim.map(Harmonics::sim_units).unwrap_or_default();
        let keys = units.iter().map(|u| unit_key(config, &u.indices)).collect();
        Self::Parallel { built, units, keys }
    }

    /// The series stage `sim` (at the simulation rate).
    fn series(sim: &Harmonics, config: &HarmonicsConfig) -> Self {
        let sim = SeriesSim::new(sim);
        let key = sim.key(config);
        Self::Series { sim, key }
    }
}

/// Readout cost of each layer of a series stage on top of its own, in tape
/// layers: in series even a closed-form layer is simulated (its bank or flux
/// filters, its clamp and DC blocker, per sample). Measured: three static
/// layers in series take about 0.4× the readout time of a static curve,
/// flux and tape in series.
const SERIES_LAYER_COST: f64 = 0.2;

/// How a [`SeriesSim`] presettles each layer's sag and bias envelopes.
#[derive(Clone, Copy, Debug)]
enum Settle {
    /// On the layer's whole input, looped (the level change's reference).
    Loop,
    /// On one period of the layer's input after `settle` samples, a tone of
    /// period `n` ([`tone_signal`]), rotated to the input's start.
    Tone { settle: usize, n: usize },
}

/// A series stage simulated whole in a readout (module docs, "Series"): its
/// layers as built at the simulation rate.
#[derive(Clone, Debug)]
struct SeriesSim {
    layers: Vec<Harmonics>,
}

impl SeriesSim {
    /// `stage` (a series stage) from rest.
    fn new(stage: &Harmonics) -> Self {
        let layers = stage
            .series
            .iter()
            .map(|l| {
                let mut l = l.clone();
                l.reset();
                l
            })
            .collect();
        Self { layers }
    }

    /// Its cache key: every layer's parameters in order and the stage's
    /// `dc_block`.
    fn key(&self, config: &HarmonicsConfig) -> String {
        format!("series {}", unit_key(config, &series_indices(config)))
    }

    /// Its output for `input` (one channel) from rest, run in one block per
    /// layer as the stage runs one `process` call, each layer's envelopes
    /// presettled on its own input as `settle` says.
    fn run(&self, input: &[f64], settle: Settle) -> Vec<f64> {
        let mut x = input.to_vec();
        for layer in &self.layers {
            let mut layer = layer.clone();
            if layer.dynamic_layers() > 0 {
                match settle {
                    Settle::Loop => layer.presettle(&x),
                    Settle::Tone { settle, n } => {
                        let r = settle % n;
                        let period: Vec<f64> =
                            (0..n).map(|t| x[settle + (n - r + t) % n]).collect();
                        layer.presettle(&period);
                    }
                }
            }
            layer.process(&mut [x.as_mut_slice()]);
        }
        x
    }

    /// The steady-state tone ([`tone_signal`], settled and measured for the
    /// plan's lengths, or longer if a layer asks) of the whole stage.
    fn tone(&self, rate: f64, freq: f64, amp: f64, plan: ReadoutPlan) -> ToneSpectrum {
        let settle_s = self
            .layers
            .iter()
            .map(Harmonics::readout_settle_s)
            .fold(plan.settle_s.max(READOUT_SETTLE_SECONDS), f64::max);
        let (buf, settle, n, w) = tone_signal(rate, freq, amp, settle_s, plan.measure_s);
        let out = self.run(&buf, Settle::Tone { settle, n });
        coherent_dft(&out[settle..], n, w)
    }

    /// The level change on music ([`Harmonics::loudness_change_db`]): the
    /// reference through the whole stage, each layer presettled on its
    /// input looped; the output from `cache` or simulated.
    fn loudness_change_db(
        &self,
        config: &HarmonicsConfig,
        cache: &ReadoutCache,
        threads: usize,
    ) -> f64 {
        let input = stage_loudness_reference();
        let paths = cache.paths(&[self.key(config)], STAGE_LOUDNESS_RATE, threads, |_| {
            self.run(&input, Settle::Loop)
        });
        stage_loudness_change(&input, &paths[0])
    }
}

/// The config indices of the layers a series stage runs, in order: enabled
/// with an amount above 0 ([`Harmonics::try_new`]).
fn series_indices(config: &HarmonicsConfig) -> Vec<usize> {
    config
        .layers
        .iter()
        .enumerate()
        .filter(|(_, l)| l.enabled() && l.amount() > 0.0)
        .map(|(i, _)| i)
        .collect()
}

/// How a readout measures its tones.
struct Tones<'a> {
    output_rate: f64,
    sim_rate: f64,
    plan: ReadoutPlan,
    cache: &'a ReadoutCache,
}

impl Tones<'_> {
    /// The readout points of `source` for `(freq, level_db)` tones, in
    /// order: the slow units' tones from the cache or simulated on the
    /// worker threads, each added to the closed form of the rest.
    fn points(&self, source: &ToneSource, tones: &[(f64, f64)]) -> Vec<HarmonicsReadoutPoint> {
        let plan = self.plan;
        let (built, units, keys) = match source {
            ToneSource::Parallel { built, units, keys } => (*built, units, keys),
            ToneSource::Series { sim, key } => return self.series_points(sim, key, tones),
        };
        let n_units = units.len();
        let requests: Vec<(usize, ToneKey)> = tones
            .iter()
            .flat_map(|&(freq, level_db)| {
                let key = tone_key(freq, db_to_lin(level_db), plan.settle_s, plan.measure_s);
                (0..n_units).map(move |u| (u, key))
            })
            .collect();
        let simulated = if requests.is_empty() {
            Vec::new()
        } else {
            self.cache.tones(
                keys,
                self.sim_rate,
                MAX_THREADS - 1,
                &requests,
                |u, [f, a, ..]| {
                    unit_tone(
                        &units[u].group,
                        self.sim_rate,
                        f64::from_bits(f),
                        f64::from_bits(a),
                        plan,
                    )
                },
            )
        };
        tones
            .iter()
            .enumerate()
            .map(|(i, &(freq, level_db))| {
                let amp = db_to_lin(level_db);
                let mut h = built.steady_tone(self.output_rate, freq, amp);
                if built.enabled {
                    for unit in &simulated[i * n_units..(i + 1) * n_units] {
                        for (y, u) in h.iter_mut().zip(unit) {
                            *y += u;
                        }
                    }
                }
                spectrum_point(level_db, freq, amp, &h, self.output_rate)
            })
            .collect()
    }

    /// [`Self::points`] of a series stage: each tone through the whole
    /// stage, from the cache or simulated on the worker threads.
    fn series_points(
        &self,
        sim: &SeriesSim,
        key: &str,
        tones: &[(f64, f64)],
    ) -> Vec<HarmonicsReadoutPoint> {
        let plan = self.plan;
        let requests: Vec<(usize, ToneKey)> = tones
            .iter()
            .map(|&(freq, level_db)| {
                (
                    0,
                    tone_key(freq, db_to_lin(level_db), plan.settle_s, plan.measure_s),
                )
            })
            .collect();
        let simulated = self.cache.tones(
            &[key.to_string()],
            self.sim_rate,
            MAX_THREADS - 1,
            &requests,
            |_, [f, a, ..]| sim.tone(self.sim_rate, f64::from_bits(f), f64::from_bits(a), plan),
        );
        tones
            .iter()
            .zip(&simulated)
            .map(|(&(freq, level_db), h)| {
                spectrum_point(level_db, freq, db_to_lin(level_db), h, self.output_rate)
            })
            .collect()
    }
}

/// A cosine at about `freq` (made coherent with the window) and peak `amp`
/// at `rate`, for `settle_s` then the measured length: the samples, the
/// settle and measured lengths and the coherent frequency (rad/sample).
fn tone_signal(
    rate: f64,
    freq: f64,
    amp: f64,
    settle_s: f64,
    measure_s: f64,
) -> (Vec<f64>, usize, usize, f64) {
    let cycles = (freq * measure_s).round().max(4.0);
    let n = (cycles * rate / freq).round() as usize;
    let settle = (settle_s * rate) as usize;
    let w = 2.0 * PI * cycles / n as f64;
    let buf: Vec<f64> = (0..settle + n)
        .map(|i| amp * (w * (i as f64 - settle as f64)).cos())
        .collect();
    (buf, settle, n, w)
}

/// Complex amplitudes `[DC, H1, …, H15]` of the `n` samples `tail` of a tone
/// at `w` rad/sample (coherent: a whole number of cycles in `n`).
fn coherent_dft(tail: &[f64], n: usize, w: f64) -> ToneSpectrum {
    let mut out = [crate::Complex64::new(0.0, 0.0); READOUT_LEN];
    for (k, slot) in out.iter_mut().enumerate() {
        let step = crate::Complex64::from_polar(1.0, -w * k as f64);
        let mut z = crate::Complex64::new(1.0, 0.0);
        let mut acc = crate::Complex64::new(0.0, 0.0);
        for (i, &y) in tail.iter().enumerate() {
            acc += z * y;
            z *= step;
            if i % 1024 == 1023 {
                // Keep the phasor on the unit circle.
                z = crate::Complex64::from_polar(1.0, -w * k as f64 * (i + 1) as f64);
            }
        }
        *slot = acc * if k == 0 { 1.0 } else { 2.0 } / n as f64;
    }
    out
}

/// The steady-state tone ([`tone_signal`], settled and measured for the
/// plan's lengths, or longer if the unit asks) of a slow unit's path alone,
/// without the output scale; its slow envelopes start on their periodic
/// steady state.
fn unit_tone(
    group: &LayerGroup,
    rate: f64,
    freq: f64,
    amp: f64,
    plan: ReadoutPlan,
) -> ToneSpectrum {
    let settle_s = plan.settle_s.max(group.readout_settle_s());
    let (buf, settle, n, w) = tone_signal(rate, freq, amp, settle_s, plan.measure_s);
    let mut xc = vec![0.0; buf.len()];
    clamp_input(&buf, &mut xc);
    let path = group.path(&xc, n.min(xc.len()));
    coherent_dft(&path[settle..], n, w)
}

/// GedLee metric `Gm = (∫₋₁¹ (cos²(πx/2) c''(x))² dx)^½` (Geddes and Lee,
/// 2003) of the curve `c(x) = Σ h_k T_k(x)` (`h[1] = 1`): the static curve
/// whose full-scale harmonics are `h`. Exact for Chebyshev layers; for flux
/// layers it is the static curve with the same harmonic levels at the tone
/// (H2..=H15).
fn gedlee_gm(h: &[f64; READOUT_LEN]) -> f64 {
    let t = chebyshev_power_forms_to::<READOUT_LEN>();
    let mut p = [0.0; READOUT_LEN];
    for k in 2..READOUT_LEN {
        for m in 0..READOUT_LEN {
            p[m] += h[k] * t[k][m];
        }
    }
    // c''(x) in power form.
    let d2: [f64; READOUT_LEN] = std::array::from_fn(|m| {
        if m + 2 < READOUT_LEN {
            p[m + 2] * ((m + 2) * (m + 1)) as f64
        } else {
            0.0
        }
    });
    // Composite Simpson's rule (the integrand is a smooth polynomial times
    // cos⁸, so 2048 intervals are far beyond the readout's precision).
    const N: usize = 2048;
    let dx = 2.0 / N as f64;
    let f = |x: f64| {
        let c = (0.5 * PI * x).cos();
        let v = c * c * d2.iter().rev().fold(0.0, |acc, &cm| acc * x + cm);
        v * v
    };
    let mut sum = f(-1.0) + f(1.0);
    for i in 1..N {
        let x = -1.0 + i as f64 * dx;
        sum += if i % 2 == 1 { 4.0 } else { 2.0 } * f(x);
    }
    (sum * dx / 3.0).sqrt()
}

/// The input every layer sees, as the bank clamps it: non-finite samples
/// are 0, the rest clamped to full scale.
#[inline(always)]
pub(crate) fn clamp_input(chan: &[f64], xc: &mut [f64]) {
    for (x, &s) in xc.iter_mut().zip(chan) {
        let v = if s.is_finite() { s } else { 0.0 };
        *x = v.clamp(-1.0, 1.0);
    }
}

impl Stage for Harmonics {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if !self.enabled {
            return;
        }
        if !self.series.is_empty() {
            // Each layer over the whole block, in list order.
            for layer in &mut self.series {
                layer.process(chans);
            }
            return;
        }
        let nch = chans.len().min(self.channels);
        let chans = &mut chans[..nch];
        if self.groups.is_empty() {
            // The bank alone.
            for (c, chan) in chans.iter_mut().enumerate() {
                self.bank.run(c, chan);
                self.bank.end_block(c);
            }
            return;
        }
        // Channel pairs in lockstep when a kind pairs, so its two serial
        // recurrences (hysteresis) run as 2-lane packed instructions, bit for
        // bit as the single-channel path the channel workers use.
        let mut xc = [[0.0; CHUNK]; 2];
        let mut acc = [[0.0; CHUNK]; 2];
        let mut c = 0;
        while self.paired && c + 1 < nch {
            let (head, tail) = chans.split_at_mut(c + 1);
            let (a, b) = (&mut *head[c], &mut *tail[0]);
            let len = a.len().min(b.len());
            let (a, a_rest) = a.split_at_mut(len);
            let (b, b_rest) = b.split_at_mut(len);
            for (ca, cb) in a.chunks_mut(CHUNK).zip(b.chunks_mut(CHUNK)) {
                self.pair_chunk(c, [ca, cb], &mut xc, &mut acc);
            }
            for ca in a_rest.chunks_mut(CHUNK) {
                self.chunk(c, ca, &mut xc[0], &mut acc[0]);
            }
            for cb in b_rest.chunks_mut(CHUNK) {
                self.chunk(c + 1, cb, &mut xc[1], &mut acc[1]);
            }
            self.end_block(c);
            self.end_block(c + 1);
            c += 2;
        }
        // The rest (every channel when no kind pairs), one at a time.
        for (c, chan) in chans.iter_mut().enumerate().skip(c) {
            for chunk in chan.chunks_mut(CHUNK) {
                self.chunk(c, chunk, &mut xc[0], &mut acc[0]);
            }
            self.end_block(c);
        }
    }

    fn reset(&mut self) {
        self.bank.reset();
        for group in &mut self.groups {
            group.reset();
        }
        for layer in &mut self.series {
            layer.reset();
        }
    }
}

impl Harmonics {
    /// End of a block on channel `c`: the bank's and the layers' filter
    /// states flush their decaying tails.
    #[inline]
    fn end_block(&mut self, c: usize) {
        self.bank.end_block(c);
        for group in &mut self.groups {
            group.end_block(c);
        }
    }

    /// One block (at most [`CHUNK`]) of channel `c`: the clamped input
    /// kept, the bank in place, then each group's sum added.
    #[inline]
    fn chunk(&mut self, c: usize, out: &mut [f64], xc: &mut [f64; CHUNK], acc: &mut [f64; CHUNK]) {
        let n = out.len().min(CHUNK);
        let (out, xc, acc) = (&mut out[..n], &mut xc[..n], &mut acc[..n]);
        clamp_input(out, xc);
        self.bank.run(c, out);
        for group in &mut self.groups {
            group.run(c, xc, acc);
            for (y, &v) in out.iter_mut().zip(acc.iter()) {
                *y += v;
            }
        }
    }

    /// [`Self::chunk`] on channels `c` and `c + 1` in lockstep (blocks of
    /// equal length).
    #[inline]
    fn pair_chunk(
        &mut self,
        c: usize,
        out: [&mut [f64]; 2],
        xc: &mut [[f64; CHUNK]; 2],
        acc: &mut [[f64; CHUNK]; 2],
    ) {
        let [o0, o1] = out;
        let n = o0.len().min(o1.len()).min(CHUNK);
        let (o0, o1) = (&mut o0[..n], &mut o1[..n]);
        let [x0, x1] = xc;
        let (x0, x1) = (&mut x0[..n], &mut x1[..n]);
        let [a0, a1] = acc;
        let (a0, a1) = (&mut a0[..n], &mut a1[..n]);
        clamp_input(o0, x0);
        clamp_input(o1, x1);
        self.bank.run(c, o0);
        self.bank.run(c + 1, o1);
        for group in &mut self.groups {
            group.run_pair(c, [x0, x1], [a0, a1]);
            for (y, &v) in o0.iter_mut().zip(a0.iter()) {
                *y += v;
            }
            for (y, &v) in o1.iter_mut().zip(a1.iter()) {
                *y += v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{FluxLayer, HARMONICS_DEFAULT_SIGNS, HarmonicSign, HysteresisLayer};
    use rustfft::FftPlanner;

    const P: HarmonicSign = HarmonicSign::Positive;
    const N: HarmonicSign = HarmonicSign::Negative;

    /// The stage's certified peak bound for inputs within full scale
    /// (linear): what its output may reach, nothing cutting it.
    fn bound(cfg: &HarmonicsConfig, rate: f64) -> f64 {
        db_to_lin(Harmonics::try_new(cfg, rate, 1).unwrap().peak_gain_db())
    }

    fn cheb(levels: [Option<f64>; 8], signs: [HarmonicSign; 8]) -> ChebyshevLayer {
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

    fn stage_of(layers: Vec<HarmonicsLayer>) -> HarmonicsConfig {
        HarmonicsConfig {
            system: None,
            enabled: true,
            layers,
            dc_block: true,
            routing: Default::default(),
        }
    }

    /// One Chebyshev layer with levels `[h2, …, h9]`.
    fn orders(levels: [Option<f64>; 8], signs: [HarmonicSign; 8]) -> HarmonicsConfig {
        stage_of(vec![HarmonicsLayer::Chebyshev(cheb(levels, signs))])
    }

    fn preset(id: &str) -> HarmonicsConfig {
        HarmonicsConfig::from_preset(id).unwrap()
    }

    fn flux(
        corner_hz: f64,
        max_boost_db: f64,
        target_h3_db: f64,
        calibration_hz: f64,
    ) -> HarmonicsLayer {
        HarmonicsLayer::Flux(FluxLayer {
            enabled: true,
            preset: None,
            corner_hz,
            max_boost_db,
            target_h3_db,
            calibration_hz,
        })
    }

    fn hyst(
        saturation: f64,
        width: f64,
        amount: f64,
        target_h3_db: f64,
        calibration_hz: f64,
    ) -> HarmonicsLayer {
        HarmonicsLayer::Hysteresis(HysteresisLayer {
            enabled: true,
            preset: None,
            saturation,
            width,
            amount,
            target_h3_db,
            calibration_hz,
            bass_eq: None,
            asymmetry: None,
        })
    }

    fn cheb_layer(cfg: &mut HarmonicsConfig) -> &mut ChebyshevLayer {
        match &mut cfg.layers[0] {
            HarmonicsLayer::Chebyshev(l) => l,
            HarmonicsLayer::Flux(_) | HarmonicsLayer::Hysteresis(_) => {
                panic!("not a Chebyshev layer")
            }
        }
    }

    fn custom(h2: f64, h3: f64, h4: Option<f64>, h5: Option<f64>) -> HarmonicsConfig {
        orders(
            [Some(h2), Some(h3), h4, h5, None, None, None, None],
            HARMONICS_DEFAULT_SIGNS,
        )
    }

    /// Every order at `db`, with `signs`.
    fn all(db: f64, signs: [HarmonicSign; 8]) -> HarmonicsConfig {
        orders([Some(db); 8], signs)
    }

    struct Measured {
        /// `[DC, H1..H9]` linear amplitudes (absolute).
        amp: [f64; 10],
        /// The same, signed (cosine phase: the real part of each bin).
        re: [f64; 10],
        peak: f64,
    }

    impl Measured {}

    /// Coherent ~1 kHz sine at `amp` peak, measured after 0.5 s of settling.
    fn measure(cfg: &HarmonicsConfig, rate: f64, amp: f64) -> Measured {
        let mut stage = Harmonics::try_new(cfg, rate, 1).unwrap();
        let settle = (rate * 0.5) as usize;
        let n = 1 << 14;
        let bin = ((1000.0 * n as f64) / rate).round() as usize;
        let f = bin as f64 * rate / n as f64;
        let mut buf: Vec<f64> = (0..settle + n)
            .map(|i| amp * (2.0 * PI * f * (i as f64 - settle as f64) / rate).cos())
            .collect();
        // Odd block sizes.
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
        let mut spec: Vec<crate::Complex64> = tail
            .iter()
            .map(|&s| crate::Complex64::new(s, 0.0))
            .collect();
        FftPlanner::new().plan_fft_forward(n).process(&mut spec);
        let mut amp_out = [0.0; 10];
        let mut re_out = [0.0; 10];
        // The input is a cosine at the window's start, so the real part of
        // each bin carries the harmonic's sign.
        amp_out[0] = spec[0].re.abs() / n as f64;
        re_out[0] = spec[0].re / n as f64;
        for k in 1..10 {
            amp_out[k] = 2.0 * spec[bin * k].norm() / n as f64;
            re_out[k] = 2.0 * spec[bin * k].re / n as f64;
        }
        Measured {
            amp: amp_out,
            re: re_out,
            peak,
        }
    }

    /// The H2–H5 power-form stage, the reference for the "H6–H9 off
    /// matches" test (no safety scale or bound, like the stage).
    mod legacy {}

    #[test]
    fn spectra_follow_the_closed_form_with_signs_up_to_h9() {
        let rate = 192_000.0;
        let cases = [
            custom(-30.0, -40.0, Some(-50.0), Some(-60.0)),
            custom(-26.0, -24.0, Some(-22.0), Some(-20.0)),
            orders(
                [-30.0, -35.0, -40.0, -45.0, -24.0, -22.0, -26.0, -20.0].map(Some),
                [P, N, N, P, N, P, P, N],
            ),
            orders(
                [
                    Some(-40.0),
                    Some(-30.0),
                    None,
                    Some(-50.0),
                    Some(-20.0),
                    None,
                    Some(-60.0),
                    Some(-25.0),
                ],
                [N, P, P, N, P, P, N, P],
            ),
            all(-20.0, [N; 8]),
        ];
        for cfg in cases {
            let stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
            for level_db in [0.0, -6.0, -20.0] {
                let a = db_to_lin(level_db);
                let m = measure(&cfg, rate, a);
                let want = harmonic_amplitudes(stage.amplitudes(), a);
                for k in 1..10 {
                    // The 5 Hz DC blocker changes 2-9 kHz by < 1e-5 relative
                    // (gain and the real part of a ~1 mrad phase).
                    let tol = 1e-5 * want[1].abs() + 1e-12;
                    let err = (m.amp[k] - want[k].abs()).abs();
                    assert!(
                        err < tol,
                        "{cfg:?} {level_db} dBFS H{k}: {} vs {}",
                        m.amp[k],
                        want[k].abs()
                    );
                    let err = (m.re[k] - want[k]).abs();
                    assert!(
                        err < tol,
                        "{cfg:?} {level_db} dBFS H{k} signed: {} vs {}",
                        m.re[k],
                        want[k]
                    );
                }
                assert!(m.amp[0] < 1e-6, "DC {}", m.amp[0]);
            }
        }
    }

    #[test]
    fn closed_form_is_exact_at_full_scale_and_signed() {
        let a = [0.01, -0.02, 0.03, -0.04, 0.05, -0.06, 0.07, -0.08];
        let h = harmonic_amplitudes(a, 1.0);
        assert!((h[1] - 1.0).abs() < 1e-13, "H1 {}", h[1]);
        for (i, ak) in a.iter().enumerate() {
            assert!((h[i + 2] - ak).abs() < 1e-13, "H{} {}", i + 2, h[i + 2]);
        }
        // DC of x + Σ a_k (T_k − T_k(0)) is −Σ_even a_k T_k(0).
        // T_2(0) = T_6(0) = −1, T_4(0) = T_8(0) = 1.
        let dc = 0.01 - 0.03 + 0.05 - 0.07;
        assert!((h[0] - dc).abs() < 1e-13, "DC {}", h[0]);
        // The top order scales as A^k: H9 = a9 A⁹ at any level.
        let h = harmonic_amplitudes([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.1], 0.01);
        assert!((h[9] - 1e-19).abs() < 1e-30, "{}", h[9]);
    }

    /// Settles on `from`, then steps to `to`; returns the peak of the step.
    fn step_peak(cfg: &HarmonicsConfig, rate: f64, from: f64, to: f64) -> f64 {
        let mut st = Harmonics::try_new(cfg, rate, 1).unwrap();
        let mut buf = vec![from; rate as usize];
        buf.extend(std::iter::repeat_n(to, 64));
        st.process(&mut [&mut buf]);
        buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()))
    }

    /// Grid arg-extrema of the even part `e` and of `o + b0 e` (b0 ≈ 1),
    /// the worst settle-then-step pair for the DC-blocked bound.
    fn worst_steps(cfg: &HarmonicsConfig) -> Vec<(f64, f64)> {
        let st = Harmonics::try_new(cfg, 192_000.0, 1).unwrap();
        let a = st.amplitudes();
        let shaped = |x: f64, even: bool| -> f64 {
            let t = chebyshev_power_forms();
            let mut y = if even { 0.0 } else { x };
            for (i, ak) in a.iter().enumerate() {
                let k = i + 2;
                if (k % 2 == 0) == even || !even {
                    y += ak * (eval(&t[k], x) - T_AT_0[k]);
                }
            }
            y
        };
        let xs: Vec<f64> = (0..=4096).map(|i| -1.0 + i as f64 / 2048.0).collect();
        let arg = |f: &dyn Fn(f64) -> f64, max: bool| -> f64 {
            *xs.iter()
                .max_by(|&&p, &&q| {
                    let (fp, fq) = (f(p), f(q));
                    if max {
                        fp.total_cmp(&fq)
                    } else {
                        fq.total_cmp(&fp)
                    }
                })
                .unwrap()
        };
        let e = |x: f64| shaped(x, true);
        let total = |x: f64| shaped(x, false);
        let (e_min, e_max) = (arg(&e, false), arg(&e, true));
        let (t_min, t_max) = (arg(&total, false), arg(&total, true));
        vec![(e_min, t_max), (e_max, t_min)]
    }

    #[test]
    fn output_stays_within_the_bound_for_worst_case_inputs() {
        let rate = 192_000.0;
        let cases = [
            custom(-120.0, -120.0, Some(-20.0), None),
            custom(-20.0, -20.0, Some(-20.0), Some(-20.0)),
            custom(-20.0, -120.0, None, None),
            preset("pentode"),
            all(-20.0, HARMONICS_DEFAULT_SIGNS),
            all(-20.0, [P; 8]),
            all(-20.0, [N; 8]),
            all(-20.0, [P, N, N, P, N, P, P, N]),
            orders(
                [
                    Some(-120.0),
                    Some(-120.0),
                    None,
                    None,
                    Some(-20.0),
                    None,
                    Some(-20.0),
                    None,
                ],
                [P, P, P, P, N, P, P, P],
            ),
            orders(
                [
                    Some(-120.0),
                    Some(-120.0),
                    None,
                    None,
                    None,
                    Some(-20.0),
                    None,
                    Some(-20.0),
                ],
                [P, P, P, P, P, N, P, P],
            ),
        ];
        for cfg in cases {
            for dc_block in [true, false] {
                let cfg = HarmonicsConfig {
                    dc_block,
                    ..cfg.clone()
                };
                let b = bound(&cfg, rate);
                let m = measure(&cfg, rate, 1.0);
                assert!(m.peak <= b, "{cfg:?}: sine peak {} > {b}", m.peak);
                let mut steps = vec![
                    (1.0, -1.0),
                    (-1.0, 1.0),
                    (0.5f64.sqrt(), 1.0),
                    (0.5f64.sqrt(), -1.0),
                    (0.0, 1.0),
                ];
                steps.extend(worst_steps(&cfg));
                for (from, to) in steps {
                    let peak = step_peak(&cfg, rate, from, to);
                    assert!(peak <= b, "{cfg:?}: step {from}->{to} peak {peak} > {b}");
                }
            }
        }
    }

    #[test]
    fn certified_range_contains_the_true_extrema() {
        let t = chebyshev_power_forms();
        // T4 - 1 has its minimum -2 at x² = 1/2, between grid points.
        let c = add(&t[4], &t[0], -1.0);
        let (lo, hi) = certified_range(&c);
        assert!(lo <= -2.0 && lo > -2.0 - 1e-5, "{lo}");
        assert!((0.0..1e-5).contains(&hi), "{hi}");
        // T8 - 1 and T9: extrema at cos(jπ/k), mostly off the grid. The
        // margin uses the power-form |f''| bound (as for H2-H5), about
        // 1.6e-3 for a bare T9, so 1.6e-4 at the -20 dB maximum.
        let c = add(&t[8], &t[0], -1.0);
        let (lo, hi) = certified_range(&c);
        assert!(lo <= -2.0 && lo > -2.0 - 2e-3, "{lo}");
        assert!((0.0..2e-3).contains(&hi), "{hi}");
        let (lo, hi) = certified_range(&t[9]);
        assert!(lo <= -1.0 && lo > -1.0 - 2e-3, "{lo}");
        assert!((1.0..1.0 + 2e-3).contains(&hi), "{hi}");
    }

    #[test]
    fn non_finite_input_does_not_poison_state() {
        for cfg in [
            custom(-30.0, -40.0, Some(-50.0), None),
            all(-30.0, [P, N, N, P, N, P, P, N]),
        ] {
            let mut st = Harmonics::try_new(&cfg, 48_000.0, 1).unwrap();
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
    }

    #[test]
    fn silence_outputs_exact_zero() {
        for cfg in [HarmonicsConfig::default(), all(-20.0, [N; 8])] {
            let mut stage = Harmonics::try_new(&cfg, 192_000.0, 2).unwrap();
            let mut l = vec![0.0; 256];
            let mut r = vec![0.0; 256];
            stage.process(&mut [&mut l, &mut r]);
            assert!(l.iter().all(|&x| x == 0.0));
            assert!(r.iter().all(|&x| x == 0.0));
        }
    }

    /// Full-scale square at `hz` for `secs`, then a step to the opposite rail.
    fn square_peak(cfg: &HarmonicsConfig, rate: f64, hz: f64) -> f64 {
        let mut st = Harmonics::try_new(cfg, rate, 1).unwrap();
        let half = (rate / hz / 2.0).round() as usize;
        let mut buf: Vec<f64> = (0..(rate as usize))
            .map(|i| {
                if (i / half).is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                }
            })
            .collect();
        st.process(&mut [&mut buf]);
        buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()))
    }

    #[test]
    fn readout_matches_the_closed_form_and_flux_calibration() {
        let rate = 192_000.0;
        let cfg = custom(-30.0, -40.0, Some(-50.0), Some(-60.0));
        let r = readout(&cfg, rate, false).unwrap();
        let a = amplitudes(&cfg).unwrap();
        assert_eq!(r.points.len(), 4);
        for p in &r.points {
            let h = harmonic_amplitudes(a, db_to_lin(p.input_dbfs));
            // The 5 Hz DC blocker takes 0.02 dB off H2 of 40 Hz.
            let tol = if p.freq_hz < 100.0 { 0.05 } else { 0.01 };
            for k in 2..6 {
                let want = lin_to_db(h[k].abs() / h[1]);
                assert!(
                    (p.harmonics_db[k - 2] - want).abs() < tol,
                    "{p:?} H{k} {want}"
                );
            }
            let thd = lin_to_db((2..10).map(|k| h[k] * h[k]).sum::<f64>().sqrt() / h[1]);
            assert!((p.thd_db - thd).abs() < tol, "{p:?} {thd}");
        }
        assert!(
            (r.peak_gain_db - Harmonics::try_new(&cfg, rate, 1).unwrap().peak_gain_db()).abs()
                < 1e-12
        );
        assert_eq!(
            r.level_change_db,
            Harmonics::loudness_change_db(&cfg).unwrap()
        );

        // Gm of a pure H2 curve: c'' = 4 a2, ∫ cos⁴(πx/2) dx = 3/4 (H3 at
        // -120 dB adds a negligible 24·a3·x to c'').
        let r = readout(&custom(-40.0, -120.0, None, None), rate, false).unwrap();
        let p = &r.points[0];
        let want = 4.0 * db_to_lin(-40.0) * 0.75f64.sqrt();
        assert!(
            (p.gedlee_gm - want).abs() < 1e-4,
            "{} vs {want}",
            p.gedlee_gm
        );
        // Gm falls with level.
        assert!(r.points[2].gedlee_gm < p.gedlee_gm);

        // Flux: calibrated H3 at 40 Hz and 0 dBFS; low bass distorts more.
        let r = readout(&stage_of(vec![flux(100.0, 15.0, -30.0, 40.0)]), rate, false).unwrap();
        let at = |db: f64, hz: f64| {
            r.points
                .iter()
                .find(|p| p.input_dbfs == db && p.freq_hz == hz)
                .unwrap()
        };
        assert!(
            (at(0.0, 40.0).harmonics_db[1] + 30.0).abs() < 0.1,
            "{:?}",
            at(0.0, 40.0)
        );
        assert!(at(0.0, 1000.0).thd_db < at(0.0, 40.0).thd_db - 20.0);
        assert!(r.sweep.is_empty());

        // The sweep: 21 levels at each tone, agreeing with the fixed points.
        let r = readout(&cfg, rate, true).unwrap();
        assert_eq!(r.sweep.len(), 42);
        assert_eq!(r.sweep[0].input_dbfs, -40.0);
        assert_eq!(r.sweep[20].input_dbfs, 0.0);
        assert_eq!(r.sweep[21].freq_hz, 40.0);
        assert_eq!(r.sweep[20], r.points[0]);
        let a = amplitudes(&cfg).unwrap();
        for p in r.sweep.iter().filter(|p| p.freq_hz > 100.0) {
            let h = harmonic_amplitudes(a, db_to_lin(p.input_dbfs));
            let want = lin_to_db(h[2].abs() / h[1]);
            assert!((p.harmonics_db[0] - want).abs() < 0.01, "{p:?} {want}");
        }
        // About 2 dB per dB of level.
        let drop = at(0.0, 40.0).harmonics_db[1] - at(-18.0, 40.0).harmonics_db[1];
        assert!((drop - 36.0).abs() < 3.0, "{drop}");
        assert!(at(0.0, 40.0).gedlee_gm > at(-18.0, 40.0).gedlee_gm);
        // H2-H5 at 192 kHz: nothing folds into the band.
        assert!(!r.aliasing.warn && r.aliasing.level_db == READOUT_FLOOR_DB);
    }

    #[test]
    fn every_system_builds_from_layer_presets_and_stays_within_full_scale() {
        use audivane_protocol::{harmonics_preset, harmonics_systems};
        assert!(harmonics_systems().len() >= 5);
        let mut ids: Vec<&str> = harmonics_systems().iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), harmonics_systems().len(), "duplicate system ids");
        for s in harmonics_systems() {
            assert!(!s.layers.is_empty(), "{}", s.id);
            assert!(
                s.layers.len() <= audivane_protocol::HARMONICS_MAX_LAYERS,
                "{}",
                s.id
            );
            // Every layer is an ordinary layer from a known preset of its kind.
            for l in &s.layers {
                let p = harmonics_preset(l.preset().unwrap_or_default())
                    .unwrap_or_else(|| panic!("{}: unknown layer preset", s.id));
                assert_eq!(
                    std::mem::discriminant(&p.layer),
                    std::mem::discriminant(l),
                    "{}: {}",
                    s.id,
                    p.id
                );
                assert!(l.enabled(), "{}", s.id);
            }
            let cfg = s.to_config();
            for rate in [44_100.0, 96_000.0, 768_000.0] {
                let st = Harmonics::try_new(&cfg, rate, 1)
                    .unwrap_or_else(|e| panic!("{} at {rate}: {e}", s.id));
                assert!(st.peak_gain_db().is_finite());
            }
            let b = bound(&cfg, 96_000.0);
            for hz in [20.0, 40.0, 1000.0] {
                let peak = square_peak(&cfg, 96_000.0, hz);
                assert!(peak <= b, "{} square {hz}: {peak} > {b}", s.id);
            }
        }
    }

    #[test]
    fn every_preset_builds_and_stays_within_full_scale() {
        use audivane_protocol::harmonics_presets;
        assert!(harmonics_presets().len() >= 10);
        for p in harmonics_presets() {
            let cfg = HarmonicsConfig::from_preset(&p.id).unwrap();
            assert_eq!(cfg.layers[0].preset(), Some(p.id.as_str()));
            assert_eq!(cfg.layers[0].is_modified(), Some(false));
            for rate in [44_100.0, 96_000.0, 768_000.0] {
                let st = Harmonics::try_new(&cfg, rate, 1)
                    .unwrap_or_else(|e| panic!("{} at {rate}: {e}", p.id));
                assert!(st.peak_gain_db().is_finite());
            }
            let b = bound(&cfg, 96_000.0);
            for hz in [20.0, 40.0, 1000.0] {
                let peak = square_peak(&cfg, 96_000.0, hz);
                assert!(peak <= b, "{} square {hz}: {peak} > {b}", p.id);
            }
            // Preset levels respect the per-order cap.
            if let HarmonicsLayer::Chebyshev(l) = &p.layer {
                assert!(layer_amplitudes(l).is_ok(), "{}", p.id);
            }
        }
        // Editing a preset layer marks it modified.
        let mut cfg = preset("triode");
        cheb_layer(&mut cfg).drive_db = -3.0;
        assert_eq!(cfg.layers[0].is_modified(), Some(true));

        let mut hcfg = preset("tape_studio");
        assert_eq!(hcfg.layers[0].is_modified(), Some(false));
        if let HarmonicsLayer::Hysteresis(l) = &mut hcfg.layers[0] {
            l.width = 0.45;
        }
        assert_eq!(hcfg.layers[0].is_modified(), Some(true));
    }

    #[test]
    fn combined_bound_holds_with_hysteresis_layers() {
        let hot_cheb = HarmonicsLayer::Chebyshev(cheb(
            [
                Some(-20.0),
                Some(-20.0),
                Some(-20.0),
                Some(-20.0),
                None,
                None,
                None,
                None,
            ],
            HARMONICS_DEFAULT_SIGNS,
        ));
        let cases = [
            // Single heavy hysteresis layer
            stage_of(vec![hyst(0.1, 0.5, 1.0, -15.0, 1_000.0)]),
            // Chebyshev + hysteresis
            stage_of(vec![hot_cheb.clone(), hyst(0.2, 0.5, 1.0, -18.0, 1_000.0)]),
            // Flux + hysteresis
            stage_of(vec![
                flux(100.0, 18.0, -18.0, 20.0),
                hyst(0.15, 0.45, 1.0, -20.0, 1_000.0),
            ]),
            // All three kinds together
            stage_of(vec![
                hot_cheb.clone(),
                flux(400.0, 12.0, -20.0, 100.0),
                hyst(0.1, 0.5, 1.0, -18.0, 1_000.0),
            ]),
            // Two stacked hysteresis layers (exercises greedy theta split)
            stage_of(vec![
                hyst(0.1, 0.5, 1.0, -18.0, 1_000.0),
                hyst(0.5, 0.4, 1.0, -20.0, 400.0),
            ]),
            // Three stacked hysteresis layers with dc_block off for Chebyshev
            HarmonicsConfig {
                system: None,
                enabled: true,
                layers: vec![
                    hot_cheb,
                    hyst(0.0, 1.0, 1.0, -20.0, 1_000.0),
                    hyst(1.0, 0.25, 0.6, -24.0, 1_000.0),
                ],
                dc_block: false,
                routing: Default::default(),
            },
        ];

        for rate in [44_100.0, 768_000.0] {
            // Keep sample counts reasonable at 768 kHz while still covering
            // full low-frequency periods and HPF time constants.
            let n_short = if rate > 100_000.0 { 65_536 } else { 24_000 };
            for cfg in &cases {
                let mut st = Harmonics::try_new(cfg, rate, 1).unwrap();
                let b = db_to_lin(st.peak_gain_db());

                // 1. Full-scale squares (slow and fast)
                for half in [1usize, 4, 64, (rate / 40.0).round() as usize] {
                    st.reset();
                    let mut buf: Vec<f64> = (0..n_short)
                        .map(|i| {
                            if (i / half).is_multiple_of(2) {
                                1.0
                            } else {
                                -1.0
                            }
                        })
                        .collect();
                    st.process(&mut [&mut buf]);
                    let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                    assert!(
                        peak <= b,
                        "rate={rate} square half={half}: peak={peak} > {b}"
                    );
                }

                // 2. Full-scale steps after holding opposite rails
                for (from, to) in [(1.0, -1.0), (-1.0, 1.0), (0.0, 1.0), (0.7, -1.0)] {
                    st.reset();
                    let mut buf = vec![from; n_short / 2];
                    buf.extend(std::iter::repeat_n(to, n_short / 2));
                    st.process(&mut [&mut buf]);
                    let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                    assert!(
                        peak <= b,
                        "rate={rate} step {from}->{to}: peak={peak} > {b}"
                    );
                }

                // 3. Asymmetric ratchets (pumps magnetic remanence + HPF state)
                st.reset();
                let mut ratchet = Vec::with_capacity(n_short);
                for i in 0..n_short {
                    let phase = i % 256;
                    let x = if phase < 200 {
                        // Ramp up slowly to +1, then slam to -1 and bounce
                        -0.2 + 1.2 * (phase as f64 / 199.0)
                    } else if phase % 2 == 0 {
                        -1.0
                    } else {
                        1.0
                    };
                    ratchet.push(x.clamp(-1.0, 1.0));
                }
                st.process(&mut [&mut ratchet]);
                let peak = ratchet.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                assert!(peak <= b, "rate={rate} ratchet: peak={peak} > {b}");

                // 4. Full-scale and -3 dBFS sines across frequencies
                for hz in [20.0, 1_000.0, 10_000.0] {
                    for amp in [1.0, 0.7] {
                        st.reset();
                        let mut buf: Vec<f64> = (0..n_short)
                            .map(|i| amp * (2.0 * PI * hz * i as f64 / rate).sin())
                            .collect();
                        st.process(&mut [&mut buf]);
                        let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                        assert!(
                            peak <= b,
                            "rate={rate} sine {hz} Hz amp={amp}: peak={peak} > {b}"
                        );
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ tilt

    use audivane_protocol::{HarmonicsTilt, OrderTilt, TiltShelf};

    fn shelf(hz: f64, db: f64) -> TiltShelf {
        TiltShelf::new(hz, db)
    }

    fn tilt(low: (f64, f64), high: (f64, f64), low_even: bool) -> HarmonicsTilt {
        HarmonicsTilt {
            low: shelf(low.0, low.1),
            high: shelf(high.0, high.1),
            low_even,
            ..HarmonicsTilt::default()
        }
    }

    /// One Chebyshev layer with `levels` (default signs) and tilt `t`.
    fn tilted(levels: [Option<f64>; 8], t: HarmonicsTilt) -> HarmonicsConfig {
        let mut l = cheb(levels, HARMONICS_DEFAULT_SIGNS);
        l.tilt = Some(Box::new(t));
        stage_of(vec![HarmonicsLayer::Chebyshev(l)])
    }

    const FIVE: [Option<f64>; 8] = [
        Some(-30.0),
        Some(-40.0),
        Some(-50.0),
        Some(-60.0),
        Some(-70.0),
        None,
        None,
        None,
    ];

    /// A loud, varied test signal with overs.
    fn noise_input(len: usize) -> Vec<f64> {
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let w = (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.4 - 1.2;
                if i % 5 == 0 {
                    (i as f64 * 0.013).sin()
                } else {
                    w
                }
            })
            .collect()
    }

    #[test]
    fn a_flat_tilt_runs_the_untilted_stage_bit_for_bit() {
        let rate = 96_000.0;
        let mut overridden = tilt((100.0, 0.0), (2_000.0, 0.0), true);
        overridden.orders[1] = Some(OrderTilt {
            low: shelf(300.0, 0.0),
            high: shelf(5_000.0, 0.0),
        });
        for cfg in [
            preset("se_triode_high_mu"),
            all(-20.0, [P, N, N, P, N, P, P, N]),
        ] {
            for t in [HarmonicsTilt::default(), overridden.clone()] {
                let mut with = cfg.clone();
                cheb_layer(&mut with).tilt = Some(Box::new(t));
                // Plus a flux layer, so the combined bound path runs too.
                let mut cfg = cfg.clone();
                for c in [&mut cfg, &mut with] {
                    c.layers.push(flux(80.0, 12.0, -40.0, 30.0));
                }
                let mut a = Harmonics::try_new(&cfg, rate, 1).unwrap();
                let mut b = Harmonics::try_new(&with, rate, 1).unwrap();
                assert_eq!(b.tilted_terms(), 0);
                assert_eq!(a.peak_gain_db().to_bits(), b.peak_gain_db().to_bits());
                let mut x = noise_input(20_000);
                let mut y = x.clone();
                a.process(&mut [&mut x]);
                b.process(&mut [&mut y]);
                assert!(x.iter().zip(&y).all(|(p, q)| p.to_bits() == q.to_bits()));
                // A flat tilt is not a modification.
                assert_eq!(with.layers[0].params(), cfg.layers[0].params());
            }
        }
    }

    /// [`tilt`] with both shelves at `slope` dB/oct.
    fn steep(low: (f64, f64), high: (f64, f64), low_even: bool, slope: u8) -> HarmonicsTilt {
        let mut t = tilt(low, high, low_even);
        t.low.slope = Some(slope);
        t.high.slope = Some(slope);
        t
    }

    /// The strong-tilt cases of the joint bound's tests: every order at
    /// -20 dB (default and alternating signs) under ±24 dB shelves, and a
    /// per-order override.
    fn strong_tilts() -> Vec<HarmonicsConfig> {
        let hot = [Some(-20.0); 8];
        let mut overridden = tilt((50.0, 24.0), (500.0, 24.0), true);
        overridden.orders[1] = Some(OrderTilt {
            low: shelf(20.0, 24.0),
            high: shelf(200.0, -24.0),
        });
        let mut out = vec![tilted(hot, overridden)];
        for t in [
            tilt((20.0, 24.0), (200.0, 24.0), true),
            tilt((2_000.0, -24.0), (20_000.0, -24.0), true),
            tilt((2_000.0, 24.0), (20_000.0, 24.0), false),
            tilt((100.0, 24.0), (1_000.0, 24.0), true),
            steep((100.0, -24.0), (1_000.0, 24.0), true, 24),
            steep((300.0, 24.0), (3_000.0, -24.0), false, 6),
        ] {
            out.push(tilted(hot, t.clone()));
            let mut l = cheb(hot, [P, N, N, P, P, N, N, P]);
            l.tilt = Some(Box::new(t));
            out.push(stage_of(vec![HarmonicsLayer::Chebyshev(l)]));
        }
        // Few orders, one dominant: the joint bound is the per-term one.
        out.push(tilted(
            [None, Some(-20.0), None, Some(-26.0), None, None, None, None],
            tilt((100.0, 24.0), (1_000.0, 24.0), false),
        ));
        out
    }

    #[test]
    fn the_joint_tilt_bound_is_reached_by_its_worst_input_and_never_exceeded() {
        // The worst input of the joint bound (each lag at its own maximum
        // of F_m, from rest) drives the real stage to its peak at the last
        // sample: within full scale, and within the grid margin and the
        // tail of it wherever the joint bound sets the trim.
        for rate in [44_100.0, 96_000.0] {
            for cfg in strong_tilts() {
                for dc_block in [true, false] {
                    let cfg = HarmonicsConfig {
                        dc_block,
                        ..cfg.clone()
                    };
                    let mut st = Harmonics::try_new(&cfg, rate, 1).unwrap();
                    let (flat, filtered) = split_terms(&cfg, rate).unwrap();
                    let mut a = [0.0; LEN];
                    a[2..].copy_from_slice(&flat);
                    let total = amplitudes(&cfg).unwrap();
                    let linear = 1.0
                        + (3..LEN)
                            .step_by(2)
                            .map(|k| total[k - 2] * T_SLOPE_0[k])
                            .sum::<f64>();
                    let (b0, a1) = (st.bank.hpf_b0, st.bank.hpf_a1);
                    let terms = joint_terms(&a, &filtered, dc_block);
                    let joint = JointBound::new(&terms, (b0, a1)).bound(linear);
                    let own: f64 = filtered
                        .iter()
                        .map(|(k, f, w)| w.abs() * tilt_term_bound(*k, f, dc_block, b0))
                        .sum();
                    let per_term = chebyshev_bound(&a, linear, dc_block, b0) + own;
                    assert!(
                        joint <= per_term * (1.0 + 1e-6),
                        "{cfg:?}: {joint} > {per_term}"
                    );
                    let n = (0.75 * rate) as usize;
                    let mut reached = 0.0_f64;
                    for positive in [true, false] {
                        let mut v = JointBound::worst_input(&terms, (b0, a1), linear, n, positive);
                        st.reset();
                        st.process(&mut [&mut v]);
                        let peak = peak_of(&v);
                        let b = db_to_lin(st.peak_gain_db());
                        assert!(peak <= b, "{rate} {cfg:?}: {peak} > {b}");
                        reached = reached.max(v[v.len() - 1].abs());
                    }
                    eprintln!(
                        "{rate} dc {dc_block}: bound {:7.2} dB (per-term {:7.2}), worst input reaches {reached:.5}",
                        st.peak_gain_db(),
                        lin_to_db(per_term)
                    );
                    if joint > 1.0 {
                        assert!(
                            reached > 0.995 * joint,
                            "{rate} {cfg:?}: {reached} of {joint}"
                        );
                    }
                }
            }
        }
    }

    /// Largest `|x|` of `v`.
    fn peak_of(v: &[f64]) -> f64 {
        v.iter().fold(0.0, |m, x| m.max(x.abs()))
    }

    #[test]
    fn dc_blocker_state_flushes_below_2_pow_minus_100() {
        for cfg in [
            custom(-30.0, -40.0, Some(-50.0), None),
            tilted(FIVE, tilt((100.0, 12.0), (2_000.0, 12.0), true)),
        ] {
            let mut st = Harmonics::try_new(&cfg, 48_000.0, 1).unwrap();
            let mut v = vec![0.9; 4096];
            // 5 Hz at 48 kHz: 2^-100 is reached after about 100 s of silence.
            v.extend(std::iter::repeat_n(0.0, 48_000 * 120));
            // In blocks, as the chain runs it (the tilt filters flush once
            // per block).
            for b in v.chunks_mut(1024) {
                st.process(&mut [b]);
            }
            assert_eq!(st.bank.dc_state[0], 0.0);
            assert_eq!(*v.last().unwrap(), 0.0);
        }
    }
}
/// Sag and bias layers in the stage ([`crate::tube_dynamics`]).
#[cfg(test)]
mod dynamic_tests {
    use super::*;

    use audivane_protocol::{HARMONICS_DEFAULT_SIGNS, HarmonicSign, HarmonicsBias, HarmonicsSag};

    const P: HarmonicSign = HarmonicSign::Positive;
    const N: HarmonicSign = HarmonicSign::Negative;

    pub(super) fn cheb(levels: [Option<f64>; 8], signs: [HarmonicSign; 8]) -> ChebyshevLayer {
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

    pub(super) fn preset_layer(id: &str) -> HarmonicsLayer {
        HarmonicsConfig::from_preset(id).unwrap().layers.remove(0)
    }

    pub(super) fn preset_cheb(id: &str) -> ChebyshevLayer {
        match preset_layer(id) {
            HarmonicsLayer::Chebyshev(l) => l,
            _ => panic!("{id} is not a Chebyshev preset"),
        }
    }

    pub(super) fn stage_of(layers: Vec<HarmonicsLayer>) -> HarmonicsConfig {
        HarmonicsConfig {
            system: None,
            enabled: true,
            layers,
            dc_block: true,
            routing: Default::default(),
        }
    }

    pub(super) fn sag(depth_db: f64, attack_ms: f64, release_ms: f64) -> Option<HarmonicsSag> {
        Some(HarmonicsSag {
            depth_db,
            attack_ms,
            release_ms,
        })
    }

    pub(super) fn bias(depth: f64, attack_ms: f64, release_ms: f64) -> Option<HarmonicsBias> {
        Some(HarmonicsBias {
            depth,
            attack_ms,
            release_ms,
        })
    }

    pub(super) fn dynamic(
        mut l: ChebyshevLayer,
        s: Option<HarmonicsSag>,
        b: Option<HarmonicsBias>,
    ) -> HarmonicsLayer {
        l.sag = s;
        l.bias = b;
        HarmonicsLayer::Chebyshev(l)
    }

    /// Curves with strong terms, high orders and mixed signs, and a preset.
    fn strong_curves() -> Vec<ChebyshevLayer> {
        vec![
            cheb([Some(-20.0); 8], [P, N, N, P, N, P, P, N]),
            cheb(
                [Some(-20.0), Some(-20.0), None, None, None, None, None, None],
                HARMONICS_DEFAULT_SIGNS,
            ),
            preset_cheb("se_triode_high_mu"),
        ]
    }

    pub(super) fn sine(rate: f64, freq: f64, amp: f64, seconds: f64) -> impl Iterator<Item = f64> {
        let n = (seconds * rate) as usize;
        (0..n).map(move |i| amp * (2.0 * PI * freq * i as f64 / rate).sin())
    }

    pub(super) fn silence(rate: f64, seconds: f64) -> impl Iterator<Item = f64> {
        std::iter::repeat_n(0.0, (seconds * rate) as usize)
    }

    /// Adversarial inputs for the bound with envelope times `(sag attack,
    /// sag release, bias attack, bias release)` in seconds (lengths capped
    /// so the 768 kHz cases stay short).
    pub(super) fn adversarial(rate: f64, (sa, sr, ba, br): (f64, f64, f64, f64)) -> Vec<Vec<f64>> {
        let mut out = Vec::new();
        // Bursts after silence.
        let mut v: Vec<f64> = silence(rate, 0.05).collect();
        for f in [1_000.0, 100.0, 5_000.0] {
            v.extend(sine(rate, f, 1.0, 0.03));
            v.extend(silence(rate, 0.03));
        }
        v.extend([1.0, -1.0, 1.0, -1.0]);
        out.push(v);
        // Sustained full scale (every envelope up), then silence (the fast
        // one falls, the slow one stays), then full-scale steps and a low
        // tone.
        let hold = (5.0 * ba.max(sa)).clamp(0.02, 0.4);
        let mut v: Vec<f64> = sine(rate, 1_000.0, 1.0, hold).collect();
        v.extend(silence(rate, (3.0 * sr.min(br)).clamp(0.001, 0.2)));
        v.extend([1.0, -1.0, 1.0]);
        v.extend(sine(rate, 40.0, 1.0, 0.05));
        v.extend(std::iter::repeat_n(-1.0, 200));
        v.extend(std::iter::repeat_n(1.0, 200));
        out.push(v);
        // Loud and quiet in turn at the envelopes' time scales.
        for (on, off) in [(sa, sr), (ba, br), (sr, sa)] {
            let (on, off) = (on.clamp(0.002, 0.1), off.clamp(0.002, 0.1));
            let mut v = Vec::new();
            for _ in 0..3 {
                v.extend(sine(rate, 700.0, 1.0, on));
                v.extend(sine(rate, 700.0, 0.03, off));
            }
            out.push(v);
        }
        // Sines 20 Hz to 20 kHz at full scale.
        let mut v = Vec::new();
        for f in [20.0, 50.0, 200.0, 1_000.0, 5_000.0, 12_000.0, 20_000.0] {
            if f < 0.45 * rate {
                v.extend(sine(rate, f, 1.0, 0.06));
            }
        }
        out.push(v);
        out
    }

    /// Process `buf` in odd block sizes.
    pub(super) fn run(stage: &mut Harmonics, buf: &mut [f64]) {
        let mut i = 0;
        let mut b = 1;
        while i < buf.len() {
            let e = (i + b).min(buf.len());
            stage.process(&mut [&mut buf[i..e]]);
            i = e;
            b = b % 1021 + 31;
        }
    }

    pub(super) fn peak(buf: &[f64]) -> f64 {
        buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()))
    }

    #[test]
    fn sag_and_bias_at_depth_zero_stay_in_the_bank_bit_for_bit() {
        let base = preset_cheb("se_triode_high_mu");
        let stat = stage_of(vec![HarmonicsLayer::Chebyshev(base.clone())]);
        let zero = stage_of(vec![dynamic(
            base,
            sag(0.0, 5.0, 50.0),
            bias(0.0, 5.0, 50.0),
        )]);
        let mut a = Harmonics::try_new(&stat, 96_000.0, 1).unwrap();
        let mut b = Harmonics::try_new(&zero, 96_000.0, 1).unwrap();
        assert_eq!(b.dynamic_layers(), 0);
        assert_eq!(a.peak_gain_db().to_bits(), b.peak_gain_db().to_bits());
        let input: Vec<f64> = sine(96_000.0, 440.0, 1.2, 0.1).collect();
        let (mut x, mut y) = (input.clone(), input);
        run(&mut a, &mut x);
        run(&mut b, &mut y);
        assert!(x.iter().zip(&y).all(|(p, q)| p.to_bits() == q.to_bits()));
        // The sound of a depth-0 layer is the static layer's.
        assert_eq!(stat.layers[0].params(), zero.layers[0].params());
    }

    #[test]
    fn output_stays_within_full_scale_with_sag_and_bias_layers() {
        // Fast envelopes (the bound must hold whatever they do) and slow ones.
        let envelopes = [
            (
                sag(6.0, 0.1, 1.0),
                bias(0.25, 0.1, 5.0),
                (1e-4, 1e-3, 1e-4, 5e-3),
            ),
            (
                sag(6.0, 0.1, 1.0),
                bias(-0.25, 0.1, 5.0),
                (1e-4, 1e-3, 1e-4, 5e-3),
            ),
            (
                sag(6.0, 20.0, 200.0),
                bias(0.25, 300.0, 2_000.0),
                (0.02, 0.2, 0.3, 2.0),
            ),
        ];
        let mut worst: f64 = 0.0;
        for l in strong_curves() {
            for (s, b, times) in &envelopes {
                let dyn_layer = dynamic(l.clone(), *s, *b);
                let mixes: Vec<(&str, Vec<HarmonicsLayer>)> = vec![
                    ("alone", vec![dyn_layer.clone()]),
                    (
                        "with a static curve",
                        vec![preset_layer("pentode"), dyn_layer.clone()],
                    ),
                    (
                        "with tilt",
                        vec![preset_layer("pp_pentode_feedback"), dyn_layer.clone()],
                    ),
                    (
                        "with flux and tape",
                        vec![
                            dyn_layer.clone(),
                            preset_layer("output_transformer"),
                            preset_layer("tape_vintage"),
                        ],
                    ),
                    (
                        "two dynamic",
                        vec![
                            dyn_layer.clone(),
                            dynamic(
                                preset_cheb("pp_triode"),
                                sag(2.0, 5.0, 50.0),
                                bias(-0.1, 1.0, 10.0),
                            ),
                        ],
                    ),
                    (
                        "three on one supply, with a static curve",
                        vec![
                            dyn_layer.clone(),
                            preset_layer("pentode"),
                            dynamic(preset_cheb("pp_triode"), sag(6.0, 0.1, 2.0), None),
                            dynamic(
                                preset_cheb("se_pentode"),
                                sag(4.0, 0.5, 3.0),
                                bias(0.2, 0.2, 4.0),
                            ),
                        ],
                    ),
                ];
                for (name, layers) in mixes {
                    for rate in [44_100.0, 768_000.0] {
                        // The slow envelopes only at 44.1 kHz (lengths).
                        if times.3 > 1.0 && rate > 100_000.0 {
                            continue;
                        }
                        let cfg = stage_of(layers.clone());
                        let st = Harmonics::try_new(&cfg, rate, 1).unwrap();
                        // Peaks as a fraction of the stage's bound.
                        let bound = db_to_lin(st.peak_gain_db());
                        for (i, mut v) in adversarial(rate, *times).into_iter().enumerate() {
                            let mut st = st.clone();
                            run(&mut st, &mut v);
                            let p = peak(&v) / bound;
                            worst = worst.max(p);
                            assert!(
                                p <= 1.0,
                                "{name} {s:?} {b:?} {rate}: signal {i} peak {p} ({:?})",
                                &l.levels_db[..3]
                            );
                        }
                    }
                }
            }
        }
        eprintln!("worst adversarial peak with sag/bias layers: {worst:.4}");
    }

    pub(super) fn tilted_cheb(
        mut l: ChebyshevLayer,
        low: (f64, f64),
        high: (f64, f64),
        even: bool,
    ) -> ChebyshevLayer {
        l.tilt = Some(Box::new(audivane_protocol::HarmonicsTilt {
            low: audivane_protocol::TiltShelf {
                hz: low.0,
                db: low.1,
                slope: None,
            },
            high: audivane_protocol::TiltShelf {
                hz: high.0,
                db: high.1,
                slope: None,
            },
            low_even: even,
            ..Default::default()
        }));
        l
    }

    /// [`adversarial`] plus sign flips after settling at full scale.
    pub(super) fn with_sign_flips(rate: f64, times: (f64, f64, f64, f64)) -> Vec<Vec<f64>> {
        let mut out = adversarial(rate, times);
        let hold = (5.0 * times.2.max(times.0)).clamp(0.02, 0.3);
        for (from, to) in [(1.0, -1.0), (-1.0, 1.0), (1.0, 0.0)] {
            let mut v = vec![from; (hold * rate) as usize];
            v.extend(std::iter::repeat_n(to, (0.01 * rate) as usize));
            v.extend(std::iter::repeat_n(from, 64));
            out.push(v);
        }
        out
    }
}

/// Separable readout tones and their cache ([`crate::harmonics_sim`]).
#[cfg(test)]
mod readout_sim_tests {
    use super::*;
    use audivane_protocol::{HarmonicsBias, HarmonicsSag};

    fn layer(id: &str) -> HarmonicsLayer {
        HarmonicsConfig::from_preset(id).unwrap().layers[0].clone()
    }

    fn stage_of(layers: Vec<HarmonicsLayer>) -> HarmonicsConfig {
        HarmonicsConfig {
            enabled: true,
            layers,
            ..Default::default()
        }
    }

    /// Preset `id`'s curve with sag and bias (tilt kept or dropped).
    fn sag_bias(id: &str, tilted: bool, sag_db: f64, bias: f64) -> HarmonicsLayer {
        let mut l = layer(id);
        if let HarmonicsLayer::Chebyshev(c) = &mut l {
            if !tilted {
                c.tilt = None;
            }
            c.sag = Some(HarmonicsSag {
                depth_db: sag_db,
                attack_ms: 20.0,
                release_ms: 200.0,
            });
            c.bias = (bias > 0.0).then_some(HarmonicsBias {
                depth: bias,
                attack_ms: 300.0,
                release_ms: 2_000.0,
            });
        }
        l
    }

    #[test]
    fn a_cached_core_builds_the_stage_bit_for_bit() {
        let cfg = stage_of(vec![
            layer("tape_vintage"),
            sag_bias("pp_pentode_feedback", true, 3.0, 0.1),
        ]);
        let run = |st: &mut Harmonics| {
            let mut l: Vec<f64> = (0..20_000)
                .map(|i| 0.9 * (i as f64 * 0.013).sin())
                .collect();
            let mut r: Vec<f64> = (0..20_000)
                .map(|i| 0.7 * (i as f64 * 0.021).cos())
                .collect();
            st.process(&mut [&mut l, &mut r]);
            (l, r)
        };
        let mut fresh = Harmonics::try_new(&cfg, 192_000.0, 2).unwrap();
        assert!(crate::harmonics_sim::cached_cores() > 0);
        let mut cached = Harmonics::try_new(&cfg, 192_000.0, 2).unwrap();
        assert_eq!(
            fresh.peak_gain_db().to_bits(),
            cached.peak_gain_db().to_bits()
        );
        assert_eq!(run(&mut fresh), run(&mut cached));
    }
}

#[path = "harmonics_live.rs"]
pub mod live;

#[cfg(test)]
#[path = "harmonics_series_tests.rs"]
mod series_tests;
