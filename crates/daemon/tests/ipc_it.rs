//! End-to-end tests of the IPC server + service with a mock engine.
//!
//! Each test runs the real [`Daemon`] (temp socket, temp config) and speaks the
//! wire protocol; nothing here touches ALSA, PipeWire or D-Bus.

mod common;

use audivane_dsp_daemon::api::{Engine, EngineError};
use audivane_dsp_daemon::control::daemon::{Daemon, DaemonOptions};
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{
    AbConfig, AbSide, AbStatus, AppRule, Call, EngineConfig, EngineState, LevelMatch,
    PassthroughBreak, Preset, PresetInfo, PresetState, RouteTarget, RoutingMode, RpcError,
    SinkChoice, Status, StreamInfo, Topic, VolumeStatus,
};
use audivane_protocol::{Band, ChainConfig, EqProfile, FilterType, RateTarget, UpsamplerConfig};
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
        device_identity: false,
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

    // set_dc_block
    client.ok(Call::SetDcBlock(Some(
        audivane_protocol::DcBlockConfig::default(),
    )));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.chain.dc_block.as_ref().is_some_and(|d| d.enabled));
    let common::Reply::Error(err) =
        client.call(Call::SetDcBlock(Some(audivane_protocol::DcBlockConfig {
            enabled: true,
            cutoff_hz: 25.0,
        })))
    else {
        panic!("expected INVALID_PARAMS for out-of-range dc_block cutoff");
    };
    assert_eq!(err.code, RpcError::INVALID_PARAMS);
    client.ok(Call::SetDcBlock(None));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.chain.dc_block.is_none());

    // set_crossfeed
    client.ok(Call::SetCrossfeed(Some(
        audivane_protocol::CrossfeedConfig::default(),
    )));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.chain.crossfeed.as_ref().is_some_and(|c| c.enabled));
    client.ok(Call::SetCrossfeed(None));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.chain.crossfeed.is_none());

    // set_harmonics
    client.ok(Call::SetHarmonics(Some(
        audivane_protocol::HarmonicsConfig::from_preset("triode").unwrap(),
    )));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.chain.harmonics.as_ref().is_some_and(|h| h.enabled));
    // Readouts of the active stage, and of a config passed in.
    let readout: audivane_protocol::ipc::HarmonicsReadout =
        serde_json::from_value(client.ok(Call::HarmonicsReadout {
            config: None,
            sweep: false,
            sweep_freq_hz: None,
            sweep_level_dbfs: None,
        }))
        .unwrap();
    assert_eq!(readout.points.len(), 4);
    assert!((readout.points[0].thd_db + 39.9).abs() < 0.2, "{readout:?}");
    // The spectrum of the active stage at 1 kHz, 0 dBFS: H2 where the
    // readout puts it, the output word's floor below it.
    let spectrum: audivane_protocol::ipc::HarmonicsSpectrum =
        serde_json::from_value(client.ok(Call::HarmonicsSpectrum {
            config: None,
            freq_hz: 1_000.0,
            level_dbfs: 0.0,
            traces: Vec::new(),
        }))
        .unwrap();
    assert_eq!(spectrum.lines.len(), 1);
    let line = &spectrum.lines[0];
    let h2 = line.peaks.iter().find(|p| p.order == 2).unwrap();
    assert!((h2.level_db - readout.points[0].harmonics_db[0]).abs() < 0.05);
    assert!(line.floor_db < h2.level_db);
    assert!(matches!(
        client.call(Call::HarmonicsSpectrum {
            config: None,
            freq_hz: 1_000.0,
            level_dbfs: 0.0,
            traces: vec![audivane_protocol::ipc::HarmonicsSpectrumTrace::Layer { layer: 9 }],
        }),
        common::Reply::Error(e) if e.code == RpcError::INVALID_PARAMS
    ));
    let presets: Vec<audivane_protocol::HarmonicsPreset> =
        serde_json::from_value(client.ok(Call::HarmonicsPresets)).unwrap();
    let tape = presets.iter().find(|p| p.id == "tape").unwrap().to_layer();
    let systems: Vec<audivane_protocol::HarmonicsSystem> =
        serde_json::from_value(client.ok(Call::HarmonicsSystems)).unwrap();
    assert_eq!(systems, audivane_protocol::harmonics_systems());
    let readout: audivane_protocol::ipc::HarmonicsReadout =
        serde_json::from_value(client.ok(Call::HarmonicsReadout {
            config: Some(audivane_protocol::HarmonicsConfig {
                system: None,
                enabled: true,
                layers: vec![tape],
                dc_block: true,
                routing: Default::default(),
            }),
            sweep: true,
            sweep_freq_hz: None,
            sweep_level_dbfs: None,
        }))
        .unwrap();
    assert!(readout.peak_gain_db > 0.0, "{readout:?}");
    assert_eq!(readout.sweep.len(), 42);
    assert!(readout.below_clean_rate.is_empty());
    assert!(readout.under_load.is_empty());
    // The sweeps at one tone and one level, as the app's sliders ask.
    let moved: audivane_protocol::ipc::HarmonicsReadout =
        serde_json::from_value(client.ok(Call::HarmonicsReadout {
            config: Some(audivane_protocol::HarmonicsConfig {
                system: None,
                enabled: true,
                layers: vec![presets.iter().find(|p| p.id == "tape").unwrap().to_layer()],
                dc_block: true,
                routing: Default::default(),
            }),
            sweep: true,
            sweep_freq_hz: Some(100.0),
            sweep_level_dbfs: Some(-6.0),
        }))
        .unwrap();
    assert_eq!(moved.sweep.len(), 21);
    assert!(moved.sweep.iter().all(|p| p.freq_hz == 100.0));
    assert!(!moved.freq_sweep.is_empty());
    assert!(moved.freq_sweep.iter().all(|p| p.input_dbfs == -6.0));
    assert_eq!(moved.points, readout.points);
    let studio = presets
        .iter()
        .find(|p| p.id == "tape_studio")
        .unwrap()
        .to_layer();
    let hyst_readout: audivane_protocol::ipc::HarmonicsReadout =
        serde_json::from_value(client.ok(Call::HarmonicsReadout {
            config: Some(audivane_protocol::HarmonicsConfig {
                system: None,
                enabled: true,
                layers: vec![studio],
                dc_block: true,
                routing: Default::default(),
            }),
            sweep: false,
            sweep_freq_hz: None,
            sweep_level_dbfs: None,
        }))
        .unwrap();
    assert!(hyst_readout.peak_gain_db > 0.5, "{hyst_readout:?}");
    assert_eq!(hyst_readout.below_clean_rate.len(), 1);
    assert_eq!(hyst_readout.below_clean_rate[0].layer, 0);
    // A layer with sag: the under-load sweep next to the static one.
    let mut sagging = presets
        .iter()
        .find(|p| p.id == "se_triode_high_mu")
        .unwrap()
        .to_layer();
    if let audivane_protocol::HarmonicsLayer::Chebyshev(l) = &mut sagging {
        l.sag = Some(audivane_protocol::HarmonicsSag::default());
    }
    let sag_readout: audivane_protocol::ipc::HarmonicsReadout =
        serde_json::from_value(client.ok(Call::HarmonicsReadout {
            config: Some(audivane_protocol::HarmonicsConfig {
                system: None,
                enabled: true,
                layers: vec![sagging],
                dc_block: true,
                routing: Default::default(),
            }),
            sweep: true,
            sweep_freq_hz: None,
            sweep_level_dbfs: None,
        }))
        .unwrap();
    assert_eq!(sag_readout.under_load.len(), sag_readout.sweep.len());
    assert!(sag_readout.bias_limited.is_empty());
    // Series: a curve into a tape layer, set, read back and read out.
    let curve = presets
        .iter()
        .find(|p| p.id == "se_triode_high_mu")
        .unwrap()
        .to_layer();
    let chained = audivane_protocol::HarmonicsConfig {
        system: None,
        enabled: true,
        layers: vec![curve, studio_layer(&presets)],
        dc_block: true,
        routing: audivane_protocol::HarmonicsRouting::Series,
    };
    client.ok(Call::SetHarmonics(Some(chained.clone())));
    let raw = client.ok(Call::GetConfig);
    assert_eq!(raw["chain"]["harmonics"]["routing"], "series");
    let config: EngineConfig = serde_json::from_value(raw).unwrap();
    assert_eq!(config.chain.harmonics.as_ref(), Some(&chained));
    let series_readout: audivane_protocol::ipc::HarmonicsReadout =
        serde_json::from_value(client.ok(Call::HarmonicsReadout {
            config: None,
            sweep: false,
            sweep_freq_hz: None,
            sweep_level_dbfs: None,
        }))
        .unwrap();
    // The peak bounds compose: the curve's and the tape layer's.
    assert!(
        series_readout.peak_gain_db > hyst_readout.peak_gain_db + 0.1,
        "{series_readout:?}"
    );
    assert_eq!(series_readout.points.len(), 4);
    client.ok(Call::SetHarmonics(None));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert!(config.chain.harmonics.is_none());

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
            matrix: None,
            calibration: false,
        },
    ]));
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.chain.convolution.len(), 1);
    assert_eq!(config.chain.convolution[0].ir_path, ir);
    client.ok(Call::SetConvolution(Vec::new()));

    // set_upsampler
    client.ok(Call::SetUpsampler(
        audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap(),
    ));
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
        level: LevelMatch::Manual { offset_db: -3.0 },
        headroom: Default::default(),
        ..AbConfig::chain(chain_b)
    }))))
    .unwrap();
    assert!(ab.enabled);
    assert_eq!(ab.current, Some(AbSide::A));
    let ab: AbStatus = serde_json::from_value(client.ok(Call::AbSelect {
        side: AbSide::B,
        crossfade_ms: 25.0,
    }))
    .unwrap();
    assert_eq!(ab.current, Some(AbSide::B));
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

    // set_stream_volume: a level out of range is refused before anything
    // else; without a graph, BAD_STATE.
    for volume_db in [0.5, -121.0] {
        match client.call(Call::SetStreamVolume {
            node: 7,
            volume_db: Some(volume_db),
            muted: None,
        }) {
            Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
            other => panic!("unexpected {other:?}"),
        }
    }
    match client.call(Call::SetStreamVolume {
        node: 7,
        volume_db: Some(-6.0),
        muted: Some(false),
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

    // Every call is a known method (set_pitch once was not), and an unknown
    // enum value inside params is bad params, not an unknown method.
    client.send_raw(
        br#"{"id":8,"method":"set_pitch","params":{"enabled":true,"cents":0.0,"quality":"high"}}"#,
    );
    client.send_raw(b"\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["id"], 8);
    assert!(value.get("error").is_none(), "{value}");
    client.send_raw(
        br#"{"id":9,"method":"set_pitch","params":{"enabled":true,"cents":0.0,"quality":"ultra"}}"#,
    );
    client.send_raw(b"\n");
    let value: Value = serde_json::from_str(client.read_line().unwrap().trim()).unwrap();
    assert_eq!(value["error"]["code"], RpcError::INVALID_PARAMS, "{value}");

    // Validation errors name the field.
    match client.call(Call::SetUpsampler(
        audivane_dsp_core::resample::ManualConfig {
            taps: 0,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap(),
    )) {
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
            matrix: None,
            calibration: false,
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
        snapshot.ratio = audivane_protocol::RateRatio::integer(4);
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

fn volume_of(client: &mut TestClient) -> VolumeStatus {
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    status.volume.unwrap()
}

fn set_volume(
    client: &mut TestClient,
    volume_db: f64,
    balance_db: f64,
    muted: bool,
    if_instance: Option<&str>,
    if_revision: Option<u64>,
) -> Reply {
    match (if_instance, if_revision) {
        (Some(instance), Some(revision)) => client.call(Call::SetVolumeGuarded {
            volume_db: Some(volume_db),
            balance_db: Some(balance_db),
            muted: Some(muted),
            if_instance: instance.to_string(),
            if_revision: revision,
        }),
        (None, None) => client.call(Call::SetVolume {
            volume_db,
            balance_db,
            muted,
        }),
        _ => panic!("a guarded write names both the instance and the revision"),
    }
}

/// Compare-and-set on the level revision: a write based on a level that
/// changed since (another client, the CLI) is refused and changes nothing;
/// the echo carries the revision for the next step.
#[test]
fn a_volume_change_based_on_an_old_revision_is_refused() {
    let harness = Harness::start_with("revision", |c| c.volume.volume_db = -20.0);
    let mut app = harness.client();
    let mut cli = harness.client();
    let start = volume_of(&mut app);
    let instance = start.instance.clone().unwrap();
    let r0 = start.revision.unwrap();

    // A drag: each step carries the revision of the previous echo.
    let Reply::Ok(v) = set_volume(&mut app, -19.0, 0.0, false, Some(&instance), Some(r0)) else {
        panic!("first step refused")
    };
    let r1 = serde_json::from_value::<VolumeStatus>(v)
        .unwrap()
        .revision
        .unwrap();
    assert!(r1 > r0);
    // Rewriting the same level is no change: the revision stays.
    let Reply::Ok(v) = set_volume(&mut app, -19.0, 0.0, false, Some(&instance), Some(r1)) else {
        panic!("same-level step refused")
    };
    assert_eq!(
        serde_json::from_value::<VolumeStatus>(v).unwrap().revision,
        Some(r1)
    );

    // Someone else changes the level (the CLI sends no condition).
    assert!(matches!(
        set_volume(&mut cli, -40.0, 0.0, true, None, None),
        Reply::Ok(_)
    ));
    for (vol, bal, muted) in [
        (-19.5, 0.0, false),
        (-40.0, -2.0, true),
        (-40.0, 0.0, false),
    ] {
        match set_volume(&mut app, vol, bal, muted, Some(&instance), Some(r1)) {
            Reply::Error(e) => assert_eq!(e.code, RpcError::STALE_REVISION, "{e:?}"),
            Reply::Ok(v) => panic!("a write from an old revision was applied: {v}"),
        }
    }
    let now = volume_of(&mut app);
    assert_eq!(
        (now.volume_db, now.balance_db, now.muted),
        (-40.0, 0.0, true)
    );
    assert!(now.revision.unwrap() > r1);
}

// ---------------------------------------------------------------- presets

#[test]
fn presets_round_trip_and_track_the_active_one() {
    let harness = Harness::start("presets");
    let mut client = harness.client();
    client.ok(Call::SetEq(Some(profile())));
    client.ok(Call::SetUpsampler(
        audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(4),
            taps: 8192,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap(),
    ));

    // Save chain A; the name is trimmed and normalized.
    let info: PresetInfo = serde_json::from_value(client.ok(Call::SavePreset {
        name: "  Bass  ".into(),
        overwrite: false,
        part: None,
        parts: None,
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
        part: None,
    }))
    .unwrap();
    assert_eq!(preset.chain, config.chain);
    assert_eq!(preset.chain.eq.as_ref().unwrap().bands.len(), 1);

    // Saving again without overwrite is refused (and leaves the file alone).
    match client.call(Call::SavePreset {
        name: "Bass".into(),
        overwrite: false,
        part: None,
        parts: None,
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
        part: None,
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
            name: "Bass".into(),
            part: None,
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
        part: None,
        parts: None,
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
        level: LevelMatch::default(),
        headroom: Default::default(),
        ..AbConfig::chain(chain_b)
    })));
    client.ok(Call::SetUpsampler(UpsamplerConfig {
        target: RateTarget::Off,
        ..UpsamplerConfig::default()
    }));
    client.ok(Call::LoadPreset {
        name: "Bass".into(),
        part: None,
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
        part: None,
    }))
    .unwrap();
    assert_eq!(info.name, "Bass boost");
    let status: Status = serde_json::from_value(client.ok(Call::Status)).unwrap();
    assert_eq!(status.preset.unwrap().name, "Bass boost");
    let config: EngineConfig = serde_json::from_value(client.ok(Call::GetConfig)).unwrap();
    assert_eq!(config.active_preset.as_deref(), Some("Bass boost"));
    match client.call(Call::GetPreset {
        name: "Bass".into(),
        part: None,
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("the old name must be gone: {other:?}"),
    }

    // Deleting the active preset clears the marker.
    assert_eq!(
        client.ok(Call::DeletePreset {
            name: "Bass boost".into(),
            part: None,
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
        part: None,
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("unexpected {other:?}"),
    }
    match client.call(Call::DeletePreset {
        name: "nope".into(),
        part: None,
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("unexpected {other:?}"),
    }
    match client.call(Call::SavePreset {
        name: "  ".into(),
        overwrite: false,
        part: None,
        parts: None,
    }) {
        Reply::Error(error) => assert_eq!(error.code, RpcError::INVALID_PARAMS),
        other => panic!("unexpected {other:?}"),
    }
}

// ---------------------------------------------------------------- devices

// ---------------------------------------------------------------- persistence

fn studio_layer(
    presets: &[audivane_protocol::HarmonicsPreset],
) -> audivane_protocol::HarmonicsLayer {
    presets
        .iter()
        .find(|p| p.id == "tape_studio")
        .unwrap()
        .to_layer()
}
