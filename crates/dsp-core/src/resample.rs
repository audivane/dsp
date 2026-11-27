//! Integer-ratio upsampler with long Kaiser-sinc kernels, plus offline
//! arbitrary-ratio resampling (used for impulse responses).

pub use crate::design::paths::standard::{
    ManualConfig, UpsamplerDesign, design_kernel, design_ratio, try_design_kernel,
    try_design_kernel_for,
};
pub use crate::design::phase::reflect::minimum_phase_contract;

use crate::fft::{RealFftPair, RingFifo, SplitSpectrum};
use crate::nupc::{HeadChannel, HeadStage, StagePlan, TailChannel, TailStage};
use crate::window::{bessel_i0, kaiser_beta_for_attenuation};
#[cfg(doc)]
use audivane_protocol::CutoffMode;
use audivane_protocol::ipc::{
    RESPONSE_CROSSING_LEVELS_DB, RESPONSE_WINDOW_MAX_POINTS, ResponseCrossing, ResponseWindow,
    ResponseWindowRequest, UpsamplerResponse,
};
use audivane_protocol::{
    BandSpec, FilterPhase, FilterPreset, RateRatio, UpsamplerConfig, WindowKind, resolve_rate_ratio,
};
use rustfft::num_complex::Complex64;
use std::f64::consts::PI;

/// Smallest cutoff (fraction of the source Nyquist) a
/// [`CutoffMode::NyquistStop`] design may use. Below it the transition would
/// eat more than half the source band; such designs are rejected
/// ([`nyquist_stop_check`]).
pub const NYQUIST_STOP_MIN_CUTOFF: f64 = 0.5;

/// Kaiser beta range over which [`CutoffMode::NyquistStop`] is calibrated.
/// Below 5 (A < 54 dB) Kaiser's transition estimate falls apart for short
/// filters (up to 0.8 dB over the target at beta 2.75); above 24 the target
/// (> 226 dB) is under the f64 rounding floor of long kernels.
pub const NYQUIST_STOP_BETA_RANGE: (f64, f64) = (5.0, 24.0);

/// Growth of the safety factor with the relative transition width (short
/// filters, see [`nyquist_stop_cutoff`]).
const NYQUIST_STOP_SHORT_GAIN: f64 = 0.7;

/// Stopband attenuation targeted by a Kaiser window of `beta` (Kaiser's
/// formula inverted): the level [`CutoffMode::NyquistStop`] guarantees at and
/// above the source Nyquist, and the stopband the response summary checks.
pub fn kaiser_attenuation_db(beta: f64) -> f64 {
    beta / 0.1102 + 8.7
}

/// Safety factor on Kaiser's half transition width for long filters: the
/// upper envelope of the measured minimum, plus 0.02.
///
/// The measured minimum (linear phase,
/// dense peak search) is a sawtooth in beta between 1.00 and 1.284:
/// Kaiser's `A(beta)` is a straight-line fit to the true sidelobe level, and
/// each tooth starts where another sidelobe becomes the one that lands on
/// Nyquist. The fit overstates the attenuation more and more above beta 18,
/// so the teeth rise by ~0.024 per unit of beta; the envelope is
/// `max(1.17, 1.19 + 0.03 (beta - 20))`.
fn nyquist_stop_k(beta: f64) -> f64 {
    (1.21 + 0.03 * (beta - 20.0)).max(1.19)
}

/// Why a [`CutoffMode::NyquistStop`] design cannot keep its promise.
#[derive(Clone, Debug, PartialEq)]
pub enum NyquistStopError {
    /// Kaiser beta outside [`NYQUIST_STOP_BETA_RANGE`].
    Beta(f64),
    /// The kernel is shorter than the calibrated range for the ratio: the
    /// cutoff formula would need to go below [`NYQUIST_STOP_MIN_CUTOFF`] to
    /// promise the stopband at the source Nyquist. That is a limit of the
    /// calibration, not a proof that no shorter filter can reach it (1253
    /// taps, x32, beta 20 meets `-A` at cutoff 0.5, one tap under the limit).
    TooShort {
        taps: usize,
        min_taps: usize,
        ratio: usize,
    },
}

impl std::fmt::Display for NyquistStopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (lo, hi) = NYQUIST_STOP_BETA_RANGE;
        match self {
            Self::Beta(beta) => write!(
                f,
                "stop at Nyquist needs a Kaiser beta between {lo} and {hi} (got {beta})"
            ),
            Self::TooShort {
                taps,
                min_taps,
                ratio,
            } => write!(
                f,
                "stop at Nyquist is only guaranteed from {min_taps} taps at x{ratio} with this beta \
                 (got {taps}): below that the calibrated cutoff formula cannot promise the stopband \
                 at the source Nyquist"
            ),
        }
    }
}

/// Relative cutoff shift per unit `k`: Kaiser's half transition width as a
/// fraction of the source Nyquist.
fn kaiser_half_width(taps: usize, ratio: usize, beta: f64) -> f64 {
    (ratio as f64) * (kaiser_attenuation_db(beta) - 7.95).max(0.0)
        / (14.36 * (taps.max(2) - 1) as f64)
}

/// Unclamped [`CutoffMode::NyquistStop`] cutoff (see [`nyquist_stop_cutoff`]).
fn nyquist_stop_raw(taps: usize, ratio: usize, beta: f64) -> f64 {
    let y = nyquist_stop_k(beta) * kaiser_half_width(taps, ratio, beta);
    1.0 - y * (1.0 + NYQUIST_STOP_SHORT_GAIN * y)
}

/// Largest `k * X` with `k X (1 + g k X) <= 1 - NYQUIST_STOP_MIN_CUTOFF`.
fn nyquist_stop_max_y() -> f64 {
    let g = NYQUIST_STOP_SHORT_GAIN;
    let t = 1.0 - NYQUIST_STOP_MIN_CUTOFF;
    ((1.0 + 4.0 * g * t).sqrt() - 1.0) / (2.0 * g)
}

/// Fewest taps for which [`CutoffMode::NyquistStop`] is feasible at `ratio`
/// and `beta` (`None` when beta is outside [`NYQUIST_STOP_BETA_RANGE`]).
pub fn nyquist_stop_min_taps(ratio: usize, beta: f64) -> Option<usize> {
    let (lo, hi) = NYQUIST_STOP_BETA_RANGE;
    if !(lo..=hi).contains(&beta) {
        return None;
    }
    if ratio <= 1 {
        return Some(1);
    }
    let need = nyquist_stop_k(beta) * (ratio as f64) * (kaiser_attenuation_db(beta) - 7.95)
        / (14.36 * nyquist_stop_max_y());
    let mut taps = (need.floor() as usize + 1).max(2);
    while taps > 2 && nyquist_stop_raw(taps - 1, ratio, beta) >= NYQUIST_STOP_MIN_CUTOFF {
        taps -= 1;
    }
    while nyquist_stop_raw(taps, ratio, beta) < NYQUIST_STOP_MIN_CUTOFF {
        taps += 1;
    }
    Some(taps)
}

/// The [`CutoffMode::NyquistStop`] cutoff, or why the design is infeasible.
pub fn nyquist_stop_check(taps: usize, ratio: usize, beta: f64) -> Result<f64, NyquistStopError> {
    if ratio <= 1 {
        return Ok(1.0);
    }
    let (lo, hi) = NYQUIST_STOP_BETA_RANGE;
    if !(lo..=hi).contains(&beta) {
        return Err(NyquistStopError::Beta(beta));
    }
    let cutoff = nyquist_stop_raw(taps, ratio, beta);
    if taps < 2 || cutoff < NYQUIST_STOP_MIN_CUTOFF {
        return Err(NyquistStopError::TooShort {
            taps,
            min_taps: nyquist_stop_min_taps(ratio, beta).unwrap_or(usize::MAX),
            ratio,
        });
    }
    Ok(cutoff.min(1.0))
}

/// Cutoff fraction (of the source Nyquist) that puts the Kaiser stopband
/// edge at the source Nyquist, so every frequency from the source Nyquist to
/// the output Nyquist is at or below `-A` dB, `A = kaiser_attenuation_db`.
///
/// Kaiser's transition-width estimate for an `N`-tap window at output rate
/// `f_out` is `df = (A - 7.95) / (14.36 (N - 1)) * f_out`. Moving the cutoff
/// `k df / 2` below the source Nyquist `f_out / (2 ratio)` gives the relative
/// shift `y = k X`, `X = ratio (A - 7.95) / (14.36 (N - 1))`. Kaiser's
/// estimate is empirical, so `k` is calibrated against the actual designs:
/// `nyquist_stop_k` for long filters, and a growth with the shift itself for
/// short ones (where the band edges at `+-f_c` start to interact):
///
/// `cutoff = 1 - y (1 + 0.7 y)`, `y = k(beta) X`.
///
/// The 0.7 bounds the measured short-filter excess over the whole feasible
/// domain (cutoff >= 0.5, taps from the feasibility edge up, ratios 2..=32),
/// verified with dense peak searches over the whole range from the source to
/// the output Nyquist (worst peak 0.150 dB
/// under `-A`; the `nyquist_stop_*` tests go up to 4M taps).
///
/// Designs outside the calibrated domain (see [`nyquist_stop_check`]) are
/// rejected by the daemon; here they clamp to [`NYQUIST_STOP_MIN_CUTOFF`],
/// which keeps a usable passband but not the attenuation promise.
pub fn nyquist_stop_cutoff(taps: usize, ratio: usize, kaiser_beta: f64) -> f64 {
    if ratio <= 1 {
        return 1.0;
    }
    if taps <= 1 {
        return NYQUIST_STOP_MIN_CUTOFF;
    }
    let beta = kaiser_beta.clamp(NYQUIST_STOP_BETA_RANGE.0, NYQUIST_STOP_BETA_RANGE.1);
    nyquist_stop_raw(taps, ratio, beta).clamp(NYQUIST_STOP_MIN_CUTOFF, 1.0)
}

/// Safety factor on the Gaussian window's analytic half transition.
const GAUSSIAN_HALF_WIDTH_K: f64 = 1.02;

/// Distance from the cutoff to where a Gaussian-windowed sinc of `taps`
/// taps reaches `-a_db` dB (stopband side; the passband side is symmetric),
/// as a fraction of the source Nyquist.
///
/// A Gaussian-windowed sinc is the ideal low-pass convolved with a Gaussian
/// of standard deviation `sigma_f = 1 / (2 pi sigma)` cycles per sample
/// (`sigma` the window's, in taps), so its transition is
/// `erfc((f - f_c) / (sqrt 2 sigma_f)) / 2`, which falls to `delta =
/// 10^{-A/20}` at `f - f_c = sqrt 2 sigma_f erfcinv(2 delta)`. The window's
/// truncation sits [`crate::window::GAUSSIAN_EDGE_MARGIN_DB`] under `-A`.
/// Times `GAUSSIAN_HALF_WIDTH_K`.
pub fn gaussian_half_width(taps: usize, ratio: usize, a_db: f64) -> f64 {
    let sigma = crate::window::gaussian_sigma(taps.max(2), a_db);
    let delta = 10.0_f64.powf(-a_db / 20.0);
    let x_cycles =
        std::f64::consts::SQRT_2 * crate::window::erfc_inv(2.0 * delta) / (2.0 * PI * sigma);
    GAUSSIAN_HALF_WIDTH_K * x_cycles * 2.0 * ratio as f64
}

/// Calibrated half transition (fraction of the source Nyquist) of `window`:
/// the distance from the cutoff to the stopband edge at `-A`, `A =
/// kaiser_attenuation_db(beta)`, and from the passband edge to the cutoff.
///
/// Tukey has no calibrated transition: it uses Kaiser's formula, and the
/// daemon only accepts it in fraction mode with linear phase.
pub fn half_transition(window: WindowKind, taps: usize, ratio: usize, beta: f64) -> f64 {
    match window {
        WindowKind::Kaiser | WindowKind::Tukey => {
            let y = nyquist_stop_k(beta) * kaiser_half_width(taps, ratio, beta);
            y * (1.0 + NYQUIST_STOP_SHORT_GAIN * y)
        }
        WindowKind::Gaussian => gaussian_half_width(taps, ratio, kaiser_attenuation_db(beta)),
    }
}

/// [`nyquist_stop_cutoff`] for any window (Kaiser: the same function).
pub fn nyquist_stop_cutoff_for(window: WindowKind, taps: usize, ratio: usize, beta: f64) -> f64 {
    match window {
        WindowKind::Kaiser | WindowKind::Tukey => nyquist_stop_cutoff(taps, ratio, beta),
        WindowKind::Gaussian => {
            if ratio <= 1 {
                return 1.0;
            }
            let beta = beta.clamp(NYQUIST_STOP_BETA_RANGE.0, NYQUIST_STOP_BETA_RANGE.1);
            (1.0 - half_transition(window, taps, ratio, beta)).clamp(NYQUIST_STOP_MIN_CUTOFF, 1.0)
        }
    }
}

/// Fewest taps (at least 2) whose half transition fits in `w` (fraction of
/// the source Nyquist), by doubling and bisection: `half_transition` falls
/// with the length.
fn min_taps_for_half_width(window: WindowKind, ratio: usize, beta: f64, w: f64) -> usize {
    let fits = |taps: usize| half_transition(window, taps, ratio, beta) <= w;
    if fits(2) {
        return 2;
    }
    let mut hi = 4usize;
    while !fits(hi) {
        if hi >= 1 << 40 {
            return usize::MAX;
        }
        hi *= 2;
    }
    let mut lo = hi / 2;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}

/// [`nyquist_stop_min_taps`] for any window.
pub fn nyquist_stop_min_taps_for(window: WindowKind, ratio: usize, beta: f64) -> Option<usize> {
    if window == WindowKind::Kaiser {
        return nyquist_stop_min_taps(ratio, beta);
    }
    let (lo, hi) = NYQUIST_STOP_BETA_RANGE;
    if !(lo..=hi).contains(&beta) {
        return None;
    }
    if ratio <= 1 {
        return Some(1);
    }
    Some(min_taps_for_half_width(
        window,
        ratio,
        beta,
        1.0 - NYQUIST_STOP_MIN_CUTOFF,
    ))
}

/// [`nyquist_stop_check`] for any window.
pub fn nyquist_stop_check_for(
    window: WindowKind,
    taps: usize,
    ratio: usize,
    beta: f64,
) -> Result<f64, NyquistStopError> {
    if window == WindowKind::Kaiser || ratio <= 1 {
        return nyquist_stop_check(taps, ratio, beta);
    }
    let (lo, hi) = NYQUIST_STOP_BETA_RANGE;
    if !(lo..=hi).contains(&beta) {
        return Err(NyquistStopError::Beta(beta));
    }
    let cutoff = 1.0 - half_transition(window, taps, ratio, beta);
    if taps < 2 || cutoff < NYQUIST_STOP_MIN_CUTOFF {
        return Err(NyquistStopError::TooShort {
            taps,
            min_taps: nyquist_stop_min_taps_for(window, ratio, beta).unwrap_or(usize::MAX),
            ratio,
        });
    }
    Ok(cutoff.min(1.0))
}

/// `20 log10(x)`, floored at -400 dB (also for 0).
fn lin_to_db_floor(x: f64) -> f64 {
    if x > 1.0e-20 {
        20.0 * x.log10()
    } else {
        -400.0
    }
}

/// Attenuation range of [`CutoffMode::Band`] designs (dB): the Kaiser beta
/// range over which the transition-width calibration of
/// [`nyquist_stop_cutoff`] holds ([`NYQUIST_STOP_BETA_RANGE`], 54.1 to
/// 226.5 dB), rounded inwards.
pub const BAND_ATTENUATION_RANGE_DB: (f64, f64) = (55.0, 225.0);

/// Smallest passband end of a [`CutoffMode::Band`] design given as a
/// fraction of the source Nyquist. An absolute passband end resolved at a
/// high source rate may sit lower, as long as the half transition stays in
/// the calibrated domain (at most 0.5).
pub const BAND_MIN_PASSBAND_END: f64 = 0.5;

/// Extra depth (dB) a band-length design ([`band_length_design_for`])
/// takes under a passband end of [`BAND_MIN_PASSBAND_END`] (only a resolved
/// passband end in Hz gets there). The transition is then wide enough that
/// a Kaiser design's passband ripples more than its stopband's relative
/// error, by up to 2.67 times at the widest; 10 dB (3.16 times) covers the
/// whole range.
pub const BAND_WIDE_PASSBAND_MARGIN_DB: f64 = 10.0;

/// Largest half transition a band-length design keeps under a 0.5
/// passband end when [`BAND_WIDE_PASSBAND_MARGIN_DB`] does not fit under
/// the depth limit (above 215 dB): the passband end is raised to the
/// stopband start minus twice this (at most 1.0), so the kernel is longer
/// and the passband still flat to (past) the requested end.
pub const BAND_WIDE_CAPPED_HALF_WIDTH: f64 = 0.2;

/// Largest stopband start of a [`CutoffMode::Band`] design (fraction of the
/// source Nyquist). Past 1.0 images of the top of the band leak through.
pub const BAND_MAX_STOPBAND_START: f64 = 1.5;

/// Why a [`CutoffMode::Band`] specification cannot be designed.
#[derive(Clone, Debug, PartialEq)]
pub enum BandError {
    /// An edge is out of range or the passband ends at or after the
    /// stopband start.
    Edges {
        passband_end: f64,
        stopband_start: f64,
    },
    /// An absolute passband end (Hz), which needs the source rate to
    /// become a fraction of the Nyquist: the design paths resolve it
    /// ([`crate::design::band_passband_end`]) before the band is designed.
    Hertz(f64),
    /// Attenuation outside [`BAND_ATTENUATION_RANGE_DB`].
    Attenuation(f64),
    Automatic(String),
    Length,
}

impl std::fmt::Display for BandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Length => write!(f, "band transition needs an unrepresentable kernel length"),
            Self::Automatic(reason) => write!(f, "automatic taps: {reason}"),
            Self::Hertz(hz) => write!(
                f,
                "the passband end ({hz} Hz) is a frequency, not a fraction of the Nyquist: it is \
                 resolved at the source rate before the band is designed"
            ),
            Self::Edges {
                passband_end,
                stopband_start,
            } => write!(
                f,
                "band edges need {BAND_MIN_PASSBAND_END} <= passband end ({passband_end}) <= 1 \
                 and passband end < stopband start ({stopband_start}) <= \
                 {BAND_MAX_STOPBAND_START} (fractions of the source Nyquist)"
            ),
            Self::Attenuation(a) => {
                let (lo, hi) = BAND_ATTENUATION_RANGE_DB;
                write!(
                    f,
                    "band attenuation must be between {lo} and {hi} dB (got {a})"
                )
            }
        }
    }
}

/// Taps, Kaiser beta and cutoff of a [`CutoffMode::Band`] design at `ratio`.
///
/// Beta comes from the attenuation (Kaiser's formula), the
/// cutoff sits midway between the edges, and the length is the shortest whose
/// calibrated half transition `y (1 + 0.7 y)` ([`nyquist_stop_cutoff`]) fits
/// in half the gap between the edges. A windowed sinc's transition is
/// symmetric about the cutoff, so the same margin that puts the stopband
/// edge at or under `-A` dB keeps the passband edge within the same relative
/// error of 0 dB. The length grows with the ratio, so the kernel lasts the
/// same time at every ratio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandDesign {
    pub taps: usize,
    pub kaiser_beta: f64,
    pub cutoff: f64,
}

/// The Kaiser [`band_length_design_for`].
pub fn band_design(spec: &BandSpec, ratio: usize) -> Result<BandDesign, BandError> {
    band_length_design_for(WindowKind::Kaiser, spec, ratio)
}

/// The design of a band length ([`band_design_for`]), kept to the band's
/// promise in its passband under a 0.5 passband end: designed
/// [`BAND_WIDE_PASSBAND_MARGIN_DB`] deeper, or, where that would pass the
/// top of [`BAND_ATTENUATION_RANGE_DB`] (above 215 dB), at the top with its
/// transition narrowed to [`BAND_WIDE_CAPPED_HALF_WIDTH`]. From 0.5 up it
/// is [`band_design_for`] itself. An automatic length measures both bands
/// and takes [`band_design_for`] as its estimate.
pub fn band_length_design_for(
    window: WindowKind,
    spec: &BandSpec,
    ratio: usize,
) -> Result<BandDesign, BandError> {
    // The band as given is validated (edges and depth) first.
    let plain = band_design_for(window, spec, ratio)?;
    if spec.passband_end >= BAND_MIN_PASSBAND_END {
        return Ok(plain);
    }
    let hi = BAND_ATTENUATION_RANGE_DB.1;
    let deeper = spec.attenuation_db + BAND_WIDE_PASSBAND_MARGIN_DB;
    let wide = if deeper <= hi {
        BandSpec {
            attenuation_db: deeper,
            ..*spec
        }
    } else {
        BandSpec {
            attenuation_db: hi,
            // At most the Nyquist (a stopband start above 1.4 keeps a
            // little more than the capped half width).
            passband_end: spec
                .passband_end
                .max((spec.stopband_start - 2.0 * BAND_WIDE_CAPPED_HALF_WIDTH).min(1.0)),
            ..*spec
        }
    };
    band_design_for(window, &wide, ratio)
}

/// [`band_design`] for any window. The Gaussian window's length comes from
/// its analytic transition ([`gaussian_half_width`]).
pub fn band_design_for(
    window: WindowKind,
    spec: &BandSpec,
    ratio: usize,
) -> Result<BandDesign, BandError> {
    let (pe, se, a) = (spec.passband_end, spec.stopband_start, spec.attenuation_db);
    if let Some(hz) = spec.passband_end_hz {
        return Err(BandError::Hertz(hz));
    }
    // A passband end under `BAND_MIN_PASSBAND_END` only comes from an
    // absolute one resolved at a high source rate (the field bounds keep a
    // fraction at or above it); the length stays calibrated while the half
    // transition is at most 0.5.
    if !(pe.is_finite() && se.is_finite())
        || pe <= 0.0
        || (pe < BAND_MIN_PASSBAND_END && se - pe > 1.0)
        || pe > 1.0
        || se <= pe
        || se > BAND_MAX_STOPBAND_START
    {
        return Err(BandError::Edges {
            passband_end: pe,
            stopband_start: se,
        });
    }
    let (lo, hi) = BAND_ATTENUATION_RANGE_DB;
    if !(a.is_finite() && (lo..=hi).contains(&a)) {
        return Err(BandError::Attenuation(a));
    }
    let beta = kaiser_beta_for_attenuation(a);
    let cutoff = 0.5 * (pe + se);
    if ratio <= 1 {
        return Ok(BandDesign {
            taps: 1,
            kaiser_beta: beta,
            cutoff,
        });
    }
    let w = 0.5 * (se - pe);
    if window == WindowKind::Gaussian {
        return Ok(BandDesign {
            taps: min_taps_for_half_width(window, ratio, beta, w),
            kaiser_beta: beta,
            cutoff,
        });
    }
    // Largest y with y (1 + g y) <= half gap.
    let g = NYQUIST_STOP_SHORT_GAIN;
    let y_max = ((1.0 + 4.0 * g * w).sqrt() - 1.0) / (2.0 * g);
    let fits = |taps: usize| {
        let y = nyquist_stop_k(beta) * kaiser_half_width(taps, ratio, beta);
        y * (1.0 + g * y) <= w
    };
    let need = nyquist_stop_k(beta) * (ratio as f64) * (kaiser_attenuation_db(beta) - 7.95)
        / (14.36 * y_max);
    if !need.is_finite() || need >= (usize::MAX - 1) as f64 {
        return Err(BandError::Length);
    }
    let mut taps = (need.ceil() as usize + 1).max(2);
    while taps > 2 && fits(taps - 1) {
        taps -= 1;
    }
    while !fits(taps) {
        taps += 1;
    }
    Ok(BandDesign {
        taps,
        kaiser_beta: beta,
        cutoff,
    })
}

use crate::design::budget::RATIONAL_SWAP_PIPELINES;

struct UpsamplerChannel {
    in_fifo: RingFifo,
    out_fifo: RingFifo,
    head: HeadChannel,
    /// One entry per tail stage (`None` never happens for the one kernel
    /// an upsampler has: its stages end where the kernel does).
    tails: Vec<Option<TailChannel>>,
}

/// The FFT polyphase engine: the `ratio` polyphase branches of one kernel
/// (`h_r[n] = h[n * ratio + r]`) convolved with the input by
/// non-uniformly partitioned overlap-save (`crate::nupc`). Stage 0 is
/// uniform at the input block `block_in` (the chain block's power of two,
/// 64 ..= 1024), so the latency is `block_in * ratio` output frames for
/// every kernel; a long kernel's later taps run in larger, time-sliced
/// stages (`crate::nupc::polyphase_plan`), one forward transform and one
/// delay line per stage shared by every branch. A kernel short enough for
/// one stage runs the uniformly partitioned engine alone.
pub struct Upsampler {
    ratio: usize,
    channels: usize,
    max_block_in: usize,
    /// Kernel length at the output rate.
    taps: usize,
    /// The kernel's own delay in output frames (see `latency_out_frames`).
    kernel_delay: usize,
    block_in: usize,
    block_latency_out: usize,
    /// Stage 0: partitions `[0][partition][branch]`, each branch `r` holding
    /// `h_r[p * block_in ..]` scaled by `1 / (2 * block_in)`. `None` at
    /// ratio 1 or without channels.
    head: Option<HeadStage>,
    tails: Vec<TailStage>,
    ch_states: Vec<UpsamplerChannel>,
    curr_in: Vec<f64>,
    block_out_interleaved: Vec<f64>,
    /// Estimated work (model units) of the last `process` call.
    last_cost: f64,
}

/// Largest integer ratio [`Upsampler`] runs at (44.1 kHz x64 = 2.8224 MHz).
pub const MAX_UPSAMPLER_RATIO: usize = audivane_protocol::MAX_INTEGER_RATIO as usize;

/// The spectra of the polyphase branches of `kernel` over the branch
/// samples `[offset, end)` in `block`-frame partitions, `[partition][branch]`:
/// as many partitions as the longest branch needs (`at_least_one`: one for
/// an empty range too), a shorter branch padded with zeros.
fn polyphase_partitions(
    fft: &mut RealFftPair,
    kernel: &[f64],
    ratio: usize,
    offset: usize,
    block: usize,
    end: usize,
    at_least_one: bool,
) -> Vec<Vec<SplitSpectrum>> {
    let inv_n = 1.0 / (2 * block) as f64;
    let max_branch_len = kernel.len().div_ceil(ratio);
    let stage_end = max_branch_len.min(end);
    let mut num_parts = stage_end.saturating_sub(offset).div_ceil(block);
    if at_least_one {
        num_parts = num_parts.max(1);
    }
    let mut branch_buf = vec![0.0; block];
    (0..num_parts)
        .map(|p| {
            (0..ratio)
                .map(|r| {
                    branch_buf.fill(0.0);
                    let mut count = 0usize;
                    let first = offset + p * block;
                    for (i, slot) in branch_buf.iter_mut().enumerate() {
                        let n = first + i;
                        let k_idx = n * ratio + r;
                        if n < end && k_idx < kernel.len() {
                            *slot = kernel[k_idx];
                            count = i + 1;
                        } else {
                            break;
                        }
                    }
                    fft.forward_padded_scaled(&branch_buf[..count], inv_n)
                })
                .collect()
        })
        .collect()
}

impl Upsampler {
    /// Panics when the kernel cannot be designed (see [`Upsampler::try_new`]).
    pub fn new(d: &UpsamplerDesign, channels: usize, max_block_in: usize) -> Self {
        Self::try_new(d, channels, max_block_in).unwrap_or_else(|e| panic!("upsampler: {e}"))
    }

    /// Build an upsampler, or report why its kernel cannot be designed to
    /// tolerance ([`try_design_kernel`]).
    pub fn try_new(
        d: &UpsamplerDesign,
        channels: usize,
        max_block_in: usize,
    ) -> Result<Self, crate::minphase::MinimumPhaseError> {
        Self::build(d, channels, max_block_in, None)
    }

    /// Build an upsampler, optionally overriding the internal partition block
    /// size. The real-time path keeps the 64..=1024 clamp (bounded latency);
    /// the offline response measurement uses a larger block for long kernels,
    /// which cuts the partitioned-convolution work roughly in proportion.
    fn build(
        d: &UpsamplerDesign,
        channels: usize,
        max_block_in: usize,
        block_override: Option<usize>,
    ) -> Result<Self, crate::minphase::MinimumPhaseError> {
        assert!(
            (1..=MAX_UPSAMPLER_RATIO).contains(&d.ratio),
            "upsampler ratio must be in 1..={MAX_UPSAMPLER_RATIO}, got {}",
            d.ratio
        );

        if d.ratio == 1 || channels == 0 {
            return Ok(Self {
                ratio: d.ratio,
                channels,
                max_block_in,
                taps: d.taps,
                kernel_delay: 0,
                block_in: 0,
                block_latency_out: 0,
                head: None,
                tails: Vec::new(),
                ch_states: Vec::new(),
                curr_in: Vec::new(),
                block_out_interleaved: Vec::new(),
                last_cost: 0.0,
            });
        }

        let kernel = try_design_kernel(d)?;
        let kernel_delay = crate::design::paths::standard::kernel_delay(&kernel, d.phase);
        Ok(Self::build_from_kernel(
            &kernel,
            d.ratio,
            kernel_delay,
            channels,
            max_block_in,
            block_override,
        ))
    }

