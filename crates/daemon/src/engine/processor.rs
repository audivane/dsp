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

use crate::engine::spectrum::SpectrumRt;
use audivane_dsp_core::bypass::{f32_to_i32, hash_i32};
use audivane_dsp_core::chain::{AbChain, Chain, Side};
use audivane_dsp_core::dither::ClipStats;
use audivane_dsp_core::gain::{MuteRamp, StereoGainRamp};
use audivane_io_pw::EngineAudioProcessor;
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{AbSide, Meters, SinkChoice};
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

/// Active DSP chain owned by the RT processor.
pub enum ActiveChain {
    Bypass,
    Single(Box<Chain>),
    Ab(Box<AbChain>),
}

impl ActiveChain {
    pub fn ratio(&self) -> u32 {
        match self {
            Self::Bypass => 1,
            Self::Single(c) => c.ratio() as u32,
            Self::Ab(ab) => ab.ratio() as u32,
        }
    }

    pub fn latency_out_frames(&self) -> usize {
        match self {
            Self::Bypass => 0,
            Self::Single(c) => c.latency_out_frames(),
            Self::Ab(ab) => ab.latency_out_frames(),
        }
    }

    pub fn max_block(&self) -> usize {
        match self {
            Self::Bypass => usize::MAX,
            Self::Single(c) => c.max_block(),
            Self::Ab(_ab) => usize::MAX,
        }
    }

    pub fn ab_select(&mut self, side: Side, crossfade_frames: usize) {
        if let Self::Ab(ab) = self {
            ab.select(side, crossfade_frames);
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
            Self::Single(c) => c.reset(),
            Self::Ab(ab) => ab.reset(),
        }
    }

