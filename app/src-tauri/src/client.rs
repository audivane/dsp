//! Unix-socket client for the daemon's control plane (see
//! docs/control-plane.md): newline-delimited JSON, requests carry an `id`,
//! responses echo it.
//!
//! - [`DaemonClient`]: one request connection, shared by every caller. Calls
//!   are pipelined (each gets its own id and waits for its own reply) and the
//!   connection is re-opened lazily after the daemon goes away.
//! - [`Subscription`]: a separate connection that sends `subscribe` and
//!   forwards events. It reconnects with backoff and falls back to the older
//!   topic set when the daemon does not know `config`/`presets` yet.

use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

/// Error returned to the webview: the daemon's `RpcError` or a client-side one.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct CallError {
    pub code: i32,
    pub message: String,
}

impl CallError {
    /// No connection to the daemon.
    pub const DISCONNECTED: i32 = -1;
    /// No reply in time.
    pub const TIMEOUT: i32 = -2;
    /// Superseded by a newer in-flight analysis request.
    pub const SUPERSEDED: i32 = -3;
    /// Same as the daemon's INVALID_REQUEST.
    pub const INVALID_REQUEST: i32 = -32600;

    fn disconnected(message: impl Into<String>) -> Self {
        Self {
            code: Self::DISCONNECTED,
            message: message.into(),
        }
    }

    fn superseded() -> Self {
        Self {
            code: Self::SUPERSEDED,
            message: "Superseded by a newer measurement.".into(),
        }
    }
}

/// The daemon's `RpcError::SUPERSEDED` (docs/control-plane.md).
const DAEMON_SUPERSEDED: i32 = 5;

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for CallError {}

type Reply = Result<Value, CallError>;
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Reply>>>>;

struct Conn {
    writer: Mutex<UnixStream>,
    pending: Pending,
    alive: Arc<AtomicBool>,
}

impl Conn {
    fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
    }
}

pub type PathFn = Arc<dyn Fn() -> PathBuf + Send + Sync>;

/// Request connection to the daemon, plus dedicated lazily-opened connections
/// so heavy calls never stall controls: the analysis one (`upsampler_response`)
/// and one per design call ([`BuildLane`]).
pub struct DaemonClient {
    path: PathFn,
    conn: Mutex<Option<Arc<Conn>>>,
    analysis_conn: Mutex<Option<Arc<Conn>>>,
    build_conns: [Mutex<Option<Arc<Conn>>>; BuildLane::COUNT],
    analysis_seq: AtomicU64,
    next_id: AtomicU64,
}

/// Default reply timeout. `self_test` plays a signal and gets longer.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);
pub const SLOW_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The calls that may design a filter: the engine answers once the design is
/// built (tens of seconds for the longest kernels it accepts), so a timeout
/// here means a wedged engine, not a slow design.
pub const BUILD_CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// `upsampler_response` designs the kernel it measures (when it is not
/// cached): as long as a build.
pub const ANALYSIS_CALL_TIMEOUT: Duration = BUILD_CALL_TIMEOUT;

pub fn is_analysis_method(method: &str) -> bool {
    method == "upsampler_response"
}

/// The design calls that run on their own connection: the daemon answers
/// one connection's requests in order, so a long build on the request
/// connection would hold `status`, `get_config` and every edit behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildLane {
    /// `set_upsampler`: the app sends one at a time (its edits queue).
    Upsampler,
    /// `check_design`: the editor's live check, beside a build.
    Check,
}

impl BuildLane {
    const COUNT: usize = 2;

    pub fn of(method: &str) -> Option<Self> {
        match method {
            "set_upsampler" => Some(Self::Upsampler),
            "check_design" => Some(Self::Check),
            _ => None,
        }
    }

    fn thread_name(self) -> &'static str {
        match self {
            Self::Upsampler => "avdsp-ui-build",
            Self::Check => "avdsp-ui-check",
        }
    }
}

pub fn timeout_for(method: &str) -> Duration {
    match method {
        "upsampler_response" => ANALYSIS_CALL_TIMEOUT,
        // Each may design the upsampler's kernel (`set_config`,
        // `load_preset` and `set_ab` with an upsampler or a side B that
        // needs one).
        "set_upsampler" | "check_design" | "set_config" | "load_preset" | "set_ab" => {
            BUILD_CALL_TIMEOUT
        }
        "self_test" | "acquire_device" | "release_device" => SLOW_CALL_TIMEOUT,
        _ => CALL_TIMEOUT,
    }
}

