//! The full processing chain built from a `ChainConfig`:
//! preamp/EQ → convolution → upsampler → (A/B crossfade) → quantizer.
//!
//! Everything that needs memory is allocated in `Chain::new` / `AbChain::new`;
//! `process_f32` is allocation-free. Interleaved f32 comes in (as PipeWire
//! delivers it), planar f64 runs through the stages, and a single quantizer
//! produces interleaved left-justified i32 at the output rate.
//!
//! When `ChainConfig::is_passthrough()` is true the f64 path is skipped
//! entirely and samples go through [`crate::bypass::f32_to_i32`], which is
//! bit-exact for every input that came from an integer sample of 24 bits or
//! less.

use crate::biquad::Eq;
use crate::conv::Convolver;
use crate::dither::{ClipStats, MAX_CLIP_CHANNELS, Quantizer};
use crate::gain::StereoGainRamp;
use crate::resample::{Upsampler, UpsamplerDesign};
use crate::{Stage, db_to_lin};
use audivane_protocol::{ChainConfig, resolve_ratio};
use std::f64::consts::PI;
use std::fmt;

/// The chain keeps its planar scratch in fixed-size stack arrays of channel
/// references, so the channel count has to be bounded to stay allocation-free.
/// 128 is far above any consumer audio interface.
pub const MAX_CHANNELS: usize = 128;

/// Why a chain could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainError {
    /// A chain needs at least one channel.
    ZeroChannels,
    /// More than [`MAX_CHANNELS`] channels.
    TooManyChannels { channels: usize, max: usize },
    /// `max_block` must be at least one frame.
    ZeroBlock,
    /// A/B needs both sides at the same sample rate.
    RateMismatch { a: u32, b: u32 },
    /// A/B needs both sides to have the same channel count.
    ChannelMismatch { a: usize, b: usize },
    /// A/B needs both sides to upsample by the same ratio.
    RatioMismatch { a: usize, b: usize },
    /// A/B has one shared quantizer, so the output configs must match.
    OutputMismatch,
    /// Convolution is enabled but the caller passed no impulse responses.
    MissingIr,
    /// An impulse response is empty.
    EmptyIr,
    /// Combined impulse response exceeds the maximum tap count.
    IrTooLong { taps: usize, max: usize },
    /// Convolution wants one IR (mono) or one per channel.
    IrCount { irs: usize, channels: usize },
    /// The upsampler kernel cannot be designed to its tolerances.
    Upsampler(String),
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => write!(f, "a chain needs at least one channel"),
            Self::TooManyChannels { channels, max } => {
                write!(f, "{channels} channels is more than the supported {max}")
            }
            Self::ZeroBlock => write!(f, "max_block must be at least 1 frame"),
            Self::RateMismatch { a, b } => {
                write!(f, "A/B sample rates differ: {a} Hz vs {b} Hz")
            }
            Self::ChannelMismatch { a, b } => {
                write!(f, "A/B channel counts differ: {a} vs {b}")
            }
            Self::RatioMismatch { a, b } => {
                write!(f, "A/B upsampling ratios differ: {a} vs {b}")
            }
            Self::OutputMismatch => write!(f, "A/B output configs differ"),
            Self::MissingIr => write!(
                f,
                "convolution is enabled but no impulse response was given"
            ),
            Self::EmptyIr => write!(f, "impulse response is empty"),
            Self::IrTooLong { taps, max } => write!(
                f,
                "combined impulse response length {taps} taps exceeds maximum {max}"
            ),
            Self::IrCount { irs, channels } => write!(
                f,
                "convolution needs 1 (mono) or {channels} impulse responses, got {irs}"
            ),
            Self::Upsampler(e) => write!(f, "upsampler: {e}"),
        }
    }
}

impl std::error::Error for ChainError {}

/// Everything needed to build a [`Chain`].
#[derive(Clone, Debug)]
pub struct ChainSpec {
    pub config: ChainConfig,
    pub sample_rate: u32,
    pub channels: usize,
    /// Largest input frame count per `process` call.
    pub max_block: usize,
    /// Combined impulse responses already at `sample_rate` and scaled by each
    /// enabled IR's `gain_db` (loaded/resampled/combined via
    /// [`crate::conv::combine_irs`] by the caller; dsp-core does no I/O).
    /// Required when any entry in `config.convolution` is enabled.
    pub irs: Option<Vec<Vec<f64>>>,
}

/// A `Vec<Vec<f64>>` viewed as `&mut [&mut [f64]]` for the [`Stage`] API,
/// truncated to `frames` per channel, without allocating.
///
/// Declares and shadows `$name` in the caller's scope, so the reference array
/// outlives the statement.
macro_rules! make_chans {
    ($name:ident, $buf:expr, $channels:expr, $frames:expr) => {
        let mut $name: [&mut [f64]; MAX_CHANNELS] =
            std::array::from_fn(|_| &mut [] as &mut [f64]);
        for (slot, chan) in $name.iter_mut().zip($buf.iter_mut()) {
            *slot = &mut chan[..$frames];
        }
        let $name = &mut $name[..$channels];
    };
}

/// The same, for the read-only slice-of-slices that the quantizer takes.
macro_rules! make_chans_ref {
    ($name:ident, $buf:expr, $channels:expr, $frames:expr) => {
        let mut $name: [&[f64]; MAX_CHANNELS] = std::array::from_fn(|_| &[] as &[f64]);
        for (slot, chan) in $name.iter_mut().zip($buf.iter()) {
            *slot = &chan[..$frames];
        }
        let $name = &$name[..$channels];
    };
}

/// The f64 part of one chain: EQ → convolution → upsampler, with all scratch.
struct Pipeline {
    channels: usize,
    ratio: usize,
    eq: Option<Eq>,
    conv: Option<Convolver>,
    up: Option<Upsampler>,
    /// Interleaved input is deinterleaved here, at the source rate.
    planar: Vec<Vec<f64>>,
    /// Upsampler output (only allocated when `up` is `Some`).
    upsampled: Vec<Vec<f64>>,
    /// Total delay in output frames.
    latency_out: usize,
}

