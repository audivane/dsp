//! A/B level matching and shared headroom.
//!
//! - **Never boost.** For B's offset `o` (dB that make B as loud as A) the
//!   pair plays `g_A = min(0, −o)` and `g_B = g_A + o`: the louder side is
//!   cut, nothing is raised.
//! - **Shared headroom** `H` cuts both sides by the same amount, so it never
//!   changes the comparison. `auto` sizes it from each side's predicted
//!   worst-case peak gain `P` (sine worst case: EQ `max |H|`, convolution
//!   `max |H|`, the crossfeed bound): `H = max(0, max(g_A + P_A, g_B + P_B))`,
//!   capped at [`AB_HEADROOM_MAX_DB`].
//! - **The analytic offset** integrates each side's power response over a
//!   pink reference with K- (default) or A-weighting
//!   ([`crate::loudness::weighted_offset_db`]). The terms: DC block, EQ (with
//!   tilt and preamp), crossfeed as a stereo stage at correlation
//!   [`crate::crossfeed::MATCHED_CORRELATION`], the convolution IRs' magnitude
//!   (from their FFT) and, when the two sides' upsampler designs differ, the
//!   upsampler's magnitude (from its measured impulse response), and the
//!   harmonics stage's loudness change, simulated on loud pink noise.
//!
//! Everything here runs off the audio thread, when the pair is built.

use crate::Complex64;
use crate::biquad::Eq;
use crate::crossfeed::{Crossfeed, MATCHED_CORRELATION};
use crate::dc_block::DcBlock;
use crate::loudness::{Weighting, weighted_offset_db};
use crate::presets::UpsamplerRuntime;
use crate::{db_to_lin, lin_to_db};
use audivane_protocol::EqProfile;
use audivane_protocol::ipc::{AB_HEADROOM_MAX_DB, AbHeadroom, LevelMatch};
use realfft::RealFftPlanner;

/// How an [`crate::chain::AbChain`] finds B's offset and the shared headroom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AbLevel {
    pub offset: AbOffset,
    pub headroom: AbHeadroom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AbOffset {
    /// Computed from both chains when the pair is built.
    Analytic(Weighting),
    /// A given offset for B, in dB (manual, measured, or 0 for off).
    Fixed(f64),
}

impl AbLevel {
    /// A fixed offset with no shared headroom: B is cut or A is cut by
    /// `|offset_db|`, and nothing else changes.
    pub fn fixed(offset_db: f64) -> Self {
        Self {
            offset: AbOffset::Fixed(offset_db),
            headroom: AbHeadroom::Fixed { db: 0.0 },
        }
    }

    /// The engine's level for an A/B config.
    pub fn from_config(level: &LevelMatch, headroom: AbHeadroom) -> Self {
        let offset = match *level {
            LevelMatch::Analytic { weighting } => AbOffset::Analytic(weighting.into()),
            LevelMatch::Manual { offset_db } | LevelMatch::Measured { offset_db, .. } => {
                AbOffset::Fixed(offset_db)
            }
            LevelMatch::Off => AbOffset::Fixed(0.0),
        };
        Self { offset, headroom }
    }
}

/// The gains an A/B pair applies (all in dB).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AbGains {
    /// B's offset: B plus `offset_db` is as loud as A.
    pub offset_db: f64,
    /// A's level-match gain, <= 0.
    pub gain_a_db: f64,
    /// B's level-match gain, <= 0; `gain_b_db - gain_a_db == offset_db`.
    pub gain_b_db: f64,
    /// Shared headroom cut from both sides, >= 0.
    pub headroom_db: f64,
    /// The predicted worst-case peak gains of A and B that `auto` sized
    /// the headroom from (see [`Self::with_offset`]).
    pub peak_db: [f64; 2],
}

