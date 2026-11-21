//! Harmonic distortion ("tube" / THD) waveshaper stage.
//!
//! Placed **after** the upsampler (at the output sample rate) and before
//! volume/quantizer, so low-order harmonics (2nd–5th) sit well below Nyquist
//! when upsampling is active. The shaper is a sum of Chebyshev polynomials with
//! their static offset removed (so 0 maps to 0), which makes `a_k` the exact
//! k-th harmonic amplitude relative to the fundamental at full scale.
//!
//! Only the even part can create DC, so only it goes through the 5 Hz
//! first-order DC blocker; the odd part carries the music unfiltered. The stage
//! multiplies by `s = min(1, 1/B)`, with `B` a certified bound of its output
//! over `|x| <= 1`, so inputs within full scale never exceed it. Samples beyond
//! full scale (inter-sample overs from the upsampler) are shaped at `±1` and
//! the excess passes with unity slope, so the quantizer still sees and counts
//! them.
//!
//! Level change: a full-scale fundamental is scaled by `s`; the small-signal
//! gain is `s (1 - 3 a3 + 5 a5)` (`T3'(0) = -3`, `T5'(0) = 5`).

use crate::loudness::LufsMeter;
use crate::{Stage, db_to_lin, lin_to_db};
use audivane_protocol::{HARMONICS_MAX_DB, HARMONICS_MIN_DB, HarmonicsConfig};
use std::f64::consts::PI;
use std::fmt;

const DC_BLOCK_HZ: f64 = 5.0;
/// Rate of the loudness simulation ([`Harmonics::loudness_change_db`]).
const LOUDNESS_REF_RATE: f64 = 48_000.0;
/// Its reference: pink noise at a loud master's RMS, peaks near full scale.
const LOUDNESS_REF_RMS_DB: f64 = -14.0;
/// Seconds of reference, and the DC blocker's settling time left out.
const LOUDNESS_REF_SECONDS: f64 = 4.5;
const LOUDNESS_SKIP_SECONDS: f64 = 0.5;
/// Grid cells for the certified extremum search.
const GRID: usize = 4096;

/// Why a [`Harmonics`] stage could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HarmonicsError {
    ZeroChannels,
    InvalidSampleRate,
    InvalidLevel(String),
}

impl fmt::Display for HarmonicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroChannels => write!(f, "harmonics requires at least 1 channel"),
            Self::InvalidSampleRate => write!(f, "harmonics requires a positive sample rate"),
            Self::InvalidLevel(msg) => write!(f, "harmonics: {msg}"),
        }
    }
}

impl std::error::Error for HarmonicsError {}

/// Chebyshev amplitudes `[a2, a3, a4, a5]` (signed) for a config.
fn amplitudes(config: &HarmonicsConfig) -> Result<[f64; 4], HarmonicsError> {
    let (h2_db, h3_db, h4_db, h5_db) = config.effective_levels_db();
    validate_level("h2_db", h2_db)?;
    validate_level("h3_db", h3_db)?;
    if let Some(h4) = h4_db {
        validate_level("h4_db", h4)?;
    }
    if let Some(h5) = h5_db {
        validate_level("h5_db", h5)?;
    }
    Ok([
        db_to_lin(h2_db),
        // Negative 3rd harmonic: compressive (soft-saturating) odd shape.
        -db_to_lin(h3_db),
        h4_db.map_or(0.0, db_to_lin),
        h5_db.map_or(0.0, db_to_lin),
    ])
}

/// Exact harmonic amplitudes `[DC, H1, H2, H3, H4, H5]` of the static shaper
/// (before scaling and DC removal) for an input `A cos θ`.
pub fn harmonic_amplitudes(a: [f64; 4], level: f64) -> [f64; 6] {
    let [a2, a3, a4, a5] = a;
    let x = level;
    let (x2, x3, x4, x5) = (x * x, x * x * x, x.powi(4), x.powi(5));
    [
        a2 * x2 + a4 * (3.0 * x4 - 4.0 * x2),
        x + a3 * (3.0 * x3 - 3.0 * x) + a5 * (10.0 * x5 - 15.0 * x3 + 5.0 * x),
        a2 * x2 + a4 * (4.0 * x4 - 4.0 * x2),
        a3 * x3 + a5 * (5.0 * x5 - 5.0 * x3),
        a4 * x4,
        a5 * x5,
    ]
}

