//! PipeWire driver `Audio/Sink` node paced by an ALSA hardware DAC clock.

use crate::clock::{
    BoundedEventStash, DriverClockState, EventFd, FadeRamp, SynchronizedIo, mono_now_ns,
    read_target_clock,
};
use crate::error::{PwError, Result};
use audivane_io_alsa::{
    AlsaConfig, AlsaOutput, DEFAULT_RESERVE_PRIORITY, DeviceReservation, NegotiatedParams,
    try_promote_current_thread_rt,
};
use pipewire as pw;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use std::io::Cursor;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PW_STREAM_FLAG_DRIVER_PRIORITY: &str = "300000";
const PW_STREAM_SESSION_PRIORITY: &str = "2000";
const FADE_MS: usize = 25;

/// High-level lifecycle state of the driver sink.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverState {
    Idle = 0,
    Ready = 1,
    Playing = 2,
    Switching = 3,
    Error = 4,
}

impl DriverState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::Ready,
            2 => Self::Playing,
            3 => Self::Switching,
            _ => Self::Error,
        }
    }
}

/// Real-time audio processing hook called on the ALSA output thread to convert
/// `f32` interleaved PipeWire frames into left-justified `i32` interleaved DAC
/// frames (possibly upsampling by `ratio`).
///
/// Implementations must be real-time safe (no heap allocations, locks, or I/O).
pub trait AudioProcessor: Send + 'static {
    fn process_f32_to_i32(
        &mut self,
        in_samples: &[f32],
        out_samples: &mut [i32],
        channels: usize,
        ratio: u32,
    );
}

/// Exact bit-perfect 1:1 converter from PipeWire `F32LE` (`[-1.0, 1.0)`) to
/// left-justified `i32` for `ratio == 1`, and zero-order hold fallback for
/// higher ratios if no DSP resampler is supplied.
#[derive(Debug, Default)]
pub struct PassthroughProcessor;

/// Convert a single PipeWire `f32` sample to a left-justified `i32`.
///
/// PipeWire converts `S24` integers via `x / 8388608.0f` (`2^23`), which is exact
/// in IEEE-754 `f32` across the entire 24-bit signed range. Multiplying by
/// `2^31` in `f64` and rounding recovers `x << 8` bit-for-bit.
#[inline]
pub fn f32_to_left_justified_i32(sample: f32) -> i32 {
    let scaled = (sample as f64) * 2_147_483_648.0;
    scaled.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

impl AudioProcessor for PassthroughProcessor {
    fn process_f32_to_i32(
        &mut self,
        in_samples: &[f32],
        out_samples: &mut [i32],
        channels: usize,
        ratio: u32,
    ) {
        if ratio <= 1 {
            for (dst, &src) in out_samples.iter_mut().zip(in_samples.iter()) {
                *dst = f32_to_left_justified_i32(src);
            }
            return;
        }
        let r = ratio as usize;
        let in_frames = in_samples.len() / channels;
        for f in 0..in_frames {
            let src_base = f * channels;
            for k in 0..r {
                let dst_base = (f * r + k) * channels;
                for ch in 0..channels {
                    out_samples[dst_base + ch] =
                        f32_to_left_justified_i32(in_samples[src_base + ch]);
                }
            }
        }
    }
}

/// Configuration for [`DriverSinkHandle::spawn`].
#[derive(Clone, Debug)]
pub struct DriverConfig {
    /// ALSA `hw:` output configuration. Note `alsa.rate` should normally equal
    /// `source_rate * ratio`.
    pub alsa: AlsaConfig,
    /// Initial PipeWire graph / sink sample rate in Hz (e.g. `48_000`).
    pub source_rate: u32,
    /// Integer output upsampling ratio (`out_rate / source_rate`, e.g. `1` or `4`).
    pub ratio: u32,
    /// PipeWire `node.name` (default `"audivane_dsp"`).
    pub node_name: String,
    /// PipeWire `node.description` (default `"Audivane DSP"`).
    pub node_description: String,
    /// Allowed rates advertised via `audio.allowed-rates`. If empty or length 1,
    /// `EnumFormat` fixes the sink rate to `source_rate`. If > 1, `EnumFormat`
    /// leaves rate unconstrained so PipeWire can switch graph rates.
    pub allowed_rates: Vec<u32>,
    /// Re-open ALSA at `target_rate * ratio` when PipeWire's graph rate changes.
    pub adapt_alsa_rate: bool,
    /// Acquire `org.freedesktop.ReserveDevice1.Audio<card>` before opening ALSA.
    pub reserve_device: bool,
    /// ReserveDevice1 priority (`DEFAULT_RESERVE_PRIORITY = 1000`).
    pub reserve_priority: i32,
    /// Real-time priority requested from `rtkit` for the ALSA thread.
    pub rt_priority: u32,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            alsa: AlsaConfig {
                device: "hw:Loopback,0,0".to_string(),
                rate: 192_000,
                channels: 2,
                period_frames: 1024,
                buffer_periods: 3,
                preferred_format: None,
                strict_format: false,
                nonblock: true,
            },
            source_rate: 48_000,
            ratio: 4,
            node_name: "audivane_dsp".to_string(),
            node_description: "Audivane DSP".to_string(),
            allowed_rates: Vec::new(),
            adapt_alsa_rate: false,
            reserve_device: true,
            reserve_priority: DEFAULT_RESERVE_PRIORITY,
            rt_priority: 20,
        }
    }
}

/// Lock-free telemetry exported from the PipeWire and ALSA RT threads.
#[derive(Debug)]
pub struct DriverMetrics {
    pub graph_rate: AtomicU32,
    pub quantum: AtomicU32,
    pub frames_received: AtomicU64,
    pub frames_written: AtomicU64,
    pub alsa_delay_frames: AtomicI64,
    pub alsa_xruns: AtomicU64,
    pub ring_underruns: AtomicU64,
    pub is_driving: AtomicBool,
    pub is_streaming: AtomicBool,
}

impl DriverMetrics {
    fn new(initial_rate: u32, initial_quantum: u32) -> Self {
        Self {
            graph_rate: AtomicU32::new(initial_rate),
            quantum: AtomicU32::new(initial_quantum),
            frames_received: AtomicU64::new(0),
            frames_written: AtomicU64::new(0),
            alsa_delay_frames: AtomicI64::new(0),
            alsa_xruns: AtomicU64::new(0),
            ring_underruns: AtomicU64::new(0),
            is_driving: AtomicBool::new(false),
            is_streaming: AtomicBool::new(false),
        }
    }

