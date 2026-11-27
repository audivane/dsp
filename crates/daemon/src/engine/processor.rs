//! Real-time audio processor (`DaemonProcessor`) implementing
//! `audivane_io_pw::EngineAudioProcessor`.
//!
//! Runs strictly allocation-free and lock-free on the ALSA RT thread:
//! - Lock-free `rtrb` command queue receives rebuilt `Chain` / `AbChain` objects,
//!   A/B select switches, and injected self-test jobs.
//! - Old `Chain` / `AbChain` allocations are returned via a second `rtrb` queue
//!   to be dropped off the RT thread.
//! - Chain swaps on EQ/A-B changes crossfade smoothly over ~10 ms using a
//!   pre-allocated scratch buffer (no gaps or clicks).
//! - Per-channel peak and RMS meters are accumulated into lock-free atomics.

use crate::engine::harmonics_live::HarmonicsLiveRt;
use crate::engine::measure::{BlockCtx, MasterMeasure, MeasureSlot};
use crate::engine::spectrum::SpectrumRt;
use crate::engine::tee::{StageView, TeeRt};
use crate::engine::varispeed::{StopKind, VarispeedApplied, VarispeedRt, VarispeedRtCmd};
use audivane_dsp_core::ab_measure::AbHop;
use audivane_dsp_core::bypass::{f32_to_i32, hash_i32};
use audivane_dsp_core::chain::{AbChain, Chain, Side};
use audivane_dsp_core::dither::{ClipStats, TpdfDither};
use audivane_dsp_core::drift::FRAME;
use audivane_dsp_core::gain::{MuteRamp, StereoGainRamp};
use audivane_dsp_core::limiter::Limiter;
use audivane_dsp_core::varispeed::Varispeed;
use audivane_io_pw::EngineAudioProcessor;
use audivane_io_pw::varispeed_rt::VarispeedPort;
use audivane_protocol::ipc::{AbSide, Meters, SinkChoice};
use audivane_protocol::{RateRatio, SampleFormat};
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

pub const MAX_METER_CHANNELS: usize = 8;
const MAX_METER_CHANNELS_OR_DEFAULT: usize = MAX_METER_CHANNELS;
const SILENCE_FLOOR_DB: f64 = -160.0;
const SWAP_CROSSFADE_MS: usize = 10;
const SWAP_FADE_HALF_MS: usize = 5;
const WARMUP_SETTLE_MARGIN_FRAMES: usize = 1024;
const RECYCLE_STASH_CAP: usize = 4;
/// The most of a new convolution's ring a swap warms up for, ms: past it
/// a room or reverb tail is far down, and the edit has waited long enough.
pub(crate) const WARMUP_RING_CAP_MS: u64 = 1_000;

/// The share of a period a swap's state handover may take on the audio
/// thread ([`ActiveChain::inherit_filter_state`]: a copy before that
/// callback's DSP, both chains then run), at [`HANDOVER_BYTES_PER_S`].
const HANDOVER_SHARE: f64 = 0.25;
/// A conservative copy rate for large, cold buffers, bytes/s (a fast machine
/// copies 7 to 8 GB/s; a laptop under load, less).
const HANDOVER_BYTES_PER_S: f64 = 2e9;

/// The most convolution state a swap copies in one callback of `frames`
/// at `rate` ([`HANDOVER_SHARE`] of it): a larger state is not handed
/// over, its chain warms up for its ring instead. 0 without a period.
pub(crate) fn handover_budget(frames: usize, rate: u32) -> usize {
    if rate == 0 {
        return 0;
    }
    (frames as f64 / f64::from(rate) * HANDOVER_SHARE * HANDOVER_BYTES_PER_S) as usize
}

/// [`WARMUP_RING_CAP_MS`] in output frames at `rate` x `ratio`.
pub(crate) fn warm_ring_cap(rate: u32, ratio: RateRatio) -> usize {
    (u64::from(rate) * WARMUP_RING_CAP_MS / 1_000) as usize * ratio.up as usize
        / (ratio.down as usize).max(1)
}

/// Target volume/balance/mute parameters published lock-free from control/engine
/// threads to the RT processor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolumeTarget {
    pub volume_db: f64,
    pub balance_db: f64,
    pub muted: bool,
    pub hardware_volume: bool,
    pub hardware_mute: bool,
}

impl Default for VolumeTarget {
    fn default() -> Self {
        Self {
            volume_db: 0.0,
            balance_db: 0.0,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        }
    }
}

/// Wait-free single-writer, RT-safe seqlock for [`VolumeTarget`].
///
/// The RT reader never allocates, blocks, or spins unboundedly: if a concurrent
/// write is observed in progress, it skips reading and picks up the update on
/// the next cycle.
#[derive(Debug)]
pub struct SharedVolume {
    seq: AtomicU64,
    volume_bits: AtomicU64,
    balance_bits: AtomicU64,
    flags: AtomicU8,
}

impl Default for SharedVolume {
    fn default() -> Self {
        Self::new(VolumeTarget::default())
    }
}

impl SharedVolume {
    const FLAG_MUTED: u8 = 1 << 0;
    const FLAG_HW_VOL: u8 = 1 << 1;
    const FLAG_HW_MUTE: u8 = 1 << 2;

    pub fn new(initial: VolumeTarget) -> Self {
        let flags = Self::encode_flags(initial);
        Self {
            // Start at even generation 2 so a fresh RT processor with volume_gen = 0
            // always synchronizes once on startup.
            seq: AtomicU64::new(2),
            volume_bits: AtomicU64::new(initial.volume_db.to_bits()),
            balance_bits: AtomicU64::new(initial.balance_db.to_bits()),
            flags: AtomicU8::new(flags),
        }
    }

    #[inline]
    fn encode_flags(t: VolumeTarget) -> u8 {
        u8::from(t.muted) | (u8::from(t.hardware_volume) << 1) | (u8::from(t.hardware_mute) << 2)
    }

    #[inline]
    fn decode_flags(flags: u8) -> (bool, bool, bool) {
        (
            (flags & Self::FLAG_MUTED) != 0,
            (flags & Self::FLAG_HW_VOL) != 0,
            (flags & Self::FLAG_HW_MUTE) != 0,
        )
    }

    /// Publish a new [`VolumeTarget`] from a non-RT serialized writer (e.g. under
    /// `BuilderState` lock).
    pub fn store(&self, target: VolumeTarget) {
        let g = self.seq.load(Ordering::Relaxed);
        let odd = g.wrapping_add(1) | 1;
        self.seq.store(odd, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        self.volume_bits
            .store(target.volume_db.to_bits(), Ordering::Relaxed);
        self.balance_bits
            .store(target.balance_db.to_bits(), Ordering::Relaxed);
        self.flags
            .store(Self::encode_flags(target), Ordering::Relaxed);
        self.seq.store(odd.wrapping_add(1), Ordering::Release);
    }

    /// Read a newly published [`VolumeTarget`] without blocking on the RT thread.
    #[inline]
    pub fn read_if_changed(&self, last_gen: &mut u64) -> Option<VolumeTarget> {
        for _ in 0..2 {
            let g1 = self.seq.load(Ordering::Acquire);
            if g1 == *last_gen {
                return None;
            }
            if (g1 & 1) == 1 {
                std::hint::spin_loop();
                continue;
            }
            let volume_db = f64::from_bits(self.volume_bits.load(Ordering::Relaxed));
            let balance_db = f64::from_bits(self.balance_bits.load(Ordering::Relaxed));
            let flags = self.flags.load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            let g2 = self.seq.load(Ordering::Acquire);
            if g1 == g2 {
                *last_gen = g1;
                let (muted, hardware_volume, hardware_mute) = Self::decode_flags(flags);
                return Some(VolumeTarget {
                    volume_db,
                    balance_db,
                    muted,
                    hardware_volume,
                    hardware_mute,
                });
            }
        }
        None
    }
}

/// Active DSP chain owned by the RT processor. Its ratio and input rate
/// are its identity: a chain only plays (and a resume only completes) at
/// the ratio and graph rate it was built for.
pub enum ActiveChain {
    Bypass,
    Single(Box<Chain>),
    Ab(Box<AbChain>),
    /// The engine bypass resampler's chain for Direct and DSP off at a
    /// ratio other than x1 (the upsampler alone, digital volume, dither):
    /// it only plays in [`PlayMode::ResampleOnly`], and the DSP chains only
    /// outside it.
    Resample(Box<Chain>),
}

impl ActiveChain {
    /// The engine bypass resampler's chain ([`Self::Resample`]).
    pub fn is_resample(&self) -> bool {
        matches!(self, Self::Resample(_))
    }

    /// The input (graph) rate the chain was built for; `None` for bypass,
    /// which runs at any.
    pub fn input_rate(&self) -> Option<u32> {
        match self {
            Self::Bypass => None,
            Self::Single(c) | Self::Resample(c) => Some(c.sample_rate()),
            Self::Ab(ab) => Some(ab.sample_rate()),
        }
    }

    pub fn ratio(&self) -> RateRatio {
        match self {
            Self::Bypass => RateRatio::ONE,
            Self::Single(c) | Self::Resample(c) => c.ratio(),
            Self::Ab(ab) => ab.ratio(),
        }
    }

    /// Output frames the next call with `frames` input frames writes.
    pub fn output_frames(&self, frames: usize) -> usize {
        match self {
            Self::Bypass => frames,
            Self::Single(c) | Self::Resample(c) => c.output_frames(frames),
            Self::Ab(ab) => ab.output_frames(frames),
        }
    }

    /// Where the chain is on the rational frame grid (input frames modulo
    /// the ratio's `down`).
    pub fn input_pos(&self) -> u64 {
        match self {
            Self::Bypass => 0,
            Self::Single(c) | Self::Resample(c) => c.input_pos(),
            Self::Ab(ab) => ab.input_pos(),
        }
    }

    /// Put a chain that is not on the frame grid of a stream at input
    /// position `pos` onto it (clearing its state). A no-op for integer
    /// ratios and for a chain already there, so a running chain is never
    /// disturbed.
    /// Returns whether it cleared the state (the chain was off the grid).
    pub fn align_to(&mut self, pos: u64) -> bool {
        let ratio = self.ratio();
        if ratio.is_integer() || self.input_pos() == pos % u64::from(ratio.down) {
            return false;
        }
        match self {
            Self::Bypass => {}
            Self::Single(c) | Self::Resample(c) => c.reset_to(pos),
            Self::Ab(ab) => ab.reset_to(pos),
        }
        true
    }

    /// The warm-up a chain that starts from cleared state needs, output
    /// frames: its latency, a convolution's ring (capped) and the margin
    /// `margin` (0 for a chain with neither latency nor convolution).
    pub(crate) fn cold_warm_frames(&self, ring_cap: usize, margin: usize) -> usize {
        let lat = self.latency_out_frames();
        let ring = self.conv_ring_frames().min(ring_cap);
        if lat == 0 && ring == 0 {
            0
        } else {
            lat + ring + margin
        }
    }

    pub fn latency_out_frames(&self) -> usize {
        match self {
            Self::Bypass => 0,
            Self::Single(c) | Self::Resample(c) => c.latency_out_frames(),
            Self::Ab(ab) => ab.latency_out_frames(),
        }
    }

    /// The convolution's own delay, graph-rate frames
    /// (the chain's; the playing side's in A/B).
    pub fn conv_latency_frames(&self) -> usize {
        match self {
            Self::Bypass => 0,
            Self::Single(c) | Self::Resample(c) => c.conv_latency_frames(),
            Self::Ab(ab) => ab.conv_latency_frames(),
        }
    }

    /// Output frames the chain plays after its last input frame: its
    /// latency plus a convolution IR's ring ([`Chain::tail_out_frames`]).
    pub fn tail_out_frames(&self) -> usize {
        match self {
            Self::Bypass => 0,
            Self::Single(c) | Self::Resample(c) => c.tail_out_frames(),
            Self::Ab(ab) => ab.tail_out_frames(),
        }
    }

    fn limiter(&self) -> Option<&Limiter> {
        match self {
            Self::Bypass => None,
            Self::Single(c) | Self::Resample(c) => c.limiter(),
            Self::Ab(ab) => ab.limiter(),
        }
    }

    #[cfg(test)]
    fn limiter_headroom_db(&self) -> f64 {
        self.limiter().map_or(0.0, Limiter::headroom_db)
    }

    /// Carry the held automatic headroom over from the chain being replaced
    /// when the limiter config is unchanged, so an EQ edit does not reset it.
    pub(crate) fn inherit_limiter_headroom(&mut self, from: &ActiveChain) {
        let Some(source) = from.limiter() else {
            return;
        };
        if let Some(target) = self.limiter_mut() {
            target.inherit_headroom(source);
        }
    }

    /// Carry DC-blocker filter history over from `from` when cutoff, sample
    /// rate, and channel count match (single chain or matching A/B sides), so
    /// an EQ-only edit does not restart the filter from zero.
    pub(crate) fn inherit_dc_block_state(&mut self, from: &ActiveChain) {
        match (self, from) {
            (Self::Single(dst), Self::Single(src)) => {
                if let (Some(d), Some(s)) = (dst.dc_block_mut(), src.dc_block()) {
                    d.inherit_state(s);
                }
            }
            (Self::Ab(dst), Self::Ab(src)) => {
                if let (Some(d), Some(s)) = (dst.dc_block_a_mut(), src.dc_block_a()) {
                    d.inherit_state(s);
                }
                if let (Some(d), Some(s)) = (dst.dc_block_b_mut(), src.dc_block_b()) {
                    d.inherit_state(s);
                }
            }
            (Self::Ab(dst), Self::Single(src)) => {
                if let Some(s) = src.dc_block() {
                    if let Some(d) = dst.dc_block_a_mut() {
                        d.inherit_state(s);
                    }
                    if let Some(d) = dst.dc_block_b_mut() {
                        d.inherit_state(s);
                    }
                }
            }
            (Self::Single(dst), Self::Ab(src)) => {
                // Disabling A/B builds the single chain from chain A, so always
                // carry outgoing side A's state regardless of which side was audible.
                if let (Some(d), Some(s)) = (dst.dc_block_mut(), src.dc_block_a()) {
                    d.inherit_state(s);
                }
            }
            _ => {}
        }
    }

    /// Carry the playing chain's filter state over to this one, which
    /// warms up next to it for a swap ([`Chain::inherit_filter_state`]): an
    /// edit that keeps the convolution and its input (nothing before it
    /// changed) plays the convolver on rather than starting it from
    /// silence. Returns whether the convolution was handed over (true
    /// without one); one that was not needs [`Self::conv_ring_frames`] of
    /// warm-up to play its whole response. Real-time safe.
    ///
    /// Call it once the chain is on the stream's frame grid
    /// ([`Self::align_to`] clears state): an align that clears it later
    /// undoes the handover. `max_copy_bytes` bounds the convolution state
    /// copied ([`handover_budget`]); a larger one is not handed over.
    pub(crate) fn inherit_filter_state(
        &mut self,
        from: &mut ActiveChain,
        max_copy_bytes: usize,
    ) -> bool {
        match (self, from) {
            (Self::Single(dst), Self::Single(src)) => dst.inherit_filter_state(src, max_copy_bytes),
            (Self::Ab(dst), Self::Ab(src)) => dst.inherit_filter_state(src, max_copy_bytes),
            (Self::Ab(dst), Self::Single(src)) => {
                dst.inherit_filter_state_from_single(src, max_copy_bytes)
            }
            (Self::Single(dst), Self::Ab(src)) => {
                dst.inherit_filter_state_from_ab(src, max_copy_bytes)
            }
            (dst, _) => dst.conv_ring_frames() == 0,
        }
    }

    /// Take the playing chain over in place for an edit before an
    /// unchanged convolution ([`Chain::take_over_upstream`]: its
    /// convolution and later stages move here, the edit fades in at the
    /// convolution's input): this chain then replaces it at once. Single
    /// chains only (an A/B pair warms up). Real-time safe.
    pub(crate) fn take_over_upstream(&mut self, from: &mut ActiveChain) -> bool {
        match (self, from) {
            (Self::Single(dst), Self::Single(src)) => dst.take_over_upstream(src),
            _ => false,
        }
    }

    /// An upstream edit's fade is running in this chain
    /// ([`Chain::upstream_fading`]): the next swap waits for its end.
    /// How long the fresh stages before a convolution take to settle,
    /// output frames, capped at the takeover's longest hold
    /// ([`Chain::settle_out_frames`]; 0 without a convolution, or an A/B
    /// pair).
    pub(crate) fn settle_out_frames(&self, out_rate: u32) -> usize {
        let cap = f64::from(out_rate) * audivane_dsp_core::chain::UPSTREAM_SETTLE_MAX_MS / 1000.0;
        match self {
            Self::Single(c) | Self::Resample(c) => {
                let s = c.settle_out_frames();
                if s.is_finite() {
                    s.min(cap) as usize
                } else {
                    cap as usize
                }
            }
            _ => 0,
        }
    }

    pub(crate) fn upstream_fading(&self) -> bool {
        match self {
            Self::Single(c) | Self::Resample(c) => c.upstream_fading(),
            _ => false,
        }
    }

    /// Output frames a convolution rings past the chain's latency
    /// ([`Chain::conv_ring_frames`]; 0 without one).
    pub(crate) fn conv_ring_frames(&self) -> usize {
        match self {
            Self::Bypass => 0,
            Self::Single(c) | Self::Resample(c) => c.conv_ring_frames(),
            Self::Ab(ab) => ab.conv_ring_frames(),
        }
    }

    fn limiter_mut(&mut self) -> Option<&mut Limiter> {
        match self {
            Self::Bypass => None,
            Self::Single(c) | Self::Resample(c) => c.limiter_mut(),
            Self::Ab(ab) => ab.limiter_mut(),
        }
    }

    /// A chain warming up in parallel hears audio that is not played (and
    /// starts mid-signal, so its detector sees a start transient): it must not
    /// raise held headroom or count events until it becomes the audible one.
    pub(crate) fn set_limiter_audible(&mut self, audible: bool) {
        if let Some(limiter) = self.limiter_mut() {
            limiter.set_learning(audible);
        }
    }

    /// The word the chain quantizes to, bits (32: none, the bypass).
    pub(crate) fn output_bits(&self) -> u32 {
        match self {
            Self::Bypass => 32,
            Self::Single(c) | Self::Resample(c) => c.output_bits(),
            Self::Ab(ab) => ab.output_bits(),
        }
    }

    /// Blocks of this chain a late channel worker made silent (its own:
    /// a chain warming up beside it does not count; 0 without workers).
    pub(crate) fn worker_late_blocks(&self) -> u64 {
        match self {
            Self::Bypass => 0,
            Self::Single(c) | Self::Resample(c) => c.worker_late_blocks(),
            Self::Ab(ab) => ab.worker_late_blocks(),
        }
    }

    pub fn max_block(&self) -> usize {
        match self {
            Self::Bypass => usize::MAX,
            Self::Single(c) | Self::Resample(c) => c.max_block(),
            Self::Ab(_ab) => usize::MAX,
        }
    }

    pub fn ab_select(&mut self, side: Side, crossfade_frames: usize) {
        if let Self::Ab(ab) = self {
            ab.select(side, crossfade_frames);
        }
    }

    /// A new offset for B on an A/B pair (a 50 ms ramp); nothing for any
    /// other chain. Allocation-free.
    pub fn ab_set_level_offset(&mut self, offset_db: f64) {
        if let Self::Ab(ab) = self {
            ab.set_level_offset_db(offset_db);
        }
    }

    /// Meter an A/B pair for the measurement `epoch` (0 stops); nothing
    /// for any other chain.
    fn set_ab_measure_epoch(&mut self, epoch: u32) {
        if let Self::Ab(ab) = self {
            ab.set_measure_epoch(epoch);
        }
    }

    /// Hand an A/B pair's completed measurement hops to `meters`.
    fn drain_ab_hops(&mut self, meters: &MeterBank) {
        if let Self::Ab(ab) = self {
            while let Some(hop) = ab.pop_measure_hop() {
                meters.ab_hops.push(hop);
            }
        }
    }

    /// Apply an A/B side without a crossfade.
    ///
    /// The selection is engine state, not chain-build metadata: this is used
    /// off-RT right before a chain is published and on RT when a queued chain is
    /// about to be installed, so a later selection is never lost.
    pub fn apply_ab_side(&mut self, side: AbSide) {
        if let Self::Ab(ab) = self {
            let side = match side {
                AbSide::A => Side::A,
                AbSide::B => Side::B,
            };
            ab.select(side, 0);
        }
    }

    pub fn reset(&mut self) {
        match self {
            Self::Bypass => {}
            Self::Single(c) | Self::Resample(c) => c.reset(),
            Self::Ab(ab) => ab.reset(),
        }
    }

    /// Process interleaved `f32` to quantized `i32`, optionally copying the
    /// floating-point chain output (before the quantizer/dither) into `tap`.
    #[cfg(test)]
    pub(crate) fn process_f32_with_gain(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        channels: usize,
        max_chunk: usize,
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
    ) -> ClipStats {
        self.process_in_with_gain(DspIn::F32(input), out, channels, max_chunk, gain, tap)
    }

    /// [`Self::process_f32_with_gain`] for either block form: the source's
    /// interleaved `f32`, or the varispeed stage's planar `f64` output (the
    /// same block for every chain).
    pub(crate) fn process_in_with_gain(
        &mut self,
        input: DspIn<'_>,
        out: &mut [i32],
        channels: usize,
        max_chunk: usize,
        gain: &mut StereoGainRamp,
        tap: Option<&mut [f32]>,
    ) -> ClipStats {
        self.process_in_with_gain_live(input, out, channels, max_chunk, gain, tap, None)
    }

    /// Copy the harmonics stage's input and output (the side playing) from the next
    /// block on ([`Chain::set_harmonics_tap`]); a flag, real-time safe.
    pub(crate) fn set_harmonics_tap(&mut self, on: bool) {
        match self {
            Self::Single(c) => c.set_harmonics_tap(on),
            Self::Ab(ab) => ab.set_harmonics_tap(on),
            Self::Bypass | Self::Resample(_) => {}
        }
    }

