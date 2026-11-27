//! The distortion spectrum of the harmonics stage for one sine, as a
//! measurement of the system would show it. Off the audio thread.
//!
//! # Tone and window
//!
//! The tone runs through the stage (or one of its [`HarmonicsSpectrumTrace`]
//! parts) at the real output rate, with nothing band-limited, so every order
//! lands where it really does, folded or not. It is made coherent with a
//! power-of-two window of `n` samples on an odd bin `c`: the steady state
//! repeats every `n` samples, and as `c` is invertible modulo `n`, order `k`
//! lands on bin `±k c mod n` and every bin holds exactly one order (below
//! `n / 2`), so the readout names each peak exactly. The bin width is about
//! a sixteenth of the tone, between [`MIN_BIN_HZ`] and [`MAX_BIN_HZ`]
//! (`n` at most [`MAX_N`]), so the windowed harmonics stay apart.
//!
//! The stage settles for [`READOUT_SETTLE_SECONDS`] (or its slow layers'
//! own settle; sag/bias envelopes presettled), plus [`EXTRA_SETTLE_S`]
//! without a tape layer (the 5 Hz DC blockers and the envelopes converge by
//! about 60 dB a second; cheap for the other layers), and the window is
//! analysed with no window: exact for the periodic steady state, every order
//! in one bin (a hairline on the plot), DC in bin 0 (off the plot). That
//! gives the numbers (each order's level, THD, THD+N) and the plot.
//!
//! A part that is not quite periodic (an envelope still moving) leaks over
//! the whole unwindowed spectrum, so the simulation runs [`TAIL`] samples
//! past the window: the jump between them and the window's start bounds the
//! leakage (a jump `d` leaks about `d / (π k)` of the fundamental's peak
//! into bin `k`). If that reaches within [`EXACT_MARGIN_DB`] of the output
//! word's floor, the plot uses a 7-term Blackman-Harris window (sidelobes
//! below −180 dB) instead, so the leak smears no further than it does on an
//! analyser.
//!
//! # Output word
//!
//! The simulation is in `f64`, so its own floor is near −300 dB. What the
//! engine plays is rounded to the output word after the fixed headroom
//! ([`HarmonicsSpectrumWord`]), so each line also gives that word's noise
//! (rounding plus the dither's unshaped variance, flat) per bin of the
//! plotted window, and THD+N counts it over 20 Hz..=20 kHz.
//!
//! # Cost
//!
//! One simulation per line of about `2 s + n / rate` of audio at the
//! output rate (`n / rate` is 0.08–0.7 s; `0.5 s + n / rate` with a tape
//! layer): 10–60 ms for Chebyshev and flux layers at 96 kHz, 50–260 ms at
//! 705.6 kHz, a tape layer at its real-time factor (about 0.1–0.5× per
//! layer at 768 kHz). Lines run on up to [`MAX_THREADS`] threads, and each
//! is cached by its layers' parameters, the rate and the tone.

use crate::harmonics::{Harmonics, HarmonicsError, READOUT_SETTLE_SECONDS};
use crate::harmonics_sim::{Lru, MAX_THREADS, unlabeled};
use crate::{Complex64, Stage};
use audivane_protocol::HarmonicsConfig;
use audivane_protocol::HarmonicsLayer;
use audivane_protocol::ipc::{
    HarmonicsSpectrum, HarmonicsSpectrumLine, HarmonicsSpectrumPeak, HarmonicsSpectrumTrace,
    HarmonicsSpectrumWord,
};
use std::f64::consts::PI;
use std::sync::{Arc, Mutex, MutexGuard};

