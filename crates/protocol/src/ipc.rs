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
    /// Set chain A's convolution items. → `null`, or `{"notes": [..]}`
    /// with the convolution warnings ([`Status::conv_warnings`]) when there
    /// are any.
    SetConvolution(
        #[serde(deserialize_with = "crate::deserialize_conv_list")] Vec<ConvolutionConfig>,
    ),
    /// Set the convolution guards (`convolution_safety`: boost cap,
    /// subsonic limit). → `null`, or `{"notes": [..]}` as
    /// [`Self::SetConvolution`].
    SetConvolutionSafety(ConvolutionSafety),
    /// → `null`
    SetUpsampler(UpsamplerConfig),
    /// Set pitch shifting for chain A. → `null`
    SetPitch(PitchConfig),
    /// Set or clear the harmonic distortion stage of chain A. → `null`
    SetHarmonics(Option<HarmonicsConfig>),
    /// The built-in harmonics layer presets. → `Vec<HarmonicsPreset>`
    HarmonicsPresets,
    /// The built-in harmonics system presets (whole stages). →
    /// `Vec<HarmonicsSystem>`
    HarmonicsSystems,
    /// Readouts of a harmonics config (default: chain A's; off when it has
    /// none) at the current output rate (else 96 kHz): level change, peak
    /// gain, and harmonics, THD and GedLee Gm at 0 and −18 dBFS (with
    /// `sweep`, also −40..=0 dBFS in 2 dB steps, and 20 Hz–20 kHz at 0 and
    /// −18 dBFS).
    /// Computed on the control thread, nothing changes. →
    /// [`HarmonicsReadout`]
    HarmonicsReadout {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        config: Option<HarmonicsConfig>,
        /// Also read sines every 2 dB from -40 to 0 dBFS (`sweep`), and
        /// harmonics against frequency (`freq_sweep`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
        sweep: bool,
        /// With `sweep`: run the level sweeps (`sweep`, `under_load`) at
        /// this one tone, Hz (10..=20000; default 1 kHz and 40 Hz).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        sweep_freq_hz: Option<f64>,
        /// With `sweep`: run `freq_sweep` at this one level, dBFS
        /// (−120..=0; default 0 and −18 dBFS).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        sweep_level_dbfs: Option<f64>,
    },
    /// The distortion spectrum of a harmonics config (default: chain A's;
    /// off when it has none) for a sine at `freq_hz` (10 Hz to 0.45 × the
    /// rate; snapped to the analysis grid) and `level_dbfs` (−120..=0) at the
    /// current output rate (else 96 kHz): the tone runs through the stage in
    /// steady state, every harmonic and alias where it lands, relative to the
    /// fundamental out. `traces` picks what runs (empty: the whole stage);
    /// see [`HarmonicsSpectrumTrace`]. Computed on the control thread,
    /// nothing changes. → [`HarmonicsSpectrum`]
    HarmonicsSpectrum {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        config: Option<HarmonicsConfig>,
        freq_hz: f64,
        level_dbfs: f64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(
            feature = "ts",
            ts(as = "Option<Vec<HarmonicsSpectrumTrace>>", optional)
        )]
        traces: Vec<HarmonicsSpectrumTrace>,
    },
    /// The harmonics stage's live view: what the stage of the side playing
    /// does to the music now (the latest analysis frame; latest wins, cheap).
    /// Each call also keeps the analysis on for a few seconds
    /// ([`HARMONICS_LIVE_HOLD_MS`]): nothing is copied or analysed while
    /// nobody calls. → [`HarmonicsLive`]
    HarmonicsLive,
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
    SetDevice {
        device: String,
        /// Its output pair ([`EngineConfig::output_channels`]; `[0, 1]`:
        /// the first two). Left out: kept for the same device, the first
        /// two for another one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        output_channels: Option<OutputChannels>,
    },
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
    /// Blind test mask (`{enabled}`, not persisted). While set,
    /// [`AbStatus::current`] is `None`, [`AbStatus::blind`] is true, the
    /// overs read 0, `ab_select` replies without the side and `ab_measure`
    /// cannot start. Cleared by `{enabled: false}`, `set_ab`, or A/B turning
    /// off. → [`AbStatus`]
    AbBlind { enabled: bool },
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
        /// Its output pair ([`FollowerConfig::output_channels`]; `[0, 1]`:
        /// the first two).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        output_channels: Option<OutputChannels>,
        /// Whether it follows the master's level
        /// ([`FollowerConfig::follow_volume`]).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        follow_volume: Option<bool>,
        /// The follower's own chain: a JSON merge patch over chain A
        /// ([`FollowerConfig::chain_patch`]); `{}` clears it (chain A).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional, type = "Record<string, unknown>"))]
        chain_patch: Option<serde_json::Value>,
        /// Its device period, ms ([`FollowerConfig::period_ms`]); `0`: the
        /// default again.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_some"
        )]
        #[cfg_attr(feature = "ts", ts(optional))]
        period_ms: Option<f64>,
    },
    /// → `Vec<StreamInfo>`
    ListStreams,
    /// Every preset: the global ones (`part: null`) and each page's own,
    /// sorted by part (globals first), then name. → `Vec<PresetInfo>`
    ListPresets,
    /// `part` picks a page's presets; absent = a global preset. → [`Preset`]
    GetPreset {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        part: Option<PresetPart>,
    },
    /// Save chain A as a preset and make it active. With `part`, a page's
    /// preset holding that part only; without, a global preset holding
    /// `parts` (absent = every part). With `overwrite: false` an existing
    /// preset is refused. → [`PresetInfo`]
    SavePreset {
        name: String,
        #[serde(default)]
        overwrite: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        part: Option<PresetPart>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        parts: Option<Vec<PresetPart>>,
    },
    /// Validate a preset, apply the parts it holds to chain A (the others
    /// stay as they are) and make it active. → `null`
    LoadPreset {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        part: Option<PresetPart>,
    },
    /// Reset one page's part of chain A to its defaults (the room
    /// calibration's convolution item stays) and clear that page's active
    /// preset name, so the next save names a new preset. → `null`
    BlankPreset { part: PresetPart },
    /// → `null`
    DeletePreset {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        part: Option<PresetPart>,
    },
    /// → [`PresetInfo`]
    RenamePreset {
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        part: Option<PresetPart>,
    },
    /// Parse Equalizer APO / AutoEQ text into a new EQ preset (`part: eq`;
    /// an existing name is replaced). Does not load it. → [`PresetInfo`]
    ImportApo { name: String, text: String },
    /// → `Vec<DeviceInfo>` (from `/proc/asound`; nothing is opened)
    ListDevices,
    /// Say which card each output is, when identical DACs moved USB ports
    /// and the daemon cannot tell them apart ([`Status::unresolved_devices`]):
    /// every assignment at once, so two outputs can swap. Each output's
    /// identity is taken again from the card it now names. → `null`
    AssignDevices { assignments: Vec<DeviceAssignment> },
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
    /// Start a measurement: play a WAV file straight to chosen output
    /// channels, other streams muted, the DACs kept awake. Returns once it
    /// is accepted (the file checked); [`Call::MeasureStatus`] follows it.
    /// → [`MeasureStatus`]
    MeasurePlay(MeasurePlayRequest),
    /// Stop the running measurement (the file fades out, the mute lifts).
    /// Without one, the last run's status. → [`MeasureStatus`]
    MeasureStop,
    /// The running (or last) measurement. → [`MeasureStatus`]
    MeasureStatus,
    /// Load a room-calibration bundle (a manifest of per-device filter
    /// matrices, gains and trims), replacing the loaded one: everything is
    /// checked first and applied as one config edit; on any error nothing
    /// changes. → [`CalibrationStatus`] (plus `notes`)
    BundleLoad { path: String },
    /// Remove the loaded calibration: exactly what it installed, the
    /// followers' trims and patches put back. → `null`
    BundleUnload,
    /// The loaded calibration. → `Option<CalibrationStatus>`
    BundleShow,
    /// A/B of calibrations on the master: side B is chain A with the
    /// bundle at `path` in place of the loaded one (`null`: without a
    /// calibration). → [`AbStatus`] (plus `notes`)
    BundleAb {
        #[serde(default)]
        path: Option<String>,
    },
    /// Set the level of one part of the loaded calibration: its item's
    /// gain, dB (`-60..=24`; `null`: back to the manifest's). `device`
    /// names a follower part; left out (or `"master"`), the master's. A
    /// chain edit: the boost cap counts it, and over it nothing changes.
    /// → [`CalibrationStatus`] (plus `notes`)
    BundleGain {
        #[serde(default)]
        device: Option<String>,
        gain_db: Option<f64>,
    },
    /// The frequency response of a chain's convolution items, each alone
    /// and combined per output, from the IRs the engine plays (resampled to
    /// the current graph rate, else the default rate, every gain applied).
    /// `side: "b"` is A/B side B's chain; `device` a follower's own chain
    /// (its `chain_patch` applied); default chain A on the master. Read-only,
    /// off the mutation lock; cached per loaded IRs and rate. →
    /// [`ConvResponse`]
    ConvResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        side: Option<AbSide>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        device: Option<String>,
    },
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
    /// A slow read-only call (`harmonics_readout`, `harmonics_spectrum`,
    /// `upsampler_response`)
    /// was still queued when a newer call of the same method arrived on the
    /// same connection: it is answered with this instead of being computed.
    pub const SUPERSEDED: i32 = 5;
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
    /// Recommended at the same target, at one source rate: the per-rate
    /// plan's fallback where the requested design does not build
    /// ([`RateFallbackCause::Design`]); never an overload step.
    Recommended,
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
    /// The source (graph) rate it was made for, Hz; 0 when unknown. The
    /// fallback is per rate: it ends when playback moves to another source
    /// rate, and that rate is then priced as over budget
    /// ([`RateFallbackCause::Overload`] in [`Status::upsampler_rates`]).
    #[serde(default)]
    pub source_rate: u32,
}

/// Where the requested upsampler runs at one source rate
/// ([`Status::upsampler_rates`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RateAdmission {
    /// Source (graph) rate, Hz.
    pub source_rate: u32,
    /// The output rate the engine lands it at, Hz.
    pub output_rate: u32,
    /// The requested design's estimated load here: the fraction of one core
    /// on the busiest thread, A/B included, as the admission check prices it
    /// (compare with [`EngineLimits::cpu_share`]).
    pub load: f64,
    /// What runs here instead of the requested upsampler; absent when the
    /// requested one runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fallback: Option<RateFallback>,
}

