//! The follower output: each follower DAC plays the master's source on its
//! own clock, held in sync with the master at the DAC.
//!
//! ```text
//! tee ring ──► source queue ──► fade ──► stage at v (1 + ε) ──► chain ──► ALSA
//!   (index)      (by index)               (log replay)          (its own)    │
//!                                ▲                                            │
//!                                └── drift loop ◄── detector ◄── readings ◄──┘
//!                                                       ▲
//!                                     the master's position at its DAC
//! ```
//!
//! - **The source** comes from the follower's tee ring by source index
//!   (`SourceQueue`): a gap (Direct, DSP off, a stalled ring) reads as
//!   silence, a skip moves the index, and a little history behind the read
//!   point lets a repeat or a stage restart take frames again.
//! - **The stage** is the varispeed stage in follower mode
//!   (`Varispeed::set_follower`): the master's commands, replayed from the
//!   command log at the source positions they were logged at, with ε from
//!   the drift loop on every step. It always runs (a follower is on another
//!   clock, so it is never bit-perfect).
//! - **The chain** is the follower's own instance, built for its device from
//!   its config slot (v1: the master's config, chain A only). A config edit
//!   builds a new one on the control side ([`FollowerHost::config_changed`]);
//!   it warms beside the playing one and takes over under a crossfade (or a
//!   fade through silence when the latency changes), as the master's; the
//!   old one goes back to the control side to be dropped.
//! - **The drift loop** runs once per period on the follower's own wakeup:
//!   the follower's reading through its [`PositionMap`], the master's
//!   published position, the [`PhaseDetector`] and the [`DriftLoop`] (with
//!   the outlier gate). Large errors skip whole source frames while muted.
//! - **Audible** only while locked, with the master playing the DSP path,
//!   source to play and no skip or restart pending: otherwise the source
//!   fades out over [`FADE_MS`] (raised cosine) before the stage, and back in.
//! - **Loss.** An xrun re-primes and the loop sees a phase jump (a skip). An
//!   unplug or another device error closes the device, marks the follower
//!   lost and reopens it once the device is back.
//! - **Placement.** Once a follower has locked, its loop's rate state is
//!   remembered ([`FollowerControls::remembered_rate`], per rate family) and
//!   kept through a pause: a new master stream (a new position epoch), a
//!   reopen (seeded from the memory) or a phase jump from lock places it
//!   from its first readings (`DriftLoop::realign`), heard within the
//!   placement floor in a fraction of a second, instead of an acquisition.
//!   The master never waits on any of it: the tee drops what a follower's
//!   ring cannot take.
//!
//! **Threads.** One per follower ([`FollowerHost`]), at real-time priority
//! one step below the master's ([`FOLLOWER_RT_PRIORITY`]). It opens the
//! device and builds the stage and chain off the audio path, then runs
//! [`run_device`], which is fixed-size and allocation-free per period
//! ([`FollowerRt`], `tests/processor_no_alloc.rs`). Its device comes through
//! [`FollowerPcm`]: ALSA ([`AlsaFollowerPcm`]) or a simulation in tests.
//!
//! Followers are added through the engine's internal API
//! (`RealEngine::add_follower`); nothing reaches the protocol yet.

use super::measure::{FollowerMeasure, MeasureFault, MeasureSlot};
use super::processor::{
    ActiveChain, DspIn, MAX_METER_CHANNELS, VolumeTarget, handover_budget, warm_ring_cap,
};
use super::tee::{
    F64Cell, LogRead, LoggedCommand, MAX_COMP_MS, MAX_FOLLOWERS, MAX_GRAPH_RATE, TeeHost,
    TeeReader, TeeShared, VsCommand, max_comp_frames, step_frames,
};
use audivane_dsp_core::drift::{
    BiasTracker, DriftLoop, EPS_MAX, FRAME, LoopConfig, PhaseDetector, PositionMap, RateEstimator,
    Reading, hermite,
};
use audivane_dsp_core::gain::StereoGainRamp;
use audivane_dsp_core::varispeed::{MAX_SPEED, Varispeed, VarispeedSpec};
use audivane_protocol::ipc::{self, EngineConfig, FollowerConfig, MeasureChain};
use audivane_protocol::{RateRatio, SampleFormat};
use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The trim's range, ms.
pub const MAX_TRIM_MS: f64 = 50.0;
/// The most a follower plays late when the master's alignment is at its
/// cap, ms: the tee ring holds that much on top of the alignment at the
/// highest graph rate.
pub const MAX_LAG_MS: f64 = super::tee::MAX_LAG_MS;
/// The source fade around muting, ms.
pub const FADE_MS: f64 = 5.0;
/// The quietest level a follower that follows the master's plays at, dB;
/// the loudest is 0 dB (a master raised past the level its own was set at
/// stops it there, and its offset holds again on the way down).
const FOLLOW_SPAN_DB: f64 = -ipc::VOLUME_MIN_DB;
const FOLLOW_FLOOR_DB: f64 = -2.0 * FOLLOW_SPAN_DB;
/// The follower thread's real-time priority: one below the master's (20).
pub const FOLLOWER_RT_PRIORITY: u32 = 19;
/// The largest stage call, graph frames.
pub const STAGE_FRAMES: usize = 4096;
/// Source frames kept behind the read point, at least (a repeat, a stage
/// restart); [`history_frames`] sizes it for the trim's range.
const MIN_HISTORY: usize = 2048;
/// Playback time kept behind the read point beyond a whole trim swing
/// (`2 ×` [`MAX_TRIM_MS`]), ms: a known step back of up to that much (a
/// trim edit, the lateness, the master's alignment) finds its source.
const HISTORY_MARGIN_MS: f64 = 10.0;
/// The fade's look-ahead at the highest rate: 5 ms at 768 kHz.
const MAX_FADE: usize = 3840;
/// Source frames the queue holds ahead of the history: a call's source at
/// speed 2 and the fade's look-ahead, with room.
const QUEUE_AHEAD: usize = 2 * STAGE_FRAMES + 2 + MAX_FADE + 4096;

/// The history at `graph_rate`, source frames: a whole trim swing and the
/// margin, at the fastest speed (source frames per playback time).
fn history_frames(graph_rate: u32) -> usize {
    let ms = 2.0 * MAX_TRIM_MS + HISTORY_MARGIN_MS;
    ((ms * 1e-3 * f64::from(graph_rate) * MAX_SPEED).ceil() as usize).max(MIN_HISTORY)
}
/// Log entries one call looks at, at most (bounded RT time).
const LOG_STEPS: usize = 2 * super::tee::LOG_LEN;
/// The longest a known step waits for a clear stretch of source (a cap
/// that never settles), seconds.
const DEFER_MAX_S: f64 = 1.0;
/// How long a follower stays shut after it ran short of source, seconds.
const STARVED_S: f64 = 0.1;
/// The window the source slack is the minimum over, seconds, and the
/// first one of a stream (the alignment it sets comes within a fraction of
/// a second: a follower is heard from the first second).
const SLACK_WINDOW_S: f64 = 1.0;
const FIRST_SLACK_WINDOW_S: f64 = 0.25;
/// The master's command lead before any follower has measured its own,
/// ms, and the margin over the largest measured one.
pub const DEFAULT_LEAD_MS: f64 = 30.0;
pub const LEAD_MARGIN_MS: f64 = 5.0;
/// A chain swap, as the master's (`processor::SwapPhase`): the new chain
/// warms beside the playing one for its latency and this margin (device
/// frames), then a crossfade (equal latencies, ms) or a fade out, the
/// switch and a fade in (ms each).
const SWAP_WARM_MARGIN: usize = 1024;
const SWAP_CROSSFADE_MS: f64 = 10.0;
const SWAP_FADE_HALF_MS: f64 = 5.0;
/// Readings held after a chain latency change on either side, seconds: the
/// other side's swap lands within it, and the loop never sees the step.
const CHAIN_HOLD_S: f64 = 3.0;
/// How long a follower's compensation target holds still before its pad
/// follows it, seconds: a chain edit that reaches both sides swaps them a
/// little apart, and the target comes back by itself. Longer than the
/// master's ([`COMP_SETTLE_S`]), so a change the master's compensation
/// takes up never moves the pad.
const PAD_SETTLE_S: f64 = 0.5;
/// How long the followers' need for the master's compensation holds still
/// before it applies, seconds.
pub const COMP_SETTLE_S: f64 = 0.25;
/// The pad's fade out and in around a change of its length, ms.
const PAD_FADE_MS: f64 = 5.0;

/// `x` source frames in 64.64.
fn fixed(x: f64) -> i128 {
    (x * FRAME as f64).round() as i128
}

fn bits(x: f64) -> u64 {
    x.to_bits()
}

fn unbits(x: u64) -> f64 {
    f64::from_bits(x)
}

// ---------------------------------------------------------------------------
// Status and controls.

/// A follower's state, as the status reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowerState {
    /// Not running (stopped, or not started yet).
    Off,
    /// Opening its device or building its chain.
    Opening,
    /// Playing, the loop not locked: muted.
    Acquiring,
    /// Playing in sync (audible unless the master plays something else).
    Locked,
    /// Its device is gone or failed: waiting to reopen it.
    Lost,
}

impl FollowerState {
    fn code(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Opening => 1,
            Self::Acquiring => 2,
            Self::Locked => 3,
            Self::Lost => 4,
        }
    }

    fn from_code(c: u8) -> Self {
        match c {
            1 => Self::Opening,
            2 => Self::Acquiring,
            3 => Self::Locked,
            4 => Self::Lost,
            _ => Self::Off,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Opening => "opening",
            Self::Acquiring => "acquiring",
            Self::Locked => "locked",
            Self::Lost => "lost",
        }
    }
}

/// What the follower thread publishes (atomics; the error text only off
/// the audio path).
#[derive(Debug, Default)]
pub struct FollowerStatus {
    state: AtomicU8,
    audible: AtomicBool,
    ppm: AtomicU64,
    phase_error: AtomicU64,
    bandwidth: AtomicU64,
    eps: AtomicU64,
    skips: AtomicU64,
    known_steps: AtomicU64,
    xruns: AtomicU64,
    gated: AtomicU64,
    underfeeds: AtomicU64,
    late_commands: AtomicU64,
    refused_plans: AtomicU64,
    latency_ms: AtomicU64,
    slack_ms: AtomicU64,
    slack_low_ms: AtomicU64,
    slack_speed: AtomicU64,
    lead_ms: AtomicU64,
    device_rate: AtomicU64,
    period: AtomicU64,
    device_channels: AtomicU64,
    device_channels_max: AtomicU64,
    reopens: AtomicU64,
    /// Its chain's latency as installed, without the pad: graph frames
    /// (`NaN` while closed) and device frames; the pad, device frames.
    chain_graph: F64Cell,
    chain_frames: AtomicU64,
    pad_frames: AtomicU64,
    /// What its compensation cannot cover, ms (positive: late).
    comp_late_ms: AtomicU64,
    /// The level it plays at, dB (`NaN` before its first block).
    level_db: F64Cell,
    error: Mutex<Option<String>>,
}

/// A copy of [`FollowerStatus`].
#[derive(Clone, Debug, PartialEq)]
pub struct FollowerSnapshot {
    pub state: FollowerState,
    /// Heard now (locked, the master on the DSP path, nothing pending).
    pub audible: bool,
    /// The follower's clock offset relative to the master, ppm (the loop's
    /// rate state).
    pub ppm: f64,
    /// The loop's filtered phase error, source frames.
    pub phase_error_frames: f64,
    pub bandwidth_hz: f64,
    /// ε applied now.
    pub eps: f64,
    /// Skips the loop made (an error it could not place).
    pub skips: u64,
    /// Steps it took exactly, without the loop (a trim or lateness change,
    /// a known step of the master's).
    pub known_steps: u64,
    pub xruns: u64,
    /// Readings the outlier gate replaced.
    pub gated: u64,
    /// Stage calls that found less source than they needed.
    pub underfeeds: u64,
    /// Speed commands that reached the follower after its stage had passed
    /// their source position (applied at once).
    pub late_commands: u64,
    pub refused_plans: u64,
    /// The follower's own path, ms: from taking a source frame to playing
    /// it (its stage, chain and device queue).
    pub latency_ms: f64,
    /// The least source it had ahead of its read point over the last
    /// second, ms of playback (negative: it ran out, the master's
    /// alignment needs that much more). `NaN` before any.
    pub slack_ms: f64,
    /// The least of the lower bounds the slack's windows gave since the
    /// last change the follower cannot account for (as [`Self::slack_ms`]
    /// starts again), ms: what the follower had ahead at its worst, at any
    /// phase of the clocks. The master's alignment less this is what the
    /// follower needs. `NaN` before a window, and while the master does
    /// not play.
    pub slack_low_ms: f64,
    /// The speed its slack is playback time of (the stage's: source frames
    /// over the graph rate times it; 1 by default).
    pub slack_speed: f64,
    /// How far its stage ran ahead of the master's over the last second,
    /// ms of playback (the most): the master holds speed commands back by
    /// more, so the follower reads each before it gets there. `NaN` before
    /// any.
    pub lead_ms: f64,
    pub device_rate: u32,
    pub period: u32,
    /// The channel count its device opened at, and the most it takes (0
    /// before an open).
    pub device_channels: u32,
    pub device_channels_max: u32,
    pub reopens: u64,
    /// Its chain's latency as it plays, without its compensation pad:
    /// graph frames (`NaN` while closed) and device frames.
    pub chain_graph: f64,
    pub chain_frames: u64,
    /// Its compensation pad after the chain, device frames.
    pub pad_frames: u64,
    /// What the compensation cannot cover, ms: positive, it plays that
    /// much later than the master (its chain is longer than the master's
    /// compensation can cover); negative, earlier (its pad is at its most).
    pub comp_late_ms: f64,
    /// The level it plays at, dB (`NaN` before its first block).
    pub level_db: f64,
    pub error: Option<String>,
}

impl FollowerStatus {
    fn set_state(&self, s: FollowerState) {
        self.state.store(s.code(), Ordering::Relaxed);
    }

    pub fn state(&self) -> FollowerState {
        FollowerState::from_code(self.state.load(Ordering::Relaxed))
    }

    /// Off the audio path only.
    fn set_error(&self, e: Option<String>) {
        if let Ok(mut g) = self.error.lock() {
            *g = e;
        }
    }

    pub fn snapshot(&self) -> FollowerSnapshot {
        let r = Ordering::Relaxed;
        FollowerSnapshot {
            state: self.state(),
            audible: self.audible.load(r),
            ppm: unbits(self.ppm.load(r)),
            phase_error_frames: unbits(self.phase_error.load(r)),
            bandwidth_hz: unbits(self.bandwidth.load(r)),
            eps: unbits(self.eps.load(r)),
            skips: self.skips.load(r),
            known_steps: self.known_steps.load(r),
            xruns: self.xruns.load(r),
            gated: self.gated.load(r),
            underfeeds: self.underfeeds.load(r),
            late_commands: self.late_commands.load(r),
            refused_plans: self.refused_plans.load(r),
            latency_ms: unbits(self.latency_ms.load(r)),
            slack_ms: unbits(self.slack_ms.load(r)),
            slack_low_ms: unbits(self.slack_low_ms.load(r)),
            slack_speed: {
                let v = unbits(self.slack_speed.load(r));
                if v.is_finite() && v > 0.0 { v } else { 1.0 }
            },
            lead_ms: unbits(self.lead_ms.load(r)),
            device_rate: self.device_rate.load(r) as u32,
            period: self.period.load(r) as u32,
            device_channels: self.device_channels.load(r) as u32,
            device_channels_max: self.device_channels_max.load(r) as u32,
            reopens: self.reopens.load(r),
            chain_graph: self.chain_graph.get(),
            chain_frames: self.chain_frames.load(r),
            pad_frames: self.pad_frames.load(r),
            comp_late_ms: unbits(self.comp_late_ms.load(r)),
            level_db: self.level_db.get(),
            error: self.error.lock().ok().and_then(|g| g.clone()),
        }
    }
}

/// The protocol's view of configured follower `f`: its running state
/// (`None`: not running), its lateness beyond the trim (ms) and why it
/// could not start.
pub fn view(
    f: &FollowerConfig,
    snap: Option<&FollowerSnapshot>,
    lag_ms: f64,
    failure: Option<&str>,
    own: Option<(Vec<String>, audivane_protocol::ChainConfig)>,
) -> ipc::FollowerStatus {
    use ipc::FollowerState as W;
    let finite = |x: f64| if x.is_finite() { x } else { 0.0 };
    // What its compensation cannot cover counts as unaligned too.
    let off_ms = lag_ms + snap.map_or(0.0, |s| finite(s.comp_late_ms));
    let state = match snap {
        None => W::Off,
        Some(s) => match s.state {
            FollowerState::Off => W::Off,
            FollowerState::Opening => W::Opening,
            FollowerState::Acquiring => W::Locking,
            FollowerState::Lost => W::Lost,
            FollowerState::Locked if !s.audible => W::Muted,
            FollowerState::Locked if off_ms != 0.0 => W::Unaligned,
            FollowerState::Locked => W::Locked,
        },
    };
    ipc::FollowerStatus {
        device: f.device.clone(),
        enabled: f.enabled,
        trim_ms: f.trim_ms,
        volume_db: f.volume_db,
        follow_volume: f.follow_volume,
        level_db: Some(
            snap.map(|s| s.level_db)
                .filter(|l| l.is_finite())
                .unwrap_or(f.volume_db),
        ),
        state,
        offset_ms: off_ms,
        ppm: snap.map_or(0.0, |s| finite(s.ppm)),
        phase_error_frames: snap.map_or(0.0, |s| finite(s.phase_error_frames)),
        bandwidth_hz: snap.map_or(0.0, |s| finite(s.bandwidth_hz)),
        skips: snap.map_or(0, |s| s.skips),
        xruns: snap.map_or(0, |s| s.xruns),
        latency_ms: snap.map_or(0.0, |s| finite(s.latency_ms)),
        device_rate: snap.map_or(0, |s| s.device_rate),
        period: snap.map_or(0, |s| s.period),
        period_ms: f.period_ms,
        need_ms: None,
        output_channels: f.output_channels,
        device_channels: snap.map_or(0, |s| s.device_channels),
        device_channels_max: snap.map_or(0, |s| s.device_channels_max),
        error: snap
            .and_then(|s| s.error.clone())
            .or_else(|| failure.map(str::to_string)),
        own_chain: own.as_ref().map(|o| o.0.clone()).unwrap_or_default(),
        chain: own.map(|o| Box::new(o.1)),
        latency: match snap {
            Some(s) if s.device_rate > 0 && s.chain_graph.is_finite() => {
                ipc::PathLatency::from_frames(
                    s.device_rate,
                    s.chain_frames as f64,
                    s.pad_frames as f64,
                    0.0,
                )
            }
            _ => ipc::PathLatency::default(),
        },
    }
}

/// A follower's level state packed in one `u64`: its own volume (high
/// half) and its offset to the master's level (low half, `NaN`: it does not
/// follow), each an `f32` in dB (exact for whole and half dB, within 1e-5
/// dB otherwise).
#[derive(Clone, Copy, Debug, PartialEq)]
struct LevelWord(u64);

impl LevelWord {
    fn new(volume_db: f64, follow_offset_db: Option<f64>) -> Self {
        let own = if volume_db.is_finite() {
            volume_db.clamp(ipc::VOLUME_MIN_DB, 0.0)
        } else {
            0.0
        };
        let offset = follow_offset_db
            .filter(|o| o.is_finite())
            .map_or(f32::NAN, |o| {
                o.clamp(-2.0 * FOLLOW_SPAN_DB, 2.0 * FOLLOW_SPAN_DB) as f32
            });
        Self((u64::from((own as f32).to_bits()) << 32) | u64::from(offset.to_bits()))
    }

    fn get(self) -> (f64, Option<f64>) {
        let own = f64::from(f32::from_bits((self.0 >> 32) as u32));
        let offset = f64::from(f32::from_bits(self.0 as u32));
        (own, offset.is_finite().then_some(offset))
    }
}

/// The level a follower plays at (dB), the balance on its inputs and its
/// mute: its own volume alone, or, following, the master's level plus its
/// offset (at most 0 dB) with the master's balance and mute.
pub(crate) fn heard_level(
    own_db: f64,
    follow_offset_db: Option<f64>,
    m: &VolumeTarget,
) -> (f64, f64, bool) {
    match follow_offset_db {
        Some(offset) if m.volume_db.is_finite() => {
            let balance = if m.balance_db.is_finite() {
                m.balance_db
            } else {
                0.0
            };
            let db = (offset + m.volume_db).clamp(FOLLOW_FLOOR_DB, 0.0);
            (db, balance, m.muted)
        }
        _ => (own_db, 0.0, false),
    }
}

/// What the control side sets on a running follower.
#[derive(Debug)]
pub struct FollowerControls {
    trim_ms: AtomicU64,
    /// Its lateness beyond the trim when the master's alignment is at its
    /// cap (it plays unaligned), ms.
    lag_ms: AtomicU64,
    /// Its level state in one word ([`LevelWord`]): its own volume and,
    /// while it follows, its offset to the master's level. One load reads
    /// both, so the audio thread never pairs one's new value with the
    /// other's old one (an untick heard for a block at the old level).
    level: AtomicU64,
    enabled: AtomicBool,
    stop: AtomicBool,
    /// Its path is padded to the master's chain latency
    /// ([`TeeShared::master_chain_graph`]); set by [`FollowerHost`].
    compensate: AtomicBool,
    /// Reopen the device (its config wants another ratio).
    reopen: AtomicBool,
    /// Its device period, ms (`f64` bits; `NaN`: the default), read at
    /// each open.
    period_ms: AtomicU64,
    /// Measurement jobs for this follower (`measure_play`).
    measure: Arc<MeasureSlot>,
    /// What its loop has learnt about it with the current master, per
    /// device rate family (48 and 44.1 kHz: a DAC may have a crystal for
    /// each): the last locked rate state (ε, `NaN` before any) and both
    /// devices' short-window reading bias (RMS, seconds). A (re)open starts
    /// from it; [`FollowerHost`] keeps it per pair, on disk.
    memory: [[AtomicU64; 3]; 2],
    /// Bumped when the memory is replaced (another master): the loop
    /// starts again from the new pair's.
    pair_gen: AtomicU64,
}

