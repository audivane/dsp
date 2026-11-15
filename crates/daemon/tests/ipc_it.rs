//! End-to-end tests of the IPC server + service with a mock engine.
//!
//! Each test runs the real [`Daemon`] (temp socket, temp config) and speaks the
//! wire protocol; nothing here touches ALSA, PipeWire or D-Bus.

mod common;

use audivane_dsp_daemon::api::{Engine, EngineError};
use audivane_dsp_daemon::control::daemon::{Daemon, DaemonOptions};
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{
    AbConfig, AbSide, AbStatus, AppRule, Call, DeviceInfo, EngineConfig, EngineState, LevelMatch,
    PassthroughBreak, Preset, PresetInfo, PresetState, RouteTarget, RoutingMode, RpcError,
    SinkChoice, Spectrum, Status, StreamInfo, Topic,
};
use audivane_protocol::{
    Band, ChainConfig, DitherKind, EqProfile, FilterType, OutputConfig, RateTarget, UpsamplerConfig,
};
use common::{Harness, MockEngine, Reply, TempDir, TestClient};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn quiet_options(socket: &Path) -> DaemonOptions {
    DaemonOptions {
        store: None,
        socket: socket.to_path_buf(),
        graph: false,
        mpris: false,
        mpris_interval: Duration::from_millis(100),
    }
}

fn profile() -> EqProfile {
    EqProfile {
        name: "test".into(),
        preamp_db: -2.0,
        bands: vec![Band {
            kind: FilterType::Peaking,
            freq_hz: 1000.0,
            gain_db: 3.0,
            q: 1.0,
            enabled: true,
        }],
        ..Default::default()
    }
}

