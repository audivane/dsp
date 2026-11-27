//! Rational resampling by `L/M` (output rate / input rate, reduced): direct
//! polyphase evaluation of one kernel defined on the intermediate grid at
//! `input * L` (= `output * M`).
//!
//! Conceptually the input is zero-stuffed by `L`, filtered with `h` and every
//! `M`-th sample is kept. As a polyphase filter, output frame `m` uses branch
//! `b = (m M) mod L` and the input frames `i0 = floor(m M / L)`, `i0 - 1`, ...:
//!
//! ```text
//! y[m] = sum_j h[b + j L] x[i0 - j]
//! ```
//!
//! A call with `n_in` input frames emits every output frame whose newest
//! input frame `i0` it delivers, so the count varies by one between calls
//! ([`RateRatio::frames_out`] gives it in advance, and the maximum for a
//! block size). Integer upsampling (`M = 1`) keeps the FFT engine of
//! [`crate::resample::Upsampler`]; this runtime handles `M > 1`, fractional
//! upsampling and decimation alike.
//!
//! Cost: `T = ceil(N / L)` multiply-adds per output frame and channel, with
//! no block latency. Memory: the kernel once (`8 N` bytes), reordered into
//! `L` reversed branches so each output is one contiguous dot product.
//!
//! State is the next output's branch and how far its newest input lies
//! ahead of the next input to arrive. A time-varying ratio (varispeed,
//! drift tracking) is the same loop with the constant step `M / L` replaced
//! by a per-output fractional step and the branch chosen (and
//! interpolated) from a finer phase: [`crate::adaptive`].

use crate::dot::Kernels;
use audivane_protocol::RateRatio;

/// Direct polyphase rational resampler (see the module docs).
pub struct RationalResampler {
    ratio: RateRatio,
    l: usize,
    m: usize,
    /// Taps per branch (input frames per output frame).
    taps: usize,
    /// Kernel length on the intermediate grid.
    kernel_len: usize,
    /// `L` branches of `taps` coefficients, each reversed so the newest input
    /// meets the last coefficient: `coeffs[b T + t] = h[b + (T - 1 - t) L]`.
    /// Shared by the per-channel resamplers of
    /// [`RationalResampler::split_channels`].
    coeffs: std::sync::Arc<Vec<f64>>,
    /// The kernel's own delay in intermediate-grid samples.
    kernel_delay: f64,
    channels: usize,
    max_block_in: usize,
    /// Per channel: `taps - 1` frames of history, then room for one block.
    hist: Vec<Vec<f64>>,
    /// Branch of the next output frame.
    phase: usize,
    /// Its newest input frame, counted from the next input frame to arrive.
    lead: usize,
    /// Input frames seen, modulo `M`.
    pos: u64,
    step_int: usize,
    step_frac: usize,
    kernels: Kernels,
}

impl RationalResampler {
    /// Run `kernel` (on the intermediate grid, each of its `L` polyphase
    /// branches summing to about 1, so the passband gain is unity) at
    /// `ratio`. `kernel_delay` is its own delay in intermediate-grid samples
    /// (the centre for linear phase), reported by
    /// [`Self::latency_out_frames`] after division by `M`.
    pub fn from_kernel(
        kernel: &[f64],
        ratio: RateRatio,
        kernel_delay: f64,
        channels: usize,
        max_block_in: usize,
    ) -> Self {
        let l = ratio.up as usize;
        let m = ratio.down as usize;
        assert!(l >= 1 && m >= 1, "ratio terms must be >= 1");
        crate::test_support::record("rational", || {
            crate::test_support::words(
                &[
                    l as u64,
                    m as u64,
                    kernel_delay.to_bits(),
                    kernel.len() as u64,
                ],
                kernel,
            )
        });
        let taps = kernel.len().div_ceil(l).max(1);
        let mut coeffs = vec![0.0; l * taps];
        for (b, row) in coeffs.chunks_exact_mut(taps).enumerate() {
            for (t, c) in row.iter_mut().enumerate() {
                let k = b + (taps - 1 - t) * l;
                *c = kernel.get(k).copied().unwrap_or(0.0);
            }
        }
        let hist_len = taps - 1 + max_block_in.max(1);
        Self {
            ratio,
            l,
            m,
            taps,
            kernel_len: kernel.len(),
            coeffs: std::sync::Arc::new(coeffs),
            kernel_delay,
            channels,
            max_block_in,
            hist: (0..channels).map(|_| vec![0.0; hist_len]).collect(),
            phase: 0,
            lead: 0,
            pos: 0,
            step_int: m / l,
            step_frac: m % l,
            kernels: Kernels::detect(),
        }
    }

