//! Minimum-phase tilt with sharp corners. The target is the ideal
//! piecewise-linear response in dB versus log2 frequency: flat below
//! `start_hz`, a constant slope up to `end_hz`, flat above.
//!
//! It is built from an exact decomposition of that target:
//! `sharp = smooth + corner(start) - corner(end)`, where `smooth` is the
//! fractional first-order shelf ramp (a difference of two softplus curves)
//! and `corner` is the cusp-shaped remainder `slope * k(log2(f / fc))`,
//! `k(x) = -1/2 * log2(1 + 4^-|x|)`. A cascade of first-order shelves fits
//! `smooth`; four peaking-type biquads per corner fit each cusp. Both fits
//! run in the digital domain against physical-Hz targets, so bilinear warping
//! is corrected, and each corner fits the residual the previous stages
//! actually leave.
//!
//! First-order section: `H(z) = (b0 + b1 z^-1) / (1 + a1 z^-1)`, DC gain 1,
//! Nyquist gain `10^(G/20)`. Corner section: the bilinear image of
//! `(s^2 + s w/Qz + w^2) / (s^2 + s w/Qp + w^2)`, unity at DC and Nyquist.
//! With `w > 0` and `Qz, Qp > 0` every pole and zero is strictly inside the
//! unit circle, so the cascade is stable and minimum-phase by construction.

use crate::Complex64;
use crate::biquad::{BiquadState, Coeffs};
use audivane_protocol::{TILT_MAX_HZ, TILT_MIN_HZ, TiltConfig, TiltPivot, check_tilt_sections};
use std::f64::consts::PI;

/// Normalized first-order shelf coefficients (`a0 == 1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShelfCoeffs {
    pub b0: f64,
    pub b1: f64,
    pub a1: f64,
}

impl ShelfCoeffs {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        a1: 0.0,
    };

    /// Design a first-order high-shelf at corner `fc_hz` with high-frequency
    /// gain `gain_db` at `sample_rate`.
    pub fn high_shelf(fc_hz: f64, gain_db: f64, sample_rate: f64) -> Self {
        if !sample_rate.is_finite()
            || sample_rate <= 0.0
            || !fc_hz.is_finite()
            || fc_hz <= 0.0
            || fc_hz >= 0.5 * sample_rate
            || !gain_db.is_finite()
            || gain_db == 0.0
        {
            return Self::IDENTITY;
        }
        let k = (PI * fc_hz / sample_rate).tan();
        let s = 10f64.powf(gain_db / 40.0); // sqrt(g)
        let inv_s = 1.0 / s;
        let a0 = k + inv_s;
        if !a0.is_finite() || a0 == 0.0 {
            return Self::IDENTITY;
        }
        let inv_a0 = 1.0 / a0;
        let c = Self {
            b0: (k + s) * inv_a0,
            b1: (k - s) * inv_a0,
            a1: (k - inv_s) * inv_a0,
        };
        if c.b0.is_finite() && c.b1.is_finite() && c.a1.is_finite() {
            c
        } else {
            Self::IDENTITY
        }
    }

    /// Complex frequency response at `f_hz`.
    pub fn response(&self, f_hz: f64, sample_rate: f64) -> Complex64 {
        if !sample_rate.is_finite() || sample_rate <= 0.0 || !f_hz.is_finite() {
            return Complex64::new(f64::NAN, f64::NAN);
        }
        let w = 2.0 * PI * f_hz / sample_rate;
        let (sin_w, cos_w) = w.sin_cos();
        // z^-1 = cos(w) - j sin(w)
        let nr = self.b0 + self.b1 * cos_w;
        let ni = -self.b1 * sin_w;
        let dr = 1.0 + self.a1 * cos_w;
        let di = -self.a1 * sin_w;
        let den = dr * dr + di * di;
        Complex64::new((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
    }
}

/// One-word state of a transposed Direct Form II first-order section.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShelfState {
    z1: f64,
}

impl ShelfState {
    #[inline]
    pub fn step(&mut self, c: &ShelfCoeffs, x: f64) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y;
        y
    }
}

/// Accuracy contract, against the sharp target from 20 Hz to
/// `min(20 kHz, 0.45 fs)`. A design that misses it is an error.
pub const MAX_ERROR_DB: f64 = 0.1;
/// Corner zones: within this many octaves of `start_hz` or `end_hz`.
pub const CORNER_ZONE_OCT: f64 = 1.0 / 6.0;
/// Error allowance inside a corner zone, per dB/oct of slope, on top of
/// [`MAX_ERROR_DB`]: `MAX_ERROR_DB + CORNER_ALLOWANCE_PER_SLOPE * |slope|`.
pub const CORNER_ALLOWANCE_PER_SLOPE: f64 = 0.01;
/// The delivered change from `start_hz` to `end_hz` is the requested
/// (effective) total within this many dB.
pub const MAX_TOTAL_ERROR_DB: f64 = 0.1;
/// Effective totals smaller than this are an identity filter.
pub const IDENTITY_TOTAL_DB: f64 = 0.001;

/// Designed tilt filter coefficients for a single sample rate.
#[derive(Clone, Debug, PartialEq)]
pub struct TiltFilter {
    /// Linear broadband gain that puts the pivot at 0 dB.
    pub gain: f64,
    /// First-order high-shelf sections (the smooth ramp) in series.
    pub shelves: Vec<ShelfCoeffs>,
    /// Second-order corner sections in series (after the shelves).
    pub corners: Vec<Coeffs>,
    /// Centre frequency (Hz) of each corner section, for response grids.
    pub corner_hz: Vec<f64>,
    /// Sample rate the filter was designed for.
    pub sample_rate: f64,
    /// Set to `Some(effective_end_hz)` when `cfg.end_hz` exceeded `0.45 * fs`
    /// and was clamped at build time.
    pub clamped_end_hz: Option<f64>,
}

/// Per-channel processing state of a [`TiltFilter`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TiltState {
    shelves: Vec<ShelfState>,
    corners: Vec<BiquadState>,
}

impl TiltState {
    pub fn reset(&mut self) {
        self.shelves.fill(ShelfState::default());
        self.corners.fill(BiquadState::default());
    }

    /// Whether [`Self::copy_from`] can take `other` (the same sections).
    pub(crate) fn same_shape(&self, other: &Self) -> bool {
        self.shelves.len() == other.shelves.len() && self.corners.len() == other.corners.len()
    }

    /// Take `other`'s state for an input `k` times as large (the sections
    /// are linear; [`Self::same_shape`]). Never allocates.
    pub(crate) fn copy_scaled(&mut self, other: &Self, k: f64) {
        for (d, s) in self.shelves.iter_mut().zip(&other.shelves) {
            d.z1 = s.z1 * k;
        }
        for (d, s) in self.corners.iter_mut().zip(&other.corners) {
            *d = s.scaled(k);
        }
    }
}

impl TiltFilter {
    pub fn identity(sample_rate: f64) -> Self {
        Self {
            gain: 1.0,
            shelves: Vec::new(),
            corners: Vec::new(),
            corner_hz: Vec::new(),
            sample_rate,
            clamped_end_hz: None,
        }
    }

