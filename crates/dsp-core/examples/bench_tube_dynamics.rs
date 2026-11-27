//! Per-sample CPU cost of `TubeDynamics` (sag, bias shift) next to the
//! static Chebyshev layer (`Harmonics`), at 48 kHz and 768 kHz stereo:
//! ns/sample/channel and share of one core at 768 kHz stereo.
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example bench_tube_dynamics
//! ```

use audivane_dsp_core::Stage;
use audivane_dsp_core::harmonics::Harmonics;
use audivane_dsp_core::tube_dynamics::{BiasConfig, SagConfig, TubeDynamics, TubeDynamicsConfig};
use audivane_protocol::{ChebyshevLayer, HARMONICS_DEFAULT_SIGNS, HarmonicsConfig, HarmonicsLayer};
use std::hint::black_box;
use std::time::Instant;

const BLOCK: usize = 1024;
const CHANNELS: usize = 2;
const RUNS: usize = 5;
const BLOCKS_PER_RUN: usize = 1024;

/// Pink-like music block (-14 dBFS RMS, peaks near -1 dBFS) plus a tone.
fn music_block(rate: f64) -> [Vec<f64>; CHANNELS] {
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

/// Best (minimum) ns per sample per channel over `RUNS`.
fn bench(stage: &mut dyn Stage, src: &[Vec<f64>; CHANNELS]) -> f64 {
    let mut l = vec![0.0; BLOCK];
    let mut r = vec![0.0; BLOCK];
    for _ in 0..64 {
        l.copy_from_slice(&src[0]);
        r.copy_from_slice(&src[1]);
        stage.process(&mut [&mut l, &mut r]);
        black_box((l[BLOCK - 1], r[BLOCK - 1]));
    }
    let samples = (BLOCKS_PER_RUN * BLOCK * CHANNELS) as f64;
    let mut best = f64::INFINITY;
    for _ in 0..RUNS {
        let t0 = Instant::now();
        for _ in 0..BLOCKS_PER_RUN {
            l.copy_from_slice(&src[0]);
            r.copy_from_slice(&src[1]);
            stage.process(&mut [&mut l, &mut r]);
            black_box((l[BLOCK - 1], r[BLOCK - 1]));
        }
        best = best.min(t0.elapsed().as_secs_f64() * 1e9 / samples);
    }
    best
}

fn layer(levels_db: [Option<f64>; 8]) -> ChebyshevLayer {
    ChebyshevLayer {
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
    }
}

fn main() {
    let h5 = layer([
        Some(-40.0),
        Some(-45.0),
        Some(-60.0),
        Some(-65.0),
        None,
        None,
        None,
        None,
    ]);
    let h9 = layer([
        Some(-40.0),
        Some(-45.0),
        Some(-60.0),
        Some(-65.0),
        Some(-75.0),
        Some(-80.0),
        Some(-90.0),
        Some(-95.0),
    ]);
    let no_sag = SagConfig {
        depth_db: 0.0,
        ..SagConfig::default()
    };
    let no_bias = BiasConfig {
        depth: 0.0,
        ..BiasConfig::default()
    };
    type Make = Box<dyn Fn(f64) -> Box<dyn Stage>>;
    let mut cases: Vec<(String, Make)> = Vec::new();
    for (name, l) in [("H2-H5", h5), ("H2-H9", h9)] {
        let static_cfg = HarmonicsConfig {
            system: None,
            enabled: true,
            layers: vec![HarmonicsLayer::Chebyshev(l.clone())],
            dc_block: true,
            routing: Default::default(),
        };
        cases.push((
            format!("Harmonics static ({name})"),
            Box::new(move |r| Box::new(Harmonics::try_new(&static_cfg, r, CHANNELS).unwrap())),
        ));
        for (label, sag, bias) in [
            ("depth 0", no_sag, no_bias),
            ("sag", SagConfig::default(), no_bias),
            ("sag + bias", SagConfig::default(), BiasConfig::default()),
        ] {
            let cfg = TubeDynamicsConfig::from_layer(&l, true, sag, bias).unwrap();
            cases.push((
                format!("TubeDynamics {label} ({name})"),
                Box::new(move |r| Box::new(TubeDynamics::try_new(&cfg, r, CHANNELS).unwrap())),
            ));
        }
    }
    println!(
        "Stage cost (stereo, music, block={BLOCK}, {BLOCKS_PER_RUN} blocks/run, best of {RUNS}):"
    );
    println!(
        "{:<36} {:>12} {:>13} {:>16}",
        "Stage", "48 kHz ns/sc", "768 kHz ns/sc", "768k stereo core"
    );
    println!("{}", "-".repeat(80));
    let (src48, src768) = (music_block(48_000.0), music_block(768_000.0));
    for (label, make) in &cases {
        let ns48 = bench(make(48_000.0).as_mut(), &src48);
        let ns768 = bench(make(768_000.0).as_mut(), &src768);
        let core = ns768 * 768_000.0 * CHANNELS as f64 / 1e9 * 100.0;
        println!("{label:<36} {ns48:>9.2} ns {ns768:>10.2} ns {core:>15.2}%");
    }
}
