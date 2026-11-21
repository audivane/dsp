//! The named designs: every [`FilterPreset`] as a [`FilterDesign`] value,
//! and the app's Recommended design ([`recommended`]).
//!
//! The values are independent of the rate: the compact designs key their
//! length and cutoff on the source rate with stepped values, the
//! constant-time designs use an output span, and the Chebyshev designs
//! state their fallback windows. A preset is exactly its design.

use super::{
    Accept, ArithmeticPath, BandSpec, BlockFftDesign, BlockSize, CascadeStage, Cutoff, Decimation,
    Fallback, FallbackWhen, FilterDesign, FilterErrorBudget, FlattopSet, GridCutoff, InterpMethod,
    KernelDesign, Length, Numerics, OddRule, Overlap, Phase, PhaseMethod, Rates, Sampling, Shape,
    Span, Stepped, Window,
};
use audivane_protocol::{FilterPreset, RateTarget};
use std::f64::consts::PI;

// Cutoff ladder, as fractions of the source Nyquist. The steps are
// multiples of 256.25 Hz below the Nyquist of a 44.1 kHz source, but they
// are applied as the same fractions at every source rate.

/// Nyquist - 1 x 256.25 Hz (at 44.1 kHz).
pub const FC_XL: f64 = 1.0 - 256.25 / 22050.0;
/// Nyquist - 4 x 256.25 Hz.
pub const FC_STD: f64 = 21025.0 / 22050.0;
/// Nyquist - 6 x 256.25 Hz (apodizing).
pub const FC_APOD: f64 = 20512.5 / 22050.0;
/// 253/256 of Nyquist, for the long steep Kaiser design.
pub const FC_STEEP: f64 = 253.0 / 256.0;
/// 125/133 of Nyquist, for the short steep Kaiser design.
pub const FC_STEEP_SHORT: f64 = 125.0 / 133.0;
/// 27/32 of Nyquist: the early roll-off designs.
pub const FC_EARLY: f64 = 27.0 / 32.0;
/// 380/441 of Nyquist (19 kHz at 44.1 kHz): the short leaky design.
pub const FC_SHORT_LEAKY: f64 = 380.0 / 441.0;
/// Nyquist: a Nyquist (half-band type) filter.
pub const FC_NYQUIST: f64 = 1.0;

/// Chebyshev sidelobe ratios.
pub const CHEB_32: f64 = 1.0 / 4_294_967_296.0; // 2^-32, -192.7 dB
pub const CHEB_24: f64 = 1.0 / 16_777_216.0; // 2^-24, -144.5 dB
pub const CHEB_20: f64 = 1.0 / 1_048_576.0; // 2^-20, -120.4 dB
pub const CHEB_16: f64 = 1.0 / 65_536.0; // 2^-16, -96.3 dB

/// The intermediate-phase blend of the `_ip` presets (75 % linear phase).
pub const INTERMEDIATE_MIX: f64 = super::phase::cepstral::INTERMEDIATE_BLEND;

/// The compact designs' taps per input sample, by source rate.
fn compact_taps() -> Stepped<usize> {
    Stepped::BySourceRate(vec![(Some(50_000), 167), (Some(100_000), 83), (None, 63)])
}

/// The compact designs' cutoffs, by source rate.
fn compact_cutoff() -> Stepped<f64> {
    Stepped::BySourceRate(vec![
        (Some(50_000), 0.88),
        (Some(100_000), 0.725),
        (None, 0.6),
    ])
}

