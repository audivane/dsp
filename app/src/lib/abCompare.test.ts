import { describe, expect, it } from "vitest";
import { MockDaemon } from "../api/mock";
import type { AbConfig, AbStatus, ChainConfig, EqProfile, FilterDesign } from "../api/types";
import { abStoppedByOverload, comparedStage, comparing, describeB, filterDesignPatch, filterPatch, levelText, outputPatch, oversText, stagePatch, topBarLabel } from "./abCompare";

const base = (): ChainConfig => structuredClone(new MockDaemon().config.chain);
const ab = (b: AbConfig["b"], label?: string): AbConfig => ({ b, label, level: { mode: "analytic", weighting: "k" }, headroom: { mode: "auto" } });
const band = { enabled: true, kind: "peaking", freq_hz: 1000, gain_db: 3, q: 1 } as unknown as EqProfile["bands"][number];

describe("stage compare patches (the CLI's `ab stage`)", () => {
  it("switches off what A runs and on what A holds switched off", () => {
    const a = base();
    a.crossfeed = { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, width: 1, width_shape: "flat", width_corner_hz: 700 };
    expect(stagePatch(a, "crossfeed")).toEqual({ patch: { crossfeed: { enabled: false } }, label: "Stereo image off" });
    a.crossfeed.enabled = false;
    expect(stagePatch(a, "crossfeed")).toEqual({ patch: { crossfeed: { enabled: true } }, label: "Stereo image on" });
    // None at width 0 (k = 1) changes nothing on or off: no compare.
    a.crossfeed = { ...a.crossfeed, mode: "none", width: 1 };
    for (const enabled of [true, false]) {
      a.crossfeed.enabled = enabled;
      expect(stagePatch(a, "crossfeed")).toHaveProperty("error");
    }
    a.crossfeed.width = 1.25;
    expect(stagePatch(a, "crossfeed")).toEqual({ patch: { crossfeed: { enabled: true } }, label: "Stereo image on" });
    a.dc_block = null;
    expect(stagePatch(a, "dc")).toEqual({ patch: { dc_block: { enabled: true } }, label: "DC block on" });
    a.harmonics = { enabled: true, dc_block: true, layers: [{ kind: "chebyshev", enabled: true, preset: "triode", levels_db: [-40, -58, -72, null, null, null, null, null], signs: ["+", "-", "+", "+", "+", "+", "+", "+"], drive_db: 0, amount: 1 }] };
    expect(stagePatch(a, "harmonics")).toEqual({ patch: { harmonics: { enabled: false } }, label: "Harmonics off" });
  });

  it("pitch needs a shift to compare", () => {
    const a = base();
    a.pitch = { enabled: true, cents: 0, quality: "high" };
    expect(stagePatch(a, "pitch")).toHaveProperty("error");
    a.pitch = { enabled: false, cents: -32, quality: "high" };
    expect(stagePatch(a, "pitch")).toEqual({ patch: { pitch: { enabled: true } }, label: "Pitch on" });
    a.pitch.enabled = true;
    expect(stagePatch(a, "pitch")).toEqual({ patch: { pitch: { enabled: false } }, label: "Pitch off" });
  });

  it("tilt toggles inside the EQ and needs a slope", () => {
    const a = base();
    const tilt = { enabled: true, slope_db_per_oct: -0.5, total_db: null, start_hz: 20, end_hz: 20000, pivot: "center" as const };
    a.eq = { ...(a.eq ?? ({ name: "", version: 2, preamp_db: 0, bands: [], linked_enabled: true } as unknown as EqProfile)), tilt };
    expect(stagePatch(a, "tilt")).toEqual({ patch: { eq: { tilt: null, tilt_high: null } }, label: "Tilt off" });
    a.eq.tilt = { ...tilt, enabled: false };
    expect(stagePatch(a, "tilt")).toEqual({ patch: { eq: { tilt: { ...tilt, enabled: true } } }, label: "Tilt on" });
    a.eq.tilt = { ...tilt, slope_db_per_oct: 0 };
    expect(stagePatch(a, "tilt")).toHaveProperty("error");
  });

  it("tilt switches both sections of Both together", () => {
    const a = base();
    const low = { enabled: true, slope_db_per_oct: 1, total_db: null, start_hz: 20, end_hz: 200, pivot: "end" as const };
    const high = { ...low, start_hz: 2000, end_hz: 20000, pivot: "start" as const };
    a.eq = { ...(a.eq ?? ({ name: "", version: 2, preamp_db: 0, bands: [], linked_enabled: true } as unknown as EqProfile)), tilt: low, tilt_high: high };
    expect(stagePatch(a, "tilt")).toEqual({ patch: { eq: { tilt: null, tilt_high: null } }, label: "Tilt off" });
    a.eq.tilt = { ...low, enabled: false };
    a.eq.tilt_high = { ...high, enabled: false };
    expect(stagePatch(a, "tilt")).toEqual({ patch: { eq: { tilt: { ...low, enabled: true }, tilt_high: { ...high, enabled: true } } }, label: "Tilt on" });
  });

  it("EQ off drops bands and preamp; on comes from the remembered EQ", () => {
    const a = base();
    const eq = { name: "x", version: 2, preamp_db: -3, bands: [band], linked_enabled: true, left: null, right: null } as unknown as EqProfile;
    a.eq = eq;
    expect(stagePatch(a, "eq")).toEqual({ patch: { eq: { preamp_db: 0, bands: [], left: null, right: null, tone: null } }, label: "EQ off" });
    a.eq = null;
    expect(stagePatch(a, "eq")).toHaveProperty("error");
    const on = stagePatch(a, "eq", { eq });
    expect(on).toEqual({ patch: { eq }, label: "EQ on" });
  });

  it("convolution off empties the list, on enables the IRs A holds", () => {
    const a = base();
    a.convolution = [{ enabled: true, ir_path: "/x.wav", gain_db: 0 }];
    expect(stagePatch(a, "conv")).toEqual({ patch: { convolution: [] }, label: "Convolution off" });
    a.convolution = [{ enabled: false, ir_path: "/x.wav", gain_db: 0 }];
    expect(stagePatch(a, "conv")).toEqual({ patch: { convolution: [{ enabled: true, ir_path: "/x.wav", gain_db: 0 }] }, label: "Convolution on" });
    a.convolution = [];
    expect(stagePatch(a, "conv")).toHaveProperty("error");
  });
});

