//! Property test of the design type: valid designs generated on purpose,
//! kind by kind, run through `check` → `feasibility` → `prepare` → `build`
//! without a panic; what
//! builds runs to steady state and keeps its runtime contract. A separate
//! set of invalid and boundary designs must be refused with the expected
//! typed error.
//!
//! The generator is a seeded xorshift (no new dependency). The default
//! seed is fixed, so the default run is reproducible; `AVDSP_DESIGN_SEED`
//! replays (or varies) it, and the seed is printed with every failure.
//! The default run caps kernels at 262 144 grid taps (65 536 for a phase
//! transform) and takes a few seconds; `--ignored` runs the full limits.

use audivane_dsp_core::design::{
    self, Accept, ArithmeticPath, BandSpec, BlockFftDesign, BlockSize, BuildContext, CascadeStage,
    Cutoff, Decimation, DesignError, Fallback, FallbackWhen, FilterDesign, FilterErrorBudget,
    FlattopSet, GridCutoff, InterpMethod, KernelDesign, Length, Numerics, OddRule, Overlap, Phase,
    PhaseMethod, Rates, Sampling, Shape, Span, Stepped, Window, budget, check,
};
use audivane_dsp_core::presets::UpsamplerRuntime;
use audivane_protocol::{OutputConfig, RateRatio};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// ------------------------------------------------------------ no_alloc

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Heap operations while running `f` on this thread.
fn heap_ops<R>(f: impl FnOnce() -> R) -> (R, usize) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let r = f();
    ARMED.with(|a| a.set(false));
    (r, COUNT.with(Cell::get))
}

// ----------------------------------------------------------- generator

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len())]
    }
    /// Log-uniform integer in `lo..=hi`.
    fn log(&mut self, lo: usize, hi: usize) -> usize {
        let (a, b) = ((lo.max(1) as f64).ln(), (hi.max(lo).max(1) as f64).ln());
        (self.range(a, b).exp().round() as usize).clamp(lo, hi.max(lo))
    }
}

const DEFAULT_SEED: u64 = 0x5eed_0004_d351_6e00;

