//! Optimal linear-phase low-pass prototypes: equiripple (Parks-McClellan)
//! and least squares, designed directly rather than by a window.
//!
//! Both design an odd length `N = 2M + 1` (type I, symmetric about the
//! centre tap) with unit gain up to `pass` and zero from `stop` (cycles per
//! sample) and no constraint between them:
//!
//! - **Equiripple** minimises the largest error in either band, the
//!   passband's weighted twice (the engine normalises the DC gain to unity,
//!   and DC sits on a passband ripple peak, so the error it measures against
//!   unity is up to twice the passband ripple: a 2 to 1 weight levels the
//!   measured errors of both bands). The Remez exchange runs on
//!   `A(x) = sum c_k T_k(x)`, `x = cos(2 pi f)`, with barycentric
//!   interpolation and weights kept as mantissa and binary exponent, so no
//!   product of node distances overflows. Its cost per iteration is
//!   `O(M * grid)`, about `16 M^2`, started from the least-squares design's
//!   error peaks. It is a short-filter design, up to [`EQUIRIPPLE_MAX_TAPS`]
//!   taps: past about 4.6k taps the exchange loses its alternation in
//!   clusters of lengths, and a length a few taps away may not converge. A
//!   length the exchange does not level is retried 2 and 4 taps shorter
//!   (zero-padded back; the automatic search's bracket tries other lengths
//!   first).
//! - **Least squares** minimises the summed squared error over both bands:
//!   the normal equations `Q c = r` with `Q` Toeplitz plus Hankel, solved by
//!   conjugate gradients with FFT products (`O(M log M)` per iteration).
//!   `Q` is nearly singular along the sequences concentrated in the
//!   transition band (their Slepian eigenvalues), but those directions move
//!   the response only inside the transition, where the error is not
//!   counted, so the iteration converges on the error that matters. (A
//!   weight on the transition would bound them, but one large enough to
//!   matter, 1e-3 on a linear ramp, costs the bands 55 to 70 dB.) Its error
//!   floors at 137 to 153 dB, and a length whose estimated depth is past
//!   [`LEAST_SQUARES_LEVEL_DB`] is refused (far past the floor the solve
//!   stalls with a passband boost); up to [`LEAST_SQUARES_MAX_TAPS`] taps,
//!   past which every length is under the floor.
//!
//! The taps come back with unit DC gain (the caller normalises them as any
//! prototype).

use crate::design::DesignError;
use realfft::RealFftPlanner;
use rustfft::num_complex::Complex64;
use std::f64::consts::PI;

/// The longest equiripple design, taps (see the module docs).
pub const EQUIRIPPLE_MAX_TAPS: usize = 4095;

/// The deepest estimated depth ([`estimate_db`]) of an equiripple length,
/// dB: deeper, the exchange's ripple is below what f64 levels (it loses
/// its alternation from about 165 dB on short kernels).
pub const EQUIRIPPLE_LEVEL_DB: f64 = 160.0;

/// The deepest estimated depth of a least-squares length, dB: far past its
/// 137 to 153 dB floor the solve stalls with a passband boost (a 512-tap
/// x2 design over 0.5 to 1.5 of the Nyquist, about 1800 dB, came out
/// +23 dB).
pub const LEAST_SQUARES_LEVEL_DB: f64 = 200.0;

/// The estimated depth of an `n`-tap optimal design over `pass` to `stop`
/// (cycles per sample), dB: Kaiser's formula `7.95 + 14.36 df (n - 1)`
/// (an equiripple design measures a few dB deeper, least squares about
/// 10 dB shallower).
pub fn estimate_db(n: usize, pass: f64, stop: f64) -> f64 {
    7.95 + 14.36 * (stop - pass) * (n.max(1) - 1) as f64
}

/// The deepest automatic budget of an equiripple design, dB, as the engine
/// measures it: the exchange levels its error down to a ripple of about
/// 1e-10 on short kernels and loses its alternation below
/// (`measure_limits`; with the 2 to 1 passband weight the passband ripple
/// is half as deep again, and 1791 taps x8, about 165 dB, no longer
/// converges).
pub const EQUIRIPPLE_MAX_DB: f64 = audivane_protocol::design::EQUIRIPPLE_MAX_DB;

/// The deepest automatic budget of a least-squares design, dB: its solve
/// floors at 137 to 153 dB (`measure_limits`).
pub const LEAST_SQUARES_MAX_DB: f64 = audivane_protocol::design::LEAST_SQUARES_MAX_DB;

