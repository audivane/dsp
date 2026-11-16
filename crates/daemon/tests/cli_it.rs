//! `avdsp ctl` end-to-end tests: the real CLI binary against the real IPC
//! server and a mock engine.

mod common;

use audivane_dsp_daemon::api::Engine;
use audivane_protocol::ipc::{AbSide, EngineConfig, LevelMatch, RoutingMode, SinkChoice, Status};
use audivane_protocol::{CutoffMode, FilterPhase, RateTarget};
use common::{Harness, ctl, ctl_json, ctl_ok};
use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
fn eq_conv_upsample_dsp() {
    let harness = Harness::start("cli-chain");

    let apo = harness.dir.write(
        "profile.txt",
        "Preamp: -6 dB\nFilter 1: ON PK Fc 100 Hz Gain 3 dB Q 1.0\n",
    );
    let stdout = ctl_ok(&harness.socket, &["eq", "set", apo.to_str().unwrap()]);
    assert!(stdout.contains("EQ set"), "{stdout}");
    let config = get_config(&harness.socket);
    let eq = config.chain.eq.expect("eq set");
    assert_eq!(eq.bands.len(), 1);
    assert!((eq.preamp_db + 6.0).abs() < 1e-9);

    let stdout = ctl_ok(&harness.socket, &["eq", "set", "--off"]);
    assert!(stdout.contains("cleared"), "{stdout}");
    assert!(get_config(&harness.socket).chain.eq.is_none());

    let ir = harness.dir.write("ir.wav", "RIFF....WAVE");
    let stdout = ctl_ok(
        &harness.socket,
        &["conv", "set", ir.to_str().unwrap(), "--gain-db", "-2.5"],
    );
    assert!(stdout.contains("convolution set"), "{stdout}");
    let convs = get_config(&harness.socket).chain.convolution;
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].ir_path, ir);
    assert!((convs[0].gain_db + 2.5).abs() < 1e-9);
    ctl_ok(&harness.socket, &["conv", "set", "--off"]);
    assert!(get_config(&harness.socket).chain.convolution.is_empty());

    // Split L/R EQ via --left/--right and `eq show`
    let apo_r = harness.dir.write(
        "right.txt",
        "Preamp: -4 dB\nFilter 1: ON PK Fc 2000 Hz Gain -2 dB Q 1.5\n",
    );
    let stdout = ctl_ok(
        &harness.socket,
        &[
            "eq",
            "set",
            "--left",
            apo.to_str().unwrap(),
            "--right",
            apo_r.to_str().unwrap(),
        ],
    );
    assert!(stdout.contains("EQ set (L/R)"), "{stdout}");
    let eq = get_config(&harness.socket)
        .chain
        .eq
        .expect("layered eq set");
    assert!(eq.bands.is_empty());
    let left = eq.left.expect("left channel set");
    assert_eq!(left.bands.len(), 1);
    let right = eq.right.expect("right channel set");
    assert_eq!(right.bands.len(), 1);
    assert!((right.preamp_db + 4.0).abs() < 1e-9);
    let show_out = ctl_ok(&harness.socket, &["eq", "show"]);
    assert!(show_out.contains("L+R:"), "{show_out}");
    assert!(show_out.contains("L:"), "{show_out}");
    assert!(show_out.contains("R:"), "{show_out}");

    // Layered EQ `--section` band edits + `eq section <sec> on|off|clear`
    ctl_ok(&harness.socket, &["eq", "set", apo.to_str().unwrap()]);
    ctl_ok(&harness.socket, &["eq", "preamp", "-3.0"]);
    ctl_ok(
        &harness.socket,
        &[
            "eq",
            "add",
            "--section",
            "right",
            "--freq",
            "138",
            "--gain",
            "-4.0",
            "--q",
            "3.0",
        ],
    );
    ctl_ok(
        &harness.socket,
        &["eq", "edit", "0", "--section", "right", "--gain", "-4.5"],
    );
    ctl_ok(
        &harness.socket,
        &["eq", "disable", "0", "--section", "right"],
    );
    assert!(
        !get_config(&harness.socket)
            .chain
            .eq
            .as_ref()
            .unwrap()
            .right
            .as_ref()
            .unwrap()
            .bands[0]
            .enabled
    );
    ctl_ok(
        &harness.socket,
        &["eq", "enable", "0", "--section", "right"],
    );
    ctl_ok(&harness.socket, &["eq", "section", "left", "on"]);
    ctl_ok(&harness.socket, &["eq", "section", "left", "off"]);

    // Multi-IR `conv add`, `conv list`, `conv disable`, `conv enable`, `conv remove`, `conv clear`, and `status`
    let ir2 = harness.dir.write("ir2.wav", "RIFF....WAVE");
    ctl_ok(
        &harness.socket,
        &[
            "conv",
            "add",
            ir.to_str().unwrap(),
            "--name",
            "Room",
            "--gain-db",
            "-1.0",
        ],
    );
    ctl_ok(
        &harness.socket,
        &["conv", "add", ir2.to_str().unwrap(), "--name", "Headphones"],
    );
    let list_out = ctl_ok(&harness.socket, &["conv", "list"]);
    assert!(
        list_out.contains("Room") && list_out.contains("Headphones"),
        "{list_out}"
    );
    ctl_ok(&harness.socket, &["conv", "disable", "Room"]);
    assert!(!get_config(&harness.socket).chain.convolution[0].enabled);
    ctl_ok(&harness.socket, &["conv", "enable", "0"]);
    assert!(get_config(&harness.socket).chain.convolution[0].enabled);

    let status_out = ctl_ok(&harness.socket, &["status"]);
    assert!(
        status_out.contains("L+R: 1 band, L: off, R: 1 band, preamp -3.0 dB"),
        "{status_out}"
    );
    assert!(status_out.contains("conv:"), "{status_out}");
    assert!(status_out.contains("Room"), "{status_out}");

    ctl_ok(
        &harness.socket,
        &["eq", "remove", "0", "--section", "right"],
    );
    ctl_ok(&harness.socket, &["eq", "section", "right", "clear"]);
    ctl_ok(&harness.socket, &["eq", "section", "left", "clear"]);
    let eq_after = get_config(&harness.socket).chain.eq.unwrap();
    assert!(eq_after.left.is_none());
    assert!(eq_after.right.is_none());

    ctl_ok(&harness.socket, &["conv", "remove", "Room"]);
    let convs = get_config(&harness.socket).chain.convolution;
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].name.as_deref(), Some("Headphones"));

    ctl_ok(&harness.socket, &["conv", "clear"]);
    assert!(get_config(&harness.socket).chain.convolution.is_empty());
    ctl_ok(&harness.socket, &["eq", "set", "--off"]);

    let stdout = ctl_ok(
        &harness.socket,
        &[
            "upsample", "--ratio", "4", "--taps", "8192", "--phase", "minimum",
        ],
    );
    assert!(stdout.contains("x4"), "{stdout}");
    let up = get_config(&harness.socket).chain.upsampler;
    assert_eq!(up.target, RateTarget::Ratio(4));
    assert_eq!(up.taps, 8192);
    assert_eq!(up.phase, FilterPhase::Minimum);

    ctl_ok(
        &harness.socket,
        &["upsample", "--cutoff-mode", "nyquist-stop"],
    );
    let up = get_config(&harness.socket).chain.upsampler;
    assert_eq!(up.cutoff_mode, CutoffMode::NyquistStop);

    let conflict = ctl(
        &harness.socket,
        &[
            "upsample",
            "--cutoff",
            "0.95",
            "--cutoff-mode",
            "nyquist-stop",
        ],
    );
    assert_eq!(conflict.status.code(), Some(3), "conflict uses exit 3");

    ctl_ok(&harness.socket, &["upsample", "--cutoff", "0.95"]);
    let up = get_config(&harness.socket).chain.upsampler;
    assert_eq!(up.cutoff_mode, CutoffMode::Fraction);
    assert_eq!(up.cutoff, 0.95);

    ctl_ok(
        &harness.socket,
        &["upsample", "--cutoff", "1.0", "--cutoff-mode", "fraction"],
    );
    let up = get_config(&harness.socket).chain.upsampler;
    assert_eq!(up.cutoff_mode, CutoffMode::Fraction);
    assert_eq!(up.cutoff, 1.0);

    ctl_ok(&harness.socket, &["upsample", "--max-rate", "768000"]);
    assert_eq!(
        get_config(&harness.socket).chain.upsampler.target,
        RateTarget::MaxRate(768_000)
    );
    ctl_ok(&harness.socket, &["upsample", "--off"]);
    assert_eq!(
        get_config(&harness.socket).chain.upsampler.target,
        RateTarget::Off
    );

    let output = ctl(&harness.socket, &["upsample"]);
    assert_eq!(output.status.code(), Some(3), "usage errors use exit 3");

    ctl_ok(&harness.socket, &["dsp", "off"]);
    assert!(!get_config(&harness.socket).dsp_enabled);
    ctl_ok(&harness.socket, &["dsp", "on"]);
    assert!(get_config(&harness.socket).dsp_enabled);
}