    pub fn snapshot(&self, ratio: u32) -> DriverSnapshot {
        let frames_received = self.frames_received.load(Ordering::Relaxed);
        let frames_written = self.frames_written.load(Ordering::Relaxed);
        let r = ratio.max(1) as f64;
        let drift_frames = (frames_written as f64) / r - (frames_received as f64);
        DriverSnapshot {
            graph_rate: self.graph_rate.load(Ordering::Relaxed),
            quantum: self.quantum.load(Ordering::Relaxed),
            frames_received,
            frames_written,
            alsa_delay_frames: self.alsa_delay_frames.load(Ordering::Relaxed),
            alsa_xruns: self.alsa_xruns.load(Ordering::Relaxed),
            ring_underruns: self.ring_underruns.load(Ordering::Relaxed),
            drift_frames,
            is_driving: self.is_driving.load(Ordering::Acquire),
            is_streaming: self.is_streaming.load(Ordering::Acquire),
        }
    }
}

/// Point-in-time telemetry snapshot.
#[derive(Clone, Copy, Debug)]
pub struct DriverSnapshot {
    pub graph_rate: u32,
    pub quantum: u32,
    pub frames_received: u64,
    pub frames_written: u64,
    pub alsa_delay_frames: i64,
    pub alsa_xruns: u64,
    pub ring_underruns: u64,
    pub drift_frames: f64,
    pub is_driving: bool,
    pub is_streaming: bool,
}

struct SharedControl {
    io: SynchronizedIo,
    streaming: AtomicBool,
    driving: AtomicBool,
    stop: AtomicBool,
    release_requested: AtomicBool,
    driver_state: AtomicU8,
    node_id: AtomicU32,
    cycles_completed: AtomicU64,
    metrics: DriverMetrics,
}

impl SharedControl {
    fn new(initial_rate: u32, initial_quantum: u32) -> Self {
        Self {
            io: SynchronizedIo::new(),
            streaming: AtomicBool::new(false),
            driving: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            release_requested: AtomicBool::new(false),
            driver_state: AtomicU8::new(DriverState::Ready as u8),
            node_id: AtomicU32::new(pw::constants::ID_ANY),
            cycles_completed: AtomicU64::new(0),
            metrics: DriverMetrics::new(initial_rate, initial_quantum),
        }
    }

    fn set_state(&self, state: DriverState) {
        self.driver_state.store(state as u8, Ordering::Release);
    }

    fn state(&self) -> DriverState {
        DriverState::from_u8(self.driver_state.load(Ordering::Acquire))
    }
}

#[derive(Clone, Copy)]
struct SendRawStream(NonNull<pw::sys::pw_stream>);

// Safety: `pw_stream_trigger_process` is thread-safe, and `DriverSinkHandle`
// joins the ALSA thread before `StreamBox` is dropped on the PipeWire thread.
unsafe impl Send for SendRawStream {}
unsafe impl Sync for SendRawStream {}

enum PwControlMsg {
    Disconnect,
    Stop,
}

/// Commands from the non-RT control thread to the ALSA RT thread.
enum RtControlCmd {
    Resume {
        alsa: Box<AlsaOutput>,
        alsa_fds: Vec<libc::pollfd>,
        params: NegotiatedParams,
        graph_rate: u32,
        in_buf: Option<Vec<f32>>,
        out_buf: Option<Vec<i32>>,
        fade_in_frames: usize,
    },
    SuspendForRelease,
}

/// Events from the ALSA RT thread to the non-RT control thread.
enum RtEvent {
    NeedReopen {
        target_rate: u32,
        target_quantum: u32,
        old_alsa: Box<AlsaOutput>,
        old_fds: Vec<libc::pollfd>,
        old_in_buf: Option<Vec<f32>>,
        old_out_buf: Option<Vec<i32>>,
    },
    ReleasedIdle {
        old_alsa: Option<Box<AlsaOutput>>,
        old_fds: Vec<libc::pollfd>,
    },
    RecycleBuffers {
        old_fds: Vec<libc::pollfd>,
        old_in_buf: Option<Vec<f32>>,
        old_out_buf: Option<Vec<i32>>,
    },
    FatalError {
        code: i32,
    },
}

/// Optional test hook parsed once on startup (off-RT) to force a deterministic
/// xrun in rig tests (`AUDIVANE_TEST_STALL_MS`, `AUDIVANE_TEST_STALL_AFTER_MS`).
#[derive(Clone, Copy, Debug)]
struct TestStallConfig {
    stall_ms: u64,
    after_frames_out: u64,
}

