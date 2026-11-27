//! The layer interface of the harmonics stage ([`crate::harmonics`]).
//!
//! The stage is a **Chebyshev bank** plus a list of **per-layer kinds**.
//! The bank is the fast path: every static Chebyshev layer merges exactly
//! into one table of terms (tilted orders through one filter per order and
//! filter, the even sum through one DC blocker), evaluated by one Chebyshev
//! recurrence per sample. Every other layer runs on its own, as a
//! [`LayerKind`]:
//!
//! - **build** at an output rate from its protocol layer ([`build_layer`];
//!   calibration here, off the audio thread);
//! - **run** per sample on the clamped input ([`LayerKind::tick`]), with one
//!   state per channel, so the stage splits for channel workers by handing
//!   each part its channel's states; two channels in lockstep where the kind
//!   vectorizes ([`LayerKind::tick_pair`], bit for bit as two ticks). Layers
//!   of one kind run as a [`Group`] (one sample through each in turn, so
//!   their recurrences overlap); the stage dispatches once per group and
//!   block of samples, never per sample;
//! - **bound**: its path as a linear part moved onto the stage's direct
//!   path plus a certified remainder ([`Split`]), combined by
//!   [`compose_bound`];
//! - **warnings** ([`layer_warnings`]: orders that alias, a loop below its
//!   clean rate);
//! - **readouts**: its steady-state tone in closed form when it has one
//!   ([`LayerKind::steady_tone`]), else simulated, with a cost hint
//!   ([`LayerKind::readout_cost`]) that thins the readout's sweeps when
//!   several slow layers are on;
//! - **validation** ([`validate_layer`], shared with the daemon);
//! - **end of block** ([`LayerKind::end_block`]): filter states that flush
//!   once per block rather than per sample (a tilted sag/bias layer's
//!   per-order filters).
//!
//! Adding a kind: implement [`LayerKind`] for its core, add a [`LayerGroup`]
//! variant with its lines in `each_kind!`, [`LayerGroup::rank`] (where its
//! sum is added), [`LayerGroup::merge`] and [`LayerGroup::split_channels`],
//! and its arms in [`build_layer`], [`layer_warnings`] and
//! [`validate_layer`]. A Chebyshev layer with sag or bias
//! ([`crate::tube_dynamics`]: its operating point moves per sample) leaves
//! the bank and runs as such a kind, [`DynamicsCore`]: its path is its
//! curve minus the dry signal the bank already carries.
//!
//! # Bound
//!
//! Every part of the stage is written, for inputs within full scale, as
//! `p[n] = −g x[n] + ρ[n]` with `|ρ[n]| <= r`. Moving every `g` onto the
//! bank's direct path, the stage output is the bank with
//! direct gain `L − Σ g` plus every `ρ`, so
//!
//! ```text
//! B = B_bank(L − Σ_j g_j) + Σ_j r_j + T,
//! ```
//!
//! with `B_bank(g)` the bank's certified bound with direct gain `g` (its
//! unfiltered terms and DC blocker) and `T` its filtered terms, bounded on
//! their own. A kind offers its splits in one of two forms:
//!
//! - [`Split::Fixed`] `{ linear: g, delayed: q, residual: R }`:
//!   `p[n] = −g x[n] − q x[n−1] + ρ'[n]` with `|ρ'| <= R`, so `r = |q| + R`.
//!   A flux layer's certified split is this with `g = q = λ/2`.
//! - [`Split::Between`] `{ g0, r0, g1, r1 }`: the segment between any two
//!   certified splits `(g0, r0)` and `(g1, r1)` of one path, its `θ = 0` end
//!   anywhere: for every `θ ∈ [0, 1]`,
//!   `p[n] = −((1 − θ) g0 + θ g1) x[n] + ρ_θ[n]` with
//!   `|ρ_θ| <= (1 − θ) r0 + θ r1`. The stage treats it as the fixed split
//!   `(g0, r0)` plus `θ` of the gain `g1 − g0` at `(r1 − r0)/(g1 − g0)` per
//!   unit (its ends ordered so that `g1 >= g0`).
//!
//! A hysteresis layer is the segment `{0, Y, 1, Q}`: its runtime holds its
//! path within `|p| <= Y` and `|x_c + p| <= Q` ([`crate::hysteresis`],
//! "Peak bound"), the splits `(0, Y)` and `(1, Q)`.
//!
//! **Sag and bias layers** ([`DynamicsCore`]) are the other case for the
//! second form. Layer `j`'s core gives `y_j = L_j d_j x + δ_j` with its sag's
//! drive `d_j` in `[d_min,j, 1]` (`d_min,j = 10^(−D_j/20)`), its pure
//! distortion at that drive `|δ_j| <= R_j`, and `|y_j| <= B_j` (the core's
//! certified bounds over `|u| = d_j |x| <= 1`, every bias point `b` the
//! clamped envelope reaches, `[0, reach]`, and any DC-blocker past: every
//! envelope value the follower can hold, since both envelopes stay in
//! `[0, 1]`).
//!
//! **Shared supply.** The stage's sag/bias layers sit on one supply
//! ([`Supply`]): each curve's distortion sees its own drive, but the dry
//! signal and their linear parts are turned down once, by the deepest sag,
//! `D = min_j d_j` (in `[D_min, 1]`, `D_min = min_j d_min,j`), never summed.
//! With `Λ = 1 + Σ_j (L_j − 1)` the supply's path (added to the bank, which
//! carries the dry signal and the static layers) is
//!
//! ```text
//! p = Σ_j δ_j(d_j x) + (D Λ − 1) x,
//! ```
//!
//! so the stage's direct gain is `(L_bank − 1) + D Λ`: one turn-down, by
//! the deepest sag, of the dry signal and the sag layers' linear parts (the
//! static layers' linear parts are not on the supply). One layer gives
//! `p = y − x`, the layer's curve minus the dry signal (equal in exact
//! arithmetic; the evaluation order differs, so samples can differ from the
//! single-layer form by an ulp). For every sample,
//! whatever the envelopes do, `D Λ` lies in `[G_lo, G_hi]` (the ends
//! `D_min Λ` and `Λ`, sorted), so with `Ḡ = (G_lo + G_hi)/2` and
//! `ΔG = G_hi − G_lo`
//!
//! ```text
//! p = −(1 − Ḡ)·x + ((D Λ − Ḡ) x + Σ δ_j)        (g0, r0) = (1 − Ḡ, Σ R_j + ΔG/2)
//! p = −1·x + (Σ_j y_j + c x)                     (g1, r1) = (1, Σ B_j + C)
//! ```
//!
//! with `c = D Λ − Σ L_j d_j = Σ L_j (D − d_j) − (N − 1) D`: `D − d_j` lies
//! in `[D_min − 1, 0]` and `D` in `[0, 1]`, so `|c| <= C =
//! Σ |L_j| (1 − D_min) + (N − 1)`. With one layer `D = d`, so `c = 0`,
//! `C = 0` and the far end is `(1, B)`: the curve's joint bound, which keeps
//! its cancellations (alone in the stage the composed bound is exactly `B`,
//! the standalone [`crate::tube_dynamics`] bound). Both ends hold at the same
//! sample for the same `x`, so their convex combination does too: the whole
//! segment is certified. `g0` is not 0 (the supply's direct gain is not the
//! dry signal's); the `(1 − Ḡ, …)` end leaves the direct gain on the bank,
//! where it can cancel against the bank's own terms when a static layer
//! shares the stage. With several sag layers the far end's `C` is loose and
//! the near end usually wins.
//!
//! The stage picks `θ` per segment greedily, in increasing
//! `(r1 − r0)/(g1 − g0)` (the remainder each unit of gain moved costs),
//! until the direct gain left, `L − Σ g_fixed` (`g0` counted as fixed),
//! reaches zero, and keeps the smaller of that bound and the one with every
//! `θ = 0`. Any `θ` is certified, so the choice only decides how tight the
//! bound is:
//!
//! ```text
//! B = min over {greedy θ, θ = 0} of
//!       B_bank(L − Σ_fixed g_j − Σ_seg θ_i (g1,i − g0,i))
//!     + Σ_fixed (|q_j| + R_j) + Σ_seg (r0,i + θ_i (r1,i − r0,i))
//!   + T
//! ```
//!
//! times `1 +` the largest margin of the parts ([`LayerKind::margin`]).
//!
//! **Each kind's split.** With flux layers (`g = q = λ_j/2`,
//! `R_j`), hysteresis layers (`{0, Y_i, 1, Q_i}`) and the bank's filtered
//! terms (`T = Σ T_t`) the formula is term for term the one in the
//! [`crate::harmonics`] docs,
//! `B_cheb(L − Σλ/2 − Σθ) + Σ(λ/2 + R) + Σ((1 − θ)Y + θQ) + ΣT_t`, with
//! margins 1 ppm for hysteresis and 1e-12 for flux, sag/bias layers and
//! filtered terms (0 for the bank alone). [`compose_bound`] adds the terms in
//! a fixed order (fixed splits in config order, then segments in the greedy
//! order; `T` last), so the bounds of stages without hysteresis are
//! bit-identical (`tests/harmonics_identity.rs`); a [`Split::Between`] only
//! adds terms where one is present.

