use super::*;
use std::time::Instant;

/// A real daemon (hardware-free waiting engine, no PipeWire/MPRIS) on `socket`.
fn real_daemon(socket: &Path, volume_db: f64) -> audivane_dsp_daemon::control::daemon::Daemon {
    use audivane_dsp_daemon::control::daemon::{Daemon, DaemonOptions};
    use audivane_dsp_daemon::engine::waiting::WaitingEngine;
    use audivane_protocol::ipc::{EngineConfig, RoutingMode};
    let mut config = EngineConfig {
        device: "hw:0,0".into(),
        ..EngineConfig::default()
    };
    config.routing = RoutingMode::Apps { rules: Vec::new() };
    config.volume.volume_db = volume_db;
    let engine = Arc::new(WaitingEngine::new(config.clone(), "test".into()));
    let mut opts = DaemonOptions::new(socket.to_path_buf());
    opts.graph = false;
    opts.mpris = false;
    Daemon::start(engine, config, opts).expect("start daemon")
}

/// The native client keeps one request socket and reconnects it on its
/// own: a `set_volume_guarded` the webview made against one daemon may be written
/// to the daemon that replaced it (here through the client's retry after
/// the old connection died). Bound to the old instance, it is refused and
/// changes nothing.
#[test]
fn a_guarded_set_volume_for_a_restarted_daemon_is_refused_through_the_native_client() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    let first = real_daemon(&socket, -12.0);
    let p = socket.clone();
    let client = DaemonClient::new(Arc::new(move || p.clone()));
    let old = client.call("status", None).unwrap();
    let old_instance = old["volume"]["instance"].as_str().unwrap().to_string();
    assert!(!old_instance.is_empty());
    assert_eq!(old["volume"]["volume_db"], json!(-12.0));
    first.shutdown();

    let second = real_daemon(&socket, -30.0);
    // The request made from the first daemon's level, delivered now.
    let request = json!({
        "method": "set_volume_guarded",
        "params": {"volume_db": 0.0, "balance_db": -2.0, "muted": false,
                   "if_instance": old_instance, "if_revision": old["volume"]["revision"]},
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    let err = loop {
        match client.call_request(request.clone()) {
            Err(e) if e.code == CallError::DISCONNECTED && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => break e,
            Ok(v) => panic!("the old daemon's volume change was applied: {v}"),
        }
    };
    assert_eq!(
        err.code,
        audivane_protocol::ipc::RpcError::STALE_INSTANCE,
        "{err}"
    );
    let now = client.call("status", None).unwrap();
    assert_ne!(now["volume"]["instance"].as_str().unwrap(), old_instance);
    assert_eq!(now["volume"]["volume_db"], json!(-30.0));
    assert_eq!(now["volume"]["balance_db"], json!(0.0));

    // Bound to the daemon that answers: applied.
    let ok = client
        .call(
            "set_volume_guarded",
            Some(json!({"volume_db": -30.5, "balance_db": 0.0, "muted": false,
                        "if_instance": now["volume"]["instance"], "if_revision": now["volume"]["revision"]})),
        )
        .unwrap();
    assert_eq!(ok["volume_db"], json!(-30.5));
    second.shutdown();
}