    /// The same sections and gain, bit for bit (a state made by one is the
    /// other's).
    pub(crate) fn same_filter(&self, other: &Self) -> bool {
        self.gain == other.gain && self.shelves == other.shelves && self.corners == other.corners
    }

    /// The slowest section's time constant, frames (see
    /// [`Coeffs::decay_frames`]).
    pub(crate) fn decay_frames(&self) -> f64 {
        let shelves = self
            .shelves
            .iter()
            .map(|c| crate::biquad::pole_decay(c.a1.abs()));
        let corners = self.corners.iter().map(Coeffs::decay_frames);
        shelves.chain(corners).fold(0.0, f64::max)
    }

    pub fn is_identity(&self) -> bool {
        self.shelves.is_empty() && self.corners.is_empty() && (self.gain - 1.0).abs() < 1e-15
    }

    /// Total number of sections (first- and second-order).
    pub fn section_count(&self) -> usize {
        self.shelves.len() + self.corners.len()
    }

    /// Zeroed processing state sized for this filter (allocates; build time).
    pub fn new_state(&self) -> TiltState {
        TiltState {
            shelves: vec![ShelfState::default(); self.shelves.len()],
            corners: vec![BiquadState::default(); self.corners.len()],
        }
    }

    /// Frequencies where the response has structure (section corners), for
    /// peak searches such as the auto preamp.
    pub fn critical_frequencies(&self) -> Vec<f64> {
        let fs = self.sample_rate;
        self.shelves
            .iter()
            .map(|c| fs / PI * ((1.0 + c.a1) / (1.0 - c.a1)).atan())
            .chain(self.corner_hz.iter().copied())
            .filter(|f| f.is_finite())
            .collect()
    }

    /// Design off the audio thread. Invalid effective bands and designs that
    /// miss the accuracy contract are errors, never silent identity filters.
    pub fn design(cfg: &TiltConfig, sample_rate: f64) -> Result<Self, String> {
        Self::design_section(cfg, sample_rate).map(|(filter, _)| filter)
    }

    /// The EQ's whole tilt stage: `tilt` and, for a two-section tilt,
    /// `tilt_high` (see [`audivane_protocol::check_tilt_pair`]), each designed
    /// on its own and run as one cascade. The cascade is checked against the sum
    /// of the two targets with the single-section limits.
    pub fn design_stage(
        tilt: Option<&TiltConfig>,
        tilt_high: Option<&TiltConfig>,
        sample_rate: f64,
    ) -> Result<Self, String> {
        check_tilt_sections(tilt, tilt_high)?;
        let mut out = Self::identity(sample_rate);
        let mut targets = Vec::with_capacity(2);
        if let Some(cfg) = tilt {
            let (filter, target) = Self::design_section(cfg, sample_rate)?;
            out.append(filter);
            targets.extend(target);
        }
        if let Some(cfg) = tilt_high {
            let (filter, target) = Self::design_section(cfg, sample_rate)
                .map_err(|e| e.replacen("tilt:", "tilt_high:", 1))?;
            out.append(filter);
            targets.extend(target);
        }
        if targets.len() == 2 {
            let corners: Vec<(f64, f64)> = targets
                .iter()
                .flat_map(|t| [(t.start, t.slope.abs()), (t.end, t.slope.abs())])
                .collect();
            out.check_target(|f| targets.iter().map(|t| t.db(f)).sum(), &corners)
                .map_err(|e| e.replacen("tilt:", "tilt (both sections):", 1))?;
        }
        Ok(out)
    }

    /// Put `next` in series after `self`.
    fn append(&mut self, next: Self) {
        self.gain *= next.gain;
        self.shelves.extend(next.shelves);
        self.corners.extend(next.corners);
        self.corner_hz.extend(next.corner_hz);
        self.clamped_end_hz = next.clamped_end_hz.or(self.clamped_end_hz);
    }

    /// One section and its normalized target (`None` for an identity).
    fn design_section(
        cfg: &TiltConfig,
        sample_rate: f64,
    ) -> Result<(Self, Option<SectionTarget>), String> {
        if !cfg.enabled {
            return Ok((Self::identity(sample_rate), None));
        }
        let (start, end) = effective_band(cfg, sample_rate)?;
        let slope = cfg
            .effective_slope_db_per_oct()
            .ok_or("tilt: specify exactly one finite slope_db_per_oct or total_db")?;
        let total = cfg.effective_total_db().ok_or("tilt: invalid total gain")?;
        if slope.abs() > 6.0 + 1e-9 || total.abs() > 24.0 + 1e-9 {
            return Err("tilt: slope must be within +/-6 dB/oct and total within +/-24 dB".into());
        }
        let clamped_end_hz = (end < cfg.end_hz).then_some(end);
        let total_eff = slope * (end / start).log2();
        if total_eff.abs() < IDENTITY_TOTAL_DB {
            let identity = Self {
                clamped_end_hz,
                ..Self::identity(sample_rate)
            };
            return Ok((identity, None));
        }
        // The target is flat above the effective end, so an Hz pivot above it
        // is the `end` pivot: anchoring there (possibly above 0.45 fs, where
        // no fit controls the response) would shift the whole curve.
        let pivot = match cfg.pivot {
            TiltPivot::Hz(hz) if hz > end => TiltPivot::End,
            p => p,
        };
        let pivot_hz = match pivot {
            TiltPivot::Hz(hz) => Some(hz),
            TiltPivot::Center => Some((start * end).sqrt()),
            _ => None,
        };
        let fs = sample_rate;

        // 1. Smooth ramp: first-order shelves fitted to the softplus prototype.
        let shelves = smooth_cascade(slope, start, end, total_eff, fs, pivot_hz)?;
        let shelf_db = |f: f64| -> f64 {
            let mut h = Complex64::new(1.0, 0.0);
            for c in &shelves {
                h *= c.response(f, fs);
            }
            crate::lin_to_db(h.norm())
        };
        let sharp = |f: f64| sharp_db(slope, start, end, f);
        let sign = slope.signum();

        // 2. Start corner: fit what the shelves leave, minus the ideal end
        //    corner (fitted next, against the actual remainder).
        let corner_a = fit_corner(start, fs, slope.abs(), sign, |f| {
            sharp(f) - shelf_db(f) + slope * cusp(f, end)
        })?;
        let a_db = |f: f64| corner_a.db(f, fs);
        // 3. End corner: fit the actual remainder of shelves + start corner.
        let corner_b = fit_corner(end, fs, slope.abs(), -sign, |f| {
            sharp(f) - shelf_db(f) - a_db(f)
        })?;

        let mut corners = corner_a.coeffs(fs);
        corners.extend(corner_b.coeffs(fs));
        let mut corner_hz = corner_a.centres_hz(fs);
        corner_hz.extend(corner_b.centres_hz(fs));
        let mut result = Self {
            gain: 1.0,
            shelves,
            corners,
            corner_hz,
            sample_rate,
            clamped_end_hz,
        };

        // Normalize on the actual cascade. Start/end anchor the flat side at
        // DC/Nyquist, where every section's gain is exact by construction.
        let anchor = match pivot {
            TiltPivot::Start => 0.0,
            TiltPivot::End => fs / 2.0,
            _ => pivot_hz.unwrap_or(0.0),
        };
        result.gain = 1.0 / result.response(anchor).norm();
        let offset = match pivot {
            TiltPivot::Start => 0.0,
            TiltPivot::End => total_eff,
            _ => sharp(anchor),
        };
        if !result.gain.is_finite() || result.gain <= 0.0 {
            return Err("tilt: non-finite normalization gain".into());
        }
        let target = SectionTarget {
            slope,
            start,
            end,
            offset,
        };
        result.validate(&target, total_eff)?;
        Ok((result, Some(target)))
    }