    /// [`Self::process_in_with_gain`], each chain call's harmonics tap
    /// queued to `live` at its output rate.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process_in_with_gain_live(
        &mut self,
        input: DspIn<'_>,
        out: &mut [i32],
        channels: usize,
        max_chunk: usize,
        gain: &mut StereoGainRamp,
        mut tap: Option<&mut [f32]>,
        mut live: Option<(&mut HarmonicsLiveRt, u32)>,
    ) -> ClipStats {
        let in_frames = input.frames(channels);
        if let Some((l, rate)) = live.as_mut()
            && matches!(self, Self::Bypass | Self::Resample(_))
        {
            l.push(None, false, *rate);
        }
        match self {
            Self::Bypass => {
                match input {
                    DspIn::F32(input) => {
                        if let Some(tap) = tap.as_deref_mut() {
                            let n = input.len().min(tap.len());
                            tap[..n].copy_from_slice(&input[..n]);
                        }
                        gain.process_interleaved_f32_to_i32(input, out, channels);
                    }
                    // The stage never feeds a bypass chain that plays
                    // alone (`DaemonProcessor::varispeed`), but a swap's
                    // outgoing one hears its block while the incoming chain
                    // takes over: it plays it as the source, scaled the same.
                    DspIn::Planar { chans, frames, .. } => {
                        for f in 0..frames {
                            let g = gain.next_frame();
                            for (c, chan) in chans.iter().enumerate().take(channels) {
                                let x = chan.get(f).copied().unwrap_or(0.0);
                                let i = f * channels + c;
                                if let Some(t) = tap.as_deref_mut().and_then(|t| t.get_mut(i)) {
                                    *t = x as f32;
                                }
                                if let Some(o) = out.get_mut(i) {
                                    *o = (x * g[c.min(2)] * 2_147_483_648.0).round() as i32;
                                }
                            }
                        }
                    }
                }
                ClipStats::default()
            }
            Self::Single(c) | Self::Resample(c) => {
                let limit = c.max_block().min(max_chunk).max(1);
                let mut total = ClipStats::default();
                let mut done = 0usize;
                let mut out_at = 0usize;
                while done < in_frames || (in_frames == 0 && done == 0) {
                    let take = (in_frames - done).min(limit);
                    let n_out = c.output_frames(take) * channels;
                    let out_s = &mut out[out_at..out_at + n_out];
                    let tap_s = tap.as_deref_mut().map(|t| &mut t[out_at..]);
                    let stats = match input.slice(channels, done, take) {
                        DspIn::F32(in_s) => {
                            c.process_f32_with_gain_tapped(in_s, out_s, gain, tap_s)
                        }
                        DspIn::Planar {
                            chans, transparent, ..
                        } => c.process_planar_with_gain_tapped(
                            &chans[..channels],
                            out_s,
                            gain,
                            tap_s,
                            transparent,
                        ),
                    };
                    if let Some((l, rate)) = live.as_mut() {
                        l.push(c.harmonics_tap(), c.has_harmonics(), *rate);
                    }
                    if in_frames <= limit {
                        return stats;
                    }
                    total.merge(stats);
                    done += take;
                    out_at += n_out;
                }
                total
            }
            Self::Ab(ab) => {
                let limit = max_chunk.max(1);
                let mut total = ClipStats::default();
                let mut done = 0usize;
                let mut out_at = 0usize;
                while done < in_frames || (in_frames == 0 && done == 0) {
                    let take = (in_frames - done).min(limit);
                    let n_out = ab.output_frames(take) * channels;
                    let out_s = &mut out[out_at..out_at + n_out];
                    let tap_s = tap.as_deref_mut().map(|t| &mut t[out_at..]);
                    let stats = match input.slice(channels, done, take) {
                        DspIn::F32(in_s) => {
                            ab.process_f32_with_gain_tapped(in_s, out_s, gain, tap_s)
                        }
                        DspIn::Planar {
                            chans, transparent, ..
                        } => ab.process_planar_with_gain_tapped(
                            &chans[..channels],
                            out_s,
                            gain,
                            tap_s,
                            transparent,
                        ),
                    };
                    if let Some((l, rate)) = live.as_mut() {
                        l.push(ab.harmonics_tap(), ab.has_harmonics(), *rate);
                    }
                    if in_frames <= limit {
                        return stats;
                    }
                    total.merge(stats);
                    done += take;
                    out_at += n_out;
                }
                total
            }
        }
    }
}

/// A block for the chains: the source's interleaved `f32` (the stage off:
/// each chain's first step converts it exactly, so this is the same block
/// for all of them), or the varispeed stage's planar `f64` output, handed
/// to every chain as it is (no rounding back to `f32`).
#[derive(Clone, Copy)]
pub(crate) enum DspIn<'a> {
    F32(&'a [f32]),
    Planar {
        chans: [&'a [f64]; MAX_METER_CHANNELS],
        frames: usize,
        /// The samples are the source's exactly (never, after the stage).
        transparent: bool,
    },
}

impl<'a> DspIn<'a> {
    /// As [`Self::planar`], `transparent` when the samples are `f32` values
    /// a client could have sent (a measurement's file).
    pub(crate) fn planar_with(bufs: &'a [Vec<f64>], frames: usize, transparent: bool) -> Self {
        match Self::planar(bufs, frames) {
            Self::Planar { chans, frames, .. } => Self::Planar {
                chans,
                frames,
                transparent,
            },
            other => other,
        }
    }

    /// The first `frames` frames of `bufs` (at most [`MAX_METER_CHANNELS`]).
    pub(crate) fn planar(bufs: &'a [Vec<f64>], frames: usize) -> Self {
        let mut chans: [&[f64]; MAX_METER_CHANNELS] = [&[]; MAX_METER_CHANNELS];
        for (c, b) in chans.iter_mut().zip(bufs.iter()) {
            *c = &b[..frames];
        }
        Self::Planar {
            chans,
            frames,
            transparent: false,
        }
    }

    pub(crate) fn frames(&self, channels: usize) -> usize {
        match self {
            Self::F32(x) => x.len() / channels.max(1),
            Self::Planar { frames, .. } => *frames,
        }
    }

    /// Frames `start..start + len`.
    pub(crate) fn slice(&self, channels: usize, start: usize, len: usize) -> Self {
        match *self {
            Self::F32(x) => Self::F32(&x[start * channels..(start + len) * channels]),
            Self::Planar {
                chans, transparent, ..
            } => {
                let mut out = chans;
                for c in out.iter_mut().take(channels) {
                    *c = &c[start..start + len];
                }
                Self::Planar {
                    chans: out,
                    frames: len,
                    transparent,
                }
            }
        }
    }
}

/// What the processor plays for a sink at a device ratio
/// ([`DaemonProcessor::mode`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayMode {
    /// Direct or DSP off at x1: the samples unchanged (volume aside).
    Bypass,
    /// Direct or DSP off away from x1: only the engine bypass resampler's
    /// chain ([`ActiveChain::Resample`]) plays; silence until it is there.
    ResampleOnly,
    /// The DSP sink with DSP on: the configured chain.
    Full,
}

/// Chain replacement queued on the dedicated swap ring buffer.
pub struct PendingSwap {
    pub chain: ActiveChain,
    pub crossfade: bool,
    pub generation: u64,
    pub swap_seq: u64,
}

/// Deterministic self-test job executed on the RT thread at the exact
/// `EngineAudioProcessor` Direct conversion boundary.
pub struct SelfTestJob {
    pub input_f32: Vec<f32>,
    pub output_format: Option<SampleFormat>,
    pub actual_hash: AtomicU64,
    pub actual_frames: AtomicU64,
    pub done: AtomicBool,
}

impl SelfTestJob {
    pub fn new(input_f32: Vec<f32>, output_format: Option<SampleFormat>) -> Arc<Self> {
        Arc::new(Self {
            input_f32,
            output_format,
            actual_hash: AtomicU64::new(0),
            actual_frames: AtomicU64::new(0),
            done: AtomicBool::new(false),
        })
    }
}

/// Non-swap commands pushed from worker threads into [`DaemonProcessor`].
pub enum RtDspCmd {
    AbSelect {
        side: AbSide,
        crossfade_ms: f64,
    },
    /// A new fixed offset for B on the running pair (ramps over 50 ms).
    AbLevel {
        offset_db: f64,
    },
    SetDspEnabled(bool),
    /// As `SetDspEnabled`, with the chain for the new mode on its way (a
    /// DSP toggle at the same ratio on the DSP sink): the outgoing chain
    /// keeps playing until it takes over (`DaemonProcessor::plays_active`).
    SetDspEnabledHolding(bool),
    RunSelfTest(Arc<SelfTestJob>),
}

/// Lock-free atomic metering bank updated once per RT audio block and reset
/// on each [`MeterBank::read_and_reset`] call.
pub struct MeterBank {
    channels: AtomicU32,
    peak_bits: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    input_peak_bits: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    chain_peak_bits: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    /// Blocks in the current window whose float peaks were measured.
    chain_blocks: AtomicU64,
    limiter_active: AtomicBool,
    limiter_headroom: AtomicU64,
    limiter_reduction: AtomicU64,
    limiter_events: AtomicU64,
    sum_sq_fixed: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    sample_count: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    clipped: AtomicU64,
    clipped_ch: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    /// A/B overs counted to A and B (`f64` bits), since the last
    /// [`Self::reset_ab_overs`]; not cleared by a meter read.
    ab_overs_bits: [AtomicU64; 2],
    /// The last block was bypass away from x1 without a chain for the
    /// ratio, written as silence ([`Self::bypass_muted`]).
    bypass_muted: AtomicBool,
    /// The running A/B level measurement's epoch, 0 when none runs.
    ab_measure_epoch: AtomicU32,
    /// Its hops, from the audio thread to the control plane.
    ab_hops: AbHopRing,
}

/// Hops an [`AbHopRing`] holds (25.6 s; the control plane drains it every
/// 100 ms). When full, new hops are dropped.
const AB_HOP_RING: usize = 256;

/// Single-producer (the audio thread), single-consumer ring of A/B
/// measurement hops: wait-free on the producer side. The consumer side
/// takes a mutex, off the audio thread only.
struct AbHopRing {
    /// Hops written and read so far.
    written: AtomicU64,
    read: AtomicU64,
    /// `[epoch, A bits, B bits]` per slot.
    slots: [[AtomicU64; 3]; AB_HOP_RING],
    consumer: std::sync::Mutex<()>,
}

impl AbHopRing {
    fn new() -> Self {
        Self {
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
            slots: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            consumer: std::sync::Mutex::new(()),
        }
    }

    /// Audio thread only. Allocation-, lock- and wait-free.
    #[inline]
    fn push(&self, hop: AbHop) {
        let w = self.written.load(Ordering::Relaxed);
        if w.wrapping_sub(self.read.load(Ordering::Acquire)) >= AB_HOP_RING as u64 {
            return;
        }
        let slot = &self.slots[(w % AB_HOP_RING as u64) as usize];
        slot[0].store(u64::from(hop.epoch), Ordering::Relaxed);
        slot[1].store(hop.a.to_bits(), Ordering::Relaxed);
        slot[2].store(hop.b.to_bits(), Ordering::Relaxed);
        self.written.store(w.wrapping_add(1), Ordering::Release);
    }

    fn drain(&self, out: &mut Vec<AbHop>) {
        let _consumer = self.consumer.lock().unwrap_or_else(|e| e.into_inner());
        let w = self.written.load(Ordering::Acquire);
        let mut r = self.read.load(Ordering::Relaxed);
        while r != w {
            let slot = &self.slots[(r % AB_HOP_RING as u64) as usize];
            out.push(AbHop {
                epoch: slot[0].load(Ordering::Relaxed) as u32,
                a: f64::from_bits(slot[1].load(Ordering::Relaxed)),
                b: f64::from_bits(slot[2].load(Ordering::Relaxed)),
            });
            r = r.wrapping_add(1);
        }
        self.read.store(r, Ordering::Release);
    }
}

const FIXED_SCALE: f64 = 4_294_967_296.0; // 2^32

/// Lock-free atomic maximum on non-negative `f64` values stored as IEEE-754
/// bits in an [`AtomicU64`]. Positive finite floats compare identically to
/// unsigned integers, and `0.0_f64.to_bits() == 0`.
#[inline]
fn atomic_fetch_max_nonneg_f64(slot: &AtomicU64, value: f64) {
    if value.is_nan() || value <= 0.0 {
        return;
    }
    let bits = value.min(f64::MAX).to_bits();
    let mut cur = slot.load(Ordering::Relaxed);
    while bits > cur {
        match slot.compare_exchange_weak(cur, bits, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }
}

/// Lock-free atomic add of a non-negative `f64` to one stored as bits.
#[inline]
fn atomic_add_nonneg_f64(slot: &AtomicU64, value: f64) {
    if value.is_nan() || value <= 0.0 {
        return;
    }
    let mut cur = slot.load(Ordering::Relaxed);
    loop {
        let next = (f64::from_bits(cur) + value).to_bits();
        match slot.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => cur = actual,
        }
    }
}

impl MeterBank {
    pub fn new(channels: usize) -> Arc<Self> {
        Arc::new(Self {
            channels: AtomicU32::new((channels.clamp(1, MAX_METER_CHANNELS)) as u32),
            peak_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            input_peak_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            chain_peak_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            chain_blocks: AtomicU64::new(0),
            limiter_active: AtomicBool::new(false),
            limiter_headroom: AtomicU64::new(0),
            limiter_reduction: AtomicU64::new(0),
            limiter_events: AtomicU64::new(0),
            sum_sq_fixed: std::array::from_fn(|_| AtomicU64::new(0)),
            sample_count: std::array::from_fn(|_| AtomicU64::new(0)),
            clipped: AtomicU64::new(0),
            clipped_ch: std::array::from_fn(|_| AtomicU64::new(0)),
            ab_overs_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            bypass_muted: AtomicBool::new(false),
            ab_measure_epoch: AtomicU32::new(0),
            ab_hops: AbHopRing::new(),
        })
    }

    /// Meter an A/B pair for the level measurement `epoch` (0 stops). The
    /// audio thread picks it up on its next block.
    pub fn set_ab_measure_epoch(&self, epoch: u32) {
        self.ab_measure_epoch.store(epoch, Ordering::Release);
    }

    /// Append the measurement hops the audio thread completed since the
    /// last call to `out` (off the audio thread).
    pub fn take_ab_hops(&self, out: &mut Vec<AbHop>) {
        self.ab_hops.drain(out);
    }

    /// Whether the last block was Direct or DSP off away from x1 with no chain
    /// for the ratio yet (a transition): nothing can play the source unchanged
    /// there, so it was silence until the chain or an x1 reopen lands. Status
    /// reports it as an error while playing, so silence is never silent.
    /// Cleared by a reset and by an update at x1, not by a read.
    pub fn bypass_muted(&self) -> bool {
        self.bypass_muted.load(Ordering::Relaxed)
    }

    #[inline]
    fn set_bypass_muted(&self, muted: bool) {
        if self.bypass_muted.load(Ordering::Relaxed) != muted {
            self.bypass_muted.store(muted, Ordering::Relaxed);
        }
    }

    /// A/B overs counted to A and B since the last [`Self::reset_ab_overs`].
    pub fn ab_overs(&self) -> [f64; 2] {
        [0, 1].map(|k| f64::from_bits(self.ab_overs_bits[k].load(Ordering::Relaxed)))
    }

    /// Restart the A/B over counts (a new comparison or a select).
    pub fn reset_ab_overs(&self) {
        for slot in &self.ab_overs_bits {
            slot.store(0, Ordering::Relaxed);
        }
    }

    /// Per-channel peak of the chain input (before the EQ), same window as
    /// [`Self::record_i32_output`]. Strictly allocation-free and lock-free.
    #[inline]
    pub fn record_input_peaks(&self, input: &[f32], channels: usize) {
        let ch = channels.clamp(1, MAX_METER_CHANNELS);
        if input.is_empty() {
            return;
        }
        let mut peaks = [0.0_f64; MAX_METER_CHANNELS];
        for frame in input.chunks_exact(ch) {
            for (c, &sample) in frame.iter().enumerate() {
                let magnitude = f64::from(sample.abs());
                if magnitude > peaks[c] {
                    peaks[c] = magnitude;
                }
            }
        }
        for (c, &peak) in peaks.iter().enumerate().take(ch) {
            atomic_fetch_max_nonneg_f64(&self.input_peak_bits[c], peak);
        }
    }

    /// The held automatic headroom of the last block (dB), 0 when the
    /// limiter was not in its path. Read-only: nothing resets.
    pub fn limiter_held_db(&self) -> f64 {
        if self.limiter_active.load(Ordering::Relaxed) {
            f64::from_bits(self.limiter_headroom.load(Ordering::Relaxed)).max(0.0)
        } else {
            0.0
        }
    }

    /// Update meters from interleaved left-justified `i32` DAC output frames.
    /// When `clip_stats` carries measured float peaks (DSP path), the output
    /// peak is the float signal just before quantization (after the master
    /// volume, so it can read above 0 dBFS when the quantizer clips) and the
    /// chain peak before the master volume is recorded too; otherwise the peak
    /// comes from the integer output. Allocation-free and lock-free.
    #[inline]
    pub fn record_i32_output(&self, out: &[i32], channels: usize, clip_stats: ClipStats) {
        let ch = channels.clamp(1, MAX_METER_CHANNELS);
        self.channels.store(ch as u32, Ordering::Relaxed);
        let limiter = clip_stats.limiter;
        self.limiter_active.store(limiter.active, Ordering::Relaxed);
        self.limiter_headroom
            .store(limiter.headroom_db.to_bits(), Ordering::Relaxed);
        atomic_fetch_max_nonneg_f64(&self.limiter_reduction, limiter.gain_reduction_db);
        self.limiter_events
            .fetch_add(limiter.events, Ordering::Relaxed);
        for (slot, &add) in self.ab_overs_bits.iter().zip(&clip_stats.ab_overs) {
            atomic_add_nonneg_f64(slot, add);
        }
        if clip_stats.clipped > 0 {
            self.clipped
                .fetch_add(clip_stats.clipped, Ordering::Relaxed);
            for (c, &count) in clip_stats.clipped_ch.iter().enumerate().take(ch) {
                if count > 0 {
                    self.clipped_ch[c].fetch_add(count, Ordering::Relaxed);
                }
            }
        }
        let frames = out.len() / ch;
        if frames == 0 {
            return;
        }
        let mut peaks = [0.0_f64; MAX_METER_CHANNELS];
        let mut sum_sq = [0.0_f64; MAX_METER_CHANNELS];
        let inv_i32 = 1.0 / 2_147_483_648.0_f64;

        for frame in out.chunks_exact(ch) {
            for (c, &s) in frame.iter().enumerate() {
                let v = (s as f64) * inv_i32;
                let av = v.abs();
                if av > peaks[c] {
                    peaks[c] = av;
                }
                sum_sq[c] += v * v;
            }
        }

        if clip_stats.measured {
            for c in 0..ch {
                let slot = c.min(clip_stats.peak_ch.len() - 1);
                peaks[c] = clip_stats.peak_ch[slot];
                atomic_fetch_max_nonneg_f64(
                    &self.chain_peak_bits[c],
                    clip_stats.chain_peak_ch[slot],
                );
            }
            self.chain_blocks.fetch_add(1, Ordering::Relaxed);
        }
        for c in 0..ch {
            atomic_fetch_max_nonneg_f64(&self.peak_bits[c], peaks[c]);
            let s_fix = (sum_sq[c].clamp(0.0, 1.0e9) * FIXED_SCALE) as u64;
            self.sum_sq_fixed[c].fetch_add(s_fix, Ordering::Relaxed);
            self.sample_count[c].fetch_add(frames as u64, Ordering::Relaxed);
        }
    }

    pub fn read_and_reset(&self, xruns: u64, dsp_load: f64) -> Meters {
        let ch = (self.channels.load(Ordering::Relaxed) as usize).clamp(1, MAX_METER_CHANNELS);
        let mut peak_db = Vec::with_capacity(ch);
        let mut rms_db = Vec::with_capacity(ch);
        let mut input_peak_db = Vec::with_capacity(ch);
        let mut clipped_ch = Vec::with_capacity(ch);
        let chain_measured = self.chain_blocks.swap(0, Ordering::AcqRel) > 0;
        let mut chain_peak_db = Vec::with_capacity(if chain_measured { ch } else { 0 });
        for c in 0..ch {
            let chain_lin = f64::from_bits(self.chain_peak_bits[c].swap(0, Ordering::AcqRel));
            if chain_measured {
                chain_peak_db.push(lin_to_dbfs(chain_lin));
            }
            let peak_lin = f64::from_bits(self.peak_bits[c].swap(0, Ordering::AcqRel));
            let input_peak_lin = f64::from_bits(self.input_peak_bits[c].swap(0, Ordering::AcqRel));
            let s_fix = self.sum_sq_fixed[c].swap(0, Ordering::AcqRel);
            let n = self.sample_count[c].swap(0, Ordering::AcqRel);

            peak_db.push(lin_to_dbfs(peak_lin));
            input_peak_db.push(lin_to_dbfs(input_peak_lin));

            let rms_lin = if n > 0 {
                ((s_fix as f64) / FIXED_SCALE / (n as f64)).sqrt()
            } else {
                0.0
            };
            rms_db.push(lin_to_dbfs(rms_lin));
            clipped_ch.push(self.clipped_ch[c].load(Ordering::Relaxed));
        }

        Meters {
            limiter: Some(audivane_protocol::LimiterStatus {
                active: self.limiter_active.load(Ordering::Relaxed),
                headroom_db: f64::from_bits(self.limiter_headroom.load(Ordering::Relaxed)),
                gain_reduction_db: f64::from_bits(
                    self.limiter_reduction.swap(0, Ordering::Relaxed),
                ),
                events: self.limiter_events.load(Ordering::Relaxed),
            }),
            peak_db,
            rms_db,
            clipped: self.clipped.load(Ordering::Relaxed),
            clipped_ch,
            input_peak_db,
            chain_peak_db,
            xruns,
            dsp_load,
        }
    }
}

#[inline]
fn lin_to_dbfs(lin: f64) -> f64 {
    if lin <= 1.0e-8 {
        SILENCE_FLOOR_DB
    } else {
        (20.0 * lin.log10()).max(SILENCE_FLOOR_DB)
    }
}

/// Active chain-swap state machine. Guarantees at most two chains ever run
/// concurrently on the RT thread:
/// - `Warming`: feeds the same `dsp_in` into `new_chain` in parallel while
///   `active_chain` serves audio until `new_chain` has emitted at least
///   `latency_out_frames + settling_margin` output frames.
/// - `Crossfading`: when `old_lat == new_lat`, equal-latency raised-cosine
///   crossfade over 10 ms.
/// - `FadeOutOld` -> `FadeInNew`: when `old_lat != new_lat`, smoothly fades
///   `old` to zero over 5 ms (keeping `pending_new` warmed in parallel), switches
///   at zero without comb-filtering, then fades `new` in over 5 ms.
enum SwapPhase {
    Idle,
    Warming {
        new_chain: ActiveChain,
        generation: u64,
        swap_seq: u64,
        target_frames: usize,
        warmed_frames: usize,
        crossfade: bool,
    },
    Crossfading {
        fading_old: ActiveChain,
        fade_pos: usize,
        fade_len: usize,
    },
    FadeOutOld {
        pending_new: ActiveChain,
        generation: u64,
        swap_seq: u64,
        fade_pos: usize,
        fade_len: usize,
    },
    FadeInNew {
        fade_pos: usize,
        fade_len: usize,
    },
}

/// A raised-cosine fade on the output, `pos` of `len` frames done, from
/// gain `from` (a fade can start where another one stopped).
#[derive(Clone, Copy, Debug, PartialEq)]
struct OutFade {
    pos: usize,
    len: usize,
    from: f64,
}

impl OutFade {
    fn new(len: usize, from: f64) -> Self {
        Self {
            pos: 0,
            len: len.max(1),
            from,
        }
    }

    /// `0.5 + 0.5 cos(π t)`: 1 at `t = 0`, 0 at `t = 1`.
    fn down(t: f64) -> f64 {
        0.5 + 0.5 * (PI * t.clamp(0.0, 1.0)).cos()
    }

    /// The gain a fade-out has reached.
    fn out_gain(&self) -> f64 {
        self.from * Self::down(self.pos as f64 / self.len as f64)
    }

    /// The gain a fade-in has reached.
    fn in_gain(&self) -> f64 {
        self.from + (1.0 - self.from) * (1.0 - Self::down(self.pos as f64 / self.len as f64))
    }

    /// Fade `out` (interleaved) down. A block that reaches the end goes to
    /// silence at its own last frame (so the switch is at a block edge).
    /// Returns whether it reached silence.
    fn apply_out(&mut self, out: &mut [i32], channels: usize) -> bool {
        let frames = out.len() / channels.max(1);
        let last = self.pos + frames >= self.len;
        let w0 = self.out_gain();
        for (i, frame) in out.chunks_exact_mut(channels.max(1)).enumerate() {
            let w = if last {
                w0 * Self::down((i + 1) as f64 / frames as f64)
            } else {
                self.from * Self::down((self.pos + i + 1) as f64 / self.len as f64)
            };
            scale_frame(frame, w);
        }
        self.pos += frames;
        last
    }

    /// Fade `out` up. Returns whether it reached full gain.
    fn apply_in(&mut self, out: &mut [i32], channels: usize) -> bool {
        for frame in out.chunks_exact_mut(channels.max(1)) {
            if self.pos >= self.len {
                break;
            }
            self.pos += 1;
            scale_frame(frame, self.in_gain());
        }
        self.pos >= self.len
    }
}

fn scale_frame(frame: &mut [i32], w: f64) {
    for x in frame {
        *x = (f64::from(*x) * w)
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    }
}

