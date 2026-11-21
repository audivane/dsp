//! Varispeed: plays the source at speed `s` (tempo and pitch together, as on a
//! tape machine) at the graph rate G.
//!
//! ```text
//! source (G, pulled at s·G) ──► ×2, steep ──► adaptive core at 2G, step s ──► ÷2, steep ──► G
//! ```
//!
//! The ×2 and ÷2 are the catalogue's built-in `varispeed` design
//! ([`catalogue::varispeed`]), prepared by the filter design builder at 2/1 and
//! 1/2 and run on the existing [`RationalResampler`]: Kaiser, linear phase,
//! passband 0.907 of the Nyquist (0.8 above 192 kHz), stopband at the Nyquist,
//! 150 dB ([`SteepDesign`]). A user design can replace it
//! ([`SteepDesign::from_design`], [`Varispeed::with_design`]; not in the
//! protocol yet). The core ([`crate::adaptive`]) needs only 25 taps at every
//! speed in `[0.5, 2]`, because the ×2 leaves nothing between a quarter and
//! three quarters of its rate. Speed commands go through the [`envelope`]: C¹
//! curves in log speed, cubic pieces of the step (at most [`PIECE_MAX`] outputs
//! each). A cap the curve cannot respect is met by a short fade, a jump under
//! silence and a fade back ([`Varispeed::set_speed_limit`]).
//!
//! **Pull contract.** The caller asks for exactly `n` output frames:
//! [`Varispeed::plan`] says how many source frames that takes (`n_src`) and
//! latches the call; [`Varispeed::process`] follows with exactly `n_src` source
//! frames and writes exactly `n` frames. A `plan` without a `process` is
//! discarded by the next `plan`; a zero call consumes and changes nothing and a
//! pending command waits for the next call with `n > 0`.
//! [`Varispeed::max_source_frames`] gives `n_src` before the plan, and
//! [`Varispeed::peak_until_settled`] the highest speed the stage will apply
//! before it settles.
//!
//! **Follower mode** (multi-device):
//! [`Varispeed::set_follower`] applies a follower's clock correction ε to every
//! step the envelope plans, so the core runs at `v (1 + ε)` while the envelope,
//! the commands, the cap and the fades work on `v` exactly as on the master's
//! stage. Each call takes ε from the last call's end to the target
//! [`Varispeed::set_epsilon_target`] gives for its end, as C¹ cubic pieces
//! ([`crate::drift::EpsilonTrack`]). Off, the stage is unchanged, bit for bit.
//!
//! **Real time.** [`Varispeed::new`] allocates everything for calls of up to
//! `max_frames` (at most [`MAX_FRAMES`]) at speed 2. `plan`, `process`, `reset`
//! and the commands never allocate, lock or block. `process` panics only on
//! caller contract breaks (no plan, wrong slice lengths).
//!
//! **Latency** at constant speed `s`: `(d_up + d_core) / (2 s) + d_down / 2`
//! output frames (`136.75 / s + 130.75` with the built-in kernels).
//! [`Varispeed::latency_exact`] stays false until the step has been constant
//! over the filters' whole span.

pub mod envelope;

use crate::adaptive::{
    AdaptiveCore, CoreError, MAX_CHANNELS, MAX_OVERPULL, PlanError, Segment, Step,
};
use crate::design::{self, BuildContext, Cutoff, FilterDesign, KernelDesign, Shape, catalogue};
use crate::drift::{EPS_MAX, EpsilonTrack, hermite};
use crate::rational::RationalResampler;
use audivane_protocol::RateRatio;
use envelope::Envelope;

/// Slowest speed a command may ask for.
pub const MIN_SPEED: f64 = 0.5;
/// Fastest speed a command may ask for (the core's largest step).
pub const MAX_SPEED: f64 = 2.0;
/// Longest ramp a command may ask for, milliseconds.
pub const MAX_RAMP_MS: f64 = 10_000.0;
/// Most output frames per call (the quantum limit): the core takes at most
/// twice this many outputs per call.
pub const MAX_FRAMES: usize = crate::adaptive::MAX_CALL_OUTPUTS / 2;
/// Longest envelope piece, core outputs (1024 output frames, the period
/// the envelope's quality figures were measured at): a longer call runs
/// several pieces.
pub const PIECE_MAX: usize = 2048;
/// Most core calls one stage call can take: the pieces of a call of
/// [`MAX_FRAMES`] (some cut shorter to stay in range), a brake's end, a
/// ramp's end, a jump and the hold after it. Beyond it the call falls back
/// to a jump (never reached in any test).
const MAX_PARTS: usize = 48;
/// Each fade of a cap drop (out, then in), milliseconds: the engine's own
/// swap fade half (`SWAP_FADE_HALF_MS` in the processor).
pub const CAP_FADE_MS: f64 = 5.0;
/// Steep filters' passband end, fraction of the Nyquist.
pub const STEEP_PASSBAND: f64 = 0.907;
/// The same above [`RELAXED_ABOVE_HZ`] (ultrasonic there; halves the cost).
pub const RELAXED_PASSBAND: f64 = 0.8;
/// Graph rates above this use [`RELAXED_PASSBAND`].
pub const RELAXED_ABOVE_HZ: u32 = 192_000;
/// Steep filters' stopband start, fraction of the Nyquist.
pub const STEEP_STOPBAND: f64 = 1.0;
/// Steep filters' stopband attenuation, dB.
pub const STEEP_ATTENUATION_DB: f64 = 150.0;

/// Why a stage was not built or a command was refused. A refused command
/// changes nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum VarispeedError {
    /// The graph rate must be at least 1 Hz.
    Rate(u32),
    /// `channels` must be `1..=MAX_CHANNELS`.
    Channels(usize),
    /// `max_frames` must be `1..=MAX_FRAMES`.
    MaxFrames(usize),
    /// A steep filter did not design.
    Design(String),
    /// The core did not build.
    Core(CoreError),
    /// A speed outside `MIN_SPEED..=MAX_SPEED` (or not finite).
    Speed(f64),
    /// A ramp outside `0..=MAX_RAMP_MS` (or not finite).
    Ramp(f64),
    /// More frames in one call than the stage was built for.
    TooManyFrames { n: usize, max: usize },
    /// The core refused the call (a guard; not reachable within
    /// `max_frames`).
    Plan(PlanError),
}

impl std::fmt::Display for VarispeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rate(r) => write!(f, "graph rate {r} Hz"),
            Self::Channels(c) => write!(f, "{c} channels, must be 1 to {MAX_CHANNELS}"),
            Self::MaxFrames(n) => write!(f, "{n} frames per call, must be 1 to {MAX_FRAMES}"),
            Self::Design(e) => write!(f, "varispeed filter design: {e}"),
            Self::Core(e) => write!(f, "varispeed core: {e}"),
            Self::Speed(s) => write!(f, "speed {s}, must be {MIN_SPEED} to {MAX_SPEED}"),
            Self::Ramp(r) => write!(f, "ramp {r} ms, must be 0 to {MAX_RAMP_MS}"),
            Self::TooManyFrames { n, max } => write!(f, "{n} frames in one call, at most {max}"),
            Self::Plan(e) => write!(f, "varispeed core: {e}"),
        }
    }
}

