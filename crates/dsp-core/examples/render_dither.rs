//! Offline quantizer harness. Input: interleaved little-endian f64. Output:
//! interleaved little-endian, left-justified i32 (including 16/24-bit output).
use audivane_dsp_core::dither::Quantizer;
use audivane_protocol::{DitherKind, OutputConfig};
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufWriter, Write};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 7 {
        return Err(
            "usage: render_dither INPUT.f64 OUTPUT.i32 BITS KIND RATE CHANNELS SEED".into(),
        );
    }
    let bits: u32 = args[2].parse()?;
    if ![16, 24, 32].contains(&bits) {
        return Err("BITS must be 16, 24 or 32".into());
    }
    let kind = match args[3].as_str() {
        "none" => DitherKind::None,
        "tpdf" => DitherKind::Tpdf,
        "shaped" => DitherKind::Shaped,
        "headroom_tpdf" => DitherKind::HeadroomTpdf,
        "rectangular" => DitherKind::Rectangular,
        "gaussian" => DitherKind::Gaussian,
        "highpass_tpdf" => DitherKind::HighpassTpdf,
        "feedback1" => DitherKind::Feedback1,
        "feedback5" => DitherKind::Feedback5,
        "resonant5" => DitherKind::Resonant5,
        "resonant9" => DitherKind::Resonant9,
        "integrator15" => DitherKind::Integrator15,
        _ => return Err("unknown dither kind".into()),
    };
    let rate: u32 = args[4].parse()?;
    let channels: usize = args[5].parse()?;
    if !(1..=64).contains(&channels) {
        return Err("CHANNELS must be 1..=64".into());
    }
    let seed: u64 = args[6].parse()?;
    let data = fs::read(&args[0])?;
    if data.len() % (8 * channels) != 0 {
        return Err("input must contain whole frames".into());
    }
    let mut q = Quantizer::with_sample_rate(
        &OutputConfig {
            bits,
            dither: kind,
            headroom_db: 0.0,
        },
        channels,
        seed,
        rate,
    );
    let mut out = vec![0; 257 * channels];
    let mut planar = vec![vec![0.0; 257]; channels];
    let mut writer = BufWriter::new(File::create(&args[1])?);
    for block in data.chunks(257 * channels * 8) {
        let frames = block.len() / (8 * channels);
        for (i, sample) in block.as_chunks::<8>().0.iter().enumerate() {
            planar[i % channels][i / channels] = f64::from_le_bytes(*sample);
        }
        let slices: Vec<_> = planar.iter().map(|ch| &ch[..frames]).collect();
        q.process(&slices, &mut out[..frames * channels]);
        for sample in &out[..frames * channels] {
            writer.write_all(&sample.to_le_bytes())?;
        }
    }
    writer.flush()?;
    Ok(())
}
