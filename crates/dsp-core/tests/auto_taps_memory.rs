//! The automatic-taps search must not leave its FFT plans behind in the
//! calling thread (the engine and IPC threads live for the whole process).
//! Own test binary: it reads this process's resident set size.
#![cfg(target_os = "linux")]

use audivane_dsp_core::filter_design::automatic_design_uncached;
use audivane_dsp_core::resample::ManualConfig;
use audivane_protocol::{BandSpec, FilterErrorBudget, FilterPhase, RateTarget};

fn rss_kib() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[test]
fn search_releases_its_fft_plans() {
    let cfg = ManualConfig {
        target: RateTarget::Ratio(16),
        phase: FilterPhase::Minimum,
        auto_taps: Some(FilterErrorBudget::Db(98.0)),
        band: BandSpec {
            passband_end: 0.99,
            stopband_start: 1.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let before = rss_kib();
    let d = automatic_design_uncached(&cfg, 16).unwrap();
    let after = rss_kib();
    eprintln!("{} taps: RSS {before} -> {after} KiB", d.taps);
    // The plans for this length (about 21 000 taps, 1M-point FFT) hold
    // about 13.5 MB; a search-local planner leaves about 1.4 MB.
    assert!(
        after < before + 6 * 1024,
        "RSS grew {before} -> {after} KiB"
    );
}
