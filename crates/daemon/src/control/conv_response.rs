//! `conv_response`: the frequency response of a chain's convolution for the
//! app's plot, from the IRs the engine plays (loaded through the engine's
//! IR cache at the graph rate, every gain applied, combined by
//! [`audivane_dsp_core::matrix::combine_conv`] as the engine combines them).
//! Off the audio thread and the mutation lock; the curves are kept for the
//! last few loaded IR sets so a redraw is cheap.

use std::sync::{Arc, Mutex};

use audivane_dsp_core::conv_response::{Curve, log_grid, output_curves, response};
use audivane_dsp_core::db_to_lin;
use audivane_dsp_core::matrix::{LoadedConv, combine_conv};
use audivane_protocol::ChainConfig;
use audivane_protocol::ConvolutionConfig;
use audivane_protocol::ipc::{
    AbSide, ConvResponse, ConvResponseCurve, ConvResponseItem, EngineConfig,
};

/// The plot's grid: 240 log steps over 20 Hz–20 kHz (the app's overlay
/// grid), up to Nyquist.
const GRID_LO: f64 = 20.0;
const GRID_HI: f64 = 20_000.0;
const GRID_STEPS: usize = 240;
/// How many computed IR sets [`ResponseCache`] keeps.
const KEPT: usize = 4;

/// The chain a call measures: chain A of the master by default, A/B side
/// B's, or follower `device`'s own (its patch applied, as the engine and
/// admission build it). Also the side and follower named in the reply.
pub fn chain_for(
    cfg: &EngineConfig,
    side: Option<AbSide>,
    device: Option<&str>,
) -> Result<(ChainConfig, AbSide, Option<String>), String> {
    let side = side.unwrap_or_default();
    let device = device.filter(|d| !d.is_empty() && *d != cfg.device);
    match (side, device) {
        (AbSide::B, Some(_)) => Err("A/B side B is the master's; a follower has no side B".into()),
        (AbSide::B, None) => cfg
            .ab
            .as_ref()
            .map(|ab| (ab.chain_b.clone(), AbSide::B, None))
            .ok_or_else(|| "A/B is not configured".into()),
        (AbSide::A, Some(d)) => {
            let f = cfg
                .outputs
                .followers
                .iter()
                .find(|f| f.device == d)
                .ok_or_else(|| format!("no follower {d:?}"))?;
            let own = super::follower_chain::config(cfg, f);
            Ok((own.chain, AbSide::A, Some(d.to_string())))
        }
        (AbSide::A, None) => Ok((cfg.chain.clone(), AbSide::A, None)),
    }
}

/// An item's label: its `name`, else its file's name.
fn label(c: &ConvolutionConfig) -> String {
    if let Some(name) = c.name.as_deref().filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    if c.matrix.is_some() {
        return "Filter matrix".into();
    }
    c.ir_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| c.ir_path.display().to_string())
}

/// The curves of one loaded IR set, before the labels.
#[derive(Debug)]
pub struct Computed {
    freqs_hz: Vec<f32>,
    combined_taps: u64,
    /// Per item: its longest IR, taps, and its curves.
    items: Vec<(u64, Vec<ConvResponseCurve>)>,
    outputs: Vec<ConvResponseCurve>,
}

fn wire(c: Curve, input: Option<usize>, output: Option<usize>) -> ConvResponseCurve {
    let round = |v: f64, q: f64| ((v * q).round() / q) as f32;
    ConvResponseCurve {
        input: input.map(|i| i as u32),
        output: output.map(|o| o as u32),
        mag_db: c.mag_db.iter().map(|&m| round(m, 1e3)).collect(),
        group_delay_ms: c
            .group_delay_ms
            .iter()
            .map(|d| d.map(|d| round(d, 1e4)))
            .collect(),
    }
}

/// The curves of `loaded` (a chain's enabled items, at `rate`) over
/// `channels` outputs.
pub fn compute(loaded: &[LoadedConv], rate: u32, channels: usize) -> Result<Computed, String> {
    let r = f64::from(rate);
    let freqs = log_grid(GRID_LO, GRID_HI, GRID_STEPS, r);
    let items = loaded
        .iter()
        .map(|item| match item {
            LoadedConv::Channels { irs, gain_db } => {
                let g = db_to_lin(*gain_db);
                let taps = irs.iter().map(Vec::len).max().unwrap_or(0) as u64;
                let curves = if irs.len() == 1 {
                    vec![wire(response(&[(&irs[0], g)], r, &freqs), None, None)]
                } else {
                    irs.iter()
                        .enumerate()
                        .map(|(c, ir)| wire(response(&[(ir, g)], r, &freqs), Some(c), Some(c)))
                        .collect()
                };
                (taps, curves)
            }
            LoadedConv::Matrix { taps, gain_db } => {
                let len = taps.iter().map(|t| t.ir.len()).max().unwrap_or(0) as u64;
                let curves = taps
                    .iter()
                    .map(|t| {
                        let g = db_to_lin(t.gain_db + gain_db);
                        wire(
                            response(&[(&t.ir, g)], r, &freqs),
                            Some(t.input),
                            Some(t.output),
                        )
                    })
                    .collect();
                (len, curves)
            }
        })
        .collect();
    let (combined_taps, outputs) = if loaded.is_empty() {
        (0, Vec::new())
    } else {
        let parts: Vec<_> = loaded.iter().map(LoadedConv::as_item).collect();
        let irs = combine_conv(&parts, channels).map_err(|e| e.to_string())?;
        let outputs = output_curves(&irs, r, channels, &freqs)
            .into_iter()
            .enumerate()
            .map(|(o, c)| wire(c, None, Some(o)))
            .collect();
        (irs.max_len() as u64, outputs)
    };
    Ok(Computed {
        freqs_hz: freqs.iter().map(|&f| f as f32).collect(),
        combined_taps,
        items,
        outputs,
    })
}

