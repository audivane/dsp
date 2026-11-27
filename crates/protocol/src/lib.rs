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

/// Scale a 48 kHz base quantum to `rate` the way PipeWire does, so the ALSA
/// period the engine opens with is the quantum the graph actually runs.
///
/// PipeWire turns `node.latency = q/rate` into `q * graph_rate / rate` and,
/// with `clock.power-of-two-quantum` (the default), rounds that *down* to a
/// power of two. So a 1024 quantum runs 512 at 44.1 kHz, 1024 at 88.2 kHz,
/// 2048 at 176.4 kHz and 4096 at 352.8 kHz, never 1881 or 3763. A period
/// that does not match the quantum costs several back-to-back cycles per
/// period and the source clients cannot keep up.
///
/// The result is also capped at PipeWire's default `clock.quantum-limit`
/// (8192): at 768 kHz a 1024 quantum would be 16384, but the graph runs 8192.
pub fn scaled_quantum(q48: u32, rate: u32) -> u32 {
    const PW_QUANTUM_LIMIT: u64 = 8192;
    let q48 = q48.max(32);
    let r = rate.max(8_000) as u64;
    let raw = ((q48 as u64 * r) / 48_000).clamp(32, PW_QUANTUM_LIMIT) as u32;
    if q48.is_power_of_two() {
        (1u32 << (31 - raw.leading_zeros())).max(32)
    } else {
        raw
    }
}

/// The largest block the engine builds its chain with at `rate`:
/// [`scaled_quantum`] clamped to 512..=8192 frames. The upsampler's
/// partitioned convolution runs this rounded up to a power of two, capped
/// at 1024, so its delay (and `status.filter_latency_ms`) follows it.
pub fn chain_max_block(quantum: u32, rate: u32) -> usize {
    scaled_quantum(quantum, rate).clamp(512, 8192) as usize
}

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

/// Bass tone control: an RBJ low shelf at this corner and Q (a gentle,
/// first-order-like shelf, as on some hardware tone controls).
pub const TONE_BASS_HZ: f64 = 140.0;
pub const TONE_BASS_Q: f64 = 0.48;
/// Treble tone control: an RBJ high shelf at this corner and Q.
pub const TONE_TREBLE_HZ: f64 = 4_900.0;
pub const TONE_TREBLE_Q: f64 = 0.49;
/// Largest tone boost or cut, dB (either way). The UI steps by 0.5 dB.
pub const TONE_MAX_DB: f64 = 6.0;

/// Bass and treble knobs: two fixed shelves on every channel, run after the
/// bands of the EQ stage. Not part of the band list. A knob at 0 dB adds no
/// filter, so 0/0 is the same as no `tone` at all.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ToneConfig {
    /// Low shelf gain at [`TONE_BASS_HZ`], dB (±[`TONE_MAX_DB`]).
    #[serde(default)]
    pub bass_db: f64,
    /// High shelf gain at [`TONE_TREBLE_HZ`], dB (±[`TONE_MAX_DB`]).
    #[serde(default)]
    pub treble_db: f64,
}

impl ToneConfig {
    /// The shelves this tone adds, as bands: none for a knob at 0 dB.
    pub fn bands(&self) -> Vec<Band> {
        let mut out = Vec::new();
        if self.bass_db != 0.0 {
            out.push(Band {
                kind: FilterType::LowShelf,
                freq_hz: TONE_BASS_HZ,
                gain_db: self.bass_db,
                q: TONE_BASS_Q,
                enabled: true,
            });
        }
        if self.treble_db != 0.0 {
            out.push(Band {
                kind: FilterType::HighShelf,
                freq_hz: TONE_TREBLE_HZ,
                gain_db: self.treble_db,
                q: TONE_TREBLE_Q,
                enabled: true,
            });
        }
        out
    }

    /// True if either knob is off 0 dB.
    pub fn is_active(&self) -> bool {
        self.bass_db != 0.0 || self.treble_db != 0.0
    }
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
    #[serde(default)]
    tone: Option<ToneConfig>,
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
    /// Bass and treble knobs (two fixed shelves, every channel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<ToneConfig>,
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
            tone: None,
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
                    tone: raw.tone,
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
                    tone: raw.tone,
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
                tone: raw.tone,
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
            #[serde(skip_serializing_if = "Option::is_none")]
            tone: Option<&'a ToneConfig>,
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
            tone: self.tone.as_ref(),
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
            || self.tone.as_ref().is_some_and(ToneConfig::is_active)
    }

    /// The tone knobs' shelves (empty when there is no tone or it is 0/0).
    pub fn tone_bands(&self) -> Vec<Band> {
        self.tone
            .as_ref()
            .map(ToneConfig::bands)
            .unwrap_or_default()
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
    /// per output channel. Resampled offline when its rate differs. Empty
    /// (and may be left out) for a `matrix` item.
    #[serde(default)]
    pub ir_path: PathBuf,
    #[serde(default)]
    pub gain_db: f64,
    /// A FIR matrix in place of `ir_path`: `out_j = Σ_i F_ji * in_i` over
    /// the chain's channels, one entry per non-zero filter (an output with
    /// no entry is silent). `gain_db` above scales every entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<Vec<MatrixEntry>>,
    /// The item the loaded room calibration installed (`bundle_load`; left
    /// out when `false`). The daemon keeps it in place across convolution
    /// edits and preset loads, and only `bundle_unload` removes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub calibration: bool,
}

