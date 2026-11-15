//! Minimum-phase twin of a linear-phase FIR (offline, off the real-time path).
//!
//! # Contract
//!
//! [`minimum_phase`] returns an FIR of the **same length** `N` whose magnitude
//! response equals the input's and whose zeros all lie inside or on the unit
//! circle: every zero outside is reflected to `1 / conj(z)`, the zeros *on*
//! the circle (the stopband nulls of a linear-phase low-pass) stay there. It
//! never returns a design that failed its checks: it returns a
//! [`MinimumPhaseError`] instead, and the caller rejects the design.
//!
//! Every returned kernel has been checked, on an FFT grid of at least `8 N`
//! points, against the input (`A = beta / 0.1102 + 8.7` dB, the Kaiser
//! stopband specification, and `edge` from [`Contract`]):
//!
//! * `| |H_min| - |H_lin| | <= 1e-4 dB` wherever `|H_lin| >= -1 dB`
//!   ([`PASSBAND_TOLERANCE_DB`]) and `<= 0.01 dB` wherever `|H_lin| >= -100
//!   dB` ([`UPPER_TOLERANCE_DB`]);
//! * from `edge` up, `| |H_min| - |H_lin| |` is at least 25 dB under `A`
//!   ([`STOPBAND_ERROR_MARGIN_DB`]). An error that size moves any level at or
//!   above `-(A + 5)` dB by at most 0.83 dB;
//! * minimum phase. Up to [`ROOT_MAX_TAPS`] taps the zeros of the returned
//!   f64 kernel are computed, and every zero outside the unit circle must be
//!   a pseudozero of it to within `delta = 64 u N` (`u = 2^-53`,
//!   [`zero_tolerance`]): along the radial segment from it to the circle,
//!   `|B(zeta)| <= delta sum |b_k| |zeta|^-k`, i.e. changing every coefficient
//!   by at most `delta` relatively (a few ulps) moves it onto the circle
//!   (Mosier; Trefethen and Toh). That is as tight as an f64 kernel allows:
//!   where `|H|` is at the rounding floor of its own coefficients, its zeros
//!   are not determined more closely. In the sweep tests (every length 2 to
//!   1024, 47,000 designs) no zero lies outside the circle where `|B|` there
//!   is above -250 dB re the DC gain, the largest outside radius (at the
//!   noise floor) is `1 + 2.5e-3` and the largest perturbation needed is 0.04
//!   `delta`. Longer kernels are checked by the cepstral unwrapping below and,
//!   in the tests, by winding counts on `|z| = e^{0.25/N}`.
//!
//! # Method, up to [`ROOT_MAX_TAPS`] taps: polynomial roots
//!
//! All `N - 1` zeros are found with the Aberth-Ehrlich iteration (Newton
//! polygon start, total-step sweeps, backward-stable stopping) and then
//! polished by Newton-Aberth steps with the polynomial evaluated in
//! double-double, so each ends within a few ulps of an exact zero of the f64
//! polynomial even where it is ill-conditioned (a reciprocal pair close to the
//! circle: 36 taps, x2, beta 20 has one at `1 +- 1.09e-3` with condition
//! number 3e8). The ones outside the circle by more than that uncertainty
//! are reflected exactly in the frequency domain, `B(e^jw) = H(e^jw) *
//! prod (conj(z) - e^-jw) / (1 - z e^-jw)` (each factor is all-pass), on a
//! `16 N`-point grid, and the inverse FFT gives the kernel. The product
//! cancels the reflected zeros' factors, so the result has `N` taps; what
//! lands past `N` measures the root error ([`MinimumPhase::tail`], at most
//! 1e-9, measured 3e-14). Then the zeros of the result are computed again and
//! checked as above. Exact leading zero taps (a pure delay, zeros at
//! infinity) become trailing ones. Design time: 0.1 s at 512 taps, 0.3 to
//! 0.6 s at 1024 (threaded sweeps).
//!
//! The contour method below needs every zero outside the circle to lie
//! outside its contour. Short kernels break that: a 15-tap x16 beta-14
//! kernel has a conjugate pair at `|z| = 1.035`, inside the contour
//! (`docs/research/scientific-review-2/`), and the uncapped contour `2/N`
//! still misses outside zeros up to 186 taps (once at 416) in a 325,000-design
//! scan. Roots have no such assumption; the switch at 1024 taps leaves a
//! margin over the largest length where the contour failed.
//!
//! # Method, longer kernels: radially scaled complex cepstrum
//!
//! Rooting a polynomial of degree 4M is not an option, so the reflection is
//! done in the cepstral domain, where it is linear: the complex cepstrum
//! `ĥ[n]` of `H` (pure delay removed) is causal for zeros inside the circle
//! and anticausal for zeros outside, and the reflected filter has
//! `ĉ[n] = ĥ[n] + ĥ[-n]` for `n > 0`, `ĉ[0] = ĥ[0]`.
//!
//! The usual homomorphic recipe evaluates `ln|H|` on the unit circle. A
//! linear-phase low-pass has up to millions of zeros *on* that circle, so
//! `ln|H|` is singular, its cepstrum decays only like `1/n` (with a comb at
//! multiples of the null spacing), and any finite FFT aliases it.
//!
//! Here the cepstrum is evaluated on the circle `|z| = rho = e^{alpha}`,
//! `alpha = R / N`, just outside the unit circle:
//! `g[n] = h[n] e^{-alpha n}` has `G(e^{jw}) = H(rho e^{jw})`. The radius is
//! adaptive: [`contour_radii`] lists `R` = 2, 1.5, 1 (1.5, 2, 1, 1.25 from
//! [`LONG_CONTOUR_TAPS`] up), and the first one whose result passes the
//! unwrap and magnitude checks is returned. Below, `R = 2`.
//!
//! * Zero geometry. The on-circle zeros of `H` are now `2/N` inside the
//!   contour. Above [`ROOT_MAX_TAPS`] taps its off-circle zeros come in
//!   reciprocal pairs at least 6/N from the unit circle for every beta >= 2,
//!   any cutoff and ratio (>= 16/N at beta 8, 21/N at 14, 25/N at 20),
//!   measured by rooting and by winding counts. The exception is real pairs
//!   next to `z = -1` on odd lengths, reflected exactly first
//!   (`reflect_nyquist_real_zeros`). So `G` has no zero within 2/N of its
//!   unit circle and the outside zeros of `H` are exactly those outside the
//!   contour.
//! * The phase of `G` is unwrapped bin by bin. Every zero is at least `2/N`
//!   from the contour and the FFT has at least `8 N` points, so adjacent bins
//!   differ by well under `pi/2` (checked; a failure is an error). The one
//!   exception is rounding noise: deep in the stopband of long high-beta
//!   kernels the f64 coefficients' own rounding dominates and scatters zeros
//!   near the circle; see [`NOISE_LEVEL`]. `arg G(pi) = -pi * (zeros outside
//!   the contour)` gives the pure delay to remove.
//! * `ln G` is analytic in an annulus of width ~`2/N` on each side, so its
//!   cepstrum decays like `e^{-2|n|/N}`. With `L >= 8 N` the aliased part is
//!   at most `e^{-8}` of the stopband comb and lands on the stopband nulls; the
//!   passband sees ~1e-8 dB.
//! * The scaling is undone exactly: `ĥ[n] = ĝ[n] rho^n`, so the reflected
//!   cepstrum in the scaled domain is `ĝ[n] + ĝ[-n] rho^{-2n}`, and the
//!   result is `b[n] = b_g[n] rho^n`. The amplification `rho^N = e^R` costs
//!   about 17 dB of rounding headroom at `R = 2`. At 4M taps and low ratios
//!   that lifts the stopband error over its bound (x2, beta 20: -206.3 dB,
//!   bound -215.2), hence `R = 1.5` first on long kernels (-215.9 dB), at
//!   the price of a larger aliased part (passband still ~1e-6 dB).
//! * At 4M taps and beta 20 the stopband error is a rounding floor of about
//!   -206 to -229 dB against the -215.2 dB bound, peaking on single bins at
//!   the first stopband nulls, and which radius lands under the bound is
//!   effectively random (x3, Stop at Nyquist: -212.7 dB at 2/N, -210.9 at
//!   1.5/N, -229.2 at 1.75/N, -218.8 at 1.25/N, passband 1.01e-4 dB at 1/N).
//!   The fourth long radius, 1.25, catches the one design of the long-length
//!   matrix that the first three miss.
//! * The exact twin has exactly `N` taps, so truncating to `N` drops only
//!   aliasing and rounding residue; [`MinimumPhase::tail`] reports its size.
//!
//! FFT length `L = next_power_of_two(8 N)`. At the 4,194,304-tap limit that is
//! a 33,554,432-point real FFT. Measured on the dev box, single-threaded,
//! with the magnitude check: about 6 s and 1.47 GiB peak RSS for the whole
//! process, including the linear kernel
//! (`minimum_phase_4m_contract_time_and_memory`). Each further radius adds
//! about 5 to 6 s. In the long-length matrix of
//! `docs/research/minimum-phase/README.md` (1,782 designs, 65,535 to
//! 4,194,304 taps) only 4M-tap beta-20 designs needed one: five needed a
//! second radius (up to 11.5 s), two a third (15.2 s), one the fourth
//! (28 s with six designs in parallel).
//!
//! # Limits
//!
//! Above [`MAX_MINIMUM_PHASE_BETA`] the stopband (> 190 dB) approaches the
//! f64 floor of long kernels and the tolerances fail at the largest lengths
//! (4M taps, beta 22: minimum-phase stopband peak 3.5 dB above the linear
//! one; beta 24: the unwrap fails). Below [`MIN_MINIMUM_PHASE_BETA`] the zero
//! geometry above does not hold. The daemon rejects both.

