//! Tauri shell: commands for the webviews, the event channel, the tray
//! (icon, menu, volume popup), the volume keys and on-screen display,
//! close-to-tray, single instance and remembered window size.
//!
//! Three webviews load the same frontend and pick their view by window
//! label: `main` (the app), `popup` (the tray's volume popup) and `osd` (the
//! on-screen volume display). Each has its own store and event channel; the
//! main one also gets meters and spectrum.
//!
//! Each webview is its own WebKit web process and is not kept while nobody
//! looks at it: closing the main window to the tray destroys it (and drops the
//! meters and spectrum topics), showing it makes it again; the popup and our
//! OSD window are made when first needed and destroyed [`IDLE_FREE`] after
//! they were last hidden. While the main window is gone the volume keys and
//! the tray wheel are handled here (`volume_step`) instead of by its store.

use crate::client::{CallError, DaemonClient, PathFn, SubMsg, Subscription};
use crate::shortcuts::{self, KeyAction, KeyEvent, KeysState, PortalKeys};
use crate::sni_watch::{self, TrayHost};
use crate::tray::{TrayHandle, TrayModel};
use crate::ui_settings::{self, OsdMode, TrayClick, UiSettings};
use crate::wheel::{self, WheelAccumulator};
use crate::window_life::{Closing, IDLE_FREE, IdleReaper};
use crate::{
    appimage, autostart, eq, irinfo, overlays, paths, placement, plasma_osd, relaunch, systemd,
    tray_text, volume_step,
};
use audivane_protocol::Band;
use audivane_protocol::ipc::Status;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, RunEvent, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

const MAIN: &str = "main";
const POPUP: &str = "popup";
const OSD: &str = "osd";
/// Popup and OSD sizes (logical pixels; the views are laid out for them).
const POPUP_SIZE: (f64, f64) = (320.0, 184.0);
const OSD_SIZE: (f64, f64) = (280.0, 104.0);
/// How long the OSD stays up after the last change.
const OSD_SHOW: Duration = Duration::from_millis(1200);
/// How long a closing main window waits for its page to hand over its
/// place (scroll position) before it is destroyed anyway.
const MAIN_CLOSE_WAIT: Duration = Duration::from_millis(500);

/// A show request kept while its window is being destroyed (`Closing`),
/// replayed once it is gone.
#[derive(Debug)]
enum ShowRequest {
    Main(Option<String>),
    Popup(Option<(i32, i32)>, Option<String>),
    Osd(Value, u64),
}

/// Meter frames for the webview: at most one delivery outstanding.
///
/// `Channel::send` is not acknowledged, so a webview that stops processing
/// messages (suspended while minimized, say) would let 30 Hz frames pile up.
/// Instead one frame goes out and the next only once the webview says it has
/// drawn it (`meters_ack`); frames arriving meanwhile replace each other.
#[derive(Default)]
struct MeterMailbox {
    /// The newest frame not yet sent.
    latest: Option<Value>,
    /// A frame was sent and not yet acknowledged.
    outstanding: bool,
}

impl MeterMailbox {
    /// A new frame: returns it when it may go out now, else keeps it.
    fn offer(&mut self, frame: Value) -> Option<Value> {
        if self.outstanding {
            self.latest = Some(frame);
            None
        } else {
            self.outstanding = true;
            Some(frame)
        }
    }

    /// The webview took the last frame: returns the next one to send, if any.
    fn ack(&mut self) -> Option<Value> {
        self.outstanding = false;
        let next = self.latest.take()?;
        self.outstanding = true;
        Some(next)
    }
}

/// Forwards daemon events to the webview and remembers the latest of each
/// (except meters and spectrum) so a reloaded webview gets a snapshot at once.
#[derive(Default)]
struct Hub {
    /// One channel per webview (by window label).
    channels: Mutex<BTreeMap<String, Channel<Value>>>,
    connection: Mutex<Option<Value>>,
    last: Mutex<BTreeMap<String, Value>>,
    meters: Mutex<MeterMailbox>,
    spectrum: Mutex<MeterMailbox>,
    /// The window is shown (not hidden to the tray).
    visible: AtomicBool,
    /// The window is minimized.
    minimized: AtomicBool,
    /// The tray could not be created, so closing the window quits.
    no_tray: AtomicBool,
}

impl Hub {
    /// To every webview.
    fn send(&self, msg: Value) {
        let mut chans = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        chans.retain(|_, ch| ch.send(msg.clone()).is_ok());
    }

    /// Meters and spectrum: the main window only.
    fn send_main(&self, msg: Value) {
        let mut chans = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ch) = chans.get(MAIN)
            && ch.send(msg).is_err()
        {
            chans.remove(MAIN);
        }
    }

    /// Meters and spectrum are only worth sending while someone can see them.
    fn meters_wanted(&self) -> bool {
        self.visible.load(Ordering::Relaxed) && !self.minimized.load(Ordering::Relaxed)
    }

    /// The window was shown, hidden, minimized or restored.
    fn set_shown(&self, visible: bool, minimized: bool) {
        self.visible.store(visible, Ordering::Relaxed);
        self.minimized.store(minimized, Ordering::Relaxed);
        if !self.meters_wanted() {
            // Nothing stale waits for the window to come back.
            self.meters.lock().unwrap_or_else(|e| e.into_inner()).latest = None;
            self.spectrum
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .latest = None;
        }
    }

    /// The webview has drawn the last meter frame: send the newest, if any.
    fn meters_ack(&self) {
        let mut mailbox = self.meters.lock().unwrap_or_else(|e| e.into_inner());
        if !self.meters_wanted() {
            mailbox.latest = None;
        }
        // Sent under the mailbox lock, so there is never more than one in flight.
        if let Some(frame) = mailbox.ack() {
            self.send_main(frame);
        }
    }

    /// The webview has drawn the last spectrum frame: send the newest, if any.
    fn spectrum_ack(&self) {
        let mut mailbox = self.spectrum.lock().unwrap_or_else(|e| e.into_inner());
        if !self.meters_wanted() {
            mailbox.latest = None;
        }
        if let Some(frame) = mailbox.ack() {
            self.send_main(frame);
        }
    }

    fn clear_spectrum(&self) {
        self.spectrum
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest = None;
    }

    fn on_message(&self, msg: &SubMsg) {
        match msg {
            SubMsg::Event(v) => {
                let name = v
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut obj = v.as_object().cloned().unwrap_or_default();
                obj.insert("kind".into(), json!("event"));
                if name == "meters" {
                    if !self.meters_wanted() {
                        return;
                    }
                    let mut mailbox = self.meters.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(frame) = mailbox.offer(Value::Object(obj)) {
                        self.send_main(frame);
                    }
                    return;
                }
                if name == "spectrum" {
                    if !self.meters_wanted() {
                        return;
                    }
                    let mut mailbox = self.spectrum.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(frame) = mailbox.offer(Value::Object(obj)) {
                        self.send_main(frame);
                    }
                    return;
                }
                // Everything else always goes out, in order.
                self.last
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(name, v.clone());
                self.send(Value::Object(obj));
            }
            other => {
                let (state, detail) = match other {
                    SubMsg::Connecting => ("connecting", None),
                    SubMsg::Up => ("up", None),
                    SubMsg::Down(why) => ("down", Some(why.clone())),
                    SubMsg::Event(_) => unreachable!(),
                };
                if state != "up" {
                    self.last.lock().unwrap_or_else(|e| e.into_inner()).clear();
                }
                let m = json!({"kind": "connection", "state": state, "detail": detail});
                *self.connection.lock().unwrap_or_else(|e| e.into_inner()) = Some(m.clone());
                self.send(m);
            }
        }
    }

    /// A webview is gone: nothing more is sent to it.
    fn detach(&self, label: &str) {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(label);
    }

    fn attach(&self, label: &str, channel: Channel<Value>) {
        let conn = self
            .connection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let last: Vec<Value> = self
            .last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        if let Some(c) = conn {
            let _ = channel.send(c);
        }
        for v in last {
            let mut obj = v.as_object().cloned().unwrap_or_default();
            obj.insert("kind".into(), json!("event"));
            let _ = channel.send(Value::Object(obj));
        }
        if label == MAIN {
            // A new main page: nothing it was sent before will be acknowledged.
            *self.meters.lock().unwrap_or_else(|e| e.into_inner()) = MeterMailbox::default();
            *self.spectrum.lock().unwrap_or_else(|e| e.into_inner()) = MeterMailbox::default();
        }
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(label.to_string(), channel);
    }
}

/// The volume level last seen in a status event (for the "any change" OSD).
#[derive(Clone, Debug, PartialEq)]
struct SeenLevel {
    instance: Option<String>,
    volume_db: f64,
    muted: bool,
}

#[derive(Default)]
struct Osd {
    /// The last payload shown (a page that loads late asks for it).
    last: Option<Value>,
    /// The page has loaded and listens.
    ready: bool,
    /// Show once the page is ready.
    pending: bool,
    /// When a key or the tray wheel last showed it.
    key_at: Option<Instant>,
    seen: Option<SeenLevel>,
}

#[derive(Default)]
struct Keys {
    portal: Option<PortalKeys>,
    /// X11 grabs are registered (Tauri's global-shortcut plugin).
    x11: bool,
    x11_plugin: bool,
    state: Option<KeysState>,
}

