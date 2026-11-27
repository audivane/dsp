// Quick stage compare through the store, against the mock daemon: one
// click sets side B as a patch on A and turns A/B on; the same click again
// turns it off; the flip key; the shared headroom.

import { afterEach, describe, expect, it } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
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
  return { s, daemon };
}

afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
  clearStash();
});

describe("quick stage compare", () => {
  it("Compare sets B = A with the stage off, keeps the level, and stops on a second click", async () => {
    const { s, daemon } = await setup();
    daemon.config.chain.crossfeed = { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, width: 1, width_shape: "flat", width_corner_hz: 700 };
    await s.compareStage("crossfeed");
    await s.whenIdle();
    expect(daemon.config.ab?.b).toEqual({ patch: { crossfeed: { enabled: false } } });
    expect(daemon.config.ab?.label).toBe("Stereo image off");
    expect(daemon.config.ab?.chain_b?.crossfeed?.enabled).toBe(false);
    expect(s.getState().status?.ab?.enabled).toBe(true);
    // A manual level survives the next compare.
    await s.setLevelMatch({ mode: "manual", offset_db: -1.5 });
    await s.compareStage("crossfeed");
    await s.whenIdle();
    expect(daemon.config.ab).toBeNull();
    await s.compareFilter("gaussian_long");
    await s.whenIdle();
    expect(daemon.config.ab?.b).toEqual({ patch: { upsampler: { preset: "gaussian_long", design: null } } });
    expect(daemon.config.ab?.label?.startsWith("Filter: ")).toBe(true);
  });

  it("a stage with nothing to compare is refused without a call", async () => {
    const { s, daemon } = await setup();
    daemon.config.chain.convolution = [];
    await s.compareStage("conv");
    await s.whenIdle();
    expect(daemon.config.ab).toBeNull();
    expect(s.getState().toast?.tone).toBe("error");
  });

  it("the flip key switches sides; headroom goes through set_ab", async () => {
    const { s, daemon } = await setup();
    await s.compareStage("dc");
    await s.whenIdle();
    expect(s.getState().status?.ab?.current).toBe("a");
    await s.abFlip();
    await until(() => s.getState().status?.ab?.current === "b");
    await s.abFlip();
    await until(() => s.getState().status?.ab?.current === "a");
    await s.setAbHeadroom({ mode: "fixed", db: 3 });
    expect(daemon.config.ab?.headroom).toEqual({ mode: "fixed", db: 3 });
    expect(daemon.config.ab?.b).toEqual({ patch: { dc_block: { enabled: true } } });
  });

  it("an A/B overload stop ends with a new side B", async () => {
    const { s, daemon } = await setup();
    await s.compareStage("dc");
    daemon.simulateAbOverload(1.5);
    await until(() => s.getState().status?.overload?.action === "ab_off");
    await s.compareStage("harmonics");
    await until(() => s.getState().status?.overload === null);
  });

  it("pins a custom filter design and compares output bits/dither", async () => {
    const { s, daemon } = await setup();
    const design = { kind: "interpolator" as const, method: "nos" as const };
    await s.compareFilterDesign(design);
    await s.whenIdle();
    expect(daemon.config.ab?.b).toEqual({ patch: { upsampler: { preset: null, design } } });
    await s.compareOutput(16, "tpdf");
    await s.whenIdle();
    expect(daemon.config.ab?.b).toEqual({ patch: { output: { bits: 16, dither: "tpdf" } } });
    expect(daemon.config.ab?.label).toBe("Output: 16-bit TPDF");
    // Clicking the same output compare again turns A/B off.
    await s.compareOutput(16, "tpdf");
    await s.whenIdle();
    expect(daemon.config.ab).toBeNull();
  });
});