/// [`RateAdmission::fallback`]: the upsampler the engine builds at that source
/// rate, at the same output rate.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RateFallback {
    /// [`OverloadAction::Reverted`] (the last upsampler setting that ran
    /// without overload), [`OverloadAction::LightUpsampler`] (the engine
    /// bypass resampler's design) or [`OverloadAction::Recommended`]
    /// (where the requested design does not build).
    pub action: OverloadAction,
    pub cause: RateFallbackCause,
    /// Its estimated load, as [`RateAdmission::load`].
    pub load: f64,
    /// Why, in a few plain words, for [`RateFallbackCause::Design`]: `too
    /// short here (needs about 1,600 taps)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reason: Option<String>,
}

/// Why a source rate falls back ([`RateFallback::cause`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum RateFallbackCause {
    /// The estimate is over [`EngineLimits::cpu_share`].
    Estimate,
    /// The requested design overloaded at this rate in this daemon run
    /// (the reactive fallback, [`Status::overload`]).
    Overload,
    /// The requested design cannot be built at this rate (too short, too
    /// long, over a design limit): a design that builds at some of its
    /// allowed rates is applied there, and plays Recommended at the others.
    Design,
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
/// narrower than four 8192-point bins; above 88.2 kHz both run on the signal
/// decimated to 44.1/48 kHz and a third FFT (bins of at most 48 Hz) reads the
/// bins above 20 kHz. Power is averaged over the frames since the previous
/// report. Bin `k` is centred on `f_min_hz * 2^(k / bins_per_octave)` (half a
/// step either side); each tap's bins run up to its own Nyquist (less two FFT
/// bins) or 100 kHz, whichever is lower, with no bin straddling that top, so
/// `pre` (graph rate) and `post` (output rate) can differ in length.
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

/// The result of [`Call::ConvResponse`]: curves on a log grid (about 1/24
/// octave, 20 Hz to 20 kHz or Nyquist), unsmoothed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConvResponse {
    /// The graph rate the IRs are taken at, Hz.
    pub rate_hz: u32,
    pub side: AbSide,
    /// The follower measured (`None`: the master).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub device: Option<String>,
    /// Shared by every curve.
    pub freqs_hz: Vec<f32>,
    /// The longest entry of the combined convolution at `rate_hz`, taps (0
    /// without an enabled item): what the engine runs.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub combined_taps: u64,
    /// Each enabled item, in chain order.
    pub items: Vec<ConvResponseItem>,
    /// The whole stage per output (all enabled items in series, a matrix
    /// output summing its inputs coherently), outputs in order. Empty
    /// without an enabled item.
    pub outputs: Vec<ConvResponseCurve>,
}

/// One enabled item of a [`ConvResponse`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConvResponseItem {
    /// Its index in the chain's `convolution` list.
    pub index: u32,
    /// Its `name`, else its file's name (`"Matrix"` for an unnamed matrix).
    pub label: String,
    /// The loaded room calibration's item.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub calibration: bool,
    /// Its longest IR at `rate_hz`, taps.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub taps: u64,
    /// A plain item: one curve per channel of its file (a mono file: one,
    /// `in`/`out` left out). A matrix: one per route.
    pub curves: Vec<ConvResponseCurve>,
}

/// One curve of a [`ConvResponse`], at its `freqs_hz`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConvResponseCurve {
    /// The input channel (left out: every input, or a combined output).
    #[serde(rename = "in", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub input: Option<u32>,
    /// The output channel (left out: a mono IR on every channel).
    #[serde(rename = "out", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub output: Option<u32>,
    /// Magnitude, dB (−200 at the quietest).
    pub mag_db: Vec<f32>,
    /// Group delay, ms (`null` where the magnitude is below −120 dB).
    pub group_delay_ms: Vec<Option<f32>>,
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
    /// The delay the live engine adds for this design, output frames: the
    /// filter's own (kernel centre or impulse peak) plus the engine's
    /// partition block at this source rate, as `status.filter_latency_ms`
    /// counts it for a chain with no other delayed stage.
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
    /// passes (rates where the target resolves to x1 are left out); at a
    /// rate that plays Recommended instead ([`Self::notes`]), Recommended's.
    pub plans: Vec<RatePlan>,
    /// What the design does differently at a source rate, one per rate: a
    /// band's passband end in Hz that does not fit the band there falls
    /// back to the band's fraction of the Nyquist; a rate where the design
    /// does not build plays Recommended instead ([`RateNote::plays`]).
    #[serde(default)]
    pub notes: Vec<RateNote>,
    /// Where the design does not build at some of the rates it would play:
    /// what is wrong in plain words, the technical detail per rate and the
    /// one-click fixes. `None` when it builds everywhere it plays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub advice: Option<DesignAdvice>,
}

/// One entry of [`DesignCheck::notes`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RateNote {
    pub source_rate: u32,
    /// The design field the note is about (`design.cutoff.passband_end_hz`,
    /// `design.length`).
    pub path: String,
    pub message: String,
    /// The design that plays at this rate instead (`recommended`, or
    /// `recommended_light` for a lighter form of it where Recommended does
    /// not fit the CPU there), when the requested one does not build there;
    /// the message is then why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub plays: Option<String>,
}

/// [`DesignCheck::advice`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DesignAdvice {
    /// Plain sentences: what is wrong, the rates grouped by reason, and
    /// what to do (`1,279 taps is too short at 44.1k ×16 (needs about
    /// 1,600) and too long for an equiripple design at 176.4k ×4 and 352.8k
    /// ×2. Use automatic length, set taps per input sample, or allow fewer
    /// rates.`).
    pub message: String,
    /// Whether the design is applied anyway: it builds at some of the
    /// rates it would play, and the others play Recommended. `false` when
    /// it builds nowhere and is refused (then [`DesignCheck::errors`]
    /// carries [`Self::message`]).
    pub applied: bool,
    /// The engine's own refusal at each rate that does not build.
    pub details: Vec<RateDetail>,
    /// Edits that make the design build at every rate it would play.
    pub fixes: Vec<DesignFix>,
}

/// One entry of [`DesignAdvice::details`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RateDetail {
    pub source_rate: u32,
    pub ratio: crate::RateRatio,
    pub path: String,
    pub message: String,
}

/// One entry of [`DesignAdvice::fixes`]: a label (`Set 1,601 taps`) and the
/// whole design with the edit made (its `based_on` kept).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DesignFix {
    pub label: String,
    pub design: crate::design::StoredDesign,
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

/// A part of chain A that a page's preset picker saves and loads on its own,
/// one per page of the app's signal path.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum PresetPart {
    /// `chain.eq`: bands, L/R sections, preamp, tilt and tone.
    Eq,
    /// `chain.convolution`.
    Convolution,
    /// Stereo image: `chain.crossfeed` (crossfeed and width).
    Stereo,
    /// `chain.harmonics`.
    Harmonics,
    /// `chain.upsampler` (target, filter preset or design).
    Upsampling,
    /// The output-side pages together: `chain.dc_block`, `chain.pitch`,
    /// `chain.limiter` and `chain.output` (headroom, bits and dither).
    Output,
}

impl PresetPart {
    /// Every part, in signal-path order of the app's pages.
    pub const ALL: [PresetPart; 6] = [
        Self::Stereo,
        Self::Eq,
        Self::Convolution,
        Self::Upsampling,
        Self::Harmonics,
        Self::Output,
    ];

    /// The wire name (`eq`, `output`, …).
    pub fn name(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Convolution => "convolution",
            Self::Stereo => "stereo",
            Self::Harmonics => "harmonics",
            Self::Upsampling => "upsampling",
            Self::Output => "output",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|part| part.name() == name)
    }

    /// The chain JSON this part owns: `(key, None)` for a whole top-level
    /// key, `(key, Some(field))` for one field of it.
    pub fn json_paths(self) -> &'static [(&'static str, Option<&'static str>)] {
        match self {
            Self::Eq => &[("eq", None)],
            Self::Convolution => &[("convolution", None)],
            Self::Stereo => &[("crossfeed", None)],
            Self::Harmonics => &[("harmonics", None)],
            Self::Upsampling => &[("upsampler", None)],
            Self::Output => &[
                ("dc_block", None),
                ("pitch", None),
                ("limiter", None),
                ("output", None),
            ],
        }
    }

    /// Copy this part of `from` into `to`, leaving the rest of `to` alone.
    pub fn copy(self, from: &ChainConfig, to: &mut ChainConfig) {
        match self {
            Self::Eq => to.eq = from.eq.clone(),
            Self::Convolution => {
                // The room calibration's item stays: it belongs to the
                // room, not to the preset.
                let kept = to.convolution.iter().filter(|c| c.calibration).cloned();
                let mut list: Vec<_> = from
                    .convolution
                    .iter()
                    .filter(|c| !c.calibration)
                    .cloned()
                    .collect();
                list.extend(kept);
                to.convolution = list;
            }
            Self::Stereo => to.crossfeed = from.crossfeed.clone(),
            Self::Harmonics => to.harmonics = from.harmonics.clone(),
            Self::Upsampling => to.upsampler = from.upsampler.clone(),
            Self::Output => {
                to.dc_block = from.dc_block;
                to.pitch = from.pitch.clone();
                to.limiter = from.limiter.clone();
                to.output = from.output;
            }
        }
    }

    /// Whether `a` and `b` agree on this part.
    pub fn same(self, a: &ChainConfig, b: &ChainConfig) -> bool {
        match self {
            Self::Eq => a.eq == b.eq,
            Self::Convolution => {
                let own = |c: &&ConvolutionConfig| !c.calibration;
                a.convolution
                    .iter()
                    .filter(own)
                    .eq(b.convolution.iter().filter(own))
            }
            Self::Stereo => a.crossfeed == b.crossfeed,
            Self::Harmonics => a.harmonics == b.harmonics,
            Self::Upsampling => a.upsampler == b.upsampler,
            Self::Output => {
                a.dc_block == b.dc_block
                    && a.pitch == b.pitch
                    && a.limiter == b.limiter
                    && a.output == b.output
            }
        }
    }
}

/// Preset envelope, on disk and on the wire:
/// `{ "version": 1, "name": …, "part"?: …, "parts"?: […], "chain": … }`.
///
/// A preset holds parts of chain A's signal path. A page's preset has
/// `part` (and only that part's keys in `chain`); a global preset has
/// `parts`, the parts it holds, or none for every part (every file written
/// before parts existed). Device, routing, rate policy and A/B are machine
/// settings and never part of a preset. Keys of `chain` outside the parts
/// held are left out of the file and read back as defaults: only
/// [`Preset::covers`] is ever applied.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Preset {
    #[serde(default = "preset_version")]
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub part: Option<PresetPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub parts: Option<Vec<PresetPart>>,
    pub chain: ChainConfig,
}

