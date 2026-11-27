//! The cepstral phase transform (`method: cepstral`), behind the
//! intermediate- and minimum-phase presets.
//!
//! A cepstral design is its linear-phase prototype (same recipe, built on
//! its arithmetic path, [`crate::design::paths`]) passed through a
//! real-cepstrum minimum-phase construction, optionally blended back
//! toward linear phase:
//!
//! 1. Zero-pad the N-tap kernel to `M = 2^(bitlen(N) + 5)` (32 to 64 N).
//! 2. `log|H|` on the half spectrum, with `|H|` clamped to at least 2^-63
//!    (about -379 dB): at some ratios a few stopband bins fall below it,
//!    and the clamp keeps them deep instead of turning them into spikes.
//! 3. Real cepstrum `c = IFFT(log|H|)`, folded to the causal side: keep
//!    `c[0]` and `c[M/2]`, double `c[1 .. M/2]`, zero the rest.
//!    `Y = FFT(c)`, so `Re Y = log|H|` and `Im Y` is the minimum phase.
//! 4. Blend `phi = (1 - p) Im Y - p k pi / 2` at bin `k` (the second term is
//!    a pure delay of `p M / 4` samples): `p = 0` for minimum phase, 0.75
//!    for intermediate phase.
//! 5. `g = IFFT(exp(Re Y + i phi))`, real, length M.
//! 6. Minimum phase keeps the first N samples of g (plain truncation, no
//!    fade). Intermediate phase keeps a four times longer kernel, `4N`
//!    samples read circularly from `round(p (M/4 - 2N))` (ties away from
//!    zero), which puts the main peak near `0.375 * 4N`.
//! 7. Normalise so that branch `L/2` sums to 1, before or after the
//!    transform (per arithmetic path, see below).
//!
//! Precision: the minimum phase is a Hilbert transform of `log|H|`, so it
//! depends on bins far down the stopband (200 to 280 dB below the peak for
//! these windows), and the taps react to the last bits of the prototype.
//! So step 2's magnitudes come from a double-double FFT (`ddfft`): the
//! logarithm makes the rest well conditioned, so steps 3 to 5 run in f64,
//! and the prototypes are defined to the last bit where their design says
//! so: the Chebyshev and Kaiser designs (`crate::design::paths::exact`:
//! an f64 sinc with a fixed operation order, the window in x87 extended
//! precision, a sequential branch sum; normalised before the transform for
//! the long Chebyshev, after it for the others), the Gaussian in
//! double-double and rounded once (after), the short leaky design with the
//! Kaiser sinc form and its Nuttall window (after), and the flat-top and
//! Lanczos designs with their linear twin's f64 prototype
//! ([`crate::design::paths::fir`]: before for flat-top, after for Lanczos).

mod ddfft;

use crate::design::{DesignError, ddouble};
use realfft::RealFftPlanner;
use rustfft::num_complex::Complex64;

/// Blend weight of the intermediate phase (75 % linear phase): the one
/// intermediate `mix` this transform builds exactly.
pub const INTERMEDIATE_BLEND: f64 = 0.75;

/// What the transform makes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// Blend 0: the first N samples.
    Minimum,
    /// Blend [`INTERMEDIATE_BLEND`]: 4N samples.
    Intermediate,
    /// Any other blend `p` strictly between 0 and 1: 4N samples read from
    /// `round(p (M/4 - 2N))`, the delay term `-p k pi / 2` reduced exactly
    /// (`p k / 2 mod 2` in double-double) before its sine and cosine.
    Mix(f64),
}

/// Smallest magnitude the logarithm sees (step 2).
const LOG_FLOOR: f64 = 1.0 / 9_223_372_036_854_775_808.0; // 2^-63

/// Index of the largest |tap| (the first one on a tie).
pub(crate) fn peak_index(h: &[f64]) -> usize {
    let mut best = 0;
    for (i, v) in h.iter().enumerate() {
        if v.abs() > h[best].abs() {
            best = i;
        }
    }
    best
}

/// The transform's FFT length for an N-tap kernel: `2^(bitlen(N) + 5)`.
pub fn fft_len(n: usize) -> usize {
    1usize << (usize::BITS - n.leading_zeros() + 5)
}

