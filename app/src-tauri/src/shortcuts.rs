//! Volume keys as global shortcuts, through the desktop's GlobalShortcuts
//! portal (`org.freedesktop.portal.GlobalShortcuts`, xdg-desktop-portal).
//! KDE Plasma implements it on Wayland and X11. The portal asks the user to
//! confirm (and may let them pick other keys); the desktop keeps the choice.
//!
//! What a press does is up to the caller: the app hands it to the main
//! window, whose store makes the guarded volume write. This module only
//! talks to the portal, and turns presses and releases into steps
//! (`Repeater`: a held key repeats, but only once the backend has shown it
//! reports releases). A
//! session that is lost (the desktop closes it, the portal restarts) shows
//! as inactive and is registered again when the portal is back.
//!
//! The X11 fallback (no portal) lives in the GUI shell (`app.rs`), on
//! Tauri's global-shortcut plugin.

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zbus::{Connection, MatchRule, MessageStream, OwnedObjectPath, OwnedVariant, Variant};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
/// The desktop file's name: the id the portal files the shortcuts under.
pub const APP_ID: &str = "audivane-dsp-ui";

/// A volume key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Up,
    Down,
    Mute,
}

impl KeyAction {
    pub const ALL: [KeyAction; 3] = [KeyAction::Up, KeyAction::Down, KeyAction::Mute];

    /// The shortcut id registered with the portal.
    pub fn id(self) -> &'static str {
        match self {
            KeyAction::Up => "volume-up",
            KeyAction::Down => "volume-down",
            KeyAction::Mute => "volume-mute",
        }
    }

    pub fn from_id(id: &str) -> Option<KeyAction> {
        KeyAction::ALL.into_iter().find(|a| a.id() == id)
    }

    /// What the desktop's dialog and shortcut settings show.
    pub fn description(self) -> &'static str {
        match self {
            KeyAction::Up => "Audivane DSP: volume up",
            KeyAction::Down => "Audivane DSP: volume down",
            KeyAction::Mute => "Audivane DSP: mute",
        }
    }

    /// The key we ask for (a keysym name, as the shortcuts spec writes them).
    pub fn preferred_trigger(self) -> &'static str {
        match self {
            KeyAction::Up => "XF86AudioRaiseVolume",
            KeyAction::Down => "XF86AudioLowerVolume",
            KeyAction::Mute => "XF86AudioMute",
        }
    }

    pub fn repeats(self) -> bool {
        self != KeyAction::Mute
    }
}

/// Which mechanism carries the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Portal,
    X11,
}

/// One bound shortcut: the key the desktop assigned, as it describes it
/// (empty when it assigned none, e.g. because the key is taken).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Bound {
    pub action: KeyAction,
    pub trigger: String,
}

/// Where the volume keys stand, for the settings view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum KeysState {
    Off,
    /// Asking the desktop; it may be showing a dialog.
    Starting,
    Active {
        via: Backend,
        bound: Vec<Bound>,
    },
    /// The user said no in the desktop's dialog.
    Declined,
    /// No way to register them here, or the desktop refused.
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    Pressed(KeyAction),
    Released(KeyAction),
}

// ------------------------------------------------------------------ repeat

/// First repeat after this long held, then every `REPEAT_EVERY`.
pub const REPEAT_DELAY: Duration = Duration::from_millis(400);
pub const REPEAT_EVERY: Duration = Duration::from_millis(90);

/// Turns presses and releases into volume steps.
///
/// - A held up/down key repeats only after this backend has reported a
///   release at least once; one that never does gets one step per press.
///   A hold of either key keeps going until its release (the ceiling and
///   the max rise still bound volume up).
/// - A second press of the held key before its release (a duplicate) does
///   nothing: no step, and the hold's timing stays as it is.
#[derive(Debug, Default)]
pub struct Repeater {
    releases_seen: bool,
    held: Option<Held>,
}

#[derive(Debug)]
struct Held {
    action: KeyAction,
    next: Instant,
}

