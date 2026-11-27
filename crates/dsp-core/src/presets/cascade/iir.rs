//! Recursive stages: one high-order direct-form-I filter per stage.
//!
//! Each stage zero-stuffs by its factor f, runs the filter at the stage's
//! output rate and multiplies by f. The recursion is (a[0] = 1)
//!
//! ```text
//! y[t] = sum_{k=0..n} b[k] x[t-k] - sum_{k=1..n} a[k] y[t-k]
//! ```
//!
//! in double with fused multiply-add, in a fixed order (the order is part
//! of the recipe; a plain left-to-right sum gives a different result in
//! the last bits, enough to move the 32-bit output by an LSB):
//!
//! 1. B = x[t] b[0], A = 0.
//! 2. If n >= 16: sixteen partial sums per side, PB[g][j] and PA[g][j]
//!    (g, j = 0..4), zero except PB[0][0] = B. Tap k = 1 + 16m + 4g + j
//!    (m = 0, and 1 if n >= 32) does PB[g][j] = fma(x[t-k], b[k], PB[g][j])
//!    and PA[g][j] = fma(y[t-k], a[k], PA[g][j]). Each side reduces as
//!    s[j] = (P[3][j] + P[2][j]) + (P[1][j] + P[0][j]),
//!    S = (s[0] + s[2]) + (s[1] + s[3]).
//! 3. The remaining taps in increasing k: B = fma(x[t-k], b[k], B),
//!    A = fma(y[t-k], a[k], A).
//! 4. y[t] = B - A.
//!
//! High-order direct forms are numerically fragile; these four tables are
//! fixed and run fine in double, but a new design should use second-order
//! sections.

use super::stage_factors;
use crate::presets::{FilterPreset, UpsamplerRuntime};
use audivane_protocol::RateRatio;

/// Coefficients of one stage filter, b[0..=n] and a[0..=n] (a[0] = 1).
pub(super) struct Table {
    pub b: &'static [f64],
    pub a: &'static [f64],
}

impl Table {
    pub fn order(&self) -> usize {
        self.b.len() - 1
    }
}

/// Elliptic-type x2 stage, order 16: ripple 0.001 dB, -3 dB at 0.2300 fs,
/// -100 dB from 0.2472 fs (fs: the stage output rate).
pub(super) const ELLIPTIC_X2: Table = Table {
    b: &[
        0.0005369312802673391,
        0.0027989540726814787,
        0.009693919667746692,
        0.024394602760616873,
        0.04944142306226031,
        0.08315569499150406,
        0.11913379767911962,
        0.14708678854570267,
        0.15769871917637884,
        0.1470867885457028,
        0.11913379767911947,
        0.08315569499150403,
        0.04944142306226036,
        0.024394602760616876,
        0.009693919667746692,
        0.0027989540726814787,
        0.0005369312802673391,
    ],
    a: &[
        1.0,
        -4.013205794272999,
        11.990627815969097,
        -25.0214112671948,
        42.820531022695434,
        -59.67622341215291,
        70.7051521665934,
        -71.08776795520369,
        61.42852601913226,
        -45.39382961767451,
        28.63309484705321,
        -15.220439640860604,
        6.7037847464068605,
        -2.3709260204210567,
        0.6393772204396305,
        -0.11865843257082657,
        0.01166985637978312,
    ],
};

/// Elliptic-type x3 stage, order 17: -3 dB at 0.1532 fs, -100 dB from
/// 0.1645 fs.
pub(super) const ELLIPTIC_X3: Table = Table {
    b: &[
        5.171845997759806e-05,
        -0.00011325250969866734,
        0.0003582430632405222,
        -0.0004475460165689019,
        0.0008048304271222489,
        -0.0005942235870540368,
        0.0008380892745252189,
        -0.00021829435502546344,
        0.0003848870632043237,
        0.00038488706320430755,
        -0.00021829435502546235,
        0.0008380892745252296,
        -0.000594223587054041,
        0.000804830427122252,
        -0.00044754601656890223,
        0.00035824306324052144,
        -0.0001132525096986672,
        5.171845997759805e-05,
    ],
    a: &[
        1.0,
        -10.005526029198002,
        50.414110334898254,
        -167.8923861771644,
        411.0867439389719,
        -781.7342474685204,
        1191.6018904839336,
        -1483.5748269833698,
        1524.651838534694,
        -1299.046662000289,
        916.8054094048362,
        -532.6047495149238,
        251.50287145764287,
        -94.51436782686035,
        27.306604622475245,
        -5.719309511643069,
        0.7762172245710627,
        -0.05148158641478107,
    ],
};

