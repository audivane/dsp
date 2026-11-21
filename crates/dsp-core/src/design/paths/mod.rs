//! The kernel builders, selected by [`ArithmeticPath`].
//!
//! A kernel design is first evaluated at one source rate and grid into a
//! `Recipe` (taps per input sample, cutoff, effective window): the rate
//! rules of [`super::rates`] and the lengths of [`super::length`] apply
//! here, and nothing depends on the rate afterwards. The path then picks
//! one of the existing builders, each kept with its exact arithmetic:
//!
//! | Path | Prototype | Normalisation |
//! |---|---|---|
//! | `Fir` | `fir::prototype` | `fir::normalise` (pairwise), before the transform |
//! | `FirAfter` | `fir::prototype` | `fir::normalise`, after the transform |
//! | `ExactPiInverseAfter` | `exact::exact`, `k ((fc pi)(1 / L))` | `exact::normalise_sequential`, after |
//! | `ExactOverRatioBefore` | `exact::exact`, `(k (fc / L)) pi` | `exact::normalise_sequential`, before |
//! | `ExactPiInversePairwise` | `exact::exact`, `k ((fc pi)(1 / L))` | `fir::normalise`, after |
//! | `DoubleDouble` | `double_double::gaussian` | `fir::normalise`, after |
//! | `Standard` | [`standard::try_design_kernel`] | sequential `R / sum`, before and after the reflection |
//!
//! Each path accepts the windows and phases it was built for; anything else
//! is refused as [`DesignError::Unsupported`]. A linear kernel's delay is
//! its centre `floor(N / 2)`; a cepstral one's is its largest tap. The
//! `Standard` path evaluates its own rules and has its own delay
//! conventions ([`standard`]); the functions here build the preset paths.

pub(crate) mod double_double;
pub(crate) mod exact;
pub mod fir;
pub mod general;
pub mod standard;

use super::phase::cepstral::{self, Target};
use super::{
    ArithmeticPath, Cutoff, Decimation, DesignError, FlattopSet, GridCutoff, Kernel, KernelDesign,
    Phase, PhaseMethod, Sampling, Shape, Window, catalogue, length, rates,
};
use crate::design::Grid;
use fir::{Recipe, windows};

/// The recipe of `k` from `source_rate` on `grid` (L branches).
pub(crate) fn recipe(
    k: &KernelDesign,
    source_rate: u32,
    grid: &Grid,
) -> Result<Recipe, DesignError> {
    let unsupported = |what: &str| {
        Err(DesignError::Unsupported(format!(
            "{what} comes with the standard path; the {:?} path does not build it yet",
            k.numerics.path
        )))
    };
    if k.numerics.path == ArithmeticPath::Standard {
        return Err(DesignError::Unsupported(
            "the standard path builds its own kernels".into(),
        ));
    }
    if k.numerics.grid_cutoff != GridCutoff::Scaled {
        return unsupported("the design-ratio grid cutoff");
    }
    if k.decimation == Decimation::Keep {
        return unsupported("an unscaled decimation length");
    }
    let mut taps = length::per_input(k, grid.up, source_rate)?;
    let (mut cutoff, window) = match &k.shape {
        Shape::Lanczos { lobes } => (catalogue::FC_NYQUIST, fir::Window::Lanczos { a: *lobes }),
        Shape::WindowedSinc { window, cutoff, .. } => {
            let Cutoff::Fraction(fc) = cutoff else {
                return unsupported("a stop-at-Nyquist or band cutoff");
            };
            let Some(fc) = fc.at(source_rate) else {
                return Err(DesignError::Unsupported(
                    "the stepped cutoff has no step for this source rate".into(),
                ));
            };
            let w = rates::effective_window(*window, &k.rates, grid);
            (fc, resolve_window(w)?)
        }
    };
    if grid.decimating {
        taps = length::decimated(taps, &k.decimation, grid);
        cutoff *= grid.ratio.as_f64();
    }
    Ok(Recipe {
        taps,
        cutoff,
        window,
    })
}

/// The builders' window for `w`: each window with the sampling the legacy
/// builders take; other samplings are refused.
pub(crate) fn resolve_window(w: Window) -> Result<fir::Window, DesignError> {
    use Sampling::*;
    Ok(match w {
        Window::Kaiser {
            beta: Some(beta),
            sampling: Periodic,
        } => fir::Window::KaiserPeriodic { beta },
        Window::Gaussian {
            alpha,
            sampling: IntegerCentre,
        } => fir::Window::Gaussian { alpha },
        Window::Flattop {
            set,
            sampling: Periodic,
        } => fir::Window::Flattop(match set {
            FlattopSet::Hft196d => windows::HFT196D,
            FlattopSet::Hft223d => windows::HFT223D,
            FlattopSet::Hft248d => windows::HFT248D,
        }),
        Window::Chebyshev {
            sidelobe_ratio,
            sampling: Symmetric,
        } => fir::Window::ChebyshevSymmetric { r: sidelobe_ratio },
        Window::Nuttall {
            sampling: Symmetric,
        } => fir::Window::NuttallSymmetric,
        Window::Hann { sampling: Periodic } => fir::Window::HannPeriodic,
        other => {
            return Err(DesignError::Unsupported(format!(
                "{other:?}: this window is not available with this sampling yet"
            )));
        }
    })
}

