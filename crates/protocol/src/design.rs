//! Upsampling filter design types.
//!
//! A [`FilterDesign`] is a rate-independent value: a kernel design (windowed
//! sinc or Lanczos) or one of the fixed-filter runtimes (cascade, interpolator,
//! block FFT). Everything that depends on the source rate or ratio is expressed
//! as a rule inside the design and evaluated at build time.
//!
//! [`StoredDesign`] wraps a [`FilterDesign`] with an optional `based_on`
//! catalogue name recorded by "Edit as custom".

use crate::{BandSpec, FilterErrorBudget, FilterPhase, OutputConfig, filter_error_budget_db};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::ops::Deref;

/// Catalogue name of the built-in Recommended design.
pub const RECOMMENDED_PRESET: &str = "recommended";

/// Kaiser beta to stopband attenuation in dB (matches `dsp-core`'s
/// `kaiser_attenuation_db`).
fn beta_to_stopband_db(beta: f64) -> f64 {
    beta / 0.1102 + 8.7
}

/// Stopband attenuation in dB to Kaiser beta (inverse of
/// `beta_to_stopband_db`).
fn stopband_db_to_beta(db: f64) -> f64 {
    0.1102 * (db - 8.7)
}

/// A custom filter design on the wire: the [`FilterDesign`] plus an optional
/// `based_on` catalogue name recorded when copying a preset into Custom.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, rename = "FilterDesign", as = "WireFilterDesign")
)]
pub struct StoredDesign {
    pub based_on: Option<String>,
    pub design: FilterDesign,
}

impl StoredDesign {
    pub fn new(design: FilterDesign) -> Self {
        Self {
            based_on: None,
            design,
        }
    }

    pub fn based_on(name: impl Into<String>, design: FilterDesign) -> Self {
        Self {
            based_on: Some(name.into()),
            design,
        }
    }

    pub fn with_output(&self, output: &OutputConfig) -> Self {
        Self {
            based_on: self.based_on.clone(),
            design: self.design.with_output(output),
        }
    }
}

impl From<FilterDesign> for StoredDesign {
    fn from(design: FilterDesign) -> Self {
        Self::new(design)
    }
}

impl Deref for StoredDesign {
    type Target = FilterDesign;

    fn deref(&self) -> &Self::Target {
        &self.design
    }
}

/// An upsampling filter design, independent of the rate.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterDesign {
    /// A kernel run by the polyphase engines (windowed sinc, Lanczos or an
    /// optimal design).
    Kernel(KernelDesign),
    /// x3 stages first, then x2 stages, from the input side.
    Cascade(CascadeStage),
    /// A short interpolator.
    Interpolator(InterpMethod),
    /// Spectral zero-padding of Tukey-windowed blocks.
    BlockFft(BlockFftDesign),
}

impl FilterDesign {
    /// The runtime kind's name: `kernel`, `cascade`, `interpolator` or
    /// `block_fft`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Kernel(_) => "kernel",
            Self::Cascade(_) => "cascade",
            Self::Interpolator(_) => "interpolator",
            Self::BlockFft(_) => "block_fft",
        }
    }

    /// The phase character of the design: a kernel's phase; the
    /// minimum-phase and asymmetric cascades as minimum and intermediate;
    /// the other runtimes linear.
    pub fn filter_phase(&self) -> FilterPhase {
        match self {
            Self::Kernel(k) => match k.phase {
                Phase::Linear => FilterPhase::Linear,
                Phase::Minimum(_) => FilterPhase::Minimum,
                Phase::Intermediate { .. } => FilterPhase::Intermediate,
            },
            Self::Cascade(CascadeStage::FirAsymmetric) => FilterPhase::Intermediate,
            Self::Cascade(
                CascadeStage::FirMinimum | CascadeStage::Elliptic | CascadeStage::InverseChebyshev,
            ) => FilterPhase::Minimum,
            _ => FilterPhase::Linear,
        }
    }

    /// Whether the design uses an output-dependent automatic budget
    /// ([`FilterErrorBudget::MatchOutput`]).
    pub fn has_match_output(&self) -> bool {
        matches!(
            self,
            FilterDesign::Kernel(KernelDesign {
                shape: Shape::WindowedSinc {
                    length: Length::Auto(FilterErrorBudget::MatchOutput),
                    ..
                } | Shape::Optimal {
                    length: Length::Auto(FilterErrorBudget::MatchOutput),
                    ..
                },
                ..
            })
        )
    }

    /// The design with an output-dependent automatic budget
    /// ([`FilterErrorBudget::MatchOutput`]) resolved for `output`; an
    /// optimal design's to at most its method's depth
    /// ([`OptimalMethod::max_budget_db`]). A design without one is returned
    /// unchanged.
    pub fn with_output(&self, output: &OutputConfig) -> Self {
        let mut d = self.clone();
        let db = filter_error_budget_db(output);
        match &mut d {
            FilterDesign::Kernel(KernelDesign {
                shape:
                    Shape::WindowedSinc {
                        length: Length::Auto(budget @ FilterErrorBudget::MatchOutput),
                        ..
                    },
                ..
            }) => *budget = FilterErrorBudget::Db(db),
            FilterDesign::Kernel(KernelDesign {
                shape:
                    Shape::Optimal {
                        method,
                        length: Length::Auto(budget @ FilterErrorBudget::MatchOutput),
                        ..
                    },
                ..
            }) => *budget = FilterErrorBudget::Db(db.min(method.max_budget_db())),
            _ => {}
        }
        d
    }
}