/// Inverse-Chebyshev-type x2 stage, order 30: monotone passband (-0.0005
/// dB at 0.22 fs), -3 dB at 0.2321 fs, -100 dB from 0.2492 fs.
pub(super) const INV_CHEBYSHEV_X2: Table = Table {
    b: &[
        0.0005919380699460736,
        0.007355663299698523,
        0.049380064134626644,
        0.23327333127756036,
        0.8606330346264864,
        2.6198033448875404,
        6.803718710752869,
        15.41272707142567,
        30.931664812929963,
        55.615429221389945,
        90.33588521259023,
        133.37748245088045,
        179.8236772881152,
        222.11703264144577,
        251.91369276973023,
        262.67227603500197,
        251.91369276973285,
        222.11703264144924,
        179.8236772881159,
        133.3774824508793,
        90.3358852125891,
        55.61542922138965,
        30.93166481293015,
        15.412727071425765,
        6.8037187107528645,
        2.61980334488754,
        0.8606330346264873,
        0.23327333127756045,
        0.04938006413462666,
        0.00735566329969852,
        0.0005919380699460738,
    ],
    a: &[
        1.0,
        3.2022831357131945,
        10.986334414710466,
        24.34825875678301,
        50.10179310421837,
        84.80299940416889,
        130.83838593355802,
        178.3847574062449,
        222.40533378691606,
        251.8702691683538,
        262.3388465491364,
        251.15129509135386,
        222.02594398780803,
        181.284000692431,
        136.88225534771007,
        95.53144976437576,
        61.58012811832006,
        36.59839245069676,
        20.005231913321712,
        10.022606912392753,
        4.58144806132984,
        1.8995041581473746,
        0.708875900103176,
        0.2357547624019189,
        0.0689570821995877,
        0.017423937776245062,
        0.0037085554798632733,
        0.0006403907630963254,
        8.442317179574361e-05,
        7.586144111479901e-06,
        3.503906786666202e-07,
    ],
};

/// Inverse-Chebyshev-type x3 stage, order 24: -3 dB at 0.1581 fs, -100 dB
/// from 0.1810 fs (only -26.7 dB at the stage input Nyquist).
pub(super) const INV_CHEBYSHEV_X3: Table = Table {
    b: &[
        7.736500214138755e-05,
        0.00015429126393544615,
        0.0006467343861767652,
        0.0012505420869052765,
        0.0028318693459531198,
        0.004852706425165653,
        0.008159175642963794,
        0.01204805309347881,
        0.0167113195512376,
        0.021292109381655336,
        0.025329070532250475,
        0.028050892798201395,
        0.02902871331171334,
        0.0280508927982054,
        0.025329070532250406,
        0.021292109381655298,
        0.016711319551238064,
        0.012048053093478643,
        0.008159175642963428,
        0.004852706425165621,
        0.0028318693459531657,
        0.001250542086905274,
        0.0006467343861767642,
        0.0001542912639354462,
        7.736500214138749e-05,
    ],
    a: &[
        1.0,
        -6.193266341325925,
        21.78737975890803,
        -53.20774199768172,
        99.84789920982725,
        -150.62467795304332,
        188.51866478862905,
        -199.25604536332273,
        180.31623552657922,
        -140.78992733153135,
        95.44026773531584,
        -56.30267893354008,
        28.960036350755573,
        -12.965193132670022,
        5.045556368834901,
        -1.6960423361733594,
        0.4904550693472691,
        -0.12031082483036745,
        0.024902883627106288,
        -0.004205243994758577,
        0.0005829997302054712,
        -5.9122927816547916e-05,
        5.018737140214244e-06,
        -1.68389150725471e-07,
        1.147085857686162e-08,
    ],
};