    pub fn ratio(&self) -> RateRatio {
        self.ratio
    }

    /// Multiply-adds per output frame and channel.
    pub fn taps_per_branch(&self) -> usize {
        self.taps
    }

    /// Kernel length on the intermediate grid.
    pub fn kernel_taps(&self) -> usize {
        self.kernel_len
    }

    /// Output frames the next call with `n_in` input frames produces.
    pub fn output_frames(&self, n_in: usize) -> usize {
        // Outputs k = 0.. while lead + floor((phase + k M) / L) < n_in.
        if n_in <= self.lead {
            return 0;
        }
        let span = (n_in - self.lead) * self.l;
        (span - self.phase).div_ceil(self.m)
    }

    /// Input frames seen since construction or the last reset, modulo `M`.
    pub fn input_pos(&self) -> u64 {
        self.pos
    }

    /// Clear the history and put the frame grid where it would be after
    /// `pos` input frames (so this runtime emits exactly the output counts
    /// of another one that has seen `pos` frames).
    pub fn reset_to(&mut self, pos: u64) {
        for h in &mut self.hist {
            h.fill(0.0);
        }
        let (l, m) = (self.l as u64, self.m as u64);
        let r = pos % m;
        // First output at or after input r on the grid: m0 = ceil(r L / M).
        let m0 = (r * l).div_ceil(m);
        self.phase = ((m0 * m) % l) as usize;
        self.lead = ((m0 * m) / l - r) as usize;
        self.pos = r;
    }

    pub fn reset(&mut self) {
        self.reset_to(0);
    }

    /// Delay in output frames: the kernel delay divided by `M`, rounded to
    /// the nearest frame (halves up).
    pub fn latency_out_frames(&self) -> usize {
        (self.latency_out_exact() + 0.5).floor() as usize
    }

    /// The same delay without rounding.
    pub fn latency_out_exact(&self) -> f64 {
        self.kernel_delay / self.m as f64
    }

    /// Output frames after which the response to an impulse in the first
    /// input frame is over.
    pub fn impulse_span_out_frames(&self) -> usize {
        self.kernel_len.div_ceil(self.m) + 1
    }

    /// Scale every output by `gain` (folded into the coefficients).
    pub fn scale_output(&mut self, gain: f64) {
        // Unique until the resampler is split, so this never copies.
        for c in std::sync::Arc::make_mut(&mut self.coeffs) {
            *c *= gain;
        }
    }

    /// One single-channel resampler per channel, each running its channel
    /// exactly as this one does, bit for bit (the two-channel dot product
    /// sums each channel in the one-channel order, `crate::dot`), from
    /// this one's state. The coefficients are shared. Off the real-time
    /// thread (it allocates); `self` is left without channels.
    pub fn split_channels(&mut self) -> Vec<RationalResampler> {
        let hist = std::mem::take(&mut self.hist);
        self.channels = 0;
        hist.into_iter()
            .map(|h| RationalResampler {
                ratio: self.ratio,
                l: self.l,
                m: self.m,
                taps: self.taps,
                kernel_len: self.kernel_len,
                coeffs: std::sync::Arc::clone(&self.coeffs),
                kernel_delay: self.kernel_delay,
                channels: 1,
                max_block_in: self.max_block_in,
                hist: vec![h],
                phase: self.phase,
                lead: self.lead,
                pos: self.pos,
                step_int: self.step_int,
                step_frac: self.step_frac,
                kernels: self.kernels,
            })
            .collect()
    }

