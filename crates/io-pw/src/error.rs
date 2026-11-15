//! Error types for `audivane-io-pw`.

use audivane_io_alsa::AlsaError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PwError {
    #[error("ALSA error: {0}")]
    Alsa(#[from] AlsaError),

    #[error("PipeWire error: {0}")]
    PipeWire(#[from] pipewire::Error),

    #[error("SPA error code {0}")]
    Spa(i32),

    #[error("OS I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid driver configuration: {0}")]
    InvalidConfig(String),

    #[error("PipeWire main loop failed during initialization: {0}")]
    Init(String),
}

pub type Result<T> = std::result::Result<T, PwError>;
