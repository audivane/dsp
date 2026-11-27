//! A-weighting, K-weighting (BS.1770) and level matching for A/B.
//!
//! Two meter families live here:
//!
//! - `AWeightedMeter` approximates the IEC 61672 A-weighting curve with an
//!   IIR filter.  The analog prototype is a 6th-order rational function
//!   (`s^4` over four poles at 20.6, 107.7, 737.9 and 12194 Hz), bilinearly
//!   transformed with each corner prewarped.  Near Nyquist a plain bilinear
//!   transform warps badly (at 44.1 kHz the error reaches ~4 dB at 16 kHz), so
//!   the meter runs the biquads at 4x the input rate and interpolates with a
//!   128-tap polyphase FIR.  The warping then corresponds to 176 kHz, and the
//!   error stays around 0.1 dB out to 16 kHz.
//! - `LufsMeter` implements ITU-R BS.1770-4 integrated loudness: the published
//!   K-weighting (a high shelf plus a high pass), 400 ms blocks with 75 %
//!   overlap, the -70 LUFS absolute gate and the -10 LU relative gate. Block
//!   loudness goes into a fixed histogram (0.01 LU bins, each keeping its
//!   block count and exact energy sum), so `push` never allocates and memory
//!   stays constant however long the meter runs.

use crate::Complex64;
use std::f64::consts::PI;

// ---------------------------------------------------------------- A-weighting

/// The textbook formula adds +2 dB to normalise at 1 kHz.  That leaves a
/// residual of 0.00014 dB at 1 kHz; using the exact value (2 dB - residual)
/// makes 1 kHz exactly 0 dB without moving anything else.
const A_WEIGHTING_NORMALISATION_DB: f64 = 1.999_858_472_305_120_5;

/// IEC 61672 A-weighting in dB (0 dB at 1 kHz), analytic formula.
pub fn a_weighting_db(f_hz: f64) -> f64 {
    if !f_hz.is_finite() || f_hz <= 0.0 {
        return f64::NEG_INFINITY;
    }
    let f2 = f_hz * f_hz;
    let num = 12_194.0f64.powi(2) * f2 * f2;
    let den = (f2 + 20.6f64.powi(2))
        * ((f2 + 107.7f64.powi(2)) * (f2 + 737.9f64.powi(2))).sqrt()
        * (f2 + 12_194.0f64.powi(2));
    let ratio = num / den;
    if ratio > 0.0 {
        20.0 * ratio.log10() + A_WEIGHTING_NORMALISATION_DB
    } else {
        f64::NEG_INFINITY
    }
}

/// Static level offset for an A/B comparison, A-weighted
/// ([`weighted_offset_db`] with [`Weighting::A`]).
pub fn analytic_offset_db(
    power_a: &dyn Fn(f64) -> f64,
    power_b: &dyn Fn(f64) -> f64,
    sample_rate: f64,
) -> f64 {
    weighted_offset_db(power_a, power_b, sample_rate, Weighting::A)
}

/// Frequency weighting for [`weighted_offset_db`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weighting {
    /// BS.1770 K-weighting as the [`LufsMeter`] runs it at the stream rate
    /// (the digital shelf and high pass), so the integral predicts LUFS.
    K,
    /// IEC 61672 A-weighting, analytic.
    A,
}

impl From<audivane_protocol::ipc::LevelWeighting> for Weighting {
    fn from(w: audivane_protocol::ipc::LevelWeighting) -> Self {
        match w {
            audivane_protocol::ipc::LevelWeighting::K => Self::K,
            audivane_protocol::ipc::LevelWeighting::A => Self::A,
        }
    }
}

/// Power (|W(f)|²) of BS.1770 K-weighting at `f_hz` for `sample_rate`.
pub fn k_weighting_power(f_hz: f64, sample_rate: f64) -> f64 {
    let z1 = Complex64::from_polar(1.0, -2.0 * PI * f_hz / sample_rate);
    (section_response(&k_shelf(sample_rate), z1) * section_response(&k_high_pass(sample_rate), z1))
        .norm_sqr()
}

