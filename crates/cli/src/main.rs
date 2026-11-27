//! `avdsp`: offline WAV processing, filter design, null tests and
//! benchmarks for Audivane DSP.
//!
//! Exit codes: 0 success, 1 a check failed (e.g. `bypass-test` mismatch),
//! 2 bad usage (clap), 3 any other error, 10 `upgrade --check` found a newer
//! release.

mod bench;
mod bypass_test;
mod conv_args;
mod ctl;
mod eqcmd;
mod gen_signal;
mod kernel;
mod level;
mod nulltest;
mod process;
mod profile;
mod render;
mod signal;
mod upgrade;
mod wavio;

use anyhow::Result;
use audivane_protocol::{DitherKind, FilterPhase};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "avdsp",
    version,
    about = "Audivane DSP offline toolkit",
    long_about = "Audivane DSP offline toolkit: run the f64 audio chain on WAV files, \
                  design kernels, import/export EQ profiles, measure level differences, \
                  run null and bit-exact bypass tests, and benchmark real-time headroom.",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Process a WAV file through the DSP chain.
    Process(ProcessArgs),
    /// Play a WAV file at another speed through the varispeed stage.
    Render(RenderArgs),
    /// EQ profile utilities (import, export, response).
    Eq {
        #[command(subcommand)]
        command: EqCommand,
    },
    /// Compute a static A/B level offset (analytic or measured).
    Levelmatch(LevelMatchArgs),
    /// Dump an upsampler kernel for reference tests.
    Kernel(KernelArgs),
    /// Compare two WAV files sample by sample.
    Nulltest(NullTestArgs),
    /// Verify that the passthrough path is bit-exact.
    BypassTest(BypassTestArgs),
    /// Generate a deterministic test signal.
    Gen(GenArgs),
    /// Benchmark the full chain, single threaded.
    Bench(BenchArgs),
    /// Control the running audivane-dsp daemon.
    Ctl(ctl::CtlArgs),
    /// Update a packaged install to the latest release (or --version TAG).
    Upgrade(upgrade::UpgradeArgs),
}

#[derive(clap::Args, Debug)]
pub struct ProcessArgs {
    /// Input WAV (16/24/32-bit integer or 32-bit float).
    pub input: PathBuf,
    /// Output WAV.
    pub output: PathBuf,
    /// Chain config JSON (protocol ChainConfig). CLI flags override it.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Equalizer APO / AutoEQ ParametricEQ.txt profile (left channel if --eq-right is given).
    #[arg(long)]
    pub eq: Option<PathBuf>,
    /// Right-channel EQ profile (creates split L/R EQ).
    #[arg(long)]
    pub eq_right: Option<PathBuf>,
    /// Preamp in dB (negative attenuates).
    #[arg(long, allow_hyphen_values = true)]
    pub preamp_db: Option<f64>,
    /// Set the preamp to minus the peak EQ boost.
    #[arg(long)]
    pub auto_preamp: bool,
    /// Impulse response WAV for convolution (repeatable; combined in series).
    /// `IN:OUT[:CH]=PATH` gives a filter-matrix entry (channel `CH` of the
    /// WAV, else a mono file); a run of entries is one matrix item.
    #[arg(long)]
    pub conv: Vec<PathBuf>,
    /// Integer upsampling ratio (e.g. 4).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with_all = ["max_rate", "out_rate"])]
    pub upsample: Option<u32>,
    /// Highest output rate to reach with a power-of-two ratio.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..), conflicts_with = "out_rate")]
    pub max_rate: Option<u32>,
    /// Resample to exactly this output rate (rational ratios and
    /// decimation included, e.g. 48000 from a 44100 Hz file).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub out_rate: Option<u32>,
    /// Upsampler kernel length at the output rate.
    #[arg(long)]
    pub taps: Option<usize>,
    /// Upsampler phase response.
    #[arg(long, value_enum)]
    pub phase: Option<PhaseArg>,
    /// Kaiser window beta (e.g. 14 for >140 dB stopband).
    #[arg(long)]
    pub beta: Option<f64>,
    /// Cutoff as a fraction of the input Nyquist (1.0 = Fs/2).
    #[arg(long)]
    pub cutoff: Option<f64>,
    /// Dither target bit depth (16..=32).
    #[arg(long, value_parser = clap::value_parser!(u32).range(16..=32))]
    pub bits: Option<u32>,
    /// Dither kind at the final quantizer.
    #[arg(long, value_enum)]
    pub dither: Option<DitherArg>,
    /// Output WAV container.
    #[arg(long, value_enum)]
    pub format: Option<ContainerArg>,
    /// Frames per process call (at most the engine's chain block, see
    /// --quantum).
    #[arg(long, default_value_t = 1024)]
    pub block: usize,
    /// The engine's graph quantum at 48 kHz (its `quantum` setting). The
    /// chain is built with the block the live engine uses at the file's
    /// rate, so the reported latency is what `status.filter_latency_ms`
    /// shows for this chain.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(i64::from(audivane_protocol::ipc::MIN_QUANTUM)..=i64::from(audivane_protocol::ipc::MAX_QUANTUM)))]
    pub quantum: u32,
    /// Remove the reported chain latency from the output.
    #[arg(long)]
    pub trim_latency: bool,
}

