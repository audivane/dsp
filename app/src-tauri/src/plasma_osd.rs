//! On KDE Plasma the volume display is Plasma's own OSD
//! (`org.kde.plasmashell`, `/org/kde/osdService`), drawn by the shell, so it
//! never takes the focus. For a level with a bar we call
//! `mediaPlayerVolumeChanged(int percent, QString playerName, QString playerIconName)`
//! — signature `(iss)` in Plasma 6's `plasma-workspace` (`shell/osd.h`,
//! `shell/osd.cpp`) — and fall back to `showText(QString icon, QString text)`
//! when the bar method fails (caching that choice for the session) or when no
//! bar applies. Elsewhere, or when there is no osdService, the app shows its
//! own small window instead.

use crate::dbus_util::{CALL_TIMEOUT, timeout};
use audivane_protocol::ipc::{VOLUME_MAX_DB, VOLUME_MIN_DB};
use std::sync::Mutex;
use zbus::Connection;

const SERVICE: &str = "org.kde.plasmashell";
const PATH: &str = "/org/kde/osdService";
const IFACE: &str = "org.kde.osdService";

/// Knee of the perceptual volume taper (matches `app/src/lib/taper.ts::volumeTaper`):
/// `-40 dB..ceiling` takes the upper 80 % of the bar, `min_db..-40 dB` the lower 20 %.
const KNEE_DB: f64 = -40.0;
const KNEE_POS: f64 = 0.2;

/// The session is a Plasma one (`XDG_CURRENT_DESKTOP` lists KDE).
pub fn is_plasma(current_desktop: Option<&str>) -> bool {
    current_desktop.is_some_and(|d| d.split(':').any(|x| x.eq_ignore_ascii_case("KDE")))
}

/// Map a volume level in dB onto `0.0..=1.0` with the exact two-segment
/// perceptual taper and range bounds used by the app's volume sliders
/// (`lib/taper.ts::volumeSliderTaper`, used by TopBar, TrayPopup and Osd):
/// `min_db` (clamped to `[VOLUME_MIN_DB, ..]` and `-inf`) maps to `0.0`,
/// `max_db` (the ceiling, capped at `VOLUME_MAX_DB`) maps to `1.0`, and when
/// `-40 dB` lies strictly between them, `-40..ceiling` takes the top 80 %.
pub fn db_to_fraction(db: f64, min_db: f64, max_db: f64) -> f64 {
    if db.is_nan() {
        return 0.0;
    }
    if db.is_infinite() {
        return if db.is_sign_negative() { 0.0 } else { 1.0 };
    }
    let base_lo = min_db.max(VOLUME_MIN_DB);
    let lo = base_lo.min(db.max(VOLUME_MIN_DB));
    let hi = max_db.min(VOLUME_MAX_DB);
    let min = lo.min(hi);
    let max = lo.max(hi);
    if (max - min).abs() < 1e-12 {
        return if db >= max { 1.0 } else { 0.0 };
    }
    let v = db.clamp(min, max);
    let pos = if min < KNEE_DB && KNEE_DB < max {
        if v <= KNEE_DB {
            KNEE_POS * (v - min) / (KNEE_DB - min)
        } else {
            KNEE_POS + (1.0 - KNEE_POS) * (v - KNEE_DB) / (max - KNEE_DB)
        }
    } else {
        (v - min) / (max - min)
    };
    pos.clamp(0.0, 1.0)
}

/// Perceptual bar percentage (`0..=100`) for a level in dB.
pub fn db_to_percent(db: f64, min_db: f64, max_db: f64) -> i32 {
    (db_to_fraction(db, min_db, max_db) * 100.0)
        .round()
        .clamp(0.0, 100.0) as i32
}

/// What Plasma's OSD should show for a volume change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    pub icon: &'static str,
    pub label: String,
    /// `1..=100` for a bar (`mediaPlayerVolumeChanged`), or `None` for
    /// `showText` (muted, 0 %, a note, or no level).
    pub percent: Option<i32>,
}

