//! What the master's RT thread gives each follower, built in the processor
//! before the master's varispeed stage.
//!
//! - **The tee** ([`tee_ring`]): each follower gets a single-producer,
//!   single-consumer ring of the source frames (`f64`, with their source
//!   index). The master never waits on it: a full ring drops the block, and
//!   the reader sees the gap in the indices ([`TeeChunk::gap`]).
//! - **The master's position at the DAC** ([`MasterPositionSlot`]): each DAC
//!   reading is mapped through a [`PositionMap`] to the source position it
//!   carries, published with its time, the applied speed and whether the
//!   stage ramps, through a seqlock.
//! - **The command log** ([`CommandLog`]): each varispeed command with the
//!   source position of the call it starts at, so a follower's stage starts
//!   the same command at the same point of the source.
//! - **The alignment delay** ([`AlignDelay`]): whole source frames in front
//!   of the master's stage, at most [`MAX_ALIGN_MS`], changed under a
//!   cross-fade.
//! - **Per-device slots** ([`FollowerSlots`]): keyed by device, each with the
//!   chain config its own chain instance is built from.
//!
//! **The source index** counts the source frames the processor has been
//! handed, across every block and mode, since it was built. It never
//! restarts: one domain for the tee, the map and the log across resets and
//! reopens.
//!
//! **Keep pitch**: the stretch runs before the
//! tee, so all of it takes stretched frames, and a block without a stage
//! maps as one (the log says `Off`). A toggle restarts the map and the epoch
//! like a new stream ([`TeeRt::new_stream`]), and followers re-sync.
//!
//! **Real time.** Everything is allocated off the RT thread ([`TeeHost`])
//! and arrives through a queue ([`TeeCmd`]); retired parts leave through
//! another ([`TeeRetired`]) and are never freed on the RT thread. With no
//! follower attached the processor only counts source frames: its output
//! and its cost are unchanged.

use super::processor::SharedVolume;
use audivane_dsp_core::drift::{FRAME, MAP_LEN, PositionMap, Reading};
use audivane_protocol::ipc::EngineConfig;
use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// Most followers.
pub const MAX_FOLLOWERS: usize = 3;
/// The most the master is delayed for alignment, ms: what a follower at
/// the default quantum needs (up to about 60 ms, three periods queued on
/// both sides) and the whole negative trim range on top.
pub const MAX_ALIGN_MS: f64 = 120.0;
/// The most chain latency the devices' compensation evens out, ms: the
/// master's in its alignment delay ([`TeeCmd::SetComp`]), a follower's
/// after its chain. A linear-phase room-correction filter of 64 k taps at
/// 48 kHz is about 680 ms.
pub const MAX_COMP_MS: f64 = 1000.0;
/// The longest a speed command waits for the followers, ms.
pub const MAX_LEAD_MS: f64 = 200.0;
/// A source sample at most this large (−80 dBFS) counts as silence for a
/// shrink of the alignment ([`TeeCmd::SetAlign`] `quiet`).
pub const QUIET_LEVEL: f32 = 1e-4;
/// The silence a shrink of the alignment waits for beyond the delay
/// line's own history, ms: the master's device queue (about 64 ms at the
/// default quantum) and the followers' read points behind the tee, so the
/// step the followers take for it lands in silence too.
pub const QUIET_MARGIN_MS: f64 = 150.0;
/// The highest graph rate the delay line and the rings are sized for.
pub const MAX_GRAPH_RATE: u32 = 768_000;
/// The cross-fade of an alignment change, and the fade-in after the delay
/// line's history restarts, ms.
pub const ALIGN_FADE_MS: f64 = 5.0;
/// The largest block the processor takes, frames (a varispeed call's
/// `2n + 1` source frames at n = 8192, with room to spare).
pub const MAX_BLOCK_FRAMES: usize = 32_768;
/// Entries the command log keeps for readers that fall behind.
pub const LOG_LEN: usize = 64;
/// Discontinuities a tee ring can flag before its reader catches up.
const MARKS: usize = 64;
/// Retired parts the RT side can hold while the recycle queue is full.
const STASH: usize = MAX_FOLLOWERS + 2;
/// Seqlock retries a reader makes before it gives up.
const READ_TRIES: usize = 64;

/// Frames of `ms` at `rate`.
fn ms_frames(ms: f64, rate: u32) -> usize {
    (ms.max(0.0) * 1e-3 * f64::from(rate)).round() as usize
}

/// The alignment cap at `rate`, frames.
pub fn max_align_frames(rate: u32) -> usize {
    ms_frames(MAX_ALIGN_MS, rate)
}

/// The compensation cap at `rate`, frames.
pub fn max_comp_frames(rate: u32) -> usize {
    ms_frames(MAX_COMP_MS, rate)
}

/// The most a follower plays late beyond the alignment, ms: past the
/// alignment's cap it plays unaligned.
pub const MAX_LAG_MS: f64 = 200.0;

/// Frames a follower's ring holds: the largest alignment and lateness, a
/// master block and four follower periods of 8192, at [`MAX_GRAPH_RATE`]
/// for the alignment and the lateness (fixed at construction).
pub fn ring_frames() -> usize {
    max_align_frames(MAX_GRAPH_RATE)
        + ms_frames(MAX_LAG_MS, MAX_GRAPH_RATE)
        + MAX_BLOCK_FRAMES
        + 4 * 8192
}

/// `x` source frames in 64.64.
fn fixed(x: f64) -> i128 {
    (x * FRAME as f64).round() as i128
}

// ---------------------------------------------------------------------------
// The tee ring.

/// Where the reader's index jumps: the frame `at` of the sample stream
/// carries source index `index`.
#[derive(Clone, Copy, Debug)]
struct TeeMark {
    at: u64,
    index: u64,
}

/// What a ring's writer dropped (the reader's status).
#[derive(Debug, Default)]
pub struct TeeStats {
    dropped_frames: AtomicU64,
    dropped_blocks: AtomicU64,
}

impl TeeStats {
    /// Source frames dropped because the ring was full.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// Blocks dropped.
    pub fn dropped_blocks(&self) -> u64 {
        self.dropped_blocks.load(Ordering::Relaxed)
    }
}

/// The master's end of a follower's ring (RT, the processor).
pub struct TeeWriter {
    samples: rtrb::Producer<f64>,
    marks: rtrb::Producer<TeeMark>,
    channels: usize,
    /// Frames pushed into the sample stream.
    written: u64,
    /// The index the next frame carries when it follows on (`None`: the
    /// next write starts with a mark).
    next_index: Option<u64>,
    stats: Arc<TeeStats>,
}

/// The follower's end of its ring.
pub struct TeeReader {
    samples: rtrb::Consumer<f64>,
    marks: rtrb::Consumer<TeeMark>,
    channels: usize,
    /// Frames taken from the sample stream.
    read: u64,
    /// The index of the next frame (`None` before the first mark).
    index: Option<u64>,
    /// A gap was crossed and not yet reported with frames.
    gap: bool,
    stats: Arc<TeeStats>,
}

/// One read from a tee ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TeeChunk {
    /// The source index of the first frame read.
    pub index: u64,
    /// Frames read (contiguous source frames).
    pub frames: usize,
    /// Frames were dropped before these (the follower resyncs by index).
    pub gap: bool,
}

/// A ring for `channels` channels holding `frames` source frames
/// (allocates: off the RT thread).
pub fn tee_ring(channels: usize, frames: usize) -> (TeeWriter, TeeReader) {
    let channels = channels.max(1);
    let (sp, sc) = rtrb::RingBuffer::new(frames.max(1) * channels);
    let (mp, mc) = rtrb::RingBuffer::new(MARKS);
    let stats = Arc::new(TeeStats::default());
    (
        TeeWriter {
            samples: sp,
            marks: mp,
            channels,
            written: 0,
            next_index: None,
            stats: Arc::clone(&stats),
        },
        TeeReader {
            samples: sc,
            marks: mc,
            channels,
            read: 0,
            index: None,
            gap: false,
            stats,
        },
    )
}

