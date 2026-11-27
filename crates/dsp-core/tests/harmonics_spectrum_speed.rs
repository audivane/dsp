//! Spectrum timings (`--ignored --nocapture`, release):
//! `cargo test --release -p audivane-dsp-core --test harmonics_spectrum_speed -- --ignored --nocapture`

use audivane_dsp_core::harmonics::{clear_readout_caches, spectrum};
use audivane_protocol::ipc::{HarmonicsSpectrumTrace, HarmonicsSpectrumWord};
use audivane_protocol::{DitherKind, HarmonicsConfig, HarmonicsLayer, HarmonicsRouting};
use std::time::Instant;

fn layer(id: &str) -> HarmonicsLayer {
    HarmonicsConfig::from_preset(id).unwrap().layers[0].clone()
}

#[test]
#[ignore]
fn spectrum_timings() {
    let word = HarmonicsSpectrumWord {
        bits: 24,
        dither: DitherKind::Tpdf,
        headroom_db: 3.0,
    };
    let parallel = HarmonicsConfig {
        enabled: true,
        layers: vec![layer("se_triode_high_mu"), layer("pp_triode")],
        ..Default::default()
    };
    let tape = HarmonicsConfig {
        enabled: true,
        layers: vec![layer("tape_master")],
        ..Default::default()
    };
    let series = HarmonicsConfig {
        enabled: true,
        layers: vec![
            layer("se_triode_high_mu"),
            layer("output_transformer"),
            layer("tape_studio"),
        ],
        routing: HarmonicsRouting::Series,
        ..Default::default()
    };
    let all = [
        HarmonicsSpectrumTrace::Stage,
        HarmonicsSpectrumTrace::After { layer: 0 },
        HarmonicsSpectrumTrace::After { layer: 1 },
    ];
    let cases: [(&str, &HarmonicsConfig, &[HarmonicsSpectrumTrace]); 4] = [
        ("parallel chebyshev", &parallel, &[]),
        ("1 tape", &tape, &[]),
        ("3-layer series (stage)", &series, &[]),
        ("3-layer series (stage + after 0, 1)", &series, &all),
    ];
    for rate in [96_000.0, 768_000.0] {
        for (name, cfg, traces) in cases {
            for f in [1_000.0, 20.0] {
                clear_readout_caches();
                let t = Instant::now();
                let s = spectrum(cfg, rate, f, 0.0, traces, word).unwrap();
                let cold = t.elapsed();
                let t = Instant::now();
                spectrum(cfg, rate, f, 0.0, traces, word).unwrap();
                let warm = t.elapsed();
                println!(
                    "{rate:>6} Hz {f:>5} Hz {name:<38} cold {:>7.1} ms  cached {:>5.2} ms  (bin {:.2} Hz, THD {:.1} dB)",
                    cold.as_secs_f64() * 1e3,
                    warm.as_secs_f64() * 1e3,
                    s.bin_hz,
                    s.lines[0].thd_db
                );
            }
        }
    }
}
