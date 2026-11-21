//! The drift loop on real position readings, from `io-alsa`'s
//! `drift_capture` example. Ignored: it needs logs, `DRIFT_LOGS=dir` with
//! `<n>.log` per device; `DRIFT_PAIR=m,f` picks the
//! master and the follower (default `0,1`). Each replay runs with the loop's
//! outlier gate and without it; `DRIFT_GATE=ms` sets the gate,
//! `DRIFT_SKIP=ms` and `DRIFT_ACQSKIP=ms` the locked and acquiring skip
//! thresholds, `DRIFT_DEGREE` the truth fits' degree.

use audivane_dsp_core::drift::readings::{Log, ReplayOptions, TimeSource, device_stats, replay};

fn load(path: &std::path::Path) -> Log {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Log::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
#[ignore]
fn drift_readings() {
    let dir = std::path::PathBuf::from(std::env::var("DRIFT_LOGS").expect("DRIFT_LOGS"));
    let pair = std::env::var("DRIFT_PAIR").unwrap_or_else(|_| "0,1".into());
    let (m, f) = pair.split_once(',').expect("DRIFT_PAIR=master,follower");
    let logs = [
        load(&dir.join(format!("{m}.log"))),
        load(&dir.join(format!("{f}.log"))),
    ];
    let degree = std::env::var("DRIFT_DEGREE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    for (i, log) in logs.iter().enumerate() {
        for src in [TimeSource::Htstamp, TimeSource::Before] {
            let s = device_stats(log, src, degree, 20);
            println!(
                "dev {i} {src:?}: {} readings, rate {} period {} buffer {}, {:+.3} ppm, noise rms {:.2} (diff {:.2}) peak {:.1} frames, steps {:?}, late us {:.0}/{:.0}/{:.0}",
                s.readings,
                log.rate,
                log.period,
                log.buffer,
                s.ppm,
                s.noise_rms,
                s.noise_diff,
                s.noise_peak,
                s.steps,
                s.late_us.0,
                s.late_us.1,
                s.late_us.2
            );
        }
    }
    if logs[0].dense || logs[1].dense || logs[0].rate != logs[1].rate {
        return;
    }
    let span = (logs[1].lines.last().unwrap().before_ns - logs[1].lines[0].before_ns) as f64 * 1e-9;
    let steady = (span * 0.4).min(120.0);
    let gate_ms: f64 = std::env::var("DRIFT_GATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.5);
    let gates = [0.0, gate_ms];
    for src in [TimeSource::Htstamp, TimeSource::Before] {
        for &gate in &gates {
            let mut opt = ReplayOptions {
                src,
                steady_from_s: steady,
                degree,
                ..ReplayOptions::default()
            };
            opt.cfg.gate_s = gate * 1e-3;
            if let Some(v) = std::env::var("DRIFT_SKIP")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
            {
                opt.cfg.skip_s = v * 1e-3;
            }
            if let Some(v) = std::env::var("DRIFT_ACQSKIP")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
            {
                opt.cfg.acquire_skip_s = v * 1e-3;
            }
            let o = replay(&logs[0], &logs[1], &opt).expect("replay");
            println!(
                "replay {src:?} gate {gate} ms: sync {:.1} s (flag {:.1}), steady peak {:.3} rms {:.3}, reading bias {:+.2} detector noise {:.2}, sb {:.1} dBc @ {:.1} Hz, pair {:+.3} ppm, eps err {:+.3} ppm, skips {}, unlocks {}",
                o.lock_s,
                o.lock_flag_s,
                o.steady_peak,
                o.steady_rms,
                o.reading_bias,
                o.detector_noise,
                o.sideband_dbc,
                o.sideband_hz,
                o.pair_ppm,
                o.eps_error_ppm,
                o.skips,
                o.unlocks
            );
        }
    }
}
