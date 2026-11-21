import { afterEach, describe, expect, it } from "vitest";
import type { Meters } from "../api/types";
import { computeClipDeltas, meterChainGainDb, meterStore, metersSettled, quietMeters } from "./meters";

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
  it("computes per-channel deltas and handles daemon restarts", () => {
    const first = computeClipDeltas([5, 2], []);
    expect(first.nextBaselines).toEqual([5, 2]);
    expect(first.deltas).toEqual([0, 0]);

    const second = computeClipDeltas([7, 2], first.nextBaselines);
    expect(second.deltas).toEqual([2, 0]);
    expect(second.nextBaselines).toEqual([5, 2]);

    // Daemon restarted: counts dropped below baselines, so baseline resets to 0.
    const restarted = computeClipDeltas([1, 0], second.nextBaselines);
    expect(restarted.nextBaselines).toEqual([0, 0]);
    expect(restarted.deltas).toEqual([1, 0]);
  });

  it("latches and resets per-channel clip LEDs independently", () => {
    meterStore.push(makeMeters({ clipped: 10, clipped_ch: [10, 0] }), 100);
    expect(meterStore.clipsPerChannel(2)).toEqual([0, 0]);

    meterStore.push(makeMeters({ clipped: 13, clipped_ch: [12, 1] }), 200);
    expect(meterStore.clipsPerChannel(2)).toEqual([2, 1]);
    expect(meterStore.clipsSinceReset()).toBe(3);

    // Resetting channel 0 leaves channel 1 latched.
    meterStore.resetClip(0);
    expect(meterStore.clipsPerChannel(2)).toEqual([0, 1]);
    expect(meterStore.clipsSinceReset()).toBe(1);

    meterStore.resetClip(1);
    expect(meterStore.clipsPerChannel(2)).toEqual([0, 0]);
    expect(meterStore.clipsSinceReset()).toBe(0);
  });

  it("computes the chain gain from the pre-volume chain peak, not the output after the volume", () => {
    // Output after a -20 dB master volume: the chain still adds +2.1 dB.
    expect(meterChainGainDb(makeMeters({ peak_db: [-28, -34], chain_peak_db: [-8, -14], input_peak_db: [-10.1, -14] }))).toBeCloseTo(2.1, 6);
    expect(meterChainGainDb(makeMeters({ chain_peak_db: [-120, -120], input_peak_db: [-120, -120] }))).toBeNull();
    expect(meterChainGainDb(makeMeters({ input_peak_db: [] }))).toBeNull();
    // Chain not in the path (Direct / bypass): no reading.
    expect(meterChainGainDb(makeMeters({ chain_peak_db: [] }))).toBeNull();
  });

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

  it("shows fresh clips when daemon restarts with a count matching the old reset baseline", () => {
    // Session 1: starts at 5 (baseline 5 after click-reset), grows to 100.
    meterStore.push(makeMeters({ clipped: 5, clipped_ch: [5, 0] }), 100);
    meterStore.resetClip();
    expect(meterStore.clipBaseline).toBe(5);
    expect(meterStore.clipsSinceReset()).toBe(0);

    meterStore.push(makeMeters({ clipped: 100, clipped_ch: [60, 40] }), 200);
    expect(meterStore.clipsSinceReset()).toBe(95);
    expect(meterStore.clipsPerChannel(2)).toEqual([55, 40]);

    // Session 2: daemon restarts and first frame arrives with count 5 (equal to old baseline 5).
    meterStore.push(makeMeters({ clipped: 5, clipped_ch: [5, 0] }), 300);
    expect(meterStore.clipBaseline).toBe(0);
    expect(meterStore.clipsSinceReset()).toBe(5);
    expect(meterStore.clipsPerChannel(2)).toEqual([5, 0]);
    expect(meterStore.lastClipAt).toBe(300);
  });

  it("handles daemon restart when clipped_ch is absent", () => {
    meterStore.push(makeMeters({ clipped: 5, clipped_ch: [] }), 100);
    meterStore.resetClip();
    meterStore.push(makeMeters({ clipped: 100, clipped_ch: [] }), 200);
    expect(meterStore.clipsSinceReset()).toBe(95);

    meterStore.push(makeMeters({ clipped: 5, clipped_ch: [] }), 300);
    expect(meterStore.clipsSinceReset()).toBe(5);
    expect(meterStore.lastClipAt).toBe(300);
  });
});

describe("meters at rest", () => {
  const base = { peak_db: [-150, -150], rms_db: [-150, -150], clipped: 0, clipped_ch: [0, 0], input_peak_db: [], chain_peak_db: [], xruns: 0, dsp_load: 0 };

  it("a silent report is quiet; signal or limiter reduction is not", () => {
    expect(quietMeters(base, -60)).toBe(true);
    expect(quietMeters({ ...base, peak_db: [-150, -59] }, -60)).toBe(false);
    expect(quietMeters({ ...base, rms_db: [-30, -150] }, -60)).toBe(false);
    expect(quietMeters({ ...base, peak_db: [], rms_db: [] }, -60)).toBe(true);
    const lim = { active: true, gain_reduction_db: 0, headroom_db: 0, events: 0 };
    expect(quietMeters({ ...base, limiter: lim } as never, -60)).toBe(true);
    expect(quietMeters({ ...base, limiter: { ...lim, gain_reduction_db: 1.2 } } as never, -60)).toBe(false);
  });

  it("the bars are settled only when peak, hold and RMS rest at the floor", () => {
    const rest = { peak: -60, rms: -60, hold: -60 };
    expect(metersSettled([rest, rest], -60)).toBe(true);
    expect(metersSettled([rest, { ...rest, hold: -12 }], -60)).toBe(false);
    expect(metersSettled([{ ...rest, peak: -59.9 }, rest], -60)).toBe(false);
    expect(metersSettled([{ ...rest, rms: -59.99 }, rest], -60)).toBe(true);
    expect(metersSettled([{ ...rest, rms: -59.5 }, rest], -60)).toBe(false);
  });

  it("every report, clip reset and clear tells the painters", () => {
    meterStore.clear();
    let n = 0;
    const off = meterStore.onChange(() => n++);
    meterStore.push(base, 0);
    meterStore.resetClip();
    meterStore.resetClip(0);
    meterStore.clear();
    off();
    meterStore.push(base, 1);
    expect(n).toBe(4);
  });
});
