//! PipeWire driver `Audio/Sink` node paced by an ALSA hardware DAC clock.

pub mod clock;
pub mod driver;
pub mod engine_driver;
pub mod error;
pub mod lifecycle;
pub mod metadata;
pub mod rate_arbiter;
pub mod rate_caps;
pub mod rt_context;
mod rt_overload;
pub mod sink_volume;
pub mod varispeed_loop;
pub mod varispeed_rt;
pub mod varispeed_sched;

pub use driver::{
    AudioProcessor, DriverConfig, DriverMetrics, DriverSinkHandle, DriverSnapshot, DriverState,
    PassthroughProcessor, f32_to_left_justified_i32,
};
pub use engine_driver::{
    BypassNotifier, DriverGapCounters, DriverWaker, DspLanding, EngineAudioProcessor,
    EngineDriverConfig, EngineDriverHandle, EngineDriverSnapshot, NegotiatedChain, RatioNegotiator,
    ReleaseHook, ReleaseKind, SinkVolumePort, device_present, predicted_dsp_landing,
    predicted_dsp_landings, scaled_quantum, waiting_text,
};
pub use error::{PwError, Result};
pub use lifecycle::{
    BypassHeld, BypassLeg, BypassMiss, BypassNote, DesiredDspConfig, RatePairMask,
};
pub use sink_volume::{SinkVolumeRequest, SinkVolumeView};