impl AbGains {
    /// Never-boost gains for `offset_db` and the shared headroom for
    /// `headroom`, with each side's predicted peak gain `peak_a_db` /
    /// `peak_b_db` (used by `auto`). A non-finite offset counts as 0.
    pub fn new(offset_db: f64, headroom: AbHeadroom, peak_a_db: f64, peak_b_db: f64) -> Self {
        let offset_db = if offset_db.is_finite() {
            offset_db
        } else {
            0.0
        };
        let gain_a_db = (-offset_db).min(0.0);
        let gain_b_db = gain_a_db + offset_db;
        // `-o + o` can round to a tiny positive number: never above 0 dB.
        let gain_b_db = gain_b_db.min(0.0);
        let headroom_db = match headroom {
            AbHeadroom::Auto => {
                let need = (gain_a_db + peak_a_db).max(gain_b_db + peak_b_db);
                if need.is_finite() { need } else { 0.0 }
            }
            AbHeadroom::Fixed { db } => {
                if db.is_finite() {
                    db
                } else {
                    0.0
                }
            }
        }
        .clamp(0.0, AB_HEADROOM_MAX_DB);
        Self {
            offset_db,
            gain_a_db,
            gain_b_db,
            headroom_db,
            peak_db: [peak_a_db, peak_b_db],
        }
    }

    /// The same pair's gains for another offset: what
    /// [`crate::chain::AbChain::set_level_offset_db`] applies.
    pub fn with_offset(&self, offset_db: f64, headroom: AbHeadroom) -> Self {
        Self::new(offset_db, headroom, self.peak_db[0], self.peak_db[1])
    }

    /// Linear gains `(A, B)` with the headroom folded in.
    pub fn linear(&self) -> (f64, f64) {
        (
            db_to_lin(self.gain_a_db - self.headroom_db),
            db_to_lin(self.gain_b_db - self.headroom_db),
        )
    }
}

/// A magnitude-squared spectrum on a uniform grid, from DC to Nyquist.
#[derive(Clone, Debug)]
pub struct PowerSpectrum {
    power: Vec<f64>,
    bin_hz: f64,
}

impl PowerSpectrum {
    /// Of `taps` at `rate`, scaled by `scale` (linear, on the magnitude).
    /// Zero-padded to at least `rate` points (bins of 1 Hz or finer).
    pub fn of_taps(taps: &[f64], rate: f64, scale: f64) -> Self {
        let n = taps
            .len()
            .max(rate.ceil() as usize)
            .max(2)
            .next_power_of_two();
        let mut planner = RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let mut input = vec![0.0; n];
        input[..taps.len()].copy_from_slice(taps);
        let mut spectrum = vec![Complex64::new(0.0, 0.0); n / 2 + 1];
        // Lengths are the plan's own; the call cannot fail.
        let _ = fft.process(&mut input, &mut spectrum);
        let s2 = scale * scale;
        Self {
            power: spectrum.iter().map(|c| c.norm_sqr() * s2).collect(),
            bin_hz: rate / n as f64,
        }
    }

    /// |H(f)|², linearly interpolated between bins (0 above Nyquist).
    pub fn power_at(&self, f_hz: f64) -> f64 {
        let x = f_hz / self.bin_hz;
        if !x.is_finite() || x < 0.0 {
            return 0.0;
        }
        let i = x.floor() as usize;
        match (self.power.get(i), self.power.get(i + 1)) {
            (Some(&p0), Some(&p1)) => {
                let t = x - i as f64;
                p0 + (p1 - p0) * t
            }
            (Some(&p0), None) => p0,
            _ => 0.0,
        }
    }

    /// Largest |H| at or below `f_max_hz`.
    pub fn peak_below(&self, f_max_hz: f64) -> f64 {
        let last = ((f_max_hz / self.bin_hz).floor() as usize).min(self.power.len() - 1);
        self.power[..=last]
            .iter()
            .fold(0.0_f64, |m, &p| m.max(p))
            .sqrt()
    }
}

