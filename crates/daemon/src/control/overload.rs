//! Fall back to a lighter chain when the DSP chain runs over real time.
//!
//! The control plane samples the engine every [`TICK`] and calls the window
//! overloaded when the load sits at real time in most samples, when underruns
//! hit a large share of the periods processed, or when the audio thread is
//! demoted to normal priority after `SIGXCPU` (a signal that found it at normal
//! priority already, or a demotion the kernel refused, is not one). The engine
//! then runs a lighter config, one step at a time, each only if the device can
//! play it ([`crate::engine::predicted_landings`]); `docs/control-plane.md`
//! ("DSP overload") holds the full rules.
//!
//! While varispeed's stage runs, its steps come first ([`VarispeedHold`]): they
//! leave the config alone, so the engine takes them through
//! [`crate::api::Engine::set_varispeed_hold`] and their attempt is the chain
//! already running: its budget pinned at half the period, its speed held at
//! 1×, the stage retired. Then the chain's steps:
//!
//! 0. A/B off, keeping chain A;
//! 1. the last upsampler setting that ran without overload in this run;
//! 2. the upsampler off, where the device plays a graph rate at x1;
//! 3. DSP off: at x1, else through the engine bypass resampler;
//! 4. as a last resort, that resampler's design at the requested target and
//!    output rate ([`cheapest_upsampler`]).
//!
//! A step is a transition the caller stages: apply [`Supervisor::config_for`]
//! and commit with [`Supervisor::commit`] only once the engine accepted it. Each
//! accepted config is an `Attempt` with its own readiness, timeout and
//! confirmation, and the report says `applied` once its chain landed with the
//! engine neither switching nor in error. A refused or failed step moves to the
//! next that plays; when none is left the fallback ends ([`Step::End`]).
//!
//! The saved config is never changed: [`Supervisor::plan`] and
//! [`Supervisor::commit_request`] map what is requested to what runs, so a
//! refused edit leaves the fallback as it was. It holds until the request
//! changes what it replaced (the upsampler, the output, A/B), the daemon
//! restarts (which falls back the same way instead of crash-looping), or,
//! for the chain's steps, playback moves to another source rate
//! ([`Step::RateLeft`]): the fallback is per rate. The request runs at the
//! new rate, and the rate left counts as over budget for it
//! ([`Supervisor::tripped_rates`]), so the per-rate plan
//! ([`super::cpu_budget::plan_rates`]) builds a lighter upsampler there
//! before it opens next time. That plan is the first line: this detector is
//! the net for a wrong estimate.
//!
//! A config counts as running fine (the one step 1 returns to) after three calm
//! seconds with no fallback, DSP on, its generation up and the device upsampled
//! ([`Supervisor::seed`] judges the start-up config the same way).
//!
//! Pure: the caller feeds samples and applies what it returns, serialized with
//! config edits.

use crate::api::{VarispeedHold, VarispeedRequest};
use audivane_protocol::ipc::{EngineConfig, OverloadAction, OverloadReport};
use audivane_protocol::{OutputConfig, RateRatio, RateTarget, UpsamplerConfig};
use std::collections::VecDeque;

/// Sampling period of the engine.
pub const TICK: std::time::Duration = std::time::Duration::from_millis(100);
/// Samples judged together (one second).
const WINDOW: usize = 10;
/// Samples at or above real time that make the window overloaded.
const LOADED_SAMPLES: usize = 8;
/// Share of the window's processed periods that underran which makes it
/// overloaded.
const FAILED_SHARE: f64 = 0.1;
/// Fewest underruns in the window for [`FAILED_SHARE`] to count (a reopen
/// or a start may underrun once or twice).
const MIN_FAILED: u64 = 3;
/// Samples a fallback step gets before the next judgement (two seconds).
const SETTLE: usize = 20;
/// Calm samples (load under [`CALM_LOAD`], no underrun) after which a
/// requested config counts as running fine (three seconds).
const GOOD_AFTER: usize = 30;
const CALM_LOAD: f64 = 0.8;
/// Samples after which a step the audio thread has not confirmed (its
/// chain generation not reached: a bypass design PipeWire stands in for
/// meanwhile, say) stops holding back the judgement (ten seconds).
const CONFIRM_WAIT: usize = 100;

/// One reading of the engine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// The DSP chain is running (playing on the DSP sink).
    pub running: bool,
    /// The engine is in its `Error` state (an open that failed).
    pub error: bool,
    /// The engine is in its `Switching` state (a reopen, or the audio
    /// thread waiting for a chain before it resumes).
    pub switching: bool,
    /// A client streams to the DSP sink: playback is requested.
    pub wanted: bool,
    /// Callback time over period time (1.0 = all of real time).
    pub load: f64,
    /// Cumulative underruns, across reopens.
    pub xrun_events: u64,
    /// Cumulative periods the audio thread processed, across reopens.
    pub periods: u64,
    /// Cumulative real-time to normal transitions of the audio thread (a
    /// `SIGXCPU` demotion that took): the only `SIGXCPU` count that is an
    /// overload.
    pub rt_overloads: u64,
    /// Cumulative `SIGXCPU` signals the audio thread saw (also those that
    /// found it at normal priority already).
    pub rt_signals: u64,
    /// Cumulative demotions of the real-time audio thread the kernel
    /// refused.
    pub rt_failed_demotions: u64,
    /// Output rate, Hz.
    pub output_rate: u32,
    /// DAC rate over graph rate.
    pub ratio: RateRatio,
    /// The chain generation on the audio thread at the last
    /// `rt_overloads` demotion (the active chain, or one warming up next to
    /// it), published with the event: a demotion is that chain's.
    pub rt_overload_generation: u64,
    /// The varispeed hold's sequence the audio thread had applied at that
    /// demotion (published with it): one before a hold applied is not the
    /// hold's.
    pub rt_overload_hold_seq: u64,
    /// Config generation the audio thread's chain runs.
    pub chain_generation: u64,
    /// The device the engine plays on, as probed (a fingerprint of its
    /// capabilities); 0 while none is known.
    pub device: u64,
    /// Device probes that answered, cumulative (the driver probes again
    /// after a failed open): one after an attempt began says the device was
    /// there for it.
    pub probes: u64,
    /// Varispeed's request and whether its stage runs.
    pub varispeed: VarispeedRequest,
    /// The per-rate plan ([`crate::api::RatePlan`]) builds another
    /// upsampler than the request's at the rate playing: a calm is not the
    /// request's own.
    pub substituted: bool,
    /// The source (graph) rate, Hz; 0 when unknown.
    pub source_rate: u32,
}

impl Default for Sample {
    fn default() -> Self {
        Self {
            running: false,
            error: false,
            switching: false,
            wanted: false,
            load: 0.0,
            xrun_events: 0,
            periods: 0,
            rt_overloads: 0,
            rt_signals: 0,
            rt_failed_demotions: 0,
            output_rate: 0,
            ratio: RateRatio::ONE,
            rt_overload_generation: 0,
            rt_overload_hold_seq: 0,
            chain_generation: 0,
            device: 0,
            probes: 0,
            varispeed: VarispeedRequest::default(),
            substituted: false,
            source_rate: 0,
        }
    }
}

/// The cheapest upsampler at the output rate `requested` lands at, with
/// the rest of the chain: the last resort when not even the bypass path
/// can play (see the module docs). The engine bypass resampler's
/// recommended design ([`super::config::bypass_upsampler`]: a Kaiser band
/// design sized to the output's resolution, minimum phase) at the same
/// target, so the same landing and output rate, and the same headroom.
pub fn cheapest_upsampler(requested: &UpsamplerConfig) -> UpsamplerConfig {
    UpsamplerConfig {
        target: requested.target,
        ..super::config::bypass_upsampler()
    }
}

/// Where the engine would play a config on the device
/// ([`crate::engine::predicted_landings`]), as the fallback needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    /// At no rate the device plays.
    None,
    /// It plays.
    Plays,
    /// DSP off plays, some source rates through the engine bypass
    /// resampler (the device lacks them).
    Converted,
}

impl Landing {
    /// The landing of `cfg` from its predicted `(graph rate, ratio)`
    /// landings ([`crate::engine::predicted_landings`]).
    pub fn of(cfg: &EngineConfig, landings: &Result<Vec<(u32, RateRatio)>, String>) -> Self {
        match landings {
            Ok(l) if l.is_empty() => Self::None,
            Err(_) => Self::None,
            Ok(l) if !cfg.dsp_enabled && l.iter().any(|(_, r)| *r != RateRatio::ONE) => {
                Self::Converted
            }
            Ok(_) => Self::Plays,
        }
    }
}