/// Polynomial in power form, `c[m] x^m`, m = 0..=5.
type Poly = [f64; 6];

fn eval(c: &Poly, x: f64) -> f64 {
    ((((c[5] * x + c[4]) * x + c[3]) * x + c[2]) * x + c[1]) * x + c[0]
}

/// Certified `(lower bound of min, upper bound of max)` of `c` on `[-1, 1]`:
/// on each grid cell `f` lies within `h²/8 · max|f''|` of its chord.
fn certified_range(c: &Poly) -> (f64, f64) {
    let h = 2.0 / GRID as f64;
    let m2: f64 = (2..6).map(|m| c[m].abs() * (m * (m - 1)) as f64).sum();
    let margin = h * h / 8.0 * m2;
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for i in 0..=GRID {
        let y = eval(c, -1.0 + h * i as f64);
        lo = lo.min(y);
        hi = hi.max(y);
    }
    (lo - margin, hi + margin)
}

fn add(a: &Poly, b: &Poly, kb: f64) -> Poly {
    std::array::from_fn(|i| a[i] + kb * b[i])
}

/// Static harmonic shaper with DC blocker on the even part.
#[derive(Clone, Debug)]
pub struct Harmonics {
    enabled: bool,
    channels: usize,
    amps: [f64; 4],
    /// Odd part `o(x)`, scaled by `scale`.
    odd: Poly,
    /// Even part `e(x)`, scaled by `scale`.
    even: Poly,
    scale: f64,
    dc_block: bool,
    hpf_b0: f64,
    hpf_a1: f64,
    dc_state: Vec<f64>,
}

impl Harmonics {
    /// Build a harmonic waveshaper at `output_rate` for `channels`.
    pub fn try_new(
        config: &HarmonicsConfig,
        output_rate: f64,
        channels: usize,
    ) -> Result<Self, HarmonicsError> {
        if channels == 0 {
            return Err(HarmonicsError::ZeroChannels);
        }
        if !output_rate.is_finite() || output_rate <= 0.0 {
            return Err(HarmonicsError::InvalidSampleRate);
        }
        let amps = amplitudes(config)?;
        let [a2, a3, a4, a5] = amps;
        // o(x) = x + a3 (4x³ - 3x) + a5 (16x⁵ - 20x³ + 5x)
        let odd: Poly = [
            0.0,
            1.0 - 3.0 * a3 + 5.0 * a5,
            0.0,
            4.0 * a3 - 20.0 * a5,
            0.0,
            16.0 * a5,
        ];
        // e(x) = a2 (2x²) + a4 (8x⁴ - 8x²)
        let even: Poly = [0.0, 0.0, 2.0 * a2 - 8.0 * a4, 0.0, 8.0 * a4, 0.0];

        // 1st-order HPF at DC_BLOCK_HZ via bilinear transform:
        // H(z) = b0 (1 - z^-1) / (1 + a1 z^-1).
        let fc = DC_BLOCK_HZ.min(0.1 * output_rate);
        let k = (PI * fc / output_rate).tan();
        let hpf_b0 = 1.0 / (1.0 + k);
        let hpf_a1 = (k - 1.0) / (1.0 + k);

        let bound = if config.dc_block {
            let (e_lo, e_hi) = certified_range(&even);
            let (s_lo, s_hi) = certified_range(&add(&odd, &even, hpf_b0));
            (s_hi - hpf_b0 * e_lo).max(hpf_b0 * e_hi - s_lo)
        } else {
            let (lo, hi) = certified_range(&add(&odd, &even, 1.0));
            hi.max(-lo)
        };
        let scale = if bound > 1.0 { 1.0 / bound } else { 1.0 };

        Ok(Self {
            enabled: config.enabled,
            channels,
            amps,
            odd: odd.map(|c| c * scale),
            even: even.map(|c| c * scale),
            scale,
            dc_block: config.dc_block,
            hpf_b0,
            hpf_a1,
            dc_state: vec![0.0; channels],
        })
    }

