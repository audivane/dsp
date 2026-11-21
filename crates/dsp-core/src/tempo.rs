//! Tempo: the key-locked speed stage (route B). It plays the source at speed
//! `s` and keeps the pitch, by running the
//! vendored Signalsmith Stretch (the pitch stage's library) as a
//! time-stretch: every call consumes about `s · n` source frames for the
//! `n` frames it writes, with no transposition.
//!
//! It takes varispeed's place in the processor while Keep pitch is on, so
//! it has the same shape as [`crate::varispeed::Varispeed`]:
//!
//! - **Pull contract.** [`TempoStretch::plan`] latches a call of exactly
//!   `n` output frames and says how many source frames it takes (`n_src`);
//!   [`TempoStretch::process`] follows with exactly `n_src` source frames
//!   and writes exactly `n` frames. The source position is the core's exact
//!   64.64 accumulator ([`crate::adaptive::Accumulator`]), stepped once per
//!   output frame, so `n_src` over any run of calls is exact for the
//!   quantised step. A `plan` without a `process` is discarded by the next
//!   `plan`; a zero call consumes and changes nothing.
//!   [`TempoStretch::max_source_frames`] gives `n_src` before the plan.
//! - **Speed commands** go through varispeed's envelope
//!   ([`crate::varispeed::envelope`], at one output per frame), cut into
//!   cubic pieces of the step of at most [`PIECE_MAX`] frames. Each piece is
//!   its own call into the library, so the input it is fed follows the curve
//!   within a call too.
//! - **The cap** ([`TempoStretch::set_speed_limit`]) is the scheduler's
//!   sustainable speed, as for varispeed. Where varispeed has to fade out and
//!   jump under silence (a jump of a resampler's step is a kink in the
//!   waveform), a time-stretch can jump: the library places its next analysis
//!   hop further on and the overlap-add output stays continuous. So a cap the
//!   curve cannot respect is met by an instant jump of the step to the capped
//!   target at the next call, without a gap.
//! - **Geometry**: the pitch stage's `high` (120 ms window, 15 ms hop), with
//!   its high-rate step-down ([`crate::pitch::stft_geometry`]). Split
//!   computation is on, so the STFT work is spread over the calls.
//!
//! **Latency** at constant speed `s`: `output_latency + input_latency / s`
//! output frames ([`TempoStretch::latency_at`]). During a ramp
//! [`TempoStretch::latency_frames`] gives the figure at the applied speed, and
//! [`TempoStretch::latency_exact`] stays false until the step has been
//! constant over the windows' whole span.
//!
//! **Real time.** [`TempoStretch::new`] allocates everything. `plan`,
//! `process`, `reset` and the commands never allocate, lock or block.
//! `process` panics only on caller contract breaks (no plan, wrong slice
//! lengths).

use crate::adaptive::{Accumulator, MAX_CHANNELS, PlanError, Segment, Step};
use crate::pitch::{
    av_pitch_configure, av_pitch_free, av_pitch_input_latency, av_pitch_new,
    av_pitch_output_latency, av_pitch_reset, av_stretch_process, stft_geometry,
};
use crate::varispeed::envelope::Envelope;
use crate::varispeed::{RANGE_SLACK, applied_range, bits_f64};
use audivane_protocol::PitchQuality;
use std::ffi::c_void;

pub use crate::varispeed::{MAX_FRAMES, MAX_RAMP_MS, MAX_SPEED, MIN_SPEED};

/// Longest envelope piece, frames (varispeed's 2 048 core outputs).
pub const PIECE_MAX: usize = 1024;
/// The geometry the stage runs ([`stft_geometry`] steps it down above
/// 192 kHz).
pub const QUALITY: PitchQuality = PitchQuality::High;
/// Most library calls one stage call can take: the pieces of a call of
/// [`MAX_FRAMES`] (some cut shorter to stay in range), a brake's end, a
/// ramp's end and the hold after it. Beyond it the call falls back to a
/// jump.
const MAX_PARTS: usize = 48;

/// Multiply-add equivalents per STFT hop and `N log2 N` of the window
/// (both transforms, the re-analysis, the spectral processing), fitted to
/// the stage against varispeed (stereo, 44.1 to 384 kHz, speeds 0.5 to 2).
const MACS_PER_HOP_NLOGN: f64 = 54.0;

