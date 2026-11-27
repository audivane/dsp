//! Varispeed in the daemon: the stage's slot in the RT processor
//! ([`VarispeedRt`]), the control side that builds and retires stages
//! ([`VarispeedHost`]), and the applied state the RT thread publishes
//! ([`VarispeedApplied`]).
//!
//! The stage (`dsp_core::varispeed::Varispeed`) runs once, in the
//! processor, before the chains. The driver's scheduler reaches it through
//! [`audivane_io_pw::varispeed_rt::VarispeedPort`]; the processor asks the
//! slot whether a stage runs for a block, and runs it on the block's
//! source frames before handing the chains its output.
//!
//! **Lifecycle** (all transitions on the RT thread, none allocates):
//!
//! | Event | While playing through the DSP chain | Otherwise |
//! |---|---|---|
//! | Install (enable, or a stage for a new rate) | restart at unity with zero history and a 5 ms source fade-in; the raw block fades out over 5 ms meanwhile (the stage's own delay keeps its output silent for ~6 ms, so the two barely overlap); the driver then ramps to the request | reset at the target, no fade (the next prime starts from there) |
//! | Disable | the stage keeps playing until the driver has sized the next pull for a raw one (`VarispeedPort::raw_next`); then its output fades out over 5 ms, the stage retires, and the raw block fades in over 5 ms at the output, in the same period (the driver ends a stage call at the fade's end) | retire now |
//! | DSP off (the processor defers the toggle) | the same fade-out, then the stage stays (stale) and the toggle applies; the raw block fades in | - |
//! | Direct takeover, a rate it was not built for | untouched; the chains get the raw block | - |
//! | Back to the DSP chain | restart at unity (no stale history plays), then the driver ramps to the request: after the DSP sink's own raw block, that block fades out while the stage fades in; otherwise under a source fade-in | - |
//! | Reopen or prime (`VarispeedPort::reset`), DSP reset | reset at the target (the driver's own fade covers it) | the same |
//! | Underfeed | the source fades out over the last 5 ms before the gap (the slot holds 5 ms of source back, so the fade has frames even when the gap starts a call), and back in when frames arrive again | - |
//! | Keep pitch toggled (an install of the other kind) | the playing stage fades out over 5 ms (`StopKind::Switch`), then the new one starts at the driver's speed from silence with its source faded in: one gap, the new stage's delay (fade, prime, fade). Into the stretch it starts where varispeed's fade ended (the source that stage had taken but not played is carried over); toggled back before the fade starts, nothing changes | replace now |
//! | Any stop of the stretch with a follower's alignment delay (which follows the stretch) | after its fade, silence for the delay's length before the stop applies, so the delay has played the fade out when its history restarts | - |
//!
//! **Keep pitch** puts the key-locked stretch (`dsp_core::tempo`) in the slot
//! instead of varispeed's resampler ([`Stage`]): the same pull contract,
//! envelope and cap, so the driver and every row above treat both alike, except
//! that a (re)start from the raw block fades it out at once (a cross-fade after
//! the stretch's 135 ms would play that much twice), and that followers neither
//! replay its commands nor need its command lead or speed cap: the processor
//! tees its output.
//!
//! The slot delays the source by its fade length (the look-ahead the underfeed
//! fade needs); the delay counts in the latency and in the drain's tail. A
//! retired stage goes to the reaper through a queue (with a small stash when it
//! is full), never dropped on the RT thread.

use super::tee::{StageView, VsCommand};
use audivane_dsp_core::tempo::{TempoSpec, TempoStretch};
use audivane_dsp_core::varispeed::{self as vsp, Varispeed, VarispeedSpec};
use audivane_io_pw::varispeed_rt::VarispeedPort;
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};

/// Most channels a stage runs for in the processor (the planar handoff
/// keeps its slices in a fixed array).
pub const MAX_STAGE_CHANNELS: usize = super::processor::MAX_METER_CHANNELS;

/// Fades around installs, retirements and gaps, milliseconds (the
/// processor's swap fade half, as the stage's own cap-drop fade).
pub const STAGE_FADE_MS: f64 = vsp::CAP_FADE_MS;

/// Source frames a stage call can take (`2n + 1` at the largest call).
const MAX_SRC_FRAMES: usize = 2 * vsp::MAX_FRAMES + 2;

/// Retired stages the RT thread can hold when the recycle queue is full.
const STASH: usize = 2;

/// The longest fade (and source look-ahead), frames: 5 ms at 768 kHz.
const MAX_HOLD: usize = 3_840;

/// The most source a switch from varispeed to the stretch carries over
/// (the varispeed stage's own delay in source frames: about 230 at 48 kHz
/// and 1×, ×16 at 768 kHz and ×2 at 2×).
const MAX_CARRY: usize = 8_192;

/// The speed cap while a follower plays: a
/// follower applies `v·(1 + ε)` with `|ε|` up to 0.2 %, which must stay
/// within the stage's 2.0.
pub const FOLLOWED_MAX_SPEED: f64 = 1.996;

/// Commands the slot records between two stage calls for the tee's log.
const ISSUED: usize = 16;

/// The stage in the slot: varispeed's resampler (speed and pitch together)
/// or, with Keep pitch, the key-locked stretch (route B). Both have the same pull contract, envelope and cap, so the
/// slot and the driver run either the same way.
pub enum Stage {
    Core(Box<Varispeed>),
    Stretch(Box<TempoStretch>),
}

/// Forward a method both stages have.
macro_rules! both {
    ($self:ident, $s:ident => $e:expr) => {
        match $self {
            Stage::Core($s) => $e,
            Stage::Stretch($s) => $e,
        }
    };
}

impl Stage {
    pub fn is_stretch(&self) -> bool {
        matches!(self, Self::Stretch(_))
    }

    pub fn sample_rate(&self) -> u32 {
        both!(self, s => s.sample_rate())
    }

    pub fn channels(&self) -> usize {
        both!(self, s => s.channels())
    }

    pub fn max_frames(&self) -> usize {
        both!(self, s => s.max_frames())
    }

    pub fn speed(&self) -> f64 {
        both!(self, s => s.speed())
    }

    pub fn step(&self) -> audivane_dsp_core::adaptive::Step {
        both!(self, s => s.step())
    }

    pub fn target(&self) -> f64 {
        both!(self, s => s.target())
    }

    pub fn requested(&self) -> f64 {
        both!(self, s => s.requested())
    }

    pub fn requested_ramp_ms(&self) -> f64 {
        both!(self, s => s.requested_ramp_ms())
    }

    pub fn speed_limit(&self) -> f64 {
        both!(self, s => s.speed_limit())
    }

    pub fn ramping(&self) -> bool {
        both!(self, s => s.ramping())
    }

    /// A cap drop's fade (varispeed only: the stretch jumps).
    pub fn fading(&self) -> bool {
        match self {
            Self::Core(s) => s.fading(),
            Self::Stretch(_) => false,
        }
    }

    pub fn command_seq(&self) -> u64 {
        both!(self, s => s.command_seq())
    }

    pub fn fallbacks(&self) -> u64 {
        both!(self, s => s.fallbacks())
    }

    pub fn latency_frames(&self) -> f64 {
        both!(self, s => s.latency_frames())
    }

    pub fn latency_exact(&self) -> bool {
        both!(self, s => s.latency_exact())
    }

    pub fn tail_frames_at(&self, speed: f64) -> usize {
        both!(self, s => s.tail_frames_at(speed))
    }

    pub fn peak_until_settled(&self) -> f64 {
        both!(self, s => s.peak_until_settled())
    }

    /// The source position the next plan starts at: the core's (64.64 on
    /// the ×2 grid) or the stretch's (64.64 source frames).
    pub fn position(&self) -> u128 {
        match self {
            Self::Core(s) => s.core().accumulator().position(),
            Self::Stretch(s) => s.accumulator().position(),
        }
    }

    /// The core's position after `n` more frames (varispeed only: the
    /// stretch's commands are not logged for followers).
    pub fn position_after(&self, n: usize) -> Option<u128> {
        match self {
            Self::Core(s) => s.position_after(n),
            Self::Stretch(_) => None,
        }
    }

    pub fn set_speed(&mut self, speed: f64, ramp_ms: f64) -> Result<(), String> {
        match self {
            Self::Core(s) => s.set_speed(speed, ramp_ms).map_err(|e| e.to_string()),
            Self::Stretch(s) => s.set_speed(speed, ramp_ms).map_err(|e| e.to_string()),
        }
    }

    pub fn set_speed_limit(&mut self, max: f64) {
        both!(self, s => s.set_speed_limit(max))
    }

    /// Latch a call of `n` frames; the source frames it takes (`None`: the
    /// stage refused it).
    pub fn plan(&mut self, n: usize) -> Option<usize> {
        match self {
            Self::Core(s) => s.plan(n).ok(),
            Self::Stretch(s) => s.plan(n).ok(),
        }
    }

    pub fn process(&mut self, input: &[&[f64]], output: &mut [&mut [f64]]) {
        both!(self, s => s.process(input, output))
    }

    pub fn reset(&mut self) {
        both!(self, s => s.reset())
    }
}

/// The commands given to the stage, for the tee's command log
/// (`tee::CommandLog`): each is stamped at the next plan with the core's
/// position, where it starts. A reset supersedes everything before it.
#[derive(Clone, Copy, Debug)]
struct Issued {
    cmds: [(VsCommand, Option<u128>); ISSUED],
    len: usize,
    lost: bool,
}

impl Default for Issued {
    fn default() -> Self {
        Self {
            cmds: [(VsCommand::Lost, None); ISSUED],
            len: 0,
            lost: false,
        }
    }
}

impl Issued {
    fn push(&mut self, cmd: VsCommand) {
        if self.len == ISSUED {
            self.lost = true;
            return;
        }
        self.cmds[self.len] = (cmd, None);
        self.len += 1;
    }

    fn reset_to(&mut self, cmd: VsCommand) {
        self.len = 0;
        self.lost = false;
        self.push(cmd);
    }

    /// A command logged ahead, at core position `pos` (it applies there).
    fn push_at(&mut self, cmd: VsCommand, pos: u128) {
        self.push(cmd);
        if let Some(c) = self.cmds[..self.len].last_mut() {
            c.1 = Some(pos);
        }
    }

    /// A call is planned at core position `pos`: what was issued before it
    /// starts there.
    fn stamp(&mut self, pos: u128) {
        for c in &mut self.cmds[..self.len] {
            c.1.get_or_insert(pos);
        }
    }

    /// Hand out the stamped commands (in order), keeping the others.
    fn drain(&mut self, mut f: impl FnMut(u128, VsCommand)) {
        let stamped = self.cmds[..self.len]
            .iter()
            .take_while(|c| c.1.is_some())
            .count();
        if stamped == 0 {
            return;
        }
        if self.lost {
            self.lost = false;
            f(self.cmds[0].1.unwrap_or(0), VsCommand::Lost);
        }
        for &(cmd, at) in &self.cmds[..stamped] {
            f(at.unwrap_or(0), cmd);
        }
        self.cmds.copy_within(stamped..self.len, 0);
        self.len -= stamped;
    }
}

