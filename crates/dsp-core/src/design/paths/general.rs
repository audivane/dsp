//! The new combinations of the preset paths: everything a preset path
//! can build beyond what its legacy builder (`super::recipe`,
//! `super::kernel`) builds. A design the legacy builder takes (`legacy`)
//! never comes here, so every preset keeps its bits.
//!
//! What it adds, on the `fir`, `fir_after`, `exact_*` and `double_double`
//! paths:
//!
//! - **windows and samplings**: every window except the `standard` path's
//!   Gaussian, `periodic` or `symmetric` (`integer_centre`: Gaussian),
//!   with a Kaiser beta derived from the band or budget when it is `None`
//!   (`crate::design::paths::fir::windows::sample`). The `exact_*` paths
//!   take their sinc form with any f64 window; `double_double` stays the
//!   integer-centre Gaussian's.
//! - **lengths**: `fixed`, `band` and `auto` ([`crate::design::search`]),
//!   besides `per_input` and `output_span`. The length rule alone sets the
//!   grid taps; the build ratio `B` below only divides the sinc's argument.
//! - **cutoffs**: `nyquist_stop` and `band`, besides `fraction`.
//! - **grid cutoff and decimation**: `design_ratio` builds the kernel at
//!   the integer ratio `R = max(L, M)` (`B = R`) and scales it by `L / R`;
//!   `scaled` builds it at `B = L` with the cutoff scaled by `out / in`
//!   when decimating. `keep` leaves a decimated `per_input` length
//!   unscaled.
//! - **phases**: linear on every path, `reflect` minimum and intermediate
//!   with a contract per window (`contract`), and any cepstral `mix`
//!   ([`cepstral::Target::Mix`]); a mix of 1 builds the linear kernel, 0
//!   the minimum phase.
//! - **`preserve_samples`**: the centre branch set to exactly 1 and 0.
//!
//! The band, stop-at-Nyquist and automatic designs promise an attenuation
//! ([`Claim`]): the window model ([`crate::design::window_model`]) sets
//! their lengths and cutoffs, and [`crate::design::claim`] measures the
//! finished kernel against the promise before it is accepted.

use super::fir::{self, Recipe};
use super::{double_double, exact};
use crate::design::phase::cepstral::{self, Target};
use crate::design::{
    ArithmeticPath, BuildContext, Cutoff, Decimation, DesignError, FilterDesign, FilterErrorBudget,
    Grid, GridCutoff, Kernel, KernelDesign, Length, Phase, PhaseMethod, Plan, Shape, Window,
    budget, check, claim, length, rates, search, window_model,
};
use crate::minphase::Contract;

/// What a design promises: within `db` dB of unity up to `pass` and under
/// `-db` dB from `stop` (fractions of the lower Nyquist; `pass` 0 promises
/// the stopband only).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Claim {
    pub pass: f64,
    pub stop: f64,
    pub db: f64,
}

/// A design evaluated at one source rate and grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct General {
    /// The cutoff the sinc uses at the build ratio and the window; its
    /// `taps` is `len / build` rounded up (what the legacy builders take
    /// when `build` divides `len`).
    pub recipe: Recipe,
    /// Taps of the prototype on the grid (`N`), set by the length rule
    /// alone, never by the build ratio.
    pub len: usize,
    /// The ratio the kernel is built at: `L` (`scaled`) or `R`
    /// (`design_ratio`).
    pub build: usize,
    /// `L / build`, applied to every tap at the end.
    pub scale: f64,
    /// The design window after the rate rules (a fallback) and with its
    /// beta resolved; `None` for Lanczos.
    pub window: Option<Window>,
    pub claim: Option<Claim>,
}

impl General {
    /// Taps of the prototype on the grid.
    pub fn taps(&self) -> usize {
        self.len
    }
}

fn unsupported<T>(what: impl Into<String>) -> Result<T, DesignError> {
    Err(DesignError::Unsupported(what.into()))
}