/// The impulse response of `rt` (channel 0, an impulse on every channel at
/// input frame 0), normalised to unity DC gain, as a spectrum at the output
/// rate. `rt` is reset afterwards.
pub fn upsampler_spectrum(
    rt: &mut dyn UpsamplerRuntime,
    channels: usize,
    max_block: usize,
    input_rate: f64,
) -> PowerSpectrum {
    let r = rt.ratio();
    let (l, m) = (r.up as usize, r.down as usize);
    let span = rt.impulse_span_out_frames();
    let frames = (span * m).div_ceil(l) + 1;
    let block = max_block.max(1);
    let mut taps = Vec::with_capacity(span + 2 * block * l);
    let mut input = vec![vec![0.0; block]; channels];
    let mut output = vec![Vec::new(); channels];
    let mut pos = 0;
    while pos < frames {
        let n = block.min(frames - pos);
        for chan in &mut input {
            chan[..n].fill(0.0);
            if pos == 0 {
                chan[0] = 1.0;
            }
        }
        let out_n = rt.output_frames(n);
        for chan in &mut output {
            chan.clear();
            chan.resize(out_n, 0.0);
        }
        let ins: Vec<&[f64]> = input.iter().map(|c| &c[..n]).collect();
        let mut outs: Vec<&mut [f64]> = output.iter_mut().map(Vec::as_mut_slice).collect();
        rt.process(&ins, &mut outs);
        taps.extend_from_slice(&output[0]);
        pos += n;
    }
    taps.truncate(span);
    rt.reset();
    let ratio = r.as_f64();
    PowerSpectrum::of_taps(&taps, input_rate * ratio, 1.0 / ratio)
}

/// The stages of one side that the analytic offset and the peak prediction
/// read.
pub struct SideTerms<'a> {
    pub sample_rate: f64,
    pub dc: Option<&'a DcBlock>,
    pub crossfeed: Option<&'a Crossfeed>,
    pub eq: Option<&'a Eq>,
    pub eq_profile: Option<&'a EqProfile>,
    pub conv: ConvTerm,
    /// The upsampler's normalised spectrum, when it is part of the compare.
    pub upsampler: Option<PowerSpectrum>,
    /// The harmonics stage's loudness change on music (linear amplitude,
    /// [`crate::harmonics::Harmonics::loudness_change_db`]); 1 when off.
    /// Flat, so it enters the loudness and not the peak prediction.
    pub harmonics_gain: f64,
}

/// The convolution stage's contribution.
#[derive(Clone, Debug)]
pub enum ConvTerm {
    /// A flat linear gain (none: 1; config-only estimate: the enabled
    /// entries' `gain_db`).
    Gain(f64),
    /// The combined IRs' spectra: one (mono) or one per channel.
    Irs(Vec<PowerSpectrum>),
}

impl ConvTerm {
    /// From combined IRs (already scaled by each entry's `gain_db`).
    pub fn of_irs(irs: &[Vec<f64>], rate: f64) -> Self {
        Self::Irs(
            irs.iter()
                .map(|ir| PowerSpectrum::of_taps(ir, rate, 1.0))
                .collect(),
        )
    }

    /// Channel `c`'s IR (a mono IR serves every channel).
    fn channel(irs: &[PowerSpectrum], c: usize) -> &PowerSpectrum {
        &irs[c.min(irs.len() - 1)]
    }

    /// |H|² of channel `c` at `f`.
    fn power(&self, c: usize, f: f64) -> f64 {
        match self {
            Self::Gain(g) => g * g,
            Self::Irs(irs) if irs.is_empty() => 1.0,
            Self::Irs(irs) => Self::channel(irs, c).power_at(f),
        }
    }

    /// Largest |H| of channel `c` up to `nyquist`.
    fn peak(&self, c: usize, nyquist: f64) -> f64 {
        match self {
            Self::Gain(g) => g.abs(),
            Self::Irs(irs) if irs.is_empty() => 1.0,
            Self::Irs(irs) => Self::channel(irs, c).peak_below(nyquist),
        }
    }
}