/// The longest least-squares design, taps.
pub const LEAST_SQUARES_MAX_TAPS: usize = 65_535;

/// Dense grid points per extremal of the equiripple exchange.
const GRID_DENSITY: usize = 16;

/// The denser grid a failed exchange is retried on (a different grid
/// moves the extremals off the points where the f64 error is noise).
const DENSE_RETRY: usize = 24;

/// Most Remez exchanges.
const MAX_EXCHANGES: usize = 100;

/// Most conjugate-gradient iterations of the least-squares solve.
const MAX_CG: usize = 4000;

/// A design method.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Equiripple,
    LeastSquares,
}

fn fail<T>(m: impl Into<String>) -> Result<T, DesignError> {
    Err(DesignError::Failed(m.into()))
}

/// The `n`-tap (odd) optimal low-pass of `method`, unit gain up to `pass`
/// and zero from `stop` (cycles per sample, `0 < pass < stop < 0.5`);
/// `retry`: an equiripple length that does not level is retried 2 and 4
/// taps shorter, zero-padded back to `n`.
///
/// Cached by its arguments (the result, refusals included, is a function
/// of them alone): applying one automatic design checks it at every
/// source rate and ratio the target can reach (the config validation, the
/// rate-pair mask, the design check, the build), and their searches try
/// the same prototypes over and over, an equiripple one costing up to a
/// second.
pub fn design(
    method: Method,
    n: usize,
    pass: f64,
    stop: f64,
    retry: bool,
) -> Result<Vec<f64>, DesignError> {
    let key = (method, n, pass.to_bits(), stop.to_bits(), retry);
    if let Some(hit) = cache::get(&key) {
        return hit.map(|taps| taps.to_vec());
    }
    let result = design_uncached(method, n, pass, stop, retry);
    cache::put(
        key,
        result
            .as_ref()
            .map(|t| t.as_slice().into())
            .map_err(Clone::clone),
    );
    result
}

/// The process-wide cache of [`design`].
mod cache {
    use super::{DesignError, Method};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    pub(super) type Key = (Method, usize, u64, u64, bool);
    type Entry = Result<Arc<[f64]>, DesignError>;

    /// Most taps kept (64 MiB) and most entries (a refusal keeps no taps);
    /// past either the cache starts over.
    const MAX_TAPS: usize = 8 << 20;
    const MAX_ENTRIES: usize = 4096;

    #[derive(Default)]
    struct Cache {
        entries: HashMap<Key, Entry>,
        taps: usize,
    }

    fn cache() -> &'static Mutex<Cache> {
        static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
        CACHE.get_or_init(Default::default)
    }

    pub(super) fn get(key: &Key) -> Option<Entry> {
        cache().lock().ok()?.entries.get(key).cloned()
    }

    pub(super) fn put(key: Key, entry: Entry) {
        let Ok(mut c) = cache().lock() else {
            return;
        };
        let len = entry.as_ref().map_or(0, |t| t.len());
        if c.taps + len > MAX_TAPS || c.entries.len() >= MAX_ENTRIES {
            c.entries.clear();
            c.taps = 0;
        }
        if c.entries.insert(key, entry).is_none() {
            c.taps += len;
        }
    }
}

fn design_uncached(
    method: Method,
    n: usize,
    pass: f64,
    stop: f64,
    retry: bool,
) -> Result<Vec<f64>, DesignError> {
    if n < 3 || n.is_multiple_of(2) {
        return fail(format!(
            "an optimal design needs an odd length of 3 or more (got {n})"
        ));
    }
    if !(pass > 0.0 && pass < stop && stop < 0.5) {
        return fail(format!(
            "the band edges must satisfy 0 < passband ({pass}) < stopband ({stop}) < 0.5 cycles \
             per sample"
        ));
    }
    let m = (n - 1) / 2;
    let c = match method {
        Method::Equiripple => {
            if n > EQUIRIPPLE_MAX_TAPS {
                return Err(DesignError::Unsupported(format!(
                    "an equiripple design is built up to {EQUIRIPPLE_MAX_TAPS} taps (got {n}): use \
                     least squares or a windowed sinc for longer kernels"
                )));
            }
            // A length the exchange does not level is retried 2 and 4
            // taps shorter, its ends zero-padded back to `n`.
            match remez(m, pass, stop) {
                Ok((c, _)) => c,
                Err(e) if !retry => return Err(e),
                Err(e) => {
                    let shorter = (1..=2)
                        .filter_map(|k| m.checked_sub(k).filter(|&k| k > 0))
                        .find_map(|k| remez(k, pass, stop).ok());
                    let Some((mut c, _)) = shorter else {
                        return Err(e);
                    };
                    c.resize(m + 1, 0.0);
                    c
                }
            }
        }
        Method::LeastSquares => {
            if n > LEAST_SQUARES_MAX_TAPS {
                return Err(DesignError::Unsupported(format!(
                    "a least-squares design is built up to {LEAST_SQUARES_MAX_TAPS} taps (got {n}): \
                     use a windowed sinc for longer kernels"
                )));
            }
            least_squares(m, pass, stop)?
        }
    };
    Ok(taps_of(&c))
}

