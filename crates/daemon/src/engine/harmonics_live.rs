//! The harmonics stage's live view ([`audivane_protocol::ipc::Call::HarmonicsLive`]).
//!
//! **Audio thread** ([`HarmonicsLiveRt`]): while someone watches, the chain
//! copies its harmonics stage's input and output (the A/B side playing, or
//! the other side when only it has a harmonics stage; the mono sum of its channels, [`audivane_dsp_core::chain::HarmonicsTap`]) after
//! each call. [`HarmonicsLiveRt::push`] moves them into a wait-free SPSC ring
//! (`rtrb`), one bulk write, plus a per-call header (rate, frame count, stream
//! position, side, linear gain, sag and bias state) into a second ring. No
//! allocation, lock or syscall; a call that does not fit is dropped whole (the
//! next header starts a new epoch), so the analyser never joins audio across a
//! gap. Headers carry the count of frames queued before them: the analyser
//! resyncs on any mismatch instead of reading one block under another's
//! header. While nobody watches, the audio thread reads one atomic flag per
//! block and copies nothing. Both rings are written through once at creation,
//! so the first writes never page-fault.
//!
//! The tap follows the A/B side playing, or the side not heard when only it
//! runs a harmonics stage (both sides run every block), one side per block;
//! a side change restarts the window, so no frame mixes the two.
//!
//! **Analysis thread**: keeps the newest FFT window of pairs (overwrites
//! rather than queues) and analyses it every [`BASE_INTERVAL`]; an analysis
//! over a quarter of the interval doubles it (up to [`MAX_INTERVAL`]), halved
//! again when there is room, so a slow machine gets fewer frames and playback
//! never waits. With no `harmonics_live` call within
//! [`HARMONICS_LIVE_HOLD_MS`] it parks.
//!
//! The analysis (protocol docs, [`HarmonicsLive`]): the least-squares gain `g`
//! of the output on the input over a Hann window, the added content
//! `a = y − g x`, log-binned spectra of the music and of `a`, the music's
//! level and centroid, and the split of `a` by order or parity from the
//! stage's static curve ([`LiveCurve`]) when it has one.
//!
//! **The headline** (the added level, its peak, the level change and the
//! parts) covers the newest `AVERAGE_S`, not one window: while draining, the
//! analyser keeps sums per short segment (`SpanFit`) and fits `g` over the
//! whole span, following the slow level changes (sag's drive gain from the
//! tap, the music's level for the curve's compression), so it moves calmly and
//! a window's few bass cycles do not read the added level low. The parts are
//! each window's split, as shares, applied to the headline.
//!
//! **The low band**: the window's FFT (about 11 Hz per bin) is coarser than a
//! 1/24-octave bin below about 1.6 kHz. As in the PEQ analyser
//! ([`crate::engine::spectrum`]), it comes from a longer span: the analyser
//! decimates the mono sum's input and output by /2 stages to about 5 to 10 kHz
//! (`LowBand`) and keeps the newest `LOW_SPAN_S`; each frame runs a Hann
//! FFT of that span (about 1.5 Hz per bin) and fits the added content over it
//! (`a = y − g x` with its own `g`, one fit over the whole span, so there is
//! no join). Log bins narrower than `CROSSOVER_BINS` window FFT bins read
//! the long span. Every log bin reads the interpolated band power, or a sine's
//! peak level where the bin holds one, so the resolutions agree and narrow
//! bins never repeat one FFT bin.

use crate::engine::spectrum::{HalfBandStage, LogBand, band_power, log_band, tonal_peaks};
use audivane_dsp_core::chain::{HARMONICS_TAP_LAYERS, HarmonicsTap};
use audivane_dsp_core::fft::RealFftPair;
use audivane_dsp_core::harmonics::LiveLayer;
use audivane_dsp_core::harmonics::live::{AGREE_DB, LiveCurve, LiveScratch, LiveSplit, live_curve};
use audivane_protocol::HarmonicsConfig;
use audivane_protocol::ipc::{
    HARMONICS_LIVE_HOLD_MS, HarmonicsLive, HarmonicsLiveLayer, HarmonicsLivePart,
    HarmonicsLiveSplit, HarmonicsLiveState,
};
use rustfft::num_complex::Complex64;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Sample ring capacity, pairs: 341 ms at 768 kHz, 85 ms at 3.072 MHz
/// (the analyser drains it every [`TICK`]).
const SAMPLE_RING: usize = 1 << 18;
/// Header ring capacity: one header per chain call.
const HEADER_RING: usize = 4096;
/// Blocker states kept for the window's seed (one per block).
const DC_MARKS: usize = 1024;
/// How often the analyser drains the rings.
const TICK: Duration = Duration::from_millis(20);
/// The frame interval when analysis is cheap.
pub const BASE_INTERVAL: Duration = Duration::from_millis(200);
/// The longest interval the load guard stretches it to.
pub const MAX_INTERVAL: Duration = Duration::from_millis(1600);
/// Analysis share of the interval above which the interval doubles, and
/// below which it halves again.
const SLOW_SHARE: f64 = 0.25;
const FAST_SHARE: f64 = 0.05;
/// No header for this long: nothing plays through the chain.
const IDLE_AFTER: Duration = Duration::from_millis(400);
/// Music below this (RMS, dBFS) is silence.
const SILENT_DBFS: f64 = -90.0;
pub const BINS_PER_OCTAVE: u32 = 24;
/// The spectra's bottom edge (the low band's FFT bins, about 1.5 Hz, are
/// wider than a log bin below 20 Hz: those bins read the interpolated band
/// power, a sine its peak level).
pub const F_MIN_HZ: f64 = 10.0;
const DB_FLOOR: f64 = -160.0;
/// Segments of the window for the peak of the added level.
const PEAK_SEGMENTS: usize = 8;
/// Log bins narrower than this many window FFT bins read the low band's
/// long span (below about 1.6 kHz).
const CROSSOVER_BINS: f64 = 4.0;
/// The low band's rate: halved while it stays at or above this, so it lands
/// in 5..10 kHz (a rate under 10 kHz is kept).
const LOW_MIN_HZ: f64 = 5000.0;
/// The low band serves log bins up to this share of its rate (the last
/// half-band stage is flat there and its aliases fall above).
const LOW_PASS: f64 = 0.35;
/// The low band's span, s: its FFT is the power of two at or below it.
const LOW_SPAN_S: f64 = 0.75;
/// The headline numbers (the added level, its peak, the level change and
/// the parts) are a power average of the frames' windows that start within
/// this span of the newest window's end, s: they settle within it.
const AVERAGE_S: f64 = 1.0;

/// The FFT (and window) length at `rate`: the power of two at or below
/// 1/8 s, within 2048..=131072 (about 11 Hz per bin from 44.1 to 768 kHz).
pub fn fft_size(rate: u32) -> usize {
    let target = (rate / 8).max(2048) as usize;
    let mut n = 2048;
    while n * 2 <= target && n < 131_072 {
        n *= 2;
    }
    n
}

/// One queued frame: the mono sum's input and output, then the input of
/// channels 0 and 1.
type TapFrame = [f32; 4];

#[derive(Clone, Copy, Debug)]
struct Header {
    epoch: u32,
    rate: u32,
    frames: u32,
    /// Frames queued before this header's own (the producer's running
    /// count).
    start: u64,
    /// The A/B side (0: A or a single chain, 1: B).
    side: u8,
    /// The chain's channel count (the split needs at most two).
    channels: u8,
    /// The side tapped runs a harmonics stage.
    active: bool,
    linear_gain: f64,
    layers: [LiveLayer; HARMONICS_TAP_LAYERS],
    n_layers: u8,
    /// The stage's DC blocker state on channels 0 and 1 after the block
    /// (a parallel stage with its blocker on).
    dc: Option<[f64; 2]>,
}

/// RT half ([`crate::engine::processor::DaemonProcessor`] owns it).
pub struct HarmonicsLiveRt {
    samples: rtrb::Producer<TapFrame>,
    headers: rtrb::Producer<Header>,
    enabled: Arc<AtomicBool>,
    epoch: u32,
    gap: bool,
    /// Frames queued so far.
    written: u64,
}

impl HarmonicsLiveRt {
    /// Whether someone watches (copy the tap this block).
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Queue one chain call's tap at the output `rate`: `tap` its frames
    /// (`None`: the call did not run the stage), `active` whether the side tapped
    /// has a stage at all. Wait-free and allocation-free; a call that does
    /// not fit is dropped whole.
    #[inline]
    pub fn push(&mut self, tap: Option<HarmonicsTap<'_>>, active: bool, rate: u32) {
        if !self.is_enabled() {
            return;
        }
        let frames = tap.map_or(0, |t| t.input.len().min(t.output.len()));
        if self.headers.slots() == 0 || self.samples.slots() < frames {
            self.gap = true;
            return;
        }
        if self.gap {
            self.epoch = self.epoch.wrapping_add(1);
            self.gap = false;
        }
        let mut header = Header {
            epoch: self.epoch,
            rate,
            frames: frames as u32,
            start: self.written,
            side: tap.map_or(0, |t| t.side),
            channels: tap.map_or(0, |t| t.channels.min(255) as u8),
            active,
            linear_gain: 1.0,
            layers: [LiveLayer::default(); HARMONICS_TAP_LAYERS],
            n_layers: 0,
            dc: None,
        };
        if let Some(t) = tap {
            if frames > 0
                && t.input0.len() >= frames
                && t.input1.len() >= frames
                && let Ok(chunk) = self.samples.write_chunk_uninit(frames)
            {
                let n = chunk.fill_from_iter(
                    (0..frames).map(|i| [t.input[i], t.output[i], t.input0[i], t.input1[i]]),
                );
                self.written += n as u64;
            }
            header.linear_gain = t.linear_gain;
            let n = t.layers.len().min(HARMONICS_TAP_LAYERS);
            header.layers[..n].copy_from_slice(&t.layers[..n]);
            header.n_layers = n as u8;
            header.dc = t.dc;
        }
        let _ = self.headers.push(header);
    }
}