/// One filter of a convolution matrix: `ir_path` from input channel `in`
/// to output channel `out`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MatrixEntry {
    #[serde(rename = "in")]
    pub input: usize,
    #[serde(rename = "out")]
    pub output: usize,
    /// WAV impulse response, resampled offline when its rate differs.
    pub ir_path: PathBuf,
    /// Which channel of the WAV to use (0-based); left out, the file must
    /// be mono.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<usize>,
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
    /// (a Kaiser design ripples by the same amount in both bands). When
    /// [`Self::passband_end_hz`] is set, this is the fallback at a source
    /// rate where that frequency does not fit the band.
    #[serde(default = "default_passband_end")]
    pub passband_end: f64,
    /// End of the passband as an absolute frequency, Hz: converted to a
    /// fraction of the source Nyquist when the design is built, so the band
    /// does not scale with the source rate (Recommended and `craven_apod`
    /// pin 20 kHz this way; the Custom editor's "Hz" unit sets it). Like
    /// the fractions it is of the lower Nyquist, so decimating converts it
    /// at the output rate. Where
    /// the frequency does not fit the band at a source rate (it must end
    /// below the stopband start, at most at the Nyquist, and leave a
    /// transition no wider than the Nyquist), the design uses
    /// [`Self::passband_end`] there instead and the design check notes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub passband_end_hz: Option<f64>,
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
            passband_end_hz: None,
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
    // Peter Craven's apodizing filter (JAES 52(3), 2004): fully down by the
    // source Nyquist, passband to 20 kHz at every source rate, minimum
    // phase.
    CravenApod,
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
    Nos,
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
        Self::CravenApod,
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
        Self::Nos,
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
    "craven_apod",
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
    "nos",
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
/// Range of the stage's stereo width (side gain `k`; 1 = unchanged).
pub const STEREO_WIDTH_MIN: f64 = 0.0;
pub const STEREO_WIDTH_MAX: f64 = 2.0;
/// Range of the `above_bass` width shape's corner.
pub const STEREO_WIDTH_CORNER_MIN_HZ: f64 = 200.0;
pub const STEREO_WIDTH_CORNER_MAX_HZ: f64 = 2000.0;

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
    /// No crossfeed: the stage runs for its stereo width alone.
    None,
}

/// How the stereo width's side gain depends on frequency.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum StereoWidthShape {
    /// The same side gain at every frequency (an M/S side level control).
    #[default]
    Flat,
    /// Side gain 1 in the bass, `width` above `width_corner_hz` (a
    /// first-order high-pass on the side's change).
    AboveBass,
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
    /// Stereo width: the side (L − R) gain `k`, 0..=2; 0 is mono, 1 leaves
    /// the image as it is. The centre (L + R) is never changed by it.
    #[serde(default = "default_stereo_width")]
    pub width: f64,
    /// Where the width acts (default `flat`).
    #[serde(default)]
    pub width_shape: StereoWidthShape,
    /// Corner of the `above_bass` shape in Hz (200..=2000).
    #[serde(default = "default_stereo_width_corner_hz")]
    pub width_corner_hz: f64,
}