struct Shared {
    client: Arc<DaemonClient>,
    hub: Arc<Hub>,
    tray: Mutex<TrayModel>,
    tray_handle: Mutex<Option<TrayHandle>>,
    subscription: Mutex<Option<Subscription>>,
    spectrum_wanted: AtomicBool,
    settings: Mutex<UiSettings>,
    settings_path: Option<PathBuf>,
    keys: Mutex<Keys>,
    /// Steps from presses and releases (with repeat); also fed by the X11 grabs.
    key_events: Mutex<Option<Sender<KeyEvent>>>,
    osd: Mutex<Osd>,
    osd_gen: AtomicU64,
    /// When the popup last hid because it lost the focus (a click on the
    /// tray icon while it is open lands just after that: it then stays shut).
    popup_hid_at: Mutex<Option<Instant>>,
    popup_shown_at: Mutex<Option<Instant>>,
    main_focused: AtomicBool,
    popup_focused: AtomicBool,
    /// What the StatusNotifier watcher says (is there a tray at all).
    tray_host: Mutex<TrayHost>,
    /// Wheel deltas over the tray icon, gathered into whole notches.
    wheel: Mutex<WheelAccumulator>,
    /// Started with `--hidden` and not shown since.
    hidden_at_start: AtomicBool,
    /// Tray item rebuilds still to finish (a change of the left-click
    /// action replaces the item): the view keeps saying there is a tray
    /// meanwhile.
    tray_respawns: AtomicUsize,
    /// One rebuild at a time, so quick toggles cannot leave two items.
    tray_respawn_lock: Mutex<()>,
    /// What the main page asked to get back when it is made again (its
    /// scroll position); kept here because its webview is destroyed.
    main_view: Mutex<Option<Value>>,
    popup_life: Mutex<IdleReaper>,
    osd_life: Mutex<IdleReaper>,
    /// The newest volume level seen, for volume keys while the main
    /// window is gone.
    levels: Mutex<volume_step::Tracker>,
    /// Those keys, in order, to the thread that writes them.
    native_steps: Mutex<Option<Sender<(KeyAction, f64, String)>>>,
    /// Windows being destroyed, by label, with the show request made meanwhile.
    closing: Mutex<BTreeMap<String, Closing<ShowRequest>>>,
    /// Which close of the main window its time-out belongs to.
    main_close_gen: AtomicU64,
    /// A package upgrade replaced the binary but relaunching into it
    /// failed: the window and the tray ask for a restart.
    update_pending: AtomicBool,
    /// A replaced binary is ready, but the main window had the focus: the
    /// relaunch waits until the user leaves it (`relaunch_when_away`).
    upgrade_waiting: AtomicBool,
    /// An upgrade check is running (one at a time).
    upgrade_checking: AtomicBool,
}

impl Shared {
    /// A tray icon can bring the window back: the watcher reports a host
    /// and our item is up.
    fn tray_live(&self) -> bool {
        self.tray_host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .available
            && self
                .tray_handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
    }

    /// What the settings view reports: a tray host, and our item up or
    /// being replaced right now.
    fn tray_shown(&self) -> bool {
        self.tray_live()
            || (self.tray_respawns.load(Ordering::SeqCst) > 0
                && self
                    .tray_host
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .available)
    }

    /// Close-to-tray follows the tray's liveness.
    fn sync_close_to_tray(&self) {
        self.hub.no_tray.store(!self.tray_live(), Ordering::Relaxed);
    }
}

type AppState = Arc<Shared>;

// ---------------------------------------------------------------- commands

#[tauri::command]
async fn daemon_call(state: State<'_, AppState>, request: Value) -> Result<Value, CallError> {
    let client = state.client.clone();
    tauri::async_runtime::spawn_blocking(move || client.call_request(request))
        .await
        .map_err(|e| CallError {
            code: -32603,
            message: e.to_string(),
        })?
}

#[tauri::command]
fn daemon_subscribe(webview: tauri::Webview, state: State<'_, AppState>, channel: Channel<Value>) {
    state.hub.attach(webview.label(), channel);
}

/// The webview has drawn the meter frame it was sent; it may get the next.
#[tauri::command]
fn meters_ack(state: State<'_, AppState>) {
    state.hub.meters_ack();
}

/// The webview has drawn the spectrum frame it was sent; it may get the next.
#[tauri::command]
fn spectrum_ack(state: State<'_, AppState>) {
    state.hub.spectrum_ack();
}

/// Subscribe to or unsubscribe from `spectrum` events on the daemon.
#[tauri::command]
fn spectrum_enable(state: State<'_, AppState>, enabled: bool) {
    state.spectrum_wanted.store(enabled, Ordering::Relaxed);
    if !enabled {
        state.hub.clear_spectrum();
    }
    if let Some(sub) = state
        .subscription
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        sub.set_spectrum(enabled);
    }
}

#[tauri::command]
async fn start_engine() -> Result<(), CallError> {
    tauri::async_runtime::spawn_blocking(systemd::start_engine)
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map(|_| ()))
        .map_err(|message| CallError { code: 2, message })
}

/// The engine's last error from the journal, when it failed to come up.
#[tauri::command]
async fn engine_last_error() -> Option<String> {
    tauri::async_runtime::spawn_blocking(systemd::last_engine_error)
        .await
        .ok()
        .flatten()
}

/// Whether the engine starts at login; null when systemd cannot say.
#[tauri::command]
async fn engine_at_login() -> Option<bool> {
    tauri::async_runtime::spawn_blocking(systemd::engine_at_login)
        .await
        .ok()
        .and_then(Result::ok)
}

#[tauri::command]
async fn set_engine_at_login(enabled: bool) -> Result<(), CallError> {
    tauri::async_runtime::spawn_blocking(move || systemd::set_engine_at_login(enabled))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r)
        .map_err(|message| CallError { code: 2, message })
}

#[tauri::command]
async fn restart_engine() -> Result<(), CallError> {
    tauri::async_runtime::spawn_blocking(systemd::restart_engine)
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map(|_| ()))
        .map_err(|message| CallError { code: 2, message })
}

/// This app's version, to compare with the engine's (`status.version`).
#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Relaunch the app, e.g. after a package upgrade replaced its binary, so the
/// new process runs the installed code (the window's Restart, or the tray's
/// "Restart to finish the update").
#[tauri::command]
fn restart_app(app: AppHandle) -> Result<(), CallError> {
    let Err(message) = relaunch_now(&app);
    Err(CallError { code: 2, message })
}

/// Where an upgrade of the app stands, for the window's notice: "ready"
/// (it relaunches once the user leaves the window), "failed" (relaunching
/// failed: restart by hand), or nothing.
#[tauri::command]
fn app_update_pending(state: State<'_, AppState>) -> Option<&'static str> {
    if state.update_pending.load(Ordering::Relaxed) {
        Some("failed")
    } else if state.upgrade_waiting.load(Ordering::Relaxed) {
        Some("ready")
    } else {
        None
    }
}

/// Running from the AppImage: what its setup offer needs (null otherwise).
#[tauri::command]
fn appimage_setup() -> Option<appimage::SetupState> {
    appimage::appdir(std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR"))?;
    let home = std::env::var_os("HOME")?;
    Some(appimage::current_state(std::path::Path::new(&home)))
}

/// The AppImage's per-user setup (it starts the engine), then a relaunch
/// into the installed app. Returns only when either failed.
#[tauri::command]
async fn appimage_install(app: AppHandle, login_start: bool) -> Result<(), CallError> {
    let fail = |message: String| CallError { code: 2, message };
    let appdir = appimage::appdir(std::env::var_os("APPIMAGE"), std::env::var_os("APPDIR"))
        .ok_or_else(|| fail("This app is not running from an AppImage.".into()))?;
    let home = std::env::var_os("HOME").ok_or_else(|| fail("HOME is not set.".into()))?;
    tauri::async_runtime::spawn_blocking(move || appimage::install(&appdir, login_start))
        .await
        .map_err(|e| fail(e.to_string()))?
        .map_err(fail)?;
    let exe = appimage::installed_app(std::path::Path::new(&home));
    let (tx, rx) = std::sync::mpsc::channel();
    let a = app.clone();
    app.run_on_main_thread(move || {
        let env: Vec<_> = appimage::APPIMAGE_ENV.iter().map(|k| (*k, None)).collect();
        let Err(e) = relaunch_into(&a, &exe, &env);
        let _ = tx.send(e);
    })
    .map_err(|e| fail(e.to_string()))?;
    let message = tauri::async_runtime::spawn_blocking(move || rx.recv())
        .await
        .map_err(|e| fail(e.to_string()))?
        .unwrap_or_else(|_| "the relaunch did not report back".into());
    Err(fail(format!(
        "Set up, but the app could not restart into the installed copy ({message}). Close it and open Audivane DSP from the app menu."
    )))
}

/// How long the user must have been away from the window (it lost the
/// focus or was closed to the tray) before a waiting relaunch runs: a
/// moment's focus change (to the tray popup, a dialog) is not leaving.
const AWAY_WAIT: Duration = Duration::from_millis(1500);

/// Wait between looks at a replaced binary: a package manager writing it
/// in place changes its size or time meanwhile; relaunch only once it
/// holds still.
const UPGRADE_SETTLE: Duration = Duration::from_secs(3);
/// The fallback look, for an upgrade that did not restart the engine.
const UPGRADE_POLL: Duration = Duration::from_secs(600);

/// Relaunching by itself after an upgrade: release builds from a package (not
/// an AppImage, whose binary sits in its own mount and never changes).
/// `AUDIVANE_SELF_RELAUNCH=1` turns it on in a debug build.
fn self_relaunch_enabled() -> bool {
    std::env::var_os("APPIMAGE").is_none_or(|a| a.is_empty())
        && (!cfg!(debug_assertions) || std::env::var_os("AUDIVANE_SELF_RELAUNCH").is_some())
}

/// Look whether a package upgrade replaced this binary (on the engine coming
/// back, which the post-upgrade scriptlet restarts, and now and then); if so,
/// relaunch into the new one once it holds still. Never blocks the caller.
fn check_for_upgrade(app: &AppHandle) {
    let state = app.state::<AppState>().inner().clone();
    if !self_relaunch_enabled()
        || state.update_pending.load(Ordering::Relaxed)
        || state.upgrade_waiting.load(Ordering::Relaxed)
        || state.upgrade_checking.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let ready = upgraded_binary_ready();
        state.upgrade_checking.store(false, Ordering::SeqCst);
        if ready {
            let a = app.clone();
            let _ = app.run_on_main_thread(move || relaunch_after_upgrade(&a));
        }
    });
}

