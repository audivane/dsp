//! MPRIS watcher against a fake player registered on the real session bus.
//!
//! Skips (successfully) when there is no session bus, so it is safe in CI.

use audivane_dsp_daemon::control::mpris::MprisWatcher;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::{OwnedVariant, Str};

#[derive(Clone)]
enum MetaValue {
    Text(String),
    Int(i32),
}

#[derive(Default)]
struct PlayerState {
    status: String,
    metadata: Vec<(String, MetaValue)>,
}

struct FakePlayer {
    state: Arc<Mutex<PlayerState>>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl FakePlayer {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.state.lock().unwrap().status.clone()
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedVariant> {
        self.state
            .lock()
            .unwrap()
            .metadata
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    MetaValue::Text(text) => OwnedVariant::from(Str::from(text.clone())),
                    MetaValue::Int(number) => OwnedVariant::from(*number),
                };
                (key.clone(), value)
            })
            .collect()
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    fn play_pause(&self) {}
}

fn wait_for<F: FnMut() -> bool>(what: &str, timeout: Duration, mut check: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("timed out waiting for {what}");
    false
}

#[test]
fn watcher_tracks_the_most_recently_playing_player() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("audivane_dsp_daemon=debug"),
    )
    .try_init();

    // No session bus: nothing to test here.
    if zbus::block_on(zbus::Connection::session()).is_err() {
        eprintln!("skipping MPRIS test: no session bus");
        return;
    }

    let state = Arc::new(Mutex::new(PlayerState {
        status: "Playing".into(),
        metadata: vec![
            ("xesam:title".into(), MetaValue::Text("T5b Title".into())),
            ("xesam:artist".into(), MetaValue::Text("T5b Artist".into())),
            ("xesam:album".into(), MetaValue::Text("T5b Album".into())),
            // Non-standard native format keys, as some players expose them.
            ("audio-samplerate".into(), MetaValue::Text("96000".into())),
            ("xesam:audioBitDepth".into(), MetaValue::Int(24)),
        ],
    }));

    let connection = zbus::block_on(async {
        zbus::connection::Builder::session()
            .name("org.mpris.MediaPlayer2.t5bfake")
            .serve_at(
                "/org/mpris/MediaPlayer2",
                FakePlayer {
                    state: Arc::clone(&state),
                },
            )
            .build()
            .await
    })
    .expect("register fake MPRIS player");

    let watcher = MprisWatcher::spawn_with_interval(Duration::from_millis(100));
    assert!(watcher.wait_ready(Duration::from_secs(3)));

    let found = wait_for("the fake track", Duration::from_secs(5), || {
        watcher.now_playing().is_some()
    });
    let now = watcher.now_playing().expect("now playing");
    assert!(found, "no now_playing: {now:?}");
    assert_eq!(now.player, "org.mpris.MediaPlayer2.t5bfake");
    assert_eq!(now.title.as_deref(), Some("T5b Title"));
    assert_eq!(now.artist.as_deref(), Some("T5b Artist"));
    assert_eq!(now.album.as_deref(), Some("T5b Album"));
    assert_eq!(now.status, "Playing");
    assert_eq!(now.native_rate, Some(96_000));
    assert_eq!(now.native_bits, Some(24));

    // A pause keeps the track, with the new status.
    state.lock().unwrap().status = "Paused".into();
    assert!(
        wait_for("paused status", Duration::from_secs(5), || {
            watcher
                .now_playing()
                .is_some_and(|now| now.status == "Paused")
        }),
        "status did not change: {:?}",
        watcher.now_playing()
    );
    assert_eq!(
        watcher.now_playing().and_then(|now| now.title).as_deref(),
        Some("T5b Title")
    );

    // When the player leaves the bus, the watcher forgets it.
    drop(connection);
    assert!(
        wait_for("player removal", Duration::from_secs(5), || {
            watcher.now_playing().is_none()
        }),
        "player was not forgotten: {:?}",
        watcher.now_playing()
    );

    watcher.shutdown();
}
