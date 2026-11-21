//! Unattended test rig exercising `audivane_dsp_daemon::engine::RealEngine`.
//!
//! Usage:
//!   engine_rig <config.json> [--interval-ms 1000]
//!
//! Prints `snapshot()` as compact JSON lines (`{"type":"snapshot",...}`) at
//! `--interval-ms` (default 1000 ms), and accepts line-delimited commands on
//! stdin:
//!   apply <json>
//!   ab <a|b> [crossfade_ms]
//!   selftest
//!   meters
//!   snapshot
//!   release
//!   acquire
//!   varispeed <on|off> [speed] [ramp_ms]
//!   varispeed_status
//!   quit

use anyhow::{Context, Result};
use audivane_dsp_daemon::api::{Engine, EngineSnapshot};
use audivane_dsp_daemon::engine::RealEngine;
use audivane_io_pw::DriverGapCounters;
use audivane_protocol::SampleFormat;
use audivane_protocol::ipc::{AbSide, AbStatus, EngineConfig, EngineState, SinkChoice};
use serde::Serialize;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Serialize)]
struct SnapshotLine {
    #[serde(rename = "type")]
    kind: &'static str,
    state: EngineState,
    device: String,
    error: Option<String>,
    format: Option<SampleFormat>,
    graph_rate: u32,
    output_rate: u32,
    ratio: audivane_protocol::RateRatio,
    quantum: u32,
    latency_ms: f64,
    xruns: u64,
    dsp_load: f64,
    dsp_active: bool,
    active_sink: Option<SinkChoice>,
    ab: Option<AbStatus>,
    dsp_node: Option<u32>,
    direct_node: Option<u32>,
    frames_out: u64,
    chain_generation: u64,
    gaps: DriverGapCounters,
}

impl From<EngineSnapshot> for SnapshotLine {
    fn from(s: EngineSnapshot) -> Self {
        Self {
            kind: "snapshot",
            state: s.state,
            device: s.device,
            error: s.error,
            format: s.format,
            graph_rate: s.graph_rate,
            output_rate: s.output_rate,
            ratio: s.ratio,
            quantum: s.quantum,
            latency_ms: s.latency_ms,
            xruns: s.xruns,
            dsp_load: s.dsp_load,
            dsp_active: s.dsp_active,
            active_sink: s.active_sink,
            ab: s.ab,
            dsp_node: s.dsp_node,
            direct_node: s.direct_node,
            frames_out: s.frames_out,
            chain_generation: s.chain_generation,
            gaps: s.gaps,
        }
    }
}

