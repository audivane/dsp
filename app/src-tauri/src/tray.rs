//! The tray icon: our own StatusNotifierItem (the freedesktop/KDE tray
//! protocol over D-Bus, through `ksni`). Unlike an appindicator icon it gets
//! left clicks (`Activate`) with their position, the wheel (`Scroll`) and,
//! on Wayland, the click's activation token, so the popup can take focus.
//!
//! Left click opens the volume popup, or the menu when the user picked that
//! in Device: whether the host opens the menu itself on a left click is a
//! constant of the item (`MENU_ON_ACTIVATE`), so changing the setting replaces
//! the item (`TrayHandle`). Right click always opens the menu.

use crate::app;
use serde_json::Value;
use tauri::AppHandle;

/// What the tray menu shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TrayModel {
    pub up: bool,
    pub line: String,
    pub dsp_enabled: bool,
    pub ab: Option<String>,
    pub presets: Vec<String>,
    pub active_preset: Option<String>,
    pub routing_apps: bool,
    pub rules: Value,
    /// Relaunching after an upgrade failed: offer a restart.
    pub update_pending: bool,
}

pub(crate) struct AvTray<const MENU_ON_LEFT: bool> {
    app: AppHandle,
    model: TrayModel,
    icon: Vec<ksni::Icon>,
}

/// The running tray item, of either kind.
pub(crate) enum TrayHandle {
    Popup(ksni::blocking::Handle<AvTray<false>>),
    Menu(ksni::blocking::Handle<AvTray<true>>),
}

impl TrayHandle {
    /// Start the item. Fails when there is no tray host (no
    /// StatusNotifierWatcher on the session bus), unless `wait_for_host`:
    /// then the icon appears once one starts (an app launched at login
    /// before the panel).
    pub fn spawn(
        app: &AppHandle,
        menu_on_left: bool,
        model: TrayModel,
        wait_for_host: bool,
    ) -> Result<TrayHandle, String> {
        use ksni::blocking::TrayMethods;
        let icon = icon_pixmap(app);
        if menu_on_left {
            AvTray::<true> {
                app: app.clone(),
                model,
                icon,
            }
            .assume_sni_available(wait_for_host)
            .spawn()
            .map(TrayHandle::Menu)
            .map_err(|e| e.to_string())
        } else {
            AvTray::<false> {
                app: app.clone(),
                model,
                icon,
            }
            .assume_sni_available(wait_for_host)
            .spawn()
            .map(TrayHandle::Popup)
            .map_err(|e| e.to_string())
        }
    }

    pub fn menu_on_left(&self) -> bool {
        matches!(self, TrayHandle::Menu(_))
    }

    /// Show a new model (menu, tooltip). Blocks briefly; call off the main thread.
    pub fn set_model(&self, model: TrayModel) {
        match self {
            TrayHandle::Popup(h) => {
                h.update(move |t| t.model = model);
            }
            TrayHandle::Menu(h) => {
                h.update(move |t| t.model = model);
            }
        }
    }

    /// Ask the service to end, without waiting (quit never waits on D-Bus).
    pub fn shutdown(self) {
        match self {
            TrayHandle::Popup(h) => drop(h.shutdown()),
            TrayHandle::Menu(h) => drop(h.shutdown()),
        }
    }

    /// End the service and wait for it, at most `limit` (when replacing the
    /// item, so the new one does not race the old; off the UI thread).
    pub fn shutdown_within(self, limit: std::time::Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            match self {
                TrayHandle::Popup(h) => h.shutdown().wait(),
                TrayHandle::Menu(h) => h.shutdown().wait(),
            }
            let _ = tx.send(());
        });
        if rx.recv_timeout(limit).is_err() {
            log::warn!("the old tray item did not shut down in time");
        }
    }
}

/// The window icon as SNI pixmaps (ARGB32, network byte order).
fn icon_pixmap(app: &AppHandle) -> Vec<ksni::Icon> {
    let Some(img) = app.default_window_icon() else {
        return Vec::new();
    };
    let data = img
        .rgba()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[3], p[0], p[1], p[2]])
        .collect();
    vec![ksni::Icon {
        width: img.width() as i32,
        height: img.height() as i32,
        data,
    }]
}

