//! `avdsp levelmatch` — analytic and measured A/B level offsets.

use crate::LevelMatchArgs;
use crate::wavio;
use anyhow::Result;
use audivane_dsp_core::chain::{AbChain, Chain, ChainSpec};
use audivane_dsp_core::loudness::{AWeightedMeter, LufsMeter};
use audivane_protocol::{ChainConfig, DitherKind, OutputConfig};
use std::path::Path;

const BLOCK: usize = 1024;

pub fn run(args: LevelMatchArgs) -> Result<i32> {
    match &args.measure {
        Some(path) => measured(&args.a, &args.b, path),
        None => {
            let a = crate::process::profile_config(&args.a)?;
            let b = crate::process::profile_config(&args.b)?;
            let offset_db = AbChain::analytic_offset_db(&a, &b, args.rate);
            println!("method: analytic");
            println!("offset_db: {offset_db:.4}");
            println!("rate: {}", args.rate);
            println!("a: {}", args.a.display());
            println!("b: {}", args.b.display());
            Ok(0)
        }
    }
}

fn measured(a: &Path, b: &Path, music: &Path) -> Result<i32> {
    let wav = wavio::read_wav(music)?;
    let channels = wav.channels;
    let config_a = measurement_config(crate::process::profile_config(a)?);
    let config_b = measurement_config(crate::process::profile_config(b)?);

    let mut chain_a = Chain::new(spec(config_a, &wav));
    let mut chain_b = Chain::new(spec(config_b, &wav));
    let out_rate = chain_a.output_rate();

    let mut meter_a = AWeightedMeter::new(f64::from(out_rate), channels);
    let mut meter_b = AWeightedMeter::new(f64::from(out_rate), channels);
    let mut lufs_a = LufsMeter::new(f64::from(out_rate), channels);
    let mut lufs_b = LufsMeter::new(f64::from(out_rate), channels);

    let ratio = chain_a.ratio();
    let mut out = vec![0i32; BLOCK * ratio * channels];
    let mut planar: Vec<Vec<f64>> = vec![vec![0.0; BLOCK * ratio]; channels];

    for chunk in wav.samples.chunks(BLOCK * channels) {
        let frames = chunk.len() / channels;
        let produced = frames * ratio * channels;
        feed(
            &mut chain_a,
            chunk,
            frames,
            &mut out[..produced],
            &mut planar,
            channels,
            &mut meter_a,
            &mut lufs_a,
        );
        feed(
            &mut chain_b,
            chunk,
            frames,
            &mut out[..produced],
            &mut planar,
            channels,
            &mut meter_b,
            &mut lufs_b,
        );
    }

    let rms_a = meter_a.rms_db();
    let rms_b = meter_b.rms_db();
    let offset_db = rms_a - rms_b;
    println!("method: measured");
    println!("offset_db: {offset_db:.4}");
    println!("a_rms_db: {rms_a:.4}");
    println!("b_rms_db: {rms_b:.4}");
    println!("a_lufs: {:.4}", lufs_a.integrated_lufs());
    println!("b_lufs: {:.4}", lufs_b.integrated_lufs());
    println!("rate: {out_rate}");
    println!("a: {}", a.display());
    println!("b: {}", b.display());
    println!("music: {}", music.display());
    Ok(0)
}

fn measurement_config(mut config: ChainConfig) -> ChainConfig {
    config.output = OutputConfig {
        bits: 32,
        dither: DitherKind::None,
    };
    config
}

fn spec(config: ChainConfig, wav: &wavio::WavData) -> ChainSpec {
    ChainSpec {
        config,
        sample_rate: wav.rate,
        channels: wav.channels,
        max_block: BLOCK,
        irs: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn feed(
    chain: &mut Chain,
    input: &[f32],
    frames: usize,
    out: &mut [i32],
    planar: &mut [Vec<f64>],
    channels: usize,
    meter: &mut AWeightedMeter,
    lufs: &mut LufsMeter,
) {
    chain.process_f32(input, out);
    for (c, chan) in planar.iter_mut().enumerate() {
        for (i, s) in chan[..frames].iter_mut().enumerate() {
            *s = f64::from(out[i * channels + c]) / 2_147_483_648.0;
        }
    }
    let refs: Vec<&[f64]> = planar.iter().map(|chan| &chan[..frames]).collect();
    meter.push(&refs);
    lufs.push(&refs);
}
