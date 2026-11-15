//! WAV input/output for `avdsp`.
//!
//! Reading goes through `hound` (it handles PCM, IEEE float and
//! WAVE_FORMAT_EXTENSIBLE). Writing is done by hand so the CLI controls the
//! exact container: packed 24-bit, no silent conversion to float.
//!
//! Integer samples are converted the way PipeWire does it: `n / 2^(bits-1)`.

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Output WAV container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Container {
    S16,
    S24,
    S32,
    F32,
}

impl Container {
    pub fn bits(self) -> u32 {
        match self {
            Self::S16 => 16,
            Self::S24 => 24,
            Self::S32 => 32,
            Self::F32 => 32,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32)
    }
}

impl std::fmt::Display for Container {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::S16 => "s16",
            Self::S24 => "s24",
            Self::S32 => "s32",
            Self::F32 => "f32",
        })
    }
}

/// A decoded WAV file.
pub struct WavData {
    pub rate: u32,
    pub channels: usize,
    /// Interleaved nominal samples in [-1, 1).
    pub samples: Vec<f32>,
    /// Left-justified i32 samples when the source was integer PCM. Used to
    /// prove bit-exactness; `None` for float files.
    pub ints: Option<Vec<i32>>,
    pub source_bits: u32,
    pub source_float: bool,
}

impl WavData {
    pub fn frames(&self) -> usize {
        self.samples.len().checked_div(self.channels).unwrap_or(0)
    }
}

/// Decode integer PCM as f32 (divided by `2^(bits-1)`, like PipeWire) and as
/// left-justified i32.
fn int_samples<T: hound::Sample + Into<i32>>(
    reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
    path: &Path,
    scale: f32,
    shift: u32,
) -> Result<(Vec<f32>, Option<Vec<i32>>)> {
    let raw: Vec<i32> = reader
        .samples::<T>()
        .map(|s| s.map(Into::into))
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("cannot decode samples from {}", path.display()))?;
    let samples = raw.iter().map(|v| *v as f32 / scale).collect();
    let ints = raw
        .iter()
        .map(|v| if shift == 0 { *v } else { *v << shift })
        .collect();
    Ok((samples, Some(ints)))
}

/// Read a WAV file into interleaved f32 plus the original integers.
pub fn read_wav(path: &Path) -> Result<WavData> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("cannot open WAV {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels == 0 {
        bail!("{} has no channels", path.display());
    }
    let channels = usize::from(spec.channels);
    let float = spec.sample_format == hound::SampleFormat::Float;
    let bits = spec.bits_per_sample;

    let (samples, ints): (Vec<f32>, Option<Vec<i32>>) = match (float, bits) {
        (false, 16) => int_samples::<i16>(&mut reader, path, 32_768.0, 16)?,
        (false, 24) => int_samples::<i32>(&mut reader, path, 8_388_608.0, 8)?,
        (false, 32) => int_samples::<i32>(&mut reader, path, 2_147_483_648.0, 0)?,
        (true, 32) => (
            reader
                .samples::<f32>()
                .collect::<std::result::Result<Vec<_>, _>>()
                .with_context(|| format!("cannot decode samples from {}", path.display()))?,
            None,
        ),
        _ => bail!(
            "{}: unsupported WAV format ({}-bit {}, only 16/24/32-bit integer and 32-bit float are supported)",
            path.display(),
            bits,
            if float { "float" } else { "integer" }
        ),
    };

    if !samples.len().is_multiple_of(channels) {
        bail!(
            "{}: {} samples is not a multiple of {} channels",
            path.display(),
            samples.len(),
            channels
        );
    }

    Ok(WavData {
        rate: spec.sample_rate,
        channels,
        samples,
        ints,
        source_bits: u32::from(bits),
        source_float: float,
    })
}

/// Write integer PCM from left-justified i32 samples.
pub fn write_wav_i32(
    path: &Path,
    rate: u32,
    channels: usize,
    container: Container,
    samples: &[i32],
) -> Result<()> {
    if container.is_float() {
        bail!("write_wav_i32 cannot write {container}");
    }
    if channels == 0 {
        bail!("cannot write a WAV with zero channels");
    }
    if !samples.len().is_multiple_of(channels) {
        bail!(
            "sample count {} is not a multiple of {} channels",
            samples.len(),
            channels
        );
    }
    let bits = container.bits();
    let bytes_per_sample = (bits / 8) as usize;
    let block_align = u16::try_from(bytes_per_sample * channels).context("too many channels")?;
    let byte_rate = u64::from(rate) * u64::from(block_align);
    let data_len = (samples.len() * bytes_per_sample) as u64;
    let pad = data_len % 2;
    let riff_len = 4 + (8 + 16) + (8 + data_len + pad);

    let file = File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut w = BufWriter::new(file);

    w.write_all(b"RIFF")?;
    w.write_all(&(riff_len as u32).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(
        &u16::try_from(channels)
            .context("too many channels")?
            .to_le_bytes(),
    )?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(
        &u32::try_from(byte_rate)
            .context("byte rate overflow")?
            .to_le_bytes(),
    )?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&u16::try_from(bits).context("bits overflow")?.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&(data_len as u32).to_le_bytes())?;

    match container {
        Container::S16 => {
            for &s in samples {
                w.write_all(&(s >> 16).to_le_bytes()[..2])?;
            }
        }
        Container::S24 => {
            for &s in samples {
                let b = (s >> 8).to_le_bytes();
                w.write_all(&b[..3])?;
            }
        }
        Container::S32 => {
            for &s in samples {
                w.write_all(&s.to_le_bytes())?;
            }
        }
        Container::F32 => unreachable!(),
    }
    if pad == 1 {
        w.write_all(&[0u8])?;
    }
    w.flush()?;
    Ok(())
}

/// Write 32-bit IEEE float, format tag 3.
pub fn write_wav_f32(path: &Path, rate: u32, channels: usize, samples: &[f32]) -> Result<()> {
    if channels == 0 {
        bail!("cannot write a WAV with zero channels");
    }
    if !samples.len().is_multiple_of(channels) {
        bail!(
            "sample count {} is not a multiple of {} channels",
            samples.len(),
            channels
        );
    }
    let bytes_per_sample = 4usize;
    let block_align = u16::try_from(bytes_per_sample * channels).context("too many channels")?;
    let byte_rate = u64::from(rate) * u64::from(block_align);
    let data_len = (samples.len() * bytes_per_sample) as u64;
    let riff_len = 4 + (8 + 16) + (8 + data_len);

    let file = File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut w = BufWriter::new(file);
    w.write_all(b"RIFF")?;
    w.write_all(&(riff_len as u32).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&3u16.to_le_bytes())?; // IEEE float
    w.write_all(
        &u16::try_from(channels)
            .context("too many channels")?
            .to_le_bytes(),
    )?;
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(
        &u32::try_from(byte_rate)
            .context("byte rate overflow")?
            .to_le_bytes(),
    )?;
    w.write_all(&block_align.to_le_bytes())?;
    w.write_all(&32u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&(data_len as u32).to_le_bytes())?;
    for &s in samples {
        w.write_all(&s.to_le_bytes())?;
    }
    w.flush()?;
    Ok(())
}

/// Left-justified i32 → nominal f32.
pub fn i32_to_f32(x: i32) -> f32 {
    x as f32 * (1.0 / 2_147_483_648.0)
}
