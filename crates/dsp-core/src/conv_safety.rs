//! Room-correction guards on a chain's combined convolution, per output
//! row (a per-channel IR is a row of one entry; a filter matrix output sums
//! its entries):
//!
//! - **Peak gain:** the certified sine worst-case gain the A/B shared
//!   headroom uses ([`crate::ab_level::row_peak_bound`]): every entry of
//!   the row driven at one frequency in the phase that adds up. The control
//!   plane refuses a chain whose row peaks over its boost cap.
//! - **Low-band gain:** the largest `Σ_i |F_i(f)|` below a frequency (the
//!   subsonic limit), and where it is, on a grid of at most 0.5 Hz (finer
//!   for long filters). A warning, not a bound: a peak between bins can
//!   read a little low.
//!
//! Runs off the audio thread, when the convolution is validated.

use crate::Complex64;
use crate::ab_level::row_peak_bound;
use crate::lin_to_db;
use crate::matrix::ConvIrs;
use realfft::RealFftPlanner;

/// The low-band grid at most: 2 Mi points (0.023 Hz at 48 kHz, 0.37 Hz at
/// 768 kHz). A longer filter is wrapped into it (exact at the bins).
const LOW_GRID_MAX: usize = 1 << 21;

/// One output row's gains.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowGain {
    /// The output channel.
    pub output: usize,
    /// Certified sine worst-case gain over the whole band, dB.
    pub peak_db: f64,
    /// The largest gain below the limit, dB (`-inf` for a silent row).
    pub low_db: f64,
    /// Where [`Self::low_db`] is, Hz.
    pub low_hz: f64,
}

/// The gains of every output of `irs` (`channels` outputs; a mono IR serves
/// each), at `rate`, the low band below `below_hz`.
pub fn row_gains(irs: &ConvIrs, rate: f64, channels: usize, below_hz: f64) -> Vec<RowGain> {
    match irs {
        ConvIrs::Channels(list) if list.is_empty() => Vec::new(),
        ConvIrs::Channels(list) => {
            // A mono IR serves every channel: measure it once.
            let rows: Vec<(f64, f64, f64)> = list
                .iter()
                .map(|ir| row_terms(&[ir.as_slice()], rate, below_hz))
                .collect();
            (0..channels)
                .map(|c| {
                    let (peak_db, low_db, low_hz) = rows[c.min(rows.len() - 1)];
                    RowGain {
                        output: c,
                        peak_db,
                        low_db,
                        low_hz,
                    }
                })
                .collect()
        }
        ConvIrs::Matrix(m) => (0..m.channels())
            .map(|out| {
                let entries: Vec<&[f64]> =
                    (0..m.channels()).filter_map(|i| m.get(out, i)).collect();
                let (peak_db, low_db, low_hz) = row_terms(&entries, rate, below_hz);
                RowGain {
                    output: out,
                    peak_db,
                    low_db,
                    low_hz,
                }
            })
            .collect(),
    }
}

/// A row's peak (dB), and its low-band maximum (dB) and where it is (Hz).
fn row_terms(entries: &[&[f64]], rate: f64, below_hz: f64) -> (f64, f64, f64) {
    let unit: Vec<(f64, &[f64])> = entries.iter().map(|t| (1.0, *t)).collect();
    let peak = row_peak_bound(&unit);
    let (low, at) = low_band_peak(entries, rate, below_hz);
    (lin_to_db(peak), lin_to_db(low), at)
}