/// Whether the legacy builders of the preset paths build `k` (the
/// combinations every preset uses): its arithmetic is theirs, untouched.
pub(crate) fn legacy(k: &KernelDesign) -> bool {
    use ArithmeticPath::*;
    let path = k.numerics.path;
    if path == Standard
        || k.numerics.grid_cutoff != GridCutoff::Scaled
        || k.decimation == Decimation::Keep
        || k.preserve_samples
    {
        return false;
    }
    let window_ok = |w: Window| match super::resolve_window(w) {
        Ok(fir::Window::Gaussian { .. }) => matches!(path, Fir | FirAfter | DoubleDouble),
        Ok(
            fir::Window::KaiserPeriodic { .. }
            | fir::Window::ChebyshevSymmetric { .. }
            | fir::Window::NuttallSymmetric,
        ) => path != DoubleDouble,
        Ok(_) => matches!(path, Fir | FirAfter),
        Err(_) => false,
    };
    let shape_ok = match &k.shape {
        Shape::Lanczos { .. } => matches!(path, Fir | FirAfter),
        Shape::WindowedSinc {
            window,
            length,
            cutoff,
        } => {
            matches!(cutoff, Cutoff::Fraction(_))
                && matches!(length, Length::PerInput(_) | Length::OutputSpan(_))
                && window_ok(*window)
                && k.rates.fallback_window.is_none_or(|f| window_ok(f.window))
        }
    };
    let phase_ok = match k.phase {
        Phase::Linear => matches!(path, Fir | FirAfter),
        Phase::Minimum(PhaseMethod::Cepstral) => true,
        Phase::Intermediate {
            mix,
            method: PhaseMethod::Cepstral,
        } => mix == cepstral::INTERMEDIATE_BLEND,
        _ => false,
    };
    shape_ok && phase_ok
}

/// The budget of an automatic length, resolved.
fn auto_budget(b: FilterErrorBudget) -> Result<f64, DesignError> {
    match b {
        FilterErrorBudget::Db(a) => Ok(a),
        FilterErrorBudget::MatchOutput => unsupported(
            "\"match_output\" must be resolved against the output settings before design",
        ),
    }
}

/// The attenuation `k` promises and its band, if it promises one.
fn target(k: &KernelDesign) -> Result<Option<(f64, f64, f64)>, DesignError> {
    let Shape::WindowedSinc { length, cutoff, .. } = &k.shape else {
        return Ok(None);
    };
    Ok(match (length, cutoff) {
        (Length::Band, Cutoff::Band(b)) => {
            Some((b.passband_end, b.stopband_start, b.attenuation_db))
        }
        (Length::Auto(budget), Cutoff::Band(b)) => {
            Some((b.passband_end, b.stopband_start, auto_budget(*budget)?))
        }
        (Length::Band | Length::Auto(_), _) => {
            return unsupported("a band or automatic length needs a band cutoff");
        }
        (_, Cutoff::Band(_)) => {
            return unsupported("a band cutoff needs a band or automatic length");
        }
        _ => None,
    })
}

/// The refusal of a promise deeper than the window goes.
fn too_deep(w: &Window, a: f64) -> DesignError {
    match window_model::nominal_db(w) {
        Some(n) => DesignError::Claim(format!(
            "the window's sidelobes (-{n:.1} dB) are above the budget ({a} dB): choose a \
             deeper window or lower the target"
        )),
        None => DesignError::Claim(format!(
            "{w:?} has no calibrated stopband: it cannot promise {a} dB"
        )),
    }
}

/// The design window `w` with its beta resolved for the attenuation `a` a
/// band or automatic length promises (Kaiser's formula for the windowed
/// sinc's measured shortfall, [`window_model::KAISER_SHORTFALL_DB`]).
fn resolve_beta(w: Window, a: Option<f64>) -> Result<Window, DesignError> {
    match w {
        Window::Kaiser {
            beta: None,
            sampling,
        } => match a {
            Some(a) => Ok(Window::Kaiser {
                beta: Some(crate::window::kaiser_beta_for_attenuation(
                    a + window_model::KAISER_SHORTFALL_DB,
                )),
                sampling,
            }),
            None => unsupported(
                "a Kaiser window without a beta needs a band or automatic length (the beta \
                 comes from the attenuation)",
            ),
        },
        w => Ok(w),
    }
}