fn preset_version() -> u32 {
    1
}

impl Preset {
    /// A global preset holding every part.
    pub fn new(name: impl Into<String>, chain: ChainConfig) -> Self {
        Self {
            version: preset_version(),
            name: name.into(),
            part: None,
            parts: None,
            chain,
        }
    }

    /// A page's preset holding `part` only.
    pub fn for_part(name: impl Into<String>, part: PresetPart, chain: ChainConfig) -> Self {
        Self {
            part: Some(part),
            ..Self::new(name, chain)
        }
    }

    /// The parts this preset holds, in [`PresetPart::ALL`] order.
    pub fn covers(&self) -> Vec<PresetPart> {
        match (&self.part, &self.parts) {
            (Some(part), _) => vec![*part],
            (None, Some(parts)) => PresetPart::ALL
                .into_iter()
                .filter(|part| parts.contains(part))
                .collect(),
            (None, None) => PresetPart::ALL.to_vec(),
        }
    }

    /// `chain` with this preset's parts applied, the rest as it was.
    pub fn apply_to(&self, chain: &ChainConfig) -> ChainConfig {
        let mut out = chain.clone();
        for part in self.covers() {
            part.copy(&self.chain, &mut out);
        }
        out
    }

    /// Whether `chain` matches this preset on every part it holds.
    pub fn matches(&self, chain: &ChainConfig) -> bool {
        self.covers()
            .into_iter()
            .all(|part| part.same(&self.chain, chain))
    }
}

/// The result of [`Call::HarmonicsReadout`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsReadout {
    /// Output rate the stage was built at, Hz.
    pub rate_hz: f64,
    /// Loudness change on music, dB (BS.1770 on loud pink noise): what A/B
    /// level matching counts for the stage.
    pub level_change_db: f64,
    /// Certified worst-case peak gain for inputs within full scale, dB:
    /// how far over 0 dBFS the stage can take a full-scale input (the stage
    /// plays at its natural level; the limiter and the output headroom
    /// handle what goes over).
    pub peak_gain_db: f64,
    /// Gain for small signals, dB.
    pub small_signal_gain_db: f64,
    /// Aliasing measured at `rate_hz`: what a 0 dBFS tone near 20 kHz
    /// folds back into the audio band through the whole stage.
    pub aliasing: HarmonicsAliasing,
    /// Layers that are not band-limited and alias significantly into the
    /// audio band at `rate_hz` (tape hysteresis below 4×, 176.4 kHz), with
    /// the rate each needs (empty, and left out, when none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<HarmonicsCleanRate>>", optional))]
    pub below_clean_rate: Vec<HarmonicsCleanRate>,
    /// Layers whose bias shift was cut short of its depth, so the bound
    /// keeps the peak gain within 1 dB of the static curve's (strong
    /// high-order curves; empty, and left out, when none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<HarmonicsBiasLimit>>", optional))]
    pub bias_limited: Vec<HarmonicsBiasLimit>,
    /// Sine readouts: 1 kHz and 40 Hz at 0 and −18 dBFS. With sag or bias
    /// on, these, `sweep` and `freq_sweep` are the static curves (envelopes
    /// at rest, as a quiet passage or the onset of a loud one hears them).
    pub points: Vec<HarmonicsReadoutPoint>,
    /// With `sweep`: 1 kHz then 40 Hz, each at −40..=0 dBFS in 2 dB steps
    /// (empty, and left out, otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "ts",
        ts(as = "Option<Vec<HarmonicsReadoutPoint>>", optional)
    )]
    pub sweep: Vec<HarmonicsReadoutPoint>,
    /// With `sweep`: harmonics against frequency, steady state in closed
    /// form at `rate_hz` (simulated with a hysteresis layer): 0 then
    /// −18 dBFS, each at the 31 third-octave frequencies from 20 Hz to
    /// 20 kHz, or every third of them (octave steps) with a hysteresis layer
    /// (empty, and left out, otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "ts",
        ts(as = "Option<Vec<HarmonicsReadoutPoint>>", optional)
    )]
    pub freq_sweep: Vec<HarmonicsReadoutPoint>,
    /// With `sweep` and a layer with sag or bias on: the level sweep under
    /// load, simulated with the envelopes settled on each steady sine (the
    /// same tones as `sweep`, or every other level with slow layers; empty,
    /// and left out, otherwise).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "ts",
        ts(as = "Option<Vec<HarmonicsReadoutPoint>>", optional)
    )]
    pub under_load: Vec<HarmonicsReadoutPoint>,
}

/// [`HarmonicsReadout::aliasing`]: the stage's output for a full-scale
/// (0 dBFS) sine at `tone_hz` (about 20 kHz, the top of the audio band),
/// in steady state at the readout's rate, measured across the whole
/// spectrum (every order, cross terms of series layers and tape layers
/// included). Every partial landing in 20 Hz..=20 kHz other than the tone
/// itself is aliasing: the tone's own harmonics all lie above 20 kHz.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsAliasing {
    /// The test tone, Hz.
    pub tone_hz: f64,
    /// Every alias in 20 Hz..=20 kHz together (power sum), dB relative to
    /// the tone's fundamental out (-200: none).
    pub level_db: f64,
    /// Where the loudest alias lands, Hz (0 when there is none).
    pub worst_hz: f64,
    /// The loudest alias, dB relative to the fundamental (-200: none).
    pub worst_db: f64,
    /// Whether `level_db` is above the warning threshold (-60 dBc).
    pub warn: bool,
}

/// One entry of [`HarmonicsReadout::below_clean_rate`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsCleanRate {
    /// Index of the layer in the config's `layers`.
    pub layer: u32,
    /// Lowest output rate at which it runs clean, Hz.
    pub clean_rate_hz: f64,
}

/// One entry of [`HarmonicsReadout::bias_limited`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsBiasLimit {
    /// Index of the layer in the config's `layers`.
    pub layer: u32,
    /// Its bias depth as applied (the layer's `depth × amount`).
    pub depth: f64,
    /// The operating point the bias may reach (same sign, smaller).
    pub reach: f64,
}

/// One sine readout of [`HarmonicsReadout`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsReadoutPoint {
    /// Sine peak level, dBFS.
    pub input_dbfs: f64,
    pub freq_hz: f64,
    /// THD (H2..=H15 below Nyquist at the readout's rate) relative to the
    /// fundamental, dB (−200 = none).
    pub thd_db: f64,
    /// GedLee metric Gm of the equivalent static curve at this level (the
    /// same orders as `thd_db`).
    pub gedlee_gm: f64,
    /// H2..=H15 relative to the fundamental, dB (−200 = none, or above
    /// Nyquist: listed in `aliased` instead).
    pub harmonics_db: [f64; crate::HARMONICS_READOUT_ORDERS],
    /// Orders up to H15 above Nyquist at the readout's rate, folded to the
    /// frequency they land on (not harmonics of the tone, so left out of
    /// `harmonics_db` and `thd_db`; empty, and left out, when none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<HarmonicsAlias>>", optional))]
    pub aliased: Vec<HarmonicsAlias>,
    /// The fundamental out over in, dB: the stage's gain for this sine
    /// (its small-signal gain, the odd orders' compression, and with tilt its
    /// change with frequency).
    pub gain_db: f64,
}

/// One entry of [`HarmonicsReadoutPoint::aliased`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsAlias {
    /// The harmonic's order.
    pub order: u32,
    /// Where it lands after folding at the readout's rate, Hz.
    pub freq_hz: f64,
    /// Its level relative to the fundamental, dB.
    pub level_db: f64,
}

/// What one line of [`Call::HarmonicsSpectrum`] runs the tone through.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarmonicsSpectrumTrace {
    /// The whole stage (what plays).
    Stage,
    /// One layer alone (index in `layers`, applied even when disabled), fed
    /// the clean tone.
    Layer { layer: u32 },
    /// In series: the chain up to and including this layer (its active
    /// layers), as the next layer receives it.
    After { layer: u32 },
}

/// The output word a [`HarmonicsSpectrum`]'s floor and THD+N assume: the
/// engine's own (`chain.output`, bits capped at the device format).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsSpectrumWord {
    /// Bit depth the dither targets.
    pub bits: u32,
    pub dither: crate::DitherKind,
    /// The fixed output headroom taken off after the stage, dB (volume at
    /// 0 dB assumed).
    pub headroom_db: f64,
}

/// The result of [`Call::HarmonicsSpectrum`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsSpectrum {
    /// Output rate the stage ran at, Hz.
    pub rate_hz: f64,
    /// The tone as run (coherent with the window), Hz.
    pub freq_hz: f64,
    pub level_dbfs: f64,
    /// The FFT's bin width, Hz.
    pub bin_hz: f64,
    pub word: HarmonicsSpectrumWord,
    /// The plotted points' frequencies, Hz (log spaced from 10 Hz to
    /// Nyquist), shared by every line's `levels_db`.
    pub freqs_hz: Vec<f32>,
    /// One per requested trace, in order.
    pub lines: Vec<HarmonicsSpectrumLine>,
}

/// One line of [`HarmonicsSpectrum`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsSpectrumLine {
    pub trace: HarmonicsSpectrumTrace,
    /// The fundamental out over in, dB.
    pub gain_db: f64,
    /// Every harmonic below Nyquist (any order), power sum, dB relative to
    /// the fundamental (−200: none).
    pub thd_db: f64,
    /// Everything but the fundamental in 20 Hz..=20 kHz (harmonics and
    /// aliases) plus the output word's dither and rounding noise in that
    /// band (flat TPDF assumed for the shaped kinds, whose noise sits mostly
    /// above the band: an upper bound), dB relative to the fundamental.
    pub thd_n_db: f64,
    /// The output word's noise per FFT bin on the plot's window, dB
    /// relative to the fundamental: the floor a measurement of the engine's
    /// output would show.
    pub floor_db: f64,
    /// `levels_db` is windowed (7-term Blackman-Harris): the steady state is
    /// not periodic enough over the window for the exact spectrum (a slow
    /// envelope still moving). Absent: unwindowed and exact, every order in
    /// one bin.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub windowed: bool,
    /// The spectrum at `freqs_hz`, peak held over each point's span, dB
    /// relative to the fundamental (−250 floor). Exact: each bin (one order)
    /// on the one point whose span holds it, the floor where a span holds
    /// none. Windowed: interpolated in dB between bins where a span holds
    /// none.
    pub levels_db: Vec<f32>,
    /// Orders H2..=H64 above −200 dB, where they land.
    pub peaks: Vec<HarmonicsSpectrumPeak>,
    /// One cycle of the output in steady state, 512 samples (the input
    /// is `10^(level_dbfs/20) · cos(2π i / 512)`), full scale 1.
    pub wave: Vec<f32>,
}