/// The installed binary is a new, runnable file that kept the same
/// identity for `UPGRADE_SETTLE` (a few tries while it is still changing).
fn upgraded_binary_ready() -> bool {
    let link = std::path::Path::new("/proc/self/exe");
    let Ok(path) = relaunch::installed_path(link) else {
        return false;
    };
    let Some(mut seen) = relaunch::replacement(link, &path) else {
        return false;
    };
    for _ in 0..5 {
        std::thread::sleep(UPGRADE_SETTLE);
        match relaunch::replacement(link, &path) {
            Some(now) if now == seen => return true,
            Some(now) => seen = now,
            None => return false,
        }
    }
    false
}

/// The main window is shown and has the focus, or the tray popup has it:
/// the user is working in the app.
fn user_in_app(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let main = state.main_focused.load(Ordering::Relaxed)
        && app
            .get_webview_window(MAIN)
            .is_some_and(|w| w.is_visible().unwrap_or(false));
    main || state.popup_focused.load(Ordering::Relaxed)
}

/// Relaunch into the replaced binary, unless the user is working in the
/// window: then the window offers "Restart now" and the relaunch waits
/// until they leave it (`relaunch_when_away`). Main thread only.
fn relaunch_after_upgrade(app: &AppHandle) {
    let state = app.state::<AppState>();
    if user_in_app(app) {
        if !state.upgrade_waiting.swap(true, Ordering::SeqCst) {
            log::info!(
                "the app's binary was replaced (a package upgrade); relaunching once the window loses the focus"
            );
            let _ = app.emit_to(MAIN, "update-ready", ());
        }
        return;
    }
    state.upgrade_waiting.store(false, Ordering::SeqCst);
    log::info!("the app's binary was replaced (a package upgrade); relaunching into it");
    let Err(e) = relaunch_now(app);
    log::warn!("{e}; the app keeps running the old code until it is restarted");
    let state = app.state::<AppState>();
    state.update_pending.store(true, Ordering::Relaxed);
    state
        .tray
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .update_pending = true;
    refresh_tray(app);
    let _ = app.emit_to(MAIN, "update-pending", ());
}

/// The user may have left the window (it lost the focus, or was closed to
/// the tray): a waiting relaunch runs if they are still away after
/// `AWAY_WAIT`. Never blocks the caller.
fn relaunch_when_away(app: &AppHandle) {
    if !app
        .state::<AppState>()
        .upgrade_waiting
        .load(Ordering::Relaxed)
    {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(AWAY_WAIT);
        let a = app.clone();
        let _ = app.run_on_main_thread(move || {
            let st = a.state::<AppState>();
            if st.upgrade_waiting.load(Ordering::Relaxed) && !user_in_app(&a) {
                relaunch_after_upgrade(&a);
            }
        });
    });
}

/// Replace this process with the installed binary (`exec`: same PID, so the
/// same session or autostart unit). The old image's tray item and
/// single-instance name go with its D-Bus connections, which `exec` closes
/// before the new image starts; the new one waits for the name to be free
/// (`relaunch::wait_name_free`). Same arguments, with `--hidden` only when no
/// main window is open; the main view's place goes along. Returns only when
/// `exec` failed, and then nothing has been torn down. Main thread only.
fn relaunch_now(app: &AppHandle) -> Result<std::convert::Infallible, String> {
    let installed = relaunch::installed_path(std::path::Path::new("/proc/self/exe"))
        .map_err(|e| format!("cannot find the app binary to relaunch: {e}"))?;
    // From an AppImage: run the AppImage file again, not the binary in its
    // mount.
    let exe = autostart::launcher(std::env::var_os("APPIMAGE"), installed);
    relaunch_into(app, &exe, &[])
}

/// `relaunch_now` into `exe`, with `env` changed besides (a `None` value
/// removes the variable).
fn relaunch_into(
    app: &AppHandle,
    exe: &std::path::Path,
    extra_env: &[(&str, Option<std::ffi::OsString>)],
) -> Result<std::convert::Infallible, String> {
    let shown = app
        .get_webview_window(MAIN)
        .is_some_and(|w| w.is_visible().unwrap_or(false));
    let _ = app.save_window_state(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED);
    let state = app.state::<AppState>();
    let view = state
        .main_view
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let old: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let args = relaunch::relaunch_args(&old, !shown);
    let reap = relaunch::format_pids(&relaunch::child_pids(std::process::id()));
    let mut env = vec![
        (relaunch::ENV_RELAUNCHED, Some("1".into())),
        (relaunch::ENV_REAP, Some(reap.into())),
        (relaunch::ENV_MAIN_VIEW, view.map(|v| v.to_string().into())),
    ];
    env.extend(extra_env.iter().cloned());
    log::info!("relaunching {} {args:?}", exe.display());
    let err = relaunch::exec(exe, &args, &env);
    Err(format!("cannot relaunch {}: {err}", exe.display()))
}

#[tauri::command]
async fn ir_info(path: String) -> Option<irinfo::IrInfo> {
    tauri::async_runtime::spawn_blocking(move || irinfo::read(std::path::Path::new(&path)))
        .await
        .ok()
        .flatten()
}

// Overlay curves (targets, bounds, measurements) are the webview's: it parses
// and draws them. This side stores one JSON file per overlay in the config
// directory. File I/O off the main thread, like the daemon calls above.

#[tauri::command]
async fn overlays_list() -> Result<Vec<Value>, CallError> {
    tauri::async_runtime::spawn_blocking(|| overlays::dir().and_then(|d| overlays::list(&d)))
        .await
        .map_err(|e| CallError {
            code: -32603,
            message: e.to_string(),
        })?
}

#[tauri::command]
async fn overlay_save(name: String, data: Value) -> Result<(), CallError> {
    tauri::async_runtime::spawn_blocking(move || {
        overlays::dir().and_then(|d| overlays::save(&d, &name, &data))
    })
    .await
    .map_err(|e| CallError {
        code: -32603,
        message: e.to_string(),
    })?
}

#[tauri::command]
async fn overlay_delete(name: String) -> Result<(), CallError> {
    tauri::async_runtime::spawn_blocking(move || {
        overlays::dir().and_then(|d| overlays::remove(&d, &name))
    })
    .await
    .map_err(|e| CallError {
        code: -32603,
        message: e.to_string(),
    })?
}

/// Same maths as the engine (dsp-core). The webview draws with an identical
/// TS port; this is here for exact checks.
#[tauri::command]
fn eq_response(bands: Vec<Band>, preamp_db: f64, sample_rate: f64, freqs: Vec<f64>) -> Vec<f64> {
    eq::response_db(&bands, preamp_db, sample_rate, &freqs)
}

#[tauri::command]
fn eq_auto_preamp(bands: Vec<Band>, sample_rate: f64) -> f64 {
    eq::auto_preamp_db(&bands, sample_rate)
}

// ---------------------------------------------------------------- tray

fn update_tray_from(state: &Shared, msg: &SubMsg) -> bool {
    let mut m = state.tray.lock().unwrap_or_else(|e| e.into_inner());
    let before = m.clone();
    match msg {
        SubMsg::Up => m.up = true,
        SubMsg::Down(_) | SubMsg::Connecting => {
            *m = TrayModel {
                update_pending: m.update_pending,
                ..TrayModel::default()
            };
        }
        SubMsg::Event(v) => match v.get("event").and_then(Value::as_str) {
            Some("status") => {
                if let Ok(st) = serde_json::from_value::<Status>(v["data"].clone()) {
                    m.line = tray_text::passthrough_line(&st);
                    m.dsp_enabled = st.dsp_enabled;
                    // "a", "b", or "blind" while a blind test hides the side.
                    m.ab = st.ab.filter(|a| a.enabled).map(|a| match a.current {
                        _ if a.blind => "blind".to_string(),
                        Some(side) => serde_json::to_value(side)
                            .ok()
                            .and_then(|s| s.as_str().map(str::to_string))
                            .unwrap_or_default(),
                        None => String::new(),
                    });
                }
                if let Some(name) = v["data"]["preset"]["name"].as_str() {
                    m.active_preset = Some(name.to_string());
                }
            }
            Some("config") => apply_config(&mut m, &v["data"]),
            Some("presets") => apply_presets(&mut m, &v["data"]),
            _ => {}
        },
    }
    *m != before
}

fn apply_config(m: &mut TrayModel, cfg: &Value) {
    m.routing_apps = cfg["routing"]["mode"] == "apps";
    m.rules = cfg["routing"]["rules"].clone();
    m.dsp_enabled = cfg["dsp_enabled"].as_bool().unwrap_or(m.dsp_enabled);
    if let Some(p) = cfg.get("active_preset") {
        m.active_preset = p.as_str().map(str::to_string);
    }
}

fn apply_presets(m: &mut TrayModel, list: &Value) {
    m.presets = list
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| p["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
}

fn refresh_tray(app: &AppHandle) {
    let state = app.state::<AppState>().inner().clone();
    let model = state.tray.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // ksni's update blocks briefly: never on the main thread.
    std::thread::spawn(move || {
        if let Some(t) = state
            .tray_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            t.set_model(model);
        }
    });
}