/// `set_speed_limit` on the stage, recorded when it changed the cap.
fn limit_stage(st: &mut Stage, issued: &mut Issued, max: f64) {
    let before = st.speed_limit();
    st.set_speed_limit(max);
    if st.speed_limit() != before {
        issued.push(VsCommand::Limit(st.speed_limit()));
    }
}

/// `set_speed` on the stage, recorded when it issued a command.
fn speed_stage(st: &mut Stage, issued: &mut Issued, speed: f64, ramp_ms: f64) {
    let before = st.command_seq();
    if st.set_speed(speed, ramp_ms).is_ok() && st.command_seq() != before {
        issued.push(VsCommand::Speed { speed, ramp_ms });
    }
}

/// The cap as the stage clamps it (`Varispeed::set_speed_limit`).
fn clamp_limit(max: f64) -> f64 {
    if max.is_finite() {
        max.clamp(vsp::MIN_SPEED, vsp::MAX_SPEED)
    } else {
        vsp::MAX_SPEED
    }
}

/// Commands logged ahead (multi-device): the driver's cap and speed, given
/// to the stage at the call that starts at output frame `at`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Scheduled {
    at: u64,
    limit: Option<f64>,
    speed: Option<(f64, f64)>,
}

/// What the control side sends the RT slot.
pub enum VarispeedRtCmd {
    /// A stage built off the RT thread for a graph rate and channel count.
    Install(Box<Varispeed>),
    /// The key-locked stretch, built the same way (Keep pitch on). It
    /// replaces a playing varispeed stage under a fade, and the reverse
    /// ([`StopKind::Switch`]).
    InstallStretch(Box<TempoStretch>),
    /// Retire the stage (under a fade while it plays).
    Disable,
}

/// Why a playing stage stops under a fade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopKind {
    /// Disabled: the stage retires.
    Retire,
    /// DSP off: the stage stays (stale); the processor applies the toggle.
    Suspend,
    /// Keep pitch toggled: the stage of the other kind waiting in the slot
    /// starts at the fade's end, at the driver's speed, from silence (the
    /// new stage's delay is the gap).
    Switch,
}

impl StopKind {
    /// Which of two stops wins: a retirement, then a suspend (its end
    /// still installs a waiting stage, stale), then a switch.
    fn rank(self) -> u8 {
        match self {
            Self::Retire => 2,
            Self::Suspend => 1,
            Self::Switch => 0,
        }
    }
}

/// Where the slot is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No stage.
    Off,
    /// A stage just (re)started while playing: the raw block plays on for
    /// `lead` frames (the stage's delay, while its output is silent), then
    /// fades out over `F` frames as the stage's faded-in source comes out.
    Enabling { pos: usize, lead: usize },
    /// The stage plays.
    On,
    /// The stage's output fades out over `F` frames, then it stops.
    ///
    /// `end`: the fade and then silence until the stop applies. After the
    /// stretch, the alignment delay still holds that much of its output
    /// (`VarispeedRt::set_post_delay`), which must play out before the
    /// stop changes what the delay is fed (its history restarts).
    Fading {
        pos: usize,
        kind: StopKind,
        end: usize,
    },
}

impl Phase {
    fn code(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Enabling { .. } => 1,
            Self::On => 2,
            Self::Fading { .. } => 3,
        }
    }
}

/// A raised-cosine gain on the stage's source frames (gaps and restarts).
#[derive(Clone, Copy, Debug, PartialEq)]
struct SourceFade {
    /// Gain at the next frame, before this frame's step.
    from: f64,
    /// Frames into the fade-in (`len` = done, gain 1).
    pos: usize,
    len: usize,
    /// New real frames silenced before the fade-in starts (they went to
    /// the raw block's fade-out instead: each frame plays on one side).
    mute: usize,
}

impl SourceFade {
    fn open(len: usize) -> Self {
        Self {
            from: 1.0,
            pos: len,
            len,
            mute: 0,
        }
    }

    fn is_open(&self) -> bool {
        self.pos >= self.len && self.from == 1.0 && self.mute == 0
    }

    /// Fade in from silence.
    fn restart(&mut self) {
        self.from = 0.0;
        self.pos = 0;
        self.mute = 0;
    }

    fn gain_at(&self, pos: usize) -> f64 {
        if pos >= self.len {
            return 1.0;
        }
        let t = (pos + 1) as f64 / self.len as f64;
        self.from + (1.0 - self.from) * (0.5 - 0.5 * (PI * t).cos())
    }

    /// Apply to a call's source, `ext[..hold + n]`: the `hold` frames held
    /// back from earlier calls, then the `n` new ones, the first `valid`
    /// real. The fade-in runs over the new real frames. With `underfeed`,
    /// the frames before the gap (at `hold + valid`) fade out over the fade
    /// length, held-back frames included, so a gap at the call's very start
    /// still fades; after it, silence, and a fade-in from the next frames
    /// on. Returns whether it faded out.
    fn apply(
        &mut self,
        ext: &mut [Vec<f64>],
        hold: usize,
        n: usize,
        valid: usize,
        underfeed: bool,
    ) -> bool {
        let valid = valid.min(n);
        if !self.is_open() {
            let real = if underfeed { valid } else { n };
            let muted = real.min(self.mute);
            for c in ext.iter_mut() {
                c[hold..hold + muted].fill(0.0);
            }
            self.mute -= muted;
            for i in muted..real {
                let g = self.gain_at(self.pos + i - muted);
                if g != 1.0 {
                    for c in ext.iter_mut() {
                        c[hold + i] *= g;
                    }
                }
            }
            self.pos = (self.pos + real - muted).min(self.len);
            if self.pos >= self.len {
                self.from = 1.0;
            }
        }
        if !underfeed {
            return false;
        }
        let gap = hold + valid;
        let start = gap.saturating_sub(self.len);
        let span = (gap - start) as f64;
        for c in ext.iter_mut() {
            for (j, x) in c[start..gap].iter_mut().enumerate() {
                let t = (j + 1) as f64 / span;
                *x *= 0.5 + 0.5 * (PI * t).cos();
            }
            c[gap..hold + n].fill(0.0);
        }
        self.restart();
        true
    }
}

/// The stage's buffers, moved out of the slot while a block uses them (a
/// `Vec` move, no allocation), so the processor can lend them to its
/// chains while it calls its own methods.
#[derive(Default)]
pub struct StageBuffers {
    /// Planar stage output (the chains' input), `n` frames.
    pub out: Vec<Vec<f64>>,
    /// The same, interleaved `f32`, for the input meter and the pre
    /// spectrum (what the chains hear).
    pub tap: Vec<f32>,
    src: Vec<Vec<f64>>,
    /// The last [`MAX_CARRY`] source frames varispeed's stage took (a ring,
    /// `hist_w` written in all): a switch to the stretch starts with the
    /// ones it had not played yet.
    hist: Vec<Vec<f64>>,
    hist_w: usize,
}

/// The applied state, published by the RT thread after every block and at
/// every reset: what is audible, never the configured target.
#[derive(Debug, Default)]
pub struct VarispeedApplied {
    wanted: AtomicBool,
    running: AtomicBool,
    stretch: AtomicBool,
    phase: AtomicU8,
    rate: AtomicU64,
    speed: AtomicU64,
    /// A sequence count around the two halves of the step (odd while the
    /// RT thread writes them), so a reader never mixes two steps.
    step_seq: AtomicU64,
    step_hi: AtomicU64,
    step_lo: AtomicU64,
    target: AtomicU64,
    requested: AtomicU64,
    limit: AtomicU64,
    ramping: AtomicBool,
    fading: AtomicBool,
    latency: AtomicU64,
    latency_exact: AtomicBool,
    fallbacks: AtomicU64,
    gap_fades: AtomicU64,
    installs: AtomicU64,
    retired: AtomicU64,
    refused_plans: AtomicU64,
    /// The last step a reader validated (control side only): what a read
    /// returns when the writer keeps it from validating a new one.
    last_step: std::sync::Mutex<Option<u128>>,
}

/// A snapshot of [`VarispeedApplied`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VarispeedAppliedSnapshot {
    /// Varispeed is enabled (a stage is installed or on its way).
    pub enabled: bool,
    /// The last block ran through the stage (DSP sink, DSP on, a stage for
    /// the running rate). Enabled at unity counts.
    pub active: bool,
    /// The installed stage is the key-locked stretch (Keep pitch).
    pub stretch: bool,
    /// 0 off, 1 enabling, 2 on, 3 fading out (disable, DSP off or a Keep
    /// pitch switch).
    pub phase: u8,
    /// The graph rate the installed stage was built for (0: none).
    pub stage_rate: u32,
    /// The applied speed (the step at the next output; 1 while inactive).
    pub speed: f64,
    /// Its exact 64.64 bits.
    pub step_bits: u128,
    pub target: f64,
    pub requested: f64,
    pub speed_limit: f64,
    pub ramping: bool,
    /// A cap drop's fade is in progress.
    pub fading: bool,
    /// The stage's delay (its look-ahead included), nominal frames (0
    /// while inactive).
    pub latency_frames: f64,
    pub latency_exact: bool,
    pub fallbacks: u64,
    /// Underfeed gaps faded over.
    pub gap_fades: u64,
    pub installs: u64,
    pub retired: u64,
    /// Blocks the stage could not plan (a guard: the driver splits periods
    /// to what one call takes); each played silence and is counted here.
    pub refused_plans: u64,
}

impl VarispeedApplied {
    pub fn snapshot(&self) -> VarispeedAppliedSnapshot {
        let f = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
        let r = Ordering::Relaxed;
        VarispeedAppliedSnapshot {
            enabled: self.wanted.load(r),
            active: self.running.load(r),
            stretch: self.stretch.load(r),
            phase: self.phase.load(r),
            stage_rate: self.rate.load(r) as u32,
            speed: f(&self.speed),
            step_bits: self.step_bits(),
            target: f(&self.target),
            requested: f(&self.requested),
            speed_limit: f(&self.limit),
            ramping: self.ramping.load(r),
            fading: self.fading.load(r),
            latency_frames: f(&self.latency),
            latency_exact: self.latency_exact.load(r),
            fallbacks: self.fallbacks.load(r),
            gap_fades: self.gap_fades.load(r),
            installs: self.installs.load(r),
            retired: self.retired.load(r),
            refused_plans: self.refused_plans.load(r),
        }
    }

    /// The step's two halves from one write (a seqlock read; the writer is
    /// the RT thread and never waits). When the retries run out, the last
    /// step validated (unity before any): never a mixed or unchecked pair.
    fn step_bits(&self) -> u128 {
        let mut last = self.last_step.lock().unwrap_or_else(|e| e.into_inner());
        for _ in 0..(1 << 16) {
            let s1 = self.step_seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let hi = self.step_hi.load(Ordering::Relaxed);
            let lo = self.step_lo.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.step_seq.load(Ordering::Relaxed) == s1 {
                let bits = (u128::from(hi) << 64) | u128::from(lo);
                *last = Some(bits);
                return bits;
            }
        }
        last.unwrap_or(1 << 64)
    }

