//! A/B level matching and shared headroom.
//!
//! - **Never boost.** For B's offset `o` (dB that make B as loud as A) the
//!   pair plays `g_A = min(0, −o)` and `g_B = g_A + o`: the louder side is
//!   cut, nothing is raised.
//! - **Shared headroom** `H` cuts both sides by the same amount, so it never
//!   changes the comparison. `auto` sizes it from each side's predicted
//!   worst-case peak gain `P` (sine worst case: EQ `max |H|`, convolution
//!   `max |H|` (a filter matrix output: its row summed, every input in the
//!   phase that adds up; both certified between bins, [`row_peak_bound`]),
//!   the crossfeed bound, the upsampler, the harmonics reach), after the
//!   output headroom. A alone plays at its natural gain (the output
//!   headroom and the limiter handle its peaks), so the pair cuts only
//!   what B needs beyond A's own peaks:
//!   `H = max(0, max(g_A + P_A, g_B + P_B) - max(0, P_A))`,
//!   capped at [`AB_HEADROOM_MAX_DB`]. Turning A/B on never turns A down
//!   unless B peaks higher than A.
//! - **The analytic offset** integrates each side's power response over a
//!   pink reference with K- (default) or A-weighting
//!   ([`crate::loudness::weighted_offset_db`]). The terms: DC block, EQ (with
//!   tilt and preamp), crossfeed as a stereo stage at correlation
//!   [`crate::crossfeed::MATCHED_CORRELATION`], the convolution IRs' magnitude
//!   (from their FFT; a filter matrix's rows composed with the crossfeed
//!   and EQ before it, at the sources' correlation) and, when the two
//!   sides' upsampler designs differ, the upsampler's magnitude (from its
//!   measured impulse response), and the harmonics stage's loudness
//!   change, simulated on loud pink noise.
//!
//! Everything here runs off the audio thread, when the pair is built.

use crate::Complex64;
use crate::biquad::Eq;
use crate::crossfeed::{Crossfeed, MATCHED_CORRELATION};
use crate::dc_block::DcBlock;
use crate::loudness::{Weighting, weighted_offset_db};
use crate::matrix::{ConvIrs, FilterMatrix};
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
                // A's own peaks are what playing A alone already risks:
                // only B's excess over them (or over full scale) is cut.
                let allowed = peak_a_db.max(0.0);
                let need = (gain_a_db + peak_a_db).max(gain_b_db + peak_b_db) - allowed;
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
/// input frame 0), normalised by the ratio (a unity DC gain for a kernel
/// without a folded gain) and scaled by `scale`, as a spectrum at the
/// output rate. `rt` is reset afterwards.
pub fn upsampler_spectrum(
    rt: &mut dyn UpsamplerRuntime,
    channels: usize,
    max_block: usize,
    input_rate: f64,
    scale: f64,
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
    PowerSpectrum::of_taps(&taps, input_rate * ratio, scale / ratio)
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
    /// The harmonics stage's peak bounds ([`crate::harmonics::Harmonics::peak_bounds`];
    /// empty when off): it plays at its natural level, so its worst case
    /// enters the peak prediction.
    pub harmonics_peaks: &'a [f64],
}

/// The certified peak's grid, over the row's longest filter: 64 points per
/// tap (a margin of at most 0.005 dB, [`row_peak_bound`]).
const PEAK_OVERSAMPLE: usize = 64;
/// The certified peak's grid at most: 4 Mi points, about 80 MiB of
/// workspace (64 points per tap up to 64 Ki taps, then 32, 16, 8 and 4 per
/// tap up to 1 Mi taps: margins up to 0.021, 0.085, 0.35 and 1.6 dB).
/// Longer filters take their L1 norm instead (valid for any input, looser).
const PEAK_GRID_MAX: usize = 1 << 22;
/// The matrix power spectra's grid at most: 256 Ki points (0.18 Hz bins at
/// 48 kHz). A longer filter is wrapped into it, which keeps the values at
/// the bins exact.
const POWER_GRID_MAX: usize = 1 << 18;

