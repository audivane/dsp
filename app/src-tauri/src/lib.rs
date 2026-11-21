//! Audivane DSP desktop app, Rust side.
//!
//! The modules below the line are plain Rust (no GTK) and are built and
//! tested with the rest of the workspace. The Tauri shell (`app`) needs the
//! `gui` feature and WebKitGTK.

pub mod autostart;
pub mod client;
pub mod dbus_util;
pub mod eq;
pub mod irinfo;
pub mod overlays;
pub mod paths;
pub mod placement;
pub mod plasma_osd;
pub mod shortcuts;
pub mod sni_watch;
pub mod systemd;
pub mod tray_text;
pub mod ui_settings;
pub mod volume_step;
pub mod wheel;
pub mod window_life;

#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
mod tray;
