import { describe, expect, it } from "vitest";
import {
  boxZoom,
  clampView,
  fmtDbPrecise,
  fmtHzPrecise,
  interpAt,
  linearTicks,
  logTicks,
  panBy,
  spliceWindow,
  steepnessText,
  viewForCrossing,
  zoomAt,
  type ViewLimits,
} from "./plotView";
import { cutoffTips, fmtHzStat, fmtRipple, windowRequestFor } from "./upsamplerPlot";

const LIM: ViewLimits = { x0: 0, x1: 48000, y0: -220, y1: 20, minSpanX: 0.002, minSpanY: 0.00002 };
const HOME = { x0: 17520, x1: 28320, y0: -150, y1: 3 };

describe("plot view maths", () => {
  it("zooms around the cursor and keeps the anchor fixed", () => {
    const v = zoomAt(HOME, 0.5, 0.25, 0.5, "x", LIM);
    const anchor = HOME.x0 + 0.25 * (HOME.x1 - HOME.x0);
    expect(v.x0 + 0.25 * (v.x1 - v.x0)).toBeCloseTo(anchor, 6);
    expect(v.x1 - v.x0).toBeCloseTo((HOME.x1 - HOME.x0) / 2, 6);
    expect(v.y0).toBe(HOME.y0);
    const y = zoomAt(HOME, 0.1, 0.5, 1, "y", LIM);
    expect(y.x0).toBe(HOME.x0);
    expect(y.y1).toBeCloseTo(3, 9);
    expect(y.y1 - y.y0).toBeCloseTo(15.3, 6);
  });

  it("zooms on a log axis around the cursor in log space", () => {
    const lim: ViewLimits = { x0: 10, x1: 48000, y0: -220, y1: 20, minSpanX: 1.00001, minSpanY: 0.0001 };
    const v = zoomAt({ x0: 20, x1: 48000, y0: -150, y1: 3 }, 0.5, 0.5, 0.5, "x", lim, "log");
    const mid = Math.sqrt(20 * 48000);
    expect(Math.sqrt(v.x0 * v.x1)).toBeCloseTo(mid, 3);
  });

  it("never shrinks below the minimum spans or leaves the limits", () => {
    let v = HOME;
    for (let i = 0; i < 200; i++) v = zoomAt(v, 0.5, 0.5, 0.5, "xy", LIM);
    expect(v.x1 - v.x0).toBeCloseTo(0.002, 9);
    expect(v.y1 - v.y0).toBeCloseTo(0.00002, 9);
    let out = HOME;
    for (let i = 0; i < 50; i++) out = zoomAt(out, 2, 0, 0, "xy", LIM);
    expect(out).toEqual({ x0: 0, x1: 48000, y0: -220, y1: 20 });
  });

  it("pans by plot fractions, keeping the span and the limits", () => {
    const v = panBy(HOME, 0.1, 0, LIM);
    expect(v.x1 - v.x0).toBeCloseTo(HOME.x1 - HOME.x0, 6);
    expect(v.x0).toBeCloseTo(HOME.x0 - 1080, 6);
    const edge = panBy({ x0: 100, x1: 1100, y0: -10, y1: 0 }, 5, -50, LIM);
    expect(edge.x0).toBe(0);
    expect(edge.x1).toBe(1000);
    expect(edge.y1).toBe(20);
  });

  it("box zoom honours the axis mode and normalises the drag direction", () => {
    const x = boxZoom(HOME, { x0: 24010, x1: 23990, y0: -1, y1: -2 }, "x", LIM);
    expect(x).toEqual({ x0: 23990, x1: 24010, y0: -150, y1: 3 });
    const xy = boxZoom(HOME, { x0: 23990, x1: 24010, y0: -0.0001, y1: -0.002 }, "xy", LIM);
    expect(xy.y0).toBeCloseTo(-0.002, 12);
    expect(xy.y1).toBeCloseTo(-0.0001, 12);
    expect(clampView({ x0: 5, x1: 5, y0: 0, y1: 0 }, LIM).x1 - 5).toBeCloseTo(0.001, 9);
  });

  it("makes round ticks and precise labels", () => {
    expect(linearTicks(-150, 0, 6).ticks).toEqual([-140, -120, -100, -80, -60, -40, -20, 0]);
    expect(linearTicks(23999.9, 24000.1, 4).step).toBeCloseTo(0.05, 12);
    expect(logTicks(20, 24000).slice(0, 4)).toEqual([20, 50, 100, 200]);
    expect(fmtHzPrecise(23999.9123, 0.2)).toBe("23,999.912");
    expect(fmtHzPrecise(23999.9123, 10800)).toBe("24k");
    expect(fmtDbPrecise(-0.000123, 0.002)).toBe("−0.000123");
    expect(fmtDbPrecise(-63.37, 150)).toBe("−63.37");
  });

  it("interpolates the curve under the cursor", () => {
    expect(interpAt([0, 10, 20], [0, -10, -30], 15)).toBe(-20);
    expect(interpAt([0, 10], [0, -10], 11)).toBeNull();
  });

  it("splices a dense window into the base points", () => {
    const out = spliceWindow([0, 10, 20, 30, 40], [0, -1, -2, -3, -4], {
      f_lo_hz: 15,
      f_hi_hz: 31,
      freqs_hz: [15, 23, 31],
      magnitude_db: [-1.5, -2.3, -3.1],
    });
    expect(out.freqs).toEqual([0, 10, 15, 23, 31, 40]);
    expect(out.mags).toEqual([0, -1, -1.5, -2.3, -3.1, -4]);
    expect(spliceWindow([1], [2], null)).toEqual({ freqs: [1], mags: [2] });
  });

  it("zooms to a crossing and describes the steepness", () => {
    const crossings = [
      { level_db: -0.0001, freq_hz: 23999.62 },
      { level_db: -0.001, freq_hz: 23999.65 },
      { level_db: -0.1, freq_hz: 23999.76 },
      { level_db: -150, freq_hz: 24000.87 },
    ];
    const v = viewForCrossing(crossings, 1, LIM);
    expect(v.x0).toBeLessThan(23999.65);
    expect(v.x1).toBeGreaterThan(23999.65);
    expect(v.x1 - v.x0).toBeCloseTo(0.12, 6);
    expect(v.y0).toBeLessThan(-0.001);
    expect(v.y1).toBeGreaterThan(-0.001);
    expect(v.y1 - v.y0).toBeLessThan(0.01);
    const deep = viewForCrossing(crossings, 3, LIM);
    expect(deep.y0).toBeLessThan(-150);
    expect(deep.y1).toBeGreaterThan(-150);
    expect(steepnessText(crossings)).toBe("−0.001 → −150 dB in 1.22 Hz");
    expect(steepnessText([])).toBeNull();
  });
});