/// Static level offset for an A/B comparison.
///
/// `power_a(f)` / `power_b(f)` return |H(f)|² of each chain. Both are
/// integrated over 20 Hz..min(20 kHz, Nyquist) on a log grid, weighted by
/// `weighting`'s power and a pink-noise (1/f) reference spectrum. Returns
/// the dB gain to apply to **B** so its weighted loudness equals A's.
pub fn weighted_offset_db(
    power_a: &dyn Fn(f64) -> f64,
    power_b: &dyn Fn(f64) -> f64,
    sample_rate: f64,
    weighting: Weighting,
) -> f64 {
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return 0.0;
    }
    let f_max = (0.5 * sample_rate).min(20_000.0);
    if f_max <= 20.0 {
        return 0.0;
    }

    const POINTS: usize = 8192;
    let u0 = 20.0f64.ln();
    let u1 = f_max.ln();
    let du = (u1 - u0) / POINTS as f64;

    let mut power_a_total = 0.0;
    let mut power_b_total = 0.0;
    for i in 0..POINTS {
        let u = u0 + (i as f64 + 0.5) * du;
        let f = u.exp();
        // Weighting power times pink 1/f, integrated over df = f du, so the
        // 1/f and the f from the substitution cancel; only du remains.
        let w2 = match weighting {
            Weighting::A => crate::db_to_lin(a_weighting_db(f)).powi(2),
            Weighting::K => k_weighting_power(f, sample_rate),
        };
        let weight = w2 * du;
        power_a_total += power_a(f) * weight;
        power_b_total += power_b(f) * weight;
    }

    if power_a_total > 0.0
        && power_b_total > 0.0
        && power_a_total.is_finite()
        && power_b_total.is_finite()
    {
        10.0 * (power_a_total / power_b_total).log10()
    } else {
        // Degenerate (silent or non-finite) input: leave the level alone.
        0.0
    }
}

// ---------------------------------------------------------------- A-weighted meter

/// Internal oversampling factor of `AWeightedMeter`.
const OVERSAMPLE: usize = 4;
/// Prototype length of the interpolation FIR.
const INTERP_TAPS: usize = 128;
/// Taps per polyphase branch.
const PHASE_TAPS: usize = INTERP_TAPS / OVERSAMPLE;
const INTERP_BETA: f64 = 9.0;

fn bessel_i0(x: f64) -> f64 {
    let half = 0.5 * x;
    let mut sum = 1.0;
    let mut term = 1.0;
    let mut k = 1.0f64;
    loop {
        term *= (half / k) * (half / k);
        sum += term;
        if term < 1e-17 * sum {
            break;
        }
        k += 1.0;
    }
    sum
}

fn kaiser_window(n: usize, len: usize, beta: f64) -> f64 {
    if len <= 1 {
        return 1.0;
    }
    let t = 2.0 * n as f64 / (len - 1) as f64 - 1.0;
    let arg = beta * (1.0 - t * t).max(0.0).sqrt();
    bessel_i0(arg) / bessel_i0(beta)
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let p = PI * x;
        p.sin() / p
    }
}

/// Polyphase 4x interpolator, cutoff at the input Nyquist (0.125 cycles per
/// internal sample) with a Kaiser window.  The prototype has unity DC gain
/// after zero-stuffing (`sum(h) = 4`).
fn design_interpolator() -> [[f64; PHASE_TAPS]; OVERSAMPLE] {
    let l = OVERSAMPLE as f64;
    let fc = 0.5 / l;
    let mid = (INTERP_TAPS - 1) as f64 / 2.0;
    let mut proto = [0.0f64; INTERP_TAPS];
    for (n, h) in proto.iter_mut().enumerate() {
        let x = 2.0 * fc * (n as f64 - mid);
        *h = 2.0 * fc * l * sinc(x) * kaiser_window(n, INTERP_TAPS, INTERP_BETA);
    }
    let mut fir = [[0.0f64; PHASE_TAPS]; OVERSAMPLE];
    for (p, branch) in fir.iter_mut().enumerate() {
        for (k, tap) in branch.iter_mut().enumerate() {
            *tap = proto[p + OVERSAMPLE * k];
        }
    }
    fir
}

/// A-weighting as three biquads, bilinearly transformed with prewarped
/// corners at `sample_rate` (the meter passes its internal 4x rate).
fn design_a_weighting(sample_rate: f64) -> [[f64; 5]; 3] {
    let fs = sample_rate;
    let t = 1.0 / fs;
    let corners = [20.6f64, 107.7, 737.9, 12_194.0];
    let mut poles = [0.0f64; 4];
    for (pole, corner) in poles.iter_mut().zip(corners) {
        // Keep the design stable for absurdly low rates.
        let f = corner.min(0.45 * fs);
        let warped = 2.0 / t * (PI * f * t).tan();
        *pole = (1.0 - warped * t * 0.5) / (1.0 + warped * t * 0.5);
    }
    let p = poles;
    let mut sections = [
        // (1 - z^-1)^2 / (1 - p1 z^-1)^2
        [1.0, -2.0, 1.0, -2.0 * p[0], p[0] * p[0]],
        // (1 - z^-1)^2 / ((1 - p2 z^-1)(1 - p3 z^-1))
        [1.0, -2.0, 1.0, -(p[1] + p[2]), p[1] * p[2]],
        // (1 + z^-1)^2 / (1 - p4 z^-1)^2
        [1.0, 2.0, 1.0, -2.0 * p[3], p[3] * p[3]],
    ];
    // Normalise at 1 kHz to the analytic A-weighting (0 dB).
    let z1 = Complex64::from_polar(1.0, -2.0 * PI * 1000.0 / fs);
    let mut h = Complex64::new(1.0, 0.0);
    for sec in &sections {
        h *= section_response(sec, z1);
    }
    let target = crate::db_to_lin(a_weighting_db(1000.0));
    let gain = target / h.norm();
    for coeff in &mut sections[0][0..3] {
        *coeff *= gain;
    }
    sections
}

