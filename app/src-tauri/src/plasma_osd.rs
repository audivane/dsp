//! On KDE Plasma the volume display is Plasma's own OSD
//! (`org.kde.plasmashell`, `/org/kde/osdService`), drawn by the shell, so it
//! never takes the focus. For a level with a bar we call
//! `mediaPlayerVolumeChanged(int percent, QString playerName, QString playerIconName)`
//! (signature `(iss)` in Plasma 6's `plasma-workspace`, `shell/osd.h`,
//! `shell/osd.cpp`), and fall back to `showText(QString icon, QString text)`
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
            label: "Audivane DSP · -".into(),
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