use crate::flux::{ChannelState as FluxState, FluxConfig, FluxCore};
use crate::harmonics::{HarmonicsError, ToneSpectrum, check_chebyshev};
use crate::harmonics_sim::{MAX_THREADS, SimUnit, cached_core, par_map};
use crate::hysteresis::{
    ChannelState as HysteresisState, HYSTERESIS_MIN_CLEAN_RATE_HZ, HysteresisConfig, HysteresisCore,
};
use crate::tube_dynamics::{DynamicsCore, DynamicsState, TubeDynamicsConfig};
use audivane_protocol::{
    ALLOWED_RATES, HARMONICS_ORDERS, HIGH_OUTPUT_RATES, HarmonicsConfig, HarmonicsLayer,
};
use std::fmt;

/// Top of the audio band, for the order cap ([`layer_warnings`]).
pub(crate) const AUDIO_BAND_HZ: f64 = 20_000.0;
/// Rate the daemon calibrates flux layers at when it validates them
/// ([`validate_layer`]).
const VALIDATION_RATE_HZ: f64 = 96_000.0;

/// One layer's contribution to the stage bound: its path as a linear part
/// on the direct path plus a certified remainder, for inputs within full
/// scale (module docs, "Bound").
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Split {
    /// `p[n] = −linear·x[n] − delayed·x[n−1] + ρ[n]`, `|ρ[n]| <= residual`.
    Fixed {
        linear: f64,
        delayed: f64,
        residual: f64,
    },
    /// For every `θ ∈ [0, 1]`: `p[n] = −((1 − θ)·g0 + θ·g1)·x[n] + ρ_θ[n]`,
    /// `|ρ_θ[n]| <= (1 − θ)·r0 + θ·r1` (two certified splits of one path and
    /// the segment between them).
    Between { g0: f64, r0: f64, g1: f64, r1: f64 },
}

/// A segment of gain the stage may move onto the bank's direct path: a
/// [`Split::Between`] past its fixed end, gain `g1 − g0 > 0` at remainder
/// `r1 − r0` for all of it.
#[derive(Clone, Copy)]
struct Movable {
    gain: f64,
    rem: f64,
}

impl Movable {
    /// Remainder per unit of gain moved.
    fn cost(&self) -> f64 {
        self.rem / self.gain
    }
}

