import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { AppStore, clearStash } from "./store";
import { resetShell } from "./shell";

const until = async (pred: () => boolean, ms = 2000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};

class SpyTransport extends MockTransport {
  calls: { method: string; params: unknown }[] = [];
  override async call<T>(method: string, params?: unknown): Promise<T> {
    this.calls.push({ method, params });
    return super.call<T>(method, params);
  }
  sent(method: string) {
    return this.calls.filter((c) => c.method === method).map((c) => c.params as Record<string, unknown>);
  }
}

let stores: AppStore[] = [];
async function setup(opts: { digital?: boolean; secondary?: boolean } = {}) {
  const daemon = new MockDaemon({ digitalVolume: opts.digital });
  const t = new SpyTransport(daemon);
  const s = new AppStore(t, { secondary: opts.secondary });
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status && s.getState().volumeFresh);
  return { s, t, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  clearStash();
  resetShell();
});

describe("volume keys through the store", () => {
  it("a step is a guarded write of the volume only, and quick presses add up", async () => {
    const { s, t, daemon } = await setup();
    const start = s.volume()!.volume_db;
    const shown = s.volumeKey("up", 1);
    expect(shown?.volume_db).toBe(start + 1);
    s.volumeKey("up", 1);
    s.volumeKey("up", 1);
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(start + 3);
    const sends = t.sent("set_volume_guarded");
    expect(sends.length).toBeGreaterThan(0);
    for (const p of sends) {
      expect(Object.keys(p).sort()).toEqual(["if_instance", "if_revision", "volume_db"]);
    }
    expect(t.sent("set_volume")).toHaveLength(0);
  });

});

