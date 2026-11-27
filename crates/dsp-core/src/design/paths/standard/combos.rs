//! The `standard` path's new combinations: what the Custom builder
//! does not build, on its own conventions (the exact centre, the sequential
//! `R / sum` scale before and after the phase):
//!
//! - the `scaled` grid cutoff: when decimating, the kernel is built at the
//!   branch count `L` with the cutoff times `out / in` (rounded to f64),
//!   with no `L / R` scale after;
//! - the cepstral phase method ([`cepstral::transform`]) for minimum and
//!   intermediate phase, any mix (0 the minimum phase, 1 the linear
//!   kernel);
//! - `periodic` sampling of the Kaiser and Tukey windows.
//!
//! The lengths, betas and cutoffs are the Custom builder's own: the design
//! is resolved as its legacy twin ([`twin`]: `design_ratio`, symmetric,
//! linear in place of a cepstral phase) by [`super::recipe`], and only the
//! kernel is built here. A promise (a band, an automatic budget, stop at
//! Nyquist) is measured on the finished kernel ([`crate::design::claim`]);
//! an automatic length is stepped up by 2 % until its kernel keeps it, as
//! the rational grid check of [`crate::design::auto`] does.

use super::{UpsamplerDesign, kernel_delay, kernel_delay_exact, minimum_phase_contract, recipe};
use crate::design::claim::{self, Claim};
use crate::design::phase::cepstral::{self, Target};
use crate::design::{
    BuildContext, Cutoff, DesignError, FilterDesign, FilterErrorBudget, GridCutoff, Kernel,
    KernelDesign, Length, Numerics, Phase, PhaseMethod, Plan, Sampling, Shape, Window, budget,
    check,
};
use audivane_protocol::{FilterPhase, RateRatio, WindowKind};
use std::f64::consts::PI;

/// Whether `k` is one of the Custom designs, or refused as the Custom
/// builder has always refused it: everything but the three combinations
/// this module adds (the `scaled` grid cutoff, a cepstral phase, a
/// periodic Kaiser or Tukey window). Those keep their bits and refusals.
pub(crate) fn legacy(k: &KernelDesign) -> bool {
    let periodic = matches!(
        k.shape,
        Shape::WindowedSinc {
            window: Window::Kaiser {
                sampling: Sampling::Periodic,
                ..
            } | Window::Tukey {
                sampling: Sampling::Periodic,
                ..
            },
            ..
        }
    );
    !(periodic
        || k.numerics.grid_cutoff == GridCutoff::Scaled
        || matches!(
            k.phase,
            Phase::Minimum(PhaseMethod::Cepstral)
                | Phase::Intermediate {
                    method: PhaseMethod::Cepstral,
                    ..
                }
        ))
}

/// The legacy twin of `k`: what the Custom builder resolves its length,
/// beta and cutoff from.
pub(crate) fn twin(k: &KernelDesign) -> KernelDesign {
    let mut t = k.clone();
    t.numerics = Numerics {
        grid_cutoff: GridCutoff::DesignRatio,
        ..k.numerics
    };
    if let Shape::WindowedSinc { window, .. } = &mut t.shape {
        *window = match *window {
            Window::Kaiser { beta, .. } => Window::Kaiser {
                beta,
                sampling: Sampling::Symmetric,
            },
            Window::Tukey { taper, .. } => Window::Tukey {
                taper,
                sampling: Sampling::Symmetric,
            },
            w => w,
        };
    }
    if matches!(
        k.phase,
        Phase::Minimum(PhaseMethod::Cepstral)
            | Phase::Intermediate {
                method: PhaseMethod::Cepstral,
                ..
            }
    ) {
        t.phase = Phase::Linear;
    }
    t
}

