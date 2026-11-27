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

use crate::tilt::{TiltFilter, TiltState};
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
    /// The time constant of the section's slowest pole, frames
    /// (`-1 / ln r`, `r` its radius): how long its start-up rings. Infinite
    /// for a pole on or outside the unit circle.
    pub fn decay_frames(&self) -> f64 {
        let disc = self.a1 * self.a1 - 4.0 * self.a2;
        let r = if disc < 0.0 {
            self.a2.abs().sqrt()
        } else {
            let s = disc.sqrt();
            ((-self.a1 + s) / 2.0)
                .abs()
                .max(((-self.a1 - s) / 2.0).abs())
        };
        pole_decay(r)
    }

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
pub(crate) struct BiquadState {
    z1: f64,
    z2: f64,
}

impl BiquadState {
    /// One TDF-II step:
    /// `y = b0 x + z1; z1 = b1 x - a1 y + z2; z2 = b2 x - a2 y`.
    #[inline]
    pub(crate) fn step(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y
    }

    /// The state for an input `k` times as large (the section is linear).
    pub(crate) fn scaled(self, k: f64) -> Self {
        Self {
            z1: self.z1 * k,
            z2: self.z2 * k,
        }
    }
}

#[derive(Clone)]
struct ChannelFilter {
    preamp: f64,
    bands: Vec<Coeffs>,
}

impl ChannelFilter {
    fn from_layers(
        global_preamp_db: f64,
        linked_bands: &[Band],
        section: Option<(f64, &[Band])>,
        tone: &[Band],
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
        bands.extend(
            tone.iter()
                .map(|b| Coeffs::design(b, sample_rate))
                .filter(|c| !c.is_identity()),
        );
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

/// The slowest decay (time constant, frames) of `f`'s bands from `first`
/// on, and of `tilt`'s sections when `with_tilt`.
fn channel_decay(f: &ChannelFilter, first: usize, tilt: &TiltFilter, with_tilt: bool) -> f64 {
    let bands = f.bands.iter().skip(first).map(Coeffs::decay_frames);
    let tilt = with_tilt.then(|| tilt.decay_frames()).into_iter();
    bands.chain(tilt).fold(0.0, f64::max)
}

/// `-1 / ln r` for a pole of radius `r` (0 for `r` 0, infinite from 1).
pub(crate) fn pole_decay(r: f64) -> f64 {
    if r <= 0.0 {
        0.0
    } else if r >= 1.0 {
        f64::INFINITY
    } else {
        -1.0 / r.ln()
    }
}

/// The filter channel `idx` runs (see [`Eq`]).
fn filter_at<'a>(
    left: &'a ChannelFilter,
    right: &'a ChannelFilter,
    linked: &'a ChannelFilter,
    idx: usize,
) -> &'a ChannelFilter {
    match idx {
        0 => left,
        1 => right,
        _ => linked,
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
    tilt: TiltFilter,
    sample_rate: f64,
    channels: usize,
    /// Index of the first channel this EQ runs (0; a part of
    /// [`Eq::split_channels`] runs one channel of a wider stream).
    first_channel: usize,
    /// `states[channel][band]`.
    states: Vec<Vec<BiquadState>>,
    /// `tilt_states[channel]`.
    tilt_states: Vec<TiltState>,
}

impl Eq {
    /// Second-order sections the busiest channel runs per sample (its
    /// bands and the tilt's sections, a first-order shelf counted as one):
    /// what the stage costs, for CPU admission.
    pub fn sections(&self) -> usize {
        self.states.iter().map(Vec::len).max().unwrap_or(0)
            + self.tilt.shelves.len()
            + self.tilt.corners.len()
    }

    /// Build the EQ, designing its tilt (one or two sections, if any) at
    /// `sample_rate`. A tilt that cannot be designed is an error.
    pub fn new(profile: &EqProfile, sample_rate: f64, channels: usize) -> Result<Self, String> {
        let tilt = TiltFilter::design_stage(
            profile.tilt.as_ref(),
            profile.tilt_high.as_ref(),
            sample_rate,
        )?;
        Ok(Self::with_tilt(profile, tilt, sample_rate, channels))
    }

