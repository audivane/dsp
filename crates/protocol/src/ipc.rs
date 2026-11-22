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

use crate::{
    ChainConfig, ConvolutionConfig, CrossfeedConfig, DcBlockConfig, EqProfile, HarmonicsConfig,
    PitchConfig, SampleFormat, UpsamplerConfig,
};
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
    /// Set or clear the DC-blocking high-pass filter of chain A. → `null`
    SetDcBlock(Option<DcBlockConfig>),
    /// Set or clear the headphone crossfeed of chain A. → `null`
    SetCrossfeed(Option<CrossfeedConfig>),
    /// Set or clear the EQ of chain A. → `null`
    SetEq(Option<EqProfile>),
    /// → `null`
    SetConvolution(
        #[serde(deserialize_with = "crate::deserialize_conv_list")] Vec<ConvolutionConfig>,
    ),
    /// → `null`
    SetUpsampler(UpsamplerConfig),
    /// Set pitch shifting for chain A. → `null`
    SetPitch(PitchConfig),
    /// Set or clear the harmonic distortion stage of chain A. → `null`
    SetHarmonics(Option<HarmonicsConfig>),
    /// Configure the shared post-volume limiter. → `null`
    SetLimiter(crate::LimiterConfig),
    /// Turn all DSP off/on without losing the settings. → `null`
    SetDspEnabled { enabled: bool },
    /// Set chain A's output format and dither (`chain.output`; chain B
    /// follows). → `null`
    SetOutput(crate::OutputConfig),
    /// Set the DAC sample formats to try, in order (`formats`). → `null`
    SetFormats(Vec<SampleFormat>),
    /// Set the rate policy (`rate`: allowed rates, default rate, following,
    /// conflicts). → `null`
    SetRate(RatePolicy),
    /// Set the output device (`device`; empty: the default playback
    /// device). → `null`
    SetDevice { device: String },
    /// Configure chain B for A/B (None disables A/B).
    ///
    /// Chain B's `upsampler`, `limiter` and `output` always follow chain A; differing
    /// values are replaced (never rejected). → [`AbStatus`], plus an optional
    /// `"notes"` array of strings when chain B was realigned with chain A.
    SetAb(Option<AbConfig>),
    /// → [`AbStatus`]
    AbSelect {
        side: AbSide,
        #[serde(default = "default_fade")]
        crossfade_ms: f64,
    },
    /// Measured level matching ("Match to this track"). `{max_seconds}`
    /// ([`AB_MEASURE_SECONDS`]) starts measuring both sides while playback
    /// continues at the current offset; the offset locks into
    /// [`LevelMatch::Measured`] once it is stable or at `max_seconds` of
    /// gated audio. `{accept: true}` locks the running value now,
    /// `{cancel: true}` stops and keeps the current level. → [`AbStatus`]
    AbMeasure {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        max_seconds: Option<f64>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
        cancel: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
        accept: bool,
    },
    /// Set output volume, balance, and mute, unconditionally (the CLI).
    /// → [`VolumeStatus`]
    SetVolume {
        volume_db: f64,
        balance_db: f64,
        muted: bool,
    },
    /// Compare-and-set of volume, balance and mute: applies only on the daemon
    /// process `if_instance` names (else [`RpcError::STALE_INSTANCE`]) while the
    /// level state is at `if_revision` (else [`RpcError::STALE_REVISION`]).
    /// Only the fields present change, the rest keeping their value at the
    /// guarded revision, so a gesture writes only what it edits. A separate
    /// method, so an older daemon refuses it instead of applying it
    /// unconditionally. → [`VolumeStatus`]
    SetVolumeGuarded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        volume_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        balance_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        muted: Option<bool>,
        if_instance: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        if_revision: u64,
    },
    /// Enable, disable, or change the varispeed stage without rebuilding
    /// the DSP chain. Omitted fields keep their current configured value;
    /// explicit `null` values are rejected. → [`VarispeedStatus`]
    SetVarispeed {
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        enabled: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        speed: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        ramp_ms: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        keep_pitch: Option<bool>,
    },
    /// Multi-device: add, edit or remove a follower (`outputs.followers`) live,
    /// without rebuilding the master. An unlisted `device` is added with the
    /// omitted fields at their defaults, `remove: true` removes it; omitted
    /// fields follow the rule of [`Self::SetVarispeed`]. → [`OutputsStatus`]
    SetFollower {
        device: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
        remove: bool,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        enabled: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        trim_ms: Option<f64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        volume_db: Option<f64>,
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
    /// Set one app stream's level and/or mute by PipeWire node id, on a stream
    /// playing on one of our sinks. The level is capped by
    /// `volume.guard.stream_max_db` and becomes the level the guard holds;
    /// without either field it only reports the stream. → [`StreamVolume`]
    SetStreamVolume {
        node: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        volume_db: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        muted: Option<bool>,
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
    /// The filter design a catalogue preset (or `recommended`) expands
    /// to, with `based_on` set to the name: the starting point of "Edit as
    /// custom". → [`crate::StoredDesign`]
    ResolveDesign { preset: String },
    /// Check a design without designing its kernel (cheap enough to call as
    /// the user types): field bounds and, at each allowed source rate (or only
    /// `source_rate`), ratio rules and budgets. `target` defaults to chain A's;
    /// the output format, dither, A/B pipeline count and block size come from
    /// the config. Nothing changes. → [`DesignCheck`]
    CheckDesign {
        design: crate::StoredDesign,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        target: Option<crate::RateTarget>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        source_rate: Option<u32>,
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

/// Deserialize a present field as `Some(T)`, rejecting explicit `null` while
/// letting `#[serde(default)]` yield `None` when the key is omitted.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
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
    /// `set_volume_guarded`'s `if_instance` names another daemon process:
    /// the change was made against a daemon that has since restarted.
    pub const STALE_INSTANCE: i32 = 3;
    /// `set_volume_guarded`'s `if_revision` is not the current level
    /// revision: the level changed since the client read it.
    pub const STALE_REVISION: i32 = 4;
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
    /// The DSP chain needed more CPU than real time and the engine fell
    /// back to a lighter one (one event per fallback step), sent to
    /// [`Topic::Status`] subscribers. [`Status::overload`] carries it while
    /// the fallback is in effect.
    Overload(OverloadReport),
    /// The playback device list (as `list_devices` returns it), after a
    /// sound card came or went, or the configured device changed.
    Devices(Vec<DeviceInfo>),
}

/// What the engine did about a DSP overload.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum OverloadAction {
    /// Varispeed's processing budget pinned at half the period, whatever
    /// the load measures (its speed range narrows to what that sustains).
    VarispeedBudget,
    /// Varispeed held at 1× (the requested speed is kept).
    VarispeedUnity,
    /// Varispeed switched off (the request is kept); it stays off while a
    /// later step runs.
    VarispeedOff,
    /// Turned A/B off, keeping chain A (the first step of the chain while
    /// A/B is on; the later steps keep it off).
    AbOff,
    /// Went back to the last upsampler setting that ran without overload
    /// in this daemon run.
    Reverted,
    /// Turned the upsampler off.
    UpsamplerOff,
    /// Kept the output rate with the engine bypass resampler's design (the
    /// device plays neither a rate at x1 nor one the bypass resampler
    /// converts to).
    LightUpsampler,
    /// Played the bypass path (DSP off; through the engine bypass
    /// resampler where the device lacks the rate).
    Bypass,
    /// Nothing lighter is left (already bypassing).
    None,
}

/// A DSP overload and the fallback the engine applied. The saved config is
/// unchanged: the requested setting stays in `get_config`; the engine runs
/// the fallback until the upsampler (or, after a bypass, any chain
/// setting) is changed or the daemon restarts. Published once the engine
/// accepted the fallback config; `applied` turns true (with another
/// `overload` event) once the audio thread runs it (a step that needs a
/// device reopen lands later).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OverloadReport {
    /// The stage the step acts on: `"varispeed"` (its budget pinned, its
    /// speed capped at 1×, or the stage retired), `"upsampler"`, or
    /// `"chain"` when it ran without one.
    pub stage: String,
    /// Output rate when it happened, Hz.
    pub output_rate: u32,
    /// Measured DSP load over the last second (1.0 = all of real time).
    pub load: f64,
    pub action: OverloadAction,
    /// Plain text for the UI.
    pub message: String,
    /// Counts fallback steps since the daemon started.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub episode: u64,
    /// The audio thread runs the fallback (false: accepted, still landing).
    #[serde(default)]
    pub applied: bool,
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
    /// The playback device list, on change (checked once a second).
    Devices,
}