impl Pipeline {
    fn new(
        config: &ChainConfig,
        sample_rate: u32,
        channels: usize,
        max_block: usize,
        irs: Option<Vec<Vec<f64>>>,
        ratio: usize,
    ) -> Result<Self, ChainError> {
        let sr = f64::from(sample_rate);

        let eq = config
            .eq
            .as_ref()
            .filter(|p| p.is_active())
            .map(|p| Eq::new(p, sr, channels));

        let conv = if config.convolution.iter().any(|c| c.enabled) {
            let irs = irs.ok_or(ChainError::MissingIr)?;
            if irs.is_empty() || irs.iter().any(|ir| ir.is_empty()) {
                return Err(ChainError::EmptyIr);
            }
            if irs.len() != 1 && irs.len() != channels {
                return Err(ChainError::IrCount {
                    irs: irs.len(),
                    channels,
                });
            }
            let max_len = irs.iter().map(Vec::len).max().unwrap_or(0);
            if max_len > crate::conv::MAX_COMBINED_IR_TAPS {
                return Err(ChainError::IrTooLong {
                    taps: max_len,
                    max: crate::conv::MAX_COMBINED_IR_TAPS,
                });
            }
            Some(Convolver::new(&irs, channels, max_block))
        } else {
            None
        };

        let up = if ratio > 1 {
            Some(
                Upsampler::try_new(
                    &UpsamplerDesign::from_config(&config.upsampler, ratio),
                    channels,
                    max_block,
                )
                .map_err(|e| ChainError::Upsampler(e.to_string()))?,
            )
        } else {
            None
        };

        let conv_latency = conv.as_ref().map_or(0, Stage::latency_frames) * ratio;
        let up_latency = up.as_ref().map_or(0, Upsampler::latency_out_frames);
        let upsampled = if up.is_some() {
            vec![vec![0.0; max_block * ratio]; channels]
        } else {
            Vec::new()
        };

        Ok(Self {
            channels,
            ratio,
            eq,
            conv,
            up,
            planar: vec![vec![0.0; max_block]; channels],
            upsampled,
            latency_out: conv_latency + up_latency,
        })
    }

    /// Run the f64 stages on the first `frames` samples of `planar`.
    /// Returns the output-rate buffer (upsampled, or `planar` when ratio 1),
    /// `frames * ratio` samples per channel.
    fn process(&mut self, frames: usize) -> &mut [Vec<f64>] {
        let Self {
            eq,
            conv,
            up,
            planar,
            upsampled,
            channels,
            ratio,
            ..
        } = self;

        if let Some(eq) = eq {
            make_chans!(chans, planar, *channels, frames);
            eq.process(chans);
        }
        if let Some(conv) = conv {
            make_chans!(chans, planar, *channels, frames);
            conv.process(chans);
        }

        if let Some(up) = up {
            let out_frames = frames * *ratio;
            make_chans_ref!(inputs, planar, *channels, frames);
            let mut outputs: [&mut [f64]; MAX_CHANNELS] =
                std::array::from_fn(|_| &mut [] as &mut [f64]);
            for (slot, chan) in outputs.iter_mut().zip(upsampled.iter_mut()) {
                *slot = &mut chan[..out_frames];
            }
            up.process(inputs, &mut outputs[..*channels]);
            &mut upsampled[..*channels]
        } else {
            &mut planar[..*channels]
        }
    }

    fn reset(&mut self) {
        if let Some(eq) = &mut self.eq {
            eq.reset();
        }
        if let Some(conv) = &mut self.conv {
            conv.reset();
        }
        if let Some(up) = &mut self.up {
            up.reset();
        }
    }
}

/// Planar f32 input → planar f64 scratch, no allocation.
/// Per-channel peak |x| of the first `frames` samples of planar buffers.
fn planar_peaks<C: AsRef<[f64]>>(chans: &[C], frames: usize) -> [f64; MAX_CLIP_CHANNELS] {
    let mut peaks = [0.0; MAX_CLIP_CHANNELS];
    for (c, chan) in chans.iter().enumerate() {
        let slot = &mut peaks[c.min(MAX_CLIP_CHANNELS - 1)];
        for &x in &chan.as_ref()[..frames] {
            let a = x.abs();
            if a > *slot && a.is_finite() {
                *slot = a;
            }
        }
    }
    peaks
}

fn deinterleave(input: &[f32], planar: &mut [Vec<f64>], frames: usize, channels: usize) {
    for (c, chan) in planar.iter_mut().enumerate() {
        for (i, s) in chan[..frames].iter_mut().enumerate() {
            *s = f64::from(input[i * channels + c]);
        }
    }
}

/// A fixed-length multichannel delay line used to time-align A/B.
struct DelayLine {
    len: usize,
    pos: usize,
    buf: Vec<Vec<f64>>,
}

impl DelayLine {
    fn new(channels: usize, len: usize) -> Self {
        Self {
            len,
            pos: 0,
            buf: if len == 0 {
                Vec::new()
            } else {
                vec![vec![0.0; len]; channels]
            },
        }
    }

    /// Delay every channel in place by exactly `len` frames.
    fn process(&mut self, chans: &mut [Vec<f64>], frames: usize) {
        if self.len == 0 {
            return;
        }
        for (chan, buf) in chans.iter_mut().zip(self.buf.iter_mut()) {
            let mut pos = self.pos;
            for s in chan[..frames].iter_mut() {
                std::mem::swap(&mut buf[pos], s);
                pos += 1;
                if pos == self.len {
                    pos = 0;
                }
            }
        }
        self.pos = (self.pos + frames) % self.len;
    }

    fn reset(&mut self) {
        for buf in &mut self.buf {
            buf.fill(0.0);
        }
        self.pos = 0;
    }
}

/// A click-free equal-gain crossfade. Weights sum to one, which is right for
/// the correlated signals of two EQ profiles; a raised cosine has no slope
/// discontinuity at either end.
struct Fade {
    /// Current weight of B, in [0, 1].
    value: f64,
    from: f64,
    to: f64,
    pos: usize,
    len: usize,
}

impl Fade {
    fn new(value: f64) -> Self {
        Self {
            value,
            from: value,
            to: value,
            pos: 0,
            len: 0,
        }
    }

    fn start(&mut self, to: f64, frames: usize) {
        if frames == 0 || to == self.value {
            self.value = to;
            self.to = to;
            self.pos = 0;
            self.len = 0;
            return;
        }
        self.from = self.value;
        self.to = to;
        self.pos = 0;
        self.len = frames;
    }