/// Plotted points per line.
pub const SPECTRUM_POINTS: usize = 2048;
/// Samples of a line's one-cycle wave.
pub const WAVE_POINTS: usize = 512;
/// Lowest plotted frequency, Hz.
const PLOT_LOW_HZ: f64 = 10.0;
/// Narrowest and widest FFT bin, Hz.
const MIN_BIN_HZ: f64 = 1.0;
const MAX_BIN_HZ: f64 = 12.0;
/// Longest window, samples.
const MAX_N: usize = 1 << 19;
/// Shortest window, samples.
const MIN_N: usize = 1 << 12;
/// Highest labelled order.
const MAX_LABEL_ORDER: usize = 64;
/// Orders quieter than this are not labelled, dB.
const LABEL_FLOOR_DB: f64 = -200.0;
/// Floor of the plotted levels, dB.
const PLOT_FLOOR_DB: f64 = -250.0;
/// Floor of THD and THD+N, dB.
const FLOOR_DB: f64 = -200.0;
/// The band THD+N counts, Hz.
const BAND_HZ: (f64, f64) = (20.0, 20_000.0);
/// Lowest and highest tone, Hz (the top as a fraction of the rate).
pub const MIN_TONE_HZ: f64 = 10.0;
pub const MAX_TONE_FRACTION: f64 = 0.45;
/// Lowest tone level, dBFS.
pub const MIN_LEVEL_DBFS: f64 = -120.0;
/// Most lines in one call.
pub const MAX_TRACES: usize = 16;
/// Lines the cache keeps.
const CACHE_ENTRIES: usize = 24;
/// Samples simulated past the window to test it for periodicity.
const TAIL: usize = 256;
/// Extra settling without a tape layer, seconds (tape layers keep their own
/// settle: they cost real time).
pub const EXTRA_SETTLE_S: f64 = 1.5;
/// The leakage bound must stay this far under the word's floor for the
/// exact plot, dB.
const EXACT_MARGIN_DB: f64 = 10.0;

/// 7-term Blackman-Harris coefficients (sidelobes about −180 dB).
const BH7: [f64; 7] = [
    0.271_051_400_693_42,
    0.433_297_939_234_48,
    0.218_122_999_543_11,
    0.065_925_446_388_03,
    0.010_811_742_098_37,
    0.000_776_584_825_22,
    0.000_013_887_217_35,
];

/// The coherent tone at a rate: window length, tone bin, frequency, peak.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Plan {
    n: usize,
    bin: usize,
    freq: f64,
    amp: f64,
}

impl Plan {
    fn at(rate: f64, freq: f64, level_dbfs: f64) -> Self {
        let width = (freq / 16.0).clamp(MIN_BIN_HZ, MAX_BIN_HZ);
        let n = ((rate / width).ceil() as usize)
            .next_power_of_two()
            .clamp(MIN_N, MAX_N);
        let exact = freq * n as f64 / rate;
        let mut bin = exact.round().max(1.0) as usize;
        if bin.is_multiple_of(2) {
            bin = if exact >= bin as f64 || bin == 2 {
                bin + 1
            } else {
                bin - 1
            };
        }
        let bin = bin.min(n / 2 - 1);
        Self {
            n,
            bin,
            freq: bin as f64 * rate / n as f64,
            amp: crate::db_to_lin(level_dbfs),
        }
    }

    /// The bin order `k` lands on (folded into `0..=n/2`).
    fn bin_of(&self, k: usize) -> usize {
        let j = (k as u128 * self.bin as u128 % self.n as u128) as usize;
        j.min(self.n - j)
    }
}

/// The inverse of odd `c` modulo `n` (a power of two).
fn inverse_mod_pow2(c: usize, n: usize) -> usize {
    // Newton: x ← x (2 − c x), doubling the correct low bits each step.
    let (c, mask) = (c as u64, n as u64 - 1);
    let mut x: u64 = 1;
    for _ in 0..6 {
        x = x.wrapping_mul(2u64.wrapping_sub(c.wrapping_mul(x)));
    }
    (x & mask) as usize
}