fn emit_json(out_lock: &Mutex<io::Stdout>, val: &impl Serialize) {
    if let Ok(line) = serde_json::to_string(val)
        && let Ok(mut out) = out_lock.lock()
    {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

fn parse_args() -> Result<(PathBuf, u64)> {
    let mut cfg_path: Option<PathBuf> = None;
    let mut interval_ms: u64 = 1000;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--interval-ms" {
            let val = args.next().context("--interval-ms requires a value")?;
            interval_ms = val.parse().context("invalid --interval-ms")?;
        } else if arg.starts_with('-') {
            anyhow::bail!("unknown option: {arg}");
        } else if cfg_path.is_none() {
            cfg_path = Some(PathBuf::from(arg));
        } else {
            anyhow::bail!("unexpected extra argument: {arg}");
        }
    }
    let path = cfg_path.context("usage: engine_rig <config.json> [--interval-ms N]")?;
    Ok((path, interval_ms.max(50)))
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (cfg_path, interval_ms) = parse_args()?;
    let raw_cfg = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("failed to read {}", cfg_path.display()))?;
    let cfg: EngineConfig = serde_json::from_str(&raw_cfg)
        .with_context(|| format!("failed to parse {}", cfg_path.display()))?;

    let engine =
        RealEngine::start(&cfg).map_err(|e| anyhow::anyhow!("engine start failed: {e}"))?;
    let out_lock = Arc::new(Mutex::new(io::stdout()));
    let stop_flag = Arc::new(AtomicBool::new(false));

    // Emit an immediate initial snapshot so callers see READY + node IDs right away.
    emit_json(&out_lock, &SnapshotLine::from(engine.snapshot()));

    let engine_in = Arc::clone(&engine);
    let out_in = Arc::clone(&out_lock);
    let stop_in = Arc::clone(&stop_flag);
    thread::Builder::new()
        .name("engine-rig-stdin".to_string())
        .spawn(move || {
            let stdin = io::stdin();
            let mut reader = io::BufReader::new(stdin.lock());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: keep running until external signal
                    Ok(_) => {}
                    Err(_) => break,
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "quit" {
                    emit_json(
                        &out_in,
                        &serde_json::json!({"type": "reply", "cmd": "quit", "ok": true}),
                    );
                    stop_in.store(true, Ordering::Release);
                    break;
                } else if trimmed == "snapshot" {
                    emit_json(&out_in, &SnapshotLine::from(engine_in.snapshot()));
                } else if trimmed == "varispeed_status" {
                    let st = engine_in.varispeed_status();
                    emit_json(
                        &out_in,
                        &serde_json::json!({"type": "reply", "cmd": "varispeed_status", "ok": true,
                            "status": format!("{st:?}")}),
                    );
                } else if let Some(rest) = trimmed.strip_prefix("varispeed ") {
                    let mut parts = rest.split_whitespace();
                    let on = parts.next() == Some("on");
                    let speed: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
                    let ramp: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    let keep_pitch = parts.next() == Some("keep_pitch");
                    match engine_in.set_varispeed(audivane_protocol::ipc::VarispeedConfig {
                        enabled: on,
                        speed,
                        ramp_ms: ramp,
                        keep_pitch,
                    }) {
                        Ok(()) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "varispeed", "ok": true}),
                        ),
                        Err(e) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "varispeed", "ok": false, "error": e.to_string()}),
                        ),
                    }
                } else if trimmed == "meters" {
                    let m = engine_in.meters();
                    emit_json(
                        &out_in,
                        &serde_json::json!({"type": "reply", "cmd": "meters", "ok": true, "meters": m}),
                    );
                } else if trimmed == "selftest" {
                    match engine_in.selftest() {
                        Ok(res) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "selftest", "ok": true, "selftest": res}),
                        ),
                        Err(e) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "selftest", "ok": false, "error": e.to_string()}),
                        ),
                    }
                } else if trimmed == "release" {
                    match engine_in.release_device() {
                        Ok(()) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "release", "ok": true}),
                        ),
                        Err(e) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "release", "ok": false, "error": e.to_string()}),
                        ),
                    }
                } else if trimmed == "acquire" {
                    match engine_in.acquire_device() {
                        Ok(()) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "acquire", "ok": true}),
                        ),
                        Err(e) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "acquire", "ok": false, "error": e.to_string()}),
                        ),
                    }
                } else if let Some(rest) = trimmed.strip_prefix("apply ") {
                    match serde_json::from_str::<EngineConfig>(rest.trim()) {
                        Ok(new_cfg) => match engine_in.apply(&new_cfg) {
                            Ok(()) => emit_json(
                                &out_in,
                                &serde_json::json!({"type": "reply", "cmd": "apply", "ok": true}),
                            ),
                            Err(e) => emit_json(
                                &out_in,
                                &serde_json::json!({"type": "reply", "cmd": "apply", "ok": false, "error": e.to_string()}),
                            ),
                        },
                        Err(e) => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "apply", "ok": false, "error": format!("bad json: {e}")}),
                        ),
                    }
                } else if let Some(rest) = trimmed.strip_prefix("ab ") {
                    let mut parts = rest.split_whitespace();
                    let side_str = parts.next().unwrap_or("");
                    let fade_ms: f64 = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(10.0);
                    let side = match side_str {
                        "a" | "A" => Some(AbSide::A),
                        "b" | "B" => Some(AbSide::B),
                        _ => None,
                    };
                    match side {
                        Some(s) => match engine_in.ab_select(s, fade_ms) {
                            Ok(ab) => emit_json(
                                &out_in,
                                &serde_json::json!({"type": "reply", "cmd": "ab", "ok": true, "ab": ab}),
                            ),
                            Err(e) => emit_json(
                                &out_in,
                                &serde_json::json!({"type": "reply", "cmd": "ab", "ok": false, "error": e.to_string()}),
                            ),
                        },
                        None => emit_json(
                            &out_in,
                            &serde_json::json!({"type": "reply", "cmd": "ab", "ok": false, "error": format!("invalid side '{side_str}'")}),
                        ),
                    }
                } else {
                    emit_json(
                        &out_in,
                        &serde_json::json!({"type": "reply", "cmd": "unknown", "ok": false, "error": format!("unknown command: {trimmed}")}),
                    );
                }
            }
        })?;

    while !stop_flag.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(interval_ms));
        if stop_flag.load(Ordering::Acquire) {
            break;
        }
        emit_json(&out_lock, &SnapshotLine::from(engine.snapshot()));
    }

    engine.shutdown();
    Ok(())
}
