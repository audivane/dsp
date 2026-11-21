//! Final quantizer: planar f64 → interleaved left-justified i32, with
//! dither applied exactly once.
//!
//! Output is always a left-justified i32 (a 24-bit value occupies the top 24
//! bits). The I/O layer packs it into S16/S24_3 etc.
//!
//! Dither is TPDF of ±1 LSB from two independent uniform draws in [0, 1):
//! `d = u1 + u2 - 1`. Its variance is `2 * (1/12) = 1/6 LSB²`; after rounding
//! to the grid, the quantization error of a zero signal has variance 1/4 LSB²
//! and no DC.
//!
//! # Noise shaping (`DitherKind::Shaped`)
//!
//! A standard error-feedback quantizer in which the *total* error (rounding
//! plus dither) is filtered:
//!
//! ```text
//! v = x - Σ h[k] e[n-1-k],   y = Q(v + d),   e = y - v
//! ```
//!
//! so `y = x + NTF(z) e` with `NTF(z) = 1 - Σ h[k] z^-(k+1)`, and `e` is the
//! white TPDF-dithered quantization error (1/4 LSB²). The filters are 9-tap,
//! minimum phase and chosen per output rate (the table with the largest
//! design rate not above the actual rate):
//!
//! | design rate | filter | in-band F-weighted noise vs TPDF | total noise |
//! |---|---|---|---|
//! | 44.1 kHz | Wannamaker's published 9-tap F-weighted filter \[1\] | -11.4 dB | +18.4 dB |
//! | 48 kHz | the same perceptual target re-derived at 48 kHz | -13.7 dB | +18.1 dB |
//! | 88.2 / 96 kHz | target to 20 kHz, flat above (ultrasonic, gentle) | -25 dB | +10.8 / +10.0 dB |
//! | 176.4 / 192 kHz | same | -32 / -34 dB | +6.0 / +5.9 dB |
//! | below 44.1 kHz | none: plain TPDF | 0 dB | 0 dB |
//!
//! \[1\] R. A. Wannamaker, "Psychoacoustically Optimal Noise Shaping",
//! J. Audio Eng. Soc. 40(7/8), 1992. The other tables come from
//! `tools/design_noise_shapers.py`: the weighting implied by \[1\] (an order-9
//! linear predictor reproduces it exactly) is re-fitted at each rate by
//! Levinson-Durbin. "F-weighted" figures use the inverse of the threshold of
//! hearing (Terhardt's approximation) over 20 Hz..20 kHz; plain A-weighting
//! under-penalises 12–20 kHz, where the audio-band shapers put their noise.
//!
//! The loop is bounded by construction: without clipping `|e| < 1.5 LSB`, so
//! the feedback never exceeds `1.5 Σ|h|` LSB (32 LSB at 44.1 kHz). When the
//! output saturates, the stored error is limited to that same range, so
//! clipping can neither poison nor destabilise the shaper. `None` and `Tpdf`
//! are unchanged; `Tpdf` stays the default.
//!
//! The fixed-coefficient modes (including `HeadroomTpdf`) use per-channel KISS
//! generators and reserve headroom before adding dither. `HighpassTpdf`
//! colours the dither alone; it has no error loop.

use audivane_protocol::{DitherKind, OutputConfig};

mod pcm;

/// Highest channel index tracked individually by [`ClipStats`]. The meter
/// bank reports up to this many channels; further channels are folded into
/// the last slot.
pub const MAX_CLIP_CHANNELS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClipStats {
    pub limiter: audivane_protocol::LimiterStatus,
    /// Samples clamped at input/output, or overflowing a fixed mode's output.
    pub clipped: u64,
    /// Largest |x| seen before quantization (linear).
    pub peak: f64,
    /// Samples clipped per channel (clamped to [`MAX_CLIP_CHANNELS`]).
    pub clipped_ch: [u64; MAX_CLIP_CHANNELS],
    /// Largest |x| per channel before quantization (linear, after the
    /// master volume; above 1.0 is over full scale).
    pub peak_ch: [f64; MAX_CLIP_CHANNELS],
    /// Largest |x| per channel at the chain output before the master volume
    /// (linear). Set by the chain, not the quantizer.
    pub chain_peak_ch: [f64; MAX_CLIP_CHANNELS],
    /// True when the peaks above were measured (a float signal went through
    /// the quantizer); false on paths that copy bytes.
    pub measured: bool,
    /// A/B overs that started in this block, counted to A and B (see
    /// [`crate::chain::AbChain::overs`]). Zero outside A/B.
    pub ab_overs: [f64; 2],
}

impl ClipStats {
    pub fn merge(&mut self, other: ClipStats) {
        self.clipped += other.clipped;
        self.peak = self.peak.max(other.peak);
        for (dst, src) in self.clipped_ch.iter_mut().zip(other.clipped_ch) {
            *dst += src;
        }
        for (dst, src) in self.peak_ch.iter_mut().zip(other.peak_ch) {
            *dst = dst.max(src);
        }
        for (dst, src) in self.chain_peak_ch.iter_mut().zip(other.chain_peak_ch) {
            *dst = dst.max(src);
        }
        self.measured |= other.measured;
        self.ab_overs[0] += other.ab_overs[0];
        self.ab_overs[1] += other.ab_overs[1];
        let events = self.limiter.events.saturating_add(other.limiter.events);
        let reduction = self
            .limiter
            .gain_reduction_db
            .max(other.limiter.gain_reduction_db);
        self.limiter = other.limiter;
        self.limiter.events = events;
        self.limiter.gain_reduction_db = reduction;
    }
}

/// Error-feedback filter length.
const SHAPER_ORDER: usize = 9;