/// What a follower's loop learnt about a master and follower pair at one
/// rate family.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClockMemory {
    /// The follower's clock against the master's, ε units (`None`: never
    /// locked).
    pub eps: Option<f64>,
    /// The master's and the follower's short-window reading bias, RMS,
    /// seconds.
    pub master_bias_s: f64,
    pub follower_bias_s: f64,
}

/// The standard deviations of the devices' short-window reading bias a
/// placement allows for, where it is not taken out of the readings.
const BIAS_SIGMAS: f64 = 2.0;

/// How far a point of the master's off its line is held back for a period
/// (seconds): past a coarse 1 ms grid's step and its async packets. A late
/// reading spans a whole master period of the follower's readings (seven
/// at 128-frame periods against the master's 1024): read as a jump, it
/// would place the follower out and back.
const MASTER_ODD_S: f64 = 1.5e-3;

/// A tracker's short-window bias RMS, seconds, where it is known (0: not).
fn known_rms(t: &BiasTracker) -> f64 {
    if t.rms_known() { t.rms_s() } else { 0.0 }
}

/// Start a loop from what is remembered of its pair: placed from its first
/// readings at the remembered rate, or at the nominal one for a pair never
/// seen (heard at once either way).
fn seed_loop(lp: &mut DriftLoop, mem: ClockMemory) {
    match mem.eps {
        Some(eps) => lp.seed(eps),
        None => lp.seed_nominal(),
    }
}

/// The rate family of a device rate: 0 for multiples of 8 kHz, 1 for
/// those of 11.025 kHz.
fn rate_family(rate: u32) -> usize {
    usize::from(!rate.is_multiple_of(8000) && rate.is_multiple_of(11_025))
}

impl FollowerControls {
    pub fn new(trim_ms: f64, volume_db: f64) -> Self {
        Self {
            trim_ms: AtomicU64::new(bits(trim_ms)),
            lag_ms: AtomicU64::new(bits(0.0)),
            level: AtomicU64::new(LevelWord::new(volume_db, None).0),
            enabled: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            compensate: AtomicBool::new(false),
            reopen: AtomicBool::new(false),
            period_ms: AtomicU64::new(bits(f64::NAN)),
            measure: Arc::new(MeasureSlot::default()),
            memory: std::array::from_fn(|_| {
                [
                    AtomicU64::new(bits(f64::NAN)),
                    AtomicU64::new(bits(0.0)),
                    AtomicU64::new(bits(0.0)),
                ]
            }),
            pair_gen: AtomicU64::new(0),
        }
    }

    /// The loop's last locked rate state at `device_rate`'s family.
    pub fn remembered_rate(&self, device_rate: u32) -> Option<f64> {
        self.memory_at(rate_family(device_rate)).eps
    }

    fn remember_rate(&self, device_rate: u32, eps: f64) {
        self.memory[rate_family(device_rate)][0].store(bits(eps), Ordering::Relaxed);
    }

    fn remember_bias(&self, device_rate: u32, master_s: f64, follower_s: f64) {
        let m = &self.memory[rate_family(device_rate)];
        m[1].store(bits(master_s), Ordering::Relaxed);
        m[2].store(bits(follower_s), Ordering::Relaxed);
    }

    /// What is remembered at family `family` (0: 48 kHz, 1: 44.1 kHz).
    pub fn memory_at(&self, family: usize) -> ClockMemory {
        let m = &self.memory[family.min(1)];
        let r = |i: usize| unbits(m[i].load(Ordering::Relaxed));
        let finite = |x: f64| if x.is_finite() && x >= 0.0 { x } else { 0.0 };
        ClockMemory {
            eps: Some(r(0)).filter(|e| e.is_finite()),
            master_bias_s: finite(r(1)),
            follower_bias_s: finite(r(2)),
        }
    }

    /// Install the memory of a pair (both families): another master, or
    /// what the disk had. The running loop starts again from it.
    pub fn install_memory(&self, mem: [ClockMemory; 2]) {
        for (m, x) in self.memory.iter().zip(mem) {
            m[0].store(bits(x.eps.unwrap_or(f64::NAN)), Ordering::Relaxed);
            m[1].store(bits(x.master_bias_s), Ordering::Relaxed);
            m[2].store(bits(x.follower_bias_s), Ordering::Relaxed);
        }
        self.pair_gen.fetch_add(1, Ordering::Release);
    }

    /// Its device period from the next open on, ms (`None`: the default,
    /// [`ipc::FOLLOWER_DEFAULT_PERIOD_MS`]). Returns whether it changed.
    pub fn set_period_ms(&self, ms: Option<f64>) -> bool {
        let new = bits(ms.filter(|m| m.is_finite() && *m > 0.0).unwrap_or(f64::NAN));
        self.period_ms.swap(new, Ordering::Relaxed) != new
    }

    /// Its device period, ms (the default when none is set).
    pub fn period_ms(&self) -> f64 {
        let ms = unbits(self.period_ms.load(Ordering::Relaxed));
        if ms.is_finite() {
            ms
        } else {
            ipc::FOLLOWER_DEFAULT_PERIOD_MS
        }
    }

    /// Pad its path to the master's chain latency (on with the host).
    pub fn set_compensate(&self, on: bool) {
        self.compensate.store(on, Ordering::Relaxed);
    }

    pub fn compensate(&self) -> bool {
        self.compensate.load(Ordering::Relaxed)
    }

    /// Ask the follower to reopen its device (taken once).
    pub fn request_reopen(&self) {
        self.reopen.store(true, Ordering::Release);
    }

    fn take_reopen(&self) -> bool {
        self.reopen.swap(false, Ordering::AcqRel)
    }

    /// Where a measurement job for this follower is posted.
    pub fn measure_slot(&self) -> &Arc<MeasureSlot> {
        &self.measure
    }

    /// The follower's delay behind the master, ms (±[`MAX_TRIM_MS`]).
    pub fn set_trim_ms(&self, ms: f64) {
        let ms = if ms.is_finite() {
            ms.clamp(-MAX_TRIM_MS, MAX_TRIM_MS)
        } else {
            0.0
        };
        self.trim_ms.store(bits(ms), Ordering::Relaxed);
    }

    pub fn trim_ms(&self) -> f64 {
        unbits(self.trim_ms.load(Ordering::Relaxed))
    }

    /// A new trim on a running follower. While it plays late (its
    /// lateness above 0: the alignment is at its cap), it cannot play
    /// earlier than it does, so a trim that moves it earlier goes into the
    /// lateness instead (the follower stays where it is, and the status
    /// says how late it is); a later one takes the lateness back first
    /// (down to 0: aligned again), and only the rest moves the follower.
    pub fn retrim(&self, ms: f64) {
        let before = self.trim_ms();
        self.set_trim_ms(ms);
        let earlier = before - self.trim_ms();
        let lag = self.lag_ms();
        if lag > 0.0 && earlier > 0.0 {
            self.set_lag_ms(lag + earlier);
        } else if lag > 0.0 && earlier < 0.0 {
            self.set_lag_ms((lag + earlier).max(0.0));
        }
    }

    /// Its lateness beyond the trim, ms (`0..=`[`MAX_LAG_MS`]).
    pub fn set_lag_ms(&self, ms: f64) {
        let ms = if ms.is_finite() {
            ms.clamp(0.0, MAX_LAG_MS)
        } else {
            0.0
        };
        self.lag_ms.store(bits(ms), Ordering::Relaxed);
    }

    pub fn lag_ms(&self) -> f64 {
        unbits(self.lag_ms.load(Ordering::Relaxed))
    }

    /// Its level, in one store: its own digital volume `volume_db` (dB, at
    /// most 0) and, to follow the master's level
    /// ([`TeeShared::master_level`]), its offset to it (its `volume_db`
    /// minus the master's level that was set at): the master's changes move
    /// it by as much, and its balance (on the first two inputs) and mute
    /// reach it. `None`: it plays at `volume_db` alone.
    pub fn set_level(&self, volume_db: f64, follow_offset_db: Option<f64>) {
        self.level.store(
            LevelWord::new(volume_db, follow_offset_db).0,
            Ordering::Relaxed,
        );
    }

    /// Its own volume and its offset to the master's, read together.
    pub fn level(&self) -> (f64, Option<f64>) {
        LevelWord(self.level.load(Ordering::Relaxed)).get()
    }

    /// Its own volume alone (its following unchanged).
    pub fn set_volume_db(&self, db: f64) {
        let _ = self
            .level
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
                Some(LevelWord::new(db, LevelWord(w).get().1).0)
            });
    }

    pub fn volume_db(&self) -> f64 {
        self.level().0
    }

    /// Its offset to the master's level, while it follows.
    pub fn follow_offset_db(&self) -> Option<f64> {
        self.level().1
    }

    /// Enabled: it may be heard (disabled, it keeps its lock, muted).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// The source queue.

/// The follower's source frames by index: what it read from its tee ring,
/// contiguous from `base`, with `history` frames kept behind the read
/// point. Frames it does not have (a gap in the ring, ahead of what the
/// master has written, or older than the history) read as zeros.
///
/// The frames sit at `buf[off..off + len]` of a buffer twice the window:
/// dropping old frames moves `off`, and they move back to the front only
/// once the window no longer fits after `off` (a copy per window's worth
/// of frames, not per call).
struct SourceQueue {
    buf: Vec<Vec<f64>>,
    off: usize,
    /// Index of `buf[..][off]`, and frames held.
    base: u64,
    len: usize,
    /// Frames it holds at most, and kept behind the read point.
    window: usize,
    history: usize,
    /// The tee ring delivered something since the last reset.
    seen: bool,
    /// The newest index seen + 1.
    end_seen: u64,
}

impl SourceQueue {
    fn new(channels: usize, history: usize) -> Self {
        let window = history + QUEUE_AHEAD;
        Self {
            buf: vec![vec![0.0; 2 * window]; channels],
            off: 0,
            base: 0,
            len: 0,
            window,
            history,
            seen: false,
            end_seen: 0,
        }
    }

    fn end(&self) -> u64 {
        self.base + self.len as u64
    }

    /// Drop frames older than `keep_from`.
    fn drop_before(&mut self, keep_from: u64) {
        if keep_from <= self.base {
            return;
        }
        let k = ((keep_from - self.base) as usize).min(self.len);
        if k == 0 {
            return;
        }
        self.off += k;
        self.base += k as u64;
        self.len -= k;
        if self.len == 0 {
            self.off = 0;
        }
    }

    /// Room for the whole window after `off`.
    fn compact(&mut self) {
        if self.off + self.window > self.buf.first().map_or(0, Vec::len) {
            let (off, len) = (self.off, self.len);
            for c in &mut self.buf {
                c.copy_within(off..off + len, 0);
            }
            self.off = 0;
        }
    }

    /// Read what the ring has, while there is room (frames older than
    /// `keep_from` make room first).
    fn fill(&mut self, reader: &mut TeeReader, keep_from: u64) {
        self.drop_before(keep_from);
        self.compact();
        let ch = self.buf.len().min(MAX_METER_CHANNELS);
        loop {
            let room = self.window - self.len;
            if room == 0 {
                break;
            }
            let at = self.off + self.len;
            let top = self.off + self.window;
            let chunk = {
                let mut outs: [&mut [f64]; MAX_METER_CHANNELS] =
                    std::array::from_fn(|_| &mut [][..]);
                for (o, b) in outs.iter_mut().zip(self.buf.iter_mut()) {
                    *o = &mut b[at..top];
                }
                reader.read(&mut outs[..ch])
            };
            let Some(chunk) = chunk else { break };
            self.seen = true;
            self.end_seen = chunk.index + chunk.frames as u64;
            if self.len == 0 || chunk.index == self.end() {
                if self.len == 0 {
                    self.base = chunk.index;
                }
                self.len += chunk.frames;
                continue;
            }
            // A gap: zeros between, when they fit; otherwise start over at
            // the chunk (everything before it reads as zeros).
            let gap = chunk.index.saturating_sub(self.end()) as usize;
            if chunk.index > self.end() && self.len + gap + chunk.frames <= self.window {
                for c in &mut self.buf {
                    c.copy_within(at..at + chunk.frames, at + gap);
                    c[at..at + gap].fill(0.0);
                }
                self.len += gap + chunk.frames;
            } else {
                for c in &mut self.buf {
                    c.copy_within(at..at + chunk.frames, 0);
                }
                self.off = 0;
                self.base = chunk.index;
                self.len = chunk.frames;
            }
        }
    }

    /// The source index up to which frames are known (here or still in the
    /// ring, if it has no gap).
    fn known_end(&self, reader: &TeeReader) -> u64 {
        self.end() + reader.available() as u64
    }

    /// Frame `i` of channel `c` (zero when not held).
    #[inline]
    fn at(&self, c: usize, i: u64) -> f64 {
        if i >= self.base && i < self.end() {
            self.buf[c][self.off + (i - self.base) as usize]
        } else {
            0.0
        }
    }

    /// Frames of `[from, from + n)` held.
    fn held(&self, from: u64, n: usize) -> usize {
        let a = from.max(self.base);
        let b = (from + n as u64).min(self.end());
        b.saturating_sub(a) as usize
    }
}

// ---------------------------------------------------------------------------
// The source fade.

/// A raised-cosine fade of the stage's input: `pos` frames into it
/// (0: silent, `len`: open).
#[derive(Clone, Copy, Debug)]
struct SourceFade {
    pos: usize,
    len: usize,
}

impl SourceFade {
    fn new(len: usize) -> Self {
        Self {
            pos: 0,
            len: len.max(1),
        }
    }

    /// The gain of the next frame, heading open or shut.
    #[inline]
    fn step(&mut self, open: bool) -> f64 {
        if open {
            self.pos = (self.pos + 1).min(self.len);
        } else {
            self.pos = self.pos.saturating_sub(1);
        }
        0.5 - 0.5 * (PI * self.pos as f64 / self.len as f64).cos()
    }

    fn silent(&self) -> bool {
        self.pos == 0
    }

    fn open(&self) -> bool {
        self.pos == self.len
    }
}

// ---------------------------------------------------------------------------
// The compensation pad.

/// What a follower's pad does about a need of `want` device frames (the
/// master's chain and compensation less its own chain; negative: its own
/// is longer).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PadPlan {
    /// The pad's length: the need, within `0..=max`.
    pub target: usize,
    /// The master's compensation still grows to cover the follower's
    /// longer chain: shut until it has.
    pub short: bool,
    /// What neither covers, device frames: later than the master
    /// (positive: its chain is longer than the master's compensation can
    /// cover) or earlier (negative: the pad is at its most).
    pub late_frames: f64,
}

/// [`PadPlan`] for a need of `want` frames, a pad of at most `max` frames,
/// and whether the master's compensation can still grow.
pub fn pad_plan(want: f64, max: usize, comp_can_grow: bool) -> PadPlan {
    if want < -0.5 {
        PadPlan {
            target: 0,
            short: comp_can_grow,
            late_frames: if comp_can_grow { 0.0 } else { -want },
        }
    } else if want > max as f64 + 0.5 {
        PadPlan {
            target: max,
            short: false,
            late_frames: -(want - max as f64),
        }
    } else {
        PadPlan {
            target: want.round().max(0.0) as usize,
            short: false,
            late_frames: 0.0,
        }
    }
}

/// A delay of whole device frames after the follower's chain (interleaved
/// `i32`, exact), which pads its path to the master's chain latency.
///
/// The ring always keeps its whole capacity of history, so a new length
/// reads frames already there: a change fades the output out
/// (`PAD_FADE_MS`), switches at silence, and fades back in, as a chain
/// swap of another latency does. Muted, it fades out and stays shut. Open
/// at length 0 it passes the samples unchanged.
pub struct OutPad {
    ch: usize,
    cap: usize,
    buf: Vec<i32>,
    w: usize,
    len: usize,
    target: usize,
    muted: bool,
    /// The fade's position (`fade_len`: open) and length.
    g: usize,
    fade_len: usize,
}

impl OutPad {
    /// For `ch` channels, up to `max` frames of delay, fading over
    /// `fade_len` frames (allocates).
    pub fn new(ch: usize, max: usize, fade_len: usize) -> Self {
        let cap = max + 1;
        Self {
            ch: ch.max(1),
            cap,
            buf: vec![0; cap * ch.max(1)],
            w: 0,
            len: 0,
            target: 0,
            muted: false,
            g: fade_len.max(1),
            fade_len: fade_len.max(1),
        }
    }

    /// Its delay now, frames.
    pub fn delay(&self) -> usize {
        self.len
    }

    pub fn max(&self) -> usize {
        self.cap - 1
    }

    /// The length it moves to (through a fade), and whether it stays shut.
    pub fn set(&mut self, target: usize, muted: bool) {
        self.target = target.min(self.max());
        self.muted = muted;
    }

    /// Set the length at once (before it plays: no fade).
    pub fn jump(&mut self, len: usize) {
        self.len = len.min(self.max());
        self.target = self.len;
    }

    /// Fully open (no fade running, not muted).
    pub fn open(&self) -> bool {
        self.g == self.fade_len && !self.muted && self.len == self.target
    }

    /// Delay `out` (interleaved frames) in place. Returns whether the
    /// length changed in it.
    pub fn process(&mut self, out: &mut [i32]) -> bool {
        let ch = self.ch;
        let mut switched = false;
        for frame in out.chunks_exact_mut(ch) {
            let at = self.w * ch;
            self.buf[at..at + ch].copy_from_slice(frame);
            let closing = self.muted || self.len != self.target;
            if closing {
                self.g = self.g.saturating_sub(1);
                if self.g == 0 && self.len != self.target {
                    self.len = self.target;
                    switched = true;
                }
            } else if self.g < self.fade_len {
                self.g += 1;
            }
            let r = (self.w + self.cap - self.len) % self.cap * ch;
            if self.g == self.fade_len {
                if self.len != 0 {
                    frame.copy_from_slice(&self.buf[r..r + ch]);
                }
            } else {
                let w = 0.5 - 0.5 * (PI * self.g as f64 / self.fade_len as f64).cos();
                for (c, x) in frame.iter_mut().enumerate() {
                    *x = (f64::from(self.buf[r + c]) * w).round() as i32;
                }
            }
            self.w = (self.w + 1) % self.cap;
        }
        switched
    }
}

// ---------------------------------------------------------------------------
// The follower's audio path.

/// The queues a follower's chains come and go by: new chains from the
/// control side, the old ones back to it (never dropped on the audio path).
pub struct ChainSwaps {
    pub incoming: rtrb::Consumer<ActiveChain>,
    pub retired: rtrb::Producer<ActiveChain>,
}

/// The control side's ends of [`ChainSwaps`].
pub struct ChainSwapHost {
    pub incoming: rtrb::Producer<ActiveChain>,
    pub retired: rtrb::Consumer<ActiveChain>,
    /// The newest chain built that the queue had no room for: it goes in
    /// at the next [`ChainSwapHost::reap`] (a newer one replaces it, the
    /// older dropped here), so the last edit always lands.
    pending: Option<ActiveChain>,
}

impl ChainSwapHost {
    /// Drop the chains handed back; queue a pending one.
    pub fn reap(&mut self) {
        while let Ok(c) = self.retired.pop() {
            drop(c);
        }
        if let Some(c) = self.pending.take()
            && let Err(rtrb::PushError::Full(c)) = self.incoming.push(c)
        {
            self.pending = Some(c);
        }
    }

    /// A chain built for the follower: queued, or pending (the newest
    /// only) while the queue is full.
    pub fn offer(&mut self, chain: ActiveChain) {
        drop(self.pending.replace(chain));
        self.reap();
    }
}

/// How many new chains a follower's queue holds (the control side keeps
/// the newest beyond it pending).
const CHAIN_QUEUE: usize = 2;

/// A pair of chain queues.
pub fn chain_swaps() -> (ChainSwapHost, ChainSwaps) {
    let (in_tx, in_rx) = rtrb::RingBuffer::new(CHAIN_QUEUE);
    let (out_tx, out_rx) = rtrb::RingBuffer::new(4);
    (
        ChainSwapHost {
            incoming: in_tx,
            retired: out_rx,
            pending: None,
        },
        ChainSwaps {
            incoming: in_rx,
            retired: out_tx,
        },
    )
}

/// Where a chain swap is (see [`SWAP_WARM_MARGIN`]).
enum Swap {
    Idle,
    /// The new chain runs beside the playing one, its output dropped.
    Warming {
        next: ActiveChain,
        warmed: usize,
        target: usize,
    },
    /// The new chain plays; the old one fades out under it.
    Crossfade {
        old: ActiveChain,
        pos: usize,
        len: usize,
    },
    /// The old chain fades out (the new one keeps warming), then the switch.
    FadeOut {
        next: ActiveChain,
        pos: usize,
        len: usize,
    },
    FadeIn {
        pos: usize,
        len: usize,
    },
}

