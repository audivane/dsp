import { describe, expect, it } from "vitest";
import {
  DEFAULT_SPECTRUM_PREFS,
  SPEC_DB_MIN,
  SPEC_STALE_FADE_MS,
  SPEC_STALE_MS,
  SpectrumAnimator,
  decaySpectrumBins,
  monotonePathD,
  spectrumCurvePoints,
  loadSpectrumPrefs,
  monotoneAt,
  monotoneTangents,
  smoothFractionalOctave,
  saveSpectrumPrefs,
  smoothSpectrumBins,
  spectrumBinFreq,
  spectrumDbToY,
  stepSpectrumAnimation,
  updatePeakHoldBins,
} from "./spectrum";

describe("spectrum helpers", () => {
  it("maps bin indices to exponential frequencies f_min * 2^(i / bins_per_octave)", () => {
    expect(spectrumBinFreq(0, 20, 24)).toBeCloseTo(20, 8);
    expect(spectrumBinFreq(24, 20, 24)).toBeCloseTo(40, 8);
    expect(spectrumBinFreq(48, 20, 24)).toBeCloseTo(80, 8);
    expect(spectrumBinFreq(240, 20, 24)).toBeCloseTo(20480, 5);
  });

  it("maps dBFS onto plot height and clamps out-of-range or non-finite values", () => {
    expect(spectrumDbToY(0, 10, 180)).toBe(10);
    expect(spectrumDbToY(-90, 10, 180)).toBe(190);
    expect(spectrumDbToY(-45, 10, 180)).toBe(100);
    expect(spectrumDbToY(6, 10, 180)).toBe(10);
    expect(spectrumDbToY(-120, 10, 180)).toBe(190);
    expect(spectrumDbToY(Number.NaN, 10, 180)).toBe(190);
  });

  it("applies fast attack and ~300 ms exponential release smoothing", () => {
    const cur = new Float64Array([-90, -20]);
    const target = [-10, -90];
    // Over 25 ms, attack bin jumps ~63% of [-90 -> -10], release bin drops ~8% of [-20 -> -90].
    const active = smoothSpectrumBins(cur, target, 0.025);
    expect(active).toBe(true);
    expect(cur[0]).toBeGreaterThan(-42);
    expect(cur[1]).toBeGreaterThan(-27);
    expect(cur[1]).toBeLessThan(-24);

    // Over 300 ms release with constant floor target, remaining gap shrinks by 1/e (~36.8%).
    const rel = new Float64Array([-10]);
    smoothSpectrumBins(rel, [-90], 0.3);
    expect(rel[0]).toBeCloseTo(-90 + 80 * Math.exp(-1), 2);

    // Settled at floor stops reporting active animation.
    const settled = new Float64Array([SPEC_DB_MIN]);
    expect(smoothSpectrumBins(settled, [SPEC_DB_MIN], 0.016)).toBe(false);
  });

  it("holds bin peaks for holdMs and releases linearly afterward", () => {
    const peaks = new Float64Array([-90, -90]);
    const until = new Float64Array([0, 0]);
    // New high latches immediately and schedules hold window.
    expect(updatePeakHoldBins(peaks, until, [-12, -30], 1000, 0.016, SPEC_DB_MIN, 2000, 10)).toBe(true);
    expect(peaks[0]).toBe(-12);
    expect(until[0]).toBe(3000);

    // During hold window, lower targets do not reduce peak.
    updatePeakHoldBins(peaks, until, [-60, -60], 2500, 0.5, SPEC_DB_MIN, 2000, 10);
    expect(peaks[0]).toBe(-12);

    // After hold expires, peak falls at 10 dB/s -> 5 dB over 0.5s.
    updatePeakHoldBins(peaks, until, [-60, -60], 3500, 0.5, SPEC_DB_MIN, 2000, 10);
    expect(peaks[0]).toBe(-17);
  });

  it("decays stale frames smoothly to floor, fades peak hold, and clears once settled", () => {
    const pre = new Float64Array([-20, -25]);
    const post = new Float64Array([-12, -18]);
    const peakVals = new Float64Array([-12, -18]);
    const peakUntil = new Float64Array([5000, 5000]);

    // Within stale threshold, keeps animating without stale fade.
    const fresh = stepSpectrumAnimation({
      frameAgeMs: SPEC_STALE_MS - 50,
      newFrame: false,
      tap: "both",
      peakHold: true,
      preTarget: [-20, -25],
      postTarget: [-12, -18],
      smoothPre: pre,
      smoothPost: post,
      peakVals,
      peakUntil,
      nowMs: 1000,
      dtSec: 0.016,
    });
    expect(fresh.stale).toBe(false);
    expect(fresh.animating).toBe(true);
    expect(fresh.peakAlpha).toBe(1);
    expect(fresh.clearFrame).toBe(false);

    // Past stale threshold + half fade window: smooth values fall toward floor and peak fades.
    const staleMid = stepSpectrumAnimation({
      frameAgeMs: SPEC_STALE_MS + SPEC_STALE_FADE_MS / 2,
      newFrame: false,
      tap: "both",
      peakHold: true,
      preTarget: [-20, -25],
      postTarget: [-12, -18],
      smoothPre: pre,
      smoothPost: post,
      peakVals,
      peakUntil,
      nowMs: 1650,
      dtSec: 0.3,
    });
    expect(staleMid.stale).toBe(true);
    expect(staleMid.animating).toBe(true);
    expect(staleMid.clearFrame).toBe(false);
    expect(staleMid.peakAlpha).toBeCloseTo(0.5, 4);
    expect(post[0]).toBeLessThan(-12);
    expect(pre[0]).toBeLessThan(-20);

    // Stepping forward until fully decayed reaches floor, peakAlpha = 0, and signals clearFrame.
    let cleared = false;
    for (let i = 0; i < 200; i++) {
      const s = stepSpectrumAnimation({
        frameAgeMs: SPEC_STALE_MS + SPEC_STALE_FADE_MS + i * 50,
        newFrame: false,
        tap: "both",
        peakHold: true,
        preTarget: [-20, -25],
        postTarget: [-12, -18],
        smoothPre: pre,
        smoothPost: post,
        peakVals,
        peakUntil,
        nowMs: 2000 + i * 50,
        dtSec: 0.05,
      });
      if (s.clearFrame) {
        cleared = true;
        expect(s.animating).toBe(false);
        expect(s.peakAlpha).toBe(0);
        break;
      }
    }
    expect(cleared).toBe(true);
    expect(post[0]).toBe(SPEC_DB_MIN);
    expect(pre[0]).toBe(SPEC_DB_MIN);
    expect(decaySpectrumBins(post, 0.05)).toBe(false);
  });

  it("persists spectrum prefs in localStorage safely", () => {
    const store = new Map<string, string>();
    const prev = globalThis.localStorage;
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => store.set(k, v),
      },
    });
    try {
      expect(loadSpectrumPrefs()).toEqual(DEFAULT_SPECTRUM_PREFS);
      saveSpectrumPrefs({ enabled: false, tap: "both", peakHold: true, smoothing: 3 });
      expect(loadSpectrumPrefs()).toEqual({ enabled: false, tap: "both", peakHold: true, smoothing: 3 });
      store.set("avdsp-spectrum", JSON.stringify({ enabled: true, smoothing: 5 }));
      expect(loadSpectrumPrefs().smoothing).toBe(12);
      store.set("avdsp-spectrum", "{broken json");
      expect(loadSpectrumPrefs()).toEqual(DEFAULT_SPECTRUM_PREFS);
    } finally {
      Object.defineProperty(globalThis, "localStorage", { configurable: true, value: prev });
    }
  });

  it("smooths in power over a fractional-octave window without raising peaks", () => {
    const flat = Array(48).fill(-40);
    expect(smoothFractionalOctave(flat, 24, 3).every((v) => Math.abs(v + 40) < 1e-9)).toBe(true);
    // 1/24 at 24 bins/octave is a no-op.
    const peaky = Array(48).fill(-80);
    peaky[20] = 0;
    expect(smoothFractionalOctave(peaky, 24, 24)).toEqual(peaky);
    // 1/12: the bin plus half of each neighbour (mean power over 2 bins).
    const s12 = smoothFractionalOctave(peaky, 24, 12);
    expect(s12[20]).toBeCloseTo(10 * Math.log10((1 + 2 * 0.5 * 1e-8) / 2), 6);
    expect(s12[21]).toBeCloseTo(10 * Math.log10((0.5 + 1e-8 + 0.5e-8) / 2), 6);
    // A narrow peak only ever comes down.
    for (const n of [3, 6, 12]) {
      const sm = smoothFractionalOctave(peaky, 24, n);
      expect(Math.max(...sm)).toBeLessThanOrEqual(0);
      expect(sm.every((v, i) => v >= peaky[i] - 1e-9 || i === 20)).toBe(true);
    }
  });

  it("draws a monotone cubic that never overshoots the bin values", () => {
    const xs = [0, 1, 2, 3, 4, 5];
    const ys = [10, 10, 30, 30, 0, 5];
    const m = monotoneTangents(xs, ys);
    for (let i = 0; i < xs.length - 1; i++) {
      const lo = Math.min(ys[i], ys[i + 1]);
      const hi = Math.max(ys[i], ys[i + 1]);
      for (let t = 0; t <= 1; t += 0.05) {
        const v = monotoneAt(xs, ys, m, i, t);
        expect(v).toBeGreaterThanOrEqual(lo - 1e-9);
        expect(v).toBeLessThanOrEqual(hi + 1e-9);
      }
    }
    expect(monotoneAt(xs, ys, m, 1, 0)).toBe(10);
    expect(monotoneAt(xs, ys, m, 1, 1)).toBe(30);
  });
});