fn seed() -> u64 {
    std::env::var("AVDSP_DESIGN_SEED")
        .ok()
        .and_then(|s| {
            let s = s.trim();
            s.strip_prefix("0x")
                .map_or_else(|| s.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
        })
        .unwrap_or(DEFAULT_SEED)
}

/// The ratios drawn, each with its source rates.
const RATIOS: &[(u32, u32, &[u32])] = &[
    (2, 1, &[44_100, 48_000, 96_000]),
    (3, 1, &[44_100, 48_000]),
    (4, 1, &[44_100, 48_000, 96_000]),
    (6, 1, &[44_100, 48_000]),
    (8, 1, &[44_100, 48_000]),
    (64, 1, &[44_100, 48_000]),
    (160, 147, &[44_100, 88_200]),
    (147, 160, &[48_000, 96_000]),
    (1, 2, &[88_200, 96_000, 192_000]),
    (2560, 147, &[44_100]),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    WindowedSinc,
    Lanczos,
    Cascade,
    Interpolator,
    BlockFft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Method {
    Linear,
    Reflect,
    Cepstral,
}

struct Case {
    design: FilterDesign,
    source_rate: u32,
    ratio: RateRatio,
    kind: Kind,
    method: Option<Method>,
    path: Option<ArithmeticPath>,
}

const PATHS: [ArithmeticPath; 7] = [
    ArithmeticPath::Fir,
    ArithmeticPath::FirAfter,
    ArithmeticPath::ExactPiInverseAfter,
    ArithmeticPath::ExactOverRatioBefore,
    ArithmeticPath::ExactPiInversePairwise,
    ArithmeticPath::DoubleDouble,
    ArithmeticPath::Standard,
];

fn sampling(rng: &mut Rng, symmetric_only: bool) -> Sampling {
    if symmetric_only || rng.chance(0.5) {
        Sampling::Symmetric
    } else {
        Sampling::Periodic
    }
}

/// A window the path builds; `reflect` keeps it symmetric and modelled.
fn window(
    rng: &mut Rng,
    path: ArithmeticPath,
    reflect: bool,
    linear: bool,
    modelled: bool,
) -> Window {
    use ArithmeticPath::*;
    match path {
        DoubleDouble => Window::Gaussian {
            alpha: rng.range(0.1, 0.3),
            sampling: Sampling::IntegerCentre,
        },
        Standard => match rng.below(if linear && !modelled { 3 } else { 2 }) {
            0 => Window::Kaiser {
                beta: Some(rng.range(
                    if reflect { 4.0 } else { 5.0 },
                    if reflect { 20.0 } else { 24.0 },
                )),
                sampling: sampling(rng, false),
            },
            1 => Window::GaussianStopband {
                beta: Some(rng.range(6.0, 20.0)),
                sampling: Sampling::Symmetric,
            },
            _ => Window::Tukey {
                taper: rng.range(0.05, 1.0),
                sampling: sampling(rng, false),
            },
        },
        _ => {
            let s = sampling(rng, reflect);
            match rng.below(if linear && !modelled { 7 } else { 6 }) {
                0 => Window::Kaiser {
                    beta: Some(rng.range(
                        if reflect { 4.0 } else { 3.0 },
                        if reflect { 20.0 } else { 30.0 },
                    )),
                    sampling: s,
                },
                1 => Window::Gaussian {
                    alpha: rng.range(0.1, 0.3),
                    sampling: if !reflect && rng.chance(0.3) {
                        Sampling::IntegerCentre
                    } else {
                        s
                    },
                },
                2 => Window::Flattop {
                    set: rng.pick(&[
                        FlattopSet::Hft196d,
                        FlattopSet::Hft223d,
                        FlattopSet::Hft248d,
                    ]),
                    sampling: s,
                },
                3 => Window::Chebyshev {
                    sidelobe_ratio: 10f64
                        .powf(-rng.range(60.0, if reflect { 190.0 } else { 220.0 }) / 20.0),
                    sampling: s,
                },
                4 => Window::Nuttall { sampling: s },
                5 => Window::Hann { sampling: s },
                _ => Window::Tukey {
                    taper: rng.range(0.05, 1.0),
                    sampling: s,
                },
            }
        }
    }
}

/// The window's nominal stopband (a budget the design can promise).
fn nominal(w: &Window) -> f64 {
    design::window_model::nominal_db(w).unwrap_or(120.0)
}

fn band(rng: &mut Rng, a: f64) -> BandSpec {
    let pe = rng.range(0.7, 0.95);
    BandSpec {
        passband_end: pe,
        stopband_start: (pe + rng.range(0.05, 0.3)).min(1.5),
        attenuation_db: a,
    }
}

/// A kernel design valid by construction.
fn kernel(rng: &mut Rng, lanczos: bool, ratio: RateRatio, cap: usize) -> (KernelDesign, Method) {
    use ArithmeticPath::*;
    let path = if lanczos {
        rng.pick(&[Fir, FirAfter])
    } else {
        rng.pick(&PATHS)
    };
    let standard = path == Standard;
    let method = match (path, rng.below(3)) {
        (Fir | FirAfter | Standard, 0) => Method::Reflect,
        (_, 1) => Method::Cepstral,
        _ => Method::Linear,
    };
    let phase = |rng: &mut Rng, m: Method| -> Phase {
        let pm = match m {
            Method::Reflect => PhaseMethod::Reflect,
            _ => PhaseMethod::Cepstral,
        };
        match m {
            Method::Linear => Phase::Linear,
            _ if rng.chance(0.5) => Phase::Minimum(pm),
            _ => {
                let free = rng.range(0.05, 0.95);
                Phase::Intermediate {
                    mix: rng.pick(&[0.0, 0.25, 0.5, 0.75, 0.9, 1.0, free]),
                    method: pm,
                }
            }
        }
    };
    let phase = phase(rng, method);
    let linear = method == Method::Linear;
    let l = ratio.up as usize;
    let r = ratio.up.max(ratio.down) as usize;
    let mut rates = Rates {
        accept: Accept::Any,
        fallback_window: None,
    };
    let grid_cutoff = if rng.chance(0.3) {
        if standard {
            GridCutoff::Scaled
        } else {
            GridCutoff::DesignRatio
        }
    } else if standard {
        GridCutoff::DesignRatio
    } else {
        GridCutoff::Scaled
    };
    let decimation = if standard || grid_cutoff == GridCutoff::DesignRatio || rng.chance(0.3) {
        Decimation::Keep
    } else {
        Decimation::Scale {
            odd: rng.pick(&[OddRule::Down, OddRule::Up]),
            base_taps: rng.chance(0.2).then(|| rng.log(8, 255)),
        }
    };
    let b = if grid_cutoff == GridCutoff::DesignRatio {
        r
    } else {
        l
    };
    // Taps per input sample within the cap.
    let max_t = (cap / b).max(1);
    if lanczos {
        rates.accept = Accept::Upsampling;
        let lobes = rng.log(1, (max_t.saturating_sub(1) / 2).clamp(1, 200));
        let k = KernelDesign {
            shape: Shape::Lanczos { lobes },
            decimation,
            phase,
            preserve_samples: false,
            rates,
            numerics: Numerics { path, grid_cutoff },
        };
        return (k, method);
    }
    let rule = rng.below(6);
    let modelled = rule >= 3 || rule == 2 && !standard;
    let w = window(
        rng,
        path,
        method == Method::Reflect,
        linear,
        modelled || method != Method::Linear,
    );
    let a_max = nominal(&w).min(if method == Method::Reflect {
        185.0
    } else {
        220.0
    });
    let fraction = |rng: &mut Rng| Cutoff::Fraction(Stepped::Fixed(rng.range(0.6, 1.0)));
    let fixed = |rng: &mut Rng| {
        let n = rng.log(
            16,
            cap.min(budget::MAX_KERNEL_TAPS) / ratio.down.max(1) as usize,
        );
        Length::Fixed(n.max(2))
    };
    let per_input = |rng: &mut Rng| Length::PerInput(Stepped::Fixed(rng.log(4, max_t.min(4095))));
    let (length, cutoff, window) = match rule {
        0 => (fixed(rng), fraction(rng), w),
        1 if !standard && rng.chance(0.3) => {
            rates.accept = Accept::Upsampling;
            (
                Length::PerInput(Stepped::BySourceRate(vec![
                    (Some(50_000), rng.log(4, max_t.min(4095))),
                    (None, rng.log(4, max_t.min(4095))),
                ])),
                Cutoff::Fraction(Stepped::BySourceRate(vec![
                    (Some(50_000), rng.range(0.7, 1.0)),
                    (None, rng.range(0.6, 1.0)),
                ])),
                w,
            )
        }
        1 => (per_input(rng), fraction(rng), w),
        2 if !standard && rng.chance(0.3) && ratio.is_integer() => {
            rates.accept = Accept::Integer;
            let n = 1usize << rng.below(5).min((cap.ilog2() as usize).saturating_sub(12)) << 12;
            (
                Length::OutputSpan(Span::Taps(n.clamp(4096, cap.max(4096)))),
                fraction(rng),
                w,
            )
        }
        2 => {
            // Stop at Nyquist: a modelled window (on the standard path a
            // Kaiser or Gaussian beta in its calibrated range).
            let w = match w {
                Window::Tukey { .. } => Window::Hann {
                    sampling: Sampling::Symmetric,
                },
                Window::Kaiser {
                    beta: Some(b),
                    sampling,
                } if standard => Window::Kaiser {
                    beta: Some(b.clamp(5.0, 24.0)),
                    sampling,
                },
                w => w,
            };
            let w = if standard && matches!(w, Window::Hann { .. }) {
                Window::Kaiser {
                    beta: Some(12.0),
                    sampling: Sampling::Symmetric,
                }
            } else {
                w
            };
            (
                if rng.chance(0.5) {
                    fixed(rng)
                } else {
                    per_input(rng)
                },
                Cutoff::NyquistStop,
                w,
            )
        }
        3 | 4 => {
            let w = no_beta(w, standard, rng);
            let a = rng.range(55.0, a_max.clamp(56.0, 200.0));
            (Length::Band, Cutoff::Band(band(rng, a)), w)
        }
        _ => {
            let w = no_beta(w, standard, rng);
            let a = rng.range(
                55.0,
                a_max.max(56.0).min(if method == Method::Cepstral {
                    100.0
                } else {
                    160.0
                }),
            );
            let mut spec = band(rng, a);
            spec.attenuation_db = a.max(55.0);
            (
                Length::Auto(FilterErrorBudget::Db(a)),
                Cutoff::Band(spec),
                w,
            )
        }
    };
    let preserve = linear
        && rule <= 1
        && !matches!(window, Window::GaussianStopband { .. })
        && rng.chance(0.15);
    let cutoff = if preserve {
        rates.accept = Accept::Upsampling;
        Cutoff::Fraction(Stepped::Fixed(1.0))
    } else {
        cutoff
    };
    if !standard && path != DoubleDouble && rng.chance(0.15) {
        rates.fallback_window = Some(Fallback {
            when: rng.pick(&[FallbackWhen::NotMultiple, FallbackWhen::Decimating]),
            window: Window::Kaiser {
                beta: Some(rng.range(5.0, 20.0)),
                sampling: Sampling::Symmetric,
            },
        });
    }
    let k = KernelDesign {
        shape: Shape::WindowedSinc {
            window,
            length,
            cutoff,
        },
        decimation,
        phase,
        preserve_samples: preserve,
        rates,
        numerics: Numerics { path, grid_cutoff },
    };
    (k, method)
}

/// The window of a band or automatic design: a Kaiser window without its
/// beta half the time (the standard path's always), the standard path's
/// Gaussian without its stopband; a Tukey window (no model) replaced.
fn no_beta(w: Window, standard: bool, rng: &mut Rng) -> Window {
    match w {
        Window::Kaiser { sampling, .. } if standard || rng.chance(0.5) => Window::Kaiser {
            beta: None,
            sampling,
        },
        Window::GaussianStopband { sampling, .. } => Window::GaussianStopband {
            beta: None,
            sampling,
        },
        Window::Tukey { sampling, .. } if standard => Window::Kaiser {
            beta: None,
            sampling,
        },
        Window::Tukey { sampling, .. } => Window::Nuttall { sampling },
        w => w,
    }
}

fn runtime_kind(rng: &mut Rng, kind: Kind, cap: usize) -> (FilterDesign, RateRatio, u32) {
    let src = rng.pick(&[44_100, 48_000]);
    match kind {
        Kind::Cascade => (
            FilterDesign::Cascade(rng.pick(&[
                CascadeStage::Fir,
                CascadeStage::FirMinimum,
                CascadeStage::FirAsymmetric,
                CascadeStage::FrequencySampled,
                CascadeStage::Elliptic,
                CascadeStage::InverseChebyshev,
            ])),
            RateRatio::integer(rng.pick(&[2, 4, 6, 8])),
            src,
        ),
        Kind::Interpolator => (
            FilterDesign::Interpolator(rng.pick(&[
                InterpMethod::Cosine,
                InterpMethod::CatmullRom,
                InterpMethod::TableSinc,
            ])),
            RateRatio::integer(rng.pick(&[2, 3, 4, 6, 8, 64])),
            src,
        ),
        _ => {
            let ratio = rng.pick(&[2u32, 4, 8, 64]);
            // Input blocks to 2^16; output blocks resolving to the input
            // block bound (2^19, the full run; 2^16 by default) within
            // 2^22 output frames.
            let block = if rng.chance(0.5) {
                BlockSize::Input(1usize << (10 + rng.below(7)))
            } else {
                let top = if cap >= budget::MAX_KERNEL_TAPS {
                    check::BLOCK_OUTPUT_MAX_INPUT
                } else {
                    1 << 16
                };
                let top = top.min((1 << 22) / ratio as usize).ilog2() as usize;
                BlockSize::Output((1usize << (10 + rng.below(top - 9))) * ratio as usize)
            };
            (
                FilterDesign::BlockFft(BlockFftDesign {
                    block,
                    overlap: rng.pick(&[Overlap::Half, Overlap::QuarterAverage]),
                }),
                RateRatio::integer(ratio),
                src,
            )
        }
    }
}

fn cx(source_rate: u32, ratio: RateRatio, max_block: usize) -> BuildContext {
    BuildContext {
        source_rate,
        ratio,
        channels: 1,
        max_block,
        pipelines: budget::RATIONAL_SWAP_PIPELINES,
        output: OutputConfig::default(),
        workspace: budget::DESIGN_WORKSPACE_FLOOR,
    }
}

/// The longest grid kernel of `d` the run allows.
fn too_long(d: &FilterDesign, c: &BuildContext, cap: usize) -> bool {
    let Ok(plan) = design::feasibility(d, c) else {
        return false;
    };
    let FilterDesign::Kernel(k) = d else {
        return false;
    };
    let transformed = k.phase != Phase::Linear;
    let limit = if transformed { cap / 4 } else { cap };
    let growth = match k.phase {
        Phase::Intermediate { mix, .. } if mix > 0.0 && mix < 1.0 => 4,
        _ => 1,
    };
    plan.grid_taps > limit * growth
}

fn generate(seed: u64, count: usize, cap: usize) -> Vec<Case> {
    // splitmix64 of the seed: nearby seeds start far apart, never at 0.
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let mut rng = Rng((z ^ (z >> 31)) | 1);
    let kinds = [
        (Kind::WindowedSinc, 0.62),
        (Kind::Lanczos, 0.1),
        (Kind::Cascade, 0.09),
        (Kind::Interpolator, 0.09),
        (Kind::BlockFft, 0.1),
    ];
    let mut cases = Vec::with_capacity(count);
    while cases.len() < count {
        let mut u = rng.unit();
        let kind = kinds
            .iter()
            .find(|(_, p)| {
                u -= p;
                u < 0.0
            })
            .map_or(Kind::WindowedSinc, |k| k.0);
        match kind {
            Kind::WindowedSinc | Kind::Lanczos => {
                for _ in 0..64 {
                    let (up, down, sources) = rng.pick(RATIOS);
                    let ratio = RateRatio::new(up, down);
                    let src = rng.pick(sources);
                    let (k, method) = kernel(&mut rng, kind == Kind::Lanczos, ratio, cap);
                    let path = k.numerics.path;
                    let d = FilterDesign::Kernel(k);
                    if too_long(&d, &cx(src, ratio, 256), cap) {
                        continue;
                    }
                    cases.push(Case {
                        design: d,
                        source_rate: src,
                        ratio,
                        kind,
                        method: Some(method),
                        path: Some(path),
                    });
                    break;
                }
            }
            _ => {
                let (d, ratio, src) = runtime_kind(&mut rng, kind, cap);
                cases.push(Case {
                    design: d,
                    source_rate: src,
                    ratio,
                    kind,
                    method: None,
                    path: None,
                });
            }
        }
    }
    cases
}

// ------------------------------------------------------------- the run

/// What happened to a case.
#[derive(Debug)]
enum Outcome {
    Built,
    Refused(&'static str),
}

fn class(e: &DesignError) -> &'static str {
    match e {
        DesignError::Ratio { .. } => "ratio",
        DesignError::TooLong { .. } => "too_long",
        DesignError::Budget(_) => "budget",
        DesignError::Unsupported(_) => "unsupported",
        DesignError::NotAKernel => "not_a_kernel",
        DesignError::Failed(_) => "failed",
        DesignError::Band(_) => "band",
        DesignError::Phase(_) => "phase",
        DesignError::Unsafe(_) => "unsafe",
        DesignError::Field(_) => "field",
        DesignError::Claim(_) => "claim",
    }
}

fn noise(n: usize, seed: &mut u64) -> Vec<f64> {
    (0..n)
        .map(|_| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            ((*seed >> 11) as f64 / (1u64 << 53) as f64) * 1.0 - 0.5
        })
        .collect()
}

/// Run `input` through `rt` in blocks of `block`: the output, and the heap
/// operations of its `process` calls.
fn run(rt: &mut dyn UpsamplerRuntime, input: &[f64], block: usize) -> (Vec<f64>, usize) {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut ops = 0;
    for chunk in input.chunks(block) {
        let n = rt.output_frames(chunk.len());
        buf.clear();
        buf.resize(n, 0.0);
        let ((), o) = heap_ops(|| rt.process(&[chunk], &mut [buf.as_mut_slice()]));
        ops += o;
        out.extend_from_slice(&buf);
    }
    (out, ops)
}

fn same(a: &[f64], b: &[f64]) -> bool {
    let peak = a.iter().fold(1e-300f64, |m, v| m.max(v.abs()));
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-12 * peak)
}