impl SideTerms<'_> {
    /// Mean power response of the left and right channels at `f_hz`, each
    /// channel's EQ and IR taken together.
    pub fn power(&self, f_hz: f64) -> f64 {
        let dc = self.dc.map_or(1.0, |d| d.response(f_hz).norm_sqr());
        let xf = self
            .crossfeed
            .map_or(1.0, |x| x.stereo_power_response(f_hz, MATCHED_CORRELATION));
        let up = self.upsampler.as_ref().map_or(1.0, |u| u.power_at(f_hz));
        let channel = |c: usize| {
            let eq = self
                .eq
                .map_or(1.0, |e| e.response_channel(c, f_hz).norm_sqr());
            eq * self.conv.power(c, f_hz)
        };
        let harm = self.harmonics_gain * self.harmonics_gain;
        dc * xf * up * harm * 0.5 * (channel(0) + channel(1))
    }

    /// Predicted worst-case peak gain of the side, in dB (sine worst case,
    /// each stage's bound multiplied; EQ and IR per channel, the louder
    /// channel counts). Negative when the side only attenuates.
    pub fn peak_gain_db(&self) -> f64 {
        let nyquist = 0.5 * self.sample_rate;
        let eq_db = self.eq_profile.map_or([0.0; 2], |p| {
            Eq::channel_peak_gains_db(p, self.sample_rate).unwrap_or([p.preamp_db; 2])
        });
        let channel_db = |c: usize| eq_db[c] + lin_to_db(self.conv.peak(c, nyquist));
        let xf = self.crossfeed.map_or(1.0, Crossfeed::peak_gain_bound);
        let up = self
            .upsampler
            .as_ref()
            .map_or(1.0, |u| u.peak_below(nyquist));
        let total = channel_db(0).max(channel_db(1)) + lin_to_db(xf * up);
        if total.is_finite() { total } else { 0.0 }
    }
}

/// The analytic offset (dB to apply to B) between two sides.
pub fn analytic_offset_db(a: &SideTerms<'_>, b: &SideTerms<'_>, weighting: Weighting) -> f64 {
    weighted_offset_db(&|f| a.power(f), &|f| b.power(f), a.sample_rate, weighting)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_boost_gains() {
        for o in [-6.0, 0.0, 6.0, -0.35, 4.16] {
            let g = AbGains::new(o, AbHeadroom::Fixed { db: 0.0 }, 0.0, 0.0);
            assert!(g.gain_a_db <= 0.0 && g.gain_b_db <= 0.0, "{o}: {g:?}");
            assert_eq!(g.gain_b_db - g.gain_a_db, o, "{o}");
            assert!(g.gain_a_db == 0.0 || g.gain_b_db == 0.0, "{o}: {g:?}");
            let h = AbGains::new(o, AbHeadroom::Fixed { db: 3.0 }, 0.0, 0.0);
            assert_eq!(h.headroom_db, 3.0);
            let (la, lb) = g.linear();
            let (ha, hb) = h.linear();
            let cut = db_to_lin(-3.0);
            assert!((ha / la - cut).abs() < 1e-15 && (hb / lb - cut).abs() < 1e-15);
        }
    }

    #[test]
    fn auto_headroom_follows_the_louder_matched_peak() {
        // B boosts by +6 dB peak and is matched down by 0.67 dB.
        let g = AbGains::new(-0.67, AbHeadroom::Auto, 0.0, 6.0);
        assert_eq!(g.gain_a_db, 0.0);
        assert!((g.headroom_db - 5.33).abs() < 1e-12, "{g:?}");
        // Nothing boosts: no headroom.
        assert_eq!(
            AbGains::new(3.0, AbHeadroom::Auto, -1.0, 0.0).headroom_db,
            0.0
        );
        // Capped.
        assert_eq!(
            AbGains::new(0.0, AbHeadroom::Auto, 0.0, 30.0).headroom_db,
            AB_HEADROOM_MAX_DB
        );
        assert_eq!(
            AbGains::new(0.0, AbHeadroom::Fixed { db: -2.0 }, 0.0, 0.0).headroom_db,
            0.0
        );
    }

    #[test]
    fn spectrum_of_a_delta_is_flat() {
        let s = PowerSpectrum::of_taps(&[0.0, 0.5], 48_000.0, 1.0);
        for f in [0.0, 20.0, 1000.0, 23_999.0] {
            assert!((s.power_at(f) - 0.25).abs() < 1e-12, "{f}");
        }
        assert!((s.peak_below(24_000.0) - 0.5).abs() < 1e-12);
    }
}