impl Repeater {
    /// A key event: returns the step to take now, if any.
    pub fn event(&mut self, e: KeyEvent, now: Instant) -> Option<KeyAction> {
        match e {
            KeyEvent::Pressed(a) => {
                if self.held.as_ref().is_some_and(|h| h.action == a) {
                    // Pressed again before its release: a duplicate.
                    return None;
                }
                self.held = (self.releases_seen && a.repeats()).then(|| Held {
                    action: a,
                    next: now + REPEAT_DELAY,
                });
                Some(a)
            }
            KeyEvent::Released(a) => {
                self.releases_seen = true;
                if self.held.as_ref().is_some_and(|h| h.action == a) {
                    self.held = None;
                }
                None
            }
        }
    }

    /// When the next repeat is due (to sleep until), if one can come.
    pub fn deadline(&self) -> Option<Instant> {
        self.held.as_ref().map(|h| h.next)
    }

    /// Time passed: the repeat step due now, if any.
    pub fn tick(&mut self, now: Instant) -> Option<KeyAction> {
        let h = self.held.as_mut()?;
        if now < h.next {
            return None;
        }
        h.next = now + REPEAT_EVERY;
        Some(h.action)
    }
}

/// Runs a `Repeater` on its own thread: events go in, steps come out
/// through `on_step`. Dropping the sender ends the thread.
pub fn spawn_repeater(
    on_step: impl Fn(KeyAction) + Send + 'static,
) -> std::sync::mpsc::Sender<KeyEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<KeyEvent>();
    std::thread::Builder::new()
        .name("volume-keys".into())
        .spawn(move || {
            let mut r = Repeater::default();
            loop {
                let got = match r.deadline() {
                    Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                    None => rx
                        .recv()
                        .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected),
                };
                match got {
                    Ok(e) => {
                        if let Some(a) = r.event(e, Instant::now()) {
                            on_step(a);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if let Some(a) = r.tick(Instant::now()) {
                            on_step(a);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("spawn the volume-keys thread");
    tx
}

// ------------------------------------------------------------------ portal

/// A running portal session. `stop` (or dropping it) asks its thread to
/// close the session and end; nothing waits for that, so quitting never
/// waits on D-Bus. The desktop keeps the keys it bound for the next session.
pub struct PortalKeys {
    stop: Option<futures_channel::oneshot::Sender<()>>,
}

impl PortalKeys {
    pub fn stop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

impl Drop for PortalKeys {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Why the portal could not be used: no portal at all (a fallback may
/// apply), the user's refusal, a failure to report as it is, or a session
/// lost after it worked (the portal restarted, the desktop closed it).
#[derive(Debug, PartialEq, Eq)]
pub enum PortalError {
    Missing(String),
    Declined,
    Failed(String),
    Lost(String),
}

/// What the settings show while a lost session waits for the portal.
pub fn lost_reason(why: &str) -> String {
    format!(
        "{} The volume keys are inactive until it is back; Audivane registers them again then.",
        capitalize_first(why)
    )
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Start a session on its own thread. `on_state` hears Starting, then
/// Active (or Declined/Unavailable, and `fallback` is called instead when
/// there is no portal); `on_key` hears every press and release. A session
/// lost later shows as Unavailable and is registered again once the portal
/// is back.
pub fn start_portal(
    on_state: impl Fn(KeysState) + Send + 'static,
    on_key: impl Fn(KeyEvent) + Send + 'static,
    fallback: impl FnOnce(String) + Send + 'static,
) -> PortalKeys {
    start_portal_on(None, on_state, on_key, fallback)
}

/// `start_portal` on the bus at `address` (tests), else the session bus.
fn start_portal_on(
    address: Option<String>,
    on_state: impl Fn(KeysState) + Send + 'static,
    on_key: impl Fn(KeyEvent) + Send + 'static,
    fallback: impl FnOnce(String) + Send + 'static,
) -> PortalKeys {
    use crate::dbus_util::{CALL_TIMEOUT, timeout};
    let (stop_tx, stop_rx) = futures_channel::oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("shortcuts-portal".into())
        .spawn(move || {
            on_state(KeysState::Starting);
            let session: Mutex<Option<OwnedObjectPath>> = Mutex::new(None);
            let (conn, r) = zbus::block_on(async {
                let connect = async {
                    match &address {
                        Some(a) => zbus::conn::Builder::address(a.as_str()).build().await,
                        None => Connection::session().await,
                    }
                };
                let conn = match timeout(CALL_TIMEOUT, connect).await {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        return (
                            None,
                            Err(PortalError::Missing(format!("no session bus ({e})"))),
                        );
                    }
                    None => {
                        return (
                            None,
                            Err(PortalError::Missing(
                                "the session bus did not answer".into(),
                            )),
                        );
                    }
                };
                let run = Box::pin(portal_loop(&conn, &session, &on_state, &on_key));
                let r = match select(run, stop_rx).await {
                    Either::Left((r, _)) => r,
                    Either::Right(_) => Ok(()),
                };
                (Some(conn), r)
            });
            // Close the session so the desktop stops sending (best effort,
            // bounded: a portal that hangs cannot keep this thread).
            let path = session.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let (Some(conn), Some(path)) = (conn, path) {
                zbus::block_on(async {
                    let _ = timeout(
                        CALL_TIMEOUT,
                        conn.call_method(
                            Some(DEST),
                            path.as_str(),
                            Some("org.freedesktop.portal.Session"),
                            "Close",
                            &(),
                        ),
                    )
                    .await;
                });
            }
            match r {
                Ok(()) => {}
                Err(PortalError::Missing(why)) => fallback(why),
                Err(PortalError::Declined) => on_state(KeysState::Declined),
                Err(PortalError::Failed(why)) | Err(PortalError::Lost(why)) => {
                    on_state(KeysState::Unavailable { reason: why })
                }
            }
        })
        .expect("spawn the shortcuts thread");
    PortalKeys {
        stop: Some(stop_tx),
    }
}

/// Register, and after a loss wait for the portal and register again.
async fn portal_loop(
    conn: &Connection,
    session_slot: &Mutex<Option<OwnedObjectPath>>,
    on_state: &(impl Fn(KeysState) + Send),
    on_key: &(impl Fn(KeyEvent) + Send),
) -> Result<(), PortalError> {
    let mut registered_once = false;
    loop {
        let r = run_portal(conn, session_slot, on_state, on_key, &mut registered_once).await;
        let why = match r {
            Err(PortalError::Lost(why)) => why,
            // The portal came back without its shortcuts interface (yet).
            Err(PortalError::Missing(why)) if registered_once => why,
            other => return other,
        };
        *session_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        on_state(KeysState::Unavailable {
            reason: lost_reason(&why),
        });
        // Not straight back: a portal that is up but refuses would spin.
        let _ = crate::dbus_util::sleep(Duration::from_secs(1)).await;
        crate::dbus_util::wait_for_owner(conn, DEST)
            .await
            .map_err(|e| PortalError::Failed(e.to_string()))?;
        on_state(KeysState::Starting);
    }
}

type Results = HashMap<String, OwnedVariant>;

async fn run_portal(
    conn: &Connection,
    session_slot: &Mutex<Option<OwnedObjectPath>>,
    on_state: &(impl Fn(KeysState) + Send),
    on_key: &(impl Fn(KeyEvent) + Send),
    registered_once: &mut bool,
) -> Result<(), PortalError> {
    // A host (not sandboxed) app tells the portal its id first, so the
    // desktop files the shortcuts under it. Older portals lack this
    // interface; they guess the id themselves.
    let _ = conn
        .call_method(
            Some(DEST),
            PATH,
            Some("org.freedesktop.host.portal.Registry"),
            "Register",
            &(APP_ID, HashMap::<&str, Variant>::new()),
        )
        .await;

    // Is there a GlobalShortcuts portal at all?
    conn.call_method(
        Some(DEST),
        PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(IFACE, "version"),
    )
    .await
    .map_err(|e| PortalError::Missing(missing_reason(&e)))?;

    // Every Request's Response, subscribed before any call so none is missed.
    let responses = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .member("Response")
        .build()
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    let mut responses = MessageStream::for_match_rule(responses, conn, None)
        .await
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    let keys = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(IFACE)
        .build()
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    let keys = MessageStream::for_match_rule(keys, conn, None)
        .await
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    // The session closed by the desktop, or the portal itself gone.
    let closed = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Session")
        .member("Closed")
        .build()
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    let closed = MessageStream::for_match_rule(closed, conn, None)
        .await
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    let owner = crate::dbus_util::owner_changes(conn, DEST)
        .await
        .map_err(|e| PortalError::Failed(e.to_string()))?;
    let mut events =
        futures_util::stream::select(keys, futures_util::stream::select(closed, owner));

    let mut opts: HashMap<&str, Variant> = HashMap::new();
    opts.insert("handle_token", Variant::from(token("create")));
    opts.insert("session_handle_token", Variant::from(token("session")));
    let request: OwnedObjectPath = call(conn, "CreateSession", &(opts,)).await?;
    let results = response(&mut responses, &request).await?;
    let session: OwnedObjectPath = results
        .get("session_handle")
        .and_then(|v| {
            // A string in the spec; some portals send an object path.
            String::try_from(v.try_clone().ok()?)
                .ok()
                .and_then(|s| OwnedObjectPath::try_from(s).ok())
                .or_else(|| OwnedObjectPath::try_from(v.try_clone().ok()?).ok())
        })
        .ok_or_else(|| PortalError::Failed("the portal did not open a session".into()))?;
    *session_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());

    // BindShortcuts: the desktop may show its dialog now.
    let shortcuts: Vec<(&str, HashMap<&str, Variant>)> = KeyAction::ALL
        .into_iter()
        .map(|a| {
            let mut m: HashMap<&str, Variant> = HashMap::new();
            m.insert("description", Variant::from(a.description()));
            m.insert("preferred_trigger", Variant::from(a.preferred_trigger()));
            (a.id(), m)
        })
        .collect();
    let mut opts: HashMap<&str, Variant> = HashMap::new();
    opts.insert("handle_token", Variant::from(token("bind")));
    let request: OwnedObjectPath =
        call(conn, "BindShortcuts", &(&session, shortcuts, "", opts)).await?;
    let results = response(&mut responses, &request).await?;
    *registered_once = true;
    on_state(KeysState::Active {
        via: Backend::Portal,
        bound: bound_from(&results),
    });

    // Presses and releases until stopped, the session is closed or the
    // portal goes away.
    while let Some(msg) = events.next().await {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        let member = header
            .member()
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        match portal_signal(
            &member,
            header.interface().map(|i| i.as_str()),
            header.path().map(|p| p.as_str()),
            session.as_str(),
            crate::dbus_util::new_owner(&msg).as_deref(),
        ) {
            Some(Loss::SessionClosed) => {
                return Err(PortalError::Lost(
                    "the desktop closed the shortcuts session.".into(),
                ));
            }
            Some(Loss::PortalGone) => {
                return Err(PortalError::Lost(
                    "the desktop's portal service stopped.".into(),
                ));
            }
            None => {}
        }
        match member.as_str() {
            "Activated" | "Deactivated" => {
                let Ok((s, id, _ts, _opts)) = msg
                    .body()
                    .deserialize::<(OwnedObjectPath, String, u64, Results)>()
                else {
                    continue;
                };
                if s != session {
                    continue;
                }
                if let Some(a) = KeyAction::from_id(&id) {
                    on_key(if member == "Activated" {
                        KeyEvent::Pressed(a)
                    } else {
                        KeyEvent::Released(a)
                    });
                }
            }
            "ShortcutsChanged" => {
                if let Ok((s, list)) = msg
                    .body()
                    .deserialize::<(OwnedObjectPath, Vec<(String, Results)>)>()
                    && s == session
                {
                    on_state(KeysState::Active {
                        via: Backend::Portal,
                        bound: bound_list(&list),
                    });
                }
            }
            _ => {}
        }
    }
    Err(PortalError::Lost("the bus connection closed.".into()))
}

#[derive(Debug, PartialEq, Eq)]
enum Loss {
    SessionClosed,
    PortalGone,
}

/// Whether a signal on the session's watch means the session is lost.
fn portal_signal(
    member: &str,
    interface: Option<&str>,
    path: Option<&str>,
    session: &str,
    new_owner: Option<&str>,
) -> Option<Loss> {
    match (interface, member) {
        (Some("org.freedesktop.portal.Session"), "Closed") if path == Some(session) => {
            Some(Loss::SessionClosed)
        }
        (Some("org.freedesktop.DBus"), "NameOwnerChanged") if new_owner == Some("") => {
            Some(Loss::PortalGone)
        }
        _ => None,
    }
}

async fn call<B>(conn: &Connection, method: &str, body: &B) -> Result<OwnedObjectPath, PortalError>
where
    B: serde::Serialize + zbus::DynamicType,
{
    let reply = conn
        .call_method(Some(DEST), PATH, Some(IFACE), method, body)
        .await
        .map_err(|e| PortalError::Failed(format!("{method}: {e}")))?;
    reply
        .body()
        .deserialize::<OwnedObjectPath>()
        .map_err(|e| PortalError::Failed(format!("{method}: {e}")))
}

/// Wait for the Response of `request`: 0 success, 1 cancelled by the user.
async fn response(
    stream: &mut MessageStream,
    request: &OwnedObjectPath,
) -> Result<Results, PortalError> {
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        if msg.header().path().map(|p| p.as_str()) != Some(request.as_str()) {
            continue;
        }
        let (code, results) = msg
            .body()
            .deserialize::<(u32, Results)>()
            .map_err(|e| PortalError::Failed(e.to_string()))?;
        return match code {
            0 => Ok(results),
            1 => Err(PortalError::Declined),
            _ => Err(PortalError::Failed(
                "the desktop refused the shortcuts".into(),
            )),
        };
    }
    Err(PortalError::Failed("the portal went away".into()))
}

fn bound_from(results: &Results) -> Vec<Bound> {
    results
        .get("shortcuts")
        .and_then(|v| Vec::<(String, Results)>::try_from(v.try_clone().ok()?).ok())
        .map(|list| bound_list(&list))
        .unwrap_or_default()
}

fn bound_list(list: &[(String, Results)]) -> Vec<Bound> {
    list.iter()
        .filter_map(|(id, props)| {
            let action = KeyAction::from_id(id)?;
            let trigger = props
                .get("trigger_description")
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
                .unwrap_or_default();
            Some(Bound { action, trigger })
        })
        .collect()
}

/// A request token: letters, digits and underscores only.
fn token(what: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    format!(
        "avdsp_{what}_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn missing_reason(e: &zbus::Error) -> String {
    let s = e.to_string();
    if s.contains("ServiceUnknown") || s.contains("was not provided") {
        "no desktop portal is running".into()
    } else {
        "the desktop portal has no global shortcuts".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// Every step a hold gives within `secs` seconds of ticks.
    fn run(r: &mut Repeater, start: Instant, secs: u64) -> Vec<KeyAction> {
        let mut out = Vec::new();
        let mut t = start;
        while t < start + Duration::from_secs(secs) {
            t += Duration::from_millis(10);
            if let Some(a) = r.tick(t) {
                out.push(a);
            }
        }
        out
    }

    #[test]
    fn a_press_steps_once_and_holds_repeat_only_after_a_release_was_seen() {
        let now = t0();
        let mut r = Repeater::default();
        assert_eq!(
            r.event(KeyEvent::Pressed(KeyAction::Up), now),
            Some(KeyAction::Up)
        );
        // No release seen yet: no repeat, however long it is held, and the
        // next press (an autorepeating backend without releases) steps again.
        assert_eq!(r.deadline(), None);
        assert!(run(&mut r, now, 2).is_empty());
        assert_eq!(
            r.event(KeyEvent::Pressed(KeyAction::Up), now),
            Some(KeyAction::Up)
        );
        assert_eq!(r.event(KeyEvent::Released(KeyAction::Up), now), None);
        // Now the backend has shown it reports releases.
        assert_eq!(
            r.event(KeyEvent::Pressed(KeyAction::Down), now),
            Some(KeyAction::Down)
        );
        assert_eq!(r.deadline(), Some(now + REPEAT_DELAY));
        assert_eq!(r.tick(now + Duration::from_millis(100)), None);
        assert_eq!(r.tick(now + REPEAT_DELAY), Some(KeyAction::Down));
        assert_eq!(r.tick(now + REPEAT_DELAY + Duration::from_millis(10)), None);
        assert_eq!(
            r.tick(now + REPEAT_DELAY + REPEAT_EVERY),
            Some(KeyAction::Down)
        );
        r.event(
            KeyEvent::Released(KeyAction::Down),
            now + Duration::from_secs(1),
        );
        assert_eq!(r.tick(now + Duration::from_secs(2)), None);
        assert_eq!(r.deadline(), None);
    }
}
