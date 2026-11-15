//! Coalesced persistence of `config.json`.
//!
//! Mutations apply to the engine immediately and reply at once; the disk write
//! is decoupled from the request so live EQ dragging (~20–30 `set_eq`/s) costs
//! one small write instead of thirty. [`Persister`] owns a writer thread:
//!
//! - at most one write per [`MIN_INTERVAL`] (~300 ms), so a burst of edits
//!   coalesces into the last state;
//! - a queued change is never left unwritten for longer than [`MAX_DELAY`]
//!   (~2 s), measured from the first change of the burst (the writer skips the
//!   interval wait when a change has waited that long);
//! - [`Persister::flush`] writes whatever is queued *now*, blocking; the daemon
//!   calls it from `shutdown` (IPC, SIGTERM/SIGINT and normal exit all end
//!   there).
//!
//! Writes are atomic ([`crate::control::config`]: temp file, fsync, rename,
//! directory fsync). A failed write is logged and recorded; it surfaces in
//! `Status.error` (when the engine has no error of its own) instead of failing
//! the mutation that queued it, because the reply was already sent.
//!
//! Preset files do not use this: they are explicit user actions and are
//! written synchronously.

use crate::control::config::ConfigStore;
use audivane_protocol::ipc::EngineConfig;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Minimum time between two config writes.
pub const MIN_INTERVAL: Duration = Duration::from_millis(300);
/// Longest a queued change may wait for disk, measured from its arrival.
pub const MAX_DELAY: Duration = Duration::from_secs(2);

struct State {
    pending: Option<EngineConfig>,
    dirty_since: Option<Instant>,
    last_write: Option<Instant>,
    last_error: Option<String>,
    stopped: bool,
}

pub struct Persister {
    store: Arc<ConfigStore>,
    state: Mutex<State>,
    cv: Condvar,
    /// Serializes the actual disk writes (writer thread and `flush`).
    write_lock: Mutex<()>,
    worker: Mutex<Option<JoinHandle<()>>>,
    writes: AtomicU64,
}