    /// Advance one frame and return the current B weight.
    fn next(&mut self) -> f64 {
        if self.pos < self.len {
            self.pos += 1;
            if self.pos >= self.len {
                self.value = self.to;
            } else {
                let t = self.pos as f64 / self.len as f64;
                let shape = 0.5 - 0.5 * (PI * t).cos();
                self.value = self.from + (self.to - self.from) * shape;
            }
        }
        self.value
    }

    fn reset_to(&mut self, value: f64) {
        *self = Self::new(value);
    }
}

/// One full processing chain: EQ, convolution, upsampling and quantization.
pub struct Chain {
    config: ChainConfig,
    channels: usize,
    sample_rate: u32,
    max_block: usize,
    ratio: usize,
    passthrough: bool,
    pipeline: Pipeline,
    quantizer: Quantizer,
}

impl Chain {
    /// Panics on an invalid spec (zero channels, too many channels, missing
    /// impulse responses). Use [`Chain::try_new`] for a `Result`.
    pub fn new(spec: ChainSpec) -> Self {
        Self::try_new(spec).unwrap_or_else(|e| panic!("chain: {e}"))
    }

    pub fn try_new(spec: ChainSpec) -> Result<Self, ChainError> {
        Self::try_new_with_seed(spec, DEFAULT_SEED)
    }

    fn try_new_with_seed(spec: ChainSpec, seed: u64) -> Result<Self, ChainError> {
        Self::validate(&spec)?;
        let ratio = if spec.config.is_passthrough() {
            1
        } else {
            resolve_ratio(spec.config.upsampler.target, spec.sample_rate) as usize
        };
        let passthrough = spec.config.is_passthrough();
        let pipeline = Pipeline::new(
            &spec.config,
            spec.sample_rate,
            spec.channels,
            spec.max_block,
            spec.irs,
            ratio,
        )?;
        // The noise shaper is chosen for the output (post-upsampling) rate.
        let quantizer = Quantizer::with_sample_rate(
            &spec.config.output,
            spec.channels,
            seed,
            spec.sample_rate.saturating_mul(ratio as u32),
        );
        Ok(Self {
            config: spec.config,
            channels: spec.channels,
            sample_rate: spec.sample_rate,
            max_block: spec.max_block,
            ratio,
            passthrough,
            pipeline,
            quantizer,
        })
    }

    fn validate(spec: &ChainSpec) -> Result<(), ChainError> {
        if spec.channels == 0 {
            return Err(ChainError::ZeroChannels);
        }
        if spec.channels > MAX_CHANNELS {
            return Err(ChainError::TooManyChannels {
                channels: spec.channels,
                max: MAX_CHANNELS,
            });
        }
        if spec.max_block == 0 {
            return Err(ChainError::ZeroBlock);
        }
        Ok(())
    }

    /// The configuration this chain was built from.
    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    pub fn output_rate(&self) -> u32 {
        self.sample_rate * self.ratio as u32
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    pub fn ratio(&self) -> usize {
        self.ratio
    }

    pub fn latency_out_frames(&self) -> usize {
        if self.passthrough {
            0
        } else {
            self.pipeline.latency_out
        }
    }

    /// True when the chain runs the bit-exact bypass path.
    ///
    /// The bypass is exactly transparent for inputs that originated as
    /// integers of 24 bits or less (what PipeWire's f32 carries without loss)
    /// written to a container with at least that many bits.
    pub fn is_bit_transparent(&self) -> bool {
        self.passthrough
    }

    /// `input`: interleaved f32 at the source rate (as PipeWire delivers).
    /// `out`: interleaved left-justified i32, `frames * ratio * channels`.
    pub fn process_f32(&mut self, input: &[f32], out: &mut [i32]) -> ClipStats {
        let mut unity = StereoGainRamp::unity(self.output_rate());
        self.process_f32_with_gain(input, out, &mut unity)
    }

    /// Like [`Chain::process_f32`], applying `gain` in `f64` immediately
    /// before the final quantizer/dither.
    ///
    /// When the chain is passthrough and `gain` is settled at unity, this runs
    /// the bit-exact bypass path directly.
    pub fn process_f32_with_gain(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        gain: &mut StereoGainRamp,
    ) -> ClipStats {
        self.process_f32_with_gain_tapped(input, out, gain, None)
    }

    /// Like [`Chain::process_f32_with_gain`], additionally copying the
    /// floating-point chain output (after gain, immediately before
    /// quantization/dither) into `tap` as interleaved `f32`.
    pub fn process_f32_with_gain_tapped(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
    ) -> ClipStats {
        let channels = self.channels;
        assert!(channels != 0, "chain has no channels");
        assert_eq!(
            input.len() % channels,
            0,
            "input length {} is not a multiple of {channels} channels",
            input.len()
        );
        let frames = input.len() / channels;
        assert!(
            frames <= self.max_block,
            "{frames} frames is more than the {} frame max_block",
            self.max_block
        );
        assert!(
            out.len() >= frames * self.ratio * channels,
            "output buffer too small: {} samples for {} frames x {} x {channels}",
            out.len(),
            frames,
            self.ratio
        );
        if frames == 0 {
            return ClipStats::default();
        }

        if self.passthrough && gain.is_unity_settled() {
            if let Some(tap) = tap {
                let n = input.len().min(tap.len());
                tap[..n].copy_from_slice(&input[..n]);
            }
            crate::bypass::f32_to_i32(input, &mut out[..frames * channels]);
            // The passthrough copy reports no statistics; the bytes are the
            // input, so the caller can meter it directly.
            return ClipStats::default();
        }

        let Self {
            pipeline,
            quantizer,
            ..
        } = self;
        deinterleave(input, &mut pipeline.planar, frames, channels);
        let out_frames = frames * pipeline.ratio;
        let chans = pipeline.process(frames);
        let chain_peaks = planar_peaks(chans, out_frames);
        gain.process_planar(chans, out_frames);
        make_chans_ref!(inputs, chans, channels, out_frames);
        if let Some(tap) = tap {
            assert!(
                tap.len() >= out_frames * channels,
                "tap buffer too small: {} samples for {out_frames} frames x {channels}",
                tap.len()
            );
            for i in 0..out_frames {
                for (ch, chan) in inputs.iter().enumerate() {
                    tap[i * channels + ch] = chan[i] as f32;
                }
            }
        }
        let mut stats = quantizer.process(inputs, &mut out[..out_frames * channels]);
        stats.chain_peak_ch = chain_peaks;
        stats
    }

    pub fn reset(&mut self) {
        self.pipeline.reset();
        self.quantizer.reset();
    }
}

/// Deterministic default dither seed. The daemon should use [`Chain::with_seed`]
/// (or set the seed explicitly) if it wants fresh noise per session; tests use
/// this default for reproducibility.
pub const DEFAULT_SEED: u64 = 0x5EED_0FA0_D100_0001;

impl Chain {
    /// Like [`Chain::new`], with an explicit quantizer seed.
    pub fn with_seed(spec: ChainSpec, seed: u64) -> Self {
        Self::try_new_with_seed(spec, seed).unwrap_or_else(|e| panic!("chain: {e}"))
    }
}

/// Which side of an A/B comparison is audible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    A,
    B,
}