/// The stage's certified bound before its margin, from the bank's direct
/// gain `linear`, its bound `bank(g)` at direct gain `g`, its filtered
/// terms' bound `own`, and the layers' splits (module docs, "Bound").
pub(crate) fn compose_bound(
    linear: f64,
    bank: impl Fn(f64) -> f64,
    own: f64,
    splits: impl IntoIterator<Item = Split>,
) -> f64 {
    let mut fixed_linear = 0.0;
    let mut fixed_rem = 0.0;
    let mut segments: Vec<Movable> = Vec::new();
    for split in splits {
        match split {
            Split::Fixed {
                linear,
                delayed,
                residual,
            } => {
                fixed_linear += linear;
                fixed_rem = fixed_rem + delayed.abs() + residual;
            }
            Split::Between { g0, r0, g1, r1 } => {
                // Ordered so that moving along it lowers the direct gain.
                let ((g0, r0), (g1, r1)) = if g1 >= g0 {
                    ((g0, r0), (g1, r1))
                } else {
                    ((g1, r1), (g0, r0))
                };
                fixed_linear += g0;
                if g1 > g0 {
                    fixed_rem += r0;
                    segments.push(Movable {
                        gain: g1 - g0,
                        rem: r1 - r0,
                    });
                } else {
                    fixed_rem += r0.min(r1);
                }
            }
        }
    }
    let direct = linear - fixed_linear;
    let parts = if segments.is_empty() {
        bank(direct) + fixed_rem
    } else {
        // Cheapest gain first; stop where the direct gain reaches 0.
        segments.sort_by(|x, y| x.cost().total_cmp(&y.cost()));
        let mut left = direct.max(0.0);
        let mut moved = 0.0;
        let mut split_rem = 0.0;
        for seg in &segments {
            let theta = (left / seg.gain).min(1.0);
            left -= theta * seg.gain;
            moved += theta * seg.gain;
            // Its θ = 0 end is in the fixed remainder already.
            split_rem += theta * seg.rem;
        }
        let split = bank(direct - moved) + fixed_rem + split_rem;
        let unsplit = bank(direct) + fixed_rem;
        split.min(unsplit)
    };
    parts + own
}

/// A per-layer kind of the harmonics stage, built at an output rate (module
/// docs). Its path carries no overs: the stage passes them.
pub(crate) trait LayerKind: Clone + fmt::Debug {
    /// Per-channel state.
    type State: Copy + Default + fmt::Debug;

    /// Whether [`Self::tick_pair`] vectorizes: the stage then runs channel
    /// pairs in lockstep (else one channel after the other).
    const PAIRED: bool = false;

    /// One sample of the path for the clamped input `xc` (`|xc| <= 1`).
    fn tick(&self, st: &mut Self::State, xc: f64) -> f64;

    /// Two channels in lockstep, bit for bit as two [`Self::tick`]s (a kind
    /// overrides it where the two lanes vectorize).
    #[inline(always)]
    fn tick_pair(
        &self,
        st0: &mut Self::State,
        st1: &mut Self::State,
        xc0: f64,
        xc1: f64,
    ) -> (f64, f64) {
        (self.tick(st0, xc0), self.tick(st1, xc1))
    }

    /// Its contribution to the stage bound.
    fn split(&self) -> Split;

    /// Relative margin its certification needs on the stage bound
    /// (discretization, rounding in its filters).
    fn margin(&self) -> f64;

    /// Steady-state complex amplitudes `[DC, H1, …, H15]` (cosine phase) of
    /// its path for `amp · cos(2π f n / rate)`;
    /// `None` when it has no closed form (the readout simulates it).
    fn steady_tone(&self, rate: f64, freq: f64, amp: f64) -> Option<ToneSpectrum>;

    /// Cost of simulating it in a readout at `rate`, in tape layers (0:
    /// closed form; a tape layer without substeps is 1).
    fn readout_cost(&self, rate: f64) -> f64;

    /// Settling a simulated readout tone needs for it, seconds, after
    /// [`Self::presettle`].
    fn readout_settle_s(&self) -> f64 {
        0.0
    }

    /// Put the slow part of `st` (envelopes) where the clamped input `xc`,
    /// looped forever, would leave it at the loop's start, so a simulated
    /// steady tone needs no settling for it. Off the audio thread.
    fn presettle(&self, _st: &mut Self::State, _xc: &[f64]) {}

    /// `(depth, reach)` when the bound cut its bias reach short of the
    /// depth (a warning).
    fn bias_limit(&self) -> Option<(f64, f64)> {
        None
    }

    /// Its path's gain for small signals with its state at rest (flux and
    /// hysteresis carry no linear part: 0).
    fn rest_gain(&self) -> f64 {
        0.0
    }

    /// End of a block on one channel's state: flush decaying filter tails
    /// before they reach subnormals (a kind whose filters flush per sample
    /// leaves it empty).
    #[inline]
    fn end_block(&self, _st: &mut Self::State) {}
}

impl LayerKind for FluxCore {
    type State = FluxState;

    #[inline(always)]
    fn tick(&self, st: &mut FluxState, xc: f64) -> f64 {
        FluxCore::tick(self, st, xc)
    }

    fn split(&self) -> Split {
        // −(λ/2)(x[n] + x[n−1]) plus a residual within R.
        let (lambda, residual) = FluxCore::split(self);
        Split::Fixed {
            linear: 0.5 * lambda,
            delayed: 0.5 * lambda,
            residual,
        }
    }

    fn margin(&self) -> f64 {
        // Rounding in the shelves (as the flux stage).
        1e-12
    }

    fn steady_tone(&self, rate: f64, freq: f64, amp: f64) -> Option<ToneSpectrum> {
        Some(self.tone_harmonics(freq, rate, amp))
    }

    fn readout_cost(&self, _rate: f64) -> f64 {
        0.0
    }
}

impl LayerKind for HysteresisCore {
    type State = HysteresisState;
    const PAIRED: bool = true;

    #[inline(always)]
    fn tick(&self, st: &mut HysteresisState, xc: f64) -> f64 {
        HysteresisCore::tick(self, st, xc)
    }