fn connect_error(path: &Path, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            format!("No engine is listening on {}.", path.display())
        }
        std::io::ErrorKind::PermissionDenied => {
            format!("No permission to open {}.", path.display())
        }
        _ => format!("Cannot connect to {}: {e}.", path.display()),
    }
}

fn open_conn(path: &Path, thread_name: &str) -> Result<Arc<Conn>, CallError> {
    let stream =
        UnixStream::connect(path).map_err(|e| CallError::disconnected(connect_error(path, &e)))?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let reader = stream
        .try_clone()
        .map_err(|e| CallError::disconnected(format!("Socket error: {e}")))?;
    let conn = Arc::new(Conn {
        writer: Mutex::new(stream),
        pending: Arc::default(),
        alive: Arc::new(AtomicBool::new(true)),
    });
    let (pending, alive) = (conn.pending.clone(), conn.alive.clone());
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || read_replies(reader, pending, alive))
        .map_err(|e| CallError::disconnected(format!("Cannot start a thread: {e}")))?;
    Ok(conn)
}

impl DaemonClient {
    pub fn new(path: PathFn) -> Self {
        Self {
            path,
            conn: Mutex::new(None),
            analysis_conn: Mutex::new(None),
            build_conns: Default::default(),
            analysis_seq: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
        }
    }

