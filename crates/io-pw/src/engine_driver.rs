//! Dual-sink PipeWire driver (`Audivane DSP` + `Audivane Direct`) paced by a
//! single ALSA `hw:` DAC clock, with PipeWire `settings` metadata rate
//! following and off-RT device lifecycle / chain negotiation.

use crate::clock::{
    BoundedEventStash, DriverClockState, EventFd, FadeRamp, SynchronizedIo, mono_now_ns,
    read_target_clock,
};
use crate::error::{PwError, Result};
use crate::lifecycle::{
    DesiredDspConfig, DeviceBackend, DeviceLifecycle, OpenedDevice, ReopenGate, decide_rt_reopen,
};
use crate::metadata::{MetadataOp, MetadataTracker};
use crate::rate_arbiter::{RateArbiter, RateClient, RateInputs, Verdict};
use audivane_io_alsa::{
    AlsaConfig, AlsaOutput, DEFAULT_RESERVE_PRIORITY, DeviceReservation, NegotiatedParams,
    card_index_from_device, try_promote_current_thread_rt,
};
use audivane_protocol::ipc::{EngineState, RatePolicy, SinkChoice};
use audivane_protocol::{RateTarget, SampleFormat, resolve_ratio};
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
    /// - `out`: interleaved left-justified `i32` output at `rate * ratio`.
    #[allow(clippy::too_many_arguments)]
    fn process_block(
        &mut self,
        active_sink: SinkChoice,
        dsp_in: &[f32],
        direct_in: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: u32,
        graph_rate: u32,
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
        actual_ratio: u32,
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
        _actual_ratio: u32,
        _required_seq: Option<u64>,
    ) -> bool {
        true
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
}

/// Callback invoked off the RT thread right after ALSA opens/reopens at
/// `(graph_rate, actual_ratio)` and before `Resume` is sent to the RT thread.
///
/// Returns the swap sequence the resume gate must wait for (when the engine
/// queued a chain swap) plus any rebuild failure that was recovered from with a
/// fallback chain.
pub type RatioNegotiator = Arc<dyn Fn(u32, u32, SinkChoice) -> NegotiatedChain + Send + Sync>;

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
    pub rate_policy: RatePolicy,
    pub reserve_device: bool,
    pub reserve_priority: i32,
    pub rt_priority: u32,
}

