//! Shared real-FFT helpers, split-complex MAC kernels, and fixed-capacity FIFOs
//! used by the convolver and the upsampler.

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex64;
use std::sync::Arc;

/// Split-complex (Structure-of-Arrays) spectrum buffer for a real FFT of
/// length `2 * (len - 1)`. Storing real and imaginary parts in separate
/// contiguous slices avoids shuffle instructions in complex multiply-accumulate
/// loops so LLVM autovectorizes cleanly.
#[derive(Clone, Debug)]
pub struct SplitSpectrum {
    pub re: Vec<f64>,
    pub im: Vec<f64>,
}

impl SplitSpectrum {
    pub fn new(spec_len: usize) -> Self {
        Self {
            re: vec![0.0; spec_len],
            im: vec![0.0; spec_len],
        }
    }

    pub fn len(&self) -> usize {
        self.re.len()
    }

    pub fn is_empty(&self) -> bool {
        self.re.is_empty()
    }

    pub fn clear(&mut self) {
        self.re.fill(0.0);
        self.im.fill(0.0);
    }

    pub fn from_complex(src: &[Complex64]) -> Self {
        let mut out = Self::new(src.len());
        out.copy_from_complex(src);
        out
    }

    #[inline]
    pub fn copy_from_complex(&mut self, src: &[Complex64]) {
        assert_eq!(self.re.len(), src.len());
        for (i, c) in src.iter().enumerate() {
            self.re[i] = c.re;
            self.im[i] = c.im;
        }
    }

    #[inline]
    pub fn copy_to_complex(&self, dst: &mut [Complex64]) {
        assert_eq!(self.re.len(), dst.len());
        for (i, c) in dst.iter_mut().enumerate() {
            c.re = self.re[i];
            c.im = self.im[i];
        }
    }
}

/// Complex multiply: `dst = a * b`.
#[inline]
pub fn complex_mul_split(dst: &mut SplitSpectrum, a: &SplitSpectrum, b: &SplitSpectrum) {
    let n = dst.re.len();
    complex_mul_split_range(dst, a, b, 0..n);
}

/// Complex multiply-accumulate: `acc += a * b`.
#[inline]
pub fn complex_mac_split(acc: &mut SplitSpectrum, a: &SplitSpectrum, b: &SplitSpectrum) {
    let n = acc.re.len();
    complex_mac_split_range(acc, a, b, 0..n);
}

#[inline]
fn assert_same_len(dst: &SplitSpectrum, a: &SplitSpectrum, b: &SplitSpectrum) {
    let n = dst.re.len();
    assert!(
        dst.im.len() == n
            && a.re.len() == n
            && a.im.len() == n
            && b.re.len() == n
            && b.im.len() == n
    );
}

/// Complex multiply over the bins in `range` only: `dst[r] = a[r] * b[r]`.
#[inline]
pub fn complex_mul_split_range(
    dst: &mut SplitSpectrum,
    a: &SplitSpectrum,
    b: &SplitSpectrum,
    range: std::ops::Range<usize>,
) {
    assert_same_len(dst, a, b);
    let dst_re = &mut dst.re[range.clone()];
    let dst_im = &mut dst.im[range.clone()];
    let n = dst_re.len();
    let a_re = &a.re[range.clone()][..n];
    let a_im = &a.im[range.clone()][..n];
    let b_re = &b.re[range.clone()][..n];
    let b_im = &b.im[range][..n];
    let dst_im = &mut dst_im[..n];

    for i in 0..n {
        let ar = a_re[i];
        let ai = a_im[i];
        let br = b_re[i];
        let bi = b_im[i];
        dst_re[i] = ar * br - ai * bi;
        dst_im[i] = ar * bi + ai * br;
    }
}

