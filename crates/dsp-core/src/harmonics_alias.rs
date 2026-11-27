//! Measured aliasing of the harmonics stage, for the readout's warning.
//! Off the audio thread.
//!
//! The stage runs at the output rate with no oversampling of its own, so
//! every partial above Nyquist folds back. Which orders are present and how
//! loud is not knowable from the curves' degrees alone (series cross terms
//! reach the product of the degrees but at the product of the levels; tape
//! and flux layers have no degree at all), so the aliasing is measured: a
//! full-scale sine near the top of the audio band runs through the whole
//! stage at the real output rate, and everything that lands in
//! 20 Hz..=20 kHz other than the tone is aliasing (its own harmonics all lie
//! above 20 kHz).
//!
//! # Probe
//!
//! The tone is 0 dBFS at about 20 kHz ([`ALIAS_TONE_HZ`]): the worst case
//! music can present (the highest frequency the band carries, at the
//! highest level). It is made coherent with a power-of-two window of `n`
//! samples (bins of at most 25 Hz) on an odd bin `c`: the steady
//! state repeats every `n` samples, so every partial, folded or not, lands
//! exactly on a bin (rectangular window, no leakage), and with `c` odd and
//! `n` a power of two no order `k > 1` ever folds onto the tone itself
//! (`(k ± 1) c ≡ 0 mod n` has no solution below `n`).
//!
//! # Measurement
//!
//! A parallel stage of Chebyshev layers only (tilt included) is a
//! polynomial of degree at most 9 through linear filters, so its steady
//! state is exact in closed form ([`Harmonics::steady_tone`], every order up
//! to H15 at its folded frequency). Any other stage (series, flux, tape,
//! sag/bias) is simulated at the output rate, settled
//! ([`READOUT_SETTLE_SECONDS`] or more, envelopes presettled), and the
//! window's spectrum taken with one real FFT. A series stage is simulated
//! whole; a parallel one is separable as its readout tones
//! ([`crate::harmonics_sim`]): the rest of the stage (bank, flux) simulated
//! whole, which is cheap, plus `s ×` each slow unit's (tape layer's,
//! sag/bias supply's) path spectrum, cached by the unit's parameters and the
//! rate, so editing one layer re-simulates only that one. Every result is
//! also cached by the whole stage's parameters and the rate.
//!
//! # Threshold
//!
//! The readout warns above [`ALIAS_WARN_DB`] (-60 dBc, the power sum of
//! every alias). The probe is far hotter than music carries at 20 kHz
//! (cymbals sit 20 dB or more below full scale, and an alias of order `k`
//! drops by about `k` times that), so -60 dBc on the probe is already well
//! under audibility on music. Above it the aliasing is a product worth
//! acting on (a higher output rate); below it the measured level is still
//! reported, but a warning would only nag (a hot tape layer measures about
//! -80 dBc even at 768 kHz).

use crate::harmonics::{Harmonics, HarmonicsError, READOUT_SETTLE_SECONDS};
use crate::harmonics_layer::AUDIO_BAND_HZ;
use crate::harmonics_sim::{Lru, unit_key, unlabeled};
use crate::{Complex64, Stage, lin_to_db};
use audivane_protocol::HarmonicsConfig;
use audivane_protocol::ipc::HarmonicsAliasing;
use std::f64::consts::PI;
use std::sync::{Arc, Mutex, MutexGuard};

/// The probe tone's target frequency, Hz (made coherent just below it).
pub const ALIAS_TONE_HZ: f64 = 20_000.0;
/// The readout warns when the aliases' power sum is above this, dBc.
pub const ALIAS_WARN_DB: f64 = -60.0;
/// Lowest frequency of the audio band, Hz.
const BAND_LOW_HZ: f64 = 20.0;
/// Widest bin of the probe's window, Hz.
const MAX_BIN_HZ: f64 = 25.0;
/// Floor of the levels, dB (no alias).
const FLOOR_DB: f64 = -200.0;
/// Measurements the cache keeps.
const CACHE_ENTRIES: usize = 32;
/// Slow units' path spectra the cache keeps.
const UNIT_ENTRIES: usize = 16;

