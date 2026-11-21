//! Tempo-preserving pitch shift stage backed by Signalsmith Stretch (`f64`).

use crate::Stage;
use audivane_protocol::PitchQuality;
use std::ffi::c_void;
use std::fmt;

/// Minimum supported pitch shift in cents (-1 octave).
pub const MIN_CENTS: f64 = -1200.0;
/// Maximum supported pitch shift in cents (+1 octave).
pub const MAX_CENTS: f64 = 1200.0;

/// Why a [`PitchShifter`] could not be constructed or updated.
#[derive(Clone, Debug, PartialEq)]
pub enum PitchError {
    InvalidSampleRate(u32),
    InvalidChannels(usize),
    InvalidMaxBlock(usize),
    InvalidCents(f64),
    FfiError(&'static str),
}

impl fmt::Display for PitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate(sr) => write!(f, "invalid sample rate: {sr}"),
            Self::InvalidChannels(ch) => write!(f, "invalid channel count: {ch}"),
            Self::InvalidMaxBlock(mb) => write!(f, "invalid max_block: {mb}"),
            Self::InvalidCents(c) => {
                write!(
                    f,
                    "pitch cents {c} must be finite and in {MIN_CENTS}..={MAX_CENTS}"
                )
            }
            Self::FfiError(msg) => write!(f, "pitch shifter FFI error: {msg}"),
        }
    }
}

impl std::error::Error for PitchError {}

// The bridge to the vendored library (`pitch_bridge.cpp`), shared with
// [`crate::tempo`].
unsafe extern "C" {
    pub(crate) fn av_pitch_new() -> *mut c_void;
    pub(crate) fn av_pitch_free(ptr: *mut c_void);
    pub(crate) fn av_pitch_configure(
        ptr: *mut c_void,
        channels: i32,
        block_samples: i32,
        interval_samples: i32,
        split_computation: bool,
    ) -> i32;
    pub(crate) fn av_pitch_reset(ptr: *mut c_void) -> i32;
    pub(crate) fn av_pitch_set_transpose_semitones(
        ptr: *mut c_void,
        semitones: f64,
        tonality_limit: f64,
    ) -> i32;
    pub(crate) fn av_pitch_input_latency(ptr: *const c_void) -> i32;
    pub(crate) fn av_pitch_output_latency(ptr: *const c_void) -> i32;
    pub(crate) fn av_pitch_process(
        ptr: *mut c_void,
        inputs: *const *const f64,
        outputs: *const *mut f64,
        frames: i32,
    ) -> i32;
    /// `in_frames` input frames into `out_frames` output frames (the
    /// time-stretch of [`crate::tempo`]).
    pub(crate) fn av_stretch_process(
        ptr: *mut c_void,
        inputs: *const *const f64,
        in_frames: i32,
        outputs: *const *mut f64,
        out_frames: i32,
        channels: i32,
        tail_frames: i64,
        quiet: *mut i64,
    ) -> i32;
}

/// The quality actually run at `sample_rate`. The STFT work per second grows
/// with the rate, so the costly levels step down at high rates to keep the
/// worst callback well inside its deadline: `max` runs as `high` above
/// 96 kHz, and `high` runs as `low` above 192 kHz.
pub fn effective_quality(sample_rate: u32, quality: PitchQuality) -> PitchQuality {
    match quality {
        PitchQuality::Max if sample_rate > 96_000 => {
            effective_quality(sample_rate, PitchQuality::High)
        }
        PitchQuality::High if sample_rate > 192_000 => PitchQuality::Low,
        q => q,
    }
}

/// Return `(block_samples, interval_samples)` scaled to `sample_rate` for
/// `quality` (after [`effective_quality`]).
pub fn stft_geometry(sample_rate: u32, quality: PitchQuality) -> (usize, usize) {
    let sr = f64::from(sample_rate);
    let (win_ms, hop_ms) = match effective_quality(sample_rate, quality) {
        PitchQuality::Low => (120.0, 30.0),
        PitchQuality::High => (120.0, 15.0),
        PitchQuality::Max => (240.0, 30.0),
    };
    let block = ((sr * win_ms / 1000.0).round() as usize).max(64);
    let interval = ((sr * hop_ms / 1000.0).round() as usize).max(16);
    (block, interval)
}

