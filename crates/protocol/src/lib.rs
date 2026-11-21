//! Types shared by the engine, the CLI and (later) the UI.
//!
//! Everything here is plain data with serde derives. JSON field names are
//! snake_case and enums are tagged with lowercase names, so the UI can
//! produce them by hand.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod design;
pub mod ipc;
pub mod ratio;

pub use design::{FilterDesign, RECOMMENDED_PRESET, StoredDesign};
pub use ratio::{MAX_RATIO_TERM, RateRatio};

/// Graph (source) rates the engine can run at. `rate.allowed_rates` must be
/// a subset of this list.
pub const ALLOWED_RATES: &[u32] = &[
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// DAC rates above the graph rates: x32 and x64 of 44.1 and 48 kHz. The
/// engine reaches them only by upsampling (integer ratios up to
/// [`MAX_INTEGER_RATIO`], or a rational ratio within the term limit).
pub const HIGH_OUTPUT_RATES: &[u32] = &[1_411_200, 1_536_000, 2_822_400, 3_072_000];

/// Highest DAC rate the engine ever asks ALSA for.
pub const MAX_OUTPUT_RATE: u32 = 3_072_000;

/// Largest integer upsampling ratio (44.1 kHz x64 = 2.8224 MHz).
pub const MAX_INTEGER_RATIO: u32 = 64;

/// Whether `rate` may be named as a fixed output rate
/// ([`RateTarget::Rate`]): one of [`ALLOWED_RATES`] or
/// [`HIGH_OUTPUT_RATES`].
pub fn is_output_rate(rate: u32) -> bool {
    ALLOWED_RATES.contains(&rate) || HIGH_OUTPUT_RATES.contains(&rate)
}

// ---------------------------------------------------------------- EQ

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FilterType {
    /// Peaking EQ (RBJ "peakingEQ"), uses `gain_db` and `q`.
    Peaking,
    /// Low shelf (RBJ), `q` is the shelf Q (0.7071 = S of 1).
    LowShelf,
    /// High shelf (RBJ), `q` is the shelf Q.
    HighShelf,
    /// 2nd-order low pass, `q` (0.7071 = Butterworth).
    LowPass,
    /// 2nd-order high pass, `q`.
    HighPass,
    /// Band pass, constant 0 dB peak gain.
    BandPass,
    Notch,
    AllPass,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Band {
    pub kind: FilterType,
    pub freq_hz: f64,
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default = "default_q")]
    pub q: f64,
    #[serde(default = "yes")]
    pub enabled: bool,
}

pub const EQ_PROFILE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ChannelEq {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub preamp_db: f64,
    #[serde(default)]
    pub bands: Vec<Band>,
}

impl Default for ChannelEq {
    fn default() -> Self {
        Self {
            enabled: true,
            preamp_db: 0.0,
            bands: Vec::new(),
        }
    }
}

impl ChannelEq {
    pub fn is_active(&self) -> bool {
        self.enabled && (self.preamp_db != 0.0 || self.bands.iter().any(|b| b.enabled))
    }
}

/// Where the 0 dB pivot sits on a tilt EQ.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum TiltPivot {
    /// Anchor the flat bass below `start_hz` (exactly 0 dB at DC).
    Start,
    /// Anchor the flat treble above `end_hz` (exactly 0 dB at Nyquist).
    End,
    /// Anchor 0 dB at the geometric mean `sqrt(start_hz * end_hz)`.
    #[default]
    Center,
    /// Anchor 0 dB at an explicit frequency in Hz.
    Hz(f64),
}

/// Lowest tilt `start_hz` and highest `end_hz` (the audible band). The end
/// is also clamped to `0.45 * fs` at build time.
pub const TILT_MIN_HZ: f64 = 20.0;
pub const TILT_MAX_HZ: f64 = 20_000.0;

fn default_tilt_start_hz() -> f64 {
    TILT_MIN_HZ
}

fn default_tilt_end_hz() -> f64 {
    TILT_MAX_HZ
}

/// Constant-slope (dB/octave) tilt filter: 0 dB below `start_hz`, the slope
/// up to `end_hz`, flat above (sharp corners), shifted so the pivot is 0 dB.
/// `total_db` is the change from `start_hz` to `end_hz`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TiltConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slope_db_per_oct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_db: Option<f64>,
    #[serde(default = "default_tilt_start_hz")]
    pub start_hz: f64,
    #[serde(default = "default_tilt_end_hz")]
    pub end_hz: f64,
    #[serde(default)]
    pub pivot: TiltPivot,
}

impl Default for TiltConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slope_db_per_oct: Some(0.0),
            total_db: None,
            start_hz: default_tilt_start_hz(),
            end_hz: default_tilt_end_hz(),
            pivot: TiltPivot::Center,
        }
    }
}

impl TiltConfig {
    /// Effective slope in dB/oct when exactly one of `slope_db_per_oct` or
    /// `total_db` is set and the band bounds are finite with `start_hz < end_hz`.
    pub fn effective_slope_db_per_oct(&self) -> Option<f64> {
        if !self.start_hz.is_finite()
            || !self.end_hz.is_finite()
            || self.start_hz <= 0.0
            || self.end_hz <= self.start_hz
        {
            return None;
        }
        match (self.slope_db_per_oct, self.total_db) {
            (Some(s), None) if s.is_finite() => Some(s),
            (None, Some(t)) if t.is_finite() => {
                let octaves = (self.end_hz / self.start_hz).log2();
                Some(t / octaves)
            }
            _ => None,
        }
    }

    /// Effective total dB change across `[start_hz, end_hz]`.
    pub fn effective_total_db(&self) -> Option<f64> {
        if !self.start_hz.is_finite()
            || !self.end_hz.is_finite()
            || self.start_hz <= 0.0
            || self.end_hz <= self.start_hz
        {
            return None;
        }
        match (self.slope_db_per_oct, self.total_db) {
            (Some(s), None) if s.is_finite() => {
                let octaves = (self.end_hz / self.start_hz).log2();
                Some(s * octaves)
            }
            (None, Some(t)) if t.is_finite() => Some(t),
            _ => None,
        }
    }

    /// True if the tilt is enabled and has a non-zero slope.
    pub fn is_active(&self) -> bool {
        self.enabled && self.effective_slope_db_per_oct().is_some_and(|s| s != 0.0)
    }
}

/// Rules for the two tilt sections together: a pair must pass
/// [`check_tilt_pair`], and `tilt_high` alone is an error (a treble tilt
/// alone is a single `tilt` with pivot `start`).
pub fn check_tilt_sections(
    low: Option<&TiltConfig>,
    high: Option<&TiltConfig>,
) -> Result<(), String> {
    match (low, high) {
        (Some(low), Some(high)) => check_tilt_pair(low, high),
        (None, Some(_)) => Err(
            "tilt_high needs tilt (the bass section); a treble tilt alone is tilt with pivot \"start\""
                .into(),
        ),
        _ => Ok(()),
    }
}

/// A two-section tilt: the bass band ends at or below the treble band's
/// start, the bass is pivoted at its end and the treble at its start, so the
/// response is exactly 0 dB between the two.
pub fn check_tilt_pair(low: &TiltConfig, high: &TiltConfig) -> Result<(), String> {
    if low.end_hz.is_nan() || high.start_hz.is_nan() || low.end_hz > high.start_hz {
        return Err(format!(
            "tilt_high.start_hz ({}) must be at or above tilt.end_hz ({}): the two tilt bands must not overlap",
            high.start_hz, low.end_hz
        ));
    }
    if low.pivot != TiltPivot::End || high.pivot != TiltPivot::Start {
        return Err(
            "with tilt_high set, tilt.pivot must be \"end\" and tilt_high.pivot \"start\" (0 dB between the two bands)"
                .into(),
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct RawEqProfile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    preamp_db: f64,
    #[serde(default = "yes")]
    linked_enabled: bool,
    #[serde(default)]
    bands: Vec<Band>,
    #[serde(default)]
    left: Option<ChannelEq>,
    #[serde(default)]
    right: Option<ChannelEq>,
    #[serde(default)]
    tilt: Option<TiltConfig>,
    #[serde(default)]
    tilt_high: Option<TiltConfig>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(from = "RawEqProfile")]
pub struct EqProfile {
    #[serde(default)]
    pub name: String,
    /// 2 = layered (this layout). Absent/0/1 = legacy (A), migrated on read.
    pub version: u32,
    #[serde(default)]
    pub preamp_db: f64,
    #[serde(default = "yes")]
    pub linked_enabled: bool,
    #[serde(default)]
    pub bands: Vec<Band>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<ChannelEq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<ChannelEq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tilt: Option<TiltConfig>,
    /// Optional treble section of a two-section tilt ("both"): `tilt` is the
    /// bass section, pivoted at its end, and this one starts at or above
    /// `tilt.end_hz`, pivoted at its start, so the response is exactly 0 dB
    /// between them. The two run in cascade. See [`check_tilt_pair`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tilt_high: Option<TiltConfig>,
}

impl Default for EqProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: EQ_PROFILE_VERSION,
            preamp_db: 0.0,
            linked_enabled: true,
            bands: Vec::new(),
            left: None,
            right: None,
            tilt: None,
            tilt_high: None,
        }
    }
}

