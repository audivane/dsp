//! The filter fingerprint matrix, shared by the dsp-core harness
//! (`tests/filter_fingerprints.rs`)
//! and the daemon's validation test (`crates/daemon/tests/filter_validation.rs`).
//!
//! The matrix is generated before any check, so refusals are cases too.

#![allow(dead_code)] // each test binary uses part of it

use audivane_dsp_core::design::{FilterDesign, catalogue};
use audivane_dsp_core::presets::FilterPreset;
use audivane_protocol::{
    ALLOWED_RATES, BandSpec, CutoffMode, DitherKind, FilterErrorBudget, FilterPhase,
    HIGH_OUTPUT_RATES, OutputConfig, RateRatio, TapsUnit, UpsamplerConfig, WindowKind,
};

/// Largest rational ratio term the engine opens a device for (io-pw
/// `MAX_RATIONAL_TERM`).
pub const MAX_RATIONAL_TERM: u32 = 4096;
/// Highest output rate.
pub const MAX_OUTPUT_RATE: u64 = audivane_protocol::MAX_OUTPUT_RATE as u64;
/// Integer ratios of tier A at 44.1 kHz (x6, x12 and x48 are mixed-factor
/// cascades).
pub const TIER_A_RATIOS: &[u32] = &[2, 3, 4, 5, 6, 8, 12, 16, 28, 32, 48, 64];
/// Integer ratios of tier A for the rate-dependent presets, at every
/// source rate.
pub const TIER_A_RATE_RATIOS: &[u32] = &[2, 3, 4, 8, 16, 32, 64];
/// Rational rate pairs of tier A for every single-kernel preset.
pub const TIER_A_PAIRS: &[(u32, u32)] = &[
    (44_100, 48_000),
    (48_000, 44_100),
    (96_000, 48_000),
    (192_000, 44_100),
    (44_100, 96_000),
    (88_200, 48_000),
    (176_400, 48_000),
];
/// The presets whose kernel depends on the source rate (compact and
/// constant-time lengths); checked against the builders.
pub const RATE_DEPENDENT: &[FilterPreset] = &[
    FilterPreset::GaussianCompact,
    FilterPreset::GaussianHuge,
    FilterPreset::GaussianHugeApod,
    FilterPreset::FlattopCompact,
    FilterPreset::FlattopHuge,
    FilterPreset::KaiserCompact,
];
/// Tier A's rational cases that pass with 2 pipelines and are refused with
/// 4 (picked from tier B's `cost4` rows): (preset, input, output).
pub const COST4_PICKS: &[(&str, u32, u32)] = &[
    ("kaiser_steep_mp", 48_000, 352_800),
    ("chebyshev_long_ip", 96_000, 352_800),
    ("chebyshev_mp", 48_000, 2_822_400),
    ("chebyshev_short", 96_000, 2_822_400),
    ("chebyshev_long", 96_000, 1_411_200),
    ("flattop_compact", 48_000, 1_411_200),
    ("halfband_large", 48_000, 1_411_200),
    ("gaussian_nyquist", 96_000, 705_600),
    ("gaussian_long", 88_200, 768_000),
    ("nuttall", 176_400, 3_072_000),
    ("lanczos511", 96_000, 352_800),
    ("lanczos2047", 88_200, 96_000),
    ("flattop_xlong", 88_200, 44_100),
];
/// Headroom of the chain cases, dB.
pub const CHAIN_HEADROOM_DB: f64 = 3.5;

#[derive(Clone, Debug)]
pub enum Design {
    Preset(FilterPreset),
    Custom(&'static str, Box<UpsamplerConfig>),
    /// A design built through the chain (`Chain::try_new`) with
    /// [`CHAIN_HEADROOM_DB`] of headroom; at a unity ratio the chain must
    /// build without an upsampler.
    Chain(Box<Design>),
}

impl Design {
    pub fn name(&self) -> String {
        match self {
            Design::Preset(p) => p.name().to_string(),
            Design::Custom(name, _) => format!("custom:{name}"),
            Design::Chain(inner) => format!("chain:{}", inner.name()),
        }
    }