/// The symmetric taps of `A(w) = c_0 + sum_k c_k cos(k w)`.
fn taps_of(c: &[f64]) -> Vec<f64> {
    let m = c.len() - 1;
    let mut h = vec![0.0; 2 * m + 1];
    h[m] = c[0];
    for k in 1..=m {
        h[m - k] = 0.5 * c[k];
        h[m + k] = 0.5 * c[k];
    }
    h
}

// ------------------------------------------------------------ equiripple

/// A barycentric weight: mantissa times `2^exp`.
#[derive(Clone, Copy)]
struct Scaled {
    mant: f64,
    exp: i32,
}

/// `1 / prod_{j != k} (x_k - x_j)` for every node, as mantissa and binary
/// exponent (the products of up to thousands of distances overflow f64).
fn weights(x: &[f64]) -> Vec<Scaled> {
    const BIG: f64 = 1.0e150;
    const BIG_EXP: i32 = 498; // BIG ~ 2^498.29; exact powers below.
    let up = 2f64.powi(BIG_EXP);
    let down = 2f64.powi(-BIG_EXP);
    x.iter()
        .enumerate()
        .map(|(k, &xk)| {
            let mut p = 1.0f64;
            let mut e = 0i32;
            for (j, &xj) in x.iter().enumerate() {
                if j == k {
                    continue;
                }
                p *= xk - xj;
                let a = p.abs();
                if a > BIG {
                    p *= down;
                    e += BIG_EXP;
                } else if a < 1.0 / BIG {
                    p *= up;
                    e -= BIG_EXP;
                }
            }
            Scaled {
                mant: 1.0 / p,
                exp: -e,
            }
        })
        .collect()
}

/// The weights as plain f64 over a common scale (only ratios matter).
fn common(w: &[Scaled]) -> Vec<f64> {
    let top = w.iter().map(|s| s.exp).max().unwrap_or(0);
    w.iter()
        .map(|s| s.mant * 2f64.powi((s.exp - top).max(-1074)))
        .collect()
}

/// The degree-`M` interpolant through `(x_k, y_k)` (barycentric weights
/// `w`) at `t`.
fn bary(x: &[f64], y: &[f64], w: &[f64], t: f64) -> f64 {
    let (mut num, mut den) = (0.0, 0.0);
    for k in 0..x.len() {
        let d = t - x[k];
        if d == 0.0 {
            return y[k];
        }
        let q = w[k] / d;
        num += q * y[k];
        den += q;
    }
    num / den
}

/// The dense frequency grid of both bands (cycles per sample), with the
/// desired value: unit up to `pass`, zero from `stop`.
fn grid(m: usize, pass: f64, stop: f64, density: usize, pw: f64) -> (Vec<f64>, Vec<f64>, Vec<f64>) {
    let step = 0.5 / (density * (m + 1)) as f64;
    let mut f = Vec::new();
    let mut d = Vec::new();
    let mut w = Vec::new();
    let mut band = |lo: f64, hi: f64, want: f64, weight: f64| {
        let count = (((hi - lo) / step).round() as usize).max(1);
        for i in 0..=count {
            f.push(lo + (hi - lo) * i as f64 / count as f64);
            d.push(want);
            w.push(weight);
        }
    };
    band(0.0, pass, 1.0, pw);
    band(stop, 0.5, 0.0, 1.0);
    (f, d, w)
}

/// The equiripple passband's weight against the stopband's (module docs).
const PASSBAND_WEIGHT: f64 = 2.0;

/// The Remez exchange for `M + 1` cosine coefficients: the coefficients and
/// the minimax error. Refused when it does not converge.
pub(crate) fn remez(m: usize, pass: f64, stop: f64) -> Result<(Vec<f64>, f64), DesignError> {
    remez_weighted(m, pass, stop, PASSBAND_WEIGHT)
}

