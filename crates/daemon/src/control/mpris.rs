//! MPRIS watcher: what is playing, and at which native rate.
//!
//! A small polling thread talks to the session bus with `zbus` (blocking
//! wrapper, no tokio). Every poll it lists `org.mpris.MediaPlayer2.*` names,
//! reads `PlaybackStatus` and the `Metadata` property of each player, and
//! keeps the most recently started `Playing` player as the current one. When
//! that player pauses or stops it stays current (so the UI keeps showing the
//! last track); when it disappears the next playing player takes over.
//!
//! Native rate/bit depth are not part of MPRIS. Players that do expose them
//! use private keys; we accept the conventions we know about:
//!
//! | field | keys (case-, `-` and `_` insensitive) |
//! | --- | --- |
//! | rate | `xesam:audioSampleRate`, `audio-samplerate`, `audioSampleRate`, `samplerate` |
//! | bits | `xesam:audioBitDepth`, `audio-bitdepth`, `audioBitDepth`, `bitsPerSample` |
//!
//! Values may be integers or numeric strings.

use audivane_protocol::ipc::NowPlaying;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use zbus::{Connection, OwnedVariant};

const PLAYER_PREFIX: &str = "org.mpris.MediaPlayer2.";
const PLAYER_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Default poll interval. MPRIS has change signals, but polling keeps the
/// watcher small and immune to players that do not emit them.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(500);

struct Shared {
    now: Mutex<Option<NowPlaying>>,
    quit: AtomicBool,
    ready: Mutex<bool>,
    ready_cv: Condvar,
}

/// Handle to the watcher thread.
pub struct MprisWatcher {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl MprisWatcher {
    pub fn spawn() -> Arc<Self> {
        Self::spawn_with_interval(DEFAULT_INTERVAL)
    }