/// A kernel design: windowed sinc or Lanczos, sharing the phase, rate rules
/// and arithmetic path.
#[derive(Clone, Debug, PartialEq)]
pub struct KernelDesign {
    pub shape: Shape,
    /// How a decimating ratio scales the length.
    pub decimation: Decimation,
    pub phase: Phase,
    /// Force the centre branch to exactly 1 and 0 (linear phase, cutoff
    /// 1.0; the `standard` path only, no preset uses it).
    pub preserve_samples: bool,
    pub rates: Rates,
    pub numerics: Numerics,
}

/// What a kernel is made of.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// `sinc(fc k / L) * w[n]`.
    WindowedSinc {
        window: Window,
        length: Length,
        cutoff: Cutoff,
    },
    /// `sinc(x) sinc(x / a)` over exactly `+-a` lobes: `2a + 1` taps per
    /// input sample, no separate cutoff.
    Lanczos { lobes: usize },
    /// A linear-phase low-pass designed directly for its band (no window):
    /// unit gain up to `passband_end`, zero from `stopband_start` (fractions
    /// of the lower Nyquist), equal weight on both errors. The length is
    /// `fixed`, `per_input` or `auto` (the shortest that keeps the budget
    /// over the band); an even length is designed one tap shorter (odd,
    /// symmetric about its centre tap).
    Optimal {
        method: OptimalMethod,
        length: Length,
        passband_end: f64,
        stopband_start: f64,
    },
}

/// How an [`Shape::Optimal`] kernel is designed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum OptimalMethod {
    /// Parks-McClellan: the smallest largest error in either band (the
    /// errors ripple at one level). Short kernels only.
    #[default]
    Equiripple,
    /// The smallest summed squared error over both bands: its largest error
    /// sits at the band edges and falls away from them. Longer kernels.
    LeastSquares,
}

/// The deepest automatic budget of an equiripple design, dB (f64 levels its
/// ripple to about 1e-10 on short kernels).
pub const EQUIRIPPLE_MAX_DB: f64 = 150.0;

/// The deepest automatic budget of a least-squares design, dB (its solve
/// floors at 137 to 153 dB).
pub const LEAST_SQUARES_MAX_DB: f64 = 135.0;

impl OptimalMethod {
    /// The deepest automatic budget of the method, dB: a `match_output`
    /// budget resolves to at most this.
    pub fn max_budget_db(self) -> f64 {
        match self {
            OptimalMethod::Equiripple => EQUIRIPPLE_MAX_DB,
            OptimalMethod::LeastSquares => LEAST_SQUARES_MAX_DB,
        }
    }
}

/// A window with its sampling convention.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "WireWindow"))]
pub enum Window {
    /// `beta: None` derives it from the band attenuation (Kaiser's
    /// formula; the `standard` path's band and automatic lengths).
    Kaiser {
        beta: Option<f64>,
        sampling: Sampling,
    },
    /// `alpha`: sigma as a fraction of the integer half length.
    Gaussian {
        alpha: f64,
        sampling: Sampling,
    },
    /// A Gaussian cut where it has fallen to `-(A + 10)` dB, with `A =
    /// kaiser_attenuation_db(beta)`: the `standard` path's Gaussian. Serializes
    /// on the wire as `stopband_db`; `None` derives it from the band
    /// attenuation.
    GaussianStopband {
        beta: Option<f64>,
        sampling: Sampling,
    },
    Flattop {
        set: FlattopSet,
        sampling: Sampling,
    },
    /// Dolph-Chebyshev, sidelobes at `20 log10(sidelobe_ratio)` dB.
    Chebyshev {
        sidelobe_ratio: f64,
        sampling: Sampling,
    },
    /// 4-term Nuttall.
    Nuttall {
        sampling: Sampling,
    },
    Hann {
        sampling: Sampling,
    },
    /// Flat over `1 - taper` of the length, cosine tapers at the ends.
    Tukey {
        taper: f64,
        sampling: Sampling,
    },
    /// Discrete prolate spheroidal (Slepian): the most energy within
    /// `nw / N` cycles per sample of any N-tap window (Kaiser approximates
    /// it with `beta = pi nw`). `nw: None` derives it from the band
    /// attenuation, as a Kaiser beta.
    Dpss {
        nw: Option<f64>,
        sampling: Sampling,
    },
}

/// How a window is sampled over the N taps of the kernel (n = 0 .. N-1).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Sampling {
    /// Divides by N.
    Periodic,
    /// Divides by N - 1, centred on `(N - 1) / 2`.
    #[default]
    Symmetric,
    /// Gaussian centred on `floor((N - 1) / 2)`.
    IntegerCentre,
}

/// The published flat-top coefficient sets (Heinzel, Rüdiger and Schilling).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FlattopSet {
    Hft196d,
    Hft223d,
    Hft248d,
}

/// A value that may depend on the source rate: `[upper_bound_exclusive,
/// value]` steps, the last one unbounded.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "WireStepped<T>"))]
pub enum Stepped<T> {
    Fixed(T),
    BySourceRate(Vec<(Option<u32>, T)>),
}

