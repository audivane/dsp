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
}
