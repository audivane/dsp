//! Multi-device step 4: the follower output. Each follower DAC plays the
//! master's source on its
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
//!   lost and reopens it once the device is back, with a fresh acquisition.
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

use super::processor::{ActiveChain, DspIn, MAX_METER_CHANNELS};
use super::tee::{
    LogRead, LoggedCommand, MAX_FOLLOWERS, MAX_GRAPH_RATE, TeeHost, TeeReader, TeeShared,
    VsCommand, step_frames,
};
use audivane_dsp_core::drift::{
    DriftLoop, EPS_MAX, FRAME, LoopConfig, PhaseDetector, PositionMap, RateEstimator, Reading,
    hermite,
};
use audivane_dsp_core::gain::StereoGainRamp;
use audivane_dsp_core::varispeed::{MAX_SPEED, Varispeed, VarispeedSpec};
use audivane_protocol::ipc::{self, EngineConfig, FollowerConfig};
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
/// The window the source slack is the minimum over, seconds.
const SLACK_WINDOW_S: f64 = 1.0;
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
    lead_ms: AtomicU64,
    device_rate: AtomicU64,
    period: AtomicU64,
    reopens: AtomicU64,
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
    /// How far its stage ran ahead of the master's over the last second,
    /// ms of playback (the most): the master holds speed commands back by
    /// more, so the follower reads each before it gets there. `NaN` before
    /// any.
    pub lead_ms: f64,
    pub device_rate: u32,
    pub period: u32,
    pub reopens: u64,
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
            lead_ms: unbits(self.lead_ms.load(r)),
            device_rate: self.device_rate.load(r) as u32,
            period: self.period.load(r) as u32,
            reopens: self.reopens.load(r),
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
) -> ipc::FollowerStatus {
    use ipc::FollowerState as W;
    let state = match snap {
        None => W::Off,
        Some(s) => match s.state {
            FollowerState::Off => W::Off,
            FollowerState::Opening => W::Opening,
            FollowerState::Acquiring => W::Locking,
            FollowerState::Lost => W::Lost,
            FollowerState::Locked if !s.audible => W::Muted,
            FollowerState::Locked if lag_ms > 0.0 => W::Unaligned,
            FollowerState::Locked => W::Locked,
        },
    };
    let finite = |x: f64| if x.is_finite() { x } else { 0.0 };
    ipc::FollowerStatus {
        device: f.device.clone(),
        enabled: f.enabled,
        trim_ms: f.trim_ms,
        volume_db: f.volume_db,
        state,
        offset_ms: lag_ms,
        ppm: snap.map_or(0.0, |s| finite(s.ppm)),
        phase_error_frames: snap.map_or(0.0, |s| finite(s.phase_error_frames)),
        bandwidth_hz: snap.map_or(0.0, |s| finite(s.bandwidth_hz)),
        skips: snap.map_or(0, |s| s.skips),
        xruns: snap.map_or(0, |s| s.xruns),
        latency_ms: snap.map_or(0.0, |s| finite(s.latency_ms)),
        device_rate: snap.map_or(0, |s| s.device_rate),
        period: snap.map_or(0, |s| s.period),
        error: snap
            .and_then(|s| s.error.clone())
            .or_else(|| failure.map(str::to_string)),
    }
}

/// What the control side sets on a running follower.
#[derive(Debug)]
pub struct FollowerControls {
    trim_ms: AtomicU64,
    /// Its lateness beyond the trim when the master's alignment is at its
    /// cap (it plays unaligned), ms.
    lag_ms: AtomicU64,
    volume_db: AtomicU64,
    enabled: AtomicBool,
    stop: AtomicBool,
}