use realfft::RealFftPlanner;
use rustfft::num_complex::Complex64;
use std::f64::consts::PI;

/// Contour radius in units of 1/N: the cepstrum is evaluated on
/// `|z| = e^{RADIUS_TAPS / N}`.
pub const RADIUS_TAPS: f64 = 2.0;

/// FFT length per kernel tap (rounded up to a power of two).
pub const FFT_LEN_PER_TAP: usize = 8;

/// Longest kernel designed (and checked) by polynomial rooting; longer ones
/// use the scaled cepstrum.
pub const ROOT_MAX_TAPS: usize = 1024;

/// FFT length per tap of the root path's reflection and checks.
const ROOT_FFT_LEN_PER_TAP: usize = 16;

/// Smallest Kaiser beta whose minimum-phase design is supported. Below it an
/// even-length kernel has a real zero pair around `z = -1` within about 2/N
/// of the unit circle, which the contour at [`RADIUS_TAPS`] cannot separate.
pub const MIN_MINIMUM_PHASE_BETA: f64 = 2.0;

/// Largest Kaiser beta whose minimum-phase design meets the contract at every
/// supported length (A = 190.2 dB; see the module docs, "Limits").
pub const MAX_MINIMUM_PHASE_BETA: f64 = 20.0;

/// Largest `| |H_min| - |H_lin| |` in dB where `|H_lin| >= -1 dB`.
pub const PASSBAND_TOLERANCE_DB: f64 = 1.0e-4;

/// Largest `| |H_min| - |H_lin| |` in dB where `|H_lin| >=`
/// [`UPPER_LEVEL_DB`].
pub const UPPER_TOLERANCE_DB: f64 = 0.01;

/// Level (dB re the DC gain) down to which [`UPPER_TOLERANCE_DB`] applies.
pub const UPPER_LEVEL_DB: f64 = -100.0;

/// From the stopband edge up, the absolute magnitude error stays this far
/// under the Kaiser stopband specification `A`.
pub const STOPBAND_ERROR_MARGIN_DB: f64 = 25.0;

/// Zero tolerance, as a relative coefficient perturbation in units of
/// `u N` (`u = 2^-53`); see [`zero_tolerance`].
pub const ZERO_TOLERANCE_ULPS: f64 = 64.0;

/// Largest phase change accepted between adjacent FFT bins while unwrapping.
const MAX_PHASE_STEP: f64 = PI / 2.0;

/// Largest accepted distance of the unwrapped Nyquist phase from a multiple of
/// pi (radians).
const MAX_RESIDUE: f64 = 0.25;

/// The same when the Nyquist bin itself is under [`NOISE_FLOOR`] and its
/// phase is extrapolated over the trailing floored run. That happens on long
/// kernels: even lengths have a zero at `z = -1` just inside the contour, so
/// the phase still turns by ~0.4 rad over the last bin (4,000,000 taps, x2,
/// beta 14: residue 0.317), and a long floored run near Nyquist (1,048,576
/// taps, x16, beta 14: 1.24 rad) carries no phase information at all. Any
/// residue then only picks the nearest multiple of pi; the residue is spread
/// over the floored bins, and the magnitude check covers the result.
const MAX_RESIDUE_FLOORED: f64 = PI / 2.0;

/// From this length up the contour radius 1.5/N is tried first: radius 2/N
/// amplifies the f64 rounding of the cepstral solution by `e^2`, which at
/// 4M taps and low ratios lifts the stopband error over its bound (4,194,304
/// taps, x2, beta 20: -206.3 dB at 2/N, -215.9 dB at 1.5/N, bound -215.2).
pub const LONG_CONTOUR_TAPS: usize = 1 << 19;

/// Contour radii (units of 1/N) tried in order until one passes the checks.
pub fn contour_radii(n: usize) -> &'static [f64] {
    if n >= LONG_CONTOUR_TAPS {
        &[1.5, RADIUS_TAPS, 1.0, 1.25]
    } else {
        &[RADIUS_TAPS, 1.5, 1.0]
    }
}

