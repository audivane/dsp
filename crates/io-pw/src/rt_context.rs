//! PipeWire contexts that keep the engine's `RLIMIT_RTTIME` soft limit.
//!
//! The overload guard ([`audivane_io_alsa::rt`]) needs the soft limit at
//! half the hard one, so `SIGXCPU` comes before the kill. PipeWire's RT
//! module (`libpipewire-module-rt`), when a context loads it, sets
//! `RLIMIT_RTTIME` for the whole process from its own thread: through rtkit
//! it sets both limits to rtkit's `RTTimeUSecMax`. PipeWire 1.4.0 and later
//! load it from `client.conf` (1.0.5 and 1.2.8 only from `client-rt.conf`,
//! which these contexts do not name) and merge the config section
//! `module.rt.args` into its arguments: `rt.time.soft` and `rt.time.hard`,
//! in microseconds, clamped to rtkit's maximum. `rlimits.enabled = false`
//! does not help: it does not stop the rtkit path.
//!
//! A section cannot be passed as a context property, so [`new_context`]
//! writes a one-section config file, `$XDG_RUNTIME_DIR/audivane-dsp/
//! pipewire/rt-limits.conf`, named by `config.override.prefix` and
//! `config.override.name` (understood by 1.0.5 too, where nothing reads the
//! section): the module then sets the engine's own limits and the data
//! threads stay real-time. The control thread's periodic repair
//! (`restore_rttime_limits`) stays as the second line.

use pipewire as pw;
use std::path::PathBuf;

/// File name of the override config.
pub const OVERRIDE_NAME: &str = "rt-limits.conf";

/// Directory of the override config.
fn override_dir() -> Option<PathBuf> {
    let dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    if dir.trim().is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join("audivane-dsp").join("pipewire"))
}

/// The override config for the limits `(soft, hard)` in microseconds.
pub fn override_config(soft: u64, hard: u64) -> String {
    format!(
        "# Written by audivane-dsp for its own PipeWire contexts: the RT module\n\
         # keeps RLIMIT_RTTIME soft below hard (see rt_context.rs).\n\
         module.rt.args = {{\n    rt.time.soft = {soft}\n    rt.time.hard = {hard}\n}}\n"
    )
}

/// Write the override config into `dir` (atomically; unchanged files are
/// left alone).
pub fn write_override(dir: &std::path::Path, soft: u64, hard: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let body = override_config(soft, hard);
    let path = dir.join(OVERRIDE_NAME);
    if std::fs::read_to_string(&path).is_ok_and(|cur| cur == body) {
        return Ok(());
    }
    let tmp = dir.join(format!(".{OVERRIDE_NAME}.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)
}

/// Context properties that make the RT module keep the engine's limits,
/// with the override config written into `dir`. `None` (and a log line)
/// when there are no limits to keep or the file cannot be written.
pub fn context_properties_in(dir: Option<PathBuf>) -> Option<pw::properties::PropertiesBox> {
    let (soft, hard) = audivane_io_alsa::rttime_limits_usec()?;
    let dir = dir?;
    if let Err(err) = write_override(&dir, soft, hard) {
        log::warn!(
            "cannot write the PipeWire RT override in {}: {err}; the soft RLIMIT_RTTIME is \
             repaired after the fact",
            dir.display()
        );
        return None;
    }
    let mut props = pw::properties::PropertiesBox::new();
    props.insert("config.override.prefix", dir.to_string_lossy().as_ref());
    props.insert("config.override.name", OVERRIDE_NAME);
    Some(props)
}

/// A PipeWire context whose RT module sets the engine's `RLIMIT_RTTIME`
/// (soft at half the hard limit) instead of soft = hard. Every context the
/// engine and the daemon create goes through it.
pub fn new_context<T: pw::loop_::IsLoopRc>(
    loop_: &T,
) -> std::result::Result<pw::context::ContextRc, pw::Error> {
    match context_properties_in(override_dir()) {
        Some(props) => match pw::context::ContextRc::new(loop_, Some(props)) {
            Ok(context) => Ok(context),
            Err(err) => {
                log::warn!("PipeWire context with the RT override failed ({err}); without it");
                pw::context::ContextRc::new(loop_, None)
            }
        },
        None => pw::context::ContextRc::new(loop_, None),
    }
}