/// The builders' window for the design window `w`: the legacy windows as
/// they are, the others sampled by [`fir::windows::sample`].
fn fir_window(w: Window) -> Result<fir::Window, DesignError> {
    if let Ok(f) = super::resolve_window(w) {
        return Ok(f);
    }
    fir::Window::other(w).map_or_else(
        || {
            unsupported(format!(
                "{w:?}: this window and sampling are not on the preset paths"
            ))
        },
        Ok,
    )
}

/// The build ratio of `k` on `grid`.
fn build_ratio(k: &KernelDesign, grid: &Grid) -> usize {
    match k.numerics.grid_cutoff {
        GridCutoff::Scaled => grid.up,
        GridCutoff::DesignRatio => grid.design_ratio,
    }
}

/// `k` from `source_rate` on `grid`: its grid taps (`taps`, for an
/// automatic length: the search's; without, the window-model estimate),
/// cutoff, window and promise. The length rule sets the grid taps on its
/// own (`fixed`: `taps * M`; `per_input`, `output_span`, Lanczos: `T * L`;
/// `band`, `auto`: the model's or the search's), whatever ratio the sinc is
/// built at.
pub(crate) fn recipe(
    k: &KernelDesign,
    source_rate: u32,
    grid: &Grid,
    taps: Option<usize>,
) -> Result<General, DesignError> {
    if k.numerics.path == ArithmeticPath::Standard {
        return unsupported("the standard path builds its own kernels");
    }
    let b = build_ratio(k, grid);
    let r = grid.design_ratio;
    if k.numerics.grid_cutoff == GridCutoff::DesignRatio
        && matches!(k.decimation, Decimation::Scale { .. })
        && grid.decimating
    {
        return unsupported(
            "the design-ratio grid cutoff designs the kernel at the decimation ratio itself: \
             use decimation keep",
        );
    }
    if matches!(k.shape, Shape::Lanczos { .. }) && grid.decimating {
        return unsupported(
            "a Lanczos kernel has no cutoff to decimate with: it runs at output rates at or above \
             the source rate",
        );
    }
    let target = target(k)?;
    let per_input = |k: &KernelDesign| -> Result<usize, DesignError> {
        let mut t = length::per_input(k, grid.up, source_rate)?;
        if grid.decimating && k.numerics.grid_cutoff == GridCutoff::Scaled {
            t = length::decimated(t, &k.decimation, grid);
        }
        Ok(t)
    };
    let (window, fir_win, fc) = match &k.shape {
        Shape::Lanczos { lobes } => (
            None,
            fir::Window::Lanczos { a: *lobes },
            crate::design::catalogue::FC_NYQUIST,
        ),
        Shape::WindowedSinc { window, cutoff, .. } => {
            let w = rates::effective_window(*window, &k.rates, grid);
            let w = resolve_beta(w, target.map(|t| t.2))?;
            let fc = match cutoff {
                Cutoff::Fraction(fc) => match fc.at(source_rate) {
                    Some(fc) => fc,
                    None => {
                        return unsupported("the stepped cutoff has no step for this source rate");
                    }
                },
                Cutoff::Band(spec) => 0.5 * (spec.passband_end + spec.stopband_start),
                // Set from the length below.
                Cutoff::NyquistStop => f64::NAN,
            };
            (Some(w), fir_window(w)?, fc)
        }
    };
    let max = budget::MAX_KERNEL_TAPS;
    let n = match (&k.shape, taps) {
        (_, Some(n)) => n,
        (Shape::Lanczos { .. }, None) => per_input(k)? * grid.up,
        (Shape::WindowedSinc { length, .. }, None) => match length {
            Length::PerInput(_) | Length::OutputSpan(_) => per_input(k)? * grid.up,
            Length::Fixed(n) => n * grid.down,
            Length::Band | Length::Auto(_) => {
                let (pe, se, a) = target.expect("a band or automatic length has a target");
                let w = window.expect("a windowed sinc has a window");
                if !window_model::reaches(&w, a) {
                    return Err(too_deep(&w, a));
                }
                match window_model::taps_for(&w, a, r, 0.5 * (se - pe), max) {
                    Some(n) => n,
                    None => {
                        return Err(DesignError::Claim(format!(
                            "{a} dB over this band needs a kernel longer than {max} taps at \
                             {}: widen the transition or lower the target",
                            grid.ratio
                        )));
                    }
                }
            }
        },
    };
    if n == 0 {
        return unsupported("the kernel has no taps");
    }
    let mut claim = target.map(|(pass, stop, db)| Claim { pass, stop, db });
    let fc = if fc.is_nan() {
        // Stop at Nyquist: the window's nominal stopband from the lower
        // Nyquist up.
        let w = window.expect("a windowed sinc has a window");
        let Some(a) = window_model::nominal_db(&w) else {
            return Err(DesignError::Claim(format!(
                "{w:?} has no calibrated stopband: stop at Nyquist needs one"
            )));
        };
        let fc = window_model::half_width(&w, a, n, r).map_or(f64::NEG_INFINITY, |h| 1.0 - h);
        if fc < crate::resample::NYQUIST_STOP_MIN_CUTOFF {
            let min = window_model::taps_for(
                &w,
                a,
                r,
                1.0 - crate::resample::NYQUIST_STOP_MIN_CUTOFF,
                max,
            );
            return Err(DesignError::Claim(format!(
                "stop at Nyquist needs at least {} taps on this grid with this window (got {n}): \
                 below that its cutoff would fall under {} of the Nyquist",
                min.map_or("more than the limit".into(), |m| m.to_string()),
                crate::resample::NYQUIST_STOP_MIN_CUTOFF
            )));
        }
        claim = Some(Claim {
            pass: 0.0,
            stop: 1.0,
            db: a,
        });
        fc.min(1.0)
    } else {
        fc
    };
    if fc.is_nan() || fc <= 0.0 {
        return unsupported("the cutoff must be above 0");
    }
    let cutoff = if grid.decimating && k.numerics.grid_cutoff == GridCutoff::Scaled {
        fc * grid.ratio.as_f64()
    } else {
        fc
    };
    if let (Some(c), Some(w)) = (claim, window)
        && !window_model::reaches(&w, c.db)
    {
        return Err(too_deep(&w, c.db));
    }
    if claim.is_some_and(|c| c.db > REFLECT_MAX_DB) && reflected(k) {
        return Err(DesignError::Claim(format!(
            "a reflected phase keeps at most {REFLECT_MAX_DB:.1} dB (got {} dB)",
            claim.map_or(0.0, |c| c.db)
        )));
    }
    Ok(General {
        recipe: Recipe {
            taps: n.div_ceil(b),
            cutoff,
            window: fir_win,
        },
        len: n,
        build: b,
        scale: grid.up as f64 / b as f64,
        window,
        claim,
    })
}