impl Default for CrossfeedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: CrossfeedMode::default(),
            cutoff_hz: default_crossfeed_cutoff_hz(),
            feed_db: default_crossfeed_feed_db(),
            width: default_stereo_width(),
            width_shape: StereoWidthShape::default(),
            width_corner_hz: default_stereo_width_corner_hz(),
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
    /// True when the stage changes the signal: on, with a crossfeed mode or
    /// a width other than 1.
    pub fn is_active(&self) -> bool {
        self.enabled && (self.crossfeed_active() || self.width_active())
    }

    /// True when a crossfeed mode (not `none`) is selected.
    pub fn crossfeed_active(&self) -> bool {
        self.mode != CrossfeedMode::None
    }

    /// True when the width changes the image (`width != 1`).
    pub fn width_active(&self) -> bool {
        self.width != 1.0
    }

    /// Effective `(cutoff_hz, feed_db)` for the selected mode.
    pub fn effective_params(&self) -> (f64, f64) {
        match self.mode {
            CrossfeedMode::Bs2bDefault => (700.0, 4.5),
            CrossfeedMode::Bs2bChuMoy => (700.0, 6.0),
            CrossfeedMode::Bs2bJanMeier => (650.0, 9.5),
            CrossfeedMode::Bs2bCustom | CrossfeedMode::None => (self.cutoff_hz, self.feed_db),
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

fn default_stereo_width() -> f64 {
    1.0
}

fn default_stereo_width_corner_hz() -> f64 {
    700.0
}

// ---------------------------------------------------------------- Harmonics

pub const HARMONICS_MIN_DB: f64 = -120.0;
pub const HARMONICS_MAX_DB: f64 = -20.0;
/// Highest harmonic order of the stage (orders run 2..=9).
pub const HARMONICS_MAX_ORDER: usize = 9;
/// Number of harmonic orders, H2..=H9 (index 0 is H2).
pub const HARMONICS_ORDERS: usize = HARMONICS_MAX_ORDER - 1;
/// Highest order the readouts report: series cross terms and tape layers
/// reach past [`HARMONICS_MAX_ORDER`].
pub const HARMONICS_READOUT_MAX_ORDER: usize = 15;
/// Number of orders in a readout, H2..=H15 (index 0 is H2).
pub const HARMONICS_READOUT_ORDERS: usize = HARMONICS_READOUT_MAX_ORDER - 1;

/// Most layers in one harmonics stage.
pub const HARMONICS_MAX_LAYERS: usize = 8;
/// Lowest Chebyshev layer drive, dB (the operating point moved down the
/// curve; 0 dB is the preset's reference point).
pub const HARMONICS_MIN_DRIVE_DB: f64 = -40.0;
/// Highest Chebyshev layer drive, dB: drive never goes past the reference.
pub const HARMONICS_MAX_DRIVE_DB: f64 = 0.0;

/// Tilt of a Chebyshev layer: low-shelf corner range, Hz (fundamental
/// frequency; the rise starts below it).
pub const TILT_LOW_MIN_HZ: f64 = 20.0;
pub const TILT_LOW_MAX_HZ: f64 = 2_000.0;
/// Tilt: high-shelf corner range, Hz (fundamental frequency; the rise starts
/// above it).
pub const TILT_HIGH_MIN_HZ: f64 = 200.0;
pub const TILT_HIGH_MAX_HZ: f64 = 20_000.0;
/// Tilt: largest rise or cut of either shelf, dB.
pub const TILT_MAX_DB: f64 = 24.0;
/// Tilt: the shelf slopes on offer, dB/oct (shelf order 1..=4).
pub const TILT_SLOPES: [u8; 4] = [6, 12, 18, 24];
/// Tilt: a low shelf's slope when `slope` is omitted, dB/oct.
pub const TILT_LOW_DEFAULT_SLOPE: u8 = 12;
/// Tilt: a high shelf's slope when `slope` is omitted, dB/oct.
pub const TILT_HIGH_DEFAULT_SLOPE: u8 = 6;

/// Sag of a Chebyshev layer: largest drop of the drive at a full envelope,
/// dB.
pub const HARMONICS_SAG_MAX_DB: f64 = 6.0;
/// Bias shift of a Chebyshev layer: largest operating point, fraction of
/// full scale (either sign).
pub const HARMONICS_BIAS_MAX_DEPTH: f64 = 0.25;
/// Sag and bias envelopes: time constant range, ms.
pub const HARMONICS_ENVELOPE_MIN_MS: f64 = 0.1;
pub const HARMONICS_ENVELOPE_MAX_MS: f64 = 10_000.0;

/// Flux layer: low-shelf corner range, Hz (boost below the corner).
pub const FLUX_MIN_CORNER_HZ: f64 = 1.0;
pub const FLUX_MAX_CORNER_HZ: f64 = 5_000.0;
/// Flux layer: largest low-frequency boost, dB.
pub const FLUX_MAX_BOOST_DB: f64 = 40.0;
/// Flux layer: H3 calibration target range, dBc.
pub const FLUX_MIN_H3_DB: f64 = -120.0;
pub const FLUX_MAX_H3_DB: f64 = -12.0;
/// Flux layer: calibration frequency range, Hz.
pub const FLUX_MIN_CALIBRATION_HZ: f64 = 5.0;
pub const FLUX_MAX_CALIBRATION_HZ: f64 = 1_000.0;

/// Hysteresis layer: H3 calibration target range, dBc.
pub const HYSTERESIS_MIN_H3_DB: f64 = -120.0;
pub const HYSTERESIS_MAX_H3_DB: f64 = -12.0;
/// Hysteresis layer: calibration frequency range, Hz.
pub const HYSTERESIS_MIN_CALIBRATION_HZ: f64 = 20.0;
pub const HYSTERESIS_MAX_CALIBRATION_HZ: f64 = 5_000.0;
/// Lowest output rate at which a hysteresis layer stays cleanly below heavy
/// aliasing (4× of 44.1 kHz). Below this, readouts warn.
pub const HYSTERESIS_MIN_CLEAN_RATE_HZ: f64 = 176_400.0;
/// Hysteresis layer: bass EQ pair corner range, Hz.
pub const HYSTERESIS_MIN_EQ_HZ: f64 = 20.0;
pub const HYSTERESIS_MAX_EQ_HZ: f64 = 500.0;
/// Hysteresis layer: largest bass EQ boost into the core, dB.
pub const HYSTERESIS_MAX_EQ_DB: f64 = 12.0;
/// Hysteresis layer: largest field offset (asymmetry), in the magnetic
/// core's knee units.
pub const HYSTERESIS_MAX_ASYMMETRY: f64 = 1.0;

/// Sign of one harmonic's Chebyshev term. For odd orders the effect
/// alternates: at low and moderate levels a negative H3 or H7 compresses
/// (soft saturation), a negative H5 or H9 expands. Flipping an even term's
/// sign flips which half-wave is flattened.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum HarmonicSign {
    #[serde(rename = "+")]
    Positive,
    #[serde(rename = "-")]
    Negative,
}

impl HarmonicSign {
    /// `1.0` or `-1.0`.
    pub fn factor(self) -> f64 {
        match self {
            Self::Positive => 1.0,
            Self::Negative => -1.0,
        }
    }
}

/// Default signs for H2..=H9: H3 negative (compressive), the others
/// positive.
pub const HARMONICS_DEFAULT_SIGNS: [HarmonicSign; HARMONICS_ORDERS] = {
    let mut s = [HarmonicSign::Positive; HARMONICS_ORDERS];
    s[1] = HarmonicSign::Negative;
    s
};

/// The harmonics stage: a list of layers, added together (parallel sum,
/// at its natural gain) or run one after the other
/// ([`HarmonicsRouting::Series`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The layers, in order. Disabled layers are kept but not applied.
    #[serde(default)]
    pub layers: Vec<HarmonicsLayer>,
    /// Remove DC offset introduced by even-order harmonics.
    #[serde(default = "yes")]
    pub dc_block: bool,
    /// How the layers combine (absent: parallel).
    #[serde(default, skip_serializing_if = "HarmonicsRouting::is_parallel")]
    #[cfg_attr(feature = "ts", ts(as = "Option<HarmonicsRouting>", optional))]
    pub routing: HarmonicsRouting,
    /// The system preset ([`HarmonicsSystem::id`]) the stage was started
    /// from, for the editor to compare against (absent: built by hand). The
    /// engine ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub system: Option<String>,
}

