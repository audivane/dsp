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
    fn apply(&self, cfg: &EngineConfig) -> Result<(), EngineError> {
        *self.lock() = cfg.clone();
        Ok(())
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

    #[test]
    fn reports_the_missing_device_and_follows_config_edits() {
        let mut cfg = EngineConfig {
            device: "hw:CARD=USB,DEV=0".into(),
            ..Default::default()
        };
        let reason = start_error_text(&cfg.device, false, "No such device");
        let engine = WaitingEngine::new(cfg.clone(), reason);
        let snap = engine.snapshot();
        assert_eq!(snap.state, EngineState::Error);
        let error = snap.error.unwrap();
        assert!(error.contains("hw:CARD=USB,DEV=0 not found"), "{error}");
        assert!(engine.selftest().is_err());

        cfg.device = "hw:CARD=PCH,DEV=0".into();
        engine.apply(&cfg).unwrap();
        assert_eq!(engine.device(), "hw:CARD=PCH,DEV=0");
        let snap = engine.snapshot();
        assert_eq!(snap.device, "hw:CARD=PCH,DEV=0");
        assert!(snap.error.unwrap().contains("hw:CARD=PCH,DEV=0 not found"));
    }
}
