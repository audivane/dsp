//! The measured automatic length of the preset paths: the shortest length
//! whose kernel keeps the budget on the band edges, with the window's shape
//! fixed (a Chebyshev ratio, a flat-top set, a Gaussian alpha, a Kaiser
//! beta; a Kaiser window without one gets the beta of the budget). The
//! `standard` path keeps its own search ([`crate::design::auto`]).
//!
//! The search variable is the kernel's span in lower-rate samples,
//! `T_low`, so a span measured on one grid means the same at another:
//!
//! 1. The window model ([`crate::design::window_model`]) gives the
//!    estimate; a budget deeper than the window's nominal attenuation is
//!    refused before anything is designed ("the window's sidelobes ... are
//!    above the budget").
//! 2. The linear-phase kernel is measured from [`start_fraction`] of the
//!    estimate, in geometric steps of [`crate::design::auto::AUTO_BRACKET_STEP`]
//!    until one span fails and a longer one passes, then by bisection
//!    (at most [`crate::design::auto::AUTO_MAX_LINEAR_STEPS`] measurements),
//!    against the budget plus [`crate::design::auto::AUTO_MEASURE_MARGIN_DB`].
//!    On grids of more than [`crate::design::auto::AUTO_EXACT_MAX_SCALE`]
//!    samples per lower-rate sample it is measured on the x16 grid (the
//!    same windowed sinc near the band, at a tenth of the cost).
//! 3. The finished kernel (its phase, its real grid) is measured against
//!    the budget itself ([`crate::design::claim`]), and the span stepped up
//!    by 2 % until it passes: at most 12 steps, giving up after 3 in a row
//!    that do not improve. Each candidate is admitted first with the
//!    design's own budgets in the context (`general::admit`: the tap
//!    limits, the rational cost, the cepstral FFT, the workspace cap), so
//!    nothing over a limit is built.
//!
//! Results are cached per design, source rate and ratio.

use super::auto::{
    AUTO_BRACKET_STEP, AUTO_EXACT_MAX_SCALE, AUTO_GRID_STEP, AUTO_GRID_WORSE_STEPS,
    AUTO_MAX_GRID_STEPS, AUTO_MAX_LINEAR_STEPS, AUTO_MAX_TAPS, AUTO_MEASURE_MARGIN_DB,
    protected_error_with,
};
use super::paths::general;
use super::{BuildContext, DesignError, Grid, KernelDesign, Phase, Rates, Shape, Window, claim};
use audivane_protocol::RateRatio;
use realfft::RealFftPlanner;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The search starts at this fraction of the window model's estimate. The
/// model is an upper bound (the worst case over lengths, ratios and
/// cutoffs); the shortest measured lengths sit a little above these
/// fractions (`paths::general::tests::calibrate_search_start`), so the
/// start is just below the smallest.
pub fn start_fraction(w: Option<&Window>) -> f64 {
    match w {
        Some(Window::Kaiser { .. }) => 0.7,
        Some(Window::Gaussian { .. }) => 0.88,
        Some(Window::Chebyshev { .. }) => 0.6,
        Some(Window::Hann { .. }) => 0.75,
        Some(Window::Nuttall { .. }) => 0.88,
        Some(Window::Flattop { .. }) => 0.94,
        _ => 0.7,
    }
}

type Found = Result<usize, DesignError>;

fn cache() -> &'static Mutex<HashMap<String, Found>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Found>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Most cached searches.
const CACHE_CAPACITY: usize = 1024;

/// The limits of a search made without a context (an offline kernel, its
/// pricing): the process's workspace cap and the usual pipelines. A found
/// length does not depend on the limits; only whether one is refused does.
pub(crate) fn limits(source_rate: u32, ratio: RateRatio) -> BuildContext {
    BuildContext {
        source_rate,
        ratio,
        channels: 2,
        max_block: 4096,
        pipelines: super::budget::RATIONAL_SWAP_PIPELINES,
        output: audivane_protocol::OutputConfig::default(),
        workspace: super::budget::design_workspace(),
    }
}

/// The grid taps of `k`'s automatic length in `cx` (see the module docs).
/// Off the real-time thread.
pub(crate) fn search(k: &KernelDesign, cx: &BuildContext) -> Found {
    let key = format!(
        "{k:?}|{}|{}|{}|{}",
        cx.source_rate, cx.ratio, cx.workspace, cx.pipelines
    );
    if let Some(hit) = cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }
    let found = uncached(k, cx);
    if let Ok(mut c) = cache().lock() {
        if c.len() >= CACHE_CAPACITY {
            c.clear();
        }
        c.insert(key, found.clone());
    }
    found
}

/// The grid taps of a span of `t_low` lower-rate samples on a grid of `r`
/// samples per lower-rate sample.
fn taps_at(t_low: usize, r: usize) -> usize {
    (t_low * r).max(1)
}