/// The cepstral transform `k` needs, if any (`None` in the target: the
/// linear kernel, a mix of 1).
fn cepstral_target(phase: Phase) -> Option<Option<Target>> {
    match phase {
        Phase::Minimum(PhaseMethod::Cepstral) => Some(Some(Target::Minimum)),
        Phase::Intermediate {
            mix,
            method: PhaseMethod::Cepstral,
        } => Some(if mix >= 1.0 {
            None
        } else if mix <= 0.0 {
            Some(Target::Minimum)
        } else if mix == cepstral::INTERMEDIATE_BLEND {
            Some(Target::Intermediate)
        } else {
            Some(Target::Mix(mix))
        }),
        _ => None,
    }
}

/// What `k` promises at the resolved design `r` (`band_rate`: the lower of
/// the input and output rates, [`crate::design::band_rate`], where a band's
/// absolute passband end resolves to a fraction).
fn claim_of(
    k: &KernelDesign,
    r: &UpsamplerDesign,
    band_rate: u32,
) -> Result<Option<Claim>, DesignError> {
    let Shape::WindowedSinc { length, cutoff, .. } = &k.shape else {
        return Ok(None);
    };
    Ok(match (length, cutoff) {
        (Length::Band, Cutoff::Band(b)) => Some(Claim {
            pass: crate::design::band_passband_end(b, band_rate),
            stop: b.stopband_start,
            db: b.attenuation_db,
        }),
        (Length::Auto(FilterErrorBudget::Db(a)), Cutoff::Band(b)) => Some(Claim {
            pass: crate::design::band_passband_end(b, band_rate),
            stop: b.stopband_start,
            db: *a,
        }),
        (Length::Auto(FilterErrorBudget::MatchOutput), _) => {
            return Err(DesignError::Unsupported(
                "\"match_output\" must be resolved against the output settings before design"
                    .into(),
            ));
        }
        (_, Cutoff::NyquistStop) => Some(Claim {
            pass: 0.0,
            stop: 1.0,
            db: crate::resample::kaiser_attenuation_db(r.kaiser_beta),
        }),
        _ => None,
    })
}

/// The window of `k` over `n` taps.
fn window(k: &KernelDesign, r: &UpsamplerDesign, n: usize) -> Vec<f64> {
    match &k.shape {
        Shape::WindowedSinc {
            window:
                Window::Kaiser {
                    sampling: Sampling::Periodic,
                    ..
                },
            ..
        } => crate::design::paths::fir::windows::kaiser_periodic(n, r.kaiser_beta),
        Shape::WindowedSinc {
            window:
                Window::Tukey {
                    taper,
                    sampling: Sampling::Periodic,
                },
            ..
        } => crate::design::paths::fir::windows::tukey_periodic(n, *taper),
        _ => match r.window {
            WindowKind::Kaiser => crate::window::kaiser(n, r.kaiser_beta),
            WindowKind::Gaussian => {
                crate::window::gaussian(n, crate::resample::kaiser_attenuation_db(r.kaiser_beta))
            }
            WindowKind::Tukey => crate::window::tukey(n, r.taper),
        },
    }
}

/// Scale `h` so it sums to `target`.
fn scale_to(h: &mut [f64], target: f64) {
    let sum: f64 = h.iter().sum();
    if sum.abs() > 0.0 {
        let s = target / sum;
        for v in h.iter_mut() {
            *v *= s;
        }
    }
}