/// Certified sine worst-case gains of a row of filters `(w_i, taps_i)`, on
/// a grid of at most `grid_max` points: the row's (at least `max_θ Σ_i
/// |w_i| |F_i(e^{iθ})|`) and each filter's own (`max_θ |F_i|`, unweighted).
///
/// Every filter is evaluated on one common grid of `N` points,
/// [`PEAK_OVERSAMPLE`] per tap of the longest filter (fewer as the grid
/// reaches `grid_max`, never under 4), and the grid's largest sum `G` is
/// raised by the between-bin margin `1 / sqrt(1 - (π d / N)² / 2)`, `d` the
/// longest filter's length less one. Why it bounds: for any fixed phases
/// `φ_i`, `q(θ) = Σ_i |w_i| e^{iφ_i} F_i(e^{iθ})` is a polynomial of degree
/// `d`, so `f = |q|²` is a real trigonometric polynomial of degree `d`; at
/// its maximum `M²`, `f' = 0`, and Bernstein's inequality gives `|f''| <=
/// d² M²`, so within `π / N` of it (where a grid point lies) `f >= M² (1 -
/// (π d / N)² / 2)`. The grid point's sum is at least `|q|` there, so `M <=
/// G × margin` for every choice of phases (a filter of a lower degree is a
/// polynomial of degree `d` too). When 4 points per tap would not fit in
/// `grid_max`, each filter's L1 norm (`Σ |h|`, a bound for any input) is
/// taken instead.
fn certified_peaks(row: &[(f64, &[f64])], grid_max: usize) -> (f64, Vec<f64>) {
    let len = row.iter().map(|(_, t)| t.len()).max().unwrap_or(0);
    if len == 0 {
        return (0.0, vec![0.0; row.len()]);
    }
    if (4 * len).next_power_of_two() > grid_max {
        let l1: Vec<f64> = row
            .iter()
            .map(|(_, t)| t.iter().map(|v| v.abs()).sum())
            .collect();
        let sum = row.iter().zip(&l1).map(|((w, _), l)| w.abs() * l).sum();
        return (sum, l1);
    }
    let n = (PEAK_OVERSAMPLE * len)
        .next_power_of_two()
        .min(grid_max)
        .max(16);
    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let mut input = vec![0.0; n];
    let mut spectrum = vec![Complex64::new(0.0, 0.0); n / 2 + 1];
    let mut sum = vec![0.0; n / 2 + 1];
    let mut own = Vec::with_capacity(row.len());
    for &(w, taps) in row {
        input.fill(0.0);
        input[..taps.len()].copy_from_slice(taps);
        // Lengths are the plan's own; the call cannot fail.
        let _ = fft.process(&mut input, &mut spectrum);
        let mut max = 0.0_f64;
        for (acc, h) in sum.iter_mut().zip(&spectrum) {
            let m = h.norm();
            *acc += w.abs() * m;
            max = max.max(m);
        }
        own.push(max);
    }
    // Bins 0..=N/2 cover [0, π]; |F| is even in θ for real taps, so every
    // frequency lies within π / N of one.
    let grid = sum.iter().fold(0.0_f64, |m, &v| m.max(v));
    let x = std::f64::consts::PI * (len - 1) as f64 / n as f64;
    let margin = 1.0 / (1.0 - 0.5 * x * x).sqrt();
    (grid * margin, own.into_iter().map(|m| m * margin).collect())
}

/// A certified sine worst-case gain of a row of filters `(w_i, taps_i)`:
/// at least `max_θ Σ_i |w_i| |F_i(e^{iθ})|` over the whole band (every
/// filter driven by a sine of one frequency in the phase that adds up), on
/// a bounded grid (`PEAK_GRID_MAX`; the L1 norm past it). A bin maximum
/// alone can miss a peak between bins by up to 3.9 dB on a grid of one
/// point per tap.
pub fn row_peak_bound(row: &[(f64, &[f64])]) -> f64 {
    certified_peaks(row, PEAK_GRID_MAX).0
}

