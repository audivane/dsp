//! Non-uniformly partitioned overlap-save convolution (NUPC), shared by the
//! convolver ([`crate::conv::Convolver`]) and the FFT polyphase upsampler
//! ([`crate::resample::Upsampler`]).
//!
//! # Partition schedule
//!
//! The filter is split into stages. Stage 0 (the head) is uniformly
//! partitioned overlap-save (UPOLS) with the base block `B0` and runs
//! immediately on every base block, so the latency is exactly `B0` input
//! frames (the FIFO pre-fill).
//!
//! Every later ("tail") stage `s` is UPOLS with a larger block `Bs` covering
//! filter samples `[Os, Os+1)`. Input block `k` of that stage (`Bs` frames)
//! is complete at `(k+1)·Bs`, and its sub-convolution belongs at output time
//! `k·Bs + Os`. The stage computes block `k` during the next period and
//! plays it out in the one after, so its inherent delay is `2·Bs`. The
//! schedule therefore requires `Os >= 2·Bs` and makes up the difference
//! `Os - 2·Bs` (a multiple of `Bs`) exactly by reading older frequency-domain
//! delay line slots. The default schedule ([`plan_schedule`]) grows blocks
//! 4x per stage and starts every tail stage at `Os = 2·Bs`:
//!
//! | stage | block  | filter range     | partitions |
//! |-------|--------|------------------|------------|
//! | 0     | `B0`   | `[0, 8·B0)`      | 8          |
//! | 1     | `4·B0` | `[8·B0, 32·B0)`  | 6          |
//! | s     | `B`    | `[2B, 8B)`       | 6          |
//!
//! Blocks are capped ([`MAX_PART`] by default): the stage before the cap ends
//! at twice the cap (2 or 6 partitions depending on `B0`), and the last stage
//! covers the rest of the filter.
//!
//! # Branches and cost
//!
//! A stage convolves one input with `branches` filters at once (the
//! polyphase branches of an upsampler; one for the convolver): one forward
//! transform and one frequency-domain delay line serve every branch, each
//! with its own multiply-accumulate and inverse transform. The head keeps one
//! accumulator per branch and sweeps the delay line once; a tail stage runs
//! the branches one after the other through one accumulator, which keeps it
//! in cache at any ratio. Stage outputs come out interleaved: frame `i` of
//! branch `r` at `i * branches + r`.
//!
//! Each tail stage has a whole period of slack, so its work (forward FFT,
//! multiply-accumulates, inverse FFTs) is time-sliced over the `Bs / B0` base
//! blocks of that period ([`Schedule`]), weighted by a measured per-job cost
//! model. Transforms with `Bs > DIRECT_FFT_MAX_BLOCK` use a four-step
//! decomposition (`Bs = M·L`), so the largest indivisible job is a few
//! sub-FFTs of size ~`sqrt(Bs)` (a few µs); smaller stages use one real FFT
//! per job. The worst base block is therefore close to the mean. Slicing only
//! decides when a job runs, never what it computes.

use crate::Complex64;
use crate::fft::{
    RealFftPair, SplitSpectrum, complex_mac_split, complex_mac_split_range, complex_mul_split,
    complex_mul_split_range,
};
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Largest partition size of any stage (the convolver's cap; the
/// upsampler's is lower at high ratios, [`polyphase_max_part`]).
pub(crate) const MAX_PART: usize = 32_768;
/// Block growth factor between consecutive stages.
const GROWTH: usize = 4;
/// Tail stages with blocks up to this size use one real FFT per transform;
/// larger ones use the time-sliced four-step transform.
pub(crate) const DIRECT_FFT_MAX_BLOCK: usize = 512;
/// Frequency bins per multiply-accumulate job of a one-branch stage (the
/// convolver's).
const MAC_CHUNK: usize = 128;
/// Frequency bins per multiply-accumulate job of a polyphase stage: each
/// job sweeps every partition of one branch over this many bins; shorter
/// sweeps restart the prefetch streams too often. A job stays well under
/// 0.2 ms.
const POLY_MAC_CHUNK: usize = 2048;
/// Target complex elements per batch of four-step sub-FFTs.
const SUBFFT_BATCH: usize = 256;
/// The most interleaved output frames (`block * branches`) of an
/// upsampler's largest stage: bounds its output double buffer to 8 MiB per
/// channel at any ratio ([`polyphase_max_part`]).
const POLY_MAX_OUT: usize = 1 << 19;

/// One stage of a partition schedule: UPOLS with `block`-frame partitions
/// covering filter samples `[offset, end)` (`end == usize::MAX` for the
/// last).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagePlan {
    pub offset: usize,
    pub block: usize,
    pub end: usize,
}

/// The default schedule for a base block `base` and filter length
/// `max_len`, blocks capped at [`MAX_PART`].
pub(crate) fn plan_schedule(base: usize, max_len: usize) -> Vec<StagePlan> {
    plan_schedule_capped(base, max_len, MAX_PART)
}

/// The default schedule with blocks capped at `max_part` (a power of two,
/// at least `base`).
pub(crate) fn plan_schedule_capped(base: usize, max_len: usize, max_part: usize) -> Vec<StagePlan> {
    let mut plan = Vec::new();
    let mut block = base;
    let mut offset = 0;
    loop {
        let next_block = (block * GROWTH).min(max_part);
        // The next stage starts at exactly twice its block size.
        let boundary = 2 * next_block;
        let last =
            (block == max_part && !plan.is_empty()) || next_block <= block || boundary >= max_len;
        let end = if last { usize::MAX } else { boundary };
        plan.push(StagePlan { offset, block, end });
        if last {
            return plan;
        }
        offset = end;
        block = next_block;
    }
}

pub(crate) fn validate_plan(plan: &[StagePlan], base: usize) {
    assert!(!plan.is_empty(), "empty partition schedule");
    assert_eq!(plan[0].offset, 0, "stage 0 must start at IR offset 0");
    assert_eq!(plan[0].block, base, "stage 0 must use the base block");
    assert_eq!(
        plan.last().unwrap().end,
        usize::MAX,
        "the last stage must be open-ended"
    );
    for (s, st) in plan.iter().enumerate() {
        assert!(st.end > st.offset, "stage {s} is empty: {st:?}");
        if s == 0 {
            continue;
        }
        assert_eq!(st.offset, plan[s - 1].end, "stage {s} is not contiguous");
        assert!(
            st.block.is_power_of_two() && st.block >= base && st.block.is_multiple_of(base),
            "stage {s}: block {} must be a power-of-two multiple of {base}",
            st.block
        );
        assert!(
            st.offset >= 2 * st.block && st.offset.is_multiple_of(st.block),
            "stage {s}: offset {} must be a multiple of the block {} and at least twice it",
            st.offset,
            st.block
        );
    }
}