/// Live FFT spectrum of the pre (chain input at the graph rate) and post
/// (chain output at the output rate) taps; `post` is the quantizer's output
/// scaled back to `f32`, within half an LSB of the value before quantization.
///
/// Each tap runs two Hann FFTs: 8192 points (50% overlap) for every bin and
/// 32768 points (87.5% overlap) for the low band, where a 1/24-octave bin is
/// narrower than four 8192-point bins. Power is averaged over the frames since
/// the previous report. Bin `k` is centred on `f_min_hz * 2^(k /
/// bins_per_octave)` (half a step either side), up to `min(20 kHz, Nyquist)`.
///
/// Values are band power in dB, channel-mean, rounded to 0.1 dB: 0 dB = a
/// full-scale sine. A bin reads the signal power inside it, or a sine's
/// interpolated peak level where it holds a spectral peak, whichever is
/// higher, independent of FFT size.
///
/// No events while the engine is idle (no complete 8192-sample frame yet);
/// clients keep the last frame.
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
    /// Integer upsampling ratio; 0 for a rational ratio (see
    /// [`Self::rate_ratio`]).
    pub ratio: u32,
    /// Output rate / source rate. For a rational ratio (`down > 1`) the
    /// response is the kernel's on the intermediate grid (source rate x
    /// `up`), which every output phase samples: the plot and the stopband
    /// then run to that grid's Nyquist, and the band edge is the lower of
    /// the two Nyquists.
    #[serde(default)]
    pub rate_ratio: crate::RateRatio,
    /// Kernel length at the output rate actually used: chosen by the engine
    /// for band designs, longer than the configured taps for intermediate
    /// phase.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub taps: usize,
    pub phase: crate::FilterPhase,
    pub filter_window: crate::WindowKind,
    /// Cutoff as a fraction of the source Nyquist actually used for the
    /// filter (`1.0` when upsampling is off).
    pub effective_cutoff: f64,
    /// Kaiser beta actually used (chosen by the engine for band designs).
    pub effective_kaiser_beta: f64,
    /// Largest |impulse| at the original sample instants (the peak plus or
    /// minus whole input periods) relative to the peak, dB. Around -250 dB
    /// or lower the filter passes the input samples through unchanged
    /// (`preserve_samples`); ordinary designs read -20 to -60 dB. -400 when
    /// upsampling is off.
    pub sample_leak_db: f64,
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
    /// Active filter preset when measured from [`UpsamplerConfig::preset`],
    /// or `None` for a manual design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<crate::FilterPreset>,
    /// The design that was measured, resolved (a preset expanded, an
    /// automatic `match_output` budget resolved for the output), with
    /// `based_on` naming the preset it came from. Absent at x1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub design: Option<crate::StoredDesign>,
    /// Its plan once designed (an automatic length searched), with the
    /// bounded-input peak. Absent at x1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub plan: Option<crate::design::DesignPlan>,
    /// Where the engine plays each `rate.allowed_rates` source rate (and
    /// [`Self::source_rate`]) with this design's target on the open device,
    /// as a fresh open decides it. The response is measured at the entry
    /// for [`Self::source_rate`]: 1:1 when it is converted. Empty without a
    /// device probe (the target's ratio is measured, as
    /// `resolve_rate_ratio` gives it).
    #[serde(default)]
    pub device_landings: Vec<SourceLanding>,
}

/// One entry of [`UpsamplerResponse::device_landings`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SourceLanding {
    pub source_rate: u32,
    /// The ratio the engine opens the device at for a stream at this rate
    /// (1:1 when [`Self::converted_to`] is set).
    pub ratio: crate::RateRatio,
    /// The ratio the target asks for without the device
    /// (`resolve_rate_ratio`).
    pub requested: crate::RateRatio,
    /// The engine does not offer this rate on the device: PipeWire converts
    /// the stream to this graph rate before the engine sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub converted_to: Option<u32>,
    /// A "Best for DAC" target (`max_rate_any`, the same cap or 768 kHz)
    /// and the ratio it would open at here, when the target lands 1:1 or
    /// converted here and that one would upsample. Absent for `off`,
    /// `rate` and `max_rate_any` targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub best_for_dac: Option<BestForDac>,
}

/// [`SourceLanding::best_for_dac`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BestForDac {
    pub target: crate::RateTarget,
    pub ratio: crate::RateRatio,
}

/// The result of [`Call::CheckDesign`]: empty `errors` means the design
/// passes every check that runs before its kernel is designed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DesignCheck {
    /// Every refusal, each naming the field it is about. A refusal at one
    /// source rate says so in its message.
    pub errors: Vec<crate::design::FieldError>,
    /// The plan at each checked source rate where the design upsamples and
    /// passes (rates where the target resolves to x1 are left out).
    pub plans: Vec<RatePlan>,
}

/// One entry of [`DesignCheck::plans`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RatePlan {
    pub source_rate: u32,
    pub ratio: crate::RateRatio,
    pub plan: crate::design::DesignPlan,
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

/// Fastest default rise for `guard: limit`, dB per second.
pub const GUARD_DEFAULT_RISE_DB_PER_S: f64 = 6.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VolumeConfig {
    #[serde(default)]
    pub volume_db: f64,
    #[serde(default)]
    pub balance_db: f64,
    #[serde(default)]
    pub muted: bool,
    /// Where the master volume is applied.
    #[serde(default)]
    pub mode: VolumeMode,
    /// Fixed hardware mixer level, dB. `software`: written once (on every
    /// device open), `None` leaves the mixer untouched. `hybrid`: the level
    /// the hardware sits at for every volume at or below it (`None`: the
    /// card's maximum). Ignored by `auto` and `hardware`.
    #[serde(default)]
    pub hardware_db: Option<f64>,
    /// Hard ceiling for the master volume, dB (`-120..=0`). No request from
    /// any path (IPC, desktop, config file) makes the engine exceed it.
    #[serde(default)]
    pub max_volume_db: Option<f64>,
    /// What a volume change on our PipeWire sinks (desktop slider, media
    /// keys, `wpctl`) does.
    #[serde(default)]
    pub sink_volume: SinkVolumePolicy,
    /// Protection against app streams (and the sink) suddenly getting louder.
    #[serde(default)]
    pub guard: VolumeGuard,
}

/// Where the master volume is applied.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum VolumeMode {
    /// The DAC's hardware mixer when the card has a usable one, otherwise
    /// the engine's `f64` digital volume.
    #[default]
    Auto,
    /// Always the engine's digital volume. The hardware mixer is left alone,
    /// or set to `hardware_db` when that is given.
    Software,
    /// Hardware mixer only. Without one the engine falls back to digital
    /// volume and reports `hardware_unavailable`.
    Hardware,
    /// The hardware stays at `hardware_db` (its best-sounding level) for every
    /// volume at or below it and the digital volume does the rest; only
    /// louder than that does the hardware go up.
    Hybrid,
}

/// What a volume change on our PipeWire sinks does.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum SinkVolumePolicy {
    /// PipeWire scales the samples itself (`f32`, before the engine) and the
    /// engine does not know about it.
    #[default]
    Ignore,
    /// The `audivane_dsp` sink volume drives the engine volume; PipeWire
    /// passes the samples at unity, and the engine's volume is shown on the
    /// sink. `audivane_direct` is held at unity (its mute is the engine
    /// mute).
    Follow,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VolumeGuard {
    #[serde(default)]
    pub mode: GuardMode,
    /// `limit`: fastest rise, dB per second; 0 lets a rise through at once
    /// (only the caps apply).
    #[serde(default = "default_rise")]
    pub max_rise_db_per_s: f64,
    /// `limit`: the loudest an app stream may be set to, dB (0 = 100 %).
    #[serde(default)]
    pub stream_max_db: Option<f64>,
}

fn default_rise() -> f64 {
    GUARD_DEFAULT_RISE_DB_PER_S
}