/// One output of a filter matrix: its inputs, the products `F_a F_b*` of
/// its entries' responses on a uniform grid (pairs `a <= b`, row-major
/// over `inputs`) and its certified peaks (the row's at unit weights, and
/// each entry's).
#[derive(Clone, Debug)]
struct MatrixRow {
    inputs: Vec<usize>,
    pairs: Vec<Vec<Complex64>>,
    peak_sum: f64,
    peak_own: Vec<f64>,
}

/// A filter matrix's A/B terms: per output, the entries' cross spectra
/// for the loudness and the certified peaks. Built once, the taps not
/// kept; workspace bounded (`POWER_GRID_MAX`, `PEAK_GRID_MAX`).
#[derive(Clone, Debug)]
pub struct MatrixSpectra {
    channels: usize,
    bin_hz: f64,
    rows: Vec<MatrixRow>,
}

/// Index of the pair `(a, b)`, `a <= b`, among `n` entries.
fn pair_index(a: usize, b: usize, n: usize) -> usize {
    a * n - a * (a + 1) / 2 + b
}

impl MatrixSpectra {
    pub fn of_matrix(m: &FilterMatrix, rate: f64) -> Self {
        let c = m.channels();
        let longest = m.max_len();
        let n = longest
            .max(rate.ceil() as usize)
            .max(2)
            .next_power_of_two()
            .min(POWER_GRID_MAX);
        let mut planner = RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let mut input = vec![0.0; n];
        let rows = (0..c)
            .map(|out| {
                let entries: Vec<(usize, &[f64])> = (0..c)
                    .filter_map(|i| m.get(out, i).map(|t| (i, t)))
                    .collect();
                // Each entry's response at the bins, the taps wrapped into
                // the grid (exact at the bins).
                let spectra: Vec<Vec<Complex64>> = entries
                    .iter()
                    .map(|(_, taps)| {
                        input.fill(0.0);
                        for (k, &v) in taps.iter().enumerate() {
                            input[k % n] += v;
                        }
                        let mut bins = vec![Complex64::new(0.0, 0.0); n / 2 + 1];
                        let _ = fft.process(&mut input, &mut bins);
                        bins
                    })
                    .collect();
                let k = spectra.len();
                let mut pairs = Vec::with_capacity(k * (k + 1) / 2);
                for a in 0..k {
                    for b in a..k {
                        pairs.push(
                            spectra[a]
                                .iter()
                                .zip(&spectra[b])
                                .map(|(x, y)| x * y.conj())
                                .collect(),
                        );
                    }
                }
                let unit: Vec<(f64, &[f64])> = entries.iter().map(|&(_, t)| (1.0, t)).collect();
                let (peak_sum, peak_own) = certified_peaks(&unit, PEAK_GRID_MAX);
                MatrixRow {
                    inputs: entries.iter().map(|&(i, _)| i).collect(),
                    pairs,
                    peak_sum,
                    peak_own,
                }
            })
            .collect();
        Self {
            channels: c,
            bin_hz: rate / n as f64,
            rows,
        }
    }

    /// A pair's product at `f`, linearly interpolated between bins (0
    /// above Nyquist). Products of entries do not rotate where both are
    /// delayed alike (and `|F|²` never does): no null between bins.
    fn product_at(&self, bins: &[Complex64], f: f64) -> Complex64 {
        let x = f / self.bin_hz;
        if !x.is_finite() || x < 0.0 {
            return Complex64::new(0.0, 0.0);
        }
        let i = x.floor() as usize;
        match (bins.get(i), bins.get(i + 1)) {
            (Some(&h0), Some(&h1)) => h0 + (h1 - h0) * (x - i as f64),
            (Some(&h0), None) => h0,
            _ => Complex64::new(0.0, 0.0),
        }
    }