/// Largest magnitude of the total error `y - v` of an unsaturated sample:
/// TPDF (< 1 LSB) plus rounding (<= 0.5 LSB).
const MAX_ERROR_LSB: f64 = 1.5;

/// `(design rate, h)` with `NTF(z) = 1 - Σ h[k] z^-(k+1)`; see the module
/// docs and `tools/design_noise_shapers.py`.
const SHAPERS: [(u32, [f64; SHAPER_ORDER]); 6] = [
    (
        44_100,
        [
            2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847,
        ],
    ),
    (
        48_000,
        [
            2.6135535518237476,
            -3.811034447120704,
            4.303473566379538,
            -3.9818284860457176,
            2.5850991741350366,
            -1.2041253883266165,
            0.3516669899146238,
            0.055502619675983356,
            -0.12074189450049017,
        ],
    ),
    (
        88_200,
        [
            2.45764063391307,
            -1.8522290138595054,
            -0.3047386831628539,
            0.9147174315899357,
            0.23412659469161046,
            -0.6004420928619404,
            -0.2308534410862378,
            0.49862751465030375,
            -0.15750654926729507,
        ],
    ),
    (
        96_000,
        [
            2.3306617295523235,
            -1.4772300953048652,
            -0.5392792175592062,
            0.6720193300541895,
            0.4343871679849845,
            -0.35582915342729965,
            -0.3668353893744987,
            0.27589717732596913,
            -0.005885784296779082,
        ],
    ),
    (
        176_400,
        [
            1.5372169710012846,
            0.08846970098054402,
            -0.47428030851364156,
            -0.4309482169177645,
            -0.10774665629815844,
            0.20172882739233605,
            0.3004117211758287,
            0.12294095218214496,
            -0.2640179629664458,
        ],
    ),
    (
        192_000,
        [
            1.4771435221937659,
            0.1649440707143121,
            -0.41400985184673267,
            -0.4457934042531255,
            -0.17390794820411468,
            0.15025914553992842,
            0.31498172933506813,
            0.18491499307199047,
            -0.27933094099308076,
        ],
    ),
];

/// The error-feedback coefficients `Shaped` uses at `sample_rate` (the table
/// with the largest design rate not above it), or `None` below 44.1 kHz,
/// where `Shaped` falls back to plain TPDF.
pub fn noise_shaper(sample_rate: u32) -> Option<&'static [f64; SHAPER_ORDER]> {
    SHAPERS
        .iter()
        .rev()
        .find(|(rate, _)| *rate <= sample_rate)
        .map(|(_, h)| h)
}

/// xoshiro256++ (Blackman & Vigna, public domain / CC0), seeded through
/// SplitMix64.  Small, fast and good enough for audio dither; no dependency.
#[derive(Clone, Debug)]
struct Rng {
    s: [u64; 4],
}

impl Rng {
    fn new(seed: u64) -> Self {
        let mut z = seed;
        let mut splitmix = move || {
            z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            x ^ (x >> 31)
        };
        Rng {
            s: [splitmix(), splitmix(), splitmix(), splitmix()],
        }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let result = self.s[0]
            .wrapping_add(self.s[3])
            .rotate_left(23)
            .wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1) with 53 bits of precision.
    #[inline]
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0)
    }
}

/// TPDF dither noise of ±1 LSB (two uniform draws), for a path that has no
/// [`Quantizer`]: Direct / bypass when the digital volume is not unity.
/// Allocation-free; a fixed seed makes it reproducible.
pub struct TpdfDither {
    rng: Rng,
    /// Per channel, output frames of dither left since the last non-zero
    /// input sample (see [`Self::keep_dithering`]).
    hold: [u32; TPDF_HOLD_CHANNELS],
}

/// Channels with their own dither hold: as many as the engine runs
/// ([`crate::chain::MAX_CHANNELS`], 512 bytes of counters). A wider stream
/// (bypass does not build a chain to refuse it) shares the last counter.
pub const TPDF_HOLD_CHANNELS: usize = crate::chain::MAX_CHANNELS;

impl TpdfDither {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
            hold: [0; TPDF_HOLD_CHANNELS],
        }
    }

    /// Whether channel `ch` is dithered this output frame: from a non-zero
    /// input sample on, and for `hold_frames` frames after the last one, so
    /// a sparse signal does not switch the noise on and off at the block
    /// rate; after that the channel is digital silence again (a DAC's
    /// auto-mute sees exact zeros). Call once per channel per frame.
    #[inline]
    pub fn keep_dithering(&mut self, ch: usize, nonzero: bool, hold_frames: u32) -> bool {
        let h = &mut self.hold[ch.min(TPDF_HOLD_CHANNELS - 1)];
        if nonzero {
            *h = hold_frames;
            true
        } else if *h > 0 {
            *h -= 1;
            true
        } else {
            false
        }
    }

    /// One sample of noise in LSBs, in (-1, 1).
    #[inline]
    pub fn next_lsb(&mut self) -> f64 {
        self.rng.uniform() + self.rng.uniform() - 1.0
    }
}

pub struct Quantizer {
    bits: u32,
    kind: DitherKind,
    channels: usize,
    seed: u64,
    rng: Rng,
    /// Feedback filter for `Shaped` (`None`: no shaping at this rate).
    shaper: Option<&'static [f64; SHAPER_ORDER]>,
    /// Per channel, the most recent total errors (`[e[n-1], e[n-2], ...]`).
    error: Vec<[f64; SHAPER_ORDER]>,
    pcm: Vec<pcm::Channel>,
}

/// Output rate assumed by [`Quantizer::new`]. The 44.1 kHz shaper keeps its
/// noise below ~20 kHz-equivalent at any higher rate, so it is the safe
/// choice when the rate is unknown.
pub const DEFAULT_SHAPER_RATE: u32 = 44_100;