    /// Band-only EQ (global preamp + linked bands, no tilt, no per-channel
    /// sections). Infallible: biquad design has no error path.
    pub fn from_bands(bands: &[Band], preamp_db: f64, sample_rate: f64, channels: usize) -> Self {
        let profile = EqProfile {
            preamp_db,
            bands: bands.to_vec(),
            ..EqProfile::default()
        };
        Self::with_tilt(
            &profile,
            TiltFilter::identity(sample_rate),
            sample_rate,
            channels,
        )
    }

    fn with_tilt(profile: &EqProfile, tilt: TiltFilter, sample_rate: f64, channels: usize) -> Self {
        let linked_bands: &[Band] = if profile.linked_enabled {
            &profile.bands
        } else {
            &[]
        };
        // The tone knobs' shelves run last on every channel (none at 0/0).
        let tone = profile.tone_bands();
        let linked =
            ChannelFilter::from_layers(profile.preamp_db, linked_bands, None, &tone, sample_rate);
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
        let left = ChannelFilter::from_layers(
            profile.preamp_db,
            linked_bands,
            left_sec,
            &tone,
            sample_rate,
        );
        let right = ChannelFilter::from_layers(
            profile.preamp_db,
            linked_bands,
            right_sec,
            &tone,
            sample_rate,
        );
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
        let tilt_states = (0..channels).map(|_| tilt.new_state()).collect();
        Eq {
            linked,
            left,
            right,
            tilt,
            sample_rate,
            channels,
            first_channel: 0,
            states,
            tilt_states,
        }
    }

    /// One single-channel EQ per channel, each running its channel exactly
    /// as this one does (bit for bit), from this one's state. Off the
    /// real-time thread (it allocates); `self` is left without channels.
    pub fn split_channels(&mut self) -> Vec<Eq> {
        let states = std::mem::take(&mut self.states);
        let tilt_states = std::mem::take(&mut self.tilt_states);
        self.channels = 0;
        states
            .into_iter()
            .zip(tilt_states)
            .enumerate()
            .map(|(c, (st, tilt_st))| Eq {
                linked: self.linked.clone(),
                left: self.left.clone(),
                right: self.right.clone(),
                tilt: self.tilt.clone(),
                sample_rate: self.sample_rate,
                channels: 1,
                first_channel: self.first_channel + c,
                states: vec![st],
                tilt_states: vec![tilt_st],
            })
            .collect()
    }

    /// Carry `from`'s filter state over, channel by channel, for the
    /// sections that run exactly as `from`'s do: the longest identical
    /// start of the cascade (the preamp, then each band's coefficients in
    /// order, then the tilt). A section from the first changed one on
    /// starts fresh, as in a new EQ: a transposed DF-II state is only valid
    /// for the coefficients that made it (copied into another section it
    /// can ring far louder than a fresh start), and a section after a
    /// changed one hears another input. So the output never moves more
    /// than a fresh EQ's would. Returns whether every section was taken
    /// over (the EQ then plays on bit for bit as `from`). Real-time safe.
    pub fn inherit_state(&mut self, from: &Eq) -> bool {
        self.inherit_state_decay(from).0
    }

    /// [`Self::inherit_state`], and the slowest decay of the sections that
    /// start fresh (their time constant, frames: how long their start-up
    /// rings; 0 when none does). A shape that does not match takes nothing
    /// over: every section is fresh. A preamp that changed scales the
    /// sections' input: their state, scaled by the same ratio, is exactly
    /// what they would hold had the new preamp played all along, so they
    /// carry over scaled (the EQ is not whole: the preamp moved).
    pub fn inherit_state_decay(&mut self, from: &Eq) -> (bool, f64) {
        if self.channels != from.channels
            || self.first_channel != from.first_channel
            || self.states.len() != from.states.len()
            || self.tilt_states.len() != from.tilt_states.len()
        {
            return (false, self.decay_from(0, true));
        }
        let tilt_same = self.tilt.same_filter(&from.tilt);
        let mut all = true;
        let mut decay = 0.0f64;
        for ch in 0..self.states.len() {
            let idx = ch + self.first_channel;
            let (df, sf) = (
                filter_at(&self.left, &self.right, &self.linked, idx),
                filter_at(&from.left, &from.right, &from.linked, idx),
            );
            let k = df.preamp / sf.preamp;
            if df.preamp != sf.preamp {
                all = false;
                if !k.is_finite() || k == 0.0 {
                    decay = decay.max(channel_decay(df, 0, &self.tilt, true));
                    continue;
                }
            }
            let k = if df.preamp == sf.preamp { 1.0 } else { k };
            let (dst, src) = (&mut self.states[ch], &from.states[ch]);
            let mut whole = df.bands.len() == sf.bands.len() && dst.len() == src.len();
            let mut taken = 0;
            for (i, (dc, sc)) in df.bands.iter().zip(&sf.bands).enumerate() {
                if dc != sc || i >= dst.len() || i >= src.len() {
                    whole = false;
                    break;
                }
                dst[i] = src[i].scaled(k);
                taken = i + 1;
            }
            if whole && df.bands.len() > taken {
                whole = false;
            }
            let (dt, st) = (&mut self.tilt_states[ch], &from.tilt_states[ch]);
            if whole && tilt_same && dt.same_shape(st) {
                dt.copy_scaled(st, k);
            } else {
                all = false;
                let tilt_fresh = !(whole && tilt_same);
                let first = if whole { df.bands.len() } else { taken };
                decay = decay.max(channel_decay(df, first, &self.tilt, tilt_fresh));
            }
        }
        (all, decay)
    }

