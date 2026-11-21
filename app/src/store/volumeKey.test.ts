import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { AppStore, clearStash } from "./store";
import { initShell, resetShell, setAutostart, setUiSettings, shellState } from "./shell";

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

  it("mute writes only the mute and the display says so", async () => {
    const { s, t, daemon } = await setup();
    const shown = s.volumeKey("mute", 1);
    expect(shown?.muted).toBe(true);
    await s.whenIdle();
    expect(daemon.volume.muted).toBe(true);
    expect(Object.keys(t.sent("set_volume_guarded").at(-1)!).sort()).toEqual(["if_instance", "if_revision", "muted"]);
  });

  it("at the top of the range nothing is sent, but the display still shows the level", async () => {
    const { s, t } = await setup();
    s.setVolume({ volume_db: 0 });
    await s.whenIdle();
    const before = t.sent("set_volume_guarded").length;
    const shown = s.volumeKey("up", 2);
    await s.whenIdle();
    expect(shown?.volume_db).toBe(0);
    expect(t.sent("set_volume_guarded").length).toBe(before);
  });

  it("digital volume on Bypass: no write, the display explains", async () => {
    const { s, t } = await setup({ digital: true });
    s.setBypass(true);
    await s.whenIdle();
    await until(() => s.getState().config?.dsp_enabled === false);
    const before = t.sent("set_volume_guarded").length;
    const shown = s.volumeKey("down", 1);
    expect(shown?.note).toMatch(/on the DAC/);
    await s.whenIdle();
    expect(t.sent("set_volume_guarded").length).toBe(before);
  });

  it("a secondary (popup) store writes the volume but leaves the main window's things alone", async () => {
    const { s, t } = await setup({ secondary: true });
    expect(t.calls.some((c) => c.method === "list_presets" || c.method === "list_devices")).toBe(false);
    s.volumeKey("down", 1);
    await s.whenIdle();
    expect(t.sent("set_volume_guarded").length).toBe(1);
  });
});

describe("shell settings", () => {
  it("loads, changes and follows the keys' state", async () => {
    const t = new MockTransport(new MockDaemon());
    initShell(t);
    await until(() => shellState().view !== null);
    expect(shellState().view!.settings).toEqual({ tray_click: "popup", volume_keys: false, volume_step_db: 1, osd: "keys", reverse_wheel: false });
    await setUiSettings({ volume_step_db: 2.4 });
    expect(shellState().view!.settings.volume_step_db).toBe(2.5);
    await setUiSettings({ volume_keys: true });
    expect(shellState().view!.keys.state).toBe("starting");
    await until(() => shellState().view!.keys.state === "active");
    await setAutostart(true);
    expect(shellState().view!.autostart).toBe(true);
    await setUiSettings({ volume_keys: false });
    expect(shellState().view!.keys.state).toBe("off");
  });
});