pub(super) fn table(p: FilterPreset, f: usize) -> &'static Table {
    match (p, f) {
        (FilterPreset::CascadeElliptic, 2) => &ELLIPTIC_X2,
        (FilterPreset::CascadeElliptic, 3) => &ELLIPTIC_X3,
        (FilterPreset::CascadeInverseChebyshev, 2) => &INV_CHEBYSHEV_X2,
        (FilterPreset::CascadeInverseChebyshev, 3) => &INV_CHEBYSHEV_X3,
        _ => unreachable!("no recursive table for {} x{f}", p.name()),
    }
}

/// A channel's state is flushed to exact zero at the end of a `process`
/// call when every value in it is below this (about -600 dBFS). Without
/// it a decaying tail would run into subnormal numbers, which are very
/// slow on x86. The effect on the output is far below the last bit of
/// any 32-bit sample.
const FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31; // 2^-100

/// Response level, relative to the peak, below which the impulse response
/// counts as over ([`UpsamplerRuntime::impulse_span_out_frames`]).
const SPAN_LEVEL: f64 = 8.673_617_379_884_035e-19; // 2^-60

/// One stage of one channel: the stage's zero-stuffed input and its
/// output, each with `order` history samples in front.
struct StageState {
    x: Vec<f64>,
    y: Vec<f64>,
}

#[derive(Clone)]
struct Stage {
    f: usize,
    table: &'static Table,
    /// Product of the factors before this stage.
    in_mult: usize,
}

/// The recursive cascade runtime. No block buffering: latency is the
/// impulse response's peak.
pub struct IirCascade {
    ratio: usize,
    channels: usize,
    max_block: usize,
    stages: Vec<Stage>,
    /// `[channel][stage]`.
    state: Vec<Vec<StageState>>,
    /// The feed-forward sums B of one stage, for two channels.
    bsum: [Vec<f64>; 2],
    gain: f64,
    use_fma: bool,
    latency: usize,
    span: usize,
}

impl IirCascade {
    pub fn new(p: FilterPreset, ratio: usize, channels: usize, max_block: usize) -> Self {
        let mut up = Self::bare(p, ratio, channels, max_block);
        let (latency, span) = Self::measure(p, ratio);
        up.latency = latency;
        up.span = span;
        up
    }

    fn bare(p: FilterPreset, ratio: usize, channels: usize, max_block: usize) -> Self {
        let mut stages = Vec::new();
        let mut mult = 1;
        for f in stage_factors(ratio) {
            let t = table(p, f);
            crate::test_support::record("iir_stage", || {
                let mut w = crate::test_support::words(&[f as u64, t.b.len() as u64], t.b);
                w.extend(crate::test_support::words(&[t.a.len() as u64], t.a));
                w
            });
            stages.push(Stage {
                f,
                table: t,
                in_mult: mult,
            });
            mult *= f;
        }
        let state = (0..channels)
            .map(|_| {
                stages
                    .iter()
                    .map(|s| {
                        let n = s.table.order();
                        let len = n + max_block * s.in_mult * s.f;
                        StageState {
                            x: vec![0.0; len],
                            y: vec![0.0; len],
                        }
                    })
                    .collect()
            })
            .collect();
        let max_len = max_block * ratio;
        Self {
            ratio,
            channels,
            max_block,
            stages,
            state,
            bsum: [vec![0.0; max_len], vec![0.0; max_len]],
            gain: 1.0,
            use_fma: fma_available(),
            latency: 0,
            span: 0,
        }
    }