fn format_db(db: f64) -> String {
    if !db.is_finite() {
        return if db.is_sign_negative() {
            "\u{2212}\u{221e} dB".into()
        } else {
            "\u{221e} dB".into()
        };
    }
    let v = if db.abs() < 0.05 { 0.0 } else { db };
    format!("{v:.1} dB").replace('-', "\u{2212}")
}

/// Plasma's icon, label and bar percent for a level: dB with a real minus
/// sign (`"Audivane DSP · −3.0 dB"`), `"Audivane DSP · muted"` when muted, or
/// the note instead of a level.
pub fn format(
    volume_db: Option<f64>,
    muted: bool,
    min_db: f64,
    max_db: f64,
    note: Option<&str>,
) -> Display {
    let fraction = volume_db.map_or(0.0, |db| db_to_fraction(db, min_db, max_db));
    let icon = if muted {
        "audio-volume-muted"
    } else if fraction < 0.34 {
        "audio-volume-low"
    } else if fraction < 0.67 {
        "audio-volume-medium"
    } else {
        "audio-volume-high"
    };
    if let Some(n) = note.filter(|n| !n.is_empty()) {
        return Display {
            icon,
            label: format!("Audivane DSP · {n}"),
            percent: None,
        };
    }
    if muted {
        return Display {
            icon,
            label: "Audivane DSP · muted".into(),
            percent: None,
        };
    }
    let Some(db) = volume_db else {
        return Display {
            icon,
            label: "Audivane DSP · —".into(),
            percent: None,
        };
    };
    let pct = (fraction * 100.0).round().clamp(0.0, 100.0) as i32;
    Display {
        icon,
        label: format!("Audivane DSP · {}", format_db(db)),
        // Plasma 6's Osd::mediaPlayerVolumeChanged treats percent == 0 as muted
        // and rewrites the text to "<playerName> Muted", so 0 % goes through
        // showText directly.
        percent: (pct > 0).then_some(pct),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Bar,
    TextOnly,
}

struct SessionState {
    conn: Option<Connection>,
    method: Method,
}

struct Client {
    address: Option<String>,
    state: Mutex<SessionState>,
}

impl Client {
    const fn session() -> Self {
        Self {
            address: None,
            state: Mutex::new(SessionState {
                conn: None,
                method: Method::Bar,
            }),
        }
    }

    #[cfg(test)]
    fn with_address(address: &str) -> Self {
        Self {
            address: Some(address.to_string()),
            state: Mutex::new(SessionState {
                conn: None,
                method: Method::Bar,
            }),
        }
    }

    fn show(&self, display: &Display) -> bool {
        zbus::block_on(self.show_async(display))
    }

    async fn show_async(&self, display: &Display) -> bool {
        let (cached_conn, method) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (guard.conn.clone(), guard.method)
        };
        let conn = match cached_conn {
            Some(c) => c,
            None => {
                let connect = async {
                    match &self.address {
                        Some(a) => zbus::conn::Builder::address(a.as_str()).build().await,
                        None => Connection::session().await,
                    }
                };
                let Some(Ok(c)) = timeout(CALL_TIMEOUT, connect).await else {
                    return false;
                };
                self.state.lock().unwrap_or_else(|e| e.into_inner()).conn = Some(c.clone());
                c
            }
        };

        if let Some(percent) = display.percent
            && method == Method::Bar
        {
            match timeout(
                CALL_TIMEOUT,
                conn.call_method(
                    Some(SERVICE),
                    PATH,
                    Some(IFACE),
                    "mediaPlayerVolumeChanged",
                    &(percent, display.label.as_str(), display.icon),
                ),
            )
            .await
            {
                Some(Ok(_)) => return true,
                None => return false,
                Some(Err(err)) if service_absent(&err) => return false,
                Some(Err(_)) => {
                    let ok = matches!(
                        timeout(
                            CALL_TIMEOUT,
                            conn.call_method(
                                Some(SERVICE),
                                PATH,
                                Some(IFACE),
                                "showText",
                                &(display.icon, display.label.as_str()),
                            ),
                        )
                        .await,
                        Some(Ok(_))
                    );
                    if ok {
                        self.state.lock().unwrap_or_else(|e| e.into_inner()).method =
                            Method::TextOnly;
                    }
                    return ok;
                }
            }
        }

        matches!(
            timeout(
                CALL_TIMEOUT,
                conn.call_method(
                    Some(SERVICE),
                    PATH,
                    Some(IFACE),
                    "showText",
                    &(display.icon, display.label.as_str()),
                ),
            )
            .await,
            Some(Ok(_))
        )
    }
}