/// The reply for `chain`'s items, from their curves.
pub fn reply(
    chain: &ChainConfig,
    computed: &Computed,
    rate: u32,
    side: AbSide,
    device: Option<String>,
) -> ConvResponse {
    let items = chain
        .convolution
        .iter()
        .enumerate()
        .filter(|(_, c)| c.enabled)
        .zip(&computed.items)
        .map(|((index, c), (taps, curves))| ConvResponseItem {
            index: index as u32,
            label: label(c),
            calibration: c.calibration,
            taps: *taps,
            curves: curves.clone(),
        })
        .collect();
    ConvResponse {
        rate_hz: rate,
        side,
        device,
        freqs_hz: computed.freqs_hz.clone(),
        combined_taps: computed.combined_taps,
        items,
        outputs: computed.outputs.clone(),
    }
}

/// A loaded IR set, its rate and its curves.
type CacheEntry = (Vec<LoadedConv>, u32, Arc<Computed>);

/// The curves of the last few loaded IR sets, newest first.
#[derive(Default)]
pub struct ResponseCache {
    entries: Mutex<Vec<CacheEntry>>,
}

impl ResponseCache {
    /// The curves of `loaded` at `rate`, computed when not kept.
    pub fn get(
        &self,
        loaded: Vec<LoadedConv>,
        rate: u32,
        channels: usize,
    ) -> Result<Arc<Computed>, String> {
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(k) = entries
                .iter()
                .position(|(l, r, _)| *r == rate && *l == loaded)
            {
                let hit = entries.remove(k);
                let out = Arc::clone(&hit.2);
                entries.insert(0, hit);
                return Ok(out);
            }
        }
        // Computed without the lock: a call for another chain need not wait.
        let computed = Arc::new(compute(&loaded, rate, channels)?);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(0, (loaded, rate, Arc::clone(&computed)));
        entries.truncate(KEPT);
        Ok(computed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ir::IrCache;
    use crate::engine::load_conv_items;

    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::path::{Path, PathBuf};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "audivane_conv_response_{name}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, rate: u32, channels: &[Vec<f64>]) {
        let spec = WavSpec {
            channels: channels.len() as u16,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut w = WavWriter::create(path, spec).unwrap();
        for k in 0..channels[0].len() {
            for ch in channels {
                w.write_sample(ch[k] as f32).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn item(path: &Path, gain_db: f64) -> ConvolutionConfig {
        ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: path.to_path_buf(),
            gain_db,
            matrix: None,
            calibration: false,
        }
    }

    fn run(
        cfg: &EngineConfig,
        side: Option<AbSide>,
        device: Option<&str>,
        rate: u32,
    ) -> ConvResponse {
        let (chain, side, device) = chain_for(cfg, side, device).unwrap();
        let loaded = load_conv_items(&chain.convolution, &mut IrCache::new(), rate).unwrap();
        let computed = compute(&loaded, rate, 2).unwrap();
        reply(&chain, &computed, rate, side, device)
    }

    #[test]
    fn a_48k_ir_at_a_96k_graph_keeps_its_gain() {
        let d = dir("rate");
        // +10 dB, a short smooth pulse (band-limited enough to resample).
        let g = 10f64.powf(10.0 / 20.0);
        let taps: Vec<f64> = (0..9)
            .map(|k| (std::f64::consts::PI * (k as f64 + 1.0) / 10.0).sin())
            .collect();
        let sum: f64 = taps.iter().sum();
        let mut h = vec![0.0; 512];
        for (k, t) in taps.iter().enumerate() {
            h[100 + k] = g * t / sum;
        }
        let p = d.join("ir48.wav");
        write(&p, 48_000, &[h]);
        let mut cfg = EngineConfig::default();
        cfg.chain.convolution = vec![item(&p, 0.0)];
        let at48 = run(&cfg, None, None, 48_000);
        let at96 = run(&cfg, None, None, 96_000);
        assert_eq!(at96.rate_hz, 96_000);
        assert!(at96.combined_taps > at48.combined_taps);
        // The same response in the band, ~+10 dB at the bottom.
        assert!((f64::from(at96.outputs[0].mag_db[0]) - 10.0).abs() < 0.01);
        for (i, f) in at48.freqs_hz.iter().enumerate() {
            if *f > 10_000.0 {
                break;
            }
            let (a, b) = (at48.outputs[0].mag_db[i], at96.outputs[0].mag_db[i]);
            assert!((a - b).abs() < 0.02, "{f} Hz: {a} vs {b}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
