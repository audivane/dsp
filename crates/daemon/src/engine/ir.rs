//! Impulse response WAV loader (`hound`) with offline arbitrary-rate
//! resampling (`audivane_dsp_core::resample::resample_offline`) and per-rate cache.

use crate::api::EngineError;
use audivane_dsp_core::resample::resample_offline;
use audivane_protocol::ipc::RpcError;
use hound::{SampleFormat, WavReader};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug)]
struct DecodedIr {
    native_rate: u32,
    channels: Vec<Vec<f64>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    mtime: Option<SystemTime>,
    target_rate: u32,
}

/// Off-RT impulse response loader and per-rate resampled cache.
#[derive(Debug, Default)]
pub struct IrCache {
    decoded: HashMap<(PathBuf, Option<SystemTime>), DecodedIr>,
    resampled: HashMap<CacheKey, Vec<Vec<f64>>>,
}

impl IrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all cached IRs (e.g. if path or gain/convolution settings are reset).
    pub fn clear(&mut self) {
        self.decoded.clear();
        self.resampled.clear();
    }

    /// Load a WAV impulse response from `path`, resample cleanly to `target_rate`
    /// if its file sample rate differs, and cache per `(path, mtime, target_rate)`.
    pub fn load_for_rate(
        &mut self,
        path: &Path,
        target_rate: u32,
    ) -> Result<Vec<Vec<f64>>, EngineError> {
        let mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
        let rate_key = CacheKey {
            path: path.to_path_buf(),
            mtime,
            target_rate,
        };
        if let Some(hit) = self.resampled.get(&rate_key) {
            return Ok(hit.clone());
        }

        let raw_key = (path.to_path_buf(), mtime);
        if !self.decoded.contains_key(&raw_key) {
            let decoded = decode_wav_ir(path)?;
            self.decoded.insert(raw_key.clone(), decoded);
        }
        let raw = self.decoded.get(&raw_key).expect("just inserted");

        let out_chans: Vec<Vec<f64>> = if raw.native_rate == target_rate {
            raw.channels.clone()
        } else {
            raw.channels
                .iter()
                .map(|ch| resample_offline(ch, raw.native_rate, target_rate))
                .collect()
        };

        self.resampled.insert(rate_key, out_chans.clone());
        Ok(out_chans)
    }
}

fn decode_wav_ir(path: &Path) -> Result<DecodedIr, EngineError> {
    let mut reader = WavReader::open(path).map_err(|e| {
        EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("failed to open IR '{}': {e}", path.display()),
        )
    })?;
    let spec = reader.spec();
    let ch_count = spec.channels as usize;
    if ch_count == 0 {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("IR '{}' has 0 channels", path.display()),
        ));
    }
    if spec.sample_rate == 0 {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("IR '{}' has 0 Hz sample rate", path.display()),
        ));
    }

    let mut channels = vec![Vec::<f64>::new(); ch_count];
    match spec.sample_format {
        SampleFormat::Float => {
            for (idx, sample) in reader.samples::<f32>().enumerate() {
                let s = sample.map_err(|e| {
                    EngineError::new(
                        RpcError::INVALID_PARAMS,
                        format!("failed reading float IR '{}': {e}", path.display()),
                    )
                })?;
                channels[idx % ch_count].push(f64::from(s));
            }
        }
        SampleFormat::Int => {
            let bits = spec.bits_per_sample.clamp(1, 32);
            let denom = if bits == 32 {
                2_147_483_648.0_f64
            } else {
                f64::from(1u32 << (bits - 1))
            };
            for (idx, sample) in reader.samples::<i32>().enumerate() {
                let s = sample.map_err(|e| {
                    EngineError::new(
                        RpcError::INVALID_PARAMS,
                        format!("failed reading int IR '{}': {e}", path.display()),
                    )
                })?;
                channels[idx % ch_count].push(f64::from(s) / denom);
            }
        }
    }

    if channels.iter().any(Vec::is_empty) {
        return Err(EngineError::new(
            RpcError::INVALID_PARAMS,
            format!("IR '{}' is empty", path.display()),
        ));
    }

    Ok(DecodedIr {
        native_rate: spec.sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{WavSpec, WavWriter};

    #[test]
    fn loads_and_resamples_wav_ir_with_rate_cache() {
        let dir = std::env::temp_dir().join(format!(
            "audivane_ir_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let wav_path = dir.join("ir48.wav");

        let spec = WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        {
            let mut w = WavWriter::create(&wav_path, spec).unwrap();
            for i in 0..240 {
                w.write_sample(if i == 100 { 1.0f32 } else { 0.0f32 })
                    .unwrap();
            }
            w.finalize().unwrap();
        }

        let mut cache = IrCache::new();
        let ir48 = cache.load_for_rate(&wav_path, 48_000).unwrap();
        assert_eq!(ir48.len(), 1);
        assert_eq!(ir48[0].len(), 240);
        assert!((ir48[0][100] - 1.0).abs() < 1e-12);

        let ir44 = cache.load_for_rate(&wav_path, 44_100).unwrap();
        assert_eq!(ir44.len(), 1);
        let expected_len = ((240u128 * 44_100 + 24_000) / 48_000) as usize;
        assert_eq!(ir44[0].len(), expected_len);
        // Cached second fetch matches identically.
        let ir44_again = cache.load_for_rate(&wav_path, 44_100).unwrap();
        assert_eq!(ir44, ir44_again);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