impl From<RawEqProfile> for EqProfile {
    fn from(raw: RawEqProfile) -> Self {
        if raw.version < EQ_PROFILE_VERSION {
            match raw.right {
                None => EqProfile {
                    name: raw.name,
                    version: EQ_PROFILE_VERSION,
                    preamp_db: raw.preamp_db,
                    linked_enabled: raw.linked_enabled,
                    bands: raw.bands,
                    left: raw.left,
                    right: None,
                    tilt: raw.tilt,
                    tilt_high: raw.tilt_high,
                },
                Some(right) => EqProfile {
                    name: raw.name,
                    version: EQ_PROFILE_VERSION,
                    preamp_db: 0.0,
                    linked_enabled: true,
                    bands: Vec::new(),
                    left: Some(ChannelEq {
                        enabled: true,
                        preamp_db: raw.preamp_db,
                        bands: raw.bands,
                    }),
                    right: Some(right),
                    tilt: raw.tilt,
                    tilt_high: raw.tilt_high,
                },
            }
        } else {
            EqProfile {
                name: raw.name,
                version: raw.version,
                preamp_db: raw.preamp_db,
                linked_enabled: raw.linked_enabled,
                bands: raw.bands,
                left: raw.left,
                right: raw.right,
                tilt: raw.tilt,
                tilt_high: raw.tilt_high,
            }
        }
    }
}

impl Serialize for EqProfile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a> {
            name: &'a str,
            version: u32,
            preamp_db: f64,
            linked_enabled: bool,
            bands: &'a [Band],
            #[serde(skip_serializing_if = "Option::is_none")]
            left: Option<&'a ChannelEq>,
            #[serde(skip_serializing_if = "Option::is_none")]
            right: Option<&'a ChannelEq>,
            #[serde(skip_serializing_if = "Option::is_none")]
            tilt: Option<&'a TiltConfig>,
            #[serde(skip_serializing_if = "Option::is_none")]
            tilt_high: Option<&'a TiltConfig>,
        }
        Out {
            name: &self.name,
            version: EQ_PROFILE_VERSION,
            preamp_db: self.preamp_db,
            linked_enabled: self.linked_enabled,
            bands: &self.bands,
            left: self.left.as_ref(),
            right: self.right.as_ref(),
            tilt: self.tilt.as_ref(),
            tilt_high: self.tilt_high.as_ref(),
        }
        .serialize(serializer)
    }
}

impl EqProfile {
    pub fn is_active(&self) -> bool {
        self.preamp_db != 0.0
            || (self.linked_enabled && self.bands.iter().any(|b| b.enabled))
            || self.left.as_ref().is_some_and(ChannelEq::is_active)
            || self.right.as_ref().is_some_and(ChannelEq::is_active)
            || self.tilt.as_ref().is_some_and(TiltConfig::is_active)
            || self.tilt_high.as_ref().is_some_and(TiltConfig::is_active)
    }
}

// ---------------------------------------------------------------- Convolution

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConvolutionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// WAV impulse response: mono (applied to every channel) or one channel
    /// per output channel. Resampled offline when its rate differs.
    pub ir_path: PathBuf,
    #[serde(default)]
    pub gain_db: f64,
}

// ---------------------------------------------------------------- Upsampling

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FilterPhase {
    #[default]
    Linear,
    /// Same magnitude as the linear-phase design, zeros reflected inside the
    /// unit circle. Needs Kaiser beta 2..=20.
    Minimum,
    /// Between the two: same magnitude, phase blended by
    /// [`design::Phase::Intermediate`]'s `mix` (0 = minimum, 1 = linear), so
    /// the pre-ringing is shorter than linear phase and the delay smaller.
    /// Needs Kaiser beta 2..=20 like minimum phase.
    Intermediate,
}

/// Window of the upsampler's windowed-sinc kernel.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    /// Kaiser: the narrowest transition for a given length and stopband.
    #[default]
    Kaiser,
    /// Gaussian: an erf-shaped transition about twice as wide as Kaiser's
    /// at the same length and stopband, with no sidelobes of its own, and an
    /// impulse response whose ringing dies away like a Gaussian (much
    /// faster than Kaiser's). Its stopband is the attenuation `kaiser_beta`
    /// stands for (`A = beta / 0.1102 + 8.7` dB), or the band attenuation.
    Gaussian,
    /// Tukey: flat (the plain sinc) over the middle of the kernel, a cosine
    /// taper over the outer `taper` fraction of each half ([`design::Window`]).
    /// Most coefficients stay equal to the ideal sinc and the roll-off
    /// is steep, but the stopband slopes down instead of sitting at a floor
    /// (a larger taper trades sinc fidelity for a deeper stopband). Pairs
    /// naturally with `preserve_samples`. Linear phase and `cutoff_mode`
    /// fraction only: it has no calibrated stopband formula, so the measured
    /// response reports it. `kaiser_beta` only sets the level the response's
    /// stopband edge is reported against.
    Tukey,
}

/// What a kernel length counts ([`design::Length`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum TapsUnit {
    /// Taps at the output rate: the same count lasts half as long at x16 as
    /// at x8.
    #[default]
    Output,
    /// Taps per input sample: the kernel has `taps * ratio` taps at the
    /// output rate, so it lasts the same time at every ratio.
    Input,
}

/// Measured passband amplitude error and image level for automatic taps.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FilterErrorBudget {
    /// Positive attenuation in dB (55..=200).
    Db(f64),
    /// Match the configured quantizer and dither (see `filter_error_budget_db`).
    MatchOutput,
}

/// Word length past which `match_output` stops raising its target: no DAC
/// resolves more than about 24 bits, so a 32-bit output gets the 24-bit
/// budget (146.24 dB without dither, 141.47 dB with TPDF or shaped dither).
/// An explicit [`FilterErrorBudget::Db`] may still ask for more.
pub const MATCH_OUTPUT_MAX_BITS: u32 = 24;

/// Variance of the dither noise added before rounding, in LSB², without
/// any error feedback (the unshaped reference).
pub fn dither_variance_lsb2(kind: DitherKind) -> f64 {
    match kind {
        DitherKind::None => 0.0,
        // ±1 LSB triangular: two uniforms, 2 · 1/12.
        DitherKind::Tpdf
        | DitherKind::Shaped
        | DitherKind::HeadroomTpdf
        | DitherKind::Feedback5
        | DitherKind::Resonant5
        | DitherKind::Resonant9
        | DitherKind::Integrator15 => 1.0 / 6.0,
        // Half-amplitude TPDF.
        DitherKind::Feedback1 => 1.0 / 24.0,
        // Uniform on [-1, 1).
        DitherKind::Rectangular => 1.0 / 3.0,
        // σ = 1/3 LSB truncated at ±3σ: σ² (1 − 6 φ(3) / (2 Φ(3) − 1)).
        DitherKind::Gaussian => 0.108_149,
        // TPDF through the sixth-order high-pass (measured total variance).
        DitherKind::HighpassTpdf => 0.0457,
    }
}

/// Full-scale sine / integrated quantizer noise: 1/12 LSB² of rounding plus
/// the dither's own variance ([`dither_variance_lsb2`]), capped at
/// [`MATCH_OUTPUT_MAX_BITS`]. This is a nominal broadband reference:
/// noise-shaped modes use their unshaped reference, which can be looser
/// than their audible-band floor, so ask for an explicit
/// [`FilterErrorBudget::Db`] to match one.
pub fn filter_error_budget_db(output: &OutputConfig) -> f64 {
    attenuation_for_bits(output.bits.min(MATCH_OUTPUT_MAX_BITS))
        - 10.0 * (1.0 + 12.0 * dither_variance_lsb2(output.dither)).log10()
}