// ------------------------------------------------------------ cost model

// Job costs, in nanoseconds per element, measured on one x86-64 core in a
// release build. Only their ratios matter to the slicing: they decide how
// the work of a period is spread over its base blocks. The polyphase
// planner ([`polyphase_plan`]) compares whole schedules with them.

/// Direct real FFT of `2B` points, per `B·log2(2B)`.
const NS_REAL_FFT: f64 = 0.42;
/// Spectral multiply-accumulate, per bin and partition.
pub(crate) const NS_MAC: f64 = 0.75;
/// Four-step forward step A (gather, sub-FFTs, twiddle, scatter), per element.
const NS_FWD_A: f64 = 4.4;
/// Four-step inverse step A (gather, sub-FFTs, unpack), per element.
const NS_INV_A: f64 = 2.8;
/// Four-step step B (contiguous sub-FFTs), per element and `log2(l)`.
const NS_STEP_B: f64 = 0.22;
/// Real-FFT split after the forward step B, per bin.
const NS_SPLIT: f64 = 2.9;
/// Hermitian pack and twiddle around the inverse step B, per bin.
const NS_PACK: f64 = 3.8;

pub(crate) fn real_fft_cost(block: usize) -> f64 {
    NS_REAL_FFT * block as f64 * (2.0 * block as f64).log2()
}

/// Rough cost of one head block in the model's units: `1 + branches` real
/// FFTs of `2 * block` points and `parts * branches` spectra of
/// multiply-accumulate.
pub(crate) fn head_block_cost(block: usize, branches: usize, parts: usize) -> f64 {
    if parts == 0 {
        return 0.0;
    }
    (1 + branches) as f64 * real_fft_cost(block)
        + NS_MAC * (block + 1) as f64 * (parts * branches) as f64
}

// -------------------------------------------------------- four-step FFT

/// Sizes of a four-step transform of `b = m * l` points and its job
/// granularity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dims {
    pub b: usize,
    pub m: usize,
    pub l: usize,
    /// Columns per step-A job (forward and inverse).
    pub cols_a: usize,
    /// Rows per inverse step-B job.
    pub rows_b: usize,
}

impl Dims {
    pub fn new(b: usize) -> Self {
        assert!(b.is_power_of_two() && b >= 16);
        let bits = b.trailing_zeros();
        let m = 1usize << (bits / 2);
        let l = b / m;
        Self {
            b,
            m,
            l,
            cols_a: (SUBFFT_BATCH / m).clamp(1, l),
            rows_b: (SUBFFT_BATCH / l).clamp(1, m),
        }
    }

    /// Number of forward step-B jobs: row pairs `{k1, m - k1}` for
    /// `k1 in 0..=m/2`.
    fn fwd_b_jobs(&self) -> usize {
        self.m / 2 + 1
    }

    fn step_a_cost(&self, cols: usize, inverse: bool) -> f64 {
        let ns = if inverse { NS_INV_A } else { NS_FWD_A };
        ns * (cols * self.m) as f64
    }

    fn step_b_cost(&self, rows: usize) -> f64 {
        NS_STEP_B * (rows * self.l) as f64 * (self.l as f64).log2()
    }

    fn fwd_b_cost(&self, k1: usize) -> f64 {
        let rows = if k1 == 0 || 2 * k1 == self.m { 1 } else { 2 };
        self.step_b_cost(rows) + NS_SPLIT * (rows * self.l) as f64
    }
}

/// Plans and tables of a time-sliced complex FFT of size `b = m * l`
/// (four-step), used to build the length-`2b` real transforms of the large
/// tail stages. All sizes are powers of two.
///
/// Forward, with `n = l*n1 + n2` and `k = k1 + m*k2`:
/// `Z[k1 + m*k2] = Σ_n2 e^{-2πi n2 k2 / l} · w^{n2 k1} · Σ_n1 z[l*n1 + n2] e^{-2πi n1 k1 / m}`
/// (`w = e^{-2πi / b}`). Step A does the inner size-`m` FFTs a few columns
/// `n2` at a time and stores the twiddled result transposed
/// (`work[k1*l + n2]`); step B does the size-`l` FFTs on contiguous rows,
/// leaving `Z[k1 + m*k2]` in `work[k1*l + k2]`, and splits the packed
/// complex transform into the real one row pair by row pair.
///
/// Spectra of four-step stages stay in that order: position `p = k1*l + k2`
/// holds bin `k = k1 + m*k2`, and position `b` the Nyquist bin. The
/// multiply-accumulate is elementwise, so the FDL and the filter spectra
/// only need to agree, and no transpose back to natural order is ever
/// needed.
///
/// The inverse runs the same decomposition backwards on that layout: size-`l`
/// FFTs over `k2` on contiguous rows (after packing the Hermitian spectrum),
/// twiddle, then size-`m` FFTs over `k1` per column, which produce
/// `z[l*n1 + n2]`.
#[derive(Clone)]
pub(crate) struct FourStepTables {
    d: Dims,
    fwd_m: Arc<dyn Fft<f64>>,
    fwd_l: Arc<dyn Fft<f64>>,
    inv_m: Arc<dyn Fft<f64>>,
    inv_l: Arc<dyn Fft<f64>>,
    /// Forward step-A twiddles `w^{n2 k1}` at `[n2 * m + k1]`.
    tw_cols: Vec<Complex64>,
    /// Inverse twiddles `w^{-n2 k1}` at `[k1 * l + n2]`.
    tw_rows: Vec<Complex64>,
    /// Forward split factor `-i/2 · e^{-iπ k / b}` at spectrum position `p`.
    split: Vec<Complex64>,
    /// Inverse pre-twiddle `i · e^{+iπ k / b}` at spectrum position `p`.
    pre: Vec<Complex64>,
}

#[derive(Clone)]
pub(crate) struct FourStep {
    /// Shared by the per-channel copies of a split convolver or upsampler.
    t: Arc<FourStepTables>,
    scratch: Vec<Complex64>,
    gather: Vec<Complex64>,
}

