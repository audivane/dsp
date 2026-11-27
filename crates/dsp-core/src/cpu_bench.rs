//! A short benchmark of the kernels the upsampler runtimes spend their time
//! in, for the daemon's CPU admission check (`control::cpu_budget`). It
//! runs once, off the real-time thread, in about 0.2 s.
//!
//! What it measures, each as the best of a few short runs:
//! - the rational runtime's stereo dot product (`crate::dot`, the same
//!   function the runtime picks), with the kernel in cache and streamed
//!   from memory (8 MiB, the largest rational kernel);
//! - the split-complex multiply-accumulate of the FFT polyphase engine
//!   ([`crate::fft::complex_mac_split`]), in the engine's own layout and
//!   loop order, with the filter spectra from 256 KiB (in cache) to 64 MiB
//!   (from memory): a long kernel's spectra do not fit the caches, and the
//!   engine is then bound by memory;
//! - the scalar `f64::mul_add` dot product of the table interpolator;
//! - a real FFT and its inverse ([`crate::fft::RealFftPair`]) at 2048
//!   points (the engine's block) and at 32 768 (block FFT interpolation);
//! - the output quantizer ([`crate::dither::Quantizer`]) of every dither
//!   kind, for the second quantizer of an A/B output compare.

use crate::dither::Quantizer;
use crate::fft::{RealFftPair, SplitSpectrum, complex_mac_split};
use audivane_protocol::{DitherKind, OutputConfig};
use std::time::{Duration, Instant};

/// Machine rates measured by [`measure`].
#[derive(Clone, Debug, PartialEq)]
pub struct KernelRates {
    /// Stereo dot products, multiply-adds per second per channel, at the
    /// kernel sizes in bytes: `(bytes, rate)`, ascending.
    pub dot: Vec<(usize, f64)>,
    /// Split-complex multiply-accumulates, bins per second (one bin is four
    /// real multiply-adds), at the filter spectra sizes in bytes.
    pub spectral: Vec<(usize, f64)>,
    /// Seconds per real transform (forward and inverse averaged) divided by
    /// `n log2 n`, at the transform sizes: `(n, seconds per unit)`.
    pub fft: Vec<(usize, f64)>,
    /// Scalar `f64::mul_add` multiply-adds per second (the table
    /// interpolator's dot product).
    pub mul_add: f64,
    /// Quantized samples per second of the output quantizer, per dither
    /// kind, in [`QUANTIZER_KINDS`] order.
    pub quantizer: Vec<(DitherKind, f64)>,
    /// How long the measurement took.
    pub elapsed: Duration,
}

/// Linear interpolation of `points` (ascending in x) in `log2 x`, held
/// at the ends.
fn interpolate(points: &[(usize, f64)], x: usize) -> f64 {
    let lx = (x.max(1) as f64).log2();
    let (first, last) = (points[0], points[points.len() - 1]);
    if x <= first.0 {
        return first.1;
    }
    if x >= last.0 {
        return last.1;
    }
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        if x <= b.0 {
            let (la, lb) = ((a.0 as f64).log2(), (b.0 as f64).log2());
            let t = (lx - la) / (lb - la);
            return a.1 + (b.1 - a.1) * t;
        }
    }
    last.1
}

impl KernelRates {
    /// Dot multiply-adds per second per channel for a kernel of `bytes`.
    pub fn dot_rate(&self, bytes: usize) -> f64 {
        interpolate(&self.dot, bytes)
    }

    /// Spectral bins per second for filter spectra of `bytes`.
    pub fn spectral_rate(&self, bytes: usize) -> f64 {
        interpolate(&self.spectral, bytes)
    }

    /// Seconds for one real transform (forward or inverse) of `n` points.
    pub fn fft_seconds(&self, n: usize) -> f64 {
        let n = n.max(2);
        interpolate(&self.fft, n) * n as f64 * (n as f64).log2()
    }

    /// Quantized samples per second of the output quantizer with `kind`
    /// (the slowest measured kind's for one not measured).
    pub fn quantizer_rate(&self, kind: DitherKind) -> f64 {
        self.quantizer
            .iter()
            .find(|q| q.0 == kind)
            .map(|q| q.1)
            .unwrap_or_else(|| {
                self.quantizer
                    .iter()
                    .map(|q| q.1)
                    .fold(f64::INFINITY, f64::min)
            })
    }

    /// The in-cache dot rate: the unit of the multiply-add figures the
    /// admission check reports.
    pub fn reference_rate(&self) -> f64 {
        self.dot[0].1
    }

