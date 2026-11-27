//! Newline-delimited JSON IPC over a Unix socket.
//!
//! One JSON object per line (`protocol::ipc`). Requests carry an `id`; replies
//! echo it. A connection that sends `subscribe` also receives `Event` lines.
//!
//! The server is deliberately std-only: a small non-blocking accept loop, one
//! reader thread and one writer thread per connection, and one event pump at
//! 30 Hz. A client that stops reading is dropped rather than allowed to block
//! the daemon.

use crate::control::service::{Dispatch, Service};
use audivane_protocol::ipc::{Call, Event, Outcome, Response, RpcError, Status, StreamInfo, Topic};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Longest accepted request line (including the newline).
pub const MAX_LINE: usize = 1024 * 1024;
/// Event pump frequency.
pub const EVENT_HZ: u64 = 30;
/// How many queued lines a connection may have before it is dropped.
const OUT_QUEUE: usize = 256;

/// Whether `method` names a call this daemon understands. Used to tell an
/// unknown method (`INVALID_REQUEST`) from bad parameters (`INVALID_PARAMS`).
/// Derived from `Call` itself (an unknown tag is the only "unknown variant"
/// error a params-free probe can give), so a new call can never be missed.
fn is_known_method(method: &str) -> bool {
    match serde_json::from_value::<Call>(serde_json::json!({ "method": method })) {
        Ok(_) => true,
        Err(err) => !err.to_string().starts_with("unknown variant"),
    }
}