    fn connection(&self, lane: Option<BuildLane>) -> Result<Arc<Conn>, CallError> {
        let (slot, name) = match lane {
            None => (&self.conn, "avdsp-ui-replies"),
            Some(l) => (&self.build_conns[l as usize], l.thread_name()),
        };
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = slot.as_ref()
            && c.alive.load(Ordering::SeqCst)
        {
            return Ok(c.clone());
        }
        let conn = open_conn(&(self.path)(), name)?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    /// Drop the current connections (the next call reconnects).
    pub fn reset(&self) {
        if let Some(c) = self.conn.lock().unwrap_or_else(|e| e.into_inner()).take() {
            c.kill();
        }
        if let Some(c) = self
            .analysis_conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            c.kill();
        }
        for slot in &self.build_conns {
            if let Some(c) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                c.kill();
            }
        }
    }

    /// Call `method`. `params: None` omits the field (unit calls such as
    /// `status`); `Some(Value::Null)` sends `"params":null` (e.g. clearing
    /// the EQ with `set_eq`).
    pub fn call(&self, method: &str, params: Option<Value>) -> Result<Value, CallError> {
        let mut obj = Map::new();
        obj.insert("method".into(), Value::String(method.into()));
        if let Some(p) = params {
            obj.insert("params".into(), p);
        }
        let timeout = timeout_for(method);
        if is_analysis_method(method) {
            self.send_analysis(obj, timeout)
        } else {
            self.send(obj, timeout, BuildLane::of(method))
        }
    }

    /// Forward a request object from the webview: `{"method": …, "params"?: …}`.
    /// Presence of `params` is preserved. `subscribe` is refused here: events
    /// come from the [`Subscription`] connection.
    pub fn call_request(&self, request: Value) -> Result<Value, CallError> {
        let Value::Object(mut obj) = request else {
            return Err(CallError {
                code: CallError::INVALID_REQUEST,
                message: "request must be an object".into(),
            });
        };
        let method = match obj.get("method") {
            Some(Value::String(m)) if !m.is_empty() => m.clone(),
            _ => {
                return Err(CallError {
                    code: CallError::INVALID_REQUEST,
                    message: "missing `method`".into(),
                });
            }
        };
        if method == "subscribe" {
            return Err(CallError {
                code: CallError::INVALID_REQUEST,
                message: "use the event channel instead of `subscribe`".into(),
            });
        }
        // `"lane": "upsampler"` sends a read behind the upsampler's build (the
        // daemon answers a connection in order), so its reply comes once that
        // build is done: a client that stopped waiting learns its outcome
        // without sending the change again.
        let after_build = obj.get("lane").and_then(Value::as_str) == Some("upsampler");
        obj.retain(|k, _| k == "method" || k == "params");
        if after_build {
            return self.send(obj, BUILD_CALL_TIMEOUT, Some(BuildLane::Upsampler));
        }
        let timeout = timeout_for(&method);
        if is_analysis_method(&method) {
            self.send_analysis(obj, timeout)
        } else {
            self.send(obj, timeout, BuildLane::of(&method))
        }
    }

    fn send(
        &self,
        mut obj: Map<String, Value>,
        timeout: Duration,
        lane: Option<BuildLane>,
    ) -> Result<Value, CallError> {
        // One retry: a connection that died since the last call is replaced.
        for attempt in 0..2 {
            let conn = self.connection(lane)?;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            obj.insert("id".into(), json!(id));
            let mut line = serde_json::to_vec(&obj).expect("JSON values always serialize");
            line.push(b'\n');
            let (tx, rx) = mpsc::channel();
            conn.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, tx);
            let written = {
                let mut w = conn.writer.lock().unwrap_or_else(|e| e.into_inner());
                w.write_all(&line).and_then(|()| w.flush())
            };
            if let Err(e) = written {
                conn.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                conn.kill();
                if attempt == 0 {
                    continue;
                }
                return Err(CallError::disconnected(format!(
                    "Lost the connection to the engine: {e}."
                )));
            }
            return match rx.recv_timeout(timeout) {
                Ok(reply) => reply,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    conn.pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    Err(CallError {
                        code: CallError::TIMEOUT,
                        message: "The engine did not answer in time.".into(),
                    })
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(CallError::disconnected("The engine closed the connection."))
                }
            };
        }
        unreachable!("the loop always returns")
    }

    /// Heavy analysis calls use a separate lazily-opened connection so control
    /// calls are never blocked. Only one analysis call may be in flight: a newer
    /// call kills any pending analysis connection so it starts immediately and
    /// the older call unblocks as superseded.
    fn send_analysis(
        &self,
        mut obj: Map<String, Value>,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let seq = self.analysis_seq.fetch_add(1, Ordering::SeqCst) + 1;
        for attempt in 0..2 {
            if self.analysis_seq.load(Ordering::SeqCst) != seq {
                return Err(CallError::superseded());
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = mpsc::channel();
            let conn = {
                let mut slot = self.analysis_conn.lock().unwrap_or_else(|e| e.into_inner());
                if self.analysis_seq.load(Ordering::SeqCst) != seq {
                    return Err(CallError::superseded());
                }
                if let Some(existing) = slot.as_ref() {
                    let busy = !existing
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_empty();
                    if busy || !existing.alive.load(Ordering::SeqCst) {
                        existing.kill();
                        *slot = None;
                    }
                }
                let conn = match slot.as_ref() {
                    Some(c) => c.clone(),
                    None => {
                        let c = open_conn(&(self.path)(), "avdsp-ui-analysis")?;
                        *slot = Some(c.clone());
                        c
                    }
                };
                conn.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, tx);
                conn
            };

            obj.insert("id".into(), json!(id));
            let mut line = serde_json::to_vec(&obj).expect("JSON values always serialize");
            line.push(b'\n');
            let written = {
                let mut w = conn.writer.lock().unwrap_or_else(|e| e.into_inner());
                w.write_all(&line).and_then(|()| w.flush())
            };
            if let Err(e) = written {
                conn.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                conn.kill();
                let mut slot = self.analysis_conn.lock().unwrap_or_else(|e| e.into_inner());
                if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                    *slot = None;
                }
                drop(slot);
                if self.analysis_seq.load(Ordering::SeqCst) != seq {
                    return Err(CallError::superseded());
                }
                if attempt == 0 {
                    continue;
                }
                return Err(CallError::disconnected(format!(
                    "Lost the connection to the engine: {e}."
                )));
            }
            let res = match rx.recv_timeout(timeout) {
                Ok(reply) => reply,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    conn.pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    conn.kill();
                    let mut slot = self.analysis_conn.lock().unwrap_or_else(|e| e.into_inner());
                    if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                        *slot = None;
                    }
                    Err(CallError {
                        code: CallError::TIMEOUT,
                        message: "The engine did not answer in time.".into(),
                    })
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(CallError::disconnected("The engine closed the connection."))
                }
            };
            if self.analysis_seq.load(Ordering::SeqCst) != seq {
                return Err(CallError::superseded());
            }
            return res;
        }
        unreachable!("the loop always returns")
    }
}

fn read_replies(reader: UnixStream, pending: Pending, alive: Arc<AtomicBool>) {
    let mut lines = BufReader::new(reader);
    let mut buf = String::new();
    loop {
        buf.clear();
        match lines.read_line(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(v) = serde_json::from_str::<Value>(buf.trim_end()) else {
            continue;
        };
        let Some(id) = v.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Some(tx) = pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        else {
            continue;
        };
        let _ = tx.send(outcome(&v));
    }
    alive.store(false, Ordering::SeqCst);
    for (_, tx) in pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
        let _ = tx.send(Err(CallError::disconnected(
            "The engine closed the connection.",
        )));
    }
}