describe("filter compare (the CLI's `ab filter`)", () => {
  it("sets the preset and clears the design, with a friendly label", () => {
    const f = filterPatch("gaussian_long");
    expect(f.patch).toEqual({ upsampler: { preset: "gaussian_long", design: null } });
    expect(f.label.startsWith("Filter: ")).toBe(true);
    expect(f.label).not.toContain("gaussian_long");
    expect(filterPatch("recommended").label).toBe("Filter: Recommended");
  });

  it("pins a custom filter design as side B with preset: null", () => {
    const design = { kind: "interpolator", method: "nos", based_on: "nos" } as unknown as FilterDesign;
    const f = filterDesignPatch(design);
    expect(f.patch).toEqual({ upsampler: { preset: null, design } });
    expect(f.label).toContain("Custom");
    expect(describeB(ab({ patch: f.patch } as AbConfig["b"]))).toMatchObject({ kind: "filter", title: f.label });
  });
});

describe("output compare", () => {
  it("builds an output bits/dither patch and describes it", () => {
    const o = outputPatch(16, "tpdf");
    expect(o.patch).toEqual({ output: { bits: 16, dither: "tpdf" } });
    expect(o.label).toBe("Output: 16-bit TPDF");
    expect(describeB(ab({ patch: o.patch } as AbConfig["b"]))).toMatchObject({ kind: "output", title: "Output: 16-bit TPDF" });
  });
});

describe("side B in words", () => {
  it("tells a running compare apart and finds its stage", () => {
    const a = base();
    a.crossfeed = { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, width: 1, width_shape: "flat", width_corner_hz: 700 };
    const cur = ab({ patch: { crossfeed: { enabled: false } } }, "Stereo image off");
    expect(comparing(cur, { crossfeed: { enabled: false } })).toBe(true);
    expect(comparing(cur, { crossfeed: { enabled: true } })).toBe(false);
    expect(comparing(null, { crossfeed: { enabled: false } })).toBe(false);
    expect(comparedStage(cur, a)).toBe("crossfeed");
  });

  it("describes flat, preset, stage and filter sides", () => {
    expect(describeB(ab({ patch: { eq: null, convolution: [] } })).kind).toBe("flat");
    expect(describeB(ab({ chain: base() }, "Studio")).title).toBe("Studio");
    expect(describeB(ab({ chain: base() }, "Studio")).kind).toBe("preset");
    const st = describeB(ab({ patch: { crossfeed: { enabled: false } } }, "Stereo image off"));
    expect(st.kind).toBe("stage");
    expect(st.detail).toContain("stereo image switched off");
    const f = filterPatch("gaussian_long");
    expect(describeB(ab({ patch: f.patch } as AbConfig["b"], f.label))).toMatchObject({ kind: "filter", title: f.label });
    expect(topBarLabel(ab({ patch: { crossfeed: { enabled: false } } }, "Stereo image off"))).toBe("B: Stereo image off");
    expect(topBarLabel(null)).toBeNull();
  });

  it("says which side is cut, never boosted", () => {
    const s = (o: number, h = 0): AbStatus => ({ enabled: true, current: "a", blind: false, offset_db: o, gain_a_db: Math.min(0, -o), gain_b_db: Math.min(0, -o) + o, headroom_db: h, overs: { a: 0, b: 0 }, error: null, level: { mode: "analytic", weighting: "k" }, measuring: null, measure_error: null });
    expect(levelText(s(4.16))).toBe("B is 4.16 dB quieter, so A is cut 4.16 dB · headroom 0.00 dB");
    expect(levelText(s(-2))).toBe("B is 2.00 dB louder, so B is cut 2.00 dB · headroom 0.00 dB");
    expect(levelText(s(0, 1.2))).toBe("A and B play at the same level · headroom −1.20 dB");
  });

  it("counts overs and the overload stop", () => {
    expect(oversText({ a: 0, b: 0 })).toBeNull();
    expect(oversText({ a: 0, b: 3 })).toBe("B was limited 3×");
    expect(oversText({ a: 0.5, b: 12.4 })).toBe("A was limited 0.5×, B was limited 12×");
    const cur = ab({ patch: { eq: null, convolution: [] } });
    const report = (action: string) => ({ stage: "ab", output_rate: 0, load: 1.4, action, message: "", episode: 1, applied: true }) as never;
    expect(abStoppedByOverload(cur, report("ab_off"))).toBe(true);
    expect(abStoppedByOverload(cur, report("upsampler_off"))).toBe(true);
    expect(abStoppedByOverload(cur, report("varispeed_budget"))).toBe(false);
    expect(abStoppedByOverload(null, report("ab_off"))).toBe(false);
  });
});
