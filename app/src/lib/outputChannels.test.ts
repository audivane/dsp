import { describe, expect, it } from "vitest";
import { isDefaultPair, pairKey, pairLabel, pairOptions, parsePairKey, showPairSelector } from "./outputChannels";

describe("output channels", () => {
  it("is offered only on a device without a stereo mode, or with a pair set", () => {
    expect(showPairSelector(2, undefined)).toBe(false);
    expect(showPairSelector(0, null)).toBe(false);
    expect(showPairSelector(2, [0, 1])).toBe(false);
    expect(showPairSelector(8, undefined)).toBe(true);
    expect(showPairSelector(2, [2, 3])).toBe(true);
  });

  it("lists neighbouring pairs, and keeps a pair set elsewhere", () => {
    expect(pairOptions(8)).toEqual([
      [0, 1],
      [2, 3],
      [4, 5],
      [6, 7],
    ]);
    expect(pairOptions(3)).toEqual([[0, 1]]);
    expect(pairOptions(4, [3, 0])).toEqual([
      [0, 1],
      [2, 3],
      [3, 0],
    ]);
    expect(pairOptions(4, [2, 3])).toHaveLength(2);
  });

  it("round-trips the select value and labels from 1", () => {
    expect(parsePairKey(pairKey([6, 7]))).toEqual([6, 7]);
    expect(pairKey(undefined)).toBe("0,1");
    expect(pairLabel([2, 3])).toBe("Channels 3 + 4");
    expect(isDefaultPair(null)).toBe(true);
    expect(isDefaultPair([1, 0])).toBe(false);
  });
});
