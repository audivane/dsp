//! Named upsampling filters: fixed recipes selected by
//! [`audivane_protocol::FilterPreset`].
//!
//! Most presets are a windowed-sinc (or Lanczos) kernel run by the regular
//! polyphase engine ([`crate::resample::Upsampler`]); the rest have their
//! own runtimes (x2/x3 cascades, short interpolators, block FFT
//! interpolation). Every runtime implements [`UpsamplerRuntime`].
//!
//! Kernel convention: kernels are at the output rate and each polyphase
//! branch sums to about 1 (passband gain `ratio` over the whole kernel), so
//! an impulse of amplitude `A` comes out as `A * kernel`. Which branch is
//! normalised exactly is part of each recipe.
//!
//! A preset is a named design ([`crate::design::catalogue`]), built like
//! any design ([`crate::design::upsampler()`]); its refusals are
//! [`PresetError`]s. The runtimes with fixed internal filters live here:
//!
//! - [`cascade`]: the x2/x3 stage cascades (the `cascade_*` presets).
//! - [`interp`]: the `nos`, `cosine`, `catmull_rom` and `table_sinc` presets.
//! - [`block_fft`]: the `block_fft*` presets.

pub mod block_fft;
pub mod cascade;
pub mod interp;
pub mod runtime;

pub use audivane_protocol::FilterPreset;
pub use runtime::UpsamplerRuntime;

use crate::design::{self, catalogue};
use audivane_protocol::RateRatio;

/// Why a preset cannot be built.
#[derive(Clone, Debug, PartialEq)]
pub enum PresetError {
    /// The preset does not run at this ratio; `need` says what it needs.
    Ratio {
        preset: FilterPreset,
        ratio: RateRatio,
        need: &'static str,
    },
    /// The kernel at this ratio would be longer than
    /// [`design::budget::MAX_KERNEL_TAPS`].
    TooLong {
        preset: FilterPreset,
        ratio: RateRatio,
        taps: usize,
    },
    /// Not ported yet.
    NotImplemented(FilterPreset),
    /// The recipe failed (for example its phase transform).
    Design(String),
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ratio {
                preset,
                ratio,
                need,
            } => write!(
                f,
                "filter preset {} does not run at {ratio}: it needs {need}",
                preset.name()
            ),
            Self::TooLong {
                preset,
                ratio,
                taps,
            } => write!(
                f,
                "filter preset {} does not run at {ratio}: its kernel would have {taps} taps, \
                 more than {}; use a lower ratio or a shorter preset",
                preset.name(),
                design::budget::MAX_KERNEL_TAPS
            ),
            Self::NotImplemented(p) => write!(f, "filter preset {} is not available yet", p.name()),
            Self::Design(e) => write!(f, "filter preset design failed: {e}"),
        }
    }
}

impl std::error::Error for PresetError {}

pub(crate) use crate::design::upsampler::preset_error;

/// `ratio` as a [`RateRatio`], refused outside 2..=64
/// before it is narrowed (a `usize` above `u32::MAX` must not wrap into
/// range).
pub(crate) fn integer_ratio(p: FilterPreset, ratio: usize) -> Result<RateRatio, PresetError> {
    if !(2..=design::budget::MAX_RATIO).contains(&ratio) {
        return Err(PresetError::Ratio {
            preset: p,
            ratio: RateRatio::integer(ratio as u32),
            need: "a ratio from 2 to 64",
        });
    }
    Ok(RateRatio::integer(ratio as u32))
}

/// Whether `p` runs at the integer `ratio` (2..=64): the runtime
/// constructors' guard ([`design::rates::check`] on its design).
pub(crate) fn check_ratio(p: FilterPreset, ratio: usize) -> Result<(), PresetError> {
    design::rates::check(&catalogue::design(p), integer_ratio(p, ratio)?)
        .map_err(|e| preset_error(p, e))
}