/// One entry of [`HarmonicsSpectrumLine::peaks`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsSpectrumPeak {
    pub order: u32,
    /// Where it lands (folded when above Nyquist), Hz.
    pub freq_hz: f64,
    /// Relative to the fundamental, dB.
    pub level_db: f64,
    /// Above Nyquist: folded back to `freq_hz`.
    pub aliased: bool,
}

/// How long one [`Call::HarmonicsLive`] keeps the live analysis on, ms.
pub const HARMONICS_LIVE_HOLD_MS: u32 = 3000;

/// What [`HarmonicsLive`] describes.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum HarmonicsLiveState {
    /// Starting up: the analysis was off until this call (or has not
    /// gathered one window yet).
    Waiting,
    /// No audio reaches chain A's stage (nothing playing, DSP off, bypass,
    /// or a chain without the stage running).
    Idle,
    /// Chain A has no active harmonics stage.
    Off,
    /// Audio runs through the stage but the music is below −90 dBFS.
    Silent,
    /// A blind test is running (`ab_blind`): a live readout would give the
    /// side away, so nothing is analysed or shown.
    Hidden,
    /// A frame of music.
    Playing,
}

/// How a [`HarmonicsLive`] frame splits the added content
/// ([`HarmonicsLive::parts`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum HarmonicsLiveSplit {
    /// One part per order: a parallel stage of static Chebyshev layers is
    /// a sum of polynomial terms, so each order's products on the music
    /// (its harmonics of every note plus the intermodulation it makes) are
    /// its term on each channel. The odd orders and a single even order
    /// are within about 0.1 dB; several even orders share the stage's DC
    /// blocker, so just after a sudden level change in deep bass their
    /// lines are estimates (their total is exact). Where their terms
    /// cancel each other, a frame shows `even_odd` instead, with a note.
    Orders,
    /// The even and the odd part of the stage's curve (a series chain of
    /// static Chebyshev layers; cross terms mix the orders), or a parallel
    /// stage's even total and odd sum when its even orders cancel each
    /// other ([`HarmonicsLive::split_note`] says so). For a series chain an
    /// estimate:
    /// its DC blocker starts in a modelled steady state, which can read
    /// well off in the frames just after a sudden level change in deep
    /// bass.
    EvenOdd,
    /// The total only ([`HarmonicsLive::split_note`] says why: a layer
    /// with memory).
    Total,
}

/// One part of the added content in a [`HarmonicsLive`] frame.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsLivePart {
    /// `"h2"`..`"h9"`, `"even"` or `"odd"`.
    pub key: String,
    /// The order (with [`HarmonicsLiveSplit::Orders`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub order: Option<u32>,
    /// Relative to the music out, as [`HarmonicsLive::added_db`] and
    /// `added_peak_db`, dB.
    pub level_db: f64,
    pub peak_db: f64,
}

/// The live state of one sag/bias layer in a [`HarmonicsLive`] frame.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsLiveLayer {
    /// Its index in the config's layers.
    pub layer: u32,
    /// How far its sag turns the drive down now, dB (0 at rest; the deeper
    /// channel).
    pub sag_db: f64,
    /// Its bias operating point now, fraction of full scale.
    pub bias: f64,
}

/// The result of [`Call::HarmonicsLive`]: one analysis of the harmonics
/// stage on the music playing (the A/B side playing, the mono sum of its
/// channels, at the output rate, the stage alone: before the output
/// headroom, volume and limiter).
///
/// The stage's output `y` is split as `y = g·x + a`: `x` its input, `g` the
/// least-squares gain of `y` on `x` (the stage has no delay), `a` the
/// **added content** left over (its DC removed). `g` against the stage's
/// linear part `G` (its small-signal gain; DC blockers and tilt filters
/// shape only the distortion, so they are not in it) is the **level
/// change**: sag's turn-down and the curve's compression, kept out of the
/// added content. The headline numbers (`added_db`, `added_pct`,
/// `added_peak_db`, `level_change_db`, `parts`) cover about the last
/// second of music, one fit over it. Gain changes faster than that (sag
/// attack, envelope ripple) stay in `a` as the sidebands they make.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct HarmonicsLive {
    pub state: HarmonicsLiveState,
    /// Counts analysed frames (a client sees whether a frame is new).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub seq: u64,
    /// Output rate the stage runs at (0 when unknown).
    pub rate: u32,
    /// Analysis window, ms (a Hann window of a power-of-two FFT near
    /// 1/8 s); one frame every `interval_ms` (slower while the machine is
    /// busy).
    pub window_ms: f64,
    pub interval_ms: f64,
    /// Log-spaced bins: bin `k` centred on `f_min_hz · 2^(k /
    /// bins_per_octave)`, up to Nyquist.
    pub f_min_hz: f64,
    pub bins_per_octave: u32,
    /// The music (the stage's input) by bin, band power in dB: 0 dB = a
    /// full-scale sine. Empty unless `playing`.
    pub music: Vec<f32>,
    /// The added content by bin, same scale.
    pub added: Vec<f32>,
    /// Added content relative to the music out (`a` against `g·x`), its
    /// power over the music's power through about the last second, dB
    /// (dBc) and percent.
    pub added_db: f64,
    pub added_pct: f64,
    /// The loudest stretch of 1/8 window of that second's added content,
    /// relative to the music there, dB: the peak behind `added_db`.
    pub added_peak_db: f64,
    /// Level change `20 log10(g / G)` over that second (`g` its RMS over
    /// the music), dB (sag and compression).
    pub level_change_db: f64,
    /// The linear part `G`, dB.
    pub linear_gain_db: f64,
    /// The music's level: RMS dBFS (a full-scale sine reads −3.01), its
    /// sine-equivalent level (RMS + 3.01, the scale of the readout
    /// sweeps), and its sample peak, dBFS. Floored at −160.
    pub music_rms_dbfs: f64,
    pub music_level_dbfs: f64,
    pub music_peak_dbfs: f64,
    /// Power-weighted mean frequency of the music, 20 Hz–20 kHz, Hz.
    pub centroid_hz: f64,
    /// Each sag/bias layer's state at the end of the window.
    pub layers: Vec<HarmonicsLiveLayer>,
    /// The added content split by order or by parity, from the stage's
    /// static curve evaluated on the windows' input (each part with its DC
    /// and what correlates with the music removed, as the total): each
    /// part's share of the windows' added content, applied to `added_db`.
    pub split: HarmonicsLiveSplit,
    /// With [`HarmonicsLiveSplit::Total`]: why no split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub split_note: Option<String>,
    pub parts: Vec<HarmonicsLivePart>,
    /// Time the analysis thread spent on this frame, ms (its own cost).
    pub analysis_ms: f64,
}

/// One entry of [`Call::ListPresets`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PresetInfo {
    pub name: String,
    /// The page this preset belongs to; absent for a global preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub part: Option<PresetPart>,
    /// The parts it holds (one for a page's preset).
    #[serde(default)]
    pub parts: Vec<PresetPart>,
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
    /// Chain A differs from the saved preset on a part it holds.
    pub modified: bool,
}

/// A page's active preset, as reported by [`Status::part_presets`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PartPresetState {
    pub part: PresetPart,
    pub name: String,
    /// Chain A's part differs from the saved one.
    pub modified: bool,
}

// ---------------------------------------------------------------- devices

/// Which physical device an output is, so it is found again when ALSA
/// names the cards in another order (identical DACs get `USB` and `USB_1`
/// in probe order). USB cards only; the daemon records it next to the ALSA
/// name and keeps the name following it.
///
/// A card is matched by `usb_id` alone while it is the only one of its
/// model (as an output and among the cards present); otherwise by `serial`
/// when the device has one, else by `port`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DeviceIdentity {
    /// The ALSA card id when it was last seen (`USB`, `USB_1`).
    pub card: String,
    /// USB vendor:product, lowercase hex (`/proc/asound/cardN/usbid`).
    pub usb_id: String,
    /// The USB serial number, when the device reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub serial: Option<String>,
    /// The USB port, as `/dev/snd/by-path` names it without the interface:
    /// the controller and the bus-relative port chain
    /// (`pci-0000:00:14.0-usb-0:1.4.3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub port: Option<String>,
}

impl DeviceIdentity {
    /// The port chain of [`Self::port`] (`1.4.3`).
    pub fn port_chain(&self) -> Option<&str> {
        let port = self.port.as_deref()?;
        Some(port.rsplit_once("-usb-0:").map_or(port, |(_, chain)| chain))
    }
}

impl std::fmt::Display for DeviceIdentity {
    /// `USB (2522:0007, serial ABC)` / `USB (2522:0007, USB port 1.4.3)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}", self.card, self.usb_id)?;
        if let Some(serial) = &self.serial {
            write!(f, ", serial {serial}")?;
        } else if let Some(port) = &self.port {
            write!(f, ", {port}")?;
        }
        f.write_str(")")
    }
}

/// One output of [`Call::AssignDevices`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DeviceAssignment {
    /// `master`, or the follower's device as configured now.
    pub output: String,
    /// The playback device it is (`hw:CARD=USB_1,DEV=0`).
    pub device: String,
}