/// Ready `next` to warm up beside `playing` from the next call on, whose
/// first input frame is `next_pos` (graph frames into the chain): onto the
/// grid first (an align after the handover would clear what it took),
/// its limiter muted, then the handover ([`ActiveChain::inherit_filter_state`],
/// at most `budget` bytes). Returns the warm-up target, device frames: as
/// the master's, a convolution the edit keeps plays on from `playing`'s
/// state, one it does not (or one too large to copy in a period) warms up
/// for its ring too (at most `ring_cap`).
pub(crate) fn ready_incoming(
    next: &mut ActiveChain,
    playing: &mut ActiveChain,
    next_pos: u64,
    budget: usize,
    ring_cap: usize,
    out_rate: u32,
) -> usize {
    next.align_to(next_pos);
    next.set_limiter_audible(false);
    let lat = next.latency_out_frames();
    let ring = if next.inherit_filter_state(playing, budget) {
        0
    } else {
        next.conv_ring_frames().min(ring_cap)
    };
    // With a convolution, the fresh EQ sections settle too (as the
    // master's), capped.
    let ring = ring.max(next.settle_out_frames(out_rate));
    if lat == 0 && ring == 0 {
        0
    } else {
        lat + ring + SWAP_WARM_MARGIN
    }
}

/// The raised cosine from 0 to 1 at frame `pos` of `len`.
fn rise(pos: usize, len: usize) -> f64 {
    let t = (pos as f64 + 0.5) / len.max(1) as f64;
    0.5 - 0.5 * (PI * t.min(1.0)).cos()
}

/// What a follower runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowerParams {
    /// The master's graph rate (the stage's rate).
    pub graph_rate: u32,
    pub channels: usize,
    /// Its device: rate, period and buffer (device frames).
    pub device_rate: u32,
    pub period: usize,
    pub buffer: usize,
    /// Device frames per graph frame (the chain's ratio).
    pub ratio: RateRatio,
}

/// Why [`FollowerRt::new`] failed.
pub type BuildError = String;

/// The follower's per-period state (see the module docs). Built off the
/// audio path; [`Self::on_reading`], [`Self::fill`], [`Self::pending`] and
/// [`Self::advance`] never allocate.
pub struct FollowerRt {
    p: FollowerParams,
    reader: TeeReader,
    shared: TeeShared,
    status: Arc<FollowerStatus>,
    controls: Arc<FollowerControls>,
    stage: Box<Varispeed>,
    chain: ActiveChain,
    /// Its volume and mute, ramped in the chain before its limiter (as the
    /// master's), at the device rate.
    gain: StereoGainRamp,
    /// The master's balance on its inputs (the chain's input: a sub's
    /// matrix sums them), ramped at the graph rate.
    in_gain: StereoGainRamp,
    /// The master's level last read ([`TeeShared::master_level`]) and its
    /// generation; the level set on the ramps (volume dB, balance dB,
    /// muted; `None` before the first block: it snaps); its own volume,
    /// linear (a `bypass` measurement's tap: no master level).
    master_gen: u64,
    master_level: VolumeTarget,
    level: Option<(f64, f64, bool)>,
    own_lin: f64,
    src: SourceQueue,
    stage_in: Vec<Vec<f64>>,
    stage_out: Vec<Vec<f64>>,
    /// Device frames rendered and not written yet (interleaved).
    fifo: Vec<i32>,
    fifo_frames: usize,
    fade: SourceFade,
    /// The next source index the stage takes, and the source position
    /// (64.64, the tee's index domain) of the stage's input frame 0.
    want: u64,
    origin: i128,
    started: bool,
    map: PositionMap,
    det: PhaseDetector,
    lp: DriftLoop,
    rate_est: RateEstimator,
    /// Device frames written, and rendered (written + the fifo).
    written: u64,
    rendered: u64,
    /// Graph frames into the chain since its reset (its frame grid).
    chain_in: u64,
    /// Graph frames the stage has output.
    graph_out: u64,
    /// ε over the coming period: from (value, slope) at graph frame
    /// `seg_start` to the loop's target `seg_h` seconds later.
    seg_start: u64,
    seg_from: (f64, f64),
    seg_to: (f64, f64),
    seg_h: f64,
    pending_skip: i64,
    /// Known steps waiting for a clear stretch of the source (no ramp, no
    /// command inside their span: `skip_clear`): the master's, source
    /// frames, and the follower's lateness, ms (taken in frames at the
    /// stage's speed then); since when (graph frames out), and whether any
    /// waits. The source position of the last command applied.
    deferred_frames: f64,
    deferred_ms: f64,
    deferred_since: u64,
    deferred: bool,
    /// Where the master's output jumped at the waiting step (source
    /// frames, the old mapping): the skip waits until the read point is
    /// there, shut from before it, so the step lands at both DACs
    /// together. And an alignment change the master has announced, not
    /// made yet (where it jumps at the earliest): shut from before it.
    deferred_at: Option<i64>,
    announced: Option<i64>,
    /// The known steps in the pending skip, as they were before it (frames,
    /// ms, the remainder, the skip, since when they wait): a skip sent back
    /// to waiting takes its lateness at the speed of the moment it lands,
    /// and keeps its deadline.
    committed: Option<(f64, f64, f64, i64, u64)>,
    last_cmd_at: i128,
    pending_reset: Option<VsCommand>,
    /// Readings of device frames before this hold (written before the last
    /// skip or restart, or while the stage ramped).
    hold_until: u64,
    log_cursor: Option<u64>,
    locked: bool,
    master_valid: bool,
    /// The master's position epoch last seen: a new one is a new stream
    /// (a pause, a reopen), and the loop places the follower anew.
    master_epoch: Option<u64>,
    master_ramping: bool,
    /// Both devices' readings against their own clocks (the reading bias,
    /// [`BiasTracker`]); the master's last reading taken (its time, known
    /// steps, speed) and its deviation (source frames, `NaN`: not taken
    /// out); the pair memory's generation the loop started from.
    bias_m: BiasTracker,
    bias_f: BiasTracker,
    bias_m_seen: (i64, i64, f64),
    bias_m_dev: f64,
    /// The master's last point on its line (position, time, epoch, known
    /// steps), and whether the newest is held back: off that line by more
    /// than [`MASTER_ODD_S`] without a step, a new stream or a ramp (a late
    /// reading at its DAC). It counts once the next agrees with it.
    master_line: Option<(i128, i64, u64, i64)>,
    master_odd: bool,
    /// Which devices' deviations were taken out at the last reading.
    bias_out: (bool, bool),
    pair_gen: u64,
    /// Device frames whose output is silenced from here (after a signalled
    /// xrun: what the chain and the pad held belongs to the old timeline).
    out_mute: usize,
    master_speed: f64,
    loop_rate: f64,
    /// The trim and the lateness (ms) the detector's offset was set for,
    /// and the offset (source frames at the master's speed).
    lateness_ms: f64,
    offset: f64,
    /// The master's known steps taken ([`TeeShared::steps`]), and the part
    /// of the known steps the whole-frame skips have not taken yet (it
    /// carries into the next one, so slider edits add up exactly).
    step_seen: i64,
    step_rest: f64,
    /// The least source slack over the window, frames, and the window's
    /// age, seconds; the slack published, ms.
    slack_min: f64,
    slack_age: f64,
    /// The stream's first slack window is not out yet (it is shorter, so
    /// the master's alignment is set within a fraction of a second).
    slack_first: bool,
    slack_last: f64,
    /// The best lower bound of the slack at the clocks' worst phase since
    /// the last change the follower cannot account for (ms, `NaN`: none),
    /// shifted by its own known steps; and the last window's phase
    /// allowance, frames.
    slack_best: f64,
    /// The least of the windows' lower bounds since the same changes, ms
    /// (`NaN`: none): the worst the follower had, for the alignment's
    /// shrink.
    slack_low: f64,
    spacing_prev: f64,
    /// The window saw the master gone (its position invalid: a rate
    /// change, a reopen, Direct): it publishes nothing.
    slack_void: bool,
    /// The ring's known end at the last call, and the largest step it made
    /// between two calls over the window (the master's write size: the
    /// slack is counted at the worst phase of the two clocks).
    known_last: u64,
    known_jump: u64,
    /// Graph frames the follower stays shut after it ran short of source
    /// (no flutter of the fade on a marginal look-ahead).
    starved: usize,
    /// How far the stage's call ends ran ahead of the master's stage over
    /// the window (source frames, the most) and the last window's, ms; the
    /// master's playback state last seen.
    lead_max: f64,
    lead_last: f64,
    state_at: i128,
    /// Chain swaps: the queues, where one is, its scratch output, and an
    /// old chain the full queue could not take yet.
    swaps: Option<ChainSwaps>,
    swap: Swap,
    swap_buf: Vec<i32>,
    retire_stash: Option<ActiveChain>,
    /// The newest chain that arrived while a swap was in flight, taken off
    /// the queue (latest wins, an older one retired) so the queue never
    /// fills and the last edit always lands; it starts once the swap ends.
    held: Option<ActiveChain>,
    /// The master's chain latency changes seen ([`TeeShared::chain_seq`]).
    chain_seq: u64,
    /// The compensation pad after the chain, the length it should have
    /// (device frames) and since when (device frames rendered).
    pad: OutPad,
    pad_want: usize,
    pad_since: u64,
    /// Measurement playback: the file at the stage input, and (`bypass`)
    /// the stage output around the chain.
    measure: FollowerMeasure,
    /// Chain swaps done (tests and the status).
    pub swaps_done: u64,
}

/// What the follower's period is for a device period of `period` frames.
fn period_seconds(p: &FollowerParams) -> f64 {
    p.period as f64 / f64::from(p.device_rate)
}

impl FollowerRt {
    /// A follower at `p` reading `reader`, with its `chain` (built for `p`:
    /// the graph rate, the ratio and the device's format).
    pub fn new(
        p: FollowerParams,
        reader: TeeReader,
        shared: TeeShared,
        chain: ActiveChain,
        status: Arc<FollowerStatus>,
        controls: Arc<FollowerControls>,
    ) -> Result<Self, BuildError> {
        if p.channels == 0 || p.channels > MAX_METER_CHANNELS || p.channels != reader.channels() {
            return Err(format!("{} channels", p.channels));
        }
        if p.graph_rate == 0 || p.graph_rate > MAX_GRAPH_RATE || p.period == 0 {
            return Err(format!("graph rate {} period {}", p.graph_rate, p.period));
        }
        if chain.input_rate().is_some_and(|r| r != p.graph_rate) || chain.ratio() != p.ratio {
            return Err("the chain is not built for this follower".into());
        }
        let g = f64::from(p.graph_rate);
        let mut stage = Box::new(
            Varispeed::new(VarispeedSpec {
                sample_rate: p.graph_rate,
                channels: p.channels,
                max_frames: STAGE_FRAMES,
                speed: 1.0,
            })
            .map_err(|e| format!("stage: {e}"))?,
        );
        stage.set_follower(true);
        let period_s = period_seconds(&p);
        // The loop's own delay: the device buffer and a period.
        let delay_s = (p.buffer + p.period) as f64 / f64::from(p.device_rate);
        let mut lp =
            DriftLoop::new(LoopConfig::default(), g, delay_s).map_err(|e| e.to_string())?;
        let pair_gen = controls.pair_gen.load(Ordering::Acquire);
        let mem = controls.memory_at(rate_family(p.device_rate));
        seed_loop(&mut lp, mem);
        let max_out = p.ratio.frames_out(0, STAGE_FRAMES) + p.ratio.up as usize + 1;
        let fade_len = ((FADE_MS * 1e-3 * g).round() as usize).clamp(1, MAX_FADE);
        let ch = p.channels;
        let measure = FollowerMeasure::new(Arc::clone(controls.measure_slot()), ch, STAGE_FRAMES);
        let mut rt = Self {
            reader,
            shared,
            status,
            controls,
            stage,
            chain,
            gain: StereoGainRamp::unity(p.device_rate),
            in_gain: StereoGainRamp::unity(p.graph_rate),
            master_gen: 0,
            master_level: VolumeTarget::default(),
            level: None,
            own_lin: 1.0,
            src: SourceQueue::new(ch, history_frames(p.graph_rate)),
            stage_in: vec![vec![0.0; 2 * STAGE_FRAMES + 2]; ch],
            stage_out: vec![vec![0.0; STAGE_FRAMES]; ch],
            fifo: vec![0; (p.buffer.max(p.period) + max_out) * ch],
            fifo_frames: 0,
            fade: SourceFade::new(fade_len),
            want: 0,
            origin: 0,
            started: false,
            map: PositionMap::new(),
            det: PhaseDetector::new(g),
            lp,
            rate_est: RateEstimator::new(),
            written: 0,
            rendered: 0,
            chain_in: 0,
            graph_out: 0,
            seg_start: 0,
            seg_from: (0.0, 0.0),
            seg_to: (0.0, 0.0),
            seg_h: period_s,
            pending_skip: 0,
            deferred_frames: 0.0,
            deferred_ms: 0.0,
            deferred_since: 0,
            deferred: false,
            deferred_at: None,
            announced: None,
            committed: None,
            last_cmd_at: i128::MIN,
            pending_reset: None,
            hold_until: 0,
            log_cursor: None,
            locked: false,
            master_valid: false,
            master_epoch: None,
            master_ramping: false,
            bias_m: BiasTracker::new(),
            bias_f: BiasTracker::new(),
            bias_m_seen: (i64::MIN, i64::MIN, f64::NAN),
            master_line: None,
            master_odd: false,
            bias_m_dev: f64::NAN,
            bias_out: (false, false),
            pair_gen: 0,
            out_mute: 0,
            master_speed: 1.0,
            loop_rate: g,
            lateness_ms: f64::NAN,
            offset: 0.0,
            step_seen: 0,
            step_rest: 0.0,
            slack_min: f64::INFINITY,
            slack_age: 0.0,
            slack_first: true,
            slack_last: f64::NAN,
            slack_best: f64::NAN,
            slack_low: f64::NAN,
            spacing_prev: f64::NAN,
            slack_void: false,
            known_last: 0,
            known_jump: 0,
            starved: 0,
            lead_max: f64::NEG_INFINITY,
            lead_last: f64::NAN,
            state_at: i128::MIN,
            swaps: None,
            swap: Swap::Idle,
            swap_buf: vec![0; max_out * ch],
            retire_stash: None,
            held: None,
            chain_seq: 0,
            pad: OutPad::new(
                ch,
                max_comp_frames(p.device_rate),
                ((PAD_FADE_MS * 1e-3 * f64::from(p.device_rate)).round() as usize).max(1),
            ),
            pad_want: 0,
            pad_since: 0,
            measure,
            swaps_done: 0,
            p,
        };
        rt.chain_seq = rt.shared.chain_seq.load(Ordering::Relaxed);
        rt.pair_gen = pair_gen;
        rt.bias_m.remember_rms_s(mem.master_bias_s);
        rt.bias_f.remember_rms_s(mem.follower_bias_s);
        rt.step_seen = rt.shared.steps.load(Ordering::Relaxed) as i64;
        // What the ring held before (the follower was away): the read
        // point starts at what comes next.
        loop {
            let mut outs: [&mut [f64]; MAX_METER_CHANNELS] = std::array::from_fn(|_| &mut [][..]);
            for (o, b) in outs.iter_mut().zip(rt.stage_in.iter_mut()) {
                *o = &mut b[..];
            }
            if rt.reader.read(&mut outs[..ch]).is_none() {
                break;
            }
        }
        // A new stream (a rate change, a reopen) plays from aligned: what
        // it needs late at this rate and period its slack shows again (muted
        // until it locks). The last stream's slack is not this one's.
        rt.controls.set_lag_ms(0.0);
        rt.status.slack_ms.store(bits(f64::NAN), Ordering::Relaxed);
        rt.status
            .slack_low_ms
            .store(bits(f64::NAN), Ordering::Relaxed);
        rt.status
            .device_rate
            .store(u64::from(p.device_rate), Ordering::Relaxed);
        rt.status.period.store(p.period as u64, Ordering::Relaxed);
        Ok(rt)
    }

    pub fn params(&self) -> FollowerParams {
        self.p
    }

    /// Give back the tee ring (the follower is rebuilt or retired).
    pub fn into_reader(self) -> TeeReader {
        self.into_parts().0
    }

    /// Give back the tee ring and the chain queues (the chains in a swap
    /// drop here, off the audio path).
    pub fn into_parts(self) -> (TeeReader, Option<ChainSwaps>) {
        (self.reader, self.swaps)
    }

    /// The queues new chains arrive by.
    pub fn set_swaps(&mut self, swaps: Option<ChainSwaps>) {
        self.swaps = swaps;
    }

    /// Device frames written so far (a reading's `written`).
    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// The source position (64.64, the tee's index domain) device frame
    /// `frame` carries, from the map (tests: the truth).
    pub fn source_at(&self, frame: u64) -> Option<i128> {
        self.map.source_at(frame)
    }

    /// The device's buffer was dropped and refilled from silence (an xrun):
    /// what was written before is gone, the loop sees the jump.
    pub fn xrun(&mut self) {
        self.status.xruns.fetch_add(1, Ordering::Relaxed);
        self.measure.interrupted();
        self.slack_best = f64::NAN;
        // Shut at once, before the prime renders: the device dropped what
        // it held, so what plays next is out of place until the loop has
        // placed it again (what the fifo, the chain and the pad still hold
        // is silenced too: the chain's whole tail, a convolution's ring
        // included, so nothing from before the xrun comes out of it).
        self.lp.realign();
        self.locked = false;
        self.fade.pos = 0;
        let ch = self.p.channels;
        self.fifo[..self.fifo_frames * ch].fill(0);
        self.out_mute = self.chain.tail_out_frames() + self.pad.delay() + self.p.period;
        self.hold_until = self.hold_until.max(self.rendered);
        self.bias_f.rebase();
        self.publish_audible(false);
        self.slack_low = f64::NAN;
    }

    fn publish_audible(&self, on: bool) {
        self.status.audible.store(on, Ordering::Relaxed);
    }

    /// The stage's next output's source position (the log's domain).
    fn here(&self) -> i128 {
        self.origin + (self.stage.core().accumulator().position() >> 1) as i128
    }

    /// The chain's delay in graph frames, its compensation pad included.
    fn chain_graph(&self) -> f64 {
        self.out_graph(self.chain_out_frames())
    }

    /// The chain's delay and the pad's, device frames.
    fn chain_out_frames(&self) -> usize {
        self.chain.latency_out_frames() + self.pad.delay()
    }

    /// Device frames in graph frames.
    fn out_graph(&self, frames: usize) -> f64 {
        let r = self.p.ratio;
        frames as f64 * f64::from(r.down) / f64::from(r.up.max(1))
    }

    /// The pad's length the master's chain latency and compensation ask for
    /// (device frames), and whether the master's compensation still falls
    /// short of this follower's chain (it would play late). `None` without
    /// compensation or before the master has mapped a block.
    fn pad_target(&self) -> Option<PadPlan> {
        if !self.controls.compensate() {
            return None;
        }
        let master = self.shared.master_chain_graph.get();
        if !master.is_finite() {
            return None;
        }
        let comp = self.shared.comp_frames.load(Ordering::Relaxed) as f64;
        let own = self.out_graph(self.chain.latency_out_frames());
        let r = self.p.ratio;
        let want = (master + comp - own) * f64::from(r.up) / f64::from(r.down.max(1));
        let can_grow = comp < max_comp_frames(self.p.graph_rate) as f64
            && own - master <= MAX_COMP_MS * 1e-3 * f64::from(self.p.graph_rate);
        Some(pad_plan(want, self.pad.max(), can_grow))
    }

    /// Once a period: the pad follows its target once that has held still
    /// for [`PAD_SETTLE_S`] with no chain of its own on the way (a chain
    /// edit swaps both sides a little apart); until then, and while the
    /// master's compensation is short, the follower is shut (it would play
    /// out of place). Before the loop has placed it, at once.
    fn update_pad(&mut self) {
        let Some(plan) = self.pad_target() else {
            self.pad.set(0, false);
            self.status.comp_late_ms.store(bits(0.0), Ordering::Relaxed);
            return;
        };
        let (want, short) = (plan.target, plan.short);
        self.status.comp_late_ms.store(
            bits(plan.late_frames / f64::from(self.p.device_rate) * 1e3),
            Ordering::Relaxed,
        );
        if want != self.pad_want {
            self.pad_want = want;
            self.pad_since = self.rendered;
        }
        if !self.lp.aligned() {
            if want != self.pad.delay() {
                self.pad.jump(want);
            }
            self.pad.set(want, false);
            return;
        }
        let in_flight = !matches!(self.swap, Swap::Idle)
            || self.swaps.as_ref().is_some_and(|s| s.incoming.slots() > 0);
        let settled = self.rendered.saturating_sub(self.pad_since) as f64
            >= PAD_SETTLE_S * f64::from(self.p.device_rate);
        // (A frame of rounding is the loop's.)
        let off = want.abs_diff(self.pad.delay()) > 1;
        let target = if settled && !in_flight && off {
            want
        } else {
            self.pad.delay()
        };
        self.pad.set(target, short || off);
    }

    /// The source position at the DAC of the stage's next output (the
    /// master's formula, `TeeRt::stage_span`).
    fn out_position(&self) -> i128 {
        let s = self.stage.speed();
        let delay = (self.stage.latency_frames() - 0.5) * s + self.chain_graph() * s;
        self.here() - fixed(delay)
    }

    /// A due command that restarts the stage (`Reset`, `Off`): applied at
    /// once when it changes nothing audible, otherwise once muted.
    fn request_reset(&mut self, cmd: VsCommand) {
        let (speed, limit, ramp_ms) = match cmd {
            VsCommand::Reset {
                speed,
                limit,
                ramp_ms,
            } => (speed, limit, ramp_ms),
            VsCommand::Off => (1.0, MAX_SPEED, 0.0),
            _ => return,
        };
        let st = &mut self.stage;
        let target = speed.min(limit);
        if !st.ramping() && st.envelope_step().to_f64() == target {
            // Already there: the cap and the request, no restart.
            st.set_speed_limit(limit);
            let _ = st.set_speed(speed, ramp_ms);
            return;
        }
        self.pending_reset = Some(cmd);
    }

