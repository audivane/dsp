//! Readout timings (`--ignored --nocapture`, release):
//! `cargo test --release -p audivane-dsp-core --test harmonics_readout_speed -- --ignored --nocapture`

use audivane_dsp_core::harmonics::{ReadoutSweeps, clear_readout_caches, readout, readout_sweeps};
use audivane_protocol::{
    HarmonicsBias, HarmonicsConfig, HarmonicsLayer, HarmonicsRouting, HarmonicsSag,
};
use std::time::Instant;

fn layer(id: &str) -> HarmonicsLayer {
    HarmonicsConfig::from_preset(id).unwrap().layers[0].clone()
}

fn stage_of(layers: Vec<HarmonicsLayer>) -> HarmonicsConfig {
    HarmonicsConfig {
        enabled: true,
        layers,
        ..Default::default()
    }
}

fn sag_bias(id: &str, tilted: bool) -> HarmonicsLayer {
    let mut l = layer(id);
    if let HarmonicsLayer::Chebyshev(c) = &mut l {
        if !tilted {
            c.tilt = None;
        }
        c.sag = Some(HarmonicsSag {
            depth_db: 3.0,
            attack_ms: 20.0,
            release_ms: 200.0,
        });
        c.bias = Some(HarmonicsBias {
            depth: 0.1,
            attack_ms: 300.0,
            release_ms: 2_000.0,
        });
    }
    l
}

pub fn cases() -> Vec<(&'static str, HarmonicsConfig)> {
    vec![
        ("1 chebyshev", stage_of(vec![layer("se_triode_high_mu")])),
        ("1 tape", stage_of(vec![layer("tape_master")])),
        (
            "3 tape",
            stage_of(vec![
                layer("tape_studio"),
                layer("tape_master"),
                layer("tape_vintage"),
            ]),
        ),
        (
            "tilt + flux + tape",
            stage_of(vec![
                layer("pp_pentode_feedback"),
                layer("output_transformer"),
                layer("tape_master"),
            ]),
        ),
        (
            "series triode > tape",
            HarmonicsConfig {
                routing: HarmonicsRouting::Series,
                ..stage_of(vec![layer("se_triode_high_mu"), layer("tape_master")])
            },
        ),
        (
            "series tape > pentode",
            HarmonicsConfig {
                routing: HarmonicsRouting::Series,
                ..stage_of(vec![layer("tape_vintage"), layer("pp_pentode_feedback")])
            },
        ),
        (
            "sag/bias",
            stage_of(vec![sag_bias("pp_pentode_feedback", false)]),
        ),
        (
            "tilted sag/bias",
            stage_of(vec![sag_bias("pp_pentode_feedback", true)]),
        ),
    ]
}

/// A copy of `cfg` with its last slow layer changed a little: an edit of
/// one layer, the others unchanged.
fn edit_one(cfg: &HarmonicsConfig) -> HarmonicsConfig {
    let mut c = cfg.clone();
    if let Some(l) = c.layers.iter_mut().rev().find(|l| match l {
        HarmonicsLayer::Hysteresis(_) => true,
        HarmonicsLayer::Chebyshev(c) => c.sag.is_some(),
        HarmonicsLayer::Flux(_) => false,
    }) {
        match l {
            HarmonicsLayer::Hysteresis(h) => h.amount *= 0.9,
            HarmonicsLayer::Chebyshev(c) => c.drive_db -= 1.0,
            HarmonicsLayer::Flux(_) => {}
        }
    }
    c
}

#[test]
#[ignore]
fn readout_timings() {
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1e3;
    println!("rate     case                 cold ms  warm ms  edit-one ms");
    for rate in [96_000.0, 768_000.0] {
        for (name, cfg) in cases() {
            clear_readout_caches();
            let t = Instant::now();
            readout(&cfg, rate, true).unwrap();
            let cold = ms(t);
            let t = Instant::now();
            readout(&cfg, rate, true).unwrap();
            let warm = ms(t);
            let t = Instant::now();
            readout(&edit_one(&cfg), rate, true).unwrap();
            let edit = ms(t);
            println!("{rate:>8} {name:<20} {cold:7.0}  {warm:7.0}  {edit:7.0}");
        }
    }
}

/// The sweeps moved by the app's sliders: cold (nothing cached), then a new
/// tone and a new level with the rest cached (a slider step).
#[test]
#[ignore]
fn sweep_slider_cost() {
    let cases = [
        (
            "parallel Chebyshev (se_triode_high_mu)",
            stage_of(vec![layer("se_triode_high_mu")]),
        ),
        (
            "tape layer (tape_master)",
            stage_of(vec![layer("tape_master")]),
        ),
        (
            "3-layer series (set_amp)",
            audivane_protocol::harmonics_system("set_amp")
                .unwrap()
                .to_config(),
        ),
    ];
    for rate in [96_000.0, 768_000.0] {
        for (name, cfg) in &cases {
            clear_readout_caches();
            let t = Instant::now();
            readout(cfg, rate, true).unwrap();
            let cold = t.elapsed();
            let t = Instant::now();
            readout_sweeps(
                cfg,
                rate,
                ReadoutSweeps {
                    level_sweep_hz: Some(315.0),
                    freq_sweep_dbfs: None,
                },
            )
            .unwrap();
            let tone = t.elapsed();
            let t = Instant::now();
            readout_sweeps(
                cfg,
                rate,
                ReadoutSweeps {
                    level_sweep_hz: Some(315.0),
                    freq_sweep_dbfs: Some(-9.0),
                },
            )
            .unwrap();
            let level = t.elapsed();
            let t = Instant::now();
            readout_sweeps(
                cfg,
                rate,
                ReadoutSweeps {
                    level_sweep_hz: Some(315.0),
                    freq_sweep_dbfs: Some(-9.0),
                },
            )
            .unwrap();
            let again = t.elapsed();
            println!(
                "{rate:>7} {name:<40} default cold {cold:>9.1?}  new tone {tone:>9.1?}  new level {level:>9.1?}  repeat {again:>9.1?}"
            );
        }
    }
}
