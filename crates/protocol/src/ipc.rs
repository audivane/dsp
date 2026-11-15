//! Control-plane messages between the daemon and its clients (CLI, UI).
//!
//! Transport: newline-delimited JSON over a Unix socket at
//! `$XDG_RUNTIME_DIR/audivane-dsp.sock` (override: `AUDIVANE_DSP_SOCKET`).
//! One JSON object per line. Requests carry an `id`; responses echo it.
//! After `subscribe`, the server also pushes [`Event`] lines (no `id`) to that
//! connection.
//!
//! Wire examples:
//! ```text
//! → {"id":1,"method":"status"}
//! ← {"id":1,"ok":{...Status...}}
//! → {"id":2,"method":"ab_select","params":{"side":"b","crossfade_ms":10.0}}
//! ← {"id":2,"error":{"code":-32602,"message":"A/B is not configured"}}
//! ← {"event":"meters","data":{...}}
//! ```

use crate::{ChainConfig, ConvolutionConfig, EqProfile, SampleFormat, UpsamplerConfig};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- envelope

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Request {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
// `SetConfig` carries a whole config; requests are rare and short-lived, so
// boxing it would only complicate the wire type for no measurable gain.
#[allow(clippy::large_enum_variant)]
pub enum Call {
    /// → [`Status`]
    Status,
    /// → [`EngineConfig`]
    GetConfig,
    /// Replace the whole config (validated, persisted, applied). → `null`
    SetConfig(EngineConfig),
    /// Set or clear the EQ of chain A. → `null`
    SetEq(Option<EqProfile>),
    /// → `null`
    SetConvolution(
        #[serde(deserialize_with = "crate::deserialize_conv_list")] Vec<ConvolutionConfig>,
    ),
    /// → `null`
    SetUpsampler(UpsamplerConfig),
    /// Turn all DSP off/on without losing the settings. → `null`
    SetDspEnabled { enabled: bool },
    /// Configure chain B for A/B (None disables A/B).
    ///
    /// Chain B's `upsampler` and `output` always follow chain A; differing
    /// values are replaced (never rejected). → [`AbStatus`], plus an optional
    /// `"notes"` array of strings when chain B was realigned with chain A.
    SetAb(Option<AbConfig>),
    /// → [`AbStatus`]
    AbSelect {
        side: AbSide,
        #[serde(default = "default_fade")]
        crossfade_ms: f64,
    },
    /// Set output volume, balance, and mute. → [`VolumeStatus`]
    SetVolume {
        volume_db: f64,
        balance_db: f64,
        muted: bool,
    },
    /// → `Vec<StreamInfo>`
    ListStreams,
    /// → `Vec<PresetInfo>` (sorted by name)
    ListPresets,
    /// → [`Preset`]
    GetPreset { name: String },
    /// Save chain A as a preset and make it active. With `overwrite: false` an
    /// existing preset is refused. → [`PresetInfo`]
    SavePreset {
        name: String,
        #[serde(default)]
        overwrite: bool,
    },
    /// Validate a preset, apply its chain to chain A and make it active.
    /// → `null`
    LoadPreset { name: String },
    /// → `null`
    DeletePreset { name: String },
    /// → [`PresetInfo`]
    RenamePreset { from: String, to: String },
    /// Parse Equalizer APO / AutoEQ text into a new preset (EQ only, flat
    /// defaults for convolution, upsampler, and output). Does not load it.
    /// → [`PresetInfo`]
    ImportApo { name: String, text: String },
    /// → `Vec<DeviceInfo>` (from `/proc/asound`; nothing is opened)
    ListDevices,
    /// Move one stream (by PipeWire node id) or every stream of an app. → `null`
    Route {
        target: RouteTarget,
        sink: SinkChoice,
    },
    /// → `null`
    SetRoutingMode(RoutingMode),
    /// → `Option<NowPlaying>`
    NowPlaying,
    /// Play a known signal through the Direct path and verify the bytes handed
    /// to ALSA. → [`SelfTestResult`]
    SelfTest,
    /// Measure the response of the engine's own upsampler for a design.
    ///
    /// Defaults: chain A's upsampler config and the current graph rate (else
    /// 44100 Hz). Runs off the control/mutation lock; long kernels take a
    /// moment. → [`UpsamplerResponse`]
    UpsamplerResponse {
        #[serde(default)]
        config: Option<UpsamplerConfig>,
        #[serde(default)]
        source_rate: Option<u32>,
        /// Also evaluate this frequency window directly from the measured
        /// impulse (zoom). Reuses the cached measurement.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<ResponseWindowRequest>,
    },
    /// Meters once (the same snapshot the `meters` event carries). → [`Meters`]
    Meters,
    /// Give the DAC back to WirePlumber (engine idles). → `null`
    ReleaseDevice,
    /// Take the DAC again. → `null`
    AcquireDevice,
    /// Start receiving events on this connection. → `null`
    Subscribe { topics: Vec<Topic> },
    /// Stop the daemon. → `null`
    Shutdown,
}

fn default_fade() -> f64 {
    10.0
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Response {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: u64,
    #[serde(flatten)]
    pub result: Outcome,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok(serde_json::Value),
    Error(RpcError),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub const PARSE: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL: i32 = -32603;
    /// The engine cannot do this in its current state (e.g. device released).
    pub const BAD_STATE: i32 = 1;
    /// Device/ALSA/PipeWire failure.
    pub const IO: i32 = 2;
}

/// Server push, only after `subscribe`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
// `Config` carries a whole `EngineConfig`; event values are short-lived and
// serialized immediately, so a box would only complicate the wire type.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Meters(Meters),
    Status(Status),
    Streams(Vec<StreamInfo>),
    NowPlaying(Option<NowPlaying>),
    /// The whole config, after any change from any client (or a file load).
    Config(EngineConfig),
    /// The preset list, after a create/delete/rename/import/save.
    Presets(Vec<PresetInfo>),
    /// A live spectrum frame, only while someone subscribes to
    /// [`Topic::Spectrum`].
    Spectrum(Spectrum),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// ~30 Hz.
    Meters,
    /// On change.
    Status,
    Streams,
    NowPlaying,
    /// The whole config, on change.
    Config,
    /// The preset list, on change.
    Presets,
    /// Live spectrum frames (~10-20 Hz, only computed while subscribed).
    Spectrum,
}

/// Live FFT spectrum of the pre (chain input at the graph rate) and post
/// (chain output at the output rate) taps. `post` is taken from the
/// quantizer's left-justified output scaled back to `f32`, which is within
/// half an LSB of the value just before quantization.
///
/// Each tap runs two Hann FFTs: 8192 points (50% overlap) for every bin and
/// 32768 points (87.5% overlap) for the low band, where a 1/24-octave bin is
/// narrower than four 8192-point bins (below about 800 Hz at 48 kHz). Power
/// is averaged over the frames since the previous report. Each tap is mapped
/// onto its own fixed log grid: bin `k` is centred on
/// `f_min_hz * 2^(k / bins_per_octave)` (half a step either side), centres
/// from [`Self::f_min_hz`] to `min(20 kHz, Nyquist)`.
///
/// Values are band power in dB, channel-mean, rounded to 0.1 dB: 0 dB = a
/// full-scale sine. A bin reads the signal power inside it (the interpolated
/// power density integrated over the bin), or a sine's interpolated peak
/// level where the bin holds a spectral peak, whichever is higher. A sine
/// therefore reads its level in one bin and noise reads its power per bin
/// (white noise rises 3 dB/octave, pink noise is flat), independent of FFT
/// size.
///
/// There are no events while the engine is idle (fewer than one complete
/// 8192-sample frame has arrived), and the last frame is kept by clients.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Spectrum {
    pub f_min_hz: f64,
    pub bins_per_octave: u32,
    pub pre: Vec<f32>,
    pub post: Vec<f32>,
    /// Graph (source) rate.
    pub rate: u32,
}

