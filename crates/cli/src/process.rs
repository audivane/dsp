//! `avdsp process` — run the DSP chain on a WAV file.

use crate::wavio::{self, Container, WavData};
use crate::{ContainerArg, ProcessArgs};
use anyhow::{Context, Result, bail};
use audivane_dsp_core::biquad::Eq;
use audivane_dsp_core::chain::{Chain, ChainSpec};
use audivane_dsp_core::conv::{combine_irs, combined_ir_length};
use audivane_dsp_core::dither::ClipStats;
use audivane_dsp_core::resample::resample_offline;
use audivane_protocol::{
    ChainConfig, ChannelEq, ConvolutionConfig, DitherKind, EqProfile, RateTarget,
    resolve_rate_ratio,
};
use std::fs;
use std::path::Path;
use std::time::Instant;

pub fn run(args: ProcessArgs) -> Result<i32> {
    if args.block == 0 {
        bail!("--block must be at least 1 frame");
    }
    if args.taps == Some(0) {
        bail!("--taps must be at least 1");
    }
    let wav = wavio::read_wav(&args.input)?;
    let (config, container) = build_config(&args, &wav)?;
    let passthrough = config.is_passthrough();
    let out_bits = config.output.bits;
    let dither = config.output.dither;
    let irs = load_irs(&config, wav.rate, wav.channels)?;
    let ratio = if passthrough {
        audivane_protocol::RateRatio::ONE
    } else {
        resolve_rate_ratio(config.upsampler.target, wav.rate)
    };
    let out_rate = ratio
        .output_rate(wav.rate)
        .context("the output rate is not a whole number of Hz")?;

    let spec = ChainSpec {
        config,
        sample_rate: wav.rate,
        channels: wav.channels,
        max_block: args.block,
        irs,
    };

    let started = Instant::now();
    let mut chain = Chain::try_new(spec)?;
    let latency = chain.latency_out_frames();

    let channels = wav.channels;
    let total_out = ratio.frames_out_total(wav.frames() as u64) as usize * channels;
    let mut out_buf = vec![0i32; chain.max_output_frames() * channels];
    let zeros = vec![0.0f32; args.block * channels];
    let mut result: Vec<i32> = Vec::with_capacity(
        total_out
            + if args.trim_latency {
                latency * channels
            } else {
                0
            },
    );
    let mut stats = ClipStats::default();

    for chunk in wav.samples.chunks(args.block * channels) {
        let frames = chunk.len() / channels;
        let produced = chain.output_frames(frames) * channels;
        stats.merge(chain.process_f32(chunk, &mut out_buf[..produced]));
        result.extend_from_slice(&out_buf[..produced]);
    }

    if args.trim_latency {
        // Feed silence long enough for the algorithmic delay *and* the
        // upsampler's interpolation tail to flush, then drop exactly the
        // reported latency and keep the input-length signal.
        // Latency and kernel length in input frames (the kernel's `taps`
        // count output frames at most `down` grid samples apart).
        let per_in = ratio.as_f64();
        let kernel_frames = if ratio.is_unity() {
            0
        } else {
            let taps = audivane_dsp_core::design::Named::of(&chain.config().upsampler)
                .ok()
                .and_then(|n| audivane_dsp_core::design::grid_taps(&n.design, wav.rate, ratio).ok())
                .unwrap_or(65_536);
            (taps as f64 / per_in).ceil() as usize
        };
        let flush_frames = (latency as f64 / per_in).ceil() as usize + kernel_frames + 4;
        let mut remaining = flush_frames;
        while remaining > 0 {
            let frames = remaining.min(args.block);
            let produced = chain.output_frames(frames) * channels;
            chain.process_f32(&zeros[..frames * channels], &mut out_buf[..produced]);
            result.extend_from_slice(&out_buf[..produced]);
            remaining -= frames;
        }
        let drop = (latency * channels).min(result.len());
        result.drain(..drop);
        result.truncate(total_out);
    } else {
        result.truncate(total_out);
    }

    if passthrough {
        // The bypass copy has no quantizer stats of its own: it *is* the input.
        let mut input_stats = ClipStats::default();
        for &x in &wav.samples {
            let a = f64::from(x).abs();
            input_stats.peak = input_stats.peak.max(a);
            if a >= 1.0 {
                input_stats.clipped += 1;
            }
        }
        stats = input_stats;
    }

    let elapsed = started.elapsed().as_secs_f64();

    match container {
        Container::F32 => {
            let float: Vec<f32> = result.iter().map(|&s| wavio::i32_to_f32(s)).collect();
            wavio::write_wav_f32(&args.output, out_rate, channels, &float)?;
        }
        other => {
            wavio::write_wav_i32(&args.output, out_rate, channels, other, &result)?;
        }
    }

    println!(
        "input:  {} ({} Hz, {} ch, {}-bit {})",
        args.input.display(),
        wav.rate,
        channels,
        wav.source_bits,
        if wav.source_float { "float" } else { "integer" }
    );
    println!(
        "output: {} ({} Hz, {} ch, {}, {} target bits, dither {})",
        args.output.display(),
        out_rate,
        channels,
        container,
        out_bits,
        dither_name(dither)
    );
    println!(
        "frames: {} in, {} out (ratio {ratio}, latency {latency} out frames, {})",
        wav.frames(),
        result.len() / channels,
        if args.trim_latency { "trimmed" } else { "kept" }
    );
    println!("peak: {:.2} dBFS", crate::signal::peak_dbfs(stats.peak));
    println!("clipped: {}", stats.clipped);
    let audio_seconds = wav.frames() as f64 / f64::from(wav.rate);
    println!(
        "elapsed: {elapsed:.3} s, real-time factor: {:.2}x",
        realtime_factor(audio_seconds, elapsed)
    );
    Ok(0)
}