/// The stage's cost for the static budget at graph rate `rate`, in
/// multiply-add equivalents per second and channel (the unit of
/// [`crate::varispeed::worst_case_macs_per_second`]), at its worst speed:
/// about 3× varispeed's worst case. Nearly constant in the speed, so there
/// is no speed argument.
pub fn macs_per_second(rate: u32) -> Result<f64, TempoError> {
    if rate == 0 {
        return Err(TempoError::Rate(rate));
    }
    let (block, interval) = stft_geometry(rate, QUALITY);
    let n = block as f64;
    let hops = f64::from(rate) / interval as f64;
    Ok(MACS_PER_HOP_NLOGN * hops * n * n.log2())
}

/// Why a stage was not built or a command was refused. A refused command
/// changes nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum TempoError {
    /// The graph rate must be at least 1 Hz.
    Rate(u32),
    /// `channels` must be `1..=MAX_CHANNELS`.
    Channels(usize),
    /// `max_frames` must be `1..=MAX_FRAMES`.
    MaxFrames(usize),
    /// A speed outside `MIN_SPEED..=MAX_SPEED` (or not finite).
    Speed(f64),
    /// A ramp outside `0..=MAX_RAMP_MS` (or not finite).
    Ramp(f64),
    /// More frames in one call than the stage was built for.
    TooManyFrames { n: usize, max: usize },
    /// The position arithmetic refused the call (a guard; not reachable
    /// within `max_frames`).
    Plan(PlanError),
    /// The library could not be built or configured.
    Ffi(&'static str),
}

impl std::fmt::Display for TempoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rate(r) => write!(f, "graph rate {r} Hz"),
            Self::Channels(c) => write!(f, "{c} channels, must be 1 to {MAX_CHANNELS}"),
            Self::MaxFrames(n) => write!(f, "{n} frames per call, must be 1 to {MAX_FRAMES}"),
            Self::Speed(s) => write!(f, "speed {s}, must be {MIN_SPEED} to {MAX_SPEED}"),
            Self::Ramp(r) => write!(f, "ramp {r} ms, must be 0 to {MAX_RAMP_MS}"),
            Self::TooManyFrames { n, max } => write!(f, "{n} frames in one call, at most {max}"),
            Self::Plan(e) => write!(f, "tempo position: {e}"),
            Self::Ffi(e) => write!(f, "time-stretch library: {e}"),
        }
    }
}

impl std::error::Error for TempoError {}

/// What a stage is built for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoSpec {
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
    /// Ramp length asked for, frames.
    ramp: f64,
    seq: u64,
}

/// One library call within a stage call.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Part {
    outputs: usize,
    seg: Segment,
    /// New source frames it takes.
    need: usize,
}

const NO_PART: Part = Part {
    outputs: 0,
    seg: Segment::Hold,
    need: 0,
};

/// A planned call: the library calls it runs, in order, and the state
/// after.
#[derive(Clone, Debug, PartialEq)]
struct Latch {
    n: usize,
    n_src: usize,
    parts: [Part; MAX_PARTS],
    n_parts: usize,
    /// The envelope and the position after the call.
    env: Envelope,
    acc: Accumulator,
    /// The command this call starts, if any.
    consumed: Option<u64>,
    /// The largest step the call applies.
    peak: f64,
    /// The position arithmetic refused a piece and the call jumps to the
    /// target instead.
    fallback: bool,
}

/// The tempo stage (see the module docs).
pub struct TempoStretch {
    inner: *mut c_void,
    rate: u32,
    channels: usize,
    max_frames: usize,
    block: usize,
    interval: usize,
    input_latency: usize,
    output_latency: usize,
    acc: Accumulator,
    env: Envelope,
    /// The speed last asked for, and the ramp it asked for (ms).
    requested: f64,
    requested_ramp_ms: f64,
    /// The scheduler's cap on the speed.
    limit: f64,
    command: Option<Command>,
    seq: u64,
    latched: Option<Latch>,
    /// Frames since the step last changed.
    steady: u64,
    fallbacks: u64,
    /// `[output frames since the last input above the noise floor, silent
    /// input frames since then or since the last lift]`, kept across calls
    /// for the bridge: within the tail, silence keeps the synthesis running
    /// (`LiftedInputs`), so the overlap-add plays out.
    quiet: [i64; 2],
}