#[test]
fn upsample_response_prints_stats() {
    let harness = Harness::start("cli-upsample-response");

    // Default config has upsampling off: a flat, ratio-1 response.
    let json = ctl_json(
        &harness.socket,
        &["upsample", "response", "--rate", "44100"],
    );
    let response: audivane_protocol::ipc::UpsamplerResponse =
        serde_json::from_value(json).expect("response JSON");
    assert_eq!(response.ratio, 1);
    assert_eq!(response.effective_cutoff, 1.0);
    assert_eq!(response.impulse, vec![1.0]);
    assert!(response.magnitude_db.iter().all(|db| db.abs() < 1e-9));

    ctl_ok(
        &harness.socket,
        &[
            "upsample",
            "--ratio",
            "4",
            "--taps",
            "16384",
            "--phase",
            "linear",
            "--cutoff-mode",
            "nyquist-stop",
        ],
    );
    let text = ctl_ok(
        &harness.socket,
        &["upsample", "response", "--rate", "44100"],
    );
    assert!(text.contains("44100 Hz -> 176400 Hz (x4)"), "{text}");
    assert!(text.contains("cutoff:"), "{text}");
    assert!(text.contains("passband:"), "{text}");
    assert!(text.contains("stopband:"), "{text}");
    assert!(text.contains("latency:"), "{text}");

    let json = ctl_json(
        &harness.socket,
        &["upsample", "response", "--rate", "44100"],
    );
    let response: audivane_protocol::ipc::UpsamplerResponse =
        serde_json::from_value(json).expect("response JSON");
    assert_eq!(response.ratio, 4);
    assert_eq!(response.taps, 16384);
    assert!(response.effective_cutoff < 1.0 && response.effective_cutoff > 0.9);
    assert!(response.stopband_atten_db > 100.0);
}

