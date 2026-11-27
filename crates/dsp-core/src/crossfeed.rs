//! Headphone crossfeed stage (bs2b and Linkwitz-style) with a stereo width
//! control.
//!
//! Stereo only (`channels == 2`), at the source sample rate, placed before the
//! parametric EQ so headphone correction EQ remains last before the
//! transducer. Each output channel is `direct(own) + cross(other)`, both paths
//! first-order IIR sections.
//!
//! - **bs2b modes** implement the published Bauer stereophonic-to-binaural
//!   design (bs2b library, MIT, by Boris Mikhaylov) for a feed level `L` dB and
//!   low-pass cut `Fc`. The direct/cross ratio at DC is exactly `L` dB. bs2b is
//!   not mono-flat (its direct and cross paths have different poles).
//! - **Linkwitz mode** is a complementary pair designed with the prewarped
//!   bilinear transform: `H_D + H_C = 1`, so mono is exactly flat. The corner
//!   (`f_c = 796.4 Hz`) gives a 0.3 ms low-frequency interaural delay.
//! - **`none`**: no crossfeed; the stage runs for its width alone.
//!
//! **Width (`CrossfeedConfig.width`, side gain `k`):** after the crossfeed
//! sums, the side `d = L − R` is scaled by `k` and the mid left alone:
//! `L′ = L + u`, `R′ = R − u` with `u = g·d`, `g = (k − 1)/2`. Written this
//! way, mono (`d = 0`) passes bit-exact at every `k`. The `above_bass` shape
//! takes `u = g·HP₁(d)`, a first-order high-pass at `width_corner_hz`: side
//! gain 1 in the bass, `k` above the corner. Crossfeed and width are both a
//! filter on the mid and one on the side, so they commute.
//!
//! **Level:** one constant `s` scales the output, keeping the direct/cross
//! ratio in place of the bs2b library's own normalisation: `s` makes the
//! A-weighted loudness of the crossfeed part equal to the input's for a
//! stereo pink-noise reference with inter-channel correlation
//! [`MATCHED_CORRELATION`] (the weighting and match of
//! [`crate::loudness::analytic_offset_db`]; `s = 1` without crossfeed). The
//! EQ preamp is not adjusted for crossfeed, so nothing compensates twice;
//! the width is never loudness-compensated (wider is also louder).
//!
//! No cut for safety: worst-case signals can exceed the input's peak by +1.4
//! to +5.6 dB from the crossfeed, more with widening
//! ([`Crossfeed::peak_gain_bound`], exact). The chain is f64 to the
//! quantizer; the limiter and the output headroom (`output.headroom_db`)
//! handle what goes over, as for every stage. All of it is computed at
//! build, never on the audio thread.

use crate::biquad::Coeffs;
use crate::loudness::analytic_offset_db;
use crate::{Complex64, Stage, db_to_lin, lin_to_db};
use audivane_protocol::{
    CROSSFEED_CUTOFF_MAX_HZ, CROSSFEED_CUTOFF_MIN_HZ, CROSSFEED_FEED_MAX_DB, CROSSFEED_FEED_MIN_DB,
    CrossfeedConfig, CrossfeedMode, STEREO_WIDTH_CORNER_MAX_HZ, STEREO_WIDTH_CORNER_MIN_HZ,
    STEREO_WIDTH_MAX, STEREO_WIDTH_MIN, StereoWidthShape,
};
use std::f64::consts::PI;
use std::fmt;

/// Target low-frequency interaural delay of the Linkwitz-style mode (s).
pub const LINKWITZ_DELAY_S: f64 = 0.3e-3;

/// Inter-channel correlation of the stereo reference signal that the
/// level match is defined for: the middle of the 0.3..0.6 range typical
/// of mixed music; the `matched` gain moves between uncorrelated (0), typical
/// and mono (1) material.
pub const MATCHED_CORRELATION: f64 = 0.5;

/// Why a [`Crossfeed`] stage could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrossfeedError {
    /// Crossfeed is defined for stereo only.
    RequiresStereo { channels: usize },
    /// Sample rate must be positive and finite.
    InvalidSampleRate,
    /// Custom parameters out of range.
    InvalidParams(String),
}

impl fmt::Display for CrossfeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequiresStereo { channels } => {
                write!(f, "crossfeed requires stereo (2 channels), got {channels}")
            }
            Self::InvalidSampleRate => write!(f, "crossfeed requires a positive sample rate"),
            Self::InvalidParams(msg) => write!(f, "crossfeed: {msg}"),
        }
    }
}

impl std::error::Error for CrossfeedError {}

/// Denormal flush threshold for the filter states (`2^-100` ≈ −602 dBFS, as
/// in `dc_block`).
const FLUSH_BELOW: f64 = 7.888_609_052_210_118e-31;

/// Single-pole TDF-II filter state (`b2 == 0, a2 == 0`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct OnePoleState {
    z1: f64,
}