    /// Run an impulse through a one-channel copy: the position of the peak,
    /// and where the response has stayed below 2^-60 of it for good (the
    /// run stops once 2^15 output frames in a row are below the level).
    fn measure(p: FilterPreset, ratio: usize) -> (usize, usize) {
        const BLOCK: usize = 256;
        const QUIET: usize = 1 << 15;
        const MAX_OUT: usize = 1 << 24;
        let mut sim = Self::bare(p, ratio, 1, BLOCK);
        let mut inp = vec![0.0; BLOCK];
        inp[0] = 1.0;
        let mut out = vec![0.0; BLOCK * ratio];
        let mut resp: Vec<f64> = Vec::new();
        let mut peak = 0.0f64;
        let mut last_loud = 0usize;
        while resp.len() < MAX_OUT {
            sim.process(&[&inp], &mut [&mut out]);
            inp[0] = 0.0;
            for &v in &out {
                peak = peak.max(v.abs());
                if v.abs() >= peak * SPAN_LEVEL {
                    last_loud = resp.len();
                }
                resp.push(v);
            }
            if resp.len() - last_loud > QUIET {
                break;
            }
        }
        let peak_at = resp
            .iter()
            .enumerate()
            .fold((0, 0.0f64), |(bi, bv), (i, &v)| {
                if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
            })
            .0;
        (peak_at, last_loud + 1)
    }

    /// Run one stage of one or two channels over `len` new output samples.
    fn run(
        table: &'static Table,
        use_fma: bool,
        bsum: &mut [Vec<f64>; 2],
        a: &mut StageState,
        b: Option<&mut StageState>,
        len: usize,
    ) {
        macro_rules! go {
            ($n:literal, $f:literal) => {{
                #[cfg(target_arch = "x86_64")]
                if use_fma {
                    // SAFETY: the CPU supports FMA (checked at construction).
                    unsafe { run_fma::<$n, $f>(table, bsum, a, b, len) };
                    return;
                }
                let _ = use_fma;
                run_generic::<$n, $f>(table, bsum, a, b, len);
            }};
        }
        // Each table belongs to one stage factor.
        match table.order() {
            16 => go!(16, 2),
            30 => go!(30, 2),
            17 => go!(17, 3),
            24 => go!(24, 3),
            n => unreachable!("order {n}"),
        }
    }
}

fn fma_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "fma")]
fn run_fma<const N: usize, const F: usize>(
    t: &'static Table,
    bsum: &mut [Vec<f64>; 2],
    a: &mut StageState,
    b: Option<&mut StageState>,
    len: usize,
) {
    run_generic::<N, F>(t, bsum, a, b, len);
}

/// The feed-forward sum B (steps 1 to 3 on the x side) depends on the
/// input only and joins A only at the end, so it runs as its own pass;
/// the recursive pass then carries A alone. Same operations, same order
/// per sample. Two channels per recursive pass keep two independent
/// dependency chains in flight. `F` is the stage factor (the input is
/// zero-stuffed by it).
#[inline(always)]
fn run_generic<const N: usize, const F: usize>(
    t: &'static Table,
    bsum: &mut [Vec<f64>; 2],
    a: &mut StageState,
    b: Option<&mut StageState>,
    len: usize,
) {
    let bc = &t.b[..=N];
    let ac = &t.a[..=N];
    let [ba, bb] = bsum;
    feed_forward::<N, F>(bc, &a.x[..N + len], &mut ba[..len]);
    match b {
        Some(b) => {
            feed_forward::<N, F>(bc, &b.x[..N + len], &mut bb[..len]);
            let (ya, yb) = (&mut a.y[..N + len], &mut b.y[..N + len]);
            let (ba, bb) = (&ba[..len], &bb[..len]);
            // y[t-1] stays in a register (no store-to-load round trip on
            // the dependency chain).
            let (mut pa, mut pb) = (ya[N - 1], yb[N - 1]);
            for i in 0..len {
                pa = ba[i] - feedback::<N>(ac, &ya[i..i + N], pa);
                pb = bb[i] - feedback::<N>(ac, &yb[i..i + N], pb);
                ya[i + N] = pa;
                yb[i + N] = pb;
            }
        }
        None => {
            let ya = &mut a.y[..N + len];
            let ba = &ba[..len];
            let mut pa = ya[N - 1];
            for i in 0..len {
                pa = ba[i] - feedback::<N>(ac, &ya[i..i + N], pa);
                ya[i + N] = pa;
            }
        }
    }
}

