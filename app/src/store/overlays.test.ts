import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { flatTarget, tiltTarget, type OverlayData } from "../lib/overlays";
import { AppStore } from "./store";

const until = async (pred: () => boolean, ms = 2000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};

/** Twelve rows of `freq dB`, 100 Hz upwards. */
function curve(offset = 0): string {
  return Array.from({ length: 12 }, (_, i) => `${100 * Math.pow(1.2, i)} ${i + offset}`).join("\n");
}

let stores: AppStore[] = [];
async function setup() {
  const daemon = new MockDaemon({ overlays: true });
  const s = new AppStore(new MockTransport(daemon));
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config);
  await until(() => s.getState().overlays.length > 0);
  return { s, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
});

describe("overlays in the store", () => {
  it("loads the overlay files on connect", async () => {
    const { s } = await setup();
    const names = s.getState().overlays.map((o) => o.name);
    expect(names).toContain("Kali IN-5 (mock room)");
    expect(names).toContain("Harman over-ear 2018");
    expect(s.getState().overlays.find((o) => o.kind === "measurement")?.predicted).toBe(true);
  });

  it("imports a measurement, names it after the file and stores it", async () => {
    const { s, daemon } = await setup();
    s.importOverlay(curve(), "Kali left seat.csv", "measurement");
    await until(() => s.getState().overlays.some((o) => o.name === "Kali left seat"));
    const o = s.getState().overlays.find((x) => x.name === "Kali left seat")!;
    expect(o.kind).toBe("measurement");
    expect(o.data.points).toHaveLength(12);
    await until(() => daemon.overlays.has("Kali left seat"));
    const stored = daemon.overlays.get("Kali left seat") as OverlayData;
    expect(stored.kind).toBe("measurement");
    expect(stored.view?.colour).toBe(o.colour);
  });

  it("keeps file names unique", async () => {
    const { s } = await setup();
    s.importOverlay(curve(), "seat.csv", "measurement");
    s.importOverlay(curve(1), "seat.csv", "measurement");
    await until(() => s.getState().overlays.some((o) => o.name === "seat"));
    await until(() => s.getState().overlays.some((o) => o.name === "seat (2)"));
  });

  it("says what is wrong with a file too short to draw", async () => {
    const { s } = await setup();
    const before = s.getState().overlays.length;
    s.importOverlay("20 1\n30 2\n40 3", "tiny.csv", "target");
    expect(s.getState().overlays).toHaveLength(before);
    expect(s.getState().toast?.tone).toBe("error");
    expect(s.getState().toast?.text).toContain("tiny.csv");
  });

  it("adds a built-in once, then says it is already there", async () => {
    const { s } = await setup();
    s.addOverlay(flatTarget());
    await until(() => s.getState().overlays.some((o) => o.name === "Flat"));
    expect(s.addOverlay(flatTarget())).toBe(false);
    expect(s.getState().overlays.filter((o) => o.name === "Flat")).toHaveLength(1);
    expect(s.getState().toast?.text).toContain("already on the graph");
  });

  it("remembers visibility, colour and offset in the file", async () => {
    const { s, daemon } = await setup();
    const o = s.getState().overlays[0];
    s.updateOverlay(o.id, { visible: false, colour: "#123456", offsetDb: -1.4 });
    const now = s.getState().overlays.find((x) => x.id === o.id)!;
    expect(now.visible).toBe(false);
    expect(now.offsetDb).toBe(-1.4);
    await until(() => (daemon.overlays.get(o.name) as OverlayData).view?.offset_db === -1.4);
    expect((daemon.overlays.get(o.name) as OverlayData).view?.visible).toBe(false);
  });

  it("removes an overlay and its file", async () => {
    const { s, daemon } = await setup();
    const o = s.getState().overlays[0];
    await s.removeOverlay(o.id);
    expect(s.getState().overlays.some((x) => x.id === o.id)).toBe(false);
    expect(daemon.overlays.has(o.name)).toBe(false);
  });

  it("builds preference bounds from two files", async () => {
    const { s } = await setup();
    s.importBounds({ text: curve(-3), name: "lower.csv" }, { text: curve(3), name: "upper.csv" });
    await until(() => s.getState().overlays.some((o) => o.kind === "bounds"));
    const b = s.getState().overlays.find((o) => o.kind === "bounds")!;
    expect(b.name).toBe("lower bounds");
    expect(b.data.lower![0][1]).toBe(-3);
    expect(b.data.upper![0][1]).toBe(3);
  });

  it("keeps the reference frequency in range", async () => {
    const { s } = await setup();
    expect(s.getState().overlayRef).toBe(1000);
    s.setOverlayRef(2000);
    expect(s.getState().overlayRef).toBe(2000);
    s.setOverlayRef(1);
    expect(s.getState().overlayRef).toBe(10);
    s.setOverlayRef(1e9);
    expect(s.getState().overlayRef).toBe(20000);
  });

  it("adds the generated tilt target", async () => {
    const { s } = await setup();
    s.addOverlay(tiltTarget(-0.7));
    await until(() => s.getState().overlays.some((o) => o.name === "Tilt −0.7 dB/oct"));
    expect(s.getState().overlays.find((o) => o.name.startsWith("Tilt"))?.kind).toBe("target");
  });

  it("serialises save and delete per overlay so a slow save cannot resurrect a deleted overlay", async () => {
    const daemon = new MockDaemon({ overlays: true });
    const events: string[] = [];
    class DelayedSaveTransport extends MockTransport {
      override async saveOverlay(name: string, data: unknown): Promise<void> {
        events.push(`save:start:${name}`);
        await new Promise((r) => setTimeout(r, 60));
        await super.saveOverlay(name, data);
        events.push(`save:end:${name}`);
      }
      override async deleteOverlay(name: string): Promise<void> {
        events.push(`delete:start:${name}`);
        await new Promise((r) => setTimeout(r, 1));
        await super.deleteOverlay(name);
        events.push(`delete:end:${name}`);
      }
    }
    const t = new DelayedSaveTransport(daemon);
    const s = new AppStore(t);
    stores.push(s);
    s.connect();
    await until(() => s.getState().conn === "up" && !!s.getState().config);
    await until(() => s.getState().overlays.length > 0);

    const o = s.getState().overlays[0];
    s.updateOverlay(o.id, { offsetDb: 2.5 });
    const removing = s.removeOverlay(o.id);
    await removing;
    await s.whenIdle();

    expect(daemon.overlays.has(o.name)).toBe(false);
    expect(s.getState().overlays.some((x) => x.id === o.id)).toBe(false);
    expect(events).toEqual([
      `save:start:${o.name}`,
      `save:end:${o.name}`,
      `delete:start:${o.name}`,
      `delete:end:${o.name}`,
    ]);
  });
});