#[test]
fn ab_commands() {
    let harness = Harness::start("cli-ab");
    // Nothing configured yet.
    let output = ctl(&harness.socket, &["ab", "a"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("A/B is not configured"));

    let profile_b = harness.dir.write(
        "b.txt",
        "Preamp: -4 dB\nFilter 1: ON PK Fc 200 Hz Gain -3 dB Q 1.0\n",
    );
    let stdout = ctl_ok(
        &harness.socket,
        &["ab", "set", profile_b.to_str().unwrap(), "--level", "-3.5"],
    );
    assert!(stdout.contains("A/B enabled"), "{stdout}");
    let config = get_config(&harness.socket);
    let ab = config.ab.expect("A/B configured");
    assert_eq!(ab.level, LevelMatch::Manual { offset_db: -3.5 });
    assert_eq!(ab.chain_b.eq.unwrap().bands.len(), 1);

    let stdout = ctl_ok(&harness.socket, &["ab", "b", "--fade", "25"]);
    assert!(stdout.contains("side B"), "{stdout}");
    let flags = &harness.engine.snapshot().ab;
    assert_eq!(flags.as_ref().map(|a| a.current), Some(AbSide::B));

    let stdout = ctl_ok(&harness.socket, &["ab", "a"]);
    assert!(stdout.contains("side A"), "{stdout}");

    let stdout = ctl_ok(&harness.socket, &["ab", "off"]);
    assert!(stdout.contains("A/B off"), "{stdout}");
    assert!(get_config(&harness.socket).ab.is_none());
}

#[test]
fn streams_route_mode_now_playing_selftest_release() {
    let harness = Harness::start("cli-misc");

    let stdout = ctl_ok(&harness.socket, &["streams"]);
    assert!(stdout.contains("no streams"), "{stdout}");
    let streams: Value = ctl_json(&harness.socket, &["streams"]);
    assert_eq!(streams, Value::Array(vec![]));

    // Routing needs the graph watcher, which this harness does not start.
    // `--app NAME dsp` must parse and reach the daemon too.
    let output = ctl(&harness.socket, &["route", "--app", "pw-play", "dsp"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("PipeWire graph"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A destination without a node is a usage error (exit 3).
    let output = ctl(&harness.socket, &["route", "5"]);
    assert_eq!(output.status.code(), Some(3));
    let output = ctl(&harness.socket, &["route", "5", "dsp"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("PipeWire graph"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = ctl_ok(
        &harness.socket,
        &[
            "mode",
            "apps",
            "--rule",
            "strawberry=dsp",
            "--rule",
            "mpv=direct",
        ],
    );
    assert!(stdout.contains("per-app"), "{stdout}");
    let config = get_config(&harness.socket);
    match config.routing {
        RoutingMode::Apps { rules } => {
            assert_eq!(rules.len(), 2);
            assert_eq!(rules[0].app, "strawberry");
            assert_eq!(rules[0].sink, SinkChoice::Dsp);
            assert_eq!(rules[1].sink, SinkChoice::Direct);
        }
        other => panic!("unexpected {other:?}"),
    }
    ctl_ok(&harness.socket, &["mode", "system"]);
    assert!(matches!(
        get_config(&harness.socket).routing,
        RoutingMode::System
    ));

    let stdout = ctl_ok(&harness.socket, &["now-playing"]);
    assert!(stdout.contains("nothing playing"), "{stdout}");

    let stdout = ctl_ok(&harness.socket, &["selftest"]);
    assert!(stdout.contains("PASS"), "{stdout}");
    assert!(stdout.contains("deadbeef"), "{stdout}");
    let result: Value = ctl_json(&harness.socket, &["selftest"]);
    assert_eq!(result["passed"], true);

    ctl_ok(&harness.socket, &["release"]);
    assert_eq!(
        harness
            .engine
            .releases
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    assert_eq!(status.state, audivane_protocol::ipc::EngineState::Idle);
    ctl_ok(&harness.socket, &["acquire"]);
    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    assert_eq!(status.state, audivane_protocol::ipc::EngineState::Ready);
}

#[test]
fn watch_streams_events_then_shutdown() {
    let harness = Harness::start("cli-watch");
    let mut child = Command::new(common::avdsp_bin())
        .args([
            "ctl",
            "--socket",
            harness.socket.to_str().unwrap(),
            "watch",
            "meters",
            "--json",
        ])
        .env_remove("AUDIVANE_DSP_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn avdsp ctl watch");
    std::thread::sleep(Duration::from_millis(700));
    child.kill().ok();
    let output = child.wait_with_output().expect("watch output");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains(r#""event":"meters""#),
        "no meters events:\n{stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The daemon is still running; shutdown works and the socket goes away.
    let stdout = ctl_ok(&harness.socket, &["shutdown"]);
    assert!(stdout.contains("stopping"), "{stdout}");
    assert!(harness.daemon.wait_timeout(Duration::from_secs(2)));
    harness.daemon.shutdown();
    let deadline = Instant::now() + Duration::from_secs(2);
    while harness.socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!harness.socket.exists());

    let output = ctl(&harness.socket, &["status"]);
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not running"), "{stderr}");
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
fn ab_set_then_upsample_off_through_the_cli() {
    // The exact reported sequence: `ab set` at x4 then `upsample --off`.
    let harness = Harness::start("cli-ab-repro");
    ctl_ok(
        &harness.socket,
        &["upsample", "--ratio", "4", "--taps", "8192"],
    );
    let profile_b = harness.dir.write(
        "b.txt",
        "Preamp: -4 dB\nFilter 1: ON PK Fc 200 Hz Gain -3 dB Q 1.0\n",
    );
    ctl_ok(&harness.socket, &["ab", "set", profile_b.to_str().unwrap()]);

    let stdout = ctl_ok(&harness.socket, &["upsample", "--off"]);
    assert!(stdout.contains("upsampling off"), "{stdout}");
    let config = get_config(&harness.socket);
    assert_eq!(config.chain.upsampler.target, RateTarget::Off);
    let chain_b = config.ab.expect("A/B still configured").chain_b;
    assert_eq!(chain_b.upsampler, config.chain.upsampler);
    assert_eq!(chain_b.output, config.chain.output);
}

#[test]
fn ab_set_reports_when_chain_b_was_realigned() {
    let harness = Harness::start("cli-ab-note");
    ctl_ok(&harness.socket, &["upsample", "--ratio", "4"]);
    let profile_b = harness
        .dir
        .write("b.txt", "Filter 1: ON PK Fc 100 Hz Gain 2 dB Q 1.0\n");
    let stdout = ctl_ok(&harness.socket, &["ab", "set", profile_b.to_str().unwrap()]);
    // The CLI builds a matching chain B, so there is nothing to realign.
    assert!(!stdout.contains("note:"), "{stdout}");
    assert!(stdout.contains("A/B enabled"), "{stdout}");
}

#[test]
fn watch_without_json_prints_human_lines() {
    let harness = Harness::start("cli-watch-human");
    let mut child = Command::new(common::avdsp_bin())
        .args(["ctl", "--socket", harness.socket.to_str().unwrap(), "watch"])
        .env_remove("AUDIVANE_DSP_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn avdsp ctl watch");
    std::thread::sleep(Duration::from_millis(900));
    child.kill().ok();
    let output = child.wait_with_output().expect("watch output");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // Redirected stdout is plain newline-delimited human text, never JSON
    // and never in-place control characters.
    assert!(
        !stdout.contains("\"event\""),
        "watch without --json must not print JSON:\n{stdout}"
    );
    assert!(
        !stdout.contains('\r'),
        "no carriage returns when redirected"
    );
    assert!(stdout.contains("peak ["), "no meter lines:\n{stdout}");
    assert!(stdout.contains("hw:0,0"), "no status line:\n{stdout}");
    assert!(
        stdout.contains("passthrough:"),
        "no passthrough verdict:\n{stdout}"
    );
    assert!(stdout.contains("stream(s):"), "no streams line:\n{stdout}");
    assert!(stdout.contains("config:"), "no config line:\n{stdout}");
    assert!(stdout.contains("preset(s):"), "no presets line:\n{stdout}");
    harness.daemon.shutdown();
}

#[test]
fn watch_without_json_prints_human_status_only() {
    let harness = Harness::start("cli-watch-status");
    let mut child = Command::new(common::avdsp_bin())
        .args([
            "ctl",
            "--socket",
            harness.socket.to_str().unwrap(),
            "watch",
            "status",
        ])
        .env_remove("AUDIVANE_DSP_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn avdsp ctl watch status");
    std::thread::sleep(Duration::from_millis(500));
    child.kill().ok();
    let output = child.wait_with_output().expect("watch output");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("ready"), "{stdout}");
    assert!(stdout.contains("dsp load"), "{stdout}");
    assert!(!stdout.contains("\"event\""), "{stdout}");
    harness.daemon.shutdown();
}

#[test]
fn preset_commands() {
    let harness = Harness::start("cli-presets");
    let apo = harness.dir.write(
        "flat.txt",
        "Preamp: -6 dB\nFilter 1: ON PK Fc 100 Hz Gain 3 dB Q 1.0\n",
    );
    ctl_ok(&harness.socket, &["eq", "set", apo.to_str().unwrap()]);
    ctl_ok(
        &harness.socket,
        &["upsample", "--ratio", "2", "--taps", "8192"],
    );

    let stdout = ctl_ok(&harness.socket, &["preset", "save", "My Preset"]);
    assert!(stdout.contains("saved preset"), "{stdout}");

    // Human list marks the active preset; JSON is a plain array.
    let stdout = ctl_ok(&harness.socket, &["preset", "list"]);
    assert!(stdout.contains("My Preset"), "{stdout}");
    assert!(stdout.contains("1 band"), "{stdout}");
    assert!(stdout.contains("(active)"), "{stdout}");
    let list = ctl_json(&harness.socket, &["preset", "list"]);
    assert_eq!(list[0]["name"], "My Preset");
    assert_eq!(list[0]["bands"], 1);
    assert_eq!(list[0]["convolution"], 0);

    // `preset show` prints the stored preset.
    let preset = ctl_json(&harness.socket, &["preset", "show", "My Preset"]);
    assert_eq!(preset["version"], 1);
    assert_eq!(preset["name"], "My Preset");
    assert_eq!(preset["chain"]["upsampler"]["target"]["ratio"], 2);

    // Saving over an existing name needs --overwrite; the CLI exits 1.
    let output = ctl(&harness.socket, &["preset", "save", "My Preset"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already exists"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ctl_ok(
        &harness.socket,
        &["preset", "save", "My Preset", "--overwrite"],
    );

    // Editing chain A marks the active preset modified in `status`.
    ctl_ok(&harness.socket, &["eq", "set", "--off"]);
    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    let preset_state = status.preset.expect("active preset");
    assert_eq!(preset_state.name, "My Preset");
    assert!(preset_state.modified);
    let stdout = ctl_ok(&harness.socket, &["status"]);
    assert!(stdout.contains("(modified)"), "{stdout}");

    // Loading restores it.
    let stdout = ctl_ok(&harness.socket, &["preset", "load", "My Preset"]);
    assert!(stdout.contains("loaded preset"), "{stdout}");
    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    assert!(!status.preset.unwrap().modified);
    assert!(get_config(&harness.socket).chain.eq.is_some());

    // Rename follows the active preset.
    let stdout = ctl_ok(
        &harness.socket,
        &["preset", "rename", "My Preset", "Renamed Preset"],
    );
    assert!(stdout.contains("renamed preset"), "{stdout}");
    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    assert_eq!(status.preset.unwrap().name, "Renamed Preset");
    let output = ctl(&harness.socket, &["preset", "show", "My Preset"]);
    assert_eq!(output.status.code(), Some(1));

    // Import a file (name defaults to the file stem) without loading it.
    let autoeq = harness.dir.write(
        "AutoEQ.txt",
        "Preamp: -3 dB\nFilter 1: ON PK Fc 2000 Hz Gain -2 dB Q 0.8\nFilter 2: ON HS Fc 8000 Hz Gain 1 dB Q 0.7\n",
    );
    let stdout = ctl_ok(
        &harness.socket,
        &["preset", "import", autoeq.to_str().unwrap()],
    );
    assert!(stdout.contains("imported preset"), "{stdout}");
    let list = ctl_json(&harness.socket, &["preset", "list"]);
    assert_eq!(list.as_array().unwrap().len(), 2);
    let imported = ctl_json(&harness.socket, &["preset", "show", "AutoEQ"]);
    assert_eq!(
        imported["chain"]["eq"]["bands"].as_array().unwrap().len(),
        2
    );
    // Importing does not change the active preset.
    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    assert_eq!(status.preset.unwrap().name, "Renamed Preset");

    // `--name` overrides the file stem.
    ctl_ok(
        &harness.socket,
        &[
            "preset",
            "import",
            autoeq.to_str().unwrap(),
            "--name",
            "Second import",
        ],
    );
    assert_eq!(
        ctl_json(&harness.socket, &["preset", "list"])
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Deleting the active preset clears it.
    let stdout = ctl_ok(&harness.socket, &["preset", "delete", "Renamed Preset"]);
    assert!(stdout.contains("deleted preset"), "{stdout}");
    let status: Status = serde_json::from_value(ctl_json(&harness.socket, &["status"])).unwrap();
    assert!(status.preset.is_none());
    let stdout = ctl_ok(&harness.socket, &["preset", "list"]);
    assert!(!stdout.contains("(active)"), "{stdout}");

    // An unknown preset is a daemon error (exit 1), not a usage error.
    let output = ctl(&harness.socket, &["preset", "load", "nope"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no preset"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn devices_command() {
    let harness = Harness::start("cli-devices");
    let stdout = ctl_ok(&harness.socket, &["devices"]);
    let devices = ctl_json(&harness.socket, &["devices"]);
    let devices = devices.as_array().expect("device array");
    if devices.is_empty() {
        assert!(stdout.contains("no ALSA playback devices"), "{stdout}");
        return;
    }
    assert!(stdout.contains("DEVICE"), "{stdout}");
    for device in devices {
        let id = device["id"].as_str().unwrap();
        assert!(id.starts_with("hw:CARD="), "{id}");
        assert!(device["hdmi"].is_boolean());
        assert!(device["loopback"].is_boolean());
        assert!(device["active"].is_boolean());
    }
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
        });
    });
    let hw_status = ctl_ok(&harness.socket, &["status"]);
    assert!(
        hw_status.contains("hardware, -64.0..=0.0 dB"),
        "{hw_status}"
    );
    assert!(hw_status.contains("L -2.0 dB"), "{hw_status}");

    // Out-of-range volume returns daemon validation error (exit 1).
    let bad = ctl(&harness.socket, &["volume", "1.5"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("volume"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
}
