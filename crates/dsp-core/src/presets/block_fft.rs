//! Block FFT interpolation (`block_fft*` presets): spectral zero-padding of
//! overlapping windowed blocks.
//!
//! Each block of B input samples is multiplied by a Tukey window (alpha 0.5:
//! cosine tapers over the first and last quarter, flat middle half), taken
//! through a real FFT of size B, and its B/2 + 1 bins are placed at the
//! bottom of an R·B/2 + 1 bin spectrum whose other bins are zero. A real
//! inverse FFT of size R·B, times R, gives R·B output samples: a brick wall
//! at exactly fs_in/2. The old Nyquist bin B/2 is kept at full value, so in
//! the long transform it is an ordinary bin and its real part counts twice:
//!
//! `y[m] = (1/B) (X_0 + 2 Re sum_{k=1}^{B/2} X_k e^{2 pi i k m / (R B)})`.
//!
//! Blocks overlap through a history buffer (each block is the last `hist`
//! samples of the previous one plus `hop = B - hist` new samples), and every
//! emitted sample comes from the flat part of the windows:
//!
//! - `block_fft_fast`: hist B/2, hop B/2; emit the middle half of each block.
//! - `block_fft`, `block_fft_large`: hist 3B/4, hop B/4; emit the average of
//!   the previous block's third quarter and this block's second quarter.
//!
//! B is 16 384 (`block_fft_large`: 2^20 / R, so the output transform is
//! always 2^20 points). Power-of-two ratios only. The response is not
//! time-invariant: it depends on where a sample falls on the block grid.
//!
//! Input and output FIFOs make every `process` call emit exactly `n_in *
//! ratio` frames. A completed block is computed during the next hop (one
//! extra hop of output latency), in steps: a call of `n_in` frames runs at
//! most `ceil(T * n_in / hop)` of the block's T steps.
//!
//! The long inverse transform is split by output phase, with L = min(B,
//! 2^15) and Q = R·B / L: output sample `m = Q n + q` is
//!
//! `y[Q n + q] = (1/B) (X_0 + 2 Re sum_b V_q[b] e^{2 pi i b n / L})`,
//! `V_q[b] = sum_{k = b mod L, 1 <= k <= B/2} X_k e^{2 pi i k q / (R B)}`,
//!
//! one real inverse FFT of size L per phase (on the Hermitian part of V_q).
//! A forward transform larger than 2^15 is split by decimation in time into
//! B / 2^15 real FFTs and a twiddled combination. Each step is one sub-FFT
//! or a bounded slice of twiddle work.

use audivane_protocol::RateRatio;
use std::f64::consts::PI;
use std::sync::Arc;

use realfft::num_complex::Complex64;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use super::{FilterPreset, PresetError, UpsamplerRuntime};

/// Size of the output transform of `block_fft_large`.
const LARGE_OUT: usize = 1 << 20;
/// Input block of `block_fft` and `block_fft_fast`.
const BLOCK: usize = 16_384;
/// Largest FFT run as one step.
const SUB_MAX: usize = 1 << 15;
/// Complex multiply-adds (or samples) per twiddle or deinterleave step.
const TWIDDLE_WORK: usize = 1 << 15;

/// Tukey window, alpha 0.5, length `n`. The ramps are sampled with step 1/n
/// (not 1/(n - 1)) and the window is exactly symmetric with a zero at both
/// ends: with K = n/4, `w[i] = 0.5 cos((2 pi / alpha)(i/n - alpha/2)) + 0.5`
/// for i < K, 1 in the middle, and
/// `0.5 cos((2 pi / alpha)((i + 1)/n + alpha/2 - 1)) + 0.5` for i >= n - K.
pub fn tukey_half(n: usize) -> Vec<f64> {
    let a = 0.5_f64;
    let k = (a * 0.5 * n as f64) as usize;
    let nf = n as f64;
    let c = 2.0 * PI / a;
    (0..n)
        .map(|i| {
            let x = i as f64;
            if i < k {
                0.5 * (c * (x / nf - a / 2.0)).cos() + 0.5
            } else if i >= n - k {
                0.5 * (c * ((x + 1.0) / nf + a / 2.0 - 1.0)).cos() + 0.5
            } else {
                1.0
            }
        })
        .collect()
}