impl TeeWriter {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Append `block` (interleaved, this ring's channel count) carrying
    /// source indices from `index`. Never waits: when the ring has no room
    /// for the whole block, the block is dropped and counted. Returns
    /// whether it was written.
    pub fn write(&mut self, index: u64, block: &[f32]) -> bool {
        let ch = self.channels;
        let frames = block.len() / ch;
        if frames == 0 {
            return true;
        }
        let need_mark = self.next_index != Some(index);
        if self.samples.slots() < frames * ch || (need_mark && self.marks.slots() == 0) {
            self.next_index = None;
            self.stats
                .dropped_frames
                .fetch_add(frames as u64, Ordering::Relaxed);
            self.stats.dropped_blocks.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if need_mark {
            // The mark goes first: a reader that sees these frames sees it.
            let _ = self.marks.push(TeeMark {
                at: self.written,
                index,
            });
        }
        if let Ok(chunk) = self.samples.write_chunk_uninit(frames * ch) {
            chunk.fill_from_iter(block[..frames * ch].iter().map(|&x| f64::from(x)));
        }
        self.written += frames as u64;
        self.next_index = Some(index + frames as u64);
        true
    }
}

impl TeeReader {
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn stats(&self) -> &TeeStats {
        &self.stats
    }

    /// Frames waiting in the ring.
    pub fn available(&self) -> usize {
        self.samples.slots() / self.channels
    }

    /// Take the marks due at the reader's position.
    fn take_marks(&mut self) {
        while let Ok(m) = self.marks.peek() {
            if m.at != self.read {
                break;
            }
            let m = *m;
            let _ = self.marks.pop();
            if self.index.is_some_and(|i| i != m.index) {
                self.gap = true;
            }
            self.index = Some(m.index);
        }
    }

    /// Read up to `out[c].len()` contiguous source frames into the planar
    /// `out` (one slice per channel, at least this ring's channel count;
    /// the shortest sets the count). `None` when nothing is waiting.
    pub fn read(&mut self, out: &mut [&mut [f64]]) -> Option<TeeChunk> {
        let ch = self.channels;
        if out.len() < ch {
            return None;
        }
        // The samples first, then the marks: a mark is pushed before its
        // frames, so every mark among the frames seen is seen too.
        let avail = self.samples.slots() / ch;
        self.take_marks();
        let index = self.index?;
        let mut frames = avail.min(out[..ch].iter().map(|o| o.len()).min().unwrap_or(0));
        if let Ok(m) = self.marks.peek() {
            frames = frames.min((m.at - self.read) as usize);
        }
        if frames == 0 {
            return None;
        }
        let chunk = self.samples.read_chunk(frames * ch).ok()?;
        let (a, b) = chunk.as_slices();
        for (i, &x) in a.iter().chain(b.iter()).enumerate() {
            out[i % ch][i / ch] = x;
        }
        chunk.commit_all();
        self.read += frames as u64;
        self.index = Some(index + frames as u64);
        let gap = std::mem::take(&mut self.gap);
        Some(TeeChunk { index, frames, gap })
    }
}

// ---------------------------------------------------------------------------
// The master's position at the DAC.

/// The master's position at its DAC, as published.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasterPoint {
    /// The source position at the DAC (64.64 source frames, the tee's
    /// index domain).
    pub pos: i128,
    /// When the DAC was read, ns (`CLOCK_MONOTONIC`).
    pub time_ns: i64,
    /// The applied speed (1 without a stage).
    pub speed: f64,
    /// The stage ramps (a follower's loop holds).
    pub ramping: bool,
    /// Bumped at every discontinuity of the master's path (reset, reopen,
    /// a block that played something else): positions of different epochs
    /// do not follow on.
    pub epoch: u64,
    /// The master's known steps up to the frame at the DAC
    /// ([`TeeShared::steps`]); inside a block that steps, the total before
    /// it (the map interpolates across that block: a follower holds).
    pub shift: i64,
}

/// A seqlock slot the master's RT thread writes at every DAC reading and
/// followers read without waiting (followers are RT threads: a read that
/// keeps meeting the writer gives up).
#[derive(Debug, Default)]
pub struct MasterPositionSlot {
    seq: AtomicU64,
    pos_hi: AtomicU64,
    pos_lo: AtomicU64,
    time: AtomicU64,
    speed: AtomicU64,
    shift: AtomicU64,
    /// Bit 0: valid; bit 1: ramping; the epoch above.
    flags: AtomicU64,
}

impl MasterPositionSlot {
    fn write(&self, pos: i128, time_ns: i64, speed: f64, shift: i64, flags: u64) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        let bits = pos as u128;
        self.pos_hi.store((bits >> 64) as u64, Ordering::Relaxed);
        self.pos_lo.store(bits as u64, Ordering::Relaxed);
        self.time.store(time_ns as u64, Ordering::Relaxed);
        self.speed.store(speed.to_bits(), Ordering::Relaxed);
        self.shift.store(shift as u64, Ordering::Relaxed);
        self.flags.store(flags, Ordering::Relaxed);
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Publish a position (RT, the single writer).
    pub fn publish(&self, p: MasterPoint) {
        let flags = 1 | (u64::from(p.ramping) << 1) | (p.epoch << 2);
        self.write(p.pos, p.time_ns, p.speed, p.shift, flags);
    }

    /// No position (RT): the master plays nothing a follower can follow.
    pub fn invalidate(&self, epoch: u64) {
        self.write(0, 0, 1.0, 0, epoch << 2);
    }

    /// The last position published, or `None` when there is none valid (or
    /// the writer kept the read from settling).
    pub fn read(&self) -> Option<MasterPoint> {
        for _ in 0..READ_TRIES {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let hi = self.pos_hi.load(Ordering::Relaxed);
            let lo = self.pos_lo.load(Ordering::Relaxed);
            let time = self.time.load(Ordering::Relaxed);
            let speed = self.speed.load(Ordering::Relaxed);
            let shift = self.shift.load(Ordering::Relaxed);
            let flags = self.flags.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) != s1 {
                continue;
            }
            if flags & 1 == 0 {
                return None;
            }
            return Some(MasterPoint {
                pos: ((u128::from(hi) << 64) | u128::from(lo)) as i128,
                time_ns: time as i64,
                speed: f64::from_bits(speed),
                ramping: flags & 2 != 0,
                epoch: flags >> 2,
                shift: shift as i64,
            });
        }
        None
    }

    /// The epoch of the last write (valid or not), for status.
    pub fn epoch(&self) -> u64 {
        self.flags.load(Ordering::Relaxed) >> 2
    }
}

// ---------------------------------------------------------------------------
// The command log.

/// A command the master's stage was given, as a follower's stage replays
/// it (the same calls on `dsp_core::varispeed::Varispeed`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VsCommand {
    /// `set_speed(speed, ramp_ms)`.
    Speed { speed: f64, ramp_ms: f64 },
    /// `set_speed_limit(max)` (the cap the stage applied).
    Limit(f64),
    /// `set_speed_limit(limit)`, `set_speed(speed, ramp_ms)`, `reset()`: a
    /// restart with no history, holding at the capped speed. Every earlier
    /// command is superseded.
    Reset {
        speed: f64,
        limit: f64,
        ramp_ms: f64,
    },
    /// The master's stage retired: from here the source plays at unity,
    /// with no stage. Every earlier command is superseded.
    Off,
    /// Commands were lost before this point (more than the stage's buffer
    /// between two calls): a follower resyncs from the snapshot
    /// ([`PlaybackStateSlot`]).
    Lost,
}

impl VsCommand {
    fn encode(self) -> [u64; 4] {
        match self {
            Self::Speed { speed, ramp_ms } => [0, speed.to_bits(), ramp_ms.to_bits(), 0],
            Self::Limit(max) => [1, max.to_bits(), 0, 0],
            Self::Reset {
                speed,
                limit,
                ramp_ms,
            } => [2, speed.to_bits(), limit.to_bits(), ramp_ms.to_bits()],
            Self::Off => [4, 0, 0, 0],
            Self::Lost => [3, 0, 0, 0],
        }
    }

    fn decode([kind, a, b, c]: [u64; 4]) -> Self {
        let (a, b, c) = (f64::from_bits(a), f64::from_bits(b), f64::from_bits(c));
        match kind {
            0 => Self::Speed {
                speed: a,
                ramp_ms: b,
            },
            1 => Self::Limit(a),
            2 => Self::Reset {
                speed: a,
                limit: b,
                ramp_ms: c,
            },
            4 => Self::Off,
            _ => Self::Lost,
        }
    }
}

/// One entry of the log.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoggedCommand {
    /// Its place in the log (0, 1, 2, ...).
    pub seq: u64,
    /// The source position (64.64, the tee's index domain, the stage's
    /// filter delays excluded) of the first output of the stage call the
    /// command starts at.
    pub at: i128,
    pub cmd: VsCommand,
}

/// What [`CommandLog::get`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LogRead {
    Entry(LoggedCommand),
    /// Not written yet.
    NotYet,
    /// Overwritten: the reader fell more than [`LOG_LEN`] behind.
    Lost,
}

#[derive(Debug, Default)]
struct LogSlot {
    /// `2 n + 1` while entry `n` is written, `2 n + 2` once it is.
    stamp: AtomicU64,
    at_hi: AtomicU64,
    at_lo: AtomicU64,
    words: [AtomicU64; 4],
}