#[test]
fn every_call_round_trips() {
    let harness = Harness::start("every-call");
    let mut client = harness.client();

    // status
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.device, "hw:0,0");
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(status.quantum, 1024);
    assert_eq!(status.graph_rate, 48_000);
    assert_eq!(status.output_rate, 48_000);
    assert!(
        status
            .passthrough
            .broken_because
            .contains(&PassthroughBreak::NotPlaying)
    );

    // get_config / set_config
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.device, "hw:0,0");
    let replaced = EngineConfig {
        device: "hw:1,0".into(),
        quantum: 512,
        ..EngineConfig::default()
    };
    client.ok(Call::SetConfig(replaced.clone()));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.device, "hw:1,0");
    assert_eq!(config.quantum, 512);
    // Persisted on disk with the version envelope.
    let on_disk = harness.wait_for_config(Duration::from_secs(3), |cfg| cfg.device == "hw:1,0");
    assert_eq!(on_disk.quantum, 512);
    let value = harness.config_json();
    assert_eq!(value["version"], 1);
    assert_eq!(value["config"]["device"], "hw:1,0");
    assert_eq!(harness.engine.config_now().device, "hw:1,0");

    // set_eq
    client.ok(Call::SetEq(Some(profile())));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.chain.eq.as_ref().unwrap().bands.len(), 1);
    client.ok(Call::SetEq(None));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.chain.eq.is_none());

    // set_convolution (a real file, because the daemon checks it)
    let ir = harness.dir.write("ir.wav", "not really a wav");
    client.ok(Call::SetConvolution(vec![
        audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: ir.clone(),
            gain_db: -1.5,
        },
    ]));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.chain.convolution.len(), 1);
    assert_eq!(config.chain.convolution[0].ir_path, ir);
    client.ok(Call::SetConvolution(Vec::new()));

    // set_upsampler
    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Ratio(4),
        taps: 8192,
        ..UpsamplerConfig::default()
    }));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.chain.upsampler.target, RateTarget::Ratio(4));
    client.ok(Call::SetUpsampler(UpsamplerConfig::default()));

    // set_dsp_enabled
    client.ok(Call::SetDspEnabled { enabled: false });
    assert!(!harness.engine.config_now().dsp_enabled);
    client.ok(Call::SetDspEnabled { enabled: true });

    // set_ab / ab_select
    let chain_b = ChainConfig {
        eq: Some(profile()),
        ..harness.engine.config_now().chain.clone()
    };
    let ab: AbStatus = serde_json::from_value(client.ok(Call::SetAb(Some(AbConfig {
        chain_b,
        level: LevelMatch::Manual { offset_db: -3.0 },
    }))))
    .unwrap();
    assert!(ab.enabled);
    assert_eq!(ab.current, AbSide::A);
    let ab: AbStatus = serde_json::from_value(client.ok(Call::AbSelect {
        side: AbSide::B,
        crossfade_ms: 25.0,
    }))
    .unwrap();
    assert_eq!(ab.current, AbSide::B);
    assert_eq!(ab.offset_db, -2.5);
    assert_eq!(
        harness
            .engine
            .ab_selects
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    client.ok(Call::SetAb(None));

    // set_volume
    let vol: audivane_protocol::ipc::VolumeStatus =
        serde_json::from_value(client.ok(Call::SetVolume {
            volume_db: -12.5,
            balance_db: 3.0,
            muted: true,
        }))
        .unwrap();
    assert_eq!(vol.volume_db, -12.5);
    assert_eq!(vol.balance_db, 3.0);
    assert!(vol.muted);
    assert!(!vol.hardware);
    assert_eq!(vol.min_db, -120.0);
    assert_eq!(vol.max_db, 0.0);
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.volume.volume_db, -12.5);
    assert_eq!(config.volume.balance_db, 3.0);
    assert!(config.volume.muted);
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.volume, Some(vol));
    client.ok(Call::SetVolume {
        volume_db: 0.0,
        balance_db: 0.0,
        muted: false,
    });

    // list_streams (no graph watcher: empty)
    let streams: Vec<StreamInfo> = serde_json::from_value(client.ok(Call::ListStreams)).unwrap();
    assert!(streams.is_empty());

    // route (no graph: BAD_STATE)
    match client.call(Call::Route {
        target: RouteTarget::Node(7),
        sink: SinkChoice::Dsp,
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::BAD_STATE),
        other => panic!("unexpected {other:?}"),
    }

    // set_routing_mode
    client.ok(Call::SetRoutingMode(RoutingMode::Apps {
        rules: vec![AppRule {
            app: "strawberry".into(),
            sink: SinkChoice::Dsp,
        }],
    }));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    match config.routing {
        RoutingMode::Apps { rules } => {
            assert_eq!(rules.len(), 1);
            assert_eq!(rules[0].app, "strawberry");
        }
        other => panic!("unexpected routing {other:?}"),
    }
    client.ok(Call::SetRoutingMode(RoutingMode::System));

    // now_playing
    assert_eq!(client.ok(Call::NowPlaying), Value::Null);

    // self_test
    let result: audivane_protocol::ipc::SelfTestResult =
        serde_json::from_value(client.ok(Call::SelfTest)).unwrap();
    assert!(result.passed);
    assert_eq!(result.frames, 4096);
    assert_eq!(result.format, Some(SampleFormat::S32));

    // release / acquire
    client.ok(Call::ReleaseDevice);
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.state, EngineState::Idle);
    client.ok(Call::AcquireDevice);
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.state, EngineState::Ready);
    assert_eq!(
        harness
            .engine
            .releases
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        harness
            .engine
            .acquires
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        harness
            .engine
            .shutdowns
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn error_paths() {
    let harness = Harness::start("errors");
    let mut client = harness.client();

    // Malformed JSON -> PARSE, id 0.
    client.send_raw(b"{not json}\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["id"], 0);
    assert_eq!(value["error"]["code"], RpcError::PARSE);

    // Empty line.
    client.send_raw(b"\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["error"]["code"], RpcError::PARSE);

    // Unknown method.
    client.send_raw(br#"{"id":5,"method":"make_coffee"}"#);
    client.send_raw(b"\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["id"], 5);
    assert_eq!(value["error"]["code"], RpcError::INVALID_REQUEST);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("make_coffee")
    );

    // Missing id.
    client.send_raw(br#"{"method":"status"}"#);
    client.send_raw(b"\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["error"]["code"], RpcError::INVALID_REQUEST);

    // Bad params (wrong type).
    client.send_raw(br#"{"id":6,"method":"set_dsp_enabled","params":{"enabled":"yes"}}"#);
    client.send_raw(b"\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["id"], 6);
    assert_eq!(value["error"]["code"], RpcError::INVALID_PARAMS);

    // Missing params (a struct variant needs them).
    client.send_raw(br#"{"id":7,"method":"set_dsp_enabled"}"#);
    client.send_raw(b"\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["error"]["code"], RpcError::INVALID_PARAMS);

    // Validation errors name the field.
    match client.call(Call::SetUpsampler(UpsamplerConfig {
        taps: 0,
        ..UpsamplerConfig::default()
    })) {
        Reply::Error(error) => {
            assert_eq!(error.code, RpcError::INVALID_PARAMS);
            assert!(error.message.contains("taps"), "{}", error.message);
        }
        other => panic!("unexpected {other:?}"),
    }
    match client.call(Call::SetConvolution(vec![
        audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: "/nonexistent/t5b-ir.wav".into(),
            gain_db: 0.0,
        },
    ])) {
        Reply::Error(error) => {
            assert_eq!(error.code, RpcError::INVALID_PARAMS);
            assert!(error.message.contains("ir_path"));
        }
        other => panic!("unexpected {other:?}"),
    }

    // A/B not configured.
    match client.call(Call::AbSelect {
        side: AbSide::B,
        crossfade_ms: 10.0,
    }) {
        Reply::Error(error) => {
            assert_eq!(error.code, RpcError::INVALID_PARAMS);
            assert_eq!(error.message, "A/B is not configured");
        }
        other => panic!("unexpected {other:?}"),
    }

    // Bad device.
    match client.call(Call::SetConfig(EngineConfig {
        device: "pulse".into(),
        ..EngineConfig::default()
    })) {
        Reply::Error(error) => {
            assert_eq!(error.code, RpcError::INVALID_PARAMS);
            assert!(error.message.contains("hw:"), "{}", error.message);
        }
        other => panic!("unexpected {other:?}"),
    }

    // Engine errors keep their code.
    *harness.engine.fail_selftest.lock().unwrap() =
        Some(EngineError::new(RpcError::BAD_STATE, "device released"));
    match client.call(Call::SelfTest) {
        Reply::Error(error) => {
            assert_eq!(error.code, RpcError::BAD_STATE);
            assert_eq!(error.message, "device released");
        }
        other => panic!("unexpected {other:?}"),
    }
    *harness.engine.fail_apply.lock().unwrap() =
        Some(EngineError::new(RpcError::IO, "snd_pcm_open: EBUSY"));
    match client.call(Call::SetDspEnabled { enabled: false }) {
        Reply::Error(error) => {
            assert_eq!(error.code, RpcError::IO);
            assert!(error.message.contains("EBUSY"));
        }
        other => panic!("unexpected {other:?}"),
    }
    // A failed edit must not change the stored config.
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.dsp_enabled);

    // Oversized line: PARSE, then the connection is closed (no resync).
    let mut big = vec![b'a'; 1024 * 1024 + 16];
    big.push(b'\n');
    client.send_raw(&big);
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["error"]["code"], RpcError::PARSE);
    assert!(client.read_line().is_none(), "connection should be closed");

    // The daemon is still healthy for new clients.
    let mut fresh = harness.client();
    let status: Status = serde_json::from_value(fresh.ok(Call::Status)).unwrap();
    assert_eq!(status.device, "hw:0,0");
}

#[test]
fn subscription_pushes_state_and_meters() {
    let harness = Harness::start("subscribe");
    harness.engine.set_state(EngineState::Playing);
    harness.engine.set_snapshot(|snapshot| {
        snapshot.dsp_active = true;
        snapshot.ratio = 4;
        snapshot.output_rate = 192_000;
    });
    let mut client = harness.client();
    client.subscribe(vec![
        Topic::Meters,
        Topic::Status,
        Topic::Streams,
        Topic::NowPlaying,
    ]);

    // The initial status/streams/now_playing snapshots arrive immediately.
    let status = client.next_event("status", Duration::from_secs(2)).unwrap();
    match status {
        audivane_protocol::ipc::Event::Status(status) => {
            assert_eq!(status.state, EngineState::Playing);
            assert_eq!(status.ratio, 4);
        }
        other => panic!("unexpected {other:?}"),
    }
    let streams = client
        .next_event("streams", Duration::from_secs(2))
        .unwrap();
    match streams {
        audivane_protocol::ipc::Event::Streams(streams) => assert!(streams.is_empty()),
        other => panic!("unexpected {other:?}"),
    }
    let now = client
        .next_event("now_playing", Duration::from_secs(2))
        .unwrap();
    match now {
        audivane_protocol::ipc::Event::NowPlaying(now) => assert!(now.is_none()),
        other => panic!("unexpected {other:?}"),
    }

    // Meters at ~30 Hz.
    let start = Instant::now();
    let mut meters = 0;
    while start.elapsed() < Duration::from_millis(1000) {
        if let Some(audivane_protocol::ipc::Event::Meters(m)) =
            client.next_event("meters", Duration::from_millis(500))
        {
            assert_eq!(m.peak_db.len(), 2);
            meters += 1;
        }
    }
    assert!(
        (15..=40).contains(&meters),
        "expected ~30 meters events, got {meters}"
    );

    // Status changes are pushed without asking.
    harness.engine.set_state(EngineState::Error);
    harness
        .engine
        .set_snapshot(|s| s.error = Some("test failure".into()));
    let mut saw_error = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match client.next_event("status", Duration::from_millis(500)) {
            Some(audivane_protocol::ipc::Event::Status(status)) => {
                if status.state == EngineState::Error {
                    assert_eq!(status.error.as_deref(), Some("test failure"));
                    saw_error = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(saw_error, "expected a status event with the error state");
}

#[test]
fn concurrent_clients() {
    let harness = Harness::start("concurrent");
    let socket = harness.socket.clone();
    let mut handles = Vec::new();
    for _ in 0..16 {
        let socket = socket.clone();
        handles.push(std::thread::spawn(move || {
            let mut client = TestClient::connect(&socket);
            for _ in 0..50 {
                let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
                assert_eq!(status.device, "hw:0,0");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("client thread");
    }
}

#[test]
fn client_disconnect_mid_request_does_not_fail_the_daemon() {
    let harness = Harness::start("disconnect");
    {
        let mut client = TestClient::connect(&harness.socket);
        client.send_raw(br#"{"id":1,"method":"sta"#);
        // Dropped without a newline.
    }
    {
        let mut client = TestClient::connect(&harness.socket);
        client.send_raw(br#"{"id":2,"method":"status"}"#);
        // Dropped before reading the reply.
    }
    let mut client = harness.client();
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.device, "hw:0,0");
}

#[test]
fn shutdown_replies_then_stops_and_removes_the_socket() {
    let harness = Harness::start("shutdown");
    let socket = harness.socket.clone();
    assert!(socket.exists());
    let mut client = harness.client();
    assert_eq!(client.ok(Call::Shutdown), Value::Null);
    assert!(harness.daemon.wait_timeout(Duration::from_secs(2)));
    harness.daemon.shutdown();
    assert_eq!(
        harness
            .engine
            .shutdowns
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(!socket.exists(), "socket file should be removed");
}

#[test]
fn second_daemon_is_refused_and_stale_sockets_are_replaced() {
    let dir = TempDir::new("second-daemon");
    let socket = dir.join("control.sock");
    let config = EngineConfig {
        device: "hw:0,0".into(),
        ..EngineConfig::default()
    };

    let engine_a: Arc<dyn Engine> = MockEngine::new(config.clone());
    let opts_a = quiet_options(&socket);
    let daemon_a = Daemon::start(engine_a, config.clone(), opts_a).expect("first daemon");

    let engine_b: Arc<dyn Engine> = MockEngine::new(config.clone());
    let opts_b = quiet_options(&socket);
    let err = match Daemon::start(engine_b, config.clone(), opts_b) {
        Ok(_) => panic!("a second daemon must not start"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("already listening"), "{err}");
    daemon_a.shutdown();

    // A socket file with no listener behind it is stale and gets replaced.
    let stale = dir.join("stale.sock");
    {
        let listener = std::os::unix::net::UnixListener::bind(&stale).unwrap();
        drop(listener);
    }
    assert!(stale.exists());
    let engine_c: Arc<dyn Engine> = MockEngine::new(config.clone());
    let opts_c = quiet_options(&stale);
    let daemon_c = Daemon::start(engine_c, config, opts_c).expect("stale socket replaced");
    let mut client = TestClient::connect(&stale);
    client.ok(Call::Status);
    daemon_c.shutdown();
    assert!(!stale.exists());
}

#[test]
fn socket_permissions_are_restricted() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::start("perms");
    let mode = std::fs::metadata(&harness.socket)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {:o}", mode & 0o777);
}

#[test]
fn passthrough_status_combines_engine_and_graph_facts() {
    // No graph watcher here, so the engine snapshot alone drives the status;
    // check that a non-bit-perfect engine state is reported faithfully.
    let harness = Harness::start("passthrough");
    harness.engine.set_state(EngineState::Playing);
    harness.engine.set_snapshot(|snapshot| {
        snapshot.dsp_active = true;
        snapshot.ratio = 2;
        snapshot.format = Some(SampleFormat::S16);
    });
    let mut client = harness.client();
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert!(!status.passthrough.intact);
    assert!(
        status
            .passthrough
            .broken_because
            .contains(&PassthroughBreak::DspActive)
    );
    assert!(
        status
            .passthrough
            .broken_because
            .contains(&PassthroughBreak::Upsampling { ratio: 2 })
    );
    assert!(
        status
            .passthrough
            .broken_because
            .contains(&PassthroughBreak::NotPlaying)
    );
}

#[test]
fn changes_survive_a_restart() {
    let harness = Harness::start("restart");
    let config_path = harness.config_path.clone();
    let mut client = harness.client();
    client.ok(Call::SetEq(Some(profile())));
    client.ok(Call::SetDspEnabled { enabled: false });
    harness.daemon.shutdown();
    drop(client);
    // The next daemon (main.rs does this) loads the persisted file.
    let store = audivane_dsp_daemon::control::config::ConfigStore::new(config_path);
    let loaded = store.load();
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    assert!(!loaded.config.dsp_enabled);
    assert_eq!(loaded.config.chain.eq.unwrap().bands.len(), 1);
}

#[test]
fn config_file_is_written_atomically() {
    let harness = Harness::start("atomic");
    let mut client = harness.client();
    client.ok(Call::SetEq(Some(profile())));
    // Writes are coalesced: wait for the file, then check that every directory
    // entry is either the config or a leftover temp file (the latter must be
    // gone once the write settled).
    harness.wait_for_config(Duration::from_secs(3), |cfg| cfg.chain.eq.is_some());
    let entries: Vec<String> = std::fs::read_dir(harness.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().all(|name| !name.contains(".tmp.")),
        "temp files left behind: {entries:?}"
    );
    assert!(
        Path::new(&harness.config_path).exists(),
        "config file missing"
    );
}

#[test]
fn upsample_off_after_ab_set_keeps_chain_b_in_step() {
    // The phase-3 repro: `ab set` while upsampling x4, then `ctl upsample
    // --off` used to fail because chain B was still x4.
    let harness = Harness::start("ab-repro");
    let mut client = harness.client();

    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Ratio(4),
        taps: 8192,
        ..UpsamplerConfig::default()
    }));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    // `avdsp ctl ab set` builds chain B from chain A.
    let chain_b = ChainConfig {
        eq: Some(profile()),
        ..config.chain.clone()
    };
    let value = client.ok(Call::SetAb(Some(AbConfig {
        chain_b,
        level: LevelMatch::Analytic,
    })));
    assert!(
        value.get("notes").is_none(),
        "an aligned chain B needs no note: {value}"
    );

    // This is the sequence that used to answer INVALID_PARAMS.
    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Off,
        ..UpsamplerConfig::default()
    }));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.chain.upsampler.target, RateTarget::Off);
    let chain_b = config.ab.expect("A/B still configured").chain_b;
    assert_eq!(chain_b.upsampler, config.chain.upsampler);
    assert_eq!(chain_b.output, config.chain.output);
    // The engine saw the same thing (not just the stored config).
    let engine = harness.engine.config_now();
    assert_eq!(engine.chain.upsampler.target, RateTarget::Off);
    assert_eq!(
        engine
            .ab
            .expect("A/B in the engine")
            .chain_b
            .upsampler
            .target,
        RateTarget::Off
    );

    // Other edits keep B in step too, and re-enabling x2 works.
    client.ok(Call::SetDspEnabled { enabled: false });
    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Ratio(2),
        taps: 4096,
        ..UpsamplerConfig::default()
    }));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    let chain_b = config.ab.unwrap().chain_b;
    assert_eq!(chain_b.upsampler, config.chain.upsampler);
    assert_eq!(chain_b.output, config.chain.output);
}

#[test]
fn set_ab_realigns_a_stale_chain_b_and_says_so() {
    let harness = Harness::start("ab-realign");
    let mut client = harness.client();
    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Ratio(4),
        taps: 8192,
        ..UpsamplerConfig::default()
    }));

    // A client that sends a chain B from before the upsampler changed.
    let stale_b = ChainConfig {
        eq: Some(profile()),
        upsampler: UpsamplerConfig {
            target: RateTarget::Ratio(2),
            ..UpsamplerConfig::default()
        },
        output: OutputConfig {
            bits: 16,
            dither: DitherKind::Shaped,
        },
        ..Default::default()
    };
    let value = client.ok(Call::SetAb(Some(AbConfig {
        chain_b: stale_b,
        level: LevelMatch::Manual { offset_db: -3.0 },
    })));
    let notes = value["notes"].as_array().expect("a note");
    assert_eq!(notes.len(), 1, "{value}");
    let note = notes[0].as_str().unwrap();
    assert!(note.contains("ab.chain_b.upsampler"), "{note}");
    assert!(note.contains("ab.chain_b.output"), "{note}");
    assert!(note.contains("x4"), "{note}");

    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    let chain_b = &config.ab.as_ref().unwrap().chain_b;
    assert_eq!(chain_b.upsampler, config.chain.upsampler);
    assert_eq!(chain_b.output, config.chain.output);
    assert_eq!(chain_b.eq.as_ref().unwrap().bands.len(), 1);
    assert_eq!(
        harness.engine.config_now().ab.unwrap().chain_b.upsampler,
        config.chain.upsampler
    );
}

#[test]
fn set_config_realigns_chain_b_before_validation() {
    let harness = Harness::start("set-config-ab");
    let mut client = harness.client();
    let mut config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    config.chain.upsampler.target = RateTarget::Ratio(8);
    // Chain B left at "off": invalid as written, normalized on the way in.
    config.ab = Some(AbConfig {
        chain_b: ChainConfig::default(),
        level: LevelMatch::Analytic,
    });
    client.ok(Call::SetConfig(config));
    let stored: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(
        stored.ab.as_ref().unwrap().chain_b.upsampler.target,
        RateTarget::Ratio(8)
    );
    assert_eq!(
        stored.ab.as_ref().unwrap().chain_b.upsampler,
        stored.chain.upsampler
    );
}

// ---------------------------------------------------------------- presets

#[test]
fn presets_round_trip_and_track_the_active_one() {
    let harness = Harness::start("presets");
    let mut client = harness.client();
    client.ok(Call::SetEq(Some(profile())));
    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Ratio(4),
        taps: 8192,
        ..UpsamplerConfig::default()
    }));

    // Save chain A; the name is trimmed and normalized.
    let info: PresetInfo = serde_json::from_value(client.ok(Call::SavePreset {
        name: "  Bass  ".into(),
        overwrite: false,
    }))
    .unwrap();
    assert_eq!(info.name, "Bass");
    assert_eq!(info.bands, 1);
    assert_eq!(info.convolution, 0);
    assert!(info.modified_unix > 0);

    // The saved file is JSON in `presets/` next to config.json.
    let preset_path = harness.dir.join("presets").join("bass.json");
    let text = std::fs::read_to_string(&preset_path).expect("preset file");
    let stored: Preset = serde_json::from_str(&text).unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.name, "Bass");
    assert_eq!(stored.chain.upsampler.target, RateTarget::Ratio(4));

    // It is the active preset, unmodified.
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(
        status.preset,
        Some(PresetState {
            name: "Bass".into(),
            modified: false,
        })
    );
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.active_preset.as_deref(), Some("Bass"));
    // ... and `active_preset` is persisted (once the write coalesces).
    harness.wait_for_config(Duration::from_secs(3), |cfg| {
        cfg.active_preset.as_deref() == Some("Bass")
    });

    // List holds it, get_preset returns the saved chain.
    let list: Vec<PresetInfo> = serde_json::from_value(client.ok(Call::ListPresets)).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "Bass");
    let preset: Preset = serde_json::from_value(client.ok(Call::GetPreset {
        name: "Bass".into(),
    }))
    .unwrap();
    assert_eq!(preset.chain, config.chain);
    assert_eq!(preset.chain.eq.as_ref().unwrap().bands.len(), 1);

    // Saving again without overwrite is refused (and leaves the file alone).
    match client.call(Call::SavePreset {
        name: "Bass".into(),
        overwrite: false,
    }) {
        Reply::Error(error) => {
            assert_eq!(error.code, RpcError::BAD_STATE);
            assert!(
                error.message.contains("already exists"),
                "{}",
                error.message
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    let again: Preset = serde_json::from_value(client.ok(Call::GetPreset {
        name: "Bass".into(),
    }))
    .unwrap();
    assert_eq!(again.chain, preset.chain);

    // Editing chain A marks the active preset modified.
    client.ok(Call::SetEq(None));
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert!(status.preset.as_ref().unwrap().modified);

    // Loading restores the saved chain and clears the flag.
    assert_eq!(
        client.ok(Call::LoadPreset {
            name: "Bass".into()
        }),
        Value::Null
    );
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert!(!status.preset.as_ref().unwrap().modified);
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.chain.eq.as_ref().unwrap().bands.len(), 1);
    assert_eq!(harness.engine.config_now().chain.eq.unwrap().bands.len(), 1);

    // Overwrite replaces the chain and keeps the preset active.
    client.ok(Call::SetEq(Some(profile())));
    client.ok(Call::SetDspEnabled { enabled: false });
    let info: PresetInfo = serde_json::from_value(client.ok(Call::SavePreset {
        name: "Bass".into(),
        overwrite: true,
    }))
    .unwrap();
    assert_eq!(info.name, "Bass");
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert!(!status.preset.unwrap().modified);

    // Loading also goes through the normal edit path: chain B follows A.
    let chain_b = ChainConfig {
        eq: Some(profile()),
        ..harness.engine.config_now().chain.clone()
    };
    client.ok(Call::SetAb(Some(AbConfig {
        chain_b,
        level: LevelMatch::Analytic,
    })));
    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Off,
        ..UpsamplerConfig::default()
    }));
    client.ok(Call::LoadPreset {
        name: "Bass".into(),
    });
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.chain.upsampler.target, RateTarget::Ratio(4));
    let ab = config.ab.expect("A/B still configured");
    assert_eq!(ab.chain_b.upsampler, config.chain.upsampler);
    assert_eq!(ab.chain_b.output, config.chain.output);
    client.ok(Call::SetAb(None));

    // Renaming the active preset follows into `active_preset`.
    let info: PresetInfo = serde_json::from_value(client.ok(Call::RenamePreset {
        from: "Bass".into(),
        to: "Bass boost".into(),
    }))
    .unwrap();
    assert_eq!(info.name, "Bass boost");
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.preset.unwrap().name, "Bass boost");
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.active_preset.as_deref(), Some("Bass boost"));
    match client.call(Call::GetPreset {
        name: "Bass".into(),
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("the old name must be gone: {other:?}"),
    }

    // Deleting the active preset clears the marker.
    assert_eq!(
        client.ok(Call::DeletePreset {
            name: "Bass boost".into()
        }),
        Value::Null
    );
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert!(status.preset.is_none());
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.active_preset.is_none());
    let list: Vec<PresetInfo> = serde_json::from_value(client.ok(Call::ListPresets)).unwrap();
    assert!(list.is_empty());
    assert!(!preset_path.exists());

    // Unknown names are INVALID_PARAMS, not silent no-ops.
    match client.call(Call::LoadPreset {
        name: "nope".into(),
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("unexpected {other:?}"),
    }
    match client.call(Call::DeletePreset {
        name: "nope".into(),
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("unexpected {other:?}"),
    }
    match client.call(Call::SavePreset {
        name: "  ".into(),
        overwrite: false,
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn import_apo_creates_a_preset_without_loading_it() {
    let harness = Harness::start("preset-import");
    let mut client = harness.client();
    let ir = harness.dir.write("room.wav", "RIFF....WAVE");
    let mut cfg: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    cfg.chain.upsampler = UpsamplerConfig {
        target: RateTarget::Ratio(2),
        taps: 8192,
        ..UpsamplerConfig::default()
    };
    cfg.chain.output = OutputConfig {
        bits: 16,
        dither: DitherKind::Shaped,
    };
    cfg.chain.convolution = vec![audivane_protocol::ConvolutionConfig {
        name: Some("Room".into()),
        enabled: true,
        ir_path: ir,
        gain_db: -1.0,
    }];
    cfg.chain.eq = None;
    client.ok(Call::SetConfig(cfg));

    let text = "Preamp: -6 dB\n\
                Filter 1: ON PK Fc 100 Hz Gain 3 dB Q 1.0\n\
                Filter 2: ON LS Fc 60 Hz Gain -2 dB Q 0.70\n";
    let info: PresetInfo = serde_json::from_value(client.ok(Call::ImportApo {
        name: "AutoEQ".into(),
        text: text.into(),
    }))
    .unwrap();
    assert_eq!(info.name, "AutoEQ");
    assert_eq!(info.bands, 2);
    assert_eq!(info.convolution, 0);

    let preset: Preset = serde_json::from_value(client.ok(Call::GetPreset {
        name: "AutoEQ".into(),
    }))
    .unwrap();
    let eq = preset.chain.eq.expect("EQ from the text");
    assert_eq!(eq.bands.len(), 2);
    assert!((eq.preamp_db + 6.0).abs() < 1e-9);
    // Everything outside EQ starts from flat defaults, regardless of chain A.
    let current: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(preset.chain.upsampler, ChainConfig::default().upsampler);
    assert_eq!(preset.chain.output, ChainConfig::default().output);
    assert_ne!(preset.chain.upsampler, current.chain.upsampler);
    assert_ne!(preset.chain.output, current.chain.output);
    assert!(preset.chain.convolution.is_empty());

    // Importing does not load the preset.
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert!(status.preset.is_none());
    assert!(current.chain.eq.is_none());

    // Bad text is a parameter error and creates nothing.
    match client.call(Call::ImportApo {
        name: "Broken".into(),
        text: "Filter 1: ON PK\n".into(),
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("unexpected {other:?}"),
    }
    let list: Vec<PresetInfo> = serde_json::from_value(client.ok(Call::ListPresets)).unwrap();
    assert_eq!(list.len(), 1);
}

#[test]
fn import_apo_over_active_preset_marks_status_modified() {
    let harness = Harness::start("preset-import-active");
    let mut client = harness.client();

    client.ok(Call::SetEq(Some(profile())));
    client.ok(Call::SavePreset {
        name: "Live".into(),
        overwrite: false,
    });
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(
        status.preset,
        Some(PresetState {
            name: "Live".into(),
            modified: false,
        })
    );

    // Importing over the active preset updates the file & cached saved chain
    // without touching the live processing chain, so `modified` flips to true.
    let text = "Preamp: -6 dB\n\
                Filter 1: ON PK Fc 250 Hz Gain -4 dB Q 1.2\n";
    let info: PresetInfo = serde_json::from_value(client.ok(Call::ImportApo {
        name: "Live".into(),
        text: text.into(),
    }))
    .unwrap();
    assert_eq!(info.name, "Live");

    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(
        status.preset,
        Some(PresetState {
            name: "Live".into(),
            modified: true,
        })
    );

    // Live processing chain is untouched.
    let cfg: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(cfg.chain.eq.as_ref().unwrap().bands[0].freq_hz, 1000.0);
    assert_eq!(
        harness.engine.config_now().chain.eq.unwrap().bands[0].freq_hz,
        1000.0
    );

    // Stored preset holds the imported EQ; loading it clears `modified`.
    let preset: Preset = serde_json::from_value(client.ok(Call::GetPreset {
        name: "Live".into(),
    }))
    .unwrap();
    assert_eq!(preset.chain.eq.as_ref().unwrap().bands[0].freq_hz, 250.0);
    client.ok(Call::LoadPreset {
        name: "Live".into(),
    });
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(
        status.preset,
        Some(PresetState {
            name: "Live".into(),
            modified: false,
        })
    );
    let cfg: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(cfg.chain.eq.as_ref().unwrap().bands[0].freq_hz, 250.0);
}

#[test]
fn config_and_preset_events_reach_subscribers() {
    let harness = Harness::start("preset-events");
    let mut watcher = harness.client();
    watcher.subscribe(vec![Topic::Presets]);
    // Initial snapshot, like the other topics.
    match watcher
        .next_event("presets", Duration::from_secs(2))
        .expect("presets snapshot")
    {
        audivane_protocol::ipc::Event::Presets(presets) => assert!(presets.is_empty()),
        other => panic!("unexpected {other:?}"),
    }

    // Another client saves a preset: the new list is pushed.
    let mut other = harness.client();
    other.ok(Call::SetEq(Some(profile())));
    other.ok(Call::SavePreset {
        name: "Live".into(),
        overwrite: false,
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for saved preset event"
        );
        match watcher
            .next_event("presets", remaining)
            .expect("presets event")
        {
            audivane_protocol::ipc::Event::Presets(presets) if presets.len() == 1 => {
                assert_eq!(presets[0].name, "Live");
                break;
            }
            audivane_protocol::ipc::Event::Presets(presets) if presets.is_empty() => {
                // Tolerate a duplicate initial snapshot from the event pump's first tick.
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // A config subscriber sees the active preset in its initial snapshot ...
    let mut configs = harness.client();
    configs.subscribe(vec![Topic::Config]);
    match configs
        .next_event("config", Duration::from_secs(2))
        .expect("config snapshot")
    {
        audivane_protocol::ipc::Event::Config(config) => {
            assert_eq!(config.device, "hw:0,0");
            assert_eq!(config.active_preset.as_deref(), Some("Live"));
        }
        other => panic!("unexpected {other:?}"),
    }

    // ... and config changes from other clients are pushed.
    let mut third = harness.client();
    third.ok(Call::SetDspEnabled { enabled: false });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for config update event"
        );
        match configs
            .next_event("config", remaining)
            .expect("config event")
        {
            audivane_protocol::ipc::Event::Config(config) if !config.dsp_enabled => break,
            audivane_protocol::ipc::Event::Config(_) => {
                // Tolerate a duplicate initial snapshot from the event pump's first tick.
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // Deleting pushes the new (empty) list.
    third.ok(Call::DeletePreset {
        name: "Live".into(),
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out waiting for delete preset event"
        );
        match watcher
            .next_event("presets", remaining)
            .expect("presets event after delete")
        {
            audivane_protocol::ipc::Event::Presets(presets) if presets.is_empty() => break,
            audivane_protocol::ipc::Event::Presets(presets) if presets.len() == 1 => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

// ---------------------------------------------------------------- devices

#[test]
fn devices_are_listed_from_proc_asound() {
    let harness = Harness::start("devices");
    let mut client = harness.client();
    let devices: Vec<DeviceInfo> = serde_json::from_value(client.ok(Call::ListDevices)).unwrap();
    for device in &devices {
        assert!(
            device.id.starts_with("hw:CARD=") && device.id.contains(",DEV="),
            "{device:?}"
        );
        assert!(!device.card.is_empty());
        assert!(!device.name.is_empty());
    }
    assert!(
        devices.iter().filter(|device| device.active).count() <= 1,
        "at most one active device: {devices:?}"
    );

    // Setting the configured device to a listed entry marks exactly that one
    // active, in `hw:CARD=` form.
    if let Some(first) = devices.first() {
        let mut config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
        config.device = first.id.clone();
        client.ok(Call::SetConfig(config));
        let devices: Vec<DeviceInfo> =
            serde_json::from_value(client.ok(Call::ListDevices)).unwrap();
        let active: Vec<&DeviceInfo> = devices.iter().filter(|device| device.active).collect();
        assert_eq!(active.len(), 1, "{devices:?}");
        assert_eq!(active[0].id, first.id);
    }
}

// ---------------------------------------------------------------- persistence

#[test]
fn deferred_persistence_coalesces_and_flushes_on_shutdown() {
    let harness = Harness::start("deferred");
    let mut client = harness.client();
    let before = harness.daemon.service().persist_writes();

    // Live-dragging a band: many mutations, few writes.
    let start = Instant::now();
    for n in 0..60 {
        let mut eq = profile();
        eq.bands[0].gain_db = f64::from(n) * 0.1;
        client.ok(Call::SetEq(Some(eq)));
    }
    let elapsed = start.elapsed();
    let writes = harness.daemon.service().persist_writes() - before;
    // Coalescing is time-based (one write per ~300 ms), so scale the bound
    // with how long the burst actually took on a busy machine.
    let allowed = elapsed.as_millis() as u64 / 300 + 3;
    assert!(
        writes <= allowed && writes < 60,
        "{writes} config writes for 60 rapid edits ({elapsed:?})"
    );
    assert_eq!(
        harness.engine.config_now().chain.eq.unwrap().bands[0].gain_db,
        5.9
    );

    // The last edit still reaches disk well within the 2 s bound.
    harness.wait_for_config(Duration::from_secs(3), |cfg| {
        cfg.chain
            .eq
            .as_ref()
            .is_some_and(|eq| (eq.bands[0].gain_db - 5.9).abs() < 1e-9)
    });

    // A change queued just before shutdown is flushed by shutdown.
    let mut eq = profile();
    eq.bands[0].gain_db = 42.0;
    client.ok(Call::SetEq(Some(eq)));
    harness.daemon.shutdown();
    let loaded =
        audivane_dsp_daemon::control::config::ConfigStore::new(harness.config_path.clone()).load();
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    assert_eq!(
        loaded.config.chain.eq.unwrap().bands[0].gain_db,
        42.0,
        "shutdown must flush the last config change"
    );
}

#[test]
fn set_convolution_accepts_null_object_and_array_over_ipc() {
    let harness = Harness::start("conv-shapes");
    let mut client = harness.client();
    let ir1 = harness.dir.write("room.wav", "RIFF....WAVE");
    let ir2 = harness.dir.write("headphones.wav", "RIFF....WAVE");

    // 1. Single object (back-compat wire shape, no `name`).
    let req = serde_json::json!({
        "id": 100,
        "method": "set_convolution",
        "params": { "enabled": true, "ir_path": ir1, "gain_db": -1.5 }
    });
    client.send_raw(format!("{req}\n").as_bytes());
    let reply: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(reply["id"], 100);
    assert!(reply.get("error").is_none(), "unexpected error: {reply}");
    let cfg: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(cfg.chain.convolution.len(), 1);
    assert_eq!(cfg.chain.convolution[0].ir_path, ir1);
    assert_eq!(cfg.chain.convolution[0].name, None);

    // 2. Array of named/unnamed configs (including disabled entry).
    let req = serde_json::json!({
        "id": 101,
        "method": "set_convolution",
        "params": [
            { "name": "Room", "enabled": true, "ir_path": ir1, "gain_db": -1.0 },
            { "enabled": false, "ir_path": "/nonexistent/disabled.wav", "gain_db": 0.0 },
            { "name": "Cans", "enabled": true, "ir_path": ir2, "gain_db": 2.0 }
        ]
    });
    client.send_raw(format!("{req}\n").as_bytes());
    let reply: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(reply["id"], 101);
    assert!(reply.get("error").is_none(), "unexpected error: {reply}");
    let cfg: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(cfg.chain.convolution.len(), 3);
    assert_eq!(cfg.chain.convolution[0].name.as_deref(), Some("Room"));
    assert!(!cfg.chain.convolution[1].enabled);
    assert_eq!(cfg.chain.convolution[2].ir_path, ir2);

    // 3. Enabled missing second entry fails validation and preserves previous config.
    let req = serde_json::json!({
        "id": 102,
        "method": "set_convolution",
        "params": [
            { "enabled": true, "ir_path": ir1, "gain_db": 0.0 },
            { "enabled": true, "ir_path": "/nonexistent/missing.wav", "gain_db": 0.0 }
        ]
    });
    client.send_raw(format!("{req}\n").as_bytes());
    let reply: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(reply["error"]["code"], RpcError::INVALID_PARAMS);
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("convolution[1].ir_path"),
        "reply: {reply}"
    );
    let cfg: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(cfg.chain.convolution.len(), 3);

    // 4. Null clears convolution chain.
    let req = serde_json::json!({
        "id": 103,
        "method": "set_convolution",
        "params": null
    });
    client.send_raw(format!("{req}\n").as_bytes());
    let reply: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(reply["id"], 103);
    assert!(reply.get("error").is_none(), "unexpected error: {reply}");
    let cfg: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(cfg.chain.convolution.is_empty());
}

#[test]
fn set_eq_supports_layered_sections_and_validates_left_and_right_channels() {
    let harness = Harness::start("layered-eq");
    let mut client = harness.client();

    let mut layered = profile();
    layered.left = Some(audivane_protocol::ChannelEq {
        enabled: true,
        preamp_db: -1.0,
        bands: vec![Band {
            kind: FilterType::Peaking,
            freq_hz: 138.0,
            gain_db: -4.0,
            q: 3.0,
            enabled: true,
        }],
    });
    layered.right = Some(audivane_protocol::ChannelEq {
        enabled: false,
        preamp_db: -4.5,
        bands: vec![Band {
            kind: FilterType::HighShelf,
            freq_hz: 6000.0,
            gain_db: -2.5,
            q: 0.707,
            enabled: true,
        }],
    });
    client.ok(Call::SetEq(Some(layered)));

    let stored: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    let eq = stored.chain.eq.expect("layered eq stored");
    assert_eq!(eq.version, audivane_protocol::EQ_PROFILE_VERSION);
    assert!(eq.linked_enabled);
    let left = eq.left.expect("left channel stored");
    assert!(left.enabled);
    assert_eq!(left.bands[0].freq_hz, 138.0);
    let right = eq.right.expect("right channel stored");
    assert!(!right.enabled);
    assert_eq!(right.preamp_db, -4.5);
    assert_eq!(right.bands.len(), 1);
    assert_eq!(right.bands[0].freq_hz, 6000.0);

    // Invalid left-channel or right-channel band is rejected without changing config.
    let mut bad_left = profile();
    bad_left.left = Some(audivane_protocol::ChannelEq {
        enabled: true,
        preamp_db: 0.0,
        bands: vec![Band {
            kind: FilterType::Peaking,
            freq_hz: 0.0,
            gain_db: 0.0,
            q: 1.0,
            enabled: true,
        }],
    });
    match client.call(Call::SetEq(Some(bad_left))) {
        Reply::Error(err) => {
            assert_eq!(err.code, RpcError::INVALID_PARAMS);
            assert!(err.message.contains("left"), "{}", err.message);
        }
        other => panic!("unexpected {other:?}"),
    }

    let mut bad_right = profile();
    bad_right.right = Some(audivane_protocol::ChannelEq {
        enabled: true,
        preamp_db: 0.0,
        bands: vec![Band {
            kind: FilterType::Peaking,
            freq_hz: -100.0,
            gain_db: 0.0,
            q: 1.0,
            enabled: true,
        }],
    });
    match client.call(Call::SetEq(Some(bad_right))) {
        Reply::Error(err) => {
            assert_eq!(err.code, RpcError::INVALID_PARAMS);
            assert!(err.message.contains("right"), "{}", err.message);
        }
        other => panic!("unexpected {other:?}"),
    }

    // Disabling all sections with 0 dB global preamp yields passthrough.
    let mut all_off = profile();
    all_off.preamp_db = 0.0;
    all_off.linked_enabled = false;
    all_off.left = Some(audivane_protocol::ChannelEq {
        enabled: false,
        preamp_db: -3.0,
        bands: all_off.bands.clone(),
    });
    client.ok(Call::SetEq(Some(all_off)));
    let after: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(after.chain.is_passthrough());
}

#[test]
fn set_volume_validates_ranges_surfaces_hardware_and_persists() {
    use audivane_protocol::ipc::VolumeStatus;

    let harness = Harness::start("volume-ipc");
    let config_path = harness.config_path.clone();
    let mut client = harness.client();

    for (volume_db, balance_db, needle) in [
        (0.5, 0.0, "volume"),
        (-120.5, 0.0, "volume"),
        (0.0, 24.5, "balance"),
        (0.0, -24.5, "balance"),
    ] {
        match client.call(Call::SetVolume {
            volume_db,
            balance_db,
            muted: false,
        }) {
            Reply::Error(err) => {
                assert_eq!(err.code, RpcError::INVALID_PARAMS);
                assert!(err.message.contains(needle), "{}", err.message);
            }
            other => panic!("expected INVALID_PARAMS, got {other:?}"),
        }
    }

    // Valid edit applies and is persisted across restart.
    client.ok(Call::SetVolume {
        volume_db: -18.0,
        balance_db: -4.5,
        muted: true,
    });

    // When the engine reports a hardware mixer snapshot, Status + SetVolume forward it.
    let hw = VolumeStatus {
        volume_db: -18.0,
        balance_db: -4.5,
        muted: true,
        hardware: true,
        min_db: -60.0,
        max_db: 0.0,
    };
    harness.engine.set_snapshot(|s| s.volume = Some(hw));
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.volume, Some(hw));

    harness.daemon.shutdown();
    drop(client);

    let loaded = audivane_dsp_daemon::control::config::ConfigStore::new(config_path).load();
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    assert_eq!(loaded.config.volume.volume_db, -18.0);
    assert_eq!(loaded.config.volume.balance_db, -4.5);
    assert!(loaded.config.volume.muted);
}

#[test]
fn concurrent_mutations_persist_final_state_and_apply_in_order() {
    let harness = Harness::start("concurrent-mutations");
    let config_path = harness.config_path.clone();
    let threads = 6usize;
    let iters = 25usize;
    let barrier = Arc::new(std::sync::Barrier::new(threads));

    std::thread::scope(|scope| {
        for tid in 0..threads {
            let barrier = Arc::clone(&barrier);
            let socket = &harness.socket;
            scope.spawn(move || {
                let mut client = TestClient::connect(socket);
                barrier.wait();
                for iter in 0..iters {
                    let tag = -50.0 + ((tid * iters + iter) as f64) * 0.5;
                    if iter % 3 == 0 {
                        let mut cfg: EngineConfig =
                            serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
                        let mut eq = profile();
                        eq.preamp_db = -((iter % 10) as f64 + 1.0);
                        eq.bands[0].gain_db = tag;
                        cfg.chain.eq = Some(eq);
                        cfg.dsp_enabled = iter % 2 == 0;
                        client.ok(Call::SetConfig(cfg));
                    } else {
                        let mut eq = profile();
                        eq.bands[0].gain_db = tag;
                        client.ok(Call::SetEq(Some(eq)));
                    }
                }
            });
        }
    });

    let final_service_cfg = harness.daemon.service().config();
    let final_engine_cfg = harness.engine.config_now();
    assert_eq!(
        final_service_cfg, final_engine_cfg,
        "engine and service must agree on the final committed config"
    );

    harness.daemon.shutdown();
    let loaded = audivane_dsp_daemon::control::config::ConfigStore::new(config_path).load();
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    assert_eq!(
        loaded.config, final_service_cfg,
        "persisted config.json must match the final committed in-memory state"
    );
}

#[test]
fn concurrent_save_preset_serializes_collisions_and_same_slug_names() {
    let harness = Harness::start("concurrent-presets");
    {
        let mut client = harness.client();
        client.ok(Call::SetEq(Some(profile())));
    }

    // 1. Same name with overwrite: false -> exactly one succeeds, others BAD_STATE.
    let same_threads = 8usize;
    let barrier = Arc::new(std::sync::Barrier::new(same_threads));
    let mut outcomes = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..same_threads {
            let barrier = Arc::clone(&barrier);
            let socket = &harness.socket;
            handles.push(scope.spawn(move || {
                let mut client = TestClient::connect(socket);
                barrier.wait();
                client.call(Call::SavePreset {
                    name: "Live".into(),
                    overwrite: false,
                })
            }));
        }
        for handle in handles {
            outcomes.push(handle.join().expect("join same-name thread"));
        }
    });
    let oks = outcomes
        .iter()
        .filter(|reply| matches!(reply, Reply::Ok(_)))
        .count();
    let bad_states = outcomes
        .iter()
        .filter(|reply| matches!(reply, Reply::Error(err) if err.code == RpcError::BAD_STATE))
        .count();
    assert_eq!(oks, 1, "expected exactly 1 success: {outcomes:?}");
    assert_eq!(
        bad_states,
        same_threads - 1,
        "expected {same_threads} - 1 BAD_STATE errors: {outcomes:?}"
    );

    // 2. Distinct names that produce the same slug ("rock-pop") -> all persist
    //    into distinct files without overwriting one another.
    let names = ["Rock Pop", "Rock-Pop", "Rock/Pop", "Rock_Pop"];
    let slug_barrier = Arc::new(std::sync::Barrier::new(names.len()));
    let mut slug_replies = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for name in names {
            let barrier = Arc::clone(&slug_barrier);
            let socket = &harness.socket;
            handles.push(scope.spawn(move || {
                let mut client = TestClient::connect(socket);
                barrier.wait();
                client.ok(Call::SavePreset {
                    name: name.into(),
                    overwrite: false,
                })
            }));
        }
        for handle in handles {
            slug_replies.push(handle.join().expect("join slug thread"));
        }
    });
    assert_eq!(slug_replies.len(), names.len());

    let mut client = harness.client();
    let list: Vec<PresetInfo> = serde_json::from_value(client.ok(Call::ListPresets)).unwrap();
    let listed_names: std::collections::BTreeSet<String> =
        list.into_iter().map(|info| info.name).collect();
    for expected in std::iter::once("Live").chain(names) {
        assert!(
            listed_names.contains(expected),
            "missing preset {expected:?} in {listed_names:?}"
        );
        let preset: Preset = serde_json::from_value(client.ok(Call::GetPreset {
            name: expected.into(),
        }))
        .unwrap();
        assert_eq!(preset.name, expected);
    }
    assert_eq!(listed_names.len(), 1 + names.len());
}

#[test]
fn shutdown_drains_inflight_mutations_before_flushing() {
    use std::sync::{Condvar, Mutex};

    // Part A: `daemon.shutdown()` (SIGTERM/SIGINT path) waits for an in-flight
    // mutation before flushing `config.json`, while rejecting new dispatches.
    {
        let harness = Harness::start("shutdown-drain-direct");
        let config_path = harness.config_path.clone();

        let state = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let hook_state = Arc::clone(&state);
        *harness.engine.on_apply.lock().unwrap() = Some(Arc::new(move |cfg| {
            let is_target = cfg
                .chain
                .eq
                .as_ref()
                .is_some_and(|eq| (eq.bands[0].gain_db - 77.0).abs() < 1e-9);
            if !is_target {
                return;
            }
            let (lock, cv) = &*hook_state;
            let mut guard = lock.lock().unwrap();
            guard.0 = true;
            cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !guard.1 && Instant::now() < deadline {
                let wait = deadline.saturating_duration_since(Instant::now());
                let (next, _) = cv.wait_timeout(guard, wait).unwrap();
                guard = next;
            }
        }));

        std::thread::scope(|scope| {
            let inflight = scope.spawn(|| {
                let mut client = TestClient::connect(&harness.socket);
                let mut eq = profile();
                eq.bands[0].gain_db = 77.0;
                client.ok(Call::SetEq(Some(eq)));
            });

            // Wait until the engine is mid-apply for the in-flight mutation.
            {
                let (lock, cv) = &*state;
                let mut guard = lock.lock().unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                while !guard.0 {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    assert!(!wait.is_zero(), "timed out waiting for on_apply");
                    let (next, _) = cv.wait_timeout(guard, wait).unwrap();
                    guard = next;
                }
            }

            let stopper = scope.spawn(|| {
                harness.daemon.shutdown();
            });

            // Give shutdown a moment to enter `begin_shutdown` and block on draining.
            std::thread::sleep(Duration::from_millis(50));
            match harness
                .daemon
                .service()
                .dispatch(Call::SetDspEnabled { enabled: false })
            {
                Err(err) => assert_eq!(err.code, RpcError::BAD_STATE),
                Ok(_) => panic!("expected dispatch rejection once shutdown started"),
            }
            // Release the blocked `apply`; shutdown can now drain, flush, and finish.
            {
                let (lock, cv) = &*state;
                let mut guard = lock.lock().unwrap();
                guard.1 = true;
                cv.notify_all();
            }

            inflight.join().expect("in-flight SetEq finished");
            stopper.join().expect("shutdown finished");
        });

        let loaded = audivane_dsp_daemon::control::config::ConfigStore::new(config_path).load();
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert_eq!(loaded.config.chain.eq.unwrap().bands[0].gain_db, 77.0);
    }

    // Part B: IPC `Call::Shutdown` follows the same drain -> flush -> join path.
    {
        let harness = Harness::start("shutdown-drain-ipc");
        let config_path = harness.config_path.clone();

        let state = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let hook_state = Arc::clone(&state);
        *harness.engine.on_apply.lock().unwrap() = Some(Arc::new(move |cfg| {
            let is_target = cfg
                .chain
                .eq
                .as_ref()
                .is_some_and(|eq| (eq.bands[0].gain_db - 78.0).abs() < 1e-9);
            if !is_target {
                return;
            }
            let (lock, cv) = &*hook_state;
            let mut guard = lock.lock().unwrap();
            guard.0 = true;
            cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !guard.1 && Instant::now() < deadline {
                let wait = deadline.saturating_duration_since(Instant::now());
                let (next, _) = cv.wait_timeout(guard, wait).unwrap();
                guard = next;
            }
        }));

        std::thread::scope(|scope| {
            let inflight = scope.spawn(|| {
                let mut client = TestClient::connect(&harness.socket);
                let mut eq = profile();
                eq.bands[0].gain_db = 78.0;
                client.ok(Call::SetEq(Some(eq)));
            });

            {
                let (lock, cv) = &*state;
                let mut guard = lock.lock().unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                while !guard.0 {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    assert!(!wait.is_zero(), "timed out waiting for on_apply");
                    let (next, _) = cv.wait_timeout(guard, wait).unwrap();
                    guard = next;
                }
            }

            let mut shutdown_client = harness.client();
            assert_eq!(shutdown_client.ok(Call::Shutdown), Value::Null);
            assert!(harness.daemon.wait_timeout(Duration::from_secs(2)));

            let stopper = scope.spawn(|| {
                harness.daemon.shutdown();
            });
            std::thread::sleep(Duration::from_millis(50));
            {
                let (lock, cv) = &*state;
                let mut guard = lock.lock().unwrap();
                guard.1 = true;
                cv.notify_all();
            }

            inflight.join().expect("in-flight SetEq finished");
            stopper.join().expect("shutdown finished");
        });

        let loaded = audivane_dsp_daemon::control::config::ConfigStore::new(config_path).load();
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert_eq!(loaded.config.chain.eq.unwrap().bands[0].gain_db, 78.0);
    }
}

fn mock_spectrum(rate: u32, level: f32) -> Spectrum {
    Spectrum {
        f_min_hz: 20.0,
        bins_per_octave: 24,
        pre: vec![-80.0, level],
        post: vec![-70.0, level],
        rate,
    }
}

#[test]
fn spectrum_subscription_yields_events() {
    let harness = Harness::start("spectrum");
    harness
        .engine
        .set_spectrum(Some(mock_spectrum(48_000, -12.0)));

    let mut client = harness.client();
    client.subscribe(vec![Topic::Spectrum]);

    // A new subscriber is seeded with the latest frame.
    let first = client
        .next_event("spectrum", Duration::from_secs(2))
        .expect("seeded spectrum event");
    match first {
        audivane_protocol::ipc::Event::Spectrum(spectrum) => {
            assert_eq!(spectrum.rate, 48_000);
            assert_eq!(spectrum.post, vec![-70.0, -12.0]);
        }
        other => panic!("unexpected event {other:?}"),
    }

    // Later frames arrive through the event pump without another request.
    harness
        .engine
        .set_spectrum(Some(mock_spectrum(44_100, -20.0)));
    let second = client
        .next_event("spectrum", Duration::from_secs(2))
        .expect("live spectrum event");
    match second {
        audivane_protocol::ipc::Event::Spectrum(spectrum) => {
            assert_eq!(spectrum.rate, 44_100);
            assert_eq!(spectrum.post, vec![-70.0, -20.0]);
        }
        other => panic!("unexpected event {other:?}"),
    }
}
