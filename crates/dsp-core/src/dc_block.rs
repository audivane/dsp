//! DC blocking (high-pass) stage.
//!
//! A 2nd-order Butterworth high-pass filter (`Q = 1/sqrt(2)`) designed via the
//! prewarped bilinear transform (RBJ Audio EQ Cookbook) in `f64`, placed as the
//! first stage in the chain at the source sample rate.
//!
//! Processing uses **Direct Form I**
//! (`y[n] = b0 x[n] + b1 x[n-1] + b2 x[n-2] - a1 y[n-1] - a2 y[n-2]`).
//! For a 1 Hz cutoff at 768 kHz, coefficients sit within ~`1e-5` of unity
//! (`a1 ≈ -2`, `a2 ≈ 1`); Direct Form I cancels DC directly in the feedforward
//! zeros (`b1 = -2 b0`, `b2 = b0`) before the poles, so the settled DC floor
//! stays around `1e-23` where transposed Direct Form II leaves ~`3e-10`, at
//! the same passband and corner accuracy.
//!
//! **Headroom:** the bounded-input sample-peak maximum (L1 norm) of this
//! 2nd-order Butterworth high-pass is `‖h‖₁ ≈ 2.4345` (+7.73 dB across
//! 1–20 Hz and 44.1–768 kHz, reached only by a sign-matched worst-case input).
//! This stage adds no automatic gain compensation; the limiter or EQ preamp
//! handles headroom if needed.
//!
//! **Latency:** zero frames (minimum-phase IIR).

use crate::biquad::Coeffs;
use crate::{Complex64, Stage};
use audivane_protocol::{
    Band, DC_BLOCK_CUTOFF_MAX_HZ, DC_BLOCK_CUTOFF_MIN_HZ, DcBlockConfig, FilterType,
};
use std::fmt;

/// Why a [`DcBlock`] stage could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DcBlockError {
    ZeroChannels,
    InvalidSampleRate,
    InvalidCutoff(String),
}

impl fmt::Display for DcBlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => write!(f, "dc_block requires at least 1 channel"),
            Self::InvalidSampleRate => write!(f, "dc_block requires a positive sample rate"),
            Self::InvalidCutoff(msg) => write!(f, "dc_block: {msg}"),
        }
    }
}

impl std::error::Error for DcBlockError {}

/// Per-sample denormal flush threshold (`2^-100` ≈ -602 dBFS, matching
/// `presets::cascade::iir::FLUSH_BELOW`). When the feedback history and current
/// output are all below this magnitude, the recursive state is snapped to exact
/// `0.0` independently of the feedforward input history `x1, x2` so a decaying
/// tail never enters IEEE 754 subnormals even during held DC, while staying
/// block-size independent.
const FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31; // 2^-100

/// Four-word Direct Form I biquad state per channel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Df1State {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Df1State {
    #[inline]
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        if self.y1.abs() < FLUSH_BELOW && self.y2.abs() < FLUSH_BELOW && y.abs() < FLUSH_BELOW {
            self.y1 = 0.0;
            self.y2 = 0.0;
            return 0.0;
        }
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// 2nd-order Butterworth DC-blocking high-pass filter.
#[derive(Clone, Debug)]
pub struct DcBlock {
    enabled: bool,
    cutoff_hz: f64,
    sample_rate: f64,
    coeffs: Coeffs,
    states: Vec<Df1State>,
}