/// An output the daemon holds closed because identical DACs moved USB
/// ports and it cannot tell which card it is ([`Status::unresolved_devices`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UnresolvedDevice {
    /// `master`, or the follower's device as configured.
    pub output: String,
    /// Its configured device.
    pub device: String,
    /// What it was: the USB port it was on.
    pub identity: DeviceIdentity,
    /// The cards of its model nothing else claims (`list_devices` ids).
    pub candidates: Vec<String>,
}

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
    /// What tells it apart from another card of the same model
    /// (`USB port 1.4.3`, `serial ABC123`); `None` when it is the only one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub suffix: Option<String>,
    /// A USB card's identity (`None` for other cards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub identity: Option<DeviceIdentity>,
    /// The playback channel counts it offers, ascending, where `/proc`
    /// lists them (USB audio); empty when unknown. The open device's own
    /// range is in `status.device_caps`.
    #[serde(default)]
    pub channels: Vec<u32>,
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
pub const MAX_ALIGNMENT_MS: f64 = 120.0;

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
    /// The USB device `device` was when it was chosen, kept by the daemon
    /// (see [`EngineConfig::device_identity`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub identity: Option<DeviceIdentity>,
    /// Heard. Disabled, it keeps playing in sync, muted (it holds its
    /// device).
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Its delay behind the master, ms (±[`FOLLOWER_MAX_TRIM_MS`]; negative
    /// plays it earlier): the DACs' own latencies, speaker distance
    /// (1 ms ≈ 34 cm).
    #[serde(default)]
    pub trim_ms: f64,
    /// Its own digital volume, dB (`-120..=0`), after the chain's own
    /// volume: with `follow_volume`, its level when the master was at
    /// `follow_ref_db`.
    #[serde(default)]
    pub volume_db: f64,
    /// The device channels its left and right play on (left out: the
    /// first two), as [`EngineConfig::output_channels`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub output_channels: Option<OutputChannels>,
    /// Master volume changes move it by as much (and the master's mute and
    /// balance reach it): it plays at `volume_db` plus the master's change
    /// since `follow_ref_db`, so its offset to the master stays what the
    /// user set. Off (the default for a follower added by hand), it plays
    /// at `volume_db` alone. A calibration bundle turns it on for the
    /// followers it calibrates.
    #[serde(default)]
    pub follow_volume: bool,
    /// With `follow_volume`: the master's volume (dB) when `volume_db` was
    /// set or following was turned on. The daemon keeps it; left out
    /// otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub follow_ref_db: Option<f64>,
    /// Its own chain: a JSON merge patch (RFC 7386) over chain A, applied
    /// as A/B side B's `patch` is and re-applied on every edit of chain A,
    /// so it follows A everywhere the patch does not reach (`None`: chain A
    /// as is). A machine setting: presets leave it alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "Record<string, unknown>"))]
    pub chain_patch: Option<serde_json::Value>,
    /// Its device period, ms, as the power of two of device frames nearest
    /// it (`None`: [`FOLLOWER_DEFAULT_PERIOD_MS`]), within
    /// [`FOLLOWER_PERIOD_MS_RANGE`] and what the device offers. A shorter
    /// period needs less source ahead of the master (a smaller alignment);
    /// a change reopens the follower.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub period_ms: Option<f64>,
}

/// A follower's device period without [`FollowerConfig::period_ms`], ms.
pub const FOLLOWER_DEFAULT_PERIOD_MS: f64 = 5.0;
/// The periods [`FollowerConfig::period_ms`] takes, ms (shorter ones,
/// 2.7 ms in the simulation, are more than the drift loop places
/// cleanly).
pub const FOLLOWER_PERIOD_MS_RANGE: std::ops::RangeInclusive<f64> = 5.0..=50.0;

impl FollowerConfig {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            identity: None,
            enabled: true,
            trim_ms: 0.0,
            volume_db: 0.0,
            output_channels: None,
            follow_volume: false,
            follow_ref_db: None,
            chain_patch: None,
            period_ms: None,
        }
    }
}

/// The most device channels an output pair may name (channels `0..64`).
pub const MAX_DEVICE_CHANNELS: u32 = 64;

/// The device channels the engine's left and right play on, 0-based
/// (`[l, r]`; on the wire a two-number array). A device with more than two
/// channels opens at the smallest count it takes that holds both; its other
/// channels play exact zeros. `[0, 1]` is the default (stored as no
/// setting): a stereo device plays as it always did.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputChannels(pub [u32; 2]);

impl OutputChannels {
    pub const DEFAULT: Self = Self([0, 1]);

    pub fn left(self) -> u32 {
        self.0[0]
    }

    pub fn right(self) -> u32 {
        self.0[1]
    }

    /// Fewest device channels that hold the pair.
    pub fn min_device_channels(self) -> u32 {
        self.left().max(self.right()) + 1
    }

    /// Why the pair is not one (the same channel twice, or past
    /// [`MAX_DEVICE_CHANNELS`]); `None` when it is.
    pub fn error(self) -> Option<String> {
        let [l, r] = self.0;
        if l == r {
            return Some(format!(
                "output_channels [{l}, {r}]: left and right need two different channels"
            ));
        }
        if l.max(r) >= MAX_DEVICE_CHANNELS {
            return Some(format!(
                "output_channels [{l}, {r}]: channels go from 0 to {}",
                MAX_DEVICE_CHANNELS - 1
            ));
        }
        None
    }

    /// Why the pair does not fit a device with `channels_max` channels
    /// (`None` when it does, or when the count is unknown: 0).
    pub fn fit_error(self, device: &str, channels_max: u32) -> Option<String> {
        if channels_max == 0 || self.min_device_channels() <= channels_max {
            return None;
        }
        let [l, r] = self.0;
        Some(format!(
            "output_channels [{l}, {r}]: device '{device}' has {channels_max} channels (0 to {})",
            channels_max - 1
        ))
    }

    /// `None` for the default pair (the stored form).
    pub fn normalized(pair: Option<Self>) -> Option<Self> {
        pair.filter(|p| *p != Self::DEFAULT)
    }
}

impl std::fmt::Display for OutputChannels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.left(), self.right())
    }
}

/// A device's latency from its chain's input to its output, and the delay
/// that aligns it with the other devices (`status.outputs`).
///
/// Device frames at its output rate, and µs. `chain` is its filters'
/// (convolution, upsampler, limiter look-ahead); `compensation` the delay
/// added so every device's chain path is as long as the longest (the
/// master's in its alignment delay, a follower's after its chain);
/// `alignment` the master's alignment delay beyond that (0 on a
/// follower); `total` the three together.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PathLatency {
    /// Its output rate (0 while closed).
    pub rate_hz: u32,
    pub chain_frames: f64,
    pub chain_us: f64,
    pub compensation_frames: f64,
    pub compensation_us: f64,
    pub alignment_frames: f64,
    pub alignment_us: f64,
    pub total_frames: f64,
    pub total_us: f64,
}

impl PathLatency {
    /// From the parts in frames at `rate_hz`.
    pub fn from_frames(rate_hz: u32, chain: f64, compensation: f64, alignment: f64) -> Self {
        let us = |f: f64| {
            if rate_hz == 0 {
                0.0
            } else {
                f / f64::from(rate_hz) * 1e6
            }
        };
        let total = chain + compensation + alignment;
        Self {
            rate_hz,
            chain_frames: chain,
            chain_us: us(chain),
            compensation_frames: compensation,
            compensation_us: us(compensation),
            alignment_frames: alignment,
            alignment_us: us(alignment),
            total_frames: total,
            total_us: us(total),
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
    /// [`FollowerConfig::follow_volume`] (absent from daemons that predate
    /// it: they never follow).
    #[serde(default)]
    pub follow_volume: bool,
    /// The level it plays at now, dB: `volume_db`, plus the master's change
    /// since `follow_ref_db` when it follows (at most 0 dB). Absent from
    /// daemons that predate it (read `volume_db` there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub level_db: Option<f64>,
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
    /// Its configured period ([`FollowerConfig::period_ms`]; `None`: the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub period_ms: Option<f64>,
    /// What it needs of the master's alignment, ms: the alignment less the
    /// least source it had ahead at its worst since its last change
    /// (negative: it needs none, by that much). `None` until measured, and
    /// while the master does not play.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub need_ms: Option<f64>,
    /// Its output pair (`None`: the first two).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub output_channels: Option<OutputChannels>,
    /// The channel count its device opened at, and the most it takes (0
    /// while closed or unknown).
    #[serde(default)]
    pub device_channels: u32,
    #[serde(default)]
    pub device_channels_max: u32,
    #[serde(default)]
    pub error: Option<String>,
    /// The chain fields where its own chain differs from chain A (top
    /// level: `convolution`, `eq`, `upsampler`, ...); empty when it plays
    /// chain A.
    #[serde(default)]
    pub own_chain: Vec<String>,
    /// Its effective chain (chain A with its `chain_patch`), with a patch
    /// only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub chain: Option<Box<ChainConfig>>,
    /// Its chain's latency and compensation, as it plays (zeros while
    /// closed).
    #[serde(default)]
    pub latency: PathLatency,
}

/// Multi-device status (`status.outputs`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputsStatus {
    /// The master's added delay that aligns the followers, ms (0 without
    /// followers, at most [`MAX_ALIGNMENT_MS`]). The chain compensation
    /// (`master.compensation_us`) comes on top.
    pub alignment_ms: f64,
    /// What the followers need of it, ms (the most of their `need_ms`),
    /// kept through a pause: the alignment comes down to it plus 3 ms.
    /// `None` until measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub alignment_need_ms: Option<f64>,
    /// The alignment it shrinks to at the next silence (a pause, a gap
    /// between tracks, a new stream), ms; `None`: none waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub alignment_shrink_ms: Option<f64>,
    /// The master's chain latency, compensation and alignment, as it
    /// plays (zeros without followers).
    #[serde(default)]
    pub master: PathLatency,
    /// Every configured follower, in config order.
    pub followers: Vec<FollowerStatus>,
    /// Why the devices' alignment may still move (`None`: settled): a
    /// follower opening or locking, the master's alignment or chain
    /// compensation changed in the last few seconds or about to. A
    /// measurement waits for it to settle.
    #[serde(default)]
    pub settling: Option<String>,
}

impl OutputsStatus {
    /// What `cfg` reports before the engine runs its followers.
    pub fn from_config(cfg: &OutputsConfig) -> Self {
        Self {
            alignment_ms: 0.0,
            alignment_need_ms: None,
            alignment_shrink_ms: None,
            master: PathLatency::default(),
            followers: cfg
                .followers
                .iter()
                .map(|f| FollowerStatus {
                    device: f.device.clone(),
                    enabled: f.enabled,
                    trim_ms: f.trim_ms,
                    volume_db: f.volume_db,
                    follow_volume: f.follow_volume,
                    level_db: None,
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
                    period_ms: f.period_ms,
                    need_ms: None,
                    output_channels: f.output_channels,
                    device_channels: 0,
                    device_channels_max: 0,
                    error: None,
                    own_chain: Vec::new(),
                    chain: None,
                    latency: PathLatency::default(),
                })
                .collect(),
            settling: None,
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
    /// The USB device `device` was when it was chosen (USB cards only). Set
    /// by the daemon: at start-up and on every hotplug the daemon finds it
    /// again and renames `device` when ALSA named the cards in another
    /// order. A client's value is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub device_identity: Option<DeviceIdentity>,
    /// The device channels (0-based) the engine's left and right play on,
    /// for a device with more than two ([`OutputChannels`]). Left out: the
    /// first two. Set with `device` (a new device starts from the first
    /// two again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub output_channels: Option<OutputChannels>,
    /// Preferred output formats in order; the first the device accepts wins.
    #[serde(default = "default_formats")]
    pub formats: Vec<SampleFormat>,
    #[serde(default = "yes")]
    pub dsp_enabled: bool,
    /// Chain A (the normal chain).
    #[serde(default)]
    pub chain: ChainConfig,
    /// Name of the global preset chain A was last saved to or loaded from.
    #[serde(default)]
    pub active_preset: Option<String>,
    /// Each page's preset its part was last saved to or loaded from.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[cfg_attr(
        feature = "ts",
        ts(
            as = "Option<std::collections::BTreeMap<PresetPart, String>>",
            optional
        )
    )]
    pub active_part_presets: std::collections::BTreeMap<PresetPart, String>,
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
    /// Run the per-channel stages after crossfeed (EQ, convolution,
    /// upsampler, harmonics) on one thread per channel: the audio thread
    /// and a worker at its priority. Output is bit for bit the same either
    /// way; off runs everything on the audio thread. Only the main output's
    /// DSP chains with 2+ channels use it (not Direct, DSP off, bypass or
    /// followers).
    #[serde(default = "yes")]
    pub channel_workers: bool,
    /// Room-correction guards on the convolution (this machine's, not part
    /// of presets). Left out of the file at its defaults.
    #[serde(default, skip_serializing_if = "ConvolutionSafety::is_default")]
    #[cfg_attr(feature = "ts", ts(as = "Option<ConvolutionSafety>", optional))]
    pub convolution_safety: ConvolutionSafety,
    /// The room calibration a bundle installed (`bundle_load`), with what
    /// it replaced. Set by the daemon; left out without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub calibration: Option<CalibrationConfig>,
}

/// The room calibration a bundle installed ([`Call::BundleLoad`]): what it
/// put where (the items with absolute IR paths) and what unloading puts
/// back.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CalibrationConfig {
    /// The manifest, absolute.
    pub path: std::path::PathBuf,
    pub label: String,
    pub design_rate_hz: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub notes: Option<String>,
    /// The master's part: chain A's item tagged `calibration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub master: Option<ConvolutionConfig>,
    /// The master part's gain in the manifest, once `bundle_gain` moved
    /// its item's (left out: the item's gain is the manifest's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub master_manifest_gain_db: Option<f64>,
    #[serde(default)]
    pub followers: Vec<CalibrationFollower>,
}

