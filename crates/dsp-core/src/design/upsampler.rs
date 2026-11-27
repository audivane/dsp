//! The engine's upsampler from its config: [`Named::of`] resolves the
//! design (a catalogue name or the Custom fields), [`upsampler`] runs
//! [`super::feasibility`], [`super::prepare`] and [`super::build`] and
//! reports a refusal with the message the engine has always given for it
//! (a preset's refusals name the preset).

use super::{BuildContext, DesignError, FilterDesign, catalogue};
use crate::presets::{FilterPreset, PresetError, UpsamplerRuntime};
use audivane_protocol::UpsamplerConfig;

/// A design and the catalogue name it came from, if any.
#[derive(Clone, Debug, PartialEq)]
pub struct Named {
    pub design: FilterDesign,
    pub preset: Option<FilterPreset>,
}

impl Named {
    /// The design `cfg` selects: its catalogue preset, its explicit `design`,
    /// or `recommended` when neither is set. Setting both is refused. An
    /// automatic `match_output` budget stays unresolved: the [`BuildContext`]'s
    /// output resolves it.
    pub fn of(cfg: &UpsamplerConfig) -> Result<Self, DesignError> {
        match (&cfg.preset, &cfg.design) {
            (Some(_), Some(_)) => Err(DesignError::Failed(
                "upsampler: set either preset or design, not both".into(),
            )),
            (Some(name), None) => {
                let design = catalogue::by_name(name).ok_or_else(|| {
                    DesignError::Failed(format!("unknown filter preset {name:?}"))
                })?;
                Ok(Self {
                    design,
                    preset: FilterPreset::from_name(name),
                })
            }
            (None, Some(stored)) => Ok(Self {
                design: stored.design.clone(),
                preset: None,
            }),
            (None, None) => Ok(Self {
                design: catalogue::recommended(),
                preset: None,
            }),
        }
    }

    /// The catalogue design `p`.
    pub fn preset(p: FilterPreset) -> Self {
        Self {
            design: catalogue::design(p),
            preset: Some(p),
        }
    }
}

/// Where a build was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Feasibility,
    Prepare,
    Build,
}

/// A refused upsampler build: the error, where it happened, and the
/// engine's message for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub error: DesignError,
    pub stage: Stage,
    message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

/// `e` as the error of preset `p` (a budget refusal names the preset).
pub fn preset_error(p: FilterPreset, e: DesignError) -> PresetError {
    match e {
        DesignError::Ratio { ratio, need } => PresetError::Ratio {
            preset: p,
            ratio,
            need,
        },
        DesignError::TooLong { ratio, taps } => PresetError::TooLong {
            preset: p,
            ratio,
            taps,
        },
        DesignError::Budget(e) => PresetError::Design(format!("{}: {e}", p.name())),
        DesignError::NotAKernel => PresetError::NotImplemented(p),
        DesignError::Unsupported(e)
        | DesignError::Failed(e)
        | DesignError::Unsafe(e)
        | DesignError::Claim(e) => PresetError::Design(e),
        e @ DesignError::Field(_) => PresetError::Design(e.to_string()),
        e @ (DesignError::Band(_) | DesignError::Phase(_)) => PresetError::Design(e.to_string()),
    }
}

impl Refusal {
    fn new(n: &Named, cx: &BuildContext, stage: Stage, error: DesignError) -> Self {
        let rational = !cx.ratio.is_integer();
        let message = match n.preset {
            // A refusal before a rational kernel is designed carries the
            // advice (and reads as a design failure).
            Some(p) if rational && stage == Stage::Feasibility => PresetError::Design(format!(
                "{}: use a shorter preset or a lower output rate",
                preset_error(p, error.clone())
            ))
            .to_string(),
            Some(p) => preset_error(p, error.clone()).to_string(),
            None => match &error {
                DesignError::Budget(e) => {
                    format!("{e}: use a shorter filter or a lower output rate")
                }
                e => e.to_string(),
            },
        };
        Self {
            error,
            stage,
            message,
        }
    }
}

/// [`super::prepare`] of `n` in `cx` (after [`super::feasibility`] as its
/// own step at a rational ratio), refused with the engine's messages: the
/// kernel and the safety verdict, for the chain build and the offline
/// response measurement alike. Off the real-time thread.
pub fn prepare(n: &Named, cx: &BuildContext) -> Result<super::Prepared, Refusal> {
    let refuse = |stage| move |e| Refusal::new(n, cx, stage, e);
    if !cx.ratio.is_integer() {
        super::feasibility(&n.design, cx).map_err(refuse(Stage::Feasibility))?;
    }
    super::prepare(&n.design, cx).map_err(refuse(Stage::Prepare))
}

