// The UI talks to the daemon through a Transport: the Tauri process (real
// daemon over the Unix socket) or an in-memory mock for the plain browser.

import type { ShellTransport } from "./shell";
import type { DaemonEvent, EngineConfig, ResponseWindowRequest, UpsamplerConfig, UpsamplerResponse } from "./types";

export class DaemonError extends Error {
  constructor(
    public code: number,
    message: string,
  ) {
    super(message);
    this.name = "DaemonError";
  }
}

/** Error codes from crates/protocol/src/ipc.rs (`RpcError`) plus client-side ones. */
export const ErrorCode = {
  PARSE: -32700,
  INVALID_REQUEST: -32600,
  INVALID_PARAMS: -32602,
  INTERNAL: -32603,
  BAD_STATE: 1,
  IO: 2,
  /** `set_volume_guarded` bound (`if_instance`) to a daemon that has since restarted. */
  STALE_INSTANCE: 3,
  /** `set_volume_guarded`'s `if_revision` is not the current level revision. */
  STALE_REVISION: 4,
  /** Client side: no daemon connection. */
  DISCONNECTED: -1,
  /** Client side: no reply in time. */
  TIMEOUT: -2,
  /** Superseded by a newer analysis request (client side, or the daemon's code 5, mapped to this by the Tauri client). */
  SUPERSEDED: -3,
} as const;

export type ConnState = "connecting" | "up" | "down";

/** See `Transport.appImageSetup`. */
export interface AppImageSetup {
  /** A system package is installed: the app uses its engine, no setup. */
  systemPackage: boolean;
  /** The user install's version, if there is one. */
  installed: string | null;
}

/** See `Transport.appUpdatePending`. */
export type AppUpdateState = "ready" | "failed";

export interface TransportHandlers {
  onEvent(e: DaemonEvent): void;
  onConnection(state: ConnState, detail?: string): void;
}

export interface IrInfo {
  channels: number;
  frames: number;
  rate: number;
}

export interface Transport extends ShellTransport {
  readonly kind: "tauri" | "mock";
  /** One request. `params === undefined` omits the field; `null` sends null. */
  call<T = unknown>(method: string, params?: unknown): Promise<T>;
  /** Start the event subscription (reconnects on its own). Returns a stop function. */
  start(handlers: TransportHandlers): () => void;
  /** Subscribe or unsubscribe from live `spectrum` events on the daemon. */
  setSpectrumEnabled(enabled: boolean): void;
  /** The config once the engine is done with every `set_upsampler` sent so
   *  far (a `get_config` behind them on their connection): how a change
   *  whose call timed out turned out, without sending it again. */
  afterUpsamplerBuild(): Promise<EngineConfig>;
  /** Measure the engine's own upsampler filter response (optionally with a zoom window evaluated from the cached measurement). */
  upsamplerResponse(config: UpsamplerConfig | null, sourceRate: number | null, window?: ResponseWindowRequest): Promise<UpsamplerResponse>;
  /** Start the systemd user unit `audivane-dsp.service`. */
  startEngine(): Promise<void>;
  /** Whether the engine starts at login (`null` if systemd cannot say). */
  engineAtLogin(): Promise<boolean | null>;
  /** Enable or disable the engine at login (does not start or stop it). */
  setEngineAtLogin(on: boolean): Promise<void>;
  /** The engine's own last error line when it failed to start (`null` if unknown). */
  engineLastError(): Promise<string | null>;
  /** Restart the systemd user unit (an upgrade left the old engine running). */
  restartEngine(): Promise<void>;
  /** This app's own version, compared with the engine's `status.version`. */
  appVersion(): Promise<string | null>;
  /** Relaunch the app process (it then runs the newly installed code). */
  restartApp(): Promise<void>;
  /** Where an upgrade of the app stands: "ready" (it relaunches itself once
   *  the user leaves the window), "failed" (it could not: restart by hand),
   *  null (nothing pending). */
  appUpdatePending(): Promise<AppUpdateState | null>;
  /** Running from the AppImage: what its per-user setup offer needs
   *  (null when not an AppImage). */
  appImageSetup(): Promise<AppImageSetup | null>;
  /** The AppImage's per-user setup (it starts the engine), then a relaunch
   *  into the installed app: resolves only if that relaunch failed. */
  installAppImage(loginStart: boolean): Promise<void>;
  /** Native file picker for an impulse response; null when cancelled. */
  pickImpulseResponse(): Promise<string | null>;
  /** Native file picker for a room-calibration bundle manifest; null when cancelled. */
  pickCalibrationBundle(): Promise<string | null>;
  /** WAV header facts of an impulse response, if readable by the UI process. */
  irInfo(path: string): Promise<IrInfo | null>;
  copyText(text: string): Promise<void>;
  /**
   * Overlay files (targets, bounds, measurements), stored by the app process in
   * the config directory. The browser mock keeps them in memory.
   */
  listOverlays(): Promise<unknown[]>;
  saveOverlay(name: string, data: unknown): Promise<void>;
  deleteOverlay(name: string): Promise<void>;
}

/** True when the daemon does not know the method (an older engine). */
export function isUnknownMethod(e: unknown): boolean {
  return (
    e instanceof DaemonError &&
    (e.code === ErrorCode.INVALID_REQUEST || /unknown (method|variant)/i.test(e.message))
  );
}