fn outcome(v: &Value) -> Reply {
    if let Some(ok) = v.get("ok") {
        return Ok(ok.clone());
    }
    if let Some(err) = v.get("error") {
        let code = err
            .get("code")
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok())
            .unwrap_or(-32603);
        // The daemon dropped a queued slow call for a newer one of the same
        // method: one superseded code for the webview, client- or daemon-side.
        if code == DAEMON_SUPERSEDED {
            return Err(CallError::superseded());
        }
        return Err(CallError {
            code,
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string(),
        });
    }
    Ok(Value::Null)
}

// ---------------------------------------------------------------- subscription

/// What the subscription reports.
#[derive(Clone, Debug, PartialEq)]
pub enum SubMsg {
    Connecting,
    Up,
    Down(String),
    /// A daemon event line: `{"event": …, "data": …}`.
    Event(Value),
}

/// Topics asked for first; [`BASE_TOPICS`] when the daemon refuses them.
pub const ALL_TOPICS: &[&str] = &[
    "meters",
    "status",
    "streams",
    "now_playing",
    "config",
    "presets",
    "devices",
];
pub const BASE_TOPICS: &[&str] = &["meters", "status", "streams", "now_playing"];

const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(3);

pub type Sink = Arc<dyn Fn(SubMsg) + Send + Sync>;

#[derive(Default)]
struct SubControl {
    spectrum: bool,
    /// `meters` left out of the topics (nothing shows them: the main
    /// window is closed to the tray).
    no_meters: bool,
    resubscribe: bool,
    writer: Option<UnixStream>,
    stream: Option<UnixStream>,
}

pub struct Subscription {
    stop: Arc<AtomicBool>,
    ctrl: Arc<Mutex<SubControl>>,
    thread: Option<JoinHandle<()>>,
}

impl Subscription {
    pub fn spawn(path: PathFn, sink: Sink) -> Self {
        Self::spawn_with(path, sink, true)
    }

    /// `meters`: whether the first connection asks for the `meters` topic
    /// (see [`Self::set_meters`]).
    pub fn spawn_with(path: PathFn, sink: Sink, meters: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let ctrl: Arc<Mutex<SubControl>> = Arc::new(Mutex::new(SubControl {
            no_meters: !meters,
            ..SubControl::default()
        }));
        let (s, c) = (stop.clone(), ctrl.clone());
        let thread = std::thread::Builder::new()
            .name("avdsp-ui-events".into())
            .spawn(move || run(path, sink, s, c))
            .expect("spawn the event thread");
        Self {
            stop,
            ctrl,
            thread: Some(thread),
        }
    }