impl OnePoleState {
    #[inline]
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y;
        // A decaying tail would otherwise settle in IEEE 754 subnormals
        // (slow on x86) and stay there; same threshold as the DC block.
        if self.z1.abs() < FLUSH_BELOW {
            self.z1 = 0.0;
        }
        y
    }
}

/// First-order section `(b0 + b1 z^-1) / (1 + a1 z^-1)`.
fn first_order(b0: f64, b1: f64, a1: f64) -> Coeffs {
    Coeffs {
        b0,
        b1,
        b2: 0.0,
        a1,
        a2: 0.0,
    }
}

/// Exact `‖h‖₁` of a stable first-order section: `h[0] = b0`,
/// `h[n] = (b1 + b0 p) p^(n-1)` with `p = -a1`.
fn l1_norm(c: &Coeffs) -> f64 {
    let p = -c.a1;
    c.b0.abs() + (c.b1 + c.b0 * p).abs() / (1.0 - p.abs())
}

fn scaled(c: &Coeffs, s: f64) -> Coeffs {
    first_order(c.b0 * s, c.b1 * s, c.a1)
}

/// Power of one output channel with direct response `d` and cross response
/// `c`, for equal-power stereo input with inter-channel correlation `rho`.
fn stereo_power(d: Complex64, c: Complex64, rho: f64) -> f64 {
    d.norm_sqr() + c.norm_sqr() + 2.0 * rho * (d * c.conj()).re
}

/// Linear gain that gives `direct`/`cross` the same A-weighted loudness as
/// their input for stereo pink noise with correlation `rho`.
fn matched_gain(direct: &Coeffs, cross: &Coeffs, fs: f64, rho: f64) -> f64 {
    let out = |f: f64| stereo_power(direct.response(f, fs), cross.response(f, fs), rho);
    db_to_lin(analytic_offset_db(&|_| 1.0, &out, fs))
}

/// Unscaled bs2b design: `(direct, cross, fc_hi)`.
pub(crate) fn bs2b_design(cutoff_hz: f64, feed_db: f64, fs: f64) -> (Coeffs, Coeffs, f64) {
    let gb_lo = feed_db * -5.0 / 6.0 - 3.0;
    let gb_hi = feed_db / 6.0 - 3.0;
    let g_lo = 10f64.powf(gb_lo / 20.0);
    let g_hi = 1.0 - 10f64.powf(gb_hi / 20.0);
    let fc_hi = cutoff_hz * 2f64.powf((gb_lo - 20.0 * g_hi.log10()) / 12.0);

    let x = (-2.0 * PI * cutoff_hz / fs).exp();
    let cross = first_order(g_lo * (1.0 - x), 0.0, -x);
    let xh = (-2.0 * PI * fc_hi / fs).exp();
    let direct = first_order(1.0 - g_hi * (1.0 - xh), -xh, -xh);
    (direct, cross, fc_hi)
}

/// Complementary (mono-flat) bilinear design used by the Linkwitz mode.
fn complementary_design(cutoff_hz: f64, feed_db: f64, fs: f64) -> (Coeffs, Coeffs) {
    let fc = cutoff_hz.min(0.49 * fs);
    let r = 10f64.powf(feed_db / 20.0);
    let g_c = 1.0 / (1.0 + r);
    let g_d = r / (1.0 + r);
    let k = (PI * fc / fs).tan();
    let inv = 1.0 / (k + 1.0);
    let a1 = (k - 1.0) * inv;
    let cross = first_order(g_c * k * inv, g_c * k * inv, a1);
    let direct = first_order((g_d * k + 1.0) * inv, (g_d * k - 1.0) * inv, a1);
    (direct, cross)
}

/// First-order high-pass at `fc_hz` (prewarped bilinear), the `above_bass`
/// width shape's filter on the side.
fn high_pass(fc_hz: f64, fs: f64) -> Coeffs {
    let k = (PI * fc_hz.min(0.49 * fs) / fs).tan();
    let inv = 1.0 / (k + 1.0);
    first_order(inv, -inv, (k - 1.0) * inv)
}

/// The stage's arithmetic: crossfeed paths, the width step after them, and
/// their filter states.
#[derive(Clone, Debug)]
struct Kernel {
    /// Scaled by `scale` (applied to both paths); used only with crossfeed.
    direct: Coeffs,
    cross: Coeffs,
    /// False for `none`: the output is the input times `scale`.
    crossfeed: bool,
    scale: f64,
    /// `(k − 1)/2`; 0 leaves the side alone (no width step at all).
    width_g: f64,
    /// `Some(HP₁)` for the `above_bass` shape.
    side_hp: Option<Coeffs>,
    direct_l: OnePoleState,
    direct_r: OnePoleState,
    cross_l: OnePoleState,
    cross_r: OnePoleState,
    side: OnePoleState,
}

