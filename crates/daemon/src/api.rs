//! The seam between the engine (real-time audio: ALSA + PipeWire driver +
//! DSP) and the control plane (IPC, config, routing, MPRIS).
//!
//! The control plane only talks to the engine through [`Engine`]; tests use a
//! mock. The engine never calls back into the control plane.

pub use audivane_io_pw::varispeed_rt::VarispeedHold;
use audivane_io_pw::{DriverGapCounters, SinkVolumeRequest};
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{
    AbSide, AbStatus, BypassResamplerStatus, DeviceCapabilities, EngineConfig, EngineState, Meters,
    OutputsStatus, SelfTestResult, SinkChoice, Spectrum, VarispeedStatus, VolumeStatus,
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
    /// DAC rate / graph rate.
    pub ratio: audivane_protocol::RateRatio,
    pub quantum: u32,
    pub latency_ms: f64,
    /// Filter delay part of `latency_ms`.
    pub filter_latency_ms: f64,
    /// The convolution's own delay (its IRs' bulk delay plus the
    /// convolver's block) at the graph rate, ms; part of
    /// `filter_latency_ms`. 0 without convolution.
    pub conv_latency_ms: f64,
    pub xruns: u64,
    pub dsp_load: f64,
    /// Something processes the samples: the DSP chain (enabled and not
    /// passthrough), a digital volume part, or the engine bypass
    /// resampler's conversion.
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
    /// Every engine rate the device plays with the current settings.
    pub playable_rates: Vec<u32>,
    pub effective_default_rate: Option<u32>,
    /// `chain.output.bits` capped at the negotiated format's width.
    pub dither_bits: Option<u32>,
    /// The engine bypass resampler: converting now, or why not.
    pub bypass_resampler: Option<BypassResamplerStatus>,
    /// Peak RT processing load per period over the last one to two seconds.
    pub rt_load_max: f64,
    /// Applied varispeed state from the audio thread.
    pub varispeed: Option<VarispeedStatus>,
    /// The configured device is not there; the sinks run on the engine's
    /// own clock until it is back.
    pub waiting_for_device: bool,
}

/// Varispeed as the overload fallback sees it ([`Engine::varispeed_request`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VarispeedRequest {
    /// Varispeed is requested (also while the fallback holds it off).
    pub wanted: bool,
    /// The stage runs on the audio thread now.
    pub running: bool,
    /// The requested speed.
    pub speed: f64,
    /// The processing budget the driver derates the speed cap for, share
    /// of the period (0.5 at most).
    pub budget: f64,
    /// Requests accepted so far: a new one ends a varispeed fallback step.
    pub requests: u64,
    /// The sequence of the overload hold the audio thread applies
    /// ([`Engine::set_varispeed_hold`]); a retirement (`Off`) counts once
    /// the stage is gone.
    pub hold_ack: u64,
}

impl Default for VarispeedRequest {
    fn default() -> Self {
        Self {
            wanted: false,
            running: false,
            speed: 1.0,
            budget: 0.5,
            requests: 0,
            hold_ack: 0,
        }
    }
}