/// The block geometry of a preset at `ratio`: (B, normal overlap?).
fn geometry(p: FilterPreset, ratio: usize) -> (usize, bool) {
    match p {
        FilterPreset::BlockFftFast => (BLOCK, false),
        FilterPreset::BlockFftLarge => (LARGE_OUT / ratio, true),
        _ => (BLOCK, true),
    }
}

/// Build the runtime for `p`.
pub fn build(
    p: FilterPreset,
    _source_rate: u32,
    ratio: usize,
    channels: usize,
    _max_block: usize,
) -> Result<Box<dyn UpsamplerRuntime>, PresetError> {
    super::check_ratio(p, ratio)?;
    let (b, normal) = geometry(p, ratio);
    Ok(Box::new(BlockFft::new(b, ratio, normal, channels)))
}

/// `e^{2 pi i j / N}` from a quarter-period table (exact symmetry for the
/// other quadrants). `N` is a power of two, at least 4.
#[derive(Clone)]
struct Twiddles {
    quarter: Vec<Complex64>,
    mask: usize,
    shift: u32,
}

impl Twiddles {
    fn new(n: usize) -> Self {
        let q = n / 4;
        let quarter = (0..q)
            .map(|j| {
                let (s, c) = (2.0 * PI * j as f64 / n as f64).sin_cos();
                Complex64::new(c, s)
            })
            .collect();
        Self {
            quarter,
            mask: q - 1,
            shift: q.trailing_zeros(),
        }
    }

    /// `e^{2 pi i j / N}` for `j < N`.
    #[inline]
    fn get(&self, j: usize) -> Complex64 {
        let t = self.quarter[j & self.mask];
        match j >> self.shift {
            0 => t,
            1 => Complex64::new(-t.im, t.re),
            2 => Complex64::new(-t.re, -t.im),
            _ => Complex64::new(t.im, -t.re),
        }
    }
}

/// Per-channel stream state.
#[derive(Clone)]
struct Channel {
    /// Input ring of 2B samples. The block being filled starts `fill`
    /// samples before the write position; the block in flight is the B
    /// samples before that block's new part, which the writer does not
    /// reach during the hop it takes to compute.
    inp: Vec<f64>,
    /// Split forward transform: the windowed block as P sub-sequences of M.
    xt: Vec<f64>,
    /// Split forward transform: the P sub-spectra (M/2 + 1 bins each).
    sub: Vec<Complex64>,
    /// The block's spectrum, bins 0 ..= B/2.
    x: Vec<Complex64>,
    /// Normal overlap: the previous block's third quarter (R·B/4 samples).
    acc: Vec<f64>,
    /// Output ring of `cap` samples.
    out: Vec<f64>,
}

/// The block FFT interpolator (see the module docs).
#[derive(Clone)]
pub struct BlockFft {
    ratio: usize,
    b: usize,
    hist: usize,
    hop: usize,
    normal: bool,
    /// Output samples preloaded as zeros (so no call ever underruns): the
    /// part-filled hop plus the hop a block takes to compute.
    pre: usize,
    /// Scale applied to the unnormalised inverse FFTs: `gain * R / (R·B)`,
    /// which is exactly 1/B (a power of two) at gain 1.
    scale: f64,
    window: Vec<f64>,
    /// Forward split: P sub-transforms of size M = B / P.
    p: usize,
    m: usize,
    /// Samples per deinterleave step and their count.
    tr_chunk: usize,
    tr_steps: usize,
    /// Bins per forward combination step.
    comb_chunk: usize,
    comb_steps: usize,
    /// Inverse split: Q phases of size L.
    l: usize,
    q: usize,
    /// Bins per fold step and fold steps per phase.
    fold_chunk: usize,
    fold_steps: usize,
    tw: Twiddles,
    fwd: Arc<dyn RealToComplex<f64>>,
    inv: Arc<dyn ComplexToReal<f64>>,
    // Work buffers, shared by the channels (steps run one at a time).
    sub_in: Vec<f64>,
    v: Vec<Complex64>,
    g: Vec<Complex64>,
    yl: Vec<f64>,
    fwd_scratch: Vec<Complex64>,
    inv_scratch: Vec<Complex64>,
    chans: Vec<Channel>,
    /// Samples in each channel's input block (same for all channels).
    fill: usize,
    /// Input ring write position.
    wpos: usize,
    /// Output ring read position and readable length (all channels).
    out_head: usize,
    out_len: usize,
    /// The block in flight: whether there is one, its steps done, where it
    /// starts in the input ring and where its output goes in the output ring.
    job: bool,
    job_start: usize,
    job_done: usize,
    job_base: usize,
}

