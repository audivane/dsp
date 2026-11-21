//! Test helpers shared by the daemon integration tests.
#![allow(dead_code)]

use audivane_dsp_daemon::api::{Engine, EngineError, EngineSnapshot};
use audivane_dsp_daemon::control::config::ConfigStore;
use audivane_dsp_daemon::control::daemon::{Daemon, DaemonOptions};
use audivane_protocol::ipc::{
    AbSide, AbStatus, EngineConfig, EngineState, Meters, RpcError, SelfTestResult, Spectrum,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- temp dir

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique directory under `/tmp`, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(name: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("avdsp-it-{}-{name}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------- mock engine

/// Scriptable [`Engine`] for control-plane tests. Every method records that it
/// was called and can be made to fail.
pub type ApplyHook = Arc<dyn Fn(&EngineConfig) + Send + Sync>;

pub struct MockEngine {
    pub config: Mutex<EngineConfig>,
    pub snapshot: Mutex<EngineSnapshot>,
    pub meters: Mutex<Meters>,
    pub spectrum: Mutex<Option<Spectrum>>,
    pub spectrum_gen: AtomicU64,
    pub applies: AtomicU64,
    pub ab_selects: AtomicU64,
    pub selftests: AtomicU64,
    pub releases: AtomicU64,
    pub acquires: AtomicU64,
    pub shutdowns: AtomicU64,
    pub fail_apply: Mutex<Option<EngineError>>,
    pub fail_selftest: Mutex<Option<EngineError>>,
    pub on_apply: Mutex<Option<ApplyHook>>,
    /// Reported by [`Engine::cpu_rates`] (CPU admission off when `None`).
    pub cpu_rates: Mutex<Option<&'static audivane_dsp_core::cpu_bench::KernelRates>>,
    /// Varispeed as [`Engine::varispeed_request`] reports it, and every
    /// hold the overload fallback set (a retired stage stops running).
    pub varispeed: Mutex<audivane_dsp_daemon::api::VarispeedRequest>,
    pub varispeed_holds: Mutex<Vec<audivane_dsp_daemon::api::VarispeedHold>>,
    /// Every live `set_varispeed(enabled, speed, ramp_ms)` call received.
    pub varispeed_calls: Mutex<Vec<(bool, f64, f64)>>,
}

impl MockEngine {
    pub fn new(config: EngineConfig) -> Arc<Self> {
        let snapshot = EngineSnapshot {
            state: EngineState::Ready,
            device: config.device.clone(),
            error: None,
            format: None,
            graph_rate: 48_000,
            output_rate: 48_000,
            ratio: audivane_protocol::RateRatio::ONE,
            quantum: config.quantum,
            latency_ms: 7.5,
            filter_latency_ms: 0.0,
            xruns: 0,
            dsp_load: 0.0,
            dsp_active: false,
            active_sink: None,
            ab: None,
            volume: None,
            dsp_node: Some(101),
            direct_node: Some(102),
            frames_out: 0,
            chain_generation: 0,
            gaps: Default::default(),
            device_caps: None,
            effective_rates: Vec::new(),
            direct_rates: Vec::new(),
            effective_default_rate: None,
            dither_bits: None,
            bypass_resampler: None,
            rt_load_max: 0.0,
            varispeed: None,
            waiting_for_device: false,
        };
        Arc::new(Self {
            config: Mutex::new(config),
            snapshot: Mutex::new(snapshot),
            meters: Mutex::new(Meters {
                limiter: None,
                peak_db: vec![-23.0, -24.0],
                rms_db: vec![-30.0, -31.0],
                clipped: 0,
                clipped_ch: vec![0, 0],
                input_peak_db: vec![-15.0, -16.0],
                chain_peak_db: Vec::new(),
                xruns: 0,
                dsp_load: 0.25,
            }),
            spectrum: Mutex::new(None),
            spectrum_gen: AtomicU64::new(0),
            applies: AtomicU64::new(0),
            ab_selects: AtomicU64::new(0),
            selftests: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            acquires: AtomicU64::new(0),
            shutdowns: AtomicU64::new(0),
            fail_apply: Mutex::new(None),
            fail_selftest: Mutex::new(None),
            on_apply: Mutex::new(None),
            cpu_rates: Mutex::new(None),
            varispeed: Mutex::new(Default::default()),
            varispeed_holds: Mutex::new(Vec::new()),
            varispeed_calls: Mutex::new(Vec::new()),
        })
    }

    pub fn config_now(&self) -> EngineConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn set_state(&self, state: EngineState) {
        self.snapshot.lock().unwrap().state = state;
    }

    pub fn set_snapshot(&self, update: impl FnOnce(&mut EngineSnapshot)) {
        update(&mut self.snapshot.lock().unwrap());
    }

    pub fn set_meters(&self, update: impl FnOnce(&mut Meters)) {
        update(&mut self.meters.lock().unwrap());
    }

    /// Publish a new spectrum frame (and bump the generation the event pump
    /// watches).
    pub fn set_spectrum(&self, spectrum: Option<Spectrum>) {
        *self.spectrum.lock().unwrap() = spectrum;
        self.spectrum_gen.fetch_add(1, Ordering::Relaxed);
    }
}

impl Engine for MockEngine {
    fn set_varispeed_hold(&self, hold: audivane_dsp_daemon::api::VarispeedHold) -> u64 {
        use audivane_dsp_daemon::api::VarispeedHold;
        let mut holds = self.varispeed_holds.lock().unwrap();
        if holds.last().copied().unwrap_or(VarispeedHold::None) == hold {
            return holds.len() as u64;
        }
        holds.push(hold);
        // Each change is the next sequence, applied at once.
        let seq = holds.len() as u64;
        let mut vs = self.varispeed.lock().unwrap();
        vs.hold_ack = seq;
        match hold {
            VarispeedHold::None => vs.running = vs.wanted,
            VarispeedHold::Budget | VarispeedHold::Unity => vs.budget = 0.5,
            VarispeedHold::Off => vs.running = false,
        }
        seq
    }
    fn varispeed_request(&self) -> audivane_dsp_daemon::api::VarispeedRequest {
        *self.varispeed.lock().unwrap()
    }
    fn set_varispeed(
        &self,
        cfg: audivane_protocol::ipc::VarispeedConfig,
    ) -> Result<(), EngineError> {
        let audivane_protocol::ipc::VarispeedConfig {
            enabled,
            speed,
            ramp_ms,
            ..
        } = cfg;
        self.varispeed_calls
            .lock()
            .unwrap()
            .push((enabled, speed, ramp_ms));
        self.config.lock().unwrap().varispeed = cfg;
        {
            let mut vs = self.varispeed.lock().unwrap();
            vs.wanted = enabled;
            vs.running = enabled;
            vs.speed = speed;
            vs.requests += 1;
        }
        Ok(())
    }
    fn cpu_rates(&self) -> Option<&'static audivane_dsp_core::cpu_bench::KernelRates> {
        *self.cpu_rates.lock().unwrap()
    }
    fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
        let hook = self.on_apply.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(cfg);
        }
        if let Some(err) = self.fail_apply.lock().unwrap().clone() {
            return Err(err);
        }
        *self.config.lock().unwrap() = cfg.clone();
        {
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.device = cfg.device.clone();
            snapshot.quantum = cfg.quantum;
        }
        self.applies.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn ab_select(&self, side: AbSide, _crossfade_ms: f64) -> Result<AbStatus, EngineError> {
        self.ab_selects.fetch_add(1, Ordering::Relaxed);
        let status = AbStatus {
            enabled: true,
            current: side,
            offset_db: -2.5,
            error: None,
            ..Default::default()
        };
        self.snapshot.lock().unwrap().ab = Some(status.clone());
        Ok(status)
    }

    fn snapshot(&self) -> EngineSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    fn meters(&self) -> Meters {
        let meters = self.meters.lock().unwrap().clone();
        // Simulate a peak window reset on read.
        self.meters.lock().unwrap().peak_db = vec![-120.0; meters.peak_db.len()];
        self.meters.lock().unwrap().input_peak_db = vec![-120.0; meters.peak_db.len()];
        meters
    }

    fn spectrum(&self) -> Option<Spectrum> {
        self.spectrum.lock().unwrap().clone()
    }

    fn spectrum_generation(&self) -> u64 {
        self.spectrum_gen.load(Ordering::Relaxed)
    }

    fn selftest(&self) -> Result<SelfTestResult, EngineError> {
        if let Some(err) = self.fail_selftest.lock().unwrap().clone() {
            return Err(err);
        }
        self.selftests.fetch_add(1, Ordering::Relaxed);
        Ok(SelfTestResult {
            passed: true,
            frames: 4096,
            rate: 48_000,
            format: Some(audivane_protocol::SampleFormat::S32),
            expected_hash: "deadbeef".into(),
            actual_hash: "deadbeef".into(),
            detail: "mock loopback".into(),
        })
    }

    fn release_device(&self) -> Result<(), EngineError> {
        self.releases.fetch_add(1, Ordering::Relaxed);
        {
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.state = EngineState::Idle;
            snapshot.dsp_node = None;
            snapshot.direct_node = None;
        }
        Ok(())
    }

    fn acquire_device(&self) -> Result<(), EngineError> {
        self.acquires.fetch_add(1, Ordering::Relaxed);
        {
            let mut snapshot = self.snapshot.lock().unwrap();
            snapshot.state = EngineState::Ready;
            snapshot.dsp_node = Some(101);
            snapshot.direct_node = Some(102);
        }
        Ok(())
    }

    fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- harness

/// A daemon with a [`MockEngine`], no PipeWire/MPRIS watchers, a temp socket
/// and a temp config file.
pub struct Harness {
    pub daemon: Daemon,
    pub engine: Arc<MockEngine>,
    pub socket: PathBuf,
    pub config_path: PathBuf,
    pub dir: TempDir,
}

impl Harness {
    pub fn start(name: &str) -> Self {
        Self::start_with(name, |_| {})
    }

    pub fn start_with(name: &str, tweak: impl FnOnce(&mut EngineConfig)) -> Self {
        let dir = TempDir::new(name);
        let socket = dir.join("control.sock");
        let config_path = dir.join("config.json");
        let mut config = EngineConfig {
            device: "hw:0,0".into(),
            ..EngineConfig::default()
        };
        // These runs must never touch a real device or the session bus.
        config.routing = audivane_protocol::ipc::RoutingMode::Apps { rules: Vec::new() };
        tweak(&mut config);
        let engine = MockEngine::new(config.clone());
        let store = Arc::new(ConfigStore::new(config_path.clone()));
        let opts = DaemonOptions {
            store: Some(store),
            socket: socket.clone(),
            graph: false,
            mpris: false,
            mpris_interval: Duration::from_millis(100),
        };
        let daemon = Daemon::start(Arc::clone(&engine) as Arc<dyn Engine>, config, opts)
            .expect("start daemon");
        Self {
            daemon,
            engine,
            socket,
            config_path,
            dir,
        }
    }

    pub fn client(&self) -> TestClient {
        TestClient::connect(&self.socket)
    }

    pub fn config_json(&self) -> Value {
        let text = std::fs::read_to_string(&self.config_path).expect("read config");
        serde_json::from_str(&text).expect("config is JSON")
    }

    /// Wait (up to `timeout`) for the persisted config to satisfy `predicate`.
    /// Writes are coalesced, so a reply can arrive before `config.json` does.
    pub fn wait_for_config(
        &self,
        timeout: Duration,
        predicate: impl Fn(&EngineConfig) -> bool,
    ) -> EngineConfig {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let config = std::fs::read_to_string(&self.config_path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| {
                    let config = value.get("config").cloned().unwrap_or(value);
                    serde_json::from_value::<EngineConfig>(config).ok()
                });
            if let Some(config) = config
                && predicate(&config)
            {
                return config;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "config.json did not reach the expected state within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

// ---------------------------------------------------------------- client

/// Minimal line-oriented client speaking the raw protocol.
pub struct TestClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl TestClient {
    pub fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).expect("connect to daemon");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Self {
            stream,
            reader,
            next_id: 1,
        }
    }

    pub fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).expect("write request");
        self.stream.flush().expect("flush request");
    }

    /// Send a request line and return the raw reply (or `None` on EOF).
    pub fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line),
            Err(err) => panic!("read failed: {err}"),
        }
    }

    /// Send one call and return the id used.
    pub fn send(&mut self, call: audivane_protocol::ipc::Call) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let request = audivane_protocol::ipc::Request { id, call };
        let mut line = serde_json::to_vec(&request).unwrap();
        line.push(b'\n');
        self.send_raw(&line);
        id
    }

    /// Send a call and return the parsed reply, skipping event lines.
    pub fn call(&mut self, call: audivane_protocol::ipc::Call) -> Reply {
        let id = self.send(call);
        loop {
            let line = self.read_line().expect("daemon closed the connection");
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if value.get("event").is_some() {
                continue;
            }
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return Reply::from_value(value);
        }
    }

    /// Call and require success.
    pub fn ok(&mut self, call: audivane_protocol::ipc::Call) -> Value {
        match self.call(call) {
            Reply::Ok(value) => value,
            Reply::Error(error) => panic!("unexpected error reply: {error:?}"),
        }
    }

    pub fn subscribe(&mut self, topics: Vec<audivane_protocol::ipc::Topic>) {
        self.ok(audivane_protocol::ipc::Call::Subscribe { topics });
    }

    /// Next event line with `event == expected`, within `timeout`.
    pub fn next_event(
        &mut self,
        expected: &str,
        timeout: Duration,
    ) -> Option<audivane_protocol::ipc::Event> {
        self.stream.set_read_timeout(Some(timeout)).unwrap();
        loop {
            let line = self.read_line()?;
            let value: Value = serde_json::from_str(line.trim()).ok()?;
            if value.get("event").and_then(Value::as_str) == Some(expected) {
                return serde_json::from_value(value).ok();
            }
        }
    }
}