/// [`remez`] with the passband weighted `pw` against the stopband.
fn remez_weighted(m: usize, pass: f64, stop: f64, pw: f64) -> Result<(Vec<f64>, f64), DesignError> {
    // From the least-squares design's peaks first, then from an even
    // spread, then both on a denser grid: each converges on some lengths
    // the others do not.
    exchange(m, pass, stop, pw, true, GRID_DENSITY)
        .or_else(|_| exchange(m, pass, stop, pw, false, GRID_DENSITY))
        .or_else(|_| exchange(m, pass, stop, pw, true, DENSE_RETRY))
        .or_else(|e| exchange(m, pass, stop, pw, false, DENSE_RETRY).map_err(|_| e))
}

/// One Remez exchange run (`seeded`: from the least-squares design's
/// error peaks, else from an even spread).
fn exchange(
    m: usize,
    pass: f64,
    stop: f64,
    pw: f64,
    seeded: bool,
    density: usize,
) -> Result<(Vec<f64>, f64), DesignError> {
    let (f, d, wt) = grid(m, pass, stop, density, pw);
    let g = f.len();
    let r = m + 2;
    if g < 2 * r {
        return fail("the band edges leave too few grid points for the exchange");
    }
    let xg: Vec<f64> = f.iter().map(|&f| (2.0 * PI * f).cos()).collect();
    let mut err = vec![0.0; g];
    // Initial extremals: the least-squares design's alternating error peaks
    // (close to the equiripple's; an even spread over the grid puts too few
    // near a narrow transition and the exchange diverges at long lengths),
    // or the even spread when they do not alternate enough.
    let seed = if seeded {
        least_squares(m, pass, stop).ok()
    } else {
        None
    };
    let mut ext = seed
        .and_then(|c| {
            chebyshev_errors(&xg, &d, &wt, &c, &mut err);
            extremals_by(&err, &f, stop, r, Prune::Spread)
        })
        .unwrap_or_else(|| (0..r).map(|k| k * (g - 1) / (r - 1)).collect());
    let mut converged = false;
    for _ in 0..MAX_EXCHANGES {
        let s = Interpolant::new(&ext, &xg, &d, &wt);
        let peak = interp_errors(&xg, &d, &wt, &s, &mut err);
        let Some(next) = extremals(&err, &f, stop, r) else {
            return fail(format!(
                "the equiripple exchange lost its alternation at {} taps: f64 does not level \
                 its ripple at this length and band (from about 165 dB deep); shorten it, widen \
                 the transition or use least squares",
                2 * m + 1
            ));
        };
        let spread = (peak - s.delta.abs()) / peak.max(f64::MIN_POSITIVE);
        let same = next == ext;
        ext = next;
        if !s.delta.is_finite() {
            break;
        }
        if same || spread < 1e-9 {
            converged = true;
            break;
        }
    }
    let s = Interpolant::new(&ext, &xg, &d, &wt);
    if !converged || !s.delta.is_finite() {
        return fail(format!(
            "the equiripple exchange did not converge at {} taps: shorten it, widen the \
             transition or use least squares",
            2 * m + 1
        ));
    }
    // A(f) at f = k / N (k = 0 ..= M) by the final interpolant, then the
    // cosine coefficients by the inverse DFT of a symmetric sequence.
    let n = 2 * m + 1;
    let a: Vec<f64> = (0..=m)
        .map(|k| s.at((2.0 * PI * k as f64 / n as f64).cos()))
        .collect();
    Ok((cosine_coefficients(&a, n), s.delta.abs()))
}

/// The exchange's interpolant for one extremal set: the levelled weighted
/// error `delta` and the degree-M polynomial through the first `r - 1`
/// extremals at `D - (-1)^k delta / W`.
struct Interpolant {
    delta: f64,
    x: Vec<f64>,
    y: Vec<f64>,
    w: Vec<f64>,
}

impl Interpolant {
    fn new(ext: &[usize], xg: &[f64], d: &[f64], wt: &[f64]) -> Self {
        let r = ext.len();
        let x: Vec<f64> = ext.iter().map(|&i| xg[i]).collect();
        let w = common(&weights(&x));
        let (mut num, mut den) = (0.0, 0.0);
        for k in 0..r {
            num += w[k] * d[ext[k]];
            let q = w[k] / wt[ext[k]];
            den += if k % 2 == 0 { q } else { -q };
        }
        let delta = num / den;
        let wi = (0..r - 1).map(|k| w[k] * (x[k] - x[r - 1])).collect();
        let y = (0..r - 1)
            .map(|k| {
                let e = delta / wt[ext[k]];
                d[ext[k]] - if k % 2 == 0 { e } else { -e }
            })
            .collect();
        let mut x = x;
        x.truncate(r - 1);
        Self { delta, x, y, w: wi }
    }