impl TestStallConfig {
    fn from_env(output_rate: u32) -> Option<Self> {
        let stall_ms: u64 = std::env::var("AUDIVANE_TEST_STALL_MS").ok()?.parse().ok()?;
        if stall_ms == 0 {
            return None;
        }
        let after_ms: u64 = std::env::var("AUDIVANE_TEST_STALL_AFTER_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1500);
        let after_frames_out = (after_ms.saturating_mul(u64::from(output_rate))) / 1000;
        Some(Self {
            stall_ms,
            after_frames_out,
        })
    }
}

/// Handle to a running Audivane PipeWire driver sink and ALSA output thread.
pub struct DriverSinkHandle {
    config: DriverConfig,
    negotiated_alsa: NegotiatedParams,
    node_id: u32,
    shared: Arc<SharedControl>,
    cmd_efd: Arc<EventFd>,
    ctrl_efd: Arc<EventFd>,
    pw_tx: pw::channel::Sender<PwControlMsg>,
    alsa_thread: Option<JoinHandle<()>>,
    ctrl_thread: Option<JoinHandle<()>>,
    pw_thread: Option<JoinHandle<()>>,
}

impl DriverSinkHandle {
    /// Acquire ALSA + ReserveDevice1, create the PipeWire `Audio/Sink` driver
    /// node, and start the PipeWire, ALSA RT, and non-RT device control threads.
    pub fn spawn<P: AudioProcessor>(config: DriverConfig, processor: P) -> Result<Self> {
        if config.ratio == 0 {
            return Err(PwError::InvalidConfig("ratio must be >= 1".to_string()));
        }
        if config.source_rate == 0 {
            return Err(PwError::InvalidConfig(
                "source_rate must be > 0".to_string(),
            ));
        }

        let ctrl_efd = Arc::new(EventFd::new()?);
        let cmd_efd = Arc::new(EventFd::new()?);
        let data_efd = Arc::new(EventFd::new()?);

        let pw_quantum_hint = (config.alsa.period_frames / config.ratio).max(32);
        let shared = Arc::new(SharedControl::new(config.source_rate, pw_quantum_hint));

        let reservation = if config.reserve_device {
            let wake_shared = Arc::clone(&shared);
            let wake_efd = Arc::clone(&ctrl_efd);
            let wakeup = Arc::new(move || {
                wake_shared.release_requested.store(true, Ordering::Release);
                wake_efd.signal();
            });
            Some(DeviceReservation::acquire_for_device_with_wakeup(
                &config.alsa.device,
                config.reserve_priority,
                wakeup,
            )?)
        } else {
            None
        };

        let mut alsa = Box::new(AlsaOutput::open(&config.alsa)?);
        let negotiated_alsa = alsa.params();
        let alsa_fds = alsa.poll_descriptors()?;
        alsa.prepare()?;
        let test_stall = TestStallConfig::from_env(negotiated_alsa.rate);

        let actual_quantum_hint = (negotiated_alsa.period_frames / config.ratio).max(32);
        shared
            .metrics
            .quantum
            .store(actual_quantum_hint, Ordering::Relaxed);

        // Allocate a generous lock-free ring buffer (at least 16 quanta).
        let ring_frames = (actual_quantum_hint as usize).max(2048) * 16;
        let ring_samples = ring_frames * (negotiated_alsa.channels as usize);
        let (rb_prod, rb_cons) = rtrb::RingBuffer::<f32>::new(ring_samples);

        let (ctrl_to_rt_tx, ctrl_to_rt_rx) = rtrb::RingBuffer::<RtControlCmd>::new(16);
        let (rt_to_ctrl_tx, rt_to_ctrl_rx) = rtrb::RingBuffer::<RtEvent>::new(16);

        let (pw_tx, pw_rx) = pw::channel::channel::<PwControlMsg>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(SendRawStream, u32)>>(1);

        let pw_config = config.clone();
        let pw_shared = Arc::clone(&shared);
        let pw_cmd_efd = Arc::clone(&cmd_efd);
        let pw_data_efd = Arc::clone(&data_efd);
        let pw_channels = negotiated_alsa.channels;
        let pw_quantum = actual_quantum_hint;

        let pw_thread = thread::Builder::new()
            .name("audivane-pw-loop".to_string())
            .spawn(move || {
                run_pw_loop(
                    pw_config,
                    pw_channels,
                    pw_quantum,
                    pw_shared,
                    pw_cmd_efd,
                    pw_data_efd,
                    rb_prod,
                    pw_rx,
                    ready_tx,
                );
            })?;

        let (raw_stream, node_id) = match ready_rx.recv() {
            Ok(Ok(info)) => info,
            Ok(Err(err)) => {
                let _ = pw_tx.send(PwControlMsg::Stop);
                let _ = pw_thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = pw_thread.join();
                return Err(PwError::Init(
                    "PipeWire loop thread exited prematurely".into(),
                ));
            }
        };

        let alsa_config = config.clone();
        let alsa_shared = Arc::clone(&shared);
        let alsa_cmd_efd = Arc::clone(&cmd_efd);
        let alsa_data_efd = Arc::clone(&data_efd);
        let alsa_ctrl_efd = Arc::clone(&ctrl_efd);

        let alsa_thread = thread::Builder::new()
            .name("audivane-alsa-rt".to_string())
            .spawn(move || {
                run_alsa_thread(
                    alsa_config,
                    alsa,
                    alsa_fds,
                    raw_stream,
                    alsa_shared,
                    alsa_cmd_efd,
                    alsa_data_efd,
                    alsa_ctrl_efd,
                    rb_cons,
                    ctrl_to_rt_rx,
                    rt_to_ctrl_tx,
                    processor,
                    test_stall,
                );
            })?;

        let ctrl_config = config.clone();
        let ctrl_shared = Arc::clone(&shared);
        let ctrl_cmd_efd = Arc::clone(&cmd_efd);
        let ctrl_ctrl_efd = Arc::clone(&ctrl_efd);
        let ctrl_pw_tx = pw_tx.clone();

        let ctrl_thread = thread::Builder::new()
            .name("audivane-pw-ctrl".to_string())
            .spawn(move || {
                run_control_thread(
                    ctrl_config,
                    negotiated_alsa.channels as usize,
                    reservation,
                    ctrl_shared,
                    ctrl_cmd_efd,
                    ctrl_ctrl_efd,
                    ctrl_to_rt_tx,
                    rt_to_ctrl_rx,
                    ctrl_pw_tx,
                );
            })?;

        Ok(Self {
            config,
            negotiated_alsa,
            node_id,
            shared,
            cmd_efd,
            ctrl_efd,
            pw_tx,
            alsa_thread: Some(alsa_thread),
            ctrl_thread: Some(ctrl_thread),
            pw_thread: Some(pw_thread),
        })
    }

    /// Negotiated ALSA output hardware parameters at startup.
    pub fn negotiated_alsa(&self) -> NegotiatedParams {
        self.negotiated_alsa
    }

    /// Current driver lifecycle state.
    pub fn state(&self) -> DriverState {
        self.shared.state()
    }

    /// Returns `true` if `ReserveDevice1.RequestRelease` caused the driver to
    /// release ALSA and enter `DriverState::Idle`.
    pub fn is_released(&self) -> bool {
        self.shared.state() == DriverState::Idle
    }

    /// PipeWire node ID assigned to the sink (may be `PW_ID_ANY` before server assignment completes).
    pub fn node_id(&self) -> u32 {
        let id = self.shared.node_id.load(Ordering::Acquire);
        if id != pw::constants::ID_ANY {
            id
        } else {
            self.node_id
        }
    }

    /// Snapshot current telemetry counters.
    pub fn snapshot(&self) -> DriverSnapshot {
        self.shared.metrics.snapshot(self.config.ratio)
    }