/// One line before the output word is applied (cached).
#[derive(Clone, Debug)]
struct Raw {
    gain_db: f64,
    /// Harmonics below Nyquist, power relative to the fundamental.
    thd: f64,
    /// Everything but the fundamental in the band, power relative to it.
    band: f64,
    /// Fundamental's peak out.
    out_amp: f64,
    /// The plot unwindowed (exact) and windowed.
    exact_db: Vec<f32>,
    windowed_db: Vec<f32>,
    /// The leakage bound of the unwindowed spectrum (module docs), dB
    /// relative to the fundamental.
    leak_db: f64,
    peaks: Vec<HarmonicsSpectrumPeak>,
    wave: Vec<f32>,
}

/// The config a trace runs.
fn trace_config(
    config: &HarmonicsConfig,
    trace: HarmonicsSpectrumTrace,
) -> Result<HarmonicsConfig, HarmonicsError> {
    let get = |layer: u32| {
        config
            .layers
            .get(layer as usize)
            .ok_or_else(|| HarmonicsError::InvalidLevel(format!("trace: no layer {layer}")))
    };
    match trace {
        HarmonicsSpectrumTrace::Stage => Ok(config.clone()),
        HarmonicsSpectrumTrace::Layer { layer } => {
            let mut l = get(layer)?.clone();
            match &mut l {
                HarmonicsLayer::Chebyshev(c) => c.enabled = true,
                HarmonicsLayer::Flux(f) => f.enabled = true,
                HarmonicsLayer::Hysteresis(h) => h.enabled = true,
            }
            Ok(HarmonicsConfig {
                enabled: true,
                layers: vec![l],
                ..config.clone()
            })
        }
        HarmonicsSpectrumTrace::After { layer } => {
            get(layer)?;
            if !config.routing.is_series() {
                return Err(HarmonicsError::InvalidLevel(
                    "trace: `after` needs series routing".into(),
                ));
            }
            Ok(HarmonicsConfig {
                layers: config.layers[..=layer as usize].to_vec(),
                ..config.clone()
            })
        }
    }
}

/// The steady-state output window of `config` on the tone and [`TAIL`]
/// samples past it (phase-aligned: sample `i` is the response to the input
/// `amp cos(2π c i / n)`).
fn simulate(config: &HarmonicsConfig, rate: f64, plan: Plan) -> Result<Vec<f64>, HarmonicsError> {
    let Plan { n, bin, amp, .. } = plan;
    let mut stage = Harmonics::try_new(config, rate, 1)?;
    let mut settle_s = stage.readout_settle_s().max(READOUT_SETTLE_SECONDS);
    let tape = config
        .layers
        .iter()
        .any(|l| matches!(l, HarmonicsLayer::Hysteresis(h) if h.enabled));
    if !tape {
        settle_s += EXTRA_SETTLE_S;
    }
    let settle = ((settle_s * rate).ceil() as usize).div_ceil(n) * n;
    let period: Vec<f64> = (0..n)
        .map(|i| amp * (2.0 * PI * ((bin * i) % n) as f64 / n as f64).cos())
        .collect();
    if stage.dynamic_layers() > 0 {
        stage.presettle(&period);
    }
    let mut buf: Vec<f64> = period
        .iter()
        .copied()
        .cycle()
        .take(settle + n + TAIL)
        .collect();
    stage.process(&mut [buf.as_mut_slice()]);
    buf.drain(..settle);
    Ok(buf)
}

fn fft(x: &[f64]) -> Vec<Complex64> {
    use realfft::RealFftPlanner;
    let mut x = x.to_vec();
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(x.len());
    let mut spec = fft.make_output_vec();
    fft.process(&mut x, &mut spec)
        .expect("FFT lengths match the plan");
    spec
}

fn bh7(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            let t = 2.0 * PI * i as f64 / n as f64;
            BH7.iter()
                .enumerate()
                .map(|(m, &a)| if m % 2 == 0 { a } else { -a } * (m as f64 * t).cos())
                .sum()
        })
        .collect()
}

fn db10(p: f64, floor: f64) -> f64 {
    let d = 10.0 * p.log10();
    if d.is_finite() { d.max(floor) } else { floor }
}