/// The upsampler-related part of a config, which a revert restores.
#[derive(Clone, Debug, PartialEq)]
struct HeavyPart {
    upsampler: UpsamplerConfig,
    output: OutputConfig,
    ab: bool,
}

impl HeavyPart {
    fn of(cfg: &EngineConfig) -> Self {
        Self {
            upsampler: cfg.chain.upsampler.clone(),
            output: cfg.chain.output,
            ab: cfg.ab.is_some(),
        }
    }
}

/// What the engine runs at: the heavy part and whether DSP is on.
type RunKey = (HeavyPart, bool);

fn run_key(cfg: &EngineConfig) -> RunKey {
    (HeavyPart::of(cfg), cfg.dsp_enabled)
}

/// A fallback step, lightest last.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
enum Level {
    VarispeedBudget,
    VarispeedUnity,
    VarispeedOff,
    AbOff,
    Revert(HeavyPart),
    UpsamplerOff,
    Bypass,
    LightUpsampler,
}

impl Level {
    fn action(&self) -> OverloadAction {
        match self {
            Self::VarispeedBudget => OverloadAction::VarispeedBudget,
            Self::VarispeedUnity => OverloadAction::VarispeedUnity,
            Self::VarispeedOff => OverloadAction::VarispeedOff,
            Self::AbOff => OverloadAction::AbOff,
            Self::Revert(_) => OverloadAction::Reverted,
            Self::UpsamplerOff => OverloadAction::UpsamplerOff,
            Self::Bypass => OverloadAction::Bypass,
            Self::LightUpsampler => OverloadAction::LightUpsampler,
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::VarispeedBudget => 0,
            Self::VarispeedUnity => 1,
            Self::VarispeedOff => 2,
            Self::AbOff => 3,
            Self::Revert(_) => 4,
            Self::UpsamplerOff => 5,
            Self::Bypass => 6,
            Self::LightUpsampler => 7,
        }
    }

    /// A varispeed step: it leaves the config alone.
    fn is_varispeed(&self) -> bool {
        matches!(
            self,
            Self::VarispeedBudget | Self::VarispeedUnity | Self::VarispeedOff
        )
    }

    /// What the step holds varispeed to: the chain's steps keep the stage
    /// off (they come after it).
    fn hold(&self) -> VarispeedHold {
        match self {
            Self::VarispeedBudget => VarispeedHold::Budget,
            Self::VarispeedUnity => VarispeedHold::Unity,
            _ => VarispeedHold::Off,
        }
    }

    /// The config the engine runs for `requested` at this step.
    fn apply(&self, requested: &EngineConfig) -> EngineConfig {
        let mut eff = requested.clone();
        match self {
            Self::VarispeedBudget | Self::VarispeedUnity | Self::VarispeedOff => return eff,
            Self::AbOff => {}
            Self::Revert(good) => {
                eff.chain.upsampler = good.upsampler.clone();
                eff.chain.output = good.output;
            }
            Self::UpsamplerOff => eff.chain.upsampler.target = RateTarget::Off,
            Self::LightUpsampler => {
                eff.chain.upsampler = cheapest_upsampler(&requested.chain.upsampler);
            }
            Self::Bypass => eff.dsp_enabled = false,
        }
        // A/B off is the chain's first step; every later one keeps it off
        // (side B may carry its own heavy filter).
        eff.ab = None;
        eff
    }
}

/// A fallback step chosen by [`Supervisor::observe`] and not applied yet.
/// The caller applies [`Supervisor::config_for`] and then calls
/// [`Supervisor::commit`] (the engine accepted it) or
/// [`Supervisor::rejected`] (it did not). A varispeed step
/// ([`Self::varispeed_hold`]) leaves the config alone: the caller sets the
/// hold instead and commits it with [`Supervisor::expected_generation`].
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    level: Level,
    landing: Landing,
    trip: Trip,
    output_rate: u32,
    /// The source rate it is for (0: unknown).
    source_rate: u32,
}

impl Transition {
    /// The hold a varispeed step sets (`None`: a step of the chain).
    pub fn varispeed_hold(&self) -> Option<VarispeedHold> {
        self.level.is_varispeed().then(|| self.level.hold())
    }
}

/// What made the window an overload.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cause {
    /// The load at or above real time in most samples.
    Load,
    /// Underruns in a large share of the periods processed.
    Underruns { failed: u64, periods: u64 },
    /// The audio thread moved from real-time to normal priority
    /// (`SIGXCPU`).
    Demotion,
}

/// An overload: its cause and the mean load over the window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Trip {
    load: f64,
    cause: Cause,
    /// The request's own upsampler ran when it tripped (not the per-rate
    /// plan's): the fallback is per rate ([`Step::RateLeft`]). One made
    /// while the plan's lighter design ran holds as before: the estimate
    /// and its fallback both failed there.
    per_rate: bool,
}

impl Trip {
    /// Why, after a subject ("this one", "it", "the chain"), for the
    /// notice: the load is quoted only when it was the cause.
    fn predicate(&self) -> String {
        match self.cause {
            Cause::Load => format!(
                "needed more CPU than real time allows (load {})",
                percent(self.load)
            ),
            Cause::Underruns { .. } => {
                "could not keep up: the DAC ran short of samples in many periods".into()
            }
            Cause::Demotion => "ran so long without a break that the audio thread was moved off \
                 real-time priority"
                .into(),
        }
    }

    /// The cause, for the log.
    fn log(&self) -> String {
        match self.cause {
            Cause::Load => format!("load {} of real time", percent(self.load)),
            Cause::Underruns { failed, periods } => {
                format!("underruns in {failed} of {periods} periods")
            }
            Cause::Demotion => "the audio thread moved off real-time priority".into(),
        }
    }
}

/// What the caller does after [`Supervisor::observe`] or
/// [`Supervisor::rejected`].
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Step {
    Nothing,
    /// Stage this step: apply [`Supervisor::config_for`] to the engine.
    Try(Transition),
    /// The step in effect now runs on the audio thread: announce its report
    /// (now `applied`) again.
    Confirmed,
    /// Overloaded with nothing lighter left that plays: log this once.
    Exhausted {
        log: String,
    },
    /// The fallback does not run and no other step is left to try on this
    /// device and policy: apply the request to the engine, then call
    /// [`Supervisor::end`] once it accepted it, or
    /// [`Supervisor::end_refused`] (it stays pending and is asked again).
    End,
    /// A new varispeed request ended the varispeed step in effect (nothing
    /// to apply but the hold, [`Supervisor::varispeed_hold`]).
    Ended,
    /// Playback moved from the source rate the chain's fallback was made
    /// for (`from`) to another (`to`): the fallback is per rate, so it ends
    /// there. Handled as [`Self::End`] (apply the request, then
    /// [`Supervisor::end`] or [`Supervisor::end_refused`]); `from` counts as
    /// over budget for the request from now on
    /// ([`Supervisor::tripped_rates`]).
    RateLeft {
        from: u32,
        to: u32,
    },
}

/// One try at running the step in effect, begun when the engine accepted
/// a config for it (the step's commit, or an edit of the device, formats
/// or rate policy it survives) or when another device turned up behind
/// the same name. Its readiness, its timeout and its confirmation are its
/// own.
#[derive(Clone, Copy, Debug)]
struct Attempt {
    /// Generation its chain must reach.
    generation: u64,
    /// The varispeed hold's sequence the audio thread must acknowledge
    /// (a varispeed step's; 0 for the chain's steps).
    hold_seq: u64,
    /// The driver's probe count when it began.
    probes: u64,
    /// Samples since it began.
    age: usize,
    /// The engine was seen out of `Error` during it.
    seen_ok: bool,
    /// It ran: its chain landed, the engine neither switching nor in error.
    ready: bool,
}

impl Attempt {
    fn new(generation: u64, probes: u64) -> Self {
        Self {
            generation,
            hold_seq: 0,
            probes,
            age: 0,
            seen_ok: false,
            ready: false,
        }
    }

    /// A probe answered since it began, and the device is known: an open
    /// that fails now is this attempt's (not an unplugged device, nor
    /// capabilities left from an earlier probe).
    fn probed(&self, s: &Sample) -> bool {
        s.device != 0 && s.probes > self.probes
    }
}

/// The step in effect.
#[derive(Debug)]
struct Active {
    /// The request it was made for (updated by later edits it survives).
    from: EngineConfig,
    level: Level,
    report: OverloadReport,
    /// The overload it was made for (the next step after it fails says
    /// the same).
    trip: Trip,
    attempt: Attempt,
    /// What its landing was last checked on: (device, the config the step
    /// runs). Checked again when either changes.
    checked: Option<(u64, EngineConfig)>,
}