/// Below this fraction of the peak (-220 dB) the scaled spectrum of a long,
/// high-beta kernel is within ~50 dB of the rounding noise of its own f64
/// coefficients (about -275 dB at 4M taps). That noise has zeros scattered
/// within a few 1/N of the unit circle; where one sits near the contour the
/// unwrapping step is ambiguous, and either choice only reflects (or not) a
/// zero whose effect stays at the noise level. Steps between such bins are
/// therefore not subject to [`MAX_PHASE_STEP`].
const NOISE_LEVEL: f64 = 1.0e-11;

/// Bins below this fraction of the peak (-320 dB, FFT rounding) carry no
/// phase at all: they are floored and continue the local phase trend.
const NOISE_FLOOR: f64 = 1.0e-16;

/// Largest accepted root-path residue past `N` taps (rms, relative).
const ROOT_MAX_TAIL: f64 = 1.0e-9;

/// Aberth sweeps before giving up.
const ROOT_MAX_SWEEPS: usize = 400;

/// What the twin is checked against.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contract {
    /// Kaiser beta of the linear design (sets `A`).
    pub kaiser_beta: f64,
    /// Where the absolute stopband error bound starts, in cycles per sample
    /// (`0.5` or more: nowhere).
    pub stopband_edge: f64,
}

impl Contract {
    /// Kaiser stopband specification `A = beta / 0.1102 + 8.7` dB.
    pub fn attenuation_db(&self) -> f64 {
        self.kaiser_beta / 0.1102 + 8.7
    }

    /// Bound on the absolute stopband error, dB re the DC gain.
    pub fn stopband_error_bound_db(&self) -> f64 {
        -(self.attenuation_db() + STOPBAND_ERROR_MARGIN_DB)
    }
}

/// How the twin was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Polynomial roots, exact reflection, zeros of the result checked.
    Roots,
    /// Radially scaled complex cepstrum.
    Cepstrum,
}

/// Measured agreement of the twin with its contract.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Check {
    /// Largest `| |H_min| - |H_lin| |` in dB where `|H_lin| >= -1 dB`.
    pub passband_db: f64,
    /// The same where `|H_lin| >= -100 dB`.
    pub upper_db: f64,
    /// Largest absolute magnitude error from the stopband edge up, dB re the
    /// DC gain.
    pub stopband_error_db: f64,
    /// Root path: the largest zero radius of the returned kernel.
    pub max_zero_radius: Option<f64>,
    /// Root path: over the zeros outside the unit circle, the largest
    /// relative coefficient perturbation that moves one onto the circle, in
    /// units of [`zero_tolerance`] (at most 1).
    pub max_zero_excess: Option<f64>,
}

/// Result of [`minimum_phase`].
#[derive(Clone, Debug)]
pub struct MinimumPhase {
    /// The minimum-phase kernel, same length as the input.
    pub kernel: Vec<f64>,
    pub method: Method,
    /// FFT length used.
    pub fft_len: usize,
    /// Cepstrum: `alpha * N`, how far outside the unit circle (in units of
    /// 1/N) the cepstrum was evaluated. Roots: 0.
    pub radius_taps: f64,
    /// Cepstrum: largest unwrapped phase step between adjacent bins
    /// (radians). Roots: 0.
    pub max_phase_step: f64,
    /// Number of zeros reflected.
    pub reflected_zeros: usize,
    /// Energy past `N` taps relative to the kept taps (rms): truncation,
    /// aliasing and root-error residue.
    pub tail: f64,
    /// Cepstrum: bins at the f64 noise floor that were floored and
    /// phase-interpolated.
    pub floored_bins: usize,
    /// Always `true` for a result of [`minimum_phase`];
    /// [`minimum_phase_with`] sets it `false` when its checks fail.
    pub exact: bool,
    /// The contract check (`None` from [`minimum_phase_with`]).
    pub check: Option<Check>,
}

/// Why no minimum-phase twin was returned.
#[derive(Clone, Debug, PartialEq)]
pub enum MinimumPhaseError {
    /// The kernel has non-finite or all-zero coefficients.
    BadInput,
    /// The root finder did not converge.
    Roots { taps: usize },
    /// The cepstral phase could not be unwrapped unambiguously.
    Unwrap {
        taps: usize,
        max_phase_step: f64,
        residue: f64,
    },
    /// The root reflection left a residue past `N` taps.
    Residue { taps: usize, tail: f64 },
    /// A zero of the result is outside the unit circle by more than a
    /// relative coefficient change of `tolerance` can explain.
    ZeroOutside {
        taps: usize,
        radius: f64,
        tolerance: f64,
    },
    /// The magnitude does not match within the contract.
    Magnitude {
        taps: usize,
        what: &'static str,
        value_db: f64,
        bound_db: f64,
    },
}

impl std::fmt::Display for MinimumPhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the minimum-phase filter could not be designed to tolerance: "
        )?;
        match self {
            Self::BadInput => write!(f, "the linear-phase kernel is zero or not finite"),
            Self::Roots { taps } => {
                write!(f, "the zeros of the {taps}-tap kernel did not converge")
            }
            Self::Unwrap {
                taps,
                max_phase_step,
                residue,
            } => write!(
                f,
                "the phase of the {taps}-tap kernel is ambiguous (step {max_phase_step:.3} rad, \
                 residue {residue:.3} rad)"
            ),
            Self::Residue { taps, tail } => {
                write!(f, "the {taps}-tap reflection left a residue of {tail:.2e}")
            }
            Self::ZeroOutside {
                taps,
                radius,
                tolerance,
            } => write!(
                f,
                "a zero of the {taps}-tap result is at radius {radius:.12}, outside the unit \
                 circle by more than a relative coefficient change of {tolerance:.1e}"
            ),
            Self::Magnitude {
                taps,
                what,
                value_db,
                bound_db,
            } => write!(
                f,
                "the {taps}-tap result misses its magnitude by {value_db:.3e} dB {what} \
                 (bound {bound_db:.3e} dB)"
            ),
        }
    }
}

impl std::error::Error for MinimumPhaseError {}

/// First contour `alpha` for an `n`-tap kernel on the cepstral path.
pub fn alpha_for(n: usize) -> f64 {
    contour_radii(n)[0] / n.max(1) as f64
}

/// Cepstral FFT length for an `n`-tap kernel: `next_power_of_two(8 n)`.
pub fn fft_len_for(n: usize) -> usize {
    n.saturating_mul(FFT_LEN_PER_TAP)
        .next_power_of_two()
        .max(64)
}

