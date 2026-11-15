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
use audivane_protocol::ipc::{EngineConfig, RoutingMode};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
}

impl DaemonOptions {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            store: None,
            socket: socket.into(),
            graph: true,
            mpris: true,
            mpris_interval: DEFAULT_INTERVAL,
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

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}