impl FourStep {
    pub fn new(b: usize) -> Self {
        let d = Dims::new(b);
        let (m, l) = (d.m, d.l);
        let mut planner = FftPlanner::<f64>::new();
        let fwd_m = planner.plan_fft_forward(m);
        let fwd_l = planner.plan_fft_forward(l);
        let inv_m = planner.plan_fft_inverse(m);
        let inv_l = planner.plan_fft_inverse(l);
        let scratch_len = [&fwd_m, &fwd_l, &inv_m, &inv_l]
            .iter()
            .map(|p| p.get_inplace_scratch_len())
            .max()
            .unwrap_or(0);
        let tau = 2.0 * std::f64::consts::PI;
        let cis = |num: usize, den: usize, sign: f64| {
            Complex64::from_polar(1.0, sign * tau * num as f64 / den as f64)
        };
        let tw_cols = (0..b).map(|i| cis((i / m) * (i % m), b, -1.0)).collect();
        let tw_rows = (0..b).map(|i| cis((i / l) * (i % l), b, 1.0)).collect();
        let bin = |p: usize| p / l + m * (p % l);
        let split = (0..b)
            .map(|p| Complex64::new(0.0, -0.5) * cis(bin(p), 2 * b, -1.0))
            .collect();
        let pre = (0..b)
            .map(|p| Complex64::i() * cis(bin(p), 2 * b, 1.0))
            .collect();
        Self {
            t: Arc::new(FourStepTables {
                d,
                fwd_m,
                fwd_l,
                inv_m,
                inv_l,
                tw_cols,
                tw_rows,
                split,
                pre,
            }),
            scratch: vec![Complex64::new(0.0, 0.0); scratch_len],
            gather: vec![Complex64::new(0.0, 0.0); d.cols_a * m],
        }
    }
}

impl FourStepTables {
    /// A natural-order spectrum (`b + 1` bins) in spectrum order.
    pub fn permute(&self, nat: &SplitSpectrum) -> SplitSpectrum {
        let Dims { b, m, l, .. } = self.d;
        let mut out = SplitSpectrum::new(b + 1);
        for p in 0..b {
            let k = p / l + m * (p % l);
            out.re[p] = nat.re[k];
            out.im[p] = nat.im[k];
        }
        out.re[b] = nat.re[b];
        out.im[b] = nat.im[b];
        out
    }

    /// Row `k1`'s mirror: `Z[b - k]` for `k = k1 + m*k2` sits at
    /// `base - k2` (row 0 mirrors onto itself shifted by one; its `k2 = 0`
    /// entry pairs with itself / the Nyquist bin and is handled separately).
    #[inline]
    fn mirror_base(&self, k1: usize) -> usize {
        if k1 == 0 {
            self.d.l
        } else {
            (self.d.m - k1) * self.d.l + self.d.l - 1
        }
    }

    /// Forward step A for columns `cols`: gather `z[l*n1 + n2]` from the
    /// `2b`-sample real block `[prev, cur]` (`z[n] = x[2n] + i x[2n+1]`),
    /// size-`m` FFTs, twiddle, and store transposed in `work`.
    fn fwd_a(
        &self,
        g: &mut [Complex64],
        scratch: &mut [Complex64],
        work: &mut [Complex64],
        cols: std::ops::Range<usize>,
        prev: &[f64],
        cur: &[f64],
    ) {
        let Dims { b, m, l, .. } = self.d;
        let r = cols.len();
        let g = &mut g[..r * m];
        // Row-major: every input row contributes `r` adjacent sample pairs,
        // so each cache line is read once.
        for n1 in 0..m {
            let base = 2 * (l * n1 + cols.start);
            let src = if base < b {
                &prev[base..base + 2 * r]
            } else {
                &cur[base - b..base - b + 2 * r]
            };
            for (j, pair) in src.as_chunks::<2>().0.iter().enumerate() {
                g[j * m + n1] = Complex64::new(pair[0], pair[1]);
            }
        }
        self.fwd_m.process_with_scratch(g, scratch);
        for (col, n2) in g.chunks_exact_mut(m).zip(cols.clone()) {
            for (v, w) in col.iter_mut().zip(&self.tw_cols[n2 * m..(n2 + 1) * m]) {
                *v *= w;
            }
        }
        for k1 in 0..m {
            let dst = &mut work[k1 * l + cols.start..k1 * l + cols.end];
            for (j, d) in dst.iter_mut().enumerate() {
                *d = g[j * m + k1];
            }
        }
    }

    /// Forward step B for the row pair `{k1, m - k1}`, then the real split
    /// of both rows into `dst`:
    /// `X[k] = (Z[k] + Z*[b-k]) / 2 + (-i/2) W^k (Z[k] - Z*[b-k])`.
    fn fwd_b_split(
        &self,
        scratch: &mut [Complex64],
        work: &mut [Complex64],
        dst: &mut SplitSpectrum,
        k1: usize,
    ) {
        let Dims { b, m, l, .. } = self.d;
        let pair = (m - k1) % m;
        self.fwd_l
            .process_with_scratch(&mut work[k1 * l..(k1 + 1) * l], scratch);
        if pair != k1 {
            self.fwd_l
                .process_with_scratch(&mut work[pair * l..(pair + 1) * l], scratch);
        }
        for row in [k1, pair] {
            let start = row * l;
            let first = if row == 0 {
                // X[0] and the Nyquist bin (W^b = -1) come from Z[0] alone.
                let z0 = work[0];
                dst.re[0] = z0.re + z0.im;
                dst.im[0] = 0.0;
                dst.re[b] = z0.re - z0.im;
                dst.im[b] = 0.0;
                1
            } else {
                0
            };
            let base = self.mirror_base(row);
            for k2 in first..l {
                let zk = work[start + k2];
                let zc = work[base - k2].conj();
                let x = (zk + zc) * 0.5 + self.split[start + k2] * (zk - zc);
                dst.re[start + k2] = x.re;
                dst.im[start + k2] = x.im;
            }
            if pair == k1 {
                break;
            }
        }
    }