fn uncached(k: &KernelDesign, cx: &BuildContext) -> Found {
    let source_rate = cx.source_rate;
    let grid = &cx.grid();
    let estimate = general::recipe(k, source_rate, grid, None)?;
    let Some(claim) = estimate.claim else {
        return Err(DesignError::Unsupported(
            "an automatic length needs a band cutoff".into(),
        ));
    };
    let (Shape::WindowedSinc { length, cutoff, .. }, Some(window)) = (&k.shape, estimate.window)
    else {
        return Err(DesignError::Unsupported(
            "an automatic length needs a windowed sinc".into(),
        ));
    };
    let r = grid.design_ratio;
    let t_low_max = (AUTO_MAX_TAPS / r).max(1);
    let est = (estimate.taps()).div_ceil(r).clamp(1, t_low_max);

    // The linear twin on the measuring grid, with the real grid's window
    // (a fallback applies there, not on the x16 grid).
    let proxy = r > AUTO_EXACT_MAX_SCALE;
    let mgrid = if proxy {
        Grid::new(source_rate, RateRatio::integer(AUTO_EXACT_MAX_SCALE as u32))
    } else {
        *grid
    };
    let linear = KernelDesign {
        shape: Shape::WindowedSinc {
            window,
            length: length.clone(),
            cutoff: cutoff.clone(),
        },
        phase: Phase::Linear,
        preserve_samples: false,
        rates: Rates {
            fallback_window: None,
            ..k.rates
        },
        ..k.clone()
    };
    let mr = mgrid.design_ratio;
    let edge = 2.0 * mr as f64;
    let (pass, stop) = (claim.pass / edge, (claim.stop / edge).min(0.5));
    let bound = 10f64.powf(-(claim.db + AUTO_MEASURE_MARGIN_DB) / 20.0);
    let mut planner = RealFftPlanner::<f64>::new();
    let mut steps = 0usize;
    let mut passes = |t_low: usize| -> Result<bool, DesignError> {
        steps += 1;
        if steps > AUTO_MAX_LINEAR_STEPS {
            return Err(DesignError::Claim(format!(
                "the automatic length search exceeded {AUTO_MAX_LINEAR_STEPS} measurements"
            )));
        }
        let t = taps_at(t_low, mr);
        super::budget::check_candidate(t, false, 0).map_err(DesignError::Failed)?;
        super::budget::check_measurement(t, false).map_err(DesignError::Failed)?;
        let g = general::recipe(&linear, source_rate, &mgrid, Some(t))?;
        let h = general::kernel(&linear, &g, &mgrid)?;
        let err = protected_error_with(&mut planner, &h.taps, mgrid.up as f64, pass, stop);
        Ok(err <= bound)
    };
    let fail = || {
        DesignError::Claim(format!(
            "no design met {} dB within {} taps at {}: widen the transition or lower the target",
            claim.db,
            t_low_max * r,
            grid.ratio
        ))
    };
    let start = ((start_fraction(Some(&window)) * est as f64) as usize).clamp(1, t_low_max);
    let (mut lo, mut hi);
    if passes(start)? {
        hi = start;
        lo = 0;
        while hi > 1 {
            let next = ((hi as f64 / AUTO_BRACKET_STEP) as usize).clamp(1, hi - 1);
            if passes(next)? {
                hi = next;
            } else {
                lo = next;
                break;
            }
        }
    } else {
        lo = start;
        loop {
            if lo >= t_low_max {
                return Err(fail());
            }
            let next = ((lo as f64 * AUTO_BRACKET_STEP).ceil() as usize).clamp(lo + 1, t_low_max);
            if passes(next)? {
                hi = next;
                break;
            }
            lo = next;
        }
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if passes(mid)? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    drop(planner);

    // The finished kernel on its own grid, stepped up until it keeps the
    // promise.
    let mut t_low = hi;
    let (mut best, mut worse, mut failures) = (f64::NEG_INFINITY, 0usize, 0usize);
    let mut last = None;
    for _ in 0..=AUTO_MAX_GRID_STEPS {
        if t_low > t_low_max {
            return Err(fail());
        }
        let t = taps_at(t_low, r);
        let g = general::recipe(k, source_rate, grid, Some(t))?;
        // The design's own limits at this length, before it is built.
        general::admit(k, &g, cx, true)?;
        let transformed = k.phase != Phase::Linear;
        super::budget::check_candidate(t, transformed, 0).map_err(DesignError::Failed)?;
        match general::kernel(k, &g, grid) {
            Ok(h) => {
                super::budget::check_measurement(h.taps.len(), true)
                    .map_err(DesignError::Failed)?;
                let m = claim::measure(&mut RealFftPlanner::new(), &h.taps, &claim, grid);
                if m.keeps(&claim) {
                    return Ok(t);
                }
                last = Some(m);
                if m.complete && m.db > best {
                    (best, worse) = (m.db, 0);
                } else {
                    worse += 1;
                }
            }
            Err(e @ DesignError::Phase(_)) => {
                failures += 1;
                if failures >= 3 {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
        if worse >= AUTO_GRID_WORSE_STEPS {
            break;
        }
        t_low = ((t_low as f64 * AUTO_GRID_STEP).ceil() as usize).max(t_low + 1);
    }
    Err(DesignError::Claim(format!(
        "no design met {} dB at {} above {} taps (best {best:.2} dB{}): widen the transition or \
         lower the target",
        claim.db,
        grid.ratio,
        taps_at(hi, r),
        last.map_or(String::new(), |m| format!(", last {:.2} dB", m.db))
    )))
}