impl std::error::Error for VarispeedError {}

/// The steep ×2 and ÷2 of the stage, on the 2G grid: the catalogue's
/// built-in design ([`catalogue::varispeed`]) prepared by the filter
/// design builder ([`design::prepare`]) at the ratios 2/1 and 1/2, or a
/// user design ([`SteepDesign::from_design`]).
#[derive(Clone, Debug, PartialEq)]
pub struct SteepDesign {
    /// Passband end, fraction of the Nyquist, when the design states one
    /// (a band cutoff).
    pub passband_end: Option<f64>,
    /// The ×2 kernel (each of its two branches sums to about 1).
    pub up: Vec<f64>,
    /// The ÷2 kernel (sums to about 1).
    pub down: Vec<f64>,
    /// The kernels' delays, 2G frames.
    up_delay: f64,
    down_delay: f64,
}

impl SteepDesign {
    /// The passband the stage uses at graph rate `rate`.
    pub fn passband_for_rate(rate: u32) -> f64 {
        if rate > RELAXED_ABOVE_HZ {
            RELAXED_PASSBAND
        } else {
            STEEP_PASSBAND
        }
    }

    /// The built-in design for graph rate `rate`.
    pub fn for_rate(rate: u32) -> Result<Self, VarispeedError> {
        Self::from_design(
            &catalogue::varispeed(Self::passband_for_rate(rate)),
            rate.max(1),
        )
    }

    /// The built-in design with passband end `passband_end` (stopband at
    /// the Nyquist, 150 dB, Kaiser, linear phase).
    pub fn with_passband(passband_end: f64) -> Result<Self, VarispeedError> {
        Self::from_design(&catalogue::varispeed(passband_end), 48_000)
    }

    /// The steep filters of `design` for graph rate `rate`: its kernels at
    /// the ratios 2/1 (from `rate`) and 1/2 (from `2 · rate`), through
    /// [`design::prepare`] (its rules, budgets and safety gate). A windowed
    /// sinc or Lanczos design that builds at both ratios is accepted; the
    /// runtime kinds (cascade, interpolator, block FFT) are refused. Not in
    /// the protocol yet.
    pub fn from_design(design: &FilterDesign, rate: u32) -> Result<Self, VarispeedError> {
        if !matches!(design, FilterDesign::Kernel(_)) {
            return Err(VarispeedError::Design(format!(
                "a {} design has no kernel; varispeed takes a windowed sinc or Lanczos design",
                design.kind_name()
            )));
        }
        if rate == 0 {
            return Err(VarispeedError::Rate(rate));
        }
        let kernel = |source_rate: u32, ratio: RateRatio| -> Result<_, VarispeedError> {
            let cx = BuildContext {
                source_rate,
                ratio,
                channels: 1,
                max_block: 1024,
                pipelines: 1,
                output: audivane_protocol::OutputConfig::default(),
                workspace: design::budget::design_workspace(),
            };
            let p = design::prepare(design, &cx)
                .map_err(|e| VarispeedError::Design(format!("at {ratio}: {e}")))?;
            p.kernel
                .ok_or_else(|| VarispeedError::Design(format!("at {ratio}: no kernel")))
        };
        let up = kernel(rate, RateRatio::new(2, 1))?;
        let down = kernel(rate.saturating_mul(2), RateRatio::new(1, 2))?;
        let passband_end = match design {
            FilterDesign::Kernel(KernelDesign {
                shape:
                    Shape::WindowedSinc {
                        cutoff: Cutoff::Band(b),
                        ..
                    },
                ..
            }) => Some(b.passband_end),
            _ => None,
        };
        Ok(Self {
            passband_end,
            up_delay: up.delay_exact,
            down_delay: down.delay_exact,
            up: up.taps,
            down: down.taps,
        })
    }

    /// The ×2 kernel's delay, 2G frames (linear phase: its centre).
    pub fn up_delay(&self) -> f64 {
        self.up_delay
    }

    /// The ÷2 kernel's delay, 2G frames.
    pub fn down_delay(&self) -> f64 {
        self.down_delay
    }
}

/// Multiply-adds per second and channel of a stage at graph rate `rate`
/// running at speed `s` (the ×2 per source frame, the core's four
/// rows per 2G output, the ÷2), without building one. The static budget
/// counts it at `s = 2` (`worst_case_macs_per_second`).
pub fn macs_per_second(rate: u32, s: f64) -> Result<f64, VarispeedError> {
    if rate == 0 {
        return Err(VarispeedError::Rate(rate));
    }
    let d = SteepDesign::for_rate(rate)?;
    let taps = crate::adaptive::CoreTable::shared()
        .map_err(|e| VarispeedError::Design(e.to_string()))?
        .taps();
    let up = 2.0 * d.up.len().div_ceil(2) as f64;
    let per_frame = up * s + 2.0 * 4.0 * taps as f64 + d.down.len() as f64;
    Ok(per_frame * f64::from(rate))
}

/// [`macs_per_second`] at the fastest speed: what the static budget adds
/// for varispeed once (one stage feeds every chain).
pub fn worst_case_macs_per_second(rate: u32) -> Result<f64, VarispeedError> {
    macs_per_second(rate, MAX_SPEED)
}

/// What a stage is built for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VarispeedSpec {
    /// Graph rate G, Hz (the stage's input and output rate).
    pub sample_rate: u32,
    pub channels: usize,
    /// Most output frames per call (`1..=MAX_FRAMES`).
    pub max_frames: usize,
    /// The speed it starts at (and holds until a command).
    pub speed: f64,
}

/// A speed command waiting for the next call with `n > 0`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Command {
    target: Step,
    /// Ramp length asked for, core outputs.
    ramp: f64,
    seq: u64,
}

/// One core call within a stage call.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Part {
    outputs: usize,
    seg: Segment,
    /// New 2G frames it needs.
    need: usize,
}

const NO_PART: Part = Part {
    outputs: 0,
    seg: Segment::Hold,
    need: 0,
};

/// The output's gain around a cap drop (see [`Varispeed::set_speed_limit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fade {
    Open,
    /// Fading out; `done` frames of the fade played.
    Out {
        done: usize,
    },
    /// Silent for `left` more frames. The step jumps at the first muted
    /// frame (`jumped` false until then); a lower target during the
    /// silence jumps again, and the deadline may move `extend` more frames
    /// later to cover its kink (one ÷2 span in all).
    Mute {
        left: usize,
        jumped: bool,
        extend: usize,
    },
    /// Fading in; `done` frames played.
    In {
        done: usize,
    },
}

