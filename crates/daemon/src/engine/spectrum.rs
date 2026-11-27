//! Live spectrum analysis.
//!
//! The RT thread copies interleaved samples into two pre-allocated wait-free
//! SPSC rings (`rtrb`): `pre` is the chain input at the graph rate, `post` is
//! the floating-point chain output at the output rate, copied immediately
//! before the quantizer/dither. Pushes never allocate, lock, or block; when a
//! ring is full samples are dropped.
//!
//! Each queued sample carries the rate and an epoch. The RT side bumps the
//! epoch whenever its rate changes, and the analyser resets a tap when it sees
//! a new one, so old-rate samples are never read as new-rate ones. A push
//! that does not fit drops whole frames and starts a new epoch with the next
//! push; reads take whole frames only, so a read during a push never swaps
//! channels.
//!
//! A non-RT analysis thread runs two Hann FFTs on each tap: 8192 points
//! (50% overlap) for every log bin and 32768 points (87.5% overlap) for the
//! low band, where a 1/24-octave bin is narrower than a few short-FFT bins.
//! Above 88.2 kHz both run at the rate decimated to 44.1/48 kHz, so they
//! serve 10 Hz..20 kHz; a third Hann FFT at a higher rate (the tap's, or
//! 352.8/384 kHz) serves the bins above 20 kHz (`HighPath`).
//! Each log bin reads interpolated band power (with a sine's peak level where
//! the bin holds one), so the resolutions agree and narrow bins never
//! plateau. Power is averaged on a fixed log grid (10 Hz up to the tap's
//! Nyquist or [`F_MAX_HZ`], at 24 bins/octave) and a [`Spectrum`] published
//! at roughly the frame cadence (about 10-20 Hz). Nothing is computed while no client
//! subscribes to [`audivane_protocol::ipc::Topic::Spectrum`]: the RT thread
//! stops copying and the analyser drains its rings and reports nothing, as
//! with fewer than one complete frame.

use audivane_dsp_core::fft::RealFftPair;
use audivane_protocol::ipc::Spectrum;
use rustfft::num_complex::Complex64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Short FFT: every log bin (about 5.9 Hz per FFT bin at 48 kHz).
pub const FFT_SIZE: usize = 8192;
/// Long FFT for the low band (about 1.5 Hz per FFT bin at 48 kHz).
pub const LONG_FFT_SIZE: usize = 32_768;
pub const BINS_PER_OCTAVE: u32 = 24;
/// The grid's bottom edge. Below 20 Hz a log bin is a fifth to two fifths
/// of a long-FFT bin: it reads the interpolated band power (a sine its peak
/// level), so the trace stays smooth but a sine there is about 3 Hz wide.
pub const F_MIN_HZ: f64 = 10.0;
/// The highest frequency a tap reports: the graph's widest range.
pub const F_MAX_HZ: f64 = 100_000.0;
/// Where the analysis is decimated to 44.1/48 kHz, the short and long FFTs
/// serve the log bins up to this edge (the half-band decimator's passband)
/// and the high path the bins above.
const BASE_MAX_HZ: f64 = 20_000.0;
/// The high path's FFT bins are at most this wide (about 47 Hz: 12 or more
/// per log bin above 20 kHz).
const HIGH_BIN_HZ: f64 = 48.0;
/// Log bins stop this many FFT bins short of the analysed Nyquist: the
/// Hann FFT's last bins hold the window's mirror image of what lies just
/// below it, and the Nyquist bin itself double counts.
const NYQUIST_GUARD_BINS: f64 = 2.0;
/// Log bins narrower than this many short-FFT bins come from the long FFT.
const CROSSOVER_BINS: f64 = 4.0;
/// Ring capacity in frames of the pre tap, at the graph rate (about 85 ms
/// at 192 kHz, 21 ms at 768 kHz).
const RING_FRAMES: usize = 16_384;
/// Ring capacity of the post tap, at the output rate: 42.7 ms at the
/// highest output rate (3.072 MHz). A period larger than half of it (x64
/// at a 2048, 4096 or 8192 quantum) is folded ([`PostFold`]) so it still
/// fits whole; what does not fit anyway is dropped and the next push
/// starts a new epoch, so audio is never joined across a gap.
const POST_RING_FRAMES: usize =
    RING_FRAMES * (audivane_protocol::MAX_OUTPUT_RATE as usize / 384_000);
/// Frames the analyser takes from a ring per tick (10 ms): 65,536 frames,
/// twice the highest output rate's 30,720 per tick.
const READ_FRAMES: usize = RING_FRAMES * 4;
// The largest period (x64 at an 8192 quantum) fits half the post ring
// once folded, and the analyser drains faster than the highest output
// rate fills it.
const _: () = assert!((8192 * 64) >> MAX_FOLD_SHIFT <= POST_RING_FRAMES / 2);
const _: () = assert!(
    READ_FRAMES
        >= 2 * (audivane_protocol::MAX_OUTPUT_RATE as usize * TICK.as_millis() as usize / 1000)
);
/// Both FFTs advance by the same hop: 50% overlap for the short one, 87.5%
/// for the long one, so every report carries fresh data for both bands.
const HOP: usize = FFT_SIZE / 2;
const TICK: Duration = Duration::from_millis(10);
const DB_FLOOR: f64 = -160.0;
/// Equivalent noise bandwidth of a periodic Hann window, in FFT bins.
pub(crate) const HANN_ENBW: f64 = 1.5;

/// Number of log bins for a tap at `rate`: from 10 Hz, every bin whose upper
/// edge lies at or below [`spectrum_top_hz`] (none straddles the top).
pub fn spectrum_bin_count(rate: u32) -> usize {
    bins_up_to(spectrum_top_hz(rate))
}

/// The top edge of a tap's log grid at `rate`: [`F_MAX_HZ`], or the Nyquist
/// of the FFT that serves the top bins less `NYQUIST_GUARD_BINS`.
pub fn spectrum_top_hz(rate: u32) -> f64 {
    let rate = rate.max(1);
    let (top_rate, size) = if decimation_stages(rate) == 0 {
        (rate, FFT_SIZE)
    } else {
        let high = rate >> high_stages(rate);
        (high, high_fft_size(high))
    };
    let bin_hz = f64::from(top_rate) / size as f64;
    (f64::from(top_rate) / 2.0 - NYQUIST_GUARD_BINS * bin_hz).min(F_MAX_HZ)
}

/// The number of log bins whose upper edge (half a step above the centre)
/// lies at or below `edge_hz`; at least one.
fn bins_up_to(edge_hz: f64) -> usize {
    let steps = f64::from(BINS_PER_OCTAVE) * (edge_hz / F_MIN_HZ).log2() - 0.5;
    if steps.is_nan() || steps < 0.0 {
        return 1;
    }
    // A hair of slack so an edge exactly on a bin's upper edge keeps it.
    (steps + 1.0e-9).floor() as usize + 1
}

