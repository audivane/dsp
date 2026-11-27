//! `avdsp ctl`: talk to the running daemon over its Unix socket.
//!
//! One subcommand per IPC call. Output is human-readable by default and raw
//! JSON with `--json`. A daemon error reply exits 1 with the daemon's message;
//! a missing daemon exits 3 with a hint.

use anyhow::{Context, Result};
use audivane_dsp_core::design::budget::MAX_KERNEL_TAPS;
use audivane_protocol::ipc::{
    AbB, AbConfig, AbSide, Call, EngineConfig, FollowerState, FollowerStatus, LevelMatch,
    LevelWeighting, OutputChannels, OutputsStatus, PresetPart, RouteTarget, RoutingMode,
    SinkChoice, Status, Topic, VarispeedConfig, VarispeedOverloadHold, VarispeedStatus,
};
use audivane_protocol::{
    Band, ChainConfig, ChannelEq, ChebyshevLayer, ConvolutionConfig, CrossfeedConfig,
    CrossfeedMode, CutoffMode, DcBlockConfig, EqProfile, FilterPhase, FilterPreset, FilterType,
    FluxLayer, HARMONICS_DEFAULT_SIGNS, HARMONICS_ORDERS, HarmonicSign, HarmonicsBias,
    HarmonicsConfig, HarmonicsLayer, HarmonicsRouting, HarmonicsSag, HarmonicsTilt,
    HysteresisBassEq, HysteresisLayer, PitchConfig, PitchQuality, RateTarget, SampleFormat,
    StereoWidthShape, TILT_SLOPES, TapsUnit, TiltConfig, TiltPivot, TiltShelf, UpsamplerConfig,
    WindowKind,
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
    /// Presets stored by the daemon: global ones (parts of chain A's
    /// signal path) or, with --part, one page's own.
    Preset {
        /// A page's presets: eq, convolution, stereo, harmonics, upsampling
        /// or output (DC block, pitch, limiter and output together; default:
        /// the global ones).
        #[arg(long, global = true, value_parser = parse_preset_part)]
        part: Option<PresetPart>,
        #[command(subcommand)]
        command: PresetCommand,
    },
    /// List ALSA playback devices (from /proc/asound; nothing is opened).
    Devices {
        #[command(subcommand)]
        command: Option<DevicesCommand>,
    },
    /// Show or set the output device, and the device channels the left and
    /// right play on (for a device with more than two).
    Device {
        /// ALSA device, e.g. hw:CARD=USB,DEV=0 (left out: show the current
        /// one).
        device: Option<String>,
        /// The device channels (from 0) the left and right play on, e.g.
        /// `2,3`; `0,1` is the default (the first two). The other channels
        /// play silence.
        #[arg(long, value_name = "L,R", value_parser = parse_output_channels)]
        output_channels: Option<OutputChannels>,
    },
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
    /// Measurement playback: a WAV file straight to chosen output channels,
    /// other streams muted, the DACs kept awake.
    Measure {
        #[command(subcommand)]
        command: MeasureCommand,
    },
    /// Room calibration: a bundle of per-device filter matrices, gains and
    /// trims, loaded as one edit.
    Bundle {
        #[command(subcommand)]
        command: BundleCommand,
    },
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
    /// No crossfeed: the stage runs for its stereo width alone.
    None,
    /// Custom BS2B parameters.
    Custom {
        /// Low-pass cutoff frequency in Hz (300..=2000).
        #[arg(long)]
        cutoff_hz: f64,
        /// Low-frequency crossfeed attenuation in dB (1..=15).
        #[arg(long)]
        feed_db: f64,
    },
    /// Stereo width: the side (L - R) gain, 0..=2 (0 mono, 1 unchanged,
    /// 1.25 and 1.5 one and two steps wider). The centre is untouched, so
    /// wider is also louder. Presets keep it.
    Width {
        /// Side gain (0..=2).
        width: f64,
        /// `flat` (every frequency) or `above-bass` (side gain 1 in the
        /// bass, the width above the corner).
        #[arg(long, value_enum)]
        shape: Option<WidthShapeArg>,
        /// Corner of the `above-bass` shape in Hz (200..=2000).
        #[arg(long)]
        corner_hz: Option<f64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum WidthShapeArg {
    Flat,
    AboveBass,
}

impl From<WidthShapeArg> for StereoWidthShape {
    fn from(arg: WidthShapeArg) -> Self {
        match arg {
            WidthShapeArg::Flat => Self::Flat,
            WidthShapeArg::AboveBass => Self::AboveBass,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum HarmonicsCommand {
    /// Print the current layers and the stage's readouts.
    Show,
    /// Turn harmonics off.
    Off,
    /// List the built-in layer presets.
    Presets,
    /// One layer from a preset (see `presets`).
    Preset {
        /// Preset id.
        id: String,
        #[command(flatten)]
        layer: HarmonicsLayerArgs,
    },
    /// List the built-in system presets (whole stages standing for a
    /// typical piece of gear).
    Systems,
    /// Replace the whole stage with a system preset (see `systems`): its
    /// layers, each from a layer preset, and its routing.
    System {
        /// System id.
        id: String,
    },
    /// The `triode` preset (dominant 2nd harmonic, subtle 3rd/4th).
    Triode {
        #[command(flatten)]
        layer: HarmonicsLayerArgs,
    },
    /// The `pentode` preset (dominant 3rd harmonic, subtle 2nd/5th).
    Pentode {
        #[command(flatten)]
        layer: HarmonicsLayerArgs,
    },
    /// A Chebyshev layer with your own levels, relative to the fundamental
    /// for a 0 dBFS sine at drive 0 dB (orders left out are off).
    Custom {
        /// 2nd harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h2_db: Option<f64>,
        /// 3rd harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h3_db: Option<f64>,
        /// 4th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h4_db: Option<f64>,
        /// 5th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h5_db: Option<f64>,
        /// 6th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h6_db: Option<f64>,
        /// 7th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h7_db: Option<f64>,
        /// 8th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h8_db: Option<f64>,
        /// 9th harmonic level in dB (-120..=-20).
        #[arg(long, allow_hyphen_values = true)]
        h9_db: Option<f64>,
        /// Sign of each order's term from H2 up, one `+` or `-` each (e.g.
        /// `+-+-`); orders left out keep the default (H3 `-`, others `+`).
        #[arg(long, allow_hyphen_values = true, value_parser = parse_harmonic_signs)]
        signs: Option<[HarmonicSign; HARMONICS_ORDERS]>,
        /// Flip every even order's sign.
        #[arg(long)]
        invert_even: bool,
        #[command(flatten)]
        layer: HarmonicsLayerArgs,
    },
    /// A flux (transformer or tape core saturation) layer.
    Flux {
        /// Low-shelf corner in Hz (1..=5000): the boost starts below it.
        #[arg(long)]
        corner_hz: f64,
        /// Low-frequency boost in dB (0..=40).
        #[arg(long)]
        boost_db: f64,
        /// H3 in dBc for a 0 dBFS sine at `--at-hz` (-120..=-12).
        #[arg(long, allow_hyphen_values = true)]
        h3_db: f64,
        /// Calibration frequency in Hz (5..=1000).
        #[arg(long)]
        at_hz: f64,
        /// Add the layer to the current ones instead of replacing them.
        #[arg(long)]
        add: bool,
    },
    /// A tape magnetic hysteresis (Jiles-Atherton) layer.
    Hysteresis {
        /// Saturation character (0..=1): lower emphasizes Rayleigh pinning,
        /// higher pushes deeper into the soft Langevin knee near full scale.
        #[arg(long, default_value_t = 0.5)]
        saturation: f64,
        /// Hysteresis loop width (0..=1): 0 is anhysteretic, 1 is a wide loop.
        #[arg(long, default_value_t = 0.35)]
        width: f64,
        /// Distortion wet amount (0..=1).
        #[arg(long, default_value_t = 1.0)]
        amount: f64,
        /// H3 in dBc for a 0 dBFS sine at `--at-hz` (at amount 1, -120..=-12).
        #[arg(long, allow_hyphen_values = true)]
        h3_db: f64,
        /// Calibration frequency in Hz (20..=5000).
        #[arg(long, default_value_t = 1_000.0)]
        at_hz: f64,
        /// Record/playback bass EQ pair, `HZ:DB` (corner 20..=500 Hz, boost
        /// into the core 0..=12 dB, 12 dB/oct; exact inverse after it): the
        /// distortion rises toward the bass, the music stays flat.
        #[arg(long, value_parser = parse_bass_eq)]
        bass_eq: Option<TiltShelf>,
        /// DC offset of the core's field (-1..=1, in units of its
        /// saturation knee): adds H2 and the other even orders.
        #[arg(long, allow_hyphen_values = true)]
        asymmetry: Option<f64>,
        /// Add the layer to the current ones instead of replacing them.
        #[arg(long)]
        add: bool,
    },
    /// Remove layer N (as numbered by `show`, from 1).
    Remove { index: usize },
    /// Move layer N to position TO (as numbered by `show`, from 1); the
    /// order matters in series.
    Move { index: usize, to: usize },
    /// How the layers combine: `parallel` (summed; the default) or `series`
    /// (one after the other in list order, each feeding the next at the
    /// level it puts out, as a preamp into a tape machine).
    Routing {
        #[arg(value_enum)]
        mode: HarmonicsRoutingArg,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HarmonicsRoutingArg {
    Parallel,
    Series,
}

impl From<HarmonicsRoutingArg> for HarmonicsRouting {
    fn from(a: HarmonicsRoutingArg) -> Self {
        match a {
            HarmonicsRoutingArg::Parallel => Self::Parallel,
            HarmonicsRoutingArg::Series => Self::Series,
        }
    }
}

/// Options shared by the Chebyshev layer commands.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct HarmonicsLayerArgs {
    /// Drive in dB (-40..=0): where a 0 dBFS sine sits on the curve.
    #[arg(long, allow_hyphen_values = true)]
    drive_db: Option<f64>,
    /// Amount of the layer's harmonics (0..=1).
    #[arg(long)]
    amount: Option<f64>,
    /// Harmonics rising toward the bass, `HZ:DB[:SLOPE]` (corner 20..=2000
    /// Hz, -24..=24 dB, slope 6, 12, 18 or 24 dB/oct, default 12; odd
    /// orders unless --tilt-even). Replaces a preset's.
    #[arg(long, allow_hyphen_values = true, value_parser = parse_tilt_shelf)]
    tilt_low: Option<TiltShelf>,
    /// Harmonics rising toward the treble, `HZ:DB[:SLOPE]` (corner
    /// 200..=20000 Hz, -24..=24 dB, slope 6, 12, 18 or 24 dB/oct, default
    /// 6; every order). Replaces a preset's.
    #[arg(long, allow_hyphen_values = true, value_parser = parse_tilt_shelf)]
    tilt_high: Option<TiltShelf>,
    /// The low tilt applies to even orders too.
    #[arg(long)]
    tilt_even: bool,
    /// Supply sag, `DB[:ATTACK_MS:RELEASE_MS]`: loud passages lower the
    /// drive by up to DB (0..=6; 0 = off; default times 20:200 ms).
    #[arg(long, value_parser = parse_sag)]
    sag: Option<HarmonicsSag>,
    /// Bias shift, `DEPTH[:ATTACK_MS:RELEASE_MS]`: sustained loud passages
    /// move the operating point to DEPTH (-0.25..=0.25 of full scale; 0 =
    /// off; default times 300:2000 ms). Not with a tilt on the same layer.
    #[arg(long, allow_hyphen_values = true, value_parser = parse_bias)]
    bias: Option<HarmonicsBias>,
    /// Add the layer to the current ones instead of replacing them.
    #[arg(long)]
    add: bool,
    /// Disable the DC-blocking high-pass filter on the even orders.
    #[arg(long)]
    no_dc_block: bool,
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
    /// Set the bass and treble knobs: a low shelf at 140 Hz and a high
    /// shelf at 4.9 kHz on every channel, ±6 dB each (0 removes it).
    Tone {
        /// Bass in dB (below about 150 Hz).
        #[arg(long, allow_hyphen_values = true)]
        bass: Option<f64>,
        /// Treble in dB (above about 5 kHz).
        #[arg(long, allow_hyphen_values = true)]
        treble: Option<f64>,
        /// Set both knobs to 0 dB.
        #[arg(long, conflicts_with_all = ["bass", "treble"])]
        off: bool,
    },
    /// Print the current parametric EQ of chain A.
    Show,
}

#[derive(Subcommand, Debug)]
pub enum ConvCommand {
    /// Replace the convolution chain with one or more WAV impulse responses (or --off).
    /// `IN:OUT[:CH]=PATH` gives a filter-matrix entry (from input channel IN
    /// to output OUT; channel CH of the WAV, else a mono file); a run of
    /// entries is one matrix item, e.g. `0:0=sub_L.wav 1:0=sub_R.wav`.
    Set {
        #[arg(value_name = "IR.wav | IN:OUT[:CH]=IR.wav")]
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
    /// Append a WAV impulse response to the convolution chain, or a filter
    /// matrix given as `IN:OUT[:CH]=PATH` entries.
    Add {
        #[arg(value_name = "IR.wav | IN:OUT[:CH]=IR.wav", required = true)]
        files: Vec<PathBuf>,
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
    /// List configured impulse responses, and the guards' warnings.
    List,
    /// Remove all impulse responses from the convolution chain.
    Clear,
    /// Show the frequency response of the convolution: each enabled item
    /// and the combined result per output, at the graph rate.
    Response {
        /// A follower's own chain (default: the master).
        #[arg(long)]
        device: Option<String>,
        /// The A/B side (default: a).
        #[arg(long, value_enum)]
        side: Option<ConvSideArg>,
    },
    /// Show or set the convolution guards: the boost cap (a chain whose
    /// convolution boosts more on any output is refused) and the subsonic
    /// limit (a warning for gain below it).
    Safety {
        /// The most the convolution may boost on any output, dB (0..=24).
        #[arg(long)]
        max_boost_db: Option<f64>,
        /// Warn about gain below this frequency, Hz (5..=60).
        #[arg(long)]
        subsonic_hz: Option<f64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ConvSideArg {
    A,
    B,
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
    /// Its own digital volume, dB (-120..=0); following the main volume,
    /// the level it plays at now.
    #[arg(long, value_name = "DB", allow_hyphen_values = true)]
    pub volume_db: Option<f64>,
    /// Follow the main volume (on): its changes, balance and mute move this
    /// device too, keeping its level to the main one; off (the default for
    /// a device added by hand), its volume is its own.
    #[arg(long, value_enum)]
    pub follow_volume: Option<OnOff>,
    /// Heard (on) or muted in sync (off).
    #[arg(long, value_enum)]
    pub enabled: Option<OnOff>,
    /// The device channels (from 0) its left and right play on, e.g. `2,3`
    /// (`0,1`: the first two).
    #[arg(long, value_name = "L,R", value_parser = parse_output_channels)]
    pub output_channels: Option<OutputChannels>,
    /// Its own chain: a JSON merge patch over chain A (inline JSON, or a
    /// file holding it), e.g. '{"convolution":[{"ir_path":"/abs/sub.wav"}]}'.
    /// It follows chain A outside what it names; `{}` clears it.
    #[arg(long, value_name = "JSON|FILE", conflicts_with = "no_chain_patch")]
    pub chain_patch: Option<String>,
    /// Play chain A again (clear its own chain).
    #[arg(long)]
    pub no_chain_patch: bool,
    /// Its device period, ms (5..=50, as the nearest power of two of
    /// frames; 0: the default, about 5 ms). Shorter needs less of the main
    /// output's alignment. It reopens the device.
    #[arg(long, value_name = "MS")]
    pub period_ms: Option<f64>,
}

/// An `--output-channels` value: `L,R`, two device channels from 0.
fn parse_output_channels(arg: &str) -> Result<OutputChannels, String> {
    let (l, r) = arg
        .split_once(',')
        .ok_or_else(|| format!("{arg:?}: give two channels as L,R (e.g. 2,3)"))?;
    let channel = |s: &str| {
        s.trim()
            .parse::<u32>()
            .map_err(|_| format!("{s:?} is not a channel number (from 0)"))
    };
    let pair = OutputChannels([channel(l)?, channel(r)?]);
    match pair.error() {
        Some(e) => Err(e),
        None => Ok(pair),
    }
}

/// A `--chain-patch` value: inline JSON, else a file holding it.
fn chain_patch_arg(arg: &str) -> CtlResult<serde_json::Value> {
    let text = if arg.trim_start().starts_with('{') {
        arg.to_string()
    } else {
        std::fs::read_to_string(arg).with_context(|| format!("reading the chain patch {arg}"))?
    };
    let v: serde_json::Value =
        serde_json::from_str(&text).context("the chain patch is not valid JSON")?;
    if !v.is_object() {
        bail!("the chain patch must be a JSON object (a merge patch on chain A)");
    }
    Ok(v)
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
    /// Band design: end of the passband, a fraction of the source Nyquist
    /// (`0.908`) or a frequency in Hz that stays put at every source rate
    /// (`20k`, `20kHz`, `20000Hz`; a plain number above 1.5 is Hz too). A
    /// frequency that does not fit the band at a rate falls back to the
    /// band's fraction there (implies --cutoff-mode band).
    #[arg(long, value_parser = parse_passband_end)]
    pub passband_end: Option<PassbandEnd>,
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
    /// Hide the playing side for a blind test (status reads `hidden`).
    /// The app runs blind tests; this sets only the mask.
    Blind {
        #[arg(value_enum)]
        state: OnOff,
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
    /// Save chain A (or, with --part, that part of it) as a preset and make
    /// it active.
    Save {
        #[arg(value_name = "NAME")]
        name: String,
        /// Replace an existing preset of the same name.
        #[arg(long)]
        overwrite: bool,
        /// A global preset holding only these parts (comma-separated;
        /// default: every part).
        #[arg(long, value_delimiter = ',', value_parser = parse_preset_part, conflicts_with = "part")]
        parts: Option<Vec<PresetPart>>,
    },
    /// Apply a preset's parts to chain A (the others stay) and make it active.
    Load {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Reset that part (needs --part) to its defaults, with no preset
    /// active: `save NAME` then saves it as a new one.
    Blank,
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
    /// Import an Equalizer APO / AutoEQ file as an EQ preset (not loaded).
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
pub enum MeasureCommand {
    /// Play FILE and wait until it has left the DACs; prints where and when
    /// it played (`--json`: the final `measure_status`). Exit code 1 when it
    /// ended early.
    Play(MeasurePlayArgs),
    /// Stop the running measurement (the file fades out, the mute lifts).
    Stop,
    /// The running (or last) measurement.
    Status,
}

#[derive(Subcommand, Debug)]
pub enum BundleCommand {
    /// Load the bundle manifest FILE (replacing the loaded one): checked
    /// whole, applied as one edit; on any error nothing changes.
    Load { file: String },
    /// Remove the loaded calibration (the followers' trims and patches put
    /// back).
    Unload,
    /// The loaded calibration.
    Show,
    /// A/B of calibrations on the master: side B is chain A with FILE's
    /// calibration in place of the loaded one (`--none`: without one).
    Ab {
        #[arg(required_unless_present = "none", conflicts_with = "none")]
        file: Option<String>,
        #[arg(long)]
        none: bool,
    },
    /// Switch the loaded calibration on or off without unloading it. Off,
    /// the main speakers play without it and its followers (a sub) are
    /// muted.
    Enable {
        #[arg(value_enum)]
        state: OnOff,
    },
    /// Set one calibration part's level (its gain, dB; `--reset`: the
    /// manifest's). `--device` names a follower part; left out, the master's.
    Gain {
        #[arg(long)]
        device: Option<String>,
        #[arg(
            required_unless_present = "reset",
            conflicts_with = "reset",
            allow_negative_numbers = true
        )]
        db: Option<f64>,
        #[arg(long)]
        reset: bool,
    },
}

#[derive(Args, Debug)]
pub struct MeasurePlayArgs {
    /// A WAV file on the daemon's machine: mono (played on every route) or
    /// one channel per route, in order.
    pub file: String,
    /// `DEVICE:CHANNEL` (repeatable): `master` or the main device's name,
    /// or a follower's device; channels from 0.
    #[arg(long = "route", value_name = "DEV:CH", required = true)]
    pub routes: Vec<String>,
    /// Play at the chain's input, through every stage (default: after the
    /// chain, its stages skipped).
    #[arg(long)]
    pub active: bool,
    /// Keepalive noise under the file, dBFS RMS (default -100).
    #[arg(
        long,
        value_name = "DB",
        allow_negative_numbers = true,
        conflicts_with = "no_keepalive"
    )]
    pub keepalive_db: Option<f64>,
    /// No keepalive noise.
    #[arg(long)]
    pub no_keepalive: bool,
    /// The keepalive alone before the file, ms (default 2000).
    #[arg(long, value_name = "MS")]
    pub lead_in_ms: Option<f64>,
    /// Let other streams play (on the channels the file does not use).
    #[arg(long)]
    pub no_mute: bool,
}

/// `DEV:CH`: the channel after the last colon (device names hold colons).
fn parse_measure_route(s: &str) -> Result<audivane_protocol::ipc::MeasureRoute, String> {
    let (dev, ch) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("--route {s}: expected DEVICE:CHANNEL"))?;
    let channel = ch
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("--route {s}: {ch:?} is not a channel number"))?;
    if dev.trim().is_empty() {
        return Err(format!("--route {s}: no device"));
    }
    Ok(audivane_protocol::ipc::MeasureRoute {
        device: dev.trim().to_string(),
        channel,
    })
}

fn print_measure_status(s: &audivane_protocol::ipc::MeasureStatus) {
    use audivane_protocol::ipc::MeasureChain;
    let chain = match s.chain {
        Some(MeasureChain::Active) => "active",
        Some(MeasureChain::Bypass) => "bypass",
        None => "-",
    };
    println!(
        "measure #{}: {:?}{}",
        s.run,
        s.state,
        s.error
            .as_ref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    );
    if s.run == 0 {
        return;
    }
    println!(
        "  file {} ({} frames, {:.2} s at {} Hz), {chain}, graph {} Hz, played {:.2} s",
        s.file.as_deref().unwrap_or("-"),
        s.file_frames,
        s.duration_s,
        s.file_rate_hz,
        s.graph_rate_hz,
        s.progress_s
    );
    if let Some(f) = s.source_frame {
        println!(
            "  starts at source frame {f}, after a {:.0} ms lead-in",
            s.lead_in_ms
        );
    }
    for d in &s.devices {
        let chans: Vec<String> = d.channels.iter().map(u32::to_string).collect();
        println!(
            "  {} ({}) ch {}: {} Hz (injected at {} Hz{}), latency {:.1} frames = {:.1} us (chain {:.1} + queue {:.1}), start frame {}, start time {}",
            d.device,
            d.role,
            chans.join(","),
            d.rate_hz,
            d.injection_rate_hz,
            if d.resampled { ", resampled" } else { "" },
            d.latency_frames,
            d.latency_us,
            d.chain_latency_frames,
            d.queue_frames,
            d.start_frame.map_or("-".into(), |v| v.to_string()),
            d.start_time_ns.map_or("-".into(), |v| format!("{v} ns")),
        );
        if d.clamped_samples > 0 || d.drained {
            println!(
                "    {}{}",
                if d.drained { "drained" } else { "not drained" },
                if d.clamped_samples > 0 {
                    format!(", {} samples clamped at -1 dBFS", d.clamped_samples)
                } else {
                    String::new()
                }
            );
        }
        if let Some(c) = &d.continuity {
            println!(
                "    continuity: {} readings, largest residual {:.1} frames = {:.0} us, largest step {:.1} frames = {:.0} us at {:.3} s",
                c.readings,
                c.max_residual_frames,
                c.max_residual_us,
                c.jump_frames,
                c.jump_us,
                c.jump_at_s
            );
        }
    }
    for n in &s.notes {
        println!("  note: {n}");
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

#[derive(Debug, Subcommand)]
pub enum DevicesCommand {
    /// Say which card each output is, when identical DACs moved USB ports
    /// (`status` lists the outputs held): OUTPUT DEVICE pairs, OUTPUT being
    /// `master` or a follower's device as configured, e.g.
    /// `assign master hw:CARD=USB,DEV=0 hw:CARD=USB,DEV=0 hw:CARD=USB_1,DEV=0`.
    Assign {
        #[arg(value_name = "OUTPUT DEVICE", num_args = 2.., required = true)]
        pairs: Vec<String>,
    },
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
        CtlCommand::Harmonics { command } => {
            match command {
                HarmonicsCommand::Show => {
                    let harmonics = current_harmonics(client)?;
                    if json {
                        print_json(&serde_json::to_value(&harmonics)?)?;
                    } else {
                        print_harmonics(harmonics.as_ref());
                        if let Some(h) = harmonics.as_ref().filter(|h| h.enabled) {
                            for (i, l) in h.layers.iter().enumerate() {
                                println!("  {}. {}", i + 1, describe_layer(l));
                            }
                            // An older daemon has no readouts: skip them.
                            if let Ok(v) = client.call(Call::HarmonicsReadout {
                                config: None,
                                sweep: false,
                                sweep_freq_hz: None,
                                sweep_level_dbfs: None,
                            }) && let Ok(r) = serde_json::from_value(v)
                            {
                                print_harmonics_readout(&r);
                                if h.layers.iter().any(|l| {
                                    l.enabled()
                                        && matches!(l, HarmonicsLayer::Chebyshev(c) if c.is_dynamic())
                                }) {
                                    println!(
                                        "  (curves at rest: sag and bias move them on loud passages)"
                                    );
                                }
                            }
                            let output_rate = client
                                .call(Call::Status)
                                .ok()
                                .and_then(|v| serde_json::from_value::<Status>(v).ok())
                                .map(|s| s.output_rate);
                            if let Some(note) = output_rate.and_then(|r| harmonics_alias_note(h, r))
                            {
                                println!("{note}");
                            }
                        }
                    }
                }
                HarmonicsCommand::Off => {
                    client.call(Call::SetHarmonics(None))?;
                    println!("harmonics off");
                }
                HarmonicsCommand::Presets => {
                    let presets = audivane_protocol::harmonics_presets();
                    if json {
                        print_json(&serde_json::to_value(presets)?)?;
                    } else {
                        for p in presets {
                            println!(
                                "{:<22} {} [{}]: {}",
                                p.id,
                                p.name,
                                p.origin,
                                summarize_layer(&p.layer)
                            );
                        }
                    }
                }
                HarmonicsCommand::Systems => {
                    let systems = audivane_protocol::harmonics_systems();
                    if json {
                        print_json(&serde_json::to_value(systems)?)?;
                    } else {
                        for s in systems {
                            let layers: Vec<&str> =
                                s.layers.iter().filter_map(|l| l.preset()).collect();
                            let routing = match s.routing {
                                HarmonicsRouting::Parallel => "",
                                HarmonicsRouting::Series => ", in series",
                            };
                            println!(
                                "{:<22} {} [{}{routing}]\n{:<22} {}\n{:<22} {}",
                                s.id,
                                s.name,
                                layers.join(" → "),
                                "",
                                s.gear,
                                "",
                                s.sound
                            );
                        }
                    }
                }
                HarmonicsCommand::System { id } => {
                    let Some(system) = audivane_protocol::harmonics_system(&id) else {
                        let ids: Vec<&str> = audivane_protocol::harmonics_systems()
                            .iter()
                            .map(|s| s.id.as_str())
                            .collect();
                        bail!(
                            "unknown harmonics system {id:?} (one of: {})",
                            ids.join(", ")
                        );
                    };
                    let cfg = system.to_config();
                    client.call(Call::SetHarmonics(Some(cfg.clone())))?;
                    println!(
                        "harmonics set to {}: {}",
                        system.name,
                        summarize_harmonics(Some(&cfg))
                    );
                }
                HarmonicsCommand::Preset { id, layer } => set_harmonics_preset(client, &id, layer)?,
                HarmonicsCommand::Triode { layer } => {
                    set_harmonics_preset(client, "triode", layer)?
                }
                HarmonicsCommand::Pentode { layer } => {
                    set_harmonics_preset(client, "pentode", layer)?
                }
                HarmonicsCommand::Custom {
                    h2_db,
                    h3_db,
                    h4_db,
                    h5_db,
                    h6_db,
                    h7_db,
                    h8_db,
                    h9_db,
                    signs,
                    invert_even,
                    layer,
                } => {
                    let l = ChebyshevLayer {
                        enabled: true,
                        preset: None,
                        levels_db: [h2_db, h3_db, h4_db, h5_db, h6_db, h7_db, h8_db, h9_db],
                        signs: signs.unwrap_or(HARMONICS_DEFAULT_SIGNS),
                        invert_even,
                        drive_db: layer.drive_db.unwrap_or(0.0),
                        amount: layer.amount.unwrap_or(1.0),
                        tilt: layer.tilt(None).map(Box::new),
                        sag: layer.sag,
                        bias: layer.bias,
                    };
                    set_harmonics_layer(
                        client,
                        HarmonicsLayer::Chebyshev(l),
                        layer.add,
                        layer.no_dc_block,
                    )?;
                }
                HarmonicsCommand::Flux {
                    corner_hz,
                    boost_db,
                    h3_db,
                    at_hz,
                    add,
                } => {
                    let l = FluxLayer {
                        enabled: true,
                        preset: None,
                        corner_hz,
                        max_boost_db: boost_db,
                        target_h3_db: h3_db,
                        calibration_hz: at_hz,
                    };
                    set_harmonics_layer(client, HarmonicsLayer::Flux(l), add, false)?;
                }
                HarmonicsCommand::Hysteresis {
                    saturation,
                    width,
                    amount,
                    h3_db,
                    at_hz,
                    bass_eq,
                    asymmetry,
                    add,
                } => {
                    let l = HysteresisLayer {
                        enabled: true,
                        preset: None,
                        saturation,
                        width,
                        amount,
                        target_h3_db: h3_db,
                        calibration_hz: at_hz,
                        bass_eq: bass_eq.map(|s| HysteresisBassEq {
                            enabled: true,
                            hz: s.hz,
                            db: s.db,
                        }),
                        asymmetry,
                    };
                    set_harmonics_layer(client, HarmonicsLayer::Hysteresis(l), add, false)?;
                }
                HarmonicsCommand::Remove { index } => {
                    let Some(mut cfg) = current_harmonics(client)? else {
                        bail!("harmonics are off");
                    };
                    if index == 0 || index > cfg.layers.len() {
                        bail!("no layer {index} (the stage has {})", cfg.layers.len());
                    }
                    cfg.layers.remove(index - 1);
                    client.call(Call::SetHarmonics(Some(cfg.clone())))?;
                    println!("harmonics set: {}", summarize_harmonics(Some(&cfg)));
                }
                HarmonicsCommand::Move { index, to } => {
                    let Some(mut cfg) = current_harmonics(client)? else {
                        bail!("harmonics are off");
                    };
                    let n = cfg.layers.len();
                    if index == 0 || index > n || to == 0 || to > n {
                        bail!(
                            "no layer {} (the stage has {n})",
                            if index == 0 || index > n { index } else { to }
                        );
                    }
                    let layer = cfg.layers.remove(index - 1);
                    cfg.layers.insert(to - 1, layer);
                    client.call(Call::SetHarmonics(Some(cfg.clone())))?;
                    println!("harmonics set: {}", summarize_harmonics(Some(&cfg)));
                }
                HarmonicsCommand::Routing { mode } => {
                    let Some(mut cfg) = current_harmonics(client)? else {
                        bail!("harmonics are off");
                    };
                    cfg.routing = mode.into();
                    client.call(Call::SetHarmonics(Some(cfg.clone())))?;
                    println!("harmonics set: {}", summarize_harmonics(Some(&cfg)));
                }
            }
        }
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
            CrossfeedCommand::Width {
                width,
                shape,
                corner_hz,
            } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                // With the stage off, width alone (no crossfeed).
                let mut cfg =
                    current
                        .chain
                        .crossfeed
                        .filter(|c| c.enabled)
                        .unwrap_or(CrossfeedConfig {
                            mode: CrossfeedMode::None,
                            ..CrossfeedConfig::default()
                        });
                cfg.width = width;
                if let Some(shape) = shape {
                    cfg.width_shape = shape.into();
                }
                if let Some(hz) = corner_hz {
                    cfg.width_corner_hz = hz;
                }
                client.call(Call::SetCrossfeed(Some(cfg.clone())))?;
                let rate = XfRate::query(client, current.rate.default_rate);
                println!("crossfeed set: {}", summarize_crossfeed(Some(&cfg), rate));
            }
            preset => {
                // Presets keep the width already chosen.
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let kept = current.chain.crossfeed.unwrap_or_default();
                let (mode, cutoff_hz, feed_db) = match preset {
                    CrossfeedCommand::Default => (CrossfeedMode::Bs2bDefault, None, None),
                    CrossfeedCommand::ChuMoy => (CrossfeedMode::Bs2bChuMoy, None, None),
                    CrossfeedCommand::JanMeier => (CrossfeedMode::Bs2bJanMeier, None, None),
                    CrossfeedCommand::Linkwitz => (CrossfeedMode::Linkwitz, None, None),
                    CrossfeedCommand::None => (CrossfeedMode::None, None, None),
                    CrossfeedCommand::Custom { cutoff_hz, feed_db } => {
                        (CrossfeedMode::Bs2bCustom, Some(cutoff_hz), Some(feed_db))
                    }
                    CrossfeedCommand::Show
                    | CrossfeedCommand::Off
                    | CrossfeedCommand::Width { .. } => unreachable!(),
                };
                let defaults = CrossfeedConfig::default();
                let cfg = CrossfeedConfig {
                    enabled: true,
                    mode,
                    cutoff_hz: cutoff_hz.unwrap_or(defaults.cutoff_hz),
                    feed_db: feed_db.unwrap_or(defaults.feed_db),
                    width: kept.width,
                    width_shape: kept.width_shape,
                    width_corner_hz: kept.width_corner_hz,
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
            EqCommand::Tone { bass, treble, off } => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut profile = current.chain.eq.unwrap_or_default();
                let mut tone = profile.tone.unwrap_or_default();
                if off {
                    tone = Default::default();
                }
                if let Some(db) = bass {
                    tone.bass_db = db;
                }
                if let Some(db) = treble {
                    tone.treble_db = db;
                }
                profile.tone = tone.is_active().then_some(tone);
                client.call(Call::SetEq(Some(profile)))?;
                println!("EQ tone: {}", format_tone(&tone));
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
                    let convs = crate::conv_args::conv_items(&files, gain_db, name.as_deref())?;
                    if convs.len() > 1 && name.is_some() {
                        bail!(
                            "--name can only be used with a single item; use `conv add` for named entries"
                        );
                    }
                    let n = convs.len();
                    let what = crate::conv_args::conv_files(&convs[0]);
                    let matrix = convs[0].matrix.is_some();
                    let reply = client.call(Call::SetConvolution(convs))?;
                    if n == 1 && matrix {
                        println!("convolution set to a filter matrix: {what}");
                    } else if n == 1 {
                        println!("convolution set from {what}");
                    } else {
                        println!("convolution set ({n} IRs)");
                    }
                    print_notes(&reply);
                }
            }
            ConvCommand::Add {
                files,
                gain_db,
                name,
                disabled,
            } => {
                let mut items = crate::conv_args::conv_items(&files, gain_db, name.as_deref())?;
                if items.len() != 1 {
                    bail!("give one IR, or the entries of one filter matrix");
                }
                let mut item = items.remove(0);
                item.enabled = !disabled;
                let what = crate::conv_args::conv_files(&item);
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                current.chain.convolution.push(item);
                let idx = current.chain.convolution.len() - 1;
                let reply = client.call(Call::SetConvolution(current.chain.convolution))?;
                println!("added convolution IR [{idx}] {what}");
                print_notes(&reply);
            }
            ConvCommand::Remove { target } => {
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                let idx = find_conv_index(&current.chain.convolution, &target)?;
                let removed = current.chain.convolution.remove(idx);
                let reply = client.call(Call::SetConvolution(current.chain.convolution))?;
                println!(
                    "removed convolution IR [{idx}] {}",
                    crate::conv_args::conv_files(&removed)
                );
                print_notes(&reply);
            }
            ConvCommand::Enable { target } => {
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                let idx = find_conv_index(&current.chain.convolution, &target)?;
                current.chain.convolution[idx].enabled = true;
                let what = crate::conv_args::conv_files(&current.chain.convolution[idx]);
                let reply = client.call(Call::SetConvolution(current.chain.convolution))?;
                println!("enabled convolution IR [{idx}] {what}");
                print_notes(&reply);
            }
            ConvCommand::Disable { target } => {
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                let idx = find_conv_index(&current.chain.convolution, &target)?;
                current.chain.convolution[idx].enabled = false;
                let what = crate::conv_args::conv_files(&current.chain.convolution[idx]);
                let reply = client.call(Call::SetConvolution(current.chain.convolution))?;
                println!("disabled convolution IR [{idx}] {what}");
                print_notes(&reply);
            }
            ConvCommand::List => {
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                if json {
                    print_json(&serde_json::to_value(&current.chain.convolution)?)?;
                } else {
                    print_conv_list(&current.chain.convolution);
                    print_conv_guards(client, &current)?;
                }
            }
            ConvCommand::Safety {
                max_boost_db,
                subsonic_hz,
            } => {
                let mut current: EngineConfig =
                    serde_json::from_value(client.call(Call::GetConfig)?)?;
                if max_boost_db.is_none() && subsonic_hz.is_none() {
                    if json {
                        print_json(&serde_json::to_value(current.convolution_safety)?)?;
                    } else {
                        print_conv_guards(client, &current)?;
                    }
                } else {
                    let safety = &mut current.convolution_safety;
                    if let Some(v) = max_boost_db {
                        safety.max_boost_db = v;
                    }
                    if let Some(v) = subsonic_hz {
                        safety.subsonic_hz = v;
                    }
                    let safety = *safety;
                    let reply = client.call(Call::SetConvolutionSafety(safety))?;
                    println!(
                        "convolution guards: boost cap {:+.1} dB, subsonic limit {} Hz",
                        safety.max_boost_db, safety.subsonic_hz
                    );
                    print_notes(&reply);
                }
            }
            ConvCommand::Response { device, side } => {
                let value = client.call(Call::ConvResponse {
                    side: side.map(|s| match s {
                        ConvSideArg::A => AbSide::A,
                        ConvSideArg::B => AbSide::B,
                    }),
                    device: device.clone(),
                })?;
                let response: audivane_protocol::ipc::ConvResponse = serde_json::from_value(value)?;
                if json {
                    println!("{}", serde_json::to_string(&response)?);
                } else {
                    print_conv_response(&response);
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
                                editable_config(k)
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
                        match set.passband_end {
                            Some(PassbandEnd::Fraction(v)) => {
                                m.band.passband_end = v;
                                m.band.passband_end_hz = None;
                            }
                            Some(PassbandEnd::Hz(hz)) => m.band.passband_end_hz = Some(hz),
                            None => {}
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
            let call = |device: String, remove: bool, set: FollowerSetArgs| -> CtlResult<Call> {
                let chain_patch = match (&set.chain_patch, set.no_chain_patch) {
                    (Some(p), _) => Some(chain_patch_arg(p)?),
                    (None, true) => Some(serde_json::json!({})),
                    (None, false) => None,
                };
                Ok(Call::SetFollower {
                    device,
                    remove,
                    enabled: set.enabled.map(|e| e == OnOff::On),
                    trim_ms: set.trim_ms,
                    volume_db: set.volume_db,
                    output_channels: set.output_channels,
                    follow_volume: set.follow_volume.map(|e| e == OnOff::On),
                    chain_patch,
                    period_ms: set.period_ms,
                })
            };
            let call = match (args.command, args.device) {
                (Some(FollowerCommand::Add { device, set }), _) => call(device, false, set)?,
                (Some(FollowerCommand::Remove { device }), _) => {
                    call(device, true, FollowerSetArgs::default())?
                }
                (None, Some(device)) => {
                    // Editing never adds: a typo must not open another DAC.
                    let st: Status = serde_json::from_value(client.call(Call::Status)?)?;
                    if !st.outputs.followers.iter().any(|f| f.device == device) {
                        bail!("{device} is not a follower (add it with `follower add {device}`)");
                    }
                    call(device, false, args.set)?
                }
                (None, None) => bail!("give a follower's DEVICE, or `add` / `remove`"),
            };
            let (device, removed) = match &call {
                Call::SetFollower { device, remove, .. } => (device.clone(), *remove),
                _ => (String::new(), true),
            };
            let outputs: OutputsStatus = serde_json::from_value(client.call(call)?)?;
            if json {
                print_json(&serde_json::to_value(&outputs)?)?;
            } else {
                print_outputs(&outputs);
                if !removed {
                    // Its own chain's convolution warnings (clip risk,
                    // subsonic energy), as `conv` prints chain A's.
                    let st: Status = serde_json::from_value(client.call(Call::Status)?)?;
                    for m in follower_warnings(&st.conv_warnings, &device) {
                        println!("warning: {m}");
                    }
                }
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
            AbCommand::Blind { state } => print_ab(client.call(Call::AbBlind {
                enabled: state == OnOff::On,
            })?)?,
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
                apply_volume_settings(&mut cfg.volume, &args);
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
        CtlCommand::Preset { part, command } => match command {
            PresetCommand::List => {
                let value = client.call(Call::ListPresets)?;
                let presets: Vec<audivane_protocol::ipc::PresetInfo> =
                    serde_json::from_value(value)?;
                let presets: Vec<_> = presets.into_iter().filter(|p| p.part == part).collect();
                if json {
                    print_json(&serde_json::to_value(&presets)?)?;
                } else {
                    let status = serde_json::from_value::<audivane_protocol::ipc::Status>(
                        client.call(Call::Status)?,
                    )
                    .ok();
                    let active: Option<String> = status.and_then(|status| match part {
                        None => status.preset.map(|preset| preset.name),
                        Some(part) => status
                            .part_presets
                            .into_iter()
                            .find(|state| state.part == part)
                            .map(|state| state.name),
                    });
                    print_presets(&presets, active.as_deref())?;
                }
            }
            PresetCommand::Show { name } => {
                let value = client.call(Call::GetPreset { name, part })?;
                print_json(&value)?;
            }
            PresetCommand::Save {
                name,
                overwrite,
                parts,
            } => {
                let value = client.call(Call::SavePreset {
                    name,
                    overwrite,
                    part,
                    parts,
                })?;
                let info: audivane_protocol::ipc::PresetInfo = serde_json::from_value(value)?;
                if json {
                    println!("{}", serde_json::to_string(&info)?);
                } else {
                    println!("saved preset {:?} ({})", info.name, preset_summary(&info));
                }
            }
            PresetCommand::Load { name } => {
                client.call(Call::LoadPreset {
                    name: name.clone(),
                    part,
                })?;
                println!("loaded preset {name:?}");
            }
            PresetCommand::Blank => {
                let Some(part) = part else {
                    bail!(
                        "preset blank needs --part (eq, convolution, stereo, harmonics, upsampling or output)"
                    );
                };
                client.call(Call::BlankPreset { part })?;
                println!("{} reset to defaults, no preset active", part.name());
            }
            PresetCommand::Delete { name } => {
                client.call(Call::DeletePreset {
                    name: name.clone(),
                    part,
                })?;
                println!("deleted preset {name:?}");
            }
            PresetCommand::Rename { from, to } => {
                let value = client.call(Call::RenamePreset {
                    from: from.clone(),
                    to: to.clone(),
                    part,
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
        CtlCommand::Devices {
            command: Some(DevicesCommand::Assign { pairs }),
        } => {
            if pairs.len() % 2 != 0 {
                return Err(anyhow::anyhow!("assign takes OUTPUT DEVICE pairs").into());
            }
            let assignments = pairs
                .chunks(2)
                .map(|p| audivane_protocol::ipc::DeviceAssignment {
                    output: p[0].clone(),
                    device: p[1].clone(),
                })
                .collect();
            client.call(Call::AssignDevices { assignments })?;
            if !json {
                println!("outputs assigned");
            }
        }
        CtlCommand::Device {
            device,
            output_channels,
        } => {
            if device.is_some() || output_channels.is_some() {
                let device = match device {
                    Some(d) => d,
                    None => {
                        let c: EngineConfig =
                            serde_json::from_value(client.call(Call::GetConfig)?)?;
                        c.device
                    }
                };
                client.call(Call::SetDevice {
                    device,
                    output_channels,
                })?;
            }
            let config: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
            let st: Status = serde_json::from_value(client.call(Call::Status)?)?;
            if json {
                print_json(&serde_json::json!({
                    "device": config.device,
                    "output_channels": config.output_channels.unwrap_or(OutputChannels::DEFAULT),
                    "device_caps": st.device_caps,
                }))?;
            } else {
                println!("{}", device_line(&config, st.device_caps.as_ref()));
            }
        }
        CtlCommand::Devices { command: None } => {
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
        CtlCommand::Measure { command } => {
            use audivane_protocol::ipc::{
                MeasureChain, MeasurePlayRequest, MeasureState, MeasureStatus,
            };
            let status = match command {
                MeasureCommand::Stop => {
                    serde_json::from_value::<MeasureStatus>(client.call(Call::MeasureStop)?)?
                }
                MeasureCommand::Status => {
                    serde_json::from_value::<MeasureStatus>(client.call(Call::MeasureStatus)?)?
                }
                MeasureCommand::Play(a) => {
                    let routes = a
                        .routes
                        .iter()
                        .map(|r| parse_measure_route(r))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| CtlError::Other(anyhow::anyhow!(e)))?;
                    let file = std::fs::canonicalize(&a.file)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or(a.file.clone());
                    let req = MeasurePlayRequest {
                        file,
                        routes,
                        chain: if a.active {
                            MeasureChain::Active
                        } else {
                            MeasureChain::Bypass
                        },
                        mute_others: !a.no_mute,
                        keepalive_db: if a.no_keepalive {
                            None
                        } else {
                            Some(a.keepalive_db.unwrap_or(-100.0))
                        },
                        lead_in_ms: a.lead_in_ms.unwrap_or(2000.0),
                    };
                    let started: MeasureStatus =
                        serde_json::from_value(client.call(Call::MeasurePlay(req))?)?;
                    let mut last = started.state;
                    if !json {
                        eprintln!("measure #{}: {:?}", started.run, last);
                    }
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let s: MeasureStatus =
                            serde_json::from_value(client.call(Call::MeasureStatus)?)?;
                        if s.run != started.run {
                            break s;
                        }
                        if s.state != last && !json {
                            eprintln!("measure #{}: {:?}", s.run, s.state);
                        }
                        last = s.state;
                        if !matches!(
                            s.state,
                            MeasureState::Preparing
                                | MeasureState::LeadIn
                                | MeasureState::Playing
                                | MeasureState::Tail
                        ) {
                            break s;
                        }
                    }
                }
            };
            if json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                print_measure_status(&status);
            }
            if matches!(status.state, MeasureState::Error | MeasureState::Stopped) {
                return Ok(1);
            }
        }
        CtlCommand::Bundle { command } => {
            use audivane_protocol::ipc::CalibrationStatus;
            let abs = |f: &str| {
                std::fs::canonicalize(f)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| f.to_string())
            };
            let value = match command {
                BundleCommand::Load { file } => {
                    client.call(Call::BundleLoad { path: abs(&file) })?
                }
                BundleCommand::Unload => {
                    let v = client.call(Call::BundleUnload)?;
                    if json {
                        println!("{}", serde_json::to_string(&v)?);
                    } else {
                        for n in v
                            .get("notes")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                        {
                            eprintln!("note: {n}");
                        }
                        println!("room calibration removed");
                    }
                    return Ok(0);
                }
                BundleCommand::Show => client.call(Call::BundleShow)?,
                BundleCommand::Ab { file, none } => client.call(Call::BundleAb {
                    path: if none { None } else { file.as_deref().map(abs) },
                })?,
                BundleCommand::Enable { state } => client.call(Call::BundleEnable {
                    on: state == OnOff::On,
                })?,
                BundleCommand::Gain { device, db, reset } => client.call(Call::BundleGain {
                    device,
                    gain_db: if reset { None } else { db },
                })?,
            };
            if json {
                println!("{}", serde_json::to_string(&value)?);
                return Ok(0);
            }
            if let Some(notes) = value.get("notes").and_then(Value::as_array) {
                for n in notes.iter().filter_map(Value::as_str) {
                    eprintln!("note: {n}");
                }
            }
            if value.get("current").is_some() {
                // `bundle ab`: the A/B status.
                let st: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
                println!(
                    "A/B on: B is chain A with the other calibration (offset {:+.2} dB); \
                     `ab select b` switches",
                    st.offset_db
                );
                return Ok(0);
            }
            match serde_json::from_value::<Option<CalibrationStatus>>(value)? {
                None => println!("no room calibration loaded"),
                Some(c) => {
                    println!(
                        "{} ({}, designed at {} Hz){}",
                        c.label,
                        c.path,
                        c.design_rate_hz,
                        if c.enabled {
                            ""
                        } else {
                            ": off (the followers are muted)"
                        }
                    );
                    if let Some(n) = &c.notes {
                        println!("  notes: {n}");
                    }
                    for d in &c.devices {
                        println!(
                            "  {:<8} {}: {} filter{}, {:+.1} dB{}{}{}",
                            d.role,
                            d.device,
                            d.entries,
                            if d.entries == 1 { "" } else { "s" },
                            d.gain_db,
                            if d.gain_db == d.manifest_gain_db {
                                String::new()
                            } else {
                                format!(" (bundle {:+.1} dB)", d.manifest_gain_db)
                            },
                            if d.role == "follower" {
                                format!(", trim {:+.3} ms", d.trim_ms)
                            } else {
                                String::new()
                            },
                            if d.installed {
                                ""
                            } else {
                                " (not installed: removed since)"
                            }
                        );
                    }
                    for w in &c.warnings {
                        println!("  warning: {w}");
                    }
                }
            }
        }
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
    if let Some(a) = &check.advice
        && a.applied
    {
        println!("applied where it builds: {}", a.message);
    }
    for n in &check.notes {
        match &n.plays {
            Some(plays) => println!(
                "{} Hz plays {}: {}",
                n.source_rate,
                plays_name(plays),
                n.message
            ),
            None => println!("{} Hz: {}", n.source_rate, n.message),
        }
    }
    if let Some(a) = &check.advice {
        for f in &a.fixes {
            println!("fix: {}", f.label);
        }
        for d in &a.details {
            println!("detail {} Hz ({}): {}", d.source_rate, d.ratio, d.message);
        }
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
    for n in &check.notes {
        println!("note {}: {}", n.path, n.message);
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
        // A fixed level (`hardware_db`), or where an earlier mode left the
        // card (`software` without one): the status does not say which.
        extra.push(format!("hardware at {hw:.1} dB"));
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
    println!("alignment:  {}", format_alignment(o));
    if o.master.rate_hz > 0 {
        println!("main:       {}", format_path_latency(&o.master));
    }
    for f in &o.followers {
        println!("follower:   {}", format_follower(f));
        if f.latency.rate_hz > 0 {
            println!("            {}", format_path_latency(&f.latency));
        }
    }
}

fn format_alignment(o: &OutputsStatus) -> String {
    let mut s = format!("{:.0} ms on the main output", o.alignment_ms);
    match (o.alignment_need_ms, o.alignment_shrink_ms) {
        (Some(need), Some(to)) => {
            s += &format!(
                " (the followers need {need:.1} ms: down to {to:.0} ms at the next silence)"
            )
        }
        (Some(need), None) => s += &format!(" (the followers need {need:.1} ms)"),
        (None, Some(to)) => s += &format!(" (down to {to:.0} ms at the next silence)"),
        (None, None) => {}
    }
    s
}

fn format_path_latency(l: &audivane_protocol::ipc::PathLatency) -> String {
    format!(
        "latency {:.0} frames ({:.0} µs) = chain {:.0} + compensation {:.0} + alignment {:.0} \
         frames at {} Hz",
        l.total_frames,
        l.total_us,
        l.chain_frames,
        l.compensation_frames,
        l.alignment_frames,
        l.rate_hz
    )
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
    if !f.own_chain.is_empty() {
        s += &format!(", own chain: {} differs", f.own_chain.join(", "));
    }
    let level = f.level_db.unwrap_or(f.volume_db);
    s += &format!(
        ", trim {:+.2} ms, volume {level:.1} dB{}",
        f.trim_ms,
        if f.follow_volume {
            ", follows the main volume"
        } else {
            ""
        }
    );
    if f.output_channels.is_some() || f.device_channels > 2 {
        let pair = f.output_channels.unwrap_or(OutputChannels::DEFAULT);
        s += &format!(", left/right on {pair}");
        if f.device_channels > 0 {
            s += &format!(" of {}", f.device_channels);
        }
    }
    if f.device_rate > 0 {
        s += &format!(
            ", {:+.2} ppm, error {:.2} frames, {} Hz × {} ({:.1} ms), latency {:.1} ms, skips {}, xruns {}",
            f.ppm,
            f.phase_error_frames,
            f.device_rate,
            f.period,
            f64::from(f.period) * 1e3 / f64::from(f.device_rate),
            f.latency_ms,
            f.skips,
            f.xruns
        );
    }
    if let Some(n) = f.need_ms {
        s += &format!(", needs {n:.1} ms of alignment");
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
    if !status.unresolved_devices.is_empty() {
        println!(
            "held:       identical DACs changed USB ports; which is which? \
             (`devices assign OUTPUT DEVICE ...`)"
        );
        for u in &status.unresolved_devices {
            println!(
                "            {} was on {}; now one of {}",
                u.output,
                u.identity
                    .port_chain()
                    .map_or_else(|| u.identity.to_string(), |c| format!("USB port {c}")),
                u.candidates.join(", ")
            );
        }
    }
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
    let conv = if status.conv_latency_ms > 0.0 {
        format!(" (convolution {:.1} ms)", status.conv_latency_ms)
    } else {
        String::new()
    };
    println!(
        "buffers:    quantum {} frames, latency {:.1} ms{conv}",
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
    if let Some(now) = &status.upsampler_fallback_now
        && let Some(text) = rate_fallback_text(now)
    {
        println!(
            "upsampler:  plays {text} at {} instead of the requested filter",
            khz(now.source_rate)
        );
    }
    if let Some(line) = per_rate_line(&status.upsampler_rates) {
        println!("per rate:   {line}");
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

fn parse_preset_part(text: &str) -> Result<PresetPart, String> {
    PresetPart::from_name(text).ok_or_else(|| {
        let names: Vec<&str> = PresetPart::ALL.iter().map(|part| part.name()).collect();
        format!("unknown part `{text}` (one of {})", names.join(", "))
    })
}

fn preset_summary(preset: &audivane_protocol::ipc::PresetInfo) -> String {
    let mut parts = Vec::new();
    // A global preset holding some parts only names them.
    if preset.part.is_none() && preset.parts.len() < PresetPart::ALL.len() {
        let names: Vec<&str> = preset.parts.iter().map(|part| part.name()).collect();
        parts.push(names.join("+"));
    }
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
        "{:<28} {:<10} {:<28} {:<9} {:<9} ACTIVE",
        "DEVICE", "CARD", "NAME", "KIND", "CHANNELS"
    );
    for device in devices {
        let name = match &device.suffix {
            Some(suffix) => format!("{} ({suffix})", device.name),
            None => device.name.clone(),
        };
        let channels: Vec<String> = device.channels.iter().map(u32::to_string).collect();
        println!(
            "{:<28} {:<10} {:<28} {:<9} {:<9} {}",
            device.id,
            truncate(&device.card, 10),
            truncate(&name, 28),
            if device.hdmi {
                "hdmi"
            } else if device.loopback {
                "loopback"
            } else {
                "analog"
            },
            if channels.is_empty() {
                "-".to_string()
            } else {
                channels.join(",")
            },
            if device.active { "yes" } else { "" }
        );
    }
    Ok(())
}

/// `device hw:CARD=USB,DEV=0, opens at 8 channels (takes 8), left/right on 2/3`.
fn device_line(
    config: &EngineConfig,
    caps: Option<&audivane_protocol::ipc::DeviceCapabilities>,
) -> String {
    let pair = config.output_channels.unwrap_or(OutputChannels::DEFAULT);
    let mut s = format!("device {}", config.device);
    if let Some(c) = caps.filter(|c| c.device == config.device) {
        let takes = if c.channels_min == c.channels_max {
            c.channels_min.to_string()
        } else {
            format!("{}-{}", c.channels_min, c.channels_max)
        };
        s += &format!(", opens at {} channels (takes {takes})", c.channels);
    }
    s += &format!(", left/right on {pair}");
    s
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

fn current_harmonics(client: &mut Client) -> CtlResult<Option<HarmonicsConfig>> {
    let cfg: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
    Ok(cfg.chain.harmonics)
}

/// Set one layer: replacing the stage's layers, or added to them.
fn set_harmonics_layer(
    client: &mut Client,
    layer: HarmonicsLayer,
    add: bool,
    no_dc_block: bool,
) -> CtlResult<()> {
    let mut cfg = match current_harmonics(client)? {
        Some(h) if add => HarmonicsConfig { enabled: true, ..h },
        _ => HarmonicsConfig {
            system: None,
            enabled: true,
            layers: Vec::new(),
            dc_block: true,
            routing: Default::default(),
        },
    };
    cfg.layers.push(layer);
    if no_dc_block {
        cfg.dc_block = false;
    }
    client.call(Call::SetHarmonics(Some(cfg.clone())))?;
    println!("harmonics set: {}", summarize_harmonics(Some(&cfg)));
    Ok(())
}

fn set_harmonics_preset(client: &mut Client, id: &str, args: HarmonicsLayerArgs) -> CtlResult<()> {
    let Some(preset) = audivane_protocol::harmonics_preset(id) else {
        let ids: Vec<&str> = audivane_protocol::harmonics_presets()
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        bail!(
            "unknown harmonics preset {id:?} (one of: {})",
            ids.join(", ")
        );
    };
    let mut layer = preset.to_layer();
    let tilting = args.tilt_low.is_some() || args.tilt_high.is_some() || args.tilt_even;
    let dynamic = args.sag.is_some() || args.bias.is_some();
    match &mut layer {
        HarmonicsLayer::Chebyshev(l) => {
            l.drive_db = args.drive_db.unwrap_or(l.drive_db);
            l.tilt = args.tilt(l.tilt.take().map(|t| *t)).map(Box::new);
            l.sag = args.sag.or(l.sag);
            l.bias = args.bias.or(l.bias);
        }
        _ if args.drive_db.is_some() || tilting || dynamic => {
            bail!("--drive-db, --tilt-*, --sag and --bias apply to Chebyshev layers only");
        }
        _ => {}
    }
    if let Some(amount) = args.amount {
        match layer.amount_mut() {
            Some(a) => *a = amount,
            None => bail!("--amount applies to Chebyshev and hysteresis layers only"),
        }
    }
    set_harmonics_layer(client, layer, args.add, args.no_dc_block)
}

impl HarmonicsLayerArgs {
    /// `base` (a preset's tilt) with the `--tilt-*` options applied.
    fn tilt(&self, base: Option<HarmonicsTilt>) -> Option<HarmonicsTilt> {
        if self.tilt_low.is_none() && self.tilt_high.is_none() && !self.tilt_even {
            return base;
        }
        let mut t = base.unwrap_or_default();
        if let Some(s) = self.tilt_low {
            t.low = s;
        }
        if let Some(s) = self.tilt_high {
            t.high = s;
        }
        t.low_even |= self.tilt_even;
        Some(t)
    }
}

/// `V` or `V:ATTACK_MS:RELEASE_MS` (times from `default`).
fn parse_envelope(text: &str, default: (f64, f64)) -> Result<(f64, f64, f64), String> {
    let num = |v: &str| {
        v.trim()
            .parse::<f64>()
            .map_err(|_| format!("not a number: {v:?}"))
    };
    let parts: Vec<&str> = text.split(':').collect();
    match parts[..] {
        [v] => Ok((num(v)?, default.0, default.1)),
        [v, a, r] => Ok((num(v)?, num(a)?, num(r)?)),
        _ => Err(format!(
            "expected VALUE or VALUE:ATTACK_MS:RELEASE_MS, got {text:?}"
        )),
    }
}

/// `--sag DB[:ATTACK_MS:RELEASE_MS]`, e.g. `2` or `2:10:300`.
fn parse_sag(text: &str) -> Result<HarmonicsSag, String> {
    let d = HarmonicsSag::default();
    let (depth_db, attack_ms, release_ms) = parse_envelope(text, (d.attack_ms, d.release_ms))?;
    let sag = HarmonicsSag {
        depth_db,
        attack_ms,
        release_ms,
    };
    sag.validate()?;
    Ok(sag)
}

/// `--bias DEPTH[:ATTACK_MS:RELEASE_MS]`, e.g. `0.1` or `-0.1:300:2000`.
fn parse_bias(text: &str) -> Result<HarmonicsBias, String> {
    let d = HarmonicsBias::default();
    let (depth, attack_ms, release_ms) = parse_envelope(text, (d.attack_ms, d.release_ms))?;
    let bias = HarmonicsBias {
        depth,
        attack_ms,
        release_ms,
    };
    bias.validate()?;
    Ok(bias)
}

/// `HZ:DB[:SLOPE]`, e.g. `60:12` or `60:12:24` (slope in dB/oct).
/// `--bass-eq HZ:DB`: a tilt shelf without a slope (the pair is fixed at
/// 12 dB/oct).
fn parse_bass_eq(text: &str) -> Result<TiltShelf, String> {
    let shelf = parse_tilt_shelf(text)?;
    if shelf.slope.is_some() {
        return Err(format!(
            "expected HZ:DB (the bass EQ is 12 dB/oct), got {text:?}"
        ));
    }
    Ok(shelf)
}

fn parse_tilt_shelf(text: &str) -> Result<TiltShelf, String> {
    let num = |v: &str| {
        v.trim()
            .parse::<f64>()
            .map_err(|_| format!("not a number: {v:?}"))
    };
    let parts: Vec<&str> = text.split(':').collect();
    let (hz, db, slope) = match parts[..] {
        [hz, db] => (hz, db, None),
        [hz, db, slope] => (hz, db, Some(slope)),
        _ => {
            return Err(format!(
                "expected HZ:DB or HZ:DB:SLOPE (e.g. 60:12 or 60:12:24), got {text:?}"
            ));
        }
    };
    let slope = match slope {
        None => None,
        Some(v) => {
            let v = v.trim();
            match v.parse::<u8>() {
                Ok(s) if TILT_SLOPES.contains(&s) => Some(s),
                _ => return Err(format!("slope must be 6, 12, 18 or 24 dB/oct, got {v:?}")),
            }
        }
    };
    Ok(TiltShelf {
        hz: num(hz)?,
        db: num(db)?,
        slope,
    })
}

/// `, 24 dB/oct` for a shelf whose slope is set.
fn slope_text(s: &TiltShelf) -> String {
    s.slope.map(|v| format!(", {v} dB/oct")).unwrap_or_default()
}

/// A tilt in words: "tilt low +12 dB below 60 Hz (odd), high +6 dB above
/// 6000 Hz, 1 order overridden".
fn summarize_tilt(t: &HarmonicsTilt) -> Option<String> {
    if t.is_flat() {
        return None;
    }
    let mut parts = Vec::new();
    if t.low.db != 0.0 {
        let which = if t.low_even {
            "all orders"
        } else {
            "odd orders"
        };
        parts.push(format!(
            "low {:+.1} dB below {} Hz ({which}{})",
            t.low.db,
            t.low.hz,
            slope_text(&t.low)
        ));
    }
    if t.high.db != 0.0 {
        let slope = t
            .high
            .slope
            .map(|v| format!(" ({v} dB/oct)"))
            .unwrap_or_default();
        parts.push(format!(
            "high {:+.1} dB above {} Hz{slope}",
            t.high.db, t.high.hz
        ));
    }
    let n = t.orders.iter().filter(|o| o.is_some()).count();
    if n > 0 {
        parts.push(format!(
            "{n} order{} overridden",
            if n == 1 { "" } else { "s" }
        ));
    }
    Some(format!("tilt {}", parts.join(", ")))
}

fn summarize_harmonics(h: Option<&HarmonicsConfig>) -> String {
    let Some(h) = h else {
        return "off".to_string();
    };
    if !h.enabled {
        return "off".to_string();
    }
    let mut parts: Vec<String> = h.layers.iter().map(describe_layer).collect();
    if parts.is_empty() {
        parts.push("no layers".to_string());
    }
    match h.routing {
        HarmonicsRouting::Series => parts.push("in series".to_string()),
        HarmonicsRouting::Parallel => {}
    }
    if !h.dc_block {
        parts.push("no dc-block".to_string());
    }
    // The level change hardly depends on the rate (DC blocker, flux filters).
    if let Ok(stage) = audivane_dsp_core::harmonics::Harmonics::try_new(h, 192_000.0, 1) {
        parts.push(format!(
            "peak {:+.2} dB, {:+.2} dB small-signal",
            stage.peak_gain_db(),
            stage.small_signal_gain_db()
        ));
    }
    parts.join("; ")
}

/// A layer's name (its preset, marked when modified) and parameters.
fn describe_layer(l: &HarmonicsLayer) -> String {
    let name = match l.preset() {
        Some(id) if l.is_modified() == Some(true) => format!("{id} (modified)"),
        Some(id) => id.to_string(),
        None => match l {
            HarmonicsLayer::Chebyshev(_) => "custom".to_string(),
            HarmonicsLayer::Flux(_) => "flux".to_string(),
            HarmonicsLayer::Hysteresis(_) => "hysteresis".to_string(),
        },
    };
    let off = if l.enabled() { "" } else { ", off" };
    format!("{name} ({}{off})", summarize_layer(l))
}

/// One layer's parameters.
fn summarize_layer(l: &HarmonicsLayer) -> String {
    match l {
        HarmonicsLayer::Chebyshev(c) => {
            let mut parts: Vec<String> = c
                .levels_db
                .iter()
                .enumerate()
                .filter_map(|(i, level)| level.map(|v| format!("H{} {v:.1} dB", i + 2)))
                .collect();
            if parts.is_empty() {
                parts.push("no orders".to_string());
            }
            if c.signs != HARMONICS_DEFAULT_SIGNS {
                parts.push(format!(
                    "signs {} (H2..H9)",
                    format_harmonic_signs(&c.signs)
                ));
            }
            if c.invert_even {
                parts.push("even orders inverted".to_string());
            }
            if c.drive_db != 0.0 {
                parts.push(format!("drive {:.1} dB", c.drive_db));
            }
            if c.amount != 1.0 {
                parts.push(format!("amount {:.2}", c.amount));
            }
            if let Some(t) = c.tilt.as_deref().and_then(summarize_tilt) {
                parts.push(t);
            }
            if let Some(sag) = c.sag.filter(HarmonicsSag::is_on) {
                parts.push(format!(
                    "sag {:.1} dB ({}/{} ms)",
                    sag.depth_db, sag.attack_ms, sag.release_ms
                ));
            }
            if let Some(bias) = c.bias.filter(HarmonicsBias::is_on) {
                parts.push(format!(
                    "bias {:+.3} ({}/{} ms)",
                    bias.depth, bias.attack_ms, bias.release_ms
                ));
            }
            parts.join(", ")
        }
        HarmonicsLayer::Flux(f) => format!(
            "flux, corner {} Hz, boost {} dB, H3 {:.1} dBc at {} Hz",
            f.corner_hz, f.max_boost_db, f.target_h3_db, f.calibration_hz
        ),
        HarmonicsLayer::Hysteresis(h) => {
            let mut s = format!(
                "hysteresis, sat {:.2}, width {:.2}, H3 {:.1} dBc at {} Hz",
                h.saturation, h.width, h.target_h3_db, h.calibration_hz
            );
            if h.amount != 1.0 {
                s.push_str(&format!(", amount {:.2}", h.amount));
            }
            if let Some(eq) = h.active_bass_eq() {
                s.push_str(&format!(", bass EQ +{} dB below {} Hz", eq.db, eq.hz));
            }
            if h.asymmetry() != 0.0 {
                s.push_str(&format!(", asymmetry {:+.2}", h.asymmetry()));
            }
            s
        }
    }
}

fn print_harmonics(h: Option<&HarmonicsConfig>) {
    let Some(h) = h.filter(|h| h.enabled) else {
        println!("harmonics: off");
        return;
    };
    let n = h.layers.len();
    let mut line = format!("harmonics: on, {n} layer{}", if n == 1 { "" } else { "s" });
    match h.routing {
        HarmonicsRouting::Series => line.push_str(", in series"),
        HarmonicsRouting::Parallel => {}
    }
    if !h.dc_block {
        line.push_str(", no dc-block");
    }
    if let Ok(stage) = audivane_dsp_core::harmonics::Harmonics::try_new(h, 192_000.0, 1) {
        line.push_str(&format!(
            ", peak {:+.2} dB, {:+.2} dB small-signal",
            stage.peak_gain_db(),
            stage.small_signal_gain_db()
        ));
    }
    println!("{line}");
}

fn print_harmonics_readout(r: &audivane_protocol::ipc::HarmonicsReadout) {
    println!(
        "readouts at {} Hz: level change on music {:+.2} dB",
        r.rate_hz, r.level_change_db
    );
    println!(
        "  peak gain {:+.2} dB (worst case at full scale; the limiter and the output headroom handle what goes over)",
        r.peak_gain_db
    );
    for b in &r.bias_limited {
        println!(
            "  layer {}: bias limited to {:+.3} of {:+.3} (the curve's high orders would raise the peak more than 1 dB)",
            b.layer + 1,
            b.reach,
            b.depth
        );
    }
    for p in &r.points {
        println!(
            "  {:>4} dBFS {:>5} Hz: THD {} ({:.1} dB), GedLee Gm {:.4}",
            p.input_dbfs,
            p.freq_hz,
            fmt_percent(100.0 * 10f64.powf(p.thd_db / 20.0)),
            p.thd_db,
            p.gedlee_gm
        );
    }
}

fn fmt_percent(p: f64) -> String {
    if p >= 1.0 {
        format!("{p:.2} %")
    } else if p >= 0.01 {
        format!("{p:.3} %")
    } else {
        format!("{p:.1e} %")
    }
}

/// Warning line when the stage's measured aliasing at the DAC rate
/// `output_rate` (0 = unknown: no line) is above the threshold
/// ([`audivane_dsp_core::harmonics::aliasing`]), with the clean rate of
/// each tape layer below it. The stage applies it anyway.
fn harmonics_alias_note(h: &HarmonicsConfig, output_rate: u32) -> Option<String> {
    if output_rate == 0 {
        return None;
    }
    let rate = f64::from(output_rate);
    let a = audivane_dsp_core::harmonics::aliasing(h, rate).ok()?;
    if !a.warn {
        return None;
    }
    let mut lines = vec![format!(
        "warning: a 0 dBFS {:.1} kHz tone folds back {:.1} dBc of aliasing into the audio band at \
         {output_rate} Hz output (loudest {:.1} dBc at {:.0} Hz); upsample further to lower it",
        a.tone_hz / 1e3,
        a.level_db,
        a.worst_db,
        a.worst_hz
    )];
    for (i, clean) in audivane_dsp_core::harmonics::below_clean_rate(h, rate) {
        let kind = match &h.layers[i] {
            HarmonicsLayer::Hysteresis(_) => "tape hysteresis",
            _ => "it",
        };
        lines.push(format!(
            "warning: layer {} ({kind}) aliases strongly below {} Hz output",
            i + 1,
            clean.round()
        ));
    }
    Some(lines.join("\n"))
}

fn format_harmonic_signs(signs: &[HarmonicSign]) -> String {
    signs
        .iter()
        .map(|s| match s {
            HarmonicSign::Positive => '+',
            HarmonicSign::Negative => '-',
        })
        .collect()
}

/// `--signs`: one `+` or `-` per order from H2; the rest keep the default.
fn parse_harmonic_signs(text: &str) -> Result<[HarmonicSign; HARMONICS_ORDERS], String> {
    let mut signs = HARMONICS_DEFAULT_SIGNS;
    if text.is_empty() || text.chars().count() > HARMONICS_ORDERS {
        return Err(format!(
            "expected 1 to {HARMONICS_ORDERS} signs (`+` or `-`, from H2 up), got {text:?}"
        ));
    }
    for (slot, c) in signs.iter_mut().zip(text.chars()) {
        *slot = match c {
            '+' => HarmonicSign::Positive,
            '-' => HarmonicSign::Negative,
            other => return Err(format!("sign must be `+` or `-`, got {other:?}")),
        };
    }
    Ok(signs)
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
    // little with the rate), and its worst case, which the limiter or the
    // output headroom has to cover.
    let (hz, assumed) = match rate {
        XfRate::Active(r) => (r, String::new()),
        XfRate::Assumed(r) => (r, format!(", assuming {r} Hz")),
    };
    let level = audivane_dsp_core::crossfeed::Crossfeed::try_new(cf, f64::from(hz), 2)
        .map(|stage| {
            format!(
                ", level {:+.2} dB, peak {:+.2} dB{assumed}",
                stage.level_db(),
                stage.peak_gain_db()
            )
        })
        .unwrap_or_default();
    let width = if cf.width_active() {
        let side = if cf.width > 0.0 {
            format!("side {:+.1} dB", 20.0 * cf.width.log10())
        } else {
            "mono".to_string()
        };
        let shape = match cf.width_shape {
            StereoWidthShape::Flat => String::new(),
            StereoWidthShape::AboveBass => format!(" above {:.0} Hz", cf.width_corner_hz),
        };
        format!("width {:.2}: {side}{shape}", cf.width)
    } else {
        String::new()
    };
    let level = if width.is_empty() {
        level
    } else {
        format!(", {width}{level}")
    };
    if cf.mode == CrossfeedMode::None {
        return if cf.width_active() {
            format!("no crossfeed ({}", &level[2..]) + ")"
        } else {
            "off (no crossfeed, width 1)".to_string()
        };
    }
    match cf.mode {
        CrossfeedMode::Bs2bDefault => format!("bs2b default ({fc:.0} Hz, {feed:.1} dB{level})"),
        CrossfeedMode::Bs2bChuMoy => format!("bs2b Chu Moy ({fc:.0} Hz, {feed:.1} dB{level})"),
        CrossfeedMode::Bs2bJanMeier => {
            format!("bs2b Jan Meier ({fc:.0} Hz, {feed:.1} dB{level})")
        }
        CrossfeedMode::Bs2bCustom => format!("bs2b custom ({fc:.0} Hz, {feed:.1} dB{level})"),
        CrossfeedMode::Linkwitz => format!("linkwitz ({fc:.1} Hz, {feed:.1} dB{level})"),
        CrossfeedMode::None => unreachable!("handled above"),
    }
}

/// A section's bands for the status line: "22 bands" when all are on,
/// "0 of 22 bands on" when some are switched off.
fn bands_on(bands: &[Band]) -> String {
    let on = bands.iter().filter(|b| b.enabled).count();
    if on == bands.len() {
        band_count(bands.len())
    } else {
        format!("{on} of {}", band_count(bands.len())) + " on"
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
        parts.push(format!("L+R: {}", bands_on(&eq.bands)));
    } else {
        parts.push("L+R: off".to_string());
    }
    if let Some(left) = &eq.left {
        if left.enabled {
            parts.push(format!("L: {}", bands_on(&left.bands)));
        } else {
            parts.push("L: off".to_string());
        }
    }
    if let Some(right) = &eq.right {
        if right.enabled {
            parts.push(format!("R: {}", bands_on(&right.bands)));
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
            let label = crate::conv_args::conv_label(c);
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

fn format_tone(tone: &audivane_protocol::ToneConfig) -> String {
    format!(
        "bass {:+.1} dB, treble {:+.1} dB",
        tone.bass_db, tone.treble_db
    )
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
    if let Some(tone) = eq.tone.as_ref().filter(|t| t.is_active()) {
        println!("  tone: {}", format_tone(tone));
    }
    if let Some(high) = &eq.tilt_high {
        println!("  treble tilt: {}", format_tilt(high));
    }
}

/// The `notes` of a mutation's reply, one per line.
fn print_notes(reply: &Value) {
    for note in reply
        .get("notes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        println!("  note: {note}");
    }
}

/// The convolution warnings of follower `device`'s own chain.
fn follower_warnings(
    warnings: &[audivane_protocol::ipc::ConvWarning],
    device: &str,
) -> Vec<String> {
    warnings
        .iter()
        .filter(|w| w.device.as_deref() == Some(device))
        .map(|w| w.message.clone())
        .collect()
}

/// The convolution guards' settings and the warnings status reports.
fn print_conv_guards(client: &mut Client, cfg: &EngineConfig) -> CtlResult<()> {
    let safety = cfg.convolution_safety;
    println!(
        "guards: boost cap {:+.1} dB, subsonic limit {} Hz",
        safety.max_boost_db, safety.subsonic_hz
    );
    let status: Status = serde_json::from_value(client.call(Call::Status)?)?;
    if status.conv_warnings.is_empty() {
        println!("warnings: none");
    }
    for w in &status.conv_warnings {
        println!("warning: {}", w.message);
    }
    Ok(())
}

fn print_conv_list(convs: &[ConvolutionConfig]) {
    if convs.is_empty() {
        println!("convolution: off");
        return;
    }
    for (i, c) in convs.iter().enumerate() {
        let state = if c.enabled { "on " } else { "off" };
        let files = crate::conv_args::conv_files(c);
        let label = match &c.name {
            Some(n) => format!("{n} ({files})"),
            None => files,
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

/// The side playing; `hidden` while a blind test runs.
/// The frequencies `conv response` prints, Hz.
const CONV_RESPONSE_AT: [f64; 7] = [20.0, 50.0, 100.0, 500.0, 1_000.0, 5_000.0, 15_000.0];

fn conv_curve_name(c: &audivane_protocol::ipc::ConvResponseCurve) -> String {
    let ch = |n: u32| match n {
        0 => "L".to_string(),
        1 => "R".to_string(),
        n => format!("ch{n}"),
    };
    match (c.input, c.output) {
        (Some(i), Some(o)) if i == o => ch(o),
        (Some(i), Some(o)) => format!("{}→{}", ch(i), ch(o)),
        (None, Some(o)) => ch(o),
        _ => "all".into(),
    }
}

fn print_conv_curve(name: &str, freqs: &[f32], c: &audivane_protocol::ipc::ConvResponseCurve) {
    let at = |f: f64| {
        freqs
            .iter()
            .enumerate()
            .min_by(|a, b| {
                (f64::from(*a.1) - f)
                    .abs()
                    .total_cmp(&(f64::from(*b.1) - f).abs())
            })
            .map(|(i, _)| i)
    };
    let mut cells = Vec::new();
    for f in CONV_RESPONSE_AT {
        if let Some(i) = at(f).filter(|&i| (f64::from(freqs[i]) / f).log2().abs() < 0.1) {
            cells.push(format!("{:+.1}", c.mag_db[i]));
        } else {
            cells.push("-".into());
        }
    }
    let (lo, hi) = c
        .mag_db
        .iter()
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &m| {
            (lo.min(m), hi.max(m))
        });
    println!(
        "  {name:<28} {}   range {lo:+.1}..{hi:+.1} dB",
        cells
            .iter()
            .map(|c| format!("{c:>6}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

fn print_conv_response(r: &audivane_protocol::ipc::ConvResponse) {
    let who = r.device.as_deref().unwrap_or("master");
    let side = match r.side {
        AbSide::A => "",
        AbSide::B => ", A/B side B",
    };
    println!(
        "convolution response ({who}{side}) at {} Hz: combined {} taps",
        r.rate_hz, r.combined_taps
    );
    if r.items.is_empty() {
        println!("  no enabled filters");
        return;
    }
    let head: Vec<String> = CONV_RESPONSE_AT
        .iter()
        .map(|&f| {
            if f >= 1000.0 {
                format!("{:>5}k", f / 1000.0)
            } else {
                format!("{f:>6}")
            }
        })
        .collect();
    println!("  {:<28} {} (dB)", "Hz", head.join(" "));
    for item in &r.items {
        for c in &item.curves {
            let name = format!("[{}] {} {}", item.index, item.label, conv_curve_name(c));
            print_conv_curve(&name, &r.freqs_hz, c);
        }
    }
    for c in &r.outputs {
        print_conv_curve(&format!("combined {}", conv_curve_name(c)), &r.freqs_hz, c);
    }
}

fn side_name(side: Option<AbSide>) -> &'static str {
    match side {
        Some(AbSide::A) => "A",
        Some(AbSide::B) => "B",
        None => "hidden",
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
            // Bands, tone knobs and preamp off; the tilt (its own stage) stays.
            let active = a.eq.as_ref().is_some_and(|eq| {
                eq.preamp_db != 0.0
                    || (eq.linked_enabled && eq.bands.iter().any(|b| b.enabled))
                    || eq.left.as_ref().is_some_and(ChannelEq::is_active)
                    || eq.right.as_ref().is_some_and(ChannelEq::is_active)
                    || eq.tone.is_some_and(|t| t.is_active())
            });
            if !active {
                return Err("EQ is off in chain A; nothing to compare".into());
            }
            Ok((
                json!({"eq": {"preamp_db": 0.0, "bands": [], "left": null, "right": null, "tone": null}}),
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
        && let Some(m) = editable_config(k)
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
                passband_text(&m.band),
                m.band.stopband_start
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
                passband_text(&m.band),
                m.band.stopband_start,
                m.band.attenuation_db
            ),
        };
    }
    let based = match &stored.based_on {
        Some(name) => format!(", based on {name}"),
        None => String::new(),
    };
    format!("{target} (custom {}{based})", stored.design.kind_name())
}

/// A band's passband end as `upsample` prints it: the fraction, or the
/// frequency and the fraction it falls back to.
fn passband_text(band: &audivane_protocol::BandSpec) -> String {
    match band.passband_end_hz {
        Some(hz) => format!("{hz} Hz ({} where it does not fit)", band.passband_end),
        None => band.passband_end.to_string(),
    }
}

/// The Custom builder's fields of a `standard` design for editing: the
/// stepped values at 44.1 kHz, the band as the design holds it (a passband
/// end in Hz kept, not converted).
fn editable_config(
    k: &audivane_protocol::design::KernelDesign,
) -> Option<audivane_dsp_core::design::paths::standard::ManualConfig> {
    let mut m = audivane_dsp_core::design::paths::standard::legacy_config(k, 44_100).ok()?;
    if let Some(b) =
        audivane_dsp_core::design::band_of(&audivane_protocol::FilterDesign::Kernel(k.clone()))
    {
        m.band = *b;
    }
    Some(m)
}

/// A `--passband-end` value: a fraction of the Nyquist or a frequency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PassbandEnd {
    Fraction(f64),
    Hz(f64),
}

/// `0.908` is a fraction; `20k`, `20kHz`, `20000Hz` and a plain number
/// above 1.5 (the largest band edge) are frequencies.
fn parse_passband_end(s: &str) -> Result<PassbandEnd, String> {
    let t = s.trim().to_ascii_lowercase();
    let (num, scale, unit) = if let Some(n) = t.strip_suffix("khz") {
        (n, 1000.0, true)
    } else if let Some(n) = t.strip_suffix('k') {
        (n, 1000.0, true)
    } else if let Some(n) = t.strip_suffix("hz") {
        (n, 1.0, true)
    } else {
        (t.as_str(), 1.0, false)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("{s:?} is not a fraction (0.908) or a frequency (20k)"))?;
    if !v.is_finite() || v <= 0.0 {
        return Err(format!("{s:?} must be above 0"));
    }
    Ok(if unit || v > 1.5 {
        PassbandEnd::Hz(v * scale)
    } else {
        PassbandEnd::Fraction(v)
    })
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

/// The source rates that fall back ([`Status::upsampler_rates`]), one
/// line: `192 kHz light filter (estimate 171%)`; `None` when the request
/// runs at every rate.
fn per_rate_line(rows: &[audivane_protocol::ipc::RateAdmission]) -> Option<String> {
    let parts: Vec<String> = rows
        .iter()
        .filter_map(|r| Some(format!("{} {}", khz(r.source_rate), rate_fallback_text(r)?)))
        .collect();
    (!parts.is_empty()).then(|| format!("falls back at {}", parts.join(", ")))
}

/// What a source rate plays instead and why (`Recommended (too short here
/// (needs about 1,600 taps))`, `light filter (estimate 171%)`); `None`
/// when it plays the request.
fn rate_fallback_text(r: &audivane_protocol::ipc::RateAdmission) -> Option<String> {
    use audivane_protocol::ipc::{OverloadAction, RateFallbackCause};
    let f = r.fallback.as_ref()?;
    let what = match f.action {
        OverloadAction::Reverted => "last setting that ran",
        OverloadAction::Recommended => "Recommended",
        _ => "light filter",
    };
    let why = match f.cause {
        RateFallbackCause::Estimate => format!("estimate {:.0}%", r.load * 100.0),
        RateFallbackCause::Overload => "overloaded".into(),
        RateFallbackCause::Design => f
            .reason
            .clone()
            .unwrap_or_else(|| "the design does not build here".into()),
    };
    Some(format!("{what} ({why})"))
}

/// A design name as `design check` prints it.
fn plays_name(plays: &str) -> &str {
    match plays {
        "recommended" => "Recommended",
        "recommended_light" => "a lighter Recommended",
        _ => plays,
    }
}

fn apply_volume_settings(v: &mut audivane_protocol::ipc::VolumeConfig, args: &VolumeArgs) {
    let prev_mode = v.mode;
    if let Some(mode) = args.mode {
        v.mode = mode;
    }
    if let Some(OptDb(db)) = args.hardware_db {
        v.hardware_db = db;
    } else if args.mode == Some(audivane_protocol::ipc::VolumeMode::Software)
        && prev_mode != audivane_protocol::ipc::VolumeMode::Software
        && v.hardware_db.is_none()
    {
        // Default the DAC mixer to 0 dB on switching to software so an earlier
        // hardware-driven attenuation isn't doubled; the engine stages the raise.
        v.hardware_db = Some(0.0);
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
}

fn khz(rate: u32) -> String {
    let k = f64::from(rate) / 1000.0;
    if (k - k.round()).abs() < 1e-9 {
        format!("{k:.0} kHz")
    } else {
        format!("{} kHz", format!("{k:.3}").trim_end_matches('0'))
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[derive(clap::Parser, Debug)]
    struct TestCli {
        #[command(subcommand)]
        command: CtlCommand,
    }

    #[test]
    fn hardware_db_above_0_db_is_refused() {
        assert!(parse_hardware_db("0.5").is_err());
        assert!(parse_hardware_db("24").is_err());
        assert_eq!(parse_hardware_db("0").unwrap().0, Some(0.0));
        assert_eq!(parse_hardware_db("-20").unwrap().0, Some(-20.0));
        assert_eq!(parse_hardware_db("none").unwrap().0, None);
    }
}
