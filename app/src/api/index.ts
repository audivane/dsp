import { MockDaemon, MockTransport } from "./mock";
import { isTauri, TauriTransport } from "./tauri";
import type { Transport } from "./transport";

/** Real daemon inside Tauri, otherwise the in-memory mock. */
export function createTransport(): Transport {
  if (isTauri()) return new TauriTransport();
  const q = new URLSearchParams(typeof location !== "undefined" ? location.search : "");
  return new MockTransport(
    new MockDaemon({
      running: q.get("engine") !== "down",
      apps: q.has("apps"),
      split: q.has("split"),
      overlays: q.get("overlays") === "none" ? "none" : q.get("overlays") === "demo",
      digitalVolume: q.has("digital"),
    }),
  );
}