    /// The slowest decay of all its sections (frames): how long a fresh
    /// one's start-up rings.
    pub fn decay_frames(&self) -> f64 {
        self.decay_from(0, true)
    }

    /// The slowest decay (frames) of every channel's sections from band
    /// `first` on, the tilt's too when `tilt`.
    fn decay_from(&self, first: usize, tilt: bool) -> f64 {
        [&self.left, &self.right, &self.linked]
            .iter()
            .map(|f| channel_decay(f, first, &self.tilt, tilt))
            .fold(0.0, f64::max)
    }

    /// Returns `Some(effective_end_hz)` if the tilt's `end_hz` was clamped to
    /// `0.45 * sample_rate` at build time.
    pub fn tilt_clamped_end_hz(&self) -> Option<f64> {
        self.tilt.clamped_end_hz
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
        self.filter_for(channel).response(f_hz, self.sample_rate) * self.tilt.response(f_hz)
    }

    /// Preamp (dB, <= 0) that keeps the largest boost of `profile` across channels
    /// from clipping a full-scale sine: minus the peak of |H| over a dense log
    /// grid from 10 Hz to Nyquist, ignoring the profile's global `preamp_db`
    /// while accounting for each enabled section's combined response and tilt.
    /// [`Self::auto_preamp_db`] for a band-only list (no tilt). Infallible.
    pub fn bands_auto_preamp_db(bands: &[Band], sample_rate: f64) -> f64 {
        Self::channel_auto_preamp_db(bands, 0.0, &[], None, sample_rate)
    }

    pub fn auto_preamp_db(profile: &EqProfile, sample_rate: f64) -> Result<f64, String> {
        let [l, r] = Self::section_peaks_db(profile, sample_rate)?;
        Ok(Self::preamp_for_peak(l).min(Self::preamp_for_peak(r)))
    }

    /// Signed peak gain (dB) of each of the left and right channels: the
    /// largest |H| over the same grid as [`Self::auto_preamp_db`], with every
    /// preamp included (global and per channel). Negative when a channel
    /// only attenuates.
    pub fn channel_peak_gains_db(
        profile: &EqProfile,
        sample_rate: f64,
    ) -> Result<[f64; 2], String> {
        Ok(Self::section_peaks_db(profile, sample_rate)?.map(|p| p + profile.preamp_db))
    }