    /// Enable or disable the `spectrum` topic on the daemon subscription.
    /// Enabling subscribes on the live socket; disabling reconnects cleanly
    /// without `spectrum` because the protocol has no `unsubscribe` call.
    pub fn set_spectrum(&self, enabled: bool) {
        let mut c = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
        if c.spectrum == enabled {
            return;
        }
        c.spectrum = enabled;
        if enabled {
            if let Some(w) = c.writer.as_mut() {
                let line =
                    json!({"id": 3, "method": "subscribe", "params": {"topics": ["spectrum"]}});
                let _ = writeln!(w, "{line}");
                let _ = w.flush();
            }
        } else {
            c.resubscribe = true;
            c.writer = None;
            if let Some(s) = c.stream.take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// Enable or disable the `meters` topic (on from the start). Like
    /// [`Self::set_spectrum`]: enabling subscribes on the live socket,
    /// disabling reconnects without it (no `Down` is reported, and the
    /// daemon sends its snapshot again).
    pub fn set_meters(&self, enabled: bool) {
        let mut c = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
        if c.no_meters != enabled {
            return;
        }
        c.no_meters = !enabled;
        if enabled {
            if let Some(w) = c.writer.as_mut() {
                let line =
                    json!({"id": 4, "method": "subscribe", "params": {"topics": ["meters"]}});
                let _ = writeln!(w, "{line}");
                let _ = w.flush();
            }
        } else {
            c.resubscribe = true;
            c.writer = None;
            if let Some(s) = c.stream.take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        {
            let mut c = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
            c.writer = None;
            if let Some(s) = c.stream.take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(path: PathFn, sink: Sink, stop: Arc<AtomicBool>, ctrl: Arc<Mutex<SubControl>>) {
    let mut backoff = BACKOFF_MIN;
    let mut last_down: Option<String> = None;
    let mut silent_reconnect = false;
    sink(SubMsg::Connecting);
    while !stop.load(Ordering::SeqCst) {
        let p = path();
        let result = session(&p, &sink, &stop, &ctrl, silent_reconnect);
        silent_reconnect = false;
        if stop.load(Ordering::SeqCst) {
            break;
        }
        {
            let mut c = ctrl.lock().unwrap_or_else(|e| e.into_inner());
            c.writer = None;
            c.stream = None;
            if c.resubscribe {
                c.resubscribe = false;
                silent_reconnect = true;
                backoff = BACKOFF_MIN;
                continue;
            }
        }
        match result {
            Ok(()) => {
                backoff = BACKOFF_MIN;
                let msg = "The engine stopped.".to_string();
                sink(SubMsg::Down(msg.clone()));
                last_down = Some(msg);
            }
            Err(msg) => {
                if last_down.as_deref() != Some(msg.as_str()) {
                    sink(SubMsg::Down(msg.clone()));
                    last_down = Some(msg);
                }
            }
        }
        // Sleep in small steps so `stop` is prompt.
        let mut slept = Duration::ZERO;
        while slept < backoff && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50));
            slept += Duration::from_millis(50);
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connection. `Ok` = it was up and then ended; `Err` = never came up.
fn session(
    path: &Path,
    sink: &Sink,
    stop: &AtomicBool,
    ctrl: &Mutex<SubControl>,
    silent_reconnect: bool,
) -> Result<(), String> {
    let stream = UnixStream::connect(path).map_err(|e| connect_error(path, &e))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    ctrl.lock().unwrap_or_else(|e| e.into_inner()).stream =
        Some(stream.try_clone().map_err(|e| e.to_string())?);
    if stop.load(Ordering::SeqCst) {
        return Err("stopped".into());
    }
    let mut reader = BufReader::new(stream);
    let mut subscribed = false;
    let no_meters = ctrl.lock().unwrap_or_else(|e| e.into_inner()).no_meters;
    for (id, topics) in [(1u64, ALL_TOPICS), (2, BASE_TOPICS)] {
        let topics: Vec<&str> = topics
            .iter()
            .copied()
            .filter(|t| !(no_meters && *t == "meters"))
            .collect();
        let line = json!({"id": id, "method": "subscribe", "params": {"topics": topics}});
        writeln!(writer, "{line}").map_err(|e| format!("Lost the engine: {e}."))?;
        match wait_reply(&mut reader, id)? {
            Ok(_) => {
                subscribed = true;
                break;
            }
            Err(e) if id == 1 => {
                log::info!("subscribe with all topics refused ({e}); using the base topics");
            }
            Err(e) => {
                return Err(format!(
                    "The engine refused the subscription: {}",
                    e.message
                ));
            }
        }
    }
    if !subscribed {
        return Err("The engine refused the subscription.".into());
    }
    {
        let mut c = ctrl.lock().unwrap_or_else(|e| e.into_inner());
        if stop.load(Ordering::SeqCst) {
            return Err("stopped".into());
        }
        if c.spectrum {
            let line = json!({"id": 3, "method": "subscribe", "params": {"topics": ["spectrum"]}});
            writeln!(writer, "{line}").map_err(|e| format!("Lost the engine: {e}."))?;
            let _ = writer.flush();
        }
        // Turned on while this connection was being made.
        if no_meters && !c.no_meters {
            let line = json!({"id": 4, "method": "subscribe", "params": {"topics": ["meters"]}});
            writeln!(writer, "{line}").map_err(|e| format!("Lost the engine: {e}."))?;
            let _ = writer.flush();
        }
        c.writer = Some(writer);
    }
    if !silent_reconnect {
        sink(SubMsg::Up);
    }
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
        let Ok(v) = serde_json::from_str::<Value>(buf.trim_end()) else {
            continue;
        };
        if v.get("event").is_some() {
            sink(SubMsg::Event(v));
        }
    }
}

/// Read lines until the reply with `id`. Event lines before it are dropped
/// (the daemon sends its snapshot only after the reply).
fn wait_reply(reader: &mut BufReader<UnixStream>, id: u64) -> Result<Reply, String> {
    let _ = reader.get_ref().set_read_timeout(Some(CALL_TIMEOUT));
    let mut buf = String::new();
    let result = loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break Err("The engine closed the connection.".to_string()),
            Err(e) => break Err(format!("The engine did not answer: {e}.")),
            Ok(_) => {}
        }
        let Ok(v) = serde_json::from_str::<Value>(buf.trim_end()) else {
            continue;
        };
        if v.get("id").and_then(Value::as_u64) == Some(id) {
            break Ok(outcome(&v));
        }
    };
    let _ = reader.get_ref().set_read_timeout(None);
    result
}

#[cfg(test)]
mod tests;