    /// Write the step (RT side).
    fn store_step(&self, bits: u128) {
        let s = self.step_seq.load(Ordering::Relaxed);
        self.step_seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.step_hi.store((bits >> 64) as u64, Ordering::Relaxed);
        self.step_lo.store(bits as u64, Ordering::Relaxed);
        self.step_seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

/// The stage's slot in the RT processor (see the module docs).
pub struct VarispeedRt {
    stage: Option<Stage>,
    /// The stage of the other kind a Keep pitch toggle installs once the
    /// playing one has faded out ([`StopKind::Switch`]).
    next: Option<Stage>,
    /// The stage kind changed (a switch or an install of the other kind):
    /// the tee's domain changes (taken by the processor).
    switched: bool,
    phase: Phase,
    rx: Option<rtrb::Consumer<VarispeedRtCmd>>,
    recycle: Option<rtrb::Producer<Box<Varispeed>>>,
    /// Retired stretches (Keep pitch); without it they wait in the stash.
    stretch_recycle: Option<rtrb::Producer<Box<TempoStretch>>>,
    stash: [Option<Stage>; STASH],
    applied: Arc<VarispeedApplied>,
    wanted: bool,
    /// The latched call: `(n, n_src)`.
    planned: Option<(usize, usize)>,
    valid: usize,
    underfeed: bool,
    source: SourceFade,
    /// A block ran without the stage while it was on (Direct, DSP off):
    /// its history is stale, so it restarts when it runs again.
    stale: bool,
    /// The last block ran through the stage.
    ran: bool,
    /// The last block played the DSP sink's raw block (the stage did not
    /// run): a restart fades that block out under the stage's fade-in.
    raw_audible: bool,
    /// A stop waits for the driver to size the next pull for a raw one;
    /// `stop_armed` once it has (the next plan starts the fade).
    stop: Option<StopKind>,
    stop_armed: bool,
    /// The processor fades its output before a switch that drops the
    /// stage with the chain ([`Self::prepare_cut`]): the driver sizes the
    /// next pulls for raw ones, and [`Self::cut`] stops the stage.
    cut_pending: bool,
    /// The stage stopped under its fade-out (taken by the processor, which
    /// fades the raw block in and applies a deferred DSP toggle).
    stopped: Option<StopKind>,
    /// Fade length at the stage's rate, frames, and the source look-ahead.
    fade: usize,
    /// Source the look-ahead holds beyond the fade length: what varispeed's
    /// stage had taken but not played when the stretch replaced it.
    carry: usize,
    /// Frames the alignment delay holds after the stretch (multi-device):
    /// a stop of the stretch waits for them ([`Phase::Fading`]).
    post_delay: usize,
    /// The driver's last cap, speed and ramp, applied to every stage
    /// installed.
    limit: f64,
    speed: f64,
    ramp_ms: f64,
    bufs: StageBuffers,
    /// Source frames the stage has taken since its last restart (the tee
    /// maps its positions from them).
    fed: u64,
    issued: Issued,
    /// Multi-device: output frames a command waits before the stage
    /// applies it, logged at once at the position it applies at, so every
    /// follower has the entry before its own stage gets there (0: commands
    /// apply from the next call, as without followers).
    lead: usize,
    /// A follower is attached: the cap is at most [`FOLLOWED_MAX_SPEED`].
    followed: bool,
    /// Output frames the stage has run since its last restart.
    outs: u64,
    sched: Option<Scheduled>,
}

impl VarispeedRt {
    /// A slot that never gets a stage (no queues, no buffers).
    pub fn inert() -> Self {
        Self::build(None, None, Arc::default(), 0)
    }

    /// A slot fed by `rx`, retiring stages into `recycle`, for up to
    /// `channels` channels (buffers allocated here, off the RT thread).
    pub fn new(
        rx: rtrb::Consumer<VarispeedRtCmd>,
        recycle: rtrb::Producer<Box<Varispeed>>,
        applied: Arc<VarispeedApplied>,
        channels: usize,
    ) -> Self {
        Self::build(Some(rx), Some(recycle), applied, channels)
    }

    fn build(
        rx: Option<rtrb::Consumer<VarispeedRtCmd>>,
        recycle: Option<rtrb::Producer<Box<Varispeed>>>,
        applied: Arc<VarispeedApplied>,
        channels: usize,
    ) -> Self {
        let ch = channels.min(MAX_STAGE_CHANNELS);
        let rt = Self {
            stage: None,
            next: None,
            switched: false,
            phase: Phase::Off,
            rx,
            recycle,
            stretch_recycle: None,
            stash: std::array::from_fn(|_| None),
            applied,
            wanted: false,
            planned: None,
            valid: 0,
            underfeed: false,
            source: SourceFade::open(1),
            stale: false,
            ran: false,
            raw_audible: false,
            stop: None,
            stop_armed: false,
            cut_pending: false,
            stopped: None,
            fade: 1,
            carry: 0,
            post_delay: 0,
            limit: vsp::MAX_SPEED,
            speed: 1.0,
            ramp_ms: 0.0,
            bufs: StageBuffers {
                out: (0..ch).map(|_| vec![0.0; vsp::MAX_FRAMES]).collect(),
                tap: vec![0.0; vsp::MAX_FRAMES * ch],
                src: (0..ch)
                    .map(|_| vec![0.0; MAX_SRC_FRAMES + MAX_HOLD + MAX_CARRY])
                    .collect(),
                hist: (0..ch).map(|_| vec![0.0; MAX_CARRY]).collect(),
                hist_w: 0,
            },
            fed: 0,
            issued: Issued::default(),
            lead: 0,
            followed: false,
            outs: 0,
            sched: None,
        };
        rt.publish();
        rt
    }

    /// Where retired stretches go (built off-RT with the slot).
    pub fn with_stretch_recycle(mut self, tx: rtrb::Producer<Box<TempoStretch>>) -> Self {
        self.stretch_recycle = Some(tx);
        self
    }

    /// The installed stage is the stretch (Keep pitch).
    pub fn is_stretch(&self) -> bool {
        self.stage.as_ref().is_some_and(Stage::is_stretch)
    }

    /// The stage kind changed since the last call: the stretch writes the
    /// tee in its own (stretched) domain, so the tee restarts.
    pub fn take_switched(&mut self) -> bool {
        std::mem::take(&mut self.switched)
    }

    /// The applied-state handle.
    pub fn applied(&self) -> Arc<VarispeedApplied> {
        Arc::clone(&self.applied)
    }

    /// Retired stages still held on the RT thread (tests, diagnostics).
    pub fn stash_len(&self) -> usize {
        self.stash.iter().filter(|s| s.is_some()).count()
    }

    fn retire(&mut self, stage: Stage) {
        self.applied.retired.fetch_add(1, Ordering::Relaxed);
        self.flush_stash();
        let Some(stage) = self.push_recycle(stage) else {
            return;
        };
        for slot in &mut self.stash {
            if slot.is_none() {
                *slot = Some(stage);
                return;
            }
        }
        // Both full: never free on the RT thread.
        debug_assert!(false, "varispeed recycle queue and stash full");
        std::mem::forget(stage);
    }

    /// Into its recycle queue; the stage back when that is full (or there
    /// is none).
    fn push_recycle(&mut self, stage: Stage) -> Option<Stage> {
        match stage {
            Stage::Core(st) => match self.recycle.as_mut() {
                Some(tx) => match tx.push(st) {
                    Ok(()) => None,
                    Err(rtrb::PushError::Full(st)) => Some(Stage::Core(st)),
                },
                None => Some(Stage::Core(st)),
            },
            Stage::Stretch(st) => match self.stretch_recycle.as_mut() {
                Some(tx) => match tx.push(st) {
                    Ok(()) => None,
                    Err(rtrb::PushError::Full(st)) => Some(Stage::Stretch(st)),
                },
                None => Some(Stage::Stretch(st)),
            },
        }
    }

    fn flush_stash(&mut self) {
        for i in 0..STASH {
            if let Some(s) = self.stash[i].take() {
                self.stash[i] = self.push_recycle(s);
            }
        }
    }

    /// Room to retire one more stage without dropping it here, two while a
    /// stage waits to switch in (a command can retire it with the playing
    /// one).
    fn can_retire(&self) -> bool {
        let free = self.stash.iter().filter(|s| s.is_none()).count();
        let core = self.recycle.as_ref().map_or(0, |tx| tx.slots());
        let queued = match self.stretch_recycle.as_ref() {
            Some(tx) => core.min(tx.slots()),
            None => core,
        };
        free + queued >= if self.next.is_some() { 2 } else { 1 }
    }

    /// The source look-ahead, frames (the fade length, within the buffers).
    fn hold(&self) -> usize {
        self.fade.min(MAX_HOLD) + self.carry
    }

    /// Multi-device: the alignment delay's length after the stretch, frames
    /// (the most it holds, its fades included).
    pub fn set_post_delay(&mut self, frames: usize) {
        self.post_delay = frames;
    }

    /// Silence a stop of the stage plays after its fade before it applies:
    /// the alignment delay's, after the stretch (none after varispeed's
    /// stage, which the delay precedes).
    fn drain(&self) -> usize {
        if self.is_stretch() {
            self.post_delay
        } else {
            0
        }
    }

    /// A (re)start from the raw block. With the stretch, the raw block's
    /// fade-out frames are not given to the stretch as well (its output
    /// plays them 135 ms later): its source fades in after them.
    fn start_enabling(&mut self) {
        self.phase = self.enabling();
        if self.is_stretch() {
            self.source.mute = self.fade;
        }
    }

    /// Hold at `speed` (capped) with no history and the source faded in.
    fn restart_at(&mut self, speed: Option<f64>) {
        self.carry = 0;
        let fade = self.fade;
        let cap = self.cap();
        if let Some(st) = self.stage.as_mut() {
            st.set_speed_limit(cap);
            let s = speed
                .unwrap_or(self.speed)
                .clamp(vsp::MIN_SPEED, vsp::MAX_SPEED);
            let _ = st.set_speed(s, 0.0);
            st.reset();
            self.issued.reset_to(VsCommand::Reset {
                speed: s,
                limit: st.speed_limit(),
                ramp_ms: 0.0,
            });
        }
        self.fed = 0;
        self.outs = 0;
        self.sched = None;
        let hold = self.hold();
        for c in &mut self.bufs.src {
            c[..hold].fill(0.0);
        }
        self.source = SourceFade::open(fade);
        self.source.restart();
        self.planned = None;
        self.stale = false;
    }

    /// Drain the control queue (RT, from `poll_updates`). `streaming`: the
    /// DAC plays; `full`: the DSP chain plays.
    pub fn poll_cmds(&mut self, streaming: bool, full: bool) {
        self.flush_stash();
        let mut took = false;
        loop {
            // Keep a slot to retire into before taking a command that may
            // retire a stage.
            if !self.can_retire() {
                break;
            }
            let Some(cmd) = self.rx.as_mut().and_then(|rx| rx.pop().ok()) else {
                break;
            };
            took = true;
            match cmd {
                VarispeedRtCmd::Install(stage) => self.install(Stage::Core(stage), streaming, full),
                VarispeedRtCmd::InstallStretch(stage) => {
                    self.install(Stage::Stretch(stage), streaming, full)
                }
                VarispeedRtCmd::Disable => self.disable(streaming, full),
            }
        }
        if took {
            // A command alone (no block follows while idle) is published too.
            self.publish();
        }
    }

    fn install(&mut self, stage: Stage, streaming: bool, full: bool) {
        self.wanted = true;
        self.applied.installs.fetch_add(1, Ordering::Relaxed);
        let playing = self.plays(streaming, full);
        let kind = stage.is_stretch();
        if playing && let Some(cur) = self.stage.as_ref() {
            if cur.is_stretch() != kind {
                // Keep pitch toggled while playing: the playing stage fades
                // out, then this one starts from silence (its delay is the
                // gap; the driver keeps the speed).
                if let Some(old) = self.next.replace(stage) {
                    self.retire(old);
                }
                self.request_stop(StopKind::Switch);
                return;
            }
            if let Some(waiting) = self.next.take() {
                // Toggled back before the switch.
                let same =
                    cur.sample_rate() == stage.sample_rate() && cur.channels() == stage.channels();
                if same && self.stop == Some(StopKind::Switch) {
                    // Not fading yet: the playing stage stays.
                    self.stop = None;
                    self.stop_armed = false;
                    self.retire(waiting);
                    self.retire(stage);
                } else {
                    // Fading already (or another stop): the switch ends in
                    // this one.
                    self.retire(waiting);
                    self.next = Some(stage);
                }
                return;
            }
        }
        if let Some(old) = self.next.take() {
            self.retire(old);
        }
        self.fade =
            ((STAGE_FADE_MS * 1e-3 * f64::from(stage.sample_rate())).round() as usize).max(1);
        let audible = streaming && full && self.ran_recently();
        // Out of the stretch the tee's stream changes (into it from the raw
        // block or a stale stage, nothing the alignment holds is lost).
        if !kind && self.is_stretch() {
            self.switched = true;
        }
        if let Some(old) = self.stage.replace(stage) {
            self.retire(old);
        }
        self.stop = None;
        self.stop_armed = false;
        if audible {
            // The raw block plays now: fade it out while the stage starts
            // from silence at unity (the driver ramps to the request).
            self.restart_at(Some(1.0));
            self.start_enabling();
        } else {
            self.restart_at(None);
            self.source = SourceFade::open(self.fade);
            self.phase = Phase::On;
            // Playing elsewhere (Direct, DSP off): start fresh when the
            // DSP chain plays again.
            self.stale = streaming && !full;
        }
    }

    /// Whether the raw block (or a stage) is playing through the DSP chain
    /// now, so an install must fade.
    fn ran_recently(&self) -> bool {
        !self.stale
    }

    /// Whether the stage plays the DSP sink's output now.
    pub fn plays_now(&self, streaming: bool, full: bool) -> bool {
        self.plays(streaming, full)
    }

    fn plays(&self, streaming: bool, full: bool) -> bool {
        streaming
            && full
            && self.ran
            && matches!(
                self.phase,
                Phase::On | Phase::Enabling { .. } | Phase::Fading { .. }
            )
    }

    /// Stop a playing stage under a fade: at once if one already runs (a
    /// retire wins over a suspend), otherwise once the driver has sized the
    /// next pull for a raw one.
    fn request_stop(&mut self, kind: StopKind) {
        match &mut self.phase {
            Phase::Fading { kind: k, .. } => {
                if kind.rank() > k.rank() {
                    *k = kind;
                }
            }
            _ => {
                if self.stop.is_none_or(|k| kind.rank() > k.rank()) {
                    self.stop = Some(kind);
                }
            }
        }
    }

    /// The stage waiting for a switch replaces the playing one, which
    /// retires. `playing`: it starts now at the driver's speed, from silence
    /// (the fade-out has ended); otherwise it restarts when the DSP chain
    /// plays again (stale).
    fn switch_in(&mut self, playing: bool) {
        let Some(next) = self.next.take() else {
            return;
        };
        self.fade =
            ((STAGE_FADE_MS * 1e-3 * f64::from(next.sample_rate())).round() as usize).max(1);
        if let Some(old) = self.stage.replace(next) {
            self.retire(old);
        }
        self.switched = true;
        self.restart_at(None);
        self.phase = Phase::On;
        if !playing {
            self.source = SourceFade::open(self.fade);
            self.stale = true;
        }
    }

    fn disable(&mut self, streaming: bool, full: bool) {
        self.wanted = false;
        if let Some(waiting) = self.next.take() {
            self.retire(waiting);
        }
        if self.plays(streaming, full) {
            self.request_stop(StopKind::Retire);
            return;
        }
        if let Some(old) = self.stage.take() {
            self.retire(old);
        }
        self.phase = Phase::Off;
        self.planned = None;
        self.stop = None;
        self.stop_armed = false;
    }

    /// DSP off while the stage plays (RT, the processor's toggle): fade the
    /// stage out first. Returns whether it will (the processor then defers
    /// the toggle until [`Self::take_stopped`]).
    pub fn request_suspend(&mut self, streaming: bool) -> bool {
        if !self.plays(streaming, true) {
            return false;
        }
        self.request_stop(StopKind::Suspend);
        true
    }

    /// A requested stop that has not started fading is cancelled (the
    /// processor's toggle was taken back). Returns whether it was. Only a
    /// suspend is taken back: a retirement (varispeed turned off) was asked
    /// for on its own and still runs, or the host, which already counts the
    /// stage as gone, would leave it installed.
    pub fn cancel_stop(&mut self) -> bool {
        if matches!(self.phase, Phase::Fading { .. }) {
            return false;
        }
        if self.stop == Some(StopKind::Suspend) {
            // A switch asked for meanwhile still runs.
            self.stop = self.next.is_some().then_some(StopKind::Switch);
            if self.stop.is_none() {
                self.stop_armed = false;
            }
        }
        true
    }

    /// The processor fades its own output before a switch that drops the
    /// stage (the chain leaves too): until [`Self::cut`] or
    /// [`Self::cancel_cut`], the driver sizes pulls for raw ones.
    pub fn prepare_cut(&mut self) {
        self.cut_pending = true;
    }

    pub fn cancel_cut(&mut self) {
        self.cut_pending = false;
    }

    /// The processor's output fade reached silence: the stage stops now
    /// (installed, stale), with nothing audible to fade.
    pub fn cut(&mut self) {
        self.cut_pending = false;
        if self.stop == Some(StopKind::Switch) {
            // Nothing audible to fade: the waiting stage takes over (stale).
            self.stop = None;
            self.stop_armed = false;
            self.switch_in(false);
        }
        if self.stage.is_some() && !matches!(self.phase, Phase::Off) {
            self.phase = Phase::On;
            self.stale = true;
        }
    }

    /// Whether a stage can run a block at `graph_rate` with `channels`.
    fn fits(&self, graph_rate: u32, channels: usize) -> bool {
        !matches!(self.phase, Phase::Off)
            && channels <= self.bufs.out.len()
            && self
                .stage
                .as_ref()
                .is_some_and(|s| s.sample_rate() == graph_rate && s.channels() == channels)
    }

    /// The port for the next block, when a stage runs for it (the processor
    /// has checked the DSP chain plays). A stale stage restarts here: under
    /// the raw block's fade-out when that played just before.
    pub fn port(&mut self, graph_rate: u32, channels: usize) -> Option<&mut dyn VarispeedPort> {
        if !self.fits(graph_rate, channels) {
            return None;
        }
        if self.stale {
            self.restart_at(Some(1.0));
            if self.raw_audible {
                self.start_enabling();
            }
        }
        Some(self)
    }

    /// The source look-ahead in nominal frames at the applied speed.
    fn hold_frames(&self) -> f64 {
        let s = self.stage.as_ref().map_or(1.0, |st| st.speed());
        self.hold() as f64 / s.max(vsp::MIN_SPEED)
    }

    /// The stage's delay while it runs at `graph_rate`, nominal frames
    /// (the look-ahead included).
    pub fn latency_frames(&self, graph_rate: u32, channels: usize) -> Option<f64> {
        if !self.fits(graph_rate, channels) {
            return None;
        }
        self.stage
            .as_ref()
            .map(|s| s.latency_frames() + self.hold_frames())
    }

    /// The latched call, consumed by the block that follows the plan.
    pub fn take_planned(&mut self) -> Option<(usize, usize)> {
        self.planned.take()
    }

    /// Move the buffers out for a block (no allocation).
    pub fn take_buffers(&mut self) -> StageBuffers {
        std::mem::take(&mut self.bufs)
    }

    pub fn put_buffers(&mut self, bufs: StageBuffers) {
        self.bufs = bufs;
    }

    /// A block played without the stage (the DSP chain did not play, or
    /// the stage did not fit): its history goes stale. `dsp_raw`: it was
    /// the DSP sink's raw block.
    pub fn note_block_without_stage(&mut self, dsp_raw: bool) {
        if self.stage.is_some() {
            self.stale = true;
        }
        // The stage stopped running before its fade ended (its port went
        // away): the stop has nothing left to fade, so it ends now and a
        // deferred toggle never waits on it.
        if let Some(kind) = self.end_stop() {
            self.stopped = Some(kind);
        }
        self.ran = false;
        self.raw_audible = dsp_raw;
    }

    /// The cross-fade from the raw block at a (re)start: the raw block
    /// leads by the stage's delay (at the restart's unity speed), the
    /// look-ahead included.
    ///
    /// The stretch's delay is about 135 ms: a cross-fade after it would play
    /// that much music twice. The raw block fades out at once instead, and
    /// the stretch's output arrives after its delay (a gap, as at a Keep
    /// pitch toggle).
    fn enabling(&self) -> Phase {
        let lead = match self.stage.as_ref() {
            Some(s) if !s.is_stretch() => s.latency_frames() + self.hold_frames(),
            _ => 0.0,
        };
        Phase::Enabling {
            pos: 0,
            lead: lead.round() as usize,
        }
    }

    /// Whether a stop is requested or fading.
    pub fn stopping(&self) -> bool {
        self.stop.is_some() || matches!(self.phase, Phase::Fading { .. })
    }

    /// The slowest speed the stage plays at until it settles: the drain's
    /// tail and deadline use it, so a slowdown in progress is not cut short.
    fn slowest_speed(&self) -> f64 {
        self.stage
            .as_ref()
            .map_or(1.0, |s| s.speed().min(s.target()))
    }

    /// End a requested or fading stop at once (a retire retires now, a
    /// suspend leaves the stage installed and stale). Returns its kind.
    fn end_stop(&mut self) -> Option<StopKind> {
        let cut = match self.phase {
            Phase::Fading { kind, .. } => Some(kind),
            _ => self.stop,
        };
        self.switched |= cut.is_some() && self.is_stretch();
        match cut {
            Some(StopKind::Retire) => {
                if let Some(old) = self.stage.take() {
                    self.retire(old);
                }
                if let Some(old) = self.next.take() {
                    self.retire(old);
                }
                self.phase = Phase::Off;
            }
            Some(StopKind::Suspend) => {
                self.phase = Phase::On;
                self.stale = true;
                self.switch_in(false);
            }
            Some(StopKind::Switch) => {
                self.phase = Phase::On;
                self.stale = true;
                self.switch_in(false);
            }
            None => {}
        }
        self.stop = None;
        self.stop_armed = false;
        // A switch is the slot's own: nothing waits on it.
        cut.filter(|&k| k != StopKind::Switch)
    }

    /// A plan the stage refused played silence (counted).
    pub fn note_refused_plan(&mut self) {
        self.applied.refused_plans.fetch_add(1, Ordering::Relaxed);
    }

    /// Run the planned call on `dsp_in` (interleaved, `n_src` frames):
    /// the source look-ahead and fade, the stage, the phase's mix into
    /// `bufs.out[..n]`, and the interleaved tap. Returns `n`, or `None`
    /// when it cannot run (the block then plays nothing).
    pub fn run(
        &mut self,
        planned: (usize, usize),
        dsp_in: &[f32],
        channels: usize,
        bufs: &mut StageBuffers,
    ) -> Option<usize> {
        let (n, n_src) = planned;
        let hold = self.hold();
        let stage = self.stage.as_mut()?;
        if channels == 0
            || channels > bufs.out.len()
            || dsp_in.len() < n_src * channels
            || n > vsp::MAX_FRAMES
            || n_src > MAX_SRC_FRAMES
        {
            return None;
        }
        // `src` holds `hold` frames back from the last call; the new ones
        // follow them.
        for (c, chan) in bufs.src.iter_mut().take(channels).enumerate() {
            for (i, s) in chan[hold..hold + n_src].iter_mut().enumerate() {
                *s = f64::from(dsp_in[i * channels + c]);
            }
        }
        if self.source.apply(
            &mut bufs.src[..channels],
            hold,
            n_src,
            self.valid,
            self.underfeed,
        ) {
            self.applied.gap_fades.fetch_add(1, Ordering::Relaxed);
        }
        if !stage.is_stretch() {
            // What varispeed's stage takes, for a switch to the stretch.
            let w = bufs.hist_w;
            for (h, src) in bufs.hist.iter_mut().zip(&bufs.src).take(channels) {
                for (k, &x) in src[..n_src].iter().enumerate() {
                    h[(w + k) % MAX_CARRY] = x;
                }
            }
            bufs.hist_w = w.wrapping_add(n_src);
        }
        {
            let mut ins: [&[f64]; MAX_STAGE_CHANNELS] = [&[]; MAX_STAGE_CHANNELS];
            for (x, s) in ins.iter_mut().zip(bufs.src.iter()) {
                *x = &s[..n_src];
            }
            let mut outs: [&mut [f64]; MAX_STAGE_CHANNELS] = std::array::from_fn(|_| &mut [][..]);
            for (o, s) in outs.iter_mut().zip(bufs.out.iter_mut()) {
                *o = &mut s[..n];
            }
            stage.process(&ins[..channels], &mut outs[..channels]);
        }
        self.fed += n_src as u64;
        self.outs += n as u64;
        for chan in bufs.src.iter_mut().take(channels) {
            chan.copy_within(n_src..n_src + hold, 0);
        }
        let fade = self.fade;
        match self.phase {
            Phase::Enabling { pos, lead } => {
                // The raw block (unfaded source frames) plays until the
                // stage's output arrives, then fades out under it.
                for i in 0..n.min(n_src) {
                    let p = pos + i;
                    if p >= lead + fade {
                        break;
                    }
                    let w = if p < lead {
                        1.0
                    } else {
                        let t = (p - lead + 1) as f64 / fade as f64;
                        0.5 + 0.5 * (PI * t).cos()
                    };
                    for (c, chan) in bufs.out.iter_mut().take(channels).enumerate() {
                        chan[i] += w * f64::from(dsp_in[i * channels + c]);
                    }
                }
                let pos = pos + n;
                if pos >= lead + fade {
                    // Unity held through the lead and the cross-fade (the
                    // lead is the stage's delay at unity): now the request.
                    self.phase = Phase::On;
                    // With followers, the next plan schedules it.
                    let cap = self.cap();
                    let lead = self.lead();
                    if let Some(st) = self.stage.as_mut()
                        && lead == 0
                    {
                        limit_stage(st, &mut self.issued, cap);
                        speed_stage(st, &mut self.issued, self.speed, self.ramp_ms);
                    }
                } else {
                    self.phase = Phase::Enabling { pos, lead };
                }
            }
            Phase::Fading { pos, kind, end } => {
                for i in 0..n {
                    let p = pos + i;
                    let w = if p >= fade {
                        0.0
                    } else {
                        0.5 + 0.5 * (PI * (p + 1) as f64 / fade as f64).cos()
                    };
                    for chan in bufs.out.iter_mut().take(channels) {
                        chan[i] *= w;
                    }
                }
                let pos = pos + n;
                if pos >= end {
                    // Out of the stretch, the tee's stream changes; the
                    // alignment delay has played out its fade by now.
                    let was_stretch = self.is_stretch();
                    match kind {
                        StopKind::Retire => {
                            if let Some(old) = self.stage.take() {
                                self.retire(old);
                            }
                            if let Some(old) = self.next.take() {
                                self.retire(old);
                            }
                            self.phase = Phase::Off;
                            self.stopped = Some(kind);
                            self.switched |= was_stretch;
                        }
                        StopKind::Suspend => {
                            self.phase = Phase::On;
                            self.stale = true;
                            self.switch_in(false);
                            self.stopped = Some(kind);
                            self.switched |= was_stretch;
                        }
                        StopKind::Switch => {
                            // Varispeed's stage had taken this much source
                            // past the frame its fade ended on.
                            let unplayed = match &self.stage {
                                Some(Stage::Core(st)) => {
                                    ((st.latency_frames() - 0.5) * st.speed()).round() as usize
                                }
                                _ => 0,
                            };
                            // Fade, prime, fade: the new stage starts at
                            // the next call from silence; its own delay is
                            // the gap, and its source fades in.
                            self.switch_in(true);
                            self.carry_into(bufs, channels, unplayed);
                        }
                    }
                } else {
                    self.phase = Phase::Fading { pos, kind, end };
                }
            }
            Phase::On | Phase::Off => {}
        }
        for (c, chan) in bufs.out.iter().take(channels).enumerate() {
            for (i, &x) in chan[..n].iter().enumerate() {
                bufs.tap[i * channels + c] = x as f32;
            }
        }
        self.ran = true;
        self.raw_audible = false;
        Some(n)
    }

    /// At a switch (inside [`Self::run`], the buffers moved out): the new
    /// stage's look-ahead. Into the stretch it starts where varispeed's fade-out
    /// ended: the `unplayed` source frames that stage had taken (from the
    /// history), then the look-ahead it held, faded in from there, so no source
    /// frame is dropped or played twice (without a follower's alignment the
    /// source inside that delay is lost, a known limitation).
    /// Into varispeed's stage, from silence (the stretch's buffered source is
    /// not recoverable).
    fn carry_into(&mut self, bufs: &mut StageBuffers, channels: usize, unplayed: usize) {
        let held = self.hold();
        if !self.is_stretch() {
            for c in bufs.src.iter_mut() {
                c[..held].fill(0.0);
            }
            return;
        }
        let r = unplayed.min(MAX_CARRY).min(bufs.hist_w);
        let w = bufs.hist_w;
        for (src, h) in bufs.src.iter_mut().zip(&bufs.hist).take(channels) {
            src.copy_within(0..held, r);
            for k in 0..r {
                src[k] = h[(w + MAX_CARRY - r + k) % MAX_CARRY];
            }
        }
        self.carry = r;
        let len = held + r;
        let f = self.fade.min(len);
        for src in bufs.src.iter_mut().take(channels) {
            for (i, x) in src[..f].iter_mut().enumerate() {
                *x *= 0.5 - 0.5 * (PI * (i + 1) as f64 / self.fade as f64).cos();
            }
        }
        // The rest of the fade-in (if any) runs over the next new frames.
        self.source = SourceFade {
            from: if f >= self.fade { 1.0 } else { 0.0 },
            pos: f,
            len: self.fade,
            mute: 0,
        };
    }

    /// Multi-device: a follower is attached (the cap drops to
    /// [`FOLLOWED_MAX_SPEED`], from the next command or plan).
    pub fn set_followed(&mut self, on: bool) {
        self.followed = on;
    }

    /// The cap the stage gets: the driver's, within
    /// [`FOLLOWED_MAX_SPEED`] while a follower plays.
    fn cap(&self) -> f64 {
        if self.followed && !self.is_stretch() {
            clamp_limit(self.limit).min(FOLLOWED_MAX_SPEED)
        } else {
            self.limit
        }
    }

    /// Multi-device: commands wait `frames` output frames before they
    /// apply (0: none waits; see `lead`).
    pub fn set_command_lead(&mut self, frames: usize) {
        self.lead = frames.min(vsp::MAX_FRAMES);
    }

    /// The command lead that applies: none for the stretch, whose
    /// followers read its output and replay no command (its cap within
    /// [`FOLLOWED_MAX_SPEED`] is not needed either).
    fn lead(&self) -> usize {
        if self.is_stretch() { 0 } else { self.lead }
    }

    /// At the plan of a call of `n` frames, with a lead: give the stage the
    /// scheduled commands that are due, then schedule what the driver asked
    /// for since (logged now at the position it will apply at; the driver
    /// ends a call there, [`VarispeedPort::frames_until_stop`]).
    fn schedule(&mut self, n: usize) {
        let cap = self.cap();
        let lead = self.lead();
        let Some(st) = self.stage.as_mut() else {
            self.sched = None;
            return;
        };
        if let Some(s) = self.sched {
            if self.outs < s.at && lead > 0 {
                return;
            }
            if let Some(max) = s.limit {
                st.set_speed_limit(max);
            }
            if let Some((speed, ramp_ms)) = s.speed {
                let _ = st.set_speed(speed, ramp_ms);
            }
            self.sched = None;
        }
        if lead == 0 || n == 0 || self.phase != Phase::On {
            return;
        }
        let limit = clamp_limit(cap);
        let new_limit = (limit != st.speed_limit()).then_some(limit);
        let valid = (vsp::MIN_SPEED..=vsp::MAX_SPEED).contains(&self.speed)
            && (0.0..=vsp::MAX_RAMP_MS).contains(&self.ramp_ms);
        let new_speed = (valid
            && (self.speed != st.requested() || self.ramp_ms != st.requested_ramp_ms()))
        .then_some((self.speed, self.ramp_ms));
        if new_limit.is_none() && new_speed.is_none() {
            return;
        }
        let k = (lead + n).min(st.max_frames()).max(n);
        let Some(pos) = st.position_after(k) else {
            // A guard (in-range commands always plan): apply now.
            if let Some(max) = new_limit {
                limit_stage(st, &mut self.issued, max);
            }
            if let Some((speed, ramp_ms)) = new_speed {
                speed_stage(st, &mut self.issued, speed, ramp_ms);
            }
            return;
        };
        if let Some(max) = new_limit {
            self.issued.push_at(VsCommand::Limit(max), pos);
        }
        if let Some((speed, ramp_ms)) = new_speed {
            self.issued
                .push_at(VsCommand::Speed { speed, ramp_ms }, pos);
        }
        self.sched = Some(Scheduled {
            at: self.outs + k as u64,
            limit: new_limit,
            speed: new_speed,
        });
    }

    /// Where the stage is, for the tee (`None` without a stage, and for
    /// the stretch: the tee takes its output, at nominal speed): read
    /// after the driver's plan, before and after the call.
    pub fn tee_view(&self) -> Option<StageView> {
        let st = match self.stage.as_ref()? {
            Stage::Core(st) => st,
            Stage::Stretch(_) => return None,
        };
        Some(StageView {
            acc: st.core().accumulator().position(),
            fed: self.fed,
            hold: self.hold(),
            speed: st.speed(),
            // The next output sits half an output frame after the core's
            // next position (the ÷2 centres it on the second of its two
            // core outputs): measured against an impulse, at every speed.
            delay_src: (st.latency_frames() - 0.5) * st.speed(),
            ramping: st.ramping(),
            requested: st.requested(),
            requested_ramp_ms: st.requested_ramp_ms(),
            step: st.step().bits(),
            limit: st.speed_limit(),
            slowest: self.slowest_speed(),
            fastest: self.peak_until_settled(),
        })
    }

    /// Hand the commands given to the stage that a plan has placed (core
    /// position, command), in order; the tee logs them. Called after every
    /// block, so nothing piles up while no follower listens.
    pub fn drain_issued(&mut self, f: impl FnMut(u128, VsCommand)) {
        self.issued.drain(f);
    }

    /// The stage stopped under its fade-out (the processor fades the raw
    /// block in, and applies a deferred DSP toggle after a suspend).
    pub fn take_stopped(&mut self) -> Option<StopKind> {
        self.stopped.take()
    }

    /// DSP reset: the stage resets at the target (a stage fading out stops
    /// now). Returns the stop that was cut short, if any.
    pub fn reset_all(&mut self) -> Option<StopKind> {
        let cut = self.end_stop();
        self.cut_pending = false;
        if let Phase::Enabling { .. } = self.phase {
            self.phase = Phase::On;
        }
        self.stopped = None;
        self.restart_at(None);
        self.source = SourceFade::open(self.fade);
        self.ran = false;
        self.raw_audible = false;
        self.publish();
        cut
    }

    /// Publish the applied state.
    pub fn publish(&self) {
        let a = &self.applied;
        let r = Ordering::Relaxed;
        a.wanted.store(self.wanted, r);
        a.running.store(self.ran, r);
        a.stretch.store(self.is_stretch(), r);
        a.phase.store(self.phase.code(), r);
        match self.stage.as_ref() {
            Some(st) if self.ran => {
                a.speed.store(st.speed().to_bits(), r);
                a.store_step(st.step().bits());
                a.target.store(st.target().to_bits(), r);
                a.requested.store(st.requested().to_bits(), r);
                a.limit.store(st.speed_limit().to_bits(), r);
                a.ramping.store(st.ramping(), r);
                a.fading.store(st.fading(), r);
                a.latency
                    .store((st.latency_frames() + self.hold_frames()).to_bits(), r);
                a.latency_exact.store(st.latency_exact(), r);
                a.fallbacks.store(st.fallbacks(), r);
            }
            _ => {
                // Nothing changes the speed: unity is audible.
                a.speed.store(1.0_f64.to_bits(), r);
                a.store_step(1 << 64);
                a.target.store(self.speed.to_bits(), r);
                a.requested.store(self.speed.to_bits(), r);
                a.limit.store(self.cap().to_bits(), r);
                a.ramping.store(false, r);
                a.fading.store(false, r);
                a.latency.store(0.0_f64.to_bits(), r);
                a.latency_exact.store(true, r);
            }
        }
        a.rate.store(
            self.stage
                .as_ref()
                .map_or(0, |s| u64::from(s.sample_rate())),
            r,
        );
    }
}

impl VarispeedPort for VarispeedRt {
    fn plan(&mut self, n: usize) -> usize {
        self.planned = None;
        if self.stop.is_some() && self.stop_armed {
            // The driver sized this period for a raw pull after the stage:
            // the fade starts here.
            let kind = self.stop.take().unwrap_or(StopKind::Retire);
            self.stop_armed = false;
            self.phase = Phase::Fading {
                pos: 0,
                kind,
                end: self.fade + self.drain(),
            };
        }
        if self.lead() > 0 || self.sched.is_some() {
            self.schedule(n);
        }
        let Some(st) = self.stage.as_mut() else {
            return n;
        };
        match st.plan(n) {
            Some(n_src) => {
                if n > 0 {
                    self.issued.stamp(st.position());
                }
                self.planned = Some((n, n_src));
                self.valid = n_src;
                self.underfeed = false;
                n_src
            }
            None => {
                // Not reached (the driver splits a period into calls of at
                // most `max_frames`): the block plays silence, counted.
                self.planned = Some((n, n));
                self.valid = n;
                self.underfeed = false;
                n
            }
        }
    }

    fn set_fill(&mut self, valid: usize, underfeed: bool) {
        self.valid = valid;
        self.underfeed = underfeed;
    }

    fn set_speed_limit(&mut self, max: f64) {
        self.limit = max;
        if self.lead() > 0 {
            // Scheduled at the next plan.
            return;
        }
        let cap = self.cap();
        if let Some(st) = self.stage.as_mut() {
            limit_stage(st, &mut self.issued, cap);
        }
    }

    fn set_speed(&mut self, speed: f64, ramp_ms: f64) {
        self.speed = speed;
        self.ramp_ms = ramp_ms;
        if matches!(self.phase, Phase::Enabling { .. }) {
            // Unity until the cross-fade from the raw block ends (its lead
            // is the stage's delay at unity); the request applies then.
            return;
        }
        if self.lead() > 0 {
            return;
        }
        if let Some(st) = self.stage.as_mut() {
            speed_stage(st, &mut self.issued, speed, ramp_ms);
        }
    }

    fn pulled(&mut self) {
        // The driver has sized the next pull (covering a raw one,
        // `raw_next`) and refills for it before then: a requested stop
        // fades from the next plan, never in a pull the ring was not
        // refilled for (a retune on the same wake forwards too).
        if self.stop.is_some() {
            self.stop_armed = true;
        }
    }

    fn peak_until_settled(&self) -> f64 {
        let peak = self.stage.as_ref().map_or(1.0, |s| s.peak_until_settled());
        if matches!(self.phase, Phase::Enabling { .. }) {
            // The request starts when the cross-fade ends, maybe within the
            // next pull.
            return peak.max(self.speed);
        }
        if self.lead() > 0 {
            // A command waiting for its turn heads for the driver's target.
            return peak.max(self.speed.min(clamp_limit(self.cap())));
        }
        peak
    }

    fn speed(&self) -> f64 {
        self.stage.as_ref().map_or(1.0, |s| s.speed())
    }

    fn latency_frames(&self) -> f64 {
        self.stage
            .as_ref()
            .map_or(0.0, |s| s.latency_frames() + self.hold_frames())
    }

    fn reset(&mut self) {
        self.reset_all();
    }

    fn max_frames(&self) -> usize {
        self.stage
            .as_ref()
            .map_or(vsp::MAX_FRAMES, |s| s.max_frames())
    }

    fn raw_next(&self) -> bool {
        self.stopping() || self.cut_pending
    }

    fn tail_frames(&self) -> usize {
        let s = self.slowest_speed();
        self.stage.as_ref().map_or(0, |st| {
            st.tail_frames_at(s) + (self.hold() as f64 / s.max(vsp::MIN_SPEED)).ceil() as usize
        })
    }

    fn drain_speed(&self) -> f64 {
        self.slowest_speed()
    }

    fn frames_until_stop(&self) -> Option<usize> {
        let stop = match self.phase {
            Phase::Fading { pos, end, .. } => Some(end.saturating_sub(pos).max(1)),
            _ if self.stop.is_some() && self.stop_armed => Some(self.fade + self.drain()),
            _ => None,
        };
        // A scheduled command starts a call.
        let due = self
            .sched
            .filter(|s| s.at > self.outs)
            .map(|s| (s.at - self.outs) as usize);
        match (stop, due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// The control side: builds a stage for the running graph rate (off the RT
/// thread) and sends it, or retires it.
pub struct VarispeedHost {
    tx: Option<rtrb::Producer<VarispeedRtCmd>>,
    channels: usize,
    enabled: bool,
    /// The rate the last stage sent was built for (cleared only once its
    /// retirement is queued).
    built_rate: Option<u32>,
    /// A retirement the full queue refused: sent again at the next call
    /// or [`Self::reconcile`].
    pending_disable: bool,
    /// An install the full queue refused: built again at the next call or
    /// [`Self::reconcile`].
    pending_install: bool,
    /// The overload fallback holds the stage off (the request is kept).
    held_off: bool,
    /// Keep pitch: the stage built is the stretch.
    keep_pitch: bool,
    /// The kind of the last stage sent (the stretch: true).
    built_stretch: bool,
    /// Why the last build was refused (budget, design).
    pub last_error: Option<String>,
}

impl VarispeedHost {
    /// Error string returned when the RT command ring is temporarily full and
    /// the install or retirement is queued for the reaper to retry.
    pub const QUEUE_FULL_ERR: &'static str = "varispeed: the RT queue is full";

    /// A host with no RT slot (tests and tools that run no stage).
    pub fn detached() -> Self {
        Self {
            tx: None,
            channels: 0,
            enabled: false,
            built_rate: None,
            pending_disable: false,
            pending_install: false,
            held_off: false,
            keep_pitch: false,
            built_stretch: false,
            last_error: None,
        }
    }

    pub fn new(tx: rtrb::Producer<VarispeedRtCmd>, channels: usize) -> Self {
        Self {
            tx: Some(tx),
            channels,
            enabled: false,
            built_rate: None,
            pending_disable: false,
            pending_install: false,
            held_off: false,
            keep_pitch: false,
            built_stretch: false,
            last_error: None,
        }
    }

    /// Whether a retirement still waits for room in the queue.
    pub fn disable_pending(&self) -> bool {
        self.pending_disable
    }

    /// Whether an install or a retirement still waits for room in the
    /// queue.
    pub fn pending(&self) -> bool {
        self.pending_disable || self.pending_install
    }

    /// Send what the queue refused again (the reaper's tick, until the
    /// queue takes it): the retirement, or the install for `graph_rate`
    /// within `budget`.
    pub fn reconcile(&mut self, graph_rate: u32, budget: Result<(), String>) -> Result<(), String> {
        if !self.pending() {
            return Ok(());
        }
        if !self.enabled() {
            self.pending_install = false;
            return self.retire_stage();
        }
        self.on_rate(graph_rate, budget)
    }

    /// Queue the stage's retirement; on a full queue keep it pending (the
    /// next call sends it again) and keep `built_rate`.
    fn retire_stage(&mut self) -> Result<(), String> {
        if self.built_rate.is_none() && !self.pending_disable {
            return Ok(());
        }
        match self.send(VarispeedRtCmd::Disable) {
            Ok(()) => {
                self.built_rate = None;
                self.pending_disable = false;
                Ok(())
            }
            Err(e) => {
                self.pending_disable = true;
                Err(e)
            }
        }
    }

    /// Keep pitch: the stage is the key-locked stretch.
    pub fn keep_pitch(&self) -> bool {
        self.keep_pitch
    }

    /// Keep pitch on or off (an internal API until the protocol step):
    /// while a stage runs, one of the other kind is built for its rate and
    /// sent; the slot fades the playing one out and starts it (fade,
    /// prime, fade). `budget`: the static check for the new kind. A
    /// refusal leaves the setting as it was.
    pub fn set_keep_pitch(
        &mut self,
        on: bool,
        graph_rate: u32,
        budget: Result<(), String>,
    ) -> Result<(), String> {
        if on == self.keep_pitch {
            return Ok(());
        }
        if let Err(e) = budget {
            self.last_error = Some(e.clone());
            return Err(e);
        }
        self.keep_pitch = on;
        if !self.enabled() {
            return Ok(());
        }
        let res = self.on_rate(graph_rate, Ok(()));
        if let Err(ref e) = res
            && e != Self::QUEUE_FULL_ERR
        {
            self.keep_pitch = !on;
        }
        res
    }

    /// Whether a stage may run: requested and not held off.
    pub fn enabled(&self) -> bool {
        self.enabled && !self.held_off
    }

    /// Whether varispeed is requested (also while the overload fallback
    /// holds it off).
    pub fn requested(&self) -> bool {
        self.enabled
    }

    /// The overload fallback holds the stage off (`off`), retiring it, or
    /// lets it run again: built at `graph_rate` when requested, within
    /// `budget`.
    pub fn set_held_off(
        &mut self,
        off: bool,
        graph_rate: u32,
        budget: Result<(), String>,
    ) -> Result<(), String> {
        self.held_off = off;
        if off {
            return self.retire_stage();
        }
        self.on_rate(graph_rate, budget)
    }

    /// The rate the installed (or sent) stage runs at.
    pub fn built_rate(&self) -> Option<u32> {
        self.built_rate
    }

    /// Enable at `graph_rate` (a stage is built when none is there for it),
    /// or disable. `budget` is the static check's verdict for the running
    /// configuration: a refused enable leaves varispeed off.
    pub fn set_enabled(
        &mut self,
        enabled: bool,
        graph_rate: u32,
        budget: Result<(), String>,
    ) -> Result<(), String> {
        if !enabled {
            self.enabled = false;
            return self.retire_stage();
        }
        if let Err(e) = budget {
            self.last_error = Some(e.clone());
            // A retirement still pending goes out anyway.
            let _ = self.retire_stage();
            return Err(e);
        }
        let was_enabled = self.enabled;
        self.enabled = true;
        if self.held_off {
            // Requested; it runs once the overload fallback lets it.
            return Ok(());
        }
        // Enabled again before a pending retirement went out: the stage on
        // the RT side stays (it was never told to go).
        let was_pending_disable = self.pending_disable;
        self.pending_disable = false;
        let res = self.on_rate(graph_rate, Ok(()));
        if let Err(ref e) = res
            && e != Self::QUEUE_FULL_ERR
        {
            self.enabled = was_enabled;
            self.pending_disable = was_pending_disable;
        }
        res
    }

    /// The graph runs at `graph_rate` (a negotiation): while enabled, a
    /// stage for it replaces one built for another rate. A budget the new
    /// configuration breaks retires the stage (varispeed stays enabled and
    /// comes back where it fits).
    pub fn on_rate(&mut self, graph_rate: u32, budget: Result<(), String>) -> Result<(), String> {
        if !self.enabled() {
            self.pending_install = false;
            return self.retire_stage();
        }
        if let Err(e) = budget {
            self.pending_install = false;
            self.last_error = Some(e.clone());
            self.retire_stage()?;
            return Err(e);
        }
        // The stage that was to retire fits again: it stays.
        self.pending_disable = false;
        if self.built_rate == Some(graph_rate) && self.built_stretch == self.keep_pitch {
            return Ok(());
        }
        let cmd = self
            .build(graph_rate)
            .inspect_err(|e| self.last_error = Some(e.clone()))?;
        // A full queue: built and sent again later (the stage is dropped
        // here, off the RT thread).
        self.pending_install = true;
        self.send(cmd)?;
        self.pending_install = false;
        self.built_rate = Some(graph_rate);
        self.built_stretch = self.keep_pitch;
        self.last_error = None;
        Ok(())
    }

    /// A stage of the kind Keep pitch asks for, at `graph_rate`.
    fn build(&self, graph_rate: u32) -> Result<VarispeedRtCmd, String> {
        let channels = self.channels.max(1);
        if self.keep_pitch {
            let st = TempoStretch::new(TempoSpec {
                sample_rate: graph_rate,
                channels,
                max_frames: vsp::MAX_FRAMES,
                speed: 1.0,
            })
            .map_err(|e| format!("tempo: {e}"))?;
            return Ok(VarispeedRtCmd::InstallStretch(Box::new(st)));
        }
        let st = Varispeed::new(VarispeedSpec {
            sample_rate: graph_rate,
            channels,
            max_frames: vsp::MAX_FRAMES,
            speed: 1.0,
        })
        .map_err(|e| e.to_string())?;
        Ok(VarispeedRtCmd::Install(Box::new(st)))
    }

    fn send(&mut self, cmd: VarispeedRtCmd) -> Result<(), String> {
        let Some(tx) = self.tx.as_mut() else {
            return Ok(());
        };
        tx.push(cmd).map_err(|_| Self::QUEUE_FULL_ERR.to_string())
    }
}

#[cfg(test)]
#[path = "varispeed_tests.rs"]
mod processor_tests;

#[cfg(test)]
#[path = "varispeed_driver_tests.rs"]
mod driver_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taking_back_dsp_off_keeps_a_pending_retirement() {
        // Varispeed turned off while the stage plays (the retirement waits
        // for the driver to size a raw pull), then DSP off and back on
        // before the fade starts: the suspend is taken back, the
        // retirement is not.
        let (_tx, rx) = rtrb::RingBuffer::<VarispeedRtCmd>::new(4);
        let (recycle_tx, _recycle_rx) = rtrb::RingBuffer::new(4);
        let mut rt = VarispeedRt::new(rx, recycle_tx, Arc::default(), 2);
        rt.stage = Some(Stage::Core(Box::new(
            vsp::Varispeed::new(vsp::VarispeedSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        )));
        rt.phase = Phase::On;
        rt.ran = true;
        rt.wanted = true;
        rt.disable(true, true);
        assert_eq!(rt.stop, Some(StopKind::Retire));
        assert!(rt.request_suspend(true));
        assert_eq!(rt.stop, Some(StopKind::Retire), "a retire wins");
        assert!(rt.cancel_stop());
        assert_eq!(rt.stop, Some(StopKind::Retire), "the retirement was lost");
        // A suspend alone is taken back.
        rt.stop = None;
        assert!(rt.request_suspend(true));
        assert_eq!(rt.stop, Some(StopKind::Suspend));
        assert!(rt.cancel_stop());
        assert_eq!(rt.stop, None);
    }

    #[test]
    fn work_a_full_queue_refused_goes_out_at_the_reconcile() {
        let (tx, mut rx) = rtrb::RingBuffer::<VarispeedRtCmd>::new(1);
        let mut host = VarispeedHost::new(tx, 2);
        // The install fits; the retirement after it finds the queue full.
        host.set_enabled(true, 48_000, Ok(())).unwrap();
        assert!(host.set_held_off(true, 48_000, Ok(())).is_err());
        assert!(host.pending());
        // The same hold again while it waits is not a no-op: the reaper's
        // reconcile sends it once the RT thread made room.
        assert!(host.reconcile(48_000, Ok(())).is_err(), "still full");
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Install(_))));
        host.reconcile(48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Disable)));
        assert!(!host.pending());
        assert_eq!(host.built_rate(), None);
        // Restored on a full queue: the install goes out at the reconcile.
        let _ = host.send(VarispeedRtCmd::Disable);
        assert!(host.set_held_off(false, 48_000, Ok(())).is_err());
        assert!(host.pending());
        assert_eq!(host.built_rate(), None);
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Disable)));
        host.reconcile(48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Install(_))));
        assert!(!host.pending());
        assert_eq!(host.built_rate(), Some(48_000));
        // Nothing pending: the reconcile does nothing.
        host.reconcile(48_000, Ok(())).unwrap();
        assert!(rx.pop().is_err());
    }

    #[test]
    fn a_step_read_that_never_validates_returns_the_last_validated_one() {
        let a = VarispeedApplied::default();
        // Nothing validated yet: unity, never the unchecked pair.
        a.step_hi.store(7, Ordering::Relaxed);
        a.step_seq.store(1, Ordering::Release);
        assert_eq!(a.step_bits(), 1 << 64);
        a.step_seq.store(2, Ordering::Release);
        let half = 1u128 << 63;
        a.store_step(half);
        assert_eq!(a.step_bits(), half);
        // A writer stuck mid-write (odd sequence), its halves torn: the
        // last step validated comes back, not the torn pair or zero.
        a.step_seq.fetch_add(1, Ordering::Release);
        a.step_hi.store(3, Ordering::Relaxed);
        a.step_lo.store(0, Ordering::Relaxed);
        assert_eq!(a.step_bits(), half);
        assert_eq!(a.snapshot().step_bits, half);
    }

    #[test]
    fn a_gap_fades_out_before_it_and_in_after_it() {
        let len = 8;
        let mut f = SourceFade::open(len);
        // Held back: `len` frames, then 32 new ones.
        let mut ext = vec![vec![1.0; len + 32]];
        // Steady: untouched.
        assert!(!f.apply(&mut ext, len, 32, 32, false));
        assert!(ext[0].iter().all(|&x| x == 1.0));
        // A gap 20 frames into the new ones (index 28): fades from 20 to
        // 28, zeros after.
        assert!(f.apply(&mut ext, len, 32, 20, true));
        assert!(ext[0][..20].iter().all(|&x| x == 1.0));
        assert!(ext[0][20..27].windows(2).all(|w| w[1] < w[0]));
        assert!(ext[0][27].abs() < 1e-15);
        assert!(ext[0][28..].iter().all(|&x| x == 0.0));
        // Then a fade-in from silence over `len` new frames.
        let mut ext = vec![vec![1.0; len + 32]];
        f.apply(&mut ext, len, 32, 32, false);
        assert!(
            ext[0][..len].iter().all(|&x| x == 1.0),
            "held back: untouched"
        );
        assert!(ext[0][len] > 0.0 && ext[0][len] < 0.1);
        assert!(ext[0][len..2 * len].windows(2).all(|w| w[1] > w[0]));
        assert!(ext[0][2 * len - 1..].iter().all(|&x| x == 1.0));
        assert!(f.is_open());
    }

    #[test]
    fn a_gap_at_a_call_s_start_fades_the_frames_held_back() {
        // A full call, then one with no frames at all: the fade-out runs
        // over the frames held back from the full one.
        let len = 8;
        let mut f = SourceFade::open(len);
        let mut ext = vec![vec![0.75; len + 16]];
        assert!(f.apply(&mut ext, len, 16, 0, true));
        assert!(ext[0][..len - 1].windows(2).all(|w| w[1] < w[0]));
        assert!(ext[0][0] > 0.7, "{}", ext[0][0]);
        assert!(ext[0][len - 1].abs() < 1e-15);
        assert!(ext[0][len..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_gap_inside_a_fade_in_starts_from_the_gain_reached() {
        let mut f = SourceFade::open(100);
        f.restart();
        let mut chans = vec![vec![1.0; 50]];
        f.apply(&mut chans, 0, 50, 10, true);
        // Never above the fade-in it interrupts.
        for (i, &x) in chans[0][..10].iter().enumerate() {
            assert!(x <= SourceFade::open(100).gain_at(i).max(f.gain_at(i)) + 1e-12);
            assert!(x <= 0.1);
        }
    }

    #[test]
    fn a_full_queue_keeps_the_retirement_for_a_retry() {
        let (tx, mut rx) = rtrb::RingBuffer::<VarispeedRtCmd>::new(1);
        let mut host = VarispeedHost::new(tx, 2);
        host.set_enabled(true, 48_000, Ok(())).unwrap();
        assert_eq!(host.built_rate(), Some(48_000));
        // The install still sits in the queue: the disable does not fit.
        assert!(host.set_enabled(false, 48_000, Ok(())).is_err());
        assert!(host.disable_pending());
        assert_eq!(host.built_rate(), Some(48_000), "kept until it is queued");
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Install(_))));
        // The retry sends it.
        host.set_enabled(false, 48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Disable)));
        assert!(!host.disable_pending());
        assert_eq!(host.built_rate(), None);
        // A negotiation retries it too, and a budget refusal keeps it.
        host.set_enabled(true, 48_000, Ok(())).unwrap();
        assert!(host.on_rate(48_000, Err("over".into())).is_err());
        assert!(host.disable_pending());
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Install(_))));
        assert!(host.on_rate(48_000, Err("over".into())).is_err());
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Disable)));
        assert!(!host.disable_pending());
    }

    /// A slot at 48 kHz, two channels, the same source on both, run in
    /// calls as the driver does (plan, fill, run, pulled).
    struct SlotRig {
        rt: VarispeedRt,
        tx: rtrb::Producer<VarispeedRtCmd>,
        _recycle: (
            rtrb::Consumer<Box<Varispeed>>,
            rtrb::Consumer<Box<TempoStretch>>,
        ),
        pulled: usize,
        out: Vec<f64>,
    }

    impl SlotRig {
        fn new() -> Self {
            let (tx, rx) = rtrb::RingBuffer::new(8);
            let (rtx, rrx) = rtrb::RingBuffer::new(8);
            let (stx, srx) = rtrb::RingBuffer::new(8);
            let rt = VarispeedRt::new(rx, rtx, Arc::default(), 2).with_stretch_recycle(stx);
            Self {
                rt,
                tx,
                _recycle: (rrx, srx),
                pulled: 0,
                out: Vec::new(),
            }
        }

        fn send(&mut self, cmd: VarispeedRtCmd) {
            assert!(self.tx.push(cmd).is_ok());
            self.rt.poll_cmds(true, true);
        }

        fn call(&mut self, n: usize, src: &dyn Fn(usize) -> f64) {
            let port: &mut dyn VarispeedPort = self.rt.port(48_000, 2).expect("a stage");
            let n_src = port.plan(n);
            port.set_fill(n_src, false);
            let input: Vec<f32> = (0..n_src)
                .flat_map(|i| {
                    let x = src(self.pulled + i) as f32;
                    [x, x]
                })
                .collect();
            let mut bufs = self.rt.take_buffers();
            let ran = self.rt.run((n, n_src), &input, 2, &mut bufs);
            assert_eq!(ran, Some(n));
            self.out.extend_from_slice(&bufs.out[0][..n]);
            self.rt.put_buffers(bufs);
            VarispeedPort::pulled(&mut self.rt);
            self.pulled += n_src;
        }
    }

    fn core48() -> VarispeedRtCmd {
        VarispeedRtCmd::Install(Box::new(
            Varispeed::new(VarispeedSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        ))
    }

    fn stretch48() -> VarispeedRtCmd {
        VarispeedRtCmd::InstallStretch(Box::new(
            TempoStretch::new(TempoSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 8192,
                speed: 1.0,
            })
            .unwrap(),
        ))
    }

    /// Where `x` peaks above `level` (one index per peak).
    fn peaks(x: &[f64], level: f64) -> Vec<usize> {
        let mut at: Vec<usize> = Vec::new();
        for (i, v) in x.iter().enumerate() {
            if v.abs() > level && at.last().is_none_or(|&l| i > l + 50) {
                at.push(i);
            }
        }
        at
    }

    #[test]
    fn raw_to_stretch_gives_each_source_frame_to_one_side() {
        // The stretch starts from the raw block at 1×. The raw block fades
        // out over its first 240 frames, and those frames are not also given
        // to the stretch, whose output would play them again 135 ms later. A
        // pulse in the middle of the raw fade plays once (raw, at half
        // level), a pulse after it once (stretched).
        let mut r = SlotRig::new();
        r.send(stretch48());
        assert!(matches!(r.rt.phase, Phase::Enabling { lead: 0, .. }));
        let src = |i: usize| if i == 120 || i == 600 { 1.0 } else { 0.0 };
        for _ in 0..80 {
            r.call(240, &src);
        }
        let at = peaks(&r.out, 0.05);
        let lat = r.rt.latency_frames(48_000, 2).unwrap().round() as usize;
        println!("raw to stretch: pulses at {at:?}, the stretch's delay {lat}");
        assert_eq!(at.len(), 2, "{at:?}");
        assert_eq!(at[0], 120, "the raw block's");
        // The stretch reads behind its look-ahead, then its own delay.
        assert!(at[1].abs_diff(600 + lat) <= 2, "{at:?} {lat}");
    }

    #[test]
    fn core_to_stretch_carries_the_unplayed_source() {
        // Keep pitch on while varispeed's stage plays. Its output fades out
        // over 240 frames; the stretch then starts where that fade ended:
        // with the source the stage had taken but not played, and its
        // look-ahead. Pulses there play once (stretched); one before the
        // fade plays once (varispeed's).
        let run = |pulses: &[usize]| {
            let mut r = SlotRig::new();
            r.send(core48());
            let src = |i: usize| if pulses.contains(&i) { 1.0 } else { 0.0 };
            for _ in 0..40 {
                r.call(240, &src);
            }
            r.send(stretch48());
            assert_eq!(r.rt.stop, Some(StopKind::Switch));
            // Armed at the next `pulled` (after this call): the call after
            // it is the fade.
            r.call(240, &src);
            assert!(!r.rt.is_stretch());
            r.call(240, &src);
            assert!(r.rt.is_stretch(), "switched at the fade's end");
            let switch = (r.out.len(), r.pulled, r.rt.carry, r.rt.hold());
            for _ in 0..40 {
                r.call(240, &src);
            }
            (r, switch)
        };
        let (_, (_, pulled, carry, hold)) = run(&[]);
        let took = pulled - 240;
        println!("the switch: {pulled} pulled, {took} taken, {carry} carried, look-ahead {hold}");
        assert!(carry > 100, "{carry}");
        assert_eq!(hold, 240 + carry);
        // Before the fade (played by varispeed's stage), in its unplayed
        // source, in its look-ahead.
        let before = took - carry - 240 - 400;
        let unplayed = took - carry / 3;
        let ahead = pulled - 100;
        let (r, (switch_out, ..)) = run(&[before, unplayed, ahead]);
        let early = peaks(&r.out[..switch_out], 0.3);
        let late = peaks(&r.out[switch_out..], 0.3);
        println!("core to stretch: before the switch {early:?}, after it {late:?}");
        assert_eq!(early.len(), 1, "{early:?}");
        assert_eq!(late.len(), 2, "each carried pulse once: {late:?}");
        assert!(late[1] - late[0] == ahead - unplayed, "{late:?}");
    }

    #[test]
    fn keep_pitch_builds_the_stretch_and_back() {
        let (tx, mut rx) = rtrb::RingBuffer::<VarispeedRtCmd>::new(8);
        let mut host = VarispeedHost::new(tx, 2);
        // Varispeed off: kept for later, nothing built.
        host.set_keep_pitch(true, 48_000, Ok(())).unwrap();
        assert!(host.keep_pitch() && rx.pop().is_err());
        host.set_enabled(true, 48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::InstallStretch(_))));
        // Off again: a varispeed stage for the same rate.
        host.set_keep_pitch(false, 48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Install(_))));
        host.set_keep_pitch(false, 48_000, Ok(())).unwrap();
        assert!(rx.pop().is_err(), "the same setting again builds nothing");
        // A budget refusal keeps the setting and sends nothing.
        assert!(
            host.set_keep_pitch(true, 48_000, Err("over".into()))
                .is_err()
        );
        assert!(!host.keep_pitch() && rx.pop().is_err());
        // A negotiation builds the kind asked for.
        host.set_keep_pitch(true, 48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::InstallStretch(_))));
        host.on_rate(96_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::InstallStretch(_))));
        assert_eq!(host.built_rate(), Some(96_000));
    }

    #[test]
    fn the_overload_hold_retires_the_stage_and_keeps_the_request() {
        let (tx, mut rx) = rtrb::RingBuffer::<VarispeedRtCmd>::new(8);
        let mut host = VarispeedHost::new(tx, 2);
        host.set_enabled(true, 48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Install(_))));
        host.set_held_off(true, 48_000, Ok(())).unwrap();
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Disable)));
        assert!(host.requested() && !host.enabled());
        // Held off, a negotiation builds nothing, and a new enable waits.
        host.on_rate(96_000, Ok(())).unwrap();
        host.set_enabled(true, 96_000, Ok(())).unwrap();
        assert!(rx.pop().is_err());
        // Released: built for the rate that runs.
        host.set_held_off(false, 96_000, Ok(())).unwrap();
        assert!(host.enabled());
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Install(_))));
        assert_eq!(host.built_rate(), Some(96_000));
        // A request turned off meanwhile stays off when released.
        host.set_held_off(true, 96_000, Ok(())).unwrap();
        host.set_enabled(false, 96_000, Ok(())).unwrap();
        host.set_held_off(false, 96_000, Ok(())).unwrap();
        assert!(!host.enabled());
        assert!(matches!(rx.pop(), Ok(VarispeedRtCmd::Disable)));
        assert!(rx.pop().is_err());
    }

    #[test]
    fn the_published_step_is_never_torn() {
        // The two halves around unity differ in both words: a reader that
        // mixed two writes would see 0 or about 2.
        let a = Arc::new(VarispeedApplied::default());
        let below: u128 = (1u128 << 64) - 1;
        let above: u128 = 1u128 << 64;
        a.store_step(below);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (a, stop) = (Arc::clone(&a), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    a.store_step(if i.is_multiple_of(2) { above } else { below });
                    i += 1;
                    for _ in 0..16 {
                        std::hint::spin_loop();
                    }
                }
            })
        };
        for _ in 0..2_000_000 {
            let b = a.snapshot().step_bits;
            assert!(b == below || b == above, "torn step {b:#x}");
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }
}