impl<T: Copy> Stepped<T> {
    /// The value at `source_rate`: the first step whose bound is above it
    /// (or unbounded). `None` when no step applies.
    pub fn at(&self, source_rate: u32) -> Option<T> {
        match self {
            Self::Fixed(v) => Some(*v),
            Self::BySourceRate(steps) => steps
                .iter()
                .find(|(bound, _)| bound.is_none_or(|b| source_rate < b))
                .map(|&(_, v)| v),
        }
    }
}

/// Kernel length rule of a windowed sinc.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "WireLength"))]
pub enum Length {
    /// Frames at the output rate: `taps * M` taps on the grid (`taps` at
    /// an integer ratio).
    Fixed(usize),
    /// Taps per input sample: `T * L` taps on the grid.
    PerInput(Stepped<usize>),
    /// A span at the output rate: `T = N / L - 1`.
    OutputSpan(Span),
    /// The shortest length whose window-formula transition fits between
    /// the band edges ([`Cutoff::Band`]).
    Band,
    /// The shortest measured length meeting the budget on the band edges
    /// ([`Cutoff::Band`]).
    Auto(FilterErrorBudget),
}

/// The output span of [`Length::OutputSpan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "WireSpan"))]
pub enum Span {
    /// The power-of-two ladder above the output rate (about 1.5 s).
    Ladder,
    /// A fixed number of output-rate taps.
    Taps(usize),
}

/// The cutoff of a windowed sinc, as fractions of the lower Nyquist.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "WireCutoff"))]
pub enum Cutoff {
    /// The -6 dB point.
    Fraction(Stepped<f64>),
    /// The cutoff that puts the window's stopband edge at the source Nyquist.
    NyquistStop,
    /// Passband end, stopband start and attenuation: the cutoff midway,
    /// the length from [`Length::Band`] or [`Length::Auto`].
    Band(BandSpec),
}

/// How a decimating ratio sets the length.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "length", rename_all = "snake_case")]
pub enum Decimation {
    /// `T' = ceil(base * M / L)`, made odd by `odd`, with `base =
    /// base_taps` or the design's own T.
    Scale {
        #[serde(default)]
        odd: OddRule,
        #[serde(default)]
        base_taps: Option<usize>,
    },
    /// T unscaled (the `standard` path: its lengths count frames).
    #[default]
    Keep,
}

/// How an even decimated length is made odd.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum OddRule {
    #[default]
    Down,
    Up,
}

/// The phase of a kernel.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, as = "WirePhase"))]
pub enum Phase {
    #[default]
    Linear,
    Minimum(PhaseMethod),
    /// `mix` counts toward linear phase: 0 is minimum, 1 linear.
    Intermediate {
        mix: f64,
        method: PhaseMethod,
    },
}

/// How a minimum or intermediate phase is made.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum PhaseMethod {
    /// The real-cepstrum transform of the phase presets.
    Cepstral,
    /// Checked zero reflection, the `standard` path's.
    #[default]
    Reflect,
}

/// Which ratios a design accepts, and the window it switches to at some.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Rates {
    pub accept: Accept,
    #[serde(default)]
    pub fallback_window: Option<Fallback>,
}

/// The ratio classes.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Accept {
    /// Integer upsampling only.
    Integer,
    /// Any output rate at or above the source rate.
    Upsampling,
    /// Any ratio, decimation included.
    #[default]
    Any,
}

/// A window that replaces the design's own when `when` holds.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Fallback {
    pub when: FallbackWhen,
    pub window: Window,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FallbackWhen {
    /// Neither rate is an integer multiple of the other.
    NotMultiple,
    /// The output rate is below the source rate.
    Decimating,
}

/// The arithmetic a kernel is built with.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Numerics {
    #[serde(default)]
    pub path: ArithmeticPath,
    #[serde(default)]
    pub grid_cutoff: GridCutoff,
}

impl Default for Numerics {
    fn default() -> Self {
        Self {
            path: ArithmeticPath::Standard,
            grid_cutoff: GridCutoff::DesignRatio,
        }
    }
}

/// One of the existing kernel builders, each kept with its exact arithmetic.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum ArithmeticPath {
    /// f64 windowed sinc, pairwise branch sum, normalised before the phase
    /// transform.
    Fir,
    /// As [`Self::Fir`], normalised after the phase transform.
    FirAfter,
    /// Fixed-order f64 sinc `k ((fc pi)(1 / L))`, extended-precision
    /// window, sequential branch sum, normalised after the transform.
    ExactPiInverseAfter,
    /// Fixed-order f64 sinc `(k (fc / L)) pi`, extended-precision window,
    /// sequential branch sum, normalised before the transform.
    ExactOverRatioBefore,
    /// As [`Self::ExactPiInverseAfter`], with the pairwise branch sum.
    ExactPiInversePairwise,
    /// Gaussian window and sinc in double-double, rounded once, pairwise
    /// branch sum after the transform.
    DoubleDouble,
    /// The Custom builder: symmetric windows, the exact centre, a sequential
    /// `R / sum` scale before and after the reflected phase.
    #[default]
    Standard,
}

