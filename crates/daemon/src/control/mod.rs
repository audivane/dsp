//! Control plane: IPC server, config persistence, PipeWire routing, MPRIS and
//! the status/passthrough logic.
//!
//! The engine (ALSA + PipeWire driver + DSP) is reached only through
//! [`crate::api::Engine`], so everything here is testable with a mock.

pub mod ab_measure;
pub mod ab_side;
pub mod calibration;
pub mod config;
pub mod conv_response;
pub mod cpu_budget;
pub mod daemon;
pub mod device_identity;
pub mod follower_chain;
pub mod graph;
pub mod ipc;
pub mod measure;
pub mod mpris;
pub mod overload;
pub mod passthrough;
pub mod persist;
pub mod presets;
pub mod service;
pub mod volume_guard;

pub use daemon::{Daemon, DaemonOptions};
pub use service::{Dispatch, Service, ShutdownFlag};