impl BlockFft {
    /// `b`: input block (a power of two, at least 8); `normal`: hop B/4
    /// with averaged quarters, otherwise hop B/2 with the middle half.
    pub fn new(b: usize, ratio: usize, normal: bool, channels: usize) -> Self {
        Self::with_split(b, ratio, normal, channels, SUB_MAX, TWIDDLE_WORK)
    }

    /// [`Self::new`] with the largest one-step FFT `sub_max` and the twiddle
    /// work per step `work` (powers of two).
    fn with_split(
        b: usize,
        ratio: usize,
        normal: bool,
        channels: usize,
        sub_max: usize,
        work: usize,
    ) -> Self {
        assert!(b.is_power_of_two() && b >= 8 && ratio.is_power_of_two());
        assert!(sub_max.is_power_of_two() && sub_max >= 8 && work >= 1);
        let hist = if normal { 3 * b / 4 } else { b / 2 };
        let hop = b - hist;
        let rb = ratio * b;
        let m = b.min(sub_max);
        let p = b / m;
        let l = b.min(sub_max);
        let q = rb / l;
        let (comb_chunk, comb_steps) = if p == 1 {
            (0, 0)
        } else {
            let c = (work / p).max(1);
            (c, (b / 2 + 1).div_ceil(c))
        };
        let tr_chunk = work.min(b);
        let tr_steps = if p == 1 { 0 } else { b.div_ceil(tr_chunk) };
        let fold_chunk = work.min(b / 2);
        let fold_steps = (b / 2).div_ceil(fold_chunk);
        let mut planner = RealFftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(m);
        let inv = planner.plan_fft_inverse(l);
        let fwd_scratch = fwd.make_scratch_vec();
        let inv_scratch = inv.make_scratch_vec();
        let pre = ratio * (2 * hop - 1);
        let cap = pre + ratio * hop;
        let zero = Complex64::new(0.0, 0.0);
        let chans = (0..channels.max(1))
            .map(|_| Channel {
                inp: vec![0.0; 2 * b],
                xt: vec![0.0; if p > 1 { b } else { 0 }],
                sub: vec![zero; if p > 1 { p * (m / 2 + 1) } else { 0 }],
                x: vec![zero; b / 2 + 1],
                acc: vec![0.0; if normal { rb / 4 } else { 0 }],
                out: vec![0.0; cap],
            })
            .collect();
        let mut s = Self {
            ratio,
            b,
            hist,
            hop,
            normal,
            pre,
            scale: ratio as f64 / rb as f64,
            window: tukey_half(b),
            p,
            m,
            tr_chunk,
            tr_steps,
            comb_chunk,
            comb_steps,
            l,
            q,
            fold_chunk,
            fold_steps,
            tw: Twiddles::new(rb),
            sub_in: vec![0.0; m],
            v: vec![zero; l],
            g: vec![zero; l / 2 + 1],
            yl: vec![0.0; l],
            fwd,
            inv,
            fwd_scratch,
            inv_scratch,
            chans,
            fill: 0,
            wpos: 0,
            out_head: 0,
            out_len: 0,
            job: false,
            job_start: 0,
            job_done: 0,
            job_base: 0,
        };
        crate::test_support::record("block_fft", || {
            let geometry = [
                s.ratio,
                s.b,
                s.hist,
                s.hop,
                usize::from(s.normal),
                s.pre,
                s.p,
                s.m,
                s.l,
                s.q,
            ];
            let mut w: Vec<u64> = geometry.iter().map(|&v| v as u64).collect();
            w.push(s.scale.to_bits());
            w.extend(s.window.iter().map(|v| v.to_bits()));
            for t in &s.tw.quarter {
                w.push(t.re.to_bits());
                w.push(t.im.to_bits());
            }
            w
        });
        s.reset();
        s
    }