/// A tray menu item was chosen. `token` is the click's Wayland activation
/// token, when the host gave one.
pub(crate) fn tray_action(app: &AppHandle, id: String, token: Option<String>) {
    match id.as_str() {
        "show" => present_main(app, token),
        "popup" => open_popup(app, None, token),
        "quit" => app.exit(0),
        "restart" => {
            let a = app.clone();
            let _ = app.run_on_main_thread(move || {
                let Err(e) = relaunch_now(&a);
                log::warn!("{e}");
            });
        }
        _ => {
            let state = app.state::<AppState>().inner().clone();
            let app = app.clone();
            std::thread::spawn(move || {
                let model = state.tray.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let c = &state.client;
                let r = match id.as_str() {
                    "bypass" => c.call(
                        "set_dsp_enabled",
                        Some(json!({"enabled": !model.dsp_enabled})),
                    ),
                    "ab-a" => c.call(
                        "ab_select",
                        Some(json!({"side": "a", "crossfade_ms": 10.0})),
                    ),
                    "ab-b" => c.call(
                        "ab_select",
                        Some(json!({"side": "b", "crossfade_ms": 10.0})),
                    ),
                    "routing-system" => c.call("set_routing_mode", Some(json!({"mode": "system"}))),
                    "routing-apps" => {
                        let rules = if model.rules.is_array() {
                            model.rules.clone()
                        } else {
                            json!([])
                        };
                        c.call(
                            "set_routing_mode",
                            Some(json!({"mode": "apps", "rules": rules})),
                        )
                    }
                    p if p.starts_with("preset:") => {
                        c.call("load_preset", Some(json!({"name": &p["preset:".len()..]})))
                    }
                    _ => return,
                };
                if let Err(e) = r {
                    log::warn!("tray action {id}: {e}");
                }
                // Old daemons have no config event: re-read what the menu shows.
                refresh_model(&state);
                // The check marks toggle locally on click; redraw from the model.
                refresh_tray(&app);
            });
        }
    }
}

/// Fetch config, status and presets (used on connect and after tray actions).
fn refresh_model(state: &Shared) {
    let c = &state.client;
    let cfg = c.call("get_config", None).ok();
    let status = c.call("status", None).ok();
    let presets = c.call("list_presets", None).ok();
    let mut m = state.tray.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(cfg) = cfg {
        apply_config(&mut m, &cfg);
    }
    if let Some(list) = presets {
        apply_presets(&mut m, &list);
    }
    drop(m);
    if let Some(st) = status {
        update_tray_from(
            state,
            &SubMsg::Event(json!({"event": "status", "data": st})),
        );
    }
}

fn show_main(app: &AppHandle) {
    present_main(app, None);
}

/// Show and raise a window. On Wayland a window may only take the focus
/// with an activation token from the user's action (the tray click); GTK
/// uses it through the startup id. Runs on the main thread.
fn present(w: &WebviewWindow, token: Option<&str>, focus: bool) {
    if let Some(t) = token {
        use gtk::prelude::GtkWindowExt;
        if let Ok(gw) = w.gtk_window() {
            gw.set_startup_id(t);
        }
    }
    let _ = w.show();
    let _ = w.unminimize();
    if focus {
        let _ = w.set_focus();
    }
}

/// The main window, made from the app config when it is not there: at
/// start (unless `--hidden`), and after it was closed to the tray, which
/// destroys it. Main thread only.
fn main_window(app: &AppHandle) -> Option<WebviewWindow> {
    if let Some(w) = app.get_webview_window(MAIN) {
        return Some(w);
    }
    let mut cfg = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN)?
        .clone();
    // First size: 75 % of the screen's height (and no wider than 90 % of
    // it). A size saved from an earlier run replaces it (window state).
    if let Ok(Some(m)) = app.primary_monitor() {
        let s = m.size().to_logical::<f64>(m.scale_factor());
        if s.height > 0.0 && s.width > 0.0 {
            cfg.height = (s.height * 0.75).max(cfg.min_height.unwrap_or(0.0));
            cfg.width = cfg
                .width
                .min(s.width * 0.9)
                .max(cfg.min_width.unwrap_or(0.0));
        }
    }
    match WebviewWindowBuilder::from_config(app, &cfg).and_then(|b| b.build()) {
        Ok(w) => {
            // Its meters come back with it (the spectrum when its page
            // asks); at start the subscription is made after this and
            // asks for them itself.
            if let Some(sub) = app
                .state::<AppState>()
                .subscription
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                sub.set_meters(true);
            }
            Some(w)
        }
        Err(e) => {
            log::error!("cannot open the main window: {e}");
            None
        }
    }
}

/// Close the main window to the tray: its size and place are saved, it is
/// hidden at once, and the meters and spectrum it showed are no longer
/// asked of the daemon. The page is asked for its scroll position
/// (`main-closing`); its answer (`main_close_ready`), or a short time-out,
/// destroys the window and its webview. Main thread only.
fn close_main_to_tray(app: &AppHandle, window: &tauri::Window) {
    let state = app.state::<AppState>().inner().clone();
    if !begin_closing(&state, MAIN) {
        return;
    }
    let _ = app.save_window_state(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED);
    main_gone(app);
    let _ = window.hide();
    let close = state.main_close_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = app.emit_to(MAIN, "main-closing", ());
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(MAIN_CLOSE_WAIT);
        let a = app.clone();
        let _ = app.run_on_main_thread(move || {
            let st = a.state::<AppState>();
            if st.main_close_gen.load(Ordering::SeqCst) == close {
                finish_main_close(&a);
            }
        });
    });
}

/// The page handed over its place (or did not answer in time): destroy the
/// main window, once per close. Main thread only.
fn finish_main_close(app: &AppHandle) {
    let state = app.state::<AppState>().inner().clone();
    if !queue_closing(&state, MAIN) {
        return;
    }
    match app.get_webview_window(MAIN) {
        Some(w) => destroy_webview_window(app, w),
        None => closing_done(app, MAIN),
    }
}

/// The closing main page's last place (its scroll position, or null);
/// the window is destroyed after this.
#[tauri::command]
fn main_close_ready(app: AppHandle, view: Option<Value>) {
    if let Some(v) = view {
        *app.state::<AppState>()
            .main_view
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(v);
    }
    let a = app.clone();
    let _ = app.run_on_main_thread(move || finish_main_close(&a));
}

/// Destroy a window with its webview, ending the web process first: WebKit
/// keeps a destroyed view's process alive, and a new view never reuses it. If
/// destroying fails, closing ends and a show request kept meanwhile runs on
/// the window as it is.
fn destroy_webview_window(app: &AppHandle, w: WebviewWindow) {
    let label = w.label().to_string();
    let (w2, a) = (w.clone(), app.clone());
    let queued = w.with_webview(move |pw| {
        use webkit2gtk::WebViewExt;
        pw.inner().terminate_web_process();
        if let Err(e) = w2.destroy() {
            log::warn!("cannot destroy the {} window: {e}", w2.label());
            closing_done(&a, w2.label());
        }
    });
    if queued.is_err() && w.destroy().is_err() {
        log::warn!("cannot destroy the {label} window");
        closing_done(app, &label);
    }
}

/// Mark `label` as closing; false when it already is.
fn begin_closing(state: &Shared, label: &str) -> bool {
    state
        .closing
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(label.to_string())
        .or_default()
        .begin()
}

/// The destruction of `label` may be queued now (once per close).
fn queue_closing(state: &Shared, label: &str) -> bool {
    state
        .closing
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(label.to_string())
        .or_default()
        .queue()
}

/// A show request for `label`: handed back when it may run now, kept
/// while the window is being destroyed. Main thread only, like the
/// `Destroyed` event that replays it.
fn show_request(state: &Shared, label: &str, req: ShowRequest) -> Option<ShowRequest> {
    state
        .closing
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(label.to_string())
        .or_default()
        .request(req)
}

/// `label` is gone (or could not be destroyed): replay what was asked
/// for meanwhile.
fn closing_done(app: &AppHandle, label: &str) {
    let req = app
        .state::<AppState>()
        .closing
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(label)
        .and_then(Closing::done);
    match req {
        Some(ShowRequest::Main(token)) => present_main(app, token),
        Some(ShowRequest::Popup(click, token)) => open_popup(app, click, token),
        Some(ShowRequest::Osd(payload, gen_)) => own_osd(app, payload, gen_),
        None => {}
    }
}

/// The main webview is going or gone (closed to the tray, or destroyed any
/// other way): stop everything that only fed it.
fn main_gone(app: &AppHandle) {
    let state = app.state::<AppState>();
    state.hub.set_shown(false, false);
    state.hub.detach(MAIN);
    state.main_focused.store(false, Ordering::Relaxed);
    state.spectrum_wanted.store(false, Ordering::Relaxed);
    state.hub.clear_spectrum();
    if let Some(sub) = state
        .subscription
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        sub.set_spectrum(false);
        sub.set_meters(false);
    }
}

/// The main page keeps what it wants back after being made again (its
/// scroll position) here, since its webview does not outlive a close to
/// the tray.
#[tauri::command]
fn main_view_save(state: State<'_, AppState>, view: Value) {
    *state.main_view.lock().unwrap_or_else(|e| e.into_inner()) = Some(view);
}

#[tauri::command]
fn main_view_load(state: State<'_, AppState>) -> Option<Value> {
    state
        .main_view
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(crate) fn present_main(app: &AppHandle, token: Option<String>) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let st = app2.state::<AppState>().inner().clone();
        let Some(ShowRequest::Main(token)) = show_request(&st, MAIN, ShowRequest::Main(token))
        else {
            // Being destroyed: shown again, made afresh, once it is gone.
            return;
        };
        if let Some(w) = main_window(&app2) {
            present(&w, token.as_deref(), true);
            let st = app2.state::<AppState>();
            st.hub.set_shown(true, false);
            st.hidden_at_start.store(false, Ordering::Relaxed);
        }
    });
}

// ---------------------------------------------------------------- popup

/// Left click on the tray icon.
pub(crate) fn on_tray_click(app: &AppHandle, click: (i32, i32), token: Option<String>) {
    let state = app.state::<AppState>();
    let action = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tray_click;
    match action {
        TrayClick::Popup => open_popup(app, Some(click), token),
        // With the menu on left click the host opens the menu itself and
        // never calls Activate; this is only reached in between, while the
        // item is being replaced.
        TrayClick::Menu => present_main(app, token),
    }
}