/// The plotted points' frequencies at `rate`.
fn plot_freqs(rate: f64) -> Vec<f64> {
    let (lo, hi) = (PLOT_LOW_HZ, 0.5 * rate);
    (0..SPECTRUM_POINTS)
        .map(|i| lo * (hi / lo).powf(i as f64 / (SPECTRUM_POINTS - 1) as f64))
        .collect()
}

/// The plotted points from per-bin levels `bin_db`: the peak over each
/// point's span; where a span holds no bin, interpolated in dB between the
/// bins around it (`interpolate`) or the floor.
fn plot_points(bin_db: &[f64], rate: f64, bin_hz: f64, interpolate: bool) -> Vec<f32> {
    let half = bin_db.len() - 1;
    let freqs = plot_freqs(rate);
    let ratio = (freqs[1] / freqs[0]).sqrt();
    freqs
        .iter()
        .map(|&f| {
            let lo = ((f / ratio) / bin_hz).ceil() as usize;
            let hi = (((f * ratio) / bin_hz).ceil() as usize).min(half + 1);
            // Spans tile the axis ([f / r, f r) each): a bin lands on one point.
            let v = if lo < hi {
                bin_db[lo..hi]
                    .iter()
                    .copied()
                    .fold(f64::NEG_INFINITY, f64::max)
            } else if interpolate {
                let x = (f / bin_hz).min(half as f64);
                let j = (x.floor() as usize).min(half - 1);
                let t = x - j as f64;
                bin_db[j] * (1.0 - t) + bin_db[j + 1] * t
            } else {
                PLOT_FLOOR_DB
            };
            ((v * 100.0).round() / 100.0) as f32
        })
        .collect()
}

/// Measure one line's steady-state window and tail `sim` (n + [`TAIL`]).
fn analyse(sim: &[f64], rate: f64, plan: Plan) -> Raw {
    let n = plan.n;
    let (out, tail) = sim.split_at(n);
    let half = n / 2;
    let bin_hz = rate / n as f64;
    let rect = fft(out);
    let h1 = rect[plan.bin].norm_sqr();
    let out_amp = 2.0 * h1.sqrt() / n as f64;
    let gain_db = 20.0 * (out_amp / plan.amp).log10();
    // Every bin is one order (module docs): sum by where it lands.
    let cinv = inverse_mod_pow2(plan.bin, n);
    let (mut thd, mut band) = (0.0, 0.0);
    for (j, x) in rect.iter().enumerate().take(half + 1).skip(1) {
        if j == plan.bin {
            continue;
        }
        let p = x.norm_sqr() / h1;
        let f = j as f64 * bin_hz;
        if (BAND_HZ.0..=BAND_HZ.1).contains(&f) {
            band += p;
        }
        let k = (j * cinv) % n;
        let k = k.min(n - k);
        if k * plan.bin <= half {
            thd += p;
        }
    }
    let peaks = (2..=MAX_LABEL_ORDER)
        .filter_map(|k| {
            let j = plan.bin_of(k);
            let level_db = db10(rect[j].norm_sqr() / h1, f64::NEG_INFINITY);
            (level_db > LABEL_FLOOR_DB).then_some(HarmonicsSpectrumPeak {
                order: k as u32,
                freq_hz: j as f64 * bin_hz,
                level_db,
                aliased: k * plan.bin > half,
            })
        })
        .collect();
    // The plot, exact: every bin is one order.
    let rect_db: Vec<f64> = rect
        .iter()
        .map(|x| db10(x.norm_sqr() / h1, PLOT_FLOOR_DB))
        .collect();
    let exact_db = plot_points(&rect_db, rate, bin_hz, false);
    // Windowed, for a line that is not quite periodic.
    let w = bh7(n);
    let windowed: Vec<f64> = out.iter().zip(&w).map(|(x, w)| x * w).collect();
    let spec = fft(&windowed);
    let ref_p = spec[plan.bin].norm_sqr();
    let bin_db: Vec<f64> = spec
        .iter()
        .map(|x| db10(x.norm_sqr() / ref_p, PLOT_FLOOR_DB))
        .collect();
    let windowed_db = plot_points(&bin_db, rate, bin_hz, true);
    // The jump from the window's end to its start (module docs).
    let jump = tail
        .iter()
        .zip(out)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    let leak_db = 20.0 * (jump / (PI * out_amp)).log10();
    // One cycle: sample i has phase (i c mod n) / n, each phase once.
    let mut cycle = vec![0.0; n];
    for (i, &y) in out.iter().enumerate() {
        cycle[(i * plan.bin) % n] = y;
    }
    let wave = (0..WAVE_POINTS)
        .map(|m| cycle[m * n / WAVE_POINTS] as f32)
        .collect();
    Raw {
        gain_db,
        thd,
        band,
        out_amp,
        exact_db,
        windowed_db,
        leak_db: if leak_db.is_finite() { leak_db } else { -400.0 },
        peaks,
        wave,
    }
}