    #[inline(always)]
    fn tick_pair(
        &self,
        st0: &mut HysteresisState,
        st1: &mut HysteresisState,
        xc0: f64,
        xc1: f64,
    ) -> (f64, f64) {
        // Two independent ODE chains in 2-lane packed instructions.
        HysteresisCore::tick_pair(self, st0, st1, xc0, xc1)
    }

    fn split(&self) -> Split {
        HysteresisCore::split(self)
    }

    fn margin(&self) -> f64 {
        // RK4 discretization and rounding (as the hysteresis stage).
        1e-6
    }

    fn steady_tone(&self, _rate: f64, _freq: f64, _amp: f64) -> Option<ToneSpectrum> {
        None
    }

    fn readout_cost(&self, rate: f64) -> f64 {
        // One RK4 step per sample, plus the substeps of big jumps (measured:
        // the step dominates, `tape_vintage` costs ~1.2× `tape_master` on
        // the readout tones and ~2.6× on music-level noise).
        self.readout_substeps(rate)
    }

    fn readout_settle_s(&self) -> f64 {
        // Its DC blocker and magnetization (the readout's default).
        crate::harmonics::READOUT_SETTLE_SECONDS
    }
}

impl LayerKind for DynamicsCore {
    type State = DynamicsState;

    /// The curve with sag and bias minus the dry signal the bank carries,
    /// for the layer alone on its supply (the stage runs its sag layers
    /// together, [`Supply`]).
    #[inline(always)]
    fn tick(&self, st: &mut DynamicsState, xc: f64) -> f64 {
        DynamicsCore::tick(self, st, xc) - xc
    }

    /// Its split alone on its supply ([`Supply::split`] with one layer).
    fn split(&self) -> Split {
        let (lo, hi, rem) = DynamicsCore::split(self);
        let mid = 0.5 * (lo + hi);
        Split::Between {
            g0: 1.0 - mid,
            r0: rem + 0.5 * (hi - lo),
            g1: 1.0,
            r1: self.peak_bound(),
        }
    }

    fn margin(&self) -> f64 {
        // Rounding in the difference with the dry signal (its own bounds
        // carry their own margin).
        1e-12
    }

    fn steady_tone(&self, _rate: f64, _freq: f64, _amp: f64) -> Option<ToneSpectrum> {
        // The envelopes ripple with the tone: simulated.
        None
    }

    fn readout_cost(&self, _rate: f64) -> f64 {
        crate::tube_dynamics::READOUT_COST * self.cost_factor()
    }

    fn readout_settle_s(&self) -> f64 {
        // Its envelopes are presettled; its DC blocker settles as a tape
        // layer's.
        crate::harmonics::READOUT_SETTLE_SECONDS
    }

    fn presettle(&self, st: &mut DynamicsState, xc: &[f64]) {
        DynamicsCore::presettle(self, st, xc);
    }

    fn bias_limit(&self) -> Option<(f64, f64)> {
        DynamicsCore::bias_limit(self)
    }

    fn rest_gain(&self) -> f64 {
        // `L − 1`: the drive is 1 and the bias point 0 at rest.
        self.small_signal() - 1.0
    }

    #[inline]
    fn end_block(&self, st: &mut DynamicsState) {
        // The filtered orders' blockers and tilt filters.
        DynamicsCore::end_block(self, st);
    }
}

/// The layers of one kind with their per-channel states: run together, their
/// sum added to the stage output at once.
#[derive(Clone, Debug)]
pub(crate) struct Group<K: LayerKind> {
    cores: Vec<K>,
    /// Each core's index in the config's layers.
    indices: Vec<usize>,
    /// `channels × cores.len()`, channel-major.
    states: Vec<K::State>,
    channels: usize,
}

impl<K: LayerKind> Group<K> {
    fn new(core: K, index: usize, channels: usize) -> Self {
        Self {
            cores: vec![core],
            indices: vec![index],
            states: vec![K::State::default(); channels],
            channels,
        }
    }

    /// Add `other`'s layers after this one's (at build: fresh states).
    fn append(&mut self, mut other: Self) {
        self.cores.append(&mut other.cores);
        self.indices.append(&mut other.indices);
        self.states = vec![K::State::default(); self.channels * self.cores.len()];
    }

    /// `sum[i] = Σ_j p_j(xc[i])` on channel `c`, layers in order.
    #[inline(always)]
    fn run(&mut self, c: usize, xc: &[f64], sum: &mut [f64]) {
        let n = self.cores.len();
        let Some(st) = self.states.get_mut(c * n..(c + 1) * n) else {
            return;
        };
        if let ([core], [st]) = (&self.cores[..], &mut *st) {
            // One layer: its state stays in registers.
            let mut s = *st;
            for (y, &x) in sum.iter_mut().zip(xc) {
                let mut h = 0.0;
                h += core.tick(&mut s, x);
                *y = h;
            }
            *st = s;
        } else {
            // Several: one sample through every layer (their recurrences
            // overlap).
            for (y, &x) in sum.iter_mut().zip(xc) {
                let mut h = 0.0;
                for (core, s) in self.cores.iter().zip(st.iter_mut()) {
                    h += core.tick(s, x);
                }
                *y = h;
            }
        }
    }

    /// [`Self::run`] on channels `c` and `c + 1` in lockstep.
    #[inline(always)]
    fn run_pair(&mut self, c: usize, xc: [&[f64]; 2], sum: [&mut [f64]; 2]) {
        let n = self.cores.len();
        let Some(st) = self.states.get_mut(c * n..(c + 2) * n) else {
            return;
        };
        let (st0, st1) = st.split_at_mut(n);
        let [x0, x1] = xc;
        let [y0, y1] = sum;
        let lanes = y0.iter_mut().zip(y1.iter_mut()).zip(x0).zip(x1);
        if let ([core], [st0], [st1]) = (&self.cores[..], &mut *st0, &mut *st1) {
            let (mut s0, mut s1) = (*st0, *st1);
            for (((y0, y1), &x0), &x1) in lanes {
                let (mut h0, mut h1) = (0.0, 0.0);
                let (p0, p1) = core.tick_pair(&mut s0, &mut s1, x0, x1);
                h0 += p0;
                h1 += p1;
                *y0 = h0;
                *y1 = h1;
            }
            *st0 = s0;
            *st1 = s1;
        } else {
            for (((y0, y1), &x0), &x1) in lanes {
                let (mut h0, mut h1) = (0.0, 0.0);
                for ((core, s0), s1) in self.cores.iter().zip(st0.iter_mut()).zip(st1.iter_mut()) {
                    let (p0, p1) = core.tick_pair(s0, s1, x0, x1);
                    h0 += p0;
                    h1 += p1;
                }
                *y0 = h0;
                *y1 = h1;
            }
        }
    }

