//! Tauri shell: commands for the webview, the event channel, the tray menu,
//! close-to-tray, single instance and remembered window size.

use crate::client::{CallError, DaemonClient, PathFn, SubMsg, Subscription};
use crate::{eq, irinfo, overlays, paths, systemd, tray_text};
use audivane_protocol::Band;
use audivane_protocol::ipc::Status;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::ipc::Channel;
use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_window_state::{AppHandleExt, StateFlags};

const MAIN: &str = "main";
const TRAY: &str = "main-tray";

/// Meter frames for the webview: at most one delivery outstanding.
///
/// `Channel::send` is not acknowledged, so a webview that stops processing
/// messages (suspended while minimized, say) would let 30 Hz meter frames pile
/// up. Instead one frame goes out, and the next only once the webview says it
/// has drawn it (`meters_ack`); frames arriving meanwhile replace each other.
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
    channel: Mutex<Option<Channel<Value>>>,
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
    fn send(&self, msg: Value) {
        let mut slot = self.channel.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ch) = slot.as_ref()
            && ch.send(msg).is_err()
        {
            *slot = None;
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
            self.send(frame);
        }
    }

    /// The webview has drawn the last spectrum frame: send the newest, if any.
    fn spectrum_ack(&self) {
        let mut mailbox = self.spectrum.lock().unwrap_or_else(|e| e.into_inner());
        if !self.meters_wanted() {
            mailbox.latest = None;
        }
        if let Some(frame) = mailbox.ack() {
            self.send(frame);
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
                        self.send(frame);
                    }
                    return;
                }
                if name == "spectrum" {
                    if !self.meters_wanted() {
                        return;
                    }
                    let mut mailbox = self.spectrum.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(frame) = mailbox.offer(Value::Object(obj)) {
                        self.send(frame);
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

    fn attach(&self, channel: Channel<Value>) {
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
        // A new webview page: nothing it was sent before will be acknowledged.
        *self.meters.lock().unwrap_or_else(|e| e.into_inner()) = MeterMailbox::default();
        *self.spectrum.lock().unwrap_or_else(|e| e.into_inner()) = MeterMailbox::default();
        *self.channel.lock().unwrap_or_else(|e| e.into_inner()) = Some(channel);
    }
}

/// What the tray menu shows; the menu is rebuilt only when this changes.
#[derive(Clone, Debug, Default, PartialEq)]
struct TrayModel {
    up: bool,
    line: String,
    dsp_enabled: bool,
    ab: Option<String>,
    presets: Vec<String>,
    active_preset: Option<String>,
    routing_apps: bool,
    rules: Value,
}

struct Shared {
    client: Arc<DaemonClient>,
    hub: Arc<Hub>,
    tray: Mutex<TrayModel>,
    subscription: Mutex<Option<Subscription>>,
    spectrum_wanted: AtomicBool,
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
fn daemon_subscribe(state: State<'_, AppState>, channel: Channel<Value>) {
    state.hub.attach(channel);
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
            *m = TrayModel::default();
        }
        SubMsg::Event(v) => match v.get("event").and_then(Value::as_str) {
            Some("status") => {
                if let Ok(st) = serde_json::from_value::<Status>(v["data"].clone()) {
                    m.line = tray_text::passthrough_line(&st);
                    m.dsp_enabled = st.dsp_enabled;
                    m.ab = st.ab.filter(|a| a.enabled).map(|a| {
                        serde_json::to_value(a.current)
                            .ok()
                            .and_then(|s| s.as_str().map(str::to_string))
                            .unwrap_or_default()
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

fn build_menu(app: &AppHandle, m: &TrayModel) -> tauri::Result<Menu<tauri::Wry>> {
    let show = MenuItem::with_id(app, "show", "Show Audivane DSP", true, None::<&str>)?;
    let line_text = if m.up {
        if m.line.is_empty() {
            "Engine running".to_string()
        } else {
            m.line.clone()
        }
    } else {
        "Engine not running".to_string()
    };
    let line = MenuItem::with_id(app, "line", line_text, false, None::<&str>)?;
    let bypass = CheckMenuItem::with_id(
        app,
        "bypass",
        "Bypass (bit-perfect)",
        m.up,
        m.up && !m.dsp_enabled,
        None::<&str>,
    )?;
    let ab_on = m.up && m.ab.is_some();
    let side = m.ab.as_deref().unwrap_or("a");
    let ab_a = CheckMenuItem::with_id(
        app,
        "ab-a",
        "Listen to A",
        ab_on,
        ab_on && side == "a",
        None::<&str>,
    )?;
    let ab_b = CheckMenuItem::with_id(
        app,
        "ab-b",
        "Listen to B",
        ab_on,
        ab_on && side == "b",
        None::<&str>,
    )?;
    let mut preset_items: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
    for name in &m.presets {
        preset_items.push(CheckMenuItem::with_id(
            app,
            format!("preset:{name}"),
            name.replace('&', "&&"),
            m.up,
            m.active_preset.as_deref() == Some(name.as_str()),
            None::<&str>,
        )?);
    }
    let refs: Vec<&dyn IsMenuItem<tauri::Wry>> = preset_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<tauri::Wry>)
        .collect();
    let presets = Submenu::with_id_and_items(
        app,
        "presets",
        "Preset",
        m.up && !m.presets.is_empty(),
        &refs,
    )?;
    let r_sys = CheckMenuItem::with_id(
        app,
        "routing-system",
        "System-wide",
        m.up,
        m.up && !m.routing_apps,
        None::<&str>,
    )?;
    let r_apps = CheckMenuItem::with_id(
        app,
        "routing-apps",
        "Selected apps",
        m.up,
        m.up && m.routing_apps,
        None::<&str>,
    )?;
    let routing = Submenu::with_id_and_items(app, "routing", "Routing", m.up, &[&r_sys, &r_apps])?;
    let quit = MenuItem::with_id(
        app,
        "quit",
        "Quit Audivane DSP window (engine keeps running)",
        true,
        None::<&str>,
    )?;
    let sep = || PredefinedMenuItem::separator(app);
    Menu::with_items(
        app,
        &[
            &show,
            &sep()?,
            &line,
            &bypass,
            &ab_a,
            &ab_b,
            &presets,
            &routing,
            &sep()?,
            &quit,
        ],
    )
}

fn refresh_tray(app: &AppHandle) {
    let state = app.state::<AppState>();
    let model = state.tray.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let app2 = app.clone();
    // Menus must be built on the main thread.
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = app2.tray_by_id(TRAY) {
            match build_menu(&app2, &model) {
                Ok(menu) => {
                    let _ = tray.set_menu(Some(menu));
                }
                Err(e) => log::warn!("tray menu: {e}"),
            }
            let tip = if model.up {
                format!("Audivane DSP · {}", model.line)
            } else {
                "Audivane DSP · engine not running".into()
            };
            let _ = tray.set_tooltip(Some(tip));
        }
    });
}

fn on_tray_menu(app: &AppHandle, event: MenuEvent) {
    let id = event.id().as_ref().to_string();
    match id.as_str() {
        "show" => show_main(app),
        "quit" => app.exit(0),
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
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        app.state::<AppState>().hub.set_shown(true, false);
    }
}

/// Create the tray icon. Errors and panics (some appindicator loaders panic
/// when the library is missing) come back as a message.
fn build_tray(app: &tauri::App) -> Result<(), String> {
    let handle = app.handle().clone();
    let icon = app.default_window_icon().cloned();
    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let menu = build_menu(&handle, &TrayModel::default())?;
        let mut tray = TrayIconBuilder::with_id(TRAY)
            .tooltip("Audivane DSP")
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(on_tray_menu);
        if let Some(icon) = icon {
            tray = tray.icon(icon);
        }
        let _tray: TrayIcon = tray.build(&handle)?;
        Ok::<(), tauri::Error>(())
    }));
    match made {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(panic) => Err(panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "the tray failed to start".into())),
    }
}