/// The deepest attenuation a reflected phase's contract holds (Kaiser beta
/// 20, [`crate::minphase::MAX_MINIMUM_PHASE_BETA`]).
pub const REFLECT_MAX_DB: f64 = crate::minphase::MAX_MINIMUM_PHASE_BETA / 0.1102 + 8.7;

/// The shallowest a reflected phase's contract may assume (Kaiser beta 2,
/// [`crate::minphase::MIN_MINIMUM_PHASE_BETA`]).
const REFLECT_MIN_DB: f64 = crate::minphase::MIN_MINIMUM_PHASE_BETA / 0.1102 + 8.7;

/// The attenuation a reflected Lanczos kernel's contract assumes: its
/// response has no stopband to hold (-13 dB at the Nyquist), so the
/// contract checks the magnitude only, down to -100 dB.
const LANCZOS_CONTRACT_DB: f64 = 60.0;

fn reflected(k: &KernelDesign) -> bool {
    matches!(
        k.phase,
        Phase::Minimum(PhaseMethod::Reflect)
            | Phase::Intermediate {
                method: PhaseMethod::Reflect,
                ..
            }
    )
}

/// The contract a reflected phase of `g`'s `n`-tap kernel is checked
/// against ([`crate::minphase`]): the window's nominal attenuation (a Kaiser
/// window's own, `beta / 0.1102 + 8.7`, as the `standard` path's),
/// clamped to what the reflection holds, from the model's stopband edge
/// with 20 % slack on the half transition, in cycles per sample of the
/// build grid. Lanczos: the magnitude only (no stopband edge).
pub(crate) fn contract(g: &General, n: usize) -> Contract {
    let Some(w) = g.window else {
        return Contract {
            kaiser_beta: window_model::contract_beta(LANCZOS_CONTRACT_DB),
            stopband_edge: 0.5,
        };
    };
    let nominal = window_model::nominal_db(&w);
    let a = match w {
        Window::Kaiser { beta: Some(b), .. } => crate::resample::kaiser_attenuation_db(b),
        _ => nominal.unwrap_or(LANCZOS_CONTRACT_DB),
    }
    .clamp(REFLECT_MIN_DB, REFLECT_MAX_DB);
    let half_bins = nominal
        .and_then(|a| window_model::half_width_bins(&w, a, n))
        .unwrap_or(n as f64 / 2.0);
    Contract {
        kaiser_beta: window_model::contract_beta(a),
        stopband_edge: g.recipe.cutoff / (2.0 * g.build as f64) + 1.2 * half_bins / n as f64,
    }
}