/// Implemented by `engine::RealEngine`; all methods are called from
/// non-real-time threads and may block briefly (never for audio-cycle work).
pub trait Engine: Send + Sync + 'static {
    /// Apply a new configuration: rebuild what changed off the RT thread and
    /// swap it in without a gap (EQ/A-B changes) or with a short mute ramp
    /// (device/rate/ratio changes).
    fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError>;
    fn ab_select(&self, side: AbSide, crossfade_ms: f64) -> Result<AbStatus, EngineError>;
    /// Measured A/B level matching: meter both sides of the running pair
    /// for the measurement `epoch` (0 stops). The audio thread picks it up
    /// on its next block; nothing runs without a pair. No-op by default.
    fn set_ab_measure(&self, _epoch: u32) {}
    /// Append the measurement hops the audio thread completed since the
    /// last call to `out`. Nothing by default.
    fn take_ab_hops(&self, _out: &mut Vec<audivane_dsp_core::ab_measure::AbHop>) {}
    fn snapshot(&self) -> EngineSnapshot;
    /// The volume as applied (as in [`EngineSnapshot::volume`]) and a counter
    /// that moves whenever it changes, also without a config change (a staged
    /// hardware raise landing, a mixer readback on an open). The control plane's
    /// level revision counts it. Cheap: no driver round trip.
    fn volume_state(&self) -> (Option<VolumeStatus>, u64) {
        (self.snapshot().volume, 0)
    }
    /// The convolution guards' warnings of the config applied last
    /// ([`audivane_protocol::ipc::Status::conv_warnings`]). None by default.
    fn conv_warnings(&self) -> Vec<audivane_protocol::ipc::ConvWarning> {
        Vec::new()
    }
    /// The enabled convolution items of `convs`, loaded at `rate` as a
    /// chain build loads them (through the engine's IR cache, resampled,
    /// shapes checked), for the response plot. Default: a cache of its own.
    fn load_conv(
        &self,
        convs: &[audivane_protocol::ConvolutionConfig],
        rate: u32,
    ) -> Result<Vec<audivane_dsp_core::matrix::LoadedConv>, EngineError> {
        crate::engine::load_conv_items(convs, &mut crate::engine::ir::IrCache::new(), rate)
    }
    /// Meters since the previous call (peak window resets on read).
    fn meters(&self) -> Meters;
    /// The limiter's held automatic headroom (dB) the audio thread last
    /// reported, 0 when it is not in the path. Does not reset the meters.
    fn limiter_held_db(&self) -> f64 {
        0.0
    }
    /// Latest live spectrum frame, if any has been analysed.
    fn spectrum(&self) -> Option<Spectrum> {
        None
    }
    /// Bumped whenever a new spectrum frame is published.
    fn spectrum_generation(&self) -> u64 {
        0
    }
    /// The harmonics stage's live view: the latest frame, and the analysis
    /// kept on for a few seconds ([`audivane_protocol::ipc::Call::HarmonicsLive`]).
    /// `None`: this engine has none.
    fn harmonics_live(&self) -> Option<audivane_protocol::ipc::HarmonicsLive> {
        None
    }
    /// Stop the live view and forget what it holds (a blind test starts
    /// or ends): the next poll starts afresh.
    fn harmonics_live_suspend(&self) {}
    /// Start/stop copying samples into the spectrum rings. The event pump
    /// calls this from its subscriber set.
    fn set_spectrum_enabled(&self, _enabled: bool) {}
    fn selftest(&self) -> Result<SelfTestResult, EngineError>;
    /// Volume changes made on our sinks from outside (desktop, media keys,
    /// `wpctl`) while `volume.sink_volume` is `follow`. The control plane
    /// takes the receiver once and turns each request into a volume change.
    fn take_sink_volume_requests(&self) -> Option<std::sync::mpsc::Receiver<SinkVolumeRequest>> {
        None
    }
    /// Show the engine's current volume on our sinks again (after a sink
    /// request was refused or reduced).
    fn refresh_sink_volume(&self) {}
    fn release_device(&self) -> Result<(), EngineError>;
    fn acquire_device(&self) -> Result<(), EngineError>;
    /// `true` when the engine can no longer play and has to be started again
    /// (its audio thread ended because the device failed or disappeared).
    fn needs_restart(&self) -> bool {
        false
    }
    fn shutdown(&self);
    /// Multi-device: the master's alignment and each configured follower's
    /// state. `None` (the default): the control plane reports the config
    /// with every follower `off`.
    fn outputs(&self) -> Option<OutputsStatus> {
        None
    }
    /// This machine's measured kernel rates, for the CPU admission of new
    /// upsampler settings (`control::cpu_budget`). `None` (the default, and an
    /// engine that is not running) admits everything.
    fn cpu_rates(&self) -> Option<&'static audivane_dsp_core::cpu_bench::KernelRates> {
        None
    }

    /// Generation of the last config [`Self::apply`] accepted:
    /// [`EngineSnapshot::chain_generation`] reaches it once the audio
    /// thread runs a chain built for it (after a reopen, when one was
    /// needed). The overload fallback confirms its steps with it. 0 (the
    /// default) for an engine whose chain generation does not move.
    fn applied_generation(&self) -> u64 {
        0
    }

    /// The overload fallback's hold on varispeed ([`VarispeedHold`]):
    /// idempotent, never refused. Returns the hold's sequence (a new one
    /// when it changed), which [`VarispeedRequest::hold_ack`] reaches once
    /// the audio thread applies it. No-op by default (no varispeed, 0).
    fn set_varispeed_hold(&self, _hold: VarispeedHold) -> u64 {
        0
    }

    /// The per-rate upsampler plan ([`RatePlan`]): which source rates
    /// build another upsampler than the config's. The engine uses it for
    /// the chains it builds from now on (an edit, a reopen at a new rate);
    /// the chain playing now changes with the next [`Self::apply`]. No-op
    /// by default.
    fn set_rate_plan(&self, _plan: RatePlan) {}

    /// Varispeed's request and whether its stage runs, for the overload
    /// fallback. Default: not requested.
    fn varispeed_request(&self) -> VarispeedRequest {
        VarispeedRequest::default()
    }

    /// Measurement playback (`measure_play`): hand `job` to the master's
    /// audio thread and to every follower it routes to (by
    /// [`crate::engine::measure::MeasureDevice::follower`]), or take it
    /// back from all of them (`None`). Default: no engine to play it.
    fn measure_post(
        &self,
        _job: Option<std::sync::Arc<crate::engine::measure::MeasureJob>>,
    ) -> Result<(), EngineError> {
        Err(EngineError::new(
            audivane_protocol::ipc::RpcError::BAD_STATE,
            "measurements need the audio engine",
        ))
    }

    /// Free what the audio threads handed back from earlier measurement
    /// posts (their references to old jobs). Default: nothing to free.
    fn measure_reclaim(&self) {}

    /// Keep an old measurement job an audio thread still holds, and drop it
    /// off the audio threads once that thread handed it back. Default: no
    /// audio threads, so it can go now.
    fn measure_retire(&self, job: std::sync::Arc<crate::engine::measure::MeasureJob>) {
        drop(job);
    }

    /// Enable or disable varispeed and set its target speed, ramp and Keep
    /// pitch without rebuilding the DSP chain. A new request bumps
    /// [`VarispeedRequest::requests`] so the overload fallback ends any
    /// active varispeed step.
    fn set_varispeed(
        &self,
        _cfg: audivane_protocol::ipc::VarispeedConfig,
    ) -> Result<(), EngineError> {
        Ok(())
    }
}