    /// Inverse step B for rows `rows`: pack the Hermitian spectrum `y`
    /// (spectrum order) into the length-`b` complex spectrum whose inverse is
    /// `y[2m] + i y[2m+1]`:
    /// `C[k] = (Y[k] + Y*[b-k]) + i e^{+iπk/b} (Y[k] - Y*[b-k])`,
    /// then size-`l` inverse FFTs on the rows and the inverse twiddle. DC and
    /// Nyquist imaginary parts are treated as zero, as `realfft` does.
    fn inv_b(
        &self,
        scratch: &mut [Complex64],
        work: &mut [Complex64],
        y: &SplitSpectrum,
        rows: std::ops::Range<usize>,
    ) {
        let Dims { b, l, .. } = self.d;
        for k1 in rows.clone() {
            let start = k1 * l;
            let first = if k1 == 0 {
                let y0 = Complex64::new(y.re[0], 0.0);
                let yb = Complex64::new(y.re[b], 0.0);
                work[0] = (y0 + yb) + self.pre[0] * (y0 - yb);
                1
            } else {
                0
            };
            let base = self.mirror_base(k1);
            for k2 in first..l {
                let p = start + k2;
                let q = base - k2;
                let yk = Complex64::new(y.re[p], y.im[p]);
                let yc = Complex64::new(y.re[q], -y.im[q]);
                work[p] = (yk + yc) + self.pre[p] * (yk - yc);
            }
        }
        let span = rows.start * l..rows.end * l;
        self.inv_l
            .process_with_scratch(&mut work[span.clone()], scratch);
        for (v, w) in work[span.clone()].iter_mut().zip(&self.tw_rows[span]) {
            *v *= w;
        }
    }

    /// Inverse step A for columns `cols`: size-`m` inverse FFTs over `k1`,
    /// giving `z[l*n1 + n2]`; the second half of the time block
    /// (`n1 >= m/2`) is unpacked into `out` (`y[2n], y[2n+1]` at `2n - b`).
    fn inv_a(
        &self,
        g: &mut [Complex64],
        scratch: &mut [Complex64],
        work: &[Complex64],
        cols: std::ops::Range<usize>,
        out: &mut [f64],
    ) {
        let Dims { b, m, l, .. } = self.d;
        let r = cols.len();
        let g = &mut g[..r * m];
        for k1 in 0..m {
            let src = &work[k1 * l + cols.start..k1 * l + cols.end];
            for (j, &v) in src.iter().enumerate() {
                g[j * m + k1] = v;
            }
        }
        self.inv_m.process_with_scratch(g, scratch);
        for n1 in m / 2..m {
            let base = 2 * (l * n1 + cols.start) - b;
            let dst = &mut out[base..base + 2 * r];
            for (j, pair) in dst.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let z = g[j * m + n1];
                pair[0] = z.re;
                pair[1] = z.im;
            }
        }
    }
}

// ---------------------------------------------------------------- stage 0

/// Stage 0: UPOLS on the base block, computed immediately.
#[derive(Clone)]
pub(crate) struct HeadStage {
    block: usize,
    branches: usize,
    /// Indexed `[ir][partition][branch]`; shared by the per-channel copies
    /// of a split convolver or upsampler.
    ir_spectra: Arc<Vec<Vec<Vec<SplitSpectrum>>>>,
    fft: RealFftPair,
    /// One accumulator per branch.
    acc: Vec<SplitSpectrum>,
    time_out: Vec<f64>,
}

impl HeadStage {
    /// `ir_spectra[ir][partition][branch]`: partitions of `block` frames,
    /// transformed with a `2 * block`-point real FFT and scaled by
    /// `1 / (2 * block)`.
    pub fn from_spectra(
        block: usize,
        branches: usize,
        ir_spectra: Vec<Vec<Vec<SplitSpectrum>>>,
    ) -> Self {
        let fft = RealFftPair::new(2 * block);
        debug_assert!(
            ir_spectra
                .iter()
                .flatten()
                .all(|p| p.len() == branches && p.iter().all(|s| s.len() == fft.spec_len()))
        );
        Self {
            block,
            branches,
            ir_spectra: Arc::new(ir_spectra),
            acc: (0..branches)
                .map(|_| SplitSpectrum::new(fft.spec_len()))
                .collect(),
            time_out: vec![0.0; 2 * block],
            fft,
        }
    }

    pub fn spec_len(&self) -> usize {
        self.fft.spec_len()
    }

    pub fn parts(&self, ir_idx: usize) -> usize {
        self.ir_spectra[ir_idx].len()
    }

    pub fn new_channel(&self, ir_idx: usize) -> HeadChannel {
        HeadChannel::new(self.block, self.spec_len(), self.parts(ir_idx))
    }

    /// Every filter spectrum times `gain`. Copies them when they are
    /// shared already.
    pub fn scale(&mut self, gain: f64) {
        for spec in Arc::make_mut(&mut self.ir_spectra)
            .iter_mut()
            .flatten()
            .flatten()
        {
            scale_spectrum(spec, gain);
        }
    }

    /// Process one base block; the result overwrites
    /// `out[..block * branches]` (interleaved by branch).
    pub fn process_block(
        &mut self,
        ir_idx: usize,
        ch: &mut HeadChannel,
        input: &[f64],
        out: &mut [f64],
    ) {
        let parts = &self.ir_spectra[ir_idx];
        let num_parts = parts.len();
        if num_parts == 0 {
            out.fill(0.0);
            return;
        }

        ch.fdl_head = (ch.fdl_head + 1) % num_parts;
        self.fft
            .forward_pair_into_split(&ch.prev_in, input, &mut ch.fdl[ch.fdl_head]);

        let x0 = &ch.fdl[ch.fdl_head];
        for (acc, ir) in self.acc.iter_mut().zip(&parts[0]) {
            complex_mul_split(acc, x0, ir);
        }
        for (p, part) in parts.iter().enumerate().skip(1) {
            let xp = &ch.fdl[(ch.fdl_head + num_parts - p) % num_parts];
            for (acc, ir) in self.acc.iter_mut().zip(part) {
                complex_mac_split(acc, xp, ir);
            }
        }

        let b = self.block;
        let r = self.branches;
        for (branch, acc) in self.acc.iter().enumerate() {
            self.fft.inverse_from_split(acc, &mut self.time_out);
            let tail = &self.time_out[b..];
            if r == 1 {
                out.copy_from_slice(tail);
            } else {
                for (i, &y) in tail.iter().enumerate() {
                    out[i * r + branch] = y;
                }
            }
        }
        ch.prev_in.copy_from_slice(input);
    }

    /// Rough cost of one `process_block` in the model's units.
    pub fn block_cost(&self, ir_idx: usize) -> f64 {
        head_block_cost(self.block, self.branches, self.parts(ir_idx))
    }

    /// Bytes of the shared filter spectra.
    pub fn shared_bytes(&self) -> usize {
        self.ir_spectra
            .iter()
            .flatten()
            .flatten()
            .map(spectrum_bytes)
            .sum()
    }

