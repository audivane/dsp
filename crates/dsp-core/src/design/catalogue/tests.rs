//! The catalogue's designs: lengths, rate rules and kernels of the presets
//! (the bits are pinned by the fingerprint tests).

use super::*;
use crate::design::paths::{self, fir};
use crate::design::{DesignError, Grid, rates};
use audivane_protocol::FilterPhase as PresetPhase;
use audivane_protocol::RateRatio;
use fir::{Recipe, pairwise_sum, sinc};
use realfft::RealFftPlanner;

fn r(input: u32, output: u32) -> RateRatio {
    RateRatio::from_rates(input, output)
}

fn kernel_design(p: FilterPreset) -> KernelDesign {
    match design(p) {
        FilterDesign::Kernel(k) => k,
        d => panic!("{p:?} is {d:?}"),
    }
}

fn is_kernel(p: FilterPreset) -> bool {
    matches!(design(p), FilterDesign::Kernel(_))
}

fn linear_presets() -> impl Iterator<Item = FilterPreset> {
    FilterPreset::ALL
        .iter()
        .copied()
        .filter(|&p| is_kernel(p) && kernel_design(p).phase == Phase::Linear)
}

/// The recipe of `p` from `rate` on the grid of `ratio`.
fn recipe_at(p: FilterPreset, rate: u32, ratio: RateRatio) -> Result<Recipe, DesignError> {
    rates::check(&design(p), ratio)?;
    paths::recipe(&kernel_design(p), rate, &Grid::new(rate, ratio))
}

/// The recipe of `p` from `rate` at the integer `ratio`.
fn recipe(p: FilterPreset, rate: u32, ratio: usize) -> Option<Recipe> {
    if !is_kernel(p) {
        return None;
    }
    let ratio = RateRatio::integer(ratio as u32);
    paths::recipe(&kernel_design(p), rate, &Grid::new(rate, ratio)).ok()
}

#[derive(Debug, PartialEq)]
struct K {
    kernel: Vec<f64>,
    delay: usize,
}

fn kernel_at(p: FilterPreset, rate: u32, ratio: RateRatio) -> Result<K, DesignError> {
    let k = crate::design::kernel(&design(p), rate, ratio)?;
    Ok(K {
        kernel: k.taps,
        delay: k.delay,
    })
}

fn kernel(p: FilterPreset, rate: u32, ratio: usize) -> Result<K, DesignError> {
    kernel_at(p, rate, RateRatio::integer(ratio as u32))
}

fn linear_kernel(p: FilterPreset, rate: u32, ratio: usize) -> Result<K, DesignError> {
    assert_eq!(kernel_design(p).phase, Phase::Linear, "{p:?}");
    kernel(p, rate, ratio)
}

fn check(p: FilterPreset, ratio: RateRatio) -> Result<(), DesignError> {
    rates::check(&design(p), ratio)
}

fn grid_taps(p: FilterPreset, rate: u32, ratio: RateRatio) -> Result<usize, DesignError> {
    crate::design::grid_taps(&design(p), rate, ratio)
}

fn phase(p: FilterPreset) -> PresetPhase {
    design(p).filter_phase()
}

fn branch_sum(h: &[f64], ratio: usize, b: usize) -> f64 {
    pairwise_sum(h, b, ratio, (h.len() - b).div_ceil(ratio))
}

fn peak_index(h: &[f64]) -> usize {
    crate::design::phase::cepstral::peak_index(h)
}

#[test]
fn every_preset_has_a_design() {
    for &p in FilterPreset::ALL {
        let d = design(p);
        assert_eq!(
            matches!(d, FilterDesign::Kernel(_)),
            runtime_preset(&d).is_none(),
            "{p:?}"
        );
        if !is_kernel(p) {
            assert_eq!(runtime_preset(&d), Some(p));
        }
    }
}