    /// `output[c].len()` must be [`Self::output_frames`]`(input[c].len())`.
    pub fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        assert_eq!(input.len(), self.channels, "input channel count mismatch");
        assert_eq!(output.len(), self.channels, "output channel count mismatch");
        if self.channels == 0 {
            return;
        }
        let n_in = input[0].len();
        assert!(
            n_in <= self.max_block_in.max(1),
            "input frame count {n_in} exceeds max_block_in {}",
            self.max_block_in
        );
        let n_out = self.output_frames(n_in);
        for c in 0..self.channels {
            assert_eq!(input[c].len(), n_in, "inconsistent input slice length");
            assert_eq!(
                output[c].len(),
                n_out,
                "output slice length must be output_frames(input.len())"
            );
        }
        if n_in == 0 {
            return;
        }
        let t = self.taps;
        let hl = t - 1;
        for (h, x) in self.hist.iter_mut().zip(input) {
            h[hl..hl + n_in].copy_from_slice(x);
        }

        let mut phase = self.phase;
        let mut lead = self.lead;
        let k = self.kernels;
        if self.channels == 2 {
            let (a, b) = (&self.hist[0], &self.hist[1]);
            let (out_a, rest) = output.split_at_mut(1);
            let (oa, ob) = (&mut *out_a[0], &mut *rest[0]);
            for j in 0..n_out {
                let row = &self.coeffs[phase * t..phase * t + t];
                let (ya, yb) = (k.dot2)(row, &a[lead..lead + t], &b[lead..lead + t]);
                oa[j] = ya;
                ob[j] = yb;
                phase += self.step_frac;
                lead += self.step_int;
                if phase >= self.l {
                    phase -= self.l;
                    lead += 1;
                }
            }
        } else {
            for j in 0..n_out {
                let row = &self.coeffs[phase * t..phase * t + t];
                for (h, out) in self.hist.iter().zip(output.iter_mut()) {
                    out[j] = (k.dot1)(row, &h[lead..lead + t]);
                }
                phase += self.step_frac;
                lead += self.step_int;
                if phase >= self.l {
                    phase -= self.l;
                    lead += 1;
                }
            }
        }
        debug_assert!(lead >= n_in);
        self.phase = phase;
        self.lead = lead - n_in;
        for h in &mut self.hist {
            h.copy_within(n_in..n_in + hl, 0);
        }
        self.pos = (self.pos + n_in as u64) % self.m as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0
    }

    /// Zero-stuff by L, convolve with h, keep every M-th sample.
    fn reference(x: &[f64], h: &[f64], l: usize, m: usize, n_out: usize) -> Vec<f64> {
        (0..n_out)
            .map(|o| {
                let t = o * m;
                let mut s = 0.0;
                for (k, &xk) in x.iter().enumerate() {
                    let idx = t as i64 - (k * l) as i64;
                    if idx >= 0 && (idx as usize) < h.len() {
                        s += h[idx as usize] * xk;
                    }
                }
                s
            })
            .collect()
    }

    fn run(rs: &mut RationalResampler, x: &[Vec<f64>], blocks: &[usize]) -> Vec<Vec<f64>> {
        let ch = x.len();
        let mut out = vec![Vec::new(); ch];
        let mut pos = 0;
        let mut bi = 0;
        while pos < x[0].len() {
            let n = blocks[bi % blocks.len()].min(x[0].len() - pos);
            bi += 1;
            let n_out = rs.output_frames(n);
            assert_eq!(n_out, rs.ratio().frames_out(pos as u64, n));
            let inp: Vec<&[f64]> = x.iter().map(|c| &c[pos..pos + n]).collect();
            let mut bufs = vec![vec![0.0; n_out]; ch];
            {
                let mut outs: Vec<&mut [f64]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
                rs.process(&inp, &mut outs);
            }
            for (o, b) in out.iter_mut().zip(bufs) {
                o.extend(b);
            }
            pos += n;
        }
        out
    }

    #[test]
    fn matches_zero_stuff_filter_decimate_for_any_block_sizes() {
        for (l, m, taps) in [
            (160usize, 147usize, 9usize),
            (147, 160, 7),
            (1, 2, 31),
            (1, 4, 17),
            (3, 2, 12),
            (2, 3, 5),
            (5, 1, 4),
        ] {
            let ratio = RateRatio::new(l as u32, m as u32);
            let mut seed = (l * 1000 + m) as u64;
            let n = l * taps - (l / 3); // not a multiple of L
            let h: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
            for channels in [1usize, 2, 3] {
                let x: Vec<Vec<f64>> = (0..channels)
                    .map(|_| (0..700).map(|_| lcg(&mut seed)).collect())
                    .collect();
                let mut rs = RationalResampler::from_kernel(&h, ratio, 0.0, channels, 300);
                let got = run(&mut rs, &x, &[1, 0, 300, 7, 147, 160, 2, 255]);
                let want_len = ratio.frames_out_total(700) as usize;
                for c in 0..channels {
                    assert_eq!(got[c].len(), want_len, "{l}/{m}");
                    let want = reference(&x[c], &h, l, m, want_len);
                    for (i, (a, b)) in got[c].iter().zip(&want).enumerate() {
                        assert!(
                            (a - b).abs() < 1e-12,
                            "{l}/{m} ch {c} frame {i}: {a} vs {b}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn reset_to_matches_a_runtime_that_saw_those_frames() {
        let ratio = RateRatio::new(160, 147);
        let h = vec![1.0; 160 * 3];
        for pos in [0u64, 1, 5, 146, 147, 148, 1000] {
            let mut a = RationalResampler::from_kernel(&h, ratio, 0.0, 1, 64);
            let mut fed = 0u64;
            let z = vec![0.0; 64];
            while fed < pos {
                let n = (pos - fed).min(64) as usize;
                let k = a.output_frames(n);
                a.process(&[&z[..n]], &mut [&mut vec![0.0; k][..]]);
                fed += n as u64;
            }
            let mut b = RationalResampler::from_kernel(&h, ratio, 0.0, 1, 64);
            b.reset_to(pos);
            assert_eq!(a.input_pos(), b.input_pos());
            for n in 0..64 {
                assert_eq!(a.output_frames(n), b.output_frames(n), "pos {pos} n {n}");
            }
        }
    }

    /// A stereo runtime's left channel is bit-identical to the same signal
    /// through a mono runtime (the fused path sums in the mono order).
    #[test]
    fn stereo_left_equals_mono_bit_for_bit() {
        let mut seed = 11;
        let ratio = RateRatio::new(160, 147);
        let h: Vec<f64> = (0..160 * 37).map(|_| lcg(&mut seed)).collect();
        let x: Vec<f64> = (0..3000).map(|_| lcg(&mut seed)).collect();
        let y: Vec<f64> = (0..3000).map(|_| lcg(&mut seed)).collect();
        let mut mono = RationalResampler::from_kernel(&h, ratio, 0.0, 1, 512);
        let mut stereo = RationalResampler::from_kernel(&h, ratio, 0.0, 2, 512);
        let mut pos = 0;
        for n in [512usize, 1, 300, 77, 512, 2, 511].iter().cycle().take(40) {
            let n = (*n).min(x.len() - pos);
            if n == 0 {
                break;
            }
            let k = mono.output_frames(n);
            assert_eq!(stereo.output_frames(n), k);
            let (mut a, mut l, mut r) = (vec![0.0; k], vec![0.0; k], vec![0.0; k]);
            mono.process(&[&x[pos..pos + n]], &mut [&mut a[..]]);
            stereo.process(
                &[&x[pos..pos + n], &y[pos..pos + n]],
                &mut [&mut l[..], &mut r[..]],
            );
            for (p, q) in a.iter().zip(&l) {
                assert_eq!(p.to_bits(), q.to_bits());
            }
            pos += n;
        }
    }

    #[test]
    fn latency_is_the_kernel_delay_over_m_rounded() {
        let h = vec![0.0; 160 * 11];
        let rs = RationalResampler::from_kernel(&h, RateRatio::new(160, 147), 880.0, 2, 16);
        assert!((rs.latency_out_exact() - 880.0 / 147.0).abs() < 1e-12);
        assert_eq!(rs.latency_out_frames(), 6);
        let rs = RationalResampler::from_kernel(&h, RateRatio::new(1, 2), 5.0, 2, 16);
        assert_eq!(rs.latency_out_frames(), 3); // 2.5 rounds up
    }
}
