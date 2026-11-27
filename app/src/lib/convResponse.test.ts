import { describe, expect, it } from "vitest";
import type { ConvResponse } from "../api/types";
import { DRAW_FREQS } from "./overlays";
import { convSeries, fitRange, routeName, seriesShown, smoothDb, smoothGd } from "./convResponse";

const flat = (db: number) => DRAW_FREQS.map(() => db);

const reply: ConvResponse = {
  rate_hz: 48000,
  side: "a",
  freqs_hz: [...DRAW_FREQS],
  combined_taps: 100,
  items: [
    { index: 0, label: "room.wav", taps: 50, curves: [{ in: 0, out: 0, mag_db: flat(0), group_delay_ms: flat(1) }, { in: 1, out: 1, mag_db: flat(0), group_delay_ms: flat(1) }] },
    { index: 2, label: "tilt.wav", taps: 10, curves: [{ mag_db: flat(-1), group_delay_ms: flat(0) }] },
    { index: 3, label: "Sub", taps: 40, curves: [{ in: 1, out: 0, mag_db: flat(-6), group_delay_ms: flat(0) }] },
  ],
  outputs: [
    { out: 0, mag_db: flat(-1), group_delay_ms: flat(1) },
    { out: 1, mag_db: flat(-1), group_delay_ms: flat(1) },
  ],
};

describe("conv response series", () => {
  it("names each curve and keeps keys stable", () => {
    const s = convSeries(reply);
    expect(s.map((x) => x.label)).toEqual(["room.wav L", "room.wav R", "tilt.wav", "Sub R→L", "Combined L", "Combined R"]);
    expect(s.map((x) => x.key)).toEqual(["item:0:0:0", "item:0:1:1", "item:2:-:-", "item:3:1:0", "out:0", "out:1"]);
    expect(routeName({ out: 1, mag_db: [], group_delay_ms: [] })).toBe("R");
  });

  it("shows the combined curves by default, items once toggled", () => {
    const s = convSeries(reply);
    expect(s.filter((x) => seriesShown(x, {})).map((x) => x.key)).toEqual(["out:0", "out:1"]);
    expect(s.filter((x) => seriesShown(x, { "out:1": false, "item:3:1:0": true })).map((x) => x.key)).toEqual(["item:3:1:0", "out:0"]);
  });
});

describe("smoothing", () => {
  it("leaves a flat curve flat and averages power over the span", () => {
    expect(smoothDb(DRAW_FREQS, flat(-3), 3).every((v) => Math.abs(v + 3) < 1e-9)).toBe(true);
    // A one-point notch is filled in mostly; a one-point peak of +20 dB rises the average.
    const notch = flat(0);
    notch[120] = -200;
    const s = smoothDb(DRAW_FREQS, notch, 3);
    expect(s[120]).toBeGreaterThan(-1);
    expect(s[0]).toBeCloseTo(0, 9);
    expect(smoothDb(DRAW_FREQS, notch, 0)).toEqual(notch);
  });

  it("averages group delay, skipping the points without one", () => {
    const gd: (number | null)[] = DRAW_FREQS.map(() => 2);
    gd[100] = null;
    const s = smoothGd(DRAW_FREQS, gd, 6);
    expect(s[100]).toBeCloseTo(2, 9);
    expect(smoothGd(DRAW_FREQS, DRAW_FREQS.map(() => null), 3).every((v) => v === null)).toBe(true);
  });
});

describe("fit", () => {
  it("fits dB in 6 dB steps, ignoring the silent floor", () => {
    expect(fitRange([flat(0)], "mag")).toEqual({ y0: -12, y1: 6 });
    expect(fitRange([[-20, 9, -200]], "mag")).toEqual({ y0: -24, y1: 12 });
    expect(fitRange([[-200, null]], "mag")).toEqual({ y0: -24, y1: 12 });
  });

  it("fits delay from zero up", () => {
    const r = fitRange([[1, 9.5]], "gd");
    expect(r.y0).toBe(0);
    expect(r.y1).toBeGreaterThanOrEqual(10);
  });
});