/// Chain A's upsampler per source rate: at the rates listed, the engine
/// builds the plan's upsampler in place of the requested one, at the same
/// target (so the device lands where the requested one would). Made by the
/// control plane for one requested upsampler and output section
/// ([`crate::control::cpu_budget::plan_rates`]); it applies only while the
/// config the engine runs carries exactly those, with DSP on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RatePlan {
    /// The requested chain A upsampler and output section it was made for.
    pub made_for: Option<(
        audivane_protocol::UpsamplerConfig,
        audivane_protocol::OutputConfig,
    )>,
    /// Source (graph) rate and the upsampler built there instead.
    pub rates: Vec<(u32, audivane_protocol::UpsamplerConfig)>,
}

impl RatePlan {
    /// The upsampler built at `graph_rate` for `cfg` in place of its own,
    /// if the plan replaces it there.
    pub fn substitute(
        &self,
        cfg: &EngineConfig,
        graph_rate: u32,
    ) -> Option<&audivane_protocol::UpsamplerConfig> {
        let (up, out) = self.made_for.as_ref()?;
        if !cfg.dsp_enabled || cfg.chain.upsampler != *up || cfg.chain.output != *out {
            return None;
        }
        self.rates
            .iter()
            .find(|(r, _)| *r == graph_rate)
            .map(|(_, u)| u)
    }

    /// `cfg` as the engine builds it at `graph_rate`.
    pub fn apply<'a>(
        &self,
        cfg: &'a EngineConfig,
        graph_rate: u32,
    ) -> std::borrow::Cow<'a, EngineConfig> {
        match self.substitute(cfg, graph_rate) {
            Some(up) => {
                let mut c = cfg.clone();
                c.chain.upsampler = up.clone();
                std::borrow::Cow::Owned(c)
            }
            None => std::borrow::Cow::Borrowed(cfg),
        }
    }
}