impl Kernel {
    #[inline]
    fn frame(&mut self, xl: f64, xr: f64) -> (f64, f64) {
        let (mut yl, mut yr) = if self.crossfeed {
            let dl = self.direct_l.step(&self.direct, xl);
            let cl = self.cross_l.step(&self.cross, xr);
            let dr = self.direct_r.step(&self.direct, xr);
            let cr = self.cross_r.step(&self.cross, xl);
            (dl + cl, dr + cr)
        } else {
            (xl * self.scale, xr * self.scale)
        };
        if self.width_g != 0.0 {
            // Exactly 0 for mono (yl == yr), so mono is unchanged.
            let d = yl - yr;
            let d = match &self.side_hp {
                Some(hp) => self.side.step(hp, d),
                None => d,
            };
            let u = self.width_g * d;
            yl += u;
            yr -= u;
        }
        (yl, yr)
    }

    fn reset(&mut self) {
        self.direct_l = OnePoleState::default();
        self.direct_r = OnePoleState::default();
        self.cross_l = OnePoleState::default();
        self.cross_r = OnePoleState::default();
        self.side = OnePoleState::default();
    }

    /// Direct and cross responses of the whole stage at `f_hz`: with the
    /// crossfeed's `d`, `c` and the width's `w = g·H(f)·(d − c)` (`H` = 1 or
    /// the high-pass), `D′ = d + w` and `C′ = c − w`.
    fn paths(&self, f_hz: f64, fs: f64) -> (Complex64, Complex64) {
        let (d, c) = if self.crossfeed {
            (
                self.direct.response(f_hz, fs),
                self.cross.response(f_hz, fs),
            )
        } else {
            (Complex64::new(self.scale, 0.0), Complex64::new(0.0, 0.0))
        };
        if self.width_g == 0.0 {
            return (d, c);
        }
        let h = self
            .side_hp
            .map_or(Complex64::new(1.0, 0.0), |hp| hp.response(f_hz, fs));
        let w = h * (d - c) * self.width_g;
        (d + w, c - w)
    }

    /// Exact worst-case peak gain `‖h_D′‖₁ + ‖h_C′‖₁` for `|L|, |R| <= 1`,
    /// summed from the impulse response of this very arithmetic until the
    /// slowest pole has decayed below 1e-20.
    fn l1_bound(&self) -> f64 {
        let mut k = self.clone();
        k.reset();
        let mut p: f64 = 0.0;
        if k.crossfeed {
            p = p.max(k.direct.a1.abs()).max(k.cross.a1.abs());
        }
        if let Some(hp) = &k.side_hp
            && k.width_g != 0.0
        {
            p = p.max(hp.a1.abs());
        }
        let n = if p > 0.0 {
            ((1e-20f64).ln() / p.ln()).ceil().clamp(1.0, 1e7) as usize
        } else {
            1
        };
        let mut sum = 0.0;
        for i in 0..=n {
            let (l, r) = k.frame(if i == 0 { 1.0 } else { 0.0 }, 0.0);
            sum += l.abs() + r.abs();
        }
        // The unsummed tail is below 1e-20 of the first terms times
        // 1/(1 − p) (≤ ~1e4 at the slowest corner), and processing rounds
        // a few ulps per sample: a 1e-9 relative margin (≈ 1e-8 dB) covers
        // both, so the peak bound and A/B headroom hold exactly.
        sum * (1.0 + 1e-9)
    }
}

/// Stereo headphone crossfeed and width stage.
#[derive(Clone, Debug)]
pub struct Crossfeed {
    active: bool,
    sample_rate: f64,
    width: f64,
    width_shape: StereoWidthShape,
    peak_bound: f64,
    kernel: Kernel,
}

impl Crossfeed {
    /// Carry `from`'s filter state over when it runs the same kernel (every
    /// coefficient bit for bit); otherwise nothing changes. Returns whether
    /// it did. Real-time safe.
    pub fn inherit_state(&mut self, from: &Crossfeed) -> bool {
        let (d, s) = (&mut self.kernel, &from.kernel);
        let same = self.active == from.active
            && d.direct == s.direct
            && d.cross == s.cross
            && d.crossfeed == s.crossfeed
            && d.scale == s.scale
            && d.width_g == s.width_g
            && d.side_hp == s.side_hp;
        if same {
            d.direct_l = s.direct_l;
            d.direct_r = s.direct_r;
            d.cross_l = s.cross_l;
            d.cross_r = s.cross_r;
            d.side = s.side;
        }
        same
    }