/// The upsampler runtime of `n` in `cx`, its output scaled by
/// `-headroom_db`: [`prepare`] and [`super::build`]. `None` at a unity
/// ratio. A Custom design at an integer ratio with no channels (a library
/// caller that only needs the runtime's shape) is not designed: its
/// resolved length and the ratio rules only, as the Custom builder has
/// always done. Off the real-time thread.
pub fn upsampler(
    n: &Named,
    cx: &BuildContext,
    headroom_db: f64,
) -> Result<Option<Box<dyn UpsamplerRuntime>>, Refusal> {
    if cx.ratio.is_unity() {
        return Ok(None);
    }
    if cx.channels == 0
        && cx.ratio.is_integer()
        && let FilterDesign::Kernel(k) = &n.design
        && k.numerics.path == super::ArithmeticPath::Standard
    {
        let refuse = |e| Refusal::new(n, cx, Stage::Prepare, e);
        let k = &n.design.with_output(&cx.output);
        let FilterDesign::Kernel(k) = k else {
            unreachable!("with_output keeps the kind")
        };
        let r = super::paths::standard::recipe(k, cx.source_rate, cx.ratio).map_err(refuse)?;
        super::rates::check(&n.design, cx.ratio).map_err(refuse)?;
        let up = crate::resample::Upsampler::try_new(&r, 0, cx.max_block)
            .map_err(|e| refuse(DesignError::Failed(e.to_string())))?;
        return Ok(Some(Box::new(up)));
    }
    let prepared = prepare(n, cx)?;
    super::build(&prepared, cx, headroom_db).map_err(|e| Refusal::new(n, cx, Stage::Build, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::budget;
    use crate::design::paths::standard::ManualConfig;
    use crate::resample::{UpsamplerDesign, try_design_kernel_for};
    use audivane_protocol::{FilterPhase, RateRatio, RateTarget, TapsUnit};

    fn cx(ratio: RateRatio, channels: usize, pipelines: usize) -> BuildContext {
        BuildContext {
            source_rate: 44_100,
            ratio,
            channels,
            max_block: 256,
            pipelines,
            output: audivane_protocol::OutputConfig::default(),
            workspace: crate::design::budget::DESIGN_WORKSPACE_FLOOR,
        }
    }

    /// The rational budget holds on the design length before the kernel is
    /// made (a `preserve_samples` design is trimmed by a tap: the refusal
    /// names the untrimmed length) and again on the final kernel (a
    /// reflected intermediate kernel grows past what its design length
    /// passed).
    #[test]
    fn the_rational_budget_holds_before_trimming_and_after_growth() {
        let r = RateRatio::from_rates(44_100, 48_000);
        let rational = |taps| ManualConfig {
            target: RateTarget::Rate(48_000),
            taps,
            taps_unit: TapsUnit::Input,
            kaiser_beta: 14.0,
            cutoff: 0.95,
            ..ManualConfig::default()
        };
        // 6554 taps per input on the 160x grid: 1 048 640 taps, over the
        // 1 048 576-tap rational limit even trimmed.
        let preserve = ManualConfig {
            cutoff: 1.0,
            preserve_samples: true,
            ..rational(6_554)
        };
        let e = prepare(
            &Named::of(&preserve.into_upsampler_config().unwrap()).unwrap(),
            &cx(r, 2, 2),
        )
        .err()
        .unwrap();
        assert_eq!(e.stage, Stage::Feasibility);
        assert!(e.to_string().contains("needs 1048640 taps"), "{e}");
        // Growth: 16 taps per input (2560 on the grid) fit the budget of
        // 800 pipelines; the grown intermediate kernel does not.
        let intermediate = ManualConfig {
            phase: FilterPhase::Intermediate,
            phase_mix: 0.5,
            kaiser_beta: 6.0,
            ..rational(16)
        };
        let d = UpsamplerDesign::try_for_ratio(&intermediate, r).unwrap();
        let grown = try_design_kernel_for(&d, r).unwrap().len();
        assert_eq!(d.taps, 2560);
        let pipelines = 800;
        budget::check_rational(d.taps, r, 44_100, usize::MAX, pipelines).unwrap();
        let want = budget::check_rational(grown, r, 44_100, usize::MAX, pipelines).unwrap_err();
        let intermediate_up = intermediate.into_upsampler_config().unwrap();
        let e = upsampler(
            &Named::of(&intermediate_up).unwrap(),
            &cx(r, 2, pipelines),
            0.0,
        )
        .err()
        .unwrap();
        assert_eq!(e.stage, Stage::Prepare);
        assert_eq!(
            e.to_string(),
            format!("{want}: use a shorter filter or a lower output rate")
        );
        upsampler(&Named::of(&intermediate_up).unwrap(), &cx(r, 2, 2), 0.0).unwrap();
    }
}