/// The coherent probe at a rate: window length, tone bin and frequency.
#[derive(Clone, Copy, Debug)]
struct Probe {
    n: usize,
    bin: usize,
    freq: f64,
}

impl Probe {
    fn at(rate: f64) -> Self {
        let n = ((rate / MAX_BIN_HZ).ceil() as usize).next_power_of_two();
        let mut bin = (ALIAS_TONE_HZ * n as f64 / rate).floor() as usize;
        if bin.is_multiple_of(2) {
            bin -= 1;
        }
        Self {
            n,
            bin,
            freq: bin as f64 * rate / n as f64,
        }
    }

    /// Whether `hz` is in the audio band.
    fn in_band(hz: f64) -> bool {
        (BAND_LOW_HZ..=AUDIO_BAND_HZ).contains(&hz)
    }
}

/// Accumulates aliases (power relative to the fundamental's).
struct Sum {
    power: f64,
    worst: f64,
    worst_hz: f64,
}

impl Sum {
    fn new() -> Self {
        Self {
            power: 0.0,
            worst: 0.0,
            worst_hz: 0.0,
        }
    }

    fn add(&mut self, hz: f64, rel_power: f64) {
        self.power += rel_power;
        if rel_power > self.worst {
            self.worst = rel_power;
            self.worst_hz = hz;
        }
    }

    fn finish(self, tone_hz: f64) -> HarmonicsAliasing {
        let db = |p: f64| {
            let d = 0.5 * lin_to_db(p);
            if d.is_finite() {
                d.max(FLOOR_DB)
            } else {
                FLOOR_DB
            }
        };
        let level_db = db(self.power);
        let worst_db = db(self.worst);
        HarmonicsAliasing {
            tone_hz,
            level_db,
            worst_hz: if worst_db > FLOOR_DB {
                self.worst_hz
            } else {
                0.0
            },
            worst_db,
            warn: level_db > ALIAS_WARN_DB,
        }
    }
}

/// No aliasing (the stage off).
pub(crate) fn none(rate: f64) -> HarmonicsAliasing {
    Sum::new().finish(Probe::at(rate).freq)
}

/// The aliasing of `stage` (built from `config` at `rate`), measured
/// (module docs).
pub(crate) fn measure(config: &HarmonicsConfig, stage: &Harmonics, rate: f64) -> HarmonicsAliasing {
    let probe = Probe::at(rate);
    if !stage.is_enabled() {
        none(rate)
    } else if stage.alias_closed_form() {
        closed_form(stage, rate, probe)
    } else if stage.is_series() {
        let settle_s = stage.readout_settle_s();
        sum_spectrum(&whole(stage, rate, probe, settle_s), rate, probe)
    } else {
        separable(config, stage, rate, probe)
    }
}

/// A Chebyshev-only parallel stage: every order's folded frequency and
/// level from its exact steady state.
fn closed_form(stage: &Harmonics, rate: f64, probe: Probe) -> HarmonicsAliasing {
    let h = stage.steady_tone(rate, probe.freq, 1.0);
    let h1 = h[1].norm_sqr();
    let mut sum = Sum::new();
    for (k, hk) in h.iter().enumerate().skip(2) {
        if k as f64 * probe.freq <= 0.5 * rate {
            continue;
        }
        let f = (k as f64 * probe.freq).rem_euclid(rate);
        let f = f.min(rate - f);
        if Probe::in_band(f) {
            sum.add(f, hk.norm_sqr() / h1);
        }
    }
    sum.finish(probe.freq)
}

/// One period of the probe and the probe for `settle_s` (rounded up to
/// whole periods, so the measured window starts on the period's start)
/// then one window.
fn probe_signal(rate: f64, probe: Probe, settle_s: f64) -> (Vec<f64>, Vec<f64>, usize) {
    let Probe { n, bin, .. } = probe;
    let settle_s = settle_s.max(READOUT_SETTLE_SECONDS);
    let settle = ((settle_s * rate).ceil() as usize).div_ceil(n) * n;
    let period: Vec<f64> = (0..n)
        .map(|i| (2.0 * PI * ((bin * i) % n) as f64 / n as f64).cos())
        .collect();
    let buf = period.iter().copied().cycle().take(settle + n).collect();
    (period, buf, settle)
}

