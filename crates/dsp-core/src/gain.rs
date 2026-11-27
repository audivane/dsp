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

    /// Advance `frames` output frames with no audio to scale (a block
    /// played as silence): the ramp ends exactly where [`Self::next_frame`]
    /// `frames` times would leave it, so a mute set while the chain is
    /// silent has finished (or gone as far) when sound comes back.
    pub fn advance(&mut self, frames: usize) {
        if frames == 0 || self.pos >= self.len {
            return;
        }
        self.pos = (self.pos + frames - 1).min(self.len - 1);
        self.next_frame();
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

impl StereoGainRamp {
    /// Interleaved `f32` to left-justified `i32` with integer frame repeat
    /// (`ratio`), for the Direct / bypass paths when the master volume has a
    /// digital part: those paths must never play louder than requested.
    /// Bit-exact with [`f32_to_i32`] while settled at unity.
    ///
    /// `dither`: the noise source and the DAC's bit depth. Below 32 bits a
    /// scaled sample is quantized to that depth with TPDF dither (instead of
    /// being truncated by the DAC); at unity nothing is added.
    pub fn process_f32_to_i32_repeat(
        &mut self,
        input: &[f32],
        out: &mut [i32],
        channels: usize,
        ratio: u32,
        dither: Option<(&mut crate::dither::TpdfDither, u32)>,
    ) {
        let r = ratio.max(1) as usize;
        if let Some((tpdf, bits)) = dither
            && (1..32).contains(&bits)
            && !self.is_unity_settled()
        {
            let scale = 2f64.powi(bits as i32 - 1);
            let max_q = (1i64 << (bits - 1)) - 1;
            let min_q = -(1i64 << (bits - 1));
            let shift = 32 - bits;
            let frames = input.len().checked_div(channels).unwrap_or(0);
            // Digital silence stays digital silence (a DAC's auto-mute or
            // standby must still see it): a channel whose gain is exactly 0
            // (a settled mute) gets no noise, and a channel is dithered only
            // from a non-zero sample on until ~100 ms after its last one
            // (per channel, so a silent channel next to music stays silent
            // and sparse clicks do not switch the noise at the block rate).
            let hold_frames = self.sample_rate / 10;
            for f in 0..frames {
                let s_base = f * channels;
                for k in 0..r {
                    let g = self.next_frame();
                    let d_base = (f * r + k) * channels;
                    for c in 0..channels {
                        let gain = g[c.min(2)];
                        let x = input[s_base + c];
                        let dithered = tpdf.keep_dithering(c, x != 0.0, hold_frames);
                        if gain == 0.0 || !dithered {
                            out[d_base + c] = 0;
                            continue;
                        }
                        let v = f64::from(x) * gain * scale + tpdf.next_lsb();
                        let q = if v.is_nan() {
                            0
                        } else {
                            (v.round_ties_even() as i64).clamp(min_q, max_q)
                        };
                        out[d_base + c] = (q << shift) as i32;
                    }
                }
            }
            return;
        }
        if r == 1 {
            self.process_interleaved_f32_to_i32(input, out, channels);
            return;
        }
        let frames = input.len().checked_div(channels).unwrap_or(0);
        for f in 0..frames {
            let s_base = f * channels;
            for k in 0..r {
                let g = self.next_frame();
                let d_base = (f * r + k) * channels;
                for c in 0..channels {
                    let scaled = f64::from(input[s_base + c]) * g[c.min(2)];
                    out[d_base + c] = (scaled * 2_147_483_648.0)
                        .round()
                        .clamp(-2_147_483_648.0, 2_147_483_647.0)
                        as i32;
                }
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
    fn advance_lands_where_frame_by_frame_does() {
        for frames in [0, 1, 7, 100, 2_205, 10_000] {
            let mut a = StereoGainRamp::unity(44_100);
            let mut b = StereoGainRamp::unity(44_100);
            a.set_targets([0.25, 0.0, 0.5], true);
            b.set_targets([0.25, 0.0, 0.5], true);
            // Part way in first, then the jump.
            for _ in 0..13 {
                a.next_frame();
                b.next_frame();
            }
            a.advance(frames);
            for _ in 0..frames {
                b.next_frame();
            }
            assert_eq!(a.current(), b.current(), "{frames} frames");
            assert_eq!(a.next_frame(), b.next_frame(), "{frames} frames, then one");
        }
    }

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
    fn repeat_path_attenuates_and_is_exact_at_unity() {
        let input: Vec<f32> = (0..64).map(|i| (i as f32 - 32.0) / 64.0).collect();
        let mut ramp = StereoGainRamp::unity(48_000);
        let mut exact = vec![0_i32; 64 * 2];
        let mut out = vec![0_i32; 64 * 2];
        ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 2, None);
        for f in 0..32 {
            for k in 0..2 {
                crate::bypass::f32_to_i32(
                    &input[f * 2..f * 2 + 2],
                    &mut exact[(f * 2 + k) * 2..(f * 2 + k) * 2 + 2],
                );
            }
        }
        assert_eq!(out, exact);
        ramp.set_volume_targets(-20.0, 0.0, false, false, false, false);
        ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 2, None);
        for (o, e) in out.iter().zip(&exact) {
            assert!((f64::from(*o) - f64::from(*e) * 0.1).abs() <= 1.0);
        }
    }

    /// Non-unity gain on a 16-bit DAC: quantized to 16 bits with TPDF
    /// dither (a constant input gives varying codes around the scaled
    /// value, not one truncated code); unity stays bit-exact, no noise.
    #[test]
    fn repeat_path_dithers_below_32_bits_only_when_scaling() {
        let mut tpdf = crate::dither::TpdfDither::new(7);
        let input = vec![0.3_f32; 4096 * 2];
        let mut out = vec![0_i32; 4096 * 2];
        let mut ramp = StereoGainRamp::unity(48_000);
        ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 1, Some((&mut tpdf, 16)));
        let mut exact = vec![0_i32; 4096 * 2];
        crate::bypass::f32_to_i32(&input, &mut exact);
        assert_eq!(out, exact, "unity must stay bit-exact");

        ramp.set_volume_targets(-6.0, 0.0, false, false, false, false);
        // Past the ramp.
        for _ in 0..4 {
            ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 1, Some((&mut tpdf, 16)));
        }
        let want = 0.3 * 10f64.powf(-6.0 / 20.0) * 32768.0;
        let codes: Vec<i64> = out.iter().map(|&v| i64::from(v >> 16)).collect();
        assert!(out.iter().all(|&v| v & 0xffff == 0), "16-bit codes only");
        let distinct: std::collections::BTreeSet<_> = codes.iter().collect();
        assert!(distinct.len() >= 2, "no dither noise");
        assert!(codes.iter().all(|&q| (q as f64 - want).abs() < 2.0));
        let mean = codes.iter().sum::<i64>() as f64 / codes.len() as f64;
        assert!((mean - want).abs() < 0.05, "mean {mean} vs {want}");
        // 32-bit DAC: no dither.
        ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 1, Some((&mut tpdf, 32)));
        let distinct: std::collections::BTreeSet<_> = out.iter().collect();
        assert_eq!(distinct.len(), 1);
    }

    /// A settled digital mute and an all-zero block stay exact zeros on the
    /// dithered path (no ±1 LSB noise for a DAC's auto-mute to see).
    #[test]
    fn dithered_repeat_path_keeps_digital_silence() {
        let mut tpdf = crate::dither::TpdfDither::new(3);
        let mut ramp = StereoGainRamp::unity(48_000);
        ramp.set_volume_targets(-20.0, 0.0, false, false, false, false);
        let zeros = vec![0.0_f32; 512 * 2];
        let mut out = vec![1_i32; 512 * 4];
        ramp.process_f32_to_i32_repeat(&zeros, &mut out, 2, 2, Some((&mut tpdf, 16)));
        assert!(out.iter().all(|&v| v == 0), "silence got noise");
        // Muted (settled at 0 gain), with signal in.
        ramp.set_volume_targets(-20.0, 0.0, true, false, false, false);
        let tone = vec![0.25_f32; 512 * 2];
        for _ in 0..8 {
            ramp.process_f32_to_i32_repeat(&tone, &mut out, 2, 2, Some((&mut tpdf, 24)));
        }
        assert!(!ramp.is_unity_settled());
        assert!(out.iter().all(|&v| v == 0), "mute got noise");
    }

    /// Sparse clicks (one every 50 ms, inside the 100 ms hold): the
    /// dither noise does not switch off between them at block edges; it
    /// stops (exact zeros) only once 100 ms have passed after the last.
    #[test]
    fn dither_holds_through_sparse_clicks_then_stops() {
        let rate = 48_000;
        let mut tpdf = crate::dither::TpdfDither::new(9);
        let mut ramp = StereoGainRamp::unity(rate);
        ramp.set_volume_targets(-20.0, 0.0, false, false, false, false);
        let block = 1024;
        let total = rate as usize; // one second
        let mut input = vec![0.0_f32; total];
        for i in (0..rate as usize / 2).step_by(rate as usize / 20) {
            input[i] = 0.5;
        }
        let mut out = vec![0_i32; total];
        for (i, o) in input.chunks(block).zip(out.chunks_mut(block)) {
            ramp.process_f32_to_i32_repeat(i, o, 1, 1, Some((&mut tpdf, 16)));
        }
        let last_click = input.iter().rposition(|&x| x != 0.0).unwrap();
        let hold_end = last_click + rate as usize / 10;
        // Inside the hold every block has noise (no silent block edges).
        for b in out[..hold_end - block].chunks(block) {
            assert!(
                b.iter().any(|&v| v != 0),
                "noise switched off inside the hold"
            );
        }
        // After it: exact digital silence.
        assert!(out[hold_end + 1..].iter().all(|&v| v == 0));
    }

    /// A silent channel next to music stays exact zeros on the dithered
    /// path.
    #[test]
    fn a_silent_channel_beside_music_gets_no_dither() {
        let mut tpdf = crate::dither::TpdfDither::new(5);
        let mut ramp = StereoGainRamp::unity(48_000);
        ramp.set_volume_targets(-6.0, 0.0, false, false, false, false);
        let input: Vec<f32> = (0..4096)
            .flat_map(|i| [((i as f32) * 0.01).sin() * 0.5, 0.0])
            .collect();
        let mut out = vec![0_i32; input.len()];
        ramp.process_f32_to_i32_repeat(&input, &mut out, 2, 1, Some((&mut tpdf, 24)));
        assert!(out.chunks(2).all(|f| f[1] == 0), "silent channel got noise");
        assert!(out.chunks(2).any(|f| f[0] != 0));
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
