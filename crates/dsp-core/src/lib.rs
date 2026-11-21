//! Pure DSP for Audivane DSP.
//!
//! Conventions (every module follows these):
//! - Samples are `f64`, nominal full scale is [-1.0, 1.0).
//! - Buffers are **planar**: `&mut [&mut [f64]]` is one slice per channel, all
//!   the same length.
//! - Constructors may allocate. `process`/`reset` must **never** allocate,
//!   lock, or do I/O: they run on the real-time audio thread.
//! - Any `process` call may pass fewer frames than `max_block` given at
//!   construction (including 0). Passing more is a caller bug and may panic.
//! - Latency is always reported in frames at the stage's *output* rate.

pub mod ab_level;
pub mod ab_measure;
pub mod adaptive;
pub mod biquad;
pub mod bypass;
pub mod chain;
pub mod conv;
pub mod cpu_bench;
pub mod crossfeed;
pub mod dc_block;
pub mod design;
pub mod dither;
pub(crate) mod dot;
pub mod drift;
pub mod eq_apo;
pub mod fft;
pub mod filter_design;
pub mod gain;
pub mod harmonics;
pub mod limiter;
pub mod loudness;
pub mod minphase;
pub mod pitch;
pub mod presets;
pub mod rational;
pub mod resample;
pub mod tempo;
#[doc(hidden)]
pub mod test_support;
pub mod tilt;
pub mod varispeed;
pub mod window;

pub use rustfft::num_complex::Complex64;

/// A same-rate, in-place processing stage.
pub trait Stage: Send {
    /// Process every channel in place. All slices have the same length,
    /// which is <= the `max_block` the stage was built with.
    fn process(&mut self, chans: &mut [&mut [f64]]);

    /// Added delay in frames.
    fn latency_frames(&self) -> usize {
        0
    }

    /// Clear all internal state (filter memories, overlap buffers).
    fn reset(&mut self);
}

/// Convert dB to linear amplitude.
#[inline]
pub fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// Convert linear amplitude to dB.
#[inline]
pub fn lin_to_db(lin: f64) -> f64 {
    20.0 * lin.abs().log10()
}