    /// Output `out`'s power at `f` for source channels at unit power each
    /// and inter-channel correlation `rho`, the matrix's input `i` taking
    /// `pre(i, k)` of source channel `k` (the crossfeed and the EQ before
    /// the matrix): `Σ_a Σ_b G_ab Q_ab`, `G_ab = F_a F_b*` (interpolated
    /// between bins), `Q_ab = Σ_k Σ_l ρ_kl pre(a, k) pre(b, l)*` (exact at
    /// `f`), `ρ_kk = 1`.
    fn power(&self, out: usize, f: f64, rho: f64, pre: &dyn Fn(usize, usize) -> Complex64) -> f64 {
        let row = &self.rows[out];
        let n = row.inputs.len();
        let c = self.channels;
        let q = |a: usize, b: usize| -> Complex64 {
            let mut sum = Complex64::new(0.0, 0.0);
            for k in 0..c {
                for l in 0..c {
                    let r = if k == l { 1.0 } else { rho };
                    sum += pre(a, k) * pre(b, l).conj() * r;
                }
            }
            sum
        };
        let mut p = 0.0;
        for a in 0..n {
            for b in a..n {
                let g = self.product_at(&row.pairs[pair_index(a, b, n)], f);
                let term = (g * q(row.inputs[a], row.inputs[b])).re;
                p += if a == b { term } else { 2.0 * term };
            }
        }
        p.max(0.0)
    }

    /// Output `out`'s certified sine worst-case gain, each input scaled by
    /// `pre_peak(i)` (its EQ `max |H|`): the smaller of the row's bound at
    /// unit weights times the largest weight and the weighted sum of each
    /// entry's bound (both bound `max_f Σ_i w_i |F_i(f)|`). 0 for an output
    /// with no entry.
    fn peak(&self, out: usize, pre_peak: &dyn Fn(usize) -> f64) -> f64 {
        let row = &self.rows[out];
        let w: Vec<f64> = row.inputs.iter().map(|&i| pre_peak(i).abs()).collect();
        let largest = w.iter().fold(0.0_f64, |m, &v| m.max(v));
        let each: f64 = w.iter().zip(&row.peak_own).map(|(w, p)| w * p).sum();
        (largest * row.peak_sum).min(each)
    }
}

/// A convolution item's flat gain for the config-only estimate (the IR
/// samples are not known there): its `gain_db`, times, for a filter
/// matrix, the largest row's sum of entry gains (an output summing two
/// inputs can reach twice the level).
pub fn config_conv_gain(conv: &audivane_protocol::ConvolutionConfig) -> f64 {
    let row_sum = conv.matrix.as_ref().map_or(1.0, |entries| {
        let outs = entries.iter().map(|e| e.output).max().map_or(0, |m| m + 1);
        (0..outs)
            .map(|o| {
                entries
                    .iter()
                    .filter(|e| e.output == o)
                    .map(|e| db_to_lin(e.gain_db))
                    .sum::<f64>()
            })
            .fold(0.0, f64::max)
    });
    db_to_lin(conv.gain_db) * row_sum
}

/// The convolution stage's contribution.
#[derive(Clone, Debug)]
pub enum ConvTerm {
    /// A flat linear gain (none: 1; config-only estimate:
    /// [`config_conv_gain`] of the enabled entries).
    Gain(f64),
    /// The combined IRs' spectra and certified peaks ([`row_peak_bound`]):
    /// one (mono) or one per channel.
    Irs {
        spectra: Vec<PowerSpectrum>,
        peaks: Vec<f64>,
    },
    /// A filter matrix's spectra.
    Matrix(MatrixSpectra),
}

impl ConvTerm {
    /// From combined IRs (already scaled by each entry's `gain_db`).
    pub fn of_irs(irs: &[Vec<f64>], rate: f64) -> Self {
        Self::Irs {
            spectra: irs
                .iter()
                .map(|ir| PowerSpectrum::of_taps(ir, rate, 1.0))
                .collect(),
            peaks: irs.iter().map(|ir| row_peak_bound(&[(1.0, ir)])).collect(),
        }
    }

