import { afterEach, describe, expect, it } from "vitest";
import type { Meters } from "../api/types";
import { meterStore } from "./meters";

function makeMeters(patch: Partial<Meters> = {}): Meters {
  return {
    peak_db: [-12, -14],
    rms_db: [-22, -24],
    clipped: 0,
    clipped_ch: [0, 0],
    input_peak_db: [-10, -12],
    chain_peak_db: [-9, -11],
    xruns: 0,
    dsp_load: 0.02,
    ...patch,
  };
}

afterEach(() => {
  meterStore.clear();
  meterStore.clipBaseline = null;
  meterStore.clipBaselines = [];
  meterStore.lastClipAt = -Infinity;
});

describe("meter clip latching and headroom", () => {

  it("lights the clip LED for a peak at or above 0 dBFS even without a counted clip", () => {
    meterStore.push(makeMeters({ clipped: 0, clipped_ch: [0, 0] }), 100);
    meterStore.push(makeMeters({ peak_db: [0, -3], clipped: 0, clipped_ch: [0, 0] }), 200);
    expect(meterStore.clipsPerChannel(2)).toEqual([1, 0]);
    expect(meterStore.lastClipAt).toBe(200);
    // A counted clip in the same frame is not double counted.
    meterStore.push(makeMeters({ peak_db: [-1, 1.5], clipped: 4, clipped_ch: [0, 4] }), 300);
    expect(meterStore.clipsPerChannel(2)).toEqual([1, 4]);
    meterStore.resetClip(0);
    expect(meterStore.clipsPerChannel(2)).toEqual([0, 4]);
  });

});