#[derive(Debug)]
pub enum Reply {
    Ok(Value),
    Error(RpcError),
}

impl Reply {
    fn from_value(value: Value) -> Self {
        if let Some(error) = value.get("error") {
            Reply::Error(serde_json::from_value(error.clone()).expect("error reply"))
        } else {
            Reply::Ok(value.get("ok").cloned().unwrap_or(Value::Null))
        }
    }
}

// ---------------------------------------------------------------- cli

/// Path to the `avdsp` binary, building it if this test run only built the
/// daemon package.
pub fn avdsp_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("AVDSP_BIN") {
        return PathBuf::from(path);
    }
    let target = target_profile_dir();
    let bin = target.join("avdsp");
    if bin.exists() {
        return bin;
    }
    build_avdsp(&target);
    assert!(
        bin.exists(),
        "{} was not built; run `cargo build -p audivane-dsp-cli`",
        bin.display()
    );
    bin
}

/// Locate `target/debug` (or `target/<profile>`), wherever the harness put it.
fn target_profile_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let base = PathBuf::from(dir);
        for profile in ["debug", "release"] {
            let dir = base.join(profile);
            // Newer cargo keeps `deps/` in a separate build dir, so also accept
            // the profile dir when it holds the lock marker or the binary.
            if dir.join("deps").is_dir()
                || dir.join(".cargo-lock").exists()
                || dir.join(".cargo-build-lock").exists()
                || dir.join("avdsp").exists()
            {
                return dir;
            }
        }
        return base;
    }
    let exe = std::env::current_exe().expect("current exe");
    // Nextest may place test binaries under `target/<profile>/build/<pkg>/<hash>/out`,
    // so look for cargo's profile marker rather than assuming `deps/`.
    for ancestor in exe.ancestors() {
        if ancestor.join(".cargo-lock").exists() || ancestor.join(".cargo-build-lock").exists() {
            return ancestor.to_path_buf();
        }
    }
    // Fall back to the conventional layout.
    exe.parent()
        .and_then(Path::parent)
        .expect("target dir")
        .to_path_buf()
}

