//! Fast simulated readouts of the harmonics stage: separable tones, a
//! process-wide cache and a few worker threads. Off the audio thread.
//!
//! # Separable tones
//!
//! The stage is a parallel sum: its output is the bank's (plus the flux
//! layers', all with a closed-form steady state) plus `s ×` each slow
//! layer's path, and each path sees only the clamped input. So a steady tone
//! is the closed form of the rest ([`crate::harmonics::Harmonics::steady_tone`],
//! at the real rate) plus `s ×` the coherent DFT of each slow **unit**'s path
//! simulated alone ([`SimUnit`]): one tape (hysteresis) layer, with its path
//! clamps, or the whole sag/bias supply (its layers share one drive, so they
//! are simulated together). The DFT is linear, so the sum is the whole
//! stage's tone up to rounding and the closed form's exactness (checked
//! against simulating the whole stage within 0.001 dB). A stage whose layers
//! are not a parallel sum (series layers) is not separable: it is simulated
//! whole, as one unit with no closed-form part.
//!
//! The A/B level change ([`crate::harmonics::Harmonics::loudness_change_db`])
//! separates the same way: the reference through the stage without its slow
//! units, plus `s ×` each unit's path on the reference (exact up to the
//! order of the additions).
//!
//! # Cache
//!
//! A unit's tones and level-change path depend only on its own layers' parameters,
//! the stage's `dc_block`, the simulation rate and the tone, so they are
//! cached process-wide by those ([`unit_key`] plus the rate bits); editing
//! another layer then costs closed-form time. Each slow layer's built core
//! (its calibration and probes, ~80 ms for a tape layer) is cached the same
//! way ([`cached_core`]). A tape layer's `amount` is not a pure gain on its
//! path (the `|x_c + p| <= Q` clamp does not scale with it), so it is part of
//! the key like every other parameter. Memory is bounded: at most
//! [`TONE_UNITS`] units' tones (each at most a few hundred tones of 200
//! bytes: under 2 MB in all), [`LEVEL_UNITS`] level-change paths (4.5 s at
//! 48 kHz, 1.7 MB each: 10.4 MB) and [`CORE_ENTRIES`] built cores (under
//! 1 KB each); the least recently used entry goes first.
//!
//! # Threads
//!
//! A readout uses at most [`MAX_THREADS`] scoped threads, its caller's
//! included (playback keeps its cores; the daemon runs one readout at a
//! time, latest wins): first for the slow layers' cores at the readout's
//! three rates, then one runs the level change (its paths one after the
//! other) while the others simulate the tones.

use crate::harmonics::ToneSpectrum;
use crate::harmonics_layer::{LayerGroup, SlowCore};
use audivane_protocol::{HarmonicsConfig, HarmonicsLayer};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Most worker threads a readout uses.
pub(crate) const MAX_THREADS: usize = 4;
/// Units whose tones the cache keeps.
pub(crate) const TONE_UNITS: usize = 32;
/// Units whose level-change path the cache keeps.
pub(crate) const LEVEL_UNITS: usize = 6;
/// Built slow-layer cores the cache keeps.
pub(crate) const CORE_ENTRIES: usize = 32;

/// A slow part of the stage a readout simulates on its own: one tape layer,
/// or the sag/bias supply (single channel, at rest).
#[derive(Clone, Debug)]
pub(crate) struct SimUnit {
    /// Its layers' indices in the config.
    pub(crate) indices: Vec<usize>,
    pub(crate) group: LayerGroup,
}

/// The cache key of the unit made of `config.layers[i]` for `indices`: the
/// layers' parameters (without their preset label, which changes nothing)
/// and the stage's `dc_block`.
pub(crate) fn unit_key(config: &HarmonicsConfig, indices: &[usize]) -> String {
    let layers: Vec<HarmonicsLayer> = indices
        .iter()
        .filter_map(|&i| config.layers.get(i))
        .map(unlabeled)
        .collect();
    format!("{layers:?} dc_block={}", config.dc_block)
}