    /// Independent dense check of the finished, normalized filter against
    /// the sharp target, the corner allowance and the delivered total.
    fn validate(&self, target: &SectionTarget, total: f64) -> Result<(), String> {
        let (start, end) = (target.start, target.end);
        let abs_slope = target.slope.abs();
        self.check_target(|f| target.db(f), &[(start, abs_slope), (end, abs_slope)])?;
        let delivered = crate::lin_to_db(self.response(end).norm())
            - crate::lin_to_db(self.response(start).norm());
        if !delivered.is_finite() || (delivered - total).abs() > MAX_TOTAL_ERROR_DB {
            return Err(format!(
                "tilt: delivered {delivered:.3} dB from start to end, requested {total:.3} dB"
            ));
        }
        Ok(())
    }

    /// Dense check of the response against `target` (dB) from 20 Hz to
    /// `min(20 kHz, 0.45 fs)`: [`MAX_ERROR_DB`], plus the corner allowance
    /// within [`CORNER_ZONE_OCT`] of each `(corner_hz, |slope|)`.
    fn check_target(
        &self,
        target: impl Fn(f64) -> f64,
        corners: &[(f64, f64)],
    ) -> Result<(), String> {
        let fs = self.sample_rate;
        let eval_end = 20_000.0_f64.min(0.45 * fs);
        let check = |f: f64| -> Result<(), String> {
            let error = (crate::lin_to_db(self.response(f).norm()) - target(f)).abs();
            let limit = corners
                .iter()
                .filter(|(hz, _)| (f / hz).log2().abs() < CORNER_ZONE_OCT)
                .map(|(_, abs_slope)| MAX_ERROR_DB + CORNER_ALLOWANCE_PER_SLOPE * abs_slope)
                .fold(MAX_ERROR_DB, f64::max);
            if !error.is_finite() || error > limit {
                return Err(format!(
                    "tilt: response validation failed at {f:.3} Hz ({error:.4} dB error; limit {limit:.3} dB)"
                ));
            }
            Ok(())
        };
        if eval_end > 20.0 {
            for i in 0..=2048 {
                check(20.0 * (eval_end / 20.0).powf(i as f64 / 2048.0))?;
                check(20.0 + (eval_end - 20.0) * i as f64 / 2048.0)?;
            }
        }
        Ok(())
    }

    /// Complex frequency response at `f_hz`.
    pub fn response(&self, f_hz: f64) -> Complex64 {
        let fs = self.sample_rate;
        if !fs.is_finite() || fs <= 0.0 || !f_hz.is_finite() {
            return Complex64::new(f64::NAN, f64::NAN);
        }
        // One sin/cos per frequency: z^-1 and z^-2 are shared by every section.
        let w = 2.0 * PI * f_hz / fs;
        let z1 = Complex64::new(w.cos(), -w.sin());
        let z2 = z1 * z1;
        let mut h = Complex64::new(self.gain, 0.0);
        for c in &self.shelves {
            h *= (c.b0 + c.b1 * z1) / (1.0 + c.a1 * z1);
        }
        for c in &self.corners {
            h *= (c.b0 + c.b1 * z1 + c.b2 * z2) / (1.0 + c.a1 * z1 + c.a2 * z2);
        }
        h
    }

    /// Process one channel slice in place (allocation-free). `state` must
    /// come from [`Self::new_state`] of this filter.
    pub fn process_channel(&self, buf: &mut [f64], state: &mut TiltState) {
        if self.gain != 1.0 {
            for x in buf.iter_mut() {
                *x *= self.gain;
            }
        }
        for (c, st) in self.shelves.iter().zip(state.shelves.iter_mut()) {
            for x in buf.iter_mut() {
                *x = st.step(c, *x);
            }
        }
        for (c, st) in self.corners.iter().zip(state.corners.iter_mut()) {
            for x in buf.iter_mut() {
                *x = st.step(c, *x);
            }
        }
    }
}

/// One designed section's sharp target, after pivot normalization.
#[derive(Clone, Copy, Debug)]
struct SectionTarget {
    slope: f64,
    start: f64,
    end: f64,
    /// The target's value at the pivot before normalization.
    offset: f64,
}

impl SectionTarget {
    fn db(&self, f: f64) -> f64 {
        sharp_db(self.slope, self.start, self.end, f) - self.offset
    }
}

/// Effective stream-rate band. Keep at least 1/6 octave after clamping.
/// This runs even for an enabled zero-slope tilt, so invalid settings cannot
/// become valid merely because the current gain happens to be zero.
pub fn effective_band(cfg: &TiltConfig, sample_rate: f64) -> Result<(f64, f64), String> {
    if !sample_rate.is_finite() || sample_rate < 100.0 {
        return Err("tilt: invalid stream sample rate".into());
    }
    let start = cfg.start_hz;
    let end = cfg.end_hz.min(0.45 * sample_rate);
    if !(TILT_MIN_HZ..=TILT_MAX_HZ).contains(&start)
        || !(TILT_MIN_HZ..=TILT_MAX_HZ).contains(&cfg.end_hz)
    {
        return Err(format!(
            "tilt: start_hz and end_hz must be within {TILT_MIN_HZ:.0}–{TILT_MAX_HZ:.0} Hz (got {start:.3}–{:.3} Hz)",
            cfg.end_hz
        ));
    }
    if cfg.end_hz <= start || end <= start || (end / start).log2() < 1.0 / 6.0 {
        return Err(format!(
            "tilt: effective band {start:.3}–{end:.3} Hz at {sample_rate:.0} Hz must span at least 1/6 octave after clamping end_hz to 0.45 * fs"
        ));
    }
    // A pivot above the effective (clamped) end, even above Nyquist, is
    // valid: the target is flat there, so it acts as the `end` pivot.
    if let TiltPivot::Hz(hz) = cfg.pivot
        && (!hz.is_finite() || hz < start || hz > cfg.end_hz)
    {
        return Err("tilt: Hz pivot must be inside the requested band".into());
    }
    Ok((start, end))
}

/// The ideal target before pivot normalization: 0 dB below `start`, slope
/// `slope` dB/oct up to `end`, flat above.
pub fn sharp_db(slope: f64, start: f64, end: f64, f: f64) -> f64 {
    if f <= start {
        return 0.0;
    }
    slope * (f / start).log2().min((end / start).log2())
}