impl Active {
    /// A new attempt: the confirmation is the new attempt's.
    fn retry(&mut self, generation: u64, probes: u64) {
        self.attempt = Attempt::new(generation, probes);
        self.report.applied = false;
        self.checked = None;
    }
}

/// One sample in the judged window.
#[derive(Clone, Copy, Debug)]
struct Slot {
    load: f64,
    failed: u64,
    periods: u64,
}

/// The fallback state machine.
#[derive(Debug, Default)]
pub struct Supervisor {
    active: Option<Active>,
    /// The last requested upsampler setting that ran fine.
    last_good: Option<HeavyPart>,
    /// Steps that failed for the current request.
    failed: Vec<Level>,
    window: VecDeque<Slot>,
    last: Option<Sample>,
    settle: usize,
    calm: usize,
    /// What the calm count was counted on: (chain generation, output
    /// rate, ratio).
    calm_key: Option<(u64, u32, RateRatio)>,
    /// A `SIGXCPU` demotion not judged yet.
    latched: bool,
    /// What the engine was last told to run, and the generation of that
    /// apply: nothing is judged before the chain reaches it.
    applied: Option<RunKey>,
    expect: u64,
    /// The varispeed hold's sequence the audio thread must have applied
    /// before what runs is judged (the hold the engine was last given).
    expect_hold: u64,
    episodes: u64,
    exhausted_logged: bool,
    /// The last device seen ([`Sample::device`], not 0).
    device: Option<u64>,
    /// [`Step::End`] is pending: asked every sample until
    /// [`Supervisor::end`], every [`SETTLE`] samples after a refusal.
    ending: Option<usize>,
    /// Refusals of the pending end in a row.
    end_refusals: u32,
    /// The source rates where the request (its heavy part) overloaded and
    /// a chain fallback ran: the per-rate plan treats them as over budget.
    tripped: Option<(HeavyPart, Vec<u32>)>,
}

/// Whether going from the request `from` to `to` ends a fallback at
/// `level`.
fn ends(level: &Level, from: &EngineConfig, to: &EngineConfig) -> bool {
    match level {
        Level::VarispeedBudget | Level::VarispeedUnity | Level::VarispeedOff => {
            HeavyPart::of(from) != HeavyPart::of(to)
                || from.dsp_enabled != to.dsp_enabled
                || from.varispeed != to.varispeed
        }
        Level::Bypass => {
            from.dsp_enabled != to.dsp_enabled || from.chain != to.chain || from.ab != to.ab
        }
        // A new side B (a lighter filter, say) tries A/B again. Its
        // description, not the materialised chain B, which follows every
        // edit of chain A.
        _ => {
            HeavyPart::of(from) != HeavyPart::of(to)
                || from.ab.as_ref().map(|ab| &ab.b) != to.ab.as_ref().map(|ab| &ab.b)
        }
    }
}

/// Whether going from the request `from` to `to` changes what the engine
/// opens: the device, the formats or the rate policy.
fn reopens(from: &EngineConfig, to: &EngineConfig) -> bool {
    from.device != to.device || from.formats != to.formats || from.rate != to.rate
}

fn percent(load: f64) -> String {
    format!("{:.0}%", load * 100.0)
}

impl Supervisor {
    /// The config the engine runs for the request `requested`: the request
    /// itself, or its fallback when the request leaves the fallback's part
    /// alone. Changes nothing: [`Self::commit_request`] does, once the
    /// engine accepted it.
    pub fn plan(&self, requested: &EngineConfig) -> EngineConfig {
        match &self.active {
            Some(a) if !ends(&a.level, &a.from, requested) => a.level.apply(requested),
            _ => requested.clone(),
        }
    }

    /// The engine accepted [`Self::plan`] of `requested`, as `generation`
    /// ([`crate::api::Engine::applied_generation`]). Ends the fallback when
    /// the request changed what it replaced; returns whether it did.
    pub fn commit_request(&mut self, requested: &EngineConfig, generation: u64) -> bool {
        let ended = self
            .active
            .as_ref()
            .is_some_and(|a| ends(&a.level, &a.from, requested));
        if ended {
            self.active = None;
            self.ending = None;
            self.exhausted_logged = false;
        }
        let probes = self.probes();
        if let Some(a) = &mut self.active {
            if reopens(&a.from, requested) {
                // The engine reopens the step on another device, format
                // or rate policy: a new attempt, and every step may play
                // (or fail) there anew.
                a.retry(generation, probes);
                self.failed.clear();
                self.ending = None;
            }
            a.from = requested.clone();
        }
        let running = self.plan(requested);
        if self.applied.as_ref() != Some(&run_key(&running)) {
            // A new chain to judge: the steps that failed for the old
            // request may play now, and a signal of the old chain is not
            // this one's.
            if self.applied.as_ref().map(|k| &k.0) != Some(&HeavyPart::of(requested)) {
                self.failed.clear();
            }
            self.applied = Some(run_key(&running));
            self.expect = generation;
            self.latched = false;
            self.reset_window(SETTLE / 2);
        }
        ended
    }

    /// The engine started with the request `cfg` (no fallback yet), as
    /// `generation` ([`crate::api::Engine::applied_generation`]): judged
    /// like an accepted edit, so the startup config is settled, judged, and
    /// counts as running fine once the checks of [`Self::observe`] see its
    /// upsampler really run calm.
    pub fn seed(&mut self, cfg: &EngineConfig, generation: u64) {
        self.commit_request(cfg, generation);
    }

    /// The engine accepted the request `requested` after [`Step::End`], as
    /// `generation`: the fallback is over.
    pub fn end(&mut self, requested: &EngineConfig, generation: u64) {
        self.active = None;
        self.ending = None;
        self.exhausted_logged = false;
        self.commit_request(requested, generation);
    }

    /// The engine refused the request after [`Step::End`]: the fallback
    /// stays, the end stays pending and is asked again after `SETTLE`.
    /// Returns whether this is the first refusal in a row (to log).
    pub fn end_refused(&mut self) -> bool {
        if self.ending.is_none() {
            return false;
        }
        self.ending = Some(SETTLE);
        self.end_refusals += 1;
        self.end_refusals == 1
    }

    /// The probe count of the last sample.
    fn probes(&self) -> u64 {
        self.last.as_ref().map_or(0, |s| s.probes)
    }

    /// The config the engine runs at the step `t` for `requested`.
    pub fn config_for(&self, t: &Transition, requested: &EngineConfig) -> EngineConfig {
        t.level.apply(requested)
    }

    /// What the fallback in effect holds varispeed to.
    pub fn varispeed_hold(&self) -> VarispeedHold {
        self.active
            .as_ref()
            .map_or(VarispeedHold::None, |a| a.level.hold())
    }

    /// The generation the chain in effect runs as: a varispeed step's
    /// attempt, which applies no config ([`Transition::varispeed_hold`]).
    pub fn expected_generation(&self) -> u64 {
        self.expect
    }

    /// The engine was given the hold in effect as `seq`
    /// ([`crate::api::Engine::set_varispeed_hold`]): nothing is judged, and
    /// no demotion counts, before the audio thread applies it.
    pub fn note_hold_seq(&mut self, seq: u64) {
        self.expect_hold = self.expect_hold.max(seq);
    }

    /// A new varispeed request arrived (via `set_varispeed`): clear failed
    /// varispeed steps and, when a varispeed fallback step is in effect,
    /// end it at once so the request is judged afresh. Chain fallback steps
    /// stay in effect (and keep the stage off).
    pub fn end_varispeed_fallback(&mut self) -> bool {
        self.failed.retain(|l| !l.is_varispeed());
        if self.active.as_ref().is_some_and(|a| a.level.is_varispeed()) {
            self.active = None;
            self.ending = None;
            self.exhausted_logged = false;
            self.latched = false;
            self.reset_window(SETTLE / 2);
            return true;
        }
        false
    }

    /// [`Self::commit`] for a varispeed step, whose hold the engine took as
    /// `hold_seq`: the step lands once the audio thread acknowledges it
    /// ([`crate::api::VarispeedRequest::hold_ack`]; for a retirement, once
    /// the stage is gone).
    pub fn commit_varispeed(
        &mut self,
        t: Transition,
        requested: &EngineConfig,
        generation: u64,
        hold_seq: u64,
    ) -> (String, OverloadReport) {
        let out = self.commit(t, requested, generation);
        if let Some(a) = &mut self.active {
            a.attempt.hold_seq = hold_seq;
        }
        self.note_hold_seq(hold_seq);
        out
    }