    fn at(&self, t: f64) -> f64 {
        bary(&self.x, &self.y, &self.w, t)
    }
}

/// The weighted error `W (D - A)` on the grid (in `err`) of `A(x) = sum
/// c_k T_k(x)` (Clenshaw).
fn chebyshev_errors(xg: &[f64], d: &[f64], wt: &[f64], c: &[f64], err: &mut [f64]) {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(16);
    let chunk = err.len().div_ceil(threads).max(1024);
    std::thread::scope(|sc| {
        for (ci, part) in err.chunks_mut(chunk).enumerate() {
            sc.spawn(move || {
                for (j, e) in part.iter_mut().enumerate() {
                    let i = ci * chunk + j;
                    let x = xg[i];
                    let (mut b1, mut b2) = (0.0, 0.0);
                    for &ck in c[1..].iter().rev() {
                        let b0 = 2.0 * x * b1 - b2 + ck;
                        b2 = b1;
                        b1 = b0;
                    }
                    *e = wt[i] * (d[i] - (x * b1 - b2 + c[0]));
                }
            });
        }
    });
}

/// The weighted error `W (D - A)` on the grid (in `err`) and its largest
/// magnitude.
fn interp_errors(xg: &[f64], d: &[f64], wt: &[f64], s: &Interpolant, err: &mut [f64]) -> f64 {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(16);
    let chunk = err.len().div_ceil(threads).max(1024);
    std::thread::scope(|sc| {
        for (c, part) in err.chunks_mut(chunk).enumerate() {
            sc.spawn(move || {
                for (j, e) in part.iter_mut().enumerate() {
                    let i = c * chunk + j;
                    *e = wt[i] * (d[i] - s.at(xg[i]));
                }
            });
        }
    });
    err.iter().fold(0.0f64, |m, e| m.max(e.abs()))
}

/// How surplus extremals are dropped.
#[derive(Clone, Copy, PartialEq)]
enum Prune {
    /// The smallest peaks first: the exchange's own rule, once the error is
    /// near level (the surplus peaks are the small ones).
    Smallest,
    /// Pairs spread evenly along the band: a least-squares seed, whose far
    /// stopband peaks are all small but still needed for coverage.
    Spread,
}

/// The next `r` alternating extremals of `err` (grid indices), or `None`
/// when fewer than `r` alternate.
fn extremals(err: &[f64], f: &[f64], stop: f64, r: usize) -> Option<Vec<usize>> {
    extremals_by(err, f, stop, r, Prune::Smallest)
}

/// [`extremals`] with the surplus dropped by `prune`.
fn extremals_by(err: &[f64], f: &[f64], stop: f64, r: usize, prune: Prune) -> Option<Vec<usize>> {
    let g = err.len();
    // Band of each point: the grid holds the passband, then the stopband.
    let split = f.iter().position(|&v| v >= stop - 1e-15).unwrap_or(g);
    let mut cand: Vec<usize> = Vec::new();
    for (lo, hi) in [(0usize, split), (split, g)] {
        for i in lo..hi {
            // A signed extremum: at least its neighbours in its band, on
            // its own side of zero.
            let e = err[i];
            let s = if e >= 0.0 { 1.0 } else { -1.0 };
            let left = i > lo && s * err[i - 1] > s * e;
            let right = i + 1 < hi && s * err[i + 1] >= s * e;
            if !left && !right {
                cand.push(i);
            }
        }
    }
    cand.dedup();
    // Alternation: of neighbours with one sign keep the larger.
    let mut alt: Vec<usize> = Vec::with_capacity(cand.len());
    for &i in &cand {
        match alt.last() {
            Some(&j) if (err[j] >= 0.0) == (err[i] >= 0.0) => {
                if err[i].abs() > err[j].abs() {
                    *alt.last_mut().unwrap() = i;
                }
            }
            _ => alt.push(i),
        }
    }
    if alt.len() < r {
        return None;
    }
    // Too many: drop the smallest peak with its smaller neighbour (a pair
    // keeps the alternation; an end peak goes alone), until r remain. Only
    // ends would leave spurious small peaks in the middle, and the
    // exchange degenerates on them at long lengths.
    if prune == Prune::Spread && alt.len() > r {
        let mut excess = alt.len() - r;
        if excess % 2 == 1 {
            let n = alt.len();
            alt.remove(if err[alt[0]].abs() < err[alt[n - 1]].abs() {
                0
            } else {
                n - 1
            });
            excess -= 1;
        }
        // excess / 2 neighbouring pairs, evenly spaced, from the back so
        // the indices stay valid.
        let pairs = excess / 2;
        let n = alt.len();
        for p in (0..pairs).rev() {
            let at = ((2 * p + 1) * (n - 1)) / (2 * pairs);
            let at = at.min(alt.len() - 2);
            alt.remove(at + 1);
            alt.remove(at);
        }
    }
    while alt.len() > r {
        let n = alt.len();
        let mag = |i: usize| err[alt[i]].abs();
        if n - r == 1 {
            alt.remove(if mag(0) < mag(n - 1) { 0 } else { n - 1 });
            continue;
        }
        let j = (0..n)
            .min_by(|&a, &b| mag(a).total_cmp(&mag(b)))
            .expect("not empty");
        if j == 0 || j + 1 == n {
            alt.remove(j);
            continue;
        }
        let k = if mag(j - 1) < mag(j + 1) {
            j - 1
        } else {
            j + 1
        };
        alt.remove(j.max(k));
        alt.remove(j.min(k));
    }
    Some(alt)
}