    /// Apply a pending restart (muted): the stage restarts at the state,
    /// taking again the frames its filters held so its position carries on.
    fn apply_reset(&mut self, cmd: VsCommand) {
        let (speed, limit, ramp_ms) = match cmd {
            VsCommand::Reset {
                speed,
                limit,
                ramp_ms,
            } => (speed, limit, ramp_ms),
            _ => (1.0, MAX_SPEED, 0.0),
        };
        let held = ((i128::from(self.want) * FRAME - self.here()) / FRAME)
            .clamp(0, self.src.history as i128);
        let st = &mut self.stage;
        st.set_speed_limit(limit);
        let _ = st.set_speed(speed, ramp_ms);
        st.reset();
        self.want -= (held as u64).min(self.want);
        self.origin = i128::from(self.want) * FRAME;
        self.hold_until = self.hold_until.max(self.rendered);
    }

    /// Move the read point by the pending skip (muted), never before the ring's
    /// first frame. When the source it lands on is not there (a repeat further
    /// back than the history, or ahead of what the master has written), the
    /// follower stays shut until it is: the fade opens only on frames held
    /// (`stage_call`), and ahead of the source it starves, its slack says so
    /// and the alignment or its lateness grows to cover it.
    fn apply_skip(&mut self) {
        let mut s = std::mem::take(&mut self.pending_skip);
        if self.stage.ramping() {
            // Inside a ramp (a step past its deadline): the skip moves the
            // ramp the follower should play by `s` frames of source, but
            // the stage's envelope keeps its timing, so over the rest of the
            // ramp the follower gains `s · (v_end − v) / v` frames on where
            // it should be. Taken out here: the readings, held through the
            // ramp, find it in place after it.
            let v = self.stage.envelope_step().to_f64();
            let v_end = self.stage.target();
            if v > 0.0 && v_end.is_finite() {
                s += (s as f64 * (v_end - v) / v).round() as i64;
            }
        }
        // The slack moves by the skip (a skip of the loop's has started it
        // again already).
        self.slack_best -= s as f64 / self.loop_rate * 1e3;
        self.slack_low -= s as f64 / self.loop_rate * 1e3;
        let from = self.want;
        self.want = self.want.saturating_add_signed(s);
        self.origin += (i128::from(self.want) - i128::from(from)) * FRAME;
        // (A hold already running, a chain swap's, keeps its end.)
        self.hold_until = self.hold_until.max(self.rendered);
        let ahead = self.want + self.fade.len as u64;
        if self.want < self.src.base || ahead > self.src.known_end(&self.reader) {
            self.starved = self
                .starved
                .max((STARVED_S * f64::from(self.p.graph_rate)) as usize);
        } else {
            // What it ran short of was at the read point it left (ahead of
            // the source, through a pause).
            self.starved = 0;
        }
    }

    /// A skip of `k` source frames keeps the follower's stage where the
    /// master's is: the stage's envelope runs on output frames, not on the
    /// source, so a skip inside a ramp, or across a command (one due within
    /// the frames it jumps, or one applied within those it repeats), would
    /// play part of the source at the other speed.
    fn skip_clear(&self, k: i64) -> bool {
        if self.stage.ramping() || self.pending_reset.is_some() {
            return false;
        }
        let here = self.here();
        // The skip lands once the fade has shut, at a call's start: the
        // stage runs on that far first (the fade and two periods).
        let r = self.p.ratio;
        let period = self.p.period * r.down as usize / r.up.max(1) as usize;
        let run = (self.fade.len + 2 * period) as f64 * self.stage.peak_until_settled().max(1.0);
        let span = i128::from(k.unsigned_abs()) * FRAME + fixed(run + 2.0);
        if k < 0 {
            return self.last_cmd_at < here - span;
        }
        let Some(cur) = self.log_cursor else {
            return true;
        };
        match self.shared.log.get(cur) {
            LogRead::Entry(e) => {
                !matches!(e.cmd, VsCommand::Speed { .. } | VsCommand::Limit(_))
                    || e.at > here + span
            }
            _ => true,
        }
    }

    /// The log's entries due at the stage's next call; returns the output
    /// frames until the next entry, when it falls ahead.
    fn replay_log(&mut self) -> Option<usize> {
        for _ in 0..LOG_STEPS {
            if self.pending_reset.is_some() {
                return None;
            }
            let Some(cur) = self.log_cursor else {
                // Joining: the master's state, then the log after it.
                let st = self.shared.state.read()?;
                self.request_reset(st.cmd);
                self.log_cursor = Some(st.cursor);
                continue;
            };
            match self.shared.log.get(cur) {
                LogRead::NotYet => return None,
                LogRead::Lost => {
                    // More than the log keeps behind: the state again, if
                    // it is newer.
                    match self.shared.state.read() {
                        Some(st) if st.cursor > cur => {
                            self.request_reset(st.cmd);
                            self.log_cursor = Some(st.cursor);
                        }
                        _ => self.log_cursor = Some(self.shared.log.head()),
                    }
                }
                LogRead::Entry(e) => match e.cmd {
                    VsCommand::Reset { .. } | VsCommand::Off => {
                        self.request_reset(e.cmd);
                        self.log_cursor = Some(cur + 1);
                    }
                    VsCommand::Lost => match self.shared.state.read() {
                        Some(st) if st.cursor > cur => {
                            self.request_reset(st.cmd);
                            self.log_cursor = Some(st.cursor);
                        }
                        _ => self.log_cursor = Some(cur + 1),
                    },
                    VsCommand::Speed { .. } | VsCommand::Limit(_) => {
                        let here = self.here();
                        let step = (self.stage.step().bits() as i128).max(1);
                        let gap = e.at - here;
                        if gap > step / 2 {
                            // Ahead (the master logs them ahead, by the
                            // followers' lead). A restart logged after it
                            // supersedes it (the master dropped it).
                            if let Some(j) = self.restart_after(cur) {
                                self.log_cursor = Some(j);
                                continue;
                            }
                            // The call ends where it starts (within half a
                            // step): while the speed moves, short of it at
                            // the fastest step, then closer.
                            let k = if self.stage.ramping() {
                                let peak = fixed(self.stage.peak_until_settled() * (1.0 + EPS_MAX));
                                gap / peak.max(step)
                            } else {
                                (gap + step / 2) / step
                            };
                            return Some(k.clamp(1, STAGE_FRAMES as i128) as usize);
                        }
                        // (Before the loop's first alignment the read
                        // point is anywhere: not late, only unplaced.)
                        if e.at < here - step && self.lp.aligned() {
                            self.status.late_commands.fetch_add(1, Ordering::Relaxed);
                        }
                        match e.cmd {
                            VsCommand::Speed { speed, ramp_ms } => {
                                let _ = self.stage.set_speed(speed, ramp_ms);
                            }
                            VsCommand::Limit(max) => self.stage.set_speed_limit(max),
                            _ => {}
                        }
                        self.last_cmd_at = here;
                        self.log_cursor = Some(cur + 1);
                    }
                },
            }
        }
        None
    }

    /// The first restart (`Reset`, `Off`, `Lost`) in the log after entry
    /// `cur`, if one is there yet.
    fn restart_after(&self, cur: u64) -> Option<u64> {
        let head = self.shared.log.head();
        (cur + 1..head.min(cur + 1 + super::tee::LOG_LEN as u64)).find(|&j| {
            matches!(
                self.shared.log.get(j),
                LogRead::Entry(LoggedCommand {
                    cmd: VsCommand::Reset { .. } | VsCommand::Off | VsCommand::Lost,
                    ..
                })
            )
        })
    }

    /// Make sure at least `n` device frames are rendered (at most the
    /// buffer).
    pub fn fill(&mut self, n: usize) {
        let ch = self.p.channels;
        let n = n.min(self.fifo.len() / ch);
        let mut guard = 0;
        while self.fifo_frames < n && guard < 64 {
            guard += 1;
            let need = n - self.fifo_frames;
            let m = if self.p.ratio.is_unity() {
                need
            } else {
                self.p.ratio.frames_in_for(self.chain_in, need)
            };
            self.stage_call(m.clamp(1, STAGE_FRAMES));
        }
    }

    /// The first `n` rendered frames (interleaved), for the device.
    pub fn pending(&self, n: usize) -> &[i32] {
        let n = n.min(self.fifo_frames);
        &self.fifo[..n * self.p.channels]
    }

    /// `n` rendered frames were written.
    pub fn advance(&mut self, n: usize) {
        let ch = self.p.channels;
        let n = n.min(self.fifo_frames);
        self.fifo.copy_within(n * ch..self.fifo_frames * ch, 0);
        self.fifo_frames -= n;
        self.written += n as u64;
    }

    /// One stage call of up to `m` graph frames, through the chain into
    /// the fifo.
    fn stage_call(&mut self, m: usize) {
        let ch = self.p.channels;
        let g = f64::from(self.p.graph_rate);
        if !self.started {
            // The first frames the ring gives set the read point; the
            // loop's alignment moves it to the master's.
            self.src.fill(&mut self.reader, 0);
            if self.src.seen {
                // The stage has run on silence until now (the prime, the
                // wait for the master's first frames): the ring's first
                // frame follows the source frames it has taken, so the
                // origin takes them off.
                let taken = self.want;
                self.want = self.src.base;
                self.origin = (i128::from(self.want) - i128::from(taken)) * FRAME;
                self.started = true;
            }
        }
        // A known step, once the source around it is clear; then the skip,
        // once muted, before the log: the next command's boundary is
        // counted from where the read point lands.
        let overdue = |since: u64, now: u64| now.saturating_sub(since) as f64 > DEFER_MAX_S * g;
        if self.deferred {
            let frames = self.deferred_step();
            let k = (frames + self.step_rest).round() as i64;
            // A step of the master's waits for the read point to reach
            // where the master's output jumped (shut before it, below), so
            // the follower's DAC jumps when the master's does: not earlier
            // (a follower that queues less), and a later one is shut over
            // the difference.
            // (Its trim and lateness: it plays the master's position that
            // much later, so it jumps that much earlier in the source.)
            // (Both count what is heard: the master's position as its DAC
            // carries it, the source the read point takes in, which the
            // follower's DAC plays its delay later.)
            let off = self.offset.round() as i64;
            let there = self.deferred_at.is_none_or(|p| self.want as i64 + off >= p);
            if (there && self.skip_clear(k)) || overdue(self.deferred_since, self.graph_out) {
                self.commit_step(frames);
            }
        }
        if self.fade.silent() && self.pending_skip != 0 {
            let late = self.committed.is_some_and(|c| overdue(c.4, self.graph_out));
            if late || self.skip_clear(self.pending_skip) {
                // (Past its deadline it goes now, whatever the stage does:
                // a ramp that outlasts it, a cap that keeps moving.)
                self.committed = None;
                self.apply_skip();
            } else if let Some((frames, ms, rest, k, since)) = self.committed.take() {
                // A command came within its span meanwhile: back to
                // waiting (the fade opens again), the deadline kept.
                self.pending_skip -= k;
                self.deferred_since = if self.deferred {
                    self.deferred_since.min(since)
                } else {
                    since
                };
                self.deferred = true;
                self.deferred_frames += frames;
                self.deferred_ms += ms;
                self.step_rest = rest;
            } else {
                self.apply_skip();
            }
        }
        let mut m = m;
        if let Some(k) = self.replay_log() {
            m = m.min(k);
        }
        // A restart, once muted (the log may just have asked for it).
        if self.fade.silent()
            && let Some(cmd) = self.pending_reset.take()
        {
            self.apply_reset(cmd);
            if let Some(k) = self.replay_log() {
                m = m.min(k);
            }
        }
        self.measure_lead(m);
        // ε at this call's end.
        let t_end = (self.graph_out + m as u64).saturating_sub(self.seg_start) as f64 / g;
        let (e, d) = hermite(self.seg_from, self.seg_to, self.seg_h, t_end);
        let (e, d) = self.within_range(e, d);
        self.stage.set_epsilon_target(e, d);
        let v0 = self.out_position();
        let n_src = match self.stage.plan(m) {
            Ok(n) => n,
            Err(_) => {
                // A guard (ε stays within the range, `within_range`):
                // restart the stage with ε at zero and the loop unlocked,
                // and play silence for the call. The next call plans at
                // the master's own step.
                self.status.refused_plans.fetch_add(1, Ordering::Relaxed);
                self.stage.set_follower(true);
                self.seg_from = (0.0, 0.0);
                self.seg_to = (0.0, 0.0);
                self.lp.unlock();
                // Shut at once: another render can run before the next
                // reading.
                self.locked = false;
                self.stage.reset();
                self.origin = i128::from(self.want) * FRAME;
                for c in &mut self.stage_out {
                    c[..m].fill(0.0);
                }
                self.hold_until = self.hold_until.max(self.rendered);
                self.chain_out(m, None);
                return;
            }
        };
        // The source, faded; is there enough ahead for the fade?
        // (Never the newest history the ring gave: a read point that ran
        // on ahead of the source, through a pause, finds it after its skip
        // back.)
        let keep = self.want.min(self.src.end_seen);
        self.src.fill(
            &mut self.reader,
            keep.saturating_sub(self.src.history as u64),
        );
        let known = self.src.known_end(&self.reader);
        if self.known_last > 0 && known > self.known_last {
            self.known_jump = self.known_jump.max(known - self.known_last);
        }
        self.known_last = known;
        let ahead = self.want + n_src as u64 + self.fade.len as u64;
        // Counted once the loop has aligned the read point (before, it is
        // anywhere).
        if self.lp.aligned()
            && self.master_valid
            && self.pending_skip == 0
            && !self.deferred
            && !self.lp.skip_pending()
        {
            let slack = known as f64 - ahead as f64;
            self.slack_min = self.slack_min.min(slack);
        }
        let enough = known >= ahead;
        let held = self.src.held(self.want, n_src);
        if held < n_src && self.started {
            self.status.underfeeds.fetch_add(1, Ordering::Relaxed);
        }
        if !enough {
            self.starved = (STARVED_S * g) as usize;
        }
        // (A repeat further back than the history reads silence: it stays
        // shut until the source is held again.)
        // (After a signalled xrun, not before the output's mute has run
        // out: the chain has then heard silence for its whole tail, so its
        // output fades in from zero rather than stepping where the mute
        // lifts.)
        // Shut before a step of the master's position, from the frame its
        // fade must end by: an announced alignment change, or a step that
        // waits for the read point (until it is taken).
        let off = self.offset.round() as i64;
        let step_ahead = |p: Option<i64>| p.is_some_and(|p| ahead as i64 + off >= p);
        let stepping =
            step_ahead(self.announced) || (self.deferred && step_ahead(self.deferred_at));
        let open = self.locked
            && self.master_valid
            && self.controls.enabled()
            && held == n_src
            && self.starved == 0
            && self.pending_skip == 0
            && self.pending_reset.is_none()
            && self.out_mute == 0
            && !stepping;
        self.starved = self.starved.saturating_sub(m);
        for i in 0..n_src {
            let gain = self.fade.step(open);
            let idx = self.want + i as u64;
            for (c, chan) in self.stage_in.iter_mut().enumerate().take(ch) {
                chan[i] = gain * self.src.at(c, idx);
            }
        }
        // A measurement: the file on the routed channels by source index,
        // before the stage (so the drift loop's correction applies).
        self.measure.poll();
        let measuring = self
            .measure
            .inject_s0(self.p.graph_rate, self.p.device_rate, self.p.ratio);
        if let Some((_, s0)) = measuring {
            let k0 = self.want as i64 - s0 as i64;
            self.measure
                .inj
                .fill_planar(&mut self.stage_in, n_src, k0, open);
        }
        self.want += n_src as u64;
        {
            let mut ins: [&[f64]; MAX_METER_CHANNELS] = [&[]; MAX_METER_CHANNELS];
            for (x, s) in ins.iter_mut().zip(self.stage_in.iter()) {
                *x = &s[..n_src];
            }
            let mut outs: [&mut [f64]; MAX_METER_CHANNELS] = std::array::from_fn(|_| &mut [][..]);
            for (o, s) in outs.iter_mut().zip(self.stage_out.iter_mut()) {
                *o = &mut s[..m];
            }
            self.stage.process(&ins[..ch], &mut outs[..ch]);
        }
        // `bypass`: the routed channels go around the chain (which hears
        // silence on them), crossfaded at both ends, until the stage has
        // passed the file's last frame (its resampler's tail included).
        if self.measure.begin_tap(self.p.graph_rate) && self.p.ratio.is_unity() {
            for c in 0..ch {
                if self.measure.routed(c) {
                    self.measure.tap[c][..m].copy_from_slice(&self.stage_out[c][..m]);
                    for f in 0..m {
                        self.stage_out[c][f] *= 1.0 - self.measure.tap_weight(f);
                    }
                }
            }
            self.measure.tapped = m;
        }
        self.publish_audible(open && self.fade.open() && self.pad.open());
        self.chain_out(m, Some(v0));
    }

    /// Hand a chain back to the control side (or keep it until the queue
    /// has room).
    fn retire_chain(&mut self, c: ActiveChain) {
        let c = match self.swaps.as_mut() {
            Some(s) => match s.retired.push(c) {
                Ok(()) => return,
                Err(rtrb::PushError::Full(c)) => c,
            },
            None => {
                // Not reached (chains arrive only with the queues).
                debug_assert!(false, "a chain to retire without queues");
                std::mem::forget(c);
                return;
            }
        };
        if self.retire_stash.is_none() {
            self.retire_stash = Some(c);
        } else {
            // Not reached (a chain is taken only with room for two).
            debug_assert!(false, "follower chain stash full");
            std::mem::forget(c);
        }
    }

    /// A new chain that fits this follower, when one has arrived and there
    /// is room to retire the playing one later.
    fn take_chain(&mut self) -> Option<ActiveChain> {
        if let Some(c) = self.retire_stash.take() {
            self.retire_chain(c);
        }
        let s = self.swaps.as_mut()?;
        if self.retire_stash.is_some() || s.retired.slots() < 2 {
            return None;
        }
        let c = s.incoming.pop().ok()?;
        let fits =
            c.input_rate().is_none_or(|r| r == self.p.graph_rate) && c.ratio() == self.p.ratio;
        if !fits {
            self.retire_chain(c);
            return None;
        }
        Some(c)
    }

    /// The step after the playing chain's call (`n_out` frames at `at` in
    /// the fifo, `gain` its volume ramp before it): a swap's other chain on
    /// the same input, and the mix.
    fn run_swap(&mut self, m: usize, at: usize, n_out: usize, gain: StereoGainRamp) {
        let ch = self.p.channels;
        let phase = std::mem::replace(&mut self.swap, Swap::Idle);
        let grid = self.chain_in;
        // The other chain's output for this call, in `swap_buf`; `None`
        // when it does not line up with the playing one's (dropped).
        let other = |c: &mut ActiveChain, buf: &mut [i32], stage_out: &[Vec<f64>]| {
            c.align_to(grid);
            if c.output_frames(m) != n_out || n_out * ch > buf.len() {
                return false;
            }
            let mut g = gain;
            let _ = c.process_in_with_gain(
                DspIn::planar(stage_out, m),
                &mut buf[..n_out * ch],
                ch,
                STAGE_FRAMES,
                &mut g,
                None,
            );
            true
        };
        let out = at..at + n_out * ch;
        let ring_cap = warm_ring_cap(self.p.graph_rate, self.p.ratio);
        // Coalesce: every chain that has come waits in `held`, the newest
        // only (an older one retired), until the swap in flight ends. At
        // most a queue's worth a call (the rest the next).
        for _ in 0..CHAIN_QUEUE {
            let Some(c) = self.take_chain() else { break };
            if let Some(old) = self.held.replace(c) {
                self.retire_chain(old);
            }
        }
        self.swap = match phase {
            // An upstream edit's fade still running: the next chain waits.
            Swap::Idle if self.chain.upstream_fading() => Swap::Idle,
            Swap::Idle => match self.held.take() {
                Some(mut next) => {
                    // As the master's: an edit before an unchanged
                    // convolution takes the playing chain over in place
                    // (on the grid first: a chain that has not run clears
                    // nothing there) and plays at once.
                    next.align_to(grid + m as u64);
                    if next.take_over_upstream(&mut self.chain) {
                        // A swap all the same: a run through this chain
                        // faults (the file would cross the fade).
                        self.measure.chain_swapped(false);
                        let old = std::mem::replace(&mut self.chain, next);
                        self.retire_chain(old);
                        self.swaps_done += 1;
                        return;
                    }
                    let budget = handover_budget(self.p.period, self.p.device_rate);
                    let target = ready_incoming(
                        &mut next,
                        &mut self.chain,
                        grid + m as u64,
                        budget,
                        ring_cap,
                        self.p.device_rate,
                    );
                    Swap::Warming {
                        next,
                        warmed: 0,
                        target,
                    }
                }
                None => Swap::Idle,
            },
            Swap::Warming {
                mut next,
                mut warmed,
                mut target,
            } => {
                if next.align_to(grid) {
                    // Off the grid after all (not reached): its state is
                    // gone; it warms up from here as a cold chain.
                    target = target.max(warmed + next.cold_warm_frames(ring_cap, SWAP_WARM_MARGIN));
                }
                if !other(&mut next, &mut self.swap_buf, &self.stage_out) {
                    self.retire_chain(next);
                    Swap::Idle
                } else {
                    warmed += n_out;
                    if warmed < target {
                        Swap::Warming {
                            next,
                            warmed,
                            target,
                        }
                    } else if next.latency_out_frames() == self.chain.latency_out_frames() {
                        self.measure.chain_swapped(false);
                        let old = self.install(next);
                        Swap::Crossfade {
                            old,
                            pos: 0,
                            len: self.out_ms(SWAP_CROSSFADE_MS),
                        }
                    } else {
                        self.measure.chain_swapped(true);
                        Swap::FadeOut {
                            next,
                            pos: 0,
                            len: self.out_ms(SWAP_FADE_HALF_MS),
                        }
                    }
                }
            }
            Swap::Crossfade { mut old, pos, len } => {
                if !other(&mut old, &mut self.swap_buf, &self.stage_out) {
                    self.retire_chain(old);
                    Swap::Idle
                } else {
                    let (fifo, buf) = (&mut self.fifo[out], &self.swap_buf[..n_out * ch]);
                    for f in 0..n_out {
                        let w = rise(pos + f, len);
                        for c in 0..ch {
                            let i = f * ch + c;
                            let x = f64::from(fifo[i]) * w + f64::from(buf[i]) * (1.0 - w);
                            fifo[i] =
                                x.round().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
                        }
                    }
                    if pos + n_out >= len {
                        self.retire_chain(old);
                        self.swaps_done += 1;
                        Swap::Idle
                    } else {
                        Swap::Crossfade {
                            old,
                            pos: pos + n_out,
                            len,
                        }
                    }
                }
            }
            Swap::FadeOut { mut next, pos, len } => {
                let warm = other(&mut next, &mut self.swap_buf, &self.stage_out);
                // The fade-out's frames left in this call; the new chain
                // fades in from the next one on (its output for the call is
                // in `swap_buf`), as the master switches within a block.
                let end = if warm { (len - pos).min(n_out) } else { n_out };
                for f in 0..n_out {
                    let frame = at + f * ch..at + (f + 1) * ch;
                    if f < end {
                        let w = 1.0 - rise(pos + f, len);
                        for x in &mut self.fifo[frame] {
                            *x = (f64::from(*x) * w).round() as i32;
                        }
                    } else {
                        let w = rise(f - end, len);
                        let new = &self.swap_buf[f * ch..(f + 1) * ch];
                        for (x, &y) in self.fifo[frame].iter_mut().zip(new) {
                            *x = (f64::from(y) * w).round() as i32;
                        }
                    }
                }
                if !warm {
                    // The fade ends here and the playing chain stays.
                    self.retire_chain(next);
                    Swap::FadeIn { pos: 0, len }
                } else if pos + n_out >= len {
                    // At zero: the switch. Its latency steps the follower's
                    // position, as the master's swap steps the master's.
                    let old = self.install(next);
                    self.retire_chain(old);
                    self.swaps_done += 1;
                    self.hold_chain();
                    let done = n_out - end;
                    if done >= len {
                        Swap::Idle
                    } else {
                        Swap::FadeIn { pos: done, len }
                    }
                } else {
                    Swap::FadeOut {
                        next,
                        pos: pos + n_out,
                        len,
                    }
                }
            }
            Swap::FadeIn { pos, len } => {
                for f in 0..n_out {
                    let w = rise(pos + f, len);
                    for x in &mut self.fifo[at + f * ch..at + (f + 1) * ch] {
                        *x = (f64::from(*x) * w).round() as i32;
                    }
                }
                if pos + n_out >= len {
                    Swap::Idle
                } else {
                    Swap::FadeIn {
                        pos: pos + n_out,
                        len,
                    }
                }
            }
        };
    }