/// State shared between the control plane and the analyser.
struct Shared {
    latest: Mutex<HarmonicsLive>,
    /// Milliseconds since `start` of the last poll, plus 1 (0: never).
    last_poll: AtomicU64,
    start: Instant,
    /// The harmonics configs of sides A and B, for the static curve.
    config: Mutex<[Option<HarmonicsConfig>; 2]>,
    thread: Mutex<Option<thread::Thread>>,
    /// Bumped by [`HarmonicsLiveWorker::suspend`]: the analysis drops
    /// everything it holds, and a frame from before is never published.
    generation: AtomicU64,
}

impl Shared {
    fn watched(&self, now: Instant) -> bool {
        let last = self.last_poll.load(Ordering::Acquire);
        if last == 0 {
            return false;
        }
        let now_ms = now.duration_since(self.start).as_millis() as u64 + 1;
        now_ms.saturating_sub(last) <= u64::from(HARMONICS_LIVE_HOLD_MS)
    }
}

/// Non-RT half: the analysis thread and the latest frame.
pub struct HarmonicsLiveWorker {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// Create the rings and start the (parked) analysis thread.
pub fn create() -> std::io::Result<(HarmonicsLiveRt, HarmonicsLiveWorker)> {
    let (mut sample_tx, mut sample_rx) = rtrb::RingBuffer::<TapFrame>::new(SAMPLE_RING);
    let (mut header_tx, mut header_rx) = rtrb::RingBuffer::<Header>::new(HEADER_RING);
    touch(&mut sample_tx, &mut sample_rx, [0.0; 4]);
    touch(&mut header_tx, &mut header_rx, EMPTY_HEADER);
    let enabled = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        latest: Mutex::new(empty_frame(HarmonicsLiveState::Waiting, 0, 0)),
        last_poll: AtomicU64::new(0),
        start: Instant::now(),
        config: Mutex::new([None, None]),
        thread: Mutex::new(None),
        generation: AtomicU64::new(0),
    });
    let handle = {
        let (shared, stop, enabled) =
            (Arc::clone(&shared), Arc::clone(&stop), Arc::clone(&enabled));
        thread::Builder::new()
            .name("audivane-harmonics-live".into())
            .spawn(move || {
                run(
                    shared,
                    stop,
                    enabled,
                    Rx {
                        samples: sample_rx,
                        headers: header_rx,
                        consumed: 0,
                    },
                )
            })?
    };
    if let Ok(mut t) = shared.thread.lock() {
        *t = Some(handle.thread().clone());
    }
    Ok((
        HarmonicsLiveRt {
            samples: sample_tx,
            headers: header_tx,
            enabled,
            epoch: 1,
            gap: false,
            written: 0,
        },
        HarmonicsLiveWorker {
            shared,
            stop,
            handle: Some(handle),
        },
    ))
}

impl HarmonicsLiveWorker {
    /// The latest frame; keeps the analysis on for
    /// [`HARMONICS_LIVE_HOLD_MS`] (wakes it when it was off).
    pub fn poll(&self) -> HarmonicsLive {
        let now = Instant::now();
        let was = self.shared.watched(now);
        let ms = now.duration_since(self.shared.start).as_millis() as u64 + 1;
        self.shared.last_poll.store(ms, Ordering::Release);
        if !was {
            if let Ok(mut l) = self.shared.latest.lock() {
                *l = empty_frame(HarmonicsLiveState::Waiting, 0, 0);
            }
            self.unpark();
        }
        self.shared
            .latest
            .lock()
            .map(|l| l.clone())
            .unwrap_or_else(|_| empty_frame(HarmonicsLiveState::Idle, 0, 0))
    }