#[test]
fn phase_variants_share_the_linear_recipe() {
    use FilterPreset::*;
    for (v, lin) in [
        (GaussianEarlyIp, GaussianEarly),
        (GaussianEarlyMp, GaussianEarly),
        (FlattopEarlyIp, FlattopEarly),
        (FlattopEarlyMp, FlattopEarly),
        (ChebyshevMp, Chebyshev),
        (ChebyshevShortMp, ChebyshevShort),
        (ChebyshevLongIp, ChebyshevLong),
        (ChebyshevLongMp, ChebyshevLong),
        (KaiserSteepMp, KaiserSteep),
        (KaiserSteepShortMp, KaiserSteepShort),
        (Lanczos23Mp, Lanczos23),
        (ShortLeakyMp, ShortLeaky),
    ] {
        assert_eq!(recipe(v, 44_100, 8), recipe(lin, 44_100, 8), "{v:?}");
        let (dv, dl) = (kernel_design(v), kernel_design(lin));
        assert_eq!(
            (dv.shape, dv.decimation, dv.rates),
            (dl.shape, dl.decimation, dl.rates)
        );
    }
    let path = |p| kernel_design(p).numerics.path;
    assert_eq!(path(GaussianEarlyMp), ArithmeticPath::DoubleDouble);
    assert_eq!(path(Lanczos23Mp), ArithmeticPath::FirAfter);
    assert_eq!(path(FlattopEarlyIp), ArithmeticPath::Fir);
    assert_eq!(path(ChebyshevLongIp), ArithmeticPath::ExactOverRatioBefore);
    assert_eq!(path(KaiserSteepMp), ArithmeticPath::ExactPiInverseAfter);
    assert_eq!(path(ShortLeakyMp), ArithmeticPath::ExactPiInversePairwise);
}

/// Combinations no preset has are built by the general builder
/// (`paths::general`), never by a preset's own, or refused with a typed
/// error.
#[test]
fn other_combinations_are_built_or_refused() {
    let base = kernel_design(FilterPreset::KaiserSteep);
    let ratio = RateRatio::integer(4);
    let build = |k: KernelDesign| crate::design::kernel(&FilterDesign::Kernel(k), 44_100, ratio);
    let refused = |k: KernelDesign| {
        let e = build(k).unwrap_err();
        assert!(matches!(e, DesignError::Unsupported(_)), "{e:?}");
    };
    let legacy = build(base.clone()).unwrap();
    // A linear kernel on an exact path: the same kernel to rounding.
    let exact = build(KernelDesign {
        numerics: Numerics {
            path: ArithmeticPath::ExactPiInverseAfter,
            grid_cutoff: GridCutoff::Scaled,
        },
        ..base.clone()
    })
    .unwrap();
    assert_eq!(exact.taps.len(), legacy.taps.len());
    assert!(
        exact
            .taps
            .iter()
            .zip(&legacy.taps)
            .all(|(a, b)| (a - b).abs() < 1e-12)
    );
    // A cepstral mix other than the exact one, a symmetric Kaiser window.
    build(KernelDesign {
        phase: Phase::Intermediate {
            mix: 0.5,
            method: PhaseMethod::Cepstral,
        },
        ..base.clone()
    })
    .unwrap();
    build(KernelDesign {
        shape: Shape::WindowedSinc {
            window: Window::Kaiser {
                beta: Some(20.0),
                sampling: Sampling::Symmetric,
            },
            length: Length::PerInput(Stepped::Fixed(63)),
            cutoff: Cutoff::Fraction(Stepped::Fixed(0.9)),
        },
        ..base.clone()
    })
    .unwrap();
    // The double-double path with a Kaiser window.
    refused(KernelDesign {
        phase: Phase::Minimum(PhaseMethod::Cepstral),
        numerics: Numerics {
            path: ArithmeticPath::DoubleDouble,
            grid_cutoff: GridCutoff::Scaled,
        },
        ..base.clone()
    });
    // Keeping the input samples needs the cutoff at the Nyquist.
    refused(KernelDesign {
        preserve_samples: true,
        ..base
    });
    // A block FFT geometry no preset has.
    let d = FilterDesign::BlockFft(BlockFftDesign {
        block: BlockSize::Input(4096),
        overlap: Overlap::Half,
    });
    assert_eq!(runtime_preset(&d), None);
}

#[test]
fn stepped_values() {
    let s = compact_taps();
    assert_eq!(s.at(44_100), Some(167));
    assert_eq!(s.at(49_999), Some(167));
    assert_eq!(s.at(50_000), Some(83));
    assert_eq!(s.at(99_999), Some(83));
    assert_eq!(s.at(100_000), Some(63));
    assert_eq!(s.at(3_072_000), Some(63));
    assert_eq!(Stepped::BySourceRate(vec![(Some(10), 1)]).at(10), None);
}

