//! Per-sample cost of the harmonics stage for every preset and a few mixed
//! stacks, stereo at 48 kHz and 768 kHz and mono (one single-channel stage
//! per channel, as the channel workers run it) at 768 kHz (ns per sample per
//! channel, best of several runs on music-level noise), plus the time of a
//! readout with sweeps.
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example bench_harmonics_layers
//! ```

use audivane_dsp_core::Stage;
use audivane_dsp_core::harmonics::{Harmonics, readout};
use audivane_protocol::{
    HarmonicsBias, HarmonicsConfig, HarmonicsLayer, HarmonicsSag, harmonics_presets,
};
use std::hint::black_box;
use std::time::Instant;

const BLOCK: usize = 512;
const RUNS: usize = 7;

fn layer(id: &str) -> HarmonicsLayer {
    HarmonicsConfig::from_preset(id).unwrap().layers[0].clone()
}

fn stack(ids: &[&str]) -> HarmonicsConfig {
    HarmonicsConfig {
        system: None,
        enabled: true,
        layers: ids.iter().map(|id| layer(id)).collect(),
        dc_block: true,
        routing: Default::default(),
    }
}

fn noise(n: usize, seed: u64) -> Vec<f64> {
    let mut s = seed;
    let mut lp = 0.0;
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let w = (s >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            lp = 0.8 * lp + 0.3 * w;
            lp
        })
        .collect()
}

/// Best ns per sample per channel: one stage for both channels (`mono`
/// false), or one single-channel stage per channel as the channel workers
/// run them.
fn bench(cfg: &HarmonicsConfig, rate: f64, mono: bool) -> f64 {
    let n = 1 << 17;
    let (l0, r0) = (
        noise(n, 0x9E37_79B9_7F4A_7C15),
        noise(n, 0x2545_F491_4F6C_DD1D),
    );
    let mut joint = Harmonics::try_new(cfg, rate, 2).unwrap();
    let mut parts = Harmonics::try_new(cfg, rate, 2).unwrap().split_channels();
    let mut best = f64::INFINITY;
    for _ in 0..RUNS {
        let (mut l, mut r) = (l0.clone(), r0.clone());
        let t = Instant::now();
        for (a, b) in l.chunks_mut(BLOCK).zip(r.chunks_mut(BLOCK)) {
            if mono {
                parts[0].process(&mut [a]);
                parts[1].process(&mut [b]);
            } else {
                joint.process(&mut [a, b]);
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
        black_box(&l);
    }
    best / (2 * n) as f64 * 1e9
}

fn main() {
    let mut cases: Vec<(String, HarmonicsConfig)> = harmonics_presets()
        .iter()
        .map(|p| (p.id.clone(), HarmonicsConfig::from_preset(&p.id).unwrap()))
        .collect();
    cases.push((
        "mix: se_triode_high_mu + output_transformer".into(),
        stack(&["se_triode_high_mu", "output_transformer"]),
    ));
    cases.push((
        "mix: se_triode_high_mu + tape_studio".into(),
        stack(&["se_triode_high_mu", "tape_studio"]),
    ));
    cases.push((
        "mix: pp_pentode_feedback + output_transformer".into(),
        stack(&["pp_pentode_feedback", "output_transformer"]),
    ));
    cases.push((
        "mix: pp_pentode_feedback + tape_studio".into(),
        stack(&["pp_pentode_feedback", "tape_studio"]),
    ));
    cases.push((
        "mix: solid_state_feedback + output_transformer + tape + tape_studio + tape_master".into(),
        stack(&[
            "solid_state_feedback",
            "output_transformer",
            "tape",
            "tape_studio",
            "tape_master",
        ]),
    ));
    cases.push((
        "mix: 8 tape layers".into(),
        stack(&[
            "tape_master",
            "tape_studio",
            "tape_vintage",
            "tape_master",
            "tape_studio",
            "tape_vintage",
            "tape_master",
            "tape_studio",
        ]),
    ));
    // Sag and bias (each such layer runs on its own, outside the bank).
    let dynamic = |id: &str, sag: bool, bias: bool| {
        let mut l = layer(id);
        if let HarmonicsLayer::Chebyshev(c) = &mut l {
            c.sag = sag.then(HarmonicsSag::default);
            c.bias = bias.then(HarmonicsBias::default);
        }
        l
    };
    for (name, sag, bias) in [
        ("sag", true, false),
        ("bias", false, true),
        ("sag + bias", true, true),
    ] {
        cases.push((
            format!("dynamic: se_triode_high_mu, {name}"),
            HarmonicsConfig {
                system: None,
                enabled: true,
                layers: vec![dynamic("se_triode_high_mu", sag, bias)],
                dc_block: true,
                routing: Default::default(),
            },
        ));
    }
    cases.push((
        "dynamic: pentode + se_triode_high_mu sag + bias + tape_studio".into(),
        HarmonicsConfig {
            system: None,
            enabled: true,
            layers: vec![
                layer("pentode"),
                dynamic("se_triode_high_mu", true, true),
                layer("tape_studio"),
            ],
            dc_block: true,
            routing: Default::default(),
        },
    ));
    // An argument keeps only the cases whose name contains it.
    let only = std::env::args().nth(1).unwrap_or_default();
    cases.retain(|(name, _)| name.contains(&only));
    println!("case\t48k stereo\t768k stereo\t768k mono\treadout 768k ms");
    for (name, cfg) in &cases {
        let a = bench(cfg, 48_000.0, false);
        let b = bench(cfg, 768_000.0, false);
        let m = bench(cfg, 768_000.0, true);
        let t = Instant::now();
        black_box(readout(cfg, 768_000.0, true).unwrap());
        let ro = t.elapsed().as_secs_f64() * 1e3;
        println!("{name}\t{a:.2}\t{b:.2}\t{m:.2}\t{ro:.0}");
    }
}