    /// The conservative floor of every rate, at [`measure`]'s sizes, as an
    /// absolute figure (not relative to any machine): well under what an old
    /// AVX2 laptop core at about 2 GHz sustains, so a real result on a slow
    /// machine is never raised to it, only one from a busy or throttled
    /// start. A measurement under it is measured again and then raised to it
    /// ([`Self::below`], [`Self::at_least`]).
    ///
    /// - Dot, kernel in cache: 0.6 G multiply-adds per second per channel;
    ///   streamed from memory (8 MiB): 0.4 G.
    /// - Spectral bins (4 multiply-adds each): 0.2 G per second in cache,
    ///   0.1 G from memory.
    /// - FFT: 1 ns per `n log2 n`, about 5x slower than a current desktop
    ///   core.
    /// - Scalar `f64::mul_add` chain: 0.05 G per second (a dependent chain,
    ///   a libm call per term where the build has no FMA target feature).
    /// - Quantizer: 4 M samples per second for every kind (the slowest kind
    ///   takes about 45 ns per sample on a current desktop core).
    pub fn floor() -> Self {
        Self {
            dot: vec![(DOT_SIZES[0].0, 0.6e9), (DOT_SIZES[1].0, 0.4e9)],
            spectral: SPECTRAL_PARTS
                .iter()
                .zip([0.2e9, 0.2e9, 0.1e9, 0.1e9])
                .map(|(&parts, rate)| (spectral_bytes(parts), rate))
                .collect(),
            fft: vec![(FFT_SIZES[0], 1.0e-9), (FFT_SIZES[1], 1.0e-9)],
            mul_add: 0.05e9,
            quantizer: QUANTIZER_KINDS.iter().map(|&k| (k, 4.0e6)).collect(),
            elapsed: Duration::ZERO,
        }
    }

    /// Whether any rate is under `floor`'s (an FFT slower than it).
    pub fn below(&self, floor: &Self) -> bool {
        let under =
            |a: &[(usize, f64)], b: &[(usize, f64)]| a.iter().zip(b).any(|(x, y)| x.1 < y.1);
        under(&self.dot, &floor.dot)
            || under(&self.spectral, &floor.spectral)
            || self.fft.iter().zip(&floor.fft).any(|(x, y)| x.1 > y.1)
            || self.mul_add < floor.mul_add
            || under(&quantizer_points(self), &quantizer_points(floor))
    }

    /// The faster of `self` and `other` for every rate (both measured at
    /// [`measure`]'s sizes); the elapsed times add up.
    pub fn best_of(&self, other: &Self) -> Self {
        let best = |a: &[(usize, f64)], b: &[(usize, f64)], faster: fn(f64, f64) -> f64| {
            a.iter()
                .zip(b)
                .map(|(x, y)| {
                    debug_assert_eq!(x.0, y.0, "rates measured at other sizes");
                    (x.0, faster(x.1, y.1))
                })
                .collect()
        };
        Self {
            dot: best(&self.dot, &other.dot, f64::max),
            spectral: best(&self.spectral, &other.spectral, f64::max),
            fft: best(&self.fft, &other.fft, f64::min),
            mul_add: self.mul_add.max(other.mul_add),
            quantizer: self
                .quantizer
                .iter()
                .zip(&other.quantizer)
                .map(|(x, y)| {
                    debug_assert_eq!(x.0, y.0, "quantizers measured in another order");
                    (x.0, x.1.max(y.1))
                })
                .collect(),
            elapsed: self.elapsed + other.elapsed,
        }
    }

    /// Every rate raised to `floor`'s where it is under it.
    pub fn at_least(&self, floor: &Self) -> Self {
        Self {
            elapsed: self.elapsed,
            ..self.best_of(floor)
        }
    }
}

/// The quantizer rates as `(index, rate)` points, for [`KernelRates::below`].
fn quantizer_points(r: &KernelRates) -> Vec<(usize, f64)> {
    r.quantizer.iter().map(|q| q.1).enumerate().collect()
}

/// The dither kinds the quantizer measurement covers: all of them.
pub const QUANTIZER_KINDS: [DitherKind; 12] = [
    DitherKind::None,
    DitherKind::Tpdf,
    DitherKind::Shaped,
    DitherKind::HeadroomTpdf,
    DitherKind::Rectangular,
    DitherKind::Gaussian,
    DitherKind::HighpassTpdf,
    DitherKind::Feedback1,
    DitherKind::Feedback5,
    DitherKind::Resonant5,
    DitherKind::Resonant9,
    DitherKind::Integrator15,
];

/// Kernel sizes of the dot measurement: `(bytes, taps)`.
const DOT_SIZES: [(usize, usize); 2] = [(256 << 10, 32 << 10), (8 << 20, 1 << 20)];
/// Partitions of the spectral measurement (16 branch spectra of 1025
/// bins each).
const SPECTRAL_PARTS: [usize; 4] = [1, 16, 64, 256];
const SPECTRAL_BINS: usize = 1025;
const SPECTRAL_BRANCHES: usize = 16;
/// Transform sizes of the FFT measurement.
const FFT_SIZES: [usize; 2] = [2048, 32768];