    /// Make `next` the playing chain (it takes over the limiter and DC
    /// state); returns the old one.
    fn install(&mut self, mut next: ActiveChain) -> ActiveChain {
        next.inherit_limiter_headroom(&self.chain);
        next.inherit_dc_block_state(&self.chain);
        next.set_limiter_audible(true);
        std::mem::replace(&mut self.chain, next)
    }

    /// Device frames of `ms`.
    fn out_ms(&self, ms: f64) -> usize {
        ((ms * 1e-3 * f64::from(self.p.device_rate)).round() as usize).max(1)
    }

    /// A chain latency changed (here or on the master): hold the readings
    /// until the other side's swap has surely landed too.
    fn hold_chain(&mut self) {
        let hold = (CHAIN_HOLD_S * f64::from(self.p.device_rate)) as u64;
        self.hold_until = self.hold_until.max(self.rendered + hold);
        // Either path's length moved: the slack with it.
        self.slack_best = f64::NAN;
        self.slack_low = f64::NAN;
    }

    /// ε (and its slope) kept where `v·(1 + ε)` stays within the stage's
    /// step range ([`Varispeed::step_range`]) over the speeds it plays
    /// until it settles (the master caps at 1.996 while followed; this
    /// guards the rest).
    fn within_range(&self, e: f64, d: f64) -> (f64, f64) {
        let st = &self.stage;
        let v = st.envelope_step().to_f64();
        // The stage's own bounds (0.499 to 2.0 and the cap's 0.2 % over).
        let (low, high) = st.step_range();
        // (A hair inside: the stage checks the steps exactly.)
        let hi = high / st.peak_until_settled().max(v) - 1.0 - 1e-9;
        let lo = low / st.target().min(v) - 1.0 + 1e-9;
        if e > hi {
            (hi.max(0.0), 0.0)
        } else if e < lo {
            (lo.min(0.0), 0.0)
        } else {
            (e, d)
        }
    }

    /// How far this call's end runs ahead of the master's stage: the
    /// master's playback state (fresh ones only) has its next output.
    fn measure_lead(&mut self, m: usize) {
        if !self.started {
            return;
        }
        let Some(st) = self.shared.state.read() else {
            return;
        };
        if st.at == self.state_at {
            return;
        }
        self.state_at = st.at;
        let end = self.here() + self.stage.step().bits() as i128 * m as i128;
        self.lead_max = self.lead_max.max((end - st.at) as f64 / FRAME as f64);
    }

    /// The level it plays at: when it follows, the master's
    /// ([`TeeShared::master_level`], read without blocking) plus its offset
    /// to it (at most 0 dB), with the master's balance and mute; its own
    /// volume alone otherwise. Set on the ramps when it changes (the first
    /// block snaps: nothing has played yet). Never allocates.
    fn update_level(&mut self) {
        if let Some(t) = self
            .shared
            .master_level
            .read_if_changed(&mut self.master_gen)
        {
            self.master_level = t;
        }
        // One load: its own volume and its following together.
        let (own, offset) = self.controls.level();
        let level = heard_level(own, offset, &self.master_level);
        self.own_lin = audivane_dsp_core::db_to_lin(own);
        if self.level == Some(level) {
            return;
        }
        let ramp = self.level.is_some();
        self.level = Some(level);
        let (volume, balance, muted) = level;
        self.status.level_db.set(volume);
        self.gain
            .set_volume_targets(volume, 0.0, muted, false, false, ramp);
        self.in_gain
            .set_volume_targets(0.0, balance, false, false, false, ramp);
    }

    /// Run `m` stage output frames through the chain into the fifo, and
    /// map them (`v0`: the first one's source position; `None` maps
    /// nothing).
    fn chain_out(&mut self, m: usize, v0: Option<i128>) {
        let ch = self.p.channels;
        let n_out = self.chain.output_frames(m);
        let at = self.fifo_frames * ch;
        let room = self.fifo.len() - at;
        if n_out * ch > room {
            // Not reached (the fifo holds a buffer and a call's output).
            debug_assert!(false, "follower fifo full");
            return;
        }
        self.update_level();
        self.in_gain.process_planar(&mut self.stage_out, m);
        let gain_before = self.gain;
        let input = DspIn::planar(&self.stage_out, m);
        let _ = self.chain.process_in_with_gain(
            input,
            &mut self.fifo[at..at + n_out * ch],
            ch,
            STAGE_FRAMES,
            &mut self.gain,
            None,
        );
        self.run_swap(m, at, n_out, gain_before);
        if self
            .pad
            .process(&mut self.fifo[at..at + n_out * self.p.channels])
        {
            // Its path's length moved: the loop never sees it as a step.
            self.hold_chain();
        }
        if self.out_mute > 0 {
            let k = self.out_mute.min(n_out);
            self.fifo[at..at + k * ch].fill(0);
            self.out_mute -= k;
        }
        let first = self.rendered;
        if self.measure.tapped == m && n_out == m {
            // The tapped stage output, at the follower's own volume (never
            // the master's level) and the measurement's ceiling,
            // crossfaded with the chain's output.
            let g = self.own_lin;
            let out = &mut self.fifo[at..at + n_out * ch];
            for c in 0..ch {
                if self.measure.routed(c) {
                    for f in 0..n_out {
                        let w = self.measure.tap_weight(f);
                        let t = self.measure.ceiling(self.measure.tap[c][f] * g);
                        let x = f64::from(out[f * ch + c]) * (1.0 - w) + t * w * 2_147_483_648.0;
                        out[f * ch + c] = x.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                    }
                }
            }
            self.measure.end_tap(m, first);
        }
        self.fifo_frames += n_out;
        self.rendered += n_out as u64;
        self.chain_in += m as u64;
        self.graph_out += m as u64;
        if self.stage.ramping() {
            // The ramp reaches the DAC after the path: hold until then.
            let tail = (self.stage.latency_frames() + self.chain_graph())
                * f64::from(self.p.ratio.up)
                / f64::from(self.p.ratio.down.max(1));
            self.hold_until = self.hold_until.max(self.rendered + tail.ceil() as u64 + 1);
        }
        if let Some(p0) = v0
            && n_out > 0
        {
            // Two entries per call: its first and its last device frame.
            let p1 = self.out_position();
            self.map.push(first, p0);
            if n_out > 1 {
                let last = p0 + (p1 - p0) * (n_out as i128 - 1) / n_out as i128;
                self.map.push(first + n_out as u64 - 1, last);
            }
            self.note_measure_span(first, n_out, p0, p1);
        } else if v0.is_none() && n_out > 0 {
            // No map (the path shut): what the stage held is gone.
            self.note_measure_unmapped(first, n_out);
        }
        // `active`: the keepalive after the chain (a low-pass chain would
        // take one at its input down), once the call's spans above have
        // told where the measurement ends: nothing of it after that frame.
        let bits = self.chain.output_bits();
        self.measure.post_keepalive(
            &mut self.fifo[at..at + n_out * ch],
            ch,
            first,
            self.p.device_rate,
            bits,
        );
        if self.measure.clamps_output() {
            // `active`: the file goes through the pad; shut, it would
            // leave a silent capture that looks like a clean run.
            if !self.pad.open()
                && self
                    .measure
                    .tracking()
                    .is_some_and(|(c, _)| c == MeasureChain::Active)
            {
                self.measure.fault(MeasureFault::Muted);
            }
            // While the measurement runs here: the whole output at the
            // ceiling, whatever the chain's limiter does.
            self.measure
                .clamp_output(&mut self.fifo[at..at + n_out * ch]);
        }
    }

    /// A measurement: the device frames that carry the file's first frame
    /// and, once the injection is over, its last trace, from a call's span
    /// (source positions at the DAC, `p0..p1` over `n_out` frames from
    /// `first`).
    fn note_measure_span(&mut self, first: u64, n_out: usize, p0: i128, p1: i128) {
        let Some((chain, s0)) = self.measure.tracking() else {
            return;
        };
        let s = self.stage.speed();
        let stage = (self.stage.latency_frames() - 0.5).max(0.0) * s;
        let ratio = self.p.ratio.as_f64();
        // The tap skips the chain: its frames sit the chain's delay ahead.
        let (shift, chain_frames) = match chain {
            MeasureChain::Bypass => (fixed(self.chain_graph() * s), stage * ratio),
            MeasureChain::Active => (0, stage * ratio + self.chain_out_frames() as f64),
        };
        let (a, b) = (p0 + shift, p1 + shift);
        if b <= a {
            return;
        }
        // The fraction of the span before `target` (source frames).
        let frac = |target: i128| ((target - a).max(0)) as f64 / (b - a) as f64;
        let Some(d) = self.measure.device() else {
            return;
        };
        let target = i128::from(s0) * FRAME;
        if d.start_frame().is_none() && target < b {
            let at = first as f64 + frac(target) * n_out as f64;
            d.start_frame.store(at.to_bits(), Ordering::Release);
            d.chain_frames
                .store(chain_frames.to_bits(), Ordering::Release);
        }
        if d.tail() == 0.0 {
            // The stage's filter and, in `active`, the chain's whole tail.
            let tail = match chain {
                MeasureChain::Bypass => stage * ratio,
                MeasureChain::Active => {
                    stage * ratio + (self.chain.tail_out_frames() + self.pad.delay()) as f64
                }
            };
            d.tail_frames.store(tail.to_bits(), Ordering::Release);
        }
        let Some(end) = self.measure.fed_end() else {
            return;
        };
        // Past the stimulus: the stage's filter rings about its latency
        // after it; an active chain rings its tail after its latency.
        let mut margin = (self.stage.latency_frames() + 1.0) * s;
        if chain == MeasureChain::Active {
            let tail = self
                .chain
                .tail_out_frames()
                .saturating_sub(self.chain.latency_out_frames());
            margin += tail as f64 / ratio.max(f64::MIN_POSITIVE);
        }
        let target = (i128::from(s0) + i128::from(end)) * FRAME + fixed(margin);
        if target >= b {
            return;
        }
        if chain == MeasureChain::Bypass && self.measure.tapped > 0 {
            self.measure.note_tap_drained();
        } else {
            // (A bypass that cannot tap, away from x1, faulted: its stage
            // output went through the chain.)
            let at = first as f64 + (frac(target) * n_out as f64).ceil();
            d.set_last_frame(at);
        }
    }

    /// A measurement: a call without a map (the path shut, the stage
    /// reset): nothing of an injection that is over stays in the stage.
    fn note_measure_unmapped(&mut self, first: u64, n_out: usize) {
        let Some((chain, _)) = self.measure.tracking() else {
            return;
        };
        if self.measure.fed_end().is_none() {
            return;
        }
        if chain == MeasureChain::Bypass && self.measure.tapped > 0 {
            self.measure.note_tap_drained();
        } else if let Some(d) = self.measure.device() {
            d.set_last_frame((first + n_out as u64) as f64);
        }
    }

    /// A measurement: when the file's first frame leaves the queue, from a
    /// reading.
    fn note_measure_time(&self, reading: &Reading) {
        let Some(d) = self.measure.device() else {
            return;
        };
        d.note_reading(
            reading.written as i64 - reading.delay,
            reading.time_ns.max(0) as u64,
        );
        let Some(sf) = d.start_frame() else {
            return;
        };
        if d.start_time_ns.load(Ordering::Relaxed) != 0 || sf > reading.written as f64 {
            return;
        }
        let at_dac = reading.written as f64 - reading.delay as f64;
        let t = reading.time_ns as f64 + (sf - at_dac) * 1e9 / f64::from(self.p.device_rate);
        d.start_time_ns
            .store(t.max(1.0).round() as u64, Ordering::Release);
        let queue = reading.delay as f64 + self.fifo_frames as f64;
        d.queue_frames.store(queue.to_bits(), Ordering::Release);
    }

    /// One period's reading of the follower's device (`written` its frames
    /// written so far): the master's published position, the detector, the
    /// loop, ε for the coming period, skips and the status.
    pub fn on_reading(&mut self, reading: Reading) {
        self.note_measure_time(&reading);
        self.update_pad();
        let g = f64::from(self.p.graph_rate);
        let period_s = period_seconds(&self.p);
        let chain_seq = self.shared.chain_seq.load(Ordering::Relaxed);
        if chain_seq != self.chain_seq {
            self.chain_seq = chain_seq;
            self.hold_chain();
            // (The master's position at its DAC moves with its chain.)
            self.bias_m.rebase();
        }
        let pair_gen = self.controls.pair_gen.load(Ordering::Acquire);
        if pair_gen != self.pair_gen {
            // Another master (or the disk's memory for this pair): its
            // clock ratio is not the last master's.
            self.pair_gen = pair_gen;
            let mem = self.controls.memory_at(rate_family(self.p.device_rate));
            seed_loop(&mut self.lp, mem);
            self.locked = false;
            self.bias_m = BiasTracker::new();
            self.bias_m.remember_rms_s(mem.master_bias_s);
            self.bias_f.remember_rms_s(mem.follower_bias_s);
        }
        let point = self.shared.position.read();
        self.master_valid = point.is_some();
        if !self.master_valid {
            // The master stopped (or plays nothing to follow): the ring's
            // end stops while the follower plays on, so what it has ahead
            // falls without anything an alignment could cover (hundreds of
            // ms before a rate change).
            self.slack_min = f64::INFINITY;
            self.known_jump = 0;
            self.slack_best = f64::NAN;
            self.slack_low = f64::NAN;
            self.slack_void = true;
        }
        if let Some(p) = point {
            if self.master_epoch.is_some_and(|e| e != p.epoch) && self.started {
                // A new master stream: where it plays from is measured
                // again (the follower played on, its read point ran on
                // through a pause).
                self.lp.realign();
            }
            if self.master_epoch != Some(p.epoch) {
                self.bias_m.rebase();
            }
            self.master_epoch = Some(p.epoch);
            // The master's reading against its clock: a new stream, a known
            // step, a speed change or a ramp starts its fit again.
            let (t_seen, shift_seen, speed_seen) = self.bias_m_seen;
            if p.ramping || p.shift != shift_seen || p.speed != speed_seen {
                self.bias_m.rebase();
            }
            if p.time_ns != t_seen {
                self.bias_m_dev = if p.ramping {
                    f64::NAN
                } else {
                    self.bias_m
                        .push(
                            p.pos as f64 / FRAME as f64,
                            p.time_ns as f64 * 1e-9,
                            g * p.speed,
                        )
                        .unwrap_or(f64::NAN)
                };
            }
            if p.time_ns != t_seen {
                let odd = !self.master_odd
                    && !p.ramping
                    && self.master_line.is_some_and(|(pos, t, epoch, shift)| {
                        let line =
                            pos as f64 / FRAME as f64 + (p.time_ns - t) as f64 * 1e-9 * g * p.speed;
                        epoch == p.epoch
                            && shift == p.shift
                            && (p.pos as f64 / FRAME as f64 - line).abs() > MASTER_ODD_S * g
                    });
                self.master_odd = odd;
                if !odd {
                    self.master_line = Some((p.pos, p.time_ns, p.epoch, p.shift));
                }
            }
            self.bias_m_seen = (p.time_ns, p.shift, p.speed);
            if !self.master_odd {
                self.det.master_position(p.pos, p.time_ns);
            }
            if p.speed != self.master_speed && p.speed.is_finite() && p.speed > 0.0 {
                self.master_speed = p.speed;
                self.det.set_master_rate(g * p.speed);
            }
            self.master_ramping = p.ramping;
        }
        // The master's known steps (an alignment change, its stage
        // installed or retired), stored before the point above was: the
        // same step here at once, and the readings held until the master's
        // DAC has played it too.
        // (The announcement first: the master clears it only after the
        // step it announced is in `steps`.)
        self.announced = super::tee::decode_pos(self.shared.step_announce.load(Ordering::Acquire));
        let steps = self.shared.steps.load(Ordering::Acquire) as i64;
        if steps != self.step_seen {
            let frames = step_frames(steps.wrapping_sub(self.step_seen));
            self.step_seen = steps;
            let at = self.shared.step_at.load(Ordering::Relaxed) as i64;
            if self.known_step(frames, 0.0) {
                self.deferred_at = Some(at);
            }
        }
        // The trim and the lateness are playback time. A change of either is a
        // step the follower takes exactly (later: it repeats), in source frames
        // at its own stage's speed: the skip moves its read point there, so its
        // output in time moves by the frames over that speed (through a master
        // ramp the master's DAC still plays the old speed while the follower's
        // stage, ahead of it, plays the new). A speed change is not a step: the
        // follower replays the master's commands at the same source positions,
        // so its lateness in frames follows the speed by itself, and only the
        // detector's offset follows it, at the master's published speed.
        let lateness = self.controls.trim_ms() + self.controls.lag_ms();
        if lateness != self.lateness_ms {
            if self.lateness_ms.is_finite() {
                self.known_step(0.0, lateness - self.lateness_ms);
            }
            self.lateness_ms = lateness;
        }
        let offset = lateness * 1e-3 * g * self.master_speed;
        if offset != self.offset {
            self.offset = offset;
            self.det.set_offset(offset);
        }
        let rate_now = g * self.stage.envelope_step().to_f64();
        if rate_now != self.loop_rate {
            self.loop_rate = rate_now;
            self.lp.set_rate(rate_now);
            // (The slack in playback time moves with the speed.)
            self.slack_best = f64::NAN;
            self.slack_low = f64::NAN;
        }
        // The reading counts when the master is valid and steady, past its
        // known steps, and the frame at the DAC was written after the last
        // skip or restart (none pending).
        let usable = self.master_valid
            && !self.master_odd
            && self.started
            && !self.master_ramping
            && point.is_some_and(|p| p.shift == self.step_seen)
            && self.pending_skip == 0
            && !self.deferred
            && reading.played().is_some_and(|f| f >= self.hold_until);
        let e = if usable {
            self.det.follower(reading, &self.map)
        } else {
            None
        };
        let e = e.unwrap_or(f64::NAN);
        // Each device's reading bias against its long-run mean, taken out
        // where its fit has run (both devices' reading noise goes with it);
        // where it has not, what a short window's bias may be bounds the
        // placement and the lock.
        let dev_f = reading
            .played()
            .and_then(|f| {
                self.bias_f.push(
                    f as f64,
                    reading.time_ns as f64 * 1e-9,
                    f64::from(self.p.device_rate),
                )
            })
            .unwrap_or(f64::NAN);
        let r = self.p.ratio;
        let src_per_device =
            self.stage.envelope_step().to_f64() * f64::from(r.down) / f64::from(r.up.max(1));
        let out = (self.bias_m.established(), self.bias_f.established());
        if out != self.bias_out {
            // The readings before do not compare with these (a device's
            // bias was in them, or is now).
            self.bias_out = out;
            self.lp.forget_readings();
        }
        // (A device whose deviations are taken out, without one for this
        // reading: an outlier, not used.)
        let outlier = (out.0 && !self.bias_m_dev.is_finite()) || (out.1 && !dev_f.is_finite());
        let mut e = if outlier { f64::NAN } else { e };
        if out.0 && self.bias_m_dev.is_finite() {
            e -= self.bias_m_dev;
        }
        if out.1 && dev_f.is_finite() {
            e += dev_f * src_per_device;
        }
        // What each device's reading bias may still be (seconds), as a
        // bound (placement and the lock wait for the floor less it) and a
        // spread (how far the readings may stray without the follower being
        // out of place): where it is taken out, what the line's offset may
        // be off by (nothing once a whole fit has run); on a coarse grid not
        // yet seen through a whole sweep, nothing to wait for but a step of
        // spread (heard at once, up to about a packet off until the sweep
        // has been seen: at a wrap the readings jump a whole packet from a
        // follower that went along with them); its short-window RMS where that was
        // measured (now or on an earlier run); unknown before its grid is
        // told.
        let left = |t: &BiasTracker| -> (f64, f64) {
            if t.established() {
                (BIAS_SIGMAS * t.left_s(), 0.0)
            } else if let Some(q) = t.coarse_grid_s() {
                (0.0, q)
            } else if t.rms_known() {
                (BIAS_SIGMAS * t.rms_s(), 0.0)
            } else if !t.grid_known() {
                // (Not placed before its grid is told, a fraction of a
                // second once.)
                (f64::INFINITY, 0.0)
            } else {
                (0.0, 0.0)
            }
        };
        let (bm, sm) = left(&self.bias_m);
        let (bf, sf) = left(&self.bias_f);
        let bound = bm.hypot(bf) * self.loop_rate;
        self.lp.set_spread(sm.hypot(sf) * self.loop_rate);
        self.lp.set_bias_bound(bound);
        let c = self.lp.update(e, period_s);
        self.rate_est.push(e, self.loop_rate);
        if c.skip != 0 {
            // The loop places the follower anew: no remainder carries.
            self.step_rest = 0.0;
            self.pending_skip += c.skip;
            self.rate_est.skipped(c.skip as f64);
            self.status.skips.fetch_add(1, Ordering::Relaxed);
        }
        if c.skip != 0 || self.lp.skip_pending() || !self.lp.aligned() {
            // The read point was not where it should be (a skip moves it:
            // a re-sync from lock, or a second skip of the acquisition
            // after a first alignment that was off): the slack measured
            // against it is not what the follower lacks, and would grow
            // the alignment or its lateness by as much (once by seconds,
            // after a reopen).
            self.slack_min = f64::INFINITY;
            self.known_jump = 0;
            self.slack_best = f64::NAN;
            self.slack_low = f64::NAN;
        }
        self.locked = c.locked;
        // ε over the coming period, from where the stage is.
        let from = self.stage.epsilon();
        self.rate_est.advance(period_s, self.seg_from, self.seg_to);
        self.seg_start = self.graph_out;
        self.seg_from = from;
        self.seg_to = (c.eps, c.eps_slope);
        self.seg_h = period_s;
        // The source slack, as playback time, at the worst phase of the
        // two clocks: the window's samples are a follower period apart, and
        // the ring's end steps by a master write, so the least within the
        // window can miss the worst by the shorter of the two (the
        // spacing). The window's least less the spacing is a lower bound of
        // the worst at any phase, and so is the best of them since the
        // last change: the phase drifts (by ppm), and a bound taken at the
        // worst phase itself would count the spacing twice, growing the
        // alignment by it once per session. Its own known steps shift the
        // best by what they move; anything else starts it again (a skip, an
        // xrun, a chain or speed change, a new master write size), and so
        // does a window that came within 1 ms of running out.
        self.slack_age += period_s;
        let window = if self.slack_first {
            FIRST_SLACK_WINDOW_S
        } else {
            SLACK_WINDOW_S
        };
        if self.slack_age >= window {
            let spacing = (period_s * self.loop_rate).min(self.known_jump as f64);
            self.slack_last = if self.slack_min.is_finite() && !self.slack_void {
                let ms = |f: f64| f / self.loop_rate * 1e3;
                let bound = ms(self.slack_min - spacing);
                let moved = ms((spacing - self.spacing_prev).abs());
                if ms(self.slack_min) < 1.0
                    || moved.is_nan()
                    || moved > 1.0
                    || self.slack_best.is_nan()
                {
                    self.slack_best = bound;
                } else {
                    self.slack_best = self.slack_best.max(bound);
                }
                self.slack_low = if self.slack_low.is_nan() {
                    bound
                } else {
                    self.slack_low.min(bound)
                };
                self.spacing_prev = spacing;
                self.slack_best
            } else {
                f64::NAN
            };
            if self.slack_last.is_finite() {
                self.slack_first = false;
            }
            self.slack_min = f64::INFINITY;
            self.known_jump = 0;
            self.slack_age = 0.0;
            self.slack_void = false;
            if self.lead_max.is_finite() {
                self.lead_last = self.lead_max / self.loop_rate * 1e3;
            }
            self.lead_max = f64::NEG_INFINITY;
        }
        self.publish(reading);
    }

