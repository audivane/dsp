//! Allocation-free, lock-free ~10 ms ramped gain and mute stages for the RT path.
//!
//! Uses a raised-cosine ramp (`0.5 - 0.5 * cos(pi * t)`) so gain, balance,
//! and mute transitions have zero slope discontinuity and zero zipper noise.

use crate::bypass::f32_to_i32;
use crate::db_to_lin;
use std::f64::consts::PI;

/// Target ramp duration in seconds (~10 ms).
pub const RAMP_SECONDS: f64 = 0.010;

/// Ramp length in frames at `sample_rate`.
#[inline]
pub fn ramp_frames_for_rate(sample_rate: u32) -> usize {
    ((f64::from(sample_rate.max(8_000))) * RAMP_SECONDS)
        .round()
        .max(1.0) as usize
}

/// Linear `[left, right]` gains for `balance_db`.
///
/// - `balance_db < 0`: attenuates the right channel by `|balance_db|` dB.
/// - `balance_db > 0`: attenuates the left channel by `balance_db` dB.
/// - `balance_db == 0`: `[1.0, 1.0]`.
#[inline]
pub fn balance_gains(balance_db: f64) -> [f64; 2] {
    if balance_db > 0.0 {
        [db_to_lin(-balance_db), 1.0]
    } else if balance_db < 0.0 {
        [1.0, db_to_lin(balance_db)]
    } else {
        [1.0, 1.0]
    }
}

/// Per-channel (`[L, R, rest]`) ramped gain for the DSP path, applied in `f64`
/// immediately before quantization/dither.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoGainRamp {
    current: [f64; 3],
    from: [f64; 3],
    to: [f64; 3],
    pos: usize,
    len: usize,
    sample_rate: u32,
}