/// The kernel of `k` (resolved as `r` by the Custom builder) on the grid
/// of `ratio`, on the standard path's conventions (module docs).
pub(crate) fn kernel(
    k: &KernelDesign,
    r: &UpsamplerDesign,
    ratio: RateRatio,
) -> Result<Kernel, DesignError> {
    let up = ratio.up as usize;
    let scaled = k.numerics.grid_cutoff == GridCutoff::Scaled && ratio.is_decimating();
    // The ratio the kernel is built at, its cutoff there, and the final
    // scale to the L branches.
    let (b, cutoff, scale) = if scaled {
        (up, r.cutoff * ratio.as_f64(), 1.0)
    } else {
        (r.ratio, r.cutoff, up as f64 / r.ratio as f64)
    };
    let taps = if r.preserve_samples && r.taps.is_multiple_of(2) {
        r.taps - 1
    } else {
        r.taps
    };
    if taps == 0 {
        return Err(DesignError::Unsupported("the kernel has no taps".into()));
    }
    let bf = b as f64;
    let win = window(k, r, taps);
    let centre = (taps - 1) as f64 / 2.0;
    let mut h: Vec<f64> = (0..taps)
        .map(|i| {
            let t = i as f64 - centre;
            let s = if t == 0.0 {
                1.0
            } else {
                let arg = PI * cutoff * t / bf;
                arg.sin() / arg
            };
            win[i] * cutoff * s
        })
        .collect();
    drop(win);
    scale_to(&mut h, bf);
    let phase = match k.phase {
        Phase::Linear => FilterPhase::Linear,
        Phase::Minimum(_) => FilterPhase::Minimum,
        Phase::Intermediate { .. } => FilterPhase::Intermediate,
    };
    if r.preserve_samples && phase == FilterPhase::Linear {
        if b != up {
            return Err(DesignError::Unsupported(
                "preserve_samples needs an output rate at or above the source rate".into(),
            ));
        }
        let c = (taps - 1) / 2;
        h[c] = 1.0;
        let mut j = b;
        while j <= c {
            h[c - j] = 0.0;
            h[c + j] = 0.0;
            j += b;
        }
    } else {
        let d = UpsamplerDesign {
            ratio: b,
            taps,
            cutoff,
            ..*r
        };
        let twin = match (cepstral_target(k.phase), k.phase) {
            (Some(Some(t)), _) if taps > 1 => Some(cepstral::transform(&h, t)?),
            (Some(_), _) => None,
            (None, Phase::Minimum(_)) if taps > 1 => Some(
                crate::minphase::minimum_phase(&h, &minimum_phase_contract(&d))
                    .map_err(DesignError::Phase)?
                    .kernel,
            ),
            (None, Phase::Intermediate { mix, .. }) if taps > 1 && mix < 1.0 => Some(
                crate::minphase::intermediate_phase(&h, &minimum_phase_contract(&d), mix)
                    .map_err(DesignError::Phase)?
                    .kernel,
            ),
            _ => None,
        };
        if let Some(twin) = twin {
            h = twin;
            scale_to(&mut h, bf);
        }
    }
    if scale != 1.0 {
        for v in &mut h {
            *v *= scale;
        }
    }
    Ok(Kernel {
        delay: kernel_delay(&h, phase),
        delay_exact: kernel_delay_exact(&h, phase),
        taps: h,
    })
}

/// Whether `k` is phase-converted, and how much its kernel can grow.
fn growth(k: &KernelDesign) -> (usize, bool) {
    match cepstral_target(k.phase) {
        Some(Some(Target::Intermediate | Target::Mix(_))) => (4, true),
        Some(_) => (1, true),
        None => match k.phase {
            Phase::Intermediate { mix, .. } if mix > 0.0 && mix < 1.0 => (4, false),
            _ => (1, true),
        },
    }
}

/// The plan of `k` at the resolved design `r` in `cx`.
fn plan(k: &KernelDesign, r: &UpsamplerDesign, cx: &BuildContext, exact: bool) -> Plan {
    let n = r.taps;
    let (growth, exact_growth) = growth(k);
    let mut workspace = match cepstral_target(k.phase) {
        Some(Some(_)) => budget::cepstral_workspace(n),
        _ => match k.phase {
            Phase::Linear => budget::linear_workspace(n),
            Phase::Intermediate { mix, .. } if mix >= 1.0 => budget::linear_workspace(n),
            _ => budget::reflect_workspace(n),
        },
    };
    if claim_of(k, r, crate::design::band_rate(cx.source_rate, cx.ratio))
        .ok()
        .flatten()
        .is_some()
    {
        workspace = workspace.max(claim::workspace(n * growth));
    }
    let priced = match cepstral_target(k.phase) {
        Some(Some(Target::Intermediate | Target::Mix(_))) => 2 * n,
        Some(_) => n,
        None => budget::standard_priced_taps(k, r),
    };
    Plan::kernel(
        cx,
        n * growth,
        exact && exact_growth,
        workspace,
        priced,
        true,
    )
}