    /// Signed Chebyshev amplitudes `[a2, a3, a4, a5]`.
    pub fn amplitudes(&self) -> [f64; 4] {
        self.amps
    }

    /// Output scale in dB (<= 0) that keeps full-scale input within full
    /// scale; this is also the gain on a full-scale fundamental.
    pub fn peak_trim_db(&self) -> f64 {
        if self.enabled {
            lin_to_db(self.scale)
        } else {
            0.0
        }
    }

    /// Loudness change of the stage on music, dB: the BS.1770 (K-weighted)
    /// loudness of a reference signal after the stage minus before. The
    /// reference is 4 s of deterministic pink noise at -14 dBFS RMS (a loud
    /// master, peaks near full scale), so it counts the anti-clip scale, the
    /// odd terms' compression on loud passages and the added harmonics'
    /// energy. Simulated at 48 kHz (the stage is memoryless apart from its
    /// 5 Hz DC blocker). 0 when off. Off the audio thread: it allocates.
    pub fn loudness_change_db(config: &HarmonicsConfig) -> Result<f64, HarmonicsError> {
        if !config.is_active() {
            return Ok(0.0);
        }
        let mut stage = Self::try_new(config, LOUDNESS_REF_RATE, 1)?;
        let input = loudness_reference();
        let mut output = input.clone();
        stage.process(&mut [output.as_mut_slice()]);
        let skip = (LOUDNESS_SKIP_SECONDS * LOUDNESS_REF_RATE) as usize;
        let lufs = |x: &[f64]| {
            let mut meter = LufsMeter::new(LOUDNESS_REF_RATE, 1);
            meter.push(&[&x[skip..]]);
            meter.integrated_lufs()
        };
        let change = lufs(&output) - lufs(&input);
        Ok(if change.is_finite() { change } else { 0.0 })
    }

    /// Gain for small signals in dB: `s (1 - 3 a3 + 5 a5)`.
    pub fn small_signal_gain_db(&self) -> f64 {
        if self.enabled {
            lin_to_db(self.odd[1])
        } else {
            0.0
        }
    }
}

/// Deterministic pink noise (xorshift white through Paul Kellett's economy
/// filter) at [`LOUDNESS_REF_RMS_DB`].
fn loudness_reference() -> Vec<f64> {
    let n = (LOUDNESS_REF_SECONDS * LOUDNESS_REF_RATE) as usize;
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let (mut b0, mut b1, mut b2) = (0.0, 0.0, 0.0);
    let mut x: Vec<f64> = (0..n)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let w = (seed >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            b0 = 0.99765 * b0 + w * 0.099_046_0;
            b1 = 0.96300 * b1 + w * 0.296_516_4;
            b2 = 0.57000 * b2 + w * 1.052_691_3;
            b0 + b1 + b2 + w * 0.1848
        })
        .collect();
    let rms = (x.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
    let g = db_to_lin(LOUDNESS_REF_RMS_DB) / rms;
    x.iter_mut().for_each(|v| *v *= g);
    x
}

fn validate_level(name: &str, db: f64) -> Result<(), HarmonicsError> {
    if !db.is_finite() || !(HARMONICS_MIN_DB..=HARMONICS_MAX_DB).contains(&db) {
        return Err(HarmonicsError::InvalidLevel(format!(
            "{name} must be between {HARMONICS_MIN_DB} and {HARMONICS_MAX_DB} dB (got {db})"
        )));
    }
    Ok(())
}

impl Stage for Harmonics {
    fn process(&mut self, chans: &mut [&mut [f64]]) {
        if !self.enabled {
            return;
        }
        let nch = chans.len().min(self.channels);
        let (odd, even) = (&self.odd, &self.even);
        let (b0, a1) = (self.hpf_b0, self.hpf_a1);
        let dc_block = self.dc_block;

        for (c, chan) in chans.iter_mut().take(nch).enumerate() {
            let z1 = &mut self.dc_state[c];
            for s in chan.iter_mut() {
                let x = if s.is_finite() { *s } else { 0.0 };
                let xc = x.clamp(-1.0, 1.0);
                let over = (x - xc) * self.scale;
                let o = eval(odd, xc);
                let e = eval(even, xc);
                let e = if dc_block {
                    // TDF-II: y = b0 e + z1; z1 = -b0 e - a1 y.
                    let y = b0 * e + *z1;
                    *z1 = -b0 * e - a1 * y;
                    y
                } else {
                    e
                };
                *s = o + e + over;
            }
        }
    }

