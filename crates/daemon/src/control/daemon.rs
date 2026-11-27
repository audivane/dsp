//! Wires the control plane together.
//!
//! [`Daemon::start`] builds the [`Service`], the graph watcher, the MPRIS
//! watcher and the IPC server around an [`Engine`] implementation. The
//! `audivane-dsp` binary uses it with `RealEngine`; integration tests use the
//! same code with a mock engine.

use crate::api::Engine;
use crate::control::ShutdownFlag;
use crate::control::config::ConfigStore;
use crate::control::graph::{GraphOptions, GraphWatcher};
use crate::control::ipc::IpcServer;
use crate::control::mpris::{DEFAULT_INTERVAL, MprisWatcher};
use crate::control::service::Service;
use audivane_protocol::ipc::{DeviceLostAction, EngineConfig, EngineState, RoutingMode};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Everything [`Daemon::start`] needs besides the engine.
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// Where config changes are persisted. `None` keeps changes in memory only
    /// (used by tests).
    pub store: Option<Arc<ConfigStore>>,
    pub socket: PathBuf,
    /// Start the PipeWire graph watcher/router. Off when there is no session.
    pub graph: bool,
    /// Start the MPRIS watcher.
    pub mpris: bool,
    pub mpris_interval: Duration,
    /// Watch the sound cards so each output follows its USB device
    /// ([`crate::control::device_identity`]).
    pub device_identity: bool,
}

impl DaemonOptions {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            store: None,
            socket: socket.into(),
            graph: true,
            mpris: true,
            mpris_interval: DEFAULT_INTERVAL,
            device_identity: true,
        }
    }

    pub fn with_store(mut self, store: Arc<ConfigStore>) -> Self {
        self.store = Some(store);
        self
    }
}

/// A fully started control plane.
pub struct Daemon {
    service: Arc<Service>,
    server: Arc<IpcServer>,
    graph: Option<Arc<GraphWatcher>>,
    mpris: Option<Arc<MprisWatcher>>,
    shutdown: Arc<ShutdownFlag>,
    engine: Arc<dyn Engine>,
    stopped: AtomicBool,
    volume_stop: Arc<AtomicBool>,
    volume_thread: Mutex<Option<JoinHandle<()>>>,
    overload_thread: Mutex<Option<JoinHandle<()>>>,
    device_thread: Mutex<Option<JoinHandle<()>>>,
    identity_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Daemon {
    /// Start every control-plane component. Fails only when the IPC socket
    /// cannot be bound (e.g. another daemon is running); missing PipeWire or
    /// D-Bus only disables the corresponding watcher and is logged.
    pub fn start(
        engine: Arc<dyn Engine>,
        config: EngineConfig,
        opts: DaemonOptions,
    ) -> anyhow::Result<Self> {
        let shutdown = ShutdownFlag::new();

        // Fail fast (before touching PipeWire, the bus or the default sink)
        // when another daemon owns the socket. `IpcServer::serve` repeats this
        // check race-free.
        if crate::control::ipc::probe(&opts.socket) {
            anyhow::bail!(
                "another audivane-dsp daemon is already listening on {}",
                opts.socket.display()
            );
        }

        let graph = if opts.graph {
            let graph_opts = GraphOptions {
                rules: match &config.routing {
                    RoutingMode::Apps { rules } => rules.clone(),
                    RoutingMode::System => Vec::new(),
                },
                system_default: matches!(config.routing, RoutingMode::System),
                ..GraphOptions::default()
            };
            match GraphWatcher::spawn(graph_opts) {
                Ok(graph) => Some(graph),
                Err(err) => {
                    log::warn!("PipeWire graph watcher disabled: {err:#}");
                    None
                }
            }
        } else {
            None
        };

        let mpris = if opts.mpris {
            Some(MprisWatcher::spawn_with_interval(opts.mpris_interval))
        } else {
            None
        };

        let service = Service::new(
            Arc::clone(&engine),
            config,
            opts.store,
            graph.clone(),
            mpris.clone(),
            Arc::clone(&shutdown),
        );
        // The engine was already started with this config; make sure routing
        // matches it (the graph watcher may have just taken the default sink).
        service.refresh();

        let volume_stop = Arc::new(AtomicBool::new(false));
        let volume_thread = engine
            .take_sink_volume_requests()
            .and_then(|rx| spawn_volume_thread(Arc::clone(&service), rx, Arc::clone(&volume_stop)));

        let overload_thread = spawn_overload_thread(Arc::clone(&service), Arc::clone(&volume_stop));
        let device_thread = if opts.mpris {
            spawn_device_thread(Arc::clone(&service), Arc::clone(&volume_stop))
        } else {
            None
        };

        let identity_thread = if opts.device_identity {
            spawn_identity_thread(Arc::clone(&service), Arc::clone(&volume_stop))
        } else {
            None
        };

        let server = IpcServer::serve(Arc::clone(&service), opts.socket)?;
        log::info!(
            "listening on {} (device {})",
            server.path().display(),
            service.config().device
        );

        Ok(Self {
            service,
            server,
            graph,
            mpris,
            shutdown,
            engine,
            stopped: AtomicBool::new(false),
            volume_stop,
            volume_thread: Mutex::new(volume_thread),
            overload_thread: Mutex::new(overload_thread),
            device_thread: Mutex::new(device_thread),
            identity_thread: Mutex::new(identity_thread),
        })
    }

    pub fn service(&self) -> Arc<Service> {
        Arc::clone(&self.service)
    }

    pub fn socket_path(&self) -> &Path {
        self.server.path()
    }

    pub fn shutdown_flag(&self) -> Arc<ShutdownFlag> {
        Arc::clone(&self.shutdown)
    }

    pub fn graph(&self) -> Option<&Arc<GraphWatcher>> {
        self.graph.as_ref()
    }

    /// Block until a shutdown is requested (IPC `shutdown`, signal, ...).
    pub fn wait(&self) {
        self.shutdown.wait();
    }

    /// Block until a shutdown is requested or `timeout` elapses.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        self.shutdown.wait_timeout(timeout)
    }