/// How the harmonics stage combines its layers.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum HarmonicsRouting {
    /// Every layer sees the input; their distortions are added (one shared
    /// sag/bias supply).
    #[default]
    Parallel,
    /// The active layers one after the other, in list order: each layer is
    /// its own device (its own calibration, supply and DC blocker) and feeds
    /// the next at the level it puts out. The cross terms of heavy settings
    /// count. Where an earlier layer's output goes past full scale, a later
    /// layer shapes the part within it and passes the excess unshaped.
    /// The old `series_makeup` (series with the output trims on the last
    /// layer) reads as this.
    #[serde(alias = "series_makeup")]
    Series,
}

impl HarmonicsRouting {
    pub fn is_parallel(&self) -> bool {
        *self == Self::Parallel
    }

    pub fn is_series(&self) -> bool {
        *self == Self::Series
    }
}

impl Default for HarmonicsConfig {
    /// One layer from the `se_triode_high_mu` preset (audible at programme
    /// levels; the hand-picked `triode` is near inaudible there).
    fn default() -> Self {
        Self::from_preset("se_triode_high_mu").expect("the se_triode_high_mu preset exists")
    }
}

impl HarmonicsConfig {
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// A one-layer config from the preset `id`.
    pub fn from_preset(id: &str) -> Option<Self> {
        Some(Self {
            system: None,
            enabled: true,
            layers: vec![harmonics_preset(id)?.to_layer()],
            dc_block: true,
            routing: HarmonicsRouting::Parallel,
        })
    }

    /// The layers that are applied.
    pub fn active_layers(&self) -> impl Iterator<Item = &HarmonicsLayer> {
        self.layers.iter().filter(|l| l.enabled())
    }
}

/// One layer of the harmonics stage. Every parameter is editable; `preset`
/// names the preset it started from (compare with that preset's layer to
/// mark it modified, copy it back to reset).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarmonicsLayer {
    /// Signed Chebyshev terms H2..=H9 (a static curve).
    Chebyshev(ChebyshevLayer),
    /// Core saturation (transformer, tape): low-shelf boost, odd saturator,
    /// exact inverse shelf. H3 rises toward low frequencies and with level.
    Flux(FluxLayer),
    /// Tape magnetic hysteresis (Jiles-Atherton ODE): level-dependent loop
    /// saturation with pinning and remanence.
    Hysteresis(HysteresisLayer),
}

impl HarmonicsLayer {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Chebyshev(l) => l.enabled,
            Self::Flux(l) => l.enabled,
            Self::Hysteresis(l) => l.enabled,
        }
    }

    /// How much of the layer is applied, 0..=1 (flux layers have no amount:
    /// 1).
    pub fn amount(&self) -> f64 {
        match self {
            Self::Chebyshev(l) => l.amount,
            Self::Flux(_) => 1.0,
            Self::Hysteresis(l) => l.amount,
        }
    }

    /// The layer's amount, for the kinds that have one.
    pub fn amount_mut(&mut self) -> Option<&mut f64> {
        match self {
            Self::Chebyshev(l) => Some(&mut l.amount),
            Self::Flux(_) => None,
            Self::Hysteresis(l) => Some(&mut l.amount),
        }
    }

    /// The preset this layer started from.
    pub fn preset(&self) -> Option<&str> {
        match self {
            Self::Chebyshev(l) => l.preset.as_deref(),
            Self::Flux(l) => l.preset.as_deref(),
            Self::Hysteresis(l) => l.preset.as_deref(),
        }
    }

    /// The same layer with `enabled` and `preset` cleared (and a flat tilt,
    /// sag or bias at depth 0 dropped): its sound.
    pub fn params(&self) -> Self {
        let mut l = self.clone();
        match &mut l {
            Self::Chebyshev(c) => {
                c.enabled = true;
                c.preset = None;
                // A flat tilt sounds as no tilt.
                if c.tilt.as_ref().is_some_and(|t| t.is_flat()) {
                    c.tilt = None;
                }
                if let Some(t) = c.tilt.as_deref_mut() {
                    t.omit_silent_slopes();
                }
                // So do sag and bias at depth 0.
                if c.sag.is_some_and(|s| !s.is_on()) {
                    c.sag = None;
                }
                if c.bias.is_some_and(|b| !b.is_on()) {
                    c.bias = None;
                }
            }
            Self::Flux(f) => {
                f.enabled = true;
                f.preset = None;
            }
            Self::Hysteresis(h) => {
                h.enabled = true;
                h.preset = None;
                // An EQ that is off and a zero offset sound as none.
                if h.bass_eq.is_some_and(|e| !e.is_on()) {
                    h.bass_eq = None;
                }
                if h.asymmetry == Some(0.0) {
                    h.asymmetry = None;
                }
            }
        }
        l
    }

    /// Whether the layer differs from the preset it names (`None`: no
    /// preset, or one this build does not know).
    pub fn is_modified(&self) -> Option<bool> {
        let preset = harmonics_preset(self.preset()?)?;
        Some(preset.layer.params() != self.params())
    }
}