/// `c` with `A(w) = c_0 + sum c_k cos(k w)` from `A` at `w = 2 pi k / n`,
/// `k = 0 ..= M` (`n = 2M + 1`).
fn cosine_coefficients(a: &[f64], n: usize) -> Vec<f64> {
    let m = a.len() - 1;
    // h[k] for k = 0 ..= M from the symmetric inverse DFT, by FFT.
    let mut planner = RealFftPlanner::<f64>::new();
    let inv = planner.plan_fft_inverse(n);
    let mut spec = inv.make_input_vec();
    for (k, s) in spec.iter_mut().enumerate() {
        *s = Complex64::new(a[k.min(m)], 0.0);
    }
    let mut h = inv.make_output_vec();
    inv.process(&mut spec, &mut h).expect("FFT lengths match");
    // h[t] = sum_k A_k e^{2 pi i k t / n} / n: the centre tap at t = 0,
    // the others at t and n - t.
    let mut c = vec![0.0; m + 1];
    c[0] = h[0] / n as f64;
    for k in 1..=m {
        c[k] = 2.0 * h[k] / n as f64;
    }
    c
}

// --------------------------------------------------------- least squares

/// The least-squares cosine coefficients (`M + 1`): conjugate gradients on
/// the normal equations, Jacobi-preconditioned, products by FFT.
pub(crate) fn least_squares(m: usize, pass: f64, stop: f64) -> Result<Vec<f64>, DesignError> {
    let (wp, ws) = (2.0 * PI * pass, 2.0 * PI * stop);
    // s(j) = integral over both bands of cos(j w) dw, j = 0 ..= 2M.
    let s = |j: usize| {
        if j == 0 {
            wp + PI - ws
        } else {
            let jf = j as f64;
            ((jf * wp).sin() - (jf * ws).sin()) / jf
        }
    };
    let sv: Vec<f64> = (0..=2 * m).map(s).collect();
    let rhs: Vec<f64> = (0..=m)
        .map(|j| {
            if j == 0 {
                wp
            } else {
                (j as f64 * wp).sin() / j as f64
            }
        })
        .collect();
    let q = NormalMatrix::new(&sv, m);
    let diag: Vec<f64> = (0..=m).map(|j| 0.5 * (sv[0] + sv[2 * j])).collect();
    // Preconditioned CG.
    let mut c = vec![0.0; m + 1];
    let mut res = rhs.clone();
    let mut z: Vec<f64> = res.iter().zip(&diag).map(|(r, d)| r / d).collect();
    let mut p = z.clone();
    let mut rz: f64 = res.iter().zip(&z).map(|(a, b)| a * b).sum();
    let r0 = rz;
    let mut qp = vec![0.0; m + 1];
    for _ in 0..MAX_CG {
        q.apply(&p, &mut qp);
        let pq: f64 = p.iter().zip(&qp).map(|(a, b)| a * b).sum();
        if pq.is_nan() || pq <= 0.0 {
            break;
        }
        let alpha = rz / pq;
        for i in 0..=m {
            c[i] += alpha * p[i];
            res[i] -= alpha * qp[i];
        }
        for i in 0..=m {
            z[i] = res[i] / diag[i];
        }
        let rz_next: f64 = res.iter().zip(&z).map(|(a, b)| a * b).sum();
        if rz_next <= 1e-30 * r0 {
            return Ok(c);
        }
        let beta = rz_next / rz;
        rz = rz_next;
        for i in 0..=m {
            p[i] = z[i] + beta * p[i];
        }
    }
    if c.iter().all(|v| v.is_finite()) {
        // Stalled at the f64 floor: the error that counts has converged.
        Ok(c)
    } else {
        fail("the least-squares solve did not converge")
    }
}