/// The DSP sink's transition between what it plays: one at a time, driven
/// by [`DaemonProcessor::poll_updates`] and `process_block`, and ended in
/// one place ([`DaemonProcessor::complete_transition`]), also by a reset
/// or a stop that cuts it short.
#[derive(Debug)]
enum Transition {
    Idle,
    /// Fading out before a switch, then `toggle` applies (and, after an
    /// output fade, the held swap installs). `out: Some`: the chain leaves
    /// with the switch (DSP off at x1, a passthrough chain under the
    /// stage), so the processor fades its rendered output, a delayed
    /// chain's included. `None`: the chain stays (DSP off away from x1);
    /// the stage fades on the source side and the switch follows its stop.
    FadingOut {
        toggle: Option<(bool, bool)>,
        out: Option<OutFade>,
    },
    /// After the switch (or a stop of the stage): the output fades in.
    FadingIn(OutFade),
}

/// Real-time audio processor bridging `EngineDriverHandle` and `dsp-core`.
pub struct DaemonProcessor {
    active_chain: ActiveChain,
    active_generation: u64,
    active_generation_atomic: Arc<AtomicU64>,
    /// Packed `(active_generation << 1) | u64::from(in_flight)` updated
    /// atomically after `swap_phase` / `held_swap` transitions so readers
    /// never see a promoted generation before `in_flight` flips on.
    swap_state: Arc<AtomicU64>,
    active_swap_seq: u64,
    /// Chains installed so far (a measurement faults on a swap).
    chain_installs: u64,
    swap_drain_paused: Arc<AtomicBool>,
    swap_phase: SwapPhase,
    dsp_enabled: bool,
    /// The last DSP toggle has the chain for its new mode on its way
    /// ([`RtDspCmd::SetDspEnabledHolding`]); cleared when that chain installs
    /// (the DSP chain with DSP on, the resample-only one with DSP off), when no
    /// chain does (a reopen to x1), or by a plain [`RtDspCmd::SetDspEnabled`]
    /// (the engine cancels it). An older swap of the outgoing kind installing
    /// (an edit just before the toggle) keeps it.
    hold_outgoing: bool,
    max_block: usize,
    active_rate: u32,
    /// The device ratio of the last update or block ([`Self::dsp_active`]).
    running_ratio: RateRatio,
    /// Input position the next DSP block starts at (the driver's frame
    /// grid; see `EngineAudioProcessor::process_block`). New chains join
    /// the stream here.
    next_pos: u64,
    ab_side: Arc<AtomicU8>,
    shared_volume: Arc<SharedVolume>,
    volume_gen: u64,
    volume_target: VolumeTarget,
    dsp_gain: StereoGainRamp,
    direct_mute: MuteRamp,
    /// Dither for the Direct / bypass paths while their digital gain is not
    /// unity, at the DAC's bit depth ([`Self::dac_bits_handle`]; 32 = none).
    direct_dither: TpdfDither,
    dac_bits: Arc<AtomicU32>,
    meters: Arc<MeterBank>,
    cmd_rx: rtrb::Consumer<RtDspCmd>,
    swap_rx: rtrb::Consumer<PendingSwap>,
    recycle_tx: rtrb::Producer<ActiveChain>,
    recycle_stash: [Option<ActiveChain>; RECYCLE_STASH_CAP],
    swap_scratch: Vec<i32>,
    /// Floating-point chain output for the post spectrum tap, captured just
    /// before the quantizer/dither.
    post_scratch: Vec<f32>,
    selftest_scratch: Vec<i32>,
    spectrum: Option<SpectrumRt>,
    /// The harmonics live view's rings ([`HarmonicsLiveRt`]).
    harmonics_live: Option<HarmonicsLiveRt>,
    /// The varispeed stage's slot (before every chain).
    vs: VarispeedRt,
    /// The toggle and fade in flight ([`Transition`]): on top of whatever
    /// a chain swap does, so a swap never eats a fade.
    transition: Transition,
    /// The newest queued swap, taken (coalesced, latest wins) but not
    /// begun: it waits while the swap phase is busy or a transition holds
    /// it for its switch.
    held_swap: Option<PendingSwap>,
    /// Graph frames of the last block: the period a swap's handover is
    /// budgeted against ([`handover_budget`]).
    period_frames: usize,
    /// Multi-device: the followers' tee, the master's position at the DAC,
    /// the command log and the alignment delay (`tee`). Inert until a
    /// follower is attached.
    tee: TeeRt,
    /// Measurement playback (`measure_play`): the mute of other streams
    /// and the file on the routed channels. Inert until a job is posted.
    measure: MasterMeasure,
    /// Tests: run without the multi-device hooks, as the processor did
    /// before them (the cost comparison).
    #[cfg(test)]
    base_path: bool,
}

