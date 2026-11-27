//! Per-sample CPU cost of `Harmonics` (Chebyshev and tape layers) at 48 kHz and
//! 768 kHz (stereo), reporting ns/sample/channel and share of one core at
//! 768 kHz stereo.
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example bench_hysteresis
//! ```

use audivane_dsp_core::Stage;
use audivane_dsp_core::harmonics::Harmonics;
use audivane_protocol::{
    ChebyshevLayer, HARMONICS_DEFAULT_SIGNS, HarmonicsConfig, HarmonicsLayer, HysteresisLayer,
};
use std::hint::black_box;
use std::time::Instant;

const BLOCK: usize = 1024;
const CHANNELS: usize = 2;
const RUNS: usize = 5;

/// Pink-like music-level stereo test block (-14 dBFS RMS, peaks near -2 dBFS)
/// plus a loud full-scale sine block to measure both typical music (mostly
/// Taylor branch) and heavy full-scale drive (`exp` branch).
fn make_music_block(rate: f64) -> [Vec<f64>; CHANNELS] {
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = [vec![0.0; BLOCK], vec![0.0; BLOCK]];
    for (c, chan) in out.iter_mut().enumerate() {
        let (mut b0, mut b1, mut b2) = (0.0, 0.0, 0.0);
        let f_tone = 120.0 + 70.0 * c as f64;
        for (i, slot) in chan.iter_mut().enumerate() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let w = (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            b0 = 0.99765 * b0 + w * 0.099_046_0;
            b1 = 0.96300 * b1 + w * 0.296_516_4;
            b2 = 0.57000 * b2 + w * 1.052_691_3;
            let pink = (b0 + b1 + b2 + w * 0.1848) * 0.06;
            let tone = 0.25 * (2.0 * std::f64::consts::PI * f_tone * i as f64 / rate).sin();
            *slot = (pink + tone).clamp(-0.95, 0.95);
        }
    }
    out
}

fn make_full_scale_sine_block(rate: f64) -> [Vec<f64>; CHANNELS] {
    let mut out = [vec![0.0; BLOCK], vec![0.0; BLOCK]];
    for (c, chan) in out.iter_mut().enumerate() {
        let f = if c == 0 { 1_000.0 } else { 1_317.0 };
        for (i, slot) in chan.iter_mut().enumerate() {
            *slot = (2.0 * std::f64::consts::PI * f * i as f64 / rate).sin();
        }
    }
    out
}

/// Measure best (minimum) ns per sample per channel across `RUNS` repetitions.
fn bench_stage(stage: &mut dyn Stage, src: &[Vec<f64>; CHANNELS], blocks_per_run: usize) -> f64 {
    let mut l = vec![0.0; BLOCK];
    let mut r = vec![0.0; BLOCK];
    // Warm up caches and branch predictors.
    for _ in 0..64 {
        l.copy_from_slice(&src[0]);
        r.copy_from_slice(&src[1]);
        stage.process(&mut [&mut l, &mut r]);
        black_box(l[BLOCK - 1]);
        black_box(r[BLOCK - 1]);
    }

    let total_samples_per_ch = (blocks_per_run * BLOCK) as f64;
    let total_channel_samples = total_samples_per_ch * CHANNELS as f64;
    let mut best_ns = f64::INFINITY;
    for _ in 0..RUNS {
        let t0 = Instant::now();
        for _ in 0..blocks_per_run {
            l.copy_from_slice(&src[0]);
            r.copy_from_slice(&src[1]);
            stage.process(&mut [&mut l, &mut r]);
            black_box(l[BLOCK - 1]);
            black_box(r[BLOCK - 1]);
        }
        let elapsed_ns = t0.elapsed().as_secs_f64() * 1e9;
        let ns_per_sample_ch = elapsed_ns / total_channel_samples;
        best_ns = best_ns.min(ns_per_sample_ch);
    }
    best_ns
}

/// One custom Chebyshev layer with `levels_db` (H2..=H9).
fn harmonics_config(levels_db: [Option<f64>; 8]) -> HarmonicsConfig {
    HarmonicsConfig {
        system: None,
        enabled: true,
        layers: vec![HarmonicsLayer::Chebyshev(ChebyshevLayer {
            enabled: true,
            preset: None,
            levels_db,
            signs: HARMONICS_DEFAULT_SIGNS,
            invert_even: false,
            drive_db: 0.0,
            amount: 1.0,
            tilt: None,
            sag: None,
            bias: None,
        })],
        dc_block: true,
        routing: Default::default(),
    }
}

