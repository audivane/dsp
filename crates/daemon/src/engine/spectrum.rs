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
//! a new epoch, so samples that were produced at the old rate are never
//! interpreted as if they were at the new rate.
//!
//! A non-RT analysis thread runs two Hann FFTs on each tap: 8192 points
//! (50% overlap) for every log bin and 32768 points (87.5% overlap) for the
//! low band, where a 1/24-octave bin is narrower than a few short-FFT bins.
//! Each log bin reads band power (interpolated power density integrated over
//! the bin), with a sine's interpolated peak level where the bin holds a
//! spectral peak, so the two resolutions agree and narrow bins never repeat
//! one FFT bin as a plateau. Power is averaged over the frames since the last
//! report on a fixed log grid (centres 20 Hz .. min(20 kHz, Nyquist) at 24
//! bins/octave) and
//! publishes a [`Spectrum`] at roughly the frame cadence (about 10-20 Hz,
//! depending on the rate). Nothing is computed and no events are emitted while
//! no client subscribes to [`audivane_protocol::ipc::Topic::Spectrum`]: the IPC
//! event pump sets a shared atomic flag, the RT thread then stops copying, and
//! the analyser drains its rings and reports nothing. Idle (fewer than one
//! complete frame) likewise produces no events; clients keep the last frame
//! they received.

use audivane_dsp_core::fft::RealFftPair;
use audivane_protocol::ipc::Spectrum;
use rustfft::num_complex::Complex64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Short FFT: every log bin (about 5.9 Hz per FFT bin at 48 kHz).
pub const FFT_SIZE: usize = 8192;
/// Long FFT for the low band (about 1.5 Hz per FFT bin at 48 kHz).
pub const LONG_FFT_SIZE: usize = 32_768;
pub const BINS_PER_OCTAVE: u32 = 24;
pub const F_MIN_HZ: f64 = 20.0;
pub const F_MAX_HZ: f64 = 20_000.0;
/// Log bins narrower than this many short-FFT bins come from the long FFT.
const CROSSOVER_BINS: f64 = 4.0;
/// Ring capacity in frames per tap (about 85 ms at 192 kHz).
const RING_FRAMES: usize = 16_384;
/// Both FFTs advance by the same hop: 50% overlap for the short one, 87.5%
/// for the long one, so every report carries fresh data for both bands.
const HOP: usize = FFT_SIZE / 2;
const TICK: Duration = Duration::from_millis(10);
const DB_FLOOR: f64 = -160.0;
/// Equivalent noise bandwidth of a periodic Hann window, in FFT bins.
const HANN_ENBW: f64 = 1.5;

/// Number of log bins for a tap at `rate`: centres from 20 Hz up to
/// min(20 kHz, Nyquist).
pub fn spectrum_bin_count(rate: u32) -> usize {
    let nyquist = f64::from(rate) / 2.0;
    let top = nyquist.min(F_MAX_HZ);
    if top <= F_MIN_HZ {
        return 1;
    }
    ((f64::from(BINS_PER_OCTAVE) * (top / F_MIN_HZ).log2()).floor() as usize) + 1
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
}

impl TapEpoch {
    #[inline]
    fn tag(&mut self, rate: u32) -> Self {
        let rate = rate.max(8_000);
        if self.rate != rate || self.epoch == 0 {
            self.rate = rate;
            self.epoch = self.epoch.wrapping_add(1);
            // Zero is the analyser's "no epoch seen yet" sentinel.
            if self.epoch == 0 {
                self.epoch = 1;
            }
        }
        *self
    }
}

/// RT half: producers for the two taps and the shared control words.
pub struct SpectrumRt {
    pre: rtrb::Producer<TapSample>,
    post: rtrb::Producer<TapSample>,
    enabled: Arc<AtomicBool>,
    pre_epoch: TapEpoch,
    post_epoch: TapEpoch,
}

