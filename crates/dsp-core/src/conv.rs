//! Partitioned FFT convolution (non-uniform partitions for long IRs).
//!
//! # Partition schedule
//!
//! The IR is split into stages. Stage 0 is uniformly partitioned overlap-save
//! (UPOLS) with the base block `B0` and runs immediately on every base block,
//! so the convolver's latency is exactly `B0` frames (the FIFO pre-fill).
//!
//! Every later ("tail") stage `s` is UPOLS with a larger block `Bs` that covers
//! IR samples `[Os, Os+1)`. Input block `k` of that stage (`Bs` frames) is
//! complete at time `(k+1)·Bs`, and its sub-convolution belongs at output time
//! `k·Bs + Os`. A tail stage computes block `k` during the whole next period
//! `[(k+1)·Bs, (k+2)·Bs)` and plays it out during `[(k+2)·Bs, (k+3)·Bs)`, so its
//! inherent delay is `2·Bs`. The schedule therefore requires `Os >= 2·Bs` and
//! makes up the difference `Os - 2·Bs` (a multiple of `Bs`) exactly by reading
//! older frequency-domain delay line slots. The default schedule grows blocks
//! 4x per stage and starts every tail stage at exactly `Os = 2·Bs`:
//!
//! | stage | block  | IR range         | partitions |
//! |-------|--------|------------------|------------|
//! | 0     | `B0`   | `[0, 8·B0)`      | 8          |
//! | 1     | `4·B0` | `[8·B0, 32·B0)`  | 6          |
//! | s     | `B`    | `[2B, 8B)`       | 6          |
//!
//! Blocks are capped at 32768: the stage before the cap ends at IR offset
//! 65536 (with 2 or 6 partitions depending on `B0`), and the last stage
//! (`B = 32768`, `Os = 65536`) covers the rest of the IR.
//!
//! (An earlier schedule started the capped stage at 65536 with no extra
//! delay, so every IR tap at or beyond 65536 came out 32768 frames early.)
//!
//! # Real-time cost
//!
//! Because each tail stage has a whole period of slack, its work (forward
//! FFT, frequency-domain multiply-accumulate, inverse FFT) is time-sliced
//! over the `Bs / B0` base blocks of that period, weighted by a measured
//! per-job cost model, instead of running all at once on the block that
//! completes it. Transforms with `Bs > 512` use a four-step decomposition
//! (`Bs = M·L`), so the biggest indivisible job is a few sub-FFTs of size
//! ~`sqrt(Bs)` (a few µs); smaller stages use one real FFT per job.
//!
//! The worst single `process` call is therefore close to the mean. For a
//! 1 s stereo IR at 192 kHz with 256-frame blocks (1333 µs period), measured
//! on one Zen 2 core (`bench_worst_call_1s_ir_192k_block256`, fastrel):
//! mean ≈ 67 µs, p99 ≈ 110 µs, worst ≈ 200–330 µs (OS noise; the estimated
//! work per call varies by < 15 %, see
//! `tail_work_is_spread_evenly_across_process_calls`). The previous schedule
//! ran each tail stage's whole FFT on one block: mean 43 µs but p99 743 µs
//! and worst 2.3 ms, over the period. The price is ~1.3x the FFT cost of a
//! direct `realfft` for the four-step stages and 8 head partitions instead of
//! 4 (a 65536-tap stereo IR at 192 kHz runs at ~43x real time instead of ~60x).

use crate::Complex64;
use crate::Stage;
use crate::fft::{
    RealFftPair, RingFifo, SplitSpectrum, complex_mac_split, complex_mac_split_range,
    complex_mul_split, complex_mul_split_range,
};
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Largest partition size of any stage.
const MAX_PART: usize = 32_768;
/// Block growth factor between consecutive stages.
const GROWTH: usize = 4;
/// Tail stages with blocks up to this size use one real FFT per transform;
/// larger ones use the time-sliced four-step transform.
const DIRECT_FFT_MAX_BLOCK: usize = 512;
/// Frequency bins per multiply-accumulate job.
const MAC_CHUNK: usize = 128;
/// Target complex elements per batch of four-step sub-FFTs.
const SUBFFT_BATCH: usize = 256;

/// One stage of a partition schedule: UPOLS with `block`-frame partitions
/// covering IR samples `[offset, end)` (`end == usize::MAX` for the last).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagePlan {
    pub offset: usize,
    pub block: usize,
    pub end: usize,
}

/// The default schedule for a base block `base` and IR length `max_ir_len`.
fn plan_schedule(base: usize, max_ir_len: usize) -> Vec<StagePlan> {
    let mut plan = Vec::new();
    let mut block = base;
    let mut offset = 0;
    loop {
        let next_block = (block * GROWTH).min(MAX_PART);
        // The next stage starts at exactly twice its block size.
        let boundary = 2 * next_block;
        let last = (block == MAX_PART && !plan.is_empty()) || boundary >= max_ir_len;
        let end = if last { usize::MAX } else { boundary };
        plan.push(StagePlan { offset, block, end });
        if last {
            return plan;
        }
        offset = end;
        block = next_block;
    }
}