/// A filter specified by its band edges and stopband attenuation, for
/// [`design::Cutoff::Band`]. The engine picks the Kaiser beta from the
/// attenuation and the shortest kernel whose transition fits between the
/// edges; other length and cutoff fields are then ignored.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BandSpec {
    /// End of the passband, as a fraction of the source Nyquist. Up to here
    /// the response stays within the stopband's relative error of 0 dB
    /// (a Kaiser design ripples by the same amount in both bands).
    #[serde(default = "default_passband_end")]
    pub passband_end: f64,
    /// Start of the stopband, as a fraction of the source Nyquist. 1.0 puts
    /// it at the source Nyquist (no imaging); above 1.0 lets images of the
    /// top of the band through, for a wider transition and shorter ringing.
    #[serde(default = "default_stopband_start")]
    pub stopband_start: f64,
    /// Stopband attenuation in dB (55..=225; minimum and intermediate phase
    /// up to 190). [`attenuation_for_bits`] gives the level that matches an
    /// output word length.
    #[serde(default = "default_attenuation_db")]
    pub attenuation_db: f64,
}

impl Default for BandSpec {
    fn default() -> Self {
        Self {
            passband_end: default_passband_end(),
            stopband_start: default_stopband_start(),
            attenuation_db: default_attenuation_db(),
        }
    }
}

/// Stopband attenuation that puts images at the dynamic range of an ideal
/// `bits`-bit quantizer (`6.02 bits + 1.76` dB): 98 dB at 16 bits, 146 dB at
/// 24, 194 dB at 32.
pub fn attenuation_for_bits(bits: u32) -> f64 {
    6.02 * f64::from(bits) + 1.76
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum RateTarget {
    /// Integer ratio of the source rate (1..=[`MAX_INTEGER_RATIO`]).
    Ratio(u32),
    /// Highest power-of-two multiple of the source rate that is <= this
    /// rate (e.g. 768000 → 44.1k·16 = 705600, 48k·16 = 768000). Stays in the
    /// source's rate family.
    MaxRate(u32),
    /// Resample to exactly this output rate, whatever the source rate:
    /// integer upsampling when it is a multiple of the source, otherwise a
    /// rational ratio (44.1k → 48k is 160/147), decimation included
    /// (192k → 96k). One of [`ALLOWED_RATES`] or [`HIGH_OUTPUT_RATES`].
    Rate(u32),
    /// Highest rate the device plays that is <= this rate, in either rate
    /// family: like [`Self::MaxRate`] when the device has a power-of-two
    /// multiple of the source, otherwise a rational ratio to its highest
    /// rate (a 48k-only device with a 44.1k source: 160/147). Without a
    /// device probe it resolves like [`Self::MaxRate`].
    MaxRateAny(u32),
    /// No upsampling.
    #[default]
    Off,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CutoffMode {
    /// Use the design's cutoff as a fraction of the source Nyquist.
    #[default]
    Fraction,
    /// Place the stopband edge at the source Nyquist: every frequency from
    /// the source to the output Nyquist stays at or below the Kaiser
    /// stopband target. Needs Kaiser beta 5..=24 and enough taps for the
    /// ratio (the daemon rejects other designs); see
    /// `audivane_dsp_core::resample::nyquist_stop_cutoff`.
    NyquistStop,
    /// Design from a [`BandSpec`]: passband end, stopband start and
    /// attenuation. The engine chooses taps, beta and cutoff.
    Band,
}

/// A named upsampling filter with a fixed recipe. A set
/// [`UpsamplerConfig::preset`] decides the whole design (kernel or runtime,
/// length, cutoff, window and phase); the manual design fields are then
/// ignored and only `target` still applies.
///
/// Lengths are per input sample unless stated, cutoffs fractions of the source
/// Nyquist. `_ip` names are intermediate phase, `_mp` minimum phase; the rest
/// linear unless stated.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FilterPreset {
    // Gaussian-windowed sinc: ringing that dies away fast.
    GaussianShort,
    GaussianMedium,
    GaussianLong,
    GaussianXlong,
    GaussianXlongApod,
    GaussianEarly,
    GaussianEarlyIp,
    GaussianEarlyMp,
    GaussianNyquist,
    GaussianNyquistShort,
    GaussianCompact,
    GaussianHuge,
    GaussianHugeApod,
    // Flat-top (HFT) and Nuttall windows: very deep stopbands.
    Nuttall,
    Flattop,
    FlattopShort,
    FlattopMedium,
    FlattopLong,
    FlattopXlong,
    FlattopXlongApod,
    FlattopXlongStd,
    FlattopEarly,
    FlattopEarlyIp,
    FlattopEarlyMp,
    FlattopCompact,
    FlattopHuge,
    FlattopMega,
    // Dolph-Chebyshev windows.
    Chebyshev,
    ChebyshevMp,
    ChebyshevShort,
    ChebyshevShortMp,
    ChebyshevLong,
    ChebyshevLongIp,
    ChebyshevLongMp,
    // Kaiser, steep transition.
    KaiserSteep,
    KaiserSteepMp,
    KaiserSteepShort,
    KaiserSteepShortMp,
    KaiserCompact,
    // Nyquist (half-band type) filters: keep the input samples.
    HalfbandKaiser,
    HalfbandLarge,
    HalfbandMedium,
    HalfbandSmall,
    HalfbandTiny,
    HannNyquist,
    // Lanczos kernels, named by their lobe count `a`.
    Lanczos23,
    Lanczos23Mp,
    Lanczos511,
    Lanczos2047,
    Lanczos8191,
    Lanczos32767,
    Lanczos65535,
    // Short Nuttall design with an early, leaky (19 kHz at 44.1k) roll-off:
    // little ringing, some images let through.
    ShortLeaky,
    ShortLeakyMp,
    // Cascades of x2/x3 stages (even ratios only).
    CascadeElliptic,
    CascadeInverseChebyshev,
    CascadeFir,
    CascadeFirAsym,
    CascadeFirMp,
    CascadeFft,
    // Short interpolators.
    Cosine,
    CatmullRom,
    TableSinc,
    // Block FFT interpolation (power-of-two ratios only, not time-invariant).
    BlockFft,
    BlockFftFast,
    BlockFftLarge,
}

impl FilterPreset {
    /// Every preset, in menu order.
    pub const ALL: &'static [FilterPreset] = &[
        Self::GaussianShort,
        Self::GaussianMedium,
        Self::GaussianLong,
        Self::GaussianXlong,
        Self::GaussianXlongApod,
        Self::GaussianEarly,
        Self::GaussianEarlyIp,
        Self::GaussianEarlyMp,
        Self::GaussianNyquist,
        Self::GaussianNyquistShort,
        Self::GaussianCompact,
        Self::GaussianHuge,
        Self::GaussianHugeApod,
        Self::Nuttall,
        Self::Flattop,
        Self::FlattopShort,
        Self::FlattopMedium,
        Self::FlattopLong,
        Self::FlattopXlong,
        Self::FlattopXlongApod,
        Self::FlattopXlongStd,
        Self::FlattopEarly,
        Self::FlattopEarlyIp,
        Self::FlattopEarlyMp,
        Self::FlattopCompact,
        Self::FlattopHuge,
        Self::FlattopMega,
        Self::Chebyshev,
        Self::ChebyshevMp,
        Self::ChebyshevShort,
        Self::ChebyshevShortMp,
        Self::ChebyshevLong,
        Self::ChebyshevLongIp,
        Self::ChebyshevLongMp,
        Self::KaiserSteep,
        Self::KaiserSteepMp,
        Self::KaiserSteepShort,
        Self::KaiserSteepShortMp,
        Self::KaiserCompact,
        Self::HalfbandKaiser,
        Self::HalfbandLarge,
        Self::HalfbandMedium,
        Self::HalfbandSmall,
        Self::HalfbandTiny,
        Self::HannNyquist,
        Self::Lanczos23,
        Self::Lanczos23Mp,
        Self::Lanczos511,
        Self::Lanczos2047,
        Self::Lanczos8191,
        Self::Lanczos32767,
        Self::Lanczos65535,
        Self::ShortLeaky,
        Self::ShortLeakyMp,
        Self::CascadeElliptic,
        Self::CascadeInverseChebyshev,
        Self::CascadeFir,
        Self::CascadeFirAsym,
        Self::CascadeFirMp,
        Self::CascadeFft,
        Self::Cosine,
        Self::CatmullRom,
        Self::TableSinc,
        Self::BlockFft,
        Self::BlockFftFast,
        Self::BlockFftLarge,
    ];

    /// The preset's config name (its snake_case JSON form).
    pub fn name(self) -> &'static str {
        NAMES[Self::ALL
            .iter()
            .position(|p| *p == self)
            .expect("listed in ALL")]
    }

    /// The preset with config name `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        NAMES.iter().position(|n| *n == name).map(|i| Self::ALL[i])
    }
}