/// The prototype of `k` from `source_rate` on `grid` (an automatic length
/// searched), before normalisation and the phase transform.
pub(crate) fn prototype_at(
    k: &KernelDesign,
    source_rate: u32,
    grid: &Grid,
) -> Result<Vec<f64>, DesignError> {
    let t = if automatic(k) {
        Some(search::search(k, &search::limits(source_rate, grid.ratio))?)
    } else {
        None
    };
    let g = recipe(k, source_rate, grid, t)?;
    prototype(k.numerics.path, &g.recipe, g.len, g.build)
}

/// The `n`-tap prototype of `r` with the sinc divided by `b` on `path`:
/// the legacy builders where they take the window (and `b` divides `n`,
/// their geometry), else the path's sinc form of `n` taps with the f64
/// window.
fn prototype(
    path: ArithmeticPath,
    r: &Recipe,
    n: usize,
    b: usize,
) -> Result<Vec<f64>, DesignError> {
    use ArithmeticPath::*;
    use exact::SincForm::*;
    let lanczos = matches!(r.window, fir::Window::Lanczos { .. });
    if n != b * r.taps {
        return match path {
            _ if lanczos => unsupported(
                "a Lanczos kernel spans whole input samples: its length is a multiple of the ratio",
            ),
            Fir | FirAfter => Ok(fir::prototype_len(r, n, b)),
            ExactPiInverseAfter | ExactPiInversePairwise | ExactOverRatioBefore => {
                let form = if path == ExactOverRatioBefore {
                    CutoffOverRatio
                } else {
                    PiCutoffTimesInverseRatio
                };
                let w = fir::window(r.window, n);
                let mut h = exact::sinc_f64(n, r.cutoff, b, form);
                for (v, w) in h.iter_mut().zip(&w) {
                    *v *= w;
                }
                Ok(h)
            }
            DoubleDouble => double_double::gaussian_len(r, n, b).map_or_else(
                || unsupported("the double-double path builds the integer-centre Gaussian only"),
                Ok,
            ),
            Standard => unsupported("the standard path builds its own kernels"),
        };
    }
    match path {
        Fir | FirAfter => Ok(fir::prototype(r, b)),
        _ if lanczos => unsupported(format!(
            "the {path:?} path does not build a Lanczos kernel: use the fir paths"
        )),
        ExactPiInverseAfter | ExactPiInversePairwise | ExactOverRatioBefore => {
            if let Ok(h) = super::prototype(path, r, b) {
                return Ok(h);
            }
            let form = if path == ExactOverRatioBefore {
                CutoffOverRatio
            } else {
                PiCutoffTimesInverseRatio
            };
            let n = b * r.taps;
            let w = fir::window(r.window, n);
            let mut h = exact::sinc_f64(n, r.cutoff, b, form);
            for (v, w) in h.iter_mut().zip(&w) {
                *v *= w;
            }
            Ok(h)
        }
        DoubleDouble => double_double::gaussian(r, b).map_or_else(
            || unsupported("the double-double path builds the integer-centre Gaussian only"),
            Ok,
        ),
        Standard => unsupported("the standard path builds its own kernels"),
    }
}