/// A Chebyshev layer: the static curve `x + Σ a_k (T_k(x) − T_k(0))` with
/// `|a_k|` the levels and the signs, read at the drive's operating point
/// (renormalised so a full-scale sine keeps a unit fundamental), its terms
/// scaled by `amount`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ChebyshevLayer {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Preset this layer started from (`null`: built by hand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub preset: Option<String>,
    /// Levels of H2..=H9 relative to the fundamental for a sine at the
    /// reference point (drive 0 dB, amount 1: a 0 dBFS sine), dB
    /// (-120..=-20); `null` = that order off.
    pub levels_db: [Option<f64>; HARMONICS_ORDERS],
    /// Sign of each order's term, H2..=H9.
    #[serde(default = "default_harmonic_signs")]
    pub signs: [HarmonicSign; HARMONICS_ORDERS],
    /// Flip every even order's sign (which half-wave is flattened).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub invert_even: bool,
    /// Where a 0 dBFS sine sits on the curve, dB below the reference
    /// (-40..=0): lower drive moves down the curve, H_k falling by about
    /// (k − 1) dB per dB.
    #[serde(default)]
    pub drive_db: f64,
    /// Scale of the layer's distortion terms (0..=1).
    #[serde(default = "one")]
    pub amount: f64,
    /// Harmonic levels that change with frequency (`null`: flat, the
    /// levels hold at every frequency).
    // Boxed: the per-order overrides make it large.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub tilt: Option<Box<HarmonicsTilt>>,
    /// Supply sag: loud passages lower the drive in front of the curve
    /// (`null` or depth 0: off; applied at `depth_db × amount`). The stage's
    /// sag layers share one supply: the dry signal is turned down once, by
    /// the deepest sag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub sag: Option<HarmonicsSag>,
    /// Bias shift: sustained loud passages move the operating point on the
    /// curve, shifting the even/odd balance (`null` or depth 0: off;
    /// applied at `depth × amount`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub bias: Option<HarmonicsBias>,
}

/// Supply sag of a Chebyshev layer: an envelope of `|x|` (attack, release)
/// lowers the drive by up to `depth_db` as it rises to full scale. Not
/// renormalised: loud passages are gently compressed and lean toward the
/// low orders.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsSag {
    /// Drive drop at a full envelope, dB (0..=6; 0 = off).
    pub depth_db: f64,
    /// Envelope attack and release time constants, ms (0.1..=10000).
    pub attack_ms: f64,
    pub release_ms: f64,
}

impl Default for HarmonicsSag {
    fn default() -> Self {
        Self {
            depth_db: 1.0,
            attack_ms: 20.0,
            release_ms: 200.0,
        }
    }
}

/// Bias shift of a Chebyshev layer: a slower envelope of `|x|` moves the
/// operating point to `depth × envelope`; the curve is read around it
/// (its value and slope there removed), so only the even/odd balance
/// changes, never the level. The reach can be cut short where strong
/// high-order curves would raise the peak gain too much (`bias_limited` in the
/// readout).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsBias {
    /// Operating point at a full envelope, fraction of full scale
    /// (-0.25..=0.25; 0 = off; the sign picks the side).
    pub depth: f64,
    /// Envelope attack and release time constants, ms (0.1..=10000).
    pub attack_ms: f64,
    pub release_ms: f64,
}

impl Default for HarmonicsBias {
    fn default() -> Self {
        Self {
            depth: 0.1,
            attack_ms: 300.0,
            release_ms: 2_000.0,
        }
    }
}

/// Check one envelope's time constants; `Err` names the field.
fn check_envelope(name: &str, attack_ms: f64, release_ms: f64) -> Result<(), String> {
    for (field, ms) in [("attack_ms", attack_ms), ("release_ms", release_ms)] {
        if !ms.is_finite() || !(HARMONICS_ENVELOPE_MIN_MS..=HARMONICS_ENVELOPE_MAX_MS).contains(&ms)
        {
            return Err(format!(
                "{name}.{field} must be between {HARMONICS_ENVELOPE_MIN_MS} and {HARMONICS_ENVELOPE_MAX_MS} ms (got {ms})"
            ));
        }
    }
    Ok(())
}

impl HarmonicsSag {
    /// Whether it changes the sound (depth above 0).
    pub fn is_on(&self) -> bool {
        self.depth_db > 0.0
    }

    /// Check its ranges; `Err` names the field.
    pub fn validate(&self) -> Result<(), String> {
        let d = self.depth_db;
        if !d.is_finite() || !(0.0..=HARMONICS_SAG_MAX_DB).contains(&d) {
            return Err(format!(
                "sag.depth_db must be between 0 and {HARMONICS_SAG_MAX_DB} dB (got {d})"
            ));
        }
        check_envelope("sag", self.attack_ms, self.release_ms)
    }
}

impl HarmonicsBias {
    /// Whether it changes the sound (depth not 0).
    pub fn is_on(&self) -> bool {
        self.depth != 0.0
    }

    /// Check its ranges; `Err` names the field.
    pub fn validate(&self) -> Result<(), String> {
        let d = self.depth;
        if !d.is_finite() || d.abs() > HARMONICS_BIAS_MAX_DEPTH {
            return Err(format!(
                "bias.depth must be between -{HARMONICS_BIAS_MAX_DEPTH} and {HARMONICS_BIAS_MAX_DEPTH} (got {d})"
            ));
        }
        check_envelope("bias", self.attack_ms, self.release_ms)
    }
}