/// The design of `p`.
pub fn design(p: FilterPreset) -> FilterDesign {
    use FilterPreset::*;

    let gauss = |alpha| Window::Gaussian {
        alpha,
        sampling: Sampling::IntegerCentre,
    };
    let kaiser = |beta| Window::Kaiser {
        beta: Some(beta),
        sampling: Sampling::Periodic,
    };
    let flattop = |set| Window::Flattop {
        set,
        sampling: Sampling::Periodic,
    };
    let cheb = |sidelobe_ratio| Window::Chebyshev {
        sidelobe_ratio,
        sampling: Sampling::Symmetric,
    };
    let nuttall = Window::Nuttall {
        sampling: Sampling::Symmetric,
    };
    let hann = Window::Hann {
        sampling: Sampling::Periodic,
    };
    let (hft196, hft223, hft248) = (
        flattop(FlattopSet::Hft196d),
        flattop(FlattopSet::Hft223d),
        flattop(FlattopSet::Hft248d),
    );
    let kaiser_steep = kaiser(8.75 * PI);
    let kaiser_7pi = kaiser(7.0 * PI);

    let minimum = Phase::Minimum(PhaseMethod::Cepstral);
    let intermediate = Phase::Intermediate {
        mix: INTERMEDIATE_MIX,
        method: PhaseMethod::Cepstral,
    };
    let per_input = |t| Length::PerInput(Stepped::Fixed(t));
    let ladder = Length::OutputSpan(Span::Ladder);
    let fc = |c| Cutoff::Fraction(Stepped::Fixed(c));

    // A linear-phase windowed sinc on the `fir` path, any ratio, decimated
    // by scaling the length and rounding an even one down.
    let sinc = |window, length, cutoff| KernelDesign {
        shape: Shape::WindowedSinc {
            window,
            length,
            cutoff,
        },
        decimation: Decimation::Scale {
            odd: OddRule::Down,
            base_taps: None,
        },
        phase: Phase::Linear,
        preserve_samples: false,
        rates: Rates {
            accept: Accept::Any,
            fallback_window: None,
        },
        numerics: Numerics {
            path: ArithmeticPath::Fir,
            grid_cutoff: GridCutoff::Scaled,
        },
    };
    let fixed = |window, taps, cutoff| sinc(window, per_input(taps), fc(cutoff));
    let compact = |window| {
        sinc(
            window,
            Length::PerInput(compact_taps()),
            Cutoff::Fraction(compact_cutoff()),
        )
    };
    // Lanczos: the windowed sinc's phase and rate fields (it never
    // decimates), fractional upsampling.
    let lanczos = |lobes| KernelDesign {
        shape: Shape::Lanczos { lobes },
        ..accept(fixed(nuttall, 1, FC_NYQUIST), Accept::Upsampling)
    };
    // The phase variants of a linear design, on their arithmetic path.
    let phase = |d: KernelDesign, phase, path| KernelDesign {
        phase,
        numerics: Numerics {
            path,
            grid_cutoff: GridCutoff::Scaled,
        },
        ..d
    };

    // The Chebyshev designs: rate rules of their own.
    let cheb_short = |taps, cutoff, base| KernelDesign {
        decimation: Decimation::Scale {
            odd: OddRule::Up,
            base_taps: Some(base),
        },
        rates: Rates {
            accept: Accept::Any,
            fallback_window: Some(Fallback {
                when: FallbackWhen::NotMultiple,
                window: kaiser_7pi,
            }),
        },
        ..fixed(cheb(CHEB_32), taps, cutoff)
    };
    let chebyshev = cheb_short(117, FC_STD, 191);
    let chebyshev_short = cheb_short(71, FC_APOD, 127);
    let chebyshev_long = KernelDesign {
        rates: Rates {
            accept: Accept::Any,
            fallback_window: Some(Fallback {
                when: FallbackWhen::Decimating,
                window: kaiser_7pi,
            }),
        },
        ..fixed(cheb(CHEB_32), 207, FC_APOD)
    };
    let steep = |taps, cutoff| KernelDesign {
        decimation: Decimation::Scale {
            odd: OddRule::Up,
            base_taps: None,
        },
        ..fixed(kaiser_steep, taps, cutoff)
    };

    let gaussian_early = fixed(gauss(0.128), 255, FC_EARLY);
    let flattop_early = fixed(hft248, 255, FC_EARLY);
    let leaky = accept(fixed(nuttall, 47, FC_SHORT_LEAKY), Accept::Upsampling);
    let lanczos23 = lanczos(23);

    use ArithmeticPath::*;
    let kernel = match p {
        GaussianShort => fixed(gauss(0.175), 127, FC_STD),
        GaussianMedium => fixed(gauss(0.175), 255, FC_APOD),
        GaussianLong => fixed(gauss(0.125), 511, FC_APOD),
        GaussianXlong => fixed(gauss(0.125), 4095, FC_XL),
        GaussianXlongApod => fixed(gauss(0.125), 4095, FC_APOD),
        GaussianEarly => gaussian_early,
        GaussianEarlyIp => phase(gaussian_early, intermediate, DoubleDouble),
        GaussianEarlyMp => phase(gaussian_early, minimum, DoubleDouble),
        GaussianNyquist => fixed(gauss(0.125), 451, FC_NYQUIST),
        GaussianNyquistShort => fixed(gauss(0.2), 191, FC_NYQUIST),
        GaussianCompact => accept(compact(gauss(0.15)), Accept::Upsampling),
        GaussianHuge => accept(sinc(gauss(0.125), ladder, fc(FC_XL)), Accept::Integer),
        GaussianHugeApod => accept(sinc(gauss(0.125), ladder, fc(FC_APOD)), Accept::Integer),

        Nuttall => accept(fixed(nuttall, 117, FC_STD), Accept::Upsampling),
        Flattop => fixed(hft248, 511, FC_STD),
        FlattopShort => fixed(hft196, 127, FC_STD),
        FlattopMedium => fixed(hft196, 255, FC_APOD),
        FlattopLong => fixed(hft248, 511, FC_APOD),
        FlattopXlong => fixed(hft248, 4095, FC_XL),
        FlattopXlongApod => fixed(hft248, 4095, FC_APOD),
        FlattopXlongStd => accept(fixed(hft248, 4095, FC_STD), Accept::Integer),
        FlattopEarly => flattop_early,
        FlattopEarlyIp => phase(flattop_early, intermediate, Fir),
        FlattopEarlyMp => phase(flattop_early, minimum, Fir),
        FlattopCompact => accept(compact(hft223), Accept::Upsampling),
        FlattopHuge => accept(sinc(hft248, ladder, fc(FC_STD)), Accept::Integer),
        // N = 2^20 - L (rounded down to a multiple of L).
        FlattopMega => accept(
            sinc(hft248, Length::OutputSpan(Span::Taps(1 << 20)), fc(FC_STD)),
            Accept::Integer,
        ),

        Chebyshev => chebyshev,
        ChebyshevMp => phase(chebyshev, minimum, ExactPiInverseAfter),
        ChebyshevShort => chebyshev_short,
        ChebyshevShortMp => phase(chebyshev_short, minimum, ExactPiInverseAfter),
        ChebyshevLong => chebyshev_long,
        ChebyshevLongIp => phase(chebyshev_long, intermediate, ExactOverRatioBefore),
        ChebyshevLongMp => phase(chebyshev_long, minimum, ExactOverRatioBefore),

        KaiserSteep => steep(577, FC_STEEP),
        KaiserSteepMp => phase(steep(577, FC_STEEP), minimum, ExactPiInverseAfter),
        KaiserSteepShort => steep(255, FC_STEEP_SHORT),
        KaiserSteepShortMp => phase(steep(255, FC_STEEP_SHORT), minimum, ExactPiInverseAfter),
        KaiserCompact => accept(compact(kaiser(8.0 * PI)), Accept::Upsampling),

        // Nyquist filters: the centre branch is an impulse to within
        // floating-point rounding.
        HalfbandKaiser => accept(fixed(kaiser_7pi, 255, FC_NYQUIST), Accept::Upsampling),
        HalfbandLarge => accept(fixed(cheb(CHEB_32), 255, FC_NYQUIST), Accept::Upsampling),
        HalfbandMedium => accept(fixed(cheb(CHEB_24), 127, FC_NYQUIST), Accept::Upsampling),
        HalfbandSmall => accept(fixed(cheb(CHEB_20), 63, FC_NYQUIST), Accept::Upsampling),
        HalfbandTiny => accept(fixed(cheb(CHEB_16), 35, FC_NYQUIST), Accept::Upsampling),
        HannNyquist => accept(fixed(hann, 16_383, FC_NYQUIST), Accept::Upsampling),

        Lanczos23 => lanczos23,
        Lanczos23Mp => phase(lanczos23, minimum, FirAfter),
        Lanczos511 => lanczos(511),
        Lanczos2047 => lanczos(2047),
        Lanczos8191 => lanczos(8191),
        Lanczos32767 => accept(lanczos(32767), Accept::Integer),
        Lanczos65535 => accept(lanczos(65535), Accept::Integer),

        // Short, early roll-off, leaky at Nyquist.
        ShortLeaky => leaky,
        ShortLeakyMp => phase(leaky, minimum, ExactPiInversePairwise),

        CascadeFir => return FilterDesign::Cascade(CascadeStage::Fir),
        CascadeFirMp => return FilterDesign::Cascade(CascadeStage::FirMinimum),
        CascadeFirAsym => return FilterDesign::Cascade(CascadeStage::FirAsymmetric),
        CascadeFft => return FilterDesign::Cascade(CascadeStage::FrequencySampled),
        CascadeElliptic => return FilterDesign::Cascade(CascadeStage::Elliptic),
        CascadeInverseChebyshev => return FilterDesign::Cascade(CascadeStage::InverseChebyshev),
        Cosine => return FilterDesign::Interpolator(InterpMethod::Cosine),
        CatmullRom => return FilterDesign::Interpolator(InterpMethod::CatmullRom),
        TableSinc => return FilterDesign::Interpolator(InterpMethod::TableSinc),
        BlockFft => {
            return FilterDesign::BlockFft(BlockFftDesign {
                block: BlockSize::Input(16_384),
                overlap: Overlap::QuarterAverage,
            });
        }
        BlockFftFast => {
            return FilterDesign::BlockFft(BlockFftDesign {
                block: BlockSize::Input(16_384),
                overlap: Overlap::Half,
            });
        }
        BlockFftLarge => {
            return FilterDesign::BlockFft(BlockFftDesign {
                block: BlockSize::Output(1 << 20),
                overlap: Overlap::QuarterAverage,
            });
        }
    };
    FilterDesign::Kernel(kernel)
}