/// How the cutoff is placed on a rational grid.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum GridCutoff {
    /// When decimating, the cutoff times `out / in` (rounded to f64); the
    /// sinc divisor is the branch count L.
    Scaled,
    /// The cutoff unchanged on `R = max(L, M)`: the kernel is designed as
    /// at the integer ratio R, then every tap scaled by `L / R`.
    #[default]
    DesignRatio,
}

/// The cascade stage filters.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CascadeStage {
    Fir,
    FirMinimum,
    FirAsymmetric,
    FrequencySampled,
    Elliptic,
    InverseChebyshev,
}

/// The short interpolators.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum InterpMethod {
    Nos,
    Cosine,
    CatmullRom,
    TableSinc,
}

/// A block FFT runtime.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BlockFftDesign {
    pub block: BlockSize,
    pub overlap: Overlap,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum BlockSize {
    /// B input frames.
    Input(usize),
    /// P output frames (`B = P / ratio`).
    Output(usize),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    /// Hop B/2.
    Half,
    /// Hop B/4, overlapping blocks averaged.
    QuarterAverage,
}

// ---------------------------------------------------------- checks and plans

/// A design field that is out of bounds or that refuses the design: the
/// path from the design (`design.window.beta`, `design.length`, `target`)
/// and the message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for FieldError {}

/// What a design costs at one source rate and ratio, before (or, in
/// `upsampler_response`, after) its kernel is designed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DesignPlan {
    /// L: polyphase branches (the grid runs at the source rate times L).
    pub grid_up: u32,
    /// M: the grid is decimated by M to the output rate (1 at an integer
    /// ratio).
    pub grid_down: u32,
    /// Kernel taps on the grid (0 for cascade, interpolator and block FFT).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub grid_taps: usize,
    /// `grid_taps` is exact; false when it is an upper bound (a reflected
    /// intermediate phase) or the estimate of an automatic length before its
    /// search.
    pub grid_taps_exact: bool,
    /// Peak memory while the design is built (an upper bound), bytes.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub workspace_bytes: u64,
    /// Memory the running filter keeps, both channels (an estimate), bytes.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub resident_bytes: u64,
    /// Fraction of one core one pipeline (both channels) takes on this
    /// machine, as the admission check prices it; absent when it cannot be
    /// priced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub cpu_load: Option<f64>,
    /// Bounded-input peak: the largest output a full-scale input can produce
    /// (the largest polyphase branch's L1 norm), dB. Only once the kernel is
    /// designed (`upsampler_response`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub bounded_peak_db: Option<f64>,
}

// ---------------------------------------------------------------- wire serde

