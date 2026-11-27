//! EQ response maths for the UI, straight from `audivane-dsp-core` so the
//! curve is the one the engine runs. The webview draws with an identical TS
//! port (app/src/lib/eqmath.ts); `examples/eq_fixture.rs` emits reference
//! values from this module and a vitest test compares the two.

use audivane_dsp_core::biquad::{Coeffs, Eq};
use audivane_protocol::Band;

/// Magnitude in dB of the whole band list (preamp included) at each frequency.
pub fn response_db(bands: &[Band], preamp_db: f64, sample_rate: f64, freqs: &[f64]) -> Vec<f64> {
    let eq = Eq::from_bands(bands, preamp_db, sample_rate, 1);
    freqs
        .iter()
        .map(|&f| 20.0 * eq.response(f).norm().log10())
        .collect()
}

/// Magnitude in dB of one band.
pub fn band_db(band: &Band, sample_rate: f64, freqs: &[f64]) -> Vec<f64> {
    let c = Coeffs::design(band, sample_rate);
    freqs
        .iter()
        .map(|&f| 20.0 * c.response(f, sample_rate).norm().log10())
        .collect()
}

/// dsp-core's automatic preamp for a band list.
pub fn auto_preamp_db(bands: &[Band], sample_rate: f64) -> f64 {
    Eq::bands_auto_preamp_db(bands, sample_rate)
}

/// Log-spaced frequencies, inclusive.
pub fn log_space(from: f64, to: f64, n: usize) -> Vec<f64> {
    let (a, b) = (from.ln(), to.ln());
    (0..n)
        .map(|i| (a + (b - a) * i as f64 / (n - 1) as f64).exp())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::FilterType;

    fn band(kind: FilterType, f: f64, g: f64, q: f64) -> Band {
        Band {
            kind,
            freq_hz: f,
            gain_db: g,
            q,
            enabled: true,
        }
    }

    #[test]
    fn auto_preamp_cancels_the_largest_boost() {
        let b = band(FilterType::Peaking, 1000.0, 6.0, 1.0);
        let pre = auto_preamp_db(&[b], 48_000.0);
        assert!((pre + 6.0).abs() < 1e-6, "{pre}");
    }
}