fn section_response(sec: &[f64; 5], z1: Complex64) -> Complex64 {
    let z2 = z1 * z1;
    (sec[0] + z1 * sec[1] + z2 * sec[2]) / (1.0 + z1 * sec[3] + z2 * sec[4])
}

#[inline]
pub(crate) fn biquad_step(coeff: &[f64; 5], state: &mut [f64; 2], x: f64) -> f64 {
    let y = coeff[0] * x + state[0];
    state[0] = coeff[1] * x - coeff[3] * y + state[1];
    state[1] = coeff[2] * x - coeff[4] * y;
    y
}

/// A-weighted RMS meter (IIR A-weighting filter, bilinear transform,
/// accurate to ±0.5 dB from 20 Hz to 16 kHz at 44.1 kHz and above).
pub struct AWeightedMeter {
    channels: usize,
    fir: [[f64; PHASE_TAPS]; OVERSAMPLE],
    /// Previous input samples per channel, a ring of `PHASE_TAPS`.
    history: Vec<[f64; PHASE_TAPS]>,
    history_pos: usize,
    sections: [[f64; 5]; 3],
    /// Per channel, `[z1, z2]` for each of the three sections.
    state: Vec<[[f64; 2]; 3]>,
    sum_sq: f64,
    samples_in: u64,
}

impl AWeightedMeter {
    pub fn new(sample_rate: f64, channels: usize) -> Self {
        let fs = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        AWeightedMeter {
            channels,
            fir: design_interpolator(),
            history: vec![[0.0; PHASE_TAPS]; channels],
            history_pos: 0,
            sections: design_a_weighting(fs * OVERSAMPLE as f64),
            state: vec![[[0.0; 2]; 3]; channels],
            sum_sq: 0.0,
            samples_in: 0,
        }
    }

    #[allow(clippy::needless_range_loop)] // one output phase per input frame
    pub fn push(&mut self, chans: &[&[f64]]) {
        let channels = self.channels.min(chans.len());
        let frames = if channels == 0 { 0 } else { chans[0].len() };
        if frames == 0 {
            return;
        }

        for frame in 0..frames {
            let pos = self.history_pos;
            for ch in 0..channels {
                self.history[ch][pos] = chans[ch][frame];
            }
            for (ch, history) in self.history.iter().enumerate().take(channels) {
                let state = &mut self.state[ch];
                for branch in &self.fir {
                    let mut y = 0.0;
                    for (k, &tap) in branch.iter().enumerate() {
                        y += tap * history[(pos + PHASE_TAPS - k) & (PHASE_TAPS - 1)];
                    }
                    for (sec, st) in self.sections.iter().zip(state.iter_mut()) {
                        y = biquad_step(sec, st, y);
                    }
                    self.sum_sq += y * y;
                }
            }
            self.history_pos = (pos + 1) & (PHASE_TAPS - 1);
            self.samples_in += 1;
        }
    }

    /// A-weighted RMS over everything pushed, summed across channels, in dB
    /// relative to full scale.
    pub fn rms_db(&self) -> f64 {
        if self.samples_in == 0 {
            return f64::NEG_INFINITY;
        }
        let denominator = (OVERSAMPLE as u64 * self.samples_in) as f64;
        let mean_square = self.sum_sq / denominator;
        if mean_square > 0.0 {
            10.0 * mean_square.log10()
        } else {
            f64::NEG_INFINITY
        }
    }

    pub fn reset(&mut self) {
        self.history.fill([0.0; PHASE_TAPS]);
        self.state.fill([[0.0; 2]; 3]);
        self.history_pos = 0;
        self.sum_sq = 0.0;
        self.samples_in = 0;
    }
}

// ---------------------------------------------------------------- LUFS

/// BS.1770-4 K-weighting shelf design constants.
const K_SHELF_F0: f64 = 1_681.974_450_955_533;
const K_SHELF_GAIN_DB: f64 = 3.999_843_853_973_347;
const K_SHELF_Q: f64 = 0.707_175_236_955_419_6;
/// `Vb = Vh^0.4996667741545416`, as in BS.1770's shelf derivation.
const K_SHELF_VB_EXP: f64 = 0.499_666_774_154_541_6;
const K_HP_F0: f64 = 38.135_470_876_024_44;
const K_HP_Q: f64 = 0.500_327_037_323_877_3;
/// Loudness offset from BS.1770 (`-0.691 + 10 log10(sum of mean squares)`).
const LUFS_OFFSET: f64 = -0.691;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = 10.0;
const BLOCK_SECONDS: f64 = 0.4;
const HOP_SECONDS: f64 = 0.1;