/// Cusp remainder of a unit-slope corner at `fc`: `k(log2(f/fc))`, with
/// `x_+ = softplus-shelf(x) + k(x)`. Always <= 0, -0.5 at the corner.
fn cusp(f: f64, fc: f64) -> f64 {
    if f <= 0.0 {
        return 0.0;
    }
    -0.5 * (1.0 + 4f64.powf(-(f / fc).log2().abs())).log2()
}

/// Smooth ramp `slope/2 * log2((1 + (f/a)^2) / (1 + (f/b)^2))`: the sharp
/// target minus both cusps. Its low/high asymptotes are 0 and the total.
fn prototype_db(slope: f64, start: f64, end: f64, f: f64) -> f64 {
    slope / 2.0 * ((1.0 + (f / start).powi(2)) / (1.0 + (f / end).powi(2))).log2()
}

/// First-order shelf cascade for the smooth ramp: three sections per octave
/// from `start/4` to Nyquist (guards outside the band let the fit correct
/// bilinear warping), gains refined by damped least squares, the last gain
/// dependent so the Nyquist gain is exactly `total`.
fn smooth_cascade(
    slope: f64,
    start: f64,
    end: f64,
    total: f64,
    fs: f64,
    pivot_hz: Option<f64>,
) -> Result<Vec<ShelfCoeffs>, String> {
    let low = start / 4.0;
    let high = 0.4999 * fs;
    let n = ((high / low).log2() * 3.0).ceil() as usize + 1;
    let corners: Vec<f64> = (0..n)
        .map(|i| low * (high / low).powf(i as f64 / (n - 1) as f64))
        .collect();
    let mut gains = vec![0.0; n];
    let in_band = corners.iter().filter(|&&f| f >= start && f <= end).count();
    // Very narrow bands may contain no grid point: the dependent final
    // section still supplies a valid starting point for the fit.
    if in_band > 0 {
        for (g, &f) in gains.iter_mut().zip(&corners) {
            if f >= start && f <= end {
                *g = total / in_band as f64;
            }
        }
    }
    gains[n - 1] = total - gains[..n - 1].iter().sum::<f64>();

    let fit_low = 5.0_f64.min(start / 8.0);
    let mut freqs: Vec<f64> = (0..512)
        .map(|i| fit_low * (0.45 * fs / fit_low).powf(i as f64 / 511.0))
        .collect();
    // A linear grid keeps the short high-frequency interval from being
    // underweighted.
    freqs.extend((0..128).map(|i| fs * (0.35 + 0.10 * i as f64 / 127.0)));
    // Only pivots the shelves are fitted for: above 0.45 fs nothing is.
    freqs.extend(pivot_hz.filter(|&p| p <= 0.45 * fs));
    let targets: Vec<f64> = freqs
        .iter()
        .map(|&f| prototype_db(slope, start, end, f))
        .collect();
    let ratios: Vec<Vec<f64>> = freqs
        .iter()
        .map(|&f| {
            let kf = (PI * f / fs).tan();
            corners
                .iter()
                .map(|&c| (kf / (PI * c / fs).tan()).powi(2))
                .collect()
        })
        .collect();
    refine(&mut gains, &ratios, &targets, total)?;
    Ok(corners
        .iter()
        .zip(&gains)
        .map(|(&fc, &g)| ShelfCoeffs::high_shelf(fc, g, fs))
        .collect())
}

// ------------------------------------------------------------ corners

/// Second-order sections per corner.
const CORNER_SECTIONS: usize = 4;
/// Bounds of every corner section's Q (zeros and poles). The upper bound
/// limits ringing; the lower one keeps the parameterization well scaled.
const CORNER_Q_MIN: f64 = 0.05;
const CORNER_Q_MAX: f64 = 10.0;
/// Each corner section's centre stays within this many octaves of the corner.
const CORNER_CENTRE_RANGE_OCT: f64 = 3.0;
/// Starting points: minimax fits of `|s| * k(x)` in the analog domain, with
/// every section centred on the corner. Columns: `|s|` (dB/oct), the four
/// zero Qs, the four pole Qs.
const CORNER_TABLE: [[f64; 9]; 14] = [
    [
        0.25, 0.741227, 6.223450, 6.223461, 8.0, 0.733803, 5.674535, 7.373633, 7.373687,
    ],
    [
        0.50, 0.741701, 5.854265, 5.854382, 8.0, 0.727006, 5.202560, 7.228223, 7.228236,
    ],
    [
        0.75, 0.730709, 5.590467, 5.590492, 8.0, 0.709525, 4.862626, 7.122112, 7.122132,
    ],
    [
        1.0, 0.752736, 5.529239, 5.529244, 8.0, 0.723052, 4.781937, 7.090896, 7.090907,
    ],
    [
        1.5, 0.760513, 5.247899, 5.247920, 8.0, 0.716191, 4.427031, 6.963861, 6.963861,
    ],
    [
        2.0, 0.754759, 4.984410, 4.984425, 8.0, 0.697815, 4.097007, 6.839945, 6.839947,
    ],
    [
        2.5, 0.749709, 4.799396, 4.799465, 8.0, 0.680687, 3.860960, 6.748827, 6.748837,
    ],
    [
        3.0, 0.748397, 4.644692, 4.644704, 8.0, 0.667465, 3.663591, 6.668775, 6.668776,
    ],
    [
        3.5, 0.747396, 4.510897, 4.510900, 8.0, 0.655047, 3.492061, 6.596740, 6.596746,
    ],
    [
        4.0, 0.746140, 4.394745, 4.394747, 8.0, 0.642986, 3.339995, 6.533079, 6.533082,
    ],
    [
        4.5, 0.745094, 4.290088, 4.290103, 8.0, 0.631525, 3.203380, 6.472927, 6.472928,
    ],
    [
        5.0, 0.745998, 4.194003, 4.194018, 8.0, 0.622091, 3.078061, 6.415649, 6.415654,
    ],
    [
        5.5, 0.747323, 4.106600, 4.106602, 8.0, 0.613287, 2.964400, 6.360982, 6.360985,
    ],
    [
        6.0, 0.748468, 4.026817, 4.026833, 8.0, 0.604592, 2.860756, 6.308791, 6.308792,
    ],
];
const CORNER_PARAMS: usize = 3 * CORNER_SECTIONS;
const DB_PER_LN: f64 = 10.0 / std::f64::consts::LN_10;

/// One corner, as unconstrained fit parameters per section
/// `[phi, theta_z, theta_p]`: centre `CORNER_CENTRE_RANGE_OCT * tanh(phi)`
/// octaves from `fc`, `ln Q` a logistic of theta between the Q bounds.
struct Corner {
    fc: f64,
    /// Realize the inverse (zeros and poles swapped).
    invert: bool,
    p: [f64; CORNER_PARAMS],
}

fn logistic(t: f64) -> f64 {
    1.0 / (1.0 + (-t).exp())
}

fn ln_q(theta: f64) -> f64 {
    let (lo, hi) = (CORNER_Q_MIN.ln(), CORNER_Q_MAX.ln());
    lo + (hi - lo) * logistic(theta)
}

