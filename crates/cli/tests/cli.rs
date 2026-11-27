//! End-to-end tests for the `avdsp` binary.
//!
//! Everything runs on generated files in a per-test temporary directory; no
//! fixtures are checked in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn avdsp() -> &'static str {
    env!("CARGO_BIN_EXE_avdsp")
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("avdsp-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path(name);
        fs::write(&path, contents).expect("write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run(dir: &TempDir, args: &[&str]) -> Output {
    Command::new(avdsp())
        .args(args)
        .current_dir(&dir.path)
        .stdin(Stdio::null())
        .output()
        .expect("run avdsp")
}

fn run_ok(dir: &TempDir, args: &[&str]) -> Output {
    let output = run(dir, args);
    assert!(
        output.status.success(),
        "avdsp {args:?} failed with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The payload of the RIFF `data` chunk.
fn data_chunk(bytes: &[u8]) -> &[u8] {
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        if id == b"data" {
            return &bytes[start..start + size];
        }
        pos = start + size + (size & 1);
    }
    panic!("no data chunk");
}

fn value(text: &str, key: &str) -> f64 {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            let token = rest
                .split_whitespace()
                .next()
                .expect("numeric field")
                .trim_end_matches('x');
            return token.parse().expect("numeric field");
        }
    }
    panic!("missing {key:?} in:\n{text}");
}

fn gen_white(dir: &TempDir, name: &str, bits: u32) {
    run_ok(
        dir,
        &[
            "gen",
            "white",
            "--rate",
            "44100",
            "--seconds",
            "0.02",
            "--bits",
            &bits.to_string(),
            "--seed",
            "5",
            name,
        ],
    );
}

#[test]
fn help_lists_every_subcommand() {
    let dir = TempDir::new("help");
    let output = run_ok(&dir, &["--help"]);
    let text = stdout(&output);
    for subcommand in [
        "process",
        "render",
        "eq",
        "levelmatch",
        "kernel",
        "nulltest",
        "bypass-test",
        "gen",
        "bench",
    ] {
        assert!(
            text.contains(subcommand),
            "help should mention {subcommand}:\n{text}"
        );
    }
    // Subcommand help works too.
    let output = run_ok(&dir, &["process", "--help"]);
    assert!(stdout(&output).contains("--trim-latency"));
}

#[test]
fn gen_white_writes_a_valid_wav() {
    let dir = TempDir::new("gen-white");
    run_ok(
        &dir,
        &[
            "gen",
            "white",
            "--rate",
            "48000",
            "--seconds",
            "0.01",
            "--bits",
            "24",
            "white.wav",
        ],
    );
    let mut reader = hound::WavReader::open(dir.path("white.wav")).expect("valid wav");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 48_000);
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.bits_per_sample, 24);
    let samples: Vec<i32> = reader.samples::<i32>().map(Result::unwrap).collect();
    assert_eq!(samples.len(), 480);
    assert!(
        samples.iter().any(|&s| s != 0),
        "noise should not be all zero"
    );
    assert!(samples.iter().all(|&s| s.abs() < (1 << 23)));
}

#[test]
fn gen_sine_has_the_requested_peak() {
    let dir = TempDir::new("gen-sine");
    run_ok(
        &dir,
        &[
            "gen",
            "sine",
            "--rate",
            "48000",
            "--seconds",
            "0.1",
            "--freq",
            "1000",
            "--amp-db",
            "-6",
            "--bits",
            "16",
            "sine.wav",
        ],
    );
    let mut reader = hound::WavReader::open(dir.path("sine.wav")).unwrap();
    let samples: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
    assert_eq!(samples.len(), 4800);
    let peak = samples
        .iter()
        .map(|s| f64::from(*s).abs())
        .fold(0.0, f64::max);
    let expected = 0.5 * 32768.0;
    assert!(
        (peak - expected).abs() < expected * 0.02,
        "peak {peak}, expected {expected}"
    );
}