/// Steps 1 to 6 of the module docs on a finished kernel `h`: the first N
/// samples for [`Target::Minimum`], 4N for [`Target::Intermediate`]. Not
/// normalised.
pub fn transform(h: &[f64], target: Target) -> Result<Vec<f64>, DesignError> {
    let blend = match target {
        Target::Minimum => 0.0,
        Target::Intermediate => INTERMEDIATE_BLEND,
        Target::Mix(p) => {
            if !(p > 0.0 && p < 1.0) {
                return Err(DesignError::Failed(format!(
                    "the cepstral blend must be strictly between 0 and 1 (got {p})"
                )));
            }
            p
        }
    };
    let n = h.len();
    if n == 0 || h.iter().all(|&v| v == 0.0) {
        return Err(DesignError::Failed("empty or all-zero kernel".into()));
    }
    let m = fft_len(n);
    let half = m / 2;
    let fail = |e: realfft::FftError| DesignError::Failed(format!("phase transform: {e}"));

    // Steps 1-2: log|H| (double-double magnitudes).
    let mut spec: Vec<Complex64> = ddfft::magnitudes(h, m)
        .into_iter()
        .map(|a| Complex64::new(a.max(LOG_FLOOR).ln(), 0.0))
        .collect();

    let mut planner = RealFftPlanner::<f64>::new();
    let forward = planner.plan_fft_forward(m);
    let inverse = planner.plan_fft_inverse(m);

    // Step 3: cepstrum, folded to the causal side (with the 1/M of the
    // inverse FFT), then Y = FFT(c).
    let mut c = vec![0.0; m];
    inverse.process(&mut spec, &mut c).map_err(fail)?;
    let scale = 1.0 / m as f64;
    c[0] *= scale;
    c[half] *= scale;
    for v in &mut c[1..half] {
        *v *= 2.0 * scale;
    }
    c[half + 1..].fill(0.0);
    forward.process(&mut c, &mut spec).map_err(fail)?;

    // Steps 4-5: exp(Re Y + i phi). The linear-phase part of phi,
    // -p k pi / 2 = -3 k pi / 8 for p = 0.75, is a multiple of pi / 8:
    // take it from an exact table instead of a large angle.
    let eighths: [Complex64; 16] = std::array::from_fn(|j| {
        let (s, c) = ddouble::Dd::new(j as f64 / 8.0).sin_cos_pi();
        Complex64::new(c.to_f64(), -s.to_f64())
    });
    let general = matches!(target, Target::Mix(_));
    for (k, y) in spec.iter_mut().enumerate() {
        let phi = (1.0 - blend) * y.im;
        let mut v = Complex64::from_polar(y.re.exp(), phi);
        if general {
            // -p k pi / 2 = -pi (p k / 2): the product is exact as a
            // double-double, and `sin_cos_pi` reduces it exactly.
            let (hi, lo) = ddouble::two_prod(blend, k as f64);
            let (s, c) = ddouble::Dd { hi, lo }.scale(0.5).sin_cos_pi();
            v *= Complex64::new(c.to_f64(), -s.to_f64());
        } else if blend != 0.0 {
            debug_assert_eq!(blend, INTERMEDIATE_BLEND);
            v *= eighths[(3 * k) % 16];
        }
        *y = v;
    }
    // DC and Nyquist are real (their phase is 0 or a multiple of 2 pi).
    spec[0].im = 0.0;
    spec[half].im = 0.0;
    inverse.process(&mut spec, &mut c).map_err(fail)?;
    drop(spec);

    // Step 6.
    let g = c;
    let out = if blend == 0.0 {
        g[..n].iter().map(|v| v * scale).collect()
    } else {
        let len = 4 * n;
        let x = blend * (m as f64 / 4.0 - len as f64 / 2.0);
        let start = x.round() as i64; // ties away from zero
        (0..len as i64)
            .map(|i| g[(start + i).rem_euclid(m as i64) as usize] * scale)
            .collect()
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small Kaiser-windowed sinc at x4 (31 taps per input sample).
    fn test_kernel() -> Vec<f64> {
        let l = 4usize;
        let n = 31 * l;
        let w = crate::design::paths::fir::windows::kaiser_periodic(n, 8.0 * std::f64::consts::PI);
        let mut h: Vec<f64> = (0..n)
            .map(|i| {
                let x = 0.95 * (i as f64 - (n / 2) as f64) / l as f64;
                let s = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                s * w[i]
            })
            .collect();
        let s: f64 = h[l / 2..].iter().step_by(l).sum();
        h.iter_mut().for_each(|v| *v /= s);
        h
    }

    fn mag_db(h: &[f64], nfft: usize) -> Vec<f64> {
        let mut planner = RealFftPlanner::<f64>::new();
        let f = planner.plan_fft_forward(nfft);
        let mut x = vec![0.0; nfft];
        x[..h.len()].copy_from_slice(h);
        let mut out = f.make_output_vec();
        f.process(&mut x, &mut out).unwrap();
        out.iter().map(|c| 20.0 * c.norm().log10()).collect()
    }

    fn energy_fraction(h: &[f64], upto: usize) -> f64 {
        let total: f64 = h.iter().map(|v| v * v).sum();
        h[..upto].iter().map(|v| v * v).sum::<f64>() / total
    }

    #[test]
    fn all_zero_kernel_is_an_error() {
        assert!(transform(&[0.0; 4], Target::Minimum).is_err());
    }

    #[test]
    fn fft_length_rule() {
        assert_eq!(fft_len(2040), 65_536);
        assert_eq!(fft_len(2048), 131_072);
        assert_eq!(fft_len(376), 16_384);
        assert_eq!(fft_len(4616), 262_144);
    }

    #[test]
    fn magnitude_is_preserved() {
        let h = test_kernel();
        for phase in [Target::Minimum, Target::Intermediate] {
            let g = transform(&h, phase).unwrap();
            let nfft = 1 << 13;
            let a = mag_db(&h, nfft);
            let b = mag_db(&g, nfft);
            let peak = a.iter().copied().fold(f64::MIN, f64::max);
            for (k, (x, y)) in a.iter().zip(&b).enumerate() {
                if *x > peak - 60.0 {
                    assert!((x - y).abs() < 1e-4, "{phase:?} bin {k}: {x} vs {y}");
                }
            }
        }
    }

    #[test]
    fn minimum_phase_front_loads_the_energy() {
        let h = test_kernel();
        let g = transform(&h, Target::Minimum).unwrap();
        assert_eq!(g.len(), h.len());
        // A minimum-phase sequence has the largest partial energy of all
        // sequences with its magnitude.
        for j in 1..h.len() {
            assert!(
                energy_fraction(&g, j) >= energy_fraction(&h, j) - 1e-12,
                "j {j}"
            );
        }
        let quarter = h.len() / 4;
        assert!(energy_fraction(&g, quarter) > 0.9);
        assert!(energy_fraction(&h, quarter) < 1e-3);
        assert!(peak_index(&g) < quarter);
        // DC gain is kept
        let (sh, sg): (f64, f64) = (h.iter().sum(), g.iter().sum());
        assert!((sh - sg).abs() < 1e-9 * sh);
    }

    #[test]
    fn intermediate_phase_is_four_times_longer_with_its_peak_near_three_eighths() {
        let h = test_kernel();
        let g = transform(&h, Target::Intermediate).unwrap();
        assert_eq!(g.len(), 4 * h.len());
        let at = peak_index(&g) as f64 / g.len() as f64;
        assert!((at - 0.375).abs() < 0.02, "peak at {at}");
        // nothing significant wraps around
        let peak = g[peak_index(&g)].abs();
        assert!(g[..g.len() / 8].iter().all(|v| v.abs() < 1e-6 * peak));
    }

    /// The general blend's exact reduction gives the table's bits at 0.75,
    /// and a blend between moves the peak smoothly.
    #[test]
    fn general_blend() {
        let h = test_kernel();
        let table = transform(&h, Target::Intermediate).unwrap();
        let general = transform(&h, Target::Mix(INTERMEDIATE_BLEND)).unwrap();
        assert!(
            table
                .iter()
                .zip(&general)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        let mut last = 0.0;
        for p in [0.1, 0.3, 0.5, 0.749, 0.9] {
            let g = transform(&h, Target::Mix(p)).unwrap();
            assert_eq!(g.len(), 4 * h.len());
            let at = peak_index(&g) as f64 / g.len() as f64;
            assert!(at > last, "{p}: {at}");
            last = at;
            let nfft = 1 << 13;
            let (a, b) = (mag_db(&h, nfft), mag_db(&g, nfft));
            let peak = a.iter().copied().fold(f64::MIN, f64::max);
            for (k, (x, y)) in a.iter().zip(&b).enumerate() {
                if *x > peak - 60.0 {
                    assert!((x - y).abs() < 1e-3, "{p} bin {k}: {x} vs {y}");
                }
            }
        }
        assert!(transform(&h, Target::Mix(1.0)).is_err());
    }

    #[test]
    fn deterministic() {
        let h = test_kernel();
        for phase in [Target::Minimum, Target::Intermediate] {
            let a = transform(&h, phase).unwrap();
            let b = transform(&h, phase).unwrap();
            assert!(a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()));
        }
    }

    #[test]
    fn scaling_commutes_with_the_transform() {
        // log|cH| = log c + log|H|: only c[0] moves, the output scales by c.
        let h = test_kernel();
        let h2: Vec<f64> = h.iter().map(|v| v * 0.5).collect();
        let a = transform(&h, Target::Minimum).unwrap();
        let b = transform(&h2, Target::Minimum).unwrap();
        let peak = a[peak_index(&a)].abs();
        assert!(
            a.iter()
                .zip(&b)
                .all(|(x, y)| (x - 2.0 * y).abs() < 1e-13 * peak)
        );
    }
}
