//! Times the automatic-taps search (uncached) over hard configurations.
//!
//! `cargo run --release -p audivane-dsp-core --example auto_taps_timing`
//! Optional args: `<ratio> <passband_end> <stopband_start> <db>` for one row.
use audivane_dsp_core::filter_design::{automatic_design_uncached, automatic_estimate};
use audivane_dsp_core::resample::ManualConfig;
use audivane_protocol::{BandSpec, FilterErrorBudget, FilterPhase, RateTarget, WindowKind};
use std::time::Instant;

fn row(ratio: u32, window: WindowKind, phase: FilterPhase, pe: f64, se: f64, a: f64) {
    let cfg = ManualConfig {
        target: RateTarget::Ratio(ratio),
        window,
        phase,
        phase_mix: 0.5,
        band: BandSpec {
            passband_end: pe,
            stopband_start: se,
            ..Default::default()
        },
        auto_taps: Some(FilterErrorBudget::Db(a)),
        ..Default::default()
    };
    let t = Instant::now();
    let estimate = automatic_estimate(&cfg, ratio as usize);
    let est_time = t.elapsed();
    let t = Instant::now();
    let result = automatic_design_uncached(&cfg, ratio as usize);
    let secs = t.elapsed().as_secs_f64();
    let est = match estimate {
        Ok(n) => n.to_string(),
        Err(_) => "rejected".into(),
    };
    let out = match result {
        Ok(d) => format!("{} taps", d.taps),
        Err(e) => format!("error: {}", e.chars().take(70).collect::<String>()),
    };
    println!(
        "| x{ratio} | {window:?} | {phase:?} | [{pe}, {se}] | {a} | {est} ({:.1} us) | {out} | {secs:.3} s |",
        est_time.as_secs_f64() * 1e6
    );
}

fn main() {
    let args: Vec<f64> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    println!(
        "| Ratio | Window | Phase | Band | Target dB | Estimate (time) | Result | Search time |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    let windows = [WindowKind::Kaiser, WindowKind::Gaussian];
    let phases = [
        FilterPhase::Linear,
        FilterPhase::Minimum,
        FilterPhase::Intermediate,
    ];
    if args.len() == 4 {
        for w in windows {
            for p in phases {
                row(args[0] as u32, w, p, args[1], args[2], args[3]);
            }
        }
        return;
    }
    for (pe, se) in [
        (0.9, 1.0),
        (0.95, 1.0),
        (0.99, 1.0),
        (0.999, 1.0),
        (0.5, 1.5),
    ] {
        for a in [98.0, 141.47, 146.24, 200.0] {
            for w in windows {
                for p in phases {
                    row(32, w, p, pe, se, a);
                }
            }
        }
    }
}
