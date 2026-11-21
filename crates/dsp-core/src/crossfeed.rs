//! Headphone crossfeed stage (bs2b and Linkwitz-style).
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
//!
//! **Level (`CrossfeedConfig.level`):** the stage scales both paths by one
//! constant `s`, which keeps the direct/cross ratio and replaces the bs2b
//! library's own level normalisation. The EQ preamp is a plain number and is
//! not adjusted for crossfeed, so nothing compensates twice.
//!
//! - `matched` (default): `s` makes the A-weighted loudness of the output
//!   equal to the input's for a stereo pink-noise reference with
//!   inter-channel correlation [`MATCHED_CORRELATION`], the same weighting and
//!   match as [`crate::loudness::analytic_offset_db`]. Worst-case signals can
//!   then exceed the input's peak by +1.4 to +5.6 dB; the chain is f64 to the
//!   quantizer, so such a signal clips only there: unless the limiter is on,
//!   or enough output headroom (`output.headroom_db`) or digital attenuation
//!   comes first. Hardware volume gives no headroom.
//! - `peak_safe`: `s = 1 / (‖h_direct‖₁ + ‖h_cross‖₁)`, the exact worst-case
//!   peak gain for inputs bounded by `|x| <= 1`, so the output never exceeds
//!   the input's peak bound (but is quieter).
//!
//! Both are computed when the stage is built, never on the audio thread.

use crate::biquad::Coeffs;
use crate::loudness::analytic_offset_db;
use crate::{Complex64, Stage, db_to_lin, lin_to_db};
use audivane_protocol::{
    CROSSFEED_CUTOFF_MAX_HZ, CROSSFEED_CUTOFF_MIN_HZ, CROSSFEED_FEED_MAX_DB, CROSSFEED_FEED_MIN_DB,
    CrossfeedConfig, CrossfeedLevel, CrossfeedMode,
};
use std::f64::consts::PI;
use std::fmt;

/// Target low-frequency interaural delay of the Linkwitz-style mode (s).
pub const LINKWITZ_DELAY_S: f64 = 0.3e-3;

/// Inter-channel correlation of the stereo reference signal that the
/// `matched` level is defined for: the middle of the 0.3..0.6 range typical
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

/// Power response of one output channel at `f_hz` for equal-power stereo
/// input with inter-channel correlation `rho`.
fn stereo_power(direct: &Coeffs, cross: &Coeffs, f_hz: f64, fs: f64, rho: f64) -> f64 {
    let d = direct.response(f_hz, fs);
    let c = cross.response(f_hz, fs);
    d.norm_sqr() + c.norm_sqr() + 2.0 * rho * (d * c.conj()).re
}