fn budgets(k: &KernelDesign, n: usize, cx: &BuildContext) -> Result<(), DesignError> {
    let (growth, exact_growth) = growth(k);
    if cx.ratio.is_integer() {
        budget::check_kernel_taps(n, cx.ratio, growth == 4)?;
    } else {
        budget::check_rational(
            if exact_growth { n * growth } else { n },
            cx.ratio,
            cx.source_rate,
            budget::MAX_RATIONAL_GRID_TAPS,
            cx.pipelines,
        )?;
    }
    if matches!(cepstral_target(k.phase), Some(Some(_))) {
        budget::check_cepstral(n, cx.ratio)?;
    }
    Ok(())
}

fn other(k: &KernelDesign) -> Result<(), DesignError> {
    if matches!(k.shape, Shape::Lanczos { .. }) {
        return Err(DesignError::Unsupported(
            "the standard path does not build a Lanczos kernel".into(),
        ));
    }
    if k.rates.fallback_window.is_some() {
        return Err(DesignError::Unsupported(
            "the standard path has no fallback window".into(),
        ));
    }
    if let Shape::WindowedSinc {
        window:
            Window::GaussianStopband {
                sampling: Sampling::Periodic,
                ..
            },
        ..
    } = k.shape
    {
        return Err(DesignError::Unsupported(
            "the standard path's Gaussian is sampled symmetric".into(),
        ));
    }
    Ok(())
}

/// [`crate::design::feasibility`] of the new combination `k` (`d` as a
/// design) on the standard path: the bounds, the twin's resolution and
/// checks, the budgets of this kernel.
pub(crate) fn feasibility(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
) -> Result<Plan, DesignError> {
    check::check(d).map_err(DesignError::Field)?;
    other(k)?;
    let t = twin(k);
    let twin_plan = super::feasibility(&FilterDesign::Kernel(t.clone()), &t, cx)?;
    let n = twin_plan.grid_taps;
    budgets(k, n, cx)?;
    // The estimate of an automatic length (its search runs in `prepare`).
    let r = UpsamplerDesign {
        taps: n,
        ..recipe_estimate(&t, cx)?
    };
    plan(k, &r, cx, twin_plan.grid_taps_exact).check_workspace(cx)
}

/// The twin's resolution without its search (an automatic length's beta
/// and cutoff, at an estimated length).
fn recipe_estimate(t: &KernelDesign, cx: &BuildContext) -> Result<UpsamplerDesign, DesignError> {
    let mut est = t.clone();
    if let Shape::WindowedSinc { length, .. } = &mut est.shape
        && matches!(length, Length::Auto(_))
    {
        // The search's window is designed for the budget plus its margin;
        // the band rule gives the same beta and cutoff.
        let Length::Auto(FilterErrorBudget::Db(a)) = *length else {
            return Err(DesignError::Unsupported(
                "\"match_output\" must be resolved against the output settings before design"
                    .into(),
            ));
        };
        *length = Length::Band;
        if let Shape::WindowedSinc {
            cutoff: Cutoff::Band(b),
            ..
        } = &mut est.shape
        {
            b.attenuation_db = a + crate::design::auto::AUTO_BETA_MARGIN_DB;
        }
    }
    recipe(&est, cx.source_rate, cx.ratio)
}