impl Quantizer {
    /// `cfg.bits` is the target resolution (16..=32). `seed` makes the dither
    /// noise reproducible for tests. `Shaped` uses the 44.1 kHz filter; use
    /// [`Quantizer::with_sample_rate`] when the output rate is known.
    pub fn new(cfg: &OutputConfig, channels: usize, seed: u64) -> Self {
        Self::with_sample_rate(cfg, channels, seed, DEFAULT_SHAPER_RATE)
    }

    /// Like [`Quantizer::new`], with the noise shaper chosen for the output
    /// `sample_rate` (see the module docs).
    pub fn with_sample_rate(
        cfg: &OutputConfig,
        channels: usize,
        seed: u64,
        sample_rate: u32,
    ) -> Self {
        let bits = cfg.bits.clamp(1, 32);
        let shaper = match cfg.dither {
            DitherKind::Shaped => noise_shaper(sample_rate),
            _ => None,
        };
        Quantizer {
            bits,
            kind: cfg.dither,
            channels,
            seed,
            rng: Rng::new(seed),
            shaper,
            error: vec![[0.0; SHAPER_ORDER]; channels],
            pcm: (0..channels)
                .map(|ch| pcm::Channel::new(seed, ch))
                .collect(),
        }
    }

    pub fn kind(&self) -> DitherKind {
        self.kind
    }