    fn reset(&mut self) {
        self.states.fill(K::State::default());
    }

    /// [`LayerKind::end_block`] of every layer on channel `c`.
    #[inline]
    fn end_block(&mut self, c: usize) {
        let n = self.cores.len();
        if let Some(st) = self.states.get_mut(c * n..(c + 1) * n) {
            for (core, s) in self.cores.iter().zip(st) {
                core.end_block(s);
            }
        }
    }

    /// [`LayerKind::presettle`] of every layer on channel `c`.
    fn presettle(&mut self, c: usize, xc: &[f64]) {
        let n = self.cores.len();
        if let Some(st) = self.states.get_mut(c * n..(c + 1) * n) {
            for (core, s) in self.cores.iter().zip(st) {
                core.presettle(s, xc);
            }
        }
    }

    /// One single-channel group per channel, from its states; `self` is
    /// left without states.
    fn split_channels(&mut self) -> Vec<Self> {
        let n = self.cores.len();
        let states = std::mem::take(&mut self.states);
        (0..self.channels)
            .map(|c| Self {
                cores: self.cores.clone(),
                indices: self.indices.clone(),
                states: states.get(c * n..(c + 1) * n).unwrap_or_default().to_vec(),
                channels: 1,
            })
            .collect()
    }
}

/// The stage's sag/bias layers on one shared supply (module docs, "Shared
/// supply"): each curve's distortion at its own drive, and the dry signal
/// with their linear parts turned down once, by the deepest sag.
#[derive(Clone, Debug)]
pub(crate) struct Supply {
    cores: Vec<DynamicsCore>,
    /// Each core's index in the config's layers.
    indices: Vec<usize>,
    /// `channels × cores.len()`, channel-major.
    states: Vec<DynamicsState>,
    channels: usize,
    /// `Λ = 1 + Σ (L_i − 1)` (exactly `L` with one layer): the direct gain
    /// the supply turns down.
    lambda: f64,
}

impl Supply {
    fn new(core: DynamicsCore, index: usize, channels: usize) -> Self {
        Self {
            lambda: core.small_signal(),
            cores: vec![core],
            indices: vec![index],
            states: vec![DynamicsState::default(); channels],
            channels,
        }
    }

    /// Add `other`'s layers after this one's (at build: fresh states).
    fn append(&mut self, mut other: Self) {
        self.cores.append(&mut other.cores);
        self.indices.append(&mut other.indices);
        self.states = vec![DynamicsState::default(); self.channels * self.cores.len()];
        let (first, rest) = self.cores.split_first().expect("a supply has a layer");
        self.lambda = rest
            .iter()
            .fold(first.small_signal(), |l, c| l + (c.small_signal() - 1.0));
    }

    /// `sum[i]` = the supply's path for `xc[i]` on channel `c`:
    /// `Σ δ_j(d_j x) + (D Λ − 1) x` with `D = min_j d_j`.
    #[inline(always)]
    fn run(&mut self, c: usize, xc: &[f64], sum: &mut [f64]) {
        let n = self.cores.len();
        let Some(st) = self.states.get_mut(c * n..(c + 1) * n) else {
            return;
        };
        let lambda = self.lambda;
        if let ([core], [st]) = (&self.cores[..], &mut *st) {
            // The tilt picked once per block, so an untilted core's state
            // stays in registers.
            if core.tilted() {
                run_one::<true>(core, st, lambda, xc, sum);
            } else {
                run_one::<false>(core, st, lambda, xc, sum);
            }
        } else {
            for (y, &x) in sum.iter_mut().zip(xc) {
                let (mut h, mut drive) = (0.0, 1.0_f64);
                for (core, s) in self.cores.iter().zip(st.iter_mut()) {
                    let (delta, d) = core.tick_parts(s, x);
                    h += delta;
                    drive = drive.min(d);
                }
                *y = h + (drive * lambda - 1.0) * x;
            }
        }
    }

    /// [`Self::run`] on channels `c` and `c + 1` (one after the other: the
    /// kind does not vectorize).
    #[inline(always)]
    fn run_pair(&mut self, c: usize, xc: [&[f64]; 2], sum: [&mut [f64]; 2]) {
        let [x0, x1] = xc;
        let [y0, y1] = sum;
        self.run(c, x0, y0);
        self.run(c + 1, x1, y1);
    }

    fn reset(&mut self) {
        self.states.fill(DynamicsState::default());
    }

    /// [`DynamicsCore::end_block`] of every layer on channel `c`.
    #[inline]
    fn end_block(&mut self, c: usize) {
        let n = self.cores.len();
        if let Some(st) = self.states.get_mut(c * n..(c + 1) * n) {
            for (core, s) in self.cores.iter().zip(st) {
                core.end_block(s);
            }
        }
    }

    fn presettle(&mut self, c: usize, xc: &[f64]) {
        let n = self.cores.len();
        if let Some(st) = self.states.get_mut(c * n..(c + 1) * n) {
            for (core, s) in self.cores.iter().zip(st) {
                core.presettle(s, xc);
            }
        }
    }