pub(crate) fn k_shelf(sample_rate: f64) -> [f64; 5] {
    let fs = sample_rate;
    let f0 = K_SHELF_F0.min(0.45 * fs);
    let k = (PI * f0 / fs).tan();
    let vh = 10f64.powf(K_SHELF_GAIN_DB / 20.0);
    let vb = vh.powf(K_SHELF_VB_EXP);
    let a0 = 1.0 + k / K_SHELF_Q + k * k;
    [
        (vh + vb * k / K_SHELF_Q + k * k) / a0,
        2.0 * (k * k - vh) / a0,
        (vh - vb * k / K_SHELF_Q + k * k) / a0,
        2.0 * (k * k - 1.0) / a0,
        (1.0 - k / K_SHELF_Q + k * k) / a0,
    ]
}

pub(crate) fn k_high_pass(sample_rate: f64) -> [f64; 5] {
    let fs = sample_rate;
    let f0 = K_HP_F0.min(0.45 * fs);
    let k = (PI * f0 / fs).tan();
    let a0 = 1.0 + k / K_HP_Q + k * k;
    [
        1.0,
        -2.0,
        1.0,
        2.0 * (k * k - 1.0) / a0,
        (1.0 - k / K_HP_Q + k * k) / a0,
    ]
}

/// Histogram range for gated block loudness: from the absolute gate up to
/// `HIST_MAX_LUFS` in `HIST_BIN_LU` steps, plus one overflow bin above.
const HIST_MAX_LUFS: f64 = 10.0;
const HIST_BIN_LU: f64 = 0.01;
const HIST_BINS: usize = ((HIST_MAX_LUFS - ABSOLUTE_GATE_LUFS) / HIST_BIN_LU) as usize;

#[inline]
fn block_loudness(energy: f64) -> f64 {
    LUFS_OFFSET + 10.0 * energy.log10()
}

/// ITU-R BS.1770-4 integrated loudness (K-weighting + gating), mono/stereo.
///
/// Every 100 ms a 400 ms block completes. Blocks above the absolute gate go
/// into a fixed histogram of 0.01 LU bins (-70 to +10 LUFS, one overflow bin
/// above) that keeps, per bin, the block count and the exact sum of block
/// energies; the absolute-gated total is kept exactly too. The relative gate
/// (-10 LU below the absolute-gated mean) is therefore exact, and so is the
/// final mean, except for the blocks in the single 0.01 LU bin that contains
/// the relative threshold: that bin is included or excluded as a whole by its
/// mean loudness. `push` and `reset` never allocate; memory is constant
/// (~130 KB) regardless of run time.
pub struct LufsMeter {
    channels: usize,
    shelf: [f64; 5],
    high_pass: [f64; 5],
    /// Per channel `[z1, z2]` for shelf and high pass.
    state: Vec<[[f64; 2]; 2]>,
    hop_frames: usize,
    block_frames: usize,
    hop_count: usize,
    hop_sums: Vec<f64>,
    /// Ring of the last four completed hop sums, per channel.
    ring: Vec<[f64; 4]>,
    ring_pos: usize,
    hops_seen: u64,
    /// `(block count, sum of block energies)` per 0.01 LU bin above the
    /// absolute gate; the last entry collects everything above +10 LUFS.
    hist: Vec<(u64, f64)>,
    /// Blocks above the absolute gate, and their energy sum.
    gated_blocks: u64,
    gated_energy: f64,
}

impl LufsMeter {
    pub fn new(sample_rate: f64, channels: usize) -> Self {
        let fs = if sample_rate.is_finite() && sample_rate > 0.0 {
            sample_rate
        } else {
            48_000.0
        };
        let hop_frames = ((HOP_SECONDS * fs).round() as usize).max(1);
        let block_frames = ((BLOCK_SECONDS * fs).round() as usize).max(hop_frames);
        LufsMeter {
            channels,
            shelf: k_shelf(fs),
            high_pass: k_high_pass(fs),
            state: vec![[[0.0; 2]; 2]; channels],
            hop_frames,
            block_frames,
            hop_count: 0,
            hop_sums: vec![0.0; channels],
            ring: vec![[0.0; 4]; channels],
            ring_pos: 0,
            hops_seen: 0,
            hist: vec![(0, 0.0); HIST_BINS + 1],
            gated_blocks: 0,
            gated_energy: 0.0,
        }
    }

    /// Feed planar frames. Real-time safe: no allocation, fixed memory.
    #[allow(clippy::needless_range_loop)] // the frame loop drives hop timing
    pub fn push(&mut self, chans: &[&[f64]]) {
        let channels = self.channels.min(chans.len());
        let frames = if channels == 0 { 0 } else { chans[0].len() };
        for frame in 0..frames {
            for (ch, channel) in chans.iter().enumerate().take(channels) {
                let x = channel[frame];
                let st = &mut self.state[ch];
                let y = biquad_step(&self.shelf, &mut st[0], x);
                let y = biquad_step(&self.high_pass, &mut st[1], y);
                self.hop_sums[ch] += y * y;
            }
            self.hop_count += 1;
            if self.hop_count >= self.hop_frames {
                self.finish_hop();
            }
        }
    }