impl ChebyshevLayer {
    /// Whether sag or bias is set (a depth other than 0), whatever the
    /// amount.
    pub fn has_sag_or_bias(&self) -> bool {
        self.sag.is_some_and(|s| s.is_on()) || self.bias.is_some_and(|b| b.is_on())
    }

    /// Whether sag or bias is set and the amount is above 0 (both depths
    /// scale with it): the layer's operating point moves, so it runs on its
    /// own instead of merging into the stage's static table.
    pub fn is_dynamic(&self) -> bool {
        self.amount > 0.0 && self.has_sag_or_bias()
    }

    /// The signs with `invert_even` applied.
    pub fn effective_signs(&self) -> [HarmonicSign; HARMONICS_ORDERS] {
        let mut s = self.signs;
        if self.invert_even {
            for sign in s.iter_mut().step_by(2) {
                *sign = match *sign {
                    HarmonicSign::Positive => HarmonicSign::Negative,
                    HarmonicSign::Negative => HarmonicSign::Positive,
                };
            }
        }
        s
    }
}

/// One shelf of a [`HarmonicsTilt`]: a corner and how far the harmonics
/// rise (or, negative, fall) beyond it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TiltShelf {
    /// Corner, Hz, as a fundamental frequency: harmonic `k`'s filter has
    /// its corner at `k × hz`.
    pub hz: f64,
    /// Rise beyond the corner, dB (-24..=24; 0 = no shelf).
    pub db: f64,
    /// Slope, dB/oct: 6, 12, 18 or 24, the shelf's order times 6 (omitted:
    /// 12 for a low shelf, 6 for a high one). It is the asymptote: a shelf
    /// of order `n` is the first-order curve squeezed `n` times closer to
    /// its corner, so its steepest slope is `slope × (R − 1)/(R + 1)`,
    /// `R = 10^{|db|/20}`: a small rise only rises over fewer octaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub slope: Option<u8>,
}

impl TiltShelf {
    /// A shelf at the default slope.
    pub fn new(hz: f64, db: f64) -> Self {
        Self {
            hz,
            db,
            slope: None,
        }
    }

    /// The shelf's order (1..=4): its slope (`default` when omitted) over 6
    /// dB/oct, clamped (validation rejects any other slope).
    pub fn order(&self, default: u8) -> usize {
        usize::from(self.slope.unwrap_or(default) / 6).clamp(1, 4)
    }
}

/// A per-order override of a [`HarmonicsTilt`]: both shelves for one order
/// (the low one applies whatever the order's parity).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OrderTilt {
    pub low: TiltShelf,
    pub high: TiltShelf,
}

/// How a Chebyshev layer's harmonic levels change with frequency. Each
/// order's distortion term goes through its own filter `G_k`: a low shelf
/// and a high shelf (12 and 6 dB/oct unless their `slope` says otherwise),
/// unity at `k × 1 kHz`, so harmonic
/// `k` of a 0 dBFS sine at `f` is `h_k + 20 log10 |G_k(k f)|` dBc (relative
/// to the input level). The layer's levels hold at 1 kHz. The terms carry no
/// linear part, so small signals keep a flat response.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsTilt {
    /// Rise toward low frequencies (`hz` 20..=2000): `slope` (default 12)
    /// dB/oct below the corner until it reaches `db`. Odd orders only,
    /// unless `low_even`.
    pub low: TiltShelf,
    /// Rise toward high frequencies (`hz` 200..=20000): `slope` (default 6)
    /// dB/oct above the corner until it reaches `db`. Every order.
    pub high: TiltShelf,
    /// The low shelf applies to the even orders too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub low_even: bool,
    /// Per-order overrides, H2..=H9 (`null`: the layer's shelves).
    #[serde(default, skip_serializing_if = "HarmonicsTilt::no_overrides")]
    #[cfg_attr(
        feature = "ts",
        ts(as = "Option<[Option<OrderTilt>; HARMONICS_ORDERS]>", optional)
    )]
    pub orders: [Option<OrderTilt>; HARMONICS_ORDERS],
}

impl Default for HarmonicsTilt {
    /// Flat: both shelves at 0 dB (low corner 100 Hz, high corner 2 kHz).
    fn default() -> Self {
        Self {
            low: TiltShelf::new(100.0, 0.0),
            high: TiltShelf::new(2_000.0, 0.0),
            low_even: false,
            orders: [None; HARMONICS_ORDERS],
        }
    }
}

impl HarmonicsTilt {
    fn no_overrides(orders: &[Option<OrderTilt>; HARMONICS_ORDERS]) -> bool {
        orders.iter().all(Option::is_none)
    }

    /// The shelves of order `k` (2..=9): its override, else the layer's
    /// (the low shelf only on odd orders unless `low_even`).
    pub fn order_shelves(&self, k: usize) -> OrderTilt {
        if let Some(o) = self.orders.get(k.wrapping_sub(2)).copied().flatten() {
            return o;
        }
        let low_db = if k % 2 == 1 || self.low_even {
            self.low.db
        } else {
            0.0
        };
        OrderTilt {
            low: TiltShelf {
                db: low_db,
                ..self.low
            },
            high: self.high,
        }
    }