impl DaemonProcessor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_chain: ActiveChain,
        dsp_enabled: bool,
        channels: usize,
        max_block: usize,
        max_ratio: usize,
        initial_rate: u32,
        ab_side: Arc<AtomicU8>,
        meters: Arc<MeterBank>,
        cmd_rx: rtrb::Consumer<RtDspCmd>,
        swap_rx: rtrb::Consumer<PendingSwap>,
        recycle_tx: rtrb::Producer<ActiveChain>,
    ) -> Self {
        let scratch_max_block = max_block.max(8192);
        let scratch_len = scratch_max_block * max_ratio.max(16) * channels.max(1);
        let rate = initial_rate.max(8_000);
        Self {
            active_chain: initial_chain,
            active_generation: 0,
            active_generation_atomic: Arc::new(AtomicU64::new(0)),
            swap_state: Arc::new(AtomicU64::new(0)),
            active_swap_seq: 0,
            chain_installs: 0,
            swap_drain_paused: Arc::new(AtomicBool::new(false)),
            swap_phase: SwapPhase::Idle,
            dsp_enabled,
            hold_outgoing: false,
            max_block: scratch_max_block,
            active_rate: rate,
            running_ratio: RateRatio::ONE,
            next_pos: 0,
            ab_side,
            shared_volume: Arc::new(SharedVolume::default()),
            volume_gen: 0,
            volume_target: VolumeTarget::default(),
            dsp_gain: StereoGainRamp::unity(rate),
            direct_mute: MuteRamp::unmuted(rate),
            direct_dither: TpdfDither::new(0x5eed_d17e),
            dac_bits: Arc::new(AtomicU32::new(32)),
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
            recycle_stash: std::array::from_fn(|_| None),
            swap_scratch: vec![0i32; scratch_len],
            post_scratch: vec![0.0f32; scratch_len],
            selftest_scratch: vec![0i32; 4096 * channels.max(1)],
            spectrum: None,
            harmonics_live: None,
            vs: VarispeedRt::inert(),
            transition: Transition::Idle,
            held_swap: None,
            period_frames: 0,
            tee: TeeRt::inert(),
            measure: MasterMeasure::inert(channels),
            #[cfg(test)]
            base_path: false,
        }
    }

    /// Whether the multi-device hooks run: always, except in a test that
    /// times the processor without them.
    #[inline(always)]
    fn multi_device_hooks(&self) -> bool {
        #[cfg(test)]
        {
            !self.base_path
        }
        #[cfg(not(test))]
        {
            true
        }
    }

    /// Tests: take the multi-device hooks out (the base processor's path).
    #[cfg(test)]
    pub(crate) fn set_base_path(&mut self, on: bool) {
        self.base_path = on;
    }

    /// Attach the slot measurement jobs arrive by (`measure_play`).
    pub fn with_measure(mut self, slot: Arc<MeasureSlot>, channels: usize) -> Self {
        self.measure = MasterMeasure::new(slot, channels);
        self
    }

    /// Attach the multi-device tee slot (built off-RT with its host,
    /// `tee::TeeHost::pair`, before the audio thread starts).
    pub fn with_tee(mut self, tee: TeeRt) -> Self {
        self.tee = tee;
        self
    }

    /// The tee slot (tests).
    #[cfg(test)]
    pub(crate) fn tee(&self) -> &TeeRt {
        &self.tee
    }

    /// Where the varispeed stage is (tests).
    #[cfg(test)]
    pub(crate) fn varispeed_view(&self) -> Option<StageView> {
        self.vs.tee_view()
    }

    /// Multi-device: the master's map and the command log after a block of
    /// `out_frames` device frames (only while a follower is attached).
    /// `index`: the block's source index; `v0`: the stage before its call
    /// (`None`: no stage ran).
    #[allow(clippy::too_many_arguments)]
    fn track_master(
        &mut self,
        full: bool,
        planned: bool,
        index: u64,
        src_frames: usize,
        v0: Option<StageView>,
        ran: bool,
        out_frames: usize,
        ratio: RateRatio,
    ) {
        let chain_graph = self.active_chain.latency_out_frames() as f64 / ratio.as_f64();
        self.tee.note_chain_latency(chain_graph);
        let v1 = self.vs.tee_view();
        let span = match (full, ran, v0, v1) {
            (true, true, Some(v0), Some(v1)) => {
                let origin = self.tee.stage_origin(index, v0);
                let tee = &self.tee;
                self.vs
                    .drain_issued(|at, cmd| tee.log(origin + (at >> 1) as i128, cmd));
                // The next output frame sits a core output after the
                // core's next position (the ÷2 took one less at the
                // stage's start).
                let next = v1.acc.wrapping_add(v1.step);
                self.tee
                    .stage_block(origin + (next >> 1) as i128, v1.settled_state());
                Some((
                    self.tee.stage_span(index, v0, v1, chain_graph),
                    (v0.speed, v1.speed),
                    v0.ramping || v1.ramping,
                ))
            }
            (true, false, _, _) if !planned => {
                let first = self.tee.input_position(index);
                self.tee
                    .plain_block(first, first + src_frames as i128 * FRAME);
                Some((
                    self.tee.plain_span(index, src_frames, chain_graph),
                    (1.0, 1.0),
                    false,
                ))
            }
            _ => None,
        };
        match span {
            Some((span, speed, ramping)) => {
                self.tee.note_block(Some(span), out_frames, speed, ramping);
            }
            None => self.tee.note_block(None, out_frames, (1.0, 1.0), false),
        }
    }

    /// Where retired stretches (Keep pitch) go, to the same reaper as the
    /// varispeed stages (after [`Self::with_varispeed`]).
    pub fn with_stretch_recycle(
        mut self,
        recycle: rtrb::Producer<Box<audivane_dsp_core::tempo::TempoStretch>>,
    ) -> Self {
        self.vs =
            std::mem::replace(&mut self.vs, VarispeedRt::inert()).with_stretch_recycle(recycle);
        self
    }

    /// Attach the varispeed slot: stages arrive on `rx` and retire into
    /// `recycle` (constructed off-RT, before the audio thread starts; the
    /// buffers for `channels` channels are allocated here).
    pub fn with_varispeed(
        mut self,
        rx: rtrb::Consumer<VarispeedRtCmd>,
        recycle: rtrb::Producer<Box<Varispeed>>,
        channels: usize,
    ) -> Self {
        self.vs = VarispeedRt::new(rx, recycle, Arc::new(VarispeedApplied::default()), channels);
        self
    }

    /// The channel count blocks run at (the meters').
    fn vs_channels(&self) -> usize {
        self.meters.channels.load(Ordering::Relaxed) as usize
    }

    /// What the varispeed stage applies (published after every block).
    pub fn varispeed_applied(&self) -> Arc<VarispeedApplied> {
        self.vs.applied()
    }

    /// Retired stages held on the RT thread (tests).
    #[cfg(test)]
    pub(crate) fn varispeed_stash_len(&self) -> usize {
        self.vs.stash_len()
    }

    /// Attach the harmonics live view's rings (constructed off-RT, before
    /// the audio thread starts).
    pub fn with_harmonics_live(mut self, live: HarmonicsLiveRt) -> Self {
        self.harmonics_live = Some(live);
        self
    }

    /// Attach the live-spectrum rings (constructed off-RT, before the audio
    /// thread starts).
    pub fn with_spectrum(mut self, spectrum: SpectrumRt) -> Self {
        self.spectrum = Some(spectrum);
        self
    }

    /// Copy the pre tap and a quantized post tap. Used on paths that have no
    /// floating-point output (Direct / bypass / muted mismatch). No-op (and no
    /// atomics) until a client subscribes; allocation-free either way.
    #[inline]
    fn tap_spectrum_i32(&mut self, pre: &[f32], out: &[i32], graph_rate: u32, ratio: RateRatio) {
        let Some(spectrum) = self.spectrum.as_mut() else {
            return;
        };
        spectrum.push_pre(pre, graph_rate);
        spectrum.push_post_i32(out, out_rate(graph_rate, ratio));
    }

    #[cfg(test)]
    pub(crate) fn set_dsp_enabled_for_test(&mut self, enabled: bool) {
        self.dsp_enabled = enabled;
    }

    #[cfg(test)]
    pub(crate) fn meters(&self) -> &MeterBank {
        &self.meters
    }

    /// Start at `target` (settled, no ramp) instead of unity, so audio that
    /// plays before the builder's first volume publication is never louder
    /// than the stored volume. Construction only, before the audio thread.
    pub fn with_initial_volume(mut self, target: VolumeTarget) -> Self {
        self.shared_volume.store(target);
        self.volume_gen = 0;
        if let Some(t) = self.shared_volume.read_if_changed(&mut self.volume_gen) {
            self.volume_target = t;
            self.dsp_gain.set_volume_targets(
                t.volume_db,
                t.balance_db,
                t.muted,
                t.hardware_volume,
                t.hardware_mute,
                false,
            );
            self.direct_mute
                .set_muted(t.muted && !t.hardware_mute, false);
        }
        self
    }

    pub fn volume_handle(&self) -> Arc<SharedVolume> {
        Arc::clone(&self.shared_volume)
    }

    /// The DAC format's bit depth, set by the builder on every negotiation.
    pub fn dac_bits_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.dac_bits)
    }

    fn sync_volume_state(&mut self, out_rate: u32, ramp: bool) {
        let out_rate = out_rate.max(8_000);
        self.dsp_gain.set_sample_rate(out_rate);
        self.direct_mute.set_sample_rate(out_rate);
        if let Some(target) = self.shared_volume.read_if_changed(&mut self.volume_gen) {
            self.volume_target = target;
            self.dsp_gain.set_volume_targets(
                target.volume_db,
                target.balance_db,
                target.muted,
                target.hardware_volume,
                target.hardware_mute,
                ramp,
            );
            self.direct_mute
                .set_muted(target.muted && !target.hardware_mute, ramp);
        }
    }

    pub fn active_generation(&self) -> u64 {
        self.active_generation
    }

    pub fn active_generation_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.active_generation_atomic)
    }

    /// Packed `(active_generation << 1) | u64::from(in_flight)` handle for
    /// tear-free observation of the promoted generation together with whether
    /// any swap phase or held swap is still in flight on the RT thread.
    pub fn swap_state_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.swap_state)
    }

    fn publish_swap_state(&self) {
        let in_flight = !matches!(self.swap_phase, SwapPhase::Idle) || self.held_swap.is_some();
        // Generations count config edits from 0: 2^63 of them (the top bit
        // this packing drops) cannot happen.
        let word = (self.active_generation << 1) | u64::from(in_flight);
        self.swap_state.store(word, Ordering::Release);
    }

    /// The generation of the chain warming up next to the active one (it
    /// runs on the audio thread, unheard, before it takes over), else the
    /// active one's.
    fn newest_running_generation(&self) -> u64 {
        match &self.swap_phase {
            SwapPhase::Warming { generation, .. } | SwapPhase::FadeOutOld { generation, .. } => {
                (*generation).max(self.active_generation)
            }
            _ => self.active_generation,
        }
    }

    pub fn active_swap_seq(&self) -> u64 {
        self.active_swap_seq
    }

    pub fn swap_drain_paused_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.swap_drain_paused)
    }

    pub fn set_swap_drain_paused(&self, paused: bool) {
        self.swap_drain_paused.store(paused, Ordering::Release);
    }

    fn set_active_swap(&mut self, generation: u64, swap_seq: u64) {
        self.active_generation = generation;
        self.active_generation_atomic
            .store(generation, Ordering::Release);
        self.active_swap_seq = swap_seq;
    }

    /// Number of retired chains currently queued in the RT-owned bounded
    /// recycle stash (for testing / diagnostics).
    pub fn recycle_stash_len(&self) -> usize {
        self.recycle_stash.iter().filter(|s| s.is_some()).count()
    }

    fn free_stash_slots(&self) -> usize {
        self.recycle_stash.iter().filter(|s| s.is_none()).count()
    }

    /// Available slots to retire `ActiveChain` instances off-RT while keeping
    /// one emergency stash slot reserved for `reset_dsp_state`.
    fn recycle_capacity(&self) -> usize {
        self.recycle_tx.slots() + self.free_stash_slots().saturating_sub(1)
    }

    /// Flush any RT-stashed chains into `recycle_tx` without dropping.
    fn flush_recycle_stash(&mut self) {
        for slot in &mut self.recycle_stash {
            if let Some(chain) = slot.take()
                && let Err(rtrb::PushError::Full(returned)) = self.recycle_tx.push(chain)
            {
                *slot = Some(returned);
                break;
            }
        }
    }

    /// Retire `chain` off-RT if `recycle_tx` has capacity, else hold it in a
    /// pre-allocated RT stash slot.
    ///
    /// RT swap callers (`can_accept_swap()`, `clear_unadvanceable_swap_phase()`)
    /// require `recycle_capacity() >= 1`, which reserves one of the
    /// `RECYCLE_STASH_CAP` (4) slots for `reset_dsp_state()`, so a free stash
    /// slot always exists when `recycle_tx` is full. The fallback below is
    /// therefore unreachable unless that invariant is violated, and leaks via
    /// `std::mem::forget` rather than deallocating on the RT thread.
    fn push_recycle(&mut self, chain: ActiveChain) {
        self.flush_recycle_stash();
        if let Err(rtrb::PushError::Full(returned)) = self.recycle_tx.push(chain) {
            for slot in &mut self.recycle_stash {
                if slot.is_none() {
                    *slot = Some(returned);
                    return;
                }
            }
            // Reachable only if invariants were bypassed; leak rather than
            // deallocating on the RT thread.
            debug_assert!(
                false,
                "push_recycle invariant violated: recycle ring and stash both full"
            );
            std::mem::forget(returned);
        }
    }

    /// Only accept a new swap when no swap is currently in progress and there is
    /// guaranteed capacity to retire the old chain without ever dropping on RT.
    fn can_accept_swap(&self) -> bool {
        matches!(self.swap_phase, SwapPhase::Idle) && self.recycle_capacity() >= 1
    }

    fn apply_dsp_enabled(&mut self, en: bool, holding: bool) {
        self.dsp_enabled = en;
        self.hold_outgoing = holding;
    }

    /// Take the queued swaps into [`Self::held_swap`], latest wins, as far
    /// as the recycle queue can retire the ones passed over (one slot stays
    /// for the chain the winner replaces).
    fn take_swaps(&mut self) {
        loop {
            let need = if self.held_swap.is_some() { 2 } else { 1 };
            if self.recycle_capacity() < need {
                break;
            }
            let Ok(newer) = self.swap_rx.pop() else {
                break;
            };
            if let Some(old) = self.held_swap.replace(newer) {
                self.push_recycle(old.chain);
            }
        }
    }

    /// Decide the transition for this update: a toggle that arrived (`toggle`),
    /// the swap in hand (already coalesced), and what plays. DSP off at x1, or a
    /// passthrough chain under a playing stage, drops the chain: its output fades
    /// first. DSP off away from x1 with the stage playing fades the stage (the
    /// chain stays). Anything else applies at once, and takes back a fade that no
    /// longer has a reason.
    fn plan_transition(
        &mut self,
        toggle: Option<(bool, bool)>,
        sink: SinkChoice,
        ratio: RateRatio,
        streaming: bool,
    ) {
        let toggle = match (&self.transition, toggle) {
            (_, Some(t)) => Some(t),
            (Transition::FadingOut { toggle, .. }, None) => *toggle,
            _ => None,
        };
        // Nothing to switch: a toggle to the state it is in.
        let toggle = toggle
            .filter(|(en, holding)| *en != self.dsp_enabled || *holding != self.hold_outgoing);
        let target_on = toggle.map_or(self.dsp_enabled, |t| t.0);
        let chain_audible = streaming
            && sink == SinkChoice::Dsp
            && self.dsp_enabled
            && !matches!(self.active_chain, ActiveChain::Bypass);
        let full = self.mode(sink, ratio) == PlayMode::Full;
        let stage_plays = self.vs.plays_now(streaming, full);
        let passthrough = matches!(
            self.held_swap.as_ref().map(|s| &s.chain),
            Some(ActiveChain::Bypass)
        );
        let leaves =
            chain_audible && ratio.is_unity() && (!target_on || (passthrough && stage_plays));
        let stage_fade = !leaves && !target_on && self.dsp_enabled && stage_plays;
        if leaves {
            let len = self.fade_len_for(SWAP_FADE_HALF_MS, ratio);
            let out = match std::mem::replace(&mut self.transition, Transition::Idle) {
                Transition::FadingOut { out: Some(f), .. } => f,
                Transition::FadingIn(f) => OutFade::new(len, f.in_gain()),
                _ => OutFade::new(len, 1.0),
            };
            self.vs.prepare_cut();
            self.transition = Transition::FadingOut {
                toggle,
                out: Some(out),
            };
        } else if stage_fade {
            if !matches!(self.transition, Transition::FadingOut { out: None, .. }) {
                self.cancel_out_fade();
            }
            self.vs.request_suspend(streaming);
            self.transition = Transition::FadingOut { toggle, out: None };
        } else {
            match &self.transition {
                Transition::FadingOut { out: None, .. } if !self.vs.cancel_stop() => {
                    // The stage is already fading: it stops, and nothing
                    // is switched after it.
                    self.transition = Transition::FadingOut {
                        toggle: None,
                        out: None,
                    };
                }
                Transition::FadingOut { .. } => self.cancel_out_fade(),
                _ => {}
            }
            if let Some((en, holding)) = toggle {
                self.apply_dsp_enabled(en, holding);
            }
        }
    }

    /// Take back an output fade-out: back up from where it got (a fade
    /// that had not started is simply dropped).
    fn cancel_out_fade(&mut self) {
        self.vs.cancel_cut();
        self.transition = match std::mem::replace(&mut self.transition, Transition::Idle) {
            Transition::FadingOut { out: Some(f), .. } if f.pos > 0 => {
                Transition::FadingIn(OutFade::new(f.len, f.out_gain()))
            }
            Transition::FadingIn(f) => Transition::FadingIn(f),
            _ => Transition::Idle,
        };
    }

    /// The one place a fade-out ends, at its switch or cut short (a reset,
    /// nothing streaming, the stage's stop ended elsewhere): the toggle
    /// applies; after an output fade the stage stops and the held swap
    /// installs at once (nothing is audible). `fade_in`: the output fades
    /// in after it.
    fn complete_transition(&mut self, fade_in: bool, ratio: RateRatio) {
        let Transition::FadingOut { toggle, out } =
            std::mem::replace(&mut self.transition, Transition::Idle)
        else {
            if !fade_in {
                self.transition = Transition::Idle;
            }
            return;
        };
        if let Some((en, holding)) = toggle {
            self.apply_dsp_enabled(en, holding);
        }
        if out.is_some() {
            self.vs.cut();
            self.install_held_swap();
        }
        if fade_in {
            self.start_fade_in(ratio);
        }
    }

    fn start_fade_in(&mut self, ratio: RateRatio) {
        self.transition = Transition::FadingIn(OutFade::new(
            self.fade_len_for(SWAP_FADE_HALF_MS, ratio),
            0.0,
        ));
    }

    /// Install the held swap now (a switch at silence, or a reset): any
    /// swap phase in progress is settled first.
    fn install_held_swap(&mut self) {
        let Some(swap) = self.held_swap.take() else {
            return;
        };
        self.settle_swap_phase();
        let mut chain = swap.chain;
        self.prepare_incoming(&mut chain);
        let old = self.install_chain(chain);
        self.set_active_swap(swap.generation, swap.swap_seq);
        self.push_recycle(old);
    }

    /// End a swap phase at once: the incoming chain installs, the outgoing
    /// one retires.
    fn settle_swap_phase(&mut self) {
        let phase = std::mem::replace(&mut self.swap_phase, SwapPhase::Idle);
        match phase {
            SwapPhase::Idle | SwapPhase::FadeInNew { .. } => {}
            SwapPhase::Warming {
                new_chain,
                generation,
                swap_seq,
                ..
            }
            | SwapPhase::FadeOutOld {
                pending_new: new_chain,
                generation,
                swap_seq,
                ..
            } => {
                let old = self.install_chain(new_chain);
                self.set_active_swap(generation, swap_seq);
                self.push_recycle(old);
            }
            SwapPhase::Crossfading { fading_old, .. } => {
                self.push_recycle(fading_old);
            }
        }
    }

    /// What `sink` plays at `ratio`: the DSP sink with DSP on plays the
    /// configured chain at any ratio; Direct and DSP off play the samples
    /// unchanged at x1 and only the resample-only chain away from it.
    pub fn mode(&self, sink: SinkChoice, ratio: RateRatio) -> PlayMode {
        if sink == SinkChoice::Dsp && self.dsp_enabled {
            PlayMode::Full
        } else if ratio.is_unity() {
            PlayMode::Bypass
        } else {
            PlayMode::ResampleOnly
        }
    }

    /// Whether `sink` plays through a chain at `ratio` (not [`PlayMode::Bypass`]).
    fn chain_runs(&self, sink: SinkChoice, ratio: RateRatio) -> bool {
        self.mode(sink, ratio) != PlayMode::Bypass
    }

    /// Whether `chain` is what `sink` plays at `ratio` at the graph rate
    /// running now: the ratio and input rate are its identity (filters are
    /// tuned for the input rate), and the resample-only chain plays exactly
    /// in [`PlayMode::ResampleOnly`].
    fn fits(&self, chain: &ActiveChain, sink: SinkChoice, ratio: RateRatio) -> bool {
        chain.ratio() == ratio
            && chain.input_rate().is_none_or(|r| r == self.active_rate)
            && chain.is_resample() == (self.mode(sink, ratio) == PlayMode::ResampleOnly)
    }

    /// Whether the active chain may play for `sink` at `ratio` (and complete
    /// a resume there).
    pub(crate) fn chain_fits(&self, sink: SinkChoice, ratio: RateRatio) -> bool {
        self.fits(&self.active_chain, sink, ratio)
    }

    /// Whether the active chain plays for `sink` at `ratio`: it fits, or it is
    /// the outgoing chain of a DSP toggle at the same ratio on the DSP sink (the
    /// DSP chain with DSP just turned off, or the resample-only chain with DSP
    /// just turned on) whose incoming chain is on its way
    /// ([`RtDspCmd::SetDspEnabledHolding`]). The outgoing chain stays audible, on
    /// the same input, until the incoming one has warmed up and taken over
    /// ([`Self::begin_swap`]), so the toggle never steps through silence or a
    /// cold filter. Without a chain on its way (PipeWire converts, after a
    /// reopen) it is silence until then. Readiness ([`Self::chain_fits`]) stays
    /// strict.
    fn plays_active(&self, sink: SinkChoice, ratio: RateRatio) -> bool {
        self.chain_fits(sink, ratio)
            || (self.hold_outgoing
                && sink == SinkChoice::Dsp
                && !ratio.is_unity()
                && self.active_chain.ratio() == ratio
                && self
                    .active_chain
                    .input_rate()
                    .is_none_or(|r| r == self.active_rate)
                && !matches!(self.active_chain, ActiveChain::Bypass))
    }

    fn is_dsp_live(
        &self,
        active_sink: SinkChoice,
        actual_ratio: RateRatio,
        is_streaming: bool,
    ) -> bool {
        is_streaming
            && self.chain_runs(active_sink, actual_ratio)
            && self.plays_active(active_sink, actual_ratio)
    }

    fn clear_unadvanceable_swap_phase(
        &mut self,
        active_sink: SinkChoice,
        actual_ratio: RateRatio,
        is_streaming: bool,
    ) {
        let dsp_live = self.is_dsp_live(active_sink, actual_ratio, is_streaming);
        let phase_ratio_matches = match &self.swap_phase {
            SwapPhase::Idle | SwapPhase::FadeInNew { .. } => true,
            SwapPhase::Warming { new_chain, .. } => new_chain.ratio() == actual_ratio,
            SwapPhase::Crossfading { fading_old, .. } => fading_old.ratio() == actual_ratio,
            SwapPhase::FadeOutOld { pending_new, .. } => pending_new.ratio() == actual_ratio,
        };
        if dsp_live && phase_ratio_matches {
            return;
        }
        match &self.swap_phase {
            SwapPhase::Idle => {}
            SwapPhase::FadeInNew { .. } => {
                self.swap_phase = SwapPhase::Idle;
            }
            SwapPhase::Warming { .. }
            | SwapPhase::Crossfading { .. }
            | SwapPhase::FadeOutOld { .. } => {
                if self.recycle_capacity() < 1 {
                    return;
                }
                let phase = std::mem::replace(&mut self.swap_phase, SwapPhase::Idle);
                match phase {
                    SwapPhase::Warming {
                        new_chain,
                        generation,
                        swap_seq,
                        ..
                    }
                    | SwapPhase::FadeOutOld {
                        pending_new: new_chain,
                        generation,
                        swap_seq,
                        ..
                    } => {
                        let old = self.install_chain(new_chain);
                        self.set_active_swap(generation, swap_seq);
                        self.push_recycle(old);
                    }
                    SwapPhase::Crossfading { fading_old, .. } => {
                        self.push_recycle(fading_old);
                    }
                    SwapPhase::Idle | SwapPhase::FadeInNew { .. } => {}
                }
            }
        }
    }

    fn fade_len_for(&self, ms: usize, ratio: RateRatio) -> usize {
        let out_rate = out_rate(self.active_rate, ratio) as usize;
        ((out_rate * ms) / 1000).max(64)
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_swap(
        &mut self,
        mut new_chain: ActiveChain,
        generation: u64,
        swap_seq: u64,
        crossfade: bool,
        active_sink: SinkChoice,
        actual_ratio: RateRatio,
        is_streaming: bool,
    ) {
        self.prepare_incoming(&mut new_chain);

        // A chain of the other kind (resample-only vs DSP) takes over
        // smoothly only when it is what the sink plays now (a DSP toggle at
        // the same ratio): the outgoing chain keeps playing while it warms
        // up, then they crossfade (or fade through silence when their
        // latencies differ). Otherwise it is installed at once.
        let same_kind = self.active_chain.is_resample() == new_chain.is_resample();
        let same_ratio = self.active_chain.ratio() == new_chain.ratio()
            && (same_kind || self.fits(&new_chain, active_sink, actual_ratio));
        let dsp_live = self.is_dsp_live(active_sink, actual_ratio, is_streaming);

        // If audio/DSP is not currently live (idle, Direct, bypass, or ratio changed
        // between chains), promote the new chain immediately without warming or fading.
        if !dsp_live || !same_ratio {
            let old = self.install_chain(new_chain);
            self.set_active_swap(generation, swap_seq);
            self.push_recycle(old);
            self.swap_phase = SwapPhase::Idle;
            return;
        }

        // An edit before an unchanged convolution: the new chain takes the
        // playing one over in place (its convolution and later stages, with
        // their state) and fades the edit in at the convolution's input,
        // so its first outputs are the playing chain's next: it replaces it
        // now, no warm-up, no crossfade after. Not when the IR's fade bound
        // refuses the edit (it warms up below).
        if new_chain.take_over_upstream(&mut self.active_chain) {
            if new_chain.is_resample() != self.dsp_enabled {
                self.hold_outgoing = false;
            }
            // A chain installed (a measurement faults on it), its fade
            // heard as a swap ([`Self::chain_swapping`]).
            self.chain_installs = self.chain_installs.wrapping_add(1);
            let old = std::mem::replace(&mut self.active_chain, new_chain);
            self.set_active_swap(generation, swap_seq);
            self.push_recycle(old);
            self.swap_phase = SwapPhase::Idle;
            return;
        }

        let new_lat = new_chain.latency_out_frames();
        // A convolution the edit keeps, with its input, plays on from the
        // outgoing chain's state (on the grid already: `prepare_incoming`);
        // one it does not (an EQ edit, a new IR, a state over the copy
        // budget) warms up for its ring too (capped), so it never takes
        // over before its IR's peak, with its tail missing, or with an
        // edited stage's start transient still ringing through it.
        let budget = handover_budget(self.period_frames, self.active_rate);
        let ring = if new_chain.inherit_filter_state(&mut self.active_chain, budget) {
            0
        } else {
            new_chain
                .conv_ring_frames()
                .min(warm_ring_cap(self.active_rate, actual_ratio))
        };
        // With a convolution, the fresh EQ sections settle too before the
        // new chain is heard (beside the ring: both from now), capped.
        let out_rate = actual_ratio
            .output_rate(self.active_rate)
            .unwrap_or(self.active_rate);
        let ring = ring.max(new_chain.settle_out_frames(out_rate));
        // A chain of the other kind always warms up: a chain that reports no
        // latency (a short rational minimum-phase kernel whose peak rounds
        // to frame 0) still needs its filter length to settle.
        let target_frames = if new_lat == 0 && ring == 0 && same_kind {
            0
        } else {
            new_lat + ring + WARMUP_SETTLE_MARGIN_FRAMES
        };

        if target_frames == 0 {
            self.promote_warmed_chain(new_chain, generation, swap_seq, crossfade);
        } else {
            new_chain.set_limiter_audible(false);
            self.swap_phase = SwapPhase::Warming {
                new_chain,
                generation,
                swap_seq,
                target_frames,
                warmed_frames: 0,
                crossfade,
            };
        }
    }

    /// Ready a chain to join the stream: its frame grid, the latest A/B
    /// side, the outgoing chain's limiter and DC-block state.
    fn prepare_incoming(&self, new_chain: &mut ActiveChain) {
        // A rational chain joins the stream on its frame grid, so it writes
        // the same output counts as the chain it runs next to. It has not
        // run yet: its convolution is clean and the reset clears nothing
        // of it (no clear to budget).
        new_chain.align_to(self.next_pos);
        // Re-apply the latest A/B selection to the chain being installed. A
        // chain built before an `AbSelect` arrived (and still sitting in the
        // swap ring) must not pin the old side.
        let latest_side = match self.ab_side.load(Ordering::Acquire) {
            1 => AbSide::B,
            _ => AbSide::A,
        };
        new_chain.apply_ab_side(latest_side);
        new_chain.inherit_limiter_headroom(&self.active_chain);
        new_chain.inherit_dc_block_state(&self.active_chain);
    }

    /// A swap is audible now: its crossfade, its fade through silence, or
    /// an upstream edit's fade at the convolution's input.
    fn chain_swapping(&self) -> bool {
        matches!(
            self.swap_phase,
            SwapPhase::Crossfading { .. }
                | SwapPhase::FadeOutOld { .. }
                | SwapPhase::FadeInNew { .. }
        ) || self.active_chain.upstream_fading()
    }

    /// Make `incoming` the audible chain and return the outgoing one. The
    /// held headroom is inherited again here, with both chains at hand: steps
    /// the outgoing chain took while the incoming one warmed up are kept,
    /// and were already counted by it.
    fn install_chain(&mut self, mut incoming: ActiveChain) -> ActiveChain {
        if matches!(incoming, ActiveChain::Bypass) || incoming.is_resample() != self.dsp_enabled {
            self.hold_outgoing = false;
        }
        incoming.inherit_limiter_headroom(&self.active_chain);
        incoming.inherit_dc_block_state(&self.active_chain);
        incoming.set_limiter_audible(true);
        self.chain_installs = self.chain_installs.wrapping_add(1);
        std::mem::replace(&mut self.active_chain, incoming)
    }

    fn promote_warmed_chain(
        &mut self,
        new_chain: ActiveChain,
        generation: u64,
        swap_seq: u64,
        crossfade: bool,
    ) {
        let old_lat = self.active_chain.latency_out_frames();
        let new_lat = new_chain.latency_out_frames();
        let ratio = new_chain.ratio();

        if !crossfade {
            let old = self.install_chain(new_chain);
            self.set_active_swap(generation, swap_seq);
            self.push_recycle(old);
            self.swap_phase = SwapPhase::Idle;
        } else if old_lat == new_lat {
            let old = self.install_chain(new_chain);
            self.set_active_swap(generation, swap_seq);
            let fade_len = self.fade_len_for(SWAP_CROSSFADE_MS, ratio);
            self.swap_phase = SwapPhase::Crossfading {
                fading_old: old,
                fade_pos: 0,
                fade_len,
            };
        } else {
            // Latencies differ: fade through silence (`FadeOutOld` -> switch
            // at zero -> `FadeInNew`) rather than delay-aligning the chains. A
            // fixed internal latency cannot grow a delay line on RT, and
            // delaying the lower-latency chain would inflate steady-state
            // latency after the swap. Switching at zero avoids comb-filtering
            // at no extra latency or RT allocation.
            let fade_len = self.fade_len_for(SWAP_FADE_HALF_MS, ratio);
            self.swap_phase = SwapPhase::FadeOutOld {
                pending_new: new_chain,
                generation,
                swap_seq,
                fade_pos: 0,
                fade_len,
            };
        }
        self.publish_swap_state();
    }

    fn warm_chain_block(
        chain: &mut ActiveChain,
        dsp_in: DspIn<'_>,
        scratch: &mut [i32],
        channels: usize,
        max_block: usize,
        gain: &mut StereoGainRamp,
    ) -> usize {
        let in_frames = dsp_in.frames(channels);
        let chunk_in_cap = chunk_in_cap(scratch.len(), channels, chain.ratio());
        let mut done_in = 0usize;
        let mut done_out = 0usize;
        while done_in < in_frames {
            let take_in = (in_frames - done_in).min(chunk_in_cap);
            let out_frames = chain.output_frames(take_in);
            let out_samples = out_frames * channels;
            let in_slice = dsp_in.slice(channels, done_in, take_in);
            chain.process_in_with_gain(
                in_slice,
                &mut scratch[..out_samples],
                channels,
                max_block,
                gain,
                None,
            );
            done_in += take_in;
            done_out += out_frames;
        }
        done_out
    }

    fn execute_selftest(&mut self, job: &SelfTestJob) {
        let total = job.input_f32.len();
        if self.selftest_scratch.len() >= total && job.output_format != Some(SampleFormat::S16) {
            let out = &mut self.selftest_scratch[..total];
            f32_to_i32(&job.input_f32, out);
            let h = hash_i32(out);
            job.actual_hash.store(h, Ordering::Release);
        } else {
            let chunk_cap = self.selftest_scratch.len().max(256);
            if job.output_format == Some(SampleFormat::S16) {
                const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
                const PRIME: u64 = 0x0000_0100_0000_01b3;
                let mut hash = OFFSET_BASIS;
                let mut offset = 0;
                while offset < total {
                    let take = (total - offset).min(chunk_cap);
                    let out = &mut self.selftest_scratch[..take];
                    f32_to_i32(&job.input_f32[offset..offset + take], out);
                    for &raw in out.iter() {
                        let truncated = raw & !0xffff_i32;
                        for byte in truncated.to_le_bytes() {
                            hash ^= u64::from(byte);
                            hash = hash.wrapping_mul(PRIME);
                        }
                    }
                    offset += take;
                }
                job.actual_hash.store(hash, Ordering::Release);
            } else {
                const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
                const PRIME: u64 = 0x0000_0100_0000_01b3;
                let mut hash = OFFSET_BASIS;
                let mut offset = 0;
                while offset < total {
                    let take = (total - offset).min(chunk_cap);
                    let out = &mut self.selftest_scratch[..take];
                    f32_to_i32(&job.input_f32[offset..offset + take], out);
                    for &s in out.iter() {
                        for byte in s.to_le_bytes() {
                            hash ^= u64::from(byte);
                            hash = hash.wrapping_mul(PRIME);
                        }
                    }
                    offset += take;
                }
                job.actual_hash.store(hash, Ordering::Release);
            }
        }
        job.actual_frames
            .store((total / 2) as u64, Ordering::Release);
        job.done.store(true, Ordering::Release);
    }
    /// The block after the stage (if it ran): `input` is what the chains
    /// get, `input_tap` the same block interleaved (for the input meter
    /// and the pre spectrum; the source itself when no chain runs).
    #[allow(clippy::too_many_arguments)]
    fn process_block_in(
        &mut self,
        active_sink: SinkChoice,
        input: DspIn<'_>,
        input_tap: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: RateRatio,
        graph_rate: u32,
        in_pos: u64,
    ) {
        self.active_rate = graph_rate.max(8_000);
        self.sync_volume_state(out_rate(self.active_rate, ratio), true);
        let in_frames = input.frames(channels);
        self.next_pos = in_pos + in_frames as u64;
        self.flush_recycle_stash();
        self.meters.record_input_peaks(input_tap, channels);

        self.running_ratio = ratio;
        let mode = self.mode(active_sink, ratio);
        let bypass = mode == PlayMode::Bypass;
        // Direct or DSP off away from x1 without the resample-only chain
        // (the engine bypass resampler off, or its chain not there yet):
        // nothing can play the source unchanged there, so silence, reported
        // (`MeterBank::bypass_muted`).
        let muted = mode == PlayMode::ResampleOnly && !self.plays_active(active_sink, ratio);
        self.meters.set_bypass_muted(muted);
        if muted {
            out.fill(0);
            self.meters
                .record_i32_output(out, channels, ClipStats::default());
            self.tap_spectrum_i32(input_tap, out, graph_rate, ratio);
            return;
        }
        if bypass {
            // Only at x1 (`chain_runs`).
            let src = input_tap;
            match ratio.as_integer() {
                // Bit-perfect: at most the mute.
                Some(r) if self.dsp_gain.is_unity_settled() => {
                    self.direct_mute.process_f32_to_i32(src, out, channels, r);
                }
                Some(r) => {
                    // The hardware does not carry the whole volume: these
                    // paths must not play louder than requested, so they get
                    // the same digital gain (which includes a digital mute).
                    self.direct_mute.set_muted(
                        self.volume_target.muted && !self.volume_target.hardware_mute,
                        false,
                    );
                    let bits = self.dac_bits.load(Ordering::Relaxed);
                    self.dsp_gain.process_f32_to_i32_repeat(
                        src,
                        out,
                        channels,
                        r,
                        Some((&mut self.direct_dither, bits)),
                    );
                }
                // Not reached: bypass plays only at x1.
                None => out.fill(0),
            }
            self.meters
                .record_i32_output(out, channels, ClipStats::default());
            self.tap_spectrum_i32(src, out, graph_rate, ratio);
            return;
        }

        // The chain hears the sink that plays (Direct for the resample-only
        // chain), after the stage when it ran.
        let dsp_in = input;
        // Guard against any chain/ALSA transition mismatch cleanly: never
        // emit unprocessed audio, or a chain built for another ratio or
        // graph rate; mute output until a chain that fits is installed.
        if !self.plays_active(active_sink, ratio) {
            out.fill(0);
            self.meters
                .record_i32_output(out, channels, ClipStats::default());
            self.tap_spectrum_i32(input_tap, out, graph_rate, ratio);
            return;
        }

        let gain_before = self.dsp_gain;
        let spectrum_on = self.spectrum.as_ref().is_some_and(SpectrumRt::is_enabled);
        let tap_len = out.len().min(self.post_scratch.len());
        let post_tap = if spectrum_on {
            Some(&mut self.post_scratch[..tap_len])
        } else {
            None
        };
        self.active_chain.align_to(in_pos);
        self.active_chain
            .set_ab_measure_epoch(self.meters.ab_measure_epoch.load(Ordering::Acquire));
        // The harmonics live view: one flag read; copies only while watched.
        let live_on = self
            .harmonics_live
            .as_ref()
            .is_some_and(HarmonicsLiveRt::is_enabled);
        self.active_chain.set_harmonics_tap(live_on);
        let live_rate = out_rate(graph_rate, ratio);
        let live = self
            .harmonics_live
            .as_mut()
            .filter(|_| live_on)
            .map(|l| (l, live_rate));
        let stats = self.active_chain.process_in_with_gain_live(
            dsp_in,
            out,
            channels,
            self.max_block,
            &mut self.dsp_gain,
            post_tap,
            live,
        );
        self.active_chain.drain_ab_hops(&self.meters);

        let phase = std::mem::replace(&mut self.swap_phase, SwapPhase::Idle);
        match phase {
            SwapPhase::Idle => {}
            SwapPhase::Warming {
                mut new_chain,
                generation,
                swap_seq,
                target_frames,
                mut warmed_frames,
                crossfade,
            } => {
                if new_chain.ratio() != ratio {
                    self.push_recycle(new_chain);
                } else {
                    let mut target_frames = target_frames;
                    if new_chain.align_to(in_pos) {
                        // Off the grid after all (not reached: it joined on
                        // it): its state, a handed-over convolution's too,
                        // is gone; it warms up from here as a cold chain.
                        target_frames = target_frames.max(
                            warmed_frames
                                + new_chain.cold_warm_frames(
                                    warm_ring_cap(self.active_rate, ratio),
                                    WARMUP_SETTLE_MARGIN_FRAMES,
                                ),
                        );
                    }
                    let mut warm_gain = gain_before;
                    warmed_frames += Self::warm_chain_block(
                        &mut new_chain,
                        dsp_in,
                        &mut self.swap_scratch,
                        channels,
                        self.max_block,
                        &mut warm_gain,
                    );
                    if warmed_frames >= target_frames {
                        self.promote_warmed_chain(new_chain, generation, swap_seq, crossfade);
                    } else {
                        self.swap_phase = SwapPhase::Warming {
                            new_chain,
                            generation,
                            swap_seq,
                            target_frames,
                            warmed_frames,
                            crossfade,
                        };
                    }
                }
            }
            SwapPhase::Crossfading {
                mut fading_old,
                mut fade_pos,
                fade_len,
            } => {
                if fading_old.ratio() != ratio {
                    self.push_recycle(fading_old);
                } else {
                    fading_old.align_to(in_pos);
                    let mut old_gain = gain_before;
                    let total_in_frames = dsp_in.frames(channels);
                    let chunk_in_cap = chunk_in_cap(self.swap_scratch.len(), channels, ratio);
                    let mut done_in = 0usize;
                    let mut done_out = 0usize;
                    while done_in < total_in_frames {
                        let take_in = (total_in_frames - done_in).min(chunk_in_cap);
                        let out_frames = fading_old.output_frames(take_in);
                        let out_samples = out_frames * channels;
                        let in_slice = dsp_in.slice(channels, done_in, take_in);
                        let out_slice =
                            &mut out[done_out * channels..(done_out + out_frames) * channels];
                        let old_buf = &mut self.swap_scratch[..out_samples];
                        fading_old.process_in_with_gain(
                            in_slice,
                            old_buf,
                            channels,
                            self.max_block,
                            &mut old_gain,
                            None,
                        );

                        for f in 0..out_frames {
                            let w_new = if fade_pos < fade_len {
                                fade_pos += 1;
                                let t = (fade_pos as f64) / (fade_len as f64);
                                0.5 - 0.5 * (PI * t).cos()
                            } else {
                                1.0
                            };
                            let w_old = 1.0 - w_new;
                            let base = f * channels;
                            for ch in 0..channels {
                                let blended = (old_buf[base + ch] as f64) * w_old
                                    + (out_slice[base + ch] as f64) * w_new;
                                out_slice[base + ch] =
                                    blended.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                            }
                        }
                        done_in += take_in;
                        done_out += out_frames;
                    }
                    if fade_pos < fade_len {
                        self.swap_phase = SwapPhase::Crossfading {
                            fading_old,
                            fade_pos,
                            fade_len,
                        };
                    } else {
                        self.push_recycle(fading_old);
                    }
                }
            }
            SwapPhase::FadeOutOld {
                mut pending_new,
                generation,
                swap_seq,
                mut fade_pos,
                fade_len,
            } => {
                if pending_new.ratio() != ratio {
                    self.push_recycle(pending_new);
                } else {
                    pending_new.align_to(in_pos);
                    let mut new_gain = gain_before;
                    let total_in_frames = dsp_in.frames(channels);
                    let chunk_in_cap = chunk_in_cap(self.swap_scratch.len(), channels, ratio);
                    let mut done_in = 0usize;
                    let mut done_out = 0usize;
                    let mut switched = false;
                    let mut in_fade_pos = 0usize;
                    while done_in < total_in_frames {
                        let take_in = (total_in_frames - done_in).min(chunk_in_cap);
                        let out_frames = pending_new.output_frames(take_in);
                        let out_samples = out_frames * channels;
                        let in_slice = dsp_in.slice(channels, done_in, take_in);
                        let out_slice =
                            &mut out[done_out * channels..(done_out + out_frames) * channels];
                        let new_buf = &mut self.swap_scratch[..out_samples];
                        pending_new.process_in_with_gain(
                            in_slice,
                            new_buf,
                            channels,
                            self.max_block,
                            &mut new_gain,
                            None,
                        );

                        for f in 0..out_frames {
                            let base = f * channels;
                            if !switched {
                                fade_pos += 1;
                                let t = (fade_pos as f64) / (fade_len as f64);
                                let w_old = 0.5 + 0.5 * (PI * t).cos();
                                for ch in 0..channels {
                                    let scaled = (out_slice[base + ch] as f64) * w_old;
                                    out_slice[base + ch] =
                                        scaled.round().clamp(i32::MIN as f64, i32::MAX as f64)
                                            as i32;
                                }
                                if fade_pos >= fade_len {
                                    switched = true;
                                }
                            } else {
                                let w_new = if in_fade_pos < fade_len {
                                    in_fade_pos += 1;
                                    let t = (in_fade_pos as f64) / (fade_len as f64);
                                    0.5 - 0.5 * (PI * t).cos()
                                } else {
                                    1.0
                                };
                                for ch in 0..channels {
                                    let scaled = (new_buf[base + ch] as f64) * w_new;
                                    out_slice[base + ch] =
                                        scaled.round().clamp(i32::MIN as f64, i32::MAX as f64)
                                            as i32;
                                }
                            }
                        }
                        done_in += take_in;
                        done_out += out_frames;
                    }

                    if switched {
                        let old = self.install_chain(pending_new);
                        self.set_active_swap(generation, swap_seq);
                        self.push_recycle(old);
                        if in_fade_pos < fade_len {
                            self.swap_phase = SwapPhase::FadeInNew {
                                fade_pos: in_fade_pos,
                                fade_len,
                            };
                        }
                    } else {
                        self.swap_phase = SwapPhase::FadeOutOld {
                            pending_new,
                            generation,
                            swap_seq,
                            fade_pos,
                            fade_len,
                        };
                    }
                }
            }
            SwapPhase::FadeInNew {
                mut fade_pos,
                fade_len,
            } => {
                let out_frames = out.len() / channels;
                for f in 0..out_frames {
                    if fade_pos >= fade_len {
                        break;
                    }
                    fade_pos += 1;
                    let t = (fade_pos as f64) / (fade_len as f64);
                    let w_new = 0.5 - 0.5 * (PI * t).cos();
                    let base = f * channels;
                    for ch in 0..channels {
                        let scaled = (out[base + ch] as f64) * w_new;
                        out[base + ch] =
                            scaled.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                    }
                }
                if fade_pos < fade_len {
                    self.swap_phase = SwapPhase::FadeInNew { fade_pos, fade_len };
                }
            }
        }
        self.publish_swap_state();

        self.meters.record_i32_output(out, channels, stats);
        if spectrum_on && let Some(spectrum) = self.spectrum.as_mut() {
            spectrum.push_pre(input_tap, graph_rate);
            spectrum.push_post_f32(&self.post_scratch[..tap_len], out_rate(graph_rate, ratio));
        }
    }
}