/// Config names of [`FilterPreset::ALL`], same order.
const NAMES: &[&str] = &[
    "gaussian_short",
    "gaussian_medium",
    "gaussian_long",
    "gaussian_xlong",
    "gaussian_xlong_apod",
    "gaussian_early",
    "gaussian_early_ip",
    "gaussian_early_mp",
    "gaussian_nyquist",
    "gaussian_nyquist_short",
    "gaussian_compact",
    "gaussian_huge",
    "gaussian_huge_apod",
    "nuttall",
    "flattop",
    "flattop_short",
    "flattop_medium",
    "flattop_long",
    "flattop_xlong",
    "flattop_xlong_apod",
    "flattop_xlong_std",
    "flattop_early",
    "flattop_early_ip",
    "flattop_early_mp",
    "flattop_compact",
    "flattop_huge",
    "flattop_mega",
    "chebyshev",
    "chebyshev_mp",
    "chebyshev_short",
    "chebyshev_short_mp",
    "chebyshev_long",
    "chebyshev_long_ip",
    "chebyshev_long_mp",
    "kaiser_steep",
    "kaiser_steep_mp",
    "kaiser_steep_short",
    "kaiser_steep_short_mp",
    "kaiser_compact",
    "halfband_kaiser",
    "halfband_large",
    "halfband_medium",
    "halfband_small",
    "halfband_tiny",
    "hann_nyquist",
    "lanczos23",
    "lanczos23_mp",
    "lanczos511",
    "lanczos2047",
    "lanczos8191",
    "lanczos32767",
    "lanczos65535",
    "short_leaky",
    "short_leaky_mp",
    "cascade_elliptic",
    "cascade_inverse_chebyshev",
    "cascade_fir",
    "cascade_fir_asym",
    "cascade_fir_mp",
    "cascade_fft",
    "cosine",
    "catmull_rom",
    "table_sinc",
    "block_fft",
    "block_fft_fast",
    "block_fft_large",
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UpsamplerConfig {
    #[serde(default)]
    pub target: RateTarget,
    /// A catalogue preset name (any [`FilterPreset`] name or `"recommended"`).
    /// Mutually exclusive with [`Self::design`]; if neither is set, the engine
    /// uses `"recommended"`.
    #[serde(default)]
    pub preset: Option<String>,
    /// An explicit filter design. Mutually exclusive with [`Self::preset`].
    #[serde(default)]
    pub design: Option<StoredDesign>,
}

impl UpsamplerConfig {
    /// Resolve an output-dependent budget (`match_output`) inside an explicit
    /// custom design before design or response caching. A named preset (or
    /// implicit `recommended`) stays as a name; callers that resolve a preset
    /// resolve its `match_output` via [`FilterDesign::with_output`].
    pub fn with_output(&self, output: &OutputConfig) -> Self {
        let mut resolved = self.clone();
        if let Some(d) = &mut resolved.design {
            *d = d.with_output(output);
        }
        resolved
    }
}

// ---------------------------------------------------------------- Output

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum DitherKind {
    None,
    /// Triangular PDF, ±1 LSB, with the original generator and full-scale gain.
    #[default]
    Tpdf,
    /// TPDF plus error-feedback noise shaping.
    Shaped,
    /// Triangular PDF with per-channel KISS noise and 1 LSB headroom.
    HeadroomTpdf,
    /// Uniform dither, ±1 LSB; 1 LSB headroom.
    Rectangular,
    /// Gaussian σ = 1/3 LSB, rejected outside ±1 LSB; 1 LSB headroom.
    Gaussian,
    /// Sixth-order high-pass coloured TPDF; 1 LSB headroom.
    HighpassTpdf,
    /// First-order all-pole feedback with half-amplitude TPDF; 4 LSB headroom.
    Feedback1,
    /// Five-tap all-pole feedback with TPDF; 2 LSB headroom.
    Feedback5,
    /// Five integrators, two resonators and TPDF; 4 LSB headroom.
    Resonant5,
    /// Nine integrators, two resonators and TPDF; 4 LSB headroom.
    Resonant9,
    /// Fifteen integrators and TPDF; 16 LSB headroom.
    Integrator15,
}

impl DitherKind {
    /// Explicit fixed-coefficient modes also enable a quantizer-only DSP
    /// path. The original three modes retain their stage-driven bypass rule.
    pub fn enables_processing(self) -> bool {
        !matches!(self, Self::None | Self::Tpdf | Self::Shaped)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    S16,
    /// 24-bit in a 32-bit container (ALSA S24_LE).
    S24,
    /// Packed 3-byte 24-bit (ALSA S24_3LE).
    S24_3,
    S32,
}

impl SampleFormat {
    /// Number of significant bits the DAC receives.
    pub fn bits(self) -> u32 {
        match self {
            SampleFormat::S16 => 16,
            SampleFormat::S24 | SampleFormat::S24_3 => 24,
            SampleFormat::S32 => 32,
        }
    }

    /// Short display name ("S16", "S24", "S24_3", "S32").
    pub fn name(self) -> &'static str {
        match self {
            SampleFormat::S16 => "S16",
            SampleFormat::S24 => "S24",
            SampleFormat::S24_3 => "S24_3",
            SampleFormat::S32 => "S32",
        }
    }
}

/// Bit depth dither actually targets: the configured `output.bits`, capped at
/// the container width of the negotiated ALSA format (dithering to 24 bits
/// into an S16 device would leave the packer to truncate the low 8 bits).
pub fn effective_dither_bits(configured: u32, format: Option<SampleFormat>) -> u32 {
    match format {
        Some(f) => configured.min(f.bits()),
        None => configured,
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputConfig {
    /// Effective bit depth that dither targets (<= format bits). Many
    /// "32-bit" DACs resolve ~24 bits, so 24 is a sane default.
    #[serde(default = "default_bits")]
    pub bits: u32,
    #[serde(default)]
    pub dither: DitherKind,
    /// Fixed headroom in dB (0..=12) taken off at the end of the f64 chain,
    /// after harmonics and before the A/B mix, volume, limiter and quantizer,
    /// at every ratio: it keeps the peaks the upsampler (or the DAC's own
    /// filter) reconstructs between samples, up to about +3 dB on loud
    /// masters, from clipping. Default [`DEFAULT_OUTPUT_HEADROOM_DB`].
    #[serde(default = "default_output_headroom_db")]
    pub headroom_db: f64,
}

/// The default [`OutputConfig::headroom_db`]: a 3 dB cut.
pub const DEFAULT_OUTPUT_HEADROOM_DB: f64 = 3.0;

fn default_output_headroom_db() -> f64 {
    DEFAULT_OUTPUT_HEADROOM_DB
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            bits: default_bits(),
            dither: DitherKind::Tpdf,
            headroom_db: DEFAULT_OUTPUT_HEADROOM_DB,
        }
    }
}

// ---------------------------------------------------------------- Limiter

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum LimiterMode {
    #[default]
    Off,
    Limit,
    AutoHeadroom,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct LimiterConfig {
    pub mode: LimiterMode,
    /// Post-volume ceiling immediately before quantization, -6..=0 dBFS.
    pub ceiling_dbfs: f64,
    /// Total stage delay, 5..=20 ms (rounded up to whole frames).
    pub lookahead_ms: f64,
    /// Gain release time constant, 10..=2000 ms.
    pub release_ms: f64,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            mode: LimiterMode::AutoHeadroom,
            ceiling_dbfs: -0.5,
            lookahead_ms: 5.0,
            release_ms: 100.0,
        }
    }
}

impl LimiterConfig {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value, min, max) in [
            ("ceiling_dbfs", self.ceiling_dbfs, -6.0, 0.0),
            ("lookahead_ms", self.lookahead_ms, 5.0, 20.0),
            ("release_ms", self.release_ms, 10.0, 2000.0),
        ] {
            if !value.is_finite() || !(min..=max).contains(&value) {
                return Err(format!(
                    "limiter.{name} must be finite and in {min}..={max}"
                ));
            }
        }
        Ok(())
    }
}

/// Limiter telemetry. From the engine, per block: events since the last
/// block (headroom steps in `auto_headroom`, limiting onsets in `limit`) and
/// the largest gain reduction of that block. In `meters` the daemon reports
/// the running event total and the largest reduction of the report window.
/// `headroom_db` is the currently held automatic headroom.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct LimiterStatus {
    pub active: bool,
    pub headroom_db: f64,
    pub gain_reduction_db: f64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub events: u64,
}

// ---------------------------------------------------------------- Pitch

/// Quality preset for tempo-preserving pitch shifting.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum PitchQuality {
    /// Library default STFT geometry (120 ms window, 30 ms hop) with split computation.
    Low,
    /// Higher overlap (120 ms window, 15 ms hop) with split computation.
    #[default]
    High,
    /// Longest window (240 ms window, 30 ms hop) with split computation.
    Max,
}

