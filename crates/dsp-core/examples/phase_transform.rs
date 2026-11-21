//! Run the phase transform of the `_ip`/`_mp` presets on a given kernel, or
//! dump a phase preset's prototype, for offline checks.
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example phase_transform -- \
//!     transform <mp|ip> <in.f64> <out.f64>
//! cargo run --release -p audivane-dsp-core --example phase_transform -- \
//!     prototype <preset> <source_rate> <ratio> <out.f64>
//! ```
//!
//! Files are raw little-endian f64. `transform` writes the unnormalised
//! result of steps 1-6 (see `design::phase::cepstral`); `prototype` writes the
//! linear-phase kernel the preset starts from, before normalisation. A
//! JSON summary goes to stdout.

use audivane_dsp_core::design::{self, catalogue, phase::cepstral};
use audivane_dsp_core::presets::FilterPreset;
use audivane_protocol::{FilterPhase, RateRatio};
use std::io::{Read, Write};

fn read_f64(path: &str) -> Vec<f64> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("open {path}: {e}"))
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| f64::from_le_bytes(*c))
        .collect()
}

fn write_f64(path: &str, data: &[f64]) {
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {path}: {e}")),
    );
    for v in data {
        out.write_all(&v.to_le_bytes()).unwrap();
    }
    out.flush().unwrap();
}

fn usage() -> ! {
    eprintln!(
        "usage: phase_transform transform <mp|ip> <in.f64> <out.f64>\n       phase_transform prototype <preset> <source_rate> <ratio> <out.f64>"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("transform") if args.len() == 4 => {
            let target = match args[1].as_str() {
                "mp" => cepstral::Target::Minimum,
                "ip" => cepstral::Target::Intermediate,
                _ => usage(),
            };
            let h = read_f64(&args[2]);
            let t = std::time::Instant::now();
            let g = cepstral::transform(&h, target).unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1)
            });
            let secs = t.elapsed().as_secs_f64();
            write_f64(&args[3], &g);
            println!(
                "{{\"taps_in\":{},\"taps_out\":{},\"fft_len\":{},\"design_s\":{secs:.3}}}",
                h.len(),
                g.len(),
                cepstral::fft_len(h.len())
            );
        }
        Some("prototype") if args.len() == 5 => {
            let preset = FilterPreset::from_name(&args[1]).unwrap_or_else(|| {
                eprintln!("unknown preset {}", args[1]);
                std::process::exit(2)
            });
            let rate: u32 = args[2].parse().expect("source_rate");
            let ratio: u32 = args[3].parse().expect("ratio");
            let d = catalogue::design(preset);
            if d.filter_phase() == FilterPhase::Linear {
                eprintln!("{} is not a phase preset", preset.name());
                std::process::exit(2)
            }
            let h = design::prototype(&d, rate, RateRatio::integer(ratio)).unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1)
            });
            write_f64(&args[4], &h);
            println!("{{\"preset\":\"{}\",\"taps\":{}}}", preset.name(), h.len());
        }
        _ => usage(),
    }
}
