//! Dual-sink PipeWire driver (`Audivane DSP` + `Audivane Direct`) paced by a
//! single ALSA `hw:` DAC clock, with PipeWire `settings` metadata rate
//! following and off-RT device lifecycle / chain negotiation.

use crate::clock::{
    BoundedEventStash, DriverClockState, EventFd, FadeRamp, SynchronizedIo, mono_now_ns,
    mono_raw_now_ns, read_target_clock,
};
use crate::error::{PwError, Result};
use crate::lifecycle::{
    DesiredDspConfig, DeviceBackend, DeviceLifecycle, DeviceRates, NegotiatedKey, OpenedDevice,
    PlaybackSessions, ReopenGate, decide_rt_reopen, negotiated_key,
};
use crate::metadata::{MetadataOp, MetadataTracker};
use crate::rate_arbiter::{RateArbiter, RateClient, RateInputs, Verdict};
use crate::rate_caps::{GraphRate, MoveAttempts, effective_rate_policy};
use crate::sink_volume::{self, SinkVolumeRequest, SinkVolumeShared, SinkVolumeView};
use crate::varispeed_loop::{
    self as vsl, Handover, PeriodBufs, PeriodPlan, VsDelay, VsDrain, VsGraph,
};
use crate::varispeed_rt::{
    VarispeedCommandSlot, VarispeedDriver, VarispeedDriverSnapshot, VarispeedDriverStatus,
    VarispeedPort,
};
use crate::varispeed_sched::{PullKind, RING_ALLOC_FRAMES, Timing};
use audivane_io_alsa::{
    AlsaConfig, AlsaOutput, DEFAULT_RESERVE_PRIORITY, DeviceReservation, NegotiatedParams, OpenPcm,
    alsa_lib_errors, card_index_from_device, probe_rates, silence_alsa_lib_on_this_thread,
};
use audivane_protocol::ipc::{
    BypassResampler, DeviceCapabilities, EngineState, RatePolicy, SinkChoice,
};
use audivane_protocol::{ALLOWED_RATES, MAX_OUTPUT_RATE, RateRatio, RateTarget, SampleFormat};
use pipewire as pw;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use spa::utils::result::AsyncSeq;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PW_STREAM_DRIVER_PRIORITY: &str = "300000";
const PW_DSP_SESSION_PRIORITY: &str = "2000";
const PW_DIRECT_SESSION_PRIORITY: &str = "1900";
const PW_DAC_GROUP: &str = "audivane-dac";
const FADE_MS: usize = 25;

/// Real-time audio processing hook invoked on the ALSA RT thread for every
/// DAC period. Implementations must be strictly allocation-free and lock-free.
pub trait EngineAudioProcessor: Send + 'static {
    /// Process one period block from whichever sink currently owns the DAC.
    ///
    /// - `active_sink`: `SinkChoice::Direct` (ratio guaranteed 1) or `SinkChoice::Dsp`.
    /// - `dsp_in`: interleaved `f32` frames from `audivane_dsp`.
    /// - `direct_in`: interleaved `f32` frames from `audivane_direct`.
    /// - `out`: interleaved left-justified `i32` output at `rate * ratio`:
    ///   exactly `ratio.frames_out(in_pos, frames)` frames (see
    ///   [`RateRatio::frames_out`]; `frames * ratio` for integer ratios).
    /// - `in_pos`: input frames handed to `process_block` since the last
    ///   [`Self::reset_dsp_state`]: where this block sits on the rational
    ///   frame grid. A processor keeps its chains on that grid (a chain that
    ///   is not, e.g. one just installed, is moved onto it).
    #[allow(clippy::too_many_arguments)]
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
    );

    /// Execute an injected deterministic self-test block through the exact
    /// Direct bypass conversion path (`f32 -> i32`).
    fn run_selftest_direct(&mut self, in_samples: &[f32], out_samples: &mut [i32]);

    /// Poll non-blocking lock-free control updates (e.g. chain swaps, A/B side,
    /// self-test jobs) on the RT thread.
    ///
    /// `is_streaming` is `true` only when the ALSA output is actively streaming
    /// audio blocks; when `false`, pending swaps install immediately without
    /// waiting on warm-up or crossfade ramps that cannot advance while idle.
    fn poll_updates(
        &mut self,
        active_sink: SinkChoice,
        graph_rate: u32,
        actual_ratio: RateRatio,
        is_streaming: bool,
    );

    /// Return current DSP algorithmic latency in output frames (`0` on Direct or bypass).
    fn latency_out_frames(&self, active_sink: SinkChoice) -> usize;

    /// Reset internal filter/convolver state cleanly when playback stops.
    fn reset_dsp_state(&mut self);

    /// Return whether the active DSP chain is installed for `(active_sink, actual_ratio)`
    /// without an in-progress crossfade and has reached at least `required_seq`
    /// (when provided).
    fn is_chain_ready(
        &self,
        _active_sink: SinkChoice,
        _actual_ratio: RateRatio,
        _required_seq: Option<u64>,
    ) -> bool {
        true
    }

    /// Whether `sink` has what it plays at `ratio` (no chain needed, or one
    /// built for it is installed or on its way in), so a switch to it needs
    /// no reopen. A sink without it would wait for a chain only a
    /// negotiation builds.
    fn has_chain_for(&self, sink: SinkChoice, ratio: RateRatio) -> bool {
        self.is_chain_ready(sink, ratio, None)
    }

    /// The varispeed stage, when one runs for the next block of
    /// `active_sink` at `ratio` and `graph_rate` with `channels` channels
    /// (the DSP sink with DSP on, a stage built for that rate and channel
    /// count). While it returns one, the driver pulls through it (the block
    /// handed to [`Self::process_block`] then holds the planned source
    /// frames, and the processor knows the nominal count from its plan) and
    /// schedules graph cycles by watermark
    /// ([`crate::varispeed_sched::VarispeedScheduler`]). `None`: no stage,
    /// so the driver plays raw blocks.
    fn varispeed(
        &mut self,
        _active_sink: SinkChoice,
        _ratio: RateRatio,
        _graph_rate: u32,
        _channels: usize,
    ) -> Option<&mut dyn VarispeedPort> {
        None
    }

    /// The newest config generation whose chain runs on this thread: the
    /// active one, or one warming up next to it (unheard, but its cost is
    /// the thread's). An overload demotion is published with it. 0 when the
    /// processor has no generations.
    fn running_generation(&self) -> u64 {
        0
    }

    /// The DAC was read at a wakeup: `delay` device frames queued ahead of
    /// the DAC at `time_ns` (`CLOCK_MONOTONIC_RAW`, the domain of ALSA's
    /// status timestamps), with everything [`Self::process_block`] has
    /// output written. Multi-device: the processor publishes its position
    /// at the DAC from it.
    fn dac_read(&mut self, _delay: i64, _time_ns: u64) {}

    /// The part of [`Self::latency_out_frames`] that delays the source
    /// before any stage (multi-device alignment), graph frames, on the
    /// path `active_sink` plays. The clock's `delay` and the stream-end
    /// flush count it on the path without varispeed.
    fn alignment_frames(&self, _active_sink: SinkChoice) -> usize {
        0
    }

    /// The most [`Self::alignment_frames`] can grow to while a drain runs
    /// (the stream-end flush budgets it).
    fn alignment_max_frames(&self, active_sink: SinkChoice) -> usize {
        self.alignment_frames(active_sink)
    }

    /// The PCM was prepared for a new stream (a start, a reopen, a
    /// recovery): nothing the processor delayed before belongs to it.
    fn stream_prepared(&mut self) {}
}

/// Source frames a varispeed block can take: the stage's most for the
/// largest call (`2n + 1` at `n` = 8192), so the input buffers never grow
/// for it.
const VS_MAX_SRC_FRAMES: usize = 2 * 8192 + 2;

/// Splits the DAC's fixed periods into input blocks on the rational frame
/// grid ([`RateRatio::frames_out`]).
///
/// Integer ratios: `period / ratio` input frames give `period / ratio *
/// ratio` output frames, every period.
/// Rational ratios: one input frame can complete several output frames, so
/// a period is planned as the fewest input frames that produce at least
/// what the DAC still needs, and the few frames written beyond a period
/// (fewer than `ceil(up / down)`) are credited to the next one.
///
/// The planner restarts (position and carried frames back to 0) on every
/// DSP reset and device reopen, and on its own when the ratio changes: a
/// position on one ratio's frame grid means nothing on another's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PeriodPlanner {
    /// Input frames processed since the last DSP reset.
    pos: u64,
    /// Output frames written beyond the periods so far.
    credit: usize,
    /// The ratio `pos` and `credit` count on.
    ratio: RateRatio,
}

impl PeriodPlanner {
    /// `(input frames, output frames)` for the next `period_out`-frame period.
    fn plan(&mut self, ratio: RateRatio, period_out: u32) -> (usize, usize) {
        if ratio != self.ratio {
            *self = Self {
                ratio,
                ..Self::default()
            };
        }
        let period_out = period_out.max(1) as usize;
        if ratio.is_integer() {
            let r = ratio.up.max(1) as usize;
            let n_in = (period_out.max(r) / r).max(1);
            return (n_in, n_in * r);
        }
        let want = period_out.saturating_sub(self.credit).max(1);
        let n_in = ratio.frames_in_for(self.pos, want);
        (n_in, ratio.frames_out(self.pos, n_in))
    }

    /// Account a processed block of `n_in` -> `n_out` frames.
    fn commit(&mut self, n_in: usize, n_out: usize, period_out: u32) {
        self.pos += n_in as u64;
        self.credit = (self.credit + n_out).saturating_sub(period_out as usize);
    }

    /// Account a silent prime of `n_out` frames: no input was processed,
    /// so the position stays, but the frames count toward the credit like
    /// audio, so the primes after it still fit the buffer.
    fn commit_silence(&mut self, n_out: usize, period_out: u32) {
        self.credit = (self.credit + n_out).saturating_sub(period_out as usize);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A silent prime (no source data yet): the planned period's frames of
/// silence, no more than the buffer takes (auto-start is off, so nothing
/// frees room), credited to the planner. Frames written, `None` when the
/// write failed.
fn prime_silence<W: PcmSink>(
    w: &mut W,
    planner: &mut PeriodPlanner,
    prime_out: usize,
    period_out: u32,
) -> Option<usize> {
    let frames = prime_out.min(w.room().unwrap_or(prime_out));
    let n = w.write_silence(frames)?;
    planner.commit_silence(n, period_out);
    Some(n)
}

/// [`OutCarry::write_ahead`] on the running device, counting the carried
/// frames written (in `frames_out` and on their own).
fn write_carry_ahead<W: PcmSink>(
    carry: &mut OutCarry,
    w: &mut W,
    channels: usize,
    shared: &SharedEngineState,
) -> CarryNext {
    let stop =
        || shared.stop.load(Ordering::Acquire) || shared.release_requested.load(Ordering::Acquire);
    let (n, next) = carry.write_ahead(w, channels, &stop);
    if n > 0 {
        shared.frames_out.fetch_add(n as u64, Ordering::Relaxed);
        shared
            .carry_frames_out
            .fetch_add(n as u64, Ordering::Relaxed);
    }
    next
}

/// The device started after its prefill: it plays, and a resume waiting
/// for that is acknowledged.
fn on_device_started(
    shared: &SharedEngineState,
    resume_ack: &mut bool,
    event_stash: &mut BoundedEventStash<RtEvent, 16>,
    rt_to_ctrl_tx: &mut rtrb::Producer<RtEvent>,
    ctrl_efd: &EventFd,
) {
    shared.set_state(EngineState::Playing);
    if std::mem::take(resume_ack) {
        let _ = event_stash.push_or_stash(RtEvent::Resumed, rt_to_ctrl_tx, ctrl_efd);
    }
}

/// Count the open's landing once: the graph runs at the rate the device
/// was opened for (a graph move it asked for, if any, was followed).
#[inline]
fn note_landing(shared: &SharedEngineState, landed: &mut bool, cur_rate: u32, opened_rate: u32) {
    if !*landed && cur_rate == opened_rate {
        *landed = true;
        shared.graph_landings.fetch_add(1, Ordering::AcqRel);
    }
}

/// Where the RT thread writes output: the ALSA device, or a test double.
#[doc(hidden)]
pub trait PcmSink {
    /// Frames the buffer has room for now (`None` when ALSA cannot say).
    fn room(&mut self) -> Option<usize>;
    /// Write all of `samples`, waiting (bounded) for room; frames written,
    /// `None` when the write failed.
    fn write_all(&mut self, samples: &[i32], stop: &dyn Fn() -> bool) -> Option<usize>;
    /// Write `frames` of digital silence; frames written, `None` when the
    /// write failed.
    fn write_silence(&mut self, frames: usize) -> Option<usize>;
    /// An underrun recovery prepared the device: it must be prefilled and
    /// started again before the next block.
    fn needs_restart(&self) -> bool;
}

impl PcmSink for AlsaOutput {
    fn room(&mut self) -> Option<usize> {
        self.avail().ok().map(|a| a as usize)
    }

    fn write_all(&mut self, samples: &[i32], stop: &dyn Fn() -> bool) -> Option<usize> {
        self.write_i32_all_until(samples, stop).ok()
    }

    fn write_silence(&mut self, frames: usize) -> Option<usize> {
        AlsaOutput::write_silence(self, frames as u32).ok()
    }

    fn needs_restart(&self) -> bool {
        AlsaOutput::needs_restart(self)
    }
}

/// What may follow [`OutCarry::write_ahead`].
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarryNext {
    /// Nothing is carried: write the block.
    Block,
    /// Frames are still carried and the device has no room yet: wait for
    /// it before taking the next block.
    Wait,
    /// The device was prepared again (an underrun): prefill and restart;
    /// the prefill writes the carried frames first.
    Restart,
}

/// Most frames the prefill can carry: fewer than `ceil(up / down)` for any
/// rational pair of engine rates (integer ratios carry none), with room.
pub const CARRY_MAX_FRAMES: usize = 64;

/// Count the open handle's xruns (`now`, [`AlsaOutput::xruns`], which
/// starts at 0 on every open) into the driver's lifetime total, and publish
/// the handle's count for status. `seen` is what was counted of this handle
/// so far (0 for a new one). Real-time safe: atomics only, no logging.
#[inline]
fn note_xruns(shared: &SharedEngineState, now: u64, seen: &mut u64) {
    if now > *seen {
        shared.xrun_events.fetch_add(now - *seen, Ordering::Relaxed);
    }
    *seen = now;
    shared.xruns.store(now, Ordering::Relaxed);
}

/// Close-side [`Option::take`] of the open handle: its xruns not counted
/// yet (an underrun in the tail drain, with no steady iteration after it)
/// are counted first, so none goes unreported. `xruns` reads a handle's
/// count.
#[inline]
fn take_counted<A>(
    handle: &mut Option<A>,
    xruns: impl Fn(&A) -> u64,
    shared: &SharedEngineState,
    seen: &mut u64,
) -> Option<A> {
    if let Some(h) = handle.as_ref() {
        note_xruns(shared, xruns(h), seen);
    }
    handle.take()
}

/// On the control thread: log the underruns the RT thread counted since
/// the last call (`logged`: the total logged so far). Returns how many were
/// new.
fn log_new_xruns(shared: &SharedEngineState, logged: &mut u64) -> u64 {
    let total = shared.xrun_events.load(Ordering::Relaxed);
    let new = total.saturating_sub(*logged);
    if new > 0 {
        log::warn!("ALSA xrun on playback ({new} new, {total} since the driver started)");
        *logged = total;
    }
    new
}

/// On the control thread: log the alsa-lib messages counted on the audio
/// thread (`total`, [`alsa_lib_errors`]) since the last call. Returns how
/// many were new.
fn log_new_alsa_lib_errors(total: u64, logged: &mut u64) -> u64 {
    let new = total.saturating_sub(*logged);
    if new > 0 {
        log::warn!(
            "alsa-lib reported {new} error(s) on the audio thread (not printed there; \
             the driver's own messages say what failed)"
        );
        *logged = total;
    }
    new
}

/// How many whole periods the prefill writes before `start()`: as many as
/// the negotiated buffer holds, at least one. Auto-start is off, so nothing
/// frees room before `start()`; a period past the buffer would overflow the
/// carry. The margin is the carry itself: on a rational grid the periods
/// add up to fewer than `ceil(up / down)` frames more than planned (at most
/// 59 for any pair the driver opens), which it holds until the device runs.
/// A buffer under two periods (a byte-capped USB buffer at 3.072 MHz holds
/// 1.5 periods of 65,536 frames) so starts with one period queued.
pub fn prefill_periods(buffer_frames: u32, period_frames: u32) -> u32 {
    (buffer_frames / period_frames.max(1)).max(1)
}

/// Output frames planned before the device starts that its buffer cannot
/// take yet. On a rational grid the prefill periods add up to a few frames
/// more than the buffer (the planner credits them to the next period), and
/// with auto-start off nothing frees room before `start()`, so they wait
/// here, in order, and go out first once the device runs. Preallocated:
/// never grows on the RT thread.
#[doc(hidden)]
pub struct OutCarry {
    buf: Vec<i32>,
    /// Samples held (interleaved).
    len: usize,
}

impl OutCarry {
    pub fn with_capacity(samples: usize) -> Self {
        Self {
            buf: vec![0; samples],
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Prefill write of `out` (the device is not running): what fits goes
    /// to the device without waiting, after anything already carried; the
    /// rest is carried. `(frames written, frames dropped)`: a drop only if
    /// the carry is full, which a prefill of whole periods never reaches.
    pub fn prime_write<W: PcmSink>(
        &mut self,
        w: &mut W,
        out: &[i32],
        channels: usize,
        stop: &dyn Fn() -> bool,
    ) -> (usize, usize) {
        let ch = channels.max(1);
        let mut room = w.room().unwrap_or(0);
        let mut written = 0;
        if self.len > 0 && room > 0 {
            let frames = (self.len / ch).min(room);
            let n = w.write_all(&self.buf[..frames * ch], stop).unwrap_or(0);
            self.buf.copy_within(n * ch..self.len, 0);
            self.len -= n * ch;
            room -= n;
            written += n;
        }
        let mut rest = out;
        if self.len == 0 && room > 0 {
            let frames = (out.len() / ch).min(room);
            let n = w.write_all(&out[..frames * ch], stop).unwrap_or(0);
            rest = &out[n * ch..];
            written += n;
        }
        let keep = rest.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + keep].copy_from_slice(&rest[..keep]);
        self.len += keep;
        (written, (rest.len() - keep) / ch)
    }

    /// Write what the device has room for of the carried frames, without
    /// waiting; the rest stays, in order. Frames written.
    pub fn flush<W: PcmSink>(
        &mut self,
        w: &mut W,
        channels: usize,
        stop: &dyn Fn() -> bool,
    ) -> usize {
        self.prime_write(w, &[], channels, stop).0
    }

    /// Before a block goes to a running device: the carried frames first.
    /// `(frames written from the carry, what next)`.
    pub fn write_ahead<W: PcmSink>(
        &mut self,
        w: &mut W,
        channels: usize,
        stop: &dyn Fn() -> bool,
    ) -> (usize, CarryNext) {
        if self.is_empty() {
            return (0, CarryNext::Block);
        }
        let n = self.flush(w, channels, stop);
        let next = if w.needs_restart() {
            CarryNext::Restart
        } else if self.is_empty() {
            CarryNext::Block
        } else {
            CarryNext::Wait
        };
        (n, next)
    }
}

/// Outcome of the off-RT DSP chain rebuild triggered by a fresh ALSA
/// negotiation (initial open, reopen, or re-acquire).
///
/// The negotiator always tries to hand back a chain that is valid for the
/// newly negotiated ratio so the resume gate can complete. When the requested
/// chain cannot be built it falls back to a reduced chain and reports the
/// failure in `error`; `fatal` is set only when even that fallback failed, in
/// which case playback must stay suspended in `Error` instead of running a
/// stale or silencing chain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NegotiatedChain {
    /// Swap sequence the resume gate must observe before resuming playback.
    pub required_chain_seq: Option<u64>,
    /// Chain rebuild error, also present when a fallback chain keeps audio flowing.
    pub error: Option<String>,
    /// No chain valid for the negotiated ratio could be built.
    pub fatal: bool,
    /// Config generation this chain was built from. A success for this
    /// generation (or newer) clears a previously recorded fallback warning,
    /// while a stale success can never clear a newer failure.
    pub generation: u64,
    /// The engine's config has moved to another device since this open
    /// began (its config change is queued behind it): the engine did not
    /// take the card (no mixer write, nothing built), so the driver closes
    /// it without resuming and lets the queued change reopen.
    pub superseded: bool,
    /// Direct or DSP off opened away from x1 (the engine bypass
    /// resampler), and its chain could not be built: the driver drops the
    /// pair from the bypass mask and reopens into the PipeWire path (the
    /// graph moves to the native rate) before resuming
    /// ([`crate::lifecycle::open_with_bypass_fallback`]).
    pub bypass_unbuildable: bool,
    /// As `bypass_unbuildable`, but the chain's design is still running
    /// (off the negotiation): the pair falls back to PipeWire until the
    /// engine reports the design done ([`BypassNotifier::designed`]).
    pub bypass_pending: bool,
    /// The engine's design epoch the negotiation ran in, for a bypass miss
    /// ([`crate::lifecycle::BypassMiss::epoch`]).
    pub bypass_epoch: u64,
}

impl NegotiatedChain {
    /// The pair whose engine bypass resampler chain this open misses.
    pub fn bypass_miss(&self, input: u32, output: u32) -> Option<crate::lifecycle::BypassMiss> {
        (self.bypass_unbuildable || self.bypass_pending).then_some(crate::lifecycle::BypassMiss {
            input,
            output,
            pending: !self.bypass_unbuildable,
            epoch: self.bypass_epoch,
        })
    }
}

/// Tells the driver about the engine bypass resampler's designs
/// ([`EngineDriverHandle::bypass_notifier`]): a pair that fell back to
/// PipeWire while its design ran comes back (or stays out, when it
/// failed), and holds from a superseded design epoch are released
/// ([`crate::lifecycle::BypassNote`]).
#[derive(Clone)]
pub struct BypassNotifier {
    tx: mpsc::Sender<CtrlReq>,
    ctrl_efd: Arc<EventFd>,
}

impl BypassNotifier {
    pub fn send(&self, note: crate::lifecycle::BypassNote) {
        let _ = self.tx.send(CtrlReq::Bypass(note));
        self.ctrl_efd.signal();
    }

    pub fn designed(&self, input: u32, output: u32, ok: bool) {
        self.send(crate::lifecycle::BypassNote::Designed { input, output, ok });
    }
}

/// Callback invoked off the RT thread right after ALSA opens/reopens
/// `device` at `(graph_rate, actual_ratio, sink, negotiated format, the
/// open's key)` and before `Resume` is sent to the RT thread. The device is
/// the one actually opened: the engine's config may have moved on in the
/// meantime (see [`NegotiatedChain::superseded`]). The key is what the RT
/// thread predicts later opens from ([`DesiredDspConfig::open_landing`]).
///
/// Returns the swap sequence the resume gate must wait for (when the engine
/// queued a chain swap) plus any rebuild failure that was recovered from with a
/// fallback chain.
pub type RatioNegotiator = Arc<
    dyn Fn(&str, u32, RateRatio, SinkChoice, SampleFormat, &NegotiatedKey) -> NegotiatedChain
        + Send
        + Sync,
>;

/// Run on the control thread whenever the driver stops holding a card,
/// with that card's device. The engine uses it to leave the card's hardware
/// volume at a safe level for the program that gets the DAC next, and to
/// stop writing the mixer; a device it does not hold is no business of it.
pub type ReleaseHook = Arc<dyn Fn(ReleaseKind, &str) + Send + Sync>;

/// How the DAC is let go (see [`ReleaseHook`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseKind {
    /// Playback has stopped and the card is still ours (reservation held):
    /// an explicit release, a ReserveDevice1 request, the driver stopping,
    /// or a failed reconnect. The hook may still write the card.
    HandOver,
    /// The card was already let go (a failed open or reopen released the
    /// reservation): the hook must not write it.
    Lost,
}

/// From the first negotiation of [`EngineDriverHandle::spawn`] on, the
/// engine holds the card (and may have written its mixer): a start-up that
/// fails after it (PipeWire registration, a thread spawn) hands the card
/// over through the release hook before ALSA and the reservation go. Runs
/// at most once: explicitly, or when dropped while still armed.
struct InitHandOver {
    shared: Arc<SharedEngineState>,
    device: String,
    armed: bool,
}

impl InitHandOver {
    fn new(shared: Arc<SharedEngineState>, device: String) -> Self {
        Self {
            shared,
            device,
            armed: true,
        }
    }

    fn run(&mut self) {
        if std::mem::take(&mut self.armed) {
            run_release_hook(&self.shared, ReleaseKind::HandOver, &self.device);
        }
    }

    /// The driver runs: from now on its own paths hand the card over.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for InitHandOver {
    fn drop(&mut self) {
        self.run();
    }
}

/// `release_device`'s error when the teardown is slower than its wait.
const RELEASE_TIMEOUT_MSG: &str = "release_device timed out waiting for control teardown";

fn run_release_hook(shared: &SharedEngineState, kind: ReleaseKind, device: &str) {
    let hook = shared
        .before_release
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(kind, device);
    }
}

/// DAC rates worth probing: every engine graph rate times every ratio up to
/// `max_ratio`, capped at [`MAX_OUTPUT_RATE`], kept to the rates a ladder
/// can offer ([`crate::lifecycle::MASK_OUTPUT_RATES`]: every multiple up to
/// 768 kHz, then the x32 and x64 rates 1.4112, 1.536, 2.8224 and
/// 3.072 MHz).
pub fn engine_probe_rates(max_ratio: u32) -> Vec<u32> {
    probe_rates(ALLOWED_RATES, max_ratio.max(1), MAX_OUTPUT_RATE)
        .into_iter()
        .filter(|r| crate::lifecycle::MASK_OUTPUT_RATES.contains(r))
        .collect()
}

/// Where the DSP sink lands for `config` on a device with `caps`: for each
/// graph rate the driver offers PipeWire (the configured rates the device
/// can play, widened to the engine's rates when none can, exactly as the
/// driver computes its policy), the ratio a fresh open picks (the first
/// buildable ladder entry the device plays in a configured format,
/// [`DesiredDspConfig::open_landing`]). Rates where no ladder entry plays
/// are left out. Without `caps` (no probe), the configured rates and each
/// one's first ladder entry. `Err`: the device plays none of the engine's
/// rates. The control plane prices a design at these landings and checks
/// that a fallback can play at all.
pub fn predicted_dsp_landings(
    config: &EngineDriverConfig,
    caps: Option<&DeviceCapabilities>,
) -> std::result::Result<Vec<(u32, RateRatio)>, String> {
    let desired = config.desired_dsp(0);
    let policy = compute_effective_policy(config, desired, caps)?;
    let rates = DeviceRates::playable(caps, &config.formats);
    Ok(policy
        .allowed_rates
        .iter()
        .filter_map(|&rate| {
            desired
                .open_landing(
                    SinkChoice::Dsp,
                    rate,
                    rates,
                    &crate::lifecycle::Rungs::EMPTY,
                )
                .map(|r| (rate, r))
        })
        .collect())
}

/// The rate policy the driver runs with for `caps`: the configured one
/// restricted to rates the device can play. Without a probe result
/// (probe failed) the configured policy is used as is.
fn compute_effective_policy(
    config: &EngineDriverConfig,
    desired: DesiredDspConfig,
    caps: Option<&DeviceCapabilities>,
) -> std::result::Result<RatePolicy, String> {
    let Some(caps) = caps else {
        return Ok(config.rate_policy.clone());
    };
    effective_rate_policy(
        &config.rate_policy,
        caps,
        desired,
        &config.formats,
        ALLOWED_RATES,
    )
    .map(|eff| {
        if !eff.unsupported.is_empty() || eff.widened {
            log::debug!(
                "Rates for '{}': offering {:?} (default {}); device cannot play {:?}{}",
                caps.device,
                eff.policy.allowed_rates,
                eff.policy.default_rate,
                eff.unsupported,
                if eff.widened {
                    "; none of the configured rates fit, using the device's"
                } else {
                    ""
                }
            );
        }
        eff.policy
    })
}

/// Lightweight handle to wake the ALSA RT thread from background helper threads
/// (e.g. the chain reaper flushing deferred publications) without holding a
/// full [`EngineDriverHandle`].
#[derive(Clone)]
pub struct DriverWaker {
    cmd_efd: Arc<EventFd>,
}

impl DriverWaker {
    /// Wake the ALSA RT thread immediately so it polls lock-free queues.
    pub fn wake_rt(&self) {
        self.cmd_efd.signal();
    }
}

/// Shows the engine's volume on our sinks and switches sink volume
/// ownership on or off ([`crate::sink_volume`]).
pub struct SinkVolumePort {
    shared: Arc<SinkVolumeShared>,
    pw_tx: Option<pw::channel::Sender<PwMsg>>,
}

impl SinkVolumePort {
    /// `follow`: own the sink volume and show `view` on `audivane_dsp`
    /// (`audivane_direct` shows unity with `view.muted`). Turning `follow`
    /// off puts both sinks back to plain unity once.
    pub fn set(&self, follow: bool, view: SinkVolumeView) {
        let follow_changed = self.shared.set(follow, view);
        if (follow || follow_changed)
            && let Some(tx) = &self.pw_tx
        {
            let _ = tx.send(PwMsg::SinkVolume { follow_changed });
        }
    }
}

/// Configuration for [`EngineDriverHandle`].
#[derive(Clone, Debug)]
pub struct EngineDriverConfig {
    pub device: String,
    pub formats: Vec<SampleFormat>,
    pub channels: u32,
    /// PipeWire quantum at 48 kHz (scaled for other sample rates).
    pub quantum: u32,
    pub buffer_periods: u32,
    /// Whether DSP processing (including upsampling) is enabled.
    pub dsp_enabled: bool,
    /// Upsampling target resolved against the actual PipeWire graph rate on
    /// every open/reopen.
    pub dsp_rate_target: RateTarget,
    /// Upper clamp for resolved upsampling ratios (`>= 1`).
    pub max_dsp_ratio: u32,
    /// The rate pairs the DSP chain can be built for (ratio ladders skip
    /// the others).
    pub dsp_buildable: crate::lifecycle::RatePairMask,
    /// The rate pairs the resample-only chain of the engine bypass
    /// resampler can be built for (`rate_policy.bypass_resampler: engine`).
    pub bypass_buildable: crate::lifecycle::RatePairMask,
    pub rate_policy: RatePolicy,
    pub reserve_device: bool,
    pub reserve_priority: i32,
    pub rt_priority: u32,
    /// `Some(view)`: own the sink volume from the start (see
    /// [`crate::sink_volume`]); later changes go through
    /// [`EngineDriverHandle::sink_volume_port`].
    pub sink_volume: Option<SinkVolumeView>,
}

impl EngineDriverConfig {
    pub fn desired_dsp(&self, generation: u64) -> DesiredDspConfig {
        DesiredDspConfig {
            dsp_enabled: self.dsp_enabled,
            rate_target: self.dsp_rate_target,
            max_ratio: self.max_dsp_ratio.max(1),
            generation,
            buildable: self.dsp_buildable,
            bypass: crate::lifecycle::BypassLeg {
                engine: self.rate_policy.bypass_resampler == BypassResampler::Engine,
                buildable: self.bypass_buildable,
                allowed: crate::lifecycle::BypassLeg::allowed_bits(&self.rate_policy.allowed_rates),
            },
        }
    }
}

impl Default for EngineDriverConfig {
    fn default() -> Self {
        Self {
            device: "hw:Loopback,0,0".to_string(),
            formats: vec![
                SampleFormat::S32,
                SampleFormat::S24,
                SampleFormat::S24_3,
                SampleFormat::S16,
            ],
            channels: 2,
            quantum: 1024,
            buffer_periods: 3,
            dsp_enabled: true,
            dsp_rate_target: RateTarget::Off,
            max_dsp_ratio: 16,
            dsp_buildable: crate::lifecycle::RatePairMask::ALL,
            bypass_buildable: crate::lifecycle::RatePairMask::NONE,
            rate_policy: RatePolicy::default(),
            reserve_device: true,
            reserve_priority: DEFAULT_RESERVE_PRIORITY,
            rt_priority: 20,
            sink_volume: None,
        }
    }
}

/// Cumulative audio-gap telemetry (all counters are plain relaxed atomics, so
/// they are safe to bump from the RT thread). They are never reset while the
/// engine runs; consumers diff successive snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DriverGapCounters {
    /// Steady-state periods on the Direct sink where the ring held fewer
    /// samples than one ALSA period, so zeros were substituted.
    pub direct_zero_periods: u64,
    /// Frames zero-filled on the Direct sink in steady state.
    pub direct_zero_frames: u64,
    /// Same for the DSP sink.
    pub dsp_zero_periods: u64,
    pub dsp_zero_frames: u64,
    /// Steady-state periods where the active sink's ring ran short (alias of
    /// the two counters above, kept for total "gaps" accounting).
    pub ring_underruns: u64,
    /// PipeWire process cycles where the Direct/DSP sink had no buffer (or a
    /// zero-length one) while at least one client was linked to it.
    pub direct_short_buffers: u64,
    pub dsp_short_buffers: u64,
    /// PipeWire process cycles that delivered an all-zero block to a sink with
    /// links (upstream starved and PipeWire filled silence).
    pub direct_zero_buffers: u64,
    pub dsp_zero_buffers: u64,
    /// Frames lost because the ring was full when a PipeWire block arrived.
    pub direct_dropped_frames: u64,
    pub dsp_dropped_frames: u64,
    /// Frames pushed into the ring by PipeWire vs. popped by the RT thread.
    pub direct_pushed_frames: u64,
    pub dsp_pushed_frames: u64,
    pub direct_popped_frames: u64,
    pub dsp_popped_frames: u64,
    /// Frames discarded by `drain_ring` while starting/idle, split by reason.
    pub direct_drained_prime: u64,
    pub dsp_drained_prime: u64,
    pub direct_drained_idle: u64,
    pub dsp_drained_idle: u64,
    pub direct_drained_unused: u64,
    pub dsp_drained_unused: u64,
    /// ALSA reopen/rate-switch/fade bookkeeping.
    pub reopens: u64,
    pub resumes: u64,
    pub rate_switches: u64,
    pub fade_outs: u64,
    pub fade_ins: u64,
    /// Times the engine stopped the DAC mid-stream because PipeWire reported no
    /// active sink (want_active false), plus the silence periods it padded into
    /// ALSA while draining the tail.
    pub idle_stops: u64,
    pub drain_silence_periods: u64,
    /// Silence periods written while priming a fresh ALSA start.
    pub prime_silence_periods: u64,
    /// Frames written from the prefill carry once the device ran (also in
    /// `frames_out`).
    pub carry_frames_out: u64,
    /// PipeWire sink state transitions.
    pub stream_starts_direct: u64,
    pub stream_starts_dsp: u64,
    pub stream_pauses_direct: u64,
    pub stream_pauses_dsp: u64,
    /// Overloads that moved the audio thread from real-time to normal
    /// priority: it ran to the `RLIMIT_RTTIME` soft limit without sleeping
    /// (`SIGXCPU`) and the demotion took. Only these are overloads.
    pub rt_overloads: u64,
    /// `SIGXCPU` signals the audio thread saw (a signal that finds it at
    /// normal priority already moves only this).
    pub rt_signals: u64,
    /// Demotions of the real-time audio thread the kernel refused.
    pub rt_failed_demotions: u64,
    /// The chain generation that ran on the audio thread at the last
    /// `rt_overloads` demotion ([`EngineAudioProcessor::running_generation`],
    /// published before the count moves).
    pub rt_overload_generation: u64,
    /// The varispeed hold's sequence the audio thread applied at that
    /// demotion (published with it).
    pub rt_overload_hold_seq: u64,
    /// Probes of the device that answered (its capabilities read), since
    /// the driver started: a failed open is followed by one, so a count
    /// that moved since an attempt began says the device was there for it.
    pub probes: u64,
    /// Short naps the audio thread took because it had not slept for
    /// 50 ms (the chain costs more than real time).
    pub rt_naps: u64,
    /// 1 while the audio thread is demoted after an overload (not
    /// real-time again yet), else 0.
    pub rt_demoted: u64,
    /// 1 while the DSP sink should drive the DAC (a client streams to it:
    /// playback is requested there), else 0.
    pub dsp_wanted: u64,
    /// Underrun recoveries since the driver started, across reopens
    /// (`xruns` is the open handle's own count and restarts at 0).
    pub xrun_events: u64,
    /// Steady-state periods processed and written since the driver
    /// started: with `xrun_events`, the share of failed periods.
    pub processed_periods: u64,
}

/// Point-in-time driver telemetry used by `RealEngine::snapshot()`.
#[derive(Clone, Debug)]
pub struct EngineDriverSnapshot {
    pub state: EngineState,
    pub error: Option<String>,
    pub format: Option<SampleFormat>,
    pub graph_rate: u32,
    pub output_rate: u32,
    pub ratio: RateRatio,
    pub quantum: u32,
    pub alsa_delay_frames: i64,
    pub xruns: u64,
    pub ring_underruns: u64,
    pub dsp_load: f64,
    pub active_sink: Option<SinkChoice>,
    pub dsp_node: Option<u32>,
    pub direct_node: Option<u32>,
    pub frames_out: u64,
    pub dsp_latency_out_frames: usize,
    pub gaps: DriverGapCounters,
    /// Probed capabilities of the open device.
    pub device_caps: Option<DeviceCapabilities>,
    /// What the last open found ([`NegotiatedKey`]): the reopen decisions
    /// predict a fresh open from it. `None` before the first open.
    pub open_key: Option<NegotiatedKey>,
    /// The playback session now: [`NegotiatedKey::in_session`] with it is
    /// what the RT thread predicts from.
    pub playback_session: u64,
    /// Rate policy actually published to PipeWire (configured policy
    /// restricted to the device). `None` before the first open.
    pub effective_policy: Option<RatePolicy>,
    /// Graph rates the Direct sink plays bit-perfect (native x1 rates among
    /// the effective ones). Empty while no device is open.
    pub direct_rates: Vec<u32>,
    /// Varispeed scheduling: cap, hold, underfeed and cycle counters.
    pub varispeed: VarispeedDriverSnapshot,
    /// The largest processing time of one period over the period's length,
    /// over the last one to two seconds (stage, chains, swaps and A/B).
    pub rt_load_max: f64,
    /// The configured device is not there: the sinks stay up and run on
    /// the engine's own clock (audio is discarded) until it comes back.
    pub waiting_for_device: bool,
}

fn state_to_u8(s: EngineState) -> u8 {
    match s {
        EngineState::Idle => 0,
        EngineState::Ready => 1,
        EngineState::Playing => 2,
        EngineState::Switching => 3,
        EngineState::Error => 4,
    }
}

fn state_from_u8(v: u8) -> EngineState {
    match v {
        0 => EngineState::Idle,
        1 => EngineState::Ready,
        2 => EngineState::Playing,
        3 => EngineState::Switching,
        _ => EngineState::Error,
    }
}

fn format_to_u8(f: Option<SampleFormat>) -> u8 {
    match f {
        None => 0,
        Some(SampleFormat::S16) => 1,
        Some(SampleFormat::S24) => 2,
        Some(SampleFormat::S24_3) => 3,
        Some(SampleFormat::S32) => 4,
    }
}

fn format_from_u8(v: u8) -> Option<SampleFormat> {
    match v {
        1 => Some(SampleFormat::S16),
        2 => Some(SampleFormat::S24),
        3 => Some(SampleFormat::S24_3),
        4 => Some(SampleFormat::S32),
        _ => None,
    }
}

fn sink_to_u8(s: Option<SinkChoice>) -> u8 {
    match s {
        None => 0,
        Some(SinkChoice::Dsp) => 1,
        Some(SinkChoice::Direct) => 2,
        Some(SinkChoice::Default) => 3,
    }
}

fn sink_from_u8(v: u8) -> Option<SinkChoice> {
    match v {
        1 => Some(SinkChoice::Dsp),
        2 => Some(SinkChoice::Direct),
        3 => Some(SinkChoice::Default),
        _ => None,
    }
}

/// Scale a 48 kHz base quantum to `rate` the way PipeWire does, so the ALSA
/// period the engine opens with is the quantum the graph actually runs.
///
/// PipeWire turns `node.latency = q/rate` into `q * graph_rate / rate` and,
/// with `clock.power-of-two-quantum` (the default), rounds that *down* to a
/// power of two. So a 1024 quantum runs 512 at 44.1 kHz, 1024 at 88.2 kHz,
/// 2048 at 176.4 kHz and 4096 at 352.8 kHz, never 1881 or 3763. A period
/// that does not match the quantum costs several back-to-back cycles per
/// period and the source clients cannot keep up.
///
/// The result is also capped at PipeWire's default `clock.quantum-limit`
/// (8192): at 768 kHz a 1024 quantum would be 16384, but the graph runs 8192.
pub fn scaled_quantum(q48: u32, rate: u32) -> u32 {
    const PW_QUANTUM_LIMIT: u64 = 8192;
    let q48 = q48.max(32);
    let r = rate.max(8_000) as u64;
    let raw = ((q48 as u64 * r) / 48_000).clamp(32, PW_QUANTUM_LIMIT) as u32;
    if q48.is_power_of_two() {
        (1u32 << (31 - raw.leading_zeros())).max(32)
    } else {
        raw
    }
}

#[derive(Clone, Copy)]
struct SendRawStream(NonNull<pw::sys::pw_stream>);

// Safety: `pw_stream_trigger_process` is thread-safe, and `EngineDriverHandle::stop`
// joins the ALSA thread before streams are dropped on the PipeWire thread.
unsafe impl Send for SendRawStream {}
unsafe impl Sync for SendRawStream {}

struct SharedEngineState {
    io: SynchronizedIo,
    state: AtomicU8,
    format: AtomicU8,
    active_sink: AtomicU8,
    dsp_streaming: AtomicBool,
    direct_streaming: AtomicBool,
    driving: AtomicBool,
    /// Set by PipeWire Registry when >= 1 running node is linked to Direct.
    direct_client_running: AtomicBool,
    /// Set by PipeWire Registry when >= 1 node (any state) is linked to Direct.
    direct_has_links: AtomicBool,
    /// Set by PipeWire Registry when >= 1 node is linked to the DSP sink.
    dsp_has_links: AtomicBool,
    /// Set by PipeWire Registry when >= 1 running node is linked to the DSP
    /// sink.
    dsp_client_running: AtomicBool,
    /// Native sample rate tracked for active Direct/DSP clients (0 = none).
    desired_client_rate: AtomicU32,
    stop: AtomicBool,
    release_requested: AtomicBool,
    dsp_node_id: AtomicU32,
    direct_node_id: AtomicU32,
    graph_rate: AtomicU32,
    output_rate: AtomicU32,
    /// [`RateRatio::to_bits`] of the negotiated ratio.
    ratio: AtomicU64,
    quantum: AtomicU32,
    alsa_delay_frames: AtomicI64,
    xruns: AtomicU64,
    /// Underrun recoveries since the driver started, across reopens (the
    /// RT thread adds them, [`note_xruns`]; the control thread logs them,
    /// [`log_new_xruns`]).
    xrun_events: AtomicU64,
    ring_underruns: AtomicU64,
    direct_zero_periods: AtomicU64,
    direct_zero_frames: AtomicU64,
    dsp_zero_periods: AtomicU64,
    dsp_zero_frames: AtomicU64,
    direct_short_buffers: AtomicU64,
    dsp_short_buffers: AtomicU64,
    direct_zero_buffers: AtomicU64,
    dsp_zero_buffers: AtomicU64,
    direct_dropped_frames: AtomicU64,
    dsp_dropped_frames: AtomicU64,
    direct_cycles_completed: AtomicU64,
    direct_pushed_frames: AtomicU64,
    dsp_pushed_frames: AtomicU64,
    direct_popped_frames: AtomicU64,
    dsp_popped_frames: AtomicU64,
    direct_drained_prime: AtomicU64,
    dsp_drained_prime: AtomicU64,
    direct_drained_idle: AtomicU64,
    dsp_drained_idle: AtomicU64,
    direct_drained_unused: AtomicU64,
    dsp_drained_unused: AtomicU64,
    reopens: AtomicU64,
    /// Device probes that answered ([`DriverGapCounters::probes`]).
    probes: AtomicU64,
    resumes: AtomicU64,
    rate_switches: AtomicU64,
    fade_outs: AtomicU64,
    fade_ins: AtomicU64,
    idle_stops: AtomicU64,
    drain_silence_periods: AtomicU64,
    prime_silence_periods: AtomicU64,
    carry_frames_out: AtomicU64,
    stream_starts_direct: AtomicU64,
    stream_starts_dsp: AtomicU64,
    stream_pauses_direct: AtomicU64,
    stream_pauses_dsp: AtomicU64,
    /// The audio thread's overload guard ([`crate::rt_overload`]).
    rt: crate::rt_overload::RtOverloadState,
    frames_out: AtomicU64,
    dsp_load_bits: AtomicU64,
    dsp_latency_out: AtomicU64,
    cycles_completed: AtomicU64,
    last_error: Mutex<Option<String>>,
    metadata_warning: Mutex<Option<String>>,
    device_caps: Mutex<Option<DeviceCapabilities>>,
    open_key: Mutex<Option<NegotiatedKey>>,
    /// Opens whose graph the RT thread saw at the rate it opened for (once
    /// per open): a pending graph move landed ([`MoveAttempts`]).
    graph_landings: AtomicU64,
    /// The playback session the RT thread is in ([`PlaybackSessions`]).
    playback_session: AtomicU64,
    effective_policy: Mutex<Option<RatePolicy>>,
    /// Each sink's rates and native base on the open device
    /// ([`crate::rate_caps::RatePolicies`]): Direct runs the graph at native
    /// x1 rates and the ones the engine bypass resampler converts, and the
    /// DSP sink's clients map through its own rates. Read by the PipeWire
    /// thread's rate arbiter.
    sink_policies: Mutex<Option<crate::rate_caps::RatePolicies>>,
    /// Recovered chain-rebuild failure plus the config generation that produced
    /// it. Kept with a generation so a success from an older chain build cannot
    /// clear a warning recorded by a newer one.
    chain_warning: Mutex<ChainWarningState>,
    /// Sink volume ownership (PipeWire thread + control threads).
    sink_volume: Arc<SinkVolumeShared>,
    before_release: Mutex<Option<ReleaseHook>>,
    /// The latest varispeed speed command (control side writes, the RT
    /// thread reads at a DAC wakeup).
    varispeed_cmd: VarispeedCommandSlot,
    /// What the RT thread's varispeed scheduling publishes.
    varispeed: VarispeedDriverStatus,
    /// `rt_load_max`: the largest processing time of one period over the
    /// period's length, over the last one to two seconds (f64 bits).
    rt_load_max_bits: AtomicU64,
    /// The configured device is not there (unplugged, or missing at
    /// start-up). With no output open, the RT thread then runs the graph on
    /// its own clock and discards what the sinks receive, so streams
    /// neither stall nor move to another output. Written by the control
    /// thread.
    device_missing: AtomicBool,
    /// Bumped by the control thread to make the RT thread retry the open
    /// while it has no output (the device came back, or a retry is due).
    device_retry: AtomicU64,
    /// The open device is no longer in the system ([`PresenceWatch`]): the
    /// RT thread hands its output back for a reopen, also while nothing
    /// plays (when ALSA reports nothing). Written by the control thread.
    device_gone: AtomicBool,
}

struct ChainWarning {
    message: String,
    generation: u64,
}

/// Fallback warning plus the newest config generation known to have been
/// published successfully. Both live under one mutex so a failure from an
/// older build that is recorded late (after a newer success already cleared
/// the warning) is rejected instead of resurrecting a stale message.
#[derive(Default)]
struct ChainWarningState {
    warning: Option<ChainWarning>,
    cleared_through: Option<u64>,
}

impl SharedEngineState {
    /// The sink that should drive the DAC now, `None` when neither has a
    /// stream: Direct when a running client feeds it
    /// ([`Self::session_streams`]), else the DSP sink while its node streams.
    fn pick_sink(&self) -> Option<SinkChoice> {
        let (_, direct) = self.session_streams();
        if direct {
            Some(SinkChoice::Direct)
        } else if self.dsp_streaming.load(Ordering::Acquire) {
            Some(SinkChoice::Dsp)
        } else {
            None
        }
    }

    /// Whether each sink (DSP, Direct) has a stream for the playback
    /// sessions ([`PlaybackSessions::observe`]): its node streams and a
    /// running client feeds it. The node's flag alone is not enough (the
    /// driver group runs both nodes while either sink's stream drives it),
    /// and neither is a link (a paused client stays linked), so a paused
    /// client never keeps a playing one on the other sink silent.
    fn session_streams(&self) -> (bool, bool) {
        let fed = |streaming: &AtomicBool, running: &AtomicBool| {
            streaming.load(Ordering::Acquire) && running.load(Ordering::Acquire)
        };
        (
            fed(&self.dsp_streaming, &self.dsp_client_running),
            fed(&self.direct_streaming, &self.direct_client_running),
        )
    }

    fn new(cfg: &EngineDriverConfig) -> Self {
        let init_rate = cfg.rate_policy.default_rate.max(8_000);
        let init_q = scaled_quantum(cfg.quantum, init_rate);
        let init_ratio = cfg
            .desired_dsp(0)
            .resolve_ratio_for(SinkChoice::Dsp, init_rate);
        Self {
            io: SynchronizedIo::new(),
            state: AtomicU8::new(state_to_u8(EngineState::Ready)),
            format: AtomicU8::new(0),
            active_sink: AtomicU8::new(0),
            dsp_streaming: AtomicBool::new(false),
            direct_streaming: AtomicBool::new(false),
            driving: AtomicBool::new(false),
            direct_client_running: AtomicBool::new(false),
            direct_has_links: AtomicBool::new(false),
            dsp_has_links: AtomicBool::new(false),
            dsp_client_running: AtomicBool::new(false),
            desired_client_rate: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            release_requested: AtomicBool::new(false),
            dsp_node_id: AtomicU32::new(pw::constants::ID_ANY),
            direct_node_id: AtomicU32::new(pw::constants::ID_ANY),
            graph_rate: AtomicU32::new(init_rate),
            output_rate: AtomicU32::new(init_ratio.output_rate(init_rate).unwrap_or(init_rate)),
            ratio: AtomicU64::new(init_ratio.to_bits()),
            quantum: AtomicU32::new(init_q),
            alsa_delay_frames: AtomicI64::new(0),
            xruns: AtomicU64::new(0),
            xrun_events: AtomicU64::new(0),
            ring_underruns: AtomicU64::new(0),
            direct_zero_periods: AtomicU64::new(0),
            direct_zero_frames: AtomicU64::new(0),
            dsp_zero_periods: AtomicU64::new(0),
            dsp_zero_frames: AtomicU64::new(0),
            direct_short_buffers: AtomicU64::new(0),
            dsp_short_buffers: AtomicU64::new(0),
            direct_zero_buffers: AtomicU64::new(0),
            dsp_zero_buffers: AtomicU64::new(0),
            direct_dropped_frames: AtomicU64::new(0),
            dsp_dropped_frames: AtomicU64::new(0),
            direct_cycles_completed: AtomicU64::new(0),
            direct_pushed_frames: AtomicU64::new(0),
            dsp_pushed_frames: AtomicU64::new(0),
            direct_popped_frames: AtomicU64::new(0),
            dsp_popped_frames: AtomicU64::new(0),
            direct_drained_prime: AtomicU64::new(0),
            dsp_drained_prime: AtomicU64::new(0),
            direct_drained_idle: AtomicU64::new(0),
            dsp_drained_idle: AtomicU64::new(0),
            direct_drained_unused: AtomicU64::new(0),
            dsp_drained_unused: AtomicU64::new(0),
            reopens: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            resumes: AtomicU64::new(0),
            rate_switches: AtomicU64::new(0),
            fade_outs: AtomicU64::new(0),
            fade_ins: AtomicU64::new(0),
            idle_stops: AtomicU64::new(0),
            drain_silence_periods: AtomicU64::new(0),
            prime_silence_periods: AtomicU64::new(0),
            carry_frames_out: AtomicU64::new(0),
            stream_starts_direct: AtomicU64::new(0),
            stream_starts_dsp: AtomicU64::new(0),
            stream_pauses_direct: AtomicU64::new(0),
            stream_pauses_dsp: AtomicU64::new(0),
            rt: Default::default(),
            frames_out: AtomicU64::new(0),
            dsp_load_bits: AtomicU64::new(0.0_f64.to_bits()),
            dsp_latency_out: AtomicU64::new(0),
            cycles_completed: AtomicU64::new(0),
            last_error: Mutex::new(None),
            metadata_warning: Mutex::new(None),
            device_caps: Mutex::new(None),
            open_key: Mutex::new(None),
            graph_landings: AtomicU64::new(0),
            playback_session: AtomicU64::new(0),
            effective_policy: Mutex::new(None),
            sink_policies: Mutex::new(None),
            chain_warning: Mutex::new(ChainWarningState::default()),
            sink_volume: Arc::new(SinkVolumeShared::new(cfg.sink_volume)),
            before_release: Mutex::new(None),
            varispeed_cmd: VarispeedCommandSlot::default(),
            varispeed: VarispeedDriverStatus::default(),
            rt_load_max_bits: AtomicU64::new(0.0_f64.to_bits()),
            device_missing: AtomicBool::new(false),
            device_retry: AtomicU64::new(0),
            device_gone: AtomicBool::new(false),
        }
    }

    fn set_open_key(&self, key: NegotiatedKey) {
        if let Ok(mut g) = self.open_key.lock() {
            *g = Some(key);
        }
    }

    fn set_device_caps(&self, caps: Option<DeviceCapabilities>) {
        if let Ok(mut g) = self.device_caps.lock() {
            *g = caps;
        }
    }

    /// A probe of the device answered (its capabilities were read).
    fn note_probe(&self) {
        self.probes.fetch_add(1, Ordering::AcqRel);
    }

    fn set_effective_policy(&self, policy: Option<RatePolicy>) {
        if let Ok(mut g) = self.effective_policy.lock() {
            *g = policy;
        }
    }

    fn sink_policies(&self) -> Option<crate::rate_caps::RatePolicies> {
        self.sink_policies.lock().ok().and_then(|g| g.clone())
    }

    fn direct_policy(&self) -> Option<RatePolicy> {
        self.sink_policies().map(|p| p.direct)
    }

    /// Drop the rate policies (no device, or an unusable one).
    fn clear_policies(&self) {
        self.set_effective_policy(None);
        if let Ok(mut g) = self.sink_policies.lock() {
            *g = None;
        }
    }

    /// Publish the policies the driver runs with on the open device.
    fn set_policies(&self, policies: &crate::rate_caps::RatePolicies) {
        self.set_effective_policy(Some(policies.published.clone()));
        if let Ok(mut g) = self.sink_policies.lock() {
            *g = Some(policies.clone());
        }
    }

    /// The output is closed (open failed, reopen aborted, released): drop
    /// what was negotiated with it (format, hence dither depth, and the
    /// effective rates) so the status never shows the previous device's
    /// values. Capabilities stay only when they belong to `device` (the
    /// target was probed, possibly unusable).
    fn clear_negotiation(&self, device: &str) {
        self.format.store(0, Ordering::Release);
        self.clear_policies();
        if let Ok(mut g) = self.device_caps.lock()
            && g.as_ref().is_some_and(|c| c.device != device)
        {
            *g = None;
        }
    }

    fn set_state(&self, s: EngineState) {
        self.state.store(state_to_u8(s), Ordering::Release);
    }

    fn state(&self) -> EngineState {
        state_from_u8(self.state.load(Ordering::Acquire))
    }

    fn set_error(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(msg.into());
        }
        self.set_state(EngineState::Error);
    }

    fn clear_error(&self) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = None;
        }
    }

    /// Clear the error only when it is `msg` (a later, different error
    /// stays).
    fn clear_error_if(&self, msg: &str) {
        if let Ok(mut g) = self.last_error.lock()
            && g.as_deref() == Some(msg)
        {
            *g = None;
        }
    }

    fn set_metadata_warning(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.metadata_warning.lock() {
            *g = Some(msg.into());
        }
    }

    fn clear_metadata_warning(&self) {
        if let Ok(mut g) = self.metadata_warning.lock() {
            *g = None;
        }
    }

    /// Record a recovered chain-rebuild failure without flipping the engine
    /// state: audio keeps playing on a fallback chain and the message still
    /// shows up in the snapshot `error` field.
    fn set_chain_warning(&self, msg: impl Into<String>, generation: u64) {
        if let Ok(mut g) = self.chain_warning.lock() {
            // A strictly newer generation already published successfully: the
            // fallback this failure describes is no longer what is playing.
            if g.cleared_through.is_some_and(|ok| ok > generation) {
                return;
            }
            g.warning = Some(ChainWarning {
                message: msg.into(),
                generation,
            });
        }
    }

    /// Clear a fallback warning once a replacement chain built from
    /// `generation` (or newer) has been published. A stale success from an
    /// older generation never clears a newer failure.
    fn clear_chain_warning_through(&self, generation: u64) {
        if let Ok(mut g) = self.chain_warning.lock() {
            g.cleared_through = Some(g.cleared_through.map_or(generation, |c| c.max(generation)));
            if g.warning
                .as_ref()
                .is_some_and(|warning| warning.generation <= generation)
            {
                g.warning = None;
            }
        }
    }

    fn clear_chain_warning(&self) {
        if let Ok(mut g) = self.chain_warning.lock() {
            g.warning = None;
        }
    }

    /// Per-sink counter selection for the process-callback telemetry.
    #[inline]
    fn cycle_counter(&self, is_direct: bool) -> &AtomicU64 {
        if is_direct {
            &self.direct_cycles_completed
        } else {
            &self.cycles_completed
        }
    }

    #[inline]
    fn note_short_buffer(&self, is_direct: bool) {
        if is_direct {
            self.direct_short_buffers.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dsp_short_buffers.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    fn note_zero_buffer(&self, is_direct: bool) {
        if is_direct {
            self.direct_zero_buffers.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dsp_zero_buffers.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    fn note_pushed_frames(&self, is_direct: bool, frames: u64) {
        if is_direct {
            self.direct_pushed_frames
                .fetch_add(frames, Ordering::Relaxed);
        } else {
            self.dsp_pushed_frames.fetch_add(frames, Ordering::Relaxed);
        }
    }

    #[inline]
    fn note_dropped_frames(&self, is_direct: bool, frames: u64) {
        if is_direct {
            self.direct_dropped_frames
                .fetch_add(frames, Ordering::Relaxed);
        } else {
            self.dsp_dropped_frames.fetch_add(frames, Ordering::Relaxed);
        }
    }

    fn effective_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| self.metadata_warning.lock().ok().and_then(|g| g.clone()))
            .or_else(|| {
                self.chain_warning
                    .lock()
                    .ok()
                    .and_then(|g| g.warning.as_ref().map(|warning| warning.message.clone()))
            })
    }
}

enum PwMsg {
    ApplyRatePolicy {
        policy: RatePolicy,
        quantum_48k: u32,
        last_rate: u32,
    },
    DisconnectAndRestore {
        ack_tx: mpsc::SyncSender<Result<()>>,
    },
    Reconnect {
        config: Box<EngineDriverConfig>,
        last_rate: u32,
        dsp_prod: rtrb::Producer<f32>,
        direct_prod: rtrb::Producer<f32>,
        reply_tx: mpsc::SyncSender<Result<SendRawStream>>,
    },
    /// The engine's volume or the ownership mode changed.
    SinkVolume {
        follow_changed: bool,
    },
    Stop,
}

enum CtrlReq {
    UpdateConfig {
        config: Box<EngineDriverConfig>,
        force_reopen: bool,
    },
    ReleaseDevice {
        reply_tx: mpsc::SyncSender<Result<()>>,
    },
    AcquireDevice {
        reply_tx: mpsc::SyncSender<Result<()>>,
    },
    /// [`BypassNotifier::send`].
    Bypass(crate::lifecycle::BypassNote),
}

#[allow(clippy::large_enum_variant)]
enum RtControlCmd {
    UpdateDesiredDsp(DesiredDspConfig),
    Resume {
        alsa: Box<AlsaOutput>,
        alsa_fds: Vec<libc::pollfd>,
        params: NegotiatedParams,
        graph_rate: u32,
        actual_ratio: RateRatio,
        target_sink: SinkChoice,
        desired_dsp: DesiredDspConfig,
        negotiated_key: NegotiatedKey,
        required_chain_seq: Option<u64>,
        /// Chain rebuild failed without a usable fallback: stay in `Error`
        /// instead of resuming with a stale/mismatched chain.
        hold_error: bool,
        new_driver_stream: Option<SendRawStream>,
        new_dsp_cons: Option<rtrb::Consumer<f32>>,
        new_direct_cons: Option<rtrb::Consumer<f32>>,
        dsp_in_buf: Option<Vec<f32>>,
        direct_in_buf: Option<Vec<f32>>,
        out_buf: Option<Vec<i32>>,
        fade_in_frames: usize,
    },
    SuspendForRelease,
    SuspendForReopen,
    ReopenAborted,
}

enum RtEvent {
    NeedReopen {
        target_rate: u32,
        target_quantum: u32,
        target_sink: SinkChoice,
        old_alsa: Option<Box<AlsaOutput>>,
        old_fds: Vec<libc::pollfd>,
        /// The device failed in a way a restart of the stream cannot fix
        /// (usually unplugged): a failed reopen waits for it to come back.
        device_failed: bool,
    },
    ReleasedIdle {
        old_alsa: Option<Box<AlsaOutput>>,
        old_fds: Vec<libc::pollfd>,
    },
    Resumed,
    RecycleBuffers {
        old_fds: Vec<libc::pollfd>,
        old_dsp_cons: Option<rtrb::Consumer<f32>>,
        old_direct_cons: Option<rtrb::Consumer<f32>>,
        old_dsp_in: Option<Vec<f32>>,
        old_direct_in: Option<Vec<f32>>,
        old_out: Option<Vec<i32>>,
    },
    FatalError {
        msg: &'static str,
    },
}

/// `AUDIVANE_TEST_UNPLUG=<file>`: while that file holds `1`, the configured
/// device counts as unplugged. It is reported missing, every open fails as
/// for a card that is not there, and the RT thread lets a running device go
/// as if it had disappeared. Lets a rig test unplug and replug without
/// touching the hardware.
fn test_unplug_file() -> Option<&'static std::path::Path> {
    static PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("AUDIVANE_TEST_UNPLUG")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    })
    .as_deref()
}

const TEST_UNPLUGGED_MSG: &str = "No such device (AUDIVANE_TEST_UNPLUG)";

fn test_unplugged() -> bool {
    test_unplug_file()
        .and_then(|p| std::fs::read(p).ok())
        .is_some_and(|b| b.first() == Some(&b'1'))
}

/// Whether the output device `device` names is in the system now
/// ([`audivane_io_alsa::device_present`]; nothing is opened).
pub fn device_present(device: &str) -> bool {
    !test_unplugged() && audivane_io_alsa::device_present(device)
}

/// `status.error` while the engine waits for its device: "USB is not
/// connected." (the card name of a `hw:CARD=…` id, else the id).
pub fn waiting_text(device: &str) -> String {
    let name = device
        .split_once("CARD=")
        .map_or(device, |(_, rest)| rest.split(',').next().unwrap_or(rest));
    format!("{name} is not connected.")
}

/// How often the control thread looks for a missing device.
const DEVICE_POLL: Duration = Duration::from_millis(500);
/// Retries of a device that is there but fails to open after a loss:
/// first after this long, doubling up to [`DEVICE_RETRY_MAX`].
const DEVICE_RETRY_FIRST: Duration = Duration::from_secs(1);
const DEVICE_RETRY_MAX: Duration = Duration::from_secs(30);

/// The control thread's watch on the device it holds open: `/proc/asound`
/// every [`DEVICE_POLL`]. ALSA only reports an unplug to a device that
/// plays, so between tracks this is what notices it. The device counts as
/// gone only after this watch saw it there, so a name it cannot resolve
/// never takes a working device away.
#[derive(Debug)]
struct PresenceWatch {
    next: Instant,
    seen: bool,
}

impl PresenceWatch {
    fn new(now: Instant) -> Self {
        Self {
            next: now,
            seen: false,
        }
    }

    /// Forget what was seen (no output held, or a new one).
    fn reset(&mut self) {
        self.seen = false;
    }

    /// `Some(gone)` when a look is due at `now`; `present` is asked then.
    fn look(&mut self, now: Instant, present: impl FnOnce() -> bool) -> Option<bool> {
        if now < self.next {
            return None;
        }
        self.next = now + DEVICE_POLL;
        if present() {
            self.seen = true;
            Some(false)
        } else {
            Some(self.seen)
        }
    }
}

/// The engine stops waiting for its device: it was released on request,
/// or opened again. `lost` ends, and the status no longer says it waits.
fn end_waiting(shared: &SharedEngineState, lost: &mut Option<LostDevice>, device: &str) {
    *lost = None;
    shared.device_missing.store(false, Ordering::Release);
    shared.device_gone.store(false, Ordering::Release);
    shared.clear_error_if(&waiting_text(device));
}

/// The control thread's view of a lost device: it waits for the device to
/// appear (missing), or retries a present one that keeps failing.
#[derive(Debug)]
struct LostDevice {
    /// When to look (or retry) next.
    next: Instant,
    /// The wait before the next retry of a present device.
    backoff: Duration,
    /// Whether it was there at the last look.
    present: bool,
}

impl LostDevice {
    fn new(present: bool, now: Instant) -> Self {
        Self {
            next: now
                + if present {
                    DEVICE_RETRY_FIRST
                } else {
                    DEVICE_POLL
                },
            backoff: DEVICE_RETRY_FIRST,
            present,
        }
    }

    /// A look at `now`: `true` when an open is due (the device appeared,
    /// or a present one's retry time came).
    fn due(&mut self, present: bool, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        let appeared = present && !self.present;
        let retry = present && self.present;
        self.present = present;
        if appeared {
            self.backoff = DEVICE_RETRY_FIRST;
            self.next = now + self.backoff;
            true
        } else if retry {
            self.backoff = (self.backoff * 2).min(DEVICE_RETRY_MAX);
            self.next = now + self.backoff;
            true
        } else {
            self.next = now + DEVICE_POLL;
            false
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TestStallConfig {
    stall_ms: u64,
    after_frames_out: u64,
}

impl TestStallConfig {
    fn from_env(rate: u32, ratio: RateRatio) -> Option<Self> {
        let stall_ms: u64 = std::env::var("AUDIVANE_TEST_STALL_MS").ok()?.parse().ok()?;
        if stall_ms == 0 {
            return None;
        }
        let after_ms: u64 = std::env::var("AUDIVANE_TEST_STALL_AFTER_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);
        let out_rate = u64::from(ratio.output_rate(rate).unwrap_or(rate));
        Some(Self {
            stall_ms,
            after_frames_out: (out_rate * after_ms) / 1000,
        })
    }
}

/// Owned handle to the dual-sink PipeWire + ALSA RT engine driver.
pub struct EngineDriverHandle {
    shared: Arc<SharedEngineState>,
    cmd_efd: Arc<EventFd>,
    ctrl_efd: Arc<EventFd>,
    ctrl_req_tx: Mutex<mpsc::Sender<CtrlReq>>,
    lifecycle_lock: Mutex<()>,
    pw_tx: Mutex<pw::channel::Sender<PwMsg>>,
    alsa_thread: Mutex<Option<JoinHandle<()>>>,
    ctrl_thread: Mutex<Option<JoinHandle<()>>>,
    pw_thread: Mutex<Option<JoinHandle<()>>>,
}

impl EngineDriverHandle {
    /// Start the dual-sink PipeWire + ALSA driver threads and wait until ALSA
    /// is opened and both PipeWire sink nodes + initial settings metadata are ready.
    ///
    /// `before_release` (see [`ReleaseHook`]) is installed before the first
    /// negotiation, so a start-up that fails after it still hands the card
    /// over.
    pub fn spawn<P: EngineAudioProcessor>(
        config: EngineDriverConfig,
        processor: P,
        on_negotiated: Option<RatioNegotiator>,
        before_release: Option<ReleaseHook>,
    ) -> Result<Self> {
        let channels = config.channels.max(1);
        let init_desired = config.desired_dsp(0);

        let shared = Arc::new(SharedEngineState::new(&config));
        *shared
            .before_release
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = before_release;
        let cmd_efd = Arc::new(EventFd::new()?);
        let data_efd = Arc::new(EventFd::new()?);
        let ctrl_efd = Arc::new(EventFd::new()?);

        // Step 1: Reserve card + open ALSA off-RT before starting RT thread.
        // A device that is not there (unplugged, a card that is not plugged
        // in) does not stop the start: the sinks come up and run on the
        // engine's own clock, and the control thread opens the device once
        // it appears.
        let started = if device_present(&config.device) {
            startup_open(
                &config,
                init_desired,
                &shared,
                &ctrl_efd,
                on_negotiated.as_ref(),
            )?
        } else {
            log::warn!(
                "output device '{}' not found; starting without it and waiting for it",
                config.device
            );
            startup_missing(&config, init_desired, &shared)?
        };
        let StartupDevice {
            reservation,
            caps,
            effective,
            bypass_held: init_bypass_held,
            desired: init_desired,
            rate: init_rate,
            quantum: init_q,
            ratio: actual_ratio,
            alsa: alsa_init,
            params: init_params,
            key: init_key,
            negotiated: init_negotiated,
        } = started;
        let missing = alsa_init.is_none();
        let test_stall = TestStallConfig::from_env(
            init_rate,
            init_desired.resolve_ratio_for(SinkChoice::Dsp, init_rate),
        );
        if let Some(key) = init_key {
            shared.set_open_key(key);
        }
        let alsa_fds = match &alsa_init {
            Some(a) => a.poll_descriptors()?,
            None => Vec::new(),
        };

        shared.format.store(
            format_to_u8(alsa_init.as_ref().map(|_| init_params.format)),
            Ordering::Release,
        );
        shared.graph_rate.store(init_rate, Ordering::Release);
        shared.output_rate.store(
            if missing { 0 } else { init_params.rate },
            Ordering::Release,
        );
        shared
            .ratio
            .store(actual_ratio.to_bits(), Ordering::Release);
        shared.quantum.store(
            actual_ratio
                .scale_to_input(init_params.period_frames)
                .max(32),
            Ordering::Release,
        );

        // Declared after the output and the reservation, so on an early
        // return it runs before they are dropped.
        let mut init_hand_over = InitHandOver::new(Arc::clone(&shared), config.device.clone());
        let mut init_waiters: Vec<mpsc::SyncSender<Result<()>>> = Vec::new();
        let (init_required_seq, init_hold_error) = match init_negotiated {
            Some(n) => apply_negotiated_chain(n, &shared, &mut init_waiters),
            None => (None, false),
        };

        // Step 2: Create lock-free audio rings (~16 * 1024 frames) and control rings.
        // Sized for varispeed's largest watermark schedule
        // (`RING_ALLOC_FRAMES`), once, before the thread turns real-time.
        let ring_samples = (RING_ALLOC_FRAMES * channels as usize).max(32 * 1024);
        let (dsp_prod, dsp_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
        let (direct_prod, direct_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
        let (ctrl_to_rt_tx, ctrl_to_rt_rx) = rtrb::RingBuffer::<RtControlCmd>::new(32);
        let (rt_to_ctrl_tx, rt_to_ctrl_rx) = rtrb::RingBuffer::<RtEvent>::new(32);

        // Step 3: Spawn PipeWire thread registering both sinks + metadata + registry listener.
        let (pw_tx, pw_rx) = pw::channel::channel::<PwMsg>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<SendRawStream>>(1);

        // PipeWire only ever sees the rates the device can play.
        let mut pw_cfg = config.clone();
        pw_cfg.rate_policy = effective.clone();
        let pw_shared = Arc::clone(&shared);
        let pw_cmd_efd = Arc::clone(&cmd_efd);
        let pw_data_efd = Arc::clone(&data_efd);
        let pw_ctrl_efd = Arc::clone(&ctrl_efd);
        let pw_thread = thread::Builder::new()
            .name("audivane-pw".to_string())
            .spawn(move || {
                run_pw_loop(
                    pw_cfg,
                    init_rate,
                    init_q,
                    pw_shared,
                    pw_cmd_efd,
                    pw_data_efd,
                    pw_ctrl_efd,
                    dsp_prod,
                    direct_prod,
                    pw_rx,
                    ready_tx,
                );
            })
            .map_err(|e| PwError::Init(format!("spawn audivane-pw failed: {e}")))?;

        let raw_driver_stream = match ready_rx.recv() {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => {
                shared.stop.store(true, Ordering::Release);
                let _ = pw_tx.send(PwMsg::Stop);
                let _ = pw_thread.join();
                init_hand_over.run();
                drop(alsa_init);
                drop(reservation);
                return Err(e);
            }
            Err(e) => {
                shared.stop.store(true, Ordering::Release);
                let _ = pw_thread.join();
                init_hand_over.run();
                drop(alsa_init);
                drop(reservation);
                return Err(PwError::Init(format!("PipeWire startup failed: {e}")));
            }
        };

        // Wait briefly for PipeWire to assign node IDs to both streams.
        let id_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < id_deadline {
            let d1 = shared.dsp_node_id.load(Ordering::Acquire);
            let d2 = shared.direct_node_id.load(Ordering::Acquire);
            if d1 != pw::constants::ID_ANY && d2 != pw::constants::ID_ANY {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        // Step 4: Spawn non-RT control thread.
        let (ctrl_req_tx, ctrl_req_rx) = mpsc::channel::<CtrlReq>();
        let ctrl_cfg = config.clone();
        let ctrl_shared = Arc::clone(&shared);
        let ctrl_cmd_efd = Arc::clone(&cmd_efd);
        let ctrl_ctrl_efd = Arc::clone(&ctrl_efd);
        let ctrl_pw_tx = pw_tx.clone();
        let ctrl_on_nego = on_negotiated.clone();
        let ctrl_thread = thread::Builder::new()
            .name("audivane-pw-ctrl".to_string())
            .spawn(move || {
                run_control_thread(
                    ctrl_cfg,
                    init_desired,
                    init_bypass_held,
                    channels as usize,
                    init_rate,
                    actual_ratio,
                    caps,
                    effective,
                    reservation,
                    missing,
                    ctrl_shared,
                    ctrl_cmd_efd,
                    ctrl_ctrl_efd,
                    ctrl_req_rx,
                    ctrl_to_rt_tx,
                    rt_to_ctrl_rx,
                    ctrl_pw_tx,
                    ctrl_on_nego,
                );
            })
            .map_err(|e| PwError::Init(format!("spawn audivane-pw-ctrl failed: {e}")))?;

        // Step 5: Spawn ALSA RT thread.
        let rt_shared = Arc::clone(&shared);
        let rt_cmd_efd = Arc::clone(&cmd_efd);
        let rt_data_efd = Arc::clone(&data_efd);
        let rt_ctrl_efd = Arc::clone(&ctrl_efd);
        let alsa_thread = thread::Builder::new()
            .name("audivane-alsa-rt".to_string())
            .spawn(move || {
                run_alsa_thread(
                    config,
                    init_rate,
                    actual_ratio,
                    init_desired,
                    init_key,
                    init_required_seq,
                    init_hold_error,
                    alsa_init.map(Box::new),
                    init_params,
                    alsa_fds,
                    raw_driver_stream,
                    rt_shared,
                    rt_cmd_efd,
                    rt_data_efd,
                    rt_ctrl_efd,
                    dsp_cons,
                    direct_cons,
                    ctrl_to_rt_rx,
                    rt_to_ctrl_tx,
                    processor,
                    test_stall,
                );
            })
            .map_err(|e| PwError::Init(format!("spawn audivane-alsa-rt failed: {e}")))?;
        init_hand_over.disarm();

        Ok(Self {
            shared,
            cmd_efd,
            ctrl_efd,
            ctrl_req_tx: Mutex::new(ctrl_req_tx),
            lifecycle_lock: Mutex::new(()),
            pw_tx: Mutex::new(pw_tx),
            alsa_thread: Mutex::new(Some(alsa_thread)),
            ctrl_thread: Mutex::new(Some(ctrl_thread)),
            pw_thread: Mutex::new(Some(pw_thread)),
        })
    }

    pub fn snapshot(&self) -> EngineDriverSnapshot {
        let error = self.shared.effective_error();
        let dsp_raw = self.shared.dsp_node_id.load(Ordering::Acquire);
        let direct_raw = self.shared.direct_node_id.load(Ordering::Acquire);
        EngineDriverSnapshot {
            state: self.shared.state(),
            error,
            format: format_from_u8(self.shared.format.load(Ordering::Acquire)),
            graph_rate: self.shared.graph_rate.load(Ordering::Acquire),
            output_rate: self.shared.output_rate.load(Ordering::Acquire),
            ratio: RateRatio::from_bits(self.shared.ratio.load(Ordering::Acquire)),
            quantum: self.shared.quantum.load(Ordering::Acquire),
            alsa_delay_frames: self.shared.alsa_delay_frames.load(Ordering::Relaxed),
            xruns: self.shared.xruns.load(Ordering::Relaxed),
            ring_underruns: self.shared.ring_underruns.load(Ordering::Relaxed),
            dsp_load: f64::from_bits(self.shared.dsp_load_bits.load(Ordering::Relaxed)),
            active_sink: sink_from_u8(self.shared.active_sink.load(Ordering::Acquire)),
            dsp_node: (dsp_raw != pw::constants::ID_ANY).then_some(dsp_raw),
            direct_node: (direct_raw != pw::constants::ID_ANY).then_some(direct_raw),
            frames_out: self.shared.frames_out.load(Ordering::Relaxed),
            dsp_latency_out_frames: self.shared.dsp_latency_out.load(Ordering::Relaxed) as usize,
            gaps: DriverGapCounters {
                direct_zero_periods: self.shared.direct_zero_periods.load(Ordering::Relaxed),
                direct_zero_frames: self.shared.direct_zero_frames.load(Ordering::Relaxed),
                dsp_zero_periods: self.shared.dsp_zero_periods.load(Ordering::Relaxed),
                dsp_zero_frames: self.shared.dsp_zero_frames.load(Ordering::Relaxed),
                ring_underruns: self.shared.ring_underruns.load(Ordering::Relaxed),
                direct_short_buffers: self.shared.direct_short_buffers.load(Ordering::Relaxed),
                dsp_short_buffers: self.shared.dsp_short_buffers.load(Ordering::Relaxed),
                direct_zero_buffers: self.shared.direct_zero_buffers.load(Ordering::Relaxed),
                dsp_zero_buffers: self.shared.dsp_zero_buffers.load(Ordering::Relaxed),
                direct_dropped_frames: self.shared.direct_dropped_frames.load(Ordering::Relaxed),
                dsp_dropped_frames: self.shared.dsp_dropped_frames.load(Ordering::Relaxed),
                direct_pushed_frames: self.shared.direct_pushed_frames.load(Ordering::Relaxed),
                dsp_pushed_frames: self.shared.dsp_pushed_frames.load(Ordering::Relaxed),
                direct_popped_frames: self.shared.direct_popped_frames.load(Ordering::Relaxed),
                dsp_popped_frames: self.shared.dsp_popped_frames.load(Ordering::Relaxed),
                direct_drained_prime: self.shared.direct_drained_prime.load(Ordering::Relaxed),
                dsp_drained_prime: self.shared.dsp_drained_prime.load(Ordering::Relaxed),
                direct_drained_idle: self.shared.direct_drained_idle.load(Ordering::Relaxed),
                dsp_drained_idle: self.shared.dsp_drained_idle.load(Ordering::Relaxed),
                direct_drained_unused: self.shared.direct_drained_unused.load(Ordering::Relaxed),
                dsp_drained_unused: self.shared.dsp_drained_unused.load(Ordering::Relaxed),
                reopens: self.shared.reopens.load(Ordering::Relaxed),
                resumes: self.shared.resumes.load(Ordering::Relaxed),
                rate_switches: self.shared.rate_switches.load(Ordering::Relaxed),
                fade_outs: self.shared.fade_outs.load(Ordering::Relaxed),
                fade_ins: self.shared.fade_ins.load(Ordering::Relaxed),
                idle_stops: self.shared.idle_stops.load(Ordering::Relaxed),
                drain_silence_periods: self.shared.drain_silence_periods.load(Ordering::Relaxed),
                prime_silence_periods: self.shared.prime_silence_periods.load(Ordering::Relaxed),
                carry_frames_out: self.shared.carry_frames_out.load(Ordering::Relaxed),
                stream_starts_direct: self.shared.stream_starts_direct.load(Ordering::Relaxed),
                stream_starts_dsp: self.shared.stream_starts_dsp.load(Ordering::Relaxed),
                stream_pauses_direct: self.shared.stream_pauses_direct.load(Ordering::Relaxed),
                stream_pauses_dsp: self.shared.stream_pauses_dsp.load(Ordering::Relaxed),
                // The count before its generation (published in that order).
                rt_overloads: self.shared.rt.demotions(),
                rt_signals: self.shared.rt.signals(),
                rt_failed_demotions: self.shared.rt.failed_demotions(),
                rt_overload_generation: self.shared.rt.demotion_generation(),
                rt_overload_hold_seq: self.shared.rt.demotion_hold_seq(),
                probes: self.shared.probes.load(Ordering::Acquire),
                rt_naps: self.shared.rt.naps(),
                rt_demoted: u64::from(self.shared.rt.demoted()),
                dsp_wanted: u64::from(self.shared.pick_sink() == Some(SinkChoice::Dsp)),
                xrun_events: self.shared.xrun_events.load(Ordering::Relaxed),
                processed_periods: self.shared.rt.periods(),
            },
            device_caps: self.shared.device_caps.lock().ok().and_then(|g| g.clone()),
            open_key: self.shared.open_key.lock().ok().and_then(|g| *g),
            playback_session: self.shared.playback_session.load(Ordering::Acquire),
            effective_policy: self
                .shared
                .effective_policy
                .lock()
                .ok()
                .and_then(|g| g.clone()),
            direct_rates: self
                .shared
                .direct_policy()
                .map(|p| p.allowed_rates)
                .unwrap_or_default(),
            varispeed: self.shared.varispeed.snapshot(),
            rt_load_max: f64::from_bits(self.shared.rt_load_max_bits.load(Ordering::Relaxed)),
            waiting_for_device: self.shared.device_missing.load(Ordering::Acquire),
        }
    }

    /// Ask for varispeed `speed` with a ramp of at least `ramp_ms` (the
    /// stage's envelope decides the shortest). The RT thread takes it at
    /// its next DAC wakeup (the last command wins), clamps it to the cap the
    /// negotiated timing allows and releases an underfeed hold. It applies
    /// only while a stage runs (the processor installs and retires it);
    /// the request is kept for when one does.
    pub fn set_varispeed_speed(&self, speed: f64, ramp_ms: f64) {
        self.shared.varispeed_cmd.send(speed, ramp_ms);
        self.cmd_efd.signal();
    }

    /// The overload fallback's hold on varispeed (its budget and speed
    /// cap; `Off` is the control side's: it retires the stage). The RT
    /// thread applies it at the stage's next plan. Returns its sequence
    /// ([`crate::varispeed_rt::VarispeedCommandSlot::set_hold`]), which the
    /// audio thread acknowledges once it applies it
    /// ([`crate::varispeed_rt::VarispeedDriverSnapshot::hold_ack`]).
    pub fn set_varispeed_hold(&self, hold: crate::varispeed_rt::VarispeedHold) -> u64 {
        let seq = self.shared.varispeed_cmd.set_hold(hold);
        self.cmd_efd.signal();
        seq
    }

    /// Volume changes made on our sinks from outside (desktop, media keys,
    /// `wpctl`) while the engine owns the sink volume. A new call replaces
    /// the previous receiver.
    pub fn sink_volume_requests(&self) -> mpsc::Receiver<SinkVolumeRequest> {
        self.shared.sink_volume.subscribe()
    }

    /// Audio already handed to ALSA or held in the DSP chain's filter delay
    /// but not yet heard, plus one period: the ALSA delay is sampled when
    /// the output thread wakes, before it writes the next period. `None`
    /// while nothing is open.
    pub fn output_latency_probe(&self) -> impl Fn() -> Option<Duration> + Send + Sync + 'static {
        let shared = Arc::clone(&self.shared);
        move || {
            let rate = shared.output_rate.load(Ordering::Relaxed);
            if rate == 0 || shared.state() == EngineState::Idle {
                return None;
            }
            let frames = shared.alsa_delay_frames.load(Ordering::Relaxed).max(0) as f64
                + shared.dsp_latency_out.load(Ordering::Relaxed) as f64;
            let graph_rate = shared.graph_rate.load(Ordering::Relaxed);
            let period = if graph_rate > 0 {
                f64::from(shared.quantum.load(Ordering::Relaxed)) / f64::from(graph_rate)
            } else {
                0.0
            };
            Some(Duration::from_secs_f64(frames / f64::from(rate) + period))
        }
    }

    /// The driver's current state, readable from any thread (an RT
    /// `FatalError` shows here before the next config apply).
    pub fn state_probe(&self) -> impl Fn() -> EngineState + Send + Sync + 'static {
        let shared = Arc::clone(&self.shared);
        move || shared.state()
    }

    /// See [`ReleaseHook`]. Replaces an earlier hook.
    pub fn set_before_release(&self, hook: ReleaseHook) {
        *self
            .shared
            .before_release
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// A clonable handle for [`SinkVolumePort::set`], usable from any thread.
    pub fn sink_volume_port(&self) -> SinkVolumePort {
        SinkVolumePort {
            shared: Arc::clone(&self.shared.sink_volume),
            pw_tx: self.pw_tx.lock().ok().map(|tx| tx.clone()),
        }
    }

    /// Return a clonable [`DriverWaker`] for waking the ALSA RT thread from
    /// auxiliary threads such as the chain reaper.
    pub fn waker(&self) -> DriverWaker {
        DriverWaker {
            cmd_efd: Arc::clone(&self.cmd_efd),
        }
    }

    /// A [`BypassNotifier`] for the engine's bypass resampler designer.
    pub fn bypass_notifier(&self) -> BypassNotifier {
        let tx = match self.ctrl_req_tx.lock() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        };
        BypassNotifier {
            tx,
            ctrl_efd: Arc::clone(&self.ctrl_efd),
        }
    }

    /// Wake the RT thread immediately so pending `poll_updates` (e.g. chain swap
    /// or self-test) runs even when idle (`Ready`).
    pub fn wake_rt(&self) {
        self.cmd_efd.signal();
    }

    /// Clear a recorded fallback-chain warning once a replacement chain built
    /// from `generation` (or newer) has been published. A stale success from an
    /// older generation leaves a newer warning intact.
    pub fn clear_chain_warning_through(&self, generation: u64) {
        self.shared.clear_chain_warning_through(generation);
    }

    /// Update runtime driver configuration (device, formats, desired DSP rate
    /// target, rate policy, quantum). Propagates the desired DSP configuration
    /// with a monotonic generation counter to the RT thread immediately.
    pub fn update_config(&self, config: EngineDriverConfig, force_reopen: bool) {
        if let Ok(tx) = self.ctrl_req_tx.lock() {
            let _ = tx.send(CtrlReq::UpdateConfig {
                config: Box::new(config),
                force_reopen,
            });
        }
        self.ctrl_efd.signal();
    }

    /// Release ALSA device + D-Bus `ReserveDevice1` reservation and move to `Idle`.
    /// Blocks until full control-side teardown (ALSA drop, PipeWire metadata
    /// restore + disconnect, and reservation release) has completed.
    pub fn release_device(&self) -> Result<()> {
        let _guard = self
            .lifecycle_lock
            .lock()
            .map_err(|_| PwError::Init("driver lifecycle lock poisoned".into()))?;
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<()>>(1);
        self.shared.release_requested.store(true, Ordering::Release);
        {
            let tx = self
                .ctrl_req_tx
                .lock()
                .map_err(|_| PwError::Init("control request channel poisoned".into()))?;
            tx.send(CtrlReq::ReleaseDevice { reply_tx })
                .map_err(|_| PwError::Init("control thread has stopped".into()))?;
        }
        self.ctrl_efd.signal();
        self.cmd_efd.signal();

        match reply_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The release can still complete (the engine's release hook
                // may wait for a chain build); it clears this error then.
                let msg = RELEASE_TIMEOUT_MSG.to_string();
                self.shared.set_error(msg.clone());
                Err(PwError::Init(msg))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let msg = "release_device aborted: control thread disconnected".to_string();
                self.shared.set_error(msg.clone());
                Err(PwError::Init(msg))
            }
        }
    }

    /// Re-acquire ALSA device + D-Bus `ReserveDevice1` and resume `Ready`.
    /// Serialized behind any pending release teardown and waits until the RT
    /// thread acknowledges `Resume` (or reports an error).
    pub fn acquire_device(&self) -> Result<()> {
        let _guard = self
            .lifecycle_lock
            .lock()
            .map_err(|_| PwError::Init("driver lifecycle lock poisoned".into()))?;
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<()>>(1);
        {
            let tx = self
                .ctrl_req_tx
                .lock()
                .map_err(|_| PwError::Init("control request channel poisoned".into()))?;
            tx.send(CtrlReq::AcquireDevice { reply_tx })
                .map_err(|_| PwError::Init("control thread has stopped".into()))?;
        }
        self.ctrl_efd.signal();

        match reply_rx.recv_timeout(Duration::from_secs(8)) {
            Ok(res) => res,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(PwError::Init(
                "acquire_device timed out waiting for driver resume".into(),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(PwError::Init(
                "acquire_device aborted: control thread disconnected".into(),
            )),
        }
    }

    /// `true` once the ALSA thread has ended by itself: the device failed in a
    /// way that restarting the stream cannot fix (usually it was unplugged).
    /// The driver cannot play again; the owner has to start a new one.
    pub fn audio_thread_ended(&self) -> bool {
        !self.shared.stop.load(Ordering::Acquire)
            && self
                .alsa_thread
                .lock()
                .is_ok_and(|slot| slot.as_ref().is_some_and(JoinHandle::is_finished))
    }

    /// Gracefully stop all threads, close ALSA, release `ReserveDevice1`, and
    /// restore PipeWire metadata. Idempotent.
    pub fn stop(&self) {
        if self.shared.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        self.cmd_efd.signal();
        self.ctrl_efd.signal();

        // Join ALSA RT thread first so `raw_driver_stream` is never used after
        // PipeWire streams are dropped on the PipeWire thread.
        if let Ok(mut slot) = self.alsa_thread.lock()
            && let Some(h) = slot.take()
        {
            let _ = h.join();
        }
        if let Ok(mut slot) = self.ctrl_thread.lock()
            && let Some(h) = slot.take()
        {
            let _ = h.join();
        }
        if let Ok(tx) = self.pw_tx.lock() {
            let _ = tx.send(PwMsg::Stop);
        }
        if let Ok(mut slot) = self.pw_thread.lock()
            && let Some(h) = slot.take()
        {
            let _ = h.join();
        }
    }
}

impl Drop for EngineDriverHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------- PipeWire loop + Registry

struct StreamSideData {
    is_direct: bool,
    /// The owned sink volume was written on this stream after it connected.
    volume_asserted: bool,
    channels: usize,
    quantum_48k: u32,
    peer_direct: Option<pw::stream::StreamRc>,
    shared: Arc<SharedEngineState>,
    cmd_efd: Arc<EventFd>,
    data_efd: Arc<EventFd>,
    prod: rtrb::Producer<f32>,
}

impl StreamSideData {
    fn sink_choice(&self) -> SinkChoice {
        if self.is_direct {
            SinkChoice::Direct
        } else {
            SinkChoice::Dsp
        }
    }
}

struct ClientNodeEntry {
    running: bool,
    native_rate: Option<u32>,
    /// `application.name` (or the node's description/name), for logs.
    name: String,
    /// Start order while running (see `RateArbiter::update_since`).
    since: Option<u64>,
    _node: pw::node::Node,
    _listener: pw::node::NodeListener,
}

struct LinkEntry {
    _link: pw::link::Link,
    _listener: pw::link::LinkListener,
}

fn apply_metadata_writes_with<F>(
    meta_tracker: &mut MetadataTracker,
    shared: &SharedEngineState,
    rate_following_disabled: &mut bool,
    writes: &[(&str, &str, &str)],
    send_ops: F,
) -> bool
where
    F: FnOnce(&[MetadataOp]),
{
    match meta_tracker.plan_writes(writes) {
        Ok(ops) => {
            *rate_following_disabled = false;
            shared.clear_metadata_warning();
            if !ops.is_empty() {
                send_ops(&ops);
            }
            true
        }
        Err(e) => {
            *rate_following_disabled = true;
            let msg = format!(
                "PipeWire settings restore durability failed ({}): {e}; rate following disabled",
                meta_tracker.restore_path().display()
            );
            log::error!("{msg}");
            shared.set_metadata_warning(msg);
            false
        }
    }
}

struct RegistryTracker {
    shared: Arc<SharedEngineState>,
    cmd_efd: Arc<EventFd>,
    policy: RatePolicy,
    forced_rate: u32,
    channels: u32,
    quantum_48k: u32,
    dsp_stream: pw::stream::StreamRc,
    direct_stream: pw::stream::StreamRc,
    metadata: Option<(u32, pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    meta_tracker: MetadataTracker,
    rate_following_disabled: bool,
    clients: HashMap<u32, ClientNodeEntry>,
    links: HashMap<u32, LinkEntry>,
    links_by_id: HashMap<u32, (u32, u32)>,
    arbiter: RateArbiter,
    /// A held-back rate switch to look at again (polled by the PW thread's
    /// fallback timer).
    rate_recheck_at: Option<Instant>,
    /// Last rate decision logged, so each distinct decision is logged once.
    last_rate_log: Option<String>,
    /// While the Direct sink decides the rate: the native rates it may run
    /// at. With rate following off, the graph leaves `default_rate` for one
    /// of these when the default is not native.
    direct_rates: Option<Vec<u32>>,
}

impl RegistryTracker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        shared: Arc<SharedEngineState>,
        cmd_efd: Arc<EventFd>,
        policy: RatePolicy,
        initial_rate: u32,
        channels: u32,
        quantum_48k: u32,
        dsp_stream: pw::stream::StreamRc,
        direct_stream: pw::stream::StreamRc,
    ) -> Self {
        Self {
            shared,
            cmd_efd,
            policy,
            forced_rate: initial_rate,
            channels,
            quantum_48k,
            dsp_stream,
            direct_stream,
            metadata: None,
            meta_tracker: MetadataTracker::new(),
            rate_following_disabled: false,
            clients: HashMap::new(),
            links: HashMap::new(),
            links_by_id: HashMap::new(),
            arbiter: RateArbiter::new(),
            rate_recheck_at: None,
            last_rate_log: None,
            direct_rates: None,
        }
    }

    fn driver_switching(&self) -> bool {
        self.shared.state() == EngineState::Switching
    }

    /// A client node's info changed: update its running state, start order
    /// and native rate.
    fn on_client_info(&mut self, nid: u32, running: bool, rate: Option<u32>) {
        let now = Instant::now();
        let switching = self.driver_switching();
        let Some(entry) = self.clients.get_mut(&nid) else {
            return;
        };
        entry.since =
            self.arbiter
                .update_since(entry.running, running, entry.since, now, switching);
        entry.running = running;
        if let Some(r) = rate {
            entry.native_rate = Some(r);
        }
        self.recompute_routing_and_rate();
    }

    /// Called from the PW thread's periodic timer.
    fn poll_rate_recheck(&mut self) {
        if self.rate_recheck_at.is_some_and(|t| Instant::now() >= t) {
            self.rate_recheck_at = None;
            self.recompute_routing_and_rate();
        }
    }

    fn sync_streams_rate(&self, rate: u32) {
        let q = scaled_quantum(self.quantum_48k, rate);
        update_stream_latency(&self.dsp_stream, q, rate);
        update_stream_latency(&self.direct_stream, q, rate);
        update_stream_format_rate(&self.direct_stream, self.channels, rate);
    }

    fn send_metadata_ops(&self, ops: &[MetadataOp]) {
        let Some((_, ref meta, _)) = self.metadata else {
            return;
        };
        for op in ops {
            meta.set_property(0, &op.key, op.type_name.as_deref(), op.value.as_deref());
        }
    }

    fn on_initial_metadata_synced(&mut self) {
        let recovery_ops = self.meta_tracker.complete_initial_sync_and_recover();
        if !recovery_ops.is_empty() {
            self.send_metadata_ops(&recovery_ops);
        }
        let init = self.forced_rate;
        self.apply_policy_to_metadata(Some(init));
    }

    fn apply_policy_to_metadata(&mut self, set_force_rate: Option<u32>) {
        let target_force = if !self.policy.follow_source {
            match (&self.direct_rates, set_force_rate) {
                (Some(native), Some(r))
                    if native.contains(&r) && !native.contains(&self.policy.default_rate) =>
                {
                    r
                }
                _ => self.policy.default_rate,
            }
        } else if let Some(r) = set_force_rate {
            r
        } else {
            self.forced_rate
        };

        if self.metadata.is_none() || !self.meta_tracker.is_snapshot_synced() {
            self.forced_rate = target_force;
            self.sync_streams_rate(target_force);
            return;
        }
        let allowed_val = if !self.policy.follow_source {
            format!("[ {target_force} ]")
        } else {
            allowed_rates_prop(&self.policy.allowed_rates, target_force)
        };
        let force_str = target_force.to_string();
        let meta_ref = self.metadata.as_ref();
        let persisted = apply_metadata_writes_with(
            &mut self.meta_tracker,
            &self.shared,
            &mut self.rate_following_disabled,
            &[
                ("clock.allowed-rates", "Spa:String:JSON", &allowed_val),
                ("clock.force-rate", "Spa:String:JSON", &force_str),
            ],
            |ops| {
                if let Some((_, meta, _)) = meta_ref {
                    for op in ops {
                        meta.set_property(0, &op.key, op.type_name.as_deref(), op.value.as_deref());
                    }
                }
            },
        );
        if persisted {
            self.forced_rate = target_force;
            self.sync_streams_rate(target_force);
        }
    }

    /// Re-publish `audio.allowed-rates` and the DSP sink's format pod after
    /// the allowed set changed (new device with other capabilities).
    fn publish_allowed_rates_on_streams(&self) {
        let allowed = allowed_rates_prop(&self.policy.allowed_rates, self.forced_rate);
        update_stream_prop(&self.dsp_stream, *pw::keys::AUDIO_ALLOWED_RATES, &allowed);
        update_stream_prop(
            &self.direct_stream,
            *pw::keys::AUDIO_ALLOWED_RATES,
            &allowed,
        );
        if let Ok(pod_bytes) = build_sink_format_pod(
            self.channels,
            &self.policy.allowed_rates,
            self.policy.default_rate,
        ) && let Some(pod) = Pod::from_bytes(&pod_bytes)
        {
            let mut params = [pod];
            let _ = self.dsp_stream.update_params(&mut params);
        }
    }

    fn restore_metadata(&mut self) -> bool {
        let was_dirty = self.meta_tracker.is_dirty();
        let ops = self.meta_tracker.plan_restore();
        let any = !ops.is_empty();
        self.send_metadata_ops(&ops);
        was_dirty || any
    }

    fn recompute_routing_and_rate(&mut self) {
        let direct_id = self.shared.direct_node_id.load(Ordering::Acquire);
        let dsp_id = self.shared.dsp_node_id.load(Ordering::Acquire);

        let mut direct_has_link = false;
        let mut dsp_has_link = false;
        let mut direct_client_running = false;
        let mut dsp_client_running = false;
        // Keyed by client node: a stereo client has one link per channel.
        let mut direct_clients: HashMap<u32, RateClient> = HashMap::new();
        let mut dsp_clients: HashMap<u32, RateClient> = HashMap::new();

        for &(out_node, in_node) in self.links_by_id.values() {
            let to_direct = in_node == direct_id && direct_id != pw::constants::ID_ANY;
            let to_dsp = in_node == dsp_id && dsp_id != pw::constants::ID_ANY;
            if !to_direct && !to_dsp {
                continue;
            }
            if to_direct {
                direct_has_link = true;
            } else {
                dsp_has_link = true;
            }
            let Some(c) = self.clients.get(&out_node).filter(|c| c.running) else {
                continue;
            };
            if to_direct {
                direct_client_running = true;
            } else {
                dsp_client_running = true;
            }
            let Some(native_rate) = c.native_rate else {
                continue;
            };
            let rc = RateClient {
                id: out_node,
                name: c.name.clone(),
                native_rate,
                since: c.since.unwrap_or(u64::MAX),
            };
            if to_direct {
                direct_clients.insert(out_node, rc);
            } else {
                dsp_clients.insert(out_node, rc);
            }
        }

        self.shared
            .direct_has_links
            .store(direct_has_link, Ordering::Release);
        self.shared
            .dsp_has_links
            .store(dsp_has_link, Ordering::Release);
        self.shared
            .direct_client_running
            .store(direct_client_running, Ordering::Release);
        self.shared
            .dsp_client_running
            .store(dsp_client_running, Ordering::Release);

        if !self.rate_following_disabled {
            let direct: Vec<RateClient> = direct_clients.into_values().collect();
            let dsp: Vec<RateClient> = dsp_clients.into_values().collect();
            let policies = self.shared.sink_policies();
            let direct_policy = policies.as_ref().map(|p| &p.direct);
            // Direct's rates apply while a running client feeds it (as for
            // sink selection, `SharedEngineState::pick_sink`): a paused one
            // that stays linked leaves the graph to the DSP sink's clients.
            let direct_active = direct_client_running;
            self.direct_rates = match (direct_policy, direct_active) {
                (Some(p), true) if !p.allowed_rates.is_empty() => Some(p.allowed_rates.clone()),
                _ => None,
            };
            let inputs = RateInputs {
                policy: &self.policy,
                current: self.forced_rate,
                direct_active,
                direct: &direct,
                dsp: &dsp,
                direct_policy,
                dsp_policy: policies.as_ref().map(|p| &p.dsp),
                direct_native: policies.as_ref().map(|p| &p.direct_native),
                dsp_native: policies.as_ref().map(|p| &p.dsp_native),
            };
            let now = Instant::now();
            let verdict = self.arbiter.decide(&inputs, now, self.driver_switching());
            self.apply_rate_verdict(verdict, now);
        }

        self.cmd_efd.signal();
    }

    fn apply_rate_verdict(&mut self, verdict: Verdict, now: Instant) {
        let current = self.forced_rate;
        let line = verdict.describe(current);
        if verdict.is_notable()
            && let Some(ref l) = line
            && line != self.last_rate_log
        {
            log::info!("Rate: {l}");
        }
        // Clients briefly stopping during our own switch must not make the
        // same decision log again once they resume.
        let flap = self.arbiter.in_flap_window(now, self.driver_switching());
        if line.is_some() || !flap {
            self.last_rate_log = line;
        }
        match verdict {
            Verdict::Idle => {
                self.rate_recheck_at = None;
            }
            Verdict::Keep(choice) => {
                self.rate_recheck_at = None;
                self.shared
                    .desired_client_rate
                    .store(choice.native.unwrap_or(choice.rate), Ordering::Relaxed);
            }
            Verdict::Switch(choice) => {
                self.rate_recheck_at = None;
                self.shared
                    .desired_client_rate
                    .store(choice.native.unwrap_or(choice.rate), Ordering::Relaxed);
                self.apply_policy_to_metadata(Some(choice.rate));
                if self.forced_rate != current {
                    self.arbiter.note_switch(now);
                }
            }
            Verdict::Wait { until, .. } => {
                self.rate_recheck_at = Some(until);
            }
        }
    }
}

fn parse_node_rate(s: &str) -> Option<u32> {
    if let Some((_, denom)) = s.split_once('/') {
        denom.trim().parse::<u32>().ok().filter(|&r| r >= 8000)
    } else {
        s.trim().parse::<u32>().ok().filter(|&r| r >= 8000)
    }
}

fn build_sink_format_pod(
    channels: u32,
    allowed_rates: &[u32],
    default_rate: u32,
) -> Result<Vec<u8>> {
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_channels(channels);
    if allowed_rates.len() <= 1 {
        let rate = allowed_rates.first().copied().unwrap_or(default_rate);
        audio_info.set_rate(rate);
    }
    let mut properties: Vec<spa::pod::Property> = audio_info.into();
    if allowed_rates.len() > 1 {
        // Several rates: an enum choice, default first (PipeWire's
        // convention lists the default again among the alternatives).
        let default = if allowed_rates.contains(&default_rate) {
            default_rate
        } else {
            allowed_rates[0]
        };
        properties.push(spa::pod::Property {
            key: spa::sys::SPA_FORMAT_AUDIO_rate,
            flags: spa::pod::PropertyFlags::empty(),
            value: spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Enum {
                    default: default as i32,
                    alternatives: allowed_rates.iter().map(|&r| r as i32).collect(),
                },
            ))),
        });
    }
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    };
    spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map(|(c, _)| c.into_inner())
    .map_err(|e| PwError::Init(format!("pod serialize failed: {e:?}")))
}

#[allow(clippy::too_many_arguments)]
fn connect_both_streams(
    dsp_stream: &pw::stream::StreamRc,
    direct_stream: &pw::stream::StreamRc,
    channels: u32,
    policy: &RatePolicy,
) -> Result<()> {
    let pod_bytes = build_sink_format_pod(channels, &policy.allowed_rates, policy.default_rate)?;
    let mut params_dsp = [Pod::from_bytes(&pod_bytes).expect("valid serialized SPA pod")];
    let dsp_flags = pw::stream::StreamFlags::DRIVER
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;
    dsp_stream.connect(
        spa::utils::Direction::Input,
        None,
        dsp_flags,
        &mut params_dsp,
    )?;

    let mut params_dir = [Pod::from_bytes(&pod_bytes).expect("valid serialized SPA pod")];
    let dir_flags = pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
    direct_stream.connect(
        spa::utils::Direction::Input,
        None,
        dir_flags,
        &mut params_dir,
    )?;
    Ok(())
}

/// `audio.allowed-rates` / `clock.allowed-rates` JSON value.
fn allowed_rates_prop(allowed: &[u32], fallback: u32) -> String {
    if allowed.is_empty() {
        format!("[ {fallback} ]")
    } else {
        let parts: Vec<String> = allowed.iter().map(u32::to_string).collect();
        format!("[ {} ]", parts.join(" "))
    }
}

fn update_stream_prop(stream: &pw::stream::Stream, key: &str, value: &str) {
    let mut props = pw::properties::PropertiesBox::new();
    props.insert(key, value.to_string());
    // Safety: as in `update_stream_latency`.
    unsafe {
        let dict_ptr = &(*props.as_raw_ptr()).dict as *const pw::spa::sys::spa_dict;
        pw::sys::pw_stream_update_properties(stream.as_raw_ptr(), dict_ptr);
    }
}

fn update_stream_latency(stream: &pw::stream::Stream, quantum: u32, rate: u32) {
    let mut props = pw::properties::PropertiesBox::new();
    props.insert(*pw::keys::NODE_LATENCY, format!("{quantum}/{rate}"));
    // Safety: `stream.as_raw_ptr()` is valid on the PipeWire mainloop thread
    // and `props.as_raw_ptr()` points to a valid `pw_properties` whose first
    // member is `struct spa_dict dict`.
    unsafe {
        let dict_ptr = &(*props.as_raw_ptr()).dict as *const pw::spa::sys::spa_dict;
        pw::sys::pw_stream_update_properties(stream.as_raw_ptr(), dict_ptr);
    }
}

fn update_stream_format_rate(stream: &pw::stream::Stream, channels: u32, rate: u32) {
    if let Ok(pod_bytes) = build_sink_format_pod(channels, &[rate], rate)
        && let Some(pod) = Pod::from_bytes(&pod_bytes)
    {
        let mut params = [pod];
        let _ = stream.update_params(&mut params);
    }
}

struct PwSession {
    tracker: Rc<RefCell<RegistryTracker>>,
    pending_init_sync: Rc<std::cell::Cell<Option<AsyncSeq>>>,
    startup_synced: Rc<std::cell::Cell<bool>>,
    _core_listener: pw::core::Listener,
    _reg_listener: pw::registry::Listener,
    _dsp_listener: pw::stream::StreamListener<StreamSideData>,
    _direct_listener: pw::stream::StreamListener<StreamSideData>,
    dsp_stream: pw::stream::StreamRc,
    direct_stream: pw::stream::StreamRc,
    channels: usize,
    shared: Arc<SharedEngineState>,
    _registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    _context: pw::context::ContextRc,
}

impl PwSession {
    #[allow(clippy::too_many_arguments)]
    fn create(
        mainloop: &pw::main_loop::MainLoopRc,
        config: &EngineDriverConfig,
        init_rate: u32,
        init_quantum: u32,
        shared: &Arc<SharedEngineState>,
        cmd_efd: &Arc<EventFd>,
        data_efd: &Arc<EventFd>,
        dsp_prod: rtrb::Producer<f32>,
        direct_prod: rtrb::Producer<f32>,
        on_core_done: Rc<dyn Fn(AsyncSeq)>,
    ) -> Result<(Self, SendRawStream)> {
        let context = crate::rt_context::new_context(mainloop).map_err(PwError::PipeWire)?;
        let core = context.connect_rc(None).map_err(PwError::PipeWire)?;
        let registry = core.get_registry_rc().map_err(PwError::PipeWire)?;

        let channels = config.channels.max(1);
        // `config.rate_policy` is already restricted to the device here.
        let allowed_str = allowed_rates_prop(&config.rate_policy.allowed_rates, init_rate);

        // Sink 1: Audivane DSP (DAC-paced group driver).
        let mut dsp_props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => "audivane_dsp",
            *pw::keys::NODE_DESCRIPTION => "Audivane DSP",
            *pw::keys::NODE_DRIVER => "true",
            *pw::keys::PRIORITY_DRIVER => PW_STREAM_DRIVER_PRIORITY,
            *pw::keys::PRIORITY_SESSION => PW_DSP_SESSION_PRIORITY,
            *pw::keys::NODE_GROUP => PW_DAC_GROUP,
            *pw::keys::NODE_LINK_GROUP => PW_DAC_GROUP,
        };
        dsp_props.insert(
            *pw::keys::NODE_LATENCY,
            format!("{init_quantum}/{init_rate}"),
        );
        dsp_props.insert(*pw::keys::AUDIO_CHANNELS, format!("{channels}"));
        dsp_props.insert(*pw::keys::AUDIO_ALLOWED_RATES, allowed_str.clone());
        if !config.rate_policy.follow_source {
            dsp_props.insert(*pw::keys::NODE_RATE, format!("1/{init_rate}"));
        }

        if shared.sink_volume.follow() {
            dsp_props.insert(sink_volume::RESTORE_PROPS_KEY, "false");
        }
        let dsp_stream = pw::stream::StreamRc::new(core.clone(), "Audivane DSP", dsp_props)
            .map_err(PwError::PipeWire)?;

        // Sink 2: Audivane Direct (follower in the same node.group, bit-exact path).
        let mut direct_props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => "audivane_direct",
            *pw::keys::NODE_DESCRIPTION => "Audivane Direct",
            *pw::keys::PRIORITY_SESSION => PW_DIRECT_SESSION_PRIORITY,
            *pw::keys::NODE_GROUP => PW_DAC_GROUP,
            *pw::keys::NODE_LINK_GROUP => PW_DAC_GROUP,
        };
        direct_props.insert(
            *pw::keys::NODE_LATENCY,
            format!("{init_quantum}/{init_rate}"),
        );
        direct_props.insert(*pw::keys::AUDIO_CHANNELS, format!("{channels}"));
        direct_props.insert(*pw::keys::AUDIO_ALLOWED_RATES, allowed_str);

        if shared.sink_volume.follow() {
            direct_props.insert(sink_volume::RESTORE_PROPS_KEY, "false");
        }
        let direct_stream =
            pw::stream::StreamRc::new(core.clone(), "Audivane Direct", direct_props)
                .map_err(PwError::PipeWire)?;

        let tracker = Rc::new(RefCell::new(RegistryTracker::new(
            Arc::clone(shared),
            Arc::clone(cmd_efd),
            config.rate_policy.clone(),
            init_rate,
            channels,
            config.quantum,
            dsp_stream.clone(),
            direct_stream.clone(),
        )));

        let pending_init_sync = Rc::new(std::cell::Cell::new(None::<AsyncSeq>));
        let startup_synced = Rc::new(std::cell::Cell::new(false));

        let tracker_done = Rc::clone(&tracker);
        let init_sync_done = Rc::clone(&pending_init_sync);
        let synced_done = Rc::clone(&startup_synced);
        let on_core_done_cb = Rc::clone(&on_core_done);
        let core_listener = core
            .add_listener_local()
            .done(move |_id, seq| {
                if init_sync_done.get() == Some(seq) {
                    init_sync_done.set(None);
                    synced_done.set(true);
                    tracker_done.borrow_mut().on_initial_metadata_synced();
                }
                on_core_done_cb(seq);
            })
            .register();

        // Register registry listener to discover `settings` metadata, client nodes, and links.
        let reg_weak = registry.downgrade();
        let core_global = core.clone();
        let init_sync_global = Rc::clone(&pending_init_sync);
        let synced_global = Rc::clone(&startup_synced);
        let tracker_global = Rc::clone(&tracker);
        let tracker_remove = Rc::clone(&tracker);
        let reg_listener = registry
            .add_listener_local()
            .global(move |global| {
                let Some(reg) = reg_weak.upgrade() else {
                    return;
                };
                match global.type_ {
                    pw::types::ObjectType::Metadata => {
                        let is_settings = global
                            .props
                            .is_some_and(|p| p.get("metadata.name") == Some("settings"));
                        if is_settings
                            && let Ok(meta) = reg.bind::<pw::metadata::Metadata, _>(global)
                        {
                            let t_meta = Rc::clone(&tracker_global);
                            let meta_id = global.id;
                            let listener = meta
                                .add_listener_local()
                                .property(move |subject, key, type_, value| {
                                    t_meta
                                        .borrow_mut()
                                        .meta_tracker
                                        .on_property(subject, key, type_, value);
                                    0
                                })
                                .register();
                            {
                                let mut t = tracker_global.borrow_mut();
                                t.metadata = Some((meta_id, meta, listener));
                            }
                            if !synced_global.get() {
                                if let Ok(seq) = core_global.sync(0) {
                                    init_sync_global.set(Some(seq));
                                }
                            } else {
                                let init = tracker_global.borrow().forced_rate;
                                tracker_global
                                    .borrow_mut()
                                    .apply_policy_to_metadata(Some(init));
                            }
                        }
                    }
                    pw::types::ObjectType::Node => {
                        let is_audio_stream = global
                            .props
                            .is_some_and(|p| p.get("media.class") == Some("Stream/Output/Audio"));
                        if is_audio_stream && let Ok(node) = reg.bind::<pw::node::Node, _>(global) {
                            let nid = global.id;
                            let initial_rate = global
                                .props
                                .and_then(|p| p.get("node.rate"))
                                .and_then(parse_node_rate);
                            let name = global
                                .props
                                .and_then(|p| {
                                    ["application.name", "node.description", "node.name"]
                                        .into_iter()
                                        .find_map(|k| p.get(k).filter(|v| !v.is_empty()))
                                })
                                .map_or_else(|| format!("node {nid}"), str::to_string);
                            let t_node = Rc::clone(&tracker_global);
                            let listener = node
                                .add_listener_local()
                                .info(move |info| {
                                    let running =
                                        matches!(info.state(), pw::node::NodeState::Running);
                                    let parsed_rate = info
                                        .props()
                                        .and_then(|p| p.get("node.rate"))
                                        .and_then(parse_node_rate);
                                    t_node
                                        .borrow_mut()
                                        .on_client_info(nid, running, parsed_rate);
                                })
                                .register();
                            let mut t = tracker_global.borrow_mut();
                            t.clients.insert(
                                nid,
                                ClientNodeEntry {
                                    running: false,
                                    native_rate: initial_rate,
                                    name,
                                    since: None,
                                    _node: node,
                                    _listener: listener,
                                },
                            );
                        }
                    }
                    pw::types::ObjectType::Link => {
                        if let Ok(link) = reg.bind::<pw::link::Link, _>(global) {
                            let lid = global.id;
                            let t_link = Rc::clone(&tracker_global);
                            let listener = link
                                .add_listener_local()
                                .info(move |info| {
                                    let out_node = info.output_node_id();
                                    let in_node = info.input_node_id();
                                    let state = info.state();
                                    log::debug!(
                                        "link {lid}: {out_node} -> {in_node} state={state:?}"
                                    );
                                    let mut t = t_link.borrow_mut();
                                    t.links_by_id.insert(lid, (out_node, in_node));
                                    t.recompute_routing_and_rate();
                                })
                                .register();
                            let mut t = tracker_global.borrow_mut();
                            t.links.insert(
                                lid,
                                LinkEntry {
                                    _link: link,
                                    _listener: listener,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            })
            .global_remove(move |id| {
                let mut t = tracker_remove.borrow_mut();
                let mut changed = false;
                if t.clients.remove(&id).is_some() {
                    changed = true;
                }
                if t.links.remove(&id).is_some() {
                    t.links_by_id.remove(&id);
                    changed = true;
                }
                if let Some((mid, _, _)) = &t.metadata
                    && *mid == id
                {
                    t.metadata = None;
                }
                if changed {
                    t.recompute_routing_and_rate();
                }
            })
            .register();

        // Issue initial registry sync barrier; if `settings` metadata is bound
        // during registry enumeration, `global` updates `pending_init_sync` to
        // a fresh `core.sync(0)` so initial property events settle first.
        if let Ok(seq) = core.sync(0)
            && pending_init_sync.get().is_none()
        {
            pending_init_sync.set(Some(seq));
        }

        let register_stream_callbacks =
            |stream: &pw::stream::StreamRc,
             is_direct: bool,
             peer_direct: Option<pw::stream::StreamRc>,
             prod: rtrb::Producer<f32>|
             -> Result<pw::stream::StreamListener<StreamSideData>> {
                let data = StreamSideData {
                    is_direct,
                    volume_asserted: false,
                    channels: channels as usize,
                    quantum_48k: config.quantum,
                    peer_direct,
                    shared: Arc::clone(shared),
                    cmd_efd: Arc::clone(cmd_efd),
                    data_efd: Arc::clone(data_efd),
                    prod,
                };
                stream
                    .add_local_listener_with_user_data(data)
                    .state_changed(|stream, data, _old, new| {
                        use pw::stream::StreamState as S;
                        let nid = stream.node_id();
                        if nid != pw::constants::ID_ANY {
                            if data.is_direct {
                                data.shared.direct_node_id.store(nid, Ordering::Release);
                            } else {
                                data.shared.dsp_node_id.store(nid, Ordering::Release);
                            }
                        }
                        if matches!(new, S::Paused | S::Streaming) && !data.volume_asserted {
                            data.volume_asserted = true;
                            let sink = data.sink_choice();
                            data.shared.sink_volume.forget_shown(sink);
                            sink_volume::assert_owned(
                                stream,
                                &data.shared.sink_volume,
                                sink,
                                data.channels,
                                false,
                            );
                        }
                        match new {
                            S::Streaming => {
                                if data.is_direct {
                                    data.shared.direct_streaming.store(true, Ordering::Release);
                                    data.shared
                                        .stream_starts_direct
                                        .fetch_add(1, Ordering::Relaxed);
                                } else {
                                    let driving = stream.is_driving();
                                    data.shared.driving.store(driving, Ordering::Release);
                                    data.shared.dsp_streaming.store(true, Ordering::Release);
                                    data.shared
                                        .stream_starts_dsp
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                data.cmd_efd.signal();
                            }
                            S::Paused => {
                                if data.is_direct {
                                    data.shared.direct_streaming.store(false, Ordering::Release);
                                    data.shared
                                        .stream_pauses_direct
                                        .fetch_add(1, Ordering::Relaxed);
                                } else {
                                    data.shared.dsp_streaming.store(false, Ordering::Release);
                                    data.shared.driving.store(false, Ordering::Release);
                                    data.shared
                                        .stream_pauses_dsp
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                data.cmd_efd.signal();
                            }
                            S::Unconnected => {
                                if data.is_direct {
                                    data.shared.direct_streaming.store(false, Ordering::Release);
                                    data.shared
                                        .direct_node_id
                                        .store(pw::constants::ID_ANY, Ordering::Release);
                                } else {
                                    data.shared.dsp_streaming.store(false, Ordering::Release);
                                    data.shared.driving.store(false, Ordering::Release);
                                    data.shared
                                        .dsp_node_id
                                        .store(pw::constants::ID_ANY, Ordering::Release);
                                    data.shared.io.clear_all();
                                }
                                data.cmd_efd.signal();
                            }
                            S::Error(msg) => {
                                log::error!(
                                    "PipeWire sink error (direct={}): {msg}",
                                    data.is_direct
                                );
                                data.shared.set_error(format!("PipeWire error: {msg}"));
                                data.cmd_efd.signal();
                            }
                            _ => {}
                        }
                    })
                    .io_changed(|_stream, data, id, area, size| {
                        if data.is_direct {
                            return;
                        }
                        // SynchronizedIo::store_* waits until ALSA reader drops old pointer.
                        if id == spa::sys::SPA_IO_Position {
                            if !area.is_null()
                                && (size as usize)
                                    >= std::mem::size_of::<spa::sys::spa_io_position>()
                            {
                                data.shared.io.store_position(area.cast());
                            } else {
                                data.shared.io.store_position(std::ptr::null_mut());
                            }
                        } else if id == spa::sys::SPA_IO_Clock {
                            if !area.is_null()
                                && (size as usize) >= std::mem::size_of::<spa::sys::spa_io_clock>()
                            {
                                data.shared.io.store_clock(area.cast());
                            } else {
                                data.shared.io.store_clock(std::ptr::null_mut());
                            }
                        }
                    })
                    .param_changed(|stream, data, id, param| {
                        let Some(param) = param else { return };
                        if id == spa::param::ParamType::Props.as_raw() {
                            sink_volume::on_props_changed(
                                stream,
                                &data.shared.sink_volume,
                                data.sink_choice(),
                                data.channels,
                                param,
                            );
                            return;
                        }
                        if id != spa::param::ParamType::Format.as_raw() {
                            return;
                        }
                        if let Ok((MediaType::Audio, MediaSubtype::Raw)) =
                            format_utils::parse_format(param)
                        {
                            let mut info = AudioInfoRaw::new();
                            if info.parse(param).is_ok() {
                                if info.channels() > 0 {
                                    data.channels = info.channels() as usize;
                                }
                                if !data.is_direct && info.rate() > 0 {
                                    let r = info.rate();
                                    data.shared.graph_rate.store(r, Ordering::Relaxed);
                                    let q = scaled_quantum(data.quantum_48k, r);
                                    update_stream_latency(stream, q, r);
                                    if let Some(ref peer) = data.peer_direct {
                                        update_stream_latency(peer, q, r);
                                        update_stream_format_rate(peer, data.channels as u32, r);
                                    }
                                }
                            }
                        }
                    })
                    .process(|stream, data| {
                        process_sink_buffer(stream, data);
                        data.shared
                            .cycle_counter(data.is_direct)
                            .fetch_add(1, Ordering::Release);
                        data.data_efd.signal();
                    })
                    .register()
                    .map_err(PwError::PipeWire)
            };

        let dsp_listener =
            register_stream_callbacks(&dsp_stream, false, Some(direct_stream.clone()), dsp_prod)?;
        let direct_listener = register_stream_callbacks(&direct_stream, true, None, direct_prod)?;
        connect_both_streams(&dsp_stream, &direct_stream, channels, &config.rate_policy)?;
        let raw = NonNull::new(dsp_stream.as_raw_ptr()).expect("dsp stream non-null");

        Ok((
            Self {
                tracker,
                pending_init_sync,
                startup_synced,
                _core_listener: core_listener,
                _reg_listener: reg_listener,
                _dsp_listener: dsp_listener,
                _direct_listener: direct_listener,
                dsp_stream,
                direct_stream,
                channels: channels as usize,
                shared: Arc::clone(shared),
                _registry: registry,
                core,
                _context: context,
            },
            SendRawStream(raw),
        ))
    }

    /// Show the engine's volume on both sinks (or release them).
    fn apply_sink_volume(&self, follow_changed: bool) {
        let shared = &self.shared.sink_volume;
        for (stream, sink) in [
            (&self.dsp_stream, SinkChoice::Dsp),
            (&self.direct_stream, SinkChoice::Direct),
        ] {
            if follow_changed {
                sink_volume::update_restore_props(stream, shared.follow());
            }
            sink_volume::assert_owned(stream, shared, sink, self.channels, follow_changed);
        }
    }

    fn force_initial_synced(&self) {
        if !self.startup_synced.get() {
            self.pending_init_sync.set(None);
            self.startup_synced.set(true);
            self.tracker.borrow_mut().on_initial_metadata_synced();
        }
    }

    fn begin_teardown(&self) -> (Option<AsyncSeq>, bool) {
        let _ = self.direct_stream.disconnect();
        let _ = self.dsp_stream.disconnect();
        self.shared.direct_streaming.store(false, Ordering::Release);
        self.shared.dsp_streaming.store(false, Ordering::Release);
        self.shared.driving.store(false, Ordering::Release);
        self.shared
            .direct_node_id
            .store(pw::constants::ID_ANY, Ordering::Release);
        self.shared
            .dsp_node_id
            .store(pw::constants::ID_ANY, Ordering::Release);
        self.shared.direct_has_links.store(false, Ordering::Release);
        self.shared.dsp_has_links.store(false, Ordering::Release);
        self.shared
            .direct_client_running
            .store(false, Ordering::Release);
        self.shared
            .dsp_client_running
            .store(false, Ordering::Release);
        self.shared.io.clear_all();
        let meta_dirty = self.tracker.borrow_mut().restore_metadata();
        (self.core.sync(0).ok(), meta_dirty)
    }

    fn finish_teardown(self, remove_restore_file: bool) {
        if remove_restore_file {
            self.tracker.borrow_mut().meta_tracker.remove_restore_file();
        }
    }
}

enum TeardownAction {
    Disconnect {
        ack_tx: mpsc::SyncSender<Result<()>>,
    },
    Reconnect {
        config: Box<EngineDriverConfig>,
        last_rate: u32,
        dsp_prod: rtrb::Producer<f32>,
        direct_prod: rtrb::Producer<f32>,
        reply_tx: mpsc::SyncSender<Result<SendRawStream>>,
    },
    Stop,
}

struct PendingTeardown {
    session: PwSession,
    sync_seq: Option<AsyncSeq>,
    meta_dirty_at_teardown: bool,
    action: TeardownAction,
}

struct PendingReconnectSync {
    raw: SendRawStream,
    reply_tx: mpsc::SyncSender<Result<SendRawStream>>,
}

struct PwLoopState {
    session: Option<PwSession>,
    teardown: Option<PendingTeardown>,
    reconnect_sync: Option<PendingReconnectSync>,
    deadline: Option<Instant>,
}

#[allow(clippy::too_many_arguments)]
fn run_pw_loop(
    config: EngineDriverConfig,
    init_rate: u32,
    init_quantum: u32,
    shared: Arc<SharedEngineState>,
    cmd_efd: Arc<EventFd>,
    data_efd: Arc<EventFd>,
    _ctrl_efd: Arc<EventFd>,
    dsp_prod: rtrb::Producer<f32>,
    direct_prod: rtrb::Producer<f32>,
    pw_rx: pw::channel::Receiver<PwMsg>,
    ready_tx: mpsc::SyncSender<Result<SendRawStream>>,
) {
    pw::init();

    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            let _ = ready_tx.send(Err(PwError::PipeWire(e)));
            return;
        }
    };

    let state: Rc<RefCell<PwLoopState>> = Rc::new(RefCell::new(PwLoopState {
        session: None,
        teardown: None,
        reconnect_sync: None,
        deadline: None,
    }));

    let complete_teardown = {
        let shared = Arc::clone(&shared);
        let cmd_efd = Arc::clone(&cmd_efd);
        let data_efd = Arc::clone(&data_efd);
        move |st_rc: &Rc<RefCell<PwLoopState>>,
              mainloop: &pw::main_loop::MainLoopRc,
              on_done: &Rc<dyn Fn(AsyncSeq)>,
              timed_out: bool| {
            let taken = {
                let mut st = st_rc.borrow_mut();
                st.deadline = None;
                st.teardown.take()
            };
            let Some(td) = taken else {
                return;
            };
            let remove_restore = !timed_out || !td.meta_dirty_at_teardown;
            if timed_out && td.meta_dirty_at_teardown {
                log::error!(
                    "PipeWire teardown sync timed out; preserving settings-restore.json for crash recovery"
                );
            }
            td.session.finish_teardown(remove_restore);
            match td.action {
                TeardownAction::Disconnect { ack_tx } => {
                    let res = if timed_out {
                        Err(PwError::Init(
                            "PipeWire teardown acknowledgement timed out".into(),
                        ))
                    } else {
                        Ok(())
                    };
                    let _ = ack_tx.send(res);
                }
                TeardownAction::Reconnect {
                    config,
                    last_rate,
                    dsp_prod,
                    direct_prod,
                    reply_tx,
                } => {
                    let q = scaled_quantum(config.quantum, last_rate);
                    match PwSession::create(
                        mainloop,
                        &config,
                        last_rate,
                        q,
                        &shared,
                        &cmd_efd,
                        &data_efd,
                        dsp_prod,
                        direct_prod,
                        Rc::clone(on_done),
                    ) {
                        Ok((sess, raw)) => {
                            let synced = sess.startup_synced.get();
                            let mut st = st_rc.borrow_mut();
                            st.session = Some(sess);
                            if synced {
                                drop(st);
                                let _ = reply_tx.send(Ok(raw));
                            } else {
                                st.reconnect_sync = Some(PendingReconnectSync { raw, reply_tx });
                                st.deadline = Some(Instant::now() + Duration::from_millis(1000));
                            }
                        }
                        Err(e) => {
                            let _ = reply_tx.send(Err(e));
                        }
                    }
                }
                TeardownAction::Stop => {
                    mainloop.quit();
                }
            }
        }
    };

    type OnCoreDoneCb = Rc<dyn Fn(AsyncSeq)>;
    let on_done_slot: Rc<RefCell<Option<OnCoreDoneCb>>> = Rc::new(RefCell::new(None));
    let on_core_done: OnCoreDoneCb = {
        let st_rc = Rc::clone(&state);
        let mainloop = mainloop.clone();
        let complete = complete_teardown.clone();
        let slot_weak = Rc::downgrade(&on_done_slot);
        Rc::new(move |seq: AsyncSeq| {
            let mut teardown_ready = false;
            let mut reconnect_ready: Option<(
                mpsc::SyncSender<Result<SendRawStream>>,
                SendRawStream,
            )> = None;
            {
                let mut st = st_rc.borrow_mut();
                if let Some(ref td) = st.teardown
                    && td.sync_seq == Some(seq)
                {
                    teardown_ready = true;
                }
                if st.reconnect_sync.is_some()
                    && st.session.as_ref().is_some_and(|s| s.startup_synced.get())
                    && let Some(rc) = st.reconnect_sync.take()
                {
                    st.deadline = None;
                    reconnect_ready = Some((rc.reply_tx, rc.raw));
                }
            }
            if let Some((tx, raw)) = reconnect_ready {
                let _ = tx.send(Ok(raw));
            }
            if teardown_ready
                && let Some(slot_rc) = slot_weak.upgrade()
                && let Some(me) = slot_rc.borrow().clone()
            {
                complete(&st_rc, &mainloop, &me, false);
            }
        })
    };
    *on_done_slot.borrow_mut() = Some(Rc::clone(&on_core_done));

    let _fallback_timer = mainloop.loop_().add_timer({
        let st_rc = Rc::clone(&state);
        let mainloop = mainloop.clone();
        let complete = complete_teardown.clone();
        let on_done = Rc::clone(&on_core_done);
        move |_exp| {
            if let Some(ref sess) = st_rc.borrow().session {
                sess.tracker.borrow_mut().poll_rate_recheck();
            }
            let expired = st_rc.borrow().deadline.is_some_and(|d| Instant::now() >= d);
            if !expired {
                return;
            }
            let has_teardown = st_rc.borrow().teardown.is_some();
            if has_teardown {
                complete(&st_rc, &mainloop, &on_done, true);
                return;
            }
            let reconnect = {
                let mut st = st_rc.borrow_mut();
                st.deadline = None;
                if let Some(rc) = st.reconnect_sync.take() {
                    if let Some(ref sess) = st.session {
                        sess.force_initial_synced();
                    }
                    Some(rc)
                } else {
                    None
                }
            };
            if let Some(rc) = reconnect {
                let _ = rc.reply_tx.send(Ok(rc.raw));
            }
        }
    });
    let _ = _fallback_timer.update_timer(
        Some(Duration::from_millis(100)),
        Some(Duration::from_millis(100)),
    );

    let (init_session, raw_stream) = match PwSession::create(
        &mainloop,
        &config,
        init_rate,
        init_quantum,
        &shared,
        &cmd_efd,
        &data_efd,
        dsp_prod,
        direct_prod,
        Rc::clone(&on_core_done),
    ) {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    state.borrow_mut().session = Some(init_session);

    // Wait for initial metadata snapshot + core.sync barrier before advertising
    // readiness, so originals + settings-restore.json are captured cleanly.
    let sync_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < sync_deadline {
        let done = state
            .borrow()
            .session
            .as_ref()
            .is_some_and(|s| s.startup_synced.get());
        if done {
            break;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
    }
    if let Some(ref sess) = state.borrow().session {
        sess.force_initial_synced();
    }
    let _ = ready_tx.send(Ok(raw_stream));

    let _rx_guard = pw_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let st_rc = Rc::clone(&state);
        let shared = Arc::clone(&shared);
        let cmd_efd = Arc::clone(&cmd_efd);
        let data_efd = Arc::clone(&data_efd);
        let on_done = Rc::clone(&on_core_done);
        move |msg| match msg {
            PwMsg::ApplyRatePolicy {
                policy,
                quantum_48k,
                last_rate,
            } => {
                if let Some(ref s) = st_rc.borrow().session {
                    let mut t = s.tracker.borrow_mut();
                    let rates_changed = t.policy.allowed_rates != policy.allowed_rates
                        || t.policy.default_rate != policy.default_rate;
                    t.policy = policy.clone();
                    t.quantum_48k = quantum_48k;
                    t.rate_following_disabled = false;
                    if rates_changed {
                        t.publish_allowed_rates_on_streams();
                    }
                    let before = t.forced_rate;
                    // Never force a rate outside the (device-restricted) set,
                    // and move to a native one (never to a rate the engine
                    // bypass resampler converts).
                    let follow_rate = if policy.allowed_rates.is_empty() {
                        last_rate
                    } else {
                        let native = shared.sink_policies().map(|p| p.dsp_native);
                        crate::rate_arbiter::sink_rate(last_rate, &policy, native.as_ref())
                    };
                    // With following off this is still the default rate,
                    // unless the Direct sink had to open at a native rate.
                    t.apply_policy_to_metadata(if policy.follow_source {
                        Some(follow_rate)
                    } else {
                        Some(last_rate)
                    });
                    if t.forced_rate != before {
                        t.arbiter.note_switch(Instant::now());
                    }
                    // The conflict mode may have changed: decide again.
                    t.recompute_routing_and_rate();
                }
            }
            PwMsg::SinkVolume { follow_changed } => {
                if let Some(ref s) = st_rc.borrow().session {
                    s.apply_sink_volume(follow_changed);
                }
            }
            PwMsg::DisconnectAndRestore { ack_tx } => {
                let prev = st_rc.borrow_mut().session.take();
                if let Some(sess) = prev {
                    let (sync_seq, meta_dirty) = sess.begin_teardown();
                    if sync_seq.is_some() {
                        let mut st = st_rc.borrow_mut();
                        st.teardown = Some(PendingTeardown {
                            session: sess,
                            sync_seq,
                            meta_dirty_at_teardown: meta_dirty,
                            action: TeardownAction::Disconnect { ack_tx },
                        });
                        st.deadline = Some(Instant::now() + Duration::from_millis(600));
                    } else {
                        sess.finish_teardown(!meta_dirty);
                        let _ = ack_tx.send(Err(PwError::Init(
                            "PipeWire teardown sync failed before acknowledgement".into(),
                        )));
                    }
                } else {
                    let _ = ack_tx.send(Ok(()));
                }
            }
            PwMsg::Reconnect {
                config,
                last_rate,
                dsp_prod,
                direct_prod,
                reply_tx,
            } => {
                let prev = st_rc.borrow_mut().session.take();
                if let Some(old) = prev {
                    let (sync_seq, meta_dirty) = old.begin_teardown();
                    if sync_seq.is_some() {
                        let mut st = st_rc.borrow_mut();
                        st.teardown = Some(PendingTeardown {
                            session: old,
                            sync_seq,
                            meta_dirty_at_teardown: meta_dirty,
                            action: TeardownAction::Reconnect {
                                config,
                                last_rate,
                                dsp_prod,
                                direct_prod,
                                reply_tx,
                            },
                        });
                        st.deadline = Some(Instant::now() + Duration::from_millis(600));
                        return;
                    }
                    old.finish_teardown(!meta_dirty);
                }

                let q = scaled_quantum(config.quantum, last_rate);
                match PwSession::create(
                    &mainloop,
                    &config,
                    last_rate,
                    q,
                    &shared,
                    &cmd_efd,
                    &data_efd,
                    dsp_prod,
                    direct_prod,
                    Rc::clone(&on_done),
                ) {
                    Ok((sess, raw)) => {
                        let synced = sess.startup_synced.get();
                        let mut st = st_rc.borrow_mut();
                        st.session = Some(sess);
                        if synced {
                            drop(st);
                            let _ = reply_tx.send(Ok(raw));
                        } else {
                            st.reconnect_sync = Some(PendingReconnectSync { raw, reply_tx });
                            st.deadline = Some(Instant::now() + Duration::from_millis(1000));
                        }
                    }
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                    }
                }
            }
            PwMsg::Stop => {
                let prev = st_rc.borrow_mut().session.take();
                if let Some(sess) = prev {
                    let (sync_seq, meta_dirty) = sess.begin_teardown();
                    if sync_seq.is_some() {
                        let mut st = st_rc.borrow_mut();
                        st.teardown = Some(PendingTeardown {
                            session: sess,
                            sync_seq,
                            meta_dirty_at_teardown: meta_dirty,
                            action: TeardownAction::Stop,
                        });
                        st.deadline = Some(Instant::now() + Duration::from_millis(600));
                    } else {
                        sess.finish_teardown(!meta_dirty);
                        mainloop.quit();
                    }
                } else {
                    mainloop.quit();
                }
            }
        }
    });

    mainloop.run();

    // Ensure session/restore state is cleaned up even if loop exited early.
    if let Some(td) = state.borrow_mut().teardown.take() {
        td.session.finish_teardown(!td.meta_dirty_at_teardown);
    }
    if let Some(sess) = state.borrow_mut().session.take() {
        let (_seq, meta_dirty) = sess.begin_teardown();
        sess.finish_teardown(!meta_dirty);
    }
}

// ---------------------------------------------------------------- Control thread

struct RealDeviceBackend {
    formats: Vec<SampleFormat>,
    channels: u32,
    buffer_periods: u32,
    ctrl_efd: Arc<EventFd>,
    shared: Arc<SharedEngineState>,
    /// Driver config the effective rate policy is computed from (configured
    /// `rate_policy`, formats, quantum, max ratio).
    config: EngineDriverConfig,
    /// Device the cached probe belongs to (set even when the probe failed,
    /// so a failing probe is not retried on every rate switch).
    probed_device: Option<String>,
    caps: Option<DeviceCapabilities>,
    /// PCM opened for the probe, handed to the next `open_and_prepare` so the
    /// device is opened once per reopen.
    probe_handle: Option<OpenPcm>,
    /// Last computed effective rate policy for `probed_device`.
    effective: Option<RatePolicy>,
    /// Back-to-back attempts to move the graph that PipeWire did not follow
    /// ([`MoveAttempts`]).
    moves: Option<MoveAttempts>,
}

impl RealDeviceBackend {
    /// Forget the probe (release, error, replug): the next open probes again.
    fn invalidate_caps(&mut self) {
        self.probed_device = None;
        self.caps = None;
        self.probe_handle = None;
        self.effective = None;
        self.moves = None;
    }

    /// After a failed open: probe `device` again (holding no handle), so
    /// the status says whether it is there now and what it plays, never
    /// what an earlier probe of it found.
    fn reprobe_after_failure(&mut self, device: &str) {
        self.invalidate_caps();
        let caps = match self.ensure_caps(device) {
            Ok(()) => self.caps_for(device).cloned(),
            Err(_) => None,
        };
        self.probe_handle = None;
        self.shared.set_device_caps(caps);
    }

    fn caps_for(&self, device: &str) -> Option<&DeviceCapabilities> {
        if self.probed_device.as_deref() == Some(device) {
            self.caps.as_ref()
        } else {
            None
        }
    }

    /// Probe `device` unless it is the one probed last.
    fn ensure_caps(&mut self, device: &str) -> std::result::Result<(), String> {
        if self.probed_device.as_deref() == Some(device) {
            return Ok(());
        }
        self.invalidate_caps();
        if test_unplugged() {
            return Err(TEST_UNPLUGGED_MSG.into());
        }
        let open = OpenPcm::open(device, true).map_err(|e| e.to_string())?;
        let caps = match open.probe(
            self.channels,
            &engine_probe_rates(self.config.max_dsp_ratio),
        ) {
            Ok(c) => {
                log::info!("Device '{device}' supports {}", c.summary());
                self.shared.note_probe();
                Some(c)
            }
            Err(e) => {
                log::warn!("Probing '{device}' failed ({e}); trying rates blind");
                None
            }
        };
        self.probed_device = Some(device.to_string());
        self.caps = caps;
        self.probe_handle = Some(open);
        Ok(())
    }

    /// Recompute the effective policy for the cached probe (after a config
    /// change). `None` when the cache is for another device.
    fn refresh_effective(
        &mut self,
        desired: DesiredDspConfig,
    ) -> Option<std::result::Result<RatePolicy, String>> {
        if self.probed_device.as_deref() != Some(self.config.device.as_str()) {
            return None;
        }
        let res = compute_effective_policy(&self.config, desired, self.caps.as_ref()).map(|p| {
            crate::rate_caps::rate_policies(p, self.caps.as_ref(), &self.config.formats, desired)
        });
        if let Ok(p) = &res {
            self.effective = Some(p.published.clone());
            self.shared.set_policies(p);
        }
        Some(res.map(|p| p.published))
    }
}

impl DeviceBackend for RealDeviceBackend {
    type Reservation = DeviceReservation;
    type Output = Box<AlsaOutput>;

    fn playback_session(&self) -> u64 {
        self.shared.playback_session.load(Ordering::Acquire)
    }

    fn card_index(&mut self, device: &str) -> std::result::Result<u32, String> {
        if test_unplugged() {
            return Err(TEST_UNPLUGGED_MSG.into());
        }
        card_index_from_device(device).map_err(|e| e.to_string())
    }

    fn reserve(
        &mut self,
        device: &str,
        _card: u32,
        priority: i32,
    ) -> std::result::Result<Self::Reservation, String> {
        let efd_cb = Arc::clone(&self.ctrl_efd);
        let flag_cb = Arc::clone(&self.shared);
        DeviceReservation::acquire_for_device_with_wakeup(
            device,
            priority,
            Arc::new(move || {
                flag_cb.release_requested.store(true, Ordering::Release);
                efd_cb.signal();
            }),
        )
        .map_err(|e| e.to_string())
    }

    fn is_release_requested(&self, res: &Self::Reservation) -> bool {
        res.release_requested()
    }

    fn release_reservation(&mut self, res: Self::Reservation) {
        drop(res);
    }

    fn resolve_graph_rate(
        &mut self,
        device: &str,
        graph_rate: u32,
        quantum: u32,
        target_sink: SinkChoice,
        desired_dsp: DesiredDspConfig,
    ) -> std::result::Result<(u32, u32), String> {
        if let Err(e) = self.ensure_caps(device) {
            // Not there (or not answering): no capabilities of an earlier
            // probe stay in the status.
            self.shared.set_device_caps(None);
            return Err(e);
        }
        let caps = self.caps_for(device).cloned();
        let eff = match compute_effective_policy(&self.config, desired_dsp, caps.as_ref()) {
            Ok(p) => p,
            Err(msg) => {
                // Unusable device: still report what it supports.
                self.probe_handle = None;
                self.shared.set_device_caps(caps);
                self.shared.clear_policies();
                return Err(msg);
            }
        };
        let policies =
            crate::rate_caps::rate_policies(eff, caps.as_ref(), &self.config.formats, desired_dsp);
        let eff = policies.published.clone();
        self.shared.set_device_caps(caps);
        self.shared.set_policies(&policies);
        self.effective = Some(eff.clone());
        let rate = match crate::rate_caps::plan_graph_rate(&policies, graph_rate, target_sink) {
            GraphRate::Keep => {
                self.moves = None;
                return Ok((graph_rate, quantum));
            }
            GraphRate::Move(rate) => rate,
            GraphRate::DirectCannotPlay => {
                let rates: Vec<String> = eff.allowed_rates.iter().map(u32::to_string).collect();
                let supports = self
                    .caps_for(device)
                    .map(|c| c.summary())
                    .unwrap_or_else(|| "unknown".into());
                self.probe_handle = None;
                return Err(format!(
                    "Audivane Direct cannot play on '{device}': it does not upsample, and none of the allowed rates ({} Hz) is native to the device (device supports {supports}). Route the app to Audivane DSP, or allow a native rate",
                    rates.join(", ")
                ));
            }
        };
        let bypass = target_sink == SinkChoice::Direct || !desired_dsp.dsp_enabled;
        if MoveAttempts::note(
            &mut self.moves,
            graph_rate,
            rate,
            mono_now_ns(),
            self.shared.graph_landings.load(Ordering::Acquire),
        ) {
            // The graph stays where it is (another driver or client fixes
            // the rate): reopening for the rate it would move to only
            // loops. Say so; the DSP path keeps working.
            self.moves = None;
            self.probe_handle = None;
            return Err(if bypass {
                format!(
                    "{} cannot play {graph_rate} Hz on '{device}': the device does not play it natively, and PipeWire keeps the graph at {graph_rate} Hz instead of moving it to {rate} Hz (another client or driver fixes the rate). {}",
                    if target_sink == SinkChoice::Direct {
                        "Audivane Direct"
                    } else {
                        "With DSP off, Audivane"
                    },
                    if target_sink == SinkChoice::Direct {
                        "Route the app to Audivane DSP, or let the graph rate change"
                    } else {
                        "Turn DSP on, or let the graph rate change"
                    }
                )
            } else {
                format!(
                    "'{device}' cannot play a {graph_rate} Hz graph, and PipeWire keeps it there instead of moving it to {rate} Hz (another client or driver fixes the rate)"
                )
            });
        }
        if target_sink == SinkChoice::Direct {
            // Direct is bit-perfect at x1, and the device does not play this
            // rate natively (it may only take it upsampled through DSP):
            // run the graph at a native rate instead; PipeWire resamples the
            // stream into Direct, which status reports as not bit-perfect.
            log::info!(
                "'{device}' cannot play {graph_rate} Hz natively for Audivane Direct; moving the graph to {rate} Hz (the stream is resampled)"
            );
        } else {
            // The graph is at a rate this device cannot play (device switch,
            // DSP off, or PipeWire had not applied our allowed rates yet).
            // Open at the rate we are about to force instead; never at graph
            // rate != device rate / ratio.
            log::info!(
                "'{device}' cannot play a {graph_rate} Hz graph; moving the graph to {rate} Hz"
            );
        }
        Ok((rate, scaled_quantum(self.config.quantum, rate)))
    }

    fn open_and_prepare(
        &mut self,
        device: &str,
        graph_rate: u32,
        ladder: &[RateRatio],
        quantum: u32,
    ) -> std::result::Result<OpenedDevice<Self::Output>, String> {
        let desired_ratio = ladder.first().copied().unwrap_or(RateRatio::ONE);
        let alsa_cfg = AlsaConfig {
            device: device.to_string(),
            rate: desired_ratio.output_rate(graph_rate).unwrap_or(graph_rate),
            channels: self.channels,
            period_frames: desired_ratio.scale_to_output(quantum),
            buffer_periods: self.buffer_periods.max(2),
            preferred_format: self.formats.first().copied(),
            strict_format: false,
            nonblock: true,
        };
        let open = match self.probe_handle.take() {
            Some(h) if h.device() == device => h,
            _ => OpenPcm::open(device, true).map_err(|e| e.to_string())?,
        };
        let caps = self.caps_for(device).cloned();
        let (mut alsa, plan) = open
            .configure_with_ratio_plan(
                &alsa_cfg,
                graph_rate,
                ladder,
                quantum,
                &self.formats,
                caps.as_ref(),
            )
            .map_err(|e| e.to_string())?;
        alsa.prepare().map_err(|e| e.to_string())?;
        let output_rate = alsa.params().rate;
        Ok(OpenedDevice {
            output: Box::new(alsa),
            actual_ratio: plan.ratio,
            output_rate,
            graph_rate,
            quantum,
            plan,
            session: 0,
        })
    }

    fn close_output(&mut self, out: Self::Output) {
        drop(out);
    }

    fn ratio_ladder(
        &self,
        desired_dsp: DesiredDspConfig,
        target_sink: SinkChoice,
        graph_rate: u32,
    ) -> Vec<RateRatio> {
        // `resolve_graph_rate` probed the target device just before.
        let caps = self.probed_device.as_ref().and(self.caps.as_ref());
        desired_dsp.ratio_ladder_for(target_sink, graph_rate, caps, &self.formats)
    }
}

impl RealDeviceBackend {
    /// The probed rates of the device [`DeviceBackend::ratio_ladder`] opens
    /// (the key of the open, [`negotiated_key`]).
    fn device_rates(&self) -> Option<DeviceRates> {
        DeviceRates::playable(
            self.probed_device.as_ref().and(self.caps.as_ref()),
            &self.formats,
        )
    }
}

fn flush_pending_rt_cmds(
    pending: &mut VecDeque<RtControlCmd>,
    tx: &mut rtrb::Producer<RtControlCmd>,
    cmd_efd: &EventFd,
) {
    let mut pushed_any = false;
    while let Some(cmd) = pending.pop_front() {
        match tx.push(cmd) {
            Ok(()) => pushed_any = true,
            Err(rtrb::PushError::Full(returned)) => {
                pending.push_front(returned);
                break;
            }
        }
    }
    if pushed_any {
        cmd_efd.signal();
    }
}

fn push_or_queue_rt_cmd(
    pending: &mut VecDeque<RtControlCmd>,
    tx: &mut rtrb::Producer<RtControlCmd>,
    cmd_efd: &EventFd,
    cmd: RtControlCmd,
) {
    flush_pending_rt_cmds(pending, tx, cmd_efd);
    if pending.is_empty() {
        match tx.push(cmd) {
            Ok(()) => cmd_efd.signal(),
            Err(rtrb::PushError::Full(returned)) => {
                pending.push_back(returned);
                cmd_efd.signal();
            }
        }
    } else {
        pending.push_back(cmd);
        cmd_efd.signal();
    }
}

fn finish_waiters(
    waiters: &mut Vec<mpsc::SyncSender<Result<()>>>,
    make_err: impl Fn() -> Result<()>,
) {
    for tx in waiters.drain(..) {
        let _ = tx.send(make_err());
    }
}

/// Fold a [`NegotiatedChain`] result into driver state before `Resume`.
///
/// - A recovered failure (fallback chain installed) is surfaced through the
///   snapshot error field while playback still resumes.
/// - Pending acquire waiters are failed immediately with the rebuild error
///   instead of waiting for the `acquire_device` timeout.
/// - A fatal failure keeps playback suspended in `Error`; the returned flag
///   tells the RT thread to stay suspended instead of reporting `Playing`.
fn apply_negotiated_chain(
    negotiated: NegotiatedChain,
    shared: &SharedEngineState,
    pending_acquire_waiters: &mut Vec<mpsc::SyncSender<Result<()>>>,
) -> (Option<u64>, bool) {
    let generation = negotiated.generation;
    let Some(err) = negotiated.error else {
        // A chain valid for this generation was published: any fallback
        // warning from an older (or the same) build is stale now.
        shared.clear_chain_warning_through(generation);
        return (negotiated.required_chain_seq, false);
    };
    if negotiated.fatal {
        log::error!("DSP chain rebuild failed without a usable fallback: {err}");
        shared.set_error(err.clone());
    } else {
        log::warn!("DSP chain rebuild failed; playing fallback chain: {err}");
        shared.set_chain_warning(err.clone(), generation);
    }
    finish_waiters(pending_acquire_waiters, || Err(PwError::Init(err.clone())));
    (negotiated.required_chain_seq, negotiated.fatal)
}

#[allow(clippy::too_many_arguments)]
fn complete_release_teardown<B: DeviceBackend, F>(
    backend: &mut B,
    lifecycle: &mut DeviceLifecycle<B>,
    shared: &SharedEngineState,
    old_alsa: Option<B::Output>,
    awaiting_release_teardown: &mut bool,
    released_externally: bool,
    pending_release_waiters: &mut Vec<mpsc::SyncSender<Result<()>>>,
    pending_acquire_waiters: &mut Vec<mpsc::SyncSender<Result<()>>>,
    disconnect_and_wait: F,
) -> bool
where
    F: FnOnce() -> Result<()>,
{
    // Playback has stopped; the card is still ours. Whatever the engine
    // must do to it (a safe hardware volume) happens before anyone else
    // can open it.
    if let Some(device) = lifecycle.active_device.clone() {
        run_release_hook(shared, ReleaseKind::HandOver, &device);
    }
    if let Some(out) = old_alsa {
        backend.close_output(out);
    }
    let teardown_res = disconnect_and_wait();
    lifecycle.finish_release_to_idle(backend, None);
    shared.format.store(0, Ordering::Release);
    shared.clear_policies();
    shared.active_sink.store(0, Ordering::Release);
    shared.alsa_delay_frames.store(0, Ordering::Relaxed);
    *awaiting_release_teardown = false;

    match teardown_res {
        Ok(()) => {
            shared.clear_error_if(RELEASE_TIMEOUT_MSG);
            shared.set_state(EngineState::Idle);
            log::info!("Engine released device to Idle");
            finish_waiters(pending_release_waiters, || Ok(()));
            !pending_acquire_waiters.is_empty() && !released_externally
        }
        Err(err) => {
            let msg = err.to_string();
            log::error!("Release teardown failed: {msg}");
            lifecycle.state = EngineState::Error;
            lifecycle.last_error = Some(msg.clone());
            shared.set_error(msg.clone());
            finish_waiters(pending_release_waiters, || Err(PwError::Init(msg.clone())));
            finish_waiters(pending_acquire_waiters, || Err(PwError::Init(msg.clone())));
            false
        }
    }
}

/// Reopen's and reacquire's open, for
/// [`crate::lifecycle::open_with_bypass_fallback`]: the lifecycle's
/// handoff, then the engine's negotiation.
/// What start-up found on the device ([`startup_open`]), or the stand-in
/// for a device that is not there ([`startup_missing`]).
struct StartupDevice {
    reservation: Option<(u32, DeviceReservation)>,
    caps: Option<DeviceCapabilities>,
    effective: RatePolicy,
    bypass_held: crate::lifecycle::BypassHeld,
    desired: DesiredDspConfig,
    rate: u32,
    quantum: u32,
    ratio: RateRatio,
    /// `None`: started without the device.
    alsa: Option<AlsaOutput>,
    /// The open's parameters, or the nominal ones the RT thread sizes its
    /// buffers from while there is no device.
    params: NegotiatedParams,
    key: Option<NegotiatedKey>,
    negotiated: Option<NegotiatedChain>,
}

/// Start-up without the device: the published rates are the configured
/// ones (nothing was probed), the graph starts at the default rate, and no
/// card is reserved. The RT thread runs the graph on its own clock until
/// the control thread opens the device.
fn startup_missing(
    config: &EngineDriverConfig,
    desired: DesiredDspConfig,
    shared: &SharedEngineState,
) -> Result<StartupDevice> {
    let effective = compute_effective_policy(config, desired, None).map_err(PwError::Init)?;
    let policies = crate::rate_caps::rate_policies(effective, None, &config.formats, desired);
    shared.set_device_caps(None);
    shared.set_policies(&policies);
    let rate = policies.published.default_rate.max(8_000);
    let quantum = scaled_quantum(config.quantum, rate);
    shared.device_missing.store(true, Ordering::Release);
    shared.set_error(waiting_text(&config.device));
    shared.set_state(EngineState::Error);
    Ok(StartupDevice {
        reservation: None,
        caps: None,
        effective: policies.published,
        bypass_held: crate::lifecycle::BypassHeld::NONE,
        desired,
        rate,
        quantum,
        ratio: RateRatio::ONE,
        alsa: None,
        params: NegotiatedParams {
            format: config.formats.first().copied().unwrap_or(SampleFormat::S32),
            rate,
            channels: config.channels.max(1),
            period_frames: quantum,
            buffer_frames: quantum * config.buffer_periods.max(2),
        },
        key: None,
        negotiated: None,
    })
}

/// Start-up's open: reserve the card, open and probe the PCM, pick the
/// graph rate and ratio from what it plays, and negotiate the chain.
fn startup_open(
    config: &EngineDriverConfig,
    init_desired: DesiredDspConfig,
    shared: &Arc<SharedEngineState>,
    ctrl_efd: &Arc<EventFd>,
    on_negotiated: Option<&RatioNegotiator>,
) -> Result<StartupDevice> {
    let channels = config.channels.max(1);
    let reservation = if config.reserve_device {
        let efd_cb = Arc::clone(ctrl_efd);
        let flag_cb = Arc::clone(shared);
        let card = card_index_from_device(&config.device)?;
        let res = DeviceReservation::acquire_for_device_with_wakeup(
            &config.device,
            config.reserve_priority,
            Arc::new(move || {
                flag_cb.release_requested.store(true, Ordering::Release);
                efd_cb.signal();
            }),
        )?;
        Some((card, res))
    } else {
        None
    };

    // Open the PCM, probe what it accepts on this very handle (no second
    // open, so a device we already reserved is never EBUSY), then pick
    // the graph rate and ratio from that.
    let open = match OpenPcm::open(&config.device, true) {
        Ok(o) => o,
        Err(e) => {
            drop(reservation);
            return Err(e.into());
        }
    };
    let caps = match open.probe(channels, &engine_probe_rates(config.max_dsp_ratio)) {
        Ok(c) => {
            log::info!("Device '{}' supports {}", config.device, c.summary());
            shared.note_probe();
            Some(c)
        }
        Err(e) => {
            log::warn!(
                "Probing '{}' failed ({e}); trying rates blind",
                config.device
            );
            None
        }
    };
    let effective = match compute_effective_policy(config, init_desired, caps.as_ref()) {
        Ok(p) => p,
        Err(msg) => {
            drop(open);
            drop(reservation);
            return Err(PwError::Init(msg));
        }
    };
    let policies =
        crate::rate_caps::rate_policies(effective, caps.as_ref(), &config.formats, init_desired);
    shared.set_device_caps(caps.clone());
    shared.set_policies(&policies);

    // The open and its negotiation, falling back to PipeWire before
    // anything resumes when the engine bypass resampler's chain is not
    // there (as reopen and reacquire do).
    let mut init_bypass_held = crate::lifecycle::BypassHeld::NONE;
    let mut opener = StartupOpen {
        pcm: Some(open),
        config,
        caps: caps.as_ref(),
        shared,
        on_negotiated,
        rate: policies.published.default_rate.max(8_000),
        policies,
        channels,
        error: None,
        negotiated: false,
    };
    let base = |keep| config.desired_dsp(0).keep_bypass(keep);
    let mut init_desired = init_desired;
    let opened = crate::lifecycle::open_and_keep(
        &mut opener,
        &mut init_desired,
        &mut init_bypass_held,
        base,
    );
    let StartupOpen {
        policies,
        error,
        negotiated: negotiated_once,
        ..
    } = opener;
    let (opened, (init_negotiated, init_key)) = match opened {
        Ok(v) => v,
        Err(msg) => {
            if negotiated_once {
                run_release_hook(shared, ReleaseKind::HandOver, &config.device);
            }
            drop(reservation);
            return Err(error.unwrap_or(PwError::Init(msg)));
        }
    };
    let params = opened.alsa.params();
    Ok(StartupDevice {
        reservation,
        caps,
        effective: policies.published,
        bypass_held: init_bypass_held,
        desired: init_desired,
        rate: opened.rate,
        quantum: opened.quantum,
        ratio: opened.plan.ratio,
        alsa: Some(opened.alsa),
        params,
        key: Some(init_key),
        negotiated: Some(init_negotiated),
    })
}

struct DeviceOpen<'a> {
    lifecycle: &'a mut DeviceLifecycle<RealDeviceBackend>,
    backend: &'a mut RealDeviceBackend,
    /// The output the RT thread handed back (reopen), closed by the first
    /// open.
    old: Option<Box<AlsaOutput>>,
    config: &'a EngineDriverConfig,
    shared: &'a Arc<SharedEngineState>,
    pw_tx: &'a pw::channel::Sender<PwMsg>,
    published: &'a mut SentRatePolicies,
    on_negotiated: Option<&'a RatioNegotiator>,
    graph_rate: u32,
    quantum: u32,
    sink: SinkChoice,
}

impl crate::lifecycle::BypassOpen for DeviceOpen<'_> {
    type Output = crate::lifecycle::OpenedDevice<Box<AlsaOutput>>;
    type Negotiated = (NegotiatedChain, NegotiatedKey);

    fn open(
        &mut self,
        previous: Option<Self::Output>,
        desired: DesiredDspConfig,
    ) -> std::result::Result<Self::Output, String> {
        let old = previous.map(|p| p.output).or_else(|| self.old.take());
        let shared = self.shared;
        self.lifecycle.handoff_and_open(
            self.backend,
            old,
            &self.config.device,
            self.config.reserve_device,
            self.config.reserve_priority,
            self.graph_rate,
            self.quantum,
            self.sink,
            desired,
            &|kind, device| run_release_hook(shared, kind, device),
        )
    }

    fn negotiate(
        &mut self,
        out: &Self::Output,
        desired: DesiredDspConfig,
    ) -> (Self::Negotiated, Option<crate::lifecycle::BypassMiss>) {
        let params = out.output.params();
        let key = negotiated_key(
            desired,
            self.sink,
            out.graph_rate,
            self.backend.device_rates(),
            &out.plan,
        )
        .with_session(out.session);
        // Rebuild the chain for (graph rate, ratio) off-RT before Resume.
        let negotiated = self
            .on_negotiated
            .map(|cb| {
                cb(
                    &self.config.device,
                    out.graph_rate,
                    out.actual_ratio,
                    self.sink,
                    params.format,
                    &key,
                )
            })
            .unwrap_or_default();
        let miss = negotiated.bypass_miss(out.graph_rate, params.rate);
        if let Some(m) = miss {
            log::info!(
                "{} Hz to {} Hz: the engine's resampler {}; PipeWire converts instead",
                m.input,
                m.output,
                if m.pending {
                    "is still being designed"
                } else {
                    "cannot be built"
                }
            );
        }
        ((negotiated, key), miss)
    }

    fn republish(&mut self, desired: DesiredDspConfig) {
        if let Some(Ok(eff)) = self.backend.refresh_effective(desired) {
            self.published
                .note(eff.clone(), self.shared.sink_policies());
            let _ = self.pw_tx.send(PwMsg::ApplyRatePolicy {
                policy: eff,
                quantum_48k: self.config.quantum,
                last_rate: self.graph_rate,
            });
        }
    }
}

/// Start-up's open, for [`crate::lifecycle::open_with_bypass_fallback`]:
/// the probed PCM handle first (a fallback opens a fresh one; the card
/// stays reserved), at the DSP sink's graph rate.
struct StartupOpen<'a> {
    pcm: Option<OpenPcm>,
    config: &'a EngineDriverConfig,
    caps: Option<&'a DeviceCapabilities>,
    shared: &'a Arc<SharedEngineState>,
    on_negotiated: Option<&'a RatioNegotiator>,
    policies: crate::rate_caps::RatePolicies,
    rate: u32,
    channels: u32,
    /// The open's own error, returned as is.
    error: Option<PwError>,
    /// The engine heard of an open (it may have written the mixer): a
    /// failure after it hands the card over.
    negotiated: bool,
}

struct StartupOpened {
    alsa: AlsaOutput,
    plan: audivane_io_alsa::RatioOpen,
    rate: u32,
    quantum: u32,
}

impl StartupOpen<'_> {
    fn fail(&mut self, e: PwError) -> String {
        let msg = e.to_string();
        self.error = Some(e);
        msg
    }
}

impl crate::lifecycle::BypassOpen for StartupOpen<'_> {
    type Output = StartupOpened;
    type Negotiated = (NegotiatedChain, NegotiatedKey);

    fn open(
        &mut self,
        previous: Option<Self::Output>,
        desired: DesiredDspConfig,
    ) -> std::result::Result<Self::Output, String> {
        drop(previous);
        let pcm = match self.pcm.take() {
            Some(p) => p,
            None => OpenPcm::open(&self.config.device, true).map_err(|e| self.fail(e.into()))?,
        };
        // A fallback moves the graph to a native rate.
        if let crate::rate_caps::GraphRate::Move(r) =
            crate::rate_caps::plan_graph_rate(&self.policies, self.rate, SinkChoice::Dsp)
        {
            self.rate = r;
        }
        let rate = self.rate;
        let quantum = scaled_quantum(self.config.quantum, rate);
        let ratio = desired.resolve_ratio_for(SinkChoice::Dsp, rate);
        let ladder =
            desired.ratio_ladder_for(SinkChoice::Dsp, rate, self.caps, &self.config.formats);
        let alsa_cfg = AlsaConfig {
            device: self.config.device.clone(),
            rate: ratio.output_rate(rate).unwrap_or(rate),
            channels: self.channels,
            period_frames: ratio.scale_to_output(quantum),
            buffer_periods: self.config.buffer_periods.max(2),
            preferred_format: self.config.formats.first().copied(),
            strict_format: false,
            nonblock: true,
        };
        let (mut alsa, plan) = pcm
            .configure_with_ratio_plan(
                &alsa_cfg,
                rate,
                &ladder,
                quantum,
                &self.config.formats,
                self.caps,
            )
            .map_err(|e| self.fail(e.into()))?;
        alsa.prepare().map_err(|e| self.fail(e.into()))?;
        Ok(StartupOpened {
            alsa,
            plan,
            rate,
            quantum,
        })
    }

    fn negotiate(
        &mut self,
        out: &Self::Output,
        desired: DesiredDspConfig,
    ) -> (Self::Negotiated, Option<crate::lifecycle::BypassMiss>) {
        let params = out.alsa.params();
        let key = negotiated_key(
            desired,
            SinkChoice::Dsp,
            out.rate,
            DeviceRates::playable(self.caps, &self.config.formats),
            &out.plan,
        );
        self.negotiated |= self.on_negotiated.is_some();
        let negotiated = self
            .on_negotiated
            .map(|cb| {
                cb(
                    &self.config.device,
                    out.rate,
                    out.plan.ratio,
                    SinkChoice::Dsp,
                    params.format,
                    &key,
                )
            })
            .unwrap_or_default();
        let miss = negotiated.bypass_miss(out.rate, params.rate);
        ((negotiated, key), miss)
    }

    fn republish(&mut self, desired: DesiredDspConfig) {
        if let Ok(eff) = compute_effective_policy(self.config, desired, self.caps) {
            self.policies =
                crate::rate_caps::rate_policies(eff, self.caps, &self.config.formats, desired);
            self.shared.set_policies(&self.policies);
        }
    }
}

/// What the PipeWire thread last heard of the rate policies
/// ([`PwMsg::ApplyRatePolicy`]).
#[derive(Clone, Debug, PartialEq)]
struct SentRatePolicies {
    published: RatePolicy,
    sinks: Option<crate::rate_caps::RatePolicies>,
}

impl SentRatePolicies {
    /// Whether the policies must be sent again: the published rates
    /// changed, or either sink's own policy did. The rate arbiter maps each
    /// sink's clients through that sink's policy, so a change there must
    /// reach it even when the published union stays the same (turning DSP
    /// off at 44.1 kHz on a 48 kHz-only DAC adds 44.1 kHz to the DSP sink's
    /// rates, which Direct had already published).
    fn due(&self, published: &RatePolicy, sinks: &Option<crate::rate_caps::RatePolicies>) -> bool {
        self.published != *published || self.sinks != *sinks
    }

    fn note(&mut self, published: RatePolicy, sinks: Option<crate::rate_caps::RatePolicies>) {
        self.published = published;
        self.sinks = sinks;
    }
}

#[allow(clippy::too_many_arguments)]
fn run_control_thread(
    mut config: EngineDriverConfig,
    init_desired: DesiredDspConfig,
    init_bypass_held: crate::lifecycle::BypassHeld,
    channels: usize,
    init_rate: u32,
    init_actual_ratio: RateRatio,
    init_caps: Option<DeviceCapabilities>,
    init_effective: RatePolicy,
    init_reservation: Option<(u32, DeviceReservation)>,
    init_missing: bool,
    shared: Arc<SharedEngineState>,
    cmd_efd: Arc<EventFd>,
    ctrl_efd: Arc<EventFd>,
    ctrl_req_rx: mpsc::Receiver<CtrlReq>,
    mut ctrl_to_rt_tx: rtrb::Producer<RtControlCmd>,
    mut rt_to_ctrl_rx: rtrb::Consumer<RtEvent>,
    pw_tx: pw::channel::Sender<PwMsg>,
    on_negotiated: Option<RatioNegotiator>,
) {
    let mut backend = RealDeviceBackend {
        formats: config.formats.clone(),
        channels: channels as u32,
        buffer_periods: config.buffer_periods.max(2),
        ctrl_efd: Arc::clone(&ctrl_efd),
        shared: Arc::clone(&shared),
        config: config.clone(),
        probed_device: (!init_missing).then(|| config.device.clone()),
        caps: init_caps,
        probe_handle: None,
        effective: Some(init_effective.clone()),
        moves: None,
    };
    // Rate policy last sent to the PipeWire thread.
    let mut published_policy = SentRatePolicies {
        published: init_effective,
        sinks: shared.sink_policies(),
    };
    let mut logged_xruns = 0u64;
    let mut repromote = crate::rt_overload::RepromoteGate::default();
    let mut logged_alsa_lib = alsa_lib_errors();
    let mut lifecycle = DeviceLifecycle::<RealDeviceBackend> {
        state: if init_missing {
            EngineState::Error
        } else {
            EngineState::Ready
        },
        last_error: None,
        reservation: init_reservation,
        active_device: (!init_missing).then(|| config.device.clone()),
        active_graph_rate: init_rate,
        active_ratio: init_actual_ratio,
        active_sink: SinkChoice::Dsp,
        output_held: !init_missing,
    };
    // The device is gone (or keeps failing after it went): look for it and
    // have the RT thread open it again. The graph meanwhile runs on the RT
    // thread's own clock (`device_missing`).
    let mut lost: Option<LostDevice> = init_missing.then(|| LostDevice::new(false, Instant::now()));
    let mut presence = PresenceWatch::new(Instant::now());

    // What start-up opened with (its fallback may hold a pair back).
    let mut desired_dsp = init_desired;
    // The engine bypass resampler's pairs held back on this device (a
    // chain that failed, until the device changes; a design still running,
    // until it is done): they fall back to PipeWire.
    let mut bypass_held = init_bypass_held;
    let mut last_graph_rate = init_rate;
    let mut last_ratio = init_actual_ratio;
    let mut last_quantum = scaled_quantum(config.quantum, init_rate);
    let base_in = (config.buffer_periods.max(2) * last_quantum).max(4096) as usize * channels;
    let mut out_cap = init_actual_ratio.max_frames_out(base_in);
    // The input buffers always hold a varispeed block's largest pull.
    let mut in_cap = base_in.max(VS_MAX_SRC_FRAMES * channels);
    let mut released_externally = false;
    let mut awaiting_release_teardown = false;
    let mut reopen_gate = ReopenGate::new();
    let mut pending_rt_cmds: VecDeque<RtControlCmd> = VecDeque::new();
    let mut pending_release_waiters: Vec<mpsc::SyncSender<Result<()>>> = Vec::new();
    let mut pending_acquire_waiters: Vec<mpsc::SyncSender<Result<()>>> = Vec::new();

    // An acquire whose open the engine refused as superseded (its config
    // moved to another device meanwhile): redone with the queued config.
    let mut reacquire_pending = false;

    // Returns whether the engine refused the open as superseded.
    #[allow(clippy::too_many_arguments)]
    let perform_acquire = |config: &EngineDriverConfig,
                           desired_dsp: &mut DesiredDspConfig,
                           last_graph_rate_io: &mut u32,
                           published_policy: &mut SentRatePolicies,
                           bypass_held: &mut crate::lifecycle::BypassHeld,
                           last_ratio: &mut RateRatio,
                           last_quantum: &mut u32,
                           in_cap: &mut usize,
                           out_cap: &mut usize,
                           backend: &mut RealDeviceBackend,
                           lifecycle: &mut DeviceLifecycle<RealDeviceBackend>,
                           pending_rt_cmds: &mut VecDeque<RtControlCmd>,
                           ctrl_to_rt_tx: &mut rtrb::Producer<RtControlCmd>,
                           pending_acquire_waiters: &mut Vec<mpsc::SyncSender<Result<()>>>|
     -> bool {
        shared.clear_error();
        shared.clear_chain_warning();
        shared.release_requested.store(false, Ordering::Release);
        shared.set_state(EngineState::Switching);

        let active_sink =
            sink_from_u8(shared.active_sink.load(Ordering::Acquire)).unwrap_or(SinkChoice::Dsp);
        let q = scaled_quantum(config.quantum, *last_graph_rate_io);
        let mut opener = DeviceOpen {
            lifecycle,
            backend,
            old: None,
            config,
            shared: &shared,
            pw_tx: &pw_tx,
            published: published_policy,
            on_negotiated: on_negotiated.as_ref(),
            graph_rate: *last_graph_rate_io,
            quantum: q,
            sink: active_sink,
        };
        // The control loop keeps the config the open ran with (a
        // fallback's, with the pair held back) before anything resumes.
        let base = |keep| config.desired_dsp(0).keep_bypass(keep);
        let (opened, negotiated, negotiated_key) =
            match crate::lifecycle::open_and_keep(&mut opener, desired_dsp, bypass_held, base) {
                Ok((o, (n, k))) => (o, n, k),
                Err(msg) => {
                    run_release_hook(&shared, ReleaseKind::Lost, &config.device);
                    backend.reprobe_after_failure(&config.device);
                    shared.clear_negotiation(&config.device);
                    shared.set_error(msg.clone());
                    finish_waiters(pending_acquire_waiters, || Err(PwError::Init(msg.clone())));
                    return false;
                }
            };

        let alsa = opened.output;
        let actual_ratio = opened.actual_ratio;
        let last_graph_rate = opened.graph_rate;
        let q = opened.quantum;
        *last_graph_rate_io = last_graph_rate;
        let params = alsa.params();
        let fds = alsa.poll_descriptors().unwrap_or_default();
        *last_ratio = actual_ratio;
        *last_quantum = q;
        shared
            .format
            .store(format_to_u8(Some(params.format)), Ordering::Release);
        shared.graph_rate.store(last_graph_rate, Ordering::Release);
        shared.output_rate.store(params.rate, Ordering::Release);
        shared
            .ratio
            .store(actual_ratio.to_bits(), Ordering::Release);
        shared.quantum.store(
            actual_ratio.scale_to_input(params.period_frames).max(32),
            Ordering::Release,
        );

        if negotiated.superseded {
            // The engine moved to another device while this one opened;
            // its config change is queued: acquire again with it.
            log::info!(
                "acquire of '{}' superseded by a device change; acquiring again",
                config.device
            );
            run_release_hook(&shared, ReleaseKind::HandOver, &config.device);
            backend.close_output(alsa);
            lifecycle.output_held = false;
            lifecycle.release_reservation(backend);
            lifecycle.active_device = None;
            lifecycle.state = EngineState::Idle;
            shared.set_state(EngineState::Idle);
            return true;
        }
        let (required_chain_seq, hold_error) =
            apply_negotiated_chain(negotiated, &shared, pending_acquire_waiters);
        let needed_in =
            (actual_ratio.scale_to_input(params.buffer_frames) + 2).max(4096) as usize * channels;
        let (dsp_in_buf, direct_in_buf, out_buf) =
            maybe_grow_buffers(needed_in, actual_ratio, in_cap, out_cap);

        let ring_samples = (RING_ALLOC_FRAMES * channels).max(32 * 1024);
        let (dsp_prod, dsp_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
        let (direct_prod, direct_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
        let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<SendRawStream>>(1);
        // The new session publishes the device-restricted rates.
        let mut pw_config = config.clone();
        if let Some(eff) = backend.effective.clone() {
            pw_config.rate_policy = eff;
        }
        published_policy.note(pw_config.rate_policy.clone(), shared.sink_policies());
        let _ = pw_tx.send(PwMsg::Reconnect {
            config: Box::new(pw_config),
            last_rate: last_graph_rate,
            dsp_prod,
            direct_prod,
            reply_tx,
        });
        let new_raw = match reply_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => {
                // Negotiation already told the engine it holds the DAC.
                run_release_hook(&shared, ReleaseKind::HandOver, &config.device);
                backend.close_output(alsa);
                lifecycle.output_held = false;
                lifecycle.release_reservation(backend);
                lifecycle.active_device = None;
                lifecycle.state = EngineState::Error;
                let msg = format!("PipeWire reconnect failed: {e}");
                shared.set_error(msg.clone());
                finish_waiters(pending_acquire_waiters, || Err(PwError::Init(msg.clone())));
                return false;
            }
            Err(e) => {
                run_release_hook(&shared, ReleaseKind::HandOver, &config.device);
                backend.close_output(alsa);
                lifecycle.output_held = false;
                lifecycle.release_reservation(backend);
                lifecycle.active_device = None;
                lifecycle.state = EngineState::Error;
                let msg = format!("PipeWire reconnect timed out: {e}");
                shared.set_error(msg.clone());
                finish_waiters(pending_acquire_waiters, || Err(PwError::Init(msg.clone())));
                return false;
            }
        };

        let fade_in_frames = ((params.rate as usize) * FADE_MS) / 1000;
        shared.set_open_key(negotiated_key);
        // Opened again: not waiting for it any more (the control loop ends
        // its `lost` watch on seeing this).
        shared.device_missing.store(false, Ordering::Release);
        shared.device_gone.store(false, Ordering::Release);
        shared.clear_error_if(&waiting_text(&config.device));
        push_or_queue_rt_cmd(
            pending_rt_cmds,
            ctrl_to_rt_tx,
            &cmd_efd,
            RtControlCmd::Resume {
                alsa,
                alsa_fds: fds,
                params,
                graph_rate: last_graph_rate,
                actual_ratio,
                target_sink: active_sink,
                desired_dsp: *desired_dsp,
                negotiated_key,
                required_chain_seq,
                hold_error,
                new_driver_stream: Some(new_raw),
                new_dsp_cons: Some(dsp_cons),
                new_direct_cons: Some(direct_cons),
                dsp_in_buf,
                direct_in_buf,
                out_buf,
                fade_in_frames,
            },
        );
        false
    };

    while !shared.stop.load(Ordering::Acquire) {
        flush_pending_rt_cmds(&mut pending_rt_cmds, &mut ctrl_to_rt_tx, &cmd_efd);

        // A reacquire opened the device again (`device_missing` cleared):
        // nothing to wait for.
        if lost.is_some() && !shared.device_missing.load(Ordering::Acquire) {
            lost = None;
        }
        // The device held open went away while nothing played.
        match lifecycle.active_device.as_deref() {
            Some(device)
                if lost.is_none()
                    && lifecycle.output_held
                    && !released_externally
                    && !awaiting_release_teardown =>
            {
                if let Some(gone) = presence.look(Instant::now(), || device_present(device))
                    && gone != shared.device_gone.load(Ordering::Acquire)
                {
                    if gone {
                        log::warn!("output device '{device}' disappeared");
                    }
                    shared.device_gone.store(gone, Ordering::Release);
                    cmd_efd.signal();
                }
            }
            _ => presence.reset(),
        }
        // A lost device: once it is there again (or a present one's retry
        // is due), the RT thread opens it through the usual reopen.
        if let Some(l) = lost.as_mut()
            && !released_externally
            && !awaiting_release_teardown
        {
            let now = Instant::now();
            if now >= l.next {
                let present = device_present(&config.device);
                if l.due(present, now) {
                    log::info!("output device '{}' is there; opening it", config.device);
                    shared.device_retry.fetch_add(1, Ordering::AcqRel);
                    cmd_efd.signal();
                }
            }
        }

        // 1. Check ReserveDevice1 external release request.
        let ext_rel = shared.release_requested.load(Ordering::Acquire)
            || lifecycle.reservation_release_requested(&backend);
        if ext_rel && !released_externally && !awaiting_release_teardown {
            released_externally = true;
            awaiting_release_teardown = true;
            reopen_gate.ctrl_clear_for_release();
            log::info!("ReserveDevice1 release requested: suspending DAC and idling");
            shared.set_state(EngineState::Switching);
            push_or_queue_rt_cmd(
                &mut pending_rt_cmds,
                &mut ctrl_to_rt_tx,
                &cmd_efd,
                RtControlCmd::SuspendForRelease,
            );
        }

        // 2. Drain requests from EngineDriverHandle (apply / release / acquire).
        while let Ok(req) = ctrl_req_rx.try_recv() {
            match req {
                CtrlReq::UpdateConfig {
                    config: new_cfg,
                    force_reopen,
                } => {
                    let rate_policy_changed = config.rate_policy != new_cfg.rate_policy
                        || config.quantum != new_cfg.quantum;
                    if config.device != new_cfg.device {
                        bypass_held = crate::lifecycle::BypassHeld::NONE;
                    }
                    config = *new_cfg;
                    // Only an edit that steers negotiation (or reopens
                    // anyway) starts a new retry generation.
                    desired_dsp = desired_dsp.next(
                        config.desired_dsp(0).keep_bypass(bypass_held.keep()),
                        force_reopen,
                    );
                    let desired_gen = desired_dsp.generation;
                    backend.formats = config.formats.clone();
                    backend.channels = config.channels.max(1);
                    backend.buffer_periods = config.buffer_periods.max(2);
                    backend.config = config.clone();
                    // Same device: restrict the new policy with the cached
                    // probe right away. A new device is probed on reopen.
                    let effective = match backend.refresh_effective(desired_dsp) {
                        Some(Ok(p)) => {
                            shared.set_effective_policy(Some(p.clone()));
                            Some(p)
                        }
                        Some(Err(msg)) => {
                            log::error!("{msg}");
                            shared.clear_policies();
                            shared.set_error(msg);
                            None
                        }
                        None => None,
                    };

                    log::debug!(
                        "ctrl UpdateConfig: desired_gen={desired_gen} force_reopen={force_reopen} target={:?} dsp_enabled={}",
                        desired_dsp.rate_target,
                        desired_dsp.dsp_enabled
                    );

                    push_or_queue_rt_cmd(
                        &mut pending_rt_cmds,
                        &mut ctrl_to_rt_tx,
                        &cmd_efd,
                        RtControlCmd::UpdateDesiredDsp(desired_dsp),
                    );

                    if let Some(eff) = effective
                        && (rate_policy_changed
                            || published_policy.due(&eff, &shared.sink_policies()))
                    {
                        published_policy.note(eff.clone(), shared.sink_policies());
                        let _ = pw_tx.send(PwMsg::ApplyRatePolicy {
                            policy: eff,
                            quantum_48k: config.quantum,
                            last_rate: last_graph_rate,
                        });
                    }
                    if std::mem::take(&mut reacquire_pending) {
                        // The device change a superseded acquire waited for:
                        // the new device is opened from scratch, no reopen.
                        reacquire_pending = perform_acquire(
                            &config,
                            &mut desired_dsp,
                            &mut last_graph_rate,
                            &mut published_policy,
                            &mut bypass_held,
                            &mut last_ratio,
                            &mut last_quantum,
                            &mut in_cap,
                            &mut out_cap,
                            &mut backend,
                            &mut lifecycle,
                            &mut pending_rt_cmds,
                            &mut ctrl_to_rt_tx,
                            &mut pending_acquire_waiters,
                        );
                    } else if reopen_gate.ctrl_request_reopen(
                        force_reopen,
                        released_externally || awaiting_release_teardown,
                        shared.state(),
                    ) {
                        shared.set_state(EngineState::Switching);
                        push_or_queue_rt_cmd(
                            &mut pending_rt_cmds,
                            &mut ctrl_to_rt_tx,
                            &cmd_efd,
                            RtControlCmd::SuspendForReopen,
                        );
                    }
                }
                CtrlReq::Bypass(note) => {
                    // A design that ran while its pair fell back to PipeWire
                    // is done: the pair comes back (or stays out). Or an
                    // edit made the designs obsolete: held pairs are
                    // released (their new design is asked for when a client
                    // moves the graph there again). Either way the arbiter
                    // hears of it.
                    if crate::lifecycle::bypass_note(
                        &mut desired_dsp,
                        &mut bypass_held,
                        note,
                        |keep| config.desired_dsp(0).keep_bypass(keep),
                    ) {
                        push_or_queue_rt_cmd(
                            &mut pending_rt_cmds,
                            &mut ctrl_to_rt_tx,
                            &cmd_efd,
                            RtControlCmd::UpdateDesiredDsp(desired_dsp),
                        );
                        if let Some(Ok(eff)) = backend.refresh_effective(desired_dsp) {
                            shared.set_effective_policy(Some(eff.clone()));
                            if published_policy.due(&eff, &shared.sink_policies()) {
                                published_policy.note(eff.clone(), shared.sink_policies());
                                let _ = pw_tx.send(PwMsg::ApplyRatePolicy {
                                    policy: eff,
                                    quantum_48k: config.quantum,
                                    last_rate: last_graph_rate,
                                });
                            }
                        }
                    }
                }
                CtrlReq::ReleaseDevice { reply_tx } => {
                    if std::mem::take(&mut reacquire_pending) {
                        finish_waiters(&mut pending_acquire_waiters, || {
                            Err(PwError::Init("released during acquire".into()))
                        });
                    }
                    if shared.state() == EngineState::Idle
                        && !awaiting_release_teardown
                        && lifecycle.reservation.is_none()
                        && !lifecycle.output_held
                    {
                        let _ = reply_tx.send(Ok(()));
                    } else {
                        pending_release_waiters.push(reply_tx);
                        if !awaiting_release_teardown {
                            released_externally = true;
                            awaiting_release_teardown = true;
                            reopen_gate.ctrl_clear_for_release();
                            shared.set_state(EngineState::Switching);
                            push_or_queue_rt_cmd(
                                &mut pending_rt_cmds,
                                &mut ctrl_to_rt_tx,
                                &cmd_efd,
                                RtControlCmd::SuspendForRelease,
                            );
                        }
                    }
                }
                CtrlReq::AcquireDevice { reply_tx } => {
                    if reacquire_pending {
                        // Already acquiring (waiting for the device change).
                        pending_acquire_waiters.push(reply_tx);
                    } else if awaiting_release_teardown {
                        // Release teardown is still in flight; queue acquire until
                        // `ReleasedIdle` finishes full teardown.
                        released_externally = false;
                        pending_acquire_waiters.push(reply_tx);
                    } else if matches!(shared.state(), EngineState::Idle | EngineState::Error) {
                        released_externally = false;
                        pending_acquire_waiters.push(reply_tx);
                        reacquire_pending = perform_acquire(
                            &config,
                            &mut desired_dsp,
                            &mut last_graph_rate,
                            &mut published_policy,
                            &mut bypass_held,
                            &mut last_ratio,
                            &mut last_quantum,
                            &mut in_cap,
                            &mut out_cap,
                            &mut backend,
                            &mut lifecycle,
                            &mut pending_rt_cmds,
                            &mut ctrl_to_rt_tx,
                            &mut pending_acquire_waiters,
                        );
                    } else {
                        let _ = reply_tx.send(Ok(()));
                    }
                }
            }
        }

        // 3. Process RT events (NeedReopen / ReleasedIdle / Resumed / RecycleBuffers / FatalError).
        while let Ok(ev) = rt_to_ctrl_rx.pop() {
            match ev {
                RtEvent::NeedReopen {
                    target_rate,
                    target_quantum,
                    target_sink,
                    old_alsa,
                    old_fds,
                    device_failed,
                } => {
                    drop(old_fds);
                    if shared.stop.load(Ordering::Acquire)
                        || released_externally
                        || awaiting_release_teardown
                    {
                        if let Some(out) = old_alsa {
                            backend.close_output(out);
                        }
                        lifecycle.output_held = false;
                        continue;
                    }
                    if device_failed {
                        log::warn!(
                            "output device '{}' stopped working; opening it again",
                            config.device
                        );
                    }
                    let awaiting_cfg = reopen_gate.ctrl_take_awaiting_cfg();

                    let eff_rate = if awaiting_cfg && !config.rate_policy.follow_source {
                        backend
                            .effective
                            .as_ref()
                            .map_or(config.rate_policy.default_rate, |p| p.default_rate)
                            .max(8_000)
                    } else {
                        target_rate
                    };
                    let q = if awaiting_cfg || target_quantum < 32 {
                        scaled_quantum(config.quantum, eff_rate)
                    } else {
                        target_quantum
                    };

                    log::debug!(
                        "ctrl NeedReopen: awaiting_cfg={awaiting_cfg} eff_rate={eff_rate} q={q} sink={target_sink:?} desired_gen={}",
                        desired_dsp.generation
                    );

                    let mut opener = DeviceOpen {
                        lifecycle: &mut lifecycle,
                        backend: &mut backend,
                        old: old_alsa,
                        config: &config,
                        shared: &shared,
                        pw_tx: &pw_tx,
                        published: &mut published_policy,
                        on_negotiated: on_negotiated.as_ref(),
                        graph_rate: eff_rate,
                        quantum: q,
                        sink: target_sink,
                    };
                    let base = |keep| config.desired_dsp(0).keep_bypass(keep);
                    match crate::lifecycle::open_and_keep(
                        &mut opener,
                        &mut desired_dsp,
                        &mut bypass_held,
                        base,
                    ) {
                        Ok((opened, (negotiated, negotiated_key))) => {
                            // The backend may have moved the graph to a rate
                            // the (new) device can play.
                            let eff_rate = opened.graph_rate;
                            let q = opened.quantum;
                            let new_alsa = opened.output;
                            let actual_ratio = opened.actual_ratio;
                            let new_params = new_alsa.params();
                            let new_fds = new_alsa.poll_descriptors().unwrap_or_default();
                            last_graph_rate = eff_rate;
                            last_ratio = actual_ratio;
                            last_quantum = q;

                            shared
                                .format
                                .store(format_to_u8(Some(new_params.format)), Ordering::Release);
                            shared.graph_rate.store(eff_rate, Ordering::Release);
                            shared.output_rate.store(new_params.rate, Ordering::Release);
                            shared
                                .ratio
                                .store(actual_ratio.to_bits(), Ordering::Release);
                            shared.quantum.store(
                                actual_ratio
                                    .scale_to_input(new_params.period_frames)
                                    .max(32),
                                Ordering::Release,
                            );
                            shared.clear_error();
                            shared.clear_chain_warning();

                            // A new device (or a graph PipeWire left at a rate
                            // the device cannot play): publish the restricted
                            // rates and force the rate we opened at.
                            if let Some(eff) = backend.effective.clone()
                                && (published_policy.due(&eff, &shared.sink_policies())
                                    || eff_rate != target_rate)
                            {
                                published_policy.note(eff.clone(), shared.sink_policies());
                                let _ = pw_tx.send(PwMsg::ApplyRatePolicy {
                                    policy: eff,
                                    quantum_48k: config.quantum,
                                    last_rate: eff_rate,
                                });
                            }

                            if negotiated.superseded {
                                // The engine moved to another device while
                                // this one reopened: never resume it. The
                                // queued config change reopens on the new
                                // device (no mixer was written here).
                                log::info!(
                                    "reopen of '{}' superseded by a device change",
                                    config.device
                                );
                                drop(new_fds);
                                backend.close_output(new_alsa);
                                lifecycle.output_held = false;
                                push_or_queue_rt_cmd(
                                    &mut pending_rt_cmds,
                                    &mut ctrl_to_rt_tx,
                                    &cmd_efd,
                                    RtControlCmd::ReopenAborted,
                                );
                                continue;
                            }
                            let (required_chain_seq, hold_error) = apply_negotiated_chain(
                                negotiated,
                                &shared,
                                &mut pending_acquire_waiters,
                            );

                            let needed_in = (actual_ratio.scale_to_input(new_params.buffer_frames)
                                + 2)
                            .max(4096) as usize
                                * channels;
                            let (dsp_in_buf, direct_in_buf, out_buf) = maybe_grow_buffers(
                                needed_in,
                                actual_ratio,
                                &mut in_cap,
                                &mut out_cap,
                            );

                            let fade_in_frames = ((new_params.rate as usize) * FADE_MS) / 1000;
                            shared.set_open_key(negotiated_key);
                            if lost.is_some() {
                                log::info!(
                                    "output device '{}' is back; playing on it",
                                    config.device
                                );
                            }
                            end_waiting(&shared, &mut lost, &config.device);
                            presence.reset();
                            push_or_queue_rt_cmd(
                                &mut pending_rt_cmds,
                                &mut ctrl_to_rt_tx,
                                &cmd_efd,
                                RtControlCmd::Resume {
                                    alsa: new_alsa,
                                    alsa_fds: new_fds,
                                    params: new_params,
                                    graph_rate: eff_rate,
                                    actual_ratio,
                                    target_sink,
                                    desired_dsp,
                                    negotiated_key,
                                    required_chain_seq,
                                    hold_error,
                                    new_driver_stream: None,
                                    new_dsp_cons: None,
                                    new_direct_cons: None,
                                    dsp_in_buf,
                                    direct_in_buf,
                                    out_buf,
                                    fade_in_frames,
                                },
                            );
                        }
                        Err(msg) => {
                            run_release_hook(&shared, ReleaseKind::Lost, &config.device);
                            backend.reprobe_after_failure(&config.device);
                            shared.clear_negotiation(&config.device);
                            // A device that went (or is not there) is
                            // waited for, and one that failed and keeps
                            // failing is retried; the sinks stay up on the
                            // engine's own clock meanwhile.
                            let present = device_present(&config.device);
                            if device_failed || lost.is_some() || !present {
                                match lost.as_mut() {
                                    Some(l) => l.present = present,
                                    None => {
                                        lost = Some(LostDevice::new(present, Instant::now()));
                                    }
                                }
                                shared.device_missing.store(true, Ordering::Release);
                                if present {
                                    log::warn!(
                                        "Reopening '{}' failed: {msg}; retrying",
                                        config.device
                                    );
                                    shared.set_error(format!("{msg} (retrying)"));
                                } else {
                                    log::warn!(
                                        "output device '{}' is gone; waiting for it",
                                        config.device
                                    );
                                    shared.set_error(waiting_text(&config.device));
                                }
                            } else {
                                log::error!("Reopening '{}' failed: {msg}", config.device);
                                shared.set_error(msg);
                            }
                            push_or_queue_rt_cmd(
                                &mut pending_rt_cmds,
                                &mut ctrl_to_rt_tx,
                                &cmd_efd,
                                RtControlCmd::ReopenAborted,
                            );
                        }
                    }
                }
                RtEvent::ReleasedIdle { old_alsa, old_fds } => {
                    drop(old_fds);
                    // Released on request: no longer waiting for the device
                    // (taking it back opens it from scratch).
                    end_waiting(&shared, &mut lost, &config.device);
                    // Re-probe on the next acquire: the device may have changed.
                    backend.invalidate_caps();
                    let (ack_tx, ack_rx) = mpsc::sync_channel::<Result<()>>(1);
                    let should_acquire = complete_release_teardown(
                        &mut backend,
                        &mut lifecycle,
                        &shared,
                        old_alsa,
                        &mut awaiting_release_teardown,
                        released_externally,
                        &mut pending_release_waiters,
                        &mut pending_acquire_waiters,
                        || {
                            pw_tx
                                .send(PwMsg::DisconnectAndRestore { ack_tx })
                                .map_err(|_| {
                                    PwError::Init(
                                        "PipeWire loop channel closed during release teardown"
                                            .into(),
                                    )
                                })?;
                            match ack_rx.recv_timeout(Duration::from_secs(3)) {
                                Ok(res) => res,
                                Err(mpsc::RecvTimeoutError::Timeout) => Err(PwError::Init(
                                    "Timed out waiting for PipeWire release teardown acknowledgement"
                                        .into(),
                                )),
                                Err(mpsc::RecvTimeoutError::Disconnected) => Err(PwError::Init(
                                    "PipeWire loop disconnected before acknowledging release teardown"
                                        .into(),
                                )),
                            }
                        },
                    );

                    if should_acquire {
                        reacquire_pending = perform_acquire(
                            &config,
                            &mut desired_dsp,
                            &mut last_graph_rate,
                            &mut published_policy,
                            &mut bypass_held,
                            &mut last_ratio,
                            &mut last_quantum,
                            &mut in_cap,
                            &mut out_cap,
                            &mut backend,
                            &mut lifecycle,
                            &mut pending_rt_cmds,
                            &mut ctrl_to_rt_tx,
                            &mut pending_acquire_waiters,
                        );
                    }
                }
                RtEvent::Resumed => {
                    finish_waiters(&mut pending_acquire_waiters, || Ok(()));
                }
                RtEvent::RecycleBuffers {
                    old_fds,
                    old_dsp_cons,
                    old_direct_cons,
                    old_dsp_in,
                    old_direct_in,
                    old_out,
                } => {
                    drop(old_fds);
                    drop(old_dsp_cons);
                    drop(old_direct_cons);
                    drop(old_dsp_in);
                    drop(old_direct_in);
                    drop(old_out);
                }
                RtEvent::FatalError { msg } => {
                    log::error!("audio thread stopped on '{}': {msg}", config.device);
                    shared.set_error(msg);
                    finish_waiters(&mut pending_acquire_waiters, || {
                        Err(PwError::Init(msg.to_string()))
                    });
                    finish_waiters(&mut pending_release_waiters, || {
                        Err(PwError::Init(msg.to_string()))
                    });
                }
            }
        }

        let _ = (last_ratio, last_quantum);
        log_new_xruns(&shared, &mut logged_xruns);
        repromote.tick(
            &shared.rt,
            config.rt_priority,
            f64::from_bits(shared.dsp_load_bits.load(Ordering::Relaxed)),
            shared.xruns.load(Ordering::Relaxed),
        );
        log_new_alsa_lib_errors(alsa_lib_errors(), &mut logged_alsa_lib);

        let mut pfd = [libc::pollfd {
            fd: ctrl_efd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        unsafe {
            libc::poll(pfd.as_mut_ptr(), 1, 50);
        }
        ctrl_efd.drain();
    }

    finish_waiters(&mut pending_release_waiters, || {
        Err(PwError::Init("driver stopping".into()))
    });
    finish_waiters(&mut pending_acquire_waiters, || {
        Err(PwError::Init("driver stopping".into()))
    });
    pending_rt_cmds.clear();

    while let Ok(ev) = rt_to_ctrl_rx.pop() {
        match ev {
            RtEvent::NeedReopen { old_alsa, .. } | RtEvent::ReleasedIdle { old_alsa, .. } => {
                if let Some(out) = old_alsa {
                    backend.close_output(out);
                }
            }
            _ => {}
        }
    }
    release_on_stop(&mut backend, &mut lifecycle, &shared);
}

/// Stopping: playback has stopped (the output thread is joined) and the
/// reservation is still ours, so the engine gets the same hand-over as on a
/// release, before the reservation goes.
fn release_on_stop<B: DeviceBackend>(
    backend: &mut B,
    lifecycle: &mut DeviceLifecycle<B>,
    shared: &SharedEngineState,
) {
    if (lifecycle.reservation.is_some() || lifecycle.output_held)
        && let Some(device) = lifecycle.active_device.clone()
    {
        run_release_hook(shared, ReleaseKind::HandOver, &device);
    }
    lifecycle.release_reservation(backend);
}

type GrowthBuffers = (Option<Vec<f32>>, Option<Vec<f32>>, Option<Vec<i32>>);

fn maybe_grow_buffers(
    needed_in: usize,
    actual_ratio: RateRatio,
    in_cap: &mut usize,
    out_cap: &mut usize,
) -> GrowthBuffers {
    // `needed_in` counts samples of whole frames; one extra output frame per
    // channel covers the rounding of a rational ratio.
    let needed_out = actual_ratio.max_frames_out(needed_in) + 64;
    let (dsp_in, direct_in) = if needed_in > *in_cap {
        *in_cap = needed_in;
        (Some(vec![0.0f32; *in_cap]), Some(vec![0.0f32; *in_cap]))
    } else {
        (None, None)
    };
    let out = if needed_out > *out_cap {
        *out_cap = needed_out;
        Some(vec![0i32; *out_cap])
    } else {
        None
    };
    (dsp_in, direct_in, out)
}

// ---------------------------------------------------------------- ALSA RT thread

#[allow(clippy::too_many_arguments)]
fn try_complete_resume_gate<P: EngineAudioProcessor>(
    processor: &mut P,
    shared: &SharedEngineState,
    awaiting_chain_for_resume: &mut Option<Option<u64>>,
    suspended: &mut bool,
    resume_ack: &mut bool,
    active_sink: SinkChoice,
    active_ratio: RateRatio,
    event_stash: &mut BoundedEventStash<RtEvent, 16>,
    rt_to_ctrl_tx: &mut rtrb::Producer<RtEvent>,
    ctrl_efd: &EventFd,
) -> bool {
    let Some(req_seq) = *awaiting_chain_for_resume else {
        return true;
    };
    if !processor.is_chain_ready(active_sink, active_ratio, req_seq) {
        *suspended = true;
        shared.set_state(EngineState::Switching);
        return false;
    }
    *awaiting_chain_for_resume = None;
    *suspended = false;
    let streaming_now = shared.dsp_streaming.load(Ordering::Acquire)
        || shared.direct_streaming.load(Ordering::Acquire);
    if streaming_now {
        // The live graph check, the prefill and the start come first: the
        // device plays (and the resume is acknowledged) once it started
        // (`on_device_started`).
        shared.set_state(EngineState::Switching);
        *resume_ack = true;
    } else {
        shared.set_state(EngineState::Ready);
        let _ = event_stash.push_or_stash(RtEvent::Resumed, rt_to_ctrl_tx, ctrl_efd);
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn run_alsa_thread<P: EngineAudioProcessor>(
    config: EngineDriverConfig,
    init_rate: u32,
    init_ratio: RateRatio,
    init_desired: DesiredDspConfig,
    init_key: Option<NegotiatedKey>,
    init_required_seq: Option<u64>,
    init_hold_error: bool,
    alsa_init: Option<Box<AlsaOutput>>,
    init_params: NegotiatedParams,
    mut alsa_fds: Vec<libc::pollfd>,
    mut raw_stream: SendRawStream,
    shared: Arc<SharedEngineState>,
    cmd_efd: Arc<EventFd>,
    data_efd: Arc<EventFd>,
    ctrl_efd: Arc<EventFd>,
    mut dsp_cons: rtrb::Consumer<f32>,
    mut direct_cons: rtrb::Consumer<f32>,
    mut ctrl_to_rt_rx: rtrb::Consumer<RtControlCmd>,
    mut rt_to_ctrl_tx: rtrb::Producer<RtEvent>,
    mut processor: P,
    test_stall: Option<TestStallConfig>,
) {
    let started_missing = alsa_init.is_none();
    let mut alsa_opt: Option<Box<AlsaOutput>> = alsa_init;
    // The xruns of the open handle already counted into `xrun_events`.
    let mut handle_xruns = 0u64;
    let mut params = init_params;
    let channels = params.channels as usize;
    let mut active_graph_rate = init_rate;
    // Whether this open's graph was seen at `active_graph_rate` yet.
    let mut landed = false;
    let mut active_ratio = init_ratio;
    let mut active_sink = SinkChoice::Dsp;
    let mut rt_dsp_cfg = init_desired;
    let mut last_negotiated_key: Option<NegotiatedKey> = init_key;
    let mut sessions = PlaybackSessions::default();

    let max_in_frames = (active_ratio.scale_to_input(params.buffer_frames) + 2).max(4096) as usize;
    // The input buffers always hold a varispeed block's largest pull.
    let in_frames_cap = max_in_frames.max(VS_MAX_SRC_FRAMES);
    let mut dsp_in_buf = vec![0.0f32; in_frames_cap * channels];
    let mut direct_in_buf = vec![0.0f32; in_frames_cap * channels];
    let mut out_buf = vec![0i32; active_ratio.max_frames_out(max_in_frames) * channels];
    let mut planner = PeriodPlanner::default();
    // Sized for every reopen: the channel count is the thread's, and no
    // ratio carries more than `CARRY_MAX_FRAMES`.
    let mut carry = OutCarry::with_capacity(CARRY_MAX_FRAMES * channels);
    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(16);
    let mut event_stash: BoundedEventStash<RtEvent, 16> = BoundedEventStash::new();
    // alsa-lib's own messages (a failed recovery `prepare`) go to a
    // counter the control thread logs, not to stderr from this thread.
    silence_alsa_lib_on_this_thread();
    // After the allocations above.
    let mut rt_guard = crate::rt_overload::RtLoopGuard::start(&shared.rt, config.rt_priority);

    let mut clock_state = DriverClockState::new();
    let mut fade_ramp = FadeRamp::unity();
    let mut alsa_running = false;
    let mut suspended = false;
    let mut awaiting_chain_for_resume: Option<Option<u64>> = None;
    // A resume is acknowledged (`RtEvent::Resumed`) once the device started.
    let mut resume_ack = false;
    let mut released_idle = false;
    let mut reopen_gate = ReopenGate::new();
    let mut pending_reopen: Option<(u32, u32, SinkChoice)> = None;
    // (graph rate, sink) of the reopen in flight, and of the last one the
    // control thread aborted (open failed): a later route or rate change
    // retries from there instead of staying in `Error`.
    let mut inflight_reopen: Option<(u32, SinkChoice)> = None;
    // Started without the device: as after a failed open, the control
    // thread's retry (`device_retry`) opens it.
    let mut aborted_reopen: Option<(u32, SinkChoice)> =
        started_missing.then_some((init_rate, SinkChoice::Dsp));
    let mut seen_retry = shared.device_retry.load(Ordering::Acquire);
    let mut pacer = CyclePacer::default();
    // Quantum, rate and DAC delay of the last written period, for cycles the
    // pacer fires between DAC periods.
    let mut pace_q: u64 = 0;
    let mut pace_rate: u32 = 0;
    let mut pace_delay_graph: i64 = 0;
    let mut first_prime_after_reopen = false;
    let mut dsp_load_ewma = 0.0_f64;
    let mut stalled_once = false;
    // Varispeed: the watermark schedule while a stage runs, when to poll it
    // next, and the clock delay for cycles between DAC wakeups.
    let mut vs = VarispeedDriver::new();
    let mut vs_wake: Option<u64> = None;
    let mut vs_delay = VsDelay::default();

    processor.poll_updates(active_sink, active_graph_rate, active_ratio, false);
    if init_hold_error {
        // No chain valid for the initial ratio could be built: start
        // suspended in `Error` (a later config apply triggers a fresh reopen).
        suspended = true;
        shared.set_state(EngineState::Error);
    } else if init_required_seq.is_some()
        && !processor.is_chain_ready(active_sink, active_ratio, init_required_seq)
    {
        suspended = true;
        awaiting_chain_for_resume = Some(init_required_seq);
        shared.set_state(EngineState::Switching);
    }

    let trigger_and_wait = |clock_state: &mut DriverClockState,
                            stream: SendRawStream,
                            cur_rate: u32,
                            cur_q: u64,
                            delay_graph: i64,
                            timeout_ms: i32,
                            completed: &AtomicU64| {
        let before = completed.load(Ordering::Acquire);
        clock_state.advance_and_trigger(
            &shared.io,
            stream.0.as_ptr(),
            cur_rate,
            cur_q,
            delay_graph,
        );
        let mut pfd = [libc::pollfd {
            fd: data_efd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let start_wait_ns = mono_now_ns();
        let timeout_ns = (timeout_ms.max(0) as u64) * 1_000_000;
        while completed.load(Ordering::Acquire) == before && !shared.stop.load(Ordering::Acquire) {
            let elapsed_ns = mono_now_ns().saturating_sub(start_wait_ns);
            if elapsed_ns >= timeout_ns {
                break;
            }
            let rem_ms = ((timeout_ns - elapsed_ns) / 1_000_000).max(1) as i32;
            unsafe {
                libc::poll(pfd.as_mut_ptr(), 1, rem_ms);
            }
            data_efd.drain();
        }
    };

    while !shared.stop.load(Ordering::Acquire) {
        vs.sync_hold(&shared.varispeed_cmd, &shared.varispeed);
        rt_guard.on_iteration(&shared.rt, processor.running_generation(), vs.hold_seq());
        event_stash.flush(&mut rt_to_ctrl_tx, &ctrl_efd);

        // 1. Drain control commands.
        while let Ok(cmd) = ctrl_to_rt_rx.pop() {
            match cmd {
                RtControlCmd::UpdateDesiredDsp(cfg) => {
                    if crate::lifecycle::rt_takes(&rt_dsp_cfg, &cfg) {
                        rt_dsp_cfg = cfg;
                    }
                }
                RtControlCmd::Resume {
                    alsa,
                    alsa_fds: new_fds,
                    params: new_params,
                    graph_rate,
                    actual_ratio,
                    target_sink,
                    desired_dsp,
                    negotiated_key,
                    required_chain_seq,
                    new_driver_stream,
                    new_dsp_cons,
                    new_direct_cons,
                    dsp_in_buf: new_dsp_in,
                    direct_in_buf: new_dir_in,
                    out_buf: new_out,
                    fade_in_frames,
                    hold_error,
                } => {
                    if let Some(s) = new_driver_stream {
                        raw_stream = s;
                    }
                    let old_dsp_cons = new_dsp_cons.map(|c| std::mem::replace(&mut dsp_cons, c));
                    let old_direct_cons =
                        new_direct_cons.map(|c| std::mem::replace(&mut direct_cons, c));
                    let old_fds = std::mem::replace(&mut alsa_fds, new_fds);
                    let old_dsp_in = new_dsp_in.map(|b| std::mem::replace(&mut dsp_in_buf, b));
                    let old_direct_in =
                        new_dir_in.map(|b| std::mem::replace(&mut direct_in_buf, b));
                    let old_out = new_out.map(|b| std::mem::replace(&mut out_buf, b));
                    if !old_fds.is_empty()
                        || old_dsp_cons.is_some()
                        || old_direct_cons.is_some()
                        || old_dsp_in.is_some()
                        || old_direct_in.is_some()
                        || old_out.is_some()
                    {
                        let _ = event_stash.push_or_stash(
                            RtEvent::RecycleBuffers {
                                old_fds,
                                old_dsp_cons,
                                old_direct_cons,
                                old_dsp_in,
                                old_direct_in,
                                old_out,
                            },
                            &mut rt_to_ctrl_tx,
                            &ctrl_efd,
                        );
                    }
                    if crate::lifecycle::rt_takes(&rt_dsp_cfg, &desired_dsp) {
                        rt_dsp_cfg = desired_dsp;
                    }
                    last_negotiated_key = Some(negotiated_key);
                    if graph_rate != active_graph_rate {
                        shared.rate_switches.fetch_add(1, Ordering::Relaxed);
                    }
                    shared.resumes.fetch_add(1, Ordering::Relaxed);
                    shared.fade_ins.fetch_add(1, Ordering::Relaxed);
                    alsa_opt = Some(alsa);
                    handle_xruns = 0;
                    params = new_params;
                    active_graph_rate = graph_rate;
                    landed = false;
                    active_ratio = actual_ratio;
                    active_sink = target_sink;
                    released_idle = false;
                    reopen_gate.rt_finish_reopen();
                    pending_reopen = None;
                    inflight_reopen = None;
                    aborted_reopen = None;
                    alsa_running = false;
                    first_prime_after_reopen = true;
                    // A reopen starts a new stream on the device: the frames
                    // carried from the old one's periods do not apply.
                    planner.reset();
                    carry.clear();
                    fade_ramp.start_fade_in(fade_in_frames);
                    processor.poll_updates(active_sink, active_graph_rate, active_ratio, false);
                    // A reopen restarts the stage under the fade-in;
                    // the priming starts the schedule again.
                    if let Some(port) =
                        processor.varispeed(active_sink, active_ratio, active_graph_rate, channels)
                    {
                        port.reset();
                    }
                    vs.stop();
                    vs.publish(&shared.varispeed);
                    if hold_error {
                        // The chain rebuild failed outright (no fallback chain
                        // valid for this ratio): never report `Playing` or clear
                        // the error. Stay suspended until a later config apply
                        // or reopen negotiates a usable chain.
                        suspended = true;
                        awaiting_chain_for_resume = None;
                        shared.set_state(EngineState::Error);
                    } else {
                        awaiting_chain_for_resume = Some(required_chain_seq);
                        try_complete_resume_gate(
                            &mut processor,
                            &shared,
                            &mut awaiting_chain_for_resume,
                            &mut suspended,
                            &mut resume_ack,
                            active_sink,
                            active_ratio,
                            &mut event_stash,
                            &mut rt_to_ctrl_tx,
                            &ctrl_efd,
                        );
                    }
                }
                RtControlCmd::SuspendForRelease => {
                    let old_alsa =
                        take_counted(&mut alsa_opt, |a| a.xruns(), &shared, &mut handle_xruns).map(
                            |mut a| {
                                let _ = a.drop_frames();
                                a
                            },
                        );
                    let old_fds = std::mem::take(&mut alsa_fds);
                    let _ = event_stash.push_or_stash(
                        RtEvent::ReleasedIdle { old_alsa, old_fds },
                        &mut rt_to_ctrl_tx,
                        &ctrl_efd,
                    );
                    alsa_running = false;
                    suspended = false;
                    awaiting_chain_for_resume = None;
                    reopen_gate.rt_finish_reopen();
                    pending_reopen = None;
                    released_idle = true;
                    inflight_reopen = None;
                    aborted_reopen = None;
                    // Control thread publishes `EngineState::Idle` only after full
                    // ALSA close + PipeWire restore/disconnect + ReserveDevice1 drop.
                    shared.set_state(EngineState::Switching);
                }
                RtControlCmd::SuspendForReopen => {
                    if !released_idle && reopen_gate.rt_try_begin_reopen() {
                        let old_alsa =
                            take_counted(&mut alsa_opt, |a| a.xruns(), &shared, &mut handle_xruns)
                                .map(|mut a| {
                                    let _ = a.drop_frames();
                                    a
                                });
                        let old_fds = std::mem::take(&mut alsa_fds);
                        let cur_q = if old_alsa.is_some() {
                            active_ratio.scale_to_input(params.period_frames).max(32)
                        } else {
                            scaled_quantum(config.quantum, active_graph_rate)
                        };
                        inflight_reopen = Some((active_graph_rate, active_sink));
                        aborted_reopen = None;
                        let _ = event_stash.push_or_stash(
                            RtEvent::NeedReopen {
                                target_rate: active_graph_rate,
                                target_quantum: cur_q,
                                target_sink: active_sink,
                                old_alsa,
                                old_fds,
                                device_failed: false,
                            },
                            &mut rt_to_ctrl_tx,
                            &ctrl_efd,
                        );
                        shared.reopens.fetch_add(1, Ordering::Relaxed);
                        alsa_running = false;
                        suspended = true;
                        awaiting_chain_for_resume = None;
                        pending_reopen = None;
                        shared.set_state(EngineState::Switching);
                    }
                }
                RtControlCmd::ReopenAborted => {
                    reopen_gate.rt_finish_reopen();
                    aborted_reopen = inflight_reopen.take();
                }
            }
        }

        // A stream starting on either sink starts a playback session:
        // every ladder entry a blind open saw fail gets another try, here
        // and in the daemon's predictions (`NegotiatedKey::in_session`).
        let (dsp_streams, direct_streams) = shared.session_streams();
        let session = sessions.observe(dsp_streams, direct_streams);
        shared.playback_session.store(session, Ordering::Release);
        let live_key = last_negotiated_key.map(|k| k.in_session(session));

        // Poll lock-free DSP updates (chain swaps, A/B side, self-tests) on RT thread.
        let is_streaming = alsa_running && !suspended && !released_idle;
        processor.poll_updates(active_sink, active_graph_rate, active_ratio, is_streaming);
        if awaiting_chain_for_resume.is_some() {
            try_complete_resume_gate(
                &mut processor,
                &shared,
                &mut awaiting_chain_for_resume,
                &mut suspended,
                &mut resume_ack,
                active_sink,
                active_ratio,
                &mut event_stash,
                &mut rt_to_ctrl_tx,
                &ctrl_efd,
            );
        }
        shared.dsp_latency_out.store(
            processor.latency_out_frames(active_sink) as u64,
            Ordering::Relaxed,
        );

        if released_idle || suspended || alsa_opt.is_none() {
            // The last reopen failed (e.g. a route to a rate the device
            // cannot play). Retry as soon as the route or the graph rate
            // changes, so going back to a playable path recovers from
            // `Error` without a config apply.
            if let Some((failed_rate, failed_sink)) = aborted_reopen
                && !released_idle
                && alsa_opt.is_none()
                && awaiting_chain_for_resume.is_none()
            {
                let want_sink = shared.pick_sink().unwrap_or(failed_sink);
                let fallback_q = scaled_quantum(config.quantum, failed_rate) as u64;
                let (cur_rate, cur_q) = read_target_clock(&shared.io, failed_rate, fallback_q);
                // The control thread asks again once a lost device is back.
                let retry = shared.device_retry.load(Ordering::Acquire);
                if (want_sink != failed_sink || cur_rate != failed_rate || retry != seen_retry)
                    && reopen_gate.rt_try_begin_reopen()
                {
                    seen_retry = retry;
                    aborted_reopen = None;
                    inflight_reopen = Some((cur_rate, want_sink));
                    let _ = event_stash.push_or_stash(
                        RtEvent::NeedReopen {
                            target_rate: cur_rate,
                            target_quantum: cur_q as u32,
                            target_sink: want_sink,
                            old_alsa: None,
                            old_fds: Vec::new(),
                            device_failed: false,
                        },
                        &mut rt_to_ctrl_tx,
                        &ctrl_efd,
                    );
                    shared.reopens.fetch_add(1, Ordering::Relaxed);
                    suspended = true;
                    active_sink = want_sink;
                    shared.set_state(EngineState::Switching);
                }
            }
            let n_dsp = drain_ring(&mut dsp_cons) / channels;
            let n_dir = drain_ring(&mut direct_cons) / channels;
            shared
                .dsp_drained_idle
                .fetch_add(n_dsp as u64, Ordering::Relaxed);
            shared
                .direct_drained_idle
                .fetch_add(n_dir as u64, Ordering::Relaxed);
            // No device to pace the graph: while the control thread waits
            // for it, run the graph on our own clock at the rate PipeWire
            // asks for, so streams keep progressing (their audio is drained
            // above and discarded) instead of stalling or moving elsewhere.
            let mut wait_ns = 100_000_000u64;
            if alsa_opt.is_none()
                && !released_idle
                && shared.device_missing.load(Ordering::Acquire)
                && shared.driving.load(Ordering::Acquire)
                && shared.pick_sink().is_some()
            {
                let fallback_q = scaled_quantum(config.quantum, active_graph_rate) as u64;
                let (cur_rate, cur_q) =
                    read_target_clock(&shared.io, active_graph_rate, fallback_q);
                if mono_now_ns() >= clock_state.next_time_ns.saturating_sub(FREE_RUN_EARLY_NS) {
                    clock_state.advance_and_trigger(
                        &shared.io,
                        raw_stream.0.as_ptr(),
                        cur_rate,
                        cur_q,
                        0,
                    );
                    shared.graph_rate.store(cur_rate, Ordering::Release);
                    shared.quantum.store(cur_q as u32, Ordering::Release);
                }
                wait_ns = clock_state
                    .next_time_ns
                    .saturating_sub(mono_now_ns())
                    .min(wait_ns);
            } else {
                // A new run (or the device's priming) starts a fresh clock.
                clock_state.next_time_ns = 0;
            }
            let mut fds = [libc::pollfd {
                fd: cmd_efd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let timeout = libc::timespec {
                tv_sec: (wait_ns / 1_000_000_000) as libc::time_t,
                tv_nsec: (wait_ns % 1_000_000_000) as libc::c_long,
            };
            unsafe {
                libc::ppoll(fds.as_mut_ptr(), 1, &timeout, std::ptr::null());
            }
            cmd_efd.drain();
            continue;
        }

        // Determine active sink under v1 policy:
        // Direct owns the DAC exclusively while a running client feeds it
        // (`SharedEngineState::pick_sink`), otherwise the DSP sink while it
        // streams.
        // Note: PipeWire always invokes `direct_stream.process()` (with silence) when the
        // `audivane-dac` driver group runs, so `direct_cons.slots()` is NOT a client indicator.
        let picked = shared.pick_sink();
        let want_active = shared.driving.load(Ordering::Acquire) && picked.is_some();
        let target_sink = picked.unwrap_or(active_sink);
        shared.active_sink.store(
            if want_active {
                sink_to_u8(Some(target_sink))
            } else {
                0
            },
            Ordering::Release,
        );

        if !want_active {
            if alsa_running {
                shared.idle_stops.fetch_add(1, Ordering::Relaxed);
                if let Some(alsa) = alsa_opt.as_mut() {
                    if !shared.stop.load(Ordering::Acquire) {
                        drain_alsa_tail(
                            alsa,
                            active_sink,
                            &mut dsp_cons,
                            &mut direct_cons,
                            &mut processor,
                            &mut fade_ramp,
                            &mut dsp_in_buf,
                            &mut direct_in_buf,
                            &mut out_buf,
                            params,
                            channels,
                            active_ratio,
                            active_graph_rate,
                            &mut planner,
                            &mut carry,
                            &mut vs,
                            &shared.stop,
                            &shared,
                            &mut rt_guard,
                        );
                    }
                    let _ = alsa.drop_frames();
                    // An idle handle may stay open a long time: count an
                    // underrun in the drain now, not at the close.
                    note_xruns(&shared, alsa.xruns(), &mut handle_xruns);
                }
                processor.reset_dsp_state();
                planner.reset();
                carry.clear();
                alsa_running = false;
                pacer.reset();
                vs.stop();
                vs.publish(&shared.varispeed);
                shared.alsa_delay_frames.store(0, Ordering::Relaxed);
                shared
                    .dsp_load_bits
                    .store(0.0_f64.to_bits(), Ordering::Relaxed);
                if shared.state() == EngineState::Playing {
                    shared.set_state(EngineState::Ready);
                }
            }
            // The device went while nothing played: ALSA says nothing
            // then, the control thread's presence watch does. Hand it back
            // as after a failure; the reopen finds it gone and waits.
            if shared.device_gone.load(Ordering::Acquire)
                && !suspended
                && hand_back_failed_device(
                    &mut alsa_opt,
                    &mut alsa_fds,
                    &shared,
                    &mut handle_xruns,
                    &mut reopen_gate,
                    &mut event_stash,
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                    (
                        active_graph_rate,
                        scaled_quantum(config.quantum, active_graph_rate),
                    ),
                    active_sink,
                )
            {
                inflight_reopen = Some((active_graph_rate, active_sink));
                aborted_reopen = None;
                suspended = true;
                awaiting_chain_for_resume = None;
                pending_reopen = None;
                continue;
            }
            // The streams stopped before a resumed device started: it is
            // ready, and the resume is done.
            if !suspended && std::mem::take(&mut resume_ack) {
                shared.set_state(EngineState::Ready);
                let _ = event_stash.push_or_stash(RtEvent::Resumed, &mut rt_to_ctrl_tx, &ctrl_efd);
            }
            let mut fds = [libc::pollfd {
                fd: cmd_efd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            unsafe {
                libc::poll(fds.as_mut_ptr(), 1, 200);
            }
            cmd_efd.drain();
            continue;
        }

        if !alsa_running {
            let n_dsp = drain_ring(&mut dsp_cons) / channels;
            let n_dir = drain_ring(&mut direct_cons) / channels;
            shared
                .dsp_drained_prime
                .fetch_add(n_dsp as u64, Ordering::Relaxed);
            shared
                .direct_drained_prime
                .fetch_add(n_dir as u64, Ordering::Relaxed);
            data_efd.drain();
            pacer.reset();
            clock_state.reset();

            let default_q = active_ratio.scale_to_input(params.period_frames).max(32) as u64;
            let (cur_rate, cur_q) = read_target_clock(&shared.io, active_graph_rate, default_q);
            shared.graph_rate.store(cur_rate, Ordering::Release);
            shared.quantum.store(cur_q as u32, Ordering::Release);
            note_landing(&shared, &mut landed, cur_rate, active_graph_rate);

            // Check if sink switch (e.g. Direct ratio 1 vs DSP resolved ratio),
            // updated DesiredDspConfig, or rate change requires ALSA reopen;
            // so does a switch to a sink whose chain is not there (only a
            // negotiation builds it).
            if alsa_opt.is_some()
                && (decide_rt_reopen(
                    rt_dsp_cfg,
                    live_key,
                    active_sink,
                    target_sink,
                    active_graph_rate,
                    cur_rate,
                    active_ratio,
                )
                .is_some()
                    || (target_sink != active_sink
                        && !processor.has_chain_for(target_sink, active_ratio)))
                && reopen_gate.rt_try_begin_reopen()
                && let Some(mut old_alsa) =
                    take_counted(&mut alsa_opt, |a| a.xruns(), &shared, &mut handle_xruns)
            {
                let _ = old_alsa.drop_frames();
                let old_fds = std::mem::take(&mut alsa_fds);
                inflight_reopen = Some((cur_rate, target_sink));
                let _ = event_stash.push_or_stash(
                    RtEvent::NeedReopen {
                        target_rate: cur_rate,
                        target_quantum: cur_q as u32,
                        target_sink,
                        old_alsa: Some(old_alsa),
                        old_fds,
                        device_failed: false,
                    },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                );
                shared.reopens.fetch_add(1, Ordering::Relaxed);
                suspended = true;
                shared.set_state(EngineState::Switching);
                continue;
            }
            active_sink = target_sink;

            let Some(alsa) = alsa_opt.as_mut() else {
                continue;
            };
            let unplugged = shared.device_gone.load(Ordering::Relaxed);
            if unplugged || alsa.prepare().is_err() {
                // Gone (usually unplugged): the control thread opens it
                // again, or waits for it with the graph on our own clock.
                if hand_back_failed_device(
                    &mut alsa_opt,
                    &mut alsa_fds,
                    &shared,
                    &mut handle_xruns,
                    &mut reopen_gate,
                    &mut event_stash,
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                    (
                        active_graph_rate,
                        scaled_quantum(config.quantum, active_graph_rate),
                    ),
                    active_sink,
                ) {
                    inflight_reopen = Some((active_graph_rate, active_sink));
                    aborted_reopen = None;
                    suspended = true;
                    awaiting_chain_for_resume = None;
                    pending_reopen = None;
                    vs.stop();
                    vs.publish(&shared.varispeed);
                    continue;
                }
                let _ = event_stash.push_or_stash(
                    RtEvent::FatalError {
                        msg: "ALSA prepare failed",
                    },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                );
                break;
            }
            processor.stream_prepared();

            let period_out = params.period_frames.max(1);
            // Nominal input frames per period (the exact count varies by
            // one on a rational frame grid; see `PeriodPlanner`).
            let period_in = planner.plan(active_ratio, period_out).0 as u32;
            let n_prefill_periods = prefill_periods(params.buffer_frames, period_out);
            // Prime at the nominal graph rate instead of as fast as the graph
            // can complete: consuming several quanta in a burst outruns the
            // source client's converter, which then fills a cycle with silence
            // (an audible dropout right after playback starts).
            let period_ns = (u64::from(period_in) * 1_000_000_000) / u64::from(cur_rate.max(8_000));
            let cycle_ns = (cur_q.max(1) * 1_000_000_000) / u64::from(cur_rate.max(8_000));
            let prime_start_ns = mono_now_ns();
            let mut prime_deadline_ns = prime_start_ns;
            // The clock delay's geometry is this open's, also when the
            // stage only starts later (a live handover).
            vs_delay.set_geometry(active_ratio, active_graph_rate);
            // Varispeed primes on its own schedule: the ring flushed, the
            // scheduler reset, cycles paced at the target speed until each
            // prime period's pull and then the watermark are in.
            let vs_prime = cur_rate == active_graph_rate
                && match processor.varispeed(active_sink, active_ratio, active_graph_rate, channels)
                {
                    Some(port) => {
                        let timing = Timing::new(
                            active_graph_rate,
                            vs_period_frames(active_ratio, period_out),
                            cur_q as u32,
                        );
                        // The latest command and the new cap first: the
                        // stage resets at the capped target, so the first
                        // prime block plays at it.
                        vs.start_priming(timing, port, &shared.varispeed_cmd);
                        vs_delay = VsDelay::new(active_ratio, active_graph_rate);
                        vs_delay.dac_read(mono_now_ns(), 0, false);
                        vs_wake = None;
                        pace_q = cur_q;
                        pace_rate = cur_rate;
                        true
                    }
                    None => {
                        vs.stop();
                        false
                    }
                };
            if vs_prime {
                vs_delay.set_latency(processor.latency_out_frames(active_sink));
            }
            vs.publish(&shared.varispeed);
            let pace_until = |deadline_ns: u64| {
                loop {
                    let now = mono_now_ns();
                    if now >= deadline_ns || shared.stop.load(Ordering::Acquire) {
                        break;
                    }
                    let rem_ms = ((deadline_ns - now) / 1_000_000).max(1) as i32;
                    let mut fds = [libc::pollfd {
                        fd: cmd_efd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    }];
                    unsafe {
                        libc::poll(fds.as_mut_ptr(), 1, rem_ms);
                    }
                    cmd_efd.drain();
                }
            };

            for p in 0..n_prefill_periods {
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                // A prefill with the source already buffered runs period
                // after period without the RT loop's own checkpoint.
                rt_guard.on_iteration(&shared.rt, processor.running_generation(), vs.hold_seq());
                if !vs_prime {
                    pace_until(prime_deadline_ns);
                }
                let (prime_in, prime_out) = planner.plan(active_ratio, period_out);
                let prime_timeout_ms = if first_prime_after_reopen && p == 0 {
                    80
                } else {
                    60
                };
                if vs_prime {
                    // Varispeed: each stage call's planned source frames,
                    // on the schedule's priming pace.
                    let out_slice = &mut out_buf[..prime_out * channels];
                    let ran = {
                        let mut graph = RtGraph {
                            clock_state: &mut clock_state,
                            shared: &shared,
                            raw_stream,
                            data_efd: &data_efd,
                            other: Some(&mut direct_cons),
                            channels,
                            guard: &mut rt_guard,
                            mark: (processor.running_generation(), vs.hold_seq()),
                        };
                        vsl::run_period(
                            &mut vs,
                            &mut graph,
                            &mut processor,
                            PeriodBufs {
                                ring: &mut dsp_cons,
                                dsp_in: &mut dsp_in_buf,
                                direct_in: &direct_in_buf,
                                out: out_slice,
                            },
                            &shared.varispeed_cmd,
                            PeriodPlan {
                                sink: active_sink,
                                ratio: active_ratio,
                                graph_rate: active_graph_rate,
                                channels,
                                pos: planner.pos,
                                n_in: prime_in,
                                n_out: prime_out,
                                kind: PullKind::Play,
                                prime_wait_ns: Some(prime_timeout_ms as u64 * 1_000_000),
                                poll: false,
                                quantum: cur_q,
                            },
                            &mut vs_delay,
                        )
                    };
                    match ran {
                        Some(run) => {
                            shared
                                .dsp_popped_frames
                                .fetch_add(run.taken as u64, Ordering::Relaxed);
                            planner.commit(prime_in, prime_out, period_out);
                            fade_ramp.apply_interleaved(out_slice, channels);
                            let (w, _) =
                                carry.prime_write(&mut **alsa, out_slice, channels, &|| {
                                    shared.stop.load(Ordering::Acquire)
                                        || shared.release_requested.load(Ordering::Acquire)
                                });
                            shared.frames_out.fetch_add(w as u64, Ordering::Relaxed);
                            vs_delay.wrote(w, prime_in);
                        }
                        None => {
                            vs_delay.clear_pending();
                            if carry.is_empty()
                                && let Some(w) =
                                    prime_silence(&mut **alsa, &mut planner, prime_out, period_out)
                            {
                                shared.prime_silence_periods.fetch_add(1, Ordering::Relaxed);
                                shared.frames_out.fetch_add(w as u64, Ordering::Relaxed);
                                vs_delay.wrote(w, 0);
                            }
                        }
                    }
                    continue;
                }
                let sample_len_in = prime_in * channels;
                let cons_slots = match active_sink {
                    SinkChoice::Direct => direct_cons.slots(),
                    _ => dsp_cons.slots(),
                };
                let mut completion_ns = None;
                if cons_slots < sample_len_in {
                    let completed = shared.cycle_counter(active_sink == SinkChoice::Direct);
                    // A period may need more than one graph cycle when the
                    // quantum PipeWire runs is smaller than the period; space
                    // them like the steady loop does.
                    let max_cycles = (prime_in as u32).div_ceil(cur_q.max(1) as u32).max(1);
                    for c in 0..max_cycles {
                        if c > 0 {
                            let slots = match active_sink {
                                SinkChoice::Direct => direct_cons.slots(),
                                _ => dsp_cons.slots(),
                            };
                            if slots >= sample_len_in || shared.stop.load(Ordering::Acquire) {
                                break;
                            }
                            pace_until(mono_now_ns().saturating_add(cycle_ns / 2));
                        }
                        trigger_and_wait(
                            &mut clock_state,
                            raw_stream,
                            cur_rate,
                            cur_q,
                            (p as i64) * (period_in as i64),
                            prime_timeout_ms,
                            completed,
                        );
                        completion_ns = Some(mono_now_ns());
                    }
                }
                // Never fire the next cycle sooner than one nominal period
                // after either the previous deadline or the previous cycle's
                // completion. A slow first cycle (client startup, chain load)
                // must not turn the remaining primes into a catch-up burst.
                prime_deadline_ns =
                    next_prime_deadline_ns(prime_deadline_ns, completion_ns, period_ns);
                let have_data = match active_sink {
                    SinkChoice::Direct => {
                        let n = drain_ring(&mut dsp_cons) / channels;
                        shared
                            .dsp_drained_unused
                            .fetch_add(n as u64, Ordering::Relaxed);
                        direct_cons.slots() >= sample_len_in
                    }
                    _ => {
                        let n = drain_ring(&mut direct_cons) / channels;
                        shared
                            .direct_drained_unused
                            .fetch_add(n as u64, Ordering::Relaxed);
                        dsp_cons.slots() >= sample_len_in
                    }
                };
                if have_data {
                    match active_sink {
                        SinkChoice::Direct => {
                            let n = pop_f32_zero_pad(
                                &mut direct_cons,
                                &mut direct_in_buf[..sample_len_in],
                            );
                            shared
                                .direct_popped_frames
                                .fetch_add((n / channels) as u64, Ordering::Relaxed);
                        }
                        _ => {
                            let n =
                                pop_f32_zero_pad(&mut dsp_cons, &mut dsp_in_buf[..sample_len_in]);
                            shared
                                .dsp_popped_frames
                                .fetch_add((n / channels) as u64, Ordering::Relaxed);
                        }
                    }
                    let out_slice = &mut out_buf[..prime_out * channels];
                    processor.process_block(
                        active_sink,
                        &dsp_in_buf[..sample_len_in],
                        &direct_in_buf[..sample_len_in],
                        out_slice,
                        channels,
                        active_ratio,
                        active_graph_rate,
                        planner.pos,
                    );
                    planner.commit(prime_in, prime_out, period_out);
                    fade_ramp.apply_interleaved(out_slice, channels);
                    // Auto-start is off: a write that does not fit would
                    // wait for room that only `start()` frees.
                    let (w, _) = carry.prime_write(&mut **alsa, out_slice, channels, &|| {
                        shared.stop.load(Ordering::Acquire)
                            || shared.release_requested.load(Ordering::Acquire)
                    });
                    shared.frames_out.fetch_add(w as u64, Ordering::Relaxed);
                } else if carry.is_empty()
                    && let Some(w) = prime_silence(&mut **alsa, &mut planner, prime_out, period_out)
                {
                    shared.prime_silence_periods.fetch_add(1, Ordering::Relaxed);
                    shared.frames_out.fetch_add(w as u64, Ordering::Relaxed);
                }
            }
            first_prime_after_reopen = false;

            if shared.stop.load(Ordering::Acquire) {
                break;
            }
            // Trigger one extra PipeWire cycle and wait for its completion before
            // starting ALSA DMA so the ring holds one period ahead of the first
            // hardware IRQ (avoids a startup race with PipeWire graph wakeup).
            // When the period and the quantum differ, the cushion also
            // covers the cycles still owed to a period when it is written.
            let ring_frames = dsp_cons.buffer().capacity() / channels;
            let cushion_slots =
                start_cushion_frames(period_in, cur_q as u32, ring_frames) * channels;
            let max_cushion_cycles = if vs_prime {
                // Varispeed's start cushion is the low watermark,
                // on the scheduler's priming pace.
                let want = vs.scheduler().watermark();
                let speed = processor
                    .varispeed(active_sink, active_ratio, active_graph_rate, channels)
                    .map_or(1.0, |port| port.speed());
                let cycles = want.div_ceil(cur_q.max(1) as usize) as u64 + 2;
                let timeout_ns = cycles * cycle_ns.max(1_000_000) * 2 + 60_000_000;
                let mut graph = RtGraph {
                    clock_state: &mut clock_state,
                    shared: &shared,
                    raw_stream,
                    data_efd: &data_efd,
                    other: Some(&mut direct_cons),
                    channels,
                    guard: &mut rt_guard,
                    mark: (processor.running_generation(), vs.hold_seq()),
                };
                vsl::fill_until(
                    &mut vs, &mut graph, &dsp_cons, channels, cur_rate, cur_q, &vs_delay, speed,
                    want, timeout_ns,
                );
                0
            } else {
                (cushion_slots / channels)
                    .div_ceil(cur_q.max(1) as usize)
                    .max(1)
            };
            for c in 0..max_cushion_cycles {
                rt_guard.on_iteration(&shared.rt, processor.running_generation(), vs.hold_seq());
                let have_cushion = match active_sink {
                    SinkChoice::Direct => direct_cons.slots() >= cushion_slots,
                    _ => dsp_cons.slots() >= cushion_slots,
                };
                if have_cushion || shared.stop.load(Ordering::Acquire) {
                    break;
                }
                if c == 0 {
                    pace_until(prime_deadline_ns);
                } else {
                    pace_until(mono_now_ns().saturating_add(cycle_ns / 2));
                }
                let completed = shared.cycle_counter(active_sink == SinkChoice::Direct);
                trigger_and_wait(
                    &mut clock_state,
                    raw_stream,
                    cur_rate,
                    cur_q,
                    i64::from(active_ratio.scale_to_input(params.buffer_frames)),
                    60,
                    completed,
                );
                // The inactive sink's ring fills too; keep it from overflowing.
                match active_sink {
                    SinkChoice::Direct => {
                        let n = drain_ring(&mut dsp_cons) / channels;
                        shared
                            .dsp_drained_unused
                            .fetch_add(n as u64, Ordering::Relaxed);
                    }
                    _ => {
                        let n = drain_ring(&mut direct_cons) / channels;
                        shared
                            .direct_drained_unused
                            .fetch_add(n as u64, Ordering::Relaxed);
                    }
                }
            }
            if alsa.start().is_err() {
                if hand_back_failed_device(
                    &mut alsa_opt,
                    &mut alsa_fds,
                    &shared,
                    &mut handle_xruns,
                    &mut reopen_gate,
                    &mut event_stash,
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                    (
                        active_graph_rate,
                        scaled_quantum(config.quantum, active_graph_rate),
                    ),
                    active_sink,
                ) {
                    inflight_reopen = Some((active_graph_rate, active_sink));
                    aborted_reopen = None;
                    suspended = true;
                    awaiting_chain_for_resume = None;
                    pending_reopen = None;
                    planner.reset();
                    carry.clear();
                    vs.stop();
                    vs.publish(&shared.varispeed);
                    continue;
                }
                let _ = event_stash.push_or_stash(
                    RtEvent::FatalError {
                        msg: "ALSA start failed",
                    },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                );
                break;
            }
            alsa_running = true;
            if vs_prime {
                vs.finish_priming();
                vs.publish(&shared.varispeed);
                let queued = alsa.delay().unwrap_or(0);
                vs_delay.dac_read(mono_now_ns(), queued, true);
            }
            on_device_started(
                &shared,
                &mut resume_ack,
                &mut event_stash,
                &mut rt_to_ctrl_tx,
                &ctrl_efd,
            );
            if let Ok(d) = alsa.delay() {
                shared.alsa_delay_frames.store(d, Ordering::Relaxed);
            }
            continue;
        }

        // Steady-state polling.
        pollfds.clear();
        pollfds.extend_from_slice(&alsa_fds);
        let cmd_idx = pollfds.len();
        pollfds.push(libc::pollfd {
            fd: cmd_efd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        // Varispeed also wakes on each cycle completion (`data_efd`).
        let data_idx = vs.is_active().then(|| {
            pollfds.push(libc::pollfd {
                fd: data_efd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            pollfds.len() - 1
        });
        // Wake for an owed graph cycle (period/quantum mismatch, or the
        // varispeed schedule's next slot) as well as for the DAC.
        let next_due = if vs.is_active() {
            vs_wake
        } else {
            pacer.next_due_ns()
        };
        let timeout_ns = next_due.map_or(200_000_000, |due| {
            due.saturating_sub(mono_now_ns()).min(200_000_000)
        });
        let timeout = libc::timespec {
            tv_sec: (timeout_ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (timeout_ns % 1_000_000_000) as libc::c_long,
        };
        let rc = unsafe {
            libc::ppoll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                &timeout,
                std::ptr::null(),
            )
        };
        vs.sync_hold(&shared.varispeed_cmd, &shared.varispeed);
        rt_guard.on_iteration(&shared.rt, processor.running_generation(), vs.hold_seq());
        if rc >= 0 && vs.is_active() {
            // A completion or the slot time: the schedule may trigger the
            // next cycle. The ALSA fds ready in the same ppoll are handled
            // below as usual.
            let completed = data_idx.is_some_and(|i| pollfds[i].revents != 0);
            if completed {
                data_efd.drain();
            }
            if completed || vs_wake.is_some_and(|w| mono_now_ns() >= w) {
                let speed = processor
                    .varispeed(active_sink, active_ratio, active_graph_rate, channels)
                    .map_or(1.0, |port| port.speed());
                let mut graph = RtGraph {
                    clock_state: &mut clock_state,
                    shared: &shared,
                    raw_stream,
                    data_efd: &data_efd,
                    other: None,
                    channels,
                    guard: &mut rt_guard,
                    mark: (processor.running_generation(), vs.hold_seq()),
                };
                vs_wake = vsl::poll_trigger(
                    &mut vs, &mut graph, &dsp_cons, channels, pace_rate, pace_q, &vs_delay, speed,
                );
            }
        } else if rc >= 0 && pacer.take_due(mono_now_ns(), pace_q, pace_rate) {
            clock_state.advance_and_trigger(
                &shared.io,
                raw_stream.0.as_ptr(),
                pace_rate,
                pace_q,
                pace_delay_graph,
            );
        }
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            let _ = event_stash.push_or_stash(
                RtEvent::FatalError {
                    msg: "ALSA poll error",
                },
                &mut rt_to_ctrl_tx,
                &ctrl_efd,
            );
            break;
        }
        if pollfds[cmd_idx].revents != 0 {
            cmd_efd.drain();
            continue;
        }
        if rc == 0 {
            continue;
        }
        if data_idx.is_some() && pollfds[..cmd_idx].iter().all(|p| p.revents == 0) {
            // Only a cycle completion: nothing for the DAC yet.
            continue;
        }

        let Some(alsa) = alsa_opt.as_mut() else {
            continue;
        };
        let _ = alsa.poll_revents(&pollfds[..cmd_idx]);

        let (avail, delay) = match alsa.avail_and_delay() {
            Ok(v) => v,
            Err(_) => {
                note_xruns(&shared, alsa.xruns(), &mut handle_xruns);
                alsa_running = false;
                continue;
            }
        };
        shared.alsa_delay_frames.store(delay, Ordering::Relaxed);
        note_xruns(&shared, alsa.xruns(), &mut handle_xruns);
        // The clock delay's DAC term, tracked from here to each trigger.
        vs_delay.dac_read(mono_now_ns(), delay, true);
        vs_delay.clear_pending();
        processor.dac_read(delay, mono_raw_now_ns());

        if alsa.needs_restart() || !alsa.is_running() || shared.device_gone.load(Ordering::Relaxed)
        {
            alsa_running = false;
            continue;
        }

        let default_q = active_ratio.scale_to_input(params.period_frames).max(32) as u64;
        let (cur_rate, cur_q) = read_target_clock(&shared.io, active_graph_rate, default_q);
        shared.graph_rate.store(cur_rate, Ordering::Release);
        shared.quantum.store(cur_q as u32, Ordering::Release);
        note_landing(&shared, &mut landed, cur_rate, active_graph_rate);

        // Check if active sink switch, updated DesiredDspConfig, or rate switch requires ALSA reopen.
        let need_reopen = decide_rt_reopen(
            rt_dsp_cfg,
            live_key,
            active_sink,
            target_sink,
            active_graph_rate,
            cur_rate,
            active_ratio,
        )
        .is_some()
            || (target_sink != active_sink && !processor.has_chain_for(target_sink, active_ratio));
        if need_reopen && pending_reopen.is_none() {
            pending_reopen = Some((cur_rate, cur_q as u32, target_sink));
            let fade_out_frames = ((params.rate as usize) * FADE_MS) / 1000;
            fade_ramp.start_fade_out(fade_out_frames);
            shared.fade_outs.fetch_add(1, Ordering::Relaxed);
            shared.set_state(EngineState::Switching);
        } else if target_sink != active_sink && pending_reopen.is_none() {
            active_sink = target_sink;
        }

        let period_out = params.period_frames.max(1);
        let (chunk_in, chunk_out) = planner.plan(active_ratio, period_out);
        if (avail as usize) < chunk_out {
            continue;
        }
        // Frames the prefill planned beyond the buffer go out first, before
        // any input is taken for the next block.
        match write_carry_ahead(&mut carry, &mut **alsa, channels, &shared) {
            CarryNext::Block => {}
            CarryNext::Wait => continue,
            CarryNext::Restart => {
                alsa_running = false;
                continue;
            }
        }

        // Varispeed: take the schedule over, retune it, or hand it back
        // (once the ring holds the first raw pull). No stage runs while the
        // graph moved to another rate (a reopen is fading out; the input is
        // discarded then).
        {
            let vs_period = vs_period_frames(active_ratio, period_out);
            let mark = (processor.running_generation(), vs.hold_seq());
            let port = if cur_rate == active_graph_rate {
                processor.varispeed(active_sink, active_ratio, active_graph_rate, channels)
            } else {
                None
            };
            let raw_pull = if active_sink == SinkChoice::Dsp {
                chunk_in
            } else {
                0
            };
            let wait_ns =
                (chunk_in as u64 * 1_000_000_000) / u64::from(active_graph_rate.max(1)) / 2;
            let mut graph = RtGraph {
                clock_state: &mut clock_state,
                shared: &shared,
                raw_stream,
                data_efd: &data_efd,
                other: None,
                channels,
                guard: &mut rt_guard,
                mark,
            };
            let h = vsl::handover(
                &mut vs,
                &mut graph,
                &dsp_cons,
                port,
                channels,
                active_graph_rate,
                vs_period,
                cur_q as u32,
                raw_pull,
                &vs_delay,
                wait_ns,
            );
            if matches!(h, Handover::Started | Handover::Restarted) {
                vs_delay.set_geometry(active_ratio, active_graph_rate);
            }
            if let Handover::HandedBack { .. } = h {
                pacer.reset();
                if active_sink == SinkChoice::Dsp {
                    // What the refill did not bring in yet: a period and a
                    // quantum, so the raw path's next pull finds them.
                    let q = cur_q.max(1) as usize;
                    let want = chunk_in + q;
                    let fill = vsl::ring_fill(&dsp_cons, channels);
                    pacer.owe(want.saturating_sub(fill).div_ceil(q) as u32);
                }
            }
            if h != Handover::Unchanged {
                vs.publish(&shared.varispeed);
            }
        }
        if vs.is_active() {
            pace_q = cur_q;
            pace_rate = cur_rate;
        } else {
            vs_wake = None;
        }

        let out_samples_len = chunk_out * channels;
        // Varispeed: the period's stage calls (split when the period is
        // longer than one call takes), each planned, pulled and processed;
        // the schedule is polled around each block.
        let mut t0 = mono_now_ns();
        let vs_ran = if vs.is_active()
            && processor
                .varispeed(active_sink, active_ratio, active_graph_rate, channels)
                .is_some()
        {
            let nd = drain_ring(&mut direct_cons) / channels;
            shared
                .direct_drained_unused
                .fetch_add(nd as u64, Ordering::Relaxed);
            vs_delay.set_latency(processor.latency_out_frames(active_sink));
            let mut graph = RtGraph {
                clock_state: &mut clock_state,
                shared: &shared,
                raw_stream,
                data_efd: &data_efd,
                other: None,
                channels,
                guard: &mut rt_guard,
                mark: (processor.running_generation(), vs.hold_seq()),
            };
            let run = vsl::run_period(
                &mut vs,
                &mut graph,
                &mut processor,
                PeriodBufs {
                    ring: &mut dsp_cons,
                    dsp_in: &mut dsp_in_buf,
                    direct_in: &direct_in_buf,
                    out: &mut out_buf[..out_samples_len],
                },
                &shared.varispeed_cmd,
                PeriodPlan {
                    sink: active_sink,
                    ratio: active_ratio,
                    graph_rate: active_graph_rate,
                    channels,
                    pos: planner.pos,
                    n_in: chunk_in,
                    n_out: chunk_out,
                    kind: PullKind::Play,
                    prime_wait_ns: None,
                    poll: true,
                    quantum: cur_q,
                },
                &mut vs_delay,
            );
            if let Some(run) = run {
                shared
                    .dsp_popped_frames
                    .fetch_add(run.taken as u64, Ordering::Relaxed);
            }
            run
        } else {
            None
        };
        if let Some(run) = vs_ran {
            vs_wake = run.wake_at_ns;
        }
        let in_samples_len = vs_ran.map_or(chunk_in * channels, |r| r.pulled * channels);

        let popped = match active_sink {
            _ if vs_ran.is_some() => vs_ran.map_or(0, |r| r.taken * channels),
            SinkChoice::Direct => {
                let nd = drain_ring(&mut dsp_cons) / channels;
                shared
                    .dsp_drained_unused
                    .fetch_add(nd as u64, Ordering::Relaxed);
                let (n, gone) = take_block_input(
                    &mut direct_cons,
                    &mut direct_in_buf[..in_samples_len],
                    cur_rate,
                    active_graph_rate,
                );
                shared
                    .direct_popped_frames
                    .fetch_add((n / channels) as u64, Ordering::Relaxed);
                shared
                    .direct_drained_unused
                    .fetch_add((gone / channels) as u64, Ordering::Relaxed);
                // Discarded input is no underrun.
                if gone > 0 { in_samples_len } else { n }
            }
            _ => {
                let nd = drain_ring(&mut direct_cons) / channels;
                shared
                    .direct_drained_unused
                    .fetch_add(nd as u64, Ordering::Relaxed);
                let (n, gone) = take_block_input(
                    &mut dsp_cons,
                    &mut dsp_in_buf[..in_samples_len],
                    cur_rate,
                    active_graph_rate,
                );
                shared
                    .dsp_popped_frames
                    .fetch_add((n / channels) as u64, Ordering::Relaxed);
                shared
                    .dsp_drained_unused
                    .fetch_add((gone / channels) as u64, Ordering::Relaxed);
                // Discarded input is no underrun.
                if gone > 0 { in_samples_len } else { n }
            }
        };
        if popped < in_samples_len {
            shared.ring_underruns.fetch_add(1, Ordering::Relaxed);
            let missing_frames = ((in_samples_len - popped) / channels) as u64;
            match active_sink {
                SinkChoice::Direct => {
                    shared.direct_zero_periods.fetch_add(1, Ordering::Relaxed);
                    shared
                        .direct_zero_frames
                        .fetch_add(missing_frames, Ordering::Relaxed);
                }
                _ => {
                    shared.dsp_zero_periods.fetch_add(1, Ordering::Relaxed);
                    shared
                        .dsp_zero_frames
                        .fetch_add(missing_frames, Ordering::Relaxed);
                }
            }
        }

        if let Some(stall) = test_stall
            && !stalled_once
            && shared.frames_out.load(Ordering::Relaxed) >= stall.after_frames_out
        {
            stalled_once = true;
            thread::sleep(Duration::from_millis(stall.stall_ms));
        }

        if vs_ran.is_none() {
            t0 = mono_now_ns();
            processor.process_block(
                active_sink,
                &dsp_in_buf[..in_samples_len],
                &direct_in_buf[..in_samples_len],
                &mut out_buf[..out_samples_len],
                channels,
                active_ratio,
                active_graph_rate,
                planner.pos,
            );
        }
        planner.commit(chunk_in, chunk_out, period_out);
        let dt_ns = mono_now_ns().saturating_sub(t0);
        let cycle_ns = (chunk_in as f64) * 1_000_000_000.0 / (active_graph_rate.max(1) as f64);
        let inst_load = if cycle_ns > 0.0 {
            (dt_ns as f64) / cycle_ns
        } else {
            0.0
        };
        dsp_load_ewma = dsp_load_ewma * 0.9 + inst_load * 0.1;
        shared
            .dsp_load_bits
            .store(dsp_load_ewma.to_bits(), Ordering::Relaxed);
        // `rt_load_max`, and the processing budget varispeed's cap is
        // derated for.
        vs.note_load(
            mono_now_ns(),
            inst_load,
            processor.varispeed(active_sink, active_ratio, active_graph_rate, channels),
        );
        shared
            .rt_load_max_bits
            .store(vs.load().peak().to_bits(), Ordering::Relaxed);

        fade_ramp.apply_interleaved(&mut out_buf[..out_samples_len], channels);

        let stop_write = || {
            shared.stop.load(Ordering::Acquire) || shared.release_requested.load(Ordering::Acquire)
        };
        let write_res = alsa
            .write_i32_all_until(&out_buf[..out_samples_len], stop_write)
            .ok();
        note_xruns(&shared, alsa.xruns(), &mut handle_xruns);
        rt_guard.period_done(&shared.rt);

        match write_res {
            Some(written) => {
                shared
                    .frames_out
                    .fetch_add(written as u64, Ordering::Relaxed);
                vs_delay.wrote(written, chunk_in);
            }
            None => {
                alsa_running = false;
                continue;
            }
        }

        if alsa.needs_restart() || !alsa.is_running() {
            alsa_running = false;
            continue;
        }

        if let Some((target_rate, target_quantum, reopen_sink)) = pending_reopen
            && fade_ramp.is_fade_out_done()
        {
            let _ = alsa.drop_frames();
            if alsa_opt.is_some()
                && reopen_gate.rt_try_begin_reopen()
                && let Some(old_alsa) =
                    take_counted(&mut alsa_opt, |a| a.xruns(), &shared, &mut handle_xruns)
            {
                let old_fds = std::mem::take(&mut alsa_fds);
                inflight_reopen = Some((target_rate, reopen_sink));
                let _ = event_stash.push_or_stash(
                    RtEvent::NeedReopen {
                        target_rate,
                        target_quantum,
                        target_sink: reopen_sink,
                        old_alsa: Some(old_alsa),
                        old_fds,
                        device_failed: false,
                    },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                );
                shared.reopens.fetch_add(1, Ordering::Relaxed);
                alsa_running = false;
                suspended = true;
                pending_reopen = None;
                continue;
            }
            pending_reopen = None;
        }

        // One graph cycle per quantum consumed. Cycles a period owes beyond
        // the first are fired from the poll above, spaced (never back to
        // back: the source clients could not keep up and PipeWire would
        // fill those cycles with silence).
        if vs.is_active() {
            vs.publish(&shared.varispeed);
            continue;
        }
        pacer.consumed(chunk_in as u64, cur_q);
        pace_q = cur_q;
        pace_rate = cur_rate;
        // The DAC's queue, and the alignment delay ahead of the chain
        // (multi-device; zero without followers).
        pace_delay_graph =
            i64::from(active_ratio.scale_to_input(delay.clamp(0, i64::from(u32::MAX)) as u32))
                + processor.alignment_frames(active_sink) as i64;
        if pacer.take_due(mono_now_ns(), cur_q, cur_rate) {
            clock_state.advance_and_trigger(
                &shared.io,
                raw_stream.0.as_ptr(),
                cur_rate,
                cur_q,
                pace_delay_graph,
            );
        }
    }

    event_stash.flush(&mut rt_to_ctrl_tx, &ctrl_efd);
    if let Some(mut alsa) = take_counted(&mut alsa_opt, |a| a.xruns(), &shared, &mut handle_xruns) {
        if alsa_running {
            let _ = alsa.drop_frames();
        }
        let old_fds = std::mem::take(&mut alsa_fds);
        let _ = event_stash.push_or_stash(
            RtEvent::ReleasedIdle {
                old_alsa: Some(alsa),
                old_fds,
            },
            &mut rt_to_ctrl_tx,
            &ctrl_efd,
        );
    }
}

// ------------------------------------------------- varispeed in the RT loop
//
// The RT loop's varispeed steps live here, apart from the loop, so the loop
// itself only calls them.

/// The largest nominal period, graph frames, the planner asks for at
/// `ratio` (a rational grid varies by a frame): the scheduler's `P`.
/// How early the free-running clock (no device) may fire a cycle: the
/// wait before it is a timed `ppoll`, which can wake a little late but
/// never much early.
const FREE_RUN_EARLY_NS: u64 = 250_000;

/// The RT thread's device failed in a way a restart of the stream cannot
/// fix (usually it was unplugged): hand it to the control thread for a
/// reopen ([`RtEvent::NeedReopen`] with `device_failed`). A failed reopen
/// then waits for the device while the graph runs on the RT thread's own
/// clock. `false` when a reopen is already in flight (the caller then ends
/// the thread).
#[allow(clippy::too_many_arguments)]
fn hand_back_failed_device<const N: usize>(
    alsa_opt: &mut Option<Box<AlsaOutput>>,
    alsa_fds: &mut Vec<libc::pollfd>,
    shared: &SharedEngineState,
    handle_xruns: &mut u64,
    reopen_gate: &mut ReopenGate,
    event_stash: &mut BoundedEventStash<RtEvent, N>,
    rt_to_ctrl_tx: &mut rtrb::Producer<RtEvent>,
    ctrl_efd: &EventFd,
    (rate, quantum): (u32, u32),
    sink: SinkChoice,
) -> bool {
    if !reopen_gate.rt_try_begin_reopen() {
        return false;
    }
    let old_alsa = take_counted(alsa_opt, |a| a.xruns(), shared, handle_xruns);
    let old_fds = std::mem::take(alsa_fds);
    let _ = event_stash.push_or_stash(
        RtEvent::NeedReopen {
            target_rate: rate,
            target_quantum: quantum,
            target_sink: sink,
            old_alsa,
            old_fds,
            device_failed: true,
        },
        rt_to_ctrl_tx,
        ctrl_efd,
    );
    shared.reopens.fetch_add(1, Ordering::Relaxed);
    shared.alsa_delay_frames.store(0, Ordering::Relaxed);
    shared.set_state(EngineState::Switching);
    true
}

fn vs_period_frames(ratio: RateRatio, period_out: u32) -> u32 {
    let period_out = period_out.max(1);
    if ratio.is_integer() {
        (period_out / ratio.up.max(1)).max(1)
    } else {
        (u64::from(period_out) * u64::from(ratio.down)).div_ceil(u64::from(ratio.up)) as u32 + 1
    }
}

/// The PipeWire graph as the varispeed steps see it
/// ([`crate::varispeed_loop::VsGraph`]).
struct RtGraph<'a> {
    clock_state: &'a mut DriverClockState,
    shared: &'a SharedEngineState,
    raw_stream: SendRawStream,
    data_efd: &'a EventFd,
    /// The other sink's ring, drained while priming.
    other: Option<&'a mut rtrb::Consumer<f32>>,
    channels: usize,
    /// The overload guard, run at each [`VsGraph::checkpoint`] with the
    /// chain generation and the varispeed hold's sequence that run
    /// (`(generation, hold_seq)`, read when the graph is built).
    guard: &'a mut crate::rt_overload::RtLoopGuard,
    mark: (u64, u64),
}

impl VsGraph for RtGraph<'_> {
    fn now_ns(&self) -> u64 {
        mono_now_ns()
    }

    fn completed(&self) -> u64 {
        self.shared.cycle_counter(false).load(Ordering::Acquire)
    }

    fn trigger(&mut self, rate: u32, quantum: u64, delay: i64, speed: f64) {
        self.clock_state.advance_and_trigger_at(
            &self.shared.io,
            self.raw_stream.0.as_ptr(),
            rate,
            quantum,
            delay,
            speed,
        );
    }

    fn wait_until(&mut self, until_ns: u64) {
        let wait_ns = until_ns.saturating_sub(mono_now_ns()).max(50_000);
        let ts = libc::timespec {
            tv_sec: (wait_ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (wait_ns % 1_000_000_000) as libc::c_long,
        };
        let mut pfd = [libc::pollfd {
            fd: self.data_efd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        unsafe {
            libc::ppoll(pfd.as_mut_ptr(), 1, &ts, std::ptr::null());
        }
        self.data_efd.drain();
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    fn checkpoint(&mut self) {
        self.guard
            .on_iteration(&self.shared.rt, self.mark.0, self.mark.1);
    }

    fn drain_other(&mut self) {
        if let Some(other) = self.other.as_mut() {
            let n = drain_ring(other) / self.channels.max(1);
            self.shared
                .direct_drained_unused
                .fetch_add(n as u64, Ordering::Relaxed);
        }
    }
}

/// The graph for the tail drain: the source has ended, so no cycle is
/// triggered; only the clock is read.
struct NoCycles;

impl VsGraph for NoCycles {
    fn now_ns(&self) -> u64 {
        mono_now_ns()
    }

    fn completed(&self) -> u64 {
        0
    }

    fn trigger(&mut self, _rate: u32, _quantum: u64, _delay: i64, _speed: f64) {}

    fn wait_until(&mut self, _until_ns: u64) {}
}

/// Read one buffer from a sink stream into its ring. Returns nothing; the
/// caller bumps the per-sink cycle marker and wakes the ALSA RT thread.
fn process_sink_buffer(stream: &pw::stream::Stream, data: &mut StreamSideData) {
    let has_links = if data.is_direct {
        data.shared.direct_has_links.load(Ordering::Relaxed)
    } else {
        data.shared.dsp_has_links.load(Ordering::Relaxed)
    };
    let Some(mut buffer) = stream.dequeue_buffer() else {
        if has_links {
            data.shared.note_short_buffer(data.is_direct);
        }
        return;
    };
    let datas = buffer.datas_mut();
    let Some(first) = datas.first_mut() else {
        if has_links {
            data.shared.note_short_buffer(data.is_direct);
        }
        return;
    };
    let chunk = first.chunk();
    let offset = chunk.offset() as usize;
    let size = chunk.size() as usize;
    let Some(bytes) = first.data() else {
        if has_links {
            data.shared.note_short_buffer(data.is_direct);
        }
        return;
    };
    let end = offset.saturating_add(size).min(bytes.len());
    let start = offset.min(end);
    let valid = &bytes[start..end];

    let ch = data.channels.max(1);
    let frames = (valid.len() / 4) / ch;
    if frames == 0 {
        if has_links {
            data.shared.note_short_buffer(data.is_direct);
        }
        return;
    }
    // Varispeed counts the cycles that deliver less than a quantum (the
    // watermark refills from the ring either way).
    if !data.is_direct
        && data.shared.varispeed.is_active()
        && frames < data.shared.quantum.load(Ordering::Relaxed) as usize
    {
        data.shared.varispeed.note_short_cycle();
    }
    let samples_needed = frames * ch;
    let payload = &valid[..samples_needed * 4];
    if has_links && payload.iter().all(|&b| b == 0) {
        data.shared.note_zero_buffer(data.is_direct);
    }
    let written = push_f32_le_bytes(&mut data.prod, payload);
    data.shared
        .note_pushed_frames(data.is_direct, (written / ch) as u64);
    if written < samples_needed {
        data.shared
            .note_dropped_frames(data.is_direct, ((samples_needed - written) / ch) as u64);
    }
}

#[allow(clippy::too_many_arguments)]
fn drain_alsa_tail<P: EngineAudioProcessor>(
    alsa: &mut AlsaOutput,
    active_sink: SinkChoice,
    dsp_cons: &mut rtrb::Consumer<f32>,
    direct_cons: &mut rtrb::Consumer<f32>,
    processor: &mut P,
    fade_ramp: &mut FadeRamp,
    dsp_in_buf: &mut [f32],
    direct_in_buf: &mut [f32],
    out_buf: &mut [i32],
    params: NegotiatedParams,
    channels: usize,
    ratio: RateRatio,
    graph_rate: u32,
    planner: &mut PeriodPlanner,
    carry: &mut OutCarry,
    vs: &mut VarispeedDriver,
    stop: &AtomicBool,
    shared: &SharedEngineState,
    rt_guard: &mut crate::rt_overload::RtLoopGuard,
) {
    let period_out = params.period_frames.max(1);

    // Varispeed: the stage's whole tail is pulled as padding once the ring
    // is empty (`VsDrain`). No cycles are triggered here: the source has
    // ended. The deadline covers what is queued: the ring at the applied
    // speed, the tail, and the device's own delay.
    let vs_tail = if vs.is_active() {
        processor
            .varispeed(active_sink, ratio, graph_rate, channels)
            .map(|port| (port.tail_frames(), port.drain_speed()))
    } else {
        None
    };
    let queued = match active_sink {
        SinkChoice::Direct => direct_cons.slots(),
        _ => dsp_cons.slots(),
    } / channels.max(1);
    let device_delay = alsa.delay().unwrap_or(0);
    let (tail, speed) = vs_tail.unwrap_or((0, 1.0));
    // The alignment line (multi-device) holds source ahead of the stage or
    // the chain: zeros push it out too, at the drain speed.
    let align_tail =
        (processor.alignment_max_frames(active_sink) as f64 / speed.max(0.5)).ceil() as usize;
    let mut align_left = align_tail;
    let deadline = Instant::now()
        + Duration::from_nanos(vsl::drain_deadline_ns(
            queued,
            speed,
            tail + align_tail,
            device_delay,
            ratio,
            graph_rate,
            period_out,
        ));
    let mut vs_drain = VsDrain::default();
    let mut no_delay = VsDelay::new(ratio, graph_rate);
    loop {
        // The drain can write for a while (the device has room) without
        // returning to the RT loop: the overload guard runs here too.
        rt_guard.on_iteration(&shared.rt, processor.running_generation(), vs.hold_seq());
        // The carried frames go ahead of the tail (with or without one),
        // never after it.
        match write_carry_ahead(carry, alsa, channels, shared) {
            CarryNext::Block => {}
            CarryNext::Wait if Instant::now() < deadline && !stop.load(Ordering::Acquire) => {
                let _ = alsa.wait_for_period(40);
                continue;
            }
            CarryNext::Wait | CarryNext::Restart => break,
        }
        let rem = match active_sink {
            SinkChoice::Direct => direct_cons.slots(),
            _ => dsp_cons.slots(),
        };
        let vs_tail = if vs.is_active() {
            processor
                .varispeed(active_sink, ratio, graph_rate, channels)
                .map(|port| port.tail_frames())
        } else {
            None
        };
        match vs_tail {
            Some(tail) if !vs_drain.more(rem / channels.max(1), tail + align_tail) => break,
            Some(_) => {}
            None if rem == 0 && align_left == 0 => break,
            None => {}
        }
        if Instant::now() >= deadline || stop.load(Ordering::Acquire) {
            break;
        }
        let Ok(avail) = alsa.avail() else { break };
        if alsa.needs_restart() {
            break;
        }
        let (chunk_in, chunk_out) = planner.plan(ratio, period_out);
        let in_samples_len = chunk_in * channels;
        let out_samples_len = chunk_out * channels;
        if avail < chunk_out as u32 {
            let _ = alsa.wait_for_period(40);
            continue;
        }
        let vs_ran = if vs_tail.is_some() {
            let run = vsl::run_period(
                vs,
                &mut NoCycles,
                processor,
                PeriodBufs {
                    ring: dsp_cons,
                    dsp_in: dsp_in_buf,
                    direct_in: direct_in_buf,
                    out: &mut out_buf[..out_samples_len],
                },
                &shared.varispeed_cmd,
                PeriodPlan {
                    sink: active_sink,
                    ratio,
                    graph_rate,
                    channels,
                    pos: planner.pos,
                    n_in: chunk_in,
                    n_out: chunk_out,
                    kind: PullKind::Drain,
                    prime_wait_ns: None,
                    poll: false,
                    quantum: 0,
                },
                &mut no_delay,
            );
            vs_drain.pulled(chunk_in);
            run.is_some()
        } else {
            false
        };
        if !vs_ran {
            if rem == 0 {
                align_left = align_left.saturating_sub(chunk_in);
            }
            match active_sink {
                SinkChoice::Direct => {
                    pop_f32_zero_pad(direct_cons, &mut direct_in_buf[..in_samples_len]);
                }
                _ => {
                    pop_f32_zero_pad(dsp_cons, &mut dsp_in_buf[..in_samples_len]);
                }
            }
            processor.process_block(
                active_sink,
                &dsp_in_buf[..in_samples_len],
                &direct_in_buf[..in_samples_len],
                &mut out_buf[..out_samples_len],
                channels,
                ratio,
                graph_rate,
                planner.pos,
            );
        }
        planner.commit(chunk_in, chunk_out, period_out);
        fade_ramp.apply_interleaved(&mut out_buf[..out_samples_len], channels);
        if alsa
            .write_i32_all_until(&out_buf[..out_samples_len], || stop.load(Ordering::Acquire))
            .is_err()
            || alsa.needs_restart()
        {
            break;
        }
    }

    let mut padded_periods = 0u32;
    let max_wait_periods = (params.buffer_frames / period_out).max(2) + 2;
    // accounted below so a mid-stream silence pad is visible in telemetry
    while padded_periods < max_wait_periods
        && Instant::now() < deadline
        && !stop.load(Ordering::Acquire)
    {
        rt_guard.on_iteration(&shared.rt, processor.running_generation(), vs.hold_seq());
        let Ok((avail, delay)) = alsa.avail_and_delay() else {
            break;
        };
        if alsa.needs_restart() || delay <= 0 {
            break;
        }
        if avail >= period_out {
            let _ = alsa.write_silence(period_out);
            padded_periods += 1;
        } else {
            let _ = alsa.wait_for_period(40);
        }
    }
    shared
        .drain_silence_periods
        .fetch_add(padded_periods as u64, Ordering::Relaxed);
}

#[inline]
fn push_f32_le_bytes(prod: &mut rtrb::Producer<f32>, bytes: &[u8]) -> usize {
    let total_samples = bytes.len() / 4;
    let writable = prod.slots().min(total_samples);
    if writable == 0 {
        return 0;
    }
    if let Ok(mut chunk) = prod.write_chunk_uninit(writable) {
        let (first, second) = chunk.as_mut_slices();
        let mut idx = 0;
        for slot in first.iter_mut() {
            let b = &bytes[idx * 4..idx * 4 + 4];
            slot.write(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            idx += 1;
        }
        for slot in second.iter_mut() {
            let b = &bytes[idx * 4..idx * 4 + 4];
            slot.write(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            idx += 1;
        }
        // Safety: every slot in `first` and `second` has been initialized.
        unsafe { chunk.commit_all() };
        idx
    } else {
        0
    }
}

#[inline]
fn pop_f32_zero_pad(cons: &mut rtrb::Consumer<f32>, dst: &mut [f32]) -> usize {
    let avail = cons.slots().min(dst.len());
    if avail > 0
        && let Ok(chunk) = cons.read_chunk(avail)
    {
        let (first, second) = chunk.as_slices();
        let n1 = first.len();
        dst[..n1].copy_from_slice(first);
        if !second.is_empty() {
            dst[n1..avail].copy_from_slice(second);
        }
        chunk.commit_all();
    }
    if avail < dst.len() {
        dst[avail..].fill(0.0);
    }
    avail
}

#[inline]
/// The active sink's input for the next block: `(popped samples, samples
/// discarded)`. While the graph runs at another rate than the device was
/// opened for (a reopen for the new rate is fading out), what the ring
/// delivers belongs to the new rate: through the old chain it would play at
/// the wrong speed. It is discarded and the block gets silence, so the fade
/// only carries what the chain already holds for the old open.
fn take_block_input(
    cons: &mut rtrb::Consumer<f32>,
    dst: &mut [f32],
    graph_rate: u32,
    opened_rate: u32,
) -> (usize, usize) {
    if graph_rate != opened_rate {
        dst.fill(0.0);
        return (0, drain_ring(cons));
    }
    (pop_f32_zero_pad(cons, dst), 0)
}

fn drain_ring(cons: &mut rtrb::Consumer<f32>) -> usize {
    let avail = cons.slots();
    if avail > 0
        && let Ok(chunk) = cons.read_chunk(avail)
    {
        chunk.commit_all();
    }
    avail
}

/// Steady-state graph-cycle pacing for the DAC-driven loop.
///
/// Each ALSA period consumes `period_in` graph frames; PipeWire produces one
/// quantum per cycle. Normally the two are equal (see [`scaled_quantum`]) and
/// the loop fires one cycle per period. When they differ (PipeWire runs
/// another quantum: `clock.force-quantum`, a client asking for lower latency,
/// a non-default `clock.power-of-two-quantum`) a period owes several cycles,
/// or a fraction of one. Firing several back to back starves the source
/// clients (they have not finished the previous cycle, so PipeWire fills the
/// next with silence), so owed cycles are spread at least half a quantum
/// apart and the ring keeps a cushion across graph cycles instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CyclePacer {
    credit: u64,
    owed: u32,
    next_ns: u64,
}

impl CyclePacer {
    /// Cap on cycles owed (a stalled graph must not cause a burst later).
    const MAX_OWED: u32 = 8;

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Account `frames` graph frames written to the DAC.
    pub(crate) fn consumed(&mut self, frames: u64, quantum: u64) {
        let q = quantum.max(1);
        self.credit += frames;
        let n = self.credit / q;
        self.credit -= n * q;
        self.owed = (self.owed + n as u32).min(Self::MAX_OWED);
    }

    /// Owe `cycles` more (a handover from the varispeed schedule tops the
    /// ring up to a period and a quantum).
    pub(crate) fn owe(&mut self, cycles: u32) {
        self.owed = (self.owed + cycles).min(Self::MAX_OWED);
    }

    /// Minimum spacing between two cycles, ns.
    fn spacing_ns(quantum: u64, rate: u32) -> u64 {
        quantum.saturating_mul(500_000_000) / u64::from(rate.max(1))
    }

    /// Whether a cycle should be fired now; if so, books it.
    pub(crate) fn take_due(&mut self, now_ns: u64, quantum: u64, rate: u32) -> bool {
        if self.owed == 0 || now_ns < self.next_ns {
            return false;
        }
        self.owed -= 1;
        self.next_ns = now_ns.saturating_add(Self::spacing_ns(quantum, rate));
        true
    }

    /// When the next owed cycle is due (`None` when nothing is owed).
    pub(crate) fn next_due_ns(&self) -> Option<u64> {
        (self.owed > 0).then_some(self.next_ns)
    }
}

/// Graph frames the ring must hold before the DAC starts: one period, plus
/// two quanta when the period and the quantum differ, because then the
/// cycles feeding a period no longer arrive right before it is written
/// (capped so the ring never has to hold more than it can).
pub(crate) fn start_cushion_frames(period_in: u32, quantum: u32, ring_frames: usize) -> usize {
    let want = if period_in == quantum {
        period_in as usize
    } else {
        period_in as usize + 2 * quantum as usize
    };
    want.min(
        ring_frames
            .saturating_sub(quantum as usize)
            .max(period_in as usize),
    )
}

/// Earliest deadline for the next priming graph cycle.
///
/// A cycle must start at least one nominal period after both the previous
/// deadline and the completion of the previous cycle, so a slow or jittery
/// cycle can never be followed immediately by a catch-up burst of cycles that
/// outruns follower clients' converters (which produced an all-zero PipeWire
/// block and broke bit-perfect playback). `completion_ns` is `None` when no
/// cycle was triggered for the previous slot. The total wait stays bounded
/// because every cycle's completion wait has its own timeout.
fn next_prime_deadline_ns(
    previous_deadline_ns: u64,
    completion_ns: Option<u64>,
    period_ns: u64,
) -> u64 {
    let after_previous = previous_deadline_ns.saturating_add(period_ns);
    match completion_ns {
        Some(completed_ns) => after_previous.max(completed_ns.saturating_add(period_ns)),
        None => after_previous,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn turning_dsp_off_reaches_the_arbiter_although_the_published_rates_stay() {
        use crate::lifecycle::{BypassLeg, DesiredDspConfig, RatePairMask};
        use crate::rate_arbiter::{RateArbiter, RateClient, RateInputs, Verdict};
        use audivane_protocol::RateTarget;
        use audivane_protocol::ipc::{FormatCapability, RateCapability};
        // 44.1 kHz through the DSP sink, upsampling off, a 48 kHz-only DAC.
        let caps = DeviceCapabilities {
            device: "hw:CARD=USB,DEV=0".into(),
            channels: 2,
            channels_min: 2,
            channels_max: 2,
            rates: vec![RateCapability {
                rate: 48_000,
                formats: vec![FormatCapability {
                    format: SampleFormat::S32,
                    sbits: Some(32),
                }],
            }],
            rate_min: 48_000,
            rate_max: 48_000,
            formats: vec![SampleFormat::S32],
        };
        let formats = [SampleFormat::S32];
        let configured = RatePolicy::default();
        let desired = |dsp_enabled| DesiredDspConfig {
            dsp_enabled,
            rate_target: RateTarget::Off,
            bypass: BypassLeg {
                engine: true,
                buildable: RatePairMask::ALL,
                allowed: BypassLeg::allowed_bits(&configured.allowed_rates),
            },
            ..DesiredDspConfig::default()
        };
        let policies = |dsp_enabled| {
            let d = desired(dsp_enabled);
            let eff = effective_rate_policy(&configured, &caps, d, &formats, ALLOWED_RATES)
                .unwrap()
                .policy;
            crate::rate_caps::rate_policies(eff, Some(&caps), &formats, d)
        };
        let (on, off) = (policies(true), policies(false));
        // The published union is the same (Direct already converted 44.1
        // kHz); only the DSP sink's own policy gains it.
        assert_eq!(on.published, off.published);
        assert!(!on.dsp.allowed_rates.contains(&44_100));
        assert!(off.dsp.allowed_rates.contains(&44_100));
        let sent = SentRatePolicies {
            published: on.published.clone(),
            sinks: Some(on.clone()),
        };
        assert!(!sent.due(&on.published, &Some(on.clone())));
        assert!(sent.due(&off.published, &Some(off.clone())));
        // And the arbiter, told, moves the graph to the source rate (the
        // engine converts it then; PipeWire no longer does).
        let clients = [RateClient {
            id: 1,
            name: "app".into(),
            native_rate: 44_100,
            since: 0,
        }];
        let decide = |p: &crate::rate_caps::RatePolicies| {
            RateArbiter::default().decide(
                &RateInputs {
                    policy: &p.published,
                    current: 48_000,
                    direct_active: false,
                    direct: &[],
                    dsp: &clients,
                    direct_policy: Some(&p.direct),
                    dsp_policy: Some(&p.dsp),
                    direct_native: Some(&p.direct_native),
                    dsp_native: Some(&p.dsp_native),
                },
                Instant::now(),
                false,
            )
        };
        assert!(matches!(decide(&on), Verdict::Keep(c) if c.rate == 48_000));
        assert!(matches!(decide(&off), Verdict::Switch(c) if c.rate == 44_100));
    }

    /// A device buffer of `capacity` frames with auto-start off: before
    /// `start` nothing frees room; afterwards each `play` frees frames, and
    /// a write waits while the device plays.
    struct FakePcm {
        capacity: usize,
        filled: usize,
        channels: usize,
        running: bool,
        received: Vec<i32>,
        timeouts: usize,
        /// The next write underruns: recovery prepares the device and
        /// the write returns 0.
        underrun_next: bool,
        restart: bool,
    }

    impl FakePcm {
        fn play(&mut self, frames: usize) {
            self.filled = self.filled.saturating_sub(frames);
        }
    }

    impl PcmSink for FakePcm {
        fn room(&mut self) -> Option<usize> {
            Some(self.capacity - self.filled)
        }

        fn needs_restart(&self) -> bool {
            self.restart
        }

        fn write_silence(&mut self, frames: usize) -> Option<usize> {
            self.write_all(&vec![0; frames * self.channels], &|| false)
        }

        fn write_all(&mut self, samples: &[i32], _stop: &dyn Fn() -> bool) -> Option<usize> {
            if self.underrun_next {
                self.underrun_next = false;
                self.restart = true;
                self.running = false;
                self.filled = 0;
                return Some(0);
            }
            let frames = samples.len() / self.channels;
            if self.running {
                // Waiting while the device plays frees what is missing.
                self.play((self.filled + frames).saturating_sub(self.capacity));
            }
            let n = frames.min(self.capacity - self.filled);
            self.received
                .extend_from_slice(&samples[..n * self.channels]);
            self.filled += n;
            if n < frames {
                // `write_i32_all_until` waits for room that never comes
                // (about 200 ms) and gives up: the rest is lost.
                self.timeouts += 1;
                return None;
            }
            Some(n)
        }
    }

    #[test]
    fn input_across_a_rate_change_never_reaches_the_old_chain() {
        // Playing 44.1 kHz at 160/147; the graph switched to 48 kHz and the
        // reopen fades out. The ring now holds 48 kHz samples: none of them
        // may go through the 44.1 kHz open (a 1 kHz tone would play at
        // 918.75 Hz). They are discarded and the block is silence.
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(64);
        for i in 0..32 {
            prod.push(1.0 + i as f32).unwrap();
        }
        let mut dst = [7.0_f32; 16];
        assert_eq!(
            take_block_input(&mut cons, &mut dst, 48_000, 44_100),
            (0, 32)
        );
        assert!(dst.iter().all(|&v| v == 0.0));
        assert_eq!(cons.slots(), 0);
        // At the opened rate the samples play.
        for i in 0..8 {
            prod.push(1.0 + i as f32).unwrap();
        }
        assert_eq!(
            take_block_input(&mut cons, &mut dst, 44_100, 44_100),
            (8, 0)
        );
        assert_eq!(dst[..8], [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert!(dst[8..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn a_failed_open_of_a_missing_device_leaves_no_earlier_probe_in_the_status() {
        // The status had capabilities of an earlier probe of the device;
        // the open failed and the device does not answer the probe after
        // it: no capabilities, and no probe counted (the overload fallback
        // waits instead of blaming its step).
        let device = "hw:CARD=AudivaneNoSuchCard,DEV=0";
        let config = EngineDriverConfig {
            device: device.into(),
            ..EngineDriverConfig::default()
        };
        let shared = Arc::new(SharedEngineState::new(&config));
        shared.set_device_caps(Some(DeviceCapabilities {
            device: device.into(),
            ..DeviceCapabilities::default()
        }));
        let mut backend = RealDeviceBackend {
            formats: config.formats.clone(),
            channels: 2,
            buffer_periods: 3,
            ctrl_efd: Arc::new(EventFd::new().unwrap()),
            shared: Arc::clone(&shared),
            config: config.clone(),
            probed_device: Some(device.into()),
            caps: None,
            probe_handle: None,
            effective: None,
            moves: None,
        };
        backend.reprobe_after_failure(device);
        assert!(shared.device_caps.lock().unwrap().is_none());
        assert_eq!(shared.probes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_paused_direct_client_never_silences_a_playing_dsp_one() {
        // A player paused on Direct but still linked, a second one playing
        // on the DSP sink; the driver group runs both nodes. The DSP sink
        // drives the DAC (and a failed open recovers onto it).
        let shared = SharedEngineState::new(&EngineDriverConfig::default());
        let set = |flag: &AtomicBool, on| flag.store(on, Ordering::Release);
        set(&shared.direct_streaming, true);
        set(&shared.direct_has_links, true);
        set(&shared.dsp_streaming, true);
        set(&shared.dsp_has_links, true);
        set(&shared.dsp_client_running, true);
        assert_eq!(shared.pick_sink(), Some(SinkChoice::Dsp));
        // The Direct player resumes: Direct takes the DAC.
        set(&shared.direct_client_running, true);
        assert_eq!(shared.pick_sink(), Some(SinkChoice::Direct));
        // Nothing streams.
        for f in [&shared.direct_streaming, &shared.dsp_streaming] {
            set(f, false);
        }
        assert_eq!(shared.pick_sink(), None);
    }

    #[test]
    fn a_paused_client_resuming_starts_a_session() {
        // Direct drives both nodes; a DSP client is linked but paused. Its
        // link is no stream: when it really resumes, a session starts.
        let shared = SharedEngineState::new(&EngineDriverConfig::default());
        let mut sessions = PlaybackSessions::default();
        let set = |flag: &AtomicBool, on| flag.store(on, Ordering::Release);
        set(&shared.direct_streaming, true);
        set(&shared.direct_client_running, true);
        set(&shared.direct_has_links, true);
        set(&shared.dsp_streaming, true);
        set(&shared.dsp_has_links, true);
        let (dsp, direct) = shared.session_streams();
        assert!(!dsp, "a paused client is no stream");
        let s = sessions.observe(dsp, direct);
        set(&shared.dsp_client_running, true);
        let (dsp, direct) = shared.session_streams();
        assert_eq!(sessions.observe(dsp, direct), s + 1);
        // The Direct client pauses and resumes, still linked: another one.
        set(&shared.direct_client_running, false);
        let (dsp, direct) = shared.session_streams();
        assert_eq!(sessions.observe(dsp, direct), s + 1);
        set(&shared.direct_client_running, true);
        let (dsp, direct) = shared.session_streams();
        assert_eq!(sessions.observe(dsp, direct), s + 2);
    }

    #[test]
    fn a_dsp_stream_starting_while_direct_drives_starts_a_session() {
        // Direct plays, so the driver group runs and the DSP node's
        // streaming flag is set without any client of its own. A stream
        // that then starts on the DSP sink is a new playback session.
        let shared = SharedEngineState::new(&EngineDriverConfig::default());
        let mut sessions = PlaybackSessions::default();
        let set = |flag: &AtomicBool, on| flag.store(on, Ordering::Release);
        set(&shared.direct_streaming, true);
        set(&shared.direct_client_running, true);
        set(&shared.dsp_streaming, true);
        let (dsp, direct) = shared.session_streams();
        let s = sessions.observe(dsp, direct);
        assert!(!dsp, "the group's flag is not a DSP stream");
        set(&shared.dsp_has_links, true);
        set(&shared.dsp_client_running, true);
        let (dsp, direct) = shared.session_streams();
        assert_eq!(sessions.observe(dsp, direct), s + 1);
        // It stops (its node may keep running with the group): a later
        // start is another session.
        set(&shared.dsp_has_links, false);
        set(&shared.dsp_client_running, false);
        let (dsp, direct) = shared.session_streams();
        assert_eq!(sessions.observe(dsp, direct), s + 1);
        set(&shared.dsp_client_running, true);
        let (dsp, direct) = shared.session_streams();
        assert_eq!(sessions.observe(dsp, direct), s + 2);
    }

    #[test]
    fn rational_prefill_fits_the_buffer_and_loses_no_frame() {
        // 44.1 -> 96 kHz, 512-frame periods, a 1536-frame buffer: the three
        // prefill periods plan 512 + 512 + 513 frames.
        let ratio = RateRatio::from_rates(44_100, 96_000);
        let (period_out, ch) = (512u32, 2usize);
        let mut pcm = FakePcm {
            capacity: 1536,
            filled: 0,
            channels: ch,
            running: false,
            received: Vec::new(),
            timeouts: 0,
            underrun_next: false,
            restart: false,
        };
        let mut carry = OutCarry::with_capacity(ratio.max_frames_out(4096) * ch);
        let mut planner = PeriodPlanner::default();
        let mut sent = Vec::new();
        let mut frame = 0i32;
        let mut period = |planner: &mut PeriodPlanner, sent: &mut Vec<i32>| {
            let (n_in, n_out) = planner.plan(ratio, period_out);
            let out: Vec<i32> = (0..n_out)
                .flat_map(|_| {
                    frame += 1;
                    [frame, -frame]
                })
                .collect();
            planner.commit(n_in, n_out, period_out);
            sent.extend_from_slice(&out);
            out
        };
        let stop = || false;
        let mut planned = 0;
        for _ in 0..3 {
            let out = period(&mut planner, &mut sent);
            planned += out.len() / ch;
            carry.prime_write(&mut pcm, &out, ch, &stop);
        }
        assert_eq!(planned, 1537);
        assert_eq!(pcm.timeouts, 0, "a prefill write waited for room");
        assert_eq!(pcm.filled, 1536);
        // Started: each period played frees room; the carried frame goes
        // out first.
        pcm.running = true;
        for _ in 0..4 {
            pcm.play(period_out as usize);
            let out = period(&mut planner, &mut sent);
            assert_eq!(carry.write_ahead(&mut pcm, ch, &stop).1, CarryNext::Block);
            pcm.write_all(&out, &stop).unwrap();
        }
        assert_eq!(pcm.timeouts, 0);
        assert!(carry.is_empty());
        assert_eq!(pcm.received, sent, "frames lost or reordered");
    }

    /// Prefill `periods` periods of `period_out` planned frames at `ratio`
    /// into a stopped device of `buffer` frames through a carry of the
    /// RT thread's size, then run it: `(frames dropped, whether the
    /// prefill left frames carried, whether every frame played in order)`.
    fn prefill_and_run(ratio: RateRatio, period_out: u32, buffer: usize) -> (usize, bool, bool) {
        prefill_and_run_after_silence(ratio, period_out, buffer, 0)
    }

    /// [`prefill_and_run`] whose first `silent` primes find no source data
    /// and write silence, as the RT thread does ([`prime_silence`]).
    fn prefill_and_run_after_silence(
        ratio: RateRatio,
        period_out: u32,
        buffer: usize,
        silent: usize,
    ) -> (usize, bool, bool) {
        let ch = 2usize;
        let mut pcm = FakePcm {
            capacity: buffer,
            filled: 0,
            channels: ch,
            running: false,
            received: Vec::new(),
            timeouts: 0,
            underrun_next: false,
            restart: false,
        };
        let mut carry = OutCarry::with_capacity(CARRY_MAX_FRAMES * ch);
        let mut planner = PeriodPlanner::default();
        let mut sent = Vec::new();
        let mut frame = 0i32;
        let mut period = |planner: &mut PeriodPlanner, sent: &mut Vec<i32>| {
            let (n_in, n_out) = planner.plan(ratio, period_out);
            let out: Vec<i32> = (0..n_out)
                .flat_map(|_| {
                    frame += 1;
                    [frame, -frame]
                })
                .collect();
            planner.commit(n_in, n_out, period_out);
            sent.extend_from_slice(&out);
            out
        };
        let stop = || false;
        let mut dropped = 0;
        for p in 0..prefill_periods(buffer as u32, period_out) as usize {
            if p < silent {
                let (_, prime_out) = planner.plan(ratio, period_out);
                assert!(carry.is_empty());
                let n = prime_silence(&mut pcm, &mut planner, prime_out, period_out).unwrap();
                sent.extend(std::iter::repeat_n(0, n * ch));
                continue;
            }
            let out = period(&mut planner, &mut sent);
            dropped += carry.prime_write(&mut pcm, &out, ch, &stop).1;
        }
        assert_eq!(pcm.timeouts, 0, "a prefill write waited for room");
        assert!(pcm.filled <= buffer);
        let carried = !carry.is_empty();
        pcm.running = true;
        for _ in 0..4 {
            pcm.play(period_out as usize);
            let out = period(&mut planner, &mut sent);
            assert_eq!(carry.write_ahead(&mut pcm, ch, &stop).1, CarryNext::Block);
            pcm.write_all(&out, &stop).unwrap();
        }
        (dropped, carried, pcm.received == sent)
    }

    #[test]
    fn alsa_lib_errors_are_logged_once_from_the_control_thread() {
        let mut logged = 2;
        assert_eq!(log_new_alsa_lib_errors(2, &mut logged), 0);
        assert_eq!(log_new_alsa_lib_errors(5, &mut logged), 3);
        assert_eq!(log_new_alsa_lib_errors(5, &mut logged), 0);
    }

    #[test]
    fn a_closed_handle_s_last_xruns_are_counted() {
        // An underrun in the tail drain, then the handle closes before
        // another steady iteration: the close counts it.
        let shared = SharedEngineState::new(&EngineDriverConfig::default());
        let mut seen = 0;
        let mut logged = 0;
        note_xruns(&shared, 1, &mut seen);
        let mut handle = Some(3u64);
        assert_eq!(
            take_counted(&mut handle, |h| *h, &shared, &mut seen),
            Some(3)
        );
        assert_eq!(handle, None);
        assert_eq!(log_new_xruns(&shared, &mut logged), 3);
        assert_eq!(take_counted(&mut handle, |h| *h, &shared, &mut seen), None);
        assert_eq!(log_new_xruns(&shared, &mut logged), 0);
    }

    #[test]
    fn xruns_are_counted_on_the_rt_thread_and_logged_on_the_control_thread() {
        let shared = SharedEngineState::new(&EngineDriverConfig::default());
        let mut seen = 0;
        let mut logged = 0;
        assert_eq!(log_new_xruns(&shared, &mut logged), 0);
        note_xruns(&shared, 1, &mut seen);
        note_xruns(&shared, 1, &mut seen);
        note_xruns(&shared, 3, &mut seen);
        assert_eq!(shared.xruns.load(Ordering::Relaxed), 3);
        assert_eq!(log_new_xruns(&shared, &mut logged), 3);
        assert_eq!(log_new_xruns(&shared, &mut logged), 0, "logged once");
        // A reopen: the new handle counts from 0; the total keeps going.
        seen = 0;
        note_xruns(&shared, 0, &mut seen);
        note_xruns(&shared, 2, &mut seen);
        assert_eq!(shared.xruns.load(Ordering::Relaxed), 2);
        assert_eq!(shared.xrun_events.load(Ordering::Relaxed), 5);
        assert_eq!(log_new_xruns(&shared, &mut logged), 2);
    }

    #[test]
    fn a_prefill_never_writes_more_periods_than_the_buffer_holds() {
        // x64: a 65,536-frame period in a 98,304-frame buffer (1.5
        // periods, as a byte-capped USB buffer gives at 3.072 MHz). Two
        // prefill periods lost 32,704 frames to the carry; one fits.
        let x64 = RateRatio::integer(64);
        assert_eq!(prefill_periods(98_304, 65_536), 1);
        assert_eq!(prefill_and_run(x64, 65_536, 98_304), (0, false, true));
        // The same short buffer at any rate (44.1 -> 96 kHz, 512-frame
        // periods, a 928-frame buffer), and a rational x64-range pair.
        let r96 = RateRatio::from_rates(44_100, 96_000);
        assert_eq!(prefill_and_run(r96, 512, 928).0, 0);
        let r2822 = RateRatio::from_rates(48_000, 2_822_400);
        assert_eq!(prefill_and_run(r2822, 60_211, 98_304).0, 0);
        // Buffers of whole periods keep their prefill depth: as many
        // periods as fit, the rational overshoot carried, never dropped.
        assert_eq!(prefill_periods(3 * 512, 512), 3);
        assert_eq!(prefill_periods(2 * 65_536, 65_536), 2);
        assert_eq!(prefill_periods(65_536, 65_536), 1);
        assert_eq!(prefill_and_run(r96, 512, 1536).0, 0);
        assert_eq!(prefill_and_run(r2822, 60_211, 2 * 60_211).0, 0);
    }

    #[test]
    fn silent_primes_are_credited_like_audio() {
        // 48 -> 2822.4 kHz (294/5) with 60,160-frame periods in a
        // 180,480-frame buffer: two silent primes, then audio. Each prime
        // overshoots its period by a few frames; uncredited, the silence's
        // overshoot pushed the audio prime past the buffer and the carry.
        let r = RateRatio::from_rates(48_000, 2_822_400);
        assert_eq!(prefill_periods(180_480, 60_160), 3);
        for silent in 0..=3 {
            let (dropped, _, in_order) = prefill_and_run_after_silence(r, 60_160, 180_480, silent);
            assert_eq!(dropped, 0, "{silent} silent primes");
            assert!(in_order, "{silent} silent primes");
        }
    }

    #[test]
    fn a_short_carry_write_keeps_the_rest() {
        let ch = 2;
        let new_pcm = || FakePcm {
            capacity: 8,
            filled: 8,
            channels: ch,
            running: true,
            received: Vec::new(),
            timeouts: 0,
            underrun_next: false,
            restart: false,
        };
        let stop = || false;
        // Room for one frame: it goes, the other waits, and no block may
        // follow yet.
        let mut pcm = new_pcm();
        let mut carry = OutCarry::with_capacity(16);
        carry.prime_write(&mut pcm, &[1, 1, 2, 2], ch, &stop);
        pcm.filled = 7;
        assert_eq!(carry.write_ahead(&mut pcm, ch, &stop), (1, CarryNext::Wait));
        assert!(!carry.is_empty());
        pcm.filled = 0;
        assert_eq!(
            carry.write_ahead(&mut pcm, ch, &stop),
            (1, CarryNext::Block)
        );
        assert_eq!(pcm.received, [1, 1, 2, 2]);
        // The first carry write underruns (recovery prepares the device,
        // the write returns 0): the frames stay for the prefill that
        // follows, which writes them first.
        let mut pcm = new_pcm();
        let mut carry = OutCarry::with_capacity(16);
        carry.prime_write(&mut pcm, &[1, 1, 2, 2], ch, &stop);
        pcm.filled = 0;
        pcm.underrun_next = true;
        assert_eq!(
            carry.write_ahead(&mut pcm, ch, &stop),
            (0, CarryNext::Restart)
        );
        assert!(!carry.is_empty());
        pcm.restart = false;
        assert_eq!(carry.prime_write(&mut pcm, &[3, 3], ch, &stop), (3, 0));
        assert_eq!(pcm.received, [1, 1, 2, 2, 3, 3]);
    }

    #[test]
    fn the_carry_bound_covers_every_rational_pair() {
        // The prefill carries fewer than ceil(up / down) frames, for any
        // graph rate and any output rate the driver opens: at most 59
        // (48 kHz -> 2.8224 MHz is 294/5).
        let mut worst = 0;
        for &from in audivane_protocol::ALLOWED_RATES {
            for to in crate::lifecycle::MASK_OUTPUT_RATES {
                let r = RateRatio::from_rates(from, to);
                let limit = crate::lifecycle::MAX_RATIONAL_TERM;
                // Pairs past the term limit (44.1 kHz -> 3.072 MHz is
                // 10240/147) are never opened.
                if !r.is_integer() && r.up <= limit && r.down <= limit {
                    let bound = (r.up as usize).div_ceil(r.down as usize);
                    assert!(bound <= CARRY_MAX_FRAMES, "{from} -> {to}: {bound}");
                    worst = worst.max(bound);
                    // Silent primes (no source data yet) followed by audio
                    // stay within it too, in whole and short buffers.
                    for buffer in [3 * 4096, 4096 * 3 / 2] {
                        for silent in 0..=3 {
                            let (dropped, _, in_order) =
                                prefill_and_run_after_silence(r, 4096, buffer, silent);
                            assert!(
                                dropped == 0 && in_order,
                                "{from} -> {to}, buffer {buffer}, {silent} silent: {dropped} dropped"
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(worst, 59);
    }

    #[test]
    fn a_reprime_writes_the_carried_frames_first() {
        // An xrun before the first steady write: the device is prepared
        // again (empty) with a frame still carried; it goes out before the
        // new prefill, and silence never jumps ahead of it.
        let ch = 2;
        let mut pcm = FakePcm {
            capacity: 4,
            filled: 0,
            channels: ch,
            running: false,
            received: Vec::new(),
            timeouts: 0,
            underrun_next: false,
            restart: false,
        };
        let mut carry = OutCarry::with_capacity(16);
        let stop = || false;
        assert_eq!(
            carry.prime_write(&mut pcm, &[1, 1, 2, 2, 3, 3, 4, 4, 5, 5], ch, &stop),
            (4, 0)
        );
        assert!(!carry.is_empty());
        pcm.filled = 0;
        pcm.received.clear();
        assert_eq!(
            carry.prime_write(&mut pcm, &[6, 6, 7, 7], ch, &stop),
            (3, 0)
        );
        assert_eq!(pcm.received, [5, 5, 6, 6, 7, 7]);
        assert!(carry.is_empty());
        // Full: what does not fit the carry is dropped and counted.
        pcm.filled = 4;
        let mut small = OutCarry::with_capacity(2);
        assert_eq!(
            small.prime_write(&mut pcm, &[8, 8, 9, 9], ch, &stop),
            (0, 1)
        );
    }
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn a_held_device_counts_as_gone_only_after_it_was_seen() {
        let t0 = Instant::now();
        let mut watch = PresenceWatch::new(t0);
        // A name the watch cannot resolve never takes the device away.
        assert_eq!(watch.look(t0, || false), Some(false));
        assert_eq!(watch.look(t0, || false), None, "one look per poll");
        let t1 = t0 + DEVICE_POLL;
        assert_eq!(watch.look(t1, || true), Some(false));
        // Unplugged between tracks.
        let t2 = t1 + DEVICE_POLL;
        assert_eq!(watch.look(t2, || false), Some(true));
        // A new output: seen again before it can count as gone.
        watch.reset();
        assert_eq!(watch.look(t2 + DEVICE_POLL, || false), Some(false));
    }

    #[test]
    fn a_release_or_reopen_ends_the_wait() {
        let shared = SharedEngineState::new(&EngineDriverConfig::default());
        let device = "hw:CARD=USB,DEV=0";
        shared.device_missing.store(true, Ordering::Release);
        shared.device_gone.store(true, Ordering::Release);
        shared.set_error(waiting_text(device));
        let mut lost = Some(LostDevice::new(false, Instant::now()));
        end_waiting(&shared, &mut lost, device);
        assert!(lost.is_none());
        assert!(!shared.device_missing.load(Ordering::Acquire));
        assert!(!shared.device_gone.load(Ordering::Acquire));
        assert_eq!(shared.effective_error(), None);
        // Another error stays.
        shared.set_error("ALSA open/prepare failed");
        end_waiting(&shared, &mut lost, device);
        assert!(shared.effective_error().is_some());
    }

    #[test]
    fn a_lost_device_is_opened_when_it_appears_and_retried_with_backoff() {
        let t0 = Instant::now();
        let mut lost = LostDevice::new(false, t0);
        // Not due before the next look, and not while it is missing.
        assert!(!lost.due(true, t0));
        let t1 = t0 + DEVICE_POLL;
        assert!(!lost.due(false, t1));
        // It appears: open at once.
        let t2 = t1 + DEVICE_POLL;
        assert!(lost.due(true, t2));
        // The open failed and it is still there: retried after a growing
        // pause, never faster than the first retry.
        assert!(!lost.due(true, t2 + DEVICE_RETRY_FIRST / 2));
        let t3 = t2 + DEVICE_RETRY_FIRST;
        assert!(lost.due(true, t3));
        assert_eq!(lost.backoff, DEVICE_RETRY_FIRST * 2);
        assert!(!lost.due(true, t3 + DEVICE_RETRY_FIRST));
        let mut t = t3 + DEVICE_RETRY_FIRST * 2;
        assert!(lost.due(true, t));
        for _ in 0..10 {
            t += lost.backoff;
            assert!(lost.due(true, t));
        }
        assert_eq!(lost.backoff, DEVICE_RETRY_MAX);
        // Gone again, then back: opened at once, the pause starts over.
        t += lost.backoff;
        assert!(!lost.due(false, t));
        t += DEVICE_POLL;
        assert!(lost.due(true, t));
        assert_eq!(lost.backoff, DEVICE_RETRY_FIRST);
    }

    #[test]
    fn quantum_scales_across_rate_families() {
        assert_eq!(scaled_quantum(1024, 48_000), 1024);
        assert_eq!(scaled_quantum(1024, 96_000), 2048);
        assert_eq!(scaled_quantum(1024, 192_000), 4096);
        assert_eq!(scaled_quantum(1024, 44_100), 512);
        assert_eq!(scaled_quantum(256, 44_100), 128);
    }

    /// The quantum must be the one PipeWire runs, a power of two, at every
    /// rate, or the ALSA period does not match the graph cycle.
    #[test]
    fn quantum_matches_pipewire_at_44k_family_rates() {
        assert_eq!(scaled_quantum(1024, 88_200), 1024);
        assert_eq!(scaled_quantum(1024, 176_400), 2048);
        assert_eq!(scaled_quantum(1024, 352_800), 4096);
        assert_eq!(scaled_quantum(1024, 705_600), 8192);
        assert_eq!(scaled_quantum(1024, 384_000), 8192);
        assert_eq!(scaled_quantum(1024, 768_000), 8192);
        assert_eq!(scaled_quantum(8192, 192_000), 8192);
        assert_eq!(scaled_quantum(1024, 144_000), 2048);
        assert_eq!(scaled_quantum(16, 44_100), 32);
        for q48 in [16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192] {
            for &rate in ALLOWED_RATES {
                let q = scaled_quantum(q48, rate);
                assert!(q.is_power_of_two(), "{q48}@{rate} -> {q}");
                // PipeWire: floor to a power of two of q48 * rate / 48000.
                let raw = (u64::from(q48.max(32)) * u64::from(rate) / 48_000).clamp(32, 8192);
                assert!(
                    u64::from(q) <= raw && raw < 2 * u64::from(q),
                    "{q48}@{rate} -> {q}"
                );
            }
        }
    }

    /// Simulate the steady DAC loop against a graph whose source client
    /// needs `min_gap_ns` between cycles (closer cycles deliver silence, as
    /// PipeWire does for an async client that has not finished). Returns
    /// (graph frames zero-filled by the ring, silent cycles).
    fn simulate_pacing(rate: u32, period_in: u32, quantum: u32, seconds: u64) -> (u64, u64) {
        let ns = |frames: u64| frames * 1_000_000_000 / u64::from(rate);
        let q = u64::from(quantum);
        let min_gap_ns = ns(q) / 4;
        let ring_frames = 16 * 1024;
        let mut ring = start_cushion_frames(period_in, quantum, ring_frames) as u64;
        let mut pacer = CyclePacer::default();
        let (mut zero_frames, mut silent_cycles) = (0u64, 0u64);
        let mut last_cycle_ns: Option<u64> = None;
        let mut fire = |now: u64, ring: &mut u64| {
            if last_cycle_ns.is_some_and(|t| now - t < min_gap_ns) {
                silent_cycles += 1;
            } else {
                *ring += q;
            }
            last_cycle_ns = Some(now);
        };
        let period_ns = ns(u64::from(period_in));
        let mut wake = period_ns;
        while wake < seconds * 1_000_000_000 {
            // Owed cycles fired from the poll timeout before this wake.
            while let Some(due) = pacer.next_due_ns() {
                if due >= wake || !pacer.take_due(due, q, rate) {
                    break;
                }
                fire(due, &mut ring);
            }
            let take = u64::from(period_in).min(ring);
            zero_frames += u64::from(period_in) - take;
            ring -= take;
            pacer.consumed(u64::from(period_in), q);
            if pacer.take_due(wake, q, rate) {
                fire(wake, &mut ring);
            }
            wake += period_ns;
        }
        (zero_frames, silent_cycles)
    }

    /// A DAC period that differs from the graph quantum must still be filled
    /// with real data: no cycle may fire so soon after another that the
    /// client cannot follow.
    #[test]
    fn period_quantum_mismatch_never_zero_fills() {
        for (rate, period_in, quantum) in [
            (88_200, 1881, 1024), // the bug: 1024 scaled linearly vs PipeWire
            (176_400, 3763, 2048),
            (48_000, 1024, 256),  // a client forcing a lower latency
            (48_000, 1024, 1500), // quantum larger than the period
            (96_000, 2048, 2048),
            (88_200, 1024, 1024),
        ] {
            let (zeros, silent) = simulate_pacing(rate, period_in, quantum, 20);
            assert_eq!(
                (zeros, silent),
                (0, 0),
                "{rate} Hz period {period_in} quantum {quantum}"
            );
        }
    }

    #[test]
    fn pacer_spaces_owed_cycles_and_caps_backlog() {
        let mut p = CyclePacer::default();
        p.consumed(1881, 1024);
        assert!(p.take_due(0, 1024, 88_200));
        assert!(p.next_due_ns().is_none());
        p.consumed(1881, 1024); // credit 857 + 1881 = 2 cycles
        let now = 21_000_000;
        assert!(p.take_due(now, 1024, 88_200));
        assert!(!p.take_due(now + 1, 1024, 88_200), "back to back");
        let due = p.next_due_ns().unwrap();
        assert_eq!(due - now, 1024 * 500_000_000 / 88_200);
        assert!(p.take_due(due, 1024, 88_200));
        p.consumed(1024 * 100, 1024);
        let mut n = 0;
        let mut t = due + 1_000_000_000;
        while p.take_due(t, 1024, 88_200) {
            n += 1;
            t += 1_000_000_000;
        }
        assert_eq!(n, CyclePacer::MAX_OWED);
        assert_eq!(start_cushion_frames(1024, 1024, 16_384), 1024);
        assert_eq!(start_cushion_frames(1881, 1024, 16_384), 1881 + 2048);
        assert_eq!(start_cushion_frames(16_384, 8192, 16_384), 16_384);
    }

    /// The rate of a sink's EnumFormat pod: `Some(vec![r])` for a fixed rate,
    /// `Some([default, alternatives..])` for an enum choice, `None` when unset.
    fn pod_rates(bytes: &[u8]) -> Option<Vec<i32>> {
        use spa::pod::deserialize::PodDeserializer;
        let (_, value) = PodDeserializer::deserialize_any_from(bytes).expect("pod");
        let spa::pod::Value::Object(obj) = value else {
            panic!("not an object");
        };
        let prop = obj
            .properties
            .iter()
            .find(|p| p.key == spa::sys::SPA_FORMAT_AUDIO_rate)?;
        match &prop.value {
            spa::pod::Value::Int(r) => Some(vec![*r]),
            spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                _,
                spa::utils::ChoiceEnum::Enum {
                    default,
                    alternatives,
                },
            ))) => Some(
                std::iter::once(*default)
                    .chain(alternatives.iter().copied())
                    .collect(),
            ),
            other => panic!("unexpected rate value {other:?}"),
        }
    }

    #[test]
    fn sink_format_pod_constrains_rate() {
        let one = build_sink_format_pod(2, &[48_000], 48_000).unwrap();
        assert_eq!(pod_rates(&one), Some(vec![48_000]));
        let many = build_sink_format_pod(2, &[48_000, 96_000], 96_000).unwrap();
        assert_eq!(pod_rates(&many), Some(vec![96_000, 48_000, 96_000]));
        // A default outside the set falls back to the first allowed rate.
        let many = build_sink_format_pod(2, &[44_100, 48_000], 96_000).unwrap();
        assert_eq!(pod_rates(&many), Some(vec![44_100, 44_100, 48_000]));
        let none = build_sink_format_pod(2, &[], 48_000).unwrap();
        assert_eq!(pod_rates(&none), Some(vec![48_000]));
    }

    #[test]
    fn parse_node_rate_handles_fraction_and_plain() {
        assert_eq!(parse_node_rate("1/44100"), Some(44_100));
        assert_eq!(parse_node_rate("1/192000"), Some(192_000));
        assert_eq!(parse_node_rate("48000"), Some(48_000));
        assert_eq!(parse_node_rate("0"), None);
    }

    #[test]
    fn prime_deadlines_stay_one_period_apart_on_time_completions() {
        let period = 1_000_000_u64;
        let start = 5_000_000_u64;
        let mut deadline = start;
        let mut deadlines = Vec::new();
        for _ in 0..4 {
            deadlines.push(deadline);
            // Cycle triggered at the deadline completes immediately.
            deadline = next_prime_deadline_ns(deadline, Some(deadline), period);
        }
        assert_eq!(
            deadlines,
            vec![
                start,
                start + period,
                start + 2 * period,
                start + 3 * period
            ]
        );
        assert_eq!(deadline, start + 4 * period);
    }

    #[test]
    fn slow_first_cycle_does_not_burst_later_prime_triggers() {
        let period = 1_000_000_u64;
        let start = 5_000_000_u64;
        let mut deadline = start;
        let mut deadlines = vec![deadline];

        // The first cycle takes four periods to complete (client startup/load).
        deadline = next_prime_deadline_ns(deadline, Some(start + 4 * period), period);
        assert_eq!(
            deadline,
            start + 5 * period,
            "next trigger must wait one period after the late completion"
        );

        // Later cycles complete on time; the schedule must never compress.
        for _ in 0..4 {
            deadlines.push(deadline);
            deadline = next_prime_deadline_ns(deadline, Some(deadline), period);
        }
        for pair in deadlines.windows(2) {
            assert!(
                pair[1] - pair[0] >= period,
                "consecutive prime triggers must stay >= one period apart"
            );
        }
    }

    #[test]
    fn jittery_completions_never_compress_prime_trigger_spacing() {
        let period = 1_000_000_u64;
        let start = 5_000_000_u64;
        let mut deadline = start;
        // Completion delay after each trigger (in nanoseconds).
        let jitter = [200_000_u64, 3_000_000, 500_000, 2_400_000, 900_000];
        for delay in jitter {
            let trigger = deadline;
            let completion = trigger + delay;
            let next = next_prime_deadline_ns(deadline, Some(completion), period);
            assert!(
                next >= completion + period,
                "next trigger must wait a full period after the completion"
            );
            assert!(
                next >= deadline + period,
                "next trigger must advance at least one period"
            );
            deadline = next;
        }
    }

    #[test]
    fn successful_replacement_chain_clears_fallback_warning() {
        let cfg = EngineDriverConfig::default();
        let shared = Arc::new(SharedEngineState::new(&cfg));
        let handle = test_driver_handle(Arc::clone(&shared));

        // A rebuild at generation 3 falls back to a reduced chain.
        apply_negotiated_chain(
            NegotiatedChain {
                required_chain_seq: Some(3),
                error: Some("failed to open IR 'room.wav'".into()),
                fatal: false,
                generation: 3,
                superseded: false,
                bypass_unbuildable: false,
                bypass_pending: false,
                bypass_epoch: 0,
            },
            &shared,
            &mut Vec::new(),
        );
        assert!(
            handle
                .snapshot()
                .error
                .unwrap_or_default()
                .contains("room.wav")
        );

        // A valid config applied without reopen publishes a replacement chain
        // at generation 4; the driver clears the stale warning through it.
        handle.clear_chain_warning_through(4);
        assert!(
            handle.snapshot().error.is_none(),
            "snapshot error must clear after a valid chain is published"
        );
    }

    #[test]
    fn stale_success_does_not_clear_newer_fallback_warning() {
        let cfg = EngineDriverConfig::default();
        let shared = Arc::new(SharedEngineState::new(&cfg));
        let handle = test_driver_handle(Arc::clone(&shared));

        apply_negotiated_chain(
            NegotiatedChain {
                required_chain_seq: Some(4),
                error: Some("newer IR failure".into()),
                fatal: false,
                generation: 4,
                superseded: false,
                bypass_unbuildable: false,
                bypass_pending: false,
                bypass_epoch: 0,
            },
            &shared,
            &mut Vec::new(),
        );

        // A success belonging to an older generation must leave the warning.
        handle.clear_chain_warning_through(3);
        assert!(
            handle
                .snapshot()
                .error
                .unwrap_or_default()
                .contains("newer IR failure")
        );

        // The matching (or a newer) generation clears it.
        handle.clear_chain_warning_through(4);
        assert!(handle.snapshot().error.is_none());
    }

    #[test]
    fn late_failure_does_not_resurrect_warning_after_newer_success() {
        let cfg = EngineDriverConfig::default();
        let shared = Arc::new(SharedEngineState::new(&cfg));
        let handle = test_driver_handle(Arc::clone(&shared));

        // A valid config at generation 5 is published (and clears through 5)
        // before the control thread records the fallback for generation 4.
        handle.clear_chain_warning_through(5);
        apply_negotiated_chain(
            NegotiatedChain {
                required_chain_seq: Some(4),
                error: Some("late IR failure".into()),
                fatal: false,
                generation: 4,
                superseded: false,
                bypass_unbuildable: false,
                bypass_pending: false,
                bypass_epoch: 0,
            },
            &shared,
            &mut Vec::new(),
        );
        assert!(
            handle.snapshot().error.is_none(),
            "a failure older than the latest success must not be recorded"
        );

        // A failure from a newer generation is still recorded.
        apply_negotiated_chain(
            NegotiatedChain {
                required_chain_seq: Some(6),
                error: Some("new IR failure".into()),
                fatal: false,
                generation: 6,
                superseded: false,
                bypass_unbuildable: false,
                bypass_pending: false,
                bypass_epoch: 0,
            },
            &shared,
            &mut Vec::new(),
        );
        assert!(
            handle
                .snapshot()
                .error
                .unwrap_or_default()
                .contains("new IR failure")
        );
    }

    fn test_driver_handle(shared: Arc<SharedEngineState>) -> EngineDriverHandle {
        let (ctrl_req_tx, _ctrl_req_rx) = mpsc::channel();
        let (pw_tx, _pw_rx) = pw::channel::channel::<PwMsg>();
        EngineDriverHandle {
            shared,
            cmd_efd: Arc::new(EventFd::new().unwrap()),
            ctrl_efd: Arc::new(EventFd::new().unwrap()),
            ctrl_req_tx: Mutex::new(ctrl_req_tx),
            lifecycle_lock: Mutex::new(()),
            pw_tx: Mutex::new(pw_tx),
            alsa_thread: Mutex::new(None),
            ctrl_thread: Mutex::new(None),
            pw_thread: Mutex::new(None),
        }
    }

    struct StubBackend {
        released_reservations: usize,
        closed_outputs: usize,
        /// Release hook runs so far, and what it was at the close and at
        /// the reservation release.
        hook_runs: Arc<AtomicUsize>,
        runs_at_close: Option<usize>,
        runs_at_release: Option<usize>,
    }

    impl DeviceBackend for StubBackend {
        type Reservation = ();
        type Output = ();

        fn card_index(&mut self, _device: &str) -> std::result::Result<u32, String> {
            Ok(0)
        }

        fn reserve(
            &mut self,
            _device: &str,
            _card: u32,
            _priority: i32,
        ) -> std::result::Result<Self::Reservation, String> {
            Ok(())
        }

        fn is_release_requested(&self, _res: &Self::Reservation) -> bool {
            false
        }

        fn release_reservation(&mut self, _res: Self::Reservation) {
            self.released_reservations += 1;
            self.runs_at_release = Some(self.hook_runs.load(Ordering::SeqCst));
        }

        fn open_and_prepare(
            &mut self,
            _device: &str,
            _graph_rate: u32,
            ladder: &[RateRatio],
            _quantum: u32,
        ) -> std::result::Result<OpenedDevice<Self::Output>, String> {
            let desired_ratio = ladder.first().copied().unwrap_or(RateRatio::ONE);
            Ok(OpenedDevice {
                output: (),
                actual_ratio: desired_ratio,
                output_rate: desired_ratio.output_rate(48_000).unwrap_or(48_000),
                graph_rate: 48_000,
                quantum: 1024,
                plan: audivane_io_alsa::RatioOpen::at(desired_ratio),
                session: 0,
            })
        }

        fn close_output(&mut self, _out: Self::Output) {
            self.closed_outputs += 1;
            self.runs_at_close = Some(self.hook_runs.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn negotiated_rebuild_failure_surfaces_error_and_fails_acquire_waiters() {
        let cfg = EngineDriverConfig::default();
        let shared = SharedEngineState::new(&cfg);

        // Recovered failure: the fallback chain resumes playback, but the
        // rebuild error shows in the snapshot and waiters get it right away
        // instead of timing out in `acquire_device`.
        let (acq_tx, acq_rx) = mpsc::sync_channel(1);
        let mut waiters = vec![acq_tx];
        let (seq, hold) = apply_negotiated_chain(
            NegotiatedChain {
                required_chain_seq: Some(7),
                error: Some("failed to open IR 'room.wav'".into()),
                fatal: false,
                generation: 1,
                superseded: false,
                bypass_unbuildable: false,
                bypass_pending: false,
                bypass_epoch: 0,
            },
            &shared,
            &mut waiters,
        );
        assert_eq!(seq, Some(7));
        assert!(!hold, "a fallback chain must not hold playback suspended");
        assert!(waiters.is_empty());
        assert_ne!(shared.state(), EngineState::Error);
        assert!(
            shared
                .effective_error()
                .unwrap_or_default()
                .contains("room.wav")
        );
        assert!(acq_rx.recv().unwrap().is_err());

        // Fatal failure: even the fallback could not be built. The engine
        // reports `Error` and the RT thread must stay suspended.
        let (acq_tx, acq_rx) = mpsc::sync_channel(1);
        let mut waiters = vec![acq_tx];
        let (seq, hold) = apply_negotiated_chain(
            NegotiatedChain {
                required_chain_seq: None,
                error: Some("no chain could be built".into()),
                fatal: true,
                generation: 2,
                superseded: false,
                bypass_unbuildable: false,
                bypass_pending: false,
                bypass_epoch: 0,
            },
            &shared,
            &mut waiters,
        );
        assert_eq!(seq, None);
        assert!(hold);
        assert_eq!(shared.state(), EngineState::Error);
        assert!(
            shared
                .effective_error()
                .unwrap_or_default()
                .contains("no chain")
        );
        assert!(acq_rx.recv().unwrap().is_err());

        // A later successful negotiation clears both the error and the warning.
        let (_, hold) = apply_negotiated_chain(
            NegotiatedChain {
                required_chain_seq: Some(8),
                error: None,
                fatal: false,
                generation: 3,
                superseded: false,
                bypass_unbuildable: false,
                bypass_pending: false,
                bypass_epoch: 0,
            },
            &shared,
            &mut Vec::new(),
        );
        assert!(!hold);
        shared.clear_error();
        assert!(shared.effective_error().is_none());
    }

    /// The latency a staged hardware raise waits for includes one period:
    /// the ALSA delay is read before the output thread writes the next one.
    #[test]
    fn output_latency_counts_the_period_about_to_be_written() {
        let cfg = EngineDriverConfig::default();
        let shared = Arc::new(SharedEngineState::new(&cfg));
        shared.set_state(EngineState::Playing);
        shared.output_rate.store(96_000, Ordering::Relaxed);
        shared.graph_rate.store(48_000, Ordering::Relaxed);
        shared.quantum.store(1024, Ordering::Relaxed);
        shared.alsa_delay_frames.store(9_600, Ordering::Relaxed);
        shared.dsp_latency_out.store(0, Ordering::Relaxed);
        let probe = test_driver_handle(Arc::clone(&shared)).output_latency_probe();
        let got = probe().unwrap().as_secs_f64();
        let want = 0.1 + 1024.0 / 48_000.0;
        assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        shared.set_state(EngineState::Idle);
        assert_eq!(probe(), None);
    }

    /// The release hook (the engine's safe hardware volume) runs before the
    /// ALSA output is closed and the reservation handed over, on an
    /// external ReserveDevice1 release as on an explicit one.
    #[test]
    fn release_hook_runs_before_the_device_is_handed_over() {
        for external in [true, false] {
            let cfg = EngineDriverConfig::default();
            let shared = SharedEngineState::new(&cfg);
            let ran = Arc::new(AtomicUsize::new(0));
            let ran_cb = Arc::clone(&ran);
            *shared.before_release.lock().unwrap() = Some(Arc::new(move |kind, device: &str| {
                assert_eq!(kind, ReleaseKind::HandOver);
                assert_eq!(device, "hw:Loopback,0,0", "the card let go of");
                ran_cb.fetch_add(1, Ordering::SeqCst);
            }));
            let mut backend = StubBackend {
                released_reservations: 0,
                closed_outputs: 0,
                hook_runs: Arc::clone(&ran),
                runs_at_close: None,
                runs_at_release: None,
            };
            let mut lifecycle = DeviceLifecycle {
                state: EngineState::Switching,
                last_error: None,
                reservation: Some((0, ())),
                active_device: Some("hw:Loopback,0,0".into()),
                active_graph_rate: 48_000,
                active_ratio: RateRatio::ONE,
                active_sink: SinkChoice::Dsp,
                output_held: true,
            };
            let mut awaiting = true;
            let mut release_waiters = Vec::new();
            let mut acquire_waiters = Vec::new();
            complete_release_teardown(
                &mut backend,
                &mut lifecycle,
                &shared,
                Some(()),
                &mut awaiting,
                external,
                &mut release_waiters,
                &mut acquire_waiters,
                || Ok(()),
            );
            assert_eq!(ran.load(Ordering::SeqCst), 1, "external={external}");
            assert_eq!(backend.runs_at_close, Some(1), "hook after the close");
            assert_eq!(backend.runs_at_release, Some(1), "hook after the handover");
            assert_eq!(backend.closed_outputs, 1);
            assert_eq!(backend.released_reservations, 1);
            assert_eq!(shared.state(), EngineState::Idle);
        }
    }

    /// A start-up failing after the first negotiation (PipeWire
    /// registration, say) hands the card over once, with its device; a
    /// start-up that succeeded leaves it to the driver.
    #[test]
    fn a_failed_start_up_hands_the_card_over_once() {
        let cfg = EngineDriverConfig::default();
        let shared = Arc::new(SharedEngineState::new(&cfg));
        let ran = Arc::new(Mutex::new(Vec::<(ReleaseKind, String)>::new()));
        let ran_cb = Arc::clone(&ran);
        *shared.before_release.lock().unwrap() = Some(Arc::new(move |kind, device: &str| {
            ran_cb.lock().unwrap().push((kind, device.to_string()));
        }));
        {
            let mut guard = InitHandOver::new(Arc::clone(&shared), "hw:CARD=A,DEV=0".into());
            guard.run();
        }
        {
            // An early `?` return: dropped while armed.
            let _guard = InitHandOver::new(Arc::clone(&shared), "hw:CARD=B,DEV=0".into());
        }
        {
            let mut guard = InitHandOver::new(Arc::clone(&shared), "hw:CARD=C,DEV=0".into());
            guard.disarm();
        }
        assert_eq!(
            *ran.lock().unwrap(),
            [
                (ReleaseKind::HandOver, "hw:CARD=A,DEV=0".to_string()),
                (ReleaseKind::HandOver, "hw:CARD=B,DEV=0".to_string()),
            ]
        );
    }

    /// The driver stopping (engine shutdown) hands the DAC over like a
    /// release: the hook runs before the reservation is released.
    #[test]
    fn stopping_runs_the_release_hook_before_the_reservation_goes() {
        let cfg = EngineDriverConfig::default();
        let shared = SharedEngineState::new(&cfg);
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_cb = Arc::clone(&ran);
        *shared.before_release.lock().unwrap() = Some(Arc::new(move |kind, device: &str| {
            assert_eq!(kind, ReleaseKind::HandOver);
            assert_eq!(device, "hw:Loopback,0,0", "the card let go of");
            ran_cb.fetch_add(1, Ordering::SeqCst);
        }));
        let mut backend = StubBackend {
            released_reservations: 0,
            closed_outputs: 0,
            hook_runs: Arc::clone(&ran),
            runs_at_close: None,
            runs_at_release: None,
        };
        let mut lifecycle = DeviceLifecycle {
            state: EngineState::Playing,
            last_error: None,
            reservation: Some((0, ())),
            active_device: Some("hw:Loopback,0,0".into()),
            active_graph_rate: 48_000,
            active_ratio: RateRatio::ONE,
            active_sink: SinkChoice::Dsp,
            output_held: false,
        };
        release_on_stop(&mut backend, &mut lifecycle, &shared);
        assert_eq!(backend.runs_at_release, Some(1));
        // Nothing held (already released): no hook.
        release_on_stop(&mut backend, &mut lifecycle, &shared);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// A release that completes after `release_device` stopped waiting
    /// clears the timeout error; any other error stays.
    #[test]
    fn a_late_release_clears_the_timeout_error() {
        for (earlier, cleared) in [(RELEASE_TIMEOUT_MSG, true), ("device unplugged", false)] {
            let cfg = EngineDriverConfig::default();
            let shared = SharedEngineState::new(&cfg);
            shared.set_error(earlier);
            let mut backend = StubBackend {
                released_reservations: 0,
                closed_outputs: 0,
                hook_runs: Arc::default(),
                runs_at_close: None,
                runs_at_release: None,
            };
            let mut lifecycle = DeviceLifecycle {
                state: EngineState::Switching,
                last_error: None,
                reservation: Some((0, ())),
                active_device: Some("hw:Loopback,0,0".into()),
                active_graph_rate: 48_000,
                active_ratio: RateRatio::ONE,
                active_sink: SinkChoice::Dsp,
                output_held: true,
            };
            complete_release_teardown(
                &mut backend,
                &mut lifecycle,
                &shared,
                Some(()),
                &mut true,
                false,
                &mut Vec::new(),
                &mut Vec::new(),
                || Ok(()),
            );
            assert_eq!(shared.state(), EngineState::Idle);
            assert_eq!(shared.effective_error().is_none(), cleared, "{earlier}");
        }
    }

    #[test]
    fn release_teardown_timeout_releases_resources_sets_error_and_fails_waiters() {
        let cfg = EngineDriverConfig::default();
        let shared = SharedEngineState::new(&cfg);
        let mut backend = StubBackend {
            released_reservations: 0,
            closed_outputs: 0,
            hook_runs: Arc::default(),
            runs_at_close: None,
            runs_at_release: None,
        };
        let mut lifecycle = DeviceLifecycle {
            state: EngineState::Switching,
            last_error: None,
            reservation: Some((0, ())),
            active_device: Some("hw:Loopback,0,0".into()),
            active_graph_rate: 48_000,
            active_ratio: RateRatio::ONE,
            active_sink: SinkChoice::Dsp,
            output_held: true,
        };

        let mut awaiting = true;
        let (rel_tx, rel_rx) = mpsc::sync_channel(1);
        let (acq_tx, acq_rx) = mpsc::sync_channel(1);
        let mut release_waiters = vec![rel_tx];
        let mut acquire_waiters = vec![acq_tx];

        let should_acquire = complete_release_teardown(
            &mut backend,
            &mut lifecycle,
            &shared,
            Some(()),
            &mut awaiting,
            false,
            &mut release_waiters,
            &mut acquire_waiters,
            || {
                Err(PwError::Init(
                    "PipeWire teardown acknowledgement timed out".into(),
                ))
            },
        );

        assert!(
            !should_acquire,
            "failed release teardown must not trigger acquire"
        );
        assert!(!awaiting);
        assert_eq!(backend.closed_outputs, 1);
        assert_eq!(backend.released_reservations, 1);
        assert!(!lifecycle.output_held);
        assert_eq!(shared.state(), EngineState::Error);
        assert!(
            shared
                .effective_error()
                .unwrap_or_default()
                .contains("timed out")
        );
        assert!(rel_rx.recv().unwrap().is_err());
        assert!(acq_rx.recv().unwrap().is_err());
    }

    #[test]
    fn apply_metadata_writes_disables_rate_following_and_sets_warning_on_persist_err() {
        let cfg = EngineDriverConfig::default();
        let shared = SharedEngineState::new(&cfg);

        let base = std::env::temp_dir().join(format!(
            "audivane-driver-meta-fail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();

        let mut tracker = MetadataTracker::with_path(blocker.join("settings-restore.json"));
        tracker.complete_initial_sync_and_recover();

        let mut disabled = false;
        let mut sent_ops = 0usize;
        let ok = apply_metadata_writes_with(
            &mut tracker,
            &shared,
            &mut disabled,
            &[("clock.force-rate", "Spa:String:JSON", "96000")],
            |ops| sent_ops += ops.len(),
        );

        assert!(!ok);
        assert!(
            disabled,
            "rate following must disable after persistence error"
        );
        assert_eq!(
            sent_ops, 0,
            "PipeWire writes must be skipped on persist failure"
        );
        assert_ne!(shared.state(), EngineState::Error);
        assert!(
            shared
                .effective_error()
                .unwrap_or_default()
                .contains("rate following disabled")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    struct ReadyMockProcessor {
        ready_seq: u64,
        installed_ratio: RateRatio,
    }

    impl EngineAudioProcessor for ReadyMockProcessor {
        fn process_block(
            &mut self,
            _active_sink: SinkChoice,
            _dsp_in: &[f32],
            _direct_in: &[f32],
            out: &mut [i32],
            _channels: usize,
            _ratio: RateRatio,
            _graph_rate: u32,
            _in_pos: u64,
        ) {
            out.fill(0);
        }

        fn run_selftest_direct(&mut self, _in_samples: &[f32], out_samples: &mut [i32]) {
            out_samples.fill(0);
        }

        fn poll_updates(
            &mut self,
            _active_sink: SinkChoice,
            _graph_rate: u32,
            _actual_ratio: RateRatio,
            _is_streaming: bool,
        ) {
        }

        fn latency_out_frames(&self, _active_sink: SinkChoice) -> usize {
            0
        }

        fn reset_dsp_state(&mut self) {}

        fn is_chain_ready(
            &self,
            active_sink: SinkChoice,
            actual_ratio: RateRatio,
            required_seq: Option<u64>,
        ) -> bool {
            if active_sink == SinkChoice::Direct {
                return true;
            }
            self.installed_ratio == actual_ratio
                && required_seq.is_none_or(|req| self.ready_seq >= req)
        }
    }

    #[test]
    fn a_resume_is_playing_only_once_the_device_started() {
        // Streams play and the chain is ready: the device still has to be
        // primed and started (and the live graph checked first). Until then
        // nothing reaches the DAC, so not Playing, and no ack yet.
        let shared = SharedEngineState::new(&EngineDriverConfig::default());
        shared.dsp_streaming.store(true, Ordering::Release);
        let (mut rt_tx, mut rt_rx) = rtrb::RingBuffer::<RtEvent>::new(8);
        let mut stash: BoundedEventStash<RtEvent, 16> = BoundedEventStash::new();
        let ctrl_efd = EventFd::new().unwrap();
        let mut proc = ReadyMockProcessor {
            ready_seq: 2,
            installed_ratio: RateRatio::ONE,
        };
        let mut awaiting = Some(Some(2));
        let mut suspended = true;
        let mut ack = false;
        assert!(try_complete_resume_gate(
            &mut proc,
            &shared,
            &mut awaiting,
            &mut suspended,
            &mut ack,
            SinkChoice::Dsp,
            RateRatio::ONE,
            &mut stash,
            &mut rt_tx,
            &ctrl_efd,
        ));
        assert!(!suspended && awaiting.is_none());
        assert_eq!(shared.state(), EngineState::Switching);
        assert!(rt_rx.pop().is_err(), "acknowledged before the start");
        on_device_started(&shared, &mut ack, &mut stash, &mut rt_tx, &ctrl_efd);
        assert_eq!(shared.state(), EngineState::Playing);
        assert!(matches!(rt_rx.pop(), Ok(RtEvent::Resumed)));
        // Only once.
        on_device_started(&shared, &mut ack, &mut stash, &mut rt_tx, &ctrl_efd);
        assert!(rt_rx.pop().is_err());
    }

    #[test]
    fn resume_gate_keeps_device_switching_until_required_chain_is_installed() {
        let cfg = EngineDriverConfig::default();
        let shared = SharedEngineState::new(&cfg);
        let (mut rt_tx, mut rt_rx) = rtrb::RingBuffer::<RtEvent>::new(8);
        let mut stash: BoundedEventStash<RtEvent, 16> = BoundedEventStash::new();
        let ctrl_efd = EventFd::new().unwrap();

        let mut proc = ReadyMockProcessor {
            ready_seq: 1,
            installed_ratio: RateRatio::ONE,
        };
        let mut awaiting = Some(Some(2));
        let mut suspended = false;
        let mut ack = false;

        // Ratio changed to 4x and chain seq=2 is not yet installed.
        let resumed = try_complete_resume_gate(
            &mut proc,
            &shared,
            &mut awaiting,
            &mut suspended,
            &mut ack,
            SinkChoice::Dsp,
            RateRatio::integer(4),
            &mut stash,
            &mut rt_tx,
            &ctrl_efd,
        );
        assert!(!resumed);
        assert!(
            suspended,
            "RT thread must stay suspended until new chain lands"
        );
        assert_eq!(shared.state(), EngineState::Switching);
        assert!(
            rt_rx.pop().is_err(),
            "Resumed event must not be emitted early"
        );

        // Now builder/reaper publishes seq=2 with 4x chain and RT installs it.
        proc.ready_seq = 2;
        proc.installed_ratio = RateRatio::integer(4);
        let resumed = try_complete_resume_gate(
            &mut proc,
            &shared,
            &mut awaiting,
            &mut suspended,
            &mut ack,
            SinkChoice::Dsp,
            RateRatio::integer(4),
            &mut stash,
            &mut rt_tx,
            &ctrl_efd,
        );
        assert!(resumed);
        assert!(!suspended);
        assert!(awaiting.is_none());
        assert_eq!(shared.state(), EngineState::Ready);
        assert!(matches!(rt_rx.pop(), Ok(RtEvent::Resumed)));
    }

    #[test]
    fn period_planner_fills_every_period_at_the_exact_rate() {
        for (ratio, period) in [
            (RateRatio::from_rates(44_100, 48_000), 1024u32),
            (RateRatio::from_rates(48_000, 44_100), 1024),
            (RateRatio::from_rates(96_000, 48_000), 512),
            (RateRatio::from_rates(192_000, 44_100), 441),
            (RateRatio::from_rates(44_100, 192_000), 4096),
            (RateRatio::integer(4), 4096),
        ] {
            let mut p = PeriodPlanner::default();
            let (mut total_in, mut total_out) = (0u64, 0u64);
            for _ in 0..2_000 {
                let (n_in, n_out) = p.plan(ratio, period);
                assert_eq!(n_out, ratio.frames_out(p.pos, n_in), "{ratio}");
                // Each period is filled; the overshoot stays below one
                // input frame's worth of output frames.
                assert!(p.credit + n_out >= period as usize, "{ratio} underrun");
                p.commit(n_in, n_out, period);
                assert!(
                    p.credit < (ratio.up as usize).div_ceil(ratio.down as usize).max(1),
                    "{ratio} credit {}",
                    p.credit
                );
                total_in += n_in as u64;
                total_out += n_out as u64;
            }
            assert_eq!(total_out, ratio.frames_out_total(total_in), "{ratio}");
            if ratio.is_integer() {
                assert_eq!(p.credit, 0);
            }
            p.reset();
            assert_eq!(p, PeriodPlanner::default());
        }
    }

    /// A ratio change restarts the planner: no position or carried frames
    /// from the old ratio's grid leak into the new one.
    #[test]
    fn period_planner_restarts_when_the_ratio_changes() {
        let a = RateRatio::from_rates(44_100, 48_000);
        let b = RateRatio::from_rates(48_000, 44_100);
        let mut p = PeriodPlanner::default();
        for _ in 0..7 {
            let (n_in, n_out) = p.plan(a, 1000);
            p.commit(n_in, n_out, 1000);
        }
        assert!(p.pos > 0);
        let mut fresh = PeriodPlanner::default();
        assert_eq!(p.plan(b, 1000), fresh.plan(b, 1000));
        assert_eq!(p, fresh);
        assert_eq!((p.pos, p.credit, p.ratio), (0, 0, b));
    }
}
