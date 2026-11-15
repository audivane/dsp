//! Audivane DSP desktop app, Rust side.
//!
//! The modules below the line are plain Rust (no GTK) and are built and
//! tested with the rest of the workspace. The Tauri shell (`app`) needs the
//! `gui` feature and WebKitGTK.

pub mod client;
pub mod eq;
pub mod irinfo;
pub mod overlays;
pub mod paths;
pub mod systemd;
pub mod tray_text;

#[cfg(feature = "gui")]
pub mod app;