impl Latch {
    /// What the call after this one starts from.
    fn base_after(&self) -> Base {
        Base {
            env: self.env,
            fade: self.fade_after,
            jump: self.jump_target,
            step: self.step_after,
        }
    }
}

/// A planned call: the core calls it runs, in order, and the state after.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Latch {
    n: usize,
    n_core: usize,
    n_src: usize,
    parts: [Part; MAX_PARTS],
    n_parts: usize,
    env: Envelope,
    /// The gain state at the call's first frame, and after the call.
    fade: Fade,
    fade_after: Fade,
    jump_target: Step,
    /// The command this call starts, if any.
    consumed: Option<u64>,
    /// The largest step the call applies, and the envelope's step after it
    /// (without ε).
    peak: f64,
    step_after: Step,
    /// Follower mode: ε after the call.
    track_after: Option<EpsilonTrack>,
    /// The core refused a piece and the call jumps to the target instead.
    fallback: bool,
    /// The core's position after the call.
    pos_after: u128,
}

/// The varispeed stage (see the module docs).
pub struct Varispeed {
    rate: u32,
    channels: usize,
    max_frames: usize,
    design: SteepDesign,
    up: RationalResampler,
    core: AdaptiveCore,
    down: RationalResampler,
    /// Per channel: the ×2 output, then the core output.
    mid: Vec<Vec<f64>>,
    core_out: Vec<Vec<f64>>,
    env: Envelope,
    /// The speed last asked for, and the ramp it asked for (ms).
    requested: f64,
    requested_ramp_ms: f64,
    /// The scheduler's cap on the speed.
    limit: f64,
    command: Option<Command>,
    seq: u64,
    latched: Option<Latch>,
    fade: Fade,
    /// Where a cap drop jumps to.
    jump_target: Step,
    /// Frames per fade, and muted frames after a jump.
    fade_len: usize,
    mute_len: usize,
    /// Core outputs since the step last changed.
    steady: u64,
    /// Calls on which the core refused the envelope's piece and the stage
    /// jumped to the target instead (never for in-range commands).
    fallbacks: u64,
    /// The envelope's step at the next output: the core's, without ε.
    vstep: Step,
    /// Follower mode: ε at the end of the last call, and the target for
    /// the next call's end.
    follow: Option<Follow>,
}

/// Follower mode's state (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Follow {
    track: EpsilonTrack,
    to: (f64, f64),
}

fn check_speed(speed: f64) -> Result<Step, VarispeedError> {
    if !(speed.is_finite() && (MIN_SPEED..=MAX_SPEED).contains(&speed)) {
        return Err(VarispeedError::Speed(speed));
    }
    Step::from_f64(speed).ok_or(VarispeedError::Speed(speed))
}

/// The steps a planned core call applies, over its integer outputs and the
/// step after it: the smallest and the largest, exact (64.64 bits).
pub(crate) fn applied_range(acc: &crate::adaptive::Accumulator) -> (u128, u128) {
    let p = acc.planned().expect("a plan");
    let n = p.outputs as i128;
    let [d1, d2, d3] = p.d;
    // v(k) = v0 + d1 k + d2 C(k,2) + d3 C(k,3), in integers.
    let v = |k: i128| -> u128 {
        let x = p.step as i128 + d1 * k + d2 * (k * (k - 1) / 2) + d3 * (k * (k - 1) * (k - 2) / 6);
        x.max(0) as u128
    };
    let (mut lo, mut hi) = (p.step.min(p.end), p.step.max(p.end));
    // v'(k) = d1 - d2/2 + d3/3 + (d2 - d3) k + d3 k²/2: its roots (in
    // `f64`, only to find them), and the integer outputs around each.
    let one = 18_446_744_073_709_551_616.0f64;
    let [f1, f2, f3] = p.d.map(|d| d as f64 / one);
    let (qa, qb, qc) = (f3 / 2.0, f2 - f3, f1 - f2 / 2.0 + f3 / 3.0);
    let mut roots = [f64::NAN; 2];
    if qa != 0.0 {
        let disc = qb * qb - 4.0 * qa * qc;
        if disc >= 0.0 {
            let r = disc.sqrt();
            roots = [(-qb + r) / (2.0 * qa), (-qb - r) / (2.0 * qa)];
        }
    } else if qb != 0.0 {
        roots[0] = -qc / qb;
    }
    for r in roots {
        if r > -1.0 && r < n as f64 + 1.0 {
            let k0 = r.floor() as i128;
            for k in (k0 - 1)..=(k0 + 2) {
                if (0..n).contains(&k) {
                    let x = v(k);
                    lo = lo.min(x);
                    hi = hi.max(x);
                }
            }
        }
    }
    (lo, hi)
}

pub(crate) fn bits_f64(bits: u128) -> f64 {
    bits as f64 / 18_446_744_073_709_551_616.0
}

/// Relative slack on the check that a piece follows its curve (the
/// integer differences' rounding is far below it). The stage's limits and
/// the cap are checked exactly, without it.
pub(crate) const RANGE_SLACK: f64 = 1e-9;

/// What a call starts from: the envelope, the gain state, the jump target
/// and the step at its first output.
#[derive(Clone, Copy, Debug)]
struct Base {
    env: Envelope,
    fade: Fade,
    jump: Step,
    step: Step,
}

/// A call's start, a pending command applied to its [`Base`].
#[derive(Clone, Copy, Debug)]
struct Start {
    env: Envelope,
    fade: Fade,
    jump: Step,
    /// The command it starts (`None` when there is none, or it waits).
    consumed: Option<u64>,
    /// A higher target during a cap drop's silence: it waits for the
    /// fade-in (the step is already within the cap).
    waiting: Option<Step>,
}

impl Varispeed {
    pub fn new(spec: VarispeedSpec) -> Result<Self, VarispeedError> {
        if spec.sample_rate == 0 {
            return Err(VarispeedError::Rate(spec.sample_rate));
        }
        Self::with_design(spec, SteepDesign::for_rate(spec.sample_rate)?)
    }

