//! What the volume failsafe leaves behind for later: switches it muted
//! (to undo on the next open of that card, also after a reboot) and
//! whether the engine still held the DAC (for `--volume-failsafe` after a
//! crash). See [`super::volume::failsafe_write`].

use audivane_io_alsa::PlaybackChannel;
use std::path::{Path, PathBuf};

/// `$XDG_STATE_HOME/audivane-dsp/failsafe-muted` (default
/// `~/.local/state/...`): persistent, because ALSA restores a muted switch
/// at boot while the runtime directory is gone.
pub fn mutes_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("audivane-dsp").join("failsafe-muted"))
}

/// `$XDG_RUNTIME_DIR/audivane-dsp/dac-released`: present while the engine
/// does not hold the DAC (it let go, another program may have it). Gone on
/// reboot, like the hold itself.
pub fn released_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|dir| PathBuf::from(dir).join("audivane-dsp").join("dac-released"))
}

/// Record whether the engine holds the DAC (see [`released_path`]).
pub fn note_released(path: Option<&Path>, released: bool) {
    let Some(path) = path else { return };
    if released {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(path, b"released\n") {
            log::warn!(
                "volume: cannot record the DAC release in {}: {e}",
                path.display()
            );
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether the engine had let go of the DAC when it last ran.
pub fn was_released(path: Option<&Path>) -> bool {
    path.is_some_and(Path::exists)
}

/// Whether `--volume-failsafe` may write the card, given the released
/// marker's location: not when the engine had let go of the DAC (the card
/// was lowered then, another program may hold it), and not when the
/// location is unknown (`$XDG_RUNTIME_DIR` unset): the marker must not
/// outlive a reboot, so there is no persistent fallback, and without it
/// the failsafe cannot tell. `Err` says why not.
pub fn crash_failsafe_may_write(marker: Option<&Path>) -> Result<(), &'static str> {
    match marker {
        None => {
            Err("$XDG_RUNTIME_DIR is not set, so it cannot tell whether the engine held the DAC")
        }
        Some(p) if p.exists() => Err("the engine had let go of the DAC"),
        Some(_) => Ok(()),
    }
}

/// An engine starting: it does not hold the DAC until its first
/// negotiation, so a crash before that leaves the card alone.
pub fn engine_starting(marker: Option<&Path>) {
    note_released(marker, true);
}

/// The card a device is on, by a name that survives reboots and other
/// cards coming and going (not its index).
pub fn card_key(device: &str) -> String {
    audivane_io_alsa::mixer::card_identity(device).unwrap_or_else(|| device.to_string())
}

/// A playback switch the failsafe muted (it was on before).
#[derive(Clone, Debug, PartialEq)]
pub struct MutedCard {
    pub card: String,
    pub device: String,
    /// The level the failsafe wrote (the heard level).
    pub level_db: f64,
    /// Every playback channel as the card was left with its switch muted
    /// (level and switch), when known. A later open undoes the mute in
    /// `software` only while every channel still reads the same (see
    /// [`same_channels`]): otherwise it was touched since and the switch is
    /// no longer ours.
    pub left: Option<Vec<PlaybackChannel>>,
}

/// Two readings of a card's channels are the same: as many channels, each
/// switch equal and each level within the mixer's readback tolerance.
pub fn same_channels(a: &[PlaybackChannel], b: &[PlaybackChannel]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.switch_on == y.switch_on
                && (x.level_db - y.level_db).abs() <= super::volume::HW_READBACK_TOL_DB
        })
}