    /// Run a ready-made kernel at the output rate (for example a
    /// [`crate::presets`] recipe). The kernel's passband gain is `ratio`
    /// (each polyphase branch sums to about 1), so an impulse of amplitude
    /// `A` comes out as `A * kernel` after the block latency.
    /// `kernel_delay` is the kernel's own delay in output frames, reported
    /// by [`Self::latency_out_frames`].
    pub fn from_kernel(
        kernel: &[f64],
        ratio: usize,
        kernel_delay: usize,
        channels: usize,
        max_block_in: usize,
    ) -> Self {
        assert!(
            (1..=MAX_UPSAMPLER_RATIO).contains(&ratio),
            "upsampler ratio must be in 1..={MAX_UPSAMPLER_RATIO}, got {ratio}"
        );
        if ratio == 1 || channels == 0 {
            let mut up = Self::build(
                &UpsamplerDesign::linear(ratio, kernel.len(), 14.0, 1.0),
                channels,
                max_block_in,
                None,
            )
            .expect("ratio-1 upsampler needs no kernel");
            up.taps = kernel.len();
            return up;
        }
        Self::build_from_kernel(kernel, ratio, kernel_delay, channels, max_block_in, None)
    }

    /// [`Self::from_kernel`] with the offline measurement's block size
    /// (one uniformly partitioned stage).
    pub(crate) fn from_kernel_with_block(
        kernel: &[f64],
        ratio: usize,
        kernel_delay: usize,
        channels: usize,
        block: usize,
    ) -> Self {
        Self::build_from_kernel(kernel, ratio, kernel_delay, channels, block, Some(block))
    }

    /// [`Self::from_kernel`] on one uniformly partitioned stage whatever
    /// the kernel's length (the engine before non-uniform partitions): a
    /// reference for tests and benchmarks.
    #[doc(hidden)]
    pub fn from_kernel_uniform(
        kernel: &[f64],
        ratio: usize,
        kernel_delay: usize,
        channels: usize,
        max_block_in: usize,
    ) -> Self {
        let block = crate::design::budget::engine_block(max_block_in);
        Self::build_from_kernel(
            kernel,
            ratio,
            kernel_delay,
            channels,
            max_block_in,
            Some(block),
        )
    }

    fn build_from_kernel(
        kernel: &[f64],
        ratio: usize,
        kernel_delay: usize,
        channels: usize,
        max_block_in: usize,
        block_override: Option<usize>,
    ) -> Self {
        let block_in = match block_override {
            Some(block) => block.next_power_of_two().max(2),
            None => crate::design::budget::engine_block(max_block_in),
        };
        crate::test_support::record("upsampler", || {
            crate::test_support::words(
                &[
                    ratio as u64,
                    kernel_delay as u64,
                    block_in as u64,
                    kernel.len() as u64,
                ],
                kernel,
            )
        });
        let max_branch_len = kernel.len().div_ceil(ratio);
        let plan = match block_override {
            Some(_) => vec![StagePlan {
                offset: 0,
                block: block_in,
                end: usize::MAX,
            }],
            None => crate::nupc::polyphase_plan(block_in, ratio, max_branch_len),
        };
        Self::with_plan(kernel, ratio, kernel_delay, channels, max_block_in, &plan)
    }

    /// Build on an explicit partition schedule (validated: see
    /// `crate::nupc`); stage 0's block is the input block.
    fn with_plan(
        kernel: &[f64],
        ratio: usize,
        kernel_delay: usize,
        channels: usize,
        max_block_in: usize,
        plan: &[StagePlan],
    ) -> Self {
        let block_in = plan[0].block;
        crate::nupc::validate_plan(plan, block_in);
        let mut fft = RealFftPair::new(2 * block_in);
        let head_parts =
            polyphase_partitions(&mut fft, kernel, ratio, 0, block_in, plan[0].end, true);
        let head = HeadStage::from_spectra(block_in, ratio, vec![head_parts]);
        let tails: Vec<TailStage> = plan[1..]
            .iter()
            .map(|&st| {
                let mut fft = RealFftPair::new(2 * st.block);
                let parts = polyphase_partitions(
                    &mut fft, kernel, ratio, st.offset, st.block, st.end, false,
                );
                // `[partition][branch]` to `[branch][partition]`.
                let mut by_branch: Vec<Vec<SplitSpectrum>> = (0..ratio)
                    .map(|_| Vec::with_capacity(parts.len()))
                    .collect();
                for part in parts {
                    for (branch, spec) in by_branch.iter_mut().zip(part) {
                        branch.push(spec);
                    }
                }
                TailStage::from_spectra(vec![by_branch], st, block_in, ratio)
            })
            .collect();

        let block_latency_out = block_in * ratio;
        let in_fifo_cap = block_in + max_block_in.max(block_in);
        let out_fifo_cap = block_latency_out + max_block_in.max(block_in) * ratio;

        let ch_states = (0..channels)
            .map(|_| {
                let mut out_fifo = RingFifo::with_capacity(out_fifo_cap);
                out_fifo.reset_zeros(block_latency_out);
                UpsamplerChannel {
                    in_fifo: RingFifo::with_capacity(in_fifo_cap),
                    out_fifo,
                    head: head.new_channel(0),
                    tails: tails.iter().map(|t| t.new_channel(0)).collect(),
                }
            })
            .collect();

        Self {
            ratio,
            channels,
            max_block_in,
            taps: kernel.len(),
            kernel_delay,
            block_in,
            block_latency_out,
            head: Some(head),
            tails,
            ch_states,
            curr_in: vec![0.0; block_in],
            block_out_interleaved: vec![0.0; block_latency_out],
            last_cost: 0.0,
        }
    }

    pub fn ratio(&self) -> usize {
        self.ratio
    }

    /// Algorithmic FIFO/block latency in output frames (`0` when `ratio == 1`,
    /// otherwise `block_in * ratio`). Useful when comparing against causal
    /// direct convolution of `design_kernel`.
    pub fn block_latency_out_frames(&self) -> usize {
        self.block_latency_out
    }

    /// Delay in output frames: the block latency plus the kernel's own
    /// delay (linear phase: `(taps - 1) / 2`; minimum and intermediate
    /// phase: the position of the impulse peak).
    pub fn latency_out_frames(&self) -> usize {
        if self.ratio == 1 {
            return 0;
        }
        self.block_latency_out + self.kernel_delay
    }

    /// Kernel length at the output rate (for intermediate phase usually
    /// longer than the design's `taps`).
    pub fn kernel_taps(&self) -> usize {
        self.taps
    }

    /// The engine's stages: `(block, partitions)` per stage, stage 0
    /// first (empty at ratio 1 or without channels).
    pub fn stages(&self) -> Vec<(usize, usize)> {
        let Some(head) = &self.head else {
            return Vec::new();
        };
        std::iter::once((self.block_in, head.parts(0)))
            .chain(self.tails.iter().map(|t| (t.schedule().block, t.parts(0))))
            .collect()
    }

    /// What the engine keeps, in bytes: the filter spectra its channels
    /// share (after [`Self::split_channels`] too), and the buffers of one
    /// channel (delay lines, stage blocks and outputs, FIFOs, the head's
    /// accumulators; FFT plans and scratch not counted).
    pub fn resident_bytes(&self) -> (usize, usize) {
        let Some(head) = &self.head else {
            return (0, 0);
        };
        let shared = head.shared_bytes()
            + self
                .tails
                .iter()
                .map(TailStage::shared_bytes)
                .sum::<usize>();
        let per_channel = self.ch_states.first().map_or(0, |ch| {
            ch.head.bytes()
                + ch.tails
                    .iter()
                    .flatten()
                    .map(TailChannel::bytes)
                    .sum::<usize>()
                + 8 * (ch.in_fifo.capacity() + ch.out_fifo.capacity())
        }) + head.own_bytes()
            + 8 * (self.curr_in.len() + self.block_out_interleaved.len());
        (shared, per_channel)
    }

    /// Estimated work of the last `process` call, in the stage cost
    /// model's units (see [`crate::conv::Convolver::last_process_cost`]).
    pub fn last_process_cost(&self) -> f64 {
        self.last_cost
    }

    /// Scale the output by `gain` (headroom). Exact: the gain is folded into
    /// the kernel spectra, so it costs nothing per sample.
    pub fn scale_output(&mut self, gain: f64) {
        // Unique until the upsampler is split (this runs right after
        // construction), so this never copies.
        if let Some(head) = &mut self.head {
            head.scale(gain);
        }
        for tail in &mut self.tails {
            tail.scale(gain);
        }
    }

    /// `output[c].len()` must be `input[c].len() * ratio`.
    pub fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        assert_eq!(input.len(), self.channels, "input channel count mismatch");
        assert_eq!(output.len(), self.channels, "output channel count mismatch");
        if self.channels == 0 {
            return;
        }
        let n_in = input[0].len();
        assert!(
            n_in <= self.max_block_in,
            "input frame count {n_in} exceeds max_block_in {}",
            self.max_block_in
        );
        let n_out = n_in * self.ratio;
        for c in 0..self.channels {
            assert_eq!(input[c].len(), n_in, "inconsistent input slice length");
            assert_eq!(
                output[c].len(),
                n_out,
                "output slice length must be input.len() * ratio"
            );
        }
        self.last_cost = 0.0;
        if n_in == 0 {
            return;
        }

        if self.ratio == 1 {
            for c in 0..self.channels {
                output[c].copy_from_slice(input[c]);
            }
            return;
        }

        let head = self.head.as_mut().expect("head stage exists for ratio > 1");
        let b = self.block_in;

        for c in 0..self.channels {
            let ch = &mut self.ch_states[c];
            ch.in_fifo.push_slice(input[c]);

            while ch.in_fifo.len() >= b {
                ch.in_fifo.pop_slice(&mut self.curr_in);
                head.process_block(
                    0,
                    &mut ch.head,
                    &self.curr_in,
                    &mut self.block_out_interleaved,
                );
                self.last_cost += head.block_cost(0);
                for (stage, tail) in self.tails.iter_mut().zip(ch.tails.iter_mut()) {
                    if let Some(tail) = tail {
                        self.last_cost +=
                            stage.tick(0, tail, &self.curr_in, &mut self.block_out_interleaved);
                    }
                }
                ch.out_fifo.push_slice(&self.block_out_interleaved);
            }

            ch.out_fifo.pop_slice(output[c]);
        }
    }

    /// One single-channel upsampler per channel, each running its channel
    /// exactly as this one does (bit for bit), from this one's state. The
    /// kernel spectra and FFT tables are shared; each gets its own FFT
    /// scratch. Off the real-time thread (it allocates); `self` is left
    /// without channels.
    pub fn split_channels(&mut self) -> Vec<Upsampler> {
        // A ratio-1 upsampler keeps no channel state.
        let mut states = std::mem::take(&mut self.ch_states).into_iter();
        let n = self.channels;
        self.channels = 0;
        (0..n)
            .map(|_| Upsampler {
                ratio: self.ratio,
                channels: 1,
                max_block_in: self.max_block_in,
                taps: self.taps,
                kernel_delay: self.kernel_delay,
                block_in: self.block_in,
                block_latency_out: self.block_latency_out,
                head: self.head.clone(),
                tails: self.tails.clone(),
                ch_states: states.next().into_iter().collect(),
                curr_in: self.curr_in.clone(),
                block_out_interleaved: self.block_out_interleaved.clone(),
                last_cost: 0.0,
            })
            .collect()
    }

    pub fn reset(&mut self) {
        if self.ratio == 1 {
            return;
        }
        for ch in &mut self.ch_states {
            ch.in_fifo.clear();
            ch.out_fifo.reset_zeros(self.block_latency_out);
            ch.head.reset();
            for tail in ch.tails.iter_mut().flatten() {
                tail.reset();
            }
        }
        self.last_cost = 0.0;
    }
}

/// FFT length for the measured upsampler response: the next power of two at or
/// above `2 * impulse_len`, guaranteeing FFT bin spacing `<= 0.5 * output_rate / taps`.
fn response_fft_len(impulse_len: usize) -> Result<usize, String> {
    impulse_len
        .checked_mul(2)
        .and_then(|n| n.checked_next_power_of_two())
        .map(|n| n.max(4))
        .ok_or_else(|| format!("impulse length {impulse_len} is too large to FFT"))
}

/// Internal partition block for the offline response measurement.
///
/// The real-time upsampler clamps to 1024 frames for bounded latency, but the
/// measurement feeds one impulse and only cares about total work: a larger
/// block cuts the number of partitions and therefore the partitioned
/// convolution work roughly proportionally, without changing the measured
/// response (the block latency is included in the impulse window).
fn measurement_block(taps: usize, ratio: usize) -> usize {
    let per_block = 512usize.saturating_mul(ratio.max(1));
    taps.div_ceil(per_block).next_power_of_two().clamp(64, 4096)
}

/// Frequency grid of [`upsampler_response`]: 0, ~600 log-spaced points up to
/// the output Nyquist, plus dense linear points over the transition around the
/// source Nyquist (0.8x .. 1.25x), sorted and deduplicated.
fn response_freqs(source_rate: u32, output_rate: u32) -> Vec<f64> {
    let nyquist_out = output_rate as f64 / 2.0;
    let nyquist_in = source_rate as f64 / 2.0;
    let lo = 20.0_f64.min(nyquist_out / 2.0).max(1.0);
    let mut freqs = Vec::with_capacity(1024);
    freqs.push(0.0);
    for i in 0..600 {
        let t = i as f64 / 599.0;
        freqs.push(lo * (nyquist_out / lo).powf(t));
    }
    let from = 0.8 * nyquist_in;
    let to = (1.25 * nyquist_in).min(nyquist_out);
    if to > from {
        for i in 0..=400 {
            freqs.push(from + (to - from) * (i as f64 / 400.0));
        }
    }
    freqs.retain(|f| *f >= 0.0 && *f <= nyquist_out + 1.0e-9);
    freqs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    freqs.dedup_by(|a, b| (*a - *b).abs() <= 1.0e-9);
    freqs
}

/// Block length of the direct evaluation: twiddles inside a block come from a
/// per-frequency table, each block start is rotated by an exact `sin_cos`.
const DFT_BLOCK: usize = 256;
/// Frequencies evaluated together per pass over a block (cache reuse).
const DFT_BATCH: usize = 16;
/// Crossings are reported down to this far above the stopband level; the
/// last few dB before it are where the transition meets the first nulls.
const CROSSING_STOPBAND_MARGIN_DB: f64 = 3.0;

/// `|sum h[n] e^(-j 2 pi f n / rate)|` for each frequency, multithreaded.
///
/// Exact apart from float rounding (no interpolation): the inner loop is a
/// dot product of a 256-sample block with a table of `cos/sin(w m)`, and each
/// block's phase `w n0` is evaluated directly, so rounding does not build up
/// over long impulses.
pub fn dft_magnitudes(h: &[f64], rate: f64, freqs: &[f64]) -> Vec<f64> {
    dft_complex(h, rate, freqs)
        .into_iter()
        .map(|c| c.norm())
        .collect()
}

/// `sum h[n] e^{-j 2 pi f n / rate}` at each frequency (see
/// [`dft_magnitudes`]).
pub fn dft_complex(h: &[f64], rate: f64, freqs: &[f64]) -> Vec<Complex64> {
    let mut out = vec![Complex64::new(0.0, 0.0); freqs.len()];
    if freqs.is_empty() {
        return out;
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 16);
    // Below a few million multiply-adds the thread start-up costs more.
    let work = h.len().saturating_mul(freqs.len());
    let threads = if work < 4_000_000 { 1 } else { threads };
    let per = freqs.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for (fs, dst) in freqs.chunks(per).zip(out.chunks_mut(per)) {
            scope.spawn(move || {
                for (fb, db) in fs.chunks(DFT_BATCH).zip(dst.chunks_mut(DFT_BATCH)) {
                    dft_batch(h, rate, fb, db);
                }
            });
        }
    });
    out
}

fn dft_batch(h: &[f64], rate: f64, freqs: &[f64], out: &mut [Complex64]) {
    let n_freqs = freqs.len();
    // cos/sin tables, one row of DFT_BLOCK per frequency.
    let mut tw_re = vec![0.0; n_freqs * DFT_BLOCK];
    let mut tw_im = vec![0.0; n_freqs * DFT_BLOCK];
    let mut cycles = vec![0.0; n_freqs];
    for (i, &f) in freqs.iter().enumerate() {
        let c = f / rate;
        cycles[i] = c;
        for m in 0..DFT_BLOCK {
            let (s, co) = (2.0 * PI * frac(c * m as f64)).sin_cos();
            tw_re[i * DFT_BLOCK + m] = co;
            tw_im[i * DFT_BLOCK + m] = -s;
        }
    }
    let mut acc_re = vec![0.0; n_freqs];
    let mut acc_im = vec![0.0; n_freqs];
    for (b, block) in h.chunks(DFT_BLOCK).enumerate() {
        let n0 = (b * DFT_BLOCK) as f64;
        for i in 0..n_freqs {
            let row_re = &tw_re[i * DFT_BLOCK..i * DFT_BLOCK + block.len()];
            let row_im = &tw_im[i * DFT_BLOCK..i * DFT_BLOCK + block.len()];
            let mut sr = 0.0;
            let mut si = 0.0;
            for ((&x, &c), &s) in block.iter().zip(row_re).zip(row_im) {
                sr += x * c;
                si += x * s;
            }
            // e^(-j w n0) * (sr + j si); keep the low product term so multi-million
            // sample blocks do not lose ~20 bits of phase precision in `cycles * n0`.
            let p_hi = cycles[i] * n0;
            let p_lo = cycles[i].mul_add(n0, -p_hi);
            let (s0, c0) = (2.0 * PI * (frac(p_hi) + p_lo)).sin_cos();
            acc_re[i] += sr * c0 + si * s0;
            acc_im[i] += si * c0 - sr * s0;
        }
    }
    for i in 0..n_freqs {
        out[i] = Complex64::new(acc_re[i], acc_im[i]);
    }
}

#[inline]
fn frac(x: f64) -> f64 {
    x - x.floor()
}

/// The engine's upsampler measured once: the summary response plus the
/// impulse it came from, so zoom windows can be evaluated later without
/// measuring again.
#[derive(Clone, Debug)]
pub struct MeasuredUpsampler {
    /// Summary response (no window).
    pub response: UpsamplerResponse,
    /// The whole measured impulse at the output rate (leading and trailing
    /// exact zeros trimmed; a shift does not change the magnitude).
    impulse: Vec<f64>,
    /// Divides the raw DFT magnitude so the passband reads 0 dB.
    gain: f64,
    /// Sample rate of `impulse`: the output rate, or the intermediate grid
    /// rate for a rational ratio.
    impulse_rate: u32,
    /// The plan of the prepared design (none at x1).
    plan: Option<crate::design::Plan>,
}

impl MeasuredUpsampler {
    /// Measured relative amplitude error in protected bands, whose edges
    /// are fractions of the input Nyquist (of the lower of the two
    /// Nyquists at a rational ratio, as the design takes them). Uses the
    /// full engine impulse, on its own grid ([`design_ratio`] impulse
    /// samples per sample at the lower rate).
    pub fn protected_band_error(&self, passband_end: f64, stopband_start: f64) -> f64 {
        let scale = 2.0 * design_ratio(self.response.rate_ratio) as f64;
        crate::design::auto::protected_error(
            &self.impulse,
            self.gain,
            passband_end / scale,
            stopband_start / scale,
        )
    }

    pub fn output_rate(&self) -> u32 {
        self.response.output_rate
    }

    /// The plan of the design as prepared (an automatic length searched);
    /// `None` at x1.
    pub fn plan(&self) -> Option<&crate::design::Plan> {
        self.plan.as_ref()
    }

    /// Record the prepared design `p` of `cfg` (its resolved design and
    /// plan, the bounded-input peak).
    fn with_prepared(mut self, cfg: &UpsamplerConfig, p: &crate::design::Prepared) -> Self {
        let based_on = cfg
            .preset
            .clone()
            .or_else(|| cfg.design.as_ref().and_then(|d| d.based_on.clone()));
        self.response.design = Some(audivane_protocol::StoredDesign {
            based_on,
            design: p.design.clone(),
        });
        self.response.plan = Some(p.plan.wire(None, Some(p.safety.l1_db)));
        self.plan = Some(p.plan);
        self
    }

    /// Rate the response runs to the Nyquist of: the output rate, or the
    /// intermediate grid rate (source x `up`) for a rational ratio.
    pub fn impulse_rate(&self) -> u32 {
        self.impulse_rate
    }

    /// Number of impulse samples kept (for cache accounting).
    pub fn impulse_len(&self) -> usize {
        self.impulse.len()
    }

    /// Magnitude in dB at each frequency, evaluated directly from the
    /// impulse.
    pub fn magnitude_db_at(&self, freqs: &[f64]) -> Vec<f64> {
        let rate = f64::from(self.impulse_rate);
        dft_magnitudes(&self.impulse, rate, freqs)
            .into_iter()
            .map(|m| crate::lin_to_db((m / self.gain).max(1.0e-30)))
            .collect()
    }

    /// The summary response plus, when asked, `points` frequencies from
    /// `f_lo_hz` to `f_hi_hz` (inclusive, evenly spaced) evaluated directly.
    /// The caller validates the window; `points` is clamped to
    /// 2 ..= [`RESPONSE_WINDOW_MAX_POINTS`].
    pub fn with_window(&self, window: Option<ResponseWindowRequest>) -> UpsamplerResponse {
        let mut response = self.response.clone();
        response.window = window.map(|w| {
            let points = w.points.clamp(2, RESPONSE_WINDOW_MAX_POINTS) as usize;
            let step = (w.f_hi_hz - w.f_lo_hz) / (points - 1) as f64;
            let freqs_hz: Vec<f64> = (0..points)
                .map(|i| {
                    if i + 1 == points {
                        w.f_hi_hz
                    } else {
                        w.f_lo_hz + step * i as f64
                    }
                })
                .collect();
            let magnitude_db = self.magnitude_db_at(&freqs_hz);
            ResponseWindow {
                f_lo_hz: w.f_lo_hz,
                f_hi_hz: w.f_hi_hz,
                freqs_hz,
                magnitude_db,
            }
        });
        response
    }
}

/// Measure what the engine's own [`Upsampler`] does for `cfg` at
/// `source_rate` (see [`measure_upsampler`]).
pub fn upsampler_response(
    cfg: &UpsamplerConfig,
    source_rate: u32,
) -> Result<UpsamplerResponse, String> {
    measure_upsampler(cfg, source_rate).map(|m| m.response)
}

/// The engine block [`measure_upsampler`] reports the latency for: the
/// [`crate::design::BuildContext`] default.
pub const DEFAULT_MEASURE_MAX_BLOCK: usize = 1024;

/// [`measure_upsampler_at`] for an engine whose largest input block is
/// [`DEFAULT_MEASURE_MAX_BLOCK`].
pub fn measure_upsampler(
    cfg: &UpsamplerConfig,
    source_rate: u32,
) -> Result<MeasuredUpsampler, String> {
    measure_upsampler_at(cfg, source_rate, DEFAULT_MEASURE_MAX_BLOCK)
}

/// Stopband detection threshold (dB) used when measuring a [`FilterPreset`]
/// (presets do not carry a Kaiser beta).
const PRESET_STOPBAND_TARGET_DB: f64 = 150.0;

/// Measure what the engine's own [`Upsampler`] (or a [`FilterPreset`] runtime)
/// does for `cfg` at `source_rate`.
///
/// A full-scale unit impulse is pushed through the runtime; only the actual
/// impulse tail (`impulse_span_out_frames` for presets, or kernel length at the
/// output rate plus block latency for manual designs) is clocked through and
/// FFTed. The transform is sized to the next power of two at or above that
/// length, so zero padding is at most 2x. The FFT gives the stats and brackets
/// the threshold crossings; the plotted points and the crossings themselves
/// are evaluated directly from the impulse, so a zoom window agrees with them
/// exactly. Ratio 1 is reported as flat without building a filter.
///
/// For presets, `taps` reports the measured non-zero impulse span,
/// `stopband_start_hz` uses a fixed `-150 dB` target (`PRESET_STOPBAND_TARGET_DB`)
/// in place of a Kaiser beta, and `filter_window` / `effective_cutoff` /
/// `effective_kaiser_beta` carry neutral placeholders (`Kaiser` / `1.0` / `0.0`).
/// Time-varying block FFT presets (`block_fft*`) give the response for one
/// block position (an impulse at the start of a block).
///
/// `latency_samples` is the delay the live engine adds for this design, in
/// output frames, when its largest input block is `max_block` (the daemon
/// builds its chains with `scaled_quantum(quantum, rate)` clamped to
/// 512..8192): the filter's own delay (linear phase: half the kernel;
/// otherwise its impulse peak) plus the partitioned convolution's block, as
/// `status.filter_latency_ms` counts it for a chain with no other delayed
/// stage. The offline partition block, chosen for speed, does not count.
///
/// Returns an error when the sizes do not fit in `usize` or the preset cannot
/// be built (the control plane validates the config before calling this).
pub fn measure_upsampler_at(
    cfg: &UpsamplerConfig,
    source_rate: u32,
    max_block: usize,
) -> Result<MeasuredUpsampler, String> {
    let max_block = max_block.max(1);
    let source_rate = source_rate.max(1);
    let rate_ratio = resolve_rate_ratio(cfg.target, source_rate);
    if !rate_ratio.is_integer() {
        return measure_rational(cfg, source_rate, rate_ratio);
    }
    let ratio = rate_ratio.up;
    let output_rate = source_rate
        .checked_mul(ratio)
        .ok_or_else(|| format!("output rate for {source_rate} Hz x{ratio} overflows"))?;

    let named = crate::design::Named::of(cfg).map_err(|e| e.to_string())?;
    let standard_manual = if named.preset.is_none()
        && let crate::design::FilterDesign::Kernel(k) = &named.design
        && k.numerics.path == crate::design::ArithmeticPath::Standard
    {
        crate::design::paths::standard::legacy_config(k, source_rate).ok()
    } else {
        None
    };

    if ratio == 1 {
        let freqs = response_freqs(source_rate, output_rate);
        let (taps, filter_window, effective_kaiser_beta) = match &standard_manual {
            Some(m) => (m.taps, m.window, m.kaiser_beta),
            None => (0, WindowKind::default(), 0.0),
        };
        return Ok(MeasuredUpsampler {
            response: UpsamplerResponse {
                source_rate,
                output_rate,
                ratio,
                rate_ratio: RateRatio::ONE,
                taps,
                phase: named.design.filter_phase(),
                filter_window,
                effective_cutoff: 1.0,
                effective_kaiser_beta,
                sample_leak_db: -400.0,
                magnitude_db: vec![0.0; freqs.len()],
                freqs_hz: freqs,
                impulse: vec![1.0],
                impulse_peak: 0,
                passband_ripple_db: 0.0,
                minus_0_1_db_hz: 0.0,
                minus_3_db_hz: 0.0,
                stopband_start_hz: 0.0,
                stopband_atten_db: 0.0,
                latency_samples: 0,
                crossings: Vec::new(),
                window: None,
                preset: named.preset,
                design: None,
                plan: None,
                device_landings: Vec::new(),
            },
            impulse: vec![1.0],
            gain: 1.0,
            impulse_rate: output_rate,
            plan: None,
        });
    }

    let Some(manual) = standard_manual else {
        // Built as the engine builds it, so its latency is the live one.
        let cx = crate::design::BuildContext {
            source_rate,
            ratio: rate_ratio,
            channels: 1,
            max_block,
            pipelines: RATIONAL_SWAP_PIPELINES,
            output: audivane_protocol::OutputConfig::default(),
            workspace: crate::design::budget::design_workspace(),
        };
        let prepared = crate::design::upsampler::prepare(&named, &cx).map_err(|e| e.to_string())?;
        let mut rt = crate::design::build(&prepared, &cx, 0.0)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no upsampler at {rate_ratio}"))?;
        let latency_samples = rt.latency_out_frames();
        let impulse_len = rt.impulse_span_out_frames().max(1);
        let (output, output_len, n_fft) =
            clock_impulse(&mut *rt, ratio as usize, max_block, impulse_len)?;
        drop(rt);

        let phase = named.design.filter_phase();
        return Ok(analyze_measured_impulse(
            output,
            output_len,
            n_fft,
            MeasuredMeta {
                source_rate,
                output_rate,
                ratio,
                taps_override: None,
                phase,
                filter_window: WindowKind::default(),
                effective_cutoff: 1.0,
                effective_kaiser_beta: 0.0,
                target_atten_db: PRESET_STOPBAND_TARGET_DB,
                latency_samples,
                preset: named.preset,
            },
        )
        .with_prepared(cfg, &prepared));
    };

    // The kernel as the engine prepares it at this very ratio (the source
    // rate here need not be one validation saw): the length, workspace
    // and ratio rules, and the safety gate.
    let cx = crate::design::BuildContext {
        source_rate,
        ratio: rate_ratio,
        channels: 1,
        max_block: 1024,
        pipelines: RATIONAL_SWAP_PIPELINES,
        output: audivane_protocol::OutputConfig::default(),
        workspace: crate::design::budget::design_workspace(),
    };
    let prepared = crate::design::upsampler::prepare(&named, &cx).map_err(|e| e.to_string())?;
    let kernel = prepared
        .kernel
        .clone()
        .ok_or_else(|| "the design was not prepared".to_string())?;
    // The resolved design (an automatic length is cached): its metadata
    // and the offline partition block.
    let design =
        UpsamplerDesign::try_from_config(&manual, ratio as usize).map_err(|e| e.to_string())?;
    let block_in = measurement_block(design.taps, ratio as usize);
    let mut up =
        Upsampler::from_kernel_with_block(&kernel.taps, ratio as usize, kernel.delay, 1, block_in);
    let block_latency = up.block_latency_out_frames();
    // The live engine's block, not the offline one: what
    // `Upsampler::from_kernel` with `max_block` delays.
    let latency_samples = crate::design::budget::engine_block(max_block) * ratio as usize
        + (up.latency_out_frames() - block_latency);

    // The response is the kernel tail (`taps` output frames) shifted by the
    // upsampler's block latency. Anything after that is exactly zero, so there
    // is no need to push a fixed multiple of the kernel through.
    let kernel_taps = up.kernel_taps();
    let impulse_len = kernel_taps
        .checked_add(block_latency)
        .ok_or_else(|| format!("{kernel_taps} taps is too large"))?;
    let (output, output_len, n_fft) =
        clock_impulse(&mut up, ratio as usize, block_in, impulse_len)?;
    // Free the partitioned convolver before the response FFT: that is where
    // most of the remaining peak memory would otherwise overlap.
    drop(up);

    let target_atten_db = kaiser_attenuation_db(design.kaiser_beta).clamp(40.0, 230.0);
    Ok(analyze_measured_impulse(
        output,
        output_len,
        n_fft,
        MeasuredMeta {
            source_rate,
            output_rate,
            ratio,
            taps_override: Some(kernel_taps),
            phase: design.phase,
            filter_window: design.window,
            effective_cutoff: design.cutoff,
            effective_kaiser_beta: design.kaiser_beta,
            target_atten_db,
            latency_samples,
            preset: None,
        },
    )
    .with_prepared(cfg, &prepared))
}

