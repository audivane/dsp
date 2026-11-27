import { afterEach, describe, expect, it, vi } from "vitest";
import { MockDaemon, MockTransport } from "../api/mock";
import { MOCK_HARMONICS_SYSTEMS } from "../api/mockHarmonics";
import type { AbConfig, ChainConfig } from "../api/types";
import { AppStore } from "../store/store";
import { describeB, stagePatch } from "./abCompare";
import { buildCustom, combinePatches, everythingOffSel, loadSaved, SAVED_KEY, selFromChain, storeSaved, withSaved, type CustomSel } from "./abCustom";
import { FLAT_PATCH, materializeB } from "./abPatch";

const base = (): ChainConfig => {
  const a = structuredClone(new MockDaemon().config.chain);
  a.harmonics = { enabled: true, dc_block: true, layers: [] };
  a.crossfeed = { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, width: 1, width_shape: "flat", width_corner_hz: 700 };
  return a;
};

describe("custom B patches", () => {
  it("combines several changes into one patch and names them", () => {
    const b = buildCustom(base(), { stages: { harmonics: "off", crossfeed: "off" }, filter: { preset: "nos" } });
    expect(b.patch).toEqual({ harmonics: { enabled: false }, crossfeed: { enabled: false }, upsampler: { preset: "nos", design: null } });
    expect(b.changes).toEqual(["harmonics off", "stereo image off", "upsampling NOS"]);
    expect(b.label).toBe("Harmonics off, stereo image off, upsampling NOS");
    expect(b.errors).toEqual([]);
  });

  it("notes choices A already has and keeps the user's label", () => {
    const a = base();
    a.crossfeed!.enabled = false;
    const b = buildCustom(a, { stages: { crossfeed: "off" }, output: { bits: a.output.bits, dither: a.output.dither }, label: "Mine" });
    expect(b.patch).toBeNull();
    expect(b.notes).toEqual(["Stereo image is already off in A.", "Output bits and dither are the same as A's."]);
    expect(b.label).toBe("Mine");
  });

  it("merges two changes to the same stage", () => {
    expect(combinePatches({ eq: { preamp_db: 0, bands: [] } }, { eq: { tilt: null } })).toEqual({ eq: { preamp_db: 0, bands: [], tilt: null } });
  });

  it("loads a harmonics system whole, replacing A's routing", () => {
    const sys = MOCK_HARMONICS_SYSTEMS.find((s) => !s.routing || s.routing === "parallel")!;
    const a = base();
    a.harmonics!.routing = "series";
    const b = buildCustom(a, { stages: {}, harmonicsSystem: sys.id }, MOCK_HARMONICS_SYSTEMS);
    const h = (b.patch as { harmonics: Record<string, unknown> }).harmonics;
    expect(h).toMatchObject({ enabled: true, system: sys.id, routing: null });
    expect((h.layers as unknown[]).length).toBe(sys.layers.length);
    expect(b.changes).toEqual([`harmonics ${sys.name}`]);
  });

  it("everything off turns off what A runs and leaves output alone", () => {
    const b = buildCustom(base(), everythingOffSel());
    expect(b.patch).toHaveProperty("harmonics", { enabled: false });
    expect(b.patch).toHaveProperty("crossfeed", { enabled: false });
    expect(b.patch).toHaveProperty("convolution", []);
    expect(b.patch).toHaveProperty("upsampler", { preset: "nos", design: null });
    expect(b.patch).not.toHaveProperty("output");
    expect(b.errors).toEqual([]);
  });

  it("a stage with nothing to turn on is an error", () => {
    const a = base();
    a.convolution = [];
    expect(buildCustom(a, { stages: { conv: "on" } }).errors).toHaveLength(1);
  });

  it("is described as a custom B, not a stage compare", () => {
    const b = buildCustom(base(), { stages: { harmonics: "off", crossfeed: "off" } });
    const d = describeB({ b: { patch: b.patch! } as AbConfig["b"], label: b.label, level: { mode: "off" }, headroom: { mode: "auto" } });
    expect(d.kind).toBe("custom");
    expect(d.title).toBe("Harmonics off, stereo image off");
  });

  it("reads back what side B is, whatever made it", () => {
    const a = base();
    const sys = MOCK_HARMONICS_SYSTEMS[0];
    const chainOf = (patch: Record<string, unknown>) => materializeB(a, { patch } as AbConfig["b"]);
    // A stage compare: one row differs.
    const flip = stagePatch(a, "harmonics");
    expect(selFromChain(a, chainOf("patch" in flip ? flip.patch : {}))).toEqual({ stages: { harmonics: "off" } });
    // Flat: EQ and convolution off.
    expect(selFromChain(a, chainOf(FLAT_PATCH)).stages).toMatchObject({ conv: "off" });
    // A custom B round-trips to the same patch.
    const sel: CustomSel = { stages: { crossfeed: "off", dc: "on" }, harmonicsSystem: sys.id, filter: { preset: "nos" }, output: { bits: 16, dither: "tpdf" } };
    const built = buildCustom(a, sel, MOCK_HARMONICS_SYSTEMS);
    const back = selFromChain(a, chainOf(built.patch!), MOCK_HARMONICS_SYSTEMS);
    expect(back).toEqual(sel);
    expect(buildCustom(a, back, MOCK_HARMONICS_SYSTEMS).patch).toEqual(built.patch);
    // B = A: nothing differs.
    expect(selFromChain(a, a)).toEqual({ stages: {} });
  });

  it("saves named selections, newest first, replacing by name", () => {
    const list = withSaved(withSaved([], { name: "x", sel: { stages: { eq: "off" } } }), { name: "y", sel: { stages: {} } });
    expect(withSaved(list, { name: "x", sel: { stages: { dc: "on" } } }).map((s) => s.name)).toEqual(["x", "y"]);
    const mem = new Map<string, string>();
    vi.stubGlobal("localStorage", { getItem: (k: string) => mem.get(k) ?? null, setItem: (k: string, v: string) => void mem.set(k, v) });
    storeSaved(list);
    expect(mem.has(SAVED_KEY)).toBe(true);
    expect(loadSaved().map((s) => s.name)).toEqual(["y", "x"]);
    vi.unstubAllGlobals();
    // Storage unavailable: nothing saved, nothing thrown.
    vi.stubGlobal("localStorage", undefined);
    expect(() => storeSaved(list)).not.toThrow();
    expect(loadSaved()).toEqual([]);
    vi.unstubAllGlobals();
  });
});