    /// Emitted output samples per block.
    fn emit_len(&self) -> usize {
        self.ratio * self.hop
    }

    /// Output frame at which an impulse in input frame 0 peaks: the output
    /// preload plus the block's own centre delay (hist - B/4 input samples).
    fn centre_delay(&self) -> usize {
        self.ratio * (self.hist - self.b / 4)
    }

    fn fwd_steps(&self) -> usize {
        if self.p == 1 {
            1
        } else {
            self.tr_steps + self.p + self.comb_steps
        }
    }

    fn steps_per_channel(&self) -> usize {
        self.fwd_steps() + self.q * (self.fold_steps + 1)
    }

    fn job_steps(&self) -> usize {
        self.chans.len() * self.steps_per_channel()
    }

    /// Run step `s` of the block in flight.
    fn step(&mut self, s: usize) {
        let per = self.steps_per_channel();
        let ch = &mut self.chans[s / per];
        let t = s % per;
        let fwd_steps = if self.p == 1 {
            1
        } else {
            self.tr_steps + self.p + self.comb_steps
        };
        let half = self.b / 2;
        if t < fwd_steps {
            let (start, in_mask) = (self.job_start, 2 * self.b - 1);
            if self.p == 1 {
                for (i, (v, &w)) in self.sub_in.iter_mut().zip(&self.window).enumerate() {
                    *v = ch.inp[(start + i) & in_mask] * w;
                }
                // Lengths are fixed at construction, so this cannot fail.
                let _ = self.fwd.process_with_scratch(
                    &mut self.sub_in,
                    &mut ch.x,
                    &mut self.fwd_scratch,
                );
                ch.x[0].im = 0.0;
            } else if t < self.tr_steps {
                // Window and deinterleave a slice of the block (sequential
                // reads): sample i goes to sub-sequence i mod P.
                let (i0, i1) = (t * self.tr_chunk, ((t + 1) * self.tr_chunk).min(self.b));
                let (p, m) = (self.p, self.m);
                for i in i0..i1 {
                    ch.xt[(i % p) * m + i / p] = ch.inp[(start + i) & in_mask] * self.window[i];
                }
            } else if t < self.tr_steps + self.p {
                // Sub-transform of the windowed samples s, s + P, s + 2P, ...
                let sp = t - self.tr_steps;
                let (m, mh) = (self.m, self.m / 2 + 1);
                let _ = self.fwd.process_with_scratch(
                    &mut ch.xt[sp * m..(sp + 1) * m],
                    &mut ch.sub[sp * mh..(sp + 1) * mh],
                    &mut self.fwd_scratch,
                );
            } else {
                // X_k = sum_p e^{-2 pi i k p / B} S_p[k mod M], summed over p
                // in order (p outermost keeps the twiddle walk sequential).
                let c = t - self.tr_steps - self.p;
                let (k0, k1) = (
                    c * self.comb_chunk,
                    ((c + 1) * self.comb_chunk).min(half + 1),
                );
                let (mh, mm) = (self.m / 2 + 1, self.m - 1);
                let n_mask = self.ratio * self.b - 1;
                ch.x[k0..k1].fill(Complex64::new(0.0, 0.0));
                for p in 0..self.p {
                    let sub = &ch.sub[p * mh..(p + 1) * mh];
                    let step = p * self.ratio;
                    for (k, x) in (k0..k1).zip(&mut ch.x[k0..k1]) {
                        let j = k & mm;
                        let sp = if j < mh {
                            sub[j]
                        } else {
                            sub[self.m - j].conj()
                        };
                        *x += sp * self.tw.get((k * step) & n_mask).conj();
                    }
                }
                if k0 == 0 {
                    ch.x[0].im = 0.0;
                }
            }
            return;
        }
        let t = t - fwd_steps;
        let phase = t / (self.fold_steps + 1);
        let f = t % (self.fold_steps + 1);
        let n_mask = self.ratio * self.b - 1;
        let l_mask = self.l - 1;
        if f < self.fold_steps {
            // V_q[k mod L] += X_k e^{2 pi i k q / (R B)} for this slice of k.
            if f == 0 {
                self.v.fill(Complex64::new(0.0, 0.0));
            }
            let k0 = 1 + f * self.fold_chunk;
            let k1 = (k0 + self.fold_chunk).min(half + 1);
            for k in k0..k1 {
                self.v[k & l_mask] += ch.x[k] * self.tw.get((k * phase) & n_mask);
            }
            return;
        }
        // The Hermitian part of V (plus X_0) through a real inverse FFT.
        let lh = self.l / 2;
        for (b, g) in self.g.iter_mut().enumerate() {
            *g = self.v[b] + self.v[(self.l - b) & l_mask].conj();
        }
        self.g[0].re += ch.x[0].re;
        self.g[0].im = 0.0;
        self.g[lh].im = 0.0;
        let _ = self
            .inv
            .process_with_scratch(&mut self.g, &mut self.yl, &mut self.inv_scratch);
        let (q, cap, base, scale) = (self.q, ch.out.len(), self.job_base, self.scale);
        let (l4, l2) = (self.l / 4, self.l / 2);
        if self.normal {
            for (i, &v) in self.yl[l4..l2].iter().enumerate() {
                let j = q * i + phase;
                ch.out[(base + j) % cap] = 0.5 * (ch.acc[j] + v * scale);
            }
            for n in l2..3 * l4 {
                ch.acc[q * (n - l2) + phase] = self.yl[n] * scale;
            }
        } else {
            for n in l4..3 * l4 {
                ch.out[(base + q * (n - l4) + phase) % cap] = self.yl[n] * scale;
            }
        }
    }