fn clock_impulse(
    rt: &mut dyn crate::presets::UpsamplerRuntime,
    ratio: usize,
    block_in: usize,
    impulse_len: usize,
) -> Result<(Vec<f64>, usize, usize), String> {
    let total_in = impulse_len.div_ceil(ratio).max(1);
    let output_len = total_in
        .checked_mul(ratio)
        .ok_or_else(|| format!("{total_in} output frames is too large"))?;
    let n_fft = response_fft_len(impulse_len)?;

    let mut input = vec![0.0; total_in];
    input[0] = 1.0;
    let mut output = vec![0.0; output_len];
    let mut block_out = vec![0.0; block_in * ratio];
    let mut offset = 0;
    while offset < total_in {
        let take = (total_in - offset).min(block_in);
        let out_len = take * ratio;
        rt.process(
            &[&input[offset..offset + take]],
            &mut [&mut block_out[..out_len]],
        );
        output[offset * ratio..(offset + take) * ratio].copy_from_slice(&block_out[..out_len]);
        offset += take;
    }
    Ok((output, output_len, n_fft))
}

struct MeasuredMeta {
    source_rate: u32,
    output_rate: u32,
    ratio: u32,
    taps_override: Option<usize>,
    phase: FilterPhase,
    filter_window: WindowKind,
    effective_cutoff: f64,
    effective_kaiser_beta: f64,
    target_atten_db: f64,
    latency_samples: usize,
    preset: Option<FilterPreset>,
}

fn analyze_measured_impulse(
    mut output: Vec<f64>,
    output_len: usize,
    n_fft: usize,
    meta: MeasuredMeta,
) -> MeasuredUpsampler {
    let nyquist_in = f64::from(meta.source_rate) / 2.0;
    let nyquist_out = f64::from(meta.output_rate) / 2.0;

    // The first of several equal peaks (`max_by` would pick the last), so a
    // flat hold kernel is plotted from its first sample.
    let peak_index = output
        .iter()
        .enumerate()
        .fold((0usize, f64::NEG_INFINITY), |(bi, bv), (i, v)| {
            if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
        })
        .0;
    let mut impulse_start = peak_index.saturating_sub(128);
    let mut impulse_end = (impulse_start + 256).min(output.len());
    if impulse_end - impulse_start < 256 {
        impulse_start = impulse_end.saturating_sub(256);
    }
    // Refresh the end in case the window moved back.
    impulse_end = (impulse_start + 256).min(output.len());
    let impulse = output[impulse_start..impulse_end].to_vec();
    let impulse_peak = peak_index - impulse_start;
    let sample_leak_db = {
        let top = output[peak_index].abs();
        let r = meta.ratio as usize;
        let mut leak = 0.0_f64;
        let mut i = peak_index % r;
        while i < output.len() {
            if i != peak_index {
                leak = leak.max(output[i].abs());
            }
            i += r;
        }
        if top > 0.0 {
            lin_to_db_floor(leak / top)
        } else {
            0.0
        }
    };

    let mut fft = RealFftPair::new(n_fft);
    let mut spec = vec![Complex64::new(0.0, 0.0); fft.spec_len()];
    // `forward_padded_complex` zero-pads to `n_fft`; rounding the input to a
    // whole number of output frames can add a few trailing zeros, so clip to
    // the transform length.
    let used = output_len.min(n_fft);
    fft.forward_padded_complex(&output[..used], &mut spec);
    drop(fft);
    // The kernel sums to the ratio (zero-stuffing divides the spectrum by the
    // ratio), so passband gain is unity after this normalisation.
    let gain = meta.ratio as f64;
    let inv_ratio = 1.0 / gain;
    let mag: Vec<f64> = spec.iter().map(|c| c.norm() * inv_ratio).collect();
    // Record bins where the phase of a linear-phase centered spectrum flips
    // (>= 60 deg) in the transition/stopband: a zero crossing can fall between
    // two FFT samples that are both above a deep threshold.
    let rot = Complex64::from_polar(1.0, -2.0 * PI * peak_index as f64 / n_fft as f64);
    let max_bin = n_fft / 2;
    let bin_hz = f64::from(meta.output_rate) / n_fft as f64;
    let mut phase_flip = vec![0u64; max_bin.div_ceil(64)];
    if meta.phase == FilterPhase::Linear {
        for k in 0..max_bin {
            if mag[k] < 0.5 && mag[k + 1] < 0.5 {
                let prod = spec[k] * spec[k + 1].conj() * rot;
                let cos_thresh = 0.5 * spec[k].norm() * spec[k + 1].norm();
                if prod.re <= cos_thresh {
                    phase_flip[k >> 6] |= 1u64 << (k & 63);
                }
            }
        }
    }
    drop(spec);

    // Keep only the non-zero part of the impulse for direct evaluation.
    let first = output[..used].iter().position(|&x| x != 0.0).unwrap_or(0);
    let last = output[..used]
        .iter()
        .rposition(|&x| x != 0.0)
        .map_or(first, |i| i + 1);
    output.truncate(last);
    output.drain(..first);
    output.shrink_to_fit();
    let kept = output;
    let taps = meta.taps_override.unwrap_or(kept.len());

    // The FFT grid is at or below 0.5 samples per impulse length, so linear
    // interpolation between bins resolves the stats grid below.
    let magnitude_at = |f: f64| -> f64 {
        let x = (f / f64::from(meta.output_rate) * n_fft as f64).clamp(0.0, max_bin as f64);
        let k = x.floor() as usize;
        let frac = x - k as f64;
        let k2 = (k + 1).min(max_bin);
        mag[k] * (1.0 - frac) + mag[k2] * frac
    };

    // Stats on a dense linear grid so the transition is resolved well.
    const STAT_POINTS: usize = 16_384;
    let stat_freq = |i: usize| -> f64 {
        if i == 0 {
            0.0
        } else {
            nyquist_out * (i as f64) / (STAT_POINTS as f64)
        }
    };
    let stat_mag: Vec<f64> = (0..=STAT_POINTS)
        .map(|i| magnitude_at(stat_freq(i)))
        .collect();
    let stat_db = |i: usize| -> f64 { crate::lin_to_db(stat_mag[i].max(1.0e-30)) };

    let passband_last = ((0.9 * nyquist_in / nyquist_out) * STAT_POINTS as f64) as usize;
    let (mut min_db, mut max_db) = (f64::INFINITY, f64::NEG_INFINITY);
    for i in 1..=passband_last.min(STAT_POINTS) {
        let db = stat_db(i);
        min_db = min_db.min(db);
        max_db = max_db.max(db);
    }
    let passband_ripple_db = if min_db.is_finite() {
        max_db - min_db
    } else {
        0.0
    };

    // Contract K: the stopband starts above the source Nyquist. The transition
    // band is still descending there, so start at the first frequency after
    // which the response never rises above the design's own target
    // attenuation (Kaiser beta -> stopband attenuation). The reported
    // attenuation is then the worst level from that frequency to the output
    // Nyquist, i.e. all points at or after it stay at or below -attenuation.
    // Both come from certified bounds on the continuous response, not from
    // grid samples (see `stopband_summary`).
    let rate = f64::from(meta.output_rate);
    let summary = stopband_summary(
        &kept,
        rate,
        gain,
        n_fft,
        &mag,
        nyquist_in,
        meta.target_atten_db,
    );
    let stopband_start_hz = summary.start_hz;

    // The plotted points, evaluated directly from the impulse.
    let freqs_hz = response_freqs(meta.source_rate, meta.output_rate);
    let magnitude_db: Vec<f64> = dft_magnitudes(&kept, rate, &freqs_hz)
        .into_iter()
        .map(|m| crate::lin_to_db((m / gain).max(1.0e-30)))
        .collect();
    // The plotted points are exact values, so they never exceed the bound;
    // include them anyway so the summary can never contradict the plot.
    let worst_plotted = freqs_hz
        .iter()
        .zip(&magnitude_db)
        .filter(|(f, _)| **f >= stopband_start_hz)
        .map(|(_, db)| *db)
        .fold(f64::NEG_INFINITY, f64::max);
    let stopband_atten_db = (-summary.peak_db.max(worst_plotted)).max(0.0);

    // Threshold crossings: adaptive bracketing from the FFT grid (with phase
    // flip and local-dip detection so intervening ripple troughs and narrow
    // nulls are subdivided with direct evaluation), followed by batched
    // bisection on the direct evaluation.
    let active_levels: Vec<f64> = RESPONSE_CROSSING_LEVELS_DB
        .iter()
        .copied()
        // The -0.1 and -3 dB points are always reported: a shallow
        // stopband (e.g. a block FFT preset judged against the fixed preset
        // target) must not hide them.
        .filter(|&level| {
            level == -0.1
                || level == -3.0
                || level >= -(stopband_atten_db - CROSSING_STOPBAND_MARGIN_DB)
        })
        .collect();
    let crossings =
        find_earliest_crossings(&kept, rate, gain, &mag, &phase_flip, bin_hz, &active_levels);
    drop(mag);
    drop(phase_flip);
    let crossing_at = |level: f64| {
        crossings
            .iter()
            .find(|c| c.level_db == level)
            .map_or(0.0, |c| c.freq_hz)
    };
    let minus_0_1_db_hz = crossing_at(-0.1);
    let minus_3_db_hz = crossing_at(-3.0);

    MeasuredUpsampler {
        response: UpsamplerResponse {
            source_rate: meta.source_rate,
            output_rate: meta.output_rate,
            ratio: meta.ratio,
            rate_ratio: RateRatio::integer(meta.ratio),
            taps,
            phase: meta.phase,
            filter_window: meta.filter_window,
            effective_cutoff: meta.effective_cutoff,
            effective_kaiser_beta: meta.effective_kaiser_beta,
            sample_leak_db,
            freqs_hz,
            magnitude_db,
            impulse,
            impulse_peak,
            passband_ripple_db,
            minus_0_1_db_hz,
            minus_3_db_hz,
            stopband_start_hz,
            stopband_atten_db,
            latency_samples: meta.latency_samples,
            crossings,
            window: None,
            preset: meta.preset,
            design: None,
            plan: None,
            device_landings: Vec::new(),
        },
        impulse: kept,
        gain,
        impulse_rate: meta.output_rate,
        plan: None,
    }
}

/// [`measure_upsampler`] for a rational ratio: the response of the kernel on
/// the intermediate grid (rate `source x up`), which every output phase
/// samples. The band edge is the lower Nyquist; the stopband runs to the
/// grid's Nyquist, because everything there is folded into the output band
/// by the decimation. The latency is the kernel delay in output frames.
fn measure_rational(
    cfg: &UpsamplerConfig,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<MeasuredUpsampler, String> {
    let output_rate = ratio
        .output_rate(source_rate)
        .ok_or_else(|| format!("{source_rate} Hz x {ratio} is not a whole output rate"))?;
    let grid_rate = source_rate
        .checked_mul(ratio.up)
        .ok_or_else(|| format!("the {ratio} grid rate from {source_rate} Hz overflows"))?;
    let band_rate = source_rate.min(output_rate);
    let up = ratio.up;

    struct Built {
        kernel: Vec<f64>,
        delay: f64,
        phase: FilterPhase,
        window: WindowKind,
        cutoff: f64,
        beta: f64,
        target_db: f64,
        taps: Option<usize>,
    }
    let named = crate::design::Named::of(cfg).map_err(|e| e.to_string())?;
    let standard_manual = if named.preset.is_none()
        && let crate::design::FilterDesign::Kernel(k) = &named.design
        && k.numerics.path == crate::design::ArithmeticPath::Standard
    {
        crate::design::paths::standard::legacy_config(k, source_rate).ok()
    } else {
        None
    };
    let cx = crate::design::BuildContext {
        source_rate,
        ratio,
        channels: 1,
        max_block: 1024,
        pipelines: 1,
        output: audivane_protocol::OutputConfig::default(),
        workspace: crate::design::budget::design_workspace(),
    };
    let prepared = crate::design::upsampler::prepare(&named, &cx).map_err(|e| e.to_string())?;
    let b = if let Some(manual) = standard_manual {
        let k = prepared
            .kernel
            .clone()
            .ok_or_else(|| "the design was not prepared".to_string())?;
        let design = UpsamplerDesign::try_for_ratio(&manual, ratio).map_err(|e| e.to_string())?;
        let (kernel, delay) = (k.taps, k.delay_exact);
        let taps = kernel.len();
        Built {
            kernel,
            delay,
            phase: design.phase,
            window: design.window,
            cutoff: design.cutoff,
            beta: design.kaiser_beta,
            target_db: kaiser_attenuation_db(design.kaiser_beta).clamp(40.0, 230.0),
            taps: Some(taps),
        }
    } else {
        let phase = named.design.filter_phase();
        let k = prepared
            .kernel
            .clone()
            .ok_or_else(|| "the design has no single kernel".to_string())?;
        Built {
            kernel: k.taps,
            delay: k.delay as f64,
            phase,
            window: WindowKind::default(),
            cutoff: 1.0,
            beta: 0.0,
            target_db: PRESET_STOPBAND_TARGET_DB,
            taps: None,
        }
    };
    let len = b.kernel.len().max(1);
    let n_fft = response_fft_len(len)?;
    let mut m = analyze_measured_impulse(
        b.kernel,
        len,
        n_fft,
        MeasuredMeta {
            source_rate: band_rate,
            output_rate: grid_rate,
            ratio: up,
            taps_override: b.taps,
            phase: b.phase,
            filter_window: b.window,
            effective_cutoff: b.cutoff,
            effective_kaiser_beta: b.beta,
            target_atten_db: b.target_db,
            latency_samples: (b.delay / f64::from(ratio.down) + 0.5).floor() as usize,
            preset: named.preset,
        },
    );
    m.response.source_rate = source_rate;
    m.response.output_rate = output_rate;
    m.response.ratio = 0;
    m.response.rate_ratio = ratio;
    if up == 1 {
        // Decimation: the output keeps no input instants, so a leak at them
        // means nothing.
        m.response.sample_leak_db = 0.0;
    }
    Ok(m.with_prepared(cfg, &prepared))
}

/// The stopband part of the response summary: where it starts and an upper
/// bound on the continuous response from there to the output Nyquist.
#[derive(Clone, Copy, Debug)]
struct StopbandSummary {
    /// First frequency after which the response is certified at or below the
    /// target (the start bound itself, when it never gets there).
    start_hz: f64,
    /// Upper bound on `|H|` from `start_hz` to the output Nyquist, dB re the
    /// passband.
    peak_db: f64,
}

/// Intervals whose bound could exceed the best level found by more than this
/// are refined, so the certified stopband peak is at most about this far
/// above the true one.
pub const STOPBAND_PEAK_TOLERANCE_DB: f64 = 0.01;

/// Truncation terms of the bounds (Taylor remainder, interpolation error)
/// stay this far under the level they certify: the target for the start,
/// the peak found for the peak (0.0017 dB).
const STOPBAND_BOUND_FLOOR_DB: f64 = 80.0;

/// Truncation terms of the start bound stay this far under the target.
const STOPBAND_START_FLOOR_DB: f64 = 60.0;

/// Cells per refined bin interval (the polynomial bound is taken per cell).
const STOPBAND_CELLS: usize = 64;

/// Upper limit on refined bin intervals; past it the remaining intervals keep
/// their (still valid, looser) Taylor bound.
const STOPBAND_MAX_REFINED: usize = 8192;

/// One refined bin interval: bounds on cells of `[lo, hi]` Hz and the values
/// actually evaluated in it.
struct Refined {
    lo: f64,
    hi: f64,
    /// Upper bound on `|H| / gain` over each of [`STOPBAND_CELLS`] equal cells.
    cells: Vec<f64>,
    /// Exact `(f, |H| / gain)` at the interpolation nodes.
    nodes: Vec<(f64, f64)>,
    /// Interpolation error bound included in `cells`.
    error: f64,
}

impl Refined {
    fn cell(&self, i: usize) -> (f64, f64) {
        let w = (self.hi - self.lo) / self.cells.len() as f64;
        (self.lo + w * i as f64, self.lo + w * (i + 1) as f64)
    }
}

/// Chebyshev interpolation of `Hc(f) = e^{j 2 pi f c / rate} H(f)`
/// (`c` = centre of `h`) on each `[lo, hi]` from `m` first-kind nodes,
/// evaluated directly, and a rigorous bound per cell:
///
/// * `|Hc^(m)| <= (M/2)^m sum |h|` for an `M`-tap `h`, so interpolation from
///   `m` Chebyshev nodes on a half-width of `rad` radians is within
///   `E = 2 sqrt(2) sum |h| (s/2)^m / m!`, `s = rad M / 2`, of `Hc`
///   (real and imaginary parts separately); `m` is the smallest with
///   `E <= floor`;
/// * on a cell of half-width `eta` (in the interval's `[-1, 1]` variable)
///   around `t`, `|P(t + x)|^2 <= sum_q |e_q| eta^q` with `e_q` the Taylor
///   coefficients of `|P|^2` at `t` (exact: `P` is a polynomial).
///
/// The bound is exact arithmetic; f64 rounding of the direct sums (below
/// -280 dB re the passband) is not in it.
fn refine_intervals(
    h: &[f64],
    rate: f64,
    gain: f64,
    spans: &[(f64, f64)],
    floor: f64,
) -> Vec<Refined> {
    let len = h.len().max(1);
    let center = (len - 1) as f64 / 2.0;
    let half_len = center.max(0.5);
    let abs_sum: f64 = h.iter().map(|x| x.abs()).sum::<f64>() / gain;
    let interp_error = |s: f64, m: usize| {
        let mut t = 2.0 * std::f64::consts::SQRT_2 * abs_sum;
        for k in 1..=m {
            t *= s / 2.0 / k as f64;
        }
        t
    };
    // Node count from the widest span.
    let widest = spans
        .iter()
        .map(|&(lo, hi)| hi - lo)
        .fold(0.0_f64, f64::max);
    let s_widest = half_len * PI * widest / rate;
    let mut m = 4;
    while m < 40 && interp_error(s_widest, m) > floor {
        m += 1;
    }
    let nodes: Vec<f64> = (0..m)
        .map(|i| ((2 * i + 1) as f64 * PI / (2 * m) as f64).cos())
        .collect();
    let freqs: Vec<f64> = spans
        .iter()
        .flat_map(|&(lo, hi)| {
            let (mid, hw) = (0.5 * (lo + hi), 0.5 * (hi - lo));
            nodes.iter().map(move |t| mid + hw * t)
        })
        .collect();
    let values = dft_complex(h, rate, &freqs);
    let zero = Complex64::new(0.0, 0.0);
    spans
        .iter()
        .enumerate()
        .map(|(idx, &(lo, hi))| {
            let error = interp_error(half_len * PI * (hi - lo) / rate, m);
            // Centred, normalised node values.
            let v: Vec<Complex64> = (0..m)
                .map(|i| {
                    let cyc = freqs[idx * m + i] / rate;
                    let p_hi = cyc * center;
                    let p_lo = cyc.mul_add(center, -p_hi);
                    let rot = Complex64::from_polar(1.0, 2.0 * PI * (frac(p_hi) + p_lo));
                    values[idx * m + i] * rot / gain
                })
                .collect();
            let nodes = (0..m).map(|i| (freqs[idx * m + i], v[i].norm())).collect();
            // Chebyshev coefficients a_k, P(t) = sum a_k T_k(t).
            let mut a: Vec<Complex64> = (0..m)
                .map(|k| {
                    let mut acc = zero;
                    for (i, vi) in v.iter().enumerate() {
                        acc += vi * ((k * (2 * i + 1)) as f64 * PI / (2 * m) as f64).cos();
                    }
                    acc * (2.0 / m as f64)
                })
                .collect();
            a[0] *= 0.5;
            // ders[j]: Chebyshev coefficients of P^(j) / j!.
            let mut ders = vec![a];
            for j in 1..m {
                let prev = &ders[j - 1];
                let deg = prev.len() - 1;
                if deg == 0 {
                    break;
                }
                let mut b = vec![zero; deg + 1];
                for k in (1..=deg).rev() {
                    b[k - 1] = b.get(k + 1).copied().unwrap_or(zero) + prev[k] * (2.0 * k as f64);
                }
                b[0] *= 0.5;
                b.truncate(deg);
                for c in &mut b {
                    *c /= j as f64;
                }
                ders.push(b);
            }
            let clenshaw = |c: &[Complex64], t: f64| -> Complex64 {
                let (mut b1, mut b2) = (zero, zero);
                for &ck in c.iter().skip(1).rev() {
                    let b0 = ck + b1 * (2.0 * t) - b2;
                    b2 = b1;
                    b1 = b0;
                }
                c[0] + b1 * t - b2
            };
            let eta = 1.0 / STOPBAND_CELLS as f64;
            let cells = (0..STOPBAND_CELLS)
                .map(|g| {
                    let t = -1.0 + (2 * g + 1) as f64 * eta;
                    let d: Vec<Complex64> = ders.iter().map(|c| clenshaw(c, t)).collect();
                    let mut bound = 0.0;
                    let mut power = 1.0;
                    for q in 0..(2 * d.len() - 1) {
                        let mut eq = 0.0;
                        for i in q.saturating_sub(d.len() - 1)..=q.min(d.len() - 1) {
                            eq += (d[i] * d[q - i].conj()).re;
                        }
                        bound += if q == 0 { eq } else { eq.abs() * power };
                        power *= eta;
                    }
                    bound.max(0.0).sqrt() + error
                })
                .collect();
            Refined {
                lo,
                hi,
                cells,
                nodes,
                error,
            }
        })
        .collect()
}

/// Per-bin Taylor bounds of [`stopband_summary`], extended term by term.
struct TaylorBound {
    /// Partial sums `sum_{j<p} |Hc^(j)(f_k)| r^j / j!` (normalised), bins
    /// `k_lo ..`.
    partial: Vec<f64>,
    /// `((n - c) / (M/2))^(p-1) h[n]`.
    buf: Vec<f64>,
    p: usize,
    /// `s^(p-1) / (p-1)!`.
    coef: f64,
    s: f64,
    center: f64,
    half_len: f64,
    abs_sum: f64,
    gain: f64,
    k_lo: usize,
    /// Forward plan, time buffer, spectrum and scratch, made on first use.
    fft: Option<TaylorFft>,
    n_fft: usize,
}

struct TaylorFft {
    plan: std::sync::Arc<dyn realfft::RealToComplex<f64>>,
    time: Vec<f64>,
    spec: Vec<Complex64>,
    scratch: Vec<Complex64>,
}

impl TaylorBound {
    /// Remainder `s^p / p! sum |h|` of the current partial sums.
    fn remainder(&self) -> f64 {
        self.coef * self.s / self.p as f64 * self.abs_sum
    }

    /// Add terms until the remainder is at most `floor` (at most 40 terms).
    fn extend(&mut self, floor: f64) {
        while self.p < 40 && self.remainder() > floor {
            let n_fft = self.n_fft;
            let fft = self.fft.get_or_insert_with(|| {
                let plan = realfft::RealFftPlanner::<f64>::new().plan_fft_forward(n_fft);
                let scratch = plan.make_scratch_vec();
                TaylorFft {
                    plan,
                    time: vec![0.0; n_fft],
                    spec: vec![Complex64::new(0.0, 0.0); n_fft / 2 + 1],
                    scratch,
                }
            });
            for (n, v) in self.buf.iter_mut().enumerate() {
                *v *= (n as f64 - self.center) / self.half_len;
            }
            fft.time.fill(0.0);
            fft.time[..self.buf.len()].copy_from_slice(&self.buf);
            fft.plan
                .process_with_scratch(&mut fft.time, &mut fft.spec, &mut fft.scratch)
                .expect("stopband bound FFT");
            let spec = &fft.spec;
            self.coef *= self.s / self.p as f64;
            let w = self.coef / self.gain;
            for (u, c) in self.partial.iter_mut().zip(&spec[self.k_lo..]) {
                *u += c.norm() * w;
            }
            self.p += 1;
        }
    }

    /// Bound on `|H| / gain` over bin `k`'s interval.
    fn at(&self, k: usize) -> f64 {
        self.partial[k - self.k_lo] + self.remainder()
    }
}

/// Certified stopband start and peak of the impulse `h` (rate `rate`,
/// passband gain `gain`), from its `n_fft`-point FFT magnitude `mag`
/// (normalised by `gain`), searched from `from_hz` (the source Nyquist) up
/// for the target attenuation `target_db`.
///
/// Sampled maxima of an FFT miss lobes narrower than a bin (the first lobe
/// after a Kaiser transition can be), so every point of the band gets a
/// bound instead:
///
/// 1. Every bin's interval (one bin wide, centred on the bin) gets the
///    Taylor bound `sum_{j<p} |Hc^(j)(f_k)| r^j / j! + (M r / 2)^p / p!
///    sum |h|` (`r` half a bin in radians, `Hc` the transform about the
///    centre of `h`), from `p - 1` extra FFTs of `((n - c) / (M/2))^j h[n]`.
/// 2. Intervals whose bound could exceed the target, or the best level found
///    by more than [`STOPBAND_PEAK_TOLERANCE_DB`], get the cell bounds of
///    [`refine_intervals`], until none is left.
///
/// The truncation terms stay [`STOPBAND_START_FLOOR_DB`] under the target for
/// the start and [`STOPBAND_BOUND_FLOOR_DB`] under the peak found for the
/// peak, but not below 20 dB under the evaluation floor `u sqrt(M) |h|_2`
/// (the rms f64 rounding of one M-term sum). So the returned peak is an upper
/// bound on the continuous response from the start to the output Nyquist, and
/// above its true value by at most `STOPBAND_PEAK_TOLERANCE_DB` + 0.002 dB,
/// or by the evaluation floor (absolute), whichever is larger. That is exact
/// arithmetic; the f64 rounding of the evaluations comes on top. The start
/// is the end of the last cell whose bound exceeds the target (or of the
/// last sample above it).
fn stopband_summary(
    h: &[f64],
    rate: f64,
    gain: f64,
    n_fft: usize,
    mag: &[f64],
    from_hz: f64,
    target_db: f64,
) -> StopbandSummary {
    let nyquist = rate / 2.0;
    let max_bin = mag.len() - 1;
    let bin_hz = rate / n_fft as f64;
    let target = crate::db_to_lin(-target_db);
    let below = crate::db_to_lin(-STOPBAND_BOUND_FLOOR_DB);
    let tol = crate::db_to_lin(STOPBAND_PEAK_TOLERANCE_DB);
    let freq = |k: usize| k as f64 * bin_hz;
    let k_lo = (((from_hz / bin_hz) - 0.5).ceil().max(0.0) as usize).min(max_bin);
    let span = |k: usize| {
        let lo = (freq(k) - 0.5 * bin_hz).max(from_hz);
        let hi = (freq(k) + 0.5 * bin_hz).min(nyquist);
        (lo, hi.max(lo))
    };

    // 1. Taylor bound per bin interval, first to the target's floor.
    let len = h.len().max(1);
    let center = (len - 1) as f64 / 2.0;
    let half_len = center.max(0.5);
    let abs_sum: f64 = h.iter().map(|x| x.abs()).sum::<f64>() / gain;
    // The f64 evaluations themselves are only good to about the rms rounding
    // error of an M-term sum, `u sqrt(M) |h|_2` (-265 dB re the passband at
    // 4M taps, -300 dB at 1k): no interval is refined to resolve differences
    // below it, and truncation terms stop 20 dB under it.
    let l2: f64 = h.iter().map(|x| x * x).sum::<f64>().sqrt() / gain;
    let eval_floor = f64::EPSILON / 2.0 * (len as f64).sqrt() * l2;
    let rounding = eval_floor * 0.1;
    let mut taylor = TaylorBound {
        partial: mag[k_lo..].to_vec(),
        buf: h.to_vec(),
        p: 1,
        coef: 1.0,
        s: half_len * PI / n_fft as f64,
        center,
        half_len,
        abs_sum,
        gain,
        k_lo,
        fft: None,
        n_fft,
    };
    let mut floor = (target * crate::db_to_lin(-STOPBAND_START_FLOOR_DB)).max(rounding);
    taylor.extend(floor);

    let mut refined: Vec<Option<Refined>> = (k_lo..=max_bin).map(|_| None).collect();
    let refine = |todo: Vec<usize>, refined: &mut Vec<Option<Refined>>, floor: f64| {
        let done = refined.iter().filter(|r| r.is_some()).count();
        let todo: Vec<usize> = todo
            .into_iter()
            .take(STOPBAND_MAX_REFINED.saturating_sub(done))
            .collect();
        let spans: Vec<(f64, f64)> = todo.iter().map(|&k| span(k)).collect();
        let out = refine_intervals(h, rate, gain, &spans, floor);
        for (k, r) in todo.into_iter().zip(out) {
            refined[k - k_lo] = Some(r);
        }
    };

    // 2a. The start. A sample above the target proves the response exceeds it
    // there; after the last such sample, every interval whose bound exceeds
    // the target is refined, and the start is the end of the last cell that
    // still might.
    let proven = (k_lo..=max_bin)
        .rev()
        .find(|&k| freq(k) >= from_hz && mag[k] > target);
    let resolve_from = proven.unwrap_or(k_lo);
    let todo: Vec<usize> = (resolve_from..=max_bin)
        .filter(|&k| Some(k) == proven || taylor.at(k) > target)
        .collect();
    refine(todo, &mut refined, floor);
    let mut start = proven.map_or(from_hz, freq);
    for k in resolve_from..=max_bin {
        match &refined[k - k_lo] {
            Some(r) => {
                for (c, &b) in r.cells.iter().enumerate() {
                    if b > target {
                        start = start.max(r.cell(c).1);
                    }
                }
            }
            // Not refined (limit reached): its whole span stays suspect.
            None if taylor.at(k) > target => start = start.max(span(k).1),
            None => {}
        }
    }
    if start >= nyquist {
        // Never certified under the target: report the worst level from the
        // source Nyquist up.
        start = from_hz;
    }

    // 2b. The peak from `start` up: tighten the truncation terms to the level
    // found there, then refine every interval that could beat the best level
    // found so far by more than the tolerance.
    let after = |k: usize| span(k).1 > start;
    let lower_bound = |refined: &[Option<Refined>]| {
        let mut lower = 0.0_f64;
        for k in k_lo..=max_bin {
            if freq(k) >= start {
                lower = lower.max(mag[k]);
            }
            if let Some(r) = &refined[k - k_lo] {
                for &(f, v) in &r.nodes {
                    if f >= start {
                        lower = lower.max(v);
                    }
                }
            }
        }
        lower
    };
    loop {
        let lower = lower_bound(&refined);
        let peak_floor = (lower.min(target) * below).max(rounding);
        if peak_floor < floor {
            floor = peak_floor;
            taylor.extend(floor);
            // Earlier refinements to a looser floor are redone.
            let redo: Vec<usize> = (k_lo..=max_bin)
                .filter(|&k| {
                    after(k) && refined[k - k_lo].as_ref().is_some_and(|r| r.error > floor)
                })
                .collect();
            for &k in &redo {
                refined[k - k_lo] = None;
            }
            refine(redo, &mut refined, floor);
            continue;
        }
        let todo: Vec<usize> = (k_lo..=max_bin)
            .filter(|&k| {
                after(k) && refined[k - k_lo].is_none() && taylor.at(k) > lower * tol + eval_floor
            })
            .collect();
        let done = refined.iter().filter(|r| r.is_some()).count();
        if todo.is_empty() || done >= STOPBAND_MAX_REFINED {
            break;
        }
        refine(todo, &mut refined, floor);
    }
    let mut peak = 0.0_f64;
    for k in (k_lo..=max_bin).filter(|&k| after(k)) {
        match &refined[k - k_lo] {
            Some(r) => {
                for (c, &b) in r.cells.iter().enumerate() {
                    if r.cell(c).1 > start {
                        peak = peak.max(b);
                    }
                }
            }
            None => peak = peak.max(taylor.at(k)),
        }
    }
    StopbandSummary {
        start_hz: start,
        peak_db: crate::lin_to_db(peak.max(1.0e-30)),
    }
}

/// Find the earliest crossing for each requested dB level (assumed descending
/// in dB) using adaptive bracketing on the FFT grid (`mag` + `phase_flip` with
/// bin spacing `bin_hz`), bounded iterative subdivision of candidate intervals
/// via direct DFT evaluation, and batched bisection.
fn find_earliest_crossings(
    h: &[f64],
    rate: f64,
    gain: f64,
    mag: &[f64],
    phase_flip: &[u64],
    bin_hz: f64,
    levels: &[f64],
) -> Vec<ResponseCrossing> {
    const N_SUB: usize = 16;
    const INTERIOR: usize = N_SUB - 1;
    const BATCH_INTERVALS: usize = 32;

    let max_bin = mag.len().saturating_sub(1);
    if max_bin == 0 || levels.is_empty() {
        return Vec::new();
    }

    let is_phase_flip = |k: usize| -> bool { (phase_flip[k >> 6] >> (k & 63)) & 1 != 0 };

    // Because `levels` are strictly descending, each crossing frequency is >=
    // the previous crossing frequency; advancing `scan_start_bin` skips the
    // already-cleared passband/transition for all deeper levels.
    let mut scan_start_bin = 0usize;
    let mut scan_start_hz = 0.0_f64;
    let mut brackets: Vec<(f64, f64, f64)> = Vec::with_capacity(levels.len());

    for &level in levels {
        let threshold = crate::db_to_lin(level);
        let threshold_raw = threshold * gain;
        let first_search = scan_start_bin.max(1);
        let k_first_opt = (first_search..=max_bin).find(|&k| mag[k] <= threshold);
        let scan_end = k_first_opt.unwrap_or(max_bin);

        let mut candidates: Vec<usize> = Vec::new();
        for k in scan_start_bin..scan_end {
            let is_direct_bracket = k_first_opt.is_some_and(|kf| k + 1 == kf);
            if is_direct_bracket {
                candidates.push(k);
                continue;
            }
            let y0 = if k == 0 { mag[1] } else { mag[k - 1] };
            let y1 = mag[k];
            let y2 = mag[k + 1];
            let y3 = if k + 2 <= max_bin {
                mag[k + 2]
            } else {
                mag[max_bin - 1]
            };
            let y_min = y1.min(y2);
            let valley = y0 > y1 || y3 > y2;
            let is_candidate = if threshold >= 0.5 {
                let curv = (y0 - 2.0 * y1 + y2).max(y1 - 2.0 * y2 + y3).max(0.0);
                curv > 0.0 && y_min - 2.0 * curv <= threshold
            } else {
                is_phase_flip(k)
                    || (k - scan_start_bin < 32 && y_min <= threshold * 1024.0)
                    || (valley && y_min <= threshold * 64.0)
                    || y_min <= threshold * 16.0
            };
            if is_candidate {
                candidates.push(k);
            }
        }

        let mut found: Option<(f64, f64)> = None;
        'batches: for chunk in candidates.chunks(BATCH_INTERVALS) {
            let mut batch_freqs = Vec::with_capacity(chunk.len() * INTERIOR);
            for &k in chunk {
                let f_lo = (k as f64 * bin_hz).max(scan_start_hz);
                let f_hi = ((k + 1) as f64 * bin_hz).max(f_lo);
                let sub_step = (f_hi - f_lo) / N_SUB as f64;
                for j in 1..N_SUB {
                    batch_freqs.push(f_lo + sub_step * j as f64);
                }
            }
            let batch_mags = dft_magnitudes(h, rate, &batch_freqs);

            // Track candidate sub-intervals from this batch in case a very
            // narrow null (< bin_hz / 16 wide) falls between sub-samples.
            let mut narrow_dips: Vec<(f64, f64, f64, f64)> = Vec::new();

            for (idx, &k) in chunk.iter().enumerate() {
                let f_lo_full = k as f64 * bin_hz;
                let f_lo = f_lo_full.max(scan_start_hz);
                let f_hi = ((k + 1) as f64 * bin_hz).max(f_lo);
                let sub_step = (f_hi - f_lo) / N_SUB as f64;
                let slice = &batch_mags[idx * INTERIOR..(idx + 1) * INTERIOR];

                let mut vals = [0.0_f64; N_SUB + 1];
                vals[0] = if f_lo == f_lo_full {
                    mag[k] * gain
                } else {
                    dft_magnitudes(h, rate, &[f_lo])[0]
                };
                vals[1..N_SUB].copy_from_slice(slice);
                vals[N_SUB] = mag[k + 1] * gain;

                if vals[0] <= threshold_raw {
                    found = Some((f_lo, f_lo));
                    break 'batches;
                }
                for j in 0..N_SUB {
                    let fa = f_lo + sub_step * j as f64;
                    let fb = if j + 1 == N_SUB {
                        f_hi
                    } else {
                        f_lo + sub_step * (j + 1) as f64
                    };
                    if vals[j + 1] <= threshold_raw {
                        found = Some((fa, fb));
                        break 'batches;
                    }
                    if sub_step > 1.0e-5 {
                        let a = vals[j];
                        let b = vals[j + 1];
                        let prev_m = if j > 0 { vals[j - 1] } else { a };
                        let next_m = if j + 2 <= N_SUB { vals[j + 2] } else { b };
                        // At a local minimum inside `(fa, fb)` (excluding
                        // strictly interior monotone segments), check whether an
                        // intervening dip could drop below `threshold_raw`.
                        let strict_mono = (j > 0 && j + 2 <= N_SUB)
                            && ((prev_m > a && a > b && b > next_m)
                                || (prev_m < a && a < b && b < next_m));
                        if !strict_mono {
                            let curv = (prev_m - 2.0 * a + b).max(a - 2.0 * b + next_m).max(0.0);
                            let may_dip = if threshold >= 0.5 {
                                curv > 0.0 && a.min(b) - 2.0 * curv <= threshold_raw
                            } else {
                                a.min(b) <= threshold_raw * 32.0
                                    || (j == 0 && k_first_opt.is_some_and(|kf| k + 1 == kf))
                            };
                            if may_dip {
                                narrow_dips.push((fa, fb, a, b));
                            }
                        }
                    }
                }
                // Check narrow dips discovered inside this chunk before moving
                // to later chunks, so the earliest crossing wins.
                if !narrow_dips.is_empty() {
                    if let Some(bracket) = refine_narrow_dips(h, rate, threshold_raw, &narrow_dips)
                    {
                        found = Some(bracket);
                        break 'batches;
                    }
                    narrow_dips.clear();
                }
            }
        }

        let bracket = if let Some((lo, hi)) = found {
            (lo, hi)
        } else if let Some(kf) = k_first_opt {
            (
                ((kf - 1) as f64 * bin_hz).max(scan_start_hz),
                kf as f64 * bin_hz,
            )
        } else {
            continue;
        };

        // Refine this bracket via batched 16-way search so the next deeper
        // level starts tightly at the true earliest crossing.
        let mut lo = bracket.0;
        let mut hi = bracket.1;
        for _ in 0..8 {
            if hi - lo <= 1.0e-5 {
                break;
            }
            let step = (hi - lo) / N_SUB as f64;
            let freqs: Vec<f64> = (1..N_SUB).map(|j| lo + step * j as f64).collect();
            let vals = dft_magnitudes(h, rate, &freqs);
            if let Some(j) = vals.iter().position(|&v| v <= threshold_raw) {
                lo += step * j as f64;
                hi = lo + step;
            } else {
                lo = hi - step;
            }
        }
        let crossing_hz = 0.5 * (lo + hi);
        scan_start_hz = lo;
        scan_start_bin = (lo / bin_hz).floor() as usize;
        brackets.push((level, crossing_hz, hi));
    }

    brackets
        .into_iter()
        .map(|(level_db, freq_hz, _)| ResponseCrossing { level_db, freq_hz })
        .collect()
}

