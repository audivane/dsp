//! Measurement playback, the audio-thread half: a WAV file played straight
//! to chosen output channels, other streams muted, the DACs kept awake by
//! a little noise (`measure_play`, `docs/control-plane.md` "Measurement").
//!
//! ```text
//!  source ─► mute (others) ─► tee ─► align ─► [active: file] ─► chain ─► [bypass: file] ─► DAC
//!                              │                                                 (master)
//!                              └─► follower: queue ─► [file] ─► stage ─► chain ─► DAC
//!                                                                  └─ bypass: tap ──┘
//! ```
//!
//! - **One timeline.** The master picks the source frame the file starts
//!   at (`s0`, the tee's source index) when it arms, a lead-in ahead. Every
//!   device plays file frame `k` where its path carries source frame
//!   `s0 + k`: the alignment, each follower's trim, lag and drift loop apply,
//!   as they do to music.
//! - **Injection points.** `active`: the master's chain input (after the
//!   alignment delay) and each follower's stage input, so the file runs
//!   through every stage. `bypass`: after the master's chain, at its output
//!   rate, with its digital volume (limiter and quantizer skipped: at unity
//!   the file's samples are exact); on a follower at the stage input
//!   (drift-corrected), its stage output taken around the chain with the
//!   follower's volume (x1 only: the stage runs at the graph rate).
//! - A routed channel carries only the file (and the keepalive) between a
//!   10 ms crossfade in at the lead-in's start and one out after the file's
//!   end; the source stays on the others, muted by the master before the tee
//!   when `mute_others`. `active` plays the keepalive after the chain instead
//!   ([`PostKeepalive`]): a low-pass chain would take it down.
//! - **Continuity.** Each device's DAC readings (its position, written minus
//!   the queue, at the reading's `CLOCK_MONOTONIC_RAW` time) go into a buffer
//!   built with the job ([`Readings`]); the control side fits them after the
//!   run and reports a step in the timeline. A chain swap (or, `active`, a
//!   late channel worker) while the run plays on a device faults it.
//! - **Never loud.** What the measurement adds is clamped at [`CEILING`] (the
//!   file, the keepalive and the live resampler's overshoot), and so is a
//!   device's whole output while the measurement runs there (lead-in through
//!   drain), whatever the chain's limiter does.
//! - **The end.** A device is `done` once its DAC has played the last frame
//!   the measurement touched: the injection's end (`fed`), the processing
//!   after it (a resampler's tail, the chain's) and the device queue. The
//!   master lifts the mute only on `release`, which the control side sets once
//!   every device is done (or gave up).
//!
//! **Real time.** The job (its buffers, at each device's injection rate) is
//! built on the control side and handed over through a [`MeasureSlot`],
//! lock-free: the audio thread takes the posted box and hands its old one back
//! through an outbox, which the control side frees
//! ([`MeasureSlot::reclaim`]); nothing is freed or allocated on the audio
//! thread. Per block the work is a copy and a few multiplications per frame.

use audivane_protocol::RateRatio;
use audivane_protocol::ipc::{MeasureChain, SinkChoice};
use std::f64::consts::PI;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering,
};

/// The stop fade of the file, ms.
pub const STOP_FADE_MS: f64 = 5.0;
/// The mute ramp of the other streams, ms.
pub const MUTE_RAMP_MS: f64 = 10.0;
/// The crossfade between what a routed channel carried and the
/// measurement, at both ends, ms.
pub const XFADE_MS: f64 = 10.0;
/// The highest sample a measurement sends a DAC, linear (−1 dBFS).
pub const CEILING: f64 = 0.891_250_938_133_745_6;
/// Frames one block may carry for the master's scratch (the tee's).
pub const MAX_BLOCK: usize = super::tee::MAX_BLOCK_FRAMES;

/// [`CEILING`] as a left-justified `i32`.
const CEILING_I32: f64 = CEILING * 2_147_483_648.0;

/// Why the audio thread ended a measurement early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MeasureFault {
    None = 0,
    /// Not the DSP sink with DSP on any more (Direct, DSP off).
    Path = 1,
    /// The graph rate or the device ratio changed (or differ from the
    /// prepared ones).
    Rate = 2,
    /// The multi-device alignment changed (a follower's step).
    Alignment = 3,
    /// Playback was interrupted: an underrun, a reopen, a stop.
    Interrupted = 4,
    /// A follower lost its lock (or was muted) while it played the file.
    LostSync = 5,
    /// Varispeed (or Keep pitch) runs.
    Varispeed = 6,
    /// A block larger than the scratch.
    Block = 7,
    /// A follower runs away from x1 in `bypass`.
    Ratio = 8,
    /// A routed follower's output shut while the file was on its way out
    /// (`active`: its compensation pad, as its chain or the master's
    /// compensation moved).
    Muted = 9,
    /// `active`: a channel worker of the main device's chain missed its
    /// deadline while the run played there (the chain played silence and
    /// reset).
    Worker = 10,
    /// A routed device's chain was swapped while the run played there
    /// (`active`: any swap; `bypass`: one that changed its latency).
    ChainSwap = 11,
}

impl MeasureFault {
    fn from_code(c: u8) -> Self {
        match c {
            1 => Self::Path,
            2 => Self::Rate,
            3 => Self::Alignment,
            4 => Self::Interrupted,
            5 => Self::LostSync,
            6 => Self::Varispeed,
            7 => Self::Block,
            8 => Self::Ratio,
            9 => Self::Muted,
            10 => Self::Worker,
            11 => Self::ChainSwap,
            _ => Self::None,
        }
    }

    /// Plain text for the status.
    pub fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Path => {
                "the engine left the DSP path (Audivane Direct, DSP off or a sink switch)"
            }
            Self::Rate => "the graph rate or a device rate changed since the run was prepared",
            Self::Alignment => "the multi-device alignment changed (a follower stepped)",
            Self::Interrupted => "playback was interrupted (an underrun, a reopen or a stop)",
            Self::LostSync => "a follower lost its lock",
            Self::Varispeed => "varispeed (or Keep pitch) is on",
            Self::Block => "a block larger than the measurement scratch",
            Self::Ratio => {
                "a follower runs away from x1 (bypass needs its device at the graph rate)"
            }
            Self::Muted => {
                "a routed follower was muted while it played the file (its chain compensation moved)"
            }
            Self::Worker => {
                "a channel worker of the main device's chain missed its deadline during the run (the chain played silence)"
            }
            Self::ChainSwap => {
                "a routed device's chain changed during the run (a chain swap: settings were edited)"
            }
        }
    }
}

/// An `f64` in an atomic (`NaN`: unset).
fn load_f64(a: &AtomicU64) -> Option<f64> {
    let v = f64::from_bits(a.load(Ordering::Acquire));
    v.is_finite().then_some(v)
}

/// The shortest time between two stored DAC readings, ns: a reading every
/// period at 768 kHz would hold ten times more than the check needs.
pub const READING_INTERVAL_NS: u64 = 2_000_000;

/// A device's DAC position against `CLOCK_MONOTONIC_RAW` through a run
/// (the playback continuity check): preallocated on the control side; its
/// audio thread appends (one writer), the control side reads what was
/// published (`len`, release/acquire) and fits it after the run.
#[derive(Debug, Default)]
pub struct Readings {
    /// Device frames at the DAC (written minus the queue).
    pos: Box<[AtomicI64]>,
    /// When, `CLOCK_MONOTONIC_RAW` ns.
    time: Box<[AtomicU64]>,
    len: AtomicUsize,
    /// Readings that found the buffer full.
    dropped: AtomicU64,
}

