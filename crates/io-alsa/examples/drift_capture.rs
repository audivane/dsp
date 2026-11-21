//! Position readings for the drift loop: play digital silence on one or more
//! ALSA `hw:` devices at once, each on its own thread woken by its own
//! period, and log one reading per period.
//!
//! ```text
//! cargo run -p audivane-io-alsa --example drift_capture -- \
//!     --rate 48000 --period 1024 --periods 3 --seconds 600 --out DIR \
//!     hw:CARD=USB,DEV=0 hw:CARD=Loopback,DEV=0
//! ```
//!
//! Writes `DIR/<n>.log` per device (n = its position on the command line),
//! one line per wakeup:
//!
//! ```text
//! written delay htstamp_ns before_ns after_ns avail xruns
//! ```
//!
//! `written` is the frames given before this wakeup's write, `delay` and
//! `htstamp_ns` come from one `snd_pcm_status` call (`CLOCK_MONOTONIC_RAW`),
//! `before_ns` and `after_ns` are the same clock read around that call.
//!
//! `--dense` instead polls the status every `--poll-us` (default 50 µs)
//! and logs a line each time `written − delay` changes, to show how the
//! pointer moves (its granularity). Silence only: nothing above zero is
//! ever written. Each device is reserved (`ReserveDevice1`) for the run,
//! so a session server lets it go and takes it back after.

use audivane_io_alsa::{AlsaConfig, AlsaOutput, DeviceReservation, try_promote_current_thread_rt};
use std::io::Write;
use std::sync::{Arc, Barrier};
use std::time::Duration;

struct Args {
    rate: u32,
    period: u32,
    periods: u32,
    seconds: f64,
    out: std::path::PathBuf,
    dense: bool,
    poll_us: u64,
    devices: Vec<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        rate: 48_000,
        period: 1024,
        periods: 3,
        seconds: 60.0,
        out: ".".into(),
        dense: false,
        poll_us: 50,
        devices: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--rate" => a.rate = val()?.parse().map_err(|e| format!("--rate: {e}"))?,
            "--period" => a.period = val()?.parse().map_err(|e| format!("--period: {e}"))?,
            "--periods" => a.periods = val()?.parse().map_err(|e| format!("--periods: {e}"))?,
            "--seconds" => a.seconds = val()?.parse().map_err(|e| format!("--seconds: {e}"))?,
            "--out" => a.out = val()?.into(),
            "--poll-us" => a.poll_us = val()?.parse().map_err(|e| format!("--poll-us: {e}"))?,
            "--dense" => a.dense = true,
            d if !d.starts_with("--") => a.devices.push(d.to_string()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    if a.devices.is_empty() {
        return Err("no device given".into());
    }
    Ok(a)
}

fn now_raw_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

fn capture(a: &Args, index: usize, barrier: &Barrier) -> Result<String, String> {
    // Every thread reaches the barrier once, also when its setup fails, so
    // the others never wait on it forever.
    let mut waited = false;
    let r = run(a, index, barrier, &mut waited);
    if !waited {
        barrier.wait();
    }
    r
}

fn run(a: &Args, index: usize, barrier: &Barrier, waited: &mut bool) -> Result<String, String> {
    let device = &a.devices[index];
    let _reservation = DeviceReservation::acquire_for_device(device, i32::MAX)
        .map_err(|e| format!("{device}: reserve: {e}"))?;
    let cfg = AlsaConfig {
        device: device.clone(),
        rate: a.rate,
        channels: 2,
        period_frames: a.period,
        buffer_periods: a.periods,
        preferred_format: None,
        strict_format: false,
        nonblock: true,
    };
    let mut out = AlsaOutput::open(&cfg).map_err(|e| format!("{device}: open: {e}"))?;
    let p = out.params();
    if p.rate != a.rate {
        return Err(format!("{device}: opened at {} Hz", p.rate));
    }
    let period = p.period_frames;
    let periods = (p.buffer_frames / period).max(2);
    let rt = try_promote_current_thread_rt(80);
    let path = a.out.join(format!("{index}.log"));
    let mut log = std::io::BufWriter::new(
        std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?,
    );
    let _ = writeln!(
        log,
        "# device {index} rate {} period {period} buffer {} format {:?} rt {:?} dense {}",
        p.rate, p.buffer_frames, p.format, rt, a.dense
    );
    let mut written: u64 = 0;
    for _ in 0..periods {
        written += out
            .write_silence(period)
            .map_err(|e| format!("{device}: prefill: {e}"))? as u64;
    }
    barrier.wait();
    *waited = true;
    out.start().map_err(|e| format!("{device}: start: {e}"))?;
    let start = now_raw_ns();
    let end = start + (a.seconds * 1e9) as i64;
    let mut last_played = i64::MIN;
    let mut lines = 0u64;
    while now_raw_ns() < end {
        if a.dense {
            std::thread::sleep(Duration::from_micros(a.poll_us));
        } else {
            out.wait_for_period(1000)
                .map_err(|e| format!("{device}: wait: {e}"))?;
        }
        if out.needs_restart() {
            return Err(format!("{device}: xrun after {lines} readings"));
        }
        let before = now_raw_ns();
        let (ts, delay) = out
            .status_timestamp_ns()
            .map_err(|e| format!("{device}: status: {e}"))?;
        let after = now_raw_ns();
        let avail = out.avail().map_err(|e| format!("{device}: avail: {e}"))?;
        let played = written as i64 - delay;
        if !a.dense || played != last_played {
            let _ = writeln!(
                log,
                "{written} {delay} {ts} {before} {after} {avail} {}",
                out.xruns()
            );
            lines += 1;
            last_played = played;
        }
        if avail >= period {
            let n = out
                .write_silence(period)
                .map_err(|e| format!("{device}: write: {e}"))?;
            written += n as u64;
        }
    }
    let _ = out.drop_frames();
    let _ = log.flush();
    Ok(format!(
        "{device}: {lines} lines, {} xruns, rt {rt:?} -> {}",
        out.xruns(),
        path.display()
    ))
}

fn main() {
    let a = match parse() {
        Ok(a) => Arc::new(a),
        Err(e) => {
            eprintln!("drift_capture: {e}");
            std::process::exit(2);
        }
    };
    let barrier = Arc::new(Barrier::new(a.devices.len()));
    let handles: Vec<_> = (0..a.devices.len())
        .map(|i| {
            let (a, b) = (Arc::clone(&a), Arc::clone(&barrier));
            std::thread::spawn(move || capture(&a, i, &b))
        })
        .collect();
    let mut failed = false;
    for h in handles {
        match h.join() {
            Ok(Ok(msg)) => println!("{msg}"),
            Ok(Err(e)) => {
                eprintln!("drift_capture: {e}");
                failed = true;
            }
            Err(_) => {
                eprintln!("drift_capture: a capture thread panicked");
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
}
