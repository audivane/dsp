//! Render a mono signal through a filter preset, for offline checks.
//!
//! ```text
//! cargo run --release -p audivane-dsp-core --example render_preset -- \
//!     <preset> <source_rate> <ratio> <in.f64> <out.f64> [--block N] [--flush N] [--s32]
//! cargo run --release -p audivane-dsp-core --example render_preset -- \
//!     <preset> <source_rate> <ratio> --kernel <out.f64>
//! ```
//!
//! `<ratio>` is an integer upsampling ratio (`8`), a reduced or unreduced
//! fraction output/input (`160/147`), or an output rate in Hz (any value
//! above 64, e.g. `48000`). At a rational ratio each block produces a
//! varying number of output frames; `--kernel` then writes the kernel on
//! the intermediate grid (source rate x the numerator).
//!
//! Files are raw little-endian f64, mono. The output is everything the
//! runtime produces, engine latency included: the input followed by
//! `--flush` zero frames (default: enough to clear the impulse span), fed
//! in blocks of `--block` frames (default 1024). `--kernel` writes a
//! single-kernel preset's taps instead. `--s32` passes the output through
//! the engine's own output quantizer (32 bits, no dither) and writes raw
//! little-endian i32 instead of f64. A JSON summary goes to stdout.

use audivane_dsp_core::design::{self, BuildContext, Named};
use audivane_dsp_core::dither::Quantizer;
use audivane_dsp_core::presets::FilterPreset;
use audivane_protocol::{DitherKind, OutputConfig, RateRatio};
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

/// An integer ratio, `up/down`, or an output rate in Hz (above 64).
fn parse_ratio(arg: &str, source_rate: u32) -> RateRatio {
    if let Some((u, d)) = arg.split_once('/') {
        return RateRatio::new(u.parse().expect("ratio up"), d.parse().expect("ratio down"));
    }
    let v: u32 = arg.parse().expect("ratio or output rate");
    if v > 64 {
        RateRatio::from_rates(source_rate, v)
    } else {
        RateRatio::integer(v)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!(
            "usage: render_preset <preset> <source_rate> <ratio> <in.f64> <out.f64> [--block N] [--flush N]\n       render_preset <preset> <source_rate> <ratio> --kernel <out.f64>"
        );
        std::process::exit(2);
    }
    let preset = FilterPreset::from_name(&args[0]).unwrap_or_else(|| {
        eprintln!("unknown preset {}", args[0]);
        std::process::exit(2)
    });
    let source_rate: u32 = args[1].parse().expect("source_rate");
    let ratio = parse_ratio(&args[2], source_rate);

    let context = |block| BuildContext {
        source_rate,
        ratio,
        channels: 1,
        max_block: block,
        pipelines: design::budget::RATIONAL_SWAP_PIPELINES,
        output: OutputConfig::default(),
        workspace: audivane_dsp_core::design::budget::design_workspace(),
    };
    let fail = |e: &dyn std::fmt::Display| -> ! {
        eprintln!("{e}");
        std::process::exit(1)
    };

    if args[3] == "--kernel" {
        let t = std::time::Instant::now();
        // An export: the construction limits and the ratio rules, not the
        // playback budgets.
        let k = design::offline_kernel(&Named::preset(preset).design, source_rate, ratio)
            .unwrap_or_else(|e| fail(&design::upsampler::preset_error(preset, e)));
        write_f64(&args[4], &k.taps);
        println!(
            "{{\"preset\":\"{}\",\"taps\":{},\"delay\":{},\"design_s\":{:.3}}}",
            preset.name(),
            k.taps.len(),
            k.delay,
            t.elapsed().as_secs_f64()
        );
        return;
    }

    let mut block = 1024usize;
    let mut flush: Option<usize> = None;
    let mut s32 = false;
    let mut i = 5;
    while i < args.len() {
        match args[i].as_str() {
            "--block" => block = args[i + 1].parse().expect("--block N"),
            "--flush" => flush = Some(args[i + 1].parse().expect("--flush N")),
            "--s32" => {
                s32 = true;
                i += 1;
                continue;
            }
            other => panic!("unknown option {other}"),
        }
        i += 2;
    }

    let input = read_f64(&args[3]);
    let t = std::time::Instant::now();
    let mut up = design::upsampler(&Named::preset(preset), &context(block), 0.0)
        .unwrap_or_else(|e| fail(&e))
        .unwrap_or_else(|| fail(&"no upsampler at a unity ratio"));
    let design_s = t.elapsed().as_secs_f64();
    let flush =
        flush.unwrap_or_else(|| ratio.max_frames_in_for(up.impulse_span_out_frames()) + block);
    let total = input.len() + flush;
    let mut output = vec![0.0; ratio.max_frames_out(total)];
    let mut buf = vec![0.0; block];
    let t = std::time::Instant::now();
    let (mut pos, mut out_pos) = (0, 0);
    while pos < total {
        let n = block.min(total - pos);
        for (j, slot) in buf[..n].iter_mut().enumerate() {
            *slot = input.get(pos + j).copied().unwrap_or(0.0);
        }
        let n_out = up.output_frames(n);
        up.process(&[&buf[..n]], &mut [&mut output[out_pos..out_pos + n_out]]);
        pos += n;
        out_pos += n_out;
    }
    output.truncate(out_pos);
    if s32 {
        let cfg = OutputConfig {
            bits: 32,
            dither: DitherKind::None,
            headroom_db: 0.0,
        };
        let mut q = Quantizer::new(&cfg, 1, 0);
        let mut ints = vec![0i32; output.len()];
        q.process(&[&output[..]], &mut ints);
        let mut out = std::io::BufWriter::new(std::fs::File::create(&args[4]).unwrap());
        for v in &ints {
            out.write_all(&v.to_le_bytes()).unwrap();
        }
        out.flush().unwrap();
    } else {
        write_f64(&args[4], &output);
    }
    println!(
        "{{\"preset\":\"{}\",\"ratio\":\"{}\",\"latency_out\":{},\"impulse_span_out\":{},\"out_frames\":{},\"design_s\":{:.3},\"run_s\":{:.3}}}",
        preset.name(),
        ratio,
        up.latency_out_frames(),
        up.impulse_span_out_frames(),
        output.len(),
        design_s,
        t.elapsed().as_secs_f64()
    );
}