/// The largest `Σ_i |F_i(f)|` over the bins below `below_hz` (DC
/// included), and its frequency.
fn low_band_peak(entries: &[&[f64]], rate: f64, below_hz: f64) -> (f64, f64) {
    let len = entries.iter().map(|t| t.len()).max().unwrap_or(0);
    if len == 0 || rate.is_nan() || rate <= 0.0 {
        return (0.0, 0.0);
    }
    let n = (4 * len)
        .max((2.0 * rate).ceil() as usize)
        .next_power_of_two()
        .clamp(16, LOW_GRID_MAX);
    let bin_hz = rate / n as f64;
    // Bins strictly below the limit (at least DC).
    let bins = ((below_hz / bin_hz).ceil() as usize).clamp(1, n / 2 + 1);
    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let mut input = vec![0.0; n];
    let mut spectrum = vec![Complex64::new(0.0, 0.0); n / 2 + 1];
    let mut sum = vec![0.0; bins];
    for taps in entries {
        input.fill(0.0);
        for (k, &v) in taps.iter().enumerate() {
            input[k % n] += v;
        }
        // Lengths are the plan's own; the call cannot fail.
        let _ = fft.process(&mut input, &mut spectrum);
        for (acc, h) in sum.iter_mut().zip(&spectrum) {
            *acc += h.norm();
        }
    }
    let (k, max) =
        sum.iter().enumerate().fold(
            (0, 0.0_f64),
            |(bk, bm), (k, &v)| {
                if v > bm { (k, v) } else { (bk, bm) }
            },
        );
    (max, k as f64 * bin_hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2nd-order high-pass at 30 Hz (RBJ, Q 0.707), as a long FIR.
    fn highpass(rate: f64, f0: f64, len: usize) -> Vec<f64> {
        biquad_ir(rate, f0, len, true, 0.0)
    }

    /// A low shelf of `gain_db` below `f0` (RBJ, S = 1), as a long FIR.
    fn low_shelf(rate: f64, f0: f64, gain_db: f64, len: usize) -> Vec<f64> {
        biquad_ir(rate, f0, len, false, gain_db)
    }

    fn biquad_ir(rate: f64, f0: f64, len: usize, hp: bool, gain_db: f64) -> Vec<f64> {
        let w = 2.0 * std::f64::consts::PI * f0 / rate;
        let (c, s) = (w.cos(), w.sin());
        let (b, a) = if hp {
            let alpha = s / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
            (
                [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
                [1.0 + alpha, -2.0 * c, 1.0 - alpha],
            )
        } else {
            let a_ = 10f64.powf(gain_db / 40.0);
            let alpha = s / 2.0 * 2f64.sqrt();
            let sq = 2.0 * a_.sqrt() * alpha;
            (
                [
                    a_ * ((a_ + 1.0) - (a_ - 1.0) * c + sq),
                    2.0 * a_ * ((a_ - 1.0) - (a_ + 1.0) * c),
                    a_ * ((a_ + 1.0) - (a_ - 1.0) * c - sq),
                ],
                [
                    (a_ + 1.0) + (a_ - 1.0) * c + sq,
                    -2.0 * ((a_ - 1.0) + (a_ + 1.0) * c),
                    (a_ + 1.0) + (a_ - 1.0) * c - sq,
                ],
            )
        };
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        (0..len)
            .map(|k| {
                let x = if k == 0 { 1.0 } else { 0.0 };
                let y = (b[0] * x + b[1] * x1 + b[2] * x2 - a[1] * y1 - a[2] * y2) / a[0];
                (x2, x1, y2, y1) = (x1, x, y1, y);
                y
            })
            .collect()
    }

    #[test]
    fn high_pass_has_no_low_band_gain_and_bass_boost_does() {
        let rate = 48_000.0;
        let hp = highpass(rate, 30.0, 1 << 15);
        let g = row_gains(&ConvIrs::Channels(vec![hp]), rate, 2, 20.0);
        assert_eq!(g.len(), 2);
        assert!(g[0].low_db < -3.0, "{g:?}");
        assert!(g[0].peak_db < 0.1, "{g:?}");

        let shelf = low_shelf(rate, 40.0, 8.0, 1 << 15);
        let g = row_gains(
            &ConvIrs::Channels(vec![shelf.clone(), shelf]),
            rate,
            2,
            20.0,
        );
        assert!(g[1].low_db > 6.0 && g[1].low_db < 8.1, "{g:?}");
        assert!(g[1].low_hz < 20.0, "{g:?}");
        assert!(g[1].peak_db >= g[1].low_db - 1e-9, "{g:?}");
    }
}