/// No `org.kde.plasmashell` or `/org/kde/osdService` on the bus at all:
/// `showText` would fail too, and we should not cache `TextOnly`.
fn service_absent(err: &zbus::Error) -> bool {
    matches!(
        err,
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.ServiceUnknown"
                    | "org.freedesktop.DBus.Error.NameHasNoOwner"
                    | "org.freedesktop.DBus.Error.UnknownObject"
            )
    )
}

static SESSION: Client = Client::session();

/// Show `display` in Plasma's OSD. Blocks up to about 2 s: call it off the UI
/// thread. False when Plasma did not take it (then show our own window).
pub fn show(display: &Display) -> bool {
    SESSION.show(display)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus_util::test_bus::private_bus;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn plasma_is_detected_from_the_desktop() {
        assert!(is_plasma(Some("KDE")));
        assert!(is_plasma(Some("X-Generic:KDE")));
        assert!(!is_plasma(Some("GNOME")));
        assert!(!is_plasma(None));
    }

    #[test]
    fn volume_taper_maps_db_to_percent() {
        // Default -80..0 dB range: -40..0 takes the top 80 %.
        assert_eq!(db_to_percent(f64::NEG_INFINITY, -80.0, 0.0), 0);
        assert_eq!(db_to_percent(-120.0, -80.0, 0.0), 0);
        assert_eq!(db_to_percent(-80.0, -80.0, 0.0), 0);
        assert_eq!(db_to_percent(-60.0, -80.0, 0.0), 10);
        assert_eq!(db_to_percent(-40.0, -80.0, 0.0), 20);
        assert_eq!(db_to_percent(-20.0, -80.0, 0.0), 60);
        assert_eq!(db_to_percent(-3.0, -80.0, 0.0), 94);
        assert_eq!(db_to_percent(0.0, -80.0, 0.0), 100);
        assert_eq!(db_to_percent(6.0, -80.0, 0.0), 100);

        // Ceiling (max_volume_db) is 100 %, knee stays at -40 dB (20 %).
        assert_eq!(db_to_percent(-10.0, -80.0, -10.0), 100);
        assert_eq!(db_to_percent(-25.0, -80.0, -10.0), 60);
        assert_eq!(db_to_percent(-40.0, -80.0, -10.0), 20);
        assert_eq!(db_to_percent(-80.0, -80.0, -10.0), 0);

        // Ceiling at or below the -40 dB knee still reaches 100 % at the ceiling.
        assert_eq!(db_to_percent(-50.0, -80.0, -50.0), 100);
        assert_eq!(db_to_percent(-65.0, -80.0, -50.0), 50);
        assert_eq!(db_to_percent(-80.0, -80.0, -50.0), 0);

        // Deeper engine floor (-120 dB) keeps the knee at -40 dB (20 %).
        assert_eq!(db_to_percent(-120.0, -120.0, 0.0), 0);
        assert_eq!(db_to_percent(-80.0, -120.0, 0.0), 10);
        assert_eq!(db_to_percent(-40.0, -120.0, 0.0), 20);
        assert_eq!(db_to_percent(0.0, -120.0, 0.0), 100);

        // Monotonic across the range.
        let mut prev = 0;
        for tenths in -800..=0 {
            let pct = db_to_percent(tenths as f64 / 10.0, -80.0, 0.0);
            assert!(pct >= prev, "non-monotonic at {} dB", tenths as f64 / 10.0);
            prev = pct;
        }
        assert_eq!(prev, 100);
    }

    #[test]
    fn taper_matches_shared_test_vector_with_frontend() {
        #[derive(serde::Deserialize)]
        struct Vector {
            min_db: f64,
            max_db: f64,
            db: f64,
            pos: f64,
            percent: i32,
        }
        let vectors: Vec<Vector> =
            serde_json::from_str(include_str!("../../src/lib/__fixtures__/volume-taper.json"))
                .expect("valid shared volume taper test vectors");
        assert!(!vectors.is_empty());
        for v in vectors {
            let frac = db_to_fraction(v.db, v.min_db, v.max_db);
            assert!(
                (frac - v.pos).abs() < 1e-9,
                "fraction mismatch for {} dB in [{}, {}]: got {}, expected {}",
                v.db,
                v.min_db,
                v.max_db,
                frac,
                v.pos
            );
            assert_eq!(
                db_to_percent(v.db, v.min_db, v.max_db),
                v.percent,
                "percent mismatch for {} dB in [{}, {}]",
                v.db,
                v.min_db,
                v.max_db
            );
        }
    }

    #[test]
    fn formats_bar_muted_floor_and_note() {
        assert_eq!(
            format(Some(-3.0), false, -80.0, 0.0, None),
            Display {
                icon: "audio-volume-high",
                label: "Audivane DSP · \u{2212}3.0 dB".into(),
                percent: Some(94),
            }
        );
        assert_eq!(
            format(Some(-23.45), false, -80.0, 0.0, None),
            Display {
                icon: "audio-volume-medium",
                label: "Audivane DSP · \u{2212}23.4 dB".into(),
                percent: Some(53),
            }
        );
        assert_eq!(
            format(Some(-0.01), false, -80.0, 0.0, None),
            Display {
                icon: "audio-volume-high",
                label: "Audivane DSP · 0.0 dB".into(),
                percent: Some(100),
            }
        );
        // Ceiling becomes 100 %.
        assert_eq!(
            format(Some(-12.0), false, -80.0, -12.0, None),
            Display {
                icon: "audio-volume-high",
                label: "Audivane DSP · \u{2212}12.0 dB".into(),
                percent: Some(100),
            }
        );
        // Muted: text-only with "Audivane DSP · muted".
        assert_eq!(
            format(Some(-6.0), true, -80.0, 0.0, None),
            Display {
                icon: "audio-volume-muted",
                label: "Audivane DSP · muted".into(),
                percent: None,
            }
        );
        // 0 % unmuted goes through showText so Plasma does not append "Muted".
        assert_eq!(
            format(Some(-80.0), false, -80.0, 0.0, None),
            Display {
                icon: "audio-volume-low",
                label: "Audivane DSP · \u{2212}80.0 dB".into(),
                percent: None,
            }
        );
        // Note: text-only.
        assert_eq!(
            format(
                None,
                false,
                -80.0,
                0.0,
                Some("Bit-perfect: set the volume on the DAC")
            ),
            Display {
                icon: "audio-volume-low",
                label: "Audivane DSP · Bit-perfect: set the volume on the DAC".into(),
                percent: None,
            }
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Bar(i32, String, String),
        Text(String, String),
    }

    struct FakeFullOsd {
        seen: std::sync::mpsc::Sender<Call>,
    }

    #[zbus::interface(name = "org.kde.osdService")]
    impl FakeFullOsd {
        #[zbus(name = "mediaPlayerVolumeChanged")]
        async fn media_player_volume_changed(
            &self,
            percent: i32,
            player_name: String,
            player_icon_name: String,
        ) {
            let _ = self
                .seen
                .send(Call::Bar(percent, player_name, player_icon_name));
        }

        #[zbus(name = "showText")]
        async fn show_text(&self, icon: String, text: String) {
            let _ = self.seen.send(Call::Text(icon, text));
        }
    }

    struct FakeTextOnlyOsd {
        seen: std::sync::mpsc::Sender<Call>,
        bar_attempts: Arc<AtomicUsize>,
    }

    #[zbus::interface(name = "org.kde.osdService")]
    impl FakeTextOnlyOsd {
        #[zbus(name = "mediaPlayerVolumeChanged")]
        async fn media_player_volume_changed(
            &self,
            _percent: i32,
            _player_name: String,
            _player_icon_name: String,
        ) -> zbus::fdo::Result<()> {
            self.bar_attempts.fetch_add(1, Ordering::SeqCst);
            Err(zbus::fdo::Error::UnknownMethod(
                "mediaPlayerVolumeChanged not available".into(),
            ))
        }

        #[zbus(name = "showText")]
        async fn show_text(&self, icon: String, text: String) {
            let _ = self.seen.send(Call::Text(icon, text));
        }
    }

    struct FakeShowTextOnlyOsd {
        seen: std::sync::mpsc::Sender<Call>,
    }

    #[zbus::interface(name = "org.kde.osdService")]
    impl FakeShowTextOnlyOsd {
        #[zbus(name = "showText")]
        async fn show_text(&self, icon: String, text: String) {
            let _ = self.seen.send(Call::Text(icon, text));
        }
    }

    #[test]
    fn bar_call_muted_and_missing_service() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let client = Client::with_address(&bus.address);
        let bar = format(Some(-3.0), false, -80.0, 0.0, None);
        // No Plasma on the bus: false (the app shows its own window), and
        // the bar method stays enabled once Plasma appears.
        assert!(!client.show(&bar));

        let (tx, rx) = std::sync::mpsc::channel();
        let _plasma = zbus::block_on(
            zbus::conn::Builder::address(bus.address.as_str())
                .serve_at(PATH, FakeFullOsd { seen: tx })
                .name(SERVICE)
                .build(),
        )
        .unwrap();

        assert!(client.show(&bar));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Call::Bar(
                94,
                "Audivane DSP · \u{2212}3.0 dB".into(),
                "audio-volume-high".into()
            )
        );

        // Muted uses showText with "Audivane DSP · muted" and does not disable bar calls.
        let muted = format(Some(-3.0), true, -80.0, 0.0, None);
        assert!(client.show(&muted));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Call::Text("audio-volume-muted".into(), "Audivane DSP · muted".into())
        );

        let low = format(Some(-20.0), false, -80.0, 0.0, None);
        assert!(client.show(&low));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Call::Bar(
                60,
                "Audivane DSP · \u{2212}20.0 dB".into(),
                "audio-volume-medium".into()
            )
        );
    }

    #[test]
    fn falls_back_to_show_text_and_caches_the_choice() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let bar_attempts = Arc::new(AtomicUsize::new(0));
        let _plasma = zbus::block_on(
            zbus::conn::Builder::address(bus.address.as_str())
                .serve_at(
                    PATH,
                    FakeTextOnlyOsd {
                        seen: tx,
                        bar_attempts: bar_attempts.clone(),
                    },
                )
                .name(SERVICE)
                .build(),
        )
        .unwrap();

        let client = Client::with_address(&bus.address);
        let first = format(Some(-6.0), false, -80.0, 0.0, None);
        assert!(client.show(&first));
        assert_eq!(bar_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Call::Text(
                "audio-volume-high".into(),
                "Audivane DSP · \u{2212}6.0 dB".into()
            )
        );

        // Cached: later bar calls go straight to showText without retrying
        // mediaPlayerVolumeChanged.
        let second = format(Some(-12.0), false, -80.0, 0.0, None);
        assert!(client.show(&second));
        assert_eq!(bar_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Call::Text(
                "audio-volume-high".into(),
                "Audivane DSP · \u{2212}12.0 dB".into()
            )
        );
    }

    #[test]
    fn falls_back_when_bar_method_is_absent_from_interface() {
        let Some(bus) = private_bus() else {
            eprintln!("dbus-daemon not available: skipped");
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let _plasma = zbus::block_on(
            zbus::conn::Builder::address(bus.address.as_str())
                .serve_at(PATH, FakeShowTextOnlyOsd { seen: tx })
                .name(SERVICE)
                .build(),
        )
        .unwrap();

        let client = Client::with_address(&bus.address);
        let display = format(Some(-3.0), false, -80.0, 0.0, None);
        assert!(client.show(&display));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Call::Text(
                "audio-volume-high".into(),
                "Audivane DSP · \u{2212}3.0 dB".into()
            )
        );
        assert_eq!(
            client.state.lock().unwrap().method,
            Method::TextOnly,
            "fallback should cache TextOnly"
        );
    }
}