/// Open the popup near `click`, or close it when it is open (a click on
/// the icon while it shows closes it, even though the click took the focus
/// from it first).
fn open_popup(app: &AppHandle, click: Option<(i32, i32)>, token: Option<String>) {
    let state = app.state::<AppState>().inner().clone();
    let just_hid = state
        .popup_hid_at
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|t| t.elapsed() < Duration::from_millis(350));
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(ShowRequest::Popup(click, token)) =
            show_request(&state, POPUP, ShowRequest::Popup(click, token))
        else {
            return;
        };
        let w = match app2.get_webview_window(POPUP) {
            Some(w) => w,
            None => match popup_window(&app2) {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("cannot open the volume popup: {e}");
                    return;
                }
            },
        };
        if w.is_visible().unwrap_or(false) {
            hide_popup_window(&app2, &w);
            return;
        }
        if just_hid && click.is_some() {
            return;
        }
        place(&app2, &w, click, POPUP_SIZE, |area, size| {
            placement::popup(click, area, size)
        });
        *state
            .popup_shown_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        state
            .popup_life
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shown();
        present(&w, token.as_deref(), true);
        let _ = app2.emit_to(POPUP, "popup-shown", ());
    });
}

fn popup_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, POPUP, WebviewUrl::App("index.html".into()))
        .title("Audivane DSP volume")
        .inner_size(POPUP_SIZE.0, POPUP_SIZE.1)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()
}

fn hide_popup_window(app: &AppHandle, w: &WebviewWindow) {
    let _ = w.hide();
    let state = app.state::<AppState>();
    state.popup_focused.store(false, Ordering::Relaxed);
    *state.popup_hid_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    let ticket = state
        .popup_life
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .hidden();
    free_when_idle(app, POPUP, ticket, |s| &s.popup_life);
}

/// Destroy the helper window `label` once `ticket` comes due, if it was not
/// shown again meanwhile (see `window_life`).
fn free_when_idle(
    app: &AppHandle,
    label: &'static str,
    ticket: u64,
    life: fn(&Shared) -> &Mutex<IdleReaper>,
) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(IDLE_FREE);
        let a = app.clone();
        let _ = app.run_on_main_thread(move || {
            let state = a.state::<AppState>();
            let due = life(&state)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .due(ticket);
            if !due {
                return;
            }
            if let Some(w) = a.get_webview_window(label)
                && !w.is_visible().unwrap_or(false)
                && begin_closing(&state, label)
                && queue_closing(&state, label)
            {
                log::debug!("freeing the {label} window");
                destroy_webview_window(&a, w);
            }
            // The `Destroyed` event does the rest.
        });
    });
}

/// A helper window is gone: its page no longer listens.
fn helper_gone(app: &AppHandle, label: &str) {
    let state = app.state::<AppState>();
    state.hub.detach(label);
    if label == POPUP {
        state.popup_focused.store(false, Ordering::Relaxed);
        state
            .popup_life
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .freed();
    } else if label == OSD {
        let mut osd = state.osd.lock().unwrap_or_else(|e| e.into_inner());
        osd.ready = false;
        osd.pending = false;
        drop(osd);
        state
            .osd_life
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .freed();
    }
}

/// Put a window where `pos` says on the monitor of `near` (else the
/// primary one). Wayland compositors ignore this and place it themselves.
fn place(
    app: &AppHandle,
    w: &WebviewWindow,
    near: Option<(i32, i32)>,
    size: (f64, f64),
    pos: impl Fn(placement::Area, (i32, i32)) -> (i32, i32),
) {
    let monitor = near
        .filter(|&p| p != (0, 0))
        .and_then(|(x, y)| app.monitor_from_point(x as f64, y as f64).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(m) = monitor else { return };
    let wa = m.work_area();
    let area = placement::Area {
        x: wa.position.x,
        y: wa.position.y,
        w: wa.size.width as i32,
        h: wa.size.height as i32,
    };
    let scale = m.scale_factor();
    let px = (
        (size.0 * scale).round() as i32,
        (size.1 * scale).round() as i32,
    );
    let (x, y) = pos(area, px);
    let _ = w.set_position(PhysicalPosition::new(x, y));
}

/// The popup's Escape, or its "Open Audivane" link.
#[tauri::command]
fn popup_hide(app: AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window(POPUP) {
            hide_popup_window(&app2, &w);
        }
    });
}

#[tauri::command]
fn show_main_window(app: AppHandle) {
    show_main(&app);
}

// ---------------------------------------------------------------- volume keys

/// A volume step from a key or the tray wheel. Every step, with or without
/// a window open, goes to one thread here that makes them one after the
/// other (`volume_step`: the store's step rules and the same guarded
/// write), then asks for the OSD. One serial writer means no handover:
/// a window that opens or closes meanwhile only shows the result through
/// the status events, and no step can start from a level another writer
/// is about to replace.
fn volume_step(app: &AppHandle, action: KeyAction, source: &str) {
    let state = app.state::<AppState>().inner().clone();
    let step = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .volume_step_db;
    let mut tx = state.native_steps.lock().unwrap_or_else(|e| e.into_inner());
    let mut item = (action, step, source.to_string());
    if let Some(t) = tx.as_ref() {
        match t.send(item) {
            Ok(()) => return,
            // The thread ended (it cannot, short of a panic): start another.
            Err(e) => item = e.0,
        }
    }
    let (t, rx) = std::sync::mpsc::channel::<(KeyAction, f64, String)>();
    let _ = t.send(item);
    *tx = Some(t);
    let app = app.clone();
    std::thread::Builder::new()
        .name("volume-keys".into())
        .spawn(move || {
            let state = app.state::<AppState>().inner().clone();
            let call = |m: &str, p: Option<Value>| state.client.call(m, p);
            for (action, step_db, source) in rx {
                let shown = {
                    let mut levels = state.levels.lock().unwrap_or_else(|e| e.into_inner());
                    volume_step::step(&mut levels, &call, action, step_db)
                };
                if let Some(payload) = shown {
                    osd_show_payload(&app, payload, &source);
                }
            }
        })
        .expect("spawn the volume-key thread");
}

/// The tray icon's wheel: deltas add up, one step per full notch (120),
/// wheel-up louder on every host (the sign is the host's, see `wheel.rs`),
/// unless the user reversed it.
pub(crate) fn on_tray_scroll(app: &AppHandle, delta: i32) {
    let state = app.state::<AppState>();
    if !state.tray.lock().unwrap_or_else(|e| e.into_inner()).up {
        return;
    }
    let sign = wheel::host_sign(
        state
            .tray_host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .watcher_process
            .as_deref(),
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
    );
    let reverse = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reverse_wheel;
    let steps = state.wheel.lock().unwrap_or_else(|e| e.into_inner()).push(
        delta,
        sign,
        reverse,
        Instant::now(),
    );
    let action = if steps > 0 {
        KeyAction::Up
    } else {
        KeyAction::Down
    };
    for _ in 0..steps.unsigned_abs() {
        volume_step(app, action, "scroll");
    }
}

fn set_keys_state(app: &AppHandle, s: KeysState) {
    let state = app.state::<AppState>();
    state.keys.lock().unwrap_or_else(|e| e.into_inner()).state = Some(s.clone());
    let _ = app.emit("keys-state", s);
}

fn keys_state(state: &Shared) -> KeysState {
    state
        .keys
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .state
        .clone()
        .unwrap_or(KeysState::Off)
}

/// X11 without a portal: grab the keys directly. A desktop that already
/// grabbed them (its own volume keys) makes this fail.
fn x11_session() -> bool {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    match var("XDG_SESSION_TYPE").as_deref() {
        Some("x11") => true,
        Some("wayland") => false,
        _ => var("WAYLAND_DISPLAY").is_none() && var("DISPLAY").is_some(),
    }
}

fn start_x11_keys(app: &AppHandle, why_no_portal: String) {
    use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};
    let state = app.state::<AppState>().inner().clone();
    let mut keys = state.keys.lock().unwrap_or_else(|e| e.into_inner());
    if !keys.x11_plugin {
        let st = state.clone();
        let plugin = tauri_plugin_global_shortcut::Builder::new()
            .with_handler(move |_app, shortcut, event| {
                let action = match shortcut.key {
                    Code::AudioVolumeUp => KeyAction::Up,
                    Code::AudioVolumeDown => KeyAction::Down,
                    Code::AudioVolumeMute => KeyAction::Mute,
                    _ => return,
                };
                let e = match event.state {
                    ShortcutState::Pressed => KeyEvent::Pressed(action),
                    ShortcutState::Released => KeyEvent::Released(action),
                };
                if let Some(tx) = st
                    .key_events
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    let _ = tx.send(e);
                }
            })
            .build();
        if let Err(e) = app.plugin(plugin) {
            drop(keys);
            set_keys_state(
                app,
                KeysState::Unavailable {
                    reason: format!("{why_no_portal}, and grabbing the keys failed ({e})."),
                },
            );
            return;
        }
        keys.x11_plugin = true;
    }
    let gs = app.global_shortcut();
    let mut bound = Vec::new();
    let mut failed = Vec::new();
    for (action, code) in [
        (KeyAction::Up, Code::AudioVolumeUp),
        (KeyAction::Down, Code::AudioVolumeDown),
        (KeyAction::Mute, Code::AudioVolumeMute),
    ] {
        match gs.register(Shortcut::new(None, code)) {
            Ok(()) => bound.push(shortcuts::Bound {
                action,
                trigger: action.preferred_trigger().to_string(),
            }),
            Err(e) => failed.push(format!("{}: {e}", action.preferred_trigger())),
        }
    }
    keys.x11 = !bound.is_empty();
    drop(keys);
    if failed.is_empty() {
        set_keys_state(
            app,
            KeysState::Active {
                via: shortcuts::Backend::X11,
                bound,
            },
        );
    } else {
        let _ = gs.unregister_all();
        state.keys.lock().unwrap_or_else(|e| e.into_inner()).x11 = false;
        set_keys_state(
            app,
            KeysState::Unavailable {
                reason: format!(
                    "Another program already uses the volume keys ({}). Unbind them in the desktop's settings, then turn this on again.",
                    failed.join("; ")
                ),
            },
        );
    }
}