impl Side {
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    fn weight_b(self) -> f64 {
        match self {
            Self::A => 0.0,
            Self::B => 1.0,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::A => "A",
            Self::B => "B",
        })
    }
}

/// Two full chains running in parallel with a shared quantizer, for click-free
/// A/B switching with static level matching.
///
/// Both f64 paths run on every block; the side with the lower latency is
/// delayed so the two stay time-aligned, B is scaled by `level_offset_db`, and
/// the selected mix is quantized once.
pub struct AbChain {
    channels: usize,
    max_block: usize,
    ratio: usize,
    sample_rate: u32,
    a: Pipeline,
    b: Pipeline,
    delay_a: DelayLine,
    delay_b: DelayLine,
    mix: Vec<Vec<f64>>,
    weights: Vec<f64>,
    quantizer: Quantizer,
    level_offset_db: f64,
    gain_b: f64,
    fade: Fade,
    current: Side,
    latency_out: usize,
}

impl AbChain {
    /// Build both sides. The two specs must agree on sample rate, channels,
    /// resolved upsampling ratio and output config (there is one quantizer).
    /// `level_offset_db` is applied statically to B.
    pub fn new(a: ChainSpec, b: ChainSpec, level_offset_db: f64) -> Result<Self, ChainError> {
        Chain::validate(&a)?;
        Chain::validate(&b)?;
        if a.sample_rate != b.sample_rate {
            return Err(ChainError::RateMismatch {
                a: a.sample_rate,
                b: b.sample_rate,
            });
        }
        if a.channels != b.channels {
            return Err(ChainError::ChannelMismatch {
                a: a.channels,
                b: b.channels,
            });
        }
        if a.config.output != b.config.output {
            return Err(ChainError::OutputMismatch);
        }
        let ratio_a = resolve_ratio(a.config.upsampler.target, a.sample_rate) as usize;
        let ratio_b = resolve_ratio(b.config.upsampler.target, b.sample_rate) as usize;
        if ratio_a != ratio_b {
            return Err(ChainError::RatioMismatch {
                a: ratio_a,
                b: ratio_b,
            });
        }

        let channels = a.channels;
        let max_block = a.max_block.min(b.max_block);
        let pipeline_a = Pipeline::new(
            &a.config,
            a.sample_rate,
            channels,
            a.max_block,
            a.irs,
            ratio_a,
        )?;
        let pipeline_b = Pipeline::new(
            &b.config,
            b.sample_rate,
            channels,
            b.max_block,
            b.irs,
            ratio_b,
        )?;

        let latency_out = pipeline_a.latency_out.max(pipeline_b.latency_out);
        let delay_a = DelayLine::new(channels, latency_out - pipeline_a.latency_out);
        let delay_b = DelayLine::new(channels, latency_out - pipeline_b.latency_out);
        let mix = vec![vec![0.0; max_block * ratio_a]; channels];

        Ok(Self {
            channels,
            max_block,
            ratio: ratio_a,
            sample_rate: a.sample_rate,
            a: pipeline_a,
            b: pipeline_b,
            delay_a,
            delay_b,
            mix,
            weights: vec![0.0; max_block * ratio_a],
            quantizer: Quantizer::with_sample_rate(
                &a.config.output,
                channels,
                DEFAULT_SEED,
                a.sample_rate.saturating_mul(ratio_a as u32),
            ),
            level_offset_db,
            gain_b: db_to_lin(level_offset_db),
            fade: Fade::new(0.0),
            current: Side::A,
            latency_out,
        })
    }

    /// Static level offset (dB) that was applied to B.
    pub fn level_offset_db(&self) -> f64 {
        self.level_offset_db
    }

    /// Replace the static B offset, e.g. after a measured level match.
    /// Takes effect on the next block and does not touch the fades.
    pub fn set_level_offset_db(&mut self, offset_db: f64) {
        self.level_offset_db = offset_db;
        self.gain_b = db_to_lin(offset_db);
    }