    /// Stop the analysis and forget it (a blind test starts or ends):
    /// the queued samples, the window, the published frame and its
    /// count go, and the next poll starts afresh, as if never watched.
    pub fn suspend(&self) {
        self.shared.last_poll.store(0, Ordering::Release);
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut l) = self.shared.latest.lock() {
            *l = empty_frame(HarmonicsLiveState::Waiting, 0, 0);
        }
        self.unpark();
    }

    fn unpark(&self) {
        if let Ok(t) = self.shared.thread.lock()
            && let Some(t) = t.as_ref()
        {
            t.unpark();
        }
    }

    /// The harmonics configs of sides A and B (on every apply; B `None`
    /// without A/B), for the static curve of the side playing.
    pub fn set_config(&self, a: Option<HarmonicsConfig>, b: Option<HarmonicsConfig>) {
        if let Ok(mut c) = self.shared.config.lock() {
            *c = [a, b];
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl Drop for HarmonicsLiveWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Write every slot of a fresh ring once and read it back (off the audio
/// thread), so its pages are mapped before the audio thread writes.
fn touch<T: Copy>(tx: &mut rtrb::Producer<T>, rx: &mut rtrb::Consumer<T>, v: T) {
    let n = tx.slots();
    if let Ok(chunk) = tx.write_chunk_uninit(n) {
        chunk.fill_from_iter(std::iter::repeat(v));
    }
    let n = rx.slots();
    if let Ok(chunk) = rx.read_chunk(n) {
        chunk.commit_all();
    }
}

const EMPTY_HEADER: Header = Header {
    epoch: 0,
    rate: 0,
    frames: 0,
    start: 0,
    side: 0,
    channels: 0,
    active: false,
    linear_gain: 1.0,
    layers: [LiveLayer {
        layer: 0,
        sag_db: 0.0,
        bias: 0.0,
    }; HARMONICS_TAP_LAYERS],
    n_layers: 0,
    dc: None,
};

/// A frame with no music in it (`state` says why).
pub(crate) fn empty_frame(state: HarmonicsLiveState, seq: u64, rate: u32) -> HarmonicsLive {
    HarmonicsLive {
        state,
        seq,
        rate,
        window_ms: 0.0,
        interval_ms: BASE_INTERVAL.as_secs_f64() * 1e3,
        f_min_hz: F_MIN_HZ,
        bins_per_octave: BINS_PER_OCTAVE,
        music: Vec::new(),
        added: Vec::new(),
        added_db: DB_FLOOR,
        added_pct: 0.0,
        added_peak_db: DB_FLOOR,
        level_change_db: 0.0,
        linear_gain_db: 0.0,
        music_rms_dbfs: DB_FLOOR,
        music_level_dbfs: DB_FLOOR,
        music_peak_dbfs: DB_FLOOR,
        centroid_hz: 0.0,
        layers: Vec::new(),
        split: HarmonicsLiveSplit::Total,
        split_note: None,
        parts: Vec::new(),
        analysis_ms: 0.0,
    }
}

/// The analyser's ends of the rings and how many frames it has taken.
struct Rx {
    samples: rtrb::Consumer<TapFrame>,
    headers: rtrb::Consumer<Header>,
    /// Frames taken (read or discarded) so far: in step with the
    /// producer's count when the next header's `start` equals it.
    consumed: u64,
}

impl Rx {
    /// Discard every queued header; their frames are skipped by count when
    /// the next header arrives.
    fn discard_headers(&mut self) {
        let n = self.headers.slots();
        if let Ok(c) = self.headers.read_chunk(n) {
            c.commit_all();
        }
    }

    /// Discard up to `n` queued frames; returns how many.
    fn skip(&mut self, n: u64) -> u64 {
        let k = (n.min(self.samples.slots() as u64)) as usize;
        if let Ok(c) = self.samples.read_chunk(k) {
            c.commit_all();
        }
        self.consumed += k as u64;
        k as u64
    }

    /// Discard everything queued.
    fn discard_all(&mut self) {
        self.discard_headers();
        self.skip(u64::MAX);
    }
}

fn run(shared: Arc<Shared>, stop: Arc<AtomicBool>, enabled: Arc<AtomicBool>, mut rx: Rx) {
    let mut an: Option<Analyzer> = None;
    let mut generation = shared.generation.load(Ordering::Acquire);
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let g = shared.generation.load(Ordering::Acquire);
        if g != generation {
            // Suspended (maybe watched again already): start over.
            generation = g;
            enabled.store(false, Ordering::Release);
            an = None;
        }
        if !shared.watched(now) {
            enabled.store(false, Ordering::Release);
            an = None;
            rx.discard_all();
            thread::park_timeout(Duration::from_secs(1));
            continue;
        }
        if !enabled.load(Ordering::Acquire) {
            // A new subscription: nothing from before it counts. Both
            // rings are emptied while the producer is still off, then it
            // starts; the window starts afresh.
            rx.discard_all();
            an = None;
            enabled.store(true, Ordering::Release);
        }
        let a = an.get_or_insert_with(|| Analyzer::new(now));
        a.drain(&mut rx, now);
        if let Some(frame) = a.step(&shared, now)
            && let Ok(mut l) = shared.latest.lock()
            && shared.generation.load(Ordering::Acquire) == generation
        {
            *l = frame;
        }
        thread::sleep(TICK);
    }
    enabled.store(false, Ordering::Release);
}

/// The analysis thread's state while watched.
struct Analyzer {
    started: Instant,
    rate: u32,
    epoch: u32,
    side: u8,
    channels: u8,
    /// History ring of the newest `n` frames.
    hist: Vec<TapFrame>,
    pos: usize,
    filled: usize,
    /// Stream position just past the newest frame in `hist`.
    hist_end: u64,
    /// The stage's blocker state at block ends within the window
    /// (position just past the block, channels 0 and 1), oldest first.
    dcs: VecDeque<(u64, [f64; 2])>,
    last_header: Option<Instant>,
    active: bool,
    linear_gain: f64,
    layers: Vec<LiveLayer>,
    interval: Duration,
    due: Instant,
    seq: u64,
    spec: Option<Spectral>,
    /// The mono sum's input and output, decimated, for the spectra's low
    /// band.
    low: LowBand,
    /// The last frame's log bins per resolution (linear): the window's for
    /// the music and the added content, then the long span's.
    #[cfg(test)]
    probe: Option<[Vec<f64>; 4]>,
    #[cfg(test)]
    test_fixed_gain: bool,
    curve: Option<CurveCache>,
    /// The recent windows' sums behind the parts ([`AVERAGE_S`]).
    recent: VecDeque<WindowSums>,
    /// Every frame's sums over the newest [`AVERAGE_S`], for the headline.
    span: SpanFit,
    /// Working buffers of the split, kept across frames.
    scratch: LiveScratch,
    parts: Vec<Vec<f64>>,
    right: Vec<Vec<f64>>,
    even: Vec<f64>,
}

struct CurveCache {
    config: Option<HarmonicsConfig>,
    rate: u32,
    curve: Result<LiveCurve, String>,
}

impl Analyzer {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            rate: 0,
            epoch: 0,
            side: 0,
            channels: 0,
            hist: Vec::new(),
            pos: 0,
            filled: 0,
            hist_end: 0,
            dcs: VecDeque::with_capacity(DC_MARKS),
            last_header: None,
            active: false,
            linear_gain: 1.0,
            layers: Vec::new(),
            interval: BASE_INTERVAL,
            due: now,
            seq: 0,
            spec: None,
            low: LowBand::new(0),
            #[cfg(test)]
            probe: None,
            #[cfg(test)]
            test_fixed_gain: false,
            curve: None,
            recent: VecDeque::new(),
            span: SpanFit::new(0),
            scratch: LiveScratch::default(),
            parts: Vec::new(),
            right: Vec::new(),
            even: Vec::new(),
        }
    }

    fn drain(&mut self, rx: &mut Rx, now: Instant) {
        while let Ok(h) = rx.headers.pop() {
            self.last_header = Some(now);
            let frames = u64::from(h.frames);
            // In step with the sample stream? Frames queued under headers
            // that were discarded come first: skip them. A header whose
            // frames are (partly) gone: skip what is left of them.
            let mut lost = false;
            if h.start > rx.consumed {
                rx.skip(h.start - rx.consumed);
                lost = true;
            }
            if h.start < rx.consumed {
                let done = rx.consumed - h.start;
                rx.skip(frames.saturating_sub(done));
                self.restart();
                continue;
            }
            let changed = h.rate != self.rate
                || h.epoch != self.epoch
                || (h.frames > 0 && (h.side != self.side || h.channels != self.channels));
            if changed || lost {
                if h.rate != self.rate {
                    self.rate = h.rate;
                    let n = fft_size(h.rate);
                    self.hist = vec![[0.0; 4]; n];
                    self.low = LowBand::new(h.rate);
                    self.span = SpanFit::new(h.rate);
                    self.spec = None;
                }
                self.epoch = h.epoch;
                if h.frames > 0 {
                    self.side = h.side;
                    self.channels = h.channels;
                }
                self.restart();
            }
            self.active = h.active;
            if h.frames > 0 {
                self.linear_gain = h.linear_gain;
                self.layers.clear();
                self.layers
                    .extend_from_slice(&h.layers[..usize::from(h.n_layers)]);
            }
            // The drive's turn-down now (the deepest sag), for the span's
            // fit.
            let sag = self
                .layers
                .iter()
                .fold(0.0_f64, |m, l| m.max(l.sag_db))
                .max(0.0);
            let sag_gain = 10f64.powf(-sag / 20.0);
            let want = frames as usize;
            let Ok(chunk) = rx.samples.read_chunk(want.min(rx.samples.slots())) else {
                continue;
            };
            let (a, b) = chunk.as_slices();
            let got = a.len() + b.len();
            let n = self.hist.len();
            if n > 0 {
                for &p in a.iter().chain(b) {
                    self.hist[self.pos] = p;
                    self.pos = (self.pos + 1) % n;
                    let (x, y) = (f64::from(p[0]), f64::from(p[1]));
                    self.low.push(x, y);
                    self.span.push(x, y, sag_gain);
                }
                self.filled = (self.filled + got).min(n);
            }
            chunk.commit_all();
            rx.consumed += got as u64;
            self.hist_end = rx.consumed;
            if got < want {
                // Not queued yet (cannot happen: frames go before their
                // header); the window would join across the hole.
                self.restart();
            } else if let Some(d) = h.dc
                && got > 0
            {
                let start = self.hist_end.saturating_sub(n as u64);
                while self
                    .dcs
                    .front()
                    .is_some_and(|&(p, _)| p < start || self.dcs.len() >= DC_MARKS)
                {
                    self.dcs.pop_front();
                }
                self.dcs.push_back((self.hist_end, d));
            }
        }
        self.resync_stale(rx);
    }

    /// Frames queued with no header after them, more than one block's
    /// worth: their headers were lost (or never came), and the producer
    /// would drop every call into the full ring. Discard them; the next
    /// header resyncs by count.
    fn resync_stale(&mut self, rx: &mut Rx) {
        if rx.headers.slots() == 0 && rx.samples.slots() > SAMPLE_RING / 2 {
            rx.skip(u64::MAX);
            self.restart();
        }
    }

    /// Start the window afresh.
    fn restart(&mut self) {
        self.pos = 0;
        self.filled = 0;
        self.dcs.clear();
        self.low.reset();
        self.recent.clear();
        self.span.reset();
    }

    /// The blocker's state at the first block end in the window: its
    /// offset in the window and the state on channels 0 and 1.
    fn dc_seed(&self) -> Option<(usize, [f64; 2])> {
        let n = self.hist.len() as u64;
        let start = self.hist_end.checked_sub(n)?;
        self.dcs
            .iter()
            .find(|&&(p, _)| p >= start)
            .map(|&(p, d)| ((p - start) as usize, d))
    }

    fn step(&mut self, shared: &Shared, now: Instant) -> Option<HarmonicsLive> {
        let quiet = match self.last_header {
            Some(t) => now.duration_since(t) > IDLE_AFTER,
            None => now.duration_since(self.started) > IDLE_AFTER,
        };
        if quiet {
            return Some(empty_frame(HarmonicsLiveState::Idle, self.seq, self.rate));
        }
        self.last_header?;
        if !self.active {
            return Some(empty_frame(HarmonicsLiveState::Off, self.seq, self.rate));
        }
        if self.filled < self.hist.len() || self.hist.is_empty() {
            return Some(empty_frame(
                HarmonicsLiveState::Waiting,
                self.seq,
                self.rate,
            ));
        }
        if now < self.due {
            return None;
        }
        let t0 = Instant::now();
        self.refresh_curve(shared);
        let mut frame = self.analyse();
        let cost = t0.elapsed();
        frame.analysis_ms = cost.as_secs_f64() * 1e3;
        // The load guard: relative to the interval, never absolute.
        let share = cost.as_secs_f64() / self.interval.as_secs_f64();
        if share > SLOW_SHARE {
            self.interval = (self.interval * 2).min(MAX_INTERVAL);
        } else if share < FAST_SHARE && self.interval > BASE_INTERVAL {
            self.interval = (self.interval / 2).max(BASE_INTERVAL);
        }
        frame.interval_ms = self.interval.as_secs_f64() * 1e3;
        self.due = now + self.interval;
        self.seq += 1;
        frame.seq = self.seq;
        Some(frame)
    }

    fn refresh_curve(&mut self, shared: &Shared) {
        let side = usize::from(self.side.min(1));
        let config = shared.config.lock().ok().and_then(|c| c[side].clone());
        let fresh = self
            .curve
            .as_ref()
            .is_some_and(|c| c.rate == self.rate && c.config == config);
        if fresh {
            return;
        }
        let curve = match &config {
            Some(c) => live_curve(c, f64::from(self.rate)),
            None => Err("no harmonics stage".into()),
        };
        self.curve = Some(CurveCache {
            config,
            rate: self.rate,
            curve,
        });
    }

    /// The newest window, oldest first: the mono sum's input and output,
    /// and the input of channels 0 and 1.
    fn window(&self) -> [Vec<f64>; 4] {
        let n = self.hist.len();
        let mut out: [Vec<f64>; 4] = std::array::from_fn(|_| Vec::with_capacity(n));
        for i in 0..n {
            let f = self.hist[(self.pos + i) % n];
            for (o, v) in out.iter_mut().zip(f) {
                o.push(f64::from(v));
            }
        }
        out
    }

    /// Tests only: the added content is `y − g x` with the stage's linear
    /// gain as `g`, no fit (the truth for a stage at full drive without
    /// sag; the headline's [`SpanFit`] also turns it down by the sag, as
    /// the stage does).
    fn fixed_gain(&self) -> bool {
        #[cfg(test)]
        return self.test_fixed_gain;
        #[cfg(not(test))]
        false
    }

    fn analyse(&mut self) -> HarmonicsLive {
        let rate = self.rate;
        let n = self.hist.len();
        let [x, y, x0, x1] = self.window();
        let low = self.low.window();
        let fixed = self.fixed_gain();
        let spec = self.spec.get_or_insert_with(|| Spectral::new(n, rate));
        let mut frame = empty_frame(HarmonicsLiveState::Playing, self.seq, rate);
        frame.window_ms = n as f64 / f64::from(rate.max(1)) * 1e3;
        frame.linear_gain_db = db(self.linear_gain.abs());
        frame.layers = self
            .layers
            .iter()
            .map(|l| HarmonicsLiveLayer {
                layer: l.layer as u32,
                sag_db: round(l.sag_db, 1e3),
                bias: round(l.bias, 1e4),
            })
            .collect();

        let sum_x2: f64 = x.iter().map(|v| v * v).sum();
        let rms = (sum_x2 / n as f64).sqrt();
        let peak = x.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
        frame.music_rms_dbfs = round(db(rms), 100.0);
        frame.music_level_dbfs = round((db(rms) + 3.0103).max(DB_FLOOR), 100.0);
        frame.music_peak_dbfs = round(db(peak), 100.0);
        if db(rms) < SILENT_DBFS {
            frame.state = HarmonicsLiveState::Silent;
            return frame;
        }

        let w = spec.window();
        let w = &w[..];
        let sxx: f64 = w.iter().zip(&x).map(|(w, x)| w * x * x).sum();
        let proj = Projection::new(w, &x);
        let g = match proj.of(&y) {
            Some(g) if !fixed => g,
            _ => self.linear_gain,
        };
        // What is left, its DC out (the even terms' offset, which a stage
        // without its DC blocker passes; not heard, and not a product).
        let mut added: Vec<f64> = x.iter().zip(&y).map(|(a, b)| b - g * a).collect();
        let sw: f64 = w.iter().sum();
        let dc = added.iter().zip(w).map(|(a, w)| a * w).sum::<f64>() / sw;
        for a in &mut added {
            *a -= dc;
        }
        let music_out = g * g * sxx;
        let saa: f64 = added.iter().zip(w).map(|(a, w)| w * a * a).sum();
        let mut sums = WindowSums {
            start: self.span.pushed.saturating_sub(n as u64),
            end: self.span.pushed,
            added: saa,
            music_out,
            sxx,
            peak: peak_ratio(&x, &added, g),
            split: SplitSums::None,
        };

        let music = spec.power(&x);
        frame.centroid_hz = round(spec.centroid(&music), 10.0);
        let a_pow = spec.power(&added);
        let long = low.and_then(|(xl, yl)| spec.long_powers(&xl, &yl, g, fixed));
        let (music_long, added_long) = match &long {
            Some((m, a)) => (Some(&m[..]), Some(&a[..])),
            None => (None, None),
        };
        frame.music = spec.bins(&music, music_long);
        frame.added = spec.bins(&a_pow, added_long);
        #[cfg(test)]
        {
            let long_only = |p: Option<&[f64]>| {
                let mut out = vec![0.0; spec.count];
                if let Some(p) = p {
                    spec.long_bins.read(p, &mut out);
                }
                out
            };
            self.probe = Some([
                spec.read(&music, None),
                spec.read(&a_pow, None),
                long_only(music_long),
                long_only(added_long),
            ]);
        }

        match self.curve.as_ref().map(|c| &c.curve) {
            Some(Ok(_)) if self.channels > 2 => {
                frame.split_note = Some(
                    "the split is evaluated per channel and is shown for up to two channels".into(),
                );
            }
            Some(Ok(curve)) if gains_match(curve.linear_gain(), self.linear_gain) => {
                // The stage's curve acts on each channel: the parts of the
                // mono sum are the mean of each channel's.
                let seed = self.dc_seed();
                let parts = &mut self.parts;
                let orders = curve.split() == LiveSplit::Orders;
                let mut fit =
                    curve.eval_with(&x0, parts, &mut self.scratch, seed.map(|(i, d)| (i, d[0])));
                let even = &mut self.even;
                even.clear();
                even.extend_from_slice(self.scratch.even_total());
                if self.channels == 2 {
                    let right = &mut self.right;
                    let f = curve.eval_with(
                        &x1,
                        right,
                        &mut self.scratch,
                        seed.map(|(i, d)| (i, d[1])),
                    );
                    fit.exact &= f.exact;
                    fit.agrees &= f.agrees;
                    for (p, r) in parts.iter_mut().zip(right.iter()) {
                        for (a, b) in p.iter_mut().zip(r) {
                            *a = 0.5 * (*a + b);
                        }
                    }
                    for (a, b) in even.iter_mut().zip(self.scratch.even_total()) {
                        *a = 0.5 * (*a + b);
                    }
                }
                // Each part's DC (the stage's blockers) and what correlates
                // with the music (the level change, fitted as for the
                // total) out; its power and peak.
                let finish = |p: &mut [f64]| -> Part {
                    let c = proj.of(p).unwrap_or(0.0);
                    for (v, xi) in p.iter_mut().zip(&x) {
                        *v -= c * xi;
                    }
                    let mean: f64 = p.iter().zip(w).map(|(v, w)| v * w).sum::<f64>() / sw;
                    for v in p.iter_mut() {
                        *v -= mean;
                    }
                    Part {
                        power: p.iter().zip(w).map(|(v, w)| w * v * v).sum(),
                        peak: peak_ratio(&x, p, g),
                    }
                };
                let levels: Vec<Part> = parts.iter_mut().map(|p| finish(p)).collect();
                sums.split = if orders {
                    let even = finish(even);
                    let ks = curve.orders().to_vec();
                    // The odd orders' sum, for the even/odd fallback.
                    let odd = &mut self.right;
                    odd.truncate(1);
                    odd.resize_with(1, Vec::new);
                    odd[0].clear();
                    odd[0].resize(n, 0.0);
                    for (p, &k) in parts.iter().zip(&ks) {
                        if k % 2 == 1 {
                            for (a, b) in odd[0].iter_mut().zip(p) {
                                *a += b;
                            }
                        }
                    }
                    let odd = finish(&mut odd[0]);
                    SplitSums::Orders {
                        ks,
                        orders: levels,
                        even,
                        odd,
                        agrees: fit.agrees,
                    }
                } else {
                    let mut it = levels.into_iter();
                    let (even, odd) =
                        (it.next().unwrap_or_default(), it.next().unwrap_or_default());
                    SplitSums::EvenOdd { even, odd }
                };
            }
            Some(Ok(_)) => {
                frame.split_note = Some("the stage is changing".into());
            }
            Some(Err(why)) => frame.split_note = Some(why.clone()),
            None => {}
        }
        // The newest window joins the recent ones; the headline is their
        // power average.
        let span = (AVERAGE_S * f64::from(rate)) as u64;
        let from = self.span.pushed.saturating_sub(span);
        self.recent.push_back(sums);
        while self.recent.front().is_some_and(|e| e.start < from) {
            self.recent.pop_front();
        }
        self.average(&mut frame, fixed);
        frame
    }

    /// The headline numbers of `frame` from the span's fit
    /// ([`SpanFit`]): the added level is its added power over its music
    /// power, the peak its loudest segment, the level change its
    /// power-weighted gain. Each part is the headline times the part's
    /// share of the added content in the recent windows split alike (each
    /// window's own split, weighted by what the span's fit reads there);
    /// its peak the window's, scaled as the span's fit reads that window.
    fn average(&self, frame: &mut HarmonicsLive, fixed: bool) {
        let Some(newest) = self.recent.back() else {
            return;
        };
        let segs = self.span.fit(self.linear_gain, fixed);
        let own = |e: &WindowSums| SpanRead {
            added: e.added,
            music: e.music_out,
            sxx: e.sxx,
            peak: e.peak,
        };
        // The headline: the whole span, or (before its first group, which
        // cannot happen: a window is a group) the windows' own fits.
        let total = match &segs {
            Some(segs) => span_read(0, u64::MAX, segs),
            None => None,
        };
        let total = total.unwrap_or_else(|| {
            self.recent.iter().map(own).fold(
                SpanRead {
                    added: 0.0,
                    music: 0.0,
                    sxx: 0.0,
                    peak: 0.0,
                },
                |a, r| SpanRead {
                    added: a.added + r.added,
                    music: a.music + r.music,
                    sxx: a.sxx + r.sxx,
                    peak: a.peak.max(r.peak),
                },
            )
        });
        let ratio = if total.music > 0.0 {
            total.added / total.music
        } else {
            0.0
        };
        frame.added_db = round(db(ratio.sqrt()), 100.0);
        frame.added_pct = round(100.0 * ratio.sqrt(), 1e4);
        frame.added_peak_db = round(db(total.peak.sqrt()), 100.0);
        frame.level_change_db = if self.linear_gain != 0.0 && total.music > 0.0 && total.sxx > 0.0 {
            round(
                db((total.music / total.sxx).sqrt() / self.linear_gain.abs()),
                1e4,
            )
        } else {
            0.0
        };

        // The newest windows split alike, each as the span's fit reads it.
        let run: Vec<(&WindowSums, SpanRead)> = self
            .recent
            .iter()
            .rev()
            .take_while(|e| e.split.alike(&newest.split))
            .filter_map(|e| match segs.as_deref() {
                // A window the span no longer holds counts no more.
                Some(s) => span_read(e.start, e.end, s).map(|r| (e, r)),
                None => Some((e, own(e))),
            })
            .collect();
        let weight: f64 = run.iter().map(|(_, r)| r.added).sum();
        let part = |key: &str, order: Option<u32>, pick: &dyn Fn(&SplitSums) -> Part| {
            let (mut share, mut peak) = (0.0, 0.0_f64);
            for (e, r) in &run {
                if e.added <= 0.0 {
                    continue;
                }
                let p = pick(&e.split);
                share += p.power / e.added * r.added;
                if r.music > 0.0 {
                    peak = peak.max(p.peak * (r.added / r.music) / (e.added / e.music_out));
                }
            }
            let share = if weight > 0.0 { share / weight } else { 0.0 };
            HarmonicsLivePart {
                key: key.into(),
                order,
                level_db: round(db((ratio * share).sqrt()), 100.0),
                peak_db: round(db(peak.sqrt()), 100.0),
            }
        };
        match &newest.split {
            SplitSums::None => {}
            SplitSums::EvenOdd { .. } => {
                frame.split = HarmonicsLiveSplit::EvenOdd;
                frame.parts = vec![
                    part("even", None, &|s| s.even()),
                    part("odd", None, &|s| s.odd()),
                ];
            }
            SplitSums::Orders { ks, .. } => {
                let orders: Vec<HarmonicsLivePart> = ks
                    .iter()
                    .enumerate()
                    .map(|(i, &k)| {
                        part(&format!("h{k}"), Some(k as u32), &|s| match s {
                            SplitSums::Orders { orders, .. } => orders[i],
                            _ => Part::default(),
                        })
                    })
                    .collect();
                let even = part("even", None, &|s| s.even());
                let agrees = run
                    .iter()
                    .all(|(e, _)| matches!(e.split, SplitSums::Orders { agrees, .. } if agrees))
                    && orders.iter().all(|p| {
                        p.order.is_some_and(|k| k % 2 == 1)
                            || p.level_db <= even.level_db + AGREE_DB
                    });
                if agrees {
                    frame.split = HarmonicsLiveSplit::Orders;
                    frame.parts = orders;
                } else {
                    // The even orders' terms cancel each other here: their
                    // lines would mislead, their total does not.
                    frame.split = HarmonicsLiveSplit::EvenOdd;
                    frame.parts = vec![even, part("odd", None, &|s| s.odd())];
                    frame.split_note = Some(
                        "the even orders' terms cancel each other at this level, so they are shown as one even total".into(),
                    );
                }
            }
        }
    }
}