impl Default for VolumeGuard {
    fn default() -> Self {
        Self {
            mode: GuardMode::Off,
            max_rise_db_per_s: GUARD_DEFAULT_RISE_DB_PER_S,
            stream_max_db: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum GuardMode {
    #[default]
    Off,
    /// Volume changes of app streams on our sinks, and of the sink itself
    /// (`sink_volume: follow`), are reverted. A stream is held at the volume
    /// it settled at about a second after it appeared (or when the guard was
    /// turned on).
    Lock,
    /// Upward changes are capped (`stream_max_db` for streams,
    /// `max_volume_db` for the sink) and slewed at `max_rise_db_per_s`;
    /// downward changes pass at once.
    Limit,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VolumeStatus {
    /// Effective master volume (after the ceiling and hardware quantization).
    pub volume_db: f64,
    pub balance_db: f64,
    pub muted: bool,
    /// True when the DAC's ALSA hardware mixer controls playback volume.
    pub hardware: bool,
    pub min_db: f64,
    pub max_db: f64,
    /// Configured mode.
    #[serde(default)]
    pub mode: VolumeMode,
    /// Level the hardware mixer is at, when the engine set it.
    #[serde(default)]
    pub hardware_db: Option<f64>,
    /// Master gain applied digitally on the DSP path, dB (0 = unity).
    #[serde(default)]
    pub software_db: f64,
    /// `mode: hardware` (or `hybrid`) was asked for but the card has no usable
    /// hardware volume control; digital volume is used instead.
    #[serde(default)]
    pub hardware_unavailable: bool,
    /// The ceiling in force, if any.
    #[serde(default)]
    pub ceiling_db: Option<f64>,
    #[serde(default)]
    pub sink_volume: SinkVolumePolicy,
    #[serde(default)]
    pub guard: GuardMode,
    /// Volume changes the guard or ceiling blocked or reduced since start.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub blocked: u64,
    /// The most recent of those.
    #[serde(default)]
    pub last_blocked: Option<BlockedVolumeChange>,
    /// This daemon process: 128 random bits in hex, new on every start.
    /// Daemons that predate it omit the field.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub instance: Option<String>,
    /// Increases on every change of the level state (`volume_db`,
    /// `balance_db`, `muted`, effective after the ceiling), from any source,
    /// and of what this status reports as applied (a staged hardware raise
    /// landing, a mixer readback): one revision goes with one reported
    /// level. With `instance`, what `set_volume_guarded`'s
    /// `if_instance`/`if_revision` compare against. Daemons that predate it omit the field.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "number", optional))]
    pub revision: Option<u64>,
}

impl VolumeStatus {
    /// What `cfg` reports before the engine has applied anything: digital
    /// volume over the full range.
    pub fn from_config(cfg: &VolumeConfig) -> Self {
        let ceiling = cfg.max_volume_db;
        let volume_db = ceiling.map_or(cfg.volume_db, |c| cfg.volume_db.min(c));
        Self {
            volume_db,
            balance_db: cfg.balance_db,
            muted: cfg.muted,
            hardware: false,
            min_db: VOLUME_MIN_DB,
            max_db: ceiling.unwrap_or(VOLUME_MAX_DB),
            mode: cfg.mode,
            hardware_db: None,
            software_db: volume_db,
            hardware_unavailable: false,
            ceiling_db: ceiling,
            sink_volume: cfg.sink_volume,
            guard: cfg.guard.mode,
            blocked: 0,
            last_blocked: None,
            instance: None,
            revision: None,
        }
    }
}

/// A volume change that was reverted, capped or slowed down.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BlockedVolumeChange {
    pub source: VolumeSource,
    /// What was asked for, dB.
    pub requested_db: f64,
    /// What was applied instead (immediately), dB.
    pub applied_db: f64,
    pub reason: BlockReason,
    /// Milliseconds since the Unix epoch.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub at_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VolumeSource {
    /// The volume of one of our sinks (desktop slider, media keys, `wpctl`).
    Sink { sink: SinkChoice },
    /// An app stream feeding one of our sinks.
    Stream { node: u32, app: Option<String> },
    /// `set_volume` / `set_config`.
    Control,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// `guard: lock` reverted it.
    Locked,
    /// Above `max_volume_db` (sink, control) or `stream_max_db` (stream).
    Capped,
    /// `guard: limit` is raising it gradually.
    Slewed,
    /// `audivane_direct` stays at unity so it stays bit-perfect.
    DirectBitPerfect,
}

// ---------------------------------------------------------------- varispeed

pub const VARISPEED_MIN_SPEED: f64 = 0.5;
pub const VARISPEED_MAX_SPEED: f64 = 2.0;
pub const VARISPEED_MAX_RAMP_MS: f64 = 10_000.0;
pub const VARISPEED_DEFAULT_RAMP_MS: f64 = 200.0;

/// Variable-speed playback stage (tempo and pitch together, as on a tape
/// machine). Sits in the processor before the chains so one stage feeds
/// both A/B sides. Off by default.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VarispeedConfig {
    /// Whether the stage is enabled (runs on the DSP sink with DSP on).
    #[serde(default)]
    pub enabled: bool,
    /// Target playback speed (`0.5..=2.0`, 1.0 = normal).
    #[serde(default = "default_varispeed_speed")]
    pub speed: f64,
    /// Speed-change ramp duration in milliseconds (`0..=10000`, raised to
    /// the envelope's minimum when shorter; 0 means the minimum).
    #[serde(default = "default_varispeed_ramp_ms")]
    pub ramp_ms: f64,
    /// Keep pitch: the speed changes the tempo only, through a key-locked
    /// time stretch in the stage's place (about 135 ms of delay, refused
    /// where its cost does not fit the budget). Off: tempo and pitch
    /// together, as on a tape machine.
    #[serde(default)]
    pub keep_pitch: bool,
}

fn default_varispeed_speed() -> f64 {
    1.0
}

fn default_varispeed_ramp_ms() -> f64 {
    VARISPEED_DEFAULT_RAMP_MS
}

impl Default for VarispeedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            speed: default_varispeed_speed(),
            ramp_ms: default_varispeed_ramp_ms(),
            keep_pitch: false,
        }
    }
}

/// Why the varispeed speed range is narrower than `0.5..=2.0`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum VarispeedCapReason {
    /// The negotiated period, quantum and processing budget cap the top
    /// speed between 1.0 and 2.0.
    PeriodQuantum,
    /// The quantum is too small for the period to guarantee even unity at
    /// the cycle bound; the range stays capped at 1.0 (`0.5..=1.0`).
    QuantumTooSmall,
    /// The fixed ring allocation limits the top speed for an oversized
    /// period or quantum.
    RingSize,
}

/// Why the varispeed stage is held at unity (`1.0`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum VarispeedHeldReason {
    /// Repeated underfeed events held the stage at unity until the next
    /// speed command.
    Underfeed,
}

/// What the RT overload fallback holds varispeed to, when active.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum VarispeedOverloadHold {
    /// Nothing held.
    #[default]
    None,
    /// Processing budget pinned at 0.5 of the period.
    Budget,
    /// Speed capped at 1.0 as well.
    Unity,
    /// Stage retired while the fallback runs.
    Off,
}

/// Variable-speed stage state, from the audio thread's applied state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VarispeedStatus {
    /// Varispeed is enabled (a stage is installed or on its way).
    pub enabled: bool,
    /// Keep pitch is on (the stage is the key-locked stretch; its delay is
    /// in `latency_frames`).
    #[serde(default)]
    pub keep_pitch: bool,
    /// The last block ran through the stage (DSP sink, DSP on, stage for
    /// the running rate). Enabled at unity (`1.0`) counts as active.
    pub active: bool,
    /// Applied speed at the next output (`1.0` while inactive).
    pub speed: f64,
    /// Speed the stage heads for (the request under `speed_limit`, or `1.0`
    /// while held).
    pub target: f64,
    /// Last requested speed (before capping or holds).
    pub requested: f64,
    /// Upper speed limit for the negotiated timing and overload hold, in
    /// `1.0..=2.0`.
    pub speed_limit: f64,
    /// Why `speed_limit` is below `2.0`, if it is.
    #[serde(default)]
    pub cap_reason: Option<VarispeedCapReason>,
    /// A speed ramp is in progress.
    pub ramping: bool,
    /// A cap-drop fade is in progress.
    pub fading: bool,
    /// Stage delay (including its 5 ms source look-ahead), nominal frames
    /// (`0` while inactive).
    pub latency_frames: f64,
    /// Whether `latency_frames` is exact (`false` during and right after a
    /// ramp, while it reports the constant-speed estimate).
    pub latency_exact: bool,
    /// Why the speed is held at unity, if it is.
    #[serde(default)]
    pub held: Option<VarispeedHeldReason>,
    /// Overload fallback hold on varispeed (`none`, `budget`, `unity`,
    /// `off`).
    #[serde(default)]
    pub overload_hold: VarispeedOverloadHold,
    /// Cumulative underfeed events (ring ran short of planned source
    /// frames).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub underfeed_events: u64,
    /// Cumulative zero-padded underfeed frames.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub underfeed_frames: u64,
    /// Cumulative graph cycles that delivered fewer frames than the quantum
    /// while the watermark schedule ran.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub short_cycles: u64,
    /// Cumulative underfeed gaps faded over on the audio thread.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub gap_fades: u64,
    /// Why the stage could not be built for the running rate (e.g. static
    /// budget exceeded after a rate change), if any.
    #[serde(default)]
    pub error: Option<String>,
}