impl LogSlot {
    /// Write `(at, cmd)` with the seqlock stamps `odd` then `odd + 1`.
    fn store(&self, odd: u64, at: i128, cmd: VsCommand) {
        self.stamp.store(odd, Ordering::Relaxed);
        fence(Ordering::Release);
        let bits = at as u128;
        self.at_hi.store((bits >> 64) as u64, Ordering::Relaxed);
        self.at_lo.store(bits as u64, Ordering::Relaxed);
        for (w, x) in self.words.iter().zip(cmd.encode()) {
            w.store(x, Ordering::Relaxed);
        }
        self.stamp.store(odd + 1, Ordering::Release);
    }

    /// Read it: the stamp seen and the entry, or `None` when a write got in
    /// the way.
    fn load(&self) -> (u64, Option<(i128, VsCommand)>) {
        let s1 = self.stamp.load(Ordering::Acquire);
        if s1 & 1 == 1 {
            return (s1, None);
        }
        let hi = self.at_hi.load(Ordering::Relaxed);
        let lo = self.at_lo.load(Ordering::Relaxed);
        let words = std::array::from_fn(|i| self.words[i].load(Ordering::Relaxed));
        fence(Ordering::Acquire);
        if self.stamp.load(Ordering::Relaxed) != s1 {
            return (s1, None);
        }
        let at = ((u128::from(hi) << 64) | u128::from(lo)) as i128;
        (s1, Some((at, VsCommand::decode(words))))
    }
}

/// A broadcast log: one writer (the master's RT thread), any number of
/// readers that never hold it up. Readers keep their own position.
#[derive(Debug)]
pub struct CommandLog {
    head: AtomicU64,
    slots: [LogSlot; LOG_LEN],
}

impl Default for CommandLog {
    fn default() -> Self {
        Self {
            head: AtomicU64::new(0),
            slots: std::array::from_fn(|_| LogSlot::default()),
        }
    }
}

impl CommandLog {
    /// Entries written so far (the next entry's `seq`).
    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// Append an entry (RT, the single writer).
    pub fn append(&self, at: i128, cmd: VsCommand) {
        let n = self.head.load(Ordering::Relaxed);
        self.slots[(n % LOG_LEN as u64) as usize].store(2 * n + 1, at, cmd);
        self.head.store(n + 1, Ordering::Release);
    }

    /// Entry `seq`.
    pub fn get(&self, seq: u64) -> LogRead {
        let slot = &self.slots[(seq % LOG_LEN as u64) as usize];
        let want = 2 * seq + 2;
        for _ in 0..READ_TRIES {
            match slot.load() {
                (s1, _) if s1 < want - 1 => return LogRead::NotYet,
                (s1, _) if s1 > want => return LogRead::Lost,
                (s1, Some((at, cmd))) if s1 == want => {
                    return LogRead::Entry(LoggedCommand { seq, at, cmd });
                }
                _ => std::hint::spin_loop(),
            }
        }
        LogRead::Lost
    }
}

/// Where a follower starts or resyncs: the master's stage state as one
/// command (a [`VsCommand::Reset`] that leaves a stage in the state the
/// master's is in, or [`VsCommand::Off`]) at a source position, and the log
/// entry that follows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackState {
    /// The first log entry after the state (apply from here on).
    pub cursor: u64,
    /// The source position the state holds at (64.64, the log's domain):
    /// the master's next output frame. A stage reset there makes its first
    /// output at it, and from its second call on its positions are the
    /// master's. Entries for the master's next call carry that call's
    /// start, up to half a step earlier (its core runs one output ahead of
    /// a fresh stage's). Without a stage: the next block's first frame.
    pub at: i128,
    pub cmd: VsCommand,
}

/// The latest [`PlaybackState`], published by the master's RT thread after
/// every block that left its stage settled (no ramp, no command waiting,
/// no fade) or played without one; read without waiting. For a follower
/// attaching (the log only has what came after it) and for one that fell
/// more than [`LOG_LEN`] entries behind.
#[derive(Debug, Default)]
pub struct PlaybackStateSlot {
    cursor: AtomicU64,
    slot: LogSlot,
    writes: AtomicU64,
}