#[derive(clap::Args, Debug)]
pub struct RenderArgs {
    /// Input WAV (16/24/32-bit integer or 32-bit float).
    pub input: PathBuf,
    /// Output WAV (32-bit float, the input's rate).
    pub output: PathBuf,
    /// Playback speed, 0.5 to 2 (1.02 = +2 %: tempo and pitch together).
    #[arg(long, default_value_t = 1.0)]
    pub speed: f64,
    /// A speed change during the output: `TIME:SPEED:RAMP_MS` (the ramp
    /// optional), the time in seconds of output. Repeatable. The ramp is never
    /// shorter than the speed envelope allows (0 means as fast as it allows).
    #[arg(long = "change", value_name = "TIME:SPEED[:RAMP_MS]")]
    pub changes: Vec<String>,
    /// Frames per call (1 to 8192).
    #[arg(long, default_value_t = 1024)]
    pub block: usize,
    /// Remove the stage's delay at the start speed from the output.
    #[arg(long)]
    pub trim_latency: bool,
}

/// Output container, independent of the protocol crate's ALSA formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ContainerArg {
    S16,
    S24,
    S32,
    F32,
}

impl From<ContainerArg> for wavio::Container {
    fn from(value: ContainerArg) -> Self {
        match value {
            ContainerArg::S16 => Self::S16,
            ContainerArg::S24 => Self::S24,
            ContainerArg::S32 => Self::S32,
            ContainerArg::F32 => Self::F32,
        }
    }
}

