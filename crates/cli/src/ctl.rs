//! `avdsp ctl` — talk to the running daemon over its Unix socket.
//!
//! One subcommand per IPC call. Output is human-readable by default and raw
//! JSON with `--json`. A daemon error reply exits 1 with the daemon's message;
//! a missing daemon exits 3 with a hint.

use anyhow::{Context, Result};
use audivane_dsp_core::design::budget::MAX_KERNEL_TAPS;
use audivane_protocol::ipc::{
    AbB, AbConfig, AbSide, Call, EngineConfig, FollowerState, FollowerStatus, LevelMatch,
    LevelWeighting, OutputsStatus, RouteTarget, RoutingMode, SinkChoice, Status, Topic,
    VarispeedConfig, VarispeedOverloadHold, VarispeedStatus,
};
use audivane_protocol::{
    Band, ChainConfig, ChannelEq, ConvolutionConfig, CrossfeedConfig, CrossfeedLevel,
    CrossfeedMode, CutoffMode, DcBlockConfig, EqProfile, FilterPhase, FilterPreset, FilterType,
    HarmonicsCharacter, HarmonicsConfig, PitchConfig, PitchQuality, RateTarget, SampleFormat,
    TapsUnit, TiltConfig, TiltPivot, UpsamplerConfig, WindowKind,
};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::Value;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `bail!` that produces a [`CtlError`] (a usage error, exit code 3).
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(CtlError::Other(::anyhow::anyhow!($($arg)*)))
    };
}