    /// A step of the follower's position it knows exactly: `frames` source
    /// frames (the master's, later negative) and `ms` more lateness (its trim
    /// and lateness, later positive). It waits for a clear stretch of the source
    /// (`skip_clear`; the follower plays on, its readings held), then whole
    /// frames are skipped under the fade, the loop left as it is (locked, its
    /// rate state and filters), so the error carries on within half a frame.
    /// Before the loop's first alignment, that alignment takes it, from readings
    /// after the step only.
    /// Returns whether it waits as a skip (not taken by an alignment).
    fn known_step(&mut self, frames: f64, ms: f64) -> bool {
        if !self.started || !frames.is_finite() || !ms.is_finite() {
            return false;
        }
        if !self.lp.aligned() {
            // (A loop placing keeps its rate state: only the readings
            // before the step go.)
            if self.lp.trusted() {
                self.lp.realign();
            } else {
                self.lp.reset();
            }
            self.step_rest = 0.0;
            return false;
        }
        self.status.known_steps.fetch_add(1, Ordering::Relaxed);
        if !self.deferred {
            self.deferred_since = self.graph_out;
        }
        self.deferred = true;
        self.deferred_frames += frames;
        self.deferred_ms += ms;
        true
    }

    /// The waiting known steps in source frames now (the lateness at the
    /// stage's speed: the skip moves its read point, so its output moves
    /// in time by the frames over that speed).
    fn deferred_step(&self) -> f64 {
        let v = self.stage.envelope_step().to_f64();
        self.deferred_frames - self.deferred_ms * 1e-3 * f64::from(self.p.graph_rate) * v
    }

    /// Take the waiting known steps (`frames` of them now) as a skip of
    /// whole frames; the rest carries into the next one.
    fn commit_step(&mut self, frames: f64) {
        let parts = (
            self.deferred_frames,
            self.deferred_ms,
            self.step_rest,
            self.deferred_since,
        );
        self.deferred = false;
        self.deferred_at = None;
        self.deferred_frames = 0.0;
        self.deferred_ms = 0.0;
        let total = frames + self.step_rest;
        let k = total.round() as i64;
        self.step_rest = total - k as f64;
        if k == 0 {
            return;
        }
        self.pending_skip += k;
        self.committed = Some((parts.0, parts.1, parts.2, k, parts.3));
        // The raw phase carries what the error kept of it.
        self.rate_est.skipped(k as f64 - frames);
        // The slack before it is not what the follower has now.
        self.slack_min = f64::INFINITY;
        self.known_jump = 0;
    }

    fn publish(&self, reading: Reading) {
        let s = &self.status;
        let r = Ordering::Relaxed;
        if self.locked && !self.lp.placed() {
            self.controls
                .remember_rate(self.p.device_rate, self.lp.rate_eps());
        }
        if self.bias_m.rms_known() || self.bias_f.rms_known() {
            self.controls.remember_bias(
                self.p.device_rate,
                known_rms(&self.bias_m),
                known_rms(&self.bias_f),
            );
        }
        s.set_state(if self.locked {
            FollowerState::Locked
        } else {
            FollowerState::Acquiring
        });
        s.ppm.store(bits(self.lp.ppm()), r);
        s.phase_error.store(bits(self.lp.filtered_error()), r);
        s.bandwidth.store(bits(self.lp.bandwidth()), r);
        s.eps.store(bits(self.stage.epsilon().0), r);
        s.gated.store(self.lp.gated(), r);
        s.slack_ms.store(bits(self.slack_last), r);
        s.slack_low_ms.store(bits(self.slack_low), r);
        s.slack_speed
            .store(bits(self.loop_rate / f64::from(self.p.graph_rate)), r);
        s.lead_ms.store(bits(self.lead_last), r);
        let chain = self.chain.latency_out_frames();
        s.chain_graph.set(self.out_graph(chain));
        s.chain_frames.store(chain as u64, r);
        s.pad_frames.store(self.pad.delay() as u64, r);
        // The path from the read point to the DAC.
        if let (Some(f), true) = (reading.played(), self.started) {
            let lat = self
                .map
                .source_at(f)
                .map(|p| (i128::from(self.want) * FRAME - p) as f64 / FRAME as f64);
            if let Some(l) = lat {
                s.latency_ms.store(bits(l / self.loop_rate * 1e3), r);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The device.

/// A device fault the follower cannot play through, as the errno the
/// device gave (carried out of the audio loop as is: nothing is formatted
/// or recovered on it, [`fault_text`] after it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcmFault {
    /// The device is gone (unplugged), suspended or failed: close it,
    /// reopen later.
    Lost(i32),
}

/// What a fault's errno means (off the audio path).
pub fn fault_text(errno: i32) -> String {
    match errno {
        libc::ESTRPIPE => "suspended".into(),
        e if e > 0 => std::io::Error::from_raw_os_error(e).to_string(),
        _ => "device error".into(),
    }
}

/// The follower's device, as [`run_device`] drives it.
pub trait FollowerPcm {
    /// Device frames queued ahead of the DAC, and when that was read
    /// (`CLOCK_MONOTONIC_RAW` ns, the master's readings' clock).
    fn status(&mut self) -> Result<(i64, i64), PcmFault>;
    /// Frames that can be written now.
    fn avail(&mut self) -> Result<usize, PcmFault>;
    /// Write interleaved frames (non-blocking): frames written.
    fn write(&mut self, samples: &[i32]) -> Result<usize, PcmFault>;
    /// Start playing what was written (after a prime).
    fn start(&mut self) -> Result<(), PcmFault>;
    /// Wait until a period can be written, or `timeout_ms`; false on a
    /// timeout.
    fn wait(&mut self, timeout_ms: u32) -> Result<bool, PcmFault>;
    /// An xrun was recovered: the device stopped, waiting for a prime and
    /// a start.
    fn needs_restart(&self) -> bool;
    /// Drop whatever the device still holds and make it ready for a prime
    /// (a recovery may leave it prepared with frames written after it).
    fn discard(&mut self) -> Result<(), PcmFault> {
        Ok(())
    }
    /// The run should end (a stop, or the master's rate changed).
    fn interrupted(&self) -> bool {
        false
    }
    /// A point where the overload guard runs.
    fn checkpoint(&mut self) {}
}

/// Why [`run_device`] returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunExit {
    /// Asked to stop.
    Stopped,
    /// The master's graph rate changed: rebuild for it.
    Rebuild,
    /// The device faulted ([`PcmFault`]'s errno).
    Lost(i32),
}

/// Prime the device's buffer with rendered periods and start it.
pub fn prime_device<P: FollowerPcm>(pcm: &mut P, rt: &mut FollowerRt) -> Result<(), PcmFault> {
    let p = rt.params();
    let periods = (p.buffer / p.period).max(1);
    for _ in 0..periods {
        rt.fill(p.period);
        let n = pcm.write(rt.pending(p.period))?;
        rt.advance(n);
        if n < p.period {
            break;
        }
    }
    pcm.start()
}

/// One wakeup of the follower's audio loop (allocation-free): the reading,
/// then as many periods as the device takes. `Some` ends the run.
pub fn step_device<P: FollowerPcm>(
    pcm: &mut P,
    rt: &mut FollowerRt,
    stop: &FollowerControls,
) -> Option<RunExit> {
    let p = rt.params();
    let r = (|| -> Result<Option<RunExit>, PcmFault> {
        if stop.stopping() || pcm.interrupted() {
            return Ok(Some(RunExit::Stopped));
        }
        if rt.shared.graph_rate.load(Ordering::Relaxed) != p.graph_rate || stop.take_reopen() {
            return Ok(Some(RunExit::Rebuild));
        }
        let timeout = ((p.buffer as f64 / f64::from(p.device_rate) * 1e3) as u32).max(10) + 100;
        if !pcm.wait(timeout)? {
            return Ok(None);
        }
        pcm.checkpoint();
        // A recovered xrun (here, or inside the status or room query):
        // shut, the device's queue dropped, primed and started again;
        // nothing rendered before it is written after it.
        let restart = |pcm: &mut P, rt: &mut FollowerRt| -> Result<Option<RunExit>, PcmFault> {
            rt.xrun();
            pcm.discard()?;
            prime_device(pcm, rt)?;
            Ok(None)
        };
        if pcm.needs_restart() {
            return restart(pcm, rt);
        }
        let (delay, time_ns) = pcm.status()?;
        if pcm.needs_restart() {
            return restart(pcm, rt);
        }
        rt.on_reading(Reading {
            written: rt.written(),
            delay,
            time_ns,
        });
        let mut room = pcm.avail()?;
        if pcm.needs_restart() {
            return restart(pcm, rt);
        }
        while room >= p.period {
            rt.fill(p.period);
            let n = pcm.write(rt.pending(p.period))?;
            rt.advance(n);
            if n < p.period || pcm.needs_restart() {
                break;
            }
            room -= n;
        }
        Ok(None)
    })();
    match r {
        Ok(x) => x,
        Err(PcmFault::Lost(e)) => Some(RunExit::Lost(e)),
    }
}

/// The follower's audio loop on `pcm`: prime, then [`step_device`] until
/// it ends.
pub fn run_device<P: FollowerPcm>(
    pcm: &mut P,
    rt: &mut FollowerRt,
    stop: &FollowerControls,
) -> RunExit {
    if let Err(PcmFault::Lost(e)) = prime_device(pcm, rt) {
        return RunExit::Lost(e);
    }
    loop {
        if let Some(x) = step_device(pcm, rt, stop) {
            return x;
        }
    }
}

/// The follower's ALSA device.
pub struct AlsaFollowerPcm {
    out: audivane_io_alsa::AlsaOutput,
    guard: audivane_io_alsa::RunGuard,
    /// `ReserveDevice1`, so a session server lets the device go (dropped
    /// after the device).
    _reservation: Option<audivane_io_alsa::DeviceReservation>,
}

fn lost(e: audivane_io_alsa::AlsaError) -> PcmFault {
    match e {
        audivane_io_alsa::AlsaError::Alsa(a) => PcmFault::Lost(a.errno()),
        // Not reached (the per-period calls fail with ALSA errors only).
        _ => PcmFault::Lost(0),
    }
}

impl AlsaFollowerPcm {
    /// Open `device` at `graph_rate × ratio` (the ratio's ladder from
    /// `desired`), `period_graph` graph frames per period and
    /// `buffer_periods` periods, its left and right on `output_channels`
    /// (`None`: the first two; allocates, off the audio path).
    pub fn open(
        device: &str,
        graph_rate: u32,
        desired: RateRatio,
        period_graph: u32,
        buffer_periods: u32,
        output_channels: Option<audivane_protocol::ipc::OutputChannels>,
    ) -> Result<(Self, RateRatio, SampleFormat, u32, u32), String> {
        let cfg = audivane_io_alsa::AlsaConfig {
            device: device.to_string(),
            rate: graph_rate,
            channels: 2,
            channel_map: output_channels.map(|p| p.0.to_vec()).unwrap_or_default(),
            period_frames: desired.scale_to_output(period_graph),
            buffer_periods: buffer_periods.max(2),
            preferred_format: None,
            strict_format: false,
            nonblock: true,
        };
        let reservation = match audivane_io_alsa::DeviceReservation::acquire_for_device(
            device,
            audivane_io_alsa::DEFAULT_RESERVE_PRIORITY,
        ) {
            Ok(r) => Some(r),
            Err(e) => {
                log::warn!("follower {device}: no reservation ({e}); opening anyway");
                None
            }
        };
        let ladder = audivane_io_alsa::ratio_ladder(desired);
        let (mut out, ratio) = audivane_io_alsa::AlsaOutput::open_with_ratio_fallback(
            &cfg,
            graph_rate,
            &ladder,
            period_graph,
            &audivane_io_alsa::FORMAT_PREFERENCE,
            None,
        )
        .map_err(|e| format!("{device}: {e}"))?;
        // A suspend ends the run (resumed by the reopen, off the audio
        // loop): alsa-lib's own resume waits in 1 s steps.
        out.set_suspend_recovery(false);
        let p = out.params();
        Ok((
            Self {
                out,
                guard: audivane_io_alsa::RunGuard::new(audivane_io_alsa::rt::MAX_RUN_WITHOUT_SLEEP),
                _reservation: reservation,
            },
            ratio,
            p.format,
            p.period_frames,
            p.buffer_frames,
        ))
    }
}

impl FollowerPcm for AlsaFollowerPcm {
    fn status(&mut self) -> Result<(i64, i64), PcmFault> {
        let (ts, delay) = self.out.status_timestamp_ns().map_err(lost)?;
        Ok((delay, ts as i64))
    }

    fn avail(&mut self) -> Result<usize, PcmFault> {
        Ok(self.out.avail().map_err(lost)? as usize)
    }

    fn write(&mut self, samples: &[i32]) -> Result<usize, PcmFault> {
        self.out.write_i32_interleaved(samples).map_err(lost)
    }

    fn start(&mut self) -> Result<(), PcmFault> {
        self.out.start().map_err(lost)
    }

    fn wait(&mut self, timeout_ms: u32) -> Result<bool, PcmFault> {
        self.out.wait_for_period(timeout_ms).map_err(lost)
    }

    fn needs_restart(&self) -> bool {
        self.out.needs_restart()
    }

    fn discard(&mut self) -> Result<(), PcmFault> {
        self.out.drop_frames().map_err(lost)?;
        self.out.prepare().map_err(lost)
    }

    fn checkpoint(&mut self) {
        self.guard.check();
    }
}

impl Drop for AlsaFollowerPcm {
    fn drop(&mut self) {
        let _ = self.out.drop_frames();
    }
}

// ---------------------------------------------------------------------------
// The control side.

/// Builds a follower's chain from its config slot for its device: the
/// graph rate, the ratio it opened at and its sample format (off the audio
/// path).
pub type ChainBuilder = Arc<
    dyn Fn(&EngineConfig, u32, RateRatio, SampleFormat) -> Result<ActiveChain, String>
        + Send
        + Sync,
>;

/// A follower's period in graph frames for `period_ms` at `graph_rate` and
/// the ratio it asks for: the power of two of device frames nearest
/// `period_ms` at that device rate (5 ms: 4096 frames at 768 kHz, 256 at
/// 48 kHz), in graph frames (the device's open scales it back by the ratio
/// it lands at), within 32..=8192.
pub fn period_graph_for(period_ms: f64, graph_rate: u32, desired: RateRatio) -> u32 {
    let ms = if period_ms.is_finite() && period_ms > 0.0 {
        period_ms
    } else {
        ipc::FOLLOWER_DEFAULT_PERIOD_MS
    };
    let out_rate = f64::from(desired.output_rate(graph_rate).unwrap_or(graph_rate));
    let frames = (ms * 1e-3 * out_rate).max(1.0);
    let pow2 = frames.log2().round().exp2();
    let graph = (pow2 * f64::from(desired.down.max(1)) / f64::from(desired.up.max(1))).round();
    graph.clamp(32.0, 8192.0) as u32
}

/// The rate ratio the engine would pick for a device at a graph rate.
pub type RatioFn = Arc<dyn Fn(&EngineConfig, u32) -> RateRatio + Send + Sync>;

/// Follower `f`'s offset to the master's level while it follows: its
/// `volume_db` minus the master's volume it was set at (`follow_ref_db`;
/// without one, the master's configured level now, as the config's
/// normalization sets it).
pub fn follow_offset_db(f: &FollowerConfig, master: &EngineConfig) -> Option<f64> {
    f.follow_volume.then(|| {
        let r = f
            .follow_ref_db
            .filter(|r| r.is_finite())
            .unwrap_or_else(|| super::volume::target_db(&master.volume));
        f.volume_db - r
    })
}

/// The level follower `f` plays at while the master's level is
/// `master_db` (the master's effective, played volume, as the follower
/// tracks it): its `volume_db`, moved by the master's change since
/// `follow_ref_db` when it follows, within `-120..=0` dB (what its
/// `volume_db` can hold, so unticking or unloading keeps it).
pub fn followed_level_db(f: &FollowerConfig, master_db: f64) -> f64 {
    let level = match (f.follow_volume, f.follow_ref_db) {
        (true, Some(r)) if r.is_finite() && master_db.is_finite() => f.volume_db + master_db - r,
        _ => f.volume_db,
    };
    level.clamp(ipc::VOLUME_MIN_DB, 0.0)
}

/// How a follower is opened (internal: no protocol yet).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FollowerOptions {
    /// Its delay behind the master, ms (±[`MAX_TRIM_MS`]).
    pub trim_ms: f64,
    /// Its digital volume, dB.
    pub volume_db: f64,
    /// Its offset to the master's level, while it follows
    /// ([`follow_offset_db`]).
    pub follow_offset_db: Option<f64>,
    /// Its period in graph frames, fixed (tests; `None`: from its
    /// `period_ms`, [`period_graph_for`]).
    pub period_graph: Option<u32>,
    /// Its device period, ms ([`FollowerConfig::period_ms`]; `None`: the
    /// default).
    pub period_ms: Option<f64>,
    pub buffer_periods: u32,
}

impl Default for FollowerOptions {
    fn default() -> Self {
        Self {
            trim_ms: 0.0,
            volume_db: 0.0,
            follow_offset_db: None,
            period_graph: None,
            period_ms: None,
            buffer_periods: 3,
        }
    }
}

/// One running follower.
struct FollowerHandle {
    controls: Arc<FollowerControls>,
    status: Arc<FollowerStatus>,
    config: Arc<Mutex<EngineConfig>>,
    /// What its device opened at (graph rate, ratio, format), while open:
    /// a config edit builds its new chain for it.
    opened: Arc<Mutex<Option<OpenedAt>>>,
    swaps: ChainSwapHost,
    thread: Option<JoinHandle<()>>,
    lag: LagControl,
    /// It plays its own chain (a patch), as of the last config.
    patched: bool,
}

/// A follower's open device, as its chain is built for it.
type OpenedAt = (u32, RateRatio, SampleFormat);

/// Whether an edit from `a` to `b` changes what a chain is built from.
fn chain_inputs_differ(a: &EngineConfig, b: &EngineConfig) -> bool {
    a.dsp_enabled != b.dsp_enabled || a.quantum != b.quantum || a.chain != b.chain
}

impl FollowerHandle {
    fn reap(&mut self) {
        self.swaps.reap();
    }