/// Iteratively narrow a small set of sub-intervals `(fa, fb, ma, mb)` in
/// left-to-right order to catch ultra-narrow nulls without recursive blowup.
fn refine_narrow_dips(
    h: &[f64],
    rate: f64,
    threshold_raw: f64,
    initial: &[(f64, f64, f64, f64)],
) -> Option<(f64, f64)> {
    const N_SUB: usize = 8;
    const INTERIOR: usize = N_SUB - 1;
    const MAX_ACTIVE: usize = 16;
    const MAX_ROUNDS: usize = 4;

    let mut active: Vec<(f64, f64, f64, f64)> = initial.iter().copied().take(MAX_ACTIVE).collect();
    for _round in 0..MAX_ROUNDS {
        if active.is_empty() {
            return None;
        }
        let mut freqs = Vec::with_capacity(active.len() * INTERIOR);
        for &(fa, fb, _, _) in &active {
            let step = (fb - fa) / N_SUB as f64;
            for j in 1..N_SUB {
                freqs.push(fa + step * j as f64);
            }
        }
        let mags = dft_magnitudes(h, rate, &freqs);
        let mut next_active = Vec::new();
        for (idx, &(fa, fb, ma, mb)) in active.iter().enumerate() {
            let step = (fb - fa) / N_SUB as f64;
            let slice = &mags[idx * INTERIOR..(idx + 1) * INTERIOR];
            let mut vals = [0.0_f64; N_SUB + 1];
            vals[0] = ma;
            vals[1..N_SUB].copy_from_slice(slice);
            vals[N_SUB] = mb;

            for j in 0..N_SUB {
                let sub_a = fa + step * j as f64;
                let sub_b = if j + 1 == N_SUB {
                    fb
                } else {
                    fa + step * (j + 1) as f64
                };
                if vals[j + 1] <= threshold_raw {
                    return Some((sub_a, sub_b));
                }
                if step > 1.0e-5 && next_active.len() < MAX_ACTIVE {
                    let prev_m = if j > 0 { vals[j - 1] } else { vals[j] };
                    let a = vals[j];
                    let b = vals[j + 1];
                    let next_m = if j + 2 <= N_SUB {
                        vals[j + 2]
                    } else {
                        vals[j + 1]
                    };
                    let strict_mono = (j > 0 && j + 2 <= N_SUB)
                        && ((prev_m > a && a > b && b > next_m)
                            || (prev_m < a && a < b && b < next_m));
                    if !strict_mono && a.min(b) <= threshold_raw * 32.0 {
                        next_active.push((sub_a, sub_b, a, b));
                    }
                }
            }
        }
        active = next_active;
    }
    None
}

fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn design_polyphase_row(
    alpha: f64,
    half_width: usize,
    beta: f64,
    i0_beta: f64,
    fc: f64,
    out: &mut [f64],
) {
    let hw_f = half_width as f64;
    let hw_i = half_width as isize;
    let mut sum = 0.0;
    for (j, slot) in out.iter_mut().enumerate() {
        let d = (j as isize - hw_i + 1) as f64;
        let u = d - alpha;
        let norm = u / hw_f;
        let arg = (1.0 - norm * norm).max(0.0).sqrt();
        let w = bessel_i0(beta * arg) / i0_beta;
        let sinc_val = if u == 0.0 {
            1.0
        } else {
            let s_arg = PI * fc * u;
            s_arg.sin() / s_arg
        };
        let val = w * fc * sinc_val;
        *slot = val;
        sum += val;
    }
    if sum.abs() > 0.0 {
        let inv = 1.0 / sum;
        for slot in out.iter_mut() {
            *slot *= inv;
        }
    }
}

/// An impulse response designed at `from_hz`, for use at `to_hz`: the
/// same filter (frequency response, DC gain), not the same sample values.
/// [`resample_offline`] keeps a signal's amplitude, so the taps' sum (the
/// DC gain) grows by `to / from` (+6.02 dB at 2x); the taps are scaled
/// back by `from / to`. Not real-time.
pub fn resample_ir(h: &[f64], from_hz: u32, to_hz: u32) -> Vec<f64> {
    let mut out = resample_offline(h, from_hz, to_hz);
    if from_hz != to_hz {
        let k = f64::from(from_hz) / f64::from(to_hz);
        for x in &mut out {
            *x *= k;
        }
    }
    out
}