#[test]
fn nulltest_reports_identical_and_different_files() {
    let dir = TempDir::new("nulltest");
    run_ok(&dir, &["gen", "silence", "--seconds", "0.01", "a.wav"]);
    run_ok(&dir, &["gen", "silence", "--seconds", "0.01", "b.wav"]);
    let output = run_ok(&dir, &["nulltest", "a.wav", "b.wav"]);
    let text = stdout(&output);
    assert!(text.contains("bit_identical: yes"), "{text}");
    assert!(text.contains("first_diff: none"), "{text}");

    run_ok(&dir, &["gen", "sine", "--seconds", "0.01", "c.wav"]);
    let output = run_ok(&dir, &["nulltest", "a.wav", "c.wav"]);
    let text = stdout(&output);
    assert!(text.contains("bit_identical: no"), "{text}");
    assert!(value(&text, "max_abs_diff") > 0.0, "{text}");
}

#[test]
fn bad_arguments_exit_with_code_2() {
    let dir = TempDir::new("bad-args");
    let output = run(&dir, &["process"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "usage errors use clap's exit code"
    );
    let output = run(&dir, &["gen", "sine", "out.wav", "--format", "nope"]);
    assert_eq!(output.status.code(), Some(2));
    let output = run(
        &dir,
        &[
            "kernel", "--rate", "44100", "--ratio", "0", "--taps", "16", "--out", "k",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "ratio 0 must be rejected by clap"
    );
}

/// `process --config` checks a convolution item's shape as the daemon
/// does: a matrix with an `ir_path` beside it, or an entry out of range,
/// is refused with the same message.
#[test]
fn process_refuses_a_malformed_matrix_item() {
    let dir = TempDir::new("matrix-shape");
    run_ok(
        &dir,
        &[
            "gen",
            "white",
            "--rate",
            "44100",
            "--seconds",
            "0.02",
            "--channels",
            "2",
            "in.wav",
        ],
    );
    gen_white(&dir, "ir.wav", 16);
    for (item, want) in [
        (
            r#"{"ir_path":"ir.wav","matrix":[{"in":0,"out":0,"ir_path":"ir.wav"}]}"#,
            "convolution[0] has both ir_path and matrix",
        ),
        (
            r#"{"matrix":[{"in":2,"out":0,"ir_path":"ir.wav"}]}"#,
            "convolution[0].matrix[0]: in 2 / out 0 must be below 2",
        ),
        (
            r#"{"matrix":[]}"#,
            "convolution[0].matrix must have at least one entry",
        ),
    ] {
        dir.write("chain.json", &format!(r#"{{"convolution":[{item}]}}"#));
        let out = run(
            &dir,
            &[
                "process",
                "in.wav",
                "out.wav",
                "--config",
                "chain.json",
                "--format",
                "f32",
            ],
        );
        assert!(!out.status.success(), "{item} was accepted");
        assert!(stderr(&out).contains(want), "{item}: {}", stderr(&out));
    }
}

/// A chain from before the safety modes went: crossfeed `level` and the
/// harmonics `series_makeup` routing.
const LEGACY_CHAIN: &str = r#"{
    "crossfeed": {"enabled": true, "mode": "bs2b_default", "level": "peak_safe"},
    "harmonics": {"enabled": true, "routing": "series_makeup", "layers": [
        {"kind": "chebyshev", "levels_db": [-30, -40, null, null, null, null, null, null],
         "signs": ["+", "-", "+", "+", "+", "+", "+", "+"], "drive_db": 0, "amount": 1},
        {"kind": "chebyshev", "levels_db": [-35, -45, null, null, null, null, null, null],
         "signs": ["+", "-", "+", "+", "+", "+", "+", "+"], "drive_db": 0, "amount": 1}
    ]}
}"#;

/// `process --config` reads a legacy chain itself (no daemon migration in
/// between): `series_makeup` runs as `series`, the old `level` is ignored.
#[test]
fn process_reads_a_legacy_chain() {
    let dir = TempDir::new("legacy-process");
    run_ok(
        &dir,
        &[
            "gen",
            "white",
            "--rate",
            "48000",
            "--seconds",
            "0.05",
            "--channels",
            "2",
            "in.wav",
        ],
    );
    dir.write("chain.json", LEGACY_CHAIN);
    run_ok(
        &dir,
        &[
            "process",
            "in.wav",
            "out.wav",
            "--config",
            "chain.json",
            "--format",
            "f32",
        ],
    );
    assert!(dir.path("out.wav").exists());
}

/// `ctl config set` parses the file before sending it: a legacy config goes
/// out in the current form instead of being refused.
#[test]
fn config_set_sends_a_legacy_config_in_the_current_form() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let dir = TempDir::new("legacy-config-set");
    dir.write(
        "config.json",
        &format!(r#"{{"device": "hw:CARD=USB,DEV=0", "chain": {LEGACY_CHAIN}}}"#),
    );
    let socket = dir.path("ctl.sock");
    let listener = UnixListener::bind(&socket).expect("bind the fake daemon");
    let daemon = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let reply = serde_json::json!({"id": request["id"], "ok": null});
        (&stream)
            .write_all(format!("{reply}\n").as_bytes())
            .unwrap();
        request
    });
    let socket = socket.to_str().unwrap().to_owned();
    let out = run_ok(
        &dir,
        &["ctl", "--socket", &socket, "config", "set", "config.json"],
    );
    assert!(
        stdout(&out).contains("configuration applied"),
        "{}",
        stdout(&out)
    );
    let request = daemon.join().unwrap();
    assert_eq!(request["method"], "set_config");
    let chain = &request["params"]["chain"];
    assert_eq!(chain["harmonics"]["routing"], "series", "{chain}");
    assert!(chain["crossfeed"].get("level").is_none(), "{chain}");
    assert_eq!(chain["crossfeed"]["mode"], "bs2b_default");
}

#[test]
fn process_passthrough_is_bit_exact() {
    let dir = TempDir::new("passthrough");
    gen_white(&dir, "in.wav", 16);
    run_ok(
        &dir,
        &[
            "process", "in.wav", "out.wav", "--format", "s16", "--block", "37",
        ],
    );
    let input = fs::read(dir.path("in.wav")).unwrap();
    let output = fs::read(dir.path("out.wav")).unwrap();
    assert_eq!(
        data_chunk(&input),
        data_chunk(&output),
        "16-bit passthrough must be byte-identical"
    );

    let check = run_ok(&dir, &["bypass-test", "in.wav", "--block", "37"]);
    assert!(stdout(&check).contains("passthrough: bit-exact"));

    // 24-bit too.
    gen_white(&dir, "in24.wav", 24);
    run_ok(
        &dir,
        &[
            "process",
            "in24.wav",
            "out24.wav",
            "--format",
            "s24",
            "--block",
            "1000",
        ],
    );
    let input = fs::read(dir.path("in24.wav")).unwrap();
    let output = fs::read(dir.path("out24.wav")).unwrap();
    assert_eq!(
        data_chunk(&input),
        data_chunk(&output),
        "24-bit passthrough must be byte-identical"
    );
}

/// The reported latency counts the upsampler's convolution block the live
/// engine runs at the file's rate: 512 frames at 44.1 kHz with the default
/// quantum, 1024 with quantum 2048, whatever `--block` the calls use.
#[test]
fn process_latency_follows_the_engine_block() {
    let dir = TempDir::new("latency");
    gen_white(&dir, "in.wav", 24);
    let latency = |quantum: &str, block: &str| {
        let output = run_ok(
            &dir,
            &[
                "process",
                "in.wav",
                "out.wav",
                "--upsample",
                "2",
                "--taps",
                "4096",
                "--quantum",
                quantum,
                "--block",
                block,
            ],
        );
        value(&stdout(&output), "latency")
    };
    let default = latency("1024", "1024");
    assert_eq!(default, latency("1024", "100"));
    assert_eq!(latency("2048", "1024") - default, 512.0 * 2.0);
}

#[test]
fn render_plays_at_another_speed() {
    let dir = TempDir::new("render");
    run_ok(
        &dir,
        &[
            "gen",
            "sine",
            "--rate",
            "48000",
            "--seconds",
            "0.5",
            "--channels",
            "2",
            "--freq",
            "1000",
            "in.wav",
        ],
    );
    let frames = |name: &str| {
        let bytes = fs::read(dir.path(name)).unwrap();
        data_chunk(&bytes).len() / 8 // f32 stereo
    };
    for (speed, want) in [("2", 12_000usize), ("0.5", 48_000), ("1.0123", 23_708)] {
        let out = run_ok(
            &dir,
            &[
                "render",
                "in.wav",
                "out.wav",
                "--speed",
                speed,
                "--trim-latency",
            ],
        );
        let peak = value(&stdout(&out), "peak");
        assert!((peak + 6.0).abs() < 0.05, "peak {peak} dBFS");
        let got = frames("out.wav");
        assert!(
            got.abs_diff(want) <= 4,
            "speed {speed}: {got} frames, want about {want}"
        );
    }
    let out = run_ok(
        &dir,
        &[
            "render",
            "in.wav",
            "out.wav",
            "--change",
            "0.1:1.5:100",
            "--change",
            "0.2:0.8",
            "--block",
            "1",
        ],
    );
    assert!(stdout(&out).contains("0.8 at the end"), "{}", stdout(&out));
    let bad = run(&dir, &["render", "in.wav", "out.wav", "--speed", "3"]);
    assert_eq!(bad.status.code(), Some(3), "{}", stderr(&bad));
    let bad = run(&dir, &["render", "in.wav", "out.wav", "--change", "1:9"]);
    assert_eq!(bad.status.code(), Some(3), "{}", stderr(&bad));
}

#[test]
fn process_upsamples_by_an_integer_ratio() {
    let dir = TempDir::new("upsample");
    gen_white(&dir, "in.wav", 16);
    let output = run_ok(
        &dir,
        &[
            "process",
            "in.wav",
            "out.wav",
            "--upsample",
            "4",
            "--taps",
            "1024",
            "--format",
            "f32",
            "--dither",
            "none",
            "--block",
            "333",
        ],
    );
    let text = stdout(&output);
    assert!(text.contains("176400 Hz"), "{text}");
    let mut reader = hound::WavReader::open(dir.path("out.wav")).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 176_400);
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);
    let frames = reader.samples::<f32>().count();
    assert_eq!(frames, (0.02f64 * 44_100.0).round() as usize * 4);
}

