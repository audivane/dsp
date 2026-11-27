//! Filter design: one design type for every upsampling filter.
//!
//! A [`FilterDesign`] is a value: a kernel design (windowed sinc or
//! Lanczos, with its window, length rule, cutoff, phase, rate rules and
//! arithmetic path) or one of the runtimes with fixed internal filters
//! (cascade, interpolator, block FFT). A preset is a named design
//! ([`catalogue::design`]). Everything that depends on the source rate or
//! the ratio is a rule inside the design, evaluated at build time, so one
//! design means the same thing at every rate.
//!
//! Module map: [`catalogue`] (the named designs and
//! [`catalogue::recommended`]), [`grid`] (the polyphase grid of a ratio),
//! [`length`] (taps per input sample and the decimation length), [`rates`]
//! (the ratio rules and the fallback window), [`paths`] (the kernel
//! builders, each kept with its exact arithmetic), [`auto`] (the measured
//! automatic length of the `standard` path), [`phase`] (the minimum- and
//! intermediate-phase transforms), [`budget`] (the design limits and the
//! [`Plan`]), [`safety`] (the post-build safety gate), [`upsampler()`] (the
//! engine's upsampler from its config).
//!
//! Building, all off the real-time thread, in three steps with one
//! [`BuildContext`] (rate, ratio, channels, block, pipelines, the output
//! a `match_output` budget resolves against): [`feasibility`] (cheap: the
//! ratio rules and the budgets, giving the [`Plan`]), [`prepare`] (the
//! search and the kernel design, the budgets on the real length, the
//! safety gate), [`build`] (the runtime, headroom applied explicitly; none
//! at a unity ratio).
//!
//! The chain builds every upsampler through [`upsampler()`]; the daemon
//! validates designs and publishes its rate masks with [`feasibility`].
//! The real-time thread only ever receives a finished runtime.

pub mod auto;
pub mod budget;
pub mod catalogue;
pub mod check;
pub mod claim;
pub(crate) mod ddouble;
pub mod explain;
pub mod grid;
pub mod length;
pub mod paths;
pub mod phase;
pub mod rates;
pub mod safety;
pub mod search;
pub mod upsampler;
pub mod window_model;

use crate::presets::UpsamplerRuntime;
use crate::rational::RationalResampler;
use crate::resample::Upsampler;
use audivane_protocol::RateRatio;

pub use audivane_protocol::design::{
    Accept, ArithmeticPath, BlockFftDesign, BlockSize, CascadeStage, Cutoff, Decimation, Fallback,
    FallbackWhen, FilterDesign, FlattopSet, GridCutoff, InterpMethod, KernelDesign, Length,
    Numerics, OddRule, OptimalMethod, Overlap, Phase, PhaseMethod, RECOMMENDED_PRESET, Rates,
    Sampling, Shape, Span, Stepped, StoredDesign, Window,
};
pub use audivane_protocol::{BandSpec, FilterErrorBudget, FilterPreset};
pub use grid::Grid;
pub use upsampler::{Named, Refusal, upsampler};

/// Why a design cannot be built.
#[derive(Clone, Debug, PartialEq)]
pub enum DesignError {
    /// The design does not run at this ratio; `need` says what it needs.
    Ratio {
        ratio: RateRatio,
        need: &'static str,
    },
    /// The kernel at this integer ratio would be longer than
    /// [`budget::MAX_KERNEL_TAPS`].
    TooLong { ratio: RateRatio, taps: usize },
    /// Over one of the design limits ([`budget`]), with its message.
    Budget(budget::Budget),
    /// A combination this step does not build yet.
    Unsupported(String),
    /// The design has no single kernel (a runtime kind).
    NotAKernel,
    /// A design step failed (for example the phase transform).
    Failed(String),
    /// A `standard` design whose band, length or automatic search cannot
    /// be designed (the Custom builder's own error and message).
    Band(crate::resample::BandError),
    /// A reflected minimum or intermediate phase that failed its checks.
    Phase(crate::minphase::MinimumPhaseError),
    /// The built design failed the safety gate ([`safety`]).
    Unsafe(String),
    /// A field out of its bounds ([`check`]).
    Field(check::FieldError),
    /// A design that cannot keep the attenuation it promises: deeper than
    /// its window goes, or measured short of it once built ([`claim`]).
    Claim(String),
}