fn start_keys(app: &AppHandle) {
    let state = app.state::<AppState>().inner().clone();
    stop_keys(app);
    let a = app.clone();
    let tx = shortcuts::spawn_repeater(move |action| volume_step(&a, action, "keys"));
    *state.key_events.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
    let (a1, a2) = (app.clone(), app.clone());
    let tx = Mutex::new(tx);
    let portal = shortcuts::start_portal(
        move |s| set_keys_state(&a1, s),
        move |e| {
            let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(e);
        },
        move |why| {
            if x11_session() {
                let a3 = a2.clone();
                let _ = a2.run_on_main_thread(move || start_x11_keys(&a3, why));
            } else {
                set_keys_state(
                    &a2,
                    KeysState::Unavailable {
                        reason: format!(
                            "{}: on Wayland the volume keys need the desktop's global shortcuts portal (KDE Plasma has it).",
                            capitalize(&why)
                        ),
                    },
                );
            }
        },
    );
    state.keys.lock().unwrap_or_else(|e| e.into_inner()).portal = Some(portal);
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn stop_keys(app: &AppHandle) {
    let state = app.state::<AppState>().inner().clone();
    let (portal, x11) = {
        let mut k = state.keys.lock().unwrap_or_else(|e| e.into_inner());
        let x11 = std::mem::replace(&mut k.x11, false);
        (k.portal.take(), x11)
    };
    // Joining the portal thread may wait for D-Bus: not on the main thread.
    if let Some(mut p) = portal {
        std::thread::spawn(move || p.stop());
    }
    if x11 {
        use tauri_plugin_global_shortcut::GlobalShortcutExt;
        let _ = app.global_shortcut().unregister_all();
    }
    // Ends the repeater thread.
    *state.key_events.lock().unwrap_or_else(|e| e.into_inner()) = None;
    set_keys_state(app, KeysState::Off);
}

// ---------------------------------------------------------------- OSD

/// Show the on-screen display with `payload` (`volume_db`, `muted`,
/// `min_db`, `max_db`, optional `note`). `source` is `keys`/`scroll` (the
/// main window answering a step) or `other` (a level change seen in status).
fn osd_show_payload(app: &AppHandle, payload: Value, source: &str) {
    let state = app.state::<AppState>().inner().clone();
    let mode = state.settings.lock().unwrap_or_else(|e| e.into_inner()).osd;
    let from_keys = source != "other";
    match mode {
        OsdMode::Off => return,
        OsdMode::Keys if !from_keys => return,
        _ => {}
    }
    {
        let mut osd = state.osd.lock().unwrap_or_else(|e| e.into_inner());
        if from_keys {
            osd.key_at = Some(Instant::now());
        } else if osd
            .key_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
        {
            // The echo of a key step: the key's own OSD already shows it.
            return;
        }
        osd.last = Some(payload.clone());
    }
    let gen_ = state.osd_gen.fetch_add(1, Ordering::Relaxed) + 1;
    let app = app.clone();
    // Off the calling thread: the Plasma call may take up to its timeout.
    std::thread::spawn(move || {
        if plasma_osd::is_plasma(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref()) {
            let display = plasma_osd_display(&payload);
            if plasma_osd::show(&display) {
                return;
            }
        }
        own_osd(&app, payload, gen_);
    });
}

/// Plasma's OSD display (icon, label and bar percent) for a payload.
fn plasma_osd_display(p: &Value) -> plasma_osd::Display {
    let db = p["volume_db"].as_f64();
    let lo = p["min_db"].as_f64().unwrap_or(-80.0);
    let hi = p["max_db"].as_f64().unwrap_or(0.0);
    plasma_osd::format(
        db,
        p["muted"].as_bool().unwrap_or(false),
        lo,
        hi,
        p["note"].as_str(),
    )
}

/// A Wayland session (GTK draws natively there). GTK3 cannot keep a new
/// window from taking the focus on Wayland, so our own OSD window is only
/// mapped there while an Audivane window has the focus anyway.
fn wayland_session() -> bool {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if var("GDK_BACKEND").is_some_and(|b| b.starts_with("x11")) {
        return false;
    }
    var("XDG_SESSION_TYPE").as_deref() == Some("wayland") || var("WAYLAND_DISPLAY").is_some()
}

/// Our own OSD window (no Plasma OSD): shown for `OSD_SHOW`, then hidden.
fn own_osd(app: &AppHandle, payload: Value, gen_: u64) {
    let state = app.state::<AppState>().inner().clone();
    if wayland_session()
        && !state.main_focused.load(Ordering::Relaxed)
        && !state.popup_focused.load(Ordering::Relaxed)
    {
        log::debug!("OSD skipped: on Wayland it could take the focus from another app");
        return;
    }
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let st = app2.state::<AppState>();
        let Some(ShowRequest::Osd(payload, _)) =
            show_request(&st, OSD, ShowRequest::Osd(payload, gen_))
        else {
            // Being freed: shown in a new window once the old one is gone.
            return;
        };
        let w = match app2.get_webview_window(OSD) {
            Some(w) => w,
            None => match osd_window(&app2) {
                Ok(w) => w,
                Err(e) => {
                    log::warn!("cannot open the on-screen display: {e}");
                    return;
                }
            },
        };
        let ready = {
            let mut osd = st.osd.lock().unwrap_or_else(|e| e.into_inner());
            osd.pending = !osd.ready;
            osd.ready
        };
        st.osd_life
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shown();
        let _ = app2.emit_to(OSD, "osd", payload);
        if ready && !w.is_visible().unwrap_or(false) {
            place(&app2, &w, None, OSD_SIZE, placement::osd);
            let _ = w.show();
        }
    });
    let app3 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(OSD_SHOW);
        let state = app3.state::<AppState>();
        if state.osd_gen.load(Ordering::Relaxed) != gen_ {
            return;
        }
        state.osd.lock().unwrap_or_else(|e| e.into_inner()).pending = false;
        let a = app3.clone();
        let _ = app3.run_on_main_thread(move || {
            if let Some(w) = a.get_webview_window(OSD) {
                let _ = w.hide();
                let ticket = a
                    .state::<AppState>()
                    .osd_life
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .hidden();
                free_when_idle(&a, OSD, ticket, |s| &s.osd_life);
            }
        });
    });
}

fn osd_window(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, OSD, WebviewUrl::App("index.html".into()))
        .title("Audivane DSP volume display")
        .inner_size(OSD_SIZE.0, OSD_SIZE.1)
        .resizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .focused(false)
        .focusable(false)
        .shadow(false)
        .visible(false)
        .build()
}

/// The main window answered a volume key: show what it set.
#[tauri::command]
fn osd_show(app: AppHandle, payload: Value, source: String) {
    osd_show_payload(&app, payload, &source);
}

/// The OSD page has loaded: it gets the latest payload, and is shown if a
/// change is waiting for it.
#[tauri::command]
fn osd_ready(app: AppHandle) -> Option<Value> {
    let state = app.state::<AppState>().inner().clone();
    let (last, show) = {
        let mut osd = state.osd.lock().unwrap_or_else(|e| e.into_inner());
        osd.ready = true;
        (osd.last.clone(), std::mem::replace(&mut osd.pending, false))
    };
    if show {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(w) = app2.get_webview_window(OSD) {
                place(&app2, &w, None, OSD_SIZE, placement::osd);
                let _ = w.show();
            }
        });
    }
    last
}

/// A status event: a level change from any source shows the OSD when the
/// user asked for that and none of our own windows has the focus (the
/// slider being dragged is already in view).
fn osd_from_status(app: &AppHandle, state: &Shared, status: &Value) {
    let v = &status["volume"];
    let (Some(db), Some(muted)) = (v["volume_db"].as_f64(), v["muted"].as_bool()) else {
        return;
    };
    let now = SeenLevel {
        instance: v["instance"].as_str().map(str::to_string),
        volume_db: db,
        muted,
    };
    let changed = {
        let mut osd = state.osd.lock().unwrap_or_else(|e| e.into_inner());
        let prev = osd.seen.replace(now.clone());
        // A first status, or another daemon: not a change anyone made.
        prev.is_some_and(|p| {
            p.instance == now.instance && (p.volume_db != now.volume_db || p.muted != now.muted)
        })
    };
    if !changed
        || state.main_focused.load(Ordering::Relaxed)
        || state.popup_focused.load(Ordering::Relaxed)
    {
        return;
    }
    osd_show_payload(
        app,
        json!({
            "volume_db": db,
            "muted": muted,
            "min_db": v["min_db"].as_f64(),
            "max_db": v["max_db"].as_f64(),
        }),
        "other",
    );
}

// ---------------------------------------------------------------- app settings

fn settings_view(state: &Shared) -> Value {
    let settings = state
        .settings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let autostart = autostart::entry_path()
        .map(|p| autostart::is_enabled(&p))
        .unwrap_or(false);
    let tray = state.tray_shown();
    let kde = plasma_osd::is_plasma(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref());
    json!({
        "settings": settings,
        "autostart": autostart,
        "keys": keys_state(state),
        "tray": tray,
        "kde": kde,
    })
}

/// Tell every window the settings view changed (the tray came or went,
/// or a setting changed).
fn emit_settings(app: &AppHandle) {
    let state = app.state::<AppState>().inner().clone();
    let _ = app.emit("ui-settings", settings_view(&state));
}

#[tauri::command]
fn ui_settings(state: State<'_, AppState>) -> Value {
    settings_view(&state)
}