const MAX_BLOCK: usize = 256;

/// The runtime contract of a built design; `Err` names the broken
/// property.
fn runtime_properties(
    d: &FilterDesign,
    c: &BuildContext,
    prepared: &design::Prepared,
) -> Result<(), String> {
    let build = || -> Result<Box<dyn UpsamplerRuntime>, String> {
        design::build(prepared, c, 0.0)
            .map_err(|e| format!("build: {e}"))?
            .ok_or_else(|| "build: no runtime".into())
    };
    let mut rt = build()?;
    let ratio = rt.ratio();
    let (l, m) = (ratio.up as usize, ratio.down as usize);
    let span = rt.latency_out_frames() + rt.impulse_span_out_frames();
    let frames = (span * m).div_ceil(l) + 4 * MAX_BLOCK;
    let mut s = 0x1234_5678_9abc_def1u64;
    let input = noise(frames, &mut s);

    // Steady state on noise: finite, no heap on the audio path.
    let (out, ops) = run(rt.as_mut(), &input, MAX_BLOCK);
    if ops != 0 {
        return Err(format!("process: {ops} heap operations"));
    }
    if !out.iter().all(|v| v.is_finite()) {
        return Err("non-finite output".into());
    }
    // `reset` then the same input: the same output.
    let ((), ops) = heap_ops(|| rt.reset());
    if ops != 0 {
        return Err(format!("reset: {ops} heap operations"));
    }
    let (again, _) = run(rt.as_mut(), &input, MAX_BLOCK);
    if !same(&out, &again) {
        return Err("reset then the same input gives another output".into());
    }
    // Block sizes 1, 7 and 256: the same output.
    for block in [1usize, 7] {
        let mut other = build()?;
        let (o, _) = run(other.as_mut(), &input, block);
        if !same(&out, &o) {
            return Err(format!("block size {block} gives another output"));
        }
    }
    // DC on a constant input, once settled.
    rt.reset();
    let dc = vec![0.5; frames];
    let (o, _) = run(rt.as_mut(), &dc, MAX_BLOCK);
    let tail = &o[o.len().saturating_sub(4 * l.max(1))..];
    let novel_or_preset = match d {
        FilterDesign::Kernel(k) => {
            k.numerics.path != ArithmeticPath::Standard
                || k.numerics.grid_cutoff == GridCutoff::Scaled
                || !matches!(
                    k.phase,
                    Phase::Linear
                        | Phase::Minimum(PhaseMethod::Reflect)
                        | Phase::Intermediate {
                            method: PhaseMethod::Reflect,
                            ..
                        }
                )
        }
        _ => true,
    };
    let worst = tail
        .iter()
        .map(|v| (20.0 * (v / 0.5).abs().log10()).abs())
        .fold(0.0, f64::max);
    if novel_or_preset && worst > 0.11 {
        return Err(format!("DC off by {worst:.3} dB"));
    }
    // A rational runtime reset to a position runs as one that got there.
    if !ratio.is_integer() {
        for pos in [1u64, m as u64 - 1, 12_345] {
            let mut a = build()?;
            a.reset_to(pos);
            let mut b = build()?;
            let zeros = vec![0.0; pos as usize];
            let _ = run(b.as_mut(), &zeros, MAX_BLOCK);
            let (oa, _) = run(a.as_mut(), &input, MAX_BLOCK);
            let (ob, _) = run(b.as_mut(), &input, MAX_BLOCK);
            if !same(&oa, &ob) {
                return Err(format!("reset_to({pos}) differs from running {pos} frames"));
            }
        }
    }
    Ok(())
}