impl std::fmt::Display for DesignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ratio { ratio, need } => {
                write!(f, "the design does not run at {ratio}: it needs {need}")
            }
            Self::TooLong { ratio, taps } => write!(
                f,
                "the design does not run at {ratio}: its kernel would have {taps} taps, more \
                 than {}; use a lower ratio or a shorter design",
                budget::MAX_KERNEL_TAPS
            ),
            Self::Budget(e) => e.fmt(f),
            Self::Unsupported(e) => f.write_str(e),
            Self::NotAKernel => f.write_str("the design has no single kernel"),
            Self::Failed(e) => write!(f, "filter design failed: {e}"),
            Self::Band(e) => e.fmt(f),
            Self::Phase(e) => e.fmt(f),
            Self::Unsafe(e) => f.write_str(e),
            Self::Field(e) => e.fmt(f),
            Self::Claim(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for DesignError {}

/// A kernel on the grid (rate `source x up`; the output rate for integer
/// ratios), each polyphase branch summing to about 1.
#[derive(Clone, Debug, PartialEq)]
pub struct Kernel {
    pub taps: Vec<f64>,
    /// Delay in grid samples: the centre for linear phase, the largest tap
    /// otherwise (the `standard` path: `floor((N - 1) / 2)` for linear).
    pub delay: usize,
    /// The delay a rational runtime reports, in grid samples: `delay`,
    /// except a `standard` linear kernel's exact centre `(N - 1) / 2`,
    /// half a sample later at even N.
    pub delay_exact: f64,
}

/// Everything a build depends on besides the design.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuildContext {
    pub source_rate: u32,
    pub ratio: RateRatio,
    pub channels: usize,
    /// Most input frames per `process` call.
    pub max_block: usize,
    /// Rational pipelines that can run at once
    /// ([`budget::RATIONAL_SWAP_PIPELINES`], twice that with A/B).
    pub pipelines: usize,
    /// The chain's output section: what an automatic length with the
    /// `match_output` budget resolves against ([`FilterDesign::with_output`]).
    /// One context per output-format variant.
    pub output: audivane_protocol::OutputConfig,
    /// The design workspace cap, bytes: the most a design may allocate
    /// while it is built ([`budget::check_workspace`]). Production uses
    /// the machine's ([`budget::design_workspace`]); tests set their own.
    pub workspace: u64,
}

impl BuildContext {
    /// The grid of the context's ratio.
    pub fn grid(&self) -> Grid {
        Grid::new(self.source_rate, self.ratio)
    }
}

/// The rate whose Nyquist a band's fractions are of, from `source_rate` at
/// `ratio`: the lower of the input and output rates (the output rate when
/// decimating), on every path. An absolute passband end converts there.
pub fn band_rate(source_rate: u32, ratio: RateRatio) -> u32 {
    ratio
        .output_rate(source_rate)
        .map_or(source_rate, |out| out.min(source_rate))
}

/// A band spec's passband end as a fraction of the Nyquist at
/// `source_rate` (the [`band_rate`] of a build):
/// [`BandSpec::passband_end_hz`] converted when set (so the band does not
/// scale with the source rate), else its own fraction. A pinned frequency
/// that does not fit the band at this rate ([`band_pin_misfit`]) falls
/// back to the spec's own fraction, [`BandSpec::passband_end`], so the
/// rate still builds.
pub fn band_passband_end(spec: &BandSpec, source_rate: u32) -> f64 {
    match spec.passband_end_hz {
        Some(_) if band_pin_misfit(spec, source_rate).is_some() => spec.passband_end,
        Some(hz) => hz / (f64::from(source_rate) / 2.0),
        None => spec.passband_end,
    }
}