    /// The upsampler config this design gives (target and headroom left
    /// as they are).
    pub fn config(&self) -> UpsamplerConfig {
        match self {
            Design::Preset(p) => UpsamplerConfig {
                preset: Some(p.name().to_string()),
                ..UpsamplerConfig::default()
            },
            Design::Custom(_, cfg) => (**cfg).clone(),
            Design::Chain(inner) => inner.config(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Case {
    pub design: Design,
    pub source_rate: u32,
    pub ratio: RateRatio,
}

impl Case {
    pub fn name(&self) -> String {
        self.design.name()
    }

    pub fn key(&self) -> String {
        format!(
            "{}\t{}\t{}/{}",
            self.name(),
            self.source_rate,
            self.ratio.up,
            self.ratio.down
        )
    }

    pub fn preset(p: FilterPreset, source_rate: u32, ratio: RateRatio) -> Self {
        Self {
            design: Design::Preset(p),
            source_rate,
            ratio,
        }
    }

    /// The output rate.
    pub fn output_rate(&self) -> u32 {
        self.ratio
            .output_rate(self.source_rate)
            .expect("an output rate")
    }
}

pub fn is_kernel(p: FilterPreset) -> bool {
    matches!(catalogue::design(p), FilterDesign::Kernel(_))
}

fn integer_ratios(source_rate: u32, ratios: impl IntoIterator<Item = u32>) -> Vec<RateRatio> {
    ratios
        .into_iter()
        .filter(|&r| u64::from(source_rate) * u64::from(r) <= MAX_OUTPUT_RATE)
        .map(RateRatio::integer)
        .collect()
}

/// Keep the first case of every key, in order.
fn dedup(cases: Vec<Case>) -> Vec<Case> {
    let mut seen = std::collections::HashSet::new();
    cases.into_iter().filter(|c| seen.insert(c.key())).collect()
}

/// Tier A (the default run).
pub fn tier_a() -> Vec<Case> {
    let mut cases = Vec::new();
    for &p in FilterPreset::ALL {
        for r in integer_ratios(44_100, TIER_A_RATIOS.iter().copied()) {
            cases.push(Case::preset(p, 44_100, r));
        }
    }
    for &p in RATE_DEPENDENT {
        for &rate in ALLOWED_RATES {
            for r in integer_ratios(rate, TIER_A_RATE_RATIOS.iter().copied()) {
                cases.push(Case::preset(p, rate, r));
            }
        }
    }
    for &p in FilterPreset::ALL.iter().filter(|&&p| is_kernel(p)) {
        for &(input, output) in TIER_A_PAIRS {
            cases.push(Case::preset(p, input, RateRatio::from_rates(input, output)));
        }
    }
    for &(name, input, output) in COST4_PICKS {
        let p = FilterPreset::from_name(name).expect("a preset name");
        cases.push(Case::preset(p, input, RateRatio::from_rates(input, output)));
    }
    cases.extend(custom_cases());
    cases.extend(chain_cases());
    dedup(cases)
}

/// The rational pairs of tier B: every input in `ALLOWED_RATES` to every
/// output in `ALLOWED_RATES` and `HIGH_OUTPUT_RATES` with a non-integer
/// ratio whose terms are within the engine's limit.
pub fn rational_pairs() -> Vec<(u32, RateRatio)> {
    let outputs: Vec<u32> = ALLOWED_RATES
        .iter()
        .chain(HIGH_OUTPUT_RATES)
        .copied()
        .collect();
    let mut pairs = Vec::new();
    for &input in ALLOWED_RATES {
        for &output in &outputs {
            let r = RateRatio::from_rates(input, output);
            if r.is_integer() || r.up > MAX_RATIONAL_TERM || r.down > MAX_RATIONAL_TERM {
                continue;
            }
            pairs.push((input, r));
        }
    }
    pairs
}

/// The output rates a rate ladder can ask for: every graph rate times 1 to
/// 16 up to 768 kHz, and the high rates (io-pw's `MASK_OUTPUT_RATES`, which
/// the daemon's `ladder_output_rates_are_the_graph_rate_multiples` pins to this formula).
pub fn mask_output_rates() -> Vec<u32> {
    let mut v: Vec<u32> = ALLOWED_RATES
        .iter()
        .flat_map(|&r| (1..=16).map(move |m| r * m))
        .filter(|&r| r <= 768_000)
        .chain(HIGH_OUTPUT_RATES.iter().copied())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The rational pairs a rate ladder can ask for that [`rational_pairs`]
/// does not cover: every input in `ALLOWED_RATES` to every ladder-only
/// output rate of [`mask_output_rates`] (132.3 kHz, 144 kHz, 480 kHz, ...),
/// within the engine's term limit.
pub fn ladder_pairs() -> Vec<(u32, RateRatio)> {
    let tier_b: std::collections::HashSet<(u32, RateRatio)> =
        rational_pairs().into_iter().collect();
    let mut pairs = Vec::new();
    for &input in ALLOWED_RATES {
        for output in mask_output_rates() {
            let r = RateRatio::from_rates(input, output);
            if r.is_integer() || r.up > MAX_RATIONAL_TERM || r.down > MAX_RATIONAL_TERM {
                continue;
            }
            if !tier_b.contains(&(input, r)) {
                pairs.push((input, r));
            }
        }
    }
    pairs
}

/// The ladder tier (`--ignored`): every single-kernel preset at every
/// [`ladder_pairs`] pair (tier B has every integer pair and the other
/// rational ones). Not fingerprinted: it checks that what the rate mask
/// accepts builds, and gates every construction.
pub fn tier_ladder() -> Vec<Case> {
    let pairs = ladder_pairs();
    let mut cases = Vec::new();
    for &p in FilterPreset::ALL.iter().filter(|&&p| is_kernel(p)) {
        for &(input, r) in &pairs {
            cases.push(Case::preset(p, input, r));
        }
    }
    dedup(cases)
}

/// Tier B (`--ignored`): every preset at every integer ratio and every
/// single-kernel preset at every rational pair.
pub fn tier_b() -> Vec<Case> {
    let mut cases = Vec::new();
    for &p in FilterPreset::ALL {
        for &rate in ALLOWED_RATES {
            for r in integer_ratios(rate, 2..=audivane_protocol::MAX_INTEGER_RATIO) {
                cases.push(Case::preset(p, rate, r));
            }
        }
    }
    let pairs = rational_pairs();
    for &p in FilterPreset::ALL.iter().filter(|&&p| is_kernel(p)) {
        for &(input, r) in &pairs {
            cases.push(Case::preset(p, input, r));
        }
    }
    dedup(cases)
}

/// The custom designs: the Recommended card for three outputs, the Custom
/// view's quick designs, and the design features the presets do not have
/// (reflected minimum and intermediate phase, band and "stop at Nyquist"
/// lengths, Gaussian band design, Tukey with kept samples, the even-length
/// delay conventions).
pub fn custom_designs() -> Vec<(&'static str, UpsamplerConfig, &'static [u32])> {
    use audivane_dsp_core::resample::ManualConfig;
    let base = ManualConfig {
        kaiser_beta: 14.0,
        cutoff: 1.0,
        ..ManualConfig::default()
    };
    let recommended = ManualConfig {
        window: WindowKind::Kaiser,
        cutoff_mode: CutoffMode::Band,
        band: BandSpec {
            passband_end: 0.9,
            stopband_start: 1.0,
            attenuation_db: 150.0,
        },
        auto_taps: Some(FilterErrorBudget::MatchOutput),
        phase: FilterPhase::Minimum,
        taps_unit: TapsUnit::Output,
        ..base.clone()
    };
    let output = |bits, dither| OutputConfig {
        bits,
        dither,
        headroom_db: 0.0,
    };
    let up = |m: ManualConfig| m.into_upsampler_config().unwrap();
    vec![
        (
            "recommended_s32_tpdf",
            up(recommended.with_output(&output(32, DitherKind::Tpdf))),
            &[4, 16, 64],
        ),
        (
            "recommended_s24_tpdf",
            up(recommended.with_output(&output(24, DitherKind::Tpdf))),
            &[4, 16, 64],
        ),
        (
            "recommended_s16_none",
            up(recommended.with_output(&output(16, DitherKind::None))),
            &[4, 16, 64],
        ),
        (
            "music",
            up(ManualConfig {
                taps: 262_144,
                ..base.clone()
            }),
            &[4, 16],
        ),
        (
            "low_latency",
            up(ManualConfig {
                taps: 16_384,
                phase: FilterPhase::Minimum,
                ..base.clone()
            }),
            &[4, 16],
        ),
        (
            "intermediate_mix05",
            up(ManualConfig {
                taps: 32_768,
                phase: FilterPhase::Intermediate,
                phase_mix: 0.5,
                ..base.clone()
            }),
            &[4, 16],
        ),
        (
            "gaussian_band",
            up(ManualConfig {
                window: WindowKind::Gaussian,
                cutoff_mode: CutoffMode::Band,
                band: BandSpec::default(),
                ..base.clone()
            }),
            &[4, 16],
        ),
        (
            "nyquist_stop_kaiser",
            up(ManualConfig {
                taps: 65_536,
                cutoff_mode: CutoffMode::NyquistStop,
                ..base.clone()
            }),
            &[4, 16],
        ),
        (
            "tukey_preserve",
            up(ManualConfig {
                taps: 65_535,
                window: WindowKind::Tukey,
                taper: 0.25,
                preserve_samples: true,
                ..base.clone()
            }),
            &[4, 16],
        ),
        (
            "even_linear",
            up(ManualConfig {
                taps: 64,
                taps_unit: TapsUnit::Input,
                cutoff: 0.95,
                ..base.clone()
            }),
            &[8],
        ),
    ]
}

fn custom_design(name: &str) -> Design {
    let (name, cfg, _) = custom_designs()
        .into_iter()
        .find(|(n, ..)| *n == name)
        .expect("a custom design");
    Design::Custom(name, Box::new(cfg))
}

/// Every custom design at 44.1 kHz times its integer ratios and to 48 kHz.
pub fn custom_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for (name, cfg, ratios) in custom_designs() {
        let rational = RateRatio::from_rates(44_100, 48_000);
        for r in ratios
            .iter()
            .map(|&r| RateRatio::integer(r))
            .chain([rational])
        {
            cases.push(Case {
                design: Design::Custom(name, Box::new(cfg.clone())),
                source_rate: 44_100,
                ratio: r,
            });
        }
    }
    cases
}

/// The chain entry point: every preset at a unity ratio (the chain must
/// build without an upsampler) and with headroom at x4, two rational
/// presets and two custom designs with headroom.
pub fn chain_cases() -> Vec<Case> {
    let chain = |d: Design, source_rate, ratio| Case {
        design: Design::Chain(Box::new(d)),
        source_rate,
        ratio,
    };
    let mut cases = Vec::new();
    for &p in FilterPreset::ALL {
        cases.push(chain(Design::Preset(p), 44_100, RateRatio::ONE));
        cases.push(chain(Design::Preset(p), 44_100, RateRatio::integer(4)));
    }
    let to48 = RateRatio::from_rates(44_100, 48_000);
    for p in [FilterPreset::GaussianLong, FilterPreset::ChebyshevMp] {
        cases.push(chain(Design::Preset(p), 44_100, to48));
    }
    cases.push(chain(custom_design("music"), 44_100, RateRatio::integer(4)));
    cases.push(chain(custom_design("even_linear"), 44_100, to48));
    cases.push(chain(custom_design("even_linear"), 44_100, RateRatio::ONE));
    cases
}