struct Report {
    outcomes: BTreeMap<String, usize>,
    failures: Vec<String>,
}

fn run_cases(cases: Vec<Case>, seed: u64) -> Report {
    let report = Mutex::new(Report {
        outcomes: BTreeMap::new(),
        failures: Vec::new(),
    });
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(i) else { break };
                    let c = cx(case.source_rate, case.ratio, MAX_BLOCK);
                    let what = format!(
                        "case {i} (seed {seed:#x}) {:?} from {} at {}",
                        case.design, case.source_rate, case.ratio
                    );
                    let result = catch_unwind(AssertUnwindSafe(|| -> Result<Outcome, String> {
                        if let Err(e) = check::check(&case.design) {
                            return Err(format!("generated out of bounds: {e}"));
                        }
                        if let Err(e) = design::feasibility(&case.design, &c) {
                            return Ok(Outcome::Refused(class(&e)));
                        }
                        let p = match design::prepare(&case.design, &c) {
                            Ok(p) => p,
                            Err(e) => return Ok(Outcome::Refused(class(&e))),
                        };
                        runtime_properties(&case.design, &c, &p)?;
                        Ok(Outcome::Built)
                    }));
                    let mut r = report.lock().unwrap();
                    match result {
                        Ok(Ok(o)) => {
                            if std::env::var_os("AVDSP_DESIGN_VERBOSE").is_some()
                                && let Outcome::Refused(_) = o
                            {
                                let e = design::prepare(&case.design, &c).err();
                                println!("{what}: {e:?}");
                            }
                            let key = match o {
                                Outcome::Built => "built".to_string(),
                                Outcome::Refused(c) => format!("refused:{c}"),
                            };
                            *r.outcomes.entry(key).or_default() += 1;
                        }
                        Ok(Err(e)) => r.failures.push(format!("{what}: {e}")),
                        Err(p) => {
                            let msg = p
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_default();
                            r.failures.push(format!("{what}: panicked: {msg}"));
                        }
                    }
                }
            });
        }
    });
    report.into_inner().unwrap()
}