#[test]
fn rate_keyed_lengths() {
    use FilterPreset::*;
    let t = |p, rate, ratio| recipe(p, rate, ratio).unwrap().taps;
    // Constant-time designs: N = N_rate - L at the output rate.
    assert_eq!(t(GaussianHuge, 44_100, 8), (1 << 19) / 8 - 1);
    assert_eq!(t(GaussianHugeApod, 44_100, 16), (1 << 20) / 16 - 1);
    assert_eq!(t(FlattopHuge, 48_000, 2), (1 << 17) / 2 - 1);
    assert_eq!(t(FlattopHuge, 192_000, 32), (1 << 22) / 32 - 1);
    assert_eq!(t(FlattopMega, 44_100, 8), (1 << 20) / 8 - 1);
    assert_eq!(t(FlattopMega, 96_000, 3), 349_524);
    assert_eq!(t(FlattopXlongStd, 192_000, 4), 4095);
    // Compact designs key on the source rate.
    for (rate, taps, fc) in [
        (44_100, 167, 0.88),
        (48_000, 167, 0.88),
        (88_200, 83, 0.725),
        (96_000, 83, 0.725),
        (176_400, 63, 0.6),
        (192_000, 63, 0.6),
    ] {
        for p in [GaussianCompact, FlattopCompact, KaiserCompact] {
            let r = recipe(p, rate, 4).unwrap();
            assert_eq!((r.taps, r.cutoff), (taps, fc), "{p:?} at {rate}");
        }
    }
}

#[test]
fn lengths_delay_and_normalised_branch() {
    for p in linear_presets() {
        for ratio in [2, 3, 8] {
            let r = recipe(p, 44_100, ratio).unwrap();
            if r.taps * ratio > 300_000 {
                continue; // the long ones are covered below
            }
            let k = linear_kernel(p, 44_100, ratio).unwrap();
            assert_eq!(k.kernel.len(), r.taps * ratio, "{p:?} x{ratio}");
            assert_eq!(k.delay, k.kernel.len() / 2);
            let s = branch_sum(&k.kernel, ratio, ratio / 2);
            assert!((s - 1.0).abs() < 1e-14, "{p:?} x{ratio}: {s}");
            // The other branches are close to 1 too (less so for
            // Lanczos, whose zero crossings are off the input grid).
            let tol = if matches!(r.window, fir::Window::Lanczos { .. }) {
                5e-4
            } else {
                2e-5
            };
            for b in 0..ratio {
                let s = branch_sum(&k.kernel, ratio, b);
                assert!((s - 1.0).abs() < tol, "{p:?} x{ratio} branch {b}: {s}");
            }
            // The peak is at the delay.
            let peak = k
                .kernel
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0;
            assert_eq!(peak, k.delay, "{p:?} x{ratio}");
        }
    }
}

#[test]
fn periodic_windows_give_symmetric_kernels() {
    use FilterPreset::*;
    for p in [FlattopShort, KaiserSteepShort, HalfbandKaiser, Lanczos23] {
        for ratio in [2, 4, 8] {
            let h = linear_kernel(p, 44_100, ratio).unwrap().kernel;
            let c = h.len() / 2;
            // Even N: symmetric about N/2 (h[0] is the unpaired tap).
            for i in 1..c {
                let (a, b) = (h[c - i], h[c + i]);
                assert!((a - b).abs() <= 1e-14, "{p:?} x{ratio} i {i}");
            }
        }
    }
    // The symmetric windows are half a sample off for even N.
    let h = linear_kernel(GaussianShort, 44_100, 8).unwrap().kernel;
    let c = h.len() / 2;
    assert!(h[c - 1] > h[c + 1]);
    assert!((h[c - 1] - h[c + 1]).abs() < 1e-3);
    // Odd N (odd ratio and odd T): every recipe is exactly symmetric
    // about its middle tap.
    for p in [GaussianShort, Nuttall, Chebyshev, HalfbandTiny] {
        let h = linear_kernel(p, 44_100, 3).unwrap().kernel;
        let n = h.len();
        for i in 0..n / 2 {
            assert!((h[i] - h[n - 1 - i]).abs() <= 1e-12, "{p:?} i {i}");
        }
    }
}