impl ContainerArg {
    /// Default container matching an input format.
    pub fn matching(bits: u32, float: bool) -> Self {
        if float {
            Self::F32
        } else {
            match bits {
                16 => Self::S16,
                24 => Self::S24,
                _ => Self::S32,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ChannelArg {
    Left,
    Right,
}

#[derive(Debug, Subcommand)]
enum EqCommand {
    /// Import an APO/AutoEQ text profile to JSON.
    Import {
        /// Equalizer APO text profile.
        input: PathBuf,
        /// Write the JSON here instead of stdout.
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Export a profile JSON (or APO text) back to APO text.
    Export {
        /// Profile JSON.
        input: PathBuf,
        /// Which channel to export for split L/R profiles.
        #[arg(long, value_enum, default_value_t = ChannelArg::Left)]
        channel: ChannelArg,
    },
    /// Print the complex frequency response of a profile.
    Response {
        /// APO text or profile JSON.
        input: PathBuf,
        /// Sample rate in Hz.
        #[arg(long)]
        rate: u32,
        /// Number of log-spaced points (20 Hz .. min(20 kHz, Nyquist)).
        #[arg(long, default_value_t = 512)]
        points: usize,
        /// Explicit comma-separated frequencies instead of a log grid.
        #[arg(long)]
        freqs: Option<String>,
        /// Which channel to evaluate for split L/R profiles.
        #[arg(long, value_enum, default_value_t = ChannelArg::Left)]
        channel: ChannelArg,
        /// Emit JSON instead of CSV.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args, Debug)]
pub struct LevelMatchArgs {
    /// Profile A (APO text or profile JSON).
    pub a: PathBuf,
    /// Profile B (APO text or profile JSON).
    pub b: PathBuf,
    /// Sample rate for the analytic offset.
    #[arg(long, default_value_t = 44_100)]
    pub rate: u32,
    /// Measure the offset by processing this WAV through both profiles.
    #[arg(long)]
    pub measure: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
pub struct KernelArgs {
    /// Sample rate the kernel is designed for (metadata only).
    #[arg(long)]
    pub rate: u32,
    /// Integer upsampling ratio.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub ratio: u32,
    /// Kernel length at the output rate.
    #[arg(long)]
    pub taps: usize,
    /// Filter phase.
    #[arg(long, value_enum, default_value_t = PhaseArg::Linear)]
    pub phase: PhaseArg,
    /// Kaiser window beta.
    #[arg(long, default_value_t = 14.0)]
    pub beta: f64,
    /// Cutoff as a fraction of the input Nyquist.
    #[arg(long, default_value_t = 1.0)]
    pub cutoff: f64,
    /// Raw little-endian f64 output.
    #[arg(long)]
    pub out: PathBuf,
    /// Print metadata as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug)]
pub struct NullTestArgs {
    /// Reference WAV.
    pub a: PathBuf,
    /// WAV to compare.
    pub b: PathBuf,
    /// Estimate and remove a whole-sample time offset first.
    #[arg(long)]
    pub align: bool,
    /// Largest offset to search for `--align`, in frames.
    #[arg(long, default_value_t = 8192)]
    pub max_lag: usize,
}

#[derive(clap::Args, Debug)]
pub struct BypassTestArgs {
    /// Integer WAV to push through the passthrough chain.
    pub input: PathBuf,
    /// Frames per process call.
    #[arg(long, default_value_t = 1024)]
    pub block: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum GenKind {
    Sine,
    White,
    Sweep,
    Silence,
}

#[derive(clap::Args, Debug)]
pub struct GenArgs {
    /// Signal type.
    #[arg(value_enum)]
    pub kind: GenKind,
    /// Sample rate in Hz.
    #[arg(long, default_value_t = 44_100)]
    pub rate: u32,
    /// Bit depth of the integer output (16, 24 or 32).
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u32).range(16..=32))]
    pub bits: u32,
    /// Duration in seconds.
    #[arg(long, default_value_t = 1.0)]
    pub seconds: f64,
    /// Sine frequency, sweep start frequency, in Hz.
    #[arg(long, default_value_t = 1000.0)]
    pub freq: f64,
    /// Sweep end frequency in Hz.
    #[arg(long, default_value_t = 20_000.0)]
    pub freq_end: f64,
    /// Peak level in dBFS.
    #[arg(long, default_value_t = -6.0, allow_hyphen_values = true)]
    pub amp_db: f64,
    /// Deterministic noise seed.
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Channel count.
    #[arg(long, default_value_t = 1)]
    pub channels: usize,
    /// Output WAV.
    pub out: PathBuf,
}

#[derive(clap::Args, Debug)]
pub struct BenchArgs {
    /// Source sample rate in Hz.
    #[arg(long, default_value_t = 44_100)]
    pub rate: u32,
    /// Integer upsampling ratio.
    #[arg(long, default_value_t = 4)]
    pub ratio: usize,
    /// Upsampler kernel length at the output rate.
    #[arg(long, default_value_t = 65_536)]
    pub taps: usize,
    /// Also run convolution with a synthetic IR of this many taps.
    #[arg(long)]
    pub conv_taps: Option<usize>,
    /// EQ profile to run.
    #[arg(long)]
    pub eq: Option<PathBuf>,
    /// Audio seconds to process.
    #[arg(long, default_value_t = 10.0)]
    pub seconds: f64,
    /// Frames per process call.
    #[arg(long, default_value_t = 1024)]
    pub block: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum PhaseArg {
    Linear,
    Minimum,
}

impl From<PhaseArg> for FilterPhase {
    fn from(value: PhaseArg) -> Self {
        match value {
            PhaseArg::Linear => Self::Linear,
            PhaseArg::Minimum => Self::Minimum,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum DitherArg {
    None,
    Tpdf,
    Shaped,
    HeadroomTpdf,
    Rectangular,
    Gaussian,
    HighpassTpdf,
    Feedback1,
    Feedback5,
    Resonant5,
    Resonant9,
    Integrator15,
}

impl From<DitherArg> for DitherKind {
    fn from(value: DitherArg) -> Self {
        match value {
            DitherArg::None => Self::None,
            DitherArg::Tpdf => Self::Tpdf,
            DitherArg::Shaped => Self::Shaped,
            DitherArg::HeadroomTpdf => Self::HeadroomTpdf,
            DitherArg::Rectangular => Self::Rectangular,
            DitherArg::Gaussian => Self::Gaussian,
            DitherArg::HighpassTpdf => Self::HighpassTpdf,
            DitherArg::Feedback1 => Self::Feedback1,
            DitherArg::Feedback5 => Self::Feedback5,
            DitherArg::Resonant5 => Self::Resonant5,
            DitherArg::Resonant9 => Self::Resonant9,
            DitherArg::Integrator15 => Self::Integrator15,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("avdsp: {err:#}");
            3
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

fn run(cli: Cli) -> Result<i32> {
    match cli.command {
        Command::Process(args) => process::run(args),
        Command::Render(args) => render::run(args),
        Command::Eq { command } => eqcmd::run(command),
        Command::Levelmatch(args) => level::run(args),
        Command::Kernel(args) => kernel::run(args),
        Command::Nulltest(args) => nulltest::run(args),
        Command::BypassTest(args) => bypass_test::run(args),
        Command::Gen(args) => gen_signal::run(args),
        Command::Bench(args) => bench::run(args),
        Command::Ctl(args) => ctl::run(args),
        Command::Upgrade(args) => upgrade::run(args),
    }
}