fn theta_of(q: f64) -> f64 {
    let (lo, hi) = (CORNER_Q_MIN.ln(), CORNER_Q_MAX.ln());
    let u = ((q.ln() - lo) / (hi - lo)).clamp(1e-6, 1.0 - 1e-6);
    (u / (1.0 - u)).ln()
}

/// Table start point for `|s|`, linear in Q between rows; below the first
/// row, the zero/pole log-Q split shrinks in proportion to `|s|`.
fn corner_start(abs_slope: f64, stretch: f64) -> [f64; CORNER_PARAMS] {
    let row = |i: usize| -> [f64; 8] {
        let mut r = [0.0; 8];
        r.copy_from_slice(&CORNER_TABLE[i][1..]);
        r
    };
    let mut q = [0.0; 8];
    let first = CORNER_TABLE[0][0];
    if abs_slope <= first {
        let r = row(0);
        let scale = abs_slope / first;
        for j in 0..CORNER_SECTIONS {
            let (lz, lp) = (r[j].ln(), r[CORNER_SECTIONS + j].ln());
            let (mid, half) = (0.5 * (lz + lp), 0.5 * (lz - lp) * scale);
            q[j] = (mid + half).exp();
            q[CORNER_SECTIONS + j] = (mid - half).exp();
        }
    } else {
        let last = CORNER_TABLE.len() - 1;
        let i = (0..last)
            .find(|&i| abs_slope <= CORNER_TABLE[i + 1][0])
            .unwrap_or(last - 1);
        let (s0, s1) = (CORNER_TABLE[i][0], CORNER_TABLE[i + 1][0]);
        let t = ((abs_slope - s0) / (s1 - s0)).clamp(0.0, 1.0);
        let (r0, r1) = (row(i), row(i + 1));
        for k in 0..8 {
            q[k] = (1.0 - t) * r0[k] + t * r1[k];
        }
    }
    let mut p = [0.0; CORNER_PARAMS];
    for j in 0..CORNER_SECTIONS {
        p[3 * j + 1] = theta_of(q[j] / stretch);
        p[3 * j + 2] = theta_of(q[CORNER_SECTIONS + j] / stretch);
    }
    p
}

/// Per-section constants of the corner model, hoisted out of the grid loop.
#[derive(Clone, Copy)]
struct SectionTerms {
    w2: f64,
    iqz2: f64,
    iqp2: f64,
    /// Chain-rule factors d(log2 w)/dphi, d(ln Qz)/dtheta_z, d(ln Qp)/dtheta_p.
    dc: f64,
    dz: f64,
    dp: f64,
}

fn section_terms(p: &[f64; CORNER_PARAMS]) -> [SectionTerms; CORNER_SECTIONS] {
    let (lo, hi) = (CORNER_Q_MIN.ln(), CORNER_Q_MAX.ln());
    std::array::from_fn(|j| {
        let (phi, tz, tp) = (p[3 * j], p[3 * j + 1], p[3 * j + 2]);
        let th = phi.tanh();
        let (sz, sp) = (logistic(tz), logistic(tp));
        SectionTerms {
            w2: (2.0 * CORNER_CENTRE_RANGE_OCT * th).exp2(),
            iqz2: (-2.0 * (lo + (hi - lo) * sz)).exp(),
            iqp2: (-2.0 * (lo + (hi - lo) * sp)).exp(),
            dc: CORNER_CENTRE_RANGE_OCT * (1.0 - th * th),
            dz: (hi - lo) * sz * (1.0 - sz),
            dp: (hi - lo) * sp * (1.0 - sp),
        }
    })
}

/// dB of the (non-inverted) corner model at normalized `o = Omega/Omega_c`,
/// with the gradient over the parameters when `grad` is given.
fn corner_model(
    terms: &[SectionTerms; CORNER_SECTIONS],
    o: f64,
    grad: Option<&mut [f64; CORNER_PARAMS]>,
) -> f64 {
    let o2 = o * o;
    let mut db = 0.0;
    match grad {
        None => {
            for t in terms {
                let d = t.w2 - o2;
                let n2 = d * d + o2 * t.w2 * t.iqz2;
                let m2 = d * d + o2 * t.w2 * t.iqp2;
                db += (n2 / m2).ln();
            }
        }
        Some(g) => {
            for (j, t) in terms.iter().enumerate() {
                let d = t.w2 - o2;
                let (ez, ep) = (o2 * t.w2 * t.iqz2, o2 * t.w2 * t.iqp2);
                let n2 = d * d + ez;
                let m2 = d * d + ep;
                db += (n2 / m2).ln();
                // d/d(log2 w) = ln2 * w d/dw.
                let dn = std::f64::consts::LN_2 * (4.0 * t.w2 * d + 2.0 * ez);
                let dm = std::f64::consts::LN_2 * (4.0 * t.w2 * d + 2.0 * ep);
                g[3 * j] = DB_PER_LN * (dn / n2 - dm / m2) * t.dc;
                g[3 * j + 1] = DB_PER_LN * (-2.0 * ez / n2) * t.dz;
                g[3 * j + 2] = DB_PER_LN * (2.0 * ep / m2) * t.dp;
            }
        }
    }
    DB_PER_LN * db
}

impl Corner {
    /// Analog section parameters `(w, Qz, Qp)` in prewarped units
    /// (`Omega = tan(pi f / fs)`), after inversion.
    fn sections(&self, fs: f64) -> impl Iterator<Item = (f64, f64, f64)> + '_ {
        let oc = (PI * self.fc / fs).tan();
        (0..CORNER_SECTIONS).map(move |j| {
            let w = oc * (2f64).powf(CORNER_CENTRE_RANGE_OCT * self.p[3 * j].tanh());
            let (qz, qp) = (ln_q(self.p[3 * j + 1]).exp(), ln_q(self.p[3 * j + 2]).exp());
            if self.invert {
                (w, qp, qz)
            } else {
                (w, qz, qp)
            }
        })
    }

    fn db(&self, f: f64, fs: f64) -> f64 {
        let o = (PI * f / fs).tan() / (PI * self.fc / fs).tan();
        let m = corner_model(&section_terms(&self.p), o, None);
        if self.invert { -m } else { m }
    }

    fn coeffs(&self, fs: f64) -> Vec<Coeffs> {
        self.sections(fs)
            .map(|(w, qz, qp)| {
                let (w2, a0) = (w * w, 1.0 + w / qp + w * w);
                Coeffs {
                    b0: (1.0 + w / qz + w2) / a0,
                    b1: 2.0 * (w2 - 1.0) / a0,
                    b2: (1.0 - w / qz + w2) / a0,
                    a1: 2.0 * (w2 - 1.0) / a0,
                    a2: (1.0 - w / qp + w2) / a0,
                }
            })
            .collect()
    }

    fn centres_hz(&self, fs: f64) -> Vec<f64> {
        self.sections(fs)
            .map(|(w, _, _)| fs / PI * w.atan())
            .collect()
    }
}