/// Bytes of filter spectra for `parts` partitions (16 bytes per
/// split-complex bin).
fn spectral_bytes(parts: usize) -> usize {
    parts * SPECTRAL_BRANCHES * SPECTRAL_BINS * 16
}

/// Run `f` (which does `work` units per call) for at least `span`, `rounds`
/// times; the best rate in units per second.
fn best_rate(rounds: usize, span: Duration, work: f64, mut f: impl FnMut()) -> f64 {
    f(); // warm the caches and the code
    let mut best = 0.0f64;
    for _ in 0..rounds {
        let t0 = Instant::now();
        let mut calls = 0u64;
        while t0.elapsed() < span {
            f();
            calls += 1;
        }
        let secs = t0.elapsed().as_secs_f64();
        best = best.max(calls as f64 * work / secs);
    }
    best
}

/// A deterministic fill that is not denormal and not zero.
fn fill(v: &mut [f64], seed: u64) {
    let mut s = seed | 1;
    for x in v {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        *x = (s >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
    }
}

/// Dot rate over a kernel of `taps` coefficients cut in rows of `row`
/// taps, like the rational runtime walks its branches.
fn dot_point(taps: usize, row: usize, span: Duration) -> f64 {
    let k = crate::dot::Kernels::detect();
    let mut h = vec![0.0; taps];
    fill(&mut h, 1);
    let mut a = vec![0.0; row + 64];
    let mut b = vec![0.0; row + 64];
    fill(&mut a, 2);
    fill(&mut b, 3);
    let rows = taps / row;
    let mut sink = 0.0;
    let mut next = 0usize;
    let per_call = 64;
    let rate = best_rate(3, span, (per_call * row) as f64, || {
        for i in 0..per_call {
            let r = &h[next * row..(next + 1) * row];
            let lead = i % 64;
            let (x, y) = (k.dot2)(r, &a[lead..lead + row], &b[lead..lead + row]);
            sink += x + y;
            next = (next + 7) % rows;
        }
    });
    std::hint::black_box(sink);
    rate
}

/// Spectral rate of the FFT polyphase engine's inner loops for `parts`
/// partitions of `branches` branch spectra of `bins` bins: the same
/// layout (one [`SplitSpectrum`] per partition and branch, one delay-line
/// spectrum per partition, one accumulator per branch) and the same loop
/// order as [`crate::resample::Upsampler`], so a long kernel's spectra are
/// swept from memory the way the runtime sweeps them.
fn spectral_point(parts: usize, branches: usize, bins: usize, span: Duration) -> f64 {
    let spectrum = |seed: u64| {
        let mut s = SplitSpectrum::new(bins);
        fill(&mut s.re, seed);
        fill(&mut s.im, seed + 1);
        s
    };
    let ir: Vec<Vec<SplitSpectrum>> = (0..parts)
        .map(|p| {
            (0..branches)
                .map(|b| spectrum(100 + (p * branches + b) as u64 * 2))
                .collect()
        })
        .collect();
    let fdl: Vec<SplitSpectrum> = (0..parts).map(|p| spectrum(7 + p as u64 * 2)).collect();
    let mut acc: Vec<SplitSpectrum> = (0..branches).map(|_| SplitSpectrum::new(bins)).collect();
    let mut head = 0usize;
    let rate = best_rate(2, span, (parts * branches * bins) as f64, || {
        head = (head + 1) % parts;
        for p in 0..parts {
            let x = &fdl[(head + parts - p) % parts];
            for (a, h) in acc.iter_mut().zip(&ir[p]) {
                complex_mac_split(a, x, h);
            }
        }
    });
    std::hint::black_box(&acc);
    rate
}

/// Rate of the scalar `f64::mul_add` dot product the table interpolator
/// runs (127 taps in groups of 16), multiply-adds per second.
fn mul_add_point(span: Duration) -> f64 {
    const TAPS: usize = 127;
    let mut x = [0.0; TAPS];
    let mut c = [0.0; TAPS];
    fill(&mut x, 8);
    fill(&mut c, 9);
    let mut sink = 0.0;
    let rate = best_rate(3, span, (16 * TAPS) as f64, || {
        for _ in 0..16 {
            let mut b = x[0] * c[0];
            for k in 1..TAPS {
                b = x[k].mul_add(c[k], b);
            }
            sink += b;
            x[0] = sink * 1e-300;
        }
    });
    std::hint::black_box(sink);
    rate
}

/// Quantized samples per second of a stereo 16-bit quantizer with `kind`
/// at 192 kHz (the shaper table every rate from 176.4 kHz up uses), on
/// 1024-frame blocks of a quiet tone.
fn quantizer_point(kind: DitherKind, span: Duration) -> f64 {
    const FRAMES: usize = 1024;
    let output = OutputConfig {
        bits: 16,
        dither: kind,
        headroom_db: 0.0,
    };
    let mut q = Quantizer::with_sample_rate(&output, 2, 1, 192_000);
    let mut l = vec![0.0; FRAMES];
    let mut r = vec![0.0; FRAMES];
    fill(&mut l, 10);
    fill(&mut r, 11);
    let mut out = vec![0i32; FRAMES * 2];
    let rate = best_rate(2, span, (FRAMES * 2) as f64, || {
        q.process(&[&l, &r], &mut out);
    });
    std::hint::black_box(&out);
    rate
}

/// Seconds per `n log2 n` of one real transform at `n` points.
fn fft_point(n: usize, span: Duration) -> f64 {
    let mut fft = RealFftPair::new(n);
    let half = n / 2;
    let mut prev = vec![0.0; half];
    let mut curr = vec![0.0; half];
    fill(&mut prev, 6);
    fill(&mut curr, 7);
    let mut spec = SplitSpectrum::new(fft.spec_len());
    let mut out = vec![0.0; n];
    let rate = best_rate(3, span, 2.0, || {
        fft.forward_pair_into_split(&prev, &curr, &mut spec);
        fft.inverse_from_split(&spec, &mut out);
        curr[0] = out[1] * 1e-3;
    });
    std::hint::black_box(&out);
    1.0 / (rate * n as f64 * (n as f64).log2())
}

/// Measure the machine (about 0.2 s; allocates about 70 MB for the
/// duration). Call off the real-time thread, once; callers cache it.
pub fn measure() -> KernelRates {
    let t0 = Instant::now();
    let span = Duration::from_millis(4);
    let dot = DOT_SIZES
        .iter()
        .map(|&(bytes, taps)| (bytes, dot_point(taps, 512, span)))
        .collect();
    // The engine's block (1024 frames) at x16: one partition is 16
    // branch spectra of 1025 bins (262 KiB).
    let spectral = SPECTRAL_PARTS
        .iter()
        .map(|&parts| {
            (
                spectral_bytes(parts),
                spectral_point(parts, SPECTRAL_BRANCHES, SPECTRAL_BINS, span),
            )
        })
        .collect();
    let fft = FFT_SIZES.iter().map(|&n| (n, fft_point(n, span))).collect();
    let quantizer = QUANTIZER_KINDS
        .iter()
        .map(|&k| (k, quantizer_point(k, Duration::from_millis(1))))
        .collect();
    KernelRates {
        dot,
        spectral,
        fft,
        mul_add: mul_add_point(span),
        quantizer,
        elapsed: t0.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_floor_is_at_the_measured_sizes_and_clamps_slow_rates() {
        let floor = KernelRates::floor();
        let slow = KernelRates {
            dot: floor.dot.iter().map(|&(b, r)| (b, r / 10.0)).collect(),
            spectral: floor.spectral.iter().map(|&(b, r)| (b, r * 2.0)).collect(),
            fft: floor.fft.iter().map(|&(n, t)| (n, t * 3.0)).collect(),
            mul_add: floor.mul_add,
            quantizer: floor.quantizer.iter().map(|&(k, r)| (k, r / 2.0)).collect(),
            elapsed: Duration::from_millis(200),
        };
        assert!(slow.below(&floor));
        let clamped = slow.at_least(&floor);
        assert_eq!(clamped.dot, floor.dot);
        assert_eq!(
            clamped.spectral, slow.spectral,
            "faster than the floor is kept"
        );
        assert_eq!(clamped.fft, floor.fft);
        assert_eq!(clamped.quantizer, floor.quantizer);
        assert_eq!(clamped.elapsed, slow.elapsed);
        assert!(!clamped.below(&floor));
        // At the sizes `measure` uses (it builds its points from the same
        // constants).
        let sizes = |p: &[(usize, f64)]| p.iter().map(|x| x.0).collect::<Vec<_>>();
        assert_eq!(
            sizes(&floor.dot),
            DOT_SIZES.iter().map(|d| d.0).collect::<Vec<_>>()
        );
        assert_eq!(
            sizes(&floor.spectral),
            SPECTRAL_PARTS
                .iter()
                .map(|&p| spectral_bytes(p))
                .collect::<Vec<_>>()
        );
        assert_eq!(sizes(&floor.fft), FFT_SIZES.to_vec());
    }
}