/// The spectrum of the last window of `out` (bins `0..=n/2`, unscaled).
fn spectrum(out: &[f64], n: usize) -> Vec<Complex64> {
    use realfft::RealFftPlanner;
    let mut tail = out[out.len() - n..].to_vec();
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(n);
    let mut spec = fft.make_output_vec();
    fft.process(&mut tail, &mut spec)
        .expect("FFT lengths match the plan");
    spec
}

/// `stage`'s steady-state spectrum on the probe, simulated whole at `rate`
/// (its envelopes presettled).
fn whole(stage: &Harmonics, rate: f64, probe: Probe, settle_s: f64) -> Vec<Complex64> {
    let mut stage = stage.clone();
    stage.reset();
    let (period, mut buf, _) = probe_signal(rate, probe, settle_s);
    if stage.dynamic_layers() > 0 {
        stage.presettle(&period);
    }
    stage.process(&mut [buf.as_mut_slice()]);
    spectrum(&buf, probe.n)
}

/// A parallel stage with slow units: separable as its tones
/// ([`crate::harmonics_sim`]), the rest (bank, flux) simulated whole (cheap)
/// plus `s ×` each slow unit's path, its spectrum from the unit cache.
fn separable(
    config: &HarmonicsConfig,
    stage: &Harmonics,
    rate: f64,
    probe: Probe,
) -> HarmonicsAliasing {
    let settle_s = stage.readout_settle_s();
    let mut spec = whole(&stage.without_simulated(), rate, probe, settle_s);
    let units = stage.sim_units();
    if !units.is_empty() {
        let (_, buf, _) = probe_signal(rate, probe, settle_s);
        let mut xc = vec![0.0; buf.len()];
        crate::harmonics::clamp_input(&buf, &mut xc);
        for unit in &units {
            let key = (unit_key(config, &unit.indices), rate.to_bits());
            let cached = lock(&UNITS).get_mut(&key).cloned();
            let path = cached.unwrap_or_else(|| {
                let p = Arc::new(spectrum(&unit.group.path(&xc, probe.n), probe.n));
                lock(&UNITS).get_or_insert_with(key, || p.clone());
                p
            });
            for (y, p) in spec.iter_mut().zip(path.iter()) {
                *y += p;
            }
        }
    }
    sum_spectrum(&spec, rate, probe)
}

/// The aliases in a probe's spectrum: every in-band bin but the tone's.
fn sum_spectrum(spec: &[Complex64], rate: f64, probe: Probe) -> HarmonicsAliasing {
    let h1 = spec[probe.bin].norm_sqr();
    let mut sum = Sum::new();
    for (j, x) in spec.iter().enumerate() {
        let f = j as f64 * rate / probe.n as f64;
        if j != probe.bin && Probe::in_band(f) {
            sum.add(f, x.norm_sqr() / h1);
        }
    }
    sum.finish(probe.freq)
}

/// A measurement or a unit's spectrum by parameters and rate.
type Key = (String, u64);
static CACHE: Mutex<Lru<Key, HarmonicsAliasing>> = Mutex::new(Lru::new(CACHE_ENTRIES));
/// Slow units' path spectra (at most 16 at 768 kHz, 260 KB each).
static UNITS: Mutex<Lru<Key, Arc<Vec<Complex64>>>> = Mutex::new(Lru::new(UNIT_ENTRIES));

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn key(config: &HarmonicsConfig, rate: f64) -> Key {
    let layers: Vec<_> = config.layers.iter().map(unlabeled).collect();
    (
        format!(
            "{} {:?} dc_block={} {layers:?}",
            config.enabled, config.routing, config.dc_block
        ),
        rate.to_bits(),
    )
}