fn coverage(cases: &[Case]) -> Result<(), String> {
    let mut kinds: BTreeMap<Kind, usize> = BTreeMap::new();
    let mut methods: BTreeMap<Method, usize> = BTreeMap::new();
    let mut paths = Vec::new();
    for c in cases {
        *kinds.entry(c.kind).or_default() += 1;
        if let Some(m) = c.method {
            *methods.entry(m).or_default() += 1;
        }
        if let Some(p) = c.path
            && !paths.contains(&p)
        {
            paths.push(p);
        }
    }
    for k in [
        Kind::WindowedSinc,
        Kind::Lanczos,
        Kind::Cascade,
        Kind::Interpolator,
        Kind::BlockFft,
    ] {
        if kinds.get(&k).copied().unwrap_or(0) < 5 {
            return Err(format!("fewer than 5 {k:?} designs: {kinds:?}"));
        }
    }
    for m in [Method::Linear, Method::Reflect, Method::Cepstral] {
        if methods.get(&m).copied().unwrap_or(0) < 5 {
            return Err(format!("fewer than 5 {m:?} designs: {methods:?}"));
        }
    }
    for p in PATHS {
        if !paths.contains(&p) {
            return Err(format!("no design on the {p:?} path"));
        }
    }
    Ok(())
}

fn property_run(count: usize, cap: usize) {
    let seed = seed();
    let cases = generate(seed, count, cap);
    coverage(&cases).unwrap_or_else(|e| panic!("seed {seed:#x}: {e}"));
    let report = run_cases(cases, seed);
    println!("seed {seed:#x}: {:?}", report.outcomes);
    let built = report.outcomes.get("built").copied().unwrap_or(0);
    assert!(
        report.failures.is_empty(),
        "seed {seed:#x}: {} failures:\n{}",
        report.failures.len(),
        report.failures.join("\n")
    );
    assert!(
        built >= count / 2,
        "seed {seed:#x}: only {built} of {count} built: {:?}",
        report.outcomes
    );
}

#[test]
fn generated_designs_keep_the_contract() {
    property_run(300, 1 << 18);
}

#[test]
#[ignore]
fn generated_designs_at_the_full_limits() {
    property_run(300, budget::MAX_KERNEL_TAPS);
}

// --------------------------------------------- invalid and boundary set

fn base_kernel() -> KernelDesign {
    KernelDesign {
        shape: Shape::WindowedSinc {
            window: Window::Kaiser {
                beta: Some(12.0),
                sampling: Sampling::Periodic,
            },
            length: Length::PerInput(Stepped::Fixed(63)),
            cutoff: Cutoff::Fraction(Stepped::Fixed(0.9)),
        },
        decimation: Decimation::Scale {
            odd: OddRule::Down,
            base_taps: None,
        },
        phase: Phase::Linear,
        preserve_samples: false,
        rates: Rates {
            accept: Accept::Any,
            fallback_window: None,
        },
        numerics: Numerics {
            path: ArithmeticPath::Fir,
            grid_cutoff: GridCutoff::Scaled,
        },
    }
}

fn with_window(w: Window) -> KernelDesign {
    let mut k = base_kernel();
    if let Shape::WindowedSinc { window, .. } = &mut k.shape {
        *window = w;
    }
    k
}

fn with_length(l: Length) -> KernelDesign {
    let mut k = base_kernel();
    if let Shape::WindowedSinc { length, .. } = &mut k.shape {
        *length = l;
    }
    k
}

fn with_cutoff(c: Cutoff) -> KernelDesign {
    let mut k = base_kernel();
    if let Shape::WindowedSinc { cutoff, .. } = &mut k.shape {
        *cutoff = c;
    }
    k
}

fn band_spec(pe: f64, se: f64, a: f64) -> Cutoff {
    Cutoff::Band(BandSpec {
        passband_end: pe,
        stopband_start: se,
        attenuation_db: a,
    })
}

fn banded(length: Length, cutoff: Cutoff) -> KernelDesign {
    let mut k = with_length(length);
    if let Shape::WindowedSinc {
        cutoff: c, window, ..
    } = &mut k.shape
    {
        *c = cutoff;
        *window = Window::Kaiser {
            beta: None,
            sampling: Sampling::Symmetric,
        };
    }
    k
}