/// `level/switch` per channel, comma-separated (switch `1` on, `0` off,
/// `-` none).
fn format_channels(channels: &[PlaybackChannel]) -> String {
    channels
        .iter()
        .map(|c| {
            let sw = match c.switch_on {
                Some(true) => "1",
                Some(false) => "0",
                None => "-",
            };
            format!("{}/{sw}", c.level_db)
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_channels(text: &str) -> Option<Vec<PlaybackChannel>> {
    let channels = text
        .trim()
        .split(',')
        .map(|c| {
            let (level, sw) = c.split_once('/')?;
            let switch_on = match sw {
                "1" => Some(true),
                "0" => Some(false),
                "-" => None,
                _ => return None,
            };
            Some(PlaybackChannel {
                level_db: level.parse().ok()?,
                switch_on,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    (!channels.is_empty()).then_some(channels)
}

/// The failsafe's mutes, one per card, kept in [`mutes_path`].
#[derive(Debug, Default)]
pub struct MuteStore {
    path: Option<PathBuf>,
    entries: Vec<MutedCard>,
}

impl MuteStore {
    pub fn load(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| {
                text.lines()
                    .filter_map(|line| {
                        let mut f = line.split('\t');
                        let card = f.next()?.to_string();
                        let device = f.next()?.to_string();
                        let level_db = f.next()?.trim().parse().ok()?;
                        let left = f.next().and_then(parse_channels);
                        Some(MutedCard {
                            card,
                            device,
                            level_db,
                            left,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { path, entries }
    }

    /// The failsafe muted `device`'s card after writing `level_db`, which
    /// left its channels as `left` (replacing an older entry for it).
    pub fn record(&mut self, device: &str, level_db: f64, left: Option<Vec<PlaybackChannel>>) {
        let card = card_key(device);
        self.entries.retain(|e| e.card != card);
        self.entries.push(MutedCard {
            card,
            device: device.to_string(),
            level_db,
            left,
        });
        self.save();
    }

    /// The mute recorded for `device`'s card, if any (it stays recorded
    /// until [`Self::forget`]: only an unmute that worked undoes it).
    pub fn get(&self, device: &str) -> Option<&MutedCard> {
        let card = card_key(device);
        self.entries.iter().find(|e| e.card == card)
    }

    /// The switch of `device`'s card was turned back on: drop its entry;
    /// other cards' entries stay.
    pub fn forget(&mut self, device: &str) {
        let card = card_key(device);
        let before = self.entries.len();
        self.entries.retain(|e| e.card != card);
        if self.entries.len() != before {
            self.save();
        }
    }

    pub fn entries(&self) -> &[MutedCard] {
        &self.entries
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if self.entries.is_empty() {
            if std::fs::remove_file(path).is_ok()
                && let Some(dir) = path.parent()
                && let Ok(d) = std::fs::File::open(dir)
            {
                // The removal, too, must survive a crash.
                let _ = d.sync_all();
            }
            return;
        }
        let text: String = self
            .entries
            .iter()
            .map(|e| match &e.left {
                Some(left) => format!(
                    "{}\t{}\t{}\t{}\n",
                    e.card,
                    e.device,
                    e.level_db,
                    format_channels(left)
                ),
                None => format!("{}\t{}\t{}\n", e.card, e.device, e.level_db),
            })
            .collect();
        if let Err(e) = write_atomically(path, text.as_bytes()) {
            log::warn!(
                "volume: cannot record the failsafe mute in {}: {e}",
                path.display()
            );
        }
    }
}

/// Write `path` so that a crash leaves either the old or the new content:
/// a temp file in the same directory, fsynced, renamed over it, and the
/// directory fsynced.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floor(switch_on: bool) -> Vec<PlaybackChannel> {
        vec![
            PlaybackChannel {
                level_db: -64.0,
                switch_on: Some(switch_on),
            };
            2
        ]
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "audivane-failsafe-store-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("failsafe-muted")
    }

    /// Saving replaces the file in one step (a new file renamed over the
    /// old one, never truncated in place) and leaves no temp file behind.
    #[test]
    fn saves_replace_the_file_atomically() {
        use std::os::unix::fs::MetadataExt;
        let path = temp("atomic");
        let mut store = MuteStore::load(Some(path.clone()));
        store.record("hw:CARD=DAC,DEV=0", -80.0, Some(floor(false)));
        let first = std::fs::metadata(&path).unwrap().ino();
        store.record("hw:CARD=Other,DEV=0", -70.0, None);
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            first,
            "rewritten in place"
        );
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("failsafe-muted")]);
        assert_eq!(MuteStore::load(Some(path.clone())).entries().len(), 2);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The crash failsafe writes only when it knows the engine held the DAC:
    /// never with the marker location unknown, never after a release, and
    /// not for an engine that never got the DAC.
    #[test]
    fn crash_failsafe_writes_only_when_the_engine_held_the_dac() {
        let path = temp("crash").with_file_name("dac-released");
        assert!(crash_failsafe_may_write(None).is_err());
        assert!(crash_failsafe_may_write(Some(&path)).is_ok());
        engine_starting(Some(&path));
        assert!(crash_failsafe_may_write(Some(&path)).is_err());
        note_released(Some(&path), false); // first negotiation
        assert!(crash_failsafe_may_write(Some(&path)).is_ok());
        note_released(Some(&path), true); // let go
        assert!(crash_failsafe_may_write(Some(&path)).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
