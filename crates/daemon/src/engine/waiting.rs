//! Stand-in engine while the configured output device cannot be opened at
//! start-up (unplugged DAC, card renumbered after a reboot, ...).
//!
//! The daemon still answers on its socket, so the app and `avdsp ctl` can
//! show why nothing plays and let the user pick another device. The binary
//! watches for the device to appear (or the configured device to change) and
//! then restarts itself with the real engine.

use std::sync::Mutex;

use audivane_protocol::ipc::{
    AbSide, AbStatus, EngineConfig, EngineState, Meters, RpcError, SelfTestResult,
};

use crate::api::{Engine, EngineError, EngineSnapshot};

pub struct WaitingEngine {
    config: Mutex<EngineConfig>,
    /// Configs are validated as the running engine validates them (the
    /// convolution guards on both sides, the followers' chains), at the
    /// default rate: a config accepted while waiting is one the engine
    /// would accept.
    ir_cache: Mutex<super::ir::IrCache>,
    conv_warnings: Mutex<Vec<audivane_protocol::ipc::ConvWarning>>,
    /// Device the engine failed on, and why.
    start_device: String,
    reason: String,
}

impl WaitingEngine {
    /// `reason` is the user-facing error shown in `status.error`.
    pub fn new(config: EngineConfig, reason: String) -> Self {
        Self {
            start_device: config.device.clone(),
            config: Mutex::new(config),
            ir_cache: Mutex::new(super::ir::IrCache::new()),
            conv_warnings: Mutex::new(Vec::new()),
            reason,
        }
    }