/// A kernel at the output rate (a cascade's composed kernel).
#[derive(Clone, Debug, PartialEq)]
pub struct PresetKernel {
    /// Taps at the output rate (see the module docs for the gain).
    pub kernel: Vec<f64>,
    /// The kernel's own delay in output frames: the centre for linear
    /// phase, the position of the largest tap otherwise.
    pub delay: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_rules() {
        assert!(check_ratio(FilterPreset::GaussianLong, 3).is_ok());
        assert!(check_ratio(FilterPreset::GaussianLong, 1).is_err());
        assert!(check_ratio(FilterPreset::CascadeFir, 8).is_ok());
        assert!(check_ratio(FilterPreset::CascadeFir, 12).is_ok());
        assert!(check_ratio(FilterPreset::CascadeFir, 3).is_err());
        assert!(check_ratio(FilterPreset::CascadeFir, 10).is_err());
        assert!(check_ratio(FilterPreset::BlockFft, 16).is_ok());
        assert!(check_ratio(FilterPreset::BlockFft, 6).is_err());
    }

    #[test]
    fn ratio_rules_reach_x64() {
        for p in [
            FilterPreset::GaussianLong,
            FilterPreset::CascadeFir,
            FilterPreset::BlockFftLarge,
            FilterPreset::Nos,
            FilterPreset::CatmullRom,
            FilterPreset::Lanczos32767,
            FilterPreset::FlattopHuge,
        ] {
            assert!(check_ratio(p, 64).is_ok(), "{p:?}");
            let err = check_ratio(p, 65).unwrap_err().to_string();
            assert!(err.contains("from 2 to 64"), "{err}");
        }
        assert!(check_ratio(FilterPreset::CascadeFir, 48).is_ok());
        assert!(check_ratio(FilterPreset::BlockFft, 48).is_err());
        // The longest kernel, 131 071 taps per input sample, fits the tap
        // limit up to x64 (8 388 544 taps).
        assert!(check_ratio(FilterPreset::Lanczos65535, 64).is_ok());
        // What a longer one would be told.
        let err = PresetError::TooLong {
            preset: FilterPreset::Lanczos65535,
            ratio: RateRatio::integer(65),
            taps: 8_519_615,
        };
        assert_eq!(
            err.to_string(),
            "filter preset lanczos65535 does not run at x65: its kernel would have 8519615 \
             taps, more than 8388608; use a lower ratio or a shorter preset"
        );
    }

    /// A ratio above `u32::MAX` is refused before it is narrowed: it must
    /// not wrap into range (`2^32 + 2` would read as x2).
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn huge_ratio_is_refused_before_narrowing() {
        let huge = (1usize << 32) + 2;
        for &p in FilterPreset::ALL {
            let e = check_ratio(p, huge).unwrap_err();
            assert!(
                matches!(
                    e,
                    PresetError::Ratio {
                        need: "a ratio from 2 to 64",
                        ..
                    }
                ),
                "{p:?}: {e:?}"
            );
        }
        assert!(matches!(
            block_fft::build(FilterPreset::BlockFftLarge, 44_100, huge, 2, 64),
            Err(PresetError::Ratio { .. })
        ));
    }

    /// Every preset runs at x64 (44.1 kHz -> 2.8224 MHz) or says why not
    /// from its ratio rules, before anything is designed.
    #[test]
    fn every_preset_builds_at_x64_or_refuses() {
        let ratio = 64;
        let max_block = 256;
        let input = vec![0.25; max_block];
        let mut out = vec![0.0; max_block * ratio];
        let cx = design::BuildContext {
            source_rate: 44_100,
            ratio: RateRatio::integer(ratio as u32),
            channels: 1,
            max_block,
            pipelines: design::budget::RATIONAL_SWAP_PIPELINES,
            output: audivane_protocol::OutputConfig::default(),
            workspace: design::budget::DESIGN_WORKSPACE_FLOOR,
        };
        for &p in FilterPreset::ALL {
            let built = design::upsampler(&design::Named::preset(p), &cx, 0.0);
            if let Err(e) = check_ratio(p, ratio) {
                assert!(matches!(
                    e,
                    PresetError::Ratio { .. } | PresetError::TooLong { .. }
                ));
                assert!(built.is_err(), "{p:?}");
                continue;
            }
            let mut rt = built
                .unwrap_or_else(|e| panic!("{p:?} at x{ratio}: {e}"))
                .expect("an upsampler");
            assert_eq!(rt.ratio(), RateRatio::integer(ratio as u32));
            rt.process(&[&input], &mut [&mut out]);
            assert!(out.iter().all(|v| v.is_finite()), "{p:?}");
        }
    }
}