/// A line by its config's parameters, the rate and the tone.
type Key = (String, [u64; 3]);
static CACHE: Mutex<Lru<Key, Arc<Raw>>> = Mutex::new(Lru::new(CACHE_ENTRIES));

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn key(config: &HarmonicsConfig, rate: f64, plan: Plan) -> Key {
    let layers: Vec<_> = config.layers.iter().map(unlabeled).collect();
    (
        format!(
            "{} {:?} dc_block={} {layers:?}",
            config.enabled, config.routing, config.dc_block
        ),
        [rate.to_bits(), plan.freq.to_bits(), plan.amp.to_bits()],
    )
}

/// The output word's noise power (rounding plus the dither's unshaped
/// variance), full scale ±1.
fn word_noise(word: &HarmonicsSpectrumWord) -> f64 {
    let lsb = 2f64.powi(1 - word.bits.clamp(1, 64) as i32);
    lsb * lsb * (1.0 / 12.0 + audivane_protocol::dither_variance_lsb2(word.dither))
}

/// The line with the output word applied.
fn finish(
    raw: &Raw,
    trace: HarmonicsSpectrumTrace,
    rate: f64,
    plan: Plan,
    word: &HarmonicsSpectrumWord,
) -> HarmonicsSpectrumLine {
    let noise = word_noise(word);
    let fund = raw.out_amp * crate::db_to_lin(-word.headroom_db.max(0.0));
    let fund_p = 0.5 * fund * fund;
    let band_frac = ((BAND_HZ.1.min(0.5 * rate) - BAND_HZ.0) / (0.5 * rate)).max(0.0);
    // A partial of peak A gives |X| = A n a0 / 2 (a0 = 1 unwindowed); white
    // noise of variance σ² gives E|X|² = σ² Σw² (n unwindowed).
    let n = plan.n as f64;
    let floor = |a0: f64, sum_w2: f64| {
        db10(
            noise * sum_w2 / (fund * n * a0 / 2.0).powi(2),
            PLOT_FLOOR_DB,
        )
    };
    let exact_floor = floor(1.0, n);
    let windowed = raw.leak_db > exact_floor - EXACT_MARGIN_DB;
    let floor_db = if windowed {
        floor(
            BH7[0],
            n * (BH7[0] * BH7[0] + 0.5 * BH7[1..].iter().map(|a| a * a).sum::<f64>()),
        )
    } else {
        exact_floor
    };
    HarmonicsSpectrumLine {
        trace,
        gain_db: raw.gain_db,
        thd_db: db10(raw.thd, FLOOR_DB),
        thd_n_db: db10(raw.band + noise * band_frac / fund_p, FLOOR_DB),
        floor_db,
        windowed,
        levels_db: if windowed {
            raw.windowed_db.clone()
        } else {
            raw.exact_db.clone()
        },
        peaks: raw.peaks.clone(),
        wave: raw.wave.clone(),
    }
}