    /// Run the job's steps up to `target`.
    fn advance(&mut self, target: usize) {
        while self.job_done < target {
            self.step(self.job_done);
            self.job_done += 1;
        }
    }
}

impl UpsamplerRuntime for BlockFft {
    fn ratio(&self) -> RateRatio {
        RateRatio::integer(self.ratio as u32)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        let n = input.first().map_or(0, |c| c.len());
        let r = self.ratio;
        let cap = self.chans[0].out.len();
        let total = self.job_steps();
        let mut pos = 0;
        while pos < n {
            let take = (self.b - self.fill).min(n - pos);
            let len = 2 * self.b;
            let first = take.min(len - self.wpos);
            for (c, ch) in self.chans.iter_mut().enumerate() {
                let src = &input.get(c).unwrap_or(&input[0])[pos..pos + take];
                ch.inp[self.wpos..self.wpos + first].copy_from_slice(&src[..first]);
                ch.inp[..take - first].copy_from_slice(&src[first..]);
            }
            self.wpos = (self.wpos + take) % len;
            self.fill += take;
            if self.job {
                // Pace the block in flight with this hop's input.
                let into = self.fill - self.hist;
                self.advance((total * into).div_ceil(self.hop));
            }
            if self.fill == self.b {
                if self.job {
                    self.out_len += self.emit_len();
                }
                // Start the next block: its output follows what is queued.
                self.job = true;
                self.job_done = 0;
                self.job_base = (self.out_head + self.out_len) % cap;
                self.job_start = (self.wpos + len - self.b) % len;
                self.fill = self.hist;
            }
            // Pull this chunk's output (the preload guarantees it is there).
            let m = take * r;
            debug_assert!(self.out_len >= m);
            for (c, out) in output.iter_mut().enumerate() {
                let ring = &self.chans[c.min(self.chans.len() - 1)].out;
                let dst = &mut out[pos * r..pos * r + m];
                let first = m.min(cap - self.out_head);
                dst[..first].copy_from_slice(&ring[self.out_head..self.out_head + first]);
                dst[first..].copy_from_slice(&ring[..m - first]);
            }
            self.out_head = (self.out_head + m) % cap;
            self.out_len -= m;
            pos += take;
        }
    }