/// [`measure`] of `stage` (built from `config` at `rate`), from the cache
/// when the stage has been measured before.
pub(crate) fn measure_cached(
    config: &HarmonicsConfig,
    stage: &Harmonics,
    rate: f64,
) -> HarmonicsAliasing {
    if !stage.is_enabled() || stage.alias_closed_form() {
        return measure(config, stage, rate);
    }
    let key = key(config, rate);
    if let Some(a) = lock(&CACHE).get_mut(&key) {
        return *a;
    }
    let a = measure(config, stage, rate);
    lock(&CACHE).get_or_insert_with(key, || a);
    a
}

/// The aliasing `config` adds at `output_rate`, measured (module docs;
/// cached). Off the audio thread (milliseconds in closed form or cached;
/// up to a few tenths of a second for tape layers at 768 kHz).
pub fn aliasing(
    config: &HarmonicsConfig,
    output_rate: f64,
) -> Result<HarmonicsAliasing, HarmonicsError> {
    let stage = Harmonics::try_new(config, output_rate, 1)?;
    Ok(measure_cached(config, &stage, output_rate))
}

/// Empty the caches.
pub(crate) fn clear_cache() {
    lock(&CACHE).clear();
    lock(&UNITS).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_is_coherent_on_an_odd_bin_near_20_khz() {
        for rate in [44_100.0, 48_000.0, 96_000.0, 192_000.0, 768_000.0] {
            let p = Probe::at(rate);
            assert!(p.n.is_power_of_two());
            assert_eq!(p.bin % 2, 1);
            assert!(rate / p.n as f64 <= MAX_BIN_HZ);
            assert!(p.freq <= ALIAS_TONE_HZ && p.freq > ALIAS_TONE_HZ - 2.0 * MAX_BIN_HZ);
        }
    }

    fn layer(id: &str) -> audivane_protocol::HarmonicsLayer {
        HarmonicsConfig::from_preset(id).unwrap().layers[0].clone()
    }

    #[test]
    fn the_separable_measurement_matches_the_whole_stage() {
        use audivane_protocol::{HarmonicsBias, HarmonicsLayer, HarmonicsSag};
        let rate = 96_000.0;
        let mut sag = layer("pp_pentode_feedback");
        if let HarmonicsLayer::Chebyshev(c) = &mut sag {
            c.sag = Some(HarmonicsSag {
                depth_db: 3.0,
                attack_ms: 20.0,
                release_ms: 200.0,
            });
            c.bias = Some(HarmonicsBias {
                depth: 0.1,
                attack_ms: 300.0,
                release_ms: 2_000.0,
            });
        }
        let cfg = HarmonicsConfig {
            layers: vec![
                layer("se_triode_high_mu"),
                layer("tape"),
                layer("tape_master"),
                layer("tape_vintage"),
                sag,
            ],
            ..HarmonicsConfig::from_preset("triode").unwrap()
        };
        let stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
        let probe = Probe::at(rate);
        let split = separable(&cfg, &stage, rate, probe);
        let all = sum_spectrum(
            &whole(&stage, rate, probe, stage.readout_settle_s()),
            rate,
            probe,
        );
        assert!(split.warn);
        assert!(
            (split.level_db - all.level_db).abs() < 0.01,
            "{split:?} {all:?}"
        );
        assert!(
            (split.worst_db - all.worst_db).abs() < 0.01,
            "{split:?} {all:?}"
        );
        assert_eq!(split.worst_hz, all.worst_hz);
        // Chebyshev layers alone: the closed form is the simulation's.
        let cfg = HarmonicsConfig::from_preset("diff_pair").unwrap();
        let stage = Harmonics::try_new(&cfg, rate, 1).unwrap();
        assert!(stage.alias_closed_form());
        let exact = closed_form(&stage, rate, probe);
        let sim = sum_spectrum(&whole(&stage, rate, probe, 0.0), rate, probe);
        assert!(
            (exact.level_db - sim.level_db).abs() < 0.01,
            "{exact:?} {sim:?}"
        );
        assert_eq!(exact.worst_hz, sim.worst_hz);
    }
}