    fn reset(&mut self) {
        self.dc_state.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::HarmonicsCharacter;
    use rustfft::FftPlanner;

    fn custom(h2: f64, h3: f64, h4: Option<f64>, h5: Option<f64>) -> HarmonicsConfig {
        HarmonicsConfig {
            enabled: true,
            character: HarmonicsCharacter::Custom,
            h2_db: h2,
            h3_db: h3,
            h4_db: h4,
            h5_db: h5,
            dc_block: true,
        }
    }

    struct Measured {
        /// `[DC, H1..H5]` linear amplitudes (H1 absolute, others absolute).
        amp: [f64; 6],
        peak: f64,
    }

    impl Measured {
        fn dbc(&self, k: usize) -> f64 {
            lin_to_db(self.amp[k] / self.amp[1])
        }
        fn thd_db(&self) -> f64 {
            let p: f64 = (2..6).map(|k| self.amp[k] * self.amp[k]).sum();
            lin_to_db(p.sqrt() / self.amp[1])
        }
    }

    /// Coherent ~1 kHz sine at `amp` peak, measured after 0.5 s of settling.
    fn measure(cfg: &HarmonicsConfig, rate: f64, amp: f64) -> Measured {
        let mut stage = Harmonics::try_new(cfg, rate, 1).unwrap();
        let settle = (rate * 0.5) as usize;
        let n = 1 << 14;
        let bin = ((1000.0 * n as f64) / rate).round() as usize;
        let f = bin as f64 * rate / n as f64;
        let mut buf: Vec<f64> = (0..settle + n)
            .map(|i| amp * (2.0 * PI * f * i as f64 / rate).cos())
            .collect();
        // Odd block sizes.
        let mut i = 0;
        let mut b = 1;
        while i < buf.len() {
            let e = (i + b).min(buf.len());
            stage.process(&mut [&mut buf[i..e]]);
            i = e;
            b = b % 1021 + 31;
        }
        let tail = &buf[settle..];
        let peak = tail.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
        let mut spec: Vec<crate::Complex64> = tail
            .iter()
            .map(|&s| crate::Complex64::new(s, 0.0))
            .collect();
        FftPlanner::new().plan_fft_forward(n).process(&mut spec);
        let mut out = [0.0; 6];
        out[0] = spec[0].re.abs() / n as f64;
        for k in 1..6 {
            out[k] = 2.0 * spec[bin * k].norm() / n as f64;
        }
        Measured { amp: out, peak }
    }

    #[test]
    fn full_scale_levels_are_exact_even_with_strong_odd_terms() {
        // H3 and H5 at -20 dB, with only H2 far below: H1's cross terms must
        // not put the odd levels out.
        let cfg = custom(-120.0, -20.0, None, Some(-20.0));
        let m = measure(&cfg, 192_000.0, 1.0);
        assert!((m.dbc(3) + 20.0).abs() < 0.01, "H3 {}", m.dbc(3));
        assert!((m.dbc(5) + 20.0).abs() < 0.01, "H5 {}", m.dbc(5));

        let cfg = custom(-30.0, -40.0, Some(-50.0), Some(-60.0));
        let m = measure(&cfg, 192_000.0, 1.0);
        for (k, want) in [(2, -30.0), (3, -40.0), (4, -50.0), (5, -60.0)] {
            assert!((m.dbc(k) - want).abs() < 0.01, "H{k}: {}", m.dbc(k));
        }
    }

    #[test]
    fn lower_levels_follow_the_closed_form_including_cross_terms() {
        let rate = 192_000.0;
        for cfg in [
            custom(-30.0, -40.0, Some(-50.0), Some(-60.0)),
            custom(-26.0, -24.0, Some(-22.0), Some(-20.0)),
        ] {
            let stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
            for level_db in [0.0, -6.0, -20.0] {
                let a = db_to_lin(level_db);
                let m = measure(&cfg, rate, a);
                let want = harmonic_amplitudes(stage.amplitudes(), a);
                let s = db_to_lin(stage.peak_trim_db());
                for k in 1..6 {
                    // DC blocker at 5 Hz changes 2-5 kHz by < 1e-5 relative.
                    let err = (m.amp[k] - s * want[k].abs()).abs();
                    assert!(
                        err < 1e-5 * s * want[1].abs() + 1e-12,
                        "{level_db} dBFS H{k}: {} vs {}",
                        m.amp[k],
                        s * want[k].abs()
                    );
                }
                assert!(m.amp[0] < 1e-6, "DC {}", m.amp[0]);
            }
        }
    }

    #[test]
    fn output_stays_within_full_scale_for_worst_case_inputs() {
        let rate = 192_000.0;
        // Worst cases for the DC-blocked even part: H4 at -20 dB on a
        // settled sine and on a settled +1 -> -1 step.
        let cases = [
            custom(-120.0, -120.0, Some(-20.0), None),
            custom(-20.0, -20.0, Some(-20.0), Some(-20.0)),
            custom(-20.0, -120.0, None, None),
            HarmonicsConfig {
                enabled: true,
                character: HarmonicsCharacter::Pentode,
                ..Default::default()
            },
        ];
        for cfg in cases {
            for dc_block in [true, false] {
                let cfg = HarmonicsConfig {
                    dc_block,
                    ..cfg.clone()
                };
                let m = measure(&cfg, rate, 1.0);
                assert!(m.peak <= 1.0, "{cfg:?}: sine peak {}", m.peak);
                // Settled steps both ways, and settled at the even part's
                // minimum (x² = 1/2) then stepped to ±1.
                for (from, to) in [
                    (1.0, -1.0),
                    (-1.0, 1.0),
                    (0.5f64.sqrt(), 1.0),
                    (0.5f64.sqrt(), -1.0),
                    (0.0, 1.0),
                ] {
                    let mut st = Harmonics::try_new(&cfg, rate, 1).unwrap();
                    let mut buf = vec![from; rate as usize];
                    buf.extend(std::iter::repeat_n(to, 64));
                    st.process(&mut [&mut buf]);
                    let peak = buf.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
                    assert!(peak <= 1.0, "{cfg:?}: step {from}->{to} peak {peak}");
                }
            }
        }
    }

    #[test]
    fn bound_is_tight_for_the_h4_review_case() {
        // The guaranteed scale must not be wildly conservative: a settled
        // x²=1/2 -> 1 step should get close to full scale.
        let cfg = custom(-120.0, -120.0, Some(-20.0), None);
        let mut st = Harmonics::try_new(&cfg, 192_000.0, 1).unwrap();
        let mut buf = vec![0.5f64.sqrt(); 192_000];
        buf.push(1.0);
        st.process(&mut [&mut buf]);
        assert!(*buf.last().unwrap() > 0.999, "{}", buf.last().unwrap());
    }

    #[test]
    fn certified_range_contains_the_true_extrema() {
        // T4 - 1 has its minimum -2 at x² = 1/2, between grid points.
        let c: Poly = [0.0, 0.0, -8.0, 0.0, 8.0, 0.0];
        let (lo, hi) = certified_range(&c);
        assert!(lo <= -2.0 && lo > -2.0 - 1e-5, "{lo}");
        assert!((0.0..1e-5).contains(&hi), "{hi}");
    }

    #[test]
    fn small_signal_gain_is_reported() {
        let cfg = custom(-120.0, -20.0, None, None);
        let st = Harmonics::try_new(&cfg, 48_000.0, 1).unwrap();
        // a3 = -0.1: o(x) = 1.3x - 0.4x³, peak 0.9 at x = 1, no trim.
        assert_eq!(st.peak_trim_db(), 0.0);
        assert!((st.small_signal_gain_db() - lin_to_db(1.3)).abs() < 1e-9);
        let mut buf = vec![1e-4];
        let mut st = st;
        st.process(&mut [&mut buf]);
        assert!((buf[0] - 1.3e-4).abs() < 1e-10);
    }

    #[test]
    fn minus_120_db_is_applied_not_off() {
        let st =
            Harmonics::try_new(&custom(-120.0, -120.0, Some(-120.0), None), 48_000.0, 1).unwrap();
        let [a2, a3, a4, a5] = st.amplitudes();
        assert!((a2 - 1e-6).abs() < 1e-18);
        assert!((a3 + 1e-6).abs() < 1e-18);
        assert!((a4 - 1e-6).abs() < 1e-18);
        assert_eq!(a5, 0.0, "None is off");
    }

    #[test]
    fn non_finite_input_does_not_poison_state() {
        let cfg = custom(-30.0, -40.0, Some(-50.0), None);
        let mut st = Harmonics::try_new(&cfg, 48_000.0, 1).unwrap();
        let mut fresh = st.clone();
        let mut a: Vec<f64> = (0..512).map(|i| (i as f64 * 0.05).sin() * 0.8).collect();
        let mut b = a.clone();
        a[7] = f64::NAN;
        a[9] = f64::NEG_INFINITY;
        b[7] = 0.0;
        b[9] = 0.0;
        st.process(&mut [&mut a]);
        fresh.process(&mut [&mut b]);
        assert!(a.iter().all(|x| x.is_finite()));
        assert_eq!(a, b);
    }

    #[test]
    fn overs_pass_with_unity_slope() {
        let cfg = custom(-30.0, -40.0, None, None);
        let mut st = Harmonics::try_new(&cfg, 48_000.0, 1).unwrap();
        let mut ref_st = st.clone();
        let mut a = vec![1.2];
        let mut b = vec![1.0];
        st.process(&mut [&mut a]);
        ref_st.process(&mut [&mut b]);
        let s = db_to_lin(st.peak_trim_db());
        assert!((a[0] - b[0] - 0.2 * s).abs() < 1e-12);
    }

    #[test]
    fn silence_outputs_exact_zero() {
        let cfg = HarmonicsConfig::default();
        let mut stage = Harmonics::try_new(&cfg, 192_000.0, 2).unwrap();
        let mut l = vec![0.0; 256];
        let mut r = vec![0.0; 256];
        stage.process(&mut [&mut l, &mut r]);
        assert!(l.iter().all(|&x| x == 0.0));
        assert!(r.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn triode_and_pentode_presets_have_expected_spectra() {
        let triode = HarmonicsConfig::default();
        let m = measure(&triode, 192_000.0, 1.0);
        assert!((m.dbc(2) + 40.0).abs() < 0.01 && (m.dbc(3) + 58.0).abs() < 0.01);
        let pentode = HarmonicsConfig {
            character: HarmonicsCharacter::Pentode,
            ..Default::default()
        };
        let m = measure(&pentode, 192_000.0, 1.0);
        assert!((m.dbc(3) + 42.0).abs() < 0.01 && (m.dbc(2) + 56.0).abs() < 0.01);
    }

    /// Prints the harmonic levels.
    /// `cargo test -p audivane-dsp-core harmonics::tests::report -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn report() {
        let rate = 192_000.0;
        let configs = [
            (
                "custom -30/-40/-50/-60",
                custom(-30.0, -40.0, Some(-50.0), Some(-60.0)),
            ),
            ("triode", HarmonicsConfig::default()),
            (
                "pentode",
                HarmonicsConfig {
                    character: HarmonicsCharacter::Pentode,
                    ..Default::default()
                },
            ),
            ("custom H4 -20", custom(-120.0, -120.0, Some(-20.0), None)),
            (
                "custom all -20",
                custom(-20.0, -20.0, Some(-20.0), Some(-20.0)),
            ),
        ];
        for (name, cfg) in configs {
            let st = Harmonics::try_new(&cfg, rate, 1).unwrap();
            println!(
                "{name}: trim {:.3} dB, small-signal {:+.3} dB",
                st.peak_trim_db(),
                st.small_signal_gain_db()
            );
            for level in [0.0, -6.0, -20.0] {
                let m = measure(&cfg, rate, db_to_lin(level));
                println!(
                    "  {level:>5} dBFS: H2 {:.2} H3 {:.2} H4 {:.2} H5 {:.2} dBc, THD {:.2} dB, peak {:.4}",
                    m.dbc(2),
                    m.dbc(3),
                    m.dbc(4),
                    m.dbc(5),
                    m.thd_db(),
                    m.peak
                );
            }
        }
    }
}
