//! Times the app's calls on an Optimal design offline: `prepare` at each
//! ratio of "Highest, same family, up to 768k" and `measure_upsampler_at`
//! (what `upsampler_response` runs).
//!
//! `cargo run --release -p audivane-dsp-core --example optimal_timing -- <equiripple|least_squares> <pass> <fixed taps|auto dB>`
use audivane_dsp_core::design::{
    Accept, ArithmeticPath, Decimation, FilterDesign, FilterErrorBudget, GridCutoff, KernelDesign,
    Length, Numerics, OddRule, OptimalMethod, Phase, Rates, Shape, upsampler,
};
use audivane_dsp_core::resample::measure_upsampler_at;
use audivane_protocol::{RateRatio, RateTarget, UpsamplerConfig};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let method = a.first().cloned().unwrap_or("least_squares".into());
    let pass: f64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.9);
    let length = match a.get(2).map(String::as_str) {
        Some(s) if s.starts_with("auto") => {
            Length::Auto(FilterErrorBudget::Db(s[4..].parse().unwrap_or(135.0)))
        }
        Some(s) => Length::Fixed(s.parse().expect("taps")),
        None => Length::Auto(FilterErrorBudget::Db(135.0)),
    };
    let design = FilterDesign::Kernel(KernelDesign {
        shape: Shape::Optimal {
            method: if method == "equiripple" {
                OptimalMethod::Equiripple
            } else {
                OptimalMethod::LeastSquares
            },
            length,
            passband_end: pass,
            stopband_start: 1.0,
        },
        decimation: Decimation::Scale {
            odd: OddRule::Down,
            base_taps: None,
        },
        phase: Phase::Linear,
        preserve_samples: false,
        rates: Rates {
            accept: Accept::Any,
            fallback_window: None,
        },
        numerics: Numerics {
            path: ArithmeticPath::Fir,
            grid_cutoff: GridCutoff::Scaled,
        },
    });
    let cfg = UpsamplerConfig {
        target: RateTarget::MaxRate(768_000),
        preset: None,
        design: Some(design.into()),
    };
    let named = upsampler::Named::of(&cfg).unwrap();
    for (src, up) in [
        (44_100, 16),
        (48_000, 16),
        (88_200, 8),
        (96_000, 8),
        (176_400, 4),
        (192_000, 4),
    ] {
        let cx = audivane_dsp_core::design::BuildContext {
            source_rate: src,
            ratio: RateRatio::integer(up),
            channels: 2,
            max_block: 1024,
            pipelines: audivane_dsp_core::design::budget::RATIONAL_SWAP_PIPELINES,
            output: Default::default(),
            workspace: audivane_dsp_core::design::budget::design_workspace(),
        };
        let t = Instant::now();
        let p = upsampler::prepare(&named, &cx);
        eprintln!(
            "MARK prepare {src} x{up}: {:.3} s, {}",
            t.elapsed().as_secs_f64(),
            match &p {
                Ok(p) => format!("{} taps", p.kernel.as_ref().map_or(0, |k| k.taps.len())),
                Err(e) => format!("{e:?}"),
            }
        );
    }
    let t = Instant::now();
    let m = measure_upsampler_at(&cfg, 48_000, 1024);
    eprintln!(
        "MARK measure_upsampler_at 48000: {:.3} s {}",
        t.elapsed().as_secs_f64(),
        m.map(|_| "ok".to_string()).unwrap_or_else(|e| e)
    );
}