/// Tempo-preserving pitch shift configuration.
///
/// Off by default (`cents == 0.0` or `enabled == false`). At most one of
/// `cents` (non-zero), `semitones`, or `reference_hz` + `target_hz` should be
/// set; `effective_cents()` resolves the shift in cents.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PitchConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Pitch shift in cents (-1200.0 ..= 1200.0).
    #[serde(default)]
    pub cents: f64,
    /// Optional pitch shift in semitones (-12.0 ..= 12.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semitones: Option<f64>,
    /// Convenience tuning reference frequency in Hz (e.g. 440.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_hz: Option<f64>,
    /// Convenience tuning target frequency in Hz (e.g. 432.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_hz: Option<f64>,
    /// Quality preset.
    #[serde(default)]
    pub quality: PitchQuality,
}

impl Default for PitchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cents: 0.0,
            semitones: None,
            reference_hz: None,
            target_hz: None,
            quality: PitchQuality::High,
        }
    }
}

// ---------------------------------------------------------------- Crossfeed

pub const CROSSFEED_CUTOFF_MIN_HZ: f64 = 300.0;
pub const CROSSFEED_CUTOFF_MAX_HZ: f64 = 2000.0;
pub const CROSSFEED_FEED_MIN_DB: f64 = 1.0;
pub const CROSSFEED_FEED_MAX_DB: f64 = 15.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CrossfeedMode {
    #[default]
    Bs2bDefault,
    Bs2bChuMoy,
    Bs2bJanMeier,
    Bs2bCustom,
    Linkwitz,
}

/// How the crossfeed stage sets its output level.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CrossfeedLevel {
    /// Same A-weighted loudness with crossfeed on and off, for stereo pink
    /// noise with inter-channel correlation 0.5. Rare worst-case peaks can
    /// exceed the input's peak by up to 5.6 dB and then clip at the output
    /// unless the limiter is on or enough digital attenuation precedes the
    /// quantizer (hardware volume gives no headroom).
    #[default]
    Matched,
    /// Scale by the exact worst-case peak gain: the output can never exceed
    /// the input's peak, at the cost of 1.4 to 5.6 dB of level.
    PeakSafe,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CrossfeedConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: CrossfeedMode,
    /// Low-pass cutoff frequency in Hz for `bs2b_custom` (300..=2000).
    #[serde(default = "default_crossfeed_cutoff_hz")]
    pub cutoff_hz: f64,
    /// Low-frequency crossfeed attenuation in dB for `bs2b_custom` (1..=15).
    #[serde(default = "default_crossfeed_feed_db")]
    pub feed_db: f64,
    /// Level policy of the stage (default `matched`).
    #[serde(default)]
    pub level: CrossfeedLevel,
}

impl Default for CrossfeedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: CrossfeedMode::default(),
            cutoff_hz: default_crossfeed_cutoff_hz(),
            feed_db: default_crossfeed_feed_db(),
            level: CrossfeedLevel::default(),
        }
    }
}

impl PitchConfig {
    /// Effective pitch shift in cents.
    pub fn effective_cents(&self) -> f64 {
        if let (Some(ref_hz), Some(tgt_hz)) = (self.reference_hz, self.target_hz)
            && ref_hz > 0.0
            && tgt_hz > 0.0
        {
            1200.0 * (tgt_hz / ref_hz).log2()
        } else if let Some(st) = self.semitones {
            st * 100.0
        } else {
            self.cents
        }
    }

    /// True when pitch shifting is enabled and the effective shift is non-zero.
    pub fn is_active(&self) -> bool {
        self.enabled && self.effective_cents() != 0.0
    }
}

impl CrossfeedConfig {
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Effective `(cutoff_hz, feed_db)` for the selected mode.
    pub fn effective_params(&self) -> (f64, f64) {
        match self.mode {
            CrossfeedMode::Bs2bDefault => (700.0, 4.5),
            CrossfeedMode::Bs2bChuMoy => (700.0, 6.0),
            CrossfeedMode::Bs2bJanMeier => (650.0, 9.5),
            CrossfeedMode::Bs2bCustom => (self.cutoff_hz, self.feed_db),
            // Corner chosen for a 0.3 ms low-frequency interaural delay:
            // f_c = (1 + R) / (2π · 0.3 ms · R), R = 10^(6/20).
            CrossfeedMode::Linkwitz => (796.4, 6.0),
        }
    }
}

fn default_crossfeed_cutoff_hz() -> f64 {
    700.0
}

fn default_crossfeed_feed_db() -> f64 {
    4.5
}

// ---------------------------------------------------------------- Harmonics

pub const HARMONICS_MIN_DB: f64 = -120.0;
pub const HARMONICS_MAX_DB: f64 = -20.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum HarmonicsCharacter {
    /// Warm triode-like profile: dominant 2nd harmonic (-40 dB), subtle 3rd (-58 dB) and 4th (-72 dB).
    #[default]
    Triode,
    /// Pentode-like profile: dominant 3rd harmonic (-42 dB), subtle 2nd (-56 dB) and 5th (-66 dB).
    Pentode,
    /// Custom per-harmonic levels.
    Custom,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub character: HarmonicsCharacter,
    /// 2nd harmonic level relative to fundamental at 0 dBFS (dB, -120..=-20).
    #[serde(default = "default_h2_db")]
    pub h2_db: f64,
    /// 3rd harmonic level relative to fundamental at 0 dBFS (dB, -120..=-20).
    #[serde(default = "default_h3_db")]
    pub h3_db: f64,
    /// Optional 4th harmonic level at 0 dBFS (dB, -120..=-20); `None` = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h4_db: Option<f64>,
    /// Optional 5th harmonic level at 0 dBFS (dB, -120..=-20); `None` = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h5_db: Option<f64>,
    /// Remove DC offset introduced by even-order harmonics.
    #[serde(default = "yes")]
    pub dc_block: bool,
}

impl Default for HarmonicsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            character: HarmonicsCharacter::default(),
            h2_db: default_h2_db(),
            h3_db: default_h3_db(),
            h4_db: None,
            h5_db: None,
            dc_block: true,
        }
    }
}

impl HarmonicsConfig {
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Effective `(h2_db, h3_db, h4_db, h5_db)` for the selected character.
    pub fn effective_levels_db(&self) -> (f64, f64, Option<f64>, Option<f64>) {
        match self.character {
            HarmonicsCharacter::Triode => (-40.0, -58.0, Some(-72.0), None),
            HarmonicsCharacter::Pentode => (-56.0, -42.0, None, Some(-66.0)),
            HarmonicsCharacter::Custom => (self.h2_db, self.h3_db, self.h4_db, self.h5_db),
        }
    }
}

fn default_h2_db() -> f64 {
    -40.0
}

fn default_h3_db() -> f64 {
    -58.0
}

// ---------------------------------------------------------------- DC block

pub const DC_BLOCK_CUTOFF_MIN_HZ: f64 = 1.0;
pub const DC_BLOCK_CUTOFF_MAX_HZ: f64 = 20.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DcBlockConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// High-pass cutoff frequency in Hz (1..=20).
    #[serde(default = "default_dc_block_cutoff_hz")]
    pub cutoff_hz: f64,
}

impl Default for DcBlockConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cutoff_hz: default_dc_block_cutoff_hz(),
        }
    }
}

impl DcBlockConfig {
    pub fn is_active(&self) -> bool {
        self.enabled
    }
}

fn default_dc_block_cutoff_hz() -> f64 {
    5.0
}

// ---------------------------------------------------------------- Chain

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ChainConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dc_block: Option<DcBlockConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crossfeed: Option<CrossfeedConfig>,
    #[serde(default)]
    pub limiter: LimiterConfig,
    #[serde(default)]
    pub eq: Option<EqProfile>,
    #[serde(default, deserialize_with = "deserialize_conv_list")]
    pub convolution: Vec<ConvolutionConfig>,
    #[serde(default)]
    pub pitch: PitchConfig,
    #[serde(default)]
    pub upsampler: UpsamplerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harmonics: Option<HarmonicsConfig>,
    #[serde(default)]
    pub output: OutputConfig,
}

/// Deserialize a convolution list from `null`, a single [`ConvolutionConfig`]
/// object, or an array of [`ConvolutionConfig`] objects.
pub fn deserialize_conv_list<'de, D>(deserializer: D) -> Result<Vec<ConvolutionConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ConvListVisitor;

    impl<'de> serde::de::Visitor<'de> for ConvListVisitor {
        type Value = Vec<ConvolutionConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("null, a convolution config object, or an array of convolution configs")
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_some<D2: serde::Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> Result<Self::Value, D2::Error> {
            deserializer.deserialize_any(self)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(item) = seq.next_element()? {
                out.push(item);
            }
            Ok(out)
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            let one =
                ConvolutionConfig::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![one])
        }
    }

    deserializer.deserialize_any(ConvListVisitor)
}

