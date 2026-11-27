//! PipeWire graph watcher + routing, against two temporary null sinks and a
//! real `pw-play` stream.
//!
//! Ignored by default because it needs a live PipeWire session and the
//! `pactl`/`pw-play`/`pw-dump`/`pw-metadata` tools. Run with:
//!
//! ```sh
//! cargo nextest run -p audivane-dsp-daemon --run-ignored all -E 'test(graph_it)'
//! # including the session-default parts (restored exactly, f3_* sinks only):
//! AUDIVANE_IT_DEFAULT_SINK=1 cargo nextest run -p audivane-dsp-daemon \
//!     --run-ignored all -E 'test(graph_it)'
//! ```
//!
//! Everything it creates is prefixed `f3_` and removed afterwards; the session
//! default (both `default.audio.sink` and `default.configured.audio.sink`) is
//! restored exactly. Tests that touch the default hold a file lock, because
//! nextest runs each test in its own process. It never opens `hw:Loopback` and
//! never restarts PipeWire/WirePlumber.

use audivane_dsp_daemon::control::graph::{GraphOptions, GraphWatcher};
use audivane_protocol::ipc::SinkChoice;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn have(command: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {command}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run(command: &str, args: &[&str]) -> Output {
    Command::new(command)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("cannot run {command}: {err}"))
}

fn wait_until<F: FnMut() -> bool>(what: &str, timeout: Duration, mut check: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("timed out waiting for {what}");
    false
}