/// The fit of `g` behind the headline: the mono sum's input `x` and output
/// `y` of every frame as sums per segment (1/8 of a window: the peak's
/// grain) in groups of eight (a window's length: the grain of the DC and of
/// the music's level), the newest [`AVERAGE_S`] of them. The added content
/// is `a = y − g x` with one fit of `g` over the whole span, so music whose
/// power sits in the bass (most music; pink noise) gives it many
/// independent stretches: a window's own fit has a few, and the added
/// content that happens to correlate with the music in them (H2 raises the
/// treble's gain on the bass wave's crests) goes into `g`, reading the
/// added level low (0.4 to 1.1 dB on noise-like music). The level changes
/// that are no added content follow the span: `g` per segment is
/// `β₀ + β₁ ℓ + β₂ d`, `ℓ` the music's power over the group (the curve's own
/// compression on loud passages) and `d` the sag's drive gain from the tap
/// (sag's turn-down), each left out while it does not change. Faster gain
/// changes (sag's attack, the envelope's ripple) stay in `a` as the
/// sidebands they make, as in a window.
struct SpanFit {
    seg_len: usize,
    /// Complete groups fitted.
    groups: usize,
    cur: Seg,
    /// Complete segments, oldest first, from a group's start.
    segs: VecDeque<Seg>,
    /// Segments completed since the reset.
    done: u64,
    /// Frames pushed since the reset.
    pushed: u64,
}