    /// One single-channel supply per channel, from its states.
    fn split_channels(&mut self) -> Vec<Self> {
        let n = self.cores.len();
        let states = std::mem::take(&mut self.states);
        (0..self.channels)
            .map(|c| Self {
                cores: self.cores.clone(),
                indices: self.indices.clone(),
                states: states.get(c * n..(c + 1) * n).unwrap_or_default().to_vec(),
                channels: 1,
                lambda: self.lambda,
            })
            .collect()
    }

    /// Its contribution to the stage bound (module docs, "Shared supply").
    fn split(&self) -> Split {
        let n = self.cores.len();
        let d_min = self
            .cores
            .iter()
            .map(DynamicsCore::min_drive)
            .fold(1.0, f64::min);
        let (a, b) = (d_min * self.lambda, self.lambda);
        let (lo, hi) = (a.min(b), a.max(b));
        let mid = 0.5 * (lo + hi);
        if let [core] = &self.cores[..] {
            // One layer: exactly its own split (its joint bound B).
            return LayerKind::split(core);
        }
        let rem: f64 = self.cores.iter().map(DynamicsCore::remainder_bound).sum();
        let joint: f64 = self.cores.iter().map(DynamicsCore::peak_bound).sum::<f64>()
            + self
                .cores
                .iter()
                .map(|c| c.small_signal().abs() * (1.0 - d_min))
                .sum::<f64>()
            + (n - 1) as f64;
        Split::Between {
            g0: 1.0 - mid,
            r0: rem + 0.5 * (hi - lo),
            g1: 1.0,
            r1: joint,
        }
    }
}

/// [`Supply::run`] with one layer, its tilt known.
#[inline(always)]
fn run_one<const TILTED: bool>(
    core: &DynamicsCore,
    st: &mut DynamicsState,
    lambda: f64,
    xc: &[f64],
    sum: &mut [f64],
) {
    let mut s = *st;
    for (y, &x) in sum.iter_mut().zip(xc) {
        let (delta, d) = core.tick_parts_as::<TILTED>(&mut s, x);
        *y = delta + (d * lambda - 1.0) * x;
    }
    *st = s;
}

/// The layers of one kind, of any kind.
#[derive(Clone, Debug)]
pub(crate) enum LayerGroup {
    Flux(Group<FluxCore>),
    Hysteresis(Group<HysteresisCore>),
    /// Chebyshev layers with sag or bias, on one shared supply.
    Dynamic(Supply),
}

/// `$body` with `$group` bound to the [`Group`] of any [`LayerGroup`].
macro_rules! each_kind {
    ($layers:expr, $group:ident => $body:expr) => {
        match $layers {
            LayerGroup::Flux($group) => $body,
            LayerGroup::Hysteresis($group) => $body,
            LayerGroup::Dynamic($group) => $body,
        }
    };
}

impl LayerGroup {
    /// Groups are added to the stage output in increasing rank: flux, then
    /// hysteresis, then sag/bias layers.
    pub(crate) fn rank(&self) -> u8 {
        match self {
            LayerGroup::Flux(_) => 0,
            LayerGroup::Hysteresis(_) => 1,
            LayerGroup::Dynamic(_) => 2,
        }
    }

    /// Append `other`'s layers when it is the same kind (at build), else
    /// hand it back.
    pub(crate) fn merge(&mut self, other: LayerGroup) -> Option<LayerGroup> {
        match (self, other) {
            (LayerGroup::Flux(a), LayerGroup::Flux(b)) => a.append(b),
            (LayerGroup::Hysteresis(a), LayerGroup::Hysteresis(b)) => a.append(b),
            (LayerGroup::Dynamic(a), LayerGroup::Dynamic(b)) => a.append(b),
            (_, other) => return Some(other),
        }
        None
    }

    /// Whether its kind runs channel pairs in lockstep.
    pub(crate) fn paired(&self) -> bool {
        fn paired<K: LayerKind>(_: &Group<K>) -> bool {
            K::PAIRED
        }
        match self {
            LayerGroup::Flux(g) => paired(g),
            LayerGroup::Hysteresis(g) => paired(g),
            LayerGroup::Dynamic(_) => false,
        }
    }

    /// Number of layers.
    pub(crate) fn len(&self) -> usize {
        each_kind!(self, g => g.cores.len())
    }

    #[inline]
    pub(crate) fn run(&mut self, c: usize, xc: &[f64], sum: &mut [f64]) {
        each_kind!(self, g => g.run(c, xc, sum))
    }

    #[inline]
    pub(crate) fn run_pair(&mut self, c: usize, xc: [&[f64]; 2], sum: [&mut [f64]; 2]) {
        each_kind!(self, g => g.run_pair(c, xc, sum))
    }

    /// The layers' splits, in order.
    pub(crate) fn splits(&self) -> Vec<Split> {
        match self {
            LayerGroup::Flux(g) => g.cores.iter().map(LayerKind::split).collect(),
            LayerGroup::Hysteresis(g) => g.cores.iter().map(LayerKind::split).collect(),
            // One split for the whole supply.
            LayerGroup::Dynamic(g) => vec![g.split()],
        }
    }

    /// The largest margin of its layers.
    pub(crate) fn margin(&self) -> f64 {
        each_kind!(self, g => g.cores.iter().map(LayerKind::margin).fold(0.0, f64::max))
    }

    /// Add each layer's closed-form tone to `y` (a layer without one adds
    /// nothing: [`Self::readout_cost`] says to simulate).
    pub(crate) fn add_steady_tone(&self, y: &mut ToneSpectrum, rate: f64, freq: f64, amp: f64) {
        each_kind!(self, g => {
            for tone in g.cores.iter().filter_map(|c| c.steady_tone(rate, freq, amp)) {
                for (ym, f) in y.iter_mut().zip(tone) {
                    *ym += f;
                }
            }
        })
    }

    /// Cost of simulating its layers in a readout at `rate`, in tape layers.
    pub(crate) fn readout_cost(&self, rate: f64) -> f64 {
        each_kind!(self, g => g.cores.iter().map(|c| c.readout_cost(rate)).sum())
    }

