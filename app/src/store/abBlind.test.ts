// Blind tests through the store, against the mock daemon: the mask, the
// real side behind each button, the result and its export, and the ways a
// run is abandoned.

import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { verifyExport } from "../lib/abBlind";
import { AppStore, clearStash } from "./store";

const until = async (pred: () => boolean, ms = 3000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};

let stores: AppStore[] = [];
async function setup() {
  const daemon = new MockDaemon();
  daemon.config.ab = null;
  const s = new AppStore(new MockTransport(daemon, { request: 1, reply: 1, event: 1 }));
  stores.push(s);
  s.connect();
  await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
  await s.compareStage("dc");
  await s.whenIdle();
  return { s, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  clearStash();
});

describe("blind test", () => {
  it("runs an ABX test to the end: hidden side, real switches, scored result", async () => {
    const { s, daemon } = await setup();
    expect(await s.blindStart({ mode: "abx", trials: 3, feedback: "end" }, "A", "B")).toBeNull();
    await until(() => !!s.getState().status?.ab?.blind);
    expect(s.getState().status?.ab?.current).toBeNull();
    const run = s.getState().blindRun!;
    expect(run.commitment).toMatch(/^[0-9a-f]{64}$/);
    for (let i = 0; i < 3; i++) {
      // No answer before listening.
      s.blindAnswer("A");
      expect(s.getState().blindRun!.records.length).toBe(i);
      await s.blindPlay("x");
      await s.whenIdle();
      expect(daemon.abSide).toBe(run.truths[i] === "A" ? "a" : "b");
      await s.blindPlay("b");
      await s.whenIdle();
      expect(daemon.abSide).toBe("b");
      s.blindAnswer(i === 0 ? "B" : run.truths[i]);
    }
    const done = s.getState().blindRun!;
    expect(done.phase).toBe("done");
    expect(done.records.map((r) => r.switches)).toEqual([2, 2, 2]);
    await until(() => s.getState().status?.ab?.blind === false);
    const e = s.blindExport()!;
    expect([e.outcome, e.feedback, e.result?.n]).toEqual(["completed", "end", 3]);
    expect(e.result?.k).toBe(run.truths[0] === "B" ? 3 : 2);
    expect(await verifyExport(e)).toBe(true);
    s.blindClose();
    expect(s.getState().blindRun).toBeNull();
  });

  it("records practice feedback; blind A/B plays shuffled sides", async () => {
    const { s, daemon } = await setup();
    await s.blindStart({ mode: "identify", trials: 2, feedback: "each" }, "A", "B");
    const run = s.getState().blindRun!;
    await s.blindPlay("1");
    await s.whenIdle();
    expect(daemon.abSide).toBe(run.truths[0] === "A" ? "a" : "b");
    await s.blindPlay("x"); // not a button in blind A/B
    expect(s.getState().blindRun!.playing).toBe("1");
    s.blindAnswer("1");
    expect(s.getState().blindRun!.records[0].correct).toBe(run.truths[0] === "B");
    await s.blindPlay("2");
    s.blindAnswer("2");
    await s.whenIdle();
    expect(s.blindExport()?.feedback).toBe("each");
  });

  it("a run is abandoned, never scored, when the view, the sides or A/B change", async () => {
    const { s, daemon } = await setup();
    await s.blindStart({ mode: "abx", trials: 5, feedback: "each" }, "A", "B");
    await s.blindPlay("a");
    s.blindAnswer("A");
    s.setView("eq");
    expect(s.getState().blindRun?.phase).toBe("abandoned");
    expect(s.getState().blindRun?.reason).toMatch(/left/);
    await until(() => s.getState().status?.ab?.blind === false);
    const e = s.blindExport()!;
    expect([e.outcome, e.result, e.trials.length]).toEqual(["abandoned", null, 1]);
    s.blindClose();

    s.setView("ab");
    await s.blindStart({ mode: "abx", trials: 5, feedback: "end" }, "A", "B");
    await until(() => !!s.getState().status?.ab?.blind);
    await s.setAbHeadroom({ mode: "fixed", db: 2 });
    await until(() => s.getState().blindRun?.phase === "abandoned");
    expect(daemon.abBlind).toBe(false);
    s.blindClose();

    await s.blindStart({ mode: "abx", trials: 5, feedback: "end" }, "A", "B");
    await until(() => !!s.getState().status?.ab?.blind);
    await s.setAbSource("off");
    await until(() => s.getState().blindRun?.phase === "abandoned");
    expect(s.getState().blindRun?.reason).toMatch(/A\/B was turned off/);
  });

  it("the level measurement is refused while the mask is set", async () => {
    const { s } = await setup();
    await s.blindStart({ mode: "abx", trials: 5, feedback: "end" }, "A", "B");
    expect(await s.abMeasure("start")).toMatch(/blind test/);
  });
});