/// Centre frequency of log bin `index`: `F_MIN_HZ * 2^(index / 24)`. The bin
/// spans half a step (1/48 octave) either side.
pub fn spectrum_bin_centre_hz(index: usize) -> f64 {
    F_MIN_HZ * 2.0_f64.powf(index as f64 / f64::from(BINS_PER_OCTAVE))
}

/// Latest published frame plus a generation counter used by the IPC event
/// pump to avoid resending the same frame at 30 Hz.
pub struct SpectrumShared {
    latest: Mutex<Option<Spectrum>>,
    generation: AtomicU64,
}

impl Default for SpectrumShared {
    fn default() -> Self {
        Self::new()
    }
}

impl SpectrumShared {
    pub fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    fn publish(&self, spectrum: Spectrum) {
        if let Ok(mut slot) = self.latest.lock() {
            *slot = Some(spectrum);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn latest(&self) -> Option<Spectrum> {
        self.latest.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// One queued sample plus the rate/epoch it was produced at. Tagging each
/// sample keeps a rate change atomic with respect to the queued data.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TapSample {
    value: f32,
    rate: u32,
    epoch: u32,
}

/// RT-owned epoch/rate for one tap. Starts at epoch 0 so the first pushed
/// sample always bumps it.
#[derive(Clone, Copy, Debug, Default)]
struct TapEpoch {
    rate: u32,
    epoch: u32,
    /// The last push dropped samples: the next one starts a new epoch.
    gap: bool,
}

impl TapEpoch {
    #[inline]
    fn tag(&mut self, rate: u32) -> Self {
        let rate = rate.max(8_000);
        if self.rate != rate || self.epoch == 0 || self.gap {
            self.rate = rate;
            self.gap = false;
            self.epoch = self.epoch.wrapping_add(1);
            // Zero is the analyser's "no epoch seen yet" sentinel.
            if self.epoch == 0 {
                self.epoch = 1;
            }
        }
        *self
    }
}

/// Push the whole frames of `samples` that fit, tagged with `epoch`'s tag
/// for `rate`. When some do not fit (the ring is full, or the analyser is
/// behind), the rest is dropped and the next push starts a new epoch.
/// Wait-free: one `slots()` read, then pushes that cannot fail.
#[inline]
fn push_frames<T: Copy>(
    ring: &mut rtrb::Producer<TapSample>,
    epoch: &mut TapEpoch,
    rate: u32,
    channels: usize,
    samples: &[T],
    value: impl Fn(T) -> f32,
) {
    let tag = epoch.tag(rate);
    let room = ring.slots() / channels * channels;
    let whole = samples.len() / channels * channels;
    let n = whole.min(room);
    if n < samples.len() {
        epoch.gap = true;
    }
    for &sample in &samples[..n] {
        let _ = ring.push(TapSample {
            value: value(sample),
            rate: tag.rate,
            epoch: tag.epoch,
        });
    }
}

/// Largest post-tap fold: the mean of 16 frames.
const MAX_FOLD_SHIFT: u32 = 4;

/// The post tap's pre-decimation for periods larger than half its ring
/// (x64 at a 2048, 4096 or 8192 quantum): each queued frame is the mean of
/// 2^`shift` output frames, tagged at the output rate / 2^`shift`, so a
/// period still fits whole and the analyser sees contiguous audio. The mean
/// is a mild low-pass (-0.002 dB at 20 kHz from 3.072 MHz); what it lets
/// alias into the audio band lies near the folded Nyquist, where an
/// upsampler's output holds only its stopband.
struct PostFold {
    shift: u32,
    /// Epoch the partial sums belong to.
    epoch: u32,
    /// Per-channel sums of the frames since the last queued one.
    acc: Vec<f32>,
    frames: usize,
}

impl PostFold {
    fn new(channels: usize) -> Self {
        Self {
            shift: 0,
            epoch: 0,
            acc: vec![0.0; channels],
            frames: 0,
        }
    }

    #[inline]
    fn clear(&mut self) {
        self.acc.fill(0.0);
        self.frames = 0;
    }

    /// The smallest fold under which `frames` take at most `room` frames.
    #[inline]
    fn shift_for(frames: usize, room: usize) -> u32 {
        let mut shift = 0;
        while shift < MAX_FOLD_SHIFT && frames.div_ceil(1 << shift) > room {
            shift += 1;
        }
        shift
    }
}

/// Push `samples` to the post tap through `fold` (see [`PostFold`]). Frames
/// that do not fit are dropped, with the partial sum, and the next push
/// starts a new epoch. Wait-free and allocation-free.
#[inline]
fn push_folded<T: Copy>(
    ring: &mut rtrb::Producer<TapSample>,
    epoch: &mut TapEpoch,
    fold: &mut PostFold,
    ring_frames: usize,
    rate: u32,
    samples: &[T],
    value: impl Fn(T) -> f32,
) {
    let channels = fold.acc.len();
    let frames = samples.len() / channels;
    let shift = PostFold::shift_for(frames, ring_frames / 2);
    let tag = epoch.tag((rate.max(8_000) >> shift).max(8_000));
    if shift != fold.shift || tag.epoch != fold.epoch {
        fold.shift = shift;
        fold.epoch = tag.epoch;
        fold.clear();
    }
    if !samples.len().is_multiple_of(channels) {
        epoch.gap = true;
    }
    let per = 1usize << shift;
    let scale = 1.0 / per as f32;
    let mut room = ring.slots() / channels;
    for frame in samples.chunks_exact(channels) {
        for (a, &x) in fold.acc.iter_mut().zip(frame) {
            *a += value(x);
        }
        fold.frames += 1;
        if fold.frames < per {
            continue;
        }
        if room == 0 {
            epoch.gap = true;
            fold.clear();
            return;
        }
        room -= 1;
        for a in fold.acc.iter_mut() {
            let _ = ring.push(TapSample {
                value: *a * scale,
                rate: tag.rate,
                epoch: tag.epoch,
            });
            *a = 0.0;
        }
        fold.frames = 0;
    }
}

/// RT half: producers for the two taps and the shared control words.
pub struct SpectrumRt {
    pre: rtrb::Producer<TapSample>,
    post: rtrb::Producer<TapSample>,
    enabled: Arc<AtomicBool>,
    channels: usize,
    pre_epoch: TapEpoch,
    post_epoch: TapEpoch,
    post_fold: PostFold,
    post_ring_frames: usize,
}

impl SpectrumRt {
    /// Create the rings and start the analysis thread.
    pub fn create(channels: usize) -> std::io::Result<(Self, SpectrumWorker)> {
        let channels = channels.max(1);
        let (pre_tx, pre_rx) = rtrb::RingBuffer::<TapSample>::new(RING_FRAMES * channels);
        let (post_tx, post_rx) = rtrb::RingBuffer::<TapSample>::new(POST_RING_FRAMES * channels);
        let enabled = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(SpectrumShared::new());

        let analyzer_enabled = Arc::clone(&enabled);
        let analyzer_shared = Arc::clone(&shared);
        let analyzer_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("audivane-spectrum".into())
            .spawn(move || {
                run_analyzer(
                    channels,
                    pre_rx,
                    post_rx,
                    analyzer_enabled,
                    analyzer_shared,
                    analyzer_stop,
                );
            })?;

        let rt = Self::from_rings(channels, pre_tx, post_tx, Arc::clone(&enabled));
        Ok((
            rt,
            SpectrumWorker {
                enabled,
                shared,
                stop,
                handle: Some(handle),
            },
        ))
    }

    fn from_rings(
        channels: usize,
        pre: rtrb::Producer<TapSample>,
        post: rtrb::Producer<TapSample>,
        enabled: Arc<AtomicBool>,
    ) -> Self {
        let channels = channels.max(1);
        let post_ring_frames = post.buffer().capacity() / channels;
        Self {
            pre,
            post,
            enabled,
            channels,
            pre_epoch: TapEpoch::default(),
            post_epoch: TapEpoch::default(),
            post_fold: PostFold::new(channels),
            post_ring_frames,
        }
    }

    #[inline]
    fn on(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Whether a client is currently subscribed (used to skip the post tap
    /// copy entirely).
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.on()
    }

    /// Copy chain input samples (interleaved `f32`, graph rate). Drops
    /// whole frames when the ring is full. Never allocates.
    #[inline]
    pub fn push_pre(&mut self, samples: &[f32], graph_rate: u32) {
        if !self.on() {
            // Keep the tag current even while disabled, so a client that
            // subscribes after a rate change starts on a fresh epoch.
            self.pre_epoch.tag(graph_rate);
            return;
        }
        push_frames(
            &mut self.pre,
            &mut self.pre_epoch,
            graph_rate,
            self.channels,
            samples,
            |v| v,
        );
    }

    /// Copy the floating-point chain output at the output rate, immediately
    /// before quantization/dither, folded when a period is larger than half
    /// the ring (`PostFold`). Drops when the ring is full. Never allocates.
    #[inline]
    pub fn push_post_f32(&mut self, samples: &[f32], output_rate: u32) {
        if !self.on() {
            self.post_epoch.tag(output_rate);
            return;
        }
        push_folded(
            &mut self.post,
            &mut self.post_epoch,
            &mut self.post_fold,
            self.post_ring_frames,
            output_rate,
            samples,
            |v| v,
        );
    }

    /// Copy quantized output (interleaved left-justified `i32`) as `f32`.
    /// Used only on paths with no float signal (Direct / bypass). Never
    /// allocates.
    #[inline]
    pub fn push_post_i32(&mut self, samples: &[i32], output_rate: u32) {
        if !self.on() {
            self.post_epoch.tag(output_rate);
            return;
        }
        const SCALE: f32 = 1.0 / 2_147_483_648.0;
        push_folded(
            &mut self.post,
            &mut self.post_epoch,
            &mut self.post_fold,
            self.post_ring_frames,
            output_rate,
            samples,
            |s| s as f32 * SCALE,
        );
    }
}

/// Non-RT analysis worker. Owned by [`crate::engine::RealEngine`].
pub struct SpectrumWorker {
    enabled: Arc<AtomicBool>,
    shared: Arc<SpectrumShared>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SpectrumWorker {
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn latest(&self) -> Option<Spectrum> {
        self.shared.latest()
    }

    pub fn generation(&self) -> u64 {
        self.shared.generation()
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SpectrumWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_analyzer(
    channels: usize,
    mut pre_rx: rtrb::Consumer<TapSample>,
    mut post_rx: rtrb::Consumer<TapSample>,
    enabled: Arc<AtomicBool>,
    shared: Arc<SpectrumShared>,
    stop: Arc<AtomicBool>,
) {
    let mut analyzer = Analyzer::new(channels);
    while !stop.load(Ordering::Acquire) {
        if !enabled.load(Ordering::Acquire) {
            drain(&mut pre_rx);
            drain(&mut post_rx);
            analyzer.reset();
            thread::sleep(TICK);
            continue;
        }
        if let Some(spectrum) = analyzer.step(&mut pre_rx, &mut post_rx, Instant::now()) {
            shared.publish(spectrum);
        }
        thread::sleep(TICK);
    }
}

/// How long the pre tap's reports wait for a post one before going out
/// without it (`post` empty): a few report periods (about 90 ms each).
const POST_WAIT: Duration = Duration::from_millis(400);

/// The analysis thread's state for both taps, one [`Analyzer::step`] per
/// tick.
struct Analyzer {
    channels: usize,
    pre: TapAnalyzer,
    post: TapAnalyzer,
    scratch: Vec<TapSample>,
    /// Since when a pre report has been waiting for a post one.
    post_wait: Option<Instant>,
}

impl Analyzer {
    fn new(channels: usize) -> Self {
        Self {
            channels,
            pre: TapAnalyzer::new(channels),
            post: TapAnalyzer::new(channels),
            scratch: Vec::with_capacity(READ_FRAMES * channels),
            post_wait: None,
        }
    }

    fn reset(&mut self) {
        self.pre.reset();
        self.post.reset();
        self.post_wait = None;
    }

    /// Read both rings and return a report when the pre tap has one. It
    /// carries the post tap's report too; when the post tap has none for
    /// [`POST_WAIT`] (it cannot complete an FFT, e.g. its periods are cut
    /// by gaps), the pre report goes out with `post` empty rather than
    /// freezing the whole spectrum.
    fn step(
        &mut self,
        pre_rx: &mut rtrb::Consumer<TapSample>,
        post_rx: &mut rtrb::Consumer<TapSample>,
        now: Instant,
    ) -> Option<Spectrum> {
        read_available(pre_rx, &mut self.scratch, self.channels);
        self.pre.feed(&self.scratch);
        read_available(post_rx, &mut self.scratch, self.channels);
        self.post.feed(&self.scratch);
        if self.pre.frames() == 0 {
            return None;
        }
        let post = if self.post.frames() > 0 {
            self.post_wait = None;
            self.post.finish()
        } else {
            let since = *self.post_wait.get_or_insert(now);
            if now.duration_since(since) < POST_WAIT {
                return None;
            }
            Vec::new()
        };
        Some(Spectrum {
            f_min_hz: F_MIN_HZ,
            bins_per_octave: BINS_PER_OCTAVE,
            rate: self.pre.rate(),
            pre: self.pre.finish(),
            post,
        })
    }
}

/// Empty a ring a chunk at a time (the analyser is off).
fn drain(rx: &mut rtrb::Consumer<TapSample>) {
    loop {
        let n = rx.slots();
        if n == 0 {
            return;
        }
        if let Ok(chunk) = rx.read_chunk(n) {
            chunk.commit_all();
        }
    }
}

fn read_available(
    rx: &mut rtrb::Consumer<TapSample>,
    scratch: &mut Vec<TapSample>,
    channels: usize,
) {
    // Whole frames only: the RT side commits one sample at a time, so the
    // rest of a frame it is still writing stays queued for the next read.
    let channels = channels.max(1);
    let n = (rx.slots().min(scratch.capacity()) / channels) * channels;
    scratch.clear();
    while scratch.len() < n {
        match rx.pop() {
            Ok(sample) => scratch.push(sample),
            Err(_) => break,
        }
    }
}

/// Integral of the unit hat function `max(0, 1 - |x|)` from -inf to `x`.
fn hat_cdf(x: f64) -> f64 {
    if x <= -1.0 {
        0.0
    } else if x < 0.0 {
        0.5 * (x + 1.0) * (x + 1.0)
    } else if x < 1.0 {
        1.0 - 0.5 * (1.0 - x) * (1.0 - x)
    } else {
        1.0
    }
}

/// How one log bin is read from one FFT resolution.
#[derive(Clone, Debug)]
pub(crate) struct LogBand {
    /// Index into the published log grid.
    pub(crate) index: usize,
    /// `(fft bin, weight)`: the integral over the band of the hat function
    /// centred on that FFT bin, in FFT-bin units. Summing `power * weight`
    /// integrates the linearly interpolated power density over the band, so
    /// bands narrower than one FFT bin still vary smoothly with frequency
    /// instead of repeating the same bin.
    pub(crate) weights: Vec<(usize, f64)>,
}

/// Log bin `index` spanning `lo_hz..hi_hz`, read from an FFT of `bin_hz`
/// per bin whose last bin is `max_bin`.
pub(crate) fn log_band(
    index: usize,
    lo_hz: f64,
    hi_hz: f64,
    bin_hz: f64,
    max_bin: usize,
) -> LogBand {
    let xa = lo_hz / bin_hz;
    let xb = (hi_hz / bin_hz).max(xa);
    let first = (xa.floor() as isize - 1).max(0) as usize;
    let last = ((xb.ceil() as usize) + 1).min(max_bin);
    let weights = (first..=last)
        .filter_map(|k| {
            let w = hat_cdf(xb - k as f64) - hat_cdf(xa - k as f64);
            (w > 0.0).then_some((k, w))
        })
        .collect();
    LogBand { index, weights }
}

/// The log bins `indices` for one FFT size at `rate`. `max_width_hz` keeps
/// only the bands narrower than that (used to give the long FFT only the
/// low band); `None` serves every one.
fn log_bands(
    rate: u32,
    size: usize,
    indices: std::ops::Range<usize>,
    max_width_hz: Option<f64>,
) -> Vec<LogBand> {
    let nyquist = f64::from(rate) / 2.0;
    let bin_hz = f64::from(rate) / size as f64;
    let max_bin = size / 2;
    let half = 2.0_f64.powf(0.5 / f64::from(BINS_PER_OCTAVE));
    let mut out = Vec::new();
    for index in indices {
        let centre = spectrum_bin_centre_hz(index);
        let lo_hz = centre / half;
        let hi_hz = (centre * half).min(nyquist);
        if let Some(max_width_hz) = max_width_hz
            && hi_hz - lo_hz >= max_width_hz
        {
            continue;
        }
        out.push(log_band(index, lo_hz, hi_hz, bin_hz, max_bin));
    }
    out
}

/// One FFT size for a tap: window, plan, last power spectrum and the
/// averaged values of the log bins it serves.
struct Resolution {
    size: usize,
    window: Vec<f64>,
    inv_norm: f64,
    windowed: Vec<f64>,
    fft: RealFftPair,
    spec: Vec<Complex64>,
    /// Channel-mean power per FFT bin, normalised so a full-scale sine's
    /// peak bin is 1.0.
    power: Vec<f64>,
    bands: Vec<LogBand>,
    /// Log-grid index -> entry of `bands`.
    slot_of: Vec<Option<usize>>,
    /// The FFT bins whose peaks can land in `bands` (the tonal search).
    peak_bins: std::ops::Range<usize>,
    /// Per entry of `bands`: tonal reading of the current frame.
    tonal: Vec<f64>,
    /// Summed linear power per entry of `bands` since the last report.
    accum: Vec<f64>,
    frames: u64,
    rate: u32,
}

impl Resolution {
    fn new(size: usize) -> Self {
        let window: Vec<f64> = (0..size)
            .map(|n| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / size as f64).cos())
            .collect();
        let window_sum: f64 = window.iter().sum();
        let fft = RealFftPair::new(size);
        let spec = vec![Complex64::new(0.0, 0.0); fft.spec_len()];
        Self {
            size,
            window,
            inv_norm: 1.0 / (window_sum / 2.0).powi(2),
            windowed: vec![0.0; size],
            fft,
            spec,
            power: vec![0.0; size / 2 + 1],
            bands: Vec::new(),
            slot_of: Vec::new(),
            peak_bins: 0..0,
            tonal: Vec::new(),
            accum: Vec::new(),
            frames: 0,
            rate: 1,
        }
    }

    fn set_bands(&mut self, rate: u32, bins: usize, bands: Vec<LogBand>) {
        self.rate = rate;
        self.accum = vec![0.0; bands.len()];
        self.tonal = vec![0.0; bands.len()];
        self.slot_of = vec![None; bins];
        for (slot, band) in bands.iter().enumerate() {
            self.slot_of[band.index] = Some(slot);
        }
        let bins_of = || bands.iter().flat_map(|b| b.weights.iter().map(|&(k, _)| k));
        self.peak_bins = match (bins_of().min(), bins_of().max()) {
            (Some(lo), Some(hi)) => lo.saturating_sub(1)..(hi + 2).min(self.power.len()),
            _ => 0..0,
        };
        self.bands = bands;
        self.frames = 0;
    }

    fn reset(&mut self) {
        self.accum.fill(0.0);
        self.frames = 0;
    }

    /// Analyse the last `size` frames of the interleaved buffer `frames`.
    fn analyze(&mut self, frames: &[f32], channels: usize) {
        let size = self.size;
        let start = frames.len() / channels - size;
        self.power.fill(0.0);
        for channel in 0..channels {
            for n in 0..size {
                self.windowed[n] =
                    f64::from(frames[(start + n) * channels + channel]) * self.window[n];
            }
            self.fft
                .forward_padded_complex(&self.windowed, &mut self.spec);
            for (dst, c) in self.power.iter_mut().zip(self.spec.iter()) {
                *dst += (c.re * c.re + c.im * c.im) * self.inv_norm;
            }
        }
        let inv_channels = 1.0 / channels as f64;
        for power in &mut self.power {
            *power *= inv_channels;
        }
        self.find_tonal_peaks();
        for (slot, band) in self.bands.iter().enumerate() {
            let value = band_power(&self.power, band).max(self.tonal[slot]);
            self.accum[slot] += value;
        }
        self.frames += 1;
    }

    /// Tonal readings: a narrow band only holds part of a sine's main lobe, so
    /// for every genuine coherent tone peak in the power spectrum its
    /// parabolically interpolated peak (the sine's true level) goes to the log
    /// bin holding the interpolated peak frequency, and that bin reads the
    /// higher of its band power and this peak. Random local maxima in broadband
    /// noise are rejected by requiring the peak to stand well above the noise
    /// floor outside the Hann main lobe (`±3..=±8` bins).
    fn find_tonal_peaks(&mut self) {
        let bin_hz = f64::from(self.rate) / self.size as f64;
        tonal_peaks_in(
            &self.power,
            self.peak_bins.clone(),
            bin_hz,
            &self.slot_of,
            &mut self.tonal,
        );
    }

    /// Write the averaged dB values of this resolution's bands into `out`.
    fn write(&self, out: &mut [f32]) {
        let frames = self.frames.max(1) as f64;
        for (band, &total) in self.bands.iter().zip(&self.accum) {
            if let Some(dst) = out.get_mut(band.index) {
                *dst = to_db(total / frames);
            }
        }
    }
}

/// The tonal readings of [`Resolution::find_tonal_peaks`] from `power`
/// (`bin_hz` per bin) into `tonal`, one per band (`slot_of` maps a log-grid
/// index to its band).
pub(crate) fn tonal_peaks(
    power: &[f64],
    bin_hz: f64,
    slot_of: &[Option<usize>],
    tonal: &mut [f64],
) {
    tonal_peaks_in(power, 0..power.len(), bin_hz, slot_of, tonal);
}

/// [`tonal_peaks`] for the peaks at the FFT bins `bins` only (those that
/// can land in a band; the rest would be looked up and dropped).
fn tonal_peaks_in(
    power: &[f64],
    bins: std::ops::Range<usize>,
    bin_hz: f64,
    slot_of: &[Option<usize>],
    tonal: &mut [f64],
) {
    tonal.fill(0.0);
    let steps = f64::from(BINS_PER_OCTAVE);
    for k in bins.start.max(1)..bins.end.min(power.len().saturating_sub(1)) {
        let Some((delta, level)) = interpolated_peak(power, k) else {
            continue;
        };
        if !is_coherent_tone(power, k, level) {
            continue;
        }
        let freq = (k as f64 + delta) * bin_hz;
        if freq <= 0.0 {
            continue;
        }
        let index = (steps * (freq / F_MIN_HZ).log2()).round();
        if index < 0.0 || index >= slot_of.len() as f64 {
            continue;
        }
        if let Some(slot) = slot_of[index as usize]
            && level > tonal[slot]
        {
            tonal[slot] = level;
        }
    }
}

/// Minimum ratio between an interpolated spectral peak and the median power in
/// the surrounding wing bins (`±3..=±8`, outside the Hann main lobe) to treat
/// the peak as a coherent sinusoid rather than a random noise maximum.
/// (+12 dB; for a pure Hann-windowed sine the ratio exceeds +30 dB.)
const TONAL_PROMINENCE_RATIO: f64 = 16.0;

fn is_coherent_tone(power: &[f64], peak: usize, level: f64) -> bool {
    let mut wings = [0.0_f64; 12];
    let mut n = 0usize;
    if peak >= 4 {
        let lo = peak.saturating_sub(8).max(1);
        let hi = peak - 3;
        for &p in &power[lo..=hi] {
            wings[n] = p;
            n += 1;
        }
    }
    if peak + 3 < power.len() {
        let lo = peak + 3;
        let hi = (peak + 8).min(power.len() - 1);
        for &p in &power[lo..=hi] {
            wings[n] = p;
            n += 1;
        }
    }
    if n == 0 {
        return false;
    }
    wings[..n].sort_by(f64::total_cmp);
    let median_bg = if n.is_multiple_of(2) {
        0.5 * (wings[n / 2 - 1] + wings[n / 2])
    } else {
        wings[n / 2]
    };
    level >= TONAL_PROMINENCE_RATIO * median_bg
}

/// Band power of one log band from a power spectrum (0 dB = full-scale sine):
/// the linearly interpolated power density integrated over the band, divided by
/// the Hann equivalent noise bandwidth. That is the signal power inside the
/// band (a full-scale sine whose main lobe fits reads 0 dB, broadband noise
/// reads its power there), so it does not depend on the FFT size and the short
/// and long resolutions agree. Linear interpolation never exceeds the larger of
/// its two neighbours, so it cannot invent a peak.
pub(crate) fn band_power(power: &[f64], band: &LogBand) -> f64 {
    band.weights.iter().map(|&(k, w)| power[k] * w).sum::<f64>() / HANN_ENBW
}

/// Parabolic (log-power) interpolation around `peak`, but only when `peak` is a
/// genuine local maximum of the FFT power spectrum: the fitted vertex has to
/// stay inside ±0.5 FFT bins and its amplitude has to be plausible next to the
/// neighbours. A bin on the shoulder of a peak living outside the log bin is
/// not a local maximum; fitting it would invent a level far above the real
/// signal. Returns the vertex offset in bins and its power.
fn interpolated_peak(power: &[f64], peak: usize) -> Option<(f64, f64)> {
    if peak == 0 || peak + 1 >= power.len() {
        return None;
    }
    let p0 = power[peak - 1];
    let p1 = power[peak];
    let p2 = power[peak + 1];
    // A genuine local maximum, including the two equal central bins of a
    // tone exactly halfway between bins.
    let local_max = p1 >= p0 && p1 >= p2 && (p1 > p0 || p1 > p2);
    if !local_max {
        return None;
    }
    let p0 = p0.max(1.0e-30);
    let p1 = p1.max(1.0e-30);
    let p2 = p2.max(1.0e-30);
    let (l0, l1, l2) = (p0.ln(), p1.ln(), p2.ln());
    let den = l0 - 2.0 * l1 + l2;
    if den >= -1.0e-12 {
        return None;
    }
    let delta = 0.5 * (l0 - l2) / den;
    if !(-0.5..=0.5).contains(&delta) {
        return None;
    }
    let power = (l1 - 0.25 * (l0 - l2) * delta).exp();
    // A valid Hann main-lobe vertex is close to the sampled peak. Reject
    // wild extrapolation (e.g. from a nearly flat three-point fit).
    if !power.is_finite() || power > 4.0 * p1 {
        return None;
    }
    Some((delta, power))
}

fn to_db(power: f64) -> f32 {
    let db = if power > 1.0e-30 {
        (10.0 * power.log10()).max(DB_FLOOR)
    } else {
        DB_FLOOR
    };
    ((db * 10.0).round() / 10.0) as f32
}

/// Number of `/2` half-band decimation stages before the FFTs so the analysis
/// rate always lands in the 44.1/48 kHz family while preserving 10 Hz..20 kHz.
fn decimation_stages(rate: u32) -> usize {
    let mut r = rate;
    let mut stages = 0;
    while r >= 88_200 && r.is_multiple_of(2) {
        r /= 2;
        stages += 1;
    }
    stages
}

/// Number of `/2` stages before the high path's FFT: down to 352.8/384 kHz
/// at most, where [`F_MAX_HZ`] still lies well inside the last stage's
/// alias-free band. Never more than [`decimation_stages`].
fn high_stages(rate: u32) -> usize {
    let mut r = rate;
    let mut stages = 0;
    while stages < decimation_stages(rate) && f64::from(r) / 4.0 >= 1.1 * F_MAX_HZ {
        r /= 2;
        stages += 1;
    }
    stages
}

/// The high path's FFT size at `rate`: bins at most [`HIGH_BIN_HZ`] wide.
fn high_fft_size(rate: u32) -> usize {
    ((f64::from(rate) / HIGH_BIN_HZ).ceil() as usize)
        .next_power_of_two()
        .max(256)
}

/// Half-band length (`k`, see [`HalfBandStage`]) for a stage from `rate_in`
/// whose passband reaches `pass_hz`: the Kaiser estimate for about 105 dB
/// over the transition `pass_hz .. rate_in/2 - pass_hz`, with a 20% margin.
/// Everything in the transition folds above `pass_hz`.
fn half_band_k(rate_in: f64, pass_hz: f64) -> usize {
    let transition = (rate_in / 2.0 - 2.0 * pass_hz).max(rate_in * 0.01);
    let dw = 2.0 * std::f64::consts::PI * transition / rate_in;
    let taps = 1.2 * (105.0 - 8.0) / (2.285 * dw);
    (((taps + 1.0) / 4.0).ceil() as usize).max(5)
}

#[derive(Clone)]
struct HalfBandChState {
    have_even: bool,
    pending_even: f64,
    u_hist: Vec<f64>,
    u_pos: usize,
    v_delay: Vec<f64>,
    v_pos: usize,
}

impl HalfBandChState {
    fn new(k: usize) -> Self {
        Self {
            have_even: false,
            pending_even: 0.0,
            u_hist: vec![0.0; 4 * k],
            u_pos: 0,
            v_delay: vec![0.0; k],
            v_pos: 0,
        }
    }

    fn reset(&mut self) {
        self.have_even = false;
        self.pending_even = 0.0;
        self.u_hist.fill(0.0);
        self.u_pos = 0;
        self.v_delay.fill(0.0);
        self.v_pos = 0;
    }
}

/// Polyphase symmetric half-band FIR decimator-by-2 (`N = 4*k - 1` taps,
/// only `k + 1` multiplies per output sample).
pub(crate) struct HalfBandStage {
    center: f64,
    odd: Vec<f64>,
    ch: Vec<HalfBandChState>,
}

impl HalfBandStage {
    /// Build a Kaiser-windowed half-band decimator for `remaining_stages`
    /// after this stage (`0` = final `88.2k/96k -> 44.1k/48k` stage).
    pub(crate) fn new(channels: usize, remaining_stages: usize) -> Self {
        // Kaiser beta = 10.5 gives >105 dB stopband attenuation; passband is
        // flat (<0.0001 dB) up to 20 kHz at every stage.
        let k = match remaining_stages {
            0 => 40, // N = 159 taps (transition 20..24.1 kHz @ 88.2k)
            1 => 8,  // N = 31 taps (transition 20..68.2 kHz @ 176.4k)
            2 => 6,  // N = 23 taps
            _ => 5,  // N = 19 taps
        };
        Self::with_k(channels, k)
    }

    /// A decimator of `4 * k - 1` taps (Kaiser beta 10.5).
    fn with_k(channels: usize, k: usize) -> Self {
        let n_taps = 4 * k - 1;
        let center_idx = 2 * k - 1;
        let win = audivane_dsp_core::window::kaiser(n_taps, 10.5);
        let mut odd = Vec::with_capacity(k);
        for j in 0..k {
            let d = (2 * j + 1) as f64;
            let arg = 0.5 * std::f64::consts::PI * d;
            odd.push(win[center_idx + 2 * j + 1] * 0.5 * arg.sin() / arg);
        }
        let sum = 0.5 + 2.0 * odd.iter().sum::<f64>();
        let inv_sum = 1.0 / sum;
        let center = 0.5 * inv_sum;
        for tap in &mut odd {
            *tap *= inv_sum;
        }
        Self {
            center,
            odd,
            ch: (0..channels).map(|_| HalfBandChState::new(k)).collect(),
        }
    }

    #[inline]
    pub(crate) fn push(&mut self, channel: usize, sample: f64) -> Option<f64> {
        let st = &mut self.ch[channel];
        if !st.have_even {
            st.pending_even = sample;
            st.have_even = true;
            return None;
        }
        let u_in = st.pending_even;
        let v_in = sample;
        st.have_even = false;

        let k = self.odd.len();
        let len2 = 2 * k;
        let pos = st.u_pos;
        st.u_hist[pos] = u_in;
        st.u_hist[pos + len2] = u_in;
        let win = &st.u_hist[pos + 1..pos + 1 + len2];

        let mut acc = st.v_delay[st.v_pos] * self.center;
        for j in 0..k {
            acc += (win[k - 1 - j] + win[k + j]) * self.odd[j];
        }
        st.v_delay[st.v_pos] = v_in;
        st.v_pos = if st.v_pos + 1 == k { 0 } else { st.v_pos + 1 };
        st.u_pos = if pos + 1 == len2 { 0 } else { pos + 1 };
        Some(acc)
    }

    pub(crate) fn reset(&mut self) {
        for ch in &mut self.ch {
            ch.reset();
        }
    }
}

struct CascadedDecimator {
    stages: Vec<HalfBandStage>,
}

impl CascadedDecimator {
    fn new() -> Self {
        Self { stages: Vec::new() }
    }

    fn configure(&mut self, stages: Vec<HalfBandStage>) {
        self.stages = stages;
    }

    fn reset(&mut self) {
        for stage in &mut self.stages {
            stage.reset();
        }
    }

    #[inline]
    fn is_identity(&self) -> bool {
        self.stages.is_empty()
    }

    #[inline]
    fn push_frame(&mut self, frame: &[f32], out: &mut [f32]) -> bool {
        let mut ready = true;
        for (c, &sample) in frame.iter().enumerate() {
            let mut val = f64::from(sample);
            for stage in &mut self.stages {
                match stage.push(c, val) {
                    Some(y) => val = y,
                    None => {
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                out[c] = val as f32;
            }
        }
        ready
    }
}

/// The bins above 20 kHz of a tap whose base analysis is decimated: a Hann
/// FFT of [`high_fft_size`] (no overlap: about 47 a second, each log bin up
/// there spans a dozen FFT bins or more) at the tap's rate, or at 352.8/384
/// kHz above that (its own half-band stages, passband to [`F_MAX_HZ`]). Its
/// decimated frames also feed the base path's remaining stages, so the
/// stages above 384 kHz run once for both.
struct HighPath {
    decimator: CascadedDecimator,
    frame: Vec<f32>,
    /// Interleaved rolling buffer of `res.size` frames.
    buf: Vec<f32>,
    fill: usize,
    res: Resolution,
}

impl HighPath {
    fn new(channels: usize, rate: u32, bins: usize) -> Self {
        let stages = high_stages(rate);
        let mut decimator = CascadedDecimator::new();
        decimator.configure(
            (0..stages)
                .map(|s| {
                    HalfBandStage::with_k(channels, half_band_k(f64::from(rate >> s), F_MAX_HZ))
                })
                .collect(),
        );
        let high_rate = rate >> stages;
        let size = high_fft_size(high_rate);
        let mut res = Resolution::new(size);
        res.set_bands(
            high_rate,
            bins,
            log_bands(high_rate, size, bins_up_to(BASE_MAX_HZ)..bins, None),
        );
        Self {
            decimator,
            frame: vec![0.0; channels],
            buf: vec![0.0; size * channels],
            fill: 0,
            res,
        }
    }

    fn reset(&mut self) {
        self.decimator.reset();
        self.fill = 0;
        self.res.reset();
    }

    /// Take one input frame; true with the decimated frame in `self.frame`
    /// when one is ready.
    fn push(&mut self, input: &[f32], channels: usize) -> bool {
        if self.decimator.is_identity() {
            self.frame.copy_from_slice(input);
        } else if !self.decimator.push_frame(input, &mut self.frame) {
            return false;
        }
        let base = self.fill * channels;
        self.buf[base..base + channels].copy_from_slice(&self.frame);
        self.fill += 1;
        if self.fill == self.res.size {
            self.res.analyze(&self.buf, channels);
            self.fill = 0;
        }
        true
    }
}

/// One tap's rolling frames, the FFT resolutions and averaged log bins.
///
/// Rates at or above 88.2 kHz are decimated on the analysis thread by a
/// cascaded half-band anti-alias filter to the 44.1/48 kHz family, so
/// resolution and bass calibration stay uniform up to 768 kHz; the bins above
/// 20 kHz then come from a [`HighPath`]. Every [`HOP`] decimated frames the
/// short FFT ([`FFT_SIZE`]) analyses the newest frames for every other log
/// bin; once [`LONG_FFT_SIZE`] are buffered the long FFT also analyses the
/// low band (log bins narrower than [`CROSSOVER_BINS`] short-FFT bins, below
/// roughly 800 Hz at 48 kHz).
pub(crate) struct TapAnalyzer {
    channels: usize,
    rate: u32,
    analysis_rate: u32,
    epoch: u32,
    bins: usize,
    decimator: CascadedDecimator,
    in_frame: Vec<f32>,
    decim_frame: Vec<f32>,
    /// Interleaved `LONG_FFT_SIZE`-frame rolling buffer (at `analysis_rate`).
    buf: Vec<f32>,
    fill: usize,
    short: Resolution,
    long: Resolution,
    high: Option<HighPath>,
}

impl TapAnalyzer {
    pub(crate) fn new(channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            channels,
            rate: 0,
            analysis_rate: 0,
            epoch: 0,
            bins: 0,
            decimator: CascadedDecimator::new(),
            in_frame: vec![0.0; channels],
            decim_frame: vec![0.0; channels],
            buf: vec![0.0; LONG_FFT_SIZE * channels],
            fill: 0,
            short: Resolution::new(FFT_SIZE),
            long: Resolution::new(LONG_FFT_SIZE),
            high: None,
        }
    }

    pub(crate) fn set_rate(&mut self, rate: u32) {
        self.rate = rate.max(1);
        let stages = decimation_stages(self.rate);
        let high = high_stages(self.rate);
        self.analysis_rate = (self.rate >> stages).max(1);
        // The base stages after the high path's (the same filters as a
        // cascade from the tap's rate would end with).
        let base_stages = stages - high;
        self.decimator.configure(
            (0..base_stages)
                .map(|s| HalfBandStage::new(self.channels, base_stages - 1 - s))
                .collect(),
        );
        self.bins = spectrum_bin_count(self.rate);
        let base_bins = if stages == 0 {
            self.bins
        } else {
            bins_up_to(BASE_MAX_HZ).min(self.bins)
        };
        self.high = (stages > 0 && base_bins < self.bins)
            .then(|| HighPath::new(self.channels, self.rate, self.bins));
        let short_bin_hz = f64::from(self.analysis_rate) / FFT_SIZE as f64;
        let (analysis_rate, bins) = (self.analysis_rate, self.bins);
        self.short.set_bands(
            analysis_rate,
            bins,
            log_bands(analysis_rate, FFT_SIZE, 0..base_bins, None),
        );
        self.long.set_bands(
            analysis_rate,
            bins,
            log_bands(
                analysis_rate,
                LONG_FFT_SIZE,
                0..base_bins,
                Some(CROSSOVER_BINS * short_bin_hz),
            ),
        );
        self.reset();
    }

    pub(crate) fn reset(&mut self) {
        self.fill = 0;
        self.decimator.reset();
        self.short.reset();
        self.long.reset();
        if let Some(high) = &mut self.high {
            high.reset();
        }
    }

    pub(crate) fn frames(&self) -> u64 {
        self.short.frames
    }

    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    pub(crate) fn feed(&mut self, samples: &[TapSample]) {
        let channels = self.channels;
        for frame in samples.chunks_exact(channels) {
            let first = frame[0];
            // A rate change starts a new epoch: everything already buffered
            // was produced at the old rate and must not be analysed at the
            // new one.
            if first.epoch != self.epoch {
                self.epoch = first.epoch;
                self.set_rate(first.rate);
            }
            if self.bins == 0 {
                continue;
            }
            for (dst, sample) in self.in_frame.iter_mut().zip(frame) {
                *dst = sample.value;
            }
            let input = match &mut self.high {
                Some(high) => {
                    if !high.push(&self.in_frame, channels) {
                        continue;
                    }
                    &high.frame
                }
                None => &self.in_frame,
            };
            let base = self.fill * channels;
            if self.decimator.is_identity() {
                self.buf[base..base + channels].copy_from_slice(input);
            } else if self.decimator.push_frame(input, &mut self.decim_frame) {
                self.buf[base..base + channels].copy_from_slice(&self.decim_frame);
            } else {
                continue;
            }
            self.fill += 1;
            if self.fill >= FFT_SIZE && (self.fill - FFT_SIZE).is_multiple_of(HOP) {
                let filled = &self.buf[..self.fill * channels];
                self.short.analyze(filled, channels);
                if self.fill == LONG_FFT_SIZE {
                    self.long.analyze(filled, channels);
                    self.buf.copy_within(HOP * channels.., 0);
                    self.fill -= HOP;
                }
            }
        }
    }

    /// Mean level per log bin since the last call, in dB (0 dB = full-scale
    /// sine), rounded to 0.1 dB, then reset.
    pub(crate) fn finish(&mut self) -> Vec<f32> {
        let mut out = vec![DB_FLOOR as f32; self.bins];
        self.short.write(&mut out);
        if self.long.frames > 0 {
            self.long.write(&mut out);
        }
        if let Some(high) = &mut self.high {
            if high.res.frames > 0 {
                high.res.write(&mut out);
            }
            high.res.reset();
        }
        self.short.reset();
        self.long.reset();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, rate: u32, freq: f64, amplitude: f64) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let sample = (amplitude
                    * (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(rate)).sin())
                    as f32;
                [sample, sample]
            })
            .collect()
    }

    fn tagged(samples: Vec<f32>, rate: u32, epoch: u32) -> Vec<TapSample> {
        samples
            .into_iter()
            .map(|value| TapSample { value, rate, epoch })
            .collect()
    }

    fn peak_bin(bins: &[f32]) -> (usize, f32) {
        bins.iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, &db)| (i, db))
            .unwrap()
    }

    /// Deterministic white noise with the requested RMS.
    fn white_noise(frames: usize, rms: f64, seed: u64) -> Vec<f32> {
        let mut state = seed;
        let mut out = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let unit = ((state >> 11) as f64) / ((1u64 << 53) as f64);
            let sample = ((unit * 2.0 - 1.0) * rms * 3.0_f64.sqrt()) as f32;
            out.push(sample);
            out.push(sample);
        }
        out
    }

    #[test]
    fn full_scale_sine_reads_zero_db_in_the_right_bin() {
        for rate in [44_100u32, 48_000, 96_000] {
            let mut tap = TapAnalyzer::new(2);
            tap.set_rate(rate);
            // A little over two frames so the 50% overlap window is filled.
            tap.feed(&tagged(
                sine(FFT_SIZE * 2 + HOP, rate, 1_000.0, 1.0),
                rate,
                1,
            ));
            assert!(tap.frames() > 0);
            let bins = tap.finish();
            assert_eq!(bins.len(), spectrum_bin_count(rate));

            let (peak_bin, peak_db) = bins
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
                .map(|(i, &db)| (i, db))
                .unwrap();
            assert!(
                (peak_db - 0.0).abs() <= 0.5,
                "rate {rate}: full-scale 1 kHz sine reads {peak_db} dB in bin {peak_bin}"
            );
            let peak_hz = spectrum_bin_centre_hz(peak_bin);
            assert!(
                (peak_hz - 1_000.0).abs() < 100.0,
                "rate {rate}: peak bin at {peak_hz} Hz"
            );
            let others = bins
                .iter()
                .enumerate()
                .filter(|(i, _)| i.abs_diff(peak_bin) > 1)
                .map(|(_, &db)| db)
                .fold(f64::NEG_INFINITY, |a, b| a.max(f64::from(b)));
            assert!(
                others < -20.0,
                "rate {rate}: energy leaked elsewhere ({others} dB)"
            );
        }
    }

    #[test]
    fn silence_and_idle_produce_a_floor_not_a_panic() {
        let mut tap = TapAnalyzer::new(2);
        tap.set_rate(44_100);
        assert_eq!(tap.frames(), 0);
        // No full frame yet: nothing to report.
        tap.feed(&tagged(vec![0.0; 128], 44_100, 1));
        assert_eq!(tap.frames(), 0);
        tap.feed(&tagged(vec![0.0; FFT_SIZE * 2], 44_100, 1));
        assert!(tap.frames() > 0);
        let bins = tap.finish();
        assert!(bins.iter().all(|&db| db <= -150.0));
    }

    #[test]
    fn white_noise_is_smooth_at_minus_twenty_dbfs_rms() {
        let rate = 48_000u32;
        let mut tap = TapAnalyzer::new(2);
        tap.set_rate(rate);
        tap.feed(&tagged(
            white_noise(FFT_SIZE * 8, 0.1, 0x5EED_1234),
            rate,
            1,
        ));
        let bins = tap.finish();

        // No log bin should tower over both neighbours. Compare the average
        // excess, which is what "smooth on average" means for a random
        // spectrum, and keep a generous single-bin bound for the sparse
        // low-frequency bins.
        let mut total_excess = 0.0_f64;
        let mut count = 0usize;
        for i in 1..bins.len() - 1 {
            let neighbours = bins[i - 1].max(bins[i + 1]);
            let excess = f64::from(bins[i] - neighbours);
            total_excess += excess;
            count += 1;
            assert!(
                excess <= 8.0,
                "bin {i} is {excess:.1} dB above both neighbours"
            );
        }
        let average = total_excess / count as f64;
        assert!(
            average <= 3.0,
            "white noise spectrum is not smooth: average excess {average:.2} dB"
        );
    }

    #[test]
    fn rate_change_discards_samples_from_the_old_epoch() {
        // A 999 Hz tone must stay at 999 Hz across a rate change, not land
        // near 2 kHz through old-rate samples analysed at the new rate.
        let mut tap = TapAnalyzer::new(2);
        let old = tagged(sine(FFT_SIZE * 3, 48_000, 999.0, 0.5), 48_000, 1);
        // Old-epoch audio is queued, then the engine changes rate. The new
        // epoch carries a 999 Hz tone at 96 kHz: it must stay at 999 Hz.
        let new = tagged(sine(FFT_SIZE * 3, 96_000, 999.0, 0.5), 96_000, 2);
        tap.feed(&old);
        tap.feed(&new);

        assert_eq!(tap.rate(), 96_000);
        let bins = tap.finish();
        let (index, _) = peak_bin(&bins);
        let peak_hz = spectrum_bin_centre_hz(index);
        assert!(
            (peak_hz - 999.0).abs() < 120.0,
            "999 Hz at 96 kHz landed at {peak_hz} Hz (bin {index})"
        );

        // The old epoch is discarded entirely: a frame made only of it
        // produces no signal after the switch.
        let mut silent = TapAnalyzer::new(2);
        silent.feed(&old);
        silent.feed(&tagged(vec![0.0; FFT_SIZE * 2], 96_000, 2));
        let quiet = silent.finish();
        assert!(
            quiet.iter().all(|&db| db <= -150.0),
            "old-epoch samples survived the rate change: {quiet:?}"
        );
    }

    #[test]
    fn the_analyser_reads_whole_frames_only() {
        // The RT side commits one sample at a time, so a read can land in
        // the middle of a frame; the rest of that frame must stay queued,
        // or every later frame would swap channels.
        let (mut tx, mut rx) = rtrb::RingBuffer::<TapSample>::new(16);
        for value in [1.0, 2.0, 1.0] {
            tx.push(TapSample {
                value,
                rate: 48_000,
                epoch: 1,
            })
            .unwrap();
        }
        let mut scratch = Vec::with_capacity(16);
        read_available(&mut rx, &mut scratch, 2);
        assert_eq!(scratch.len(), 2);
        tx.push(TapSample {
            value: 2.0,
            rate: 48_000,
            epoch: 1,
        })
        .unwrap();
        read_available(&mut rx, &mut scratch, 2);
        assert_eq!(
            scratch.iter().map(|s| s.value).collect::<Vec<_>>(),
            [1.0, 2.0]
        );
    }
}
