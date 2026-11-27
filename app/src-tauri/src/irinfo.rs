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