/// The output rate of `ratio` from `graph_rate` (the graph rate when it is
/// not a whole number of Hz, which a negotiated ratio never is).
fn out_rate(graph_rate: u32, ratio: RateRatio) -> u32 {
    ratio.output_rate(graph_rate).unwrap_or(graph_rate)
}

/// Largest input block whose output (at most `ceil(n up / down)` frames)
/// fits in `scratch_len` interleaved samples.
fn chunk_in_cap(scratch_len: usize, channels: usize, ratio: RateRatio) -> usize {
    let out_frames = scratch_len / channels.max(1);
    if ratio.is_integer() {
        return (out_frames / ratio.up.max(1) as usize).max(1);
    }
    let n = (out_frames as u64 * u64::from(ratio.down)) / u64::from(ratio.up);
    (n.saturating_sub(1) as usize).max(1)
}

impl DaemonProcessor {
    /// A block through varispeed's stage (or none): the tee takes the
    /// source frames, the alignment delays them, then the stage runs.
    /// Returns the stop that ended inside the call.
    #[allow(clippy::too_many_arguments)]
    fn process_staged(
        &mut self,
        active_sink: SinkChoice,
        planned: Option<(usize, usize)>,
        full: bool,
        inject: bool,
        dsp_in: &[f32],
        direct_in: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: RateRatio,
        graph_rate: u32,
        in_pos: u64,
    ) -> Option<StopKind> {
        let ch = channels.max(1);
        let src_frames = dsp_in.len() / ch;
        let hooks = self.multi_device_hooks();
        let index = if hooks {
            self.tee.begin_block(src_frames)
        } else {
            0
        };
        let teeing = hooks && self.tee.is_active();
        if teeing && full {
            self.tee.write(index, dsp_in, channels);
        }
        if hooks && self.tee.has_delay() {
            // The alignment cap is in playback time: at the slowest speed
            // the stage plays until it settles.
            let (slowest, fastest, ramping) = match (planned, full) {
                (Some(_), true) => self
                    .vs
                    .tee_view()
                    .map_or((1.0, 1.0, false), |v| (v.slowest, v.fastest, v.ramping)),
                _ => (1.0, 1.0, false),
            };
            self.tee.bound_speed(slowest, ramping, src_frames);
            self.tee.note_fastest(fastest);
        }
        // (Read before the line is taken out: the tee reads 0 without it.)
        let delay_frames = self.tee.delay_frames();
        let mut delay = if hooks { self.tee.take_delay() } else { None };
        let dsp_in = match delay.as_deref_mut() {
            Some(d) => d.process(dsp_in, channels, full),
            None => dsp_in,
        };
        let v0 = if teeing { self.vs.tee_view() } else { None };
        let mut bufs = self.vs.take_buffers();
        let ran = match planned {
            Some(p) if full => self.vs.run(p, dsp_in, channels, &mut bufs),
            _ => None,
        };
        if ran.is_none() && planned.is_none() {
            self.vs
                .note_block_without_stage(active_sink == SinkChoice::Dsp);
        }
        // A stop ended inside this call: a deferred DSP toggle applies to
        // the next call already (the driver ends the call at the fade).
        let stopped = self.vs.take_stopped();
        match (planned, ran) {
            (_, Some(n)) => {
                let input = DspIn::planar(&bufs.out, n);
                let tap = &bufs.tap[..n * channels];
                self.process_block_in(
                    active_sink,
                    input,
                    tap,
                    out,
                    channels,
                    ratio,
                    graph_rate,
                    in_pos,
                );
            }
            (Some((n, _)), None) => {
                // A plan the stage could not run (not reached): the block
                // plays nothing rather than source frames at a wrong count.
                self.vs.note_refused_plan();
                self.vs.note_block_without_stage(false);
                self.next_pos = in_pos + n as u64;
                out.fill(0);
                self.meters
                    .record_i32_output(out, channels, ClipStats::default());
            }
            (None, None) => {
                let tap = if active_sink == SinkChoice::Direct {
                    direct_in
                } else {
                    dsp_in
                };
                // `active`: the file at the chain's input, by the source
                // index the delayed block carries.
                let mut planar = Vec::new();
                let first = index.saturating_sub(delay_frames as u64);
                let injected = if inject {
                    planar = std::mem::take(&mut self.measure.planar);
                    self.measure.active_input(tap, first, &mut planar)
                } else {
                    None
                };
                let input = match injected {
                    Some(exact) => DspIn::planar_with(&planar, tap.len() / ch, exact),
                    None => DspIn::F32(tap),
                };
                self.process_block_in(
                    active_sink,
                    input,
                    tap,
                    out,
                    channels,
                    ratio,
                    graph_rate,
                    in_pos,
                );
                if !planar.is_empty() {
                    self.measure.planar = planar;
                }
                if inject {
                    // `bypass`: the file after the chain, with the
                    // master's digital volume; `active` without a
                    // limiter: the whole output at the ceiling.
                    let unity = self.dsp_gain.is_unity_settled();
                    let gains = self.dsp_gain.current();
                    let bits = self.dac_bits.load(Ordering::Relaxed);
                    self.measure.bypass_output(out, gains, unity, bits);
                    // `active`: the keepalive after the chain, on the word
                    // the chain wrote.
                    let word = bits.min(self.active_chain.output_bits());
                    self.measure.post_keepalive(out, word);
                    self.measure.clamp_output(out);
                }
            }
        }
        self.vs.put_buffers(bufs);
        self.tee.put_delay(delay);
        if teeing {
            self.track_master(
                full,
                planned.is_some(),
                index,
                src_frames,
                v0,
                ran.is_some(),
                out.len() / ch,
                ratio,
            );
        }
        // Commands a plan placed go to the log above, or nowhere.
        if hooks {
            self.vs.drain_issued(|_, _| {});
        }
        stopped
    }

    /// A block through the stretch (Keep pitch): it runs on the source
    /// first, and the tee, the alignment delay, the map and the log all take its
    /// output, at nominal speed (followers replay no command: their stages only
    /// correct their clocks). The chains get its planar output, or the delayed
    /// `f32` frames while a follower's alignment runs.
    #[allow(clippy::too_many_arguments)]
    fn process_stretched(
        &mut self,
        active_sink: SinkChoice,
        planned: (usize, usize),
        dsp_in: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: RateRatio,
        graph_rate: u32,
        in_pos: u64,
    ) -> Option<StopKind> {
        let ch = channels.max(1);
        let hooks = self.multi_device_hooks();
        let mut bufs = self.vs.take_buffers();
        let ran = self.vs.run(planned, dsp_in, channels, &mut bufs);
        let stopped = self.vs.take_stopped();
        let n = ran.unwrap_or(planned.0);
        let index = if hooks { self.tee.begin_block(n) } else { 0 };
        let teeing = hooks && self.tee.is_active();
        match ran {
            Some(n) => {
                let tap = &bufs.tap[..n * channels];
                if teeing {
                    self.tee.write(index, tap, channels);
                }
                if hooks && self.tee.has_delay() {
                    self.tee.bound_speed(1.0, false, n);
                }
                let mut delay = if hooks { self.tee.take_delay() } else { None };
                match delay.as_deref_mut() {
                    Some(d) => {
                        let x = d.process(tap, channels, true);
                        self.process_block_in(
                            active_sink,
                            DspIn::F32(x),
                            x,
                            out,
                            channels,
                            ratio,
                            graph_rate,
                            in_pos,
                        );
                    }
                    None => self.process_block_in(
                        active_sink,
                        DspIn::planar(&bufs.out, n),
                        tap,
                        out,
                        channels,
                        ratio,
                        graph_rate,
                        in_pos,
                    ),
                }
                self.tee.put_delay(delay);
            }
            None => {
                // Not reached (see `process_staged`).
                self.vs.note_refused_plan();
                self.vs.note_block_without_stage(false);
                self.next_pos = in_pos + n as u64;
                out.fill(0);
                self.meters
                    .record_i32_output(out, channels, ClipStats::default());
            }
        }
        self.vs.put_buffers(bufs);
        if teeing {
            let out_frames = out.len() / ch;
            let chain_graph = self.active_chain.latency_out_frames() as f64 / ratio.as_f64();
            self.tee.note_chain_latency(chain_graph);
            match ran {
                Some(n) => {
                    let first = self.tee.input_position(index);
                    self.tee.plain_block(first, first + n as i128 * FRAME);
                    let span = self.tee.plain_span(index, n, chain_graph);
                    self.tee
                        .note_block(Some(span), out_frames, (1.0, 1.0), false);
                }
                None => self.tee.note_block(None, out_frames, (1.0, 1.0), false),
            }
        }
        if hooks {
            self.vs.drain_issued(|_, _| {});
        }
        stopped
    }

    /// The output path's DSP delay, output frames: the chain and the stage,
    /// and the multi-device alignment ahead of them (`0, 0` on Direct or
    /// bypass).
    fn latency_parts(&self, active_sink: SinkChoice) -> (usize, usize) {
        if !self.chain_runs(active_sink, self.running_ratio) {
            (0, 0)
        } else {
            // The stage's delay (nominal frames) counts at the output rate.
            let stage = if self.mode(active_sink, self.running_ratio) == PlayMode::Full {
                self.vs
                    .latency_frames(self.active_rate, self.vs_channels())
                    .map_or(0, |l| (l * self.running_ratio.as_f64()).round() as usize)
            } else {
                0
            };
            // The alignment delay (multi-device), source frames ahead of
            // the stage: at its speed, at the output rate.
            let align = self.tee.delay_frames();
            let align = if align > 0 && self.mode(active_sink, self.running_ratio) == PlayMode::Full
            {
                let s = if stage > 0 {
                    self.vs.tee_view().map_or(1.0, |v| v.speed)
                } else {
                    1.0
                };
                (align as f64 / s * self.running_ratio.as_f64()).round() as usize
            } else {
                0
            };
            (self.active_chain.latency_out_frames() + stage, align)
        }
    }
}

impl EngineAudioProcessor for DaemonProcessor {
    fn running_generation(&self) -> u64 {
        self.newest_running_generation()
    }

    fn poll_updates(
        &mut self,
        active_sink: SinkChoice,
        graph_rate: u32,
        actual_ratio: RateRatio,
        is_streaming: bool,
    ) {
        self.active_rate = graph_rate.max(8_000);
        self.running_ratio = actual_ratio;
        if actual_ratio.is_unity() {
            // The device is at x1 again (a reopen landed): bypass plays.
            self.meters.set_bypass_muted(false);
        }
        self.sync_volume_state(out_rate(self.active_rate, actual_ratio), is_streaming);
        self.flush_recycle_stash();

        // Always drain lightweight control commands even when swaps are paused.
        // A DSP toggle is decided with the swaps below (latest wins).
        let mut toggle = None;
        while let Ok(cmd) = self.cmd_rx.pop() {
            match cmd {
                RtDspCmd::AbSelect { side, crossfade_ms } => {
                    let s = match side {
                        AbSide::A => Side::A,
                        AbSide::B => Side::B,
                    };
                    let out_rate = f64::from(out_rate(self.active_rate, self.active_chain.ratio()));
                    let frames = ((crossfade_ms.max(0.0) / 1000.0) * out_rate).round() as usize;
                    self.active_chain.ab_select(s, frames);
                    match &mut self.swap_phase {
                        SwapPhase::Idle => {}
                        SwapPhase::Warming { new_chain, .. } => new_chain.ab_select(s, frames),
                        SwapPhase::Crossfading { fading_old, .. } => {
                            fading_old.ab_select(s, frames)
                        }
                        SwapPhase::FadeOutOld { pending_new, .. } => {
                            pending_new.ab_select(s, frames)
                        }
                        SwapPhase::FadeInNew { .. } => {}
                    }
                    self.ab_side.store(
                        match side {
                            AbSide::A => 0,
                            AbSide::B => 1,
                        },
                        Ordering::Release,
                    );
                }
                RtDspCmd::AbLevel { offset_db } => {
                    // Every pair that may play: the audible one and any
                    // in a swap with it.
                    self.active_chain.ab_set_level_offset(offset_db);
                    match &mut self.swap_phase {
                        SwapPhase::Idle | SwapPhase::FadeInNew { .. } => {}
                        SwapPhase::Warming { new_chain, .. } => {
                            new_chain.ab_set_level_offset(offset_db)
                        }
                        SwapPhase::Crossfading { fading_old, .. } => {
                            fading_old.ab_set_level_offset(offset_db)
                        }
                        SwapPhase::FadeOutOld { pending_new, .. } => {
                            pending_new.ab_set_level_offset(offset_db)
                        }
                    }
                    // A pair taken off the queue but not playing yet.
                    if let Some(held) = &mut self.held_swap {
                        held.chain.ab_set_level_offset(offset_db);
                    }
                }
                RtDspCmd::SetDspEnabled(en) => toggle = Some((en, false)),
                RtDspCmd::SetDspEnabledHolding(en) => toggle = Some((en, true)),
                RtDspCmd::RunSelfTest(job) => {
                    self.execute_selftest(&job);
                }
            }
        }

        self.measure.poll();
        // Varispeed stages (install, retire) before the chains.
        let full = self.mode(active_sink, actual_ratio) == PlayMode::Full;
        self.vs.poll_cmds(is_streaming, full);
        // The tee: followers' rings, the alignment. Nothing streams (a
        // prime, a pause): the master's map restarts.
        if self.multi_device_hooks() {
            self.tee.poll_cmds(self.active_rate);
            self.vs.set_command_lead(self.tee.command_lead_frames());
            self.vs.set_followed(self.tee.is_active());
            // After the stretch the alignment delay holds this much of its
            // output: a stop of the stretch lets it play out first.
            self.vs.set_post_delay(self.tee.post_delay_frames());
        }
        if !is_streaming && self.tee.is_active() {
            self.tee.note_block(None, 0, (1.0, 1.0), false);
        }

        // Never allow a swap/fade phase to remain stuck when `process_block`
        // is paused, in passthrough, or running a different ratio.
        self.clear_unadvanceable_swap_phase(active_sink, actual_ratio, is_streaming);

        if !self.swap_drain_paused.load(Ordering::Acquire) {
            // If a newer swap arrived while an older chain is still warming up,
            // abort the obsolete warm-up so the latest config takes effect immediately.
            if matches!(self.swap_phase, SwapPhase::Warming { .. })
                && (!self.swap_rx.is_empty() || self.held_swap.is_some())
                && self.recycle_capacity() >= 2
            {
                let phase = std::mem::replace(&mut self.swap_phase, SwapPhase::Idle);
                if let SwapPhase::Warming { new_chain, .. } = phase {
                    self.push_recycle(new_chain);
                }
            }
            // Coalesce first ("latest wins"), then decide whether the
            // winner needs a fade.
            self.take_swaps();
        }
        if !is_streaming {
            // Nothing plays: a fade has nothing to cover, so the toggle
            // applies now (before a resume's readiness check).
            self.complete_transition(false, actual_ratio);
        } else if matches!(self.transition, Transition::FadingOut { out: None, .. })
            && !self.vs.stopping()
        {
            // The stage's stop ended without its fade (a reset of the
            // port, its port gone).
            self.complete_transition(true, actual_ratio);
        }
        self.plan_transition(toggle, active_sink, actual_ratio, is_streaming);
        if !self.swap_drain_paused.load(Ordering::Acquire)
            && !matches!(self.transition, Transition::FadingOut { .. })
            && self.can_accept_swap()
            && !(is_streaming && self.active_chain.upstream_fading())
            && let Some(swap) = self.held_swap.take()
        {
            self.begin_swap(
                swap.chain,
                swap.generation,
                swap.swap_seq,
                swap.crossfade,
                active_sink,
                actual_ratio,
                is_streaming,
            );
        }
        self.publish_swap_state();
    }

    fn process_block(
        &mut self,
        active_sink: SinkChoice,
        dsp_in: &[f32],
        direct_in: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: RateRatio,
        graph_rate: u32,
        in_pos: u64,
    ) {
        // Varispeed: a planned block holds the stage's source frames; the
        // stage runs first, once, and every chain gets its planar output.
        // The taps read what the chains hear.
        self.period_frames = dsp_in.len() / channels.max(1);
        let planned = self.vs.take_planned();
        let full = self.mode(active_sink, ratio) == PlayMode::Full;
        // Multi-device: the block's source frames go to the followers'
        // rings, then through the alignment delay, before the stage. With
        // no follower and no delay line, `dsp_in` passes untouched.
        let ch = channels.max(1);
        let dsp_in = match planned {
            Some((_, n_src)) => &dsp_in[..(n_src * ch).min(dsp_in.len())],
            None => dsp_in,
        };
        let hooks = self.multi_device_hooks();
        // A measurement: arm or check it, and mute the other streams
        // before the tee (the followers hear the mute too), on either
        // branch (Keep pitch faults it here).
        let measuring = hooks && !self.measure.idle();
        if hooks {
            self.tee.set_measuring(measuring);
        }
        let mut inject = false;
        let mut mute_buf = Vec::new();
        if measuring {
            inject = self.measure.begin_block(BlockCtx {
                full,
                sink: active_sink,
                staged: planned.is_some() || self.vs.is_stretch(),
                ratio,
                graph_rate,
                index: self.tee.next_index(),
                frames: dsp_in.len() / ch,
                delay: self.tee.delay_frames(),
                delay_fading: self.tee.delay_fading(),
                chain_latency: self.active_chain.latency_out_frames(),
                chain_tail: self.active_chain.tail_out_frames(),
                chain_installs: self.chain_installs,
                chain_swapping: self.chain_swapping(),
                late_blocks: self.active_chain.worker_late_blocks(),
            });
            mute_buf = std::mem::take(&mut self.measure.mute_buf);
        }
        let muted_len = if measuring && self.measure.mute_source(dsp_in, &mut mute_buf) {
            dsp_in.len()
        } else {
            0
        };
        let dsp_in: &[f32] = if muted_len > 0 {
            &mute_buf[..muted_len]
        } else {
            dsp_in
        };
        let stopped = match planned {
            // Keep pitch: the stretch runs first, and the tee takes its
            // output.
            Some(p) if full && self.vs.is_stretch() => self.process_stretched(
                active_sink,
                p,
                dsp_in,
                out,
                channels,
                ratio,
                graph_rate,
                in_pos,
            ),
            _ => self.process_staged(
                active_sink,
                planned,
                full,
                inject,
                dsp_in,
                direct_in,
                out,
                channels,
                ratio,
                graph_rate,
                in_pos,
            ),
        };
        self.measure.end_block(out.len() / ch);
        if measuring {
            self.measure.mute_buf = mute_buf;
        }
        if hooks && self.vs.take_switched() {
            // The stage kind changed (Keep pitch): the tee's frames are
            // source frames or stretched ones, so its map, its epoch and
            // the alignment's history restart, and followers re-sync.
            self.tee.new_stream();
        }
        let channels = channels.max(1);
        match &mut self.transition {
            Transition::FadingOut { out: Some(f), .. } => {
                if f.apply_out(out, channels) {
                    // Silence at this block's end: switch for the next one.
                    self.complete_transition(true, ratio);
                }
            }
            Transition::FadingIn(f) => {
                if f.apply_in(out, channels) {
                    self.transition = Transition::Idle;
                }
            }
            _ => {}
        }
        if stopped.is_some() {
            // The stage stopped under its own fade (inside this call): a
            // toggle waiting for it applies to the next call already (the
            // driver ends the call at the fade), and the raw block fades in.
            if matches!(self.transition, Transition::FadingOut { out: None, .. }) {
                self.complete_transition(true, ratio);
            } else if !matches!(self.transition, Transition::FadingOut { .. }) {
                self.start_fade_in(ratio);
            }
        }
        self.vs.publish();
    }

    fn run_selftest_direct(&mut self, in_samples: &[f32], out_samples: &mut [i32]) {
        f32_to_i32(in_samples, out_samples);
    }

    fn latency_out_frames(&self, active_sink: SinkChoice) -> usize {
        let (filters, align) = self.latency_parts(active_sink);
        filters + align
    }

    fn align_out_frames(&self, active_sink: SinkChoice) -> usize {
        self.latency_parts(active_sink).1
    }

    fn conv_latency_frames(&self, active_sink: SinkChoice) -> usize {
        if self.chain_runs(active_sink, self.running_ratio) {
            self.active_chain.conv_latency_frames()
        } else {
            0
        }
    }

    fn dac_read(&mut self, delay: i64, time_ns: u64) {
        self.tee.dac_read(delay, time_ns);
        self.measure.dac_read(delay, time_ns);
    }