/// Measured response of the engine's own upsampler.
///
/// Built by pushing a full-scale unit impulse through a real `Upsampler` for
/// the design, flushing only the impulse tail (kernel length plus block
/// latency at the output rate), and FFTing it with the next power of two at or
/// above that length (at most 2x zero padding).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UpsamplerResponse {
    pub source_rate: u32,
    pub output_rate: u32,
    pub ratio: u32,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub taps: usize,
    pub phase: crate::FilterPhase,
    /// Cutoff as a fraction of the source Nyquist actually used for the
    /// filter (`1.0` when upsampling is off).
    pub effective_cutoff: f64,
    /// 0 .. output Nyquist: ~600 log points plus dense linear points from
    /// 0.8x to 1.25x source Nyquist (sorted, deduplicated).
    pub freqs_hz: Vec<f64>,
    pub magnitude_db: Vec<f64>,
    /// Impulse response at the output rate around its peak (at most 256
    /// samples); [`Self::impulse_peak`] is the peak index within this slice.
    pub impulse: Vec<f64>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub impulse_peak: usize,
    /// Max-min magnitude below 0.9x source Nyquist.
    pub passband_ripple_db: f64,
    /// First frequency where the response has fallen to -0.1 dB.
    pub minus_0_1_db_hz: f64,
    /// First frequency where the response has fallen to -3 dB.
    pub minus_3_db_hz: f64,
    /// First frequency at or above the source Nyquist after which every
    /// response point stays at or below the design's target attenuation
    /// (derived from `kaiser_beta`). The transition band between Nyquist and
    /// this point may be above that level.
    pub stopband_start_hz: f64,
    /// Worst level from [`Self::stopband_start_hz`] to the output Nyquist,
    /// as a positive attenuation in dB (at least the target attenuation).
    pub stopband_atten_db: f64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub latency_samples: usize,
    /// Precise threshold crossings ([`RESPONSE_CROSSING_LEVELS_DB`]): for
    /// each level, the first frequency from 0 Hz where the response falls to
    /// it, found on the FFT grid and refined by bisection on the direct
    /// evaluation to under 0.001 Hz. Levels never reached are left out;
    /// empty when upsampling is off.
    #[serde(default)]
    pub crossings: Vec<ResponseCrossing>,
    /// The requested zoom window, evaluated directly from the measured
    /// impulse (a DFT at each frequency, no interpolation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<ResponseWindow>,
}

/// Levels reported in [`UpsamplerResponse::crossings`].
pub const RESPONSE_CROSSING_LEVELS_DB: [f64; 13] = [
    -0.0001, -0.001, -0.01, -0.1, -0.5, -1.0, -3.0, -6.0, -20.0, -60.0, -100.0, -120.0, -150.0,
];

/// Most points one [`ResponseWindowRequest`] may ask for.
pub const RESPONSE_WINDOW_MAX_POINTS: u32 = 4096;

/// Zoom window for [`Call::UpsamplerResponse`]: `points` frequencies evenly
/// spaced from `f_lo_hz` to `f_hi_hz` inclusive. Both must be finite, within
/// 0 .. output Nyquist, and `f_lo_hz < f_hi_hz`. `points` is clamped to
/// 2 ..= [`RESPONSE_WINDOW_MAX_POINTS`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ResponseWindowRequest {
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    pub points: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ResponseWindow {
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    pub freqs_hz: Vec<f64>,
    pub magnitude_db: Vec<f64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ResponseCrossing {
    pub level_db: f64,
    pub freq_hz: f64,
}

// ---------------------------------------------------------------- presets

/// On-disk preset envelope: `{ "version": 1, "name": …, "chain": … }`.
///
/// A preset holds chain A's signal path (EQ incl. L/R, convolution,
/// upsampling, output). Device, routing, rate policy and A/B are machine
/// settings and never part of a preset.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Preset {
    #[serde(default = "preset_version")]
    pub version: u32,
    pub name: String,
    pub chain: ChainConfig,
}