    /// From a chain's combined convolution (per channel or a matrix).
    pub fn of_conv(irs: &ConvIrs, rate: f64) -> Self {
        match irs {
            ConvIrs::Channels(irs) => Self::of_irs(irs, rate),
            ConvIrs::Matrix(m) => Self::Matrix(MatrixSpectra::of_matrix(m, rate)),
        }
    }

    /// Channel `c`'s entry (a mono IR serves every channel).
    fn channel<T>(irs: &[T], c: usize) -> &T {
        &irs[c.min(irs.len() - 1)]
    }

    /// |H|² of channel `c` at `f` (the per-channel terms; a matrix is
    /// handled by [`SideTerms`] itself).
    fn power(&self, c: usize, f: f64) -> f64 {
        match self {
            Self::Gain(g) => g * g,
            Self::Irs { spectra, .. } if spectra.is_empty() => 1.0,
            Self::Irs { spectra, .. } => Self::channel(spectra, c).power_at(f),
            Self::Matrix(_) => unreachable!("matrix terms are taken per output"),
        }
    }

    /// Certified `max |H|` of channel `c` over the band (per-channel
    /// terms).
    fn peak(&self, c: usize) -> f64 {
        match self {
            Self::Gain(g) => g.abs(),
            Self::Irs { peaks, .. } if peaks.is_empty() => 1.0,
            Self::Irs { peaks, .. } => *Self::channel(peaks, c),
            Self::Matrix(_) => unreachable!("matrix terms are taken per output"),
        }
    }
}