fn validate_cents(cents: f64) -> Result<(), PitchError> {
    if !cents.is_finite() || !(MIN_CENTS..=MAX_CENTS).contains(&cents) {
        return Err(PitchError::InvalidCents(cents));
    }
    Ok(())
}

/// Multichannel, phase-coherent, tempo-preserving pitch shifter in `f64`.
///
/// Built off the real-time thread; [`Stage::process`], [`Stage::reset`], and
/// [`PitchShifter::set_cents`] do not allocate, lock, or perform I/O.
pub struct PitchShifter {
    inner: *mut c_void,
    channels: usize,
    max_block: usize,
    latency: usize,
    scratch: Vec<Vec<f64>>,
    in_ptrs: Vec<*const f64>,
    out_ptrs: Vec<*mut f64>,
}

// SAFETY: `SignalsmithStretch<double>` has no thread-local or global mutable
// state; `PitchShifter` owns the heap-allocated C++ object exclusively.
unsafe impl Send for PitchShifter {}

impl Drop for PitchShifter {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { av_pitch_free(self.inner) };
            self.inner = std::ptr::null_mut();
        }
    }
}

impl PitchShifter {
    /// Construct a pitch shifter for `channels` at `sample_rate` Hz.
    ///
    /// `max_block` is the largest frame count that may be passed to
    /// [`Stage::process`]. Split computation is always enabled so STFT work is
    /// spread evenly across callbacks.
    pub fn try_new(
        sample_rate: u32,
        channels: usize,
        max_block: usize,
        cents: f64,
        quality: PitchQuality,
    ) -> Result<Self, PitchError> {
        if sample_rate == 0 {
            return Err(PitchError::InvalidSampleRate(sample_rate));
        }
        let ch = i32::try_from(channels)
            .ok()
            .filter(|&c| c > 0)
            .ok_or(PitchError::InvalidChannels(channels))?;
        if max_block == 0 || i32::try_from(max_block).is_err() {
            return Err(PitchError::InvalidMaxBlock(max_block));
        }
        validate_cents(cents)?;

        let inner = unsafe { av_pitch_new() };
        if inner.is_null() {
            return Err(PitchError::FfiError("allocation failed"));
        }

        // Ensure `inner` is freed if configuration fails.
        struct Guard(*mut c_void);
        impl Drop for Guard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { av_pitch_free(self.0) };
                }
            }
        }
        let mut guard = Guard(inner);

        let (block_samples, interval_samples) = stft_geometry(sample_rate, quality);
        let block_i32 =
            i32::try_from(block_samples).map_err(|_| PitchError::InvalidSampleRate(sample_rate))?;
        let interval_i32 = i32::try_from(interval_samples)
            .map_err(|_| PitchError::InvalidSampleRate(sample_rate))?;

        let rc = unsafe { av_pitch_configure(inner, ch, block_i32, interval_i32, true) };
        if rc != 0 {
            return Err(PitchError::FfiError("configure failed"));
        }
        let rc = unsafe { av_pitch_set_transpose_semitones(inner, cents / 100.0, 0.0) };
        if rc != 0 {
            return Err(PitchError::FfiError("set_transpose_semitones failed"));
        }

        let in_lat = unsafe { av_pitch_input_latency(inner) };
        let out_lat = unsafe { av_pitch_output_latency(inner) };
        if in_lat < 0 || out_lat < 0 {
            return Err(PitchError::FfiError("latency query failed"));
        }
        let latency = usize::try_from(in_lat + out_lat).unwrap_or(0);

        let scratch = vec![vec![0.0; max_block]; channels];
        let in_ptrs = vec![std::ptr::null(); channels];
        let out_ptrs = vec![std::ptr::null_mut(); channels];

        guard.0 = std::ptr::null_mut();
        Ok(Self {
            inner,
            channels,
            max_block,
            latency,
            scratch,
            in_ptrs,
            out_ptrs,
        })
    }

    /// Update the pitch shift in cents without reallocating.
    pub fn set_cents(&mut self, cents: f64) -> Result<(), PitchError> {
        validate_cents(cents)?;
        let rc = unsafe { av_pitch_set_transpose_semitones(self.inner, cents / 100.0, 0.0) };
        if rc != 0 {
            return Err(PitchError::FfiError("set_transpose_semitones failed"));
        }
        Ok(())
    }
}