#[test]
fn process_convolution_changes_the_signal() {
    let dir = TempDir::new("conv");
    gen_white(&dir, "in.wav", 24);
    // Build a simple IR with `gen sine` and use it directly.
    run_ok(
        &dir,
        &[
            "gen",
            "sine",
            "--rate",
            "44100",
            "--seconds",
            "0.005",
            "--freq",
            "2000",
            "--bits",
            "24",
            "ir.wav",
        ],
    );
    let output = run_ok(
        &dir,
        &[
            "process",
            "in.wav",
            "out.wav",
            "--conv",
            "ir.wav",
            "--format",
            "f32",
            "--dither",
            "none",
            "--trim-latency",
        ],
    );
    assert!(stdout(&output).contains("latency"), "should report latency");
    let mut reader = hound::WavReader::open(dir.path("out.wav")).unwrap();
    let samples: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
    assert_eq!(samples.len(), (0.02f64 * 44_100.0).round() as usize);
    assert!(
        samples.iter().any(|&s| s.abs() > 1e-6),
        "convolved output should not be silent"
    );

    // Chaining multiple `--conv` flags in `avdsp process`.
    run_ok(
        &dir,
        &[
            "process",
            "in.wav",
            "out_chain.wav",
            "--conv",
            "ir.wav",
            "--conv",
            "ir.wav",
            "--format",
            "f32",
            "--dither",
            "none",
            "--trim-latency",
        ],
    );
    let mut reader2 = hound::WavReader::open(dir.path("out_chain.wav")).unwrap();
    let samples2: Vec<f32> = reader2.samples::<f32>().map(Result::unwrap).collect();
    assert_eq!(samples2.len(), samples.len());
    assert!(samples2.iter().any(|&s| s.abs() > 1e-6));
}