impl EngineDriverConfig {
    pub fn desired_dsp(&self, generation: u64) -> DesiredDspConfig {
        DesiredDspConfig {
            dsp_enabled: self.dsp_enabled,
            rate_target: self.dsp_rate_target,
            max_ratio: self.max_dsp_ratio.max(1),
            generation,
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
            rate_policy: RatePolicy::default(),
            reserve_device: true,
            reserve_priority: DEFAULT_RESERVE_PRIORITY,
            rt_priority: 20,
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
    /// PipeWire sink state transitions.
    pub stream_starts_direct: u64,
    pub stream_starts_dsp: u64,
    pub stream_pauses_direct: u64,
    pub stream_pauses_dsp: u64,
}

/// Point-in-time driver telemetry used by `RealEngine::snapshot()`.
#[derive(Clone, Debug)]
pub struct EngineDriverSnapshot {
    pub state: EngineState,
    pub error: Option<String>,
    pub format: Option<SampleFormat>,
    pub graph_rate: u32,
    pub output_rate: u32,
    pub ratio: u32,
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

/// Scale a 48 kHz base quantum to `rate`, matching PipeWire's quantum scaling
/// rules (power-of-two friendly when `q48` is a power of two).
pub fn scaled_quantum(q48: u32, rate: u32) -> u32 {
    let q48 = q48.max(32);
    let r = rate.max(8_000) as u64;
    let raw = ((q48 as u64 * r) / 48_000).max(32) as u32;
    if q48.is_power_of_two() && r < 48_000 {
        let p2 = 1u32 << (31 - raw.leading_zeros());
        p2.max(32)
    } else if q48.is_power_of_two() && r.is_multiple_of(48_000) {
        q48 * (r / 48_000) as u32
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
    /// Native sample rate tracked for active Direct/DSP clients (0 = none).
    desired_client_rate: AtomicU32,
    stop: AtomicBool,
    release_requested: AtomicBool,
    dsp_node_id: AtomicU32,
    direct_node_id: AtomicU32,
    graph_rate: AtomicU32,
    output_rate: AtomicU32,
    ratio: AtomicU32,
    quantum: AtomicU32,
    alsa_delay_frames: AtomicI64,
    xruns: AtomicU64,
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
    resumes: AtomicU64,
    rate_switches: AtomicU64,
    fade_outs: AtomicU64,
    fade_ins: AtomicU64,
    idle_stops: AtomicU64,
    drain_silence_periods: AtomicU64,
    prime_silence_periods: AtomicU64,
    stream_starts_direct: AtomicU64,
    stream_starts_dsp: AtomicU64,
    stream_pauses_direct: AtomicU64,
    stream_pauses_dsp: AtomicU64,
    frames_out: AtomicU64,
    dsp_load_bits: AtomicU64,
    dsp_latency_out: AtomicU64,
    cycles_completed: AtomicU64,
    last_error: Mutex<Option<String>>,
    metadata_warning: Mutex<Option<String>>,
    /// Recovered chain-rebuild failure plus the config generation that produced
    /// it. Kept with a generation so a success from an older chain build cannot
    /// clear a warning recorded by a newer one.
    chain_warning: Mutex<ChainWarningState>,
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
    fn new(cfg: &EngineDriverConfig) -> Self {
        let init_rate = cfg.rate_policy.default_rate.max(8_000);
        let init_q = scaled_quantum(cfg.quantum, init_rate);
        let init_ratio = if cfg.dsp_enabled {
            resolve_ratio(cfg.dsp_rate_target, init_rate).clamp(1, cfg.max_dsp_ratio.max(1))
        } else {
            1
        };
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
            desired_client_rate: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            release_requested: AtomicBool::new(false),
            dsp_node_id: AtomicU32::new(pw::constants::ID_ANY),
            direct_node_id: AtomicU32::new(pw::constants::ID_ANY),
            graph_rate: AtomicU32::new(init_rate),
            output_rate: AtomicU32::new(init_rate * init_ratio),
            ratio: AtomicU32::new(init_ratio),
            quantum: AtomicU32::new(init_q),
            alsa_delay_frames: AtomicI64::new(0),
            xruns: AtomicU64::new(0),
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
            resumes: AtomicU64::new(0),
            rate_switches: AtomicU64::new(0),
            fade_outs: AtomicU64::new(0),
            fade_ins: AtomicU64::new(0),
            idle_stops: AtomicU64::new(0),
            drain_silence_periods: AtomicU64::new(0),
            prime_silence_periods: AtomicU64::new(0),
            stream_starts_direct: AtomicU64::new(0),
            stream_starts_dsp: AtomicU64::new(0),
            stream_pauses_direct: AtomicU64::new(0),
            stream_pauses_dsp: AtomicU64::new(0),
            frames_out: AtomicU64::new(0),
            dsp_load_bits: AtomicU64::new(0.0_f64.to_bits()),
            dsp_latency_out: AtomicU64::new(0),
            cycles_completed: AtomicU64::new(0),
            last_error: Mutex::new(None),
            metadata_warning: Mutex::new(None),
            chain_warning: Mutex::new(ChainWarningState::default()),
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
        config: EngineDriverConfig,
        last_rate: u32,
        dsp_prod: rtrb::Producer<f32>,
        direct_prod: rtrb::Producer<f32>,
        reply_tx: mpsc::SyncSender<Result<SendRawStream>>,
    },
    Stop,
}

enum CtrlReq {
    UpdateConfig {
        config: EngineDriverConfig,
        force_reopen: bool,
    },
    ReleaseDevice {
        reply_tx: mpsc::SyncSender<Result<()>>,
    },
    AcquireDevice {
        reply_tx: mpsc::SyncSender<Result<()>>,
    },
}

#[allow(clippy::large_enum_variant)]
enum RtControlCmd {
    UpdateDesiredDsp(DesiredDspConfig),
    Resume {
        alsa: Box<AlsaOutput>,
        alsa_fds: Vec<libc::pollfd>,
        params: NegotiatedParams,
        graph_rate: u32,
        actual_ratio: u32,
        target_sink: SinkChoice,
        desired_dsp: DesiredDspConfig,
        negotiated_key: (u64, SinkChoice, u32),
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

#[derive(Clone, Copy, Debug)]
struct TestStallConfig {
    stall_ms: u64,
    after_frames_out: u64,
}

impl TestStallConfig {
    fn from_env(rate: u32, ratio: u32) -> Option<Self> {
        let stall_ms: u64 = std::env::var("AUDIVANE_TEST_STALL_MS").ok()?.parse().ok()?;
        if stall_ms == 0 {
            return None;
        }
        let after_ms: u64 = std::env::var("AUDIVANE_TEST_STALL_AFTER_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);
        let out_rate = rate.saturating_mul(ratio.max(1)) as u64;
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
    pub fn spawn<P: EngineAudioProcessor>(
        config: EngineDriverConfig,
        processor: P,
        on_negotiated: Option<RatioNegotiator>,
    ) -> Result<Self> {
        let channels = config.channels.max(1);
        let init_rate = config.rate_policy.default_rate.max(8_000);
        let init_q = scaled_quantum(config.quantum, init_rate);
        let init_desired = config.desired_dsp(0);
        let desired_ratio = init_desired.resolve_ratio_for(SinkChoice::Dsp, init_rate);
        let test_stall = TestStallConfig::from_env(init_rate, desired_ratio);

        let shared = Arc::new(SharedEngineState::new(&config));
        let cmd_efd = Arc::new(EventFd::new()?);
        let data_efd = Arc::new(EventFd::new()?);
        let ctrl_efd = Arc::new(EventFd::new()?);

        // Step 1: Reserve card + open ALSA off-RT before starting RT thread.
        let reservation = if config.reserve_device {
            let efd_cb = Arc::clone(&ctrl_efd);
            let flag_cb = Arc::clone(&shared);
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

        let mut base_alsa_cfg = AlsaConfig {
            device: config.device.clone(),
            rate: init_rate.saturating_mul(desired_ratio),
            channels,
            period_frames: init_q.saturating_mul(desired_ratio),
            buffer_periods: config.buffer_periods.max(2),
            preferred_format: config.formats.first().copied(),
            strict_format: false,
            nonblock: true,
        };
        let (mut alsa_init, actual_ratio) = match AlsaOutput::open_with_ratio_fallback(
            &base_alsa_cfg,
            init_rate,
            desired_ratio,
            init_q,
            &config.formats,
        ) {
            Ok(v) => v,
            Err(e) => {
                drop(reservation);
                return Err(e.into());
            }
        };
        if let Err(e) = alsa_init.prepare() {
            drop(alsa_init);
            drop(reservation);
            return Err(e.into());
        }
        let init_params = alsa_init.params();
        let alsa_fds = alsa_init.poll_descriptors()?;

        base_alsa_cfg.rate = init_params.rate;
        shared
            .format
            .store(format_to_u8(Some(init_params.format)), Ordering::Release);
        shared.graph_rate.store(init_rate, Ordering::Release);
        shared
            .output_rate
            .store(init_params.rate, Ordering::Release);
        shared.ratio.store(actual_ratio, Ordering::Release);
        shared.quantum.store(
            (init_params.period_frames / actual_ratio).max(32),
            Ordering::Release,
        );

        let init_negotiated = on_negotiated
            .as_ref()
            .map(|cb| cb(init_rate, actual_ratio, SinkChoice::Dsp))
            .unwrap_or_default();
        let mut init_waiters: Vec<mpsc::SyncSender<Result<()>>> = Vec::new();
        let (init_required_seq, init_hold_error) =
            apply_negotiated_chain(init_negotiated, &shared, &mut init_waiters);

        // Step 2: Create lock-free audio rings (~16 * 1024 frames) and control rings.
        let ring_samples = (16 * 1024 * channels as usize).max(32 * 1024);
        let (dsp_prod, dsp_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
        let (direct_prod, direct_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
        let (ctrl_to_rt_tx, ctrl_to_rt_rx) = rtrb::RingBuffer::<RtControlCmd>::new(32);
        let (rt_to_ctrl_tx, rt_to_ctrl_rx) = rtrb::RingBuffer::<RtEvent>::new(32);

        // Step 3: Spawn PipeWire thread registering both sinks + metadata + registry listener.
        let (pw_tx, pw_rx) = pw::channel::channel::<PwMsg>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<SendRawStream>>(1);

        let pw_cfg = config.clone();
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
                drop(alsa_init);
                drop(reservation);
                return Err(e);
            }
            Err(e) => {
                shared.stop.store(true, Ordering::Release);
                let _ = pw_thread.join();
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
                    channels as usize,
                    init_rate,
                    actual_ratio,
                    reservation,
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
                    init_required_seq,
                    init_hold_error,
                    Box::new(alsa_init),
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
            ratio: self.shared.ratio.load(Ordering::Acquire),
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
                stream_starts_direct: self.shared.stream_starts_direct.load(Ordering::Relaxed),
                stream_starts_dsp: self.shared.stream_starts_dsp.load(Ordering::Relaxed),
                stream_pauses_direct: self.shared.stream_pauses_direct.load(Ordering::Relaxed),
                stream_pauses_dsp: self.shared.stream_pauses_dsp.load(Ordering::Relaxed),
            },
        }
    }

    /// Return a clonable [`DriverWaker`] for waking the ALSA RT thread from
    /// auxiliary threads such as the chain reaper.
    pub fn waker(&self) -> DriverWaker {
        DriverWaker {
            cmd_efd: Arc::clone(&self.cmd_efd),
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
                config,
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
                let msg = "release_device timed out waiting for control teardown".to_string();
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
    channels: usize,
    quantum_48k: u32,
    peer_direct: Option<pw::stream::StreamRc>,
    shared: Arc<SharedEngineState>,
    cmd_efd: Arc<EventFd>,
    data_efd: Arc<EventFd>,
    prod: rtrb::Producer<f32>,
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
            self.policy.default_rate
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
        let allowed_val = if !self.policy.follow_source || self.policy.allowed_rates.is_empty() {
            format!("[ {target_force} ]")
        } else {
            let parts: Vec<String> = self
                .policy
                .allowed_rates
                .iter()
                .map(u32::to_string)
                .collect();
            format!("[ {} ]", parts.join(" "))
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

        if !self.rate_following_disabled {
            let direct: Vec<RateClient> = direct_clients.into_values().collect();
            let dsp: Vec<RateClient> = dsp_clients.into_values().collect();
            let inputs = RateInputs {
                policy: &self.policy,
                current: self.forced_rate,
                direct_active: direct_client_running || direct_has_link,
                direct: &direct,
                dsp: &dsp,
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
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
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
        let context = pw::context::ContextRc::new(mainloop, None).map_err(PwError::PipeWire)?;
        let core = context.connect_rc(None).map_err(PwError::PipeWire)?;
        let registry = core.get_registry_rc().map_err(PwError::PipeWire)?;

        let channels = config.channels.max(1);
        let allowed_str = if config.rate_policy.allowed_rates.is_empty() {
            format!("[ {init_rate} ]")
        } else {
            let joined: Vec<String> = config
                .rate_policy
                .allowed_rates
                .iter()
                .map(u32::to_string)
                .collect();
            format!("[ {} ]", joined.join(" "))
        };

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
                shared: Arc::clone(shared),
                _registry: registry,
                core,
                _context: context,
            },
            SendRawStream(raw),
        ))
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
        config: EngineDriverConfig,
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
                    t.policy = policy.clone();
                    t.quantum_48k = quantum_48k;
                    t.rate_following_disabled = false;
                    let before = t.forced_rate;
                    t.apply_policy_to_metadata(if policy.follow_source {
                        Some(last_rate)
                    } else {
                        Some(policy.default_rate)
                    });
                    if t.forced_rate != before {
                        t.arbiter.note_switch(Instant::now());
                    }
                    // The conflict mode may have changed: decide again.
                    t.recompute_routing_and_rate();
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
}

impl DeviceBackend for RealDeviceBackend {
    type Reservation = DeviceReservation;
    type Output = Box<AlsaOutput>;

    fn card_index(&mut self, device: &str) -> std::result::Result<u32, String> {
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

    fn open_and_prepare(
        &mut self,
        device: &str,
        graph_rate: u32,
        desired_ratio: u32,
        quantum: u32,
    ) -> std::result::Result<OpenedDevice<Self::Output>, String> {
        let alsa_cfg = AlsaConfig {
            device: device.to_string(),
            rate: graph_rate.saturating_mul(desired_ratio),
            channels: self.channels,
            period_frames: quantum.saturating_mul(desired_ratio),
            buffer_periods: self.buffer_periods.max(2),
            preferred_format: self.formats.first().copied(),
            strict_format: false,
            nonblock: true,
        };
        let (mut alsa, actual_ratio) = AlsaOutput::open_with_ratio_fallback(
            &alsa_cfg,
            graph_rate,
            desired_ratio,
            quantum,
            &self.formats,
        )
        .map_err(|e| e.to_string())?;
        alsa.prepare().map_err(|e| e.to_string())?;
        let output_rate = alsa.params().rate;
        Ok(OpenedDevice {
            output: Box::new(alsa),
            actual_ratio,
            output_rate,
        })
    }

    fn close_output(&mut self, out: Self::Output) {
        drop(out);
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
    if let Some(out) = old_alsa {
        backend.close_output(out);
    }
    let teardown_res = disconnect_and_wait();
    lifecycle.finish_release_to_idle(backend, None);
    shared.format.store(0, Ordering::Release);
    shared.active_sink.store(0, Ordering::Release);
    shared.alsa_delay_frames.store(0, Ordering::Relaxed);
    *awaiting_release_teardown = false;

    match teardown_res {
        Ok(()) => {
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

#[allow(clippy::too_many_arguments)]
fn run_control_thread(
    mut config: EngineDriverConfig,
    channels: usize,
    init_rate: u32,
    init_actual_ratio: u32,
    init_reservation: Option<(u32, DeviceReservation)>,
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
    };
    let mut lifecycle = DeviceLifecycle::<RealDeviceBackend> {
        state: EngineState::Ready,
        last_error: None,
        reservation: init_reservation,
        active_device: Some(config.device.clone()),
        active_graph_rate: init_rate,
        active_ratio: init_actual_ratio,
        active_sink: SinkChoice::Dsp,
        output_held: true,
    };

    let mut desired_gen: u64 = 0;
    let mut desired_dsp = config.desired_dsp(desired_gen);
    let mut last_graph_rate = init_rate;
    let mut last_ratio = init_actual_ratio;
    let mut last_quantum = scaled_quantum(config.quantum, init_rate);
    let mut in_cap = (config.buffer_periods.max(2) * last_quantum).max(4096) as usize * channels;
    let mut out_cap = in_cap * init_actual_ratio.max(1) as usize;
    let mut released_externally = false;
    let mut awaiting_release_teardown = false;
    let mut reopen_gate = ReopenGate::new();
    let mut pending_rt_cmds: VecDeque<RtControlCmd> = VecDeque::new();
    let mut pending_release_waiters: Vec<mpsc::SyncSender<Result<()>>> = Vec::new();
    let mut pending_acquire_waiters: Vec<mpsc::SyncSender<Result<()>>> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    let perform_acquire =
        |config: &EngineDriverConfig,
         desired_dsp: DesiredDspConfig,
         last_graph_rate: u32,
         last_ratio: &mut u32,
         last_quantum: &mut u32,
         in_cap: &mut usize,
         out_cap: &mut usize,
         backend: &mut RealDeviceBackend,
         lifecycle: &mut DeviceLifecycle<RealDeviceBackend>,
         pending_rt_cmds: &mut VecDeque<RtControlCmd>,
         ctrl_to_rt_tx: &mut rtrb::Producer<RtControlCmd>,
         pending_acquire_waiters: &mut Vec<mpsc::SyncSender<Result<()>>>| {
            shared.clear_error();
            shared.clear_chain_warning();
            shared.release_requested.store(false, Ordering::Release);
            shared.set_state(EngineState::Switching);

            let active_sink =
                sink_from_u8(shared.active_sink.load(Ordering::Acquire)).unwrap_or(SinkChoice::Dsp);
            let q = scaled_quantum(config.quantum, last_graph_rate);
            let opened = match lifecycle.handoff_and_open(
                backend,
                None,
                &config.device,
                config.reserve_device,
                config.reserve_priority,
                last_graph_rate,
                q,
                active_sink,
                desired_dsp,
            ) {
                Ok(o) => o,
                Err(msg) => {
                    shared.set_error(msg.clone());
                    finish_waiters(pending_acquire_waiters, || Err(PwError::Init(msg.clone())));
                    return;
                }
            };

            let alsa = opened.output;
            let actual_ratio = opened.actual_ratio;
            let params = alsa.params();
            let fds = alsa.poll_descriptors().unwrap_or_default();
            *last_ratio = actual_ratio;
            *last_quantum = q;
            shared
                .format
                .store(format_to_u8(Some(params.format)), Ordering::Release);
            shared.graph_rate.store(last_graph_rate, Ordering::Release);
            shared.output_rate.store(params.rate, Ordering::Release);
            shared.ratio.store(actual_ratio, Ordering::Release);
            shared.quantum.store(
                (params.period_frames / actual_ratio).max(32),
                Ordering::Release,
            );

            // Rebuild DSP chain for (last_graph_rate, actual_ratio) off-RT before Resume.
            let negotiated = on_negotiated
                .as_ref()
                .map(|cb| cb(last_graph_rate, actual_ratio, active_sink))
                .unwrap_or_default();
            let (required_chain_seq, hold_error) =
                apply_negotiated_chain(negotiated, &shared, pending_acquire_waiters);
            let needed_in = (params.buffer_frames / actual_ratio).max(4096) as usize * channels;
            let (dsp_in_buf, direct_in_buf, out_buf) =
                maybe_grow_buffers(needed_in, actual_ratio, in_cap, out_cap);

            let ring_samples = (16 * 1024 * channels).max(32 * 1024);
            let (dsp_prod, dsp_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let (direct_prod, direct_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);
            let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<SendRawStream>>(1);
            let _ = pw_tx.send(PwMsg::Reconnect {
                config: config.clone(),
                last_rate: last_graph_rate,
                dsp_prod,
                direct_prod,
                reply_tx,
            });
            let new_raw = match reply_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(raw)) => raw,
                Ok(Err(e)) => {
                    backend.close_output(alsa);
                    lifecycle.output_held = false;
                    lifecycle.release_reservation(backend);
                    lifecycle.active_device = None;
                    lifecycle.state = EngineState::Error;
                    let msg = format!("PipeWire reconnect failed: {e}");
                    shared.set_error(msg.clone());
                    finish_waiters(pending_acquire_waiters, || Err(PwError::Init(msg.clone())));
                    return;
                }
                Err(e) => {
                    backend.close_output(alsa);
                    lifecycle.output_held = false;
                    lifecycle.release_reservation(backend);
                    lifecycle.active_device = None;
                    lifecycle.state = EngineState::Error;
                    let msg = format!("PipeWire reconnect timed out: {e}");
                    shared.set_error(msg.clone());
                    finish_waiters(pending_acquire_waiters, || Err(PwError::Init(msg.clone())));
                    return;
                }
            };

            let fade_in_frames = ((params.rate as usize) * FADE_MS) / 1000;
            let negotiated_key = (desired_dsp.generation, active_sink, last_graph_rate);
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
                    desired_dsp,
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
        };

    while !shared.stop.load(Ordering::Acquire) {
        flush_pending_rt_cmds(&mut pending_rt_cmds, &mut ctrl_to_rt_tx, &cmd_efd);

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
                    desired_gen = desired_gen.saturating_add(1);
                    config = new_cfg;
                    desired_dsp = config.desired_dsp(desired_gen);
                    backend.formats = config.formats.clone();
                    backend.channels = config.channels.max(1);
                    backend.buffer_periods = config.buffer_periods.max(2);

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

                    if rate_policy_changed {
                        let _ = pw_tx.send(PwMsg::ApplyRatePolicy {
                            policy: config.rate_policy.clone(),
                            quantum_48k: config.quantum,
                            last_rate: last_graph_rate,
                        });
                    }
                    if reopen_gate.ctrl_request_reopen(
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
                CtrlReq::ReleaseDevice { reply_tx } => {
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
                    if awaiting_release_teardown {
                        // Release teardown is still in flight; queue acquire until
                        // `ReleasedIdle` finishes full teardown.
                        released_externally = false;
                        pending_acquire_waiters.push(reply_tx);
                    } else if matches!(shared.state(), EngineState::Idle | EngineState::Error) {
                        released_externally = false;
                        pending_acquire_waiters.push(reply_tx);
                        perform_acquire(
                            &config,
                            desired_dsp,
                            last_graph_rate,
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
                    let awaiting_cfg = reopen_gate.ctrl_take_awaiting_cfg();

                    let eff_rate = if awaiting_cfg && !config.rate_policy.follow_source {
                        config.rate_policy.default_rate.max(8_000)
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

                    match lifecycle.handoff_and_open(
                        &mut backend,
                        old_alsa,
                        &config.device,
                        config.reserve_device,
                        config.reserve_priority,
                        eff_rate,
                        q,
                        target_sink,
                        desired_dsp,
                    ) {
                        Ok(opened) => {
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
                            shared.ratio.store(actual_ratio, Ordering::Release);
                            shared.quantum.store(
                                (new_params.period_frames / actual_ratio).max(32),
                                Ordering::Release,
                            );
                            shared.clear_error();
                            shared.clear_chain_warning();

                            // Rebuild DSP chain for (eff_rate, actual_ratio) off-RT before Resume.
                            let negotiated = on_negotiated
                                .as_ref()
                                .map(|cb| cb(eff_rate, actual_ratio, target_sink))
                                .unwrap_or_default();
                            let (required_chain_seq, hold_error) = apply_negotiated_chain(
                                negotiated,
                                &shared,
                                &mut pending_acquire_waiters,
                            );

                            let needed_in = (new_params.buffer_frames / actual_ratio).max(4096)
                                as usize
                                * channels;
                            let (dsp_in_buf, direct_in_buf, out_buf) = maybe_grow_buffers(
                                needed_in,
                                actual_ratio,
                                &mut in_cap,
                                &mut out_cap,
                            );

                            let fade_in_frames = ((new_params.rate as usize) * FADE_MS) / 1000;
                            let negotiated_key = (desired_dsp.generation, target_sink, eff_rate);
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
                            shared.set_error(msg);
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
                        perform_acquire(
                            &config,
                            desired_dsp,
                            last_graph_rate,
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
    lifecycle.release_reservation(&mut backend);
}

type GrowthBuffers = (Option<Vec<f32>>, Option<Vec<f32>>, Option<Vec<i32>>);

fn maybe_grow_buffers(
    needed_in: usize,
    actual_ratio: u32,
    in_cap: &mut usize,
    out_cap: &mut usize,
) -> GrowthBuffers {
    let needed_out = needed_in * (actual_ratio.max(1) as usize);
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
    active_sink: SinkChoice,
    active_ratio: u32,
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
    shared.set_state(if streaming_now {
        EngineState::Playing
    } else {
        EngineState::Ready
    });
    let _ = event_stash.push_or_stash(RtEvent::Resumed, rt_to_ctrl_tx, ctrl_efd);
    true
}

#[allow(clippy::too_many_arguments)]
fn run_alsa_thread<P: EngineAudioProcessor>(
    config: EngineDriverConfig,
    init_rate: u32,
    init_ratio: u32,
    init_desired: DesiredDspConfig,
    init_required_seq: Option<u64>,
    init_hold_error: bool,
    alsa_init: Box<AlsaOutput>,
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
    try_promote_current_thread_rt(config.rt_priority);

    let mut alsa_opt: Option<Box<AlsaOutput>> = Some(alsa_init);
    let mut params = alsa_opt.as_ref().unwrap().params();
    let channels = params.channels as usize;
    let mut active_graph_rate = init_rate;
    let mut active_ratio = init_ratio.max(1);
    let mut active_sink = SinkChoice::Dsp;
    let mut rt_dsp_cfg = init_desired;
    let mut last_negotiated_key: Option<(u64, SinkChoice, u32)> =
        Some((init_desired.generation, SinkChoice::Dsp, init_rate));

    let max_in_frames = (params.buffer_frames / active_ratio).max(4096) as usize;
    let mut dsp_in_buf = vec![0.0f32; max_in_frames * channels];
    let mut direct_in_buf = vec![0.0f32; max_in_frames * channels];
    let mut out_buf = vec![0i32; max_in_frames * (active_ratio as usize) * channels];
    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(16);
    let mut event_stash: BoundedEventStash<RtEvent, 16> = BoundedEventStash::new();

    let mut clock_state = DriverClockState::new();
    let mut fade_ramp = FadeRamp::unity();
    let mut alsa_running = false;
    let mut suspended = false;
    let mut awaiting_chain_for_resume: Option<Option<u64>> = None;
    let mut released_idle = false;
    let mut reopen_gate = ReopenGate::new();
    let mut pending_reopen: Option<(u32, u32, SinkChoice)> = None;
    let mut credit_in_frames: u64 = 0;
    let mut first_prime_after_reopen = false;
    let mut dsp_load_ewma = 0.0_f64;
    let mut stalled_once = false;

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
        event_stash.flush(&mut rt_to_ctrl_tx, &ctrl_efd);

        // 1. Drain control commands.
        while let Ok(cmd) = ctrl_to_rt_rx.pop() {
            match cmd {
                RtControlCmd::UpdateDesiredDsp(cfg) => {
                    if cfg.generation >= rt_dsp_cfg.generation {
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
                    if desired_dsp.generation >= rt_dsp_cfg.generation {
                        rt_dsp_cfg = desired_dsp;
                    }
                    last_negotiated_key = Some(negotiated_key);
                    if graph_rate != active_graph_rate {
                        shared.rate_switches.fetch_add(1, Ordering::Relaxed);
                    }
                    shared.resumes.fetch_add(1, Ordering::Relaxed);
                    shared.fade_ins.fetch_add(1, Ordering::Relaxed);
                    alsa_opt = Some(alsa);
                    params = new_params;
                    active_graph_rate = graph_rate;
                    active_ratio = actual_ratio.max(1);
                    active_sink = target_sink;
                    released_idle = false;
                    reopen_gate.rt_finish_reopen();
                    pending_reopen = None;
                    alsa_running = false;
                    first_prime_after_reopen = true;
                    fade_ramp.start_fade_in(fade_in_frames);
                    processor.poll_updates(active_sink, active_graph_rate, active_ratio, false);
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
                            active_sink,
                            active_ratio,
                            &mut event_stash,
                            &mut rt_to_ctrl_tx,
                            &ctrl_efd,
                        );
                    }
                }
                RtControlCmd::SuspendForRelease => {
                    let old_alsa = alsa_opt.take().map(|mut a| {
                        let _ = a.drop_frames();
                        a
                    });
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
                    // Control thread publishes `EngineState::Idle` only after full
                    // ALSA close + PipeWire restore/disconnect + ReserveDevice1 drop.
                    shared.set_state(EngineState::Switching);
                }
                RtControlCmd::SuspendForReopen => {
                    if !released_idle && reopen_gate.rt_try_begin_reopen() {
                        let old_alsa = alsa_opt.take().map(|mut a| {
                            let _ = a.drop_frames();
                            a
                        });
                        let old_fds = std::mem::take(&mut alsa_fds);
                        let cur_q = if old_alsa.is_some() {
                            (params.period_frames / active_ratio).max(32)
                        } else {
                            scaled_quantum(config.quantum, active_graph_rate)
                        };
                        let _ = event_stash.push_or_stash(
                            RtEvent::NeedReopen {
                                target_rate: active_graph_rate,
                                target_quantum: cur_q,
                                target_sink: active_sink,
                                old_alsa,
                                old_fds,
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
                }
            }
        }

        // Poll lock-free DSP updates (chain swaps, A/B side, self-tests) on RT thread.
        let is_streaming = alsa_running && !suspended && !released_idle;
        processor.poll_updates(active_sink, active_graph_rate, active_ratio, is_streaming);
        if awaiting_chain_for_resume.is_some() {
            try_complete_resume_gate(
                &mut processor,
                &shared,
                &mut awaiting_chain_for_resume,
                &mut suspended,
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
            let n_dsp = drain_ring(&mut dsp_cons) / channels;
            let n_dir = drain_ring(&mut direct_cons) / channels;
            shared
                .dsp_drained_idle
                .fetch_add(n_dsp as u64, Ordering::Relaxed);
            shared
                .direct_drained_idle
                .fetch_add(n_dir as u64, Ordering::Relaxed);
            let mut fds = [libc::pollfd {
                fd: cmd_efd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            unsafe {
                libc::poll(fds.as_mut_ptr(), 1, 100);
            }
            cmd_efd.drain();
            continue;
        }

        // Determine active sink under v1 policy:
        // Direct owns DAC exclusively whenever it has an active/linked streaming client.
        // Note: PipeWire always invokes `direct_stream.process()` (with silence) when the
        // `audivane-dac` driver group runs, so `direct_cons.slots()` is NOT a client indicator.
        let direct_running = shared.direct_streaming.load(Ordering::Acquire)
            && (shared.direct_client_running.load(Ordering::Acquire)
                || shared.direct_has_links.load(Ordering::Acquire));
        let dsp_running = shared.dsp_streaming.load(Ordering::Acquire);
        let want_active = shared.driving.load(Ordering::Acquire) && (direct_running || dsp_running);

        let target_sink = if direct_running {
            SinkChoice::Direct
        } else if dsp_running {
            SinkChoice::Dsp
        } else {
            active_sink
        };
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
                            &shared.stop,
                            &shared,
                        );
                    }
                    let _ = alsa.drop_frames();
                }
                processor.reset_dsp_state();
                alsa_running = false;
                credit_in_frames = 0;
                shared.alsa_delay_frames.store(0, Ordering::Relaxed);
                shared
                    .dsp_load_bits
                    .store(0.0_f64.to_bits(), Ordering::Relaxed);
                if shared.state() == EngineState::Playing {
                    shared.set_state(EngineState::Ready);
                }
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
            credit_in_frames = 0;
            clock_state.reset();

            let default_q = (params.period_frames / active_ratio).max(32) as u64;
            let (cur_rate, cur_q) = read_target_clock(&shared.io, active_graph_rate, default_q);
            shared.graph_rate.store(cur_rate, Ordering::Release);
            shared.quantum.store(cur_q as u32, Ordering::Release);

            // Check if sink switch (e.g. Direct ratio 1 vs DSP resolved ratio),
            // updated DesiredDspConfig, or rate change requires ALSA reopen.
            if alsa_opt.is_some()
                && decide_rt_reopen(
                    rt_dsp_cfg,
                    last_negotiated_key,
                    active_sink,
                    target_sink,
                    active_graph_rate,
                    cur_rate,
                    active_ratio,
                )
                .is_some()
                && reopen_gate.rt_try_begin_reopen()
                && let Some(mut old_alsa) = alsa_opt.take()
            {
                let _ = old_alsa.drop_frames();
                let old_fds = std::mem::take(&mut alsa_fds);
                let _ = event_stash.push_or_stash(
                    RtEvent::NeedReopen {
                        target_rate: cur_rate,
                        target_quantum: cur_q as u32,
                        target_sink,
                        old_alsa: Some(old_alsa),
                        old_fds,
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
            if alsa.prepare().is_err() {
                let _ = event_stash.push_or_stash(
                    RtEvent::FatalError {
                        msg: "ALSA prepare failed",
                    },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                );
                break;
            }

            let period_out = params.period_frames.max(active_ratio);
            let period_in = (period_out / active_ratio).max(1);
            let aligned_period_out = period_in * active_ratio;
            let n_prefill_periods = (params.buffer_frames / aligned_period_out).max(2);
            // Prime at the nominal graph rate instead of as fast as the graph
            // can complete: consuming several quanta in a burst outruns the
            // source client's converter, which then fills a cycle with silence
            // (an audible dropout right after playback starts).
            let period_ns = (u64::from(period_in) * 1_000_000_000) / u64::from(cur_rate.max(8_000));
            let prime_start_ns = mono_now_ns();
            let mut prime_deadline_ns = prime_start_ns;
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
                pace_until(prime_deadline_ns);
                let sample_len_in = (period_in as usize) * channels;
                let prime_timeout_ms = if first_prime_after_reopen && p == 0 {
                    80
                } else {
                    60
                };
                let cons_slots = match active_sink {
                    SinkChoice::Direct => direct_cons.slots(),
                    _ => dsp_cons.slots(),
                };
                let mut completion_ns = None;
                if cons_slots < sample_len_in {
                    let completed = shared.cycle_counter(active_sink == SinkChoice::Direct);
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
                    let out_slice = &mut out_buf[..(aligned_period_out as usize) * channels];
                    processor.process_block(
                        active_sink,
                        &dsp_in_buf[..sample_len_in],
                        &direct_in_buf[..sample_len_in],
                        out_slice,
                        channels,
                        active_ratio,
                        active_graph_rate,
                    );
                    fade_ramp.apply_interleaved(out_slice, channels);
                    if let Ok(w) = alsa.write_i32_all_until(out_slice, || {
                        shared.stop.load(Ordering::Acquire)
                            || shared.release_requested.load(Ordering::Acquire)
                    }) {
                        shared.frames_out.fetch_add(w as u64, Ordering::Relaxed);
                    }
                } else if let Ok(w) = alsa.write_silence(aligned_period_out) {
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
            let cushion_slots = (period_in as usize) * channels;
            let have_cushion = match active_sink {
                SinkChoice::Direct => direct_cons.slots() >= cushion_slots,
                _ => dsp_cons.slots() >= cushion_slots,
            };
            if !have_cushion {
                pace_until(prime_deadline_ns);
                let completed = shared.cycle_counter(active_sink == SinkChoice::Direct);
                trigger_and_wait(
                    &mut clock_state,
                    raw_stream,
                    cur_rate,
                    cur_q,
                    (params.buffer_frames / active_ratio) as i64,
                    60,
                    completed,
                );
            }
            if alsa.start().is_err() {
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
            shared.set_state(EngineState::Playing);
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
        let rc = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, 200) };
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

        let Some(alsa) = alsa_opt.as_mut() else {
            continue;
        };
        let _ = alsa.poll_revents(&pollfds[..cmd_idx]);

        let (avail, delay) = match alsa.avail_and_delay() {
            Ok(v) => v,
            Err(_) => {
                shared.xruns.store(alsa.xruns(), Ordering::Relaxed);
                alsa_running = false;
                continue;
            }
        };
        shared.alsa_delay_frames.store(delay, Ordering::Relaxed);
        shared.xruns.store(alsa.xruns(), Ordering::Relaxed);

        if alsa.needs_restart() || !alsa.is_running() {
            alsa_running = false;
            continue;
        }

        let default_q = (params.period_frames / active_ratio).max(32) as u64;
        let (cur_rate, cur_q) = read_target_clock(&shared.io, active_graph_rate, default_q);
        shared.graph_rate.store(cur_rate, Ordering::Release);
        shared.quantum.store(cur_q as u32, Ordering::Release);

        // Check if active sink switch, updated DesiredDspConfig, or rate switch requires ALSA reopen.
        let need_reopen = decide_rt_reopen(
            rt_dsp_cfg,
            last_negotiated_key,
            active_sink,
            target_sink,
            active_graph_rate,
            cur_rate,
            active_ratio,
        )
        .is_some();
        if need_reopen && pending_reopen.is_none() {
            pending_reopen = Some((cur_rate, cur_q as u32, target_sink));
            let fade_out_frames = ((params.rate as usize) * FADE_MS) / 1000;
            fade_ramp.start_fade_out(fade_out_frames);
            shared.fade_outs.fetch_add(1, Ordering::Relaxed);
            shared.set_state(EngineState::Switching);
        } else if target_sink != active_sink && pending_reopen.is_none() {
            active_sink = target_sink;
        }

        let period_out = params.period_frames.max(active_ratio);
        let period_in = (period_out / active_ratio).max(1);
        if avail < period_in * active_ratio {
            continue;
        }

        let chunk_in = period_in as usize;
        let chunk_out = (period_in * active_ratio) as usize;
        let in_samples_len = chunk_in * channels;
        let out_samples_len = chunk_out * channels;

        let popped = match active_sink {
            SinkChoice::Direct => {
                let nd = drain_ring(&mut dsp_cons) / channels;
                shared
                    .dsp_drained_unused
                    .fetch_add(nd as u64, Ordering::Relaxed);
                let n = pop_f32_zero_pad(&mut direct_cons, &mut direct_in_buf[..in_samples_len]);
                shared
                    .direct_popped_frames
                    .fetch_add((n / channels) as u64, Ordering::Relaxed);
                n
            }
            _ => {
                let nd = drain_ring(&mut direct_cons) / channels;
                shared
                    .direct_drained_unused
                    .fetch_add(nd as u64, Ordering::Relaxed);
                let n = pop_f32_zero_pad(&mut dsp_cons, &mut dsp_in_buf[..in_samples_len]);
                shared
                    .dsp_popped_frames
                    .fetch_add((n / channels) as u64, Ordering::Relaxed);
                n
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

        let t0 = mono_now_ns();
        processor.process_block(
            active_sink,
            &dsp_in_buf[..in_samples_len],
            &direct_in_buf[..in_samples_len],
            &mut out_buf[..out_samples_len],
            channels,
            active_ratio,
            active_graph_rate,
        );
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

        fade_ramp.apply_interleaved(&mut out_buf[..out_samples_len], channels);

        let write_res = alsa.write_i32_all_until(&out_buf[..out_samples_len], || {
            shared.stop.load(Ordering::Acquire) || shared.release_requested.load(Ordering::Acquire)
        });
        shared.xruns.store(alsa.xruns(), Ordering::Relaxed);

        match write_res {
            Ok(written) => {
                shared
                    .frames_out
                    .fetch_add(written as u64, Ordering::Relaxed);
            }
            Err(_) => {
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
                && let Some(old_alsa) = alsa_opt.take()
            {
                let old_fds = std::mem::take(&mut alsa_fds);
                let _ = event_stash.push_or_stash(
                    RtEvent::NeedReopen {
                        target_rate,
                        target_quantum,
                        target_sink: reopen_sink,
                        old_alsa: Some(old_alsa),
                        old_fds,
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

        credit_in_frames += chunk_in as u64;
        if credit_in_frames >= cur_q {
            let delay_graph = delay / (active_ratio as i64);
            let completed = shared.cycle_counter(active_sink == SinkChoice::Direct);
            while credit_in_frames >= cur_q * 2 {
                trigger_and_wait(
                    &mut clock_state,
                    raw_stream,
                    cur_rate,
                    cur_q,
                    delay_graph,
                    5,
                    completed,
                );
                credit_in_frames -= cur_q;
            }
            clock_state.advance_and_trigger(
                &shared.io,
                raw_stream.0.as_ptr(),
                cur_rate,
                cur_q,
                delay_graph,
            );
            credit_in_frames -= cur_q;
        }
    }

    event_stash.flush(&mut rt_to_ctrl_tx, &ctrl_efd);
    if let Some(mut alsa) = alsa_opt.take() {
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
    ratio: u32,
    graph_rate: u32,
    stop: &AtomicBool,
    shared: &SharedEngineState,
) {
    let period_out = params.period_frames.max(ratio);
    let period_in = (period_out / ratio).max(1);
    let chunk_in = period_in as usize;
    let chunk_out = (period_in * ratio) as usize;
    let in_samples_len = chunk_in * channels;
    let out_samples_len = chunk_out * channels;

    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let rem = match active_sink {
            SinkChoice::Direct => direct_cons.slots(),
            _ => dsp_cons.slots(),
        };
        if rem == 0 || Instant::now() >= deadline || stop.load(Ordering::Acquire) {
            break;
        }
        let Ok(avail) = alsa.avail() else { break };
        if alsa.needs_restart() {
            break;
        }
        if avail < chunk_out as u32 {
            let _ = alsa.wait_for_period(40);
            continue;
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
        );
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
fn drain_ring(cons: &mut rtrb::Consumer<f32>) -> usize {
    let avail = cons.slots();
    if avail > 0
        && let Ok(chunk) = cons.read_chunk(avail)
    {
        chunk.commit_all();
    }
    avail
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
    use super::*;

    #[test]
    fn quantum_scales_across_rate_families() {
        assert_eq!(scaled_quantum(1024, 48_000), 1024);
        assert_eq!(scaled_quantum(1024, 96_000), 2048);
        assert_eq!(scaled_quantum(1024, 192_000), 4096);
        assert_eq!(scaled_quantum(1024, 44_100), 512);
        assert_eq!(scaled_quantum(256, 44_100), 128);
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
        }

        fn open_and_prepare(
            &mut self,
            _device: &str,
            _graph_rate: u32,
            desired_ratio: u32,
            _quantum: u32,
        ) -> std::result::Result<OpenedDevice<Self::Output>, String> {
            Ok(OpenedDevice {
                output: (),
                actual_ratio: desired_ratio,
                output_rate: 48_000 * desired_ratio,
            })
        }

        fn close_output(&mut self, _out: Self::Output) {
            self.closed_outputs += 1;
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
            },
            &shared,
            &mut Vec::new(),
        );
        assert!(!hold);
        shared.clear_error();
        assert!(shared.effective_error().is_none());
    }

    #[test]
    fn release_teardown_timeout_releases_resources_sets_error_and_fails_waiters() {
        let cfg = EngineDriverConfig::default();
        let shared = SharedEngineState::new(&cfg);
        let mut backend = StubBackend {
            released_reservations: 0,
            closed_outputs: 0,
        };
        let mut lifecycle = DeviceLifecycle {
            state: EngineState::Switching,
            last_error: None,
            reservation: Some((0, ())),
            active_device: Some("hw:Loopback,0,0".into()),
            active_graph_rate: 48_000,
            active_ratio: 1,
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
        installed_ratio: u32,
    }

    impl EngineAudioProcessor for ReadyMockProcessor {
        fn process_block(
            &mut self,
            _active_sink: SinkChoice,
            _dsp_in: &[f32],
            _direct_in: &[f32],
            out: &mut [i32],
            _channels: usize,
            _ratio: u32,
            _graph_rate: u32,
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
            _actual_ratio: u32,
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
            actual_ratio: u32,
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
    fn resume_gate_keeps_device_switching_until_required_chain_is_installed() {
        let cfg = EngineDriverConfig::default();
        let shared = SharedEngineState::new(&cfg);
        let (mut rt_tx, mut rt_rx) = rtrb::RingBuffer::<RtEvent>::new(8);
        let mut stash: BoundedEventStash<RtEvent, 16> = BoundedEventStash::new();
        let ctrl_efd = EventFd::new().unwrap();

        let mut proc = ReadyMockProcessor {
            ready_seq: 1,
            installed_ratio: 1,
        };
        let mut awaiting = Some(Some(2));
        let mut suspended = false;

        // Ratio changed to 4x and chain seq=2 is not yet installed.
        let resumed = try_complete_resume_gate(
            &mut proc,
            &shared,
            &mut awaiting,
            &mut suspended,
            SinkChoice::Dsp,
            4,
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
        proc.installed_ratio = 4;
        let resumed = try_complete_resume_gate(
            &mut proc,
            &shared,
            &mut awaiting,
            &mut suspended,
            SinkChoice::Dsp,
            4,
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
}