/// Fit one corner at `fc` so that its dB response follows `target` (the
/// remainder the corner has to supply, about `sign * |s| * k(x)`), on
/// physical frequencies from 8 octaves below `fc` (or 10 Hz, if lower) to 8
/// octaves above, capped at `0.45 fs`.
/// Deterministic damped Gauss-Newton (Levenberg-Marquardt) from the table
/// start, inside a few Lawson reweighting rounds that move the least-squares
/// fit toward minimax. Fails rather than returning a nonfinite corner.
fn fit_corner(
    fc: f64,
    fs: f64,
    abs_slope: f64,
    sign: f64,
    target: impl Fn(f64) -> f64,
) -> Result<Corner, String> {
    let oc = (PI * fc / fs).tan();
    // 12 points/octave from 8 octaves below the corner (or 10 Hz, whichever
    // is lower: broad sections must not drift inside the measured band) to
    // 8 octaves above, plus a dense patch at the corner.
    let x_low = (-8.0_f64).min((10.0 / fc).log2()).floor();
    let n_coarse = ((8.0 - x_low) * 12.0) as usize;
    let mut pts: Vec<(f64, f64)> = Vec::with_capacity(n_coarse + 75);
    let xs = (0..=n_coarse)
        .map(|i| x_low + (8.0 - x_low) * i as f64 / n_coarse as f64)
        .chain((0..=40).map(|i| -0.2 + 0.4 * i as f64 / 40.0))
        // A linear patch up to 0.45 fs: the top of the measured band is short
        // in octaves but long in Hz.
        .chain((0..=32).map(|i| (fs * (0.3 + 0.15 * i as f64 / 32.0) / fc).log2()));
    for x in xs {
        let f = fc * x.exp2();
        if x.abs() <= 8.0 + 1e-9 && x >= x_low && f <= 0.45 * fs * (1.0 + 1e-12) {
            // The model approximates the non-inverted cusp; fold the sign.
            pts.push(((PI * f / fs).tan() / oc, sign * target(f)));
        }
    }
    if pts.len() < CORNER_PARAMS * 2 || pts.iter().any(|(o, t)| !o.is_finite() || !t.is_finite()) {
        return Err(format!("tilt: corner fit grid at {fc:.3} Hz is degenerate"));
    }
    // Near Nyquist the bilinear map stretches log frequency around the
    // corner by `2 theta / sin(2 theta)`: widen the start sections to match.
    let theta = PI * fc / fs;
    let stretch = 2.0 * theta / (2.0 * theta).sin();
    let mut p = corner_start(abs_slope, stretch);
    // The corner frequency itself carries extra weight so the band edges,
    // and with them the delivered total, land on the target.
    let mut weights: Vec<f64> = pts
        .iter()
        .map(|&(o, _)| if (o - 1.0).abs() < 1e-12 { 50.0 } else { 1.0 })
        .collect();
    let mut residuals = vec![0.0; pts.len()];
    let cost = |p: &[f64; CORNER_PARAMS], w: &[f64]| -> f64 {
        let terms = section_terms(p);
        pts.iter()
            .zip(w)
            .map(|(&(o, t), &w)| w * (t - corner_model(&terms, o, None)).powi(2))
            .sum()
    };
    const ROUNDS: usize = 6;
    const STEPS: usize = 80;
    let goal = 0.005 * abs_slope + 0.004;
    for round in 0..ROUNDS {
        let mut damping = 1e-3_f64;
        let mut current = cost(&p, &weights);
        for _ in 0..STEPS {
            let mut ata = vec![0.0; CORNER_PARAMS * CORNER_PARAMS];
            let mut atr = vec![0.0; CORNER_PARAMS];
            let mut jac = [0.0; CORNER_PARAMS];
            let terms = section_terms(&p);
            for (&(o, t), &w) in pts.iter().zip(&weights) {
                let r = t - corner_model(&terms, o, Some(&mut jac));
                for a in 0..CORNER_PARAMS {
                    atr[a] += w * jac[a] * r;
                    for b in a..CORNER_PARAMS {
                        ata[a * CORNER_PARAMS + b] += w * jac[a] * jac[b];
                    }
                }
            }
            let mut accepted = false;
            let mut stalled = false;
            while damping < 1e9 {
                let mut m = ata.clone();
                let mut rhs = atr.clone();
                for a in 0..CORNER_PARAMS {
                    for b in 0..a {
                        m[a * CORNER_PARAMS + b] = m[b * CORNER_PARAMS + a];
                    }
                    m[a * CORNER_PARAMS + a] += damping * (m[a * CORNER_PARAMS + a] + 1e-9);
                }
                if let Some(delta) = solve_linear(&mut m, &mut rhs, CORNER_PARAMS) {
                    let mut cand = p;
                    for (c, d) in cand.iter_mut().zip(&delta) {
                        *c += d;
                    }
                    let c = cost(&cand, &weights);
                    if c.is_finite() && c < current {
                        p = cand;
                        stalled = c > current * (1.0 - 1e-3);
                        current = c;
                        damping = (damping / 3.0).max(1e-9);
                        accepted = true;
                        break;
                    }
                }
                damping *= 10.0;
            }
            if !accepted || stalled || current < 1e-14 {
                break;
            }
        }
        let mut max_err = 0.0_f64;
        let terms = section_terms(&p);
        for (r, &(o, t)) in residuals.iter_mut().zip(&pts) {
            *r = (t - corner_model(&terms, o, None)).abs();
            max_err = max_err.max(*r);
        }
        // Good enough (the analog minimax is far below the 0.1 dB contract):
        // stop. Otherwise a Lawson step weights up where the error is large.
        if round + 1 == ROUNDS || max_err <= goal || !max_err.is_finite() {
            break;
        }
        let mut sum = 0.0;
        for (w, r) in weights.iter_mut().zip(&residuals) {
            *w *= 1.0 + 1.5 * (r / max_err).powi(2);
            sum += *w;
        }
        let mean = sum / weights.len() as f64;
        for w in &mut weights {
            *w /= mean;
        }
    }
    if p.iter().any(|v| !v.is_finite()) {
        return Err(format!("tilt: corner fit at {fc:.3} Hz diverged"));
    }
    Ok(Corner {
        fc,
        invert: sign < 0.0,
        p,
    })
}

// Last gain is dependent: sum(gains) == total, giving an exact high plateau.
// DC is exactly unity for every shelf, so there is no broadband fit parameter.
fn fit_row(lins: &[f64], ratios: &[f64], jacobian: &mut [f64]) -> f64 {
    let mut ln_sum = 0.0;
    let mut product = 1.0;
    let mut last = 0.0;
    for (i, (&lin, &u2)) in lins.iter().zip(ratios).enumerate() {
        let num = 1.0 + lin * u2;
        let den = 1.0 + u2 / lin;
        // Each ratio is within the +/-72 dB section bound; take the log
        // every few factors so the running product cannot overflow.
        product *= num / den;
        if i % 8 == 7 {
            ln_sum += product.ln();
            product = 1.0;
        }
        let derivative = 0.5 * (lin * u2 / num + u2 / (lin + u2));
        if i < jacobian.len() {
            jacobian[i] = derivative;
        } else {
            last = derivative;
        }
    }
    for j in jacobian {
        *j -= last;
    }
    DB_PER_LN * (ln_sum + product.ln())
}