impl ChainConfig {
    /// Every stage off, the limiter off and no output headroom: a chain that
    /// changes nothing ([`Self::is_passthrough`]). The default config instead
    /// runs the limiter in `auto_headroom` and cuts
    /// [`DEFAULT_OUTPUT_HEADROOM_DB`].
    pub fn neutral() -> Self {
        Self {
            limiter: LimiterConfig {
                mode: LimiterMode::Off,
                ..LimiterConfig::default()
            },
            output: OutputConfig {
                headroom_db: 0.0,
                ..OutputConfig::default()
            },
            ..Self::default()
        }
    }

    /// True when no stage changes the signal (DC block/crossfeed/EQ/
    /// convolution/pitch/harmonics absent or disabled, no upsampling,
    /// explicit fixed-coefficient dither or output headroom). The engine then
    /// uses the bit-exact path.
    pub fn is_passthrough(&self) -> bool {
        let dc_block_active = self.dc_block.as_ref().is_some_and(DcBlockConfig::is_active);
        let crossfeed_active = self
            .crossfeed
            .as_ref()
            .is_some_and(CrossfeedConfig::is_active);
        let eq_active = self.eq.as_ref().is_some_and(EqProfile::is_active);
        let conv_active = self.convolution.iter().any(|c| c.enabled);
        let harmonics_active = self
            .harmonics
            .as_ref()
            .is_some_and(HarmonicsConfig::is_active);
        !dc_block_active
            && !crossfeed_active
            && !eq_active
            && !conv_active
            && !self.pitch.is_active()
            && !harmonics_active
            && self.limiter.mode == LimiterMode::Off
            && matches!(self.upsampler.target, RateTarget::Off)
            && !self.output.dither.enables_processing()
            && self.output.headroom_db == 0.0
    }
}

/// Resolve an upsampling target into an output/input ratio for a source
/// rate, without knowing the device ([`RateTarget::MaxRateAny`] resolves
/// like [`RateTarget::MaxRate`] here; the engine refines it from the
/// device's rates when it opens the device).
pub fn resolve_rate_ratio(target: RateTarget, source_rate: u32) -> RateRatio {
    match target {
        RateTarget::Off => RateRatio::ONE,
        RateTarget::Ratio(r) => RateRatio::integer(r),
        RateTarget::MaxRate(max) | RateTarget::MaxRateAny(max) => {
            let mut r = 1u32;
            while u64::from(source_rate) * u64::from(r) * 2 <= u64::from(max) {
                r *= 2;
            }
            RateRatio::integer(r)
        }
        RateTarget::Rate(rate) => RateRatio::from_rates(source_rate.max(1), rate.max(1)),
    }
}

