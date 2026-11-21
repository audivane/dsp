//! Minimum and intermediate phase by checked zero reflection, the phase
//! method of the `standard` path. The transforms are
//! [`crate::minphase::minimum_phase`] and
//! [`crate::minphase::intermediate_phase`]; this module gives them the
//! design's [`Contract`]: where the absolute stopband check starts.

use crate::design::paths::standard::UpsamplerDesign;
use crate::minphase::Contract;
use crate::resample::{gaussian_half_width, kaiser_attenuation_db};
use audivane_protocol::WindowKind;

/// Where [`crate::minphase`] starts checking the absolute stopband error of a
/// design: Kaiser's stopband edge estimate with 20% slack on the half
/// transition width, in cycles per output sample.
pub fn minimum_phase_contract(d: &UpsamplerDesign) -> Contract {
    let a = kaiser_attenuation_db(d.kaiser_beta);
    let half = match d.window {
        // Tukey is linear-phase only (daemon validation); Kaiser's estimate.
        WindowKind::Kaiser | WindowKind::Tukey => {
            (a - 7.95).max(0.0) / (28.72 * (d.taps.max(2) - 1) as f64)
        }
        // Fraction of the source Nyquist -> cycles per output sample.
        WindowKind::Gaussian => {
            gaussian_half_width(d.taps, d.ratio.max(1), a) / (2.0 * d.ratio.max(1) as f64)
        }
    };
    Contract {
        kaiser_beta: d.kaiser_beta,
        stopband_edge: d.cutoff / (2.0 * d.ratio.max(1) as f64) + 1.2 * half,
    }
}