impl DcBlock {
    /// Design a 2nd-order Butterworth high-pass DC blocker at `sample_rate` for
    /// `channels`.
    pub fn try_new(
        config: &DcBlockConfig,
        sample_rate: f64,
        channels: usize,
    ) -> Result<Self, DcBlockError> {
        if channels == 0 {
            return Err(DcBlockError::ZeroChannels);
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(DcBlockError::InvalidSampleRate);
        }
        if !config.cutoff_hz.is_finite()
            || !(DC_BLOCK_CUTOFF_MIN_HZ..=DC_BLOCK_CUTOFF_MAX_HZ).contains(&config.cutoff_hz)
        {
            return Err(DcBlockError::InvalidCutoff(format!(
                "cutoff_hz must be between {DC_BLOCK_CUTOFF_MIN_HZ} and {DC_BLOCK_CUTOFF_MAX_HZ} Hz (got {})",
                config.cutoff_hz
            )));
        }
        if config.cutoff_hz >= 0.5 * sample_rate {
            return Err(DcBlockError::InvalidCutoff(format!(
                "cutoff_hz ({} Hz) must be below Nyquist ({} Hz)",
                config.cutoff_hz,
                0.5 * sample_rate
            )));
        }

        let coeffs = Coeffs::design(
            &Band {
                kind: FilterType::HighPass,
                freq_hz: config.cutoff_hz,
                gain_db: 0.0,
                q: std::f64::consts::FRAC_1_SQRT_2,
                enabled: true,
            },
            sample_rate,
        );

        Ok(Self {
            enabled: config.enabled,
            cutoff_hz: config.cutoff_hz,
            sample_rate,
            coeffs,
            states: vec![Df1State::default(); channels],
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn cutoff_hz(&self) -> f64 {
        self.cutoff_hz
    }

    pub fn coeffs(&self) -> Coeffs {
        self.coeffs
    }

    /// Complex frequency response at `f_hz` (unity when disabled).
    pub fn response(&self, f_hz: f64) -> Complex64 {
        if self.enabled {
            self.coeffs.response(f_hz, self.sample_rate)
        } else {
            Complex64::new(1.0, 0.0)
        }
    }

    /// How long the filter's start-up rings, frames (`fs / (2 pi fc)`, the
    /// time constant of its corner); 0 when off.
    pub fn decay_frames(&self) -> f64 {
        if self.enabled && self.cutoff_hz > 0.0 {
            self.sample_rate / (2.0 * std::f64::consts::PI * self.cutoff_hz)
        } else {
            0.0
        }
    }

    /// Copy the four-word filter history per channel from `from` when both
    /// stages are enabled with the same cutoff, sample rate, and channel count.
    /// Returns `true` when state was carried over. Allocation-free.
    pub fn inherit_state(&mut self, from: &DcBlock) -> bool {
        if !self.enabled
            || !from.enabled
            || self.cutoff_hz != from.cutoff_hz
            || self.sample_rate != from.sample_rate
            || self.states.len() != from.states.len()
        {
            return false;
        }
        self.states.copy_from_slice(&from.states);
        true
    }
}

impl Stage for DcBlock {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if !self.enabled {
            return;
        }
        let nch = chans.len().min(self.states.len());
        let c = self.coeffs;
        for (chan, state) in chans.iter_mut().take(nch).zip(self.states.iter_mut()) {
            for s in chan.iter_mut() {
                // Non-finite input would poison the recursive state forever.
                let x = if s.is_finite() { *s } else { 0.0 };
                *s = state.step(&c, x);
            }
        }
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn reset(&mut self) {
        self.states.fill(Df1State::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::lin_to_db;
    use std::f64::consts::PI;

    const RATES: [f64; 5] = [44_100.0, 48_000.0, 96_000.0, 192_000.0, 768_000.0];

    fn cfg(cutoff_hz: f64) -> DcBlockConfig {
        DcBlockConfig {
            enabled: true,
            cutoff_hz,
        }
    }

    /// Measure steady-state magnitude of `DcBlock` at `f_hz` by running a sine
    /// long enough for the startup transient to decay (< 1e-12) and projecting
    /// the last integer cycles onto `sin` and `cos`.
    fn measure_sine_mag(cutoff_hz: f64, f_hz: f64, fs: f64) -> f64 {
        let mut stage = DcBlock::try_new(&cfg(cutoff_hz), fs, 1).unwrap();
        // Transient envelope decays as sqrt(2) * exp(-2 pi f_c t / sqrt(2)).
        // At t = 7 / f_c, envelope < 4e-14.
        let settle_frames = ((7.0 / cutoff_hz) * fs).ceil() as usize;
        let cycle_frames = ((2.0 * fs / f_hz).round() as usize).max(16);
        let total = settle_frames + cycle_frames;
        let w = 2.0 * PI * f_hz / fs;

        let block = 4096;
        let mut buf = vec![0.0; block];
        let mut re = 0.0;
        let mut im = 0.0;
        let mut idx = 0;
        while idx < total {
            let n = (total - idx).min(block);
            for (k, slot) in buf[..n].iter_mut().enumerate() {
                *slot = (w * (idx + k) as f64).sin();
            }
            stage.process(&mut [&mut buf[..n]]);
            for (k, &y) in buf[..n].iter().enumerate() {
                let pos = idx + k;
                if pos >= settle_frames {
                    let theta = w * pos as f64;
                    re += y * theta.sin();
                    im += y * theta.cos();
                }
            }
            idx += n;
        }
        2.0 * re.hypot(im) / (cycle_frames as f64)
    }

    #[test]
    fn cutoff_and_passband_magnitude_across_rates() {
        for fs in RATES {
            for cutoff in [1.0, 5.0, 20.0] {
                let stage = DcBlock::try_new(&cfg(cutoff), fs, 1).unwrap();
                let at_fc = lin_to_db(stage.response(cutoff).norm());
                assert!(
                    (at_fc + 3.0103).abs() < 0.05,
                    "fs={fs} cutoff={cutoff}: analytic at fc = {at_fc} dB"
                );
                assert!(stage.response(0.0).norm() < 1e-12);
                assert!(lin_to_db(stage.response(0.49 * fs).norm()).abs() < 0.001);

                let meas_fc = lin_to_db(measure_sine_mag(cutoff, cutoff, fs));
                assert!(
                    (meas_fc + 3.0103).abs() < 0.05,
                    "fs={fs} cutoff={cutoff}: measured at fc = {meas_fc} dB"
                );
            }

            // Default 5 Hz cutoff: exact Butterworth at 20 Hz is -0.01693 dB
            // (flat within 0.02 dB from 20 Hz up, and within 0.01 dB from 23 Hz up).
            let stage5 = DcBlock::try_new(&cfg(5.0), fs, 1).unwrap();
            for f in [20.0, 23.0, 25.0, 50.0, 100.0, 1000.0, 20_000.0] {
                let db = lin_to_db(stage5.response(f).norm());
                let expected = -10.0 * (1.0 + (5.0 / f).powi(4)).log10();
                assert!(
                    (db - expected).abs() < 1e-4,
                    "fs={fs} f={f}: {db} vs {expected}"
                );
                if f >= 23.0 {
                    assert!(db.abs() < 0.01, "fs={fs} f={f}: {db} dB");
                } else {
                    assert!(db.abs() < 0.02, "fs={fs} f={f}: {db} dB");
                }
            }
            let meas_20 = lin_to_db(measure_sine_mag(5.0, 20.0, fs));
            assert!(
                (meas_20 + 0.01693).abs() < 0.001,
                "fs={fs}: measured at 20 Hz = {meas_20} dB"
            );
            let meas_25 = lin_to_db(measure_sine_mag(5.0, 25.0, fs));
            assert!(
                meas_25.abs() < 0.01,
                "fs={fs}: measured at 25 Hz = {meas_25} dB"
            );
        }
    }

    #[test]
    fn off_is_bit_exact() {
        let off = DcBlockConfig {
            enabled: false,
            cutoff_hz: 5.0,
        };
        let mut stage = DcBlock::try_new(&off, 48_000.0, 2).unwrap();
        let orig_l = vec![0.5, -0.25, 1.0, 0.123456789, -0.99999];
        let orig_r = vec![-0.5, 0.75, 0.0, -0.987654321, 0.5];
        let mut l = orig_l.clone();
        let mut r = orig_r.clone();
        stage.process(&mut [&mut l, &mut r]);
        assert_eq!(
            l.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            orig_l.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(
            r.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            orig_r.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn silence_after_impulse_flushes_state_to_exact_zero_and_matches_across_block_sizes() {
        let fs = 48_000.0;
        let total = 48_000 * 35; // 35 s: past ~31 s subnormal onset at 5 Hz / 48 kHz
        let mut stage_ref = DcBlock::try_new(&cfg(5.0), fs, 1).unwrap();
        let mut buf = vec![0.0; 4096];
        buf[0] = 1.0;
        stage_ref.process(&mut [&mut buf]);
        let mut pos = 4096;
        while pos < total {
            let n = (total - pos).min(4096);
            buf[..n].fill(0.0);
            stage_ref.process(&mut [&mut buf[..n]]);
            // During silence decay, recursive state must never become subnormal.
            for st in &stage_ref.states {
                assert!(!st.y1.is_subnormal(), "y1 went subnormal: {:e}", st.y1);
                assert!(!st.y2.is_subnormal(), "y2 went subnormal: {:e}", st.y2);
            }
            pos += n;
        }
        assert_eq!(stage_ref.states[0], Df1State::default());

        // Block sizes 1, 7, 4096 must still produce bit-identical output across the
        // flush boundary (3.5 s covers the ~3.12 s decay to FLUSH_BELOW at 5 Hz).
        let short_total = 48_000 * 4;
        let mut reference = Vec::with_capacity(short_total);
        let mut s_full = DcBlock::try_new(&cfg(5.0), fs, 1).unwrap();
        let mut remaining = short_total;
        let mut first = true;
        while remaining > 0 {
            let n = remaining.min(4096);
            buf[..n].fill(0.0);
            if first {
                buf[0] = 1.0;
                first = false;
            }
            s_full.process(&mut [&mut buf[..n]]);
            reference.extend_from_slice(&buf[..n]);
            remaining -= n;
        }
        assert_eq!(s_full.states[0], Df1State::default());

        for block in [1usize, 7, 4096] {
            let mut s = DcBlock::try_new(&cfg(5.0), fs, 1).unwrap();
            let mut out = Vec::with_capacity(short_total);
            let mut chunk = vec![0.0; block];
            let mut idx = 0;
            while idx < short_total {
                let n = (short_total - idx).min(block);
                chunk[..n].fill(0.0);
                if idx == 0 {
                    chunk[0] = 1.0;
                }
                s.process(&mut [&mut chunk[..n]]);
                out.extend_from_slice(&chunk[..n]);
                idx += n;
            }
            assert_eq!(out, reference, "block={block}");
            assert_eq!(s.states[0], Df1State::default(), "block={block}");
        }
    }
}
