//! The full processing chain built from a `ChainConfig`:
//! preamp/EQ → convolution → upsampler → (A/B crossfade) → volume → limiter → quantizer.
//!
//! Everything that needs memory is allocated in `Chain::new` / `AbChain::new`;
//! `process_f32` is allocation-free. Interleaved f32 comes in (as PipeWire
//! delivers it), planar f64 runs through the stages, and a single quantizer
//! produces interleaved left-justified i32 at the output rate (an A/B pair
//! whose sides differ in bits or dither runs two, see [`AbChain`]).
//!
//! When `ChainConfig::is_passthrough()` is true the f64 path is skipped
//! entirely and samples go through [`crate::bypass::f32_to_i32`], which is
//! bit-exact for every input that came from an integer sample of 24 bits or
//! less.

use crate::ab_level::{AbGains, AbLevel, AbOffset, ConvTerm, SideTerms};
use crate::ab_measure::{AbHop, AbHopMeter};
use crate::biquad::Eq;
use crate::conv::Convolver;
use crate::crossfeed::{Crossfeed, CrossfeedError};
use crate::dc_block::{DcBlock, DcBlockError};
use crate::dither::{ClipStats, MAX_CLIP_CHANNELS, Quantizer};
use crate::gain::StereoGainRamp;
use crate::harmonics::{Harmonics, HarmonicsError};
use crate::limiter::Limiter;
use crate::loudness::Weighting;
use crate::pitch::PitchShifter;
use crate::presets::UpsamplerRuntime;
use crate::{Stage, db_to_lin};
use audivane_protocol::{ChainConfig, RateRatio, resolve_rate_ratio};
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
    RatioMismatch { a: RateRatio, b: RateRatio },
    /// A/B takes the output headroom before the mix: both sides must use
    /// the same `output.headroom_db` (the bits and dither may differ).
    HeadroomMismatch,
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
    /// Pitch shifter configuration or initialization failed.
    Pitch(String),
    /// DC block configuration error.
    DcBlock(DcBlockError),
    /// Crossfeed configuration error (e.g. non-stereo channel count).
    Crossfeed(CrossfeedError),
    /// Harmonics configuration error.
    Harmonics(HarmonicsError),
    /// The limiter config is invalid or cannot run at this rate.
    Limiter(String),
    /// EQ configuration is invalid at the stream rate or design failed.
    Eq(String),
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
            Self::HeadroomMismatch => write!(f, "A/B output headroom differs"),
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
            Self::Pitch(e) => write!(f, "pitch: {e}"),
            Self::DcBlock(e) => write!(f, "{e}"),
            Self::Crossfeed(e) => write!(f, "{e}"),
            Self::Harmonics(e) => write!(f, "{e}"),
            Self::Limiter(e) => write!(f, "limiter: {e}"),
            Self::Eq(e) => write!(f, "EQ: {e}"),
        }
    }
}

impl std::error::Error for ChainError {}

/// Largest volume/balance gain of channel `c` over a block. The gain ramp
/// is a monotone raised cosine, so its ends bound it.
fn block_gain(before: [f64; 3], after: [f64; 3]) -> impl Fn(usize) -> f64 {
    move |c| before[c.min(2)].max(after[c.min(2)])
}

fn build_limiter(
    config: &ChainConfig,
    rate: u32,
    channels: usize,
    ratio: RateRatio,
) -> Result<Option<Limiter>, ChainError> {
    config.limiter.validate().map_err(ChainError::Limiter)?;
    if config.limiter.mode == audivane_protocol::LimiterMode::Off {
        return Ok(None);
    }
    // The limiter runs at the output rate, whatever the ratio.
    let rate = ratio
        .output_rate(rate)
        .ok_or_else(|| ChainError::Limiter("output rate is not a whole number of Hz".into()))?;
    Limiter::new(&config.limiter, rate, channels)
        .map(Some)
        .map_err(ChainError::Limiter)
}

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

/// The f64 part of one chain: DC block → pitch → crossfeed → EQ →
/// convolution → upsampler → harmonics → output headroom, with all scratch.
struct Pipeline {
    channels: usize,
    ratio: RateRatio,
    /// Input frames processed since construction or reset, modulo
    /// `ratio.down` (the position on the rational frame grid).
    pos: u64,
    dc_block: Option<DcBlock>,
    pitch: Option<PitchShifter>,
    crossfeed: Option<Crossfeed>,
    eq: Option<Eq>,
    conv: Option<Convolver>,
    up: Option<Box<dyn UpsamplerRuntime>>,
    harmonics: Option<Harmonics>,
    /// The output headroom gain still to apply after harmonics: 1 when
    /// `output.headroom_db` is 0 or folded into the upsampler's kernel.
    out_gain: f64,
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
        ratio: RateRatio,
    ) -> Result<Self, ChainError> {
        let sr = f64::from(sample_rate);

        let dc_block = match config.dc_block.as_ref().filter(|d| d.is_active()) {
            Some(cfg) => Some(DcBlock::try_new(cfg, sr, channels).map_err(ChainError::DcBlock)?),
            None => None,
        };

        let pitch = if config.pitch.is_active() {
            Some(
                PitchShifter::try_new(
                    sample_rate,
                    channels,
                    max_block,
                    config.pitch.effective_cents(),
                    config.pitch.quality,
                )
                .map_err(|e| ChainError::Pitch(e.to_string()))?,
            )
        } else {
            None
        };

        let crossfeed = match config.crossfeed.as_ref().filter(|c| c.is_active()) {
            Some(cfg) => {
                Some(Crossfeed::try_new(cfg, sr, channels).map_err(ChainError::Crossfeed)?)
            }
            None => None,
        };

        let eq = config
            .eq
            .as_ref()
            .map(|p| Eq::new(p, sr, channels))
            .transpose()
            .map_err(ChainError::Eq)?;

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

        // The output headroom goes last, after harmonics (non-linear, its
        // levels defined at 0 dBFS). Without harmonics the upsampler is the
        // last stage and the gain is folded into its kernel (free per
        // sample); otherwise it is one multiply per output sample.
        let harmonics_on = config.harmonics.as_ref().is_some_and(|h| h.is_active());
        let fold_headroom = !ratio.is_unity() && !harmonics_on;
        let headroom_db = config.output.headroom_db;
        let out_gain = if fold_headroom || headroom_db == 0.0 {
            1.0
        } else {
            crate::db_to_lin(-headroom_db)
        };

        // The upsampler: its design checked, designed and built for this
        // rate (none at a unity ratio).
        let up = if ratio.is_unity() {
            None
        } else {
            let named = crate::design::Named::of(&config.upsampler)
                .map_err(|e| ChainError::Upsampler(e.to_string()))?;
            let cx = crate::design::BuildContext {
                source_rate: sample_rate,
                ratio,
                channels,
                max_block,
                pipelines: crate::design::budget::RATIONAL_SWAP_PIPELINES,
                output: config.output,
                workspace: crate::design::budget::design_workspace(),
            };
            let folded = if fold_headroom { headroom_db } else { 0.0 };
            crate::design::upsampler(&named, &cx, folded)
                .map_err(|e| ChainError::Upsampler(e.to_string()))?
        };

        // Pitch, crossfeed and convolution run at the input rate: their
        // delays in output frames are scaled by the ratio. Harmonics run at
        // the output rate. The sum is rounded once, to the nearest frame.
        let output_rate = sr * ratio.as_f64();
        let harmonics = match config.harmonics.as_ref().filter(|h| h.is_active()) {
            Some(cfg) => Some(
                Harmonics::try_new(cfg, output_rate, channels).map_err(ChainError::Harmonics)?,
            ),
            None => None,
        };
        let input_rate_latency = dc_block.as_ref().map_or(0, Stage::latency_frames)
            + pitch.as_ref().map_or(0, Stage::latency_frames)
            + crossfeed.as_ref().map_or(0, Stage::latency_frames)
            + conv.as_ref().map_or(0, Stage::latency_frames);
        let up_latency = up.as_ref().map_or(0.0, |u| u.latency_out_exact());
        let harmonics_latency = harmonics.as_ref().map_or(0, Stage::latency_frames);
        let latency_exact =
            input_rate_latency as f64 * ratio.as_f64() + up_latency + harmonics_latency as f64;
        let upsampled = if up.is_some() {
            vec![vec![0.0; ratio.max_frames_out(max_block)]; channels]
        } else {
            Vec::new()
        };

        Ok(Self {
            channels,
            ratio,
            pos: 0,
            dc_block,
            pitch,
            crossfeed,
            eq,
            conv,
            up,
            harmonics,
            out_gain,
            planar: vec![vec![0.0; max_block]; channels],
            upsampled,
            latency_out: (latency_exact + 0.5).floor() as usize,
        })
    }

    /// Output frames the next `process(frames)` produces.
    fn output_frames(&self, frames: usize) -> usize {
        self.ratio.frames_out(self.pos, frames)
    }

    /// The stages the A/B level terms read ([`crate::ab_level`]).
    fn level_terms<'a>(
        &'a self,
        config: &'a ChainConfig,
        sample_rate: f64,
        conv: ConvTerm,
        upsampler: Option<crate::ab_level::PowerSpectrum>,
        analytic: bool,
    ) -> SideTerms<'a> {
        // The harmonics simulation feeds only the analytic offset.
        let harmonics_db = match config.harmonics.as_ref() {
            Some(h) if analytic => Harmonics::loudness_change_db(h).unwrap_or(0.0),
            _ => 0.0,
        };
        SideTerms {
            sample_rate,
            dc: self.dc_block.as_ref(),
            crossfeed: self.crossfeed.as_ref(),
            eq: self.eq.as_ref(),
            eq_profile: config.eq.as_ref(),
            conv,
            upsampler,
            harmonics_gain: db_to_lin(harmonics_db),
        }
    }

    /// Run the f64 stages on the first `frames` samples of `planar`.
    /// Returns the output-rate buffer (upsampled, or `planar` when ratio 1)
    /// and its length, [`Self::output_frames`]`(frames)` per channel.
    fn process(&mut self, frames: usize) -> (&mut [Vec<f64>], usize) {
        let out_frames = self.output_frames(frames);
        self.pos = (self.pos + frames as u64) % u64::from(self.ratio.down);
        let Self {
            dc_block,
            pitch,
            crossfeed,
            eq,
            conv,
            up,
            harmonics,
            out_gain,
            planar,
            upsampled,
            channels,
            ..
        } = self;

        if let Some(dc) = dc_block {
            make_chans!(chans, planar, *channels, frames);
            dc.process(chans);
        }
        if let Some(pitch) = pitch {
            make_chans!(chans, planar, *channels, frames);
            pitch.process(chans);
        }
        if let Some(cf) = crossfeed {
            make_chans!(chans, planar, *channels, frames);
            cf.process(chans);
        }
        if let Some(eq) = eq {
            make_chans!(chans, planar, *channels, frames);
            eq.process(chans);
        }
        if let Some(conv) = conv {
            make_chans!(chans, planar, *channels, frames);
            conv.process(chans);
        }

        let out = if let Some(up) = up {
            debug_assert_eq!(up.output_frames(frames), out_frames);
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
        };
        if let Some(harm) = harmonics {
            make_chans!(chans, out, *channels, out_frames);
            harm.process(chans);
        }
        if *out_gain != 1.0 {
            for chan in out.iter_mut() {
                for x in &mut chan[..out_frames] {
                    *x *= *out_gain;
                }
            }
        }
        (out, out_frames)
    }

    /// Clear all state and continue the rational frame grid at `pos`.
    fn reset_to(&mut self, pos: u64) {
        if let Some(dc) = &mut self.dc_block {
            dc.reset();
        }
        if let Some(pitch) = &mut self.pitch {
            pitch.reset();
        }
        if let Some(cf) = &mut self.crossfeed {
            cf.reset();
        }
        if let Some(eq) = &mut self.eq {
            eq.reset();
        }
        if let Some(conv) = &mut self.conv {
            conv.reset();
        }
        self.pos = pos % u64::from(self.ratio.down);
        if let Some(up) = &mut self.up {
            up.reset_to(self.pos);
        }
        if let Some(harm) = &mut self.harmonics {
            harm.reset();
        }
    }
}

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

/// A chain's input block: interleaved `f32` as PipeWire delivers it, or
/// planar `f64` handed over by the processor.
#[derive(Clone, Copy)]
enum BlockIn<'a> {
    F32(&'a [f32]),
    Planar {
        input: &'a [&'a [f64]],
        transparent: bool,
    },
}