fn preset_version() -> u32 {
    1
}

impl Preset {
    pub fn new(name: impl Into<String>, chain: ChainConfig) -> Self {
        Self {
            version: preset_version(),
            name: name.into(),
            chain,
        }
    }
}

/// One entry of [`Call::ListPresets`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PresetInfo {
    pub name: String,
    /// File modification time, unix seconds (0 when unknown).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub modified_unix: u64,
    /// Number of EQ bands in the preset.
    pub bands: u32,
    /// Number of convolution impulse responses in the preset.
    pub convolution: u32,
}

/// The active preset, as reported by [`Status::preset`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PresetState {
    pub name: String,
    /// Chain A differs from the saved preset's chain.
    pub modified: bool,
}

// ---------------------------------------------------------------- devices

/// One ALSA playback device, read from `/proc/asound` (never opened).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DeviceInfo {
    /// `hw:CARD=<card-id>,DEV=<n>`.
    pub id: String,
    /// ALSA card id (e.g. `DAC`, `PCH`, `Loopback`).
    pub card: String,
    /// PCM device name (e.g. `HDMI 0`), with the card name as a fallback.
    pub name: String,
    pub hdmi: bool,
    pub loopback: bool,
    /// This is the configured device.
    pub active: bool,
}

// ---------------------------------------------------------------- volume

pub const VOLUME_MIN_DB: f64 = -120.0;
pub const VOLUME_MAX_DB: f64 = 0.0;
pub const BALANCE_MAX_DB: f64 = 24.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VolumeConfig {
    #[serde(default)]
    pub volume_db: f64,
    #[serde(default)]
    pub balance_db: f64,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VolumeStatus {
    pub volume_db: f64,
    pub balance_db: f64,
    pub muted: bool,
    /// True when the DAC's ALSA hardware mixer controls playback volume.
    pub hardware: bool,
    pub min_db: f64,
    pub max_db: f64,
}

// ---------------------------------------------------------------- config

/// Everything the daemon persists (`$XDG_CONFIG_HOME/audivane-dsp/config.json`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct EngineConfig {
    /// ALSA device, e.g. `hw:Loopback,0,0` or `hw:CARD=DAC,DEV=0`.
    pub device: String,
    /// Preferred output formats in order; the first the device accepts wins.
    #[serde(default = "default_formats")]
    pub formats: Vec<SampleFormat>,
    #[serde(default = "yes")]
    pub dsp_enabled: bool,
    /// Chain A (the normal chain).
    #[serde(default)]
    pub chain: ChainConfig,
    /// Name of the preset chain A was last saved to or loaded from.
    #[serde(default)]
    pub active_preset: Option<String>,
    #[serde(default)]
    pub ab: Option<AbConfig>,
    #[serde(default)]
    pub rate: RatePolicy,
    /// Graph quantum at 48 kHz (scaled for other rates). Larger = more
    /// efficient long filters, more latency.
    #[serde(default = "default_quantum")]
    pub quantum: u32,
    #[serde(default)]
    pub routing: RoutingMode,
    #[serde(default)]
    pub volume: VolumeConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            device: String::new(),
            formats: default_formats(),
            dsp_enabled: true,
            chain: ChainConfig::default(),
            active_preset: None,
            ab: None,
            rate: RatePolicy::default(),
            quantum: default_quantum(),
            routing: RoutingMode::default(),
            volume: VolumeConfig::default(),
        }
    }
}

fn default_formats() -> Vec<SampleFormat> {
    vec![
        SampleFormat::S32,
        SampleFormat::S24,
        SampleFormat::S24_3,
        SampleFormat::S16,
    ]
}
fn default_quantum() -> u32 {
    1024
}
fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RatePolicy {
    /// Switch the graph (and DAC) to the playing stream's native rate.
    #[serde(default = "yes")]
    pub follow_source: bool,
    /// Graph rates the engine may run at (source rates, before upsampling).
    #[serde(default = "default_rates")]
    pub allowed_rates: Vec<u32>,
    /// Rate used when idle or when following is off.
    #[serde(default = "default_rate")]
    pub default_rate: u32,
    /// What to do when apps playing into the DSP sink at the same time want
    /// different rates. Only used while `follow_source` is on.
    #[serde(default)]
    pub conflict: RateConflict,
}

impl Default for RatePolicy {
    fn default() -> Self {
        Self {
            follow_source: true,
            allowed_rates: default_rates(),
            default_rate: default_rate(),
            conflict: RateConflict::default(),
        }
    }
}

/// How the engine picks the graph rate when apps playing into the DSP sink
/// disagree. The Direct (bit-perfect) sink always decides on its own.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum RateConflict {
    /// Keep the current rate while any app still plays at it; otherwise
    /// follow the app that started playing first. The others are resampled.
    #[default]
    KeepCurrent,
    /// Use the highest native rate among the apps playing.
    Highest,
    /// Use `default_rate` while the apps disagree (one app is still followed).
    DefaultRate,
}