/// The distortion spectrum of `config` at `output_rate` for a sine at
/// `freq_hz` and `level_dbfs`, one line per trace (empty: the whole stage),
/// with the floor and THD+N of `word` (module docs; cached per line).
/// Off the audio thread (allocates; milliseconds without tape layers, about
/// a tape layer's real-time factor times 0.6–1.2 s of audio with them).
pub fn spectrum(
    config: &HarmonicsConfig,
    output_rate: f64,
    freq_hz: f64,
    level_dbfs: f64,
    traces: &[HarmonicsSpectrumTrace],
    word: HarmonicsSpectrumWord,
) -> Result<HarmonicsSpectrum, HarmonicsError> {
    if !output_rate.is_finite() || output_rate <= 0.0 {
        return Err(HarmonicsError::InvalidSampleRate);
    }
    let top = MAX_TONE_FRACTION * output_rate;
    if !(MIN_TONE_HZ..=top).contains(&freq_hz) {
        return Err(HarmonicsError::InvalidLevel(format!(
            "freq_hz must be in {MIN_TONE_HZ}..={top} (got {freq_hz})"
        )));
    }
    if !(MIN_LEVEL_DBFS..=0.0).contains(&level_dbfs) {
        return Err(HarmonicsError::InvalidLevel(format!(
            "level_dbfs must be in {MIN_LEVEL_DBFS}..=0 (got {level_dbfs})"
        )));
    }
    if traces.len() > MAX_TRACES {
        return Err(HarmonicsError::InvalidLevel(format!(
            "at most {MAX_TRACES} traces"
        )));
    }
    let traces = if traces.is_empty() {
        &[HarmonicsSpectrumTrace::Stage][..]
    } else {
        traces
    };
    let plan = Plan::at(output_rate, freq_hz, level_dbfs);
    let configs = traces
        .iter()
        .map(|&t| trace_config(config, t))
        .collect::<Result<Vec<_>, _>>()?;
    let keys: Vec<Key> = configs.iter().map(|c| key(c, output_rate, plan)).collect();
    let mut raws: Vec<Option<Arc<Raw>>> = keys
        .iter()
        .map(|k| lock(&CACHE).get_mut(k).cloned())
        .collect();
    // The missing lines (each config once), on up to MAX_THREADS threads.
    let mut todo: Vec<usize> = Vec::new();
    for (i, r) in raws.iter().enumerate() {
        if r.is_none() && !todo.iter().any(|&j| keys[j] == keys[i]) {
            todo.push(i);
        }
    }
    for chunk in todo.chunks(MAX_THREADS) {
        let done: Vec<Result<Raw, HarmonicsError>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|&i| {
                    let cfg = &configs[i];
                    s.spawn(move || {
                        simulate(cfg, output_rate, plan).map(|out| analyse(&out, output_rate, plan))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
                .collect()
        });
        for (&i, raw) in chunk.iter().zip(done) {
            let raw = Arc::new(raw?);
            lock(&CACHE).get_or_insert_with(keys[i].clone(), || raw.clone());
            raws[i] = Some(raw);
        }
    }
    let lines = traces
        .iter()
        .zip(&keys)
        .enumerate()
        .map(|(i, (&t, k))| {
            let raw = raws[i].clone().unwrap_or_else(|| {
                let j = keys.iter().position(|kj| kj == k).expect("own key");
                raws[j].clone().expect("simulated above")
            });
            finish(&raw, t, output_rate, plan, &word)
        })
        .collect();
    Ok(HarmonicsSpectrum {
        rate_hz: output_rate,
        freq_hz: plan.freq,
        level_dbfs,
        bin_hz: output_rate / plan.n as f64,
        word,
        freqs_hz: plot_freqs(output_rate)
            .into_iter()
            .map(|f| f as f32)
            .collect(),
        lines,
    })
}

/// Empty the cache.
pub(crate) fn clear_cache() {
    lock(&CACHE).clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use audivane_protocol::{DitherKind, HarmonicsRouting};

    const WORD: HarmonicsSpectrumWord = HarmonicsSpectrumWord {
        bits: 24,
        dither: DitherKind::Tpdf,
        headroom_db: 3.0,
    };

    fn layer(id: &str) -> HarmonicsLayer {
        HarmonicsConfig::from_preset(id).unwrap().layers[0].clone()
    }

    #[test]
    fn the_plan_is_coherent_on_an_odd_bin() {
        for rate in [44_100.0, 96_000.0, 768_000.0] {
            for f in [10.0, 20.0, 1_000.0, 19_997.0] {
                let p = Plan::at(rate, f, 0.0);
                assert!(p.n.is_power_of_two());
                assert_eq!(p.bin % 2, 1);
                assert!(p.bin >= 5, "{rate} {f} {p:?}");
                assert!((p.freq - f).abs() <= 2.0 * rate / p.n as f64);
                let inv = inverse_mod_pow2(p.bin, p.n);
                assert_eq!(p.bin * inv % p.n, 1);
            }
        }
    }

    #[test]
    fn chebyshev_peaks_match_the_readout() {
        let rate = 96_000.0;
        let cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
        let s = spectrum(&cfg, rate, 1_000.0, 0.0, &[], WORD).unwrap();
        let r = crate::harmonics::readout(&cfg, rate, false).unwrap();
        let line = &s.lines[0];
        // The readout's 1 kHz 0 dBFS point (the spectrum's tone is a few Hz
        // off 1 kHz; the curve is flat, so the levels match).
        let pt = &r.points[0];
        for k in 2..=5 {
            let peak = line.peaks.iter().find(|p| p.order == k).unwrap();
            assert!(!peak.aliased);
            assert!((peak.freq_hz - k as f64 * s.freq_hz).abs() < 1e-6);
            assert!(
                (peak.level_db - pt.harmonics_db[k as usize - 2]).abs() < 0.01,
                "H{k}: {} vs {}",
                peak.level_db,
                pt.harmonics_db[k as usize - 2]
            );
        }
        assert!((line.thd_db - pt.thd_db).abs() < 0.01);
        assert!((line.gain_db - pt.gain_db).abs() < 0.01);
        // Below the 24-bit floor nothing is lost: THD+N is at least THD.
        assert!(line.thd_n_db >= line.thd_db - 0.01);
        assert!(line.floor_db < -150.0 && line.floor_db > -220.0);
        // The plot peaks at the fundamental (0 dBr) and at H2.
        let at = |f: f64| {
            let i = s.freqs_hz.iter().position(|&x| f64::from(x) >= f).unwrap();
            line.levels_db[i - 1].max(line.levels_db[i])
        };
        assert!(at(s.freq_hz).abs() < 0.1);
        assert!((f64::from(at(2.0 * s.freq_hz)) - pt.harmonics_db[0]).abs() < 0.5);
        assert_eq!(line.wave.len(), WAVE_POINTS);
    }

    #[test]
    fn periodic_lines_plot_exact_hairlines() {
        // A sag system: its envelope settles to a periodic steady state, so
        // the plot is exact; the stage's residual DC stays in bin 0 (no skirt
        // at 10-60 Hz).
        let rate = 705_600.0;
        for level in [-10.0, 0.0] {
            let cfg = audivane_protocol::harmonics_system("set_amp")
                .unwrap()
                .to_config();
            let s = spectrum(&cfg, rate, 1_000.0, level, &[], WORD).unwrap();
            let line = &s.lines[0];
            assert!(!line.windowed, "{level}");
            for (f, l) in s.freqs_hz.iter().zip(&line.levels_db) {
                if (12.0..60.0).contains(f) {
                    assert!(f64::from(*l) < line.floor_db, "{f} Hz: {l}");
                }
            }
            // The fundamental is one point, its neighbours far below.
            let i = s
                .freqs_hz
                .iter()
                .position(|&x| f64::from(x) * (1.0 + 1e-3) >= s.freq_hz)
                .unwrap();
            let top = (i - 1..=i + 1)
                .max_by(|&a, &b| line.levels_db[a].total_cmp(&line.levels_db[b]))
                .unwrap();
            assert!(line.levels_db[top].abs() < 0.01);
            assert!(line.levels_db[top - 2] < -150.0 && line.levels_db[top + 2] < -150.0);
        }
        // A line whose window doesn't repeat falls back to the window, with
        // its (wider) floor.
        let cfg = HarmonicsConfig::from_preset("se_triode_high_mu").unwrap();
        let plan = Plan::at(rate, 1_000.0, 0.0);
        let mut raw = analyse(&simulate(&cfg, rate, plan).unwrap(), rate, plan);
        let exact = finish(&raw, HarmonicsSpectrumTrace::Stage, rate, plan, &WORD);
        assert!(!exact.windowed);
        raw.leak_db = -120.0;
        let windowed = finish(&raw, HarmonicsSpectrumTrace::Stage, rate, plan, &WORD);
        assert!(windowed.windowed);
        assert_eq!(windowed.levels_db, raw.windowed_db);
        assert!(windowed.floor_db > exact.floor_db + 3.0);
    }

    #[test]
    fn aliases_fold_and_series_traces_run() {
        let rate = 48_000.0;
        let mut cfg = HarmonicsConfig::from_preset("diff_pair").unwrap();
        cfg.layers.push(layer("se_triode_high_mu"));
        cfg.routing = HarmonicsRouting::Series;
        let traces = [
            HarmonicsSpectrumTrace::Stage,
            HarmonicsSpectrumTrace::Layer { layer: 0 },
            HarmonicsSpectrumTrace::Layer { layer: 1 },
            HarmonicsSpectrumTrace::After { layer: 0 },
        ];
        let s = spectrum(&cfg, rate, 10_000.0, 0.0, &traces, WORD).unwrap();
        assert_eq!(s.lines.len(), 4);
        // After the first layer is the first layer alone.
        assert_eq!(s.lines[1].levels_db, s.lines[3].levels_db);
        let h3 = s.lines[0].peaks.iter().find(|p| p.order == 3).unwrap();
        assert!(h3.aliased);
        assert!((h3.freq_hz - (rate - 3.0 * s.freq_hz)).abs() < 1e-6);
        // Aliases count in THD+N, not in THD.
        assert!(s.lines[0].thd_n_db > s.lines[0].thd_db);
        // `after` needs series.
        cfg.routing = HarmonicsRouting::Parallel;
        assert!(spectrum(&cfg, rate, 1_000.0, 0.0, &traces[3..], WORD).is_err());
    }

    #[test]
    fn a_tape_layer_is_simulated() {
        let rate = 96_000.0;
        let cfg = HarmonicsConfig::from_preset("tape_master").unwrap();
        let s = spectrum(&cfg, rate, 1_000.0, 0.0, &[], WORD).unwrap();
        let r = crate::harmonics::readout(&cfg, rate, false).unwrap();
        let h3 = s.lines[0].peaks.iter().find(|p| p.order == 3).unwrap();
        assert!(
            (h3.level_db - r.points[0].harmonics_db[1]).abs() < 0.5,
            "{} vs {}",
            h3.level_db,
            r.points[0].harmonics_db[1]
        );
    }

    #[test]
    fn off_is_a_clean_tone() {
        let cfg = HarmonicsConfig {
            enabled: false,
            ..HarmonicsConfig::from_preset("triode").unwrap()
        };
        let s = spectrum(&cfg, 96_000.0, 1_000.0, -20.0, &[], WORD).unwrap();
        let line = &s.lines[0];
        assert!(line.peaks.is_empty());
        assert!(line.gain_db.abs() < 1e-9);
        assert!(line.thd_db <= FLOOR_DB);
        // THD+N is the word's noise: 24-bit TPDF, 20 kHz of 48, a −20 dBFS
        // tone 3 dB down: about −(146 − 23 − 3.8 + 4.8) ≈ −124 dB.
        assert!(
            line.thd_n_db > -130.0 && line.thd_n_db < -118.0,
            "{}",
            line.thd_n_db
        );
    }
}