    /// Drop every `slope` that changes nothing (at its shelf's default, or
    /// on a shelf at 0 dB).
    pub fn omit_silent_slopes(&mut self) {
        let omit = |s: &mut TiltShelf, default: u8| {
            if s.db == 0.0 || s.slope == Some(default) {
                s.slope = None;
            }
        };
        omit(&mut self.low, TILT_LOW_DEFAULT_SLOPE);
        omit(&mut self.high, TILT_HIGH_DEFAULT_SLOPE);
        for o in self.orders.iter_mut().flatten() {
            omit(&mut o.low, TILT_LOW_DEFAULT_SLOPE);
            omit(&mut o.high, TILT_HIGH_DEFAULT_SLOPE);
        }
    }

    /// Whether every order's filter is unity (both shelves at 0 dB).
    pub fn is_flat(&self) -> bool {
        (2..=HARMONICS_MAX_ORDER).all(|k| {
            let o = self.order_shelves(k);
            o.low.db == 0.0 && o.high.db == 0.0
        })
    }

    /// Check every shelf's range; `Err` names the field.
    pub fn validate(&self) -> Result<(), String> {
        let check = |name: &str, s: &TiltShelf, lo: f64, hi: f64| {
            if !s.hz.is_finite() || !(lo..=hi).contains(&s.hz) {
                return Err(format!(
                    "{name}.hz must be between {lo} and {hi} Hz (got {})",
                    s.hz
                ));
            }
            if !s.db.is_finite() || s.db.abs() > TILT_MAX_DB {
                return Err(format!(
                    "{name}.db must be between -{TILT_MAX_DB} and {TILT_MAX_DB} dB (got {})",
                    s.db
                ));
            }
            if let Some(v) = s.slope
                && !TILT_SLOPES.contains(&v)
            {
                return Err(format!(
                    "{name}.slope must be 6, 12, 18 or 24 dB/oct (got {v})"
                ));
            }
            Ok(())
        };
        check("tilt.low", &self.low, TILT_LOW_MIN_HZ, TILT_LOW_MAX_HZ)?;
        check("tilt.high", &self.high, TILT_HIGH_MIN_HZ, TILT_HIGH_MAX_HZ)?;
        for (i, o) in self.orders.iter().enumerate() {
            if let Some(o) = o {
                let name = format!("tilt.orders[{i}] (H{})", i + 2);
                check(
                    &format!("{name}.low"),
                    &o.low,
                    TILT_LOW_MIN_HZ,
                    TILT_LOW_MAX_HZ,
                )?;
                check(
                    &format!("{name}.high"),
                    &o.high,
                    TILT_HIGH_MIN_HZ,
                    TILT_HIGH_MAX_HZ,
                )?;
            }
        }
        Ok(())
    }
}

/// A flux (core saturation) layer, calibrated from one number: H3 in dBc
/// for a 0 dBFS sine at `calibration_hz`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FluxLayer {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Preset this layer started from (`null`: built by hand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub preset: Option<String>,
    /// Low-shelf corner, Hz (1..=5000): the boost starts below it.
    pub corner_hz: f64,
    /// Low-frequency boost of the shelf, dB (0..=40); the boost flattens
    /// out below `corner_hz / 10^(boost/20)`.
    pub max_boost_db: f64,
    /// H3 for a 0 dBFS sine at `calibration_hz`, dBc (-120..=-12).
    pub target_h3_db: f64,
    /// Calibration frequency, Hz (5..=1000).
    pub calibration_hz: f64,
}

/// A tape magnetic hysteresis layer (Jiles-Atherton model), calibrated from
/// H3 in dBc for a 0 dBFS sine at `calibration_hz` (at `amount = 1`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HysteresisLayer {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Preset this layer started from (`null`: built by hand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub preset: Option<String>,
    /// Saturation character (0..=1): lower emphasizes Rayleigh pinning across
    /// levels; higher pushes deeper into the soft Langevin knee near 0 dBFS.
    pub saturation: f64,
    /// Hysteresis loop width (0..=1): 0 is anhysteretic Langevin saturation;
    /// 1 gives a wide magnetic loop.
    pub width: f64,
    /// Scale of the layer's distortion residual (0..=1).
    #[serde(default = "one")]
    pub amount: f64,
    /// H3 for a 0 dBFS sine at `calibration_hz` (at `amount = 1`), dBc
    /// (-120..=-12).
    pub target_h3_db: f64,
    /// Calibration frequency, Hz (20..=5000).
    pub calibration_hz: f64,
    /// Record/playback EQ pair around the core: a low-shelf boost into the
    /// magnetic core and its exact inverse after it, so the music's
    /// response stays flat and only the distortion rises toward the bass
    /// (`null`, disabled or 0 dB: off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub bass_eq: Option<HysteresisBassEq>,
    /// DC offset of the core's field, in units of its saturation knee
    /// (-1..=1, whatever the drive; `null` or 0: symmetric, no even orders).
    /// Adds H2 (and the other even orders) with a phase that holds across
    /// levels; the sign flips its polarity. The drive is calibrated without
    /// it, so the H3 target holds for the symmetric core (the offset moves
    /// H3 a little at large values); an offset that makes the layer expand
    /// is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub asymmetry: Option<f64>,
}

/// The bass EQ pair of a hysteresis layer: a 12 dB/oct low shelf (unity at
/// 1 kHz) in front of the core and its exact inverse after it. Harmonic
/// `k` of a tone at `f` rises by about `(k-th order's level slope + 1) ×`
/// the boost at `f`, minus the boost at `k f`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HysteresisBassEq {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Shelf corner, Hz (20..=500): the boost starts below it.
    pub hz: f64,
    /// Boost into the core below the corner, dB (0..=12; 0 = off).
    pub db: f64,
}

