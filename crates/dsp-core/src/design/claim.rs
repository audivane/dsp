//! The promise check of the new combinations: a design that promises an
//! attenuation (a band or automatic length, a stop-at-Nyquist cutoff,
//! [`Claim`]) is measured once built, and refused when it misses. Nothing
//! that promises a level builds without measuring at it.
//!
//! The measurement is the automatic search's
//! ([`crate::design::auto::protected_error_bounded`]): the relative error
//! against unity up to the passband edge and against zero from the
//! stopband edge to the grid Nyquist, on 32 points per tap plus the exact
//! band edges, in FFTs of at most 2^23 points. A grid can sit beside a
//! peak (by up to about 0.5 % of it at 32 points per tap), so a kernel the
//! grid passes is not accepted on the grid alone: every peak within
//! [`PEAK_SLACK`] of the promise is refined by direct evaluation of the
//! response ([`refine`]), and the error is the largest refined value.
//! Results are cached by the kernel's bits and the promise.

pub use super::paths::general::Claim;
use super::{DesignError, Grid, Kernel, budget};
use realfft::RealFftPlanner;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// What a check found: the measured attenuation (dB, the smallest margin
/// under unity error), and whether it was measured in full (a measurement
/// stopped once it exceeded the promise is a bound).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measured {
    pub db: f64,
    pub complete: bool,
}

impl Measured {
    /// Whether it keeps `claim`.
    pub fn keeps(&self, claim: &Claim) -> bool {
        self.complete && self.db >= claim.db
    }
}

/// Most cached checks.
const CACHE_CAPACITY: usize = 4096;

fn cache() -> &'static Mutex<HashMap<u64, Measured>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Measured>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(taps: &[f64], claim: &Claim, grid: &Grid) -> u64 {
    crate::bypass::hash_u64(
        [
            claim.pass.to_bits(),
            claim.stop.to_bits(),
            claim.db.to_bits(),
            grid.up as u64,
            grid.design_ratio as u64,
            taps.len() as u64,
        ]
        .into_iter()
        .chain(taps.iter().map(|v| v.to_bits())),
    )
}

/// The workspace of a check of a `len`-tap kernel.
pub fn workspace(len: usize) -> u64 {
    budget::measurement_workspace(len, true)
}

/// Measure `taps` (a kernel on `grid`, each branch summing to about 1)
/// against `claim`: stops early once the error is past the promise.
pub fn measure(
    planner: &mut RealFftPlanner<f64>,
    taps: &[f64],
    claim: &Claim,
    grid: &Grid,
) -> Measured {
    let k = key(taps, claim, grid);
    if let Some(m) = cache().lock().ok().and_then(|c| c.get(&k).copied()) {
        return m;
    }
    let edge = 2.0 * grid.design_ratio.max(1) as f64;
    let bound = 10f64.powf(-claim.db / 20.0);
    let (gain, pass, stop) = (
        grid.up.max(1) as f64,
        claim.pass / edge,
        (claim.stop / edge).min(0.5),
    );
    let (err, complete) = error(planner, taps, gain, pass, stop, bound);
    let m = Measured {
        db: if err > 0.0 {
            -20.0 * err.log10()
        } else {
            f64::INFINITY
        },
        complete,
    };
    if let Ok(mut c) = cache().lock() {
        if c.len() >= CACHE_CAPACITY {
            c.clear();
        }
        c.insert(k, m);
    }
    m
}

/// How far under the promise (relative error) a grid peak may sit and
/// still be refined: well over the most a 32-points-per-tap grid misses a
/// lobe's peak by (`1 - cos(pi / 32)`, 0.5 %, for a lobe half a bin wide).
pub const PEAK_SLACK: f64 = 0.02;

/// Most grid points near the promise kept for refinement, and most peaks
/// refined; past them the remaining peaks count as `(1 + PEAK_SLACK)`
/// times their grid value (a refusal near the promise, never a miss).
const MAX_POINTS: usize = 1 << 16;
const MAX_PEAKS: usize = 512;