impl BlockIn<'_> {
    /// Frames in the block (panics on a malformed block: caller bugs).
    fn frames(&self, channels: usize) -> usize {
        match self {
            Self::F32(input) => {
                assert_eq!(
                    input.len() % channels,
                    0,
                    "input length {} is not a multiple of {channels} channels",
                    input.len()
                );
                input.len() / channels
            }
            Self::Planar { input, .. } => {
                assert_eq!(input.len(), channels, "input channel count mismatch");
                let frames = input.first().map_or(0, |c| c.len());
                assert!(
                    input.iter().all(|c| c.len() == frames),
                    "planar input channels differ in length"
                );
                frames
            }
        }
    }

    /// The samples are the source's `f32` values exactly.
    fn transparent(&self) -> bool {
        match self {
            Self::F32(_) => true,
            Self::Planar { transparent, .. } => *transparent,
        }
    }

    /// Fill the first `frames` samples of `planar`. `f32 → f64` is exact,
    /// so both forms give the same `f64` values for the same samples.
    fn load(&self, planar: &mut [Vec<f64>], frames: usize, channels: usize) {
        match self {
            Self::F32(input) => deinterleave(input, planar, frames, channels),
            Self::Planar { input, .. } => {
                for (dst, src) in planar.iter_mut().zip(input.iter()) {
                    dst[..frames].copy_from_slice(&src[..frames]);
                }
            }
        }
    }
}