impl SideTerms<'_> {
    /// Mean power response of the left and right channels at `f_hz`, each
    /// channel's EQ and IR taken together.
    pub fn power(&self, f_hz: f64) -> f64 {
        let dc = self.dc.map_or(1.0, |d| d.response(f_hz).norm_sqr());
        let up = self.upsampler.as_ref().map_or(1.0, |u| u.power_at(f_hz));
        let harm = self.harmonics_gain * self.harmonics_gain;
        if let ConvTerm::Matrix(m) = &self.conv {
            // A matrix mixes the channels after the crossfeed and the EQ:
            // each output's response to each source channel is composed
            // (crossfeed direct and cross paths, the EQ of the matrix
            // input, the entry), then taken at the sources' matched
            // correlation. The crossfeed's own correlation change (and its
            // width) is in the composition, not assumed.
            let one = Complex64::new(1.0, 0.0);
            let paths = self.crossfeed.map(|x| x.stereo_paths(f_hz));
            let pre = |i: usize, k: usize| {
                let x = match paths {
                    Some((d, c)) => {
                        if i == k {
                            d
                        } else {
                            c
                        }
                    }
                    None if i == k => one,
                    None => Complex64::new(0.0, 0.0),
                };
                let eq = self.eq.map_or(one, |e| e.response_channel(i, f_hz));
                x * eq
            };
            let out = |j: usize| m.power(j, f_hz, MATCHED_CORRELATION, &pre);
            return dc * up * harm * 0.5 * (out(0) + out(1));
        }
        let xf = self
            .crossfeed
            .map_or(1.0, |x| x.stereo_power_response(f_hz, MATCHED_CORRELATION));
        let channel = |c: usize| {
            let eq = self
                .eq
                .map_or(1.0, |e| e.response_channel(c, f_hz).norm_sqr());
            eq * self.conv.power(c, f_hz)
        };
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
        let channel_db = |c: usize| match &self.conv {
            ConvTerm::Matrix(m) => lin_to_db(m.peak(c, &|i| db_to_lin(eq_db[i.min(1)]))),
            conv => eq_db[c] + lin_to_db(conv.peak(c)),
        };
        let xf = self.crossfeed.map_or(1.0, Crossfeed::peak_gain_bound);
        let up = self
            .upsampler
            .as_ref()
            .map_or(1.0, |u| u.peak_below(nyquist));
        let total = channel_db(0).max(channel_db(1)) + lin_to_db(xf * up);
        // The harmonics stage sits after the upsampler: it shapes what
        // reaches it within full scale and passes the excess.
        let total = if self.harmonics_peaks.is_empty() || !total.is_finite() {
            total
        } else {
            lin_to_db(crate::harmonics::peak_reach(
                self.harmonics_peaks,
                db_to_lin(total),
            ))
        };
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

    /// The matrix peak bound sums a row: a worst-case input (each input a
    /// sine at the worst bin, phased so every entry of the row adds up)
    /// stays within it, and reaches it, above the larger entry alone.
    #[test]
    fn matrix_peak_bound_holds_on_a_worst_case_input() {
        use crate::Stage;
        use crate::matrix::MatrixConvolver;
        let rate = 48_000.0;
        let mut seed = 3_u64;
        let mut noise = |n: usize| -> Vec<f64> {
            (0..n)
                .map(|k| {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let x = ((seed >> 11) as f64) / ((1u64 << 53) as f64) * 2.0 - 1.0;
                    x * (-(k as f64) / 40.0).exp()
                })
                .collect()
        };
        let mut m = FilterMatrix::new(2);
        m.set(0, 0, noise(200));
        m.set(0, 1, noise(150));
        m.set(1, 1, noise(180));
        let spectra = MatrixSpectra::of_matrix(&m, rate);
        let terms = SideTerms {
            sample_rate: rate,
            dc: None,
            crossfeed: None,
            eq: None,
            eq_profile: None,
            conv: ConvTerm::Matrix(spectra),
            upsampler: None,
            harmonics_gain: 1.0,
            harmonics_peaks: &[],
        };
        let bound = db_to_lin(terms.peak_gain_db());

        // Output 0's worst frequency (on a dense grid of its own) and the
        // phases that line its entries up.
        let n = 1 << 16;
        let mut planner = RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(n);
        let dense = |taps: &[f64]| {
            let mut input = vec![0.0; n];
            input[..taps.len()].copy_from_slice(taps);
            let mut out = vec![Complex64::new(0.0, 0.0); n / 2 + 1];
            let _ = fft.process(&mut input, &mut out);
            out
        };
        let row: Vec<(usize, Vec<Complex64>)> = (0..2)
            .filter_map(|i| m.get(0, i).map(|t| (i, dense(t))))
            .collect();
        let k = (0..n / 2 + 1)
            .max_by(|&a, &b| {
                let sum = |k: usize| row.iter().map(|(_, s)| s[k].norm()).sum::<f64>();
                sum(a).total_cmp(&sum(b))
            })
            .unwrap();
        let f = k as f64 * rate / n as f64;
        let single = row.iter().map(|(_, s)| s[k].norm()).fold(0.0, f64::max);
        let frames = 40_000;
        let x: Vec<Vec<f64>> = (0..2)
            .map(|i| {
                let phase = -row
                    .iter()
                    .find(|(j, _)| *j == i)
                    .map_or(0.0, |(_, s)| s[k].arg());
                (0..frames)
                    .map(|n| (std::f64::consts::TAU * f * n as f64 / rate + phase).cos())
                    .collect()
            })
            .collect();
        let mut y = x.clone();
        let mut conv = MatrixConvolver::new(&m, 512);
        for start in (0..frames).step_by(512) {
            let end = (start + 512).min(frames);
            let (a, b) = y.split_at_mut(1);
            conv.process(&mut [&mut a[0][start..end], &mut b[0][start..end]]);
        }
        let peak = y
            .iter()
            .flat_map(|c| c[2_000..].iter())
            .fold(0.0_f64, |p, &v| p.max(v.abs()));
        assert!(
            peak <= bound * (1.0 + 1e-9),
            "peak {peak} over bound {bound}"
        );
        // Within the certified margin of the true maximum (64 points per
        // tap: at most 0.06 %).
        assert!(peak > bound / 1.001, "peak {peak} well under bound {bound}");
        assert!(bound > 1.2 * single, "the row sums: {bound} vs {single}");
    }
}