    /// Each channel's peak (dB) without the global preamp.
    fn section_peaks_db(profile: &EqProfile, sample_rate: f64) -> Result<[f64; 2], String> {
        let linked: &[Band] = if profile.linked_enabled {
            &profile.bands
        } else {
            &[]
        };
        let left = profile.left.as_ref().filter(|c| c.enabled);
        let right = profile.right.as_ref().filter(|c| c.enabled);
        // The tone shelves count like bands on both channels.
        let tone = profile.tone_bands();
        let with_tone = |sec: Option<&audivane_protocol::ChannelEq>| -> Vec<Band> {
            sec.map_or(&[][..], |c| c.bands.as_slice())
                .iter()
                .chain(&tone)
                .cloned()
                .collect()
        };
        let left_bands = with_tone(left);
        let right_bands = with_tone(right);
        let tilt = TiltFilter::design_stage(
            profile.tilt.as_ref(),
            profile.tilt_high.as_ref(),
            sample_rate,
        )
        .map(|t| Some(t).filter(|t| !t.is_identity()))?;

        let peak_l = Self::channel_peak_db(
            linked,
            left.map_or(0.0, |c| c.preamp_db),
            &left_bands,
            tilt.as_ref(),
            sample_rate,
        );
        let peak_r = Self::channel_peak_db(
            linked,
            right.map_or(0.0, |c| c.preamp_db),
            &right_bands,
            tilt.as_ref(),
            sample_rate,
        );
        Ok([peak_l, peak_r])
    }

    /// The preamp (dB, <= 0) that brings a peak of `peak_db` to 0 dB.
    fn preamp_for_peak(peak_db: f64) -> f64 {
        let preamp = (-peak_db).min(0.0);
        // Treat numerical dust as "no preamp needed".
        if preamp > -1e-9 || !preamp.is_finite() {
            0.0
        } else {
            preamp
        }
    }

    fn channel_auto_preamp_db(
        linked_bands: &[Band],
        section_preamp_db: f64,
        section_bands: &[Band],
        tilt: Option<&TiltFilter>,
        sample_rate: f64,
    ) -> f64 {
        Self::preamp_for_peak(Self::channel_peak_db(
            linked_bands,
            section_preamp_db,
            section_bands,
            tilt,
            sample_rate,
        ))
    }

    /// Signed peak (dB) of one channel's sections, its preamp included; 0
    /// for a degenerate rate or response.
    fn channel_peak_db(
        linked_bands: &[Band],
        section_preamp_db: f64,
        section_bands: &[Band],
        tilt: Option<&TiltFilter>,
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
        if bands.is_empty() && tilt.is_none() {
            return if section_preamp_db.is_finite() {
                section_preamp_db
            } else {
                0.0
            };
        }

        // Dense log grid (16384 points) plus DC and every band centre, so
        // that a high-Q peak (or a low shelf's DC plateau) cannot slip
        // between grid points. The grid starts at 10 Hz or 3 octaves below
        // the lowest band or tilt section, whichever is lower, and each band
        // centre and tilt corner gets a dense +/-1/2 octave patch (a sharp
        // tilt corner's overshoot sits within a fraction of an octave).
        const GRID: usize = 16_384;
        const PATCH: usize = 128;
        let band_hz: Vec<f64> = linked_bands
            .iter()
            .chain(section_bands.iter())
            .filter(|b| b.enabled && b.freq_hz > 0.0 && b.freq_hz < nyquist)
            .map(|b| b.freq_hz)
            .collect();
        let tilt_hz: Vec<f64> = tilt.map(|t| t.critical_frequencies()).unwrap_or_default();
        let centres: Vec<f64> = band_hz
            .iter()
            .chain(tilt.map_or(&[][..], |t| t.corner_hz.as_slice()))
            .copied()
            .filter(|f| f.is_finite() && *f > 0.0)
            .collect();
        let f_low = band_hz
            .iter()
            .chain(&tilt_hz)
            .filter(|f| f.is_finite() && **f > 0.0)
            .fold(10.0_f64, |lo, &f| lo.min(f / 8.0))
            .max(1e-3);
        let mut freqs = Vec::with_capacity(GRID + 2056 + centres.len() * (PATCH + 1) + 2);
        freqs.push(0.0);
        freqs.push(nyquist);
        // Tilt sections may sit very close to Nyquist; include their corner
        // and centre frequencies and a linear high-frequency grid as well.
        if tilt.is_some() {
            freqs.extend(&tilt_hz);
            freqs.extend((0..=2048).map(|i| nyquist * (0.9 + 0.1 * i as f64 / 2048.0)));
        }
        let u_end = (nyquist / f_low).ln();
        for i in 0..GRID {
            let u = u_end * (i as f64) / ((GRID - 1) as f64);
            freqs.push(f_low * u.exp());
        }
        for &c in &centres {
            freqs.extend(
                (0..=PATCH)
                    .map(|i| c * (i as f64 / PATCH as f64 - 0.5).exp2())
                    .filter(|&f| f < nyquist),
            );
        }
        freqs.extend(band_hz.iter().copied());

        let mut peak: f64 = 0.0;
        for f in freqs {
            let mut mag = section_gain;
            for c in &bands {
                mag *= c.response(f, sample_rate).norm();
            }
            if let Some(t) = tilt {
                mag *= t.response(f).norm();
            }
            if mag.is_finite() && mag > peak {
                peak = mag;
            }
        }
        if peak <= 0.0 || !peak.is_finite() {
            return 0.0;
        }
        crate::lin_to_db(peak)
    }
}

impl Stage for Eq {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        let nch = chans.len().min(self.channels);
        for (ch, buf) in chans.iter_mut().enumerate().take(nch) {
            let filter = match ch + self.first_channel {
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
            if !self.tilt.is_identity() {
                self.tilt.process_channel(buf, &mut self.tilt_states[ch]);
            }
        }
    }