/// A follower's part of a room calibration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CalibrationFollower {
    pub device: String,
    /// Its chain patch's whole `convolution` while the calibration is on.
    pub item: ConvolutionConfig,
    /// The trim the bundle set.
    pub trim_ms: f64,
    /// The part's gain in the manifest, once `bundle_gain` moved its
    /// item's (left out: the item's gain is the manifest's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub manifest_gain_db: Option<f64>,
    pub restore: CalibrationRestore,
}

/// What unloading a calibration puts back on a follower.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CalibrationRestore {
    /// The bundle added the follower: unloading removes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "ts", ts(as = "Option<bool>", optional))]
    pub added: bool,
    pub trim_ms: f64,
    /// Its chain patch's `convolution` before (`None`: the key was absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "unknown"))]
    pub convolution: Option<serde_json::Value>,
}

/// Side B's own room calibration in an A/B ([`AbConfig::calibration_b`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AbCalibration {
    /// The bundle (`None`: side B plays without a calibration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub path: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    /// Its master part, in place of chain A's calibration item on side B.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub master: Option<ConvolutionConfig>,
}

/// The loaded room calibration (`Status.calibration`, [`Call::BundleShow`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CalibrationStatus {
    pub label: String,
    pub path: String,
    pub design_rate_hz: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub notes: Option<String>,
    /// The master first, then the followers in the bundle's order.
    pub devices: Vec<CalibrationDeviceStatus>,
    /// The rates now: the graph rate against the design rate, devices at
    /// different DAC rates.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// One device's part of the loaded calibration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CalibrationDeviceStatus {
    /// `master` or `follower`.
    pub role: String,
    pub device: String,
    /// Matrix entries (filters) in its part.
    pub entries: u32,
    /// Its item's gain as it plays now (`bundle_gain`).
    pub gain_db: f64,
    /// The manifest's gain for the part (what a reset goes back to).
    pub manifest_gain_db: f64,
    /// A follower's trim as it is now (the bundle's unless edited since);
    /// 0 on the master.
    pub trim_ms: f64,
    /// Its part plays: `false` when the follower was removed since.
    pub installed: bool,
}

/// The most a chain's convolution may boost by default, dB.
pub const DEFAULT_CONV_MAX_BOOST_DB: f64 = 12.0;
/// [`ConvolutionSafety::max_boost_db`]'s range, dB.
pub const CONV_MAX_BOOST_RANGE_DB: (f64, f64) = (0.0, 24.0);
/// The subsonic limit by default, Hz.
pub const DEFAULT_CONV_SUBSONIC_HZ: f64 = 20.0;
/// [`ConvolutionSafety::subsonic_hz`]'s range, Hz.
pub const CONV_SUBSONIC_RANGE_HZ: (f64, f64) = (5.0, 60.0);

/// Room-correction guards on each chain's combined convolution, per output
/// row (a filter matrix output sums its entries).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct ConvolutionSafety {
    /// The most a row may boost (its certified sine worst-case gain), dB
    /// (0..=24, default 12). A chain over it is refused.
    pub max_boost_db: f64,
    /// Warn when a row has more than 0 dB of gain below this frequency, Hz
    /// (5..=60, default 20): a sub's driver has limited excursion there.
    pub subsonic_hz: f64,
}

impl Default for ConvolutionSafety {
    fn default() -> Self {
        Self {
            max_boost_db: DEFAULT_CONV_MAX_BOOST_DB,
            subsonic_hz: DEFAULT_CONV_SUBSONIC_HZ,
        }
    }
}

impl ConvolutionSafety {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn validate(&self) -> Result<(), String> {
        let (lo, hi) = CONV_MAX_BOOST_RANGE_DB;
        if !self.max_boost_db.is_finite() || !(lo..=hi).contains(&self.max_boost_db) {
            return Err(format!(
                "convolution_safety.max_boost_db must be in {lo}..={hi}"
            ));
        }
        let (lo, hi) = CONV_SUBSONIC_RANGE_HZ;
        if !self.subsonic_hz.is_finite() || !(lo..=hi).contains(&self.subsonic_hz) {
            return Err(format!(
                "convolution_safety.subsonic_hz must be in {lo}..={hi}"
            ));
        }
        Ok(())
    }
}

/// What a convolution warning is about.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum ConvWarningKind {
    /// More than 0 dB of gain below the subsonic limit.
    Subsonic,
    /// The limiter is off and the convolution boosts more than the output
    /// headroom covers: loud passages can clip.
    ClipRisk,
}

/// One output row's convolution warning ([`Status::conv_warnings`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConvWarning {
    /// The chain (B only with A/B).
    pub side: AbSide,
    /// A follower's own chain (`outputs.followers[].chain_patch`): its
    /// device; absent for the main output's chains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub device: Option<String>,
    /// The output channel (0 = L, 1 = R).
    pub output: u32,
    pub kind: ConvWarningKind,
    /// `subsonic`: the largest gain below the limit; `clip_risk`: the
    /// row's peak gain. dB.
    pub gain_db: f64,
    /// `subsonic`: where that gain is, Hz.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// `subsonic`: the limit (`convolution_safety.subsonic_hz`), Hz.
    #[serde(default)]
    pub limit_hz: Option<f64>,
    /// `clip_risk`: the output headroom (`chain.output.headroom_db`), dB.
    #[serde(default)]
    pub headroom_db: Option<f64>,
    /// A one-line description, e.g. "Below 20 Hz: +4.2 dB on L".
    pub message: String,
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
            device_identity: None,
            output_channels: None,
            formats: default_formats(),
            dsp_enabled: true,
            chain: ChainConfig::default(),
            active_preset: None,
            active_part_presets: Default::default(),
            ab: None,
            rate: RatePolicy::default(),
            quantum: default_quantum(),
            routing: RoutingMode::default(),
            volume: VolumeConfig::default(),
            varispeed: VarispeedConfig::default(),
            device_lost: DeviceLostAction::default(),
            outputs: OutputsConfig::default(),
            channel_workers: true,
            convolution_safety: ConvolutionSafety::default(),
            calibration: None,
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
    /// Side B's own room calibration on the master, in place of chain A's
    /// (`bundle_ab`); left out, B keeps A's calibration whatever `b` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub calibration_b: Option<AbCalibration>,
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
            calibration_b: None,
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
    /// From each side's predicted worst-case peak gain after matching:
    /// what B needs beyond A's own peaks (A alone already plays them),
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
    /// The side playing; `None` while a blind test runs (`blind`).
    pub current: Option<AbSide>,
    /// A blind test runs (`ab_blind`): `current` is hidden and the overs
    /// read 0.
    #[serde(default)]
    pub blind: bool,
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
    /// delay plus [`Self::filter_latency_ms`], the varispeed stage and the
    /// multi-device alignment (`outputs.alignment_ms`).
    pub latency_ms: f64,
    /// The part of [`Self::latency_ms`] that is filter delay in the active
    /// chain (upsampler, convolution and limiter), ms. 0 on the Direct sink or with
    /// DSP off, where the chain is not in the path.
    #[serde(default)]
    pub filter_latency_ms: f64,
    /// The convolution's own delay in the active chain, ms at the graph
    /// rate: the IRs' bulk delay as the engine runs them (where the largest
    /// one peaks, after resampling to the graph rate and combining) plus
    /// the convolver's partition block. The block is in
    /// [`Self::filter_latency_ms`]; the bulk delay is the IRs' own and is
    /// not. 0 without convolution, on the Direct sink or with DSP off.
    /// With a calibration bundle: the master's convolution only (each
    /// follower's latency is in `outputs`). The playing side's in A/B.
    #[serde(default)]
    pub conv_latency_ms: f64,
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
    /// Active global preset, when chain A was saved to or loaded from one.
    #[serde(default)]
    pub preset: Option<PresetState>,
    /// Each page's active preset (pages without one are left out).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<PartPresetState>>", optional))]
    pub part_presets: Vec<PartPresetState>,
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
    /// The requested upsampler at each source rate the engine would land
    /// (chain A's, with DSP on): its estimated load, and what runs instead
    /// where it is over budget or overloaded. Empty with DSP or the
    /// upsampler off, or before the CPU is measured.
    #[serde(default)]
    pub upsampler_rates: Vec<RateAdmission>,
    /// The [`Self::upsampler_rates`] row of the source rate playing now on
    /// the DSP chain, when it plays a fallback there instead of the
    /// requested upsampler; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub upsampler_fallback_now: Option<RateAdmission>,
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
    /// Warnings about the convolution the engine validated last (each
    /// chain's rows): energy below the subsonic limit, and clipping risk
    /// with the limiter off. Empty without convolution.
    #[serde(default)]
    pub conv_warnings: Vec<ConvWarning>,
    /// The loaded room calibration (`None`: none).
    #[serde(default)]
    pub calibration: Option<CalibrationStatus>,
    /// Outputs held closed because identical DACs moved USB ports and the
    /// daemon cannot tell which is which: [`Call::AssignDevices`] settles
    /// it. Empty normally.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<UnresolvedDevice>>", optional))]
    pub unresolved_devices: Vec<UnresolvedDevice>,
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
    /// Channel count the probe used: the one the engine opens the device
    /// at (the smallest it takes of at least 2, or more for
    /// `output_channels` reaching past them). The engine's two channels
    /// land on `output_channels` (default the first two), the others get
    /// exact zeros.
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
    /// Every channel count of at least the engine's that the device takes,
    /// ascending, each with its own usable (rate, format) pairs. `channels`
    /// and `rates` are one of them, picked for the output pair and the
    /// configured formats ([`Self::for_output`]). In-process only: not on
    /// the wire (a probe read back from JSON has none, and is its own view).
    #[serde(skip)]
    #[cfg_attr(feature = "ts", ts(skip))]
    pub modes: Vec<ChannelMode>,
}

