//! `avdsp ctl` end-to-end tests: the real CLI binary against the real IPC
//! server and a mock engine.

mod common;

use audivane_protocol::ipc::{EngineConfig, Status};
use common::{Harness, ctl, ctl_json, ctl_ok};

fn get_config(socket: &std::path::Path) -> EngineConfig {
    serde_json::from_value(ctl_json(socket, &["config", "get"])).expect("config JSON")
}

#[test]
fn status_human_and_json() {
    let harness = Harness::start("cli-status");
    let human = ctl_ok(&harness.socket, &["status"]);
    assert!(human.contains("audivane-dsp"), "{human}");
    assert!(human.contains("state:"), "{human}");
    assert!(human.contains("passthrough:"), "{human}");

    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    assert_eq!(status.device, "hw:0,0");
}

#[test]
fn config_get_and_set() {
    let harness = Harness::start("cli-config");
    let config = get_config(&harness.socket);
    assert_eq!(config.device, "hw:0,0");

    let replacement = serde_json::json!({
        "device": "hw:2,0",
        "quantum": 256,
        "dsp_enabled": true,
    });
    let path = harness.dir.join("new-config.json");
    std::fs::write(&path, serde_json::to_string_pretty(&replacement).unwrap()).unwrap();
    let stdout = ctl_ok(&harness.socket, &["config", "set", path.to_str().unwrap()]);
    assert!(stdout.contains("applied"), "{stdout}");
    let config = get_config(&harness.socket);
    assert_eq!(config.device, "hw:2,0");
    assert_eq!(config.quantum, 256);

    // Invalid config: exit 1 and a message naming the problem.
    let bad = harness.dir.write("bad.json", r#"{"device":"pulse"}"#);
    let output = ctl(&harness.socket, &["config", "set", bad.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("hw:"), "{stderr}");
    // The stored config is untouched.
    assert_eq!(get_config(&harness.socket).device, "hw:2,0");
}

#[test]
fn missing_daemon_has_a_clear_message() {
    let dir = common::TempDir::new("cli-missing");
    let socket = dir.join("nothing.sock");
    let output = ctl(&socket, &["status"]);
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not running"), "{stderr}");
    assert!(stderr.contains("nothing.sock"), "{stderr}");
}

#[test]
fn volume_commands_and_status() {
    use audivane_protocol::ipc::VolumeStatus;

    let harness = Harness::start("cli-volume");

    // Query default volume (both human and JSON).
    let stdout = ctl_ok(&harness.socket, &["volume"]);
    assert!(stdout.contains("0.0 dB"), "{stdout}");
    assert!(stdout.contains("center"), "{stdout}");
    assert!(stdout.contains("[digital]"), "{stdout}");

    let v: VolumeStatus = serde_json::from_value(ctl_json(&harness.socket, &["volume"])).unwrap();
    assert_eq!(v.volume_db, 0.0);
    assert_eq!(v.balance_db, 0.0);
    assert!(!v.muted);
    assert!(!v.hardware);

    // Partial updates with negative values and flags merge cleanly with stored config.
    let stdout = ctl_ok(
        &harness.socket,
        &["volume", "-12.5", "--balance", "-3.5", "--mute"],
    );
    assert!(stdout.contains("-12.5 dB"), "{stdout}");
    assert!(stdout.contains("-3.5 dB"), "{stdout}");
    assert!(stdout.contains("MUTED"), "{stdout}");
    let cfg = get_config(&harness.socket);
    assert_eq!(cfg.volume.volume_db, -12.5);
    assert_eq!(cfg.volume.balance_db, -3.5);
    assert!(cfg.volume.muted);

    // `--unmute` only clears mute, keeping attenuation and balance.
    let v: VolumeStatus =
        serde_json::from_value(ctl_json(&harness.socket, &["volume", "--unmute"])).unwrap();
    assert_eq!(v.volume_db, -12.5);
    assert_eq!(v.balance_db, -3.5);
    assert!(!v.muted);

    // `avdsp ctl status` renders the volume line and respects hardware mixer metadata.
    let status_txt = ctl_ok(&harness.socket, &["status"]);
    assert!(status_txt.contains("volume:"), "{status_txt}");
    assert!(status_txt.contains("-12.5 dB"), "{status_txt}");

    harness.engine.set_snapshot(|s| {
        s.volume = Some(VolumeStatus {
            volume_db: -6.0,
            balance_db: 2.0,
            muted: false,
            hardware: true,
            min_db: -64.0,
            max_db: 0.0,
            ..VolumeStatus::from_config(&Default::default())
        });
    });
    let hw_status = ctl_ok(&harness.socket, &["status"]);
    assert!(
        hw_status.contains("hardware, effective range -64.0..=0.0 dB"),
        "{hw_status}"
    );
    assert!(hw_status.contains("L -2.0 dB"), "{hw_status}");

    // Below a -64 dB card floor: the range is the effective one (digital
    // attenuation included), and the digital part is shown.
    harness.engine.set_snapshot(|s| {
        s.volume = Some(VolumeStatus {
            volume_db: -90.0,
            balance_db: 0.0,
            muted: false,
            hardware: true,
            min_db: -120.0,
            max_db: 0.0,
            hardware_db: Some(-64.0),
            software_db: -26.0,
            ..VolumeStatus::from_config(&Default::default())
        });
    });
    let deep = ctl_ok(&harness.socket, &["status"]);
    assert!(
        deep.contains("hardware, effective range -120.0..=0.0 dB"),
        "{deep}"
    );
    assert!(
        deep.contains("hardware -64.0 dB + digital -26.0 dB"),
        "{deep}"
    );

    // Out-of-range volume returns daemon validation error (exit 1).
    let bad = ctl(&harness.socket, &["volume", "1.5"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("volume"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
}