impl Persister {
    /// Start a writer thread for `store`.
    pub fn new(store: Arc<ConfigStore>) -> Arc<Self> {
        let persister = Arc::new(Self {
            store,
            state: Mutex::new(State {
                pending: None,
                dirty_since: None,
                last_write: None,
                last_error: None,
                stopped: false,
            }),
            cv: Condvar::new(),
            write_lock: Mutex::new(()),
            worker: Mutex::new(None),
            writes: AtomicU64::new(0),
        });
        let worker = Arc::clone(&persister);
        match thread::Builder::new()
            .name("avdsp-persist".into())
            .spawn(move || worker.run())
        {
            Ok(handle) => {
                *persister.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            }
            Err(err) => {
                // Without the thread every change waits for `flush` (shutdown), so
                // the daemon still works; say so loudly.
                log::error!("cannot start the config writer thread: {err}");
            }
        }
        persister
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queue `cfg`; the writer thread picks up the latest state.
    pub fn mark(&self, cfg: &EngineConfig) {
        let mut state = self.lock();
        if state.pending.is_none() {
            state.dirty_since = Some(Instant::now());
        }
        state.pending = Some(cfg.clone());
        self.cv.notify_all();
    }

    /// Number of successful writes (used by tests and diagnostics).
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Last write failure, cleared by the next successful write.
    pub fn last_error(&self) -> Option<String> {
        self.lock().last_error.clone()
    }

    /// Is a write still owed to disk?
    pub fn is_dirty(&self) -> bool {
        self.lock().pending.is_some()
    }

    /// Write everything queued now and wait for the write to finish. Returns
    /// an error when the write failed (it is logged and kept for `Status` too).
    pub fn flush(&self) -> std::io::Result<()> {
        let write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = {
            let mut state = self.lock();
            let cfg = state.pending.take();
            state.dirty_since = None;
            cfg
        };
        let Some(cfg) = cfg else {
            return match &self.lock().last_error {
                Some(message) => Err(std::io::Error::other(message.clone())),
                None => Ok(()),
            };
        };
        let result = self.store.save(&cfg);
        self.record(result.as_ref().err());
        drop(write_guard);
        result.map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!("cannot save {}: {err}", self.store.path().display()),
            )
        })
    }

    /// Update counters/error state after a write; the caller holds `write_lock`.
    fn record(&self, error: Option<&std::io::Error>) {
        if error.is_none() {
            self.writes.fetch_add(1, Ordering::Relaxed);
        }
        let mut state = self.lock();
        state.last_write = Some(Instant::now());
        match error {
            Some(err) => {
                let message = format!("cannot save {}: {err}", self.store.path().display());
                log::error!("{message}");
                state.last_error = Some(message);
            }
            None => state.last_error = None,
        }
        self.cv.notify_all();
    }

    /// Ask the writer thread to finish any queued write and stop, then join it.
    ///
    /// `flush` first, then this; the daemon's shutdown path does both.
    pub fn stop(&self) {
        self.lock().stopped = true;
        self.cv.notify_all();
        let handle = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn run(&self) {
        loop {
            let mut state = self.lock();
            loop {
                if state.stopped && state.pending.is_none() {
                    return;
                }
                if state.pending.is_none() {
                    state = self
                        .cv
                        .wait_timeout(state, Duration::from_millis(500))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                    continue;
                }
                let wait = match state.last_write {
                    Some(last) => (last + MIN_INTERVAL).saturating_duration_since(Instant::now()),
                    None => Duration::ZERO,
                };
                // Never hold a change longer than MAX_DELAY.
                let waited = state
                    .dirty_since
                    .map(|since| since.elapsed())
                    .unwrap_or_default();
                if state.stopped || wait.is_zero() || waited >= MAX_DELAY {
                    break;
                }
                let wait = wait.min(MAX_DELAY - waited);
                state = self
                    .cv
                    .wait_timeout(state, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            drop(state);

            let write_guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            let cfg = {
                let mut state = self.lock();
                let cfg = state.pending.take();
                state.dirty_since = None;
                cfg
            };
            let Some(cfg) = cfg else {
                continue;
            };
            let result = self.store.save(&cfg);
            self.record(result.as_ref().err());
            drop(write_guard);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "avdsp-persist-{}-{name}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config(quantum: u32) -> EngineConfig {
        EngineConfig {
            device: "hw:0,0".into(),
            quantum,
            ..EngineConfig::default()
        }
    }

    fn wait_for(predicate: impl Fn() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        predicate()
    }

    #[test]
    fn rapid_changes_coalesce_into_few_writes() {
        let dir = temp_dir("coalesce");
        let store = Arc::new(ConfigStore::new(dir.join("config.json")));
        let persister = Persister::new(Arc::clone(&store));

        for n in 0..50 {
            persister.mark(&config(256 + n));
            thread::sleep(Duration::from_millis(5));
        }
        let early = persister.writes();
        assert!(early <= 3, "50 rapid marks wrote {early} times");

        // The last change lands within the bounded delay.
        assert!(
            wait_for(
                || {
                    std::fs::read_to_string(store.path())
                        .ok()
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                        .and_then(|value| {
                            value
                                .get("config")
                                .and_then(|cfg| cfg.get("quantum"))
                                .and_then(serde_json::Value::as_u64)
                        })
                        == Some(305)
                },
                Duration::from_secs(3)
            ),
            "the final config was not written in time"
        );
        let total = persister.writes();
        assert!(total <= 5, "coalescing produced {total} writes");
        assert!(total >= 1);

        persister.flush().unwrap();
        persister.stop();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flush_writes_the_queued_state_and_reports_errors() {
        let dir = temp_dir("flush");
        let store = Arc::new(ConfigStore::new(dir.join("config.json")));
        let persister = Persister::new(Arc::clone(&store));
        persister.mark(&config(512));
        persister.flush().unwrap();
        assert_eq!(store.load().config.quantum, 512);
        assert!(persister.last_error().is_none());
        assert!(!persister.is_dirty());

        // A directory where the file belongs makes the write fail; the error is
        // kept for Status instead of being lost.
        let blocked = temp_dir("flush-error");
        fs::create_dir_all(blocked.join("config.json")).unwrap();
        let bad = Arc::new(ConfigStore::new(blocked.join("config.json")));
        let failing = Persister::new(bad);
        failing.mark(&config(256));
        assert!(failing.flush().is_err());
        assert!(
            failing
                .last_error()
                .as_deref()
                .is_some_and(|e| e.contains("cannot save")),
            "{:?}",
            failing.last_error()
        );
        failing.stop();
        persister.stop();
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&blocked).unwrap();
    }
}