    fn alignment_frames(&self, active_sink: SinkChoice) -> usize {
        if self.mode(active_sink, self.running_ratio) == PlayMode::Full {
            self.tee.delay_frames()
        } else {
            0
        }
    }

    fn alignment_max_frames(&self, active_sink: SinkChoice) -> usize {
        if self.mode(active_sink, self.running_ratio) == PlayMode::Full {
            self.tee.max_delay_frames()
        } else {
            0
        }
    }

    fn chain_tail_frames(&self, active_sink: SinkChoice) -> usize {
        if !self.chain_runs(active_sink, self.running_ratio) {
            return 0;
        }
        // Output frames back to graph frames, rounded up: the flush feeds
        // whole periods anyway.
        let out = self.active_chain.tail_out_frames();
        (out as f64 / self.running_ratio.as_f64().max(f64::MIN_POSITIVE)).ceil() as usize
    }

    fn stream_prepared(&mut self) {
        self.tee.new_stream();
        self.measure.interrupted();
    }

    fn reset_dsp_state(&mut self) {
        self.settle_swap_phase();
        // A transition ends here (the driver's own fade covers the reset):
        // its toggle applies and a swap it held installs.
        let ratio = self.running_ratio;
        self.complete_transition(false, ratio);
        if self.held_swap.is_some() {
            self.install_held_swap();
        }
        self.active_chain.reset();
        self.vs.reset_all();
        self.tee.reset();
        self.measure.interrupted();
        self.next_pos = 0;
        self.meters.set_bypass_muted(false);
        self.publish_swap_state();
    }

    fn is_chain_ready(
        &self,
        active_sink: SinkChoice,
        actual_ratio: RateRatio,
        required_seq: Option<u64>,
    ) -> bool {
        if !self.chain_runs(active_sink, actual_ratio) {
            return true;
        }
        self.chain_fits(active_sink, actual_ratio)
            && matches!(self.swap_phase, SwapPhase::Idle)
            && required_seq.is_none_or(|req| self.active_swap_seq >= req)
    }

    fn has_chain_for(&self, sink: SinkChoice, ratio: RateRatio) -> bool {
        if !self.chain_runs(sink, ratio) {
            return true;
        }
        let incoming = match &self.swap_phase {
            SwapPhase::Warming { new_chain, .. } => Some(new_chain),
            SwapPhase::FadeOutOld { pending_new, .. } => Some(pending_new),
            _ => None,
        };
        self.chain_fits(sink, ratio) || incoming.is_some_and(|c| self.fits(c, sink, ratio))
    }