    fn finish_hop(&mut self) {
        let pos = self.ring_pos;
        for ch in 0..self.channels {
            self.ring[ch][pos] = self.hop_sums[ch];
            self.hop_sums[ch] = 0.0;
        }
        self.ring_pos = (pos + 1) & 3;
        self.hops_seen += 1;
        self.hop_count = 0;

        if self.hops_seen >= 4 {
            let mut energy = 0.0;
            for channel in &self.ring {
                energy += channel.iter().sum::<f64>();
            }
            self.add_block(energy / self.block_frames as f64);
        }
    }

    fn add_block(&mut self, energy: f64) {
        let loudness = block_loudness(energy);
        // Absolute gate; also drops silent (log of 0) and NaN blocks.
        if loudness.is_nan() || loudness <= ABSOLUTE_GATE_LUFS {
            return;
        }
        let bin = (((loudness - ABSOLUTE_GATE_LUFS) / HIST_BIN_LU) as usize).min(HIST_BINS);
        let slot = &mut self.hist[bin];
        slot.0 += 1;
        slot.1 += energy;
        self.gated_blocks += 1;
        self.gated_energy += energy;
    }

    /// Integrated loudness in LUFS; `f64::NEG_INFINITY` if all blocks gated.
    pub fn integrated_lufs(&self) -> f64 {
        if self.gated_blocks == 0 {
            return f64::NEG_INFINITY;
        }
        // Relative gate from the blocks that passed the absolute gate
        // (exact), applied on top of the absolute gate.
        let threshold =
            block_loudness(self.gated_energy / self.gated_blocks as f64) - RELATIVE_GATE_LU;
        let position = (threshold - ABSOLUTE_GATE_LUFS) / HIST_BIN_LU;
        let (first_full, edge) = if position < 0.0 {
            (0, None)
        } else {
            let edge = (position as usize).min(HIST_BINS);
            (edge + 1, Some(edge))
        };

        let mut count = 0u64;
        let mut sum = 0.0;
        for &(n, e) in &self.hist[first_full.min(self.hist.len())..] {
            count += n;
            sum += e;
        }
        // The bin holding the threshold: all its blocks are within one bin
        // width of it; keep them if their mean loudness passes.
        if let Some(edge) = edge {
            let (n, e) = self.hist[edge];
            if n > 0 && block_loudness(e / n as f64) > threshold {
                count += n;
                sum += e;
            }
        }
        if count == 0 {
            return f64::NEG_INFINITY;
        }
        block_loudness(sum / count as f64)
    }

    /// Forget all meter state (the integrated value restarts from zero).
    /// Real-time safe.
    pub fn reset(&mut self) {
        self.state.fill([[0.0; 2]; 2]);
        self.hop_sums.fill(0.0);
        self.ring.fill([0.0; 4]);
        self.ring_pos = 0;
        self.hops_seen = 0;
        self.hop_count = 0;
        self.hist.fill((0, 0.0));
        self.gated_blocks = 0;
        self.gated_energy = 0.0;
    }
}

// ---------------------------------------------------------------- stage loudness

/// Rate of the stage loudness simulation ([`stage_loudness_change`]).
pub(crate) const STAGE_LOUDNESS_RATE: f64 = 48_000.0;
/// Its reference: pink noise at a loud master's RMS, peaks near full scale.
const STAGE_LOUDNESS_RMS_DB: f64 = -14.0;
/// Seconds of reference, and the settling time left out of the measurement.
const STAGE_LOUDNESS_SECONDS: f64 = 4.5;
const STAGE_LOUDNESS_SKIP_SECONDS: f64 = 0.5;

/// [`stage_loudness_change`] of `stage` (built at [`STAGE_LOUDNESS_RATE`],
/// one channel) for the reference. Off the audio thread: it allocates.
#[cfg(test)]
pub(crate) fn stage_loudness_change_db(stage: &mut dyn crate::Stage) -> f64 {
    let input = stage_loudness_reference();
    let mut output = input.clone();
    stage.process(&mut [output.as_mut_slice()]);
    stage_loudness_change(&input, &output)
}

/// Loudness change of a non-linear stage on music, dB: the BS.1770
/// (K-weighted) loudness of the stage's `output` for the reference `input`
/// ([`stage_loudness_reference`], one channel at [`STAGE_LOUDNESS_RATE`])
/// minus the input's, after 0.5 s of settling. 0 when not finite.
pub(crate) fn stage_loudness_change(input: &[f64], output: &[f64]) -> f64 {
    let skip = (STAGE_LOUDNESS_SKIP_SECONDS * STAGE_LOUDNESS_RATE) as usize;
    let lufs = |x: &[f64]| {
        let mut meter = LufsMeter::new(STAGE_LOUDNESS_RATE, 1);
        meter.push(&[&x[skip..]]);
        meter.integrated_lufs()
    };
    let change = lufs(output) - lufs(input);
    if change.is_finite() { change } else { 0.0 }
}