/// The protected error of `taps` against `bound` (relative): the grid's
/// ([`crate::design::auto::protected_error_scan`]), its peaks within
/// [`PEAK_SLACK`] of `bound` refined ([`refine`]). Incomplete (a lower
/// bound) when the grid stopped past `bound`.
pub(crate) fn error(
    planner: &mut RealFftPlanner<f64>,
    taps: &[f64],
    gain: f64,
    pass: f64,
    stop: f64,
    bound: f64,
) -> (f64, bool) {
    let threshold = bound / (1.0 + PEAK_SLACK);
    let mut points = Vec::new();
    let mut overflow = false;
    let (err, complete, step) = crate::design::auto::protected_error_scan(
        planner,
        taps,
        gain,
        pass,
        stop,
        bound,
        &mut |f, e| {
            if e >= threshold {
                if points.len() < MAX_POINTS {
                    points.push((f, e));
                } else {
                    overflow = true;
                }
            }
        },
    );
    if !complete || err > bound {
        return (err, complete);
    }
    let mut err = err;
    if overflow {
        err = err.max(err * (1.0 + PEAK_SLACK));
    }
    // One peak per run of neighbouring grid points: its largest point.
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut peaks: Vec<(f64, f64)> = Vec::new();
    let mut last = f64::NEG_INFINITY;
    for &(f, e) in &points {
        let joined = f - last <= 1.5 * step && (f <= pass) == (last <= pass);
        last = f;
        match peaks.last_mut() {
            Some(p) if joined => {
                if e > p.1 {
                    *p = (f, e);
                }
            }
            _ => peaks.push((f, e)),
        }
    }
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (i, &(f, e)) in peaks.iter().enumerate() {
        err = err.max(if i < MAX_PEAKS {
            refine(taps, gain, pass, stop, f, step)
        } else {
            e * (1.0 + PEAK_SLACK)
        });
    }
    (err, true)
}

/// The response of `taps` at `f` (cycles per sample), summed directly in
/// blocks whose phase is reduced exactly (`f t mod 1` with the product's
/// rounding error), so a long kernel keeps the accuracy of a short one.
pub fn response(taps: &[f64], f: f64) -> rustfft::num_complex::Complex64 {
    use rustfft::num_complex::Complex64;
    const BLOCK: usize = 4096;
    let step = Complex64::from_polar(1.0, -2.0 * std::f64::consts::PI * f);
    let mut total = Complex64::new(0.0, 0.0);
    for (b, block) in taps.chunks(BLOCK).enumerate() {
        let t0 = (b * BLOCK) as f64;
        let p = f * t0;
        let frac = (p - p.round()) + f.mul_add(t0, -p);
        let anchor = Complex64::from_polar(1.0, -2.0 * std::f64::consts::PI * frac);
        let sum = block
            .iter()
            .rev()
            .fold(Complex64::new(0.0, 0.0), |a, &h| a * step + h);
        total += anchor * sum;
    }
    total
}

/// The protected error at `f` (in a protected band): against unity up to
/// `pass`, against zero past it.
fn error_at(taps: &[f64], gain: f64, pass: f64, f: f64) -> f64 {
    let a = response(taps, f).norm() / gain;
    if f <= pass { (a - 1.0).abs() } else { a }
}

/// The peak of the protected error near the grid point `f0` (a grid's
/// largest point of one lobe, so the lobe's peak lies within one `step`
/// of it), by direct evaluation: a parabola through three points half a
/// step apart, then one through three an eighth of a step apart around
/// its vertex; the largest value evaluated. Kept inside `f0`'s band.
pub fn refine(taps: &[f64], gain: f64, pass: f64, stop: f64, f0: f64, step: f64) -> f64 {
    let (lo, hi) = if f0 <= pass { (0.0, pass) } else { (stop, 0.5) };
    let (lo, hi) = (lo.max(f0 - step), hi.min(f0 + step));
    let e = |f: f64| error_at(taps, gain, pass, f.clamp(lo, hi));
    let mut best = f64::NEG_INFINITY;
    let mut centre = f0;
    for h in [0.5 * step, 0.125 * step] {
        let (a, b, c) = (e(centre - h), e(centre), e(centre + h));
        best = best.max(a).max(b).max(c);
        let denom = a - 2.0 * b + c;
        let shift = if denom < 0.0 {
            (0.5 * h * (a - c) / denom).clamp(-2.0 * h, 2.0 * h)
        } else if a > c {
            -h
        } else {
            h
        };
        centre = (centre + shift).clamp(lo, hi);
        best = best.max(e(centre));
    }
    best
}

/// Refuse `kernel` when it misses `claim` on `grid`.
pub fn verify(kernel: &Kernel, claim: &Claim, grid: &Grid) -> Result<Measured, DesignError> {
    let m = measure(&mut RealFftPlanner::new(), &kernel.taps, claim, grid);
    if m.keeps(claim) {
        return Ok(m);
    }
    let measured = if m.complete {
        format!("{:.2} dB", m.db)
    } else {
        format!("at most {:.2} dB", m.db)
    };
    Err(DesignError::Claim(format!(
        "the {}-tap kernel at {} measures {measured} over its band, short of the {} dB it \
         promises: lengthen it, widen the transition or lower the target",
        kernel.taps.len(),
        grid.ratio,
        claim.db
    )))
}