/// One segment's sums: frames, `Σx`, `Σy`, `Σx²`, `Σxy`, `Σy²`, the sag's
/// drive gain summed per frame, and where it ends (frames pushed).
#[derive(Clone, Copy, Default)]
struct Seg {
    n: f64,
    sx: f64,
    sy: f64,
    sxx: f64,
    sxy: f64,
    syy: f64,
    sag: f64,
    end: u64,
}

/// One segment as the fit reads it: where it starts and ends (frames
/// pushed), the added content's power around its group's DC, the music
/// out's (`g² Σx²`) and in's (`Σx²`).
#[derive(Clone, Copy)]
struct SegRead {
    start: u64,
    end: u64,
    added: f64,
    music: f64,
    sxx: f64,
}

/// A stretch as the span's fit reads it: the added content's power, the
/// music out's and in's, and the loudest segment's power ratio.
#[derive(Clone, Copy)]
struct SpanRead {
    added: f64,
    music: f64,
    sxx: f64,
    peak: f64,
}

/// The segments within `start..end` (frames pushed) as the span's fit
/// reads them (`None`: none); segments with under 1/1000 of their mean
/// music power have no peak, as in [`peak_ratio`].
fn span_read(start: u64, end: u64, segs: &[SegRead]) -> Option<SpanRead> {
    let inside = || segs.iter().filter(|s| s.start >= start && s.end <= end);
    let count = inside().count();
    if count == 0 {
        return None;
    }
    let (mut added, mut music, mut sxx) = (0.0, 0.0, 0.0);
    for s in inside() {
        added += s.added;
        music += s.music;
        sxx += s.sxx;
    }
    let floor = 1e-3 * sxx / count as f64;
    let peak = inside()
        .filter(|s| s.sxx > floor && s.music > 0.0)
        .fold(0.0_f64, |b, s| b.max(s.added / s.music));
    Some(SpanRead {
        added,
        music,
        sxx,
        peak,
    })
}

impl SpanFit {
    fn new(rate: u32) -> Self {
        let n = if rate == 0 { 0 } else { fft_size(rate) };
        let groups = if n == 0 {
            0
        } else {
            ((AVERAGE_S * f64::from(rate) / n as f64) as usize).max(1)
        };
        Self {
            seg_len: n / PEAK_SEGMENTS,
            groups,
            cur: Seg::default(),
            segs: VecDeque::with_capacity((groups + 1) * PEAK_SEGMENTS),
            done: 0,
            pushed: 0,
        }
    }

    fn reset(&mut self) {
        self.cur = Seg::default();
        self.segs.clear();
        self.done = 0;
        self.pushed = 0;
    }

    #[inline]
    fn push(&mut self, x: f64, y: f64, sag: f64) {
        if self.seg_len == 0 {
            return;
        }
        self.pushed += 1;
        let c = &mut self.cur;
        c.n += 1.0;
        c.sx += x;
        c.sy += y;
        c.sxx += x * x;
        c.sxy += x * y;
        c.syy += y * y;
        c.sag += sag;
        if c.n as usize >= self.seg_len {
            c.end = self.pushed;
            self.segs.push_back(std::mem::take(c));
            self.done += 1;
            if self.segs.len() > (self.groups + 1) * PEAK_SEGMENTS {
                self.segs.drain(..PEAK_SEGMENTS);
            }
        }
    }

    /// Fit `g` over the newest complete groups and read every segment
    /// from the oldest of them on (when `fixed`, tests only: `g` is `g0`,
    /// the stage's linear part, times the sag's drive gain, as the stage
    /// turns it down); `None` before the first group.
    fn fit(&self, g0: f64, fixed: bool) -> Option<Vec<SegRead>> {
        let partial = (self.done % PEAK_SEGMENTS as u64) as usize;
        let usable = self.segs.len().checked_sub(partial)?;
        let ng = (usable / PEAK_SEGMENTS).min(self.groups);
        if ng == 0 {
            return None;
        }
        let segs: Vec<Seg> = self
            .segs
            .range(usable - ng * PEAK_SEGMENTS..)
            .copied()
            .collect();
        let fitted = ng * PEAK_SEGMENTS;
        // The last group may be partial: read, not fitted.
        let groups: Vec<&[Seg]> = segs.chunks(PEAK_SEGMENTS).collect();
        // The regressors, each relative to its mean over the fitted span:
        // the group's music power and the segment's drive gain.
        let power: Vec<f64> = groups
            .iter()
            .map(|g| {
                let (n, sx, sxx) = g.iter().fold((0.0, 0.0, 0.0), |(n, a, b), s| {
                    (n + s.n, a + s.sx, b + s.sxx)
                });
                (sxx / n - (sx / n).powi(2)).max(0.0)
            })
            .collect();
        let mean_power = power[..ng].iter().sum::<f64>() / ng as f64;
        let drive: Vec<f64> = segs.iter().map(|s| s.sag / s.n).collect();
        let mean_drive = drive[..fitted].iter().sum::<f64>() / fitted as f64;
        let rel = |v: f64, mean: f64| if mean > 0.0 { v / mean - 1.0 } else { 0.0 };
        let level: Vec<f64> = power.iter().map(|&p| rel(p, mean_power)).collect();
        let sag: Vec<f64> = drive.iter().map(|&d| rel(d, mean_drive)).collect();
        let mut use_level = level[..ng].iter().any(|v| v.abs() > 1e-3);
        let mut use_sag = sag[..fitted].iter().any(|v| v.abs() > 1e-6);
        let regs = |si: usize, use_level: bool, use_sag: bool| -> [f64; 3] {
            [
                1.0,
                if use_level {
                    level[si / PEAK_SEGMENTS]
                } else {
                    0.0
                },
                if use_sag { sag[si] } else { 0.0 },
            ]
        };
        // Least squares over the fitted groups, each around its own DC.
        let beta = loop {
            if fixed {
                break [0.0; 3];
            }
            let mut a = [[0.0; 3]; 3];
            let mut b = [0.0; 3];
            for (gi, g) in groups[..ng].iter().enumerate() {
                let (mut u, mut n, mut sy) = ([0.0; 3], 0.0, 0.0);
                for (j, s) in g.iter().enumerate() {
                    let r = regs(gi * PEAK_SEGMENTS + j, use_level, use_sag);
                    for p in 0..3 {
                        u[p] += r[p] * s.sx;
                        b[p] += r[p] * s.sxy;
                        for q in 0..3 {
                            a[p][q] += r[p] * r[q] * s.sxx;
                        }
                    }
                    n += s.n;
                    sy += s.sy;
                }
                for p in 0..3 {
                    b[p] -= u[p] * sy / n;
                    for q in 0..3 {
                        a[p][q] -= u[p] * u[q] / n;
                    }
                }
            }
            // A regressor left out: an identity row.
            for (p, on) in [(1, use_level), (2, use_sag)] {
                if !on {
                    a[p] = [0.0; 3];
                    a[p][p] = 1.0;
                    b[p] = 0.0;
                }
            }
            match solve3(a, b) {
                Some(beta) => break beta,
                None if use_level || use_sag => (use_level, use_sag) = (false, false),
                None => return None,
            }
        };
        let gain = |si: usize| -> f64 {
            if fixed {
                return g0 * drive[si];
            }
            let r = regs(si, use_level, use_sag);
            beta[0] * r[0] + beta[1] * r[1] + beta[2] * r[2]
        };
        let mut out = Vec::with_capacity(segs.len());
        for (gi, g) in groups.iter().enumerate() {
            let at = gi * PEAK_SEGMENTS;
            let (mut n, mut sa) = (0.0, 0.0);
            for (j, s) in g.iter().enumerate() {
                n += s.n;
                sa += s.sy - gain(at + j) * s.sx;
            }
            // The group's DC.
            let m = sa / n;
            for (j, s) in g.iter().enumerate() {
                let k = gain(at + j);
                let added = s.syy - 2.0 * k * s.sxy + k * k * s.sxx - 2.0 * m * (s.sy - k * s.sx)
                    + s.n * m * m;
                out.push(SegRead {
                    start: s.end - s.n as u64,
                    end: s.end,
                    added: added.max(0.0),
                    music: k * k * s.sxx,
                    sxx: s.sxx,
                });
            }
        }
        Some(out)
    }
}