impl SpectrumRt {
    /// Create the rings and start the analysis thread.
    pub fn create(channels: usize) -> std::io::Result<(Self, SpectrumWorker)> {
        let channels = channels.max(1);
        let capacity = RING_FRAMES * channels;
        let (pre_tx, pre_rx) = rtrb::RingBuffer::<TapSample>::new(capacity);
        let (post_tx, post_rx) = rtrb::RingBuffer::<TapSample>::new(capacity);
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

        let rt = Self {
            pre: pre_tx,
            post: post_tx,
            enabled: Arc::clone(&enabled),
            pre_epoch: TapEpoch::default(),
            post_epoch: TapEpoch::default(),
        };
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

    /// Copy chain input samples (interleaved `f32`, graph rate). Drops when
    /// the ring is full. Never allocates.
    #[inline]
    pub fn push_pre(&mut self, samples: &[f32], graph_rate: u32) {
        // Keep the tag current even while disabled, so a client that
        // subscribes after a rate change starts on a fresh epoch.
        let tag = self.pre_epoch.tag(graph_rate);
        if !self.on() {
            return;
        }
        for &value in samples {
            let _ = self.pre.push(TapSample {
                value,
                rate: tag.rate,
                epoch: tag.epoch,
            });
        }
    }

    /// Copy the floating-point chain output at the output rate, immediately
    /// before quantization/dither. Drops when the ring is full. Never
    /// allocates.
    #[inline]
    pub fn push_post_f32(&mut self, samples: &[f32], output_rate: u32) {
        let tag = self.post_epoch.tag(output_rate);
        if !self.on() {
            return;
        }
        for &value in samples {
            let _ = self.post.push(TapSample {
                value,
                rate: tag.rate,
                epoch: tag.epoch,
            });
        }
    }

    /// Copy quantized output (interleaved left-justified `i32`) as `f32`.
    /// Used only on paths with no float signal (Direct / bypass). Never
    /// allocates.
    #[inline]
    pub fn push_post_i32(&mut self, samples: &[i32], output_rate: u32) {
        let tag = self.post_epoch.tag(output_rate);
        if !self.on() {
            return;
        }
        const SCALE: f32 = 1.0 / 2_147_483_648.0;
        for &sample in samples {
            let _ = self.post.push(TapSample {
                value: sample as f32 * SCALE,
                rate: tag.rate,
                epoch: tag.epoch,
            });
        }
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
    let mut pre = TapAnalyzer::new(channels);
    let mut post = TapAnalyzer::new(channels);
    let mut scratch: Vec<TapSample> = Vec::with_capacity(RING_FRAMES * channels);
    while !stop.load(Ordering::Acquire) {
        if !enabled.load(Ordering::Acquire) {
            drain(&mut pre_rx);
            drain(&mut post_rx);
            pre.reset();
            post.reset();
            thread::sleep(TICK);
            continue;
        }
        read_available(&mut pre_rx, &mut scratch);
        pre.feed(&scratch);
        read_available(&mut post_rx, &mut scratch);
        post.feed(&scratch);
        if pre.frames() > 0 && post.frames() > 0 {
            let rate = pre.rate();
            shared.publish(Spectrum {
                f_min_hz: F_MIN_HZ,
                bins_per_octave: BINS_PER_OCTAVE,
                pre: pre.finish(),
                post: post.finish(),
                rate,
            });
        }
        thread::sleep(TICK);
    }
}

fn drain(rx: &mut rtrb::Consumer<TapSample>) {
    while rx.pop().is_ok() {}
}

fn read_available(rx: &mut rtrb::Consumer<TapSample>, scratch: &mut Vec<TapSample>) {
    scratch.clear();
    while scratch.len() < scratch.capacity() {
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
struct LogBand {
    /// Index into the published log grid.
    index: usize,
    /// `(fft bin, weight)`: the integral over the band of the hat function
    /// centred on that FFT bin, in FFT-bin units. Summing `power * weight`
    /// integrates the linearly interpolated power density over the band, so
    /// bands narrower than one FFT bin still vary smoothly with frequency
    /// instead of repeating the same bin.
    weights: Vec<(usize, f64)>,
}

/// Log bins for one FFT size at `rate`. `max_width_hz` keeps only the bands
/// narrower than that (used to give the long FFT only the low band); `None`
/// serves every log bin.
fn log_bands(rate: u32, size: usize, max_width_hz: Option<f64>) -> Vec<LogBand> {
    let count = spectrum_bin_count(rate);
    let nyquist = f64::from(rate) / 2.0;
    let bin_hz = f64::from(rate) / size as f64;
    let max_bin = size / 2;
    let half = 2.0_f64.powf(0.5 / f64::from(BINS_PER_OCTAVE));
    let mut out = Vec::new();
    for index in 0..count {
        let centre = spectrum_bin_centre_hz(index);
        let lo_hz = centre / half;
        let hi_hz = (centre * half).min(nyquist);
        if let Some(max_width_hz) = max_width_hz
            && hi_hz - lo_hz >= max_width_hz
        {
            continue;
        }
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
        out.push(LogBand { index, weights });
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

    /// Tonal readings: a narrow band only holds part of a sine's main lobe,
    /// so for every genuine coherent tone peak in the power spectrum its
    /// parabolically interpolated peak (the sine's true level) goes to the
    /// log bin that contains the interpolated peak frequency, and that bin
    /// reads the higher of its band power and this peak.
    ///
    /// Random local maxima in broadband noise are rejected by requiring the
    /// peak to stand well above the surrounding noise floor outside the Hann
    /// main lobe (`±3..=±8` bins).
    fn find_tonal_peaks(&mut self) {
        self.tonal.fill(0.0);
        let bin_hz = f64::from(self.rate) / self.size as f64;
        let steps = f64::from(BINS_PER_OCTAVE);
        for k in 1..self.power.len() - 1 {
            let Some((delta, level)) = interpolated_peak(&self.power, k) else {
                continue;
            };
            if !is_coherent_tone(&self.power, k, level) {
                continue;
            }
            let freq = (k as f64 + delta) * bin_hz;
            if freq <= 0.0 {
                continue;
            }
            let index = (steps * (freq / F_MIN_HZ).log2()).round();
            if index < 0.0 || index >= self.slot_of.len() as f64 {
                continue;
            }
            if let Some(slot) = self.slot_of[index as usize]
                && level > self.tonal[slot]
            {
                self.tonal[slot] = level;
            }
        }
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

/// Band power of one log band from a power spectrum (0 dB = full-scale
/// sine): the linearly interpolated power density integrated over the band,
/// divided by the Hann equivalent noise bandwidth. This is the signal power
/// inside the band (a full-scale sine whose main lobe fits in the band reads
/// 0 dB, broadband noise reads its power in that band), so it does not depend
/// on the FFT size and the short and long resolutions agree. Linear
/// interpolation never exceeds the larger of its two neighbours, so it
/// cannot invent a peak.
fn band_power(power: &[f64], band: &LogBand) -> f64 {
    band.weights
        .iter()
        .map(|&(k, w)| power[k] * w)
        .sum::<f64>()
        / HANN_ENBW
}

/// Parabolic (log-power) interpolation around `peak`, but only when `peak`
/// is a genuine local maximum of the FFT power spectrum: the fitted vertex
/// has to stay inside ±0.5 FFT bins and its amplitude has to be plausible
/// next to the neighbours. A bin on the shoulder of a peak that lives outside
/// the log bin is not a local maximum; fitting it would invent a level far
/// above the real signal.
/// Returns the vertex offset in bins and its power.
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
/// rate always lands in the 44.1/48 kHz family while preserving 20 Hz..20 kHz.
fn decimation_stages(rate: u32) -> usize {
    let mut r = rate;
    let mut stages = 0;
    while r >= 88_200 && r.is_multiple_of(2) {
        r /= 2;
        stages += 1;
    }
    stages
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
struct HalfBandStage {
    center: f64,
    odd: Vec<f64>,
    ch: Vec<HalfBandChState>,
}

impl HalfBandStage {
    /// Build a Kaiser-windowed half-band decimator for `remaining_stages`
    /// after this stage (`0` = final `88.2k/96k -> 44.1k/48k` stage).
    fn new(channels: usize, remaining_stages: usize) -> Self {
        // Kaiser beta = 10.5 gives >105 dB stopband attenuation; passband is
        // flat (<0.0001 dB) up to 20 kHz at every stage.
        let k = match remaining_stages {
            0 => 40, // N = 159 taps (transition 20..24.1 kHz @ 88.2k)
            1 => 8,  // N = 31 taps (transition 20..68.2 kHz @ 176.4k)
            2 => 6,  // N = 23 taps
            _ => 5,  // N = 19 taps
        };
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
    fn push(&mut self, channel: usize, sample: f64) -> Option<f64> {
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

    fn reset(&mut self) {
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

    fn configure(&mut self, channels: usize, num_stages: usize) {
        self.stages.clear();
        for s in 0..num_stages {
            let remaining = num_stages - 1 - s;
            self.stages.push(HalfBandStage::new(channels, remaining));
        }
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
    fn push_frame(&mut self, frame: &[TapSample], out: &mut [f32]) -> bool {
        let mut ready = true;
        for (c, sample) in frame.iter().enumerate() {
            let mut val = f64::from(sample.value);
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

/// One tap's rolling frames, the two FFT resolutions and averaged log bins.
///
/// High sample rates (`>= 88.2 kHz`) are decimated on the analysis thread via
/// a cascaded half-band anti-alias filter down to the 44.1/48 kHz family so
/// resolution and bass calibration stay uniform from 44.1 kHz to 768 kHz.
/// Every [`HOP`] decimated frames the short FFT ([`FFT_SIZE`]) analyses the
/// newest frames for every log bin; once [`LONG_FFT_SIZE`] decimated frames
/// are buffered the long FFT also analyses the low band (log bins narrower
/// than [`CROSSOVER_BINS`] short-FFT bins, below roughly 800 Hz at 48 kHz).
pub(crate) struct TapAnalyzer {
    channels: usize,
    rate: u32,
    analysis_rate: u32,
    epoch: u32,
    bins: usize,
    decimator: CascadedDecimator,
    decim_frame: Vec<f32>,
    /// Interleaved `LONG_FFT_SIZE`-frame rolling buffer (at `analysis_rate`).
    buf: Vec<f32>,
    fill: usize,
    short: Resolution,
    long: Resolution,
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
            decim_frame: vec![0.0; channels],
            buf: vec![0.0; LONG_FFT_SIZE * channels],
            fill: 0,
            short: Resolution::new(FFT_SIZE),
            long: Resolution::new(LONG_FFT_SIZE),
        }
    }

    pub(crate) fn set_rate(&mut self, rate: u32) {
        self.rate = rate.max(1);
        let stages = decimation_stages(self.rate);
        self.analysis_rate = (self.rate >> stages).max(1);
        self.decimator.configure(self.channels, stages);
        self.bins = spectrum_bin_count(self.rate);
        let short_bin_hz = f64::from(self.analysis_rate) / FFT_SIZE as f64;
        let (analysis_rate, bins) = (self.analysis_rate, self.bins);
        self.short
            .set_bands(analysis_rate, bins, log_bands(analysis_rate, FFT_SIZE, None));
        self.long.set_bands(
            analysis_rate,
            bins,
            log_bands(
                analysis_rate,
                LONG_FFT_SIZE,
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
            let base = self.fill * channels;
            if self.decimator.is_identity() {
                for (dst, sample) in self.buf[base..base + channels].iter_mut().zip(frame) {
                    *dst = sample.value;
                }
            } else if self.decimator.push_frame(frame, &mut self.decim_frame) {
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

    fn sine_mono(frames: usize, rate: u32, freq: f64, amplitude: f64) -> Vec<f32> {
        (0..frames)
            .map(|n| {
                (amplitude * (2.0 * std::f64::consts::PI * freq * n as f64 / f64::from(rate)).sin())
                    as f32
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
    fn bin_count_matches_the_formula() {
        assert_eq!(spectrum_bin_count(44_100), 240);
        assert_eq!(spectrum_bin_count(48_000), 240);
        assert_eq!(spectrum_bin_count(96_000), 240);
        // A low rate limits the top to its Nyquist.
        assert_eq!(spectrum_bin_count(8_000), 184);
        // 20 Hz .. min(20 kHz, Nyquist), 24 bins/octave.
        for rate in [8_000u32, 32_000, 44_100, 48_000, 192_000] {
            let top = (f64::from(rate) / 2.0).min(F_MAX_HZ);
            let expected = ((24.0 * (top / F_MIN_HZ).log2()).floor() as usize) + 1;
            assert_eq!(spectrum_bin_count(rate), expected);
        }
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
            let peak_hz =
                spectrum_bin_centre_hz(peak_bin);
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
    fn channel_mean_power_ignores_a_silent_channel() {
        let rate = 48_000u32;
        let mut tap = TapAnalyzer::new(1);
        tap.set_rate(rate);
        tap.feed(&tagged(
            sine_mono(FFT_SIZE * 2, rate, 1_000.0, 1.0),
            rate,
            1,
        ));
        let mono = tap.finish();

        let mut stereo = TapAnalyzer::new(2);
        stereo.set_rate(rate);
        let mut samples = sine(FFT_SIZE * 2, rate, 1_000.0, 1.0);
        for frame in samples.as_chunks_mut::<2>().0 {
            frame[1] = 0.0;
        }
        stereo.feed(&tagged(samples, rate, 1));
        let mean = stereo.finish();

        assert_eq!(mono.len(), mean.len());
        // Compare the bins with signal in them (both hit the -160 dB floor
        // elsewhere).
        for (a, b) in mono.iter().zip(&mean).filter(|(a, _)| **a > -100.0) {
            assert!(
                (f64::from(a - b) - 3.0).abs() < 0.2,
                "channel-mean power with one silent channel is 3 dB below mono: {a} vs {b}"
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
    fn low_frequency_sine_does_not_inflate_neighbouring_bins() {
        // Reviewer probe: -12.04 dBFS at 35.156 Hz used to show +17.9 dBFS in
        // bins that only contained the shoulder of the peak.
        let rate = 48_000;
        let freq = 35.156_25;
        let amplitude = 10.0_f64.powf(-12.04 / 20.0);
        let mut tap = TapAnalyzer::new(2);
        tap.set_rate(rate);
        tap.feed(&tagged(sine(FFT_SIZE * 4, rate, freq, amplitude), rate, 1));
        let bins = tap.finish();
        let (peak_index, peak) = peak_bin(&bins);
        assert!(
            (f64::from(peak) - (-12.04)).abs() <= 0.5,
            "35.156 Hz reads {peak} dB in bin {peak_index}"
        );
        let next = bins
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != peak_index)
            .map(|(_, &db)| db)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            next <= peak + 0.5,
            "a neighbouring bin ({next} dB) is above the peak ({peak} dB)"
        );
    }

    #[test]
    fn bin_centred_and_off_bin_sines_read_zero_db() {
        let rate = 48_000u32;
        // Exactly on FFT bin 100 (Hann has no scalloping loss there).
        let bin_centred = 100.0 * f64::from(rate) / FFT_SIZE as f64;
        // Deliberately between two bins, and exactly halfway between two bins.
        let off_bin = 1_000.0;
        let half_bin = 100.5 * f64::from(rate) / FFT_SIZE as f64;

        for (freq, label) in [
            (bin_centred, "bin-centred"),
            (off_bin, "off-bin"),
            (half_bin, "half-bin"),
        ] {
            let mut tap = TapAnalyzer::new(2);
            tap.set_rate(rate);
            tap.feed(&tagged(sine(FFT_SIZE * 2 + HOP, rate, freq, 1.0), rate, 1));
            let bins = tap.finish();
            let (index, peak) = peak_bin(&bins);
            assert!(
                (f64::from(peak) - 0.0).abs() <= 0.5,
                "{label} sine at {freq} Hz reads {peak} dB in bin {index}"
            );
        }
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
        // Reviewer probe: a 999 Hz tone at 48 kHz reappeared near 2 kHz when
        // queued old-rate samples were analysed at 96 kHz.
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

    fn mono_to_stereo(samples: Vec<f32>) -> Vec<f32> {
        samples.into_iter().flat_map(|s| [s, s]).collect()
    }

    fn bin_of(freq: f64) -> usize {
        (f64::from(BINS_PER_OCTAVE) * (freq / F_MIN_HZ).log2()).round() as usize
    }

    #[test]
    fn low_band_sines_read_zero_db_from_the_long_fft() {
        for rate in [44_100u32, 48_000, 96_000, 192_000] {
            let factor = 1usize << decimation_stages(rate);
            for freq in [23.0, 31.7, 50.0, 87.3, 160.0, 400.0] {
                let mut tap = TapAnalyzer::new(2);
                tap.set_rate(rate);
                tap.feed(&tagged(
                    sine((LONG_FFT_SIZE + HOP * 2) * factor, rate, freq, 1.0),
                    rate,
                    1,
                ));
                assert!(tap.long.frames > 0);
                let bins = tap.finish();
                let (index, peak) = peak_bin(&bins);
                assert!(
                    f64::from(peak).abs() <= 0.5,
                    "rate {rate}: {freq} Hz reads {peak} dB"
                );
                assert!(
                    index.abs_diff(bin_of(freq)) <= 1,
                    "rate {rate}: {freq} Hz peaks at {} Hz",
                    spectrum_bin_centre_hz(index)
                );
            }
        }
    }

    #[test]
    fn high_rate_sines_stay_calibrated_across_all_rates() {
        for rate in [44_100u32, 48_000, 96_000, 192_000, 384_000, 768_000] {
            let factor = 1usize << decimation_stages(rate);
            let frames = (LONG_FFT_SIZE + HOP * 2) * factor;
            for freq in [20.0, 31.7, 100.0, 1_000.0, 10_000.0] {
                let mut tap = TapAnalyzer::new(2);
                tap.set_rate(rate);
                tap.feed(&tagged(sine(frames, rate, freq, 1.0), rate, 1));
                assert!(tap.long.frames > 0, "rate {rate} {freq} Hz produced no long frames");
                let bins = tap.finish();
                let (index, peak) = peak_bin(&bins);
                assert!(
                    f64::from(peak).abs() <= 0.5,
                    "rate {rate}: {freq} Hz sine reads {peak} dB (expected 0 +- 0.5 dB)"
                );
                assert!(
                    index.abs_diff(bin_of(freq)) <= 1,
                    "rate {rate}: {freq} Hz sine peaks at {:.1} Hz (bin {index})",
                    spectrum_bin_centre_hz(index)
                );
            }
        }
    }

    /// Generate stereo independent pink noise (1/f power density across
    /// 5 Hz .. 20 kHz) calibrated to a fixed per-octave power independent of
    /// sample rate, using an inverse FFT of random-phase 1/sqrt(f) bins.
    fn steady_pink_noise(frames: usize, rate: u32, octave_rms: f64, seed: u64) -> Vec<f32> {
        let nfft = frames.next_power_of_two();
        let mut fft = RealFftPair::new(nfft);
        let spec_len = fft.spec_len();
        let bin_hz = f64::from(rate) / nfft as f64;
        let max_bin = ((20_000.0 / bin_hz).floor() as usize).min(spec_len - 1);
        let min_bin = ((5.0 / bin_hz).ceil() as usize).max(1);

        // Normalize so total power summed across one octave equals octave_rms^2:
        // sum_{f in [f0, 2*f0]} |X_k|^2 = (sum_{k} 1/k) * C^2 = ln(2) * C^2 = octave_rms^2
        // => each complex FFT bin gets amplitude C / sqrt(k) * nfft / 2?
        // In unnormalized inverse_complex_normalized (1/nfft scaling), a real sine of
        // amplitude A at bin k has X[k] = (nfft / 2) * A, power A^2 / 2.
        // Wait: a frame of white-phase bins X[k] = (nfft / sqrt(2)) * amp_k produces
        // mean-square per bin sum(amp_k^2). Let's normalize directly to target band power.
        let scale = (octave_rms * octave_rms / 2.0_f64.ln()).sqrt() * (nfft as f64) * 0.5_f64.sqrt();

        let mut state = seed;
        let mut next_u01 = || -> f64 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64) / ((1u64 << 53) as f64)
        };

        let mut ch_bufs = [vec![0.0_f64; nfft], vec![0.0_f64; nfft]];
        let mut spec = vec![Complex64::new(0.0, 0.0); spec_len];
        for ch_buf in &mut ch_bufs {
            spec.fill(Complex64::new(0.0, 0.0));
            for (k, slot) in spec.iter_mut().enumerate().take(max_bin + 1).skip(min_bin) {
                let mag = scale / (k as f64).sqrt();
                let phase = 2.0 * std::f64::consts::PI * next_u01();
                *slot = Complex64::from_polar(mag, phase);
            }
            fft.inverse_complex_normalized(&mut spec, ch_buf);
        }

        let mut out = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            out.push(ch_bufs[0][i] as f32);
            out.push(ch_bufs[1][i] as f32);
        }
        out
    }

    /// Independent stereo white noise with per-channel RMS `rms`.
    fn independent_white_noise(frames: usize, rms: f64, seed: u64) -> Vec<f32> {
        let mut state = seed;
        let mut out = Vec::with_capacity(frames * 2);
        for _ in 0..frames * 2 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let unit = ((state >> 11) as f64) / ((1u64 << 53) as f64);
            out.push(((unit * 2.0 - 1.0) * rms * 3.0_f64.sqrt()) as f32);
        }
        out
    }

    #[test]
    fn pink_noise_is_flat_without_bass_lift_and_matches_across_all_rates() {
        // 1) At 48 kHz, average multiple seeds (representing long-term analyser
        //    averaging) and verify every 1/24-oct bin from 20 Hz to 18 kHz is flat
        //    within +-0.5 dB with zero bass lift from random noise peaks.
        let rate_48 = 48_000u32;
        let frames_48 = LONG_FFT_SIZE * 8;
        let n_bins = spectrum_bin_count(rate_48);
        let mut lin_sum = vec![0.0_f64; n_bins];
        const TRIALS: usize = 8;
        for t in 0..TRIALS {
            let mut tap = TapAnalyzer::new(2);
            tap.set_rate(rate_48);
            tap.feed(&tagged(
                steady_pink_noise(frames_48, rate_48, 0.05, 0xA000_0000 + t as u64),
                rate_48,
                1,
            ));
            let bins = tap.finish();
            for (dst, &db) in lin_sum.iter_mut().zip(&bins) {
                *dst += 10.0_f64.powf(f64::from(db) / 10.0);
            }
        }
        let avg_db: Vec<f64> = lin_sum
            .iter()
            .map(|&p| 10.0 * (p / TRIALS as f64).log10())
            .collect();
        let top_bin = bin_of(18_000.0).min(avg_db.len() - 1);
        let mut sorted = avg_db[..=top_bin].to_vec();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        for (i, &db) in avg_db[..=top_bin].iter().enumerate() {
            assert!(
                (db - median).abs() <= 0.5,
                "48 kHz pink noise bin {i} ({:.1} Hz): {db:.2} dB vs median {median:.2} dB",
                spectrum_bin_centre_hz(i)
            );
        }

        // 2) Across all rates (44.1k..768k), steady pink noise reads the same
        //    level (low, mid and high bands match within +-0.5 dB).
        let mut ref_low = None;
        let mut ref_mid = None;
        let mut ref_high = None;
        for rate in [44_100u32, 48_000, 96_000, 192_000, 384_000, 768_000] {
            let factor = 1usize << decimation_stages(rate);
            let frames = (LONG_FFT_SIZE * 4) * factor;
            let mut tap = TapAnalyzer::new(2);
            tap.set_rate(rate);
            tap.feed(&tagged(
                steady_pink_noise(frames, rate, 0.05, 0xBEEF_0000 + u64::from(rate)),
                rate,
                1,
            ));
            assert!(tap.long.frames > 0);
            let bins = tap.finish();
            let mean_db = |lo_hz: f64, hi_hz: f64| -> f64 {
                let lo = bin_of(lo_hz);
                let hi = bin_of(hi_hz).min(bins.len() - 1);
                let lin = (lo..=hi)
                    .map(|i| 10.0_f64.powf(f64::from(bins[i]) / 10.0))
                    .sum::<f64>()
                    / (hi - lo + 1) as f64;
                10.0 * lin.log10()
            };
            let low = mean_db(20.0, 200.0);
            let mid = mean_db(200.0, 2_000.0);
            let high = mean_db(2_000.0, 16_000.0);
            assert!(
                (low - mid).abs() <= 0.6 && (high - mid).abs() <= 0.6,
                "rate {rate}: pink noise tilt low={low:.2} mid={mid:.2} high={high:.2}"
            );
            let r_low = *ref_low.get_or_insert(low);
            let r_mid = *ref_mid.get_or_insert(mid);
            let r_high = *ref_high.get_or_insert(high);
            assert!(
                (low - r_low).abs() <= 0.5
                    && (mid - r_mid).abs() <= 0.5
                    && (high - r_high).abs() <= 0.5,
                "rate {rate}: pink level low={low:.2} (ref {r_low:.2}), mid={mid:.2} (ref {r_mid:.2}), high={high:.2} (ref {r_high:.2})"
            );
        }
    }

    #[test]
    fn white_noise_rises_three_db_per_octave() {
        let rate = 48_000u32;
        let frames = LONG_FFT_SIZE * 8;
        let n_bins = spectrum_bin_count(rate);
        let mut lin_sum = vec![0.0_f64; n_bins];
        const TRIALS: usize = 4;
        for t in 0..TRIALS {
            let mut tap = TapAnalyzer::new(2);
            tap.set_rate(rate);
            tap.feed(&tagged(
                independent_white_noise(frames, 0.1, 0x1234_5678 + t as u64),
                rate,
                1,
            ));
            let bins = tap.finish();
            for (dst, &db) in lin_sum.iter_mut().zip(&bins) {
                *dst += 10.0_f64.powf(f64::from(db) / 10.0);
            }
        }
        let avg_lin: Vec<f64> = lin_sum.iter().map(|&p| p / TRIALS as f64).collect();
        // Compare third-octave averages spaced 1 octave apart (24 bins apart).
        let group_db = |centre_bin: usize| -> f64 {
            let lin: f64 = avg_lin[centre_bin - 3..=centre_bin + 3].iter().sum::<f64>() / 7.0;
            10.0 * lin.log10()
        };
        let octaves = [
            bin_of(100.0),
            bin_of(200.0),
            bin_of(400.0),
            bin_of(800.0),
            bin_of(1_600.0),
            bin_of(3_200.0),
            bin_of(6_400.0),
            bin_of(12_800.0),
        ];
        for w in octaves.windows(2) {
            let slope = group_db(w[1]) - group_db(w[0]);
            assert!(
                (slope - 3.01).abs() <= 0.5,
                "white noise rose {slope:.2} dB between {:.0} Hz and {:.0} Hz (expected +3.01 dB/oct)",
                spectrum_bin_centre_hz(w[0]),
                spectrum_bin_centre_hz(w[1]),
            );
        }
    }

    #[test]
    #[ignore]
    fn bench_analyzer_768khz_cpu() {
        use std::time::Instant;
        let rate = 768_000u32;
        let secs = 10usize;
        let frames = rate as usize * secs;
        let samples = tagged(sine(frames, rate, 100.0, 0.5), rate, 1);
        let mut tap = TapAnalyzer::new(2);
        tap.set_rate(rate);

        // Warmup 1s
        tap.feed(&samples[..rate as usize * 2]);
        let _ = tap.finish();

        let t0 = Instant::now();
        for chunk in samples.chunks(1024 * 2) {
            tap.feed(chunk);
        }
        let _ = tap.finish();
        let elapsed = t0.elapsed().as_secs_f64();
        let cpu_pct = (elapsed / secs as f64) * 100.0;
        eprintln!(
            "TapAnalyzer 768 kHz stereo (single tap): {secs}s audio in {elapsed:.4}s => {cpu_pct:.2}% of 1 core (2 taps pre+post ~{:.2}%)",
            cpu_pct * 2.0
        );
    }

    #[test]
    fn long_fft_resolves_close_bass_tones() {
        // 40 and 50 Hz are under two short-FFT bins apart at 48 kHz, so their
        // main lobes merge there; only the long FFT shows a dip between them.
        let rate = 48_000u32;
        let frames = LONG_FFT_SIZE + HOP * 4;
        let a = sine_mono(frames, rate, 40.0, 0.5);
        let b = sine_mono(frames, rate, 50.0, 0.5);
        let mixed: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x + y).collect();
        let mut tap = TapAnalyzer::new(2);
        tap.set_rate(rate);
        tap.feed(&tagged(mono_to_stereo(mixed), rate, 1));
        let bins = tap.finish();
        let at = |f: f64| {
            let i = bin_of(f);
            f64::from(bins[i - 1].max(bins[i]).max(bins[i + 1]))
        };
        let dip = f64::from(bins[bin_of(44.7)]);
        assert!(at(40.0) > dip + 12.0, "40 Hz {} vs dip {dip}", at(40.0));
        assert!(at(50.0) > dip + 12.0, "50 Hz {} vs dip {dip}", at(50.0));
        let six = 20.0 * 0.5_f64.log10();
        assert!((at(40.0) - six).abs() <= 0.6 && (at(50.0) - six).abs() <= 0.6);
    }

    #[test]
    fn narrow_low_bins_do_not_form_plateaus() {
        // Before the long FFT and the interpolated bands, several 1/24-octave
        // bins below ~150 Hz repeated one FFT bin and the curve stair-stepped.
        let rate = 48_000u32;
        let mut tap = TapAnalyzer::new(2);
        tap.set_rate(rate);
        tap.feed(&tagged(sine(LONG_FFT_SIZE + HOP * 2, rate, 60.0, 1.0), rate, 1));
        let bins = tap.finish();
        let mut run = 1;
        for i in 1..bin_of(200.0) {
            if bins[i] > -120.0 && bins[i] == bins[i - 1] {
                run += 1;
                assert!(run < 3, "plateau ending at bin {i}: {:?}", &bins[..bin_of(200.0)]);
            } else {
                run = 1;
            }
        }
        // Short FFT alone (before the long buffer has filled) is smooth too.
        let mut short = TapAnalyzer::new(2);
        short.set_rate(rate);
        short.feed(&tagged(sine(FFT_SIZE * 2, rate, 60.0, 1.0), rate, 1));
        assert_eq!(short.long.frames, 0);
        let bins = short.finish();
        let mut run = 1;
        for i in 1..bin_of(200.0) {
            if bins[i] > -120.0 && bins[i] == bins[i - 1] {
                run += 1;
                assert!(run < 3, "short-FFT plateau ending at bin {i}");
            } else {
                run = 1;
            }
        }
    }

    #[test]
    fn white_noise_reads_its_band_power_on_both_sides_of_the_crossover() {
        // Band power: a white noise of RMS r has power 2 r^2 relative to a
        // full-scale sine, spread evenly up to Nyquist.
        let rate = 48_000u32;
        let rms = 0.1;
        let mut tap = TapAnalyzer::new(2);
        tap.set_rate(rate);
        tap.feed(&tagged(white_noise(LONG_FFT_SIZE * 6, rms, 0xC0FFEE), rate, 1));
        let bins = tap.finish();
        let nyquist = f64::from(rate) / 2.0;
        let half = 2.0_f64.powf(0.5 / f64::from(BINS_PER_OCTAVE));
        let expected = |i: usize| {
            let c = spectrum_bin_centre_hz(i);
            10.0 * (2.0 * rms * rms * (c * half - c / half) / nyquist).log10()
        };
        // Average error in third-octave groups (8 bins) from 100 Hz to 10 kHz,
        // which spans the crossover (about 800 Hz).
        let mut i = bin_of(100.0);
        while i + 8 <= bin_of(10_000.0) {
            let err: f64 =
                (i..i + 8).map(|k| f64::from(bins[k]) - expected(k)).sum::<f64>() / 8.0;
            assert!(
                err.abs() <= 1.5,
                "white noise near {:.0} Hz reads {err:+.2} dB off its band power",
                spectrum_bin_centre_hz(i)
            );
            i += 8;
        }
    }
}