/// Complex multiply-accumulate over the bins in `range` only:
/// `acc[r] += a[r] * b[r]`.
#[inline]
pub fn complex_mac_split_range(
    acc: &mut SplitSpectrum,
    a: &SplitSpectrum,
    b: &SplitSpectrum,
    range: std::ops::Range<usize>,
) {
    assert_same_len(acc, a, b);
    let acc_re = &mut acc.re[range.clone()];
    let acc_im = &mut acc.im[range.clone()];
    let n = acc_re.len();
    let a_re = &a.re[range.clone()][..n];
    let a_im = &a.im[range.clone()][..n];
    let b_re = &b.re[range.clone()][..n];
    let b_im = &b.im[range][..n];
    let acc_im = &mut acc_im[..n];

    for i in 0..n {
        let ar = a_re[i];
        let ai = a_im[i];
        let br = b_re[i];
        let bi = b_im[i];
        acc_re[i] += ar * br - ai * bi;
        acc_im[i] += ar * bi + ai * br;
    }
}

/// Pre-planned real-to-complex / complex-to-real FFT pair with pre-allocated
/// scratch buffers so forward/inverse transforms never allocate.
pub struct RealFftPair {
    fft_len: usize,
    spec_len: usize,
    fwd: Arc<dyn RealToComplex<f64>>,
    inv: Arc<dyn ComplexToReal<f64>>,
    fwd_scratch: Vec<Complex64>,
    inv_scratch: Vec<Complex64>,
    time_buf: Vec<f64>,
    spec_buf: Vec<Complex64>,
}

impl RealFftPair {
    /// Plan a real FFT pair of length `fft_len` (must be even and >= 2).
    pub fn new(fft_len: usize) -> Self {
        assert!(fft_len >= 2 && fft_len.is_multiple_of(2));
        let mut planner = RealFftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(fft_len);
        let inv = planner.plan_fft_inverse(fft_len);
        let fwd_scratch = fwd.make_scratch_vec();
        let inv_scratch = inv.make_scratch_vec();
        let spec_len = fft_len / 2 + 1;
        Self {
            fft_len,
            spec_len,
            fwd,
            inv,
            fwd_scratch,
            inv_scratch,
            time_buf: vec![0.0; fft_len],
            spec_buf: vec![Complex64::new(0.0, 0.0); spec_len],
        }
    }

    pub fn fft_len(&self) -> usize {
        self.fft_len
    }

    pub fn spec_len(&self) -> usize {
        self.spec_len
    }

    /// Zero-pad `input` (length <= `fft_len`), run the forward real FFT, and
    /// return its spectrum scaled by `scale` as a `SplitSpectrum`.
    pub fn forward_padded_scaled(&mut self, input: &[f64], scale: f64) -> SplitSpectrum {
        let mut out = SplitSpectrum::new(self.spec_len);
        self.forward_padded_into_split(input, scale, &mut out);
        out
    }

    /// Zero-pad `input` (length <= `fft_len`), run the forward real FFT, and
    /// write its spectrum scaled by `scale` into `dst`.
    pub fn forward_padded_into_split(
        &mut self,
        input: &[f64],
        scale: f64,
        dst: &mut SplitSpectrum,
    ) {
        assert!(input.len() <= self.fft_len);
        assert_eq!(dst.len(), self.spec_len);
        self.time_buf.fill(0.0);
        self.time_buf[..input.len()].copy_from_slice(input);
        self.fwd
            .process_with_scratch(
                &mut self.time_buf,
                &mut self.spec_buf,
                &mut self.fwd_scratch,
            )
            .expect("realfft forward failed");
        for (i, c) in self.spec_buf.iter().enumerate() {
            dst.re[i] = c.re * scale;
            dst.im[i] = c.im * scale;
        }
    }

    /// Forward FFT of `[prev, curr]` (each of length `fft_len / 2`), storing
    /// the unscaled spectrum into `dst`. Zero allocations.
    pub fn forward_pair_into_split(&mut self, prev: &[f64], curr: &[f64], dst: &mut SplitSpectrum) {
        let half = self.fft_len / 2;
        assert_eq!(prev.len(), half);
        assert_eq!(curr.len(), half);
        assert_eq!(dst.len(), self.spec_len);

        self.time_buf[..half].copy_from_slice(prev);
        self.time_buf[half..].copy_from_slice(curr);
        self.fwd
            .process_with_scratch(
                &mut self.time_buf,
                &mut self.spec_buf,
                &mut self.fwd_scratch,
            )
            .expect("realfft forward failed");
        dst.copy_from_complex(&self.spec_buf);
    }