    /// Bytes of each copy's own buffers (accumulators, time block; the FFT
    /// plan's scratch not counted).
    pub fn own_bytes(&self) -> usize {
        self.acc.iter().map(spectrum_bytes).sum::<usize>() + 8 * self.time_out.len()
    }
}

fn spectrum_bytes(s: &SplitSpectrum) -> usize {
    16 * s.len()
}

pub(crate) struct HeadChannel {
    prev_in: Vec<f64>,
    fdl: Vec<SplitSpectrum>,
    fdl_head: usize,
}

impl HeadChannel {
    fn new(block_size: usize, spec_len: usize, num_parts: usize) -> Self {
        let mut ch = Self {
            prev_in: vec![0.0; block_size],
            fdl: (0..num_parts)
                .map(|_| SplitSpectrum::new(spec_len))
                .collect(),
            fdl_head: 0,
        };
        // Writes every page now, off the real-time thread.
        ch.reset();
        ch
    }

    /// Bytes of the channel's delay line and input block.
    pub fn bytes(&self) -> usize {
        8 * self.prev_in.len() + self.fdl.iter().map(spectrum_bytes).sum::<usize>()
    }

    pub fn reset(&mut self) {
        self.prev_in.fill(0.0);
        for spec in &mut self.fdl {
            spec.clear();
        }
        self.fdl_head = 0;
    }

    /// Whether [`Self::copy_from`] can take `other`: the same block and
    /// delay line.
    pub fn same_shape(&self, other: &Self) -> bool {
        self.prev_in.len() == other.prev_in.len()
            && self.fdl.len() == other.fdl.len()
            && self.fdl.first().map(SplitSpectrum::len) == other.fdl.first().map(SplitSpectrum::len)
    }

    /// Take `other`'s state ([`Self::same_shape`]): a channel of another
    /// convolver of the same filter. Never allocates.
    pub fn copy_from(&mut self, other: &Self) {
        self.prev_in.copy_from_slice(&other.prev_in);
        for (d, s) in self.fdl.iter_mut().zip(&other.fdl) {
            d.copy_from(s);
        }
        self.fdl_head = other.fdl_head;
    }
}

fn scale_spectrum(spec: &mut SplitSpectrum, gain: f64) {
    for (re, im) in spec.re.iter_mut().zip(spec.im.iter_mut()) {
        *re *= gain;
        *im *= gain;
    }
}

// ---------------------------------------------------------------- tail stages

/// One slice of a tail stage's per-period work. The index says which
/// chunk (columns, row pair, rows or bins) the job covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    /// Whole forward real FFT of `[prev, cur]` (small stages).
    FwdDirect,
    /// Four-step forward: step A columns, then step B + real split per row
    /// pair.
    FwdA(usize),
    FwdB(usize),
    /// Multiply-accumulate of one branch's partitions over a chunk of bins.
    Mac {
        branch: usize,
        start: usize,
    },
    /// Four-step inverse of one branch: pack + step B rows, then step A
    /// columns + unpack.
    InvB {
        branch: usize,
        k1: usize,
    },
    InvA {
        branch: usize,
        n2: usize,
    },
    /// Whole inverse real FFT of one branch (small stages).
    InvDirect {
        branch: usize,
    },
    Done,
}

/// What a tail stage does in one period and how it is spread over the
/// period's base blocks, without its buffers: the runtime runs it
/// ([`TailStage`]) and the budget prices it
/// ([`crate::design::budget`]) through the same functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Schedule {
    pub block: usize,
    pub branches: usize,
    /// Base blocks per period (`block / base`).
    pub ticks: usize,
    /// The four-step sizes; `None` for a direct stage.
    pub four: Option<Dims>,
    /// Bins per multiply-accumulate job.
    pub mac_chunk: usize,
}

/// Where a tail stage is in its period: the base block, the next job and
/// the work done so far in the model's units.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Slicer {
    tick: usize,
    job: Job,
    done: f64,
    /// The period's whole work.
    total: f64,
}

impl Schedule {
    pub fn new(block: usize, branches: usize, base: usize) -> Self {
        Self {
            block,
            branches,
            ticks: block / base,
            four: (block > DIRECT_FFT_MAX_BLOCK).then(|| Dims::new(block)),
            mac_chunk: if branches == 1 {
                MAC_CHUNK
            } else {
                POLY_MAC_CHUNK
            },
        }
    }

    pub fn first_job(&self) -> Job {
        match self.four {
            None => Job::FwdDirect,
            Some(_) => Job::FwdA(0),
        }
    }

    fn after_branch(&self, branch: usize) -> Job {
        if branch + 1 < self.branches {
            Job::Mac {
                branch: branch + 1,
                start: 0,
            }
        } else {
            Job::Done
        }
    }

    pub fn next_job(&self, job: Job) -> Job {
        let bins = self.block + 1;
        match (self.four, job) {
            (_, Job::Done) => Job::Done,
            (_, Job::FwdDirect) => Job::Mac {
                branch: 0,
                start: 0,
            },
            (_, Job::Mac { branch, start }) if start + self.mac_chunk < bins => Job::Mac {
                branch,
                start: start + self.mac_chunk,
            },
            (None, Job::Mac { branch, .. }) => Job::InvDirect { branch },
            (Some(_), Job::Mac { branch, .. }) => Job::InvB { branch, k1: 0 },
            (_, Job::InvDirect { branch }) => self.after_branch(branch),
            (Some(d), Job::FwdA(n2)) if n2 + d.cols_a < d.l => Job::FwdA(n2 + d.cols_a),
            (Some(_), Job::FwdA(_)) => Job::FwdB(0),
            (Some(d), Job::FwdB(k1)) if k1 + 1 < d.fwd_b_jobs() => Job::FwdB(k1 + 1),
            (Some(_), Job::FwdB(_)) => Job::Mac {
                branch: 0,
                start: 0,
            },
            (Some(d), Job::InvB { branch, k1 }) if k1 + d.rows_b < d.m => Job::InvB {
                branch,
                k1: k1 + d.rows_b,
            },
            (Some(_), Job::InvB { branch, .. }) => Job::InvA { branch, n2: 0 },
            (Some(d), Job::InvA { branch, n2 }) if n2 + d.cols_a < d.l => Job::InvA {
                branch,
                n2: n2 + d.cols_a,
            },
            (Some(_), Job::InvA { branch, .. }) => self.after_branch(branch),
            (None, j) => unreachable!("job {j:?} on a direct stage"),
        }
    }