    /// Build a crossfeed stage for `sample_rate` and `channels`.
    ///
    /// Returns [`CrossfeedError::RequiresStereo`] when the config is active
    /// and `channels != 2`.
    pub fn try_new(
        config: &CrossfeedConfig,
        sample_rate: f64,
        channels: usize,
    ) -> Result<Self, CrossfeedError> {
        if config.is_active() && channels != 2 {
            return Err(CrossfeedError::RequiresStereo { channels });
        }
        if !sample_rate.is_finite() || sample_rate <= 0.0 {
            return Err(CrossfeedError::InvalidSampleRate);
        }

        let (cutoff_hz, feed_db) = config.effective_params();
        if !cutoff_hz.is_finite()
            || !(CROSSFEED_CUTOFF_MIN_HZ..=CROSSFEED_CUTOFF_MAX_HZ).contains(&cutoff_hz)
        {
            return Err(CrossfeedError::InvalidParams(format!(
                "cutoff_hz must be between {CROSSFEED_CUTOFF_MIN_HZ} and {CROSSFEED_CUTOFF_MAX_HZ} Hz (got {cutoff_hz})"
            )));
        }
        if !feed_db.is_finite()
            || !(CROSSFEED_FEED_MIN_DB..=CROSSFEED_FEED_MAX_DB).contains(&feed_db)
        {
            return Err(CrossfeedError::InvalidParams(format!(
                "feed_db must be between {CROSSFEED_FEED_MIN_DB} and {CROSSFEED_FEED_MAX_DB} dB (got {feed_db})"
            )));
        }
        let width = config.width;
        if !width.is_finite() || !(STEREO_WIDTH_MIN..=STEREO_WIDTH_MAX).contains(&width) {
            return Err(CrossfeedError::InvalidParams(format!(
                "width must be between {STEREO_WIDTH_MIN} and {STEREO_WIDTH_MAX} (got {width})"
            )));
        }
        let corner = config.width_corner_hz;
        if !corner.is_finite()
            || !(STEREO_WIDTH_CORNER_MIN_HZ..=STEREO_WIDTH_CORNER_MAX_HZ).contains(&corner)
        {
            return Err(CrossfeedError::InvalidParams(format!(
                "width_corner_hz must be between {STEREO_WIDTH_CORNER_MIN_HZ} and {STEREO_WIDTH_CORNER_MAX_HZ} Hz (got {corner})"
            )));
        }

        let crossfeed = config.crossfeed_active();
        let (direct, cross) = match config.mode {
            CrossfeedMode::Linkwitz => complementary_design(cutoff_hz, feed_db, sample_rate),
            CrossfeedMode::None => (first_order(1.0, 0.0, 0.0), first_order(0.0, 0.0, 0.0)),
            _ => {
                let (d, c, _) = bs2b_design(cutoff_hz, feed_db, sample_rate);
                (d, c)
            }
        };
        let width_g = (width - 1.0) / 2.0;
        let side_hp = match config.width_shape {
            StereoWidthShape::Flat => None,
            StereoWidthShape::AboveBass => Some(high_pass(corner, sample_rate)),
        };
        let unscaled = Kernel {
            direct,
            cross,
            crossfeed,
            scale: 1.0,
            width_g,
            side_hp,
            direct_l: OnePoleState::default(),
            direct_r: OnePoleState::default(),
            cross_l: OnePoleState::default(),
            cross_r: OnePoleState::default(),
            side: OnePoleState::default(),
        };
        // The crossfeed part only: the width is never compensated.
        let scale = if crossfeed {
            matched_gain(&direct, &cross, sample_rate, MATCHED_CORRELATION)
        } else {
            1.0
        };
        let kernel = Kernel {
            direct: scaled(&direct, scale),
            cross: scaled(&cross, scale),
            scale,
            ..unscaled
        };
        let peak_bound = if width_g == 0.0 {
            l1_norm(&kernel.direct) + l1_norm(&kernel.cross)
        } else {
            kernel.l1_bound()
        };
        Ok(Self {
            active: config.is_active(),
            sample_rate,
            width,
            width_shape: config.width_shape,
            peak_bound,
            kernel,
        })
    }

    /// Crossfeed direct-path coefficients (level scale included; without
    /// crossfeed, the scale alone). The width is not in them.
    pub fn direct_coeffs(&self) -> Coeffs {
        self.kernel.direct
    }

    /// Crossfeed cross-path coefficients (level scale included; zero
    /// without crossfeed). The width is not in them.
    pub fn cross_coeffs(&self) -> Coeffs {
        self.kernel.cross
    }

    /// Complex frequency response of the stage's direct path (own channel
    /// to own ear, width included) at `f_hz`.
    pub fn direct_response(&self, f_hz: f64) -> Complex64 {
        self.kernel.paths(f_hz, self.sample_rate).0
    }

    /// Complex frequency response of the stage's cross path (other channel
    /// to this ear, width included) at `f_hz`.
    pub fn cross_response(&self, f_hz: f64) -> Complex64 {
        self.kernel.paths(f_hz, self.sample_rate).1
    }

    /// Complex frequency response for a mono signal (`L == R`) at `f_hz`;
    /// the width does not change it.
    pub fn mono_response(&self, f_hz: f64) -> Complex64 {
        let (d, c) = self.kernel.paths(f_hz, self.sample_rate);
        d + c
    }

    /// The stage's direct and cross paths at `f_hz` as built (level and
    /// width included): output `L = d L + c R`, `R = c L + d R`. `(1, 0)`
    /// when inactive.
    pub fn stereo_paths(&self, f_hz: f64) -> (Complex64, Complex64) {
        if !self.active {
            return (Complex64::new(1.0, 0.0), Complex64::new(0.0, 0.0));
        }
        self.kernel.paths(f_hz, self.sample_rate)
    }

