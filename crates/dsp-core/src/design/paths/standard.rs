//! The `standard` arithmetic path: the Custom kernel builder.
//!
//! [`UpsamplerDesign`] is the path's resolved design at one ratio (taps on
//! the grid, Kaiser beta, cutoff, phase, window), [`try_design_kernel`]
//! the windowed sinc with its sequential `R / sum` normalisation before
//! and after the checked zero reflection, and [`try_design_kernel_for`]
//! the `L / R` scale of a rational grid (`grid_cutoff: design_ratio`).
//!
//! A [`KernelDesign`] on this path selects these builders. Its length and
//! cutoff rules (fixed or per-input taps, stop at Nyquist, band, the
//! measured automatic length of [`crate::design::auto`]) are evaluated by
//! the Custom builder's own code, [`UpsamplerDesign::try_for_ratio`],
//! which reads the flat Custom fields of an [`UpsamplerConfig`]:
//! [`legacy_config`] writes the design into them, and [`design_of`] is
//! the inverse.
//!
//! What the path accepts (anything else is
//! [`DesignError::Unsupported`]):
//!
//! | Field | Values |
//! |---|---|
//! | window | Kaiser, [`Window::GaussianStopband`] or Tukey, `symmetric` |
//! | length, cutoff | `fixed`/`per_input` with `fraction` or `nyquist_stop`; `band` or `auto` with `band` |
//! | beta | given with `fraction` and `nyquist_stop`, derived (`None`) with `band` and `auto`; Tukey takes none, so it needs linear phase with a `fraction` cutoff, or a `band` |
//! | phase | linear, or minimum/intermediate with `reflect` |
//! | decimation, rates, grid cutoff | `keep`; no fallback window; `design_ratio` |
//!
//! Delays: linear `floor((N - 1) / 2)` at integer ratios and exactly
//! `(N - 1) / 2` on rational grids, none for minimum phase, the largest
//! tap for intermediate phase.

pub(crate) mod combos;

use crate::design::budget;
use crate::design::phase::reflect::minimum_phase_contract;
use crate::design::{
    Accept, ArithmeticPath, BuildContext, Cutoff, Decimation, DesignError, FilterDesign,
    GridCutoff, Kernel, KernelDesign, Length, Numerics, Phase, PhaseMethod, Plan, Rates, Sampling,
    Shape, Stepped, Window,
};
use crate::resample::{BandError, band_design_for, kaiser_attenuation_db, nyquist_stop_cutoff_for};
use crate::window::{gaussian, kaiser, tukey};
use audivane_protocol::{
    BandSpec, CutoffMode, FilterErrorBudget, FilterPhase, FilterPreset, OutputConfig, RateRatio,
    RateTarget, StoredDesign, TapsUnit, UpsamplerConfig, WindowKind, filter_error_budget_db,
};
use std::f64::consts::PI;

/// Legacy flat manual-design fields used internally by the intact `standard`
/// path builder (`paths::standard`, `auto`, `resample`).
#[derive(Clone, Debug, PartialEq)]
pub struct ManualConfig {
    pub target: RateTarget,
    pub taps: usize,
    pub phase: FilterPhase,
    pub kaiser_beta: f64,
    pub cutoff: f64,
    pub cutoff_mode: CutoffMode,
    pub taps_unit: TapsUnit,
    pub phase_mix: f64,
    pub band: BandSpec,
    pub auto_taps: Option<FilterErrorBudget>,
    pub preserve_samples: bool,
    pub window: WindowKind,
    pub taper: f64,
    pub preset: Option<FilterPreset>,
}

impl Default for ManualConfig {
    fn default() -> Self {
        Self {
            target: RateTarget::Off,
            taps: 65_536,
            phase: FilterPhase::Linear,
            kaiser_beta: 14.0,
            cutoff: 1.0,
            cutoff_mode: CutoffMode::Fraction,
            taps_unit: TapsUnit::Output,
            phase_mix: 0.5,
            band: BandSpec::default(),
            auto_taps: None,
            preserve_samples: false,
            window: WindowKind::Kaiser,
            taper: 0.25,
            preset: None,
        }
    }
}

impl ManualConfig {
    /// Resolve an output-dependent budget before design or response caching.
    pub fn with_output(&self, output: &OutputConfig) -> Self {
        let mut resolved = self.clone();
        if self.auto_taps == Some(FilterErrorBudget::MatchOutput) {
            resolved.auto_taps = Some(FilterErrorBudget::Db(filter_error_budget_db(output)));
        }
        resolved
    }

    /// Kernel length at the output rate for `ratio` with the manual controls.
    pub fn manual_taps(&self, ratio: u32) -> usize {
        match self.taps_unit {
            TapsUnit::Output => self.taps,
            TapsUnit::Input => self.taps.saturating_mul(ratio.max(1) as usize),
        }
    }

    /// Kernel length on the intermediate grid for a rational `ratio`.
    pub fn manual_taps_for(&self, ratio: RateRatio) -> usize {
        match self.taps_unit {
            TapsUnit::Output => self.taps.saturating_mul(ratio.down as usize),
            TapsUnit::Input => self.taps.saturating_mul(ratio.up as usize),
        }
    }