#[test]
fn eq_import_export_and_response() {
    let dir = TempDir::new("eq");
    let apo = dir.write(
        "profile.txt",
        "Preamp: -4.0 dB\n\
         Filter 1: ON PK Fc 100 Hz Gain 3.0 dB Q 0.7\n\
         Filter 2: OFF HS Fc 8000 Hz Gain -2.0 dB Q 0.7071\n",
    );
    run_ok(
        &dir,
        &[
            "eq",
            "import",
            apo.to_str().unwrap(),
            "--json",
            "profile.json",
        ],
    );
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path("profile.json")).unwrap()).unwrap();
    assert_eq!(json["preamp_db"], -4.0);
    assert_eq!(json["bands"].as_array().unwrap().len(), 2);
    assert_eq!(json["bands"][0]["kind"], "peaking");
    assert_eq!(json["bands"][1]["enabled"], false);

    let output = run_ok(&dir, &["eq", "export", "profile.json"]);
    let text = stdout(&output);
    // Formatting details are up to eq_apo; check the structure and values.
    assert!(text.contains("Preamp:"), "{text}");
    assert!(
        text.lines()
            .any(|l| l.starts_with("Preamp:") && l.split_whitespace().nth(1) == Some("-4")),
        "{text}"
    );
    let first = text
        .lines()
        .find(|line| line.contains("Filter 1:"))
        .unwrap();
    assert!(first.contains("ON PK"), "{first}");
    assert!(first.contains("100"), "{first}");
    assert!(
        first.contains("Gain 3 dB") || first.contains("Gain 3.0 dB"),
        "{first}"
    );
    assert!(first.contains("0.7"), "{first}");
    assert!(
        text.lines()
            .any(|line| line.contains("Filter 2:") && line.contains("OFF HS")),
        "{text}"
    );

    let output = run_ok(
        &dir,
        &[
            "eq",
            "response",
            "profile.json",
            "--rate",
            "44100",
            "--points",
            "33",
            "--json",
        ],
    );
    let json: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(json["rate"], 44_100);
    assert_eq!(json["points"].as_array().unwrap().len(), 33);
    let first = &json["points"][0];
    assert!((first["freq_hz"].as_f64().unwrap() - 20.0).abs() < 1e-9);

    // Explicit frequencies, text/CSV mode.
    let output = run_ok(
        &dir,
        &[
            "eq",
            "response",
            apo.to_str().unwrap(),
            "--rate",
            "48000",
            "--freqs",
            "100,1000",
        ],
    );
    let text = stdout(&output);
    let rows: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].starts_with("100.000000000,"));
    assert!(rows[1].starts_with("1000.000000000,"));

    // Split L/R profile export/response with `--channel right`.
    let split_path = dir.write(
        "split.json",
        &serde_json::to_string_pretty(&serde_json::json!({
            "name": "split",
            "preamp_db": -4.0,
            "bands": [],
            "right": {
                "preamp_db": -2.5,
                "bands": [
                    { "kind": "peaking", "freq_hz": 500.0, "gain_db": 4.0, "q": 1.0, "enabled": true }
                ]
            }
        }))
        .unwrap(),
    );
    let exp_r = stdout(&run_ok(
        &dir,
        &[
            "eq",
            "export",
            split_path.to_str().unwrap(),
            "--channel",
            "right",
        ],
    ));
    assert!(exp_r.contains("Preamp: -2.5 dB"), "{exp_r}");
    assert!(exp_r.contains("500"), "{exp_r}");

    let resp_r = stdout(&run_ok(
        &dir,
        &[
            "eq",
            "response",
            split_path.to_str().unwrap(),
            "--rate",
            "48000",
            "--freqs",
            "500",
            "--channel",
            "right",
        ],
    ));
    let row = resp_r.lines().nth(1).unwrap();
    let mag_db: f64 = row.split(',').nth(1).unwrap().parse().unwrap();
    // -2.5 preamp + 4.0 dB peaking @ 500 Hz ≈ +1.5 dB.
    assert!(
        (mag_db - 1.5).abs() < 0.05,
        "expected +1.5 dB, got {mag_db}"
    );
}