    /// Process interleaved `f32` to quantized `i32`, optionally copying the
    /// floating-point chain output (before the quantizer/dither) into `tap`.
    fn process_f32_with_gain(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        channels: usize,
        max_chunk: usize,
        gain: &mut StereoGainRamp,
        mut tap: Option<&mut [f32]>,
    ) -> ClipStats {
        match self {
            Self::Bypass => {
                if let Some(tap) = tap.as_deref_mut() {
                    let n = input.len().min(tap.len());
                    tap[..n].copy_from_slice(&input[..n]);
                }
                gain.process_interleaved_f32_to_i32(input, out, channels);
                ClipStats::default()
            }
            Self::Single(c) => {
                let ratio = c.ratio().max(1);
                let limit = c.max_block().min(max_chunk).max(1);
                let in_frames = input.len() / channels;
                if in_frames <= limit {
                    return c.process_f32_with_gain_tapped(input, out, gain, tap);
                }
                let mut total = ClipStats::default();
                let mut done = 0usize;
                while done < in_frames {
                    let take = (in_frames - done).min(limit);
                    let in_s = &input[done * channels..(done + take) * channels];
                    let out_s = &mut out[done * ratio * channels..(done + take) * ratio * channels];
                    let tap_s = tap
                        .as_deref_mut()
                        .map(|t| &mut t[done * ratio * channels..(done + take) * ratio * channels]);
                    total.merge(c.process_f32_with_gain_tapped(in_s, out_s, gain, tap_s));
                    done += take;
                }
                total
            }
            Self::Ab(ab) => {
                let ratio = ab.ratio().max(1);
                let in_frames = input.len() / channels;
                let limit = max_chunk.max(1);
                if in_frames <= limit {
                    return ab.process_f32_with_gain_tapped(input, out, gain, tap);
                }
                let mut total = ClipStats::default();
                let mut done = 0usize;
                while done < in_frames {
                    let take = (in_frames - done).min(limit);
                    let in_s = &input[done * channels..(done + take) * channels];
                    let out_s = &mut out[done * ratio * channels..(done + take) * ratio * channels];
                    let tap_s = tap
                        .as_deref_mut()
                        .map(|t| &mut t[done * ratio * channels..(done + take) * ratio * channels]);
                    total.merge(ab.process_f32_with_gain_tapped(in_s, out_s, gain, tap_s));
                    done += take;
                }
                total
            }
        }
    }
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
    AbSelect { side: AbSide, crossfade_ms: f64 },
    SetDspEnabled(bool),
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
    sum_sq_fixed: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    sample_count: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
    clipped: AtomicU64,
    clipped_ch: [AtomicU64; MAX_METER_CHANNELS_OR_DEFAULT],
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

impl MeterBank {
    pub fn new(channels: usize) -> Arc<Self> {
        Arc::new(Self {
            channels: AtomicU32::new((channels.clamp(1, MAX_METER_CHANNELS)) as u32),
            peak_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            input_peak_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            chain_peak_bits: std::array::from_fn(|_| AtomicU64::new(0)),
            chain_blocks: AtomicU64::new(0),
            sum_sq_fixed: std::array::from_fn(|_| AtomicU64::new(0)),
            sample_count: std::array::from_fn(|_| AtomicU64::new(0)),
            clipped: AtomicU64::new(0),
            clipped_ch: std::array::from_fn(|_| AtomicU64::new(0)),
        })
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

    /// Update meters from interleaved left-justified `i32` DAC output frames.
    /// When `clip_stats` carries measured float peaks (DSP path), the output
    /// peak is the float signal just before quantization (after the master
    /// volume, so it can read above 0 dBFS when the quantizer clips) and the
    /// chain peak before the master volume is recorded too. Otherwise the
    /// peak comes from the integer output. Strictly allocation-free and
    /// lock-free.
    #[inline]
    pub fn record_i32_output(&self, out: &[i32], channels: usize, clip_stats: ClipStats) {
        let ch = channels.clamp(1, MAX_METER_CHANNELS);
        self.channels.store(ch as u32, Ordering::Relaxed);
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

/// Real-time audio processor bridging [`EngineDriverHandle`] and `dsp-core`.
pub struct DaemonProcessor {
    active_chain: ActiveChain,
    active_generation: u64,
    active_generation_atomic: Arc<AtomicU64>,
    active_swap_seq: u64,
    swap_drain_paused: Arc<AtomicBool>,
    swap_phase: SwapPhase,
    dsp_enabled: bool,
    max_block: usize,
    active_rate: u32,
    ab_side: Arc<AtomicU8>,
    shared_volume: Arc<SharedVolume>,
    volume_gen: u64,
    volume_target: VolumeTarget,
    dsp_gain: StereoGainRamp,
    direct_mute: MuteRamp,
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
            active_swap_seq: 0,
            swap_drain_paused: Arc::new(AtomicBool::new(false)),
            swap_phase: SwapPhase::Idle,
            dsp_enabled,
            max_block: scratch_max_block,
            active_rate: rate,
            ab_side,
            shared_volume: Arc::new(SharedVolume::default()),
            volume_gen: 0,
            volume_target: VolumeTarget::default(),
            dsp_gain: StereoGainRamp::unity(rate),
            direct_mute: MuteRamp::unmuted(rate),
            meters,
            cmd_rx,
            swap_rx,
            recycle_tx,
            recycle_stash: std::array::from_fn(|_| None),
            swap_scratch: vec![0i32; scratch_len],
            post_scratch: vec![0.0f32; scratch_len],
            selftest_scratch: vec![0i32; 4096 * channels.max(1)],
            spectrum: None,
        }
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
    fn tap_spectrum_i32(&mut self, pre: &[f32], out: &[i32], graph_rate: u32, ratio: u32) {
        let Some(spectrum) = self.spectrum.as_mut() else {
            return;
        };
        spectrum.push_pre(pre, graph_rate);
        spectrum.push_post_i32(out, graph_rate.saturating_mul(ratio.max(1)));
    }

    pub fn volume_handle(&self) -> Arc<SharedVolume> {
        Arc::clone(&self.shared_volume)
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
    /// Invariant: RT swap callers (`can_accept_swap()` and
    /// `clear_unadvanceable_swap_phase()`) require `recycle_capacity() >= 1`,
    /// which reserves 1 of the `RECYCLE_STASH_CAP` (= 4) slots exclusively for
    /// `reset_dsp_state()`. Therefore a free stash slot is always available
    /// when `recycle_tx` is full; the fallback below is unreachable unless that
    /// invariant is violated, and leaks via `std::mem::forget` rather than
    /// deallocating on the RT thread.
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

    fn is_dsp_live(&self, active_sink: SinkChoice, actual_ratio: u32, is_streaming: bool) -> bool {
        is_streaming
            && self.dsp_enabled
            && active_sink != SinkChoice::Direct
            && actual_ratio > 0
            && self.active_chain.ratio() == actual_ratio
    }

    fn clear_unadvanceable_swap_phase(
        &mut self,
        active_sink: SinkChoice,
        actual_ratio: u32,
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
                        let old = std::mem::replace(&mut self.active_chain, new_chain);
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

    fn fade_len_for(&self, ms: usize, ratio: u32) -> usize {
        let out_rate = (self.active_rate as usize) * (ratio.max(1) as usize);
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
        actual_ratio: u32,
        is_streaming: bool,
    ) {
        // Re-apply the latest A/B selection to the chain being installed. A
        // chain built before an `AbSelect` arrived (and still sitting in the
        // swap ring) must not pin the old side.
        let latest_side = match self.ab_side.load(Ordering::Acquire) {
            1 => AbSide::B,
            _ => AbSide::A,
        };
        new_chain.apply_ab_side(latest_side);

        let same_ratio = self.active_chain.ratio() == new_chain.ratio();
        let dsp_live = self.is_dsp_live(active_sink, actual_ratio, is_streaming);

        // If audio/DSP is not currently live (idle, Direct, bypass, or ratio changed
        // between chains), promote the new chain immediately without warming or fading.
        if !dsp_live || !same_ratio {
            let old = std::mem::replace(&mut self.active_chain, new_chain);
            self.set_active_swap(generation, swap_seq);
            self.push_recycle(old);
            self.swap_phase = SwapPhase::Idle;
            return;
        }

        let new_lat = new_chain.latency_out_frames();
        let target_frames = if new_lat == 0 {
            0
        } else {
            new_lat + WARMUP_SETTLE_MARGIN_FRAMES
        };

        if target_frames == 0 {
            self.promote_warmed_chain(new_chain, generation, swap_seq, crossfade);
        } else {
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
            let old = std::mem::replace(&mut self.active_chain, new_chain);
            self.set_active_swap(generation, swap_seq);
            self.push_recycle(old);
            self.swap_phase = SwapPhase::Idle;
        } else if old_lat == new_lat {
            let old = std::mem::replace(&mut self.active_chain, new_chain);
            self.set_active_swap(generation, swap_seq);
            let fade_len = self.fade_len_for(SWAP_CROSSFADE_MS, ratio);
            self.swap_phase = SwapPhase::Crossfading {
                fading_old: old,
                fade_pos: 0,
                fade_len,
            };
        } else {
            // Latencies differ: use fade-through-silence (`FadeOutOld` -> switch at
            // zero -> `FadeInNew`) rather than dynamically delay-aligning `old_chain`
            // vs `new_chain`. `old_chain` is already running on the RT thread with a
            // fixed internal latency and cannot allocate a delay line on RT without
            // preallocating worst-case delay buffers on every chain, while delaying a
            // lower-latency `new_chain` would permanently inflate steady-state latency
            // after the swap. Switching at zero avoids comb-filtering with zero extra
            // latency or RT allocation.
            let fade_len = self.fade_len_for(SWAP_FADE_HALF_MS, ratio);
            self.swap_phase = SwapPhase::FadeOutOld {
                pending_new: new_chain,
                generation,
                swap_seq,
                fade_pos: 0,
                fade_len,
            };
        }
    }

    fn warm_chain_block(
        chain: &mut ActiveChain,
        dsp_in: &[f32],
        scratch: &mut [i32],
        channels: usize,
        max_block: usize,
        gain: &mut StereoGainRamp,
    ) -> usize {
        let ratio = chain.ratio().max(1) as usize;
        let in_frames = dsp_in.len() / channels;
        let chunk_in_cap = (scratch.len() / (channels * ratio)).max(1);
        let mut done_in = 0usize;
        while done_in < in_frames {
            let take_in = (in_frames - done_in).min(chunk_in_cap);
            let out_samples = take_in * ratio * channels;
            let in_slice = &dsp_in[done_in * channels..(done_in + take_in) * channels];
            chain.process_f32_with_gain(
                in_slice,
                &mut scratch[..out_samples],
                channels,
                max_block,
                gain,
                None,
            );
            done_in += take_in;
        }
        in_frames * ratio
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
}

impl EngineAudioProcessor for DaemonProcessor {
    fn poll_updates(
        &mut self,
        active_sink: SinkChoice,
        graph_rate: u32,
        actual_ratio: u32,
        is_streaming: bool,
    ) {
        self.active_rate = graph_rate.max(8_000);
        self.sync_volume_state(
            self.active_rate.saturating_mul(actual_ratio.max(1)),
            is_streaming,
        );
        self.flush_recycle_stash();

        // Always drain lightweight control commands even when swaps are paused.
        while let Ok(cmd) = self.cmd_rx.pop() {
            match cmd {
                RtDspCmd::AbSelect { side, crossfade_ms } => {
                    let s = match side {
                        AbSide::A => Side::A,
                        AbSide::B => Side::B,
                    };
                    let out_rate =
                        (self.active_rate as f64) * (self.active_chain.ratio().max(1) as f64);
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
                RtDspCmd::SetDspEnabled(en) => {
                    self.dsp_enabled = en;
                }
                RtDspCmd::RunSelfTest(job) => {
                    self.execute_selftest(&job);
                }
            }
        }

        // Never allow a swap/fade phase to remain stuck when `process_block`
        // is paused, in passthrough, or running a different ratio.
        self.clear_unadvanceable_swap_phase(active_sink, actual_ratio, is_streaming);

        if !self.swap_drain_paused.load(Ordering::Acquire) {
            // If a newer swap arrived while an older chain is still warming up,
            // abort the obsolete warm-up so the latest config takes effect immediately.
            if matches!(self.swap_phase, SwapPhase::Warming { .. })
                && !self.swap_rx.is_empty()
                && self.recycle_capacity() >= 2
            {
                let phase = std::mem::replace(&mut self.swap_phase, SwapPhase::Idle);
                if let SwapPhase::Warming { new_chain, .. } = phase {
                    self.push_recycle(new_chain);
                }
            }

            // Accept and coalesce queued chain swaps ("latest wins") when idle and
            // recycle capacity allows retiring any bypassed chains without RT drop.
            if self.can_accept_swap()
                && let Ok(mut latest) = self.swap_rx.pop()
            {
                while self.recycle_capacity() >= 2
                    && let Ok(newer) = self.swap_rx.pop()
                {
                    self.push_recycle(latest.chain);
                    latest = newer;
                }
                self.begin_swap(
                    latest.chain,
                    latest.generation,
                    latest.swap_seq,
                    latest.crossfade,
                    active_sink,
                    actual_ratio,
                    is_streaming,
                );
            }
        }
    }

    fn process_block(
        &mut self,
        active_sink: SinkChoice,
        dsp_in: &[f32],
        direct_in: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: u32,
        graph_rate: u32,
    ) {
        self.active_rate = graph_rate.max(8_000);
        self.sync_volume_state(self.active_rate.saturating_mul(ratio.max(1)), true);
        self.flush_recycle_stash();
        let input_tap = if active_sink == SinkChoice::Direct {
            direct_in
        } else {
            dsp_in
        };
        self.meters.record_input_peaks(input_tap, channels);

        if active_sink == SinkChoice::Direct || !self.dsp_enabled || ratio == 0 {
            let src = input_tap;
            self.direct_mute
                .process_f32_to_i32(src, out, channels, ratio);
            self.meters
                .record_i32_output(out, channels, ClipStats::default());
            self.tap_spectrum_i32(src, out, graph_rate, ratio);
            return;
        }

        // Guard against any chain/ALSA ratio transition mismatch cleanly:
        // never emit un-processed passthrough audio when DSP is enabled with a
        // mismatched chain; mute output until the matching chain is installed.
        if self.active_chain.ratio() != ratio {
            out.fill(0);
            self.meters
                .record_i32_output(out, channels, ClipStats::default());
            self.tap_spectrum_i32(dsp_in, out, graph_rate, ratio);
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
        let stats = self.active_chain.process_f32_with_gain(
            dsp_in,
            out,
            channels,
            self.max_block,
            &mut self.dsp_gain,
            post_tap,
        );

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
                    let mut old_gain = gain_before;
                    let r = ratio.max(1) as usize;
                    let total_in_frames = dsp_in.len() / channels;
                    let chunk_in_cap = (self.swap_scratch.len() / (channels * r)).max(1);
                    let mut done_in = 0usize;
                    while done_in < total_in_frames {
                        let take_in = (total_in_frames - done_in).min(chunk_in_cap);
                        let out_frames = take_in * r;
                        let out_samples = out_frames * channels;
                        let in_slice = &dsp_in[done_in * channels..(done_in + take_in) * channels];
                        let out_slice =
                            &mut out[done_in * r * channels..(done_in * r + out_frames) * channels];
                        let old_buf = &mut self.swap_scratch[..out_samples];
                        fading_old.process_f32_with_gain(
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
                    let mut new_gain = gain_before;
                    let r = ratio.max(1) as usize;
                    let total_in_frames = dsp_in.len() / channels;
                    let chunk_in_cap = (self.swap_scratch.len() / (channels * r)).max(1);
                    let mut done_in = 0usize;
                    let mut switched = false;
                    let mut in_fade_pos = 0usize;
                    while done_in < total_in_frames {
                        let take_in = (total_in_frames - done_in).min(chunk_in_cap);
                        let out_frames = take_in * r;
                        let out_samples = out_frames * channels;
                        let in_slice = &dsp_in[done_in * channels..(done_in + take_in) * channels];
                        let out_slice =
                            &mut out[done_in * r * channels..(done_in * r + out_frames) * channels];
                        let new_buf = &mut self.swap_scratch[..out_samples];
                        pending_new.process_f32_with_gain(
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
                    }

                    if switched {
                        let old = std::mem::replace(&mut self.active_chain, pending_new);
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

        self.meters.record_i32_output(out, channels, stats);
        if spectrum_on && let Some(spectrum) = self.spectrum.as_mut() {
            spectrum.push_pre(dsp_in, graph_rate);
            spectrum.push_post_f32(
                &self.post_scratch[..tap_len],
                graph_rate.saturating_mul(ratio.max(1)),
            );
        }
    }

    fn run_selftest_direct(&mut self, in_samples: &[f32], out_samples: &mut [i32]) {
        f32_to_i32(in_samples, out_samples);
    }

    fn latency_out_frames(&self, active_sink: SinkChoice) -> usize {
        if active_sink == SinkChoice::Direct || !self.dsp_enabled {
            0
        } else {
            self.active_chain.latency_out_frames()
        }
    }

    fn reset_dsp_state(&mut self) {
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
                let old = std::mem::replace(&mut self.active_chain, new_chain);
                self.set_active_swap(generation, swap_seq);
                self.push_recycle(old);
            }
            SwapPhase::Crossfading { fading_old, .. } => {
                self.push_recycle(fading_old);
            }
        }
        self.active_chain.reset();
    }

    fn is_chain_ready(
        &self,
        active_sink: SinkChoice,
        actual_ratio: u32,
        required_seq: Option<u64>,
    ) -> bool {
        if active_sink == SinkChoice::Direct || !self.dsp_enabled || actual_ratio == 0 {
            return true;
        }
        self.active_chain.ratio() == actual_ratio
            && matches!(self.swap_phase, SwapPhase::Idle)
            && required_seq.is_none_or(|req| self.active_swap_seq >= req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_dsp_core::chain::ChainSpec;
    use audivane_protocol::{
        Band, ChainConfig, EqProfile, FilterType, RateTarget, UpsamplerConfig,
    };

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
                ..Default::default()
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 1, false);

        let inp = vec![0.5_f32; 512 * 2];
        let mut out = vec![0_i32; 512 * 2];
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);

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
            config: ChainConfig::default(),
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
            ..Default::default()
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 1, true);
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_swap_seq(), 1);

        let inp = vec![0.1f32; 512 * 2];
        let mut out = vec![0i32; 512 * 2];
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);
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
                    upsampler: UpsamplerConfig {
                        target: RateTarget::Ratio(4),
                        taps: 8192,
                        ..Default::default()
                    },
                    ..Default::default()
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
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 4, 48_000);
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);

        let mut min_block_rms = f64::MAX;
        for _ in 0..30 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
            let inp = gen_block();
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 4, 48_000);
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
            config: ChainConfig::default(),
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
                }],
                ..Default::default()
            },
            sample_rate: 48_000,
            channels: 2,
            max_block: 512,
            irs: Some(vec![vec![1.0; 2048]]),
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 1, true);

