// The desktop shell's state for the settings panel: the app's own settings
// (tray click, volume keys, on-screen display), start at login and the
// volume keys' state. The app process owns them (src-tauri/src/app.rs) and
// announces every change, also ones made from another window; this is the
// last view it sent, nothing more.

import { useSyncExternalStore } from "react";
import type { ShellEvent, ShellView, UiSettings } from "../api/shell";
import type { Transport } from "../api/transport";

export interface ShellState {
  view: ShellView | null;
  /** The last refusal (a settings file that cannot be written), shown inline. */
  error: string | null;
}

let transport: Transport | null = null;
let state: ShellState = { view: null, error: null };
const listeners = new Set<() => void>();

function set(patch: Partial<ShellState>) {
  state = { ...state, ...patch };
  for (const l of listeners) l();
}

function onEvent(e: ShellEvent) {
  if (e.kind === "ui-settings") set({ view: e.view });
  else if (e.kind === "keys-state" && state.view) set({ view: { ...state.view, keys: e.keys } });
}

/** Start following the shell (once per page). Returns a stop function. */
export function initShell(t: Transport): () => void {
  transport = t;
  const off = t.onShell(onEvent);
  void t
    .shellView()
    .then((view) => set({ view }))
    .catch(() => undefined);
  return off;
}

/** The main page's shell transport (null in the popup and the OSD). */
export function shellTransport(): Transport | null {
  return transport;
}

export function shellState(): ShellState {
  return state;
}

export function useShell<T>(select: (s: ShellState) => T): T {
  return useSyncExternalStore(
    (fn) => {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    () => select(state),
  );
}

function message(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  return m ? m.charAt(0).toUpperCase() + m.slice(1) : "The app could not save that.";
}

/** Change some of the app's settings. */
export async function setUiSettings(patch: Partial<UiSettings>): Promise<void> {
  if (!transport || !state.view) return;
  const next = { ...state.view.settings, ...patch };
  // Shown at once; the app's answer replaces it.
  set({ view: { ...state.view, settings: next }, error: null });
  try {
    set({ view: await transport.setUiSettings(next) });
  } catch (e) {
    set({ error: message(e) });
    void transport
      .shellView()
      .then((view) => set({ view }))
      .catch(() => undefined);
  }
}

export async function setAutostart(on: boolean): Promise<void> {
  if (!transport || !state.view) return;
  set({ error: null });
  try {
    set({ view: await transport.setAutostart(on) });
  } catch (e) {
    set({ error: message(e) });
  }
}

/** Tests: forget everything. */
export function resetShell() {
  transport = null;
  state = { view: null, error: null };
}
