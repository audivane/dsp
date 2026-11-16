//! The seam between the engine (real-time audio: ALSA + PipeWire driver +
//! DSP) and the control plane (IPC, config, routing, MPRIS).
//!
//! The control plane only talks to the engine through [`Engine`]; tests use a
//! mock. The engine never calls back into the control plane.

use audivane_io_pw::DriverGapCounters;
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{
    AbSide, AbStatus, DeviceCapabilities, EngineConfig, EngineState, Meters, SelfTestResult,
    SinkChoice, Spectrum, VolumeStatus,
};

#[derive(Clone, Debug, PartialEq)]
pub struct EngineError {
    /// One of `audivane_protocol::ipc::RpcError` codes.
    pub code: i32,
    pub message: String,
}

impl EngineError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for EngineError {}

/// What the engine knows about itself. The control plane combines this with
/// PipeWire registry facts (streams, volumes) into `ipc::Status`.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineSnapshot {
    pub state: EngineState,
    pub device: String,
    pub error: Option<String>,
    pub format: Option<SampleFormat>,
    pub graph_rate: u32,
    pub output_rate: u32,
    pub ratio: u32,
    pub quantum: u32,
    pub latency_ms: f64,
    /// Filter delay part of `latency_ms`.
    pub filter_latency_ms: f64,
    pub xruns: u64,
    pub dsp_load: f64,
    /// DSP chain actually active (enabled and not passthrough).
    pub dsp_active: bool,
    pub active_sink: Option<SinkChoice>,
    pub ab: Option<AbStatus>,
    pub volume: Option<VolumeStatus>,
    /// PipeWire node ids of our two sinks, when registered.
    pub dsp_node: Option<u32>,
    pub direct_node: Option<u32>,
    /// Frames handed to ALSA since start.
    pub frames_out: u64,
    /// Config generation currently active on the RT DSP chain.
    pub chain_generation: u64,
    /// Cumulative gap/glitch telemetry from the driver.
    pub gaps: DriverGapCounters,
    /// Probed capabilities of the open device.
    pub device_caps: Option<DeviceCapabilities>,
    /// Graph rates offered to PipeWire (configured ∩ device).
    pub effective_rates: Vec<u32>,
    /// Effective rates the device plays natively (Direct runs only at these).
    pub direct_rates: Vec<u32>,
    pub effective_default_rate: Option<u32>,
    /// `chain.output.bits` capped at the negotiated format's width.
    pub dither_bits: Option<u32>,
}

/// Implemented by `engine::RealEngine`; all methods are called from
/// non-real-time threads and may block briefly (never for audio-cycle work).
pub trait Engine: Send + Sync + 'static {
    /// Apply a new configuration: rebuild what changed off the RT thread and
    /// swap it in without a gap (EQ/A-B changes) or with a short mute ramp
    /// (device/rate/ratio changes).
    fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError>;
    fn ab_select(&self, side: AbSide, crossfade_ms: f64) -> Result<AbStatus, EngineError>;
    fn snapshot(&self) -> EngineSnapshot;
    /// Meters since the previous call (peak window resets on read).
    fn meters(&self) -> Meters;
    /// Latest live spectrum frame, if any has been analysed.
    fn spectrum(&self) -> Option<Spectrum> {
        None
    }
    /// Bumped whenever a new spectrum frame is published.
    fn spectrum_generation(&self) -> u64 {
        0
    }
    /// Start/stop copying samples into the spectrum rings. The event pump
    /// calls this from its subscriber set.
    fn set_spectrum_enabled(&self, _enabled: bool) {}
    fn selftest(&self) -> Result<SelfTestResult, EngineError>;
    fn release_device(&self) -> Result<(), EngineError>;
    fn acquire_device(&self) -> Result<(), EngineError>;
    /// `true` when the engine can no longer play and has to be started again
    /// (its audio thread ended because the device failed or disappeared).
    fn needs_restart(&self) -> bool {
        false
    }
    fn shutdown(&self);
}
