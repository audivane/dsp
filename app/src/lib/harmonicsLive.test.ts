import { describe, expect, it } from "vitest";
import type { HarmonicsLive } from "../api/types";
import { HISTORY_CAP, HISTORY_MS, LiveHistory, bucketHistory, gmAt, holdSpectrum, musicWeights, partHideKey, partLabel, pointOf } from "./harmonicsLive";

const frame = (over: Partial<HarmonicsLive> = {}): HarmonicsLive => ({
  state: "playing",
  seq: 1,
  rate: 96000,
  window_ms: 85,
  interval_ms: 200,
  f_min_hz: 20,
  bins_per_octave: 24,
  music: [],
  added: [],
  added_db: -40,
  added_pct: 1,
  added_peak_db: -35,
  level_change_db: -0.2,
  linear_gain_db: -1,
  music_rms_dbfs: -20,
  music_level_dbfs: -17,
  music_peak_dbfs: -6,
  centroid_hz: 1200,
  layers: [{ layer: 0, sag_db: 0.6, bias: 0 }],
  split: "orders",
  parts: [{ key: "h2", order: 2, level_db: -42, peak_db: -38 }],
  analysis_ms: 1,
  ...over,
});

describe("live history", () => {
  it("keeps one point per new frame and one gap a second", () => {
    const h = new LiveHistory();
    expect(h.push(frame({ seq: 1 }), 0)).toBe(true);
    expect(h.push(frame({ seq: 1 }), 100)).toBe(false);
    expect(h.push(frame({ seq: 2 }), 200)).toBe(true);
    expect(h.push(frame({ state: "silent" }), 300)).toBe(true);
    expect(h.push(frame({ state: "silent" }), 800)).toBe(false);
    expect(h.points.map((p) => p.added)).toEqual([-40, -40, null]);
    expect(h.points[0].sag).toBe(0.6);
    expect(h.points[0].parts.h2).toBe(-42);
  });

  it("is bounded by time and count", () => {
    const h = new LiveHistory();
    for (let i = 0; i < HISTORY_CAP + 500; i++) h.push(frame({ seq: i }), i * 50);
    expect(h.points.length).toBeLessThanOrEqual(HISTORY_CAP);
    const last = h.points[h.points.length - 1].t;
    expect(last - h.points[0].t).toBeLessThanOrEqual(HISTORY_MS);
  });

  it("buckets by mean power and maximum peak, gaps as null", () => {
    const pts = [pointOf(frame({ added_db: -40, added_peak_db: -30 }), 100), pointOf(frame({ added_db: -30, added_peak_db: -25 }), 200), pointOf(frame({ state: "idle" }), 600)];
    const b = bucketHistory(pts, 1000, 1000, 2);
    expect(b[0].n).toBe(2);
    // Mean power of −40 and −30 dB.
    expect(b[0].added).toBeCloseTo(10 * Math.log10((1e-4 + 1e-3) / 2), 6);
    expect(b[0].peak).toBe(-25);
    expect(b[1].added).toBeNull();
  });
});

describe("spectrum helpers", () => {
  it("holds peaks and restarts on a new grid", () => {
    expect(holdSpectrum([1, 5], [3, 2])).toEqual([3, 5]);
    expect(holdSpectrum([1], [3, 2])).toEqual([3, 2]);
    expect(holdSpectrum(null, [3])).toEqual([3]);
  });

  it("weights the music's energy within the audio band", () => {
    const w = musicWeights([-10, -25, -60, 0], [100, 1000, 5000, 30000]);
    expect(w[0]).toBe(1);
    expect(w[1]).toBeCloseTo(0.5, 6);
    expect(w[2]).toBe(0);
    expect(w[3]).toBe(0);
  });

  it("names parts", () => {
    expect(partLabel("h3")).toBe("H3");
    expect(partLabel("even")).toBe("Even orders");
    expect(partHideKey("h5")).toBe(5);
    expect(partHideKey("odd")).toBe(101);
  });
});

describe("gmAt", () => {
  const pts = [
    { input_dbfs: -40, gedlee_gm: 0.001 },
    { input_dbfs: -20, gedlee_gm: 0.01 },
    { input_dbfs: 0, gedlee_gm: 0.13 },
  ];
  it("interpolates between sweep levels and clamps outside them", () => {
    expect(gmAt(pts, -10)).toBeCloseTo(0.07, 6);
    expect(gmAt(pts, -60)).toBe(0.001);
    expect(gmAt(pts, 3)).toBe(0.13);
    expect(gmAt([], -10)).toBeNull();
  });
});