    fn reset(&mut self) {
        for ch in &mut self.states {
            for st in ch.iter_mut() {
                *st = BiquadState::default();
            }
        }
        for st in &mut self.tilt_states {
            st.reset();
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
        let mut eq = Eq::new(&profile, fs, 1).unwrap();
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
        let mut eq = Eq::new(&profile, fs, 1).unwrap();
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
        let pre = Eq::auto_preamp_db(&profile, fs).unwrap();
        assert!((pre + 6.0).abs() < 0.02, "{pre}");

        // Narrow high-Q band: the grid must not miss the peak.
        let mut profile = EqProfile {
            preamp_db: 0.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 3000.0, 10.0, 20.0));
        let pre = Eq::auto_preamp_db(&profile, fs).unwrap();
        assert!((pre + 10.0).abs() < 0.02, "{pre}");

        // The profile's own preamp is ignored.
        let mut profile = EqProfile {
            preamp_db: -12.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 1000.0, 6.0, 1.0));
        assert!((Eq::auto_preamp_db(&profile, fs).unwrap() + 6.0).abs() < 0.02);

        // Cuts only: no attenuation needed.
        let mut profile = EqProfile {
            preamp_db: 0.0,
            ..Default::default()
        };
        profile
            .bands
            .push(band(FilterType::Peaking, 1000.0, -6.0, 1.0));
        assert_eq!(Eq::auto_preamp_db(&profile, fs).unwrap(), 0.0);

        // Disabled bands are ignored.
        let mut profile = EqProfile {
            preamp_db: 0.0,
            ..Default::default()
        };
        let mut b = band(FilterType::Peaking, 1000.0, 12.0, 1.0);
        b.enabled = false;
        profile.bands.push(b);
        assert_eq!(Eq::auto_preamp_db(&profile, fs).unwrap(), 0.0);

        assert_eq!(Eq::auto_preamp_db(&EqProfile::default(), fs).unwrap(), 0.0);
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

        let mut eq_dis = Eq::new(&with_disabled, fs, 3).unwrap();
        let mut eq_abs = Eq::new(&absent, fs, 3).unwrap();

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
    fn flat_tone_is_bit_identical_to_none() {
        let fs = 48_000.0;
        let src: Vec<f64> = (0..512).map(|i| (i as f64 * 0.23).sin() * 0.5).collect();
        let base = EqProfile {
            preamp_db: -1.5,
            bands: vec![band(FilterType::Peaking, 1000.0, 4.0, 1.2)],
            right: Some(audivane_protocol::ChannelEq {
                bands: vec![band(FilterType::LowShelf, 90.0, 2.0, 0.7)],
                ..Default::default()
            }),
            ..Default::default()
        };
        let flat = EqProfile {
            tone: Some(audivane_protocol::ToneConfig::default()),
            ..base.clone()
        };
        let run = |p: &EqProfile| {
            let mut eq = Eq::new(p, fs, 3).unwrap();
            let mut ch = [src.clone(), src.clone(), src.clone()];
            let [a, b, c] = &mut ch;
            eq.process(&mut [a, b, c]);
            ch
        };
        assert_eq!(run(&flat), run(&base));
        assert_eq!(
            Eq::auto_preamp_db(&flat, fs).unwrap(),
            Eq::auto_preamp_db(&base, fs).unwrap()
        );
    }
}