    /// Effective bit depth after clamping to 1..=32.
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// The error-feedback coefficients in use (`None` unless `Shaped` at a
    /// rate of 44.1 kHz or more).
    pub fn shaper(&self) -> Option<&'static [f64]> {
        self.shaper.map(|h| &h[..])
    }

    /// `out.len()` must equal `frames * channels`.
    pub fn process(&mut self, input: &[&[f64]], out: &mut [i32]) -> ClipStats {
        self.process_with(input, out, |_| {})
    }

    /// [`Self::process`], calling `on_clip(frame)` for every clipped sample
    /// (once per clipped channel). Allocation-free.
    pub fn process_with(
        &mut self,
        input: &[&[f64]],
        out: &mut [i32],
        mut on_clip: impl FnMut(usize),
    ) -> ClipStats {
        let channels = self.channels.min(input.len());
        let frames = if channels == 0 { 0 } else { input[0].len() };
        debug_assert_eq!(
            out.len(),
            frames * self.channels,
            "out must be frames * channels"
        );

        let scale = 2f64.powi(self.bits as i32 - 1);
        let shift = 32 - self.bits;
        let max_q = (1i64 << (self.bits - 1)) - 1;
        let min_q = -(1i64 << (self.bits - 1));
        let dither = self.kind != DitherKind::None;

        let mut stats = ClipStats {
            measured: frames > 0 && channels > 0,
            ..ClipStats::default()
        };

        for frame in 0..frames {
            for ch in 0..channels {
                let x = input[ch][frame];
                if x.is_finite() {
                    let a = x.abs();
                    stats.peak = stats.peak.max(a);
                    let slot = &mut stats.peak_ch[ch.min(MAX_CLIP_CHANNELS - 1)];
                    *slot = slot.max(a);
                }

                if !matches!(
                    self.kind,
                    DitherKind::None | DitherKind::Tpdf | DitherKind::Shaped
                ) {
                    if x.is_nan() {
                        self.pcm[ch].clear_filter();
                        out[frame * self.channels + ch] = 0;
                        continue;
                    }
                    let v = x.clamp(-1.0, 1.0) * (scale - 1.0 - pcm::headroom(self.kind));
                    let q = self.pcm[ch].sample(self.kind, v);
                    if x.abs() > 1.0 || q > max_q as f64 || q < min_q as f64 {
                        stats.clipped += 1;
                        stats.clipped_ch[ch.min(MAX_CLIP_CHANNELS - 1)] += 1;
                        on_clip(frame);
                    }
                    // Headroom alone cannot prevent every overflow. Saturate
                    // only the packed output: sample() has already stored the
                    // clean-input error using the UNCLAMPED q, so the loop's
                    // recurrence and non-overloaded statistics stay unchanged.
                    let packed_q = (q as i64).clamp(min_q, max_q);
                    out[frame * self.channels + ch] = (packed_q * (1i64 << shift)) as i32;
                    continue;
                }

                // v = x - H(e): the value the quantizer is asked to produce.
                let mut v = x * scale;
                if let Some(h) = self.shaper {
                    let e = &self.error[ch];
                    let mut feedback = 0.0;
                    for k in 0..SHAPER_ORDER {
                        feedback += h[k] * e[k];
                    }
                    v -= feedback;
                }
                let u = if dither {
                    // TPDF ±1 LSB from two uniform draws.
                    v + (self.rng.uniform() + self.rng.uniform() - 1.0)
                } else {
                    v
                };

                if u.is_nan() {
                    // A NaN sample cannot be quantized; emit silence rather
                    // than poisoning the shaping state forever.
                    self.error[ch] = [0.0; SHAPER_ORDER];
                    out[frame * self.channels + ch] = 0;
                    continue;
                }

                // Ties to even: unbiased, and what other S32 paths do.
                let q = u.round_ties_even() as i64;
                let clipped = q > max_q || q < min_q;
                let saturated = q.clamp(min_q, max_q);
                if clipped {
                    stats.clipped += 1;
                    stats.clipped_ch[ch.min(MAX_CLIP_CHANNELS - 1)] += 1;
                    on_clip(frame);
                }
                if self.shaper.is_some() {
                    // Total error e = y - v (dither included). A saturated
                    // sample's error is limited to the unsaturated range so
                    // the loop never tries to "repay" a clip.
                    let mut e = saturated as f64 - v;
                    if clipped {
                        e = e.clamp(-MAX_ERROR_LSB, MAX_ERROR_LSB);
                    }
                    let hist = &mut self.error[ch];
                    hist.copy_within(0..SHAPER_ORDER - 1, 1);
                    hist[0] = e;
                }

                out[frame * self.channels + ch] = (saturated * (1i64 << shift)) as i32;
            }
        }
        stats
    }

    /// Clear filter/error state and restart the dither noise sequence from the
    /// construction seed, so a reset stream is reproducible.
    pub fn reset(&mut self) {
        self.rng = Rng::new(self.seed);
        self.error.fill([0.0; SHAPER_ORDER]);
        for (ch, state) in self.pcm.iter_mut().enumerate() {
            *state = pcm::Channel::new(self.seed, ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{DitherKind, OutputConfig};

    /// Every channel the engine runs has its own hold: a busy channel
    /// does not keep a silent one dithering, up to the last channel.
    #[test]
    fn every_engine_channel_has_its_own_dither_hold() {
        let last = crate::chain::MAX_CHANNELS - 1;
        for (busy, quiet) in [(8, 9), (last - 1, last), (last, last - 1)] {
            let mut d = TpdfDither::new(1);
            assert!(d.keep_dithering(busy, true, 4));
            assert!(
                !d.keep_dithering(quiet, false, 4),
                "channel {quiet} shares channel {busy}'s hold"
            );
        }
    }

    fn cfg(bits: u32, dither: DitherKind) -> OutputConfig {
        OutputConfig {
            bits,
            dither,
            headroom_db: 0.0,
        }
    }

    fn lcg(seed: u64) -> impl FnMut() -> f64 {
        let mut state = seed;
        move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 11) as f64) / 9_007_199_254_740_992.0
        }
    }

    #[test]
    fn uniform_and_tpdf_statistics() {
        // The generator behind the dither must be uniform with variance 1/12.
        let mut rng = Rng::new(42);
        let n = 1u64 << 20;
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        for _ in 0..n {
            let u = rng.uniform();
            assert!((0.0..1.0).contains(&u));
            sum += u;
            sum_sq += u * u;
        }
        let mean = sum / n as f64;
        let var = sum_sq / n as f64 - mean * mean;
        assert!((mean - 0.5).abs() < 2e-3, "uniform mean {mean}");
        assert!((var - 1.0 / 12.0).abs() < 2e-3, "uniform var {var}");

        // TPDF = u1 + u2 - 1 has variance 2/12 = 1/6.
        let mut rng = Rng::new(7);
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        for _ in 0..n {
            let d = rng.uniform() + rng.uniform() - 1.0;
            assert!((-1.0..1.0).contains(&d));
            sum += d;
            sum_sq += d * d;
        }
        let mean = sum / n as f64;
        let var = sum_sq / n as f64 - mean * mean;
        assert!(mean.abs() < 2e-3, "tpdf mean {mean}");
        assert!((var - 1.0 / 6.0).abs() < 2e-3, "tpdf var {var}");
    }

    #[test]
    fn tpdf_quantization_noise_has_no_dc_and_var_quarter_lsb() {
        // Quantizing silence with TPDF: q is -1, 0 or +1 with probabilities
        // 1/8, 3/4, 1/8, hence mean 0 and variance 1/4 LSB².
        let n = 1usize << 20;
        let zeros = vec![0.0f64; n];
        let mut q = Quantizer::new(&cfg(16, DitherKind::Tpdf), 1, 1234);
        let mut out = vec![0i32; n];
        let stats = q.process(&[&zeros], &mut out);
        assert_eq!(stats.clipped, 0);
        assert_eq!(stats.peak, 0.0);

        let lsb = 65_536.0; // 16-bit LSB in left-justified units
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        for &s in &out {
            let v = s as f64 / lsb;
            assert!(v == 0.0 || v == 1.0 || v == -1.0, "{v}");
            sum += v;
            sum_sq += v * v;
        }
        let mean = sum / n as f64;
        let var = sum_sq / n as f64 - mean * mean;
        assert!(mean.abs() < 2e-3, "dc {mean}");
        assert!((var - 0.25).abs() < 5e-3, "variance {var}");
    }

    #[test]
    fn one_lsb_minus_140_dbfs_sine_survives_at_24_bit() {
        let fs = 48_000.0;
        let f = 1000.0;
        let n = 48_000;
        let amp = 1e-7; // -140 dBFS
        let mut x = vec![0.0; n];
        for (i, v) in x.iter_mut().enumerate() {
            *v = amp * (2.0 * std::f64::consts::PI * f * i as f64 / fs).sin();
        }
        let mut q = Quantizer::new(&cfg(24, DitherKind::Tpdf), 1, 99);
        let mut out = vec![0i32; n];
        q.process(&[&x], &mut out);

        // Project the quantized output onto the tone.  TPDF dither makes the
        // quantizer unbiased, so the recovered amplitude is close to the
        // original even though it is below one 24-bit LSB (1.19e-7).
        let lsb = 256.0;
        let mut re = 0.0;
        let mut im = 0.0;
        for (i, &s) in out.iter().enumerate() {
            let v = s as f64 / lsb;
            let phase = 2.0 * std::f64::consts::PI * f * i as f64 / fs;
            re += v * phase.cos();
            im += v * phase.sin();
        }
        // The projection is in LSB units; convert back to full scale.
        let recovered = 2.0 * (re * re + im * im).sqrt() / n as f64 / 8_388_608.0;
        assert!(
            (recovered - amp).abs() < 0.03 * amp,
            "recovered {recovered:e}, expected {amp:e}"
        );
        // And the tone must stand far above the per-bin dither floor.
        let noise_per_bin = (0.25f64 / n as f64).sqrt() * 2.0 / 8_388_608.0;
        assert!(recovered / noise_per_bin > 100.0);
    }

    #[test]
    fn none_is_exact_on_grid() {
        for bits in [16u32, 24, 32] {
            let scale = 2f64.powi(bits as i32 - 1);
            let shift = 32 - bits;
            let max = (1i64 << (bits - 1)) - 1;
            let samples: Vec<i64> = vec![-max - 1, -max, -1, 0, 1, 12345.min(max), max];
            let xs: Vec<f64> = samples.iter().map(|&n| n as f64 / scale).collect();
            let mut q = Quantizer::new(&cfg(bits, DitherKind::None), 1, 0);
            let mut out = vec![0i32; xs.len()];
            let stats = q.process(&[&xs], &mut out);
            assert_eq!(stats.clipped, 0);
            for (i, &n) in samples.iter().enumerate() {
                let expected = (n * (1i64 << shift)) as i32;
                assert_eq!(out[i], expected, "bits {bits} n {n}");
            }
        }
    }

    #[test]
    fn bits_32_none_rounds_to_the_f64_grid() {
        let x = 0.123_456_789f64;
        let mut q = Quantizer::new(&cfg(32, DitherKind::None), 1, 0);
        let mut out = [0i32; 1];
        q.process(&[&[x][..]], &mut out);
        assert_eq!(out[0], (x * 2_147_483_648.0).round() as i32);
    }

    #[test]
    fn silence_with_none_is_exact_zero() {
        let zeros = vec![0.0; 1000];
        let mut q = Quantizer::new(&cfg(16, DitherKind::None), 1, 0);
        let mut out = vec![7i32; 1000];
        q.process(&[&zeros], &mut out);
        assert!(out.iter().all(|&v| v == 0));
    }

    #[test]
    fn same_seed_is_deterministic_and_reset_repeats() {
        let n = 4096;
        let zeros = vec![0.0; n];
        let mut a = Quantizer::new(&cfg(16, DitherKind::Tpdf), 1, 555);
        let mut b = Quantizer::new(&cfg(16, DitherKind::Tpdf), 1, 555);
        let mut c = Quantizer::new(&cfg(16, DitherKind::Tpdf), 1, 556);
        let (mut oa, mut ob, mut oc) = (vec![0; n], vec![0; n], vec![0; n]);
        a.process(&[&zeros], &mut oa);
        b.process(&[&zeros], &mut ob);
        c.process(&[&zeros], &mut oc);
        assert_eq!(oa, ob);
        assert_ne!(oa, oc);

        // reset() restarts the sequence.
        a.reset();
        let mut oa2 = vec![0; n];
        a.process(&[&zeros], &mut oa2);
        assert_eq!(oa, oa2);
    }

    #[test]
    fn legacy_tpdf_preserves_original_samples_gain_and_clipping() {
        // Independent integer PRNG / f64 quantizer calculation, including
        // interleaved channel draw order, off-grid values, rails and NaN/Inf.
        for (bits, expected) in [
            (16, [0xf76d2df9045d6194, 0x174f5755d952e2d7]),
            (24, [0x7cb90a22a9ce1d21, 0xe16f2396d398a910]),
            (32, [0xdb3e32a3e55a9a1c, 0x1ab62f9de585e86f]),
        ] {
            let scale = 2f64.powi(bits as i32 - 1);
            let x = [
                -1.25,
                -1.0,
                -0.5,
                -0.5 / scale,
                0.5 / scale,
                1.5 / scale,
                0.25 / scale,
                1.0 - 1.0 / scale,
                1.0,
                1.25,
                0.12345,
                f64::INFINITY,
                f64::NAN,
            ];
            let right = x.map(|x| -x);
            for (seed, hash) in [42, crate::chain::DEFAULT_SEED].into_iter().zip(expected) {
                let mut q = Quantizer::new(&cfg(bits, DitherKind::Tpdf), 2, seed);
                let mut out = [0; 26];
                for _ in 0..2 {
                    let stats = q.process(&[&x, &right], &mut out);
                    assert_eq!(
                        crate::bypass::hash_i32(&out),
                        hash,
                        "bits {bits}, seed {seed}"
                    );
                    assert_eq!(stats.clipped, 8);
                    q.reset();
                }
                let mut fallback =
                    Quantizer::with_sample_rate(&cfg(bits, DitherKind::Shaped), 2, seed, 32_000);
                let mut shaped = [0; 26];
                assert_eq!(fallback.process(&[&x, &right], &mut shaped).clipped, 8);
                assert_eq!(shaped, out);
            }
        }
    }

    #[test]
    fn feedback5_positive_rail_never_packs_as_negative() {
        let input = vec![1.0; 65_536];
        for bits in [16, 24, 32] {
            for seed in [42, crate::chain::DEFAULT_SEED] {
                let mut q = Quantizer::new(&cfg(bits, DitherKind::Feedback5), 1, seed);
                let mut out = vec![0; input.len()];
                q.process(&[&input], &mut out);
                for (i, &packed) in out.iter().enumerate() {
                    assert!(
                        packed >= 0,
                        "bits {bits}, seed {seed}, sample {i}: {packed}"
                    );
                }
            }
        }
    }

    #[test]
    fn saturates_and_counts_clips() {
        for kind in [DitherKind::None, DitherKind::Tpdf, DitherKind::Shaped] {
            let input = [1.5f64, -1.5, 0.5];
            let mut q = Quantizer::new(&cfg(16, kind), 1, 3);
            let mut out = [0i32; 3];
            let stats = q.process(&[&input], &mut out);
            assert_eq!(out[0], 32_767i32 << 16, "positive full scale, {kind:?}");
            assert_eq!(out[1], i32::MIN, "negative full scale, {kind:?}");
            // 0.5 full scale; after two clips the shaper feeds back their
            // (limited) errors, at most 1.5·Σ|h| LSB.
            let tolerance = if kind == DitherKind::Shaped {
                34 << 16
            } else {
                0
            };
            assert!(
                (out[2] - (1 << 30)).abs() <= tolerance,
                "0.5 full scale in 16-bit S16, {kind:?}: {}",
                out[2]
            );
            assert_eq!(stats.clipped, 2, "{kind:?}");
            assert_eq!(stats.peak, 1.5);
        }

        // Without dither, exactly +1.0 is one LSB past the largest 16-bit
        // sample and must saturate (with dither it may round either way).
        let mut q = Quantizer::new(&cfg(16, DitherKind::None), 1, 3);
        let mut out = [0i32; 1];
        let stats = q.process(&[&[1.0][..]], &mut out);
        assert_eq!(out[0], 32_767i32 << 16);
        assert_eq!(stats.clipped, 1);
    }

    #[test]
    fn interleaves_channels() {
        let frames = 4;
        let left: Vec<f64> = (0..frames).map(|i| (i as f64 - 2.0) / 8.0).collect();
        let right: Vec<f64> = (0..frames).map(|i| -(i as f64) / 8.0).collect();
        let mut q = Quantizer::new(&cfg(16, DitherKind::None), 2, 0);
        let mut out = vec![0i32; frames * 2];
        q.process(&[&left, &right], &mut out);
        for i in 0..frames {
            assert_eq!(out[2 * i], (left[i] * 32_768.0).round() as i32 * 65_536);
            assert_eq!(
                out[2 * i + 1],
                (right[i] * 32_768.0).round() as i32 * 65_536
            );
        }
    }

    /// Inverse threshold of hearing (Terhardt 1979) as a power weighting:
    /// the F-weighting approximation the shapers are judged by.
    fn f_weight(f: f64) -> f64 {
        let k = f / 1000.0;
        let t = 3.64 * k.powf(-0.8) - 6.5 * (-0.6 * (k - 3.3) * (k - 3.3)).exp() + 1e-3 * k.powi(4);
        10f64.powf(-t / 10.0)
    }

    fn a_weight(f: f64) -> f64 {
        crate::db_to_lin(crate::loudness::a_weighting_db(f)).powi(2)
    }

    /// Quantization error `y - x` in LSB for `x` (full scale) at 16 bits.
    fn quantization_error(kind: DitherKind, fs: u32, x: &[f64], seed: u64) -> Vec<f64> {
        let mut q = Quantizer::with_sample_rate(&cfg(16, kind), 1, seed, fs);
        let mut out = vec![0i32; x.len()];
        q.process(&[x], &mut out);
        out.iter()
            .zip(x)
            .map(|(&y, &x)| y as f64 / 65_536.0 - x * 32_768.0)
            .collect()
    }

    /// Welch PSD (Hann, 50 % overlap, `n`-point segments), bins 0..=n/2.
    fn welch_psd(x: &[f64], n: usize) -> Vec<f64> {
        let mut fft = crate::fft::RealFftPair::new(n);
        let win: Vec<f64> = (0..n)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos())
            .collect();
        let norm: f64 = win.iter().map(|w| w * w).sum();
        let mut spec = vec![crate::Complex64::new(0.0, 0.0); n / 2 + 1];
        let mut psd = vec![0.0; n / 2 + 1];
        let mut seg = vec![0.0; n];
        let mut count = 0;
        let mut start = 0;
        while start + n <= x.len() {
            for i in 0..n {
                seg[i] = x[start + i] * win[i];
            }
            fft.forward_padded_complex(&seg, &mut spec);
            for (p, c) in psd.iter_mut().zip(&spec) {
                *p += c.norm_sqr();
            }
            count += 1;
            start += n / 2;
        }
        // Scaled so that the sum over all bins (one-sided) is the variance.
        for (k, p) in psd.iter_mut().enumerate() {
            let one_sided = if k == 0 || k == n / 2 { 1.0 } else { 2.0 };
            *p *= one_sided / (count as f64 * norm);
        }
        psd
    }

    /// Noise power in 20 Hz..20 kHz weighted by `w`.
    fn band_power(psd: &[f64], fs: f64, w: fn(f64) -> f64) -> f64 {
        let n = 2 * (psd.len() - 1);
        psd.iter()
            .enumerate()
            .map(|(k, &p)| (k as f64 * fs / n as f64, p))
            .filter(|&(f, _)| (20.0..=20_000.0).contains(&f))
            .map(|(f, p)| p * w(f))
            .sum()
    }

    fn db(x: f64) -> f64 {
        10.0 * x.log10()
    }

    #[test]
    fn shaped_lowers_audible_noise_on_silence_and_quiet_sine() {
        let n = 1 << 18;
        // (rate, max F-weighted ratio dB, max total ratio dB, max A ratio dB)
        let limits = [
            (44_100u32, -9.0, 19.5, f64::INFINITY),
            (48_000, -11.0, 19.5, f64::INFINITY),
            (88_200, -20.0, 12.0, -4.0),
            (96_000, -20.0, 11.0, -5.0),
            (176_400, -27.0, 7.0, -12.0),
            (192_000, -27.0, 7.0, -12.0),
        ];
        for (fs, max_f, max_total, max_a) in limits {
            let sine: Vec<f64> = (0..n)
                .map(|i| 1e-3 * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / fs as f64).sin())
                .collect();
            for (name, x) in [("silence", vec![0.0; n]), ("-60 dBFS sine", sine)] {
                let tpdf = quantization_error(DitherKind::Tpdf, fs, &x, 11);
                let shaped = quantization_error(DitherKind::Shaped, fs, &x, 11);
                let (pt, ps) = (welch_psd(&tpdf, 4096), welch_psd(&shaped, 4096));
                let f_ratio =
                    db(band_power(&ps, fs as f64, f_weight) / band_power(&pt, fs as f64, f_weight));
                let a_ratio =
                    db(band_power(&ps, fs as f64, a_weight) / band_power(&pt, fs as f64, a_weight));
                let var = |e: &[f64]| e.iter().map(|v| v * v).sum::<f64>() / e.len() as f64;
                let total = db(var(&shaped) / var(&tpdf));
                eprintln!(
                    "{fs} Hz, {name}: shaped vs TPDF: F-weighted {f_ratio:+.2} dB, A-weighted {a_ratio:+.2} dB, total {total:+.2} dB (TPDF {:.3} LSB²)",
                    var(&tpdf)
                );
                assert!(
                    f_ratio <= max_f,
                    "{fs} Hz {name}: F-weighted {f_ratio:.2} dB"
                );
                assert!(total <= max_total, "{fs} Hz {name}: total {total:.2} dB");
                assert!(
                    a_ratio <= max_a,
                    "{fs} Hz {name}: A-weighted {a_ratio:.2} dB"
                );
                // TPDF itself is white at 1/4 LSB².
                assert!((var(&tpdf) - 0.25).abs() < 0.01);
            }
        }
    }

    #[test]
    fn shaper_table_selection() {
        assert!(noise_shaper(32_000).is_none());
        assert!(noise_shaper(44_099).is_none());
        assert_eq!(noise_shaper(44_100), Some(&SHAPERS[0].1));
        assert_eq!(noise_shaper(47_999), Some(&SHAPERS[0].1));
        assert_eq!(noise_shaper(48_000), Some(&SHAPERS[1].1));
        assert_eq!(noise_shaper(64_000), Some(&SHAPERS[1].1));
        assert_eq!(noise_shaper(96_000), Some(&SHAPERS[3].1));
        assert_eq!(noise_shaper(384_000), Some(&SHAPERS[5].1));
        // Below 44.1 kHz Shaped is exactly TPDF.
        let x: Vec<f64> = (0..4096).map(|i| 1e-4 * (i as f64 * 0.01).sin()).collect();
        let mut a = Quantizer::with_sample_rate(&cfg(16, DitherKind::Shaped), 1, 9, 32_000);
        let mut b = Quantizer::with_sample_rate(&cfg(16, DitherKind::Tpdf), 1, 9, 32_000);
        let (mut oa, mut ob) = (vec![0; 4096], vec![0; 4096]);
        a.process(&[&x], &mut oa);
        b.process(&[&x], &mut ob);
        assert_eq!(oa, ob);
        assert!(a.shaper().is_none());
        // The default constructor assumes 44.1 kHz.
        let q = Quantizer::new(&cfg(16, DitherKind::Shaped), 1, 0);
        assert_eq!(q.shaper(), Some(&SHAPERS[0].1[..]));
        // Every shaper is minimum phase enough to be a sensible NTF: its
        // DC gain is well below 1 and the coefficients are finite.
        for (rate, h) in SHAPERS {
            let dc: f64 = 1.0 - h.iter().sum::<f64>();
            assert!(dc.abs() < 0.5, "{rate}: NTF(1) = {dc}");
        }
    }

    #[test]
    fn shaped_is_stable_through_ten_seconds_of_full_scale_square() {
        for fs in [44_100u32, 48_000, 96_000, 192_000] {
            for amp in [1.0, 0.9995] {
                let n = 10 * fs as usize;
                let period = fs as usize / 1000;
                let mut x: Vec<f64> = (0..n)
                    .map(|i| {
                        if (i / (period / 2)).is_multiple_of(2) {
                            amp
                        } else {
                            -amp
                        }
                    })
                    .collect();
                // Then two seconds of silence: the shaper must settle back.
                x.extend(std::iter::repeat_n(0.0, 2 * fs as usize));
                let mut q = Quantizer::with_sample_rate(&cfg(16, DitherKind::Shaped), 1, 21, fs);
                let h = q.shaper().unwrap().to_vec();
                let bound = MAX_ERROR_LSB * (1.0 + h.iter().map(|v| v.abs()).sum::<f64>());
                let mut out = vec![0i32; x.len()];
                let stats = q.process(&[&x], &mut out);
                if amp == 1.0 {
                    // +1.0 is one LSB past the largest code: every positive
                    // half-cycle saturates.
                    assert!(
                        stats.clipped as usize >= n / 2,
                        "{fs} Hz: {} clips",
                        stats.clipped
                    );
                }
                for (i, (&y, &xi)) in out.iter().zip(&x).enumerate() {
                    let y = y as f64 / 65_536.0;
                    let ideal = (xi * 32_768.0).clamp(-32_768.0, 32_767.0);
                    assert!(
                        (y - ideal).abs() <= bound + 1.0,
                        "{fs} Hz amp {amp}: sample {i}: {y} vs {ideal}"
                    );
                }
                assert!(
                    q.error[0]
                        .iter()
                        .all(|e| e.is_finite() && e.abs() <= MAX_ERROR_LSB)
                );

                // The last second of silence has the steady-state shaped
                // noise of a fresh quantizer.
                let tail: Vec<f64> = out[x.len() - fs as usize..]
                    .iter()
                    .map(|&y| y as f64 / 65_536.0)
                    .collect();
                let fresh = quantization_error(DitherKind::Shaped, fs, &vec![0.0; fs as usize], 5);
                let var = |e: &[f64]| e.iter().map(|v| v * v).sum::<f64>() / e.len() as f64;
                let ratio = var(&tail) / var(&fresh);
                assert!(
                    (0.9..1.1).contains(&ratio),
                    "{fs} Hz amp {amp}: tail/fresh {ratio}"
                );
            }
        }
    }

    #[test]
    fn shaped_is_deterministic_and_reset_repeats() {
        let n = 8192;
        let x: Vec<f64> = (0..n).map(|i| 0.3 * (i as f64 * 0.013).sin()).collect();
        let run = |q: &mut Quantizer| {
            let mut out = vec![0i32; 2 * n];
            q.process(&[&x, &x], &mut out);
            out
        };
        let mut a = Quantizer::with_sample_rate(&cfg(16, DitherKind::Shaped), 2, 77, 48_000);
        let mut b = Quantizer::with_sample_rate(&cfg(16, DitherKind::Shaped), 2, 77, 48_000);
        let oa = run(&mut a);
        assert_eq!(oa, run(&mut b));
        a.reset();
        assert_eq!(oa, run(&mut a));
        let mut c = Quantizer::with_sample_rate(&cfg(16, DitherKind::Shaped), 2, 78, 48_000);
        assert_ne!(oa, run(&mut c));
    }

    #[test]
    fn nan_input_is_silent_and_does_not_poison_shaping() {
        for kind in [DitherKind::None, DitherKind::Tpdf, DitherKind::Shaped] {
            let input = [f64::NAN, 0.25, f64::INFINITY, 0.25];
            let mut q = Quantizer::new(&cfg(24, kind), 1, 5);
            let mut out = [7i32; 4];
            let stats = q.process(&[&input], &mut out);
            assert_eq!(out[0], 0, "{kind:?}");
            // 0.25 full scale is on the 24-bit grid; dither may move it by an
            // LSB and the shaper's feedback by up to 1.5·Σ|h| (~32) more.
            let expected = (0.25 * 8_388_608.0) as i32 * 256;
            let tolerance = match kind {
                DitherKind::None => 0,
                DitherKind::Tpdf => 256,
                DitherKind::Shaped => 34 * 256,
                _ => unreachable!("only original modes in this test"),
            };
            assert!(
                (out[1] - expected).abs() <= tolerance,
                "{kind:?}: {}",
                out[1]
            );
            assert_eq!(out[2], i32::MAX & !0xff, "{kind:?}"); // +inf clamps
            assert!(
                (out[3] - expected).abs() <= tolerance,
                "{kind:?}: {}",
                out[3]
            );
            assert_eq!(stats.clipped, 1, "{kind:?}");
            assert_eq!(stats.peak, 0.25);
        }
    }

    #[test]
    fn empty_and_fewer_frames_are_fine() {
        let mut q = Quantizer::new(&cfg(24, DitherKind::Tpdf), 2, 1);
        let empty: [&[f64]; 2] = [&[], &[]];
        let mut out: [i32; 0] = [];
        let stats = q.process(&empty, &mut out);
        assert_eq!(stats, ClipStats::default());

        let a = vec![0.25; 7];
        let b = vec![-0.25; 7];
        let mut out = vec![0i32; 14];
        let stats = q.process(&[&a, &b], &mut out);
        assert_eq!(stats.peak, 0.25);
        // No clipping and the sign is preserved.
        assert_eq!(stats.clipped, 0);
        assert!(out[0] > 0 && out[1] < 0);
    }

    #[test]
    fn merge_clip_stats() {
        let mut a = ClipStats {
            clipped: 2,
            peak: 1.2,
            clipped_ch: [1, 1, 0, 0, 0, 0, 0, 0],
            ..ClipStats::default()
        };
        a.merge(ClipStats {
            clipped: 3,
            peak: 0.5,
            clipped_ch: [0, 2, 1, 0, 0, 0, 0, 0],
            peak_ch: [0.5, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            chain_peak_ch: [0.7, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            measured: true,
            limiter: Default::default(),
            ab_overs: [0.25, 1.0],
        });
        assert_eq!(
            a,
            ClipStats {
                clipped: 5,
                peak: 1.2,
                clipped_ch: [1, 3, 1, 0, 0, 0, 0, 0],
                peak_ch: [0.5, 0.25, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                chain_peak_ch: [0.7, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                measured: true,
                limiter: Default::default(),
                ab_overs: [0.25, 1.0],
            }
        );
    }

    #[test]
    fn merge_keeps_the_largest_limiter_reduction() {
        use audivane_protocol::LimiterStatus;
        let status = |reduction, events, headroom| ClipStats {
            limiter: LimiterStatus {
                active: true,
                headroom_db: headroom,
                gain_reduction_db: reduction,
                events,
            },
            ..ClipStats::default()
        };
        let mut a = ClipStats::default();
        a.merge(status(6.0, 1, 1.0));
        a.merge(status(0.5, 2, 1.5));
        assert_eq!(a.limiter, status(6.0, 3, 1.5).limiter);
    }

    #[test]
    fn generator_is_stable_with_extreme_seeds() {
        for seed in [0, 1, u64::MAX] {
            let mut q = Quantizer::new(&cfg(16, DitherKind::Tpdf), 1, seed);
            let mut rng = lcg(seed ^ 0xdead_beef);
            let x: Vec<f64> = (0..1000).map(|_| rng() * 0.9 - 0.45).collect();
            let mut out = vec![0i32; x.len()];
            let stats = q.process(&[&x], &mut out);
            assert_eq!(stats.clipped, 0);
            assert!(out.iter().all(|&v| v > i32::MIN && v < i32::MAX));
        }
    }
}