impl StereoGainRamp {
    /// Settled unity (`[1.0, 1.0, 1.0]`) at `sample_rate`.
    pub fn unity(sample_rate: u32) -> Self {
        Self {
            current: [1.0, 1.0, 1.0],
            from: [1.0, 1.0, 1.0],
            to: [1.0, 1.0, 1.0],
            pos: 0,
            len: 0,
            sample_rate: sample_rate.max(8_000),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn current(&self) -> [f64; 3] {
        self.current
    }

    pub fn target(&self) -> [f64; 3] {
        self.to
    }

    /// Update the output sample rate (preserving in-progress ramp fraction).
    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let sr = sample_rate.max(8_000);
        if self.sample_rate == sr {
            return;
        }
        self.sample_rate = sr;
        if self.pos < self.len && self.len > 0 {
            let frac = (self.pos as f64) / (self.len as f64);
            let new_len = ramp_frames_for_rate(sr);
            self.len = new_len;
            self.pos = ((frac * (new_len as f64)).round() as usize).min(new_len);
            if self.pos >= self.len {
                self.current = self.to;
            }
        }
    }

    /// Set raw linear targets `[L, R, rest]`. When `ramp` is false, snaps
    /// immediately without interpolation (used when playback is idle).
    pub fn set_targets(&mut self, targets: [f64; 3], ramp: bool) {
        if !ramp {
            self.current = targets;
            self.from = targets;
            self.to = targets;
            self.pos = 0;
            self.len = 0;
            return;
        }
        if self.to == targets {
            return;
        }
        if self.current == targets {
            self.from = targets;
            self.to = targets;
            self.pos = 0;
            self.len = 0;
        } else {
            self.from = self.current;
            self.to = targets;
            self.pos = 0;
            self.len = ramp_frames_for_rate(self.sample_rate);
        }
    }

    /// Update targets from volume/balance/mute parameters.
    pub fn set_volume_targets(
        &mut self,
        volume_db: f64,
        balance_db: f64,
        muted: bool,
        hardware_volume: bool,
        hardware_mute: bool,
        ramp: bool,
    ) {
        let vol_lin = if hardware_volume {
            1.0
        } else {
            db_to_lin(volume_db)
        };
        let mute_lin = if !hardware_mute && muted { 0.0 } else { 1.0 };
        let [bal_l, bal_r] = balance_gains(balance_db);
        self.set_targets(
            [
                vol_lin * bal_l * mute_lin,
                vol_lin * bal_r * mute_lin,
                vol_lin * mute_lin,
            ],
            ramp,
        );
    }

    /// True when the gain is settled at exact unity (`1.0`) on every channel.
    #[inline]
    pub fn is_unity_settled(&self) -> bool {
        self.pos >= self.len && self.current == [1.0, 1.0, 1.0] && self.to == [1.0, 1.0, 1.0]
    }

    #[inline]
    fn is_settled(&self) -> bool {
        self.pos >= self.len
    }

    /// Advance one output frame and return `[L, R, rest]` gains.
    #[inline]
    pub fn next_frame(&mut self) -> [f64; 3] {
        if self.pos < self.len {
            self.pos += 1;
            if self.pos >= self.len {
                self.current = self.to;
            } else {
                let t = (self.pos as f64) / (self.len as f64);
                let shape = 0.5 - 0.5 * (PI * t).cos();
                for i in 0..3 {
                    self.current[i] = self.from[i] + (self.to[i] - self.from[i]) * shape;
                }
            }
        }
        self.current
    }

    /// Scale `frames` samples per channel of a planar `f64` buffer in place.
    /// Zero-cost no-op when settled at unity.
    pub fn process_planar(&mut self, chans: &mut [Vec<f64>], frames: usize) {
        if frames == 0 || self.is_unity_settled() {
            return;
        }
        if self.is_settled() {
            let g = self.current;
            for (c, chan) in chans.iter_mut().enumerate() {
                let gc = g[c.min(2)];
                for s in &mut chan[..frames] {
                    *s *= gc;
                }
            }
            return;
        }
        for f in 0..frames {
            let g = self.next_frame();
            for (c, chan) in chans.iter_mut().enumerate() {
                chan[f] *= g[c.min(2)];
            }
        }
    }

    /// Scale interleaved `f32` directly into left-justified `i32` (fallback when
    /// running without a `Chain`, e.g. `ActiveChain::Bypass`). Bit-exact when
    /// settled at unity.
    pub fn process_interleaved_f32_to_i32(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        channels: usize,
    ) {
        if self.is_unity_settled() {
            f32_to_i32(input, out);
            return;
        }
        let frames = input.len().checked_div(channels).unwrap_or(0);
        for f in 0..frames {
            let g = self.next_frame();
            let base = f * channels;
            for c in 0..channels {
                let scaled = f64::from(input[base + c]) * g[c.min(2)];
                out[base + c] = (scaled * 2_147_483_648.0).round() as i32;
            }
        }
    }
}

/// Ramped digital mute for the Direct / bypass path (never scales by volume or
/// balance; passes samples bit-exactly when unmuted and settled).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MuteRamp {
    current: f64,
    from: f64,
    to: f64,
    pos: usize,
    len: usize,
    sample_rate: u32,
}

