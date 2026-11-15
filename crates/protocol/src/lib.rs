//! Types shared by the engine, the CLI and (later) the UI.
//!
//! Everything here is plain data with serde derives. JSON field names are
//! snake_case and enums are tagged with lowercase names, so the UI can
//! produce them by hand.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub mod ipc;

/// Graph (source) rates the engine can run at. `rate.allowed_rates` must be
/// a subset of this list.
pub const ALLOWED_RATES: &[u32] = &[
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// Highest DAC rate the engine ever asks ALSA for.
pub const MAX_OUTPUT_RATE: u32 = 768_000;

// ---------------------------------------------------------------- EQ

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FilterType {
    /// Peaking EQ (RBJ "peakingEQ"), uses `gain_db` and `q`.
    Peaking,
    /// Low shelf (RBJ), `q` is the shelf Q (0.7071 = S of 1).
    LowShelf,
    /// High shelf (RBJ), `q` is the shelf Q.
    HighShelf,
    /// 2nd-order low pass, `q` (0.7071 = Butterworth).
    LowPass,
    /// 2nd-order high pass, `q`.
    HighPass,
    /// Band pass, constant 0 dB peak gain.
    BandPass,
    Notch,
    AllPass,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Band {
    pub kind: FilterType,
    pub freq_hz: f64,
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default = "default_q")]
    pub q: f64,
    #[serde(default = "yes")]
    pub enabled: bool,
}

pub const EQ_PROFILE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ChannelEq {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub preamp_db: f64,
    #[serde(default)]
    pub bands: Vec<Band>,
}

impl Default for ChannelEq {
    fn default() -> Self {
        Self {
            enabled: true,
            preamp_db: 0.0,
            bands: Vec::new(),
        }
    }
}

impl ChannelEq {
    pub fn is_active(&self) -> bool {
        self.enabled && (self.preamp_db != 0.0 || self.bands.iter().any(|b| b.enabled))
    }
}

#[derive(Deserialize)]
struct RawEqProfile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    preamp_db: f64,
    #[serde(default = "yes")]
    linked_enabled: bool,
    #[serde(default)]
    bands: Vec<Band>,
    #[serde(default)]
    left: Option<ChannelEq>,
    #[serde(default)]
    right: Option<ChannelEq>,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(from = "RawEqProfile")]
pub struct EqProfile {
    #[serde(default)]
    pub name: String,
    /// 2 = layered (this layout). Absent/0/1 = legacy (A), migrated on read.
    pub version: u32,
    #[serde(default)]
    pub preamp_db: f64,
    #[serde(default = "yes")]
    pub linked_enabled: bool,
    #[serde(default)]
    pub bands: Vec<Band>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<ChannelEq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<ChannelEq>,
}

impl Default for EqProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: EQ_PROFILE_VERSION,
            preamp_db: 0.0,
            linked_enabled: true,
            bands: Vec::new(),
            left: None,
            right: None,
        }
    }
}

impl From<RawEqProfile> for EqProfile {
    fn from(raw: RawEqProfile) -> Self {
        if raw.version < EQ_PROFILE_VERSION {
            match raw.right {
                None => EqProfile {
                    name: raw.name,
                    version: EQ_PROFILE_VERSION,
                    preamp_db: raw.preamp_db,
                    linked_enabled: raw.linked_enabled,
                    bands: raw.bands,
                    left: raw.left,
                    right: None,
                },
                Some(right) => EqProfile {
                    name: raw.name,
                    version: EQ_PROFILE_VERSION,
                    preamp_db: 0.0,
                    linked_enabled: true,
                    bands: Vec::new(),
                    left: Some(ChannelEq {
                        enabled: true,
                        preamp_db: raw.preamp_db,
                        bands: raw.bands,
                    }),
                    right: Some(right),
                },
            }
        } else {
            EqProfile {
                name: raw.name,
                version: raw.version,
                preamp_db: raw.preamp_db,
                linked_enabled: raw.linked_enabled,
                bands: raw.bands,
                left: raw.left,
                right: raw.right,
            }
        }
    }
}

impl Serialize for EqProfile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a> {
            name: &'a str,
            version: u32,
            preamp_db: f64,
            linked_enabled: bool,
            bands: &'a [Band],
            #[serde(skip_serializing_if = "Option::is_none")]
            left: Option<&'a ChannelEq>,
            #[serde(skip_serializing_if = "Option::is_none")]
            right: Option<&'a ChannelEq>,
        }
        Out {
            name: &self.name,
            version: EQ_PROFILE_VERSION,
            preamp_db: self.preamp_db,
            linked_enabled: self.linked_enabled,
            bands: &self.bands,
            left: self.left.as_ref(),
            right: self.right.as_ref(),
        }
        .serialize(serializer)
    }
}