    pub fn is_stopping(&self) -> bool {
        self.shutdown.is_requested()
    }

    /// Shut everything down: stop admitting requests, drain active dispatches,
    /// flush queued config writes, give the default sink back, stop the
    /// watchers, close the engine, and join the persistence worker.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("shutting down");
        self.service.begin_shutdown();
        self.server.shutdown();
        self.service.drain_dispatches();
        self.volume_stop.store(true, Ordering::Release);
        if let Some(thread) = self.volume_thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = thread.join();
        }
        if let Some(thread) = self.overload_thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = thread.join();
        }
        if let Some(thread) = self.device_thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = thread.join();
        }
        if let Some(thread) = self.identity_thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = thread.join();
        }
        // Every exit path (IPC `shutdown`, SIGTERM/SIGINT, main returning)
        // ends here: the last config change must reach disk before we go.
        if let Err(err) = self.service.flush_persist() {
            log::error!("{}", err.message);
        }
        if let Some(mpris) = &self.mpris {
            mpris.shutdown();
        }
        if let Some(graph) = &self.graph {
            // Restores the previous default sink on the way out.
            graph.shutdown();
        }
        self.engine.shutdown();
        self.service.stop_persist();
    }
}

/// How long the volume thread waits for a request when no guarded rise is
/// pending (it also checks for shutdown this often).
const VOLUME_IDLE_WAIT: Duration = Duration::from_millis(200);

/// Turns sink volume requests from the engine into volume changes, and steps
/// guarded rises.
fn spawn_volume_thread(
    service: Arc<Service>,
    rx: mpsc::Receiver<audivane_io_pw::SinkVolumeRequest>,
    stop: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("avdsp-volume".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let wait = if service.volume_slewing() {
                    crate::control::volume_guard::SLEW_TICK
                } else {
                    VOLUME_IDLE_WAIT
                };
                match rx.recv_timeout(wait) {
                    Ok(request) => service.on_sink_volume(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if service.volume_slewing() {
                    service.volume_tick();
                }
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!("sink volume thread disabled: {err}");
            None
        }
    }
}

/// Samples the engine for the DSP overload fallback
/// ([`crate::control::overload`]) until `stop`.
fn spawn_overload_thread(service: Arc<Service>, stop: Arc<AtomicBool>) -> Option<JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("avdsp-overload".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(crate::control::overload::TICK);
                service.overload_tick();
                crate::engine::workers::tick();
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!("DSP overload fallback disabled: {err}");
            None
        }
    }
}