/// Where the daemon listens.
///
/// Order: explicit (`--socket`), `$AUDIVANE_DSP_SOCKET`,
/// `$XDG_RUNTIME_DIR/audivane-dsp.sock`, else `/tmp/audivane-dsp-<uid>.sock`.
pub fn socket_path(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return check_path(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("AUDIVANE_DSP_SOCKET") {
        return check_path(PathBuf::from(path));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return check_path(PathBuf::from(runtime).join("audivane-dsp.sock"));
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    check_path(PathBuf::from(format!("/tmp/audivane-dsp-{uid}.sock")))
}

fn check_path(path: PathBuf) -> Result<PathBuf, String> {
    // sun_path is 108 bytes on Linux, including the trailing NUL.
    if path.as_os_str().len() > 107 {
        return Err(format!(
            "socket path is too long ({} bytes, max 107): {}",
            path.as_os_str().len(),
            path.display()
        ));
    }
    Ok(path)
}

/// Is a daemon answering on `path`?
pub fn probe(path: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    if stream
        .write_all(br#"{"id":0,"method":"status"}"#)
        .and_then(|()| stream.write_all(b"\n"))
        .is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    match serde_json::from_str::<Response>(line.trim()) {
        Ok(response) => response.id == 0,
        Err(_) => false,
    }
}

// ---------------------------------------------------------------- server

struct Conn {
    tx: SyncSender<Vec<u8>>,
    stream: Arc<UnixStream>,
    topics: HashSet<Topic>,
}

struct ConnThreads {
    stream: Arc<UnixStream>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    reader_done: Arc<AtomicBool>,
    writer_done: Arc<AtomicBool>,
}

struct ServerInner {
    service: Arc<Service>,
    conns: Mutex<HashMap<u64, Conn>>,
    conn_threads: Mutex<HashMap<u64, ConnThreads>>,
    next_id: AtomicU64,
    running: AtomicBool,
}

impl ServerInner {
    fn send(&self, id: u64, bytes: Vec<u8>) {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = conns.get(&id) else { return };
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) = conn.tx.try_send(bytes)
        {
            log::debug!("dropping slow IPC client {id}");
            conn.stream.shutdown(std::net::Shutdown::Both).ok();
            conns.remove(&id);
        }
    }

    fn broadcast(&self, topic: Topic, bytes: Vec<u8>) {
        let mut dead = Vec::new();
        {
            let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
            for (&id, conn) in conns.iter_mut() {
                if !conn.topics.contains(&topic) {
                    continue;
                }
                match conn.tx.try_send(bytes.clone()) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                        log::debug!("dropping slow IPC client {id}");
                        conn.stream.shutdown(std::net::Shutdown::Both).ok();
                        dead.push(id);
                    }
                }
            }
            for id in dead {
                conns.remove(&id);
            }
        }
    }

    fn subscribe(&self, id: u64, topics: Vec<Topic>) {
        let mut conns = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = conns.get_mut(&id) {
            conn.topics.extend(topics);
        }
    }

    /// Seed a new subscriber with the current state of its topics, so `watch`
    /// shows something before the first pump tick.
    fn send_snapshots(&self, id: u64, topics: &[Topic]) {
        if topics.contains(&Topic::Status)
            && let Ok(bytes) = event_bytes(&Event::Status(self.service.status()))
        {
            self.send(id, bytes);
        }
        if topics.contains(&Topic::Streams)
            && let Ok(bytes) = event_bytes(&Event::Streams(self.service.streams()))
        {
            self.send(id, bytes);
        }
        if topics.contains(&Topic::NowPlaying)
            && let Ok(bytes) = event_bytes(&Event::NowPlaying(self.service.now_playing()))
        {
            self.send(id, bytes);
        }
        if topics.contains(&Topic::Config)
            && let Ok(bytes) = event_bytes(&Event::Config(self.service.config()))
        {
            self.send(id, bytes);
        }
        if topics.contains(&Topic::Presets)
            && let Ok(bytes) = event_bytes(&Event::Presets(self.service.presets_info()))
        {
            self.send(id, bytes);
        }
        if topics.contains(&Topic::Spectrum)
            && let Some(spectrum) = self.service.spectrum()
            && let Ok(bytes) = event_bytes(&Event::Spectrum(spectrum))
        {
            self.send(id, bytes);
        }
        if topics.contains(&Topic::Devices)
            && let Ok(bytes) = event_bytes(&Event::Devices(self.service.devices()))
        {
            self.send(id, bytes);
        }
    }

    fn subscribers(&self) -> HashSet<Topic> {
        self.conns
            .lock()
            .map(|conns| {
                conns
                    .values()
                    .flat_map(|c| c.topics.iter().copied())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn prune_finished_threads(&self) {
        let finished: Vec<ConnThreads> = {
            let mut map = self.conn_threads.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<u64> = map
                .iter()
                .filter(|(_, ct)| {
                    ct.reader_done.load(Ordering::Acquire) && ct.writer_done.load(Ordering::Acquire)
                })
                .map(|(&id, _)| id)
                .collect();
            ids.into_iter().filter_map(|id| map.remove(&id)).collect()
        };
        for mut ct in finished {
            if let Some(reader) = ct.reader.take() {
                let _ = reader.join();
            }
            if let Some(writer) = ct.writer.take() {
                let _ = writer.join();
            }
        }
    }
}

/// A running IPC server.
pub struct IpcServer {
    inner: Arc<ServerInner>,
    path: PathBuf,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl IpcServer {
    /// Bind `path` and start serving. Fails when another live daemon answers
    /// on the socket; a stale socket file is removed.
    pub fn serve(service: Arc<Service>, path: PathBuf) -> io::Result<Arc<Self>> {
        prepare_socket(&path)?;
        let listener = UnixListener::bind(&path)?;
        if let Err(err) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            remove_socket(&path);
            return Err(err);
        }
        listener.set_nonblocking(true)?;

        let inner = Arc::new(ServerInner {
            service,
            conns: Mutex::new(HashMap::new()),
            conn_threads: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            running: AtomicBool::new(true),
        });

        let accept_inner = Arc::clone(&inner);
        let accept = thread::Builder::new()
            .name("avdsp-ipc-accept".into())
            .spawn(move || accept_loop(listener, accept_inner))?;

        let pump_inner = Arc::clone(&inner);
        let pump = thread::Builder::new()
            .name("avdsp-ipc-events".into())
            .spawn(move || event_loop(pump_inner))?;

        Ok(Arc::new(Self {
            inner,
            path,
            threads: Mutex::new(vec![accept, pump]),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting, wait for active dispatches and queued replies to finish,
    /// close every connection, and remove the socket file.
    pub fn shutdown(&self) {
        if !self.inner.running.swap(false, Ordering::SeqCst) {
            return;
        }
        self.inner.service.begin_shutdown();

        // Wait for accept and event threads first so no new connections or
        // event broadcasts can start while we tear down existing ones.
        let threads = self
            .threads
            .lock()
            .map(|mut t| std::mem::take(&mut *t))
            .unwrap_or_default();
        for thread in threads {
            let _ = thread.join();
        }

        let mut conn_threads: Vec<ConnThreads> = self
            .inner
            .conn_threads
            .lock()
            .map(|mut t| t.drain().map(|(_, v)| v).collect())
            .unwrap_or_default();

        // Shut down the read half to wake idle reader threads while letting
        // replies (such as `shutdown` or an in-flight mutation's reply) drain
        // out on the write half.
        for ct in &conn_threads {
            ct.stream.shutdown(std::net::Shutdown::Read).ok();
        }

        // Wait for every reader thread (and every in-flight dispatch) to exit.
        for ct in &mut conn_threads {
            if let Some(reader) = ct.reader.take() {
                let _ = reader.join();
            }
        }

        // Drop remaining `Conn.tx` senders so writer threads drain and exit.
        if let Ok(mut conns) = self.inner.conns.lock() {
            conns.clear();
        }

        // Give writer threads a brief window to flush queued replies, then
        // force-close any slow/blocked sockets before joining writers.
        let deadline = std::time::Instant::now() + Duration::from_millis(50);
        while conn_threads
            .iter()
            .any(|ct| !ct.writer_done.load(Ordering::Acquire))
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(2));
        }
        for ct in &conn_threads {
            if !ct.writer_done.load(Ordering::Acquire) {
                ct.stream.shutdown(std::net::Shutdown::Both).ok();
            }
        }
        for ct in &mut conn_threads {
            if let Some(writer) = ct.writer.take() {
                let _ = writer.join();
            }
        }

        remove_socket(&self.path);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn remove_socket(path: &Path) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if let Err(err) = fs::remove_file(path) {
                log::warn!("cannot remove socket {}: {err}", path.display());
            }
        }
        Ok(_) => {}
        Err(_) => {}
    }
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            if probe(path) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "another audivane-dsp daemon is already listening on {}",
                        path.display()
                    ),
                ));
            }
            fs::remove_file(path)?;
            log::debug!("removed stale socket {}", path.display());
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    Ok(())
}