fn lins_of(gains: &[f64]) -> Vec<f64> {
    gains.iter().map(|&g| crate::db_to_lin(g)).collect()
}

fn refine(
    gains: &mut Vec<f64>,
    ratios: &[Vec<f64>],
    targets: &[f64],
    total: f64,
) -> Result<(), String> {
    let m = gains.len() - 1;
    let mut damping = 1e-3_f64;
    let mut jac = vec![0.0; m];
    // Fixed iteration order and bounds: no random initialization or line search.
    for _ in 0..64 {
        let mut ata = vec![0.0; m * m];
        let mut atr = vec![0.0; m];
        let mut cost = 0.0;
        let mut max_error = 0.0_f64;
        let lins = lins_of(gains);
        for (row, &target) in ratios.iter().zip(targets) {
            let residual = target - fit_row(&lins, row, &mut jac);
            cost += residual * residual;
            max_error = max_error.max(residual.abs());
            for a in 0..m {
                atr[a] += jac[a] * residual;
                for b in a..m {
                    ata[a * m + b] += jac[a] * jac[b];
                }
            }
        }
        if max_error <= 0.015 {
            return Ok(());
        }
        for a in 0..m {
            for b in 0..a {
                ata[a * m + b] = ata[b * m + a];
            }
            ata[a * m + a] += damping;
        }
        let delta = solve_linear(&mut ata, &mut atr, m)
            .ok_or("tilt: damped refinement matrix is singular")?;
        let mut candidate = gains.clone();
        for (g, &d) in candidate[..m].iter_mut().zip(&delta) {
            *g += d;
        }
        candidate[m] = total - candidate[..m].iter().sum::<f64>();
        let bounded = candidate.iter().all(|g| g.is_finite() && g.abs() <= 36.0);
        let new_cost: f64 = if bounded {
            let lins = lins_of(&candidate);
            ratios
                .iter()
                .zip(targets)
                .map(|(row, &target)| (target - fit_row(&lins, row, &mut jac)).powi(2))
                .sum()
        } else {
            f64::INFINITY
        };
        if new_cost.is_finite() && new_cost < cost {
            *gains = candidate;
            damping = (damping / 3.0).max(1e-9);
        } else {
            damping *= 10.0;
        }
    }
    Err("tilt: damped refinement did not converge within 64 steps".into())
}