impl VarispeedStatus {
    /// What `cfg` reports before the engine has published an applied
    /// snapshot (e.g. a mock or waiting engine).
    pub fn from_config(cfg: &VarispeedConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            keep_pitch: cfg.keep_pitch,
            active: false,
            speed: 1.0,
            target: 1.0,
            requested: cfg.speed,
            speed_limit: VARISPEED_MAX_SPEED,
            cap_reason: None,
            ramping: false,
            fading: false,
            latency_frames: 0.0,
            latency_exact: true,
            held: None,
            overload_hold: VarispeedOverloadHold::None,
            underfeed_events: 0,
            underfeed_frames: 0,
            short_cycles: 0,
            gap_fades: 0,
            error: None,
        }
    }
}

// ---------------------------------------------------------------- outputs

/// Most followers (multi-device).
pub const MAX_FOLLOWERS: usize = 3;
/// A follower's trim range, ms (±).
pub const FOLLOWER_MAX_TRIM_MS: f64 = 50.0;
/// The master's speed cap while followers are configured: a follower plays
/// at `speed · (1 + ε)` with `|ε|` up to 0.2 %, within the stage's 2.0.
pub const FOLLOWED_MAX_SPEED: f64 = 1.996;
/// The most the master is delayed to align its followers, ms. A follower
/// that needs more plays late by the rest (`unaligned`).
pub const MAX_ALIGNMENT_MS: f64 = 30.0;

/// Multi-device output: the engine's device (the master) plays as always;
/// each follower plays the same source on another DAC, in sync, with its
/// own instance of the master's chain. Off by default (no followers).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputsConfig {
    /// At most [`MAX_FOLLOWERS`], each on its own device, never the
    /// master's.
    #[serde(default)]
    pub followers: Vec<FollowerConfig>,
}

/// One follower (`outputs.followers[]`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FollowerConfig {
    /// ALSA device, e.g. `hw:CARD=USB,DEV=0`.
    pub device: String,
    /// Heard. Disabled, it keeps playing in sync, muted (it holds its
    /// device).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Its delay behind the master, ms (±[`FOLLOWER_MAX_TRIM_MS`]; negative
    /// plays it earlier): the DACs' own latencies, speaker distance
    /// (1 ms ≈ 34 cm).
    #[serde(default)]
    pub trim_ms: f64,
    /// Its digital volume, dB (`-120..=0`), after the chain's own volume.
    #[serde(default)]
    pub volume_db: f64,
}

impl FollowerConfig {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            enabled: true,
            trim_ms: 0.0,
            volume_db: 0.0,
        }
    }
}

/// A follower's state.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FollowerState {
    /// Not running (the engine is stopped, or it could not start: see
    /// `error`).
    #[default]
    Off,
    /// Opening its device or building its chain.
    Opening,
    /// Playing, not in sync yet: muted.
    Locking,
    /// In sync and heard.
    Locked,
    /// In sync, not heard: disabled, or the master plays Direct or with
    /// DSP off.
    Muted,
    /// In sync and heard, but later than the master by `offset_ms`: its
    /// path needs more than the master's [`MAX_ALIGNMENT_MS`] of alignment.
    /// A smaller period on the follower brings it back.
    Unaligned,
    /// Its device is gone or failed; it reopens when the device is back.
    Lost,
}

/// One follower's status (`status.outputs.followers[]`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FollowerStatus {
    pub device: String,
    pub enabled: bool,
    pub trim_ms: f64,
    pub volume_db: f64,
    pub state: FollowerState,
    /// How much later than the master (plus its trim) it plays, ms: 0 when
    /// aligned, more when `unaligned`.
    pub offset_ms: f64,
    /// Its clock against the master's, ppm (the drift loop's estimate).
    pub ppm: f64,
    /// The loop's filtered phase error, source frames.
    pub phase_error_frames: f64,
    /// The loop's bandwidth, Hz (it narrows as it locks).
    pub bandwidth_hz: f64,
    /// Phase jumps the loop corrected by skipping frames.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub skips: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub xruns: u64,
    /// Its own path, ms: from taking a source frame to playing it.
    pub latency_ms: f64,
    /// The rate and period its device opened at (0 while closed).
    pub device_rate: u32,
    pub period: u32,
    #[serde(default)]
    pub error: Option<String>,
}

/// Multi-device status (`status.outputs`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputsStatus {
    /// The master's added delay that aligns the followers, ms (0 without
    /// followers, at most [`MAX_ALIGNMENT_MS`]).
    pub alignment_ms: f64,
    /// Every configured follower, in config order.
    pub followers: Vec<FollowerStatus>,
}

impl OutputsStatus {
    /// What `cfg` reports before the engine runs its followers.
    pub fn from_config(cfg: &OutputsConfig) -> Self {
        Self {
            alignment_ms: 0.0,
            followers: cfg
                .followers
                .iter()
                .map(|f| FollowerStatus {
                    device: f.device.clone(),
                    enabled: f.enabled,
                    trim_ms: f.trim_ms,
                    volume_db: f.volume_db,
                    state: FollowerState::Off,
                    offset_ms: 0.0,
                    ppm: 0.0,
                    phase_error_frames: 0.0,
                    bandwidth_hz: 0.0,
                    skips: 0,
                    xruns: 0,
                    latency_ms: 0.0,
                    device_rate: 0,
                    period: 0,
                    error: None,
                })
                .collect(),
        }
    }
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
    /// Variable-speed playback stage (before the chains; one stage feeds
    /// both A/B sides).
    #[serde(default)]
    pub varispeed: VarispeedConfig,
    /// What the music does while the output device is gone (unplugged).
    #[serde(default)]
    pub device_lost: DeviceLostAction,
    /// Multi-device output: followers on other DACs (none by default).
    #[serde(default)]
    pub outputs: OutputsConfig,
}

/// What the music does while the configured output device is gone
/// (unplugged, or missing at start-up). Audivane's sinks stay up either
/// way, on the engine's own clock, so streams never move to another output;
/// playback goes back to the device, faded in, once it returns.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum DeviceLostAction {
    /// Pause the players that are playing (MPRIS) when the device goes.
    /// They are not resumed when it returns. A player without MPRIS keeps
    /// playing into silence.
    #[default]
    Pause,
    /// Keep playing: the sinks discard the audio and players keep
    /// progressing as usual.
    KeepPlaying,
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
            varispeed: VarispeedConfig::default(),
            device_lost: DeviceLostAction::default(),
            outputs: OutputsConfig::default(),
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
/// Minimum PipeWire graph quantum accepted by control validation and the engine.
pub const MIN_QUANTUM: u32 = 32;
/// Maximum PipeWire graph quantum accepted by control validation and the engine.
pub const MAX_QUANTUM: u32 = 8192;

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
    /// Who converts the rate for Direct and DSP off when the DAC does not
    /// play the graph rate natively.
    #[serde(default)]
    pub bypass_resampler: BypassResampler,
}

impl Default for RatePolicy {
    fn default() -> Self {
        Self {
            follow_source: true,
            allowed_rates: default_rates(),
            default_rate: default_rate(),
            conflict: RateConflict::default(),
            bypass_resampler: BypassResampler::default(),
        }
    }
}