#[test]
fn nyquist_presets_keep_the_input_samples() {
    use FilterPreset::*;
    for p in [
        GaussianNyquist,
        GaussianNyquistShort,
        HalfbandKaiser,
        HalfbandLarge,
        HalfbandMedium,
        HalfbandSmall,
        HalfbandTiny,
        HannNyquist,
    ] {
        for ratio in [2, 3, 4, 8] {
            let k = linear_kernel(p, 44_100, ratio).unwrap();
            let h = &k.kernel;
            // Summing the long Hann branch includes roundoff at the
            // sinc's zero crossings.
            let tol = if p == HannNyquist { 1e-12 } else { 1e-15 };
            let centre_branch = k.delay % ratio;
            for (j, &v) in h.iter().skip(centre_branch).step_by(ratio).enumerate() {
                let want = if j * ratio + centre_branch == k.delay {
                    1.0
                } else {
                    0.0
                };
                assert!((v - want).abs() < tol, "{p:?} x{ratio} tap {j}: {v}");
            }
        }
    }
}

#[test]
fn lanczos_spans_exactly_a_lobes() {
    // x2, a = 23: N = 94, centre 47, x = (n - 47) * 23 / 47.
    let h = linear_kernel(FilterPreset::Lanczos23, 44_100, 2)
        .unwrap()
        .kernel;
    assert_eq!(h.len(), 94);
    assert_eq!(h[0], 0.0); // x = -a exactly
    assert!(h[1] != 0.0 && h[93] != 0.0);
    // Not a Nyquist filter: the centre branch is not an impulse.
    assert!(h[47 + 2].abs() > 1e-3);
    // Zero crossings where x is an integer: n - 47 = 47 m / 23, which
    // only lands on the grid for m = 0 and m = +-23.
    let raw = fir::prototype(&recipe(FilterPreset::Lanczos23, 44_100, 2).unwrap(), 2);
    assert_eq!(raw[47], 1.0);
    let x = 23.0 / 47.0;
    let want = sinc(x) * sinc(x / 23.0);
    assert_eq!(raw[48], want);
}

#[test]
fn spot_values() {
    // Unnormalised prototype taps against values computed directly
    // from the formulas.
    let r = recipe(FilterPreset::GaussianShort, 44_100, 2).unwrap();
    let h = fir::prototype(&r, 2);
    let (n, c) = (254.0_f64, 126.0_f64);
    let g = |i: f64| (-0.5 * ((i - c) / (0.175 * c)).powi(2)).exp();
    for i in [0usize, 1, 100, 126, 127, 128, 253] {
        let k = i as f64 - (n / 2.0);
        let want = sinc(FC_STD * k / 2.0) * g(i as f64);
        assert!((h[i] - want).abs() < 1e-16, "tap {i}: {} vs {want}", h[i]);
    }
    assert_eq!(h[127], 1.0 * g(127.0));

    // The Hann period spans 16,383 input samples. Check away from the
    // centre, where the window is sensitive to the period.
    let r = recipe(FilterPreset::HannNyquist, 44_100, 2).unwrap();
    let h = fir::prototype(&r, 2);
    let n = 2 * 16_383;
    assert_eq!(h.len(), n);
    let k = 8191;
    let w = 0.5 * (1.0 + (2.0 * PI * k as f64 / n as f64).cos());
    assert!((h[n / 2 + k] - sinc(k as f64 / 2.0) * w).abs() < 1e-15);
}

#[test]
fn long_designs() {
    // Lengths and the normalised branch of the long and rate-keyed
    // designs (also checks they design in reasonable time).
    use FilterPreset::*;
    for (p, rate, ratio, n) in [
        (GaussianHuge, 44_100, 8, (1 << 19) - 8),
        (FlattopMega, 44_100, 8, (1 << 20) - 8),
        (Lanczos65535, 44_100, 2, 2 * 131_071),
        (FlattopXlongStd, 96_000, 4, 4 * 4095),
    ] {
        let k = linear_kernel(p, rate, ratio).unwrap();
        assert_eq!(k.kernel.len(), n, "{p:?}");
        let s = branch_sum(&k.kernel, ratio, ratio / 2);
        assert!((s - 1.0).abs() < 1e-12, "{p:?}: {s}");
    }
}