impl Readings {
    /// Room for `n` readings (control side: it allocates).
    pub fn with_capacity(n: usize) -> Self {
        Self {
            pos: (0..n).map(|_| AtomicI64::new(0)).collect(),
            time: (0..n).map(|_| AtomicU64::new(0)).collect(),
            len: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Audio thread: store a reading, at most one per
    /// [`READING_INTERVAL_NS`] (and only forward in time). Never allocates.
    fn push(&self, pos: i64, time_ns: u64) {
        let n = self.len.load(Ordering::Relaxed);
        if n > 0 {
            let last = self.time[n - 1].load(Ordering::Relaxed);
            if time_ns < last.saturating_add(READING_INTERVAL_NS) {
                return;
            }
        }
        if n >= self.pos.len() {
            if !self.pos.is_empty() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        self.pos[n].store(pos, Ordering::Relaxed);
        self.time[n].store(time_ns, Ordering::Relaxed);
        self.len.store(n + 1, Ordering::Release);
    }

    /// Control side: the readings so far, (DAC position, time ns).
    pub fn snapshot(&self) -> Vec<(i64, u64)> {
        let n = self.len.load(Ordering::Acquire);
        (0..n)
            .map(|i| {
                (
                    self.pos[i].load(Ordering::Relaxed),
                    self.time[i].load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    /// Readings that found the buffer full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// One device's part of a job: its routed channels' samples at its
/// injection rate, and what it reports.
#[derive(Debug)]
pub struct MeasureDevice {
    /// `None`: the main device.
    pub follower: Option<String>,
    /// Injection rate, Hz.
    pub rate: u32,
    /// The device's output rate the run was prepared for, Hz.
    pub device_rate: u32,
    /// (device channel, samples) per route on this device.
    pub routes: Vec<(usize, Vec<f64>)>,
    /// File frames (at the injection rate).
    pub len: u64,
    /// The lead-in, injection frames.
    pub lead: u64,
    /// The file position this device plays next (`< 0`: the lead-in).
    pub pos: AtomicI64,
    /// The injection touched its first frame (the lead-in's crossfade).
    pub started: AtomicBool,
    /// The injection is over: the routed channels carry the source again
    /// from file position `fed_end` (injection frames).
    pub fed: AtomicBool,
    pub fed_end: AtomicI64,
    /// The stimulus is over at the injection (the file's end, or the stop
    /// fade's): the keepalive after the chain fades out.
    pub stim_over: AtomicBool,
    /// The device frame (its own output count, `f64` bits) from which
    /// nothing of the measurement is left in its output, once known.
    pub last_frame: AtomicU64,
    /// The DAC has played up to `last_frame` (or the device never started).
    pub done: AtomicBool,
    /// Its output frame carrying file frame 0 (`f64` bits; NaN unset).
    pub start_frame: AtomicU64,
    /// When that frame leaves the queue, `CLOCK_MONOTONIC_RAW` ns (0 unset).
    pub start_time_ns: AtomicU64,
    /// Processing between the injection and the queue, device frames
    /// (`f64` bits), and the queue at the first reading after the start.
    pub chain_frames: AtomicU64,
    pub queue_frames: AtomicU64,
    /// From the injection point to the device's output after the
    /// injection's end: the processing's whole tail (a chain's ringing
    /// included), device frames (`f64` bits).
    pub tail_frames: AtomicU64,
    /// Samples clamped at [`CEILING`].
    pub clamped: AtomicU64,
    pub fault: AtomicU8,
    /// The DAC readings from the device's first touched frame until it
    /// drained (empty: not recorded; [`Self::with_readings`]).
    pub readings: Readings,
}

impl MeasureDevice {
    pub fn new(
        follower: Option<String>,
        rate: u32,
        device_rate: u32,
        routes: Vec<(usize, Vec<f64>)>,
        lead: u64,
    ) -> Self {
        let len = routes.iter().map(|(_, s)| s.len()).max().unwrap_or(0) as u64;
        Self {
            follower,
            rate,
            device_rate,
            routes,
            len,
            lead,
            pos: AtomicI64::new(i64::MIN),
            started: AtomicBool::new(false),
            fed: AtomicBool::new(false),
            fed_end: AtomicI64::new(i64::MIN),
            stim_over: AtomicBool::new(false),
            last_frame: AtomicU64::new(f64::NAN.to_bits()),
            done: AtomicBool::new(false),
            start_frame: AtomicU64::new(f64::NAN.to_bits()),
            start_time_ns: AtomicU64::new(0),
            chain_frames: AtomicU64::new(0f64.to_bits()),
            queue_frames: AtomicU64::new(f64::NAN.to_bits()),
            tail_frames: AtomicU64::new(0f64.to_bits()),
            clamped: AtomicU64::new(0),
            fault: AtomicU8::new(0),
            readings: Readings::default(),
        }
    }

    /// Record up to `n` DAC readings through the run (control side).
    pub fn with_readings(mut self, n: usize) -> Self {
        self.readings = Readings::with_capacity(n);
        self
    }

    /// A DAC reading (audio thread): the DAC is at `at_dac` (device
    /// frames) at `time_ns`. Stored while the run plays here (from its
    /// first touched frame until drained); marks the drain.
    pub fn note_reading(&self, at_dac: i64, time_ns: u64) {
        if self.started.load(Ordering::Relaxed) && !self.done.load(Ordering::Relaxed) {
            self.readings.push(at_dac, time_ns);
        }
        self.note_played(at_dac as f64);
    }

    /// The run plays on this device now: it touched its first frame and
    /// has not drained.
    pub(crate) fn live(&self) -> bool {
        self.started.load(Ordering::Relaxed) && !self.done.load(Ordering::Relaxed)
    }

    pub fn fault(&self) -> MeasureFault {
        MeasureFault::from_code(self.fault.load(Ordering::Acquire))
    }

    fn set_fault(&self, f: MeasureFault) {
        let _ = self
            .fault
            .compare_exchange(0, f as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn start_frame(&self) -> Option<f64> {
        load_f64(&self.start_frame)
    }

    pub fn last_frame(&self) -> Option<f64> {
        load_f64(&self.last_frame)
    }

    /// Set the last frame once.
    pub fn set_last_frame(&self, at: f64) {
        if self.last_frame().is_none() {
            self.last_frame.store(at.to_bits(), Ordering::Release);
        }
    }

    pub fn queue(&self) -> Option<f64> {
        load_f64(&self.queue_frames)
    }

    pub fn chain(&self) -> f64 {
        f64::from_bits(self.chain_frames.load(Ordering::Acquire))
    }

    pub fn tail(&self) -> f64 {
        f64::from_bits(self.tail_frames.load(Ordering::Acquire))
    }

    /// The injection's end, once it is over.
    pub fn fed_end(&self) -> Option<i64> {
        self.fed
            .load(Ordering::Acquire)
            .then(|| self.fed_end.load(Ordering::Acquire))
    }

    /// Nothing of the run is left on this device: drained, or (`ending`)
    /// never started.
    pub fn finished(&self, ending: bool) -> bool {
        self.done.load(Ordering::Acquire) || (ending && !self.started.load(Ordering::Acquire))
    }

    /// The run ends before this device touched a frame: it is over here.
    fn finish_unstarted(&self) {
        if !self.started.load(Ordering::Relaxed) {
            self.stim_over.store(true, Ordering::Release);
            self.fed.store(true, Ordering::Release);
            self.done.store(true, Ordering::Release);
        }
    }

    /// The DAC played up to `at_dac` (device frames).
    pub fn note_played(&self, at_dac: f64) {
        if !self.done.load(Ordering::Relaxed)
            && let Some(lf) = self.last_frame()
            && at_dac >= lf
        {
            self.done.store(true, Ordering::Release);
        }
    }

    fn count_clamps(&self, n: u64) {
        if n > 0 {
            self.clamped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The device has a route on `channel`: its index in `routes`.
    fn route_of(&self, channel: usize) -> Option<usize> {
        self.routes.iter().position(|(c, _)| *c == channel)
    }
}

/// A measurement, shared by the control side and every device's thread.
#[derive(Debug)]
pub struct MeasureJob {
    pub chain: MeasureChain,
    pub mute_others: bool,
    /// Keepalive noise RMS, linear (0: none).
    pub keepalive: f64,
    /// The lead-in, graph frames.
    pub lead_graph: u64,
    /// The graph rate the run was prepared for, Hz.
    pub graph_rate: u32,
    /// The main device the run was prepared for (`None`: not checked).
    pub master_device: Option<String>,
    /// Every file sample is an `f32` value: the planar input of `active`
    /// stays `f32`-exact (a crossfade rounded to `f32`), so a chain that
    /// passes the source unchanged passes the file unchanged too.
    pub f32_exact: bool,
    pub devices: Vec<MeasureDevice>,
    /// The source frame file frame 0 plays at (`i64::MIN` until the
    /// master arms).
    pub s0: AtomicI64,
    /// Stop (the file fades out).
    pub cancel: AtomicBool,
    /// Every device is done (or the control side gave up): lift the mute.
    pub release: AtomicBool,
    /// The master's mute is back at unity after the run (or never fell).
    pub released: AtomicBool,
    /// Master-wide fault (before or apart from a device's).
    pub fault: AtomicU8,
}

impl MeasureJob {
    pub fn new(
        chain: MeasureChain,
        mute_others: bool,
        keepalive: f64,
        lead_graph: u64,
        graph_rate: u32,
        devices: Vec<MeasureDevice>,
    ) -> Self {
        let f32_exact = devices.iter().all(|d| {
            d.routes
                .iter()
                .all(|(_, x)| x.iter().all(|v| f64::from(*v as f32) == *v))
        });
        Self {
            chain,
            mute_others,
            keepalive,
            lead_graph,
            graph_rate,
            master_device: None,
            f32_exact,
            devices,
            s0: AtomicI64::new(i64::MIN),
            cancel: AtomicBool::new(false),
            release: AtomicBool::new(false),
            released: AtomicBool::new(false),
            fault: AtomicU8::new(0),
        }
    }

    pub fn s0(&self) -> Option<u64> {
        let v = self.s0.load(Ordering::Acquire);
        (v >= 0).then_some(v as u64)
    }

    pub fn fault(&self) -> MeasureFault {
        MeasureFault::from_code(self.fault.load(Ordering::Acquire))
    }

    pub fn set_fault(&self, f: MeasureFault) {
        let _ = self
            .fault
            .compare_exchange(0, f as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Any fault, the master's first.
    pub fn any_fault(&self) -> MeasureFault {
        let f = self.fault();
        if f != MeasureFault::None {
            return f;
        }
        self.devices
            .iter()
            .map(MeasureDevice::fault)
            .find(|f| *f != MeasureFault::None)
            .unwrap_or(MeasureFault::None)
    }

    pub fn ending(&self) -> bool {
        self.cancel.load(Ordering::Acquire) || self.any_fault() != MeasureFault::None
    }
}

/// What a slot carries: the job and this thread's device in it.
type Posted = Option<(Arc<MeasureJob>, usize)>;

/// Boxes an audio thread can hand back before the control side frees them.
const OUTBOX: usize = 4;

/// The handover of a job to one device's thread, lock-free: the control
/// side swaps a box into `inbox`; the audio thread swaps it out and parks
/// the box it held in a free `outbox` entry, which the control side frees.
#[derive(Debug, Default)]
pub struct MeasureSlot {
    inbox: AtomicPtr<Posted>,
    outbox: [AtomicPtr<Posted>; OUTBOX],
}

/// The device index for a thread that plays no route (the master still
/// mutes and arms).
pub const NO_DEVICE: usize = usize::MAX;

/// Free a box the audio thread no longer sees.
fn free(p: *mut Posted) {
    if !p.is_null() {
        // SAFETY: every pointer here came from `Box::into_raw` in `post`,
        // and was taken out of the slot by this one owner.
        drop(unsafe { Box::from_raw(p) });
    }
}

/// The posted job behind `p`. The reference lives as long as the owner
/// keeps `p` (the audio thread, until it takes the next box).
fn posted<'a>(p: *mut Posted) -> Option<(&'a MeasureJob, usize)> {
    // SAFETY: `p` is null or a box this thread owns (see `MeasureSlot`).
    unsafe { p.as_ref() }
        .and_then(Option::as_ref)
        .map(|(j, i)| (&**j, *i))
}

impl MeasureSlot {
    /// Control side: hand `job` (and this thread's device in it) over, or
    /// take it back (`None`). Frees what the audio thread handed back.
    pub fn post(&self, job: Posted) {
        let new = Box::into_raw(Box::new(job));
        // A box the audio thread never took is still ours.
        free(self.inbox.swap(new, Ordering::AcqRel));
        self.reclaim();
    }

    /// Control side: free the boxes the audio thread handed back (and with
    /// them its references to their jobs).
    pub fn reclaim(&self) {
        for o in &self.outbox {
            free(o.swap(ptr::null_mut(), Ordering::AcqRel));
        }
    }

    /// Audio thread: take a newly posted box into `cur`, handing the old one
    /// back. Never blocks or frees: with no free outbox entry it waits for
    /// the control side's next `reclaim`.
    fn take(&self, cur: &mut *mut Posted) -> bool {
        if self.inbox.load(Ordering::Acquire).is_null() {
            return false;
        }
        let back = if cur.is_null() {
            None
        } else {
            // Only this thread stores into an empty outbox entry.
            match self
                .outbox
                .iter()
                .find(|o| o.load(Ordering::Acquire).is_null())
            {
                Some(o) => Some(o),
                None => return false,
            }
        };
        let new = self.inbox.swap(ptr::null_mut(), Ordering::AcqRel);
        if new.is_null() {
            return false;
        }
        if let Some(o) = back {
            o.store(*cur, Ordering::Release);
        }
        *cur = new;
        true
    }
}

impl Drop for MeasureSlot {
    fn drop(&mut self) {
        free(*self.inbox.get_mut());
        for o in &mut self.outbox {
            free(*o.get_mut());
        }
    }
}

/// A small, fast generator for the keepalive (xorshift64*).
#[derive(Clone, Copy, Debug)]
struct Noise(u64);

impl Noise {
    /// Uniform in `[-1, 1)`.
    #[inline]
    fn next(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        let r = x.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (r >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// Raised cosine of `t` in `[0, 1]`.
#[inline]
fn rc(t: f64) -> f64 {
    0.5 - 0.5 * (PI * t.clamp(0.0, 1.0)).cos()
}

/// One device's audio-thread state: the job it plays, its noise, its stop
/// fade and its crossfade.
#[derive(Debug)]
pub struct Injector {
    slot: Arc<MeasureSlot>,
    /// The box taken from the slot (null: none yet).
    cur: *mut Posted,
    noise: Noise,
    /// The stop fade's gain and step (per injection frame).
    fade: f64,
    fade_step: f64,
    /// The crossfade's length, injection frames (0: not set).
    xfade: i64,
    /// Where the stop fade ended (the file position), once it did.
    stop_end: Option<i64>,
}

// SAFETY: `cur` is a box this injector owns until it hands it back through
// the slot; the job behind it is `Sync`.
unsafe impl Send for Injector {}

impl Drop for Injector {
    fn drop(&mut self) {
        // The processor's teardown, off the audio loop.
        free(self.cur);
    }
}

impl Injector {
    pub fn new(slot: Arc<MeasureSlot>, seed: u64) -> Self {
        Self {
            slot,
            cur: ptr::null_mut(),
            noise: Noise(seed | 1),
            fade: 1.0,
            fade_step: 0.0,
            xfade: 0,
            stop_end: None,
        }
    }

    /// Take a newly posted job. Returns whether it changed.
    pub fn poll(&mut self) -> bool {
        let changed = self.slot.take(&mut self.cur);
        if changed {
            self.fade = 1.0;
            self.fade_step = 0.0;
            self.xfade = 0;
            self.stop_end = None;
        }
        changed
    }

    /// The job and this thread's device index.
    fn posted<'a>(&self) -> Option<(&'a MeasureJob, usize)> {
        posted(self.cur)
    }

    pub fn job(&self) -> Option<&MeasureJob> {
        self.posted().map(|(j, _)| j)
    }

    /// This thread's device in the job (`None`: no route here, or no job).
    pub fn device(&self) -> Option<&MeasureDevice> {
        let (j, i) = self.posted()?;
        j.devices.get(i)
    }

    /// The device still injects: a job, a route here, not fed.
    pub fn playing(&self) -> bool {
        self.device()
            .is_some_and(|d| !d.fed.load(Ordering::Relaxed))
    }

    /// Prepare the stop fade and the crossfade at the injection rate.
    fn ensure_steps(&mut self, rate: u32) {
        if self.fade_step == 0.0 {
            let r = f64::from(rate.max(1));
            self.fade_step = 1.0 / (STOP_FADE_MS * 1e-3 * r).max(1.0);
            self.xfade = (XFADE_MS * 1e-3 * r).round().max(1.0) as i64;
        }
    }

    /// The stimulus's end: the file's, or the stop fade's.
    fn end(&self, len: i64) -> i64 {
        self.stop_end.map_or(len, |e| e.min(len))
    }

    /// The next injected value on one route at file position `k` (before
    /// the crossfade), with the stop fade `g`.
    #[inline]
    fn value(&mut self, samples: &[f64], k: i64, ka: f64, g: f64) -> f64 {
        let mut v = 0.0;
        if ka > 0.0 {
            v += self.noise.next() * ka;
        }
        if k >= 0
            && let Some(s) = samples.get(k as usize)
        {
            v += *s;
        }
        v * g
    }

    /// One frame's stop fade and crossfade weight at file position `k`:
    /// `None` past the window (the routed channels are the source again).
    #[inline]
    fn step(&mut self, k: i64, lead: i64, len: i64, ending: bool) -> Option<(f64, f64)> {
        let mut g = 1.0;
        if k < self.end(len) && ending {
            self.fade -= self.fade_step;
            if self.fade < 0.5 * self.fade_step {
                self.fade = 0.0;
            }
            if self.fade <= 0.0 && self.stop_end.is_none() {
                self.stop_end = Some(k);
            }
            g = self.fade;
        }
        let end = self.end(len);
        if k >= end + self.xfade {
            return None;
        }
        let x = self.xfade as f64;
        let rise = (k + lead + 1) as f64 / x;
        let fall = (end + self.xfade - k) as f64 / x;
        let g = if k < end { g } else { 0.0 };
        Some((g, rc(rise.min(fall))))
    }

    /// Fill frames `0..n` of `chans` (planar, the device's channels) on the
    /// routed channels with the file from position `k0` (injection frames),
    /// crossfaded with what they carry at the window's ends. Counts the
    /// position and marks the injection's end. `open`: the device's own
    /// path is in sync (a follower); a frame of the window played while not
    /// is a fault.
    pub fn fill_planar(&mut self, chans: &mut [Vec<f64>], n: usize, k0: i64, open: bool) {
        let Some((job, i)) = self.posted() else {
            return;
        };
        let Some(d) = job.devices.get(i) else {
            return;
        };
        if d.fed.load(Ordering::Relaxed) {
            return;
        }
        self.ensure_steps(d.rate);
        let lead = d.lead as i64;
        let len = d.len as i64;
        // `active` plays the keepalive after the chain instead
        // ([`PostKeepalive`]): one copy at the set level whatever the chain
        // does to its input.
        let ka = match job.chain {
            MeasureChain::Active => 0.0,
            MeasureChain::Bypass => job.keepalive * 3f64.sqrt(),
        };
        let mut ending = job.ending() || d.fault() != MeasureFault::None;
        if ending && !d.started.load(Ordering::Relaxed) {
            d.finish_unstarted();
            return;
        }
        let in_window = k0 + n as i64 > -lead && k0 < self.end(len) + self.xfade;
        if in_window && !open && !ending {
            d.set_fault(MeasureFault::LostSync);
            ending = true;
            if !d.started.load(Ordering::Relaxed) {
                d.finish_unstarted();
                return;
            }
        }
        let exact = job.f32_exact;
        let mut over = None;
        let mut clamps = 0u64;
        for f in 0..n {
            let k = k0 + f as i64;
            if k < -lead {
                continue;
            }
            let Some((g, w)) = self.step(k, lead, len, ending) else {
                over = Some(k);
                break;
            };
            d.started.store(true, Ordering::Relaxed);
            for (c, samples) in &d.routes {
                let v = self.value(samples, k, ka, g);
                let Some(chan) = chans.get_mut(*c) else {
                    continue;
                };
                let v = if v.abs() > CEILING {
                    clamps += 1;
                    v.clamp(-CEILING, CEILING)
                } else {
                    v
                };
                let x = if w >= 1.0 {
                    v
                } else {
                    chan[f] * (1.0 - w) + v * w
                };
                chan[f] = if exact { f64::from(x as f32) } else { x };
            }
        }
        d.count_clamps(clamps);
        let next = k0 + n as i64;
        d.pos.store(next.min(len), Ordering::Relaxed);
        if next >= self.end(len) {
            d.stim_over.store(true, Ordering::Release);
        }
        if let Some(k) = over {
            d.fed_end.store(k, Ordering::Release);
            d.stim_over.store(true, Ordering::Release);
            d.fed.store(true, Ordering::Release);
        }
    }

    /// Write frames of the file into interleaved left-justified `i32`
    /// output on the routed channels from position `k0`, crossfaded with
    /// what they carry at the window's ends: exact at unity `gains`
    /// (`[L, R, rest]`), otherwise scaled and TPDF-dithered to `bits`.
    pub fn fill_i32(
        &mut self,
        out: &mut [i32],
        channels: usize,
        k0: i64,
        gains: [f64; 3],
        unity: bool,
        bits: u32,
    ) {
        let ch = channels.max(1);
        let n = out.len() / ch;
        let Some((job, i)) = self.posted() else {
            return;
        };
        let Some(d) = job.devices.get(i) else {
            return;
        };
        if d.fed.load(Ordering::Relaxed) {
            return;
        }
        self.ensure_steps(d.rate);
        let lead = d.lead as i64;
        let len = d.len as i64;
        let ka = job.keepalive * 3f64.sqrt();
        let ending = job.ending() || d.fault() != MeasureFault::None;
        if ending && !d.started.load(Ordering::Relaxed) {
            d.finish_unstarted();
            return;
        }
        let bits = bits.clamp(8, 32);
        let lsb = (1u64 << (32 - bits)) as f64;
        let scale = 2_147_483_648.0;
        let mut over = None;
        let mut clamps = 0u64;
        for f in 0..n {
            let k = k0 + f as i64;
            if k < -lead {
                continue;
            }
            let Some((g, w)) = self.step(k, lead, len, ending) else {
                over = Some(k);
                break;
            };
            d.started.store(true, Ordering::Relaxed);
            for (c, samples) in &d.routes {
                let v = self.value(samples, k, ka, g);
                if *c >= ch {
                    continue;
                }
                let exact = unity && g == 1.0 && w >= 1.0;
                let t = if exact { v } else { v * gains[(*c).min(2)] };
                let t = if t.abs() > CEILING {
                    clamps += 1;
                    t.clamp(-CEILING, CEILING)
                } else {
                    t
                };
                let o = &mut out[f * ch + c];
                let x = if exact && t == v {
                    v * scale
                } else {
                    let m = f64::from(*o) * (1.0 - w) + t * scale * w;
                    if bits < 32 {
                        // TPDF over one LSB of the device word, rounded to it.
                        let dz = (self.noise.next() + self.noise.next()) * 0.5 * lsb;
                        ((m + dz) / lsb).round() * lsb
                    } else {
                        m
                    }
                };
                *o = x.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            }
        }
        d.count_clamps(clamps);
        let next = k0 + n as i64;
        d.pos.store(next.min(len), Ordering::Relaxed);
        if next >= self.end(len) {
            d.stim_over.store(true, Ordering::Release);
        }
        if let Some(k) = over {
            d.fed_end.store(k, Ordering::Release);
            d.stim_over.store(true, Ordering::Release);
            d.fed.store(true, Ordering::Release);
        }
    }
}

/// `active`: the keepalive after the chain, on the routed channels of the
/// device's output (the only copy: none goes in at the chain's input), so
/// a chain that would take it down (a sub's low-pass) still leaves the
/// DAC enough to stay awake, at the set level whatever the chain does. It
/// runs from the device's first touched frame (raised-cosine in over
/// [`XFADE_MS`]) until the stimulus is over at the injection (out over
/// the same from there), and never past the measurement's last frame at
/// the output. The chain's output is already on the device's word there
/// (`bits`): the sum is rounded back to it with no dither of its own (the
/// noise dithers itself), so the routed channel's added noise is the
/// keepalive alone.
#[derive(Debug)]
pub struct PostKeepalive {
    noise: Noise,
    /// The ramp's weight phase: 0..1 (up from the start, down once the
    /// stimulus is over).
    phase: f64,
}

impl PostKeepalive {
    fn new(seed: u64) -> Self {
        Self {
            noise: Noise(seed | 1),
            phase: 0.0,
        }
    }

    /// A new job: the ramp starts again.
    fn reset(&mut self) {
        self.phase = 0.0;
    }

    /// Add the keepalive to `out` (interleaved, `channels`; its first frame
    /// is the device's output frame `first`) at `rate`, when `job` is
    /// `active` and plays on `d` now. Returns the samples clamped.
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        job: &MeasureJob,
        d: &MeasureDevice,
        out: &mut [i32],
        channels: usize,
        first: u64,
        rate: u32,
        bits: u32,
    ) {
        if job.chain != MeasureChain::Active
            || job.keepalive <= 0.0
            || !d.live()
            || job.release.load(Ordering::Relaxed)
        {
            return;
        }
        let ch = channels.max(1);
        let n = out.len() / ch;
        let ka = job.keepalive * 3f64.sqrt() * 2_147_483_648.0;
        let xfade = (XFADE_MS * 1e-3 * f64::from(rate.max(1))).max(1.0);
        let step = 1.0 / xfade;
        let end = d.last_frame();
        let over = d.stim_over.load(Ordering::Acquire);
        if over && self.phase <= 0.0 {
            return;
        }
        let bits = bits.clamp(8, 32);
        let lsb = (1u64 << (32 - bits)) as f64;
        let mut clamps = 0u64;
        for f in 0..n {
            let o = (first + f as u64) as f64;
            let fall = match end {
                Some(e) if o >= e => break,
                Some(e) => (e - o) / xfade,
                None => 1.0,
            };
            self.phase = if over {
                (self.phase - step).max(0.0)
            } else {
                (self.phase + step).min(1.0)
            };
            let w = rc(self.phase.min(fall));
            if w <= 0.0 {
                if over {
                    break;
                }
                continue;
            }
            for (c, _) in &d.routes {
                let Some(x) = out.get_mut(f * ch + c).filter(|_| *c < ch) else {
                    continue;
                };
                let mut m = f64::from(*x) + self.noise.next() * ka * w;
                if bits < 32 {
                    m = (m / lsb).round() * lsb;
                }
                if m.abs() > CEILING_I32 {
                    clamps += 1;
                    m = m.clamp(-CEILING_I32, CEILING_I32);
                }
                *x = m.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            }
        }
        d.count_clamps(clamps);
    }
}

/// Clamp interleaved `i32` output at [`CEILING`]: how many samples were.
fn clamp_i32(out: &mut [i32]) -> u64 {
    let c = CEILING_I32 as i32;
    let mut n = 0;
    for x in out {
        if *x > c || *x < -c {
            *x = (*x).clamp(-c, c);
            n += 1;
        }
    }
    n
}

/// The master's mute of the other streams: a raised-cosine ramp on the
/// source before the tee.
#[derive(Clone, Copy, Debug)]
struct SourceMute {
    /// 1 = open, 0 = muted (the ramp's phase).
    phase: f64,
    step: f64,
}

impl SourceMute {
    fn gain(&self) -> f64 {
        rc(self.phase)
    }

    fn settled_open(&self) -> bool {
        self.phase >= 1.0
    }
}

/// What the master's processor knows about a block when it arms or checks
/// a measurement.
#[derive(Clone, Copy, Debug)]
pub struct BlockCtx {
    /// The DSP sink with DSP on (`PlayMode::Full`).
    pub full: bool,
    pub sink: SinkChoice,
    /// A varispeed plan or stage runs (Keep pitch included).
    pub staged: bool,
    pub ratio: RateRatio,
    pub graph_rate: u32,
    /// The block's source index, frames.
    pub index: u64,
    pub frames: usize,
    /// The alignment delay, frames, and whether it is changing.
    pub delay: usize,
    pub delay_fading: bool,
    /// The chain's latency and its tail (latency included), output frames.
    pub chain_latency: usize,
    pub chain_tail: usize,
    /// Chains installed so far (a swap counts one), and a swap audible now
    /// (its crossfade or its fade through silence).
    pub chain_installs: u64,
    pub chain_swapping: bool,
    /// Blocks of the playing chain a late channel worker made silent (a
    /// chain warming up beside it, unheard, does not count; once a swap is
    /// heard, `chain_swapping` faults an `active` run anyway).
    pub late_blocks: u64,
}

/// The master processor's measurement state (audio thread).
#[derive(Debug)]
pub struct MasterMeasure {
    inj: Injector,
    mute: SourceMute,
    channels: usize,
    /// Scratch: the muted source (interleaved `f32`) and the active
    /// input (planar `f64`), [`MAX_BLOCK`] frames each, taken out while a
    /// block runs.
    pub mute_buf: Vec<f32>,
    pub planar: Vec<Vec<f64>>,
    armed: bool,
    /// What it armed with.
    ratio: RateRatio,
    graph_rate: u32,
    delay: usize,
    chain_latency: usize,
    chain_tail: usize,
    chain_installs: u64,
    late_blocks: u64,
    /// `active`: the keepalive after the chain.
    post: PostKeepalive,
    /// `bypass`: the file position of the next output frame.
    k_out: i64,
    /// The master's output frames so far (its count for `start_frame`).
    out_total: u64,
    /// The output rate it armed at.
    out_rate: u32,
}

impl MasterMeasure {
    pub fn new(slot: Arc<MeasureSlot>, channels: usize) -> Self {
        let ch = channels.max(1);
        Self {
            inj: Injector::new(slot, 0x6d65_6173_7572_6531),
            mute: SourceMute {
                phase: 1.0,
                step: 0.0,
            },
            channels: ch,
            mute_buf: vec![0.0; MAX_BLOCK * ch],
            planar: vec![vec![0.0; MAX_BLOCK]; ch],
            armed: false,
            ratio: RateRatio::ONE,
            graph_rate: 0,
            delay: 0,
            chain_latency: 0,
            chain_tail: 0,
            chain_installs: 0,
            late_blocks: 0,
            post: PostKeepalive::new(0x6b65_6570_616c_6976),
            k_out: 0,
            out_total: 0,
            out_rate: 0,
        }
    }

    /// An inert state (no slot anyone posts to): what a processor built
    /// without one runs.
    pub fn inert(channels: usize) -> Self {
        Self::new(Arc::new(MeasureSlot::default()), channels)
    }

    /// Nothing to do: no job and the source open.
    #[inline]
    pub fn idle(&self) -> bool {
        self.inj.posted().is_none() && self.mute.settled_open()
    }

    pub fn poll(&mut self) {
        if self.inj.poll() {
            self.armed = false;
            self.post.reset();
        }
    }

    /// The master's device and the frames after its injection point
    /// (`active`: the chain's tail).
    fn tail(&self, job: &MeasureJob) -> f64 {
        match job.chain {
            MeasureChain::Active => self.chain_tail.max(self.chain_latency) as f64,
            MeasureChain::Bypass => 0.0,
        }
    }

    /// Before a block: arm a waiting job, or check that an armed one can
    /// go on. Returns whether a measurement runs on this block (inject).
    pub fn begin_block(&mut self, b: BlockCtx) -> bool {
        let Some((job, di)) = self.inj.posted() else {
            self.set_mute_target(false, b.graph_rate);
            return false;
        };
        let dev = job.devices.get(di);
        let rate = b.graph_rate.max(1);
        let out_rate = b.ratio.output_rate(b.graph_rate).unwrap_or(b.graph_rate);
        if !self.armed {
            if job.ending() || job.release.load(Ordering::Acquire) {
                if let Some(d) = dev {
                    d.finish_unstarted();
                }
                self.finish(job, rate);
                return false;
            }
            // Arm on the first block the DSP path plays plainly.
            if !b.full
                || b.sink != SinkChoice::Dsp
                || b.delay_fading
                || b.chain_swapping
                || b.frames > MAX_BLOCK
            {
                return false;
            }
            if b.staged {
                job.set_fault(MeasureFault::Varispeed);
                return false;
            }
            // The rates the job was built for.
            if b.graph_rate != job.graph_rate || dev.is_some_and(|d| d.device_rate != out_rate) {
                job.set_fault(MeasureFault::Rate);
                return false;
            }
            self.armed = true;
            self.ratio = b.ratio;
            self.graph_rate = b.graph_rate;
            self.delay = b.delay;
            self.chain_latency = b.chain_latency;
            self.chain_tail = b.chain_tail;
            self.chain_installs = b.chain_installs;
            self.late_blocks = b.late_blocks;
            self.out_rate = out_rate;
            let s0 = b.index as i64 - b.delay as i64 + job.lead_graph as i64;
            job.s0.store(s0, Ordering::Release);
            if let Some(d) = dev {
                // Bypass: the lead-in starts with this block's first
                // output frame (`lead_graph` is a multiple of the ratio's
                // `down`, so its output length is exact).
                self.k_out = -(d.lead as i64);
                let chain = match job.chain {
                    MeasureChain::Active => b.chain_latency as f64,
                    MeasureChain::Bypass => 0.0,
                };
                d.chain_frames.store(chain.to_bits(), Ordering::Release);
                d.tail_frames
                    .store(self.tail(job).to_bits(), Ordering::Release);
            }
        } else {
            let fault = if !b.full || b.sink != SinkChoice::Dsp {
                MeasureFault::Path
            } else if b.staged {
                MeasureFault::Varispeed
            } else if b.ratio != self.ratio || b.graph_rate != self.graph_rate {
                MeasureFault::Rate
            } else if b.delay != self.delay || b.delay_fading {
                MeasureFault::Alignment
            } else if b.frames > MAX_BLOCK {
                MeasureFault::Block
            } else if dev.is_some_and(MeasureDevice::live) {
                self.chain_fault(job, &b)
            } else {
                MeasureFault::None
            };
            if fault != MeasureFault::None && !job.release.load(Ordering::Acquire) {
                job.set_fault(fault);
            }
        }
        let can_inject = b.full && b.sink == SinkChoice::Dsp && b.frames <= MAX_BLOCK && !b.staged;
        if !can_inject
            && let Some(d) = dev
            && !d.fed.load(Ordering::Relaxed)
        {
            // Nothing plays the file here any more: what was written
            // drains (the injection cut, not faded).
            if job.any_fault() == MeasureFault::None {
                job.set_fault(MeasureFault::Path);
            }
            d.fed_end.store(self.k_out, Ordering::Release);
            d.stim_over.store(true, Ordering::Release);
            d.fed.store(true, Ordering::Release);
            if d.started.load(Ordering::Relaxed) {
                d.set_last_frame(self.out_total as f64 + self.tail(job));
            } else {
                d.finish_unstarted();
            }
        }
        // The mute holds from arming until the release.
        let lift = job.release.load(Ordering::Acquire);
        self.set_mute_target(job.mute_others && !lift, rate);
        if lift && self.mute.settled_open() {
            self.finish(job, rate);
        }
        can_inject
    }

    /// While the run plays on the main device: its chain swapped (`active`:
    /// any swap, the file runs through it; `bypass`: one that moved its
    /// latency), or a channel worker missed its deadline (`active`: the
    /// chain played silence; `bypass` skips the chain).
    fn chain_fault(&self, job: &MeasureJob, b: &BlockCtx) -> MeasureFault {
        match job.chain {
            MeasureChain::Active => {
                if b.chain_installs != self.chain_installs || b.chain_swapping {
                    MeasureFault::ChainSwap
                } else if b.late_blocks != self.late_blocks {
                    MeasureFault::Worker
                } else {
                    MeasureFault::None
                }
            }
            MeasureChain::Bypass if b.chain_latency != self.chain_latency => {
                MeasureFault::ChainSwap
            }
            MeasureChain::Bypass => MeasureFault::None,
        }
    }

    fn finish(&mut self, job: &MeasureJob, rate: u32) {
        self.set_mute_target(false, rate);
        if self.mute.settled_open() {
            job.released.store(true, Ordering::Release);
        }
    }

    fn set_mute_target(&mut self, muted: bool, rate: u32) {
        let step = 1.0 / (MUTE_RAMP_MS * 1e-3 * f64::from(rate.max(1))).max(1.0);
        self.mute.step = if muted { -step } else { step };
    }

    /// Mute the source (interleaved `f32`) into `scratch` when the mute is
    /// down or moving. Returns whether `scratch` holds the block.
    pub fn mute_source(&mut self, input: &[f32], scratch: &mut [f32]) -> bool {
        let ch = self.channels;
        let moving = (self.mute.step < 0.0 && self.mute.phase > 0.0)
            || (self.mute.step > 0.0 && self.mute.phase < 1.0);
        if !moving && self.mute.settled_open() {
            return false;
        }
        if input.len() > scratch.len() {
            // Too large for the scratch (not reached: blocks are smaller).
            return false;
        }
        for (i, frame) in input.chunks_exact(ch).enumerate() {
            if moving {
                self.mute.phase = (self.mute.phase + self.mute.step).clamp(0.0, 1.0);
            }
            let g = self.mute.gain() as f32;
            for (c, x) in frame.iter().enumerate() {
                scratch[i * ch + c] = x * g;
            }
        }
        true
    }

    /// `active`: the chain's input for a block of delayed source `tap`
    /// (interleaved) whose first frame carries source index `first`, the
    /// file on the routed channels, into `planar`. Returns whether it
    /// injected (the input is `planar`) and whether it is `f32`-exact.
    pub fn active_input(
        &mut self,
        tap: &[f32],
        first: u64,
        planar: &mut [Vec<f64>],
    ) -> Option<bool> {
        if !self.armed {
            return None;
        }
        let (job, di) = self.inj.posted()?;
        if job.chain != MeasureChain::Active {
            return None;
        }
        let d = job.devices.get(di)?;
        if d.fed.load(Ordering::Relaxed) {
            return None;
        }
        let exact = job.f32_exact;
        let s0 = job.s0()?;
        let ch = self.channels;
        let n = tap.len() / ch;
        if n > MAX_BLOCK || planar.len() < ch {
            return None;
        }
        for (i, frame) in tap.chunks_exact(ch).enumerate() {
            for (c, x) in frame.iter().enumerate() {
                planar[c][i] = f64::from(*x);
            }
        }
        let k0 = first as i64 - s0 as i64;
        let starts_here = d.start_frame().is_none() && k0 <= 0 && k0 + n as i64 > 0;
        self.inj.fill_planar(planar, n, k0, true);
        let at_out = |j: i64| self.out_total as f64 + self.ratio.frames_out(0, j as usize) as f64;
        if starts_here {
            // File frame 0 is input frame `-k0` here; it leaves the chain
            // its latency later.
            let at = at_out(-k0) + self.chain_latency as f64;
            d.start_frame.store(at.to_bits(), Ordering::Release);
        }
        if let Some(end) = d.fed_end()
            && d.last_frame().is_none()
        {
            let j = (end - k0).clamp(0, n as i64);
            d.set_last_frame(at_out(j) + self.tail(job));
        }
        Some(exact)
    }

    /// `bypass`: after the chain, write the file on the routed channels of
    /// `out` (this block's output), with the master's digital volume
    /// `gains` (`unity`: settled at 1, exact).
    pub fn bypass_output(&mut self, out: &mut [i32], gains: [f64; 3], unity: bool, bits: u32) {
        if !self.armed {
            return;
        }
        let Some((job, di)) = self.inj.posted() else {
            return;
        };
        if job.chain != MeasureChain::Bypass {
            return;
        }
        let Some(d) = job.devices.get(di) else {
            return;
        };
        if d.fed.load(Ordering::Relaxed) {
            return;
        }
        let ch = self.channels;
        let n = out.len() / ch;
        let k0 = self.k_out;
        if d.start_frame().is_none() && k0 <= 0 && k0 + n as i64 > 0 {
            let at = self.out_total as f64 + (-k0) as f64;
            d.start_frame.store(at.to_bits(), Ordering::Release);
        }
        self.inj.fill_i32(out, ch, k0, gains, unity, bits);
        self.k_out += n as i64;
        if let Some(end) = d.fed_end() {
            let j = (end - k0).clamp(0, n as i64);
            d.set_last_frame(self.out_total as f64 + j as f64);
        }
    }

    /// `active`: the keepalive after the chain on the routed channels of
    /// this block's output ([`PostKeepalive`]), on the `bits` word.
    pub fn post_keepalive(&mut self, out: &mut [i32], bits: u32) {
        if !self.armed {
            return;
        }
        let Some((job, di)) = self.inj.posted() else {
            return;
        };
        let Some(d) = job.devices.get(di) else {
            return;
        };
        let rate = self.out_rate;
        self.post
            .add(job, d, out, self.channels, self.out_total, rate, bits);
    }

    /// Clamp the whole output at [`CEILING`] while the measurement runs on
    /// this device (from its first touched frame until it drained), either
    /// mode, whatever the chain's limiter does (a no-op under a limiter).
    pub fn clamp_output(&mut self, out: &mut [i32]) {
        if !self.armed {
            return;
        }
        let Some((job, di)) = self.inj.posted() else {
            return;
        };
        let Some(d) = job.devices.get(di) else {
            return;
        };
        if d.started.load(Ordering::Relaxed) && !d.done.load(Ordering::Relaxed) {
            d.count_clamps(clamp_i32(out));
        }
    }

    /// After a block of `frames` output frames (whatever played).
    pub fn end_block(&mut self, frames: usize) {
        self.out_total += frames as u64;
    }

    /// A DAC reading (see `EngineAudioProcessor::dac_read`): the start's
    /// time and the queue once the file's first frame is out, and the
    /// drain.
    pub fn dac_read(&mut self, delay: i64, time_ns: u64) {
        let Some(d) = self.inj.device() else {
            return;
        };
        let at_dac = self.out_total as f64 - delay as f64;
        d.note_reading(self.out_total as i64 - delay, time_ns);
        let Some(sf) = d.start_frame() else {
            return;
        };
        if d.start_time_ns.load(Ordering::Relaxed) != 0 || self.out_rate == 0 {
            return;
        }
        if sf > self.out_total as f64 {
            // Not written yet (active: still in the chain).
            return;
        }
        let t = time_ns as f64 + (sf - at_dac) * 1e9 / f64::from(self.out_rate);
        d.start_time_ns
            .store(t.max(1.0).round() as u64, Ordering::Release);
        d.queue_frames
            .store((delay as f64).to_bits(), Ordering::Release);
    }

    /// The stream restarted (a stop, a reopen, an underrun recovery): an
    /// armed measurement that still plays cannot keep its timing.
    pub fn interrupted(&mut self) {
        if self.armed
            && let Some(job) = self.inj.job()
            && !job.release.load(Ordering::Acquire)
            && job.devices.iter().any(|d| !d.done.load(Ordering::Relaxed))
        {
            job.set_fault(MeasureFault::Interrupted);
        }
    }
}

/// A follower's measurement state (its audio thread): the injector, the
/// bypass tap's scratch and its crossfade at the device output.
#[derive(Debug)]
pub struct FollowerMeasure {
    pub inj: Injector,
    /// The routed channels' stage output, taken around the chain
    /// (`bypass`), planar.
    pub tap: Vec<Vec<f64>>,
    pub tapped: usize,
    /// The tap's weight at the output (raised-cosine phase, 0..1) at the
    /// call's start, and its direction this call.
    xo: f64,
    xo_dir: f64,
    xo_step: f64,
    /// The stage output has carried the stimulus's last frame: the tap
    /// fades out.
    tap_ending: bool,
    /// `active`: the keepalive after the chain.
    post: PostKeepalive,
}

impl FollowerMeasure {
    pub fn new(slot: Arc<MeasureSlot>, channels: usize, max_frames: usize) -> Self {
        Self {
            inj: Injector::new(slot, 0x666f_6c6c_6f77_6572),
            tap: vec![vec![0.0; max_frames]; channels.max(1)],
            tapped: 0,
            xo: 0.0,
            xo_dir: 0.0,
            xo_step: 0.0,
            tap_ending: false,
            post: PostKeepalive::new(0x666b_6565_7061_6c76),
        }
    }

    /// Take a newly posted job.
    pub fn poll(&mut self) {
        if self.inj.poll() {
            self.xo = 0.0;
            self.xo_dir = 0.0;
            self.xo_step = 0.0;
            self.tap_ending = false;
            self.tapped = 0;
            self.post.reset();
        }
    }

    /// The job and this device's part, once the master armed: (job, device,
    /// s0).
    fn armed(&self) -> Option<(&MeasureJob, &MeasureDevice, u64)> {
        let (job, i) = self.inj.posted()?;
        let d = job.devices.get(i)?;
        Some((job, d, job.s0()?))
    }

    /// The device plays a measurement in `chain` mode and its output may
    /// still carry it (the start and the end are tracked): `s0`.
    pub fn tracking(&self) -> Option<(MeasureChain, u64)> {
        let (job, d, s0) = self.armed()?;
        d.last_frame().is_none().then_some((job.chain, s0))
    }

    /// The injection runs: the master armed, not fed yet. Faults the run
    /// (`Rate`, `Ratio`) when the follower's rates differ from the
    /// prepared ones; the injector then ends it.
    pub fn inject_s0(
        &self,
        graph_rate: u32,
        device_rate: u32,
        ratio: RateRatio,
    ) -> Option<(MeasureChain, u64)> {
        let (job, d, s0) = self.armed()?;
        if d.fed.load(Ordering::Relaxed) {
            return None;
        }
        if graph_rate != job.graph_rate || device_rate != d.device_rate {
            d.set_fault(MeasureFault::Rate);
        } else if job.chain == MeasureChain::Bypass && !ratio.is_unity() {
            d.set_fault(MeasureFault::Ratio);
        }
        Some((job.chain, s0))
    }

    /// The injection's end, once fed.
    pub fn fed_end(&self) -> Option<i64> {
        self.inj.device()?.fed_end()
    }

    /// Whether `channel` carries the file here.
    pub fn routed(&self, channel: usize) -> bool {
        self.inj
            .device()
            .is_some_and(|d| d.route_of(channel).is_some())
    }

    /// `bypass`: whether this call taps the stage output (`m` frames at
    /// `rate`), and the tap's start weight and direction for it.
    pub fn begin_tap(&mut self, rate: u32) -> bool {
        self.tapped = 0;
        let Some((MeasureChain::Bypass, _)) = self.tracking() else {
            self.xo = 0.0;
            self.xo_dir = 0.0;
            return false;
        };
        if self.xo_step == 0.0 {
            self.xo_step = 1.0 / (XFADE_MS * 1e-3 * f64::from(rate.max(1))).max(1.0);
        }
        self.xo_dir = if self.tap_ending { -1.0 } else { 1.0 };
        true
    }

    /// The tap's weight at the output on frame `f` of this call.
    #[inline]
    pub fn tap_weight(&self, f: usize) -> f64 {
        rc(self.xo + self.xo_dir * self.xo_step * (f + 1) as f64)
    }

    /// After the call's `m` tapped frames went out (`first..first + m`
    /// device frames): the tap's weight moves on, and when it faded out
    /// the device's last frame is known.
    pub fn end_tap(&mut self, m: usize, first: u64) {
        if self.tapped == 0 {
            return;
        }
        self.xo = (self.xo + self.xo_dir * self.xo_step * m as f64).clamp(0.0, 1.0);
        if self.tap_ending
            && self.xo <= 0.0
            && let Some(d) = self.inj.device()
        {
            d.set_last_frame((first + m as u64) as f64);
        }
    }

    /// The stimulus's last frame has left the stage (`bypass`: the tap
    /// fades out from the next call).
    pub fn note_tap_drained(&mut self) {
        self.tap_ending = true;
    }

    /// The device's part (for the end and the start).
    pub fn device(&self) -> Option<&MeasureDevice> {
        self.inj.device()
    }

    /// The ceiling applies to this device's whole output now (either
    /// mode, from its first touched frame until it drained).
    pub fn clamps_output(&self) -> bool {
        self.armed().is_some_and(|(_, d, _)| {
            d.started.load(Ordering::Relaxed) && !d.done.load(Ordering::Relaxed)
        })
    }

    /// Clamp `out` (interleaved) at [`CEILING`], counting.
    pub fn clamp_output(&self, out: &mut [i32]) {
        if let Some(d) = self.inj.device() {
            d.count_clamps(clamp_i32(out));
        }
    }

    /// Clamp one tapped sample at [`CEILING`], counting.
    pub fn ceiling(&self, x: f64) -> f64 {
        if x.abs() > CEILING {
            if let Some(d) = self.inj.device() {
                d.count_clamps(1);
            }
            x.clamp(-CEILING, CEILING)
        } else {
            x
        }
    }

    /// `active`: the keepalive after the chain on the routed channels of
    /// `out` (interleaved, `channels`; device frames from `first`, at
    /// `rate`), on the `bits` word ([`PostKeepalive`]).
    pub fn post_keepalive(
        &mut self,
        out: &mut [i32],
        channels: usize,
        first: u64,
        rate: u32,
        bits: u32,
    ) {
        let Some((job, i)) = self.inj.posted() else {
            return;
        };
        if job.s0().is_none() {
            return;
        }
        let Some(d) = job.devices.get(i) else {
            return;
        };
        self.post.add(job, d, out, channels, first, rate, bits);
    }

    /// The device's chain is swapped now (`latency_changed`: to one of
    /// another latency): a fault while the run plays here (`active`: any
    /// swap, the file runs through the chain; `bypass`: the tap skips it,
    /// but a latency change moves where the file is placed).
    pub fn chain_swapped(&self, latency_changed: bool) {
        if let Some((job, d, _)) = self.armed()
            && d.live()
            && !job.release.load(Ordering::Relaxed)
            && (job.chain == MeasureChain::Active || latency_changed)
        {
            d.set_fault(MeasureFault::ChainSwap);
        }
    }

    /// A fault on this device's part.
    pub fn fault(&self, f: MeasureFault) {
        if let Some(d) = self.inj.device() {
            d.set_fault(f);
        }
    }

    /// The device's stream restarted (an xrun): the file's timing is lost.
    pub fn interrupted(&self) {
        if let Some(d) = self.inj.device()
            && d.started.load(Ordering::Relaxed)
            && !d.done.load(Ordering::Relaxed)
        {
            d.set_fault(MeasureFault::Interrupted);
        }
    }
}

#[cfg(test)]
#[path = "measure_tests.rs"]
mod processor_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn job(
        rate: u32,
        chain: MeasureChain,
        keepalive: f64,
        lead: u64,
        samples: Vec<f64>,
    ) -> Arc<MeasureJob> {
        Arc::new(MeasureJob::new(
            chain,
            true,
            keepalive,
            lead,
            rate,
            vec![MeasureDevice::new(
                None,
                rate,
                rate,
                vec![(1, samples)],
                lead,
            )],
        ))
    }

    /// The audio side never drops a job: it hands its box back, and the
    /// control side's `reclaim` lets go of it.
    #[test]
    fn the_slot_hands_a_job_over_and_back_without_freeing_on_the_audio_side() {
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        assert!(!inj.poll());
        let j = job(48_000, MeasureChain::Bypass, 0.0, 4, vec![0.5; 8]);
        slot.post(Some((Arc::clone(&j), 0)));
        assert!(inj.poll());
        assert!(inj.playing());
        assert_eq!(Arc::strong_count(&j), 2);
        slot.post(None);
        assert!(inj.poll());
        assert!(inj.job().is_none());
        // Handed back, not freed.
        assert_eq!(Arc::strong_count(&j), 2);
        slot.reclaim();
        assert_eq!(Arc::strong_count(&j), 1);
        // A post the audio side never took is freed by the next one.
        let k = job(48_000, MeasureChain::Bypass, 0.0, 4, vec![0.5; 8]);
        slot.post(Some((Arc::clone(&k), 0)));
        slot.post(None);
        assert_eq!(Arc::strong_count(&k), 1);
    }

    /// With every outbox entry taken, the audio side keeps its job and
    /// takes the new one after the control side reclaimed.
    #[test]
    fn a_full_outbox_defers_the_handover() {
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        for _ in 0..OUTBOX + 1 {
            // Fill the outbox by hand, as a control side that never
            // reclaims would leave it.
            slot.inbox.store(
                Box::into_raw(Box::new(Some((
                    job(48_000, MeasureChain::Bypass, 0.0, 4, vec![0.5; 8]),
                    0,
                )))),
                Ordering::Release,
            );
            inj.poll();
        }
        let j = job(48_000, MeasureChain::Bypass, 0.0, 4, vec![0.5; 8]);
        let raw = Box::into_raw(Box::new(Some((Arc::clone(&j), 0))));
        slot.inbox.store(raw, Ordering::Release);
        assert!(!inj.poll(), "no room to hand the old box back");
        slot.reclaim();
        assert!(inj.poll());
        assert!(ptr::eq(inj.job().unwrap(), &*j));
    }

    #[test]
    fn the_file_lands_on_the_routed_channel_after_the_lead_in_crossfaded() {
        // At 1 kHz the crossfade is 10 frames.
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        let file: Vec<f64> = (0..6).map(|i| (i as f64 + 1.0) / 8.0).collect();
        let j = job(1_000, MeasureChain::Bypass, 0.0, 20, file.clone());
        slot.post(Some((Arc::clone(&j), 0)));
        inj.poll();
        let src = 1 << 28;
        let mut out = vec![src; 2 * 40];
        inj.fill_i32(&mut out, 2, -20, [1.0; 3], true, 32);
        let at = |f: usize| out[2 * f + 1];
        for f in 0..40 {
            assert_eq!(out[2 * f], src, "channel 0 untouched");
        }
        // In: from the source to the lead-in's silence, smoothly.
        assert!((0..9).all(|f| at(f) < src && at(f) > 0));
        assert!((1..10).all(|f| at(f) < at(f - 1)));
        assert!((9..20).all(|f| at(f) == 0));
        for (k, v) in file.iter().enumerate() {
            assert_eq!(at(20 + k), (v * 2_147_483_648.0) as i32, "file frame {k}");
        }
        // Out: back to the source after the file.
        assert!((27..36).all(|f| at(f) > at(f - 1)));
        assert!((36..40).all(|f| at(f) == src));
        let d = &j.devices[0];
        assert!(d.fed.load(Ordering::Relaxed));
        assert_eq!(d.fed_end(), Some(16));
        assert_eq!(d.clamped.load(Ordering::Relaxed), 0);
    }

    /// Nothing the measurement adds goes above the ceiling.
    #[test]
    fn the_injection_is_clamped_at_the_ceiling() {
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        let j = job(1_000, MeasureChain::Bypass, 0.0, 20, vec![0.99; 10]);
        slot.post(Some((Arc::clone(&j), 0)));
        inj.poll();
        let mut out = vec![0; 2 * 30];
        inj.fill_i32(&mut out, 2, -20, [1.0; 3], true, 32);
        let ceiling = (CEILING * 2_147_483_648.0).round() as i32;
        assert!(out.iter().all(|x| x.abs() <= ceiling));
        assert_eq!(out[2 * 20 + 1], ceiling);
        assert_eq!(j.devices[0].clamped.load(Ordering::Relaxed), 10);
        // Planar too.
        let j = job(1_000, MeasureChain::Active, 0.0, 20, vec![-0.99; 10]);
        slot.post(Some((Arc::clone(&j), 0)));
        inj.poll();
        let mut chans = vec![vec![0.0; 30]; 2];
        inj.fill_planar(&mut chans, 30, -20, true);
        assert!(chans[1].iter().all(|x| x.abs() <= CEILING));
        assert_eq!(j.devices[0].clamped.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn the_keepalive_plays_from_the_lead_in_at_its_level() {
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        let ka = 1e-5; // -100 dBFS
        let n = 48_000;
        // (`bypass`: a follower's injection at its stage input; `active`
        // plays it after the chain instead.)
        slot.post(Some((
            job(48_000, MeasureChain::Bypass, ka, n as u64, vec![0.0; n]),
            0,
        )));
        inj.poll();
        let mut chans = vec![vec![1.0; 2 * n], vec![0.0; 2 * n]];
        inj.fill_planar(&mut chans, 2 * n, -(n as i64), true);
        let rms = |x: &[f64]| (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt();
        // Past the crossfade in (480 frames).
        let lead = rms(&chans[1][480..n]);
        let during = rms(&chans[1][n..]);
        assert!((lead / ka - 1.0).abs() < 0.02, "lead-in rms {lead}");
        assert!(
            (during / ka - 1.0).abs() < 0.02,
            "rms under the file {during}"
        );
        assert!(chans[0].iter().all(|v| *v == 1.0));
    }

    #[test]
    fn a_stop_fades_the_file_out_then_hands_the_channel_back() {
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        let j = job(48_000, MeasureChain::Active, 0.0, 1000, vec![0.5; 48_000]);
        slot.post(Some((Arc::clone(&j), 0)));
        inj.poll();
        let mut chans = vec![vec![0.0; 2000]; 2];
        inj.fill_planar(&mut chans, 2000, -1000, true);
        assert!(chans[1][1000..].iter().all(|v| *v == 0.5));
        j.cancel.store(true, Ordering::Release);
        let mut chans = vec![vec![0.25; 1000]; 2];
        inj.fill_planar(&mut chans, 1000, 1000, true);
        let x = &chans[1];
        assert!(x[0] < 0.5 && x[0] > 0.49);
        assert!(x[..240].windows(2).all(|w| w[1] <= w[0]));
        assert_eq!(x[239], 0.0, "5 ms at 48 kHz");
        // Then 10 ms back to what the channel carried.
        assert!(x[240..720].windows(2).all(|w| w[1] >= w[0]));
        assert!(x[720..].iter().all(|v| *v == 0.25));
        let d = &j.devices[0];
        assert!(d.fed.load(Ordering::Relaxed));
        assert!(!d.done.load(Ordering::Relaxed), "done only once drained");
    }

    /// A run that ends before a device touched a frame is over there.
    #[test]
    fn a_stop_before_the_lead_in_finishes_at_once() {
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        let j = job(48_000, MeasureChain::Active, 0.0, 100, vec![0.5; 1000]);
        slot.post(Some((Arc::clone(&j), 0)));
        inj.poll();
        j.cancel.store(true, Ordering::Release);
        let mut chans = vec![vec![0.25; 100]; 2];
        inj.fill_planar(&mut chans, 100, -150, true);
        assert!(chans[1].iter().all(|v| *v == 0.25));
        assert!(j.devices[0].done.load(Ordering::Relaxed));
    }

    #[test]
    fn a_follower_out_of_sync_faults() {
        let slot = Arc::new(MeasureSlot::default());
        let mut inj = Injector::new(Arc::clone(&slot), 1);
        let j = job(48_000, MeasureChain::Active, 0.0, 10, vec![0.5; 100]);
        slot.post(Some((Arc::clone(&j), 0)));
        inj.poll();
        let mut chans = vec![vec![0.0; 10]; 2];
        // Before the window: no fault.
        inj.fill_planar(&mut chans, 10, -20, false);
        assert_eq!(j.any_fault(), MeasureFault::None);
        inj.fill_planar(&mut chans, 10, -10, false);
        assert_eq!(j.any_fault(), MeasureFault::LostSync);
    }

    /// The drain: done once the DAC played past the last frame.
    #[test]
    fn done_waits_for_the_dac() {
        let d = MeasureDevice::new(None, 48_000, 48_000, vec![(0, vec![0.0; 4])], 0);
        d.note_played(1e9);
        assert!(!d.done.load(Ordering::Relaxed), "no last frame yet");
        d.set_last_frame(1000.0);
        d.set_last_frame(5.0);
        assert_eq!(d.last_frame(), Some(1000.0), "set once");
        d.note_played(999.0);
        assert!(!d.done.load(Ordering::Relaxed));
        d.note_played(1000.0);
        assert!(d.done.load(Ordering::Relaxed));
    }
}