    /// The fallback in effect, for `Status`.
    pub fn report(&self) -> Option<&OverloadReport> {
        self.active.as_ref().map(|a| &a.report)
    }

    /// The source rates where `requested` (its upsampler, output and A/B)
    /// overloaded in this run and a fallback ran ([`Step::RateLeft`]): the
    /// per-rate plan falls back there.
    pub fn tripped_rates(&self, requested: &EngineConfig) -> &[u32] {
        match &self.tripped {
            Some((h, rates)) if *h == HeavyPart::of(requested) => rates,
            _ => &[],
        }
    }

    /// The last requested upsampler setting that ran without overload.
    pub fn last_good_upsampler(&self) -> Option<&UpsamplerConfig> {
        self.last_good.as_ref().map(|g| &g.upsampler)
    }

    fn reset_window(&mut self, settle: usize) {
        self.window.clear();
        self.settle = settle;
        self.calm = 0;
    }

    /// Feed one sample with the current request; returns what to do.
    /// `plays` says whether the engine would find a rate to play a config
    /// at on the device ([`crate::engine::predicted_landings`]).
    pub fn observe(
        &mut self,
        s: Sample,
        requested: &EngineConfig,
        plays: &dyn Fn(&EngineConfig) -> Landing,
    ) -> Step {
        let prev = self.last.replace(s);
        // A demotion before the first sample counts too (the counters start
        // at 0 with the engine).
        let before = |f: fn(&Sample) -> u64| prev.as_ref().map_or(0, f);
        // Only a real real-time to normal transition is an overload; a
        // signal that found the thread at normal priority, or a demotion
        // the kernel refused, is not (the load and underrun rules judge
        // what such a chain does).
        let signal = s.rt_overloads > before(|p| p.rt_overloads);
        let refused = s.rt_failed_demotions > before(|p| p.rt_failed_demotions);
        let signals = s.rt_signals.saturating_sub(before(|p| p.rt_signals));
        if refused {
            log::warn!(
                "the audio thread ran out of its real-time budget and the kernel refused to \
                 demote it ({} times so far)",
                s.rt_failed_demotions
            );
        } else if signals > 0 && !signal {
            log::debug!("SIGXCPU for the audio thread at normal priority: no overload");
        }
        let (failed, periods) = match prev {
            Some(p) => (
                s.xrun_events.saturating_sub(p.xrun_events),
                s.periods.saturating_sub(p.periods),
            ),
            None => (0, 0),
        };
        // Latched through startup and settling. A demotion is the chain's
        // that ran on the audio thread when it happened (published with
        // it): one of the old chain is not the new one's, one while the new
        // chain warms up next to the old one is.
        if signal
            && s.rt_overload_generation >= self.expect
            && s.rt_overload_hold_seq >= self.expect_hold
        {
            self.latched = true;
        }
        if s.device != 0 && self.device != Some(s.device) {
            if self.device.is_some() {
                // Another device behind the same name: a new attempt, and
                // every step may play (or fail) there anew, and every rate
                // is priced anew.
                self.failed.clear();
                self.tripped = None;
                self.ending = None;
                let probes = before(|p| p.probes);
                if let Some(a) = &mut self.active {
                    let generation = a.attempt.generation;
                    a.retry(generation, probes);
                }
            }
            self.device = Some(s.device);
        }
        // A new varispeed request replaces what a varispeed step held
        // back: the step ends and the request is judged afresh (its steps
        // may play again).
        if prev.is_some_and(|p| p.varispeed.requests != s.varispeed.requests) {
            self.failed.retain(|l| !l.is_varispeed());
            if self.active.as_ref().is_some_and(|a| a.level.is_varispeed()) {
                self.active = None;
                self.ending = None;
                self.exhausted_logged = false;
                self.latched = false;
                self.reset_window(SETTLE / 2);
                return Step::Ended;
            }
        }
        if let Some(step) = self.judge_step(&s, requested, plays) {
            return step;
        }
        if !s.running || s.chain_generation < self.expect || s.varispeed.hold_ack < self.expect_hold
        {
            self.window.clear();
            self.calm = 0;
            return Step::Nothing;
        }
        if self.settle > 0 {
            self.settle -= 1;
            return Step::Nothing;
        }
        self.window.push_back(Slot {
            load: s.load,
            failed,
            periods,
        });
        if self.window.len() > WINDOW {
            self.window.pop_front();
        }
        let loaded = self.window.iter().filter(|w| w.load >= 1.0).count();
        let failed_total: u64 = self.window.iter().map(|w| w.failed).sum();
        let periods_total: u64 = self.window.iter().map(|w| w.periods).sum();
        let underrunning = failed_total >= MIN_FAILED
            && failed_total as f64 >= FAILED_SHARE * periods_total.max(1) as f64;
        let overloaded = self.latched || loaded >= LOADED_SAMPLES || underrunning;
        if !overloaded {
            let key = (s.chain_generation, s.output_rate, s.ratio);
            if self.calm_key != Some(key) {
                self.calm_key = Some(key);
                self.calm = 0;
            }
            if s.load < CALM_LOAD && failed == 0 {
                self.calm += 1;
            } else {
                self.calm = 0;
            }
            if self.calm >= GOOD_AFTER && self.upsampler_runs(&s, requested) {
                self.last_good = Some(HeavyPart::of(requested));
            }
            return Step::Nothing;
        }
        // The load reported: the mean over the window. The cause the
        // notice gives: the load when it tripped, else the demotion, else
        // the underruns.
        let load = self.window.iter().map(|w| w.load).sum::<f64>() / self.window.len() as f64;
        let cause = if loaded >= LOADED_SAMPLES {
            Cause::Load
        } else if self.latched {
            Cause::Demotion
        } else {
            Cause::Underruns {
                failed: failed_total,
                periods: periods_total,
            }
        };
        self.latched = false;
        let current = self.active.as_ref().map(|a| a.level.clone());
        self.next_step(
            current.as_ref(),
            false,
            Trip {
                load,
                cause,
                per_rate: !s.substituted,
            },
            (s.output_rate, s.source_rate),
            requested,
            plays,
        )
    }

    /// The checks of the step in effect for the sample `s`: a pending end,
    /// its landing on the device, then its attempt (an open that failed, a
    /// resume that stalled, its confirmation). `Some` when they decide.
    fn judge_step(
        &mut self,
        s: &Sample,
        requested: &EngineConfig,
        plays: &dyn Fn(&EngineConfig) -> Landing,
    ) -> Option<Step> {
        self.active.as_ref()?;
        if let Some(wait) = &mut self.ending {
            if *wait > 1 {
                *wait -= 1;
                return Some(Step::Nothing);
            }
            *wait = 0;
            return Some(Step::End);
        }
        // Per rate: a chain fallback made at one source rate ends once
        // playback runs at another (the request is judged there afresh, and
        // the per-rate plan keeps the old rate on a fallback).
        if let Some(a) = &self.active
            && !a.level.is_varispeed()
            && a.trip.per_rate
            && a.report.source_rate != 0
            && s.running
            && !s.switching
            && !s.error
            && s.source_rate != 0
            && s.source_rate != a.report.source_rate
        {
            let (from, to) = (a.report.source_rate, s.source_rate);
            let heavy = HeavyPart::of(requested);
            match &mut self.tripped {
                Some((h, rates)) if *h == heavy => {
                    if !rates.contains(&from) {
                        rates.push(from);
                    }
                }
                _ => self.tripped = Some((heavy, vec![from])),
            }
            log::info!(
                "overload fallback made at {from} Hz: playback moved to {to} Hz, running the \
                 requested setting there ({from} Hz falls back from now on)"
            );
            self.ending = Some(0);
            self.end_refusals = 0;
            return Some(Step::RateLeft { from, to });
        }
        let a = self.active.as_mut()?;
        a.attempt.age += 1;
        let landed = s.chain_generation >= a.attempt.generation
            && s.varispeed.hold_ack >= a.attempt.hold_seq;
        // Its landing, again whenever the device or what it runs changed:
        // a fallback that no longer plays there is replaced.
        if s.device != 0 {
            let key = (s.device, a.level.apply(requested));
            if a.checked.as_ref() != Some(&key) {
                let lost = plays(&key.1) == Landing::None;
                a.checked = Some(key);
                if lost {
                    log::warn!(
                        "overload fallback ({}) no longer plays on the device",
                        action_name(a.report.action)
                    );
                    let level = a.level.clone();
                    return Some(self.advance(level, requested, plays));
                }
            }
        }
        let at = &mut a.attempt;
        if !at.ready {
            let failed = if s.error && at.probed(s) && (at.seen_ok || at.age >= SETTLE) {
                // A failed open of a device that answered a probe for this
                // attempt (an unplugged one, or one not probed again yet,
                // is waited for).
                Some("left the engine in error")
            } else if !s.error && s.switching && s.wanted && at.age >= CONFIRM_WAIT {
                // Playback is requested and the attempt has not run: stuck.
                Some("did not start playback in time")
            } else {
                None
            };
            if let Some(why) = failed {
                log::warn!(
                    "overload fallback step ({}) {why}; trying the next one",
                    action_name(a.report.action)
                );
                let level = a.level.clone();
                self.failed.push(level.clone());
                return Some(self.advance(level, requested, plays));
            }
            if !landed && at.age == CONFIRM_WAIT {
                // Not running in time, nobody waiting on it: judge what
                // runs anyway (it is still announced once it runs).
                log::info!(
                    "overload fallback step not confirmed by the audio thread yet; judging \
                     what runs"
                );
                self.expect = self.expect.min(s.chain_generation);
                self.expect_hold = self.expect_hold.min(s.varispeed.hold_ack);
            }
        }
        if !s.error {
            at.seen_ok = true;
            if landed && !s.switching {
                at.ready = true;
                if !a.report.applied {
                    a.report.applied = true;
                    return Some(Step::Confirmed);
                }
            }
        }
        None
    }