fn harmonics_config_h5() -> HarmonicsConfig {
    harmonics_config([
        Some(-40.0),
        Some(-45.0),
        Some(-60.0),
        Some(-65.0),
        None,
        None,
        None,
        None,
    ])
}

fn harmonics_config_h9() -> HarmonicsConfig {
    harmonics_config([
        Some(-40.0),
        Some(-45.0),
        Some(-60.0),
        Some(-65.0),
        Some(-75.0),
        Some(-80.0),
        Some(-90.0),
        Some(-95.0),
    ])
}

fn main() {
    let blocks_per_run = 1024; // 1,048,576 frames per channel (~2.10M channel-samples)
    println!(
        "Stage cost benchmark (stereo, block={BLOCK}, {blocks_per_run} blocks/run, best of {RUNS} runs):"
    );
    println!(
        "{:<34} {:>12} {:>12} {:>16}",
        "Stage (signal)", "48 kHz ns/sc", "768 kHz ns/sc", "768k stereo core"
    );
    println!("{}", "-".repeat(78));

    let run_pair = |label: &str,
                    make_48: &dyn Fn(f64) -> Box<dyn Stage>,
                    make_src: &dyn Fn(f64) -> [Vec<f64>; CHANNELS]| {
        let src48 = make_src(48_000.0);
        let src768 = make_src(768_000.0);
        let mut s48 = make_48(48_000.0);
        let mut s768 = make_48(768_000.0);
        let ns48 = bench_stage(s48.as_mut(), &src48, blocks_per_run);
        let ns768 = bench_stage(s768.as_mut(), &src768, blocks_per_run);
        // 768 kHz stereo is 1,536,000 channel-samples/s, so one core is
        // 651.04 ns/sc.
        let core_pct = (ns768 * 768_000.0 * CHANNELS as f64) / 1e9 * 100.0;
        println!(
            "{:<34} {:>10.2} ns {:>10.2} ns {:>14.2}%",
            label, ns48, ns768, core_pct
        );
    };

    run_pair(
        "Harmonics (H2-H5, music)",
        &|r| Box::new(Harmonics::try_new(&harmonics_config_h5(), r, CHANNELS).unwrap()),
        &make_music_block,
    );
    run_pair(
        "Harmonics (H2-H9, music)",
        &|r| Box::new(Harmonics::try_new(&harmonics_config_h9(), r, CHANNELS).unwrap()),
        &make_music_block,
    );
    let hyst_layer_cfg = HarmonicsConfig {
        system: None,
        enabled: true,
        layers: vec![HarmonicsLayer::Hysteresis(HysteresisLayer {
            enabled: true,
            preset: None,
            saturation: 0.5,
            width: 0.5,
            amount: 1.0,
            target_h3_db: -40.0,
            calibration_hz: 1_000.0,
            bass_eq: None,
            asymmetry: None,
        })],
        dc_block: true,
        routing: Default::default(),
    };
    run_pair(
        "Harmonics (1 hyst -40, music)",
        &|r| Box::new(Harmonics::try_new(&hyst_layer_cfg, r, CHANNELS).unwrap()),
        &make_music_block,
    );
    run_pair(
        "Harmonics (1 hyst -40, 0 dBFS)",
        &|r| Box::new(Harmonics::try_new(&hyst_layer_cfg, r, CHANNELS).unwrap()),
        &make_full_scale_sine_block,
    );
    // The tape presets as they ship (bass EQ pair and field offset on), and
    // without the two.
    for id in ["tape_master", "tape_vintage"] {
        let shipped = HarmonicsConfig::from_preset(id).unwrap();
        let mut plain = shipped.clone();
        if let HarmonicsLayer::Hysteresis(l) = &mut plain.layers[0] {
            l.bass_eq = None;
            l.asymmetry = None;
        }
        for (what, cfg) in [("", shipped), (" no EQ/offset", plain)] {
            for (signal, make) in [
                ("music", make_music_block as fn(f64) -> [Vec<f64>; CHANNELS]),
                ("0 dBFS", make_full_scale_sine_block),
            ] {
                run_pair(
                    &format!("{id}{what} ({signal})"),
                    &|r| Box::new(Harmonics::try_new(&cfg, r, CHANNELS).unwrap()),
                    &make,
                );
            }
        }
    }
}