/// The catalogue name of the Recommended design.
pub const RECOMMENDED: &str = "recommended";

/// The rate target the Recommended card sets with its design (not part of
/// the design): the highest power-of-two multiple of the source rate up
/// to 768 kHz, so never a fractional ratio.
pub const RECOMMENDED_TARGET: RateTarget = RateTarget::MaxRate(768_000);

/// The app's Recommended design (`app/src/lib/upsamplerRecommended.ts`):
/// a Kaiser windowed sinc, flat to 0.908 (20 kHz at 44.1 kHz) and stopped
/// from 1.00 times the lower Nyquist, its length the shortest measured one
/// that matches the output's resolution (the 150 dB band depth is the editor's default;
/// the automatic search sets its own from the budget), linear phase, on the
/// `standard` path. No preserved samples;
/// headroom and the target are not part of a design.
pub fn recommended() -> FilterDesign {
    FilterDesign::Kernel(KernelDesign {
        shape: Shape::WindowedSinc {
            window: Window::Kaiser {
                beta: None,
                sampling: Sampling::Symmetric,
            },
            length: Length::Auto(FilterErrorBudget::MatchOutput),
            cutoff: Cutoff::Band(BandSpec {
                passband_end: 0.908,
                stopband_start: 1.0,
                attenuation_db: 150.0,
            }),
        },
        decimation: Decimation::Keep,
        phase: Phase::Linear,
        preserve_samples: false,
        rates: Rates {
            accept: Accept::Any,
            fallback_window: None,
        },
        numerics: Numerics {
            path: ArithmeticPath::Standard,
            grid_cutoff: GridCutoff::DesignRatio,
        },
    })
}