fn yes() -> bool {
    true
}
fn default_q() -> f64 {
    std::f64::consts::FRAC_1_SQRT_2
}
fn default_passband_end() -> f64 {
    0.9
}
fn default_stopband_start() -> f64 {
    1.0
}
fn default_attenuation_db() -> f64 {
    150.0
}
fn default_bits() -> u32 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfeed_level_defaults_to_matched() {
        let cf: CrossfeedConfig = serde_json::from_str(r#"{"mode":"bs2b_chu_moy"}"#).unwrap();
        assert_eq!(cf.level, CrossfeedLevel::Matched);
        assert_eq!(CrossfeedConfig::default().level, CrossfeedLevel::Matched);
        let cf: CrossfeedConfig = serde_json::from_str(r#"{"level":"peak_safe"}"#).unwrap();
        assert_eq!(cf.level, CrossfeedLevel::PeakSafe);
        let json = serde_json::to_value(&cf).unwrap();
        assert_eq!(json["level"], "peak_safe");
        assert!(serde_json::from_str::<CrossfeedConfig>(r#"{"level":"loud"}"#).is_err());
    }

    #[test]
    fn limiter_defaults_wire_and_validation() {
        // The default: auto headroom and a 3 dB output cut.
        let chain: ChainConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(chain, ChainConfig::default());
        assert_eq!(chain.limiter.mode, LimiterMode::AutoHeadroom);
        assert_eq!(chain.output.headroom_db, DEFAULT_OUTPUT_HEADROOM_DB);
        assert!(!chain.is_passthrough());
        assert!(ChainConfig::neutral().is_passthrough());
        // A stored mode is kept.
        let off: ChainConfig = serde_json::from_str(r#"{"limiter":{"mode":"off"}}"#).unwrap();
        assert_eq!(off.limiter.mode, LimiterMode::Off);
        let chain = ChainConfig::neutral();
        for mode in [LimiterMode::Limit, LimiterMode::AutoHeadroom] {
            let mut chain = chain.clone();
            chain.limiter.mode = mode;
            assert!(!chain.is_passthrough());
            let json = serde_json::to_string(&chain).unwrap();
            assert_eq!(serde_json::from_str::<ChainConfig>(&json).unwrap(), chain);
        }
        let config: LimiterConfig = serde_json::from_str(r#"{"mode":"limit"}"#).unwrap();
        assert_eq!(config.ceiling_dbfs, -0.5);
        assert!(config.validate().is_ok());
        for (field, values) in [
            ("ceiling", vec![f64::NAN, -6.1, 0.1]),
            ("lookahead", vec![f64::INFINITY, 0.0, 20.1]),
            ("release", vec![f64::NEG_INFINITY, 9.9, 2000.1]),
        ] {
            for value in values {
                let mut c = config.clone();
                match field {
                    "ceiling" => c.ceiling_dbfs = value,
                    "lookahead" => c.lookahead_ms = value,
                    _ => c.release_ms = value,
                }
                assert!(c.validate().is_err());
            }
        }
    }

    #[test]
    fn preset_names_match_serde() {
        assert_eq!(FilterPreset::ALL.len(), NAMES.len());
        for &p in FilterPreset::ALL {
            let json = serde_json::to_value(p).unwrap();
            assert_eq!(json.as_str(), Some(p.name()));
            assert_eq!(FilterPreset::from_name(p.name()), Some(p));
            let back: FilterPreset = serde_json::from_value(json).unwrap();
            assert_eq!(back, p);
        }
        let mut names = NAMES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), NAMES.len(), "duplicate preset names");
    }

    #[test]
    fn ratio_resolution() {
        let r = |t, rate| resolve_rate_ratio(t, rate);
        assert_eq!(
            r(RateTarget::MaxRate(768_000), 44_100),
            RateRatio::integer(16)
        );
        assert_eq!(
            r(RateTarget::MaxRate(384_000), 96_000),
            RateRatio::integer(4)
        );
        assert_eq!(
            r(RateTarget::MaxRate(705_600), 44_100),
            RateRatio::integer(16)
        );
        assert_eq!(
            r(RateTarget::MaxRate(768_000), 48_000),
            RateRatio::integer(16)
        );
        assert_eq!(
            r(RateTarget::MaxRateAny(768_000), 44_100),
            RateRatio::integer(16)
        );
        assert_eq!(r(RateTarget::Off, 48_000), RateRatio::ONE);
        assert_eq!(
            r(RateTarget::Rate(48_000), 44_100),
            RateRatio::new(160, 147)
        );
        assert_eq!(r(RateTarget::Rate(96_000), 192_000), RateRatio::new(1, 2));
        assert_eq!(r(RateTarget::Rate(176_400), 44_100), RateRatio::integer(4));
        assert_eq!(r(RateTarget::Rate(44_100), 44_100), RateRatio::ONE);
        let json = serde_json::to_string(&RateTarget::Rate(48_000)).unwrap();
        assert_eq!(json, r#"{"rate":48000}"#);
        let json = serde_json::to_string(&RateTarget::MaxRateAny(768_000)).unwrap();
        assert_eq!(json, r#"{"max_rate_any":768000}"#);
    }

    #[test]
    fn high_output_rates_resolve_to_x32_and_x64() {
        let r = |t, rate| resolve_rate_ratio(t, rate);
        let cap = RateTarget::MaxRate(MAX_OUTPUT_RATE);
        assert_eq!(r(cap, 44_100), RateRatio::integer(64));
        assert_eq!(r(cap, 48_000), RateRatio::integer(64));
        assert_eq!(r(cap, 96_000), RateRatio::integer(32));
        assert_eq!(r(cap, 768_000), RateRatio::integer(4));
        assert_eq!(
            r(RateTarget::MaxRate(1_536_000), 44_100),
            RateRatio::integer(32)
        );
        assert_eq!(
            r(RateTarget::Rate(2_822_400), 44_100),
            RateRatio::integer(64)
        );
        assert_eq!(
            r(RateTarget::Rate(2_822_400), 48_000),
            RateRatio::new(294, 5)
        );
        for &rate in HIGH_OUTPUT_RATES {
            assert!(is_output_rate(rate) && rate <= MAX_OUTPUT_RATE);
            assert!(!ALLOWED_RATES.contains(&rate));
        }
        assert!(is_output_rate(44_100) && !is_output_rate(1_000_000));
        assert_eq!(
            HIGH_OUTPUT_RATES.iter().max(),
            Some(&MAX_OUTPUT_RATE),
            "the cap is the highest rate"
        );
    }

    #[test]
    fn upsampler_config_wire_and_legacy_fallback() {
        // An old flat config ignores unknown fields and falls back to preset/design None.
        let old = r#"{"target":{"ratio":8},"taps":65536,"phase":"minimum","kaiser_beta":14.0,"cutoff":1.0,"cutoff_mode":"nyquist_stop"}"#;
        let up: UpsamplerConfig = serde_json::from_str(old).unwrap();
        assert_eq!(up.target, RateTarget::Ratio(8));
        assert_eq!(up.preset, None);
        assert_eq!(up.design, None);

        // Preset by name (including "recommended").
        let preset_json = r#"{"target":{"max_rate":768000},"preset":"flattop_long","design":null}"#;
        let up: UpsamplerConfig = serde_json::from_str(preset_json).unwrap();
        assert_eq!(up.target, RateTarget::MaxRate(768_000));
        assert_eq!(up.preset.as_deref(), Some("flattop_long"));
        assert_eq!(up.design, None);
        assert_eq!(attenuation_for_bits(24).round(), 146.0);
    }

    #[test]
    fn explicit_dither_modes_round_trip_and_enable_processing() {
        for kind in [
            DitherKind::HeadroomTpdf,
            DitherKind::Rectangular,
            DitherKind::Gaussian,
            DitherKind::HighpassTpdf,
            DitherKind::Feedback1,
            DitherKind::Feedback5,
            DitherKind::Resonant5,
            DitherKind::Resonant9,
            DitherKind::Integrator15,
        ] {
            let mut cfg = ChainConfig::neutral();
            cfg.output.dither = kind;
            assert!(!cfg.is_passthrough());
            let json = serde_json::to_string(&cfg).unwrap();
            assert_eq!(serde_json::from_str::<ChainConfig>(&json).unwrap(), cfg);
        }
    }

    #[test]
    fn match_output_budget_follows_each_dither_kinds_noise() {
        let budget = |dither| {
            filter_error_budget_db(&OutputConfig {
                bits: 16,
                dither,
                headroom_db: 0.0,
            })
        };
        let none = budget(DitherKind::None);
        for (kind, below) in [
            (DitherKind::Tpdf, 4.7712),
            (DitherKind::HeadroomTpdf, 4.7712),
            (DitherKind::Integrator15, 4.7712),
            (DitherKind::Feedback1, 1.7609),
            (DitherKind::Rectangular, 6.9897),
            (DitherKind::Gaussian, 3.6131),
            (DitherKind::HighpassTpdf, 1.8988),
        ] {
            assert!((none - budget(kind) - below).abs() < 1e-3, "{kind:?}");
        }
    }

    #[test]
    fn automatic_filter_wire_format() {
        let up: UpsamplerConfig = serde_json::from_str(
            r#"{
                "design": {
                    "kind": "windowed_sinc",
                    "window": { "type": "kaiser", "beta": null, "sampling": "symmetric" },
                    "length": { "rule": "auto", "budget": "match_output" },
                    "cutoff": { "mode": "band", "passband_end": 0.9, "stopband_start": 1.0, "attenuation_db": 150.0 },
                    "phase": { "type": "minimum", "method": "reflect" }
                }
            }"#,
        )
        .unwrap();
        assert!(up.design.as_ref().unwrap().has_match_output());
        let resolved = up.with_output(&OutputConfig {
            bits: 16,
            dither: DitherKind::None,
            headroom_db: 0.0,
        });
        let value = serde_json::to_value(&resolved).unwrap();
        assert_eq!(
            value["design"]["length"]["budget"],
            serde_json::json!({"db": 98.08})
        );
        // 32-bit output is capped at the 24-bit equivalent.
        for (dither, db) in [(DitherKind::None, 146.24), (DitherKind::Tpdf, 141.4688)] {
            let out = OutputConfig {
                bits: 32,
                dither,
                headroom_db: 0.0,
            };
            let resolved = up.with_output(&out);
            let Some(StoredDesign {
                design:
                    FilterDesign::Kernel(design::KernelDesign {
                        shape:
                            design::Shape::WindowedSinc {
                                length: design::Length::Auto(FilterErrorBudget::Db(a)),
                                ..
                            },
                        ..
                    }),
                ..
            }) = resolved.design
            else {
                panic!("unresolved");
            };
            assert!((a - db).abs() < 1e-3, "{a}");
            assert_eq!(
                a,
                filter_error_budget_db(&OutputConfig {
                    bits: 24,
                    dither,
                    headroom_db: 0.0
                })
            );
        }
    }

    #[test]
    fn empty_chain_is_passthrough() {
        assert!(ChainConfig::neutral().is_passthrough());
        assert!(!ChainConfig::default().is_passthrough());
        let mut no_cut = ChainConfig::neutral();
        no_cut.output.headroom_db = 0.5;
        assert!(!no_cut.is_passthrough());
    }

    #[test]
    fn eq_profile_legacy_migration_none_and_right_some() {
        // Legacy profile without right section stays linked in v2.
        let old_linked = r#"{"name":"old","preamp_db":-3.0,"bands":[{"kind":"peaking","freq_hz":1000.0,"gain_db":3.0,"q":1.0,"enabled":true}]}"#;
        let eq: EqProfile = serde_json::from_str(old_linked).unwrap();
        assert_eq!(eq.version, EQ_PROFILE_VERSION);
        assert!(eq.linked_enabled);
        assert_eq!(eq.preamp_db, -3.0);
        assert_eq!(eq.bands.len(), 1);
        assert_eq!(eq.left, None);
        assert_eq!(eq.right, None);
        assert!(eq.is_active());
        let reserialized = serde_json::to_string(&eq).unwrap();
        assert!(reserialized.contains("\"version\":2"));
        assert!(!reserialized.contains("\"left\""));
        assert!(!reserialized.contains("\"right\""));

        // Legacy split profile (version absent or 1, right: Some) moves left into .left.
        let old_split = r#"{
            "name":"old-split",
            "version":1,
            "preamp_db":-2.5,
            "bands":[{"kind":"peaking","freq_hz":200.0,"gain_db":2.0,"q":1.0,"enabled":true}],
            "right":{"preamp_db":-1.0,"bands":[{"kind":"peaking","freq_hz":500.0,"gain_db":4.0,"q":1.0,"enabled":true}]}
        }"#;
        let migrated: EqProfile = serde_json::from_str(old_split).unwrap();
        assert_eq!(migrated.version, EQ_PROFILE_VERSION);
        assert!(migrated.linked_enabled);
        assert_eq!(migrated.preamp_db, 0.0);
        assert!(migrated.bands.is_empty());
        let left = migrated.left.as_ref().expect("left populated from legacy");
        assert!(left.enabled);
        assert_eq!(left.preamp_db, -2.5);
        assert_eq!(left.bands.len(), 1);
        assert_eq!(left.bands[0].freq_hz, 200.0);
        let right = migrated.right.as_ref().expect("right preserved");
        assert!(right.enabled);
        assert_eq!(right.preamp_db, -1.0);
        assert_eq!(right.bands.len(), 1);
        assert_eq!(right.bands[0].freq_hz, 500.0);

        let rt_json = serde_json::to_string(&migrated).unwrap();
        let rt: EqProfile = serde_json::from_str(&rt_json).unwrap();
        assert_eq!(rt, migrated);
    }

    #[test]
    fn eq_profile_v2_roundtrip_and_is_active() {
        let layered = EqProfile {
            name: "layered".into(),
            version: 0, // Serialize must always emit version 2
            preamp_db: 0.0,
            linked_enabled: true,
            bands: vec![Band {
                kind: FilterType::Peaking,
                freq_hz: 100.0,
                gain_db: -2.0,
                q: 2.0,
                enabled: true,
            }],
            left: None,
            right: Some(ChannelEq {
                enabled: true,
                preamp_db: -2.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 500.0,
                    gain_db: 2.0,
                    q: 1.0,
                    enabled: true,
                }],
            }),
            tilt: None,
            tilt_high: None,
        };
        assert!(layered.is_active());
        let json = serde_json::to_string(&layered).unwrap();
        assert!(json.contains("\"version\":2"));
        assert!(!json.contains("\"left\""));
        assert!(json.contains("\"right\""));
        let back: EqProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, EQ_PROFILE_VERSION);
        assert_eq!(back.bands.len(), 1);
        assert_eq!(back.left, None);
        assert_eq!(back.right, layered.right);

        // Disabled linked section and disabled ChannelEq sections make profile inactive
        // unless global preamp_db is non-zero.
        let disabled_sections = EqProfile {
            name: "disabled".into(),
            version: EQ_PROFILE_VERSION,
            preamp_db: 0.0,
            linked_enabled: false,
            bands: layered.bands.clone(),
            left: Some(ChannelEq {
                enabled: false,
                preamp_db: -3.0,
                bands: layered.bands.clone(),
            }),
            right: Some(ChannelEq {
                enabled: false,
                preamp_db: 1.5,
                bands: layered.bands,
            }),
            tilt: None,
            tilt_high: None,
        };
        assert!(!disabled_sections.is_active());

        let preamp_only = EqProfile {
            preamp_db: -1.0,
            ..disabled_sections
        };
        assert!(preamp_only.is_active());
    }

    #[test]
    fn convolution_config_deserializes_null_object_and_array() {
        let missing: ChainConfig = serde_json::from_str(r#"{}"#).unwrap();
        assert!(missing.convolution.is_empty());

        let null: ChainConfig = serde_json::from_str(r#"{"convolution":null}"#).unwrap();
        assert!(null.convolution.is_empty());

        let single: ChainConfig = serde_json::from_str(
            r#"{"convolution":{"enabled":true,"ir_path":"/tmp/ir.wav","gain_db":-1.5}}"#,
        )
        .unwrap();
        assert_eq!(single.convolution.len(), 1);
        assert_eq!(single.convolution[0].name, None);
        assert_eq!(single.convolution[0].ir_path, PathBuf::from("/tmp/ir.wav"));
        assert_eq!(single.convolution[0].gain_db, -1.5);
        assert!(!single.is_passthrough());

        let many: ChainConfig = serde_json::from_str(
            r#"{"convolution":[{"name":"Room","enabled":true,"ir_path":"/tmp/a.wav","gain_db":0.0},{"enabled":false,"ir_path":"/tmp/b.wav","gain_db":1.0}]}"#,
        )
        .unwrap();
        assert_eq!(many.convolution.len(), 2);
        assert_eq!(many.convolution[0].name.as_deref(), Some("Room"));
        assert!(!many.convolution[1].enabled);
        assert!(!many.is_passthrough());

        let all_disabled = ChainConfig {
            convolution: vec![ConvolutionConfig {
                name: None,
                enabled: false,
                ir_path: PathBuf::from("/tmp/x.wav"),
                gain_db: 0.0,
            }],
            ..ChainConfig::neutral()
        };
        assert!(all_disabled.is_passthrough());
    }

    #[test]
    fn pitch_config_effective_cents_and_passthrough() {
        let mut chain = ChainConfig::neutral();
        assert!(!chain.pitch.is_active());
        assert!(chain.is_passthrough());

        chain.pitch = PitchConfig {
            enabled: true,
            reference_hz: Some(440.0),
            target_hz: Some(432.0),
            ..Default::default()
        };
        assert!((chain.pitch.effective_cents() - (-31.76665363342977)).abs() < 1e-9);
        assert!(chain.pitch.is_active());
        assert!(!chain.is_passthrough());

        chain.pitch.enabled = false;
        assert!(!chain.pitch.is_active());
        assert!(chain.is_passthrough());

        let semi = PitchConfig {
            enabled: true,
            semitones: Some(-2.0),
            ..Default::default()
        };
        assert_eq!(semi.effective_cents(), -200.0);
        assert!(semi.is_active());

        let zero_hz = PitchConfig {
            reference_hz: Some(440.0),
            target_hz: Some(440.0),
            ..Default::default()
        };
        assert_eq!(zero_hz.effective_cents(), 0.0);
        assert!(!zero_hz.is_active());
    }

    #[test]
    fn tilt_config_serde_and_helpers() {
        let json = r#"{
            "enabled": true,
            "slope_db_per_oct": -0.7,
            "start_hz": 20.0,
            "end_hz": 20000.0,
            "pivot": {"hz": 1000.0}
        }"#;
        let tilt: TiltConfig = serde_json::from_str(json).unwrap();
        assert!(tilt.enabled);
        assert_eq!(tilt.slope_db_per_oct, Some(-0.7));
        assert_eq!(tilt.total_db, None);
        assert_eq!(tilt.pivot, TiltPivot::Hz(1000.0));
        assert!(tilt.is_active());
        let oct = (20_000.0_f64 / 20.0).log2();
        assert!((tilt.effective_slope_db_per_oct().unwrap() + 0.7).abs() < 1e-12);
        assert!((tilt.effective_total_db().unwrap() + 0.7 * oct).abs() < 1e-12);

        let total_json =
            r#"{"total_db": -6.0, "start_hz": 100.0, "end_hz": 1600.0, "pivot": "start"}"#;
        let total_tilt: TiltConfig = serde_json::from_str(total_json).unwrap();
        assert!(total_tilt.enabled);
        assert_eq!(total_tilt.pivot, TiltPivot::Start);
        assert!((total_tilt.effective_slope_db_per_oct().unwrap() + 1.5).abs() < 1e-12);
        assert!((total_tilt.effective_total_db().unwrap() + 6.0).abs() < 1e-12);

        let eq = EqProfile {
            tilt: Some(tilt),
            ..Default::default()
        };
        assert!(eq.is_active());
        let ser = serde_json::to_string(&eq).unwrap();
        assert!(ser.contains("\"tilt\""));
        let back: EqProfile = serde_json::from_str(&ser).unwrap();
        assert_eq!(back, eq);

        let disabled = EqProfile {
            tilt: Some(TiltConfig {
                enabled: false,
                ..tilt
            }),
            ..Default::default()
        };
        assert!(!disabled.is_active());
    }

    #[test]
    fn two_section_tilt_serde_and_pair_rule() {
        let json = r#"{"version": 2,
            "tilt": {"slope_db_per_oct": 1.0, "start_hz": 20.0, "end_hz": 200.0, "pivot": "end"},
            "tilt_high": {"slope_db_per_oct": -1.0, "start_hz": 4000.0, "end_hz": 20000.0, "pivot": "start"}}"#;
        let eq: EqProfile = serde_json::from_str(json).unwrap();
        let (low, high) = (eq.tilt.unwrap(), eq.tilt_high.unwrap());
        assert!(check_tilt_pair(&low, &high).is_ok());
        assert!(eq.is_active());
        let back: EqProfile = serde_json::from_str(&serde_json::to_string(&eq).unwrap()).unwrap();
        assert_eq!(back, eq);
        // Touching bands are fine; overlapping ones are not.
        let touching = TiltConfig {
            start_hz: 200.0,
            ..high
        };
        assert!(check_tilt_pair(&low, &touching).is_ok());
        let overlap = TiltConfig {
            start_hz: 150.0,
            ..high
        };
        assert!(
            check_tilt_pair(&low, &overlap)
                .unwrap_err()
                .contains("overlap")
        );
        // Any other pivot would leave the middle off 0 dB.
        let centred = TiltConfig {
            pivot: TiltPivot::Center,
            ..low
        };
        assert!(
            check_tilt_pair(&centred, &high)
                .unwrap_err()
                .contains("pivot")
        );
        let high_end = TiltConfig {
            pivot: TiltPivot::End,
            ..high
        };
        assert!(check_tilt_pair(&low, &high_end).is_err());
        // Absent in serialized form when unset.
        let single = EqProfile {
            tilt: Some(low),
            ..Default::default()
        };
        assert!(
            !serde_json::to_string(&single)
                .unwrap()
                .contains("tilt_high")
        );
    }

    #[test]
    fn dc_block_config_defaults_and_passthrough() {
        let chain: ChainConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(chain.dc_block, None);

        let partial: DcBlockConfig = serde_json::from_str("{}").unwrap();
        assert!(partial.enabled);
        assert_eq!(partial.cutoff_hz, 5.0);

        let disabled = ChainConfig {
            dc_block: Some(DcBlockConfig {
                enabled: false,
                cutoff_hz: 10.0,
            }),
            ..ChainConfig::neutral()
        };
        assert!(disabled.is_passthrough());

        let enabled = ChainConfig {
            dc_block: Some(DcBlockConfig::default()),
            ..ChainConfig::neutral()
        };
        assert!(!enabled.is_passthrough());
        let json = serde_json::to_string(&enabled).unwrap();
        assert_eq!(serde_json::from_str::<ChainConfig>(&json).unwrap(), enabled);
    }
}
