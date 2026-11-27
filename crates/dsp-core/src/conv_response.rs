//! Frequency responses of a chain's convolution for plotting: one curve
//! per filter, or the coherent sum of several (a matrix output row), on a
//! log frequency grid. Magnitude in dB and group delay in milliseconds.
//!
//! Each curve is the filter's DFT on a fine grid (bins at most 0.5 Hz
//! apart, at least the filter's length; a longer filter is wrapped into
//! 2 Mi points, exact at the bins), read at each grid frequency between its
//! two nearest bins. Nothing is smoothed. Group delay is
//! `Re(DFT(n·h) / DFT(h))`, exact at the bins; it is `None` where the
//! magnitude is below [`GD_FLOOR_DB`] (a null has no meaningful delay).
//!
//! Runs off the audio thread.

use crate::Complex64;
use crate::matrix::ConvIrs;
use realfft::RealFftPlanner;

/// The analysis grid at most (as [`crate::conv_safety`]'s low band).
const GRID_MAX: usize = 1 << 21;
/// The finest bin spacing asked for, Hz.
const BIN_HZ: f64 = 0.5;
/// The quietest level reported, dB (silence reads this).
pub const FLOOR_DB: f64 = -200.0;
/// Below this level a curve has no group delay, dB.
pub const GD_FLOOR_DB: f64 = -120.0;

/// The plot's log grid: `steps + 1` points from `lo` to `hi` Hz, the ones
/// above Nyquist (`rate / 2`) left out.
pub fn log_grid(lo: f64, hi: f64, steps: usize, rate: f64) -> Vec<f64> {
    let (a, b) = (lo.ln(), hi.ln());
    (0..=steps)
        .map(|i| (a + (b - a) * i as f64 / steps.max(1) as f64).exp())
        .filter(|&f| f <= rate / 2.0)
        .collect()
}

/// One curve on the grid.
#[derive(Clone, Debug, PartialEq)]
pub struct Curve {
    /// |H(f)|, dB ([`FLOOR_DB`] at the quietest).
    pub mag_db: Vec<f64>,
    /// Group delay, ms (`None` below [`GD_FLOOR_DB`]).
    pub group_delay_ms: Vec<Option<f64>>,
}

/// The response of `Σ_k g_k · h_k` (each entry a filter and its linear
/// gain, all at `rate`) at `freqs` Hz.
pub fn response(entries: &[(&[f64], f64)], rate: f64, freqs: &[f64]) -> Curve {
    let len = entries.iter().map(|(h, _)| h.len()).max().unwrap_or(0);
    if len == 0 || rate.is_nan() || rate <= 0.0 {
        return Curve {
            mag_db: vec![FLOOR_DB; freqs.len()],
            group_delay_ms: vec![None; freqs.len()],
        };
    }
    let n = len
        .max((rate / BIN_HZ).ceil() as usize)
        .next_power_of_two()
        .clamp(16, GRID_MAX);
    let mut planner = RealFftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let bins = n / 2 + 1;
    let zero = Complex64::new(0.0, 0.0);
    // H = Σ g·DFT(h), D = Σ g·DFT(k·h[k]) (the true index k: exact at the
    // bins when a long filter wraps).
    let mut h_sum = vec![zero; bins];
    let mut d_sum = vec![zero; bins];
    let mut input = vec![0.0; n];
    let mut spectrum = vec![zero; bins];
    for &(taps, g) in entries {
        for (pass, acc) in [(0, &mut h_sum), (1, &mut d_sum)] {
            input.fill(0.0);
            for (k, &v) in taps.iter().enumerate() {
                let w = if pass == 0 { 1.0 } else { k as f64 };
                input[k % n] += g * v * w;
            }
            // Lengths are the plan's own; the call cannot fail.
            let _ = fft.process(&mut input, &mut spectrum);
            for (a, s) in acc.iter_mut().zip(&spectrum) {
                *a += s;
            }
        }
    }
    let mag: Vec<f64> = h_sum.iter().map(|h| h.norm()).collect();
    let floor = 10f64.powf(GD_FLOOR_DB / 20.0);
    let gd: Vec<Option<f64>> = h_sum
        .iter()
        .zip(&d_sum)
        .zip(&mag)
        .map(|((h, d), &m)| (m > floor).then(|| (d * h.conj()).re / (m * m)))
        .collect();
    let per_ms = 1000.0 / rate;
    let mut mag_db = Vec::with_capacity(freqs.len());
    let mut group_delay_ms = Vec::with_capacity(freqs.len());
    for &f in freqs {
        let p = (f * n as f64 / rate).clamp(0.0, (bins - 1) as f64);
        let k0 = (p.floor() as usize).min(bins - 1);
        let k1 = (k0 + 1).min(bins - 1);
        let t = p - k0 as f64;
        let m = mag[k0] + (mag[k1] - mag[k0]) * t;
        mag_db.push(if m > 0.0 {
            (20.0 * m.log10()).max(FLOOR_DB)
        } else {
            FLOOR_DB
        });
        let d = match (gd[k0], gd[k1]) {
            (Some(a), Some(b)) => Some(a + (b - a) * t),
            (Some(a), None) if t < 0.5 => Some(a),
            (None, Some(b)) if t >= 0.5 => Some(b),
            _ => None,
        };
        group_delay_ms.push(d.map(|s| s * per_ms));
    }
    Curve {
        mag_db,
        group_delay_ms,
    }
}

