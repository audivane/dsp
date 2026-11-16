//! RBJ Audio-EQ-Cookbook biquads in f64 and the parametric EQ cascade.
//!
//! Coefficients follow Robert Bristow-Johnson's Audio EQ Cookbook (2005) as
//! restated in the W3C note: every filter uses
//! `alpha = sin(w0) / (2 Q)`, including the shelves, where `Q = 1/sqrt(2)`
//! corresponds to the cookbook slope `S = 1`.
//!
//! Processing uses a transposed Direct Form II (TDF-II) section.  It needs two
//! state words instead of four and is the usual choice for audio.  TDF-II has
//! a reputation for losing precision for very low-frequency / high-Q filters
//! because its state variables are not the input history; in f64 that is not
//! a problem in practice.  At the extreme 20 Hz, Q = 10, 768 kHz peaking case
//! TDF-II and Direct Form I agree to ~2e-13 (about -225 dB relative to the
//! impulse response peak), so the simpler form is used throughout.

use crate::{Complex64, Stage};
use audivane_protocol::{Band, EqProfile, FilterType};

/// Normalized biquad coefficients (a0 == 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coeffs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coeffs {
    pub const IDENTITY: Coeffs = Coeffs {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// Design one band at `sample_rate` using the RBJ cookbook formulas.
    /// A disabled band, or one at/above Nyquist, yields `IDENTITY`.
    ///
    /// Degenerate parameters (non-positive/NaN frequency, Q or gain) also
    /// yield `IDENTITY`: a band that cannot be designed is a no-op rather
    /// than a source of NaNs in the audio path.
    pub fn design(band: &Band, sample_rate: f64) -> Coeffs {
        if !band.enabled {
            return Coeffs::IDENTITY;
        }
        let fs = sample_rate;
        let nyquist = 0.5 * fs;
        let f0 = band.freq_hz;
        let q = band.q;
        let gain_db = band.gain_db;
        if !fs.is_finite() || fs <= 0.0 {
            return Coeffs::IDENTITY;
        }
        if !f0.is_finite() || f0 <= 0.0 || f0 >= nyquist {
            return Coeffs::IDENTITY;
        }
        if !q.is_finite() || q <= 0.0 || !gain_db.is_finite() {
            return Coeffs::IDENTITY;
        }

        let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        // Amplitudes: A = 10^(dBgain/40) (cookbook).
        let a = 10f64.powf(gain_db / 40.0);

        // (b0, b1, b2, a0, a1, a2) before normalisation.
        let (b0, b1, b2, a0, a1, a2) = match band.kind {
            FilterType::Peaking => (
                1.0 + alpha * a,
                -2.0 * cos_w0,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos_w0,
                1.0 - alpha / a,
            ),
            FilterType::LowShelf => {
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                    (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
            FilterType::HighShelf => {
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                    (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
            FilterType::LowPass => (
                (1.0 - cos_w0) * 0.5,
                1.0 - cos_w0,
                (1.0 - cos_w0) * 0.5,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
            FilterType::HighPass => (
                (1.0 + cos_w0) * 0.5,
                -(1.0 + cos_w0),
                (1.0 + cos_w0) * 0.5,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
            FilterType::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos_w0, 1.0 - alpha),
            FilterType::Notch => (
                1.0,
                -2.0 * cos_w0,
                1.0,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
            FilterType::AllPass => (
                1.0 - alpha,
                -2.0 * cos_w0,
                1.0 + alpha,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
        };

        if !a0.is_finite() || a0 == 0.0 {
            return Coeffs::IDENTITY;
        }
        let inv = 1.0 / a0;
        let c = Coeffs {
            b0: b0 * inv,
            b1: b1 * inv,
            b2: b2 * inv,
            a1: a1 * inv,
            a2: a2 * inv,
        };
        if !c.is_finite() {
            return Coeffs::IDENTITY;
        }
        c
    }

    /// Complex frequency response at `f_hz`.
    ///
    /// Evaluates `(b0 + b1 z^-1 + b2 z^-2) / (1 + a1 z^-1 + a2 z^-2)` with
    /// `z = exp(j 2 pi f / fs)`.
    pub fn response(&self, f_hz: f64, sample_rate: f64) -> Complex64 {
        if !sample_rate.is_finite() || sample_rate <= 0.0 || !f_hz.is_finite() {
            return Complex64::new(f64::NAN, f64::NAN);
        }
        let w = 2.0 * std::f64::consts::PI * f_hz / sample_rate;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        // z^-1 = cos(w) - j sin(w), z^-2 = cos(2w) - j sin(2w).
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        let den = dr * dr + di * di;
        Complex64::new((nr * dr + ni * di) / den, (ni * dr - nr * di) / den)
    }

    fn is_identity(&self) -> bool {
        *self == Coeffs::IDENTITY
    }

    fn is_finite(&self) -> bool {
        self.b0.is_finite()
            && self.b1.is_finite()
            && self.b2.is_finite()
            && self.a1.is_finite()
            && self.a2.is_finite()
    }
}

/// Two-word state of a transposed Direct Form II section.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BiquadState {
    z1: f64,
    z2: f64,
}

impl BiquadState {
    /// One TDF-II step:
    /// `y = b0 x + z1; z1 = b1 x - a1 y + z2; z2 = b2 x - a2 y`.
    #[inline]
    fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }
}

struct ChannelFilter {
    preamp: f64,
    bands: Vec<Coeffs>,
}

impl ChannelFilter {
    fn from_layers(
        global_preamp_db: f64,
        linked_bands: &[Band],
        section: Option<(f64, &[Band])>,
        sample_rate: f64,
    ) -> Self {
        let mut preamp = crate::db_to_lin(global_preamp_db);
        let mut bands: Vec<Coeffs> = linked_bands
            .iter()
            .map(|b| Coeffs::design(b, sample_rate))
            .filter(|c| !c.is_identity())
            .collect();
        if let Some((sec_preamp_db, sec_bands)) = section {
            if sec_preamp_db != 0.0 {
                preamp *= crate::db_to_lin(sec_preamp_db);
            }
            bands.extend(
                sec_bands
                    .iter()
                    .map(|b| Coeffs::design(b, sample_rate))
                    .filter(|c| !c.is_identity()),
            );
        }
        Self { preamp, bands }
    }

    fn response(&self, f_hz: f64, sample_rate: f64) -> Complex64 {
        let mut h = Complex64::new(self.preamp, 0.0);
        for c in &self.bands {
            h *= c.response(f_hz, sample_rate);
        }
        h
    }
}

/// Parametric EQ: preamp followed by a cascade of biquads, per channel.
///
/// Channel 0 runs global preamp + linked bands (if `linked_enabled`) + `left`
/// (if present and enabled). Channel 1 runs the same with `right`. Channels >= 2
/// run global preamp + linked bands only.
pub struct Eq {
    linked: ChannelFilter,
    left: ChannelFilter,
    right: ChannelFilter,
    sample_rate: f64,
    channels: usize,
    /// `states[channel][band]`.
    states: Vec<Vec<BiquadState>>,
}

impl Eq {
    pub fn new(profile: &EqProfile, sample_rate: f64, channels: usize) -> Self {
        let linked_bands: &[Band] = if profile.linked_enabled {
            &profile.bands
        } else {
            &[]
        };
        let linked = ChannelFilter::from_layers(profile.preamp_db, linked_bands, None, sample_rate);
        let left_sec = profile
            .left
            .as_ref()
            .filter(|c| c.enabled)
            .map(|c| (c.preamp_db, c.bands.as_slice()));
        let right_sec = profile
            .right
            .as_ref()
            .filter(|c| c.enabled)
            .map(|c| (c.preamp_db, c.bands.as_slice()));
        let left =
            ChannelFilter::from_layers(profile.preamp_db, linked_bands, left_sec, sample_rate);
        let right =
            ChannelFilter::from_layers(profile.preamp_db, linked_bands, right_sec, sample_rate);
        let states = (0..channels)
            .map(|ch| {
                let band_count = match ch {
                    0 => left.bands.len(),
                    1 => right.bands.len(),
                    _ => linked.bands.len(),
                };
                vec![BiquadState::default(); band_count]
            })
            .collect();
        Eq {
            linked,
            left,
            right,
            sample_rate,
            channels,
            states,
        }
    }

    fn filter_for(&self, channel: usize) -> &ChannelFilter {
        match channel {
            0 => &self.left,
            1 => &self.right,
            _ => &self.linked,
        }
    }

    /// Complex response of channel 0 (left, preamp included) at `f_hz`.
    pub fn response(&self, f_hz: f64) -> Complex64 {
        self.response_channel(0, f_hz)
    }

    /// Complex response of `channel` (preamp included) at `f_hz`.
    /// Channel 0 applies global + linked + enabled `left`, channel 1 applies
    /// global + linked + enabled `right`, and channels >= 2 apply global + linked.
    pub fn response_channel(&self, channel: usize, f_hz: f64) -> Complex64 {
        self.filter_for(channel).response(f_hz, self.sample_rate)
    }

    /// Preamp (dB, <= 0) that keeps the largest boost of `profile` across channels
    /// from clipping a full-scale sine: minus the peak of |H| over a dense log
    /// grid from 10 Hz to Nyquist, ignoring the profile's global `preamp_db`
    /// while accounting for each enabled section's combined response.
    pub fn auto_preamp_db(profile: &EqProfile, sample_rate: f64) -> f64 {
        let linked: &[Band] = if profile.linked_enabled {
            &profile.bands
        } else {
            &[]
        };
        let left = profile.left.as_ref().filter(|c| c.enabled);
        let right = profile.right.as_ref().filter(|c| c.enabled);

        let pre_l = Self::channel_auto_preamp_db(
            linked,
            left.map_or(0.0, |c| c.preamp_db),
            left.map_or(&[][..], |c| c.bands.as_slice()),
            sample_rate,
        );
        let pre_r = Self::channel_auto_preamp_db(
            linked,
            right.map_or(0.0, |c| c.preamp_db),
            right.map_or(&[][..], |c| c.bands.as_slice()),
            sample_rate,
        );
        pre_l.min(pre_r)
    }

    fn channel_auto_preamp_db(
        linked_bands: &[Band],
        section_preamp_db: f64,
        section_bands: &[Band],
        sample_rate: f64,
    ) -> f64 {
        let nyquist = 0.5 * sample_rate;
        if !sample_rate.is_finite() || nyquist <= 10.0 {
            return 0.0;
        }
        let bands: Vec<Coeffs> = linked_bands
            .iter()
            .chain(section_bands.iter())
            .map(|b| Coeffs::design(b, sample_rate))
            .filter(|c| !c.is_identity())
            .collect();
        let section_gain = crate::db_to_lin(section_preamp_db);
        if bands.is_empty() {
            let preamp = (-section_preamp_db).min(0.0);
            return if preamp > -1e-9 || !preamp.is_finite() {
                0.0
            } else {
                preamp
            };
        }

        // Dense log grid (16384 points) plus DC and every band centre, so
        // that a high-Q peak (or a low shelf's DC plateau) cannot slip
        // between grid points.
        const GRID: usize = 16_384;
        let mut freqs = Vec::with_capacity(GRID + bands.len() + 1);
        freqs.push(0.0);
        let u_end = (nyquist / 10.0).ln();
        for i in 0..GRID {
            let u = u_end * (i as f64) / ((GRID - 1) as f64);
            freqs.push(10.0 * u.exp());
        }
        for b in linked_bands.iter().chain(section_bands.iter()) {
            if b.enabled && b.freq_hz > 10.0 && b.freq_hz < nyquist {
                freqs.push(b.freq_hz);
            }
        }

        let mut peak: f64 = 0.0;
        for f in freqs {
            let mut mag = section_gain;
            for c in &bands {
                mag *= c.response(f, sample_rate).norm();
            }
            if mag.is_finite() && mag > peak {
                peak = mag;
            }
        }
        if peak <= 0.0 || !peak.is_finite() {
            return 0.0;
        }
        let preamp = (-crate::lin_to_db(peak)).min(0.0);
        // Treat numerical dust as "no preamp needed".
        if preamp > -1e-9 { 0.0 } else { preamp }
    }
}

impl Stage for Eq {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        let nch = chans.len().min(self.channels);
        for (ch, buf) in chans.iter_mut().enumerate().take(nch) {
            let filter = match ch {
                0 => &self.left,
                1 => &self.right,
                _ => &self.linked,
            };
            if filter.preamp != 1.0 {
                for x in buf.iter_mut() {
                    *x *= filter.preamp;
                }
            }
            for (band, st) in filter.bands.iter().zip(self.states[ch].iter_mut()) {
                for x in buf.iter_mut() {
                    *x = st.step(band, *x);
                }
            }
        }
    }

    fn reset(&mut self) {
        for ch in &mut self.states {
            for st in ch.iter_mut() {
                *st = BiquadState::default();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{Band, EqProfile, FilterType};
    use rustfft::FftPlanner;

    fn band(kind: FilterType, freq_hz: f64, gain_db: f64, q: f64) -> Band {
        Band {
            kind,
            freq_hz,
            gain_db,
            q,
            enabled: true,
        }
    }

    fn to_db(c: Complex64) -> f64 {
        20.0 * c.norm().log10()
    }

    #[test]
    fn peaking_gain_and_shelf_limits() {
        let fs = 48_000.0;
        for gain in [-12.0, -3.0, 3.0, 6.0, 12.0] {
            let c = Coeffs::design(&band(FilterType::Peaking, 1000.0, gain, 1.0), fs);
            assert!(
                (to_db(c.response(1000.0, fs)) - gain).abs() < 1e-9,
                "gain {gain}"
            );
            // Peaking filters are unity at DC and at Nyquist.
            assert!(to_db(c.response(0.0, fs)).abs() < 1e-9);
            assert!(to_db(c.response(fs / 2.0, fs)).abs() < 1e-9);
        }

        let ls = Coeffs::design(
            &band(FilterType::LowShelf, 200.0, 6.0, 1.0 / 2f64.sqrt()),
            fs,
        );
        assert!((to_db(ls.response(0.0, fs)) - 6.0).abs() < 1e-9);
        assert!(to_db(ls.response(fs / 2.0, fs)).abs() < 1e-9);
        assert!((to_db(ls.response(200.0, fs)) - 3.0).abs() < 1e-9);

        let hs = Coeffs::design(
            &band(FilterType::HighShelf, 8000.0, -6.0, 1.0 / 2f64.sqrt()),
            fs,
        );
        assert!((to_db(hs.response(fs / 2.0, fs)) + 6.0).abs() < 1e-9);
        assert!(to_db(hs.response(0.0, fs)).abs() < 1e-9);
        assert!((to_db(hs.response(8000.0, fs)) + 3.0).abs() < 1e-9);
    }

    #[test]
    fn standard_filter_shapes() {
        let fs = 48_000.0;
        let f0 = 1000.0;
        let lp = Coeffs::design(&band(FilterType::LowPass, f0, 0.0, 1.0 / 2f64.sqrt()), fs);
        let hp = Coeffs::design(&band(FilterType::HighPass, f0, 0.0, 1.0 / 2f64.sqrt()), fs);
        assert!((to_db(lp.response(f0, fs)) + 3.010_299_956_639_812).abs() < 1e-9);
        assert!((to_db(hp.response(f0, fs)) + 3.010_299_956_639_812).abs() < 1e-9);
        assert!(to_db(lp.response(0.0, fs)).abs() < 1e-12);
        assert!(lp.response(fs / 2.0, fs).norm() < 1e-12);
        assert!(hp.response(0.0, fs).norm() < 1e-12);
        assert!(to_db(hp.response(fs / 2.0, fs)).abs() < 1e-12);

        let bp = Coeffs::design(&band(FilterType::BandPass, f0, 0.0, 4.0), fs);
        assert!(to_db(bp.response(f0, fs)).abs() < 1e-9);
        assert!(bp.response(0.0, fs).norm() < 1e-12);
        let notch = Coeffs::design(&band(FilterType::Notch, f0, 0.0, 4.0), fs);
        assert!(notch.response(f0, fs).norm() < 1e-12);
        assert!(to_db(notch.response(100.0, fs)).abs() < 0.01);

        let ap = Coeffs::design(&band(FilterType::AllPass, f0, 0.0, 2.0), fs);
        for f in [20.0, 100.0, 500.0, 1000.0, 5000.0, 20_000.0] {
            assert!((ap.response(f, fs).norm() - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn identity_for_disabled_and_out_of_band() {
        let fs = 48_000.0;
        let mut b = band(FilterType::Peaking, 1000.0, 6.0, 1.0);
        assert_ne!(
            Coeffs::design(&b, fs),
            Coeffs::IDENTITY,
            "enabled band must not be identity"
        );
        b.enabled = false;
        assert_eq!(Coeffs::design(&b, fs), Coeffs::IDENTITY);

        let at_nyq = band(FilterType::Peaking, 24_000.0, 6.0, 1.0);
        assert_eq!(Coeffs::design(&at_nyq, fs), Coeffs::IDENTITY);
        let above_nyq = band(FilterType::Peaking, 30_000.0, 6.0, 1.0);
        assert_eq!(Coeffs::design(&above_nyq, fs), Coeffs::IDENTITY);
        let zero = band(FilterType::Peaking, 0.0, 6.0, 1.0);
        assert_eq!(Coeffs::design(&zero, fs), Coeffs::IDENTITY);
        let bad_q = band(FilterType::Peaking, 1000.0, 6.0, 0.0);
        assert_eq!(Coeffs::design(&bad_q, fs), Coeffs::IDENTITY);
        let nan = band(FilterType::Peaking, f64::NAN, 6.0, 1.0);
        assert_eq!(Coeffs::design(&nan, fs), Coeffs::IDENTITY);
    }

    /// Naive direct-form-I difference equation, used as the reference.
    fn direct_form_i(c: &Coeffs, x: &[f64]) -> Vec<f64> {
        let mut y = vec![0.0; x.len()];
        let (mut x1, mut x2, mut y1, mut y2) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for (n, &xn) in x.iter().enumerate() {
            let yn = c.b0 * xn + c.b1 * x1 + c.b2 * x2 - c.a1 * y1 - c.a2 * y2;
            y[n] = yn;
            x2 = x1;
            x1 = xn;
            y2 = y1;
            y1 = yn;
        }
        y
    }

    #[test]
    fn impulse_response_matches_direct_form() {
        let fs = 44_100.0;
        for (kind, f0, gain, q) in [
            (FilterType::Peaking, 1000.0, 9.0, 5.0),
            (FilterType::LowShelf, 120.0, -8.0, 0.9),
            (
                FilterType::HighShelf,
                6000.0,
                4.0,
                std::f64::consts::FRAC_1_SQRT_2,
            ),
            (FilterType::LowPass, 250.0, 0.0, 1.8),
            (FilterType::HighPass, 80.0, 0.0, 0.6),
        ] {
            let c = Coeffs::design(&band(kind, f0, gain, q), fs);
            let mut profile = EqProfile {
                preamp_db: 0.0,
                ..Default::default()
            };
            profile.bands.push(band(kind, f0, gain, q));
            let mut eq = Eq::new(&profile, fs, 1);
            let mut x = vec![0.0; 512];
            x[0] = 1.0;
            let reference = direct_form_i(&c, &x);
            let mut ch = x.clone();
            eq.process(&mut [&mut ch]);
            let worst = ch
                .iter()
                .zip(&reference)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(worst < 1e-12, "{kind:?}: {worst:e}");
        }
    }

    #[test]
    fn transposed_df2_matches_df1_at_extreme_low_frequency() {
        // 20 Hz, Q=10 at 768 kHz: the poles and zeros sit within ~1e-5 of the
        // unit circle and of each other.  TDF-II is not measurably worse than
        // Direct Form I in f64.
        let fs = 768_000.0;
        let b = band(FilterType::Peaking, 20.0, 12.0, 10.0);
        let c = Coeffs::design(&b, fs);
        assert!((to_db(c.response(20.0, fs)) - 12.0).abs() < 1e-9);

        let mut profile = EqProfile::default();
        profile.bands.push(b);
        let mut eq = Eq::new(&profile, fs, 1);
        let n = 200_000;
        let mut impulse = vec![0.0; n];
        impulse[0] = 1.0;
        let reference = direct_form_i(&c, &impulse);
        let mut processed = impulse.clone();
        eq.process(&mut [&mut processed]);

        let peak = reference.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let worst = processed
            .iter()
            .zip(&reference)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        // ~-225 dB relative to the impulse-response peak.
        assert!(worst / peak < 1e-10, "worst {worst:e}, peak {peak}");
    }

    #[test]
    fn response_matches_fft_of_impulse_response() {
        let fs = 48_000.0;
        let mut profile = EqProfile {
            preamp_db: -3.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 1000.0, 6.0, 2.0));
        profile.bands.push(band(
            FilterType::HighShelf,
            6000.0,
            -4.0,
            std::f64::consts::FRAC_1_SQRT_2,
        ));
        profile.bands.push(band(
            FilterType::LowShelf,
            100.0,
            3.0,
            std::f64::consts::FRAC_1_SQRT_2,
        ));

        let n = 16_384;
        let mut eq = Eq::new(&profile, fs, 1);
        let mut impulse = vec![0.0; n];
        impulse[0] = 1.0;
        eq.process(&mut [&mut impulse]);

        let mut spectrum: Vec<Complex64> =
            impulse.iter().map(|&v| Complex64::new(v, 0.0)).collect();
        FftPlanner::new().plan_fft_forward(n).process(&mut spectrum);

        let mut worst: f64 = 0.0;
        for (k, &bin) in spectrum.iter().enumerate().take(n / 2).skip(1) {
            let f = k as f64 * fs / n as f64;
            let exact = eq.response(f);
            let diff = (bin - exact).norm() / exact.norm().max(1e-12);
            worst = worst.max(diff);
        }
        assert!(worst < 1e-6, "FFT/analytic mismatch {worst:e}");
    }

    #[test]
    fn eq_applies_preamp_once() {
        let fs = 48_000.0;
        let mut profile = EqProfile {
            preamp_db: -6.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 1000.0, 3.0, 1.0));
        let eq = Eq::new(&profile, fs, 1);
        let expected = 10f64.powf(-6.0 / 20.0) * 10f64.powf(3.0 / 20.0);
        assert!((eq.response(1000.0).norm() - expected).abs() < 1e-12);
    }

    #[test]
    fn auto_preamp_tracks_largest_boost() {
        let fs = 48_000.0;
        // A +6 dB peak: the preamp should be about -6 dB.
        let mut profile = EqProfile {
            preamp_db: 0.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 1000.0, 6.0, 2.0));
        let pre = Eq::auto_preamp_db(&profile, fs);
        assert!((pre + 6.0).abs() < 0.02, "{pre}");

        // Narrow high-Q band: the grid must not miss the peak.
        let mut profile = EqProfile {
            preamp_db: 0.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 3000.0, 10.0, 20.0));
        let pre = Eq::auto_preamp_db(&profile, fs);
        assert!((pre + 10.0).abs() < 0.02, "{pre}");

        // The profile's own preamp is ignored.
        let mut profile = EqProfile {
            preamp_db: -12.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 1000.0, 6.0, 1.0));
        assert!((Eq::auto_preamp_db(&profile, fs) + 6.0).abs() < 0.02);

        // Cuts only: no attenuation needed.
        let mut profile = EqProfile {
            preamp_db: 0.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 1000.0, -6.0, 1.0));
        assert_eq!(Eq::auto_preamp_db(&profile, fs), 0.0);

        // Disabled bands are ignored.
        let mut profile = EqProfile {
            preamp_db: 0.0,
            ..Default::default()
        };
        let mut b = band(FilterType::Peaking, 1000.0, 12.0, 1.0);
        b.enabled = false;
        profile.bands.push(b);
        assert_eq!(Eq::auto_preamp_db(&profile, fs), 0.0);

        assert_eq!(Eq::auto_preamp_db(&EqProfile::default(), fs), 0.0);
    }

    #[test]
    fn process_is_channel_independent_and_in_place() {
        let fs = 48_000.0;
        let mut profile = EqProfile {
            preamp_db: -2.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 500.0, 5.0, 1.5));
        let mut eq = Eq::new(&profile, fs, 2);
        let mut left: Vec<f64> = (0..128).map(|i| (i as f64 * 0.1).sin()).collect();
        let mut right = vec![0.0; 128];
        let left_orig = left.clone();
        eq.process(&mut [&mut left, &mut right]);
        // Channel 2 is silent and must stay silent.
        assert!(right.iter().all(|&v| v == 0.0));
        // Re-processing a fresh copy gives the same first channel.
        let mut eq2 = Eq::new(&profile, fs, 2);
        let mut l2 = left_orig;
        let mut r2 = vec![0.0; 128];
        eq2.process(&mut [&mut l2, &mut r2]);
        assert_eq!(left, l2);

        // Zero-length process must be a no-op and must not panic.
        let mut empty: [&mut [f64]; 0] = [];
        eq.process(&mut empty);
        let mut zero = [0.0f64; 0];
        eq.process(&mut [&mut zero]);
    }

    #[test]
    fn layered_eq_routes_linked_left_right_and_extra_channels() {
        let fs = 48_000.0;
        let shared = band(FilterType::Peaking, 1000.0, 3.0, 2.0);
        let left_only_band = band(FilterType::Peaking, 138.0, -4.0, 3.0);
        let profile = EqProfile {
            name: "layered".into(),
            preamp_db: -3.0,
            bands: vec![shared.clone()],
            left: Some(audivane_protocol::ChannelEq {
                enabled: true,
                preamp_db: 0.0,
                bands: vec![left_only_band.clone()],
            }),
            right: None,
            ..Default::default()
        };

        let mut eq = Eq::new(&profile, fs, 4);
        // L-only band affects ch0 only; ch1, ch2, ch3 receive linked + preamp only.
        assert_ne!(eq.response_channel(0, 138.0), eq.response_channel(1, 138.0));
        assert_eq!(eq.response(138.0), eq.response_channel(0, 138.0));
        assert_eq!(eq.response_channel(2, 138.0), eq.response_channel(1, 138.0));
        assert_eq!(eq.response_channel(3, 138.0), eq.response_channel(1, 138.0));

        // ch0 must match an equivalent flat cascade [shared, left_only_band] with preamp -3.0.
        let flat_ch0 = EqProfile {
            preamp_db: -3.0,
            bands: vec![shared.clone(), left_only_band],
            ..Default::default()
        };
        let flat_linked = EqProfile {
            preamp_db: -3.0,
            bands: vec![shared],
            ..Default::default()
        };

        let src: Vec<f64> = (0..256).map(|i| (i as f64 * 0.13).sin() * 0.5).collect();
        let mut c0 = src.clone();
        let mut c1 = src.clone();
        let mut c2 = src.clone();
        let mut c3 = src.clone();
        eq.process(&mut [&mut c0, &mut c1, &mut c2, &mut c3]);

        let mut eq_ch0_ref = Eq::new(&flat_ch0, fs, 1);
        let mut ref_ch0 = src.clone();
        eq_ch0_ref.process(&mut [&mut ref_ch0]);

        let mut eq_linked_ref = Eq::new(&flat_linked, fs, 1);
        let mut ref_linked = src;
        eq_linked_ref.process(&mut [&mut ref_linked]);

        assert_eq!(c0, ref_ch0);
        assert_eq!(c1, ref_linked);
        assert_eq!(c2, ref_linked);
        assert_eq!(c3, ref_linked);
        assert_ne!(c0, c1);
    }

    #[test]
    fn disabled_sections_are_bit_identical_to_absent() {
        let fs = 48_000.0;
        let src: Vec<f64> = (0..256).map(|i| (i as f64 * 0.19).cos() * 0.4).collect();

        // Profile with all sections populated but disabled, leaving only global preamp.
        let with_disabled = EqProfile {
            name: "disabled".into(),
            preamp_db: -2.0,
            linked_enabled: false,
            bands: vec![band(FilterType::Peaking, 1000.0, 6.0, 1.5)],
            left: Some(audivane_protocol::ChannelEq {
                enabled: false,
                preamp_db: -4.0,
                bands: vec![band(FilterType::LowShelf, 120.0, 5.0, 0.7)],
            }),
            right: Some(audivane_protocol::ChannelEq {
                enabled: false,
                preamp_db: 3.0,
                bands: vec![band(FilterType::HighShelf, 6000.0, -4.0, 0.7)],
            }),
            ..Default::default()
        };
        let absent = EqProfile {
            name: "absent".into(),
            preamp_db: -2.0,
            ..Default::default()
        };

        let mut eq_dis = Eq::new(&with_disabled, fs, 3);
        let mut eq_abs = Eq::new(&absent, fs, 3);

        let mut d0 = src.clone();
        let mut d1 = src.clone();
        let mut d2 = src.clone();
        let mut a0 = src.clone();
        let mut a1 = src.clone();
        let mut a2 = src;
        eq_dis.process(&mut [&mut d0, &mut d1, &mut d2]);
        eq_abs.process(&mut [&mut a0, &mut a1, &mut a2]);
        assert_eq!(d0, a0);
        assert_eq!(d1, a1);
        assert_eq!(d2, a2);
    }

    #[test]
    fn legacy_migrated_profile_matches_old_independent_behaviour() {
        let fs = 48_000.0;
        // Post-migration shape of a legacy split profile (preamp_db: -3, bands: [...], right: Some(-6, [...])):
        // global preamp = 0, linked bands = [], left = old left, right = old right.
        let migrated = EqProfile {
            name: "legacy-split".into(),
            preamp_db: 0.0,
            linked_enabled: true,
            bands: Vec::new(),
            left: Some(audivane_protocol::ChannelEq {
                enabled: true,
                preamp_db: -3.0,
                bands: vec![band(FilterType::Peaking, 1000.0, 6.0, 2.0)],
            }),
            right: Some(audivane_protocol::ChannelEq {
                enabled: true,
                preamp_db: -6.0,
                bands: vec![band(FilterType::Peaking, 250.0, -4.0, 1.5)],
            }),
            ..Default::default()
        };
        let mut eq = Eq::new(&migrated, fs, 2);

        let old_left = EqProfile {
            preamp_db: -3.0,
            bands: vec![band(FilterType::Peaking, 1000.0, 6.0, 2.0)],
            ..Default::default()
        };
        let old_right = EqProfile {
            preamp_db: -6.0,
            bands: vec![band(FilterType::Peaking, 250.0, -4.0, 1.5)],
            ..Default::default()
        };

        let src: Vec<f64> = (0..256).map(|i| (i as f64 * 0.17).sin() * 0.5).collect();
        let mut c0 = src.clone();
        let mut c1 = src.clone();
        eq.process(&mut [&mut c0, &mut c1]);

        let mut ref_l = src.clone();
        let mut ref_r = src;
        Eq::new(&old_left, fs, 1).process(&mut [&mut ref_l]);
        Eq::new(&old_right, fs, 1).process(&mut [&mut ref_r]);

        assert_eq!(c0, ref_l);
        assert_eq!(c1, ref_r);
    }

    #[test]
    fn auto_preamp_uses_combined_channel_responses() {
        let fs = 48_000.0;
        // Linked +3 dB @ 1 kHz + Left +4 dB @ 1 kHz => ch0 combined peak ~ +7 dB => ~-7 dB.
        let combined = EqProfile {
            name: "combined-boost".into(),
            bands: vec![band(FilterType::Peaking, 1000.0, 3.0, 1.5)],
            left: Some(audivane_protocol::ChannelEq {
                enabled: true,
                preamp_db: 1.0,
                bands: vec![band(FilterType::Peaking, 1000.0, 4.0, 1.5)],
            }),
            right: Some(audivane_protocol::ChannelEq {
                enabled: false,
                preamp_db: 12.0,
                bands: vec![band(FilterType::Peaking, 2000.0, 12.0, 2.0)],
            }),
            ..Default::default()
        };
        let pre = Eq::auto_preamp_db(&combined, fs);
        assert!((pre + 8.0).abs() < 0.05, "expected ~-8 dB, got {pre}");

        // Disabling left drops back to linked +3 dB only.
        let mut left_off = combined.clone();
        left_off.left.as_mut().unwrap().enabled = false;
        let pre_linked = Eq::auto_preamp_db(&left_off, fs);
        assert!(
            (pre_linked + 3.0).abs() < 0.03,
            "expected ~-3 dB, got {pre_linked}"
        );

        // Disabling linked section too leaves zero active boost.
        left_off.linked_enabled = false;
        assert_eq!(Eq::auto_preamp_db(&left_off, fs), 0.0);
    }
}