fn accept_loop(listener: UnixListener, inner: Arc<ServerInner>) {
    while inner.running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                if !inner.running.load(Ordering::SeqCst) {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    break;
                }
                if let Err(err) = spawn_connection(&inner, stream) {
                    log::warn!("cannot serve IPC connection: {err}");
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                log::warn!("accept failed: {err}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn spawn_connection(inner: &Arc<ServerInner>, stream: UnixStream) -> io::Result<()> {
    inner.prune_finished_threads();
    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let read_stream = stream.try_clone()?;
    let shared_stream = Arc::new(stream);
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(OUT_QUEUE);
    inner
        .conns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            Conn {
                tx: tx.clone(),
                stream: Arc::clone(&shared_stream),
                topics: HashSet::new(),
            },
        );

    let writer_done = Arc::new(AtomicBool::new(false));
    let writer_done_flag = Arc::clone(&writer_done);
    // Writer: owns the sending half, so a blocked client blocks only this
    // thread until the queue is full (then the connection is dropped).
    let writer = match thread::Builder::new()
        .name(format!("avdsp-ipc-w{id}"))
        .spawn(move || {
            let mut stream = read_stream;
            while let Ok(bytes) = rx.recv() {
                if stream.write_all(&bytes).is_err() {
                    break;
                }
                let _ = stream.flush();
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
            writer_done_flag.store(true, Ordering::Release);
        }) {
        Ok(handle) => handle,
        Err(err) => {
            inner
                .conns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(err);
        }
    };

    let read_side = match shared_stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            inner
                .conns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            drop(tx);
            let _ = shared_stream.shutdown(std::net::Shutdown::Both);
            let _ = writer.join();
            return Err(io::Error::new(err.kind(), err));
        }
    };
    let reader_done = Arc::new(AtomicBool::new(false));
    let reader_done_flag = Arc::clone(&reader_done);
    let reader_inner = Arc::clone(inner);
    let reader = match thread::Builder::new()
        .name(format!("avdsp-ipc-r{id}"))
        .spawn(move || {
            serve_connection(&reader_inner, id, read_side, tx);
            reader_inner
                .conns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            reader_done_flag.store(true, Ordering::Release);
        }) {
        Ok(handle) => handle,
        Err(err) => {
            inner
                .conns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            let _ = shared_stream.shutdown(std::net::Shutdown::Both);
            let _ = writer.join();
            return Err(err);
        }
    };

    inner
        .conn_threads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            ConnThreads {
                stream: shared_stream,
                reader: Some(reader),
                writer: Some(writer),
                reader_done,
                writer_done,
            },
        );
    Ok(())
}

fn serve_connection(
    inner: &Arc<ServerInner>,
    id: u64,
    stream: UnixStream,
    tx: SyncSender<Vec<u8>>,
) {
    let replier = Replier {
        tx: tx.clone(),
        stream: stream.try_clone().ok().map(Arc::new),
    };
    // Dropped (and its workers joined) on every way out of this function.
    let mut slow = SlowCalls::new(Arc::clone(inner), id, replier);
    let mut reader = LineReader::new(BufReader::new(stream));
    loop {
        let bytes = match reader.read_line() {
            Ok(ReadLine::Line(bytes)) => bytes,
            Ok(ReadLine::Eof) => break,
            Ok(ReadLine::TooLong) => {
                let error = error_response(0, RpcError::PARSE, "request line exceeds 1 MiB");
                let _ = tx.try_send(error);
                break;
            }
            Err(err) => {
                log::debug!("IPC read error on connection {id}: {err}");
                break;
            }
        };
        let (request_id, call) = match parse_request(&bytes) {
            Ok(request) => request,
            Err(error) => {
                if tx.try_send(error).is_err() {
                    break;
                }
                continue;
            }
        };
        if let Some(kind) = SlowKind::of(&call) {
            slow.submit(kind, request_id, call);
            continue;
        }
        let (response, after) = handle_call(inner, id, request_id, call);
        if tx.try_send(response).is_err() {
            break;
        }
        match after {
            After::None => {}
            After::Subscribe(topics) => inner.send_snapshots(id, &topics),
            After::Shutdown => {
                // Let the reply drain before the daemon tears the socket down.
                thread::sleep(Duration::from_millis(20));
                inner.service.shutdown_flag().request();
                break;
            }
        }
    }
}

/// What to do after sending the reply for a request.
enum After {
    None,
    /// Send the current state of these topics to the new subscriber.
    Subscribe(Vec<Topic>),
    /// Stop the daemon (after the reply has been queued).
    Shutdown,
}

/// Parse one request line into its id and call, or the serialized error
/// reply for a line that is not a valid request.
fn parse_request(line: &[u8]) -> Result<(u64, Call), Vec<u8>> {
    let Ok(text) = std::str::from_utf8(line) else {
        return Err(error_response(
            0,
            RpcError::PARSE,
            "request is not valid UTF-8",
        ));
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(error_response(0, RpcError::PARSE, "empty request line"));
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|err| error_response(0, RpcError::PARSE, format!("malformed JSON: {err}")))?;
    let Some(id) = value.get("id").and_then(Value::as_u64) else {
        return Err(error_response(
            0,
            RpcError::INVALID_REQUEST,
            "missing or invalid `id`",
        ));
    };
    let Some(method) = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(error_response(
            id,
            RpcError::INVALID_REQUEST,
            "missing `method`",
        ));
    };
    if !is_known_method(&method) {
        return Err(error_response(
            id,
            RpcError::INVALID_REQUEST,
            format!("unknown method {method:?}"),
        ));
    }
    let call: Call = serde_json::from_value(value).map_err(|err| {
        error_response(
            id,
            RpcError::INVALID_PARAMS,
            format!("bad params for {method:?}: {err}"),
        )
    })?;
    Ok((id, call))
}