// ---------------------------------------------------------------- run

pub fn run() {
    let path: PathFn = Arc::new(paths::socket_path);
    let shared: AppState = Arc::new(Shared {
        client: Arc::new(DaemonClient::new(path.clone())),
        hub: Arc::new(Hub::default()),
        tray: Mutex::new(TrayModel::default()),
        subscription: Mutex::new(None),
        spectrum_wanted: AtomicBool::new(false),
    });
    let start_hidden = std::env::args().any(|a| a == "--hidden");

    let app = tauri::Builder::default()
        // Must be first: a second launch just shows the running window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app)
        }))
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED)
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
            ir_info,
            overlays_list,
            overlay_save,
            overlay_delete,
            eq_response,
            eq_auto_preamp
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            // No tray host (no StatusNotifier watcher, no session bus, no
            // appindicator library) must not stop the window: log and go on
            // without a tray. `refresh_tray` then finds no tray and does nothing.
            if let Err(e) = build_tray(app) {
                log::warn!("no tray icon: {e}");
                // Closing the window hides it to the tray; without one, keep it closable.
                shared.hub.no_tray.store(true, Ordering::Relaxed);
            }

            if let Some(w) = app.get_webview_window(MAIN) {
                // Without a tray a hidden window could never be shown again.
                if start_hidden && !shared.hub.no_tray.load(Ordering::Relaxed) {
                    let _ = w.hide();
                } else {
                    shared.hub.set_shown(true, false);
                }
            }

            // Event subscription: forwards to the webview and drives the tray.
            let state = shared.clone();
            let h = handle.clone();
            let sink = Arc::new(move |msg: SubMsg| {
                state.hub.on_message(&msg);
                let mut changed = update_tray_from(&state, &msg);
                if msg == SubMsg::Up {
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
            let sub = Subscription::spawn(path.clone(), sink);
            if shared.spectrum_wanted.load(Ordering::Relaxed) {
                sub.set_spectrum(true);
            }
            *shared
                .subscription
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(sub);
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != MAIN {
                return;
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
                window
                    .app_handle()
                    .state::<AppState>()
                    .hub
                    .set_shown(visible, minimized);
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if app.state::<AppState>().hub.no_tray.load(Ordering::Relaxed) {
                    // Nowhere to hide to: let the window close (the engine keeps running).
                    return;
                }
                // Closing hides to the tray; the engine is a separate service and keeps running.
                api.prevent_close();
                let _ = app.save_window_state(
                    StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED,
                );
                let _ = window.hide();
                app.state::<AppState>().hub.set_shown(false, false);
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build the Audivane DSP window");

    app.run(|handle, event| {
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