/// Who converts the rate for Direct and DSP off (the bit-perfect paths)
/// when the DAC does not play the graph rate natively. A rate the DAC plays
/// natively always stays x1 and bit-perfect.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum BypassResampler {
    /// The graph stays at the source rate and the engine converts it to the
    /// native rate PipeWire would have picked (a fixed L/M, the recommended
    /// filter design). Falls back to `pipewire` for a rate it cannot build.
    #[default]
    Engine,
    /// PipeWire moves the graph to a native rate and resamples the stream.
    Pipewire,
}

/// The engine leg of [`BypassResampler`] in status.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BypassResamplerStatus {
    /// The configured mode.
    pub mode: BypassResampler,
    /// The engine converts the rate right now (Direct or DSP off, at a
    /// ratio other than x1).
    pub active: bool,
    /// Why the engine leg is not active although `mode` is `engine` and the
    /// path is Direct or DSP off away from a native rate (the PipeWire
    /// fallback), or why it is waiting.
    #[serde(default)]
    pub reason: Option<String>,
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
    /// How side B is described: chain A plus a change, or a full chain.
    pub b: AbB,
    /// Free text naming B (`"Crossfeed off"`), set by the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    /// Chain B as the daemon materialised it from [`Self::b`] and chain A.
    /// Read-only: the daemon rebuilds it on every config edit and load, so
    /// a value sent in is overwritten.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(as = "Option<ChainConfig>", optional))]
    pub chain_b: ChainConfig,
    #[serde(default)]
    pub level: LevelMatch,
    /// Cut applied equally to both sides after level matching, so B's own
    /// boosts do not clip (default `auto`).
    #[serde(default)]
    pub headroom: AbHeadroom,
}

impl AbConfig {
    /// A/B with `b` and default level matching and headroom. `chain_b` is
    /// materialised by the daemon.
    pub fn new(b: AbB) -> Self {
        Self {
            b,
            label: None,
            chain_b: ChainConfig::default(),
            level: LevelMatch::default(),
            headroom: AbHeadroom::default(),
        }
    }

    /// A/B with a full chain B, already materialised as `chain`.
    pub fn chain(chain: ChainConfig) -> Self {
        Self {
            chain_b: chain.clone(),
            ..Self::new(AbB::Chain(Box::new(chain)))
        }
    }
}

/// Side B of an A/B comparison.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum AbB {
    /// A JSON merge patch (RFC 7386) on chain A: B follows every later edit
    /// to A outside the patched fields. `upsampler.target`, `limiter` and
    /// `output` may not be patched; arrays are replaced whole.
    Patch(serde_json::Value),
    /// A full chain (a stored preset). Its own filter design is kept; the
    /// upsampler target, limiter and output follow chain A.
    Chain(Box<ChainConfig>),
}

/// Frequency weighting of the analytic level match.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum LevelWeighting {
    /// BS.1770 K-weighting, as LUFS (default).
    #[default]
    K,
    /// IEC 61672 A-weighting.
    A,
}

/// How B's level offset `o` (dB) is found. Whatever the mode, nothing is
/// boosted: the engine applies `gain_a = min(0, -o)` and
/// `gain_b = gain_a + o`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LevelMatch {
    /// Analytic offset from both chains' responses over a pink-noise
    /// reference (default, K-weighted).
    Analytic {
        #[serde(default)]
        weighting: LevelWeighting,
    },
    /// Fixed offset for B, in dB.
    Manual {
        offset_db: f64,
    },
    /// An offset measured on the music and locked by `ab_measure`: B's
    /// K-weighted loudness (BS.1770 gating on A's blocks) against A's,
    /// over `seconds` of gated audio. It never moves until replaced.
    Measured {
        offset_db: f64,
        seconds: f64,
    },
    Off,
}

impl Default for LevelMatch {
    fn default() -> Self {
        Self::Analytic {
            weighting: LevelWeighting::default(),
        }
    }
}

/// A/B's shared headroom.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AbHeadroom {
    /// From each side's predicted worst-case peak gain after matching,
    /// capped at [`AB_HEADROOM_MAX_DB`] (default).
    #[default]
    Auto,
    /// A fixed cut, 0 to [`AB_HEADROOM_MAX_DB`] dB.
    Fixed { db: f64 },
}

/// Largest A/B shared headroom, in dB.
pub const AB_HEADROOM_MAX_DB: f64 = 12.0;

/// Range of `ab_measure`'s `max_seconds` (seconds of gated audio).
pub const AB_MEASURE_SECONDS: std::ops::RangeInclusive<f64> = 10.0..=300.0;
/// `ab_measure`'s `max_seconds` when the call gives none.
pub const AB_MEASURE_DEFAULT_SECONDS: f64 = 60.0;

/// A running measured level match (`ab_measure`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AbMeasuring {
    /// Seconds of audio measured so far: what passed A's gates (silence
    /// and quiet passages do not count, so a pause waits).
    pub seconds: f64,
    /// The running offset for B (dB), `None` until a block passes A's
    /// gates. It is not applied until it locks.
    pub offset_db: Option<f64>,
    /// The lock rule holds (enough seconds and a settled offset): it locks
    /// on the next tick.
    pub stable: bool,
    /// It locks at this many seconds at the latest.
    pub max_seconds: f64,
}

