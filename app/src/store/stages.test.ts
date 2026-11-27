// The effect stages' setters (targeted calls with a draft and the daemon's
// echo), the volume settings (get_config + set_config, never the level) and
// the EQ tilt, against the mock daemon.

import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { DaemonError, ErrorCode } from "../api/transport";
import { AppStore, clearStash } from "./store";

const until = async (pred: () => boolean, ms = 3000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};

class SpyTransport extends MockTransport {
  calls: { method: string; params: unknown }[] = [];
  refuse = new Set<string>();
  override async call<T>(method: string, params?: unknown): Promise<T> {
    this.calls.push({ method, params });
    if (this.refuse.has(method)) throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
    return super.call<T>(method, params);
  }
  count(method: string) {
    return this.calls.filter((c) => c.method === method).length;
  }
  last(method: string) {
    return [...this.calls].reverse().find((c) => c.method === method)?.params;
  }
}

let stores: AppStore[] = [];
async function setup(opts: { refuse?: string[]; digital?: boolean } = {}) {
  const daemon = new MockDaemon({ digitalVolume: opts.digital });
  const t = new SpyTransport(daemon, { request: 1, reply: 1, event: 1 });
  for (const m of opts.refuse ?? []) t.refuse.add(m);
  const s = new AppStore(t);
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
  return { s, t, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  clearStash();
});

describe("volume settings", () => {
  it("go through set_config and never write the level", async () => {
    const { s, t, daemon } = await setup();
    const level = daemon.volume.volume_db;
    const sets = t.count("set_volume") + t.count("set_volume_guarded");
    await s.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, mode: "lock" }, sink_volume: "follow", volume_db: 0, muted: true }));
    expect(t.count("set_config")).toBe(1);
    expect(t.count("set_volume") + t.count("set_volume_guarded")).toBe(sets);
    const sent = t.last("set_config") as { volume: Record<string, unknown> };
    expect(sent.volume).not.toHaveProperty("volume_db");
    expect(sent.volume).not.toHaveProperty("muted");
    expect(daemon.config.volume.guard.mode).toBe("lock");
    expect(daemon.volume.volume_db).toBe(level);
    await until(() => s.getState().status?.volume?.guard === "lock" && s.getState().status?.volume?.sink_volume === "follow");
  });

  it("a ceiling below the level lowers it; hybrid needs a DAC level", async () => {
    const { s, daemon } = await setup();
    await s.setVolumeSettings((v) => ({ ...v, max_volume_db: -20 }));
    expect(daemon.volume.volume_db).toBe(-20);
    await until(() => s.getState().status?.volume?.ceiling_db === -20);
    const ok = await s.setVolumeSettings((v) => ({ ...v, mode: "hybrid", hardware_db: null }));
    expect(ok).toBe(false);
    expect(s.getState().stageErrors.volume).toMatch(/Hybrid needs a DAC level/);
  });

});

describe("review fixes", () => {

  it("volume settings leave the level keys out of set_config", async () => {
    const { s, t } = await setup();
    await s.setVolumeSettings((v) => ({ ...v, sink_volume: "follow" }));
    const sent = t.last("set_config") as { volume: Record<string, unknown> };
    expect(sent.volume).toBeDefined();
    expect("volume_db" in sent.volume).toBe(false);
    expect("balance_db" in sent.volume).toBe(false);
    expect("muted" in sent.volume).toBe(false);
  });

});