    /// Inverse real FFT of `src`, writing the full `fft_len` real samples into
    /// `out` (unnormalized, i.e. scaled by `fft_len` unless the spectrum was
    /// pre-scaled). Zero allocations.
    pub fn inverse_from_split(&mut self, src: &SplitSpectrum, out: &mut [f64]) {
        assert_eq!(src.len(), self.spec_len);
        assert_eq!(out.len(), self.fft_len);
        src.copy_to_complex(&mut self.spec_buf);
        // realfft requires DC and Nyquist imaginary parts to be strictly zero.
        self.spec_buf[0].im = 0.0;
        self.spec_buf[self.spec_len - 1].im = 0.0;
        self.inv
            .process_with_scratch(&mut self.spec_buf, out, &mut self.inv_scratch)
            .expect("realfft inverse failed");
    }

    /// Forward FFT of a full `fft_len`-sample real vector (zero-padded if
    /// shorter) into `dst`. Used offline (e.g. cepstrum design).
    pub fn forward_padded_complex(&mut self, input: &[f64], dst: &mut [Complex64]) {
        assert!(input.len() <= self.fft_len);
        assert_eq!(dst.len(), self.spec_len);
        self.time_buf.fill(0.0);
        self.time_buf[..input.len()].copy_from_slice(input);
        self.fwd
            .process_with_scratch(&mut self.time_buf, dst, &mut self.fwd_scratch)
            .expect("realfft forward failed");
    }

    /// Inverse real FFT of `src` (mutated by `realfft`) into `out`, scaled by
    /// `1.0 / fft_len`. Used offline (e.g. cepstrum design).
    pub fn inverse_complex_normalized(&mut self, src: &mut [Complex64], out: &mut [f64]) {
        assert_eq!(src.len(), self.spec_len);
        assert_eq!(out.len(), self.fft_len);
        src[0].im = 0.0;
        src[self.spec_len - 1].im = 0.0;
        self.inv
            .process_with_scratch(src, out, &mut self.inv_scratch)
            .expect("realfft inverse failed");
        let inv_n = 1.0 / (self.fft_len as f64);
        for v in out.iter_mut() {
            *v *= inv_n;
        }
    }
}

/// Fixed-capacity ring-buffer FIFO for `f64` samples. Never allocates after
/// construction.
#[derive(Clone, Debug)]
pub struct RingFifo {
    buf: Vec<f64>,
    head: usize,
    len: usize,
}

impl RingFifo {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: vec![0.0; cap.max(1)],
            head: 0,
            len: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Reset to `count` zero samples.
    pub fn reset_zeros(&mut self, count: usize) {
        assert!(count <= self.buf.len());
        self.head = 0;
        self.len = count;
        self.buf[..count].fill(0.0);
    }

    #[inline]
    pub fn push_slice(&mut self, src: &[f64]) {
        let cap = self.buf.len();
        assert!(self.len + src.len() <= cap, "RingFifo overflow");
        if src.is_empty() {
            return;
        }
        let tail = (self.head + self.len) % cap;
        let first = (cap - tail).min(src.len());
        self.buf[tail..tail + first].copy_from_slice(&src[..first]);
        if first < src.len() {
            let rem = src.len() - first;
            self.buf[..rem].copy_from_slice(&src[first..]);
        }
        self.len += src.len();
    }

    #[inline]
    pub fn pop_slice(&mut self, dst: &mut [f64]) {
        assert!(dst.len() <= self.len, "RingFifo underflow");
        if dst.is_empty() {
            return;
        }
        let cap = self.buf.len();
        let first = (cap - self.head).min(dst.len());
        dst[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        if first < dst.len() {
            let rem = dst.len() - first;
            dst[first..].copy_from_slice(&self.buf[..rem]);
        }
        self.head = (self.head + dst.len()) % cap;
        self.len -= dst.len();
    }
}