#[derive(Args, Debug)]
pub struct CtlArgs {
    /// Control socket (default: $AUDIVANE_DSP_SOCKET or
    /// $XDG_RUNTIME_DIR/audivane-dsp.sock).
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Print raw JSON instead of human-readable output.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: CtlCommand,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum LimiterModeArg {
    Off,
    Limit,
    AutoHeadroom,
}

#[derive(Subcommand, Debug)]
pub enum CtlCommand {
    /// Show engine status, including passthrough integrity.
    Status,
    /// Read or replace the whole configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Configure the DC-blocking high-pass filter before pitch and crossfeed.
    DcBlock {
        /// Turn DC blocking on or off (omit to inspect, or adjust --cutoff).
        #[arg(value_enum)]
        state: Option<OnOff>,
        /// High-pass cutoff frequency in Hz (1..=20, default 5).
        #[arg(long)]
        cutoff: Option<f64>,
    },
    /// Configure headphone crossfeed (BS2B or Linkwitz).
    Crossfeed {
        #[command(subcommand)]
        command: CrossfeedCommand,
    },
    /// Set, inspect, or clear the parametric EQ of chain A.
    Eq {
        #[command(subcommand)]
        command: EqCommand,
    },
    /// Manage the convolution impulse response chain.
    Conv {
        #[command(subcommand)]
        command: ConvCommand,
    },
    /// Set upsampling (ratio, max rate, or off).
    Upsample(UpsampleArgs),
    /// Upsampling filter designs: expand a preset, check a design file.
    Design {
        #[command(subcommand)]
        command: DesignCommand,
    },
    /// Set or inspect tempo-preserving pitch shifting.
    Pitch(PitchArgs),
    /// Set or inspect variable-speed playback (0.5..=2.0, on, or off).
    Varispeed(VarispeedArgs),
    /// Multi-device: the master's alignment and every follower's state.
    Outputs,
    /// Multi-device: add, edit or remove a follower (another DAC playing
    /// the same source in sync).
    Follower(FollowerArgs),
    /// Configure harmonic distortion ("tube" / THD) after the upsampler.
    Harmonics {
        #[command(subcommand)]
        command: HarmonicsCommand,
    },
    /// Inspect or set post-volume peak protection (off, limit, auto-headroom).
    Limiter {
        #[arg(value_enum)]
        mode: Option<LimiterModeArg>,
        #[arg(long, allow_hyphen_values = true)]
        ceiling: Option<f64>,
        #[arg(long)]
        lookahead: Option<f64>,
        #[arg(long)]
        release: Option<f64>,
    },
    /// Inspect the output format or set its fixed headroom.
    Output {
        /// Headroom in dB (0..12) taken off at the end of the chain, before
        /// volume and the limiter, at every ratio.
        #[arg(long)]
        headroom: Option<f64>,
    },
    /// Turn all DSP off/on, keeping the settings.
    Dsp {
        #[arg(value_enum)]
        state: OnOff,
    },
    /// A/B comparison: configure chain B, switch sides, or turn it off.
    Ab {
        #[command(subcommand)]
        command: AbCommand,
    },
    /// Show or set master volume, L/R balance, and mute.
    Volume(VolumeArgs),
    /// Show or change the allowed graph sample rates and the default rate.
    Rates(RatesArgs),
    /// List streams in the PipeWire graph.
    Streams,
    /// One app stream on our sinks: show or set its volume and mute.
    Stream {
        #[command(subcommand)]
        command: StreamCommand,
    },
    /// Presets stored by the daemon (chain A's signal path).
    Preset {
        #[command(subcommand)]
        command: PresetCommand,
    },
    /// List ALSA playback devices (from /proc/asound; nothing is opened).
    Devices,
    /// Move a stream (by node id or app) to one of our sinks.
    Route(RouteArgs),
    /// Choose system-wide routing or per-app rules.
    Mode {
        #[command(subcommand)]
        command: ModeCommand,
    },
    /// Show the current MPRIS track.
    NowPlaying,
    /// Run the bit-exactness self-test through the Direct path.
    Selftest,
    /// Give the DAC back to the session (engine idles).
    Release,
    /// Take the DAC again.
    Acquire,
    /// Subscribe to live events.
    Watch(WatchArgs),
    /// Stop the daemon.
    Shutdown,
}

#[derive(Args, Debug)]
pub struct RatesArgs {
    /// Allow these rates in Hz (e.g. 352800 384000).
    #[arg(long, value_name = "HZ", num_args = 1..)]
    pub enable: Vec<u32>,
    /// Stop allowing these rates; a disabled default moves to the nearest
    /// allowed rate (same family first).
    #[arg(long, value_name = "HZ", num_args = 1..)]
    pub disable: Vec<u32>,
    /// Make this (allowed) rate the default.
    #[arg(long, value_name = "HZ")]
    pub default: Option<u32>,
    /// Who converts the rate for Direct and DSP off when the DAC does not
    /// play the source rate: the engine (stays at the source rate, a fixed
    /// filter to the DAC's native rate; the default) or PipeWire (moves the
    /// graph and resamples the stream). A native rate is always bit-perfect.
    #[arg(long, value_enum, value_name = "WHO")]
    pub bypass_resampler: Option<BypassResamplerArg>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BypassResamplerArg {
    Engine,
    Pipewire,
}

impl From<BypassResamplerArg> for audivane_protocol::ipc::BypassResampler {
    fn from(a: BypassResamplerArg) -> Self {
        match a {
            BypassResamplerArg::Engine => Self::Engine,
            BypassResamplerArg::Pipewire => Self::Pipewire,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Print the current configuration as JSON.
    Get,
    /// Replace the configuration from a JSON file (`-` reads stdin). The
    /// level (volume, balance, mute) is kept: set it with `volume`.
    Set {
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum EqSectionArg {
    Linked,
    Left,
    Right,
}

impl EqSectionArg {
    fn label(self) -> &'static str {
        match self {
            Self::Linked => "L+R",
            Self::Left => "L",
            Self::Right => "R",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SectionActionArg {
    On,
    Off,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum FilterKindArg {
    #[value(alias = "pk", alias = "peq")]
    Peaking,
    #[value(alias = "ls", alias = "lsc")]
    LowShelf,
    #[value(alias = "hs", alias = "hsc")]
    HighShelf,
    #[value(alias = "lp", alias = "lpq")]
    LowPass,
    #[value(alias = "hp", alias = "hpq")]
    HighPass,
    #[value(alias = "bp")]
    BandPass,
    #[value(alias = "no")]
    Notch,
    #[value(alias = "ap")]
    AllPass,
}

impl From<FilterKindArg> for FilterType {
    fn from(value: FilterKindArg) -> Self {
        match value {
            FilterKindArg::Peaking => Self::Peaking,
            FilterKindArg::LowShelf => Self::LowShelf,
            FilterKindArg::HighShelf => Self::HighShelf,
            FilterKindArg::LowPass => Self::LowPass,
            FilterKindArg::HighPass => Self::HighPass,
            FilterKindArg::BandPass => Self::BandPass,
            FilterKindArg::Notch => Self::Notch,
            FilterKindArg::AllPass => Self::AllPass,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum CrossfeedCommand {
    /// Print the current crossfeed setting.
    Show,
    /// Turn crossfeed off.
    Off,
    /// BS2B default preset (700 Hz, 4.5 dB).
    Default,
    /// BS2B Chu Moy preset (700 Hz, 6.0 dB).
    ChuMoy,
    /// BS2B Jan Meier preset (650 Hz, 9.5 dB).
    JanMeier,
    /// Linkwitz-style crossfeed (mono-flat, 0.3 ms interaural delay, -6.0 dB).
    Linkwitz,
    /// Custom BS2B parameters.
    Custom {
        /// Low-pass cutoff frequency in Hz (300..=2000).
        #[arg(long)]
        cutoff_hz: f64,
        /// Low-frequency crossfeed attenuation in dB (1..=15).
        #[arg(long)]
        feed_db: f64,
    },
    /// Level policy: `matched` keeps the loudness the same with crossfeed on
    /// and off (rare worst-case peaks clip at the output unless the limiter
    /// is on or the digital volume leaves room; hardware volume does not),
    /// `peak-safe` can never go over but is quieter. Presets keep it.
    Level {
        #[arg(value_enum)]
        level: CrossfeedLevelArg,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CrossfeedLevelArg {
    Matched,
    PeakSafe,
}

impl From<CrossfeedLevelArg> for CrossfeedLevel {
    fn from(arg: CrossfeedLevelArg) -> Self {
        match arg {
            CrossfeedLevelArg::Matched => Self::Matched,
            CrossfeedLevelArg::PeakSafe => Self::PeakSafe,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum HarmonicsCommand {
    /// Print the current harmonics setting.
    Show,
    /// Turn harmonics off.
    Off,
    /// Warm triode character (dominant 2nd harmonic, subtle 3rd/4th).
    Triode,
    /// Pentode character (dominant 3rd harmonic, subtle 2nd/5th).
    Pentode,
    /// Custom harmonic levels relative to fundamental at 0 dBFS.
    Custom {
        /// 2nd harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h2_db: f64,
        /// 3rd harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h3_db: f64,
        /// Optional 4th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h4_db: Option<f64>,
        /// Optional 5th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h5_db: Option<f64>,
        /// Disable the post-shaper DC-blocking high-pass filter.
        #[arg(long)]
        no_dc_block: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum EqCommand {
    /// Apply Equalizer APO/AutoEQ `.txt` or profile `.json` (whole profile, --section, --left/--right, or --off).
    Set {
        /// Profile file (replaces profile for --section linked, or loads into --section left/right).
        #[arg(value_name = "FILE", conflicts_with = "left")]
        file: Option<PathBuf>,
        /// Target section when loading a file (`linked`, `left`, or `right`).
        #[arg(long, value_enum, conflicts_with_all = ["left", "right", "off"])]
        section: Option<EqSectionArg>,
        /// Left-channel profile file (populates the L section).
        #[arg(long, value_name = "FILE")]
        left: Option<PathBuf>,
        /// Right-channel profile file (populates the R section).
        #[arg(long, value_name = "FILE")]
        right: Option<PathBuf>,
        /// Remove the EQ instead.
        #[arg(long)]
        off: bool,
    },
    /// Add a parametric band to an EQ section.
    Add {
        /// Target section (`linked`, `left`, or `right`).
        #[arg(long, value_enum, default_value_t = EqSectionArg::Linked)]
        section: EqSectionArg,
        /// Filter kind (peaking, low-shelf, high-shelf, low-pass, high-pass, band-pass, notch, all-pass).
        #[arg(long = "kind", alias = "type", value_enum, default_value_t = FilterKindArg::Peaking)]
        kind: FilterKindArg,
        /// Center/corner frequency in Hz.
        #[arg(long)]
        freq: f64,
        /// Gain in dB.
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        gain: f64,
        /// Quality factor Q.
        #[arg(long, default_value_t = std::f64::consts::FRAC_1_SQRT_2)]
        q: f64,
        /// Add the band disabled.
        #[arg(long)]
        disabled: bool,
    },
    /// Edit an existing band (0-based index within the selected section).
    Edit {
        /// 0-based band index within the selected section.
        index: usize,
        /// Target section (`linked`, `left`, or `right`).
        #[arg(long, value_enum, default_value_t = EqSectionArg::Linked)]
        section: EqSectionArg,
        /// Filter kind.
        #[arg(long = "kind", alias = "type", value_enum)]
        kind: Option<FilterKindArg>,
        /// Center/corner frequency in Hz.
        #[arg(long)]
        freq: Option<f64>,
        /// Gain in dB.
        #[arg(long, allow_hyphen_values = true)]
        gain: Option<f64>,
        /// Quality factor Q.
        #[arg(long)]
        q: Option<f64>,
        /// Enable the band.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Disable the band.
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },
    /// Remove a band by 0-based index from a section.
    Remove {
        /// 0-based band index within the selected section.
        index: usize,
        /// Target section (`linked`, `left`, or `right`).
        #[arg(long, value_enum, default_value_t = EqSectionArg::Linked)]
        section: EqSectionArg,
    },
    /// Enable a band by 0-based index in a section.
    Enable {
        /// 0-based band index within the selected section.
        index: usize,
        /// Target section (`linked`, `left`, or `right`).
        #[arg(long, value_enum, default_value_t = EqSectionArg::Linked)]
        section: EqSectionArg,
    },
    /// Disable a band by 0-based index in a section.
    Disable {
        /// 0-based band index within the selected section.
        index: usize,
        /// Target section (`linked`, `left`, or `right`).
        #[arg(long, value_enum, default_value_t = EqSectionArg::Linked)]
        section: EqSectionArg,
    },
    /// Set preamp for global/linked (`linked`) or a per-channel section (`left`/`right`), or `auto`.
    Preamp {
        /// Preamp in dB (or `auto` on `linked` to prevent clipping).
        #[arg(allow_hyphen_values = true)]
        db: String,
        /// Target section (`linked`, `left`, or `right`).
        #[arg(long, value_enum, default_value_t = EqSectionArg::Linked)]
        section: EqSectionArg,
    },
    /// Turn a layered EQ section on/off, or clear/remove it.
    Section {
        #[arg(value_enum)]
        section: EqSectionArg,
        #[arg(value_enum)]
        action: SectionActionArg,
    },
    /// Configure the linked tilt EQ: a constant dB/oct slope from --start to
    /// --end with sharp corners, flat (untouched) outside.
    Tilt {
        /// Slope in dB per octave (e.g. -0.7). Conflicts with --total.
        #[arg(long, allow_hyphen_values = true, conflicts_with = "total")]
        slope: Option<f64>,
        /// Total gain change across [start, end] in dB. Conflicts with --slope.
        #[arg(long, allow_hyphen_values = true, conflicts_with = "slope")]
        total: Option<f64>,
        /// Band start frequency in Hz (default 20).
        #[arg(long)]
        start: Option<f64>,
        /// Band end frequency in Hz (default 20000).
        #[arg(long)]
        end: Option<f64>,
        /// 0 dB pivot: `start` (bass below the start untouched), `end` (treble
        /// above the end untouched), `center` (geometric mean), or a frequency in Hz.
        #[arg(long)]
        pivot: Option<String>,
        /// Enable the tilt filter.
        #[arg(long, conflicts_with_all = ["disable", "off"])]
        enable: bool,
        /// Disable the tilt filter without clearing its parameters.
        #[arg(long, conflicts_with_all = ["enable", "off"])]
        disable: bool,
        /// Clear/remove the tilt filter.
        #[arg(long, conflicts_with_all = ["enable", "disable"])]
        off: bool,
        /// Edit the treble section of a two-section tilt (`tilt_high`, 0 dB
        /// below its start). It pivots at its start, and the bass section
        /// (the plain tilt) at its end, so the response is 0 dB between them.
        #[arg(long)]
        high: bool,
    },
    /// Print the current parametric EQ of chain A.
    Show,
}

#[derive(Subcommand, Debug)]
pub enum ConvCommand {
    /// Replace the convolution chain with one or more WAV impulse responses (or --off).
    Set {
        #[arg(value_name = "IR.wav")]
        files: Vec<PathBuf>,
        /// Remove convolution instead.
        #[arg(long)]
        off: bool,
        /// Extra gain for the IR(s), in dB.
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        gain_db: f64,
        /// Optional display name (for single-IR set).
        #[arg(long)]
        name: Option<String>,
    },
    /// Append a WAV impulse response to the convolution chain.
    Add {
        #[arg(value_name = "IR.wav")]
        file: PathBuf,
        /// Extra gain for the IR, in dB.
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        gain_db: f64,
        /// Optional display name.
        #[arg(long)]
        name: Option<String>,
        /// Add in disabled state.
        #[arg(long)]
        disabled: bool,
    },
    /// Remove an impulse response by index (0-based), name, or path.
    Remove { target: String },
    /// Enable an impulse response by index (0-based), name, or path.
    Enable { target: String },
    /// Disable an impulse response by index (0-based), name, or path.
    Disable { target: String },
    /// List configured impulse responses.
    List,
    /// Remove all impulse responses from the convolution chain.
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PitchQualityArg {
    Low,
    High,
    Max,
}

impl From<PitchQualityArg> for PitchQuality {
    fn from(q: PitchQualityArg) -> Self {
        match q {
            PitchQualityArg::Low => Self::Low,
            PitchQualityArg::High => Self::High,
            PitchQualityArg::Max => Self::Max,
        }
    }
}

#[derive(Args, Debug)]
pub struct PitchArgs {
    /// Pitch shift in cents (-1200..=1200).
    #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["semitones", "reference_hz", "target_hz", "off"])]
    pub cents: Option<f64>,
    /// Pitch shift in semitones (-12..=12).
    #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["cents", "reference_hz", "target_hz", "off"])]
    pub semitones: Option<f64>,
    /// Reference frequency in Hz (e.g. 440); must be paired with --target-hz.
    #[arg(long, requires = "target_hz", conflicts_with = "off")]
    pub reference_hz: Option<f64>,
    /// Target frequency in Hz (e.g. 432); must be paired with --reference-hz.
    #[arg(long, requires = "reference_hz", conflicts_with = "off")]
    pub target_hz: Option<f64>,
    /// Quality preset.
    #[arg(long, value_enum)]
    pub quality: Option<PitchQualityArg>,
    /// Turn pitch shifting off (reset to 0 cents).
    #[arg(long)]
    pub off: bool,
}

#[derive(Args, Debug)]
pub struct VarispeedArgs {
    /// Speed multiplier (0.5..=2.0), `on`, or `off`. Omit to inspect.
    #[arg(value_name = "SPEED|on|off", allow_hyphen_values = true)]
    pub value: Option<String>,
    /// Speed-change ramp duration in milliseconds (0..=10000; 0 uses the envelope minimum).
    #[arg(long, value_name = "MS", allow_hyphen_values = true)]
    pub ramp_ms: Option<f64>,
    /// Keep the pitch: the speed changes the tempo only (a time stretch,
    /// about 135 ms of delay). Kept until `--no-keep-pitch`.
    #[arg(long, conflicts_with = "no_keep_pitch")]
    pub keep_pitch: bool,
    /// Speed and pitch together again (tape-style varispeed).
    #[arg(long)]
    pub no_keep_pitch: bool,
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct FollowerArgs {
    #[command(subcommand)]
    pub command: Option<FollowerCommand>,
    /// A listed follower's device, to edit.
    #[arg(value_name = "DEVICE")]
    pub device: Option<String>,
    #[command(flatten)]
    pub set: FollowerSetArgs,
}

#[derive(Subcommand, Debug)]
pub enum FollowerCommand {
    /// Add a follower on DEVICE (e.g. hw:CARD=USB,DEV=0).
    Add {
        device: String,
        #[command(flatten)]
        set: FollowerSetArgs,
    },
    /// Remove the follower on DEVICE.
    Remove { device: String },
}

#[derive(Args, Debug, Default)]
pub struct FollowerSetArgs {
    /// Its delay behind the main output, ms (-50..=50).
    #[arg(long, value_name = "MS", allow_hyphen_values = true)]
    pub trim_ms: Option<f64>,
    /// Its digital volume, dB (-120..=0).
    #[arg(long, value_name = "DB", allow_hyphen_values = true)]
    pub volume_db: Option<f64>,
    /// Heard (on) or muted in sync (off).
    #[arg(long, value_enum)]
    pub enabled: Option<OnOff>,
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct UpsampleArgs {
    #[command(subcommand)]
    pub command: Option<UpsampleCommand>,
    #[command(flatten)]
    pub set: UpsampleSetArgs,
}

#[derive(Subcommand, Debug)]
pub enum UpsampleCommand {
    /// Measure the response of the engine's own upsampler filter.
    Response(UpsampleResponseArgs),
}

#[derive(Subcommand, Debug)]
pub enum DesignCommand {
    /// Print the design a preset (or `recommended`) expands to, as JSON for
    /// `upsample --design`.
    Resolve {
        #[arg(value_name = "PRESET")]
        preset: String,
    },
    /// Check a design JSON file (`-` reads stdin) without designing its
    /// kernel: its fields, and its plan at each allowed source rate. Exits
    /// 1 when the design is refused.
    Check(DesignCheckArgs),
}

#[derive(Args, Debug)]
pub struct DesignCheckArgs {
    #[arg(value_name = "FILE")]
    pub file: PathBuf,
    /// Check against this integer ratio instead of chain A's target.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=64), conflicts_with_all = ["max_rate", "max_rate_any", "out_rate"])]
    pub ratio: Option<u32>,
    /// Check against this power-of-two maximum output rate.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with_all = ["max_rate_any", "out_rate"])]
    pub max_rate: Option<u32>,
    /// Check against this maximum output rate in either rate family.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with = "out_rate")]
    pub max_rate_any: Option<u32>,
    /// Check against this one output rate.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub out_rate: Option<u32>,
    /// Check at this source rate only (default: every allowed rate).
    #[arg(long)]
    pub rate: Option<u32>,
}

#[derive(Args, Debug)]
pub struct UpsampleResponseArgs {
    /// Source (graph) rate; default is the daemon's current graph rate, else
    /// 44100 Hz.
    #[arg(long, value_parser = clap::value_parser!(u32).range(8_000..=3_072_000))]
    pub rate: Option<u32>,
}

#[derive(Args, Debug)]
pub struct UpsampleSetArgs {
    /// Integer ratio of the source rate (e.g. 4).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=64), conflicts_with_all = ["max_rate", "max_rate_any", "out_rate", "off"])]
    pub ratio: Option<u32>,
    /// Highest output rate to reach with a power-of-two ratio.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with_all = ["max_rate_any", "out_rate", "off"])]
    pub max_rate: Option<u32>,
    /// Highest output rate the DAC plays, in either rate family (44.1k
    /// sources may then resample to a 48k-family rate).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with_all = ["out_rate", "off"])]
    pub max_rate_any: Option<u32>,
    /// One explicit output rate for every source rate (fractional or
    /// decimating where needed, e.g. 48000).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with = "off")]
    pub out_rate: Option<u32>,
    /// Disable upsampling.
    #[arg(long)]
    pub off: bool,
    /// Kernel length at the output rate (1 ..= 8M).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_KERNEL_TAPS as u64))]
    pub taps: Option<u64>,
    /// Filter phase.
    #[arg(long, value_enum)]
    pub phase: Option<PhaseArg>,
    /// Kaiser window beta (0..40).
    #[arg(long)]
    pub beta: Option<f64>,
    /// Cutoff as a fraction of the input Nyquist ((0, 1]).
    #[arg(long)]
    pub cutoff: Option<f64>,
    /// How the cutoff frequency is chosen.
    #[arg(long, value_enum)]
    pub cutoff_mode: Option<CutoffModeArg>,
    /// What --taps counts: output-rate taps, or taps per input sample (the
    /// kernel then lasts the same time at every ratio).
    #[arg(long, value_enum)]
    pub taps_unit: Option<TapsUnitArg>,
    /// Intermediate phase blend: 0 = minimum, 1 = linear (implies
    /// --phase intermediate).
    #[arg(long)]
    pub phase_mix: Option<f64>,
    /// Automatic taps against an explicit amplitude-error budget in dB.
    #[arg(long, conflicts_with_all = ["auto_taps_match_output", "manual_taps"])]
    pub auto_taps_db: Option<f64>,
    /// Automatic taps matching the output word length and dither.
    #[arg(long, conflicts_with = "manual_taps")]
    pub auto_taps_match_output: bool,
    /// Disable automatic taps and return to manual or band mode.
    #[arg(long)]
    pub manual_taps: bool,
    /// Band design: end of the passband, fraction of the source Nyquist
    /// (implies --cutoff-mode band).
    #[arg(long)]
    pub passband_end: Option<f64>,
    /// Band design: start of the stopband, fraction of the source Nyquist
    /// (implies --cutoff-mode band).
    #[arg(long)]
    pub stopband_start: Option<f64>,
    /// Band design: stopband attenuation in dB (implies --cutoff-mode band).
    #[arg(long)]
    pub attenuation: Option<f64>,
    /// Pass the input samples through unchanged (Nyquist filter; linear
    /// phase, cutoff 1.0). `--preserve-samples false` turns it off.
    #[arg(long, num_args = 0..=1, default_missing_value = "true")]
    pub preserve_samples: Option<bool>,
    /// Window of the windowed sinc.
    #[arg(long, value_enum)]
    pub window: Option<WindowArg>,
    /// Tukey window: tapered fraction of each half of the kernel (0..1].
    #[arg(long)]
    pub taper: Option<f64>,
    /// Select a named filter preset (including `recommended`), or `none` to
    /// clear it.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["manual", "design"])]
    pub preset: Option<String>,
    /// Load a custom filter design from a JSON file.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["preset", "manual"])]
    pub design: Option<PathBuf>,
    /// Clear the active filter preset and return to custom settings.
    #[arg(long)]
    pub manual: bool,
    /// List the built-in upsampler filter presets.
    #[arg(long)]
    pub list_presets: bool,
}

/// A stage `ab stage` compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum AbStage {
    Crossfeed,
    Eq,
    Tilt,
    Conv,
    Dc,
    Pitch,
    Harmonics,
}

#[derive(Subcommand, Debug)]
pub enum AbCommand {
    /// Configure chain B from a profile.
    Set {
        #[arg(value_name = "PROFILE")]
        profile: PathBuf,
        /// Level matching: `analytic` (K-weighted), `analytic-a` (A-weighted),
        /// `off`, or a fixed offset in dB. Nothing is boosted: the louder side
        /// is cut.
        #[arg(long, default_value = "analytic", allow_hyphen_values = true)]
        level: String,
    },
    /// Compare chain A with one stage switched off (or on, when it is off
    /// in A). B follows every later edit to A.
    Stage {
        #[arg(value_enum)]
        stage: AbStage,
        /// Level matching, as for `ab set`.
        #[arg(long, default_value = "analytic", allow_hyphen_values = true)]
        level: String,
    },
    /// Compare chain A with another upsampling filter at the same ratio: a
    /// preset name or a design JSON file.
    Filter {
        #[arg(value_name = "PRESET|FILE")]
        filter: String,
        /// Level matching, as for `ab set`.
        #[arg(long, default_value = "analytic", allow_hyphen_values = true)]
        level: String,
    },
    /// Compare chain A with other output bits or dither (B runs its own
    /// quantizer; a switch is a hard cut between the two quantized streams,
    /// at the middle of the crossfade).
    Output {
        /// B's effective bits (16-32), capped to the device format.
        #[arg(long)]
        bits: Option<u32>,
        /// B's dither, as in the config (`tpdf`, `shaped`, `none`,
        /// `resonant5`, ...).
        #[arg(long)]
        dither: Option<String>,
        /// Level matching, as for `ab set`.
        #[arg(long, default_value = "analytic", allow_hyphen_values = true)]
        level: String,
    },
    /// Switch to side A.
    A {
        /// Crossfade length in milliseconds.
        #[arg(long, default_value_t = 10.0)]
        fade: f64,
    },
    /// Switch to side B.
    B {
        /// Crossfade length in milliseconds.
        #[arg(long, default_value_t = 10.0)]
        fade: f64,
    },
    /// Match B's level to A on the music playing ("Match to this track"):
    /// measure both sides, then lock the offset once it is stable.
    Measure {
        /// Lock at this many seconds of measured audio at the latest
        /// (10-300).
        #[arg(long, value_name = "N", conflicts_with_all = ["accept", "cancel"])]
        seconds: Option<f64>,
        /// Lock the running value now.
        #[arg(long, conflicts_with = "cancel")]
        accept: bool,
        /// Stop measuring and keep the current level.
        #[arg(long)]
        cancel: bool,
        /// Follow the measurement until it locks or stops.
        #[arg(long, conflicts_with_all = ["accept", "cancel"])]
        wait: bool,
    },
    /// Turn A/B off (chain A stays active).
    Off,
}

#[derive(Subcommand, Debug)]
pub enum PresetCommand {
    /// List presets (sorted by name).
    List,
    /// Print one preset as JSON.
    Show {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Save chain A as a preset and make it active.
    Save {
        #[arg(value_name = "NAME")]
        name: String,
        /// Replace an existing preset of the same name.
        #[arg(long)]
        overwrite: bool,
    },
    /// Apply a preset to chain A and make it active.
    Load {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Delete a preset.
    Delete {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Rename a preset.
    Rename {
        #[arg(value_name = "FROM")]
        from: String,
        #[arg(value_name = "TO")]
        to: String,
    },
    /// Import an Equalizer APO / AutoEQ file as a preset (not loaded).
    Import {
        #[arg(value_name = "FILE")]
        file: PathBuf,
        /// Preset name (default: the file name without extension).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum StreamCommand {
    /// Show or set one stream's volume in dB (0 = 100 %) and mute. The
    /// level is capped by the guard's stream maximum (`volume
    /// --stream-max`) and becomes the level the guard holds.
    Volume(StreamVolumeArgs),
}

#[derive(Args, Debug)]
pub struct StreamVolumeArgs {
    /// PipeWire node id (see `avdsp ctl streams`).
    pub node: u32,
    /// Level in dB (-120..=0). Omit with no flags to show the current one.
    #[arg(allow_hyphen_values = true)]
    pub db: Option<f64>,
    /// Mute the stream.
    #[arg(long, conflicts_with = "unmute")]
    pub mute: bool,
    /// Unmute the stream.
    #[arg(long, conflicts_with = "mute")]
    pub unmute: bool,
}

impl StreamVolumeArgs {
    fn muted(&self) -> Option<bool> {
        match (self.mute, self.unmute) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }
}

#[derive(Args, Debug)]
pub struct VolumeArgs {
    /// Master attenuation in dB (-120..=0). Omit with no flags to query.
    #[arg(allow_hyphen_values = true)]
    pub db: Option<f64>,
    /// Left/right balance in dB (-24..=24; negative attenuates right, positive attenuates left).
    #[arg(long, value_name = "DB", allow_hyphen_values = true)]
    pub balance: Option<f64>,
    /// Mute playback.
    #[arg(long, conflicts_with = "unmute")]
    pub mute: bool,
    /// Unmute playback.
    #[arg(long, conflicts_with = "mute")]
    pub unmute: bool,
    /// Where the volume is applied: auto, software, hardware or hybrid.
    #[arg(long, value_parser = parse_volume_mode)]
    pub mode: Option<audivane_protocol::ipc::VolumeMode>,
    /// Fixed hardware mixer level in dB for software/hybrid (at most 0), or `none`.
    #[arg(long, value_name = "DB|none", allow_hyphen_values = true, value_parser = parse_hardware_db)]
    pub hardware_db: Option<OptDb>,
    /// Hard ceiling for the volume in dB, or `none`.
    #[arg(long, value_name = "DB|none", allow_hyphen_values = true, value_parser = parse_opt_db)]
    pub ceiling: Option<OptDb>,
    /// What the PipeWire sink volume does: follow (drives the engine volume)
    /// or ignore (PipeWire scales the samples itself).
    #[arg(long, value_parser = parse_sink_volume)]
    pub sink_volume: Option<audivane_protocol::ipc::SinkVolumePolicy>,
    /// Guard against apps or the desktop raising the volume: off, lock or limit.
    #[arg(long, value_parser = parse_guard_mode)]
    pub guard: Option<audivane_protocol::ipc::GuardMode>,
    /// `limit`: fastest rise in dB per second (0 = only the caps).
    #[arg(long, value_name = "DB_PER_S")]
    pub rise: Option<f64>,
    /// `limit`: loudest an app stream may be set to in dB, or `none`.
    #[arg(long, value_name = "DB|none", allow_hyphen_values = true, value_parser = parse_opt_db)]
    pub stream_max: Option<OptDb>,
}

impl VolumeArgs {
    fn edits_settings(&self) -> bool {
        self.mode.is_some()
            || self.hardware_db.is_some()
            || self.ceiling.is_some()
            || self.sink_volume.is_some()
            || self.guard.is_some()
            || self.rise.is_some()
            || self.stream_max.is_some()
    }

    fn edits_level(&self) -> bool {
        self.db.is_some() || self.balance.is_some() || self.mute || self.unmute
    }
}

/// A snake_case protocol enum from its wire name.
macro_rules! wire_parser {
    ($name:ident, $ty:ty) => {
        fn $name(s: &str) -> Result<$ty, String> {
            serde_json::from_value(serde_json::Value::String(s.to_ascii_lowercase()))
                .map_err(|_| format!("unknown value {s:?}"))
        }
    };
}
wire_parser!(parse_volume_mode, audivane_protocol::ipc::VolumeMode);
wire_parser!(parse_sink_volume, audivane_protocol::ipc::SinkVolumePolicy);
wire_parser!(parse_guard_mode, audivane_protocol::ipc::GuardMode);

/// A dB value or `none` (clap cannot hold `Option<Option<_>>` from a parser).
#[derive(Clone, Copy, Debug)]
pub struct OptDb(pub Option<f64>);

/// `none` or a dB value.
fn parse_opt_db(s: &str) -> Result<OptDb, String> {
    if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("off") {
        return Ok(OptDb(None));
    }
    s.parse::<f64>()
        .map(|v| OptDb(Some(v)))
        .map_err(|_| format!("expected a dB value or `none`, got {s:?}"))
}

/// `--hardware-db`: `none` or a level of at most 0 dB (a fixed level above
/// it would play louder than the volume shown).
fn parse_hardware_db(s: &str) -> Result<OptDb, String> {
    let db = parse_opt_db(s)?;
    match db.0 {
        Some(v) if v > audivane_protocol::ipc::VOLUME_MAX_DB => Err(format!(
            "the hardware level must be at most {} dB, got {v}",
            audivane_protocol::ipc::VOLUME_MAX_DB
        )),
        _ => Ok(db),
    }
}

#[derive(Args, Debug)]
pub struct RouteArgs {
    /// PipeWire node id (see `avdsp ctl streams`).
    ///
    /// With `--app`, the sink may be given after the app name, so this
    /// positional is parsed by hand in that case.
    pub node: Option<String>,
    /// Route every stream of this app instead.
    #[arg(long, value_name = "NAME")]
    pub app: Option<String>,
    /// Destination sink.
    #[arg(value_enum)]
    pub sink: Option<SinkArg>,
}

impl RouteArgs {
    /// Resolve `route NODE SINK` and `route --app NAME SINK` into the protocol
    /// target/sink pair.
    fn resolve(self) -> CtlResult<(RouteTarget, SinkChoice)> {
        let sink_from = |text: &str| -> Option<SinkArg> {
            match text {
                "dsp" => Some(SinkArg::Dsp),
                "direct" => Some(SinkArg::Direct),
                "default" => Some(SinkArg::Default),
                _ => None,
            }
        };
        match (self.app, self.sink) {
            (Some(app), Some(sink)) if self.node.is_none() => {
                Ok((RouteTarget::App(app), sink.into()))
            }
            (Some(app), Some(_)) => {
                let _ = app;
                bail!("give either a node id or --app NAME, not both")
            }
            (Some(app), None) => {
                // `route --app NAME dsp`: clap put the sink in `node`.
                let text = self
                    .node
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("give a destination: dsp, direct or default"))?;
                let sink = sink_from(text).ok_or_else(|| {
                    anyhow::anyhow!("unknown destination {text:?}: use dsp, direct or default")
                })?;
                Ok((RouteTarget::App(app), sink.into()))
            }
            (None, Some(sink)) => {
                let text = self.node.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("give a node id (see `avdsp ctl streams`) or --app NAME")
                })?;
                let node: u32 = text
                    .parse()
                    .with_context(|| format!("node id must be a number, got {text:?}"))?;
                Ok((RouteTarget::Node(node), sink.into()))
            }
            (None, None) => {
                bail!("give a node id and a destination: avdsp ctl route NODE dsp|direct|default")
            }
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ModeCommand {
    /// Make Audivane DSP the default sink.
    System,
    /// Leave the default alone and move only matching apps.
    Apps {
        /// Rule in the form `APP=dsp|direct|default` (repeatable).
        #[arg(long = "rule", value_name = "APP=SINK")]
        rules: Vec<String>,
    },
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// Restrict to one event topic (default: meters, status, streams,
    /// now_playing, config and presets).
    #[arg(value_enum)]
    pub topic: Option<WatchTopic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum WatchTopic {
    Meters,
    Status,
    Streams,
    NowPlaying,
    Config,
    Presets,
    Spectrum,
    Devices,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum SinkArg {
    Dsp,
    Direct,
    Default,
}

impl From<SinkArg> for SinkChoice {
    fn from(value: SinkArg) -> Self {
        match value {
            SinkArg::Dsp => SinkChoice::Dsp,
            SinkArg::Direct => SinkChoice::Direct,
            SinkArg::Default => SinkChoice::Default,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PhaseArg {
    Linear,
    Minimum,
    Intermediate,
}

impl From<PhaseArg> for audivane_protocol::FilterPhase {
    fn from(value: PhaseArg) -> Self {
        match value {
            PhaseArg::Linear => Self::Linear,
            PhaseArg::Minimum => Self::Minimum,
            PhaseArg::Intermediate => Self::Intermediate,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CutoffModeArg {
    Fraction,
    NyquistStop,
    Band,
}

impl From<CutoffModeArg> for CutoffMode {
    fn from(value: CutoffModeArg) -> Self {
        match value {
            CutoffModeArg::Fraction => Self::Fraction,
            CutoffModeArg::NyquistStop => Self::NyquistStop,
            CutoffModeArg::Band => Self::Band,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum WindowArg {
    Kaiser,
    Gaussian,
    Tukey,
}

impl From<WindowArg> for WindowKind {
    fn from(value: WindowArg) -> Self {
        match value {
            WindowArg::Kaiser => Self::Kaiser,
            WindowArg::Gaussian => Self::Gaussian,
            WindowArg::Tukey => Self::Tukey,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TapsUnitArg {
    Output,
    Input,
}

impl From<TapsUnitArg> for TapsUnit {
    fn from(value: TapsUnitArg) -> Self {
        match value {
            TapsUnitArg::Output => Self::Output,
            TapsUnitArg::Input => Self::Input,
        }
    }
}

// ---------------------------------------------------------------- errors

enum CtlError {
    Rpc(audivane_protocol::ipc::RpcError),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CtlError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value)
    }
}

impl From<serde_json::Error> for CtlError {
    fn from(value: serde_json::Error) -> Self {
        Self::Other(value.into())
    }
}

impl From<std::io::Error> for CtlError {
    fn from(value: std::io::Error) -> Self {
        Self::Other(value.into())
    }
}

type CtlResult<T> = std::result::Result<T, CtlError>;

// ---------------------------------------------------------------- client

struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    fn connect(explicit: Option<&Path>) -> Result<Self> {
        let path = daemon_socket_path(explicit)?;
        let stream = match UnixStream::connect(&path) {
            Ok(stream) => stream,
            Err(err) => anyhow::bail!(
                "audivane-dsp is not running (cannot connect to {}: {err})",
                path.display()
            ),
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let reader = BufReader::new(stream.try_clone().context("clone socket")?);
        Ok(Self {
            stream,
            reader,
            next_id: 1,
        })
    }

    fn send(&mut self, call: Call) -> std::io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let request = audivane_protocol::ipc::Request { id, call };
        let mut line = serde_json::to_vec(&request)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        line.push(b'\n');
        self.stream.write_all(&line)?;
        self.stream.flush()?;
        Ok(id)
    }

    /// Send a call and wait for its reply, skipping event lines.
    fn call(&mut self, call: Call) -> CtlResult<Value> {
        let id = self.send(call)?;
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .reader
                .read_line(&mut line)
                .map_err(|err| anyhow::anyhow!("reading from the daemon failed: {err}"))?;
            if read == 0 {
                return Err(CtlError::Other(anyhow::anyhow!(
                    "the daemon closed the connection"
                )));
            }
            let value: Value = match serde_json::from_str(line.trim()) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value.get("event").is_some() || value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                let error: audivane_protocol::ipc::RpcError = serde_json::from_value(error.clone())
                    .map_err(|err| anyhow::anyhow!("bad error reply: {err}"))?;
                return Err(CtlError::Rpc(error));
            }
            return Ok(value.get("ok").cloned().unwrap_or(Value::Null));
        }
    }
}

fn daemon_socket_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("AUDIVANE_DSP_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(runtime).join("audivane-dsp.sock"));
    }
    // SAFETY: getuid has no preconditions.
    let uid = libc_getuid();
    Ok(PathBuf::from(format!("/tmp/audivane-dsp-{uid}.sock")))
}

// Kept in a tiny wrapper so the crate does not need a `libc` dependency just
// for one call.
fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: getuid has no preconditions.
    unsafe { getuid() }
}

/// The running engine's version from `status`, or None when nothing answers
/// within `timeout` (used by `avdsp upgrade`).
pub(crate) fn engine_version(timeout: Duration) -> Option<String> {
    engine_version_at(&daemon_socket_path(None).ok()?, timeout)
}

fn engine_version_at(path: &Path, timeout: Duration) -> Option<String> {
    let stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let reader = BufReader::new(stream.try_clone().ok()?);
    let mut client = Client {
        stream,
        reader,
        next_id: 1,
    };
    let status = client.call(Call::Status).ok()?;
    status.get("version")?.as_str().map(str::to_owned)
}

// ---------------------------------------------------------------- entry

pub fn run(args: CtlArgs) -> Result<i32> {
    let mut client = Client::connect(args.socket.as_deref())?;
    let json = args.json;
    match dispatch(&mut client, args.command, json) {
        Ok(code) => Ok(code),
        Err(CtlError::Rpc(error)) => {
            eprintln!("avdsp: {} (code {})", error.message, error.code);
            Ok(1)
        }
        Err(CtlError::Other(error)) => Err(error),
    }
}

fn dispatch(client: &mut Client, command: CtlCommand, json: bool) -> CtlResult<i32> {
    match command {
        CtlCommand::Status => {
            let value = client.call(Call::Status)?;
            if json {
                print_json(&value)?;
            } else {
                let status: audivane_protocol::ipc::Status = serde_json::from_value(value)?;
                let config: Option<EngineConfig> = client
                    .call(Call::GetConfig)
                    .ok()
                    .and_then(|v| serde_json::from_value(v).ok());
                print_status(&status, config.as_ref())?;
                if let Some(meters) = client
                    .call(Call::Meters)
                    .ok()
                    .and_then(|v| serde_json::from_value::<audivane_protocol::ipc::Meters>(v).ok())
                {
                    println!(
                        "clips:      {}  (peak [{:.1}] dBFS in, [{:.1}] dBFS out)",
                        clip_line(&meters),
                        meters.input_peak_db.first().copied().unwrap_or(-160.0),
                        meters.peak_db.first().copied().unwrap_or(-160.0)
                    );
                    if let Some(limiter) = meters.limiter {
                        println!(
                            "limiter:    {}  headroom {:.1} dB, max reduction {:.2} dB, {} events",
                            if limiter.active { "active" } else { "off" },
                            limiter.headroom_db,
                            limiter.gain_reduction_db,
                            limiter.events
                        );
                    }
                }
            }
        }
        CtlCommand::Limiter {
            mode,
            ceiling,
            lookahead,
            release,
        } => {
            let config: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
            let mut limiter = config.chain.limiter;
            let changed =
                mode.is_some() || ceiling.is_some() || lookahead.is_some() || release.is_some();
            if let Some(mode) = mode {
                limiter.mode = match mode {
                    LimiterModeArg::Off => audivane_protocol::LimiterMode::Off,
                    LimiterModeArg::Limit => audivane_protocol::LimiterMode::Limit,
                    LimiterModeArg::AutoHeadroom => audivane_protocol::LimiterMode::AutoHeadroom,
                };
            }
            if let Some(v) = ceiling {
                limiter.ceiling_dbfs = v;
            }
            if let Some(v) = lookahead {
                limiter.lookahead_ms = v;
            }
            if let Some(v) = release {
                limiter.release_ms = v;
            }
            if changed {
                client.call(Call::SetLimiter(limiter.clone()))?;
            }
            if json {
                print_json(&serde_json::to_value(&limiter)?)?;
            } else {
                println!(
                    "limiter: {:?}, ceiling {:.2} dBFS, look-ahead {:.2} ms, release {:.1} ms",
                    limiter.mode, limiter.ceiling_dbfs, limiter.lookahead_ms, limiter.release_ms
                );
            }
        }
        CtlCommand::Output { headroom } => {
            let config: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
            let mut output = config.chain.output;
            if let Some(db) = headroom {
                output.headroom_db = db;
                client.call(Call::SetOutput(output))?;
            }
            if json {
                print_json(&serde_json::to_value(output)?)?;
            } else {
                println!(
                    "output: {} bits, {:?} dither, headroom {} dB",
                    output.bits, output.dither, output.headroom_db
                );
            }
        }
        CtlCommand::Config { command } => match command {
            ConfigCommand::Get => {
                let value = client.call(Call::GetConfig)?;
                print_json(&value)?;
            }
            ConfigCommand::Set { file } => {
                let text = if file.as_os_str() == "-" {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    text
                } else {
                    std::fs::read_to_string(&file)
                        .with_context(|| format!("cannot read {}", file.display()))?
                };
                let config: EngineConfig = serde_json::from_str(&text)
                    .with_context(|| format!("{} is not a valid config JSON", file.display()))?;
                client.call(Call::SetConfig(config))?;
                println!("configuration applied");
            }
        },
        CtlCommand::Harmonics { command } => match command {
            HarmonicsCommand::Show => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                if json {
                    print_json(&serde_json::to_value(&current.chain.harmonics)?)?;
                } else {
                    println!(
                        "harmonics: {}",
                        summarize_harmonics(current.chain.harmonics.as_ref())
                    );
                }
            }
            HarmonicsCommand::Off => {
                client.call(Call::SetHarmonics(None))?;
                println!("harmonics off");
            }
            HarmonicsCommand::Triode => {
                let cfg = HarmonicsConfig {
                    enabled: true,
                    character: HarmonicsCharacter::Triode,
                    ..Default::default()
                };
                client.call(Call::SetHarmonics(Some(cfg.clone())))?;
                println!("harmonics set: {}", summarize_harmonics(Some(&cfg)));
            }
            HarmonicsCommand::Pentode => {
                let cfg = HarmonicsConfig {
                    enabled: true,
                    character: HarmonicsCharacter::Pentode,
                    ..Default::default()
                };
                client.call(Call::SetHarmonics(Some(cfg.clone())))?;
                println!("harmonics set: {}", summarize_harmonics(Some(&cfg)));
            }
            HarmonicsCommand::Custom {
                h2_db,
                h3_db,
                h4_db,
                h5_db,
                no_dc_block,
            } => {
                let cfg = HarmonicsConfig {
                    enabled: true,
                    character: HarmonicsCharacter::Custom,
                    h2_db,
                    h3_db,
                    h4_db,
                    h5_db,
                    dc_block: !no_dc_block,
                };
                client.call(Call::SetHarmonics(Some(cfg.clone())))?;
                println!("harmonics set: {}", summarize_harmonics(Some(&cfg)));
            }
        },
        CtlCommand::DcBlock { state, cutoff } => {
            let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
            match (state, cutoff) {
                (None, None) => {
                    if json {
                        print_json(&serde_json::to_value(current.chain.dc_block)?)?;
                    } else {
                        println!(
                            "dc-block: {}",
                            summarize_dc_block(current.chain.dc_block.as_ref())
                        );
                    }
                }
                (state, cutoff) => {
                    let prev = current.chain.dc_block;
                    let enabled = match state {
                        Some(OnOff::On) => true,
                        Some(OnOff::Off) => false,
                        None => prev.is_some_and(|c| c.enabled),
                    };
                    let cutoff_hz = cutoff.unwrap_or_else(|| prev.map_or(5.0, |c| c.cutoff_hz));
                    let cfg = DcBlockConfig { enabled, cutoff_hz };
                    client.call(Call::SetDcBlock(Some(cfg)))?;
                    if json {
                        print_json(&serde_json::to_value(cfg)?)?;
                    } else {
                        println!("dc-block set: {}", summarize_dc_block(Some(&cfg)));
                    }
                }
            }
        }
        CtlCommand::Crossfeed { command } => match command {
            CrossfeedCommand::Show => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                if json {
                    print_json(&serde_json::to_value(&current.chain.crossfeed)?)?;
                } else {
                    let rate = XfRate::query(client, current.rate.default_rate);
                    println!(
                        "crossfeed: {}",
                        summarize_crossfeed(current.chain.crossfeed.as_ref(), rate)
                    );
                }
            }
            CrossfeedCommand::Off => {
                client.call(Call::SetCrossfeed(None))?;
                println!("crossfeed off");
            }
            CrossfeedCommand::Level { level } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let Some(mut cfg) = current.chain.crossfeed.filter(|c| c.enabled) else {
                    bail!("crossfeed is off; pick a preset first");
                };
                cfg.level = level.into();
                client.call(Call::SetCrossfeed(Some(cfg.clone())))?;
                let rate = XfRate::query(client, current.rate.default_rate);
                println!("crossfeed set: {}", summarize_crossfeed(Some(&cfg), rate));
            }
            preset => {
                // Presets keep the level policy already chosen.
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let level = current.chain.crossfeed.map(|c| c.level).unwrap_or_default();
                let (mode, cutoff_hz, feed_db) = match preset {
                    CrossfeedCommand::Default => (CrossfeedMode::Bs2bDefault, None, None),
                    CrossfeedCommand::ChuMoy => (CrossfeedMode::Bs2bChuMoy, None, None),
                    CrossfeedCommand::JanMeier => (CrossfeedMode::Bs2bJanMeier, None, None),
                    CrossfeedCommand::Linkwitz => (CrossfeedMode::Linkwitz, None, None),
                    CrossfeedCommand::Custom { cutoff_hz, feed_db } => {
                        (CrossfeedMode::Bs2bCustom, Some(cutoff_hz), Some(feed_db))
                    }
                    CrossfeedCommand::Show
                    | CrossfeedCommand::Off
                    | CrossfeedCommand::Level { .. } => unreachable!(),
                };
                let defaults = CrossfeedConfig::default();
                let cfg = CrossfeedConfig {
                    enabled: true,
                    mode,
                    cutoff_hz: cutoff_hz.unwrap_or(defaults.cutoff_hz),
                    feed_db: feed_db.unwrap_or(defaults.feed_db),
                    level,
                };
                client.call(Call::SetCrossfeed(Some(cfg.clone())))?;
                let rate = XfRate::query(client, current.rate.default_rate);
                println!("crossfeed set: {}", summarize_crossfeed(Some(&cfg), rate));
            }
        },
        CtlCommand::Eq { command } => match command {
            EqCommand::Set {
                file,
                section,
                left,
                right,
                off,
            } => {
                if off {
                    if file.is_some() || left.is_some() || right.is_some() {
                        bail!("give profile files or --off, not both");
                    }
                    client.call(Call::SetEq(None))?;
                    println!("EQ cleared");
                } else if let Some(sec) = section {
                    let Some(path) = file else {
                        bail!("give a profile file with --section");
                    };
                    let loaded = crate::profile::load(&path)?;
                    match sec {
                        EqSectionArg::Linked => {
                            let current: EngineConfig =
                                serde_json::from_value(client.call(Call::GetConfig)?)?;
                            let mut profile = loaded;
                            if profile.left.is_none()
                                && profile.right.is_none()
                                && let Some(prev) = current.chain.eq
                            {
                                profile.left = prev.left;
                                profile.right = prev.right;
                            }
                            client.call(Call::SetEq(Some(profile)))?;
                        }
                        EqSectionArg::Left => {
                            let current: EngineConfig =
                                serde_json::from_value(client.call(Call::GetConfig)?)?;
                            let mut profile = current.chain.eq.unwrap_or_default();
                            profile.left = Some(ChannelEq {
                                enabled: true,
                                preamp_db: loaded.preamp_db,
                                bands: loaded.bands,
                            });
                            client.call(Call::SetEq(Some(profile)))?;
                        }
                        EqSectionArg::Right => {
                            let current: EngineConfig =
                                serde_json::from_value(client.call(Call::GetConfig)?)?;
                            let mut profile = current.chain.eq.unwrap_or_default();
                            profile.right = Some(ChannelEq {
                                enabled: true,
                                preamp_db: loaded.preamp_db,
                                bands: loaded.bands,
                            });
                            client.call(Call::SetEq(Some(profile)))?;
                        }
                    }
                    println!("EQ {} section set from {}", sec.label(), path.display());
                } else {
                    match (file, left, right) {
                        (None, None, None) => {
                            bail!("give a profile file, --left/--right, or --off");
                        }
                        (Some(path), None, None) => {
                            let profile = crate::profile::load(&path)?;
                            client.call(Call::SetEq(Some(profile)))?;
                            println!("EQ set from {}", path.display());
                        }
                        (None, Some(lpath), Some(rpath)) => {
                            let lprof = crate::profile::load(&lpath)?;
                            let rprof = crate::profile::load(&rpath)?;
                            let profile = EqProfile {
                                name: if !lprof.name.is_empty() {
                                    lprof.name
                                } else {
                                    rprof.name
                                },
                                left: Some(ChannelEq {
                                    enabled: true,
                                    preamp_db: lprof.preamp_db,
                                    bands: lprof.bands,
                                }),
                                right: Some(ChannelEq {
                                    enabled: true,
                                    preamp_db: rprof.preamp_db,
                                    bands: rprof.bands,
                                }),
                                ..Default::default()
                            };
                            client.call(Call::SetEq(Some(profile)))?;
                            println!(
                                "EQ set (L/R) from {} / {}",
                                lpath.display(),
                                rpath.display()
                            );
                        }
                        (None, Some(lpath), None) => {
                            let lprof = crate::profile::load(&lpath)?;
                            let current: EngineConfig =
                                serde_json::from_value(client.call(Call::GetConfig)?)?;
                            let mut profile = current.chain.eq.unwrap_or_default();
                            profile.left = Some(ChannelEq {
                                enabled: true,
                                preamp_db: lprof.preamp_db,
                                bands: lprof.bands,
                            });
                            client.call(Call::SetEq(Some(profile)))?;
                            println!("EQ L section set from {}", lpath.display());
                        }
                        (Some(lpath), None, Some(rpath)) => {
                            let mut profile = crate::profile::load(&lpath)?;
                            let rprof = crate::profile::load(&rpath)?;
                            profile.right = Some(ChannelEq {
                                enabled: true,
                                preamp_db: rprof.preamp_db,
                                bands: rprof.bands,
                            });
                            client.call(Call::SetEq(Some(profile)))?;
                            println!(
                                "EQ set (L+R/R) from {} / {}",
                                lpath.display(),
                                rpath.display()
                            );
                        }
                        (None, None, Some(rpath)) => {
                            let rprof = crate::profile::load(&rpath)?;
                            let current: EngineConfig =
                                serde_json::from_value(client.call(Call::GetConfig)?)?;
                            let mut profile = current.chain.eq.unwrap_or_default();
                            profile.right = Some(ChannelEq {
                                enabled: true,
                                preamp_db: rprof.preamp_db,
                                bands: rprof.bands,
                            });
                            client.call(Call::SetEq(Some(profile)))?;
                            println!("EQ R section set from {}", rpath.display());
                        }
                        (Some(_), Some(_), _) => unreachable!("clap conflicts_with"),
                    }
                }
            }
            EqCommand::Add {
                section,
                kind,
                freq,
                gain,
                q,
                disabled,
            } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current.chain.eq.unwrap_or_default();
                let bands = eq_section_bands_mut(&mut profile, section, true)?;
                bands.push(Band {
                    kind: kind.into(),
                    freq_hz: freq,
                    gain_db: gain,
                    q,
                    enabled: !disabled,
                });
                let idx = bands.len() - 1;
                client.call(Call::SetEq(Some(profile)))?;
                println!(
                    "added {} band [{idx}] {:.1} Hz {:+.2} dB Q {:.3}",
                    section.label(),
                    freq,
                    gain,
                    q
                );
            }
            EqCommand::Edit {
                index,
                section,
                kind,
                freq,
                gain,
                q,
                enable,
                disable,
            } => {
                if kind.is_none()
                    && freq.is_none()
                    && gain.is_none()
                    && q.is_none()
                    && !enable
                    && !disable
                {
                    bail!(
                        "nothing to change: give --kind, --freq, --gain, --q, --enable, or --disable"
                    );
                }
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current
                    .chain
                    .eq
                    .ok_or_else(|| anyhow::anyhow!("EQ is off"))?;
                let bands = eq_section_bands_mut(&mut profile, section, false)?;
                let len = bands.len();
                let band = bands.get_mut(index).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{} band index {index} out of range (have {len} bands)",
                        section.label(),
                    )
                })?;
                if let Some(k) = kind {
                    band.kind = k.into();
                }
                if let Some(f) = freq {
                    band.freq_hz = f;
                }
                if let Some(g) = gain {
                    band.gain_db = g;
                }
                if let Some(qv) = q {
                    band.q = qv;
                }
                if enable {
                    band.enabled = true;
                } else if disable {
                    band.enabled = false;
                }
                let updated = band.clone();
                client.call(Call::SetEq(Some(profile)))?;
                println!(
                    "updated {} band [{index}] ({} {:?} {:.1} Hz {:+.2} dB Q {:.3})",
                    section.label(),
                    if updated.enabled { "on" } else { "off" },
                    updated.kind,
                    updated.freq_hz,
                    updated.gain_db,
                    updated.q
                );
            }
            EqCommand::Remove { index, section } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current
                    .chain
                    .eq
                    .ok_or_else(|| anyhow::anyhow!("EQ is off"))?;
                let bands = eq_section_bands_mut(&mut profile, section, false)?;
                if index >= bands.len() {
                    bail!(
                        "{} band index {index} out of range (have {} bands)",
                        section.label(),
                        bands.len()
                    );
                }
                bands.remove(index);
                client.call(Call::SetEq(Some(profile)))?;
                println!("removed {} band [{index}]", section.label());
            }
            EqCommand::Enable { index, section } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current
                    .chain
                    .eq
                    .ok_or_else(|| anyhow::anyhow!("EQ is off"))?;
                let bands = eq_section_bands_mut(&mut profile, section, false)?;
                let len = bands.len();
                let band = bands.get_mut(index).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{} band index {index} out of range (have {len} bands)",
                        section.label(),
                    )
                })?;
                band.enabled = true;
                client.call(Call::SetEq(Some(profile)))?;
                println!("enabled {} band [{index}]", section.label());
            }
            EqCommand::Disable { index, section } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current
                    .chain
                    .eq
                    .ok_or_else(|| anyhow::anyhow!("EQ is off"))?;
                let bands = eq_section_bands_mut(&mut profile, section, false)?;
                let len = bands.len();
                let band = bands.get_mut(index).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{} band index {index} out of range (have {len} bands)",
                        section.label(),
                    )
                })?;
                band.enabled = false;
                client.call(Call::SetEq(Some(profile)))?;
                println!("disabled {} band [{index}]", section.label());
            }
            EqCommand::Preamp { db, section } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current.chain.eq.unwrap_or_default();
                let value = if db.trim().eq_ignore_ascii_case("auto") {
                    if section != EqSectionArg::Linked {
                        bail!("auto preamp applies to the global/linked preamp (omit --section)");
                    }
                    let rate = serde_json::from_value::<audivane_protocol::ipc::Status>(
                        client.call(Call::Status)?,
                    )
                    .ok()
                    .map(|s| s.graph_rate)
                    .filter(|&r| r > 0)
                    .unwrap_or(current.rate.default_rate);
                    // Crossfeed applies its own headroom inside the stage, so
                    // the EQ preamp covers the EQ alone.
                    audivane_dsp_core::biquad::Eq::auto_preamp_db(&profile, f64::from(rate))
                        .map_err(anyhow::Error::msg)?
                } else {
                    db.trim()
                        .parse::<f64>()
                        .with_context(|| format!("invalid preamp {db:?}: expected dB or `auto`"))?
                };
                match section {
                    EqSectionArg::Linked => profile.preamp_db = value,
                    EqSectionArg::Left => {
                        profile
                            .left
                            .get_or_insert_with(ChannelEq::default)
                            .preamp_db = value;
                    }
                    EqSectionArg::Right => {
                        profile
                            .right
                            .get_or_insert_with(ChannelEq::default)
                            .preamp_db = value;
                    }
                }
                client.call(Call::SetEq(Some(profile)))?;
                println!("{} preamp set to {:.2} dB", section.label(), value);
            }
            EqCommand::Section { section, action } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current.chain.eq.unwrap_or_default();
                match (section, action) {
                    (EqSectionArg::Linked, SectionActionArg::On) => {
                        profile.linked_enabled = true;
                        client.call(Call::SetEq(Some(profile)))?;
                        println!("EQ L+R section on");
                    }
                    (EqSectionArg::Linked, SectionActionArg::Off) => {
                        profile.linked_enabled = false;
                        client.call(Call::SetEq(Some(profile)))?;
                        println!("EQ L+R section off");
                    }
                    (EqSectionArg::Linked, SectionActionArg::Clear) => {
                        profile.bands.clear();
                        profile.preamp_db = 0.0;
                        client.call(Call::SetEq(Some(profile)))?;
                        println!("EQ L+R section cleared");
                    }
                    (EqSectionArg::Left, SectionActionArg::On) => {
                        profile.left.get_or_insert_with(ChannelEq::default).enabled = true;
                        client.call(Call::SetEq(Some(profile)))?;
                        println!("EQ L section on");
                    }
                    (EqSectionArg::Left, SectionActionArg::Off) => {
                        if let Some(left) = &mut profile.left {
                            left.enabled = false;
                            client.call(Call::SetEq(Some(profile)))?;
                        }
                        println!("EQ L section off");
                    }
                    (EqSectionArg::Left, SectionActionArg::Clear) => {
                        profile.left = None;
                        client.call(Call::SetEq(Some(profile)))?;
                        println!("EQ L section cleared");
                    }
                    (EqSectionArg::Right, SectionActionArg::On) => {
                        profile.right.get_or_insert_with(ChannelEq::default).enabled = true;
                        client.call(Call::SetEq(Some(profile)))?;
                        println!("EQ R section on");
                    }
                    (EqSectionArg::Right, SectionActionArg::Off) => {
                        if let Some(right) = &mut profile.right {
                            right.enabled = false;
                            client.call(Call::SetEq(Some(profile)))?;
                        }
                        println!("EQ R section off");
                    }
                    (EqSectionArg::Right, SectionActionArg::Clear) => {
                        profile.right = None;
                        client.call(Call::SetEq(Some(profile)))?;
                        println!("EQ R section cleared");
                    }
                }
            }
            EqCommand::Tilt {
                slope,
                total,
                start,
                end,
                pivot,
                enable,
                disable,
                off,
                high,
            } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current.chain.eq.unwrap_or_default();
                let (label, existing) = if high {
                    ("treble tilt", profile.tilt_high)
                } else {
                    ("tilt", profile.tilt)
                };
                if high && profile.tilt.is_none() {
                    bail!(
                        "no bass tilt: a treble tilt alone is a single tilt (`eq tilt --start <hz> --pivot start`)"
                    );
                }
                if off {
                    if high {
                        profile.tilt_high = None;
                    } else {
                        // The treble section, if any, becomes the single tilt.
                        profile.tilt = profile.tilt_high.take();
                    }
                    client.call(Call::SetEq(Some(profile)))?;
                    println!("EQ {label} cleared");
                } else {
                    let mut tilt = existing.unwrap_or(TiltConfig {
                        enabled: true,
                        pivot: if high {
                            TiltPivot::Start
                        } else {
                            TiltPivot::Center
                        },
                        ..Default::default()
                    });
                    if let Some(s) = slope {
                        tilt.slope_db_per_oct = Some(s);
                        tilt.total_db = None;
                    }
                    if let Some(t) = total {
                        tilt.total_db = Some(t);
                        tilt.slope_db_per_oct = None;
                    }
                    if let Some(s_hz) = start {
                        tilt.start_hz = s_hz;
                    }
                    if let Some(e_hz) = end {
                        tilt.end_hz = e_hz;
                    }
                    if let Some(p) = pivot {
                        tilt.pivot = parse_tilt_pivot(&p)?;
                    }
                    if enable {
                        tilt.enabled = true;
                    } else if disable {
                        tilt.enabled = false;
                    } else if slope.is_some()
                        || total.is_some()
                        || start.is_some()
                        || end.is_some()
                        || existing.is_none()
                    {
                        tilt.enabled = true;
                    }
                    if high {
                        // Two sections: the bass one ends in 0 dB.
                        if let Some(low) = profile.tilt.as_mut()
                            && low.pivot != TiltPivot::End
                        {
                            low.pivot = TiltPivot::End;
                            println!("note: bass tilt pivot set to end");
                        }
                        profile.tilt_high = Some(tilt);
                    } else {
                        profile.tilt = Some(tilt);
                    }
                    client.call(Call::SetEq(Some(profile)))?;
                    println!("EQ {label}: {}", format_tilt(&tilt));
                    if tilt.enabled
                        && let Ok(val) = client.call(Call::Status)
                        && let Ok(status) = serde_json::from_value::<Status>(val)
                    {
                        let rate = status.graph_rate;
                        let max_end = 0.45 * f64::from(rate);
                        if rate > 0 && tilt.end_hz > max_end {
                            println!(
                                "note: end_hz ({:.1} Hz) is clamped to {max_end:.1} Hz at {rate} Hz",
                                tilt.end_hz
                            );
                        }
                    }
                }
            }
            EqCommand::Show => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                if json {
                    print_json(&serde_json::to_value(&current.chain.eq)?)?;
                } else {
                    print_eq(current.chain.eq.as_ref());
                }
            }
        },
        CtlCommand::Conv { command } => match command {
            ConvCommand::Set {
                files,
                off,
                gain_db,
                name,
            } => {
                if off {
                    if !files.is_empty() {
                        bail!("give impulse response files or --off, not both");
                    }
                    client.call(Call::SetConvolution(Vec::new()))?;
                    println!("convolution cleared");
                } else {
                    if files.is_empty() {
                        bail!("give at least one impulse response WAV or --off");
                    }
                    if files.len() > 1 && name.is_some() {
                        bail!(
                            "--name can only be used with a single file; use `conv add` for named entries"
                        );
                    }
                    for f in &files {
                        if !f.is_file() {
                            bail!("{} does not exist", f.display());
                        }
                    }
                    let convs: Vec<ConvolutionConfig> = files
                        .iter()
                        .map(|f| ConvolutionConfig {
                            name: name.clone(),
                            enabled: true,
                            ir_path: f.clone(),
                            gain_db,
                        })
                        .collect();
                    client.call(Call::SetConvolution(convs))?;
                    if files.len() == 1 {
                        println!("convolution set from {}", files[0].display());
                    } else {
                        println!("convolution set ({} IRs)", files.len());
                    }
                }
            }
            ConvCommand::Add {
                file,
                gain_db,
                name,
                disabled,
            } => {
                if !file.is_file() {
                    bail!("{} does not exist", file.display());
                }
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                current.chain.convolution.push(ConvolutionConfig {
                    name,
                    enabled: !disabled,
                    ir_path: file.clone(),
                    gain_db,
                });
                let idx = current.chain.convolution.len() - 1;
                client.call(Call::SetConvolution(current.chain.convolution))?;
                println!("added convolution IR [{idx}] {}", file.display());
            }
            ConvCommand::Remove { target } => {
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                let idx = find_conv_index(&current.chain.convolution, &target)?;
                let removed = current.chain.convolution.remove(idx);
                client.call(Call::SetConvolution(current.chain.convolution))?;
                println!(
                    "removed convolution IR [{idx}] {}",
                    removed.ir_path.display()
                );
            }
            ConvCommand::Enable { target } => {
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                let idx = find_conv_index(&current.chain.convolution, &target)?;
                current.chain.convolution[idx].enabled = true;
                let path = current.chain.convolution[idx].ir_path.clone();
                client.call(Call::SetConvolution(current.chain.convolution))?;
                println!("enabled convolution IR [{idx}] {}", path.display());
            }
            ConvCommand::Disable { target } => {
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                let idx = find_conv_index(&current.chain.convolution, &target)?;
                current.chain.convolution[idx].enabled = false;
                let path = current.chain.convolution[idx].ir_path.clone();
                client.call(Call::SetConvolution(current.chain.convolution))?;
                println!("disabled convolution IR [{idx}] {}", path.display());
            }
            ConvCommand::List => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                if json {
                    print_json(&serde_json::to_value(&current.chain.convolution)?)?;
                } else {
                    print_conv_list(&current.chain.convolution);
                }
            }
            ConvCommand::Clear => {
                client.call(Call::SetConvolution(Vec::new()))?;
                println!("convolution cleared");
            }
        },
        CtlCommand::Upsample(args) => match args.command {
            Some(UpsampleCommand::Response(response_args)) => {
                let value = client.call(Call::UpsamplerResponse {
                    config: None,
                    source_rate: response_args.rate,
                    window: None,
                })?;
                let response: audivane_protocol::ipc::UpsamplerResponse =
                    serde_json::from_value(value)?;
                if json {
                    println!("{}", serde_json::to_string(&response)?);
                } else {
                    print_upsampler_response(&response);
                }
            }
            None => {
                let set = args.set;
                if set.list_presets {
                    let mut names: Vec<&'static str> = vec![audivane_protocol::RECOMMENDED_PRESET];
                    names.extend(FilterPreset::ALL.iter().map(|p| p.name()));
                    if json {
                        println!("{}", serde_json::to_string(&names)?);
                    } else {
                        for name in names {
                            println!("{name}");
                        }
                    }
                    return Ok(0);
                }
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut upsampler = current.chain.upsampler.clone();
                let mut changed = false;
                if let Some(name) = set.preset {
                    if name == "none" {
                        upsampler.preset = None;
                    } else if name == audivane_protocol::RECOMMENDED_PRESET
                        || FilterPreset::from_name(&name).is_some()
                    {
                        upsampler.preset = Some(name);
                        upsampler.design = None;
                    } else {
                        bail!(
                            "unknown filter preset `{name}` (run `avdsp ctl upsample \
                             --list-presets`)"
                        );
                    }
                    changed = true;
                }
                if let Some(path) = set.design {
                    let text = std::fs::read_to_string(&path)
                        .with_context(|| format!("read design file {}", path.display()))?;
                    let design: audivane_protocol::StoredDesign = serde_json::from_str(&text)
                        .with_context(|| format!("parse design JSON {}", path.display()))?;
                    upsampler.design = Some(design);
                    upsampler.preset = None;
                    changed = true;
                }
                if set.manual {
                    upsampler.preset = None;
                    changed = true;
                }
                if let Some(ratio) = set.ratio {
                    upsampler.target = RateTarget::Ratio(ratio);
                    changed = true;
                }
                if let Some(max_rate) = set.max_rate {
                    upsampler.target = RateTarget::MaxRate(max_rate);
                    changed = true;
                }
                if let Some(max_rate) = set.max_rate_any {
                    upsampler.target = RateTarget::MaxRateAny(max_rate);
                    changed = true;
                }
                if let Some(rate) = set.out_rate {
                    upsampler.target = RateTarget::Rate(rate);
                    changed = true;
                }
                if set.off {
                    upsampler.target = RateTarget::Off;
                    changed = true;
                }
                let band_flag = set.passband_end.is_some()
                    || set.stopband_start.is_some()
                    || set.attenuation.is_some();
                let manual_flag = set.taps.is_some()
                    || set.phase.is_some()
                    || set.beta.is_some()
                    || set.cutoff.is_some()
                    || set.cutoff_mode.is_some()
                    || set.taps_unit.is_some()
                    || set.phase_mix.is_some()
                    || set.auto_taps_db.is_some()
                    || set.auto_taps_match_output
                    || set.manual_taps
                    || band_flag
                    || set.window.is_some()
                    || set.taper.is_some()
                    || set.preserve_samples.is_some();
                if manual_flag {
                    let mut m = upsampler
                        .design
                        .as_ref()
                        .and_then(|d| match &d.design {
                            audivane_protocol::FilterDesign::Kernel(k)
                                if k.numerics.path
                                    == audivane_protocol::design::ArithmeticPath::Standard =>
                            {
                                audivane_dsp_core::design::paths::standard::legacy_config(k, 44_100)
                                    .ok()
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    if let Some(taps) = set.taps {
                        m.taps = usize::try_from(taps).unwrap_or(usize::MAX);
                    }
                    if let Some(phase) = set.phase {
                        m.phase = phase.into();
                    }
                    if let Some(beta) = set.beta {
                        m.kaiser_beta = beta;
                    }
                    if set.cutoff_mode == Some(CutoffModeArg::NyquistStop) && set.cutoff.is_some() {
                        bail!("--cutoff cannot be combined with --cutoff-mode nyquist-stop");
                    }
                    if let Some(cutoff) = set.cutoff {
                        m.cutoff = cutoff;
                        if set.cutoff_mode.is_none() {
                            m.cutoff_mode = CutoffMode::Fraction;
                        }
                    }
                    if let Some(mode) = set.cutoff_mode {
                        m.cutoff_mode = mode.into();
                    }
                    if let Some(unit) = set.taps_unit {
                        m.taps_unit = unit.into();
                    }
                    if let Some(mix) = set.phase_mix {
                        m.phase_mix = mix;
                        if set.phase.is_none() {
                            m.phase = FilterPhase::Intermediate;
                        }
                    }
                    if let Some(a) = set.auto_taps_db {
                        m.auto_taps = Some(audivane_protocol::FilterErrorBudget::Db(a));
                    }
                    if set.auto_taps_match_output {
                        m.auto_taps = Some(audivane_protocol::FilterErrorBudget::MatchOutput);
                    }
                    if set.manual_taps {
                        m.auto_taps = None;
                    }
                    if band_flag {
                        if set.cutoff_mode.is_some_and(|m| m != CutoffModeArg::Band) {
                            bail!(
                                "--passband-end, --stopband-start and --attenuation need \
                                 --cutoff-mode band"
                            );
                        }
                        if let Some(v) = set.passband_end {
                            m.band.passband_end = v;
                        }
                        if let Some(v) = set.stopband_start {
                            m.band.stopband_start = v;
                        }
                        if let Some(v) = set.attenuation {
                            m.band.attenuation_db = v;
                        }
                        m.cutoff_mode = CutoffMode::Band;
                    }
                    if let Some(window) = set.window {
                        m.window = window.into();
                    }
                    if let Some(taper) = set.taper {
                        m.taper = taper;
                        m.window = WindowKind::Tukey;
                    }
                    if let Some(preserve) = set.preserve_samples {
                        m.preserve_samples = preserve;
                    }
                    let next = m
                        .into_upsampler_config()
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    upsampler.design = next.design;
                    upsampler.preset = None;
                    changed = true;
                }
                if !changed {
                    bail!(
                        "nothing to change: give --ratio, --max-rate, --off, --preset, --design \
                         or a tuning flag, or `upsample response`"
                    );
                }
                client.call(Call::SetUpsampler(upsampler.clone()))?;
                println!("{}", describe_upsampler(&upsampler));
            }
        },
        CtlCommand::Pitch(args) => {
            let cfg: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
            let mut pitch = cfg.chain.pitch;
            let mut changed = false;
            if args.off {
                pitch = PitchConfig {
                    quality: pitch.quality,
                    ..Default::default()
                };
                changed = true;
            } else if let Some(cents) = args.cents {
                pitch.enabled = true;
                pitch.cents = cents;
                pitch.semitones = None;
                pitch.reference_hz = None;
                pitch.target_hz = None;
                changed = true;
            } else if let Some(semitones) = args.semitones {
                pitch.enabled = true;
                pitch.cents = 0.0;
                pitch.semitones = Some(semitones);
                pitch.reference_hz = None;
                pitch.target_hz = None;
                changed = true;
            } else if let (Some(ref_hz), Some(tgt_hz)) = (args.reference_hz, args.target_hz) {
                pitch.enabled = true;
                pitch.cents = 0.0;
                pitch.semitones = None;
                pitch.reference_hz = Some(ref_hz);
                pitch.target_hz = Some(tgt_hz);
                changed = true;
            }
            if let Some(quality) = args.quality {
                pitch.quality = quality.into();
                changed = true;
            }
            if changed {
                client.call(Call::SetPitch(pitch.clone()))?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&pitch)?);
            } else {
                println!("{}", describe_pitch(&pitch));
            }
        }
        CtlCommand::Varispeed(args) => {
            if let Some(ms) = args.ramp_ms
                && !ms.is_finite()
            {
                bail!("invalid --ramp-ms value {ms}: expected a finite number between 0 and 10000");
            }
            let keep_pitch = match (args.keep_pitch, args.no_keep_pitch) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let status: VarispeedStatus = match args.value.as_deref().map(str::trim) {
                Some(v) if v.eq_ignore_ascii_case("off") => {
                    if args.ramp_ms.is_some() {
                        bail!("--ramp-ms cannot be used with `off`");
                    }
                    if keep_pitch == Some(true) {
                        bail!("--keep-pitch cannot be used with `off`");
                    }
                    serde_json::from_value(client.call(Call::SetVarispeed {
                        enabled: Some(false),
                        speed: None,
                        ramp_ms: None,
                        keep_pitch,
                    })?)?
                }
                Some(v) if v.eq_ignore_ascii_case("on") => {
                    serde_json::from_value(client.call(Call::SetVarispeed {
                        enabled: Some(true),
                        speed: None,
                        ramp_ms: args.ramp_ms,
                        keep_pitch,
                    })?)?
                }
                Some(v) => {
                    let speed: f64 = match v.parse::<f64>() {
                        Ok(s) if s.is_finite() => s,
                        _ => bail!(
                            "invalid varispeed value {v:?}: expected a speed between 0.5 and 2.0, `on`, or `off`"
                        ),
                    };
                    serde_json::from_value(client.call(Call::SetVarispeed {
                        enabled: Some(true),
                        speed: Some(speed),
                        ramp_ms: args.ramp_ms,
                        keep_pitch,
                    })?)?
                }
                None => {
                    if args.ramp_ms.is_some() || keep_pitch.is_some() {
                        serde_json::from_value(client.call(Call::SetVarispeed {
                            enabled: None,
                            speed: None,
                            ramp_ms: args.ramp_ms,
                            keep_pitch,
                        })?)?
                    } else {
                        let st: Status = serde_json::from_value(client.call(Call::Status)?)?;
                        st.varispeed.unwrap_or_else(|| {
                            VarispeedStatus::from_config(&VarispeedConfig::default())
                        })
                    }
                }
            };
            if json {
                print_json(&serde_json::to_value(&status)?)?;
            } else {
                println!("varispeed: {}", format_varispeed(&status));
            }
        }
        CtlCommand::Outputs => {
            let st: Status = serde_json::from_value(client.call(Call::Status)?)?;
            if json {
                print_json(&serde_json::to_value(&st.outputs)?)?;
            } else {
                print_outputs(&st.outputs);
            }
        }
        CtlCommand::Follower(args) => {
            let call = |device: String, remove: bool, set: FollowerSetArgs| Call::SetFollower {
                device,
                remove,
                enabled: set.enabled.map(|e| e == OnOff::On),
                trim_ms: set.trim_ms,
                volume_db: set.volume_db,
            };
            let call = match (args.command, args.device) {
                (Some(FollowerCommand::Add { device, set }), _) => call(device, false, set),
                (Some(FollowerCommand::Remove { device }), _) => {
                    call(device, true, FollowerSetArgs::default())
                }
                (None, Some(device)) => {
                    // Editing never adds: a typo must not open another DAC.
                    let st: Status = serde_json::from_value(client.call(Call::Status)?)?;
                    if !st.outputs.followers.iter().any(|f| f.device == device) {
                        bail!("{device} is not a follower (add it with `follower add {device}`)");
                    }
                    call(device, false, args.set)
                }
                (None, None) => bail!("give a follower's DEVICE, or `add` / `remove`"),
            };
            let outputs: OutputsStatus = serde_json::from_value(client.call(call)?)?;
            if json {
                print_json(&serde_json::to_value(&outputs)?)?;
            } else {
                print_outputs(&outputs);
            }
        }
        CtlCommand::Dsp { state } => {
            let enabled = state == OnOff::On;
            client.call(Call::SetDspEnabled { enabled })?;
            println!(
                "DSP {}",
                if enabled {
                    "on"
                } else {
                    "off (bit-perfect path)"
                }
            );
        }
        CtlCommand::Ab { command } => match command {
            AbCommand::Stage { stage, level } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let (patch, label) =
                    stage_patch(&current.chain, stage).map_err(|e| anyhow::anyhow!(e))?;
                set_ab_patch(client, patch, label, parse_level(&level)?)?;
            }
            AbCommand::Filter { filter, level } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                if current.chain.upsampler.target == RateTarget::Off {
                    bail!("upsampling is off in chain A; a filter compare runs at A's ratio");
                }
                let path = Path::new(&filter);
                let (upsampler, label) = if path.is_file() {
                    let text = std::fs::read_to_string(path)
                        .with_context(|| format!("read design file {}", path.display()))?;
                    let design: audivane_protocol::StoredDesign = serde_json::from_str(&text)
                        .with_context(|| format!("parse design JSON {}", path.display()))?;
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or(filter.clone());
                    (
                        serde_json::json!({"preset": null, "design": design}),
                        format!("Filter: {name}"),
                    )
                } else if filter == audivane_protocol::RECOMMENDED_PRESET
                    || FilterPreset::from_name(&filter).is_some()
                {
                    (
                        serde_json::json!({"preset": filter, "design": null}),
                        format!("Filter: {filter}"),
                    )
                } else {
                    bail!(
                        "`{filter}` is neither a design file nor a filter preset (run `avdsp \
                         ctl upsample --list-presets`)"
                    );
                };
                set_ab_patch(
                    client,
                    serde_json::json!({ "upsampler": upsampler }),
                    label,
                    parse_level(&level)?,
                )?;
            }
            AbCommand::Output {
                bits,
                dither,
                level,
            } => {
                let (patch, label) = output_patch(bits, dither.as_deref())?;
                set_ab_patch(client, patch, label, parse_level(&level)?)?;
            }
            AbCommand::Set { profile, level } => {
                let profile = crate::profile::load(&profile)?;
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let label = (!profile.name.is_empty()).then(|| profile.name.clone());
                // A full chain B: its upsampler target, limiter and output
                // follow chain A; it starts with A's filter.
                let chain_b = ChainConfig {
                    dc_block: None,
                    crossfeed: None,
                    eq: Some(profile),
                    convolution: Vec::new(),
                    pitch: Default::default(),
                    upsampler: current.chain.upsampler.clone(),
                    harmonics: None,
                    output: current.chain.output,
                    limiter: current.chain.limiter.clone(),
                };
                let ab = AbConfig {
                    label,
                    level: parse_level(&level)?,
                    ..AbConfig::new(AbB::Chain(Box::new(chain_b)))
                };
                let value = client.call(Call::SetAb(Some(ab)))?;
                // Chain B is realigned with chain A when needed; say so.
                for note in reply_notes(&value) {
                    println!("note: {note}");
                }
                let status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
                println!(
                    "A/B enabled: side {} ({})",
                    side_name(status.current),
                    ab_levels(&status)
                );
            }
            AbCommand::A { fade } => print_ab(client.call(Call::AbSelect {
                side: AbSide::A,
                crossfade_ms: fade,
            })?)?,
            AbCommand::B { fade } => print_ab(client.call(Call::AbSelect {
                side: AbSide::B,
                crossfade_ms: fade,
            })?)?,
            AbCommand::Measure {
                seconds,
                accept,
                cancel,
                wait,
            } => {
                let value = client.call(Call::AbMeasure {
                    max_seconds: seconds,
                    cancel,
                    accept,
                })?;
                let mut status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
                println!("{}", describe_measure(&status));
                while wait && status.measuring.is_some() {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let st: Status = serde_json::from_value(client.call(Call::Status)?)?;
                    let Some(ab) = st.ab else {
                        println!("A/B off");
                        break;
                    };
                    status = ab;
                    println!("{}", describe_measure(&status));
                }
            }
            AbCommand::Off => {
                let value = client.call(Call::SetAb(None))?;
                let status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
                let _ = status;
                println!("A/B off");
            }
        },
        CtlCommand::Rates(args) => {
            let mut current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
            let changing = !args.enable.is_empty()
                || !args.disable.is_empty()
                || args.default.is_some()
                || args.bypass_resampler.is_some();
            if changing {
                let rate = &mut current.rate;
                if let Some(who) = args.bypass_resampler {
                    rate.bypass_resampler = who.into();
                }
                for r in &args.enable {
                    if !audivane_protocol::ALLOWED_RATES.contains(r) {
                        bail!(
                            "{r} Hz is not a rate the engine runs at ({})",
                            audivane_protocol::ALLOWED_RATES
                                .iter()
                                .map(u32::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if !rate.allowed_rates.contains(r) {
                        rate.allowed_rates.push(*r);
                    }
                }
                rate.allowed_rates.retain(|r| !args.disable.contains(r));
                if rate.allowed_rates.is_empty() {
                    bail!("at least one rate must stay allowed");
                }
                if let Some(d) = args.default {
                    if !rate.allowed_rates.contains(&d) {
                        bail!("{d} Hz is not allowed; add it with --enable {d}");
                    }
                    rate.default_rate = d;
                }
                // The daemon sorts the list and moves a disabled default.
                client.call(Call::SetConfig(current))?;
                current = serde_json::from_value(client.call(Call::GetConfig)?)?;
            }
            if json {
                print_json(&serde_json::to_value(&current.rate)?)?;
            } else {
                let list: Vec<String> = current
                    .rate
                    .allowed_rates
                    .iter()
                    .map(|&r| {
                        if r == current.rate.default_rate {
                            format!("{r}*")
                        } else {
                            r.to_string()
                        }
                    })
                    .collect();
                println!("allowed rates: {} Hz (* default)", list.join(", "));
                println!(
                    "bypass resampler: {} (Direct and DSP off at a rate the DAC lacks)",
                    match current.rate.bypass_resampler {
                        audivane_protocol::ipc::BypassResampler::Engine => "engine",
                        audivane_protocol::ipc::BypassResampler::Pipewire => "pipewire",
                    }
                );
            }
        }
        CtlCommand::Volume(args) => {
            if args.edits_settings() {
                let mut cfg: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let v = &mut cfg.volume;
                if let Some(mode) = args.mode {
                    v.mode = mode;
                }
                if let Some(OptDb(db)) = args.hardware_db {
                    v.hardware_db = db;
                }
                if let Some(OptDb(db)) = args.ceiling {
                    v.max_volume_db = db;
                }
                if let Some(policy) = args.sink_volume {
                    v.sink_volume = policy;
                }
                if let Some(mode) = args.guard {
                    v.guard.mode = mode;
                }
                if let Some(rise) = args.rise {
                    v.guard.max_rise_db_per_s = rise;
                }
                if let Some(OptDb(db)) = args.stream_max {
                    v.guard.stream_max_db = db;
                }
                client.call(Call::SetConfig(cfg))?;
            }
            if !args.edits_level() {
                let status: audivane_protocol::ipc::Status =
                    serde_json::from_value(client.call(Call::Status)?)?;
                let volume = match status.volume {
                    Some(v) => v,
                    None => {
                        let current: EngineConfig =
                            serde_json::from_value(client.call(Call::GetConfig)?)?;
                        audivane_protocol::ipc::VolumeStatus::from_config(&current.volume)
                    }
                };
                if json {
                    print_json(&serde_json::to_value(volume)?)?;
                } else {
                    print_volume(&volume);
                }
            } else {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let volume_db = args.db.unwrap_or(current.volume.volume_db);
                let balance_db = args.balance.unwrap_or(current.volume.balance_db);
                let muted = if args.mute {
                    true
                } else if args.unmute {
                    false
                } else {
                    current.volume.muted
                };
                let value = client.call(Call::SetVolume {
                    volume_db,
                    balance_db,
                    muted,
                })?;
                if json {
                    print_json(&value)?;
                } else {
                    let status: audivane_protocol::ipc::VolumeStatus =
                        serde_json::from_value(value)?;
                    print_volume(&status);
                }
            }
        }
        CtlCommand::Streams => {
            let value = client.call(Call::ListStreams)?;
            if json {
                print_json(&value)?;
            } else {
                let streams: Vec<audivane_protocol::ipc::StreamInfo> =
                    serde_json::from_value(value)?;
                print_streams(&streams)?;
            }
        }
        CtlCommand::Stream {
            command: StreamCommand::Volume(args),
        } => {
            let value = client.call(Call::SetStreamVolume {
                node: args.node,
                volume_db: args.db,
                muted: args.muted(),
            })?;
            if json {
                print_json(&value)?;
            } else {
                let volume: audivane_protocol::ipc::StreamVolume = serde_json::from_value(value)?;
                println!("{}", stream_volume_line(&volume, args.db));
            }
        }
        CtlCommand::Preset { command } => match command {
            PresetCommand::List => {
                let value = client.call(Call::ListPresets)?;
                if json {
                    print_json(&value)?;
                } else {
                    let presets: Vec<audivane_protocol::ipc::PresetInfo> =
                        serde_json::from_value(value)?;
                    let active: Option<String> = serde_json::from_value::<
                        audivane_protocol::ipc::Status,
                    >(client.call(Call::Status)?)
                    .ok()
                    .and_then(|status| status.preset.map(|preset| preset.name));
                    print_presets(&presets, active.as_deref())?;
                }
            }
            PresetCommand::Show { name } => {
                let value = client.call(Call::GetPreset { name })?;
                print_json(&value)?;
            }
            PresetCommand::Save { name, overwrite } => {
                let value = client.call(Call::SavePreset { name, overwrite })?;
                let info: audivane_protocol::ipc::PresetInfo = serde_json::from_value(value)?;
                if json {
                    println!("{}", serde_json::to_string(&info)?);
                } else {
                    println!("saved preset {:?} ({})", info.name, preset_summary(&info));
                }
            }
            PresetCommand::Load { name } => {
                client.call(Call::LoadPreset { name: name.clone() })?;
                println!("loaded preset {name:?}");
            }
            PresetCommand::Delete { name } => {
                client.call(Call::DeletePreset { name: name.clone() })?;
                println!("deleted preset {name:?}");
            }
            PresetCommand::Rename { from, to } => {
                let value = client.call(Call::RenamePreset {
                    from: from.clone(),
                    to: to.clone(),
                })?;
                let info: audivane_protocol::ipc::PresetInfo = serde_json::from_value(value)?;
                if json {
                    println!("{}", serde_json::to_string(&info)?);
                } else {
                    println!("renamed preset {from:?} to {:?}", info.name);
                }
            }
            PresetCommand::Import { file, name } => {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("cannot read {}", file.display()))?;
                let name = match name {
                    Some(name) => name,
                    None => file
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_string)
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "{} has no usable file name; pass --name",
                                file.display()
                            )
                        })?,
                };
                let value = client.call(Call::ImportApo {
                    name: name.clone(),
                    text,
                })?;
                let info: audivane_protocol::ipc::PresetInfo = serde_json::from_value(value)?;
                if json {
                    println!("{}", serde_json::to_string(&info)?);
                } else {
                    println!(
                        "imported preset {:?} ({})",
                        info.name,
                        preset_summary(&info)
                    );
                }
            }
        },
        CtlCommand::Devices => {
            let value = client.call(Call::ListDevices)?;
            if json {
                print_json(&value)?;
            } else {
                let devices: Vec<audivane_protocol::ipc::DeviceInfo> =
                    serde_json::from_value(value)?;
                print_devices(&devices)?;
            }
        }
        CtlCommand::Route(args) => {
            let (target, sink) = args.resolve()?;
            client.call(Call::Route { target, sink })?;
            println!("routed to {}", sink_name(sink));
        }
        CtlCommand::Mode { command } => {
            let mode = match command {
                ModeCommand::System => RoutingMode::System,
                ModeCommand::Apps { rules } => {
                    let mut parsed = Vec::new();
                    for rule in rules {
                        let (app, sink) = rule
                            .split_once('=')
                            .with_context(|| format!("rule {rule:?} must be APP=SINK"))?;
                        let sink = match sink {
                            "dsp" => SinkChoice::Dsp,
                            "direct" => SinkChoice::Direct,
                            "default" => SinkChoice::Default,
                            other => bail!("unknown sink {other:?} in rule {rule:?}"),
                        };
                        parsed.push(audivane_protocol::ipc::AppRule {
                            app: app.trim().to_string(),
                            sink,
                        });
                    }
                    if parsed.is_empty() {
                        // Keep the rules already stored, if any.
                        let current: EngineConfig =
                            serde_json::from_value(client.call(Call::GetConfig)?)?;
                        if let RoutingMode::Apps { rules } = current.routing {
                            parsed = rules;
                        }
                    }
                    RoutingMode::Apps { rules: parsed }
                }
            };
            client.call(Call::SetRoutingMode(mode.clone()))?;
            match mode {
                RoutingMode::System => {
                    println!("routing: system-wide (Audivane DSP is the default sink)")
                }
                RoutingMode::Apps { rules } if rules.is_empty() => {
                    println!("routing: per-app (no rules)")
                }
                RoutingMode::Apps { rules } => {
                    println!("routing: per-app");
                    for rule in rules {
                        println!("  {} -> {}", rule.app, sink_name(rule.sink));
                    }
                }
            }
        }
        CtlCommand::NowPlaying => {
            let value = client.call(Call::NowPlaying)?;
            if json {
                print_json(&value)?;
            } else if let Some(now) =
                serde_json::from_value::<Option<audivane_protocol::ipc::NowPlaying>>(value)?
            {
                println!(
                    "{}  [{}]",
                    now.title.as_deref().unwrap_or("(no title)"),
                    now.status
                );
                if let Some(artist) = &now.artist {
                    println!("artist: {artist}");
                }
                if let Some(album) = &now.album {
                    println!("album:  {album}");
                }
                let native = match (now.native_rate, now.native_bits) {
                    (Some(rate), Some(bits)) => format!("{rate} Hz / {bits}-bit"),
                    (Some(rate), None) => format!("{rate} Hz"),
                    (None, Some(bits)) => format!("{bits}-bit"),
                    (None, None) => "not reported".to_string(),
                };
                println!("native: {native}");
                println!("player: {}", now.player);
            } else {
                println!("nothing playing");
            }
        }
        CtlCommand::Design { command } => match command {
            DesignCommand::Resolve { preset } => {
                let value = client.call(Call::ResolveDesign { preset })?;
                print_json(&value)?;
            }
            DesignCommand::Check(args) => {
                let file = &args.file;
                let text = if file.as_os_str() == "-" {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    text
                } else {
                    std::fs::read_to_string(file)
                        .with_context(|| format!("cannot read {}", file.display()))?
                };
                let design: audivane_protocol::StoredDesign = serde_json::from_str(&text)
                    .with_context(|| format!("{} is not a valid design JSON", file.display()))?;
                let target = match (args.ratio, args.max_rate, args.max_rate_any, args.out_rate) {
                    (Some(r), ..) => Some(RateTarget::Ratio(r)),
                    (_, Some(m), ..) => Some(RateTarget::MaxRate(m)),
                    (_, _, Some(m), _) => Some(RateTarget::MaxRateAny(m)),
                    (.., Some(r)) => Some(RateTarget::Rate(r)),
                    _ => None,
                };
                let value = client.call(Call::CheckDesign {
                    design,
                    target,
                    source_rate: args.rate,
                })?;
                let check: audivane_protocol::ipc::DesignCheck = serde_json::from_value(value)?;
                if json {
                    println!("{}", serde_json::to_string(&check)?);
                } else {
                    print_design_check(&check);
                }
                if !check.errors.is_empty() {
                    return Ok(1);
                }
            }
        },
        CtlCommand::Selftest => {
            let value = client.call(Call::SelfTest)?;
            let result: audivane_protocol::ipc::SelfTestResult = serde_json::from_value(value)?;
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                println!(
                    "self-test: {} ({} frames at {} Hz)",
                    if result.passed { "PASS" } else { "FAIL" },
                    result.frames,
                    result.rate
                );
                println!("expected: {}", result.expected_hash);
                println!("actual:   {}", result.actual_hash);
                if !result.detail.is_empty() {
                    println!("detail:   {}", result.detail);
                }
            }
            if !result.passed {
                return Ok(1);
            }
        }
        CtlCommand::Release => {
            client.call(Call::ReleaseDevice)?;
            println!("device released");
        }
        CtlCommand::Acquire => {
            client.call(Call::AcquireDevice)?;
            println!("device acquired");
        }
        CtlCommand::Watch(args) => return watch(client, args.topic, json),
        CtlCommand::Shutdown => {
            client.call(Call::Shutdown)?;
            println!("audivane-dsp stopping");
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- watch

fn watch(client: &mut Client, topic: Option<WatchTopic>, json: bool) -> CtlResult<i32> {
    let topics = match topic {
        Some(WatchTopic::Meters) => vec![Topic::Meters],
        Some(WatchTopic::Status) => vec![Topic::Status],
        Some(WatchTopic::Streams) => vec![Topic::Streams],
        Some(WatchTopic::NowPlaying) => vec![Topic::NowPlaying],
        Some(WatchTopic::Config) => vec![Topic::Config],
        Some(WatchTopic::Presets) => vec![Topic::Presets],
        Some(WatchTopic::Spectrum) => vec![Topic::Spectrum],
        Some(WatchTopic::Devices) => vec![Topic::Devices],
        None => vec![
            Topic::Meters,
            Topic::Status,
            Topic::Streams,
            Topic::NowPlaying,
            Topic::Config,
            Topic::Presets,
        ],
    };
    client.call(Call::Subscribe { topics })?;
    // Events can legitimately be minutes apart (e.g. `watch status` on an
    // idle daemon); block until one arrives or the daemon goes away.
    client
        .stream
        .set_read_timeout(None)
        .context("clear the socket read timeout")?;
    // Only a terminal gets in-place meter updates; redirected output is
    // plain newline-delimited text. `--json` selects raw event lines.
    let tty = std::io::stdout().is_terminal();
    let mut line = String::new();
    loop {
        line.clear();
        let read = client
            .reader
            .read_line(&mut line)
            .map_err(|err| anyhow::anyhow!("reading events failed: {err}"))?;
        if read == 0 {
            return Ok(0);
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if json {
            println!("{}", serde_json::to_string(&value)?);
            let _ = std::io::stdout().flush();
            continue;
        }
        let Some((text, in_place)) = human_event(&value) else {
            continue;
        };
        if tty && in_place {
            print!("\r{text}");
        } else if tty {
            // Leave the in-place meter line, then start a fresh one.
            print!("\n{text}\n");
        } else {
            println!("{text}");
        }
        let _ = std::io::stdout().flush();
    }
}

/// Human rendering of one event line: the text, and whether it may overwrite
/// the previous line (meters only).
fn human_event(value: &Value) -> Option<(String, bool)> {
    use audivane_protocol::ipc::{Meters, NowPlaying, Status, StreamInfo};
    let data = value.get("data")?;
    match value.get("event").and_then(Value::as_str)? {
        "meters" => {
            let meters: Meters = serde_json::from_value(data.clone()).ok()?;
            Some((meter_line(&meters), true))
        }
        "status" => {
            let status: Status = serde_json::from_value(data.clone()).ok()?;
            Some((status_line(&status), false))
        }
        "streams" => {
            let streams: Vec<StreamInfo> = serde_json::from_value(data.clone()).ok()?;
            let mut text = format!("{} stream(s):", streams.len());
            for stream in &streams {
                text.push('\n');
                text.push_str("  ");
                text.push_str(&stream_line(stream));
            }
            Some((text, false))
        }
        "overload" => {
            let report: audivane_protocol::ipc::OverloadReport =
                serde_json::from_value(data.clone()).ok()?;
            Some((format!("overload: {}", report.message), false))
        }
        "now_playing" => {
            let now: Option<NowPlaying> = serde_json::from_value(data.clone()).ok()?;
            Some((
                match now {
                    Some(now) => format!(
                        "now playing: {} [{}]",
                        now.title.as_deref().unwrap_or("(no title)"),
                        now.status
                    ),
                    None => "now playing: nothing".to_string(),
                },
                false,
            ))
        }
        "config" => {
            let config: audivane_protocol::ipc::EngineConfig =
                serde_json::from_value(data.clone()).ok()?;
            Some((config_line(&config), false))
        }
        "presets" => {
            let presets: Vec<audivane_protocol::ipc::PresetInfo> =
                serde_json::from_value(data.clone()).ok()?;
            let mut text = format!("{} preset(s):", presets.len());
            for preset in &presets {
                text.push('\n');
                text.push_str("  ");
                text.push_str(&preset_line(preset));
            }
            Some((text, false))
        }
        "spectrum" => {
            let spectrum: audivane_protocol::ipc::Spectrum =
                serde_json::from_value(data.clone()).ok()?;
            Some((spectrum_line(&spectrum), true))
        }
        "devices" => {
            let devices: Vec<audivane_protocol::ipc::DeviceInfo> =
                serde_json::from_value(data.clone()).ok()?;
            let mut text = format!("{} device(s):", devices.len());
            for device in &devices {
                text.push_str(&format!(
                    "\n  {}{}  {}",
                    if device.active { "* " } else { "  " },
                    device.id,
                    device.name
                ));
            }
            Some((text, false))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- output

fn print_design_check(check: &audivane_protocol::ipc::DesignCheck) {
    for e in &check.errors {
        println!("refused {}: {}", e.path, e.message);
    }
    for p in &check.plans {
        let plan = &p.plan;
        let taps = if plan.grid_taps == 0 {
            "fixed filters".to_string()
        } else {
            format!(
                "{}{} taps on the x{} grid",
                if plan.grid_taps_exact { "" } else { "~" },
                plan.grid_taps,
                plan.grid_up
            )
        };
        let cpu = plan
            .cpu_load
            .map_or_else(String::new, |l| format!(", CPU {:.1} %", 100.0 * l));
        println!(
            "{} Hz ({}): {taps}, design memory {:.1} MB, resident {:.1} MB{cpu}",
            p.source_rate,
            p.ratio,
            plan.workspace_bytes as f64 / 1e6,
            plan.resident_bytes as f64 / 1e6,
        );
    }
    if check.errors.is_empty() && check.plans.is_empty() {
        println!("ok (the target does not upsample at the checked rates)");
    }
}

fn print_json(value: &Value) -> CtlResult<()> {
    // `config get` is the common case and is much nicer pretty-printed.
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn meter_line(meters: &audivane_protocol::ipc::Meters) -> String {
    let peaks = meters
        .peak_db
        .iter()
        .map(|db| format!("{db:6.1}"))
        .collect::<Vec<_>>()
        .join(" ");
    let rms = meters
        .rms_db
        .iter()
        .map(|db| format!("{db:6.1}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "peak [{peaks}] dBFS   rms [{rms}]   load {:5.1}%   xruns {}   clipped {}   {}",
        meters.dsp_load * 100.0,
        meters.xruns,
        meters.clipped,
        clip_line(meters)
    )
}

/// Per-channel clip counters, e.g. `clips [L:0 R:3]`.
fn clip_line(meters: &audivane_protocol::ipc::Meters) -> String {
    if meters.clipped_ch.is_empty() {
        return "clips []".to_string();
    }
    let labels = ["L", "R", "C", "LFE", "SL", "SR", "BL", "BR"];
    let values = meters
        .clipped_ch
        .iter()
        .enumerate()
        .map(|(channel, count)| {
            let label = labels.get(channel).copied().unwrap_or("CH");
            if meters.clipped_ch.len() > 2 && channel >= 2 {
                format!("{label}{channel}:{count}")
            } else {
                format!("{label}:{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("clips [{values}]")
}

/// Coarse spectrum view: one bar per octave from 20 Hz to 20 kHz.
fn spectrum_line(spectrum: &audivane_protocol::ipc::Spectrum) -> String {
    let bars = |levels: &[f32]| -> String {
        const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        levels
            .iter()
            .map(|&db| {
                if !db.is_finite() || db <= -90.0 {
                    '·'
                } else {
                    let t = ((db + 90.0) / 90.0).clamp(0.0, 1.0);
                    GLYPHS[((t * 7.0).round() as usize).min(7)]
                }
            })
            .collect()
    };
    let octave_peaks = |bins: &[f32]| -> Vec<f32> {
        let per_octave = f64::from(spectrum.bins_per_octave.max(1));
        (0..10)
            .map(|octave| {
                let lo = 20.0 * 2.0_f64.powi(octave);
                let hi = lo * 2.0;
                let mut peak = f32::NEG_INFINITY;
                for (bin, &db) in bins.iter().enumerate() {
                    let hz = spectrum.f_min_hz * 2.0_f64.powf((bin as f64 + 0.5) / per_octave);
                    if hz >= lo && hz < hi {
                        peak = peak.max(db);
                    }
                }
                peak
            })
            .collect()
    };
    format!(
        "spectrum {:>6.1}k  pre {}  post {}  (20 Hz..20 kHz, 1 bar/octave, -90..0 dBFS)",
        f64::from(spectrum.rate) / 1000.0,
        bars(&octave_peaks(&spectrum.pre)),
        bars(&octave_peaks(&spectrum.post))
    )
}

fn print_upsampler_response(response: &audivane_protocol::ipc::UpsamplerResponse) {
    let phase = match response.phase {
        audivane_protocol::FilterPhase::Linear => "linear",
        audivane_protocol::FilterPhase::Minimum => "minimum",
        audivane_protocol::FilterPhase::Intermediate => "intermediate",
    };
    if let Some(preset) = response.preset {
        println!(
            "upsampler: {} Hz -> {} Hz ({}), preset {}, {} taps, {} phase",
            response.source_rate,
            response.output_rate,
            response.rate_ratio,
            preset.name(),
            response.taps,
            phase
        );
    } else {
        println!(
            "upsampler: {} Hz -> {} Hz ({}), {} taps, {} phase, {:?} window",
            response.source_rate,
            response.output_rate,
            response.rate_ratio,
            response.taps,
            phase,
            response.filter_window
        );
    }
    if response.rate_ratio.is_unity() {
        println!("response:  flat (upsampling off)");
        println!("impulse:   [1.0]");
        return;
    }
    println!(
        "samples:   leak {:.1} dB at the input sample instants{}",
        response.sample_leak_db,
        if response.sample_leak_db < -250.0 {
            " (passes the input samples through)"
        } else {
            ""
        }
    );
    if response.preset.is_none() {
        println!(
            "cutoff:    {:.6} of source Nyquist ({:.1} Hz)",
            response.effective_cutoff,
            response.effective_cutoff * f64::from(response.source_rate) / 2.0
        );
    }
    println!(
        "passband:  ripple {:.4} dB, -0.1 dB at {:.0} Hz, -3 dB at {:.0} Hz",
        response.passband_ripple_db, response.minus_0_1_db_hz, response.minus_3_db_hz
    );
    println!(
        "stopband:  {:.1} dB from {:.0} Hz (source Nyquist {:.0} Hz)",
        response.stopband_atten_db,
        response.stopband_start_hz,
        f64::from(response.source_rate) / 2.0
    );
    println!(
        "latency:   {} samples at {} Hz ({:.3} ms)",
        response.latency_samples,
        response.output_rate,
        response.latency_samples as f64 * 1000.0 / f64::from(response.output_rate)
    );
    println!(
        "impulse:   {} samples around peak {}",
        response.impulse.len(),
        response.impulse_peak
    );
    if !response.crossings.is_empty() {
        let list: Vec<String> = response
            .crossings
            .iter()
            .map(|c| format!("{} dB {:.3} Hz", c.level_db, c.freq_hz))
            .collect();
        println!("crossings: {}", list.join(", "));
    }
}

fn status_line(status: &audivane_protocol::ipc::Status) -> String {
    let vol = status
        .volume
        .as_ref()
        .map(|v| {
            let vol_db = if v.volume_db == 0.0 { 0.0 } else { v.volume_db };
            let tag = if v.hardware { "hw" } else { "dsp" };
            let mute = if v.muted { " MUTE" } else { "" };
            format!("  vol {vol_db:.1} dB ({tag}{mute})")
        })
        .unwrap_or_default();
    format!(
        "{}  {}  {} Hz -> {} Hz ({})  {:.1} ms  dsp load {:.1}%{vol}  {}",
        status_state(status),
        status.device,
        status.graph_rate,
        status.output_rate,
        status.rate_ratio,
        status.latency_ms,
        status.dsp_load * 100.0,
        passthrough_line(&status.passthrough)
    )
}

fn format_volume(v: &audivane_protocol::ipc::VolumeStatus) -> String {
    let vol_db = if v.volume_db == 0.0 { 0.0 } else { v.volume_db };
    let bal_db = if v.balance_db == 0.0 {
        0.0
    } else {
        v.balance_db
    };
    let mode = if v.hardware {
        // The range the volume can take, digital attenuation included.
        format!(
            "hardware, effective range {:.1}..={:.1} dB",
            v.min_db, v.max_db
        )
    } else {
        "digital".to_string()
    };
    let mute = if v.muted { ", MUTED" } else { "" };
    let mut extra = Vec::new();
    if v.mode != audivane_protocol::ipc::VolumeMode::Auto {
        extra.push(format!("mode {}", wire_name(serde_json::to_value(v.mode))));
    }
    if let Some(hw) = v.hardware_db
        && !v.hardware
    {
        extra.push(format!("hardware fixed at {hw:.1} dB"));
    }
    if v.hardware && v.software_db != 0.0 {
        extra.push(format!(
            "hardware {:.1} dB + digital {:.1} dB",
            v.hardware_db.unwrap_or(0.0),
            v.software_db
        ));
    }
    if v.hardware_unavailable {
        extra.push("no hardware control, digital".into());
    }
    if let Some(c) = v.ceiling_db {
        extra.push(format!("ceiling {c:.1} dB"));
    }
    if v.sink_volume != audivane_protocol::ipc::SinkVolumePolicy::Ignore {
        extra.push(format!(
            "sink volume {}",
            wire_name(serde_json::to_value(v.sink_volume))
        ));
    }
    if v.guard != audivane_protocol::ipc::GuardMode::Off {
        extra.push(format!(
            "guard {}",
            wire_name(serde_json::to_value(v.guard))
        ));
    }
    if v.blocked > 0 {
        extra.push(format!("{} blocked", v.blocked));
    }
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(" ({})", extra.join(", "))
    };
    let bal = if bal_db.abs() < 1e-6 {
        "center".to_string()
    } else if bal_db < 0.0 {
        format!("{bal_db:+.1} dB (R {bal_db:.1} dB)")
    } else {
        format!("{bal_db:+.1} dB (L -{bal_db:.1} dB)")
    };
    format!("{vol_db:.1} dB, balance {bal}{mute} [{mode}]{extra}")
}

/// The wire (snake_case) name of a protocol enum, from its JSON value.
fn wire_name(value: serde_json::Result<serde_json::Value>) -> String {
    value
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn print_volume(v: &audivane_protocol::ipc::VolumeStatus) {
    println!("volume: {}", format_volume(v));
}

fn print_outputs(o: &OutputsStatus) {
    if o.followers.is_empty() {
        println!("followers:  none");
        return;
    }
    println!("alignment:  {:.0} ms on the main output", o.alignment_ms);
    for f in &o.followers {
        println!("follower:   {}", format_follower(f));
    }
}

fn format_follower(f: &FollowerStatus) -> String {
    let state = match f.state {
        FollowerState::Off => "off",
        FollowerState::Opening => "opening",
        FollowerState::Locking => "locking",
        FollowerState::Locked => "locked",
        FollowerState::Muted => "muted",
        FollowerState::Unaligned => "unaligned",
        FollowerState::Lost => "lost",
    };
    let mut s = format!("{} {state}", f.device);
    if f.state == FollowerState::Unaligned || f.offset_ms > 0.0 {
        s += &format!(
            " ({:.0} ms late; a smaller period on it helps)",
            f.offset_ms
        );
    }
    if !f.enabled {
        s += ", disabled";
    }
    s += &format!(", trim {:+.2} ms, volume {:.1} dB", f.trim_ms, f.volume_db);
    if f.device_rate > 0 {
        s += &format!(
            ", {:+.2} ppm, error {:.2} frames, {} Hz × {}, latency {:.1} ms, skips {}, xruns {}",
            f.ppm, f.phase_error_frames, f.device_rate, f.period, f.latency_ms, f.skips, f.xruns
        );
    }
    if let Some(e) = &f.error {
        s += &format!(", error: {e}");
    }
    s
}

fn format_varispeed(vs: &VarispeedStatus) -> String {
    let mut notes = Vec::new();
    if vs.keep_pitch {
        notes.push("keep pitch".to_string());
    }
    if vs.active {
        if (vs.target - vs.speed).abs() > 1e-6 || vs.ramping {
            let mut t = format!("target {}x", format_speed(vs.target));
            if vs.ramping {
                t.push_str(", ramping");
            }
            notes.push(t);
        }
        if (vs.requested - vs.target).abs() > 1e-6 {
            notes.push(format!("requested {}x", format_speed(vs.requested)));
        }
        if vs.fading {
            notes.push("fading".to_string());
        }
        if vs.speed_limit < 2.0 - 1e-9 {
            let cap = match vs.cap_reason {
                Some(reason) => format!(
                    "cap {}x ({})",
                    format_speed(vs.speed_limit),
                    wire_name(serde_json::to_value(reason))
                ),
                None => format!("cap {}x", format_speed(vs.speed_limit)),
            };
            notes.push(cap);
        }
        if let Some(held) = vs.held {
            notes.push(format!("held: {}", wire_name(serde_json::to_value(held))));
        }
        if vs.overload_hold != VarispeedOverloadHold::None {
            notes.push(format!(
                "overload hold: {}",
                wire_name(serde_json::to_value(vs.overload_hold))
            ));
        }
        if vs.latency_frames > 0.0 {
            notes.push(format!(
                "{:.1} frames{}",
                vs.latency_frames,
                if vs.latency_exact { "" } else { " est." }
            ));
        }
        if vs.underfeed_events > 0 {
            notes.push(format!(
                "{} underfeed{} ({} frames)",
                vs.underfeed_events,
                if vs.underfeed_events == 1 { "" } else { "s" },
                vs.underfeed_frames
            ));
        }
        if vs.short_cycles > 0 {
            notes.push(format!(
                "{} short cycle{}",
                vs.short_cycles,
                if vs.short_cycles == 1 { "" } else { "s" }
            ));
        }
        if let Some(err) = &vs.error {
            notes.push(format!("error: {err}"));
        }
        if notes.is_empty() {
            format!("{}x", format_speed(vs.speed))
        } else {
            format!("{}x ({})", format_speed(vs.speed), notes.join(", "))
        }
    } else if vs.enabled {
        notes.push(format!("requested {}x", format_speed(vs.requested)));
        notes.push("inactive".to_string());
        if vs.overload_hold != VarispeedOverloadHold::None {
            notes.push(format!(
                "overload hold: {}",
                wire_name(serde_json::to_value(vs.overload_hold))
            ));
        }
        if let Some(err) = &vs.error {
            notes.push(format!("error: {err}"));
        }
        format!("on ({})", notes.join(", "))
    } else {
        if vs.overload_hold != VarispeedOverloadHold::None {
            notes.push(format!(
                "overload hold: {}",
                wire_name(serde_json::to_value(vs.overload_hold))
            ));
        }
        if let Some(err) = &vs.error {
            notes.push(format!("error: {err}"));
        }
        if notes.is_empty() {
            "off".to_string()
        } else {
            format!("off ({})", notes.join(", "))
        }
    }
}

fn print_status(
    status: &audivane_protocol::ipc::Status,
    config: Option<&EngineConfig>,
) -> CtlResult<()> {
    println!("audivane-dsp {}", status.version);
    println!("state:      {}", status_state(status));
    println!("device:     {}", status.device);
    println!(
        "format:     {}",
        status
            .format
            .map(|f| format!("{f:?}").to_lowercase() + &used_bits_note(status, f))
            .unwrap_or_else(|| "not negotiated".to_string())
    );
    println!(
        "rate:       {} Hz graph -> {} Hz DAC ({})",
        status.graph_rate, status.output_rate, status.rate_ratio
    );
    if !status.effective_rates.is_empty() {
        let rates: Vec<String> = status.effective_rates.iter().map(u32::to_string).collect();
        println!(
            "rates:      {} Hz offered (default {})",
            rates.join(", "),
            status
                .effective_default_rate
                .map_or_else(|| "-".to_string(), |r| r.to_string())
        );
    }
    if !status.effective_rates.is_empty() && status.direct_rates != status.effective_rates {
        // Direct's rates: native ones (bit-perfect) and, with the engine
        // bypass resampler, ones the engine converts.
        let native = |r: &u32| {
            status
                .device_caps
                .as_ref()
                .is_none_or(|c| c.rates.iter().any(|x| x.rate == *r))
        };
        let (bit_perfect, converted): (Vec<u32>, Vec<u32>) =
            status.direct_rates.iter().partition(|r| native(r));
        let list = |v: &[u32]| v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
        if status.direct_rates.is_empty() {
            println!("direct:     no native rate (Audivane Direct cannot play here)");
        } else if converted.is_empty() {
            println!(
                "direct:     {} Hz bit-perfect (other rates resampled)",
                list(&bit_perfect)
            );
        } else {
            println!(
                "direct:     {} Hz bit-perfect, {} Hz converted by the engine",
                list(&bit_perfect),
                list(&converted)
            );
        }
    }
    if let Some(b) = &status.bypass_resampler {
        let mode = match b.mode {
            audivane_protocol::ipc::BypassResampler::Engine => "engine",
            audivane_protocol::ipc::BypassResampler::Pipewire => "pipewire",
        };
        if b.active {
            println!(
                "bypass:     {mode}, converting {} Hz -> {} Hz (not bit-perfect)",
                status.graph_rate, status.output_rate
            );
        } else if let Some(reason) = &b.reason {
            println!("bypass:     {mode}, {reason}");
        }
    }
    if let Some(caps) = &status.device_caps {
        println!("device can: {}", caps.summary());
    }
    if let Some(bits) = status.dither_bits {
        println!("dither:     {bits} bits");
    }
    println!(
        "buffers:    quantum {} frames, latency {:.1} ms",
        status.quantum, status.latency_ms
    );
    let peak = if status.rt_load_max > 0.0 {
        format!(" (peak {:.1}%)", status.rt_load_max * 100.0)
    } else {
        String::new()
    };
    println!(
        "engine:     dsp {}, load {:.1}%{}, xruns {}",
        if status.dsp_enabled { "on" } else { "off" },
        status.dsp_load * 100.0,
        peak,
        status.xruns
    );
    if let Some(vs) = &status.varispeed
        && (vs.enabled
            || vs.active
            || vs.error.is_some()
            || vs.held.is_some()
            || vs.overload_hold != VarispeedOverloadHold::None)
    {
        println!("varispeed:  {}", format_varispeed(vs));
    }
    if !status.outputs.followers.is_empty() {
        print_outputs(&status.outputs);
    }
    if let Some(error) = &status.error {
        println!("error:      {error}");
    }
    if let Some(overload) = &status.overload {
        println!("overload:   {}", overload.message);
    }
    match status.active_sink {
        Some(sink) => println!("sink:       {} (active)", sink_name(sink)),
        None => println!("sink:       none"),
    }
    println!("{}", passthrough_line(&status.passthrough));
    if let Some(volume) = &status.volume {
        println!("volume:     {}", format_volume(volume));
    }
    match &status.ab {
        Some(ab) if ab.enabled => {
            println!(
                "A/B:        side {}, offset {:+.2} dB",
                side_name(ab.current),
                ab.offset_db
            );
            if ab.measuring.is_some() || matches!(ab.level, LevelMatch::Measured { .. }) {
                println!("            {}", describe_measure(ab));
            }
        }
        _ => println!("A/B:        off"),
    }
    match &status.preset {
        Some(preset) => println!(
            "preset:     {:?}{}",
            preset.name,
            if preset.modified {
                " (modified)"
            } else {
                " (saved)"
            }
        ),
        None => println!("preset:     none"),
    }
    if let Some(cfg) = config {
        println!(
            "dc-block:   {}",
            summarize_dc_block(cfg.chain.dc_block.as_ref())
        );
        println!(
            "crossfeed:  {}",
            summarize_crossfeed(
                cfg.chain.crossfeed.as_ref(),
                XfRate::new(Some(status.graph_rate), cfg.rate.default_rate)
            )
        );
        println!("eq:         {}", summarize_eq(cfg.chain.eq.as_ref()));
        println!("conv:       {}", summarize_conv(&cfg.chain.convolution));
        println!(
            "harmonics:  {}",
            summarize_harmonics(cfg.chain.harmonics.as_ref())
        );
    }
    Ok(())
}

fn print_presets(
    presets: &[audivane_protocol::ipc::PresetInfo],
    active: Option<&str>,
) -> CtlResult<()> {
    if presets.is_empty() {
        println!("no presets");
        return Ok(());
    }
    for preset in presets {
        println!(
            "{}{}",
            preset_line(preset),
            if active == Some(preset.name.as_str()) {
                "  (active)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn preset_line(preset: &audivane_protocol::ipc::PresetInfo) -> String {
    format!(
        "{:<32} {:<10} {}",
        truncate(&preset.name, 32),
        preset_summary(preset),
        date(preset.modified_unix)
    )
}

fn preset_summary(preset: &audivane_protocol::ipc::PresetInfo) -> String {
    let mut parts = Vec::new();
    if preset.bands > 0 {
        parts.push(format!(
            "{} band{}",
            preset.bands,
            if preset.bands == 1 { "" } else { "s" }
        ));
    }
    if preset.convolution > 0 {
        parts.push(format!(
            "{} IR{}",
            preset.convolution,
            if preset.convolution == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        "flat".to_string()
    } else {
        parts.join(", ")
    }
}

/// Unix seconds → `YYYY-MM-DD HH:MM` (UTC), without a date library.
fn date(unix: u64) -> String {
    let secs = unix as i64;
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        time / 3600,
        (time % 3600) / 60
    )
}

fn print_devices(devices: &[audivane_protocol::ipc::DeviceInfo]) -> CtlResult<()> {
    if devices.is_empty() {
        println!("no ALSA playback devices (see /proc/asound/pcm)");
        return Ok(());
    }
    println!(
        "{:<28} {:<10} {:<28} {:<9} ACTIVE",
        "DEVICE", "CARD", "NAME", "KIND"
    );
    for device in devices {
        println!(
            "{:<28} {:<10} {:<28} {:<9} {}",
            device.id,
            truncate(&device.card, 10),
            truncate(&device.name, 28),
            if device.hdmi {
                "hdmi"
            } else if device.loopback {
                "loopback"
            } else {
                "analog"
            },
            if device.active { "yes" } else { "" }
        );
    }
    Ok(())
}

fn config_line(config: &audivane_protocol::ipc::EngineConfig) -> String {
    let mut text = format!(
        "config: device {}, dsp {}",
        config.device,
        if config.dsp_enabled { "on" } else { "off" }
    );
    text.push_str(&format!(
        ", {}",
        describe_upsampler(&config.chain.upsampler)
    ));
    match &config.active_preset {
        Some(name) => text.push_str(&format!(", preset {name:?}")),
        None => text.push_str(", no preset"),
    }
    text
}

fn eq_section_bands_mut(
    profile: &mut EqProfile,
    section: EqSectionArg,
    create_if_missing: bool,
) -> CtlResult<&mut Vec<audivane_protocol::Band>> {
    match section {
        EqSectionArg::Linked => Ok(&mut profile.bands),
        EqSectionArg::Left => {
            if create_if_missing {
                Ok(&mut profile.left.get_or_insert_with(ChannelEq::default).bands)
            } else {
                profile
                    .left
                    .as_mut()
                    .map(|c| &mut c.bands)
                    .ok_or_else(|| anyhow::anyhow!("L section is not present").into())
            }
        }
        EqSectionArg::Right => {
            if create_if_missing {
                Ok(&mut profile.right.get_or_insert_with(ChannelEq::default).bands)
            } else {
                profile
                    .right
                    .as_mut()
                    .map(|c| &mut c.bands)
                    .ok_or_else(|| anyhow::anyhow!("R section is not present").into())
            }
        }
    }
}

fn band_count(count: usize) -> String {
    format!("{count} band{}", if count == 1 { "" } else { "s" })
}

fn summarize_harmonics(h: Option<&HarmonicsConfig>) -> String {
    let Some(h) = h else {
        return "off".to_string();
    };
    if !h.enabled {
        return "off".to_string();
    }
    let (h2, h3, h4, h5) = h.effective_levels_db();
    let label = match h.character {
        HarmonicsCharacter::Triode => "triode",
        HarmonicsCharacter::Pentode => "pentode",
        HarmonicsCharacter::Custom => "custom",
    };
    let mut extras = Vec::new();
    if let Some(v) = h4 {
        extras.push(format!("H4 {v:.1}"));
    }
    if let Some(v) = h5 {
        extras.push(format!("H5 {v:.1}"));
    }
    if !h.dc_block {
        extras.push("no dc-block".to_string());
    }
    // The level change hardly depends on the rate (only via the DC blocker).
    if let Ok(stage) = audivane_dsp_core::harmonics::Harmonics::try_new(h, 192_000.0, 1) {
        extras.push(format!(
            "level {:+.2} dB at 0 dBFS, {:+.2} dB small-signal",
            stage.peak_trim_db(),
            stage.small_signal_gain_db()
        ));
    }
    if extras.is_empty() {
        format!("{label} (H2 {h2:.1} dB, H3 {h3:.1} dB)")
    } else {
        format!(
            "{label} (H2 {h2:.1} dB, H3 {h3:.1} dB, {})",
            extras.join(", ")
        )
    }
}

fn summarize_dc_block(dc: Option<&DcBlockConfig>) -> String {
    let Some(dc) = dc else {
        return "off".to_string();
    };
    if dc.enabled {
        format!("on ({} Hz)", dc.cutoff_hz)
    } else {
        format!("off ({} Hz)", dc.cutoff_hz)
    }
}

/// Where the crossfeed gain is evaluated: the active graph rate when the
/// engine reports one, else the configured default rate (an estimate, so the
/// summary says which rate it assumes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum XfRate {
    Active(u32),
    Assumed(u32),
}

impl XfRate {
    fn new(graph_rate: Option<u32>, default_rate: u32) -> Self {
        match graph_rate.filter(|&r| r > 0) {
            Some(r) => Self::Active(r),
            None => Self::Assumed(default_rate),
        }
    }

    /// Ask the engine for its graph rate (an error leaves the estimate).
    fn query(client: &mut Client, default_rate: u32) -> Self {
        let graph = client
            .call(Call::Status)
            .ok()
            .and_then(|v| serde_json::from_value::<audivane_protocol::ipc::Status>(v).ok())
            .map(|s| s.graph_rate);
        Self::new(graph, default_rate)
    }
}

fn summarize_crossfeed(cf: Option<&CrossfeedConfig>, rate: XfRate) -> String {
    let Some(cf) = cf else {
        return "off".to_string();
    };
    if !cf.enabled {
        return "off".to_string();
    }
    let (fc, feed) = cf.effective_params();
    // The stage's own level, so the user sees where it went (it varies a
    // little with the rate).
    let policy = match cf.level {
        CrossfeedLevel::Matched => "matched",
        CrossfeedLevel::PeakSafe => "peak-safe",
    };
    let (hz, assumed) = match rate {
        XfRate::Active(r) => (r, String::new()),
        XfRate::Assumed(r) => (r, format!(", assuming {r} Hz")),
    };
    let level = audivane_dsp_core::crossfeed::Crossfeed::try_new(cf, f64::from(hz), 2)
        .map(|stage| format!(", level {:+.2} dB {policy}{assumed}", stage.level_db()))
        .unwrap_or_default();
    match cf.mode {
        CrossfeedMode::Bs2bDefault => format!("bs2b default ({fc:.0} Hz, {feed:.1} dB{level})"),
        CrossfeedMode::Bs2bChuMoy => format!("bs2b Chu Moy ({fc:.0} Hz, {feed:.1} dB{level})"),
        CrossfeedMode::Bs2bJanMeier => {
            format!("bs2b Jan Meier ({fc:.0} Hz, {feed:.1} dB{level})")
        }
        CrossfeedMode::Bs2bCustom => format!("bs2b custom ({fc:.0} Hz, {feed:.1} dB{level})"),
        CrossfeedMode::Linkwitz => format!("linkwitz ({fc:.1} Hz, {feed:.1} dB{level})"),
    }
}

fn summarize_eq(eq: Option<&EqProfile>) -> String {
    let Some(eq) = eq else {
        return "off".to_string();
    };
    let name = if eq.name.is_empty() {
        "unnamed"
    } else {
        eq.name.as_str()
    };
    let mut parts = Vec::new();
    if eq.linked_enabled {
        parts.push(format!("L+R: {}", band_count(eq.bands.len())));
    } else {
        parts.push("L+R: off".to_string());
    }
    if let Some(left) = &eq.left {
        if left.enabled {
            parts.push(format!("L: {}", band_count(left.bands.len())));
        } else {
            parts.push("L: off".to_string());
        }
    }
    if let Some(right) = &eq.right {
        if right.enabled {
            parts.push(format!("R: {}", band_count(right.bands.len())));
        } else {
            parts.push("R: off".to_string());
        }
    }
    let preamp = if eq.preamp_db == 0.0 {
        0.0
    } else {
        eq.preamp_db
    };
    parts.push(format!("preamp {preamp:.1} dB"));
    if let Some(tilt) = &eq.tilt
        && tilt.is_active()
    {
        parts.push(format!("tilt {}", format_tilt_short(tilt)));
    }
    if let Some(high) = &eq.tilt_high
        && high.is_active()
    {
        parts.push(format!("treble tilt {}", format_tilt_short(high)));
    }
    format!("{name} ({})", parts.join(", "))
}

fn parse_tilt_pivot(s: &str) -> std::result::Result<TiltPivot, CtlError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "start" => Ok(TiltPivot::Start),
        "end" => Ok(TiltPivot::End),
        "center" | "centre" => Ok(TiltPivot::Center),
        other => {
            let hz: f64 = other.parse().map_err(|_| {
                CtlError::Other(anyhow::anyhow!(
                    "bad --pivot {s:?}: expected start, end, center, or Hz"
                ))
            })?;
            if !hz.is_finite() || hz <= 0.0 {
                bail!("--pivot frequency must be positive (got {hz})");
            }
            Ok(TiltPivot::Hz(hz))
        }
    }
}

fn format_tilt_pivot(p: TiltPivot) -> String {
    match p {
        TiltPivot::Start => "start".into(),
        TiltPivot::End => "end".into(),
        TiltPivot::Center => "center".into(),
        TiltPivot::Hz(hz) => format!("{hz:.1} Hz"),
    }
}

fn format_tilt_short(t: &TiltConfig) -> String {
    if let Some(s) = t.slope_db_per_oct {
        format!("{s:+.2} dB/oct")
    } else if let Some(tot) = t.total_db {
        format!("{tot:+.2} dB")
    } else {
        "invalid".into()
    }
}

fn format_tilt(t: &TiltConfig) -> String {
    let state = if t.enabled { "on" } else { "off" };
    let amount = if let Some(s) = t.slope_db_per_oct {
        format!("{s:+.2} dB/oct")
    } else if let Some(tot) = t.total_db {
        format!("{tot:+.2} dB total")
    } else {
        "invalid".into()
    };
    format!(
        "{state}, {amount}, {:.1}–{:.1} Hz, pivot {}",
        t.start_hz,
        t.end_hz,
        format_tilt_pivot(t.pivot)
    )
}

fn summarize_conv(convs: &[ConvolutionConfig]) -> String {
    if convs.is_empty() {
        return "off".to_string();
    }
    let enabled = convs.iter().filter(|c| c.enabled).count();
    let names: Vec<String> = convs
        .iter()
        .map(|c| {
            let label = c.name.as_deref().map(str::to_string).unwrap_or_else(|| {
                c.ir_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string()
            });
            if c.enabled {
                label
            } else {
                format!("{label} (off)")
            }
        })
        .collect();
    format!("{enabled}/{} enabled [{}]", convs.len(), names.join(", "))
}

fn print_band_list(bands: &[audivane_protocol::Band]) {
    for (i, b) in bands.iter().enumerate() {
        println!(
            "    [{i}] {} {:?} {:.1} Hz {:+.2} dB Q {:.3}",
            if b.enabled { "on " } else { "off" },
            b.kind,
            b.freq_hz,
            b.gain_db,
            b.q
        );
    }
}

fn print_eq(eq: Option<&EqProfile>) {
    let Some(eq) = eq else {
        println!("eq: off");
        return;
    };
    let name = if eq.name.is_empty() {
        "(unnamed)"
    } else {
        eq.name.as_str()
    };
    let preamp = if eq.preamp_db == 0.0 {
        0.0
    } else {
        eq.preamp_db
    };
    println!("eq: {name} (preamp {preamp:+.2} dB)");
    println!(
        "  L+R: {} ({})",
        if eq.linked_enabled { "on" } else { "off" },
        band_count(eq.bands.len())
    );
    print_band_list(&eq.bands);
    if let Some(left) = &eq.left {
        let l_pre = if left.preamp_db == 0.0 {
            0.0
        } else {
            left.preamp_db
        };
        println!(
            "  L:   {} (preamp {l_pre:+.2} dB, {})",
            if left.enabled { "on" } else { "off" },
            band_count(left.bands.len())
        );
        print_band_list(&left.bands);
    }
    if let Some(right) = &eq.right {
        let r_pre = if right.preamp_db == 0.0 {
            0.0
        } else {
            right.preamp_db
        };
        println!(
            "  R:   {} (preamp {r_pre:+.2} dB, {})",
            if right.enabled { "on" } else { "off" },
            band_count(right.bands.len())
        );
        print_band_list(&right.bands);
    }
    if let Some(tilt) = &eq.tilt {
        println!("  tilt: {}", format_tilt(tilt));
    }
    if let Some(high) = &eq.tilt_high {
        println!("  treble tilt: {}", format_tilt(high));
    }
}

fn print_conv_list(convs: &[ConvolutionConfig]) {
    if convs.is_empty() {
        println!("convolution: off");
        return;
    }
    for (i, c) in convs.iter().enumerate() {
        let state = if c.enabled { "on " } else { "off" };
        let label = match &c.name {
            Some(n) => format!("{n} ({})", c.ir_path.display()),
            None => c.ir_path.display().to_string(),
        };
        println!("[{i}] {state}  {label}  gain {:+.2} dB", c.gain_db);
    }
}

fn find_conv_index(convs: &[ConvolutionConfig], target: &str) -> CtlResult<usize> {
    if let Ok(idx) = target.parse::<usize>() {
        if idx < convs.len() {
            return Ok(idx);
        }
        bail!(
            "convolution index {idx} out of range (have {} IRs)",
            convs.len()
        );
    }
    if let Some(idx) = convs
        .iter()
        .position(|c| c.name.as_deref() == Some(target) || c.ir_path.to_str() == Some(target))
    {
        return Ok(idx);
    }
    bail!("no convolution IR matches {target:?}");
}

fn print_streams(streams: &[audivane_protocol::ipc::StreamInfo]) -> CtlResult<()> {
    if streams.is_empty() {
        println!("no streams");
        return Ok(());
    }
    println!(
        "{:>6}  {:<20} {:<28} {:>8} {:>3} {:<7} {:<8} {:>8}  {:>5}",
        "NODE", "APP", "STREAM", "RATE", "CH", "FORMAT", "SINK", "VOL", "MUTE"
    );
    for stream in streams {
        println!("{}", stream_row(stream));
    }
    Ok(())
}

fn stream_row(stream: &audivane_protocol::ipc::StreamInfo) -> String {
    format!(
        "{:>6}  {:<20} {:<28} {:>8} {:>3} {:<7} {:<8} {:>8}  {:>5}",
        stream.node,
        truncate(stream.app_name.as_deref().unwrap_or("-"), 20),
        truncate(stream.media_name.as_deref().unwrap_or("-"), 28),
        stream
            .rate
            .map(|r| r.to_string())
            .unwrap_or_else(|| "-".into()),
        stream
            .channels
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into()),
        truncate(stream.format.as_deref().unwrap_or("-"), 7),
        stream.sink.map(sink_name).unwrap_or("-"),
        match (stream.volume_db, stream.volume) {
            (Some(db), _) => format!("{db:.1}dB"),
            (None, Some(v)) => format!("{v:.2}"),
            (None, None) => "-".into(),
        },
        if stream.muted { "yes" } else { "no" },
    )
}

fn stream_volume_line(
    volume: &audivane_protocol::ipc::StreamVolume,
    requested_db: Option<f64>,
) -> String {
    use audivane_protocol::ipc::StreamVolumeLimit;
    let mut line = format!(
        "stream {}: {:.2} dB{}",
        volume.node,
        volume.volume_db,
        if volume.muted { ", muted" } else { "" }
    );
    match (volume.limited_by, requested_db) {
        (Some(StreamVolumeLimit::StreamMax), Some(req)) => line.push_str(&format!(
            " (asked {req:.2} dB; the stream maximum is {:.2} dB, see `volume --stream-max`)",
            volume.max_db
        )),
        (Some(StreamVolumeLimit::AppVolume), Some(req)) => line.push_str(&format!(
            " (asked {req:.2} dB; the app's own volume allows at most {:.2} dB, raise it in the app)",
            volume.max_db
        )),
        _ => {}
    }
    line
}

fn stream_line(stream: &audivane_protocol::ipc::StreamInfo) -> String {
    format!(
        "#{} {} ({} Hz, {} ch, {}) -> {}",
        stream.node,
        stream.app_name.as_deref().unwrap_or("-"),
        stream.rate.map(|r| r.to_string()).unwrap_or_default(),
        stream.channels.map(|c| c.to_string()).unwrap_or_default(),
        stream.format.as_deref().unwrap_or("?"),
        stream.sink.map(sink_name).unwrap_or("not routed")
    )
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The state as `status` prints it: "waiting for device" while the
/// configured device is not there.
fn status_state(status: &audivane_protocol::ipc::Status) -> &'static str {
    if status.waiting_for_device {
        "waiting for device"
    } else {
        state_name(status.state)
    }
}

fn state_name(state: audivane_protocol::ipc::EngineState) -> &'static str {
    use audivane_protocol::ipc::EngineState::*;
    match state {
        Idle => "idle",
        Ready => "ready",
        Playing => "playing",
        Switching => "switching",
        Error => "error",
    }
}

fn sink_name(sink: SinkChoice) -> &'static str {
    match sink {
        SinkChoice::Dsp => "dsp",
        SinkChoice::Direct => "direct",
        SinkChoice::Default => "default",
    }
}

fn side_name(side: AbSide) -> &'static str {
    match side {
        AbSide::A => "A",
        AbSide::B => "B",
    }
}

fn parse_level(text: &str) -> Result<LevelMatch> {
    match text.trim() {
        "analytic" | "auto" | "k" => Ok(LevelMatch::default()),
        "analytic-a" | "a" => Ok(LevelMatch::Analytic {
            weighting: LevelWeighting::A,
        }),
        "off" | "none" => Ok(LevelMatch::Off),
        other => {
            let offset_db: f64 = other.parse().with_context(|| {
                format!("--level must be analytic, analytic-a, off or dB, got {other:?}")
            })?;
            Ok(LevelMatch::Manual { offset_db })
        }
    }
}

/// The merge patch and label for `ab output`: B's own bits and dither.
fn output_patch(bits: Option<u32>, dither: Option<&str>) -> anyhow::Result<(Value, String)> {
    if bits.is_none() && dither.is_none() {
        anyhow::bail!("give --bits, --dither or both");
    }
    let mut output = serde_json::Map::new();
    let mut label = Vec::new();
    if let Some(bits) = bits {
        output.insert("bits".into(), bits.into());
        label.push(format!("{bits}-bit"));
    }
    if let Some(dither) = dither {
        let kind: audivane_protocol::DitherKind =
            serde_json::from_value(Value::String(dither.into()))
                .map_err(|_| anyhow::anyhow!("unknown dither `{dither}`"))?;
        output.insert("dither".into(), serde_json::to_value(kind)?);
        label.push(format!("{dither} dither"));
    }
    Ok((
        serde_json::json!({ "output": output }),
        format!("Output: {}", label.join(", ")),
    ))
}

/// `set_ab` with a patch B, printing the daemon's notes.
fn set_ab_patch(
    client: &mut Client,
    patch: Value,
    label: String,
    level: LevelMatch,
) -> CtlResult<()> {
    let ab = AbConfig {
        label: Some(label.clone()),
        level,
        ..AbConfig::new(AbB::Patch(patch))
    };
    let value = client.call(Call::SetAb(Some(ab)))?;
    for note in reply_notes(&value) {
        println!("note: {note}");
    }
    let status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
    println!(
        "A/B enabled: side {}, B = {label} ({})",
        side_name(status.current),
        ab_levels(&status)
    );
    Ok(())
}

/// The merge patch and label for `ab stage`: the stage off when it is
/// active in chain A, on when A has it set up but switched off.
fn stage_patch(a: &ChainConfig, stage: AbStage) -> std::result::Result<(Value, String), String> {
    use serde_json::json;
    let toggle = |key: &str, name: &str, active: bool, can_enable: bool| {
        if active {
            Ok((json!({ key: {"enabled": false} }), format!("{name} off")))
        } else if can_enable {
            Ok((json!({ key: {"enabled": true} }), format!("{name} on")))
        } else {
            Err(format!(
                "{name} has no settings in chain A to compare; set it up first"
            ))
        }
    };
    match stage {
        AbStage::Crossfeed => {
            let active = a.crossfeed.as_ref().is_some_and(CrossfeedConfig::is_active);
            let cf = a.crossfeed.clone().unwrap_or_default();
            toggle(
                "crossfeed",
                "Crossfeed",
                active,
                CrossfeedConfig {
                    enabled: true,
                    ..cf
                }
                .is_active(),
            )
        }
        AbStage::Dc => {
            let active = a.dc_block.as_ref().is_some_and(DcBlockConfig::is_active);
            let dc = a.dc_block.unwrap_or_default();
            toggle(
                "dc_block",
                "DC block",
                active,
                DcBlockConfig {
                    enabled: true,
                    ..dc
                }
                .is_active(),
            )
        }
        AbStage::Harmonics => {
            let active = a.harmonics.as_ref().is_some_and(HarmonicsConfig::is_active);
            let h = a.harmonics.clone().unwrap_or_default();
            toggle(
                "harmonics",
                "Harmonics",
                active,
                HarmonicsConfig { enabled: true, ..h }.is_active(),
            )
        }
        AbStage::Pitch => toggle(
            "pitch",
            "Pitch",
            a.pitch.is_active(),
            PitchConfig {
                enabled: true,
                ..a.pitch.clone()
            }
            .is_active(),
        ),
        AbStage::Tilt => {
            // Both sections of a two-section tilt switch together.
            let sections: Vec<(&str, TiltConfig)> = a.eq.as_ref().map_or_else(Vec::new, |eq| {
                [("tilt", eq.tilt), ("tilt_high", eq.tilt_high)]
                    .into_iter()
                    .filter_map(|(k, t)| Some((k, t?)))
                    .collect()
            });
            let active = sections.iter().any(|(_, t)| t.is_active());
            let can = sections.iter().any(|(_, t)| {
                TiltConfig {
                    enabled: true,
                    ..*t
                }
                .is_active()
            });
            let patch = |on: bool| {
                let eq: serde_json::Map<String, serde_json::Value> = sections
                    .iter()
                    .map(|(k, _)| ((*k).to_string(), json!({"enabled": on})))
                    .collect();
                json!({ "eq": eq })
            };
            if active {
                // Off removes both sections from B, whatever shape A takes
                // later (an `enabled: false` merged onto a section A drops
                // would leave B a section without a slope).
                Ok((
                    json!({"eq": {"tilt": null, "tilt_high": null}}),
                    "Tilt off".into(),
                ))
            } else if can {
                Ok((patch(true), "Tilt on".into()))
            } else {
                Err("Tilt has no settings in chain A to compare; set it up first".into())
            }
        }
        AbStage::Eq => {
            // Bands and preamp off; the tilt (its own stage) stays.
            let active = a.eq.as_ref().is_some_and(|eq| {
                eq.preamp_db != 0.0
                    || (eq.linked_enabled && eq.bands.iter().any(|b| b.enabled))
                    || eq.left.as_ref().is_some_and(ChannelEq::is_active)
                    || eq.right.as_ref().is_some_and(ChannelEq::is_active)
            });
            if !active {
                return Err("EQ is off in chain A; nothing to compare".into());
            }
            Ok((
                json!({"eq": {"preamp_db": 0.0, "bands": [], "left": null, "right": null}}),
                "EQ off".into(),
            ))
        }
        AbStage::Conv => {
            if !a.convolution.iter().any(|c| c.enabled) {
                return Err("convolution is off in chain A; nothing to compare".into());
            }
            Ok((json!({"convolution": []}), "Convolution off".into()))
        }
    }
}

fn print_ab(value: Value) -> CtlResult<()> {
    for note in reply_notes(&value) {
        println!("note: {note}");
    }
    let status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
    println!(
        "A/B side {} ({})",
        side_name(status.current),
        ab_levels(&status)
    );
    Ok(())
}

/// The running measurement, or the level in force.
fn describe_measure(status: &audivane_protocol::ipc::AbStatus) -> String {
    if let Some(m) = &status.measuring {
        let offset = m.offset_db.map_or_else(
            || "waiting for music".to_string(),
            |o| format!("B {o:+.2} dB"),
        );
        return format!(
            "measuring {:.1} s of {:.0} · {offset} · {}",
            m.seconds,
            m.max_seconds,
            if m.stable { "stable" } else { "settling" }
        );
    }
    if let Some(error) = &status.measure_error {
        return format!("not locked: {error}");
    }
    match status.level {
        LevelMatch::Measured { offset_db, seconds } => {
            format!("locked: B {offset_db:+.2} dB (measured over {seconds:.1} s)")
        }
        _ => format!("not measuring ({})", ab_levels(status)),
    }
}

/// "B offset … · A … · B … · headroom … · overs A … B …".
fn ab_levels(status: &audivane_protocol::ipc::AbStatus) -> String {
    format!(
        "B offset {:+.2} dB · A {:+.2} dB · B {:+.2} dB · headroom {:.2} dB · overs A {:.0} B {:.0}",
        status.offset_db,
        status.gain_a_db,
        status.gain_b_db,
        status.headroom_db,
        status.overs.a,
        status.overs.b
    )
}

/// Optional `"notes"` array some replies carry (e.g. `ab set` when chain B was
/// realigned with chain A).
fn reply_notes(value: &Value) -> Vec<String> {
    value
        .get("notes")
        .and_then(Value::as_array)
        .map(|notes| {
            notes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn describe_pitch(p: &PitchConfig) -> String {
    if !p.is_active() {
        return "pitch off".to_string();
    }
    let quality = match p.quality {
        PitchQuality::Low => "low",
        PitchQuality::High => "high",
        PitchQuality::Max => "max",
    };
    let cents = p.effective_cents();
    if let (Some(r), Some(t)) = (p.reference_hz, p.target_hz) {
        format!("pitch {cents:+.2} cents ({r} -> {t} Hz, {quality})")
    } else if let Some(st) = p.semitones {
        format!("pitch {cents:+.2} cents ({st:+} st, {quality})")
    } else {
        format!("pitch {cents:+.2} cents ({quality})")
    }
}

fn describe_upsampler(up: &UpsamplerConfig) -> String {
    let target = match up.target {
        RateTarget::Off => "upsampling off".to_string(),
        RateTarget::Ratio(ratio) => format!("upsampling x{ratio}"),
        RateTarget::MaxRate(max) => format!("upsampling up to {max} Hz"),
        RateTarget::MaxRateAny(max) => format!("resampling up to {max} Hz (any rate family)"),
        RateTarget::Rate(rate) => format!("resampling to {rate} Hz"),
    };
    if let Some(preset) = &up.preset {
        return format!("{target} (preset {preset})");
    }
    let Some(stored) = &up.design else {
        return format!("{target} (preset recommended)");
    };
    if let audivane_protocol::FilterDesign::Kernel(k) = &stored.design
        && k.numerics.path == audivane_protocol::design::ArithmeticPath::Standard
        && let Ok(m) = audivane_dsp_core::design::paths::standard::legacy_config(k, 44_100)
    {
        let phase = match m.phase {
            FilterPhase::Intermediate => format!("intermediate phase (mix {})", m.phase_mix),
            other => format!("{other:?} phase"),
        };
        let mut extras = String::new();
        if m.preserve_samples {
            extras.push_str(", keeps the input samples");
        }
        match m.window {
            WindowKind::Kaiser => {}
            WindowKind::Gaussian => extras.push_str(", Gaussian window"),
            WindowKind::Tukey => extras.push_str(&format!(", Tukey window (taper {})", m.taper)),
        }
        let taps = match m.taps_unit {
            TapsUnit::Output => format!("{} taps", m.taps),
            TapsUnit::Input => format!("{} taps per input sample", m.taps),
        };
        if let Some(budget) = m.auto_taps {
            let budget = match budget {
                audivane_protocol::FilterErrorBudget::Db(a) => format!("{a} dB"),
                audivane_protocol::FilterErrorBudget::MatchOutput => "matches output".into(),
            };
            return format!(
                "{target} (automatic taps: {budget}, passband to {}, stopband from {}, {phase}{extras})",
                m.band.passband_end, m.band.stopband_start
            );
        }
        return match m.cutoff_mode {
            CutoffMode::Fraction => format!(
                "{target} ({taps}, {phase}, beta {}, cutoff {}{extras})",
                m.kaiser_beta, m.cutoff
            ),
            CutoffMode::NyquistStop => format!(
                "{target} ({taps}, {phase}, beta {}, stop at Nyquist{extras})",
                m.kaiser_beta
            ),
            CutoffMode::Band => format!(
                "{target} (band: passband to {}, stopband from {}, {} dB, {phase}{extras})",
                m.band.passband_end, m.band.stopband_start, m.band.attenuation_db
            ),
        };
    }
    let based = match &stored.based_on {
        Some(name) => format!(", based on {name}"),
        None => String::new(),
    };
    format!("{target} (custom {}{based})", stored.design.kind_name())
}

fn passthrough_reasons(passthrough: &audivane_protocol::ipc::Passthrough) -> String {
    use audivane_protocol::ipc::PassthroughBreak::*;
    passthrough
        .broken_because
        .iter()
        .map(|reason| match reason {
            DspActive => "DSP is on".to_string(),
            Upsampling { ratio } => format!("upsampling x{ratio}"),
            RateConversion {
                graph_rate,
                output_rate,
            } => format!("resampling {graph_rate} -> {output_rate} Hz"),
            Varispeed { speed } => format!("varispeed {}x", format_speed(*speed)),
            Volume { node, volume } if *volume == 0.0 => format!("node {node} is muted"),
            Volume { node, volume } => format!(
                "volume of node {node} is {}%; set it to 100% for bit-perfect playback",
                slider_percent(*volume)
            ),
            Mixing { streams } => format!("{streams} streams mixed"),
            Resampled {
                stream_rate,
                graph_rate,
            } => format!("resampled {stream_rate} -> {graph_rate} Hz"),
            ChannelMix {
                stream_channels,
                sink_channels,
            } => format!("channel mix {stream_channels} -> {sink_channels}"),
            SourceOver24Bit => "source has more than 24 bits".to_string(),
            Truncated {
                source_bits,
                output_bits,
            } => format!("{source_bits}-bit source truncated to {output_bits}-bit output"),
            NotPlaying => "nothing is playing".to_string(),
            NoOutput => "the output is not playing: nothing reaches the DAC".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A linear PipeWire volume as desktop sliders (pavucontrol, KDE, GNOME)
/// show it: they use a cubic scale, so linear 0.27 reads 65%.
/// ", DAC converts 24 of 32 bits" when the device resolves fewer bits than
/// the format carries, ", all 32 bits used" when it uses them all.
fn used_bits_note(status: &audivane_protocol::ipc::Status, format: SampleFormat) -> String {
    let used = status.device_caps.as_ref().and_then(|caps| {
        caps.rates
            .iter()
            .find(|r| r.rate == status.output_rate)?
            .formats
            .iter()
            .find(|f| f.format == format)?
            .sbits
    });
    match used {
        Some(bits) if bits < format.bits() => {
            format!(" (DAC converts {bits} of {} bits)", format.bits())
        }
        Some(bits) => format!(" (all {bits} bits used)"),
        None => String::new(),
    }
}

fn format_speed(speed: f64) -> String {
    let s = format!("{speed:.4}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

fn slider_percent(volume: f64) -> u32 {
    (volume.max(0.0).cbrt() * 100.0).round() as u32
}

/// One line for the passthrough verdict, including anything we could not
/// verify (a caveat never changes `intact`). "Not bit-perfect" is not an
/// error: DSP and upsampling change the samples on purpose.
fn passthrough_line(passthrough: &audivane_protocol::ipc::Passthrough) -> String {
    use audivane_protocol::ipc::PassthroughBreak;
    let only_idle = passthrough
        .broken_because
        .iter()
        .all(|b| matches!(b, PassthroughBreak::NotPlaying));
    let mut text = if passthrough.intact {
        "passthrough: bit-perfect".to_string()
    } else if only_idle {
        "passthrough: nothing is playing".to_string()
    } else {
        format!(
            "passthrough: not bit-perfect ({})",
            passthrough_reasons(passthrough)
        )
    };
    for caveat in &passthrough.caveats {
        text.push_str(" (");
        text.push_str(&passthrough_caveat(caveat));
        text.push(')');
    }
    text
}

fn passthrough_caveat(caveat: &audivane_protocol::ipc::PassthroughCaveat) -> String {
    use audivane_protocol::ipc::PassthroughCaveat::*;
    match caveat {
        UnknownSourcePrecision { format } => {
            format!("source precision unknown: {format} container")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::ipc::{Passthrough, PassthroughBreak, PassthroughCaveat};

    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(subcommand)]
        command: CtlCommand,
    }

    fn parse_stream_volume(args: &[&str]) -> Result<StreamVolumeArgs, clap::Error> {
        use clap::Parser;
        let argv = ["avdsp", "stream", "volume"].iter().chain(args);
        match TestCli::try_parse_from(argv)?.command {
            CtlCommand::Stream {
                command: StreamCommand::Volume(args),
            } => Ok(args),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stream_volume_takes_negative_levels_without_a_separator() {
        let args = parse_stream_volume(&["42", "-12.5"]).unwrap();
        assert_eq!((args.node, args.db, args.muted()), (42, Some(-12.5), None));
        let args = parse_stream_volume(&["42"]).unwrap();
        assert_eq!((args.db, args.muted()), (None, None));
        let args = parse_stream_volume(&["42", "--mute"]).unwrap();
        assert_eq!(args.muted(), Some(true));
        let args = parse_stream_volume(&["42", "-3", "--unmute"]).unwrap();
        assert_eq!((args.db, args.muted()), (Some(-3.0), Some(false)));
        assert!(parse_stream_volume(&["42", "--mute", "--unmute"]).is_err());
        assert!(parse_stream_volume(&[]).is_err());
    }

    #[test]
    fn stream_volume_line_says_what_limited_the_level() {
        use audivane_protocol::ipc::{StreamVolume, StreamVolumeLimit};
        let mut volume = StreamVolume {
            node: 42,
            volume_db: -12.0,
            muted: false,
            max_db: -12.0,
            limited_by: None,
        };
        assert_eq!(stream_volume_line(&volume, None), "stream 42: -12.00 dB");
        volume.limited_by = Some(StreamVolumeLimit::StreamMax);
        let line = stream_volume_line(&volume, Some(-3.0));
        assert!(line.contains("stream maximum is -12.00 dB"), "{line}");
        volume.limited_by = Some(StreamVolumeLimit::AppVolume);
        volume.muted = true;
        let line = stream_volume_line(&volume, Some(0.0));
        assert!(line.starts_with("stream 42: -12.00 dB, muted"), "{line}");
        assert!(line.contains("raise it in the app"), "{line}");
    }

    #[test]
    fn hardware_db_above_0_db_is_refused() {
        assert!(parse_hardware_db("0.5").is_err());
        assert!(parse_hardware_db("24").is_err());
        assert_eq!(parse_hardware_db("0").unwrap().0, Some(0.0));
        assert_eq!(parse_hardware_db("-20").unwrap().0, Some(-20.0));
        assert_eq!(parse_hardware_db("none").unwrap().0, None);
    }

    #[test]
    fn status_line_prints_rational_ratios() {
        let status: audivane_protocol::ipc::Status = serde_json::from_str(
            r#"{
                "version":"0.1.0", "state":"playing", "device":"hw:0,0", "error":null,
                "format":null, "graph_rate":44100, "output_rate":48000, "ratio":0,
                "rate_ratio":{"up":160,"down":147}, "quantum":1024, "latency_ms":1.0,
                "xruns":0, "dsp_load":0.0, "dsp_enabled":true, "active_sink":null,
                "passthrough":{"intact":false,"broken_because":[]}, "ab":null
            }"#,
        )
        .unwrap();
        let line = status_line(&status);
        assert!(line.contains("44100 Hz -> 48000 Hz (160/147)"), "{line}");
        assert!(!line.contains("x0"), "{line}");
    }

    #[test]
    fn engine_version_reads_status_and_skips_events() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("avdsp-ctl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "status");
            let mut out = stream;
            writeln!(out, r#"{{"event":"meters","data":{{}}}}"#).unwrap();
            writeln!(
                out,
                r#"{{"id":{},"ok":{{"version":"9.8.7"}}}}"#,
                request["id"]
            )
            .unwrap();
        });
        assert_eq!(
            engine_version_at(&path, Duration::from_secs(5)).as_deref(),
            Some("9.8.7")
        );
        server.join().unwrap();
        assert_eq!(
            engine_version_at(&dir.join("none.sock"), Duration::from_millis(100)),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn passthrough_line_keeps_caveats_out_of_the_breaks() {
        let caveat = PassthroughCaveat::UnknownSourcePrecision {
            format: "S32LE".into(),
        };
        let intact = Passthrough {
            intact: true,
            broken_because: Vec::new(),
            caveats: vec![caveat.clone()],
        };
        assert_eq!(
            passthrough_line(&intact),
            "passthrough: bit-perfect (source precision unknown: S32LE container)"
        );

        let broken = Passthrough {
            intact: false,
            broken_because: vec![PassthroughBreak::DspActive],
            caveats: vec![caveat],
        };
        assert_eq!(
            passthrough_line(&broken),
            "passthrough: not bit-perfect (DSP is on) (source precision unknown: S32LE container)"
        );

        let volume = Passthrough {
            intact: false,
            broken_because: vec![PassthroughBreak::Volume {
                node: 62,
                volume: 0.27,
            }],
            caveats: Vec::new(),
        };
        assert_eq!(
            passthrough_line(&volume),
            "passthrough: not bit-perfect (volume of node 62 is 65%; set it to 100% for \
             bit-perfect playback)"
        );

        let idle = Passthrough {
            intact: false,
            broken_because: vec![PassthroughBreak::NotPlaying],
            caveats: Vec::new(),
        };
        assert_eq!(passthrough_line(&idle), "passthrough: nothing is playing");

        let plain = Passthrough {
            intact: true,
            broken_because: Vec::new(),
            caveats: Vec::new(),
        };
        assert_eq!(passthrough_line(&plain), "passthrough: bit-perfect");
    }

    #[test]
    fn human_events_render_text() {
        let meters = serde_json::json!({
            "event": "meters",
            "data": {
                "peak_db": [-3.0, -4.0],
                "rms_db": [-9.0, -10.0],
                "clipped": 0,
                "xruns": 0,
                "dsp_load": 0.1,
            },
        });
        let (text, in_place) = human_event(&meters).expect("meters render");
        assert!(in_place, "meters may update in place");
        assert!(text.contains("peak [  -3.0   -4.0]"), "{text}");
        assert!(text.contains("load  10.0%"), "{text}");
        assert!(!text.contains('{'), "{text}");

        let streams = serde_json::json!({"event": "streams", "data": []});
        let (text, in_place) = human_event(&streams).expect("streams render");
        assert!(!in_place);
        assert_eq!(text, "0 stream(s):");

        let now = serde_json::json!({"event": "now_playing", "data": null});
        let (text, in_place) = human_event(&now).expect("now_playing renders");
        assert!(!in_place);
        assert_eq!(text, "now playing: nothing");

        assert!(human_event(&serde_json::json!({"event": "other"})).is_none());
        let overload = serde_json::json!({"event": "overload", "data": {
            "stage": "upsampler", "output_rate": 768000, "load": 1.4,
            "action": "upsampler_off", "message": "The engine switched the upsampler off.",
            "episode": 1
        }});
        assert_eq!(
            human_event(&overload).unwrap().0,
            "overload: The engine switched the upsampler off."
        );
    }

    #[test]
    fn spectrum_and_clip_lines_render() {
        let spectrum = audivane_protocol::ipc::Spectrum {
            f_min_hz: 20.0,
            bins_per_octave: 24,
            pre: vec![-160.0; 240],
            post: {
                let mut post = vec![-160.0_f32; 240];
                // A loud 1 kHz region and a quiet high end.
                for db in &mut post[133..138] {
                    *db = 0.0;
                }
                post
            },
            rate: 48_000,
        };
        let line = spectrum_line(&spectrum);
        assert!(line.contains("spectrum   48.0k"), "{line}");
        assert!(line.contains("pre "), "{line}");
        assert!(line.contains("post "), "{line}");
        // The loud 1 kHz octave shows a full bar; quiet ones are floor dots.
        assert!(line.contains('█'), "{line}");
        assert!(line.contains('·'), "{line}");

        let meters = audivane_protocol::ipc::Meters {
            peak_db: vec![-1.0, -2.0],
            rms_db: vec![-3.0, -4.0],
            clipped: 4,
            clipped_ch: vec![1, 3],
            input_peak_db: vec![-0.5, -0.6],
            chain_peak_db: Vec::new(),
            limiter: None,
            xruns: 0,
            dsp_load: 0.1,
        };
        assert_eq!(clip_line(&meters), "clips [L:1 R:3]");
        assert!(meter_line(&meters).contains("clips [L:1 R:3]"));
    }

    #[test]
    fn output_patches_name_bits_and_dither() {
        let (patch, label) = output_patch(Some(16), Some("shaped")).unwrap();
        assert_eq!(
            patch,
            serde_json::json!({"output": {"bits": 16, "dither": "shaped"}})
        );
        assert_eq!(label, "Output: 16-bit, shaped dither");
        let (patch, _) = output_patch(None, Some("none")).unwrap();
        assert_eq!(patch, serde_json::json!({"output": {"dither": "none"}}));
        assert!(output_patch(None, None).is_err());
        assert!(output_patch(None, Some("loud")).is_err());
    }

    #[test]
    fn stage_patches_switch_off_what_a_runs_and_on_what_it_holds() {
        let mut a = ChainConfig::default();
        assert!(stage_patch(&a, AbStage::Eq).is_err());
        assert!(stage_patch(&a, AbStage::Tilt).is_err());
        assert!(stage_patch(&a, AbStage::Pitch).is_err());
        let (patch, label) = stage_patch(&a, AbStage::Dc).unwrap();
        assert_eq!(patch, serde_json::json!({"dc_block": {"enabled": true}}));
        assert_eq!(label, "DC block on");
        a.eq = Some(EqProfile {
            preamp_db: -3.0,
            tilt: Some(TiltConfig {
                enabled: false,
                slope_db_per_oct: Some(-0.5),
                ..serde_json::from_value(serde_json::json!({})).unwrap()
            }),
            ..EqProfile::default()
        });
        let (patch, label) = stage_patch(&a, AbStage::Eq).unwrap();
        assert_eq!(label, "EQ off");
        assert!(patch["eq"].get("tilt").is_none(), "{patch}");
        let (patch, label) = stage_patch(&a, AbStage::Tilt).unwrap();
        assert_eq!(
            patch,
            serde_json::json!({"eq": {"tilt": {"enabled": true}}})
        );
        assert_eq!(label, "Tilt on");
        // A two-section tilt switches both sections.
        let eq = a.eq.as_mut().unwrap();
        eq.tilt_high = Some(TiltConfig {
            enabled: true,
            slope_db_per_oct: Some(1.0),
            start_hz: 4000.0,
            pivot: TiltPivot::Start,
            ..serde_json::from_value(serde_json::json!({})).unwrap()
        });
        let (patch, label) = stage_patch(&a, AbStage::Tilt).unwrap();
        assert_eq!(
            patch,
            serde_json::json!({"eq": {"tilt": null, "tilt_high": null}})
        );
        assert_eq!(label, "Tilt off");
    }

    #[test]
    fn reply_notes_are_optional() {
        assert!(reply_notes(&serde_json::json!({"enabled": true})).is_empty());
        assert_eq!(
            reply_notes(&serde_json::json!({"notes": ["a", "b"]})),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn crossfeed_summary_uses_the_active_rate_or_names_the_assumed_one() {
        let cf = CrossfeedConfig {
            level: CrossfeedLevel::PeakSafe,
            ..Default::default()
        };
        let at = |r: f64| {
            audivane_dsp_core::crossfeed::Crossfeed::try_new(&cf, r, 2)
                .unwrap()
                .level_db()
        };
        assert_eq!(XfRate::new(Some(96_000), 48_000), XfRate::Active(96_000));
        assert_eq!(XfRate::new(Some(0), 48_000), XfRate::Assumed(48_000));
        assert_eq!(XfRate::new(None, 44_100), XfRate::Assumed(44_100));
        let active = summarize_crossfeed(Some(&cf), XfRate::Active(96_000));
        assert!(
            active.contains(&format!("level {:+.2} dB peak-safe)", at(96_000.0))),
            "{active}"
        );
        assert!(!active.contains("assuming"));
        let assumed = summarize_crossfeed(Some(&cf), XfRate::Assumed(44_100));
        assert!(
            assumed.contains(&format!(
                "level {:+.2} dB peak-safe, assuming 44100 Hz)",
                at(44_100.0)
            )),
            "{assumed}"
        );
        assert_ne!(
            format!("{:+.2}", at(44_100.0)),
            format!("{:+.2}", at(96_000.0))
        );
        assert_eq!(summarize_crossfeed(None, XfRate::Active(48_000)), "off");
    }

    #[test]
    fn summarize_eq_formats_layered_sections() {
        use audivane_protocol::{Band, ChannelEq, EQ_PROFILE_VERSION, EqProfile, FilterType};

        assert_eq!(summarize_eq(None), "off");

        let band = Band {
            kind: FilterType::Peaking,
            freq_hz: 100.0,
            gain_db: 2.0,
            q: 1.0,
            enabled: true,
        };
        let profile = EqProfile {
            name: "Kali".into(),
            version: EQ_PROFILE_VERSION,
            preamp_db: -3.0,
            linked_enabled: true,
            bands: vec![band.clone(); 21],
            left: Some(ChannelEq {
                enabled: false,
                preamp_db: 0.0,
                bands: vec![band.clone()],
            }),
            right: Some(ChannelEq {
                enabled: true,
                preamp_db: 0.0,
                bands: vec![band],
            }),
            tilt: None,
            tilt_high: None,
        };
        assert_eq!(
            summarize_eq(Some(&profile)),
            "Kali (L+R: 21 bands, L: off, R: 1 band, preamp -3.0 dB)"
        );

        let mut linked_off = profile.clone();
        linked_off.linked_enabled = false;
        linked_off.left = None;
        linked_off.preamp_db = -0.0;
        assert_eq!(
            summarize_eq(Some(&linked_off)),
            "Kali (L+R: off, R: 1 band, preamp 0.0 dB)"
        );
    }

    #[test]
    fn eq_section_bands_mut_handles_absent_and_created_sections() {
        let mut profile = EqProfile::default();
        assert!(eq_section_bands_mut(&mut profile, EqSectionArg::Left, false).is_err());
        assert!(eq_section_bands_mut(&mut profile, EqSectionArg::Right, false).is_err());
        assert!(
            eq_section_bands_mut(&mut profile, EqSectionArg::Left, true).is_ok()
                && profile.left.is_some()
        );
    }
}
