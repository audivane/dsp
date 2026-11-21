//! Which ratios a design runs at, and the window it uses there.
//!
//! - Integer upsampling (`M = 1`): every design, from x2 to
//!   [`budget::MAX_RATIO`]. A kernel must stay within
//!   [`budget::MAX_KERNEL_TAPS`] at every source rate; the cascades need an
//!   even ratio made of 2 and 3, the block FFT a power of two.
//! - Other ratios: by [`Accept`] class. The runtime kinds are integer-only.
//!
//! Decimation keeps the band edge below the output Nyquist: the cutoff is
//! scaled by `out / in` and the length by [`length::decimated`]. A
//! [`super::Fallback`] window replaces the design's own where its condition
//! holds (for example a Kaiser window when neither rate is a multiple of
//! the other: keeping every M-th grid sample folds the whole stopband into
//! the output band, and Kaiser sidelobes, which fall off with frequency,
//! fold in less than a flat equiripple floor).

use super::{
    Accept, ArithmeticPath, DesignError, FallbackWhen, FilterDesign, Grid, KernelDesign, Rates,
    Window, budget, length,
};
use audivane_protocol::RateRatio;

/// Whether `d` runs at `ratio` (rules in the module docs). Cheap: designs
/// nothing.
pub fn check(d: &FilterDesign, ratio: RateRatio) -> Result<(), DesignError> {
    if ratio.is_integer() {
        return check_integer(d, ratio.up as usize);
    }
    let fail = |need| Err(DesignError::Ratio { ratio, need });
    let accept = match d {
        FilterDesign::Kernel(k) => k.rates.accept,
        _ => Accept::Integer,
    };
    match accept {
        Accept::Integer => fail("an integer upsampling ratio"),
        Accept::Upsampling if ratio.is_decimating() => {
            fail("an output rate at or above the source rate")
        }
        _ => Ok(()),
    }
}

/// [`check`] at the integer `ratio`.
fn check_integer(d: &FilterDesign, ratio: usize) -> Result<(), DesignError> {
    let r = RateRatio::integer(ratio as u32);
    let fail = |need| Err(DesignError::Ratio { ratio: r, need });
    if !(2..=budget::MAX_RATIO).contains(&ratio) {
        return fail("a ratio from 2 to 64");
    }
    match d {
        // The Custom builder's lengths are validated by the daemon, not
        // here.
        FilterDesign::Kernel(k) if k.numerics.path == ArithmeticPath::Standard => Ok(()),
        FilterDesign::Kernel(k) => {
            let taps = longest_kernel(k, ratio);
            if taps > budget::MAX_KERNEL_TAPS {
                return Err(DesignError::TooLong { ratio: r, taps });
            }
            Ok(())
        }
        FilterDesign::Interpolator(_) => Ok(()),
        FilterDesign::Cascade(_) => {
            let mut rest = ratio;
            while rest.is_multiple_of(3) {
                rest /= 3;
            }
            if ratio.is_multiple_of(2) && rest.is_power_of_two() {
                Ok(())
            } else {
                fail("an even ratio made of factors 2 and 3")
            }
        }
        FilterDesign::BlockFft(_) => {
            if ratio.is_power_of_two() {
                Ok(())
            } else {
                fail("a power-of-two ratio")
            }
        }
    }
}

/// The longest kernel of `k` at the integer `ratio` over every source rate
/// (the length can depend on it), in output-rate taps.
fn longest_kernel(k: &KernelDesign, ratio: usize) -> usize {
    audivane_protocol::ALLOWED_RATES
        .iter()
        .filter_map(|&rate| length::per_input(k, ratio, rate).ok())
        .map(|t| t * ratio)
        .max()
        .unwrap_or(0)
}

/// The window a design with `window` and `rates` uses on `grid`.
pub fn effective_window(window: Window, rates: &Rates, grid: &Grid) -> Window {
    match rates.fallback_window {
        Some(f) if applies(f.when, grid) => f.window,
        _ => window,
    }
}

fn applies(when: FallbackWhen, grid: &Grid) -> bool {
    match when {
        FallbackWhen::NotMultiple => !grid.multiple,
        FallbackWhen::Decimating => grid.decimating,
    }
}