/// Planar `f64` holding `f32` values → interleaved left-justified `i32`,
/// exactly as [`crate::bypass::f32_to_i32`] converts them, and the same
/// values as `f32` into `tap`.
fn planar_to_i32(input: &[&[f64]], out: &mut [i32], frames: usize, tap: Option<&mut [f32]>) {
    let channels = input.len();
    for (c, chan) in input.iter().enumerate() {
        for (i, &x) in chan[..frames].iter().enumerate() {
            out[i * channels + c] = (x * 2_147_483_648.0).round() as i32;
        }
    }
    if let Some(tap) = tap {
        for (c, chan) in input.iter().enumerate() {
            for (i, &x) in chan[..frames].iter().enumerate() {
                if let Some(t) = tap.get_mut(i * channels + c) {
                    *t = x as f32;
                }
            }
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

/// A/B's per-side linear gains. A change ramps linearly over `len` frames,
/// so a new level never steps.
struct LevelRamp {
    cur: [f64; 2],
    target: [f64; 2],
    step: [f64; 2],
    left: usize,
    len: usize,
}

impl LevelRamp {
    fn new(gains: (f64, f64), len: usize) -> Self {
        Self {
            cur: [gains.0, gains.1],
            target: [gains.0, gains.1],
            step: [0.0; 2],
            left: 0,
            len,
        }
    }

    fn set(&mut self, gains: (f64, f64)) {
        self.target = [gains.0, gains.1];
        if self.len == 0 || self.target == self.cur {
            self.cur = self.target;
            self.left = 0;
            return;
        }
        for k in 0..2 {
            self.step[k] = (self.target[k] - self.cur[k]) / self.len as f64;
        }
        self.left = self.len;
    }

    /// Advance one frame and return the gains `(A, B)`.
    #[inline]
    fn next(&mut self) -> (f64, f64) {
        if self.left > 0 {
            self.left -= 1;
            if self.left == 0 {
                self.cur = self.target;
            } else {
                self.cur[0] += self.step[0];
                self.cur[1] += self.step[1];
            }
        }
        (self.cur[0], self.cur[1])
    }

    fn reset(&mut self) {
        self.cur = self.target;
        self.left = 0;
    }
}

/// Length of an A/B level change ramp.
const LEVEL_RAMP_SECONDS: f64 = 0.05;

/// One full processing chain: EQ, convolution, upsampling and quantization.
pub struct Chain {
    config: ChainConfig,
    channels: usize,
    sample_rate: u32,
    max_block: usize,
    ratio: RateRatio,
    passthrough: bool,
    pipeline: Pipeline,
    quantizer: Quantizer,
    limiter: Option<Limiter>,
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
            RateRatio::ONE
        } else {
            resolve_rate_ratio(spec.config.upsampler.target, spec.sample_rate)
        };
        if ratio.output_rate(spec.sample_rate).is_none() {
            return Err(ChainError::Upsampler(format!(
                "{} Hz x {ratio} is not a whole output rate",
                spec.sample_rate
            )));
        }
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
            ratio
                .output_rate(spec.sample_rate)
                .unwrap_or(spec.sample_rate),
        );
        let limiter = build_limiter(&spec.config, spec.sample_rate, spec.channels, ratio)?;
        Ok(Self {
            limiter,
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
        self.ratio
            .output_rate(self.sample_rate)
            .unwrap_or(self.sample_rate)
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// The peak limiter stage, when the config enables one.
    pub fn limiter(&self) -> Option<&Limiter> {
        self.limiter.as_ref()
    }

    pub fn limiter_mut(&mut self) -> Option<&mut Limiter> {
        self.limiter.as_mut()
    }

    /// The input (graph) rate the chain was built for.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn dc_block(&self) -> Option<&DcBlock> {
        self.pipeline.dc_block.as_ref()
    }

    pub fn dc_block_mut(&mut self) -> Option<&mut DcBlock> {
        self.pipeline.dc_block.as_mut()
    }

    /// Output rate / input rate (1 on the bypass path).
    pub fn ratio(&self) -> RateRatio {
        self.ratio
    }

    /// Output frames the next `process_f32*` call with `frames` input
    /// frames writes: `frames * ratio` for integer ratios, otherwise the
    /// count on the rational frame grid ([`RateRatio::frames_out`]).
    pub fn output_frames(&self, frames: usize) -> usize {
        if self.passthrough {
            frames
        } else {
            self.pipeline.output_frames(frames)
        }
    }

    /// Most output frames one call with `max_block` input frames can write.
    pub fn max_output_frames(&self) -> usize {
        self.ratio.max_frames_out(self.max_block)
    }

    /// Input frames processed, modulo `ratio.down`: where this chain is on
    /// the rational frame grid.
    pub fn input_pos(&self) -> u64 {
        self.pipeline.pos
    }
    /// Test support for the filter fingerprint tests
    /// (`tests/filter_fingerprints.rs`): the upsampler runtime this chain
    /// built (`None` when it does not upsample).
    #[doc(hidden)]
    pub fn upsampler_runtime_mut(&mut self) -> Option<&mut (dyn UpsamplerRuntime + 'static)> {
        self.pipeline.up.as_deref_mut()
    }

    pub fn latency_out_frames(&self) -> usize {
        if self.passthrough {
            0
        } else {
            self.pipeline.latency_out + self.limiter.as_ref().map_or(0, Stage::latency_frames)
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
    /// `out`: interleaved left-justified i32, at least
    /// [`Self::output_frames`]`(frames) * channels` samples; only that many
    /// are written.
    pub fn process_f32(&mut self, input: &[f32], out: &mut [i32]) -> ClipStats {
        let mut unity = StereoGainRamp::unity(self.output_rate());
        self.process_f32_with_gain(input, out, &mut unity)
    }

    /// Like [`Chain::process_f32`], applying `gain` in `f64` before the
    /// optional limiter and final quantizer/dither.
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
        self.process_block_in(BlockIn::F32(input), out, gain, tap)
    }

    /// Like [`Chain::process_f32_with_gain_tapped`], from planar `f64`
    /// input: `input[c]` holds the block's frames for channel `c` (every
    /// channel the same length). This is how the processor hands one block
    /// to every chain (the varispeed stage's output, or the source
    /// deinterleaved once).
    ///
    /// `transparent` says the samples are the source's `f32` values exactly
    /// (no stage changed them): only then may a passthrough chain take its
    /// bit-exact shortcut. With `transparent` set, the result is bit for bit
    /// that of the `f32` entry point on the same samples.
    pub fn process_planar_with_gain_tapped(
        &mut self,
        input: &[&[f64]],
        out: &mut [i32],
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
        transparent: bool,
    ) -> ClipStats {
        self.process_block_in(BlockIn::Planar { input, transparent }, out, gain, tap)
    }

    fn process_block_in(
        &mut self,
        input: BlockIn<'_>,
        out: &mut [i32],
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
    ) -> ClipStats {
        let channels = self.channels;
        assert!(channels != 0, "chain has no channels");
        let frames = input.frames(channels);
        assert!(
            frames <= self.max_block,
            "{frames} frames is more than the {} frame max_block",
            self.max_block
        );
        let out_frames = self.output_frames(frames);
        assert!(
            out.len() >= out_frames * channels,
            "output buffer too small: {} samples for {out_frames} output frames x {channels}",
            out.len(),
        );
        if frames == 0 {
            return ClipStats::default();
        }

        if self.passthrough && gain.is_unity_settled() && input.transparent() {
            match input {
                BlockIn::F32(input) => {
                    if let Some(tap) = tap {
                        let n = input.len().min(tap.len());
                        tap[..n].copy_from_slice(&input[..n]);
                    }
                    crate::bypass::f32_to_i32(input, &mut out[..frames * channels]);
                }
                BlockIn::Planar { input, .. } => {
                    planar_to_i32(input, &mut out[..frames * channels], frames, tap);
                }
            }
            // The passthrough copy reports no statistics; the bytes are the
            // input, so the caller can meter it directly.
            return ClipStats::default();
        }

        let Self {
            pipeline,
            quantizer,
            limiter,
            ..
        } = self;
        input.load(&mut pipeline.planar, frames, channels);
        let (chans, out_frames) = pipeline.process(frames);
        let chain_peaks = planar_peaks(chans, out_frames);
        let volume_before = gain.current();
        gain.process_planar(chans, out_frames);
        if let Some(limiter) = limiter {
            limiter.set_channel_gains(block_gain(volume_before, gain.current()));
            limiter.set_ramping(volume_before != gain.current());
            make_chans!(limited, chans, channels, out_frames);
            limiter.process(limited);
        }
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
        stats.limiter = limiter.as_mut().map_or(
            audivane_protocol::LimiterStatus::default(),
            Limiter::take_status,
        );
        stats
    }

    pub fn reset(&mut self) {
        self.reset_to(0);
    }

    /// [`Self::reset`], continuing the rational frame grid at `pos` input
    /// frames, so this chain writes the same output counts as another one
    /// that has processed `pos` frames (see [`Self::input_pos`]). Used when
    /// a chain joins a running stream next to another.
    pub fn reset_to(&mut self, pos: u64) {
        self.pipeline.reset_to(pos);
        if let Some(limiter) = &mut self.limiter {
            limiter.reset();
        }
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
/// delayed so the two stay time-aligned, each side is scaled by its
/// never-boost gain and the shared headroom ([`crate::ab_level`]), and the
/// selected mix is quantized once.
///
/// # Output compare
///
/// When B's `output.bits` or `output.dither` differ from A's, the pair runs a
/// second quantizer with B's settings. Both quantize the same shared mix
/// (after the volume and the limiter) on every block, so each one's dither
/// sequence and shaper state stay continuous and neither starts cold at a
/// switch. Two quantized streams cannot be crossfaded without quantizing
/// again, so the output is a hard cut: output frame `i` comes from B's
/// quantizer when the crossfade's B weight of that frame leaving the limiter
/// is at least one half, and from A's otherwise. The cut lands at the middle
/// of the fade, where the f64 mix is half of each side. Both streams carry the
/// same signal, so the step at the cut is the difference of their noise (a few
/// LSB of the coarser quantizer, plus a fixed-coefficient mode's reserved
/// headroom, up to 16 LSB of attenuation at full scale). Clips then count to
/// the side whose quantizer produced the frame. With the same bits and dither
/// on both sides there is one quantizer and the output is unchanged. B's
/// stages are built with A's bits and dither, so B's own settings change its
/// quantization only.
pub struct AbChain {
    channels: usize,
    max_block: usize,
    ratio: RateRatio,
    sample_rate: u32,
    a: Pipeline,
    b: Pipeline,
    delay_a: DelayLine,
    delay_b: DelayLine,
    mix: Vec<Vec<f64>>,
    weights: Vec<f64>,
    quantizer: Quantizer,
    /// B's own quantizer, when B's bits or dither differ from A's (see
    /// "Output compare" above), and the scratch its discarded frames (and
    /// A's) are written to.
    quantizer_b: Option<Quantizer>,
    discard: Vec<i32>,
    limiter: Option<Limiter>,
    headroom: audivane_protocol::ipc::AbHeadroom,
    /// Predicted worst-case peak gain (dB) of A and B, for `auto` headroom.
    peak_db: [f64; 2],
    gains: AbGains,
    level: LevelRamp,
    /// Per-frame mix coefficients of A and B (fade weight times gain).
    coef_a: Vec<f64>,
    coef_b: Vec<f64>,
    /// The B weight of each frame leaving the limiter (`weights` delayed by
    /// its latency), for the clips counted at the quantizer.
    weights_out: Vec<f64>,
    /// Ring of the B weights of the last limiter-latency frames that
    /// entered the limiter, oldest at `weight_pos`.
    weight_delay: Vec<f64>,
    weight_pos: usize,
    overs: [f64; 2],
    /// Per-side K-weighted hop energies for a measured level match
    /// ([`crate::ab_measure`]); idle unless a measurement runs.
    meter: AbHopMeter,
    fade: Fade,
    current: Side,
    latency_out: usize,
}

impl AbChain {
    /// Build both sides. The two specs must agree on sample rate, channels,
    /// resolved upsampling ratio, limiter and output headroom. B's bits and
    /// dither may differ from A's: B then gets its own quantizer.
    /// `level` gives B's offset (analytic offsets are computed here, from
    /// the built stages and the IRs) and the shared headroom.
    pub fn new(a: ChainSpec, b: ChainSpec, level: AbLevel) -> Result<Self, ChainError> {
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
        if a.config.limiter != b.config.limiter {
            return Err(ChainError::Limiter("A/B limiter configs must match".into()));
        }
        if a.config.output.headroom_db != b.config.output.headroom_db {
            return Err(ChainError::HeadroomMismatch);
        }
        let ratio_a = resolve_rate_ratio(a.config.upsampler.target, a.sample_rate);
        let ratio_b = resolve_rate_ratio(b.config.upsampler.target, b.sample_rate);
        if ratio_a.output_rate(a.sample_rate).is_none() {
            return Err(ChainError::Upsampler(format!(
                "{} Hz x {ratio_a} is not a whole output rate",
                a.sample_rate
            )));
        }
        if ratio_a != ratio_b {
            return Err(ChainError::RatioMismatch {
                a: ratio_a,
                b: ratio_b,
            });
        }

        let channels = a.channels;
        let max_block = a.max_block.min(b.max_block);
        let sr = f64::from(a.sample_rate);
        let analytic = matches!(level.offset, AbOffset::Analytic(_));
        let wants_terms =
            analytic || matches!(level.headroom, audivane_protocol::ipc::AbHeadroom::Auto);
        let conv_term = |spec: &ChainSpec| match &spec.irs {
            Some(irs) if wants_terms && spec.config.convolution.iter().any(|c| c.enabled) => {
                ConvTerm::of_irs(irs, sr)
            }
            _ => ConvTerm::Gain(1.0),
        };
        let conv_a = conv_term(&a);
        let conv_b = conv_term(&b);
        let mut pipeline_a = Pipeline::new(
            &a.config,
            a.sample_rate,
            channels,
            a.max_block,
            a.irs,
            ratio_a,
        )?;
        // B's stages are built with A's bits and dither: an automatic
        // (`match_output`) filter budget then resolves the same on both
        // sides, so a bits or dither compare changes the quantizer only.
        let b_design = ChainConfig {
            output: audivane_protocol::OutputConfig {
                bits: a.config.output.bits,
                dither: a.config.output.dither,
                ..b.config.output
            },
            ..b.config.clone()
        };
        let mut pipeline_b = Pipeline::new(
            &b_design,
            b.sample_rate,
            channels,
            b.max_block,
            b.irs,
            ratio_b,
        )?;

        let latency_out = pipeline_a.latency_out.max(pipeline_b.latency_out);
        let delay_a = DelayLine::new(channels, latency_out - pipeline_a.latency_out);
        let delay_b = DelayLine::new(channels, latency_out - pipeline_b.latency_out);
        let max_out = ratio_a.max_frames_out(max_block);
        let mix = vec![vec![0.0; max_out]; channels];
        let limiter = build_limiter(&a.config, a.sample_rate, channels, ratio_a)?;
        let latency_out = latency_out + limiter.as_ref().map_or(0, Stage::latency_frames);

        // The upsampler enters the level terms only where the two designs
        // differ; the same design on both sides cancels.
        let (up_a, up_b) = if wants_terms && a.config.upsampler != b.config.upsampler {
            let measure = |p: &mut Pipeline, block: usize| {
                p.up.as_mut()
                    .map(|up| crate::ab_level::upsampler_spectrum(up.as_mut(), channels, block, sr))
            };
            (
                measure(&mut pipeline_a, a.max_block),
                measure(&mut pipeline_b, b.max_block),
            )
        } else {
            (None, None)
        };
        let (offset_db, peak_db) = if wants_terms {
            let analytic = matches!(level.offset, AbOffset::Analytic(_));
            let ta = pipeline_a.level_terms(&a.config, sr, conv_a, up_a, analytic);
            let tb = pipeline_b.level_terms(&b.config, sr, conv_b, up_b, analytic);
            let offset_db = match level.offset {
                AbOffset::Analytic(w) => crate::ab_level::analytic_offset_db(&ta, &tb, w),
                AbOffset::Fixed(db) => db,
            };
            // The output headroom is cut before the mix: the peaks it
            // already took off need no A/B headroom.
            let cut = a.config.output.headroom_db;
            (
                offset_db,
                [ta.peak_gain_db() - cut, tb.peak_gain_db() - cut],
            )
        } else {
            let AbOffset::Fixed(db) = level.offset else {
                unreachable!("analytic offsets always want the terms")
            };
            (db, [0.0; 2])
        };
        let gains = AbGains::new(offset_db, level.headroom, peak_db[0], peak_db[1]);
        let output_rate = ratio_a.output_rate(a.sample_rate).unwrap_or(a.sample_rate);
        let ramp_frames = (LEVEL_RAMP_SECONDS * f64::from(output_rate)).round() as usize;
        let limiter_delay = limiter.as_ref().map_or(0, Stage::latency_frames);
        let quantizer =
            |output| Quantizer::with_sample_rate(output, channels, DEFAULT_SEED, output_rate);
        let out_b = &b.config.output;
        let quantizer_b = (out_b.bits != a.config.output.bits
            || out_b.dither != a.config.output.dither)
            .then(|| quantizer(out_b));
        let discard = if quantizer_b.is_some() {
            vec![0; max_out * channels]
        } else {
            Vec::new()
        };

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
            weights: vec![0.0; max_out],
            quantizer: quantizer(&a.config.output),
            quantizer_b,
            discard,
            limiter,
            headroom: level.headroom,
            peak_db,
            gains,
            level: LevelRamp::new(gains.linear(), ramp_frames),
            coef_a: vec![0.0; max_out],
            coef_b: vec![0.0; max_out],
            weights_out: vec![0.0; max_out],
            weight_delay: vec![0.0; limiter_delay],
            weight_pos: 0,
            overs: [0.0; 2],
            meter: AbHopMeter::new(f64::from(output_rate), channels),
            fade: Fade::new(0.0),
            current: Side::A,
            latency_out,
        })
    }

    /// True when B has its own quantizer (its bits or dither differ from
    /// A's): the pair cuts between two quantized streams at a switch.
    pub fn compares_output(&self) -> bool {
        self.quantizer_b.is_some()
    }

    /// B's level offset (dB): B plus this is as loud as A.
    pub fn level_offset_db(&self) -> f64 {
        self.gains.offset_db
    }

    /// The gains the pair applies: never-boost per side, shared headroom.
    pub fn gains(&self) -> AbGains {
        self.gains
    }

    /// Replace B's offset, e.g. after a measured level match. The gains
    /// (and an `auto` headroom) follow; the change ramps over 50 ms from the
    /// next block and does not touch the fades. Allocation-free.
    pub fn set_level_offset_db(&mut self, offset_db: f64) {
        self.gains = AbGains::new(offset_db, self.headroom, self.peak_db[0], self.peak_db[1]);
        self.level.set(self.gains.linear());
    }

    /// Meter both sides for the measurement `epoch` (0 stops; see
    /// [`crate::ab_measure`]). A new epoch restarts the meter.
    /// Allocation-free.
    pub fn set_measure_epoch(&mut self, epoch: u32) {
        self.meter.set_epoch(epoch);
    }

    /// The oldest completed measurement hop not yet taken.
    pub fn pop_measure_hop(&mut self) -> Option<AbHop> {
        self.meter.pop()
    }

    /// Overs counted to A and B since the pair was built or the last
    /// [`Self::select`]: each limiter event (a held-headroom step or a
    /// limiting onset) and each output frame the quantizer clips, split by
    /// the crossfade weight of the frame it belongs to. With two quantizers
    /// ([`Self::compares_output`]) a clipped frame counts whole to the side
    /// whose quantizer produced it.
    pub fn overs(&self) -> [f64; 2] {
        self.overs
    }

    pub fn output_rate(&self) -> u32 {
        self.ratio
            .output_rate(self.sample_rate)
            .unwrap_or(self.sample_rate)
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The peak limiter stage, when the config enables one.
    pub fn limiter(&self) -> Option<&Limiter> {
        self.limiter.as_ref()
    }

    pub fn limiter_mut(&mut self) -> Option<&mut Limiter> {
        self.limiter.as_mut()
    }

    /// The input (graph) rate the chain was built for.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn dc_block_a(&self) -> Option<&DcBlock> {
        self.a.dc_block.as_ref()
    }

    pub fn dc_block_a_mut(&mut self) -> Option<&mut DcBlock> {
        self.a.dc_block.as_mut()
    }

    pub fn dc_block_b(&self) -> Option<&DcBlock> {
        self.b.dc_block.as_ref()
    }

    pub fn dc_block_b_mut(&mut self) -> Option<&mut DcBlock> {
        self.b.dc_block.as_mut()
    }

    pub fn ratio(&self) -> RateRatio {
        self.ratio
    }

    /// Output frames the next call with `frames` input frames writes (both
    /// sides run on the same frame grid).
    pub fn output_frames(&self, frames: usize) -> usize {
        self.a.output_frames(frames)
    }

    /// Most output frames one call with `max_block` input frames can write.
    pub fn max_output_frames(&self) -> usize {
        self.ratio.max_frames_out(self.max_block)
    }

    /// Input frames processed, modulo `ratio.down` (see
    /// [`Chain::input_pos`]).
    pub fn input_pos(&self) -> u64 {
        self.a.pos
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
    ///
    /// Restarts the over counts ([`Self::overs`]).
    pub fn select(&mut self, side: Side, crossfade_frames: usize) {
        self.current = side;
        self.fade.start(side.weight_b(), crossfade_frames);
        self.overs = [0.0; 2];
    }

    /// `out`: interleaved left-justified i32, at least
    /// [`Self::output_frames`]`(frames) * channels` samples.
    pub fn process_f32(&mut self, input: &[f32], out: &mut [i32]) -> ClipStats {
        let mut unity = StereoGainRamp::unity(self.output_rate());
        self.process_f32_with_gain(input, out, &mut unity)
    }

    /// Like [`AbChain::process_f32`], applying `gain` in `f64` immediately
    /// before the shared limiter and quantizer/dither.
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
        self.process_block_in(BlockIn::F32(input), out, gain, tap)
    }

    /// Like [`AbChain::process_f32_with_gain_tapped`], from planar `f64`
    /// input (see [`Chain::process_planar_with_gain_tapped`]; an A/B pair
    /// has no passthrough shortcut, so the flag only documents the input).
    pub fn process_planar_with_gain_tapped(
        &mut self,
        input: &[&[f64]],
        out: &mut [i32],
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
        transparent: bool,
    ) -> ClipStats {
        self.process_block_in(BlockIn::Planar { input, transparent }, out, gain, tap)
    }

    fn process_block_in(
        &mut self,
        input: BlockIn<'_>,
        out: &mut [i32],
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
    ) -> ClipStats {
        let channels = self.channels;
        let frames = input.frames(channels);
        assert!(
            frames <= self.max_block,
            "{frames} frames is more than the {} frame max_block",
            self.max_block
        );
        let out_frames = self.output_frames(frames);
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
            quantizer_b,
            discard,
            limiter,
            fade,
            level,
            coef_a,
            coef_b,
            weights_out,
            weight_delay,
            weight_pos,
            overs,
            meter,
            ..
        } = self;

        input.load(&mut a.planar, frames, channels);
        input.load(&mut b.planar, frames, channels);
        let (a_buf, _) = a.process(frames);
        let (b_buf, _) = b.process(frames);
        delay_a.process(a_buf, out_frames);
        delay_b.process(b_buf, out_frames);
        // Both sides as they leave their chains, aligned, before the gains.
        meter.push(a_buf, b_buf, out_frames);

        for i in 0..out_frames {
            let w = fade.next();
            let (ga, gb) = level.next();
            weights[i] = w;
            coef_a[i] = (1.0 - w) * ga;
            coef_b[i] = w * gb;
        }
        for (c, dst) in mix.iter_mut().enumerate() {
            let ac = &a_buf[c];
            let bc = &b_buf[c];
            for (i, s) in dst[..out_frames].iter_mut().enumerate() {
                *s = coef_a[i] * ac[i] + coef_b[i] * bc[i];
            }
        }

        let chain_peaks = planar_peaks(mix, out_frames);
        let volume_before = gain.current();
        gain.process_planar(mix, out_frames);
        let mut block_overs = [0.0; 2];
        let count = |overs: &mut [f64; 2], w: f64| {
            overs[0] += 1.0 - w;
            overs[1] += w;
        };
        if let Some(limiter) = limiter {
            limiter.set_channel_gains(block_gain(volume_before, gain.current()));
            limiter.set_ramping(volume_before != gain.current());
            make_chans!(limited, mix, channels, out_frames);
            // The frame that caused an event entered `age` frames earlier:
            // in this block, or still in the ring of the previous ones.
            let len = weight_delay.len();
            limiter.process_with(limited, |i, age| {
                let w = if age <= i {
                    weights[i - age]
                } else {
                    weight_delay[(*weight_pos + len - (age - i).min(len)) % len]
                };
                count(&mut block_overs, w);
            });
        }
        // The frames leaving the limiter entered it its latency earlier.
        if weight_delay.is_empty() {
            weights_out[..out_frames].copy_from_slice(&weights[..out_frames]);
        } else {
            for (out, &w) in weights_out[..out_frames]
                .iter_mut()
                .zip(&weights[..out_frames])
            {
                *out = std::mem::replace(&mut weight_delay[*weight_pos], w);
                *weight_pos = (*weight_pos + 1) % weight_delay.len();
            }
        }
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
        let mut stats = match quantizer_b {
            None => {
                // One over per clipped frame, whatever the channels.
                let mut last_clip = usize::MAX;
                quantizer.process_with(inputs, &mut out[..out_frames * channels], |i| {
                    if i != last_clip {
                        last_clip = i;
                        count(&mut block_overs, weights_out[i]);
                    }
                })
            }
            Some(quantizer_b) => {
                // Runs of frames from one quantizer: B's where the frame's B
                // weight is at least one half. Both quantize every frame;
                // the other one's frames go to `discard`.
                let mut stats = ClipStats::default();
                let mut start = 0;
                while start < out_frames {
                    let from_b = weights_out[start] >= 0.5;
                    let end = weights_out[start..out_frames]
                        .iter()
                        .position(|&w| (w >= 0.5) != from_b)
                        .map_or(out_frames, |n| start + n);
                    let mut run: [&[f64]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
                    for (slot, chan) in run.iter_mut().zip(inputs.iter()) {
                        *slot = &chan[start..end];
                    }
                    let run = &run[..channels];
                    let (heard, other) = if from_b {
                        (&mut *quantizer_b, &mut *quantizer)
                    } else {
                        (&mut *quantizer, &mut *quantizer_b)
                    };
                    let samples = start * channels..end * channels;
                    let side = usize::from(from_b);
                    let mut last_clip = usize::MAX;
                    stats.merge(heard.process_with(run, &mut out[samples.clone()], |i| {
                        if i != last_clip {
                            last_clip = i;
                            block_overs[side] += 1.0;
                        }
                    }));
                    other.process(run, &mut discard[samples]);
                    start = end;
                }
                stats
            }
        };
        overs[0] += block_overs[0];
        overs[1] += block_overs[1];
        stats.chain_peak_ch = chain_peaks;
        stats.ab_overs = block_overs;
        stats.limiter = limiter.as_mut().map_or(
            audivane_protocol::LimiterStatus::default(),
            Limiter::take_status,
        );
        stats
    }

    pub fn reset(&mut self) {
        self.reset_to(0);
    }

    /// [`Self::reset`], continuing the rational frame grid at `pos` (see
    /// [`Chain::reset_to`]).
    pub fn reset_to(&mut self, pos: u64) {
        if let Some(limiter) = &mut self.limiter {
            limiter.reset();
        }
        self.a.reset_to(pos);
        self.b.reset_to(pos);
        self.delay_a.reset();
        self.delay_b.reset();
        self.meter.reset();
        self.quantizer.reset();
        if let Some(q) = &mut self.quantizer_b {
            q.reset();
        }
        self.fade.reset_to(self.current.weight_b());
        self.level.reset();
        self.weight_delay.fill(self.current.weight_b());
        self.weight_pos = 0;
    }

    /// Analytic offset (dB to apply to B) from two configs alone, via
    /// [`crate::ab_level::analytic_offset_db`]: DC block, EQ, crossfeed at
    /// [`crate::crossfeed::MATCHED_CORRELATION`], the harmonics stage's
    /// simulated loudness change, and each enabled convolution's `gain_db` only (the IR samples are not known here). The
    /// built pair ([`Self::new`]) uses the IRs' and upsamplers' magnitudes
    /// as well.
    pub fn analytic_offset_db(
        a: &ChainConfig,
        b: &ChainConfig,
        sample_rate: u32,
        weighting: Weighting,
    ) -> Result<f64, String> {
        let level = AbLevel {
            offset: AbOffset::Analytic(weighting),
            headroom: audivane_protocol::ipc::AbHeadroom::Fixed { db: 0.0 },
        };
        Ok(Self::estimate_gains(a, b, sample_rate, level)?.offset_db)
    }

    /// The gains [`Self::new`] would apply, from the configs alone (the
    /// estimate of [`Self::analytic_offset_db`]; convolution enters by its
    /// `gain_db`). For status before a pair is built.
    pub fn estimate_gains(
        a: &ChainConfig,
        b: &ChainConfig,
        sample_rate: u32,
        level: AbLevel,
    ) -> Result<AbGains, String> {
        let sr = f64::from(sample_rate);
        let dc = |c: &ChainConfig| {
            c.dc_block
                .as_ref()
                .filter(|d| d.is_active())
                .map(|d| DcBlock::try_new(d, sr, 2))
                .transpose()
                .map_err(|e| e.to_string())
        };
        let crossfeed = |c: &ChainConfig| {
            c.crossfeed
                .as_ref()
                .filter(|x| x.is_active())
                .map(|x| Crossfeed::try_new(x, sr, 2))
                .transpose()
                .map_err(|e| e.to_string())
        };
        let eq = |c: &ChainConfig| c.eq.as_ref().map(|p| Eq::new(p, sr, 2)).transpose();
        let harmonics_gain = |c: &ChainConfig| -> Result<f64, String> {
            match c.harmonics.as_ref() {
                // The simulation feeds only the analytic offset.
                Some(h) if matches!(level.offset, AbOffset::Analytic(_)) => {
                    Harmonics::loudness_change_db(h)
                        .map(db_to_lin)
                        .map_err(|e| e.to_string())
                }
                _ => Ok(1.0),
            }
        };
        let conv_gain = |c: &ChainConfig| -> f64 {
            c.convolution
                .iter()
                .filter(|conv| conv.enabled)
                .map(|conv| db_to_lin(conv.gain_db))
                .product()
        };
        let (dc_a, dc_b) = (dc(a)?, dc(b)?);
        let (xf_a, xf_b) = (crossfeed(a)?, crossfeed(b)?);
        let (eq_a, eq_b) = (eq(a)?, eq(b)?);
        let ta = SideTerms {
            sample_rate: sr,
            dc: dc_a.as_ref(),
            crossfeed: xf_a.as_ref(),
            eq: eq_a.as_ref(),
            eq_profile: a.eq.as_ref(),
            conv: ConvTerm::Gain(conv_gain(a)),
            upsampler: None,
            harmonics_gain: harmonics_gain(a)?,
        };
        let tb = SideTerms {
            sample_rate: sr,
            dc: dc_b.as_ref(),
            crossfeed: xf_b.as_ref(),
            eq: eq_b.as_ref(),
            eq_profile: b.eq.as_ref(),
            conv: ConvTerm::Gain(conv_gain(b)),
            upsampler: None,
            harmonics_gain: harmonics_gain(b)?,
        };
        let offset_db = match level.offset {
            AbOffset::Analytic(w) => crate::ab_level::analytic_offset_db(&ta, &tb, w),
            AbOffset::Fixed(db) => db,
        };
        // The output headroom is cut before the mix (see above).
        let cut = a.output.headroom_db;
        Ok(AbGains::new(
            offset_db,
            level.headroom,
            ta.peak_gain_db() - cut,
            tb.peak_gain_db() - cut,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{Band, DitherKind, EqProfile, FilterType, OutputConfig, RateTarget};

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
            headroom_db: 0.0,
        }
    }

    #[test]
    fn explicit_dither_runs_without_other_stages() {
        let config = ChainConfig {
            output: OutputConfig {
                bits: 16,
                dither: DitherKind::Resonant5,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        };
        let mut chain = Chain::new(ChainSpec {
            config,
            sample_rate: 352_800,
            channels: 1,
            max_block: 4096,
            irs: None,
        });
        assert!(!chain.is_bit_transparent());
        assert_eq!(chain.latency_out_frames(), 0);
        let input = [0.0; 4096];
        let mut output = [0; 4096];
        chain.process_f32(&input, &mut output);
        assert!(output.iter().any(|&x| x != 0));
    }

    #[test]
    fn shaped_dither_uses_the_output_rate_shaper() {
        let output = OutputConfig {
            bits: 16,
            dither: DitherKind::Shaped,
            headroom_db: 0.0,
        };
        let eq = Some(flat_profile(-1.0));
        for (ratio, out_rate) in [(1u32, 48_000u32), (4, 192_000)] {
            let config = ChainConfig {
                eq: eq.clone(),
                upsampler: crate::resample::ManualConfig {
                    target: if ratio == 1 {
                        RateTarget::Off
                    } else {
                        RateTarget::Ratio(ratio)
                    },
                    taps: 256,
                    ..Default::default()
                }
                .into_upsampler_config()
                .unwrap(),
                output,
                ..ChainConfig::neutral()
            };
            let chain = Chain::new(spec(config.clone(), 2, 64));
            assert_eq!(chain.output_rate(), out_rate);
            assert_eq!(
                chain.quantizer.shaper(),
                crate::dither::noise_shaper(out_rate).map(|h| &h[..])
            );
            let ab = AbChain::new(
                spec(config.clone(), 2, 64),
                spec(config, 2, 64),
                AbLevel::fixed(0.0),
            )
            .unwrap();
            assert_eq!(
                ab.quantizer.shaper(),
                crate::dither::noise_shaper(out_rate).map(|h| &h[..])
            );
        }
    }

    /// The planar f64 entry points (the processor's one-block handoff)
    /// give bit for bit what the f32 ones give on the same samples, for a
    /// passthrough chain, an EQ with dither, an integer and a rational
    /// upsampler, and an A/B pair, over blocks of varying length, with the
    /// taps compared too.
    #[test]
    fn planar_input_is_bit_identical_to_f32_input() {
        let eq = ChainConfig {
            eq: Some(EqProfile {
                name: "eq".into(),
                preamp_db: -3.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 1000.0,
                    gain_db: 2.0,
                    q: 1.0,
                    enabled: true,
                }],
                ..Default::default()
            }),
            output: OutputConfig {
                bits: 24,
                dither: DitherKind::Tpdf,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        };
        let mut up4 = eq.clone();
        up4.upsampler = crate::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 256,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut rational = eq.clone();
        rational.upsampler = crate::resample::ManualConfig {
            target: RateTarget::Rate(44_100),
            taps: 32,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.95,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let configs = [ChainConfig::neutral(), eq.clone(), up4, rational];
        let blocks = [1usize, 7, 64, 333, 512, 0, 129];
        let channels = 2;
        let mut x = 0x1234_5678_u32;
        let mut noise = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            ((x as i32) >> 8) as f32 / 8_388_608.0 * 0.7
        };
        for config in configs {
            let mut a = Chain::new(spec(config.clone(), channels, 512));
            let mut b = Chain::new(spec(config.clone(), channels, 512));
            let mut ab_a = AbChain::new(
                spec(config.clone(), channels, 512),
                spec(eq.clone(), channels, 512),
                AbLevel::fixed(-1.0),
            )
            .ok();
            let mut ab_b = AbChain::new(
                spec(config.clone(), channels, 512),
                spec(eq.clone(), channels, 512),
                AbLevel::fixed(-1.0),
            )
            .ok();
            for (k, &n) in blocks.iter().cycle().take(40).enumerate() {
                let input: Vec<f32> = (0..n * channels).map(|_| noise()).collect();
                let planar: Vec<Vec<f64>> = (0..channels)
                    .map(|c| (0..n).map(|i| f64::from(input[i * channels + c])).collect())
                    .collect();
                let refs: Vec<&[f64]> = planar.iter().map(Vec::as_slice).collect();
                let n_out = a.output_frames(n);
                assert_eq!(n_out, b.output_frames(n));
                let (mut oa, mut ob) = (vec![0; n_out * channels], vec![0; n_out * channels]);
                let (mut ta, mut tb) = (vec![0.0; n_out * channels], vec![0.0; n_out * channels]);
                let mut ga = StereoGainRamp::unity(a.output_rate());
                let mut gb = StereoGainRamp::unity(b.output_rate());
                if k % 3 == 1 {
                    ga.set_volume_targets(-6.0, 0.0, false, false, false, true);
                    gb.set_volume_targets(-6.0, 0.0, false, false, false, true);
                }
                let sa = a.process_f32_with_gain_tapped(&input, &mut oa, &mut ga, Some(&mut ta));
                let sb =
                    b.process_planar_with_gain_tapped(&refs, &mut ob, &mut gb, Some(&mut tb), true);
                assert_eq!(oa, ob, "block {k}");
                assert_eq!(
                    ta.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    tb.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
                );
                assert_eq!(format!("{sa:?}"), format!("{sb:?}"));
                if let (Some(x), Some(y)) = (ab_a.as_mut(), ab_b.as_mut()) {
                    let n_out = x.output_frames(n);
                    let (mut oa, mut ob) = (vec![0; n_out * channels], vec![0; n_out * channels]);
                    let mut ga = StereoGainRamp::unity(x.output_rate());
                    let mut gb = StereoGainRamp::unity(y.output_rate());
                    x.process_f32_with_gain_tapped(&input, &mut oa, &mut ga, None);
                    y.process_planar_with_gain_tapped(&refs, &mut ob, &mut gb, None, true);
                    assert_eq!(oa, ob, "A/B block {k}");
                }
            }
        }
    }

    /// A planar block a stage changed (not `transparent`) never takes the
    /// passthrough shortcut: it is quantized like processed audio.
    #[test]
    fn a_changed_planar_block_is_quantized_not_copied() {
        let config = ChainConfig {
            output: OutputConfig {
                bits: 16,
                dither: DitherKind::Tpdf,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        };
        let mut chain = Chain::new(spec(config, 1, 64));
        let x = [0.123_456_789_f64; 64];
        let mut out = [0; 64];
        let mut gain = StereoGainRamp::unity(48_000);
        chain.process_planar_with_gain_tapped(&[&x[..]], &mut out, &mut gain, None, true);
        let copied = (0.123_456_789_f64 * 2_147_483_648.0).round() as i32;
        assert!(out.iter().all(|&s| s == copied));
        chain.process_planar_with_gain_tapped(&[&x[..]], &mut out, &mut gain, None, false);
        assert!(out.iter().all(|&s| s & 0xffff == 0), "quantized to 16 bits");
    }

    #[test]
    fn default_chain_is_bit_transparent() {
        let chain = Chain::new(spec(ChainConfig::neutral(), 2, 64));
        assert!(chain.is_bit_transparent());
        assert_eq!(chain.ratio(), RateRatio::integer(1));
        assert_eq!(chain.output_rate(), 48_000);
        assert_eq!(chain.latency_out_frames(), 0);
        assert_eq!(chain.channels(), 2);
    }

    #[test]
    fn tilt_invalid_at_stream_rate_is_a_chain_build_error() {
        use audivane_protocol::{TiltConfig, TiltPivot};
        for slope in [0.0, 1.0] {
            let config = ChainConfig {
                eq: Some(EqProfile {
                    tilt: Some(TiltConfig {
                        enabled: true,
                        slope_db_per_oct: Some(slope),
                        total_db: None,
                        start_hz: 17_750.0,
                        end_hz: 20_000.0,
                        pivot: TiltPivot::Start,
                    }),
                    ..EqProfile::default()
                }),
                ..ChainConfig::neutral()
            };
            let mut low = spec(config.clone(), 2, 64);
            low.sample_rate = 44_100;
            match Chain::try_new(low) {
                Err(ChainError::Eq(error)) => assert!(error.contains("at least 1/6 octave")),
                Err(other) => panic!("unexpected error {other}"),
                Ok(_) => panic!("invalid tilt built at 44.1 kHz"),
            }
            let mut high = spec(config, 2, 64);
            high.sample_rate = 96_000;
            assert!(Chain::try_new(high).is_ok());
        }
    }

    #[test]
    fn an_upsampler_that_cannot_be_designed_is_an_error() {
        // Minimum phase at beta 40 (the daemon rejects it) cannot meet its
        // tolerances: the chain refuses it instead of running an
        // approximation (`crate::minphase`).
        let config = ChainConfig {
            upsampler: crate::resample::ManualConfig {
                target: RateTarget::Ratio(4),
                taps: 512,
                phase: audivane_protocol::FilterPhase::Minimum,
                kaiser_beta: 40.0,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            ..ChainConfig::neutral()
        };
        match Chain::try_new(spec(config, 2, 64)) {
            Err(ChainError::Upsampler(msg)) => {
                assert!(msg.contains("minimum-phase"), "{msg}");
            }
            Err(other) => panic!("unexpected error {other}"),
            Ok(_) => panic!("built a chain with an untoleranced kernel"),
        }
    }

    /// A Custom chain of 4096 output taps, upsampling only: x4 from 48 kHz,
    /// or 44.1 kHz to 48 kHz (160/147).
    fn custom_spec(rational: bool, phase: audivane_protocol::FilterPhase) -> ChainSpec {
        let config = ChainConfig {
            upsampler: crate::resample::ManualConfig {
                target: if rational {
                    RateTarget::Rate(48_000)
                } else {
                    RateTarget::Ratio(4)
                },
                taps: 4096,
                phase,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let mut s = spec(config, 2, 256);
        s.sample_rate = if rational { 44_100 } else { 48_000 };
        s
    }

    /// Custom minimum phase reports its impulse peak: block latency + peak at
    /// an integer ratio (1024 + 19), grid peak / M on a rational grid
    /// (823 / 147 = 5.6, rounded to 6). The chain sums it.
    #[test]
    fn custom_minimum_phase_chain_latency_is_its_peak() {
        use audivane_protocol::FilterPhase::Minimum;
        let mut x4 = Chain::try_new(custom_spec(false, Minimum)).unwrap();
        assert_eq!(x4.latency_out_frames(), 1043);
        assert_eq!(
            x4.upsampler_runtime_mut().unwrap().latency_out_exact(),
            1043.0
        );

        let mut r = Chain::try_new(custom_spec(true, Minimum)).unwrap();
        assert_eq!(r.ratio(), RateRatio::from_rates(44_100, 48_000));
        assert_eq!(r.latency_out_frames(), 6);
        let exact = r.upsampler_runtime_mut().unwrap().latency_out_exact();
        assert_eq!(exact, 823.0 / 147.0);

        // The reported delay is where the impulse peak comes out.
        let mut block = vec![0.0f32; 256 * 2];
        block[0] = 0.5;
        block[1] = 0.5;
        let mut out = vec![0i32; 256 * 4 * 2];
        let mut y = Vec::new();
        while y.len() < 4096 {
            x4.process_f32(&block, &mut out);
            y.extend(out.chunks(2).map(|f| f64::from(f[0])));
            block[..2].fill(0.0);
        }
        let peak = (0..y.len()).fold(0, |b, i| if y[i].abs() > y[b].abs() { i } else { b });
        assert_eq!(peak, 1043);
    }

    /// A/B delay matching pads the shorter side to the longer latency: a
    /// linear A against a Custom minimum-phase B pads B by the difference to
    /// its peak (x4: 3071 - 1043 = 2028; 160/147: 2048 - 6 = 2042).
    #[test]
    fn ab_pads_custom_minimum_phase_to_its_peak() {
        use audivane_protocol::FilterPhase::{Linear, Minimum};
        for (rational, total, pad_b) in [(false, 3071, 2028), (true, 2048, 2042)] {
            let ab = AbChain::new(
                custom_spec(rational, Linear),
                custom_spec(rational, Minimum),
                AbLevel::fixed(0.0),
            )
            .unwrap();
            assert_eq!(ab.latency_out_frames(), total, "rational {rational}");
            assert_eq!(ab.delay_a.len, 0, "rational {rational}");
            assert_eq!(ab.delay_b.len, pad_b, "rational {rational}");
        }
    }

    #[test]
    fn upsampling_sets_ratio_rate_and_latency() {
        let config = ChainConfig {
            upsampler: crate::resample::ManualConfig {
                target: RateTarget::Ratio(4),
                taps: 1024,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let chain = Chain::new(spec(config, 1, 64));
        assert!(!chain.is_bit_transparent());
        assert_eq!(chain.ratio(), RateRatio::integer(4));
        assert_eq!(chain.output_rate(), 192_000);
        assert!(chain.latency_out_frames() > 0);
    }

    /// Peak before quantization and clipped samples for an fs/4 sine at 45
    /// degrees whose samples peak at -0.1 dBFS (its true peak is ~+2.9 dBFS).
    fn intersample_over(headroom_db: f64) -> (f64, u64) {
        let config = ChainConfig {
            upsampler: crate::resample::ManualConfig {
                target: RateTarget::Ratio(4),
                taps: 4096,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            output: OutputConfig {
                headroom_db,
                ..quiet_output()
            },
            ..ChainConfig::neutral()
        };
        let mut chain = Chain::new(spec(config, 1, 256));
        let amp = db_to_lin(-0.1);
        let x: Vec<f32> = (0..256)
            .map(|n| (amp * (PI / 2.0 * n as f64 + PI / 4.0).sin() / (PI / 4.0).sin().abs()) as f32)
            .map(|v| v.clamp(-amp as f32, amp as f32))
            .collect();
        let mut out = vec![0i32; 256 * 4];
        let (mut peak, mut clipped) = (0.0_f64, 0u64);
        for _ in 0..64 {
            let stats = chain.process_f32(&x, &mut out);
            peak = peak.max(stats.peak);
            clipped += stats.clipped;
        }
        (peak, clipped)
    }

    #[test]
    fn upsampler_headroom_keeps_intersample_overs_from_clipping() {
        let (peak, clipped) = intersample_over(0.0);
        assert!(
            crate::lin_to_db(peak) > 2.5,
            "true peak {:.2} dBFS",
            crate::lin_to_db(peak)
        );
        assert!(clipped > 0);
        let (peak, clipped) = intersample_over(3.5);
        assert!(
            crate::lin_to_db(peak) < -0.5,
            "true peak {:.2} dBFS",
            crate::lin_to_db(peak)
        );
        assert_eq!(clipped, 0);
    }

    /// The chain's quantized output for `config` (32-bit, no dither) over
    /// eight blocks of a fixed input.
    fn render(mut config: ChainConfig) -> Vec<i32> {
        config.output = OutputConfig {
            headroom_db: config.output.headroom_db,
            ..quiet_output()
        };
        let mut chain = Chain::new(spec(config, 2, 64));
        let x: Vec<f32> = (0..128).map(|n| 0.5 * ((n as f32) * 0.37).sin()).collect();
        let mut all = Vec::new();
        for _ in 0..8 {
            let mut out = vec![0i32; chain.output_frames(64) * 2];
            chain.process_f32(&x, &mut out);
            all.extend(out);
        }
        all
    }

    #[test]
    fn output_headroom_applies_at_every_ratio_and_after_harmonics() {
        let harmonics = Some(audivane_protocol::HarmonicsConfig {
            enabled: true,
            character: audivane_protocol::HarmonicsCharacter::Triode,
            ..Default::default()
        });
        let x4 = crate::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 256,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let eq = Some(flat_profile(-1.0));
        let g = db_to_lin(-6.0);
        for (upsampler, harmonics) in [
            (audivane_protocol::UpsamplerConfig::default(), None),
            (
                audivane_protocol::UpsamplerConfig::default(),
                harmonics.clone(),
            ),
            (x4.clone(), None),
            (x4, harmonics),
        ] {
            let base = ChainConfig {
                eq: eq.clone(),
                upsampler,
                harmonics,
                ..ChainConfig::neutral()
            };
            let mut cut = base.clone();
            cut.output.headroom_db = 6.0;
            let (want, got) = (render(base), render(cut));
            assert_eq!(want.len(), got.len());
            for (&a, &b) in want.iter().zip(&got) {
                // After the non-linear harmonics stage too: a plain gain on
                // the 0 dB output.
                let expected = (f64::from(a) * g).round();
                assert!((f64::from(b) - expected).abs() <= 2.0, "{a} {b}");
            }
        }
    }

    #[test]
    fn output_headroom_alone_leaves_passthrough() {
        let mut config = ChainConfig::neutral();
        assert!(config.is_passthrough());
        config.output.headroom_db = 3.0;
        assert!(!config.is_passthrough());
        let chain = Chain::new(spec(config, 2, 64));
        assert!(!chain.is_bit_transparent());
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
        let mut chain = Chain::new(spec(ChainConfig::neutral(), 1, 64));
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
        let mut chain = Chain::new(spec(ChainConfig::neutral(), 2, 32));
        let mut out: Vec<i32> = Vec::new();
        assert_eq!(chain.process_f32(&[], &mut out), ClipStats::default());
        let mut ab = AbChain::new(
            spec(ChainConfig::neutral(), 2, 32),
            spec(ChainConfig::neutral(), 2, 32),
            AbLevel::fixed(0.0),
        )
        .unwrap();
        assert_eq!(ab.process_f32(&[], &mut out), ClipStats::default());
    }

    #[test]
    fn rejects_bad_specs() {
        assert_eq!(
            Chain::try_new(spec(ChainConfig::neutral(), 0, 16))
                .err()
                .unwrap(),
            ChainError::ZeroChannels
        );
        assert_eq!(
            Chain::try_new(spec(ChainConfig::neutral(), MAX_CHANNELS + 1, 16))
                .err()
                .unwrap(),
            ChainError::TooManyChannels {
                channels: MAX_CHANNELS + 1,
                max: MAX_CHANNELS
            }
        );
        assert_eq!(
            Chain::try_new(spec(ChainConfig::neutral(), 2, 0))
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
            ..ChainConfig::neutral()
        };
        assert_eq!(
            Chain::try_new(spec(config, 2, 16)).err().unwrap(),
            ChainError::MissingIr
        );
    }

    #[test]
    #[should_panic(expected = "max_block")]
    fn rejects_blocks_larger_than_max_block() {
        let mut chain = Chain::new(spec(ChainConfig::neutral(), 1, 4));
        let mut out = vec![0i32; 8];
        chain.process_f32(&[0.0; 8], &mut out);
    }

    #[test]
    fn quantizer_counts_clips_on_the_f64_path() {
        let mut config = ChainConfig::neutral();
        config.upsampler.target = RateTarget::Ratio(1); // not passthrough
        config.output = OutputConfig {
            bits: 16,
            dither: DitherKind::None,
            headroom_db: 0.0,
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
            ..ChainConfig::neutral()
        };
        let mut chain = Chain::new(spec(config, 2, 64));
        let input = vec![0.5f32; 128];
        let mut out = vec![0i32; input.len()];
        let mut gain = StereoGainRamp::unity(48_000);
        gain.set_volume_targets(-10.0, 0.0, false, false, false, false);
        let stats = chain.process_f32_with_gain_tapped(&input, &mut out, &mut gain, None);
        assert!(stats.measured);
        let db = |x: f64| 20.0 * x.log10();
        assert!(
            (db(stats.chain_peak_ch[0]) - 0.0).abs() < 0.1,
            "{:?}",
            stats.chain_peak_ch
        );
        assert!(
            (db(stats.peak_ch[0]) + 10.0).abs() < 0.1,
            "{:?}",
            stats.peak_ch
        );
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
            ..ChainConfig::neutral()
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
        let mut other_rate = spec(ChainConfig::neutral(), 2, 16);
        other_rate.sample_rate = 44_100;
        assert!(matches!(
            AbChain::new(
                spec(ChainConfig::neutral(), 2, 16),
                other_rate,
                AbLevel::fixed(0.0)
            ),
            Err(ChainError::RateMismatch { .. })
        ));
        assert!(matches!(
            AbChain::new(
                spec(ChainConfig::neutral(), 2, 16),
                spec(ChainConfig::neutral(), 1, 16),
                AbLevel::fixed(0.0)
            ),
            Err(ChainError::ChannelMismatch { .. })
        ));

        let mut a = ChainConfig::neutral();
        a.upsampler.target = RateTarget::Ratio(2);
        let mut b = ChainConfig::neutral();
        b.upsampler.target = RateTarget::Ratio(4);
        assert!(matches!(
            AbChain::new(spec(a, 2, 16), spec(b, 2, 16), AbLevel::fixed(0.0)),
            Err(ChainError::RatioMismatch { a, b })
                if a == RateRatio::integer(2) && b == RateRatio::integer(4)
        ));

        // Other bits and dither build B's own quantizer; other headroom
        // does not build.
        let mut c = ChainConfig::neutral();
        c.output.bits = 16;
        c.output.dither = DitherKind::Shaped;
        let ab = AbChain::new(
            spec(ChainConfig::neutral(), 2, 16),
            spec(c.clone(), 2, 16),
            AbLevel::fixed(0.0),
        )
        .unwrap();
        assert!(ab.compares_output());
        c.output.headroom_db += 1.0;
        assert!(matches!(
            AbChain::new(
                spec(ChainConfig::neutral(), 2, 16),
                spec(c, 2, 16),
                AbLevel::fixed(0.0)
            ),
            Err(ChainError::HeadroomMismatch)
        ));
    }

    #[test]
    fn ab_applies_never_boost_gains_and_ramps_changes() {
        let config = ChainConfig {
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let peak = |out: &[i32]| out.iter().map(|s| (*s as f64).abs()).fold(0.0, f64::max);
        let level_db = |p: f64| 20.0 * (p / (0.25 * 2_147_483_648.0)).log10();
        let frames = 32;
        let input = vec![0.25f32; frames * 2];
        let mut out = vec![0i32; frames * 2];
        // (offset, A's level, B's level): the louder side is cut, nothing
        // is raised.
        for (offset, want_a, want_b) in [(-6.0, 0.0, -6.0), (6.0, -6.0, 0.0), (0.0, 0.0, 0.0)] {
            let mut ab = AbChain::new(
                spec(config.clone(), 2, 64),
                spec(config.clone(), 2, 64),
                AbLevel::fixed(offset),
            )
            .unwrap();
            assert_eq!(ab.current(), Side::A);
            assert_eq!(ab.level_offset_db(), offset);
            let g = ab.gains();
            assert_eq!(
                (g.gain_a_db, g.gain_b_db, g.headroom_db),
                (want_a, want_b, 0.0)
            );
            assert_eq!(ab.output_rate(), 48_000);
            assert_eq!(ab.ratio(), RateRatio::integer(1));
            ab.process_f32(&input, &mut out);
            assert!((level_db(peak(&out)) - want_a).abs() < 1e-3, "{offset}: A");
            ab.select(Side::B, 0);
            assert_eq!(ab.current(), Side::B);
            ab.process_f32(&input, &mut out);
            assert!((level_db(peak(&out)) - want_b).abs() < 1e-3, "{offset}: B");
        }

        // A new offset ramps over 50 ms (2400 frames): never a step.
        let mut ab = AbChain::new(
            spec(config.clone(), 2, 64),
            spec(config, 2, 64),
            AbLevel::fixed(-6.0),
        )
        .unwrap();
        ab.select(Side::B, 0);
        ab.process_f32(&input, &mut out);
        ab.set_level_offset_db(0.0);
        let mut last = peak(&out);
        let mut largest_step: f64 = 0.0;
        for _ in 0..(2400 / frames + 2) {
            ab.process_f32(&input, &mut out);
            for f in out.as_chunks::<2>().0 {
                let v = f64::from(f[0]);
                largest_step = largest_step.max((v - last).abs());
                last = v;
            }
        }
        assert!(
            level_db(last).abs() < 1e-3,
            "after the ramp: {}",
            level_db(last)
        );
        // 6 dB over 2400 frames on a 0.25 DC: under 0.0001 of full scale.
        assert!(largest_step / 2_147_483_648.0 < 1e-4, "step {largest_step}");
    }

    /// The measurement hops see both sides before the level gains, the
    /// output does not change while metering, and epoch 0 meters nothing.
    #[test]
    fn ab_measure_hops_see_both_sides_and_leave_the_output_alone() {
        let a = ChainConfig {
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let b = ChainConfig {
            eq: Some(flat_profile(-6.0)),
            ..a.clone()
        };
        let build = || {
            AbChain::new(
                spec(a.clone(), 2, 256),
                spec(b.clone(), 2, 256),
                AbLevel::fixed(6.0),
            )
            .unwrap()
        };
        let mut seed = 3u64;
        let input: Vec<f32> = (0..256 * 2)
            .map(|_| {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                ((seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5) as f32 * 0.5
            })
            .collect();
        let (mut plain, mut metered) = (build(), build());
        metered.set_measure_epoch(5);
        let (mut out_p, mut out_m) = (vec![0i32; 512], vec![0i32; 512]);
        let mut hops = Vec::new();
        for _ in 0..(48_000 / 256) {
            plain.process_f32(&input, &mut out_p);
            metered.process_f32(&input, &mut out_m);
            assert_eq!(out_p, out_m);
            while let Some(h) = metered.pop_measure_hop() {
                hops.push(h);
            }
            assert_eq!(plain.pop_measure_hop(), None);
        }
        assert_eq!(hops.len(), 9);
        for h in &hops[1..] {
            assert_eq!(h.epoch, 5);
            let db = 10.0 * (h.a / h.b).log10();
            assert!((db - 6.0).abs() < 1e-6, "{db}");
        }
    }

    /// A = B with different latencies (a delta IR through the block convolver
    /// on A only). Aligned, the mix during a fade equals A: a null below
    /// -300 dB.
    #[test]
    fn ab_alignment_nulls_during_a_fade() {
        let a = ChainConfig {
            convolution: vec![audivane_protocol::ConvolutionConfig {
                name: None,
                enabled: true,
                ir_path: "delta.wav".into(),
                gain_db: 0.0,
            }],
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let b = ChainConfig {
            dc_block: None,
            output: quiet_output(),
            ..ChainConfig::default()
        };
        let pair = |fade: bool| {
            let mut spec_a = spec(a.clone(), 2, 256);
            spec_a.irs = Some(vec![vec![1.0]]);
            let level = AbLevel {
                offset: AbOffset::Analytic(Weighting::K),
                headroom: audivane_protocol::ipc::AbHeadroom::Auto,
            };
            let mut ab = AbChain::new(spec_a, spec(b.clone(), 2, 256), level).unwrap();
            assert!(ab.a.latency_out > ab.b.latency_out);
            assert_eq!(ab.gains(), AbGains::default());
            let mut seed = 0x1234_5678_u64;
            let mut out = vec![0i32; 512];
            let mut mixes = Vec::new();
            for block in 0..40 {
                if fade && block == 10 {
                    ab.select(Side::B, 4000);
                }
                let input: Vec<f32> = (0..512)
                    .map(|_| {
                        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        ((seed >> 40) as f64 / (1u64 << 24) as f64 - 0.5) as f32
                    })
                    .collect();
                ab.process_f32(&input, &mut out);
                for chan in &ab.mix[..2] {
                    mixes.extend_from_slice(&chan[..256]);
                }
            }
            mixes
        };
        let only_a = pair(false);
        let faded = pair(true);
        let peak = only_a.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
        let err = only_a
            .iter()
            .zip(&faded)
            .fold(0.0_f64, |m, (x, y)| m.max((x - y).abs()));
        assert!(peak > 0.1);
        let null_db = 20.0 * (err / peak).log10();
        assert!(err == 0.0 || null_db < -300.0, "null {null_db} dB");
    }

    #[test]
    fn ab_crossfade_is_click_free_and_equal_gain() {
        let config = ChainConfig {
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let mut ab = AbChain::new(
            spec(config.clone(), 1, 64),
            spec(config, 1, 64),
            AbLevel::fixed(0.0),
        )
        .unwrap();
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
            upsampler: crate::resample::ManualConfig {
                target: RateTarget::Ratio(2),
                taps: 512,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let b = ChainConfig {
            upsampler: crate::resample::ManualConfig {
                target: RateTarget::Ratio(2),
                taps: 2048,
                ..Default::default()
            }
            .into_upsampler_config()
            .unwrap(),
            output: quiet_output(),
            ..ChainConfig::neutral()
        };

        let chain_a = Chain::new(spec(a.clone(), 1, 32));
        let chain_b = Chain::new(spec(b.clone(), 1, 32));
        let expected = chain_a
            .latency_out_frames()
            .max(chain_b.latency_out_frames());
        let mut ab = AbChain::new(spec(a, 1, 32), spec(b, 1, 32), AbLevel::fixed(0.0)).unwrap();
        assert_eq!(ab.latency_out_frames(), expected);
        assert_eq!(ab.ratio(), RateRatio::integer(2));

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
    fn analytic_offset_includes_harmonics_gain() {
        use audivane_protocol::{HarmonicsCharacter, HarmonicsConfig};
        let off = ChainConfig::neutral();
        let harm = |h2_db, h3_db, h4_db, h5_db| ChainConfig {
            harmonics: Some(HarmonicsConfig {
                enabled: true,
                character: HarmonicsCharacter::Custom,
                h2_db,
                h3_db,
                h4_db,
                h5_db,
                ..Default::default()
            }),
            ..ChainConfig::neutral()
        };
        let rate = 48_000;
        for (cfg, lo, hi) in [
            (
                ChainConfig {
                    harmonics: Some(Default::default()),
                    ..ChainConfig::neutral()
                },
                -0.3,
                0.1,
            ),
            (harm(-20.0, -120.0, None, None), -2.0, -0.8),
            (harm(-20.0, -20.0, Some(-20.0), Some(-20.0)), 0.5, 3.0),
        ] {
            let change = Harmonics::loudness_change_db(cfg.harmonics.as_ref().unwrap()).unwrap();
            eprintln!("{:?}: {change:.3} dB", cfg.harmonics);
            assert!((lo..hi).contains(&change), "{change}");
            let offset = AbChain::analytic_offset_db(&off, &cfg, rate, Weighting::K).unwrap();
            assert!((offset + change).abs() < 1e-6, "{offset} vs {change}");
        }
    }

    #[test]
    fn analytic_offset_includes_crossfeed_level() {
        use audivane_protocol::{CrossfeedConfig, CrossfeedLevel};
        let xf = |level| ChainConfig {
            crossfeed: Some(CrossfeedConfig {
                level,
                ..Default::default()
            }),
            ..ChainConfig::neutral()
        };
        let off = ChainConfig::neutral();
        let safe = xf(CrossfeedLevel::PeakSafe);
        let matched = xf(CrossfeedLevel::Matched);
        for rate in [44_100, 96_000] {
            let fs = f64::from(rate);
            let stage =
                |c: &ChainConfig| Crossfeed::try_new(c.crossfeed.as_ref().unwrap(), fs, 2).unwrap();
            // Off vs peak-safe: B is quieter by the stage's loudness change on
            // the reference (-4.27 dB for bs2b default at 44.1 kHz), so B is
            // raised by that much.
            let expected = -stage(&safe).loudness_change_db(crate::crossfeed::MATCHED_CORRELATION);
            let offset = AbChain::analytic_offset_db(&off, &safe, rate, Weighting::A).unwrap();
            assert!(
                (offset - expected).abs() < 1e-6,
                "{rate}: {offset} vs {expected}"
            );
            assert!(offset > 4.0 && offset < 4.6, "{rate}: {offset}");
            // Off vs matched: same loudness by construction.
            let offset = AbChain::analytic_offset_db(&off, &matched, rate, Weighting::A).unwrap();
            assert!(offset.abs() < 1e-6, "{rate}: {offset}");
            // Matched vs peak-safe: the difference of the two levels.
            let offset = AbChain::analytic_offset_db(&matched, &safe, rate, Weighting::A).unwrap();
            let diff = stage(&matched).level_db() - stage(&safe).level_db();
            assert!((offset - diff).abs() < 1e-6, "{rate}: {offset} vs {diff}");
            // Symmetric, and a disabled stage counts as off.
            let back = AbChain::analytic_offset_db(&safe, &off, rate, Weighting::A).unwrap();
            assert!((back + expected).abs() < 1e-6);
            let mut disabled = safe.clone();
            disabled.crossfeed.as_mut().unwrap().enabled = false;
            assert_eq!(
                AbChain::analytic_offset_db(&off, &disabled, rate, Weighting::A).unwrap(),
                0.0
            );
        }
        let at_44 = AbChain::analytic_offset_db(&off, &safe, 44_100, Weighting::A).unwrap();
        assert!((at_44 - 4.27).abs() < 0.01, "{at_44}");
        let at_96 = AbChain::analytic_offset_db(&off, &safe, 96_000, Weighting::A).unwrap();
        assert!(
            (at_96 - at_44).abs() > 0.05,
            "the rate must change the design"
        );
    }

    #[test]
    fn analytic_offset_negates_flat_boost() {
        let boosted = ChainConfig {
            eq: Some(flat_profile(6.0)),
            ..ChainConfig::neutral()
        };
        let offset =
            AbChain::analytic_offset_db(&ChainConfig::neutral(), &boosted, 44_100, Weighting::A)
                .unwrap();
        assert!((offset + 6.0).abs() < 0.05, "offset {offset}");
        // The A/B builder uses the same sign convention.
        let boosted_config = ChainConfig {
            output: quiet_output(),
            ..boosted
        };
        let flat = ChainConfig {
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let ab = AbChain::new(
            spec(flat, 1, 8),
            spec(boosted_config, 1, 8),
            AbLevel::fixed(offset),
        )
        .unwrap();
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
            ..ChainConfig::neutral()
        };
        // Mean power of L=0 dB (disabled left ignored) and R=+6 dB, plus the
        // enabled convolution gains 2 + 1 = +3 dB: offset ≈ -6.96 dB.
        let expected_eq_pwr = 0.5 * (1.0 + db_to_lin(6.0).powi(2));
        let expected_db = -(crate::lin_to_db(expected_eq_pwr.sqrt()) + 3.0);
        let offset =
            AbChain::analytic_offset_db(&ChainConfig::neutral(), &split, 48_000, Weighting::A)
                .unwrap();
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
            ..ChainConfig::neutral()
        };
        // L = +6 dB, R = +3 dB: mean power ≈ +4.76 dB.
        let expected_layered_pwr = 0.5 * (db_to_lin(6.0).powi(2) + db_to_lin(3.0).powi(2));
        let expected_layered_db = -crate::lin_to_db(expected_layered_pwr.sqrt());
        let layered_offset =
            AbChain::analytic_offset_db(&ChainConfig::neutral(), &layered, 48_000, Weighting::A)
                .unwrap();
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
            ..ChainConfig::neutral()
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
        let mut chain = Chain::new(spec(ChainConfig::neutral(), 2, 1024));
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

    fn rational_spec(target: RateTarget, rate: u32, taps: usize, block: usize) -> ChainSpec {
        ChainSpec {
            config: ChainConfig {
                upsampler: crate::resample::ManualConfig {
                    target,
                    taps,
                    taps_unit: audivane_protocol::TapsUnit::Input,
                    cutoff: 0.95,
                    ..Default::default()
                }
                .into_upsampler_config()
                .unwrap(),
                output: quiet_output(),
                ..ChainConfig::neutral()
            },
            sample_rate: rate,
            channels: 2,
            max_block: block,
            irs: None,
        }
    }

    /// Feed `input` (interleaved stereo) through `chain` in varying blocks;
    /// return the f32 tap of everything it wrote.
    fn run_tapped(chain: &mut Chain, input: &[f32], blocks: &[usize]) -> Vec<f32> {
        let mut out = vec![0i32; chain.max_output_frames() * 2];
        let mut tap = vec![0f32; chain.max_output_frames() * 2];
        let mut all = Vec::new();
        let mut pos = 0;
        let mut i = 0;
        let mut unity = StereoGainRamp::unity(chain.output_rate());
        while pos < input.len() / 2 {
            let n = blocks[i % blocks.len()].min(input.len() / 2 - pos);
            i += 1;
            let n_out = chain.output_frames(n);
            assert!(n_out <= chain.max_output_frames());
            chain.process_f32_with_gain_tapped(
                &input[pos * 2..(pos + n) * 2],
                &mut out,
                &mut unity,
                Some(&mut tap),
            );
            all.extend_from_slice(&tap[..n_out * 2]);
            pos += n;
        }
        all
    }

    fn stereo_sine(freq: f64, rate: f64, frames: usize, amp: f64) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (amp * (2.0 * PI * freq * i as f64 / rate).sin()) as f32;
                [v, v]
            })
            .collect()
    }

    fn rms_after(x: &[f32], skip: usize) -> f64 {
        let mono: Vec<f64> = x.iter().step_by(2).map(|&v| f64::from(v)).collect();
        let tail = &mono[skip.min(mono.len())..];
        (tail.iter().map(|v| v * v).sum::<f64>() / tail.len().max(1) as f64).sqrt()
    }

    #[test]
    fn rational_upsampling_writes_the_grid_count_and_passes_the_band() {
        let mut chain = Chain::new(rational_spec(RateTarget::Rate(48_000), 44_100, 64, 512));
        assert_eq!(chain.ratio(), RateRatio::new(160, 147));
        assert_eq!(chain.output_rate(), 48_000);
        assert!(!chain.is_bit_transparent());
        // Linear phase: half the kernel (64 input frames) at the output rate.
        let lat = chain.latency_out_frames();
        assert!(
            (lat as f64 - 32.0 * 48_000.0 / 44_100.0).abs() <= 1.0,
            "{lat}"
        );

        let frames = 44_100 / 2;
        let x = stereo_sine(1_000.0, 44_100.0, frames, 0.5);
        let y = run_tapped(&mut chain, &x, &[512, 1, 300, 0, 77, 441]);
        assert_eq!(
            y.len() / 2,
            chain.ratio().frames_out_total(frames as u64) as usize
        );
        let rms = rms_after(&y, 2 * lat);
        assert!((rms - 0.5 / 2f64.sqrt()).abs() < 1e-3, "1 kHz rms {rms}");
    }

    #[test]
    fn decimation_removes_what_the_output_cannot_carry() {
        let mut chain = Chain::new(rational_spec(RateTarget::Rate(48_000), 96_000, 128, 256));
        assert_eq!(chain.ratio(), RateRatio::new(1, 2));
        let frames = 96_000 / 4;
        // 30 kHz is above the 24 kHz output Nyquist: it must not alias in.
        let x = stereo_sine(30_000.0, 96_000.0, frames, 0.5);
        let y = run_tapped(&mut chain, &x, &[256, 13, 100]);
        assert_eq!(y.len() / 2, frames / 2);
        let rms = rms_after(&y, 2 * chain.latency_out_frames());
        assert!(rms < 1e-5, "30 kHz leaked at {rms}");
        // 5 kHz passes at unity.
        chain.reset();
        let x = stereo_sine(5_000.0, 96_000.0, frames, 0.5);
        let y = run_tapped(&mut chain, &x, &[256]);
        let rms = rms_after(&y, 2 * chain.latency_out_frames());
        assert!((rms - 0.5 / 2f64.sqrt()).abs() < 1e-3, "5 kHz rms {rms}");
    }

    #[test]
    fn presets_run_at_fractional_and_decimating_ratios() {
        use audivane_protocol::FilterPreset;
        for (preset, rate, out) in [
            (FilterPreset::GaussianLong, 44_100, 48_000),
            (FilterPreset::Chebyshev, 44_100, 48_000),
            (FilterPreset::KaiserSteep, 96_000, 48_000),
            (FilterPreset::ChebyshevLong, 96_000, 44_100),
            (FilterPreset::GaussianShort, 96_000, 48_000),
            (FilterPreset::FlattopShort, 48_000, 44_100),
            (FilterPreset::Nuttall, 44_100, 48_000),
            (FilterPreset::ShortLeaky, 44_100, 96_000),
            (FilterPreset::Lanczos23, 44_100, 48_000),
        ] {
            let mut spec = rational_spec(RateTarget::Rate(out), rate, 64, 512);
            spec.config.upsampler.design = None;
            spec.config.upsampler.preset = Some(preset.name().to_string());
            let mut chain = Chain::new(spec);
            let frames = rate as usize / 2;
            let x = stereo_sine(1_000.0, f64::from(rate), frames, 0.5);
            let y = run_tapped(&mut chain, &x, &[512, 3, 200]);
            assert_eq!(
                y.len() / 2,
                chain.ratio().frames_out_total(frames as u64) as usize
            );
            let rms = rms_after(&y, 2 * chain.latency_out_frames());
            assert!(
                (rms - 0.5 / 2f64.sqrt()).abs() < 1e-4,
                "{preset:?} {rate} -> {out}: 1 kHz rms {rms}"
            );
        }
        // Integer-only and upsampling-only presets refuse with an error.
        let mut spec = rational_spec(RateTarget::Rate(48_000), 44_100, 64, 512);
        spec.config.upsampler.design = None;
        spec.config.upsampler.preset = Some(FilterPreset::GaussianHuge.name().to_string());
        assert!(matches!(
            Chain::try_new(spec),
            Err(ChainError::Upsampler(_))
        ));
        let mut spec = rational_spec(RateTarget::Rate(48_000), 96_000, 64, 512);
        spec.config.upsampler.design = None;
        spec.config.upsampler.preset = Some(FilterPreset::Lanczos511.name().to_string());
        assert!(matches!(
            Chain::try_new(spec),
            Err(ChainError::Upsampler(_))
        ));
        // A design over the cost budget errors instead of running.
        let mut spec = rational_spec(RateTarget::Rate(768_000), 44_100, 64, 512);
        spec.config.upsampler.design = None;
        spec.config.upsampler.preset = Some(FilterPreset::FlattopXlong.name().to_string());
        let err = Chain::try_new(spec).err().unwrap();
        assert!(err.to_string().contains("flattop_xlong"), "{err}");
    }

    #[test]
    fn reset_to_puts_a_new_chain_on_a_running_chain_grid() {
        let spec = || rational_spec(RateTarget::Rate(48_000), 44_100, 16, 256);
        let mut running = Chain::new(spec());
        let x = vec![0.0f32; 2 * 1000];
        run_tapped(&mut running, &x, &[123]);
        let mut fresh = Chain::new(spec());
        fresh.reset_to(running.input_pos());
        for n in [0usize, 1, 2, 17, 147, 256] {
            assert_eq!(fresh.output_frames(n), running.output_frames(n), "n {n}");
        }
    }

    #[test]
    fn every_stage_runs_at_a_rational_ratio_resets_and_adds_its_latency() {
        use audivane_protocol::{
            CrossfeedConfig, CrossfeedMode, HarmonicsCharacter, HarmonicsConfig, LimiterConfig,
            LimiterMode, PitchConfig,
        };
        let ratio = RateRatio::from_rates(44_100, 48_000);
        let bare = rational_spec(RateTarget::Rate(48_000), 44_100, 32, 256);
        let mut full = bare.clone();
        full.config.pitch = PitchConfig {
            enabled: true,
            cents: -31.77,
            ..Default::default()
        };
        full.config.crossfeed = Some(CrossfeedConfig {
            enabled: true,
            mode: CrossfeedMode::Bs2bDefault,
            ..Default::default()
        });
        full.config.harmonics = Some(HarmonicsConfig {
            enabled: true,
            character: HarmonicsCharacter::Triode,
            ..Default::default()
        });
        full.config.limiter = LimiterConfig {
            mode: LimiterMode::Limit,
            ..Default::default()
        };
        let mut chain = Chain::new(full.clone());
        assert_eq!(chain.ratio(), ratio);
        assert_eq!(chain.output_rate(), 48_000);
        assert!(chain.limiter().is_some());

        // Pitch and crossfeed run at the input rate: their delay scales by
        // 160/147, added before the one rounding (the limiter's look-ahead
        // is at the output rate).
        let pitch = PitchShifter::try_new(
            44_100,
            2,
            256,
            full.config.pitch.effective_cents(),
            full.config.pitch.quality,
        )
        .unwrap();
        let xf = Crossfeed::try_new(full.config.crossfeed.as_ref().unwrap(), 44_100.0, 2).unwrap();
        let input_rate = (Stage::latency_frames(&pitch) + Stage::latency_frames(&xf)) as f64;
        let added = chain.latency_out_frames() as f64
            - chain.limiter().map_or(0, Stage::latency_frames) as f64
            - Chain::new(bare).latency_out_frames() as f64;
        let want = input_rate * ratio.as_f64();
        assert!((added - want).abs() <= 1.0, "added {added}, want {want:.2}");

        // reset_to clears pitch, crossfeed, harmonics and the limiter: the
        // same input gives the same output again.
        let x = stereo_sine(997.0, 44_100.0, 6000, 0.7);
        let first = run_tapped(&mut chain, &x, &[256, 17, 147]);
        chain.reset_to(0);
        let again = run_tapped(&mut chain, &x, &[256, 17, 147]);
        assert_eq!(first, again);
        let fresh = run_tapped(&mut Chain::new(full), &x, &[256, 17, 147]);
        assert_eq!(first, fresh);
    }

    #[test]
    fn ab_runs_both_sides_on_one_rational_grid() {
        let a = rational_spec(RateTarget::Rate(48_000), 44_100, 32, 128);
        let mut b = rational_spec(RateTarget::Rate(48_000), 44_100, 96, 128);
        b.config.upsampler = crate::resample::ManualConfig {
            target: RateTarget::Rate(48_000),
            taps: 96,
            taps_unit: audivane_protocol::TapsUnit::Input,
            cutoff: 0.9,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let mut ab = AbChain::new(a, b, AbLevel::fixed(0.0)).unwrap();
        assert_eq!(ab.ratio(), RateRatio::new(160, 147));
        let x = stereo_sine(1_000.0, 44_100.0, 4410, 0.25);
        let mut out = vec![0i32; ab.max_output_frames() * 2];
        let mut total = 0;
        for (i, chunk) in x.chunks(2 * 100).enumerate() {
            if i == 10 {
                ab.select(Side::B, 64);
            }
            let n_out = ab.output_frames(chunk.len() / 2);
            ab.process_f32(chunk, &mut out);
            total += n_out;
        }
        assert_eq!(total as u64, ab.ratio().frames_out_total(4410));

        let mut c = rational_spec(RateTarget::Rate(48_000), 44_100, 32, 128);
        c.config.upsampler.target = RateTarget::Ratio(2);
        let d = rational_spec(RateTarget::Rate(48_000), 44_100, 32, 128);
        assert!(matches!(
            AbChain::new(c, d, AbLevel::fixed(0.0)),
            Err(ChainError::RatioMismatch { .. })
        ));
    }

    #[test]
    fn a_rational_kernel_over_budget_is_an_error() {
        let spec = rational_spec(RateTarget::Rate(384_000), 44_100, 65_536, 256);
        let err = Chain::try_new(spec).err().expect("over budget");
        assert!(matches!(err, ChainError::Upsampler(_)), "{err}");
    }

    #[test]
    fn pitch_stage_bypass_and_latency() {
        let off_cfg = ChainConfig {
            pitch: audivane_protocol::PitchConfig {
                cents: 0.0,
                ..Default::default()
            },
            ..ChainConfig::neutral()
        };
        assert!(off_cfg.is_passthrough());
        let off_chain = Chain::new(ChainSpec {
            config: off_cfg,
            sample_rate: 44_100,
            channels: 2,
            max_block: 512,
            irs: None,
        });
        assert_eq!(off_chain.latency_out_frames(), 0);

        let mut on_cfg = ChainConfig {
            pitch: audivane_protocol::PitchConfig {
                enabled: true,
                cents: -31.77,
                ..Default::default()
            },
            ..ChainConfig::neutral()
        };
        assert!(!on_cfg.is_passthrough());
        let on_chain = Chain::new(ChainSpec {
            config: on_cfg.clone(),
            sample_rate: 44_100,
            channels: 2,
            max_block: 512,
            irs: None,
        });
        let base_lat = on_chain.latency_out_frames();
        assert!(base_lat > 0);

        on_cfg.upsampler = crate::resample::ManualConfig {
            target: RateTarget::Ratio(2),
            taps: 64,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap();
        let up_chain = Chain::new(ChainSpec {
            config: on_cfg,
            sample_rate: 44_100,
            channels: 2,
            max_block: 512,
            irs: None,
        });
        assert!(up_chain.latency_out_frames() >= base_lat * 2);
    }

    #[test]
    fn crossfeed_in_chain_requires_stereo_and_breaks_passthrough() {
        let cfg = ChainConfig {
            crossfeed: Some(audivane_protocol::CrossfeedConfig {
                enabled: true,
                mode: audivane_protocol::CrossfeedMode::Bs2bDefault,
                ..Default::default()
            }),
            ..ChainConfig::neutral()
        };
        assert!(!cfg.is_passthrough());

        // Non-stereo fails with a clear Crossfeed error.
        let err = Chain::try_new(spec(cfg.clone(), 1, 256))
            .err()
            .expect("mono chain with crossfeed should fail");
        assert_eq!(
            err,
            ChainError::Crossfeed(CrossfeedError::RequiresStereo { channels: 1 })
        );

        let mut chain = Chain::try_new(spec(cfg, 2, 256)).unwrap();
        assert!(!chain.is_bit_transparent());
        assert_eq!(chain.latency_out_frames(), 0);

        // Hard-panned left signal feeds into right channel.
        let frames = 256;
        let mut input = vec![0.0f32; frames * 2];
        for i in 0..frames {
            input[i * 2] = 0.5;
        }
        let mut out = vec![0i32; frames * 2];
        chain.process_f32(&input, &mut out);
        let right_energy: f64 = (0..frames)
            .map(|i| (out[i * 2 + 1] as f64 / 2_147_483_648.0).powi(2))
            .sum();
        assert!(
            right_energy > 0.1,
            "expected crossfeed into R, got {right_energy}"
        );
    }

    #[test]
    fn crossfeed_in_chain_does_not_clip_antiphase_steps() {
        // Full-scale L=-1/R=+1, then flipped: the unscaled crossfeed peaks at
        // ~1.68 here; the peak-safe level must keep it below full scale.
        let cfg = ChainConfig {
            crossfeed: Some(audivane_protocol::CrossfeedConfig {
                level: audivane_protocol::CrossfeedLevel::PeakSafe,
                ..Default::default()
            }),
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        let mut chain = Chain::try_new(spec(cfg, 2, 256)).unwrap();
        let mut input = vec![0.0f32; 512 * 2];
        for i in 0..512 {
            let v = if i < 256 { -1.0 } else { 1.0 };
            input[i * 2] = v;
            input[i * 2 + 1] = -v;
        }
        let mut out = vec![0i32; 256 * 2];
        let mut clipped = 0;
        let mut peak = 0.0_f64;
        for block in input.chunks(256 * 2) {
            let stats = chain.process_f32(block, &mut out);
            clipped += stats.clipped;
            peak = peak.max(stats.peak);
        }
        assert_eq!(clipped, 0, "peak {peak}");
        assert!(peak <= 1.0 && peak > 0.9, "peak {peak}");
    }

    #[test]
    fn matched_crossfeed_overs_are_left_to_the_limiter() {
        // The same worst case at the default matched level goes over full
        // scale and clips at the quantizer without the limiter (nothing else
        // in the chain catches it); with the limiter on, nothing clips.
        let run = |limiter: audivane_protocol::LimiterMode| {
            let cfg = ChainConfig {
                crossfeed: Some(audivane_protocol::CrossfeedConfig::default()),
                limiter: audivane_protocol::LimiterConfig {
                    mode: limiter,
                    ..Default::default()
                },
                output: quiet_output(),
                ..ChainConfig::neutral()
            };
            let mut chain = Chain::try_new(spec(cfg, 2, 256)).unwrap();
            let mut input = vec![0.0f32; 4096 * 2];
            for i in 0..4096 {
                let v = if (i / 256) % 2 == 0 { -1.0 } else { 1.0 };
                input[i * 2] = v;
                input[i * 2 + 1] = -v;
            }
            let mut out = vec![0i32; 256 * 2];
            let mut clipped = 0;
            for block in input.chunks(256 * 2) {
                clipped += chain.process_f32(block, &mut out).clipped;
            }
            clipped
        };
        assert!(run(audivane_protocol::LimiterMode::Off) > 0);
        assert_eq!(run(audivane_protocol::LimiterMode::Limit), 0);
    }

    #[test]
    fn harmonics_in_chain_runs_after_upsampler_and_breaks_passthrough() {
        let cfg = ChainConfig {
            harmonics: Some(audivane_protocol::HarmonicsConfig {
                enabled: true,
                character: audivane_protocol::HarmonicsCharacter::Triode,
                ..Default::default()
            }),
            ..ChainConfig::neutral()
        };
        assert!(!cfg.is_passthrough());

        let chain = Chain::try_new(spec(cfg, 2, 256)).unwrap();
        assert!(!chain.is_bit_transparent());
        assert_eq!(chain.latency_out_frames(), 0);
    }

    #[test]
    fn dc_block_in_chain_removes_dc_and_breaks_passthrough() {
        let cfg = ChainConfig {
            dc_block: Some(audivane_protocol::DcBlockConfig::default()),
            output: quiet_output(),
            ..ChainConfig::neutral()
        };
        assert!(!cfg.is_passthrough());

        let mut chain = Chain::try_new(spec(cfg, 2, 256)).unwrap();
        assert!(!chain.is_bit_transparent());
        assert_eq!(chain.latency_out_frames(), 0);

        // Push 1.0 s of +0.5 DC offset at 48 kHz; final block should decay well below -60 dB.
        let input = vec![0.5f32; 256 * 2];
        let mut out = vec![0i32; 256 * 2];
        for _ in 0..(48_000 / 256) {
            chain.process_f32(&input, &mut out);
        }
        let peak = out
            .iter()
            .map(|&s| (s as f64 / 2_147_483_648.0).abs())
            .fold(0.0, f64::max);
        assert!(peak < 1e-4, "DC not removed: tail peak {peak:e}");
    }
}
