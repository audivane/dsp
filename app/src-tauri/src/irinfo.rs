//! WAV header facts of an impulse response, for the Convolution view.

use serde::Serialize;
use std::path::Path;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct IrInfo {
    pub channels: u16,
    pub frames: u32,
    pub rate: u32,
}

/// Read only the header; `None` when the file is missing or not a WAV.
pub fn read(path: &Path) -> Option<IrInfo> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    Some(IrInfo {
        channels: spec.channels,
        frames: reader.duration(),
        rate: spec.sample_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_wav_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ir.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..(2 * 1000) {
            w.write_sample(0.0f32).unwrap();
        }
        w.finalize().unwrap();
        assert_eq!(
            read(&path),
            Some(IrInfo {
                channels: 2,
                frames: 1000,
                rate: 44_100
            })
        );
        assert_eq!(read(&dir.path().join("missing.wav")), None);
    }
}