/// Offline high-quality resampling of a signal between arbitrary rates
/// (its amplitude kept; an impulse response takes [`resample_ir`]).
/// Output length is `round(x.len() * to / from)`. Not real-time.
pub fn resample_offline(x: &[f64], from_hz: u32, to_hz: u32) -> Vec<f64> {
    assert!(from_hz > 0 && to_hz > 0, "sample rates must be > 0");
    if x.is_empty() {
        return Vec::new();
    }
    if from_hz == to_hz {
        return x.to_vec();
    }

    let out_len =
        ((x.len() as u128 * to_hz as u128 + (from_hz as u128) / 2) / from_hz as u128) as usize;
    if out_len == 0 {
        return Vec::new();
    }

    let g = gcd_u32(from_hz, to_hz);
    let l = (to_hz / g) as usize;
    let m = (from_hz / g) as usize;

    let ratio = (l as f64) / (m as f64);
    let r_min = ratio.min(1.0);
    // beta = 13.5 gives ~137 dB stopband attenuation (well above >= 120 dB).
    let beta = 13.5_f64;
    let i0_beta = bessel_i0(beta);
    let cutoff_frac = 0.95_f64;
    let fc = cutoff_frac * r_min;
    let half_width = ((128.0 / r_min).ceil() as usize).clamp(128, 16_384);
    let k_taps = 2 * half_width;
    let x_len = x.len() as isize;
    let hw_i = half_width as isize;

    let mut y = vec![0.0; out_len];

    if l <= 4096 {
        let mut taps = vec![0.0; l * k_taps];
        for p in 0..l {
            let alpha = (p as f64) / (l as f64);
            design_polyphase_row(
                alpha,
                half_width,
                beta,
                i0_beta,
                fc,
                &mut taps[p * k_taps..(p + 1) * k_taps],
            );
        }

        let l_u128 = l as u128;
        let m_u128 = m as u128;
        for (out_idx, out_sample) in y.iter_mut().enumerate() {
            let num = (out_idx as u128) * m_u128;
            let n0 = (num / l_u128) as isize;
            let p = (num % l_u128) as usize;
            let w_slice = &taps[p * k_taps..(p + 1) * k_taps];

            let first_in = n0 - hw_i + 1;
            let last_in_excl = first_in + k_taps as isize;

            if first_in >= 0 && last_in_excl <= x_len {
                let x_slice = &x[first_in as usize..last_in_excl as usize];
                let mut acc = 0.0;
                for i in 0..k_taps {
                    acc += x_slice[i] * w_slice[i];
                }
                *out_sample = acc;
            } else {
                let j_start = if first_in < 0 {
                    (-first_in) as usize
                } else {
                    0
                };
                let j_end = if last_in_excl > x_len {
                    (x_len - first_in).max(0) as usize
                } else {
                    k_taps
                };
                let mut acc = 0.0;
                for j in j_start..j_end {
                    acc += x[(first_in + j as isize) as usize] * w_slice[j];
                }
                *out_sample = acc;
            }
        }
    } else {
        // Fallback for non-standard coprime rates with very large L:
        // build a 4096-segment interpolated polyphase table.
        const GRID: usize = 4096;
        let mut taps = vec![0.0; (GRID + 1) * k_taps];
        for p in 0..=GRID {
            let alpha = (p as f64) / (GRID as f64);
            design_polyphase_row(
                alpha,
                half_width,
                beta,
                i0_beta,
                fc,
                &mut taps[p * k_taps..(p + 1) * k_taps],
            );
        }

        let l_f = l as f64;
        let l_u128 = l as u128;
        let m_u128 = m as u128;
        for (out_idx, out_sample) in y.iter_mut().enumerate() {
            let num = (out_idx as u128) * m_u128;
            let n0 = (num / l_u128) as isize;
            let rem = (num % l_u128) as f64;
            let pos = (rem / l_f) * (GRID as f64);
            let idx0 = (pos as usize).min(GRID - 1);
            let frac = pos - (idx0 as f64);
            let w0 = &taps[idx0 * k_taps..(idx0 + 1) * k_taps];
            let w1 = &taps[(idx0 + 1) * k_taps..(idx0 + 2) * k_taps];

            let first_in = n0 - hw_i + 1;
            let last_in_excl = first_in + k_taps as isize;
            let j_start = if first_in < 0 {
                (-first_in) as usize
            } else {
                0
            };
            let j_end = if last_in_excl > x_len {
                (x_len - first_in).max(0) as usize
            } else {
                k_taps
            };

            let mut acc = 0.0;
            for j in j_start..j_end {
                let w = w0[j] + frac * (w1[j] - w0[j]);
                acc += x[(first_in + j as isize) as usize] * w;
            }
            *out_sample = acc;
        }
    }

    y
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::complex_mul_split;

    fn upsampler_response(
        cfg: &ManualConfig,
        source_rate: u32,
    ) -> Result<UpsamplerResponse, String> {
        let up = cfg
            .clone()
            .into_upsampler_config()
            .map_err(|e| e.to_string())?;
        super::upsampler_response(&up, source_rate)
    }

    fn measure_upsampler(
        cfg: &ManualConfig,
        source_rate: u32,
    ) -> Result<MeasuredUpsampler, String> {
        let up = cfg
            .clone()
            .into_upsampler_config()
            .map_err(|e| e.to_string())?;
        super::measure_upsampler(&up, source_rate)
    }

    fn measure_upsampler_at(
        cfg: &ManualConfig,
        source_rate: u32,
        max_block: usize,
    ) -> Result<MeasuredUpsampler, String> {
        let up = cfg
            .clone()
            .into_upsampler_config()
            .map_err(|e| e.to_string())?;
        super::measure_upsampler_at(&up, source_rate, max_block)
    }
    use crate::lin_to_db;
    use audivane_protocol::CutoffMode;
    use std::f64::consts::FRAC_PI_2;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bits = ((*seed >> 11) as f64) / ((1u64 << 53) as f64);
        bits * 2.0 - 1.0
    }

    fn naive_zero_stuff_convolve(x: &[f64], kernel: &[f64], ratio: usize) -> Vec<f64> {
        let out_len = x.len() * ratio;
        let mut up = vec![0.0; out_len];
        for (i, &v) in x.iter().enumerate() {
            up[i * ratio] = v;
        }
        let mut y = vec![0.0; out_len];
        for (n, yn) in y.iter_mut().enumerate() {
            let max_k = kernel.len().min(n + 1);
            let mut acc = 0.0;
            for k in 0..max_k {
                acc += kernel[k] * up[n - k];
            }
            *yn = acc;
        }
        y
    }

    fn magnitude_spectrum(h: &[f64], nfft: usize) -> Vec<f64> {
        let mut fft = RealFftPair::new(nfft);
        let mut spec = vec![Complex64::new(0.0, 0.0); fft.spec_len()];
        fft.forward_padded_complex(h, &mut spec);
        spec.iter().map(|c| c.norm()).collect()
    }

    #[test]
    fn design_kernel_linear_symmetry_stopband_and_ripple() {
        let d = UpsamplerDesign {
            ratio: 4,
            taps: 1025,
            phase: FilterPhase::Linear,
            kaiser_beta: 14.0,
            cutoff: 1.0,
            phase_mix: 1.0,
            preserve_samples: false,
            window: audivane_protocol::WindowKind::Kaiser,
            taper: 0.25,
        };
        let h = design_kernel(&d);
        assert_eq!(h.len(), 1025);
        for k in 0..h.len() {
            assert_eq!(h[k], h[h.len() - 1 - k], "not symmetric at k={k}");
        }

        let nfft = 16_384;
        let mag = magnitude_spectrum(&h, nfft);
        let passband_end = (0.8 * (0.5 / 4.0) * nfft as f64) as usize;
        let stopband_start = (1.2 * (0.5 / 4.0) * nfft as f64) as usize;

        let mut max_ripple_db = 0.0_f64;
        for &m in &mag[..=passband_end] {
            let db = lin_to_db(m / 4.0).abs();
            max_ripple_db = max_ripple_db.max(db);
        }
        assert!(
            max_ripple_db < 0.001,
            "passband ripple too high: {max_ripple_db} dB"
        );

        let mut max_stop_db = f64::NEG_INFINITY;
        for &m in &mag[stopband_start..] {
            let db = lin_to_db((m / 4.0).max(1e-18));
            max_stop_db = max_stop_db.max(db);
        }
        assert!(
            max_stop_db <= -130.0,
            "stopband attenuation insufficient: {max_stop_db:.2} dB"
        );
    }

    /// Worst-case comparison of a minimum-phase kernel with its linear twin
    /// on an FFT grid of at least `grid_per_tap * N` points.
    #[derive(Debug)]
    struct TwinReport {
        /// Max |dB difference| where the linear magnitude is >= -1 dB.
        passband_db: f64,
        /// Max |dB difference| where the linear magnitude is >= -100 dB.
        upper_db: f64,
        /// Max over f >= edge of (worst min level from f on) - (worst lin
        /// level from f on), where the latter is above `envelope_floor_db`.
        envelope_db: f64,
        /// Max over f >= edge of `| |H_min| - |H_lin| |`, dB re passband.
        stopband_error_db: f64,
        /// Min over m of (energy fraction of min in [0, m)) - (lin's).
        energy_lead: f64,
    }

    fn compare_twins(
        lin: &[f64],
        min: &[f64],
        ratio: usize,
        edge: f64,
        grid_per_tap: usize,
        envelope_floor_db: f64,
    ) -> TwinReport {
        let nfft = (lin.len() * grid_per_tap).next_power_of_two().max(1024);
        let gain = ratio as f64;
        let ma = magnitude_spectrum(lin, nfft);
        let mb = magnitude_spectrum(min, nfft);
        let la: Vec<f64> = ma
            .iter()
            .map(|m| lin_to_db((m / gain).max(1e-30)))
            .collect();
        let lb: Vec<f64> = mb
            .iter()
            .map(|m| lin_to_db((m / gain).max(1e-30)))
            .collect();
        let (mut passband_db, mut upper_db) = (0.0_f64, 0.0_f64);
        for (a, b) in la.iter().zip(&lb) {
            let d = (a - b).abs();
            if *a >= -1.0 {
                passband_db = passband_db.max(d);
            }
            if *a >= -100.0 {
                upper_db = upper_db.max(d);
            }
        }
        let first = ((edge * nfft as f64).ceil() as usize).min(la.len() - 1);
        let (mut ea, mut eb) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        let mut envelope_db = f64::NEG_INFINITY;
        let mut stopband_error = 0.0_f64;
        for k in (first..la.len()).rev() {
            ea = ea.max(la[k]);
            eb = eb.max(lb[k]);
            if ea >= envelope_floor_db {
                envelope_db = envelope_db.max(eb - ea);
            }
            stopband_error = stopband_error.max((ma[k] - mb[k]).abs() / gain);
        }
        let tl: f64 = lin.iter().map(|x| x * x).sum();
        let tm: f64 = min.iter().map(|x| x * x).sum();
        let (mut el, mut em, mut energy_lead) = (0.0, 0.0, f64::INFINITY);
        for (x, y) in lin.iter().zip(min) {
            el += x * x;
            em += y * y;
            energy_lead = energy_lead.min(em / tm - el / tl);
        }
        TwinReport {
            passband_db,
            upper_db,
            envelope_db,
            stopband_error_db: lin_to_db(stopband_error.max(1e-30)),
            energy_lead,
        }
    }

    /// Kaiser stopband edge estimate (cycles per output sample), with 20%
    /// slack: where the "stopband" of the envelope comparison starts.
    fn stopband_edge(d: &UpsamplerDesign) -> f64 {
        let a = kaiser_attenuation_db(d.kaiser_beta);
        let half = (a - 7.95) / (28.72 * (d.taps - 1) as f64);
        d.cutoff / (2.0 * d.ratio as f64) + 1.2 * half
    }

    /// Schur-Cohn step-down: true iff every zero of `sum b[n] z^-n` is
    /// strictly inside the unit circle.
    fn schur_cohn_stable(b: &[f64]) -> bool {
        let mut a = b.to_vec();
        while a.len() > 1 {
            let m = a.len() - 1;
            let k = a[m] / a[0];
            if k.is_nan() || k.abs() >= 1.0 {
                return false;
            }
            let d = 1.0 - k * k;
            a = (0..m).map(|i| (a[i] - k * a[m - i]) / d).collect();
        }
        true
    }

    /// Zeros of `b` outside radius `e^{c/N}`: winding count of `B` on that
    /// circle, with the largest phase step between bins (must stay < pi/2
    /// for the count to be trusted).
    fn zeros_outside(b: &[f64], c: f64, nfft: usize) -> (i64, f64) {
        let n = b.len() as f64;
        let g: Vec<f64> = b
            .iter()
            .enumerate()
            .map(|(i, &x)| x * (-c * i as f64 / n).exp())
            .collect();
        let mut fft = RealFftPair::new(nfft);
        let mut spec = vec![Complex64::new(0.0, 0.0); fft.spec_len()];
        fft.forward_padded_complex(&g, &mut spec);
        let (mut phase, mut max_step) = (0.0_f64, 0.0_f64);
        for k in 1..spec.len() {
            let d = (spec[k] * spec[k - 1].conj()).arg();
            max_step = max_step.max(d.abs());
            phase += d;
        }
        ((-phase / PI).round() as i64, max_step)
    }

    fn design(ratio: usize, taps: usize, phase: FilterPhase, beta: f64, cutoff: f64) -> Vec<f64> {
        design_kernel(&UpsamplerDesign {
            ratio,
            taps,
            phase,
            kaiser_beta: beta,
            cutoff,
            phase_mix: 1.0,
            preserve_samples: false,
            window: audivane_protocol::WindowKind::Kaiser,
            taper: 0.25,
        })
    }

    /// `crate::minphase` contract: the stopband error stays this far under
    /// the Kaiser stopband specification `A`, and the envelope comparison
    /// holds wherever the linear twin is above `-(A + 5)` dB (20 dB above
    /// the error bound, where an error of that size moves a level by at most
    /// 0.83 dB) and, on kernels up to 32k taps, down to -220 dB.
    const STOPBAND_ERROR_MARGIN_DB: f64 = crate::minphase::STOPBAND_ERROR_MARGIN_DB;

    /// Checks one design against the contract in `crate::minphase`.
    fn assert_minimum_phase_twin(ratio: usize, taps: usize, beta: f64, cutoff: f64) {
        let d = UpsamplerDesign {
            ratio,
            taps,
            phase: FilterPhase::Linear,
            kaiser_beta: beta,
            cutoff,
            phase_mix: 1.0,
            preserve_samples: false,
            window: audivane_protocol::WindowKind::Kaiser,
            taper: 0.25,
        };
        let lin = design_kernel(&d);
        let report = crate::minphase::minimum_phase(&lin, &minimum_phase_contract(&d))
            .unwrap_or_else(|e| panic!("N={taps} x{ratio} beta={beta} cutoff={cutoff}: {e}"));
        assert!(report.exact);
        let min = design(ratio, taps, FilterPhase::Minimum, beta, cutoff);
        let t = compare_twins(&lin, &min, ratio, stopband_edge(&d), 16, -220.0);
        let what = format!("N={taps} x{ratio} beta={beta} cutoff={cutoff}");
        assert!(
            t.passband_db <= 1e-4,
            "{what}: passband {} dB",
            t.passband_db
        );
        assert!(
            t.upper_db <= 0.01,
            "{what}: >= -100 dB region {} dB",
            t.upper_db
        );
        assert!(
            t.envelope_db <= 1.0,
            "{what}: stopband envelope +{} dB",
            t.envelope_db
        );
        let error_bound = -(kaiser_attenuation_db(beta) + STOPBAND_ERROR_MARGIN_DB);
        assert!(
            t.stopband_error_db <= error_bound,
            "{what}: stopband error {} dB > {error_bound} dB",
            t.stopband_error_db
        );
        assert!(
            t.energy_lead >= -1e-12,
            "{what}: energy lead {}",
            t.energy_lead
        );
        // The winding count needs every bin above the rounding floor; at
        // beta 24 the deep stopband of 16k+ kernels is at it (Schur-Cohn
        // covers beta 24 on short kernels).
        if beta <= 20.0 {
            let (outside, step) = zeros_outside(&min, 0.25, (taps * 64).next_power_of_two());
            assert!(step < FRAC_PI_2, "{what}: winding step {step}");
            assert_eq!(outside, 0, "{what}: zeros outside 1 + 0.25/N");
            let (outside_lin, _) = zeros_outside(&lin, 0.25, (taps * 64).next_power_of_two());
            assert!(
                outside_lin > 0,
                "{what}: the linear twin should have outside zeros"
            );
        }
    }

    #[test]
    fn minimum_phase_twin_matches_magnitude_and_is_minimum_phase() {
        // Odd, even and non-power-of-two lengths, both cutoff modes, the
        // calibrated beta range ends.
        for &(ratio, taps) in &[(4usize, 1024usize), (16, 4097), (2, 16_385), (16, 32_767)] {
            for &beta in &[2.0_f64, 8.0, 14.0, 20.0] {
                for cutoff in [1.0, 0.95, nyquist_stop_cutoff(taps, ratio, beta)] {
                    assert_minimum_phase_twin(ratio, taps, beta, cutoff);
                }
            }
        }
    }

    #[test]
    fn minimum_phase_short_kernels_pass_schur_cohn() {
        // Exact algebraic check: every zero strictly inside e^{0.25/N} (the
        // stopband nulls sit on the unit circle, so test just outside it).
        // Rounding noise of the f64 coefficients (below ~-300 dB, reached at
        // beta 24) has zeros of its own within ~0.13/N of the circle.
        for &taps in &[2usize, 3, 16, 63, 64, 255, 256, 1024, 2047] {
            for &beta in &[2.0_f64, 3.0, 8.0, 14.0, 20.0] {
                for &(ratio, cutoff) in &[(2usize, 1.0), (4, 0.95), (16, 1.0)] {
                    let lin = design(ratio, taps, FilterPhase::Linear, beta, cutoff);
                    let min = design(ratio, taps, FilterPhase::Minimum, beta, cutoff);
                    let scale = |h: &[f64]| -> Vec<f64> {
                        h.iter()
                            .enumerate()
                            .map(|(i, &x)| x * (-0.25 * i as f64 / taps as f64).exp())
                            .collect()
                    };
                    assert!(
                        schur_cohn_stable(&scale(&min)),
                        "N={taps} beta={beta} x{ratio} cutoff={cutoff}: zero outside e^(0.25/N)"
                    );
                    if taps >= 64 {
                        assert!(!schur_cohn_stable(&scale(&lin)), "linear twin passed");
                    }
                    let (g_lin, g_min) = (lin.iter().sum::<f64>(), min.iter().sum::<f64>());
                    assert!((g_lin - g_min).abs() < 1e-9 * g_lin.abs());
                }
            }
        }
    }

    /// Every zero of the short minimum-phase kernel `b` outside the unit
    /// circle sits where `|B|` on the circle at its angle is at the f64
    /// rounding floor (under -250 dB re the DC gain), and, as an independent
    /// algebraic check, Schur-Cohn puts every zero inside `e^{slack/N}`.
    fn assert_zeros_inside(b: &[f64], slack: f64, what: &str) {
        let n = b.len();
        let dc: f64 = b.iter().sum::<f64>().abs();
        for z in crate::minphase::zeros(b).unwrap_or_else(|| panic!("{what}: no zeros")) {
            if z.norm() > 1.0 {
                let f = z.arg().abs() / (2.0 * PI);
                let level = lin_to_db(dft_magnitudes(b, 1.0, &[f])[0] / dc);
                assert!(
                    level < -250.0,
                    "{what}: zero at radius 1 + {:.3e} where |B| is {level:.1} dB",
                    z.norm() - 1.0
                );
            }
        }
        let scaled: Vec<f64> = b
            .iter()
            .enumerate()
            .map(|(i, &x)| x * (-slack * i as f64 / n as f64).exp())
            .collect();
        assert!(
            schur_cohn_stable(&scaled),
            "{what}: Schur-Cohn finds a zero outside e^({slack}/N)"
        );
    }

    /// The contract of `crate::minphase` on one short design, plus its zeros.
    fn assert_short_twin(ratio: usize, taps: usize, beta: f64, cutoff: f64, slack: f64) {
        let d = UpsamplerDesign {
            ratio,
            taps,
            phase: FilterPhase::Linear,
            kaiser_beta: beta,
            cutoff,
            phase_mix: 1.0,
            preserve_samples: false,
            window: audivane_protocol::WindowKind::Kaiser,
            taper: 0.25,
        };
        let what = format!("N={taps} x{ratio} beta={beta} cutoff={cutoff}");
        let lin = design_kernel(&d);
        let report = crate::minphase::minimum_phase(&lin, &minimum_phase_contract(&d))
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(report.method, crate::minphase::Method::Roots, "{what}");
        let check = report.check.expect("checked");
        assert!(check.max_zero_excess.unwrap() <= 1.0, "{what}: {check:?}");
        let min = design(ratio, taps, FilterPhase::Minimum, beta, cutoff);
        let t = compare_twins(&lin, &min, ratio, stopband_edge(&d), 32, -220.0);
        assert!(
            t.passband_db <= 1e-4,
            "{what}: passband {} dB",
            t.passband_db
        );
        assert!(
            t.upper_db <= 0.01,
            "{what}: >= -100 dB region {} dB",
            t.upper_db
        );
        assert!(
            t.envelope_db <= 1.0,
            "{what}: stopband envelope +{} dB",
            t.envelope_db
        );
        let bound = -(kaiser_attenuation_db(beta) + STOPBAND_ERROR_MARGIN_DB);
        assert!(
            t.stopband_error_db <= bound,
            "{what}: stopband error {} dB",
            t.stopband_error_db
        );
        assert!(
            t.energy_lead >= -1e-12,
            "{what}: energy lead {}",
            t.energy_lead
        );
        if taps >= 2 {
            assert_zeros_inside(&min, slack, &what);
        }
    }

    #[test]
    fn minimum_phase_review_counterexamples() {
        // The scaled cepstrum left the 15-tap pair at |z| = 1.035 outside
        // and reported success; the other two silently fell back and missed
        // the magnitude tolerances.
        let cases = [
            (16usize, 15usize, 14.0_f64, 1.0_f64),
            (2, 36, 20.0, 1.0),
            (8, 35, 19.817_216_698_664_79, 0.333_086_506_655_635_2),
        ];
        for &(ratio, taps, beta, cutoff) in &cases {
            // Tighter than the general contract: these zeros are all within
            // 1e-6 of the circle (measured: 2.5e-9).
            assert_short_twin(ratio, taps, beta, cutoff, 1.0e-6 * taps as f64);
        }
        let lin = design(16, 15, FilterPhase::Linear, 14.0, 1.0);
        let outside = crate::minphase::zeros(&lin)
            .unwrap()
            .into_iter()
            .filter(|z| (z.norm() - 1.034_996_6).abs() < 1e-6)
            .count();
        assert_eq!(outside, 2, "the conjugate pair at 1.035");
        let min = design(16, 15, FilterPhase::Minimum, 14.0, 1.0);
        let worst = crate::minphase::zeros(&min)
            .unwrap()
            .iter()
            .map(|z| z.norm())
            .fold(0.0, f64::max);
        assert!(
            worst < 1.0 + 1e-8,
            "15-tap result: largest zero radius {worst}"
        );
    }

    #[test]
    fn minimum_phase_short_lengths_sweep() {
        // Every length the root path covers up to 160 taps, odd and even,
        // three rotating (ratio, beta, cutoff) designs per length. The heavy
        // `minimum_phase_sweep_to_4096_taps` covers every length to 4096.
        let ratios = [2usize, 3, 4, 8, 16, 32];
        let betas = [2.0_f64, 5.0, 8.0, 11.0, 14.0, 17.0, 19.0, 19.817, 20.0];
        let cutoffs = [1.0_f64, 0.95, 0.5, 0.333, 0.9883];
        let mut i = 0usize;
        for taps in 2..=160usize {
            for _ in 0..3 {
                let (r, b, c) = (ratios[i % 6], betas[(i / 6) % 9], cutoffs[(i / 54) % 5]);
                i += 1;
                assert_short_twin(r, taps, b, c, 0.25);
            }
        }
    }

    /// Every length from 2 to 4096 taps, three designs each (seeded random
    /// ratio 2..=32, beta 2..=20 and cutoff 0.3..=1, plus fixed ones), in
    /// parallel. The root path (to 1024 taps) is checked by its zeros, the
    /// cepstral path by winding counts on `e^{0.25/N}`. Run with
    /// `cargo test -p audivane-dsp-core --release minimum_phase_sweep_to_4096 -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn minimum_phase_sweep_to_4096_taps() {
        let mut seed = 0x5eed_u64;
        let mut jobs = Vec::new();
        for taps in 2..=4096usize {
            for k in 0..3 {
                let ratio = [2usize, 3, 4, 8, 16, 32][(lcg(&mut seed).abs() * 6.0) as usize % 6];
                let beta = if k == 0 {
                    [2.0, 8.0, 14.0, 20.0][taps % 4]
                } else {
                    2.0 + 18.0 * lcg(&mut seed).abs()
                };
                let cutoff = if k == 1 {
                    1.0
                } else {
                    0.3 + 0.7 * lcg(&mut seed).abs()
                };
                jobs.push((ratio, taps, beta, cutoff));
            }
        }
        let total = jobs.len();
        let queue = std::sync::Mutex::new(jobs.into_iter());
        let untrusted = std::sync::atomic::AtomicUsize::new(0);
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(32);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let job = queue.lock().unwrap().next();
                        let Some((ratio, taps, beta, cutoff)) = job else {
                            break;
                        };
                        if taps <= crate::minphase::ROOT_MAX_TAPS {
                            let slack = if taps <= 160 { 0.25 } else { 64.0 };
                            assert_short_twin(ratio, taps, beta, cutoff, slack);
                            continue;
                        }
                        let what = format!("N={taps} x{ratio} beta={beta} cutoff={cutoff}");
                        let d = UpsamplerDesign {
                            ratio,
                            taps,
                            phase: FilterPhase::Linear,
                            kaiser_beta: beta,
                            cutoff,
                            phase_mix: 1.0,
                            preserve_samples: false,
                            window: audivane_protocol::WindowKind::Kaiser,
                            taper: 0.25,
                        };
                        let lin = design_kernel(&d);
                        let min = try_design_kernel(&UpsamplerDesign {
                            phase: FilterPhase::Minimum,
                            ..d
                        })
                        .unwrap_or_else(|e| panic!("{what}: {e}"));
                        let t = compare_twins(&lin, &min, ratio, stopband_edge(&d), 16, -220.0);
                        let bound = -(kaiser_attenuation_db(beta) + STOPBAND_ERROR_MARGIN_DB);
                        assert!(
                            t.passband_db <= 1e-4 && t.upper_db <= 0.01 && t.envelope_db <= 1.0,
                            "{what}: {t:?}"
                        );
                        assert!(t.stopband_error_db <= bound, "{what}: {t:?}");
                        let (outside, step) =
                            zeros_outside(&min, 0.25, (taps * 256).next_power_of_two());
                        if step >= 1.5 {
                            untrusted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            assert_eq!(outside, 0, "{what}: zeros outside e^(0.25/N)");
                        }
                    }
                });
            }
        });
        let untrusted = untrusted.into_inner();
        eprintln!(
            "{total} designs passed; {untrusted} winding counts not trusted (step >= 1.5 rad)"
        );
        assert!(
            untrusted * 1000 <= total,
            "too many untrusted winding counts: {untrusted}"
        );
    }

    #[test]
    fn minimum_phase_failures_are_errors_not_fallbacks() {
        use crate::minphase::{Contract, MinimumPhaseError, minimum_phase};
        let contract = Contract {
            kaiser_beta: 8.0,
            stopband_edge: 0.3,
        };
        assert_eq!(
            minimum_phase(&[f64::NAN, 1.0], &contract).unwrap_err(),
            MinimumPhaseError::BadInput
        );
        assert_eq!(
            minimum_phase(&[0.0; 8], &contract).unwrap_err(),
            MinimumPhaseError::BadInput
        );
        // Beta 40 is outside the supported range (the daemon rejects it): the
        // twin cannot be 25 dB under A = 371.7 dB in f64, on either path.
        for taps in [512usize, 4096] {
            let d = UpsamplerDesign {
                ratio: 4,
                taps,
                phase: FilterPhase::Minimum,
                kaiser_beta: 40.0,
                cutoff: 1.0,
                phase_mix: 1.0,
                preserve_samples: false,
                window: audivane_protocol::WindowKind::Kaiser,
                taper: 0.25,
            };
            let err = try_design_kernel(&d).unwrap_err();
            assert!(
                err.to_string()
                    .contains("could not be designed to tolerance"),
                "{taps}: {err}"
            );
            assert!(Upsampler::try_new(&d, 2, 256).is_err());
            let cfg = ManualConfig {
                target: audivane_protocol::RateTarget::Ratio(4),
                taps,
                phase: FilterPhase::Minimum,
                kaiser_beta: 40.0,
                cutoff: 1.0,
                cutoff_mode: CutoffMode::Fraction,
                ..Default::default()
            };
            let msg = measure_upsampler(&cfg, 48_000).unwrap_err();
            assert!(msg.contains("minimum-phase"), "{msg}");
        }
    }

    /// The contract at the longest lengths (power of two and one below it),
    /// both cutoff modes, plus design time and peak memory. Run with
    /// `cargo test -p audivane-dsp-core --release minimum_phase_4m -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn minimum_phase_4m_contract_time_and_memory() {
        use std::time::Instant;
        for &taps in &[4_194_304usize, 4_194_303, 1_000_001] {
            for &ratio in &[16usize, 2] {
                for &beta in &[8.0_f64, 14.0, 20.0] {
                    for cutoff in [nyquist_stop_cutoff(taps, ratio, beta), 0.95, 1.0] {
                        let d = UpsamplerDesign {
                            ratio,
                            taps,
                            phase: FilterPhase::Linear,
                            kaiser_beta: beta,
                            cutoff,
                            phase_mix: 1.0,
                            preserve_samples: false,
                            window: audivane_protocol::WindowKind::Kaiser,
                            taper: 0.25,
                        };
                        let started = Instant::now();
                        let lin = design_kernel(&d);
                        let t_lin = started.elapsed().as_secs_f64();
                        let started = Instant::now();
                        let min = design_kernel(&UpsamplerDesign {
                            phase: FilterPhase::Minimum,
                            ..d
                        });
                        let t_min = started.elapsed().as_secs_f64();
                        let a = kaiser_attenuation_db(beta);
                        let t = compare_twins(&lin, &min, ratio, stopband_edge(&d), 4, -(a + 5.0));
                        eprintln!(
                            "N={taps} x{ratio} beta={beta} cutoff={cutoff:.6}: linear {t_lin:.2} s, \
                             minimum {t_min:.2} s, peak RSS {:.0} MiB; passband {:.2e} dB, \
                             >=-100 dB {:.2e} dB, envelope {:+.3} dB, stopband error {:.1} dB",
                            peak_rss_mib(),
                            t.passband_db,
                            t.upper_db,
                            t.envelope_db,
                            t.stopband_error_db
                        );
                        assert!(
                            t.passband_db <= 1e-4 && t.upper_db <= 0.01 && t.envelope_db <= 1.0
                        );
                        assert!(t.stopband_error_db <= -(a + STOPBAND_ERROR_MARGIN_DB));
                        assert!(t.energy_lead >= -1e-12);
                        assert!(t_min <= 15.0, "{t_min} s");
                    }
                }
            }
        }
        assert!(peak_rss_mib() <= 2048.0, "{} MiB", peak_rss_mib());
    }

    #[test]
    fn upsampler_ratio_1_is_bit_exact() {
        let d = UpsamplerDesign {
            ratio: 1,
            taps: 256,
            phase: FilterPhase::Linear,
            kaiser_beta: 14.0,
            cutoff: 1.0,
            phase_mix: 1.0,
            preserve_samples: false,
            window: audivane_protocol::WindowKind::Kaiser,
            taper: 0.25,
        };
        let mut up = Upsampler::new(&d, 2, 128);
        assert_eq!(up.ratio(), 1);
        assert_eq!(up.latency_out_frames(), 0);

        let mut seed = 11u64;
        let in0: Vec<f64> = (0..73).map(|_| lcg(&mut seed)).collect();
        let in1: Vec<f64> = (0..73).map(|_| lcg(&mut seed)).collect();
        let mut out0 = vec![0.0; 73];
        let mut out1 = vec![0.0; 73];
        up.process(&[&in0, &in1], &mut [&mut out0, &mut out1]);
        assert_eq!(out0, in0);
        assert_eq!(out1, in1);
    }

    /// Zero-stuff `x` by `ratio` and convolve with `kernel` through one big
    /// real FFT (first `x.len() * ratio` samples).
    fn fft_zero_stuff_convolve(x: &[f64], kernel: &[f64], ratio: usize) -> Vec<f64> {
        let out_len = x.len() * ratio;
        let mut up = vec![0.0; out_len];
        for (i, &v) in x.iter().enumerate() {
            up[i * ratio] = v;
        }
        let nfft = (out_len + kernel.len()).next_power_of_two();
        let mut fft = RealFftPair::new(nfft);
        let sx = fft.forward_padded_scaled(&up, 1.0);
        let sh = fft.forward_padded_scaled(kernel, 1.0 / nfft as f64);
        let mut prod = SplitSpectrum::new(fft.spec_len());
        complex_mul_split(&mut prod, &sx, &sh);
        let mut y = vec![0.0; nfft];
        fft.inverse_from_split(&prod, &mut y);
        y.truncate(out_len);
        y
    }

    #[test]
    fn upsampler_long_kernels_match_fft_reference() {
        // Polyphase branches longer than 65536 taps (where the convolver's
        // tail schedule has to be right), up to a 1.1M-tap kernel at 16x. The
        // upsampler is uniformly partitioned, so every partition must land
        // exactly `p * block_in` input frames late.
        for (ratio, taps, max_block, frames) in [
            (2usize, 140_001usize, 1000usize, 75_000usize),
            (16, 1_100_000, 512, 72_000),
        ] {
            let d = UpsamplerDesign {
                ratio,
                taps,
                phase: FilterPhase::Linear,
                kaiser_beta: 12.0,
                cutoff: 0.95,
                phase_mix: 1.0,
                preserve_samples: false,
                window: audivane_protocol::WindowKind::Kaiser,
                taper: 0.25,
            };
            let kernel = design_kernel(&d);
            assert!(kernel.len().div_ceil(ratio) > 65_536);
            let mut up = Upsampler::new(&d, 1, max_block);
            let lat = up.block_latency_out_frames();
            let mut seed = taps as u64;
            let mut x: Vec<f64> = (0..frames).map(|_| lcg(&mut seed)).collect();
            x.resize(frames + lat / ratio + 1, 0.0);

            let mut y = Vec::with_capacity(x.len() * ratio);
            let mut out = vec![0.0; max_block * ratio];
            let mut offset = 0;
            let mut step = 0usize;
            while offset < x.len() {
                let chunk = [max_block, 1, 333, max_block / 2 + 1][step % 4].min(x.len() - offset);
                step += 1;
                up.process(
                    &[&x[offset..offset + chunk]],
                    &mut [&mut out[..chunk * ratio]],
                );
                y.extend_from_slice(&out[..chunk * ratio]);
                offset += chunk;
            }

            let expected = fft_zero_stuff_convolve(&x[..frames], &kernel, ratio);
            let peak = expected.iter().fold(1.0f64, |m, v| m.max(v.abs()));
            let mut max_err = y[..lat].iter().fold(0.0f64, |m, v| m.max(v.abs()));
            for i in 0..frames * ratio {
                max_err = max_err.max((y[i + lat] - expected[i]).abs());
            }
            assert!(
                max_err <= 1e-12 * peak,
                "ratio {ratio}, taps {taps}: max_err {max_err:.3e} (peak {peak:.2})"
            );
        }
    }

    /// Run `x` through `up` (one channel) in calls of varying size: full
    /// blocks, 1, odd, 0, random.
    fn run_varying(up: &mut Upsampler, x: &[f64], max_block: usize, seed: u64) -> Vec<f64> {
        let r = up.ratio;
        let mut seed = seed;
        let mut y = Vec::with_capacity(x.len() * r);
        let mut out = vec![0.0; max_block * r];
        let (mut offset, mut step) = (0, 0usize);
        while offset < x.len() {
            let n = match step % 6 {
                0 => max_block,
                1 => 1,
                2 => 0,
                3 => (max_block / 2) | 1,
                _ => {
                    (((lcg(&mut seed) + 1.0) * 0.5 * max_block as f64) as usize).clamp(1, max_block)
                }
            }
            .min(x.len() - offset);
            step += 1;
            up.process(&[&x[offset..offset + n]], &mut [&mut out[..n * r]]);
            y.extend_from_slice(&out[..n * r]);
            offset += n;
        }
        y
    }

    /// A kernel short enough for one stage runs the uniformly partitioned
    /// engine unchanged, bit for bit (its latency too).
    #[test]
    fn short_kernels_run_the_uniform_engine_bit_for_bit() {
        let mut seed = 21u64;
        for (ratio, per_branch, max_block) in [
            (2usize, 8192usize, 512usize),
            (16, 8192, 1024),
            (64, 4096, 512),
            (3, 20 * 1024, 1024),
        ] {
            let kernel: Vec<f64> = (0..per_branch * ratio)
                .map(|_| lcg(&mut seed) * 0.01)
                .collect();
            let mut up = Upsampler::from_kernel(&kernel, ratio, 17, 1, max_block);
            let mut uni = Upsampler::from_kernel_uniform(&kernel, ratio, 17, 1, max_block);
            assert_eq!(
                up.stages().len(),
                1,
                "x{ratio} {per_branch}: {:?}",
                up.stages()
            );
            assert_eq!(up.latency_out_frames(), uni.latency_out_frames());
            let x: Vec<f64> = (0..3 * per_branch).map(|_| lcg(&mut seed)).collect();
            let a = run_varying(&mut up, &x, max_block, 1);
            let b = run_varying(&mut uni, &x, max_block, 1);
            assert!(a.iter().zip(&b).all(|(p, q)| p.to_bits() == q.to_bits()));
        }
    }

    /// Long kernels on non-uniform partitions against one big FFT
    /// convolution of the zero-stuffed input: at most 1e-12 of the output
    /// peak (-240 dB; measured about -290 dB), with the latency of the
    /// uniform engine. Covers a non-power-of-two ratio, the output buffer
    /// cap at x64, every chunk size and both base blocks.
    #[test]
    fn long_kernels_on_tail_stages_match_the_reference() {
        let mut seed = 77u64;
        for (ratio, per_branch, max_block, frames) in [
            (16usize, 70_000usize, 512usize, 150_000usize),
            (3, 300_000, 1024, 330_000),
            (64, 40_000, 1024, 60_000),
            (4, 140_000, 2048, 170_000),
        ] {
            let kernel: Vec<f64> = (0..per_branch * ratio)
                .map(|i| lcg(&mut seed) * (-(i as f64) / (per_branch * ratio) as f64 * 3.0).exp())
                .collect();
            let mut up = Upsampler::from_kernel(&kernel, ratio, 0, 1, max_block);
            let uni = Upsampler::from_kernel_uniform(&kernel, ratio, 0, 1, max_block);
            assert!(up.stages().len() > 1, "{:?}", up.stages());
            assert_eq!(up.latency_out_frames(), uni.latency_out_frames());
            let lat = up.block_latency_out_frames();
            let mut x: Vec<f64> = (0..frames).map(|_| lcg(&mut seed)).collect();
            x.resize(frames + lat / ratio + 1, 0.0);
            let y = run_varying(&mut up, &x, max_block, ratio as u64);
            let expected = fft_zero_stuff_convolve(&x[..frames], &kernel, ratio);
            let peak = expected.iter().fold(1e-30f64, |m, v| m.max(v.abs()));
            let mut err = y[..lat].iter().fold(0.0f64, |m, v| m.max(v.abs()));
            for i in 0..frames * ratio {
                err = err.max((y[i + lat] - expected[i]).abs());
            }
            let db = 20.0 * (err / peak).max(1e-30).log10();
            eprintln!(
                "x{ratio} {per_branch}/branch, block {max_block}, {:?}: {db:.1} dB",
                up.stages()
            );
            assert!(err <= 1e-12 * peak, "x{ratio}: {err:.3e} of peak {peak:.3}");
        }
    }

    /// The tail stages' work is spread over the base blocks of their
    /// periods: no call does much more than the mean (in the model's units).
    #[test]
    fn tail_work_is_spread_evenly_across_calls() {
        let mut seed = 4u64;
        let ratio = 16;
        let kernel: Vec<f64> = (0..ratio * 300_000)
            .map(|_| lcg(&mut seed) * 1e-3)
            .collect();
        let block = 512;
        let mut up = Upsampler::from_kernel(&kernel, ratio, 0, 1, block);
        let largest = up.stages().iter().map(|s| s.0).max().unwrap();
        assert_eq!(largest, 32_768);
        let x = vec![0.1; block];
        let mut out = vec![0.0; block * ratio];
        let per = largest / block;
        let mut costs = Vec::new();
        for i in 0..4 * per {
            up.process(&[&x], &mut [&mut out]);
            if i >= per {
                costs.push(up.last_process_cost());
            }
        }
        let mean = costs.iter().sum::<f64>() / costs.len() as f64;
        let max = costs.iter().fold(0.0f64, |m, &c| m.max(c));
        eprintln!(
            "work per call: mean {mean:.0}, max {max:.0} (x{:.3})",
            max / mean
        );
        assert!(max <= 1.1 * mean, "max {max} vs mean {mean}");
    }

    #[test]
    fn reset_and_split_channels_keep_tail_stages_exact() {
        let mut seed = 8u64;
        let ratio = 4;
        let kernel: Vec<f64> = (0..ratio * 100_000)
            .map(|_| lcg(&mut seed) * 1e-2)
            .collect();
        let x: Vec<f64> = (0..140_000).map(|_| lcg(&mut seed)).collect();
        let mut fresh = Upsampler::from_kernel(&kernel, ratio, 0, 1, 512);
        let reference = run_varying(&mut fresh, &x, 512, 3);
        // Stopped mid-period of every stage, then reset.
        let mut up = Upsampler::from_kernel(&kernel, ratio, 0, 2, 512);
        let junk: Vec<f64> = (0..23_456).map(|_| lcg(&mut seed)).collect();
        let mut o = vec![0.0; 512 * ratio];
        let mut o2 = vec![0.0; 512 * ratio];
        for c in junk.chunks(500) {
            let n = c.len();
            up.process(&[c, c], &mut [&mut o[..n * ratio], &mut o2[..n * ratio]]);
        }
        up.reset();
        let mut lanes = up.split_channels();
        let again = run_varying(&mut lanes[1], &x, 512, 3);
        assert!(
            reference
                .iter()
                .zip(&again)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        // After a reset, zeros in give exact zeros out.
        lanes[0].reset();
        let z = run_varying(&mut lanes[0], &vec![0.0; 70_000], 512, 4);
        assert!(z.iter().all(|&v| v == 0.0));
    }

    /// The design budget's resident estimate ([`crate::design::budget::kernel_resident`])
    /// against what the engine keeps: within 10 % on many stages, 20 % on one.
    #[test]
    fn the_resident_estimate_matches_the_engine() {
        let mut seed = 6u64;
        for (ratio, taps, max_block) in [
            (16usize, 65_536usize, 1024usize),
            (16, 1 << 20, 512),
            (4, 1 << 21, 4096),
            (64, 1 << 21, 1024),
        ] {
            let kernel: Vec<f64> = (0..taps).map(|_| lcg(&mut seed) * 1e-3).collect();
            let up = Upsampler::from_kernel(&kernel, ratio, 0, 2, max_block);
            let (shared, per_channel) = up.resident_bytes();
            let actual = (shared + 2 * per_channel) as f64;
            let cx = crate::design::BuildContext {
                source_rate: 48_000,
                ratio: RateRatio::integer(ratio as u32),
                channels: 2,
                max_block,
                pipelines: 1,
                output: Default::default(),
                workspace: crate::design::budget::design_workspace(),
            };
            let estimate = crate::design::budget::kernel_resident(taps, &cx) as f64;
            eprintln!(
                "x{ratio} {taps} {:?}: estimate {estimate:.0}, actual {actual:.0}",
                up.stages()
            );
            // One stage: the formula from before non-uniform partitions,
            // which leaves out the head's accumulators.
            let tol = if up.stages().len() == 1 { 0.2 } else { 0.1 };
            assert!(
                (estimate / actual - 1.0).abs() < tol,
                "{estimate} vs {actual}"
            );
        }
    }

    /// Mean seconds per base block of `up` (mono, one base block per
    /// call) over `periods` periods of its largest stage after one.
    fn time_per_block(up: &mut Upsampler, periods: usize) -> f64 {
        let b = up.block_in;
        let r = up.ratio;
        let largest = up.stages().iter().map(|s| s.0).max().unwrap();
        let per = largest / b;
        let mut seed = 3u64;
        let x: Vec<f64> = (0..b).map(|_| lcg(&mut seed)).collect();
        let mut out = vec![0.0; b * r];
        for _ in 0..per {
            up.process(&[&x], &mut [&mut out]);
        }
        let calls = (periods * per).max(400);
        let t0 = std::time::Instant::now();
        for _ in 0..calls {
            up.process(&[&x], &mut [&mut out]);
        }
        t0.elapsed().as_secs_f64() / calls as f64
    }

    /// Where non-uniform partitions start to pay: the uniform engine, the
    /// planner's schedule and the unmerged default schedule, per base block,
    /// over branch lengths around the planner's threshold.
    #[test]
    #[ignore = "benchmark: cargo test --release -p audivane-dsp-core --lib bench_plan_crossover -- --ignored --nocapture"]
    fn bench_plan_crossover() {
        let mut seed = 9u64;
        for base in [512usize, 1024] {
            for ratio in [2usize, 4, 16, 64] {
                for mult in [8usize, 12, 16, 20, 24, 28, 32, 40, 48, 64, 96, 128, 256] {
                    let len = mult * base;
                    let kernel: Vec<f64> =
                        (0..len * ratio).map(|_| lcg(&mut seed) * 1e-3).collect();
                    let mut uni = Upsampler::from_kernel_uniform(&kernel, ratio, 0, 1, base);
                    let mut auto = Upsampler::from_kernel(&kernel, ratio, 0, 1, base);
                    let default = crate::nupc::plan_schedule_capped(
                        base,
                        len,
                        crate::nupc::polyphase_max_part(base, ratio),
                    );
                    let mut def = Upsampler::with_plan(&kernel, ratio, 0, 1, base, &default);
                    let tu = time_per_block(&mut uni, 3);
                    let ta = time_per_block(&mut auto, 3);
                    let td = time_per_block(&mut def, 3);
                    let fmt = |u: &Upsampler| {
                        u.stages()
                            .iter()
                            .map(|(b, p)| format!("{b}x{p}"))
                            .collect::<Vec<_>>()
                            .join("+")
                    };
                    eprintln!(
                        "B0 {base:4} x{ratio:<2} len {mult:3}*B0: uniform {:8.1} us | plan {:8.1} us ({:5.2}x) {:28} | default {:8.1} us ({:5.2}x) {}",
                        tu * 1e6,
                        ta * 1e6,
                        tu / ta,
                        fmt(&auto),
                        td * 1e6,
                        tu / td,
                        fmt(&def)
                    );
                }
            }
        }
    }

    #[test]
    fn upsampler_matches_naive_zero_stuff_random_blocks() {
        for (ratio, taps, phase, max_block) in [
            (4, 300, FilterPhase::Linear, 64),
            (7, 513, FilterPhase::Minimum, 50),
            (16, 1200, FilterPhase::Linear, 128),
            (32, 1024, FilterPhase::Linear, 64),
        ] {
            let d = UpsamplerDesign {
                ratio,
                taps,
                phase,
                kaiser_beta: 12.0,
                cutoff: 0.95,
                phase_mix: 1.0,
                preserve_samples: false,
                window: audivane_protocol::WindowKind::Kaiser,
                taper: 0.25,
            };
            let kernel = design_kernel(&d);
            let mut up = Upsampler::new(&d, 2, max_block);
            let block_lat = up.block_latency_out_frames();
            let peak = kernel
                .iter()
                .enumerate()
                .fold(0, |b, (i, v)| if v.abs() > kernel[b].abs() { i } else { b });
            let expected_lat = match phase {
                FilterPhase::Linear => block_lat + (taps - 1) / 2,
                FilterPhase::Minimum | FilterPhase::Intermediate => block_lat + peak,
            };
            assert_eq!(up.latency_out_frames(), expected_lat);

            let total_in = 800usize;
            let pad_in = block_lat / ratio + 2;
            let mut seed = 500 + ratio as u64;
            let mut x0: Vec<f64> = (0..total_in).map(|_| lcg(&mut seed)).collect();
            let mut x1: Vec<f64> = (0..total_in).map(|_| lcg(&mut seed)).collect();
            x0.resize(total_in + pad_in, 0.0);
            x1.resize(total_in + pad_in, 0.0);

            let mut y0 = Vec::with_capacity((total_in + pad_in) * ratio);
            let mut y1 = Vec::with_capacity((total_in + pad_in) * ratio);

            let mut offset = 0usize;
            let mut step = 0usize;
            while offset < x0.len() {
                let rem = x0.len() - offset;
                let chunk = match step % 5 {
                    0 => 0,
                    1 => max_block.min(rem),
                    2 => 1.min(rem),
                    _ => (((lcg(&mut seed) + 1.0) * 0.5 * max_block as f64) as usize)
                        .clamp(1, max_block)
                        .min(rem),
                };
                step += 1;
                let mut o0 = vec![0.0; chunk * ratio];
                let mut o1 = vec![0.0; chunk * ratio];
                up.process(
                    &[&x0[offset..offset + chunk], &x1[offset..offset + chunk]],
                    &mut [&mut o0, &mut o1],
                );
                y0.extend_from_slice(&o0);
                y1.extend_from_slice(&o1);
                offset += chunk;
            }

            let exp0 = naive_zero_stuff_convolve(&x0[..total_in], &kernel, ratio);
            let exp1 = naive_zero_stuff_convolve(&x1[..total_in], &kernel, ratio);
            let mut max_err = 0.0_f64;
            for i in 0..block_lat {
                max_err = max_err.max(y0[i].abs()).max(y1[i].abs());
            }
            for i in 0..total_in * ratio {
                max_err = max_err
                    .max((y0[i + block_lat] - exp0[i]).abs())
                    .max((y1[i + block_lat] - exp1[i]).abs());
            }
            assert!(
                max_err <= 1e-12,
                "ratio={ratio}, taps={taps}: max_err={max_err:.3e}"
            );

            // Reset clears state.
            up.reset();
            let zeros = vec![0.0; max_block];
            let mut out0 = vec![0.0; max_block * ratio];
            let mut out1 = vec![0.0; max_block * ratio];
            for _ in 0..10 {
                up.process(&[&zeros, &zeros], &mut [&mut out0, &mut out1]);
                for &v in out0.iter().chain(out1.iter()) {
                    assert_eq!(v, 0.0);
                }
            }
        }
    }

    #[test]
    fn upsampler_4x_sine_spectral_images_below_minus_130_dbfs() {
        let d = UpsamplerDesign {
            ratio: 4,
            taps: 2048,
            phase: FilterPhase::Linear,
            kaiser_beta: 14.0,
            cutoff: 1.0,
            phase_mix: 1.0,
            preserve_samples: false,
            window: audivane_protocol::WindowKind::Kaiser,
            taper: 0.25,
        };
        let mut up = Upsampler::new(&d, 1, 512);
        let fs_in = 44_100.0;
        let fs_out = fs_in * 4.0;
        let n_fft = 16_384usize;
        // Coherent bin near 1 kHz inside the 16384-sample output FFT window.
        let bin = ((1000.0 * n_fft as f64) / fs_out).round() as usize;
        let f_tone = bin as f64 * fs_out / n_fft as f64;

        let settle_in = up.latency_out_frames() / 4 + 2048;
        let total_in = settle_in + n_fft / 4;
        let input: Vec<f64> = (0..total_in)
            .map(|n| (2.0 * PI * f_tone * (n as f64) / fs_in).sin())
            .collect();

        let mut output = vec![0.0; total_in * 4];
        for (in_chunk, out_chunk) in input.chunks(512).zip(output.chunks_mut(512 * 4)) {
            up.process(&[in_chunk], &mut [out_chunk]);
        }

        let start = output.len() - n_fft;
        let segment = &output[start..];
        let mut fft = RealFftPair::new(n_fft);
        let mut spec = vec![Complex64::new(0.0, 0.0); fft.spec_len()];
        fft.forward_padded_complex(segment, &mut spec);

        let fund_mag = spec[bin].norm();
        let mut max_spur_db = f64::NEG_INFINITY;
        for (k, c) in spec.iter().enumerate() {
            if k == bin {
                continue;
            }
            let rel_db = lin_to_db((c.norm() / fund_mag).max(1e-18));
            max_spur_db = max_spur_db.max(rel_db);
        }
        assert!(
            max_spur_db <= -130.0,
            "spectral image too high: {max_spur_db:.2} dBFS"
        );
    }

    fn response_config(ratio: u32, taps: usize, phase: FilterPhase) -> ManualConfig {
        ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(ratio),
            taps,
            phase,
            kaiser_beta: 14.0,
            cutoff: 1.0,
            cutoff_mode: CutoffMode::Fraction,
            ..Default::default()
        }
    }

    #[test]
    fn upsampler_response_matches_the_design() {
        let cfg = response_config(4, 4096, FilterPhase::Linear);
        let response = upsampler_response(&cfg, 44_100).expect("response");
        assert_eq!(response.source_rate, 44_100);
        assert_eq!(response.output_rate, 176_400);
        assert_eq!(response.ratio, 4);
        assert_eq!(response.taps, 4096);
        assert_eq!(response.phase, FilterPhase::Linear);
        assert!(
            response.passband_ripple_db < 0.01,
            "passband ripple {} dB",
            response.passband_ripple_db
        );
        // With cutoff 1.0 the -3 dB point sits at the source Nyquist.
        let nyquist = 44_100.0 / 2.0;
        assert!(
            (response.minus_3_db_hz - nyquist).abs() < 250.0,
            "-3 dB at {} Hz, expected ~{nyquist} Hz",
            response.minus_3_db_hz
        );
        assert!(response.minus_0_1_db_hz < response.minus_3_db_hz);
        assert!(
            response.stopband_atten_db > 100.0,
            "stopband only {:.1} dB",
            response.stopband_atten_db
        );
        assert!(response.stopband_start_hz >= nyquist - 1.0);
        assert!(response.stopband_start_hz <= response.output_rate as f64 / 2.0);
        assert!(!response.impulse.is_empty() && response.impulse.len() <= 256);
        assert!(response.impulse_peak < response.impulse.len());
        assert_eq!(response.impulse.len(), 256);
        assert!(response.latency_samples > 0);

        // The -3 dB point follows the cutoff fraction.
        let mut apodized = cfg.clone();
        apodized.cutoff = 0.95;
        let response = upsampler_response(&apodized, 44_100).expect("response");
        let expected = 0.95 * nyquist;
        assert!(
            (response.minus_3_db_hz - expected).abs() < 300.0,
            "cutoff 0.95: -3 dB at {} Hz, expected ~{expected} Hz",
            response.minus_3_db_hz
        );
        assert_eq!(response.freqs_hz.len(), response.magnitude_db.len());
        assert!(response.freqs_hz.windows(2).all(|w| w[0] < w[1]));
        assert!(response.freqs_hz.iter().all(|f| f.is_finite() && *f >= 0.0));
    }

    #[test]
    fn stopband_starts_above_nyquist_and_covers_the_settled_range() {
        let response = upsampler_response(&response_config(4, 4096, FilterPhase::Linear), 48_000)
            .expect("response");
        let nyquist = 48_000.0 / 2.0;
        let start = response.stopband_start_hz;
        let atten = response.stopband_atten_db;
        assert!(
            start >= nyquist,
            "stopband starts at {start} Hz, Nyquist {nyquist}"
        );
        assert!(start <= response.output_rate as f64 / 2.0);

        // Everything from `start` to the output Nyquist is at or below the
        // reported attenuation.
        let mut worst_after = f64::NEG_INFINITY;
        for (f, db) in response.freqs_hz.iter().zip(&response.magnitude_db) {
            if *f >= start {
                worst_after = worst_after.max(*db);
            }
        }
        assert!(
            worst_after <= -atten + 1.0e-6,
            "worst {worst_after} dB after {start} Hz is above -{atten} dB"
        );

        // The transition between Nyquist and `start` is allowed to be above
        // the stopband threshold: prove we did not just start at Nyquist.
        let transition_above = response
            .freqs_hz
            .iter()
            .zip(&response.magnitude_db)
            .any(|(f, db)| *f >= nyquist && *f < start && *db > -atten + 1.0);
        assert!(
            transition_above,
            "no transition point above -{atten} dB between {nyquist} and {start} Hz"
        );
        assert!(atten > 100.0, "stopband attenuation only {atten:.1} dB");
    }

    #[test]
    fn stopband_attenuation_is_an_envelope_of_a_dense_search() {
        // Review finding: the 16k-point stats grid missed sidelobe peaks, so
        // the summary claimed ~2 dB more than the response's own direct
        // evaluation. Now the summary must bound a dense direct search.
        for &(ratio, taps, phase, beta) in &[
            (16u32, 65_536usize, FilterPhase::Minimum, 20.0_f64),
            (4, 16_385, FilterPhase::Linear, 14.0),
            (2, 4_097, FilterPhase::Minimum, 8.0),
        ] {
            let cfg = ManualConfig {
                target: audivane_protocol::RateTarget::Ratio(ratio),
                taps,
                phase,
                kaiser_beta: beta,
                cutoff: 1.0,
                cutoff_mode: CutoffMode::NyquistStop,
                ..Default::default()
            };
            let m = measure_upsampler(&cfg, 48_000).expect("measure");
            let rate = f64::from(m.output_rate());
            let start = m.response.stopband_start_hz / rate;
            let (f, db) = dense_peak_above(&m.impulse, m.gain, start);
            assert!(
                db <= -m.response.stopband_atten_db + 1.0e-4,
                "x{ratio} {taps} {phase:?} beta {beta}: {db:.5} dB at {:.4} Hz above the \
                 reported -{:.5} dB",
                f * rate,
                m.response.stopband_atten_db
            );
        }
    }

    /// The reported stopband of `cfg` at `source` against an independent
    /// dense search from the reported start: never optimistic, and at most
    /// `slack_db` pessimistic. Returns (reported, dense) peak in dB.
    fn assert_stopband_summary_bounds(
        cfg: &ManualConfig,
        source: u32,
        slack_db: f64,
    ) -> (f64, f64) {
        let m = measure_upsampler(cfg, source).expect("measure");
        let rate = f64::from(m.output_rate());
        let start = m.response.stopband_start_hz;
        let reported = -m.response.stopband_atten_db;
        let (f, dense) = dense_peak_above(&m.impulse, m.gain, start / rate);
        let what = format!(
            "N={} x{} {:?} beta {} cutoff {}: reported {reported:.6} dB from {start:.4} Hz, \
             dense {dense:.6} dB at {:.4} Hz",
            cfg.taps,
            m.response.ratio,
            cfg.phase,
            cfg.kaiser_beta,
            cfg.cutoff,
            f * rate
        );
        assert!(dense <= reported + 1.0e-6, "optimistic: {what}");
        assert!(reported <= dense + slack_db, "too pessimistic: {what}");
        (reported, dense)
    }

    #[test]
    fn stopband_summary_finds_the_lobe_between_bins() {
        // At 48 kHz, x3, the first lobe after the transition (peak
        // -92.748 dB at 24134.27 Hz, above the -92.920 dB target) falls
        // between two FFT bins that are not local maxima: a sampled maximum
        // would report the transition as over.
        for phase in [FilterPhase::Linear, FilterPhase::Minimum] {
            let cfg = ManualConfig {
                target: audivane_protocol::RateTarget::Ratio(3),
                taps: 1084,
                phase,
                kaiser_beta: 9.281_026_226_494_81,
                cutoff: 0.988_318_032_981_387,
                cutoff_mode: CutoffMode::Fraction,
                ..Default::default()
            };
            let m = measure_upsampler(&cfg, 48_000).expect("measure");
            let lobe = m.magnitude_db_at(&[24_134.269_86])[0];
            let target = -kaiser_attenuation_db(cfg.kaiser_beta);
            assert!(lobe > target, "{phase:?}: the hidden lobe is at {lobe} dB");
            // It breaks the target, so the stopband starts after it.
            assert!(
                m.response.stopband_start_hz > 24_134.27,
                "{phase:?}: {:?}",
                m.response.stopband_start_hz
            );
            assert!(-m.response.stopband_atten_db <= target + 1e-9);
            let (reported, _) = assert_stopband_summary_bounds(&cfg, 48_000, 0.01);
            assert!((reported - -92.98).abs() < 0.01, "{phase:?}: {reported}");
        }
    }

    #[test]
    fn stopband_summary_bounds_random_designs() {
        // Seeded random Fraction designs, both phases: the summary is an
        // upper bound on the continuous stopband and within 0.02 dB of it.
        let mut seed = 0xabcd_u64;
        for i in 0..18 {
            let ratio = [2u32, 3, 4, 8, 16, 32][i % 6];
            let phase = if i % 2 == 0 {
                FilterPhase::Linear
            } else {
                FilterPhase::Minimum
            };
            let beta = 2.0 + 18.0 * lcg(&mut seed).abs();
            let taps = 32 + (lcg(&mut seed).abs() * 3000.0) as usize;
            let cutoff = 0.5 + 0.5 * lcg(&mut seed).abs();
            let cfg = ManualConfig {
                target: audivane_protocol::RateTarget::Ratio(ratio),
                taps,
                phase,
                kaiser_beta: beta,
                cutoff,
                cutoff_mode: CutoffMode::Fraction,
                ..Default::default()
            };
            assert_stopband_summary_bounds(&cfg, [44_100, 48_000][i % 2], 0.02);
        }
    }

    /// Brute-force direct evaluation of one frequency (no blocking).
    fn brute_db(h: &[f64], rate: f64, gain: f64, f: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &x) in h.iter().enumerate() {
            let (s, c) = (2.0 * PI * f * n as f64 / rate).sin_cos();
            re += x * c;
            im -= x * s;
        }
        crate::lin_to_db((re.hypot(im) / gain).max(1.0e-30))
    }

    #[test]
    fn blocked_dft_matches_the_brute_force_sum() {
        let measured =
            measure_upsampler(&response_config(4, 2048, FilterPhase::Minimum), 44_100).unwrap();
        let rate = f64::from(measured.output_rate());
        for f in [
            0.0, 1_000.0, 21_987.25, 22_050.0, 22_100.5, 40_000.0, 88_000.0,
        ] {
            let fast = measured.magnitude_db_at(&[f])[0];
            let slow = brute_db(&measured.impulse, rate, measured.gain, f);
            // Both sums round to ~1e-16 of the passband; deeper levels get
            // the corresponding relative tolerance.
            let tol = if slow > -100.0 {
                1.0e-9
            } else if slow > -180.0 {
                1.0e-4
            } else {
                1.0e-2
            };
            assert!((fast - slow).abs() < tol, "{f} Hz: {fast} vs {slow}");
        }
    }

    fn verify_crossings_with_dense_scan(
        cfg: &ManualConfig,
        source: u32,
        dense_margin_hz: f64,
        dense_step_hz: f64,
        fft_oversample: usize,
        label: &str,
    ) {
        let measured = measure_upsampler(cfg, source).unwrap();
        let rate = f64::from(measured.output_rate());
        let h = &measured.impulse;
        let gain = measured.gain;
        let crossings = &measured.response.crossings;
        assert!(!crossings.is_empty(), "{label}: no crossings reported");
        // Strictly non-decreasing: deeper levels cannot cross earlier.
        assert!(
            crossings.windows(2).all(|w| w[0].freq_hz <= w[1].freq_hz),
            "{label}: crossings not monotone: {crossings:?}"
        );
        // Every level down to the stopband attenuation is reported.
        let nyquist_out = rate / 2.0;
        let expected = RESPONSE_CROSSING_LEVELS_DB
            .iter()
            .filter(|l| **l >= -(measured.response.stopband_atten_db - 3.0))
            .count();
        assert!(
            crossings.len() + 1 >= expected,
            "{label}: missing levels {crossings:?}"
        );

        let at = |level: f64| crossings.iter().find(|c| c.level_db == level).unwrap();
        assert_eq!(measured.response.minus_3_db_hz, at(-3.0).freq_hz);
        assert_eq!(measured.response.minus_0_1_db_hz, at(-0.1).freq_hz);

        // 1) Global dense FFT scan from 0 Hz up to the last crossing to prove no
        //    earlier passband ripple dip was skipped anywhere in 0..crossing.
        let nfft = (h.len() * fft_oversample).next_power_of_two();
        let mut fft = RealFftPair::new(nfft);
        let mut spec = vec![Complex64::new(0.0, 0.0); fft.spec_len()];
        fft.forward_padded_complex(h, &mut spec);
        let fft_bin_hz = rate / nfft as f64;
        let fft_db: Vec<f64> = spec
            .iter()
            .map(|c| lin_to_db((c.norm() / gain).max(1.0e-30)))
            .collect();
        drop(spec);
        drop(fft);

        for c in crossings {
            if let Some(first_fft_bin) = fft_db.iter().position(|&db| db <= c.level_db) {
                let first_fft_hz = first_fft_bin as f64 * fft_bin_hz;
                assert!(
                    c.freq_hz <= first_fft_hz + 0.01,
                    "{label} {} dB: reported {} Hz is later than FFT dip at {} Hz",
                    c.level_db,
                    c.freq_hz,
                    first_fft_hz
                );
            }
        }

        // 2) Dense direct-DFT brute-force scan covering both a local window
        //    around every shallow crossing and the entire transition skirt
        //    (from `-0.5 dB - dense_margin_hz` through the last crossing),
        //    batched cleanly so even 4M-tap verification runs in seconds.
        let skirt_anchor = crossings
            .iter()
            .find(|c| c.level_db <= -0.5)
            .map_or(crossings[0].freq_hz, |c| c.freq_hz);
        let skirt_lo = (skirt_anchor - dense_margin_hz).max(0.0);
        let skirt_hi = (crossings.last().unwrap().freq_hz + 0.02).min(nyquist_out);

        let mut windows: Vec<(f64, f64, f64)> = Vec::new();
        for c in crossings {
            if c.freq_hz < skirt_lo {
                let lo = (c.freq_hz - dense_margin_hz).max(0.0);
                let hi = (c.freq_hz + 0.02).min(nyquist_out);
                windows.push((lo, hi, dense_step_hz));
            }
        }
        windows.push((skirt_lo, skirt_hi, dense_step_hz));

        for &(win_lo, win_hi, step) in &windows {
            let count = ((win_hi - win_lo) / step).ceil() as usize + 1;
            let grid: Vec<f64> = (0..count)
                .map(|i| (win_lo + i as f64 * step).min(win_hi))
                .collect();
            let dense = measured.magnitude_db_at(&grid);
            for c in crossings {
                if c.freq_hz < win_lo || c.freq_hz > win_hi {
                    continue;
                }
                let i = dense
                    .iter()
                    .position(|db| *db <= c.level_db)
                    .unwrap_or_else(|| {
                        panic!(
                            "{label} {} dB ({} Hz): dense scan in [{win_lo}, {win_hi}] never crossed",
                            c.level_db, c.freq_hz
                        )
                    });
                let g0 = grid[i.saturating_sub(1)];
                let fine_step = (step / 20.0).min(0.0002);
                let fine_freqs: Vec<f64> = (0..=((grid[i] - g0) / fine_step).ceil() as usize + 1)
                    .map(|j| g0 + j as f64 * fine_step)
                    .collect();
                let fine_vals = measured.magnitude_db_at(&fine_freqs);
                let g = if let Some(j) = fine_vals.iter().position(|db| *db <= c.level_db) {
                    fine_freqs[j]
                } else {
                    grid[i]
                };
                assert!(
                    (g - c.freq_hz).abs() < 0.01,
                    "{label} {} dB: reported {} Hz, dense brute-force earliest {g} Hz",
                    c.level_db,
                    c.freq_hz
                );
            }
        }
    }

    #[test]
    fn crossings_match_a_dense_brute_force_scan() {
        for (cfg, source, label) in [
            (
                response_config(2, 2048, FilterPhase::Linear),
                44_100u32,
                "2k x2 linear",
            ),
            (
                ManualConfig {
                    cutoff: 0.95,
                    ..response_config(4, 4096, FilterPhase::Minimum)
                },
                48_000,
                "4k x4 min",
            ),
            (
                ManualConfig {
                    kaiser_beta: 8.0,
                    ..response_config(2, 16_384, FilterPhase::Linear)
                },
                48_000,
                "16k beta 8 x2 linear",
            ),
            (
                ManualConfig {
                    kaiser_beta: 8.0,
                    ..response_config(2, 16_384, FilterPhase::Minimum)
                },
                48_000,
                "16k beta 8 x2 minimum",
            ),
        ] {
            verify_crossings_with_dense_scan(&cfg, source, 40.0, 0.05, 8, label);
        }
    }

    #[test]
    fn crossings_1m_and_4m_match_dense_scan() {
        for (taps, phase, fft_os, label) in [
            (1_000_000usize, FilterPhase::Linear, 4usize, "1M x2 linear"),
            (1_000_000, FilterPhase::Minimum, 4, "1M x2 minimum"),
            (4_000_000, FilterPhase::Linear, 4, "4M x2 linear"),
            // The longest reflected minimum phase within the 1.5 GiB design
            // workspace limit (a 4M one needs about 2 GB to design; the
            // cepstral cap is the presets' transform, not this one).
            (2_097_152, FilterPhase::Minimum, 4, "2M x2 minimum"),
        ] {
            let cfg = response_config(2, taps, phase);
            let lobe_hz = 96_000.0 / taps as f64;
            verify_crossings_with_dense_scan(
                &cfg,
                48_000,
                8.0 * lobe_hz,
                0.005 * lobe_hz,
                fft_os,
                label,
            );
        }
        let e = measure_upsampler(&response_config(2, 4_000_000, FilterPhase::Minimum), 48_000)
            .err()
            .unwrap();
        assert!(e.contains("GB design limit"), "{e}");
    }

    #[test]
    fn window_agrees_with_the_full_response_at_shared_frequencies() {
        let measured =
            measure_upsampler(&response_config(4, 4096, FilterPhase::Linear), 48_000).unwrap();
        let full = &measured.response;
        // A window from one plotted point to another, with points landing on
        // several plotted frequencies of the dense transition grid.
        let lo_index = full
            .freqs_hz
            .iter()
            .position(|&f| f >= 0.8 * 24_000.0)
            .unwrap();
        let hi_index = lo_index + 40;
        let f_lo = full.freqs_hz[lo_index];
        let f_hi = full.freqs_hz[hi_index];
        let response = measured.with_window(Some(ResponseWindowRequest {
            f_lo_hz: f_lo,
            f_hi_hz: f_hi,
            points: 41,
        }));
        let window = response.window.expect("window");
        assert_eq!(window.freqs_hz.len(), 41);
        assert_eq!(window.freqs_hz[0], f_lo);
        assert_eq!(window.freqs_hz[40], f_hi);
        for (f, db) in window.freqs_hz.iter().zip(&window.magnitude_db) {
            if let Some(i) = full.freqs_hz.iter().position(|g| (g - f).abs() < 1.0e-9) {
                assert!(
                    (full.magnitude_db[i] - db).abs() < 1.0e-6,
                    "{f} Hz: window {db} vs full {}",
                    full.magnitude_db[i]
                );
            }
        }
        // The two ends are plotted points.
        assert!((window.magnitude_db[0] - full.magnitude_db[lo_index]).abs() < 1.0e-6);
        assert!((window.magnitude_db[40] - full.magnitude_db[hi_index]).abs() < 1.0e-6);
        // Points are clamped.
        let tiny = measured.with_window(Some(ResponseWindowRequest {
            f_lo_hz: 1_000.0,
            f_hi_hz: 2_000.0,
            points: 0,
        }));
        assert_eq!(tiny.window.unwrap().freqs_hz.len(), 2);
    }

    #[test]
    fn ratio_one_window_is_flat() {
        let measured =
            measure_upsampler(&response_config(1, 4096, FilterPhase::Linear), 48_000).unwrap();
        assert!(measured.response.crossings.is_empty());
        let window = measured
            .with_window(Some(ResponseWindowRequest {
                f_lo_hz: 0.0,
                f_hi_hz: 24_000.0,
                points: 5,
            }))
            .window
            .unwrap();
        assert!(window.magnitude_db.iter().all(|db| db.abs() < 1.0e-12));
    }

    #[cfg(target_os = "linux")]
    fn peak_rss_mib() -> f64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                let kb: f64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                return kb / 1024.0;
            }
        }
        0.0
    }

    #[cfg(not(target_os = "linux"))]
    fn peak_rss_mib() -> f64 {
        0.0
    }

    /// Prints wall time and peak RSS for the response measurement of the
    /// longest designs: 4M linear taps, and the longest minimum phase, 2^21
    /// taps (a Custom minimum phase is reflected: a 4M one needs about 2 GB
    /// to design, over the 1.5 GiB design workspace limit; the cepstral cap
    /// does not apply to it). Run with `cargo test -p audivane-dsp-core
    /// bench_upsampler_response -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_upsampler_response_long_kernel() {
        use std::time::Instant;
        for (phase, taps) in [
            (FilterPhase::Linear, 4_000_000),
            (FilterPhase::Minimum, 2_097_152),
        ] {
            let cfg = response_config(2, taps, phase);
            let started = Instant::now();
            let measured = measure_upsampler(&cfg, 48_000).expect("response");
            let elapsed = started.elapsed().as_secs_f64();
            let started = Instant::now();
            let _ = measured.with_window(Some(ResponseWindowRequest {
                f_lo_hz: 23_900.0,
                f_hi_hz: 24_100.0,
                points: 1_000,
            }));
            eprintln!(
                "  window of 1000 points: {:.2} s; crossings {:?}",
                started.elapsed().as_secs_f64(),
                measured.response.crossings
            );
            let response = measured.response;
            eprintln!(
                "upsampler_response {taps} taps x2 {:?}: {elapsed:.2} s, peak RSS {:.1} MiB, \
                 stopband {:.1} dB from {:.0} Hz",
                phase,
                peak_rss_mib(),
                response.stopband_atten_db,
                response.stopband_start_hz
            );
            assert!(response.stopband_atten_db > 100.0);
        }
    }

    /// Wall time of the response measurement at x32 against x64 for the
    /// longest designs of each kind. Run with `cargo test --release -p
    /// audivane-dsp-core bench_upsampler_response_x64 -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore]
    fn bench_upsampler_response_x64() {
        use audivane_protocol::FilterPreset::*;
        use std::time::Instant;
        let manual = |taps, phase| response_config(1, taps, phase);
        let preset = |p| ManualConfig {
            preset: Some(p),
            ..response_config(1, 4096, FilterPhase::Linear)
        };
        let cases = [
            ("manual 65536 linear", manual(65_536, FilterPhase::Linear)),
            ("manual 4M linear", manual(4_194_304, FilterPhase::Linear)),
            ("manual 1M minimum", manual(1_048_576, FilterPhase::Minimum)),
            ("lanczos32767", preset(Lanczos32767)),
            ("flattop_huge", preset(FlattopHuge)),
            ("kaiser_steep_mp", preset(KaiserSteepMp)),
            ("cascade_fir_mp", preset(CascadeFirMp)),
            ("block_fft_large", preset(BlockFftLarge)),
            ("table_sinc", preset(TableSinc)),
        ];
        for (name, cfg) in cases {
            for ratio in [32, 64] {
                let cfg = ManualConfig {
                    target: audivane_protocol::RateTarget::Ratio(ratio),
                    ..cfg.clone()
                };
                let started = Instant::now();
                let measured = measure_upsampler(&cfg, 44_100).expect("response");
                eprintln!(
                    "{name} x{ratio}: {:.2} s, peak RSS {:.0} MiB, stopband {:.1} dB",
                    started.elapsed().as_secs_f64(),
                    peak_rss_mib(),
                    measured.response.stopband_atten_db
                );
            }
        }
    }

    #[test]
    fn upsampler_response_minimum_phase_has_the_same_magnitude() {
        let linear = upsampler_response(&response_config(4, 4096, FilterPhase::Linear), 48_000)
            .expect("linear response");
        let minimum = upsampler_response(&response_config(4, 4096, FilterPhase::Minimum), 48_000)
            .expect("minimum response");
        assert_eq!(linear.freqs_hz, minimum.freqs_hz);
        // The two phases share the passband magnitude. Deep in the stopband
        // the f64 noise floors differ, so compare the audible range and the
        // stopband attenuation rather than individual -150 dB bins.
        let passband_top = 0.9 * 48_000.0 / 2.0;
        let max_pass_diff = linear
            .freqs_hz
            .iter()
            .zip(&linear.magnitude_db)
            .zip(&minimum.magnitude_db)
            .filter(|((freq, _), _)| **freq <= passband_top)
            .map(|((_, a), b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            max_pass_diff < 0.01,
            "min-phase passband magnitude differs by {max_pass_diff:.6} dB"
        );
        assert!(
            (minimum.passband_ripple_db - linear.passband_ripple_db).abs() < 0.01,
            "ripple {} vs {}",
            minimum.passband_ripple_db,
            linear.passband_ripple_db
        );
        assert!(
            minimum.stopband_atten_db > 100.0,
            "min-phase stopband {:.1} dB",
            minimum.stopband_atten_db
        );
        assert!(
            (minimum.stopband_atten_db - linear.stopband_atten_db).abs() < 3.0,
            "stopband {} vs {} dB",
            minimum.stopband_atten_db,
            linear.stopband_atten_db
        );
    }

    /// `upsampler_response` reports a Custom minimum-phase design's peak in
    /// `latency_samples`: at x4 the engine block's latency plus the peak;
    /// at 160/147 the grid peak / M rounded (823 / 147 = 5.6 -> 6).
    #[test]
    fn upsampler_response_latency_of_custom_minimum_phase_is_its_peak() {
        let cfg = |target| ManualConfig {
            target,
            taps: 4096,
            phase: FilterPhase::Minimum,
            ..ManualConfig::default()
        };
        let x4 = upsampler_response(&cfg(audivane_protocol::RateTarget::Ratio(4)), 48_000).unwrap();
        // The default engine block (1024 x 4) + peak 19.
        assert_eq!(x4.latency_samples, 4096 + 19);
        let x4 = measure_upsampler_at(&cfg(audivane_protocol::RateTarget::Ratio(4)), 48_000, 512)
            .unwrap();
        assert_eq!(x4.response.latency_samples, 2048 + 19);
        let r =
            upsampler_response(&cfg(audivane_protocol::RateTarget::Rate(48_000)), 44_100).unwrap();
        assert_eq!(r.latency_samples, 6);
    }

    #[test]
    fn upsampler_response_ratio_one_is_flat() {
        let cfg = ManualConfig {
            target: audivane_protocol::RateTarget::Off,
            taps: 4096,
            ..ManualConfig::default()
        };
        let response = upsampler_response(&cfg, 44_100).expect("response");
        assert_eq!(response.ratio, 1);
        assert_eq!(response.output_rate, 44_100);
        assert_eq!(response.latency_samples, 0);
        assert_eq!(response.impulse, vec![1.0]);
        assert_eq!(response.impulse_peak, 0);
        assert_eq!(response.passband_ripple_db, 0.0);
        assert_eq!(response.stopband_atten_db, 0.0);
        assert!(
            response.magnitude_db.iter().all(|db| db.abs() < 1.0e-9),
            "ratio 1 must be flat"
        );
        let nyquist = 44_100.0 / 2.0;
        assert!(response.freqs_hz.iter().all(|f| *f <= nyquist + 1.0e-9));
    }

    #[test]
    fn resample_offline_length_sine_and_roundtrip() {
        // Length rule
        assert_eq!(resample_offline(&[], 44_100, 48_000).len(), 0);
        assert_eq!(
            resample_offline(&vec![0.0; 1000], 48_000, 44_100).len(),
            919
        );
        assert_eq!(
            resample_offline(&vec![0.0; 44_100], 44_100, 48_000).len(),
            48_000
        );

        // Sine preservation (48k -> 44.1k, 5 kHz passband + 23 kHz stopband alias test)
        let n_48 = 12_000;
        let sig_5k: Vec<f64> = (0..n_48)
            .map(|n| (2.0 * PI * 5000.0 * n as f64 / 48_000.0).sin())
            .collect();
        let res_5k = resample_offline(&sig_5k, 48_000, 44_100);
        assert_eq!(res_5k.len(), 11_025);
        let mut max_err_5k = 0.0_f64;
        for (m, &y) in res_5k.iter().enumerate().take(res_5k.len() - 400).skip(400) {
            let expected = (2.0 * PI * 5000.0 * m as f64 / 44_100.0).sin();
            max_err_5k = max_err_5k.max((y - expected).abs());
        }
        assert!(
            max_err_5k < 1e-6,
            "5 kHz sine error too large: {max_err_5k:.3e}"
        );

        let sig_23k: Vec<f64> = (0..n_48)
            .map(|n| (2.0 * PI * 23_000.0 * n as f64 / 48_000.0).sin())
            .collect();
        let res_23k = resample_offline(&sig_23k, 48_000, 44_100);
        let max_alias = res_23k[400..res_23k.len() - 400]
            .iter()
            .map(|v| v.abs())
            .fold(0.0_f64, f64::max);
        let alias_db = lin_to_db(max_alias.max(1e-18));
        assert!(
            alias_db <= -120.0,
            "offline resampler stopband/alias attenuation insufficient: {alias_db:.2} dB"
        );

        // Round-trip 44.1k -> 48k -> 44.1k in passband (1 kHz + 18 kHz)
        let n_44 = 11_025;
        let orig: Vec<f64> = (0..n_44)
            .map(|n| {
                let t = n as f64 / 44_100.0;
                0.5 * (2.0 * PI * 1000.0 * t).sin() + 0.5 * (2.0 * PI * 18_000.0 * t).sin()
            })
            .collect();
        let at_48 = resample_offline(&orig, 44_100, 48_000);
        let rt = resample_offline(&at_48, 48_000, 44_100);
        assert_eq!(rt.len(), orig.len());
        let rt_err = rt[800..rt.len() - 800]
            .iter()
            .zip(&orig[800..orig.len() - 800])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        assert!(rt_err < 1e-6, "round-trip error too large: {rt_err:.3e}");
    }

    #[test]
    #[ignore]
    fn bench_upsampler_and_offline_resampler() {
        use std::time::Instant;

        for taps in [100_000usize, 1_000_000usize] {
            let d = UpsamplerDesign {
                ratio: 16,
                taps,
                phase: FilterPhase::Linear,
                kaiser_beta: 14.0,
                cutoff: 1.0,
                phase_mix: 1.0,
                preserve_samples: false,
                window: audivane_protocol::WindowKind::Kaiser,
                taper: 0.25,
            };
            let block_in = 1024usize;
            let mut up = Upsampler::new(&d, 2, block_in);

            let in0 = vec![0.1; block_in];
            let in1 = vec![-0.1; block_in];
            let mut out0 = vec![0.0; block_in * 16];
            let mut out1 = vec![0.0; block_in * 16];

            // Warmup
            for _ in 0..20 {
                up.process(&[&in0, &in1], &mut [&mut out0, &mut out1]);
            }

            let iters = if taps <= 100_000 { 400 } else { 150 };
            let t0 = Instant::now();
            for _ in 0..iters {
                up.process(&[&in0, &in1], &mut [&mut out0, &mut out1]);
            }
            let elapsed = t0.elapsed().as_secs_f64();
            let audio_secs = (iters * block_in) as f64 / 44_100.0;
            let rtf = audio_secs / elapsed;
            eprintln!(
                "Upsampler {taps} taps x16 stereo @ 44.1kHz (block {block_in}): {audio_secs:.2}s audio in {elapsed:.4}s => {rtf:.2}x real-time"
            );
        }

        let big = vec![0.25_f64; 1_000_000];
        let t0 = Instant::now();
        let res = resample_offline(&big, 48_000, 44_100);
        let elapsed = t0.elapsed().as_secs_f64();
        eprintln!(
            "resample_offline 1M samples 48k->44.1k -> {} samples in {elapsed:.4}s",
            res.len()
        );
    }

    /// Worst level (dB re `gain`) of `h` from `f_lo` (cycles per sample) to
    /// half the sample rate, independently of the production summary: every
    /// local maximum of a 16x oversampled FFT (8x from 256k taps, 4x from 1M)
    /// within 20 dB of the highest (at most 256, 64 from 256k taps) is
    /// refined by golden-section search on direct evaluations over its two
    /// neighbouring bins, and `f_lo` itself is evaluated. Returns `(f, dB)`.
    fn dense_peak_above(h: &[f64], gain: f64, f_lo: f64) -> (f64, f64) {
        let (oversample, max_cands) = match h.len() {
            0..=262_143 => (16, 256),
            262_144..=1_048_575 => (8, 64),
            _ => (4, 64),
        };
        let nfft = (h.len() * oversample).next_power_of_two().max(4096);
        let mag = magnitude_spectrum(h, nfft);
        let max_bin = mag.len() - 1;
        let first = ((f_lo * nfft as f64).ceil() as usize).min(max_bin);
        let top = mag[first..].iter().copied().fold(0.0_f64, f64::max);
        let mut cands: Vec<usize> = (first..=max_bin)
            .filter(|&k| {
                mag[k] >= top * 0.1
                    && (k == first || mag[k] >= mag[k - 1])
                    && (k == max_bin || mag[k] >= mag[k + 1])
            })
            .collect();
        cands.sort_by(|a, b| mag[*b].total_cmp(&mag[*a]));
        cands.truncate(max_cands);
        let db = |m: f64| lin_to_db((m / gain).max(1.0e-30));
        let mut best = (f_lo, db(dft_magnitudes(h, 1.0, &[f_lo])[0]));
        for k in cands {
            let mut lo = ((k as f64 - 1.0) / nfft as f64).max(f_lo);
            let mut hi = ((k as f64 + 1.0) / nfft as f64).min(0.5);
            // 16 iterations narrow the bin to 8.5e-4 of it, far under the
            // 0.02 dB tolerances.
            for _ in 0..16 {
                let a = lo + (hi - lo) * 0.381_966_011_250_105_1;
                let b = lo + (hi - lo) * 0.618_033_988_749_894_9;
                let v = dft_magnitudes(h, 1.0, &[a, b]);
                if v[0] > v[1] {
                    hi = b;
                } else {
                    lo = a;
                }
            }
            let f = 0.5 * (lo + hi);
            let v = db(dft_magnitudes(h, 1.0, &[f])[0]);
            if v > best.1 {
                best = (f, v);
            }
        }
        best
    }

    /// Margin (dB) by which the measured engine response stays under `-A`
    /// over the whole band from the source to the output Nyquist (negative:
    /// below the target). Also returns the effective cutoff.
    fn nyquist_stop_margin(
        taps: usize,
        ratio: u32,
        phase: FilterPhase,
        beta: f64,
        source_rate: u32,
    ) -> (f64, f64) {
        let cfg = ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(ratio),
            taps,
            phase,
            kaiser_beta: beta,
            cutoff: 1.0,
            cutoff_mode: CutoffMode::NyquistStop,
            ..Default::default()
        };
        let measured = measure_upsampler(&cfg, source_rate).expect("measure");
        let expected = nyquist_stop_check(taps, ratio as usize, beta).expect("feasible");
        assert!((measured.response.effective_cutoff - expected).abs() < 1e-12);
        let (_, peak_db) =
            dense_peak_above(&measured.impulse, measured.gain, 0.5 / f64::from(ratio));
        (peak_db + kaiser_attenuation_db(beta), expected)
    }

    /// Numerical tolerance on `-A` for the dense peak search: the minimum-phase
    /// conversion and the DFT agree with the linear twin to ~0.01 dB at
    /// -190 dB.
    const NYQUIST_STOP_TOL_DB: f64 = 0.02;

    #[test]
    fn nyquist_stop_feasibility_and_shift() {
        assert_eq!(nyquist_stop_cutoff(16_384, 1, 14.0), 1.0);
        assert_eq!(nyquist_stop_check(16_384, 1, 14.0), Ok(1.0));
        // A clamp example: 64 taps x16 beta 20 cannot stop at Nyquist
        // (cutoff 0.5 would leave -6.96 dB there).
        let err = nyquist_stop_check(64, 16, 20.0).unwrap_err();
        let min = nyquist_stop_min_taps(16, 20.0).unwrap();
        assert_eq!(
            err,
            NyquistStopError::TooShort {
                taps: 64,
                min_taps: min,
                ratio: 16
            }
        );
        assert!(err.to_string().contains("only guaranteed from"), "{err}");
        assert!(nyquist_stop_check(min, 16, 20.0).unwrap() >= NYQUIST_STOP_MIN_CUTOFF);
        assert!(nyquist_stop_check(min - 1, 16, 20.0).is_err());
        assert!(err.to_string().contains(&min.to_string()));
        assert!(matches!(
            nyquist_stop_check(65_536, 4, 4.0),
            Err(NyquistStopError::Beta(_))
        ));
        assert!(matches!(
            nyquist_stop_check(65_536, 4, 25.0),
            Err(NyquistStopError::Beta(_))
        ));
        assert_eq!(nyquist_stop_min_taps(4, 30.0), None);
        // Shared with app/src/lib/upsamplerLimits.ts (see its test).
        for &(ratio, beta, expect) in &[
            (2usize, 8.0_f64, 32usize),
            (16, 14.0, 433),
            (32, 20.0, 1254),
            (32, 24.0, 1653),
            (16, 5.0, 157),
        ] {
            assert_eq!(
                nyquist_stop_min_taps(ratio, beta),
                Some(expect),
                "x{ratio} beta {beta}"
            );
        }

        // 1M taps at 48 kHz, beta 14: the cutoff shift below Nyquist.
        let c16 = nyquist_stop_cutoff(1_048_576, 16, 14.0);
        let c2 = nyquist_stop_cutoff(1_048_576, 2, 14.0);
        assert!(c16 > 0.99 && c16 < 1.0 && c2 > c16 && c2 < 1.0);
        let shift16_hz = (1.0 - c16) * 24_000.0;
        assert!((shift16_hz - 3.88).abs() < 0.01, "shift16={shift16_hz}");
    }

    #[test]
    fn nyquist_stop_holds_at_the_feasibility_edge() {
        // Shortest feasible kernels (cutoff near 0.5) and a few times longer,
        // at the betas where the calibration has its teeth, kernel level.
        for &ratio in &[2usize, 4, 16, 32] {
            for &beta in &[5.0_f64, 5.25, 9.25, 14.0, 17.75, 20.0, 22.0, 23.75, 24.0] {
                let min_taps = nyquist_stop_min_taps(ratio, beta).unwrap();
                for taps in [min_taps, min_taps + 1, 2 * min_taps, 7 * min_taps] {
                    let cutoff = nyquist_stop_check(taps, ratio, beta).unwrap();
                    let lin = design(ratio, taps, FilterPhase::Linear, beta, cutoff);
                    let (f, db) = dense_peak_above(&lin, ratio as f64, 0.5 / ratio as f64);
                    let a = kaiser_attenuation_db(beta);
                    assert!(
                        db <= -a + NYQUIST_STOP_TOL_DB,
                        "N={taps} x{ratio} beta={beta}: {db:.3} dB at {f:.6} > -{a:.3} dB"
                    );
                }
            }
        }
    }

    #[test]
    fn nyquist_stop_verifies_stopband_at_nyquist() {
        // The measured engine response, whole band above Nyquist, odd and
        // non-power-of-two lengths, both phases, both rate families.
        for &(taps, ratio) in &[(16_385usize, 16u32), (16_384, 2), (4_097, 8), (65_535, 4)] {
            for phase in [FilterPhase::Linear, FilterPhase::Minimum] {
                for &beta in &[8.0_f64, 14.0, 20.0] {
                    for &rate in &[44_100u32, 48_000] {
                        let (margin, cutoff) = nyquist_stop_margin(taps, ratio, phase, beta, rate);
                        assert!(
                            margin <= NYQUIST_STOP_TOL_DB,
                            "taps={taps} x{ratio} {phase:?} beta={beta} {rate} Hz: \
                             {margin:+.3} dB over -A (cutoff {cutoff:.8})"
                        );
                    }
                }
            }
        }
    }

    /// The UI's filter-length steps (16k..4M) include 32k, 128k and 512k, and
    /// the Kaiser presets include beta 24 for linear phase: all design.
    #[test]
    fn ui_length_steps_and_beta_24_design() {
        for &taps in &[32_768usize, 131_072, 524_288] {
            let h = design(4, taps, FilterPhase::Linear, 14.0, 0.95);
            assert_eq!(h.len(), taps);
            assert!(h.iter().all(|v| v.is_finite()));
            assert!(
                (h[0] - h[taps - 1]).abs() < 1e-15,
                "linear phase is symmetric"
            );
            let dc: f64 = h.iter().sum();
            assert!((dc - 4.0).abs() < 1e-6, "taps={taps}: DC gain {dc}");
        }
        let h = design(4, 32_768, FilterPhase::Linear, 24.0, 0.95);
        assert!(h.iter().all(|v| v.is_finite()));
        let dc: f64 = h.iter().sum();
        assert!((dc - 4.0).abs() < 1e-6, "beta 24: DC gain {dc}");
        // Beta 24 is the top of the "Stop at Nyquist" range.
        assert!(nyquist_stop_check(131_072, 4, 24.0).is_ok());
        assert!(nyquist_stop_check(131_072, 4, 24.5).is_err());
    }

    /// Full verification matrix: the requested odd/non-power-of-two lengths
    /// up to the 4M limit, every ratio, both phases, beta 8/14/20, both rate
    /// families, dense peak search over [source Nyquist, output Nyquist].
    /// Run with `cargo test -p audivane-dsp-core --release
    /// nyquist_stop_full_matrix_verification -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn nyquist_stop_full_matrix_verification() {
        let mut count = 0usize;
        let mut worst = f64::NEG_INFINITY;
        for &taps in &[16_385usize, 65_536, 1_000_001, 4_194_303, 4_194_304] {
            for &beta in &[8.0_f64, 14.0, 20.0] {
                for phase in [FilterPhase::Linear, FilterPhase::Minimum] {
                    let mut group = f64::NEG_INFINITY;
                    for &ratio in &[2u32, 4, 8, 16] {
                        for &rate in &[44_100u32, 48_000] {
                            let (margin, _) = nyquist_stop_margin(taps, ratio, phase, beta, rate);
                            assert!(
                                margin <= NYQUIST_STOP_TOL_DB,
                                "taps={taps} x{ratio} {phase:?} beta={beta} {rate} Hz: {margin:+.3} dB"
                            );
                            group = group.max(margin);
                            worst = worst.max(margin);
                            count += 1;
                        }
                    }
                    eprintln!(
                        "taps={taps:>7} beta={beta:>2.0} {phase:?}: worst peak above Nyquist \
                         {group:+.3} dB re -A"
                    );
                }
            }
        }
        eprintln!("ALL {count} CASES PASSED (worst {worst:+.3} dB re -A)");
    }

    // ------------------------------------------------------------ band mode

    fn band(pe: f64, se: f64, a: f64) -> BandSpec {
        BandSpec {
            passband_end: pe,
            passband_end_hz: None,
            stopband_start: se,
            attenuation_db: a,
        }
    }

    /// Largest |dB| deviation from 0 over `[0, f_hi]` (cycles per output
    /// sample): a 16x FFT grid plus golden-section refinement of its worst
    /// bins and the edge itself.
    fn passband_deviation_db(h: &[f64], gain: f64, f_hi: f64) -> f64 {
        let nfft = (h.len() * 16).next_power_of_two().max(4096);
        let mag = magnitude_spectrum(h, nfft);
        let last = ((f_hi * nfft as f64).floor() as usize).min(mag.len() - 1);
        let dev = |m: f64| lin_to_db(m / gain).abs();
        let mut worst = dev(dft_magnitudes(h, 1.0, &[f_hi])[0]);
        let mut cands: Vec<usize> = (0..=last).collect();
        cands.sort_by(|a, b| dev(mag[*b]).total_cmp(&dev(mag[*a])));
        // The worst eight bins of the grid, and the edge itself.
        cands.truncate(8);
        for k in cands {
            let lo0 = ((k as f64 - 1.0) / nfft as f64).max(0.0);
            let hi0 = ((k as f64 + 1.0) / nfft as f64).min(f_hi);
            for sign in [1.0, -1.0] {
                // Maximise sign * (|H| - gain) on [lo, hi].
                let (mut lo, mut hi) = (lo0, hi0);
                // 16 iterations: 8.5e-4 of a bin, far under the tolerances.
                for _ in 0..16 {
                    let a = lo + (hi - lo) * 0.381_966_011_250_105_1;
                    let b = lo + (hi - lo) * 0.618_033_988_749_894_9;
                    let v = dft_magnitudes(h, 1.0, &[a, b]);
                    if sign * v[0] > sign * v[1] {
                        hi = b;
                    } else {
                        lo = a;
                    }
                }
                let f = 0.5 * (lo + hi);
                worst = worst.max(dev(dft_magnitudes(h, 1.0, &[f])[0]));
            }
            worst = worst.max(dev(mag[k]));
        }
        worst
    }

    /// An absolute passband end (Hz) needs the source rate to become a
    /// fraction: the band builder, which has only the ratio, refuses an
    /// unresolved one.
    #[test]
    fn a_band_passband_in_hz_needs_the_source_rate() {
        let spec = BandSpec {
            passband_end_hz: Some(20_000.0),
            ..band(0.9, 1.0, 150.0)
        };
        assert_eq!(band_design(&spec, 2), Err(BandError::Hertz(20_000.0)));
    }

    #[test]
    fn band_design_meets_both_edges() {
        let specs = [
            band(0.9, 1.0, 100.0),
            band(0.95, 1.0, 150.0),
            band(0.8, 1.2, 60.0),
            band(0.9, 1.05, 200.0),
            band(0.5, 0.7, 120.0),
            band(0.99, 1.0, 140.0),
            band(1.0, 1.5, 225.0),
            band(0.9, 0.95, 55.0),
            // Absolute passband ends resolved at high source rates (20 kHz
            // from 96 to 768 kHz), designed deeper by the wide-band margin.
            band(20_000.0 / 48_000.0, 1.0, 150.0),
            band(20_000.0 / 96_000.0, 1.0, 150.0),
            band(20_000.0 / 192_000.0, 1.0, 120.0),
            band(20_000.0 / 192_000.0, 1.0, 150.0),
            band(20_000.0 / 384_000.0, 1.0, 150.0),
            band(0.3, 1.0, 205.0),
            // A stopband past the Nyquist (23 kHz at 96 kHz, 1.45), a wide
            // one deep, and bands past 215 dB, where the transition is
            // narrowed instead of the margin.
            band(23_000.0 / 48_000.0, 1.45, 160.0),
            band(0.45, 1.4, 205.0),
            band(0.3, 1.1, 225.0),
            band(0.49, 1.1, 222.0),
            band(0.46, 1.45, 222.0),
        ];
        for spec in specs {
            for ratio in [2usize, 3, 4, 8, 16] {
                let b = band_design(&spec, ratio).unwrap();
                let d = UpsamplerDesign {
                    taps: b.taps,
                    ..UpsamplerDesign::linear(ratio, b.taps, b.kaiser_beta, b.cutoff)
                };
                let h = design_kernel(&d);
                let gain = ratio as f64;
                let r2 = 2.0 * ratio as f64;
                let (_, stop_db) = dense_peak_above(&h, gain, spec.stopband_start / r2);
                assert!(
                    stop_db <= -spec.attenuation_db + 0.02,
                    "{spec:?} x{ratio} ({} taps): stopband peak {stop_db:.3} dB",
                    b.taps
                );
                let pass_db = passband_deviation_db(&h, gain, spec.passband_end / r2);
                // A Kaiser design ripples by the stopband's relative error
                // in the passband: 20 log10(1 + delta) ~ 8.69 delta dB.
                let bound = 8.69 * 10.0_f64.powf(-spec.attenuation_db / 20.0) * 1.25;
                assert!(
                    pass_db <= bound.max(1.0e-9),
                    "{spec:?} x{ratio} ({} taps): passband deviation {pass_db:.3e} dB > {bound:.3e}",
                    b.taps
                );
                // The shortest: one tap fewer must break the calibrated fit.
                let y = |taps: usize| {
                    let y = nyquist_stop_k(b.kaiser_beta)
                        * kaiser_half_width(taps, ratio, b.kaiser_beta);
                    y * (1.0 + NYQUIST_STOP_SHORT_GAIN * y)
                };
                // A wide band past 215 dB is designed for a narrower
                // transition (`BAND_WIDE_CAPPED_HALF_WIDTH`): longer.
                let narrowed = spec.passband_end < BAND_MIN_PASSBAND_END
                    && spec.attenuation_db + BAND_WIDE_PASSBAND_MARGIN_DB > 225.0;
                let pe = if narrowed {
                    spec.passband_end
                        .max((spec.stopband_start - 2.0 * BAND_WIDE_CAPPED_HALF_WIDTH).min(1.0))
                } else {
                    spec.passband_end
                };
                let w = 0.5 * (spec.stopband_start - pe);
                assert!(y(b.taps) <= w && (b.taps <= 2 || y(b.taps - 1) > w));
            }
        }
    }

    #[test]
    fn band_design_scales_with_the_ratio_and_rejects_bad_specs() {
        let spec = band(0.9, 1.0, 140.0);
        let t2 = band_design(&spec, 2).unwrap().taps as f64;
        let t16 = band_design(&spec, 16).unwrap().taps as f64;
        // Same duration at every ratio: 8x the taps for 8x the rate.
        assert!((t16 / t2 - 8.0).abs() < 0.05, "{t2} {t16}");
        assert!(band_design(&band(0.9, 0.9, 140.0), 4).is_err());
        // Under 0.5 only while the half transition stays calibrated (a
        // resolved passband end in Hz; the field bounds keep fractions at
        // 0.5 and up).
        assert!(band_design(&band(0.4, 1.5, 140.0), 4).is_err());
        assert!(band_design(&band(0.4, 0.9, 140.0), 4).is_ok());
        assert!(band_design(&band(1.01, 1.2, 140.0), 4).is_err());
        assert!(band_design(&band(0.9, 1.6, 140.0), 4).is_err());
        assert!(band_design(&band(0.9, 1.0, 40.0), 4).is_err());
        assert!(band_design(&band(0.9, 1.0, 230.0), 4).is_err());
        assert!(band_design(&band(f64::NAN, 1.0, 140.0), 4).is_err());
        let cfg = ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(4),
            cutoff_mode: CutoffMode::Band,
            band: spec,
            ..Default::default()
        };
        let d = UpsamplerDesign::try_from_config(&cfg, 4).unwrap();
        let b = band_design(&spec, 4).unwrap();
        assert_eq!(
            (d.taps, d.kaiser_beta, d.cutoff),
            (b.taps, b.kaiser_beta, b.cutoff)
        );
    }

    #[test]
    fn taps_per_input_sample_scale_with_the_ratio() {
        let cfg = ManualConfig {
            taps: 1000,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..Default::default()
        };
        assert_eq!(UpsamplerDesign::from_config(&cfg, 8).taps, 8000);
        assert_eq!(UpsamplerDesign::from_config(&cfg, 16).taps, 16_000);
        let out = ManualConfig {
            taps_unit: audivane_protocol::TapsUnit::Output,
            ..cfg
        };
        assert_eq!(UpsamplerDesign::from_config(&out, 16).taps, 1000);
    }

    #[test]
    fn scale_output_is_an_exact_gain() {
        let d = UpsamplerDesign::linear(4, 1024, 12.0, 1.0);
        let mut plain = Upsampler::new(&d, 1, 256);
        let mut quiet = Upsampler::new(&d, 1, 256);
        quiet.scale_output(0.5);
        let mut seed = 7u64;
        let x: Vec<f64> = (0..256).map(|_| lcg(&mut seed)).collect();
        let (mut a, mut b) = (vec![0.0; 1024], vec![0.0; 1024]);
        for _ in 0..8 {
            plain.process(&[&x], &mut [&mut a]);
            quiet.process(&[&x], &mut [&mut b]);
            for (p, q) in a.iter().zip(&b) {
                assert!((0.5 * p - q).abs() <= 1e-15 * p.abs().max(1.0));
            }
        }
    }

    // ---------------------------------------------------- intermediate phase

    /// Pre-ringing duration: samples from the first one above -60 dB of the
    /// impulse peak to the peak.
    fn pre_ringing(h: &[f64]) -> usize {
        let (peak, top) = h
            .iter()
            .enumerate()
            .map(|(i, v)| (i, v.abs()))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap();
        let first = h.iter().position(|v| v.abs() > top * 1.0e-3).unwrap();
        peak - first
    }

    fn assert_intermediate(ratio: usize, taps: usize, beta: f64, cutoff: f64) {
        let lin_d = UpsamplerDesign::linear(ratio, taps, beta, cutoff);
        let lin = design_kernel(&lin_d);
        let min = design_kernel(&UpsamplerDesign {
            phase: FilterPhase::Minimum,
            ..lin_d
        });
        let min_pre = pre_ringing(&min);
        let lin_pre = pre_ringing(&lin);
        let slack = (lin_pre / 10).max(3);
        let mut pres = Vec::new();
        for mix in [0.25, 0.5, 0.75, 0.9] {
            let d = UpsamplerDesign {
                phase: FilterPhase::Intermediate,
                phase_mix: mix,
                ..lin_d
            };
            let report =
                crate::minphase::intermediate_phase(&lin, &minimum_phase_contract(&d), mix)
                    .unwrap_or_else(|e| panic!("x{ratio} {taps} taps beta {beta} mix {mix}: {e}"));
            let check = report.check.expect("checked");
            assert!(check.passband_db <= crate::minphase::PASSBAND_TOLERANCE_DB);
            let h = design_kernel(&d);
            assert!(h.len() <= 4 * taps, "mix {mix}: {} taps", h.len());
            let sum: f64 = h.iter().sum();
            assert!((sum - ratio as f64).abs() < 1e-9 * ratio as f64);
            pres.push(pre_ringing(&h));
        }
        // Audible pre-ringing stays between the two twins and grows with the
        // mix (mostly over 0.75..1: below that it stays near minimum phase).
        let what = format!(
            "x{ratio} {taps} taps beta {beta}: pre-ringing {pres:?}, minimum {min_pre}, linear {lin_pre}"
        );
        assert!(
            pres.iter()
                .all(|&p| p + slack >= min_pre && p <= lin_pre + slack),
            "{what}"
        );
        assert!(pres.windows(2).all(|w| w[1] + slack >= w[0]), "{what}");
        assert!(pres[3] > pres[1], "{what}");
        // The ends are the two twins exactly.
        let at = |mix: f64| {
            design_kernel(&UpsamplerDesign {
                phase: FilterPhase::Intermediate,
                phase_mix: mix,
                ..lin_d
            })
        };
        assert_eq!(at(1.0), lin);
        assert_eq!(at(0.0), min);
    }

    #[test]
    fn intermediate_phase_short_kernels_by_roots() {
        for (ratio, taps, beta) in [
            (2, 63, 8.0),
            (4, 255, 12.0),
            (8, 1023, 14.0),
            (16, 512, 20.0),
        ] {
            assert_intermediate(ratio, taps, beta, 1.0);
        }
        assert_intermediate(4, 400, 10.0, 0.9);
    }

    #[test]
    fn intermediate_phase_long_kernels_by_cepstrum() {
        for (ratio, taps, beta) in [(2, 4096, 8.0), (8, 16_384, 14.0), (16, 16_383, 20.0)] {
            assert_intermediate(ratio, taps, beta, 1.0);
        }
        assert_intermediate(4, 8192, 5.0, 0.95);
    }

    #[test]
    fn intermediate_upsampler_reports_its_length_and_peak_delay() {
        let d = UpsamplerDesign {
            phase: FilterPhase::Intermediate,
            phase_mix: 0.5,
            ..UpsamplerDesign::linear(8, 4096, 12.0, 1.0)
        };
        let kernel = design_kernel(&d);
        let up = Upsampler::new(&d, 2, 256);
        assert_eq!(up.kernel_taps(), kernel.len());
        let peak = kernel
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        assert_eq!(
            up.latency_out_frames(),
            up.block_latency_out_frames() + peak
        );
        // Between minimum (0) and linear ((N - 1) / 2) phase.
        assert!(peak > 0 && peak < kernel.len() / 2, "peak {peak}");
    }

    /// Pre-ringing (samples above -60 and -100 dB before the peak), latency
    /// and length against the mix. A research printout:
    /// `cargo nextest run --release -E 'test(intermediate_phase_profile)' --run-ignored all --no-capture`.
    #[test]
    #[ignore = "research printout"]
    fn intermediate_phase_profile() {
        let pre_at = |h: &[f64], level: f64| {
            let (peak, top) = h
                .iter()
                .enumerate()
                .map(|(i, v)| (i, v.abs()))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .unwrap();
            peak - h.iter().position(|v| v.abs() > top * level).unwrap()
        };
        for (ratio, taps, beta) in [(2, 4096, 8.0), (8, 65_536, 14.0), (16, 65_536, 20.0)] {
            let lin_d = UpsamplerDesign::linear(ratio, taps, beta, 1.0);
            eprintln!("x{ratio} {taps} taps beta {beta}");
            for mix in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 1.0] {
                let h = design_kernel(&UpsamplerDesign {
                    phase: FilterPhase::Intermediate,
                    phase_mix: mix,
                    ..lin_d
                });
                let peak = h
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                    .unwrap()
                    .0;
                eprintln!(
                    "  mix {mix:4}: {:>7} taps ({:.2}x), peak at {peak:>6}, pre-ringing {:>6} (-60 dB) {:>6} (-100 dB)",
                    h.len(),
                    h.len() as f64 / taps as f64,
                    pre_at(&h, 1e-3),
                    pre_at(&h, 1e-5)
                );
            }
        }
    }

    /// The largest intermediate-phase design the daemon accepts.
    #[test]
    #[ignore = "heavy: 1M-tap intermediate design"]
    fn intermediate_phase_1m() {
        for (ratio, beta, mix) in [(8, 14.0, 0.75), (16, 20.0, 0.5), (2, 5.0, 0.9)] {
            let d = UpsamplerDesign {
                phase: FilterPhase::Intermediate,
                phase_mix: mix,
                ..UpsamplerDesign::linear(ratio, 1 << 20, beta, 1.0)
            };
            let t = std::time::Instant::now();
            let h = try_design_kernel(&d).unwrap_or_else(|e| panic!("x{ratio} beta {beta}: {e}"));
            eprintln!(
                "x{ratio} beta {beta} mix {mix}: {} taps ({:.2}x) in {:.1} s, peak RSS {:.0} MiB",
                h.len(),
                h.len() as f64 / (1 << 20) as f64,
                t.elapsed().as_secs_f64(),
                peak_rss_mib()
            );
        }
    }

    // ---------------------------------------------------- keep the samples

    #[test]
    fn preserve_samples_passes_the_input_through() {
        for (ratio, taps) in [(2usize, 4096usize), (8, 65_536), (16, 8191), (3, 1000)] {
            let d = UpsamplerDesign {
                preserve_samples: true,
                ..UpsamplerDesign::linear(ratio, taps, 14.0, 1.0)
            };
            let h = design_kernel(&d);
            assert_eq!(h.len() % 2, 1, "odd length");
            assert_eq!(h.len(), if taps % 2 == 0 { taps - 1 } else { taps });
            let c = (h.len() - 1) / 2;
            assert_eq!(h[c], 1.0);
            for k in (ratio..=c).step_by(ratio) {
                assert_eq!((h[c - k], h[c + k]), (0.0, 0.0));
            }
            // Through the real (FFT) upsampler, the input samples come back.
            let mut up = Upsampler::new(&d, 1, 512);
            let delay = up.latency_out_frames();
            let mut seed = 11u64;
            let n_in = 512 * 8 + delay / ratio + 2;
            let x: Vec<f64> = (0..n_in).map(|_| lcg(&mut seed)).collect();
            let mut y = vec![0.0; n_in * ratio];
            for (xi, yi) in x.chunks(512).zip(y.chunks_mut(512 * ratio)) {
                up.process(&[xi], &mut [yi]);
            }
            let mut worst = 0.0_f64;
            for (n, &xn) in x.iter().enumerate() {
                if let Some(&yn) = y.get(n * ratio + delay) {
                    worst = worst.max((yn - xn).abs());
                }
            }
            assert!(worst < 1e-12, "x{ratio} {taps} taps: worst {worst:e}");
        }
    }

    #[test]
    fn sample_leak_separates_nyquist_filters_from_the_rest() {
        let leak = |preserve: bool, cutoff: f64, phase: FilterPhase| {
            let cfg = ManualConfig {
                target: audivane_protocol::RateTarget::Ratio(8),
                taps: 16_384,
                phase,
                cutoff,
                preserve_samples: preserve,
                ..Default::default()
            };
            measure_upsampler(&cfg, 44_100)
                .unwrap()
                .response
                .sample_leak_db
        };
        assert!(leak(true, 1.0, FilterPhase::Linear) < -250.0);
        assert!(leak(false, 0.95, FilterPhase::Linear) > -80.0);
        assert!(leak(false, 1.0, FilterPhase::Minimum) > -80.0);
    }

    // ------------------------------------------------------ Gaussian window

    fn gaussian_design(ratio: usize, taps: usize, beta: f64, cutoff: f64) -> UpsamplerDesign {
        UpsamplerDesign {
            window: WindowKind::Gaussian,
            ..UpsamplerDesign::linear(ratio, taps, beta, cutoff)
        }
    }

    #[test]
    fn gaussian_band_designs_meet_both_edges() {
        let specs = [
            band(0.9, 1.0, 100.0),
            band(0.95, 1.0, 150.0),
            band(0.8, 1.2, 60.0),
            band(0.9, 1.05, 200.0),
            band(0.5, 0.7, 120.0),
            band(1.0, 1.5, 225.0),
        ];
        for spec in specs {
            for ratio in [2usize, 4, 8, 16] {
                let b = band_design_for(WindowKind::Gaussian, &spec, ratio).unwrap();
                let h = design_kernel(&gaussian_design(ratio, b.taps, b.kaiser_beta, b.cutoff));
                let gain = ratio as f64;
                let r2 = 2.0 * ratio as f64;
                let (_, stop_db) = dense_peak_above(&h, gain, spec.stopband_start / r2);
                assert!(
                    stop_db <= -spec.attenuation_db + 0.02,
                    "{spec:?} x{ratio} ({} taps): stopband peak {stop_db:.3} dB",
                    b.taps
                );
                let pass_db = passband_deviation_db(&h, gain, spec.passband_end / r2);
                let bound = 8.69 * 10.0_f64.powf(-spec.attenuation_db / 20.0) * 1.25;
                assert!(
                    pass_db <= bound.max(1.0e-9),
                    "{spec:?} x{ratio} ({} taps): passband {pass_db:.3e} dB > {bound:.3e}",
                    b.taps
                );
                let w = 0.5 * (spec.stopband_start - spec.passband_end);
                let ht = |t| half_transition(WindowKind::Gaussian, t, ratio, b.kaiser_beta);
                assert!(ht(b.taps) <= w && (b.taps <= 2 || ht(b.taps - 1) > w));
            }
        }
    }

    #[test]
    fn gaussian_nyquist_stop_holds() {
        for ratio in [2usize, 4, 8, 16, 32] {
            for beta in [5.0, 10.0, 14.0, 20.0, 24.0] {
                let min = nyquist_stop_min_taps_for(WindowKind::Gaussian, ratio, beta).unwrap();
                for taps in [min, 2 * min + 1, 8 * min] {
                    if taps > 300_000 {
                        continue;
                    }
                    let cutoff =
                        nyquist_stop_check_for(WindowKind::Gaussian, taps, ratio, beta).unwrap();
                    let h = design_kernel(&gaussian_design(ratio, taps, beta, cutoff));
                    let (_, peak) = dense_peak_above(&h, ratio as f64, 0.5 / ratio as f64);
                    let a = kaiser_attenuation_db(beta);
                    assert!(
                        peak <= -a + 0.02,
                        "x{ratio} beta {beta} {taps} taps (cutoff {cutoff:.4}): {peak:.3} dB vs -{a:.1}"
                    );
                }
                assert!(
                    nyquist_stop_check_for(WindowKind::Gaussian, min - 1, ratio, beta).is_err()
                );
            }
        }
    }

    #[test]
    fn gaussian_rings_shorter_but_wider_than_kaiser() {
        let (ratio, taps, beta) = (8usize, 16_383usize, 14.0);
        let kaiser_h = design_kernel(&UpsamplerDesign::linear(ratio, taps, beta, 1.0));
        let gauss_h = design_kernel(&gaussian_design(ratio, taps, beta, 1.0));
        // Ringing level three quarters of the way from the centre to the end.
        let at = |h: &[f64]| {
            let c = (h.len() - 1) / 2;
            let i = c + 3 * c / 4;
            let top = h[c].abs();
            lin_to_db(
                h[i - 64..i + 64]
                    .iter()
                    .fold(0.0_f64, |m, v| m.max(v.abs()))
                    / top,
            )
        };
        let (k, g) = (at(&kaiser_h), at(&gauss_h));
        assert!(g < k - 25.0, "Gaussian {g:.1} dB vs Kaiser {k:.1} dB");
        // The price: about twice the transition at the same length.
        let ratio_w = half_transition(WindowKind::Gaussian, taps, ratio, beta)
            / half_transition(WindowKind::Kaiser, taps, ratio, beta);
        assert!((1.6..2.4).contains(&ratio_w), "{ratio_w}");
    }

    #[test]
    fn tukey_keeps_the_sinc_over_its_flat_middle() {
        let (ratio, taps) = (8usize, 65_535usize);
        let d = UpsamplerDesign {
            window: WindowKind::Tukey,
            taper: 0.25,
            preserve_samples: true,
            ..UpsamplerDesign::linear(ratio, taps, 14.0, 1.0)
        };
        let h = design_kernel(&d);
        let c = (h.len() - 1) / 2;
        assert_eq!(h[c], 1.0);
        // The middle 75% of each half is the ideal sinc.
        let mut worst = 0.0_f64;
        for k in 1..(3 * c / 4) {
            let x = PI * k as f64 / ratio as f64;
            let ideal = x.sin() / x;
            worst = worst
                .max((h[c + k] - ideal).abs())
                .max((h[c - k] - ideal).abs());
        }
        // (Up to the sum-to-ratio normalisation, ~1e-11.)
        assert!(worst < 1e-9, "worst {worst:e}");
        // A Kaiser of the same length keeps the sinc nowhere near that far
        // out: its window is under 0.99 past ~4% of the half.
        let kaiser_h = design_kernel(&UpsamplerDesign {
            preserve_samples: true,
            ..UpsamplerDesign::linear(ratio, taps, 14.0, 1.0)
        });
        let k = c / 2;
        let x = PI * k as f64 / ratio as f64;
        assert!((kaiser_h[c + k] / (x.sin() / x)) < 0.5);

        // The stopband slopes down instead of sitting at a floor: about
        // -111 dB from 2% past the source Nyquist, -150 dB from 10%.
        let nfft = (h.len() * 16).next_power_of_two();
        let mag = magnitude_spectrum(&h, nfft);
        let peak_from = |edge: f64| {
            let from = (0.5 / ratio as f64 * edge * nfft as f64).ceil() as usize;
            lin_to_db(mag[from..].iter().fold(0.0_f64, |m, &v| m.max(v)) / ratio as f64)
        };
        let (near, far) = (peak_from(1.02), peak_from(1.1));
        assert!((-116.0..-106.0).contains(&near), "from 1.02: {near:.1} dB");
        assert!(far < -147.0, "from 1.1: {far:.1} dB");
        let pass = passband_deviation_db(&h, ratio as f64, 0.5 / ratio as f64 * 0.99);
        assert!(pass < 1e-3, "passband {pass:e} dB");
    }

    #[test]
    fn gaussian_minimum_and_intermediate_phase() {
        for (ratio, taps, beta) in [
            (4usize, 512usize, 10.0),
            (8, 16_384, 14.0),
            (16, 65_536, 20.0),
        ] {
            for (phase, mix) in [
                (FilterPhase::Minimum, 0.0),
                (FilterPhase::Intermediate, 0.75),
            ] {
                let d = UpsamplerDesign {
                    phase,
                    phase_mix: mix,
                    ..gaussian_design(ratio, taps, beta, 1.0)
                };
                let h = try_design_kernel(&d)
                    .unwrap_or_else(|e| panic!("x{ratio} {taps} beta {beta} {phase:?}: {e}"));
                let sum: f64 = h.iter().sum();
                assert!((sum - ratio as f64).abs() < 1e-9 * ratio as f64);
            }
        }
    }

    #[test]
    fn measure_upsampler_preset_ratio_one() {
        let off = ManualConfig {
            target: audivane_protocol::RateTarget::Off,
            preset: Some(FilterPreset::GaussianShort),
            ..Default::default()
        };
        let flat = measure_upsampler(&off, 44_100).expect("ratio 1 preset");
        assert_eq!(flat.response.ratio, 1);
        assert_eq!(flat.response.preset, Some(FilterPreset::GaussianShort));
        assert_eq!(flat.response.impulse, vec![1.0]);
    }

    /// The response is measured at the source rate asked for, which need
    /// not be one validation saw: the design is prepared at that ratio
    /// first, so its limits hold (here 8 kHz resolves to x64, a kernel one
    /// branch over the 8 Mi-tap limit, refused before it is designed), and
    /// what builds is the engine's kernel.
    #[test]
    fn measure_upsampler_prepares_the_ratio_it_measures() {
        let long = ManualConfig {
            target: audivane_protocol::RateTarget::MaxRate(768_000),
            taps: 131_073,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..ManualConfig::default()
        };
        let e = measure_upsampler(&long, 8_000).err().unwrap();
        assert!(e.contains("8388672") && e.contains("8388608"), "{e}");
        // Rational: the budget at the ratio measured.
        let rational = ManualConfig {
            target: audivane_protocol::RateTarget::Rate(705_600),
            taps: 8_192,
            taps_unit: audivane_protocol::TapsUnit::Input,
            ..ManualConfig::default()
        };
        let e = measure_upsampler(&rational, 8_000).err().unwrap();
        assert!(
            e.contains("use a shorter filter or a lower output rate"),
            "{e}"
        );
        // A design that builds measures the kernel the engine prepares.
        let cfg = ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(4),
            taps: 4_096,
            phase: FilterPhase::Intermediate,
            phase_mix: 0.5,
            ..ManualConfig::default()
        };
        let m = measure_upsampler(&cfg, 44_100).unwrap();
        let d = UpsamplerDesign::try_from_config(&cfg, 4).unwrap();
        let k = try_design_kernel(&d).unwrap();
        assert_eq!(m.response.taps, k.len());
        assert_eq!(
            m.response.latency_samples,
            Upsampler::try_new(&d, 1, DEFAULT_MEASURE_MAX_BLOCK)
                .unwrap()
                .latency_out_frames()
        );
    }

    /// The measured latency is what the engine's own builder delays at the
    /// same block (what `status.filter_latency_ms` shows), not the offline
    /// partition block: a kernel preset, a block FFT, an IIR cascade and the
    /// standard path, at x16 from 44.1 kHz with the 512-frame block of the
    /// default quantum, and at a larger block.
    #[test]
    fn measured_latency_is_the_live_engines() {
        let preset = |p| ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(16),
            preset: Some(p),
            ..Default::default()
        };
        let cfgs = [
            preset(FilterPreset::KaiserSteep),
            preset(FilterPreset::BlockFft),
            preset(FilterPreset::CascadeElliptic),
            response_config(16, 3393, FilterPhase::Linear),
        ];
        for max_block in [512, 2048] {
            for cfg in &cfgs {
                let up = cfg.clone().into_upsampler_config().unwrap();
                let named = crate::design::Named::of(&up).unwrap();
                let cx = crate::design::BuildContext {
                    source_rate: 44_100,
                    ratio: RateRatio::integer(16),
                    channels: 1,
                    max_block,
                    pipelines: RATIONAL_SWAP_PIPELINES,
                    output: audivane_protocol::OutputConfig::default(),
                    workspace: crate::design::budget::design_workspace(),
                };
                let live = crate::design::upsampler::upsampler(&named, &cx, 0.0)
                    .unwrap()
                    .unwrap()
                    .latency_out_frames();
                let m = measure_upsampler_at(cfg, 44_100, max_block).unwrap();
                assert_eq!(
                    m.response.latency_samples, live,
                    "{:?} at block {max_block}",
                    cfg.preset
                );
            }
        }
        // kaiser_steep: its kernel's centre plus 512 x 16, 18.15 ms.
        let m = measure_upsampler_at(&cfgs[0], 44_100, 512).unwrap();
        assert_eq!(m.response.latency_samples, 4616 + 8192);
    }

    #[test]
    fn measure_upsampler_fir_preset() {
        let cfg = ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(4),
            preset: Some(FilterPreset::GaussianShort),
            ..Default::default()
        };
        let measured = measure_upsampler(&cfg, 44_100).expect("measure preset");
        assert_eq!(measured.response.preset, Some(FilterPreset::GaussianShort));
        assert_eq!(measured.response.ratio, 4);
        assert_eq!(measured.response.phase, FilterPhase::Linear);
        assert!(measured.response.taps > 0);
        assert!(measured.response.stopband_atten_db > 40.0);
    }

    #[test]
    fn measure_block_fft_preset_reports_its_edges() {
        // Its stopband stays far above the fixed preset target, which must
        // not hide the -0.1 and -3 dB points.
        let cfg = ManualConfig {
            target: audivane_protocol::RateTarget::Ratio(4),
            preset: Some(FilterPreset::BlockFft),
            ..Default::default()
        };
        let r = measure_upsampler(&cfg, 44_100)
            .expect("measure preset")
            .response;
        assert!(
            (20_000.0..22_050.0).contains(&r.minus_0_1_db_hz),
            "-0.1 dB at {}",
            r.minus_0_1_db_hz
        );
        assert!(
            (22_000.0..22_200.0).contains(&r.minus_3_db_hz),
            "-3 dB at {}",
            r.minus_3_db_hz
        );
    }

    use audivane_protocol::RateTarget;

    fn rational_cfg(target: RateTarget, taps: usize, mode: CutoffMode) -> ManualConfig {
        ManualConfig {
            target,
            taps,
            taps_unit: audivane_protocol::TapsUnit::Input,
            kaiser_beta: 14.0,
            cutoff: 0.95,
            cutoff_mode: mode,
            ..ManualConfig::default()
        }
    }

    #[test]
    fn rational_budget_caps_kernel_bytes_and_counts_pipelines() {
        use crate::design::budget::{MAX_RATIONAL_GRID_TAPS, MAX_RATIONAL_KERNEL_BYTES};
        let r = RateRatio::from_rates(44_100, 768_000);
        let check = |n, cap, pipelines| {
            crate::design::budget::check_rational(n, r, 44_100, cap, pipelines)
                .map_err(|e| e.to_string())
        };
        let cap = MAX_RATIONAL_GRID_TAPS;
        assert_eq!(cap * 8, MAX_RATIONAL_KERNEL_BYTES);
        // 260 taps per branch at 768 kHz: 200 M per pipeline.
        let n = 2560 * 260;
        check(n, cap, 2).unwrap();
        check(n, cap, 4).unwrap();
        // 409 taps per branch: 314 M; a swap is fine, A/B is not.
        let n = 2560 * 409;
        check(n, cap, 2).unwrap();
        let e = check(n, cap, 4).unwrap_err();
        assert!(e.contains("4 pipelines"), "{e}");
        // The 21 MB kernel is over the byte cap whatever its cost.
        let e = check(2560 * 1041, usize::MAX, 1).unwrap_err();
        assert!(e.contains("MB"), "{e}");
    }

    #[test]
    fn rational_designs_put_the_band_edge_at_the_lower_nyquist() {
        // Upsampling 44.1k -> 48k: the edge is the source Nyquist.
        let up = UpsamplerDesign::try_for_ratio(
            &rational_cfg(RateTarget::Rate(48_000), 100, CutoffMode::Fraction),
            RateRatio::new(160, 147),
        )
        .unwrap();
        assert_eq!((up.ratio, up.taps), (160, 16_000));
        // Decimation 96k -> 48k: the same shape as x2 from 48k, `taps`
        // input frames long.
        let cfg = rational_cfg(RateTarget::Rate(48_000), 100, CutoffMode::NyquistStop);
        let down = UpsamplerDesign::try_for_ratio(&cfg, RateRatio::new(1, 2)).unwrap();
        let x2 = UpsamplerDesign::try_from_config(
            &ManualConfig {
                taps: 100,
                taps_unit: audivane_protocol::TapsUnit::Output,
                ..cfg.clone()
            },
            2,
        )
        .unwrap();
        assert_eq!((down.ratio, down.taps), (2, 100));
        assert_eq!(down.cutoff, x2.cutoff);
        // The decimation kernel is the x2 kernel at half the gain (one
        // branch).
        let k = try_design_kernel_for(&down, RateRatio::new(1, 2)).unwrap();
        let k2 = try_design_kernel(&x2).unwrap();
        for (a, b) in k.iter().zip(&k2) {
            assert!((a - 0.5 * b).abs() < 1e-15);
        }
        assert!((k.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        // Integer ratios are untouched.
        let cfg = rational_cfg(RateTarget::Ratio(4), 100, CutoffMode::Fraction);
        assert_eq!(
            UpsamplerDesign::try_for_ratio(&cfg, RateRatio::integer(4)).unwrap(),
            UpsamplerDesign::try_from_config(&cfg, 4).unwrap()
        );
    }

    #[test]
    fn rational_response_is_measured_on_the_grid() {
        let cfg = rational_cfg(RateTarget::Rate(48_000), 512, CutoffMode::NyquistStop);
        let m = measure_upsampler(&cfg, 44_100).unwrap();
        let r = &m.response;
        assert_eq!((r.source_rate, r.output_rate, r.ratio), (44_100, 48_000, 0));
        assert_eq!(r.rate_ratio, RateRatio::new(160, 147));
        assert_eq!(m.impulse_rate(), 44_100 * 160);
        assert!(r.passband_ripple_db < 1e-3, "{}", r.passband_ripple_db);
        assert!(
            r.stopband_start_hz <= 22_050.0 * 1.001,
            "{}",
            r.stopband_start_hz
        );
        assert!(r.stopband_atten_db > 130.0, "{}", r.stopband_atten_db);
        // Linear phase: half of 512 input frames, in output frames.
        let want = (160.0 * 512.0 - 1.0) / 2.0 / 147.0;
        assert!((r.latency_samples as f64 - want).abs() <= 0.5);

        // The protected-band error is measured on the grid too (it read 0,
        // a perfect filter, when it divided by the integer ratio field).
        let err = m.protected_band_error(0.9, 1.0);
        let h = try_design_kernel_for(
            &UpsamplerDesign::try_for_ratio(&cfg, RateRatio::new(160, 147)).unwrap(),
            RateRatio::new(160, 147),
        )
        .unwrap();
        let direct = crate::design::auto::protected_error(&h, 160.0, 0.9 / 320.0, 1.0 / 320.0);
        assert!(err > 0.0 && err.is_finite(), "{err}");
        assert!((err / direct - 1.0).abs() < 1e-6, "{err} vs {direct}");

        let cfg = rational_cfg(RateTarget::Rate(48_000), 128, CutoffMode::NyquistStop);
        let m = measure_upsampler(&cfg, 96_000).unwrap();
        let r = &m.response;
        assert_eq!(r.rate_ratio, RateRatio::new(1, 2));
        let err = m.protected_band_error(0.9, 1.0);
        assert!(err > 0.0 && err.is_finite(), "{err}");
        assert!(
            r.stopband_start_hz <= 24_000.0 * 1.001,
            "{}",
            r.stopband_start_hz
        );
        assert!(
            r.minus_3_db_hz < 24_000.0 && r.minus_3_db_hz > 19_000.0,
            "{}",
            r.minus_3_db_hz
        );
        assert!(r.stopband_atten_db > 130.0);
    }
}