        let inp = vec![0.25f32; 512 * 2];
        let mut out = vec![0i32; 512 * 2];
        for _ in 0..10 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, 1, true);
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);
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
                    upsampler: UpsamplerConfig {
                        target: if ratio <= 1 {
                            RateTarget::Off
                        } else {
                            RateTarget::Ratio(ratio)
                        },
                        taps,
                        ..Default::default()
                    },
                    ..Default::default()
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 1, false);
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, false);
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_chain.ratio(), 4);
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
            proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 4, 48_000);
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
                    upsampler: UpsamplerConfig {
                        target: RateTarget::Ratio(4),
                        taps: 8192,
                        ..Default::default()
                    },
                    ..Default::default()
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 4, 48_000);
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
        assert!(matches!(
            proc.swap_phase,
            SwapPhase::Warming { generation: 4, .. }
        ));

        // Finish warming + crossfade -> active generation becomes 4.
        for _ in 0..20 {
            proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
            proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 4, 48_000);
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
                    upsampler: UpsamplerConfig {
                        target: if ratio <= 1 {
                            RateTarget::Off
                        } else {
                            RateTarget::Ratio(ratio)
                        },
                        taps,
                        ..Default::default()
                    },
                    ..Default::default()
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 1, true);
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, false);
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_chain.ratio(), 4);
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, true);
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 4, 48_000);
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
                    upsampler: UpsamplerConfig {
                        target: if ratio <= 1 {
                            RateTarget::Off
                        } else {
                            RateTarget::Ratio(ratio)
                        },
                        taps: if ratio <= 1 { 16 } else { 8192 },
                        ..Default::default()
                    },
                    ..Default::default()
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, false);
        assert!(!proc.is_chain_ready(SinkChoice::Dsp, 4, Some(5)));
        assert!(!proc.is_chain_ready(SinkChoice::Dsp, 4, None));
        // Direct sink bypasses DSP chain readiness.
        assert!(proc.is_chain_ready(SinkChoice::Direct, 4, Some(5)));

        // Any block rendered while 1x chain is active for a 4x ALSA output MUST be muted (all 0s),
        // never un-EQ'd passthrough.
        let inp = vec![0.5f32; 512 * 2];
        let mut out = vec![123_i32; 512 * 4 * 2];
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 4, 48_000);
        assert!(
            out.iter().all(|&s| s == 0),
            "mismatched DSP chain ratio must output silence instead of passthrough"
        );

        // Unpause drain -> poll installs swap_seq = 5 and makes chain ready for 4x.
        proc.set_swap_drain_paused(false);
        proc.poll_updates(SinkChoice::Dsp, 48_000, 4, false);
        assert!(proc.is_chain_ready(SinkChoice::Dsp, 4, Some(5)));
        assert_eq!(proc.active_generation(), 2);
        assert_eq!(proc.active_swap_seq(), 5);
        while recycle_rx.pop().is_ok() {}
    }

    #[test]
    fn direct_sink_stays_bit_exact_with_digital_volume_and_balance_and_ramps_mute() {
        let (_cmd_tx, cmd_rx) = rtrb::RingBuffer::new(8);
        let (_swap_tx, swap_rx) = rtrb::RingBuffer::new(8);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(8);
        let ab_side = Arc::new(AtomicU8::new(0));
        let meters = MeterBank::new(2);

        let chain = ActiveChain::Single(Box::new(Chain::new(ChainSpec {
            config: ChainConfig::default(),
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

        // Digital volume -18 dB and balance +12 dB must NEVER scale Direct path.
        vol.store(VolumeTarget {
            volume_db: -18.0,
            balance_db: 12.0,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        });
        proc.poll_updates(SinkChoice::Direct, 48_000, 1, false);
        let mut out = vec![0_i32; frames * 2];
        proc.process_block(SinkChoice::Direct, &inp, &inp, &mut out, 2, 1, 48_000);
        assert_eq!(out, expected_i32);
        assert_eq!(hash_i32(&out), hash_i32(&expected_i32));

        // Mute without hardware switch ramps to silence on Direct path.
        vol.store(VolumeTarget {
            volume_db: -18.0,
            balance_db: 12.0,
            muted: true,
            hardware_volume: false,
            hardware_mute: false,
        });
        // First 512-frame block completes the 480-frame ramp; next block is exact zeros.
        proc.process_block(SinkChoice::Direct, &inp, &inp, &mut out, 2, 1, 48_000);
        assert!(out[..480 * 2].iter().any(|&s| s != 0));
        proc.process_block(SinkChoice::Direct, &inp, &inp, &mut out, 2, 1, 48_000);
        assert!(out.iter().all(|&s| s == 0));

        // Snap unmute while idle restores bit-exactness immediately.
        vol.store(VolumeTarget {
            volume_db: -18.0,
            balance_db: 12.0,
            muted: false,
            hardware_volume: false,
            hardware_mute: false,
        });
        proc.poll_updates(SinkChoice::Direct, 48_000, 1, false);
        proc.process_block(SinkChoice::Direct, &inp, &inp, &mut out, 2, 1, 48_000);
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
            config: ChainConfig::default(),
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
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);
        proc.process_block(SinkChoice::Dsp, &inp, &inp, &mut out, 2, 1, 48_000);
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
        proc.poll_updates(SinkChoice::Dsp, 48_000, 1, false);
        proc.process_block(
            SinkChoice::Dsp,
            &exact_inp,
            &exact_inp,
            &mut out,
            2,
            1,
            48_000,
        );
        assert_eq!(out, expected_i32);
    }
}