/// `Q_{jk} = (s(j - k) + s(j + k)) / 2`, applied by FFT.
struct NormalMatrix {
    m: usize,
    len: usize,
    toeplitz: Vec<Complex64>,
    hankel: Vec<Complex64>,
    fwd: std::sync::Arc<dyn realfft::RealToComplex<f64>>,
    inv: std::sync::Arc<dyn realfft::ComplexToReal<f64>>,
}

impl NormalMatrix {
    fn new(s: &[f64], m: usize) -> Self {
        let len = (4 * m + 2).next_power_of_two();
        let mut planner = RealFftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(len);
        let inv = planner.plan_fft_inverse(len);
        // Lags -M ..= M at 0 ..= 2M, and s(0 ..= 2M).
        let mut t = fwd.make_input_vec();
        for i in 0..=2 * m {
            t[i] = s[i.abs_diff(m)];
        }
        let mut toeplitz = fwd.make_output_vec();
        fwd.process(&mut t, &mut toeplitz)
            .expect("FFT lengths match");
        let mut h = fwd.make_input_vec();
        h[..=2 * m].copy_from_slice(&s[..=2 * m]);
        let mut hankel = fwd.make_output_vec();
        fwd.process(&mut h, &mut hankel).expect("FFT lengths match");
        Self {
            m,
            len,
            toeplitz,
            hankel,
            fwd,
            inv,
        }
    }