// SAFETY: `SignalsmithStretch<double>` has no thread-local or global mutable
// state; the stage owns the heap-allocated C++ object exclusively.
unsafe impl Send for TempoStretch {}

impl Drop for TempoStretch {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { av_pitch_free(self.inner) };
            self.inner = std::ptr::null_mut();
        }
    }
}

fn check_speed(speed: f64) -> Result<Step, TempoError> {
    if !(speed.is_finite() && (MIN_SPEED..=MAX_SPEED).contains(&speed)) {
        return Err(TempoError::Speed(speed));
    }
    Step::from_f64(speed).ok_or(TempoError::Speed(speed))
}

impl TempoStretch {
    pub fn new(spec: TempoSpec) -> Result<Self, TempoError> {
        if spec.sample_rate == 0 {
            return Err(TempoError::Rate(spec.sample_rate));
        }
        if !(1..=MAX_CHANNELS).contains(&spec.channels) {
            return Err(TempoError::Channels(spec.channels));
        }
        if !(1..=MAX_FRAMES).contains(&spec.max_frames) {
            return Err(TempoError::MaxFrames(spec.max_frames));
        }
        let speed = check_speed(spec.speed)?;
        let (block, interval) = stft_geometry(spec.sample_rate, QUALITY);
        let block_i32 = i32::try_from(block).map_err(|_| TempoError::Rate(spec.sample_rate))?;
        let interval_i32 =
            i32::try_from(interval).map_err(|_| TempoError::Rate(spec.sample_rate))?;
        let channels_i32 = i32::try_from(spec.channels).expect("at most MAX_CHANNELS");

        let inner = unsafe { av_pitch_new() };
        if inner.is_null() {
            return Err(TempoError::Ffi("allocation failed"));
        }
        // Build the stage at once, so `Drop` frees `inner` on every error
        // below.
        let mut stage = Self {
            inner,
            rate: spec.sample_rate,
            channels: spec.channels,
            max_frames: spec.max_frames,
            block,
            interval,
            input_latency: 0,
            output_latency: 0,
            acc: Accumulator::new(speed),
            env: Envelope::new(f64::from(spec.sample_rate), speed),
            requested: speed.to_f64(),
            requested_ramp_ms: 0.0,
            limit: MAX_SPEED,
            command: None,
            seq: 0,
            latched: None,
            steady: u64::MAX / 2,
            fallbacks: 0,
            quiet: [i64::MAX / 2, 0],
        };
        let rc = unsafe { av_pitch_configure(inner, channels_i32, block_i32, interval_i32, true) };
        if rc != 0 {
            return Err(TempoError::Ffi("configure failed"));
        }
        let (i, o) = unsafe {
            (
                av_pitch_input_latency(inner),
                av_pitch_output_latency(inner),
            )
        };
        let (Ok(i), Ok(o)) = (usize::try_from(i), usize::try_from(o)) else {
            return Err(TempoError::Ffi("latency query failed"));
        };
        stage.input_latency = i;
        stage.output_latency = o;
        Ok(stage)
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

    /// The library's `(window, hop)` in frames, after the high-rate
    /// step-down.
    pub fn geometry(&self) -> (usize, usize) {
        (self.block, self.interval)
    }

    /// The exact source position (64.64) and the source frames taken.
    pub fn accumulator(&self) -> &Accumulator {
        &self.acc
    }

    /// The speed applied at the next output (what is audible).
    pub fn speed(&self) -> f64 {
        self.acc.step().to_f64()
    }

    /// The applied speed's exact bits.
    pub fn step(&self) -> Step {
        self.acc.step()
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

    /// The ramp last asked for with it, ms.
    pub fn requested_ramp_ms(&self) -> f64 {
        self.requested_ramp_ms
    }

    /// The cap on the speed ([`Self::set_speed_limit`]).
    pub fn speed_limit(&self) -> f64 {
        self.limit
    }

    /// True while the speed is moving or a command waits to start.
    pub fn ramping(&self) -> bool {
        self.env.ramping() || self.command.is_some()
    }

    /// Counts the commands the stage has issued to its envelope (as
    /// [`crate::varispeed::Varispeed::command_seq`]).
    pub fn command_seq(&self) -> u64 {
        self.seq
    }

    /// Calls on which the position arithmetic refused a piece and the
    /// stage jumped to the target (a guard: zero for every in-range
    /// command).
    pub fn fallbacks(&self) -> u64 {
        self.fallbacks
    }

    /// The library's input latency (source frames: half the window).
    pub fn input_latency(&self) -> usize {
        self.input_latency
    }

    /// The library's output latency (output frames: half the window plus
    /// the split computation's hop).
    pub fn output_latency(&self) -> usize {
        self.output_latency
    }

    /// Output frames of delay at constant speed `s`: the input latency is
    /// consumed at `s` source frames per output frame.
    pub fn latency_at(&self, s: f64) -> f64 {
        let s = if s.is_finite() && s > 0.0 { s } else { 1.0 };
        self.output_latency as f64 + self.input_latency as f64 / s
    }

    /// Output frames of delay at the applied speed: exact at constant
    /// speed, an estimate while [`Self::latency_exact`] is false.
    pub fn latency_frames(&self) -> f64 {
        self.latency_at(self.speed())
    }

    /// Output frames of zero source after the last real one until the
    /// last window holding it has played out at constant speed `s`: the
    /// whole analysis window in source time and the whole synthesis window
    /// and hop (not their centres, as [`Self::latency_at`]), rounded up,
    /// plus a frame for the fractional position.
    pub fn tail_frames_at(&self, s: f64) -> usize {
        let s = if s.is_finite() && s > 0.0 { s } else { 1.0 };
        (self.block as f64 / s + (self.block + self.interval) as f64).ceil() as usize + 1
    }

    /// [`Self::tail_frames_at`] at the applied speed.
    pub fn tail_frames(&self) -> usize {
        self.tail_frames_at(self.speed())
    }

    /// True once the step has been constant over everything the next
    /// output depends on ([`Self::tail_frames`]).
    pub fn latency_exact(&self) -> bool {
        !self.ramping() && self.steady >= self.tail_frames() as u64
    }

    /// Ask for `speed` (`MIN_SPEED..=MAX_SPEED`), ramping over at least
    /// `ramp_ms` (`0..=MAX_RAMP_MS`; 0 and anything shorter than the
    /// envelope allows mean as fast as it allows). It starts at the next
    /// call with `n > 0`, from the applied speed and slope, and is capped
    /// by [`Self::set_speed_limit`]. The same speed and ramp again, while
    /// nothing else changed the target, is ignored.
    pub fn set_speed(&mut self, speed: f64, ramp_ms: f64) -> Result<(), TempoError> {
        check_speed(speed)?;
        if !(ramp_ms.is_finite() && (0.0..=MAX_RAMP_MS).contains(&ramp_ms)) {
            return Err(TempoError::Ramp(ramp_ms));
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

    /// Cap the speed at `max` (the scheduler's sustainable speed),
    /// clamped to `MIN_SPEED..=MAX_SPEED`; not finite means no cap. The
    /// request is kept, so raising the cap again returns to it.
    ///
    /// From the next call the stage heads for the capped target as fast as
    /// the envelope allows. When that curve would still go above the cap
    /// (the speed is above it, or rising too fast to stop below it), the
    /// step jumps to the capped target at that call's first frame instead
    /// (see the module docs). A latched call runs as planned.
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
            Some(c) if c.target == target => {}
            _ if self.settles_within(target) => self.command = None,
            _ => self.issue(0.0),
        }
    }

    /// True when, without a command, the stage heads to `target` within the
    /// cap, whether the latched call (if any) runs or is planned again.
    fn settles_within(&self, target: Step) -> bool {
        let ok = |env: Envelope, step: Step| {
            env.target() == target && env.peak_speed(step) <= self.limit
        };
        ok(self.env, self.acc.step())
            && self
                .latched
                .as_ref()
                .is_none_or(|l| ok(l.env, l.acc.step()))
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

    /// The envelope a call with `n > 0` starts with, from `env` at `step`
    /// with `cmd` pending: the command's curve, or (when that curve would
    /// pass the cap) a hold at its target, which the call jumps to.
    fn start_from(&self, mut env: Envelope, step: Step, cmd: Option<Command>) -> Envelope {
        if let Some(c) = cmd {
            env.retarget(step, c.target, c.ramp);
            if env.peak_speed(step) > self.limit {
                env.snap(c.target);
            }
        }
        env
    }

    /// The largest speed from `env` at `step`, with `cmd` pending, until
    /// the stage settles.
    fn settle_peak(&self, env: Envelope, step: Step, cmd: Option<Command>) -> f64 {
        let env = self.start_from(env, step, cmd);
        if env.ramping() {
            env.peak_speed(step)
        } else {
            // Holding, or about to jump: the next frame is at the target.
            env.target().to_f64()
        }
    }

    /// The largest speed the stage applies from now until it settles, if
    /// no further command arrives: over the latched call (when there is
    /// one) and every call after it, and over the calls a new plan would
    /// run instead. A scheduler sizes for it.
    pub fn peak_until_settled(&self) -> f64 {
        let mut peak = self.settle_peak(self.env, self.acc.step(), self.command);
        if let Some(l) = &self.latched {
            let cmd = self.command.filter(|c| Some(c.seq) != l.consumed);
            peak = peak
                .max(l.peak)
                .max(self.settle_peak(l.env, l.acc.step(), cmd));
        }
        peak
    }

    /// The source frames [`Self::plan`] will ask for a call of `n` frames
    /// if no command arrives in between (worked out the same way, without
    /// changing anything). Never more than `2n + 1`.
    pub fn max_source_frames(&self, n: usize) -> usize {
        self.plan_call(n).map_or(2 * n + 1, |l| l.n_src)
    }

    /// Latch a call of exactly `n` output frames and return the source
    /// frames it takes (see the module docs). Nothing changes until
    /// [`Self::process`]; an error leaves nothing pending.
    pub fn plan(&mut self, n: usize) -> Result<usize, TempoError> {
        self.latched = None;
        let latch = self.plan_call(n)?;
        let n_src = latch.n_src;
        self.latched = Some(latch);
        Ok(n_src)
    }

    /// The pure part of [`Self::plan`].
    fn plan_call(&self, n: usize) -> Result<Latch, TempoError> {
        if n > self.max_frames {
            return Err(TempoError::TooManyFrames {
                n,
                max: self.max_frames,
            });
        }
        let step = self.acc.step();
        let (env, consumed) = if n > 0 {
            (
                self.start_from(self.env, step, self.command),
                self.command.map(|c| c.seq),
            )
        } else {
            (self.env, None)
        };
        let mut latch = Latch {
            n,
            n_src: 0,
            parts: [NO_PART; MAX_PARTS],
            n_parts: 0,
            env,
            acc: self.acc.clone(),
            consumed,
            peak: step.to_f64(),
            fallback: false,
        };
        let cap = Step::from_f64(self.limit).unwrap_or(Step::MAX);
        let mut acc = self.acc.clone();
        if Self::plan_parts(&mut latch, &mut acc, Some(cap)).is_err() {
            // A guard: in-range commands never get here. Jump to the target
            // rather than stall the stream (counted when processed).
            let mut env = env;
            env.snap(env.target());
            latch.env = env;
            latch.n_parts = 0;
            latch.peak = step.to_f64();
            latch.fallback = true;
            acc = self.acc.clone();
            Self::plan_parts(&mut latch, &mut acc, None).map_err(TempoError::Plan)?;
        }
        latch.n_src = latch.parts[..latch.n_parts].iter().map(|p| p.need).sum();
        latch.acc = acc;
        Ok(latch)
    }

    /// Cut the call's frames into library calls on `acc`: the envelope's
    /// pieces (each at most [`PIECE_MAX`] frames, ending at a brake's end
    /// and at the ramp's end, and cut shorter until the steps it applies
    /// stay within the curve's range over it, up to [`RANGE_SLACK`], and
    /// exactly within the speed range and `cap`), then a jump or a hold.
    fn plan_parts(
        latch: &mut Latch,
        acc: &mut Accumulator,
        cap: Option<Step>,
    ) -> Result<(), PlanError> {
        let lowest = Step::from_f64(MIN_SPEED).expect("in range").bits();
        let highest = cap.map_or(Step::MAX, |c| c.min(Step::MAX)).bits();
        let highest = highest.min(Step::from_f64(MAX_SPEED).expect("in range").bits());
        let mut env = latch.env;
        let mut left = latch.n;
        while left > 0 {
            let part = if env.ramping() {
                let mut max = left.min(PIECE_MAX);
                loop {
                    let (seg, span, after, (lo, hi)) = env.segment(max);
                    let mut trial = acc.clone();
                    let ok = trial.plan(span, seg).is_ok_and(|_| {
                        let (a, b) = applied_range(&trial);
                        let curve = bits_f64(a) >= lo * (1.0 - RANGE_SLACK)
                            && bits_f64(b) <= hi * (1.0 + RANGE_SLACK);
                        curve && a >= lowest && b <= highest
                    });
                    if ok {
                        env = after;
                        break Part {
                            outputs: span,
                            seg,
                            need: 0,
                        };
                    }
                    if span <= 1 {
                        return Err(PlanError::StepOutOfRange { min: lo, max: hi });
                    }
                    max = span / 2;
                }
            } else if env.target() != acc.step() {
                // A cap the curve cannot respect, or the fallback: jump.
                Part {
                    outputs: left,
                    seg: Segment::Jump(env.target()),
                    need: 0,
                }
            } else {
                Part {
                    outputs: left,
                    seg: Segment::Hold,
                    need: 0,
                }
            };
            let need = acc.plan(part.outputs, part.seg)?;
            latch.peak = latch.peak.max(bits_f64(applied_range(acc).1));
            acc.commit(need);
            if latch.n_parts == MAX_PARTS {
                return Err(PlanError::Overflow);
            }
            latch.parts[latch.n_parts] = Part { need, ..part };
            latch.n_parts += 1;
            left -= part.outputs;
        }
        latch.env = env;
        Ok(())
    }

    /// The exact 64.64 source positions (absolute, as
    /// [`Accumulator::position`]) of every output frame of the latched
    /// call, in order; nothing when no call is latched.
    pub fn planned_positions(&self, mut f: impl FnMut(u128)) {
        let Some(latch) = &self.latched else { return };
        let mut acc = self.acc.clone();
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
        let mut ins = [std::ptr::null::<f64>(); MAX_CHANNELS];
        let mut outs = [std::ptr::null_mut::<f64>(); MAX_CHANNELS];
        let (mut a, mut o) = (0, 0);
        let tail = self.tail_frames() as i64;
        for part in &latch.parts[..latch.n_parts] {
            let (b, p) = (a + part.need, o + part.outputs);
            for c in 0..ch {
                ins[c] = input[c][a..b].as_ptr();
                outs[c] = output[c][o..p].as_mut_ptr();
            }
            let rc = unsafe {
                av_stretch_process(
                    self.inner,
                    ins.as_ptr(),
                    i32::try_from(part.need).expect("need fits i32"),
                    outs.as_ptr(),
                    i32::try_from(part.outputs).expect("outputs fit i32"),
                    ch as i32,
                    tail,
                    self.quiet.as_mut_ptr(),
                )
            };
            debug_assert_eq!(rc, 0, "av_stretch_process failed");
            (a, o) = (b, p);
            self.steady = match part.seg {
                Segment::Hold => self.steady.saturating_add(part.outputs as u64),
                Segment::Jump(_) => part.outputs as u64,
                Segment::Piece { .. } => 0,
            };
        }
        debug_assert_eq!((a, o), (latch.n_src, latch.n));
        self.env = latch.env;
        self.acc = latch.acc;
        if latch.fallback {
            self.fallbacks += 1;
        }
        if latch.consumed.is_some() && self.command.map(|c| c.seq) == latch.consumed {
            self.command = None;
        }
    }

    /// Clear every history and hold at the target (the last request,
    /// capped by the current limit) from position 0, with no ramp: a reopen
    /// or DSP reset, under the output's own fade.
    pub fn reset(&mut self) {
        let target = self.capped_target();
        let rc = unsafe { av_pitch_reset(self.inner) };
        debug_assert_eq!(rc, 0, "av_pitch_reset failed");
        self.acc.reset(target);
        self.env.hold_at(target);
        self.command = None;
        self.latched = None;
        self.steady = u64::MAX / 2;
        self.quiet = [i64::MAX / 2, 0];
    }
}

#[cfg(test)]
mod tests;
