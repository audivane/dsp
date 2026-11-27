//! What the followers' drift loops learnt about each master and follower
//! pair, kept across runs: the follower's clock against the master's (per
//! rate family) and both devices' short-window reading bias. A follower
//! that opens on a pair it knows is placed from it at once (heard within a
//! fraction of a second), after a daemon restart, a reboot or an update
//! too.
//!
//! `$XDG_STATE_HOME/audivane-dsp/follower-clocks.json` (default
//! `~/.local/state/...`), written atomically when something moved, at most
//! every [`SAVE_EVERY_S`] and when a follower stops. A file that does not
//! parse is logged and ignored (the loops learn again).

use super::follower::ClockMemory;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The least time between two writes, seconds.
pub const SAVE_EVERY_S: f64 = 30.0;
/// A change of the rate smaller than this is not worth a write, ppm.
const PPM_STEP: f64 = 0.2;

/// `$XDG_STATE_HOME/audivane-dsp/follower-clocks.json`.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("audivane-dsp").join("follower-clocks.json"))
}

/// The two rate families' base rates, as the file names them.
const FAMILY_HZ: [u32; 2] = [48_000, 44_100];

#[derive(Debug, Serialize, Deserialize)]
struct FileFormat {
    version: u32,
    pairs: Vec<PairEntry>,
    /// The master's alignment with its followers, ms, by master device.
    #[serde(default)]
    alignments: BTreeMap<String, f64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PairEntry {
    master: String,
    follower: String,
    family_hz: u32,
    /// The follower's clock against the master's, ppm (the status's sign);
    /// absent: never locked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ppm: Option<f64>,
    #[serde(default)]
    master_bias_us: f64,
    #[serde(default)]
    follower_bias_us: f64,
}

/// The pairs' memories, by (master device, follower device).
#[derive(Debug, Default)]
pub struct ClockStore {
    path: Option<PathBuf>,
    pairs: BTreeMap<(String, String), [ClockMemory; 2]>,
    alignments: BTreeMap<String, f64>,
    dirty: bool,
    last_save: Option<Instant>,
}

fn moved(a: &ClockMemory, b: &ClockMemory) -> bool {
    let ppm = |m: &ClockMemory| m.eps.map(|e| e * 1e6);
    let rate_moved = match (ppm(a), ppm(b)) {
        (Some(x), Some(y)) => (x - y).abs() >= PPM_STEP,
        (None, None) => false,
        _ => true,
    };
    let bias_moved = |x: f64, y: f64| (x - y).abs() > 0.2 * x.max(y) + 0.1e-6;
    rate_moved
        || bias_moved(a.master_bias_s, b.master_bias_s)
        || bias_moved(a.follower_bias_s, b.follower_bias_s)
}

impl ClockStore {
    /// A store kept in memory only (tests, no state directory).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// The store at `path`, with what it holds (nothing when it is missing
    /// or does not parse).
    pub fn open(path: PathBuf) -> Self {
        let mut s = Self {
            path: Some(path),
            ..Self::default()
        };
        if let Some(p) = s.path.clone() {
            match std::fs::read_to_string(&p) {
                Ok(text) => match serde_json::from_str::<FileFormat>(&text) {
                    Ok(f) => s.load(f),
                    Err(e) => log::warn!("followers: {} ignored: {e}", p.display()),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("followers: {} not read: {e}", p.display()),
            }
        }
        s
    }

    fn load(&mut self, f: FileFormat) {
        self.alignments = f
            .alignments
            .into_iter()
            .filter(|(_, ms)| ms.is_finite() && *ms > 0.0)
            .collect();
        for e in f.pairs {
            let Some(fam) = FAMILY_HZ.iter().position(|&h| h == e.family_hz) else {
                continue;
            };
            let us = |x: f64| {
                if x.is_finite() && x >= 0.0 {
                    x * 1e-6
                } else {
                    0.0
                }
            };
            let eps = e
                .ppm
                .filter(|p| p.is_finite() && p.abs() < 2000.0)
                .map(|p| -p * 1e-6);
            let m = self.pairs.entry((e.master, e.follower)).or_default();
            m[fam] = ClockMemory {
                eps,
                master_bias_s: us(e.master_bias_us),
                follower_bias_s: us(e.follower_bias_us),
            };
        }
    }

    /// What is known of `master` → `follower` (both families).
    pub fn get(&self, master: &str, follower: &str) -> [ClockMemory; 2] {
        self.pairs
            .get(&(master.to_string(), follower.to_string()))
            .copied()
            .unwrap_or_default()
    }

    /// The master's alignment last time, ms.
    pub fn alignment(&self, master: &str) -> Option<f64> {
        self.alignments.get(master).copied()
    }

    /// The master's alignment now (0: none is kept).
    pub fn note_alignment(&mut self, master: &str, ms: f64) {
        if master.is_empty() || !ms.is_finite() || ms <= 0.0 {
            return;
        }
        if self.alignments.get(master) != Some(&ms) {
            self.alignments.insert(master.to_string(), ms);
            self.dirty = true;
        }
    }

    /// What a follower's loop knows now of its pair.
    pub fn note(&mut self, master: &str, follower: &str, mem: [ClockMemory; 2]) {
        if master.is_empty() || follower.is_empty() {
            return;
        }
        let key = (master.to_string(), follower.to_string());
        let old = self.pairs.get(&key).copied().unwrap_or_default();
        // An empty slot keeps what was known (a run that never locked
        // there).
        let mut new = old;
        for (n, m) in new.iter_mut().zip(mem) {
            if m.eps.is_some() {
                n.eps = m.eps;
            }
            if m.master_bias_s > 0.0 {
                n.master_bias_s = m.master_bias_s;
            }
            if m.follower_bias_s > 0.0 {
                n.follower_bias_s = m.follower_bias_s;
            }
        }
        if old.iter().zip(&new).any(|(a, b)| moved(a, b)) {
            self.pairs.insert(key, new);
            self.dirty = true;
        }
    }

    /// Write when something moved and the last write is [`SAVE_EVERY_S`]
    /// old (`force`: now, a follower stops).
    pub fn save(&mut self, force: bool) {
        let recent = self
            .last_save
            .is_some_and(|t| t.elapsed().as_secs_f64() < SAVE_EVERY_S);
        if !self.dirty || (!force && recent) {
            return;
        }
        self.dirty = false;
        self.last_save = Some(Instant::now());
        let Some(path) = self.path.clone() else {
            return;
        };
        if let Err(e) = self.write(&path) {
            log::warn!("followers: {} not written: {e}", path.display());
        }
    }

    fn write(&self, path: &Path) -> std::io::Result<()> {
        let mut pairs = Vec::new();
        for ((master, follower), mem) in &self.pairs {
            for (fam, m) in mem.iter().enumerate() {
                if m.eps.is_none() && m.master_bias_s == 0.0 && m.follower_bias_s == 0.0 {
                    continue;
                }
                pairs.push(PairEntry {
                    master: master.clone(),
                    follower: follower.clone(),
                    family_hz: FAMILY_HZ[fam],
                    ppm: m.eps.map(|e| -e * 1e6),
                    master_bias_us: m.master_bias_s * 1e6,
                    follower_bias_us: m.follower_bias_s * 1e6,
                });
            }
        }
        let json = serde_json::to_vec_pretty(&FileFormat {
            version: 1,
            pairs,
            alignments: self.alignments.clone(),
        })
        .map_err(std::io::Error::other)?;
        crate::control::config::write_atomic(path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_keeps_each_pair_and_family_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("audivane-clocks-{}", std::process::id()));
        let path = dir.join("follower-clocks.json");
        let _ = std::fs::remove_file(&path);
        let mut s = ClockStore::open(path.clone());
        assert_eq!(s.get("m", "f"), [ClockMemory::default(); 2]);
        let a = ClockMemory {
            eps: Some(-3.5e-5),
            master_bias_s: 4e-6,
            follower_bias_s: 2e-6,
        };
        s.note("m", "f", [a, ClockMemory::default()]);
        s.note("m2", "f", [ClockMemory::default(), a]);
        // Not yet (the last write is too recent), then forced.
        s.last_save = Some(Instant::now());
        s.save(false);
        assert!(!path.exists());
        s.save(true);
        let back = ClockStore::open(path.clone());
        let got = back.get("m", "f");
        assert!((got[0].eps.unwrap() - a.eps.unwrap()).abs() < 1e-12);
        assert!((got[0].master_bias_s - 4e-6).abs() < 1e-12);
        assert_eq!(got[1], ClockMemory::default());
        assert!(back.get("m2", "f")[1].eps.is_some());
        assert_eq!(back.alignment("m"), None);
        assert_eq!(back.get("f", "m"), [ClockMemory::default(); 2]);
        // A run that never locked keeps what was known.
        let mut back = back;
        back.note("m", "f", [ClockMemory::default(); 2]);
        assert!(back.get("m", "f")[0].eps.is_some());
        // A file that does not parse is ignored.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(
            ClockStore::open(path.clone()).get("m", "f"),
            [ClockMemory::default(); 2]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
