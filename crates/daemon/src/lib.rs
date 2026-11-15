//! Audivane DSP engine daemon.
//!
//! - [`api`]: the `Engine` trait, the only seam between halves.
//! - [`engine`]: real-time side (ALSA device, PipeWire driver sinks, DSP).
//! - [`control`]: IPC server, config persistence, routing, MPRIS, status.

pub mod api;
pub mod control;
pub mod engine;