    /// Convert to a protocol [`UpsamplerConfig`].
    pub fn into_upsampler_config(self) -> Result<UpsamplerConfig, DesignError> {
        if let Some(p) = self.preset {
            return Ok(UpsamplerConfig {
                target: self.target,
                preset: Some(p.name().to_string()),
                design: None,
            });
        }
        let design = FilterDesign::Kernel(design_of(&self)?);
        Ok(UpsamplerConfig {
            target: self.target,
            preset: None,
            design: Some(StoredDesign::new(design)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UpsamplerDesign {
    pub ratio: usize,
    /// Kernel length at the output rate.
    pub taps: usize,
    pub phase: FilterPhase,
    pub kaiser_beta: f64,
    /// Fraction of the input Nyquist.
    pub cutoff: f64,
    /// Phase blend for [`FilterPhase::Intermediate`]: 0 = minimum, 1 =
    /// linear. Ignored for the other phases.
    pub phase_mix: f64,
    /// Make the kernel a Nyquist (M-th band) filter that passes the input
    /// samples through exactly (linear phase, cutoff 1.0; see
    /// [`ManualConfig::preserve_samples`]).
    pub preserve_samples: bool,
    /// Window of the windowed sinc.
    pub window: WindowKind,
    /// Tukey taper fraction (see [`ManualConfig::taper`]).
    pub taper: f64,
}

impl UpsamplerDesign {
    /// A linear-phase design; the other fields can be set with struct
    /// update syntax (`UpsamplerDesign { phase: .., ..UpsamplerDesign::linear(r, n, b, c) }`).
    pub fn linear(ratio: usize, taps: usize, kaiser_beta: f64, cutoff: f64) -> Self {
        Self {
            ratio,
            taps,
            phase: FilterPhase::Linear,
            kaiser_beta,
            cutoff,
            phase_mix: 1.0,
            preserve_samples: false,
            window: WindowKind::Kaiser,
            taper: 0.25,
        }
    }

    /// The design `cfg` gives at `ratio`. [`CutoffMode::Band`] specs that
    /// cannot be designed are an error; the daemon validates them first.
    pub fn try_from_config(cfg: &ManualConfig, ratio: usize) -> Result<Self, BandError> {
        if cfg.auto_taps.is_some() {
            return crate::design::auto::automatic_design(cfg, ratio).map_err(BandError::Automatic);
        }
        let (taps, kaiser_beta, cutoff) = match cfg.cutoff_mode {
            CutoffMode::Fraction => (cfg.manual_taps(ratio as u32), cfg.kaiser_beta, cfg.cutoff),
            CutoffMode::NyquistStop => {
                let taps = cfg.manual_taps(ratio as u32);
                (
                    taps,
                    cfg.kaiser_beta,
                    nyquist_stop_cutoff_for(cfg.window, taps, ratio, cfg.kaiser_beta),
                )
            }
            CutoffMode::Band => {
                let b = band_design_for(cfg.window, &cfg.band, ratio)?;
                (b.taps, b.kaiser_beta, b.cutoff)
            }
        };
        Ok(Self {
            ratio,
            taps,
            phase: cfg.phase,
            kaiser_beta,
            cutoff,
            phase_mix: cfg.phase_mix,
            preserve_samples: cfg.preserve_samples,
            window: cfg.window,
            taper: cfg.taper,
        })
    }

    /// [`Self::try_from_config`]; panics on an invalid band spec.
    pub fn from_config(cfg: &ManualConfig, ratio: usize) -> Self {
        Self::try_from_config(cfg, ratio).unwrap_or_else(|e| panic!("upsampler design: {e}"))
    }

    /// The design `cfg` gives at any `ratio`, integer or rational.
    ///
    /// The kernel lives on the intermediate grid (rate `input x up`), and
    /// its band edge is the lower of the input and output Nyquists, so a
    /// rational design is the integer design at the [`design_ratio`]
    /// (grid rate / the lower rate): `cutoff` is a fraction of that lower
    /// Nyquist (for decimation the anti-alias cutoff scales by out/in),
    /// "stop at Nyquist" and band designs take their transition from it, and
    /// manual `taps` count output or input frames
    /// ([`ManualConfig::manual_taps_for`]). The returned `ratio` field is
    /// the design ratio and `taps` the length on the grid. Integer ratios
    /// give exactly [`Self::try_from_config`].
    pub fn try_for_ratio(cfg: &ManualConfig, ratio: RateRatio) -> Result<Self, BandError> {
        if ratio.is_integer() {
            return Self::try_from_config(cfg, ratio.up as usize);
        }
        if cfg.auto_taps.is_some() {
            return crate::design::auto::automatic_design_for(cfg, ratio)
                .map_err(BandError::Automatic);
        }
        let r = design_ratio(ratio);
        let (taps, kaiser_beta, cutoff) = match cfg.cutoff_mode {
            CutoffMode::Fraction => (cfg.manual_taps_for(ratio), cfg.kaiser_beta, cfg.cutoff),
            CutoffMode::NyquistStop => {
                let taps = cfg.manual_taps_for(ratio);
                (
                    taps,
                    cfg.kaiser_beta,
                    nyquist_stop_cutoff_for(cfg.window, taps, r, cfg.kaiser_beta),
                )
            }
            CutoffMode::Band => {
                let b = band_design_for(cfg.window, &cfg.band, r)?;
                (b.taps, b.kaiser_beta, b.cutoff)
            }
        };
        Ok(Self {
            ratio: r,
            taps,
            phase: cfg.phase,
            kaiser_beta,
            cutoff,
            phase_mix: cfg.phase_mix,
            preserve_samples: cfg.preserve_samples,
            window: cfg.window,
            taper: cfg.taper,
        })
    }
}

/// Intermediate grid rate over the lower of the input and output rates:
/// `max(up, down)` (the integer ratio itself when `down == 1`). Manual
/// designs at a rational ratio are shaped like integer designs at this
/// ratio.
pub fn design_ratio(ratio: RateRatio) -> usize {
    ratio.up.max(ratio.down) as usize
}

/// The kernel of `d` (from [`UpsamplerDesign::try_for_ratio`]) on the
/// intermediate grid of `ratio`, scaled so each of the `up` polyphase
/// branches sums to about 1 (unity passband gain). Integer ratios give
/// [`try_design_kernel`] unchanged.
pub fn try_design_kernel_for(
    d: &UpsamplerDesign,
    ratio: RateRatio,
) -> Result<Vec<f64>, crate::minphase::MinimumPhaseError> {
    let mut h = try_design_kernel(d)?;
    let up = ratio.up as usize;
    if up != d.ratio {
        let scale = up as f64 / d.ratio as f64;
        for v in &mut h {
            *v *= scale;
        }
    }
    Ok(h)
}

/// The kernel's own delay in grid samples: the centre for linear phase,
/// the impulse peak (the first largest |tap|) for minimum and intermediate
/// phase, as every other non-linear design reports.
pub(crate) fn kernel_delay_exact(kernel: &[f64], phase: FilterPhase) -> f64 {
    match phase {
        FilterPhase::Linear => kernel.len().saturating_sub(1) as f64 / 2.0,
        FilterPhase::Minimum | FilterPhase::Intermediate => peak_index(kernel) as f64,
    }
}

fn peak_index(kernel: &[f64]) -> usize {
    kernel
        .iter()
        .enumerate()
        .fold((0, 0.0_f64), |(bi, bv), (i, &v)| {
            if v.abs() > bv { (i, v.abs()) } else { (bi, bv) }
        })
        .0
}

/// Kernel at the output rate. Passband gain is `ratio` so that zero-stuffing
/// followed by this filter has unity passband gain. Minimum phase is the
/// linear-phase design with its zeros outside the unit circle reflected
/// inside (same length, same magnitude; [`crate::minphase`]).
///
/// Panics when a minimum-phase design fails its checks; the engine and the
/// response measurement use [`try_design_kernel`].
pub fn design_kernel(d: &UpsamplerDesign) -> Vec<f64> {
    try_design_kernel(d).unwrap_or_else(|e| panic!("upsampler kernel: {e}"))
}

/// [`design_kernel`], or why the minimum-phase twin could not be made to
/// its tolerances (the design is then rejected, never approximated).
pub fn try_design_kernel(
    d: &UpsamplerDesign,
) -> Result<Vec<f64>, crate::minphase::MinimumPhaseError> {
    assert!(d.ratio >= 1, "upsampler ratio must be >= 1");
    if d.taps == 0 {
        return Ok(Vec::new());
    }

    // Keeping the input samples needs the centre on a tap: odd length.
    let taps = if d.preserve_samples && d.taps.is_multiple_of(2) {
        d.taps - 1
    } else {
        d.taps
    };
    if taps == 0 {
        return Ok(Vec::new());
    }
    let d = &UpsamplerDesign { taps, ..*d };
    let r = d.ratio as f64;
    let win = match d.window {
        WindowKind::Kaiser => kaiser(d.taps, d.kaiser_beta),
        WindowKind::Gaussian => gaussian(d.taps, kaiser_attenuation_db(d.kaiser_beta)),
        WindowKind::Tukey => tukey(d.taps, d.taper),
    };
    let mut h = vec![0.0; d.taps];
    let center = (d.taps - 1) as f64 / 2.0;
    let half = d.taps.div_ceil(2);

    for k in 0..half {
        let t = k as f64 - center;
        let sinc_val = if t == 0.0 {
            1.0
        } else {
            let arg = PI * d.cutoff * t / r;
            arg.sin() / arg
        };
        let val = win[k] * d.cutoff * sinc_val;
        h[k] = val;
        h[d.taps - 1 - k] = val;
    }
    // Long kernels: do not keep a second full-length Vec alive through the
    // cepstral FFT below.
    drop(win);

    let sum: f64 = h.iter().sum();
    if sum.abs() > 0.0 {
        let scale = r / sum;
        for v in &mut h {
            *v *= scale;
        }
    }

    if d.preserve_samples && d.phase == FilterPhase::Linear {
        // A Nyquist filter: exactly 1 at the centre and 0 at every
        // `ratio`-th tap from it (the windowed sinc's zeros, which rounding
        // leaves at ~1e-17), so each input sample reappears unchanged. The
        // other polyphase branches keep their (unity-sum) shape.
        let c = (d.taps - 1) / 2;
        h[c] = 1.0;
        let mut k = d.ratio;
        while k <= c {
            h[c - k] = 0.0;
            h[c + k] = 0.0;
            k += d.ratio;
        }
        return Ok(h);
    }

    let twin = match d.phase {
        FilterPhase::Linear => None,
        // Same magnitude, zeros reflected inside the unit circle, checked;
        // see `crate::minphase` for the methods, tolerances and limits.
        FilterPhase::Minimum if d.taps > 1 => {
            Some(crate::minphase::minimum_phase(&h, &minimum_phase_contract(d))?.kernel)
        }
        // Same magnitude, phase blended; usually longer than `taps`.
        FilterPhase::Intermediate if d.taps > 1 && d.phase_mix < 1.0 => Some(
            crate::minphase::intermediate_phase(&h, &minimum_phase_contract(d), d.phase_mix)?
                .kernel,
        ),
        _ => None,
    };
    if let Some(twin) = twin {
        h = twin;
        let min_sum: f64 = h.iter().sum();
        if min_sum.abs() > 0.0 {
            let scale = r / min_sum;
            for v in &mut h {
                *v *= scale;
            }
        }
    }

    Ok(h)
}

/// The kernel's own delay in output frames at an integer ratio: the
/// centre `floor((N - 1) / 2)` for linear phase, the largest tap for
/// minimum and intermediate phase.
pub(crate) fn kernel_delay(kernel: &[f64], phase: FilterPhase) -> usize {
    match phase {
        FilterPhase::Linear => kernel.len().saturating_sub(1) / 2,
        FilterPhase::Minimum | FilterPhase::Intermediate => peak_index(kernel),
    }
}

// ------------------------------------------------- the path's design values

/// The refusal of a value the standard path does not build.
fn unsupported<T>(what: &str) -> Result<T, DesignError> {
    Err(DesignError::Unsupported(format!(
        "the standard path does not build {what} yet"
    )))
}

/// The standard design of a Custom config `cfg` (its flat manual fields;
/// `preset` and `target` are not part of a design).
/// An automatic length keeps its budget as it is: resolve
/// `match_output` first ([`ManualConfig::with_output`]), or the build
/// refuses it as the Custom builder always has.
///
/// Every config maps, except a Tukey window with a beta of its own: the
/// beta of a Tukey design only feeds Kaiser's formulas (stop at Nyquist,
/// the reflected phase's contract), so the design has no place for it,
/// and those configs (which the daemon refuses) are refused here.
pub fn design_of(cfg: &ManualConfig) -> Result<KernelDesign, DesignError> {
    let searched = cfg.auto_taps.is_some() || cfg.cutoff_mode == CutoffMode::Band;
    let beta = (!searched).then_some(cfg.kaiser_beta);
    let sampling = Sampling::Symmetric;
    let window = match cfg.window {
        WindowKind::Kaiser => Window::Kaiser { beta, sampling },
        WindowKind::Gaussian => Window::GaussianStopband { beta, sampling },
        WindowKind::Tukey => {
            if beta.is_some()
                && (cfg.phase != FilterPhase::Linear || cfg.cutoff_mode != CutoffMode::Fraction)
            {
                return Err(DesignError::Unsupported(
                    "the Tukey window needs linear phase with cutoff_mode fraction (its stopband \
                     has no calibrated formula)"
                        .into(),
                ));
            }
            Window::Tukey {
                taper: cfg.taper,
                sampling,
            }
        }
    };
    let manual = || match cfg.taps_unit {
        TapsUnit::Output => Length::Fixed(cfg.taps),
        TapsUnit::Input => Length::PerInput(Stepped::Fixed(cfg.taps)),
    };
    let (length, cutoff) = match (cfg.auto_taps, cfg.cutoff_mode) {
        (Some(budget), _) => (Length::Auto(budget), Cutoff::Band(cfg.band)),
        (None, CutoffMode::Band) => (Length::Band, Cutoff::Band(cfg.band)),
        (None, CutoffMode::NyquistStop) => (manual(), Cutoff::NyquistStop),
        (None, CutoffMode::Fraction) => (manual(), Cutoff::Fraction(Stepped::Fixed(cfg.cutoff))),
    };
    let phase = match cfg.phase {
        FilterPhase::Linear => Phase::Linear,
        FilterPhase::Minimum => Phase::Minimum(PhaseMethod::Reflect),
        FilterPhase::Intermediate => Phase::Intermediate {
            mix: cfg.phase_mix,
            method: PhaseMethod::Reflect,
        },
    };
    Ok(KernelDesign {
        shape: Shape::WindowedSinc {
            window,
            length,
            cutoff,
        },
        decimation: Decimation::Keep,
        phase,
        preserve_samples: cfg.preserve_samples,
        rates: Rates {
            accept: Accept::Any,
            fallback_window: None,
        },
        numerics: Numerics {
            path: ArithmeticPath::Standard,
            grid_cutoff: GridCutoff::DesignRatio,
        },
    })
}

/// The Custom config whose manual fields are `k` at `source_rate` (the
/// stepped values evaluated): what the Custom builder reads. The other
/// fields are the defaults, which the builder does not read.
pub fn legacy_config(k: &KernelDesign, source_rate: u32) -> Result<ManualConfig, DesignError> {
    if k.numerics
        != (Numerics {
            path: ArithmeticPath::Standard,
            grid_cutoff: GridCutoff::DesignRatio,
        })
    {
        return unsupported("this grid cutoff");
    }
    if k.decimation != Decimation::Keep {
        return unsupported("a scaled decimation length");
    }
    if k.rates.fallback_window.is_some() {
        return unsupported("a fallback window");
    }
    let Shape::WindowedSinc {
        window,
        length,
        cutoff,
    } = &k.shape
    else {
        return unsupported("a Lanczos kernel");
    };
    let mut cfg = ManualConfig {
        preset: None,
        preserve_samples: k.preserve_samples,
        ..ManualConfig::default()
    };
    let (window, beta) = match *window {
        Window::Kaiser {
            beta,
            sampling: Sampling::Symmetric,
        } => (WindowKind::Kaiser, beta),
        Window::GaussianStopband {
            beta,
            sampling: Sampling::Symmetric,
        } => (WindowKind::Gaussian, beta),
        Window::Tukey {
            taper,
            sampling: Sampling::Symmetric,
        } => {
            cfg.taper = taper;
            (WindowKind::Tukey, None)
        }
        other => return unsupported(&format!("{other:?}")),
    };
    cfg.window = window;
    (cfg.phase, cfg.phase_mix) = match k.phase {
        Phase::Linear => (FilterPhase::Linear, cfg.phase_mix),
        Phase::Minimum(PhaseMethod::Reflect) => (FilterPhase::Minimum, cfg.phase_mix),
        Phase::Intermediate {
            mix,
            method: PhaseMethod::Reflect,
        } => (FilterPhase::Intermediate, mix),
        _ => return unsupported("the cepstral phase method"),
    };
    let taps = |unit, t: &Stepped<usize>| match t.at(source_rate) {
        Some(t) => Ok((unit, t)),
        None => Err(DesignError::Unsupported(
            "the stepped length has no step for this source rate".into(),
        )),
    };
    let manual = match length {
        Length::Fixed(t) => Some((TapsUnit::Output, *t)),
        Length::PerInput(t) => Some(taps(TapsUnit::Input, t)?),
        _ => None,
    };
    // A given beta drives the fraction and stop-at-Nyquist designs; band
    // and automatic designs derive it from the attenuation.
    let needs_beta = matches!(cutoff, Cutoff::Fraction(_) | Cutoff::NyquistStop);
    match (beta, needs_beta, window) {
        (Some(b), true, _) => cfg.kaiser_beta = b,
        (None, false, _) => {}
        (None, true, WindowKind::Tukey)
            if k.phase == Phase::Linear && matches!(cutoff, Cutoff::Fraction(_)) => {}
        (None, true, WindowKind::Tukey) => {
            return unsupported("a Tukey window with stop at Nyquist or a reflected phase");
        }
        (None, true, _) => return unsupported("a fraction or stop-at-Nyquist cutoff without beta"),
        (Some(_), false, _) => return unsupported("a given beta with a band design"),
    }
    match (length, cutoff, manual) {
        (_, Cutoff::Fraction(fc), Some((unit, t))) => {
            let Some(fc) = fc.at(source_rate) else {
                return Err(DesignError::Unsupported(
                    "the stepped cutoff has no step for this source rate".into(),
                ));
            };
            (cfg.taps_unit, cfg.taps) = (unit, t);
            (cfg.cutoff_mode, cfg.cutoff) = (CutoffMode::Fraction, fc);
        }
        (_, Cutoff::NyquistStop, Some((unit, t))) => {
            (cfg.taps_unit, cfg.taps) = (unit, t);
            cfg.cutoff_mode = CutoffMode::NyquistStop;
        }
        (Length::Band, Cutoff::Band(band), _) => {
            (cfg.cutoff_mode, cfg.band) = (CutoffMode::Band, *band);
        }
        (Length::Auto(budget), Cutoff::Band(band), _) => {
            (cfg.cutoff_mode, cfg.band) = (CutoffMode::Band, *band);
            cfg.auto_taps = Some(*budget);
        }
        _ => return unsupported("this length rule with this cutoff"),
    }
    Ok(cfg)
}

/// The resolved design of `k` from `source_rate` at `ratio` (the length
/// rule evaluated, an automatic length searched), by the Custom builder.
pub(crate) fn recipe(
    k: &KernelDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<UpsamplerDesign, DesignError> {
    let cfg = legacy_config(k, source_rate)?;
    UpsamplerDesign::try_for_ratio(&cfg, ratio).map_err(|e| match &e {
        // A search candidate over the workspace limit.
        BandError::Automatic(m) => match budget::candidate_refusal(m) {
            Some(b) => DesignError::Budget(b),
            None => DesignError::Band(e),
        },
        _ => DesignError::Band(e),
    })
}

/// The kernel of the resolved design `d` on the grid of `ratio`.
pub(crate) fn kernel(d: &UpsamplerDesign, ratio: RateRatio) -> Result<Kernel, DesignError> {
    let taps = try_design_kernel_for(d, ratio).map_err(DesignError::Phase)?;
    Ok(Kernel {
        delay: kernel_delay(&taps, d.phase),
        delay_exact: kernel_delay_exact(&taps, d.phase),
        taps,
    })
}

/// The length budget for `taps` design taps in `cx`: at a rational ratio
/// the rational runtime's length and cost, at an integer ratio the kernel
/// length (a reflected intermediate phase a quarter of it).
fn check_budget(k: &KernelDesign, taps: usize, cx: &BuildContext) -> Result<(), DesignError> {
    if cx.ratio.is_integer() {
        let mixed = matches!(k.phase, Phase::Intermediate { mix, .. } if mix > 0.0 && mix < 1.0);
        return budget::check_kernel_taps(taps, cx.ratio, mixed);
    }
    budget::check_rational(
        taps,
        cx.ratio,
        cx.source_rate,
        budget::MAX_RATIONAL_GRID_TAPS,
        cx.pipelines,
    )
}

/// Refuse a resolved design whose kernel would be all zeros: a two-tap
/// Tukey window is zero at both ends (unless the input samples are kept,
/// which sets a tap). Caught here, before the safety gate refuses the
/// built kernel, so the rate mask never offers it.
fn check_design(r: &UpsamplerDesign) -> Result<(), DesignError> {
    if r.window == WindowKind::Tukey && r.taps == 2 && !r.preserve_samples {
        return Err(DesignError::Unsupported(
            "the Tukey window needs at least 3 taps: at 2 both taps are its zero ends".into(),
        ));
    }
    Ok(())
}

/// The rational budget for the final kernel length (a reflected
/// intermediate phase grows the kernel).
fn check_final(taps: usize, cx: &BuildContext) -> Result<(), DesignError> {
    if cx.ratio.is_integer() {
        return Ok(());
    }
    budget::check_rational(
        taps,
        cx.ratio,
        cx.source_rate,
        budget::MAX_RATIONAL_GRID_TAPS,
        cx.pipelines,
    )
}

/// The plan of a resolved design of `design_taps` taps in `cx`: its length
/// on the grid (four times it at most for a blended intermediate phase; a
/// mix of 0 is the minimum phase, 1 the linear one, both of the design
/// length), its construction workspace and its callback priced at
/// `priced_taps`.
pub(crate) fn plan(
    k: &KernelDesign,
    design_taps: usize,
    priced_taps: usize,
    cx: &BuildContext,
) -> Plan {
    // As the kernel builder does: a mix of 1 keeps the linear kernel, 0 is
    // the minimum phase (the delay convention stays the intermediate one).
    let workspace = match k.phase {
        Phase::Linear => budget::linear_workspace(design_taps),
        Phase::Intermediate { mix, .. } if mix >= 1.0 => budget::linear_workspace(design_taps),
        _ => budget::reflect_workspace(design_taps),
    };
    let mixed = matches!(k.phase, Phase::Intermediate { mix, .. } if mix > 0.0 && mix < 1.0);
    if mixed {
        Plan::kernel(cx, 4 * design_taps, false, workspace, priced_taps, false)
    } else {
        Plan::kernel(cx, design_taps, true, workspace, priced_taps, false)
    }
}

/// [`crate::design::feasibility`] of `k`, cheap: no search, no kernel.
///
/// - An automatic length gets the search's own preliminary checks (the
///   budget, the band, the window-formula estimate against the search's
///   limits) with the messages the search gives; its length is not known
///   until [`prepare`] has searched, so the rational budget is checked
///   there, and the plan holds the estimate.
/// - Any other length is resolved by its formula (fixed, stop at Nyquist,
///   band: arithmetic only), then at a rational ratio held to the budget.
///
/// The ratio rules come after the design's own errors, as the Custom
/// builder has always reported them.
///
/// `d` is `k` as a [`FilterDesign`] (for the ratio rules).
pub(crate) fn feasibility(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
) -> Result<Plan, DesignError> {
    let cfg = legacy_config(k, cx.source_rate)?;
    let check_ratio = || crate::design::rates::check(d, cx.ratio);
    if cfg.auto_taps.is_some() {
        // The checks the search starts with (`automatic_design` and
        // `automatic_design_for` refuse exactly these first).
        let estimate = if cx.ratio.is_integer() {
            crate::design::auto::automatic_estimate(&cfg, cx.ratio.up as usize)
        } else {
            crate::design::auto::automatic_estimate_for(&cfg, cx.ratio)
        };
        let estimate = estimate.map_err(|e| DesignError::Band(BandError::Automatic(e)))?;
        check_ratio()?;
        return Ok(Plan {
            grid_taps_exact: false,
            ..plan(k, estimate, estimate, cx)
        });
    }
    let r = UpsamplerDesign::try_for_ratio(&cfg, cx.ratio).map_err(DesignError::Band)?;
    check_design(&r)?;
    check_ratio()?;
    check_budget(k, r.taps, cx)?;
    plan(k, r.taps, budget::standard_priced_taps(k, &r), cx).check_workspace(cx)
}

/// [`crate::design::prepare`] of `k`, in the Custom builder's order: the
/// resolved design (an automatic length searched), the ratio rules, at a
/// rational ratio the budget for the design length, the kernel, and the
/// budget again for the final length (a reflected intermediate phase
/// grows the kernel). `d` is `k` as a [`FilterDesign`]. Returns the plan
/// of the resolved design and the kernel.
pub(crate) fn prepare(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
) -> Result<(Plan, Kernel), DesignError> {
    let (plan, r) = searched(d, k, cx)?;
    let kernel = kernel(&r, cx.ratio)?;
    check_final(kernel.taps.len(), cx)?;
    Ok((plan, kernel))
}

/// [`prepare`] up to the kernel: the resolved design (an automatic length
/// searched, cached), the ratio rules and the budgets for the design
/// length, and its plan, whose workspace includes the search's (what a
/// cold search was charged, [`crate::design::auto::search_workspace`]).
/// [`crate::design::searched_feasibility`] on the `standard` path.
pub(crate) fn searched(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
) -> Result<(Plan, UpsamplerDesign), DesignError> {
    let r = recipe(k, cx.source_rate, cx.ratio)?;
    check_design(&r)?;
    crate::design::rates::check(d, cx.ratio)?;
    check_budget(k, r.taps, cx)?;
    let mut plan = plan(k, r.taps, budget::standard_priced_taps(k, &r), cx);
    let cfg = legacy_config(k, cx.source_rate)?;
    plan.workspace_bytes = plan
        .workspace_bytes
        .max(crate::design::auto::search_workspace(&cfg, cx.ratio));
    Ok((plan.check_workspace(cx)?, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design;
    use audivane_protocol::{BandSpec, FilterErrorBudget};

    fn base() -> ManualConfig {
        ManualConfig {
            taps: 255,
            kaiser_beta: 12.0,
            cutoff: 0.95,
            ..ManualConfig::default()
        }
    }

    /// Custom configs over every length rule, cutoff mode, window and
    /// phase the path builds, small enough for a debug build.
    fn configs() -> Vec<(&'static str, ManualConfig)> {
        let band = BandSpec {
            passband_end: 0.9,
            stopband_start: 1.0,
            attenuation_db: 100.0,
        };
        vec![
            ("kaiser", base()),
            (
                "kaiser_even_input",
                ManualConfig {
                    taps: 32,
                    taps_unit: TapsUnit::Input,
                    ..base()
                },
            ),
            (
                "minimum",
                ManualConfig {
                    taps: 1024,
                    phase: FilterPhase::Minimum,
                    phase_mix: 0.3,
                    ..base()
                },
            ),
            (
                "intermediate",
                ManualConfig {
                    taps: 1024,
                    phase: FilterPhase::Intermediate,
                    phase_mix: 0.5,
                    ..base()
                },
            ),
            (
                "nyquist_stop",
                ManualConfig {
                    taps: 4096,
                    cutoff_mode: CutoffMode::NyquistStop,
                    ..base()
                },
            ),
            (
                "gaussian_band",
                ManualConfig {
                    window: WindowKind::Gaussian,
                    cutoff_mode: CutoffMode::Band,
                    band,
                    ..base()
                },
            ),
            (
                "gaussian_fraction_minimum",
                ManualConfig {
                    window: WindowKind::Gaussian,
                    taps: 1024,
                    phase: FilterPhase::Minimum,
                    ..base()
                },
            ),
            (
                "kaiser_band_minimum",
                ManualConfig {
                    cutoff_mode: CutoffMode::Band,
                    band,
                    phase: FilterPhase::Minimum,
                    ..base()
                },
            ),
            (
                "tukey_preserve",
                ManualConfig {
                    taps: 256,
                    window: WindowKind::Tukey,
                    taper: 0.4,
                    cutoff: 1.0,
                    preserve_samples: true,
                    ..base()
                },
            ),
            (
                "auto",
                ManualConfig {
                    auto_taps: Some(FilterErrorBudget::Db(100.0)),
                    band,
                    phase: FilterPhase::Minimum,
                    // Ignored by an automatic design.
                    cutoff_mode: CutoffMode::NyquistStop,
                    taper: 0.7,
                    ..base()
                },
            ),
        ]
    }

    /// What the Custom builder gives: the resolved design, its kernel and
    /// the delay the runtime received.
    fn legacy(cfg: &ManualConfig, ratio: RateRatio) -> (Vec<f64>, usize, f64) {
        let d = UpsamplerDesign::try_for_ratio(cfg, ratio).unwrap();
        let h = try_design_kernel_for(&d, ratio).unwrap();
        (
            h.clone(),
            kernel_delay(&h, d.phase),
            kernel_delay_exact(&h, d.phase),
        )
    }

    #[test]
    #[ignore = "refactor pin against removed code; run with tier B"]
    fn custom_configs_build_the_same_kernels_through_the_design() {
        let ratios = [
            RateRatio::integer(2),
            RateRatio::integer(8),
            RateRatio::from_rates(44_100, 48_000),
            RateRatio::from_rates(96_000, 44_100),
        ];
        for (name, cfg) in configs() {
            let d = FilterDesign::Kernel(design_of(&cfg).unwrap());
            for &ratio in &ratios {
                if cfg.preserve_samples && ratio.is_decimating() {
                    continue;
                }
                let k = design::kernel(&d, 44_100, ratio).unwrap();
                let (taps, delay, delay_exact) = legacy(&cfg, ratio);
                let bits = |h: &[f64]| h.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
                assert_eq!(bits(&k.taps), bits(&taps), "{name} at {ratio}");
                assert_eq!(k.delay, delay, "{name} at {ratio}");
                assert_eq!(
                    k.delay_exact.to_bits(),
                    delay_exact.to_bits(),
                    "{name} at {ratio}"
                );
            }
        }
    }

    /// The Custom builder reads the same manual fields from the design's
    /// config as from the original, whatever the fields it ignores hold.
    #[test]
    fn legacy_config_keeps_every_field_the_builder_reads() {
        for (name, cfg) in configs() {
            let back = legacy_config(&design_of(&cfg).unwrap(), 44_100).unwrap();
            for ratio in [RateRatio::integer(4), RateRatio::from_rates(48_000, 44_100)] {
                let (a, b) = (
                    UpsamplerDesign::try_for_ratio(&cfg, ratio).unwrap(),
                    UpsamplerDesign::try_for_ratio(&back, ratio).unwrap(),
                );
                assert_eq!(
                    (a.ratio, a.taps, a.phase, a.preserve_samples, a.window),
                    (b.ratio, b.taps, b.phase, b.preserve_samples, b.window),
                    "{name}"
                );
                // A linear Tukey design reads no beta (it has none).
                if a.window != WindowKind::Tukey {
                    assert_eq!(a.kaiser_beta.to_bits(), b.kaiser_beta.to_bits(), "{name}");
                }
                assert_eq!(a.cutoff.to_bits(), b.cutoff.to_bits(), "{name}");
                if a.phase == FilterPhase::Intermediate {
                    assert_eq!(a.phase_mix.to_bits(), b.phase_mix.to_bits(), "{name}");
                }
                if a.window == WindowKind::Tukey {
                    assert_eq!(a.taper.to_bits(), b.taper.to_bits(), "{name}");
                }
            }
        }
    }

    #[test]
    fn the_design_of_a_custom_config() {
        let k = design_of(&base()).unwrap();
        assert_eq!(
            k.shape,
            Shape::WindowedSinc {
                window: Window::Kaiser {
                    beta: Some(12.0),
                    sampling: Sampling::Symmetric
                },
                length: Length::Fixed(255),
                cutoff: Cutoff::Fraction(Stepped::Fixed(0.95)),
            }
        );
        assert_eq!(k.decimation, Decimation::Keep);
        assert_eq!(k.numerics.grid_cutoff, GridCutoff::DesignRatio);
        let auto = ManualConfig {
            auto_taps: Some(FilterErrorBudget::MatchOutput),
            window: WindowKind::Gaussian,
            phase: FilterPhase::Intermediate,
            phase_mix: 0.25,
            ..base()
        };
        let k = design_of(&auto).unwrap();
        assert_eq!(
            k.shape,
            Shape::WindowedSinc {
                window: Window::GaussianStopband {
                    beta: None,
                    sampling: Sampling::Symmetric
                },
                length: Length::Auto(FilterErrorBudget::MatchOutput),
                cutoff: Cutoff::Band(BandSpec::default()),
            }
        );
        assert_eq!(
            k.phase,
            Phase::Intermediate {
                mix: 0.25,
                method: PhaseMethod::Reflect
            }
        );
    }

    #[test]
    fn refusals_keep_the_custom_builders_messages() {
        let cx = |ratio| BuildContext {
            source_rate: 44_100,
            ratio,
            channels: 2,
            max_block: 256,
            pipelines: budget::RATIONAL_SWAP_PIPELINES,
            output: audivane_protocol::OutputConfig::default(),
            workspace: budget::DESIGN_WORKSPACE_FLOOR,
        };
        let prepare = |cfg: &ManualConfig, ratio| {
            design::prepare(&FilterDesign::Kernel(design_of(cfg).unwrap()), &cx(ratio))
        };
        // A band the builder refuses: its BandError, word for word.
        let bad_band = ManualConfig {
            cutoff_mode: CutoffMode::Band,
            band: BandSpec {
                attenuation_db: 300.0,
                ..BandSpec::default()
            },
            ..base()
        };
        let want = UpsamplerDesign::try_for_ratio(&bad_band, RateRatio::integer(4))
            .unwrap_err()
            .to_string();
        let got = prepare(&bad_band, RateRatio::integer(4)).unwrap_err();
        assert!(matches!(got, DesignError::Band(BandError::Attenuation(_))));
        assert_eq!(got.to_string(), want);
        // An output budget is resolved against the context's output.
        let unresolved = ManualConfig {
            auto_taps: Some(FilterErrorBudget::MatchOutput),
            cutoff_mode: CutoffMode::Band,
            ..base()
        };
        let resolved = unresolved.with_output(&audivane_protocol::OutputConfig::default());
        assert_eq!(
            design::feasibility(
                &FilterDesign::Kernel(design_of(&unresolved).unwrap()),
                &cx(RateRatio::integer(4))
            ),
            design::feasibility(
                &FilterDesign::Kernel(design_of(&resolved).unwrap()),
                &cx(RateRatio::integer(4))
            )
        );
        // Over the rational budget: the budget's own message.
        let long = ManualConfig {
            taps: 262_144,
            ..base()
        };
        let r = RateRatio::from_rates(44_100, 48_000);
        let got = prepare(&long, r).unwrap_err();
        let DesignError::Budget(m) = &got else {
            panic!("{got:?}");
        };
        assert!(
            m.message
                .starts_with("the 160/147 kernel needs 38535168 taps"),
            "{m}"
        );
        // Outside the integer ratios: a refusal, not a panic.
        let got = prepare(&base(), RateRatio::integer(65)).unwrap_err();
        assert!(matches!(got, DesignError::Ratio { .. }), "{got:?}");
    }

    /// Feasibility never runs the measured search; prepare does. The
    /// cheap refusals keep the search's messages.
    #[test]
    fn feasibility_does_not_search() {
        use audivane_protocol::{DitherKind, OutputConfig};
        let out = OutputConfig {
            bits: 16,
            dither: DitherKind::None,
            headroom_db: 0.0,
        };
        let cx = |ratio| BuildContext {
            source_rate: 44_100,
            ratio,
            channels: 2,
            max_block: 256,
            pipelines: budget::RATIONAL_SWAP_PIPELINES,
            output: audivane_protocol::OutputConfig::default(),
            workspace: budget::DESIGN_WORKSPACE_FLOOR,
        };
        let searches = crate::design::auto::searches;
        let recommended = crate::design::catalogue::recommended().with_output(&out);
        // A band no other test searches, so nothing is cached either.
        let FilterDesign::Kernel(mut cold) = recommended.clone() else {
            unreachable!()
        };
        if let Shape::WindowedSinc { cutoff, .. } = &mut cold.shape {
            *cutoff = Cutoff::Band(BandSpec {
                passband_end: 0.8765,
                ..BandSpec::default()
            });
        }
        let cold = FilterDesign::Kernel(cold);
        let to48 = RateRatio::from_rates(44_100, 48_000);
        for d in [&recommended, &cold] {
            for ratio in [RateRatio::integer(2), RateRatio::integer(16), to48] {
                let before = searches();
                design::feasibility(d, &cx(ratio)).unwrap();
                assert_eq!(searches(), before, "{ratio}");
            }
        }
        let before = searches();
        design::prepare(&cold, &cx(RateRatio::integer(2))).unwrap();
        assert!(searches() > before);

        // The search's preliminary refusals, from feasibility too.
        for cfg in [
            ManualConfig {
                auto_taps: Some(FilterErrorBudget::Db(150.0)),
                window: WindowKind::Tukey,
                ..base()
            },
            ManualConfig {
                auto_taps: Some(FilterErrorBudget::Db(20.0)),
                ..base()
            },
        ] {
            let d = FilterDesign::Kernel(design_of(&cfg).unwrap());
            for ratio in [RateRatio::integer(4), to48] {
                let before = searches();
                let early = design::feasibility(&d, &cx(ratio)).unwrap_err();
                assert_eq!(searches(), before);
                let late = design::prepare(&d, &cx(ratio)).unwrap_err();
                assert_eq!(early, late, "{ratio}");
            }
        }
    }

    #[test]
    fn the_path_refuses_what_it_does_not_build() {
        let tukey = ManualConfig {
            window: WindowKind::Tukey,
            ..base()
        };
        for cfg in [
            ManualConfig {
                phase: FilterPhase::Minimum,
                ..tukey.clone()
            },
            ManualConfig {
                cutoff_mode: CutoffMode::NyquistStop,
                ..tukey.clone()
            },
        ] {
            let e = design_of(&cfg).unwrap_err().to_string();
            assert!(e.contains("Tukey window needs linear phase"), "{e}");
        }
        let k = design_of(&base()).unwrap();
        let refused = |k: KernelDesign| {
            let e = design::kernel(&FilterDesign::Kernel(k), 44_100, RateRatio::integer(4))
                .unwrap_err();
            assert!(matches!(e, DesignError::Unsupported(_)), "{e:?}");
        };
        // The path's new combinations (`combos`): a cepstral phase, the
        // scaled grid cutoff, a periodic Kaiser window.
        let built = |k: KernelDesign| {
            design::kernel(&FilterDesign::Kernel(k), 44_100, RateRatio::integer(4)).unwrap();
        };
        built(KernelDesign {
            phase: Phase::Minimum(PhaseMethod::Cepstral),
            ..k.clone()
        });
        refused(KernelDesign {
            decimation: Decimation::Scale {
                odd: crate::design::OddRule::Down,
                base_taps: None,
            },
            ..k.clone()
        });
        built(KernelDesign {
            numerics: Numerics {
                grid_cutoff: GridCutoff::Scaled,
                ..k.numerics
            },
            ..k.clone()
        });
        built(KernelDesign {
            shape: Shape::WindowedSinc {
                window: Window::Kaiser {
                    beta: Some(12.0),
                    sampling: Sampling::Periodic,
                },
                length: Length::Fixed(255),
                cutoff: Cutoff::Fraction(Stepped::Fixed(0.95)),
            },
            ..k.clone()
        });
        refused(KernelDesign {
            shape: Shape::WindowedSinc {
                window: Window::Kaiser {
                    beta: None,
                    sampling: Sampling::Symmetric,
                },
                length: Length::Fixed(255),
                cutoff: Cutoff::Fraction(Stepped::Fixed(0.95)),
            },
            ..k.clone()
        });
        refused(KernelDesign {
            shape: Shape::WindowedSinc {
                window: Window::Kaiser {
                    beta: Some(12.0),
                    sampling: Sampling::Symmetric,
                },
                length: Length::Band,
                cutoff: Cutoff::Fraction(Stepped::Fixed(0.95)),
            },
            ..k.clone()
        });
        refused(KernelDesign {
            shape: Shape::Lanczos { lobes: 3 },
            ..k.clone()
        });
        // The preset paths build the standard path's values
        // (`paths::general`).
        let preset_path = KernelDesign {
            decimation: Decimation::Scale {
                odd: crate::design::OddRule::Down,
                base_taps: None,
            },
            numerics: Numerics {
                path: ArithmeticPath::Fir,
                grid_cutoff: GridCutoff::Scaled,
            },
            ..k.clone()
        };
        built(preset_path.clone());
        built(KernelDesign {
            shape: Shape::WindowedSinc {
                window: Window::Kaiser {
                    beta: Some(12.0),
                    sampling: Sampling::Periodic,
                },
                length: Length::PerInput(Stepped::Fixed(63)),
                cutoff: Cutoff::Fraction(Stepped::Fixed(0.95)),
            },
            phase: Phase::Minimum(PhaseMethod::Reflect),
            ..preset_path
        });
    }

    fn cx2(ratio: RateRatio) -> BuildContext {
        BuildContext {
            source_rate: 44_100,
            ratio,
            channels: 2,
            max_block: 256,
            pipelines: budget::RATIONAL_SWAP_PIPELINES,
            output: audivane_protocol::OutputConfig::default(),
            workspace: budget::DESIGN_WORKSPACE_FLOOR,
        }
    }

    /// An intermediate phase at a mix of 1 builds the linear kernel and at
    /// 0 the minimum phase (`intermediate_phase`), and is budgeted so: a
    /// 2 097 153-tap design at x2 is a linear one at a mix of 1, over the
    /// workspace limit as a reflected one at 0, over the length limit
    /// blended.
    #[test]
    fn intermediate_mix_endpoints_are_budgeted_as_built() {
        let at = |mix| {
            let cfg = ManualConfig {
                taps: 2_097_153,
                phase: FilterPhase::Intermediate,
                phase_mix: mix,
                ..base()
            };
            design::feasibility(
                &FilterDesign::Kernel(design_of(&cfg).unwrap()),
                &cx2(RateRatio::integer(2)),
            )
        };
        let linear = at(1.0).unwrap();
        assert_eq!(linear.grid_taps, 2_097_153);
        assert!(linear.grid_taps_exact);
        assert_eq!(
            linear.workspace_bytes,
            budget::linear_workspace(2_097_153).max(budget::gate_workspace(2_097_153, 2, false))
        );
        let e = at(0.0).unwrap_err();
        assert!(e.to_string().contains("GB design limit"), "{e}");
        let e = at(0.5).unwrap_err();
        assert!(matches!(e, DesignError::Budget(_)), "{e}");
    }

    /// A two-tap Tukey kernel is all zeros (both ends of the window): it
    /// is refused at feasibility, before the rate mask can offer it, with
    /// the same refusal as prepare's.
    #[test]
    fn two_tap_tukey_is_refused_before_it_is_built() {
        let cfg = ManualConfig {
            taps: 2,
            window: WindowKind::Tukey,
            cutoff: 1.0,
            ..base()
        };
        let d = FilterDesign::Kernel(design_of(&cfg).unwrap());
        let e = design::feasibility(&d, &cx2(RateRatio::integer(2))).unwrap_err();
        assert!(
            e.to_string().contains("Tukey window needs at least 3 taps"),
            "{e}"
        );
        assert_eq!(
            design::prepare(&d, &cx2(RateRatio::integer(2))).unwrap_err(),
            e
        );
        // Three taps have a centre tap.
        let three = FilterDesign::Kernel(design_of(&ManualConfig { taps: 3, ..cfg }).unwrap());
        design::prepare(&three, &cx2(RateRatio::integer(2))).unwrap();
    }
}