    fn reset(&mut self) {
        // Every buffer is written here, which also maps its pages before
        // the first `process`.
        for ch in &mut self.chans {
            ch.inp.fill(0.0);
            ch.xt.fill(0.0);
            ch.sub.fill(Complex64::new(0.0, 0.0));
            ch.x.fill(Complex64::new(0.0, 0.0));
            ch.acc.fill(0.0);
            ch.out.fill(0.0);
        }
        self.sub_in.fill(0.0);
        self.v.fill(Complex64::new(0.0, 0.0));
        self.g.fill(Complex64::new(0.0, 0.0));
        self.yl.fill(0.0);
        self.fwd_scratch.fill(Complex64::new(0.0, 0.0));
        self.inv_scratch.fill(Complex64::new(0.0, 0.0));
        self.fill = self.hist;
        // The first block's history is the ring's zeros before `hist`.
        self.wpos = self.hist;
        self.job_start = 0;
        self.out_head = 0;
        self.out_len = self.pre;
        self.job = false;
        self.job_done = 0;
        self.job_base = 0;
    }

    fn latency_out_frames(&self) -> usize {
        self.pre + self.centre_delay()
    }

    fn scale_output(&mut self, gain: f64) {
        self.scale = gain * self.ratio as f64 / (self.ratio * self.b) as f64;
    }

