import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport, type MockLatency } from "../api/mock";
import { DaemonError, ErrorCode } from "../api/transport";
import { profilePeakGainDb } from "../lib/eqmath";
import { AppStore, clearStash } from "./store";

const until = async (pred: () => boolean, ms = 2000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Records every call the store makes. */
class SpyTransport extends MockTransport {
  calls: { method: string; params: unknown }[] = [];
  refuse = new Set<string>();
  /** Calls of these methods wait for the promise before they are delivered. */
  hold = new Map<string, Promise<void>>();
  /** Calls of these methods run now; their reply waits for the promise. */
  holdReply = new Map<string, Promise<void>>();
  /** The next call of each of these methods times out. */
  timeoutOnce = new Set<string>();
  override async call<T>(method: string, params?: unknown): Promise<T> {
    this.calls.push({ method, params });
    if (this.refuse.has(method)) throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
    if (this.timeoutOnce.delete(method)) throw new DaemonError(ErrorCode.TIMEOUT, "The engine did not answer in time.");
    const gate = this.hold.get(method);
    if (gate) await gate;
    const replyGate = this.holdReply.get(method);
    const result = await super.call<T>(method, params);
    if (replyGate) await replyGate;
    return result;
  }
  count(method: string) {
    return this.calls.filter((c) => c.method === method).length;
  }
}

let stores: AppStore[] = [];
async function setup(opts: { refuse?: string[]; latency?: MockLatency; legacyRatio?: boolean } = {}) {
  const daemon = new MockDaemon({ legacyRatio: opts.legacyRatio });
  const t = new SpyTransport(daemon, opts.latency);
  for (const m of opts.refuse ?? []) t.refuse.add(m);
  const s = new AppStore(t);
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
  await until(() => s.getState().caps.presets !== null && s.getState().caps.devices !== null);
  return { s, t, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  // Remembered settings (last preset, rate targets, limiter mode) live in
  // memory when localStorage is unavailable: start each test without them.
  clearStash();
});

describe("store", () => {

  it("save refuses an existing name without overwrite (BAD_STATE), like the daemon", async () => {
    const { t } = await setup();
    await expect(t.call("save_preset", { name: "Flat", overwrite: false })).rejects.toMatchObject({ code: ErrorCode.BAD_STATE });
    const info = await t.call<{ name: string; bands: number }>("save_preset", { name: "Flat", overwrite: true });
    expect(info).toMatchObject({ name: "Flat", bands: 7 });
  });

  it("the last throttled drag or wheel step reaches the daemon's stored config, even without a commit", async () => {
    const { s, daemon } = await setup();
    for (let i = 0; i < 15; i++) {
      s.setVolume({ volume_db: -20 + i * 0.5 }, "drag");
      await sleep(1);
    }
    await until(() => daemon.volume.volume_db === -13);
    expect(daemon.config.volume?.volume_db).toBe(-13);
    // The wheel's closing commit repeats the value: still one level, stored.
    s.setVolume({ volume_db: -13 });
    await s.whenIdle();
    expect(daemon.config.volume?.volume_db).toBe(-13);
    expect(s.getState().volumeDraft).toBeNull();
    expect(s.getState().status?.volume?.volume_db).toBe(-13);
  });

  it("a drag interrupted by a reconnect never survives it", async () => {
    const { s, t, daemon } = await setup({ latency: { request: 4, reply: 30 } });
    // An uncommitted drag up to 0 dB (pointer still held), then the
    // daemon restarts at -30 dB.
    s.setVolume({ volume_db: -10 }, "drag");
    s.setVolume({ volume_db: 0 }, "drag");
    daemon.volume = { ...daemon.volume, volume_db: -30 };
    if (daemon.config.volume) daemon.config.volume = { ...daemon.config.volume, volume_db: -30 };
    daemon.restartEngine();
    await until(() => s.getState().conn === "down");
    await until(() => s.getState().conn === "up" && s.getState().status?.volume?.volume_db === -30);
    const sentAfter = t.count("set_volume_guarded");
    await sleep(120);
    // Nothing queued or in flight from the old connection lands now.
    expect(t.count("set_volume_guarded")).toBe(sentAfter);
    expect(daemon.volume.volume_db).toBe(-30);
    expect(s.getState().volumeDraft).toBeNull();
    expect(s.volume()?.volume_db).toBe(-30);
    // Volume Down is a step down from what the daemon reports.
    const v = s.volume()!;
    s.setVolume({ volume_db: v.volume_db - 0.5 });
    await s.whenIdle();
    expect(daemon.volume.volume_db).toBe(-30.5);
    expect(daemon.config.volume?.volume_db).toBe(-30.5);
    expect(s.getState().status?.volume?.volume_db).toBe(-30.5);
  });

  it("a set_volume dispatched before a restart and delivered after it is refused", async () => {
    const { s, t, daemon } = await setup();
    let release!: () => void;
    t.hold.set("set_volume_guarded", new Promise<void>((r) => (release = r)));
    s.setVolume({ volume_db: 0, balance_db: -2, muted: false });
    await until(() => t.count("set_volume_guarded") > 0);
    daemon.volume = { ...daemon.volume, volume_db: -30, muted: true };
    if (daemon.config.volume) daemon.config.volume = { ...daemon.config.volume, volume_db: -30, muted: true };
    daemon.restartEngine();
    await until(() => s.getState().conn === "down");
    await until(() => s.getState().conn === "up" && s.volume()?.volume_db === -30);
    // The old request now reaches the replacement daemon.
    t.hold.delete("set_volume_guarded");
    release();
    await sleep(50);
    expect(daemon.volume).toMatchObject({ volume_db: -30, balance_db: 0, muted: true });
    expect(daemon.config.volume).toMatchObject({ volume_db: -30, balance_db: 0, muted: true });
    expect(s.volume()).toMatchObject({ volume_db: -30, muted: true });
  });

  it("every app volume write is a compare-and-set on the status it came from", async () => {
    const { s, t } = await setup();
    const v0 = s.getState().status!.volume!;
    s.setVolume({ volume_db: -13 }, "drag");
    s.setVolume({ volume_db: -14 });
    await s.whenIdle();
    const sends = t.calls.filter((c) => c.method === "set_volume_guarded").map((c) => c.params as Record<string, unknown>);
    expect(sends.length).toBeGreaterThan(0);
    for (const p of sends) {
      expect(p.if_instance).toBe(v0.instance);
      expect(typeof p.if_revision).toBe("number");
    }
    // The first step from the status's revision, the next from its echo.
    expect(sends[0].if_revision).toBe(v0.revision);
    if (sends.length > 1) expect(sends[1].if_revision).toBeGreaterThan(v0.revision!);
  });

  it("a daemon without instance and revision gets no volume writes", async () => {
    const daemon = new MockDaemon();
    daemon.legacyVolume = true;
    const t = new SpyTransport(daemon);
    const s = new AppStore(t);
    stores.push(s);
    s.connect();
    await until(() => s.getState().conn === "up" && !!s.getState().status?.volume);
    expect(s.volume()).toBeNull();
    s.setVolume({ volume_db: -20 });
    s.setVolume({ muted: true });
    await sleep(100);
    expect(t.count("set_volume_guarded")).toBe(0);
    // And it does not keep re-reading the status for a revision.
    const reads = t.count("status");
    await sleep(300);
    expect(t.count("status")).toBeLessThanOrEqual(reads + 1);
  });

  it("floors auto-preamp cleanly so peak EQ response never exceeds 0 dB", async () => {
    const { s } = await setup();
    s.setManualPreamp(0);
    await s.whenIdle();
    const eqBoosted = s.eq()!;
    expect(profilePeakGainDb(eqBoosted, s.graphRate())).toBeGreaterThan(1);

    s.setAutoPreamp(true);
    await s.whenIdle();
    const eqFixed = s.eq()!;
    expect(profilePeakGainDb(eqFixed, s.graphRate())).toBeLessThanOrEqual(0);
  });

});