    /// [`Self::new`] with the steep filters `design` (from
    /// [`SteepDesign::from_design`] at `spec.sample_rate`) instead of the
    /// built-in ones. Not in the protocol yet.
    pub fn with_design(spec: VarispeedSpec, design: SteepDesign) -> Result<Self, VarispeedError> {
        if spec.sample_rate == 0 {
            return Err(VarispeedError::Rate(spec.sample_rate));
        }
        if !(1..=MAX_CHANNELS).contains(&spec.channels) {
            return Err(VarispeedError::Channels(spec.channels));
        }
        if !(1..=MAX_FRAMES).contains(&spec.max_frames) {
            return Err(VarispeedError::MaxFrames(spec.max_frames));
        }
        let speed = check_speed(spec.speed)?;
        let (ch, n) = (spec.channels, spec.max_frames);
        // At most 2n core outputs per call (the ÷2), and the core's input
        // (`2 n_src`) is at most its `max_in` = 4n + 5.
        let core = AdaptiveCore::new(ch, 2 * n, speed).map_err(VarispeedError::Core)?;
        let max_src = core.max_in().div_ceil(2);
        let up = RationalResampler::from_kernel(
            &design.up,
            RateRatio::new(2, 1),
            design.up_delay(),
            ch,
            max_src,
        );
        let down = RationalResampler::from_kernel(
            &design.down,
            RateRatio::new(1, 2),
            design.down_delay(),
            ch,
            2 * n,
        );
        let rate = f64::from(spec.sample_rate);
        Ok(Self {
            rate: spec.sample_rate,
            channels: ch,
            max_frames: n,
            mid: (0..ch).map(|_| vec![0.0; 2 * max_src]).collect(),
            core_out: (0..ch).map(|_| vec![0.0; 2 * n]).collect(),
            env: Envelope::new(2.0 * rate, speed),
            fade_len: ((CAP_FADE_MS * 1e-3 * rate).round() as usize).max(1),
            // The ÷2 spans `down.len()` core outputs, two per frame.
            mute_len: design.down.len().div_ceil(2) + 1,
            design,
            up,
            core,
            down,
            requested: speed.to_f64(),
            requested_ramp_ms: 0.0,
            limit: MAX_SPEED,
            command: None,
            seq: 0,
            latched: None,
            fade: Fade::Open,
            jump_target: speed,
            steady: u64::MAX / 2,
            fallbacks: 0,
            vstep: speed,
            follow: None,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn max_frames(&self) -> usize {
        self.max_frames
    }

    /// The steep filters in use.
    pub fn design(&self) -> &SteepDesign {
        &self.design
    }

    /// The core (its step and accumulator).
    pub fn core(&self) -> &AdaptiveCore {
        &self.core
    }

    /// The speed applied at the next output (what is audible).
    pub fn speed(&self) -> f64 {
        self.core.step().to_f64()
    }

    /// The applied speed's exact bits.
    pub fn step(&self) -> Step {
        self.core.step()
    }

    /// Follower mode on or off (see the module docs). Turning it on starts
    /// ε at zero; either way the core's step changes only at the next
    /// [`Self::reset`] or call.
    pub fn set_follower(&mut self, on: bool) {
        self.follow = on.then(|| Follow {
            track: EpsilonTrack::new(2.0 * f64::from(self.rate)),
            to: (0.0, 0.0),
        });
    }

    pub fn is_follower(&self) -> bool {
        self.follow.is_some()
    }

    /// Follower mode: ε and dε/dt (per second) at the end of the next call
    /// (clamped to ±[`EPS_MAX`]; not finite holds where it is). Ignored
    /// off follower mode.
    pub fn set_epsilon_target(&mut self, eps: f64, slope: f64) {
        if let Some(f) = self.follow.as_mut() {
            let (e, d) = if eps.is_finite() && slope.is_finite() {
                (eps.clamp(-EPS_MAX, EPS_MAX), slope)
            } else {
                (f.track.state().0, 0.0)
            };
            f.to = (e, d);
        }
    }

    /// Follower mode: ε and dε/dt (per second) at the next output; zero
    /// off it.
    pub fn epsilon(&self) -> (f64, f64) {
        self.follow.map_or((0.0, 0.0), |f| f.track.state())
    }

    /// The envelope's step at the next output (the master's, without ε;
    /// the core's off follower mode).
    pub fn envelope_step(&self) -> Step {
        self.vstep
    }

    /// The speed the stage is heading to: the last request, capped by the
    /// limit (a command not yet started included).
    pub fn target(&self) -> f64 {
        self.command
            .map_or(self.env.target(), |c| c.target)
            .to_f64()
    }

    /// The speed last asked for (before the limit).
    pub fn requested(&self) -> f64 {
        self.requested
    }

    /// The ramp last asked for with it, ms (a copy of the stage replays
    /// both: the same request again is ignored).
    pub fn requested_ramp_ms(&self) -> f64 {
        self.requested_ramp_ms
    }

    /// The lowest and highest step a call may apply (follower mode: the
    /// speed range and the cap widened by [`EPS_MAX`], within the core's
    /// [`Step::MAX`]): a plan that needs one outside it is refused.
    pub fn step_range(&self) -> (f64, f64) {
        let widen = if self.follow.is_some() { EPS_MAX } else { 0.0 };
        let hi = (self.limit.min(MAX_SPEED) * (1.0 + widen)).min(Step::MAX.to_f64());
        (MIN_SPEED * (1.0 - widen), hi)
    }

    /// The cap on the speed ([`Self::set_speed_limit`]).
    pub fn speed_limit(&self) -> f64 {
        self.limit
    }

    /// True while the speed is moving, a command waits to start, or a cap
    /// drop's fade is in progress.
    pub fn ramping(&self) -> bool {
        self.env.ramping() || self.command.is_some() || self.fade != Fade::Open
    }

    /// True while a cap drop's fade (out, silence, in) is in progress.
    pub fn fading(&self) -> bool {
        self.fade != Fade::Open
    }

    /// Counts the commands the stage has issued to its envelope: it moves
    /// when a [`Self::set_speed`] or [`Self::set_speed_limit`] call changed
    /// what the stage heads for (a repeated command leaves it), so a caller
    /// can tell which calls a copy of the stage must replay.
    pub fn command_seq(&self) -> u64 {
        self.seq
    }

    /// Calls on which the core refused a piece and the stage jumped to the
    /// target (a guard: zero for every in-range command).
    pub fn fallbacks(&self) -> u64 {
        self.fallbacks
    }

    /// Frames of each fade of a cap drop ([`CAP_FADE_MS`]).
    pub fn fade_frames(&self) -> usize {
        self.fade_len
    }

    /// Muted frames between a cap drop's fades: the ÷2's span, which the
    /// jump's kink must clear.
    pub fn mute_frames(&self) -> usize {
        self.mute_len
    }

    /// Output frames of delay at constant speed `s`.
    pub fn latency_at(&self, s: f64) -> f64 {
        (self.design.up_delay() + self.core.delay()) / (2.0 * s) + self.design.down_delay() / 2.0
    }

    /// Output frames of delay at the applied speed: exact at constant
    /// speed, an estimate while [`Self::latency_exact`] is false.
    pub fn latency_frames(&self) -> f64 {
        self.latency_at(self.speed())
    }

    /// Output frames of zero source after the last real one until the
    /// whole impulse response has played out at constant speed `s`: the
    /// filters' full spans, not their centres ([`Self::latency_at`] is
    /// about half of it), rounded up, plus a frame for the fractional
    /// position.
    pub fn tail_frames_at(&self, s: f64) -> usize {
        let s = if s.is_finite() && s > 0.0 { s } else { 1.0 };
        let up_core = (self.design.up.len() + self.core.table().taps()) as f64;
        (up_core / (2.0 * s) + self.design.down.len() as f64 / 2.0).ceil() as usize + 1
    }

    /// [`Self::tail_frames_at`] at the applied speed.
    pub fn tail_frames(&self) -> usize {
        self.tail_frames_at(self.speed())
    }

    /// True once the step has been constant over everything the next
    /// output's delay depends on: the ÷2's span and the ×2's and the core's
    /// spans in source time (`down + (up + core) / s` core outputs).
    pub fn latency_exact(&self) -> bool {
        let span = self.design.down.len() as f64
            + (self.design.up.len() + self.core.table().taps()) as f64 / self.speed();
        !self.ramping() && self.steady as f64 >= span.ceil()
    }

    /// Multiply-adds per output frame and channel at speed `s`: the ×2 per
    /// source frame (times `s`), the core's four rows per 2G output (two per
    /// frame) and the ÷2.
    pub fn macs_per_frame(&self, s: f64) -> f64 {
        let up = 2.0 * self.design.up.len().div_ceil(2) as f64;
        let core = 2.0 * 4.0 * self.core.table().taps() as f64;
        up * s + core + self.design.down.len() as f64
    }

    /// Ask for `speed` (`MIN_SPEED..=MAX_SPEED`), ramping over at least
    /// `ramp_ms` (`0..=MAX_RAMP_MS`; 0 and anything shorter than the
    /// envelope allows mean as fast as it allows, and never under
    /// [`envelope::MIN_RAMP_S`]). It starts at the next
    /// call with `n > 0`, from the applied speed and slope. The speed
    /// applied is capped by [`Self::set_speed_limit`]. The same speed and
    /// ramp again, while nothing else changed the target, is ignored (a
    /// repeated command does not restart its ramp).
    pub fn set_speed(&mut self, speed: f64, ramp_ms: f64) -> Result<(), VarispeedError> {
        check_speed(speed)?;
        if !(ramp_ms.is_finite() && (0.0..=MAX_RAMP_MS).contains(&ramp_ms)) {
            return Err(VarispeedError::Ramp(ramp_ms));
        }
        let heading = self.command.map_or(self.env.target(), |c| c.target);
        if speed == self.requested
            && ramp_ms == self.requested_ramp_ms
            && Step::from_f64(speed.min(self.limit)) == Some(heading)
        {
            return Ok(());
        }
        self.requested = speed;
        self.requested_ramp_ms = ramp_ms;
        self.issue(ramp_ms);
        Ok(())
    }

    /// Cap the speed at `max` (the scheduler's sustainable speed for the
    /// negotiated period and quantum), clamped to `MIN_SPEED..=MAX_SPEED`;
    /// not finite means no cap. The request is kept, so raising the cap
    /// again returns to it.
    ///
    /// **The cap-drop rule.** From the next call, the stage heads for the
    /// capped target as fast as the envelope allows. When that curve would
    /// still go above the cap (the speed is above it, or rising too fast to
    /// stop below it within the curvature limit), the stage instead fades
    /// the output out over [`CAP_FADE_MS`] (on that curve), jumps the step
    /// to the capped target at the first silent frame, stays silent while
    /// the jump clears the ÷2 ([`Self::mute_frames`]), and fades back in.
    /// From the jump on, the speed is at or below the cap.
    /// [`Self::peak_until_settled`] says how high it goes before. A command
    /// during the fade-out moves the jump's target; when the curve to it
    /// fits the cap after all, the fade turns back instead.
    ///
    /// **During the silence** its deadline is fixed: a command never
    /// restarts it. A lower target jumps the step at once (so the cap
    /// holds), and the deadline moves just far enough for the ÷2 to clear
    /// that jump too, but never more than one ÷2 span past where it first
    /// stood: the silence lasts at most `2 ×` [`Self::mute_frames`]. A
    /// higher target waits for the fade-in, where it ramps (the step is
    /// already within the cap). A command that leaves the target where it
    /// is changes nothing.
    pub fn set_speed_limit(&mut self, max: f64) {
        let max = if max.is_finite() {
            max.clamp(MIN_SPEED, MAX_SPEED)
        } else {
            MAX_SPEED
        };
        if max == self.limit {
            return;
        }
        self.limit = max;
        let target = self.capped_target();
        match self.command {
            // The pending command already does it.
            Some(c) if c.target == target && self.settles_within(Some(c), target) => {}
            // Without the pending command the stage heads there anyway,
            // within the cap: drop it (a cap that goes and comes back).
            _ if self.settles_within(None, target) => self.command = None,
            _ => self.issue(0.0),
        }
    }

    /// True when, with `cmd` pending in place of the current command, the
    /// stage heads to `target` and stays within the cap, whether the
    /// latched call (if any) runs or is planned again.
    fn settles_within(&self, cmd: Option<Command>, target: Step) -> bool {
        let ok = |base: Base, cmd: Option<Command>| {
            let heading = cmd.map_or(
                match base.fade {
                    Fade::Open | Fade::In { .. } => base.env.target(),
                    _ => base.jump,
                },
                |c| c.target,
            );
            heading == target && self.settle_peak(base, cmd) <= self.limit
        };
        ok(self.base(), cmd)
            && self
                .latched
                .is_none_or(|l| ok(l.base_after(), cmd.filter(|c| Some(c.seq) != l.consumed)))
    }

    fn base(&self) -> Base {
        Base {
            env: self.env,
            fade: self.fade,
            jump: self.jump_target,
            step: self.vstep,
        }
    }

    fn capped_target(&self) -> Step {
        Step::from_f64(self.requested.min(self.limit)).unwrap_or(Step::ONE)
    }

    fn issue(&mut self, ramp_ms: f64) {
        self.seq = self.seq.wrapping_add(1);
        self.command = Some(Command {
            target: self.capped_target(),
            ramp: self.env.outputs_for_ms(ramp_ms),
            seq: self.seq,
        });
    }

    fn fade_gain(&self, f: Fade) -> f64 {
        let x = |done: usize| std::f64::consts::PI * (done as f64 + 0.5) / self.fade_len as f64;
        match f {
            Fade::Open => 1.0,
            Fade::Out { done } => 0.5 * (1.0 + x(done).cos()),
            Fade::Mute { .. } => 0.0,
            Fade::In { done } => 0.5 * (1.0 - x(done).cos()),
        }
    }

    /// The gain state one frame later (a jump due at a muted frame is
    /// marked done by the caller).
    fn fade_step(&self, f: Fade) -> Fade {
        match f {
            Fade::Open => Fade::Open,
            Fade::Out { done } if done + 1 >= self.fade_len => Fade::Mute {
                left: self.mute_len,
                jumped: false,
                extend: self.mute_len,
            },
            Fade::Out { done } => Fade::Out { done: done + 1 },
            Fade::Mute { left, jumped, .. } if left <= 1 => {
                debug_assert!(jumped);
                Fade::In { done: 0 }
            }
            Fade::Mute {
                left,
                jumped,
                extend,
            } => Fade::Mute {
                left: left - 1,
                jumped,
                extend,
            },
            Fade::In { done } if done + 1 >= self.fade_len => Fade::Open,
            Fade::In { done } => Fade::In { done: done + 1 },
        }
    }

    /// The state a call with `n > 0` starts from, given its base and the
    /// pending command: the command started (a smooth curve, a cap drop's
    /// fade, or a jump during the silence), or waiting.
    fn start_from(&self, b: Base, cmd: Option<Command>) -> Start {
        let mut s = Start {
            env: b.env,
            fade: b.fade,
            jump: b.jump,
            consumed: None,
            waiting: None,
        };
        let Some(c) = cmd else { return s };
        s.consumed = Some(c.seq);
        match b.fade {
            // The jump at the fade's end goes to the latest target; or, when
            // the curve to it fits the cap after all, the fade turns back
            // from the gain it has reached.
            Fade::Out { done } => {
                s.jump = c.target;
                s.env.retarget(b.step, c.target, c.ramp);
                if s.env.peak_speed(b.step) <= self.limit {
                    s.fade = Fade::In {
                        done: self.fade_len - 1 - done.min(self.fade_len - 1),
                    };
                }
            }
            // The jump is at this call's first frame: to the latest target.
            Fade::Mute { jumped: false, .. } => s.jump = c.target,
            Fade::Mute {
                left,
                jumped: true,
                extend,
            } => {
                if c.target > b.jump {
                    // Higher: within the cap already; it ramps from the
                    // fade-in.
                    s.consumed = None;
                    s.waiting = Some(c.target);
                } else if c.target < b.jump {
                    // Lower: jump at this call's first frame, and move the
                    // deadline to clear that jump too, within its budget.
                    let more = self.mute_len.saturating_sub(left).min(extend);
                    s.jump = c.target;
                    s.fade = Fade::Mute {
                        left: left + more,
                        jumped: false,
                        extend: extend - more,
                    };
                }
                // The same target: nothing to do.
            }
            Fade::Open | Fade::In { .. } => {
                s.env.retarget(b.step, c.target, c.ramp);
                if s.env.peak_speed(b.step) > self.limit {
                    s.jump = c.target;
                    s.fade = match b.fade {
                        // From the gain the fade-in has reached.
                        Fade::In { done } => Fade::Out {
                            done: self.fade_len - 1 - done.min(self.fade_len - 1),
                        },
                        _ => Fade::Out { done: 0 },
                    };
                }
            }
        }
        s
    }

    /// The largest speed a call started from `b` with `cmd` pending, and
    /// every call after it, applies until the stage settles (no further
    /// command): the top of the curve; during a cap drop, of the curve
    /// until the jump (at most [`CAP_FADE_MS`] away) and the target after
    /// it; during the silence, the step and a target waiting for the
    /// fade-in.
    fn settle_peak(&self, b: Base, cmd: Option<Command>) -> f64 {
        let s = self.start_from(b, cmd);
        let now = b.step;
        let peak = match s.fade {
            Fade::Out { done } => {
                // Two core outputs per frame until the jump, plus one.
                let window = 2.0 * (self.fade_len - done) as f64 + 1.0;
                s.env.peak_within(now, window).max(s.jump.to_f64())
            }
            Fade::Mute { .. } => now.to_f64().max(s.jump.to_f64()),
            _ => s.env.peak_speed(now),
        };
        peak.max(s.waiting.map_or(0.0, Step::to_f64))
    }

    /// The largest speed the stage applies from now until it settles, if
    /// no further command arrives: over the latched call (when there is
    /// one) and every call after it, and over the calls a new plan would
    /// run instead. A command can raise it (call again after one); a
    /// scheduler sizes for it. Never below what the stage then applies.
    pub fn peak_until_settled(&self) -> f64 {
        let mut peak = self.settle_peak(self.base(), self.command);
        if let Some(l) = self.latched {
            let cmd = self.command.filter(|c| Some(c.seq) != l.consumed);
            peak = peak.max(l.peak).max(self.settle_peak(l.base_after(), cmd));
        }
        peak
    }

    /// The source frames [`Self::plan`] will ask for a call of `n` frames
    /// if no command arrives in between (worked out the same way, without
    /// changing anything). Never more than `2n + 1`.
    pub fn max_source_frames(&self, n: usize) -> usize {
        self.plan_call(n).map_or(2 * n + 1, |l| l.n_src)
    }

    /// The core's position (as [`crate::adaptive::Accumulator::position`])
    /// after `n` more output frames (at most [`Self::max_frames`]) if no
    /// command arrives in between: where a command given then would start.
    /// Exact on a hold; through a ramp the calls' cuts move it by a fraction
    /// of a frame (the envelope's pieces follow them). Changes nothing.
    pub fn position_after(&self, n: usize) -> Option<u128> {
        self.plan_call(n).ok().map(|l| l.pos_after)
    }

    /// Latch a call of exactly `n` output frames and return the source
    /// frames it takes (see the module docs). Nothing changes until
    /// [`Self::process`]; an error leaves nothing pending.
    pub fn plan(&mut self, n: usize) -> Result<usize, VarispeedError> {
        self.latched = None;
        let latch = self.plan_call(n)?;
        self.latched = Some(latch);
        Ok(latch.n_src)
    }

    /// The pure part of [`Self::plan`].
    fn plan_call(&self, n: usize) -> Result<Latch, VarispeedError> {
        if n > self.max_frames {
            return Err(VarispeedError::TooManyFrames {
                n,
                max: self.max_frames,
            });
        }
        let ratio = RateRatio::new(1, 2);
        let n_core = ratio.frames_in_for(self.down.input_pos(), n);
        let base = self.base();
        let (env, fade, jump, consumed) = if n_core > 0 {
            let s = self.start_from(base, self.command);
            (s.env, s.fade, s.jump, s.consumed)
        } else {
            (base.env, base.fade, base.jump, None)
        };
        // Walk the call's frames through the fade: where the jump falls (the
        // first muted frame not yet jumped), and the state after.
        let mut f = fade;
        let mut jump_frame = None;
        for i in 0..n {
            if let Fade::Mute {
                left,
                jumped: false,
                extend,
            } = f
            {
                jump_frame = Some(i);
                f = Fade::Mute {
                    left,
                    jumped: true,
                    extend,
                };
            }
            if f == Fade::Open {
                break;
            }
            f = self.fade_step(f);
        }
        let fade_after = f;
        let pre = jump_frame.map_or(n_core, |j| ratio.frames_in_for(self.down.input_pos(), j));
        let mut latch = Latch {
            n,
            n_core,
            n_src: 0,
            parts: [NO_PART; MAX_PARTS],
            n_parts: 0,
            env,
            fade,
            fade_after,
            jump_target: jump,
            consumed,
            peak: self.core.step().to_f64(),
            step_after: base.step,
            track_after: self.follow.map(|f| f.track),
            fallback: false,
            pos_after: 0,
        };
        // Pieces before the jump stay within the cap, except on a cap
        // drop's fade-out (whose curve the jump cuts short).
        let cap = matches!(fade, Fade::Open | Fade::In { .. })
            .then(|| Step::from_f64(self.limit).unwrap_or(Step::MAX));
        let mut acc = self.core.accumulator().clone();
        if self
            .plan_parts(&mut latch, &mut acc, pre, jump_frame.map(|_| jump), cap)
            .is_err()
        {
            // A guard: in-range commands never get here. Jump to the target
            // rather than stall the stream (counted when processed).
            let mut env = env;
            env.snap(env.target());
            latch.env = env;
            latch.n_parts = 0;
            latch.peak = self.core.step().to_f64();
            latch.track_after = self.follow.map(|f| f.track);
            latch.fallback = true;
            acc = self.core.accumulator().clone();
            self.plan_parts(&mut latch, &mut acc, pre, jump_frame.map(|_| jump), None)
                .map_err(VarispeedError::Plan)?;
        }
        latch.pos_after = acc.position();
        let need: usize = latch.parts[..latch.n_parts].iter().map(|p| p.need).sum();
        if need + MAX_OVERPULL > self.core.max_in() {
            return Err(VarispeedError::Plan(PlanError::Overflow)); // a guard
        }
        latch.n_src = need.div_ceil(2);
        Ok(latch)
    }

    /// Cut the call's core outputs into core calls on `acc`: the
    /// envelope's pieces over the first `pre` outputs (each at most
    /// [`PIECE_MAX`] outputs, ending at a brake's end and at the ramp's
    /// end, and cut shorter until the steps it applies stay within the
    /// curve's range over it, up to [`RANGE_SLACK`], and exactly within
    /// the stage's speed range and `cap`), then holds; or, with `jump`, a
    /// jump to it for the rest of the call.
    fn plan_parts(
        &self,
        latch: &mut Latch,
        acc: &mut crate::adaptive::Accumulator,
        pre: usize,
        jump: Option<Step>,
        cap: Option<Step>,
    ) -> Result<(), PlanError> {
        let push = |latch: &mut Latch, part: Part| -> Result<(), PlanError> {
            if latch.n_parts == MAX_PARTS {
                return Err(PlanError::Overflow);
            }
            latch.parts[latch.n_parts] = part;
            latch.n_parts += 1;
            Ok(())
        };
        // Follower mode: every part's step is the envelope's times 1 + ε,
        // with ε from the last call's end to the target at this one's
        // (over the call's core outputs), so the limits widen by ε.
        let follow = self.follow;
        let widen = if follow.is_some() { EPS_MAX } else { 0.0 };
        let lowest = Step::from_f64(MIN_SPEED * (1.0 - widen))
            .expect("in range")
            .bits();
        let highest = cap.map_or(Step::MAX, |c| {
            Step::from_f64(c.to_f64() * (1.0 + widen)).map_or(Step::MAX, |c| c.min(Step::MAX))
        });
        let highest = highest.bits().min(
            Step::from_f64(MAX_SPEED * (1.0 + widen))
                .unwrap_or(Step::MAX)
                .bits(),
        );
        let r = 2.0 * f64::from(self.rate);
        let call_s = latch.n_core.max(1) as f64 / r;
        let eps_from = follow.map_or((0.0, 0.0), |f| f.track.state());
        let eps_to = follow.map_or((0.0, 0.0), |f| f.to);
        // ε (value, slope) at `c` core outputs into the call.
        let eps_at = |c: usize| hermite(eps_from, eps_to, call_s, c as f64 / r);
        // The envelope's segment `seg` from its step `v0`, over outputs
        // ending at `c1`, with ε applied (`None`: out of the core's range).
        let apply = |track: &mut Option<EpsilonTrack>,
                     seg: Segment,
                     v0: Step,
                     c1: usize|
         -> Option<Segment> {
            match track.as_mut() {
                None => Some(seg),
                Some(t) => t.piece(seg, v0, eps_at(c1)),
            }
        };
        // The envelope's step after `seg` from `v0`.
        let after = |seg: Segment, v0: Step| match seg {
            Segment::Hold => v0,
            Segment::Jump(s) | Segment::Piece { end: s, .. } => s,
        };
        let plan = |latch: &mut Latch,
                    acc: &mut crate::adaptive::Accumulator,
                    part: Part|
         -> Result<usize, PlanError> {
            let need = acc.plan(part.outputs, part.seg)?;
            latch.peak = latch.peak.max(bits_f64(applied_range(acc).1));
            acc.commit(need);
            push(latch, Part { need, ..part })?;
            Ok(part.outputs)
        };
        let mut env = latch.env;
        let mut v = latch.step_after;
        let mut track = latch.track_after;
        let mut done = 0usize;
        let mut left = pre;
        while left > 0 {
            let (part, v_next) = if env.ramping() {
                let mut max = left.min(PIECE_MAX);
                loop {
                    let (seg, span, env_after, (lo, hi)) = env.segment(max);
                    let mut trial = acc.clone();
                    let mut t = track;
                    let applied = apply(&mut t, seg, v, done + span);
                    let ok = applied.is_some_and(|aseg| {
                        trial.plan(span, aseg).is_ok_and(|_| {
                            let (a, b) = applied_range(&trial);
                            let curve = bits_f64(a) >= lo * (1.0 - widen) * (1.0 - RANGE_SLACK)
                                && bits_f64(b) <= hi * (1.0 + widen) * (1.0 + RANGE_SLACK);
                            curve && a >= lowest && b <= highest
                        })
                    });
                    if ok {
                        env = env_after;
                        track = t;
                        break (
                            Part {
                                outputs: span,
                                seg: applied.expect("checked"),
                                need: 0,
                            },
                            after(seg, v),
                        );
                    }
                    if span <= 1 {
                        return Err(PlanError::StepOutOfRange { min: lo, max: hi });
                    }
                    max = span / 2;
                }
            } else {
                // Holding off the target only after a snap (the fallback,
                // or the tests' instant change): jump there.
                let seg = if env.target() != v {
                    Segment::Jump(env.target())
                } else {
                    Segment::Hold
                };
                // In follower mode a hold is a piece of ε: at most
                // `PIECE_MAX` outputs each.
                let outputs = if track.is_some() {
                    left.min(PIECE_MAX)
                } else {
                    left
                };
                let applied =
                    apply(&mut track, seg, v, done + outputs).ok_or(PlanError::StepOutOfRange {
                        min: v.to_f64(),
                        max: v.to_f64(),
                    })?;
                (
                    Part {
                        outputs,
                        seg: applied,
                        need: 0,
                    },
                    after(seg, v),
                )
            };
            let n = plan(latch, acc, part)?;
            left -= n;
            done += n;
            v = v_next;
        }
        if let Some(target) = jump {
            let rest = latch.n_core - pre;
            if rest > 0 {
                let seg = apply(&mut track, Segment::Jump(target), v, latch.n_core).ok_or(
                    PlanError::StepOutOfRange {
                        min: target.to_f64(),
                        max: target.to_f64(),
                    },
                )?;
                let part = Part {
                    outputs: rest,
                    seg,
                    need: 0,
                };
                plan(latch, acc, part)?;
            }
            env.snap(target);
            v = target;
        }
        // ε ends on the target (the Hermite's end), or with zero slope
        // after a jump.
        latch.env = env;
        latch.step_after = v;
        latch.track_after = track;
        Ok(())
    }

    /// The exact 64.64 positions (absolute, as
    /// [`crate::adaptive::Accumulator::position`]) of every core output of
    /// the latched call, in order; nothing when no call is latched.
    pub fn planned_positions(&self, mut f: impl FnMut(u128)) {
        let Some(latch) = self.latched else { return };
        let mut acc = self.core.accumulator().clone();
        for part in &latch.parts[..latch.n_parts] {
            if acc.plan(part.outputs, part.seg).is_err() {
                return;
            }
            let p = acc.planned().expect("planned");
            let pos = acc.position();
            for k in 0..part.outputs {
                if let Some(q) = crate::adaptive::position_after(pos, p.step, p.d, k) {
                    f(q);
                }
            }
            acc.commit(part.need);
        }
    }

    /// Run the latched call: `input[c]` holds the `n_src` source frames the
    /// plan asked for, `output[c]` receives the plan's `n` frames. Panics
    /// without a pending plan or with wrong lengths (caller bugs).
    pub fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        let latch = self.latched.take().expect("process without a plan");
        let ch = self.channels;
        assert_eq!(input.len(), ch, "input channel count mismatch");
        assert_eq!(output.len(), ch, "output channel count mismatch");
        for c in 0..ch {
            assert_eq!(
                input[c].len(),
                latch.n_src,
                "input must be the planned n_src"
            );
            assert_eq!(output[c].len(), latch.n, "output must be the planned n");
        }
        let n_mid = 2 * latch.n_src;
        {
            let mut outs: [&mut [f64]; MAX_CHANNELS] = std::array::from_fn(|_| &mut [][..]);
            for (o, m) in outs.iter_mut().zip(self.mid.iter_mut()) {
                *o = &mut m[..n_mid];
            }
            self.up.process(input, &mut outs[..ch]);
        }
        // The core calls. The ×2's rounding frame (when `2 n_src` exceeds
        // the total need) goes to the last part that needs input: later
        // parts need none, with or without it.
        let parts = &latch.parts[..latch.n_parts];
        let last_fed = parts.iter().rposition(|p| p.need > 0);
        let (mut a, mut c0) = (0, 0);
        for (i, part) in parts.iter().enumerate() {
            let need = self.core.plan(part.outputs, part.seg);
            debug_assert_eq!(need, Ok(part.need));
            let b = if Some(i) == last_fed {
                n_mid
            } else {
                a + part.need
            };
            let c1 = c0 + part.outputs;
            let mut ins: [&[f64]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            for (x, m) in ins.iter_mut().zip(self.mid.iter()) {
                *x = &m[a..b];
            }
            let mut outs: [&mut [f64]; MAX_CHANNELS] = std::array::from_fn(|_| &mut [][..]);
            for (o, m) in outs.iter_mut().zip(self.core_out.iter_mut()) {
                *o = &mut m[c0..c1];
            }
            self.core.process(&ins[..ch], &mut outs[..ch]);
            (a, c0) = (b, c1);
            self.steady = match part.seg {
                Segment::Hold => self.steady.saturating_add(part.outputs as u64),
                Segment::Jump(_) => part.outputs as u64,
                Segment::Piece { .. } => 0,
            };
        }
        debug_assert_eq!((a, c0), (n_mid, latch.n_core));
        {
            debug_assert_eq!(self.down.output_frames(latch.n_core), latch.n);
            let mut ins: [&[f64]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            for (i, m) in ins.iter_mut().zip(self.core_out.iter()) {
                *i = &m[..latch.n_core];
            }
            self.down.process(&ins[..ch], output);
        }
        if latch.fade != Fade::Open {
            let mut f = latch.fade;
            for i in 0..latch.n {
                if f == Fade::Open {
                    break;
                }
                if let Fade::Mute {
                    left,
                    jumped: false,
                    extend,
                } = f
                {
                    f = Fade::Mute {
                        left,
                        jumped: true,
                        extend,
                    };
                }
                let g = self.fade_gain(f);
                for o in output.iter_mut() {
                    o[i] *= g;
                }
                f = self.fade_step(f);
            }
            debug_assert_eq!(f, latch.fade_after);
        }
        self.env = latch.env;
        self.vstep = latch.step_after;
        if let (Some(f), Some(t)) = (self.follow.as_mut(), latch.track_after) {
            f.track = t;
        }
        self.fade = latch.fade_after;
        self.jump_target = latch.jump_target;
        if latch.fallback {
            self.fallbacks += 1;
        }
        if latch.consumed.is_some() && self.command.map(|c| c.seq) == latch.consumed {
            self.command = None;
        }
    }

    /// Clear every history and hold at the target (the last request,
    /// capped by the current limit) from position 0, with no ramp and no
    /// fade: a reopen or DSP reset, under the output's own fade.
    pub fn reset(&mut self) {
        let target = self.capped_target();
        self.up.reset();
        self.down.reset();
        // A follower keeps its ε (the rate state) across a reset.
        let core_step = match self.follow.as_mut() {
            Some(f) => {
                let e = f.track.state().0;
                f.track.reset(e);
                f.to = (e, 0.0);
                Step::from_f64(target.to_f64() * (1.0 + e)).unwrap_or(target)
            }
            None => target,
        };
        self.core.reset(core_step);
        self.vstep = target;
        self.env.hold_at(target);
        self.command = None;
        self.latched = None;
        self.fade = Fade::Open;
        self.jump_target = target;
        self.steady = u64::MAX / 2;
    }

    /// Test hook: an instant change of the step at the next call (speed
    /// commands always ramp).
    #[cfg(test)]
    fn jump(&mut self, speed: f64) {
        let s = check_speed(speed).unwrap();
        self.requested = speed;
        self.env.snap(s);
        self.command = None;
    }
}

#[cfg(test)]
mod tests;