/// Dispatch one parsed call. Returns the serialized reply plus the
/// follow-up action.
fn handle_call(inner: &Arc<ServerInner>, conn_id: u64, id: u64, call: Call) -> (Vec<u8>, After) {
    match inner.service.dispatch(call) {
        Ok(Dispatch::Value(value)) => (ok_response(id, value), After::None),
        Ok(Dispatch::Subscribe(topics)) => {
            inner.subscribe(conn_id, topics.clone());
            (ok_response(id, Value::Null), After::Subscribe(topics))
        }
        Ok(Dispatch::Shutdown) => (ok_response(id, Value::Null), After::Shutdown),
        Err(err) => (error_response(id, err.code, err.message), After::None),
    }
}

// ---------------------------------------------------------------- slow calls

/// Read-only calls that can take tens of milliseconds to seconds. They run
/// on a worker per connection and kind, off the reader thread, so quick
/// calls on the same connection (volume, mute, edits) are not held up, and
/// their replies may arrive out of order.
///
/// Per connection and kind, at most one call runs and one waits; a newer
/// call replaces the waiting one, which is answered `SUPERSEDED` at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlowKind {
    /// Simulates the harmonics stage (~10–60 ms, more with a sweep).
    HarmonicsReadout,
    /// Simulates a tone through the harmonics stage (milliseconds; up to
    /// ~0.4 s with tape layers at 768 kHz).
    HarmonicsSpectrum,
    /// Designs and measures a kernel when the design is not cached
    /// (0.1–1.5 s on an uncached preset; ~0.3 ms cached).
    UpsamplerResponse,
    /// Loads and transforms a chain's convolution filters when not cached
    /// (tens of milliseconds to a second for long filters at high rates).
    ConvResponse,
}

impl SlowKind {
    const COUNT: usize = 4;

    fn of(call: &Call) -> Option<Self> {
        match call {
            Call::HarmonicsReadout { .. } => Some(Self::HarmonicsReadout),
            Call::HarmonicsSpectrum { .. } => Some(Self::HarmonicsSpectrum),
            Call::UpsamplerResponse { .. } => Some(Self::UpsamplerResponse),
            Call::ConvResponse { .. } => Some(Self::ConvResponse),
            _ => None,
        }
    }

    fn method(self) -> &'static str {
        match self {
            Self::HarmonicsReadout => "harmonics_readout",
            Self::HarmonicsSpectrum => "harmonics_spectrum",
            Self::UpsamplerResponse => "upsampler_response",
            Self::ConvResponse => "conv_response",
        }
    }
}

/// Sends a worker's reply on its connection. A full or closed queue closes
/// the connection, as on the reader thread.
#[derive(Clone)]
struct Replier {
    tx: SyncSender<Vec<u8>>,
    stream: Option<Arc<UnixStream>>,
}

impl Replier {
    /// False when the reply could not be queued (the connection is gone).
    fn send(&self, bytes: Vec<u8>) -> bool {
        if self.tx.try_send(bytes).is_ok() {
            return true;
        }
        if let Some(stream) = &self.stream {
            stream.shutdown(std::net::Shutdown::Both).ok();
        }
        false
    }
}

#[derive(Default)]
struct Lane {
    /// A worker thread is running this lane.
    running: bool,
    /// The call waiting for it (request id and call).
    queued: Option<(u64, Call)>,
}

struct SlowCalls {
    inner: Arc<ServerInner>,
    conn_id: u64,
    replier: Replier,
    lanes: [Arc<Mutex<Lane>>; SlowKind::COUNT],
    workers: Vec<JoinHandle<()>>,
}

impl SlowCalls {
    fn new(inner: Arc<ServerInner>, conn_id: u64, replier: Replier) -> Self {
        Self {
            inner,
            conn_id,
            replier,
            lanes: Default::default(),
            workers: Vec::new(),
        }
    }

    fn submit(&mut self, kind: SlowKind, id: u64, call: Call) {
        self.join_finished();
        let lane = Arc::clone(&self.lanes[kind as usize]);
        let (start, superseded) = {
            let mut state = lane.lock().unwrap_or_else(|e| e.into_inner());
            let superseded = state.queued.replace((id, call)).map(|(old, _)| old);
            let start = !state.running;
            state.running = true;
            (start, superseded)
        };
        if let Some(old) = superseded {
            self.replier.send(error_response(
                old,
                RpcError::SUPERSEDED,
                format!("superseded by a newer {} request", kind.method()),
            ));
        }
        if !start {
            return;
        }
        let worker_lane = Arc::clone(&lane);
        let inner = Arc::clone(&self.inner);
        let replier = self.replier.clone();
        let conn_id = self.conn_id;
        let spawned = thread::Builder::new()
            .name(format!("avdsp-ipc-s{conn_id}-{}", kind as usize))
            .spawn(move || run_lane(&inner, conn_id, &worker_lane, &replier));
        match spawned {
            Ok(handle) => self.workers.push(handle),
            Err(err) => {
                // No thread: answer it here, as before.
                log::warn!("cannot start an IPC worker: {err}");
                let job = {
                    let mut state = lane.lock().unwrap_or_else(|e| e.into_inner());
                    state.running = false;
                    state.queued.take()
                };
                if let Some((id, call)) = job {
                    let (reply, _) = handle_call(&self.inner, conn_id, id, call);
                    self.replier.send(reply);
                }
            }
        }
    }