    /// The step in effect at `from` failed or lost its landing: the next
    /// step to try on this device and policy (a lighter one first, else
    /// from the top of the ladder, each once), or the end of the fallback
    /// ([`Step::End`]) when none is left. With no fallback in effect (a
    /// first step refused), nothing left is [`Step::Exhausted`] instead:
    /// the request runs.
    fn advance(
        &mut self,
        from: Level,
        requested: &EngineConfig,
        plays: &dyn Fn(&EngineConfig) -> Landing,
    ) -> Step {
        let Some(a) = &self.active else {
            return Step::Nothing;
        };
        let (trip, rate, source_rate) = (a.trip, a.report.output_rate, a.report.source_rate);
        self.reset_window(SETTLE);
        let next = self
            .choose(Some(&from), true, requested, plays)
            .or_else(|| self.choose(None, true, requested, plays));
        if let Some((level, landing)) = next {
            log::info!("overload fallback: trying {}", action_name(level.action()));
            return Step::Try(Transition {
                level,
                landing,
                trip,
                output_rate: rate,
                source_rate,
            });
        }
        log::warn!(
            "overload fallback: no other step plays on this device; running the requested \
             setting"
        );
        self.ending = Some(0);
        self.end_refusals = 0;
        Step::End
    }

    /// Whether the calm of `s` is the requested upsampler's own: no
    /// fallback, DSP on, upsampling, and the device at an upsampled rate.
    fn upsampler_runs(&self, s: &Sample, requested: &EngineConfig) -> bool {
        self.active.is_none()
            && !s.substituted
            && self.applied.as_ref() == Some(&run_key(requested))
            && requested.dsp_enabled
            && requested.chain.upsampler.target != RateTarget::Off
            && s.ratio != RateRatio::ONE
    }

    /// The engine refused the step `t`: the next one to try (see
    /// `advance`).
    pub fn rejected(
        &mut self,
        t: Transition,
        requested: &EngineConfig,
        plays: &dyn Fn(&EngineConfig) -> Landing,
    ) -> Step {
        self.failed.push(t.level.clone());
        if self.active.is_some() {
            return self.advance(t.level, requested, plays);
        }
        self.next_step(
            Some(&t.level),
            true,
            t.trip,
            (t.output_rate, t.source_rate),
            requested,
            plays,
        )
    }

    fn next_step(
        &mut self,
        from: Option<&Level>,
        from_failed: bool,
        trip: Trip,
        (output_rate, source_rate): (u32, u32),
        requested: &EngineConfig,
        plays: &dyn Fn(&EngineConfig) -> Landing,
    ) -> Step {
        self.reset_window(SETTLE);
        if let Some((level, landing)) = self.choose(from, from_failed, requested, plays) {
            return Step::Try(Transition {
                level,
                landing,
                trip,
                output_rate,
                source_rate,
            });
        }
        if self.exhausted_logged {
            return Step::Nothing;
        }
        self.exhausted_logged = true;
        Step::Exhausted {
            log: format!(
                "DSP overload at {output_rate} Hz ({}) with nothing lighter left that the device \
                 plays",
                trip.log()
            ),
        }
    }

    /// The next step after `from` (`None`: the first one): lighter, not
    /// failed for this request, and playable, with where it plays. A step
    /// that ran (and still overloaded) is not followed by a heavier one: the
    /// light upsampler only follows a revert, or a step that failed.
    fn choose(
        &self,
        from: Option<&Level>,
        from_failed: bool,
        requested: &EngineConfig,
        plays: &dyn Fn(&EngineConfig) -> Landing,
    ) -> Option<(Level, Landing)> {
        let wanted = HeavyPart::of(requested);
        let upsampling = requested.chain.upsampler.target != RateTarget::Off;
        let mut ladder = Vec::new();
        // Varispeed's steps, while its stage runs: each only where it
        // changes something.
        let vs = self
            .last
            .map_or_else(VarispeedRequest::default, |l| l.varispeed);
        if vs.running {
            if vs.budget < 0.5 {
                ladder.push(Level::VarispeedBudget);
            }
            if vs.speed > 1.0 {
                ladder.push(Level::VarispeedUnity);
            }
            ladder.push(Level::VarispeedOff);
        }
        if requested.dsp_enabled && requested.ab.is_some() {
            ladder.push(Level::AbOff);
        }
        if let Some(good) = &self.last_good
            && *good != wanted
        {
            ladder.push(Level::Revert(good.clone()));
        }
        if upsampling {
            ladder.push(Level::UpsamplerOff);
        }
        if requested.dsp_enabled {
            ladder.push(Level::Bypass);
            if upsampling
                && cheapest_upsampler(&requested.chain.upsampler) != requested.chain.upsampler
            {
                ladder.push(Level::LightUpsampler);
            }
        }
        let start = from.map_or(0, |l| l.rank() + 1);
        let lighter_ran = matches!(from, Some(Level::UpsamplerOff | Level::Bypass)) && !from_failed;
        ladder
            .into_iter()
            .filter(|l| {
                l.rank() >= start
                    && !(lighter_ran && *l == Level::LightUpsampler)
                    && !self.failed.contains(l)
            })
            .find_map(|l| match plays(&l.apply(requested)) {
                Landing::None => None,
                landing => Some((l, landing)),
            })
    }