/// Minimum-phase twin of `h`, checked against `contract` (see the module
/// docs).
pub fn minimum_phase(h: &[f64], contract: &Contract) -> Result<MinimumPhase, MinimumPhaseError> {
    let n = h.len();
    if h.iter().any(|x| !x.is_finite()) || h.iter().all(|&x| x == 0.0) {
        return Err(MinimumPhaseError::BadInput);
    }
    if n <= 1 {
        return Ok(MinimumPhase {
            kernel: h.to_vec(),
            method: Method::Roots,
            fft_len: 0,
            radius_taps: 0.0,
            max_phase_step: 0.0,
            reflected_zeros: 0,
            tail: 0.0,
            floored_bins: 0,
            exact: true,
            check: None,
        });
    }
    if n <= ROOT_MAX_TAPS {
        by_roots(h, contract)
    } else {
        by_cepstrum(h, contract)
    }
}

/// The root method at any length (`O(N^2)` time; [`minimum_phase`] uses it
/// up to [`ROOT_MAX_TAPS`] taps), for tests and experiments.
pub fn minimum_phase_by_roots(
    h: &[f64],
    contract: &Contract,
) -> Result<MinimumPhase, MinimumPhaseError> {
    if h.iter().any(|x| !x.is_finite()) || h.iter().all(|&x| x == 0.0) {
        return Err(MinimumPhaseError::BadInput);
    }
    by_roots(h, contract)
}

/// Scaled cepstrum with an explicit FFT length and contour radius
/// (`radius_taps = alpha * N`) and no contract check, for experiments.
/// `exact` is `false` (and `kernel` the input) when the unwrap fails.
pub fn minimum_phase_with(h: &[f64], fft_len: usize, radius_taps: f64) -> MinimumPhase {
    let n = h.len();
    assert!(fft_len >= 2 * n && fft_len.is_multiple_of(2));
    let alpha = radius_taps / n as f64;
    let mut h = h.to_vec();
    let deflated = reflect_nyquist_real_zeros(&mut h, alpha.exp());
    let mut work = Work::new(fft_len);
    match scaled_cepstrum(&h, alpha, &mut work) {
        Ok(mut result) => {
            result.reflected_zeros += deflated;
            result
        }
        Err(_) => MinimumPhase {
            kernel: h,
            method: Method::Cepstrum,
            fft_len,
            radius_taps,
            max_phase_step: f64::NAN,
            reflected_zeros: 0,
            tail: f64::NAN,
            floored_bins: 0,
            exact: false,
            check: None,
        },
    }
}