    fn join_finished(&mut self) {
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].is_finished() {
                let _ = self.workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
    }
}

impl Drop for SlowCalls {
    /// The reader is done: let the workers answer what they have (a client
    /// that half-closes its end still gets its replies) and join them, so
    /// the reader thread, which shutdown waits on, outlives every dispatch.
    fn drop(&mut self) {
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn run_lane(inner: &Arc<ServerInner>, conn_id: u64, lane: &Mutex<Lane>, replier: &Replier) {
    loop {
        let job = {
            let mut state = lane.lock().unwrap_or_else(|e| e.into_inner());
            match state.queued.take() {
                Some(job) => job,
                None => {
                    state.running = false;
                    return;
                }
            }
        };
        let (id, call) = job;
        let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_call(inner, conn_id, id, call).0
        }))
        .unwrap_or_else(|_| error_response(id, RpcError::INTERNAL, "the call panicked"));
        if !replier.send(reply) {
            // The connection is gone: drop what waits instead of computing it.
            let mut state = lane.lock().unwrap_or_else(|e| e.into_inner());
            state.queued = None;
            state.running = false;
            return;
        }
    }
}

fn ok_response(id: u64, value: Value) -> Vec<u8> {
    let response = Response {
        id,
        result: Outcome::Ok(value),
    };
    line_bytes(&response)
}

fn error_response(id: u64, code: i32, message: impl Into<String>) -> Vec<u8> {
    let response = Response {
        id,
        result: Outcome::Error(RpcError {
            code,
            message: message.into(),
        }),
    };
    line_bytes(&response)
}

fn line_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec());
    bytes.push(b'\n');
    bytes
}

fn event_bytes(event: &Event) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    Ok(bytes)
}

// ---------------------------------------------------------------- events

fn event_loop(inner: Arc<ServerInner>) {
    let period = Duration::from_millis(1000 / EVENT_HZ);
    let mut last_status: Option<Status> = None;
    let mut last_streams: Option<Vec<StreamInfo>> = None;
    let mut last_now: Option<Option<audivane_protocol::ipc::NowPlaying>> = None;
    let mut last_config_gen: Option<u64> = None;
    let mut last_presets_gen: Option<u64> = None;
    let mut last_spectrum_gen: Option<u64> = None;
    let mut last_overload_gen = inner.service.overload_generation();
    let mut spectrum_enabled = false;
    let mut last_devices: Option<Vec<audivane_protocol::ipc::DeviceInfo>> = None;
    let mut devices_checked: Option<Instant> = None;

    while inner.running.load(Ordering::Relaxed) {
        let topics = inner.subscribers();
        if topics.is_empty() {
            if spectrum_enabled {
                inner.service.set_spectrum_enabled(false);
                spectrum_enabled = false;
                last_spectrum_gen = None;
            }
            thread::sleep(period);
            continue;
        }
        // The RT thread only copies spectrum taps while someone listens.
        let want_spectrum = topics.contains(&Topic::Spectrum);
        if want_spectrum != spectrum_enabled {
            inner.service.set_spectrum_enabled(want_spectrum);
            spectrum_enabled = want_spectrum;
            last_spectrum_gen = None;
        }

        if topics.contains(&Topic::Meters) {
            let meters = inner.service.meters();
            if let Ok(bytes) = event_bytes(&Event::Meters(meters)) {
                inner.broadcast(Topic::Meters, bytes);
            }
        }
        let overload_gen = inner.service.overload_generation();
        if overload_gen != last_overload_gen {
            last_overload_gen = overload_gen;
            if topics.contains(&Topic::Status)
                && let Some(report) = inner.service.overload_report()
                && let Ok(bytes) = event_bytes(&Event::Overload(report))
            {
                inner.broadcast(Topic::Status, bytes);
            }
        }
        if topics.contains(&Topic::Status) {
            let status = inner.service.status();
            if last_status.as_ref() != Some(&status) {
                if let Ok(bytes) = event_bytes(&Event::Status(status.clone())) {
                    inner.broadcast(Topic::Status, bytes);
                }
                last_status = Some(status);
            }
        }
        if topics.contains(&Topic::Streams) {
            let streams = inner.service.streams();
            if last_streams.as_ref() != Some(&streams) {
                if let Ok(bytes) = event_bytes(&Event::Streams(streams.clone())) {
                    inner.broadcast(Topic::Streams, bytes);
                }
                last_streams = Some(streams);
            }
        }
        if topics.contains(&Topic::NowPlaying) {
            let now = inner.service.now_playing();
            if last_now.as_ref() != Some(&now) {
                if let Ok(bytes) = event_bytes(&Event::NowPlaying(now.clone())) {
                    inner.broadcast(Topic::NowPlaying, bytes);
                }
                last_now = Some(now);
            }
        }
        // Config changes are rare: a generation counter avoids cloning (and
        // comparing) the config 30 times a second.
        if topics.contains(&Topic::Config) {
            let generation = inner.service.config_generation();
            if last_config_gen != Some(generation) {
                if let Ok(bytes) = event_bytes(&Event::Config(inner.service.config())) {
                    inner.broadcast(Topic::Config, bytes);
                }
                last_config_gen = Some(generation);
            }
        }
        if topics.contains(&Topic::Presets) {
            let generation = inner.service.presets_generation();
            if last_presets_gen != Some(generation) {
                if let Ok(bytes) = event_bytes(&Event::Presets(inner.service.presets_info())) {
                    inner.broadcast(Topic::Presets, bytes);
                }
                last_presets_gen = Some(generation);
            }
        }
        if topics.contains(&Topic::Spectrum) {
            let generation = inner.service.spectrum_generation();
            if last_spectrum_gen != Some(generation)
                && let Some(spectrum) = inner.service.spectrum()
            {
                if let Ok(bytes) = event_bytes(&Event::Spectrum(spectrum)) {
                    inner.broadcast(Topic::Spectrum, bytes);
                }
                last_spectrum_gen = Some(generation);
            }
        }
        // Sound cards come and go (a USB DAC plugged in or out): the list
        // is read from `/proc/asound` once a second while someone listens.
        if topics.contains(&Topic::Devices) {
            if devices_checked.is_none_or(|at| at.elapsed() >= DEVICES_CHECK) {
                devices_checked = Some(Instant::now());
                let devices = inner.service.devices();
                if last_devices.as_ref() != Some(&devices) {
                    if last_devices.is_some()
                        && let Ok(bytes) = event_bytes(&Event::Devices(devices.clone()))
                    {
                        inner.broadcast(Topic::Devices, bytes);
                    }
                    last_devices = Some(devices);
                }
            }
        } else {
            last_devices = None;
        }
        thread::sleep(period);
    }
}

