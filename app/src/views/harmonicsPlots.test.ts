import { describe, expect, it } from "vitest";
import { fmtHzExact, followNyquist, spectrumHome, viewForScale, xPresetRange, yPresetRange } from "./HarmonicsSpectrumPlot";
import { levelHome, pointsSummary } from "./HarmonicsView";
import type { HarmonicsReadoutPoint } from "../api/types";
import { parseLimit, withLimit } from "./ZoomPlot";
import { liveFit, liveYRange } from "./HarmonicsLivePanel";

describe("zoom plot limits", () => {
  it("parses typed limits", () => {
    expect(parseLimit("−160")).toBe(-160);
    expect(parseLimit(" -12.5 dB")).toBe(-12.5);
    expect(parseLimit("20k")).toBe(20000);
    expect(parseLimit("1.5 kHz")).toBe(1500);
    expect(parseLimit("abc")).toBeNull();
    expect(parseLimit("")).toBeNull();
  });
  it("sets one end, refusing an empty range", () => {
    const v = { x0: 20, x1: 20000, y0: -180, y1: 5 };
    expect(withLimit(v, "y0", -200, "log")).toEqual({ ...v, y0: -200 });
    expect(withLimit(v, "y1", -190, "log")).toBeNull();
    expect(withLimit(v, "x0", 0, "log")).toBeNull();
    expect(withLimit(v, "x0", 0, "lin")).toEqual({ ...v, x0: 0 });
  });
});

describe("harmonics plot views", () => {
  it("homes the spectrum at +5 to −160 dBr, 20 Hz–20 kHz", () => {
    expect(spectrumHome("log")).toEqual({ x0: 20, x1: 20000, y0: -160, y1: 5 });
    expect(spectrumHome("lin")).toEqual({ x0: 0, x1: 20000, y0: -160, y1: 5 });
  });
  it("gives every y preset 5 dB of headroom", () => {
    expect(yPresetRange("160", -181)).toEqual({ y0: -160, y1: 5 });
    expect(yPresetRange("200", -181)).toEqual({ y0: -200, y1: 5 });
    expect(yPresetRange("fit", -181)).toEqual({ y0: -200, y1: 5 });
    expect(yPresetRange("fit", -163)).toEqual({ y0: -180, y1: 5 });
  });
  it("spans the x presets, past Nyquist too", () => {
    expect(xPresetRange("100k", "log", 24000)).toEqual({ x0: 10, x1: 100000 });
    expect(xPresetRange("50k", "lin", 24000)).toEqual({ x0: 0, x1: 50000 });
    expect(xPresetRange("nyquist", "log", 176400)).toEqual({ x0: 20, x1: 176400 });
    // 1 kHz sits in the middle of 10 Hz–100 kHz on a log axis.
    expect(Math.sqrt(10 * 100000)).toBe(1000);
  });
  it("keeps a view across a scale change and follows Nyquist only from it", () => {
    expect(viewForScale({ x0: 20, x1: 50000, y0: -160, y1: 5 }, "lin").x0).toBe(0);
    expect(viewForScale({ x0: 0, x1: 100000, y0: -160, y1: 5 }, "log").x0).toBe(10);
    expect(viewForScale({ x0: 0, x1: 20000, y0: -160, y1: 5 }, "log").x0).toBe(20);
    const v = { x0: 20, x1: 48000, y0: -160, y1: 5 };
    expect(followNyquist(v, 48000, 96000).x1).toBe(96000);
    expect(followNyquist({ ...v, x1: 20000 }, 48000, 96000).x1).toBe(20000);
  });
  it("writes the snapped tone to a tenth of a hertz", () => {
    expect(fmtHzExact(996.0938)).toBe("996.1 Hz");
    expect(fmtHzExact(10001.95)).toBe("10.002 kHz");
    expect(fmtHzExact(2000)).toBe("2 kHz");
  });
  it("fits the level plot to its curves", () => {
    expect(levelHome([[-120, -60, -34], [-200, -200, -200]])).toEqual({ x0: -40, x1: 0, y0: -130, y1: -20 });
    expect(levelHome([[-200]])).toEqual({ x0: -40, x1: 0, y0: -140, y1: 0 });
    expect(levelHome([[-195, -5]]).y0).toBe(-200);
  });
});

describe("the sine table's summary", () => {
  const pt = (freq_hz: number, input_dbfs: number, thd_db: number, aliased = false): HarmonicsReadoutPoint =>
    ({ freq_hz, input_dbfs, thd_db, gedlee_gm: 0, gain_db: 0, harmonics_db: new Array(14).fill(-200), ...(aliased ? { aliased: [{ order: 5, freq_hz: 4000, level_db: -90 }] } : {}) }) as HarmonicsReadoutPoint;
  it("puts THD of every sine on one line", () => {
    const pts = [pt(1000, 0, -33.8), pt(40, 0, -31.6), pt(1000, -18, -51.9), pt(40, -18, -200)];
    expect(pointsSummary(pts, "db")).toBe("THD at 1 kHz / 40 Hz: −33.8 / −31.6 dB at 0 dBFS, −51.9 / none dB at −18 dBFS");
    expect(pointsSummary([pt(1000, 0, -40, true)], "pct")).toBe("THD at 1 kHz: 1.00 % at 0 dBFS · some orders fold back");
    expect(pointsSummary([], "db")).toBe("No test sines.");
  });
});

describe("live spectrum range", () => {
  it("spans the level presets in either unit", () => {
    expect(liveYRange("160", "dbfs", null)).toEqual({ y0: -160, y1: 5 });
    expect(liveYRange("200", "dbr", null)).toEqual({ y0: -200, y1: 10 });
    expect(liveYRange("fit", "dbr", -130)).toEqual({ y0: -130, y1: 10 });
  });
  it("fits 10 dB under the quietest trace within the span", () => {
    const freqs = [20, 1000, 50000];
    expect(liveFit([[-30, -40, -50], [-90, -121, -170]], freqs, 20, 20000)).toBe(-140);
    expect(liveFit([[-30, -40, -50], [-90, -121, -170]], freqs, 20, 100000)).toBe(-180);
    expect(liveFit([[]], [], 20, 20000)).toBeNull();
  });
});
