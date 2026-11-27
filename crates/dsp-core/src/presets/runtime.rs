//! The interface every upsampler runtime implements, so the chain can run
//! the windowed-sinc polyphase engine, the rational resampler and the other
//! preset runtimes (cascades, interpolators, block FFT) the same way.

use crate::rational::RationalResampler;
use crate::resample::Upsampler;
use audivane_protocol::RateRatio;

/// A resampler on the real-time path: integer upsampling, or any rational
/// ratio for the runtimes that support it.
///
/// Same rules as the rest of dsp-core: constructors may allocate;
/// `process` and `reset` never allocate, lock or do I/O. Buffers are planar.
pub trait UpsamplerRuntime: Send {
    /// Output rate / input rate.
    fn ratio(&self) -> RateRatio;

    /// Output frames the next `process` call with `n_in` input frames
    /// produces. Integer ratios: always `n_in * ratio`. Rational ratios: the
    /// output frames whose newest input frame the block delivers, a function
    /// of the input position (see [`RateRatio::frames_out`]).
    fn output_frames(&self, n_in: usize) -> usize {
        n_in * self.ratio().up as usize
    }

    /// Resample one block. `input[c].len()` (the same for every channel)
    /// must be `<= max_block` given at construction and may be 0;
    /// `output[c].len()` must be [`Self::output_frames`]`(input[c].len())`.
    /// A runtime that works in larger blocks buffers internally and reports
    /// the buffering in [`Self::latency_out_frames`].
    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]);

    /// Back to the state right after construction (silence in, silence out).
    fn reset(&mut self);

    /// [`Self::reset`], then continue the frame grid as if `pos` input frames
    /// had been processed, so this runtime produces the same output counts
    /// as one that has (only matters for rational ratios).
    fn reset_to(&mut self, pos: u64) {
        let _ = pos;
        self.reset();
    }

    /// Total delay in output frames: buffering plus the filter's own delay
    /// (for a linear-phase kernel its centre; otherwise the position of the
    /// impulse response's peak), rounded to the nearest frame.
    fn latency_out_frames(&self) -> usize;

    /// [`Self::latency_out_frames`] before rounding (rational ratios can
    /// delay by a fraction of an output frame).
    fn latency_out_exact(&self) -> f64 {
        self.latency_out_frames() as f64
    }

    /// Scale every output sample by `gain` from now on (headroom). Called
    /// once after construction, before any `process`.
    fn scale_output(&mut self, gain: f64);

    /// Output frames, counted from the first `process` call, after which the
    /// response to an impulse in the first input frame is over: exactly zero
    /// for block runtimes and uniformly partitioned FIR ones, below 2^-60 of
    /// its peak for recursive ones. An FFT polyphase engine with tail stages
    /// (a long kernel, [`crate::resample::Upsampler::stages`]) leaves
    /// transform rounding past it, about -300 dB of the peak, for up to its
    /// largest stage's block of input frames. Used by the offline response
    /// measurement.
    fn impulse_span_out_frames(&self) -> usize;

    /// One single-channel runtime per channel, each running its channel
    /// exactly as this one does (bit for bit) from this one's state,
    /// sharing read-only kernel data where it can: the chain's channel
    /// workers ([`crate::chain::Chain::attach_workers`]). `None` when this
    /// runtime cannot split; it is then left untouched. After `Some`, `self`
    /// has no channels left and is dropped. Off the real-time thread (it
    /// allocates), on a runtime that has not run yet or was just reset.
    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        None
    }
}

impl UpsamplerRuntime for Upsampler {
    fn ratio(&self) -> RateRatio {
        RateRatio::integer(Upsampler::ratio(self) as u32)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        Upsampler::process(self, input, output);
    }

    fn reset(&mut self) {
        Upsampler::reset(self);
    }

    fn latency_out_frames(&self) -> usize {
        Upsampler::latency_out_frames(self)
    }

    fn scale_output(&mut self, gain: f64) {
        Upsampler::scale_output(self, gain);
    }

    fn impulse_span_out_frames(&self) -> usize {
        self.block_latency_out_frames() + self.kernel_taps()
    }

    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        Some(
            Upsampler::split_channels(self)
                .into_iter()
                .map(|u| Box::new(u) as Box<dyn UpsamplerRuntime>)
                .collect(),
        )
    }
}

impl UpsamplerRuntime for RationalResampler {
    fn ratio(&self) -> RateRatio {
        RationalResampler::ratio(self)
    }

    fn output_frames(&self, n_in: usize) -> usize {
        RationalResampler::output_frames(self, n_in)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        RationalResampler::process(self, input, output);
    }

    fn reset(&mut self) {
        RationalResampler::reset(self);
    }

    fn reset_to(&mut self, pos: u64) {
        RationalResampler::reset_to(self, pos);
    }

    fn latency_out_frames(&self) -> usize {
        RationalResampler::latency_out_frames(self)
    }

    fn latency_out_exact(&self) -> f64 {
        RationalResampler::latency_out_exact(self)
    }

    fn scale_output(&mut self, gain: f64) {
        RationalResampler::scale_output(self, gain);
    }

    fn impulse_span_out_frames(&self) -> usize {
        RationalResampler::impulse_span_out_frames(self)
    }

    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        Some(
            RationalResampler::split_channels(self)
                .into_iter()
                .map(|r| Box::new(r) as Box<dyn UpsamplerRuntime>)
                .collect(),
        )
    }
}