/// Solve `A x = b` in place via Gaussian elimination with partial pivoting.
fn solve_linear(a: &mut [f64], b: &mut [f64], n: usize) -> Option<Vec<f64>> {
    for col in 0..n {
        let mut pivot_row = col;
        let mut pivot_val = a[col * n + col].abs();
        for row in (col + 1)..n {
            let v = a[row * n + col].abs();
            if v > pivot_val {
                pivot_val = v;
                pivot_row = row;
            }
        }
        if !pivot_val.is_finite() || pivot_val < 1e-15 {
            return None;
        }
        if pivot_row != col {
            for k in col..n {
                a.swap(col * n + k, pivot_row * n + k);
            }
            b.swap(col, pivot_row);
        }
        let diag = a[col * n + col];
        for row in (col + 1)..n {
            let factor = a[row * n + col] / diag;
            if factor == 0.0 {
                continue;
            }
            for k in col..n {
                a[row * n + k] -= factor * a[col * n + k];
            }
            b[row] -= factor * b[col];
        }
    }
    let mut x = vec![0.0_f64; n];
    for row in (0..n).rev() {
        let mut sum = b[row];
        for k in (row + 1)..n {
            sum -= a[row * n + k] * x[k];
        }
        x[row] = sum / a[row * n + row];
        if !x[row].is_finite() {
            return None;
        }
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfft::FftPlanner;
    use std::time::Instant;

    fn to_db(c: Complex64) -> f64 {
        20.0 * c.norm().max(1e-300).log10()
    }

    /// Independent sharp target from the config alone: flat 0 dB below the
    /// start, the configured slope to the (clamped) end, flat above, shifted
    /// so the pivot is 0 dB.
    fn target_db(cfg: &TiltConfig, fs: f64, f: f64) -> f64 {
        let a = cfg.start_hz;
        let b = cfg.end_hz.min(0.45 * fs);
        let s = cfg.effective_slope_db_per_oct().unwrap();
        let raw = |f: f64| {
            let oct = if f <= a { 0.0 } else { (f / a).log2() };
            s * oct.min((b / a).log2())
        };
        let offset = match cfg.pivot {
            TiltPivot::Start => 0.0,
            TiltPivot::End => raw(b),
            TiltPivot::Center => raw((a * b).sqrt()),
            TiltPivot::Hz(hz) => raw(hz),
        };
        raw(f) - offset
    }

    fn config(slope: f64, start: f64, end: f64, pivot: TiltPivot) -> TiltConfig {
        TiltConfig {
            enabled: true,
            slope_db_per_oct: Some(slope),
            total_db: None,
            start_hz: start,
            end_hz: end,
            pivot,
        }
    }

    struct Measured {
        outside: f64,
        inside: f64,
        delivered: f64,
        requested: f64,
        sections: usize,
        ms: f64,
    }

    fn assert_stable_min_phase(t: &TiltFilter) {
        for c in &t.shelves {
            assert!(c.a1.abs() < 1.0, "unstable pole: {c:?}");
            assert!((c.b1 / c.b0).abs() < 1.0, "non-minimum-phase zero: {c:?}");
        }
        // Second-order: both roots of 1 + c1 z^-1 + c2 z^-2 inside the unit
        // circle iff |c2| < 1 and |c1| < 1 + c2.
        for c in &t.corners {
            assert!(
                c.a2.abs() < 1.0 && c.a1.abs() < 1.0 + c.a2,
                "unstable: {c:?}"
            );
            let (n1, n2) = (c.b1 / c.b0, c.b2 / c.b0);
            assert!(
                n2.abs() < 1.0 && n1.abs() < 1.0 + n2,
                "non-min-phase: {c:?}"
            );
        }
    }

    fn measure(cfg: &TiltConfig, fs: f64) -> Measured {
        let t0 = Instant::now();
        let tilt = TiltFilter::design(cfg, fs).unwrap_or_else(|e| panic!("{cfg:?} at {fs}: {e}"));
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        assert_stable_min_phase(&tilt);
        let a = cfg.start_hz;
        let b = cfg.end_hz.min(0.45 * fs);
        let fmax = 20_000.0_f64.min(0.45 * fs);
        let (mut outside, mut inside) = (0.0_f64, 0.0_f64);
        // Denser than the designer's own validation, log and linear grids.
        for i in 0..=8192 {
            for f in [
                20.0 * (fmax / 20.0).powf(i as f64 / 8192.0),
                20.0 + (fmax - 20.0) * i as f64 / 8192.0,
            ] {
                let e = (to_db(tilt.response(f)) - target_db(cfg, fs, f)).abs();
                if (f / a).log2().abs() < 1.0 / 6.0 || (f / b).log2().abs() < 1.0 / 6.0 {
                    inside = inside.max(e);
                } else {
                    outside = outside.max(e);
                }
            }
        }
        let delivered = to_db(tilt.response(b)) - to_db(tilt.response(a));
        let requested = cfg.effective_slope_db_per_oct().unwrap() * (b / a).log2();
        let s = cfg.effective_slope_db_per_oct().unwrap().abs();
        assert!(
            outside <= MAX_ERROR_DB,
            "{cfg:?} at {fs}: outside {outside:.4}"
        );
        assert!(
            inside <= MAX_ERROR_DB + CORNER_ALLOWANCE_PER_SLOPE * s,
            "{cfg:?} at {fs}: corner {inside:.4}"
        );
        assert!(
            (delivered - requested).abs() <= MAX_TOTAL_ERROR_DB,
            "{cfg:?} at {fs}: delivered {delivered:.4} of {requested:.4}"
        );
        Measured {
            outside,
            inside,
            delivered,
            requested,
            sections: tilt.section_count(),
            ms,
        }
    }

    #[test]
    fn tilt_accuracy_benchmarks_across_sample_rates() {
        let cases = [
            (
                "-0.7/oct 20-20k pivot 1k",
                config(-0.7, 20.0, 20_000.0, TiltPivot::Hz(1000.0)),
            ),
            (
                "-0.7/oct 20-20k start",
                config(-0.7, 20.0, 20_000.0, TiltPivot::Start),
            ),
            (
                "+1/oct 200-20k start",
                config(1.0, 200.0, 20_000.0, TiltPivot::Start),
            ),
            (
                "-3/oct 2k-10k center",
                config(-3.0, 2000.0, 10_000.0, TiltPivot::Center),
            ),
            (
                "+6/oct 1k-2k start",
                config(6.0, 1000.0, 2000.0, TiltPivot::Start),
            ),
            (
                "+6/oct 1/6 oct 1k",
                config(6.0, 1000.0, 1000.0 * 2f64.powf(1.0 / 6.0), TiltPivot::Start),
            ),
            (
                "+6/oct 10k-20k end",
                config(6.0, 10_000.0, 20_000.0, TiltPivot::End),
            ),
        ];
        eprintln!(
            "| setting | fs | max err outside | max err corners | delivered / requested | sections | ms |"
        );
        for (name, cfg) in cases {
            for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0, 384_000.0] {
                let m = measure(&cfg, fs);
                eprintln!(
                    "| {name} | {fs:.0} | {:.4} | {:.4} | {:+.3} / {:+.3} | {} | {:.2} |",
                    m.outside, m.inside, m.delivered, m.requested, m.sections, m.ms
                );
            }
        }
        // The edges the sharp target promises: bass under a "start" pivot is
        // untouched, and the band edge delivers its full value.
        let t =
            TiltFilter::design(&config(1.0, 200.0, 20_000.0, TiltPivot::Start), 48_000.0).unwrap();
        assert!(to_db(t.response(100.0)).abs() < 0.02);
        assert!(to_db(t.response(200.0)).abs() < 0.05);
    }

    #[test]
    fn extremes_meet_target() {
        for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0, 384_000.0] {
            for total in [-24.0, -0.003, 0.003, 24.0] {
                let cfg = TiltConfig {
                    slope_db_per_oct: None,
                    total_db: Some(total),
                    ..config(0.0, 20.0, 20_000.0, TiltPivot::Hz(20.0))
                };
                measure(&cfg, fs);
            }
            for slope in [-6.0, 6.0] {
                measure(&config(slope, 20.0, 160.0, TiltPivot::Hz(20.0)), fs);
                let top = (0.45 * fs).min(TILT_MAX_HZ);
                let a = top / 2f64.powf(1.0 / 6.0);
                measure(&config(slope, a, top, TiltPivot::End), fs);
                measure(
                    &config(slope, 20.0, 20.0 * 2f64.powf(1.0 / 6.0), TiltPivot::Start),
                    fs,
                );
            }
        }
    }

    #[test]
    fn pivots_are_exact_and_continuous_including_band_edges() {
        for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for slope in [-3.0, -0.3, 0.3, 1.0, 3.0] {
                let cfg = config(slope, 200.0, 20_000.0, TiltPivot::Start);
                for (pivot, anchor) in [
                    (TiltPivot::Start, 0.0),
                    (TiltPivot::End, fs / 2.0),
                    (
                        TiltPivot::Center,
                        (200.0 * 20_000.0_f64.min(0.45 * fs)).sqrt(),
                    ),
                    (TiltPivot::Hz(200.0), 200.0),
                    (TiltPivot::Hz(210.0), 210.0),
                    // Above the clamped end (44.1 kHz) it is the end pivot.
                    (
                        TiltPivot::Hz(20_000.0),
                        if 20_000.0 > 0.45 * fs {
                            fs / 2.0
                        } else {
                            20_000.0
                        },
                    ),
                ] {
                    let t = TiltFilter::design(&TiltConfig { pivot, ..cfg }, fs).unwrap();
                    assert!(to_db(t.response(anchor)).abs() < 1e-10, "{pivot:?} at {fs}");
                }
                let at = |p: f64| {
                    let t = TiltFilter::design(
                        &TiltConfig {
                            pivot: TiltPivot::Hz(p),
                            ..cfg
                        },
                        fs,
                    )
                    .unwrap();
                    to_db(t.response(1000.0))
                };
                assert!((at(219.99) - at(220.01)).abs() < 0.001);
            }
        }
    }

    #[test]
    fn a_fit_that_cannot_meet_the_target_is_an_error() {
        // Equal fixed frequencies with zero total can only produce identity.
        // The incompatible target must exhaust damping and return an error.
        let err = refine(&mut vec![0.0, 0.0], &[vec![1.0, 1.0]], &[1.0], 0.0).unwrap_err();
        assert!(err.contains("did not converge"));
    }

    #[test]
    fn tilt_process_matches_analytic_response() {
        let fs = 48_000.0;
        let tilt = TiltFilter::design(&config(-3.0, 200.0, 2000.0, TiltPivot::Center), fs).unwrap();
        let n = 1 << 16;
        let mut buf = vec![0.0_f64; n];
        buf[0] = 1.0;
        let mut state = tilt.new_state();
        // Odd block sizes, including empty blocks.
        let mut pos = 0;
        for len in [0, 1, 7, 300, 0, 4096].iter().cycle() {
            if pos >= n {
                break;
            }
            let end = (pos + len).min(n);
            tilt.process_channel(&mut buf[pos..end], &mut state);
            pos = end;
        }
        let mut planner = FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let mut spectrum: Vec<Complex64> = buf.iter().map(|&x| Complex64::new(x, 0.0)).collect();
        fft.process(&mut spectrum);
        for bin in [10, 270, 273, 300, 2731, 3000, 6000] {
            let f = bin as f64 * fs / n as f64;
            let exact = tilt.response(f);
            assert!((exact - spectrum[bin]).norm() < 1e-6, "at {f:.1} Hz");
        }
        state.reset();
        assert_eq!(state, tilt.new_state());
    }
}