    /// The device of the latest applied config.
    pub fn device(&self) -> String {
        self.lock().device.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EngineConfig> {
        self.config.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Why nothing plays: the start-up error, or, after the user picked
    /// another device that is not there either, that one is missing.
    fn reason(&self) -> String {
        let device = self.device();
        if device == self.start_device {
            self.reason.clone()
        } else {
            start_error_text(&device, false, "")
        }
    }

    fn not_running(&self) -> EngineError {
        EngineError::new(RpcError::BAD_STATE, self.reason())
    }
}

/// User-facing text for an engine that could not start on `device`.
pub fn start_error_text(device: &str, present: bool, err: &str) -> String {
    if present {
        format!("Cannot open output device {device}: {err}")
    } else {
        format!(
            "Output device {device} not found. Plug it in or pick another output in the \
             device list; the engine starts as soon as it is available."
        )
    }
}

impl Engine for WaitingEngine {
    fn load_conv(
        &self,
        convs: &[audivane_protocol::ConvolutionConfig],
        rate: u32,
    ) -> Result<Vec<audivane_dsp_core::matrix::LoadedConv>, EngineError> {
        let mut cache = self.ir_cache.lock().unwrap_or_else(|e| e.into_inner());
        super::load_conv_items(convs, &mut cache, rate)
    }

    fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
        let warnings = {
            let mut cache = self.ir_cache.lock().unwrap_or_else(|e| e.into_inner());
            super::validate_engine_config(cfg, &mut cache, cfg.rate.default_rate.max(8_000))?
        };
        *self.conv_warnings.lock().unwrap_or_else(|e| e.into_inner()) = warnings;
        *self.lock() = cfg.clone();
        Ok(())
    }

    fn conv_warnings(&self) -> Vec<audivane_protocol::ipc::ConvWarning> {
        self.conv_warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn ab_select(&self, _side: AbSide, _crossfade_ms: f64) -> Result<AbStatus, EngineError> {
        Err(self.not_running())
    }

    fn snapshot(&self) -> EngineSnapshot {
        let cfg = self.lock().clone();
        EngineSnapshot {
            state: EngineState::Error,
            device: cfg.device.clone(),
            error: Some(self.reason()),
            format: None,
            graph_rate: cfg.rate.default_rate,
            output_rate: 0,
            ratio: audivane_protocol::RateRatio::ONE,
            quantum: cfg.quantum,
            latency_ms: 0.0,
            filter_latency_ms: 0.0,
            conv_latency_ms: 0.0,
            xruns: 0,
            dsp_load: 0.0,
            dsp_active: false,
            active_sink: None,
            ab: None,
            volume: None,
            dsp_node: None,
            direct_node: None,
            frames_out: 0,
            chain_generation: 0,
            gaps: Default::default(),
            device_caps: None,
            effective_rates: Vec::new(),
            direct_rates: Vec::new(),
            playable_rates: Vec::new(),
            effective_default_rate: None,
            dither_bits: None,
            bypass_resampler: None,
            rt_load_max: 0.0,
            varispeed: None,
            waiting_for_device: false,
        }
    }

    fn set_varispeed(
        &self,
        cfg: audivane_protocol::ipc::VarispeedConfig,
    ) -> Result<(), EngineError> {
        self.lock().varispeed = cfg;
        Ok(())
    }

    fn meters(&self) -> Meters {
        Meters {
            peak_db: Vec::new(),
            rms_db: Vec::new(),
            clipped: 0,
            clipped_ch: Vec::new(),
            input_peak_db: Vec::new(),
            chain_peak_db: Vec::new(),
            xruns: 0,
            dsp_load: 0.0,
            limiter: None,
        }
    }

    fn selftest(&self) -> Result<SelfTestResult, EngineError> {
        Err(self.not_running())
    }

    fn release_device(&self) -> Result<(), EngineError> {
        Ok(())
    }

    fn acquire_device(&self) -> Result<(), EngineError> {
        Err(self.not_running())
    }

    fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A side B over the boost cap is refused while the engine waits for its
    /// device, as when it runs; and the master's own build (a rate change)
    /// holds both sides' exact IRs at that rate to the cap.
    #[test]
    fn a_boost_over_the_cap_is_refused_while_waiting_and_on_a_rebuild() {
        use audivane_protocol::ipc::{AbB, AbConfig};
        let dir = std::env::temp_dir().join(format!("audivane-waiting-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ir = |name: &str, peak: f32| {
            let p = dir.join(name);
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(&p, spec).unwrap();
            for i in 0..64 {
                w.write_sample(if i == 0 { peak } else { 0.0 }).unwrap();
            }
            w.finalize().unwrap();
            p
        };
        let conv = |path: std::path::PathBuf| audivane_protocol::ConvolutionConfig {
            name: None,
            enabled: true,
            ir_path: path,
            gain_db: 0.0,
            matrix: None,
            calibration: false,
        };
        let mut cfg = EngineConfig {
            device: "hw:CARD=PCH,DEV=0".into(),
            ..EngineConfig::default()
        };
        cfg.chain.convolution = vec![conv(ir("ok.wav", 1.0))];
        let engine = WaitingEngine::new(cfg.clone(), "missing".into());
        engine.apply(&cfg).unwrap();
        // +18 dB on side B.
        let mut loud = cfg.chain.clone();
        loud.convolution = vec![conv(ir("loud.wav", 7.94))];
        let mut with_b = cfg.clone();
        with_b.ab = Some(AbConfig {
            chain_b: loud.clone(),
            ..AbConfig::new(AbB::Chain(Box::new(loud)))
        });
        let e = engine.apply(&with_b).unwrap_err();
        assert!(
            e.message.contains("A/B side B") && e.message.contains("cap"),
            "{}",
            e.message
        );
        assert_eq!(engine.lock().ab, None, "kept the accepted config");
        // The master's build at another rate refuses it too (the fallback
        // then drops the convolution).
        let mut cache = super::super::ir::IrCache::new();
        let e = super::super::build_active_chain(
            &with_b,
            &mut cache,
            96_000,
            audivane_protocol::RateRatio::ONE,
            AbSide::A,
        )
        .err()
        .expect("refused");
        assert!(
            e.message.contains("A/B side B") && e.message.contains("cap"),
            "{}",
            e.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