/// The exactly symmetric prototype a reflected phase starts from: the sinc
/// (or the Lanczos kernel) centred on `(N - 1) / 2`, as the window of a
/// `symmetric` sampling is, so the kernel is linear-phase at every length
/// (the preset paths' centre `floor(N / 2)` is half a sample off at even
/// N, and the reflection's zero geometry needs a linear-phase kernel). The
/// fir paths only.
fn symmetric_prototype(
    path: ArithmeticPath,
    r: &Recipe,
    n: usize,
    b: usize,
) -> Result<Vec<f64>, DesignError> {
    if !matches!(path, ArithmeticPath::Fir | ArithmeticPath::FirAfter) {
        return unsupported(format!(
            "a reflected phase is built on the fir paths (or the standard path), not the {path:?} \
             path"
        ));
    }
    let c = (n as f64 - 1.0) / 2.0;
    let l = b as f64;
    Ok(match r.window {
        fir::Window::Lanczos { a } => {
            let af = a as f64;
            (0..n)
                .map(|i| {
                    let x = if c > 0.0 {
                        (i as f64 - c) * af / c
                    } else {
                        0.0
                    };
                    if x.abs() < af {
                        fir::sinc(x) * fir::sinc(x / af)
                    } else {
                        0.0
                    }
                })
                .collect()
        }
        w => {
            let mut h = fir::window(w, n);
            for (i, v) in h.iter_mut().enumerate() {
                *v *= fir::sinc(r.cutoff * (i as f64 - c) / l);
            }
            h
        }
    })
}

/// The transform `k`'s phase needs.
enum Transform {
    None,
    Cepstral(Target),
    /// `None`: minimum phase; `Some(mix)`: intermediate.
    Reflect(Option<f64>),
}

fn transform_of(phase: Phase) -> Transform {
    match phase {
        Phase::Linear => Transform::None,
        Phase::Minimum(PhaseMethod::Cepstral) => Transform::Cepstral(Target::Minimum),
        Phase::Minimum(PhaseMethod::Reflect) => Transform::Reflect(None),
        Phase::Intermediate { mix, .. } if mix >= 1.0 => Transform::None,
        Phase::Intermediate {
            mix,
            method: PhaseMethod::Cepstral,
        } => Transform::Cepstral(if mix <= 0.0 {
            Target::Minimum
        } else if mix == cepstral::INTERMEDIATE_BLEND {
            Target::Intermediate
        } else {
            Target::Mix(mix)
        }),
        Phase::Intermediate {
            mix,
            method: PhaseMethod::Reflect,
        } => Transform::Reflect(Some(mix)),
    }
}

/// The kernel of `k` for `g` on `grid`, with its delay: the prototype,
/// normalised before the transform (`fir`, `exact_over_ratio_before`) or
/// after it (the others) on the build grid, the phase transform, then the
/// `L / B` scale. The delay is the centre `floor(N / 2)` for linear phase,
/// the first largest tap otherwise.
pub(crate) fn kernel(k: &KernelDesign, g: &General, grid: &Grid) -> Result<Kernel, DesignError> {
    use ArithmeticPath::*;
    let path = k.numerics.path;
    let b = g.build;
    let transform = transform_of(k.phase);
    let mut h = if matches!(transform, Transform::Reflect(_)) {
        symmetric_prototype(path, &g.recipe, g.len, b)?
    } else {
        prototype(path, &g.recipe, g.len, b)?
    };
    let before = matches!(path, Fir | ExactOverRatioBefore);
    let sequential = matches!(path, ExactPiInverseAfter | ExactOverRatioBefore);
    let normalise = |h: &mut [f64]| {
        if sequential {
            exact::normalise_sequential(h, b);
        } else {
            fir::normalise(h, b);
        }
    };
    let transformed = !matches!(transform, Transform::None);
    if before || !transformed {
        normalise(&mut h);
    }
    if k.preserve_samples {
        if transformed || grid.decimating || b != grid.up || g.recipe.cutoff != 1.0 {
            return unsupported(
                "preserve_samples needs linear phase and the cutoff 1.0, at an output rate at or \
                 above the source rate",
            );
        }
        // The sinc's zeros fall on every b-th tap from the centre: the
        // centre branch becomes exactly an impulse.
        let n = h.len();
        let c = n / 2;
        h[c] = 1.0;
        let mut j = b;
        while j <= c {
            h[c - j] = 0.0;
            if c + j < n {
                h[c + j] = 0.0;
            }
            j += b;
        }
    }
    if transformed {
        let n = h.len();
        h = match transform {
            Transform::Cepstral(t) => cepstral::transform(&h, t)?,
            Transform::Reflect(_) if n < 2 => h,
            Transform::Reflect(None) => {
                crate::minphase::minimum_phase(&h, &contract(g, n))
                    .map_err(DesignError::Phase)?
                    .kernel
            }
            Transform::Reflect(Some(mix)) => {
                crate::minphase::intermediate_phase(&h, &contract(g, n), mix)
                    .map_err(DesignError::Phase)?
                    .kernel
            }
            Transform::None => h,
        };
        if !before {
            normalise(&mut h);
        }
    }
    if g.scale != 1.0 {
        for v in &mut h {
            *v *= g.scale;
        }
    }
    let delay = if k.phase == Phase::Linear {
        h.len() / 2
    } else {
        cepstral::peak_index(&h)
    };
    Ok(Kernel {
        taps: h,
        delay,
        delay_exact: delay as f64,
    })
}