fn default_rates() -> Vec<u32> {
    vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000]
}
fn default_rate() -> u32 {
    48_000
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RoutingMode {
    /// Make "Audivane DSP" the default sink; everything plays through it.
    #[default]
    System,
    /// Leave the default sink alone; only matching apps are moved to us.
    Apps { rules: Vec<AppRule> },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AppRule {
    /// Matches `application.name` or `application.process.binary`
    /// (case-insensitive, exact).
    pub app: String,
    pub sink: SinkChoice,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum SinkChoice {
    /// "Audivane DSP": full chain.
    Dsp,
    /// "Audivane Direct": bit-exact, no DSP.
    Direct,
    /// Whatever the session default is (unroute).
    Default,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Node(u32),
    App(String),
}

// ---------------------------------------------------------------- A/B

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AbSide {
    #[default]
    A,
    B,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AbConfig {
    /// Chain B; must share A's upsampler ratio and output config.
    pub chain_b: ChainConfig,
    #[serde(default)]
    pub level: LevelMatch,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LevelMatch {
    /// A-weighted, pink-noise-referenced analytic offset (default).
    #[default]
    Analytic,
    /// Fixed gain applied to B, in dB.
    Manual {
        offset_db: f64,
    },
    Off,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AbStatus {
    pub enabled: bool,
    pub current: AbSide,
    /// Gain applied to B (dB) after level matching.
    pub offset_db: f64,
}

// ---------------------------------------------------------------- status

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    /// Device released or never acquired.
    Idle,
    /// Device open, graph driven, no stream playing.
    Ready,
    Playing,
    /// Reopening the device for a new rate.
    Switching,
    Error,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Status {
    pub version: String,
    pub state: EngineState,
    pub device: String,
    /// Last error, if `state == Error` (or a recent recoverable one).
    pub error: Option<String>,
    /// Negotiated ALSA format, when open (`None` in `Error` after a failed
    /// open or switch).
    pub format: Option<SampleFormat>,
    /// Graph (source) rate.
    pub graph_rate: u32,
    /// DAC rate (graph_rate × ratio).
    pub output_rate: u32,
    pub ratio: u32,
    pub quantum: u32,
    /// End-to-end added latency (DSP + buffers), ms: the ALSA output buffer
    /// delay plus [`Self::filter_latency_ms`].
    pub latency_ms: f64,
    /// The part of [`Self::latency_ms`] that is filter delay in the active
    /// chain (upsampler and convolution), ms. 0 on the Direct sink or with
    /// DSP off, where the chain is not in the path.
    #[serde(default)]
    pub filter_latency_ms: f64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub xruns: u64,
    /// DSP time per cycle / cycle period, 0..1.
    pub dsp_load: f64,
    pub dsp_enabled: bool,
    /// Which sink currently feeds the DAC.
    pub active_sink: Option<SinkChoice>,
    pub passthrough: Passthrough,
    pub ab: Option<AbStatus>,
    /// Active preset, when chain A was saved to or loaded from one.
    #[serde(default)]
    pub preset: Option<PresetState>,
    #[serde(default)]
    pub volume: Option<VolumeStatus>,
    /// What the configured output device accepts, probed when it was
    /// opened; also kept when that device turned out unusable (then `rates`
    /// is empty and the unfiltered fields say why). `None` before the first
    /// open, when the probe failed or the device could not be opened.
    #[serde(default)]
    pub device_caps: Option<DeviceCapabilities>,
    /// Graph rates the engine actually offers PipeWire: `rate.allowed_rates`
    /// restricted to rates the device can play (directly or upsampled).
    /// Streams at other rates are resampled by PipeWire. Empty while no
    /// device is open.
    #[serde(default)]
    pub effective_rates: Vec<u32>,
    /// The subset of [`Self::effective_rates`] the device plays natively
    /// (x1). Audivane Direct never upsamples, so it only runs the graph at
    /// these: a Direct stream at another rate makes the graph move to one of
    /// them and is resampled by PipeWire (passthrough then reports
    /// `resampled`). Empty while no device is open, or when no effective rate
    /// is native (Direct cannot play on the device then).
    #[serde(default)]
    pub direct_rates: Vec<u32>,
    /// Rate used when idle / when following is off, after the same
    /// restriction (`rate.default_rate` when the device supports it).
    #[serde(default)]
    pub effective_default_rate: Option<u32>,
    /// Bit depth dither targets: `chain.output.bits` capped at the container
    /// width of [`Self::format`]. `None` while no device is open.
    #[serde(default)]
    pub dither_bits: Option<u32>,
    /// The session's default output when it is not one of Audivane's sinks
    /// (its `node.description`): where a stream left unrouted plays in
    /// per-app routing. `None` in system-wide routing (the default is
    /// Audivane DSP) or when there is no other output.
    #[serde(default)]
    pub normal_output: Option<String>,
}

/// Sample formats the device accepts at one rate.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RateCapability {
    pub rate: u32,
    pub formats: Vec<FormatCapability>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FormatCapability {
    pub format: SampleFormat,
    /// Significant bits ALSA reports for this format at this rate (e.g. 24
    /// for a 24-in-32 S32 device), when the driver says.
    #[serde(default)]
    pub sbits: Option<u32>,
}

/// What an ALSA playback device accepts for the engine's channel count with
/// interleaved read/write access. Only rates the engine could ask for are
/// probed (every allowed graph rate times every upsampling ratio, up to
/// 768 kHz), as (rate, format) pairs: USB devices often tie formats to rates.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DeviceCapabilities {
    pub device: String,
    /// Channel count the probe used.
    pub channels: u32,
    /// Channel counts the device accepts at all.
    pub channels_min: u32,
    pub channels_max: u32,
    /// Ascending by rate; only rates with at least one usable format.
    pub rates: Vec<RateCapability>,
    /// Unfiltered rate range the device accepts (any channel count or
    /// format, also rates the engine never runs at). Equal for a fixed-rate
    /// device; 0 when unknown.
    #[serde(default)]
    pub rate_min: u32,
    #[serde(default)]
    pub rate_max: u32,
    /// Engine formats the device accepts at all (any channel count or rate).
    #[serde(default)]
    pub formats: Vec<SampleFormat>,
}

impl DeviceCapabilities {
    /// Rates with at least one supported format, ascending.
    pub fn supported_rates(&self) -> Vec<u32> {
        self.rates.iter().map(|r| r.rate).collect()
    }

    /// Formats supported at `rate` (empty when the rate is not supported).
    pub fn formats_at(&self, rate: u32) -> Vec<SampleFormat> {
        self.rates
            .iter()
            .find(|r| r.rate == rate)
            .map(|r| r.formats.iter().map(|f| f.format).collect())
            .unwrap_or_default()
    }

    pub fn supports(&self, rate: u32, format: SampleFormat) -> bool {
        self.rates
            .iter()
            .any(|r| r.rate == rate && r.formats.iter().any(|f| f.format == format))
    }

    pub fn supports_rate(&self, rate: u32) -> bool {
        self.rates
            .iter()
            .any(|r| r.rate == rate && !r.formats.is_empty())
    }

    /// Every format supported at some rate, in `S16, S24, S24_3, S32` order.
    pub fn all_formats(&self) -> Vec<SampleFormat> {
        [
            SampleFormat::S16,
            SampleFormat::S24,
            SampleFormat::S24_3,
            SampleFormat::S32,
        ]
        .into_iter()
        .filter(|f| {
            self.rates
                .iter()
                .any(|r| r.formats.iter().any(|c| c.format == *f))
        })
        .collect()
    }

    /// Whether the device takes the probed channel count at all.
    pub fn channels_ok(&self) -> bool {
        (self.channels_min..=self.channels_max).contains(&self.channels)
    }

    /// The device's own rate range, e.g. `32000 Hz` or `8000-96000 Hz`
    /// (`None` when unknown).
    pub fn native_rate_range(&self) -> Option<String> {
        match (self.rate_min, self.rate_max) {
            (0, 0) => None,
            (a, b) if a == b => Some(format!("{a} Hz")),
            (a, b) => Some(format!("{a}-{b} Hz")),
        }
    }

    fn channels_text(&self) -> String {
        if self.channels_min == self.channels_max {
            format!("channels {}", self.channels_min)
        } else {
            format!("channels {}-{}", self.channels_min, self.channels_max)
        }
    }

    /// What the device accepts regardless of the engine's channel count and
    /// rate list, e.g. `rate 32000 Hz; formats S16; channels 2`.
    pub fn native_summary(&self) -> String {
        let rates = match (self.rate_min, self.rate_max) {
            (0, 0) => "unknown rates".to_string(),
            (a, b) if a == b => format!("rate {a} Hz"),
            (a, b) => format!("rates {a}-{b} Hz"),
        };
        let formats: Vec<&str> = self.formats.iter().map(|f| f.name()).collect();
        let formats = if formats.is_empty() {
            "none of S16/S24/S24_3/S32".to_string()
        } else {
            formats.join(", ")
        };
        format!("{rates}; formats {formats}; {}", self.channels_text())
    }

    /// One-line summary for error messages, e.g.
    /// `rates 48000 Hz; formats S16; channels 2`: the usable (rate, format)
    /// pairs, or what the device accepts at all when none is usable.
    pub fn summary(&self) -> String {
        let rates = self.supported_rates();
        let formats = self.all_formats();
        if rates.is_empty() || formats.is_empty() {
            return self.native_summary();
        }
        let list: Vec<String> = rates.iter().map(u32::to_string).collect();
        let names: Vec<&str> = formats.iter().map(|f| f.name()).collect();
        format!(
            "rates {} Hz; formats {}; {}",
            list.join(", "),
            names.join(", "),
            self.channels_text()
        )
    }

    /// Why a device with no usable (rate, format) pair cannot be used:
    /// wrong channel count, only rates outside the engine's list, or no
    /// format the engine can write. `None` when something is usable.
    pub fn unusable_reason(&self) -> Option<String> {
        if !self.rates.is_empty() {
            return None;
        }
        let supports = self.native_summary();
        let device = &self.device;
        if !self.channels_ok() {
            let has = if self.channels_min == self.channels_max {
                self.channels_min.to_string()
            } else {
                format!("{}-{}", self.channels_min, self.channels_max)
            };
            return Some(format!(
                "device '{device}' needs {} channels; device supports {has} (engine: channels {}; device: {supports})",
                self.channels, self.channels
            ));
        }
        if self.formats.is_empty() {
            return Some(format!(
                "device '{device}' takes none of the engine's sample formats S16/S24/S24_3/S32 (device supports {supports})"
            ));
        }
        Some(match self.native_rate_range() {
            Some(range) => format!(
                "device '{device}' supports {range} only, which the engine doesn't run at (device supports {supports})"
            ),
            None => format!(
                "device '{device}' supports none of the engine's sample rates (device supports {supports})"
            ),
        })
    }
}

/// "Is what reaches the DAC bit-identical to the source?"
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Passthrough {
    pub intact: bool,
    /// Empty when intact. Machine-readable codes the UI maps to text.
    pub broken_because: Vec<PassthroughBreak>,
    /// Things the control plane could not verify (the path may well be
    /// bit-perfect). Never a reason to report `intact: false`.
    #[serde(default)]
    pub caveats: Vec<PassthroughCaveat>,
}

/// A piece of information missing from the passthrough verdict.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PassthroughCaveat {
    /// The negotiated PipeWire format is a container width, not the source's
    /// precision: `pw-play` opens a 24-bit WAV as `S32LE`, and a float bus
    /// says nothing about the file behind it. We only know the source has
    /// more than 24 bits when the player reports it (MPRIS `native_bits`).
    UnknownSourcePrecision {
        /// Container name, e.g. `S32LE` or `F32LE`.
        format: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PassthroughBreak {
    DspActive,
    Upsampling {
        ratio: u32,
    },
    /// Sink or stream software volume is not exactly 1.0, or muted.
    Volume {
        node: u32,
        volume: f64,
    },
    /// More than one stream is mixed into the sink.
    Mixing {
        streams: u32,
    },
    /// The stream is resampled by PipeWire to the graph rate.
    Resampled {
        stream_rate: u32,
        graph_rate: u32,
    },
    /// The stream's channel layout is remixed.
    ChannelMix {
        stream_channels: u32,
        sink_channels: u32,
    },
    /// The source has more precision than the f32 graph carries exactly.
    /// Only reported when that precision is actually known (e.g. from MPRIS
    /// metadata); a wide container on its own is a [`PassthroughCaveat`].
    SourceOver24Bit,
    /// Output word length is shorter than the source (e.g. 24-bit into S16).
    Truncated {
        source_bits: u32,
        output_bits: u32,
    },
    NotPlaying,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Meters {
    /// Per channel, dBFS, since last report: the output just before the
    /// quantizer, after the master volume. On the DSP path this is the float
    /// signal, so it reads above 0 dBFS when the quantizer clips; on the
    /// Direct / bypass paths it is the integer output.
    pub peak_db: Vec<f64>,
    pub rms_db: Vec<f64>,
    /// Samples clipped since daemon start.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub clipped: u64,
    /// Samples clipped since daemon start, per channel.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "Array<number>"))]
    pub clipped_ch: Vec<u64>,
    /// Per channel, dBFS, chain input (before the EQ) since last report.
    #[serde(default)]
    pub input_peak_db: Vec<f64>,
    /// Per channel, dBFS, chain output before the master volume, same
    /// window. `chain_peak_db - input_peak_db` is the gain the chain (EQ,
    /// convolution, upsampler) adds at the peaks. Empty when no float signal
    /// went through the chain in this window (Direct / bypass / idle).
    #[serde(default)]
    pub chain_peak_db: Vec<f64>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub xruns: u64,
    pub dsp_load: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct StreamInfo {
    /// PipeWire node id.
    pub node: u32,
    pub app_name: Option<String>,
    pub binary: Option<String>,
    pub media_name: Option<String>,
    pub rate: Option<u32>,
    pub channels: Option<u32>,
    /// e.g. "S24LE", "F32LE".
    pub format: Option<String>,
    /// Sink it is linked to, if one of ours.
    pub sink: Option<SinkChoice>,
    pub volume: Option<f64>,
    pub muted: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct NowPlaying {
    /// MPRIS bus name, e.g. `org.mpris.MediaPlayer2.strawberry`.
    pub player: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub status: String,
    /// Native file rate/bit depth if the player exposes it (non-standard
    /// MPRIS metadata keys; often absent).
    pub native_rate: Option<u32>,
    pub native_bits: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SelfTestResult {
    pub passed: bool,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub frames: u64,
    pub rate: u32,
    pub format: Option<SampleFormat>,
    /// FNV-1a of the expected left-justified i32 samples.
    pub expected_hash: String,
    /// FNV-1a of what was handed to ALSA.
    pub actual_hash: String,
    pub detail: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r = Request {
            id: 2,
            call: Call::AbSelect {
                side: AbSide::B,
                crossfade_ms: 10.0,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"id":2,"method":"ab_select","params":{"side":"b","crossfade_ms":10.0}}"#
        );
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back, r);
        let st: Request = serde_json::from_str(r#"{"id":1,"method":"status"}"#).unwrap();
        assert_eq!(st.call, Call::Status);
        let resp = Response {
            id: 1,
            result: Outcome::Error(RpcError {
                code: 1,
                message: "x".into(),
            }),
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"id":1,"error":{"code":1,"message":"x"}}"#
        );
        let cfg: EngineConfig = serde_json::from_str(r#"{"device":"hw:Loopback,0,0"}"#).unwrap();
        assert_eq!(cfg.quantum, 1024);
        assert!(cfg.rate.follow_source);
        assert_eq!(cfg.rate.conflict, RateConflict::KeepCurrent);
    }

    #[test]
    fn rate_conflict_wire_format_and_old_files() {
        // A rate policy written before `conflict` existed still loads.
        let old: RatePolicy = serde_json::from_str(
            r#"{"follow_source":true,"allowed_rates":[44100,48000],"default_rate":48000}"#,
        )
        .unwrap();
        assert_eq!(old.conflict, RateConflict::KeepCurrent);
        for (mode, text) in [
            (RateConflict::KeepCurrent, "\"keep_current\""),
            (RateConflict::Highest, "\"highest\""),
            (RateConflict::DefaultRate, "\"default_rate\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), text);
            assert_eq!(serde_json::from_str::<RateConflict>(text).unwrap(), mode);
        }
    }

    #[test]
    fn preset_wire_shapes() {
        // `overwrite` is optional on the wire.
        let request: Request =
            serde_json::from_str(r#"{"id":1,"method":"save_preset","params":{"name":"X"}}"#)
                .unwrap();
        assert_eq!(
            request.call,
            Call::SavePreset {
                name: "X".into(),
                overwrite: false,
            }
        );
        let request: Request = serde_json::from_str(
            r#"{"id":2,"method":"import_apo","params":{"name":"X","text":"Preamp: -1 dB\n"}}"#,
        )
        .unwrap();
        assert!(matches!(request.call, Call::ImportApo { .. }));
        let request: Request = serde_json::from_str(r#"{"id":3,"method":"list_devices"}"#).unwrap();
        assert_eq!(request.call, Call::ListDevices);

        // Presets are a versioned envelope around an opaque chain.
        let preset = Preset::new("Bass", ChainConfig::default());
        let text = serde_json::to_string(&preset).unwrap();
        assert!(
            text.starts_with(r#"{"version":1,"name":"Bass","chain":{"#),
            "{text}"
        );
        let back: Preset = serde_json::from_str(&text).unwrap();
        assert_eq!(back, preset);
        // A file without `version` still loads.
        let bare: Preset = serde_json::from_str(r#"{"name":"Old","chain":{}}"#).unwrap();
        assert_eq!(bare.version, 1);

        let info = PresetInfo {
            name: "Bass".into(),
            modified_unix: 1_700_000_000,
            bands: 3,
            convolution: 1,
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"name":"Bass","modified_unix":1700000000,"bands":3,"convolution":1}"#
        );

        // `Status.preset` is additive: older peers omit it.
        let status = Status {
            version: "x".into(),
            state: EngineState::Idle,
            device: "hw:0,0".into(),
            error: None,
            format: None,
            graph_rate: 48_000,
            output_rate: 48_000,
            ratio: 1,
            quantum: 1024,
            latency_ms: 0.0,
            filter_latency_ms: 0.0,
            xruns: 0,
            dsp_load: 0.0,
            dsp_enabled: true,
            active_sink: None,
            passthrough: Passthrough {
                intact: true,
                broken_because: Vec::new(),
                caveats: Vec::new(),
            },
            ab: None,
            volume: None,
            preset: Some(PresetState {
                name: "Bass".into(),
                modified: true,
            }),
            device_caps: None,
            effective_rates: Vec::new(),
            direct_rates: Vec::new(),
            effective_default_rate: None,
            dither_bits: None,
            normal_output: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["preset"]["name"], "Bass");
        assert_eq!(value["preset"]["modified"], true);
        let mut without = value.clone();
        without.as_object_mut().unwrap().remove("preset");
        let parsed: Status = serde_json::from_value(without).unwrap();
        assert!(parsed.preset.is_none());

        // `active_preset` is optional and persisted with the config.
        let cfg: EngineConfig = serde_json::from_str(r#"{"device":"hw:0,0"}"#).unwrap();
        assert!(cfg.active_preset.is_none());
    }

    #[test]
    fn passthrough_caveats_are_optional_on_the_wire() {
        // Older peers omit `caveats` entirely.
        let p: Passthrough =
            serde_json::from_str(r#"{"intact":true,"broken_because":[]}"#).unwrap();
        assert!(p.caveats.is_empty());
        let p = Passthrough {
            intact: true,
            broken_because: Vec::new(),
            caveats: vec![PassthroughCaveat::UnknownSourcePrecision {
                format: "S32LE".into(),
            }],
        };
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"intact":true,"broken_because":[],"caveats":[{"reason":"unknown_source_precision","format":"S32LE"}]}"#
        );
    }

    #[test]
    fn set_convolution_accepts_null_object_and_array() {
        let cases = [
            (
                r#"{"id":1,"method":"set_convolution","params":null}"#,
                0usize,
            ),
            (
                r#"{"id":2,"method":"set_convolution","params":{"ir_path":"/tmp/a.wav"}}"#,
                1,
            ),
            (
                r#"{"id":3,"method":"set_convolution","params":[{"ir_path":"/tmp/a.wav","gain_db":-2.0},{"name":"Second","ir_path":"/tmp/b.wav","enabled":false}]}"#,
                2,
            ),
        ];
        for (line, expected_len) in cases {
            let req: Request = serde_json::from_str(line).unwrap();
            let val: serde_json::Value = serde_json::from_str(line).unwrap();
            let call: Call = serde_json::from_value(val).unwrap();
            assert_eq!(req.call, call);
            match call {
                Call::SetConvolution(list) => assert_eq!(list.len(), expected_len),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn volume_wire_format_and_back_compat() {
        let req = Request {
            id: 7,
            call: Call::SetVolume {
                volume_db: -12.5,
                balance_db: -1.5,
                muted: true,
            },
        };
        let encoded = serde_json::to_string(&req).unwrap();
        assert_eq!(
            encoded,
            r#"{"id":7,"method":"set_volume","params":{"volume_db":-12.5,"balance_db":-1.5,"muted":true}}"#
        );
        let decoded: Request = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, req);

        // Old config JSON without `volume` field deserializes with defaults.
        let cfg: EngineConfig = serde_json::from_str(r#"{"device":"hw:0,0"}"#).unwrap();
        assert_eq!(cfg.volume, VolumeConfig::default());
        assert_eq!(cfg.volume.volume_db, 0.0);
        assert_eq!(cfg.volume.balance_db, 0.0);
        assert!(!cfg.volume.muted);

        // Partial VolumeConfig defaults omitted fields.
        let partial: VolumeConfig = serde_json::from_str(r#"{"volume_db":-6.0}"#).unwrap();
        assert_eq!(partial.volume_db, -6.0);
        assert_eq!(partial.balance_db, 0.0);
        assert!(!partial.muted);

        // Status without `volume` field deserializes cleanly with `None`.
        let status_json = r#"{
            "version":"0.1.0",
            "state":"ready",
            "device":"hw:0,0",
            "error":null,
            "format":null,
            "graph_rate":48000,
            "output_rate":48000,
            "ratio":1,
            "quantum":1024,
            "latency_ms":0.0,
            "xruns":0,
            "dsp_load":0.0,
            "dsp_enabled":true,
            "active_sink":null,
            "passthrough":{"intact":true,"broken_because":[]},
            "ab":null
        }"#;
        let status: Status = serde_json::from_str(status_json).unwrap();
        assert_eq!(status.volume, None);
    }

    #[test]
    fn meters_clip_and_headroom_fields_are_additive() {
        // Older peers omit both fields.
        let meters: Meters = serde_json::from_str(
            r#"{"peak_db":[-1.0],"rms_db":[-2.0],"clipped":3,"xruns":0,"dsp_load":0.1}"#,
        )
        .unwrap();
        assert!(meters.clipped_ch.is_empty());
        assert!(meters.input_peak_db.is_empty());

        let meters = Meters {
            peak_db: vec![-1.0, -2.0],
            rms_db: vec![-3.0, -4.0],
            clipped: 5,
            clipped_ch: vec![2, 3],
            input_peak_db: vec![-0.5, -0.6],
            chain_peak_db: Vec::new(),
            xruns: 1,
            dsp_load: 0.2,
        };
        let value = serde_json::to_value(&meters).unwrap();
        assert_eq!(value["clipped_ch"], serde_json::json!([2, 3]));
        assert_eq!(value["input_peak_db"], serde_json::json!([-0.5, -0.6]));
        assert_eq!(serde_json::from_value::<Meters>(value).unwrap(), meters);
    }

    #[test]
    fn spectrum_topic_and_upsampler_response_wire_formats() {
        // `upsampler_response` params are optional.
        let request: Request =
            serde_json::from_str(r#"{"id":1,"method":"upsampler_response","params":{}}"#).unwrap();
        assert_eq!(
            request.call,
            Call::UpsamplerResponse {
                config: None,
                source_rate: None,
                window: None,
            }
        );
        let request: Request = serde_json::from_str(
            r#"{"id":2,"method":"upsampler_response","params":{"source_rate":44100}}"#,
        )
        .unwrap();
        assert_eq!(
            request.call,
            Call::UpsamplerResponse {
                config: None,
                source_rate: Some(44_100),
                window: None,
            }
        );
        let request: Request = serde_json::from_str(
            r#"{"id":2,"method":"upsampler_response","params":{"window":{"f_lo_hz":21000,"f_hi_hz":23000,"points":800}}}"#,
        )
        .unwrap();
        assert_eq!(
            request.call,
            Call::UpsamplerResponse {
                config: None,
                source_rate: None,
                window: Some(ResponseWindowRequest {
                    f_lo_hz: 21_000.0,
                    f_hi_hz: 23_000.0,
                    points: 800,
                }),
            }
        );
        let request: Request = serde_json::from_str(r#"{"id":3,"method":"meters"}"#).unwrap();
        assert_eq!(request.call, Call::Meters);

        // Topic/Event use the usual lowercase names.
        let request: Request = serde_json::from_str(
            r#"{"id":4,"method":"subscribe","params":{"topics":["spectrum"]}}"#,
        )
        .unwrap();
        assert_eq!(
            request.call,
            Call::Subscribe {
                topics: vec![Topic::Spectrum],
            }
        );
        let event = Event::Spectrum(Spectrum {
            f_min_hz: 20.0,
            bins_per_octave: 24,
            pre: vec![-10.0],
            post: vec![-20.0],
            rate: 48_000,
        });
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["event"], "spectrum");
        assert_eq!(value["data"]["post"], serde_json::json!([-20.0]));
        assert_eq!(serde_json::from_value::<Event>(value).unwrap(), event);

        let response = UpsamplerResponse {
            source_rate: 44_100,
            output_rate: 176_400,
            ratio: 4,
            taps: 4096,
            phase: crate::FilterPhase::Linear,
            effective_cutoff: 1.0,
            freqs_hz: vec![0.0, 1000.0],
            magnitude_db: vec![0.0, -0.1],
            impulse: vec![1.0, 0.5],
            impulse_peak: 0,
            passband_ripple_db: 0.001,
            minus_0_1_db_hz: 21_000.0,
            minus_3_db_hz: 22_000.0,
            stopband_start_hz: 24_000.0,
            stopband_atten_db: 130.0,
            latency_samples: 2048,
            crossings: vec![ResponseCrossing {
                level_db: -3.0,
                freq_hz: 22_000.5,
            }],
            window: Some(ResponseWindow {
                f_lo_hz: 21_000.0,
                f_hi_hz: 23_000.0,
                freqs_hz: vec![21_000.0, 23_000.0],
                magnitude_db: vec![0.0, -150.0],
            }),
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["ratio"], 4);
        assert_eq!(value["phase"], "linear");
        assert_eq!(
            serde_json::from_value::<UpsamplerResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn device_capabilities_helpers() {
        let caps = DeviceCapabilities {
            device: "hw:CARD=V3,DEV=0".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rate_min: 48_000,
            rate_max: 48_000,
            formats: vec![SampleFormat::S16],
            rates: vec![RateCapability {
                rate: 48_000,
                formats: vec![FormatCapability {
                    format: SampleFormat::S16,
                    sbits: Some(16),
                }],
            }],
        };
        assert_eq!(caps.supported_rates(), vec![48_000]);
        assert!(caps.supports(48_000, SampleFormat::S16));
        assert!(!caps.supports(48_000, SampleFormat::S32));
        assert!(!caps.supports_rate(44_100));
        assert_eq!(caps.all_formats(), vec![SampleFormat::S16]);
        assert_eq!(caps.summary(), "rates 48000 Hz; formats S16; channels 2");
        let back: DeviceCapabilities =
            serde_json::from_value(serde_json::to_value(&caps).unwrap()).unwrap();
        assert_eq!(back, caps);
    }
}
