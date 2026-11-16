//! `avdsp ctl` — talk to the running daemon over its Unix socket.
//!
//! One subcommand per IPC call. Output is human-readable by default and raw
//! JSON with `--json`. A daemon error reply exits 1 with the daemon's message;
//! a missing daemon exits 3 with a hint.

use anyhow::{Context, Result};
use audivane_protocol::ipc::{
    AbConfig, AbSide, Call, EngineConfig, LevelMatch, RouteTarget, RoutingMode, SinkChoice, Topic,
};
use audivane_protocol::{
    Band, ChainConfig, ChannelEq, ConvolutionConfig, CutoffMode, EqProfile, FilterType, RateTarget,
    UpsamplerConfig,
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

#[derive(Subcommand, Debug)]
pub enum CtlCommand {
    /// Show engine status, including passthrough integrity.
    Status,
    /// Read or replace the whole configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
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
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Print the current configuration as JSON.
    Get,
    /// Replace the configuration from a JSON file (`-` reads stdin).
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

#[derive(Args, Debug)]
pub struct UpsampleResponseArgs {
    /// Source (graph) rate; default is the daemon's current graph rate, else
    /// 44100 Hz.
    #[arg(long, value_parser = clap::value_parser!(u32).range(8_000..=1_536_000))]
    pub rate: Option<u32>,
}

#[derive(Args, Debug)]
pub struct UpsampleSetArgs {
    /// Integer ratio of the source rate (e.g. 4).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=64), conflicts_with_all = ["max_rate", "off"])]
    pub ratio: Option<u32>,
    /// Highest output rate to reach with a power-of-two ratio.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with = "off")]
    pub max_rate: Option<u32>,
    /// Disable upsampling.
    #[arg(long)]
    pub off: bool,
    /// Kernel length at the output rate (1 ..= 4M).
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=4_194_304))]
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
}