/// One channel count's usable (rate, format) pairs
/// ([`DeviceCapabilities::modes`]).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ChannelMode {
    pub channels: u32,
    /// Ascending by rate; only rates with at least one usable format.
    pub rates: Vec<RateCapability>,
}

impl ChannelMode {
    /// Plays one of `formats` (empty: any) at some rate.
    pub fn plays_any(&self, formats: &[SampleFormat]) -> bool {
        self.rates.iter().any(|r| {
            r.formats
                .iter()
                .any(|f| formats.is_empty() || formats.contains(&f.format))
        })
    }
}

impl DeviceCapabilities {
    /// The view for an output pair that needs `min_channels` and the
    /// configured `formats` (in preference order; empty: any): `channels`
    /// and `rates` from the smallest mode of at least `min_channels` that
    /// plays one of `formats`, else the smallest mode that holds the pair
    /// (its rates, none of them in `formats`), else `min_channels` with no
    /// rates (the pair is past the device). A probe without modes is its
    /// own view. Narrowest first: a stereo device with the default pair
    /// keeps its 2-channel mode even when a wider one has more rates.
    pub fn for_output(&self, min_channels: u32, formats: &[SampleFormat]) -> Self {
        if self.modes.is_empty() {
            return self.clone();
        }
        let mut out = self.clone();
        let mut fits = self.modes.iter().filter(|m| m.channels >= min_channels);
        let pick = fits
            .clone()
            .find(|m| m.plays_any(formats))
            .or_else(|| fits.next());
        match pick {
            Some(m) => {
                out.channels = m.channels;
                out.rates = m.rates.clone();
            }
            None => {
                out.channels = min_channels;
                out.rates = Vec::new();
            }
        }
        out
    }

    /// The modes an open needing `min_channels` may use, narrowest first
    /// (the view's own channel count and rates for a probe without modes).
    pub fn modes_for(&self, min_channels: u32) -> Vec<ChannelMode> {
        if self.modes.is_empty() {
            return vec![ChannelMode {
                channels: self.channels,
                rates: self.rates.clone(),
            }];
        }
        self.modes
            .iter()
            .filter(|m| m.channels >= min_channels)
            .cloned()
            .collect()
    }

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
            if self.channels_max == 1 {
                return Some(format!(
                    "device '{device}' is mono (1 channel): the engine plays 2 channels and needs a device with at least 2 (device supports {supports})"
                ));
            }
            return Some(format!(
                "device '{device}' needs {} or more channels; device supports {has} (engine: channels {}; device: {supports})",
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

/// Where a measurement plays the file ([`MeasurePlayRequest::chain`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum MeasureChain {
    /// After the chain: its stages are skipped; the device's volume and
    /// timing are kept (see `docs/control-plane.md`, "Measurement").
    #[default]
    Bypass,
    /// At the chain's input, through every stage (a verification run).
    Active,
}

/// One output channel a measurement plays on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MeasureRoute {
    /// The main device (`"master"`, or its configured name) or a
    /// follower's device.
    pub device: String,
    /// The engine's channel, from 0 (0 left, 1 right): it plays on the
    /// device channel the device's `output_channels` map it to (by
    /// default the same one).
    pub channel: u32,
}

fn default_true() -> bool {
    true
}

fn default_keepalive_db() -> Option<f64> {
    Some(-100.0)
}

fn default_lead_in_ms() -> f64 {
    2000.0
}

/// [`Call::MeasurePlay`]'s parameters.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MeasurePlayRequest {
    /// A WAV file on the daemon's machine (16/24/32-bit integer or 32-bit
    /// float). One channel plays on every route; otherwise one channel per
    /// route, in order.
    pub file: String,
    pub routes: Vec<MeasureRoute>,
    #[serde(default)]
    pub chain: MeasureChain,
    /// Every other stream is silent while it plays (ramped).
    #[serde(default = "default_true")]
    pub mute_others: bool,
    /// Noise under the file on every routed channel (RMS, dBFS; `null`:
    /// none), from the lead-in to the file's end: keeps DACs that mute on
    /// digital silence awake. `active` plays it after the chain (where a
    /// low-pass chain cannot take it down), at that level.
    /// -140..=-60.
    #[serde(default = "default_keepalive_db")]
    pub keepalive_db: Option<f64>,
    /// The keepalive alone before the file, ms (1500..=10000).
    #[serde(default = "default_lead_in_ms")]
    pub lead_in_ms: f64,
}

/// Where a measurement is.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum MeasureState {
    /// None has run since the daemon started.
    #[default]
    Idle,
    /// Waiting for the engine to play and the followers to lock, loading
    /// and resampling the file.
    Preparing,
    /// The keepalive alone, before the file.
    LeadIn,
    Playing,
    /// The file has been handed to every device; waiting for it to leave
    /// the DACs before the mute lifts.
    Tail,
    /// Played to the end.
    Done,
    /// Stopped by `measure_stop`.
    Stopped,
    /// Ended early: see `error`.
    Error,
}

/// One device's part of a measurement, and its timing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MeasureDeviceTiming {
    pub device: String,
    /// `"master"` or `"follower"`.
    pub role: String,
    /// The routed channels, in route order.
    pub channels: Vec<u32>,
    /// The device's output rate, Hz.
    pub rate_hz: u32,
    /// The rate the file plays at where it is injected (the output rate
    /// for `bypass` on the main device, the graph rate otherwise), Hz.
    pub injection_rate_hz: u32,
    /// The file was resampled offline to `injection_rate_hz`.
    pub resampled: bool,
    /// From the injection point to the DAC: the processing in between
    /// (`chain_latency_frames`) plus the device queue (`queue_frames`),
    /// device frames and µs. 0 until the file starts.
    pub latency_frames: f64,
    pub latency_us: f64,
    pub chain_latency_frames: f64,
    pub queue_frames: f64,
    /// The engine's output frame (its count for this device) that carries
    /// the file's first frame; `null` until it is written.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub start_frame: Option<u64>,
    /// When that frame leaves the device queue (`CLOCK_MONOTONIC_RAW`,
    /// ns); `null` until the device has been read after it.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub start_time_ns: Option<u64>,
    /// File frames handed to this device's injection point so far (what
    /// the engine fed it, not what its DAC played: see `drained` and
    /// `continuity`).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub frames_played: u64,
    /// Samples the measurement clamped at its ceiling (−1 dBFS) on this
    /// device: the file plus the keepalive (and a resampler's overshoot),
    /// or in `active` without a limiter the chain's whole output.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub clamped_samples: u64,
    /// The device's DAC played the last frame the measurement touched.
    #[serde(default)]
    pub drained: bool,
    /// The playback continuity check, once the run ended: `null` before,
    /// or with too few DAC readings.
    #[serde(default)]
    pub continuity: Option<MeasureContinuity>,
}

/// A device's DAC position (frames written minus the queue, as ALSA
/// reports it) against `CLOCK_MONOTONIC_RAW` through a run, from its first
/// touched frame until it drained. A step in the timeline the host can see
/// (a lost or repeated USB packet the driver counts) shows as a jump; a
/// drop inside the DAC, after what the driver reports, does not. The fit
/// takes out the clocks' offset and a slow drift, and a lone reading off
/// its neighbours is a stray, not a step.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MeasureContinuity {
    /// Readings fitted (at most one every 2 ms).
    pub readings: u32,
    /// The farthest reading from a slowly drifting clock (a quadratic
    /// through the readings, the strays left out), device frames and µs.
    pub max_residual_frames: f64,
    pub max_residual_us: f64,
    /// The largest step in the position: the difference between the
    /// medians of the 8 readings on either side, once the rate is taken
    /// out, device frames and µs (positive: the position ran ahead), and
    /// when, seconds from the file's first frame at the DAC (negative: in
    /// the lead-in).
    pub jump_frames: f64,
    pub jump_us: f64,
    pub jump_at_s: f64,
    /// The readings' timing jitter (a robust spread), µs: a step or a
    /// residual counts only well above it.
    #[serde(default)]
    pub jitter_us: f64,
    /// Readings off their neighbours (a late timestamp), left out.
    #[serde(default)]
    pub strays: u32,
    /// Readings the buffer had no room for (the check covers the run up
    /// to there).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub dropped: u64,
}