/// How often the identity watch looks at the sound cards.
const IDENTITY_TICK: Duration = Duration::from_millis(500);

/// Follows each output to its USB device as cards come and go
/// ([`Service::identity_tick`]) until `stop`.
fn spawn_identity_thread(service: Arc<Service>, stop: Arc<AtomicBool>) -> Option<JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("avdsp-identity".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                service.identity_tick();
                std::thread::sleep(IDENTITY_TICK);
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!("device identity watch disabled: {err}");
            None
        }
    }
}

/// How often the device watch looks at the engine.
const DEVICE_WATCH_TICK: Duration = Duration::from_millis(200);

/// When the output device goes while something plays through Audivane:
/// pause the players (MPRIS) if `device_lost` is `pause`. Only the
/// transition into waiting counts, so a start-up without the device, or a
/// device that stays away, pauses nothing.
#[derive(Debug, Default)]
pub struct LossPause {
    waiting: bool,
    last_playing: Option<Instant>,
}

impl LossPause {
    /// Audio counts as playing through the loss when the engine played
    /// this recently before it started waiting.
    pub const PLAYED_WITHIN: Duration = Duration::from_secs(3);

    /// One look at the engine: `true` when the players should be paused
    /// now.
    pub fn observe(
        &mut self,
        state: EngineState,
        waiting: bool,
        action: DeviceLostAction,
        now: Instant,
    ) -> bool {
        if state == EngineState::Playing {
            self.last_playing = Some(now);
        }
        let went = waiting && !self.waiting;
        self.waiting = waiting;
        went && action == DeviceLostAction::Pause
            && self
                .last_playing
                .is_some_and(|at| now.duration_since(at) <= Self::PLAYED_WITHIN)
    }
}

/// Watches for the output device going away ([`LossPause`]) until `stop`.
fn spawn_device_thread(service: Arc<Service>, stop: Arc<AtomicBool>) -> Option<JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("avdsp-device".into())
        .spawn(move || {
            let mut watch = LossPause::default();
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(DEVICE_WATCH_TICK);
                let snap = service.engine().snapshot();
                let action = service.config().device_lost;
                if watch.observe(snap.state, snap.waiting_for_device, action, Instant::now()) {
                    // The streams still linked to our sinks: their players
                    // are the ones to pause (the sinks stay up meanwhile).
                    let streams = service.graph().map(|graph| {
                        graph
                            .facts()
                            .streams
                            .into_iter()
                            .filter(|s| s.sink.is_some())
                            .map(|s| crate::control::mpris::StreamOwner {
                                pid: s.pid,
                                names: s.binary.into_iter().chain(s.app_name).collect(),
                            })
                            .collect::<Vec<_>>()
                    });
                    let paused = crate::control::mpris::pause_playing(streams.as_deref());
                    if paused.is_empty() {
                        log::info!("output device gone; no MPRIS player to pause");
                    } else {
                        log::info!("output device gone; paused {}", paused.join(", "));
                    }
                }
            }
        });
    match spawned {
        Ok(handle) => Some(handle),
        Err(err) => {
            log::warn!("device watch disabled: {err}");
            None
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn players_are_paused_once_when_the_device_goes_while_playing() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let pause = DeviceLostAction::Pause;
        let mut watch = LossPause::default();
        assert!(!watch.observe(EngineState::Playing, false, pause, at(0)));
        // Unplugged: the reopen fails a moment later.
        assert!(!watch.observe(EngineState::Switching, false, pause, at(200)));
        assert!(watch.observe(EngineState::Error, true, pause, at(400)));
        // Still away: nothing more.
        assert!(!watch.observe(EngineState::Error, true, pause, at(600)));
        // Back, then gone again while idle: no pause.
        assert!(!watch.observe(EngineState::Ready, false, pause, at(10_000)));
        assert!(!watch.observe(EngineState::Error, true, pause, at(20_000)));
    }
}