/// The combined convolution's curve per output (`channels` outputs; a mono
/// IR serves each): a per-channel IR's own response, a matrix output the
/// coherent sum of its row's entries (what a signal common to every input
/// gets). An output with no entry is silent.
pub fn output_curves(irs: &ConvIrs, rate: f64, channels: usize, freqs: &[f64]) -> Vec<Curve> {
    match irs {
        ConvIrs::Channels(list) => (0..channels)
            .map(|c| match list.get(c.min(list.len().saturating_sub(1))) {
                Some(ir) => response(&[(ir, 1.0)], rate, freqs),
                None => response(&[], rate, freqs),
            })
            .collect(),
        ConvIrs::Matrix(m) => (0..m.channels())
            .map(|out| {
                let row: Vec<(&[f64], f64)> = (0..m.channels())
                    .filter_map(|i| m.get(out, i).map(|t| (t, 1.0)))
                    .collect();
                response(&row, rate, freqs)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::FilterMatrix;

    const RATE: f64 = 48_000.0;

    fn grid() -> Vec<f64> {
        log_grid(20.0, 20_000.0, 240, RATE)
    }

    #[test]
    fn the_grid_is_the_apps_and_stops_at_nyquist() {
        let g = grid();
        assert_eq!(g.len(), 241);
        assert!((g[0] - 20.0).abs() < 1e-9 && (g[240] - 20_000.0).abs() < 1e-6);
        // About 1/24 octave: three decades over 240 steps.
        let step = (g[1] / g[0]).log2();
        assert!((step - 1000f64.log2() / 240.0).abs() < 1e-12);
        let low = log_grid(20.0, 20_000.0, 240, 32_000.0);
        assert!(low.last().copied().unwrap() <= 16_000.0 && low.len() < 241);
    }

    #[test]
    fn a_delta_is_flat_with_no_delay() {
        let c = response(&[(&[1.0], 1.0)], RATE, &grid());
        assert!(c.mag_db.iter().all(|m| m.abs() < 1e-9), "{:?}", c.mag_db);
        assert!(c.group_delay_ms.iter().all(|d| d.unwrap().abs() < 1e-9));
    }

    #[test]
    fn a_delayed_scaled_delta_is_flat_at_its_gain_and_delay() {
        let mut h = vec![0.0; 480];
        h[479] = 0.5;
        let c = response(&[(&h, 1.0)], RATE, &grid());
        for (m, d) in c.mag_db.iter().zip(&c.group_delay_ms) {
            assert!((m + 6.0206).abs() < 1e-3, "{m}");
            assert!((d.unwrap() - 479.0 / 48.0).abs() < 1e-6, "{d:?}");
        }
    }

    #[test]
    fn a_two_tap_average_is_a_cosine_low_pass() {
        // h = [0.5, 0.5]: |H| = |cos(πf/fs)|, half a sample of delay.
        let freqs = grid();
        let c = response(&[(&[0.5, 0.5], 1.0)], RATE, &freqs);
        for (i, &f) in freqs.iter().enumerate() {
            let want = 20.0 * (std::f64::consts::PI * f / RATE).cos().log10();
            assert!(
                (c.mag_db[i] - want).abs() < 1e-3,
                "{f} Hz: {} vs {want}",
                c.mag_db[i]
            );
            let d = c.group_delay_ms[i].unwrap();
            assert!((d - 0.5 / 48.0).abs() < 1e-9, "{d}");
        }
        // About −3 dB at 12 kHz (cos 45°).
        let k = freqs.iter().position(|&f| f > 12_000.0).unwrap();
        assert!(c.mag_db[k] < -3.0 && c.mag_db[k] > -3.5);
    }

    #[test]
    fn silence_reads_the_floor() {
        let c = response(&[(&[0.0, 0.0], 1.0)], RATE, &grid());
        assert!(c.mag_db.iter().all(|&m| m == FLOOR_DB));
        assert!(c.group_delay_ms.iter().all(Option::is_none));
    }

    #[test]
    fn a_matrix_output_sums_its_row() {
        // Output 0 = in 0 + in 1 (both unit): +6 dB; output 1 = in 1 at −6 dB.
        let mut m = FilterMatrix::new(2);
        m.set(0, 0, vec![1.0]);
        m.set(0, 1, vec![1.0]);
        m.set(1, 1, vec![0.5]);
        let out = output_curves(&ConvIrs::Matrix(m), RATE, 2, &grid());
        assert_eq!(out.len(), 2);
        assert!(out[0].mag_db.iter().all(|v| (v - 6.0206).abs() < 1e-3));
        assert!(out[1].mag_db.iter().all(|v| (v + 6.0206).abs() < 1e-3));
        // A mono per-channel IR serves both outputs.
        let mono = output_curves(&ConvIrs::Channels(vec![vec![2.0]]), RATE, 2, &grid());
        assert_eq!(mono.len(), 2);
        assert!(mono[1].mag_db.iter().all(|v| (v - 6.0206).abs() < 1e-3));
    }

    #[test]
    fn opposite_entries_cancel_in_the_sum() {
        let c = response(&[(&[1.0], 1.0), (&[1.0], -1.0)], RATE, &grid());
        assert!(c.mag_db.iter().all(|&m| m == FLOOR_DB));
    }
}