/// The pinned passband end of `spec` as a fraction of the Nyquist at
/// `source_rate`, when it is set and does not fit the band there: it must
/// end below the stopband start and at most at the Nyquist, and leave a
/// transition no wider than the source Nyquist (the band length's
/// calibrated domain, a half transition of at most 0.5). `None` when the
/// pin fits or there is none. 20 kHz fits from 44.1 kHz up with the
/// stopband at the Nyquist; at 32 kHz it comes to 1.25 and falls back.
pub fn band_pin_misfit(spec: &BandSpec, source_rate: u32) -> Option<f64> {
    let hz = spec.passband_end_hz?;
    let f = hz / (f64::from(source_rate) / 2.0);
    let se = spec.stopband_start;
    let fits = f.is_finite() && f > 0.0 && f <= 1.0 && f < se && se - f <= 1.0;
    (!fits).then_some(f)
}

/// The band of a windowed-sinc kernel design with a band cutoff.
pub fn band_of(d: &FilterDesign) -> Option<&BandSpec> {
    match d {
        FilterDesign::Kernel(KernelDesign {
            shape:
                Shape::WindowedSinc {
                    cutoff: Cutoff::Band(b),
                    ..
                },
            ..
        }) => Some(b),
        _ => None,
    }
}

/// What a design does instead of its pinned passband end from
/// `source_rate` at `ratio` when the pin does not fit the band there
/// ([`band_pin_misfit`]), for the design check's notes; `None` when it fits
/// or there is no pin. The band is of the lower Nyquist ([`band_rate`]).
pub fn band_pin_note(d: &FilterDesign, source_rate: u32, ratio: RateRatio) -> Option<String> {
    let b = band_of(d)?;
    let band_rate = band_rate(source_rate, ratio);
    let f = band_pin_misfit(b, band_rate)?;
    let hz = b.passband_end_hz?;
    let nyquist = f64::from(band_rate) / 2.0;
    Some(format!(
        "at {source_rate} Hz, the passband end {hz} Hz ({f:.3} of the Nyquist) does not fit \
         below the stopband start ({}): the passband ends at {} of the Nyquist ({:.0} Hz) instead",
        b.stopband_start,
        b.passband_end,
        b.passband_end * nyquist
    ))
}

pub use budget::Plan;

/// A design checked and designed for one context: what [`build`] turns
/// into a runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    /// The design as built: `match_output` resolved for the context.
    pub design: FilterDesign,
    pub source_rate: u32,
    pub ratio: RateRatio,
    /// What [`feasibility`] planned (for an automatic length, the length
    /// the search found).
    pub plan: Plan,
    /// The kernel of a kernel design; `None` for the runtime kinds.
    pub kernel: Option<Kernel>,
    /// What the safety gate measured ([`safety`]), including the
    /// bounded-input peak.
    pub safety: safety::Safety,
}

/// Whether the `standard` path builds `k` (its legacy builder or its new
/// combinations): a design on that path but an optimal one, which only
/// the general path builds (and refuses off the fir paths).
fn on_combos(k: &KernelDesign) -> bool {
    k.numerics.path == ArithmeticPath::Standard && !matches!(k.shape, Shape::Optimal { .. })
}

/// `d`'s kernel design when the `standard` path's legacy builder (the
/// Custom builder) builds it.
fn standard(d: &FilterDesign) -> Option<&KernelDesign> {
    match d {
        FilterDesign::Kernel(k) if on_combos(k) && paths::standard::combos::legacy(k) => Some(k),
        _ => None,
    }
}