    fn stop(&mut self) {
        self.controls.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What a follower thread needs.
struct ThreadArgs {
    device: String,
    reader: TeeReader,
    shared: TeeShared,
    config: Arc<Mutex<EngineConfig>>,
    build: ChainBuilder,
    opts: FollowerOptions,
    status: Arc<FollowerStatus>,
    controls: Arc<FollowerControls>,
    desired_ratio: RatioFn,
    opened: Arc<Mutex<Option<OpenedAt>>>,
    swaps: ChainSwaps,
}

/// Sleep `total` in short steps, returning early on a stop.
fn nap(controls: &FollowerControls, total: Duration) {
    let step = Duration::from_millis(20);
    let mut left = total;
    while !left.is_zero() && !controls.stopping() {
        let d = left.min(step);
        std::thread::sleep(d);
        left -= d;
    }
}

/// The follower thread: open, build, play; on a loss or a rate change,
/// close and do it again.
fn follower_thread(a: ThreadArgs) {
    audivane_io_alsa::silence_alsa_lib_on_this_thread();
    let ThreadArgs {
        device,
        reader,
        shared,
        config,
        build,
        opts,
        status,
        controls,
        desired_ratio,
        opened,
        swaps,
    } = a;
    let mut reader = Some(reader);
    let mut swaps = Some(swaps);
    let set_opened = |o: Option<OpenedAt>| {
        if let Ok(mut g) = opened.lock() {
            *g = o;
        }
    };
    let mut first = true;
    while !controls.stopping() {
        let graph_rate = shared.graph_rate.load(Ordering::Relaxed);
        if graph_rate == 0 {
            nap(&controls, Duration::from_millis(50));
            continue;
        }
        if !first {
            status.reopens.fetch_add(1, Ordering::Relaxed);
        }
        first = false;
        status.set_state(FollowerState::Opening);
        let cfg = match config.lock() {
            Ok(c) => c.clone(),
            Err(_) => break,
        };
        let desired = desired_ratio(&cfg, graph_rate);
        // Its own period (about 5 ms by default), not the master's
        // quantum: a short period needs little source ahead of the
        // master's, so the master's alignment stays small. Its chain keeps
        // the master's quantum (the same convolver blocks and latency).
        let period_graph = opts
            .period_graph
            .unwrap_or_else(|| period_graph_for(controls.period_ms(), graph_rate, desired));
        let opened = AlsaFollowerPcm::open(
            &device,
            graph_rate,
            desired,
            period_graph,
            opts.buffer_periods,
            cfg.output_channels,
        );
        let (mut pcm, ratio, format, period, buffer) = match opened {
            Ok(x) => {
                let p = x.0.out.params();
                let r = Ordering::Relaxed;
                status
                    .device_channels
                    .store(u64::from(p.device_channels), r);
                status
                    .device_channels_max
                    .store(u64::from(p.device_channels_max), r);
                x
            }
            Err(e) => {
                status.set_error(Some(e));
                status.set_state(FollowerState::Lost);
                // Back when the device is (the device monitor's view of a
                // replug), at most once a second.
                nap(&controls, Duration::from_secs(1));
                while !controls.stopping() && !audivane_io_alsa::device_present(&device) {
                    nap(&controls, Duration::from_secs(1));
                }
                continue;
            }
        };
        let chain = match build(&cfg, graph_rate, ratio, format) {
            Ok(c) => c,
            Err(e) => {
                // Not played (a convolution over the boost cap, an IR gone):
                // shut, with why, and tried again.
                log::warn!("follower {device}: chain not built: {e}");
                status.set_error(Some(format!("chain: {e}")));
                status.set_state(FollowerState::Off);
                drop(pcm);
                nap(&controls, Duration::from_secs(2));
                continue;
            }
        };
        let params = FollowerParams {
            graph_rate,
            channels: 2,
            device_rate: ratio.output_rate(graph_rate).unwrap_or(graph_rate),
            period: period as usize,
            buffer: buffer as usize,
            ratio,
        };
        let Some(rd) = reader.take() else { break };
        let mut rt = match FollowerRt::new(
            params,
            rd,
            shared.clone(),
            chain,
            Arc::clone(&status),
            Arc::clone(&controls),
        ) {
            Ok(rt) => rt,
            Err(e) => {
                status.set_error(Some(e));
                status.set_state(FollowerState::Lost);
                // The reader went with the failed build: nothing to play
                // from any more.
                break;
            }
        };
        // Chains built for an earlier open: this one is built from the
        // latest config already.
        if let Some(s) = swaps.as_mut() {
            while let Ok(c) = s.incoming.pop() {
                drop(c);
            }
        }
        rt.set_swaps(swaps.take());
        set_opened(Some((graph_rate, ratio, format)));
        status.set_error(None);
        let promoted = audivane_io_alsa::try_promote_current_thread_rt(FOLLOWER_RT_PRIORITY);
        if promoted.is_none() {
            log::warn!("follower {device}: no real-time priority");
        }
        let exit = run_device(&mut pcm, &mut rt, &controls);
        let _ = audivane_io_alsa::demote_current_thread();
        set_opened(None);
        drop(pcm);
        let (rd, sw) = rt.into_parts();
        reader = Some(rd);
        swaps = sw;
        match exit {
            RunExit::Stopped => break,
            RunExit::Rebuild => {}
            RunExit::Lost(errno) => {
                // Off the audio loop and demoted: format it, and a
                // suspended device recovers by the reopen.
                let e = fault_text(errno);
                log::warn!("follower {device}: lost ({e})");
                status.set_error(Some(e));
                status.set_state(FollowerState::Lost);
                nap(&controls, Duration::from_millis(500));
                while !controls.stopping() && !audivane_io_alsa::device_present(&device) {
                    nap(&controls, Duration::from_secs(1));
                }
            }
        }
    }
    status.audible.store(false, Ordering::Relaxed);
    if controls.stopping() {
        status.set_state(FollowerState::Off);
    }
}

/// The master's alignment from `current` (ms) when the followers' least source
/// slack over the last second was `least_slack_ms` (at the worst phase of the
/// clocks): when a follower had less than 1 ms ahead, enough for 2 ms, within
/// the cap. It only grows: a change steps the master's position, and every
/// follower re-acquires (muted for a few seconds). `None`: leave it.
pub fn next_alignment_ms(current: f64, least_slack_ms: f64) -> Option<f64> {
    grow_ms(current, least_slack_ms, super::tee::MAX_ALIGN_MS)
}

/// A follower's lateness from `current` (ms) once the master's alignment
/// is at its cap, by the same rule as [`next_alignment_ms`], within
/// [`MAX_LAG_MS`] (it plays unaligned).
pub fn next_lag_ms(current: f64, slack_ms: f64) -> Option<f64> {
    grow_ms(current, slack_ms, MAX_LAG_MS)
}

/// A follower's lateness from `current` (ms) when its source slack (a lower
/// bound at the clocks' worst phase) is `slack_ms`: it steps back by the
/// whole milliseconds over 2 ms of it, so it never runs short and the
/// growth rule's 1 ms never comes back; all the way when it can (aligned
/// again), else only by [`SHRINK_MIN_MS`] or more (each step is a fade: not
/// one per millisecond the bound improves by as the phase drifts). `None`:
/// leave it.
pub fn shrunk_lag_ms(current: f64, slack_ms: f64) -> Option<f64> {
    if current.is_nan() || current <= 0.0 || !slack_ms.is_finite() {
        return None;
    }
    let spare = (slack_ms - 2.0).floor();
    if spare >= current {
        Some(0.0)
    } else if spare >= SHRINK_MIN_MS {
        Some(current - spare)
    } else {
        None
    }
}

/// The least step back of a late follower that does not reach aligned, ms:
/// more than [`expected_need_ms`] and its margin can be over what it
/// measures (about 20 ms at 2048-frame periods in the sim), so the
/// lateness a follower starts at is not stepped back from in play (a
/// fade) for a few milliseconds of latency.
pub const SHRINK_MIN_MS: f64 = 30.0;

fn grow_ms(current: f64, slack_ms: f64, cap: f64) -> Option<f64> {
    if !slack_ms.is_finite() || slack_ms >= 1.0 {
        return None;
    }
    let want = (current + (2.0 - slack_ms).ceil()).clamp(0.0, cap);
    (want > current).then_some(want)
}

/// The alignment a shrink leaves for followers that need `need_ms` (the
/// most of them): the need plus [`SHRINK_MARGIN_MS`], whole ms, within the
/// cap. `None`: the need is not known.
pub fn shrunk_alignment_ms(need_ms: f64) -> Option<f64> {
    need_ms.is_finite().then(|| {
        (need_ms + SHRINK_MARGIN_MS)
            .ceil()
            .clamp(0.0, super::tee::MAX_ALIGN_MS)
    })
}

/// What a shrink of the alignment leaves the followers at their worst
/// beyond what they needed, ms: a millisecond more than the growth rule's
/// 2 ms, so a shrink is never followed by a growth.
pub const SHRINK_MARGIN_MS: f64 = 3.0;

/// What a follower needs ahead beyond its queue and a wakeup, ms: its
/// fade, its stage's reach, the USB packets and the readings' spread (the
/// sim's measured need at 128- to 1024-frame periods and ±100 ppm, 2 ms
/// under this at the most).
pub const NEED_FIXED_MS: f64 = 13.0;

/// The alignment a follower is expected to need before it is measured (ms;
/// negative: none, by that much): its queue and a wakeup
/// (`buffer_periods` + 1 periods of `period_ms`) and [`NEED_FIXED_MS`],
/// less the least source the master holds ahead of its DAC
/// (`master_lead_ms`, [`master_lead_ms`]). 2 to 15 ms above what the sim
/// measures at 128- to 1024-frame periods.
pub fn expected_need_ms(period_ms: f64, buffer_periods: u32, master_lead_ms: f64) -> f64 {
    f64::from(buffer_periods + 1) * period_ms + NEED_FIXED_MS - master_lead_ms
}

/// A follower not placed yet: neither heard nor locked (its lateness can
/// move without a step heard).
pub fn unplaced(s: &FollowerSnapshot) -> bool {
    !s.audible && s.state != FollowerState::Locked
}

/// The lateness a follower expected to need `expected_ms` of the alignment
/// starts at (ms; `None`: none): what the alignment at its cap leaves of it
/// and [`SHRINK_MARGIN_MS`], whole ms, within [`MAX_LAG_MS`].
pub fn floor_lag_ms(expected_ms: f64) -> Option<f64> {
    let cap = super::tee::MAX_ALIGN_MS;
    let lag = ((expected_ms + SHRINK_MARGIN_MS).ceil() - cap).min(MAX_LAG_MS);
    (lag > 0.0).then_some(lag)
}

/// The least source the master holds ahead of its DAC, ms, for a queue of
/// `periods` periods of `quantum_ms`: one period fewer just before it
/// wakes, less half a quantum for a late graph cycle.
pub fn master_lead_ms(quantum_ms: f64, periods: u32) -> f64 {
    (f64::from(periods) - 1.5).max(0.0) * quantum_ms
}

/// Paces [`next_alignment_ms`]: once a second (the slack's window), and
/// not for 3 s after a change (the change reaches both DACs and the
/// window refills). It also keeps what the followers need
/// ([`Self::note_need`]) and, when that is less than the alignment, the
/// shrink that waits for silence ([`Self::shrink`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AlignControl {
    /// The alignment applied, ms.
    pub ms: f64,
    next_s: f64,
    /// When it last changed.
    changed_s: f64,
    /// What the followers need, ms (`NaN`: not known): the most the
    /// alignment less their least worst slack
    /// ([`FollowerSnapshot::slack_low_ms`]) has been since their paths last
    /// changed ([`Self::forget_need`]): a high-water mark, kept through
    /// pauses, xruns, speed and chain swaps (each restarts a follower's
    /// worst slack, and the next window alone would undercut a stall seen
    /// once before it).
    pub need_ms: f64,
    /// Seconds of play the need has been measured over (a shrink waits for
    /// [`SHRINK_MEASURED_S`]), and the last measurement.
    pub measured_s: f64,
    last_need_s: f64,
    /// The shrink sent, waiting for silence, ms.
    pub pending: Option<f64>,
}

/// The play a need is measured over before a shrink goes to it, seconds.
pub const SHRINK_MEASURED_S: f64 = 30.0;

impl Default for AlignControl {
    fn default() -> Self {
        Self {
            ms: 0.0,
            next_s: 0.0,
            changed_s: 0.0,
            need_ms: f64::NAN,
            measured_s: 0.0,
            last_need_s: f64::NAN,
            pending: None,
        }
    }
}

impl AlignControl {
    /// At `now_s` (any clock in seconds), with the followers' least slack:
    /// the new alignment, when it changes.
    pub fn tick(&mut self, now_s: f64, least_slack_ms: f64) -> Option<f64> {
        // (Nothing measured yet: the first window's slack is looked at as
        // soon as it is out.)
        if now_s < self.next_s || !least_slack_ms.is_finite() {
            return None;
        }
        self.next_s = now_s + 1.0;
        let want = next_alignment_ms(self.ms, least_slack_ms)?;
        self.next_s = now_s + 3.0;
        self.changed_s = now_s;
        self.ms = want;
        // (Applied at once, it drops a shrink waiting.)
        self.pending = None;
        Some(want)
    }

    /// What the followers need now (ms, the most of them; `None`: not known
    /// now, a follower is not measured, the master does not play): kept
    /// once the last change has had time to reach their slack, and through
    /// a pause, until a shrink lands or [`Self::forget_need`].
    pub fn note_need(&mut self, now_s: f64, need_ms: Option<f64>) {
        let measured = now_s >= self.changed_s + 3.0;
        match need_ms.filter(|n| n.is_finite() && measured) {
            Some(n) => {
                self.need_ms = if self.need_ms.is_nan() {
                    n
                } else {
                    self.need_ms.max(n)
                };
                // (Ticks a second apart; a gap is a pause, not play.)
                if self.last_need_s.is_finite() {
                    self.measured_s += (now_s - self.last_need_s).clamp(0.0, 2.0);
                }
                self.last_need_s = now_s;
            }
            None => self.last_need_s = f64::NAN,
        }
    }

    /// While the need is not measured (a fresh pair, new paths): the
    /// alignment the followers are expected to need (`expected_ms`,
    /// [`expected_need_ms`]) plus [`SHRINK_MARGIN_MS`], when that is more
    /// than it has, so a follower is placed with the source it needs and
    /// never grows into it while heard. Only up: the shrink takes it down
    /// to the measured need in silence.
    pub fn floor(&mut self, now_s: f64, expected_ms: Option<f64>) -> Option<f64> {
        if !self.need_ms.is_nan() {
            return None;
        }
        let want = shrunk_alignment_ms(expected_ms?)?;
        if want <= self.ms {
            return None;
        }
        self.ms = want;
        self.changed_s = now_s;
        self.next_s = now_s + 3.0;
        self.pending = None;
        Some(want)
    }

    /// The followers' paths changed (one added or removed, a chain, a
    /// period, the quantum): what they needed is not known any more.
    pub fn forget_need(&mut self) {
        self.need_ms = f64::NAN;
        self.measured_s = 0.0;
        self.last_need_s = f64::NAN;
    }

    /// The shrink to send when it changes (`Some((ms, quiet))`): to what the
    /// followers need plus [`SHRINK_MARGIN_MS`], at the next silence
    /// (`quiet`), when that is at least a millisecond under the alignment;
    /// or the alignment itself, at once (not `quiet`), to drop a shrink
    /// waiting that the need no longer allows. `allowed`: no follower plays
    /// late (the alignment is at its cap then).
    pub fn shrink(&mut self, now_s: f64, allowed: bool) -> Option<(f64, bool)> {
        let target = shrunk_alignment_ms(self.need_ms).filter(|t| {
            allowed
                && self.measured_s >= SHRINK_MEASURED_S
                && now_s >= self.changed_s + 3.0
                && *t + 1.0 <= self.ms
        });
        match (target, self.pending) {
            (Some(t), p) if p != Some(t) => {
                self.pending = Some(t);
                Some((t, true))
            }
            (None, Some(_)) => {
                self.pending = None;
                Some((self.ms, false))
            }
            _ => None,
        }
    }

    /// The alignment the RT side applies is `applied_ms`: a shrink that
    /// waited has landed (true). The followers re-measure from there.
    pub fn landed(&mut self, now_s: f64, applied_ms: f64) -> bool {
        if self.pending != Some(applied_ms) {
            return false;
        }
        self.pending = None;
        self.ms = applied_ms;
        self.changed_s = now_s;
        self.next_s = now_s + 3.0;
        // (The need is the alignment less the slack: the step moves both,
        // so the high-water mark holds.)
        true
    }

    /// At its cap, and the last change has had time to reach the
    /// followers' slack (what a follower still lacks is real).
    pub fn settled_at_cap(&self, now_s: f64) -> bool {
        self.ms >= super::tee::MAX_ALIGN_MS && now_s >= self.changed_s + 3.0
    }

    /// No follower: no alignment.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Start from the alignment the master had with its followers before
    /// (a restart): applied at once, grown from there as usual.
    pub fn start_at(&mut self, now_s: f64, ms: f64) -> Option<f64> {
        if self.ms > 0.0 || !ms.is_finite() || ms <= 0.0 {
            return None;
        }
        self.ms = ms.min(super::tee::MAX_ALIGN_MS);
        self.changed_s = now_s;
        self.next_s = now_s + 3.0;
        Some(self.ms)
    }

    /// Why the alignment may still move at `now_s` with the followers'
    /// least slack (`None`: it holds): it changed less than 3 s ago (the
    /// followers re-acquire and the slack window refills), or a follower
    /// is short of source and the next tick grows it.
    pub fn settling(&self, now_s: f64, least_slack_ms: f64) -> Option<String> {
        if self.ms > 0.0 && now_s < self.changed_s + 3.0 {
            return Some(format!(
                "the master's alignment changed {:.1} s ago",
                (now_s - self.changed_s).max(0.0)
            ));
        }
        next_alignment_ms(self.ms, least_slack_ms)
            .map(|ms| format!("a follower is short of source: the alignment grows to {ms} ms"))
    }
}

/// Paces [`next_lag_ms`] and [`shrunk_lag_ms`] for one follower as
/// [`AlignControl`] paces the alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LagControl {
    next_s: f64,
}

impl LagControl {
    /// At `now_s`: the follower's new lateness (set on `c`), when it
    /// changes. It grows only with the master's alignment at its cap
    /// (`at_cap`), and steps back whenever the follower has the source to.
    pub fn tick(
        &mut self,
        now_s: f64,
        s: &FollowerSnapshot,
        c: &FollowerControls,
        at_cap: bool,
    ) -> Option<f64> {
        if now_s < self.next_s {
            return None;
        }
        self.next_s = now_s + 1.0;
        if !matches!(s.state, FollowerState::Acquiring | FollowerState::Locked) {
            return None;
        }
        let grown = if at_cap {
            next_lag_ms(c.lag_ms(), s.slack_ms)
        } else {
            None
        };
        let lag = grown.or_else(|| shrunk_lag_ms(c.lag_ms(), s.slack_ms))?;
        c.set_lag_ms(lag);
        self.next_s = now_s + 3.0;
        Some(lag)
    }
}

/// The master's command lead one follower needs (ms), when it plays: its
/// measured lead plus [`LEAD_MARGIN_MS`], or [`DEFAULT_LEAD_MS`] before it
/// has measured one.
pub fn lead_needed_ms(s: &FollowerSnapshot) -> Option<f64> {
    if !matches!(s.state, FollowerState::Acquiring | FollowerState::Locked) {
        return None;
    }
    let ms = if s.lead_ms.is_finite() {
        (s.lead_ms.max(0.0) + LEAD_MARGIN_MS).ceil()
    } else {
        DEFAULT_LEAD_MS
    };
    Some(ms.min(super::tee::MAX_LEAD_MS))
}

/// The config follower `device`'s chain is built from: `master` with the
/// follower's own chain patch, without A/B.
fn own_config(master: &EngineConfig, device: &str) -> EngineConfig {
    match master.outputs.followers.iter().find(|f| f.device == device) {
        Some(f) => crate::control::follower_chain::config(master, f),
        None => {
            let mut cfg = master.clone();
            cfg.ab = None;
            cfg.output_channels = None;
            cfg
        }
    }
}

fn has_patch(master: &EngineConfig, device: &str) -> bool {
    master
        .outputs
        .followers
        .iter()
        .any(|f| f.device == device && f.chain_patch.is_some())
}

/// The compensation the master needs for `followers` (state, chain
/// latency in graph frames): how much longer the longest playing one's
/// chain is than the master's (`master_chain_graph`), never below 0.
/// `None` (hold what applies) while the master is not mapped or a follower
/// is opening (it may play a longer chain in a moment); a lost or stopped
/// one needs nothing, so with none playing the master settles to 0.
pub fn comp_need_of(
    followers: impl IntoIterator<Item = (FollowerState, f64)>,
    master_chain_graph: f64,
) -> Option<f64> {
    if !master_chain_graph.is_finite() {
        return None;
    }
    let mut need = 0.0f64;
    for (state, chain) in followers {
        match state {
            FollowerState::Opening => return None,
            FollowerState::Acquiring | FollowerState::Locked if chain.is_finite() => {
                need = need.max(chain - master_chain_graph);
            }
            _ => {}
        }
    }
    Some(need)
}

/// What followers in `snaps` need of the master's alignment `align_ms`
/// (ms of source at the graph rate, as the alignment): the most of
/// [`need_ms_of`] over those that play. `None` while one that plays has not
/// measured it at 1× or faster, or one is opening.
pub fn need_of(snaps: impl IntoIterator<Item = FollowerSnapshot>, align_ms: f64) -> Option<f64> {
    let mut need: Option<f64> = None;
    for s in snaps {
        match s.state {
            FollowerState::Opening => return None,
            FollowerState::Acquiring | FollowerState::Locked => {
                let n = need_ms_of(&s, align_ms)?;
                need = Some(need.map_or(n, |m: f64| m.max(n)));
            }
            _ => {}
        }
    }
    need
}

/// What one follower needs of the alignment `align_ms`, in the
/// alignment's unit (ms of source at the graph rate): the alignment less
/// its worst slack in source (its playback ms times the speed it was
/// measured at). `None` before it is measured, or when it was measured
/// slower than 1×: a playback queue holds fewer source frames then, so the
/// need comes out short of what 1× needs (a shrink to it would starve on a
/// resume at 1×). Faster speeds count (they need more).
pub fn need_ms_of(s: &FollowerSnapshot, align_ms: f64) -> Option<f64> {
    let v = s.slack_speed;
    (s.slack_low_ms.is_finite() && v.is_finite() && v >= 1.0 - 1e-9)
        .then_some(align_ms - s.slack_low_ms * v)
}

/// Paces the master's chain compensation ([`TeeHost::set_comp_frames`]):
/// what the followers that play need (the longest chain's lead over the
/// master's, graph frames) applies once it has held still for
/// [`COMP_SETTLE_S`] (a chain edit swaps the devices a little apart).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CompControl {
    /// The compensation applied, graph frames.
    pub frames: usize,
    want: usize,
    since_s: f64,
}