    fn apply(&self, c: &[f64], out: &mut [f64]) {
        let m = self.m;
        let mut a = self.fwd.make_input_vec();
        a[..=m].copy_from_slice(c);
        let mut fa = self.fwd.make_output_vec();
        self.fwd
            .process(&mut a, &mut fa)
            .expect("FFT lengths match");
        let mut b = self.fwd.make_input_vec();
        for k in 0..=m {
            b[k] = c[m - k];
        }
        let mut fb = self.fwd.make_output_vec();
        self.fwd
            .process(&mut b, &mut fb)
            .expect("FFT lengths match");
        for i in 0..fa.len() {
            fa[i] = fa[i] * self.toeplitz[i] + fb[i] * self.hankel[i];
        }
        let mut y = self.inv.make_output_vec();
        self.inv
            .process(&mut fa, &mut y)
            .expect("FFT lengths match");
        let scale = 0.5 / self.len as f64;
        for j in 0..=m {
            out[j] = y[j + m] * scale;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest error of `h` (unit DC gain) over both bands, by direct
    /// evaluation on a fine grid.
    fn max_error(h: &[f64], pass: f64, stop: f64) -> f64 {
        let n = h.len();
        let c = (n - 1) / 2;
        let points = 40 * n;
        let mut worst = 0.0f64;
        for i in 0..=points {
            let f = 0.5 * i as f64 / points as f64;
            if f > pass && f < stop {
                continue;
            }
            let mut a = h[c];
            for k in 1..=c {
                a += 2.0 * h[c + k] * (2.0 * PI * f * k as f64).cos();
            }
            let want = if f <= pass { 1.0 } else { 0.0 };
            worst = worst.max((a - want).abs());
        }
        worst
    }

    /// A design is cached: asked again it is the same, bit for bit, and so
    /// is a refusal.
    #[test]
    fn designs_are_cached_bit_for_bit() {
        for method in [Method::Equiripple, Method::LeastSquares] {
            let first = design(method, 301, 0.2, 0.23, true).unwrap();
            let again = design(method, 301, 0.2, 0.23, true).unwrap();
            assert_eq!(first, again);
            assert_eq!(
                first,
                design_uncached(method, 301, 0.2, 0.23, true).unwrap()
            );
        }
        let refused = design(Method::Equiripple, 300, 0.2, 0.23, true).unwrap_err();
        assert_eq!(
            design(Method::Equiripple, 300, 0.2, 0.23, true).unwrap_err(),
            refused
        );
    }

    #[test]
    fn equiripple_matches_scipy_remez() {
        // scipy.signal.remez(31, [0, 0.2, 0.3, 0.5], [1, 0]): its minimax
        // error is the design's ripple.
        let h = taps_of(&remez_weighted(15, 0.2, 0.3, 1.0).unwrap().0);
        let want = [
            -0.002104277637238513,
            1.9344144067320787e-06,
            0.0046465781985608,
            -7.372188033157258e-07,
            -0.009421938587867648,
        ];
        // Both are minimax on a 16-point grid: they agree to the grid.
        for (g, w) in h.iter().zip(want) {
            assert!((g - w).abs() < 1e-5, "{g} vs {w}");
        }
        // scipy's largest error, measured on 20000 points: 0.0013629907.
        let (_, delta) = remez_weighted(15, 0.2, 0.3, 1.0).unwrap();
        let e = max_error(&h, 0.2, 0.3);
        assert!(e < 0.0013629907 * 1.001, "{e}");
        assert!((e - delta).abs() < 0.01 * delta, "{e} vs {delta}");
    }

    #[test]
    fn least_squares_matches_the_dense_solve() {
        let (m, pass, stop) = (40usize, 0.1, 0.15);
        let c = least_squares(m, pass, stop).unwrap();
        // Dense normal equations by Gaussian elimination.
        let (wp, ws) = (2.0 * PI * pass, 2.0 * PI * stop);
        let s = |j: usize| {
            if j == 0 {
                wp + PI - ws
            } else {
                let jf = j as f64;
                ((jf * wp).sin() - (jf * ws).sin()) / jf
            }
        };
        let mut a: Vec<Vec<f64>> = (0..=m)
            .map(|j| {
                let mut row: Vec<f64> = (0..=m)
                    .map(|k| 0.5 * (s(j.abs_diff(k)) + s(j + k)))
                    .collect();
                row.push(if j == 0 {
                    wp
                } else {
                    (j as f64 * wp).sin() / j as f64
                });
                row
            })
            .collect();
        for i in 0..=m {
            let p = (i..=m)
                .max_by(|&x, &y| a[x][i].abs().total_cmp(&a[y][i].abs()))
                .unwrap();
            a.swap(i, p);
            for r in i + 1..=m {
                let f = a[r][i] / a[i][i];
                for k in i..=m + 1 {
                    a[r][k] -= f * a[i][k];
                }
            }
        }
        let mut x = vec![0.0; m + 1];
        for i in (0..=m).rev() {
            let s: f64 = (i + 1..=m).map(|k| a[i][k] * x[k]).sum();
            x[i] = (a[i][m + 1] - s) / a[i][i];
        }
        for (g, w) in c.iter().zip(&x) {
            assert!((g - w).abs() < 1e-9, "{g} vs {w}");
        }
    }

    /// Prints the equiripple and least-squares error and design time over
    /// lengths (the limits above).
    #[test]
    #[ignore]
    fn measure_limits() {
        for &(n, l) in &[
            (255usize, 8usize),
            (1535, 8),
            (1791, 8),
            (12287, 64),
            (16383, 64),
            (65535, 64),
            (262143, 64),
            (1048575, 64),
        ] {
            let pass = 0.9 / (2.0 * l as f64);
            let stop = 1.0 / (2.0 * l as f64);
            for method in [Method::Equiripple, Method::LeastSquares] {
                if method == Method::Equiripple && n > 16383
                    || method == Method::LeastSquares && n > 1 << 20
                {
                    continue;
                }
                let t = std::time::Instant::now();
                let m = (n - 1) / 2;
                let r = match method {
                    Method::Equiripple => remez(m, pass, stop).map(|x| x.0),
                    Method::LeastSquares => least_squares(m, pass, stop),
                };
                let dt = t.elapsed().as_secs_f64();
                match r {
                    Ok(c) => {
                        let h = taps_of(&c);
                        let e = crate::design::auto::protected_error_with(
                            &mut RealFftPlanner::new(),
                            &h,
                            h.iter().sum(),
                            pass,
                            stop,
                        );
                        println!(
                            "{method:?} n {n} x{l}: {:.1} dB in {dt:.2} s",
                            -20.0 * e.log10()
                        );
                    }
                    Err(e) => println!("{method:?} n {n} x{l}: {e} ({dt:.2} s)"),
                }
            }
        }
    }
}