/// The click's Wayland activation token. It may reach us just after the
/// click itself (KDE sends it separately), so look a few times, briefly.
fn activation_token() -> Option<String> {
    for _ in 0..10 {
        if let Some(t) = ksni::current_xdg_activation_token() {
            return Some(t);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    None
}

/// `_` starts a mnemonic in SNI menu labels; a literal one is doubled.
fn label(s: &str) -> String {
    s.replace('_', "__")
}

impl<const MENU_ON_LEFT: bool> ksni::Tray for AvTray<MENU_ON_LEFT> {
    const MENU_ON_ACTIVATE: bool = MENU_ON_LEFT;

    fn id(&self) -> String {
        "audivane-dsp-ui".into()
    }

    fn title(&self) -> String {
        "Audivane DSP".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let m = &self.model;
        ksni::ToolTip {
            title: "Audivane DSP".into(),
            description: if m.update_pending {
                "Updated: restart to finish".into()
            } else if m.up {
                m.line.clone()
            } else {
                "Engine not running".into()
            },
            icon_pixmap: self.icon.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, x: i32, y: i32) {
        app::on_tray_click(&self.app, (x, y), activation_token());
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        // Middle click: the main window, like most tray apps.
        app::present_main(&self.app, activation_token());
    }

    fn scroll(&mut self, delta: i32, orientation: ksni::Orientation) {
        if matches!(orientation, ksni::Orientation::Vertical) {
            app::on_tray_scroll(&self.app, delta);
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{CheckmarkItem, MenuItem, StandardItem, SubMenu};
        let m = &self.model;
        let act = |id: &'static str| -> Box<dyn Fn(&mut Self) + Send> {
            Box::new(move |t: &mut Self| {
                app::tray_action(&t.app, id.to_string(), activation_token())
            })
        };
        let check =
            |id: &'static str, text: &str, enabled: bool, checked: bool| -> MenuItem<Self> {
                CheckmarkItem {
                    label: text.into(),
                    enabled,
                    checked,
                    activate: act(id),
                    ..Default::default()
                }
                .into()
            };
        let line = if m.up {
            if m.line.is_empty() {
                "Engine running".to_string()
            } else {
                m.line.clone()
            }
        } else {
            "Engine not running".to_string()
        };
        let ab_on = m.up && m.ab.is_some();
        let side = m.ab.as_deref().unwrap_or("a");
        let presets: Vec<MenuItem<Self>> = m
            .presets
            .iter()
            .map(|name| {
                let id = format!("preset:{name}");
                CheckmarkItem {
                    label: label(name),
                    enabled: m.up,
                    checked: m.active_preset.as_deref() == Some(name.as_str()),
                    activate: Box::new(move |t: &mut Self| {
                        app::tray_action(&t.app, id.clone(), None)
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        let mut items: Vec<MenuItem<Self>> = Vec::new();
        if m.update_pending {
            items.push(
                StandardItem {
                    label: "Restart to finish the update".into(),
                    activate: act("restart"),
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }
        items.extend([
            StandardItem {
                label: "Show Audivane DSP".into(),
                activate: act("show"),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Volume…".into(),
                enabled: m.up,
                activate: act("popup"),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: label(&line),
                enabled: false,
                ..Default::default()
            }
            .into(),
            check(
                "bypass",
                "Bypass (bit-perfect)",
                m.up,
                m.up && !m.dsp_enabled,
            ),
        ]);
        if side == "blind" {
            // A blind test runs in the app: no side shown, and no switching
            // from here.
            items.push(
                StandardItem {
                    label: "A/B: blind".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            items.push(check("ab-a", "Listen to A", ab_on, ab_on && side == "a"));
            items.push(check("ab-b", "Listen to B", ab_on, ab_on && side == "b"));
        }
        items.extend([
            SubMenu {
                label: "Preset".into(),
                enabled: m.up && !m.presets.is_empty(),
                submenu: presets,
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Routing".into(),
                enabled: m.up,
                submenu: vec![
                    check(
                        "routing-system",
                        "System-wide",
                        m.up,
                        m.up && !m.routing_apps,
                    ),
                    check(
                        "routing-apps",
                        "Selected apps",
                        m.up,
                        m.up && m.routing_apps,
                    ),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit Audivane DSP window (engine keeps running)".into(),
                activate: act("quit"),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}
