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

impl AlsaError {
    /// Whether the device refused for good: the parameter negotiation
    /// confirmed every candidate is outside its configuration space
    /// (`EINVAL` from a setter or the install step, or a rate it rounds
    /// away), reported as [`Self::Unsupported`]. Any other error (busy, try
    /// again, gone during a hot-plug, an I/O error or a timeout, from any
    /// step) is one a later attempt may get past.
    pub fn is_capability(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_errors_are_told_from_transient_ones() {
        let unsupported = AlsaError::Unsupported {
            device: "hw:0".into(),
            rejected: "rate 288000 Hz".into(),
            supported: "48000 Hz".into(),
        };
        assert!(unsupported.is_capability());
        let alsa = |errno| AlsaError::Alsa(alsa::Error::new("snd_pcm_hw_params", errno));
        // A raw errno is not a confirmed rejection, EINVAL included.
        for errno in [libc::EINVAL, -libc::EINVAL] {
            assert!(!alsa(errno).is_capability(), "{errno}");
        }
        for errno in [
            libc::EBUSY,
            libc::EAGAIN,
            libc::ENODEV,
            libc::ETIMEDOUT,
            libc::EPERM,
        ] {
            assert!(!alsa(errno).is_capability(), "{errno}");
        }
        assert!(!AlsaError::WriteTimeout.is_capability());
    }
}