    /// Side gain `k` the stage was built with.
    pub fn width(&self) -> f64 {
        self.width
    }

    /// Width shape the stage was built with.
    pub fn width_shape(&self) -> StereoWidthShape {
        self.width_shape
    }

    /// Constant gain (dB) the stage applies: the crossfeed's loudness match
    /// (0 without crossfeed). 0 when inactive.
    pub fn level_db(&self) -> f64 {
        if self.active {
            lin_to_db(self.kernel.scale)
        } else {
            0.0
        }
    }

    /// Worst-case peak gain of the stage as built, width included
    /// (`‖h_D′‖₁ + ‖h_C′‖₁`) for inputs
    /// with `|L|, |R| <= 1`.
    pub fn peak_gain_bound(&self) -> f64 {
        self.peak_bound
    }

    /// Power response of one output channel at `f_hz` as built (level and
    /// width included) for equal-power stereo input with inter-channel
    /// correlation `rho`; 1 when inactive. The A/B analytic level match uses
    /// it with [`MATCHED_CORRELATION`].
    pub fn stereo_power_response(&self, f_hz: f64, rho: f64) -> f64 {
        if !self.active {
            return 1.0;
        }
        let (d, c) = self.kernel.paths(f_hz, self.sample_rate);
        stereo_power(d, c, rho)
    }

    /// [`Self::peak_gain_bound`] in dB: how far over the input's peak a
    /// worst-case signal can go.
    pub fn peak_gain_db(&self) -> f64 {
        lin_to_db(self.peak_gain_bound())
    }

    /// Gain change (dB) of the stage as built (width included) for a stereo
    /// pink-noise input with inter-channel correlation `rho`, A-weighted (0
    /// at [`MATCHED_CORRELATION`] and width 1).
    pub fn loudness_change_db(&self, rho: f64) -> f64 {
        let out = |f: f64| {
            let (d, c) = self.kernel.paths(f, self.sample_rate);
            stereo_power(d, c, rho)
        };
        -analytic_offset_db(&|_| 1.0, &out, self.sample_rate)
    }
}

impl Stage for Crossfeed {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if !self.active || chans.len() != 2 {
            return;
        }
        let (left_slice, right_slice) = chans.split_at_mut(1);
        let left = &mut left_slice[0];
        let right = &mut right_slice[0];
        let frames = left.len().min(right.len());