describe("a report without a post spectrum", () => {
  // The engine sends `post: []` when the post tap cannot complete an
  // analysis (its periods are cut by gaps): the pre side keeps drawing.
  it("smooths and animates the pre side only", () => {
    const pre = Array.from({ length: 64 }, (_, i) => -40 - i / 4);
    expect(smoothFractionalOctave([], 24, 6)).toEqual([]);
    const smoothPre = new Float64Array(64).fill(SPEC_DB_MIN);
    const smoothPost = new Float64Array(0);
    for (const tap of ["pre", "post", "both"] as const) {
      const step = stepSpectrumAnimation({
        frameAgeMs: 0,
        newFrame: true,
        tap,
        peakHold: true,
        preTarget: smoothFractionalOctave(pre, 24, 6),
        postTarget: [],
        smoothPre,
        smoothPost,
        peakVals: new Float64Array(tap === "pre" ? 64 : 0),
        peakUntil: new Float64Array(tap === "pre" ? 64 : 0),
        nowMs: 1000,
        dtSec: 1 / 60,
      });
      expect(step.clearFrame).toBe(false);
    }
    expect(smoothPre[10]).toBeGreaterThan(SPEC_DB_MIN);
  });
});

describe("shared analyser drawing", () => {
  it("animates toward targets below the PEQ floor, then falls away when stale", () => {
    const a = new SpectrumAnimator();
    const target = [-150, -120, -40];
    let step = a.step({ frameKey: 1, frameAtMs: 0, nowMs: 0, tap: "both", peakHold: false, preTarget: target, postTarget: target, floorDb: -250, ceilDb: 60, staleMs: 1500 });
    for (let t = 16; t <= 1000; t += 16) step = a.step({ frameKey: 1, frameAtMs: 0, nowMs: t, tap: "both", peakHold: false, preTarget: target, postTarget: target, floorDb: -250, ceilDb: 60, staleMs: 1500 });
    expect(step.stale).toBe(false);
    expect(a.post[0]).toBeCloseTo(-150, 0);
    expect(a.pre[2]).toBeCloseTo(-40, 0);
    let t = 1000;
    while (!step.clearFrame && t < 20000) {
      t += 16;
      step = a.step({ frameKey: 1, frameAtMs: 0, nowMs: t, tap: "both", peakHold: false, preTarget: target, postTarget: target, floorDb: -250, ceilDb: 60, staleMs: 1500 });
    }
    expect(step.clearFrame).toBe(true);
    expect(t).toBeGreaterThan(1500);
  });

  it("places bins at their centres and carries the last on to Nyquist", () => {
    const bins = [0, -10, -20, -30];
    const { xs, ys } = spectrumCurvePoints(bins, 10, 1, (f) => f, (db) => -db, 1e9, 85);
    expect(xs).toEqual([10, 20, 40, 80, 85]);
    expect(ys).toEqual([-0, 10, 20, 30, 30]);
    // Far from Nyquist: no carry; past the right edge: one bin over.
    expect(spectrumCurvePoints(bins, 10, 1, (f) => f, (db) => db, 1e9, 400).xs).toHaveLength(4);
    expect(spectrumCurvePoints(bins, 10, 1, (f) => f, (db) => db, 25).xs).toEqual([10, 20, 40]);
  });

  it("writes the monotone curve as SVG commands", () => {
    expect(monotonePathD([0, 10, 20], [5, 5, 0])).toBe("M0.0,5.0C3.3,5.0 6.7,5.0 10.0,5.0C13.3,5.0 16.7,1.7 20.0,0.0");
    expect(monotonePathD([1], [2], true)).toBe("L1.0,2.0");
    expect(monotonePathD([], [])).toBe("");
  });
});