/// How often the event loop reads the device list for `devices` events.
const DEVICES_CHECK: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------- framing

enum ReadLine {
    Line(Vec<u8>),
    Eof,
    TooLong,
}

struct LineReader {
    reader: BufReader<UnixStream>,
    buffer: Vec<u8>,
}

impl LineReader {
    fn new(reader: BufReader<UnixStream>) -> Self {
        Self {
            reader,
            buffer: Vec::with_capacity(4096),
        }
    }

    fn read_line(&mut self) -> io::Result<ReadLine> {
        self.buffer.clear();
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                return Ok(if self.buffer.is_empty() {
                    ReadLine::Eof
                } else {
                    ReadLine::Line(std::mem::take(&mut self.buffer))
                });
            }
            if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
                if self.buffer.len() + position > MAX_LINE {
                    self.reader.consume(position + 1);
                    return Ok(ReadLine::TooLong);
                }
                self.buffer.extend_from_slice(&available[..position]);
                self.reader.consume(position + 1);
                let mut line = std::mem::take(&mut self.buffer);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(ReadLine::Line(line));
            }
            let len = available.len();
            if self.buffer.len() + len > MAX_LINE {
                self.reader.consume(len);
                return Ok(ReadLine::TooLong);
            }
            self.buffer.extend_from_slice(available);
            self.reader.consume(len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ShutdownFlag;
    use audivane_protocol::ipc::EngineConfig;

    #[test]
    fn every_call_variant_is_a_known_method() {
        // One representative JSON line per call; all must parse and be known.
        let lines = [
            r#"{"id":1,"method":"status"}"#,
            r#"{"id":1,"method":"get_config"}"#,
            r#"{"id":1,"method":"set_config","params":{"device":"hw:0,0"}}"#,
            r#"{"id":1,"method":"set_dc_block","params":null}"#,
            r#"{"id":1,"method":"set_crossfeed","params":null}"#,
            r#"{"id":1,"method":"set_eq","params":null}"#,
            r#"{"id":1,"method":"set_convolution","params":null}"#,
            r#"{"id":1,"method":"set_upsampler","params":{}}"#,
            r#"{"id":1,"method":"set_pitch","params":{"enabled":true,"cents":100.0,"quality":"high"}}"#,
            r#"{"id":1,"method":"set_harmonics","params":null}"#,
            r#"{"id":1,"method":"set_limiter","params":{"mode":"limit"}}"#,
            r#"{"id":1,"method":"set_dsp_enabled","params":{"enabled":true}}"#,
            r#"{"id":1,"method":"set_output","params":{"bits":24}}"#,
            r#"{"id":1,"method":"set_formats","params":["s32"]}"#,
            r#"{"id":1,"method":"set_rate","params":{}}"#,
            r#"{"id":1,"method":"set_device","params":{"device":"hw:0,0"}}"#,
            r#"{"id":1,"method":"set_ab","params":null}"#,
            r#"{"id":1,"method":"ab_select","params":{"side":"b","crossfade_ms":10.0}}"#,
            r#"{"id":1,"method":"set_volume","params":{"volume_db":-12.0,"balance_db":0.0,"muted":false}}"#,
            r#"{"id":1,"method":"set_volume_guarded","params":{"volume_db":-12.0,"balance_db":0.0,"muted":false,"if_instance":"a","if_revision":1}}"#,
            r#"{"id":1,"method":"list_streams"}"#,
            r#"{"id":1,"method":"list_presets"}"#,
            r#"{"id":1,"method":"get_preset","params":{"name":"X"}}"#,
            r#"{"id":1,"method":"save_preset","params":{"name":"X","overwrite":false}}"#,
            r#"{"id":1,"method":"load_preset","params":{"name":"X"}}"#,
            r#"{"id":1,"method":"delete_preset","params":{"name":"X"}}"#,
            r#"{"id":1,"method":"rename_preset","params":{"from":"X","to":"Y"}}"#,
            r#"{"id":1,"method":"import_apo","params":{"name":"X","text":"Preamp: -1 dB\\n"}}"#,
            r#"{"id":1,"method":"list_devices"}"#,
            r#"{"id":1,"method":"route","params":{"target":{"node":4},"sink":"dsp"}}"#,
            r#"{"id":1,"method":"set_routing_mode","params":{"mode":"system"}}"#,
            r#"{"id":1,"method":"now_playing"}"#,
            r#"{"id":1,"method":"self_test"}"#,
            r#"{"id":1,"method":"upsampler_response","params":{}}"#,
            r#"{"id":1,"method":"meters"}"#,
            r#"{"id":1,"method":"release_device"}"#,
            r#"{"id":1,"method":"acquire_device"}"#,
            r#"{"id":1,"method":"subscribe","params":{"topics":["meters"]}}"#,
            r#"{"id":1,"method":"shutdown"}"#,
        ];
        for line in lines {
            let value: Value = serde_json::from_str(line).unwrap();
            let method = value["method"].as_str().unwrap();
            assert!(is_known_method(method), "{method} is not a known method");
            serde_json::from_value::<Call>(value).unwrap_or_else(|err| {
                panic!("{line} does not parse as a Call: {err}");
            });
        }
        assert!(!is_known_method("set_nothing"));
        assert!(!is_known_method(""));
    }

    struct NoopEngine;

    impl crate::api::Engine for NoopEngine {
        fn apply(&self, _cfg: &EngineConfig) -> Result<(), crate::api::EngineError> {
            Ok(())
        }
        fn ab_select(
            &self,
            side: audivane_protocol::ipc::AbSide,
            _crossfade_ms: f64,
        ) -> Result<audivane_protocol::ipc::AbStatus, crate::api::EngineError> {
            Ok(audivane_protocol::ipc::AbStatus {
                enabled: true,
                current: Some(side),
                offset_db: 0.0,
                error: None,
                ..Default::default()
            })
        }
        fn snapshot(&self) -> crate::api::EngineSnapshot {
            crate::api::EngineSnapshot {
                state: audivane_protocol::ipc::EngineState::Idle,
                device: "hw:0,0".into(),
                error: None,
                format: None,
                graph_rate: 48_000,
                output_rate: 48_000,
                ratio: audivane_protocol::RateRatio::ONE,
                quantum: 1024,
                latency_ms: 0.0,
                filter_latency_ms: 0.0,
                conv_latency_ms: 0.0,
                xruns: 0,
                dsp_load: 0.0,
                dsp_active: false,
                active_sink: None,
                ab: None,
                volume: None,
                dsp_node: None,
                direct_node: None,
                frames_out: 0,
                chain_generation: 0,
                gaps: Default::default(),
                device_caps: None,
                effective_rates: Vec::new(),
                direct_rates: Vec::new(),
                playable_rates: Vec::new(),
                effective_default_rate: None,
                dither_bits: None,
                bypass_resampler: None,
                rt_load_max: 0.0,
                varispeed: None,
                waiting_for_device: false,
            }
        }
        fn meters(&self) -> audivane_protocol::ipc::Meters {
            audivane_protocol::ipc::Meters {
                peak_db: vec![-120.0],
                rms_db: vec![-120.0],
                clipped: 0,
                clipped_ch: vec![0],
                input_peak_db: vec![-120.0],
                chain_peak_db: Vec::new(),
                limiter: None,
                xruns: 0,
                dsp_load: 0.0,
            }
        }
        fn selftest(
            &self,
        ) -> Result<audivane_protocol::ipc::SelfTestResult, crate::api::EngineError> {
            todo!()
        }
        fn release_device(&self) -> Result<(), crate::api::EngineError> {
            Ok(())
        }
        fn acquire_device(&self) -> Result<(), crate::api::EngineError> {
            Ok(())
        }
        fn shutdown(&self) {}
    }

    fn test_inner() -> Arc<ServerInner> {
        let service = Service::new(
            Arc::new(NoopEngine),
            EngineConfig {
                device: "hw:0,0".into(),
                ..EngineConfig::default()
            },
            None,
            None,
            None,
            ShutdownFlag::new(),
        );
        Arc::new(ServerInner {
            service,
            conns: Mutex::new(HashMap::new()),
            conn_threads: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            running: AtomicBool::new(true),
        })
    }

    #[test]
    fn slow_clients_are_dropped_instead_of_blocking() {
        let inner = test_inner();
        let (slow_socket, _peer) = UnixStream::pair().unwrap();
        // Zero capacity: every send is "full", like a client that never reads.
        let (tx, _rx) = mpsc::sync_channel(0);
        inner.conns.lock().unwrap().insert(
            1,
            Conn {
                tx,
                stream: Arc::new(slow_socket),
                topics: HashSet::from([Topic::Meters]),
            },
        );
        assert_eq!(inner.subscribers(), HashSet::from([Topic::Meters]));
        inner.broadcast(Topic::Meters, b"{\"event\":\"meters\"}\n".to_vec());
        assert!(
            inner.conns.lock().unwrap().is_empty(),
            "the slow client should have been dropped"
        );
        assert!(inner.subscribers().is_empty());
    }

    #[test]
    fn events_reach_subscribed_connections_only() {
        let inner = test_inner();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::sync_channel(4);
        inner.conns.lock().unwrap().insert(
            1,
            Conn {
                tx,
                stream: Arc::new(socket),
                topics: HashSet::from([Topic::Meters]),
            },
        );
        inner.broadcast(Topic::Meters, b"meters\n".to_vec());
        inner.broadcast(Topic::Status, b"status\n".to_vec());
        assert_eq!(rx.try_recv().unwrap(), b"meters\n");
        assert!(rx.try_recv().is_err(), "only subscribed topics are queued");
    }

    /// A client end on a connection served by `inner`, with a reader that
    /// collects reply lines by id.
    fn connect(inner: &Arc<ServerInner>) -> (UnixStream, BufReader<UnixStream>) {
        let (server, client) = UnixStream::pair().unwrap();
        spawn_connection(inner, server).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let reader = BufReader::new(client.try_clone().unwrap());
        (client, reader)
    }

    fn next_reply(reader: &mut BufReader<UnixStream>) -> Response {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("{line:?}: {e}"))
    }

    /// A readout that simulates the stage with the level sweep: the slowest
    /// readout there is (tens of milliseconds in release, more in debug).
    fn slow_readout(id: u64) -> String {
        let config = serde_json::to_value(audivane_protocol::HarmonicsConfig::default()).unwrap();
        let request = serde_json::json!({
            "id": id,
            "method": "harmonics_readout",
            "params": { "config": config, "sweep": true },
        });
        format!("{request}\n")
    }

    #[test]
    fn quick_calls_overtake_a_running_readout_and_queued_readouts_are_superseded() {
        let inner = test_inner();
        let (mut client, mut reader) = connect(&inner);
        // 1 starts (or still waits when 2 arrives), 2 waits or replaces 1,
        // 3 replaces whichever waits (answered SUPERSEDED at once), then a
        // volume change that must not wait for the readout still running.
        let mut batch = slow_readout(1) + &slow_readout(2) + &slow_readout(3);
        batch.push_str(
            r#"{"id":4,"method":"set_volume","params":{"volume_db":-20.0,"balance_db":0.0,"muted":false}}"#,
        );
        batch.push('\n');
        client.write_all(batch.as_bytes()).unwrap();

        let replies: Vec<Response> = (0..4).map(|_| next_reply(&mut reader)).collect();
        let order: Vec<u64> = replies.iter().map(|r| r.id).collect();
        let superseded: Vec<u64> = replies
            .iter()
            .filter(|r| matches!(&r.result, Outcome::Error(e) if e.code == RpcError::SUPERSEDED))
            .map(|r| r.id)
            .collect();
        for reply in &replies {
            assert!(
                matches!(reply.result, Outcome::Ok(_)) || superseded.contains(&reply.id),
                "{reply:?}"
            );
        }
        // At most one runs and one waits: of three, 1 or 2 (or both, when
        // the worker had not picked up 1 yet) are dropped unrun.
        assert!(
            !superseded.is_empty() && superseded.iter().all(|id| [1, 2].contains(id)),
            "a queued readout is superseded: {order:?}"
        );
        assert_eq!(
            order.last(),
            Some(&3),
            "the newest readout is computed last: {order:?}"
        );
        let at = |id: u64| order.iter().position(|&x| x == id).unwrap();
        assert!(
            at(4) < at(3),
            "set_volume waited for the readout: {order:?}"
        );

        // Dropping the client ends the reader and its worker.
        client.write_all(slow_readout(5).as_bytes()).unwrap();
        drop(reader);
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            inner.prune_finished_threads();
            if inner.conn_threads.lock().unwrap().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "connection threads did not exit");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(inner.conns.lock().unwrap().is_empty());
    }

    #[test]
    fn a_half_closed_client_still_gets_its_readout() {
        let inner = test_inner();
        let (mut client, mut reader) = connect(&inner);
        client.write_all(slow_readout(7).as_bytes()).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let reply = next_reply(&mut reader);
        assert_eq!(reply.id, 7);
        assert!(matches!(reply.result, Outcome::Ok(_)), "{reply:?}");
    }

    #[test]
    fn defaults() {
        let path = socket_path(Some(Path::new("/tmp/x.sock"))).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/x.sock"));
        let long = PathBuf::from(format!("/tmp/{}", "a".repeat(200)));
        assert!(socket_path(Some(&long)).is_err());
    }
}