    /// Whether a layer has no closed-form tone (the readout simulates it).
    pub(crate) fn simulated(&self) -> bool {
        self.readout_cost(crate::harmonics::READOUT_MAX_SIM_RATE) > 0.0
    }

    /// The longest settle its layers need in a simulated readout tone.
    pub(crate) fn readout_settle_s(&self) -> f64 {
        each_kind!(self, g => g.cores.iter().map(LayerKind::readout_settle_s).fold(0.0, f64::max))
    }

    /// [`LayerKind::presettle`] of its layers on channel `c`.
    pub(crate) fn presettle(&mut self, c: usize, xc: &[f64]) {
        each_kind!(self, g => g.presettle(c, xc))
    }

    /// `(config index, depth, reach)` of each layer whose bias reach was cut.
    pub(crate) fn bias_limits(&self) -> Vec<(usize, f64, f64)> {
        each_kind!(self, g => g
            .cores
            .iter()
            .zip(&g.indices)
            .filter_map(|(c, &i)| c.bias_limit().map(|(d, r)| (i, d, r)))
            .collect())
    }

    /// `f(config index, drive d, operating point b)` for each sag/bias
    /// layer on channel `c` now (other kinds have none). A read: no
    /// allocation, nothing steps.
    #[inline]
    pub(crate) fn live(&self, c: usize, mut f: impl FnMut(usize, f64, f64)) {
        let LayerGroup::Dynamic(g) = self else {
            return;
        };
        let n = g.cores.len();
        if let Some(st) = g.states.get(c * n..(c + 1) * n) {
            for ((core, s), &i) in g.cores.iter().zip(st).zip(&g.indices) {
                let (d, b) = core.operating_point(s);
                f(i, d, b);
            }
        }
    }

    /// The sum of its layers' [`LayerKind::rest_gain`].
    pub(crate) fn rest_gain(&self) -> f64 {
        each_kind!(self, g => g.cores.iter().map(LayerKind::rest_gain).sum())
    }

    pub(crate) fn reset(&mut self) {
        each_kind!(self, g => g.reset())
    }

    /// End of a block on channel `c` ([`LayerKind::end_block`]).
    #[inline]
    pub(crate) fn end_block(&mut self, c: usize) {
        each_kind!(self, g => g.end_block(c))
    }

    /// One single-channel group per channel.
    pub(crate) fn split_channels(&mut self) -> Vec<LayerGroup> {
        match self {
            LayerGroup::Flux(g) => g
                .split_channels()
                .into_iter()
                .map(LayerGroup::Flux)
                .collect(),
            LayerGroup::Hysteresis(g) => g
                .split_channels()
                .into_iter()
                .map(LayerGroup::Hysteresis)
                .collect(),
            LayerGroup::Dynamic(g) => g
                .split_channels()
                .into_iter()
                .map(LayerGroup::Dynamic)
                .collect(),
        }
    }
}

/// A slow layer's built core: what its build calibrates and probes (cached
/// across builds, [`crate::harmonics_sim`]).
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // Off the audio thread, a few dozen at most.
pub(crate) enum SlowCore {
    Hysteresis(HysteresisCore),
    Dynamics(DynamicsCore),
}

impl LayerGroup {
    /// Its parts a readout simulates on their own, single channel and at
    /// rest ([`crate::harmonics_sim`], "Separable tones"): each tape layer
    /// alone, and the sag/bias supply whole (its layers share one drive).
    /// Flux layers have a closed form: none.
    pub(crate) fn sim_units(&self) -> Vec<SimUnit> {
        match self {
            LayerGroup::Flux(_) => Vec::new(),
            LayerGroup::Hysteresis(g) => g
                .cores
                .iter()
                .zip(&g.indices)
                .map(|(&core, &i)| SimUnit {
                    indices: vec![i],
                    group: LayerGroup::Hysteresis(Group::new(core, i, 1)),
                })
                .collect(),
            LayerGroup::Dynamic(g) => vec![SimUnit {
                indices: g.indices.clone(),
                group: LayerGroup::Dynamic(Supply {
                    cores: g.cores.clone(),
                    indices: g.indices.clone(),
                    states: vec![DynamicsState::default(); g.cores.len()],
                    channels: 1,
                    lambda: g.lambda,
                }),
            }],
        }
    }

    /// Its path alone for the clamped input `xc` on
    /// channel 0, from rest, with its slow state presettled on `xc[..period]`
    /// looped (`period == 0`: not presettled), run in one block as the stage
    /// runs one `process` call.
    pub(crate) fn path(&self, xc: &[f64], period: usize) -> Vec<f64> {
        let mut g = self.clone();
        g.reset();
        if period > 0 {
            g.presettle(0, &xc[..period.min(xc.len())]);
        }
        let mut out = vec![0.0; xc.len()];
        g.run(0, xc, &mut out);
        g.end_block(0);
        out
    }
}

/// Build every slow layer's core of `config` (tape, sag/bias) at each of
/// `rates` on the worker threads, so the stage builds that follow find them
/// in the core cache. Errors are left for those builds to report.
pub(crate) fn prebuild_slow_cores(config: &HarmonicsConfig, rates: &[f64]) {
    let mut jobs: Vec<(usize, f64)> = Vec::new();
    for (i, layer) in config.layers.iter().enumerate() {
        let slow = match layer {
            HarmonicsLayer::Hysteresis(_) => true,
            HarmonicsLayer::Chebyshev(l) => l.is_dynamic(),
            HarmonicsLayer::Flux(_) => false,
        };
        if slow && layer.enabled() {
            for &rate in rates {
                if !jobs.contains(&(i, rate)) {
                    jobs.push((i, rate));
                }
            }
        }
    }
    if jobs.len() > 1 {
        par_map(&jobs, MAX_THREADS, |&(i, rate)| {
            let _ = build_layer(&config.layers[i], i, rate, 1, config.dc_block);
        });
    }
}