/// `d`'s kernel design when it is a new combination: one the legacy
/// builders do not build, on a preset path ([`paths::general`]) or the
/// `standard` path ([`paths::standard::combos`]).
fn general(d: &FilterDesign) -> Option<&KernelDesign> {
    match d {
        FilterDesign::Kernel(k) if novel(k) => Some(k),
        _ => None,
    }
}

/// Whether `k` is a new combination: the legacy builders keep every other
/// design's bits and refusals.
pub(crate) fn novel(k: &KernelDesign) -> bool {
    if on_combos(k) {
        !paths::standard::combos::legacy(k)
    } else {
        !paths::general::legacy(k)
    }
}

/// [`feasibility`] of the new combination `k` (`search`: an automatic
/// length searched, not estimated).
fn novel_feasibility(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
    search: bool,
) -> Result<Plan, DesignError> {
    if on_combos(k) {
        return paths::standard::combos::feasibility(d, k, cx);
    }
    paths::general::feasibility(d, k, cx, search).map(|(plan, _)| plan)
}

/// The kernel of the new combination `k` (its promise measured).
fn novel_kernel(
    k: &KernelDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<Kernel, DesignError> {
    if on_combos(k) {
        return paths::standard::combos::kernel_at(k, source_rate, ratio);
    }
    paths::general::kernel_at(k, source_rate, &Grid::new(source_rate, ratio))
}

/// The grid taps of the new combination `k` and the taps its runtime is
/// priced at (an automatic length searched).
pub(crate) fn novel_taps(
    k: &KernelDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<(usize, usize), DesignError> {
    if on_combos(k) {
        return paths::standard::combos::taps(k, source_rate, ratio);
    }
    paths::general::taps(k, source_rate, &Grid::new(source_rate, ratio))
}

/// The plan of the new combination `k` in `cx` with its length resolved,
/// no budget checked.
pub(crate) fn novel_plan(k: &KernelDesign, cx: &BuildContext) -> Result<Plan, DesignError> {
    if on_combos(k) {
        return paths::standard::combos::resolved_plan(k, cx);
    }
    paths::general::resolved_plan(k, cx)
}

/// Taps of `d`'s kernel on the grid of `ratio` (four times the design
/// length for intermediate phase), without designing it. On the
/// `standard` path this evaluates the length rule (an automatic length
/// runs its search, cached), and a reflected intermediate kernel is at
/// most four times as long.
pub fn grid_taps(
    d: &FilterDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<usize, DesignError> {
    if let Some(k) = standard(d) {
        let n = paths::standard::recipe(k, source_rate, ratio)?.taps;
        rates::check(d, ratio)?;
        return Ok(match k.phase {
            Phase::Intermediate { .. } => 4 * n,
            _ => n,
        });
    }
    rates::check(d, ratio)?;
    if let Some(k) = general(d) {
        return Ok(novel_taps(k, source_rate, ratio)?.0);
    }
    let FilterDesign::Kernel(k) = d else {
        return Err(DesignError::NotAKernel);
    };
    let grid = Grid::new(source_rate, ratio);
    let n = paths::recipe(k, source_rate, &grid)?.taps * grid.up;
    Ok(match k.phase {
        Phase::Intermediate { .. } => 4 * n,
        _ => n,
    })
}

/// Whether `d` can be built in `cx`, cheaply (no kernel, no search, no
/// FFT), and its [`Plan`]: the ratio rules ([`rates::check`]) and, at a
/// rational ratio, the rational runtime's length and cost budget with
/// `cx.pipelines` pipelines. On the `standard` path an automatic length
/// gets only the search's preliminary checks (its plan holds the
/// window-formula estimate); its budget is checked by [`prepare`]
/// (`paths::standard::feasibility`). An automatic `match_output` budget
/// is resolved against `cx.output` first.
pub fn feasibility(d: &FilterDesign, cx: &BuildContext) -> Result<Plan, DesignError> {
    let d = &d.with_output(&cx.output);
    if let Some(k) = standard(d) {
        return paths::standard::feasibility(d, k, cx);
    }
    if let Some(k) = general(d) {
        return novel_feasibility(d, k, cx, false);
    }
    rates::check(d, cx.ratio)?;
    let FilterDesign::Kernel(k) = d else {
        if let FilterDesign::BlockFft(b) = d
            && catalogue::runtime_preset(d).is_none()
        {
            block_fft_block(b, cx.ratio)?;
        }
        return Plan::runtime(d, cx).check_workspace(cx);
    };
    let n = grid_taps(d, cx.source_rate, cx.ratio)?;
    if !cx.ratio.is_integer() {
        budget::check_rational(
            n,
            cx.ratio,
            cx.source_rate,
            budget::MAX_RATIONAL_GRID_TAPS,
            cx.pipelines,
        )?;
    }
    if !matches!(k.phase, Phase::Linear) {
        budget::check_cepstral(prototype_taps(k, n), cx.ratio)?;
    }
    kernel_plan(k, n, cx).check_workspace(cx)
}

/// The taps of the prototype a preset-path kernel of `n` grid taps starts
/// from: the phase transform makes an intermediate-phase kernel four times
/// longer.
fn prototype_taps(k: &KernelDesign, n: usize) -> usize {
    match k.phase {
        Phase::Intermediate { .. } => n / 4,
        _ => n,
    }
}

/// The plan of the preset-path kernel `k` of `n` grid taps in `cx` (no
/// budget checked).
pub(crate) fn kernel_plan(k: &KernelDesign, n: usize, cx: &BuildContext) -> Plan {
    let prototype = prototype_taps(k, n);
    let workspace = match k.phase {
        Phase::Linear => budget::linear_workspace(n),
        _ => budget::cepstral_workspace(prototype),
    };
    Plan::kernel(
        cx,
        n,
        true,
        workspace,
        budget::preset_priced_taps(k, prototype),
        true,
    )
}

/// The kernel of `d` from `source_rate` on the grid of `ratio`, within the
/// ratio rules (no cost check).
pub(crate) fn kernel(
    d: &FilterDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<Kernel, DesignError> {
    if let Some(k) = standard(d) {
        let r = paths::standard::recipe(k, source_rate, ratio)?;
        rates::check(d, ratio)?;
        return paths::standard::kernel(&r, ratio);
    }
    rates::check(d, ratio)?;
    if let Some(k) = general(d) {
        return novel_kernel(k, source_rate, ratio);
    }
    let FilterDesign::Kernel(k) = d else {
        return Err(DesignError::NotAKernel);
    };
    let grid = Grid::new(source_rate, ratio);
    let r = paths::recipe(k, source_rate, &grid)?;
    paths::kernel(k, &r, &grid)
}

/// [`feasibility`] with an automatic length searched (cached), not
/// estimated: the budgets [`prepare`] holds the design length to, without
/// designing the kernel (the rate mask, at the pairs a ladder can pick).
/// The same as [`feasibility`] for a design without an automatic length.
/// Off the real-time thread.
pub fn searched_feasibility(d: &FilterDesign, cx: &BuildContext) -> Result<Plan, DesignError> {
    let d = &d.with_output(&cx.output);
    match standard(d) {
        Some(k)
            if matches!(
                k.shape,
                Shape::WindowedSinc {
                    length: Length::Auto(_),
                    ..
                }
            ) =>
        {
            paths::standard::searched(d, k, cx).map(|(plan, _)| plan)
        }
        _ => match general(d) {
            Some(k) => novel_feasibility(d, k, cx, true),
            None => feasibility(d, cx),
        },
    }
}

/// The kernel of `d` from `source_rate` at `ratio` for offline use (an
/// export, an analysis): the ratio rules and the construction limits (the
/// cepstral transform, the design workspace), but not the playback
/// budgets (the rational runtime's length and cost) nor the safety gate.
/// Off the real-time thread.
pub fn offline_kernel(
    d: &FilterDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<Kernel, DesignError> {
    let cx = BuildContext {
        source_rate,
        ratio,
        channels: 1,
        max_block: 1024,
        pipelines: 1,
        output: audivane_protocol::OutputConfig::default(),
        workspace: budget::design_workspace(),
    };
    if let Some(k) = standard(d) {
        let r = paths::standard::recipe(k, source_rate, ratio)?;
        rates::check(d, ratio)?;
        let priced = budget::standard_priced_taps(k, &r);
        paths::standard::plan(k, r.taps, priced, &cx).check_workspace(&cx)?;
        return paths::standard::kernel(&r, ratio);
    }
    rates::check(d, ratio)?;
    if let Some(k) = general(d) {
        check::check(d).map_err(DesignError::Field)?;
        if !on_combos(k) {
            let plan = paths::general::resolved_plan(k, &cx)?;
            if paths::general::method(k) == Some(PhaseMethod::Cepstral) {
                budget::check_cepstral(plan.grid_taps / paths::general::growth(k).0, ratio)?;
            }
            plan.check_workspace(&cx)?;
        }
        return novel_kernel(k, source_rate, ratio);
    }
    let FilterDesign::Kernel(k) = d else {
        return Err(DesignError::NotAKernel);
    };
    let n = grid_taps(d, source_rate, ratio)?;
    if !matches!(k.phase, Phase::Linear) {
        budget::check_cepstral(prototype_taps(k, n), ratio)?;
    }
    kernel_plan(k, n, &cx).check_workspace(&cx)?;
    kernel(d, source_rate, ratio)
}

/// The prototype `d`'s kernel starts from (before normalisation and the
/// phase transform), for offline checks.
pub fn prototype(
    d: &FilterDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<Vec<f64>, DesignError> {
    rates::check(d, ratio)?;
    let FilterDesign::Kernel(k) = d else {
        return Err(DesignError::NotAKernel);
    };
    let grid = Grid::new(source_rate, ratio);
    if general(d).is_some() && !on_combos(k) {
        return paths::general::prototype_at(k, source_rate, &grid);
    }
    let r = paths::recipe(k, source_rate, &grid)?;
    paths::prototype(k.numerics.path, &r, grid.up)
}

/// Check `d` in `cx` ([`feasibility`]) and design its kernel. Off the
/// real-time thread.
pub fn prepare(d: &FilterDesign, cx: &BuildContext) -> Result<Prepared, DesignError> {
    let d = d.with_output(&cx.output);
    let (plan, kernel) = if let Some(k) = standard(&d) {
        let (plan, kernel) = paths::standard::prepare(&d, k, cx)?;
        (plan, Some(kernel))
    } else if let Some(k) = general(&d) {
        let (plan, kernel) = if on_combos(k) {
            paths::standard::combos::prepare(&d, k, cx)?
        } else {
            paths::general::prepare(&d, k, cx)?
        };
        (plan, Some(kernel))
    } else {
        let plan = feasibility(&d, cx)?;
        match &d {
            FilterDesign::Kernel(_) => (plan, Some(kernel(&d, cx.source_rate, cx.ratio)?)),
            _ => (plan, None),
        }
    };
    let safety = match (&d, &kernel) {
        (FilterDesign::Kernel(kd), Some(k)) => safety::check_kernel(&d, kd, k, cx)?,
        _ if cx.ratio.is_unity() => safety::Safety::unity(),
        _ => safety::check_runtime(&d, cx, || {
            runtime_kind(&d, cx.source_rate, cx.ratio, 1, cx.max_block)
        })?,
    };
    Ok(Prepared {
        design: d,
        source_rate: cx.source_rate,
        ratio: cx.ratio,
        plan,
        kernel,
        safety,
    })
}

/// The runtime of a runtime kind (fixed internal filters).
fn runtime_kind(
    d: &FilterDesign,
    source_rate: u32,
    ratio: RateRatio,
    channels: usize,
    max_block: usize,
) -> Result<Box<dyn UpsamplerRuntime>, DesignError> {
    let Some(preset) = catalogue::runtime_preset(d) else {
        // Any block FFT geometry within the bounds; the stage
        // filters of the other kinds are fixed (one per catalogue design).
        if let FilterDesign::BlockFft(b) = d {
            let r = block_fft_block(b, ratio)?;
            return Ok(Box::new(crate::presets::block_fft::BlockFft::new(
                r,
                ratio.up as usize,
                b.overlap == Overlap::QuarterAverage,
                channels,
            )));
        }
        return Err(DesignError::Unsupported(format!(
            "{d:?}: only the catalogue's runtime geometries are available"
        )));
    };
    let l = ratio.up as usize;
    let (src, ch, mb) = (source_rate, channels, max_block);
    use crate::presets::{block_fft, cascade, interp};
    match d {
        FilterDesign::Cascade(_) => cascade::build(preset, src, l, ch, mb),
        FilterDesign::Interpolator(_) => interp::build(preset, src, l, ch, mb),
        _ => block_fft::build(preset, src, l, ch, mb),
    }
    .map_err(|e| match e {
        crate::presets::PresetError::Design(m) => DesignError::Failed(m),
        e => DesignError::Failed(e.to_string()),
    })
}

/// The input block of the block FFT `b` at `ratio`, held to the bounds
/// ([`check`]; an output block resolves to a power of two from 2^10 to
/// [`check::BLOCK_OUTPUT_MAX_INPUT`] input frames at the ratio).
pub(crate) fn block_fft_block(b: &BlockFftDesign, ratio: RateRatio) -> Result<usize, DesignError> {
    check::check(&FilterDesign::BlockFft(*b)).map_err(DesignError::Field)?;
    let r = (ratio.up as usize).max(1);
    let block = match b.block {
        BlockSize::Input(n) => n,
        BlockSize::Output(p) => p / r,
    };
    let max = check::BLOCK_OUTPUT_MAX_INPUT;
    if !block.is_power_of_two() || !(1 << 10..=max).contains(&block) {
        return Err(DesignError::Field(check::FieldError {
            path: "design.block.output".into(),
            message: format!(
                "the output block over the ratio {ratio} is {block} input frames, outside 1024 to \
                 {max}"
            ),
        }));
    }
    Ok(block)
}

/// Allocate the runtime for `p` with `cx`'s channels and block size, its
/// output scaled by `-headroom_db`. Off the real-time thread. A unity
/// ratio has no upsampler: `None` (the chain bypasses the stage).
pub fn build(
    p: &Prepared,
    cx: &BuildContext,
    headroom_db: f64,
) -> Result<Option<Box<dyn UpsamplerRuntime>>, DesignError> {
    if p.ratio.is_unity() {
        return Ok(None);
    }
    let ratio = p.ratio;
    let (channels, max_block) = (cx.channels, cx.max_block);
    let mut rt: Box<dyn UpsamplerRuntime> = match (&p.design, &p.kernel) {
        (FilterDesign::Kernel(_), Some(k)) if ratio.is_integer() => Box::new(
            Upsampler::from_kernel(&k.taps, ratio.up as usize, k.delay, channels, max_block),
        ),
        (FilterDesign::Kernel(_), Some(k)) => Box::new(RationalResampler::from_kernel(
            &k.taps,
            ratio,
            k.delay_exact,
            channels,
            max_block,
        )),
        (FilterDesign::Kernel(_), None) => {
            return Err(DesignError::Failed("the design was not prepared".into()));
        }
        (d, _) => runtime_kind(d, p.source_rate, ratio, channels, max_block)?,
    };
    if headroom_db != 0.0 {
        rt.scale_output(crate::db_to_lin(-headroom_db));
    }
    Ok(Some(rt))
}