/// `a β = b` for a 3×3 system (partial pivoting); `None` when singular.
fn solve3(mut a: [[f64; 3]; 3], mut b: [f64; 3]) -> Option<[f64; 3]> {
    let scale = a
        .iter()
        .map(|r| r.iter().fold(0.0_f64, |m, v| m.max(v.abs())))
        .fold(0.0_f64, f64::max);
    if scale <= 0.0 {
        return None;
    }
    for c in 0..3 {
        let p = (c..3).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs()))?;
        if a[p][c].abs() <= 1e-12 * scale {
            return None;
        }
        a.swap(c, p);
        b.swap(c, p);
        for r in c + 1..3 {
            let f = a[r][c] / a[c][c];
            for k in c..3 {
                a[r][k] -= f * a[c][k];
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = [0.0; 3];
    for c in (0..3).rev() {
        let s: f64 = (c + 1..3).map(|k| a[c][k] * x[k]).sum();
        x[c] = (b[c] - s) / a[c][c];
    }
    Some(x)
}

/// One window's sums behind the headline: powers are Hann-weighted sums
/// over the window, peaks power ratios against the music out.
struct WindowSums {
    /// Where the window starts and ends ([`SpanFit`]'s frames pushed).
    start: u64,
    end: u64,
    /// The added content's power.
    added: f64,
    /// The music out's power (`g² Σ w x²`).
    music_out: f64,
    /// The music in's power (`Σ w x²`).
    sxx: f64,
    peak: f64,
    split: SplitSums,
}

/// One part's power and peak in a window.
#[derive(Clone, Copy, Default)]
struct Part {
    power: f64,
    peak: f64,
}

/// One window's split of the added content.
enum SplitSums {
    None,
    /// By order (`ks`), with the even total and the odd orders' sum for
    /// the fallback when the even orders cancel; `agrees`: the curve's
    /// evaluation agrees with itself.
    Orders {
        ks: Vec<usize>,
        orders: Vec<Part>,
        even: Part,
        odd: Part,
        agrees: bool,
    },
    EvenOdd {
        even: Part,
        odd: Part,
    },
}

impl SplitSums {
    /// Split the same way (averaged together).
    fn alike(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Orders { ks: a, .. }, Self::Orders { ks: b, .. }) => a == b,
            (Self::EvenOdd { .. }, Self::EvenOdd { .. }) => true,
            _ => false,
        }
    }

    fn even(&self) -> Part {
        match self {
            Self::Orders { even, .. } | Self::EvenOdd { even, .. } => *even,
            Self::None => Part::default(),
        }
    }

    fn odd(&self) -> Part {
        match self {
            Self::Orders { odd, .. } | Self::EvenOdd { odd, .. } => *odd,
            Self::None => Part::default(),
        }
    }
}

/// The least-squares coefficient of the music `x` in a signal over a Hann
/// window, both with their DC out.
struct Projection<'a> {
    w: &'a [f64],
    x: &'a [f64],
    /// The music's weighted mean and `Σ w (x − mean)²`.
    mx: f64,
    sxx: f64,
}

impl<'a> Projection<'a> {
    fn new(w: &'a [f64], x: &'a [f64]) -> Self {
        let (mut sw, mut s1, mut s2) = (0.0, 0.0, 0.0);
        for (w, x) in w.iter().zip(x) {
            sw += w;
            s1 += w * x;
            s2 += w * x * x;
        }
        let mx = if sw > 0.0 { s1 / sw } else { 0.0 };
        Self {
            w,
            x,
            mx,
            sxx: s2 - sw * mx * mx,
        }
    }

    /// The coefficient of the music in `v` (`None`: no music to fit on).
    fn of(&self, v: &[f64]) -> Option<f64> {
        if self.sxx <= 1e-30 {
            return None;
        }
        let (mut sv, mut sxv) = (0.0, 0.0);
        for ((w, x), v) in self.w.iter().zip(self.x).zip(v) {
            sv += w * v;
            sxv += w * x * v;
        }
        Some((sxv - self.mx * sv) / self.sxx)
    }
}

/// The running stage is the curve's (its linear gain, to rounding).
fn gains_match(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1e-12)
}

/// The loudest of [`PEAK_SEGMENTS`] segments of `part` against the music
/// out there (`g x`), a power ratio; segments where the music is under
/// 1/1000 of its mean power are skipped.
fn peak_ratio(x: &[f64], part: &[f64], g: f64) -> f64 {
    let n = x.len();
    let seg = (n / PEAK_SEGMENTS).max(1);
    let mean: f64 = x.iter().map(|v| v * v).sum::<f64>() / PEAK_SEGMENTS as f64;
    let mut best = 0.0_f64;
    for s in 0..PEAK_SEGMENTS {
        let r = s * seg..((s + 1) * seg).min(n);
        let mx: f64 = x[r.clone()].iter().map(|v| v * v).sum();
        if mx <= 1e-3 * mean || mx <= 0.0 || g == 0.0 {
            continue;
        }
        let mp: f64 = part[r].iter().map(|v| v * v).sum();
        best = best.max(mp / (g * g * mx));
    }
    best
}

fn db(lin: f64) -> f64 {
    if lin > 0.0 && lin.is_finite() {
        (20.0 * lin.log10()).max(DB_FLOOR)
    } else {
        DB_FLOOR
    }
}

fn round(v: f64, k: f64) -> f64 {
    (v * k).round() / k
}

/// The mono sum's input and output decimated for the spectra's low band:
/// [`Binomial`] stages while the rate is high, then half-band stages (the
/// PEQ analyser's), and the newest span of pairs.
struct LowBand {
    pre: Vec<Binomial>,
    stages: Vec<HalfBandStage>,
    hist: Vec<[f64; 2]>,
    pos: usize,
    filled: usize,
}

/// Stages at this input rate and above are [`Binomial`].
const BINOMIAL_FROM_HZ: f64 = 192_000.0;

/// A /2 decimator of (input, output) pairs with the kernel
/// `[1, 4, 6, 4, 1] / 16`, for the first stages, at
/// [`BINOMIAL_FROM_HZ`] and above, where the low band (under 3.5 kHz) is a
/// small share of the rate: what aliases into it is at least 99 dB down
/// and its droop there is under 0.01 dB in all, at a fraction of a
/// half-band's cost.
#[derive(Clone, Copy, Default)]
struct Binomial {
    /// The last four inputs, oldest first.
    h: [[f64; 2]; 4],
    odd: bool,
}

impl Binomial {
    #[inline]
    fn push(&mut self, v: [f64; 2]) -> Option<[f64; 2]> {
        let [a, b, c, d] = self.h;
        self.h = [b, c, d, v];
        self.odd = !self.odd;
        if self.odd {
            return None;
        }
        Some(std::array::from_fn(|i| {
            (a[i] + v[i] + 4.0 * (b[i] + d[i]) + 6.0 * c[i]) * (1.0 / 16.0)
        }))
    }
}

/// Halving stages to the low band's rate from `rate`.
fn low_stages(rate: u32) -> usize {
    let mut r = f64::from(rate);
    let mut s = 0;
    while r / 2.0 >= LOW_MIN_HZ {
        r /= 2.0;
        s += 1;
    }
    s
}

/// The low band's FFT length at its rate: the power of two at or below
/// [`LOW_SPAN_S`], at least 1024.
fn low_size(low_rate: f64) -> usize {
    let target = (low_rate * LOW_SPAN_S) as usize;
    let mut n = 1024;
    while n * 2 <= target {
        n *= 2;
    }
    n
}