impl CompControl {
    /// At `now_s`, with `need` (graph frames, `None`: nothing known): the
    /// new compensation when it changes, within `cap`.
    pub fn tick(&mut self, now_s: f64, need: Option<f64>, cap: usize) -> Option<usize> {
        let want = need.map_or(self.frames, |n| (n.max(0.0).ceil() as usize).min(cap));
        if want != self.want {
            self.want = want;
            self.since_s = now_s;
        }
        if want == self.frames || now_s - self.since_s < COMP_SETTLE_S {
            return None;
        }
        self.frames = want;
        Some(want)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether the compensation moves for `need` (as [`Self::tick`] would
    /// take it, within `cap`).
    pub fn settling(&self, need: Option<f64>, cap: usize) -> bool {
        let want = need.map_or(self.frames, |n| (n.max(0.0).ceil() as usize).min(cap));
        want != self.frames || self.want != self.frames
    }
}

/// Every follower's thread, keyed by device (the control side; the tee's
/// slots stay in [`TeeHost`]).
pub struct FollowerHost {
    by_device: BTreeMap<String, FollowerHandle>,
    /// Configured followers that could not start, with why.
    failed: BTreeMap<String, String>,
    build: ChainBuilder,
    desired_ratio: RatioFn,
    /// What the loops learnt per master and follower pair
    /// ([`super::clocks`]), and the master they play with now.
    clocks: super::clocks::ClockStore,
    master_device: String,
}

impl FollowerHost {
    pub fn new(build: ChainBuilder, desired_ratio: RatioFn) -> Self {
        Self {
            by_device: BTreeMap::new(),
            failed: BTreeMap::new(),
            build,
            desired_ratio,
            clocks: super::clocks::ClockStore::in_memory(),
            master_device: String::new(),
        }
    }

    /// Keep the pairs' clocks in `store` (on disk).
    pub fn with_clock_store(mut self, store: super::clocks::ClockStore) -> Self {
        self.clocks = store;
        self
    }

    /// What follower `device`'s loop knows of its pair now, into the store.
    fn harvest(&mut self, device: &str) {
        if let Some(h) = self.by_device.get(device) {
            let mem = [h.controls.memory_at(0), h.controls.memory_at(1)];
            self.clocks.note(&self.master_device, device, mem);
        }
    }

    /// The master's alignment with its followers last time (ms), if known.
    pub fn remembered_alignment(&self) -> Option<f64> {
        self.clocks.alignment(&self.master_device)
    }

    /// The master's alignment now (ms), for the next start.
    pub fn note_alignment(&mut self, ms: f64) {
        let m = self.master_device.clone();
        self.clocks.note_alignment(&m, ms);
    }

    /// The followers' loops into the store, written when it moved (at most
    /// every [`super::clocks::SAVE_EVERY_S`]).
    pub fn tick_clocks(&mut self) {
        let devices: Vec<String> = self.by_device.keys().cloned().collect();
        for d in &devices {
            self.harvest(d);
        }
        self.clocks.save(false);
    }

    /// The master plays `master_device` now: each follower's loop starts
    /// from what is known of its new pair (a clock ratio is a pair's).
    fn set_master(&mut self, master_device: &str) {
        if master_device == self.master_device {
            return;
        }
        let devices: Vec<String> = self.by_device.keys().cloned().collect();
        for d in &devices {
            self.harvest(d);
        }
        self.clocks.save(true);
        self.master_device = master_device.to_string();
        for (d, h) in &self.by_device {
            h.controls
                .install_memory(self.clocks.get(&self.master_device, d));
        }
    }

    pub fn len(&self) -> usize {
        self.by_device.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_device.is_empty()
    }

    /// Add a follower on `device` (not `master_device`, at most
    /// [`MAX_FOLLOWERS`]): its tee slot, filled from `master`, and its
    /// thread.
    pub fn add(
        &mut self,
        tee: &mut TeeHost,
        device: &str,
        master_device: &str,
        master: &EngineConfig,
        opts: FollowerOptions,
    ) -> Result<(), String> {
        if device.is_empty() || device == master_device {
            return Err(format!("{device}: not a follower (the master plays it)"));
        }
        if !opts.trim_ms.is_finite() || opts.trim_ms.abs() > MAX_TRIM_MS {
            return Err(format!("trim {} ms outside ±{MAX_TRIM_MS}", opts.trim_ms));
        }
        if self.by_device.len() >= MAX_FOLLOWERS {
            return Err(format!("at most {MAX_FOLLOWERS} followers"));
        }
        self.set_master(master_device);
        tee.add_follower(device, master)?;
        let reader = tee
            .slots_mut()
            .get_mut(device)
            .and_then(|s| s.reader.take())
            .ok_or_else(|| format!("{device}: no tee ring"))?;
        // Its own chain (chain A without a patch); A/B stays on the master.
        let config = Arc::new(Mutex::new(own_config(master, device)));
        let opened = Arc::new(Mutex::new(None));
        let (swap_host, swaps) = chain_swaps();
        let status = Arc::new(FollowerStatus::default());
        let controls = Arc::new(FollowerControls::new(opts.trim_ms, opts.volume_db));
        controls.set_level(opts.volume_db, opts.follow_offset_db);
        controls.set_compensate(true);
        controls.install_memory(self.clocks.get(master_device, device));
        controls.set_period_ms(opts.period_ms);
        let args = ThreadArgs {
            device: device.to_string(),
            reader,
            shared: tee.shared().clone(),
            config: Arc::clone(&config),
            build: Arc::clone(&self.build),
            opts,
            status: Arc::clone(&status),
            controls: Arc::clone(&controls),
            desired_ratio: Arc::clone(&self.desired_ratio),
            opened: Arc::clone(&opened),
            swaps,
        };
        let thread = std::thread::Builder::new()
            .name(format!("audivane-follower-{}", self.by_device.len()))
            .spawn(move || follower_thread(args));
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                let _ = tee.remove_follower(device);
                return Err(format!("follower thread: {e}"));
            }
        };
        self.by_device.insert(
            device.to_string(),
            FollowerHandle {
                controls,
                status,
                config,
                opened,
                swaps: swap_host,
                thread: Some(thread),
                lag: LagControl::default(),
                patched: has_patch(master, device),
            },
        );
        Ok(())
    }

    /// The devices followers play, in order.
    pub fn devices(&self) -> Vec<String> {
        self.by_device.keys().cloned().collect()
    }

    /// A follower plays `device`.
    pub fn contains(&self, device: &str) -> bool {
        self.by_device.contains_key(device)
    }

    /// Stop and remove the follower on `device` (waits for its thread).
    pub fn remove(&mut self, tee: &mut TeeHost, device: &str) -> Result<(), String> {
        self.harvest(device);
        self.clocks.save(true);
        let Some(mut h) = self.by_device.remove(device) else {
            return Ok(());
        };
        h.stop();
        tee.remove_follower(device)
    }

    /// Stop every follower (the engine stops).
    pub fn stop_all(&mut self, tee: &mut TeeHost) {
        let devices: Vec<String> = self.by_device.keys().cloned().collect();
        for d in devices {
            let _ = self.remove(tee, &d);
        }
    }

    /// Make the running followers what `master.outputs` lists: stop
    /// the ones it no longer lists, start the new ones, and set the
    /// controls of the others (a trim that moves a late follower earlier
    /// goes into its lateness, [`FollowerControls::retrim`]).
    pub fn sync(&mut self, tee: &mut TeeHost, master_device: &str, master: &EngineConfig) {
        self.set_master(master_device);
        let want = &master.outputs.followers;
        let listed = |d: &str| want.iter().any(|f| f.device == d);
        let gone: Vec<String> = self
            .by_device
            .keys()
            .filter(|d| !listed(d))
            .cloned()
            .collect();
        for d in gone {
            log::info!("followers: {d} removed");
            if let Err(e) = self.remove(tee, &d) {
                log::warn!("followers: {d} not removed: {e}");
            }
        }
        self.failed.retain(|d, _| listed(d));
        for f in want {
            if let Some(c) = self.controls(&f.device) {
                if c.trim_ms() != f.trim_ms {
                    c.retrim(f.trim_ms);
                }
                c.set_level(f.volume_db, follow_offset_db(f, master));
                c.set_enabled(f.enabled);
                if c.set_period_ms(f.period_ms) {
                    log::info!("follower {}: its period changed, it reopens", f.device);
                    c.request_reopen();
                }
                continue;
            }
            let opts = FollowerOptions {
                trim_ms: f.trim_ms,
                volume_db: f.volume_db,
                follow_offset_db: follow_offset_db(f, master),
                period_ms: f.period_ms,
                ..FollowerOptions::default()
            };
            match self.add(tee, &f.device, master_device, master, opts) {
                Ok(()) => {
                    log::info!("followers: {} added", f.device);
                    self.failed.remove(&f.device);
                    if let Some(c) = self.controls(&f.device) {
                        c.set_enabled(f.enabled);
                    }
                }
                Err(e) => {
                    log::warn!("followers: {} not started: {e}", f.device);
                    self.failed.insert(f.device.clone(), e);
                }
            }
        }
    }

    /// Why a configured follower is not running, if it could not start.
    pub fn failure(&self, device: &str) -> Option<&str> {
        self.failed.get(device).map(String::as_str)
    }

    /// The master's alignment is at its cap (`at_cap`): each follower that
    /// still runs out of source plays later by what it lacks, paced as
    /// [`AlignControl`]: once a second, not for 3 s
    /// after a change. A follower that plays late and has more source than
    /// it needs steps back, to aligned once nothing needs the delay.
    pub fn tick_lag(&mut self, now_s: f64, at_cap: bool) {
        for (device, h) in &mut self.by_device {
            let before = h.controls.lag_ms();
            match h.lag.tick(now_s, &h.status.snapshot(), &h.controls, at_cap) {
                Some(lag) if lag > before => log::info!(
                    "followers: {device} needs more than the master's alignment, plays {lag} ms late"
                ),
                Some(0.0) => log::info!("followers: {device} plays aligned again"),
                Some(lag) => log::info!("followers: {device} needs less, plays {lag} ms late"),
                None => {}
            }
        }
    }

    pub fn controls(&self, device: &str) -> Option<&FollowerControls> {
        self.by_device.get(device).map(|h| &*h.controls)
    }

    /// The config follower `device`'s chain is built from now.
    pub fn config_of(&self, device: &str) -> Option<EngineConfig> {
        let h = self.by_device.get(device)?;
        h.config.lock().ok().map(|c| c.clone())
    }

    /// Free what every follower's thread handed back from measurement
    /// posts.
    pub fn reclaim_measure(&self) {
        for h in self.by_device.values() {
            h.controls.measure_slot().reclaim();
        }
    }

    /// The master's config changed: every follower's slot follows, with
    /// its own chain patch; an open follower whose chain changed gets one
    /// built for its device (swapped in on its thread), or reopens when its
    /// own chain asks for another ratio.
    pub fn config_changed(&mut self, master: &EngineConfig) {
        for (device, h) in &mut self.by_device {
            let cfg = own_config(master, device);
            h.reap();
            let old = match h.config.lock() {
                Ok(mut c) => Some(std::mem::replace(&mut *c, cfg.clone())),
                Err(_) => None,
            };
            // Another output pair: its device opens again, at the channel
            // count the pair needs.
            let configured = master.outputs.followers.iter().any(|f| f.device == *device);
            if configured
                && old
                    .as_ref()
                    .is_some_and(|o| o.output_channels != cfg.output_channels)
            {
                log::info!("follower {device}: its output channels changed, it reopens");
                h.patched = has_patch(master, device);
                h.controls.request_reopen();
                continue;
            }
            let Some(old) = old.filter(|c| chain_inputs_differ(c, &cfg)) else {
                h.patched = has_patch(master, device);
                continue;
            };
            let opened = h.opened.lock().ok().and_then(|g| *g);
            let patched = std::mem::replace(&mut h.patched, has_patch(master, device));
            let Some((rate, ratio, format)) = opened else {
                continue;
            };
            // A patch that sets (or set, until this edit) its own upsampler
            // target: its device opens at its own ratio, when the ratio it
            // asks for changes. (What it asks for and what its device gave
            // may differ: the device stepped down, and keeps that.) Without
            // a patch, a follower keeps the ratio it opened at.
            if (patched || h.patched)
                && (self.desired_ratio)(&old, rate) != (self.desired_ratio)(&cfg, rate)
            {
                // Its own upsampler target: a device open at another rate.
                log::info!("follower {device}: its chain asks for another ratio, it reopens");
                h.controls.request_reopen();
                continue;
            }
            // Built here, off its audio thread; it swaps in there. A
            // follower that reopens meanwhile builds from the slot anyway.
            match (self.build)(&cfg, rate, ratio, format) {
                Ok(chain) => h.swaps.offer(chain),
                Err(e) => log::warn!("follower {device}: chain not rebuilt: {e}"),
            }
        }
    }

    /// Rebuild any open follower whose opened rate's upsampler substitute
    /// changed in the per-rate plan.
    pub fn rebuild_for_rate_plan(
        &mut self,
        master: &EngineConfig,
        old: &crate::api::RatePlan,
        new: &crate::api::RatePlan,
    ) {
        for (device, h) in &mut self.by_device {
            let cfg = own_config(master, device);
            h.reap();
            let Some((rate, ratio, format)) = h.opened.lock().ok().and_then(|g| *g) else {
                continue;
            };
            if old.substitute(&cfg, rate) == new.substitute(&cfg, rate) {
                continue;
            }
            match (self.build)(&cfg, rate, ratio, format) {
                Ok(chain) => h.swaps.offer(chain),
                Err(e) => log::warn!("follower {device}: chain not rebuilt for rate plan: {e}"),
            }
        }
    }

    /// Drop the chains the followers handed back.
    pub fn reap(&mut self) {
        for h in self.by_device.values_mut() {
            h.reap();
        }
    }

    pub fn status(&self) -> Vec<(String, FollowerSnapshot)> {
        self.by_device
            .iter()
            .map(|(d, h)| (d.clone(), h.status.snapshot()))
            .collect()
    }

    /// The master's command lead (ms) for the followers that play: the
    /// largest they measured plus [`LEAD_MARGIN_MS`], or [`DEFAULT_LEAD_MS`]
    /// while one has not measured yet; 0 without followers.
    pub fn command_lead_ms(&self) -> f64 {
        self.by_device
            .values()
            .filter_map(|h| lead_needed_ms(&h.status.snapshot()))
            .fold(0.0, f64::max)
    }

    /// How much longer the longest playing follower's chain is than the
    /// master's (`master_chain_graph`), graph frames: [`comp_need_of`].
    pub fn comp_need(&self, master_chain_graph: f64) -> Option<f64> {
        comp_need_of(
            self.by_device.values().map(|h| {
                let s = h.status.snapshot();
                (s.state, s.chain_graph)
            }),
            master_chain_graph,
        )
    }

    /// The first follower opening or locking (its device and state text),
    /// whatever its `enabled`: its slack and chain still move the master's
    /// alignment and compensation.
    pub fn first_unsettled(&self) -> Option<(String, &'static str)> {
        self.by_device
            .iter()
            .find_map(|(d, h)| match h.status.snapshot().state {
                FollowerState::Opening => Some((d.clone(), "opening")),
                FollowerState::Acquiring => Some((d.clone(), "locking")),
                _ => None,
            })
    }

    /// What the followers need of the master's alignment `align_ms` now
    /// (ms, the most of them, [`need_of`]); `None` unless every follower
    /// that plays has measured its worst slack, and none is opening.
    pub fn alignment_need_ms(&self, align_ms: f64) -> Option<f64> {
        need_of(
            self.by_device.values().map(|h| h.status.snapshot()),
            align_ms,
        )
    }

    /// A follower plays later than aligned (its lateness, at the
    /// alignment's cap).
    pub fn any_late(&self) -> bool {
        self.by_device.values().any(|h| h.controls.lag_ms() > 0.0)
    }

    /// What the enabled followers are expected to need of the alignment
    /// (ms, the most of them, [`expected_need_ms`]; `None`: none), with the
    /// master's queue `master_lead_ms` ahead: from the period its device
    /// opened at, else the one it asks for at `graph_rate`.
    pub fn expected_need_ms(
        &self,
        master: &EngineConfig,
        graph_rate: u32,
        master_lead_ms: f64,
    ) -> Option<f64> {
        self.by_device
            .values()
            .filter(|h| h.controls.enabled())
            .filter_map(|h| self.expected_of(h, master, graph_rate, master_lead_ms))
            .reduce(f64::max)
    }

    fn expected_of(
        &self,
        h: &FollowerHandle,
        master: &EngineConfig,
        graph_rate: u32,
        master_lead_ms: f64,
    ) -> Option<f64> {
        let s = h.status.snapshot();
        let ms = if s.period > 0 && s.device_rate > 0 {
            f64::from(s.period) * 1e3 / f64::from(s.device_rate)
        } else if graph_rate > 0 {
            let ratio = (self.desired_ratio)(master, graph_rate);
            let graph = period_graph_for(h.controls.period_ms(), graph_rate, ratio);
            f64::from(graph) * 1e3 / f64::from(graph_rate)
        } else {
            return None;
        };
        let periods = FollowerOptions::default().buffer_periods;
        Some(expected_need_ms(ms, periods, master_lead_ms))
    }

    /// Each enabled follower not placed yet (a new stream starts it
    /// aligned: an open, a reopen, a rate change) and expected to need more
    /// than the alignment's cap plays late by the rest from the start
    /// ([`floor_lag_ms`]), so it is placed there, not grown into it while
    /// heard. Only up, and not once heard (a step back in play stays).
    pub fn floor_lag(&self, master: &EngineConfig, graph_rate: u32, master_lead_ms: f64) {
        for (device, h) in &self.by_device {
            let s = h.status.snapshot();
            if !h.controls.enabled() || !unplaced(&s) {
                continue;
            }
            let Some(lag) = self
                .expected_of(h, master, graph_rate, master_lead_ms)
                .and_then(floor_lag_ms)
            else {
                continue;
            };
            if lag > h.controls.lag_ms() {
                h.controls.set_lag_ms(lag);
                log::info!(
                    "followers: {device} is expected to need more than the master's alignment, plays {lag} ms late"
                );
            }
        }
    }

    /// The least source slack over the followers that play (ms, `NaN`
    /// without one), for [`AlignControl::tick`].
    pub fn least_slack_ms(&self) -> f64 {
        self.by_device
            .values()
            .map(|h| h.status.snapshot())
            .filter(|s| matches!(s.state, FollowerState::Acquiring | FollowerState::Locked))
            .map(|s| s.slack_ms)
            .filter(|s| s.is_finite())
            .fold(f64::NAN, f64::min)
    }
}

impl Drop for FollowerHost {
    fn drop(&mut self) {
        let devices: Vec<String> = self.by_device.keys().cloned().collect();
        for d in &devices {
            self.harvest(d);
        }
        self.clocks.save(true);
        for h in self.by_device.values_mut() {
            h.stop();
        }
    }
}

#[cfg(test)]
#[path = "follower_tests.rs"]
mod tests;