impl FollowerControls {
    pub fn new(trim_ms: f64, volume_db: f64) -> Self {
        Self {
            trim_ms: AtomicU64::new(bits(trim_ms)),
            lag_ms: AtomicU64::new(bits(0.0)),
            volume_db: AtomicU64::new(bits(volume_db)),
            enabled: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        }
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
    /// says how late it is); a later one is taken as it is.
    pub fn retrim(&self, ms: f64) {
        let before = self.trim_ms();
        self.set_trim_ms(ms);
        let earlier = before - self.trim_ms();
        let lag = self.lag_ms();
        if lag > 0.0 && earlier > 0.0 {
            self.set_lag_ms(lag + earlier);
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

    /// Its digital volume, dB (at most 0).
    pub fn set_volume_db(&self, db: f64) {
        let db = if db.is_finite() {
            db.clamp(-120.0, 0.0)
        } else {
            0.0
        };
        self.volume_db.store(bits(db), Ordering::Relaxed);
    }

    pub fn volume_db(&self) -> f64 {
        unbits(self.volume_db.load(Ordering::Relaxed))
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
}

/// A pair of chain queues.
pub fn chain_swaps() -> (ChainSwapHost, ChainSwaps) {
    let (in_tx, in_rx) = rtrb::RingBuffer::new(2);
    let (out_tx, out_rx) = rtrb::RingBuffer::new(4);
    (
        ChainSwapHost {
            incoming: in_tx,
            retired: out_rx,
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
    gain: StereoGainRamp,
    volume_db: f64,
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
    master_ramping: bool,
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
    /// age, seconds.
    slack_min: f64,
    slack_age: f64,
    slack_last: f64,
    /// The ring's known end at the last call, and the largest step it made
    /// between two calls over the window (the master's write size: the
    /// slack is counted at the worst phase of the two clocks).
    known_last: u64,
    known_jump: u64,
    /// The loop's bandwidth before its alignment (it narrows from there).
    bw0: f64,
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
    /// The master's chain latency changes seen ([`TeeShared::chain_seq`]).
    chain_seq: u64,
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
        let lp = DriftLoop::new(LoopConfig::default(), g, delay_s).map_err(|e| e.to_string())?;
        let bw0 = lp.bandwidth();
        let max_out = p.ratio.frames_out(0, STAGE_FRAMES) + p.ratio.up as usize + 1;
        let fade_len = ((FADE_MS * 1e-3 * g).round() as usize).clamp(1, MAX_FADE);
        let ch = p.channels;
        let mut rt = Self {
            reader,
            shared,
            status,
            controls,
            stage,
            chain,
            gain: StereoGainRamp::unity(p.device_rate),
            volume_db: 0.0,
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
            committed: None,
            last_cmd_at: i128::MIN,
            pending_reset: None,
            hold_until: 0,
            log_cursor: None,
            locked: false,
            master_valid: false,
            master_ramping: false,
            master_speed: 1.0,
            loop_rate: g,
            lateness_ms: f64::NAN,
            offset: 0.0,
            step_seen: 0,
            step_rest: 0.0,
            slack_min: f64::INFINITY,
            slack_age: 0.0,
            slack_last: f64::NAN,
            known_last: 0,
            known_jump: 0,
            bw0,
            starved: 0,
            lead_max: f64::NEG_INFINITY,
            lead_last: f64::NAN,
            state_at: i128::MIN,
            swaps: None,
            swap: Swap::Idle,
            swap_buf: vec![0; max_out * ch],
            retire_stash: None,
            chain_seq: 0,
            swaps_done: 0,
            p,
        };
        rt.chain_seq = rt.shared.chain_seq.load(Ordering::Relaxed);
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
    }

    fn publish_audible(&self, on: bool) {
        self.status.audible.store(on, Ordering::Relaxed);
    }

    /// The stage's next output's source position (the log's domain).
    fn here(&self) -> i128 {
        self.origin + (self.stage.core().accumulator().position() >> 1) as i128
    }

    /// The chain's delay in graph frames.
    fn chain_graph(&self) -> f64 {
        let r = self.p.ratio;
        self.chain.latency_out_frames() as f64 * f64::from(r.down) / f64::from(r.up.max(1))
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
            if self.skip_clear(k) || overdue(self.deferred_since, self.graph_out) {
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
        self.src.fill(
            &mut self.reader,
            self.want.saturating_sub(self.src.history as u64),
        );
        let known = self.src.known_end(&self.reader);
        if self.known_last > 0 && known > self.known_last {
            self.known_jump = self.known_jump.max(known - self.known_last);
        }
        self.known_last = known;
        let ahead = self.want + n_src as u64 + self.fade.len as u64;
        // Counted once the loop has aligned the read point (before, it is
        // anywhere).
        if self.lp.bandwidth() < self.bw0 && self.pending_skip == 0 && !self.deferred {
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
        let open = self.locked
            && self.master_valid
            && self.controls.enabled()
            && held == n_src
            && self.starved == 0
            && self.pending_skip == 0
            && self.pending_reset.is_none();
        self.starved = self.starved.saturating_sub(m);
        for i in 0..n_src {
            let gain = self.fade.step(open);
            let idx = self.want + i as u64;
            for (c, chan) in self.stage_in.iter_mut().enumerate().take(ch) {
                chan[i] = gain * self.src.at(c, idx);
            }
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
        self.publish_audible(open && self.fade.open());
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
        self.swap = match phase {
            Swap::Idle => match self.take_chain() {
                Some(mut next) => {
                    let lat = next.latency_out_frames();
                    next.set_limiter_audible(false);
                    Swap::Warming {
                        next,
                        warmed: 0,
                        target: if lat == 0 { 0 } else { lat + SWAP_WARM_MARGIN },
                    }
                }
                None => Swap::Idle,
            },
            Swap::Warming {
                mut next,
                mut warmed,
                target,
            } => {
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
                        let old = self.install(next);
                        Swap::Crossfade {
                            old,
                            pos: 0,
                            len: self.out_ms(SWAP_CROSSFADE_MS),
                        }
                    } else {
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
        let volume = self.controls.volume_db();
        if volume != self.volume_db {
            self.volume_db = volume;
            self.gain
                .set_volume_targets(volume, 0.0, false, false, false, true);
        }
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
        let first = self.rendered;
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
        }
    }

    /// One period's reading of the follower's device (`written` its frames
    /// written so far): the master's published position, the detector, the
    /// loop, ε for the coming period, skips and the status.
    pub fn on_reading(&mut self, reading: Reading) {
        let g = f64::from(self.p.graph_rate);
        let period_s = period_seconds(&self.p);
        let chain_seq = self.shared.chain_seq.load(Ordering::Relaxed);
        if chain_seq != self.chain_seq {
            self.chain_seq = chain_seq;
            self.hold_chain();
        }
        let point = self.shared.position.read();
        self.master_valid = point.is_some();
        if let Some(p) = point {
            self.det.master_position(p.pos, p.time_ns);
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
        let steps = self.shared.steps.load(Ordering::Relaxed) as i64;
        if steps != self.step_seen {
            let frames = step_frames(steps.wrapping_sub(self.step_seen));
            self.step_seen = steps;
            self.known_step(frames, 0.0);
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
        }
        // The reading counts when the master is valid and steady, past its
        // known steps, and the frame at the DAC was written after the last
        // skip or restart (none pending).
        let usable = self.master_valid
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
        let c = self.lp.update(e, period_s);
        self.rate_est.push(e, self.loop_rate);
        if c.skip != 0 {
            // The loop places the follower anew: no remainder carries.
            self.step_rest = 0.0;
            self.pending_skip += c.skip;
            self.rate_est.skipped(c.skip as f64);
            self.status.skips.fetch_add(1, Ordering::Relaxed);
            // A re-sync from lock (a stage change: the old read point at
            // the new speed) moves the read point: the slack measured
            // before it is not what the follower lacks, and would grow its
            // lag.
            if self.locked {
                self.slack_min = f64::INFINITY;
                self.known_jump = 0;
            }
        }
        self.locked = c.locked;
        // ε over the coming period, from where the stage is.
        let from = self.stage.epsilon();
        self.rate_est.advance(period_s, self.seg_from, self.seg_to);
        self.seg_start = self.graph_out;
        self.seg_from = from;
        self.seg_to = (c.eps, c.eps_slope);
        self.seg_h = period_s;
        // The source slack, as playback time.
        // At the worst phase of the two clocks: the window's samples are a
        // follower period apart, and the ring's end steps by a master
        // write, so the least within the window can miss the worst by the
        // shorter of the two.
        self.slack_age += period_s;
        if self.slack_age >= SLACK_WINDOW_S {
            let spacing = (period_s * self.loop_rate).min(self.known_jump as f64);
            self.slack_last = if self.slack_min.is_finite() {
                (self.slack_min - spacing) / self.loop_rate * 1e3
            } else {
                f64::NAN
            };
            self.slack_min = f64::INFINITY;
            self.known_jump = 0;
            self.slack_age = 0.0;
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
    fn known_step(&mut self, frames: f64, ms: f64) {
        if !self.started || !frames.is_finite() || !ms.is_finite() {
            return;
        }
        if !self.lp.aligned() {
            self.lp.reset();
            self.step_rest = 0.0;
            return;
        }
        self.status.known_steps.fetch_add(1, Ordering::Relaxed);
        if !self.deferred {
            self.deferred_since = self.graph_out;
        }
        self.deferred = true;
        self.deferred_frames += frames;
        self.deferred_ms += ms;
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
        s.lead_ms.store(bits(self.lead_last), r);
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
    /// An xrun was recovered: the buffer is empty and the device stopped,
    /// waiting for a prime and a start.
    fn needs_restart(&self) -> bool;
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
        if rt.shared.graph_rate.load(Ordering::Relaxed) != p.graph_rate {
            return Ok(Some(RunExit::Rebuild));
        }
        let timeout = ((p.buffer as f64 / f64::from(p.device_rate) * 1e3) as u32).max(10) + 100;
        if !pcm.wait(timeout)? {
            return Ok(None);
        }
        pcm.checkpoint();
        if pcm.needs_restart() {
            rt.xrun();
            prime_device(pcm, rt)?;
            return Ok(None);
        }
        let (delay, time_ns) = pcm.status()?;
        rt.on_reading(Reading {
            written: rt.written(),
            delay,
            time_ns,
        });
        let mut room = pcm.avail()?;
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
    /// `buffer_periods` periods (allocates; off the audio path).
    pub fn open(
        device: &str,
        graph_rate: u32,
        desired: RateRatio,
        period_graph: u32,
        buffer_periods: u32,
    ) -> Result<(Self, RateRatio, SampleFormat, u32, u32), String> {
        let cfg = audivane_io_alsa::AlsaConfig {
            device: device.to_string(),
            rate: graph_rate,
            channels: 2,
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

/// The rate ratio the engine would pick for a device at a graph rate.
pub type RatioFn = Arc<dyn Fn(&EngineConfig, u32) -> RateRatio + Send + Sync>;

/// The engine's period at a graph rate.
pub type QuantumFn = Arc<dyn Fn(&EngineConfig, u32) -> u32 + Send + Sync>;

/// How a follower is opened (internal: no protocol yet).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FollowerOptions {
    /// Its delay behind the master, ms (±[`MAX_TRIM_MS`]).
    pub trim_ms: f64,
    /// Its digital volume, dB.
    pub volume_db: f64,
    /// Its period in graph frames (`None`: the master's quantum).
    pub period_graph: Option<u32>,
    pub buffer_periods: u32,
}

impl Default for FollowerOptions {
    fn default() -> Self {
        Self {
            trim_ms: 0.0,
            volume_db: 0.0,
            period_graph: None,
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
}

/// A follower's open device, as its chain is built for it.
type OpenedAt = (u32, RateRatio, SampleFormat);

/// Whether an edit from `a` to `b` changes what a chain is built from.
fn chain_inputs_differ(a: &EngineConfig, b: &EngineConfig) -> bool {
    a.dsp_enabled != b.dsp_enabled || a.quantum != b.quantum || a.chain != b.chain
}

impl FollowerHandle {
    fn reap(&mut self) {
        while let Ok(c) = self.swaps.retired.pop() {
            drop(c);
        }
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
    quantum: QuantumFn,
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
        quantum,
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
        let period_graph = opts
            .period_graph
            .unwrap_or_else(|| quantum(&cfg, graph_rate));
        let opened = AlsaFollowerPcm::open(
            &device,
            graph_rate,
            desired,
            period_graph,
            opts.buffer_periods,
        );
        let (mut pcm, ratio, format, period, buffer) = match opened {
            Ok(x) => x,
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
                status.set_error(Some(format!("chain: {e}")));
                status.set_state(FollowerState::Lost);
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

fn grow_ms(current: f64, slack_ms: f64, cap: f64) -> Option<f64> {
    if !slack_ms.is_finite() || slack_ms >= 1.0 {
        return None;
    }
    let want = (current + (2.0 - slack_ms).ceil()).clamp(0.0, cap);
    (want > current).then_some(want)
}

/// Paces [`next_alignment_ms`]: once a second (the slack's window), and
/// not for 3 s after a change (the change reaches both DACs and the
/// window refills).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AlignControl {
    /// The alignment applied, ms.
    pub ms: f64,
    next_s: f64,
    /// When it last changed.
    changed_s: f64,
}

impl AlignControl {
    /// At `now_s` (any clock in seconds), with the followers' least slack:
    /// the new alignment, when it changes.
    pub fn tick(&mut self, now_s: f64, least_slack_ms: f64) -> Option<f64> {
        if now_s < self.next_s {
            return None;
        }
        self.next_s = now_s + 1.0;
        let want = next_alignment_ms(self.ms, least_slack_ms)?;
        self.next_s = now_s + 3.0;
        self.changed_s = now_s;
        self.ms = want;
        Some(want)
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
}

/// Paces [`next_lag_ms`] for one follower as [`AlignControl`] paces the
/// alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LagControl {
    next_s: f64,
}

impl LagControl {
    /// At `now_s`, with the master's alignment at its cap: the follower's
    /// new lateness (set on `c`), when it grows.
    pub fn tick(&mut self, now_s: f64, s: &FollowerSnapshot, c: &FollowerControls) -> Option<f64> {
        if now_s < self.next_s {
            return None;
        }
        self.next_s = now_s + 1.0;
        if !matches!(s.state, FollowerState::Acquiring | FollowerState::Locked) {
            return None;
        }
        let lag = next_lag_ms(c.lag_ms(), s.slack_ms)?;
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

/// Every follower's thread, keyed by device (the control side; the tee's
/// slots stay in [`TeeHost`]).
pub struct FollowerHost {
    by_device: BTreeMap<String, FollowerHandle>,
    /// Configured followers that could not start, with why.
    failed: BTreeMap<String, String>,
    build: ChainBuilder,
    desired_ratio: RatioFn,
    quantum: QuantumFn,
}

impl FollowerHost {
    pub fn new(build: ChainBuilder, desired_ratio: RatioFn, quantum: QuantumFn) -> Self {
        Self {
            by_device: BTreeMap::new(),
            failed: BTreeMap::new(),
            build,
            desired_ratio,
            quantum,
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
        tee.add_follower(device, master)?;
        let reader = tee
            .slots_mut()
            .get_mut(device)
            .and_then(|s| s.reader.take())
            .ok_or_else(|| format!("{device}: no tee ring"))?;
        // A/B stays on the master.
        let mut cfg = tee
            .slots()
            .get(device)
            .map_or_else(|| master.clone(), |s| s.chain_config.clone());
        cfg.ab = None;
        let config = Arc::new(Mutex::new(cfg));
        let opened = Arc::new(Mutex::new(None));
        let (swap_host, swaps) = chain_swaps();
        let status = Arc::new(FollowerStatus::default());
        let controls = Arc::new(FollowerControls::new(opts.trim_ms, opts.volume_db));
        controls.set_volume_db(opts.volume_db);
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
            quantum: Arc::clone(&self.quantum),
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
            },
        );
        Ok(())
    }

    /// A follower plays `device`.
    pub fn contains(&self, device: &str) -> bool {
        self.by_device.contains_key(device)
    }

    /// Stop and remove the follower on `device` (waits for its thread).
    pub fn remove(&mut self, tee: &mut TeeHost, device: &str) -> Result<(), String> {
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
                c.set_volume_db(f.volume_db);
                c.set_enabled(f.enabled);
                continue;
            }
            let opts = FollowerOptions {
                trim_ms: f.trim_ms,
                volume_db: f.volume_db,
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
    /// still runs out of source plays later by what it lacks (owner
    /// decision 3), paced as [`AlignControl`]: once a second, not for 3 s
    /// after a change. It never shrinks while the follower runs.
    pub fn tick_lag(&mut self, now_s: f64, at_cap: bool) {
        if !at_cap {
            return;
        }
        for (device, h) in &mut self.by_device {
            if let Some(lag) = h.lag.tick(now_s, &h.status.snapshot(), &h.controls) {
                log::info!(
                    "followers: {device} needs more than the master's alignment, plays {lag} ms late"
                );
            }
        }
    }

    pub fn controls(&self, device: &str) -> Option<&FollowerControls> {
        self.by_device.get(device).map(|h| &*h.controls)
    }

    /// The master's config changed: every follower's slot follows (v1); its
    /// chain is rebuilt from it at its next open (a live rebuild is not wired
    /// yet).
    pub fn config_changed(&mut self, master: &EngineConfig) {
        let mut cfg = master.clone();
        cfg.ab = None;
        for (device, h) in &mut self.by_device {
            h.reap();
            let changed = match h.config.lock() {
                Ok(mut c) => {
                    let changed = chain_inputs_differ(&c, &cfg);
                    *c = cfg.clone();
                    changed
                }
                Err(_) => false,
            };
            let opened = h.opened.lock().ok().and_then(|g| *g);
            let Some((rate, ratio, format)) = opened.filter(|_| changed) else {
                continue;
            };
            // Built here, off its audio thread; it swaps in there. A
            // follower that reopens meanwhile builds from the slot anyway.
            match (self.build)(&cfg, rate, ratio, format) {
                Ok(chain) => {
                    if h.swaps.incoming.push(chain).is_err() {
                        log::warn!(
                            "follower {device}: chain queue full, the edit applies at its next open"
                        );
                    }
                }
                Err(e) => log::warn!("follower {device}: chain not rebuilt: {e}"),
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
        for h in self.by_device.values_mut() {
            h.stop();
        }
    }
}

#[cfg(test)]
#[path = "follower_tests.rs"]
mod tests;