/// Deterministic pink noise (xorshift white through Paul Kellett's economy
/// filter) at [`STAGE_LOUDNESS_RMS_DB`].
pub(crate) fn stage_loudness_reference() -> Vec<f64> {
    let n = (STAGE_LOUDNESS_SECONDS * STAGE_LOUDNESS_RATE) as usize;
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
    let g = crate::db_to_lin(STAGE_LOUDNESS_RMS_DB) / rms;
    x.iter_mut().for_each(|v| *v *= g);
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(fs: f64, freq: f64, amplitude: f64, seconds: f64) -> Vec<f64> {
        let n = (seconds * fs) as usize;
        (0..n)
            .map(|i| amplitude * (2.0 * PI * freq * i as f64 / fs).sin())
            .collect()
    }

    #[test]
    fn a_weighting_reference_values() {
        assert!(
            a_weighting_db(1000.0).abs() < 1e-12,
            "{}",
            a_weighting_db(1000.0)
        );
        let table = [
            (20.0, -50.4),
            (31.5, -39.5),
            (100.0, -19.1),
            (1000.0, 0.0),
            (4000.0, 0.96),
            (10_000.0, -2.5),
            (20_000.0, -9.3),
        ];
        for (f, expected) in table {
            let got = a_weighting_db(f);
            assert!((got - expected).abs() < 0.15, "{f} Hz: {got} vs {expected}");
        }
        assert_eq!(a_weighting_db(0.0), f64::NEG_INFINITY);
        assert_eq!(a_weighting_db(-5.0), f64::NEG_INFINITY);
    }

    #[test]
    fn analytic_offset_identical_is_zero() {
        let power = |f: f64| 1.0 + (f / 1000.0).sin().abs();
        let offset = analytic_offset_db(&power, &power, 48_000.0);
        assert_eq!(offset, 0.0);
    }

    #[test]
    fn analytic_offset_three_db_flat() {
        let a = |f: f64| 1.0 + 0.5 * (f / 700.0).cos();
        let b = |f: f64| a(f) * 10f64.powf(3.0 / 10.0);
        let offset = analytic_offset_db(&a, &b, 48_000.0);
        assert!((offset + 3.0).abs() < 1e-9, "{offset}");
    }

    #[test]
    fn low_frequency_boost_matters_much_less_than_midrange() {
        // A +20 dB narrow boost at f0.  A-weighting is ~-50 dB at 20 Hz, so
        // boosting there barely changes the A-weighted loudness; at 3 kHz it
        // does.
        let bump = |f0: f64| {
            move |f: f64| {
                let x = (f / f0).ln() / 0.02;
                1.0 + 100.0 * (-x * x).exp()
            }
        };
        let flat = |_: f64| 1.0;
        let at_20 = analytic_offset_db(&flat, &bump(20.0), 48_000.0);
        let at_3k = analytic_offset_db(&flat, &bump(3000.0), 48_000.0);
        assert!(at_20.abs() < 0.05, "20 Hz offset {at_20}");
        assert!(at_3k < -2.0, "3 kHz offset {at_3k}");
        assert!(at_20.abs() < at_3k.abs() / 100.0);
    }

    #[test]
    fn analytic_offset_handles_degenerate_inputs() {
        let zero = |_: f64| 0.0;
        let one = |_: f64| 1.0;
        assert_eq!(analytic_offset_db(&zero, &one, 48_000.0), 0.0);
        assert_eq!(analytic_offset_db(&one, &zero, 48_000.0), 0.0);
        assert_eq!(analytic_offset_db(&one, &one, 0.0), 0.0);
    }

    #[test]
    fn a_weighted_meter_matches_analytic_curve() {
        // 0.5 amplitude sine: a pure tone at f must read its level plus the
        // A-weighting at f.
        for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            for freq in [100.0, 1000.0, 4000.0, 8000.0, 16_000.0] {
                let x = sine(fs, freq, 0.5, 1.0);
                let mut meter = AWeightedMeter::new(fs, 1);
                meter.push(&[&x]);
                let expected = 20.0 * (0.5 / 2f64.sqrt()).log10() + a_weighting_db(freq);
                let error = meter.rms_db() - expected;
                assert!(error.abs() < 0.5, "fs {fs} f {freq}: {error} dB");
            }
        }

        // 20 Hz sits where A-weighting is -50 dB.  The filter's start-up
        // transient (its natural frequency is right there) takes over 100 ms
        // to die away, so measure it over a long block.
        for fs in [44_100.0, 48_000.0] {
            let x = sine(fs, 20.0, 0.5, 5.0);
            let mut meter = AWeightedMeter::new(fs, 1);
            meter.push(&[&x]);
            let expected = 20.0 * (0.5 / 2f64.sqrt()).log10() + a_weighting_db(20.0);
            let error = meter.rms_db() - expected;
            assert!(error.abs() < 0.5, "fs {fs} f 20: {error} dB");
        }
    }

    #[test]
    fn a_weighted_meter_summed_channels_and_reset() {
        let fs = 48_000.0;
        let x = sine(fs, 1000.0, 0.5, 0.25);
        let mut mono = AWeightedMeter::new(fs, 1);
        mono.push(&[&x]);
        let mono_db = mono.rms_db();
        let mut stereo = AWeightedMeter::new(fs, 2);
        stereo.push(&[&x, &x]);
        // Two identical channels sum their mean squares: +3.01 dB.
        assert!((stereo.rms_db() - mono_db - 3.010_299_956_639_812).abs() < 0.01);

        let before = mono.rms_db();
        mono.reset();
        assert_eq!(mono.rms_db(), f64::NEG_INFINITY);
        mono.push(&[&x]);
        assert!((mono.rms_db() - before).abs() < 1e-9);
    }

    #[test]
    fn k_weighting_matches_published_48k_coefficients() {
        let shelf = k_shelf(48_000.0);
        let expected_shelf = [
            1.535_124_859_586_97,
            -2.691_696_189_406_38,
            1.198_392_810_852_85,
            -1.690_659_293_182_41,
            0.732_480_774_215_85,
        ];
        for (a, b) in shelf.iter().zip(expected_shelf) {
            assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        }
        let hp = k_high_pass(48_000.0);
        let expected_hp = [1.0, -2.0, 1.0, -1.990_047_454_833_98, 0.990_072_250_366_21];
        for (a, b) in hp.iter().zip(expected_hp) {
            assert!((a - b).abs() < 1e-12, "{a} vs {b}");
        }
    }

    #[test]
    fn lufs_of_minus_23_dbfs_sine() {
        for fs in [44_100.0, 48_000.0, 96_000.0, 192_000.0] {
            let amplitude = 10f64.powf(-23.0 / 20.0);
            let x = sine(fs, 1000.0, amplitude, 2.0);
            let mut meter = LufsMeter::new(fs, 2);
            meter.push(&[&x, &x]);
            let lufs = meter.integrated_lufs();
            assert!((lufs + 23.0).abs() < 0.1, "fs {fs}: {lufs} LUFS");
        }
    }

    #[test]
    fn lufs_gates_silence_and_short_signals() {
        let mut meter = LufsMeter::new(48_000.0, 2);
        assert_eq!(meter.integrated_lufs(), f64::NEG_INFINITY);
        meter.push(&[&vec![0.0; 4800], &vec![0.0; 4800]]);
        assert_eq!(meter.integrated_lufs(), f64::NEG_INFINITY);

        // -80 dBFS is below the absolute gate.
        let x = sine(48_000.0, 1000.0, 10f64.powf(-80.0 / 20.0), 1.0);
        let mut meter = LufsMeter::new(48_000.0, 2);
        meter.push(&[&x, &x]);
        assert_eq!(meter.integrated_lufs(), f64::NEG_INFINITY);
    }

    #[test]
    fn lufs_relative_gate_ignores_quiet_tail() {
        // Loud 1 s, then a 3 s section 30 dB quieter.  The quiet blocks are
        // below the relative gate, so the result stays close to the loud part.
        let fs = 48_000.0;
        let loud = sine(fs, 1000.0, 10f64.powf(-20.0 / 20.0), 1.0);
        let quiet = sine(fs, 1000.0, 10f64.powf(-50.0 / 20.0), 3.0);
        let mut x = loud;
        x.extend_from_slice(&quiet);
        let mut meter = LufsMeter::new(fs, 2);
        meter.push(&[&x, &x]);
        let lufs = meter.integrated_lufs();
        assert!((lufs + 20.0).abs() < 1.0, "{lufs} LUFS");
    }

    /// BS.1770-4 integrated loudness the direct way: K-weight, keep every
    /// 400 ms block (75 % overlap), gate the stored list exactly.
    fn exact_lufs(chans: &[&[f64]], fs: f64) -> f64 {
        let shelf = k_shelf(fs);
        let hp = k_high_pass(fs);
        let hop = (HOP_SECONDS * fs).round() as usize;
        let block = (BLOCK_SECONDS * fs).round() as usize;
        let frames = chans[0].len();
        let mut hop_energy = vec![0.0; frames / hop];
        for ch in chans {
            let mut s0 = [0.0; 2];
            let mut s1 = [0.0; 2];
            for (i, &x) in ch.iter().enumerate().take(hop_energy.len() * hop) {
                let y = biquad_step(&hp, &mut s1, biquad_step(&shelf, &mut s0, x));
                hop_energy[i / hop] += y * y;
            }
        }
        let blocks: Vec<f64> = hop_energy
            .windows(4)
            .map(|w| w.iter().sum::<f64>() / block as f64)
            .collect();
        let abs: Vec<f64> = blocks
            .into_iter()
            .filter(|&z| block_loudness(z) > ABSOLUTE_GATE_LUFS)
            .collect();
        if abs.is_empty() {
            return f64::NEG_INFINITY;
        }
        let thr = block_loudness(abs.iter().sum::<f64>() / abs.len() as f64) - RELATIVE_GATE_LU;
        let rel: Vec<f64> = abs
            .into_iter()
            .filter(|&z| block_loudness(z) > thr)
            .collect();
        if rel.is_empty() {
            return f64::NEG_INFINITY;
        }
        block_loudness(rel.iter().sum::<f64>() / rel.len() as f64)
    }

    /// Program-like material: noise whose level jumps every 50-350 ms over
    /// `lo..hi` dBFS, so many blocks sit near the relative threshold.
    fn dynamic_program(fs: f64, seconds: f64, lo: f64, hi: f64, seed: u64) -> (Vec<f64>, Vec<f64>) {
        let mut state = seed;
        let mut rnd = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 11) as f64) / 9_007_199_254_740_992.0
        };
        let n = (seconds * fs) as usize;
        let (mut l, mut r) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let mut left = 0usize;
        let mut gain = 0.0;
        for i in 0..n {
            if left == 0 {
                left = ((0.05 + 0.3 * rnd()) * fs) as usize;
                gain = 10f64.powf((lo + (hi - lo) * rnd()) / 20.0);
            }
            left -= 1;
            let tone = (2.0 * PI * 440.0 * i as f64 / fs).sin();
            l.push(gain * (0.5 * tone + (rnd() - 0.5)));
            r.push(gain * (0.5 * tone + (rnd() - 0.5)));
        }
        (l, r)
    }

    #[test]
    fn histogram_lufs_matches_exact_gating() {
        let cases = [
            (48_000.0, 120.0, -75.0, -5.0, 1u64),
            (44_100.0, 90.0, -40.0, -20.0, 2),
            (96_000.0, 60.0, -90.0, 0.0, 3),
            (48_000.0, 30.0, -72.0, -60.0, 4),
        ];
        for (fs, seconds, lo, hi, seed) in cases {
            let (l, r) = dynamic_program(fs, seconds, lo, hi, seed);
            let exact = exact_lufs(&[&l, &r], fs);
            let mut meter = LufsMeter::new(fs, 2);
            // Odd push sizes so hop boundaries fall mid-call.
            let mut i = 0;
            let mut step = 1;
            while i < l.len() {
                let n = step.min(l.len() - i);
                meter.push(&[&l[i..i + n], &r[i..i + n]]);
                i += n;
                step = step * 7 % 4093 + 1;
            }
            let got = meter.integrated_lufs();
            eprintln!(
                "case {seed}: histogram {got:.5} exact {exact:.5} diff {:.2e} LU",
                got - exact
            );
            assert!(exact.is_finite(), "case {seed}: exact {exact}");
            assert!(
                (got - exact).abs() < 0.01,
                "case {seed}: histogram {got} vs exact {exact} LUFS"
            );
        }
    }

    #[test]
    fn histogram_lufs_extreme_levels() {
        // Above the histogram's +10 LUFS top (overflow bin) and mixed with
        // blocks near the absolute gate.
        let fs = 48_000.0;
        let mut x = sine(fs, 1000.0, 8.0, 3.0); // ~ +15 LUFS stereo
        x.extend(sine(fs, 1000.0, 10f64.powf(-68.0 / 20.0), 3.0));
        let exact = exact_lufs(&[&x, &x], fs);
        let mut meter = LufsMeter::new(fs, 2);
        meter.push(&[&x, &x]);
        assert!(
            (meter.integrated_lufs() - exact).abs() < 0.01,
            "{} vs {exact}",
            meter.integrated_lufs()
        );

        // Only blocks just above the gate (mono -66 dBFS: about -69.7 LUFS).
        let y = sine(fs, 1000.0, 10f64.powf(-66.0 / 20.0), 2.0);
        let mut meter = LufsMeter::new(fs, 1);
        meter.push(&[&y]);
        let exact = exact_lufs(&[&y], fs);
        assert!(exact.is_finite() && exact < -69.0, "{exact}");
        assert!(
            (meter.integrated_lufs() - exact).abs() < 0.01,
            "{} vs {exact}",
            meter.integrated_lufs()
        );
    }

    #[test]
    fn lufs_reset_restarts() {
        let fs = 48_000.0;
        let x = sine(fs, 1000.0, 0.1, 1.0);
        let mut meter = LufsMeter::new(fs, 2);
        meter.push(&[&x, &x]);
        let first = meter.integrated_lufs();
        meter.reset();
        assert_eq!(meter.integrated_lufs(), f64::NEG_INFINITY);
        meter.push(&[&x, &x]);
        assert!((meter.integrated_lufs() - first).abs() < 1e-9);
    }
}