#[test]
fn kernel_dumps_the_requested_number_of_taps() {
    let dir = TempDir::new("kernel");
    let output = run_ok(
        &dir,
        &[
            "kernel",
            "--rate",
            "44100",
            "--ratio",
            "2",
            "--taps",
            "64",
            "--phase",
            "linear",
            "--beta",
            "14",
            "--cutoff",
            "1.0",
            "--out",
            "kernel.f64",
            "--json",
        ],
    );
    let json: serde_json::Value = serde_json::from_str(stdout(&output).trim()).unwrap();
    assert_eq!(json["kernel_len"], 64);
    assert_eq!(json["ratio"], 2);
    let bytes = fs::read(dir.path("kernel.f64")).unwrap();
    assert_eq!(bytes.len(), 64 * 8);
    let sum: f64 = bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| f64::from_le_bytes(*c))
        .sum();
    // The kernel documents a passband gain of `ratio`; allow for windowing
    // details while catching a gross normalization mistake.
    assert!(
        (sum - 2.0).abs() < 0.02,
        "kernel DC gain {sum} should be close to the ratio"
    );
}

#[test]
fn levelmatch_is_analytic_and_measured() {
    let dir = TempDir::new("levelmatch");
    let flat = dir.write("flat.txt", "Preamp: 0 dB\n");
    let boost = dir.write("boost.txt", "Preamp: 6 dB\n");

    let output = run_ok(
        &dir,
        &[
            "levelmatch",
            flat.to_str().unwrap(),
            boost.to_str().unwrap(),
            "--rate",
            "44100",
        ],
    );
    let offset = value(&stdout(&output), "offset_db");
    assert!((offset + 6.0).abs() < 0.05, "analytic offset {offset}");

    run_ok(
        &dir,
        &[
            "gen",
            "white",
            "--rate",
            "44100",
            "--seconds",
            "0.8",
            "--bits",
            "24",
            "music.wav",
        ],
    );
    let output = run_ok(
        &dir,
        &[
            "levelmatch",
            flat.to_str().unwrap(),
            boost.to_str().unwrap(),
            "--measure",
            "music.wav",
        ],
    );
    let text = stdout(&output);
    let measured = value(&text, "offset_db");
    assert!(
        (measured + 6.0).abs() < 0.2,
        "measured offset {measured}\n{text}"
    );
    assert!(text.contains("a_lufs:"), "{text}");
}

#[test]
fn bench_reports_a_real_time_factor() {
    let dir = TempDir::new("bench");
    let output = run_ok(
        &dir,
        &[
            "bench",
            "--rate",
            "44100",
            "--ratio",
            "2",
            "--taps",
            "512",
            "--seconds",
            "0.05",
            "--block",
            "256",
        ],
    );
    let text = stdout(&output);
    let rtf = value(&text, "real-time factor");
    assert!(rtf > 0.0, "{text}");
}

/// The binary must not need any arguments beyond a path that exists.
#[test]
fn nulltest_reports_missing_files_as_runtime_errors() {
    let dir = TempDir::new("missing");
    let output = run(&dir, &["nulltest", "nope.wav", "nope2.wav"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("cannot open WAV"));
}

#[allow(dead_code)]
fn assert_path(_: &Path) {}
