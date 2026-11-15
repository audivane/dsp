//! `avdsp kernel` — dump an upsampler kernel for reference tests.

use crate::KernelArgs;
use anyhow::{Context, Result};
use audivane_dsp_core::resample::{UpsamplerDesign, try_design_kernel};
use serde_json::json;
use std::fs::File;
use std::io::{BufWriter, Write};

pub fn run(args: KernelArgs) -> Result<i32> {
    if args.cutoff <= 0.0 || args.cutoff > 1.0 {
        anyhow::bail!("--cutoff must be in (0, 1], got {}", args.cutoff);
    }
    if args.taps == 0 {
        anyhow::bail!("--taps must be at least 1");
    }
    let design = UpsamplerDesign {
        ratio: args.ratio as usize,
        taps: args.taps,
        phase: args.phase.into(),
        kaiser_beta: args.beta,
        cutoff: args.cutoff,
    };
    let kernel = try_design_kernel(&design).map_err(|e| anyhow::anyhow!("{e}"))?;

    let file =
        File::create(&args.out).with_context(|| format!("cannot create {}", args.out.display()))?;
    let mut writer = BufWriter::new(file);
    for tap in &kernel {
        writer.write_all(&tap.to_le_bytes())?;
    }
    writer.flush()?;

    let sum: f64 = kernel.iter().sum();
    let peak = kernel.iter().fold(0.0f64, |acc, x| acc.max(x.abs()));
    if args.json {
        println!(
            "{}",
            json!({
                "rate": args.rate,
                "ratio": args.ratio,
                "taps": args.taps,
                "phase": match args.phase {
                    crate::PhaseArg::Linear => "linear",
                    crate::PhaseArg::Minimum => "minimum",
                },
                "kaiser_beta": args.beta,
                "cutoff": args.cutoff,
                "kernel_len": kernel.len(),
                "sum": sum,
                "peak": peak,
                "out": args.out.display().to_string(),
            })
        );
    } else {
        println!(
            "wrote {}: {} taps, ratio {}, {:?} phase, beta {:.2}, cutoff {:.4}",
            args.out.display(),
            kernel.len(),
            args.ratio,
            args.phase,
            args.beta,
            args.cutoff
        );
        println!("sum: {sum:.9}, peak: {peak:.9}");
    }
    Ok(0)
}