fn build_avdsp(target: &Path) {
    // Serialize the build across test processes; nextest runs every test in its
    // own process and `cargo build` would otherwise contend for the lock.
    let lock_path = target.join(".avdsp-it-build.lock");
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .expect("create build lock");
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&lock);
    // SAFETY: flock on a valid fd.
    unsafe {
        libc::flock(fd, libc::LOCK_EX);
    }
    if !target.join("avdsp").exists() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.parent().and_then(Path::parent).expect("workspace");
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .current_dir(root)
            .args(["build", "-p", "audivane-dsp-cli", "--bin", "avdsp"])
            .status()
            .expect("run cargo build");
        assert!(status.success(), "building avdsp failed");
    }
    // SAFETY: same fd.
    unsafe {
        libc::flock(fd, libc::LOCK_UN);
    }
}

/// Run `avdsp ctl --socket <socket> ...` and capture the output.
pub fn ctl(socket: &Path, args: &[&str]) -> Output {
    let bin = avdsp_bin();
    let mut command = Command::new(bin);
    command
        .arg("ctl")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .env_remove("AUDIVANE_DSP_SOCKET")
        .env_remove("XDG_RUNTIME_DIR");
    command.output().expect("run avdsp ctl")
}

pub fn ctl_ok(socket: &Path, args: &[&str]) -> String {
    let output = ctl(socket, args);
    assert!(
        output.status.success(),
        "avdsp ctl {args:?} failed ({:?})\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn ctl_json(socket: &Path, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    let stdout = ctl_ok(socket, &args);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
        panic!("not JSON: {err}\n{stdout}");
    })
}

pub fn read_all(mut stream: impl Read) -> String {
    let mut text = String::new();
    stream.read_to_string(&mut text).ok();
    text
}