const until = async (pred: () => boolean, ms = 3000) => {
  const t = Date.now();
  while (!pred()) {
    if (Date.now() - t > ms) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 5));
  }
};

let stores: AppStore[] = [];
afterEach(() => {
  for (const s of stores) s.disconnect();
  stores = [];
});

describe("custom B through the mock daemon", () => {
  it("sets a multi-field patch as side B and reports refusals inline", async () => {
    const daemon = new MockDaemon();
    daemon.config.ab = null;
    const s = new AppStore(new MockTransport(daemon, { request: 1, reply: 1, event: 1 }));
    stores.push(s);
    s.connect();
    await until(() => s.getState().conn === "up" && !!s.getState().config && !!s.getState().status);
    const a = s.getState().config!.chain;
    const built = buildCustom(a, { stages: { conv: "off" }, filter: { preset: "nos" }, output: { bits: 16, dither: "tpdf" } });
    expect(await s.setAbCustom(built.patch!, built.label)).toBeNull();
    const ab = s.getState().config!.ab!;
    expect(ab.b).toEqual({ patch: built.patch });
    expect(ab.label).toBe(built.label);
    expect(ab.chain_b?.upsampler.preset).toBe("nos");
    expect(ab.chain_b?.output.bits).toBe(16);
    expect(ab.chain_b?.convolution).toEqual([]);
    expect(s.getState().status?.ab?.enabled).toBe(true);
    const refused = await s.setAbCustom({ crossfeed: { enabled: false }, limiter: { ceiling_dbfs: -1 } }, "Bad");
    expect(refused).toMatch(/limiter may not differ/);
  });
});