/// A measurement's progress and timing ([`Call::MeasurePlay`],
/// [`Call::MeasureStatus`], [`Call::MeasureStop`]).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MeasureStatus {
    /// Counts runs since the daemon started (0: none).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub run: u64,
    pub state: MeasureState,
    pub error: Option<String>,
    pub file: Option<String>,
    pub chain: Option<MeasureChain>,
    pub mute_others: bool,
    pub keepalive_db: Option<f64>,
    pub lead_in_ms: f64,
    /// The file's own rate, Hz, and length, frames and seconds.
    pub file_rate_hz: u32,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub file_frames: u64,
    pub duration_s: f64,
    /// Seconds of the file every device has played.
    pub progress_s: f64,
    /// The graph rate it runs at, Hz.
    pub graph_rate_hz: u32,
    /// The source frame (the engine's count of graph frames) the file
    /// starts at on every device.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub source_frame: Option<u64>,
    pub devices: Vec<MeasureDeviceTiming>,
    /// What the engine did to play it (e.g. a resampling).
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_play_defaults() {
        let r: MeasurePlayRequest = serde_json::from_value(serde_json::json!({
            "file": "/tmp/x.wav",
            "routes": [{"device": "master", "channel": 0}]
        }))
        .unwrap();
        assert_eq!(r.chain, MeasureChain::Bypass);
        assert!(r.mute_others);
        assert_eq!(r.keepalive_db, Some(-100.0));
        assert_eq!(r.lead_in_ms, 2000.0);
        let off: MeasurePlayRequest = serde_json::from_value(serde_json::json!({
            "file": "/tmp/x.wav", "routes": [], "chain": "active", "keepalive_db": null
        }))
        .unwrap();
        assert_eq!(off.chain, MeasureChain::Active);
        assert_eq!(off.keepalive_db, None);
        let call: Request = serde_json::from_str(
            r#"{"id":1,"method":"measure_play","params":{"file":"a.wav","routes":[]}}"#,
        )
        .unwrap();
        assert!(matches!(call.call, Call::MeasurePlay(_)));
        let stop: Request = serde_json::from_str(r#"{"id":2,"method":"measure_stop"}"#).unwrap();
        assert_eq!(stop.call, Call::MeasureStop);
    }

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
        let blind: Request =
            serde_json::from_str(r#"{"id":3,"method":"ab_blind","params":{"enabled":true}}"#)
                .unwrap();
        assert_eq!(blind.call, Call::AbBlind { enabled: true });
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
                device: "hw:CARD=DAC,DEV=0".into(),
                output_channels: None,
            }
        );
        assert_eq!(
            parse(
                r#"{"id":5,"method":"set_device","params":{"device":"hw:CARD=USB,DEV=0","output_channels":[2,3]}}"#
            ),
            Call::SetDevice {
                device: "hw:CARD=USB,DEV=0".into(),
                output_channels: Some(OutputChannels([2, 3])),
            }
        );
    }

    #[test]
    fn a_pair_views_the_narrowest_mode_that_plays_its_formats() {
        let rows = |f: SampleFormat, rates: &[u32]| -> Vec<RateCapability> {
            rates
                .iter()
                .map(|&rate| RateCapability {
                    rate,
                    formats: vec![FormatCapability {
                        format: f,
                        sbits: None,
                    }],
                })
                .collect()
        };
        let probe = DeviceCapabilities {
            device: "hw:X".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 8,
            modes: vec![
                ChannelMode {
                    channels: 2,
                    rates: rows(SampleFormat::S16, &[48_000]),
                },
                ChannelMode {
                    channels: 8,
                    rates: rows(SampleFormat::S32, &[48_000, 192_000]),
                },
            ],
            ..Default::default()
        };
        let all = [
            SampleFormat::S32,
            SampleFormat::S24,
            SampleFormat::S24_3,
            SampleFormat::S16,
        ];
        // The default pair keeps stereo, though 8 channels play more.
        let v = probe.for_output(2, &all);
        assert_eq!(v.channels, 2);
        assert_eq!(v.supported_rates(), [48_000]);
        // S32 only: the 8-channel mode.
        assert_eq!(probe.for_output(2, &[SampleFormat::S32]).channels, 8);
        // A pair past every mode: no rates, the count it needs.
        let past = probe.for_output(10, &all);
        assert_eq!((past.channels, past.rates.len()), (10, 0));
        assert!(past.unusable_reason().unwrap().contains("needs 10 or more"));
        // Not on the wire.
        let back: DeviceCapabilities =
            serde_json::from_str(&serde_json::to_string(&probe).unwrap()).unwrap();
        assert!(back.modes.is_empty());
        assert_eq!(back.for_output(8, &all), back);
    }

    #[test]
    fn output_channels_wire_shape_and_checks() {
        let p: OutputChannels = serde_json::from_str("[2, 3]").unwrap();
        assert_eq!(p, OutputChannels([2, 3]));
        assert_eq!(serde_json::to_string(&p).unwrap(), "[2,3]");
        assert_eq!(p.min_device_channels(), 4);
        assert_eq!(p.error(), None);
        assert!(OutputChannels([1, 1]).error().is_some());
        assert!(OutputChannels([0, 64]).error().is_some());
        assert_eq!(p.fit_error("hw:X", 4), None);
        assert_eq!(p.fit_error("hw:X", 0), None, "unknown count");
        assert!(
            p.fit_error("hw:X", 2)
                .unwrap()
                .contains("has 2 channels (0 to 1)")
        );
        assert_eq!(
            OutputChannels::normalized(Some(OutputChannels::DEFAULT)),
            None
        );
        // Left out of a config without one; an old config loads.
        let cfg = EngineConfig::default();
        let text = serde_json::to_string(&cfg).unwrap();
        assert!(!text.contains("output_channels"));
        let cfg: EngineConfig = serde_json::from_str(r#"{"device":"hw:0,0"}"#).unwrap();
        assert_eq!(cfg.output_channels, None);
        let f: FollowerConfig =
            serde_json::from_str(r#"{"device":"hw:1,0","output_channels":[4,5]}"#).unwrap();
        assert_eq!(f.output_channels, Some(OutputChannels([4, 5])));
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
    fn preset_parts_cover_copy_and_compare() {
        // Every part on the wire by its snake_case name.
        for part in PresetPart::ALL {
            let text = serde_json::to_string(&part).unwrap();
            assert_eq!(text, format!("\"{}\"", part.name()));
            assert_eq!(PresetPart::from_name(part.name()), Some(part));
        }
        let request: Request = serde_json::from_str(
            r#"{"id":1,"method":"load_preset","params":{"name":"X","part":"eq"}}"#,
        )
        .unwrap();
        assert_eq!(
            request.call,
            Call::LoadPreset {
                name: "X".into(),
                part: Some(PresetPart::Eq),
            }
        );

        // An old file holds every part; `parts` narrows it, `part` wins.
        let old: Preset = serde_json::from_str(r#"{"name":"Old","chain":{}}"#).unwrap();
        assert_eq!(old.covers(), PresetPart::ALL.to_vec());
        let mut some = Preset::new("Some", ChainConfig::neutral());
        some.parts = Some(vec![PresetPart::Output, PresetPart::Eq]);
        assert_eq!(some.covers(), vec![PresetPart::Eq, PresetPart::Output]);
        let page = Preset::for_part("Page", PresetPart::Output, ChainConfig::neutral());
        assert_eq!(page.covers(), vec![PresetPart::Output]);

        // Copying one part leaves the rest bit-identical; the output part
        // holds DC block, pitch, limiter and the whole output.
        let a = ChainConfig::default();
        let mut b = ChainConfig::neutral();
        b.output.bits = 16;
        b.output.headroom_db = 6.0;
        b.eq = Some(EqProfile::default());
        let mut out = a.clone();
        PresetPart::Output.copy(&b, &mut out);
        assert_eq!(out.output, b.output);
        assert_eq!(out.limiter, b.limiter);
        assert_eq!(out.eq, a.eq);
        assert!(PresetPart::Output.same(&out, &b));
        assert!(!PresetPart::Output.same(&out, &a));
        for part in PresetPart::ALL {
            if part != PresetPart::Output {
                assert!(part.same(&out, &a), "{part:?}");
            }
        }
        // A full preset applied is the preset's chain.
        assert_eq!(Preset::new("All", b.clone()).apply_to(&a), b);
        assert!(Preset::new("All", b.clone()).matches(&b));
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
                part: None,
                parts: None,
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
            part: None,
            parts: Vec::new(),
            modified_unix: 1_700_000_000,
            bands: 3,
            convolution: 1,
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"name":"Bass","parts":[],"modified_unix":1700000000,"bands":3,"convolution":1}"#
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
            conv_latency_ms: 0.0,
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
            part_presets: Vec::new(),
            device_caps: None,
            effective_rates: Vec::new(),
            direct_rates: Vec::new(),
            playable_rates: Vec::new(),
            effective_default_rate: None,
            dither_bits: None,
            limiter_held_db: 0.0,
            normal_output: None,
            overload: None,
            upsampler_rates: Vec::new(),
            upsampler_fallback_now: None,
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
            conv_warnings: Vec::new(),
            calibration: None,
            unresolved_devices: Vec::new(),
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
    fn conv_response_wire_format() {
        let request: Request =
            serde_json::from_str(r#"{"id":1,"method":"conv_response","params":{}}"#).unwrap();
        assert_eq!(
            request.call,
            Call::ConvResponse {
                side: None,
                device: None,
            }
        );
        let request: Request = serde_json::from_str(
            r#"{"id":2,"method":"conv_response","params":{"side":"b","device":"hw:CARD=USB,DEV=0"}}"#,
        )
        .unwrap();
        assert_eq!(
            request.call,
            Call::ConvResponse {
                side: Some(AbSide::B),
                device: Some("hw:CARD=USB,DEV=0".into()),
            }
        );
        let curve = ConvResponseCurve {
            input: Some(1),
            output: Some(0),
            mag_db: vec![-3.0, -200.0],
            group_delay_ms: vec![Some(1.5), None],
        };
        let v = serde_json::to_value(&curve).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"in": 1, "out": 0, "mag_db": [-3.0, -200.0], "group_delay_ms": [1.5, null]})
        );
        let mono = ConvResponseCurve {
            input: None,
            output: None,
            ..curve
        };
        let v = serde_json::to_value(&mono).unwrap();
        assert!(v.get("in").is_none() && v.get("out").is_none());
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
            notes: vec![],
            advice: None,
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
            modes: Vec::new(),
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
        // A follower added by hand keeps its own level.
        assert!(!cfg.outputs.followers[0].follow_volume);
        let req: Request = serde_json::from_str(
            r#"{"id":4,"method":"set_follower","params":{"device":"hw:1,0","follow_volume":false}}"#,
        )
        .unwrap();
        let Call::SetFollower { follow_volume, .. } = req.call else {
            panic!("{:?}", req.call)
        };
        assert_eq!(follow_volume, Some(false));
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
                output_channels: None,
                follow_volume: None,
                chain_patch: None,
                period_ms: None,
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
        assert_eq!(v["followers"][0]["own_chain"], serde_json::json!([]));
        assert_eq!(v["master"]["total_us"], 0.0);
        // A follower's own chain: absent when it has none, a merge patch
        // otherwise.
        assert!(
            serde_json::to_value(&cfg.outputs.followers[0])
                .unwrap()
                .get("chain_patch")
                .is_none()
        );
        let req: Request = serde_json::from_str(
            r#"{"id":3,"method":"set_follower","params":{"device":"hw:1,0","chain_patch":{"convolution":[]}}}"#,
        )
        .unwrap();
        let Call::SetFollower { chain_patch, .. } = req.call else {
            panic!("{:?}", req.call)
        };
        assert_eq!(chain_patch, Some(serde_json::json!({"convolution": []})));
        let l = PathLatency::from_frames(48_000, 1024.0, 96.0, 48.0);
        assert_eq!((l.total_frames, l.chain_us), (1168.0, 1024.0 / 48e3 * 1e6));
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