#[test]
fn decimated_lengths_follow_the_family_rules() {
    use FilterPreset::*;
    for (p, input, output, taps) in [
        (GaussianLong, 96_000, 48_000, 1021),
        (GaussianLong, 192_000, 48_000, 2043),
        (ChebyshevLong, 96_000, 48_000, 413),
        (Chebyshev, 96_000, 48_000, 383),
        (KaiserSteep, 96_000, 48_000, 1155),
        (ChebyshevShort, 96_000, 48_000, 255),
        (GaussianShort, 96_000, 48_000, 253),
        (FlattopShort, 96_000, 48_000, 253),
        // Fractional decimation: ceil(511 * 160 / 147) = 557, odd.
        (GaussianLong, 48_000, 44_100, 557),
    ] {
        let rec = recipe_at(p, input, r(input, output)).unwrap();
        assert_eq!(rec.taps, taps, "{p:?} {input} -> {output}");
    }
    // Upsampling keeps the integer recipe's length.
    let up = recipe_at(GaussianLong, 44_100, r(44_100, 48_000)).unwrap();
    assert_eq!(up.taps, 511);
    assert_eq!(
        grid_taps(GaussianLong, 44_100, r(44_100, 48_000)).unwrap(),
        160 * 511
    );
}

#[test]
fn window_and_cutoff_switch_where_the_rules_say() {
    use FilterPreset::*;
    let kaiser = fir::Window::KaiserPeriodic { beta: 7.0 * PI };
    // Fractional: the short Chebyshev designs switch, the long one not.
    assert_eq!(
        recipe_at(Chebyshev, 44_100, r(44_100, 48_000))
            .unwrap()
            .window,
        kaiser
    );
    assert_ne!(
        recipe_at(ChebyshevLong, 44_100, r(44_100, 48_000))
            .unwrap()
            .window,
        kaiser
    );
    // Integer decimation: the long one switches, the short ones keep
    // their Chebyshev window.
    assert_eq!(
        recipe_at(ChebyshevLong, 96_000, r(96_000, 48_000))
            .unwrap()
            .window,
        kaiser
    );
    assert_ne!(
        recipe_at(Chebyshev, 96_000, r(96_000, 48_000))
            .unwrap()
            .window,
        kaiser
    );
    let base = recipe(GaussianLong, 96_000, 1).unwrap().cutoff;
    let dec = recipe_at(GaussianLong, 96_000, r(96_000, 48_000))
        .unwrap()
        .cutoff;
    assert!((dec - base / 2.0).abs() < 1e-15);
}

#[test]
fn ratio_classes() {
    use FilterPreset::*;
    let frac = r(44_100, 48_000);
    let dec = r(96_000, 48_000);
    for p in [
        FlattopMega,
        GaussianHuge,
        Lanczos65535,
        CascadeFir,
        BlockFft,
        Cosine,
    ] {
        assert!(check(p, frac).is_err(), "{p:?}");
        assert!(check(p, dec).is_err(), "{p:?}");
    }
    for p in [
        GaussianCompact,
        KaiserCompact,
        Lanczos23Mp,
        Lanczos511,
        Nuttall,
        ShortLeaky,
        ShortLeakyMp,
        HalfbandLarge,
        HannNyquist,
    ] {
        assert!(check(p, frac).is_ok(), "{p:?}");
        assert!(check(p, dec).is_err(), "{p:?}");
    }
    for p in [
        GaussianShort,
        FlattopShort,
        GaussianLong,
        Chebyshev,
        ChebyshevLongIp,
        KaiserSteepMp,
        FlattopEarly,
    ] {
        assert!(check(p, frac).is_ok(), "{p:?}");
        assert!(check(p, dec).is_ok(), "{p:?}");
    }
}