/// The catalogue name of the varispeed stage's steep filters ([`varispeed`]).
pub const VARISPEED: &str = "varispeed";

/// The varispeed stage's steep ×2 and ÷2, built at the ratios 2/1 and 1/2:
/// a Kaiser windowed sinc, flat to `passband_end` (the stage uses 0.907, and
/// 0.8 above
/// 192 kHz) and stopped from the Nyquist at 150 dB, the band length, linear
/// phase, on the `standard` path. 150 dB is the floor for its
/// `match_output` attenuation, which no output reaches (24 bits and up:
/// 146.2 dB), so it is fixed. Built in, not a preset and not user-facing.
pub fn varispeed(passband_end: f64) -> FilterDesign {
    FilterDesign::Kernel(KernelDesign {
        shape: Shape::WindowedSinc {
            window: Window::Kaiser {
                beta: None,
                sampling: Sampling::Symmetric,
            },
            length: Length::Band,
            cutoff: Cutoff::Band(BandSpec {
                passband_end,
                stopband_start: 1.0,
                attenuation_db: 150.0,
            }),
        },
        decimation: Decimation::Keep,
        phase: Phase::Linear,
        preserve_samples: false,
        rates: Rates {
            accept: Accept::Any,
            fallback_window: None,
        },
        numerics: Numerics {
            path: ArithmeticPath::Standard,
            grid_cutoff: GridCutoff::DesignRatio,
        },
    })
}