/// Whether `k`'s kernel is four times its prototype (a blended cepstral
/// phase), or up to four times (a blended reflected one).
pub(crate) fn growth(k: &KernelDesign) -> (usize, bool) {
    match transform_of(k.phase) {
        Transform::Cepstral(Target::Intermediate | Target::Mix(_)) => (4, true),
        Transform::Reflect(Some(mix)) if mix > 0.0 => (4, false),
        _ => (1, true),
    }
}

/// Whether `k` is phase-transformed (for its workspace).
pub(crate) fn method(k: &KernelDesign) -> Option<PhaseMethod> {
    match transform_of(k.phase) {
        Transform::None => None,
        Transform::Cepstral(_) => Some(PhaseMethod::Cepstral),
        Transform::Reflect(_) => Some(PhaseMethod::Reflect),
    }
}

/// Whether `k` has an automatic length.
fn automatic(k: &KernelDesign) -> bool {
    matches!(
        k.shape,
        Shape::WindowedSinc {
            length: Length::Auto(_),
            ..
        }
    )
}

/// The plan of `k` built from `g` in `cx` (`exact`: the length is known,
/// not an automatic length's estimate).
fn plan(k: &KernelDesign, g: &General, cx: &BuildContext, exact: bool) -> Plan {
    let n = g.taps();
    let (growth, exact_growth) = growth(k);
    let grid_taps = n * growth;
    let mut workspace = match method(k) {
        None => budget::linear_workspace(n),
        Some(PhaseMethod::Cepstral) => budget::cepstral_workspace(n),
        Some(PhaseMethod::Reflect) => budget::reflect_workspace(n),
    };
    if g.claim.is_some() {
        workspace = workspace.max(claim::workspace(grid_taps));
    }
    let priced = match (method(k), growth) {
        (Some(PhaseMethod::Cepstral), 4) => 2 * n,
        (Some(PhaseMethod::Reflect), 4) => {
            let beta = contract(g, n.max(2)).kaiser_beta;
            (n as f64 * budget::intermediate_growth(beta)).ceil() as usize
        }
        _ => n,
    };
    Plan::kernel(
        cx,
        grid_taps,
        exact && exact_growth,
        workspace,
        priced,
        true,
    )
}

/// [`crate::design::feasibility`] of the new combination `k` (`d` as a
/// design): the bounds ([`check`]), the ratio rules, the length (an
/// automatic one searched when `search`, else its estimate), the budgets
/// and the plan.
pub(crate) fn feasibility(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
    search: bool,
) -> Result<(Plan, General), DesignError> {
    check::check(d).map_err(DesignError::Field)?;
    rates::check(d, cx.ratio)?;
    let grid = cx.grid();
    let auto = automatic(k);
    let t = if auto && search {
        Some(search::search(k, cx)?)
    } else {
        None
    };
    let g = recipe(k, cx.source_rate, &grid, t)?;
    let plan = admit(k, &g, cx, !auto || search)?;
    Ok((plan, g))
}

