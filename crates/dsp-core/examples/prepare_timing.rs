//! Times `design::prepare` of a long windowed-sinc design (FIR path,
//! stop at Nyquist, linear phase), 48 kHz x16.
//!
//! `cargo run --release -p audivane-dsp-core --example prepare_timing -- <dpss|kaiser> <nw|beta> <taps> [linear|minimum|intermediate] [cepstral|reflect]`
use audivane_dsp_core::design::{
    Accept, ArithmeticPath, BuildContext, Cutoff, Decimation, FilterDesign, GridCutoff,
    KernelDesign, Length, Numerics, OddRule, Phase, PhaseMethod, Rates, Sampling, Shape, Window,
    budget, prepare,
};
use audivane_protocol::{OutputConfig, RateRatio};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let p: f64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(8.0);
    let taps: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(8 << 20);
    let window = match a.first().map(String::as_str) {
        Some("kaiser") => Window::Kaiser {
            beta: Some(p),
            sampling: Sampling::Symmetric,
        },
        _ => Window::Dpss {
            nw: Some(p),
            sampling: Sampling::Symmetric,
        },
    };
    let method = match a.get(4).map(String::as_str) {
        Some("reflect") => PhaseMethod::Reflect,
        _ => PhaseMethod::Cepstral,
    };
    let phase = match a.get(3).map(String::as_str) {
        Some("minimum") => Phase::Minimum(method),
        Some("intermediate") => Phase::Intermediate { mix: 0.5, method },
        _ => Phase::Linear,
    };
    let d = FilterDesign::Kernel(KernelDesign {
        shape: Shape::WindowedSinc {
            window,
            length: Length::Fixed(taps),
            cutoff: Cutoff::NyquistStop,
        },
        decimation: Decimation::Scale {
            odd: OddRule::Down,
            base_taps: None,
        },
        phase,
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
    let cx = BuildContext {
        source_rate: 48_000,
        ratio: RateRatio::integer(16),
        channels: 2,
        max_block: 1024,
        pipelines: budget::RATIONAL_SWAP_PIPELINES,
        output: OutputConfig::default(),
        // `WORKSPACE_GIB` overrides the machine's cap (the daemon's is a
        // quarter of RAM within 1.5 to 4 GiB).
        workspace: std::env::var("WORKSPACE_GIB")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map_or_else(budget::design_workspace, |g| {
                (g * (1u64 << 30) as f64) as u64
            }),
    };
    let t = Instant::now();
    let r = prepare(&d, &cx);
    let secs = t.elapsed().as_secs_f64();
    match r {
        Ok(p) => println!(
            "ok: {} taps, {secs:.3} s",
            p.kernel.as_ref().map_or(0, |k| k.taps.len())
        ),
        Err(e) => println!("error: {e:?} after {secs:.3} s"),
    }
}