/// The design named `name`: a [`FilterPreset`] name or [`RECOMMENDED`].
pub fn by_name(name: &str) -> Option<FilterDesign> {
    if name == RECOMMENDED {
        return Some(recommended());
    }
    FilterPreset::from_name(name).map(design)
}

/// The preset whose runtime builds the runtime design `d` (the cascade,
/// interpolator and block FFT runtimes are keyed by preset); `None` for a
/// kernel design or a geometry no preset has.
pub(crate) fn runtime_preset(d: &FilterDesign) -> Option<FilterPreset> {
    use FilterPreset::*;
    Some(match d {
        FilterDesign::Kernel(_) => return None,
        FilterDesign::Cascade(stage) => match stage {
            CascadeStage::Fir => CascadeFir,
            CascadeStage::FirMinimum => CascadeFirMp,
            CascadeStage::FirAsymmetric => CascadeFirAsym,
            CascadeStage::FrequencySampled => CascadeFft,
            CascadeStage::Elliptic => CascadeElliptic,
            CascadeStage::InverseChebyshev => CascadeInverseChebyshev,
        },
        FilterDesign::Interpolator(method) => match method {
            InterpMethod::Cosine => Cosine,
            InterpMethod::CatmullRom => CatmullRom,
            InterpMethod::TableSinc => TableSinc,
        },
        FilterDesign::BlockFft(b) => match (b.block, b.overlap) {
            (BlockSize::Input(16_384), Overlap::QuarterAverage) => BlockFft,
            (BlockSize::Input(16_384), Overlap::Half) => BlockFftFast,
            (BlockSize::Output(0x10_0000), Overlap::QuarterAverage) => BlockFftLarge,
            _ => return None,
        },
    })
}

/// `d` with the ratio class `accept`.
fn accept(d: KernelDesign, accept: Accept) -> KernelDesign {
    KernelDesign {
        rates: Rates { accept, ..d.rates },
        ..d
    }
}

#[cfg(test)]
mod tests;
