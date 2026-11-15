//! `audivane-dsp`: the engine daemon (systemd user service).
//!
//! Loads the config, starts the engine, the PipeWire graph watcher, the MPRIS
//! watcher and the control socket, then waits for SIGTERM/SIGINT or an IPC
//! `shutdown` before tearing everything down gracefully (the previous default
//! sink is restored, the device is released and the socket removed).

use audivane_dsp_daemon::api::Engine;
use audivane_dsp_daemon::control::config::ConfigStore;
use audivane_dsp_daemon::control::daemon::{Daemon, DaemonOptions};
use audivane_dsp_daemon::control::ipc;
use audivane_dsp_daemon::engine::RealEngine;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

#[derive(Parser, Debug)]
#[command(
    name = "audivane-dsp",
    version,
    about = "Audivane DSP engine daemon",
    long_about = "Applies EQ, convolution and upsampling to system or per-app audio and \
                  drives the DAC directly. Control it with `avdsp ctl`."
)]
struct Args {
    /// Config file (default: $XDG_CONFIG_HOME/audivane-dsp/config.json).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Control socket (default: $AUDIVANE_DSP_SOCKET or
    /// $XDG_RUNTIME_DIR/audivane-dsp.sock).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Keep running in the foreground. This is implied: the daemon never
    /// forks, so a systemd `Type=simple` unit works.
    #[arg(long)]
    foreground: bool,
    /// Log filter (off, error, warn, info, debug, trace). RUST_LOG wins when
    /// both are set.
    #[arg(long, value_name = "LEVEL", default_value = "info")]
    log_level: String,
}

fn main() {
    let args = Args::parse();

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| args.log_level.clone());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter))
        .format_timestamp_millis()
        .init();

    if let Err(err) = run(args) {
        log::error!("{err:#}");
        eprintln!("audivane-dsp: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let socket = ipc::socket_path(args.socket.as_deref()).map_err(anyhow::Error::msg)?;
    let store = Arc::new(ConfigStore::new(
        args.config
            .clone()
            .unwrap_or_else(ConfigStore::default_path),
    ));
    let loaded = store.load();
    if let Some(warning) = &loaded.warning {
        log::warn!("{warning}");
    }
    let config = loaded.config;
    if config.device.is_empty() {
        anyhow::bail!(
            "no ALSA output device configured (set `device` in {})",
            store.path().display()
        );
    }
    log::info!(
        "audivane-dsp {} starting: device {}, socket {}",
        env!("CARGO_PKG_VERSION"),
        config.device,
        socket.display()
    );
    log::debug!("config {}", store.path().display());

    let engine: Arc<dyn Engine> = match RealEngine::start(&config) {
        Ok(engine) => engine,
        Err(err) => anyhow::bail!("cannot start the engine: {err}"),
    };

    let opts = DaemonOptions::new(socket).with_store(Arc::clone(&store));
    let daemon = match Daemon::start(Arc::clone(&engine), config, opts) {
        Ok(daemon) => daemon,
        Err(err) => {
            // Do not leave the DAC reserved when the control socket is taken.
            engine.shutdown();
            return Err(err);
        }
    };

    install_signal_handlers(Arc::clone(&daemon.shutdown_flag()))?;

    // Block until SIGTERM/SIGINT or `avdsp ctl shutdown`.
    daemon.wait();
    daemon.shutdown();
    Ok(())
}

// ---------------------------------------------------------------- signals

/// Write end of the self-pipe used by the signal handlers.
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_signal: libc::c_int) {
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 0u8;
        // write(2) is async-signal-safe; a full pipe is harmless.
        unsafe {
            libc::write(fd, (&raw const byte).cast(), 1);
        }
    }
}

/// SIGTERM/SIGINT -> shutdown latch. The handler only writes one byte to a
/// pipe; a helper thread does the actual work.
fn install_signal_handlers(
    shutdown: Arc<audivane_dsp_daemon::control::ShutdownFlag>,
) -> std::io::Result<()> {
    let mut fds = [0i32; 2];
    // SAFETY: plain syscall with a valid array of two ints.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    SIGNAL_FD.store(fds[1], Ordering::SeqCst);
    // SAFETY: the handler is async-signal-safe (an atomic load and write(2)).
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let read_fd = fds[0];
    std::thread::Builder::new()
        .name("avdsp-signals".into())
        .spawn(move || {
            let mut byte = 0u8;
            loop {
                // SAFETY: reading into a one-byte buffer from our pipe.
                let n = unsafe { libc::read(read_fd, (&raw mut byte).cast(), 1) };
                if n == 1 {
                    log::info!("signal received, shutting down");
                    shutdown.request();
                    return;
                }
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    log::warn!("signal pipe read failed: {err}");
                    return;
                }
                return;
            }
        })?;
    Ok(())
}
