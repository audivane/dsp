//! Control plane: IPC server, config persistence, PipeWire routing, MPRIS and
//! the status/passthrough logic.
//!
//! The engine (ALSA + PipeWire driver + DSP) is reached only through
//! [`crate::api::Engine`], so everything here is testable with a mock.

pub mod config;
pub mod daemon;
pub mod graph;
pub mod ipc;
pub mod mpris;
pub mod passthrough;
pub mod persist;
pub mod presets;
pub mod service;

pub use daemon::{Daemon, DaemonOptions};
pub use service::{Dispatch, Service, ShutdownFlag};