/// The prototype of recipe `r` on `l` branches built on `path`, not
/// normalised.
pub(crate) fn prototype(
    path: ArithmeticPath,
    r: &Recipe,
    l: usize,
) -> Result<Vec<f64>, DesignError> {
    use exact::SincForm::*;
    let built = match path {
        ArithmeticPath::Fir | ArithmeticPath::FirAfter => return Ok(fir::prototype(r, l)),
        ArithmeticPath::ExactPiInverseAfter | ArithmeticPath::ExactPiInversePairwise => {
            exact::exact(
                r,
                l,
                exact::Exact {
                    form: PiCutoffTimesInverseRatio,
                },
            )
        }
        ArithmeticPath::ExactOverRatioBefore => exact::exact(
            r,
            l,
            exact::Exact {
                form: CutoffOverRatio,
            },
        ),
        ArithmeticPath::DoubleDouble => double_double::gaussian(r, l),
        ArithmeticPath::Standard => None,
    };
    built.ok_or_else(|| {
        DesignError::Unsupported(format!(
            "the {path:?} path does not build a {:?} kernel",
            r.window
        ))
    })
}

/// Where a path normalises after the transform.
enum After {
    Sequential,
    Pairwise,
}

/// The kernel of `k` for recipe `r` on `grid`, with its delay in grid
/// samples.
pub(crate) fn kernel(k: &KernelDesign, r: &Recipe, grid: &Grid) -> Result<Kernel, DesignError> {
    let l = grid.up;
    let path = k.numerics.path;
    if k.preserve_samples {
        return Err(DesignError::Unsupported(
            "preserve_samples comes with the standard path".into(),
        ));
    }
    let target = match k.phase {
        Phase::Linear => {
            if !matches!(path, ArithmeticPath::Fir | ArithmeticPath::FirAfter) {
                return Err(DesignError::Unsupported(format!(
                    "a linear-phase kernel on the {path:?} path is not available yet"
                )));
            }
            let mut taps = fir::prototype(r, l);
            fir::normalise(&mut taps, l);
            let delay = taps.len() / 2;
            return Ok(Kernel {
                taps,
                delay,
                delay_exact: delay as f64,
            });
        }
        Phase::Minimum(PhaseMethod::Reflect)
        | Phase::Intermediate {
            method: PhaseMethod::Reflect,
            ..
        } => {
            return Err(DesignError::Unsupported(format!(
                "the reflect phase method comes with the standard path; the {path:?} path does \
                 not build it yet"
            )));
        }
        Phase::Minimum(PhaseMethod::Cepstral) => Target::Minimum,
        Phase::Intermediate {
            mix,
            method: PhaseMethod::Cepstral,
        } => {
            if mix != cepstral::INTERMEDIATE_BLEND {
                return Err(DesignError::Unsupported(format!(
                    "the cepstral transform builds an intermediate mix of {} only (got {mix})",
                    cepstral::INTERMEDIATE_BLEND
                )));
            }
            Target::Intermediate
        }
    };
    let mut h = prototype(path, r, l)?;
    let after = match path {
        ArithmeticPath::Fir => {
            fir::normalise(&mut h, l);
            None
        }
        ArithmeticPath::ExactOverRatioBefore => {
            exact::normalise_sequential(&mut h, l);
            None
        }
        ArithmeticPath::ExactPiInverseAfter => Some(After::Sequential),
        ArithmeticPath::FirAfter
        | ArithmeticPath::ExactPiInversePairwise
        | ArithmeticPath::DoubleDouble => Some(After::Pairwise),
        // `prototype` refuses it; the standard path has its own builder.
        ArithmeticPath::Standard => {
            return Err(DesignError::Unsupported(
                "the standard path builds its own kernels".into(),
            ));
        }
    };
    let mut taps = cepstral::transform(&h, target)?;
    drop(h);
    match after {
        Some(After::Sequential) => exact::normalise_sequential(&mut taps, l),
        Some(After::Pairwise) => fir::normalise(&mut taps, l),
        None => {}
    }
    let delay = cepstral::peak_index(&taps);
    Ok(Kernel {
        taps,
        delay,
        delay_exact: delay as f64,
    })
}