/// The budgets of `k` at `g`'s length in `cx`: the tap limits, the
/// rational cost, the cepstral transform's FFT and the workspace cap, and
/// the plan. [`feasibility`] applies them to the design, the automatic
/// search to each candidate before it is built.
pub(crate) fn admit(
    k: &KernelDesign,
    g: &General,
    cx: &BuildContext,
    exact: bool,
) -> Result<Plan, DesignError> {
    let n = g.taps();
    let (growth, exact_growth) = growth(k);
    if cx.ratio.is_integer() {
        budget::check_kernel_taps(n, cx.ratio, growth == 4)?;
    } else {
        // A reflected blend's length is known once built: its design
        // length here, the final one in `prepare`.
        let taps = if exact_growth { n * growth } else { n };
        budget::check_rational(
            taps,
            cx.ratio,
            cx.source_rate,
            budget::MAX_RATIONAL_GRID_TAPS,
            cx.pipelines,
        )?;
    }
    if method(k) == Some(PhaseMethod::Cepstral) {
        budget::check_cepstral(n, cx.ratio)?;
    }
    plan(k, g, cx, exact).check_workspace(cx)
}

/// [`crate::design::prepare`] of the new combination `k` up to the safety
/// gate: [`feasibility`] with the length searched, the kernel, the budgets
/// on its final length, and its promise measured ([`claim::verify`]).
pub(crate) fn prepare(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
) -> Result<(Plan, Kernel), DesignError> {
    let (plan, g) = feasibility(d, k, cx, true)?;
    let grid = cx.grid();
    let kernel = kernel(k, &g, &grid)?;
    let len = kernel.taps.len();
    if cx.ratio.is_integer() {
        budget::check_kernel_taps(len, cx.ratio, false)?;
    } else {
        budget::check_rational(
            len,
            cx.ratio,
            cx.source_rate,
            budget::MAX_RATIONAL_GRID_TAPS,
            cx.pipelines,
        )?;
    }
    if let Some(c) = g.claim {
        claim::verify(&kernel, &c, &grid)?;
    }
    let plan = Plan {
        grid_taps: len,
        grid_taps_exact: true,
        resident_bytes: budget::kernel_resident(len, cx),
        ..plan
    };
    Ok((plan, kernel))
}

/// The kernel of the new combination `k` from `source_rate` at `ratio`, an
/// automatic length searched, its promise measured; no budget checked.
pub(crate) fn kernel_at(
    k: &KernelDesign,
    source_rate: u32,
    grid: &Grid,
) -> Result<Kernel, DesignError> {
    let t = if automatic(k) {
        Some(search::search(k, &search::limits(source_rate, grid.ratio))?)
    } else {
        None
    };
    let g = recipe(k, source_rate, grid, t)?;
    let kernel = kernel(k, &g, grid)?;
    if let Some(c) = g.claim {
        claim::verify(&kernel, &c, grid)?;
    }
    Ok(kernel)
}

/// The grid taps of `k` (an automatic length searched) and the taps its
/// runtime is priced at.
pub(crate) fn taps(
    k: &KernelDesign,
    source_rate: u32,
    grid: &Grid,
) -> Result<(usize, usize), DesignError> {
    let t = if automatic(k) {
        Some(search::search(k, &search::limits(source_rate, grid.ratio))?)
    } else {
        None
    };
    let g = recipe(k, source_rate, grid, t)?;
    let n = g.taps();
    let (growth, _) = growth(k);
    let priced = match (method(k), growth) {
        (Some(PhaseMethod::Cepstral), 4) => 2 * n,
        (Some(PhaseMethod::Reflect), 4) => {
            let beta = contract(&g, n.max(2)).kaiser_beta;
            (n as f64 * budget::intermediate_growth(beta)).ceil() as usize
        }
        _ => n,
    };
    Ok((n * growth, priced))
}

/// The plan of `k` with its length resolved (searched), no budget checked.
pub(crate) fn resolved_plan(k: &KernelDesign, cx: &BuildContext) -> Result<Plan, DesignError> {
    let grid = cx.grid();
    let t = if automatic(k) {
        Some(search::search(k, cx)?)
    } else {
        None
    };
    let g = recipe(k, cx.source_rate, &grid, t)?;
    Ok(plan(k, &g, cx, true))
}

#[cfg(test)]
pub(crate) mod tests;