    /// The engine accepted the step `t` for `requested`, as `generation`:
    /// it is in effect. Returns the line to log and the report to announce
    /// (`applied` once the audio thread runs it, [`Step::Confirmed`]).
    pub fn commit(
        &mut self,
        t: Transition,
        requested: &EngineConfig,
        generation: u64,
    ) -> (String, OverloadReport) {
        self.episodes += 1;
        let Transition {
            level,
            landing,
            trip,
            output_rate,
            source_rate,
        } = t;
        let why = trip.predicate();
        let upsampling = requested.chain.upsampler.target != RateTarget::Off;
        let stage = if level.is_varispeed() {
            "varispeed"
        } else if level == Level::AbOff {
            "ab"
        } else if upsampling && level != Level::Bypass {
            "upsampler"
        } else {
            "chain"
        };
        let vs = self
            .last
            .map_or_else(VarispeedRequest::default, |l| l.varispeed);
        let (action, mut message, done) = match &level {
            Level::VarispeedBudget => (
                OverloadAction::VarispeedBudget,
                format!(
                    "The engine narrowed varispeed's speed range to what half of each period \
                     sustains: the chain with varispeed {why}. Your settings are kept."
                ),
                "pinned varispeed's processing budget at half the period",
            ),
            Level::VarispeedUnity => (
                OverloadAction::VarispeedUnity,
                format!(
                    "The engine held varispeed at 1× (you asked for {:.2}×): the chain with \
                     varispeed {why}. Your settings are kept; a new speed tries again.",
                    vs.speed
                ),
                "held varispeed at 1×",
            ),
            Level::VarispeedOff => {
                let after_unity = matches!(
                    self.active.as_ref().map(|a| &a.level),
                    Some(Level::VarispeedUnity)
                );
                (
                    OverloadAction::VarispeedOff,
                    format!(
                        "The engine switched varispeed off: {}the chain with varispeed {why}. \
                         Your settings are kept; a new varispeed setting tries again.",
                        if after_unity { "even at 1×, " } else { "" }
                    ),
                    "switched varispeed off",
                )
            }
            Level::AbOff => (
                OverloadAction::AbOff,
                format!(
                    "The engine turned A/B off and kept chain A: both sides together {why}. \
                     Your settings are kept; a new side B (a lighter filter, say) tries again."
                ),
                "turned A/B off (chain A kept)",
            ),
            Level::Revert(_) => (
                OverloadAction::Reverted,
                format!(
                    "The engine went back to the previous upsampler setting: this one {why}. \
                     Your setting is kept; lower the taps or the output rate."
                ),
                "went back to the last upsampler setting that ran",
            ),
            Level::UpsamplerOff => (
                OverloadAction::UpsamplerOff,
                format!(
                    "The engine switched the upsampler off: it {why}. Your setting is kept; \
                     lower the taps or the output rate."
                ),
                "switched the upsampler off",
            ),
            Level::Bypass => {
                let after_off = matches!(
                    self.active.as_ref().map(|a| &a.level),
                    Some(Level::UpsamplerOff)
                );
                let converted = landing == Landing::Converted;
                (
                    OverloadAction::Bypass,
                    format!(
                        "The engine switched DSP off: {}the chain {why}.{} Your settings are \
                         kept; make the chain lighter.",
                        if after_off {
                            "even without the upsampler, "
                        } else {
                            ""
                        },
                        if converted {
                            " Where the DAC lacks the source's rate, the engine's bypass \
                             resampler converts it (its recommended filter, nothing else of the \
                             chain)."
                        } else {
                            ""
                        }
                    ),
                    if converted {
                        "switched to the bypass path (the engine's bypass resampler converts \
                         the rates the device lacks: resample-only)"
                    } else {
                        "switched to the bypass path"
                    },
                )
            }
            Level::LightUpsampler => (
                OverloadAction::LightUpsampler,
                format!(
                    "The engine switched to its bypass resampler's filter at the same output \
                     rate: this one {why}, and the DAC plays nothing lighter. Your setting is \
                     kept; lower the taps or the output rate."
                ),
                "switched to the bypass resampler's filter at the same output rate (nothing \
                 lighter plays on this device)",
            ),
        };
        if vs.wanted && !level.is_varispeed() {
            message.push_str(" Varispeed stays off while this runs.");
        }
        if requested.ab.is_some() && !level.is_varispeed() && level != Level::AbOff {
            message.push_str(" A/B stays off while this runs.");
        }
        let report = OverloadReport {
            stage: stage.to_string(),
            output_rate,
            load: trip.load,
            action,
            message,
            episode: self.episodes,
            applied: false,
            source_rate,
        };
        let running = level.apply(requested);
        self.applied = Some(run_key(&running));
        self.expect = generation;
        self.latched = false;
        self.reset_window(SETTLE);
        self.active = Some(Active {
            from: requested.clone(),
            level,
            report: report.clone(),
            trip,
            attempt: Attempt::new(generation, self.probes()),
            checked: None,
        });
        self.ending = None;
        let log = format!(
            "DSP overload of the {stage} at {output_rate} Hz ({}); {done} (the saved config is \
             unchanged)",
            trip.log()
        );
        (log, report)
    }
}