#[derive(Subcommand, Debug)]
pub enum AbCommand {
    /// Configure chain B from a profile.
    Set {
        #[arg(value_name = "PROFILE")]
        profile: PathBuf,
        /// Level matching: `analytic`, `off`, or a fixed offset in dB.
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
}

impl From<PhaseArg> for audivane_protocol::FilterPhase {
    fn from(value: PhaseArg) -> Self {
        match value {
            PhaseArg::Linear => Self::Linear,
            PhaseArg::Minimum => Self::Minimum,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CutoffModeArg {
    Fraction,
    NyquistStop,
}

impl From<CutoffModeArg> for CutoffMode {
    fn from(value: CutoffModeArg) -> Self {
        match value {
            CutoffModeArg::Fraction => Self::Fraction,
            CutoffModeArg::NyquistStop => Self::NyquistStop,
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
                }
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
                    audivane_dsp_core::biquad::Eq::auto_preamp_db(&profile, f64::from(rate))
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
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                let mut upsampler = current.chain.upsampler.clone();
                let mut changed = false;
                if let Some(ratio) = set.ratio {
                    upsampler.target = RateTarget::Ratio(ratio);
                    changed = true;
                }
                if let Some(max_rate) = set.max_rate {
                    upsampler.target = RateTarget::MaxRate(max_rate);
                    changed = true;
                }
                if set.off {
                    upsampler.target = RateTarget::Off;
                    changed = true;
                }
                if let Some(taps) = set.taps {
                    upsampler.taps = usize::try_from(taps).unwrap_or(usize::MAX);
                    changed = true;
                }
                if let Some(phase) = set.phase {
                    upsampler.phase = phase.into();
                    changed = true;
                }
                if let Some(beta) = set.beta {
                    upsampler.kaiser_beta = beta;
                    changed = true;
                }
                if set.cutoff_mode == Some(CutoffModeArg::NyquistStop) && set.cutoff.is_some() {
                    bail!("--cutoff cannot be combined with --cutoff-mode nyquist-stop");
                }
                if let Some(cutoff) = set.cutoff {
                    upsampler.cutoff = cutoff;
                    if set.cutoff_mode.is_none() {
                        upsampler.cutoff_mode = CutoffMode::Fraction;
                    }
                    changed = true;
                }
                if let Some(mode) = set.cutoff_mode {
                    upsampler.cutoff_mode = mode.into();
                    changed = true;
                }
                if !changed {
                    bail!(
                        "nothing to change: give --ratio, --max-rate, --off or a tuning flag, \
                         or `upsample response`"
                    );
                }
                client.call(Call::SetUpsampler(upsampler.clone()))?;
                println!("{}", describe_upsampler(&upsampler));
            }
        },
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
            AbCommand::Set { profile, level } => {
                let profile = crate::profile::load(&profile)?;
                let current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
                // Chain B must quantize and upsample exactly like chain A.
                let chain_b = ChainConfig {
                    eq: Some(profile),
                    convolution: Vec::new(),
                    upsampler: current.chain.upsampler.clone(),
                    output: current.chain.output,
                };
                let ab = AbConfig {
                    chain_b,
                    level: parse_level(&level)?,
                };
                let value = client.call(Call::SetAb(Some(ab)))?;
                // Chain B is realigned with chain A when needed; say so.
                for note in reply_notes(&value) {
                    println!("note: {note}");
                }
                let status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
                println!(
                    "A/B enabled: side {} (B offset {:+.2} dB)",
                    side_name(status.current),
                    status.offset_db
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
            AbCommand::Off => {
                let value = client.call(Call::SetAb(None))?;
                let status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
                let _ = status;
                println!("A/B off");
            }
        },
        CtlCommand::Rates(args) => {
            let mut current: EngineConfig = serde_json::from_value(client.call(Call::GetConfig)?)?;
            let changing =
                !args.enable.is_empty() || !args.disable.is_empty() || args.default.is_some();
            if changing {
                let rate = &mut current.rate;
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
            }
        }
        CtlCommand::Volume(args) => {
            if args.db.is_none() && args.balance.is_none() && !args.mute && !args.unmute {
                let status: audivane_protocol::ipc::Status =
                    serde_json::from_value(client.call(Call::Status)?)?;
                let volume = match status.volume {
                    Some(v) => v,
                    None => {
                        let current: EngineConfig =
                            serde_json::from_value(client.call(Call::GetConfig)?)?;
                        audivane_protocol::ipc::VolumeStatus {
                            volume_db: current.volume.volume_db,
                            balance_db: current.volume.balance_db,
                            muted: current.volume.muted,
                            hardware: false,
                            min_db: audivane_protocol::ipc::VOLUME_MIN_DB,
                            max_db: audivane_protocol::ipc::VOLUME_MAX_DB,
                        }
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
        _ => None,
    }
}

// ---------------------------------------------------------------- output

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
    println!(
        "upsampler: {} Hz -> {} Hz (x{}), {} taps, {} phase",
        response.source_rate,
        response.output_rate,
        response.ratio,
        response.taps,
        match response.phase {
            audivane_protocol::FilterPhase::Linear => "linear",
            audivane_protocol::FilterPhase::Minimum => "minimum",
        }
    );
    if response.ratio == 1 {
        println!("response:  flat (upsampling off)");
        println!("impulse:   [1.0]");
        return;
    }
    println!(
        "cutoff:    {:.6} of source Nyquist ({:.1} Hz)",
        response.effective_cutoff,
        response.effective_cutoff * f64::from(response.source_rate) / 2.0
    );
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
        "{}  {}  {} Hz -> {} Hz (x{})  {:.1} ms  dsp load {:.1}%{vol}  {}",
        state_name(status.state),
        status.device,
        status.graph_rate,
        status.output_rate,
        status.ratio,
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
        format!("hardware, {:.1}..={:.1} dB", v.min_db, v.max_db)
    } else {
        "digital".to_string()
    };
    let mute = if v.muted { ", MUTED" } else { "" };
    let bal = if bal_db.abs() < 1e-6 {
        "center".to_string()
    } else if bal_db < 0.0 {
        format!("{bal_db:+.1} dB (R {bal_db:.1} dB)")
    } else {
        format!("{bal_db:+.1} dB (L -{bal_db:.1} dB)")
    };
    format!("{vol_db:.1} dB, balance {bal}{mute} [{mode}]")
}

fn print_volume(v: &audivane_protocol::ipc::VolumeStatus) {
    println!("volume: {}", format_volume(v));
}

fn print_status(
    status: &audivane_protocol::ipc::Status,
    config: Option<&EngineConfig>,
) -> CtlResult<()> {
    println!("audivane-dsp {}", status.version);
    println!("state:      {}", state_name(status.state));
    println!("device:     {}", status.device);
    println!(
        "format:     {}",
        status
            .format
            .map(|f| format!("{f:?}").to_lowercase())
            .unwrap_or_else(|| "not negotiated".to_string())
    );
    println!(
        "rate:       {} Hz graph -> {} Hz DAC (x{})",
        status.graph_rate, status.output_rate, status.ratio
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
        let rates: Vec<String> = status.direct_rates.iter().map(u32::to_string).collect();
        if rates.is_empty() {
            println!("direct:     no native rate (Audivane Direct cannot play here)");
        } else {
            println!(
                "direct:     {} Hz bit-perfect (other rates resampled)",
                rates.join(", ")
            );
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
    println!(
        "engine:     dsp {}, load {:.1}%, xruns {}",
        if status.dsp_enabled { "on" } else { "off" },
        status.dsp_load * 100.0,
        status.xruns
    );
    if let Some(error) = &status.error {
        println!("error:      {error}");
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
        Some(ab) if ab.enabled => println!(
            "A/B:        side {}, offset {:+.2} dB",
            side_name(ab.current),
            ab.offset_db
        ),
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
        println!("eq:         {}", summarize_eq(cfg.chain.eq.as_ref()));
        println!("conv:       {}", summarize_conv(&cfg.chain.convolution));
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
    format!("{name} ({})", parts.join(", "))
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
        "{:>6}  {:<20} {:<28} {:>8} {:>3} {:<7} {:<8} {:>5}  {:>5}",
        "NODE", "APP", "STREAM", "RATE", "CH", "FORMAT", "SINK", "VOL", "MUTE"
    );
    for stream in streams {
        println!("{}", stream_row(stream));
    }
    Ok(())
}

fn stream_row(stream: &audivane_protocol::ipc::StreamInfo) -> String {
    format!(
        "{:>6}  {:<20} {:<28} {:>8} {:>3} {:<7} {:<8} {:>5}  {:>5}",
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
        stream
            .volume
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "-".into()),
        if stream.muted { "yes" } else { "no" },
    )
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
        "analytic" | "auto" => Ok(LevelMatch::Analytic),
        "off" | "none" => Ok(LevelMatch::Off),
        other => {
            let offset_db: f64 = other
                .parse()
                .with_context(|| format!("--level must be analytic, off or dB, got {other:?}"))?;
            Ok(LevelMatch::Manual { offset_db })
        }
    }
}

fn print_ab(value: Value) -> CtlResult<()> {
    for note in reply_notes(&value) {
        println!("note: {note}");
    }
    let status: audivane_protocol::ipc::AbStatus = serde_json::from_value(value)?;
    println!(
        "A/B side {} (B offset {:+.2} dB)",
        side_name(status.current),
        status.offset_db
    );
    Ok(())
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

fn describe_upsampler(up: &UpsamplerConfig) -> String {
    let target = match up.target {
        RateTarget::Off => "upsampling off".to_string(),
        RateTarget::Ratio(ratio) => format!("upsampling x{ratio}"),
        RateTarget::MaxRate(max) => format!("upsampling up to {max} Hz"),
    };
    let cutoff = match up.cutoff_mode {
        CutoffMode::Fraction => format!("cutoff {}", up.cutoff),
        CutoffMode::NyquistStop => "stop at Nyquist".to_string(),
    };
    format!(
        "{target} ({} taps, {:?} phase, beta {}, {cutoff})",
        up.taps, up.phase, up.kaiser_beta
    )
}

fn passthrough_reasons(passthrough: &audivane_protocol::ipc::Passthrough) -> String {
    use audivane_protocol::ipc::PassthroughBreak::*;
    passthrough
        .broken_because
        .iter()
        .map(|reason| match reason {
            DspActive => "DSP is on".to_string(),
            Upsampling { ratio } => format!("upsampling x{ratio}"),
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
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A linear PipeWire volume as desktop sliders (pavucontrol, KDE, GNOME)
/// show it: they use a cubic scale, so linear 0.27 reads 65%.
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
            xruns: 0,
            dsp_load: 0.1,
        };
        assert_eq!(clip_line(&meters), "clips [L:1 R:3]");
        assert!(meter_line(&meters).contains("clips [L:1 R:3]"));
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
