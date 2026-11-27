import { describe, expect, it } from "vitest";
import { mergePatch } from "./abPatch";

describe("A/B patch", () => {
  it("merges like the daemon, leaving a stage A lacks absent on removal", () => {
    expect(mergePatch({ a: 1, b: { c: 2 } }, { b: { c: null, d: 3 } })).toEqual({ a: 1, b: { d: 3 } });
    expect(mergePatch({ eq: null }, { eq: { tilt: null } })).toEqual({ eq: null });
    expect(mergePatch({ eq: null }, { eq: { tilt: { enabled: false } } })).toEqual({ eq: { tilt: { enabled: false } } });
    expect(mergePatch({ convolution: [1, 2] }, { convolution: [] })).toEqual({ convolution: [] });
  });

});