impl MuteRamp {
    pub fn unmuted(sample_rate: u32) -> Self {
        Self {
            current: 1.0,
            from: 1.0,
            to: 1.0,
            pos: 0,
            len: 0,
            sample_rate: sample_rate.max(8_000),
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let sr = sample_rate.max(8_000);
        if self.sample_rate == sr {
            return;
        }
        self.sample_rate = sr;
        if self.pos < self.len && self.len > 0 {
            let frac = (self.pos as f64) / (self.len as f64);
            let new_len = ramp_frames_for_rate(sr);
            self.len = new_len;
            self.pos = ((frac * (new_len as f64)).round() as usize).min(new_len);
            if self.pos >= self.len {
                self.current = self.to;
            }
        }
    }

    pub fn set_muted(&mut self, muted: bool, ramp: bool) {
        let target = if muted { 0.0 } else { 1.0 };
        if !ramp {
            self.current = target;
            self.from = target;
            self.to = target;
            self.pos = 0;
            self.len = 0;
            return;
        }
        if self.to == target {
            return;
        }
        if self.current == target {
            self.from = target;
            self.to = target;
            self.pos = 0;
            self.len = 0;
        } else {
            self.from = self.current;
            self.to = target;
            self.pos = 0;
            self.len = ramp_frames_for_rate(self.sample_rate);
        }
    }

    #[inline]
    pub fn is_unity_settled(&self) -> bool {
        self.pos >= self.len && self.current == 1.0 && self.to == 1.0
    }

    #[inline]
    pub fn next_frame(&mut self) -> f64 {
        if self.pos < self.len {
            self.pos += 1;
            if self.pos >= self.len {
                self.current = self.to;
            } else {
                let t = (self.pos as f64) / (self.len as f64);
                let shape = 0.5 - 0.5 * (PI * t).cos();
                self.current = self.from + (self.to - self.from) * shape;
            }
        }
        self.current
    }

    /// Convert interleaved `f32` to left-justified `i32` (with optional integer
    /// frame repeat when `ratio > 1`), applying the mute ramp only when active.
    /// Bit-exact with [`f32_to_i32`] when unmuted and settled.
    pub fn process_f32_to_i32(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: u32,
    ) {
        let r = ratio.max(1) as usize;
        if self.is_unity_settled() {
            if r == 1 {
                f32_to_i32(input, out);
            } else {
                let frames = input.len() / channels;
                for f in 0..frames {
                    let s_base = f * channels;
                    for k in 0..r {
                        let d_base = (f * r + k) * channels;
                        f32_to_i32(
                            &input[s_base..s_base + channels],
                            &mut out[d_base..d_base + channels],
                        );
                    }
                }
            }
            return;
        }

        let frames = input.len().checked_div(channels).unwrap_or(0);
        for f in 0..frames {
            let s_base = f * channels;
            for k in 0..r {
                let g = self.next_frame();
                let d_base = (f * r + k) * channels;
                for c in 0..channels {
                    let scaled = f64::from(input[s_base + c]) * g;
                    out[d_base + c] = (scaled * 2_147_483_648.0).round() as i32;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_attenuates_correct_channel() {
        let [l0, r0] = balance_gains(0.0);
        assert_eq!(l0, 1.0);
        assert_eq!(r0, 1.0);

        let [l_neg_0, r_neg_0] = balance_gains(-0.0);
        assert_eq!(l_neg_0, 1.0);
        assert_eq!(r_neg_0, 1.0);

        // balance_db < 0 attenuates right channel by |b|
        let [l_left, r_left] = balance_gains(-6.0);
        assert_eq!(l_left, 1.0);
        assert!((r_left - db_to_lin(-6.0)).abs() < 1e-12);

        // balance_db > 0 attenuates left channel by b
        let [l_right, r_right] = balance_gains(6.0);
        assert_eq!(r_right, 1.0);
        assert!((l_right - db_to_lin(-6.0)).abs() < 1e-12);
    }

    #[test]
    fn stereo_gain_ramp_smoothly_reaches_target_in_10ms() {
        let sr = 48_000;
        let expected_len = ramp_frames_for_rate(sr);
        assert_eq!(expected_len, 480);

        let mut ramp = StereoGainRamp::unity(sr);
        assert!(ramp.is_unity_settled());

        ramp.set_volume_targets(-6.0, -3.0, false, false, false, true);
        assert!(!ramp.is_unity_settled());

        let target_l = db_to_lin(-6.0);
        let target_r = db_to_lin(-6.0) * db_to_lin(-3.0);

        let mut prev_l = 1.0_f64;
        let mut prev_r = 1.0_f64;
        let mut max_step_l = 0.0_f64;
        for f in 0..expected_len {
            let g = ramp.next_frame();
            assert!(
                g[0] <= prev_l + 1e-15,
                "frame {f}: L must decrease monotonically"
            );
            assert!(
                g[1] <= prev_r + 1e-15,
                "frame {f}: R must decrease monotonically"
            );
            max_step_l = max_step_l.max((prev_l - g[0]).abs());
            prev_l = g[0];
            prev_r = g[1];
        }
        assert!(
            max_step_l < 0.01,
            "raised cosine step must be smooth: {max_step_l}"
        );
        assert!((prev_l - target_l).abs() < 1e-12);
        assert!((prev_r - target_r).abs() < 1e-12);
        assert!(ramp.is_settled());

        // Mid-ramp reversal starts from current value, no jump.
        ramp.set_volume_targets(0.0, 0.0, false, false, false, true);
        for _ in 0..100 {
            let _ = ramp.next_frame();
        }
        let mid = ramp.current()[0];
        assert!(mid > target_l && mid < 1.0);
        ramp.set_volume_targets(-12.0, 0.0, false, false, false, true);
        let first_after = ramp.next_frame()[0];
        assert!(
            (first_after - mid).abs() < 0.01,
            "reversal must not jump: mid={mid}, after={first_after}"
        );
    }

    #[test]
    fn hardware_volume_keeps_dsp_gain_at_unity_while_balance_remains_digital() {
        let mut ramp = StereoGainRamp::unity(48_000);
        // Hardware volume handles -18 dB, but balance +6 dB is always digital on DSP path.
        ramp.set_volume_targets(-18.0, 6.0, false, true, false, false);
        assert!(!ramp.is_unity_settled());
        assert!((ramp.current()[0] - db_to_lin(-6.0)).abs() < 1e-12);
        assert_eq!(ramp.current()[1], 1.0);

        // Hardware volume + 0 balance + hardware mute -> exact unity on DSP gain.
        ramp.set_volume_targets(-18.0, 0.0, true, true, true, false);
        assert!(ramp.is_unity_settled());

        // Hardware volume + digital mute (no playback switch) -> muted (0.0).
        ramp.set_volume_targets(-18.0, 0.0, true, true, false, false);
        assert_eq!(ramp.current(), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn mute_ramp_preserves_24bit_samples_when_unmuted_and_ramps_to_zero() {
        let mut mute = MuteRamp::unmuted(48_000);
        assert!(mute.is_unity_settled());

        let mut samples_i32 = Vec::with_capacity(2048);
        for i in 0..1024 {
            let l = ((i * 12347 - 500_000) & 0x007f_ffff) << 8;
            let r = ((-i * 23457 + 100_000) & 0x007f_ffff) << 8;
            samples_i32.push(l);
            samples_i32.push(r);
        }
        let mut samples_f32 = vec![0.0_f32; samples_i32.len()];
        crate::bypass::i32_to_f32(&samples_i32, &mut samples_f32);

        let mut out = vec![0_i32; samples_i32.len()];
        mute.process_f32_to_i32(&samples_f32, &mut out, 2, 1);
        assert_eq!(out, samples_i32);

        // Ramp to mute over 480 frames.
        mute.set_muted(true, true);
        assert!(!mute.is_unity_settled());
        mute.process_f32_to_i32(&samples_f32[..480 * 2], &mut out[..480 * 2], 2, 1);
        // After 480 frames, subsequent blocks are exact digital zeros.
        mute.process_f32_to_i32(&samples_f32[..128 * 2], &mut out[..128 * 2], 2, 1);
        assert!(out[..128 * 2].iter().all(|&s| s == 0));

        // Snap unmute and check bit-exactness restored immediately.
        mute.set_muted(false, false);
        assert!(mute.is_unity_settled());
        mute.process_f32_to_i32(&samples_f32, &mut out, 2, 1);
        assert_eq!(out, samples_i32);
    }
}
