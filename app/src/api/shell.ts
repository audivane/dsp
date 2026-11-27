// The desktop shell around the webviews: the app's own settings (tray click,
// volume keys, on-screen display, start at login), the volume keys' state,
// and the messages between the three windows (main, the tray's volume
// popup, the on-screen display). Served by src-tauri/src/app.rs; the mock
// keeps the same shapes in memory.

/** What a left click on the tray icon does. */
export type TrayClick = "popup" | "menu";
/** When the on-screen display shows: never, after a volume key (or the
 *  tray wheel), or after any level change. */
export type OsdMode = "off" | "keys" | "any";

/** Mirrors `UiSettings` in src-tauri/src/ui_settings.rs. */
export interface UiSettings {
  tray_click: TrayClick;
  volume_keys: boolean;
  /** 0.5 … 3 dB, half-dB steps. */
  volume_step_db: number;
  osd: OsdMode;
  /** Turn the wheel over the tray icon the other way round. */
  reverse_wheel: boolean;
}

export const STEP_MIN_DB = 0.5;
export const STEP_MAX_DB = 3;

export type KeyAction = "up" | "down" | "mute";

/** Mirrors `KeysState` in src-tauri/src/shortcuts.rs. */
export type KeysState =
  | { state: "off" }
  | { state: "starting" }
  | { state: "active"; via: "portal" | "x11"; bound: { action: KeyAction; trigger: string }[] }
  | { state: "declined" }
  | { state: "unavailable"; reason: string };

export interface ShellView {
  settings: UiSettings;
  /** The start-at-login entry exists. */
  autostart: boolean;
  keys: KeysState;
  /** A tray icon is up (a tray host is running). */
  tray: boolean;
  /** Running in a KDE Plasma session (`XDG_CURRENT_DESKTOP` lists KDE). */
  kde: boolean;
}

/** What the on-screen display shows. */
export interface OsdPayload {
  volume_db: number | null;
  muted: boolean;
  min_db?: number | null;
  max_db?: number | null;
  /** Instead of a level: why there is none ("Volume is on the DAC"). */
  note?: string | null;
}

export type WindowLabel = "main" | "popup" | "osd";

/** What the main page gets back when its window is made again: closing it
 *  to the tray destroys the webview, so the app process keeps this. */
export interface MainViewMemory {
  view: string;
  scroll_top: number;
}

export type ShellEvent =
  /** Browser mock only: in the app the app process handles the keys itself. */
  | { kind: "volume-key"; action: KeyAction; step_db: number; source: "keys" | "scroll" }
  | { kind: "ui-settings"; view: ShellView }
  | { kind: "keys-state"; keys: KeysState }
  | { kind: "popup-shown" }
  /** The main window is closing to the tray: hand over its place now
   *  (`mainCloseReady`); the page is destroyed right after. */
  | { kind: "main-closing" }
  /** The app was upgraded: it relaunches once the user leaves the window
   *  ("ready"), or it could not relaunch ("failed"). */
  | { kind: "update-pending"; state: "ready" | "failed" }
  | { kind: "osd"; payload: OsdPayload };

export interface ShellTransport {
  /** Which window this page is. */
  windowLabel(): WindowLabel;
  shellView(): Promise<ShellView>;
  setUiSettings(s: UiSettings): Promise<ShellView>;
  setAutostart(on: boolean): Promise<ShellView>;
  /** Shell messages for this page. Returns a stop function. */
  onShell(handler: (e: ShellEvent) => void): () => void;
  showMainWindow(): void;
  hidePopup(): void;
  osdShow(p: OsdPayload, source: "keys" | "scroll"): void;
  /** The OSD page is listening: the latest payload, if any. */
  osdReady(): Promise<OsdPayload | null>;
  /** Keep the main view's place for when the window is made again. */
  saveMainView(m: MainViewMemory): void;
  loadMainView(): Promise<MainViewMemory | null>;
  /** The answer to `main-closing`: the place to keep (null: none). */
  mainCloseReady(m: MainViewMemory | null): void;
}

export const DEFAULT_UI_SETTINGS: UiSettings = { tray_click: "popup", volume_keys: false, volume_step_db: 1, osd: "keys", reverse_wheel: false };