describe("upsampling view helpers", () => {
  it("asks for a window only where the base points are too sparse", () => {
    expect(windowRequestFor({ x0: 23990, x1: 24010 }, "lin", 48000)).toEqual({ f_lo_hz: 23990, f_hi_hz: 24010, points: 1200 });
    expect(windowRequestFor({ x0: -5, x1: 60000 }, "lin", 48000)).toEqual({ f_lo_hz: 0, f_hi_hz: 48000, points: 1200 });
    expect(windowRequestFor({ x0: 20, x1: 48000 }, "log", 48000)).toBeNull();
    expect(windowRequestFor({ x0: 20000, x1: 26000 }, "log", 48000)).not.toBeNull();
  });

  it("formats precise stats", () => {
    expect(fmtHzStat(23999.9374, 3)).toBe("23,999.937");
    expect(fmtRipple(0.0234)).toBe("0.023 dB");
    expect(fmtRipple(0.00000312)).toBe("0.000003 dB");
    expect(fmtRipple(1e-9)).toBe("< 0.000001 dB");
  });

  it("explains the cutoff choices with numbers for the source rate", () => {
    const at48 = cutoffTips(48000, null);
    expect(at48.full).toContain("24 kHz");
    expect(at48.nyquistStop).toContain("24 kHz");
    expect(at48.nyquistStop).toContain("Zero imaging");
    expect(at48.apod).toContain("22.8 kHz");
    expect(at48.apod).toContain("no imaging");
    expect(at48.apod).toContain("pre-ringing");
    expect(cutoffTips(44100, null).apod).toContain("20.95 kHz");
    const measured = cutoffTips(48000, {
      ratio: 2,
      source_rate: 48000,
      effective_cutoff: 0.95,
      minus_3_db_hz: 22799.94,
      crossings: [
        { level_db: -0.1, freq_hz: 22799.76 },
        { level_db: -6, freq_hz: 22800 },
        { level_db: -100, freq_hz: 22800.38 },
      ],
    });
    expect(measured.apod).toContain("Measured now: −0.1 dB at 22,799.76 Hz, −100 dB at 22,800.38 Hz.");
    expect(measured.full).not.toContain("Measured now");
    expect(measured.nyquistStop).not.toContain("Measured now");

    const nyqStopMeasured = cutoffTips(
      48000,
      {
        ratio: 16,
        source_rate: 48000,
        effective_cutoff: 0.999837,
        minus_3_db_hz: 23996.08,
        crossings: [
          { level_db: -0.1, freq_hz: 23993.1 },
          { level_db: -100, freq_hz: 23999.2 },
        ],
      },
      "nyquist_stop",
    );
    expect(nyqStopMeasured.nyquistStop).toContain("Cutoff now: 23,996.09 Hz");
    expect(nyqStopMeasured.nyquistStop).toContain("Measured now: −0.1 dB at 23,993.10 Hz, −100 dB at 23,999.20 Hz.");
    expect(nyqStopMeasured.full).not.toContain("Measured now");
    expect(nyqStopMeasured.apod).not.toContain("Measured now");
  });
});