/// What a config layer becomes in the stage.
pub(crate) enum Built {
    /// A static Chebyshev layer: merged into the bank.
    Bank,
    /// Its own run (a group of one).
    Layer(LayerGroup),
    /// Nothing to apply (off, or amount 0).
    Silent,
}

/// Build config layer `index` at `rate` for `channels` (off the audio
/// thread: flux and hysteresis layers are calibrated here, sag/bias layers
/// certified). `dc_block` is the stage's.
pub(crate) fn build_layer(
    layer: &HarmonicsLayer,
    index: usize,
    rate: f64,
    channels: usize,
    dc_block: bool,
) -> Result<Built, HarmonicsError> {
    if !layer.enabled() {
        return Ok(Built::Silent);
    }
    Ok(match layer {
        HarmonicsLayer::Chebyshev(l) if l.is_dynamic() => {
            let core = cached_core(layer, rate, dc_block, || {
                let cfg = TubeDynamicsConfig::from_dynamic_layer(l, dc_block)
                    .map_err(|e| HarmonicsError::Dynamics(index, e.to_string()))?;
                DynamicsCore::try_new(&cfg, rate)
                    .map(SlowCore::Dynamics)
                    .map_err(|e| HarmonicsError::Dynamics(index, e.to_string()))
            })?;
            let SlowCore::Dynamics(core) = core else {
                unreachable!("a sag/bias layer's core")
            };
            Built::Layer(LayerGroup::Dynamic(Supply::new(core, index, channels)))
        }
        HarmonicsLayer::Chebyshev(_) => Built::Bank,
        HarmonicsLayer::Flux(l) => {
            let core = FluxCore::try_new(&FluxConfig::from(l), rate)
                .map_err(|e| HarmonicsError::Flux(index, e))?;
            Built::Layer(LayerGroup::Flux(Group::new(core, index, channels)))
        }
        HarmonicsLayer::Hysteresis(l) => {
            // Built (validated and calibrated) even at amount 0.
            let core = cached_core(layer, rate, dc_block, || {
                HysteresisCore::try_new(&HysteresisConfig::from(l), rate)
                    .map(SlowCore::Hysteresis)
                    .map_err(|e| HarmonicsError::Hysteresis(index, e))
            })?;
            let SlowCore::Hysteresis(core) = core else {
                unreachable!("a tape layer's core")
            };
            if l.amount > 0.0 {
                Built::Layer(LayerGroup::Hysteresis(Group::new(core, index, channels)))
            } else {
                Built::Silent
            }
        }
    })
}

/// A layer's warnings at an output rate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LayerWarnings {
    /// Orders (index 0 = H2) that fold back into the audio band: harmonic
    /// `k` of a 20 kHz tone lands at `k × 20 kHz`, which aliases below
    /// 20 kHz when `k × 20 kHz > fs − 20 kHz`. Applied anyway, as at 1×.
    pub aliased_orders: [bool; HARMONICS_ORDERS],
    /// A non-polynomial loop that aliases significantly into the audio band
    /// below its clean rate: that rate, when the output rate is below it
    /// (tape hysteresis below 4×, [`HYSTERESIS_MIN_CLEAN_RATE_HZ`]).
    pub clean_rate_hz: Option<f64>,
}

/// The warnings of `layer` at `output_rate` (none when it is off or its
/// amount is 0). Flux layers are anti-aliased (ADAA): none. Sag and bias
/// never raise a curve's degree (the sag's gain modulation adds only narrow
/// sidebands around each harmonic): the Chebyshev order rule holds.
pub fn layer_warnings(layer: &HarmonicsLayer, output_rate: f64) -> LayerWarnings {
    let mut w = LayerWarnings::default();
    let on = layer.enabled() && layer.amount() > 0.0;
    if !on {
        return w;
    }
    match layer {
        HarmonicsLayer::Chebyshev(l) => {
            for (i, (o, level)) in w.aliased_orders.iter_mut().zip(&l.levels_db).enumerate() {
                let k = (i + 2) as f64;
                *o = level.is_some() && k * AUDIO_BAND_HZ > output_rate - AUDIO_BAND_HZ;
            }
        }
        HarmonicsLayer::Flux(_) => {}
        HarmonicsLayer::Hysteresis(_) => {
            if output_rate < HYSTERESIS_MIN_CLEAN_RATE_HZ {
                w.clean_rate_hz = Some(HYSTERESIS_MIN_CLEAN_RATE_HZ);
            }
        }
    }
    w
}

/// Validate one layer's parameters, enabled or not (so turning it on cannot
/// pick up an invalid value); messages name `field` (e.g.
/// `chain.harmonics.layers[1]`). Flux layers are also calibrated once at
/// 96 kHz and hysteresis layers at every engine rate, so a target they
/// cannot reach fails here rather than when the chain builds. Off the audio
/// thread.
pub fn validate_layer(field: &str, layer: &HarmonicsLayer) -> Result<(), String> {
    match layer {
        HarmonicsLayer::Chebyshev(l) => check_chebyshev(l).map_err(|e| format!("{field}.{e}")),
        HarmonicsLayer::Flux(l) => FluxCore::try_new(&FluxConfig::from(l), VALIDATION_RATE_HZ)
            .map(drop)
            .map_err(|e| format!("{field}: {e}")),
        HarmonicsLayer::Hysteresis(l) => {
            // A bass EQ that is switched off is checked too.
            if let Some(eq) = &l.bass_eq {
                eq.validate()
                    .map_err(|e| format!("{field}: hysteresis: {e}"))?;
            }
            // Calibrated at every engine rate: the drive is rate independent
            // only up to the probe's resolution, and a layer that fails at one
            // rate must fail here, not when the chain builds there.
            let cfg = HysteresisConfig::from(l);
            for &rate in ALLOWED_RATES.iter().chain(HIGH_OUTPUT_RATES) {
                HysteresisCore::check(&cfg, f64::from(rate))
                    .map_err(|e| format!("{field}: {e} (at {rate} Hz)"))?;
            }
            Ok(())
        }
    }
}