    /// The model cost of `job` with `num_parts` partitions per branch.
    pub fn job_cost(&self, job: Job, num_parts: usize) -> f64 {
        let bins = self.block + 1;
        match (self.four, job) {
            (_, Job::Done) => 0.0,
            (_, Job::FwdDirect | Job::InvDirect { .. }) => real_fft_cost(self.block),
            (_, Job::Mac { start, .. }) => {
                NS_MAC * ((start + self.mac_chunk).min(bins) - start) as f64 * num_parts as f64
            }
            (Some(d), Job::FwdA(n2)) => d.step_a_cost((n2 + d.cols_a).min(d.l) - n2, false),
            (Some(d), Job::InvA { n2, .. }) => d.step_a_cost((n2 + d.cols_a).min(d.l) - n2, true),
            (Some(d), Job::FwdB(k1)) => d.fwd_b_cost(k1),
            (Some(d), Job::InvB { k1, .. }) => {
                let rows = (k1 + d.rows_b).min(d.m) - k1;
                d.step_b_cost(rows) + NS_PACK * (rows * d.l) as f64
            }
            (None, j) => unreachable!("job {j:?} on a direct stage"),
        }
    }

    /// Every job of a period, in order.
    pub fn jobs(&self) -> impl Iterator<Item = Job> + '_ {
        std::iter::successors(Some(self.first_job()), |&j| Some(self.next_job(j)))
            .take_while(|&j| j != Job::Done)
    }

    /// The model cost of one period with `num_parts` partitions per branch.
    pub fn period_cost(&self, num_parts: usize) -> f64 {
        self.jobs().map(|j| self.job_cost(j, num_parts)).sum()
    }

    pub fn slicer(&self, num_parts: usize) -> Slicer {
        Slicer {
            tick: 0,
            job: Job::Done,
            done: 0.0,
            total: self.period_cost(num_parts),
        }
    }

    /// Start a base block: whether it starts a period (and the slicer
    /// with it).
    fn begin_tick(&self, s: &mut Slicer) -> bool {
        let start = s.tick == 0;
        if start {
            debug_assert_eq!(s.job, Job::Done, "previous period's work unfinished");
            s.job = self.first_job();
            s.done = 0.0;
        }
        start
    }

    /// Run this base block's share of the period's work through `run`
    /// (each job in order), then move to the next base block. Returns the
    /// model cost of the work done.
    fn run_share(&self, s: &mut Slicer, num_parts: usize, mut run: impl FnMut(Job)) -> f64 {
        s.tick += 1;
        let last = s.tick == self.ticks;
        let target = s.total * s.tick as f64 / self.ticks as f64;
        let mut spent = 0.0;
        // Run jobs until this block's share is reached, rounding to the
        // nearest job so the overshoot does not accumulate in one direction.
        while s.job != Job::Done {
            let cost = self.job_cost(s.job, num_parts);
            if !last && s.done + 0.5 * cost > target {
                break;
            }
            run(s.job);
            s.done += cost;
            spent += cost;
            s.job = self.next_job(s.job);
        }
        if last {
            s.tick = 0;
        }
        spent
    }

    /// The jobs of one period, base block by base block, as the runtime
    /// slices them: `run(tick, job)` for each.
    pub fn for_each_sliced(&self, num_parts: usize, mut run: impl FnMut(usize, Job)) {
        let mut s = self.slicer(num_parts);
        for tick in 0..self.ticks {
            self.begin_tick(&mut s);
            self.run_share(&mut s, num_parts, |job| run(tick, job));
        }
    }
}

enum TailFft {
    Direct {
        fft: RealFftPair,
        time_out: Vec<f64>,
    },
    FourStep(Box<FourStep>),
}

impl Clone for TailFft {
    fn clone(&self) -> Self {
        match self {
            Self::Direct { fft, time_out } => Self::Direct {
                fft: fft.clone(),
                time_out: time_out.clone(),
            },
            Self::FourStep(fs) => Self::FourStep(fs.clone()),
        }
    }
}

/// A tail stage: UPOLS on `block`-frame partitions starting at a filter
/// offset `>= 2 * block`, computed in slices across one period.
#[derive(Clone)]
pub(crate) struct TailStage {
    sched: Schedule,
    /// Extra whole-partition delay: `(offset - 2 * block) / block`.
    delay_parts: usize,
    /// Indexed `[ir][branch][partition]` (every branch of one ir has the
    /// same number of partitions); shared like the head's.
    ir_spectra: Arc<Vec<Vec<Vec<SplitSpectrum>>>>,
    engine: TailFft,
}

pub(crate) struct TailChannel {
    num_parts: usize,
    /// `num_parts + delay_parts` slots.
    fdl: Vec<SplitSpectrum>,
    fdl_head: usize,
    /// Input blocks: `input[slot]` is being filled; the previous two are
    /// being transformed.
    input: [Vec<f64>; 3],
    slot: usize,
    /// Output for the current period (block `k - 2`), branch-major:
    /// branch `r` at `[r * block, (r + 1) * block)`.
    out_cur: Vec<f64>,
    /// Output being computed this period (block `k - 1`), same layout.
    out_next: Vec<f64>,
    acc: SplitSpectrum,
    /// Four-step work buffer (empty for direct stages).
    work: Vec<Complex64>,
    slicer: Slicer,
}

impl TailChannel {
    /// Bytes of the channel's delay line, input blocks, output double
    /// buffer, accumulator and four-step work buffer.
    pub fn bytes(&self) -> usize {
        self.fdl.iter().map(spectrum_bytes).sum::<usize>()
            + 8 * self.input.iter().map(Vec::len).sum::<usize>()
            + 8 * (self.out_cur.len() + self.out_next.len())
            + spectrum_bytes(&self.acc)
            + 16 * self.work.len()
    }

    pub fn reset(&mut self) {
        for spec in &mut self.fdl {
            spec.clear();
        }
        self.fdl_head = 0;
        for buf in &mut self.input {
            buf.fill(0.0);
        }
        self.slot = 0;
        self.out_cur.fill(0.0);
        self.out_next.fill(0.0);
        self.acc.clear();
        self.work.fill(Complex64::new(0.0, 0.0));
        self.slicer.tick = 0;
        self.slicer.job = Job::Done;
        self.slicer.done = 0.0;
    }