impl PlaybackStateSlot {
    /// Publish (RT, the single writer).
    pub fn publish(&self, state: PlaybackState) {
        let n = self.writes.load(Ordering::Relaxed);
        // The cursor goes inside the stamps too.
        self.slot.stamp.store(2 * n + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.cursor.store(state.cursor, Ordering::Relaxed);
        self.slot.store(2 * n + 1, state.at, state.cmd);
        self.writes.store(n + 1, Ordering::Release);
    }

    /// The latest state, `None` before the first (or when the writer kept
    /// the read from settling).
    pub fn read(&self) -> Option<PlaybackState> {
        for _ in 0..READ_TRIES {
            let s0 = self.slot.stamp.load(Ordering::Acquire);
            if s0 == 0 {
                return None;
            }
            let cursor = self.cursor.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            match self.slot.load() {
                (s1, Some((at, cmd))) if s1 == s0 => {
                    return Some(PlaybackState { cursor, at, cmd });
                }
                _ => std::hint::spin_loop(),
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// The alignment delay.

/// A delay line of whole source frames in front of the master's stage
/// (interleaved `f32`, as the processor's blocks).
///
/// - A change of the delay cross-fades from the old read point to the new
///   one over [`ALIGN_FADE_MS`].
/// - The history restarts when the DSP path stops taking blocks (Direct,
///   DSP off) or on a reset: frames older than the restart read as silence,
///   and the first real ones fade in, so no stale audio and no step plays.
/// - At a delay of zero (no fade) the block passes unchanged, bit for bit;
///   the history is still written so a later delay has it.
/// - It counts the silent frames at the end of its history
///   ([`Self::quiet_frames`]): a shrink of the alignment waits for them.
pub struct AlignDelay {
    channels: usize,
    /// Frames of history.
    cap: usize,
    buf: Vec<f32>,
    out: Vec<f32>,
    /// Frames written since the history restarted counting.
    w: u64,
    /// The first frame of real history (`None`: the next block restarts
    /// it).
    valid_from: Option<u64>,
    delay: usize,
    /// The delay being faded from, and how far the fade is.
    from: usize,
    fade_pos: usize,
    fade_len: usize,
    /// Silent frames ([`QUIET_LEVEL`]) at the end of the history
    /// (`u64::MAX` after a restart: the history reads as silence).
    quiet: u64,
}

impl AlignDelay {
    /// For `channels` channels, up to `max_delay` frames (allocates).
    pub fn new(channels: usize, max_delay: usize) -> Self {
        let channels = channels.max(1);
        let cap = max_delay + 1;
        Self {
            channels,
            cap,
            buf: vec![0.0; cap * channels],
            out: vec![0.0; MAX_BLOCK_FRAMES * channels],
            w: 0,
            valid_from: None,
            delay: 0,
            from: 0,
            fade_pos: 0,
            fade_len: 1,
            quiet: u64::MAX,
        }
    }

    /// Silent frames at the end of the history (saturating; `u64::MAX`
    /// right after a restart).
    pub fn quiet_frames(&self) -> u64 {
        self.quiet
    }

    /// Count `input`'s silent frames into [`Self::quiet`].
    fn note_quiet(&mut self, input: &[f32]) {
        let ch = self.channels;
        let frames = input.len() / ch;
        let last_loud = input
            .chunks_exact(ch)
            .rposition(|f| f.iter().any(|x| x.abs() > QUIET_LEVEL));
        self.quiet = match last_loud {
            None => self.quiet.saturating_add(frames as u64),
            Some(i) => (frames - 1 - i) as u64,
        };
    }

    /// The most frames it delays.
    pub fn max_delay(&self) -> usize {
        self.cap - 1
    }

    /// The delay applied once a fade in progress ends, frames.
    pub fn delay(&self) -> usize {
        self.delay
    }

    /// The most frames a block's output lags its input by now (the old
    /// delay during a cross-fade).
    pub fn held(&self) -> usize {
        self.delay.max(self.from)
    }

    /// The delay passes blocks unchanged.
    pub fn is_identity(&self) -> bool {
        self.delay == 0 && self.from == 0
    }

    /// Delay by `frames` (capped), cross-fading over `fade_len` frames.
    pub fn set_delay(&mut self, frames: usize, fade_len: usize) {
        let frames = frames.min(self.max_delay());
        if frames == self.delay {
            return;
        }
        // Mid-fade, the new fade starts from the delay the old one headed
        // for.
        self.from = self.delay;
        self.delay = frames;
        self.fade_len = fade_len.max(1);
        self.fade_pos = 0;
    }

    /// Restart the history (a reset): older frames read as silence.
    pub fn restart(&mut self) {
        self.valid_from = None;
        self.quiet = u64::MAX;
    }

    /// The gain of history frame `r`: 0 before the real history, then a
    /// fade-in.
    fn gain(&self, r: u64, vf: u64) -> f32 {
        if r < vf {
            return 0.0;
        }
        let k = (r - vf) as usize;
        if k >= self.fade_len {
            1.0
        } else {
            (0.5 - 0.5 * (PI * (k + 1) as f64 / self.fade_len as f64).cos()) as f32
        }
    }

    /// Run a block. `full`: the DSP path takes it (otherwise it passes
    /// unchanged and the history restarts at the next block that does).
    /// Returns the delayed block (or `input` itself).
    pub fn process<'a>(&'a mut self, input: &'a [f32], channels: usize, full: bool) -> &'a [f32] {
        let ch = self.channels;
        let frames = input.len() / ch;
        if !full || channels != ch || frames > MAX_BLOCK_FRAMES {
            self.restart();
            return input;
        }
        self.note_quiet(input);
        let vf = *self.valid_from.get_or_insert(self.w);
        let cap = self.cap as u64;
        if self.is_identity() {
            // Bit for bit, but the history keeps up.
            for (i, frame) in input.chunks_exact(ch).enumerate() {
                let at = ((self.w + i as u64) % cap) as usize * ch;
                self.buf[at..at + ch].copy_from_slice(frame);
            }
            self.w += frames as u64;
            return input;
        }
        for (i, frame) in input.chunks_exact(ch).enumerate() {
            let now = self.w;
            let at = (now % cap) as usize * ch;
            self.buf[at..at + ch].copy_from_slice(frame);
            let read = |d: usize, me: &Self| -> (usize, f32) {
                match now.checked_sub(d as u64) {
                    Some(r) => ((r % cap) as usize * ch, me.gain(r, vf)),
                    None => (0, 0.0),
                }
            };
            let (pn, gn) = read(self.delay, self);
            if self.from != self.delay {
                let (po, go) = read(self.from, self);
                self.fade_pos += 1;
                let t = self.fade_pos as f64 / self.fade_len as f64;
                let wn = (0.5 - 0.5 * (PI * t.min(1.0)).cos()) as f32;
                for c in 0..ch {
                    self.out[i * ch + c] =
                        (1.0 - wn) * go * self.buf[po + c] + wn * gn * self.buf[pn + c];
                }
                if self.fade_pos >= self.fade_len {
                    self.from = self.delay;
                }
            } else {
                for c in 0..ch {
                    self.out[i * ch + c] = gn * self.buf[pn + c];
                }
            }
            self.w += 1;
        }
        &self.out[..frames * ch]
    }
}

// ---------------------------------------------------------------------------
// The RT side.

/// What the control side sends the processor's tee slot.
pub enum TeeCmd {
    /// A follower's ring in slot `slot` (`< MAX_FOLLOWERS`).
    Attach { slot: usize, writer: Box<TeeWriter> },
    /// Stop writing slot `slot` (its writer retires).
    Detach { slot: usize },
    /// The delay line (replaces one installed).
    InstallDelay(Box<AlignDelay>),
    /// Fade the delay to zero, then retire the line.
    RemoveDelay,
    /// The master's alignment delay, ms (capped at [`MAX_ALIGN_MS`]).
    /// `quiet`: a shrink, applied only once the delay line's history has
    /// been silent for the longer of the two delays plus
    /// [`QUIET_MARGIN_MS`] (at once after a restart: a pause, a new
    /// stream), never during a measurement; until then the alignment holds
    /// and a later one replaces it. Without `quiet` it applies now and
    /// drops a shrink waiting.
    SetAlign { ms: f64, quiet: bool },
    /// The master's chain compensation, graph frames (capped at
    /// [`MAX_COMP_MS`]): on top of the alignment, in the same delay line,
    /// but a chain latency to the followers, not a step
    /// ([`TeeShared::comp_frames`]).
    SetComp { frames: usize },
}

/// What the processor's tee slot hands back to be freed off the RT thread.
pub enum TeeRetired {
    Writer(Box<TeeWriter>),
    Delay(Box<AlignDelay>),
}

/// The handles followers read (shared with the RT side).
#[derive(Clone, Debug, Default)]
pub struct TeeShared {
    pub position: Arc<MasterPositionSlot>,
    pub log: Arc<CommandLog>,
    pub state: Arc<PlaybackStateSlot>,
    /// The graph rate the master's processor runs at (0 before its first
    /// poll): a follower's stage and chain are built for it.
    pub graph_rate: Arc<AtomicU32>,
    /// Counts the changes of the master's alignment delay: the master's
    /// position steps by the change, which a follower re-acquires.
    pub align_seq: Arc<AtomicU64>,
    /// The master's alignment the RT side applies, ms (`f64` bits): a
    /// shrink waiting for silence shows here once it lands.
    pub align_ms: Arc<AtomicU64>,
    /// How long the master's stage holds a speed command back (ms, `f64`
    /// bits; set by the control side from the followers' leads), so each
    /// follower reads its log entry before its stage reaches it.
    pub command_lead_ms: Arc<AtomicU64>,
    /// Counts the changes of the master's chain latency (a chain swap of
    /// another latency): its position steps by the change, as a follower's
    /// does when it swaps its own; a follower holds its readings over both.
    pub chain_seq: Arc<AtomicU64>,
    /// The master's chain latency as it maps its blocks, graph frames
    /// (`f64` bits; `NaN` before its first block): a follower pads its own
    /// path to it ([`Self::comp_frames`] included).
    pub master_chain_graph: Arc<F64Cell>,
    /// The master's chain compensation in its delay line, graph frames
    /// (what applies, the alignment's history permitting): the delay that
    /// makes its path as long as a follower's longer chain. A change counts
    /// in [`Self::chain_seq`], not in [`Self::steps`]: a follower's own
    /// padding moves with it.
    pub comp_frames: Arc<AtomicU64>,
    /// The master's known steps so far (`i64` bits, 2^-32 source frames):
    /// the sum of the jumps of its position at the DAC that are not its
    /// chain's latency (an alignment change, a stage installed or retired),
    /// stored when the block that steps is processed, so a follower takes
    /// the same step at once ([`MasterPoint::shift`] says when it has
    /// reached the master's DAC).
    pub steps: Arc<AtomicU64>,
    /// Where the master's output jumped at its last known step: the source
    /// position (whole frames, `i64` bits) it would have played next, in
    /// the old mapping. Stored before [`Self::steps`] (release): a follower
    /// that sees the step takes it once its own read point is there, so
    /// the step lands at both DACs together.
    pub step_at: Arc<AtomicU64>,
    /// An alignment change the master has scheduled and not made yet: the
    /// source position (whole frames, [`encode_pos`]; 0: none) its output
    /// jumps at, at the earliest. A follower shuts before its read point
    /// gets there and stays shut until it has taken the step. Cleared
    /// after the step is published in [`Self::steps`].
    pub step_announce: Arc<AtomicU64>,
    /// The master's level as heard (set by the engine whenever it applies
    /// its volume): the effective volume, hardware part included (a
    /// follower's equivalent digital gain), balance and mute. A follower
    /// with `follow_volume` plays at it plus its own volume.
    pub master_level: Arc<SharedVolume>,
}

/// An `f64` shared between threads (`NaN` by default).
#[derive(Debug)]
pub struct F64Cell(AtomicU64);

impl Default for F64Cell {
    fn default() -> Self {
        Self(AtomicU64::new(f64::NAN.to_bits()))
    }
}

impl F64Cell {
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, x: f64) {
        self.0.store(x.to_bits(), Ordering::Relaxed);
    }
}

/// A source position (whole frames) as [`TeeShared::step_announce`]
/// holds it (never 0).
pub fn encode_pos(frames: i64) -> u64 {
    ((frames as u64) ^ (1 << 63)).max(1)
}

/// [`encode_pos`] back (`None`: 0, nothing announced).
pub fn decode_pos(v: u64) -> Option<i64> {
    (v != 0).then_some((v ^ (1 << 63)) as i64)
}

/// One unit of [`TeeShared::steps`], in 64.64 source frames.
pub const STEP_UNIT_SHIFT: u32 = 32;
/// A jump of the master's map smaller than this (64.64) is not a step (the
/// rounding between two blocks' ends).
const STEP_MIN: i128 = FRAME >> 10;

/// A step total in source frames.
pub fn step_frames(units: i64) -> f64 {
    units as f64 / (1u64 << STEP_UNIT_SHIFT) as f64
}

/// Where the stage was, for the master's map and the log (see
/// `VarispeedRt::tee_view`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageView {
    /// The core's position at its next output (64.64 frames on the ×2
    /// grid, from the stage's last reset).
    pub acc: u128,
    /// Source frames the stage has taken since its last reset.
    pub fed: u64,
    /// The slot's source look-ahead, frames.
    pub hold: usize,
    pub speed: f64,
    /// The stage's filter delay at its speed, source frames.
    pub delay_src: f64,
    pub ramping: bool,
    /// The stage's request (speed and ramp) and cap: with `ramping` false,
    /// the state a [`VsCommand::Reset`] reproduces.
    pub requested: f64,
    pub requested_ramp_ms: f64,
    /// The step at the core's next output (64.64 bits).
    pub step: u128,
    pub limit: f64,
    /// The slowest speed it plays at until it settles (no further
    /// command).
    pub slowest: f64,
    /// The fastest, a command waiting included.
    pub fastest: f64,
}

impl StageView {
    /// The command that leaves a fresh stage as this one is, when it is
    /// settled.
    pub fn settled_state(&self) -> Option<VsCommand> {
        (!self.ramping).then_some(VsCommand::Reset {
            speed: self.requested,
            limit: self.limit,
            ramp_ms: self.requested_ramp_ms,
        })
    }
}

/// One block's device frames `[start, end)` and the speed at their ends,
/// beside the map: a reading takes its speed and ramp state from the block
/// its frame belongs to, as its position.
#[derive(Clone, Copy, Debug, Default)]
struct Span {
    start: u64,
    end: u64,
    speed: (f64, f64),
    ramping: bool,
    /// The known steps before the block and after it (they differ when
    /// the block steps).
    shift: (i64, i64),
}

/// The tee's slot in the RT processor.
pub struct TeeRt {
    rx: Option<rtrb::Consumer<TeeCmd>>,
    recycle: Option<rtrb::Producer<TeeRetired>>,
    stash: [Option<TeeRetired>; STASH],
    outs: [Option<Box<TeeWriter>>; MAX_FOLLOWERS],
    active: bool,
    delay: Option<Box<AlignDelay>>,
    retire_delay: bool,
    align_ms: f64,
    /// A shrink of the alignment waiting for silence, ms; a measurement
    /// runs (it holds it).
    align_quiet: Option<f64>,
    measuring: bool,
    /// The alignment part of the delay applied (frames), and a change of
    /// it scheduled at a source index (announced: [`TeeShared::step_announce`]);
    /// the announcement goes once the step is published.
    align_now: usize,
    align_due: Option<(usize, u64)>,
    announce_clear: bool,
    /// The compensation asked for, and the part of the delay it has now
    /// (graph frames).
    comp_want: usize,
    comp_now: usize,
    rate: u32,
    /// The next block's source index.
    index: u64,
    map: PositionMap,
    /// Device frames the processor has output since the last reset.
    out: u64,
    epoch: u64,
    /// The map's blocks (newest last, `MAP_LEN` at most).
    spans: [Span; MAP_LEN],
    span_head: usize,
    span_len: usize,
    /// The speed the alignment cap is worked out at (in
    /// playback time, see [`Self::bound_speed`]), and the stage's slowest
    /// speed at the last block.
    cap_speed: f64,
    speed_now: f64,
    /// The stage's fastest speed until it settles, at the last block (a
    /// scheduled step's lead is playback time: source frames at it).
    speed_fast: f64,
    /// A stream started: the next block fits the cap to its speed (the
    /// prime has reset the stage at the speed the stream plays at by then).
    refit: bool,
    /// Source frames since the stage last ramped.
    settled_frames: usize,
    /// The last block on the DSP path ran the stage (its retirement logs
    /// [`VsCommand::Off`]).
    staged: bool,
    /// The chain's delay the last block was mapped with (graph frames).
    chain_graph: f64,
    /// The position after the last block without the chain's delay, and
    /// the known steps so far ([`TeeShared::steps`]).
    last_next: Option<i128>,
    /// The last block's end as mapped (the position its next frame would
    /// carry at the DAC, 64.64): a step is published there.
    last_next_map: i128,
    /// The last block's source frames (a scheduled change lands on a
    /// block start: blocks keep their size in steady play).
    last_block: usize,
    shift: i64,
    shared: TeeShared,
}

impl TeeRt {
    /// A slot that never gets a follower (no queues).
    pub fn inert() -> Self {
        Self::build(None, None, TeeShared::default())
    }

    pub fn new(
        rx: rtrb::Consumer<TeeCmd>,
        recycle: rtrb::Producer<TeeRetired>,
        shared: TeeShared,
    ) -> Self {
        Self::build(Some(rx), Some(recycle), shared)
    }

    fn build(
        rx: Option<rtrb::Consumer<TeeCmd>>,
        recycle: Option<rtrb::Producer<TeeRetired>>,
        shared: TeeShared,
    ) -> Self {
        Self {
            rx,
            recycle,
            stash: std::array::from_fn(|_| None),
            outs: std::array::from_fn(|_| None),
            active: false,
            delay: None,
            retire_delay: false,
            align_ms: 0.0,
            align_quiet: None,
            measuring: false,
            align_now: 0,
            align_due: None,
            announce_clear: false,
            comp_want: 0,
            comp_now: 0,
            rate: 48_000,
            index: 0,
            map: PositionMap::new(),
            out: 0,
            epoch: 0,
            spans: [Span::default(); MAP_LEN],
            span_head: 0,
            span_len: 0,
            cap_speed: 1.0,
            speed_now: 1.0,
            speed_fast: 1.0,
            refit: false,
            settled_frames: 0,
            staged: false,
            chain_graph: f64::NAN,
            last_next: None,
            last_next_map: 0,
            last_block: 0,
            shift: 0,
            shared,
        }
    }

    pub fn shared(&self) -> &TeeShared {
        &self.shared
    }

    /// A follower is attached: the processor tees, maps and logs.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Retired parts still held on the RT thread (tests).
    pub fn stash_len(&self) -> usize {
        self.stash.iter().filter(|s| s.is_some()).count()
    }

    fn flush_stash(&mut self) {
        let Some(tx) = self.recycle.as_mut() else {
            return;
        };
        for slot in &mut self.stash {
            if let Some(r) = slot.take()
                && let Err(rtrb::PushError::Full(r)) = tx.push(r)
            {
                *slot = Some(r);
                break;
            }
        }
    }

    fn can_retire(&self) -> bool {
        self.recycle.as_ref().is_some_and(|tx| tx.slots() > 0)
            || self.stash.iter().any(Option::is_none)
    }

    fn retire(&mut self, r: TeeRetired) {
        let r = match self.recycle.as_mut() {
            Some(tx) => match tx.push(r) {
                Ok(()) => return,
                Err(rtrb::PushError::Full(r)) => r,
            },
            None => r,
        };
        for slot in &mut self.stash {
            if slot.is_none() {
                *slot = Some(r);
                return;
            }
        }
        // Not reached (a command is taken only with room to retire).
        debug_assert!(false, "tee recycle queue and stash full");
        std::mem::forget(r);
    }

    /// The alignment at the running rate, frames: the request, at most
    /// [`MAX_ALIGN_MS`] of playback at the cap's speed (source frames
    /// drain at the speed, so at 0.5 the cap is half as many frames).
    fn align_frames(&self) -> usize {
        let cap = (max_align_frames(self.rate) as f64 * self.cap_speed.min(1.0)).floor() as usize;
        ms_frames(self.align_ms.min(MAX_ALIGN_MS), self.rate).min(cap)
    }

    /// The stage's slowest speed until it settles, and whether it ramps,
    /// before a block (1 and settled without a stage).
    ///
    /// The cap is fitted at a stream start (the first block after
    /// [`Self::new_stream`], at its speed: the prime has reset the stage by
    /// then) and refitted once the stage has been settled at 1.0 for
    /// the cap's worth of source (`frames`: this block's), so the longer
    /// delay never looks back over frames that played slower. Through the
    /// speed changes in between, the delay holds in source frames: a change
    /// would step the master's position under every follower (a skip and a
    /// re-acquisition). While slowed, the alignment therefore adds up to
    /// [`MAX_ALIGN_MS`] divided by the speed of playback.
    pub fn bound_speed(&mut self, slowest: f64, ramping: bool, frames: usize) {
        let s = if slowest.is_finite() && slowest > 0.0 {
            slowest.min(1.0)
        } else {
            1.0
        };
        self.speed_now = s;
        let settled = !ramping && self.settled_frames >= max_align_frames(self.rate);
        if std::mem::take(&mut self.refit) {
            if s != self.cap_speed {
                self.cap_speed = s;
                self.apply_align();
            }
        } else if s >= 1.0 && self.cap_speed < 1.0 && settled {
            self.cap_speed = 1.0;
            self.apply_align();
        }
        self.settled_frames = if ramping {
            0
        } else {
            self.settled_frames.saturating_add(frames)
        };
    }

    /// The stage's fastest speed until it settles, before a block (1
    /// without a stage).
    pub fn note_fastest(&mut self, fastest: f64) {
        self.speed_fast = if fastest.is_finite() && fastest > 0.0 {
            fastest
        } else {
            1.0
        };
    }

    /// A delay line is installed.
    #[inline]
    pub fn has_delay(&self) -> bool {
        self.delay.is_some()
    }

    fn apply_align(&mut self) {
        self.apply_align_with(false);
    }

    /// Apply the alignment and compensation asked for. A change of the
    /// alignment while followers follow the master's position (its map
    /// runs) is not made at once unless `immediate`: it is scheduled a
    /// command lead and two fades ahead (the followers' stages run at most
    /// their measured lead ahead of the master's) and announced, so each
    /// follower shuts before it gets there, then takes the step at the
    /// source position the master's output jumps at: the step lands at both
    /// DACs together, whatever each one queues.
    fn apply_align_with(&mut self, immediate: bool) {
        let (align, comp) = if self.retire_delay {
            (0, 0)
        } else {
            (
                self.align_frames(),
                self.comp_want.min(max_comp_frames(self.rate)),
            )
        };
        let timed = !immediate && !self.retire_delay && self.active && !self.map.is_empty();
        if align == self.align_now {
            if self.align_due.take().is_some() {
                self.announce_clear = true;
            }
        } else if !timed || self.delay.is_none() {
            self.align_now = align;
            if self.align_due.take().is_some() {
                self.announce_clear = true;
            }
        } else if self.align_due.map(|d| d.0) != Some(align) {
            // (A change while one waits keeps its place: announced already.)
            let at = self.align_due.map_or_else(
                || {
                    // The followers' lead and the fades are playback
                    // time: source frames at the fastest the stage plays
                    // until the step (twice as many at 2×).
                    let play = self.command_lead_frames() + 2 * ms_frames(ALIGN_FADE_MS, self.rate);
                    let lead = (play as f64 * self.speed_fast.max(self.speed_now)).ceil() as usize;
                    // On a block start (in whole blocks of the last one's
                    // size), so it lands where it is announced.
                    let n = self.last_block.max(1);
                    self.index + (lead.div_ceil(n) * n) as u64
                },
                |d| d.1,
            );
            let varying = self.speed_fast != 1.0 || self.speed_now != 1.0;
            let n_block = if varying { self.last_block.max(1) } else { 0 };
            self.align_due = Some((align, at));
            // As the step will be published: the position the master's
            // output carries at the DAC there, at the earliest. The source
            // runs that far past where its next frame is mapped now (the
            // delay, the stage's filter and what it holds, the chain);
            // off 1×, a block more (the stage pulls source in blocks of
            // varying size, the step lands within one) and the chain's
            // growth up to the fastest speed: a follower shuts a little
            // early, never late.
            let grow = if self.chain_graph.is_finite() {
                self.chain_graph * (self.speed_fast.max(self.speed_now) - self.speed_now).max(0.0)
            } else {
                0.0
            };
            let now = self.last_next_map.div_euclid(FRAME) as i64;
            let from =
                now + (at as i64 - self.index as i64) - n_block as i64 - grow.ceil() as i64 - 1;
            self.announce_clear = false;
            self.shared
                .step_announce
                .store(encode_pos(from), Ordering::Release);
        }
        let align = self.align_now;
        let fade = ms_frames(ALIGN_FADE_MS, self.rate).max(1);
        let mut comp_now = 0;
        if let Some(d) = self.delay.as_mut() {
            let frames = (align + comp).min(d.max_delay());
            comp_now = frames.saturating_sub(align);
            if d.delay() != frames {
                self.shared.align_seq.fetch_add(1, Ordering::Relaxed);
            }
            d.set_delay(frames, fade);
        }
        if comp_now != self.comp_now {
            // A chain latency to the followers (they pad to it), not a
            // step: they hold their readings over it.
            self.comp_now = comp_now;
            self.shared.chain_seq.fetch_add(1, Ordering::Relaxed);
        }
        self.shared
            .comp_frames
            .store(comp_now as u64, Ordering::Relaxed);
        self.shared
            .align_ms
            .store(self.align_ms.to_bits(), Ordering::Relaxed);
    }

    /// A measurement plays on this block (a shrink waiting for silence
    /// holds: it would move the measured timing).
    pub fn set_measuring(&mut self, on: bool) {
        self.measuring = on;
    }

    /// Apply the shrink waiting ([`TeeCmd::SetAlign`] `quiet`) once the
    /// delay line's history is silent for long enough.
    fn try_quiet(&mut self) {
        let Some(ms) = self.align_quiet else {
            return;
        };
        if self.measuring {
            return;
        }
        let quiet = match self.delay.as_deref() {
            None => true,
            Some(d) => {
                let cap = (max_align_frames(self.rate) as f64 * self.cap_speed.min(1.0)).floor();
                let new =
                    ms_frames(ms.min(MAX_ALIGN_MS), self.rate).min(cap as usize) + self.comp_now;
                let need = d.held().max(new) + ms_frames(QUIET_MARGIN_MS, self.rate);
                d.quiet_frames() >= need as u64
            }
        };
        if quiet {
            // In silence the step is not heard on any device: at once.
            self.align_quiet = None;
            self.align_ms = ms;
            self.apply_align_with(true);
        }
    }

    /// The compensation in the delay now, graph frames.
    pub fn comp_frames(&self) -> usize {
        self.comp_now
    }

    /// Drain the control queue (RT, from `poll_updates`), at `graph_rate`.
    pub fn poll_cmds(&mut self, graph_rate: u32) {
        self.flush_stash();
        let rate = graph_rate.max(8_000);
        let mut changed = rate != self.rate;
        self.rate = rate;
        if self.shared.graph_rate.load(Ordering::Relaxed) != rate {
            self.shared.graph_rate.store(rate, Ordering::Relaxed);
        }
        while self.can_retire() {
            let Some(cmd) = self.rx.as_mut().and_then(|rx| rx.pop().ok()) else {
                break;
            };
            changed = true;
            match cmd {
                TeeCmd::Attach { slot, writer } => {
                    if slot >= MAX_FOLLOWERS {
                        self.retire(TeeRetired::Writer(writer));
                    } else if let Some(old) = self.outs[slot].replace(writer) {
                        self.retire(TeeRetired::Writer(old));
                    }
                }
                TeeCmd::Detach { slot } => {
                    if let Some(old) = self.outs.get_mut(slot).and_then(Option::take) {
                        self.retire(TeeRetired::Writer(old));
                    }
                }
                TeeCmd::InstallDelay(d) => {
                    self.retire_delay = false;
                    if let Some(old) = self.delay.replace(d) {
                        self.retire(TeeRetired::Delay(old));
                    }
                }
                TeeCmd::RemoveDelay => self.retire_delay = true,
                TeeCmd::SetAlign { ms, quiet } => {
                    let ms = if ms.is_finite() { ms.max(0.0) } else { 0.0 };
                    if quiet {
                        self.align_quiet = Some(ms);
                    } else {
                        self.align_quiet = None;
                        self.align_ms = ms;
                    }
                }
                TeeCmd::SetComp { frames } => self.comp_want = frames,
            }
        }
        if changed {
            self.apply_align();
        }
        self.try_quiet();
        if self.retire_delay
            && self.delay.as_ref().is_some_and(|d| d.is_identity())
            && self.can_retire()
            && let Some(d) = self.delay.take()
        {
            self.retire_delay = false;
            self.retire(TeeRetired::Delay(d));
        }
        let active = self.outs.iter().any(Option::is_some);
        if active != self.active {
            self.active = active;
            self.clear_position();
        }
    }

    /// Take the next block's source index and advance it by `frames`.
    #[inline]
    pub fn begin_block(&mut self, frames: usize) -> u64 {
        let i = self.index;
        // The scheduled alignment change, at the first block from its
        // index on (its step is published with the block, `note_block`).
        if self.align_due.is_some_and(|d| i >= d.1) {
            self.apply_align_with(true);
        }
        self.index += frames as u64;
        self.last_block = frames;
        i
    }

    /// The source index the next block starts at (what `begin_block`
    /// returns next).
    pub fn next_index(&self) -> u64 {
        self.index
    }

    /// Write a block of source frames (interleaved, `channels`) carrying
    /// indices from `index` to every follower's ring.
    pub fn write(&mut self, index: u64, block: &[f32], channels: usize) {
        for w in self.outs.iter_mut().flatten() {
            if w.channels() == channels {
                w.write(index, block);
            }
        }
    }

    /// The alignment is cross-fading to a new delay.
    pub fn delay_fading(&self) -> bool {
        self.delay.as_ref().is_some_and(|d| d.from != d.delay)
    }

    /// The alignment applied once a fade ends, frames.
    pub fn delay_frames(&self) -> usize {
        self.delay.as_ref().map_or(0, |d| d.delay())
    }

    /// The most the alignment can grow to while the cap's speed recovers
    /// (the requested alignment at a cap speed of 1), frames: what a
    /// stream-end flush budgets, so a delay that grows during the drain is
    /// still pushed out.
    pub fn max_delay_frames(&self) -> usize {
        if self.delay.is_none() || self.retire_delay {
            return self.delay_frames();
        }
        (ms_frames(self.align_ms.min(MAX_ALIGN_MS), self.rate) + self.comp_now)
            .max(self.delay_frames())
    }

    /// The most a block can be delayed until the alignment settles: what a
    /// stage before the delay waits for before the delay's history restarts
    /// (`VarispeedRt::set_post_delay`).
    pub fn post_delay_frames(&self) -> usize {
        self.delay
            .as_ref()
            .map_or(0, |d| d.held())
            .max(self.max_delay_frames())
    }

    /// Move the delay line out for a block (no allocation). A shrink
    /// waiting for silence applies first when the history before this
    /// block is silent enough.
    pub fn take_delay(&mut self) -> Option<Box<AlignDelay>> {
        self.try_quiet();
        self.delay.take()
    }

    pub fn put_delay(&mut self, d: Option<Box<AlignDelay>>) {
        if d.is_some() {
            self.delay = d;
        }
    }

    /// A block of `out_frames` device frames went out. `span`: the source
    /// positions its first frame and the frame after its last carry at the
    /// DAC (`None`: it played nothing a follower can follow). `speed`: the
    /// speed at its ends; `ramping`: the stage ramped over it.
    pub fn note_block(
        &mut self,
        span: Option<(i128, i128)>,
        out_frames: usize,
        speed: (f64, f64),
        ramping: bool,
    ) {
        let start = self.out;
        self.out += out_frames as u64;
        match span {
            Some((first, next)) => {
                // A jump from the last block's end, beside the chain's own
                // latency (a follower's chain steps it there too): a known
                // step.
                // The compensation in the delay counts with the chain: a
                // follower's padding moves with it.
                let comp = self.comp_now as i128 * FRAME;
                let chain = |s: f64| {
                    comp + if self.chain_graph.is_finite() {
                        fixed(self.chain_graph * s)
                    } else {
                        0
                    }
                };
                let before = self.shift;
                if self.map.is_empty() {
                    self.map.push(start, first);
                } else if let Some(prev) = self.last_next {
                    let jump = first + chain(speed.0) - prev;
                    if jump.abs() >= STEP_MIN {
                        self.shift = self.shift.wrapping_add((jump >> STEP_UNIT_SHIFT) as i64);
                        // Where the master's output jumped, as its frames
                        // carry it at the DAC (its stage's filter delay,
                        // its chain and compensation taken off): what a
                        // follower's output carries is compared with it.
                        let at = self.last_next_map.div_euclid(FRAME) as i64;
                        self.shared.step_at.store(at as u64, Ordering::Relaxed);
                        self.shared
                            .steps
                            .store(self.shift as u64, Ordering::Release);
                    }
                }
                self.last_next = Some(next + chain(speed.1));
                self.last_next_map = next;
                if out_frames > 0 {
                    self.map.push(self.out, next);
                    self.spans[self.span_head] = Span {
                        start,
                        end: self.out,
                        speed,
                        ramping,
                        shift: (before, self.shift),
                    };
                    self.span_head = (self.span_head + 1) % MAP_LEN;
                    self.span_len = (self.span_len + 1).min(MAP_LEN);
                }
            }
            None => {
                if !self.map.is_empty() {
                    self.clear_position();
                }
            }
        }
        // The announced step is published (or nothing follows): the
        // announcement goes after it.
        if std::mem::take(&mut self.announce_clear) {
            self.shared.step_announce.store(0, Ordering::Release);
        }
    }

    /// The speed, ramp state and known steps of device frame `f`: from its
    /// block (the newest block's end for the frame after it).
    fn span_at(&self, f: u64) -> Option<(f64, bool, i64)> {
        for i in 0..self.span_len {
            let sp = self.spans[(self.span_head + MAP_LEN - 1 - i) % MAP_LEN];
            if i == 0 && f == sp.end {
                return Some((sp.speed.1, sp.ramping, sp.shift.1));
            }
            if (sp.start..sp.end).contains(&f) {
                let t = (f - sp.start) as f64 / (sp.end - sp.start) as f64;
                // Inside a block that steps the map is interpolated across
                // the step: a total no follower holds.
                let shift = if f == sp.start || sp.shift.0 == sp.shift.1 {
                    sp.shift.0
                } else {
                    i64::MIN
                };
                return Some((
                    sp.speed.0 + (sp.speed.1 - sp.speed.0) * t,
                    sp.ramping,
                    shift,
                ));
            }
        }
        None
    }

    /// A DAC reading: `delay` device frames queued ahead of the DAC at
    /// `time_ns` (`CLOCK_MONOTONIC_RAW`), everything the processor has
    /// output being written. The position, the speed and the ramp state
    /// are all the frame at the DAC's.
    pub fn dac_read(&mut self, delay: i64, time_ns: u64) {
        if !self.active {
            return;
        }
        let reading = Reading {
            written: self.out,
            delay,
            time_ns: time_ns as i64,
        };
        let Some(f) = reading.played() else {
            return;
        };
        let (Some(pos), Some((speed, ramping, shift))) = (self.map.source_at(f), self.span_at(f))
        else {
            return;
        };
        self.shared.position.publish(MasterPoint {
            pos,
            time_ns: reading.time_ns,
            speed,
            ramping,
            epoch: self.epoch,
            shift,
        });
    }

    /// A discontinuity: the map restarts and the published position goes
    /// invalid under a new epoch.
    pub fn clear_position(&mut self) {
        self.map.clear();
        self.span_len = 0;
        self.last_next = None;
        self.epoch += 1;
        self.shared.position.invalidate(self.epoch);
    }

    /// Output frames the stage holds commands back (0 without a
    /// follower), [`TeeShared::command_lead_ms`].
    pub fn command_lead_frames(&self) -> usize {
        if !self.active {
            return 0;
        }
        let ms = f64::from_bits(self.shared.command_lead_ms.load(Ordering::Relaxed));
        ms_frames(
            if ms.is_finite() {
                ms.min(MAX_LEAD_MS)
            } else {
                0.0
            },
            self.rate,
        )
    }

    /// The chain's delay a block is mapped with (graph frames): a change
    /// counts in [`TeeShared::chain_seq`].
    pub fn note_chain_latency(&mut self, chain_graph: f64) {
        if chain_graph != self.chain_graph {
            if self.chain_graph.is_finite() {
                self.shared.chain_seq.fetch_add(1, Ordering::Relaxed);
            }
            self.chain_graph = chain_graph;
            self.shared.master_chain_graph.set(chain_graph);
        }
    }

    /// Append to the command log (only while a follower is attached).
    pub fn log(&self, at: i128, cmd: VsCommand) {
        if self.active {
            self.shared.log.append(at, cmd);
        }
    }

    /// A DSP reset or reopen: the device count restarts with the map, and
    /// the delay line's history.
    pub fn reset(&mut self) {
        self.out = 0;
        self.new_stream();
    }

    /// The PCM was prepared for a new stream (a start, a rate reopen, a
    /// recovery): what the delay line holds belongs to the old one, and the
    /// device's queue starts empty.
    pub fn new_stream(&mut self) {
        if self.active {
            self.clear_position();
        }
        // The followers place themselves anew: a change waiting goes now.
        if self.align_due.is_some() {
            self.apply_align_with(true);
            self.shared.step_announce.store(0, Ordering::Release);
            self.announce_clear = false;
        }
        self.refit = true;
        if let Some(d) = self.delay.as_mut() {
            d.restart();
        }
    }

    /// The source position (log domain: no filter delays) of the delayed
    /// source frame the block at `index` starts with.
    pub fn input_position(&self, index: u64) -> i128 {
        (i128::from(index) - self.delay_frames() as i128) * FRAME
    }

    /// After a block that ran the stage: the state it left, when settled
    /// (`next_at`: its next output frame's position).
    pub fn stage_block(&mut self, next_at: i128, settled: Option<VsCommand>) {
        self.staged = true;
        if let Some(cmd) = settled {
            self.publish_state(next_at, cmd);
        }
    }

    /// After a block on the DSP path without a stage (`[first_at, next_at)`
    /// in the log's domain): a stage that ran before retired, so the log
    /// says [`VsCommand::Off`], and the state is unity.
    pub fn plain_block(&mut self, first_at: i128, next_at: i128) {
        if std::mem::take(&mut self.staged) {
            self.log(first_at, VsCommand::Off);
        }
        self.publish_state(next_at, VsCommand::Off);
    }

    fn publish_state(&self, at: i128, cmd: VsCommand) {
        if self.active {
            self.shared.state.publish(PlaybackState {
                cursor: self.shared.log.head(),
                at,
                cmd,
            });
        }
    }

    /// The source positions (at the DAC) of a block's first output frame
    /// and the frame after its last, when the stage ran: `v0` before the
    /// call, `v1` after. `chain_graph`: the chain's delay in frames at the
    /// graph rate (stage output frames: scaled by the speed here).
    pub fn stage_span(
        &self,
        index: u64,
        v0: StageView,
        v1: StageView,
        chain_graph: f64,
    ) -> (i128, i128) {
        let origin = self.stage_origin(index, v0);
        let at = |v: StageView| {
            origin + (v.acc >> 1) as i128 - fixed(v.delay_src + chain_graph * v.speed)
        };
        (at(v0), at(v1))
    }

    /// The source position (tee domain) of the stage's input frame 0 for
    /// a block at `index`: the stage takes the delayed source behind its
    /// look-ahead.
    pub fn stage_origin(&self, index: u64, v0: StageView) -> i128 {
        (i128::from(index) - i128::from(v0.fed) - v0.hold as i128 - self.delay_frames() as i128)
            * FRAME
    }

    /// The span of a block without a stage: `frames` source frames from
    /// `index`, behind the alignment and the chain's delay.
    pub fn plain_span(&self, index: u64, frames: usize, chain_graph: f64) -> (i128, i128) {
        let first = (i128::from(index) - self.delay_frames() as i128) * FRAME - fixed(chain_graph);
        (first, first + frames as i128 * FRAME)
    }
}

// ---------------------------------------------------------------------------
// The control side.

/// A follower's per-device slot (control side).
pub struct FollowerSlot {
    /// Index of its ring in the processor (`< MAX_FOLLOWERS`).
    pub slot: usize,
    /// The config its chain instance is built from (v1: the master's).
    pub chain_config: EngineConfig,
    /// Its end of the tee, until its thread takes it (step 4).
    pub reader: Option<TeeReader>,
}

/// Every follower, keyed by device.
#[derive(Default)]
pub struct FollowerSlots {
    by_device: BTreeMap<String, FollowerSlot>,
}

impl FollowerSlots {
    pub fn len(&self) -> usize {
        self.by_device.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_device.is_empty()
    }

    pub fn get(&self, device: &str) -> Option<&FollowerSlot> {
        self.by_device.get(device)
    }

    pub fn get_mut(&mut self, device: &str) -> Option<&mut FollowerSlot> {
        self.by_device.get_mut(device)
    }

    pub fn devices(&self) -> impl Iterator<Item = &str> {
        self.by_device.keys().map(String::as_str)
    }

    /// Refill every slot from the master's config (v1):
    /// a chain edit applies to the master and every follower.
    pub fn fill_from_master(&mut self, master: &EngineConfig) {
        for s in self.by_device.values_mut() {
            s.chain_config = master.clone();
        }
    }

    fn free_slot(&self) -> Option<usize> {
        (0..MAX_FOLLOWERS).find(|i| self.by_device.values().all(|s| s.slot != *i))
    }
}

/// The control side of the tee: builds rings and the delay line off the
/// RT thread, keeps the per-device slots, and frees what the RT side
/// retires.
pub struct TeeHost {
    tx: Option<rtrb::Producer<TeeCmd>>,
    recycle: Option<rtrb::Consumer<TeeRetired>>,
    channels: usize,
    shared: TeeShared,
    slots: FollowerSlots,
    delay_installed: bool,
}

impl TeeHost {
    /// A host whose commands go nowhere (tests, and no tee).
    pub fn detached() -> Self {
        Self {
            tx: None,
            recycle: None,
            channels: 2,
            shared: TeeShared::default(),
            slots: FollowerSlots::default(),
            delay_installed: false,
        }
    }

    /// A host and the processor's slot it drives, for `channels` channels.
    pub fn pair(channels: usize) -> (Self, TeeRt) {
        let (tx, rx) = rtrb::RingBuffer::new(16);
        let (rtx, rrx) = rtrb::RingBuffer::new(16);
        let shared = TeeShared::default();
        let rt = TeeRt::new(rx, rtx, shared.clone());
        (
            Self {
                tx: Some(tx),
                recycle: Some(rrx),
                channels: channels.max(1),
                shared,
                slots: FollowerSlots::default(),
                delay_installed: false,
            },
            rt,
        )
    }

    pub fn shared(&self) -> &TeeShared {
        &self.shared
    }

    pub fn slots(&self) -> &FollowerSlots {
        &self.slots
    }

    pub fn slots_mut(&mut self) -> &mut FollowerSlots {
        &mut self.slots
    }

    fn send(&mut self, cmd: TeeCmd) -> Result<(), String> {
        match self.tx.as_mut() {
            Some(tx) => tx
                .push(cmd)
                .map_err(|_| "tee command queue full".to_string()),
            None => Ok(()),
        }
    }

    /// Free what the RT side retired.
    pub fn reap(&mut self) -> usize {
        let mut n = 0;
        if let Some(rx) = self.recycle.as_mut() {
            while let Ok(r) = rx.pop() {
                drop(r);
                n += 1;
            }
        }
        n
    }

    /// Add a follower on `device` with its chain config slot filled from
    /// `master`: its ring goes to the processor (and the delay line with the
    /// first follower).
    pub fn add_follower(&mut self, device: &str, master: &EngineConfig) -> Result<(), String> {
        self.reap();
        if self.slots.get(device).is_some() {
            return Err(format!("{device} is a follower already"));
        }
        let slot = self
            .slots
            .free_slot()
            .ok_or_else(|| format!("at most {MAX_FOLLOWERS} followers"))?;
        if !self.delay_installed {
            let d = Box::new(AlignDelay::new(
                self.channels,
                max_align_frames(MAX_GRAPH_RATE) + max_comp_frames(MAX_GRAPH_RATE),
            ));
            self.send(TeeCmd::InstallDelay(d))?;
            self.delay_installed = true;
        }
        let (writer, reader) = tee_ring(self.channels, ring_frames());
        self.send(TeeCmd::Attach {
            slot,
            writer: Box::new(writer),
        })?;
        self.slots.by_device.insert(
            device.to_string(),
            FollowerSlot {
                slot,
                chain_config: master.clone(),
                reader: Some(reader),
            },
        );
        Ok(())
    }

    /// Remove the follower on `device` (and the delay line with the last).
    pub fn remove_follower(&mut self, device: &str) -> Result<(), String> {
        self.reap();
        let Some(s) = self.slots.by_device.remove(device) else {
            return Ok(());
        };
        self.send(TeeCmd::Detach { slot: s.slot })?;
        if self.slots.is_empty() && self.delay_installed {
            self.send(TeeCmd::SetAlign {
                ms: 0.0,
                quiet: false,
            })?;
            self.send(TeeCmd::SetComp { frames: 0 })?;
            self.send(TeeCmd::RemoveDelay)?;
            self.delay_installed = false;
        }
        Ok(())
    }

    /// How long the master holds speed commands back, ms
    /// ([`TeeShared::command_lead_ms`]).
    pub fn set_command_lead_ms(&self, ms: f64) {
        let ms = if ms.is_finite() {
            ms.clamp(0.0, MAX_LEAD_MS)
        } else {
            0.0
        };
        self.shared
            .command_lead_ms
            .store(ms.to_bits(), Ordering::Relaxed);
    }

    /// The master's alignment delay, ms. Returns what applies: at most
    /// [`MAX_ALIGN_MS`] (a follower that needs more plays unaligned) and 0
    /// without followers.
    pub fn set_alignment_ms(&mut self, ms: f64) -> Result<f64, String> {
        let ms = if self.slots.is_empty() || !ms.is_finite() {
            0.0
        } else {
            ms.clamp(0.0, MAX_ALIGN_MS)
        };
        self.send(TeeCmd::SetAlign { ms, quiet: false })?;
        Ok(ms)
    }

    /// Shrink the master's alignment delay to `ms` at the next silence
    /// ([`TeeCmd::SetAlign`] `quiet`): it holds while anything plays. A
    /// later [`Self::set_alignment_ms`] drops it.
    pub fn shrink_alignment_ms(&mut self, ms: f64) -> Result<(), String> {
        let ms = if ms.is_finite() {
            ms.clamp(0.0, MAX_ALIGN_MS)
        } else {
            return Ok(());
        };
        self.send(TeeCmd::SetAlign { ms, quiet: true })
    }

    /// The alignment the RT side applies, ms (0 before its first).
    pub fn applied_alignment_ms(&self) -> f64 {
        f64::from_bits(self.shared.align_ms.load(Ordering::Relaxed))
    }

    /// The master's chain compensation, graph frames ([`TeeCmd::SetComp`];
    /// 0 without followers).
    pub fn set_comp_frames(&mut self, frames: usize) -> Result<(), String> {
        let frames = if self.slots.is_empty() { 0 } else { frames };
        self.send(TeeCmd::SetComp { frames })
    }

    /// A chain edit: every follower's slot follows the master (v1).
    pub fn master_config_changed(&mut self, master: &EngineConfig) {
        self.slots.fill_from_master(master);
    }
}

#[cfg(test)]
#[path = "tee_tests.rs"]
mod tests;