impl LowBand {
    fn new(rate: u32) -> Self {
        if rate == 0 {
            return Self {
                pre: Vec::new(),
                stages: Vec::new(),
                hist: Vec::new(),
                pos: 0,
                filled: 0,
            };
        }
        let k = low_stages(rate);
        let low = f64::from(rate) / f64::from(1u32 << k);
        let pre = (0..k)
            .take_while(|&s| f64::from(rate) / f64::from(1u32 << s) >= BINOMIAL_FROM_HZ)
            .count();
        let k = k - pre;
        Self {
            pre: vec![Binomial::default(); pre],
            stages: (0..k).map(|s| HalfBandStage::new(2, k - 1 - s)).collect(),
            hist: vec![[0.0; 2]; low_size(low)],
            pos: 0,
            filled: 0,
        }
    }

    fn reset(&mut self) {
        self.pre.fill(Binomial::default());
        for s in &mut self.stages {
            s.reset();
        }
        self.pos = 0;
        self.filled = 0;
    }

    /// One frame of the input `x` and output `y`.
    #[inline]
    fn push(&mut self, x: f64, y: f64) {
        let n = self.hist.len();
        if n == 0 {
            return;
        }
        let mut v = [x, y];
        for s in &mut self.pre {
            match s.push(v) {
                Some(o) => v = o,
                None => return,
            }
        }
        for s in &mut self.stages {
            // Both channels step together: each has a sample out or neither.
            match (s.push(0, v[0]), s.push(1, v[1])) {
                (Some(a), Some(b)) => v = [a, b],
                _ => return,
            }
        }
        self.hist[self.pos] = v;
        self.pos = (self.pos + 1) % n;
        self.filled = (self.filled + 1).min(n);
    }

    /// The span, oldest first (input, output), once full.
    fn window(&self) -> Option<(Vec<f64>, Vec<f64>)> {
        let n = self.hist.len();
        if n == 0 || self.filled < n {
            return None;
        }
        let (mut x, mut y) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for i in 0..n {
            let [a, b] = self.hist[(self.pos + i) % n];
            x.push(a);
            y.push(b);
        }
        Some((x, y))
    }
}

/// A Hann-windowed FFT of one length: power per bin, a full-scale sine's
/// peak bin = 1.
struct Fft {
    fft: RealFftPair,
    window: Arc<[f64]>,
    buf: Vec<f64>,
    spec: Vec<Complex64>,
    /// `(Σ w / 2)²`: a full-scale sine's peak bin power.
    norm: f64,
}

impl Fft {
    fn new(n: usize) -> Self {
        let window: Arc<[f64]> = (0..n)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos())
            .collect();
        let half: f64 = window.iter().sum::<f64>() / 2.0;
        let fft = RealFftPair::new(n);
        let spec = vec![Complex64::new(0.0, 0.0); fft.spec_len()];
        Self {
            fft,
            buf: vec![0.0; n],
            window,
            spec,
            norm: half * half,
        }
    }

    fn power(&mut self, v: &[f64]) -> Vec<f64> {
        for ((b, &s), &w) in self.buf.iter_mut().zip(v).zip(self.window.iter()) {
            *b = s * w;
        }
        self.fft.forward_padded_complex(&self.buf, &mut self.spec);
        self.spec.iter().map(|c| c.norm_sqr() / self.norm).collect()
    }
}

/// Log bins at least this many FFT bins wide hold a sine's main lobe:
/// their band power reads its level, so no peak is searched for there.
const TONAL_BELOW_BINS: f64 = 16.0;

/// How one FFT resolution reads its log bins.
struct Reader {
    bin_hz: f64,
    /// FFT bins searched for sine peaks (those of the bins narrower than
    /// [`TONAL_BELOW_BINS`], and a margin for the search's wings).
    tonal_bins: usize,
    bands: Vec<LogBand>,
    /// Log-grid index -> entry of `bands`.
    slot_of: Vec<Option<usize>>,
}

impl Reader {
    /// The log bins of a `count`-bin grid that `keep(lo, hi)` accepts, from
    /// an FFT of `bin_hz` per bin and `max_bin` the last, edges clamped to
    /// `nyq`.
    fn new(
        count: usize,
        bin_hz: f64,
        max_bin: usize,
        nyq: f64,
        keep: impl Fn(f64, f64) -> bool,
    ) -> Self {
        let half = 2f64.powf(0.5 / f64::from(BINS_PER_OCTAVE));
        let mut bands = Vec::new();
        let mut slot_of = vec![None; count];
        let mut tonal_bins = 0;
        for (index, slot) in slot_of.iter_mut().enumerate() {
            let c = F_MIN_HZ * 2f64.powf(index as f64 / f64::from(BINS_PER_OCTAVE));
            let (lo, hi) = (c / half, (c * half).min(nyq));
            if !keep(lo, hi) {
                continue;
            }
            *slot = Some(bands.len());
            if hi - lo < TONAL_BELOW_BINS * bin_hz {
                tonal_bins = ((hi / bin_hz).ceil() as usize + 10).min(max_bin + 1);
            }
            bands.push(log_band(index, lo, hi, bin_hz, max_bin));
        }
        Self {
            bin_hz,
            tonal_bins,
            bands,
            slot_of,
        }
    }

    /// Per log bin it serves, into `out` (linear power): the interpolated
    /// band power, or a sine's peak level where the bin holds one, whichever
    /// is higher.
    fn read(&self, p: &[f64], out: &mut [f64]) {
        let mut tonal = vec![0.0; self.bands.len()];
        let searched = &p[..self.tonal_bins.min(p.len())];
        tonal_peaks(searched, self.bin_hz, &self.slot_of, &mut tonal);
        for (band, t) in self.bands.iter().zip(tonal) {
            out[band.index] = band_power(p, band).max(t);
        }
    }
}

/// The FFTs and the log grid for one window size and rate.
struct Spectral {
    rate: u32,
    short: Fft,
    short_bins: Reader,
    /// The low band's FFT and the log bins it serves (none at very low
    /// rates).
    long: Fft,
    long_bins: Reader,
    count: usize,
}

impl Spectral {
    fn new(n: usize, rate: u32) -> Self {
        let short = Fft::new(n);
        let df = f64::from(rate) / n as f64;
        let nyq = f64::from(rate) / 2.0;
        let count = if nyq > F_MIN_HZ {
            (f64::from(BINS_PER_OCTAVE) * (nyq / F_MIN_HZ).log2()).floor() as usize + 1
        } else {
            1
        };
        let short_bins = Reader::new(count, df, n / 2, nyq, |_, _| true);
        let k = low_stages(rate);
        let low_rate = f64::from(rate) / f64::from(1u32 << k);
        let ln = low_size(low_rate);
        let long_bins = Reader::new(count, low_rate / ln as f64, ln / 2, nyq, |lo, hi| {
            hi - lo < CROSSOVER_BINS * df && hi <= LOW_PASS * low_rate
        });
        Self {
            rate,
            short,
            short_bins,
            long: Fft::new(ln),
            long_bins,
            count,
        }
    }

    fn window(&self) -> Arc<[f64]> {
        Arc::clone(&self.short.window)
    }

    /// Power per window FFT bin of `v` windowed, a full-scale sine's peak
    /// bin = 1.
    fn power(&mut self, v: &[f64]) -> Vec<f64> {
        self.short.power(v)
    }

    /// The low band's power spectra of the music and of the added content
    /// `a = y − g x` (`g` fitted over the span; `g0`, the window's, when the
    /// span has no music, or `fixed`), its DC out.
    fn long_powers(
        &mut self,
        x: &[f64],
        y: &[f64],
        g0: f64,
        fixed: bool,
    ) -> Option<(Vec<f64>, Vec<f64>)> {
        if self.long_bins.bands.is_empty() || x.len() != self.long.buf.len() {
            return None;
        }
        let w = Arc::clone(&self.long.window);
        let g = match Projection::new(&w, x).of(y) {
            Some(g) if !fixed => g,
            _ => g0,
        };
        let mut a: Vec<f64> = x.iter().zip(y).map(|(x, y)| y - g * x).collect();
        let sw: f64 = w.iter().sum();
        let dc = a.iter().zip(w.iter()).map(|(a, w)| a * w).sum::<f64>() / sw;
        for v in &mut a {
            *v -= dc;
        }
        Some((self.long.power(x), self.long.power(&a)))
    }

    /// Linear power per log bin: the window's FFT for every bin, the long
    /// span's (`long`) for the low band.
    fn read(&self, p: &[f64], long: Option<&[f64]>) -> Vec<f64> {
        let mut out = vec![0.0; self.count];
        self.short_bins.read(p, &mut out);
        if let Some(l) = long {
            self.long_bins.read(l, &mut out);
        }
        out
    }

    /// Per log bin, dB to 0.1.
    fn bins(&self, p: &[f64], long: Option<&[f64]>) -> Vec<f32> {
        self.read(p, long).into_iter().map(bin_db).collect()
    }

    /// Power-weighted mean frequency of `p` from 20 Hz to 20 kHz.
    fn centroid(&self, p: &[f64]) -> f64 {
        let df = f64::from(self.rate) / self.short.buf.len() as f64;
        let (mut num, mut den) = (0.0, 0.0);
        for (k, &v) in p.iter().enumerate() {
            let f = k as f64 * df;
            if (20.0..=20_000.0).contains(&f) {
                num += f * v;
                den += v;
            }
        }
        if den > 0.0 { num / den } else { 0.0 }
    }
}