    pub fn spawn_with_interval(interval: Duration) -> Arc<Self> {
        let shared = Arc::new(Shared {
            now: Mutex::new(None),
            quit: AtomicBool::new(false),
            ready: Mutex::new(false),
            ready_cv: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("avdsp-mpris".into())
            .spawn(move || watch(thread_shared, interval))
            .ok();
        Arc::new(Self {
            shared,
            thread: Mutex::new(handle),
        })
    }

    /// The current track, if any player is (or was recently) playing.
    pub fn now_playing(&self) -> Option<NowPlaying> {
        self.shared
            .now
            .lock()
            .map(|n| n.clone())
            .unwrap_or_default()
    }

    /// Wait until the first poll completed (or `timeout` elapsed). Mainly for
    /// tests.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let ready = self.shared.ready.lock().unwrap_or_else(|e| e.into_inner());
        let (ready, _) = self
            .shared
            .ready_cv
            .wait_timeout_while(ready, timeout, |r| !*r)
            .unwrap_or_else(|e| e.into_inner());
        *ready
    }

    pub fn shutdown(&self) {
        self.shared.quit.store(true, Ordering::SeqCst);
        let handle = self.thread.lock().map(|mut h| h.take()).unwrap_or(None);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for MprisWatcher {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------- thread

fn watch(shared: Arc<Shared>, interval: Duration) {
    let connection = match zbus::block_on(Connection::session()) {
        Ok(connection) => connection,
        Err(err) => {
            log::warn!("MPRIS disabled: cannot connect to the session bus: {err}");
            mark_ready(&shared);
            return;
        }
    };

    let mut tracker = Tracker::default();
    loop {
        if shared.quit.load(Ordering::Relaxed) {
            break;
        }
        let started = Instant::now();
        let players = zbus::block_on(poll(&connection));
        let now = tracker.update(players);
        if let Ok(mut slot) = shared.now.lock() {
            *slot = now;
        }
        mark_ready(&shared);

        let elapsed = started.elapsed();
        thread::sleep(
            interval
                .saturating_sub(elapsed)
                .max(Duration::from_millis(10)),
        );
    }
}

fn mark_ready(shared: &Shared) {
    if let Ok(mut ready) = shared.ready.lock() {
        *ready = true;
        shared.ready_cv.notify_all();
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct PlayerSnapshot {
    name: String,
    /// `Playing`, `Paused`, `Stopped`, or empty when it could not be read.
    status: String,
    now: Option<NowPlaying>,
}

#[derive(Debug, Default)]
struct Tracker {
    current: Option<String>,
    statuses: HashMap<String, String>,
    /// Players in the order they last started playing (oldest first).
    order: Vec<String>,
}

impl Tracker {
    /// Feed one poll and return the track that should be reported.
    fn update(&mut self, players: Vec<PlayerSnapshot>) -> Option<NowPlaying> {
        for player in &players {
            let was_playing = self.statuses.get(&player.name).map(String::as_str);
            if player.status == "Playing" && was_playing != Some("Playing") {
                self.order.retain(|name| name != &player.name);
                self.order.push(player.name.clone());
                self.current = Some(player.name.clone());
            }
        }
        let present = |name: &String| players.iter().any(|p| &p.name == name);
        if self
            .current
            .as_ref()
            .is_some_and(|current| !present(current))
        {
            // The current player went away: fall back to the most recently
            // playing player that is still on the bus.
            self.current = self.order.iter().rev().find(|name| present(name)).cloned();
        }
        if self.current.is_none() {
            // Nothing has played since we started: show any player we can
            // read, so a paused track still appears in the UI.
            self.current = players
                .iter()
                .find(|p| p.now.is_some())
                .map(|p| p.name.clone());
        }
        self.statuses = players
            .iter()
            .map(|p| (p.name.clone(), p.status.clone()))
            .collect();
        let current = self.current.as_deref()?;
        players
            .iter()
            .find(|p| p.name == current)
            .and_then(|p| p.now.clone())
    }
}

async fn poll(connection: &Connection) -> Vec<PlayerSnapshot> {
    let players = match list_players(connection).await {
        Ok(players) => players,
        Err(err) => {
            log::debug!("MPRIS: cannot list names: {err}");
            return Vec::new();
        }
    };
    log::debug!("poll: {} MPRIS player(s): {players:?}", players.len());
    let mut snapshots = Vec::with_capacity(players.len());
    for name in players {
        let status = match get_string(connection, &name, "PlaybackStatus").await {
            Ok(status) => status,
            Err(err) => {
                log::debug!("{name}: PlaybackStatus failed: {err}");
                String::new()
            }
        };
        let metadata = match get_all(connection, &name).await {
            Ok(properties) => metadata_map(&properties),
            Err(err) => {
                log::debug!("{name}: GetAll failed: {err}");
                HashMap::new()
            }
        };
        snapshots.push(PlayerSnapshot {
            now: build_now_playing(&name, &status, &metadata),
            status,
            name,
        });
    }
    snapshots
}

async fn list_players(connection: &Connection) -> zbus::Result<Vec<String>> {
    let reply = connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .await?;
    let names: Vec<String> = reply.body().deserialize()?;
    Ok(names
        .into_iter()
        .filter(|name| name.starts_with(PLAYER_PREFIX))
        .collect())
}

async fn get_string(connection: &Connection, player: &str, property: &str) -> zbus::Result<String> {
    let reply = connection
        .call_method(
            Some(player),
            PLAYER_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(PLAYER_INTERFACE, property),
        )
        .await?;
    // `Properties.Get` returns a variant (`v`); unwrap it to a string.
    let value: OwnedVariant = reply.body().deserialize()?;
    match <&str>::try_from(&value) {
        Ok(text) => Ok(text.to_string()),
        Err(_) => Err(zbus::Error::IncorrectType),
    }
}

async fn get_all(
    connection: &Connection,
    player: &str,
) -> zbus::Result<HashMap<String, OwnedVariant>> {
    let reply = connection
        .call_method(
            Some(player),
            PLAYER_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &(PLAYER_INTERFACE,),
        )
        .await?;
    reply.body().deserialize()
}

/// `Properties.GetAll` returns every property; the track metadata is the
/// `Metadata` entry, itself an `a{sv}` dictionary.
fn metadata_map(properties: &HashMap<String, OwnedVariant>) -> HashMap<String, OwnedVariant> {
    properties
        .get("Metadata")
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| HashMap::<String, OwnedVariant>::try_from(value).ok())
        .unwrap_or_default()
}

fn build_now_playing(
    player: &str,
    status: &str,
    metadata: &HashMap<String, OwnedVariant>,
) -> Option<NowPlaying> {
    let track = extract_metadata(metadata);
    if status.is_empty() && track.is_empty() {
        return None;
    }
    Some(NowPlaying {
        player: player.to_string(),
        title: track.title,
        artist: track.artist,
        album: track.album,
        status: if status.is_empty() {
            "Unknown".to_string()
        } else {
            status.to_string()
        },
        native_rate: track.native_rate,
        native_bits: track.native_bits,
    })
}

#[derive(Debug, Default, PartialEq)]
struct TrackMetadata {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    native_rate: Option<u32>,
    native_bits: Option<u32>,
}

impl TrackMetadata {
    fn is_empty(&self) -> bool {
        self.title.is_none() && self.artist.is_none() && self.album.is_none()
    }
}

fn extract_metadata(metadata: &HashMap<String, OwnedVariant>) -> TrackMetadata {
    let title = take_string(metadata, "xesam:title");
    let album = take_string(metadata, "xesam:album");
    let artist = take_artist(metadata, "xesam:artist");

    let mut track = TrackMetadata {
        title,
        artist,
        album,
        native_rate: None,
        native_bits: None,
    };
    if let Some(key) = find_key(
        metadata,
        &[
            "xesamaudiosamplerate",
            "audiosamplerate",
            "samplerate",
            "sampleratehz",
            "audiosampleratehz",
        ],
    ) {
        track.native_rate = take_u32(metadata, &key);
    }
    if let Some(key) = find_key(
        metadata,
        &[
            "xesamaudiobitdepth",
            "audiobitdepth",
            "bitdepth",
            "bitspersample",
            "audiobits",
        ],
    ) {
        track.native_bits = take_u32(metadata, &key);
    }
    track
}

/// Normalise a metadata key for comparison: lowercase, no `-` or `_`.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn find_key(values: &HashMap<String, OwnedVariant>, candidates: &[&str]) -> Option<String> {
    values
        .keys()
        .find(|key| candidates.contains(&normalize_key(key).as_str()))
        .cloned()
}

fn take_string(values: &HashMap<String, OwnedVariant>, key: &str) -> Option<String> {
    let value = values.get(key)?;
    let text = <&str>::try_from(value).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn take_artist(values: &HashMap<String, OwnedVariant>, key: &str) -> Option<String> {
    let value = values.get(key)?;
    if let Ok(clone) = value.try_clone()
        && let Ok(artists) = Vec::<String>::try_from(clone)
    {
        let joined = artists
            .into_iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    let text = <&str>::try_from(value).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn take_u32(values: &HashMap<String, OwnedVariant>, key: &str) -> Option<u32> {
    let value = values.get(key)?;
    let nonzero = |v: u32| (v > 0).then_some(v);
    // Players disagree on the integer width; accept any of them.
    if let Ok(v) = u32::try_from(value) {
        return nonzero(v);
    }
    if let Ok(v) = i32::try_from(value) {
        return u32::try_from(v).ok().and_then(nonzero);
    }
    if let Ok(v) = u64::try_from(value) {
        return u32::try_from(v).ok().and_then(nonzero);
    }
    if let Ok(v) = i64::try_from(value) {
        return u32::try_from(v).ok().and_then(nonzero);
    }
    if let Ok(v) = u16::try_from(value) {
        return nonzero(u32::from(v));
    }
    if let Ok(text) = <&str>::try_from(value) {
        let text = text.trim();
        let digits = text
            .strip_suffix("Hz")
            .or_else(|| text.strip_suffix("hz"))
            .unwrap_or(text)
            .trim();
        return digits.parse::<u32>().ok().and_then(nonzero);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::Str;

    fn text(value: &str) -> OwnedVariant {
        OwnedVariant::from(Str::from(value.to_string()))
    }

    #[test]
    fn metadata_extraction_and_key_conventions() {
        let mut map = HashMap::new();
        map.insert("xesam:title".to_string(), text("Track"));
        map.insert("xesam:album".to_string(), text("Album"));
        map.insert("xesam:artist".to_string(), text("Artist"));
        map.insert(
            "audio-samplerate".to_string(),
            OwnedVariant::from(96_000u32),
        );
        map.insert("xesam:audioBitDepth".to_string(), text("24"));

        let track = extract_metadata(&map);
        assert_eq!(track.title.as_deref(), Some("Track"));
        assert_eq!(track.artist.as_deref(), Some("Artist"));
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.native_rate, Some(96_000));
        assert_eq!(track.native_bits, Some(24));
    }

    #[test]
    fn unknown_and_empty_metadata() {
        let map = HashMap::new();
        assert_eq!(extract_metadata(&map), TrackMetadata::default());
        let mut map = HashMap::new();
        map.insert("xesam:title".to_string(), text("   "));
        assert!(extract_metadata(&map).is_empty());
    }

    #[test]
    fn key_normalisation() {
        assert_eq!(
            normalize_key("XESAM:Audio-Sample_Rate"),
            "xesamaudiosamplerate"
        );
        assert_eq!(normalize_key("bitsPerSample"), "bitspersample");
        let mut values = HashMap::new();
        values.insert("audioSampleRate".to_string(), OwnedVariant::from(44_100u32));
        assert_eq!(
            find_key(&values, &["audiosamplerate"]),
            Some("audioSampleRate".to_string())
        );
    }

    #[test]
    fn tracker_prefers_the_most_recently_playing() {
        let player = |name: &str, status: &str| PlayerSnapshot {
            name: name.to_string(),
            status: status.to_string(),
            now: Some(NowPlaying {
                player: name.to_string(),
                title: Some(name.to_string()),
                artist: None,
                album: None,
                status: status.to_string(),
                native_rate: None,
                native_bits: None,
            }),
        };
        let mut tracker = Tracker::default();
        let first = tracker.update(vec![
            player("org.mpris.MediaPlayer2.a", "Playing"),
            player("org.mpris.MediaPlayer2.b", "Paused"),
        ]);
        assert_eq!(first.unwrap().player, "org.mpris.MediaPlayer2.a");

        // b starts playing: it becomes current.
        let second = tracker.update(vec![
            player("org.mpris.MediaPlayer2.a", "Playing"),
            player("org.mpris.MediaPlayer2.b", "Playing"),
        ]);
        assert_eq!(second.unwrap().player, "org.mpris.MediaPlayer2.b");

        // a pauses: b is still current.
        let third = tracker.update(vec![
            player("org.mpris.MediaPlayer2.a", "Paused"),
            player("org.mpris.MediaPlayer2.b", "Paused"),
        ]);
        assert_eq!(third.unwrap().player, "org.mpris.MediaPlayer2.b");

        // b disappears, a is still paused: keep reporting a.
        let fourth = tracker.update(vec![player("org.mpris.MediaPlayer2.a", "Paused")]);
        assert_eq!(fourth.unwrap().player, "org.mpris.MediaPlayer2.a");

        // Nothing left.
        assert!(tracker.update(vec![]).is_none());
    }
}
