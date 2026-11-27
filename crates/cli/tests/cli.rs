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

#[allow(dead_code)]
fn assert_path(_: &Path) {}
