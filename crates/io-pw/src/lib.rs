//! PipeWire driver `Audio/Sink` node paced by an ALSA hardware DAC clock.

pub mod clock;
pub mod driver;
pub mod engine_driver;
pub mod error;
pub mod lifecycle;
pub mod metadata;
pub mod rate_arbiter;

pub use driver::{
    AudioProcessor, DriverConfig, DriverMetrics, DriverSinkHandle, DriverSnapshot, DriverState,
    PassthroughProcessor, f32_to_left_justified_i32,
};
pub use engine_driver::{
    DriverGapCounters, DriverWaker, EngineAudioProcessor, EngineDriverConfig, EngineDriverHandle,
    EngineDriverSnapshot, NegotiatedChain, RatioNegotiator, scaled_quantum,
};
pub use error::{PwError, Result};
pub use lifecycle::DesiredDspConfig;