/// [`crate::design::prepare`] of the new combination `k` on the standard
/// path, up to the safety gate.
pub(crate) fn prepare(
    d: &FilterDesign,
    k: &KernelDesign,
    cx: &BuildContext,
) -> Result<(Plan, Kernel), DesignError> {
    check::check(d).map_err(DesignError::Field)?;
    other(k)?;
    let t = twin(k);
    let (_, mut r) = super::searched(&FilterDesign::Kernel(t.clone()), &t, cx)?;
    budgets(k, r.taps, cx)?;
    let claim = claim_of(k, &r, crate::design::band_rate(cx.source_rate, cx.ratio))?;
    let grid = cx.grid();
    let auto = matches!(
        k.shape,
        Shape::WindowedSinc {
            length: Length::Auto(_),
            ..
        }
    );
    let kernel = match claim {
        Some(c) if auto => {
            // Step the length up until the kernel keeps its budget.
            let scale = r.ratio.max(1);
            let mut half = (r.taps.saturating_sub(1) / (2 * scale)).max(1);
            let mut best = f64::NEG_INFINITY;
            let mut worse = 0;
            let mut found = None;
            for _ in 0..=crate::design::auto::AUTO_MAX_GRID_STEPS {
                let candidate = UpsamplerDesign {
                    taps: 2 * half * scale + 1,
                    ..r
                };
                budgets(k, candidate.taps, cx)?;
                let h = kernel(k, &candidate, cx.ratio)?;
                let m = claim::measure(&mut realfft::RealFftPlanner::new(), &h.taps, &c, &grid);
                if m.keeps(&c) {
                    r = candidate;
                    found = Some(h);
                    break;
                }
                if m.complete && m.db > best {
                    (best, worse) = (m.db, 0);
                } else {
                    worse += 1;
                    if worse >= crate::design::auto::AUTO_GRID_WORSE_STEPS {
                        break;
                    }
                }
                half = ((half as f64 * crate::design::auto::AUTO_GRID_STEP).ceil() as usize)
                    .max(half + 1);
            }
            found.ok_or_else(|| {
                DesignError::Claim(format!(
                    "no design met {} dB at {} above {} taps (best {best:.2} dB): widen the \
                     transition or lower the target",
                    c.db, cx.ratio, r.taps
                ))
            })?
        }
        _ => kernel(k, &r, cx.ratio)?,
    };
    if let Some(c) = claim {
        claim::verify(&kernel, &c, &grid)?;
    }
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
    let plan = Plan {
        grid_taps: len,
        grid_taps_exact: true,
        resident_bytes: budget::kernel_resident(len, cx),
        ..plan(k, &r, cx, true)
    };
    Ok((plan.check_workspace(cx)?, kernel))
}

/// The kernel of the new combination `k` from `source_rate` at `ratio` (an
/// automatic length searched and stepped, its promise measured); no
/// budget checked beyond the twin's.
pub(crate) fn kernel_at(
    k: &KernelDesign,
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
    prepare(&FilterDesign::Kernel(k.clone()), k, &cx).map(|(_, kernel)| kernel)
}

/// The grid taps of `k` from `source_rate` at `ratio` (an automatic length
/// at its twin's searched length) and the taps its runtime is priced at.
pub(crate) fn taps(
    k: &KernelDesign,
    source_rate: u32,
    ratio: RateRatio,
) -> Result<(usize, usize), DesignError> {
    let r = recipe(&twin(k), source_rate, ratio)?;
    let (growth, _) = growth(k);
    let priced = match cepstral_target(k.phase) {
        Some(Some(Target::Intermediate | Target::Mix(_))) => 2 * r.taps,
        Some(_) => r.taps,
        None => budget::standard_priced_taps(k, &r),
    };
    Ok((r.taps * growth, priced))
}

/// The plan of `k` in `cx` with its length resolved (the twin's search),
/// no budget checked.
pub(crate) fn resolved_plan(k: &KernelDesign, cx: &BuildContext) -> Result<Plan, DesignError> {
    let r = recipe(&twin(k), cx.source_rate, cx.ratio)?;
    Ok(plan(k, &r, cx, true))
}
