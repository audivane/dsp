//! Error types for `audivane-io-alsa`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AlsaError {
    #[error("ALSA error: {0}")]
    Alsa(#[from] alsa::Error),

    #[error("D-Bus error: {0}")]
    Dbus(#[from] zbus::Error),

    #[error("D-Bus fdo error: {0}")]
    DbusFdo(#[from] zbus::fdo::Error),

    #[error("invalid ALSA device name '{0}'")]
    InvalidDeviceName(String),

    /// The device rejected a hardware parameter. `rejected` names it (with
    /// the values asked for); `supported` lists what the device accepts.
    #[error("device '{device}' does not support {rejected} (device supports {supported})")]
    Unsupported {
        device: String,
        rejected: String,
        supported: String,
    },

    #[error("device reservation denied for card {card}: current owner refused release")]
    ReservationDenied { card: u32 },

    #[error("device reservation timed out waiting for card {card} release")]
    ReservationTimeout { card: u32 },

    #[error("rtkit promotion failed: {0}")]
    Rtkit(String),

    #[error("invalid sample slice length {len} for {channels} channels")]
    InvalidSampleLength { len: usize, channels: u32 },

    #[error("ALSA write stalled with zero progress")]
    WriteTimeout,

    #[error("ALSA operation interrupted")]
    Interrupted,

    #[error("OS error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AlsaError>;