/// Default `rates.accept` for a kernel design when omitted on the wire:
/// `integer` for `output_span`, `upsampling` for stepped values, `lanczos` and
/// `preserve_samples`, `any` otherwise.
fn default_accept(shape: &Shape, preserve_samples: bool) -> Accept {
    match shape {
        Shape::Lanczos { .. } => Accept::Upsampling,
        Shape::Optimal { length, .. } => {
            if matches!(length, Length::PerInput(Stepped::BySourceRate(_))) {
                Accept::Upsampling
            } else {
                Accept::Any
            }
        }
        Shape::WindowedSinc { length, cutoff, .. } => {
            if matches!(length, Length::OutputSpan(_)) {
                Accept::Integer
            } else if preserve_samples
                || matches!(length, Length::PerInput(Stepped::BySourceRate(_)))
                || matches!(cutoff, Cutoff::Fraction(Stepped::BySourceRate(_)))
            {
                Accept::Upsampling
            } else {
                Accept::Any
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
enum WireStepped<T> {
    Fixed(T),
    BySourceRate {
        by_source_rate: Vec<(Option<u32>, T)>,
    },
}

impl<T: Serialize> Serialize for Stepped<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Fixed(v) => v.serialize(serializer),
            Self::BySourceRate(steps) => {
                #[derive(Serialize)]
                struct Obj<'a, T> {
                    by_source_rate: &'a [(Option<u32>, T)],
                }
                Obj {
                    by_source_rate: steps,
                }
                .serialize(serializer)
            }
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Stepped<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match WireStepped::<T>::deserialize(deserializer)? {
            WireStepped::Fixed(v) => Ok(Self::Fixed(v)),
            WireStepped::BySourceRate { by_source_rate } => Ok(Self::BySourceRate(by_source_rate)),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(untagged)]
enum WireSpan {
    Ladder(LadderTag),
    Taps(usize),
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
enum LadderTag {
    Ladder,
}

impl Serialize for Span {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match *self {
            Self::Ladder => "ladder".serialize(serializer),
            Self::Taps(n) => n.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match WireSpan::deserialize(deserializer)? {
            WireSpan::Ladder(LadderTag::Ladder) => Ok(Self::Ladder),
            WireSpan::Taps(n) => Ok(Self::Taps(n)),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "rule", rename_all = "snake_case")]
enum WireLength {
    Fixed { taps: usize },
    PerInput { taps: Stepped<usize> },
    OutputSpan { taps: Span },
    Band,
    Auto { budget: FilterErrorBudget },
}

impl Serialize for Length {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let w = match self {
            Self::Fixed(taps) => WireLength::Fixed { taps: *taps },
            Self::PerInput(taps) => WireLength::PerInput { taps: taps.clone() },
            Self::OutputSpan(taps) => WireLength::OutputSpan { taps: *taps },
            Self::Band => WireLength::Band,
            Self::Auto(budget) => WireLength::Auto { budget: *budget },
        };
        w.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Length {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match WireLength::deserialize(deserializer)? {
            WireLength::Fixed { taps } => Self::Fixed(taps),
            WireLength::PerInput { taps } => Self::PerInput(taps),
            WireLength::OutputSpan { taps } => Self::OutputSpan(taps),
            WireLength::Band => Self::Band,
            WireLength::Auto { budget } => Self::Auto(budget),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "snake_case")]
enum WireCutoff {
    Fraction { value: Stepped<f64> },
    NyquistStop,
    Band(BandSpec),
}

impl Serialize for Cutoff {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let w = match self {
            Self::Fraction(value) => WireCutoff::Fraction {
                value: value.clone(),
            },
            Self::NyquistStop => WireCutoff::NyquistStop,
            Self::Band(b) => WireCutoff::Band(*b),
        };
        w.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Cutoff {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match WireCutoff::deserialize(deserializer)? {
            WireCutoff::Fraction { value } => Self::Fraction(value),
            WireCutoff::NyquistStop => Self::NyquistStop,
            WireCutoff::Band(b) => Self::Band(b),
        })
    }
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
enum WirePhase {
    Linear,
    Minimum {
        #[serde(default)]
        method: PhaseMethod,
    },
    Intermediate {
        mix: f64,
        #[serde(default)]
        method: PhaseMethod,
    },
}

impl Serialize for Phase {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let w = match *self {
            Self::Linear => WirePhase::Linear,
            Self::Minimum(method) => WirePhase::Minimum { method },
            Self::Intermediate { mix, method } => WirePhase::Intermediate { mix, method },
        };
        w.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Phase {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match WirePhase::deserialize(deserializer)? {
            WirePhase::Linear => Self::Linear,
            WirePhase::Minimum { method } => Self::Minimum(method),
            WirePhase::Intermediate { mix, method } => Self::Intermediate { mix, method },
        })
    }
}

/// Wire shape of [`Window`] (used for serialization and TypeScript generation).
#[derive(Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireWindow {
    Kaiser {
        beta: Option<f64>,
        sampling: Sampling,
    },
    #[serde(rename = "gaussian")]
    GaussianAlpha {
        alpha: f64,
        sampling: Sampling,
    },
    #[serde(rename = "gaussian")]
    GaussianStopband {
        stopband_db: Option<f64>,
        sampling: Sampling,
    },
    Flattop {
        set: FlattopSet,
        sampling: Sampling,
    },
    Chebyshev {
        sidelobe_ratio: f64,
        sampling: Sampling,
    },
    Nuttall {
        sampling: Sampling,
    },
    Hann {
        sampling: Sampling,
    },
    Tukey {
        taper: f64,
        sampling: Sampling,
    },
    Dpss {
        nw: Option<f64>,
        sampling: Sampling,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawWindow {
    Kaiser {
        #[serde(default)]
        beta: Option<f64>,
        #[serde(default)]
        sampling: Sampling,
    },
    Gaussian {
        #[serde(default)]
        alpha: Option<f64>,
        #[serde(default)]
        stopband_db: Option<f64>,
        #[serde(default)]
        sampling: Sampling,
    },
    Flattop {
        set: FlattopSet,
        #[serde(default)]
        sampling: Sampling,
    },
    Chebyshev {
        sidelobe_ratio: f64,
        #[serde(default)]
        sampling: Sampling,
    },
    Nuttall {
        #[serde(default)]
        sampling: Sampling,
    },
    Hann {
        #[serde(default)]
        sampling: Sampling,
    },
    Tukey {
        taper: f64,
        #[serde(default)]
        sampling: Sampling,
    },
    Dpss {
        #[serde(default)]
        nw: Option<f64>,
        #[serde(default)]
        sampling: Sampling,
    },
}

impl Serialize for Window {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let out = match *self {
            Self::Kaiser { beta, sampling } => WireWindow::Kaiser { beta, sampling },
            Self::Gaussian { alpha, sampling } => WireWindow::GaussianAlpha { alpha, sampling },
            Self::GaussianStopband { beta, sampling } => WireWindow::GaussianStopband {
                stopband_db: beta.map(beta_to_stopband_db),
                sampling,
            },
            Self::Flattop { set, sampling } => WireWindow::Flattop { set, sampling },
            Self::Chebyshev {
                sidelobe_ratio,
                sampling,
            } => WireWindow::Chebyshev {
                sidelobe_ratio,
                sampling,
            },
            Self::Nuttall { sampling } => WireWindow::Nuttall { sampling },
            Self::Hann { sampling } => WireWindow::Hann { sampling },
            Self::Tukey { taper, sampling } => WireWindow::Tukey { taper, sampling },
            Self::Dpss { nw, sampling } => WireWindow::Dpss { nw, sampling },
        };
        out.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Window {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match RawWindow::deserialize(deserializer)? {
            RawWindow::Kaiser { beta, sampling } => Ok(Self::Kaiser { beta, sampling }),
            RawWindow::Gaussian {
                alpha,
                stopband_db,
                sampling,
            } => match (alpha, stopband_db) {
                (Some(_), Some(_)) => Err(de::Error::custom(
                    "gaussian window cannot set both alpha and stopband_db",
                )),
                (Some(alpha), None) => Ok(Self::Gaussian { alpha, sampling }),
                (None, stopband_db) => Ok(Self::GaussianStopband {
                    beta: stopband_db.map(stopband_db_to_beta),
                    sampling,
                }),
            },
            RawWindow::Flattop { set, sampling } => Ok(Self::Flattop { set, sampling }),
            RawWindow::Chebyshev {
                sidelobe_ratio,
                sampling,
            } => Ok(Self::Chebyshev {
                sidelobe_ratio,
                sampling,
            }),
            RawWindow::Nuttall { sampling } => Ok(Self::Nuttall { sampling }),
            RawWindow::Hann { sampling } => Ok(Self::Hann { sampling }),
            RawWindow::Tukey { taper, sampling } => Ok(Self::Tukey { taper, sampling }),
            RawWindow::Dpss { nw, sampling } => Ok(Self::Dpss { nw, sampling }),
        }
    }
}

/// The optimal designs' default numerics: the `fir` path (they are not on
/// the `standard` path), the `scaled` grid cutoff.
fn fir_numerics() -> Numerics {
    Numerics {
        path: ArithmeticPath::Fir,
        grid_cutoff: GridCutoff::Scaled,
    }
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireDesignKind {
    WindowedSinc {
        window: Window,
        length: Length,
        #[serde(default)]
        decimation: Decimation,
        cutoff: Cutoff,
        #[serde(default)]
        phase: Phase,
        #[serde(default)]
        preserve_samples: bool,
        #[serde(default)]
        rates: Option<Rates>,
        #[serde(default)]
        numerics: Numerics,
    },
    Lanczos {
        lobes: usize,
        #[serde(default)]
        decimation: Decimation,
        #[serde(default)]
        phase: Phase,
        #[serde(default)]
        preserve_samples: bool,
        #[serde(default)]
        rates: Option<Rates>,
        #[serde(default)]
        numerics: Numerics,
    },
    Optimal {
        method: OptimalMethod,
        length: Length,
        passband_end: f64,
        stopband_start: f64,
        #[serde(default)]
        decimation: Decimation,
        #[serde(default)]
        phase: Phase,
        /// Always false (an optimal low-pass is not a Nyquist filter); on
        /// the wire so the design kinds share their fields.
        #[serde(default)]
        preserve_samples: bool,
        #[serde(default)]
        rates: Option<Rates>,
        #[serde(default = "fir_numerics")]
        numerics: Numerics,
    },
    Cascade {
        stage: CascadeStage,
    },
    Interpolator {
        method: InterpMethod,
    },
    BlockFft {
        block: BlockSize,
        overlap: Overlap,
    },
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
struct WireFilterDesign {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    based_on: Option<String>,
    #[serde(flatten)]
    kind: WireDesignKind,
}

impl From<&FilterDesign> for WireDesignKind {
    fn from(d: &FilterDesign) -> Self {
        match d {
            FilterDesign::Kernel(k) => match &k.shape {
                Shape::WindowedSinc {
                    window,
                    length,
                    cutoff,
                } => Self::WindowedSinc {
                    window: *window,
                    length: length.clone(),
                    decimation: k.decimation,
                    cutoff: cutoff.clone(),
                    phase: k.phase,
                    preserve_samples: k.preserve_samples,
                    rates: Some(k.rates),
                    numerics: k.numerics,
                },
                Shape::Optimal {
                    method,
                    length,
                    passband_end,
                    stopband_start,
                } => Self::Optimal {
                    method: *method,
                    length: length.clone(),
                    passband_end: *passband_end,
                    stopband_start: *stopband_start,
                    decimation: k.decimation,
                    phase: k.phase,
                    preserve_samples: k.preserve_samples,
                    rates: Some(k.rates),
                    numerics: k.numerics,
                },
                Shape::Lanczos { lobes } => Self::Lanczos {
                    lobes: *lobes,
                    decimation: k.decimation,
                    phase: k.phase,
                    preserve_samples: k.preserve_samples,
                    rates: Some(k.rates),
                    numerics: k.numerics,
                },
            },
            FilterDesign::Cascade(stage) => Self::Cascade { stage: *stage },
            FilterDesign::Interpolator(method) => Self::Interpolator { method: *method },
            FilterDesign::BlockFft(b) => Self::BlockFft {
                block: b.block,
                overlap: b.overlap,
            },
        }
    }
}

impl From<WireDesignKind> for FilterDesign {
    fn from(w: WireDesignKind) -> Self {
        match w {
            WireDesignKind::WindowedSinc {
                window,
                length,
                decimation,
                cutoff,
                phase,
                preserve_samples,
                rates,
                numerics,
            } => {
                let shape = Shape::WindowedSinc {
                    window,
                    length,
                    cutoff,
                };
                let rates = rates.unwrap_or_else(|| Rates {
                    accept: default_accept(&shape, preserve_samples),
                    fallback_window: None,
                });
                FilterDesign::Kernel(KernelDesign {
                    shape,
                    decimation,
                    phase,
                    preserve_samples,
                    rates,
                    numerics,
                })
            }
            WireDesignKind::Lanczos {
                lobes,
                decimation,
                phase,
                preserve_samples,
                rates,
                numerics,
            } => {
                let shape = Shape::Lanczos { lobes };
                let rates = rates.unwrap_or_else(|| Rates {
                    accept: default_accept(&shape, preserve_samples),
                    fallback_window: None,
                });
                FilterDesign::Kernel(KernelDesign {
                    shape,
                    decimation,
                    phase,
                    preserve_samples,
                    rates,
                    numerics,
                })
            }
            WireDesignKind::Optimal {
                method,
                length,
                passband_end,
                stopband_start,
                decimation,
                phase,
                preserve_samples,
                rates,
                numerics,
            } => {
                let shape = Shape::Optimal {
                    method,
                    length,
                    passband_end,
                    stopband_start,
                };
                let rates = rates.unwrap_or_else(|| Rates {
                    accept: default_accept(&shape, false),
                    fallback_window: None,
                });
                FilterDesign::Kernel(KernelDesign {
                    shape,
                    decimation,
                    phase,
                    preserve_samples,
                    rates,
                    numerics,
                })
            }
            WireDesignKind::Cascade { stage } => FilterDesign::Cascade(stage),
            WireDesignKind::Interpolator { method } => FilterDesign::Interpolator(method),
            WireDesignKind::BlockFft { block, overlap } => {
                FilterDesign::BlockFft(BlockFftDesign { block, overlap })
            }
        }
    }
}

impl Serialize for FilterDesign {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireDesignKind::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FilterDesign {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(WireDesignKind::deserialize(deserializer)?.into())
    }
}

impl Serialize for StoredDesign {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireFilterDesign {
            based_on: self.based_on.clone(),
            kind: WireDesignKind::from(&self.design),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StoredDesign {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let w = WireFilterDesign::deserialize(deserializer)?;
        Ok(Self {
            based_on: w.based_on,
            design: w.kind.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An optimal design's `match_output` resolves to at most its method's
    /// depth; a windowed sinc's to the output's own.
    #[test]
    fn match_output_is_clamped_for_optimal_designs() {
        let json = |method: &str| {
            format!(
                r#"{{
                "kind": "optimal",
                "method": "{method}",
                "length": {{ "rule": "auto", "budget": "match_output" }},
                "passband_end": 0.9,
                "stopband_start": 1.0,
                "phase": {{ "type": "linear" }}
            }}"#
            )
        };
        let out = OutputConfig {
            bits: 32,
            dither: crate::DitherKind::Tpdf,
            headroom_db: 0.0,
        };
        for (method, want) in [("least_squares", 135.0), ("equiripple", 141.4688)] {
            let d: StoredDesign = serde_json::from_str(&json(method)).unwrap();
            let FilterDesign::Kernel(KernelDesign {
                shape:
                    Shape::Optimal {
                        length: Length::Auto(FilterErrorBudget::Db(a)),
                        ..
                    },
                ..
            }) = d.design.with_output(&out)
            else {
                panic!("not resolved");
            };
            assert!((a - want).abs() < 1e-3, "{method}: {a}");
        }
    }

    #[test]
    fn spec_examples_round_trip() {
        let flattop_long = r#"{
            "based_on": "flattop_long",
            "kind": "windowed_sinc",
            "window": { "type": "flattop", "set": "hft248d", "sampling": "periodic" },
            "length": { "rule": "per_input", "taps": 511 },
            "decimation": { "length": "scale", "odd": "down", "base_taps": null },
            "cutoff": { "mode": "fraction", "value": 0.9302721088435374 },
            "phase": { "type": "linear" },
            "preserve_samples": false,
            "rates": { "accept": "any", "fallback_window": null },
            "numerics": { "path": "fir", "grid_cutoff": "scaled" }
        }"#;
        let parsed: StoredDesign = serde_json::from_str(flattop_long).unwrap();
        assert_eq!(parsed.based_on.as_deref(), Some("flattop_long"));
        let back: StoredDesign =
            serde_json::from_value(serde_json::to_value(&parsed).unwrap()).unwrap();
        assert_eq!(back, parsed);

        let chebyshev_mp = r#"{
            "based_on": "chebyshev_mp",
            "kind": "windowed_sinc",
            "window": { "type": "chebyshev", "sidelobe_ratio": 2.3283064365386963e-10, "sampling": "symmetric" },
            "length": { "rule": "per_input", "taps": 117 },
            "decimation": { "length": "scale", "odd": "up", "base_taps": 191 },
            "cutoff": { "mode": "fraction", "value": 0.953514739229025 },
            "phase": { "type": "minimum", "method": "cepstral" },
            "preserve_samples": false,
            "rates": {
                "accept": "any",
                "fallback_window": {
                    "when": "not_multiple",
                    "window": { "type": "kaiser", "beta": 21.991148575128552, "sampling": "periodic" }
                }
            },
            "numerics": { "path": "exact_pi_inverse_after", "grid_cutoff": "scaled" }
        }"#;
        let parsed: StoredDesign = serde_json::from_str(chebyshev_mp).unwrap();
        let back: StoredDesign =
            serde_json::from_value(serde_json::to_value(&parsed).unwrap()).unwrap();
        assert_eq!(back, parsed);

        let recommended = r#"{
            "based_on": "recommended",
            "kind": "windowed_sinc",
            "window": { "type": "kaiser", "beta": null, "sampling": "symmetric" },
            "length": { "rule": "auto", "budget": "match_output" },
            "decimation": { "length": "keep" },
            "cutoff": { "mode": "band", "passband_end": 0.9, "stopband_start": 1.0, "attenuation_db": 150.0 },
            "phase": { "type": "minimum", "method": "reflect" },
            "preserve_samples": false,
            "rates": { "accept": "any", "fallback_window": null },
            "numerics": { "path": "standard", "grid_cutoff": "design_ratio" }
        }"#;
        let parsed: StoredDesign = serde_json::from_str(recommended).unwrap();
        assert!(parsed.has_match_output());
        let back: StoredDesign =
            serde_json::from_value(serde_json::to_value(&parsed).unwrap()).unwrap();
        assert_eq!(back, parsed);

        // Omitted optional fields take defaults (including derived rates.accept).
        let minimal = r#"{
            "based_on": "chebyshev_long_ip",
            "kind": "windowed_sinc",
            "window": { "type": "chebyshev", "sidelobe_ratio": 1e-8, "sampling": "symmetric" },
            "length": { "rule": "auto", "budget": { "db": 150.0 } },
            "cutoff": { "mode": "band", "passband_end": 0.9, "stopband_start": 1.0, "attenuation_db": 150.0 },
            "phase": { "type": "intermediate", "mix": 0.6, "method": "cepstral" },
            "numerics": { "path": "fir_after", "grid_cutoff": "scaled" }
        }"#;
        let parsed: StoredDesign = serde_json::from_str(minimal).unwrap();
        let FilterDesign::Kernel(k) = &parsed.design else {
            panic!("expected kernel");
        };
        assert_eq!(k.decimation, Decimation::Keep);
        assert!(!k.preserve_samples);
        assert_eq!(k.rates.accept, Accept::Any);
        assert!(k.rates.fallback_window.is_none());
    }

    #[test]
    fn stepped_span_runtime_and_gaussian_wire_shapes() {
        let stepped: Stepped<usize> =
            serde_json::from_str(r#"{"by_source_rate":[[50000,167],[100000,83],[null,63]]}"#)
                .unwrap();
        assert_eq!(stepped.at(44_100), Some(167));
        assert_eq!(stepped.at(96_000), Some(83));
        assert_eq!(stepped.at(192_000), Some(63));
        assert_eq!(
            serde_json::to_value(&stepped).unwrap(),
            serde_json::json!({"by_source_rate": [[50000, 167], [100000, 83], [null, 63]]})
        );

        let fixed: Stepped<usize> = serde_json::from_str("511").unwrap();
        assert_eq!(fixed, Stepped::Fixed(511));
        assert_eq!(
            serde_json::to_value(&fixed).unwrap(),
            serde_json::json!(511)
        );

        assert_eq!(
            serde_json::from_str::<Span>(r#""ladder""#).unwrap(),
            Span::Ladder
        );
        assert_eq!(
            serde_json::to_value(Span::Ladder).unwrap(),
            serde_json::json!("ladder")
        );
        assert_eq!(
            serde_json::from_str::<Span>("1048576").unwrap(),
            Span::Taps(1_048_576)
        );
        assert_eq!(
            serde_json::to_value(Span::Taps(1_048_576)).unwrap(),
            serde_json::json!(1_048_576)
        );

        // Runtime kinds.
        for json in [
            r#"{"kind":"cascade","stage":"fir_minimum"}"#,
            r#"{"kind":"interpolator","method":"table_sinc"}"#,
            r#"{"kind":"block_fft","block":{"input":16384},"overlap":"quarter_average"}"#,
            r#"{"kind":"block_fft","block":{"output":1048576},"overlap":"half"}"#,
        ] {
            let d: FilterDesign = serde_json::from_str(json).unwrap();
            assert_eq!(
                serde_json::to_value(&d).unwrap(),
                serde_json::from_str::<serde_json::Value>(json).unwrap()
            );
        }

        // Gaussian: alpha or stopband_db, never both.
        let g_alpha: Window = serde_json::from_str(
            r#"{"type":"gaussian","alpha":0.175,"sampling":"integer_centre"}"#,
        )
        .unwrap();
        assert_eq!(
            g_alpha,
            Window::Gaussian {
                alpha: 0.175,
                sampling: Sampling::IntegerCentre,
            }
        );
        let g_stop: Window = serde_json::from_str(
            r#"{"type":"gaussian","stopband_db":150.0,"sampling":"symmetric"}"#,
        )
        .unwrap();
        let Window::GaussianStopband { beta, sampling } = g_stop else {
            panic!("expected GaussianStopband");
        };
        assert_eq!(sampling, Sampling::Symmetric);
        assert!((beta_to_stopband_db(beta.unwrap()) - 150.0).abs() < 1e-12);
        let both: Result<Window, _> = serde_json::from_str(
            r#"{"type":"gaussian","alpha":0.175,"stopband_db":150.0,"sampling":"symmetric"}"#,
        );
        assert!(both.is_err());
    }
}