    // Every channel runs the same steps on its own data; only the pacing of
    // a block in flight is shared, so a split waits for none to be.
    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        if self.job {
            return None;
        }
        let chans = std::mem::take(&mut self.chans);
        Some(
            chans
                .into_iter()
                .map(|ch| {
                    Box::new(BlockFft {
                        chans: vec![ch],
                        ..self.clone()
                    }) as Box<dyn UpsamplerRuntime>
                })
                .collect(),
        )
    }

    fn impulse_span_out_frames(&self) -> usize {
        // An impulse in the first hop of input lies in the blocks
        // 0 ..= hist/hop (in frame 0 it falls on the last one's zero window
        // sample, so frames 1 .. hop are the longest case); in normal mode
        // the last block's third quarter is emitted by the block after it.
        // Counted from the first call, this bounds the tail of an impulse
        // anywhere in the first hop.
        let last_block = self.hist / self.hop + usize::from(self.normal);
        self.pre + (last_block + 1) * self.emit_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(rt: &mut dyn UpsamplerRuntime, x: &[f64], block: usize) -> Vec<f64> {
        let r = rt.ratio().up as usize;
        let mut y = vec![0.0; x.len() * r];
        for (i, c) in x.chunks(block).enumerate() {
            let o = i * block * r;
            rt.process(&[c], &mut [&mut y[o..o + c.len() * r]]);
        }
        y
    }

    fn small(normal: bool, ratio: usize) -> BlockFft {
        BlockFft::new(64, ratio, normal, 1)
    }

    #[test]
    fn tukey_shape() {
        let w = tukey_half(16);
        assert_eq!(w[0], 0.0);
        assert_eq!(w[15], 0.0);
        for i in 0..16 {
            assert_eq!(w[i], w[15 - i], "symmetric at {i}");
        }
        assert!(w[4..12].iter().all(|&v| v == 1.0));
        // 0.5 (1 - cos(pi i / K)) with K = 4.
        for (i, &v) in w.iter().enumerate().take(4).skip(1) {
            let e = 0.5 * (1.0 - (PI * i as f64 / 4.0).cos());
            assert!((v - e).abs() < 1e-15);
        }
    }

    #[test]
    fn geometry_per_preset() {
        assert_eq!(geometry(FilterPreset::BlockFft, 8), (16_384, true));
        assert_eq!(geometry(FilterPreset::BlockFftFast, 8), (16_384, false));
        assert_eq!(geometry(FilterPreset::BlockFftLarge, 8), (131_072, true));
        assert_eq!(geometry(FilterPreset::BlockFftLarge, 2), (524_288, true));
        assert!(build(FilterPreset::BlockFft, 44_100, 6, 2, 512).is_err());
    }

    #[test]
    fn impulse_peaks_at_latency_and_ends_within_span() {
        for normal in [true, false] {
            for ratio in [2, 4, 8] {
                let mut rt = small(normal, ratio);
                let span = rt.impulse_span_out_frames();
                let n = span.div_ceil(ratio) + 64;
                let mut x = vec![0.0; n];
                x[0] = 1.0;
                let y = run(&mut rt, &x, 7);
                let peak = y
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                    .unwrap()
                    .0;
                assert_eq!(peak, rt.latency_out_frames(), "normal {normal} x{ratio}");
                assert!((y[peak] - 1.0).abs() < 0.05, "unity gain: {}", y[peak]);
                assert!(y[span..].iter().all(|&v| v == 0.0), "span covers the tail");
                // An impulse in frame 1 also reaches the block that starts
                // at frame 0 (where the window is zero); the span is tight
                // for it.
                rt.reset();
                x[0] = 0.0;
                x[1] = 1.0;
                let y = run(&mut rt, &x, 7);
                let last = span - rt.emit_len();
                assert!(y[last..span].iter().any(|&v| v != 0.0), "span is tight");
                assert!(y[span..].iter().all(|&v| v == 0.0), "span covers the tail");
            }
        }
    }

    #[test]
    fn output_independent_of_call_sizes() {
        for normal in [true, false] {
            let mut seed = 1u64;
            let x: Vec<f64> = (0..1000)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
                })
                .collect();
            let a = run(&mut small(normal, 4), &x, 1);
            let b = run(&mut small(normal, 4), &x, 37);
            let c = run(&mut small(normal, 4), &x, 1000);
            assert_eq!(a, b);
            assert_eq!(a, c);
            let mut rt = small(normal, 4);
            let _ = run(&mut rt, &x, 100);
            rt.reset();
            assert_eq!(run(&mut rt, &x, 13), a, "reset restores the start state");
        }
    }

    #[test]
    fn dc_passes_at_unity_and_gain_scales() {
        for normal in [true, false] {
            let x = vec![0.25; 8000];
            let mk = || BlockFft::new(1024, 2, normal, 1);
            let y = run(&mut mk(), &x, 50);
            // Past the start-up, a constant comes out at unity gain, up to
            // the small ripple of the block edges.
            for &v in &y[4000..12000] {
                assert!((v - 0.25).abs() < 1e-4, "{v}");
            }
            let mut q = mk();
            q.scale_output(0.5);
            let z = run(&mut q, &x, 50);
            for (a, b) in y.iter().zip(&z) {
                assert!((a * 0.5 - b).abs() < 1e-15);
            }
        }
    }

    #[test]
    fn stereo_channels_are_independent() {
        let mut rt = BlockFft::new(64, 2, true, 2);
        let n = 300;
        let mut l = vec![0.0; n];
        l[5] = 1.0;
        let r = vec![0.0; n];
        let (mut yl, mut yr) = (vec![0.0; 2 * n], vec![0.0; 2 * n]);
        rt.process(&[&l, &r], &mut [&mut yl, &mut yr]);
        assert!(yl.iter().any(|&v| v != 0.0));
        assert!(yr.iter().all(|&v| v == 0.0));
    }

    /// The block stream by its explicit formula, with a naive DFT:
    /// y[m] = (1/B)(X_0 + 2 sum_{k=1}^{B/2} Re(X_k e^{2 pi i k m / (R B)})).
    fn naive(x: &[f64], b: usize, r: usize, normal: bool) -> Vec<f64> {
        let hist = if normal { 3 * b / 4 } else { b / 2 };
        let hop = b - hist;
        let w = tukey_half(b);
        let mut xs = vec![0.0; hist];
        xs.extend_from_slice(x);
        let rb = r * b;
        let mut acc = vec![0.0; rb / 4];
        let mut out = Vec::new();
        let mut s = 0;
        while s + b <= xs.len() {
            let seg: Vec<f64> = (0..b).map(|n| xs[s + n] * w[n]).collect();
            let bins: Vec<(f64, f64)> = (0..=b / 2)
                .map(|k| {
                    seg.iter()
                        .enumerate()
                        .fold((0.0, 0.0), |(re, im), (n, &v)| {
                            let t = -2.0 * PI * (k * n) as f64 / b as f64;
                            (re + v * t.cos(), im + v * t.sin())
                        })
                })
                .collect();
            let y: Vec<f64> = (0..rb)
                .map(|m| {
                    let mut v = bins[0].0;
                    for (k, &(re, im)) in bins.iter().enumerate().skip(1) {
                        let t = 2.0 * PI * ((k * m) % rb) as f64 / rb as f64;
                        v += 2.0 * (re * t.cos() - im * t.sin());
                    }
                    v / b as f64
                })
                .collect();
            if normal {
                let q = rb / 4;
                out.extend((0..q).map(|k| 0.5 * (acc[k] + y[q + k])));
                acc.copy_from_slice(&y[2 * q..3 * q]);
            } else {
                out.extend_from_slice(&y[rb / 4..3 * rb / 4]);
            }
            s += hop;
        }
        out
    }

    fn noise(n: usize, seed: u64) -> Vec<f64> {
        let mut seed = seed;
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
            })
            .collect()
    }

    /// Run with varying call sizes, including 0, 1, odd ones and calls
    /// longer than a hop.
    fn run_mixed(rt: &mut BlockFft, x: &[f64]) -> Vec<f64> {
        let r = rt.ratio;
        let sizes = [0, 1, 7, 0, 3 * rt.hop + 5, 1, 2, 31, rt.hop, 13];
        let mut y = vec![0.0; x.len() * r];
        let (mut pos, mut i) = (0, 0);
        while pos < x.len() {
            let n = sizes[i % sizes.len()].min(x.len() - pos);
            i += 1;
            let (a, b) = (pos * r, (pos + n) * r);
            rt.process(&[&x[pos..pos + n]], &mut [&mut y[a..b]]);
            pos += n;
        }
        y
    }

    #[test]
    fn matches_the_explicit_formula() {
        // (B, R, largest one-step FFT, twiddle work): unsplit, folded
        // inverse with split forward, and fine twiddle slices.
        for (b, r, sub, work) in [(32, 4, 1 << 15, 1 << 16), (64, 2, 16, 8), (64, 8, 8, 3)] {
            for normal in [true, false] {
                let x = noise(1000, 3);
                let mut rt = BlockFft::with_split(b, r, normal, 1, sub, work);
                let pre = rt.pre;
                let y = run_mixed(&mut rt, &x);
                let e = naive(&x, b, r, normal);
                assert!(y[..pre].iter().all(|&v| v == 0.0));
                let n = (y.len() - pre).min(e.len());
                assert!(n > 500);
                for i in 0..n {
                    assert!(
                        (y[pre + i] - e[i]).abs() < 1e-12,
                        "B {b} R {r} sub {sub} normal {normal} at {i}"
                    );
                }
            }
        }
    }

    #[test]
    fn work_is_spread_over_the_hop() {
        for normal in [true, false] {
            let mut rt = BlockFft::with_split(256, 4, normal, 2, 16, 8);
            let total = rt.job_steps();
            assert!(total > 100);
            let x = noise(64, 5);
            let mut y = vec![vec![0.0; 4 * 4]; 2];
            let mut worst = 0;
            for c in x.chunks(4).cycle().take(2000) {
                let before = rt.job_done;
                let started = rt.fill + c.len() >= rt.b;
                let [a, b] = &mut y[..] else { unreachable!() };
                rt.process(&[c, c], &mut [&mut a[..], &mut b[..]]);
                if !started {
                    worst = worst.max(rt.job_done - before);
                }
            }
            // A 4-frame call runs about 4/hop of a block's steps.
            assert!(worst <= (total * 4).div_ceil(rt.hop) + 1, "worst {worst}");
        }
    }

    #[test]
    fn large_geometry_splits_into_small_steps() {
        for r in [2, 8, 32] {
            let rt = BlockFft::new(LARGE_OUT / r, r, true, 1);
            assert!(rt.m <= SUB_MAX && rt.l <= SUB_MAX);
            assert_eq!(rt.q * rt.l, LARGE_OUT);
            assert!(rt.fold_chunk <= TWIDDLE_WORK && rt.comb_chunk * rt.p <= TWIDDLE_WORK);
        }
    }
}