    /// Whether [`Self::copy_from`] can take `other`: the same partitions,
    /// delay line and buffers.
    pub fn same_shape(&self, other: &Self) -> bool {
        self.num_parts == other.num_parts
            && self.fdl.len() == other.fdl.len()
            && self.acc.len() == other.acc.len()
            && self.input[0].len() == other.input[0].len()
            && self.out_cur.len() == other.out_cur.len()
            && self.work.len() == other.work.len()
    }

    /// Take `other`'s state ([`Self::same_shape`]), its place in the
    /// period's sliced work included. Never allocates.
    pub fn copy_from(&mut self, other: &Self) {
        for (d, s) in self.fdl.iter_mut().zip(&other.fdl) {
            d.copy_from(s);
        }
        self.fdl_head = other.fdl_head;
        for (d, s) in self.input.iter_mut().zip(&other.input) {
            d.copy_from_slice(s);
        }
        self.slot = other.slot;
        self.out_cur.copy_from_slice(&other.out_cur);
        self.out_next.copy_from_slice(&other.out_next);
        self.acc.copy_from(&other.acc);
        self.work.copy_from_slice(&other.work);
        self.slicer = other.slicer;
    }
}

impl TailStage {
    /// `ir_spectra[ir][branch][partition]` in natural order, partitions of
    /// `plan.block` frames from `plan.offset` on, transformed with a
    /// `2 * block`-point real FFT and scaled by `1 / (2 * block)`.
    pub fn from_spectra(
        mut ir_spectra: Vec<Vec<Vec<SplitSpectrum>>>,
        plan: StagePlan,
        base: usize,
        branches: usize,
    ) -> Self {
        let block = plan.block;
        let sched = Schedule::new(block, branches, base);
        debug_assert!(
            ir_spectra
                .iter()
                .all(|ir| ir.len() == branches && ir.iter().all(|b| b.len() == ir[0].len()))
        );
        let engine = if sched.four.is_none() {
            TailFft::Direct {
                fft: RealFftPair::new(2 * block),
                time_out: vec![0.0; 2 * block],
            }
        } else {
            let fs = FourStep::new(block);
            for spec in ir_spectra.iter_mut().flatten().flatten() {
                *spec = fs.t.permute(spec);
            }
            TailFft::FourStep(Box::new(fs))
        };
        Self {
            sched,
            delay_parts: (plan.offset - 2 * block) / block,
            ir_spectra: Arc::new(ir_spectra),
            engine,
        }
    }

    pub fn schedule(&self) -> &Schedule {
        &self.sched
    }

    /// Bytes of the shared filter spectra (and four-step tables).
    pub fn shared_bytes(&self) -> usize {
        let tables = match &self.engine {
            TailFft::Direct { .. } => 0,
            TailFft::FourStep(fs) => 4 * 16 * fs.t.d.b,
        };
        tables
            + self
                .ir_spectra
                .iter()
                .flatten()
                .flatten()
                .map(spectrum_bytes)
                .sum::<usize>()
    }

    pub fn parts(&self, ir_idx: usize) -> usize {
        self.ir_spectra[ir_idx].first().map_or(0, Vec::len)
    }

    /// Every filter spectrum times `gain` (see [`HeadStage::scale`]).
    pub fn scale(&mut self, gain: f64) {
        for spec in Arc::make_mut(&mut self.ir_spectra)
            .iter_mut()
            .flatten()
            .flatten()
        {
            scale_spectrum(spec, gain);
        }
    }

    /// A channel's state, or `None` when its filter ends before this stage.
    /// Its buffers are written once here, off the real-time thread.
    pub fn new_channel(&self, ir_idx: usize) -> Option<TailChannel> {
        let num_parts = self.parts(ir_idx);
        if num_parts == 0 {
            return None;
        }
        let block = self.sched.block;
        let spec_len = block + 1;
        let work_len = match self.engine {
            TailFft::Direct { .. } => 0,
            TailFft::FourStep(_) => block,
        };
        let out_len = block * self.sched.branches;
        let mut ch = TailChannel {
            num_parts,
            fdl: (0..num_parts + self.delay_parts)
                .map(|_| SplitSpectrum::new(spec_len))
                .collect(),
            fdl_head: 0,
            input: std::array::from_fn(|_| vec![0.0; block]),
            slot: 0,
            out_cur: vec![0.0; out_len],
            out_next: vec![0.0; out_len],
            acc: SplitSpectrum::new(spec_len),
            work: vec![Complex64::new(0.0, 0.0); work_len],
            slicer: self.sched.slicer(num_parts),
        };
        ch.reset();
        Some(ch)
    }

    /// Run `job` of the current period.
    fn run_job(&mut self, ir_idx: usize, ch: &mut TailChannel, job: Job) {
        let b = self.sched.block;
        let bins = b + 1;
        let mac_chunk = self.sched.mac_chunk;
        let TailStage {
            ir_spectra,
            engine,
            delay_parts,
            ..
        } = self;
        let prev = &ch.input[(ch.slot + 1) % 3];
        let cur = &ch.input[(ch.slot + 2) % 3];
        match (engine, job) {
            (_, Job::Done) => {}
            (TailFft::Direct { fft, .. }, Job::FwdDirect) => {
                fft.forward_pair_into_split(prev, cur, &mut ch.fdl[ch.fdl_head]);
            }
            (_, Job::Mac { branch, start }) => {
                let range = start..(start + mac_chunk).min(bins);
                let slots = ch.fdl.len();
                for (p, part) in ir_spectra[ir_idx][branch].iter().enumerate() {
                    let idx = (ch.fdl_head + slots - p - *delay_parts) % slots;
                    if p == 0 {
                        complex_mul_split_range(&mut ch.acc, &ch.fdl[idx], part, range.clone());
                    } else {
                        complex_mac_split_range(&mut ch.acc, &ch.fdl[idx], part, range.clone());
                    }
                }
            }
            (TailFft::Direct { fft, time_out }, Job::InvDirect { branch }) => {
                fft.inverse_from_split(&ch.acc, time_out);
                ch.out_next[branch * b..(branch + 1) * b].copy_from_slice(&time_out[b..]);
            }
            (TailFft::FourStep(fs), Job::FwdA(n2)) => {
                let FourStep { t, scratch, gather } = &mut **fs;
                let cols = n2..(n2 + t.d.cols_a).min(t.d.l);
                t.fwd_a(gather, scratch, &mut ch.work, cols, prev, cur);
            }
            (TailFft::FourStep(fs), Job::FwdB(k1)) => {
                let FourStep { t, scratch, .. } = &mut **fs;
                t.fwd_b_split(scratch, &mut ch.work, &mut ch.fdl[ch.fdl_head], k1);
            }
            (TailFft::FourStep(fs), Job::InvB { k1, .. }) => {
                let FourStep { t, scratch, .. } = &mut **fs;
                let rows = k1..(k1 + t.d.rows_b).min(t.d.m);
                t.inv_b(scratch, &mut ch.work, &ch.acc, rows);
            }
            (TailFft::FourStep(fs), Job::InvA { branch, n2 }) => {
                let FourStep { t, scratch, gather } = &mut **fs;
                let cols = n2..(n2 + t.d.cols_a).min(t.d.l);
                let out = &mut ch.out_next[branch * b..(branch + 1) * b];
                t.inv_a(gather, scratch, &ch.work, cols, out);
            }
            (_, j) => unreachable!("job {j:?} does not match the stage engine"),
        }
    }