    fn varispeed(
        &mut self,
        active_sink: SinkChoice,
        ratio: RateRatio,
        graph_rate: u32,
        channels: usize,
    ) -> Option<&mut dyn VarispeedPort> {
        // The stage feeds the DSP chains only: never Direct or DSP off,
        // never the bypass chain the processor starts with.
        if self.mode(active_sink, ratio) != PlayMode::Full
            || matches!(self.active_chain, ActiveChain::Bypass)
        {
            return None;
        }
        self.vs.port(graph_rate, channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_dsp_core::chain::ChainSpec;
    use audivane_dsp_core::db_to_lin;
    use audivane_protocol::{Band, ChainConfig, EqProfile, FilterPhase, FilterType, RateTarget};

    /// A Custom chain of `taps` output taps at `rate`, upsampling only.
    fn custom_chain(rate: u32, target: RateTarget, taps: usize, phase: FilterPhase) -> ActiveChain {
        ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig {
                upsampler: audivane_dsp_core::resample::ManualConfig {
                    target,
                    taps,
                    phase,
                    ..Default::default()
                }
                .into_upsampler_config()
                .unwrap(),
                ..ChainConfig::neutral()
            },
            sample_rate: rate,
            channels: 2,
            max_block: 512,
            irs: None,
        })))
    }

    /// A streaming DSP processor at `rate` playing `active`, sent `next`
    /// as a crossfading swap: the phase the swap starts in.
    fn start_swap(
        rate: u32,
        ratio: RateRatio,
        active: ActiveChain,
        next: ActiveChain,
    ) -> DaemonProcessor {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            active,
            true,
            2,
            512,
            16,
            rate,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        swap_tx
            .push(PendingSwap {
                chain: next,
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, rate, ratio, true);
        proc
    }

    /// Warm `proc`'s incoming chain to the end: the phase its promotion
    /// starts (equal latencies crossfade, unequal ones fade out and in).
    fn finish_warming(proc: &mut DaemonProcessor) -> &SwapPhase {
        let phase = std::mem::replace(&mut proc.swap_phase, SwapPhase::Idle);
        let SwapPhase::Warming { new_chain, .. } = phase else {
            panic!("the swap does not warm up");
        };
        proc.promote_warmed_chain(new_chain, 1, 1, true);
        &proc.swap_phase
    }

    /// A Custom minimum-phase chain reports its impulse peak, not the block
    /// latency alone at integer ratios nor 0 on a rational grid: the swap
    /// warm-up covers it.
    #[test]
    fn custom_minimum_phase_warm_up_covers_its_peak() {
        let min = FilterPhase::Minimum;
        // x4: block latency 2048 + peak 19.
        let x4 = RateRatio::integer(4);
        let next = custom_chain(48_000, RateTarget::Ratio(4), 4096, min);
        assert_eq!(next.latency_out_frames(), 2067);
        let active = custom_chain(48_000, RateTarget::Ratio(4), 4096, min);
        let proc = start_swap(48_000, x4, active, next);
        let SwapPhase::Warming { target_frames, .. } = proc.swap_phase else {
            panic!("an x4 swap warms up");
        };
        assert_eq!(target_frames, 2067 + WARMUP_SETTLE_MARGIN_FRAMES);
        // Status `latency_out_frames` of the playing chain.
        assert_eq!(proc.latency_out_frames(SinkChoice::Dsp), 2067);

        // 160/147: grid peak 823 / 147 = 5.6, 6 frames, so the swap warms up
        // rather than being promoted at once.
        let r = RateRatio::from_rates(44_100, 48_000);
        let target = RateTarget::Rate(48_000);
        let next = custom_chain(44_100, target, 4096, min);
        assert_eq!(next.latency_out_frames(), 6);
        let active = custom_chain(44_100, target, 4096, min);
        let proc = start_swap(44_100, r, active, next);
        let SwapPhase::Warming { target_frames, .. } = proc.swap_phase else {
            panic!("a rational minimum-phase swap now warms up");
        };
        assert_eq!(target_frames, 6 + WARMUP_SETTLE_MARGIN_FRAMES);
        assert_eq!(proc.latency_out_frames(SinkChoice::Dsp), 6);
    }

    /// The stream-end flush covers the chain's whole latency, in graph
    /// frames rounded up (on a rational grid too).
    #[test]
    fn the_chain_tail_covers_its_latency_in_graph_frames() {
        let min = FilterPhase::Minimum;
        let x4 = RateRatio::integer(4);
        let active = custom_chain(48_000, RateTarget::Ratio(4), 4096, min);
        let next = custom_chain(48_000, RateTarget::Ratio(4), 4096, min);
        let proc = start_swap(48_000, x4, active, next);
        // 2067 output frames at x4.
        assert_eq!(proc.chain_tail_frames(SinkChoice::Dsp), 517);

        // 6 output frames at 160/147: 5.5 graph frames, so 6.
        let r = RateRatio::from_rates(44_100, 48_000);
        let target = RateTarget::Rate(48_000);
        let active = custom_chain(44_100, target, 4096, min);
        let next = custom_chain(44_100, target, 4096, min);
        let proc = start_swap(44_100, r, active, next);
        assert_eq!(proc.chain_tail_frames(SinkChoice::Dsp), 6);
    }

    /// The audio thread meters a running A/B pair for the epoch the
    /// control plane set, hands the hops over through the meter bank, and
    /// stops at epoch 0. A single chain meters nothing.
    #[test]
    fn ab_measure_hops_reach_the_control_plane() {
        let pair = |b_preamp: f64| {
            let b = ChainConfig {
                eq: Some(audivane_protocol::EqProfile {
                    preamp_db: b_preamp,
                    ..Default::default()
                }),
                ..ChainConfig::neutral()
            };
            let spec = |config| ChainSpec {
                config,
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            };
            ActiveChain::Ab(Box::new(
                audivane_dsp_core::chain::AbChain::new(
                    spec(ChainConfig::neutral()),
                    spec(b),
                    audivane_dsp_core::ab_level::AbLevel::fixed(0.0),
                )
                .unwrap(),
            ))
        };
        let meters = MeterBank::new(2);
        let mut proc = DaemonProcessor::new(
            pair(-4.0),
            true,
            2,
            512,
            16,
            48_000,
            Arc::new(AtomicU8::new(0)),
            Arc::clone(&meters),
            rtrb::RingBuffer::new(8).1,
            rtrb::RingBuffer::new(8).1,
            rtrb::RingBuffer::new(8).0,
        );
        let input: Vec<f32> = (0..512 * 2)
            .map(|i| (0.3 * (i as f64 * 0.37).sin()) as f32)
            .collect();
        let mut out = vec![0i32; 512 * 2];
        let mut run = |proc: &mut DaemonProcessor, blocks: usize| {
            for _ in 0..blocks {
                proc.process_block(
                    SinkChoice::Dsp,
                    &input,
                    &input,
                    &mut out,
                    2,
                    RateRatio::ONE,
                    48_000,
                    0,
                );
            }
        };
        let mut hops = Vec::new();
        run(&mut proc, 20);
        meters.take_ab_hops(&mut hops);
        assert!(hops.is_empty(), "nothing runs without a measurement");
        meters.set_ab_measure_epoch(9);
        run(&mut proc, 94); // ~1 s
        meters.take_ab_hops(&mut hops);
        assert_eq!(hops.len(), 10);
        for h in &hops[1..] {
            assert_eq!(h.epoch, 9);
            let db = 10.0 * (h.b / h.a).log10();
            assert!((db + 4.0).abs() < 1e-6, "{db}");
        }
        meters.set_ab_measure_epoch(0);
        run(&mut proc, 20);
        hops.clear();
        meters.take_ab_hops(&mut hops);
        assert!(hops.len() <= 1, "stops on the next block");
    }

    #[test]
    fn ab_overs_accumulate_across_reads_until_reset() {
        let meters = MeterBank::new(2);
        let out = [0i32; 4];
        let stats = ClipStats {
            ab_overs: [0.25, 0.75],
            ..ClipStats::default()
        };
        meters.record_i32_output(&out, 2, stats);
        meters.record_i32_output(&out, 2, stats);
        let _ = meters.read_and_reset(0, 0.0);
        assert_eq!(meters.ab_overs(), [0.5, 1.5]);
        meters.reset_ab_overs();
        assert_eq!(meters.ab_overs(), [0.0, 0.0]);
    }

    #[test]
    fn custom_minimum_phase_swap_alignment_follows_the_peak() {
        let min = FilterPhase::Minimum;
        // (rate, target, ratio, [(taps, latency now); 2]): at x4 the block
        // latency 2048 + peaks 19 and 20 (both 2048 before); at 160/147 the
        // grid peaks round to 5 and 6 frames (both 0 before).
        for (rate, target, ratio, [(short, short_lat), (long, long_lat)]) in [
            (
                48_000,
                RateTarget::Ratio(4),
                RateRatio::integer(4),
                [(4096, 2067), (16_384, 2068)],
            ),
            (
                44_100,
                RateTarget::Rate(48_000),
                RateRatio::from_rates(44_100, 48_000),
                [(1024, 5), (4096, 6)],
            ),
        ] {
            let a = custom_chain(rate, target, short, min);
            let b = custom_chain(rate, target, long, min);
            assert_eq!(a.latency_out_frames(), short_lat, "{ratio}");
            assert_eq!(b.latency_out_frames(), long_lat, "{ratio}");
            let mut proc = start_swap(rate, ratio, a, b);
            assert!(
                matches!(finish_warming(&mut proc), SwapPhase::FadeOutOld { .. }),
                "{ratio}: unequal latencies fade out and in"
            );

            let a = custom_chain(rate, target, short, min);
            let same = custom_chain(rate, target, short, min);
            let mut proc = start_swap(rate, ratio, a, same);
            assert!(
                matches!(finish_warming(&mut proc), SwapPhase::Crossfading { .. }),
                "{ratio}: equal latencies crossfade"
            );
        }
    }

    #[test]
    fn limiter_telemetry_accumulates_events_and_clears_on_bypass() {
        let bank = MeterBank::new(2);
        let stats = |reduction| ClipStats {
            limiter: audivane_protocol::LimiterStatus {
                active: true,
                headroom_db: 3.5,
                gain_reduction_db: reduction,
                events: 2,
            },
            ..Default::default()
        };
        bank.record_i32_output(&[0; 8], 2, stats(4.0));
        bank.record_i32_output(&[0; 8], 2, stats(0.5));
        let limiter = bank.read_and_reset(0, 0.0).limiter.unwrap();
        assert_eq!(limiter.events, 4);
        assert_eq!(limiter.headroom_db, 3.5);
        // Status reads the held headroom without resetting anything.
        assert_eq!(bank.limiter_held_db(), 3.5);
        assert_eq!(bank.limiter_held_db(), 3.5);
        // The largest reduction of the window, not the last block's.
        assert_eq!(limiter.gain_reduction_db, 4.0);
        bank.record_i32_output(&[0; 8], 2, stats(0.5));
        assert_eq!(
            bank.read_and_reset(0, 0.0)
                .limiter
                .unwrap()
                .gain_reduction_db,
            0.5
        );
        bank.record_i32_output(&[0; 8], 2, ClipStats::default());
        let limiter = bank.read_and_reset(0, 0.0).limiter.unwrap();
        assert!(!limiter.active);
        assert_eq!(limiter.headroom_db, 0.0);
        assert_eq!(limiter.gain_reduction_db, 0.0);
        assert_eq!(limiter.events, 6);
        assert_eq!(bank.limiter_held_db(), 0.0);
    }

    #[test]
    fn chain_swap_carries_held_headroom_and_counts_warm_up_events_once() {
        use audivane_protocol::{LimiterConfig, LimiterMode};
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let meters = MeterBank::new(2);
        let make = |preamp_db: f64| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    eq: Some(EqProfile {
                        name: "g".into(),
                        preamp_db,
                        ..Default::default()
                    }),
                    limiter: LimiterConfig {
                        mode: LimiterMode::AutoHeadroom,
                        ..Default::default()
                    },
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            })))
        };
        let mut proc = DaemonProcessor::new(
            make(0.0),
            true,
            2,
            512,
            16,
            48_000,
            Arc::new(AtomicU8::new(0)),
            meters.clone(),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let mut phase = 0.0_f64;
        let mut out = vec![0i32; 1024];
        let mut pos = 0u64;
        let mut play = |proc: &mut DaemonProcessor, blocks: usize, amplitude: f64| {
            for _ in 0..blocks {
                proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);
                let inp: Vec<f32> = (0..1024)
                    .map(|i| {
                        if i % 2 == 0 {
                            phase += 2.0 * PI * 1000.0 / 48_000.0;
                        }
                        (amplitude * phase.sin()) as f32
                    })
                    .collect();
                proc.process_block(
                    SinkChoice::Dsp,
                    &inp,
                    &inp,
                    &mut out,
                    2,
                    RateRatio::ONE,
                    48_000,
                    pos,
                );
                pos += 512;
            }
        };
        let mut swap = |chain, generation| {
            swap_tx
                .push(PendingSwap {
                    chain,
                    crossfade: true,
                    generation,
                    swap_seq: generation,
                })
                .unwrap();
        };
        play(&mut proc, 20, 0.5);
        assert_eq!(proc.active_chain.limiter_headroom_db(), 0.0);
        // The music gets loud right as an EQ edit starts warming up: the
        // outgoing chain steps to 2.5 dB (1.2 needs it) while the new one is
        // frozen. The new one picks that up at promotion, counted once.
        swap(make(-0.1), 1);
        play(&mut proc, 20, 1.2);
        assert_eq!(proc.active_generation(), 1);
        assert_eq!(proc.active_chain.limiter_headroom_db(), 2.5);
        assert_eq!(meters.read_and_reset(0, 0.0).limiter.unwrap().events, 1);
        // An EQ cut alone would need only 1.5 dB: the held 2.5 dB carries
        // over. The warm-up starts mid-signal; its start transient must not
        // raise the held level or count an event.
        swap(make(-1.0), 2);
        play(&mut proc, 20, 1.2);
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_chain.limiter_headroom_db(), 2.5);
        // `events` is a running total.
        assert_eq!(meters.read_and_reset(0, 0.0).limiter.unwrap().events, 1);
        // An EQ boost needs more (5.5 dB): decided by the new chain once it is
        // the audible one, and counted once.
        swap(make(3.0), 3);
        play(&mut proc, 20, 1.2);
        assert_eq!(proc.active_generation(), 3);
        assert_eq!(proc.active_chain.limiter_headroom_db(), 5.5);
        let limiter = meters.read_and_reset(0, 0.0).limiter.unwrap();
        assert_eq!((limiter.events, limiter.headroom_db), (2, 5.5));
    }

    #[test]
    fn meters_use_float_peaks_and_report_the_chain_before_volume() {
        let meters = MeterBank::new(2);
        // The integer output saturates at full scale; the float peak before
        // the quantizer was 1.5 (+3.5 dBFS) after a -10 dB master volume.
        let out = vec![i32::MAX, 0, i32::MAX, 0];
        let mut stats = ClipStats {
            clipped: 2,
            peak: 1.5,
            clipped_ch: [2, 0, 0, 0, 0, 0, 0, 0],
            measured: true,
            ..ClipStats::default()
        };
        stats.peak_ch[0] = 1.5;
        stats.peak_ch[1] = 0.1;
        stats.chain_peak_ch[0] = 1.5 * 10.0_f64.powf(10.0 / 20.0);
        stats.chain_peak_ch[1] = 0.1 * 10.0_f64.powf(10.0 / 20.0);
        meters.record_i32_output(&out, 2, stats);
        let m = meters.read_and_reset(0, 0.0);
        assert!((m.peak_db[0] - 3.52).abs() < 0.05, "{}", m.peak_db[0]);
        assert!((m.peak_db[1] - (-20.0)).abs() < 0.05, "{}", m.peak_db[1]);
        assert_eq!(m.chain_peak_db.len(), 2);
        assert!((m.chain_peak_db[0] - 13.52).abs() < 0.05);
        assert!((m.chain_peak_db[0] - m.peak_db[0] - 10.0).abs() < 0.05);
        // Next window without a measured block: no chain peaks.
        meters.record_i32_output(&out, 2, ClipStats::default());
        let m = meters.read_and_reset(0, 0.0);
        assert!(m.chain_peak_db.is_empty());
        assert!(m.peak_db[0].abs() < 0.01, "integer path reads full scale");
    }

    #[test]
    fn meters_preserve_chain_peaks_above_plus_24_dbfs() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let chain = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig {
                eq: Some(EqProfile {
                    name: "hot".into(),
                    preamp_db: 40.0,
                    bands: vec![],
                    ..Default::default()
                }),
                ..ChainConfig::neutral()
            },
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: None,
        })));
        let mut proc = DaemonProcessor::new(
            chain,
            true,
            2,
            512,
            16,
            48_000,
            ab_side,
            Arc::clone(&meters),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let vol = proc.volume_handle();
        vol.store(VolumeTarget {
            volume_db: -40.0,
            balance_db: 0.0,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        });
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), false);

        let inp = vec![0.5_f32; 512 * 2];
        let mut out = vec![0_i32; 512 * 2];
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );

        let m = meters.read_and_reset(0, 0.0);
        let expected_in = 20.0 * 0.5_f64.log10(); // -6.0206 dBFS
        let expected_chain = expected_in + 40.0; // +33.9794 dBFS
        assert!(
            (m.input_peak_db[0] - expected_in).abs() < 0.05,
            "input_peak_db = {}, expected {expected_in}",
            m.input_peak_db[0]
        );
        assert!(
            (m.chain_peak_db[0] - expected_chain).abs() < 0.05,
            "chain_peak_db = {}, expected {expected_chain}",
            m.chain_peak_db[0]
        );
        assert!(
            (m.chain_peak_db[0] - m.input_peak_db[0] - 40.0).abs() < 0.05,
            "chain gain = {}, expected +40.0 dB",
            m.chain_peak_db[0] - m.input_peak_db[0]
        );
        assert!(
            (m.peak_db[0] - expected_in).abs() < 0.05,
            "peak_db = {}, expected {expected_in}",
            m.peak_db[0]
        );
        assert_eq!(m.clipped, 0);
    }

    #[test]
    fn meters_measure_sine_and_reset_window() {
        let meters = MeterBank::new(2);
        let frames = 4800usize;
        let mut out = Vec::with_capacity(frames * 2);
        let amp = 0.5_f64;
        for i in 0..frames {
            let l = ((i as f64 * 0.1).sin() * amp * 2_147_483_648.0).round() as i32;
            let r = 0i32;
            out.push(l);
            out.push(r);
        }
        meters.record_i32_output(
            &out,
            2,
            ClipStats {
                clipped: 2,
                peak: 0.5,
                clipped_ch: [1, 1, 0, 0, 0, 0, 0, 0],
                ..ClipStats::default()
            },
        );
        let mut input = vec![0.0_f32; frames * 2];
        for frame in input.as_chunks_mut::<2>().0 {
            frame[0] = -0.25;
            frame[1] = 0.5;
        }
        meters.record_input_peaks(&input, 2);

        let m1 = meters.read_and_reset(1, 0.12);
        assert_eq!(m1.clipped, 2);
        assert_eq!(m1.clipped_ch, vec![1, 1]);
        assert!(
            (m1.input_peak_db[0] - (-12.04)).abs() < 0.1,
            "got {}",
            m1.input_peak_db[0]
        );
        assert!(
            (m1.input_peak_db[1] - (-6.02)).abs() < 0.1,
            "got {}",
            m1.input_peak_db[1]
        );
        assert_eq!(m1.xruns, 1);
        assert!(
            (m1.peak_db[0] - (-6.02)).abs() < 0.1,
            "got {}",
            m1.peak_db[0]
        );
        assert!(
            (m1.rms_db[0] - (-9.03)).abs() < 0.15,
            "got {}",
            m1.rms_db[0]
        );
        assert!(m1.peak_db[1] <= -150.0);

        let m2 = meters.read_and_reset(1, 0.0);
        assert!(m2.peak_db[0] <= -150.0);
        assert!(m2.rms_db[0] <= -150.0);
        assert!(m2.input_peak_db[0] <= -150.0);
        assert_eq!(m2.clipped, 2);
        assert_eq!(m2.clipped_ch, vec![1, 1]);
    }

    #[test]
    fn processor_swaps_chain_lockfree_and_returns_old_to_recycle() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let chain1 = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig::neutral(),
            sample_rate: 48_000,
            channels: 2,
            max_block: 1024,
            irs: None,
        })));

        let mut proc = DaemonProcessor::new(
            chain1, true, 2, 1024, 16, 48_000, ab_side, meters, cmd_rx, swap_rx, recycle_tx,
        );

        let cfg2 = ChainConfig {
            eq: Some(EqProfile {
                name: "boost".into(),
                preamp_db: 0.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 1000.0,
                    gain_db: 3.0,
                    q: 1.0,
                    enabled: true,
                }],
                ..Default::default()
            }),
            ..ChainConfig::neutral()
        };
        let chain2 = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: cfg2,
            sample_rate: 48_000,
            channels: 2,
            max_block: 1024,
            irs: None,
        })));

        swap_tx
            .push(PendingSwap {
                chain: chain2,
                crossfade: true,
                generation: 2,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), true);
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_swap_seq(), 1);

        let inp = vec![0.1f32; 512 * 2];
        let mut out = vec![0i32; 512 * 2];
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        assert!(
            recycle_rx.pop().is_ok(),
            "old chain must be recycled off-RT"
        );
    }

    #[test]
    fn long_filter_warmup_prevents_silence_gap_on_swap() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let make_4x = |preamp_db: f64| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    eq: Some(EqProfile {
                        name: "g".into(),
                        preamp_db,
                        bands: vec![],
                        ..Default::default()
                    }),
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: RateTarget::Ratio(4),
                        taps: 8192,
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            })))
        };

        let mut proc = DaemonProcessor::new(
            make_4x(0.0),
            true,
            2,
            512,
            16,
            48_000,
            ab_side,
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
        );

        let block_in = 512usize;
        let block_out = block_in * 4;
        let mut phase = 0.0_f64;
        let step = 2.0 * PI * 1000.0 / 48_000.0;
        let mut gen_block = || {
            let mut buf = Vec::with_capacity(block_in * 2);
            for _ in 0..block_in {
                let s = (phase.sin() * 0.5) as f32;
                buf.push(s);
                buf.push(s);
                phase += step;
            }
            buf
        };

        let mut out = vec![0i32; block_out * 2];
        // Pre-warm chain 1 cleanly.
        for _ in 0..16 {
            let inp = gen_block();
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
        }

        // Swap to chain 2 (-3 dB preamp). Old chain keeps serving until new chain warms up.
        swap_tx
            .push(PendingSwap {
                chain: make_4x(-3.0),
                crossfade: true,
                generation: 2,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);

        let mut min_block_rms = f64::MAX;
        let mut seen_warming = false;
        for _ in 0..30 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
            // While chain 2 warms up next to chain 1, it is the one running
            // (an overload demotion is published with it).
            if proc.active_generation() != 2 {
                seen_warming |= proc.running_generation() == 2;
            }
            let inp = gen_block();
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
            let sum_sq: f64 = out
                .iter()
                .step_by(2)
                .map(|&s| {
                    let v = (s as f64) / 2_147_483_648.0;
                    v * v
                })
                .sum();
            let rms = (sum_sq / (block_out as f64)).sqrt();
            min_block_rms = min_block_rms.min(rms);
        }

        assert!(
            min_block_rms > 0.22,
            "expected continuous level without gap during warm-up+crossfade, got min_rms={min_block_rms:.4}"
        );
        assert_eq!(proc.active_generation(), 2);
        assert!(seen_warming, "the warm-up was not counted as running");
        assert_eq!(proc.running_generation(), 2);
        assert!(
            recycle_rx.pop().is_ok(),
            "old chain should be recycled after swap"
        );
    }

    #[test]
    fn mismatched_latency_swap_transitions_through_fade_out_fade_in() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let chain_no_lat = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig::neutral(),
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: None,
        })));
        let chain_with_conv = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig {
                convolution: vec![audivane_protocol::ConvolutionConfig {
                    name: None,
                    enabled: true,
                    ir_path: "unused".into(),
                    gain_db: 0.0,
                    matrix: None,
                    calibration: false,
                }],
                ..ChainConfig::neutral()
            },
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: Some(vec![vec![1.0; 2048]].into()),
        })));
        assert_ne!(
            chain_no_lat.latency_out_frames(),
            chain_with_conv.latency_out_frames()
        );

        let mut proc = DaemonProcessor::new(
            chain_no_lat,
            true,
            2,
            512,
            16,
            48_000,
            ab_side,
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        swap_tx
            .push(PendingSwap {
                chain: chain_with_conv,
                crossfade: true,
                generation: 2,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), true);

        let inp = vec![0.25f32; 512 * 2];
        let mut out = vec![0i32; 512 * 2];
        for _ in 0..10 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), true);
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(1),
                48_000,
                0,
            );
        }
        assert_eq!(proc.active_generation(), 2);
        assert!(recycle_rx.pop().is_ok());
    }

    #[test]
    fn idle_eq_swap_followed_by_4x_reopen_never_deadlocks_and_applies_eq() {
        // Exact sequence from tests/daemon/daemon_ctl_e2e.sh:
        // 1. Engine idle (`is_streaming = false`) with Bypass at 1x.
        // 2. Apply EQ (-3 dB preamp, -2 dB @ 1 kHz PK) -> swap queued & polled while idle.
        // 3. Reopen to 4x upsampling (65536 taps -> high latency) with same EQ -> polled on Resume (`is_streaming = false`).
        // 4. Play -6 dBFS 1 kHz sine -> output level must be -11.0 dBFS, NOT -6.0 dBFS!
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let eq = EqProfile {
            name: "e2e".into(),
            preamp_db: -3.0,
            bands: vec![Band {
                kind: FilterType::Peaking,
                freq_hz: 1000.0,
                gain_db: -2.0,
                q: 2.0,
                enabled: true,
            }],
            ..Default::default()
        };

        let make_chain = |ratio: u32, taps: usize| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    eq: Some(eq.clone()),
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: if ratio <= 1 {
                            RateTarget::Off
                        } else {
                            RateTarget::Ratio(ratio)
                        },
                        taps,
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 1024,
                irs: None,
            })))
        };

        let mut proc = DaemonProcessor::new(
            ActiveChain::Bypass,
            true,
            2,
            1024,
            16,
            48_000,
            ab_side,
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
        );

        // Step 2: EQ set while idle (`is_streaming = false`).
        swap_tx
            .push(PendingSwap {
                chain: make_chain(1, 16),
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), false);
        assert_eq!(proc.active_generation(), 1);
        assert!(matches!(proc.swap_phase, SwapPhase::Idle));

        // Step 3: Upsample 4x reopen (`on_negotiated` pushes 4x EQ chain, `Resume` calls `poll_updates(..., false)`).
        swap_tx
            .push(PendingSwap {
                chain: make_chain(4, 65_536),
                crossfade: false,
                generation: 2,
                swap_seq: 2,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), false);
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_chain.ratio(), RateRatio::integer(4));
        assert!(matches!(proc.swap_phase, SwapPhase::Idle));

        while recycle_rx.pop().is_ok() {}

        // Step 4: Play -6 dBFS 1 kHz sine and measure steady-state RMS/peak.
        let block_in = 1024usize;
        let amp = 10.0_f64.powf(-6.0 / 20.0);
        let step = 2.0 * PI * 1000.0 / 48_000.0;
        let mut phase = 0.0_f64;
        let mut out = vec![0i32; block_in * 4 * 2];
        let mut last_peak = 0.0_f64;
        for b in 0..40 {
            let mut inp = Vec::with_capacity(block_in * 2);
            for _ in 0..block_in {
                let s = (phase.sin() * amp) as f32;
                inp.push(s);
                inp.push(s);
                phase += step;
            }
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
            if b == 39 {
                for &s in out.iter().step_by(2) {
                    last_peak = last_peak.max((s as f64).abs() / 2_147_483_648.0);
                }
            }
        }
        let peak_db = 20.0 * last_peak.log10();
        assert!(
            (peak_db - (-11.0)).abs() < 0.2,
            "expected -11.0 dBFS with EQ+4x active, got {peak_db:.3} dBFS"
        );
    }

    #[test]
    fn rapid_swaps_and_warmup_abort_install_latest_generation() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(16);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let make_4x = |preamp_db: f64| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    eq: Some(EqProfile {
                        name: "g".into(),
                        preamp_db,
                        bands: vec![],
                        ..Default::default()
                    }),
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: RateTarget::Ratio(4),
                        taps: 8192,
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            })))
        };

        let mut proc = DaemonProcessor::new(
            make_4x(0.0),
            true,
            2,
            512,
            16,
            48_000,
            ab_side,
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
        );

        let inp = vec![0.1f32; 512 * 2];
        let mut out = vec![0i32; 512 * 4 * 2];

        // Start warming generation 1.
        swap_tx
            .push(PendingSwap {
                chain: make_4x(-2.0),
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(4),
            48_000,
            0,
        );
        assert!(matches!(proc.swap_phase, SwapPhase::Warming { .. }));

        // Three rapid applies arrive while gen 1 is still warming up: must abort gen 1
        // and coalesce directly to gen 4.
        for (swap_gen, db) in [(2, -3.0), (3, -4.0), (4, -5.0)] {
            swap_tx
                .push(PendingSwap {
                    chain: make_4x(db),
                    crossfade: true,
                    generation: swap_gen,
                    swap_seq: swap_gen,
                })
                .unwrap();
        }
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
        assert!(matches!(
            proc.swap_phase,
            SwapPhase::Warming { generation: 4, .. }
        ));

        // Finish warming + crossfade -> active generation becomes 4.
        for _ in 0..20 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::integer(4),
                48_000,
                0,
            );
        }
        assert_eq!(proc.active_generation(), 4);
        assert_eq!(proc.active_swap_seq(), 4);
        let mut recycled = 0;
        while recycle_rx.pop().is_ok() {
            recycled += 1;
        }
        assert_eq!(recycled, 4);
    }

    #[test]
    fn reset_dsp_state_mid_warmup_and_streaming_crossfade_then_reopen_keep_latest() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(16);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(16);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let make_chain = |ratio: u32, taps: usize| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    eq: Some(EqProfile {
                        name: "eq".into(),
                        preamp_db: -3.0,
                        bands: vec![],
                        ..Default::default()
                    }),
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: if ratio <= 1 {
                            RateTarget::Off
                        } else {
                            RateTarget::Ratio(ratio)
                        },
                        taps,
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            })))
        };

        let mut proc = DaemonProcessor::new(
            ActiveChain::Bypass,
            true,
            2,
            512,
            16,
            48_000,
            ab_side,
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
        );

        // Start a streaming crossfade at 1x, then immediately stop streaming and reopen to 4x
        // before crossfade completes.
        swap_tx
            .push(PendingSwap {
                chain: make_chain(1, 16),
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), true);
        assert!(matches!(proc.swap_phase, SwapPhase::Crossfading { .. }));

        // Reopen delivers 4x chain while processor wasn't given any more 1x blocks.
        swap_tx
            .push(PendingSwap {
                chain: make_chain(4, 8192),
                crossfade: false,
                generation: 2,
                swap_seq: 2,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), false);
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_chain.ratio(), RateRatio::integer(4));
        assert!(matches!(proc.swap_phase, SwapPhase::Idle));

        // Now begin warming gen 3 while streaming, then stop playback (`reset_dsp_state`).
        let inp = vec![0.1f32; 512 * 2];
        let mut out = vec![0i32; 512 * 4 * 2];
        swap_tx
            .push(PendingSwap {
                chain: make_chain(4, 8192),
                crossfade: true,
                generation: 3,
                swap_seq: 3,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), true);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(4),
            48_000,
            0,
        );
        assert!(matches!(proc.swap_phase, SwapPhase::Warming { .. }));

        proc.reset_dsp_state();
        assert_eq!(
            proc.active_generation(),
            3,
            "playback stop mid-warmup must promote the pending chain instead of dropping it"
        );
        assert!(matches!(proc.swap_phase, SwapPhase::Idle));
        while recycle_rx.pop().is_ok() {}
    }

    #[test]
    fn ratio_mismatch_mutes_dsp_output_and_blocks_chain_ready_while_drain_paused() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let make_chain = |ratio: u32, preamp_db: f64| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    eq: Some(EqProfile {
                        name: "eq".into(),
                        preamp_db,
                        bands: vec![],
                        ..Default::default()
                    }),
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: if ratio <= 1 {
                            RateTarget::Off
                        } else {
                            RateTarget::Ratio(ratio)
                        },
                        taps: if ratio <= 1 { 16 } else { 8192 },
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            })))
        };

        let mut proc = DaemonProcessor::new(
            make_chain(1, -3.0),
            true,
            2,
            512,
            16,
            48_000,
            ab_side,
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        // Pause RT swap drain and push a 4x swap with swap_seq = 5.
        proc.set_swap_drain_paused(true);
        swap_tx
            .push(PendingSwap {
                chain: make_chain(4, -6.0),
                crossfade: false,
                generation: 2,
                swap_seq: 5,
            })
            .unwrap();

        // Even after poll_updates, paused RT keeps the old 1x chain; is_chain_ready must be false.
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), false);
        assert!(!proc.is_chain_ready(SinkChoice::Dsp, RateRatio::integer(4), Some(5)));
        assert!(!proc.is_chain_ready(SinkChoice::Dsp, RateRatio::integer(4), None));
        // Direct at x1 bypasses chain readiness: it plays no chain. Away
        // from x1 it waits for the resample-only chain (a DSP chain for the
        // ratio is not it).
        assert!(proc.is_chain_ready(SinkChoice::Direct, RateRatio::ONE, Some(5)));
        assert!(!proc.is_chain_ready(SinkChoice::Direct, RateRatio::integer(4), Some(5)));

        // A block rendered while the 1x chain is active for a 4x ALSA output is
        // muted (all zeros), never un-EQ'd passthrough.
        let inp = vec![0.5f32; 512 * 2];
        let mut out = vec![123_i32; 512 * 4 * 2];
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(4),
            48_000,
            0,
        );
        assert!(
            out.iter().all(|&s| s == 0),
            "mismatched DSP chain ratio must output silence instead of passthrough"
        );

        // Unpause drain -> poll installs swap_seq = 5 and makes chain ready for 4x.
        proc.set_swap_drain_paused(false);
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(4), false);
        assert!(proc.is_chain_ready(SinkChoice::Dsp, RateRatio::integer(4), Some(5)));
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_swap_seq(), 5);
        while recycle_rx.pop().is_ok() {}
    }

    #[test]
    fn direct_and_bypass_get_the_digital_volume_part_and_ramp_mute() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let chain = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig::neutral(),
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: None,
        })));
        let mut proc = DaemonProcessor::new(
            chain, true, 2, 512, 16, 48_000, ab_side, meters, cmd_rx, swap_rx, recycle_tx,
        );
        let vol = proc.volume_handle();

        // Build deterministic 24-bit left-justified samples.
        let frames = 512usize;
        let mut expected_i32 = Vec::with_capacity(frames * 2);
        for f in 0..frames {
            let l = (((f as i32 * 13_337 - 400_000) & 0x007f_ffff) - (1 << 22)) << 8;
            let r = (((-(f as i32) * 21_111 + 250_000) & 0x007f_ffff) - (1 << 22)) << 8;
            expected_i32.push(l);
            expected_i32.push(r);
        }
        let mut inp = vec![0.0_f32; frames * 2];
        audivane_dsp_core::bypass::i32_to_f32(&expected_i32, &mut inp);

        // Unity digital gain (the hardware carries the volume): bit-exact.
        let mut out = vec![0_i32; frames * 2];
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::ONE,
            48_000,
            0,
        );
        assert_eq!(out, expected_i32);
        assert_eq!(hash_i32(&out), hash_i32(&expected_i32));

        // A digital volume part is applied on Direct too: never louder than
        // requested (-18 dB, and -12 dB more on the left from the balance).
        vol.store(VolumeTarget {
            volume_db: -18.0,
            balance_db: 12.0,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        });
        proc.poll_updates(SinkChoice::Direct, 48_000, RateRatio::ONE, false);
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::ONE,
            48_000,
            0,
        );
        let (gl, gr) = (db_to_lin(-30.0), db_to_lin(-18.0));
        for (f, pair) in out.chunks(2).enumerate() {
            let (el, er) = (expected_i32[f * 2], expected_i32[f * 2 + 1]);
            assert!((f64::from(pair[0]) - f64::from(el) * gl).abs() <= 1.0);
            assert!((f64::from(pair[1]) - f64::from(er) * gr).abs() <= 1.0);
        }
        // And with DSP off (bypass) the same.
        proc.set_dsp_enabled_for_test(false);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::ONE,
            48_000,
            0,
        );
        assert!((f64::from(out[1]) - f64::from(expected_i32[1]) * gr).abs() <= 1.0);
        // On a 24-bit DAC the scaled samples are 24-bit codes (dithered),
        // not 32-bit values the DAC would truncate.
        proc.dac_bits_handle().store(24, Ordering::Relaxed);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::ONE,
            48_000,
            0,
        );
        assert!(
            out.iter().all(|&v| v & 0xff == 0),
            "not quantized to 24 bits"
        );
        for (f, pair) in out.chunks(2).enumerate() {
            let er = expected_i32[f * 2 + 1];
            assert!((f64::from(pair[1]) - f64::from(er) * gr).abs() <= 2.0 * 256.0);
        }
        proc.dac_bits_handle().store(32, Ordering::Relaxed);
        proc.set_dsp_enabled_for_test(true);

        // Mute without hardware switch ramps to silence on Direct path.
        vol.store(VolumeTarget {
            volume_db: -18.0,
            balance_db: 12.0,
            muted: true,
            hardware_volume: false,
            hardware_mute: false,
        });
        // First 512-frame block completes the 480-frame ramp; next block is exact zeros.
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        assert!(out[..480 * 2].iter().any(|&s| s != 0));
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        assert!(out.iter().all(|&s| s == 0));

        // Snap unmute at unity gain while idle restores bit-exactness.
        vol.store(VolumeTarget {
            volume_db: 0.0,
            balance_db: 0.0,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        });
        proc.poll_updates(SinkChoice::Direct, 48_000, RateRatio::integer(1), false);
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        assert_eq!(out, expected_i32);
    }

    #[test]
    fn dsp_path_applies_digital_volume_and_balance_and_hardware_stays_bit_exact() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let chain = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig::neutral(),
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: None,
        })));
        let mut proc = DaemonProcessor::new(
            chain, true, 2, 512, 16, 48_000, ab_side, meters, cmd_rx, swap_rx, recycle_tx,
        );
        let vol = proc.volume_handle();

        // Digital -12 dB + balance -6 dB (attenuates right by 6 dB -> L=-12 dB, R=-18 dB).
        vol.store(VolumeTarget {
            volume_db: -12.0,
            balance_db: -6.0,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        });
        let inp = vec![0.5_f32; 512 * 2];
        let mut out = vec![0_i32; 512 * 2];
        // First streaming block completes the 480-frame ramp; second is settled.
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        let l = (out[0] as f64) / 2_147_483_648.0;
        let r = (out[1] as f64) / 2_147_483_648.0;
        let exp_l = 0.5 * 10.0_f64.powf(-12.0 / 20.0);
        let exp_r = 0.5 * 10.0_f64.powf(-18.0 / 20.0);
        assert!((l - exp_l).abs() < 1e-4, "L={l} expected {exp_l}");
        assert!((r - exp_r).abs() < 1e-4, "R={r} expected {exp_r}");

        // Hardware volume & hardware mute with balance == 0 keeps DSP path bit-exact!
        let mut expected_i32 = Vec::with_capacity(512 * 2);
        for f in 0..512 {
            let l_s = (((f * 9_973 - 200_000) & 0x007f_ffff) - (1 << 22)) << 8;
            let r_s = (((-f * 15_485 + 300_000) & 0x007f_ffff) - (1 << 22)) << 8;
            expected_i32.push(l_s);
            expected_i32.push(r_s);
        }
        let mut exact_inp = vec![0.0_f32; 512 * 2];
        audivane_dsp_core::bypass::i32_to_f32(&expected_i32, &mut exact_inp);

        vol.store(VolumeTarget {
            volume_db: -24.0,
            balance_db: 0.0,
            muted: true,
            hardware_volume: true,
            hardware_mute: true,
        });
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::integer(1), false);
        proc.process_block(
            SinkChoice::Dsp,
            &exact_inp,
            &exact_inp,
            &mut out,
            2,
            RateRatio::integer(1),
            48_000,
            0,
        );
        assert_eq!(out, expected_i32);
    }

    /// A chain swapped in mid-stream at a fractional ratio joins the running
    /// frame grid: with variable block sizes, a warmed and crossfaded
    /// replacement produces the same output as a chain that never changed.
    /// The volume and mute ramps (and the Direct/bypass dither hold) run
    /// at the real output rate at a rational ratio, not graph rate x an
    /// integer. DSP off and Direct away from x1 (until their x1 reopen)
    /// write silence and report it until a reset or an update at x1.
    #[test]
    fn volume_stages_run_at_the_rational_output_rate() {
        let ratio = RateRatio::from_rates(44_100, 48_000);
        let build = || {
            Chain::try_new(ChainSpec {
                config: ChainConfig {
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: RateTarget::Rate(48_000),
                        taps: 64,
                        taps_unit: audivane_protocol::TapsUnit::Input,
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 44_100,
                channels: 2,
                max_block: 1024,
                irs: None,
            })
            .unwrap()
        };
        let chain = ActiveChain::Single(Box::new(build()));
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            chain,
            true,
            2,
            1024,
            16,
            44_100,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let inp = vec![0.25_f32; 2 * 441];
        let mut out = vec![1_i32; 2 * ratio.max_frames_out(441)];
        proc.poll_updates(SinkChoice::Dsp, 44_100, ratio, true);
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, ratio, 44_100, 0);
        assert_eq!(proc.dsp_gain.sample_rate(), 48_000);
        proc.set_dsp_enabled_for_test(false);
        out.fill(1);
        let n = ratio.frames_out(441, 441) * 2;
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out[..n],
            2,
            ratio,
            44_100,
            441,
        );
        // DSP off while the device is still at 160/147 (until its x1
        // reopen, or its resample-only chain): the DSP chain stops at once,
        // nothing plays, and it is reported. DSP off away from x1 waits for
        // the resample-only chain, never the DSP one.
        assert!(out[..n].iter().all(|&v| v == 0));
        assert!(proc.meters.bypass_muted());
        assert!(!proc.is_chain_ready(SinkChoice::Dsp, ratio, None));
        // A swap arriving meanwhile neither warms nor fades on that chain.
        swap_tx
            .push(PendingSwap {
                chain: ActiveChain::Single(Box::new(build())),
                crossfade: true,
                generation: 7,
                swap_seq: 7,
            })
            .ok()
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 44_100, ratio, true);
        out[..n].fill(1);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out[..n],
            2,
            ratio,
            44_100,
            882,
        );
        assert!(out[..n].iter().all(|&v| v == 0));
        assert!(matches!(proc.swap_phase, SwapPhase::Idle));
        // At x1 bypass takes over: the samples unchanged.
        proc.poll_updates(SinkChoice::Dsp, 44_100, RateRatio::ONE, true);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out[..2 * 441],
            2,
            RateRatio::ONE,
            44_100,
            1_323,
        );
        assert!(out[..2 * 441].iter().all(|&v| v == 1 << 29), "{}", out[0]);
        assert_eq!(proc.latency_out_frames(SinkChoice::Dsp), 0);
        // Direct away from x1 (only between a sink switch and its x1
        // reopen) never plays a chain: silence, reported, even with a chain
        // for that ratio waiting.
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out[..n],
            2,
            ratio,
            44_100,
            1_323,
        );
        assert!(out[..n].iter().all(|&v| v == 0));
        assert!(proc.meters.bypass_muted());
        swap_tx
            .push(PendingSwap {
                chain: ActiveChain::Single(Box::new(build())),
                crossfade: false,
                generation: 1,
                swap_seq: 1,
            })
            .ok()
            .unwrap();
        proc.poll_updates(SinkChoice::Direct, 44_100, ratio, true);
        out[..n].fill(1);
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out[..n],
            2,
            ratio,
            44_100,
            1_323,
        );
        assert!(out[..n].iter().all(|&v| v == 0));
        assert!(proc.meters.bypass_muted());
        // Any other ratio on Direct: the same.
        let other = RateRatio::from_rates(44_100, 32_000);
        let m = other.frames_out(0, 441) * 2;
        let mut out2 = vec![1_i32; m];
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out2,
            2,
            other,
            44_100,
            0,
        );
        assert!(out2.iter().all(|&v| v == 0));
        assert!(proc.meters.bypass_muted());
        // With DSP on, the DSP sink has no chain for it: not ready.
        proc.set_dsp_enabled_for_test(true);
        assert!(!proc.is_chain_ready(SinkChoice::Dsp, other, None));
        proc.set_dsp_enabled_for_test(false);
        // A reset (playback stopped) or an update at x1 (a reopen landed)
        // clears the report without another block.
        proc.reset_dsp_state();
        assert!(!proc.meters.bypass_muted());
        proc.process_block(
            SinkChoice::Direct,
            &inp,
            &inp,
            &mut out2,
            2,
            other,
            44_100,
            0,
        );
        assert!(proc.meters.bypass_muted());
        proc.poll_updates(SinkChoice::Direct, 44_100, RateRatio::ONE, false);
        assert!(!proc.meters.bypass_muted());
    }

    #[test]
    fn direct_never_plays_a_chain_even_mid_fade() {
        // DSP chain swaps (a crossfade, then a warm-up and a fade out and
        // in) run through
        // their phases while blocks alternate between the sinks: every Direct block is the
        // samples unchanged, never either chain of the fade.
        let chain_db = |preamp_db: f64, pitch: bool| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    eq: Some(EqProfile {
                        name: "eq".into(),
                        preamp_db,
                        ..Default::default()
                    }),
                    // Pitch adds latency: that swap fades out and in.
                    pitch: audivane_protocol::PitchConfig {
                        enabled: pitch,
                        cents: 100.0,
                        ..Default::default()
                    },
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 1024,
                irs: None,
            })))
        };
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, mut recycle_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            chain_db(-6.0, false),
            true,
            2,
            1024,
            16,
            48_000,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let one = RateRatio::ONE;
        let inp = vec![0.25_f32; 2 * 256];
        let mut out = vec![0_i32; 2 * 256];
        let mut pos = 0u64;
        let mut phases = std::collections::HashSet::new();
        for round in 0..2 {
            swap_tx
                .push(PendingSwap {
                    chain: chain_db(-12.0, round == 1),
                    crossfade: true,
                    generation: round + 1,
                    swap_seq: round + 1,
                })
                .ok()
                .unwrap();
            for b in 0..400 {
                let sink = if b % 2 == 0 {
                    SinkChoice::Dsp
                } else {
                    SinkChoice::Direct
                };
                proc.poll_updates(sink, 48_000, one, true);
                phases.insert(std::mem::discriminant(&proc.swap_phase));
                proc.process_block(sink, &inp, &inp, &mut out, 2, one, 48_000, pos);
                pos += 256;
                if sink == SinkChoice::Direct {
                    assert!(
                        out.iter().all(|&v| v == 1 << 29),
                        "round {round} block {b}: {}",
                        out[0]
                    );
                }
            }
            while recycle_rx.pop().is_ok() {}
            assert_eq!(proc.active_generation(), round + 1);
        }
        // A crossfade, and a fade out and in (with Idle between).
        assert!(phases.len() >= 3, "{phases:?}");
    }

    #[test]
    fn a_sink_switch_knows_whether_the_target_has_its_chain() {
        // The DSP chain is x4; the device runs x1 for Direct. A switch to
        // the DSP sink at x1 has no chain there (the RT thread reopens so a
        // negotiation builds it); Direct and DSP off need none.
        let chain_x = |up: u32| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig {
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: RateTarget::Ratio(up),
                        taps: 64,
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 1024,
                irs: None,
            })))
        };
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            chain_x(4),
            true,
            2,
            1024,
            16,
            48_000,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let one = RateRatio::ONE;
        proc.poll_updates(SinkChoice::Direct, 48_000, one, true);
        assert!(proc.has_chain_for(SinkChoice::Direct, one));
        assert!(proc.has_chain_for(SinkChoice::Dsp, RateRatio::integer(4)));
        assert!(!proc.has_chain_for(SinkChoice::Dsp, one));
        // One on its way in counts.
        swap_tx
            .push(PendingSwap {
                chain: chain_x(1),
                crossfade: true,
                generation: 1,
                swap_seq: 1,
            })
            .ok()
            .unwrap();
        proc.poll_updates(SinkChoice::Direct, 48_000, one, true);
        assert!(proc.has_chain_for(SinkChoice::Dsp, one));
        // DSP off needs none at x1.
        proc.set_dsp_enabled_for_test(false);
        assert!(proc.has_chain_for(SinkChoice::Dsp, one));
    }

    #[test]
    fn a_chain_built_for_another_input_rate_never_plays() {
        // Its filters are tuned for 96 kHz; the device runs 44.1 kHz. It is
        // not the chain for this stream: silence and not ready (a resume
        // waits for the right one), never the wrong response.
        let chain_at = |rate: u32| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: ChainConfig::neutral(),
                sample_rate: rate,
                channels: 2,
                max_block: 1024,
                irs: None,
            })))
        };
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            chain_at(44_100),
            true,
            2,
            1024,
            16,
            44_100,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        let inp = vec![0.25_f32; 2 * 441];
        let mut out = vec![0_i32; 2 * 441];
        proc.poll_updates(SinkChoice::Dsp, 44_100, RateRatio::ONE, true);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::ONE,
            44_100,
            0,
        );
        assert!(out.iter().any(|&v| v != 0));
        assert!(proc.is_chain_ready(SinkChoice::Dsp, RateRatio::ONE, None));
        swap_tx
            .push(PendingSwap {
                chain: chain_at(96_000),
                crossfade: false,
                generation: 1,
                swap_seq: 1,
            })
            .ok()
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 44_100, RateRatio::ONE, true);
        proc.process_block(
            SinkChoice::Dsp,
            &inp,
            &inp,
            &mut out,
            2,
            RateRatio::ONE,
            44_100,
            441,
        );
        assert!(out.iter().all(|&v| v == 0));
        assert!(!proc.is_chain_ready(SinkChoice::Dsp, RateRatio::ONE, None));
        // Once the graph is at 96 kHz it is the right one.
        proc.poll_updates(SinkChoice::Dsp, 96_000, RateRatio::ONE, true);
        assert!(proc.is_chain_ready(SinkChoice::Dsp, RateRatio::ONE, None));
    }

    #[test]
    fn bypass_runs_only_at_x1_never_repeating_samples() {
        // 48 kHz into a 96 kHz-only DAC with DSP off: until the x1 reopen
        // the device stays at x2. Neither sink repeats each sample twice or
        // plays the x2 chain: silence, reported.
        let x2 = RateRatio::integer(2);
        let chain = ActiveChain::Single(Box::new(
            Chain::try_new(ChainSpec {
                config: ChainConfig {
                    upsampler: audivane_dsp_core::resample::ManualConfig {
                        target: RateTarget::Ratio(2),
                        taps: 256,
                        ..Default::default()
                    }
                    .into_upsampler_config()
                    .unwrap(),
                    ..ChainConfig::neutral()
                },
                sample_rate: 48_000,
                channels: 2,
                max_block: 1024,
                irs: None,
            })
            .unwrap(),
        ));
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let mut proc = DaemonProcessor::new(
            chain,
            false,
            2,
            1024,
            16,
            48_000,
            Arc::new(AtomicU8::new(0)),
            MeterBank::new(2),
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        // A 1 kHz tone: a sample-and-hold output repeats every frame.
        let inp: Vec<f32> = (0..2 * 480)
            .map(|i| (((i / 2) as f64) * 2.0 * std::f64::consts::PI / 48.0).sin() as f32 * 0.5)
            .collect();
        let mut out = vec![0_i32; 2 * 960];
        for pos in 0..4u64 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, x2, true);
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                x2,
                48_000,
                pos * 480,
            );
        }
        assert!(out.iter().all(|&v| v == 0), "DSP off played");
        assert!(proc.meters.bypass_muted());
        for pos in 4..8u64 {
            proc.poll_updates(SinkChoice::Direct, 48_000, x2, true);
            proc.process_block(
                SinkChoice::Direct,
                &inp,
                &inp,
                &mut out,
                2,
                x2,
                48_000,
                pos * 480,
            );
        }
        assert!(out.iter().all(|&v| v == 0));
        assert!(proc.meters.bypass_muted());
    }

    #[test]
    fn rational_swap_stays_on_the_frame_grid() {
        let ratio = RateRatio::from_rates(44_100, 48_000);
        let make_chain = || {
            ActiveChain::Single(Box::new(
                Chain::try_new(ChainSpec {
                    config: ChainConfig {
                        upsampler: audivane_dsp_core::resample::ManualConfig {
                            target: RateTarget::Rate(48_000),
                            taps: 256,
                            ..Default::default()
                        }
                        .into_upsampler_config()
                        .unwrap(),
                        output: audivane_protocol::OutputConfig {
                            bits: 24,
                            dither: audivane_protocol::DitherKind::None,
                            headroom_db: 0.0,
                        },
                        ..ChainConfig::neutral()
                    },
                    sample_rate: 44_100,
                    channels: 2,
                    max_block: 1024,
                    irs: None,
                })
                .unwrap(),
            ))
        };
        let new_proc = |swap_rx| {
            let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
            let (recycle_tx, recycle_rx) = rtrb::RingBuffer::new(8);
            let proc = DaemonProcessor::new(
                make_chain(),
                true,
                2,
                1024,
                16,
                44_100,
                Arc::new(AtomicU8::new(0)),
                MeterBank::new(2),
                cmd_rx,
                swap_rx,
                recycle_tx,
            );
            (proc, recycle_rx, _cmd_tx)
        };
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (mut swapped, mut recycled, _c1) = new_proc(swap_rx);
        let (_unused_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (mut steady, _r2, _c2) = new_proc(swap_rx);

        let mut pos = 0u64;
        let mut phase = 0.0_f64;
        let step = 2.0 * PI * 997.0 / 44_100.0;
        let mut max_diff = 0i64;
        let mut swapped_in = false;
        for b in 0..400usize {
            // The swap arrives at input frame 28414, which is 43 (mod 147):
            // off the grid of a fresh chain, so it must be moved onto it.
            if b == 61 {
                swap_tx
                    .push(PendingSwap {
                        chain: make_chain(),
                        crossfade: true,
                        generation: 1,
                        swap_seq: 1,
                    })
                    .unwrap();
            }
            let n = 64 + (b * 37) % 900;
            let mut inp = Vec::with_capacity(n * 2);
            for _ in 0..n {
                let s = (phase.sin() * 0.5) as f32;
                inp.extend([s, -s]);
                phase += step;
            }
            let n_out = ratio.frames_out(pos, n);
            let mut out_a = vec![0i32; n_out * 2];
            let mut out_b = vec![0i32; n_out * 2];
            swapped.poll_updates(SinkChoice::Dsp, 44_100, ratio, true);
            steady.poll_updates(SinkChoice::Dsp, 44_100, ratio, true);
            swapped.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out_a,
                2,
                ratio,
                44_100,
                pos,
            );
            steady.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out_b,
                2,
                ratio,
                44_100,
                pos,
            );
            for (a, b) in out_a.iter().zip(&out_b) {
                max_diff = max_diff.max((i64::from(*a) - i64::from(*b)).abs());
            }
            swapped_in |= swapped.active_generation() == 1;
            while recycled.pop().is_ok() {}
            pos += n as u64;
        }
        assert!(swapped_in, "the replacement chain never became active");
        // Both sides see identical input: only float rounding may differ
        // (one 24-bit LSB is 256 in the left-justified i32 output).
        assert!(max_diff <= 512, "outputs diverged by {max_diff}");
        // And the output is not silent.
        assert!(steady.active_chain.ratio() == ratio);
    }

    #[test]
    fn dc_block_live_swap_preserves_filter_state_across_eq_edit() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let make_cfg = |preamp_db: f64| ChainConfig {
            dc_block: Some(audivane_protocol::DcBlockConfig {
                enabled: true,
                cutoff_hz: 5.0,
            }),
            eq: Some(EqProfile {
                name: "pre".into(),
                preamp_db,
                bands: vec![],
                ..Default::default()
            }),
            output: audivane_protocol::OutputConfig {
                bits: 32,
                dither: audivane_protocol::DitherKind::None,
                headroom_db: 0.0,
            },
            ..ChainConfig::neutral()
        };
        let make_chain = |preamp_db: f64| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: make_cfg(preamp_db),
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            })))
        };

        let mut proc = DaemonProcessor::new(
            make_chain(0.0),
            true,
            2,
            512,
            16,
            48_000,
            ab_side,
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
        );
        // Continuous reference chains run from t = 0 without swapping.
        let mut ref_old = Chain::new(ChainSpec {
            config: make_cfg(0.0),
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: None,
        });
        let mut ref_new = Chain::new(ChainSpec {
            config: make_cfg(-6.0),
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: None,
        });

        let block_in = 512usize;
        let step = 2.0 * PI * 41.0 / 48_000.0;
        let mut frame_idx = 0usize;
        let mut gen_block = || {
            let mut buf = Vec::with_capacity(block_in * 2);
            for _ in 0..block_in {
                let s = (0.5 + 0.25 * (step * frame_idx as f64).sin()) as f32;
                buf.push(s);
                buf.push(s);
                frame_idx += 1;
            }
            buf
        };

        let mut out = vec![0i32; block_in * 2];
        let mut out_old = vec![0i32; block_in * 2];
        let mut out_new = vec![0i32; block_in * 2];

        // Settle the DC step + 41 Hz sine for 2.0 s (188 blocks * 512 = 96256 frames).
        for _ in 0..188 {
            let inp = gen_block();
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::ONE,
                48_000,
                0,
            );
            ref_old.process_f32(&inp, &mut out_old);
            ref_new.process_f32(&inp, &mut out_new);
        }

        // Perform an EQ-only live swap (-6 dB preamp) with the 10 ms crossfade.
        swap_tx
            .push(PendingSwap {
                chain: make_chain(-6.0),
                crossfade: true,
                generation: 2,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);

        let fade_len = 480usize; // 10 ms at 48 kHz
        let mut fade_pos = 0usize;
        let mut max_err = 0.0_f64;
        for _ in 0..4 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);
            let inp = gen_block();
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::ONE,
                48_000,
                0,
            );
            ref_old.process_f32(&inp, &mut out_old);
            ref_new.process_f32(&inp, &mut out_new);

            for f in 0..block_in {
                let w_new = if fade_pos < fade_len {
                    fade_pos += 1;
                    let t = (fade_pos as f64) / (fade_len as f64);
                    0.5 - 0.5 * (PI * t).cos()
                } else {
                    1.0
                };
                let w_old = 1.0 - w_new;
                for ch in 0..2 {
                    let idx = f * 2 + ch;
                    let expected = (out_old[idx] as f64) * w_old + (out_new[idx] as f64) * w_new;
                    let err = ((out[idx] as f64) - expected).abs() / 2_147_483_648.0;
                    max_err = max_err.max(err);
                }
            }
        }
        assert!(
            max_err < 1e-6,
            "live swap must match continuous DC-blocker state without pulse, max_err={max_err:e}"
        );

        // Also check matching A/B sides carry independent DC-blocker state across A/B swaps.
        let make_ab = |pre_a: f64, pre_b: f64, cut_b: f64| {
            let mut cfg_a = make_cfg(pre_a);
            cfg_a.dc_block.as_mut().unwrap().cutoff_hz = 5.0;
            let mut cfg_b = make_cfg(pre_b);
            cfg_b.dc_block.as_mut().unwrap().cutoff_hz = cut_b;
            ActiveChain::Ab(Box::new(
                audivane_dsp_core::chain::AbChain::new(
                    ChainSpec {
                        config: cfg_a,
                        sample_rate: 48_000,
                        channels: 2,
                        max_block: 512,
                        irs: None,
                    },
                    ChainSpec {
                        config: cfg_b,
                        sample_rate: 48_000,
                        channels: 2,
                        max_block: 512,
                        irs: None,
                    },
                    audivane_dsp_core::ab_level::AbLevel::fixed(0.0),
                )
                .unwrap(),
            ))
        };
        let mut ab_proc = DaemonProcessor::new(
            make_ab(0.0, 0.0, 10.0),
            true,
            2,
            512,
            16,
            48_000,
            Arc::new(AtomicU8::new(1)), // Side B audible (10 Hz)
            MeterBank::new(2),
            rtrb::RingBuffer::new(8).1,
            {
                let (mut tx, rx) = rtrb::RingBuffer::new(8);
                tx.push(PendingSwap {
                    chain: make_ab(-3.0, -6.0, 10.0),
                    crossfade: true,
                    generation: 2,
                    swap_seq: 1,
                })
                .unwrap();
                rx
            },
            rtrb::RingBuffer::new(8).0,
        );
        // Process 1 s before polling swap so both A (5 Hz) and B (10 Hz) settle.
        let dc_block_inp = vec![0.5_f32; 512 * 2];
        for _ in 0..94 {
            ab_proc.process_block(
                SinkChoice::Dsp,
                &dc_block_inp,
                &dc_block_inp,
                &mut out,
                2,
                RateRatio::ONE,
                48_000,
                0,
            );
        }
        ab_proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);
        ab_proc.process_block(
            SinkChoice::Dsp,
            &dc_block_inp,
            &dc_block_inp,
            &mut out,
            2,
            RateRatio::ONE,
            48_000,
            0,
        );
        let ab_tail_peak = out
            .iter()
            .map(|&s| (s as f64).abs() / 2_147_483_648.0)
            .fold(0.0, f64::max);
        assert!(
            ab_tail_peak < 1e-4,
            "A/B live swap must preserve settled B state without pulse, got {ab_tail_peak:e}"
        );
    }

    #[test]
    fn dc_block_live_swap_across_ab_transitions() {
        use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec};
        use audivane_protocol::{ChainConfig, DcBlockConfig};
        use std::f64::consts::PI;

        let cfg_with = |cutoff_hz: f64| ChainConfig {
            dc_block: Some(DcBlockConfig {
                enabled: true,
                cutoff_hz,
            }),
            ..ChainConfig::neutral()
        };
        let make_single = |cutoff_hz: f64| {
            ActiveChain::Single(Box::new(Chain::new(ChainSpec {
                config: cfg_with(cutoff_hz),
                sample_rate: 48_000,
                channels: 2,
                max_block: 512,
                irs: None,
            })))
        };
        let make_ab = |cut_a: f64, cut_b: f64, side: audivane_dsp_core::chain::Side| {
            let mut ab = AbChain::new(
                ChainSpec {
                    config: cfg_with(cut_a),
                    sample_rate: 48_000,
                    channels: 2,
                    max_block: 512,
                    irs: None,
                },
                ChainSpec {
                    config: cfg_with(cut_b),
                    sample_rate: 48_000,
                    channels: 2,
                    max_block: 512,
                    irs: None,
                },
                audivane_dsp_core::ab_level::AbLevel::fixed(0.0),
            )
            .unwrap();
            ab.select(side, 0);
            ab
        };

        let block_in = 512usize;
        let step = 2.0 * PI * 41.0 / 48_000.0;
        let mut frame_idx = 0usize;
        let mut gen_block = || {
            let mut buf = Vec::with_capacity(block_in * 2);
            for _ in 0..block_in {
                let s = (0.5 + 0.25 * (step * frame_idx as f64).sin()) as f32;
                buf.push(s);
                buf.push(s);
                frame_idx += 1;
            }
            buf
        };

        // 1. Disabling A/B while side B (10 Hz) is selected and single comes from A (5 Hz):
        // incoming Single must inherit side A's settled filter state, not selected side B's.
        let (mut swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(1)); // Side B audible
        let mut proc = DaemonProcessor::new(
            ActiveChain::Ab(Box::new(make_ab(
                5.0,
                10.0,
                audivane_dsp_core::chain::Side::B,
            ))),
            true,
            2,
            block_in,
            16,
            48_000,
            ab_side,
            MeterBank::new(2),
            rtrb::RingBuffer::new(8).1,
            swap_rx,
            rtrb::RingBuffer::new(8).0,
        );
        let mut ref_old_ab = make_ab(5.0, 10.0, audivane_dsp_core::chain::Side::B);
        let mut ref_a = Chain::new(ChainSpec {
            config: cfg_with(5.0),
            sample_rate: 48_000,
            channels: 2,
            max_block: block_in,
            irs: None,
        });

        let mut out = vec![0i32; block_in * 2];
        let mut out_old = vec![0i32; block_in * 2];
        let mut out_a = vec![0i32; block_in * 2];

        for _ in 0..188 {
            let inp = gen_block();
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::ONE,
                48_000,
                0,
            );
            ref_old_ab.process_f32(&inp, &mut out_old);
            ref_a.process_f32(&inp, &mut out_a);
        }

        swap_tx
            .push(PendingSwap {
                chain: make_single(5.0),
                crossfade: true,
                generation: 2,
                swap_seq: 1,
            })
            .unwrap();
        proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);

        let fade_len = 480usize; // 10 ms at 48 kHz
        let mut fade_pos = 0usize;
        let mut max_err = 0.0_f64;
        for _ in 0..4 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, RateRatio::ONE, true);
            let inp = gen_block();
            proc.process_block(
                SinkChoice::Dsp,
                &inp,
                &inp,
                &mut out,
                2,
                RateRatio::ONE,
                48_000,
                0,
            );
            ref_old_ab.process_f32(&inp, &mut out_old);
            ref_a.process_f32(&inp, &mut out_a);

            for f in 0..block_in {
                let w_new = if fade_pos < fade_len {
                    fade_pos += 1;
                    let t = (fade_pos as f64) / (fade_len as f64);
                    0.5 - 0.5 * (PI * t).cos()
                } else {
                    1.0
                };
                let w_old = 1.0 - w_new;
                for ch in 0..2 {
                    let idx = f * 2 + ch;
                    let expected = (out_old[idx] as f64) * w_old + (out_a[idx] as f64) * w_new;
                    let err = ((out[idx] as f64) - expected).abs() / 2_147_483_648.0;
                    max_err = max_err.max(err);
                }
            }
        }
        assert!(
            max_err < 1e-6,
            "disabling A/B from B must carry side A's continuous DC-blocker state, max_err={max_err:e}"
        );

        // 2. Reverse direction (Single -> A/B): A inherits Single's state; B stays fresh
        // when its cutoff differs, and inherits when compatible.
        let mut src = make_single(5.0);
        let dc_inp = vec![0.5_f32; block_in * 2];
        for _ in 0..94 {
            if let ActiveChain::Single(ref mut c) = src {
                c.process_f32(&dc_inp, &mut out);
            }
        }

        let mut dst_diff_a = ActiveChain::Ab(Box::new(make_ab(
            5.0,
            10.0,
            audivane_dsp_core::chain::Side::A,
        )));
        dst_diff_a.inherit_dc_block_state(&src);
        if let ActiveChain::Ab(ref mut ab) = dst_diff_a {
            ab.process_f32(&dc_inp, &mut out_a);
            let peak_a = out_a
                .iter()
                .map(|&s| (s as f64).abs() / 2_147_483_648.0)
                .fold(0.0, f64::max);
            assert!(
                peak_a < 1e-3,
                "Single -> A/B must carry A state across matching cutoff, got {peak_a:e}"
            );
        } else {
            panic!("expected Ab");
        }

        let mut dst_diff_b = ActiveChain::Ab(Box::new(make_ab(
            5.0,
            10.0,
            audivane_dsp_core::chain::Side::B,
        )));
        dst_diff_b.inherit_dc_block_state(&src);
        let mut fresh_b = Chain::new(ChainSpec {
            config: cfg_with(10.0),
            sample_rate: 48_000,
            channels: 2,
            max_block: block_in,
            irs: None,
        });
        fresh_b.process_f32(&dc_inp, &mut out_a);
        if let ActiveChain::Ab(ref mut ab) = dst_diff_b {
            ab.process_f32(&dc_inp, &mut out_old);
            assert_eq!(
                out_old, out_a,
                "Single -> A/B must keep B fresh when cutoff differs (10 Hz vs 5 Hz)"
            );
        } else {
            panic!("expected Ab");
        }

        let mut dst_same = ActiveChain::Ab(Box::new(make_ab(
            5.0,
            5.0,
            audivane_dsp_core::chain::Side::B,
        )));
        dst_same.inherit_dc_block_state(&src);
        if let ActiveChain::Ab(ref mut ab) = dst_same {
            ab.process_f32(&dc_inp, &mut out_old);
            let peak_b_same = out_old
                .iter()
                .map(|&s| (s as f64).abs() / 2_147_483_648.0)
                .fold(0.0, f64::max);
            assert!(
                peak_b_same < 1e-3,
                "Single -> A/B should let compatible B inherit settled state, got {peak_b_same:e}"
            );
        } else {
            panic!("expected Ab");
        }
    }
}

#[cfg(test)]
#[path = "conv_swap_tests.rs"]
mod conv_swap_tests;
