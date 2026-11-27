// Transport for the real app: the Tauri process owns the Unix-socket client
// (src-tauri/src/client.rs) and forwards daemon events over an ipc Channel.

import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { KeysState, MainViewMemory, OsdPayload, ShellEvent, ShellView, UiSettings, WindowLabel } from "./shell";
import { type AppImageSetup, type AppUpdateState, DaemonError, ErrorCode, type IrInfo, type Transport, type TransportHandlers } from "./transport";
import type { DaemonEvent, EngineConfig, ResponseWindowRequest, UpsamplerConfig, UpsamplerResponse } from "./types";

/** Messages on the `daemon_subscribe` channel (see `Hub` in src-tauri/src/app.rs). */
type UiMessage =
  | { kind: "connection"; state: "connecting" | "up" | "down"; detail?: string | null }
  | ({ kind: "event" } & DaemonEvent);

function toError(e: unknown): DaemonError {
  if (e && typeof e === "object" && "code" in e && "message" in e) {
    const o = e as { code: number; message: string };
    return new DaemonError(o.code, o.message);
  }
  return new DaemonError(ErrorCode.INTERNAL, typeof e === "string" ? e : "Unexpected error from the app process");
}

export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export class TauriTransport implements Transport {
  readonly kind = "tauri" as const;
  private analysisSeq = 0;

  async afterUpsamplerBuild(): Promise<EngineConfig> {
    try {
      return await invoke<EngineConfig>("daemon_call", { request: { method: "get_config", lane: "upsampler" } });
    } catch (e) {
      throw toError(e);
    }
  }

  async call<T>(method: string, params?: unknown): Promise<T> {
    const request: Record<string, unknown> = { method };
    if (params !== undefined) request.params = params;
    try {
      return await invoke<T>("daemon_call", { request });
    } catch (e) {
      throw toError(e);
    }
  }

  start(h: TransportHandlers): () => void {
    const channel = new Channel<UiMessage>();
    let live = true;
    // The app process sends one meter/spectrum frame at a time and waits for
    // this acknowledgement before the next (newer frames replace older ones
    // meanwhile). Acknowledging on the next animation frame ties the rate to
    // what the page can draw: a hidden or suspended page acknowledges nothing.
    const ackFrame = (cmd: "meters_ack" | "spectrum_ack") => {
      const ack = () => void invoke(cmd).catch(() => undefined);
      if (typeof requestAnimationFrame === "function") requestAnimationFrame(ack);
      else setTimeout(ack, 0);
    };
    channel.onmessage = (m) => {
      if (m.kind === "event" && m.event === "meters") ackFrame("meters_ack");
      if (m.kind === "event" && m.event === "spectrum") ackFrame("spectrum_ack");
      if (!live) return;
      if (m.kind === "connection") h.onConnection(m.state, m.detail ?? undefined);
      else {
        const { kind: _kind, ...event } = m;
        h.onEvent(event as DaemonEvent);
      }
    };
    h.onConnection("connecting");
    invoke("daemon_subscribe", { channel }).catch((e) => h.onConnection("down", toError(e).message));
    return () => {
      live = false;
    };
  }

  setSpectrumEnabled(enabled: boolean): void {
    void invoke("spectrum_enable", { enabled }).catch(() => undefined);
  }

  async upsamplerResponse(config: UpsamplerConfig | null, sourceRate: number | null, window?: ResponseWindowRequest): Promise<UpsamplerResponse> {
    const seq = ++this.analysisSeq;
    try {
      const params = window ? { config, source_rate: sourceRate, window } : { config, source_rate: sourceRate };
      const res = await this.call<UpsamplerResponse>("upsampler_response", params);
      if (seq !== this.analysisSeq) {
        throw new DaemonError(ErrorCode.SUPERSEDED, "Superseded by a newer measurement.");
      }
      return res;
    } catch (e) {
      if (seq !== this.analysisSeq) {
        throw new DaemonError(ErrorCode.SUPERSEDED, "Superseded by a newer measurement.");
      }
      throw e;
    }
  }

  async startEngine(): Promise<void> {
    try {
      await invoke("start_engine");
    } catch (e) {
      throw toError(e);
    }
  }

  async engineAtLogin(): Promise<boolean | null> {
    return invoke<boolean | null>("engine_at_login").catch(() => null);
  }

  async setEngineAtLogin(on: boolean): Promise<void> {
    try {
      await invoke("set_engine_at_login", { enabled: on });
    } catch (e) {
      throw toError(e);
    }
  }

  async engineLastError(): Promise<string | null> {
    return invoke<string | null>("engine_last_error").catch(() => null);
  }

  async restartEngine(): Promise<void> {
    try {
      await invoke("restart_engine");
    } catch (e) {
      throw toError(e);
    }
  }

  async appVersion(): Promise<string | null> {
    return invoke<string>("app_version").catch(() => null);
  }

  async appUpdatePending(): Promise<AppUpdateState | null> {
    return invoke<AppUpdateState | null>("app_update_pending").catch(() => null);
  }

  async restartApp(): Promise<void> {
    try {
      await invoke("restart_app");
    } catch (e) {
      throw toError(e);
    }
  }

  async appImageSetup(): Promise<AppImageSetup | null> {
    return invoke<AppImageSetup | null>("appimage_setup").catch(() => null);
  }

  async installAppImage(loginStart: boolean): Promise<void> {
    try {
      await invoke("appimage_install", { loginStart });
    } catch (e) {
      throw toError(e);
    }
  }

  async pickImpulseResponse(): Promise<string | null> {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      title: "Add impulse response",
      multiple: false,
      directory: false,
      filters: [{ name: "WAV impulse response", extensions: ["wav", "WAV"] }],
    });
    return typeof picked === "string" ? picked : null;
  }

  async pickCalibrationBundle(): Promise<string | null> {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      title: "Load room calibration",
      multiple: false,
      directory: false,
      filters: [{ name: "Calibration bundle", extensions: ["json", "JSON"] }],
    });
    return typeof picked === "string" ? picked : null;
  }

  async irInfo(path: string): Promise<IrInfo | null> {
    try {
      return await invoke<IrInfo | null>("ir_info", { path });
    } catch {
      return null;
    }
  }

  async copyText(text: string): Promise<void> {
    const { writeText } = await import("@tauri-apps/plugin-clipboard-manager");
    await writeText(text);
  }

  async listOverlays(): Promise<unknown[]> {
    try {
      return await invoke<unknown[]>("overlays_list");
    } catch (e) {
      throw toError(e);
    }
  }

  async saveOverlay(name: string, data: unknown): Promise<void> {
    try {
      await invoke("overlay_save", { name, data });
    } catch (e) {
      throw toError(e);
    }
  }

  async deleteOverlay(name: string): Promise<void> {
    try {
      await invoke("overlay_delete", { name });
    } catch (e) {
      throw toError(e);
    }
  }

  // ---------------------------------------------------------------- shell

  windowLabel(): WindowLabel {
    const l = getCurrentWindow().label;
    return l === "popup" || l === "osd" ? l : "main";
  }

  async shellView(): Promise<ShellView> {
    return invoke<ShellView>("ui_settings");
  }

  async setUiSettings(settings: UiSettings): Promise<ShellView> {
    try {
      return await invoke<ShellView>("set_ui_settings", { settings });
    } catch (e) {
      throw toError(e);
    }
  }

  async setAutostart(enabled: boolean): Promise<ShellView> {
    try {
      return await invoke<ShellView>("set_autostart", { enabled });
    } catch (e) {
      throw toError(e);
    }
  }

  onShell(handler: (e: ShellEvent) => void): () => void {
    let live = true;
    const offs: (() => void)[] = [];
    const on = <P>(name: string, map: (p: P) => ShellEvent) => {
      void listen<P>(name, (e) => {
        if (live) handler(map(e.payload));
      }).then((off) => (live ? offs.push(off) : off()));
    };
    on<ShellView>("ui-settings", (view) => ({ kind: "ui-settings", view }));
    on<KeysState>("keys-state", (keys) => ({ kind: "keys-state", keys }));
    on<null>("popup-shown", () => ({ kind: "popup-shown" }));
    on<null>("main-closing", () => ({ kind: "main-closing" }));
    on<null>("update-pending", () => ({ kind: "update-pending", state: "failed" }));
    on<null>("update-ready", () => ({ kind: "update-pending", state: "ready" }));
    on<OsdPayload>("osd", (payload) => ({ kind: "osd", payload }));
    return () => {
      live = false;
      for (const off of offs) off();
    };
  }

  showMainWindow(): void {
    void invoke("show_main_window").catch(() => undefined);
  }

  hidePopup(): void {
    void invoke("popup_hide").catch(() => undefined);
  }

  osdShow(payload: OsdPayload, source: "keys" | "scroll"): void {
    void invoke("osd_show", { payload, source }).catch(() => undefined);
  }

  async osdReady(): Promise<OsdPayload | null> {
    return invoke<OsdPayload | null>("osd_ready").catch(() => null);
  }

  saveMainView(view: MainViewMemory): void {
    void invoke("main_view_save", { view }).catch(() => undefined);
  }

  mainCloseReady(view: MainViewMemory | null): void {
    void invoke("main_close_ready", { view }).catch(() => undefined);
  }

  async loadMainView(): Promise<MainViewMemory | null> {
    return invoke<MainViewMemory | null>("main_view_load").catch(() => null);
  }
}
