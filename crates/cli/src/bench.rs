//! `avdsp bench` — real-time factor of the full chain, single threaded.

use crate::BenchArgs;
use crate::signal::Rng;
use anyhow::{Result, bail};
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_protocol::{ChainConfig, ConvolutionConfig, DitherKind, RateTarget};
use std::path::PathBuf;
use std::time::Instant;

pub fn run(args: BenchArgs) -> Result<i32> {
    if args.rate == 0 {
        bail!("--rate must be greater than zero");
    }
    if !(args.seconds.is_finite() && args.seconds > 0.0) {
        bail!("--seconds must be greater than zero");
    }
    if args.block == 0 {
        bail!("--block must be at least 1 frame");
    }
    if args.taps == 0 {
        bail!("--taps must be at least 1");
    }

    let channels = 2usize;
    let mut config = ChainConfig {
        upsampler: audivane_dsp_core::resample::ManualConfig {
            target: RateTarget::Ratio(args.ratio as u32),
            taps: args.taps,
            ..Default::default()
        }
        .into_upsampler_config()
        .map_err(|e| anyhow::anyhow!("{e}"))?,
        ..Default::default()
    };
    config.output.dither = DitherKind::Tpdf;

    if let Some(path) = &args.eq {
        config.eq = Some(crate::profile::load(path)?);
    }

    let mut irs = None;
    if let Some(taps) = args.conv_taps {
        if taps == 0 {
            bail!("--conv-taps must be at least 1");
        }
        config.convolution = vec![ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: PathBuf::new(),
            gain_db: 0.0,
        }];
        irs = Some(vec![synthetic_ir(taps)]);
    }

    let frames = (args.seconds * f64::from(args.rate)).round() as usize;
    let mut rng = Rng::new(0xBADC_0FFEE);
    let input: Vec<f32> = (0..frames * channels)
        .map(|_| (rng.next_bipolar() * 0.25) as f32)
        .collect();

    let spec = ChainSpec {
        config,
        sample_rate: args.rate,
        channels,
        max_block: args.block,
        irs,
    };
    let mut chain = Chain::new(spec);
    let latency = chain.latency_out_frames();
    let mut out = vec![0i32; chain.max_output_frames() * channels];

    // Warm up: page in the buffers and the FFT plans.
    for chunk in input.chunks(args.block * channels).take(4) {
        chain.process_f32(chunk, &mut out);
    }
    chain.reset();

    let started = Instant::now();
    for chunk in input.chunks(args.block * channels) {
        chain.process_f32(chunk, &mut out);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let audio_seconds = frames as f64 / f64::from(args.rate);
    let rtf = crate::process::realtime_factor(audio_seconds, elapsed);

    println!(
        "chain: {} Hz x{} ({:.0} Hz out), {} taps, conv {}, eq {}, block {}, latency {} frames",
        args.rate,
        args.ratio,
        f64::from(args.rate) * args.ratio as f64,
        args.taps,
        match args.conv_taps {
            Some(taps) => format!("{taps} taps"),
            None => "off".to_string(),
        },
        match &args.eq {
            Some(path) => path.display().to_string(),
            None => "off".to_string(),
        },
        args.block,
        latency
    );
    println!("audio: {audio_seconds:.3} s, elapsed: {elapsed:.3} s");
    println!(
        "real-time factor: {rtf:.2}x ({:.2}% of one core)",
        100.0 / rtf.max(f64::MIN_POSITIVE)
    );
    Ok(0)
}

/// Deterministic, decaying pseudo-random IR for convolution benchmarks.
fn synthetic_ir(taps: usize) -> Vec<f64> {
    let mut rng = Rng::new(0x1234_5678_9ABC_DEF0);
    let decay = taps as f64 / 6.0;
    (0..taps)
        .map(|i| {
            let envelope = (-(i as f64) / decay).exp();
            rng.next_bipolar() * envelope * 0.2
        })
        .collect()
}