/// Every accepted preset designs at a fractional and a decimating
/// ratio, with unity gain on the normalised branch and a stopband that
/// protects the lower Nyquist.
#[test]
fn accepted_presets_design_with_unity_gain() {
    for &p in FilterPreset::ALL {
        for (input, output) in [(44_100, 48_000), (96_000, 44_100)] {
            let ratio = r(input, output);
            if check(p, ratio).is_err() {
                continue;
            }
            let n = grid_taps(p, input, ratio).unwrap();
            let linear = phase(p) == PresetPhase::Linear;
            if n > if linear { 200_000 } else { 40_000 } {
                continue; // the long designs: covered by the acceptance runs
            }
            let k = kernel_at(p, input, ratio).unwrap();
            let l = ratio.up as usize;
            let start = l / 2;
            let sum: f64 = k.kernel[start..].iter().step_by(l).sum();
            // Designs normalised before their phase transform keep the
            // whole-kernel gain, so a branch sums to 1 only closely.
            let tol = if linear { 1e-9 } else { 1e-6 };
            assert!((sum - 1.0).abs() < tol, "{p:?} {ratio}: branch sum {sum}");
            assert!(k.delay < k.kernel.len());
        }
    }
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

/// At x3 and x28 a few of the early Gaussian's stopband bins fall below
/// the log floor; they must stay deep (no spikes after truncation).
#[test]
fn deep_bins_stay_deep() {
    for ratio in [3usize, 28] {
        for p in [FilterPreset::GaussianEarlyMp, FilterPreset::GaussianEarlyIp] {
            let h = kernel(p, 44_100, ratio).unwrap().kernel;
            let nfft = (8 * h.len()).next_power_of_two();
            let db = mag_db(&h, nfft);
            let dc = db[0];
            // From 1.25x the source Nyquist up: bin k is k/nfft of the
            // output rate, the source Nyquist is 1/(2 ratio) of it.
            let from = (1.25 * nfft as f64 / (2.0 * ratio as f64)) as usize;
            let worst = db[from..].iter().copied().fold(f64::NEG_INFINITY, f64::max) - dc;
            assert!(
                worst < -100.0,
                "{p:?} x{ratio}: stopband peak {worst:.1} dB"
            );
        }
    }
}

/// A 0.5 impulse through `p` at 44.1 kHz x8, as S32, around the peak.
fn s32_around_peak(p: FilterPreset) -> (usize, usize, Vec<i64>) {
    let k = kernel(p, 44_100, 8).unwrap();
    let d = k.delay;
    let q = k.kernel[d - 3..d + 4]
        .iter()
        .map(|v| (0.5 * v * 2f64.powi(31)).round_ties_even() as i64)
        .collect();
    (k.kernel.len(), d, q)
}

#[test]
fn spot_values_at_x8() {
    // Regression values: the engine's own S32 output.
    assert_eq!(
        s32_around_peak(FilterPreset::Lanczos23Mp),
        (
            376,
            20,
            vec![
                719798936, 786566796, 833792075, 856020050, 848945009, 809944633, 738516636
            ]
        )
    );
    assert_eq!(
        s32_around_peak(FilterPreset::ShortLeakyMp),
        (
            376,
            32,
            vec![
                619429044, 660656740, 690580268, 706551660, 706297896, 688115794, 651053736
            ]
        )
    );
    assert_eq!(
        s32_around_peak(FilterPreset::ChebyshevLongMp),
        (
            1656,
            47,
            vec![
                589757207, 611730916, 624173722, 625589529, 614746776, 590776954, 553266718
            ]
        )
    );
}

#[test]
fn preset_kernels() {
    use FilterPreset::*;
    let presets = [
        GaussianEarlyIp,
        GaussianEarlyMp,
        FlattopEarlyIp,
        FlattopEarlyMp,
        ChebyshevMp,
        ChebyshevShortMp,
        ChebyshevLongIp,
        ChebyshevLongMp,
        KaiserSteepMp,
        KaiserSteepShortMp,
        Lanczos23Mp,
        ShortLeakyMp,
    ];
    for p in presets {
        for ratio in [2usize, 3] {
            let r = recipe(p, 44_100, ratio).unwrap();
            let k = kernel(p, 44_100, ratio).unwrap();
            let n = ratio * r.taps;
            let ip = phase(p) == PresetPhase::Intermediate;
            assert_eq!(k.kernel.len(), if ip { 4 * n } else { n }, "{p:?} x{ratio}");
            assert_eq!(k.delay, peak_index(&k.kernel));
            let at = k.delay as f64 / k.kernel.len() as f64;
            if ip {
                assert!((at - 0.375).abs() < 0.02, "{p:?} x{ratio}: peak at {at}");
            } else {
                assert!(at < 0.1, "{p:?} x{ratio}: peak at {at}");
            }
            // the normalised branch sums to 1 (closely, when the
            // normalisation comes before the transform)
            let after = !matches!(
                kernel_design(p).numerics.path,
                ArithmeticPath::Fir | ArithmeticPath::ExactOverRatioBefore
            );
            let tol = if after { 1e-12 } else { 1e-8 };
            let s: f64 = k.kernel[ratio / 2..].iter().step_by(ratio).sum();
            assert!((s - 1.0).abs() < tol, "{p:?} x{ratio}: {s}");
            // every branch is close to 1 (passband gain `ratio`)
            for b in 0..ratio {
                let s: f64 = k.kernel[b..].iter().step_by(ratio).sum();
                assert!((s - 1.0).abs() < 1e-3, "{p:?} x{ratio} branch {b}: {s}");
            }
            // deterministic
            assert_eq!(kernel(p, 44_100, ratio).unwrap(), k);
        }
    }
}

/// The app's Recommended card (`app/src/lib/upsamplerRecommended.ts`) over
/// a config: the fields it writes, the others as they were.
fn recommended_card(
    cur: crate::design::paths::standard::ManualConfig,
) -> crate::design::paths::standard::ManualConfig {
    use audivane_protocol::{CutoffMode, FilterPhase, TapsUnit, WindowKind};
    crate::design::paths::standard::ManualConfig {
        window: WindowKind::Kaiser,
        cutoff_mode: CutoffMode::Band,
        band: BandSpec {
            passband_end: 0.908,
            stopband_start: 1.0,
            attenuation_db: 150.0,
        },
        auto_taps: Some(FilterErrorBudget::MatchOutput),
        phase: FilterPhase::Linear,
        taps_unit: TapsUnit::Output,
        preserve_samples: false,
        preset: None,
        target: RECOMMENDED_TARGET,
        ..cur
    }
}

#[test]
fn recommended_is_the_app_card() {
    use crate::design::paths::standard::{ManualConfig, design_of};
    use audivane_protocol::{DitherKind, OutputConfig};
    assert_eq!(
        RECOMMENDED_TARGET,
        audivane_protocol::RateTarget::MaxRate(768_000)
    );
    assert_eq!(by_name(RECOMMENDED), Some(recommended()));
    assert_eq!(
        by_name("flattop_long"),
        Some(design(FilterPreset::FlattopLong))
    );
    assert_eq!(by_name("nonesuch"), None);
    // Whatever the fields the card leaves alone hold (the fingerprint
    // harness's Recommended rows start from beta 14 and cutoff 1.0).
    let starts = [
        ManualConfig::default(),
        ManualConfig {
            kaiser_beta: 14.0,
            cutoff: 1.0,
            taps: 1234,
            phase_mix: 0.2,
            taper: 0.9,
            ..ManualConfig::default()
        },
    ];
    let outputs = [
        OutputConfig {
            bits: 32,
            dither: DitherKind::Tpdf,
            headroom_db: 0.0,
        },
        OutputConfig {
            bits: 24,
            dither: DitherKind::Tpdf,
            headroom_db: 0.0,
        },
        OutputConfig {
            bits: 16,
            dither: DitherKind::None,
            headroom_db: 0.0,
        },
    ];
    for cur in starts {
        let card = recommended_card(cur);
        assert_eq!(
            FilterDesign::Kernel(design_of(&card).unwrap()),
            recommended()
        );
        for out in &outputs {
            assert_eq!(
                FilterDesign::Kernel(design_of(&card.with_output(out)).unwrap()),
                recommended().with_output(out),
                "{out:?}"
            );
        }
    }
    // The resolved budget is the output's, and nothing else changes.
    let out = outputs[2];
    let FilterDesign::Kernel(k) = recommended().with_output(&out) else {
        unreachable!()
    };
    let Shape::WindowedSinc { length, .. } = k.shape else {
        unreachable!()
    };
    assert_eq!(
        length,
        Length::Auto(FilterErrorBudget::Db(
            audivane_protocol::filter_error_budget_db(&out)
        ))
    );
    assert_eq!(
        design(FilterPreset::GaussianLong).with_output(&out),
        design(FilterPreset::GaussianLong)
    );
}

/// The Recommended design builds the Custom builder's kernel for the card.
#[test]
fn recommended_builds_the_cards_kernel() {
    use crate::design::paths::standard::{ManualConfig, UpsamplerDesign, try_design_kernel_for};
    use audivane_protocol::{DitherKind, OutputConfig};
    let out = OutputConfig {
        bits: 16,
        dither: DitherKind::None,
        headroom_db: 0.0,
    };
    let card = recommended_card(ManualConfig::default()).with_output(&out);
    for ratio in [RateRatio::integer(2), r(44_100, 48_000)] {
        let k = crate::design::kernel(&recommended().with_output(&out), 44_100, ratio).unwrap();
        let d = UpsamplerDesign::try_for_ratio(&card, ratio).unwrap();
        let want = try_design_kernel_for(&d, ratio).unwrap();
        assert_eq!(
            k.taps.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            want.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "{ratio}"
        );
        // Linear phase: the delay is the kernel's centre.
        assert_eq!(k.delay_exact, (want.len() - 1) as f64 / 2.0, "{ratio}");
    }
}