/// Values just outside and exactly on every bound, and impossible
/// pairings: each out-of-bounds value is refused with its field's path,
/// each on-bound value passes `check`.
#[test]
fn bounds_are_refused_with_their_field() {
    let kaiser = |beta| Window::Kaiser {
        beta: Some(beta),
        sampling: Sampling::Symmetric,
    };
    let cheb = |db: f64| Window::Chebyshev {
        sidelobe_ratio: 10f64.powf(-db / 20.0),
        sampling: Sampling::Symmetric,
    };
    let reflect = |mut k: KernelDesign| {
        k.phase = Phase::Minimum(PhaseMethod::Reflect);
        k
    };
    let mixed = |mix| KernelDesign {
        phase: Phase::Intermediate {
            mix,
            method: PhaseMethod::Cepstral,
        },
        ..base_kernel()
    };
    let block = |b| {
        FilterDesign::BlockFft(BlockFftDesign {
            block: b,
            overlap: Overlap::Half,
        })
    };
    let k = FilterDesign::Kernel;
    let cases: Vec<(FilterDesign, Option<&str>)> = vec![
        (k(with_window(kaiser(40.0))), None),
        (
            k(with_window(kaiser(40.000_001))),
            Some("design.window.beta"),
        ),
        (k(with_window(kaiser(-0.1))), Some("design.window.beta")),
        (k(with_window(kaiser(f64::NAN))), Some("design.window.beta")),
        (k(reflect(with_window(kaiser(20.0)))), None),
        (
            k(reflect(with_window(kaiser(20.01)))),
            Some("design.window.beta"),
        ),
        (
            k(reflect(with_window(kaiser(1.99)))),
            Some("design.window.beta"),
        ),
        (
            k(with_window(Window::Gaussian {
                alpha: 0.05,
                sampling: Sampling::Symmetric,
            })),
            None,
        ),
        (
            k(with_window(Window::Gaussian {
                alpha: 0.049,
                sampling: Sampling::Symmetric,
            })),
            Some("design.window.alpha"),
        ),
        (
            k(with_window(Window::Gaussian {
                alpha: 0.501,
                sampling: Sampling::Symmetric,
            })),
            Some("design.window.alpha"),
        ),
        (k(with_window(cheb(220.0))), None),
        (
            k(with_window(cheb(220.1))),
            Some("design.window.sidelobe_ratio"),
        ),
        (
            k(with_window(cheb(39.9))),
            Some("design.window.sidelobe_ratio"),
        ),
        (k(reflect(with_window(cheb(190.0)))), None),
        (
            k(reflect(with_window(cheb(190.1)))),
            Some("design.window.sidelobe_ratio"),
        ),
        (
            k(with_window(Window::Tukey {
                taper: 1.0,
                sampling: Sampling::Symmetric,
            })),
            None,
        ),
        (
            k(with_window(Window::Tukey {
                taper: 0.0,
                sampling: Sampling::Symmetric,
            })),
            Some("design.window.taper"),
        ),
        (
            k(with_window(Window::Tukey {
                taper: 1.01,
                sampling: Sampling::Symmetric,
            })),
            Some("design.window.taper"),
        ),
        (
            k(reflect(with_window(Window::Tukey {
                taper: 0.5,
                sampling: Sampling::Symmetric,
            }))),
            Some("design.window.type"),
        ),
        (
            k(with_window(Window::Hann {
                sampling: Sampling::IntegerCentre,
            })),
            Some("design.window.sampling"),
        ),
        (
            k(reflect(with_window(Window::Hann {
                sampling: Sampling::Periodic,
            }))),
            Some("design.window.sampling"),
        ),
        (
            k(with_window(Window::GaussianStopband {
                beta: Some(12.0),
                sampling: Sampling::Symmetric,
            })),
            Some("design.window.type"),
        ),
        (k(with_length(Length::Fixed(1))), None),
        (k(with_length(Length::Fixed(0))), Some("design.length.taps")),
        (
            k(with_length(Length::Fixed(budget::MAX_KERNEL_TAPS + 1))),
            Some("design.length.taps"),
        ),
        (
            k(with_length(Length::PerInput(Stepped::Fixed(65_535)))),
            None,
        ),
        (
            k(with_length(Length::PerInput(Stepped::Fixed(65_536)))),
            Some("design.length.taps"),
        ),
        (
            k(with_length(Length::PerInput(Stepped::BySourceRate(vec![
                (Some(50_000), 63),
                (Some(40_000), 31),
                (None, 15),
            ])))),
            Some("design.length.taps"),
        ),
        (
            k(with_length(Length::OutputSpan(Span::Taps(1 << 12)))),
            None,
        ),
        (
            k(with_length(Length::OutputSpan(Span::Taps(1 << 22)))),
            None,
        ),
        (
            k(with_length(Length::OutputSpan(Span::Taps((1 << 12) + 1)))),
            Some("design.length.taps"),
        ),
        (
            k(with_length(Length::OutputSpan(Span::Taps(1 << 23)))),
            Some("design.length.taps"),
        ),
        (
            k(banded(
                Length::Auto(FilterErrorBudget::Db(55.0)),
                band_spec(0.9, 1.0, 100.0),
            )),
            None,
        ),
        (
            k(banded(
                Length::Auto(FilterErrorBudget::Db(200.1)),
                band_spec(0.9, 1.0, 100.0),
            )),
            Some("design.length.budget"),
        ),
        (
            k(banded(
                Length::Auto(FilterErrorBudget::Db(54.9)),
                band_spec(0.9, 1.0, 100.0),
            )),
            Some("design.length.budget"),
        ),
        (k(with_cutoff(Cutoff::Fraction(Stepped::Fixed(1.0)))), None),
        (
            k(with_cutoff(Cutoff::Fraction(Stepped::Fixed(1.000_001)))),
            Some("design.cutoff.value"),
        ),
        (
            k(with_cutoff(Cutoff::Fraction(Stepped::Fixed(0.0)))),
            Some("design.cutoff.value"),
        ),
        (k(banded(Length::Band, band_spec(0.5, 1.5, 225.0))), None),
        (
            k(banded(Length::Band, band_spec(0.49, 1.0, 100.0))),
            Some("design.cutoff.passband_end"),
        ),
        (
            k(banded(Length::Band, band_spec(0.9, 1.51, 100.0))),
            Some("design.cutoff.passband_end"),
        ),
        (
            k(banded(Length::Band, band_spec(0.9, 0.9, 100.0))),
            Some("design.cutoff.passband_end"),
        ),
        (
            k(banded(Length::Band, band_spec(0.9, 1.0, 225.1))),
            Some("design.cutoff.attenuation_db"),
        ),
        (
            k(reflect(banded(Length::Band, band_spec(0.9, 1.0, 190.5)))),
            Some("design.cutoff.attenuation_db"),
        ),
        // Impossible pairings.
        (
            k(with_cutoff(band_spec(0.9, 1.0, 100.0))),
            Some("design.cutoff.mode"),
        ),
        (k(with_length(Length::Band)), Some("design.cutoff.mode")),
        (k(mixed(0.0)), None),
        (k(mixed(1.0)), None),
        (k(mixed(1.01)), Some("design.phase.mix")),
        (k(mixed(-0.01)), Some("design.phase.mix")),
        (
            k(KernelDesign {
                preserve_samples: true,
                ..base_kernel()
            }),
            Some("design.preserve_samples"),
        ),
        (
            k(KernelDesign {
                decimation: Decimation::Scale {
                    odd: OddRule::Up,
                    base_taps: Some(0),
                },
                ..base_kernel()
            }),
            Some("design.decimation.base_taps"),
        ),
        (
            k(KernelDesign {
                numerics: Numerics {
                    path: ArithmeticPath::Fir,
                    grid_cutoff: GridCutoff::DesignRatio,
                },
                ..base_kernel()
            }),
            Some("design.decimation.length"),
        ),
        (
            k(KernelDesign {
                phase: Phase::Minimum(PhaseMethod::Reflect),
                numerics: Numerics {
                    path: ArithmeticPath::ExactPiInverseAfter,
                    grid_cutoff: GridCutoff::Scaled,
                },
                ..with_window(kaiser(12.0))
            }),
            Some("design.phase.method"),
        ),
        (
            k(KernelDesign {
                rates: Rates {
                    accept: Accept::Any,
                    fallback_window: Some(Fallback {
                        when: FallbackWhen::Decimating,
                        window: Window::Tukey {
                            taper: 0.5,
                            sampling: Sampling::Symmetric,
                        },
                    }),
                },
                ..base_kernel()
            }),
            Some("design.rates.fallback_window"),
        ),
        (
            k(KernelDesign {
                shape: Shape::Lanczos { lobes: 65_536 },
                ..base_kernel()
            }),
            Some("design.lobes"),
        ),
        (block(BlockSize::Input(1 << 10)), None),
        (block(BlockSize::Input(1 << 16)), None),
        (
            block(BlockSize::Input((1 << 10) - 1)),
            Some("design.block.input"),
        ),
        (block(BlockSize::Input(1 << 17)), Some("design.block.input")),
        (block(BlockSize::Output(1 << 22)), None),
        (
            block(BlockSize::Output(1 << 23)),
            Some("design.block.output"),
        ),
    ];
    let mut failures = Vec::new();
    for (d, want) in &cases {
        let got = catch_unwind(AssertUnwindSafe(|| check::check(d)));
        match (got, want) {
            (Err(_), _) => failures.push(format!("{d:?}: check panicked")),
            (Ok(Ok(())), None) => {}
            (Ok(Err(e)), Some(path)) if e.path == *path => {}
            (Ok(got), want) => failures.push(format!("{d:?}: got {got:?}, want {want:?}")),
        }
        // Nothing panics further down either.
        for (src, ratio) in [
            (44_100, RateRatio::integer(4)),
            (48_000, RateRatio::new(147, 160)),
        ] {
            let c = cx(src, ratio, MAX_BLOCK);
            if catch_unwind(AssertUnwindSafe(|| design::feasibility(d, &c))).is_err() {
                failures.push(format!("{d:?}: feasibility panicked at {ratio}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The ceilings: the tap limits, the rational cost and the workspace cap,
/// each a typed refusal.
#[test]
fn ceilings_are_typed_refusals() {
    let long = with_length(Length::PerInput(Stepped::Fixed(65_535)));
    let e = design::feasibility(
        &FilterDesign::Kernel(long.clone()),
        &cx(44_100, RateRatio::new(2560, 147), 256),
    )
    .unwrap_err();
    assert!(
        matches!(e, DesignError::TooLong { .. } | DesignError::Budget(_)),
        "{e:?}"
    );
    let e = design::feasibility(
        &FilterDesign::Kernel(with_length(Length::PerInput(Stepped::Fixed(4095)))),
        &cx(44_100, RateRatio::new(2560, 147), 256),
    )
    .unwrap_err();
    assert!(matches!(e, DesignError::Budget(_)), "{e:?}");
    // A novel combination's workspace over a small cap.
    let mut c = cx(44_100, RateRatio::integer(2), 256);
    c.workspace = 1 << 20;
    let e = design::feasibility(
        &FilterDesign::Kernel(with_window(Window::Kaiser {
            beta: Some(12.0),
            sampling: Sampling::Symmetric,
        })),
        &c,
    )
    .unwrap_err();
    assert!(matches!(e, DesignError::Budget(_)), "{e:?}");
    // The rate class.
    let e = design::feasibility(
        &FilterDesign::Kernel(KernelDesign {
            rates: Rates {
                accept: Accept::Integer,
                fallback_window: None,
            },
            ..with_window(Window::Hann {
                sampling: Sampling::Symmetric,
            })
        }),
        &cx(44_100, RateRatio::new(160, 147), 256),
    )
    .unwrap_err();
    assert!(matches!(e, DesignError::Ratio { .. }), "{e:?}");
}

// ----------------------------------------------------- fixed regressions

fn prepare_ok(d: &FilterDesign, src: u32, ratio: RateRatio) -> design::Prepared {
    design::prepare(d, &cx(src, ratio, MAX_BLOCK))
        .unwrap_or_else(|e| panic!("{d:?} at {ratio}: {e}"))
}

#[test]
fn cepstral_and_reflect_mix_endpoints() {
    let base = KernelDesign {
        phase: Phase::Linear,
        ..with_window(Window::Kaiser {
            beta: Some(12.0),
            sampling: Sampling::Symmetric,
        })
    };
    let x4 = RateRatio::integer(4);
    let taps = |mix, method| {
        let d = FilterDesign::Kernel(KernelDesign {
            phase: Phase::Intermediate { mix, method },
            ..base.clone()
        });
        let p = prepare_ok(&d, 44_100, x4);
        let rt = design::build(&p, &cx(44_100, x4, MAX_BLOCK), 0.0)
            .unwrap()
            .unwrap();
        (p.kernel.unwrap().taps.len(), rt.latency_out_frames())
    };
    let n = 63 * 4;
    for (mix, len) in [
        (0.0, n),
        (0.75, 4 * n),
        (1.0, n),
        (0.749, 4 * n),
        (0.751, 4 * n),
    ] {
        assert_eq!(taps(mix, PhaseMethod::Cepstral).0, len, "cepstral {mix}");
    }
    // The table and the general reduction meet at 0.75: the blends on
    // either side are close to it.
    assert_eq!(taps(0.0, PhaseMethod::Reflect).0, n);
    assert_eq!(taps(1.0, PhaseMethod::Reflect).0, n);
}

#[test]
fn chebyshev_window_around_the_direct_sum_limit() {
    for path in [ArithmeticPath::Fir, ArithmeticPath::ExactPiInverseAfter] {
        for (taps, ratio) in [(8192usize, 2u32), (8193, 3)] {
            let d = FilterDesign::Kernel(KernelDesign {
                numerics: Numerics {
                    path,
                    grid_cutoff: GridCutoff::Scaled,
                },
                ..with_length(Length::Fixed(taps))
            });
            let mut d = d;
            if let FilterDesign::Kernel(k) = &mut d
                && let Shape::WindowedSinc { window, .. } = &mut k.shape
            {
                *window = Window::Chebyshev {
                    sidelobe_ratio: 2f64.powi(-32),
                    sampling: Sampling::Symmetric,
                };
            }
            let p = prepare_ok(&d, 44_100, RateRatio::integer(ratio));
            assert_eq!(p.kernel.unwrap().taps.len(), taps, "{path:?}");
        }
    }
}

#[test]
fn smallest_block_fft_geometry() {
    for overlap in [Overlap::QuarterAverage, Overlap::Half] {
        for block in [BlockSize::Input(1 << 10), BlockSize::Output(1 << 16)] {
            let d = FilterDesign::BlockFft(BlockFftDesign { block, overlap });
            let x64 = RateRatio::integer(64);
            let p = prepare_ok(&d, 44_100, x64);
            runtime_properties(&d, &cx(44_100, x64, MAX_BLOCK), &p).unwrap();
        }
    }
    // One block too small at this ratio.
    let d = FilterDesign::BlockFft(BlockFftDesign {
        block: BlockSize::Output(1 << 15),
        overlap: Overlap::Half,
    });
    let e = design::feasibility(&d, &cx(44_100, RateRatio::integer(64), MAX_BLOCK)).unwrap_err();
    assert!(matches!(e, DesignError::Field(_)), "{e:?}");
}

/// The largest output-sized blocks: each resolves to its input block
/// (`P / L`, at most [`check::BLOCK_OUTPUT_MAX_INPUT`]) and runs as that
/// input block does; one past the bound is refused with its field.
#[test]
fn largest_block_fft_geometries() {
    let run = |d: &FilterDesign, ratio: RateRatio| {
        let c = cx(44_100, ratio, MAX_BLOCK);
        let p = prepare_ok(d, 44_100, ratio);
        let mut rt = design::build(&p, &c, 0.0).unwrap().unwrap();
        let mut s = 7u64;
        let input = noise(4 << 16, &mut s);
        let (out, _) = run(rt.as_mut(), &input, MAX_BLOCK);
        (rt.latency_out_frames(), out)
    };
    for overlap in [Overlap::Half, Overlap::QuarterAverage] {
        for l in [2u32, 8, 64] {
            let ratio = RateRatio::integer(l);
            // The output block of the 2^16 input block: the same runtime.
            let out = FilterDesign::BlockFft(BlockFftDesign {
                block: BlockSize::Output((1 << 16) * l as usize),
                overlap,
            });
            let inp = FilterDesign::BlockFft(BlockFftDesign {
                block: BlockSize::Input(1 << 16),
                overlap,
            });
            let (a, b) = (run(&out, ratio), run(&inp, ratio));
            assert_eq!(a.0, b.0, "x{l} {overlap:?}");
            assert!(same(&a.1, &b.1), "x{l} {overlap:?}");
            // The largest admitted, and one past it.
            let largest = check::BLOCK_OUTPUT_MAX_INPUT.min((1 << 22) / l as usize) * l as usize;
            let d = |p| {
                FilterDesign::BlockFft(BlockFftDesign {
                    block: BlockSize::Output(p),
                    overlap,
                })
            };
            let c = cx(44_100, ratio, MAX_BLOCK);
            design::feasibility(&d(largest), &c).unwrap_or_else(|e| panic!("x{l}: {e}"));
            let e = design::feasibility(&d(largest * 2), &c).unwrap_err();
            match e {
                DesignError::Field(f) => assert_eq!(f.path, "design.block.output", "x{l}"),
                e => panic!("x{l}: {e:?}"),
            }
        }
    }
    // The largest geometry runs: 2^19 input frames at x2.
    let d = FilterDesign::BlockFft(BlockFftDesign {
        block: BlockSize::Output(1 << 20),
        overlap: Overlap::Half,
    });
    let x2 = RateRatio::integer(2);
    let p = prepare_ok(&d, 44_100, x2);
    runtime_properties(&d, &cx(44_100, x2, MAX_BLOCK), &p).unwrap();
}

#[test]
fn lanczos65535_ratio_limits() {
    let d = design::catalogue::design(audivane_protocol::FilterPreset::Lanczos65535);
    for r in [32, 33, 64] {
        design::feasibility(&d, &cx(44_100, RateRatio::integer(r), MAX_BLOCK))
            .unwrap_or_else(|e| panic!("x{r}: {e}"));
    }
    let e = design::feasibility(&d, &cx(44_100, RateRatio::integer(65), MAX_BLOCK)).unwrap_err();
    assert!(matches!(e, DesignError::Ratio { .. }), "{e:?}");
}

#[test]
fn even_length_standard_delay_conventions() {
    let d = FilterDesign::Kernel(
        design::paths::standard::design_of(&design::paths::standard::ManualConfig {
            taps: 4096,
            kaiser_beta: 12.0,
            cutoff: 0.95,
            ..Default::default()
        })
        .unwrap(),
    );
    let p = prepare_ok(&d, 44_100, RateRatio::integer(8));
    let k = p.kernel.as_ref().unwrap();
    assert_eq!(k.delay, 2047);
    let r = RateRatio::new(160, 147);
    let p = prepare_ok(&d, 44_100, r);
    let k = p.kernel.as_ref().unwrap();
    assert_eq!(k.delay_exact, (k.taps.len() as f64 - 1.0) / 2.0);
    let rt = design::build(&p, &cx(44_100, r, MAX_BLOCK), 0.0)
        .unwrap()
        .unwrap();
    assert!((rt.latency_out_exact() - k.delay_exact / 147.0).abs() < 1e-9);
}