/// `layer` without its preset label.
pub(crate) fn unlabeled(layer: &HarmonicsLayer) -> HarmonicsLayer {
    let mut l = layer.clone();
    match &mut l {
        HarmonicsLayer::Chebyshev(c) => c.preset = None,
        HarmonicsLayer::Flux(f) => f.preset = None,
        HarmonicsLayer::Hysteresis(h) => h.preset = None,
    }
    l
}

/// One simulated tone: its frequency, peak and plan lengths (bits).
pub(crate) type ToneKey = [u64; 4];

pub(crate) fn tone_key(freq: f64, amp: f64, settle_s: f64, measure_s: f64) -> ToneKey {
    [
        freq.to_bits(),
        amp.to_bits(),
        settle_s.to_bits(),
        measure_s.to_bits(),
    ]
}

/// A unit at a simulation rate.
type UnitKey = (String, u64);

/// A small least-recently-used map (linear scans: a few dozen entries).
#[derive(Debug)]
pub(crate) struct Lru<K, V> {
    cap: usize,
    clock: u64,
    entries: Vec<(K, V, u64)>,
}

impl<K: PartialEq, V> Lru<K, V> {
    pub(crate) const fn new(cap: usize) -> Self {
        Self {
            cap,
            clock: 0,
            entries: Vec::new(),
        }
    }