    /// Stop the ALSA, control, and PipeWire threads cleanly and release `ReserveDevice1`.
    pub fn stop(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.cmd_efd.signal();
        self.ctrl_efd.signal();
        if let Some(handle) = self.alsa_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.ctrl_thread.take() {
            let _ = handle.join();
        }
        let _ = self.pw_tx.send(PwControlMsg::Stop);
        if let Some(handle) = self.pw_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DriverSinkHandle {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

struct StreamState {
    channels: usize,
    shared: Arc<SharedControl>,
    cmd_efd: Arc<EventFd>,
    data_efd: Arc<EventFd>,
    prod: rtrb::Producer<f32>,
}

#[allow(clippy::too_many_arguments)]
fn run_pw_loop(
    config: DriverConfig,
    channels: u32,
    pw_quantum: u32,
    shared: Arc<SharedControl>,
    cmd_efd: Arc<EventFd>,
    data_efd: Arc<EventFd>,
    prod: rtrb::Producer<f32>,
    pw_rx: pw::channel::Receiver<PwControlMsg>,
    ready_tx: mpsc::SyncSender<Result<(SendRawStream, u32)>>,
) {
    pw::init();

    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => {
            let _ = ready_tx.send(Err(PwError::PipeWire(e)));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(PwError::PipeWire(e)));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(PwError::PipeWire(e)));
            return;
        }
    };

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => config.node_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => config.node_description.as_str(),
        *pw::keys::NODE_DRIVER => "true",
        *pw::keys::PRIORITY_DRIVER => PW_STREAM_FLAG_DRIVER_PRIORITY,
        *pw::keys::PRIORITY_SESSION => PW_STREAM_SESSION_PRIORITY,
    };
    props.insert(
        *pw::keys::NODE_LATENCY,
        format!("{pw_quantum}/{}", config.source_rate),
    );
    props.insert(*pw::keys::NODE_RATE, format!("1/{}", config.source_rate));
    props.insert(*pw::keys::AUDIO_CHANNELS, format!("{channels}"));
    props.insert(*pw::keys::AUDIO_RATE, format!("{}", config.source_rate));

    let multi_rate = config.allowed_rates.len() > 1;
    if !config.allowed_rates.is_empty() {
        let joined: Vec<String> = config.allowed_rates.iter().map(u32::to_string).collect();
        props.insert(
            *pw::keys::AUDIO_ALLOWED_RATES,
            format!("[ {} ]", joined.join(" ")),
        );
    }

    let stream = match pw::stream::StreamRc::new(core.clone(), &config.node_description, props) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(PwError::PipeWire(e)));
            return;
        }
    };

    let state = StreamState {
        channels: channels as usize,
        shared: Arc::clone(&shared),
        cmd_efd: Arc::clone(&cmd_efd),
        data_efd: Arc::clone(&data_efd),
        prod,
    };

    let _listener = match stream
        .add_local_listener_with_user_data(state)
        .state_changed(|stream, data, _old, new| {
            use pw::stream::StreamState as S;
            let nid = stream.node_id();
            if nid != pw::constants::ID_ANY {
                data.shared.node_id.store(nid, Ordering::Release);
            }
            match new {
                S::Streaming => {
                    let driving = stream.is_driving();
                    log::info!("PipeWire stream STREAMING (is_driving={driving})");
                    data.shared.driving.store(driving, Ordering::Release);
                    data.shared
                        .metrics
                        .is_driving
                        .store(driving, Ordering::Release);
                    data.shared.streaming.store(true, Ordering::Release);
                    data.shared
                        .metrics
                        .is_streaming
                        .store(true, Ordering::Release);
                    if data.shared.state() != DriverState::Idle
                        && data.shared.state() != DriverState::Switching
                    {
                        data.shared.set_state(DriverState::Playing);
                    }
                    data.cmd_efd.signal();
                }
                S::Paused => {
                    log::info!("PipeWire stream PAUSED");
                    data.shared.streaming.store(false, Ordering::Release);
                    data.shared
                        .metrics
                        .is_streaming
                        .store(false, Ordering::Release);
                    data.shared.driving.store(false, Ordering::Release);
                    data.shared
                        .metrics
                        .is_driving
                        .store(false, Ordering::Release);
                    if data.shared.state() != DriverState::Idle
                        && data.shared.state() != DriverState::Switching
                    {
                        data.shared.set_state(DriverState::Ready);
                    }
                    data.cmd_efd.signal();
                }
                S::Unconnected => {
                    log::info!("PipeWire stream UNCONNECTED");
                    data.shared.streaming.store(false, Ordering::Release);
                    data.shared
                        .metrics
                        .is_streaming
                        .store(false, Ordering::Release);
                    data.shared.driving.store(false, Ordering::Release);
                    data.shared
                        .metrics
                        .is_driving
                        .store(false, Ordering::Release);
                    // Wait for the ALSA reader to drop any IO pointer before returning.
                    data.shared.io.clear_all();
                    data.cmd_efd.signal();
                }
                S::Error(msg) => {
                    log::error!("PipeWire stream ERROR: {msg}");
                    data.shared.streaming.store(false, Ordering::Release);
                    data.shared
                        .metrics
                        .is_streaming
                        .store(false, Ordering::Release);
                    data.shared.set_state(DriverState::Error);
                    data.cmd_efd.signal();
                }
                _ => {}
            }
        })
        .io_changed(|_stream, data, id, area, size| {
            // `SynchronizedIo::store_*` writes the pointer with SeqCst and waits
            // until the ALSA RT reader has finished any active access before
            // returning to PipeWire, ensuring old mappings stay alive.
            if id == spa::sys::SPA_IO_Position {
                if !area.is_null()
                    && (size as usize) >= std::mem::size_of::<spa::sys::spa_io_position>()
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
        .param_changed(|_stream, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if let Ok((MediaType::Audio, MediaSubtype::Raw)) = format_utils::parse_format(param) {
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    log::info!(
                        "PipeWire sink format negotiated: {:?} rate={} channels={}",
                        info.format(),
                        info.rate(),
                        info.channels()
                    );
                    if info.rate() > 0 {
                        data.shared
                            .metrics
                            .graph_rate
                            .store(info.rate(), Ordering::Relaxed);
                    }
                    if info.channels() > 0 {
                        data.channels = info.channels() as usize;
                    }
                }
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(first) = datas.first_mut() else {
                return;
            };
            let chunk = first.chunk();
            let offset = chunk.offset() as usize;
            let size = chunk.size() as usize;
            let Some(bytes) = first.data() else {
                return;
            };
            let end = offset.saturating_add(size).min(bytes.len());
            let start = offset.min(end);
            let valid = &bytes[start..end];

            let ch = data.channels.max(1);
            let avail_samples = valid.len() / std::mem::size_of::<f32>();
            let frames = avail_samples / ch;
            if frames == 0 {
                return;
            }

            let samples_needed = frames * ch;
            let pushed = push_f32_le_bytes(&mut data.prod, &valid[..samples_needed * 4]);
            let pushed_frames = pushed / ch;
            if pushed_frames > 0 {
                data.shared
                    .metrics
                    .frames_received
                    .fetch_add(pushed_frames as u64, Ordering::Relaxed);
            }
            data.shared.cycles_completed.fetch_add(1, Ordering::Release);
            data.data_efd.signal();
        })
        .register()
    {
        Ok(l) => l,
        Err(e) => {
            let _ = ready_tx.send(Err(PwError::PipeWire(e)));
            return;
        }
    };

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_channels(channels);
    if !multi_rate {
        audio_info.set_rate(config.source_rate);
    }

    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let pod_bytes = match spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    ) {
        Ok((cursor, _)) => cursor.into_inner(),
        Err(e) => {
            let _ = ready_tx.send(Err(PwError::Init(format!("pod serialize failed: {e:?}"))));
            return;
        }
    };
    let mut params = [Pod::from_bytes(&pod_bytes).expect("valid serialized SPA pod")];

    let flags = pw::stream::StreamFlags::DRIVER
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;

    if let Err(e) = stream.connect(spa::utils::Direction::Input, None, flags, &mut params) {
        let _ = ready_tx.send(Err(PwError::PipeWire(e)));
        return;
    }

    let raw = NonNull::new(stream.as_raw_ptr()).expect("stream ptr non-null");
    let node_id = stream.node_id();
    let _ = ready_tx.send(Ok((SendRawStream(raw), node_id)));

    let _rx_guard = pw_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |msg| match msg {
            PwControlMsg::Disconnect => {
                let _ = stream.disconnect();
            }
            PwControlMsg::Stop => mainloop.quit(),
        }
    });

    mainloop.run();
}