/// Linear power to dB, rounded to 0.1 (the floor below it).
fn bin_db(v: f64) -> f32 {
    let d = if v > 0.0 {
        (10.0 * v.log10()).max(DB_FLOOR)
    } else {
        DB_FLOOR
    };
    ((d * 10.0).round() / 10.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_dsp_core::Stage;
    use audivane_dsp_core::harmonics::Harmonics;

    pub(super) fn shared(a: Option<HarmonicsConfig>, b: Option<HarmonicsConfig>) -> Shared {
        Shared {
            latest: Mutex::new(empty_frame(HarmonicsLiveState::Waiting, 0, 0)),
            last_poll: AtomicU64::new(0),
            start: Instant::now(),
            config: Mutex::new([a, b]),
            thread: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// One block of `chans` (one or two channels) through `stage`, tapped
    /// as the chain does (mono sum, channels 0 and 1).
    pub(super) fn push_block(
        rt: &mut HarmonicsLiveRt,
        stage: &mut Harmonics,
        chans: &[&[f64]],
        rate: u32,
        side: u8,
    ) {
        let n = chans[0].len();
        let mut ys: Vec<Vec<f64>> = chans.iter().map(|c| c.to_vec()).collect();
        {
            let mut refs: Vec<&mut [f64]> = ys.iter_mut().map(|v| &mut v[..]).collect();
            stage.process(&mut refs);
        }
        let k = chans.len() as f64;
        let mono = |v: &[Vec<f64>], i: usize| (v.iter().map(|c| c[i]).sum::<f64>() / k) as f32;
        let xs: Vec<Vec<f64>> = chans.iter().map(|c| c.to_vec()).collect();
        let xi: Vec<f32> = (0..n).map(|i| mono(&xs, i)).collect();
        let yi: Vec<f32> = (0..n).map(|i| mono(&ys, i)).collect();
        let x0: Vec<f32> = chans[0].iter().map(|&v| v as f32).collect();
        let x1: Vec<f32> = chans[chans.len().min(2) - 1]
            .iter()
            .map(|&v| v as f32)
            .collect();
        let mut layers = [LiveLayer::default(); HARMONICS_TAP_LAYERS];
        let n_layers = stage.live_layers(&mut layers);
        rt.push(
            Some(HarmonicsTap {
                input: &xi,
                output: &yi,
                input0: &x0,
                input1: &x1,
                channels: chans.len(),
                side,
                linear_gain: stage.linear_gain(),
                layers: &layers[..n_layers],
                dc: stage
                    .live_dc(0)
                    .zip(stage.live_dc(chans.len().min(2) - 1))
                    .map(|(a, b)| [a, b]),
            }),
            true,
            rate,
        );
    }

    /// Both ends of fresh rings, watched, and an analyser, without the
    /// thread: the tests drive it by hand.
    pub(super) struct Harness {
        pub rt: HarmonicsLiveRt,
        pub rx: Rx,
        pub an: Analyzer,
    }

    pub(super) fn harness() -> Harness {
        let (sample_tx, sample_rx) = rtrb::RingBuffer::<TapFrame>::new(SAMPLE_RING);
        let (header_tx, header_rx) = rtrb::RingBuffer::<Header>::new(HEADER_RING);
        Harness {
            rt: HarmonicsLiveRt {
                samples: sample_tx,
                headers: header_tx,
                enabled: Arc::new(AtomicBool::new(true)),
                epoch: 1,
                gap: false,
                written: 0,
            },
            rx: Rx {
                samples: sample_rx,
                headers: header_rx,
                consumed: 0,
            },
            an: Analyzer::new(Instant::now()),
        }
    }

    impl Harness {
        /// One block of `chans` (one or two channels) through `stage`,
        /// tapped as the chain does (mono sum, channels 0 and 1).
        pub fn block(&mut self, stage: &mut Harmonics, chans: &[&[f64]], rate: u32, side: u8) {
            push_block(&mut self.rt, stage, chans, rate, side);
        }

        /// `chans` in blocks of 1024, the analyser draining after each.
        pub fn feed(&mut self, stage: &mut Harmonics, chans: &[&[f64]], rate: u32) {
            let n = chans[0].len();
            let mut at = 0;
            while at < n {
                let end = (at + 1024).min(n);
                let block: Vec<&[f64]> = chans.iter().map(|c| &c[at..end]).collect();
                self.block(stage, &block, rate, 0);
                self.an.drain(&mut self.rx, Instant::now());
                at = end;
            }
        }

        pub fn frame(&mut self, sh: &Shared) -> HarmonicsLive {
            self.an.due = Instant::now();
            self.an.step(sh, Instant::now()).unwrap()
        }
    }

    fn sines(n: usize, rate: f64, f: &[(f64, f64)]) -> Vec<f64> {
        (0..n)
            .map(|i| {
                f.iter()
                    .map(|(hz, a)| a * (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin())
                    .sum()
            })
            .collect()
    }

    fn frame_for(config: &HarmonicsConfig, x: &[f64], rate: u32) -> HarmonicsLive {
        let mut h = harness();
        let mut stage = Harmonics::try_new(config, f64::from(rate), 1).unwrap();
        h.feed(&mut stage, &[x], rate);
        h.frame(&shared(Some(config.clone()), None))
    }

    /// A stage with H2 alone at `db` (no DC blocker: the measured level is
    /// the steady one).
    fn h2_only(db: f64) -> HarmonicsConfig {
        let mut cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
        let audivane_protocol::HarmonicsLayer::Chebyshev(l) = &mut cfg.layers[0] else {
            panic!("a Chebyshev preset");
        };
        l.levels_db = [Some(db), None, None, None, None, None, None, None];
        l.drive_db = 0.0;
        l.amount = 1.0;
        cfg
    }

    /// A sine through a stage of known harmonics: the added level is the
    /// stage's THD at that level, the per-order parts its harmonics, and no
    /// level change at full drive.
    #[test]
    fn a_sine_reads_the_stage_harmonics() {
        let rate = 96_000;
        let cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
        let readout = audivane_dsp_core::harmonics::readout(&cfg, f64::from(rate), false).unwrap();
        let p0 = &readout.points[0];
        // A full-scale 1 kHz sine (the readout's first point).
        let x = sines(rate as usize, f64::from(rate), &[(1000.0, 1.0)]);
        let f = frame_for(&cfg, &x, rate);
        assert_eq!(f.state, HarmonicsLiveState::Playing);
        assert_eq!(f.split, HarmonicsLiveSplit::Orders);
        let h2 = f.parts.iter().find(|p| p.key == "h2").unwrap();
        let want_h2 = p0.harmonics_db[0];
        assert!(
            (h2.level_db - want_h2).abs() < 0.3,
            "h2 {} vs readout {want_h2}",
            h2.level_db
        );
        assert!(
            (f.added_db - p0.thd_db).abs() < 0.3,
            "{} vs {}",
            f.added_db,
            p0.thd_db
        );
        assert!((f.music_level_dbfs - 0.0).abs() < 0.05);
        assert!(f.music.len() > 200 && f.music.len() == f.added.len());
    }

    /// The interleaving that desynchronized the rings: a block lands while
    /// a new subscription discards the queued headers, so its frames stay
    /// queued without one. The analyser skips them by count and reads the
    /// next block's own frames under its own header.
    #[test]
    fn a_header_lost_at_subscription_never_shifts_the_stream() {
        let mut h = harness();
        let cfg = h2_only(-30.0);
        // A 48 kHz block queued, then (the race) its header discarded.
        let mut s48 = Harmonics::try_new(&cfg, 48_000.0, 1).unwrap();
        let old = vec![0.25; 700];
        h.block(&mut s48, &[&old], 48_000, 0);
        h.rx.discard_headers();
        // The music at 96 kHz: distinct values, so a shifted read shows.
        let mut s96 = Harmonics::try_new(&cfg, 96_000.0, 1).unwrap();
        let ramp: Vec<f64> = (0..fft_size(96_000))
            .map(|i| (i % 1000) as f64 * 1e-4)
            .collect();
        for block in ramp.chunks(1024) {
            h.block(&mut s96, &[block], 96_000, 0);
            h.an.drain(&mut h.rx, Instant::now());
        }
        assert_eq!(h.an.rate, 96_000);
        assert_eq!(h.an.filled, ramp.len());
        let [x, ..] = h.an.window();
        for (i, (&got, &want)) in x.iter().zip(&ramp).enumerate() {
            assert!((got - want).abs() < 1e-6, "frame {i}: {got} vs {want}");
        }
        assert_eq!(h.rx.consumed, h.rt.written);
    }

    /// A sample ring full of frames whose headers were discarded (a reopen
    /// mid-subscription) does not wedge the analysis: the analyser discards
    /// them and resyncs.
    #[test]
    fn a_full_ring_without_headers_recovers() {
        let mut h = harness();
        let cfg = h2_only(-30.0);
        let mut stage = Harmonics::try_new(&cfg, 96_000.0, 1).unwrap();
        let x = sines(1024, 96_000.0, &[(1000.0, 0.5)]);
        while h.rt.samples.slots() >= x.len() {
            h.block(&mut stage, &[&x], 96_000, 0);
        }
        h.block(&mut stage, &[&x], 96_000, 0);
        assert!(h.rt.gap, "the ring is full");
        h.rx.discard_headers();
        let sines = sines(fft_size(96_000) * 2, 96_000.0, &[(1000.0, 0.5)]);
        h.feed(&mut stage, &[&sines], 96_000);
        assert_eq!(h.an.filled, fft_size(96_000));
        let f = h.frame(&shared(Some(cfg), None));
        assert_eq!(f.state, HarmonicsLiveState::Playing);
        assert_eq!(h.rx.consumed, h.rt.written);
    }
}