    /// The entry for `key`, marked as just used.
    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.iter_mut().find(|e| e.0 == *key).map(|e| {
            e.2 = clock;
            &mut e.1
        })
    }

    /// The entry for `key`, inserted with `make` (evicting the least
    /// recently used at capacity) when absent.
    pub(crate) fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        self.clock += 1;
        let clock = self.clock;
        let at = match self.entries.iter().position(|e| e.0 == key) {
            Some(i) => i,
            None => {
                if self.entries.len() >= self.cap
                    && let Some(lru) = (0..self.entries.len()).min_by_key(|&i| self.entries[i].2)
                {
                    self.entries.swap_remove(lru);
                }
                self.entries.push((key, make(), clock));
                self.entries.len() - 1
            }
        };
        let e = &mut self.entries[at];
        e.2 = clock;
        &mut e.1
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Simulated readout results by unit: tones and level-change paths.
#[derive(Debug)]
pub(crate) struct ReadoutCache {
    tones: Mutex<Lru<UnitKey, HashMap<ToneKey, ToneSpectrum>>>,
    levels: Mutex<Lru<UnitKey, Arc<Vec<f64>>>>,
    /// Tones and paths found / simulated (for tests).
    hits: AtomicUsize,
    misses: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ReadoutCache {
    pub(crate) const fn new() -> Self {
        Self {
            tones: Mutex::new(Lru::new(TONE_UNITS)),
            levels: Mutex::new(Lru::new(LEVEL_UNITS)),
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
        }
    }

    pub(crate) fn clear(&self) {
        lock(&self.tones).clear();
        lock(&self.levels).clear();
    }

    /// `requests[i] = (unit, tone)`: each unit's tone (its path alone, no
    /// output scale) from the cache, the missing ones simulated with
    /// `simulate` on the worker threads and cached. `keys[u]` is unit `u`'s
    /// key at `rate`.
    pub(crate) fn tones(
        &self,
        keys: &[String],
        rate: f64,
        threads: usize,
        requests: &[(usize, ToneKey)],
        simulate: impl Fn(usize, ToneKey) -> ToneSpectrum + Sync,
    ) -> Vec<ToneSpectrum> {
        let rate_bits = rate.to_bits();
        let unit = |u: usize| (keys[u].clone(), rate_bits);
        let mut out: Vec<Option<ToneSpectrum>> = vec![None; requests.len()];
        let mut missing: Vec<(usize, ToneKey)> = Vec::new();
        {
            let mut cache = lock(&self.tones);
            for (slot, &(u, tone)) in out.iter_mut().zip(requests) {
                *slot = cache.get_mut(&unit(u)).and_then(|m| m.get(&tone).copied());
                if slot.is_none() && !missing.contains(&(u, tone)) {
                    missing.push((u, tone));
                }
            }
        }
        let found = out.iter().filter(|s| s.is_some()).count();
        self.hits.fetch_add(found, Ordering::Relaxed);
        self.misses.fetch_add(missing.len(), Ordering::Relaxed);
        let simulated = par_map(&missing, threads, |&(u, tone)| simulate(u, tone));
        let mut cache = lock(&self.tones);
        for (&(u, tone), h) in missing.iter().zip(&simulated) {
            cache
                .get_or_insert_with(unit(u), HashMap::new)
                .insert(tone, *h);
        }
        out.into_iter()
            .zip(requests)
            .map(|(slot, req)| {
                slot.unwrap_or_else(|| {
                    let i = missing.iter().position(|m| m == req).expect("simulated");
                    simulated[i]
                })
            })
            .collect()
    }

    /// Each unit's level-change path (`keys[u]` at `rate`), from the cache
    /// or simulated with `simulate` on the worker threads and cached.
    pub(crate) fn paths(
        &self,
        keys: &[String],
        rate: f64,
        threads: usize,
        simulate: impl Fn(usize) -> Vec<f64> + Sync,
    ) -> Vec<Arc<Vec<f64>>> {
        let rate_bits = rate.to_bits();
        let mut out: Vec<Option<Arc<Vec<f64>>>> = {
            let mut cache = lock(&self.levels);
            keys.iter()
                .map(|k| cache.get_mut(&(k.clone(), rate_bits)).cloned())
                .collect()
        };
        let missing: Vec<usize> = (0..keys.len()).filter(|&u| out[u].is_none()).collect();
        self.hits
            .fetch_add(keys.len() - missing.len(), Ordering::Relaxed);
        self.misses.fetch_add(missing.len(), Ordering::Relaxed);
        let simulated = par_map(&missing, threads, |&u| Arc::new(simulate(u)));
        let mut cache = lock(&self.levels);
        for (&u, p) in missing.iter().zip(simulated) {
            *cache.get_or_insert_with((keys[u].clone(), rate_bits), || p.clone()) = p.clone();
            out[u] = Some(p);
        }
        out.into_iter().map(|p| p.expect("simulated")).collect()
    }
}

/// The process-wide readout cache.
pub(crate) static READOUT_CACHE: ReadoutCache = ReadoutCache::new();

/// A built slow-layer core (what [`crate::harmonics_layer::build_layer`]
/// calibrates), by layer, rate and `dc_block`.
type CoreKey = (String, u64, bool);
static CORES: Mutex<Lru<CoreKey, SlowCore>> = Mutex::new(Lru::new(CORE_ENTRIES));

/// The layer's built core from the core cache, or built with `build` and
/// cached (errors are not cached). Building is deterministic, so a cached
/// core is the one `build` would make.
pub(crate) fn cached_core<E>(
    layer: &HarmonicsLayer,
    rate: f64,
    dc_block: bool,
    build: impl FnOnce() -> Result<SlowCore, E>,
) -> Result<SlowCore, E> {
    let key = (format!("{:?}", unlabeled(layer)), rate.to_bits(), dc_block);
    if let Some(core) = lock(&CORES).get_mut(&key) {
        return Ok(core.clone());
    }
    let core = build()?;
    lock(&CORES).get_or_insert_with(key, || core.clone());
    Ok(core)
}

/// Empty every readout cache (the tones, paths, aliasing measurements,
/// spectra and built cores).
pub fn clear_readout_caches() {
    READOUT_CACHE.clear();
    crate::harmonics_alias::clear_cache();
    crate::harmonics_spectrum::clear_cache();
    lock(&CORES).clear();
}

/// Entries in the core cache (for tests).
#[cfg(test)]
pub(crate) fn cached_cores() -> usize {
    lock(&CORES).len()
}

/// `f` over `items` on up to `threads` scoped threads, the caller's included
/// (at most the machine's cores; results in order).
pub(crate) fn par_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(threads)
        .min(items.len());
    if threads <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let work = || {
        let mut done = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(item) = items.get(i) else {
                return done;
            };
            done.push((i, f(item)));
        }
    };
    let mut out: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(work)).collect();
        for h in handles {
            match h.join() {
                Ok(done) => {
                    for (i, r) in done {
                        out[i] = Some(r);
                    }
                }
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    out.into_iter()
        .map(|r| r.expect("every item is mapped"))
        .collect()
}