/// Overs counted to each side since the last `set_ab` or `ab_select`: an
/// over starts on a frame where the mix after volume exceeds the limiter
/// ceiling (full scale without a limiter), and counts to A and B in
/// proportion to the crossfade position at that frame.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AbOvers {
    pub a: f64,
    pub b: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AbStatus {
    pub enabled: bool,
    pub current: AbSide,
    /// B's level offset `o` (dB): B plus `o` is as loud as A.
    pub offset_db: f64,
    /// Gain applied to A (dB, <= 0), before the shared headroom.
    #[serde(default)]
    pub gain_a_db: f64,
    /// Gain applied to B (dB, <= 0), before the shared headroom;
    /// `gain_b_db - gain_a_db == offset_db`.
    #[serde(default)]
    pub gain_b_db: f64,
    /// Shared headroom cut from both sides (dB, >= 0).
    #[serde(default)]
    pub headroom_db: f64,
    #[serde(default)]
    pub overs: AbOvers,
    /// The level mode in force (`measured` once a measurement locked).
    #[serde(default)]
    pub level: LevelMatch,
    /// The measurement in progress, `None` when none runs.
    #[serde(default)]
    pub measuring: Option<AbMeasuring>,
    /// Why the last measurement ended without a lock (no match, the track
    /// or rate changed, the compared chains changed). Cleared when one
    /// starts or locks; `None` after a lock or a cancel.
    #[serde(default)]
    pub measure_error: Option<String>,
    /// Why analytic level matching could not be computed (then
    /// `offset_db` is 0). `None` when the offset is valid.
    #[serde(default)]
    pub error: Option<String>,
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

/// Rate-specific interpretation of an enabled tilt stage (`tilt` and
/// `tilt_high`): the first enabled section's start, the last one's effective
/// end. This describes the requested chain at the current graph rate, even
/// while idle.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TiltStatus {
    pub sample_rate: u32,
    pub effective_start_hz: f64,
    pub effective_end_hz: f64,
    pub clamp_note: Option<String>,
    /// An unusable effective band or pivot, or a design that fails at this
    /// rate. A chain build returns this error; the bounds alone do not imply
    /// that a filter is running.
    pub error: Option<String>,
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
    /// Integer upsampling ratio; 0 while a rational ratio runs (see
    /// [`Self::rate_ratio`]).
    pub ratio: u32,
    /// DAC rate / graph rate, reduced (`{up: 160, down: 147}` for 44.1k →
    /// 48k, `{up: 1, down: 2}` for 192k → 96k).
    #[serde(default)]
    pub rate_ratio: crate::RateRatio,
    pub quantum: u32,
    /// End-to-end added latency (DSP + buffers), ms: the ALSA output buffer
    /// delay plus [`Self::filter_latency_ms`].
    pub latency_ms: f64,
    /// The part of [`Self::latency_ms`] that is filter delay in the active
    /// chain (upsampler, convolution and limiter), ms. 0 on the Direct sink or with
    /// DSP off, where the chain is not in the path.
    #[serde(default)]
    pub filter_latency_ms: f64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub xruns: u64,
    /// DSP time per cycle / cycle period, 0..1.
    pub dsp_load: f64,
    pub dsp_enabled: bool,
    /// Enabled tilt stage in chain A (both sections of a two-section tilt),
    /// interpreted at graph_rate.
    #[serde(default)]
    pub eq_tilt: Option<TiltStatus>,
    /// Enabled tilt in chain B, when A/B is configured.
    #[serde(default)]
    pub eq_tilt_b: Option<TiltStatus>,
    /// Which sink currently feeds the DAC.
    pub active_sink: Option<SinkChoice>,
    pub passthrough: Passthrough,
    pub ab: Option<AbStatus>,
    /// Active preset, when chain A was saved to or loaded from one.
    #[serde(default)]
    pub preset: Option<PresetState>,
    #[serde(default)]
    pub volume: Option<VolumeStatus>,
    /// What the configured output device accepts, probed when it was opened;
    /// kept when the device turns out unusable (`rates` is empty then). `None`
    /// before the first open, or when the probe or the open failed.
    #[serde(default)]
    pub device_caps: Option<DeviceCapabilities>,
    /// Graph rates the engine offers PipeWire: `rate.allowed_rates` the device
    /// can play, directly or upsampled. Streams at other rates are resampled
    /// by PipeWire. Empty while no device is open.
    #[serde(default)]
    pub effective_rates: Vec<u32>,
    /// The rates Audivane Direct runs the graph at: the subset of
    /// [`Self::effective_rates`] the device plays natively, plus, with
    /// `rate.bypass_resampler: engine`, the rates the engine converts to a
    /// native one. A Direct stream at another rate moves the graph to one of
    /// them (PipeWire resamples, passthrough reports `resampled`). Empty while
    /// no device is open, or when no effective rate is native.
    #[serde(default)]
    pub direct_rates: Vec<u32>,
    /// Every engine graph rate ([`crate::ALLOWED_RATES`], allowed or not)
    /// the device plays with the current settings: natively, through the
    /// DSP chain's ratio ladder (decimation and fractional ratios
    /// included), or with the engine bypass resampler. A rate in
    /// `rate.allowed_rates` but not here is resampled by PipeWire on this
    /// device. Empty while no device is open, or without a probe.
    #[serde(default)]
    pub playable_rates: Vec<u32>,
    /// Rate used when idle / when following is off, after the same
    /// restriction (`rate.default_rate` when the device supports it).
    #[serde(default)]
    pub effective_default_rate: Option<u32>,
    /// Bit depth dither targets: `chain.output.bits` capped at the container
    /// width of [`Self::format`]. `None` while no device is open.
    #[serde(default)]
    pub dither_bits: Option<u32>,
    /// The limiter's held automatic headroom, dB (`>= 0`), for clients without
    /// the `meters` topic; 0 when none is held or the limiter, DSP or playback
    /// is off. Same value as `meters.limiter.headroom_db`.
    #[serde(default)]
    pub limiter_held_db: f64,
    /// The session's default output when it is not one of Audivane's sinks
    /// (its `node.description`): where a stream left unrouted plays in
    /// per-app routing. `None` in system-wide routing (the default is
    /// Audivane DSP) or when there is no other output.
    #[serde(default)]
    pub normal_output: Option<String>,
    /// The fallback in effect after a DSP overload (see [`OverloadReport`]);
    /// `None` when the engine runs the config as saved.
    #[serde(default)]
    pub overload: Option<OverloadReport>,
    /// The engine bypass resampler ([`RatePolicy::bypass_resampler`]):
    /// whether it converts the rate now, or why not. `None` while no device
    /// is open.
    #[serde(default)]
    pub bypass_resampler: Option<BypassResamplerStatus>,
    /// Peak RT processing load per period over the last one to two seconds
    /// (share of one period; 1.0 = all of real time).
    #[serde(default)]
    pub rt_load_max: f64,
    /// Variable-speed stage state from the audio thread's applied state.
    /// `None` from daemons that predate varispeed.
    #[serde(default)]
    pub varispeed: Option<VarispeedStatus>,
    /// The engine's design and CPU limits on this machine. `None` from
    /// daemons that predate it.
    #[serde(default)]
    pub limits: Option<EngineLimits>,
    /// The configured output device is missing (unplugged, or absent at
    /// start-up): `state` is `error` and `error` says so, the sinks stay up
    /// into silence and the engine opens the device as soon as it is back (a
    /// device that keeps failing to open is retried).
    #[serde(default)]
    pub waiting_for_device: bool,
    /// Multi-device: the master's alignment and each follower's state.
    #[serde(default)]
    pub outputs: OutputsStatus,
}

/// The limits the daemon holds upsampler designs to on this machine. The design
/// workspace depends on the machine (a quarter of its RAM, 1.5..=4 GiB) and is
/// fixed for the daemon's lifetime.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct EngineLimits {
    /// Longest kernel at an integer ratio, output-rate taps.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub max_kernel_taps: u64,
    /// Longest intermediate-phase design with `phase_mix` strictly between
    /// 0 and 1 (its kernel can grow to 4x the design length).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub intermediate_max_taps: u64,
    /// The most memory designing one filter may take, bytes.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub design_workspace_bytes: u64,
    /// The share of one core the upsampler may take: a client change
    /// above it is refused ("Too heavy for this CPU ...").
    pub cpu_share: f64,
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
/// interleaved read/write access. Probed only at rates the engine could ask
/// for (allowed rates times upsampling ratios, then the x32/x64 rates), as
/// (rate, format) pairs: USB devices often tie formats to rates.
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
    /// The engine resamples by a non-integer ratio (rational upsampling or
    /// decimation) to reach the DAC rate.
    RateConversion {
        graph_rate: u32,
        output_rate: u32,
    },
    /// The variable-speed stage is active on the DSP path (unity included).
    /// `speed` is the applied speed from the audio thread.
    Varispeed {
        speed: f64,
    },
    /// Sink or stream software volume is not exactly 1.0, or muted.
    /// `volume` is linear; 0 when muted.
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
    /// Streams are linked, but the output is not writing them (no format
    /// negotiated, a reopen in flight, an error): nothing reaches the DAC.
    NoOutput,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Meters {
    /// Post-volume limiter state; event count is cumulative since daemon start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub limiter: Option<crate::LimiterStatus>,
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
    /// The level the stream plays at, in dB: its loudest channel volume
    /// times its own master volume (what `set_stream_volume` sets). Absent
    /// until the stream reports its volume, and from daemons that predate
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub volume_db: Option<f64>,
}

/// One stream's level after `set_stream_volume`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct StreamVolume {
    pub node: u32,
    /// The level the stream plays at now, in dB (loudest channel times the
    /// app's own master volume).
    pub volume_db: f64,
    pub muted: bool,
    /// The loudest this call can set the stream to now: the lower of
    /// `guard.stream_max_db` (0 dB without one) and the app's own volume
    /// (channel volumes never go above 100 %).
    pub max_db: f64,
    /// Why the level is below the request, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub limited_by: Option<StreamVolumeLimit>,
}