#[tauri::command]
fn set_ui_settings(app: AppHandle, settings: UiSettings) -> Result<Value, CallError> {
    let state = app.state::<AppState>().inner().clone();
    let new = settings.normalized();
    let old = std::mem::replace(
        &mut *state.settings.lock().unwrap_or_else(|e| e.into_inner()),
        new.clone(),
    );
    if old.tray_click != new.tray_click {
        respawn_tray(&app);
    }
    if old.volume_keys != new.volume_keys {
        if new.volume_keys {
            start_keys(&app);
        } else {
            stop_keys(&app);
        }
    }
    // In force now either way; a file that cannot be written only means
    // they are not kept for the next start, which the view says.
    let saved = match &state.settings_path {
        Some(p) => ui_settings::save(p, &new),
        None => Err("there is no config folder (neither XDG_CONFIG_HOME nor HOME is set)".into()),
    };
    let view = settings_view(&state);
    let _ = app.emit("ui-settings", view.clone());
    saved.map(|()| view).map_err(|e| {
        log::warn!("{e}");
        CallError {
            code: 2,
            message: format!("The setting applies now but was not saved for the next start: {e}"),
        }
    })
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<Value, CallError> {
    let state = app.state::<AppState>().inner().clone();
    let err = |message: String| CallError { code: 2, message };
    let path = autostart::entry_path().map_err(err)?;
    // Tauri's resolver (as for Restart): the AppImage itself when the app
    // runs from one, never the binary in its temporary mount.
    let env = app.env();
    let binary = tauri::process::current_binary(&env)
        .map_err(|e| err(format!("cannot find the app's own binary: {e}")))?;
    let exe = autostart::launcher(env.appimage.clone(), binary);
    autostart::set_enabled(&path, enabled, &exe).map_err(err)?;
    let view = settings_view(&state);
    let _ = app.emit("ui-settings", view.clone());
    Ok(view)
}

// ---------------------------------------------------------------- tray item

/// Replace the tray item so its left click matches the settings (popup or
/// menu). Off the main thread, one rebuild at a time; each rebuild reads
/// the setting when it runs, so the last change wins. The settings view is
/// sent again once the new item is up.
fn respawn_tray(app: &AppHandle) {
    let app = app.clone();
    let state = app.state::<AppState>().inner().clone();
    state.tray_respawns.fetch_add(1, Ordering::SeqCst);
    std::thread::spawn(move || {
        let state = app.state::<AppState>().inner().clone();
        {
            let _one = state
                .tray_respawn_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let menu_on_left = state
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .tray_click
                == TrayClick::Menu;
            replace_tray_item(&app, &state, menu_on_left);
        }
        state.tray_respawns.fetch_sub(1, Ordering::SeqCst);
        state.sync_close_to_tray();
        emit_settings(&app);
    });
}

fn replace_tray_item(app: &AppHandle, state: &Shared, menu_on_left: bool) {
    {
        let old = state
            .tray_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(old) = old {
            if old.menu_on_left() == menu_on_left {
                *state.tray_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(old);
                return;
            }
            old.shutdown_within(dbus_timeout());
        }
        let model = state.tray.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match TrayHandle::spawn(app, menu_on_left, model, true) {
            Ok(h) => *state.tray_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(h),
            Err(e) => log::warn!("tray icon: {e}"),
        }
    }
}

fn dbus_timeout() -> Duration {
    crate::dbus_util::CALL_TIMEOUT
}

/// Start the tray item and the watch on the tray host, both off the UI
/// thread (an unresponsive watcher must not hold up the window). With
/// `--hidden` and no tray after a short wait, the window is shown instead
/// of staying hidden for good.
fn start_tray(app: &AppHandle, menu_on_left: bool, start_hidden: bool) {
    let state = app.state::<AppState>().inner().clone();
    {
        let (app, state) = (app.clone(), state.clone());
        std::thread::Builder::new()
            .name("tray-item".into())
            .spawn(move || {
                // `true`: a watcher that is missing now (late login, a
                // panel restart) is waited for; the item registers then.
                let (tx, rx) = std::sync::mpsc::channel();
                let a = app.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(TrayHandle::spawn(
                        &a,
                        menu_on_left,
                        TrayModel::default(),
                        true,
                    ));
                });
                let handle = match rx.recv_timeout(dbus_timeout()) {
                    Ok(r) => r,
                    Err(_) => {
                        log::warn!(
                            "the tray item is slow to register; going on without it for now"
                        );
                        rx.recv()
                            .unwrap_or_else(|_| Err("the tray thread ended".into()))
                    }
                };
                match handle {
                    Ok(h) => {
                        *state.tray_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
                        refresh_tray(&app);
                    }
                    Err(e) => log::warn!("no tray icon: {e}"),
                }
                state.sync_close_to_tray();
                emit_settings(&app);
            })
            .expect("spawn the tray thread");
    }
    {
        let (app, state) = (app.clone(), state.clone());
        sni_watch::watch(move |host| {
            if !host.available {
                log::info!("no tray host: closing the window quits until one is back");
            }
            *state.tray_host.lock().unwrap_or_else(|e| e.into_inner()) = host;
            state.sync_close_to_tray();
            emit_settings(&app);
        });
    }
    if start_hidden {
        let app = app.clone();
        std::thread::spawn(move || {
            // Long enough for the tray host and item to come up.
            std::thread::sleep(Duration::from_secs(3));
            let state = app.state::<AppState>();
            if state.hidden_at_start.load(Ordering::Relaxed) && !state.tray_live() {
                log::warn!("started hidden, but there is no tray: showing the window");
                show_main(&app);
            }
        });
    }
}

// ---------------------------------------------------------------- run

/// First run: start in the tray at login by default, and write the
/// settings so this happens once.
fn first_run_defaults(app: &AppHandle, settings: &UiSettings) {
    let env = app.env();
    if let (Ok(path), Ok(binary)) = (
        autostart::entry_path(),
        tauri::process::current_binary(&env),
    ) {
        let exe = autostart::launcher(env.appimage.clone(), binary);
        if let Err(e) = autostart::set_enabled(&path, true, &exe) {
            log::warn!("start at login: {e}");
        }
    }
    if let Ok(p) = ui_settings::path()
        && let Err(e) = ui_settings::save(&p, settings)
    {
        log::warn!("{e}");
    }
}