fn validate_plan(plan: &[StagePlan], base: usize) {
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

/// Pre-FFT'd IR partitions of `ir[offset..end)`, scaled by `1 / (2 * block)`.
fn ir_partitions(
    fft: &mut RealFftPair,
    ir: &[f64],
    offset: usize,
    block: usize,
    end: usize,
) -> Vec<SplitSpectrum> {
    if ir.len() <= offset {
        return Vec::new();
    }
    let inv_n = 1.0 / (2 * block) as f64;
    let stage_end = ir.len().min(end);
    let num_parts = (stage_end - offset).div_ceil(block);
    (0..num_parts)
        .map(|p| {
            let start = offset + p * block;
            let stop = (start + block).min(stage_end);
            fft.forward_padded_scaled(&ir[start..stop], inv_n)
        })
        .collect()
}

// ---------------------------------------------------------------- stage 0

/// Stage 0: UPOLS on the base block, computed immediately.
struct HeadStage {
    block_size: usize,
    /// Indexed `[ir_idx][partition_idx]`.
    ir_spectra: Vec<Vec<SplitSpectrum>>,
    fft: RealFftPair,
    acc: SplitSpectrum,
    time_out: Vec<f64>,
}

impl HeadStage {
    fn new(irs: &[Vec<f64>], block_size: usize, end: usize) -> Self {
        let mut fft = RealFftPair::new(2 * block_size);
        let ir_spectra = irs
            .iter()
            .map(|ir| ir_partitions(&mut fft, ir, 0, block_size, end))
            .collect();
        Self {
            block_size,
            ir_spectra,
            acc: SplitSpectrum::new(fft.spec_len()),
            time_out: vec![0.0; 2 * block_size],
            fft,
        }
    }

    /// Process one base block; the result overwrites `out[..block_size]`.
    fn process_block(
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

        complex_mul_split(&mut self.acc, &ch.fdl[ch.fdl_head], &parts[0]);
        for (p, part) in parts.iter().enumerate().skip(1) {
            let fdl_idx = (ch.fdl_head + num_parts - p) % num_parts;
            complex_mac_split(&mut self.acc, &ch.fdl[fdl_idx], part);
        }

        self.fft.inverse_from_split(&self.acc, &mut self.time_out);
        out.copy_from_slice(&self.time_out[self.block_size..]);
        ch.prev_in.copy_from_slice(input);
    }

    /// Rough cost of one `process_block` in flop-like units.
    fn block_cost(&self, ir_idx: usize) -> f64 {
        let parts = self.ir_spectra[ir_idx].len();
        if parts == 0 {
            return 0.0;
        }
        2.0 * real_fft_cost(self.block_size) + NS_MAC * (self.block_size + 1) as f64 * parts as f64
    }
}

struct HeadChannel {
    prev_in: Vec<f64>,
    fdl: Vec<SplitSpectrum>,
    fdl_head: usize,
}

impl HeadChannel {
    fn new(block_size: usize, spec_len: usize, num_parts: usize) -> Self {
        Self {
            prev_in: vec![0.0; block_size],
            fdl: (0..num_parts)
                .map(|_| SplitSpectrum::new(spec_len))
                .collect(),
            fdl_head: 0,
        }
    }

    fn reset(&mut self) {
        self.prev_in.fill(0.0);
        for spec in &mut self.fdl {
            spec.clear();
        }
        self.fdl_head = 0;
    }
}

// ---------------------------------------------------------------- tail stages

// Job costs, in nanoseconds per element as measured on one Zen 2 core
// (EPYC 7402P, release build). Only their ratios matter: they decide how the
// work of a period is spread over its base blocks.

/// Direct real FFT of `2B` points, per `B·log2(2B)`.
const NS_REAL_FFT: f64 = 0.42;
/// Spectral multiply-accumulate, per bin and partition.
const NS_MAC: f64 = 0.75;
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

fn real_fft_cost(block: usize) -> f64 {
    NS_REAL_FFT * block as f64 * (2.0 * block as f64).log2()
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
/// multiply-accumulate is elementwise, so the FDL and the IR spectra only
/// need to agree, and no transpose back to natural order is ever needed.
///
/// The inverse runs the same decomposition backwards on that layout: size-`l`
/// FFTs over `k2` on contiguous rows (after packing the Hermitian spectrum),
/// twiddle, then size-`m` FFTs over `k1` per column, which produce
/// `z[l*n1 + n2]`.
struct FourStepTables {
    b: usize,
    m: usize,
    l: usize,
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
    /// Columns per step-A job (forward and inverse).
    cols_a: usize,
    /// Rows per inverse step-B job.
    rows_b: usize,
}

struct FourStep {
    t: FourStepTables,
    scratch: Vec<Complex64>,
    gather: Vec<Complex64>,
}

impl FourStep {
    fn new(b: usize) -> Self {
        assert!(b.is_power_of_two() && b >= 16);
        let bits = b.trailing_zeros();
        let m = 1usize << (bits / 2);
        let l = b / m;
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
        let cols_a = (SUBFFT_BATCH / m).clamp(1, l);
        Self {
            t: FourStepTables {
                b,
                m,
                l,
                fwd_m,
                fwd_l,
                inv_m,
                inv_l,
                tw_cols,
                tw_rows,
                split,
                pre,
                cols_a,
                rows_b: (SUBFFT_BATCH / l).clamp(1, m),
            },
            scratch: vec![Complex64::new(0.0, 0.0); scratch_len],
            gather: vec![Complex64::new(0.0, 0.0); cols_a * m],
        }
    }
}

impl FourStepTables {
    /// A natural-order spectrum (`b + 1` bins) in spectrum order.
    fn permute(&self, nat: &SplitSpectrum) -> SplitSpectrum {
        let mut out = SplitSpectrum::new(self.b + 1);
        for p in 0..self.b {
            let k = p / self.l + self.m * (p % self.l);
            out.re[p] = nat.re[k];
            out.im[p] = nat.im[k];
        }
        out.re[self.b] = nat.re[self.b];
        out.im[self.b] = nat.im[self.b];
        out
    }

    /// Row `k1`'s mirror: `Z[b - k]` for `k = k1 + m*k2` sits at
    /// `base - k2` (row 0 mirrors onto itself shifted by one; its `k2 = 0`
    /// entry pairs with itself / the Nyquist bin and is handled separately).
    #[inline]
    fn mirror_base(&self, k1: usize) -> usize {
        if k1 == 0 {
            self.l
        } else {
            (self.m - k1) * self.l + self.l - 1
        }
    }

    /// Number of forward step-B jobs: row pairs `{k1, m - k1}` for
    /// `k1 in 0..=m/2`.
    fn fwd_b_jobs(&self) -> usize {
        self.m / 2 + 1
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
        let (m, l, b) = (self.m, self.l, self.b);
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
        let (m, l, b) = (self.m, self.l, self.b);
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
        let (l, b) = (self.l, self.b);
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
        let (m, l, b) = (self.m, self.l, self.b);
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

enum TailFft {
    Direct {
        fft: RealFftPair,
        time_out: Vec<f64>,
    },
    FourStep(Box<FourStep>),
}

/// One slice of a tail stage's per-period work. The index says which
/// chunk (columns, row pair, rows or bins) the job covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Job {
    /// Whole forward real FFT of `[prev, cur]` (small stages).
    FwdDirect,
    /// Four-step forward: step A columns, then step B + real split per row
    /// pair.
    FwdA(usize),
    FwdB(usize),
    /// Multiply-accumulate of all partitions over a chunk of bins.
    Mac(usize),
    /// Four-step inverse: pack + step B rows, then step A columns + unpack.
    InvB(usize),
    InvA(usize),
    /// Whole inverse real FFT (small stages).
    InvDirect,
    Done,
}

/// A tail stage: UPOLS on `block`-frame partitions starting at an IR offset
/// `>= 2 * block`, computed in slices across one period.
struct TailStage {
    block: usize,
    /// Base blocks per period (`block / base`).
    ticks: usize,
    /// Extra whole-partition delay: `(offset - 2 * block) / block`.
    delay_parts: usize,
    /// Indexed `[ir_idx][partition_idx]`.
    ir_spectra: Vec<Vec<SplitSpectrum>>,
    engine: TailFft,
}

struct TailChannel {
    num_parts: usize,
    /// `num_parts + delay_parts` slots.
    fdl: Vec<SplitSpectrum>,
    fdl_head: usize,
    /// Input blocks: `input[slot]` is being filled; the previous two are
    /// being transformed.
    input: [Vec<f64>; 3],
    slot: usize,
    /// Output for the current period (block `k - 2`).
    out_cur: Vec<f64>,
    /// Output being computed this period (block `k - 1`).
    out_next: Vec<f64>,
    acc: SplitSpectrum,
    /// Four-step work buffer (empty for direct stages).
    work: Vec<Complex64>,
    /// Base-block index within the current period.
    tick: usize,
    job: Job,
    done: f64,
    total: f64,
}

impl TailChannel {
    fn reset(&mut self) {
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
        self.tick = 0;
        self.job = Job::Done;
        self.done = 0.0;
    }
}

impl TailStage {
    fn new(irs: &[Vec<f64>], plan: StagePlan, base: usize) -> Self {
        let block = plan.block;
        let mut fft = RealFftPair::new(2 * block);
        let mut ir_spectra: Vec<Vec<SplitSpectrum>> = irs
            .iter()
            .map(|ir| ir_partitions(&mut fft, ir, plan.offset, block, plan.end))
            .collect();
        let engine = if block <= DIRECT_FFT_MAX_BLOCK {
            TailFft::Direct {
                fft,
                time_out: vec![0.0; 2 * block],
            }
        } else {
            let fs = FourStep::new(block);
            for parts in &mut ir_spectra {
                for spec in parts.iter_mut() {
                    *spec = fs.t.permute(spec);
                }
            }
            TailFft::FourStep(Box::new(fs))
        };
        Self {
            block,
            ticks: block / base,
            delay_parts: (plan.offset - 2 * block) / block,
            ir_spectra,
            engine,
        }
    }

    fn new_channel(&self, ir_idx: usize) -> Option<TailChannel> {
        let num_parts = self.ir_spectra[ir_idx].len();
        if num_parts == 0 {
            return None;
        }
        let spec_len = self.block + 1;
        let work_len = match self.engine {
            TailFft::Direct { .. } => 0,
            TailFft::FourStep(_) => self.block,
        };
        let mut ch = TailChannel {
            num_parts,
            fdl: (0..num_parts + self.delay_parts)
                .map(|_| SplitSpectrum::new(spec_len))
                .collect(),
            fdl_head: 0,
            input: std::array::from_fn(|_| vec![0.0; self.block]),
            slot: 0,
            out_cur: vec![0.0; self.block],
            out_next: vec![0.0; self.block],
            acc: SplitSpectrum::new(spec_len),
            work: vec![Complex64::new(0.0, 0.0); work_len],
            tick: 0,
            job: Job::Done,
            done: 0.0,
            total: 0.0,
        };
        let mut job = self.first_job();
        while job != Job::Done {
            ch.total += self.job_cost(job, num_parts);
            job = self.next_job(job);
        }
        Some(ch)
    }

    fn first_job(&self) -> Job {
        match self.engine {
            TailFft::Direct { .. } => Job::FwdDirect,
            TailFft::FourStep(_) => Job::FwdA(0),
        }
    }

    fn next_job(&self, job: Job) -> Job {
        let bins = self.block + 1;
        match (&self.engine, job) {
            (_, Job::Done | Job::InvDirect) => Job::Done,
            (_, Job::FwdDirect) => Job::Mac(0),
            (_, Job::Mac(start)) if start + MAC_CHUNK < bins => Job::Mac(start + MAC_CHUNK),
            (TailFft::Direct { .. }, Job::Mac(_)) => Job::InvDirect,
            (TailFft::FourStep(_), Job::Mac(_)) => Job::InvB(0),
            (TailFft::FourStep(fs), Job::FwdA(n2)) if n2 + fs.t.cols_a < fs.t.l => {
                Job::FwdA(n2 + fs.t.cols_a)
            }
            (TailFft::FourStep(_), Job::FwdA(_)) => Job::FwdB(0),
            (TailFft::FourStep(fs), Job::FwdB(k1)) if k1 + 1 < fs.t.fwd_b_jobs() => {
                Job::FwdB(k1 + 1)
            }
            (TailFft::FourStep(_), Job::FwdB(_)) => Job::Mac(0),
            (TailFft::FourStep(fs), Job::InvB(k1)) if k1 + fs.t.rows_b < fs.t.m => {
                Job::InvB(k1 + fs.t.rows_b)
            }
            (TailFft::FourStep(_), Job::InvB(_)) => Job::InvA(0),
            (TailFft::FourStep(fs), Job::InvA(n2)) if n2 + fs.t.cols_a < fs.t.l => {
                Job::InvA(n2 + fs.t.cols_a)
            }
            (TailFft::FourStep(_), Job::InvA(_)) => Job::Done,
            (TailFft::Direct { .. }, j) => unreachable!("job {j:?} on a direct stage"),
        }
    }

    fn job_cost(&self, job: Job, num_parts: usize) -> f64 {
        let bins = self.block + 1;
        match (&self.engine, job) {
            (_, Job::Done) => 0.0,
            (_, Job::FwdDirect | Job::InvDirect) => real_fft_cost(self.block),
            (_, Job::Mac(start)) => {
                NS_MAC * ((start + MAC_CHUNK).min(bins) - start) as f64 * num_parts as f64
            }
            (TailFft::FourStep(fs), Job::FwdA(n2)) => {
                fs.t.step_a_cost((n2 + fs.t.cols_a).min(fs.t.l) - n2, false)
            }
            (TailFft::FourStep(fs), Job::InvA(n2)) => {
                fs.t.step_a_cost((n2 + fs.t.cols_a).min(fs.t.l) - n2, true)
            }
            (TailFft::FourStep(fs), Job::FwdB(k1)) => fs.t.fwd_b_cost(k1),
            (TailFft::FourStep(fs), Job::InvB(k1)) => {
                let rows = (k1 + fs.t.rows_b).min(fs.t.m) - k1;
                fs.t.step_b_cost(rows) + NS_PACK * (rows * fs.t.l) as f64
            }
            (TailFft::Direct { .. }, j) => unreachable!("job {j:?} on a direct stage"),
        }
    }

    /// Run `ch.job`, advance to the next job and return its cost.
    fn run_job(&mut self, ir_idx: usize, ch: &mut TailChannel) -> f64 {
        let job = ch.job;
        let cost = self.job_cost(job, ch.num_parts);
        let next = self.next_job(job);
        let b = self.block;
        let bins = b + 1;
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
            (_, Job::Mac(start)) => {
                let range = start..(start + MAC_CHUNK).min(bins);
                let slots = ch.fdl.len();
                for (p, part) in ir_spectra[ir_idx].iter().enumerate() {
                    let idx = (ch.fdl_head + slots - p - *delay_parts) % slots;
                    if p == 0 {
                        complex_mul_split_range(&mut ch.acc, &ch.fdl[idx], part, range.clone());
                    } else {
                        complex_mac_split_range(&mut ch.acc, &ch.fdl[idx], part, range.clone());
                    }
                }
            }
            (TailFft::Direct { fft, time_out }, Job::InvDirect) => {
                fft.inverse_from_split(&ch.acc, time_out);
                ch.out_next.copy_from_slice(&time_out[b..]);
            }
            (TailFft::FourStep(fs), Job::FwdA(n2)) => {
                let FourStep { t, scratch, gather } = &mut **fs;
                let cols = n2..(n2 + t.cols_a).min(t.l);
                t.fwd_a(gather, scratch, &mut ch.work, cols, prev, cur);
            }
            (TailFft::FourStep(fs), Job::FwdB(k1)) => {
                let FourStep { t, scratch, .. } = &mut **fs;
                t.fwd_b_split(scratch, &mut ch.work, &mut ch.fdl[ch.fdl_head], k1);
            }
            (TailFft::FourStep(fs), Job::InvB(k1)) => {
                let FourStep { t, scratch, .. } = &mut **fs;
                let rows = k1..(k1 + t.rows_b).min(t.m);
                t.inv_b(scratch, &mut ch.work, &ch.acc, rows);
            }
            (TailFft::FourStep(fs), Job::InvA(n2)) => {
                let FourStep { t, scratch, gather } = &mut **fs;
                let cols = n2..(n2 + t.cols_a).min(t.l);
                t.inv_a(gather, scratch, &ch.work, cols, &mut ch.out_next);
            }
            (_, j) => unreachable!("job {j:?} does not match the stage engine"),
        }
        ch.job = next;
        cost
    }

    /// One base block for this stage: add this period's precomputed output
    /// into `out`, store the input, and run this block's share of the work.
    /// Returns the cost of the work done.
    fn tick(&mut self, ir_idx: usize, ch: &mut TailChannel, input: &[f64], out: &mut [f64]) -> f64 {
        let b0 = input.len();
        if ch.tick == 0 {
            debug_assert_eq!(ch.job, Job::Done, "previous period's work unfinished");
            std::mem::swap(&mut ch.out_cur, &mut ch.out_next);
            ch.slot = (ch.slot + 1) % 3;
            ch.fdl_head = (ch.fdl_head + 1) % ch.fdl.len();
            ch.job = self.first_job();
            ch.done = 0.0;
        }
        let pos = ch.tick * b0;
        for (o, &y) in out.iter_mut().zip(&ch.out_cur[pos..pos + b0]) {
            *o += y;
        }
        ch.input[ch.slot][pos..pos + b0].copy_from_slice(input);

        ch.tick += 1;
        let last = ch.tick == self.ticks;
        let target = ch.total * ch.tick as f64 / self.ticks as f64;
        let mut spent = 0.0;
        // Run jobs until this block's share is reached, rounding to the
        // nearest job so the overshoot does not accumulate in one direction.
        while ch.job != Job::Done
            && (last || ch.done + 0.5 * self.job_cost(ch.job, ch.num_parts) <= target)
        {
            let cost = self.run_job(ir_idx, ch);
            ch.done += cost;
            spent += cost;
        }
        if last {
            ch.tick = 0;
        }
        spent
    }
}

// ---------------------------------------------------------------- convolver

struct ChannelState {
    ir_idx: usize,
    in_fifo: RingFifo,
    out_fifo: RingFifo,
    head: HeadChannel,
    /// One entry per tail stage; `None` when this channel's IR ends before it.
    tails: Vec<Option<TailChannel>>,
}

pub struct Convolver {
    channels: usize,
    max_block: usize,
    base_block: usize,
    head: Option<HeadStage>,
    tails: Vec<TailStage>,
    ch_states: Vec<ChannelState>,
    block_in_scratch: Vec<f64>,
    block_out_scratch: Vec<f64>,
    /// Estimated work (flop-like units) done by the last `process` call.
    last_cost: f64,
}

impl Convolver {
    /// `irs`: either one IR (applied to every channel) or exactly `channels`
    /// IRs. `max_block`: the largest frame count `process` will receive.
    pub fn new(irs: &[Vec<f64>], channels: usize, max_block: usize) -> Self {
        let base = Self::base_block_for(max_block);
        let max_ir_len = irs.iter().map(|ir| ir.len()).max().unwrap_or(0);
        Self::with_plan(irs, channels, max_block, &plan_schedule(base, max_ir_len))
    }

    fn base_block_for(max_block: usize) -> usize {
        if max_block == 0 {
            64
        } else {
            max_block.next_power_of_two().clamp(64, 1024)
        }
    }

    /// Build with an explicit partition schedule (validated; see the module
    /// docs for the rules).
    pub(crate) fn with_plan(
        irs: &[Vec<f64>],
        channels: usize,
        max_block: usize,
        plan: &[StagePlan],
    ) -> Self {
        let base_block = Self::base_block_for(max_block);
        if channels == 0 {
            return Self {
                channels: 0,
                max_block,
                base_block,
                head: None,
                tails: Vec::new(),
                ch_states: Vec::new(),
                block_in_scratch: Vec::new(),
                block_out_scratch: Vec::new(),
                last_cost: 0.0,
            };
        }
        assert!(
            irs.len() == 1 || irs.len() == channels,
            "Convolver expects 1 mono IR or exactly {channels} IRs, got {}",
            irs.len()
        );
        validate_plan(plan, base_block);

        let head = HeadStage::new(irs, base_block, plan[0].end);
        let tails: Vec<TailStage> = plan[1..]
            .iter()
            .map(|&p| TailStage::new(irs, p, base_block))
            .collect();

        let mono = irs.len() == 1;
        let fifo_cap = base_block + max_block.max(base_block);
        let ch_states = (0..channels)
            .map(|ch| {
                let ir_idx = if mono { 0 } else { ch };
                let mut out_fifo = RingFifo::with_capacity(fifo_cap);
                out_fifo.reset_zeros(base_block);
                ChannelState {
                    ir_idx,
                    in_fifo: RingFifo::with_capacity(fifo_cap),
                    out_fifo,
                    head: HeadChannel::new(
                        base_block,
                        head.fft.spec_len(),
                        head.ir_spectra[ir_idx].len(),
                    ),
                    tails: tails.iter().map(|t| t.new_channel(ir_idx)).collect(),
                }
            })
            .collect();

        Self {
            channels,
            max_block,
            base_block,
            head: Some(head),
            tails,
            ch_states,
            block_in_scratch: vec![0.0; base_block],
            block_out_scratch: vec![0.0; base_block],
            last_cost: 0.0,
        }
    }

    /// Estimated work of the last `process` call, in flop-like units. Lets
    /// tests check that tail-stage work is spread evenly without timing.
    pub fn last_process_cost(&self) -> f64 {
        self.last_cost
    }
}

impl Stage for Convolver {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if self.channels == 0 {
            assert!(chans.is_empty());
            return;
        }
        assert_eq!(chans.len(), self.channels, "channel count mismatch");
        let n = chans[0].len();
        assert!(
            n <= self.max_block,
            "frame count {n} exceeds max_block {}",
            self.max_block
        );
        for ch in chans.iter() {
            assert_eq!(ch.len(), n, "all channels must have the same frame count");
        }
        self.last_cost = 0.0;
        if n == 0 {
            return;
        }

        let b0 = self.base_block;
        let head = self.head.as_mut().expect("head stage exists with channels");

        for (c, ch_buf) in chans.iter_mut().enumerate() {
            let ch = &mut self.ch_states[c];
            let ir_idx = ch.ir_idx;
            ch.in_fifo.push_slice(ch_buf);

            while ch.in_fifo.len() >= b0 {
                ch.in_fifo.pop_slice(&mut self.block_in_scratch);
                head.process_block(
                    ir_idx,
                    &mut ch.head,
                    &self.block_in_scratch,
                    &mut self.block_out_scratch,
                );
                self.last_cost += head.block_cost(ir_idx);
                for (stage, tail) in self.tails.iter_mut().zip(ch.tails.iter_mut()) {
                    if let Some(tail) = tail {
                        self.last_cost += stage.tick(
                            ir_idx,
                            tail,
                            &self.block_in_scratch,
                            &mut self.block_out_scratch,
                        );
                    }
                }
                ch.out_fifo.push_slice(&self.block_out_scratch);
            }

            ch.out_fifo.pop_slice(ch_buf);
        }
    }

    fn latency_frames(&self) -> usize {
        self.base_block
    }

    fn reset(&mut self) {
        for ch in &mut self.ch_states {
            ch.in_fifo.clear();
            ch.out_fifo.reset_zeros(self.base_block);
            ch.head.reset();
            for tail in ch.tails.iter_mut().flatten() {
                tail.reset();
            }
        }
        self.last_cost = 0.0;
    }
}

/// Maximum allowed combined impulse response length in taps (at graph rate).
pub const MAX_COMBINED_IR_TAPS: usize = 4_000_000;

/// Compute the combined length of linearly convolving IRs of lengths `lengths`.
pub fn combined_ir_length(lengths: &[usize]) -> Result<usize, crate::chain::ChainError> {
    if lengths.is_empty() {
        return Err(crate::chain::ChainError::MissingIr);
    }
    let mut sum = 0usize;
    for &len in lengths {
        if len == 0 {
            return Err(crate::chain::ChainError::EmptyIr);
        }
        sum = sum.saturating_add(len);
    }
    let taps = sum - (lengths.len() - 1);
    if taps > MAX_COMBINED_IR_TAPS {
        return Err(crate::chain::ChainError::IrTooLong {
            taps,
            max: MAX_COMBINED_IR_TAPS,
        });
    }
    Ok(taps)
}

/// Linearly convolve all enabled impulse responses (each paired with its
/// `gain_db`) into one combined IR per channel (or a single mono IR if every
/// input IR is mono), enforcing [`MAX_COMBINED_IR_TAPS`].
///
/// Mono IRs (`len == 1`) are applied to every channel; multi-channel IRs must
/// match `channels` and map channel-for-channel (e.g. stereo L→L, R→R).
pub fn combine_irs(
    irs: &[(&[Vec<f64>], f64)],
    channels: usize,
) -> Result<Vec<Vec<f64>>, crate::chain::ChainError> {
    use crate::chain::ChainError;

    if channels == 0 {
        return Err(ChainError::ZeroChannels);
    }
    if irs.is_empty() {
        return Err(ChainError::MissingIr);
    }
    for &(ir, _) in irs {
        if ir.is_empty() || ir.iter().any(Vec::is_empty) {
            return Err(ChainError::EmptyIr);
        }
        if ir.len() != 1 && ir.len() != channels {
            return Err(ChainError::IrCount {
                irs: ir.len(),
                channels,
            });
        }
    }

    let all_mono = irs.iter().all(|&(ir, _)| ir.len() == 1);
    let out_channels = if all_mono { 1 } else { channels };
    let total_gain: f64 = irs.iter().map(|&(_, g)| crate::db_to_lin(g)).product();

    let mut out_lens = Vec::with_capacity(out_channels);
    let mut max_out_len = 0usize;
    let mut ch_lengths = Vec::with_capacity(irs.len());
    for c in 0..out_channels {
        ch_lengths.clear();
        for &(ir, _) in irs {
            let slice = if ir.len() == 1 { &ir[0] } else { &ir[c] };
            ch_lengths.push(slice.len());
        }
        let len = combined_ir_length(&ch_lengths)?;
        max_out_len = max_out_len.max(len);
        out_lens.push(len);
    }

    if irs.len() == 1 {
        let (ir, _) = irs[0];
        return Ok((0..out_channels)
            .map(|c| {
                let src = if ir.len() == 1 { &ir[0] } else { &ir[c] };
                src.iter().map(|&s| s * total_gain).collect()
            })
            .collect());
    }

    let fft_len = max_out_len.max(2).next_power_of_two();
    let mut fft = RealFftPair::new(fft_len);
    let spec_len = fft.spec_len();
    let mut acc = SplitSpectrum::new(spec_len);
    let mut tmp = SplitSpectrum::new(spec_len);
    let mut time_out = vec![0.0; fft_len];
    let scale = total_gain / (fft_len as f64);

    let mut combined = Vec::with_capacity(out_channels);
    for (c, &out_len) in out_lens.iter().enumerate() {
        let first = if irs[0].0.len() == 1 {
            &irs[0].0[0]
        } else {
            &irs[0].0[c]
        };
        fft.forward_padded_into_split(first, 1.0, &mut acc);

        for &(ir, _) in &irs[1..] {
            let slice = if ir.len() == 1 { &ir[0] } else { &ir[c] };
            fft.forward_padded_into_split(slice, 1.0, &mut tmp);
            for i in 0..spec_len {
                let ar = acc.re[i];
                let ai = acc.im[i];
                let br = tmp.re[i];
                let bi = tmp.im[i];
                acc.re[i] = ar * br - ai * bi;
                acc.im[i] = ar * bi + ai * br;
            }
        }

        fft.inverse_from_split(&acc, &mut time_out);
        let ch_out: Vec<f64> = time_out[..out_len].iter().map(|&v| v * scale).collect();
        combined.push(ch_out);
    }

    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let bits = ((*seed >> 11) as f64) / ((1u64 << 53) as f64);
        bits * 2.0 - 1.0
    }

    fn naive_convolve(x: &[f64], h: &[f64]) -> Vec<f64> {
        let mut y = vec![0.0; x.len()];
        for (n, yn) in y.iter_mut().enumerate() {
            let max_k = h.len().min(n + 1);
            let mut acc = 0.0;
            for k in 0..max_k {
                acc += h[k] * x[n - k];
            }
            *yn = acc;
        }
        y
    }

    /// Reference linear convolution (first `x.len()` samples) through one
    /// big real FFT.
    fn fft_convolve(x: &[f64], h: &[f64]) -> Vec<f64> {
        if h.is_empty() {
            return vec![0.0; x.len()];
        }
        let nfft = (x.len() + h.len()).next_power_of_two().max(2);
        let mut fft = RealFftPair::new(nfft);
        let spec_x = fft.forward_padded_scaled(x, 1.0);
        let spec_h = fft.forward_padded_scaled(h, 1.0 / nfft as f64);
        let mut prod = SplitSpectrum::new(fft.spec_len());
        complex_mul_split(&mut prod, &spec_x, &spec_h);
        let mut y = vec![0.0; nfft];
        fft.inverse_from_split(&prod, &mut y);
        y.truncate(x.len());
        y
    }

    /// Feed `inputs` through `conv` in chunks of varying size (0, 1, odd,
    /// `max_block`, random) and return the outputs.
    fn run_chunks(
        conv: &mut Convolver,
        inputs: &[Vec<f64>],
        max_block: usize,
        seed: u64,
    ) -> Vec<Vec<f64>> {
        let channels = inputs.len();
        let total = inputs[0].len();
        let mut seed = seed;
        let mut outputs = vec![vec![0.0; total]; channels];
        let mut work = vec![vec![0.0; max_block]; channels];
        let mut offset = 0;
        let mut step = 0usize;
        while offset < total {
            let rem = total - offset;
            let chunk = match step % 7 {
                0 => 0,
                1 => max_block,
                2 => 1,
                3 => (max_block / 2) | 1,
                _ => {
                    let r = ((lcg(&mut seed) + 1.0) * 0.5 * max_block as f64) as usize;
                    r.clamp(1, max_block)
                }
            }
            .min(rem);
            step += 1;
            for c in 0..channels {
                work[c][..chunk].copy_from_slice(&inputs[c][offset..offset + chunk]);
            }
            {
                let mut slices: Vec<&mut [f64]> =
                    work.iter_mut().map(|v| &mut v[..chunk]).collect();
                conv.process(&mut slices);
            }
            for c in 0..channels {
                outputs[c][offset..offset + chunk].copy_from_slice(&work[c][..chunk]);
            }
            offset += chunk;
        }
        outputs
    }

    /// Run random input through a convolver and compare against the
    /// reference. Returns the worst error relative to the output peak.
    fn check_against_reference(
        irs: &[Vec<f64>],
        channels: usize,
        max_block: usize,
        input_frames: usize,
        seed: u64,
    ) -> f64 {
        let mut s = seed;
        let mut conv = Convolver::new(irs, channels, max_block);
        let lat = conv.latency_frames();
        let inputs: Vec<Vec<f64>> = (0..channels)
            .map(|_| {
                let mut x: Vec<f64> = (0..input_frames).map(|_| lcg(&mut s)).collect();
                x.resize(input_frames + lat, 0.0);
                x
            })
            .collect();
        let outputs = run_chunks(&mut conv, &inputs, max_block, seed ^ 0x55);
        let mut worst = 0.0f64;
        for c in 0..channels {
            let ir = if irs.len() == 1 { &irs[0] } else { &irs[c] };
            let expected = if input_frames * ir.len() <= 20_000_000 {
                naive_convolve(&inputs[c][..input_frames], ir)
            } else {
                fft_convolve(&inputs[c][..input_frames], ir)
            };
            let peak = expected.iter().fold(1e-30f64, |m, v| m.max(v.abs()));
            let mut err = outputs[c][..lat].iter().fold(0.0f64, |m, v| m.max(v.abs()));
            for i in 0..input_frames {
                err = err.max((outputs[c][i + lat] - expected[i]).abs());
            }
            let rel = err / peak.max(1.0);
            assert!(
                rel <= 1e-12,
                "ir_len={}, ch={c}, max_block={max_block}: max_err={err:.3e} (peak {peak:.3})",
                ir.len()
            );
            worst = worst.max(rel);
        }
        worst
    }

    #[test]
    fn default_schedule_starts_each_tail_stage_at_twice_its_block() {
        // The repro case: block 1024 used to start a 32768-block stage at
        // IR offset 65536, playing it out 32768 frames early.
        let plan = plan_schedule(1024, 1 << 20);
        assert_eq!(
            plan,
            vec![
                StagePlan {
                    offset: 0,
                    block: 1024,
                    end: 8192
                },
                StagePlan {
                    offset: 8192,
                    block: 4096,
                    end: 32_768
                },
                StagePlan {
                    offset: 32_768,
                    block: 16_384,
                    end: 65_536
                },
                StagePlan {
                    offset: 65_536,
                    block: 32_768,
                    end: usize::MAX
                },
            ]
        );
        for base in [64, 128, 256, 512, 1024] {
            for ir_len in [0, 1, 8 * base, 8 * base + 1, 70_000, 600_000] {
                let plan = plan_schedule(base, ir_len);
                validate_plan(&plan, base);
                for st in &plan[1..] {
                    assert_eq!(st.offset, 2 * st.block, "base {base}: {st:?}");
                    assert!(st.block <= MAX_PART);
                }
            }
        }
    }

    #[test]
    fn single_tap_at_65536_lands_at_65536_plus_latency() {
        let mut ir = vec![0.0; 65_537];
        ir[65_536] = 1.0;
        let mut conv = Convolver::new(&[ir], 1, 1024);
        let lat = conv.latency_frames();
        assert_eq!(lat, 1024);
        let total = 70_000;
        let mut x = vec![0.0; total];
        x[0] = 1.0;
        let out = run_chunks(&mut conv, &[x], 1024, 1).pop().unwrap();
        let (peak_idx, peak) = out
            .iter()
            .enumerate()
            .fold((0, 0.0f64), |(bi, bv), (i, &v)| {
                if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
            });
        assert_eq!(peak_idx, 66_560);
        assert!((peak - 1.0).abs() < 1e-12);
    }

    #[test]
    fn impulse_taps_at_every_partition_boundary() {
        for max_block in [1024usize, 64, 256, 100, 512] {
            let base = Convolver::base_block_for(max_block);
            let plan = plan_schedule(base, 300_000);
            // Taps at every partition boundary of every stage (±1), plus
            // fixed positions around and beyond 65536.
            let mut positions = vec![
                0usize, 1, 65_535, 65_536, 65_537, 98_303, 98_304, 131_071, 131_072, 131_073,
                200_000, 262_143,
            ];
            for st in &plan {
                let end = st.end.min(270_000);
                let mut p = st.offset;
                while p < end {
                    positions.extend([p.saturating_sub(1), p, p + 1]);
                    p += st.block;
                }
            }
            positions.sort_unstable();
            positions.dedup();
            let len = positions.last().unwrap() + 1;
            let mut ir = vec![0.0; len];
            for (i, &p) in positions.iter().enumerate() {
                ir[p] = if i % 2 == 0 { 1.0 } else { -0.5 } * (1.0 - 1e-4 * i as f64);
            }

            let mut conv = Convolver::new(std::slice::from_ref(&ir), 1, max_block);
            let lat = conv.latency_frames();
            // Two impulses at different block phases: y = ir + 0.25 ir[n-777].
            let total = len + 2000 + lat;
            let mut x = vec![0.0; total];
            x[0] = 1.0;
            x[777] = 0.25;
            let out = run_chunks(&mut conv, &[x], max_block, max_block as u64)
                .pop()
                .unwrap();
            let tap = |n: isize| {
                if n >= 0 && (n as usize) < len {
                    ir[n as usize]
                } else {
                    0.0
                }
            };
            for (i, &y) in out.iter().enumerate() {
                let n = i as isize - lat as isize;
                let expected = tap(n) + 0.25 * tap(n - 777);
                assert!(
                    (y - expected).abs() < 1e-12,
                    "max_block {max_block}: output {i} (IR index {n}) = {y}, expected {expected}"
                );
            }
        }
    }

    #[test]
    fn convolver_matches_naive_various_ir_lengths_and_chunks() {
        let mut seed = 42u64;
        for ir_len in [1, 7, 1000] {
            let ir: Vec<f64> = (0..ir_len).map(|_| lcg(&mut seed) * 0.1).collect();
            check_against_reference(&[ir], 2, 128, 4500, 100 + ir_len as u64);
        }
    }

    #[test]
    fn convolver_mono_shared_and_mixed_channel_lengths() {
        let mut seed = 314u64;
        let mono_ir: Vec<f64> = (0..1500).map(|_| lcg(&mut seed) * 0.1).collect();
        check_against_reference(&[mono_ir], 3, 100, 3500, 315);

        let ir0: Vec<f64> = Vec::new();
        let ir1: Vec<f64> = (0..9).map(|_| lcg(&mut seed) * 0.1).collect();
        let ir2: Vec<f64> = (0..4000).map(|_| lcg(&mut seed) * 0.1).collect();
        check_against_reference(&[ir0, ir1, ir2], 3, 128, 5000, 316);
    }

    #[test]
    fn random_70k_stereo_irs_match_fft_reference() {
        let mut seed = 12345u64;
        let irs: Vec<Vec<f64>> = (0..2)
            .map(|c| {
                (0..70_000 + 3 * c)
                    .map(|i| lcg(&mut seed) * (-1e-5 * i as f64).exp() * 0.05)
                    .collect()
            })
            .collect();
        for max_block in [1024, 333] {
            check_against_reference(&irs, 2, max_block, 90_000, 999 + max_block as u64);
        }
    }

    #[test]
    fn random_200k_ir_matches_fft_reference() {
        let mut seed = 777u64;
        let ir: Vec<f64> = (0..200_000).map(|_| lcg(&mut seed) * 0.02).collect();
        for max_block in [64, 1000] {
            check_against_reference(
                std::slice::from_ref(&ir),
                1,
                max_block,
                230_000,
                max_block as u64,
            );
        }
    }

    #[test]
    fn random_600k_ir_matches_fft_reference() {
        let mut seed = 4242u64;
        let ir: Vec<f64> = (0..600_000).map(|_| lcg(&mut seed) * 0.01).collect();
        let worst = check_against_reference(&[ir], 1, 257, 640_000, 17);
        eprintln!("600k taps: worst error relative to peak {worst:.2e}");
    }

    #[test]
    fn explicit_schedule_with_extra_stage_delay() {
        // Stages that start later than twice their block (delay made up in
        // the frequency-domain delay line), on both the direct and the
        // four-step engine.
        let plan = [
            StagePlan {
                offset: 0,
                block: 64,
                end: 512,
            },
            StagePlan {
                offset: 512,
                block: 128,
                end: 8192,
            }, // 4·B: 2 extra slots
            StagePlan {
                offset: 8192,
                block: 4096,
                end: 24_576,
            }, // 2·B, four-step
            StagePlan {
                offset: 24_576,
                block: 8192,
                end: usize::MAX,
            }, // 3·B, four-step
        ];
        let mut seed = 99u64;
        let ir: Vec<f64> = (0..45_000).map(|_| lcg(&mut seed) * 0.05).collect();
        let mut conv = Convolver::with_plan(std::slice::from_ref(&ir), 1, 64, &plan);
        let lat = conv.latency_frames();
        let n = 60_000;
        let mut x: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
        x.resize(n + lat, 0.0);
        let out = run_chunks(&mut conv, std::slice::from_ref(&x), 64, 3)
            .pop()
            .unwrap();
        let expected = fft_convolve(&x[..n], &ir);
        let peak = expected.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        for i in 0..n {
            assert!(
                (out[i + lat] - expected[i]).abs() <= 1e-12 * peak.max(1.0),
                "sample {i}: {} vs {}",
                out[i + lat],
                expected[i]
            );
        }
    }

    #[test]
    #[should_panic(expected = "at least twice")]
    fn schedule_without_slack_is_rejected() {
        let plan = [
            StagePlan {
                offset: 0,
                block: 64,
                end: 256,
            },
            StagePlan {
                offset: 256,
                block: 256,
                end: usize::MAX,
            },
        ];
        let _ = Convolver::with_plan(&[vec![1.0; 1000]], 1, 64, &plan);
    }

    /// 1 s IR at 192 kHz, block 256 (the stated worst case for tail spikes).
    fn one_second_ir_192k(channels: usize) -> Vec<Vec<f64>> {
        let mut seed = 0x1234_5678_u64;
        (0..channels)
            .map(|_| {
                (0..192_000)
                    .map(|i| lcg(&mut seed) * (-(i as f64) / 40_000.0).exp() * 0.01)
                    .collect()
            })
            .collect()
    }

    #[test]
    fn tail_work_is_spread_evenly_across_process_calls() {
        let irs = one_second_ir_192k(2);
        let block = 256;
        let mut conv = Convolver::new(&irs, 2, block);
        assert_eq!(plan_schedule(256, 192_000).last().unwrap().block, MAX_PART);
        let mut a = vec![0.0; block];
        let mut b = vec![0.0; block];
        let mut costs = Vec::new();
        // Three periods of the largest stage after a one-period warm-up.
        let calls = 4 * MAX_PART / block;
        for i in 0..calls {
            a[0] = 1.0;
            b[0] = -1.0;
            conv.process(&mut [&mut a, &mut b]);
            if i >= MAX_PART / block {
                costs.push(conv.last_process_cost());
            }
        }
        let mean = costs.iter().sum::<f64>() / costs.len() as f64;
        let max = costs.iter().fold(0.0f64, |m, &c| m.max(c));
        let min = costs.iter().fold(f64::MAX, |m, &c| m.min(c));
        eprintln!(
            "work per call: mean {mean:.0}, min {min:.0}, max {max:.0} (max/mean {:.3})",
            max / mean
        );
        assert!(max <= 1.15 * mean, "max {max} vs mean {mean}");
    }

    #[test]
    fn convolver_reset_clears_state() {
        let mut seed = 77u64;
        let ir: Vec<f64> = (0..40_000).map(|_| lcg(&mut seed) * 0.1).collect();
        let n = 50_000;
        let x: Vec<f64> = (0..n).map(|_| lcg(&mut seed)).collect();
        let mut fresh = Convolver::new(std::slice::from_ref(&ir), 1, 256);
        let reference = run_chunks(&mut fresh, std::slice::from_ref(&x), 256, 5);

        let mut conv = Convolver::new(std::slice::from_ref(&ir), 1, 256);
        // Stop mid-period of every stage, then reset.
        let junk: Vec<f64> = (0..12_345).map(|_| lcg(&mut seed)).collect();
        run_chunks(&mut conv, &[junk], 256, 6);
        conv.reset();
        let again = run_chunks(&mut conv, std::slice::from_ref(&x), 256, 5);
        assert_eq!(reference, again);

        // After reset, zeros in give exact zeros out.
        conv.reset();
        let z = run_chunks(&mut conv, &[vec![0.0; 70_000]], 256, 7);
        assert!(z[0].iter().all(|&v| v == 0.0));
    }

    #[test]
    #[ignore]
    fn bench_worst_call_1s_ir_192k_block256() {
        use std::time::Instant;
        let irs = one_second_ir_192k(2);
        let block = 256;
        let mut conv = Convolver::new(&irs, 2, block);
        let mut seed = 5u64;
        let mut a = vec![0.0; block];
        let mut b = vec![0.0; block];
        let calls = 192_000 * 6 / block;
        let mut times = Vec::with_capacity(calls);
        for _ in 0..calls {
            for i in 0..block {
                a[i] = lcg(&mut seed) * 0.1;
                b[i] = lcg(&mut seed) * 0.1;
            }
            let t0 = Instant::now();
            conv.process(&mut [&mut a, &mut b]);
            times.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        let warm = &mut times[750..];
        let mean = warm.iter().sum::<f64>() / warm.len() as f64;
        warm.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let p99 = warm[warm.len() * 99 / 100];
        let worst = *warm.last().unwrap();
        let period = block as f64 / 192_000.0 * 1e6;
        eprintln!(
            "1 s IR @192k stereo, block {block} (period {period:.0} us): mean {mean:.1} us, p99 {p99:.1} us, worst {worst:.1} us"
        );
    }

    #[test]
    #[ignore]
    fn bench_convolver_65536_stereo_192khz() {
        use std::time::Instant;
        let mut seed = 0xdeadbeef_u64;
        let ir0: Vec<f64> = (0..65_536).map(|_| lcg(&mut seed) * 0.01).collect();
        let ir1: Vec<f64> = (0..65_536).map(|_| lcg(&mut seed) * 0.01).collect();
        let block = 1024usize;
        let mut conv = Convolver::new(&[ir0, ir1], 2, block);

        let mut ch0 = vec![0.1; block];
        let mut ch1 = vec![-0.1; block];
        for _ in 0..100 {
            conv.process(&mut [&mut ch0, &mut ch1]);
        }

        let iters = 2000usize;
        let t0 = Instant::now();
        for _ in 0..iters {
            ch0[0] = 0.1;
            ch1[0] = -0.1;
            conv.process(&mut [&mut ch0, &mut ch1]);
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let audio_secs = (iters * block) as f64 / 192_000.0;
        let rtf = audio_secs / elapsed;
        eprintln!(
            "Convolver 65536-tap stereo @ 192kHz (block {block}): {audio_secs:.2}s audio in {elapsed:.4}s => {rtf:.2}x real-time"
        );
    }

    #[test]
    fn combine_irs_matches_serial_convolution_null_test() {
        let mut seed = 0xCAFE_BABEu64;
        // Mix of stereo + mono + stereo IRs with non-trivial lengths and gains.
        let ir1 = vec![
            (0..311)
                .map(|i| lcg(&mut seed) * (-0.01 * i as f64).exp())
                .collect::<Vec<_>>(),
            (0..311)
                .map(|i| lcg(&mut seed) * (-0.012 * i as f64).exp())
                .collect::<Vec<_>>(),
        ];
        let ir2 = vec![
            (0..517)
                .map(|i| lcg(&mut seed) * (-0.006 * i as f64).exp())
                .collect::<Vec<_>>(),
        ];
        let ir3 = vec![
            (0..129)
                .map(|i| lcg(&mut seed) * (-0.02 * i as f64).exp())
                .collect::<Vec<_>>(),
            (0..129)
                .map(|i| lcg(&mut seed) * (-0.015 * i as f64).exp())
                .collect::<Vec<_>>(),
        ];

        let gains = [-2.5, 1.25, -0.75];
        let combined =
            combine_irs(&[(&ir1, gains[0]), (&ir2, gains[1]), (&ir3, gains[2])], 2).unwrap();
        assert_eq!(combined.len(), 2);
        let expected_len = 311 + 517 + 129 - 2;
        assert_eq!(combined[0].len(), expected_len);
        assert_eq!(combined[1].len(), expected_len);

        // Build test signal (padded by latency + IR tails).
        let max_block = 128;
        let frames = 4096;
        let in_l: Vec<f64> = (0..frames)
            .map(|i| if i < 2048 { lcg(&mut seed) } else { 0.0 })
            .collect();
        let in_r: Vec<f64> = (0..frames)
            .map(|i| if i < 2048 { lcg(&mut seed) } else { 0.0 })
            .collect();

        // Path A: single Convolver running the combined IR.
        let mut conv_combined = Convolver::new(&combined, 2, max_block);
        let out_combined = run_chunks(
            &mut conv_combined,
            &[in_l.clone(), in_r.clone()],
            max_block,
            11,
        );

        // Path B: three Convolvers in series, each IR scaled by its own gain_db.
        let scale_ir = |ir: &[Vec<f64>], db: f64| -> Vec<Vec<f64>> {
            let g = crate::db_to_lin(db);
            ir.iter()
                .map(|c| c.iter().map(|&v| v * g).collect())
                .collect()
        };
        let mut c1 = Convolver::new(&scale_ir(&ir1, gains[0]), 2, max_block);
        let mut c2 = Convolver::new(&scale_ir(&ir2, gains[1]), 2, max_block);
        let mut c3 = Convolver::new(&scale_ir(&ir3, gains[2]), 2, max_block);
        let s1 = run_chunks(&mut c1, &[in_l, in_r], max_block, 12);
        let s2 = run_chunks(&mut c2, &s1, max_block, 13);
        let out_serial = run_chunks(&mut c3, &s2, max_block, 14);

        // Serial adds 3 * lat latency vs 1 * lat for combined, so shift by 2 * lat.
        let lat = conv_combined.latency_frames();
        assert_eq!(c1.latency_frames(), lat);
        let extra_lat = 2 * lat;
        for ch in 0..2 {
            let valid_len = frames - extra_lat;
            let peak = out_serial[ch][extra_lat..]
                .iter()
                .fold(0.0f64, |m, v| m.max(v.abs()));
            let max_err = (0..valid_len)
                .map(|i| (out_combined[ch][i] - out_serial[ch][i + extra_lat]).abs())
                .fold(0.0f64, f64::max);
            let rel = max_err / peak.max(1e-12);
            assert!(rel <= 1e-12, "ch {ch} serial vs combined rel err {rel:e}");
        }
    }

    #[test]
    fn combine_irs_keeps_mono_when_all_inputs_mono_and_enforces_cap() {
        let m1 = vec![vec![0.5, 0.25]];
        let m2 = vec![vec![1.0, -0.5, 0.25]];
        let out = combine_irs(&[(&m1, 0.0), (&m2, -6.0)], 2).unwrap();
        assert_eq!(out.len(), 1);
        let g = crate::db_to_lin(-6.0);
        let expected = [0.5 * g, 0.0, 0.0, 0.0625 * g];
        assert_eq!(out[0].len(), expected.len());
        for (a, b) in out[0].iter().zip(&expected) {
            assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        }

        // Single IR > 4M taps or two IRs whose combined length > 4M taps are rejected.
        assert_eq!(
            combined_ir_length(&[MAX_COMBINED_IR_TAPS]).unwrap(),
            MAX_COMBINED_IR_TAPS
        );
        assert_eq!(
            combined_ir_length(&[2_000_001, 2_000_000]).unwrap(),
            MAX_COMBINED_IR_TAPS
        );
        assert!(matches!(
            combined_ir_length(&[2_000_001, 2_000_001]),
            Err(crate::chain::ChainError::IrTooLong {
                taps: 4_000_001,
                max: MAX_COMBINED_IR_TAPS
            })
        ));

        // Channel mismatch (3ch IR on 2ch chain) fails.
        let bad = vec![vec![1.0], vec![1.0], vec![1.0]];
        assert!(matches!(
            combine_irs(&[(&bad, 0.0)], 2),
            Err(crate::chain::ChainError::IrCount {
                irs: 3,
                channels: 2
            })
        ));
    }
}
