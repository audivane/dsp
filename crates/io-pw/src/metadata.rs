//! Crash-safe PipeWire `settings` metadata capture, ownership tracking, and
//! restoration.
//!
//! Before modifying `clock.allowed-rates` or `clock.force-rate`, the PipeWire
//! session must:
//! 1. Receive the initial `settings` metadata property snapshot followed by a
//!    `core.sync` barrier so pre-existing keys are captured faithfully.
//! 2. Recover any un-clobbered keys left behind by a previous crashed process
//!    from `$XDG_RUNTIME_DIR/audivane-dsp/settings-restore.json`.
//! 3. Persist the original values atomically to disk before applying changes.
//! 4. Track property echoes so externally modified keys are marked unowned and
//!    left untouched on teardown.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Keys inside the PipeWire `settings` metadata object managed by Audivane.
pub const MANAGED_KEYS: &[&str] = &["clock.allowed-rates", "clock.force-rate"];

const RESTORE_FILE_VERSION: u32 = 1;

/// Captured PipeWire metadata value + type string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataValue {
    pub type_name: Option<String>,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedKeyEntry {
    pub original: Option<MetadataValue>,
    pub last_written: Option<MetadataValue>,
    pub owned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsRestoreFile {
    pub version: u32,
    pub keys: BTreeMap<String, PersistedKeyEntry>,
}

/// Concrete `pw_metadata_set_property` operation to execute on PipeWire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataOp {
    pub key: String,
    pub type_name: Option<String>,
    pub value: Option<String>,
}

/// Resolve the default crash-recovery file path:
/// `$XDG_RUNTIME_DIR/audivane-dsp/settings-restore.json`.
pub fn default_restore_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir)
            .join("audivane-dsp")
            .join("settings-restore.json");
    }
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("audivane-dsp-{uid}"))
        .join("settings-restore.json")
}

fn is_managed_key(key: &str) -> bool {
    MANAGED_KEYS.contains(&key)
}

/// Pure state machine + persistence handler for PipeWire `settings` metadata.
#[derive(Debug)]
pub struct MetadataTracker {
    restore_path: PathBuf,
    snapshot_synced: bool,
    dirty: bool,
    live: BTreeMap<String, Option<MetadataValue>>,
    originals: BTreeMap<String, Option<MetadataValue>>,
    last_written: BTreeMap<String, Option<MetadataValue>>,
    owned: BTreeMap<String, bool>,
    pending_self_writes: BTreeMap<String, VecDeque<Option<MetadataValue>>>,
}

impl MetadataTracker {
    pub fn new() -> Self {
        Self::with_path(default_restore_path())
    }

    pub fn with_path(restore_path: PathBuf) -> Self {
        Self {
            restore_path,
            snapshot_synced: false,
            dirty: false,
            live: BTreeMap::new(),
            originals: BTreeMap::new(),
            last_written: BTreeMap::new(),
            owned: BTreeMap::new(),
            pending_self_writes: BTreeMap::new(),
        }
    }

    pub fn restore_path(&self) -> &Path {
        &self.restore_path
    }