fn by_cepstrum(h: &[f64], contract: &Contract) -> Result<MinimumPhase, MinimumPhaseError> {
    let n = h.len();
    let mut work = Work::new(fft_len_for(n));
    let lin_mag = unit_magnitudes(h, &mut work);
    let mut last_error = MinimumPhaseError::BadInput;
    for &radius in contour_radii(n) {
        let alpha = radius / n as f64;
        // Zeros between the unit circle and the contour would not be
        // reflected. On these lengths the only ones are real zeros next to
        // z = -1 (see `reflect_nyquist_real_zeros`); reflect them exactly
        // first.
        let mut g = h.to_vec();
        let deflated = reflect_nyquist_real_zeros(&mut g, alpha.exp());
        let attempt = scaled_cepstrum(&g, alpha, &mut work);
        drop(g);
        let mut result = match attempt {
            Ok(result) => result,
            Err(e) => {
                last_error = e;
                continue;
            }
        };
        result.reflected_zeros += deflated;
        match check_magnitude(&lin_mag, &result.kernel, contract, &mut work) {
            Ok(check) => {
                result.check = Some(check);
                return Ok(result);
            }
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}

// ---------------------------------------------------------------------------
// Root path

/// `p(z) = sum c[k] z^(n-k)` at `z`, `n = c.len() - 1`: the Newton ratio
/// `p / p'`, the backward residual `|p| / sum |c_k| |z|^(n-k)` and the radial
/// condition number `sum |c_k| |z|^(n-k) / (|z| |p'|)`. Outside the unit
/// circle it works on the reversed polynomial in `1/z`, so nothing
/// overflows.
struct Eval {
    ratio: Complex64,
    residual: f64,
    cond: f64,
}

fn eval_poly(c: &[f64], z: Complex64) -> Eval {
    let n = c.len() - 1;
    let r = z.norm();
    let zero = Complex64::new(0.0, 0.0);
    if r <= 1.0 {
        let mut p = Complex64::new(c[0], 0.0);
        let mut dp = zero;
        let mut s = c[0].abs();
        for &ck in &c[1..] {
            dp = dp * z + p;
            p = p * z + ck;
            s = s * r + ck.abs();
        }
        let dn = dp.norm();
        Eval {
            ratio: if dn > 0.0 { p / dp } else { zero },
            residual: if s > 0.0 { p.norm() / s } else { 0.0 },
            cond: if dn > 0.0 && r > 0.0 {
                s / (dn * r)
            } else {
                f64::INFINITY
            },
        }
    } else {
        // p(z) = z^n q(w), q(w) = sum c[k] w^k, w = 1/z:
        // p / p' = q / (w (n q - w q')).
        let w = z.inv();
        let rw = 1.0 / r;
        let mut q = Complex64::new(c[n], 0.0);
        let mut dq = zero;
        let mut s = c[n].abs();
        for &ck in c[..n].iter().rev() {
            dq = dq * w + q;
            q = q * w + ck;
            s = s * rw + ck.abs();
        }
        let d = q * n as f64 - w * dq;
        let dn = d.norm();
        Eval {
            ratio: if dn > 0.0 { q / (w * d) } else { zero },
            residual: if s > 0.0 { q.norm() / s } else { 0.0 },
            cond: if dn > 0.0 { s / dn } else { f64::INFINITY },
        }
    }
}

/// Starting points from the Newton polygon of the coefficients (Bini):
/// for each edge of the upper convex hull of `(m, ln|a_m|)` (`a_m` the
/// coefficient of `z^m`), as many points as its width, on the circle of the
/// edge's radius.
fn initial_guesses(c: &[f64]) -> Vec<Complex64> {
    let n = c.len() - 1;
    let pts: Vec<(f64, f64)> = (0..=n)
        .filter_map(|m| {
            let a = c[n - m].abs();
            (a > 0.0).then(|| (m as f64, a.ln()))
        })
        .collect();
    let mut hull: Vec<(f64, f64)> = Vec::new();
    for p in pts {
        while hull.len() >= 2 {
            let (o, a) = (hull[hull.len() - 2], hull[hull.len() - 1]);
            let cross = (a.0 - o.0) * (p.1 - o.1) - (a.1 - o.1) * (p.0 - o.0);
            if cross >= 0.0 {
                hull.pop();
            } else {
                break;
            }
        }
        hull.push(p);
    }
    let mut z = Vec::with_capacity(n);
    for (i, pair) in hull.windows(2).enumerate() {
        let k = (pair[1].0 - pair[0].0).round() as usize;
        let radius = ((pair[0].1 - pair[1].1) / k as f64).exp();
        for j in 0..k {
            let theta = 2.0 * PI * j as f64 / k as f64 + 2.0 * PI * i as f64 / n as f64 + 0.7;
            z.push(Complex64::from_polar(radius, theta));
        }
    }
    z
}

/// `sum_{j != i} 1 / (zi - z[j])`.
fn aberth_sum(z: &[Complex64], i: usize, zi: Complex64) -> Complex64 {
    let mut s = Complex64::new(0.0, 0.0);
    for (j, &zj) in z.iter().enumerate() {
        if j != i {
            let d = zi - zj;
            s += d.conj() / d.norm_sqr();
        }
    }
    s
}

/// Below this degree the root sweeps run on the calling thread.
const ROOT_PARALLEL_MIN: usize = 192;

/// `f(i, &mut out[i], &mut flag[i])` for every `i`, split over a few
/// threads for large `out`.
fn par_rows<F>(out: &mut [Complex64], flag: &mut [bool], f: F)
where
    F: Fn(usize, &mut Complex64, &mut bool) + Sync,
{
    let n = out.len();
    let threads = if n < ROOT_PARALLEL_MIN {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, |t| t.get())
            .clamp(1, 8)
    };
    if threads == 1 {
        for (i, (o, d)) in out.iter_mut().zip(flag.iter_mut()).enumerate() {
            f(i, o, d);
        }
        return;
    }
    let per = n.div_ceil(threads);
    std::thread::scope(|scope| {
        for (chunk, (os, ds)) in out.chunks_mut(per).zip(flag.chunks_mut(per)).enumerate() {
            let f = &f;
            scope.spawn(move || {
                for (k, (o, d)) in os.iter_mut().zip(ds.iter_mut()).enumerate() {
                    f(chunk * per + k, o, d);
                }
            });
        }
    });
}

/// All roots of `p(z) = sum c[k] z^(n-k)` (`c[0]` and `c[n]` non-zero) by
/// the Aberth-Ehrlich iteration, or `None` if it does not converge.
fn polynomial_roots(c: &[f64]) -> Option<Vec<Complex64>> {
    let n = c.len() - 1;
    if n == 0 {
        return Some(Vec::new());
    }
    if n == 1 {
        return Some(vec![Complex64::new(-c[1] / c[0], 0.0)]);
    }
    let eps = f64::EPSILON / 2.0;
    // Converged when the residual reaches the rounding level of the
    // evaluation, or the correction stops moving the root. The accepted
    // residual is a backward error: the roots are exact for a polynomial
    // whose coefficients differ by at most that much, relatively.
    let stop = 4.0 * eps * ((n + 1) as f64).sqrt();
    // Gross failures only: a multiple or clustered root stalls at a larger
    // residual, and the reflection is checked downstream anyway.
    let accept = 1024.0 * eps * (n + 1) as f64;
    let mut z = initial_guesses(c);
    if z.len() != n {
        return None;
    }
    // Total-step (Jacobi) sweeps: every correction from the same iterate, so
    // the roots can be updated in parallel.
    let mut done = vec![false; n];
    let mut next = z.clone();
    for _ in 0..ROOT_MAX_SWEEPS {
        par_rows(&mut next, &mut done, |i, zi_next, done_i| {
            if *done_i {
                return;
            }
            let zi = z[i];
            let e = eval_poly(c, zi);
            let s = aberth_sum(&z, i, zi);
            let corr = e.ratio / (Complex64::new(1.0, 0.0) - e.ratio * s);
            if corr.re.is_finite() && corr.im.is_finite() {
                *zi_next = zi - corr;
            }
            if e.residual <= stop || corr.norm() <= 4.0 * eps * zi.norm() {
                *done_i = true;
            }
        });
        z.copy_from_slice(&next);
        if done.iter().all(|&d| d) {
            break;
        }
    }
    let ok = z.iter().all(|&zi| zi.re.is_finite() && zi.im.is_finite())
        && z.iter().all(|&zi| eval_poly(c, zi).residual <= accept);

    ok.then_some(z)
}

/// Double-double value `hi + lo`.
#[derive(Clone, Copy)]
struct Dd {
    hi: f64,
    lo: f64,
}

impl Dd {
    fn from(x: f64) -> Self {
        Self { hi: x, lo: 0.0 }
    }

    fn quick(hi: f64, lo: f64) -> Self {
        let s = hi + lo;
        Self {
            hi: s,
            lo: lo - (s - hi),
        }
    }

    fn add(self, b: Self) -> Self {
        let s = self.hi + b.hi;
        let bb = s - self.hi;
        let e = (self.hi - (s - bb)) + (b.hi - bb);
        Self::quick(s, e + self.lo + b.lo)
    }

    fn mul_f(self, b: f64) -> Self {
        let p = self.hi * b;
        let e = self.hi.mul_add(b, -p);
        Self::quick(p, e + self.lo * b)
    }

    fn neg(self) -> Self {
        Self {
            hi: -self.hi,
            lo: -self.lo,
        }
    }

    fn value(self) -> f64 {
        self.hi + self.lo
    }
}

/// `sum c[k] x^(m-k)` (`m = c.len() - 1`, Horner from `c[0]` when
/// `forward`, else `sum c[k] x^k`) at complex `x`, the value in
/// double-double and the derivative in f64.
fn horner_dd(c: &[f64], x: Complex64, forward: bool) -> (Complex64, Complex64) {
    let (mut re, mut im) = (Dd::from(0.0), Dd::from(0.0));
    let mut d = Complex64::new(0.0, 0.0);
    let mut step = |ck: f64| {
        d = d * x + Complex64::new(re.value(), im.value());
        // (re + j im) (x.re + j x.im) + ck
        let nre = re.mul_f(x.re).add(im.mul_f(x.im).neg()).add(Dd::from(ck));
        let nim = re.mul_f(x.im).add(im.mul_f(x.re));
        re = nre;
        im = nim;
    };
    if forward {
        c.iter().for_each(|&ck| step(ck));
    } else {
        c.iter().rev().for_each(|&ck| step(ck));
    }
    (Complex64::new(re.value(), im.value()), d)
}

/// Newton-Aberth sweeps with the polynomial evaluated in double-double, so
/// each root ends within a few ulps of an exact root of the f64 polynomial
/// even where it is ill-conditioned (a reciprocal pair close to the circle).
/// Roots outside the unit circle are refined in `w = 1/z`.
fn polish_roots(c: &[f64], z: &mut [Complex64]) {
    let one = Complex64::new(1.0, 0.0);
    let mut next = z.to_vec();
    let mut unused = vec![false; z.len()];
    for _ in 0..4 {
        let cur: &[Complex64] = z;
        par_rows(&mut next, &mut unused, |i, out, _| {
            let zi = cur[i];
            if zi.norm() <= 1.0 {
                let (p, dp) = horner_dd(c, zi, true);
                if dp.norm() == 0.0 {
                    return;
                }
                let ratio = p / dp;
                let corr = ratio / (one - ratio * aberth_sum(cur, i, zi));
                if corr.re.is_finite() && corr.im.is_finite() {
                    *out = zi - corr;
                }
            } else {
                let wi = zi.inv();
                let (q, dq) = horner_dd(c, wi, false);
                if dq.norm() == 0.0 {
                    return;
                }
                let ratio = q / dq;
                let mut s = Complex64::new(0.0, 0.0);
                for (j, &zj) in cur.iter().enumerate() {
                    if j != i {
                        s += (wi - zj.inv()).inv();
                    }
                }
                let w = wi - ratio / (one - ratio * s);
                if w.re.is_finite() && w.im.is_finite() && w.norm() > 0.0 {
                    *out = w.inv();
                }
            }
        });
        z.copy_from_slice(&next);
    }
}

/// Zeros of `sum b[k] z^-k` (the root path's finder, polished in
/// double-double), for tests and experiments. Exact zeros at the origin
/// (trailing zero taps) are left out; `None` if the iteration does not
/// converge or `b[0]` is zero.
pub fn zeros(b: &[f64]) -> Option<Vec<Complex64>> {
    let (lead, trail) = trim_zeros(b);
    if lead > 0 {
        return None;
    }
    let core = &b[..b.len() - trail];
    let mut z = polynomial_roots(core)?;
    polish_roots(core, &mut z);
    Some(z)
}

/// Relative coefficient perturbation of an `n`-tap kernel within which its
/// zeros are only defined: `ZERO_TOLERANCE_ULPS * u * n`.
pub fn zero_tolerance(n: usize) -> f64 {
    ZERO_TOLERANCE_ULPS * f64::EPSILON / 2.0 * n as f64
}

/// The smallest relative coefficient perturbation of `b` that has a zero at
/// every point of the radial segment from `z` to the unit circle (sampled at
/// five points): the pseudozero level `|B(zeta)| / sum |b_k| |zeta|^-k` there
/// (Mosier; Trefethen and Toh). 0 for a zero on or inside the circle.
fn outside_level(b: &[f64], z: Complex64) -> f64 {
    let r = z.norm();
    if r <= 1.0 {
        return 0.0;
    }
    (1..=4)
        .map(|t| eval_poly(b, z * r.powf(-(t as f64) / 4.0)).residual)
        .fold(0.0, f64::max)
}

/// Exact leading and trailing zeros of `h` (a pure delay, and zeros at the
/// origin).
fn trim_zeros(h: &[f64]) -> (usize, usize) {
    let lead = h.iter().position(|&x| x != 0.0).unwrap_or(h.len());
    let trail = h.iter().rev().position(|&x| x != 0.0).unwrap_or(0);
    (lead, trail)
}

fn by_roots(h: &[f64], contract: &Contract) -> Result<MinimumPhase, MinimumPhaseError> {
    let n = h.len();
    let (lead, trail) = trim_zeros(h);
    let core = &h[lead..n - trail];
    let m = core.len();
    let l = (n * ROOT_FFT_LEN_PER_TAP).next_power_of_two().max(512);
    let mut work = Work::new(l);
    let lin_mag = unit_magnitudes(h, &mut work);

    // Zeros of `core`, and the ones to reflect.
    let mut kernel = vec![0.0; n];
    let mut reflected = 0;
    let mut tail = 0.0;
    if m >= 2 {
        let mut roots = polynomial_roots(core).ok_or(MinimumPhaseError::Roots { taps: n })?;
        polish_roots(core, &mut roots);
        // Reflect every zero further outside than its polished position is
        // uncertain: a few ulps, plus the double-double residual times the
        // condition number.
        let eps = f64::EPSILON / 2.0;
        let outside: Vec<Complex64> = roots
            .iter()
            .copied()
            .filter(|&z| {
                let cond = eval_poly(core, z).cond;
                let uncertain = 64.0 * (eps + (m + 1) as f64 * eps * eps * cond);
                z.norm() - 1.0 > uncertain
            })
            .collect();
        reflected = outside.len();

        // B = H * prod (conj z - e^-jw) / (1 - z e^-jw) on the grid.
        work.time.fill(0.0);
        work.time[..m].copy_from_slice(core);
        work.forward();
        let half = l / 2;
        for (k, c) in work.spec.iter_mut().enumerate() {
            let e = Complex64::from_polar(1.0, -PI * k as f64 / half as f64);
            let mut num = Complex64::new(1.0, 0.0);
            let mut den = Complex64::new(1.0, 0.0);
            for &z in &outside {
                num *= z.conj() - e;
                den *= Complex64::new(1.0, 0.0) - z * e;
                // Keep the running products in range.
                let scale = den.norm();
                if !(1.0e-100..=1.0e100).contains(&scale) {
                    num /= scale;
                    den /= scale;
                }
            }
            *c *= num / den;
        }
        work.inverse();
        let sign = if work.time[..m].iter().sum::<f64>() * core.iter().sum::<f64>() < 0.0 {
            -1.0
        } else {
            1.0
        };
        for (dst, &v) in kernel.iter_mut().zip(&work.time[..m]) {
            *dst = sign * v;
        }
        let kept: f64 = kernel[..m].iter().map(|x| x * x).sum();
        let past: f64 = work.time[m..].iter().map(|x| x * x).sum();
        tail = (past / kept.max(f64::MIN_POSITIVE)).sqrt();
        if tail.is_nan() || tail > ROOT_MAX_TAIL {
            return Err(MinimumPhaseError::Residue { taps: n, tail });
        }
    } else {
        kernel[..m].copy_from_slice(core);
    }
    // A pure delay is a zero at infinity: reflected to the origin, it becomes
    // trailing zeros (already there: `kernel` is zero past `m`).
    reflected += lead;

    // Zeros of the result.
    let (b_lead, b_trail) = trim_zeros(&kernel);
    if b_lead > 0 {
        return Err(MinimumPhaseError::ZeroOutside {
            taps: n,
            radius: f64::INFINITY,
            tolerance: 0.0,
        });
    }
    let b = &kernel[..n - b_trail];
    let allowed = zero_tolerance(n);
    let (mut max_radius, mut max_excess) = (0.0_f64, 0.0_f64);
    if b.len() >= 2 {
        let zeros = polynomial_roots(b).ok_or(MinimumPhaseError::Roots { taps: n })?;
        for z in zeros {
            let radius = z.norm();
            max_radius = max_radius.max(radius);
            let excess = outside_level(b, z) / allowed;
            max_excess = max_excess.max(excess);
            if excess.is_nan() || excess > 1.0 {
                return Err(MinimumPhaseError::ZeroOutside {
                    taps: n,
                    radius,
                    tolerance: allowed,
                });
            }
        }
    }

    let mut check = check_magnitude(&lin_mag, &kernel, contract, &mut work)?;
    check.max_zero_radius = Some(max_radius);
    check.max_zero_excess = Some(max_excess);
    Ok(MinimumPhase {
        kernel,
        method: Method::Roots,
        fft_len: l,
        radius_taps: 0.0,
        max_phase_step: 0.0,
        reflected_zeros: reflected,
        tail,
        floored_bins: 0,
        exact: true,
        check: Some(check),
    })
}

// ---------------------------------------------------------------------------
// Magnitude check

/// `|H|` on the `work.fft_len` grid, bins `0 ..= L/2`.
fn unit_magnitudes(h: &[f64], w: &mut Work) -> Vec<f64> {
    w.time.fill(0.0);
    w.time[..h.len()].copy_from_slice(h);
    w.forward();
    w.spec.iter().map(|c| c.norm()).collect()
}

/// The contract's magnitude tolerances of `min` against `lin_mag`
/// ([`unit_magnitudes`] of the linear twin on the same grid).
fn check_magnitude(
    lin_mag: &[f64],
    min: &[f64],
    contract: &Contract,
    w: &mut Work,
) -> Result<Check, MinimumPhaseError> {
    let taps = min.len();
    w.time.fill(0.0);
    w.time[..taps].copy_from_slice(min);
    w.forward();
    let reference = lin_mag[0].abs().max(f64::MIN_POSITIVE);
    let upper_level = 10.0_f64.powf(UPPER_LEVEL_DB / 20.0) * reference;
    let passband_level = 10.0_f64.powf(-1.0 / 20.0) * reference;
    let edge_bin = (contract.stopband_edge * w.fft_len as f64).ceil();
    let (mut passband, mut upper, mut stop_err) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (k, (&a, c)) in lin_mag.iter().zip(&w.spec).enumerate() {
        let b = c.norm();
        if !b.is_finite() {
            return Err(MinimumPhaseError::BadInput);
        }
        if a >= upper_level {
            let d = (20.0 * (b / a).log10()).abs();
            upper = upper.max(d);
            if a >= passband_level {
                passband = passband.max(d);
            }
        }
        if k as f64 >= edge_bin {
            stop_err = stop_err.max((b - a).abs() / reference);
        }
    }
    let stop_db = 20.0 * stop_err.max(1.0e-300).log10();
    let fail = |what, value_db, bound_db| {
        Err(MinimumPhaseError::Magnitude {
            taps,
            what,
            value_db,
            bound_db,
        })
    };
    if passband.is_nan() || passband > PASSBAND_TOLERANCE_DB {
        return fail("in the passband", passband, PASSBAND_TOLERANCE_DB);
    }
    if upper.is_nan() || upper > UPPER_TOLERANCE_DB {
        return fail("above -100 dB", upper, UPPER_TOLERANCE_DB);
    }
    if stop_db.is_nan() || stop_db > contract.stopband_error_bound_db() {
        return fail(
            "in the stopband (absolute)",
            stop_db,
            contract.stopband_error_bound_db(),
        );
    }
    Ok(Check {
        passband_db: passband,
        upper_db: upper,
        stopband_error_db: stop_db,
        max_zero_radius: None,
        max_zero_excess: None,
    })
}

// ---------------------------------------------------------------------------
// Cepstral path

/// `sum h[n] z^-n` at the real point `z = -x` (Horner in `-1/x`, `x >= 1`).
fn eval_negative_real(h: &[f64], x: f64) -> f64 {
    let y = -1.0 / x;
    h.iter().rev().fold(0.0, |acc, &c| acc * y + c)
}

/// Grid points per contour radius scanned for real zeros near `z = -1`.
const NYQUIST_SCAN_POINTS: usize = 64;

/// Reflect every real zero `z = -x` of `h` with `1 < x < rho` to `-1/x`, in
/// place, and return how many there were.
///
/// A linear-phase kernel of odd length has a stationary point of its
/// zero-phase amplitude at the output Nyquist. When the last half sidelobe
/// stops just short of zero there, the kernel has a reciprocal pair of real
/// zeros `-x`, `-1/x` arbitrarily close to the unit circle (0.88/N for 63
/// taps, x2, beta 8), which no contour radius can separate. The zero is
/// found by bisection on sign changes over `NYQUIST_SCAN_POINTS` points of
/// `(1, rho]` and reflected exactly: `H(z) = (1 + x z^-1) Q(z)` (division
/// from the top end, stable for `x > 1`), then `H'(z) = (x + z^-1) Q(z)`,
/// which has the same magnitude on the unit circle.
fn reflect_nyquist_real_zeros(h: &mut [f64], rho: f64) -> usize {
    let n = h.len();
    if n < 2 {
        return 0;
    }
    let mut found = 0;
    let ln_rho = rho.ln();
    let at = |i: usize| (ln_rho * i as f64 / NYQUIST_SCAN_POINTS as f64).exp();
    let mut i = 1;
    let mut prev_x = at(1);
    let mut prev = eval_negative_real(h, prev_x);
    while i < NYQUIST_SCAN_POINTS {
        i += 1;
        let x = at(i);
        let f = eval_negative_real(h, x);
        if prev != 0.0 && f != 0.0 && (prev < 0.0) != (f < 0.0) {
            let (mut lo, mut hi, mut f_lo) = (prev_x, x, prev);
            for _ in 0..80 {
                let mid = 0.5 * (lo + hi);
                let f_mid = eval_negative_real(h, mid);
                if (f_mid < 0.0) == (f_lo < 0.0) {
                    lo = mid;
                    f_lo = f_mid;
                } else {
                    hi = mid;
                }
            }
            let root = 0.5 * (lo + hi);
            // h[k] = q[k] + root q[k-1], solved from the top: q[n-2] =
            // h[n-1] / root, q[k-1] = (h[k] - q[k]) / root.
            let mut q = vec![0.0; n - 1];
            q[n - 2] = h[n - 1] / root;
            for k in (1..n - 1).rev() {
                q[k - 1] = (h[k] - q[k]) / root;
            }
            // h'[k] = root q[k] + q[k-1].
            for k in 0..n {
                let a = if k < n - 1 { root * q[k] } else { 0.0 };
                let b = if k > 0 { q[k - 1] } else { 0.0 };
                h[k] = a + b;
            }
            found += 1;
            // The reflected zero is inside now; rescan from here.
            prev = eval_negative_real(h, x);
        } else {
            prev = f;
        }
        prev_x = x;
    }
    found
}

struct Work {
    fft_len: usize,
    fwd: std::sync::Arc<dyn realfft::RealToComplex<f64>>,
    inv: std::sync::Arc<dyn realfft::ComplexToReal<f64>>,
    time: Vec<f64>,
    spec: Vec<Complex64>,
    scratch: Vec<Complex64>,
}

impl Work {
    fn new(fft_len: usize) -> Self {
        let mut planner = RealFftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(fft_len);
        let inv = planner.plan_fft_inverse(fft_len);
        let scratch_len = fwd.get_scratch_len().max(inv.get_scratch_len());
        Self {
            fft_len,
            fwd,
            inv,
            time: vec![0.0; fft_len],
            spec: vec![Complex64::new(0.0, 0.0); fft_len / 2 + 1],
            scratch: vec![Complex64::new(0.0, 0.0); scratch_len],
        }
    }

    /// `spec <- FFT(time)` (`time` is clobbered).
    fn forward(&mut self) {
        self.fwd
            .process_with_scratch(&mut self.time, &mut self.spec, &mut self.scratch)
            .expect("minimum-phase forward FFT");
    }

    /// `time <- IFFT(spec) / L` (`spec` is clobbered).
    fn inverse(&mut self) {
        let last = self.spec.len() - 1;
        self.spec[0].im = 0.0;
        self.spec[last].im = 0.0;
        self.inv
            .process_with_scratch(&mut self.spec, &mut self.time, &mut self.scratch)
            .expect("minimum-phase inverse FFT");
        let inv_n = 1.0 / self.fft_len as f64;
        for v in &mut self.time {
            *v *= inv_n;
        }
    }

    /// From a real cepstrum-domain sequence in `time` that is already causal,
    /// compute `time <- IFFT(exp(FFT(time)))`.
    fn exponentiate(&mut self) {
        self.forward();
        for c in &mut self.spec {
            *c = Complex64::from_polar(c.re.exp(), c.im);
        }
        self.inverse();
    }
}

fn scaled_cepstrum(h: &[f64], alpha: f64, w: &mut Work) -> Result<MinimumPhase, MinimumPhaseError> {
    let n = h.len();
    let l = w.fft_len;
    let half = l / 2;
    let unwrap_error = |max_phase_step, residue| MinimumPhaseError::Unwrap {
        taps: n,
        max_phase_step,
        residue,
    };

    // g[k] = h[k] e^{-alpha k}, zero padded.
    w.time.fill(0.0);
    for (k, (dst, &x)) in w.time.iter_mut().zip(h).enumerate() {
        *dst = x * (-alpha * k as f64).exp();
    }
    w.forward();

    // A low-pass has positive gain at DC on any circle close to the unit
    // circle; flip the sign otherwise so the log starts on the real axis.
    let sign = if w.spec[0].re < 0.0 { -1.0 } else { 1.0 };
    if w.spec[0].re == 0.0 {
        return Err(unwrap_error(f64::NAN, f64::NAN));
    }

    // ln|G| + j * unwrapped arg G, in place. Bins below the f64 rounding
    // floor of the transform (deep stopband of long, high-beta kernels) carry
    // no phase information: they get the floor as magnitude and continue the
    // local phase trend; the next reliable bin is put on the 2*pi branch
    // nearest to that prediction.
    let peak = w.spec.iter().map(|c| c.norm()).fold(0.0_f64, f64::max);
    let floor = peak * NOISE_FLOOR;
    let noise_level = peak * NOISE_LEVEL;
    let mut prev = w.spec[0] * sign;
    let mut prev_reliable = true;
    let mut phase = 0.0_f64;
    let mut trend = 0.0_f64;
    let mut max_step = 0.0_f64;
    let mut floored = 0usize;
    let mut last_reliable = 0usize;
    for (k, c) in w.spec.iter_mut().enumerate() {
        let z = *c * sign;
        let mag = z.norm();
        if !mag.is_finite() {
            return Err(MinimumPhaseError::BadInput);
        }
        let reliable = mag >= floor;
        if k > 0 {
            if reliable && prev_reliable {
                let step = (z * prev.conj()).arg();
                // A large step next to a deep bin is a rounding-noise zero of
                // the f64 kernel crossing near the contour (see NOISE_LEVEL);
                // either branch only moves that zero across the contour.
                if mag.max(prev.norm()) >= noise_level {
                    max_step = max_step.max(step.abs());
                }
                phase += step;
                trend = step;
            } else if reliable {
                let predicted = phase + trend;
                let measured = z.arg();
                phase = measured + 2.0 * PI * ((predicted - measured) / (2.0 * PI)).round();
            } else {
                phase += trend;
                floored += 1;
            }
        }
        if reliable {
            last_reliable = k;
        }
        prev = z;
        prev_reliable = reliable;
        *c = Complex64::new(mag.max(floor).ln(), phase);
    }
    // arg G(pi) = -pi * (zeros outside the circle); remove that pure delay.
    // G(pi) is real, so the exact end phase is a multiple of pi; the residue
    // is accumulated rounding (and trend error if Nyquist itself is floored).
    let lag = (-phase / PI).round();
    let residue = phase + lag * PI;

    let max_residue = if last_reliable < half {
        MAX_RESIDUE_FLOORED
    } else {
        MAX_RESIDUE
    };
    if max_step > MAX_PHASE_STEP || residue.abs() > max_residue || lag < 0.0 {
        return Err(unwrap_error(max_step, residue));
    }
    // Spread the residue linearly over the trailing floored run (or the last
    // bin) so the log spectrum ends exactly on the real axis.
    let from = last_reliable.min(half - 1);
    let start = w.spec[from].im;
    let end = -lag * PI;
    let span = (half - from) as f64;
    for (i, c) in w.spec[from + 1..=half].iter_mut().enumerate() {
        c.im = start + (end - start) * ((i + 1) as f64 / span);
    }
    let dw = PI / half as f64;
    for (k, c) in w.spec.iter_mut().enumerate() {
        c.im += lag * dw * k as f64;
    }
    w.inverse();

    // Reflect: c[m] = g^[m] + g^[-m] rho^{-2m}; the delay removal scaled the
    // gain by rho^{-lag}, restore it in c[0].
    let (causal, anti) = w.time.split_at_mut(half);
    for (m, c) in causal.iter_mut().enumerate().skip(1) {
        *c += anti[half - m] * (-2.0 * alpha * m as f64).exp();
    }
    anti[1..].fill(0.0);
    w.time[0] += lag * alpha;
    w.exponentiate();

    let mut kernel = Vec::with_capacity(n);
    let mut kept = 0.0;
    for (k, &v) in w.time[..n].iter().enumerate() {
        let x = sign * v * (alpha * k as f64).exp();
        kept += x * x;
        kernel.push(x);
    }
    // Discarded samples past N (causal side only: the wrapped end of the
    // buffer holds negative times, which would blow up when unscaled).
    let tail: f64 = w.time[n..half.max(n)]
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = v * (alpha * (n + i) as f64).exp();
            x * x
        })
        .sum();
    if kernel.iter().any(|x| !x.is_finite()) {
        return Err(MinimumPhaseError::BadInput);
    }
    Ok(MinimumPhase {
        kernel,
        method: Method::Cepstrum,
        fft_len: l,
        radius_taps: alpha * n as f64,
        max_phase_step: max_step,
        reflected_zeros: lag as usize,
        tail: (tail / kept.max(f64::MIN_POSITIVE)).sqrt(),
        floored_bins: floored,
        exact: true,
        check: None,
    })
}