fn load_null_sink(name: &str) -> String {
    let output = run(
        "pactl",
        &[
            "load-module",
            "module-null-sink",
            &format!("sink_name={name}"),
            &format!("sink_properties=device.description={name}"),
        ],
    );
    assert!(
        output.status.success(),
        "pactl load-module failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn unload(module: &str) {
    let _ = run("pactl", &["unload-module", module]);
}

/// Raw `default` metadata values (the JSON text, or `None` when absent).
fn metadata_values() -> (Option<String>, Option<String>) {
    let output = run("pw-dump", &[]);
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(dump) = serde_json::from_str::<serde_json::Value>(&text).ok() else {
        return (None, None);
    };
    let mut effective = None;
    let mut configured = None;
    for object in dump.as_array().into_iter().flatten() {
        if object["type"] != "PipeWire:Interface:Metadata" {
            continue;
        }
        if object["props"]["metadata.name"] != "default" {
            continue;
        }
        for entry in object["metadata"].as_array().into_iter().flatten() {
            let value = entry["value"].to_string();
            match entry["key"].as_str() {
                Some("default.audio.sink") => effective = Some(value),
                Some("default.configured.audio.sink") => configured = Some(value),
                _ => {}
            }
        }
    }
    (effective, configured)
}

fn metadata_name(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| {
            value
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
}

fn set_metadata(key: &str, raw: Option<&str>) {
    let output = match raw {
        Some(raw) => run(
            "pw-metadata",
            &["-n", "default", "0", key, raw, "Spa:String:JSON"],
        ),
        None => run("pw-metadata", &["-n", "default", "-d", "0", key]),
    };
    assert!(
        output.status.success(),
        "pw-metadata {key} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Exclusive lock so the two process-per-test runs do not fight over the
/// session default.
struct SessionDefaultLock {
    file: std::fs::File,
}

impl SessionDefaultLock {
    fn acquire() -> Self {
        let path = std::env::temp_dir().join("avdsp-f3-default-sink.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .expect("open the session-default lock");
        // SAFETY: flock on a valid fd.
        let rc =
            unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
        assert_eq!(rc, 0, "flock the session-default lock");
        Self { file }
    }
}

impl Drop for SessionDefaultLock {
    fn drop(&mut self) {
        // SAFETY: same fd.
        unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&self.file),
                libc::LOCK_UN,
            );
        }
    }
}

/// The session default as it was before the test, restored exactly (including
/// "absent") even when the test panics.
struct DefaultGuard {
    _lock: SessionDefaultLock,
    effective: Option<String>,
    configured: Option<String>,
}

impl DefaultGuard {
    fn capture() -> Self {
        // Take the lock first, then read, so a mid-change state is never
        // recorded as the value to restore.
        let lock = SessionDefaultLock::acquire();
        let (effective, configured) = metadata_values();
        Self {
            _lock: lock,
            effective,
            configured,
        }
    }

    fn restore(&self) {
        set_metadata("default.audio.sink", self.effective.as_deref());
        set_metadata("default.configured.audio.sink", self.configured.as_deref());
        assert!(
            wait_until("default restored", Duration::from_secs(5), || {
                metadata_values() == (self.effective.clone(), self.configured.clone())
            }),
            "default not restored exactly: {:?}",
            metadata_values()
        );
    }
}

impl Drop for DefaultGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn skip_default_tests() -> bool {
    std::env::var_os("AUDIVANE_IT_DEFAULT_SINK").is_none()
}

/// Snapshot/restore of the two default keys: a configured preference that
/// differs from the effective default, a normal stop, and a `kill -9`.
#[test]
#[ignore = "requires a live PipeWire session, pactl and pw-metadata"]
fn graph_it_default_sink_snapshot_restore() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("avdsp_it=debug,audivane_dsp_daemon=debug"),
    )
    .try_init();
    for tool in ["pactl", "pw-play", "pw-dump", "pw-metadata"] {
        if !have(tool) {
            eprintln!("skipping graph test: {tool} not available");
            return;
        }
    }
    if run("pactl", &["info"]).status.code() != Some(0) {
        eprintln!("skipping graph test: no PipeWire/PulseAudio session");
        return;
    }
    if skip_default_tests() {
        eprintln!("skipping (set AUDIVANE_IT_DEFAULT_SINK=1 to run it)");
        return;
    }

    let guard = DefaultGuard::capture();
    guard.restore();
    let (orig_effective, orig_configured) = metadata_values();
    let orig_effective_name = metadata_name(&orig_effective);
    let orig_configured_name = metadata_name(&orig_configured);

    let suffix = std::process::id();
    let a_name = format!("f3_it_a_{suffix}");
    let preferred_name = format!("f3_it_pref_{suffix}");
    let module_a = load_null_sink(&a_name);
    let module_preferred = load_null_sink(&preferred_name);
    let dir = std::env::temp_dir().join(format!("avdsp-default-it-{suffix}"));
    std::fs::create_dir_all(&dir).unwrap();

    let cleanup = SinkCleanup {
        modules: vec![module_a, module_preferred.clone()],
        dir: dir.clone(),
    };

    let graph = GraphWatcher::spawn(GraphOptions {
        dsp_sink_name: a_name.clone(),
        direct_sink_name: a_name.clone(),
        rules: Vec::new(),
        system_default: false,
        restore_path: Some(dir.join("scenario-a.json")),
    })
    .expect("start graph watcher");
    let restore_a = dir.join("scenario-a.json");

    // ---- unplugged preference: configured != effective -------------------
    // Make the preferred sink disappear while it is the configured choice:
    // WirePlumber keeps `default.configured.audio.sink` and falls the
    // effective default back to an available device.
    set_metadata(
        "default.configured.audio.sink",
        Some(&format!(r#"{{"name":"{preferred_name}"}}"#)),
    );
    assert!(
        wait_until("preferred sink effective", Duration::from_secs(5), || {
            graph.facts().default_sink.as_deref() == Some(preferred_name.as_str())
        }),
        "configured preference not applied: {:?}",
        graph.facts()
    );
    unload(&module_preferred);
    let gone = wait_until("preferred sink gone", Duration::from_secs(5), || {
        let facts = graph.facts();
        facts.sink(SinkChoice::Dsp).is_some()
            && facts.default_configured_sink.as_deref() == Some(preferred_name.as_str())
            && facts.default_sink.as_deref() != Some(preferred_name.as_str())
    });
    assert!(
        gone,
        "expected the preference to survive the sink: {:?}",
        graph.facts()
    );
    let (before_effective, before_configured) = metadata_values();
    let before_effective_name = metadata_name(&before_effective);
    assert_ne!(before_effective, before_configured);

    // Taking over the default must snapshot both keys, independently.
    graph.set_system_default(true);
    assert!(
        wait_until("take over both keys", Duration::from_secs(5), || {
            let (effective, configured) = metadata_values();
            metadata_name(&effective).as_deref() == Some(a_name.as_str())
                && metadata_name(&configured).as_deref() == Some(a_name.as_str())
        }),
        "system default not taken over: {:?}",
        metadata_values()
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&restore_a).expect("restore file"))
            .expect("restore file is JSON");
    assert_eq!(
        metadata_name(&saved["effective"].as_str().map(str::to_string)),
        before_effective_name,
        "saved effective default: {saved}"
    );
    assert_eq!(
        metadata_name(&saved["configured"].as_str().map(str::to_string)),
        Some(preferred_name.clone()),
        "saved configured preference: {saved}"
    );

    // Leaving system mode restores each key to what it was, not the effective
    // value into both.
    graph.set_system_default(false);
    assert!(
        wait_until("restore both keys", Duration::from_secs(5), || {
            metadata_values() == (before_effective.clone(), before_configured.clone())
        }),
        "default not restored independently: {:?} (expected {:?}, {:?})",
        metadata_values(),
        before_effective,
        before_configured
    );
    assert!(
        wait_until("restore file removed", Duration::from_secs(2), || {
            !restore_a.exists()
        }),
        "restore file must be removed after restore sync"
    );
    assert_eq!(
        graph.facts().default_configured_sink,
        Some(preferred_name.clone()),
        "the configured preference must survive"
    );
    guard.restore();
    graph.shutdown();

    // ---- crash safety: kill -9 -------------------------------------------
    let restore_b = dir.join("scenario-b.json");
    let first = GraphWatcher::spawn(GraphOptions {
        dsp_sink_name: a_name.clone(),
        direct_sink_name: a_name.clone(),
        rules: Vec::new(),
        system_default: true,
        restore_path: Some(restore_b.clone()),
    })
    .expect("start graph watcher (crash scenario)");
    assert!(
        wait_until("takeover before crash", Duration::from_secs(5), || {
            let (effective, configured) = metadata_values();
            metadata_name(&effective).as_deref() == Some(a_name.as_str())
                && metadata_name(&configured).as_deref() == Some(a_name.as_str())
        }),
        "default not taken over: {:?}",
        metadata_values()
    );
    assert!(restore_b.is_file(), "the snapshot must be persisted first");
    // Behave like `kill -9`: the loop stops, nothing is restored.
    first.stop_without_restore();
    let after_crash = metadata_values();
    assert_eq!(
        metadata_name(&after_crash.0),
        Some(a_name.clone()),
        "a crash leaves our sink as the effective default: {after_crash:?}"
    );
    assert_eq!(
        metadata_name(&after_crash.1),
        Some(a_name.clone()),
        "a crash leaves our sink as the configured default: {after_crash:?}"
    );
    assert!(restore_b.is_file(), "a crash leaves the snapshot behind");

    // The next start repairs it before doing anything else.
    let second = GraphWatcher::spawn(GraphOptions {
        dsp_sink_name: a_name.clone(),
        direct_sink_name: a_name.clone(),
        rules: Vec::new(),
        system_default: false,
        restore_path: Some(restore_b.clone()),
    })
    .expect("start graph watcher (recovery)");
    assert!(
        wait_until("crash recovery", Duration::from_secs(5), || {
            metadata_values() == (orig_effective.clone(), orig_configured.clone())
        }),
        "default not recovered: {:?} (expected {:?}, {:?})",
        metadata_values(),
        orig_effective,
        orig_configured
    );
    assert!(!restore_b.exists(), "the snapshot is consumed");
    second.shutdown();

    // ---- a stale snapshot never clobbers someone else's default ----------
    let restore_c = dir.join("scenario-c.json");
    std::fs::write(
        &restore_c,
        serde_json::json!({
            "written": [a_name.clone()],
            // Raw metadata values, as stored by `DefaultSnapshot`.
            "effective": "{\"name\":\"f3_it_never_1\"}",
            "configured": "{\"name\":\"f3_it_never_2\"}",
        })
        .to_string(),
    )
    .unwrap();
    let third = GraphWatcher::spawn(GraphOptions {
        dsp_sink_name: a_name.clone(),
        direct_sink_name: a_name.clone(),
        rules: Vec::new(),
        system_default: false,
        restore_path: Some(restore_c.clone()),
    })
    .expect("start graph watcher (stale snapshot)");
    assert!(
        wait_until("stale snapshot consumed", Duration::from_secs(5), || {
            !restore_c.exists()
        }),
        "the stale snapshot was not consumed"
    );
    assert_eq!(
        metadata_values(),
        (orig_effective.clone(), orig_configured.clone()),
        "keys that no longer point at our sink must be left alone"
    );
    third.shutdown();

    assert_eq!(orig_effective_name, metadata_name(&orig_effective));
    assert_eq!(orig_configured_name, metadata_name(&orig_configured));
    // Unload our sinks first, then put the user's default back (the guard's
    // Drop repeats the restore harmlessly).
    drop(cleanup);
    guard.restore();
}

struct SinkCleanup {
    modules: Vec<String>,
    dir: PathBuf,
}

impl Drop for SinkCleanup {
    fn drop(&mut self) {
        for module in &self.modules {
            unload(module);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