pub fn run() {
    // stderr, which the session journal keeps for a desktop-launched app.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let context = tauri::generate_context!();
    // Relaunched into a new binary (`relaunch_now`): the old image's
    // single-instance name may still look taken for a moment, its WebKit
    // processes are still our children, and its main view's place came along.
    let relaunched = std::env::var_os(relaunch::ENV_RELAUNCHED).is_some();
    let relaunched_view: Option<Value> = if relaunched {
        relaunch::wait_name_free(
            &format!("{}.SingleInstance", context.config().identifier),
            Duration::from_secs(3),
        );
        relaunch::reap(relaunch::parse_pids(
            &std::env::var(relaunch::ENV_REAP).unwrap_or_default(),
        ));
        std::env::var(relaunch::ENV_MAIN_VIEW)
            .ok()
            .and_then(|v| serde_json::from_str(&v).ok())
    } else {
        None
    };
    let path: PathFn = Arc::new(paths::socket_path);
    let settings_path = ui_settings::path().map_err(|e| log::warn!("{e}")).ok();
    // No settings file yet: the app's first run on this account.
    let first_run = settings_path.as_ref().is_some_and(|p| !p.exists());
    let settings = settings_path
        .as_deref()
        .map(ui_settings::load)
        .unwrap_or_default();
    let shared: AppState = Arc::new(Shared {
        client: Arc::new(DaemonClient::new(path.clone())),
        hub: Arc::new(Hub::default()),
        tray: Mutex::new(TrayModel::default()),
        tray_handle: Mutex::new(None),
        subscription: Mutex::new(None),
        spectrum_wanted: AtomicBool::new(false),
        settings: Mutex::new(settings.clone()),
        settings_path,
        keys: Mutex::new(Keys::default()),
        key_events: Mutex::new(None),
        osd: Mutex::new(Osd::default()),
        osd_gen: AtomicU64::new(0),
        popup_hid_at: Mutex::new(None),
        popup_shown_at: Mutex::new(None),
        main_focused: AtomicBool::new(false),
        popup_focused: AtomicBool::new(false),
        tray_host: Mutex::new(TrayHost::default()),
        wheel: Mutex::new(WheelAccumulator::default()),
        hidden_at_start: AtomicBool::new(false),
        tray_respawns: AtomicUsize::new(0),
        tray_respawn_lock: Mutex::new(()),
        main_view: Mutex::new(relaunched_view),
        popup_life: Mutex::new(IdleReaper::default()),
        osd_life: Mutex::new(IdleReaper::default()),
        levels: Mutex::new(volume_step::Tracker::default()),
        native_steps: Mutex::new(None),
        closing: Mutex::new(BTreeMap::new()),
        main_close_gen: AtomicU64::new(0),
        update_pending: AtomicBool::new(false),
        upgrade_waiting: AtomicBool::new(false),
        upgrade_checking: AtomicBool::new(false),
    });
    // Until the tray-host watch says otherwise there is no tray: closing
    // the window quits.
    shared.hub.no_tray.store(true, Ordering::Relaxed);
    let start_hidden = std::env::args().any(|a| a == "--hidden");

    let app = tauri::Builder::default()
        // Must be first: a second launch just shows the running window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app)
        }))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED)
                // Only the main window: the popup and the OSD are placed
                // next to the tray each time.
                .with_denylist(&[POPUP, OSD])
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(shared.clone())
        .invoke_handler(tauri::generate_handler![
            daemon_call,
            daemon_subscribe,
            meters_ack,
            spectrum_ack,
            spectrum_enable,
            start_engine,
            engine_at_login,
            set_engine_at_login,
            engine_last_error,
            restart_engine,
            app_version,
            restart_app,
            app_update_pending,
            appimage_setup,
            appimage_install,
            ir_info,
            overlays_list,
            overlay_save,
            overlay_delete,
            eq_response,
            eq_auto_preamp,
            popup_hide,
            show_main_window,
            osd_show,
            osd_ready,
            ui_settings,
            set_ui_settings,
            set_autostart,
            main_view_save,
            main_view_load,
            main_close_ready
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            if first_run {
                first_run_defaults(&handle, &settings);
            }
            // The main window is not in the config's start-up windows
            // (`create: false`): with `--hidden` it is not made at all
            // until it is shown (after a few seconds if no tray comes up).
            if start_hidden {
                shared.hidden_at_start.store(true, Ordering::Relaxed);
            } else if main_window(&handle).is_some() {
                shared.hub.set_shown(true, false);
            }
            // The tray item and the tray-host watch run on their own
            // threads: nothing here waits on D-Bus.
            start_tray(
                &handle,
                settings.tray_click == TrayClick::Menu,
                start_hidden,
            );

            if settings.volume_keys {
                start_keys(&handle);
            }

            // Event subscription: forwards to the webviews and drives the tray.
            let state = shared.clone();
            let h = handle.clone();
            let sink = Arc::new(move |msg: SubMsg| {
                state.hub.on_message(&msg);
                if let SubMsg::Event(v) = &msg
                    && v.get("event").and_then(Value::as_str) == Some("status")
                {
                    state
                        .levels
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .observe_status(&v["data"]);
                    osd_from_status(&h, &state, &v["data"]);
                }
                if matches!(msg, SubMsg::Down(_) | SubMsg::Connecting) {
                    state
                        .levels
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clear();
                }
                if !matches!(msg, SubMsg::Event(_) | SubMsg::Up) {
                    state.osd.lock().unwrap_or_else(|e| e.into_inner()).seen = None;
                }
                let mut changed = update_tray_from(&state, &msg);
                if msg == SubMsg::Up {
                    // The post-upgrade scriptlet restarts the engine: a
                    // reconnect is when a new app binary may be in place.
                    check_for_upgrade(&h);
                    let (st, h2) = (state.clone(), h.clone());
                    std::thread::spawn(move || {
                        refresh_model(&st);
                        refresh_tray(&h2);
                    });
                    changed = true;
                }
                if changed {
                    refresh_tray(&h);
                }
            });
            // Meters only while a main window exists to show them (none
            // with `--hidden`, or when making it failed).
            let has_main = handle.get_webview_window(MAIN).is_some();
            let sub = Subscription::spawn_with(path.clone(), sink, has_main);
            if shared.spectrum_wanted.load(Ordering::Relaxed) {
                sub.set_spectrum(true);
            }
            *shared
                .subscription
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(sub);
            // The fallback look for a replaced binary.
            let h = handle.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(UPGRADE_POLL);
                    check_for_upgrade(&h);
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            let app = window.app_handle();
            let state = app.state::<AppState>();
            if let WindowEvent::Destroyed = event {
                match window.label() {
                    MAIN => {
                        main_gone(app);
                        relaunch_when_away(app);
                    }
                    label => helper_gone(app, label),
                }
                // A show request made while it was going is made afresh now.
                closing_done(app, window.label());
                return;
            }
            if window.label() == POPUP {
                if let WindowEvent::Focused(focused) = event {
                    state.popup_focused.store(*focused, Ordering::Relaxed);
                    if !*focused {
                        relaunch_when_away(app);
                    }
                    // Focus loss closes it, like a menu. GTK can report a
                    // short focus-out while mapping; ignore one right after
                    // it opened.
                    let fresh = state
                        .popup_shown_at
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(150));
                    if !*focused
                        && !fresh
                        && let Some(w) = app.get_webview_window(POPUP)
                    {
                        hide_popup_window(app, &w);
                    }
                }
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if let Some(w) = app.get_webview_window(POPUP) {
                        hide_popup_window(app, &w);
                    }
                }
                return;
            }
            if window.label() == OSD {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                    let ticket = state
                        .osd_life
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .hidden();
                    free_when_idle(app, OSD, ticket, |s| &s.osd_life);
                }
                return;
            }
            if window.label() != MAIN {
                return;
            }
            if let WindowEvent::Focused(focused) = event {
                state.main_focused.store(*focused, Ordering::Relaxed);
                if !*focused {
                    relaunch_when_away(app);
                }
            }
            // Minimizing has no event of its own. On GTK, (de)iconifying sends
            // Moved and Resized, and focus changes with it; read the state then.
            // (These handlers run on the main thread, where the getters answer at once.)
            if matches!(
                event,
                WindowEvent::Moved(_) | WindowEvent::Resized(_) | WindowEvent::Focused(_)
            ) {
                let visible = window.is_visible().unwrap_or(true);
                let minimized = window.is_minimized().unwrap_or(false);
                state.hub.set_shown(visible, minimized);
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                if state.hub.no_tray.load(Ordering::Relaxed) {
                    // Nowhere to hide to: closing the window quits the app,
                    // whatever helper windows (popup, OSD) still exist, so
                    // nothing is left running out of reach. The engine keeps
                    // running.
                    let _ = app.save_window_state(
                        StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED,
                    );
                    app.exit(0);
                    return;
                }
                // Closing goes to the tray: the window and its webview are
                // destroyed (made again when shown); the engine is a separate
                // service and keeps running.
                api.prevent_close();
                close_main_to_tray(app, window);
            }
        })
        .build(context)
        .expect("failed to build the Audivane DSP window");

    app.run(|handle, event| {
        // The last window was destroyed (the main window closed to the
        // tray, the popup or OSD freed): the tray icon keeps the app.
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = &event
            && handle.state::<AppState>().tray_live()
        {
            api.prevent_exit();
            return;
        }
        if let RunEvent::Exit = event {
            let state = handle.state::<AppState>();
            if let Some(mut s) = state
                .subscription
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                s.stop();
            }
            state.client.reset();
            if let Some(mut p) = state
                .keys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .portal
                .take()
            {
                p.stop();
            }
            if let Some(t) = state
                .tray_handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                // Asks the tray service to end; never waits on D-Bus.
                t.shutdown();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_keeps_one_frame_outstanding_and_only_the_newest_waiting() {
        let mut m = MeterMailbox::default();
        assert_eq!(m.offer(json!(1)), Some(json!(1)));
        // Not acknowledged yet: later frames wait, the newest replacing older ones.
        assert_eq!(m.offer(json!(2)), None);
        assert_eq!(m.offer(json!(3)), None);
        assert_eq!(m.ack(), Some(json!(3)));
        assert_eq!(m.offer(json!(4)), None);
        assert_eq!(m.ack(), Some(json!(4)));
        // Acknowledged with nothing waiting: the next frame goes out at once.
        assert_eq!(m.ack(), None);
        assert_eq!(m.offer(json!(5)), Some(json!(5)));
    }

    #[test]
    fn hidden_or_minimized_window_gets_no_meters() {
        let hub = Hub::default();
        let meters = SubMsg::Event(json!({"event": "meters", "data": {}}));
        hub.set_shown(true, false);
        hub.on_message(&meters);
        assert!(hub.meters.lock().unwrap().outstanding);
        hub.on_message(&meters);
        assert!(hub.meters.lock().unwrap().latest.is_some());
        // Minimized: the waiting frame is dropped and new ones are not kept.
        hub.set_shown(true, true);
        assert!(hub.meters.lock().unwrap().latest.is_none());
        hub.on_message(&meters);
        assert!(hub.meters.lock().unwrap().latest.is_none());
        hub.meters_ack();
        assert!(!hub.meters.lock().unwrap().outstanding);
        // Other events are still remembered (and sent) while minimized.
        hub.on_message(&SubMsg::Event(json!({"event": "status", "data": {}})));
        assert!(hub.last.lock().unwrap().contains_key("status"));
        // Hidden to the tray: same as minimized.
        hub.set_shown(false, false);
        hub.on_message(&meters);
        assert!(!hub.meters.lock().unwrap().outstanding);
        hub.set_shown(true, false);
        hub.on_message(&meters);
        assert!(hub.meters.lock().unwrap().outstanding);
    }

    #[test]
    fn a_destroyed_webview_is_detached_and_gets_nothing() {
        let hub = Hub::default();
        let has_main = |hub: &Hub| hub.channels.lock().unwrap().contains_key(MAIN);
        // A destroyed page's channel is dropped at once, before any send fails.
        let ch: Channel<Value> = Channel::new(|_| Ok(()));
        hub.attach(MAIN, ch);
        assert!(has_main(&hub));
        hub.detach(POPUP);
        assert!(has_main(&hub));
        hub.detach(MAIN);
        assert!(!has_main(&hub));
        // Status is still remembered for the next page.
        hub.on_message(&SubMsg::Event(json!({"event": "status", "data": {}})));
        assert!(hub.last.lock().unwrap().contains_key("status"));
    }

    #[test]
    fn hidden_or_minimized_window_gets_no_spectrum() {
        let hub = Hub::default();
        let spec = SubMsg::Event(json!({"event": "spectrum", "data": {}}));
        hub.set_shown(true, false);
        hub.on_message(&spec);
        assert!(hub.spectrum.lock().unwrap().outstanding);
        hub.on_message(&spec);
        assert!(hub.spectrum.lock().unwrap().latest.is_some());
        // Minimized: waiting frame dropped and new ones ignored.
        hub.set_shown(true, true);
        assert!(hub.spectrum.lock().unwrap().latest.is_none());
        hub.on_message(&spec);
        assert!(hub.spectrum.lock().unwrap().latest.is_none());
        hub.spectrum_ack();
        assert!(!hub.spectrum.lock().unwrap().outstanding);
        // Hidden to the tray: dropped too.
        hub.set_shown(false, false);
        hub.on_message(&spec);
        assert!(!hub.spectrum.lock().unwrap().outstanding);
        hub.set_shown(true, false);
        hub.on_message(&spec);
        assert!(hub.spectrum.lock().unwrap().outstanding);
    }
}