        let k = &mut self.kernel;
        for i in 0..frames {
            // Non-finite input would poison the recursive state forever.
            let xl = if left[i].is_finite() { left[i] } else { 0.0 };
            let xr = if right[i].is_finite() { right[i] } else { 0.0 };
            let (yl, yr) = k.frame(xl, xr);
            left[i] = yl;
            right[i] = yr;
        }
    }

    fn reset(&mut self) {
        self.kernel.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [f64; 4] = [44_100.0, 48_000.0, 96_000.0, 192_000.0];

    /// A custom config (the tests below check its exact peak bound).
    fn cfg(mode: CrossfeedMode) -> CrossfeedConfig {
        CrossfeedConfig {
            enabled: true,
            mode,
            cutoff_hz: 1200.0,
            feed_db: 7.5,
            ..CrossfeedConfig::default()
        }
    }

    fn log_freqs(n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| 20.0 * 1000f64.powf(i as f64 / (n - 1) as f64))
            .collect()
    }

    /// Reference bs2b responses written straight from the published
    /// equations (independent of `bs2b_design`).
    fn bs2b_reference(fc: f64, level: f64, fs: f64, f: f64) -> (Complex64, Complex64) {
        let gb_lo = level * -5.0 / 6.0 - 3.0;
        let gb_hi = level / 6.0 - 3.0;
        let g_lo = 10f64.powf(gb_lo / 20.0);
        let g_hi = 1.0 - 10f64.powf(gb_hi / 20.0);
        let gd_hi = 20.0 * g_hi.log10();
        let fc_hi = fc * 2f64.powf((gb_lo - gd_hi) / 12.0);
        let x = (-1.0 / (fs / (2.0 * PI * fc))).exp();
        let xh = (-1.0 / (fs / (2.0 * PI * fc_hi))).exp();
        let zi = Complex64::from_polar(1.0, -2.0 * PI * f / fs);
        let one = Complex64::new(1.0, 0.0);
        let lo = (g_lo * (1.0 - x)) / (one - zi * x);
        let hi = ((1.0 - g_hi * (1.0 - xh)) - zi * xh) / (one - zi * xh);
        (hi, lo)
    }

    /// Measure direct and cross responses by pushing an impulse through
    /// the stage (in odd-sized blocks) and taking the DFT.
    fn measured(cf: &mut Crossfeed, freqs: &[f64], fs: f64) -> Vec<(Complex64, Complex64)> {
        let n = 1 << 15;
        let mut l = vec![0.0; n];
        let mut r = vec![0.0; n];
        l[0] = 1.0;
        let mut i = 0;
        let mut b = 1;
        while i < n {
            let e = (i + b).min(n);
            cf.process(&mut [&mut l[i..e], &mut r[i..e]]);
            i = e;
            b = b % 97 + 13;
        }
        freqs
            .iter()
            .map(|&f| {
                let mut d = Complex64::new(0.0, 0.0);
                let mut c = Complex64::new(0.0, 0.0);
                for k in 0..n {
                    let w = Complex64::from_polar(1.0, -2.0 * PI * f * k as f64 / fs);
                    d += w * l[k];
                    c += w * r[k];
                }
                (d, c)
            })
            .collect()
    }

    /// Group delay (s) of `h(f)` by central difference of its phase.
    fn group_delay(h: impl Fn(f64) -> Complex64, f: f64) -> f64 {
        let df = f * 1e-4;
        let p1 = h(f - df).arg();
        let p2 = h(f + df).arg();
        -(p2 - p1) / (2.0 * PI * 2.0 * df)
    }

    #[test]
    fn bs2b_matches_published_equations_magnitude_and_phase() {
        let presets = [
            (CrossfeedMode::Bs2bDefault, 700.0, 4.5),
            (CrossfeedMode::Bs2bChuMoy, 700.0, 6.0),
            (CrossfeedMode::Bs2bJanMeier, 650.0, 9.5),
            (CrossfeedMode::Bs2bCustom, 1200.0, 7.5),
        ];
        let freqs = [20.0, 100.0, 700.0, 995.0, 3000.0, 10_000.0, 20_000.0];
        for fs in RATES {
            for (mode, fc, level) in presets {
                let mut cf = Crossfeed::try_new(&cfg(mode), fs, 2).unwrap();
                // The reference responses' L1 norms in closed form (direct
                // 1 - G_hi + 2 G_hi x_h, cross G_lo), times the level, are the
                // stage's peak bound.
                let gb_hi = level / 6.0 - 3.0;
                let g_hi = 1.0 - 10f64.powf(gb_hi / 20.0);
                let g_lo = 10f64.powf((level * -5.0 / 6.0 - 3.0) / 20.0);
                let (_, _, fc_hi) = bs2b_design(fc, level, fs);
                let xh = (-2.0 * PI * fc_hi / fs).exp();
                let s = db_to_lin(cf.level_db());
                let l1 = 1.0 - g_hi + 2.0 * g_hi * xh + g_lo;
                assert!((cf.peak_gain_bound() / (s * l1) - 1.0).abs() < 1e-9);

                let meas = measured(&mut cf, &freqs, fs);
                for (&f, (d, c)) in freqs.iter().zip(meas) {
                    let (hi, lo) = bs2b_reference(fc, level, fs, f);
                    assert!((d - hi * s).norm() < 1e-9, "{mode:?} {fs} {f}: direct");
                    assert!((c - lo * s).norm() < 1e-9, "{mode:?} {fs} {f}: cross");
                    // Relative phase cross vs direct.
                    let dphi = (c / d).arg() - (lo / hi).arg();
                    assert!(dphi.abs() < 1e-7, "{mode:?} {fs} {f}: phase {dphi}");
                }
                // DC ratio is the feed level.
                let ratio =
                    lin_to_db(cf.direct_response(0.0).norm() / cf.cross_response(0.0).norm());
                assert!((ratio - level).abs() < 1e-9, "{mode:?}: ratio {ratio}");
                // Relative LF delay matches the reference equations.
                let tau = group_delay(|f| cf.cross_response(f) / cf.direct_response(f), 100.0);
                let tau_ref = group_delay(
                    |f| {
                        let (hi, lo) = bs2b_reference(fc, level, fs, f);
                        lo / hi
                    },
                    100.0,
                );
                assert!(
                    (tau - tau_ref).abs() < 1e-9,
                    "{mode:?} {fs}: {tau} vs {tau_ref}"
                );
            }
        }
    }

    #[test]
    fn linkwitz_is_mono_flat_with_0_3_ms_interaural_delay() {
        for fs in RATES {
            let cf = Crossfeed::try_new(&cfg(CrossfeedMode::Linkwitz), fs, 2).unwrap();
            let s = 10f64.powf(cf.level_db() / 20.0);
            for f in log_freqs(200) {
                let dev = lin_to_db(cf.mono_response(f).norm() / s);
                assert!(dev.abs() < 1e-9, "mono deviation {dev} dB at {f} Hz, {fs}");
            }
            let tau = group_delay(|f| cf.cross_response(f) / cf.direct_response(f), 20.0);
            assert!(
                (tau - LINKWITZ_DELAY_S).abs() < 0.003 * LINKWITZ_DELAY_S,
                "{fs}: relative delay {} ms",
                tau * 1e3
            );
            let ratio = lin_to_db(cf.direct_response(0.0).norm() / cf.cross_response(0.0).norm());
            assert!((ratio - 6.0).abs() < 1e-9);
        }
    }

    #[test]
    fn output_stays_within_the_peak_bound_for_worst_case_signals() {
        for fs in RATES {
            for mode in [
                CrossfeedMode::Bs2bDefault,
                CrossfeedMode::Bs2bChuMoy,
                CrossfeedMode::Bs2bJanMeier,
                CrossfeedMode::Linkwitz,
            ] {
                let mut cf = Crossfeed::try_new(&cfg(mode), fs, 2).unwrap();
                let bound = cf.peak_gain_bound();
                // No cut for safety: the matched level goes over.
                assert!(bound > 1.0, "{mode:?} {fs}: bound {bound}");
                // A hard case: L=-1, R=+1 for 256 samples, then flipped.
                let mut l: Vec<f64> = (0..1024)
                    .map(|i| if i < 256 { -1.0 } else { 1.0 })
                    .collect();
                let mut r: Vec<f64> = l.iter().map(|x| -x).collect();
                cf.process(&mut [&mut l, &mut r]);
                let peak = l.iter().chain(&r).fold(0.0_f64, |m, x| m.max(x.abs()));
                assert!(peak <= bound * (1.0 + 1e-12), "{mode:?} {fs}: peak {peak}");

                // The input that attains the L1 bound for the left output at
                // sample N: L = sign(h_d[N-n]), R = sign(h_c[N-n]).
                cf.reset();
                let n = 4096;
                let mut l: Vec<f64> = vec![-1.0; n];
                let mut r: Vec<f64> = vec![1.0; n];
                l[n - 1] = 1.0;
                cf.process(&mut [&mut l, &mut r]);
                assert!(
                    l[n - 1] <= bound * (1.0 + 1e-12) && l[n - 1] > 0.99 * bound,
                    "{mode:?}: {} of {bound}",
                    l[n - 1]
                );
            }
        }
    }

    #[test]
    fn non_finite_input_does_not_poison_state() {
        let mut cf = Crossfeed::try_new(&cfg(CrossfeedMode::Bs2bDefault), 48_000.0, 2).unwrap();
        let mut fresh = cf.clone();
        let sig = |i: usize| (i as f64 * 0.01).sin() * 0.5;
        let mut l: Vec<f64> = (0..512).map(sig).collect();
        let mut r: Vec<f64> = (0..512).map(|i| sig(i + 7)).collect();
        let (mut l0, mut r0) = (l.clone(), r.clone());
        l[10] = f64::NAN;
        r[20] = f64::INFINITY;
        l0[10] = 0.0;
        r0[20] = 0.0;
        cf.process(&mut [&mut l, &mut r]);
        fresh.process(&mut [&mut l0, &mut r0]);
        assert!(l.iter().chain(&r).all(|x| x.is_finite()));
        assert_eq!(l, l0);
        assert_eq!(r, r0);
    }

    /// Deterministic uniform noise in (0, 1] (xorshift64*).
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let r = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
            ((r >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        }
    }

    fn width_cfg(
        mode: CrossfeedMode,
        width: f64,
        width_shape: StereoWidthShape,
    ) -> CrossfeedConfig {
        CrossfeedConfig {
            mode,
            width,
            width_shape,
            ..CrossfeedConfig::default()
        }
    }

    const WIDTHS: [f64; 8] = [0.0, 0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
    const SHAPES: [StereoWidthShape; 2] = [StereoWidthShape::Flat, StereoWidthShape::AboveBass];
    const WIDTH_MODES: [CrossfeedMode; 4] = [
        CrossfeedMode::None,
        CrossfeedMode::Linkwitz,
        CrossfeedMode::Bs2bDefault,
        CrossfeedMode::Bs2bJanMeier,
    ];

    /// Run `l`, `r` through `cf` in odd-sized blocks.
    fn run(cf: &mut Crossfeed, l: &mut [f64], r: &mut [f64]) {
        let mut i = 0;
        let mut b = 1;
        while i < l.len() {
            let e = (i + b).min(l.len());
            cf.process(&mut [&mut l[i..e], &mut r[i..e]]);
            i = e;
            b = b % 97 + 13;
        }
    }

    #[test]
    fn mono_is_bit_exact_at_every_width_and_shape() {
        let sig: Vec<f64> = (0..4096)
            .map(|i| 0.9 * (i as f64 * 0.013).sin() + 0.05 * (i as f64 * 1.7).cos())
            .collect();
        for fs in [44_100.0, 192_000.0] {
            for mode in WIDTH_MODES {
                for shape in SHAPES {
                    let reference = {
                        let c = width_cfg(mode, 1.0, shape);
                        let mut cf = Crossfeed::try_new(&c, fs, 2).unwrap();
                        let (mut l, mut r) = (sig.clone(), sig.clone());
                        run(&mut cf, &mut l, &mut r);
                        l
                    };
                    for k in WIDTHS {
                        let c = width_cfg(mode, k, shape);
                        let mut cf = Crossfeed::try_new(&c, fs, 2).unwrap();
                        let (mut l, mut r) = (sig.clone(), sig.clone());
                        run(&mut cf, &mut l, &mut r);
                        let bits = |v: &[f64]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
                        assert_eq!(bits(&l), bits(&r), "{mode:?} {shape:?} k {k}");
                        if mode == CrossfeedMode::None {
                            // No stage at k = 1: the input itself.
                            assert_eq!(bits(&l), bits(&sig), "{shape:?} k {k}");
                        } else {
                            assert_eq!(bits(&l), bits(&reference), "{mode:?} {shape:?} k {k}");
                        }
                        // The model agrees: mono response is the crossfeed's.
                        let fresh =
                            Crossfeed::try_new(&width_cfg(mode, 1.0, shape), fs, 2).unwrap();
                        for f in [20.0, 700.0, 10_000.0] {
                            let dev = (cf.mono_response(f) - fresh.mono_response(f)).norm();
                            assert!(dev < 1e-12, "{mode:?} {shape:?} k {k} {f} Hz");
                        }
                    }
                }
            }
        }
    }

    /// Impulse responses of the stage's direct and cross paths.
    fn paths_ir(cf: &Crossfeed, n: usize) -> (Vec<f64>, Vec<f64>) {
        let mut cf = cf.clone();
        cf.reset();
        let mut l = vec![0.0; n];
        let mut r = vec![0.0; n];
        l[0] = 1.0;
        run(&mut cf, &mut l, &mut r);
        (l, r)
    }

    #[test]
    fn width_filter_state_flushes_instead_of_going_subnormal() {
        // An impulse then silence left the above-bass high-pass state
        // stuck in subnormals (slow on x86).
        let c = width_cfg(CrossfeedMode::None, 2.0, StereoWidthShape::AboveBass);
        let mut cf = Crossfeed::try_new(&c, 768_000.0, 2).unwrap();
        let n = 768_000 / 2;
        let mut l = vec![0.0; n];
        let mut r = vec![0.0; n];
        l[0] = 1.0;
        run(&mut cf, &mut l, &mut r);
        for x in l.iter().chain(&r) {
            assert!(*x == 0.0 || x.is_normal(), "{x:e}");
        }
        assert_eq!((l[n - 1], r[n - 1]), (0.0, 0.0));
    }

    #[test]
    fn width_peak_bound_is_reached_and_not_exceeded() {
        let n = 8192;
        for fs in [44_100.0, 96_000.0] {
            for mode in WIDTH_MODES {
                for shape in SHAPES {
                    for k in WIDTHS {
                        let c = width_cfg(mode, k, shape);
                        if !c.is_active() {
                            continue;
                        }
                        let tag = format!("{mode:?} {shape:?} k {k} @{fs}");
                        let cf = Crossfeed::try_new(&c, fs, 2).unwrap();
                        let bound = cf.peak_gain_bound();
                        let (hd, hc) = paths_ir(&cf, n);
                        let l1: f64 = hd.iter().chain(&hc).map(|x| x.abs()).sum();
                        assert!((l1 - bound).abs() < 2e-9 * bound, "{tag}: {l1} vs {bound}");
                        // The input that reaches the bound at the last
                        // sample: L = sign(h_D′[N−1−n]), R = sign(h_C′[N−1−n]).
                        let sign = |x: f64| if x < 0.0 { -1.0 } else { 1.0 };
                        let mut l: Vec<f64> = (0..n).map(|i| sign(hd[n - 1 - i])).collect();
                        let mut r: Vec<f64> = (0..n).map(|i| sign(hc[n - 1 - i])).collect();
                        let mut run_cf = cf.clone();
                        run(&mut run_cf, &mut l, &mut r);
                        let peak = l.iter().chain(&r).fold(0.0_f64, |m, x| m.max(x.abs()));
                        assert!(peak <= bound + 1e-12, "{tag}: {peak} > {bound}");
                        assert!(
                            l[n - 1] > bound * (1.0 - 2e-9),
                            "{tag}: {} < {bound}",
                            l[n - 1]
                        );
                        // Random full-scale ±1 never goes over it.
                        let mut noise = Noise(3);
                        let mut l: Vec<f64> = (0..n).map(|_| sign(noise.next() - 0.5)).collect();
                        let mut r: Vec<f64> = (0..n).map(|_| sign(noise.next() - 0.5)).collect();
                        let mut run_cf = cf.clone();
                        run(&mut run_cf, &mut l, &mut r);
                        let peak = l.iter().chain(&r).fold(0.0_f64, |m, x| m.max(x.abs()));
                        assert!(peak <= bound + 1e-12, "{tag}: noise {peak} > {bound}");
                    }
                }
            }
        }
        // Width alone, flat: max(1, k).
        for k in WIDTHS {
            let c = width_cfg(CrossfeedMode::None, k, SHAPES[0]);
            let cf = Crossfeed::try_new(&c, 48_000.0, 2).unwrap();
            assert!(
                (cf.peak_gain_bound() - k.max(1.0)).abs() < 2e-9 * k.max(1.0),
                "k {k}"
            );
        }
    }
}