/// Linear gain that gives `direct`/`cross` the same A-weighted loudness as
/// their input for stereo pink noise with correlation `rho`.
fn matched_gain(direct: &Coeffs, cross: &Coeffs, fs: f64, rho: f64) -> f64 {
    let out = |f: f64| stereo_power(direct, cross, f, fs, rho);
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

/// Stereo headphone crossfeed stage.
#[derive(Clone, Debug)]
pub struct Crossfeed {
    enabled: bool,
    sample_rate: f64,
    /// Scaled by `scale` (applied to both paths).
    direct: Coeffs,
    cross: Coeffs,
    scale: f64,
    level: CrossfeedLevel,
    direct_l: OnePoleState,
    direct_r: OnePoleState,
    cross_l: OnePoleState,
    cross_r: OnePoleState,
}

impl Crossfeed {
    /// Build a crossfeed stage for `sample_rate` and `channels`.
    ///
    /// Returns [`CrossfeedError::RequiresStereo`] when `config.enabled` is true
    /// and `channels != 2`.
    pub fn try_new(
        config: &CrossfeedConfig,
        sample_rate: f64,
        channels: usize,
    ) -> Result<Self, CrossfeedError> {
        if config.enabled && channels != 2 {
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

        let (direct, cross) = match config.mode {
            CrossfeedMode::Linkwitz => complementary_design(cutoff_hz, feed_db, sample_rate),
            _ => {
                let (d, c, _) = bs2b_design(cutoff_hz, feed_db, sample_rate);
                (d, c)
            }
        };
        let scale = match config.level {
            CrossfeedLevel::Matched => {
                matched_gain(&direct, &cross, sample_rate, MATCHED_CORRELATION)
            }
            CrossfeedLevel::PeakSafe => 1.0 / (l1_norm(&direct) + l1_norm(&cross)),
        };
        Ok(Self {
            enabled: config.enabled,
            sample_rate,
            direct: scaled(&direct, scale),
            cross: scaled(&cross, scale),
            scale,
            level: config.level,
            direct_l: OnePoleState::default(),
            direct_r: OnePoleState::default(),
            cross_l: OnePoleState::default(),
            cross_r: OnePoleState::default(),
        })
    }

    /// Direct-path coefficients (headroom scale included).
    pub fn direct_coeffs(&self) -> Coeffs {
        self.direct
    }

    /// Crossfeed-path coefficients (headroom scale included).
    pub fn cross_coeffs(&self) -> Coeffs {
        self.cross
    }

    /// Complex frequency response of the direct path at `f_hz`.
    pub fn direct_response(&self, f_hz: f64) -> Complex64 {
        self.direct.response(f_hz, self.sample_rate)
    }

    /// Complex frequency response of the crossfeed path at `f_hz`.
    pub fn cross_response(&self, f_hz: f64) -> Complex64 {
        self.cross.response(f_hz, self.sample_rate)
    }

    /// Complex frequency response for a mono signal (`L == R`) at `f_hz`.
    pub fn mono_response(&self, f_hz: f64) -> Complex64 {
        self.direct_response(f_hz) + self.cross_response(f_hz)
    }

    /// Level policy the stage was built with.
    pub fn level(&self) -> CrossfeedLevel {
        self.level
    }

    /// Constant gain (dB) the stage applies to both paths: the loudness
    /// match for `matched`, the worst-case peak gain for `peak_safe` (then
    /// the stage's peak gain is exactly 0 dB). 0 when disabled.
    pub fn level_db(&self) -> f64 {
        if self.enabled {
            lin_to_db(self.scale)
        } else {
            0.0
        }
    }

    /// Worst-case peak gain of the stage as built (`‖h_d‖₁ + ‖h_c‖₁`; 1 up to
    /// rounding for `peak_safe`) for inputs with `|L|, |R| <= 1`.
    pub fn peak_gain_bound(&self) -> f64 {
        l1_norm(&self.direct) + l1_norm(&self.cross)
    }

    /// Power response of one output channel at `f_hz` as built (level
    /// included) for equal-power stereo input with inter-channel correlation
    /// `rho`; 1 when disabled. The A/B analytic level match uses it with
    /// [`MATCHED_CORRELATION`].
    pub fn stereo_power_response(&self, f_hz: f64, rho: f64) -> f64 {
        if !self.enabled {
            return 1.0;
        }
        stereo_power(&self.direct, &self.cross, f_hz, self.sample_rate, rho)
    }

    /// [`Self::peak_gain_bound`] in dB: how far over the input's peak a
    /// worst-case signal can go.
    pub fn peak_gain_db(&self) -> f64 {
        lin_to_db(self.peak_gain_bound())
    }

    /// Gain change (dB) of the stage as built for a stereo pink-noise input
    /// with inter-channel correlation `rho`, A-weighted (0 for `matched` at
    /// [`MATCHED_CORRELATION`]).
    pub fn loudness_change_db(&self, rho: f64) -> f64 {
        let out = |f: f64| stereo_power(&self.direct, &self.cross, f, self.sample_rate, rho);
        -analytic_offset_db(&|_| 1.0, &out, self.sample_rate)
    }
}

impl Stage for Crossfeed {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if !self.enabled || chans.len() != 2 {
            return;
        }
        let (left_slice, right_slice) = chans.split_at_mut(1);
        let left = &mut left_slice[0];
        let right = &mut right_slice[0];
        let frames = left.len().min(right.len());

        let d = &self.direct;
        let c = &self.cross;
        for i in 0..frames {
            // Non-finite input would poison the recursive state forever.
            let xl = if left[i].is_finite() { left[i] } else { 0.0 };
            let xr = if right[i].is_finite() { right[i] } else { 0.0 };
            let dl = self.direct_l.step(d, xl);
            let cl = self.cross_l.step(c, xr);
            let dr = self.direct_r.step(d, xr);
            let cr = self.cross_r.step(c, xl);
            left[i] = dl + cl;
            right[i] = dr + cr;
        }
    }

    fn reset(&mut self) {
        self.direct_l = OnePoleState::default();
        self.direct_r = OnePoleState::default();
        self.cross_l = OnePoleState::default();
        self.cross_r = OnePoleState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [f64; 4] = [44_100.0, 48_000.0, 96_000.0, 192_000.0];

    /// Peak-safe config (the tests below check its exact peak bound).
    fn cfg(mode: CrossfeedMode) -> CrossfeedConfig {
        CrossfeedConfig {
            enabled: true,
            mode,
            cutoff_hz: 1200.0,
            feed_db: 7.5,
            level: CrossfeedLevel::PeakSafe,
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
                // Reference headroom scale from the reference responses' L1
                // norms (closed form: direct 1 - G_hi + 2 G_hi x_h, cross G_lo).
                let gb_hi = level / 6.0 - 3.0;
                let g_hi = 1.0 - 10f64.powf(gb_hi / 20.0);
                let g_lo = 10f64.powf((level * -5.0 / 6.0 - 3.0) / 20.0);
                let (_, _, fc_hi) = bs2b_design(fc, level, fs);
                let xh = (-2.0 * PI * fc_hi / fs).exp();
                let s = 1.0 / (1.0 - g_hi + 2.0 * g_hi * xh + g_lo);
                assert!((lin_to_db(s) - cf.level_db()).abs() < 1e-9);

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
    fn bs2b_default_high_shelf_corner_is_995_hz() {
        let (_, _, fc_hi) = bs2b_design(700.0, 4.5, 48_000.0);
        assert!((fc_hi - 994.7).abs() < 0.5, "fc_hi = {fc_hi}");
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
    fn output_never_exceeds_input_peak_for_worst_case_signals() {
        for fs in RATES {
            for mode in [
                CrossfeedMode::Bs2bDefault,
                CrossfeedMode::Bs2bChuMoy,
                CrossfeedMode::Bs2bJanMeier,
                CrossfeedMode::Linkwitz,
            ] {
                let mut cf = Crossfeed::try_new(&cfg(mode), fs, 2).unwrap();
                assert!((cf.peak_gain_bound() - 1.0).abs() < 1e-12);
                // The worst case for peak safety: L=-1, R=+1 for 256 samples,
                // then flipped.
                let mut l: Vec<f64> = (0..1024)
                    .map(|i| if i < 256 { -1.0 } else { 1.0 })
                    .collect();
                let mut r: Vec<f64> = l.iter().map(|x| -x).collect();
                cf.process(&mut [&mut l, &mut r]);
                let peak = l.iter().chain(&r).fold(0.0_f64, |m, x| m.max(x.abs()));
                assert!(peak <= 1.0 + 1e-12, "{mode:?} {fs}: peak {peak}");

                // The input that attains the L1 bound for the left output at
                // sample N: L = sign(h_d[N-n]), R = sign(h_c[N-n]).
                cf.reset();
                let n = 4096;
                let mut l: Vec<f64> = vec![-1.0; n];
                let mut r: Vec<f64> = vec![1.0; n];
                l[n - 1] = 1.0;
                cf.process(&mut [&mut l, &mut r]);
                assert!(
                    l[n - 1] <= 1.0 + 1e-12 && l[n - 1] > 0.99,
                    "{mode:?}: {}",
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

    #[test]
    fn non_stereo_is_rejected_and_process_is_noop() {
        let cfg = CrossfeedConfig::default();
        assert_eq!(
            Crossfeed::try_new(&cfg, 48_000.0, 1).unwrap_err(),
            CrossfeedError::RequiresStereo { channels: 1 }
        );
        assert_eq!(
            Crossfeed::try_new(&cfg, 48_000.0, 6).unwrap_err(),
            CrossfeedError::RequiresStereo { channels: 6 }
        );

        let mut cf = Crossfeed::try_new(&cfg, 48_000.0, 2).unwrap();
        let mut mono = vec![0.5, -0.25, 0.75];
        let orig = mono.clone();
        cf.process(&mut [&mut mono]);
        assert_eq!(mono, orig);
    }

    /// Every preset plus the corners of the `bs2b_custom` range.
    fn level_cases() -> Vec<(&'static str, CrossfeedConfig)> {
        let preset = |mode| CrossfeedConfig {
            mode,
            ..CrossfeedConfig::default()
        };
        let custom = |cutoff_hz, feed_db| CrossfeedConfig {
            mode: CrossfeedMode::Bs2bCustom,
            cutoff_hz,
            feed_db,
            ..CrossfeedConfig::default()
        };
        vec![
            ("bs2b_default", preset(CrossfeedMode::Bs2bDefault)),
            ("bs2b_chu_moy", preset(CrossfeedMode::Bs2bChuMoy)),
            ("bs2b_jan_meier", preset(CrossfeedMode::Bs2bJanMeier)),
            ("linkwitz", preset(CrossfeedMode::Linkwitz)),
            ("custom 300 Hz / 1 dB", custom(300.0, 1.0)),
            ("custom 300 Hz / 15 dB", custom(300.0, 15.0)),
            ("custom 2000 Hz / 1 dB", custom(2000.0, 1.0)),
            ("custom 2000 Hz / 15 dB", custom(2000.0, 15.0)),
        ]
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

        /// Standard normal (Box-Muller).
        fn gauss(&mut self) -> f64 {
            let (u, v) = (self.next(), self.next());
            (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
        }
    }

    /// `frames` samples of pink noise built in the frequency domain: seeded
    /// complex Gaussian bins with power `1/f` from 10 Hz to Nyquist (zero
    /// below), inverse real FFT, scaled to an RMS of 0.1.
    fn pink(seed: u64, frames: usize, fs: f64) -> Vec<f64> {
        use realfft::RealFftPlanner;
        let n = frames.next_power_of_two();
        let ifft = RealFftPlanner::<f64>::new().plan_fft_inverse(n);
        let mut spec = ifft.make_input_vec();
        let mut noise = Noise(seed);
        let bins = spec.len();
        for (k, bin) in spec.iter_mut().enumerate() {
            let f = k as f64 * fs / n as f64;
            *bin = if f < 10.0 || k == bins - 1 {
                Complex64::new(0.0, 0.0)
            } else {
                Complex64::new(noise.gauss(), noise.gauss()) / f.sqrt()
            };
        }
        let mut out = ifft.make_output_vec();
        ifft.process(&mut spec, &mut out).unwrap();
        out.truncate(frames);
        let rms = (out.iter().map(|x| x * x).sum::<f64>() / frames as f64).sqrt();
        out.iter().map(|x| 0.1 * x / rms).collect()
    }

    /// Stereo pink noise with inter-channel correlation `rho`.
    fn stereo_pink(rho: f64, frames: usize, fs: f64) -> (Vec<f64>, Vec<f64>) {
        let (c, u1, u2) = (
            pink(1, frames, fs),
            pink(2, frames, fs),
            pink(3, frames, fs),
        );
        let (a, b) = (rho.sqrt(), (1.0 - rho).sqrt());
        (0..frames)
            .map(|i| (a * c[i] + b * u1[i], a * c[i] + b * u2[i]))
            .unzip()
    }

    /// A-weighted level change (dB) of the stage, measured on stereo pink
    /// noise with correlation `rho` (first second discarded as warm-up).
    fn measured_loudness_change_db(cf: &mut Crossfeed, fs: f64, rho: f64) -> f64 {
        let warm = fs as usize;
        let (mut l, mut r) = stereo_pink(rho, warm + 8 * fs as usize, fs);
        let mut meter_in = crate::loudness::AWeightedMeter::new(fs, 2);
        meter_in.push(&[&l[warm..], &r[warm..]]);
        let mut i = 0;
        let mut b = 1;
        while i < l.len() {
            let e = (i + b).min(l.len());
            cf.process(&mut [&mut l[i..e], &mut r[i..e]]);
            i = e;
            b = b % 997 + 61;
        }
        let mut meter_out = crate::loudness::AWeightedMeter::new(fs, 2);
        meter_out.push(&[&l[warm..], &r[warm..]]);
        meter_out.rms_db() - meter_in.rms_db()
    }

    #[test]
    fn matched_level_keeps_measured_a_weighted_loudness() {
        for fs in [44_100.0, 48_000.0] {
            for (name, config) in level_cases() {
                let mut cf = Crossfeed::try_new(&config, fs, 2).unwrap();
                assert_eq!(cf.level(), CrossfeedLevel::Matched);
                assert!(cf.loudness_change_db(MATCHED_CORRELATION).abs() < 1e-9);
                let change = measured_loudness_change_db(&mut cf, fs, MATCHED_CORRELATION);
                assert!(change.abs() < 0.1, "{name} @{fs}: {change:+.3} dB");
            }
        }
    }

    #[test]
    fn loudness_change_prediction_matches_measurement_at_other_correlations() {
        let fs = 48_000.0;
        for (name, config) in level_cases() {
            for level in [CrossfeedLevel::Matched, CrossfeedLevel::PeakSafe] {
                for rho in [0.0, 1.0] {
                    let config = CrossfeedConfig {
                        level,
                        ..config.clone()
                    };
                    let mut cf = Crossfeed::try_new(&config, fs, 2).unwrap();
                    let predicted = cf.loudness_change_db(rho);
                    let measured = measured_loudness_change_db(&mut cf, fs, rho);
                    assert!(
                        (predicted - measured).abs() < 0.1,
                        "{name} {level:?} rho {rho}: predicted {predicted:+.3}, measured {measured:+.3}"
                    );
                }
            }
        }
    }

    #[test]
    fn matched_is_louder_than_peak_safe_and_bounded() {
        for fs in RATES {
            for (name, config) in level_cases() {
                let matched = Crossfeed::try_new(&config, fs, 2).unwrap();
                let safe = Crossfeed::try_new(
                    &CrossfeedConfig {
                        level: CrossfeedLevel::PeakSafe,
                        ..config.clone()
                    },
                    fs,
                    2,
                )
                .unwrap();
                assert!((safe.peak_gain_db()).abs() < 1e-9);
                assert!(matched.level_db() > safe.level_db(), "{name} @{fs}");
                // Same filters, other constant: the peak bound moves by the
                // difference of the two gains.
                let over = matched.peak_gain_db();
                assert!(
                    (over - (matched.level_db() - safe.level_db())).abs() < 1e-9,
                    "{name} @{fs}"
                );
                assert!(over > 1.0 && over < 6.0, "{name} @{fs}: +{over:.2} dB");
            }
        }
    }

    /// Prints the crossfeed level table.
    /// `cargo test -p audivane-dsp-core crossfeed::tests::level_report -- --nocapture --ignored`
    #[test]
    #[ignore]
    fn level_report() {
        for fs in [44_100.0, 96_000.0] {
            println!("@{fs}");
            for (name, config) in level_cases() {
                let matched = Crossfeed::try_new(&config, fs, 2).unwrap();
                let safe = Crossfeed::try_new(
                    &CrossfeedConfig {
                        level: CrossfeedLevel::PeakSafe,
                        ..config.clone()
                    },
                    fs,
                    2,
                )
                .unwrap();
                let rho = [0.0, 0.3, 0.5, 0.6, 1.0].map(|r| matched.loudness_change_db(r));
                let safe_rho = safe.loudness_change_db(MATCHED_CORRELATION);
                println!(
                    "| {name} | {:+.2} dB | {:+.2} dB | {:+.2} dB | {:+.2} dB | {:+.2} / {:+.2} / {:+.2} / {:+.2} / {:+.2} dB |",
                    safe.level_db(),
                    safe_rho,
                    matched.level_db(),
                    matched.peak_gain_db(),
                    rho[0],
                    rho[1],
                    rho[2],
                    rho[3],
                    rho[4],
                );
            }
        }
    }

    /// Prints the crossfeed response numbers.
    /// `cargo test -p audivane-dsp-core crossfeed::tests::report -- --nocapture --ignored`
    #[test]
    #[ignore]
    fn report() {
        for (mode, name) in [
            (CrossfeedMode::Bs2bDefault, "bs2b_default"),
            (CrossfeedMode::Bs2bChuMoy, "bs2b_chu_moy"),
            (CrossfeedMode::Bs2bJanMeier, "bs2b_jan_meier"),
            (CrossfeedMode::Linkwitz, "linkwitz"),
        ] {
            for fs in [44_100.0, 96_000.0] {
                let cf = Crossfeed::try_new(&cfg(mode), fs, 2).unwrap();
                let s = 10f64.powf(cf.level_db() / 20.0);
                let (mut lo, mut hi) = (f64::MAX, f64::MIN);
                for f in log_freqs(400) {
                    let m = lin_to_db(cf.mono_response(f).norm() / s);
                    lo = lo.min(m);
                    hi = hi.max(m);
                }
                let m20 = lin_to_db(cf.mono_response(20.0).norm());
                let tau = group_delay(|f| cf.cross_response(f) / cf.direct_response(f), 20.0);
                let tau1k = group_delay(|f| cf.cross_response(f) / cf.direct_response(f), 1000.0);
                println!(
                    "{name} @{fs}: headroom {:.2} dB, mono (unscaled) {:.3}..{:.3} dB, mono@20Hz {:.2} dB, rel delay 20 Hz {:.3} ms, 1 kHz {:.3} ms",
                    cf.level_db(),
                    lo,
                    hi,
                    m20,
                    tau * 1e3,
                    tau1k * 1e3
                );
            }
        }
    }
}
