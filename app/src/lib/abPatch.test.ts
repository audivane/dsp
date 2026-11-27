import { describe, expect, it } from "vitest";
import { abLabel, mergePatch, patchError } from "./abPatch";

describe("A/B patch", () => {
  it("merges like the daemon, leaving a stage A lacks absent on removal", () => {
    expect(mergePatch({ a: 1, b: { c: 2 } }, { b: { c: null, d: 3 } })).toEqual({ a: 1, b: { d: 3 } });
    expect(mergePatch({ eq: null }, { eq: { tilt: null } })).toEqual({ eq: null });
    expect(mergePatch({ eq: null }, { eq: { tilt: { enabled: false } } })).toEqual({ eq: { tilt: { enabled: false } } });
    expect(mergePatch({ convolution: [1, 2] }, { convolution: [] })).toEqual({ convolution: [] });
  });

  it("refuses the fields both sides share", () => {
    expect(patchError({ crossfeed: { enabled: false } })).toBeNull();
    expect(patchError({ upsampler: { preset: "x" } })).toBeNull();
    expect(patchError({ upsampler: { target: "off" } })).toContain("ab.b.patch.upsampler.target");
    expect(patchError({ limiter: {} })).toContain("ab.b.patch.limiter");
    expect(patchError({ output: { bits: 16, dither: "tpdf" } })).toBeNull();
    expect(patchError({ output: { headroom_db: 1 } })).toContain("ab.b.patch.output.headroom_db");
  });

  it("names side B", () => {
    const base = { level: { mode: "off" as const }, headroom: { mode: "auto" as const } };
    expect(abLabel({ ...base, b: { patch: { eq: null, convolution: [] } } })).toBe("Flat");
    expect(abLabel({ ...base, b: { patch: {} }, label: "Crossfeed off" })).toBe("Crossfeed off");
  });
});
