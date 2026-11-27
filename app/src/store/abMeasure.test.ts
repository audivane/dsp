// "Match to this track" through the store, against the mock daemon's
// simulated measurement: progress, a lock, an early accept, a cancel and
// the ends without a lock.

import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { AppStore, clearStash } from "./store";

const until = async (pred: () => boolean, ms = 8000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 10));
  }
};

let stores: AppStore[] = [];
let daemons: MockDaemon[] = [];
async function setup() {
  const daemon = new MockDaemon();
  daemon.config.ab = null;
  daemons.push(daemon);
  const s = new AppStore(new MockTransport(daemon, { request: 1, reply: 1, event: 1 }));
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
  daemon.config.chain.crossfeed = { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, width: 1, width_shape: "flat", width_corner_hz: 700 };
  await s.compareStage("crossfeed");
  await s.whenIdle();
  return { s, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  for (const d of daemons) d.stop();
  stores = [];
  daemons = [];
  clearStash();
});

const ab = (s: AppStore) => s.getState().status?.ab ?? null;

describe("match to this track", () => {
  it("reports progress, then locks a measured level", async () => {
    const { s, daemon } = await setup();
    expect(await s.abMeasure("start")).toBeNull();
    expect(ab(s)?.measuring).toMatchObject({ seconds: 0, offset_db: null, max_seconds: 60 });
    await until(() => (ab(s)?.measuring?.seconds ?? 0) >= 4);
    expect(ab(s)?.measuring?.offset_db).not.toBeNull();
    await until(() => s.getState().config?.ab?.level.mode === "measured");
    const level = daemon.config.ab?.level;
    expect(level?.mode).toBe("measured");
    if (level?.mode === "measured") expect(level.seconds).toBeGreaterThanOrEqual(20);
    await until(() => ab(s)?.measuring === null);
    expect(ab(s)?.measure_error).toBeNull();
    expect(ab(s)?.level.mode).toBe("measured");
  }, 15000);

  it("Use now locks early, and is refused before anything was measured", async () => {
    const { s } = await setup();
    await s.abMeasure("start");
    expect(await s.abMeasure("accept")).toContain("Nothing measured yet");
    await until(() => ab(s)?.measuring?.offset_db != null);
    expect(await s.abMeasure("accept")).toBeNull();
    expect(s.getState().config?.ab?.level.mode).toBe("measured");
    expect(ab(s)?.measuring).toBeNull();
  });

  it("Cancel keeps the level; a track change or a chain edit cancels with a reason", async () => {
    const { s, daemon } = await setup();
    await s.abMeasure("start");
    await s.abMeasure("cancel");
    expect(ab(s)?.measuring).toBeNull();
    expect(ab(s)?.measure_error).toBeNull();
    expect(s.getState().config?.ab?.level.mode).toBe("analytic");

    await s.abMeasure("start");
    daemon.simulateTrackChange();
    await until(() => ab(s)?.measure_error === "cancelled: the track changed");
    expect(ab(s)?.measuring).toBeNull();

    // A level change does not end it; a change to chain A does.
    await s.abMeasure("start");
    expect(ab(s)?.measure_error).toBeNull();
    await s.setLevelMatch({ mode: "off" });
    expect(ab(s)?.measuring).not.toBeNull();
    s.setStageEnabled("crossfeed", false);
    await s.whenIdle();
    await until(() => ab(s)?.measure_error === "cancelled: the compared chains changed");
  });

  it("ends with no match when side B stays silent", async () => {
    const { s, daemon } = await setup();
    daemon.measureSilentB = true;
    await s.abMeasure("start", 10);
    await until(() => !!ab(s)?.measure_error);
    expect(ab(s)?.measure_error).toBe("no match: B stayed silent over 10 s of A");
    expect(s.getState().config?.ab?.level.mode).toBe("analytic");
    expect(await s.abMeasure("start", 5)).toContain("max_seconds");
  });
});
