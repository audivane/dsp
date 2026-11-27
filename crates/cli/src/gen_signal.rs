//! `avdsp gen`: deterministic test signals.

use crate::signal::Rng;
use crate::wavio::{self, Container};
use crate::{GenArgs, GenKind};
use anyhow::{Result, bail};
use std::f64::consts::PI;

pub fn run(args: GenArgs) -> Result<i32> {
    if !(args.seconds.is_finite() && args.seconds > 0.0) {
        bail!("--seconds must be greater than zero");
    }
    if args.rate == 0 {
        bail!("--rate must be greater than zero");
    }
    let channels = args.channels.max(1);
    let frames = (args.seconds * f64::from(args.rate)).round() as usize;
    if frames == 0 {
        bail!("--seconds is too short to produce a sample");
    }
    let amp = 10f64.powf(args.amp_db / 20.0);
    let nyquist = f64::from(args.rate) / 2.0;
    let mut rng = Rng::new(args.seed);
    let mut signal = vec![0.0f64; frames * channels];

    for (frame, slot) in signal.chunks_mut(channels).enumerate() {
        let t = frame as f64 / f64::from(args.rate);
        for sample in slot {
            *sample = match args.kind {
                GenKind::Silence => 0.0,
                GenKind::Sine => {
                    let f = args.freq.min(nyquist * 0.999);
                    amp * (2.0 * PI * f * t).sin()
                }
                GenKind::White => amp * rng.next_bipolar(),
                GenKind::Sweep => {
                    let f0 = args.freq.max(1.0).min(nyquist * 0.5);
                    let f1 = args.freq_end.min(nyquist * 0.98).max(f0 * 1.000_001);
                    let k = f1 / f0;
                    let total = frames as f64 / f64::from(args.rate);
                    let phase = 2.0 * PI * f0 * total / k.ln()
                        * (k.powf(frame as f64 / frames as f64) - 1.0);
                    amp * phase.sin()
                }
            };
        }
    }

    let container = match args.bits {
        16 => Container::S16,
        24 => Container::S24,
        32 => Container::S32,
        other => bail!("--bits must be 16, 24 or 32, got {other}"),
    };
    let shift = 32 - args.bits;
    let scale = 2f64.powi(args.bits as i32 - 1);
    let ints: Vec<i32> = signal
        .iter()
        .map(|&x| {
            let q = (x * scale).round().clamp(-scale, scale - 1.0) as i32;
            q << shift
        })
        .collect();
    wavio::write_wav_i32(&args.out, args.rate, channels, container, &ints)?;

    println!(
        "wrote {}: {:?} at {} Hz, {} ch, {}-bit, {} frames, {:.3} s, amplitude {:.2} dBFS",
        args.out.display(),
        args.kind,
        args.rate,
        channels,
        args.bits,
        frames,
        frames as f64 / f64::from(args.rate),
        args.amp_db
    );
    Ok(0)
}