fn action_name(action: OverloadAction) -> &'static str {
    match action {
        OverloadAction::VarispeedBudget => "varispeed budget pinned",
        OverloadAction::VarispeedUnity => "varispeed at 1x",
        OverloadAction::VarispeedOff => "varispeed off",
        OverloadAction::AbOff => "A/B off",
        OverloadAction::Reverted => "back to the last setting that ran",
        OverloadAction::UpsamplerOff => "upsampler off",
        OverloadAction::LightUpsampler => "lightest upsampler",
        OverloadAction::Bypass => "bypass",
        OverloadAction::None => "none",
        OverloadAction::Recommended => "Recommended",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual_up(target: RateTarget, taps: usize) -> UpsamplerConfig {
        audivane_dsp_core::resample::ManualConfig {
            target,
            taps,
            ..Default::default()
        }
        .into_upsampler_config()
        .unwrap()
    }

    fn heavy() -> EngineConfig {
        let mut cfg = EngineConfig::default();
        cfg.chain.upsampler = manual_up(RateTarget::Ratio(16), 4 << 20);
        cfg
    }

    fn light() -> EngineConfig {
        let mut cfg = EngineConfig::default();
        cfg.chain.upsampler = manual_up(RateTarget::Ratio(4), 4095);
        cfg
    }

    fn everything_plays(_: &EngineConfig) -> Landing {
        Landing::Plays
    }

    /// A device with only rates above the bypass resampler's (or with
    /// `bypass_resampler: pipewire`): only upsampling plays.
    fn only_high_rates(cfg: &EngineConfig) -> Landing {
        if cfg.dsp_enabled && cfg.chain.upsampler.target != RateTarget::Off {
            Landing::Plays
        } else {
            Landing::None
        }
    }

    /// A DAC that plays only 48 kHz: nothing upsampled lands, x1 and DSP
    /// off do.
    fn only_48k(cfg: &EngineConfig) -> Landing {
        if cfg.dsp_enabled && cfg.chain.upsampler.target != RateTarget::Off {
            Landing::None
        } else {
            Landing::Plays
        }
    }

    /// A device that plays the request ([`heavy`]) and nothing lighter.
    fn only_the_request(cfg: &EngineConfig) -> Landing {
        if *cfg == heavy() {
            Landing::Plays
        } else {
            Landing::None
        }
    }

    /// A test engine: runs what it is given, as the next generation; the
    /// chain lands at once unless `lag` is set.
    struct Rig {
        sup: Supervisor,
        s: Sample,
        generation: u64,
        running: Option<EngineConfig>,
        reports: Vec<OverloadReport>,
        refuse: Option<fn(&EngineConfig) -> bool>,
        lag: bool,
        confirmed: usize,
        exhausted: usize,
        ended: usize,
        /// The varispeed hold the engine was given (as the service syncs
        /// it), and the varispeed steps ended by a new request.
        hold: VarispeedHold,
        vs_ended: usize,
        /// The hold's sequence, and whether the audio thread is slow to
        /// acknowledge it (`ack_lag`: [`Self::ack`] does).
        hold_seq: u64,
        ack_lag: bool,
    }

    impl Rig {
        fn new(cfg: &EngineConfig) -> Self {
            let mut rig = Self::idle();
            rig.edit(cfg).unwrap();
            rig
        }

        fn idle() -> Self {
            Self {
                sup: Supervisor::default(),
                s: Sample {
                    output_rate: 768_000,
                    ratio: RateRatio::integer(16),
                    source_rate: 48_000,
                    ..Sample::default()
                },
                generation: 0,
                running: None,
                reports: Vec::new(),
                refuse: None,
                lag: false,
                confirmed: 0,
                exhausted: 0,
                ended: 0,
                hold: VarispeedHold::None,
                vs_ended: 0,
                hold_seq: 0,
                ack_lag: false,
            }
        }

        /// The engine takes the fallback's hold: a pinned budget reads
        /// 0.5, a retired stage no longer runs.
        fn sync_hold(&mut self) {
            let seq = self.set_hold(self.sup.varispeed_hold());
            self.sup.note_hold_seq(seq);
        }

        /// As the engine: a new hold takes the next sequence, which the
        /// audio thread acknowledges (at once unless `ack_lag`).
        fn set_hold(&mut self, hold: VarispeedHold) -> u64 {
            if hold != self.hold {
                self.hold_seq += 1;
            }
            if !self.ack_lag {
                self.ack();
            }
            self.hold = hold;
            match self.hold {
                VarispeedHold::None => {}
                VarispeedHold::Budget | VarispeedHold::Unity => self.s.varispeed.budget = 0.5,
                VarispeedHold::Off => self.s.varispeed.running = false,
            }
            self.hold_seq
        }

        /// The audio thread applies the latest hold.
        fn ack(&mut self) {
            self.s.varispeed.hold_ack = self.hold_seq;
        }

        /// A client edit: plan, apply (maybe refused), commit.
        fn edit(&mut self, cfg: &EngineConfig) -> Result<bool, ()> {
            let eff = self.sup.plan(cfg);
            if self.refuse.is_some_and(|r| r(&eff)) {
                return Err(());
            }
            self.generation += 1;
            self.running = Some(eff);
            if !self.lag {
                self.s.chain_generation = self.generation;
            }
            let ended = self.sup.commit_request(cfg, self.generation);
            self.sync_hold();
            Ok(ended)
        }

        fn handle(&mut self, step: Step, cfg: &EngineConfig, plays: fn(&EngineConfig) -> Landing) {
            self.handle_step(step, cfg, plays);
            self.sync_hold();
        }

        fn handle_step(
            &mut self,
            mut step: Step,
            cfg: &EngineConfig,
            plays: fn(&EngineConfig) -> Landing,
        ) {
            loop {
                match step {
                    Step::Nothing => return,
                    Step::Ended => {
                        self.vs_ended += 1;
                        return;
                    }
                    Step::Try(t) if t.varispeed_hold().is_some() => {
                        // As the service: no config, the chain's generation
                        // and the hold's sequence.
                        let seq = self.set_hold(t.varispeed_hold().unwrap_or_default());
                        let generation = self.sup.expected_generation();
                        let (_, report) = self.sup.commit_varispeed(t, cfg, generation, seq);
                        self.reports.push(report);
                        return;
                    }
                    Step::Confirmed => {
                        self.confirmed += 1;
                        return;
                    }
                    Step::Exhausted { .. } => {
                        self.exhausted += 1;
                        return;
                    }
                    Step::End | Step::RateLeft { .. } => {
                        self.generation += 1;
                        self.running = Some(cfg.clone());
                        if !self.lag {
                            self.s.chain_generation = self.generation;
                        }
                        self.sup.end(cfg, self.generation);
                        self.ended += 1;
                        return;
                    }
                    Step::Try(t) => {
                        let eff = self.sup.config_for(&t, cfg);
                        if self.refuse.is_some_and(|r| r(&eff)) {
                            step = self.sup.rejected(t, cfg, &plays);
                            continue;
                        }
                        self.generation += 1;
                        self.running = Some(eff);
                        if !self.lag {
                            self.s.chain_generation = self.generation;
                        }
                        let (_, report) = self.sup.commit(t, cfg, self.generation);
                        self.reports.push(report);
                        return;
                    }
                }
            }
        }

        /// `n` samples at `load`, with `xruns` new underruns per sample out
        /// of `periods` processed.
        fn feed(
            &mut self,
            cfg: &EngineConfig,
            n: usize,
            load: f64,
            xruns: u64,
            plays: fn(&EngineConfig) -> Landing,
        ) {
            for _ in 0..n {
                self.s.running = true;
                self.s.load = load;
                self.s.xrun_events += xruns;
                self.s.periods += 5;
                let step = self.sup.observe(self.s, cfg, &plays);
                self.handle(step, cfg, plays);
            }
        }

        /// A demotion of the audio thread while the chain in effect runs
        /// (the driver publishes its generation with it).
        fn demote(&mut self) {
            self.s.rt_signals += 1;
            self.s.rt_overloads += 1;
            self.s.rt_overload_generation = self.s.chain_generation;
            self.s.rt_overload_hold_seq = self.s.varispeed.hold_ack;
        }

        /// The engine ends in `Error` after an open that failed, and the
        /// driver's probe after it found the device.
        fn fail_open(&mut self) {
            self.s.error = true;
            self.s.running = false;
            self.s.probes += 1;
        }

        /// `n` samples while not playing (the rest of the sample as set).
        fn wait(&mut self, cfg: &EngineConfig, n: usize, plays: fn(&EngineConfig) -> Landing) {
            for _ in 0..n {
                self.s.running = false;
                let step = self.sup.observe(self.s, cfg, &plays);
                self.handle(step, cfg, plays);
            }
        }

        /// The device moves to `ratio` on the same 48 kHz graph rate.
        fn set_ratio(&mut self, ratio: RateRatio) {
            self.s.ratio = ratio;
            self.s.output_rate = ratio.output_rate(48_000).unwrap_or(48_000);
        }

        fn actions(&self) -> Vec<OverloadAction> {
            self.reports.iter().map(|r| r.action).collect()
        }

        fn runs(&self) -> &EngineConfig {
            self.running.as_ref().unwrap()
        }
    }

    /// Samples to the first trip after an edit, and to the next one after a
    /// step (its settling, then a loaded second).
    const FIRST: usize = SETTLE / 2 + LOADED_SAMPLES;
    const NEXT: usize = SETTLE + LOADED_SAMPLES + 1;

    /// Varispeed's stage running at `speed`, the load having lowered its
    /// budget to `budget`.
    fn varispeed_at(rig: &mut Rig, speed: f64, budget: f64) {
        rig.s.varispeed = VarispeedRequest {
            wanted: true,
            running: true,
            speed,
            budget,
            requests: rig.s.varispeed.requests + 1,
            hold_ack: rig.s.varispeed.hold_ack,
        };
    }

    #[test]
    fn varispeed_steps_come_before_the_chain_s() {
        let cfg = heavy();
        let mut rig = Rig::new(&cfg);
        varispeed_at(&mut rig, 1.5, 0.25);
        rig.feed(&cfg, FIRST, 1.4, 0, everything_plays);
        assert_eq!(rig.actions(), vec![OverloadAction::VarispeedBudget]);
        assert_eq!(rig.hold, VarispeedHold::Budget);
        rig.feed(&cfg, 1, 1.4, 0, everything_plays);
        // No config changes: the step's attempt is the chain that runs.
        assert_eq!(rig.generation, 1);
        assert_eq!(*rig.runs(), cfg);
        assert_eq!(rig.confirmed, 1);
        assert_eq!(rig.reports[0].stage, "varispeed");
        assert!(rig.reports[0].message.contains("half of each period"));
        rig.feed(&cfg, 2 * NEXT, 1.4, 0, everything_plays);
        assert_eq!(
            rig.actions(),
            vec![
                OverloadAction::VarispeedBudget,
                OverloadAction::VarispeedUnity,
                OverloadAction::VarispeedOff,
            ]
        );
        assert_eq!(rig.hold, VarispeedHold::Off);
        assert!(rig.reports[1].message.contains("1.50×"));
        assert!(rig.reports[2].message.contains("even at 1×"));
        assert_eq!(rig.generation, 1);
        // Then the chain's steps, with the stage kept off.
        rig.feed(&cfg, 2 * NEXT, 1.4, 0, everything_plays);
        assert_eq!(
            rig.actions()[3..],
            [OverloadAction::UpsamplerOff, OverloadAction::Bypass]
        );
        assert_eq!(rig.hold, VarispeedHold::Off);
        assert!(
            rig.reports[3]
                .message
                .ends_with("Varispeed stays off while this runs.")
        );
        assert!(!rig.runs().dsp_enabled);
    }

    #[test]
    fn a_saved_heavy_config_turns_the_upsampler_off_then_bypasses() {
        let cfg = heavy();
        let mut rig = Rig::new(&cfg);
        // Startup settle, then a second of load over real time.
        rig.feed(&cfg, SETTLE / 2 + LOADED_SAMPLES, 1.4, 0, everything_plays);
        assert_eq!(rig.actions(), [OverloadAction::UpsamplerOff]);
        let report = &rig.reports[0];
        assert_eq!(
            report.message,
            "The engine switched the upsampler off: it needed more CPU than real time allows \
             (load 140%). Your setting is kept; lower the taps or the output rate."
        );
        assert_eq!(report.stage, "upsampler");
        assert!(!report.applied, "published before the chain runs it");
        assert_eq!(rig.runs().chain.upsampler.target, RateTarget::Off);
        assert!(rig.runs().dsp_enabled);
        // The next sample sees the step's chain running: applied.
        rig.set_ratio(RateRatio::ONE);
        rig.feed(&cfg, 1, 0.9, 0, everything_plays);
        assert_eq!(rig.confirmed, 1);
        assert!(rig.sup.report().unwrap().applied);
        // Still overloaded without it (most periods underrun): bypass.
        rig.feed(&cfg, SETTLE + 4, 0.9, 1, everything_plays);
        assert_eq!(
            rig.actions(),
            [OverloadAction::UpsamplerOff, OverloadAction::Bypass]
        );
        assert!(!rig.runs().dsp_enabled);
        // Nothing lighter: logged once, then quiet.
        rig.feed(&cfg, 100, 1.2, 0, everything_plays);
        assert_eq!(rig.exhausted, 1);
    }

    #[test]
    fn falls_back_to_the_last_setting_that_ran() {
        let good = light();
        let mut rig = Rig::new(&good);
        rig.set_ratio(RateRatio::integer(4));
        rig.feed(&good, SETTLE / 2 + GOOD_AFTER, 0.3, 0, everything_plays);
        assert!(rig.reports.is_empty());
        let bad = heavy();
        rig.edit(&bad).unwrap();
        assert_eq!(rig.runs(), &bad);
        rig.set_ratio(RateRatio::integer(16));
        rig.feed(&bad, SETTLE / 2 + LOADED_SAMPLES, 1.5, 0, everything_plays);
        assert_eq!(rig.actions(), [OverloadAction::Reverted]);
        assert_eq!(rig.runs().chain.upsampler, good.chain.upsampler);
        // An EQ edit applies on top of the fallback.
        let mut eq = bad.clone();
        eq.chain.eq = Some(Default::default());
        assert_eq!(rig.edit(&eq), Ok(false));
        assert_eq!(rig.runs().chain.upsampler, good.chain.upsampler);
        assert_eq!(rig.runs().chain.eq, eq.chain.eq);
        // A new upsampler setting ends the fallback.
        let mut retry = eq.clone();
        retry.chain.upsampler = manual_up(RateTarget::Ratio(16), 8191);
        assert_eq!(rig.edit(&retry), Ok(true));
        assert_eq!(rig.runs(), &retry);
        assert_eq!(rig.sup.report(), None);
    }

    #[test]
    fn short_bursts_and_idle_are_not_overloads() {
        let cfg = heavy();
        let mut rig = Rig::new(&cfg);
        rig.feed(&cfg, 10, 0.5, 0, everything_plays);
        rig.feed(&cfg, 5, 1.3, 0, everything_plays);
        rig.feed(&cfg, 50, 0.6, 0, everything_plays);
        // Two underruns (a reopen) are no overload.
        rig.feed(&cfg, 2, 0.6, 1, everything_plays);
        rig.feed(&cfg, 50, 0.6, 0, everything_plays);
        assert!(rig.reports.is_empty());
        // Not running: never judged.
        for _ in 0..50 {
            rig.s.running = false;
            rig.s.load = 2.0;
            assert_eq!(
                rig.sup.observe(rig.s, &cfg, &everything_plays),
                Step::Nothing
            );
        }
    }

    #[test]
    fn when_not_even_the_bypass_lands_the_rate_is_kept_with_the_light_filter() {
        // Rates only above what the bypass resampler converts to: the
        // bypass resampler's design at the requested target, with the chain.
        let cfg = heavy();
        let mut rig = Rig::new(&cfg);
        rig.feed(&cfg, SETTLE / 2 + LOADED_SAMPLES, 1.4, 0, only_high_rates);
        assert_eq!(rig.actions(), [OverloadAction::LightUpsampler]);
        let light = cheapest_upsampler(&cfg.chain.upsampler);
        assert_eq!(rig.runs().chain.upsampler, light);
        assert_eq!(light.target, RateTarget::Ratio(16));
        assert_eq!(
            light.preset,
            super::super::config::bypass_upsampler().preset
        );
        assert!(rig.runs().dsp_enabled);
        rig.feed(&cfg, 1 + SETTLE + LOADED_SAMPLES, 1.2, 0, only_high_rates);
        assert_eq!(rig.actions(), [OverloadAction::LightUpsampler]);
        assert_eq!(rig.exhausted, 1);
    }

    #[test]
    fn a_step_that_ends_in_error_moves_to_the_next_one() {
        let cfg = heavy();
        let mut rig = Rig::new(&cfg);
        // The upsampler-off step needs a reopen that fails.
        rig.s.device = 1;
        rig.lag = true;
        rig.feed(&cfg, SETTLE / 2 + LOADED_SAMPLES, 1.4, 0, everything_plays);
        assert_eq!(rig.actions(), [OverloadAction::UpsamplerOff]);
        rig.s.running = false;
        rig.s.error = false;
        let step = rig.sup.observe(rig.s, &cfg, &everything_plays);
        rig.handle(step, &cfg, everything_plays);
        // In error with no probe since (the device not answering): waited.
        rig.s.error = true;
        rig.wait(&cfg, 3 * SETTLE, everything_plays);
        assert_eq!(rig.actions(), [OverloadAction::UpsamplerOff]);
        // The driver's probe after the failed open found the device.
        rig.fail_open();
        let step = rig.sup.observe(rig.s, &cfg, &everything_plays);
        rig.handle(step, &cfg, everything_plays);
        // Next: the bypass path, applied and landing.
        assert_eq!(
            rig.actions(),
            [OverloadAction::UpsamplerOff, OverloadAction::Bypass]
        );
        rig.lag = false;
        rig.s.error = false;
        rig.s.chain_generation = rig.generation;
        let step = rig.sup.observe(rig.s, &cfg, &everything_plays);
        rig.handle(step, &cfg, everything_plays);
        assert_eq!(rig.confirmed, 1);
        assert_eq!(rig.sup.report().unwrap().action, OverloadAction::Bypass);
    }

    /// Upsampler off, confirmed on a 48 kHz DAC.
    fn off_confirmed_on_48k(cfg: &EngineConfig) -> Rig {
        let mut rig = Rig::new(cfg);
        rig.s.device = 1;
        rig.feed(cfg, SETTLE / 2 + LOADED_SAMPLES, 1.4, 0, only_48k);
        assert_eq!(rig.actions(), [OverloadAction::UpsamplerOff]);
        rig.set_ratio(RateRatio::ONE);
        rig.s.output_rate = 48_000;
        rig.feed(cfg, 5, 0.3, 0, only_48k);
        assert_eq!(rig.confirmed, 1);
        rig
    }

    #[test]
    fn only_a_real_demotion_is_an_overload() {
        let cfg = heavy();
        let mut rig = Rig::new(&cfg);
        rig.feed(&cfg, SETTLE / 2, 0.5, 0, everything_plays);
        // Signals that found the thread at normal priority, and demotions
        // the kernel refused: no fallback.
        for _ in 0..50 {
            rig.s.rt_signals += 1;
            rig.s.rt_failed_demotions += 1;
            rig.feed(&cfg, 1, 0.5, 0, everything_plays);
        }
        assert!(rig.reports.is_empty());
        // A real transition to normal priority is.
        rig.demote();
        rig.feed(&cfg, 1, 0.5, 0, everything_plays);
        assert_eq!(rig.actions(), [OverloadAction::UpsamplerOff]);
    }

    /// Upsampling plays (a revert or the light filter), x1 and DSP off do
    /// not.
    fn only_upsampling(cfg: &EngineConfig) -> Landing {
        only_high_rates(cfg)
    }

    #[test]
    fn a_replan_tries_every_step_once_then_ends_and_retries_a_refused_end() {
        let (good, bad) = (light(), heavy());
        let mut rig = Rig::new(&good);
        rig.s.device = 1;
        rig.set_ratio(RateRatio::integer(4));
        rig.feed(&good, SETTLE / 2 + GOOD_AFTER, 0.3, 0, everything_plays);
        rig.edit(&bad).unwrap();
        rig.set_ratio(RateRatio::integer(16));
        // Back to the calm setting, still overloaded: upsampler off.
        rig.feed(&bad, SETTLE / 2 + LOADED_SAMPLES, 1.5, 0, everything_plays);
        rig.feed(&bad, 1 + SETTLE + LOADED_SAMPLES, 1.5, 0, everything_plays);
        assert_eq!(
            rig.actions(),
            [OverloadAction::Reverted, OverloadAction::UpsamplerOff]
        );
        rig.feed(&bad, 2, 0.3, 0, everything_plays);
        // A DAC where only upsampling plays: the light filter first ...
        rig.s.device = 2;
        rig.s.probes += 1;
        rig.feed(&bad, 1, 0.3, 0, only_upsampling);
        assert_eq!(rig.actions().last(), Some(&OverloadAction::LightUpsampler));
        // ... whose open fails: the calm setting at the top is tried too.
        rig.fail_open();
        rig.wait(&bad, SETTLE, only_upsampling);
        assert_eq!(rig.actions().last(), Some(&OverloadAction::Reverted));
        // It fails as well: nothing is left on this DAC, the fallback ends.
        rig.fail_open();
        rig.wait(&bad, SETTLE, only_upsampling);
        assert_eq!(rig.actions().len(), 4);
        assert_eq!(rig.ended, 1);
        assert_eq!(rig.sup.report(), None);
        assert_eq!(rig.runs(), &bad);

        // A refused end: kept pending, asked again after the settle.
        let mut rig = off_confirmed_on_48k(&bad);
        rig.s.device = 2;
        assert_eq!(rig.sup.observe(rig.s, &bad, &only_the_request), Step::End);
        assert!(rig.sup.end_refused());
        for _ in 0..SETTLE - 1 {
            assert_eq!(
                rig.sup.observe(rig.s, &bad, &only_the_request),
                Step::Nothing
            );
        }
        assert_eq!(rig.sup.observe(rig.s, &bad, &only_the_request), Step::End);
        assert!(!rig.sup.end_refused(), "logged once in a row");
        assert!(rig.sup.report().is_some());
    }
}