impl Stage for PitchShifter {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        assert_eq!(
            chans.len(),
            self.channels,
            "channel count mismatch in PitchShifter::process"
        );
        let frames = chans[0].len();
        if frames == 0 {
            return;
        }
        assert!(
            frames <= self.max_block,
            "block size {frames} exceeds max_block {}",
            self.max_block
        );

        for (c, chan) in chans.iter_mut().enumerate() {
            assert_eq!(chan.len(), frames, "channel slice length mismatch");
            self.scratch[c][..frames].copy_from_slice(&chan[..frames]);
            self.in_ptrs[c] = self.scratch[c].as_ptr();
            self.out_ptrs[c] = chan.as_mut_ptr();
        }

        let rc = unsafe {
            av_pitch_process(
                self.inner,
                self.in_ptrs.as_ptr(),
                self.out_ptrs.as_ptr(),
                i32::try_from(frames).expect("frames fits i32"),
            )
        };
        debug_assert_eq!(rc, 0, "av_pitch_process failed");
    }

    fn latency_frames(&self) -> usize {
        self.latency
    }

    fn reset(&mut self) {
        let rc = unsafe { av_pitch_reset(self.inner) };
        debug_assert_eq!(rc, 0, "av_pitch_reset failed");
        for buf in &mut self.scratch {
            buf.fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::FftPlanner;
    use std::f64::consts::PI;

    fn run_signal(
        sr: u32,
        channels: usize,
        input: &[f64],
        block_size: usize,
        cents: f64,
        quality: PitchQuality,
    ) -> (Vec<Vec<f64>>, usize) {
        let total = input.len();
        let mut shifter = PitchShifter::try_new(sr, channels, block_size, cents, quality).unwrap();
        let latency = shifter.latency_frames();
        let mut out = vec![vec![0.0; total]; channels];
        let mut pos = 0;
        while pos < total {
            let n = (total - pos).min(block_size);
            let mut block: Vec<Vec<f64>> = (0..channels)
                .map(|_| input[pos..pos + n].to_vec())
                .collect();
            let mut refs: Vec<&mut [f64]> = block.iter_mut().map(Vec::as_mut_slice).collect();
            shifter.process(&mut refs);
            for c in 0..channels {
                out[c][pos..pos + n].copy_from_slice(&block[c]);
            }
            pos += n;
        }
        (out, latency)
    }

    /// Solve a linear 3x3 system `A x = b` via Gaussian elimination with partial pivoting.
    fn solve_3x3(mut a: [[f64; 3]; 3], mut b: [f64; 3]) -> [f64; 3] {
        for i in 0..3 {
            let mut max_row = i;
            for r in (i + 1)..3 {
                if a[r][i].abs() > a[max_row][i].abs() {
                    max_row = r;
                }
            }
            a.swap(i, max_row);
            b.swap(i, max_row);
            let piv = a[i][i];
            if piv.abs() < 1e-30 {
                continue;
            }
            for r in (i + 1)..3 {
                let factor = a[r][i] / piv;
                for c in i..3 {
                    a[r][c] -= factor * a[i][c];
                }
                b[r] -= factor * b[i];
            }
        }
        let mut x = [0.0; 3];
        for i in (0..3).rev() {
            let mut sum = b[i];
            for c in (i + 1)..3 {
                sum -= a[i][c] * x[c];
            }
            x[i] = if a[i][i].abs() > 1e-30 {
                sum / a[i][i]
            } else {
                0.0
            };
        }
        x
    }

    /// Least-squares fit of `a*cos(2π f t) + b*sin(2π f t) + c` at fixed `freq_hz`.
    /// Returns `(a, b, c, residual_rms, signal_rms)`.
    fn fit_sine_at_freq(samples: &[f64], sr: u32, freq_hz: f64) -> (f64, f64, f64, f64, f64) {
        let n = samples.len();
        let w = 2.0 * PI * freq_hz / f64::from(sr);
        let mut s_cc = 0.0;
        let mut s_cs = 0.0;
        let mut s_c = 0.0;
        let mut s_ss = 0.0;
        let mut s_s = 0.0;
        let mut s_yc = 0.0;
        let mut s_ys = 0.0;
        let mut s_y = 0.0;
        let mut s_yy = 0.0;
        for (i, &y) in samples.iter().enumerate() {
            let (s, c) = (w * i as f64).sin_cos();
            s_cc += c * c;
            s_cs += c * s;
            s_c += c;
            s_ss += s * s;
            s_s += s;
            s_yc += y * c;
            s_ys += y * s;
            s_y += y;
            s_yy += y * y;
        }
        let nf = n as f64;
        let [a, b, c_dc] = solve_3x3(
            [[s_cc, s_cs, s_c], [s_cs, s_ss, s_s], [s_c, s_s, nf]],
            [s_yc, s_ys, s_y],
        );
        let mut err_pow = 0.0;
        for (i, &y) in samples.iter().enumerate() {
            let (s, c) = (w * i as f64).sin_cos();
            let r = y - (a * c + b * s + c_dc);
            err_pow += r * r;
        }
        let res_rms = (err_pow / nf).sqrt();
        let sig_rms = (s_yy / nf).sqrt();
        (a, b, c_dc, res_rms, sig_rms)
    }

    /// Find best-fit frequency near `hint_hz` (or via FFT peak if `None`) and return
    /// `(fitted_freq_hz, residual_db)`.
    fn measure_fitted_sine(samples: &[f64], sr: u32, hint_hz: Option<f64>) -> (f64, f64) {
        let coarse_hz = match hint_hz {
            Some(h) => h,
            None => {
                let n = samples.len().next_power_of_two() / 2;
                let slice = &samples[samples.len() - n..];
                let fft = FftPlanner::<f64>::new().plan_fft_forward(n);
                let mut buf: Vec<crate::Complex64> = slice
                    .iter()
                    .enumerate()
                    .map(|(i, &x)| {
                        let w = 0.5 * (1.0 - (2.0 * PI * i as f64 / n as f64).cos());
                        crate::Complex64::new(x * w, 0.0)
                    })
                    .collect();
                fft.process(&mut buf);
                let half = n / 2;
                let (peak_bin, _) = buf[..half]
                    .iter()
                    .enumerate()
                    .skip(1)
                    .max_by(|a, b| a.1.norm().total_cmp(&b.1.norm()))
                    .unwrap();
                peak_bin as f64 * f64::from(sr) / n as f64
            }
        };

        // Coarse grid scan over +/- 4 bins (step 0.1 bin) to avoid sidelobe traps,
        // then golden-section refinement around the global minimum.
        let bin_hz = f64::from(sr) / samples.len() as f64;
        let step = 0.1 * bin_hz;
        let mut best_grid_hz = coarse_hz;
        let mut best_grid_err = f64::INFINITY;
        for k in -40..=40 {
            let f = (coarse_hz + k as f64 * step).max(1.0);
            let err = fit_sine_at_freq(samples, sr, f).3;
            if err < best_grid_err {
                best_grid_err = err;
                best_grid_hz = f;
            }
        }
        let mut lo = (best_grid_hz - step).max(1.0);
        let mut hi = best_grid_hz + step;
        let phi = (5.0_f64.sqrt() - 1.0) * 0.5;
        let mut x1 = hi - phi * (hi - lo);
        let mut x2 = lo + phi * (hi - lo);
        let mut f1 = fit_sine_at_freq(samples, sr, x1).3;
        let mut f2 = fit_sine_at_freq(samples, sr, x2).3;
        for _ in 0..40 {
            if f1 < f2 {
                hi = x2;
                x2 = x1;
                f2 = f1;
                x1 = hi - phi * (hi - lo);
                f1 = fit_sine_at_freq(samples, sr, x1).3;
            } else {
                lo = x1;
                x1 = x2;
                f1 = f2;
                x2 = lo + phi * (hi - lo);
                f2 = fit_sine_at_freq(samples, sr, x2).3;
            }
        }
        let best_hz = 0.5 * (lo + hi);
        let (_, _, _, res_rms, sig_rms) = fit_sine_at_freq(samples, sr, best_hz);
        let res_db = 20.0 * (res_rms / sig_rms.max(1e-30)).log10();
        (best_hz, res_db)
    }

    #[test]
    fn validate_cents_and_constructor_errors() {
        assert!(matches!(
            PitchShifter::try_new(0, 2, 512, 0.0, PitchQuality::High),
            Err(PitchError::InvalidSampleRate(0))
        ));
        assert!(matches!(
            PitchShifter::try_new(48_000, 0, 512, 0.0, PitchQuality::High),
            Err(PitchError::InvalidChannels(0))
        ));
        assert!(matches!(
            PitchShifter::try_new(48_000, 2, 0, 0.0, PitchQuality::High),
            Err(PitchError::InvalidMaxBlock(0))
        ));
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1201.0, 1201.0] {
            assert!(matches!(
                PitchShifter::try_new(48_000, 2, 512, bad, PitchQuality::High),
                Err(PitchError::InvalidCents(_))
            ));
        }
        let mut shifter =
            PitchShifter::try_new(48_000, 2, 512, -31.77, PitchQuality::High).unwrap();
        for bad in [f64::NAN, f64::INFINITY, -1200.1, 1200.1] {
            assert!(matches!(
                shifter.set_cents(bad),
                Err(PitchError::InvalidCents(_))
            ));
        }
        assert!(shifter.set_cents(100.0).is_ok());
    }

    #[test]
    fn fitted_sine_residual_calibrates_clean_sine() {
        let sr = 44_100;
        let freq = 1000.0;
        let clean: Vec<f64> = (0..32_768)
            .map(|i| (2.0 * PI * freq * i as f64 / f64::from(sr)).sin() * 0.8)
            .collect();
        let (meas_hz, res_db) = measure_fitted_sine(&clean, sr, None);
        assert!((meas_hz - freq).abs() < 1e-4, "meas_hz={meas_hz}");
        assert!(
            res_db < -140.0,
            "clean sine residual {res_db:.1} dB >= -140 dB"
        );
    }

    #[test]
    #[ignore = "measurement report; run with tier B"]
    fn measure_all_quality_levels() {
        let sr = 44_100;
        let total = 132_300; // 3 seconds
        let steady_start = 44_100; // skip 1 s settling

        for quality in [PitchQuality::Low, PitchQuality::High, PitchQuality::Max] {
            // 1. Single sines
            for &cents in &[-31.77, 100.0, -1200.0] {
                let in_freq = 1000.0;
                let input: Vec<f64> = (0..total)
                    .map(|i| (2.0 * PI * in_freq * i as f64 / f64::from(sr)).sin() * 0.5)
                    .collect();
                let (out, lat) = run_signal(sr, 2, &input, 512, cents, quality);
                let expected_hz = in_freq * 2f64.powf(cents / 1200.0);
                let (meas_hz, res_db) =
                    measure_fitted_sine(&out[0][steady_start..], sr, Some(expected_hz));
                let err_cents = 1200.0 * (meas_hz / expected_hz).log2();
                eprintln!(
                    "[{quality:?}] sine cents={cents:+.2}: lat={lat} ({:.1}ms) err={err_cents:+.4}c res={res_db:.1}dB",
                    lat as f64 * 1000.0 / f64::from(sr)
                );
                assert!(err_cents.abs() < 2.0, "{quality:?} err={err_cents}");
                assert!(res_db < -50.0, "{quality:?} res={res_db}");
            }

            // 2. Two-tone (440 + 1000 Hz) & Multi-tone (220, 440, 880, 1760, 3520 Hz)
            for (name, freqs) in [
                ("two-tone", &[440.0, 1000.0][..]),
                ("multi-tone", &[220.0, 440.0, 880.0, 1760.0, 3520.0][..]),
            ] {
                let amp = 0.8 / freqs.len() as f64;
                let input: Vec<f64> = (0..total)
                    .map(|i| {
                        freqs
                            .iter()
                            .map(|&f| (2.0 * PI * f * i as f64 / f64::from(sr)).sin() * amp)
                            .sum()
                    })
                    .collect();
                let cents = -31.77;
                let ratio = 2f64.powf(cents / 1200.0);
                let (out, _) = run_signal(sr, 2, &input, 512, cents, quality);
                let mut residual = out[0][steady_start..].to_vec();
                let sig_rms =
                    (residual.iter().map(|x| x * x).sum::<f64>() / residual.len() as f64).sqrt();
                let mut max_err_c = 0.0f64;
                for &f in freqs {
                    let exp_hz = f * ratio;
                    // Subtract best-fit tone near exp_hz sequentially
                    let (fit_hz, _) = measure_fitted_sine(&residual, sr, Some(exp_hz));
                    let err_c = 1200.0 * (fit_hz / exp_hz).log2();
                    max_err_c = max_err_c.max(err_c.abs());
                    let (a, b, c_dc, _, _) = fit_sine_at_freq(&residual, sr, fit_hz);
                    let w = 2.0 * PI * fit_hz / f64::from(sr);
                    for (i, s) in residual.iter_mut().enumerate() {
                        let (sn, cs) = (w * i as f64).sin_cos();
                        *s -= a * cs + b * sn + c_dc;
                    }
                }
                let res_rms =
                    (residual.iter().map(|x| x * x).sum::<f64>() / residual.len() as f64).sqrt();
                let res_db = 20.0 * (res_rms / sig_rms.max(1e-30)).log10();
                eprintln!(
                    "[{quality:?}] {name} (-31.77c): max_err={max_err_c:.4}c res={res_db:.1}dB"
                );
                assert!(max_err_c < 6.0);
            }

            // 3. Click train smearing
            let period = 4410;
            let mut clicks = vec![0.0; sr as usize];
            for i in (0..clicks.len()).step_by(period) {
                clicks[i] = 0.9;
            }
            let (out, lat) = run_signal(sr, 2, &clicks, 512, -31.77, quality);
            let target_click = period * 4 + lat;
            let half_win = (sr as usize * 15) / 1000;
            let peak_near = out[0][target_click - half_win..target_click + half_win]
                .iter()
                .map(|x| x.abs())
                .fold(0.0f64, f64::max);
            let quiet_rms = (out[0]
                [target_click + period / 2 - 200..target_click + period / 2 + 200]
                .iter()
                .map(|x| x * x)
                .sum::<f64>()
                / 400.0)
                .sqrt();
            eprintln!(
                "[{quality:?}] click-train: lat={lat} peak={peak_near:.4} quiet_rms={quiet_rms:.6}"
            );
            assert!(peak_near > 0.2);
            assert!(quiet_rms < 0.02);
        }
    }

    #[test]
    fn multichannel_phase_coherence() {
        let input: Vec<f64> = (0..48_000)
            .map(|i| (2.0 * PI * 440.0 * i as f64 / 48_000.0).sin() * 0.5)
            .collect();
        let (out, _) = run_signal(48_000, 4, &input, 256, -31.77, PitchQuality::High);
        for ch in 1..4 {
            let max_diff = out[0]
                .iter()
                .zip(out[ch].iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f64, f64::max);
            assert!(
                max_diff < 1e-3,
                "channel {ch} diverged from channel 0 by {max_diff}"
            );
        }
    }

    #[test]
    fn latency_includes_the_split_computation_interval() {
        // Split computation adds one interval of output latency; this
        // catches it being switched off (which brings back callback spikes).
        for quality in [PitchQuality::Low, PitchQuality::High, PitchQuality::Max] {
            let (block, interval) = stft_geometry(44_100, quality);
            let s = PitchShifter::try_new(44_100, 2, 512, -31.77, quality).unwrap();
            assert_eq!(s.latency_frames(), block + interval, "{quality:?}");
        }
        assert_eq!(
            [PitchQuality::Low, PitchQuality::High, PitchQuality::Max].map(|q| {
                PitchShifter::try_new(44_100, 2, 512, 0.0, q)
                    .unwrap()
                    .latency_frames()
            }),
            [6615, 5954, 11907]
        );
    }

    #[test]
    fn costly_levels_step_down_at_high_rates() {
        use PitchQuality::*;
        assert_eq!(effective_quality(96_000, Max), Max);
        assert_eq!(effective_quality(192_000, Max), High);
        assert_eq!(effective_quality(192_000, High), High);
        assert_eq!(effective_quality(384_000, Max), Low);
        assert_eq!(effective_quality(384_000, High), Low);
        assert_eq!(stft_geometry(192_000, Max), stft_geometry(192_000, High));
    }

    #[test]
    #[ignore = "timing benchmark: run with --ignored --test-threads=1"]
    fn cpu_rtf_and_p99_128_frames() {
        for quality in [PitchQuality::Low, PitchQuality::High, PitchQuality::Max] {
            for sr in [44_100u32, 192_000u32] {
                let block = 128;
                let blocks = (sr as usize * 2) / block; // 2 seconds
                let audio_secs = (blocks * block) as f64 / f64::from(sr);
                let budget_us = (block as f64 / f64::from(sr)) * 1_000_000.0;
                let mut shifter = PitchShifter::try_new(sr, 2, block, -31.77, quality).unwrap();
                // Fresh multi-tone input every call (feeding the output back
                // in would decay and under-load the STFT).
                let source: Vec<f64> = (0..blocks * block)
                    .map(|i| {
                        let t = i as f64 / f64::from(sr);
                        [220.0, 440.0, 1000.0, 3520.0, 9000.0]
                            .iter()
                            .map(|f| (2.0 * PI * f * t).sin() * 0.15)
                            .sum::<f64>()
                    })
                    .collect();
                let mut l = vec![0.0; block];
                let mut r = vec![0.0; block];

                let mut times_us = Vec::with_capacity(blocks);
                let total_start = std::time::Instant::now();
                for b in 0..blocks {
                    l.copy_from_slice(&source[b * block..(b + 1) * block]);
                    r.copy_from_slice(&source[b * block..(b + 1) * block]);
                    let t0 = std::time::Instant::now();
                    let mut refs = [l.as_mut_slice(), r.as_mut_slice()];
                    shifter.process(&mut refs);
                    times_us.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
                }
                let total_secs = total_start.elapsed().as_secs_f64();
                let rtf = total_secs / audio_secs;
                times_us.sort_by(f64::total_cmp);
                let p99 = times_us[(times_us.len() * 99) / 100];
                let max_us = *times_us.last().unwrap();
                let over = times_us.iter().filter(|&&t| t > budget_us).count();
                eprintln!(
                    "[{quality:?}] {sr}Hz 128f (budget {budget_us:.1}us): RTF={rtf:.4}, p99={p99:.1}us, max={max_us:.1}us, over={over}/{blocks}"
                );
                assert!(rtf < 1.0, "{quality:?} {sr}Hz RTF={rtf:.4} >= 1.0");
                assert!(
                    p99 < budget_us,
                    "{quality:?} {sr}Hz p99={p99:.1}us exceeds budget {budget_us:.1}us"
                );
            }
        }
    }

    #[test]
    fn variable_blocks_and_reset() {
        let mut shifter =
            PitchShifter::try_new(48_000, 2, 1024, 100.0, PitchQuality::High).unwrap();
        for &n in &[0usize, 1, 7, 13, 256, 1024, 33] {
            let mut l = vec![0.25; n];
            let mut r = vec![-0.25; n];
            let mut refs = [l.as_mut_slice(), r.as_mut_slice()];
            shifter.process(&mut refs);
            assert!(l.iter().all(|x| x.is_finite()));
            assert!(r.iter().all(|x| x.is_finite()));
        }
        shifter.set_cents(-50.0).unwrap();
        shifter.reset();
        let mut l = vec![0.0; 128];
        let mut r = vec![0.0; 128];
        let mut refs = [l.as_mut_slice(), r.as_mut_slice()];
        shifter.process(&mut refs);
        assert!(l.iter().all(|&x| x == 0.0));
    }
}
