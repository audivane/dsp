//! The catalogue's designs: lengths, rate rules and kernels of the presets
//! (the bits are pinned by the fingerprint tests).

use super::*;
use crate::design::paths::{self, fir};
use crate::design::{DesignError, Grid, rates};
use audivane_protocol::FilterPhase as PresetPhase;
use audivane_protocol::RateRatio;
use fir::Recipe;
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
            passband_end: RECOMMENDED_PASSBAND_END,
            stopband_start: 1.0,
            passband_end_hz: Some(RECOMMENDED_PASSBAND_HZ),
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
    for (rate, ratio) in [
        (44_100, RateRatio::integer(2)),
        (44_100, r(44_100, 48_000)),
        (96_000, RateRatio::integer(2)),
    ] {
        let k = crate::design::kernel(&recommended().with_output(&out), rate, ratio).unwrap();
        // The builder takes the 20 kHz pin as a fraction of the Nyquist.
        let mut card = card.clone();
        card.band.passband_end = RECOMMENDED_PASSBAND_HZ / (f64::from(rate) / 2.0);
        card.band.passband_end_hz = None;
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