impl Default for HysteresisBassEq {
    fn default() -> Self {
        Self {
            enabled: true,
            hz: 120.0,
            db: 3.0,
        }
    }
}

impl HysteresisBassEq {
    /// Whether it changes the sound.
    pub fn is_on(&self) -> bool {
        self.enabled && self.db != 0.0
    }

    /// Check its ranges; `Err` names the field.
    pub fn validate(&self) -> Result<(), String> {
        if !self.hz.is_finite() || !(HYSTERESIS_MIN_EQ_HZ..=HYSTERESIS_MAX_EQ_HZ).contains(&self.hz)
        {
            return Err(format!(
                "bass_eq.hz must be between {HYSTERESIS_MIN_EQ_HZ} and {HYSTERESIS_MAX_EQ_HZ} Hz (got {})",
                self.hz
            ));
        }
        if !self.db.is_finite() || !(0.0..=HYSTERESIS_MAX_EQ_DB).contains(&self.db) {
            return Err(format!(
                "bass_eq.db must be between 0 and {HYSTERESIS_MAX_EQ_DB} dB (got {})",
                self.db
            ));
        }
        Ok(())
    }
}

impl HysteresisLayer {
    /// The bass EQ pair when it is on.
    pub fn active_bass_eq(&self) -> Option<HysteresisBassEq> {
        self.bass_eq.filter(HysteresisBassEq::is_on)
    }

    /// The field offset (0 when unset).
    pub fn asymmetry(&self) -> f64 {
        self.asymmetry.unwrap_or(0.0)
    }
}

/// A preset: one layer's full parameters, plus where they come from.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsPreset {
    /// Stable id, stored in a layer's `preset`.
    pub id: String,
    /// Display name (a generic archetype, never a product).
    pub name: String,
    /// `hand_picked`, `computed` (from device equations) or `archetype`.
    pub origin: String,
    /// What 0 dBFS and drive 0 dB stand for on the device.
    pub reference: String,
    /// Where the numbers come from.
    pub source: String,
    /// The layer (its `preset` field is left out here).
    pub layer: HarmonicsLayer,
}

impl HarmonicsPreset {
    /// The preset's layer, tagged with this preset's id.
    pub fn to_layer(&self) -> HarmonicsLayer {
        let mut l = self.layer.params();
        match &mut l {
            HarmonicsLayer::Chebyshev(c) => c.preset = Some(self.id.clone()),
            HarmonicsLayer::Flux(f) => f.preset = Some(self.id.clone()),
            HarmonicsLayer::Hysteresis(h) => h.preset = Some(self.id.clone()),
        }
        l
    }
}

/// The built-in presets (`harmonics_presets.json`).
pub fn harmonics_presets() -> &'static [HarmonicsPreset] {
    static PRESETS: std::sync::OnceLock<Vec<HarmonicsPreset>> = std::sync::OnceLock::new();
    PRESETS.get_or_init(|| {
        serde_json::from_str(include_str!("harmonics_presets.json"))
            .expect("harmonics_presets.json parses")
    })
}

/// The built-in preset `id`.
pub fn harmonics_preset(id: &str) -> Option<&'static HarmonicsPreset> {
    harmonics_presets().iter().find(|p| p.id == id)
}

/// A system preset: a whole harmonics stage standing for a typical piece of
/// gear (an archetype, never a product). Each layer is an ordinary layer
/// tagged with the layer preset it started from, some with settings
/// changed, so the editor marks those modified and can reset them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsSystem {
    /// Stable id.
    pub id: String,
    /// Display name (a generic archetype, never a product).
    pub name: String,
    /// What the gear is: the chain the layers stand for.
    pub gear: String,
    /// What it sounds like.
    pub sound: String,
    /// How the layers combine (absent: parallel).
    #[serde(default, skip_serializing_if = "HarmonicsRouting::is_parallel")]
    #[cfg_attr(feature = "ts", ts(as = "Option<HarmonicsRouting>", optional))]
    pub routing: HarmonicsRouting,
    /// The layers, in order.
    pub layers: Vec<HarmonicsLayer>,
}

impl HarmonicsSystem {
    /// The whole stage this preset stands for (on, DC blocker on).
    pub fn to_config(&self) -> HarmonicsConfig {
        HarmonicsConfig {
            enabled: true,
            layers: self.layers.clone(),
            dc_block: true,
            routing: self.routing,
            system: Some(self.id.clone()),
        }
    }
}

/// The built-in system presets (`harmonics_systems.json`).
pub fn harmonics_systems() -> &'static [HarmonicsSystem] {
    static SYSTEMS: std::sync::OnceLock<Vec<HarmonicsSystem>> = std::sync::OnceLock::new();
    SYSTEMS.get_or_init(|| {
        serde_json::from_str(include_str!("harmonics_systems.json"))
            .expect("harmonics_systems.json parses")
    })
}

/// The built-in system preset `id`.
pub fn harmonics_system(id: &str) -> Option<&'static HarmonicsSystem> {
    harmonics_systems().iter().find(|s| s.id == id)
}

fn default_harmonic_signs() -> [HarmonicSign; HARMONICS_ORDERS] {
    HARMONICS_DEFAULT_SIGNS
}

fn one() -> f64 {
    1.0
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
    fn empty_chain_is_passthrough() {
        assert!(ChainConfig::neutral().is_passthrough());
        assert!(!ChainConfig::default().is_passthrough());
        let mut no_cut = ChainConfig::neutral();
        no_cut.output.headroom_db = 0.5;
        assert!(!no_cut.is_passthrough());
    }
}