    pub fn output_rate(&self) -> u32 {
        self.sample_rate * self.ratio as u32
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn ratio(&self) -> usize {
        self.ratio
    }

    /// Latency of the time-aligned pair, in output frames.
    pub fn latency_out_frames(&self) -> usize {
        self.latency_out
    }

    /// The side being switched to (immediately after `select`, before the
    /// crossfade has finished).
    pub fn current(&self) -> Side {
        self.current
    }

    /// Switch to `side` over `crossfade_frames` output frames (0 = hard cut).
    /// Equal-gain raised-cosine; switching again mid-fade starts from the
    /// current mix, so repeated clicks never produce a jump.
    pub fn select(&mut self, side: Side, crossfade_frames: usize) {
        self.current = side;
        self.fade.start(side.weight_b(), crossfade_frames);
    }

    /// `out`: interleaved left-justified i32, `frames * ratio * channels`.
    pub fn process_f32(&mut self, input: &[f32], out: &mut [i32]) -> ClipStats {
        let mut unity = StereoGainRamp::unity(self.output_rate());
        self.process_f32_with_gain(input, out, &mut unity)
    }

    /// Like [`AbChain::process_f32`], applying `gain` in `f64` immediately
    /// before the shared quantizer/dither.
    pub fn process_f32_with_gain(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        gain: &mut StereoGainRamp,
    ) -> ClipStats {
        self.process_f32_with_gain_tapped(input, out, gain, None)
    }

    /// Like [`AbChain::process_f32_with_gain`], additionally copying the
    /// shared pre-quantizer mix into `tap` as interleaved `f32`.
    pub fn process_f32_with_gain_tapped(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
    ) -> ClipStats {
        let channels = self.channels;
        assert_eq!(
            input.len() % channels,
            0,
            "input length {} is not a multiple of {channels} channels",
            input.len()
        );
        let frames = input.len() / channels;
        assert!(
            frames <= self.max_block,
            "{frames} frames is more than the {} frame max_block",
            self.max_block
        );
        let out_frames = frames * self.ratio;
        assert!(
            out.len() >= out_frames * channels,
            "output buffer too small: {} samples for {out_frames} output frames x {channels}",
            out.len()
        );
        if frames == 0 {
            return ClipStats::default();
        }

        let Self {
            a,
            b,
            delay_a,
            delay_b,
            mix,
            weights,
            quantizer,
            gain_b,
            fade,
            ..
        } = self;

        deinterleave(input, &mut a.planar, frames, channels);
        deinterleave(input, &mut b.planar, frames, channels);
        let a_buf = a.process(frames);
        let b_buf = b.process(frames);
        delay_a.process(a_buf, out_frames);
        delay_b.process(b_buf, out_frames);

        for w in &mut weights[..out_frames] {
            *w = fade.next();
        }
        for (c, dst) in mix.iter_mut().enumerate() {
            let ac = &a_buf[c];
            let bc = &b_buf[c];
            for (i, s) in dst[..out_frames].iter_mut().enumerate() {
                let w = weights[i];
                *s = (1.0 - w) * ac[i] + w * *gain_b * bc[i];
            }
        }

        let chain_peaks = planar_peaks(mix, out_frames);
        gain.process_planar(mix, out_frames);
        make_chans_ref!(inputs, mix, channels, out_frames);
        if let Some(tap) = tap {
            assert!(
                tap.len() >= out_frames * channels,
                "tap buffer too small: {} samples for {out_frames} frames x {channels}",
                tap.len()
            );
            for i in 0..out_frames {
                for (ch, chan) in inputs.iter().enumerate() {
                    tap[i * channels + ch] = chan[i] as f32;
                }
            }
        }
        let mut stats = quantizer.process(inputs, &mut out[..out_frames * channels]);
        stats.chain_peak_ch = chain_peaks;
        stats
    }

    pub fn reset(&mut self) {
        self.a.reset();
        self.b.reset();
        self.delay_a.reset();
        self.delay_b.reset();
        self.quantizer.reset();
        self.fade.reset_to(self.current.weight_b());
    }

    /// Analytic A-weighted offset (dB to apply to B) from the EQ responses of
    /// two configs, via [`crate::loudness::analytic_offset_db`]. Convolution
    /// magnitude responses are not known offline (they need the IR samples),
    /// so only a configured `gain_db` is included; use the measured mode for
    /// full chains.
    pub fn analytic_offset_db(a: &ChainConfig, b: &ChainConfig, sample_rate: u32) -> f64 {
        let sr = f64::from(sample_rate);
        let eq_a = a.eq.as_ref().map(|p| Eq::new(p, sr, 2));
        let eq_b = b.eq.as_ref().map(|p| Eq::new(p, sr, 2));
        let conv_gain = |c: &ChainConfig| -> f64 {
            c.convolution
                .iter()
                .filter(|conv| conv.enabled)
                .map(|conv| db_to_lin(conv.gain_db))
                .product()
        };
        let eq_power = |eq: &Option<Eq>, f: f64| -> f64 {
            eq.as_ref().map_or(1.0, |e| {
                0.5 * (e.response_channel(0, f).norm_sqr() + e.response_channel(1, f).norm_sqr())
            })
        };
        let gain_a = conv_gain(a);
        let gain_b = conv_gain(b);
        let power_a = |f: f64| gain_a * gain_a * eq_power(&eq_a, f);
        let power_b = |f: f64| gain_b * gain_b * eq_power(&eq_b, f);
        crate::loudness::analytic_offset_db(&power_a, &power_b, sr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{
        Band, DitherKind, EqProfile, FilterType, OutputConfig, RateTarget, UpsamplerConfig,
    };

    fn spec(config: ChainConfig, channels: usize, block: usize) -> ChainSpec {
        ChainSpec {
            config,
            sample_rate: 48_000,
            channels,
            max_block: block,
            irs: None,
        }
    }

    fn flat_profile(preamp_db: f64) -> EqProfile {
        EqProfile {
            name: "flat".into(),
            preamp_db,
            ..Default::default()
        }
    }

    fn quiet_output() -> OutputConfig {
        OutputConfig {
            bits: 32,
            dither: DitherKind::None,
        }
    }

    #[test]
    fn shaped_dither_uses_the_output_rate_shaper() {
        let output = OutputConfig {
            bits: 16,
            dither: DitherKind::Shaped,
        };
        let eq = Some(flat_profile(-1.0));
        for (ratio, out_rate) in [(1u32, 48_000u32), (4, 192_000)] {
            let config = ChainConfig {
                eq: eq.clone(),
                upsampler: UpsamplerConfig {
                    target: if ratio == 1 {
                        RateTarget::Off
                    } else {
                        RateTarget::Ratio(ratio)
                    },
                    taps: 256,
                    ..Default::default()
                },
                output,
                ..Default::default()
            };
            let chain = Chain::new(spec(config.clone(), 2, 64));
            assert_eq!(chain.output_rate(), out_rate);
            assert_eq!(
                chain.quantizer.shaper(),
                crate::dither::noise_shaper(out_rate).map(|h| &h[..])
            );
            let ab = AbChain::new(spec(config.clone(), 2, 64), spec(config, 2, 64), 0.0).unwrap();
            assert_eq!(
                ab.quantizer.shaper(),
                crate::dither::noise_shaper(out_rate).map(|h| &h[..])
            );
        }
    }

    #[test]
    fn default_chain_is_bit_transparent() {
        let chain = Chain::new(spec(ChainConfig::default(), 2, 64));
        assert!(chain.is_bit_transparent());
        assert_eq!(chain.ratio(), 1);
        assert_eq!(chain.output_rate(), 48_000);
        assert_eq!(chain.latency_out_frames(), 0);
        assert_eq!(chain.channels(), 2);
    }

    #[test]
    fn an_upsampler_that_cannot_be_designed_is_an_error() {
        // Minimum phase at beta 40 (the daemon rejects it) cannot meet its
        // tolerances: the chain refuses it instead of running an
        // approximation (`crate::minphase`).
        let mut config = ChainConfig::default();
        config.upsampler.target = RateTarget::Ratio(4);
        config.upsampler.taps = 512;
        config.upsampler.phase = audivane_protocol::FilterPhase::Minimum;
        config.upsampler.kaiser_beta = 40.0;
        match Chain::try_new(spec(config, 2, 64)) {
            Err(ChainError::Upsampler(msg)) => {
                assert!(msg.contains("minimum-phase"), "{msg}");
            }
            Err(other) => panic!("unexpected error {other}"),
            Ok(_) => panic!("built a chain with an untoleranced kernel"),
        }
    }

    #[test]
    fn upsampling_sets_ratio_rate_and_latency() {
        let mut config = ChainConfig::default();
        config.upsampler.target = RateTarget::Ratio(4);
        config.upsampler.taps = 1024;
        config.output = quiet_output();
        let chain = Chain::new(spec(config, 1, 64));
        assert!(!chain.is_bit_transparent());
        assert_eq!(chain.ratio(), 4);
        assert_eq!(chain.output_rate(), 192_000);
        assert!(chain.latency_out_frames() > 0);
    }

    #[test]
    fn passthrough_is_bit_exact_and_reports_zero_latency() {
        let ints: Vec<i32> = (0..256)
            .map(|i| {
                let v = (i as i64 * 2_000_003) % (1 << 23);
                ((v - (1 << 22)) << 8) as i32
            })
            .collect();
        let mut samples = vec![0.0f32; ints.len()];
        crate::bypass::i32_to_f32(&ints, &mut samples);
        let mut chain = Chain::new(spec(ChainConfig::default(), 1, 64));
        let mut out = vec![0i32; ints.len()];
        // Odd partial blocks, as the audio thread may deliver.
        for (i, chunk) in samples.chunks(37).enumerate() {
            let start = i * 37;
            let stats = chain.process_f32(chunk, &mut out[start..start + chunk.len()]);
            assert_eq!(stats, ClipStats::default());
        }
        assert_eq!(out, ints);
        assert_eq!(chain.latency_out_frames(), 0);
    }

    #[test]
    fn empty_blocks_are_noops() {
        let mut chain = Chain::new(spec(ChainConfig::default(), 2, 32));
        let mut out: Vec<i32> = Vec::new();
        assert_eq!(chain.process_f32(&[], &mut out), ClipStats::default());
        let mut ab = AbChain::new(
            spec(ChainConfig::default(), 2, 32),
            spec(ChainConfig::default(), 2, 32),
            0.0,
        )
        .unwrap();
        assert_eq!(ab.process_f32(&[], &mut out), ClipStats::default());
    }

    #[test]
    fn rejects_bad_specs() {
        assert_eq!(
            Chain::try_new(spec(ChainConfig::default(), 0, 16))
                .err()
                .unwrap(),
            ChainError::ZeroChannels
        );
        assert_eq!(
            Chain::try_new(spec(ChainConfig::default(), MAX_CHANNELS + 1, 16))
                .err()
                .unwrap(),
            ChainError::TooManyChannels {
                channels: MAX_CHANNELS + 1,
                max: MAX_CHANNELS
            }
        );
        assert_eq!(
            Chain::try_new(spec(ChainConfig::default(), 2, 0))
                .err()
                .unwrap(),
            ChainError::ZeroBlock
        );
        let config = ChainConfig {
            convolution: vec![audivane_protocol::ConvolutionConfig {
                name: None,
                enabled: true,
                ir_path: "ir.wav".into(),
                gain_db: 0.0,
            }],
            ..ChainConfig::default()
        };
        assert_eq!(
            Chain::try_new(spec(config, 2, 16)).err().unwrap(),
            ChainError::MissingIr
        );
    }

    #[test]
    #[should_panic(expected = "max_block")]
    fn rejects_blocks_larger_than_max_block() {
        let mut chain = Chain::new(spec(ChainConfig::default(), 1, 4));
        let mut out = vec![0i32; 8];
        chain.process_f32(&[0.0; 8], &mut out);
    }

    #[test]
    fn quantizer_counts_clips_on_the_f64_path() {
        let mut config = ChainConfig::default();
        config.upsampler.target = RateTarget::Ratio(1); // not passthrough
        config.output = OutputConfig {
            bits: 16,
            dither: DitherKind::None,
        };
        let mut chain = Chain::new(spec(config, 1, 8));
        let input = [1.5f32, -1.5, 0.0, 0.25, -0.25, 0.5, -0.5, 0.0];
        let mut out = vec![0i32; input.len()];
        let stats = chain.process_f32(&input, &mut out);
        assert_eq!(stats.clipped, 2, "1.5 and -1.5 saturate");
        assert!((stats.peak - 1.5).abs() < 1e-12);
        // 16-bit target: the low 16 bits of the left-justified output are zero.
        assert!(out[2..].iter().all(|s| s & 0xffff == 0));
    }

    #[test]
    fn clip_stats_carry_float_peaks_and_the_chain_peak_before_volume() {
        // +6 dB preamp lifts 0.5 to 1.0; a -10 dB master volume then brings
        // the output to about -10 dBFS. The chain peak excludes the volume.
        let config = ChainConfig {
            eq: Some(flat_profile(6.0)),
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let mut chain = Chain::new(spec(config, 2, 64));
        let input = vec![0.5f32; 128];
        let mut out = vec![0i32; input.len()];
        let mut gain = StereoGainRamp::unity(48_000);
        gain.set_volume_targets(-10.0, 0.0, false, false, false, false);
        let stats = chain.process_f32_with_gain_tapped(&input, &mut out, &mut gain, None);
        assert!(stats.measured);
        let db = |x: f64| 20.0 * x.log10();
        assert!((db(stats.chain_peak_ch[0]) - 0.0).abs() < 0.1, "{:?}", stats.chain_peak_ch);
        assert!((db(stats.peak_ch[0]) + 10.0).abs() < 0.1, "{:?}", stats.peak_ch);
        assert!((db(stats.peak_ch[1]) + 10.0).abs() < 0.1);
    }

    #[test]
    fn pre_quantizer_tap_keeps_the_clipped_float_signal() {
        // +12 dB preamp lifts a 0.5 signal to 2.0; the quantizer saturates it,
        // but the tap must still show the real pre-quantizer level (no
        // clipping harmonics, no quantization).
        let config = ChainConfig {
            eq: Some(flat_profile(12.0)),
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let mut chain = Chain::new(spec(config, 1, 64));
        let input = vec![0.5f32; 64];
        let mut out = vec![0i32; input.len()];
        let mut tap = vec![0f32; input.len()];
        let mut gain = StereoGainRamp::unity(48_000);
        chain.process_f32_with_gain_tapped(&input, &mut out, &mut gain, Some(&mut tap));

        let tap_peak = tap.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(
            tap_peak > 1.9,
            "pre-quantizer tap shows {tap_peak}, expected the ~2.0 float level"
        );
        assert!(out.contains(&i32::MAX), "quantizer should have clipped");
        let dequantized = out[0] as f32 / 2_147_483_648.0;
        assert!(
            (tap[0] - dequantized).abs() > 0.1,
            "tap is just the quantized output again: {} vs {dequantized}",
            tap[0]
        );
    }

    #[test]
    fn ab_rejects_mismatched_configs() {
        let mut other_rate = spec(ChainConfig::default(), 2, 16);
        other_rate.sample_rate = 44_100;
        assert!(matches!(
            AbChain::new(spec(ChainConfig::default(), 2, 16), other_rate, 0.0),
            Err(ChainError::RateMismatch { .. })
        ));
        assert!(matches!(
            AbChain::new(
                spec(ChainConfig::default(), 2, 16),
                spec(ChainConfig::default(), 1, 16),
                0.0
            ),
            Err(ChainError::ChannelMismatch { .. })
        ));

        let mut a = ChainConfig::default();
        a.upsampler.target = RateTarget::Ratio(2);
        let mut b = ChainConfig::default();
        b.upsampler.target = RateTarget::Ratio(4);
        assert!(matches!(
            AbChain::new(spec(a, 2, 16), spec(b, 2, 16), 0.0),
            Err(ChainError::RatioMismatch { a: 2, b: 4 })
        ));

        let c = ChainConfig {
            output: OutputConfig {
                bits: 16,
                dither: DitherKind::Tpdf,
            },
            ..ChainConfig::default()
        };
        assert!(matches!(
            AbChain::new(spec(ChainConfig::default(), 2, 16), spec(c, 2, 16), 0.0),
            Err(ChainError::OutputMismatch)
        ));
    }

    #[test]
    fn ab_applies_static_level_to_b() {
        let config = ChainConfig {
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let mut ab = AbChain::new(spec(config.clone(), 2, 64), spec(config, 2, 64), -6.0).unwrap();
        assert_eq!(ab.current(), Side::A);
        assert_eq!(ab.level_offset_db(), -6.0);
        assert_eq!(ab.output_rate(), 48_000);
        assert_eq!(ab.ratio(), 1);

        let frames = 32;
        let input = vec![0.25f32; frames * 2];
        let mut out = vec![0i32; frames * 2];
        ab.process_f32(&input, &mut out);
        let a_peak = out.iter().map(|s| (*s as f64).abs()).fold(0.0, f64::max);
        assert!(
            (a_peak - 0.25 * 2_147_483_648.0).abs() < 2.0,
            "A should be unity"
        );

        ab.select(Side::B, 0);
        assert_eq!(ab.current(), Side::B);
        ab.process_f32(&input, &mut out);
        let b_peak = out.iter().map(|s| (*s as f64).abs()).fold(0.0, f64::max);
        let ratio_db = 20.0 * (b_peak / a_peak).log10();
        assert!(
            (ratio_db + 6.0).abs() < 1e-3,
            "B should be 6 dB down, got {ratio_db}"
        );

        ab.set_level_offset_db(0.0);
        ab.process_f32(&input, &mut out);
        let unity = out.iter().map(|s| (*s as f64).abs()).fold(0.0, f64::max);
        assert!((unity - a_peak).abs() < 2.0);
    }

    #[test]
    fn ab_crossfade_is_click_free_and_equal_gain() {
        let config = ChainConfig {
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let mut ab = AbChain::new(spec(config.clone(), 1, 64), spec(config, 1, 64), 0.0).unwrap();
        let frames = 64;
        let input = vec![0.5f32; frames];
        let mut out = vec![0i32; frames];
        ab.process_f32(&input, &mut out);
        ab.select(Side::B, 40);
        ab.process_f32(&input, &mut out);
        // Equal-gain mixing of two identical signals keeps the level constant
        // through the fade: no dip and no jump.
        for (i, &sample) in out.iter().enumerate() {
            let value = sample as f64 / 2_147_483_648.0;
            assert!((value - 0.5).abs() < 2e-6, "sample {i} jumped to {value}");
        }
        assert_eq!(ab.current(), Side::B);
    }

    #[test]
    fn ab_time_aligns_different_latencies() {
        // Two upsamplers with different kernel lengths: same ratio, different
        // latency, so one side must be delayed.
        let a = ChainConfig {
            upsampler: UpsamplerConfig {
                target: RateTarget::Ratio(2),
                taps: 512,
                ..Default::default()
            },
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let b = ChainConfig {
            upsampler: UpsamplerConfig {
                target: RateTarget::Ratio(2),
                taps: 2048,
                ..Default::default()
            },
            output: quiet_output(),
            ..ChainConfig::default()
        };

        let chain_a = Chain::new(spec(a.clone(), 1, 32));
        let chain_b = Chain::new(spec(b.clone(), 1, 32));
        let expected = chain_a
            .latency_out_frames()
            .max(chain_b.latency_out_frames());
        let mut ab = AbChain::new(spec(a, 1, 32), spec(b, 1, 32), 0.0).unwrap();
        assert_eq!(ab.latency_out_frames(), expected);
        assert_eq!(ab.ratio(), 2);

        // A block of silence must stay silent on both sides.
        let mut out = vec![0i32; 64];
        ab.process_f32(&[0.0; 32], &mut out);
        assert!(out.iter().all(|&s| s == 0));
        ab.select(Side::B, 8);
        ab.reset();
        ab.process_f32(&[0.0; 32], &mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn analytic_offset_negates_flat_boost() {
        let boosted = ChainConfig {
            eq: Some(flat_profile(6.0)),
            ..ChainConfig::default()
        };
        let offset = AbChain::analytic_offset_db(&ChainConfig::default(), &boosted, 44_100);
        assert!((offset + 6.0).abs() < 0.05, "offset {offset}");
        // The A/B builder uses the same sign convention.
        let boosted_config = ChainConfig {
            output: quiet_output(),
            ..boosted
        };
        let flat = ChainConfig {
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let ab = AbChain::new(spec(flat, 1, 8), spec(boosted_config, 1, 8), offset).unwrap();
        assert!(ab.level_offset_db() < -5.9);
    }

    #[test]
    fn analytic_offset_uses_mean_power_for_layered_eq_and_enabled_conv_gains() {
        let split = ChainConfig {
            eq: Some(EqProfile {
                name: "layered".into(),
                preamp_db: 0.0,
                linked_enabled: true,
                bands: Vec::new(),
                left: Some(audivane_protocol::ChannelEq {
                    enabled: false,
                    preamp_db: 12.0,
                    bands: Vec::new(),
                }),
                right: Some(audivane_protocol::ChannelEq {
                    enabled: true,
                    preamp_db: 6.0,
                    bands: Vec::new(),
                }),
                ..Default::default()
            }),
            convolution: vec![
                audivane_protocol::ConvolutionConfig {
                    name: None,
                    enabled: true,
                    ir_path: "a.wav".into(),
                    gain_db: 2.0,
                },
                audivane_protocol::ConvolutionConfig {
                    name: None,
                    enabled: false,
                    ir_path: "disabled.wav".into(),
                    gain_db: 20.0,
                },
                audivane_protocol::ConvolutionConfig {
                    name: None,
                    enabled: true,
                    ir_path: "b.wav".into(),
                    gain_db: 1.0,
                },
            ],
            ..ChainConfig::default()
        };
        // Mean power of L=0 dB (disabled left ignored, 1.0) and R=+6 dB (10^(6/10) ≈ 3.981):
        // (1 + 3.981)/2 = 2.4905 (≈ +3.963 dB),
        // plus enabled convolution gains 2 + 1 = +3 dB => total ≈ +6.963 dB, offset ≈ -6.963 dB.
        let expected_eq_pwr = 0.5 * (1.0 + db_to_lin(6.0).powi(2));
        let expected_db = -(crate::lin_to_db(expected_eq_pwr.sqrt()) + 3.0);
        let offset = AbChain::analytic_offset_db(&ChainConfig::default(), &split, 48_000);
        assert!(
            (offset - expected_db).abs() < 0.05,
            "expected {expected_db}, got {offset}"
        );

        // Layered linked + left in series on ch0 combines with right.
        let layered = ChainConfig {
            eq: Some(EqProfile {
                name: "layered-series".into(),
                preamp_db: 3.0,
                linked_enabled: true,
                bands: Vec::new(),
                left: Some(audivane_protocol::ChannelEq {
                    enabled: true,
                    preamp_db: 3.0,
                    bands: Vec::new(),
                }),
                right: None,
                ..Default::default()
            }),
            ..ChainConfig::default()
        };
        // L = +6 dB (2x power = 4x), R = +3 dB (2x). Mean power = 2.9905 => ~+4.757 dB.
        let expected_layered_pwr = 0.5 * (db_to_lin(6.0).powi(2) + db_to_lin(3.0).powi(2));
        let expected_layered_db = -crate::lin_to_db(expected_layered_pwr.sqrt());
        let layered_offset = AbChain::analytic_offset_db(&ChainConfig::default(), &layered, 48_000);
        assert!(
            (layered_offset - expected_layered_db).abs() < 0.05,
            "expected {expected_layered_db}, got {layered_offset}"
        );
    }

    #[test]
    fn eq_stage_runs_before_the_quantizer() {
        let config = ChainConfig {
            eq: Some(EqProfile {
                name: "boost".into(),
                preamp_db: 0.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 1000.0,
                    gain_db: 6.0,
                    q: 1.0,
                    enabled: true,
                }],
                ..Default::default()
            }),
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let mut chain = Chain::new(spec(config, 1, 4096));
        assert!(!chain.is_bit_transparent());
        let frames = 4096;
        let rate = 48_000.0;
        let input: Vec<f32> = (0..frames)
            .map(|i| (0.25 * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate).sin()) as f32)
            .collect();
        let mut out = vec![0i32; frames];
        chain.process_f32(&input, &mut out);
        // Sine at the +6 dB band centre settles to about 2x amplitude.
        let tail = &out[frames / 2..];
        let peak = tail.iter().map(|&s| (s as f64).abs()).fold(0.0, f64::max) / 2_147_483_648.0;
        assert!((peak - 0.5).abs() < 0.02, "expected ~0.5 peak, got {peak}");
    }

    #[test]
    fn digital_gain_and_balance_run_before_quantizer_on_passthrough_chain() {
        let mut chain = Chain::new(spec(ChainConfig::default(), 2, 1024));
        assert!(chain.is_bit_transparent());

        // Constant stereo 24-bit integer signal at 0.5 full scale.
        let sample_i32 = 1_i32 << 30; // +0.5 in 24-bit left-justified i32
        let sample_f32 = (sample_i32 as f64 / 2_147_483_648.0) as f32;
        let frames = 512usize;
        let input = vec![sample_f32; frames * 2];

        // Settled unity gain -> exact bit-for-bit passthrough.
        let mut unity = StereoGainRamp::unity(48_000);
        let mut out_unity = vec![0i32; frames * 2];
        chain.process_f32_with_gain(&input, &mut out_unity, &mut unity);
        assert!(out_unity.iter().all(|&s| s == sample_i32));

        // -6 dB volume + -6 dB balance (attenuates right channel by extra 6 dB).
        let mut gain = StereoGainRamp::unity(48_000);
        gain.set_volume_targets(-6.0, -6.0, false, false, false, false);
        let mut out_scaled = vec![0i32; frames * 2];
        chain.process_f32_with_gain(&input, &mut out_scaled, &mut gain);

        let last_l = (out_scaled[(frames - 1) * 2] as f64) / 2_147_483_648.0;
        let last_r = (out_scaled[(frames - 1) * 2 + 1] as f64) / 2_147_483_648.0;
        let expected_l = 0.5 * db_to_lin(-6.0);
        let expected_r = 0.5 * db_to_lin(-12.0);
        assert!(
            (last_l - expected_l).abs() < 1e-4,
            "L expected {expected_l}, got {last_l}"
        );
        assert!(
            (last_r - expected_r).abs() < 1e-4,
            "R expected {expected_r}, got {last_r}"
        );
    }
}
