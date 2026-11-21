//! One FIR stage (zero-stuff by f, filter, times f) as a polyphase FFT
//! block convolution: the f branches `h_r[j] = f h[j f + r]` share one
//! forward FFT of `[previous block, current block]` (length 2B, B input
//! frames per block, B >= the branch length), each branch takes one
//! inverse FFT, and the output lags by one block (B f output frames).
//!
//! The branch spectra are the exact DFT of the branch (double-double,
//! rounded once), not an FFT of it: the FFT's own twiddle rounding would
//! perturb the kernel, which adds a systematic error to every block.

use super::design;
use crate::fft::{RealFftPair, SplitSpectrum, complex_mul_split};

pub(super) struct FirStage {
    f: usize,
    block: usize,
    fft: RealFftPair,
    /// Branch spectra, scaled by 1 / (2B) (the inverse FFT is unscaled).
    branch: Vec<SplitSpectrum>,
    prev: Vec<f64>,
    cur: Vec<f64>,
    /// Input frames in `cur`.
    fill: usize,
    spec: SplitSpectrum,
    acc: SplitSpectrum,
    time: Vec<f64>,
    /// The last block's output, interleaved; the next `pending` frames of
    /// it (from its end backwards) are still to be handed out.
    out: Vec<f64>,
    pending: usize,
}

impl FirStage {
    /// `h` at the stage output rate, unit DC gain (the stage gain f is
    /// applied here).
    pub fn new(h: &[f64], f: usize) -> Self {
        crate::test_support::record("cascade_stage", || {
            crate::test_support::words(&[f as u64, h.len() as u64], h)
        });
        let block = h.len().div_ceil(f).next_power_of_two().max(8);
        let m = 2 * block;
        let fft = RealFftPair::new(m);
        let gain = f as f64;
        let scale = 1.0 / m as f64;
        let branch = (0..f)
            .map(|r| {
                let taps: Vec<f64> = h.iter().skip(r).step_by(f).map(|v| v * gain).collect();
                let (re, im) = design::dft_dd(&taps, m);
                let mut s = SplitSpectrum::new(block + 1);
                for k in 0..=block {
                    s.re[k] = re[k] * scale;
                    s.im[k] = im[k] * scale;
                }
                s
            })
            .collect();
        Self {
            f,
            block,
            fft,
            branch,
            prev: vec![0.0; block],
            cur: vec![0.0; block],
            fill: 0,
            spec: SplitSpectrum::new(block + 1),
            acc: SplitSpectrum::new(block + 1),
            time: vec![0.0; m],
            out: vec![0.0; block * f],
            pending: block * f,
        }
    }

    /// Delay in stage output frames.
    pub fn block_latency(&self) -> usize {
        self.block * self.f
    }

    /// Scale the output (folded into the branch spectra).
    pub fn scale(&mut self, gain: f64) {
        for s in &mut self.branch {
            for v in s.re.iter_mut().chain(s.im.iter_mut()) {
                *v *= gain;
            }
        }
    }

    pub fn reset(&mut self) {
        self.prev.fill(0.0);
        self.cur.fill(0.0);
        self.fill = 0;
        self.out.fill(0.0);
        self.pending = self.block * self.f;
    }

    /// `output.len() == input.len() * f`.
    pub fn process(&mut self, input: &[f64], output: &mut [f64]) {
        let (f, b) = (self.f, self.block);
        let mut i = 0;
        while i < input.len() {
            let c = (b - self.fill).min(input.len() - i);
            self.cur[self.fill..self.fill + c].copy_from_slice(&input[i..i + c]);
            let from = b * f - self.pending;
            output[i * f..(i + c) * f].copy_from_slice(&self.out[from..from + c * f]);
            self.fill += c;
            self.pending -= c * f;
            i += c;
            if self.fill == b {
                self.run_block();
                self.fill = 0;
                self.pending = b * f;
            }
        }
    }

    fn run_block(&mut self) {
        let (f, b) = (self.f, self.block);
        self.fft
            .forward_pair_into_split(&self.prev, &self.cur, &mut self.spec);
        for r in 0..f {
            complex_mul_split(&mut self.acc, &self.spec, &self.branch[r]);
            self.fft.inverse_from_split(&self.acc, &mut self.time);
            for (j, &v) in self.time[b..].iter().enumerate() {
                self.out[j * f + r] = v;
            }
        }
        std::mem::swap(&mut self.prev, &mut self.cur);
    }
}