impl EqProfile {
    pub fn is_active(&self) -> bool {
        self.preamp_db != 0.0
            || (self.linked_enabled && self.bands.iter().any(|b| b.enabled))
            || self.left.as_ref().is_some_and(ChannelEq::is_active)
            || self.right.as_ref().is_some_and(ChannelEq::is_active)
    }
}

// ---------------------------------------------------------------- Convolution

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConvolutionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// WAV impulse response: mono (applied to every channel) or one channel
    /// per output channel. Resampled offline when its rate differs.
    pub ir_path: PathBuf,
    #[serde(default)]
    pub gain_db: f64,
}

// ---------------------------------------------------------------- Upsampling

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum FilterPhase {
    #[default]
    Linear,
    /// Same magnitude as the linear-phase design, zeros reflected inside the
    /// unit circle. Needs Kaiser beta 2..=20.
    Minimum,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum RateTarget {
    /// Integer ratio of the source rate.
    Ratio(u32),
    /// Highest power-of-two multiple of the source rate that is <= this
    /// rate (e.g. 768000 → 44.1k·16 = 705600, 48k·16 = 768000).
    MaxRate(u32),
    /// No upsampling.
    #[default]
    Off,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum CutoffMode {
    /// Use `UpsamplerConfig::cutoff` as a fraction of the source Nyquist.
    #[default]
    Fraction,
    /// Place the stopband edge at the source Nyquist: every frequency from
    /// the source to the output Nyquist stays at or below the Kaiser
    /// stopband target. Needs Kaiser beta 5..=24 and enough taps for the
    /// ratio (the daemon rejects other designs); see
    /// `audivane_dsp_core::resample::nyquist_stop_cutoff`.
    NyquistStop,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UpsamplerConfig {
    #[serde(default)]
    pub target: RateTarget,
    /// Total kernel length at the *output* rate.
    #[serde(default = "default_taps")]
    pub taps: usize,
    #[serde(default)]
    pub phase: FilterPhase,
    /// Kaiser window beta. ~14 gives > 140 dB stopband.
    #[serde(default = "default_beta")]
    pub kaiser_beta: f64,
    /// Cutoff as a fraction of the *input* Nyquist (1.0 = Fs_in/2).
    /// Values < 1.0 give an apodizing filter.
    #[serde(default = "default_cutoff")]
    pub cutoff: f64,
    /// How the cutoff frequency is chosen.
    #[serde(default)]
    pub cutoff_mode: CutoffMode,
}

impl Default for UpsamplerConfig {
    fn default() -> Self {
        Self {
            target: RateTarget::Off,
            taps: default_taps(),
            phase: FilterPhase::Linear,
            kaiser_beta: default_beta(),
            cutoff: default_cutoff(),
            cutoff_mode: CutoffMode::Fraction,
        }
    }
}

// ---------------------------------------------------------------- Output

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum DitherKind {
    None,
    /// Triangular PDF, ±1 LSB.
    #[default]
    Tpdf,
    /// TPDF plus error-feedback noise shaping.
    Shaped,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum SampleFormat {
    S16,
    /// 24-bit in a 32-bit container (ALSA S24_LE).
    S24,
    /// Packed 3-byte 24-bit (ALSA S24_3LE).
    S24_3,
    S32,
}

impl SampleFormat {
    /// Number of significant bits the DAC receives.
    pub fn bits(self) -> u32 {
        match self {
            SampleFormat::S16 => 16,
            SampleFormat::S24 | SampleFormat::S24_3 => 24,
            SampleFormat::S32 => 32,
        }
    }

    /// Short display name ("S16", "S24", "S24_3", "S32").
    pub fn name(self) -> &'static str {
        match self {
            SampleFormat::S16 => "S16",
            SampleFormat::S24 => "S24",
            SampleFormat::S24_3 => "S24_3",
            SampleFormat::S32 => "S32",
        }
    }
}

/// Bit depth dither actually targets: the configured `output.bits`, capped at
/// the container width of the negotiated ALSA format (dithering to 24 bits
/// into an S16 device would leave the packer to truncate the low 8 bits).
pub fn effective_dither_bits(configured: u32, format: Option<SampleFormat>) -> u32 {
    match format {
        Some(f) => configured.min(f.bits()),
        None => configured,
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OutputConfig {
    /// Effective bit depth that dither targets (<= format bits). Many
    /// "32-bit" DACs resolve ~24 bits, so 24 is a sane default.
    #[serde(default = "default_bits")]
    pub bits: u32,
    #[serde(default)]
    pub dither: DitherKind,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            bits: default_bits(),
            dither: DitherKind::Tpdf,
        }
    }
}

// ---------------------------------------------------------------- Chain

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ChainConfig {
    #[serde(default)]
    pub eq: Option<EqProfile>,
    #[serde(default, deserialize_with = "deserialize_conv_list")]
    pub convolution: Vec<ConvolutionConfig>,
    #[serde(default)]
    pub upsampler: UpsamplerConfig,
    #[serde(default)]
    pub output: OutputConfig,
}

/// Deserialize a convolution list from `null`, a single [`ConvolutionConfig`]
/// object, or an array of [`ConvolutionConfig`] objects.
pub fn deserialize_conv_list<'de, D>(deserializer: D) -> Result<Vec<ConvolutionConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ConvListVisitor;

    impl<'de> serde::de::Visitor<'de> for ConvListVisitor {
        type Value = Vec<ConvolutionConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("null, a convolution config object, or an array of convolution configs")
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_some<D2: serde::Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> Result<Self::Value, D2::Error> {
            deserializer.deserialize_any(self)
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(item) = seq.next_element()? {
                out.push(item);
            }
            Ok(out)
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            let one =
                ConvolutionConfig::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![one])
        }
    }

    deserializer.deserialize_any(ConvListVisitor)
}

impl ChainConfig {
    /// True when no stage changes the signal (EQ/convolution absent or
    /// disabled, no upsampling). The engine then uses the bit-exact path.
    pub fn is_passthrough(&self) -> bool {
        let eq_active = self.eq.as_ref().is_some_and(EqProfile::is_active);
        let conv_active = self.convolution.iter().any(|c| c.enabled);
        !eq_active && !conv_active && matches!(self.upsampler.target, RateTarget::Off)
    }
}

/// Resolve an upsampling target into an integer ratio for a source rate.
pub fn resolve_ratio(target: RateTarget, source_rate: u32) -> u32 {
    match target {
        RateTarget::Off => 1,
        RateTarget::Ratio(r) => r.max(1),
        RateTarget::MaxRate(max) => {
            let mut r = 1u32;
            while u64::from(source_rate) * u64::from(r) * 2 <= u64::from(max) {
                r *= 2;
            }
            r
        }
    }
}

fn yes() -> bool {
    true
}
fn default_q() -> f64 {
    std::f64::consts::FRAC_1_SQRT_2
}
fn default_taps() -> usize {
    65_536
}
fn default_beta() -> f64 {
    14.0
}
fn default_cutoff() -> f64 {
    1.0
}
fn default_bits() -> u32 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_resolution() {
        assert_eq!(resolve_ratio(RateTarget::MaxRate(768_000), 44_100), 16);
        assert_eq!(resolve_ratio(RateTarget::MaxRate(384_000), 96_000), 4);
        assert_eq!(resolve_ratio(RateTarget::MaxRate(705_600), 44_100), 16);
        assert_eq!(resolve_ratio(RateTarget::MaxRate(768_000), 48_000), 16);
        assert_eq!(resolve_ratio(RateTarget::Off, 48_000), 1);
    }

    #[test]
    fn empty_chain_is_passthrough() {
        assert!(ChainConfig::default().is_passthrough());
    }

    #[test]
    fn eq_profile_legacy_migration_none_and_right_some() {
        // Legacy profile without right section stays linked in v2.
        let old_linked = r#"{"name":"old","preamp_db":-3.0,"bands":[{"kind":"peaking","freq_hz":1000.0,"gain_db":3.0,"q":1.0,"enabled":true}]}"#;
        let eq: EqProfile = serde_json::from_str(old_linked).unwrap();
        assert_eq!(eq.version, EQ_PROFILE_VERSION);
        assert!(eq.linked_enabled);
        assert_eq!(eq.preamp_db, -3.0);
        assert_eq!(eq.bands.len(), 1);
        assert_eq!(eq.left, None);
        assert_eq!(eq.right, None);
        assert!(eq.is_active());
        let reserialized = serde_json::to_string(&eq).unwrap();
        assert!(reserialized.contains("\"version\":2"));
        assert!(!reserialized.contains("\"left\""));
        assert!(!reserialized.contains("\"right\""));

        // Legacy split profile (version absent or 1, right: Some) moves left into .left.
        let old_split = r#"{
            "name":"old-split",
            "version":1,
            "preamp_db":-2.5,
            "bands":[{"kind":"peaking","freq_hz":200.0,"gain_db":2.0,"q":1.0,"enabled":true}],
            "right":{"preamp_db":-1.0,"bands":[{"kind":"peaking","freq_hz":500.0,"gain_db":4.0,"q":1.0,"enabled":true}]}
        }"#;
        let migrated: EqProfile = serde_json::from_str(old_split).unwrap();
        assert_eq!(migrated.version, EQ_PROFILE_VERSION);
        assert!(migrated.linked_enabled);
        assert_eq!(migrated.preamp_db, 0.0);
        assert!(migrated.bands.is_empty());
        let left = migrated.left.as_ref().expect("left populated from legacy");
        assert!(left.enabled);
        assert_eq!(left.preamp_db, -2.5);
        assert_eq!(left.bands.len(), 1);
        assert_eq!(left.bands[0].freq_hz, 200.0);
        let right = migrated.right.as_ref().expect("right preserved");
        assert!(right.enabled);
        assert_eq!(right.preamp_db, -1.0);
        assert_eq!(right.bands.len(), 1);
        assert_eq!(right.bands[0].freq_hz, 500.0);

        let rt_json = serde_json::to_string(&migrated).unwrap();
        let rt: EqProfile = serde_json::from_str(&rt_json).unwrap();
        assert_eq!(rt, migrated);
    }

    #[test]
    fn eq_profile_v2_roundtrip_and_is_active() {
        let layered = EqProfile {
            name: "layered".into(),
            version: 0, // Serialize must always emit version 2
            preamp_db: 0.0,
            linked_enabled: true,
            bands: vec![Band {
                kind: FilterType::Peaking,
                freq_hz: 100.0,
                gain_db: -2.0,
                q: 2.0,
                enabled: true,
            }],
            left: None,
            right: Some(ChannelEq {
                enabled: true,
                preamp_db: -2.0,
                bands: vec![Band {
                    kind: FilterType::Peaking,
                    freq_hz: 500.0,
                    gain_db: 2.0,
                    q: 1.0,
                    enabled: true,
                }],
            }),
        };
        assert!(layered.is_active());
        let json = serde_json::to_string(&layered).unwrap();
        assert!(json.contains("\"version\":2"));
        assert!(!json.contains("\"left\""));
        assert!(json.contains("\"right\""));
        let back: EqProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, EQ_PROFILE_VERSION);
        assert_eq!(back.bands.len(), 1);
        assert_eq!(back.left, None);
        assert_eq!(back.right, layered.right);

        // Disabled linked section and disabled ChannelEq sections make profile inactive
        // unless global preamp_db is non-zero.
        let disabled_sections = EqProfile {
            name: "disabled".into(),
            version: EQ_PROFILE_VERSION,
            preamp_db: 0.0,
            linked_enabled: false,
            bands: layered.bands.clone(),
            left: Some(ChannelEq {
                enabled: false,
                preamp_db: -3.0,
                bands: layered.bands.clone(),
            }),
            right: Some(ChannelEq {
                enabled: false,
                preamp_db: 1.5,
                bands: layered.bands,
            }),
        };
        assert!(!disabled_sections.is_active());

        let preamp_only = EqProfile {
            preamp_db: -1.0,
            ..disabled_sections
        };
        assert!(preamp_only.is_active());
    }

    #[test]
    fn convolution_config_deserializes_null_object_and_array() {
        let missing: ChainConfig = serde_json::from_str(r#"{}"#).unwrap();
        assert!(missing.convolution.is_empty());
        assert!(missing.is_passthrough());

        let null: ChainConfig = serde_json::from_str(r#"{"convolution":null}"#).unwrap();
        assert!(null.convolution.is_empty());

        let single: ChainConfig = serde_json::from_str(
            r#"{"convolution":{"enabled":true,"ir_path":"/tmp/ir.wav","gain_db":-1.5}}"#,
        )
        .unwrap();
        assert_eq!(single.convolution.len(), 1);
        assert_eq!(single.convolution[0].name, None);
        assert_eq!(single.convolution[0].ir_path, PathBuf::from("/tmp/ir.wav"));
        assert_eq!(single.convolution[0].gain_db, -1.5);
        assert!(!single.is_passthrough());

        let many: ChainConfig = serde_json::from_str(
            r#"{"convolution":[{"name":"Room","enabled":true,"ir_path":"/tmp/a.wav","gain_db":0.0},{"enabled":false,"ir_path":"/tmp/b.wav","gain_db":1.0}]}"#,
        )
        .unwrap();
        assert_eq!(many.convolution.len(), 2);
        assert_eq!(many.convolution[0].name.as_deref(), Some("Room"));
        assert!(!many.convolution[1].enabled);
        assert!(!many.is_passthrough());

        let all_disabled = ChainConfig {
            convolution: vec![ConvolutionConfig {
                name: None,
                enabled: false,
                ir_path: PathBuf::from("/tmp/x.wav"),
                gain_db: 0.0,
            }],
            ..Default::default()
        };
        assert!(all_disabled.is_passthrough());
    }
}