    pub fn is_snapshot_synced(&self) -> bool {
        self.snapshot_synced
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn is_owned(&self, key: &str) -> bool {
        self.owned.get(key).copied().unwrap_or(false)
    }

    /// Record a `Metadata::property` event from PipeWire (`subject == 0`).
    pub fn on_property(
        &mut self,
        subject: u32,
        key: Option<&str>,
        type_: Option<&str>,
        value: Option<&str>,
    ) {
        if subject != 0 {
            return;
        }
        let Some(key_str) = key.filter(|k| is_managed_key(k)) else {
            return;
        };
        let incoming = value.map(|v| MetadataValue {
            type_name: type_.map(str::to_string),
            value: v.to_string(),
        });
        self.live.insert(key_str.to_string(), incoming.clone());

        if !self.snapshot_synced {
            return;
        }

        // Check if this event is an echo of our own pending write (allowing
        // PipeWire to coalesce earlier pending writes on the same key).
        if let Some(q) = self.pending_self_writes.get_mut(key_str)
            && let Some(pos) = q.iter().position(|expected| match (expected, &incoming) {
                (None, None) => true,
                (Some(e), Some(inc)) => e.value == inc.value,
                _ => false,
            })
        {
            for _ in 0..=pos {
                q.pop_front();
            }
            return;
        }

        if !self.dirty {
            return;
        }

        // If we previously modified this key and an external client changed it
        // to something other than our last written value, relinquish ownership
        // so teardown leaves the external change untouched.
        let matches_last_written = match (self.last_written.get(key_str), &incoming) {
            (Some(Some(lw)), Some(inc)) => lw.value == inc.value,
            (Some(None), None) => true,
            _ => false,
        };
        if self.is_owned(key_str) && !matches_last_written {
            log::info!(
                "PipeWire metadata key '{key_str}' was modified externally; relinquishing ownership"
            );
            self.owned.insert(key_str.to_string(), false);
            if let Some(q) = self.pending_self_writes.get_mut(key_str) {
                q.clear();
            }
            if let Err(e) = Self::persist_snapshot(
                &self.restore_path,
                &self.originals,
                &self.last_written,
                &self.owned,
            ) {
                log::warn!("Failed to update {}: {e}", self.restore_path.display());
            }
        }
    }

    /// Complete the initial `core.sync` snapshot barrier and check for any stale
    /// crash-recovery file on disk.
    ///
    /// Returns recovery operations that must be sent to PipeWire and synced
    /// BEFORE writing new policy values (or on abort/cleanup).
    pub fn complete_initial_sync_and_recover(&mut self) -> Vec<MetadataOp> {
        self.snapshot_synced = true;
        let mut recovery_ops = Vec::new();

        if let Some(prev) = self.read_restore_file() {
            log::warn!(
                "Found stale PipeWire metadata restore file at {}; recovering original settings",
                self.restore_path.display()
            );
            for &key in MANAGED_KEYS {
                let live_val = self.live.get(key).cloned().flatten();
                if let Some(entry) = prev.keys.get(key) {
                    let matches_crashed_write = match (&entry.last_written, &live_val) {
                        (Some(lw), Some(lv)) => lw.value == lv.value,
                        (None, None) => true,
                        _ => false,
                    };
                    if entry.owned && matches_crashed_write {
                        // Still holds the value written by the crashed instance:
                        // emit a restore op back to the true pre-crash original,
                        // and preserve that true original for this session.
                        let orig = entry.original.clone();
                        self.originals.insert(key.to_string(), orig.clone());
                        let already_at_original = match (&orig, &live_val) {
                            (Some(o), Some(lv)) => o.value == lv.value,
                            (None, None) => true,
                            _ => false,
                        };
                        if !already_at_original {
                            self.pending_self_writes
                                .entry(key.to_string())
                                .or_default()
                                .push_back(orig.clone());
                            recovery_ops.push(MetadataOp {
                                key: key.to_string(),
                                type_name: orig.as_ref().and_then(|v| v.type_name.clone()),
                                value: orig.as_ref().map(|v| v.value.clone()),
                            });
                        }
                    } else {
                        // Externally modified after crash (or unowned): keep live.
                        self.originals.insert(key.to_string(), live_val);
                    }
                } else {
                    self.originals.insert(key.to_string(), live_val);
                }
            }
        } else {
            for &key in MANAGED_KEYS {
                let live_val = self.live.get(key).cloned().flatten();
                self.originals.insert(key.to_string(), live_val);
            }
        }

        recovery_ops
    }

    /// Plan metadata writes for `(key, type_name, value)` pairs, atomically
    /// persisting and fsyncing `settings-restore.json` (plus its parent
    /// directory) BEFORE updating tracker state or returning operations to send
    /// to PipeWire.
    ///
    /// If persistence fails, returns `Err` with `MetadataTracker` state left
    /// completely unchanged so callers do not send unrecoverable metadata edits.
    /// Returns `Ok(Vec::new())` if the initial snapshot sync barrier has not
    /// completed yet.
    pub fn plan_writes(
        &mut self,
        writes: &[(&str, &str, &str)],
    ) -> std::io::Result<Vec<MetadataOp>> {
        if !self.snapshot_synced {
            return Ok(Vec::new());
        }

        let mut cand_originals = self.originals.clone();
        let mut cand_last_written = self.last_written.clone();
        let mut cand_owned = self.owned.clone();
        let mut cand_pending = self.pending_self_writes.clone();

        let mut ops = Vec::new();
        for &(key, type_name, value) in writes {
            if !is_managed_key(key) {
                continue;
            }
            // If an external client modified this key after we took ownership,
            // leave it untouched.
            if self.dirty && cand_owned.get(key) == Some(&false) {
                continue;
            }
            // Ensure an original entry exists even if key wasn't seen during snapshot.
            cand_originals
                .entry(key.to_string())
                .or_insert_with(|| self.live.get(key).cloned().flatten());

            let next_val = MetadataValue {
                type_name: Some(type_name.to_string()),
                value: value.to_string(),
            };
            cand_owned.insert(key.to_string(), true);
            cand_last_written.insert(key.to_string(), Some(next_val.clone()));
            cand_pending
                .entry(key.to_string())
                .or_default()
                .push_back(Some(next_val));

            ops.push(MetadataOp {
                key: key.to_string(),
                type_name: Some(type_name.to_string()),
                value: Some(value.to_string()),
            });
        }

        if !ops.is_empty() {
            Self::persist_snapshot(
                &self.restore_path,
                &cand_originals,
                &cand_last_written,
                &cand_owned,
            )?;
            self.dirty = true;
            self.originals = cand_originals;
            self.last_written = cand_last_written;
            self.owned = cand_owned;
            self.pending_self_writes = cand_pending;
        }

        Ok(ops)
    }

    /// Plan restore operations for keys we still own.
    pub fn plan_restore(&mut self) -> Vec<MetadataOp> {
        if !self.dirty {
            return Vec::new();
        }
        let mut ops = Vec::new();
        for &key in MANAGED_KEYS {
            if !self.is_owned(key) {
                continue;
            }
            let orig = self.originals.get(key).cloned().flatten();
            ops.push(MetadataOp {
                key: key.to_string(),
                type_name: orig.as_ref().and_then(|v| v.type_name.clone()),
                value: orig.as_ref().map(|v| v.value.clone()),
            });
            self.owned.insert(key.to_string(), false);
        }
        self.dirty = false;
        self.pending_self_writes.clear();
        ops
    }

    /// Remove the on-disk restore file once restore operations have been flushed
    /// to PipeWire.
    pub fn remove_restore_file(&self) {
        if let Err(e) = fs::remove_file(&self.restore_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("Failed to remove {}: {e}", self.restore_path.display());
        }
    }

    fn read_restore_file(&self) -> Option<SettingsRestoreFile> {
        let raw = fs::read_to_string(&self.restore_path).ok()?;
        match serde_json::from_str::<SettingsRestoreFile>(&raw) {
            Ok(parsed) if parsed.version == RESTORE_FILE_VERSION => Some(parsed),
            Ok(parsed) => {
                log::warn!(
                    "Ignoring {} with unexpected version {}",
                    self.restore_path.display(),
                    parsed.version
                );
                None
            }
            Err(e) => {
                log::warn!("Ignoring corrupt {}: {e}", self.restore_path.display());
                None
            }
        }
    }

    fn persist_snapshot(
        restore_path: &Path,
        originals: &BTreeMap<String, Option<MetadataValue>>,
        last_written: &BTreeMap<String, Option<MetadataValue>>,
        owned_map: &BTreeMap<String, bool>,
    ) -> std::io::Result<()> {
        if let Some(parent) = restore_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut keys = BTreeMap::new();
        for &k in MANAGED_KEYS {
            let original = originals.get(k).cloned().flatten();
            let last_written_val = last_written.get(k).cloned().flatten();
            let owned = owned_map.get(k).copied().unwrap_or(false);
            if last_written_val.is_some() || owned {
                keys.insert(
                    k.to_string(),
                    PersistedKeyEntry {
                        original,
                        last_written: last_written_val,
                        owned,
                    },
                );
            }
        }
        let payload = SettingsRestoreFile {
            version: RESTORE_FILE_VERSION,
            keys,
        };
        let json = serde_json::to_string_pretty(&payload).map_err(std::io::Error::other)?;
        let tmp_path = restore_path.with_extension("json.tmp");
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path)?;
        let write_res = (|| -> std::io::Result<()> {
            file.write_all(json.as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(e) = write_res {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        if let Err(e) = fs::rename(&tmp_path, restore_path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        if let Some(parent) = restore_path.parent()
            && !parent.as_os_str().is_empty()
        {
            let dir_handle = fs::File::open(parent)?;
            dir_handle.sync_all()?;
        }
        Ok(())
    }
}

impl Default for MetadataTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_restore_path(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "audivane-meta-test-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let path = dir.join("settings-restore.json");
        (dir, path)
    }

    #[test]
    fn captures_originals_before_writing_and_restores_them() {
        let (dir, path) = temp_restore_path("capture_restore");
        let mut tracker = MetadataTracker::with_path(path.clone());

        // Writes before initial snapshot + sync are refused.
        assert!(
            tracker
                .plan_writes(&[("clock.allowed-rates", "Spa:String:JSON", "[ 48000 ]")])
                .unwrap()
                .is_empty()
        );

        // Initial metadata events arrive (`clock.allowed-rates` present,
        // `clock.force-rate` absent).
        tracker.on_property(
            0,
            Some("clock.allowed-rates"),
            Some("Spa:String:JSON"),
            Some("[ 44100 48000 ]"),
        );
        let recovery = tracker.complete_initial_sync_and_recover();
        assert!(recovery.is_empty());

        let ops = tracker
            .plan_writes(&[
                (
                    "clock.allowed-rates",
                    "Spa:String:JSON",
                    "[ 44100 48000 96000 192000 ]",
                ),
                ("clock.force-rate", "Spa:String:JSON", "48000"),
            ])
            .unwrap();
        assert_eq!(ops.len(), 2);
        assert!(path.exists(), "restore file must exist before PW write");

        // Echo our own writes back from PipeWire: ownership must remain intact.
        tracker.on_property(
            0,
            Some("clock.allowed-rates"),
            Some("Spa:String:JSON"),
            Some("[ 44100 48000 96000 192000 ]"),
        );
        tracker.on_property(
            0,
            Some("clock.force-rate"),
            Some("Spa:String:JSON"),
            Some("48000"),
        );
        assert!(tracker.is_owned("clock.allowed-rates"));
        assert!(tracker.is_owned("clock.force-rate"));

        let restore = tracker.plan_restore();
        assert_eq!(
            restore,
            vec![
                MetadataOp {
                    key: "clock.allowed-rates".into(),
                    type_name: Some("Spa:String:JSON".into()),
                    value: Some("[ 44100 48000 ]".into()),
                },
                MetadataOp {
                    key: "clock.force-rate".into(),
                    type_name: None,
                    value: None,
                },
            ]
        );
        tracker.remove_restore_file();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaves_externally_modified_key_untouched_on_restore() {
        let (dir, path) = temp_restore_path("external_mod");
        let mut tracker = MetadataTracker::with_path(path.clone());
        tracker.on_property(
            0,
            Some("clock.allowed-rates"),
            Some("Spa:String:JSON"),
            Some("[ 48000 96000 ]"),
        );
        assert!(tracker.complete_initial_sync_and_recover().is_empty());

        tracker
            .plan_writes(&[
                ("clock.allowed-rates", "Spa:String:JSON", "[ 44100 48000 ]"),
                ("clock.force-rate", "Spa:String:JSON", "48000"),
            ])
            .unwrap();
        // Echo our writes first.
        tracker.on_property(
            0,
            Some("clock.allowed-rates"),
            Some("Spa:String:JSON"),
            Some("[ 44100 48000 ]"),
        );
        tracker.on_property(
            0,
            Some("clock.force-rate"),
            Some("Spa:String:JSON"),
            Some("48000"),
        );

        // External user changes `clock.force-rate` to 96000.
        tracker.on_property(
            0,
            Some("clock.force-rate"),
            Some("Spa:String:JSON"),
            Some("96000"),
        );
        assert!(tracker.is_owned("clock.allowed-rates"));
        assert!(!tracker.is_owned("clock.force-rate"));

        // Subsequent rate-following write should skip the unowned key.
        let follow = tracker
            .plan_writes(&[("clock.force-rate", "Spa:String:JSON", "44100")])
            .unwrap();
        assert!(follow.is_empty());

        // Restore touches ONLY `clock.allowed-rates`, leaving `clock.force-rate` alone.
        let restore = tracker.plan_restore();
        assert_eq!(
            restore,
            vec![MetadataOp {
                key: "clock.allowed-rates".into(),
                type_name: Some("Spa:String:JSON".into()),
                value: Some("[ 48000 96000 ]".into()),
            }]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovers_true_originals_after_crash_and_respects_post_crash_external_edits() {
        let (dir, path) = temp_restore_path("crash_recovery");

        // Run 1 starts with custom clock.allowed-rates, modifies both keys, then "crashes" (no restore).
        {
            let mut run1 = MetadataTracker::with_path(path.clone());
            run1.on_property(
                0,
                Some("clock.allowed-rates"),
                Some("Spa:String:JSON"),
                Some("[ 48000 192000 ]"),
            );
            assert!(run1.complete_initial_sync_and_recover().is_empty());
            run1.plan_writes(&[
                (
                    "clock.allowed-rates",
                    "Spa:String:JSON",
                    "[ 44100 48000 96000 192000 ]",
                ),
                ("clock.force-rate", "Spa:String:JSON", "48000"),
            ])
            .unwrap();
            assert!(path.exists());
            // Dropped without plan_restore() -> simulates kill -9!
        }

        // Subcase A: Run 2 sees the dirty values left by Run 1, recovers true originals first.
        {
            let mut run2 = MetadataTracker::with_path(path.clone());
            run2.on_property(
                0,
                Some("clock.allowed-rates"),
                Some("Spa:String:JSON"),
                Some("[ 44100 48000 96000 192000 ]"),
            );
            run2.on_property(
                0,
                Some("clock.force-rate"),
                Some("Spa:String:JSON"),
                Some("48000"),
            );
            let recovery = run2.complete_initial_sync_and_recover();
            assert_eq!(
                recovery,
                vec![
                    MetadataOp {
                        key: "clock.allowed-rates".into(),
                        type_name: Some("Spa:String:JSON".into()),
                        value: Some("[ 48000 192000 ]".into()),
                    },
                    MetadataOp {
                        key: "clock.force-rate".into(),
                        type_name: None,
                        value: None,
                    },
                ]
            );

            // Run 2 then writes its own values, receives PipeWire echoes for BOTH
            // the recovery ops and the new writes, and cleanly shuts down ->
            // restores pre-Run-1 originals!
            run2.plan_writes(&[
                ("clock.allowed-rates", "Spa:String:JSON", "[ 48000 ]"),
                ("clock.force-rate", "Spa:String:JSON", "48000"),
            ])
            .unwrap();
            run2.on_property(
                0,
                Some("clock.allowed-rates"),
                Some("Spa:String:JSON"),
                Some("[ 48000 192000 ]"),
            );
            run2.on_property(0, Some("clock.force-rate"), None, None);
            run2.on_property(
                0,
                Some("clock.allowed-rates"),
                Some("Spa:String:JSON"),
                Some("[ 48000 ]"),
            );
            run2.on_property(
                0,
                Some("clock.force-rate"),
                Some("Spa:String:JSON"),
                Some("48000"),
            );
            assert!(run2.is_owned("clock.allowed-rates"));
            assert!(run2.is_owned("clock.force-rate"));

            let restore = run2.plan_restore();
            assert_eq!(
                restore,
                vec![
                    MetadataOp {
                        key: "clock.allowed-rates".into(),
                        type_name: Some("Spa:String:JSON".into()),
                        value: Some("[ 48000 192000 ]".into()),
                    },
                    MetadataOp {
                        key: "clock.force-rate".into(),
                        type_name: None,
                        value: None,
                    },
                ]
            );
            run2.remove_restore_file();
        }

        // Subcase B: If someone changed `clock.force-rate` externally while dead,
        // crash recovery does NOT clobber their post-crash value.
        {
            let mut run1 = MetadataTracker::with_path(path.clone());
            run1.on_property(
                0,
                Some("clock.allowed-rates"),
                Some("Spa:String:JSON"),
                Some("[ 44100 ]"),
            );
            run1.complete_initial_sync_and_recover();
            run1.plan_writes(&[
                ("clock.allowed-rates", "Spa:String:JSON", "[ 48000 ]"),
                ("clock.force-rate", "Spa:String:JSON", "48000"),
            ])
            .unwrap();
        }
        {
            let mut run2 = MetadataTracker::with_path(path.clone());
            run2.on_property(
                0,
                Some("clock.allowed-rates"),
                Some("Spa:String:JSON"),
                Some("[ 48000 ]"),
            );
            run2.on_property(
                0,
                Some("clock.force-rate"),
                Some("Spa:String:JSON"),
                Some("88200"),
            );
            let recovery = run2.complete_initial_sync_and_recover();
            assert_eq!(
                recovery,
                vec![MetadataOp {
                    key: "clock.allowed-rates".into(),
                    type_name: Some("Spa:String:JSON".into()),
                    value: Some("[ 44100 ]".into()),
                }]
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_writes_fails_cleanly_without_mutating_state_when_persist_fails() {
        let (dir, _) = temp_restore_path("persist_failure");
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not_a_dir");
        fs::write(&blocker, b"file").unwrap();
        let unwriteable_path = blocker.join("settings-restore.json");

        let mut tracker = MetadataTracker::with_path(unwriteable_path);
        tracker.on_property(
            0,
            Some("clock.allowed-rates"),
            Some("Spa:String:JSON"),
            Some("[ 48000 ]"),
        );
        assert!(tracker.complete_initial_sync_and_recover().is_empty());

        let res = tracker.plan_writes(&[
            ("clock.allowed-rates", "Spa:String:JSON", "[ 44100 48000 ]"),
            ("clock.force-rate", "Spa:String:JSON", "48000"),
        ]);
        assert!(res.is_err(), "expected persistence failure to return Err");
        assert!(
            !tracker.is_dirty(),
            "tracker must not become dirty when restore file persistence failed"
        );
        assert!(!tracker.is_owned("clock.allowed-rates"));
        assert!(!tracker.is_owned("clock.force-rate"));
        assert!(tracker.plan_restore().is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