pub fn realtime_factor(audio_seconds: f64, elapsed: f64) -> f64 {
    if elapsed > 0.0 {
        audio_seconds / elapsed
    } else {
        f64::INFINITY
    }
}

pub fn dither_name(kind: DitherKind) -> &'static str {
    match kind {
        DitherKind::None => "none",
        DitherKind::Tpdf => "tpdf",
        DitherKind::Shaped => "shaped",
        DitherKind::HeadroomTpdf => "headroom_tpdf",
        DitherKind::Rectangular => "rectangular",
        DitherKind::Gaussian => "gaussian",
        DitherKind::HighpassTpdf => "highpass_tpdf",
        DitherKind::Feedback1 => "feedback1",
        DitherKind::Feedback5 => "feedback5",
        DitherKind::Resonant5 => "resonant5",
        DitherKind::Resonant9 => "resonant9",
        DitherKind::Integrator15 => "integrator15",
    }
}

/// Merge CLI flags over an optional JSON config, and pick the output format.
fn build_config(args: &ProcessArgs, wav: &WavData) -> Result<(ChainConfig, Container)> {
    let mut config = match &args.config {
        Some(path) => {
            let text = fs::read_to_string(path)
                .with_context(|| format!("cannot read config {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("cannot parse config {}", path.display()))?
        }
        // Without a config file nothing but what the flags ask for runs.
        None => ChainConfig::neutral(),
    };

    if let Some(path) = &args.eq {
        config.eq = Some(crate::profile::load(path)?);
    }
    if let Some(path) = &args.eq_right {
        let right = crate::profile::load(path)?;
        let eq = config.eq.get_or_insert_with(EqProfile::default);
        if args.eq.is_some() && eq.left.is_none() {
            eq.left = Some(ChannelEq {
                enabled: true,
                preamp_db: eq.preamp_db,
                bands: std::mem::take(&mut eq.bands),
            });
            eq.preamp_db = 0.0;
        }
        eq.right = Some(ChannelEq {
            enabled: true,
            preamp_db: right.preamp_db,
            bands: right.bands,
        });
    }
    if let Some(db) = args.preamp_db {
        config.eq.get_or_insert_with(EqProfile::default).preamp_db = db;
    }
    if args.auto_preamp {
        let profile = config.eq.as_ref().cloned().unwrap_or_default();
        let auto = Eq::auto_preamp_db(&profile, f64::from(wav.rate)).map_err(anyhow::Error::msg)?;
        let eq = config.eq.get_or_insert_with(EqProfile::default);
        eq.preamp_db = auto;
    }

    if !args.conv.is_empty() {
        let single_gain = if args.conv.len() == 1 && config.convolution.len() == 1 {
            config.convolution[0].gain_db
        } else {
            0.0
        };
        config.convolution = args
            .conv
            .iter()
            .map(|path| ConvolutionConfig {
                name: None,
                enabled: true,
                ir_path: path.clone(),
                gain_db: single_gain,
            })
            .collect();
    }

    if let Some(ratio) = args.upsample {
        config.upsampler.target = RateTarget::Ratio(ratio);
    }
    if let Some(max) = args.max_rate {
        config.upsampler.target = RateTarget::MaxRate(max);
    }
    if let Some(rate) = args.out_rate {
        config.upsampler.target = RateTarget::Rate(rate);
    }
    if args.taps.is_some() || args.phase.is_some() || args.beta.is_some() || args.cutoff.is_some() {
        let mut m = config
            .upsampler
            .design
            .as_ref()
            .and_then(|d| match &d.design {
                audivane_protocol::FilterDesign::Kernel(k)
                    if k.numerics.path == audivane_protocol::design::ArithmeticPath::Standard =>
                {
                    audivane_dsp_core::design::paths::standard::legacy_config(k, wav.rate).ok()
                }
                _ => None,
            })
            .unwrap_or_default();
        if let Some(taps) = args.taps {
            m.taps = taps;
        }
        if let Some(phase) = args.phase {
            m.phase = phase.into();
        }
        if let Some(beta) = args.beta {
            m.kaiser_beta = beta;
        }
        if let Some(cutoff) = args.cutoff {
            m.cutoff = cutoff;
        }
        if m.taps == 0 {
            bail!("upsampler taps must be at least 1");
        }
        if !(m.cutoff > 0.0 && m.cutoff <= 1.0) {
            bail!("upsampler cutoff must be in (0, 1], got {}", m.cutoff);
        }
        if !(m.kaiser_beta.is_finite() && m.kaiser_beta >= 0.0) {
            bail!(
                "upsampler beta must be a finite value >= 0, got {}",
                m.kaiser_beta
            );
        }
        let target = config.upsampler.target;
        config.upsampler = m
            .into_upsampler_config()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        config.upsampler.target = target;
    }

    let container: Container = match args.format {
        Some(format) => format.into(),
        None => ContainerArg::matching(wav.source_bits, wav.source_float).into(),
    };

    let default_bits = if container.is_float() {
        32
    } else {
        match container {
            Container::S16 => 16,
            _ => 24,
        }
    };
    if let Some(bits) = args.bits {
        config.output.bits = bits;
    } else if args.config.is_none() {
        config.output.bits = default_bits;
    }
    if !(16..=32).contains(&config.output.bits) {
        bail!(
            "dither target bits must be 16..=32, got {}",
            config.output.bits
        );
    }
    if !container.is_float() && config.output.bits > container.bits() {
        bail!(
            "--bits {} does not fit in --format {container} ({} bits)",
            config.output.bits,
            container.bits()
        );
    }

    if let Some(dither) = args.dither {
        config.output.dither = dither.into();
    } else if container.is_float() {
        config.output.dither = DitherKind::None;
    }

    Ok((config, container))
}

/// Load, resample, and combine all enabled impulse responses to the chain rate.
pub fn load_irs(config: &ChainConfig, rate: u32, channels: usize) -> Result<Option<Vec<Vec<f64>>>> {
    let enabled: Vec<&ConvolutionConfig> =
        config.convolution.iter().filter(|c| c.enabled).collect();
    if enabled.is_empty() {
        return Ok(None);
    }
    let mut loaded: Vec<(Vec<Vec<f64>>, f64)> = Vec::with_capacity(enabled.len());
    let mut lengths = Vec::with_capacity(enabled.len());
    for conv in enabled {
        let wav = wavio::read_wav(&conv.ir_path)?;
        if wav.channels != 1 && wav.channels != channels {
            bail!(
                "impulse response {} has {} channels; expected 1 or {channels}",
                conv.ir_path.display(),
                wav.channels
            );
        }
        let mut ir = Vec::with_capacity(wav.channels);
        for c in 0..wav.channels {
            let mono: Vec<f64> = (0..wav.frames())
                .map(|i| f64::from(wav.samples[i * wav.channels + c]))
                .collect();
            let mono = if wav.rate == rate {
                mono
            } else {
                resample_offline(&mono, wav.rate, rate)
            };
            if mono.is_empty() {
                bail!("impulse response {} is empty", conv.ir_path.display());
            }
            ir.push(mono);
        }
        lengths.push(ir.iter().map(Vec::len).max().unwrap_or(0));
        loaded.push((ir, conv.gain_db));
    }
    combined_ir_length(&lengths).map_err(|e| anyhow::anyhow!("{e}"))?;
    let pairs: Vec<(&[Vec<f64>], f64)> = loaded
        .iter()
        .map(|(ir, gain)| (ir.as_slice(), *gain))
        .collect();
    let combined = combine_irs(&pairs, channels).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Some(combined))
}

/// Build a chain config with just an EQ profile at `rate` (used by the level
/// matching commands).
pub fn profile_config(path: &Path) -> Result<ChainConfig> {
    let profile = crate::profile::load(path)?;
    Ok(ChainConfig {
        eq: Some(profile),
        ..ChainConfig::default()
    })
}