/// What kept a `set_stream_volume` level below the request.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum StreamVolumeLimit {
    /// `volume.guard.stream_max_db`.
    StreamMax,
    /// The app's own master volume (Audivane does not write it).
    AppVolume,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct NowPlaying {
    /// MPRIS bus name, e.g. `org.mpris.MediaPlayer2.example`.
    pub player: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub status: String,
    /// Native file rate/bit depth if the player exposes it (non-standard
    /// MPRIS metadata keys; often absent).
    pub native_rate: Option<u32>,
    pub native_bits: Option<u32>,
    /// The player's `mpris:trackid`, when it gives one: tells apart tracks
    /// whose tags are the same (or missing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub track_id: Option<String>,
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
    fn ab_b_is_a_patch_or_a_chain() {
        let ab: AbConfig = serde_json::from_value(serde_json::json!({
            "b": {"patch": {"crossfeed": {"enabled": false}}},
            "label": "Crossfeed off",
            "chain_b": {"eq": {"preamp_db": -3.0}}
        }))
        .unwrap();
        assert_eq!(
            ab.b,
            AbB::Patch(serde_json::json!({"crossfeed": {"enabled": false}}))
        );
        assert_eq!(ab.label.as_deref(), Some("Crossfeed off"));
        // Clients read the materialised chain back.
        assert_eq!(ab.chain_b.eq.as_ref().unwrap().preamp_db, -3.0);
        let v = serde_json::to_value(&ab).unwrap();
        assert!(v["chain_b"].is_object());
        let ab: AbConfig = serde_json::from_value(serde_json::json!({"b": {"patch": {}}})).unwrap();
        assert_eq!(ab.chain_b, ChainConfig::default());
        assert!(serde_json::from_value::<AbConfig>(serde_json::json!({"b": {"x": {}}})).is_err());
    }

    #[test]
    fn ab_level_and_headroom_defaults() {
        let ab: AbConfig = serde_json::from_value(serde_json::json!({
            "b": {"patch": {}},
            "level": {"mode": "analytic"}
        }))
        .unwrap();
        assert_eq!(
            ab.level,
            LevelMatch::Analytic {
                weighting: LevelWeighting::K
            }
        );
        assert_eq!(ab.headroom, AbHeadroom::Auto);
        let ab: AbConfig = serde_json::from_value(serde_json::json!({"b": {"chain": {}}})).unwrap();
        assert_eq!(ab.level, LevelMatch::default());
        assert_eq!(ab.label, None);
        let v = serde_json::to_value(AbConfig {
            level: LevelMatch::Analytic {
                weighting: LevelWeighting::A,
            },
            headroom: AbHeadroom::Fixed { db: 3.0 },
            ..ab
        })
        .unwrap();
        assert_eq!(
            v["level"],
            serde_json::json!({"mode": "analytic", "weighting": "a"})
        );
        assert_eq!(
            v["headroom"],
            serde_json::json!({"mode": "fixed", "db": 3.0})
        );
        // An older status without the new fields still parses.
        let st: AbStatus = serde_json::from_value(serde_json::json!({
            "enabled": true, "current": "b", "offset_db": -1.0
        }))
        .unwrap();
        assert_eq!(
            (st.gain_a_db, st.headroom_db, st.overs),
            (0.0, 0.0, AbOvers::default())
        );
    }

    #[test]
    fn ab_measure_wire_format() {
        let call: Request =
            serde_json::from_str(r#"{"id":1,"method":"ab_measure","params":{"max_seconds":30}}"#)
                .unwrap();
        assert_eq!(
            call.call,
            Call::AbMeasure {
                max_seconds: Some(30.0),
                cancel: false,
                accept: false,
            }
        );
        let cancel = Request {
            id: 2,
            call: Call::AbMeasure {
                max_seconds: None,
                cancel: true,
                accept: false,
            },
        };
        assert_eq!(
            serde_json::to_string(&cancel).unwrap(),
            r#"{"id":2,"method":"ab_measure","params":{"cancel":true}}"#
        );
        let level: LevelMatch = serde_json::from_value(serde_json::json!(
            {"mode": "measured", "offset_db": -0.84, "seconds": 31.5}
        ))
        .unwrap();
        assert_eq!(
            level,
            LevelMatch::Measured {
                offset_db: -0.84,
                seconds: 31.5
            }
        );
    }

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

    /// The targeted config setters, as documented in control-plane.md.
    #[test]
    fn targeted_setter_wire_format() {
        let parse = |line: &str| serde_json::from_str::<Request>(line).unwrap().call;
        let Call::SetOutput(out) =
            parse(r#"{"id":1,"method":"set_output","params":{"bits":24,"dither":"tpdf"}}"#)
        else {
            panic!("set_output")
        };
        assert_eq!(out.bits, 24);
        assert_eq!(
            parse(r#"{"id":2,"method":"set_formats","params":["s32","s24"]}"#),
            Call::SetFormats(vec![SampleFormat::S32, SampleFormat::S24])
        );
        let Call::SetRate(rate) = parse(
            r#"{"id":3,"method":"set_rate","params":{"follow_source":true,"allowed_rates":[44100,48000],"default_rate":48000,"conflict":"keep_current"}}"#,
        ) else {
            panic!("set_rate")
        };
        assert_eq!(rate.allowed_rates, [44_100, 48_000]);
        assert_eq!(
            parse(r#"{"id":4,"method":"set_device","params":{"device":"hw:CARD=DAC,DEV=0"}}"#),
            Call::SetDevice {
                device: "hw:CARD=DAC,DEV=0".into()
            }
        );
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
        let preset = Preset::new("Bass", ChainConfig::neutral());
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
            rate_ratio: crate::RateRatio::ONE,
            quantum: 1024,
            latency_ms: 0.0,
            filter_latency_ms: 0.0,
            xruns: 0,
            dsp_load: 0.0,
            dsp_enabled: true,
            eq_tilt: None,
            eq_tilt_b: None,
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
            playable_rates: Vec::new(),
            effective_default_rate: None,
            dither_bits: None,
            limiter_held_db: 0.0,
            normal_output: None,
            overload: None,
            bypass_resampler: None,
            rt_load_max: 0.0,
            varispeed: None,
            limits: Some(EngineLimits {
                max_kernel_taps: 8_388_608,
                intermediate_max_taps: 2_097_152,
                design_workspace_bytes: 4 << 30,
                cpu_share: 0.9,
            }),
            waiting_for_device: false,
            outputs: OutputsStatus::default(),
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(
            value["limits"],
            serde_json::json!({
                "max_kernel_taps": 8_388_608,
                "intermediate_max_taps": 2_097_152,
                "design_workspace_bytes": 4_294_967_296u64,
                "cpu_share": 0.9,
            })
        );
        // Additive: older daemons omit it.
        let mut old = value.clone();
        old.as_object_mut().unwrap().remove("limits");
        assert!(
            serde_json::from_value::<Status>(old)
                .unwrap()
                .limits
                .is_none()
        );
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

        // New fields default to today's behaviour.
        assert_eq!(cfg.volume.mode, VolumeMode::Auto);
        assert_eq!(cfg.volume.sink_volume, SinkVolumePolicy::Ignore);
        assert_eq!(cfg.volume.guard.mode, GuardMode::Off);
        assert_eq!(cfg.volume.max_volume_db, None);
        let full: VolumeConfig = serde_json::from_str(
            r#"{"volume_db":-20,"mode":"hybrid","hardware_db":-10,"max_volume_db":-6,
                "sink_volume":"follow","guard":{"mode":"limit","stream_max_db":-12}}"#,
        )
        .unwrap();
        assert_eq!(full.mode, VolumeMode::Hybrid);
        assert_eq!(full.hardware_db, Some(-10.0));
        assert_eq!(full.sink_volume, SinkVolumePolicy::Follow);
        assert_eq!(full.guard.mode, GuardMode::Limit);
        assert_eq!(full.guard.max_rise_db_per_s, GUARD_DEFAULT_RISE_DB_PER_S);
        assert_eq!(full.guard.stream_max_db, Some(-12.0));

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
            limiter: None,
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
            rate_ratio: crate::RateRatio::integer(4),
            taps: 4096,
            phase: crate::FilterPhase::Linear,
            filter_window: crate::WindowKind::Kaiser,
            effective_cutoff: 1.0,
            effective_kaiser_beta: 14.0,
            sample_leak_db: -40.0,
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
            preset: None,
            design: Some(crate::StoredDesign::new(crate::FilterDesign::Interpolator(
                crate::design::InterpMethod::Cosine,
            ))),
            plan: Some(crate::design::DesignPlan {
                grid_up: 4,
                grid_down: 1,
                grid_taps: 4096,
                grid_taps_exact: true,
                workspace_bytes: 1 << 20,
                resident_bytes: 1 << 16,
                cpu_load: Some(0.01),
                bounded_peak_db: Some(3.5),
            }),
            device_landings: vec![SourceLanding {
                source_rate: 44_100,
                ratio: crate::RateRatio::ONE,
                requested: crate::RateRatio::integer(4),
                converted_to: Some(48_000),
                best_for_dac: Some(BestForDac {
                    target: crate::RateTarget::MaxRateAny(768_000),
                    ratio: crate::RateRatio::new(160, 147),
                }),
            }],
        };
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["ratio"], 4);
        assert_eq!(value["phase"], "linear");
        assert_eq!(value["design"]["kind"], "interpolator");
        assert_eq!(value["plan"]["grid_up"], 4);
        assert_eq!(
            serde_json::from_value::<UpsamplerResponse>(value).unwrap(),
            response
        );
    }

    #[test]
    fn design_rpc_wire_formats() {
        let call: Request = serde_json::from_str(
            r#"{"id":1,"method":"resolve_design","params":{"preset":"recommended"}}"#,
        )
        .unwrap();
        assert_eq!(
            call.call,
            Call::ResolveDesign {
                preset: "recommended".into()
            }
        );
        let call: Request = serde_json::from_str(
            r#"{"id":2,"method":"check_design","params":{"design":{"kind":"interpolator","method":"cosine"}}}"#,
        )
        .unwrap();
        let Call::CheckDesign {
            design,
            target,
            source_rate,
        } = call.call
        else {
            panic!("{call:?}")
        };
        assert_eq!((target, source_rate), (None, None));
        assert_eq!(design.design.kind_name(), "interpolator");
        let call: Request = serde_json::from_str(
            r#"{"id":3,"method":"check_design","params":{"design":{"kind":"interpolator","method":"cosine"},"target":{"ratio":4},"source_rate":48000}}"#,
        )
        .unwrap();
        assert!(matches!(
            call.call,
            Call::CheckDesign {
                target: Some(crate::RateTarget::Ratio(4)),
                source_rate: Some(48_000),
                ..
            }
        ));
        let check = DesignCheck {
            errors: vec![crate::design::FieldError {
                path: "design.window.beta".into(),
                message: "must be between 0 and 40 (got 41)".into(),
            }],
            plans: vec![RatePlan {
                source_rate: 44_100,
                ratio: crate::RateRatio::integer(8),
                plan: crate::design::DesignPlan {
                    grid_up: 8,
                    grid_down: 1,
                    grid_taps: 0,
                    grid_taps_exact: true,
                    workspace_bytes: 0,
                    resident_bytes: 0,
                    cpu_load: None,
                    bounded_peak_db: None,
                },
            }],
        };
        let value = serde_json::to_value(&check).unwrap();
        assert_eq!(value["errors"][0]["path"], "design.window.beta");
        assert_eq!(value["plans"][0]["ratio"]["up"], 8);
        assert!(value["plans"][0]["plan"].get("cpu_load").is_none());
        assert_eq!(serde_json::from_value::<DesignCheck>(value).unwrap(), check);
    }

    #[test]
    fn device_capabilities_helpers() {
        let caps = DeviceCapabilities {
            device: "hw:CARD=SPK,DEV=0".into(),
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

    #[test]
    fn varispeed_config_defaults_and_roundtrip() {
        let cfg: EngineConfig = serde_json::from_str(r#"{"device":"hw:0,0"}"#).unwrap();
        assert_eq!(cfg.varispeed, VarispeedConfig::default());
        assert!(!cfg.varispeed.enabled);
        assert_eq!(cfg.varispeed.speed, 1.0);
        assert_eq!(cfg.varispeed.ramp_ms, VARISPEED_DEFAULT_RAMP_MS);

        let partial: VarispeedConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(partial.enabled);
        assert_eq!(partial.speed, 1.0);
        assert_eq!(partial.ramp_ms, VARISPEED_DEFAULT_RAMP_MS);
        assert!(!partial.keep_pitch);

        let full = VarispeedConfig {
            enabled: true,
            speed: 1.02,
            ramp_ms: 500.0,
            keep_pitch: true,
        };
        let encoded = serde_json::to_string(&full).unwrap();
        assert_eq!(
            serde_json::from_str::<VarispeedConfig>(&encoded).unwrap(),
            full
        );
    }

    #[test]
    fn outputs_config_defaults_and_set_follower_wire_format() {
        let cfg: EngineConfig = serde_json::from_str(r#"{"device":"hw:0,0"}"#).unwrap();
        assert!(cfg.outputs.followers.is_empty());
        let cfg: EngineConfig = serde_json::from_str(
            r#"{"device":"hw:0,0","outputs":{"followers":[{"device":"hw:CARD=USB,DEV=0"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            cfg.outputs.followers,
            vec![FollowerConfig::new("hw:CARD=USB,DEV=0")]
        );
        let req: Request = serde_json::from_str(
            r#"{"id":1,"method":"set_follower","params":{"device":"hw:CARD=USB,DEV=0","trim_ms":1.5}}"#,
        )
        .unwrap();
        assert_eq!(
            req.call,
            Call::SetFollower {
                device: "hw:CARD=USB,DEV=0".into(),
                remove: false,
                enabled: None,
                trim_ms: Some(1.5),
                volume_db: None,
            }
        );
        let back = serde_json::to_value(&req).unwrap();
        assert_eq!(
            back["params"],
            serde_json::json!({"device": "hw:CARD=USB,DEV=0", "trim_ms": 1.5})
        );
        assert!(
            serde_json::from_str::<Request>(
                r#"{"id":2,"method":"set_follower","params":{"device":"hw:1,0","trim_ms":null}}"#
            )
            .is_err()
        );
        let st = OutputsStatus::from_config(&cfg.outputs);
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["followers"][0]["state"], "off");
        assert_eq!(v["alignment_ms"], 0.0);
    }

    #[test]
    fn varispeed_rpc_status_and_passthrough_wire_format() {
        let req: Request = serde_json::from_str(
            r#"{"id":1,"method":"set_varispeed","params":{"speed":1.02,"ramp_ms":500}}"#,
        )
        .unwrap();
        assert_eq!(
            req.call,
            Call::SetVarispeed {
                enabled: None,
                speed: Some(1.02),
                ramp_ms: Some(500.0),
                keep_pitch: None,
            }
        );

        let off: Request =
            serde_json::from_str(r#"{"id":2,"method":"set_varispeed","params":{"enabled":false}}"#)
                .unwrap();
        assert_eq!(
            off.call,
            Call::SetVarispeed {
                enabled: Some(false),
                speed: None,
                ramp_ms: None,
                keep_pitch: None,
            }
        );

        let empty: Request =
            serde_json::from_str(r#"{"id":3,"method":"set_varispeed","params":{}}"#).unwrap();
        assert_eq!(
            empty.call,
            Call::SetVarispeed {
                enabled: None,
                speed: None,
                ramp_ms: None,
                keep_pitch: None,
            }
        );

        // Explicit `null` must be rejected rather than silently treated as omitted.
        for bad in [
            r#"{"id":4,"method":"set_varispeed","params":{"enabled":true,"speed":null}}"#,
            r#"{"id":5,"method":"set_varispeed","params":{"ramp_ms":null}}"#,
            r#"{"id":6,"method":"set_varispeed","params":{"enabled":null}}"#,
            r#"{"id":7,"method":"set_varispeed","params":{"keep_pitch":null}}"#,
        ] {
            assert!(
                serde_json::from_str::<Request>(bad).is_err(),
                "explicit null must be rejected: {bad}"
            );
        }

        assert_eq!(
            serde_json::from_str::<Request>(
                r#"{"id":8,"method":"set_varispeed","params":{"keep_pitch":true}}"#
            )
            .unwrap()
            .call,
            Call::SetVarispeed {
                enabled: None,
                speed: None,
                ramp_ms: None,
                keep_pitch: Some(true),
            }
        );

        let brk = PassthroughBreak::Varispeed { speed: 1.05 };
        let brk_json = serde_json::to_string(&brk).unwrap();
        assert_eq!(brk_json, r#"{"reason":"varispeed","speed":1.05}"#);
        assert_eq!(
            serde_json::from_str::<PassthroughBreak>(&brk_json).unwrap(),
            brk
        );

        let st = VarispeedStatus {
            enabled: true,
            keep_pitch: true,
            active: true,
            speed: 1.02,
            target: 1.05,
            requested: 1.5,
            speed_limit: 1.05,
            cap_reason: Some(VarispeedCapReason::PeriodQuantum),
            ramping: true,
            fading: false,
            latency_frames: 500.25,
            latency_exact: false,
            held: Some(VarispeedHeldReason::Underfeed),
            overload_hold: VarispeedOverloadHold::Budget,
            underfeed_events: 3,
            underfeed_frames: 120,
            short_cycles: 2,
            gap_fades: 1,
            error: None,
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["cap_reason"], "period_quantum");
        assert_eq!(v["held"], "underfeed");
        assert_eq!(v["overload_hold"], "budget");
        assert_eq!(serde_json::from_value::<VarispeedStatus>(v).unwrap(), st);
    }
}