/// B for each of `out.len()` samples; `x[i + N - k]` is x[t-k] for sample
/// i. The input is zero-stuffed by `F` with its samples at the indices
/// that are multiples of `F`, so for output phase `ph` (i = ph mod F) only
/// the taps k = ph mod F see a non-zero sample. The others are skipped:
/// fma(0, b, s) is s, so the sum is the same (up to the sign of a zero).
#[inline(always)]
fn feed_forward<const N: usize, const F: usize>(b: &[f64], x: &[f64], out: &mut [f64]) {
    feed_forward_phase::<N, F, 0>(b, x, out);
    feed_forward_phase::<N, F, 1>(b, x, out);
    if F == 3 {
        feed_forward_phase::<N, F, 2>(b, x, out);
    }
}

#[inline(always)]
fn feed_forward_phase<const N: usize, const F: usize, const PH: usize>(
    b: &[f64],
    x: &[f64],
    out: &mut [f64],
) {
    let b = &b[..=N];
    let mut i = PH;
    while i < out.len() {
        let x = &x[i..=i + N];
        let mut s = if PH == 0 { x[N] * b[0] } else { 0.0 };
        if N >= 16 {
            let mut p = [[0.0f64; 4]; 4];
            p[0][0] = s;
            for m in 0..N / 16 {
                for (g, row) in p.iter_mut().enumerate() {
                    for (j, partial) in row.iter_mut().enumerate() {
                        let k = 1 + 16 * m + 4 * g + j;
                        if k % F == PH {
                            *partial = x[N - k].mul_add(b[k], *partial);
                        }
                    }
                }
            }
            s = reduce(&p);
        }
        for k in 16 * (N / 16) + 1..=N {
            if k % F == PH {
                s = x[N - k].mul_add(b[k], s);
            }
        }
        out[i] = s;
        i += F;
    }
}

/// A for one sample; `y[N - k]` is y[t-k] (k = 2..=N), `y1` is y[t-1].
/// The partial sums start at zero, so their first tap is a plain product
/// (fma(u, v, 0) is u v, up to the sign of a zero).
#[inline(always)]
fn feedback<const N: usize>(a: &[f64], y: &[f64], y1: f64) -> f64 {
    let (a, y) = (&a[..=N], &y[..N]);
    let at = |k: usize| if k == 1 { y1 } else { y[N - k] };
    let mut s = 0.0;
    if N >= 16 {
        let mut p = [[0.0f64; 4]; 4];
        for m in 0..N / 16 {
            for (g, row) in p.iter_mut().enumerate() {
                for (j, partial) in row.iter_mut().enumerate() {
                    let k = 1 + 16 * m + 4 * g + j;
                    *partial = if m == 0 {
                        at(k) * a[k]
                    } else {
                        at(k).mul_add(a[k], *partial)
                    };
                }
            }
        }
        s = reduce(&p);
    }
    for (k, &ak) in a.iter().enumerate().skip(16 * (N / 16) + 1) {
        s = at(k).mul_add(ak, s);
    }
    s
}

#[inline(always)]
fn reduce(p: &[[f64; 4]; 4]) -> f64 {
    let s = |j: usize| (p[3][j] + p[2][j]) + (p[1][j] + p[0][j]);
    (s(0) + s(2)) + (s(1) + s(3))
}

impl UpsamplerRuntime for IirCascade {
    fn ratio(&self) -> RateRatio {
        RateRatio::integer(self.ratio as u32)
    }

    fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        assert_eq!(input.len(), self.channels, "input channel count mismatch");
        assert_eq!(output.len(), self.channels, "output channel count mismatch");
        if self.channels == 0 {
            return;
        }
        let n_in = input[0].len();
        assert!(
            n_in <= self.max_block,
            "input frame count {n_in} exceeds max_block {}",
            self.max_block
        );
        for c in 0..self.channels {
            assert_eq!(input[c].len(), n_in, "inconsistent input slice length");
            assert_eq!(output[c].len(), n_in * self.ratio, "output length");
        }
        if n_in == 0 {
            return;
        }
        let mut state = std::mem::take(&mut self.state);
        for (s, stage) in self.stages.iter().enumerate() {
            let n = stage.table.order();
            let f = stage.f;
            let len_in = n_in * stage.in_mult;
            let len = len_in * f;
            // Zero-stuff the stage input (the previous stage's output times
            // its factor, or the block itself) after the history.
            for (c, ch) in state.iter_mut().enumerate() {
                let (prev, rest) = ch.split_at_mut(s);
                let cur = &mut rest[0];
                let x = &mut cur.x[n..n + len];
                x.fill(0.0);
                if s == 0 {
                    for (i, &v) in input[c].iter().enumerate() {
                        x[i * f] = v;
                    }
                } else {
                    let p = &prev[s - 1];
                    let pf = self.stages[s - 1].f as f64;
                    let pn = self.stages[s - 1].table.order();
                    for (i, &v) in p.y[pn..pn + len_in].iter().enumerate() {
                        x[i * f] = v * pf;
                    }
                }
            }
            let mut c = 0;
            while c < self.channels {
                if c + 1 < self.channels {
                    let (lo, hi) = state.split_at_mut(c + 1);
                    Self::run(
                        stage.table,
                        self.use_fma,
                        &mut self.bsum,
                        &mut lo[c][s],
                        Some(&mut hi[0][s]),
                        len,
                    );
                    c += 2;
                } else {
                    Self::run(
                        stage.table,
                        self.use_fma,
                        &mut self.bsum,
                        &mut state[c][s],
                        None,
                        len,
                    );
                    c += 1;
                }
            }
        }
        // Output: the last stage times its factor (and the headroom gain).
        let last = self.stages.len() - 1;
        let lf = self.stages[last].f as f64;
        let ln = self.stages[last].table.order();
        let n_out = n_in * self.ratio;
        for (c, ch) in state.iter().enumerate() {
            let y = &ch[last].y[ln..ln + n_out];
            if self.gain == 1.0 {
                for (o, &v) in output[c].iter_mut().zip(y) {
                    *o = v * lf;
                }
            } else {
                for (o, &v) in output[c].iter_mut().zip(y) {
                    *o = v * lf * self.gain;
                }
            }
        }
        // Keep the last `order` samples as history; flush tiny states.
        for ch in &mut state {
            for (st, stage) in ch.iter_mut().zip(&self.stages) {
                let n = stage.table.order();
                let len = n_in * stage.in_mult * stage.f;
                st.x.copy_within(len..len + n, 0);
                st.y.copy_within(len..len + n, 0);
                let tiny = st.x[..n]
                    .iter()
                    .chain(&st.y[..n])
                    .all(|v| v.abs() < FLUSH_BELOW);
                if tiny {
                    st.x[..n].fill(0.0);
                    st.y[..n].fill(0.0);
                }
            }
        }
        self.state = state;
    }

    fn reset(&mut self) {
        for ch in &mut self.state {
            for st in ch {
                st.x.fill(0.0);
                st.y.fill(0.0);
            }
        }
    }

    fn latency_out_frames(&self) -> usize {
        self.latency
    }

    fn scale_output(&mut self, gain: f64) {
        self.gain = gain;
    }

    fn impulse_span_out_frames(&self) -> usize {
        self.span
    }

    // The two-channel recursive pass runs each channel's operations in the
    // one-channel order (`run_generic`), so a part is bit for bit its
    // channel here.
    fn split_channels(&mut self) -> Option<Vec<Box<dyn UpsamplerRuntime>>> {
        let state = std::mem::take(&mut self.state);
        self.channels = 0;
        Some(
            state
                .into_iter()
                .map(|st| {
                    Box::new(IirCascade {
                        ratio: self.ratio,
                        channels: 1,
                        max_block: self.max_block,
                        stages: self.stages.clone(),
                        state: vec![st],
                        bsum: self.bsum.clone(),
                        gain: self.gain,
                        use_fma: self.use_fma,
                        latency: self.latency,
                        span: self.span,
                    }) as Box<dyn UpsamplerRuntime>
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Literal direct form: include zero taps and fuse every partial sum.
    // This deliberately does not use the runtime's phase pruning,
    // separate passes, cached previous output, or table specializations.
    fn direct(t: &Table, x: &[f64]) -> Vec<f64> {
        let n = t.order();
        let mut y = vec![0.0f64; x.len()];
        for i in 0..x.len() {
            let mut b = x[i] * t.b[0];
            let mut a = 0.0;
            let mut pb = [[0.0; 4]; 4];
            let mut pa = [[0.0; 4]; 4];
            pb[0][0] = b;
            for m in 0..n / 16 {
                for g in 0..4 {
                    for j in 0..4 {
                        let k = 1 + 16 * m + 4 * g + j;
                        let (xi, yi) = if k <= i {
                            (x[i - k], y[i - k])
                        } else {
                            (0.0, 0.0)
                        };
                        pb[g][j] = xi.mul_add(t.b[k], pb[g][j]);
                        pa[g][j] = yi.mul_add(t.a[k], pa[g][j]);
                    }
                }
            }
            if n >= 16 {
                let sum = |p: [[f64; 4]; 4]| {
                    let s: [f64; 4] =
                        std::array::from_fn(|j| (p[3][j] + p[2][j]) + (p[1][j] + p[0][j]));
                    (s[0] + s[2]) + (s[1] + s[3])
                };
                b = sum(pb);
                a = sum(pa);
            }
            for k in 16 * (n / 16) + 1..=n {
                let (xi, yi) = if k <= i {
                    (x[i - k], y[i - k])
                } else {
                    (0.0, 0.0)
                };
                b = xi.mul_add(t.b[k], b);
                a = yi.mul_add(t.a[k], a);
            }
            y[i] = (b - a) * t.a[0];
        }
        y
    }

    #[test]
    fn optimized_and_fallback_paths_match_literal_fma_order() {
        for p in [
            FilterPreset::CascadeElliptic,
            FilterPreset::CascadeInverseChebyshev,
        ] {
            for ratio in [2, 6, 18, 32] {
                let frames = 1025;
                let mut seed = 17u64;
                let inputs: Vec<Vec<f64>> = (0..3)
                    .map(|_| {
                        (0..frames)
                            .map(|_| {
                                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                                (seed >> 11) as f64 / (1u64 << 53) as f64 - 0.5
                            })
                            .collect()
                    })
                    .collect();
                let expected: Vec<Vec<f64>> = inputs
                    .iter()
                    .map(|x| {
                        let mut y = x.clone();
                        for f in stage_factors(ratio) {
                            let mut u = vec![0.0; y.len() * f];
                            for (i, &v) in y.iter().enumerate() {
                                u[i * f] = v;
                            }
                            y = direct(table(p, f), &u);
                            for v in &mut y {
                                *v *= f as f64;
                            }
                        }
                        y
                    })
                    .collect();
                for hardware_fma in [false, fma_available()] {
                    let mut rt = IirCascade::bare(p, ratio, 3, 127);
                    rt.use_fma = hardware_fma;
                    rt.scale_output(1.0);
                    let mut actual = vec![vec![0.0; frames * ratio]; 3];
                    for start in (0..frames).step_by(127) {
                        let end = (start + 127).min(frames);
                        let src: Vec<&[f64]> = inputs.iter().map(|x| &x[start..end]).collect();
                        let mut dst: Vec<&mut [f64]> = actual
                            .iter_mut()
                            .map(|y| &mut y[start * ratio..end * ratio])
                            .collect();
                        rt.process(&src, &mut dst);
                    }
                    assert_eq!(
                        actual,
                        expected,
                        "{} x{ratio}, fma={hardware_fma}",
                        p.name()
                    );
                }
            }
        }
    }
}