    /// One base block for this stage: add this period's precomputed output
    /// into `out` (`input.len() * branches` frames, interleaved by branch),
    /// store the input, and run this block's share of the work. Returns the
    /// model cost of the work done.
    pub fn tick(
        &mut self,
        ir_idx: usize,
        ch: &mut TailChannel,
        input: &[f64],
        out: &mut [f64],
    ) -> f64 {
        let b0 = input.len();
        let b = self.sched.block;
        let r = self.sched.branches;
        if self.sched.begin_tick(&mut ch.slicer) {
            std::mem::swap(&mut ch.out_cur, &mut ch.out_next);
            ch.slot = (ch.slot + 1) % 3;
            ch.fdl_head = (ch.fdl_head + 1) % ch.fdl.len();
        }
        let pos = ch.slicer.tick * b0;
        if r == 1 {
            for (o, &y) in out.iter_mut().zip(&ch.out_cur[pos..pos + b0]) {
                *o += y;
            }
        } else {
            for branch in 0..r {
                let src = &ch.out_cur[branch * b + pos..branch * b + pos + b0];
                for (i, &y) in src.iter().enumerate() {
                    out[i * r + branch] += y;
                }
            }
        }
        ch.input[ch.slot][pos..pos + b0].copy_from_slice(input);

        let sched = self.sched;
        let num_parts = ch.num_parts;
        let mut slicer = ch.slicer;
        let spent = sched.run_share(&mut slicer, num_parts, |job| self.run_job(ir_idx, ch, job));
        ch.slicer = slicer;
        spent
    }
}

// ------------------------------------------------- the polyphase planner

/// The largest block of an upsampler's stages at `ratio`: 32768, lower at
/// ratios above 16 so that the largest stage's output double buffer stays
/// within [`POLY_MAX_OUT`] frames; never below `base`.
pub(crate) fn polyphase_max_part(base: usize, ratio: usize) -> usize {
    let cap = POLY_MAX_OUT / ratio.max(1);
    let cap = if cap.is_power_of_two() {
        cap
    } else {
        cap.next_power_of_two() / 2
    };
    cap.min(MAX_PART).max(base)
}

/// Partitions of a stage over branches of at most `len` frames.
pub(crate) fn stage_parts(st: &StagePlan, len: usize) -> usize {
    if len <= st.offset {
        return 0;
    }
    (len.min(st.end) - st.offset).div_ceil(st.block)
}

/// The model cost of `plan` per base block, for branches of at most `len`
/// frames.
fn plan_cost(plan: &[StagePlan], base: usize, branches: usize, len: usize) -> f64 {
    let head = head_block_cost(base, branches, stage_parts(&plan[0], len).max(1));
    plan[1..].iter().fold(head, |sum, st| {
        let parts = stage_parts(st, len);
        if parts == 0 {
            return sum;
        }
        let sched = Schedule::new(st.block, branches, base);
        sum + sched.period_cost(parts) / sched.ticks as f64
    })
}

/// The partition schedule of an FFT polyphase upsampler with base block
/// `base`, `branches` polyphase branches of at most `len` taps each.
///
/// The default schedule ([`plan_schedule_capped`]) with tail stages merged
/// into the stage before them while that lowers the model cost of a base
/// block: a tail stage costs `1 + branches` transforms per sample whatever
/// its length, so one with few partitions (the first, for a filter not
/// much longer than the head; the last, just past a stage boundary) is
/// cheaper as more partitions of the stage before it. A short filter keeps
/// one stage: the uniformly partitioned engine, unchanged.
pub(crate) fn polyphase_plan(base: usize, branches: usize, len: usize) -> Vec<StagePlan> {
    let mut plan = plan_schedule_capped(base, len, polyphase_max_part(base, branches));
    // Stages past the filter's end (none from the default schedule).
    plan.retain(|st| st.offset == 0 || stage_parts(st, len) > 0);
    if let Some(last) = plan.last_mut() {
        last.end = usize::MAX;
    }
    loop {
        let cost = plan_cost(&plan, base, branches, len);
        let best = (1..plan.len())
            .map(|s| {
                let mut merged = plan.clone();
                merged[s - 1].end = merged[s].end;
                merged.remove(s);
                (plan_cost(&merged, base, branches, len), merged)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0));
        match best {
            Some((c, merged)) if c < cost => plan = merged,
            _ => return plan,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_job_runs_once_per_period_in_order() {
        for (block, branches, base) in [(2048, 16, 512), (512, 4, 64), (32_768, 3, 1024)] {
            let s = Schedule::new(block, branches, base);
            let mut sliced = Vec::new();
            let mut per_tick = vec![0usize; s.ticks];
            s.for_each_sliced(6, |tick, job| {
                per_tick[tick] += 1;
                sliced.push(job);
            });
            assert_eq!(sliced, s.jobs().collect::<Vec<_>>());
            // Every base block does some of the work.
            assert!(per_tick.iter().all(|&n| n > 0), "{per_tick:?}");
        }
    }

    #[test]
    fn polyphase_plans_are_valid_and_long_ones_partition() {
        for base in [64, 256, 512, 1024] {
            for branches in [2, 3, 8, 16, 48, 64] {
                for len in [1, 8 * base + 1, 100_000, 262_144, 524_288, 1 << 21] {
                    let plan = polyphase_plan(base, branches, len);
                    validate_plan(&plan, base);
                    let cap = polyphase_max_part(base, branches);
                    assert!(plan.iter().all(|st| st.block <= cap), "{plan:?}");
                    if len >= 262_144 {
                        assert!(plan.len() > 1, "base {base} x{branches} len {len}");
                    }
                }
            }
        }
    }
}