#[allow(clippy::too_many_arguments)]
fn run_control_thread(
    config: DriverConfig,
    channels: usize,
    mut reservation: Option<DeviceReservation>,
    shared: Arc<SharedControl>,
    cmd_efd: Arc<EventFd>,
    ctrl_efd: Arc<EventFd>,
    mut ctrl_to_rt_tx: rtrb::Producer<RtControlCmd>,
    mut rt_to_ctrl_rx: rtrb::Consumer<RtEvent>,
    pw_tx: pw::channel::Sender<PwControlMsg>,
) {
    let ratio = config.ratio.max(1);
    let mut in_cap = (config.alsa.buffer_periods.max(2) * config.alsa.period_frames / ratio)
        .max(4096) as usize
        * channels;
    let mut release_sent = false;
    let mut pending_cmds: std::collections::VecDeque<RtControlCmd> =
        std::collections::VecDeque::new();
    let flush_cmds = |pending: &mut std::collections::VecDeque<RtControlCmd>,
                      tx: &mut rtrb::Producer<RtControlCmd>,
                      cmd_efd: &EventFd| {
        let mut pushed = false;
        while let Some(cmd) = pending.pop_front() {
            match tx.push(cmd) {
                Ok(()) => pushed = true,
                Err(rtrb::PushError::Full(returned)) => {
                    pending.push_front(returned);
                    break;
                }
            }
        }
        if pushed {
            cmd_efd.signal();
        }
    };

    while !shared.stop.load(Ordering::Acquire) {
        flush_cmds(&mut pending_cmds, &mut ctrl_to_rt_tx, &cmd_efd);

        // Check ReserveDevice1 release request first.
        let rel_req = shared.release_requested.load(Ordering::Acquire)
            || reservation.as_ref().is_some_and(|r| r.release_requested());
        if rel_req && !release_sent {
            release_sent = true;
            log::info!("ReserveDevice1 release requested: suspending ALSA and yielding device");
            shared.set_state(DriverState::Idle);
            pending_cmds.push_back(RtControlCmd::SuspendForRelease);
            flush_cmds(&mut pending_cmds, &mut ctrl_to_rt_tx, &cmd_efd);
        }

        while let Ok(ev) = rt_to_ctrl_rx.pop() {
            match ev {
                RtEvent::NeedReopen {
                    target_rate,
                    target_quantum,
                    old_alsa,
                    old_fds,
                    old_in_buf,
                    old_out_buf,
                } => {
                    // Close old ALSA handle and free old scratch off the RT thread.
                    drop(old_alsa);
                    drop(old_fds);
                    drop(old_in_buf);
                    drop(old_out_buf);

                    if shared.stop.load(Ordering::Acquire)
                        || shared.release_requested.load(Ordering::Acquire)
                    {
                        continue;
                    }

                    let mut new_cfg = config.alsa.clone();
                    new_cfg.rate = target_rate.saturating_mul(ratio);
                    if target_quantum >= 32 {
                        new_cfg.period_frames = target_quantum.saturating_mul(ratio);
                    }
                    match AlsaOutput::open(&new_cfg) {
                        Ok(mut new_out) => {
                            let new_params = new_out.params();
                            if let Err(e) = new_out.prepare() {
                                log::error!("ALSA prepare after reopen failed: {e}");
                                shared.set_state(DriverState::Error);
                                continue;
                            }
                            let new_fds = new_out.poll_descriptors().unwrap_or_default();
                            let needed_in =
                                (new_params.buffer_frames / ratio).max(4096) as usize * channels;
                            let (in_buf, out_buf) = if needed_in > in_cap {
                                in_cap = needed_in;
                                (
                                    Some(vec![0.0f32; in_cap]),
                                    Some(vec![0i32; in_cap * (ratio as usize)]),
                                )
                            } else {
                                (None, None)
                            };
                            let fade_in_frames = ((new_params.rate as usize) * FADE_MS) / 1000;
                            pending_cmds.push_back(RtControlCmd::Resume {
                                alsa: Box::new(new_out),
                                alsa_fds: new_fds,
                                params: new_params,
                                graph_rate: target_rate,
                                in_buf,
                                out_buf,
                                fade_in_frames,
                            });
                            flush_cmds(&mut pending_cmds, &mut ctrl_to_rt_tx, &cmd_efd);
                        }
                        Err(e) => {
                            log::error!("Failed to reopen ALSA at rate {}: {e}", new_cfg.rate);
                            shared.set_state(DriverState::Error);
                        }
                    }
                }
                RtEvent::ReleasedIdle { old_alsa, old_fds } => {
                    drop(old_alsa);
                    drop(old_fds);
                    drop(reservation.take());
                    let _ = pw_tx.send(PwControlMsg::Disconnect);
                    shared.set_state(DriverState::Idle);
                    log::info!("Device released cleanly to Idle");
                }
                RtEvent::RecycleBuffers {
                    old_fds,
                    old_in_buf,
                    old_out_buf,
                } => {
                    drop(old_fds);
                    drop(old_in_buf);
                    drop(old_out_buf);
                }
                RtEvent::FatalError { code } => {
                    log::error!("ALSA RT thread reported fatal error code {code}");
                    shared.set_state(DriverState::Error);
                }
            }
        }

        flush_cmds(&mut pending_cmds, &mut ctrl_to_rt_tx, &cmd_efd);
        let poll_ms = if pending_cmds.is_empty() { 100 } else { 5 };
        let mut pfd = [libc::pollfd {
            fd: ctrl_efd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        unsafe {
            libc::poll(pfd.as_mut_ptr(), 1, poll_ms);
        }
        ctrl_efd.drain();
    }

    // Drain any returned ALSA handles and drop reservation off-RT.
    while let Ok(ev) = rt_to_ctrl_rx.pop() {
        match ev {
            RtEvent::NeedReopen {
                old_alsa,
                old_fds,
                old_in_buf,
                old_out_buf,
                ..
            } => {
                drop(old_alsa);
                drop(old_fds);
                drop(old_in_buf);
                drop(old_out_buf);
            }
            RtEvent::ReleasedIdle { old_alsa, old_fds } => {
                drop(old_alsa);
                drop(old_fds);
            }
            RtEvent::RecycleBuffers {
                old_fds,
                old_in_buf,
                old_out_buf,
            } => {
                drop(old_fds);
                drop(old_in_buf);
                drop(old_out_buf);
            }
            RtEvent::FatalError { .. } => {}
        }
    }
    drop(reservation.take());
}

#[allow(clippy::too_many_arguments)]
fn run_alsa_thread<P: AudioProcessor>(
    config: DriverConfig,
    alsa_init: Box<AlsaOutput>,
    mut alsa_fds: Vec<libc::pollfd>,
    raw_stream: SendRawStream,
    shared: Arc<SharedControl>,
    cmd_efd: Arc<EventFd>,
    data_efd: Arc<EventFd>,
    ctrl_efd: Arc<EventFd>,
    mut cons: rtrb::Consumer<f32>,
    mut ctrl_to_rt_rx: rtrb::Consumer<RtControlCmd>,
    mut rt_to_ctrl_tx: rtrb::Producer<RtEvent>,
    mut processor: P,
    test_stall: Option<TestStallConfig>,
) {
    try_promote_current_thread_rt(config.rt_priority);

    let mut alsa_opt: Option<Box<AlsaOutput>> = Some(alsa_init);
    let mut params = alsa_opt.as_ref().unwrap().params();
    let ratio = config.ratio.max(1);
    let channels = params.channels as usize;
    let mut active_graph_rate = config.source_rate;

    // Pre-allocate scratch buffers large enough for any single period/buffer chunk.
    let max_in_frames = (params.buffer_frames / ratio).max(4096) as usize;
    let mut in_buf = vec![0.0f32; max_in_frames * channels];
    let mut out_buf = vec![0i32; max_in_frames * (ratio as usize) * channels];
    let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(alsa_fds.len().max(8) + 1);

    let mut clock_state = DriverClockState::new();
    let mut fade_ramp = FadeRamp::unity();
    let mut alsa_running = false;
    let mut suspended_for_reopen = false;
    let mut released_idle = false;
    let mut pending_reopen: Option<(u32, u32)> = None;
    let mut credit_in_frames: u64 = 0;
    let mut first_prime_after_reopen = false;
    let mut stalled_once = false;
    let mut rt_stash: BoundedEventStash<RtEvent, 16> = BoundedEventStash::new();

    let trigger_and_wait = |clock_state: &mut DriverClockState,
                            cur_rate: u32,
                            cur_q: u64,
                            delay_graph: i64,
                            timeout_ms: i32| {
        let before = shared.cycles_completed.load(Ordering::Acquire);
        clock_state.advance_and_trigger(
            &shared.io,
            raw_stream.0.as_ptr(),
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
        while shared.cycles_completed.load(Ordering::Acquire) == before
            && !shared.stop.load(Ordering::Acquire)
        {
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
        rt_stash.flush(&mut rt_to_ctrl_tx, &ctrl_efd);

        // Process control commands without allocating or blocking.
        while let Ok(cmd) = ctrl_to_rt_rx.pop() {
            match cmd {
                RtControlCmd::Resume {
                    alsa,
                    alsa_fds: new_fds,
                    params: new_params,
                    graph_rate,
                    in_buf: new_in,
                    out_buf: new_out,
                    fade_in_frames,
                } => {
                    let old_fds = std::mem::replace(&mut alsa_fds, new_fds);
                    let old_in = new_in.map(|b| std::mem::replace(&mut in_buf, b));
                    let old_out = new_out.map(|b| std::mem::replace(&mut out_buf, b));
                    if (!old_fds.is_empty() || old_in.is_some() || old_out.is_some())
                        && let Err(ev) = rt_stash.push_or_stash(
                            RtEvent::RecycleBuffers {
                                old_fds,
                                old_in_buf: old_in,
                                old_out_buf: old_out,
                            },
                            &mut rt_to_ctrl_tx,
                            &ctrl_efd,
                        )
                    {
                        std::mem::forget(ev);
                    }
                    alsa_opt = Some(alsa);
                    params = new_params;
                    active_graph_rate = graph_rate;
                    suspended_for_reopen = false;
                    released_idle = false;
                    pending_reopen = None;
                    alsa_running = false;
                    first_prime_after_reopen = true;
                    fade_ramp.start_fade_in(fade_in_frames);
                    let streaming_now = shared.streaming.load(Ordering::Acquire);
                    shared.set_state(if streaming_now {
                        DriverState::Playing
                    } else {
                        DriverState::Ready
                    });
                }
                RtControlCmd::SuspendForRelease => {
                    if let Some(mut alsa) = alsa_opt.take() {
                        let _ = alsa.drop_frames();
                        let old_fds = std::mem::take(&mut alsa_fds);
                        if let Err(ev) = rt_stash.push_or_stash(
                            RtEvent::ReleasedIdle {
                                old_alsa: Some(alsa),
                                old_fds,
                            },
                            &mut rt_to_ctrl_tx,
                            &ctrl_efd,
                        ) {
                            std::mem::forget(ev);
                        }
                    } else {
                        let old_fds = std::mem::take(&mut alsa_fds);
                        if let Err(ev) = rt_stash.push_or_stash(
                            RtEvent::ReleasedIdle {
                                old_alsa: None,
                                old_fds,
                            },
                            &mut rt_to_ctrl_tx,
                            &ctrl_efd,
                        ) {
                            std::mem::forget(ev);
                        }
                    }
                    alsa_running = false;
                    suspended_for_reopen = false;
                    pending_reopen = None;
                    released_idle = true;
                    shared.set_state(DriverState::Idle);
                    shared.metrics.alsa_delay_frames.store(0, Ordering::Relaxed);
                }
            }
        }

        if released_idle || suspended_for_reopen || alsa_opt.is_none() {
            drain_ring(&mut cons);
            let poll_ms = if rt_stash.has_pending() { 10 } else { 100 };
            let mut fds = [libc::pollfd {
                fd: cmd_efd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            unsafe {
                libc::poll(fds.as_mut_ptr(), 1, poll_ms);
            }
            cmd_efd.drain();
            continue;
        }

        let want_active =
            shared.streaming.load(Ordering::Acquire) && shared.driving.load(Ordering::Acquire);

        if !want_active {
            if alsa_running {
                if let Some(alsa) = alsa_opt.as_mut() {
                    if !shared.stop.load(Ordering::Acquire) {
                        drain_alsa_tail(
                            alsa,
                            &mut cons,
                            &mut processor,
                            &mut fade_ramp,
                            &mut in_buf,
                            &mut out_buf,
                            params,
                            channels,
                            ratio,
                            &shared.stop,
                        );
                    }
                    let _ = alsa.drop_frames();
                }
                alsa_running = false;
                credit_in_frames = 0;
                shared.metrics.alsa_delay_frames.store(0, Ordering::Relaxed);
                if shared.state() == DriverState::Playing {
                    shared.set_state(DriverState::Ready);
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
            drain_ring(&mut cons);
            data_efd.drain();
            credit_in_frames = 0;
            clock_state.reset();

            let default_q = (params.period_frames / ratio).max(32) as u64;
            let (cur_rate, cur_q) = read_target_clock(&shared.io, active_graph_rate, default_q);
            shared.metrics.graph_rate.store(cur_rate, Ordering::Relaxed);
            shared
                .metrics
                .quantum
                .store(cur_q as u32, Ordering::Relaxed);

            if config.adapt_alsa_rate && cur_rate != active_graph_rate {
                // Suspend immediately (nothing is playing yet) and ask control
                // thread to open ALSA at the new rate off-RT.
                if let Some(mut old_alsa) = alsa_opt.take() {
                    let _ = old_alsa.drop_frames();
                    let old_fds = std::mem::take(&mut alsa_fds);
                    if let Err(ev) = rt_stash.push_or_stash(
                        RtEvent::NeedReopen {
                            target_rate: cur_rate,
                            target_quantum: cur_q as u32,
                            old_alsa,
                            old_fds,
                            old_in_buf: None,
                            old_out_buf: None,
                        },
                        &mut rt_to_ctrl_tx,
                        &ctrl_efd,
                    ) {
                        std::mem::forget(ev);
                    }
                    suspended_for_reopen = true;
                    shared.set_state(DriverState::Switching);
                    continue;
                }
            }

            let Some(alsa) = alsa_opt.as_mut() else {
                continue;
            };

            if alsa.prepare().is_err() {
                if let Err(ev) = rt_stash.push_or_stash(
                    RtEvent::FatalError { code: -1 },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                ) {
                    std::mem::forget(ev);
                }
                break;
            }

            // Bounded priming sequence: prefill `buffer_frames` sequentially,
            // giving the first post-reopen prime cycle 50 ms slack.
            let period_out = params.period_frames.max(ratio);
            let period_in = (period_out / ratio).max(1);
            let aligned_period_out = period_in * ratio;
            let n_prefill_periods = (params.buffer_frames / aligned_period_out).max(2);

            for p in 0..n_prefill_periods {
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                let sample_len_in = (period_in as usize) * channels;
                let prime_timeout_ms = if first_prime_after_reopen && p == 0 {
                    50
                } else {
                    25
                };
                if cons.slots() < sample_len_in {
                    trigger_and_wait(
                        &mut clock_state,
                        cur_rate,
                        cur_q,
                        (p as i64) * (period_in as i64),
                        prime_timeout_ms,
                    );
                }
                if cons.slots() >= sample_len_in {
                    pop_f32_zero_pad(&mut cons, &mut in_buf[..sample_len_in]);
                    let out_slice = &mut out_buf[..(aligned_period_out as usize) * channels];
                    processor.process_f32_to_i32(
                        &in_buf[..sample_len_in],
                        out_slice,
                        channels,
                        ratio,
                    );
                    fade_ramp.apply_interleaved(out_slice, channels);
                    if let Ok(w) = alsa.write_i32_all_until(out_slice, || {
                        shared.stop.load(Ordering::Acquire)
                            || shared.release_requested.load(Ordering::Acquire)
                    }) {
                        shared
                            .metrics
                            .frames_written
                            .fetch_add(w as u64, Ordering::Relaxed);
                    }
                } else if let Ok(w) = alsa.write_silence(aligned_period_out) {
                    shared
                        .metrics
                        .frames_written
                        .fetch_add(w as u64, Ordering::Relaxed);
                }
            }
            first_prime_after_reopen = false;

            if shared.stop.load(Ordering::Acquire) {
                break;
            }
            if alsa.start().is_err() {
                if let Err(ev) = rt_stash.push_or_stash(
                    RtEvent::FatalError { code: -2 },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                ) {
                    std::mem::forget(ev);
                }
                break;
            }
            alsa_running = true;
            shared.set_state(DriverState::Playing);
            clock_state.advance_and_trigger(
                &shared.io,
                raw_stream.0.as_ptr(),
                cur_rate,
                cur_q,
                (params.buffer_frames / ratio) as i64,
            );
            if let Ok(d) = alsa.delay() {
                shared.metrics.alsa_delay_frames.store(d, Ordering::Relaxed);
            }
            continue;
        }

        // Steady state: poll ALSA playback descriptors + cmd_efd.
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
            if let Err(ev) = rt_stash.push_or_stash(
                RtEvent::FatalError {
                    code: err.raw_os_error().unwrap_or(-3),
                },
                &mut rt_to_ctrl_tx,
                &ctrl_efd,
            ) {
                std::mem::forget(ev);
            }
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
                shared
                    .metrics
                    .alsa_xruns
                    .store(alsa.xruns(), Ordering::Relaxed);
                alsa_running = false;
                continue;
            }
        };
        shared
            .metrics
            .alsa_delay_frames
            .store(delay, Ordering::Relaxed);
        shared
            .metrics
            .alsa_xruns
            .store(alsa.xruns(), Ordering::Relaxed);

        // Finding 1 fix: if `avail_and_delay()` recovered from EPIPE/ESTRPIPE,
        // the PCM is back in `State::Prepared` with `start_threshold = boundary`,
        // so we MUST drop back to `!alsa_running` to re-prefill and `start()`.
        if alsa.needs_restart() || !alsa.is_running() {
            alsa_running = false;
            continue;
        }

        let default_q = (params.period_frames / ratio).max(32) as u64;
        let (cur_rate, cur_q) = read_target_clock(&shared.io, active_graph_rate, default_q);
        shared.metrics.graph_rate.store(cur_rate, Ordering::Relaxed);
        shared
            .metrics
            .quantum
            .store(cur_q as u32, Ordering::Relaxed);

        if config.adapt_alsa_rate && cur_rate != active_graph_rate && pending_reopen.is_none() {
            pending_reopen = Some((cur_rate, cur_q as u32));
            let fade_out_frames = ((params.rate as usize) * FADE_MS) / 1000;
            fade_ramp.start_fade_out(fade_out_frames);
            shared.set_state(DriverState::Switching);
        }

        let period_out = params.period_frames.max(ratio);
        let period_in = (period_out / ratio).max(1);
        if avail < period_in * ratio {
            continue;
        }

        let chunk_in = period_in as usize;
        let chunk_out = (period_in * ratio) as usize;
        let in_samples_len = chunk_in * channels;
        let out_samples_len = chunk_out * channels;

        let popped = pop_f32_zero_pad(&mut cons, &mut in_buf[..in_samples_len]);
        if popped < in_samples_len {
            shared
                .metrics
                .ring_underruns
                .fetch_add(1, Ordering::Relaxed);
        }

        if let Some(stall) = test_stall
            && !stalled_once
            && shared.metrics.frames_written.load(Ordering::Relaxed) >= stall.after_frames_out
        {
            stalled_once = true;
            thread::sleep(Duration::from_millis(stall.stall_ms));
        }

        processor.process_f32_to_i32(
            &in_buf[..in_samples_len],
            &mut out_buf[..out_samples_len],
            channels,
            ratio,
        );
        fade_ramp.apply_interleaved(&mut out_buf[..out_samples_len], channels);

        let write_res = alsa.write_i32_all_until(&out_buf[..out_samples_len], || {
            shared.stop.load(Ordering::Acquire) || shared.release_requested.load(Ordering::Acquire)
        });
        shared
            .metrics
            .alsa_xruns
            .store(alsa.xruns(), Ordering::Relaxed);

        match write_res {
            Ok(written) => {
                shared
                    .metrics
                    .frames_written
                    .fetch_add(written as u64, Ordering::Relaxed);
            }
            Err(_) => {
                alsa_running = false;
                continue;
            }
        }

        // Finding 1 fix: if `write_i32_all_until` hit EPIPE and recovered,
        // immediately transition back to prefill + `start()`.
        if alsa.needs_restart() || !alsa.is_running() {
            alsa_running = false;
            continue;
        }

        // Finding 4 fix: once the ~25 ms fade-out finishes, drop ALSA and hand
        // the reopen off to the non-RT control thread.
        if let Some((target_rate, target_quantum)) = pending_reopen
            && fade_ramp.is_fade_out_done()
        {
            let _ = alsa.drop_frames();
            if let Some(old_alsa) = alsa_opt.take() {
                let old_fds = std::mem::take(&mut alsa_fds);
                if let Err(ev) = rt_stash.push_or_stash(
                    RtEvent::NeedReopen {
                        target_rate,
                        target_quantum,
                        old_alsa,
                        old_fds,
                        old_in_buf: None,
                        old_out_buf: None,
                    },
                    &mut rt_to_ctrl_tx,
                    &ctrl_efd,
                ) {
                    std::mem::forget(ev);
                }
                alsa_running = false;
                suspended_for_reopen = true;
                pending_reopen = None;
                continue;
            }
        }

        credit_in_frames += chunk_in as u64;
        if credit_in_frames >= cur_q {
            let delay_graph = delay / (ratio as i64);
            while credit_in_frames >= cur_q * 2 {
                trigger_and_wait(&mut clock_state, cur_rate, cur_q, delay_graph, 5);
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

    if let Some(mut alsa) = alsa_opt.take() {
        if alsa_running {
            let _ = alsa.drop_frames();
        }
        let old_fds = std::mem::take(&mut alsa_fds);
        let _ = rt_stash.push_or_stash(
            RtEvent::ReleasedIdle {
                old_alsa: Some(alsa),
                old_fds,
            },
            &mut rt_to_ctrl_tx,
            &ctrl_efd,
        );
    }
    rt_stash.flush(&mut rt_to_ctrl_tx, &ctrl_efd);
}

#[allow(clippy::too_many_arguments)]
fn drain_alsa_tail<P: AudioProcessor>(
    alsa: &mut AlsaOutput,
    cons: &mut rtrb::Consumer<f32>,
    processor: &mut P,
    fade_ramp: &mut FadeRamp,
    in_buf: &mut [f32],
    out_buf: &mut [i32],
    params: NegotiatedParams,
    channels: usize,
    ratio: u32,
    stop: &AtomicBool,
) {
    let period_out = params.period_frames.max(ratio);
    let period_in = (period_out / ratio).max(1);
    let chunk_in = period_in as usize;
    let chunk_out = (period_in * ratio) as usize;
    let in_samples_len = chunk_in * channels;
    let out_samples_len = chunk_out * channels;

    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    while cons.slots() > 0 && std::time::Instant::now() < deadline && !stop.load(Ordering::Acquire)
    {
        let Ok(avail) = alsa.avail() else { break };
        if alsa.needs_restart() {
            break;
        }
        if avail < chunk_out as u32 {
            let _ = alsa.wait_for_period(50);
            continue;
        }
        pop_f32_zero_pad(cons, &mut in_buf[..in_samples_len]);
        processor.process_f32_to_i32(
            &in_buf[..in_samples_len],
            &mut out_buf[..out_samples_len],
            channels,
            ratio,
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
    while padded_periods < max_wait_periods
        && std::time::Instant::now() < deadline
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
        // Safety: we initialized every element of `first` and `second`.
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
fn drain_ring(cons: &mut rtrb::Consumer<f32>) {
    let avail = cons.slots();
    if avail > 0
        && let Ok(chunk) = cons.read_chunk(avail)
    {
        chunk.commit_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i32_is_bit_exact_for_all_24bit_boundary_and_sampled_values() {
        let test_vals = [
            -8_388_608_i32,
            -8_388_607,
            -4_194_304,
            -123_456,
            -256,
            -1,
            0,
            1,
            256,
            123_456,
            4_194_304,
            8_388_606,
            8_388_607,
        ];
        for s24 in test_vals {
            let f = (s24 as f32) / 8_388_608.0_f32;
            let got = f32_to_left_justified_i32(f);
            assert_eq!(got, s24 << 8, "mismatch for s24={s24}, f={f}");
        }

        let mut s24 = -8_388_608_i32;
        while s24 <= 8_388_607 {
            let f = (s24 as f32) / 8_388_608.0_f32;
            let got = f32_to_left_justified_i32(f);
            assert_eq!(got, s24 << 8, "mismatch at s24={s24}");
            s24 += 97;
        }
    }
}
