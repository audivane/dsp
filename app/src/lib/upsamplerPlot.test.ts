import { describe, expect, it } from "vitest";
import {
  nextWindowToFetch,
  responsePlotTop,
  UPS_Y_MAX_DB,
  UPS_Y_MIN_DB,
  logXOf,
  responseYOf,
  sliceFreqRange,
  zoomFreqRange,
  zoomTicks,
  zoomXOf,
} from "./upsamplerPlot";

describe("upsamplerPlot helpers", () => {
  it("computes the 0.73x..1.18x Nyquist zoom range", () => {
    const r44 = zoomFreqRange(44100);
    expect(r44.nyquistIn).toBe(22050);
    expect(r44.fMin).toBeCloseTo(16096.5, 2);
    expect(r44.fMax).toBeCloseTo(26019, 2);

    const r96 = zoomFreqRange(96000);
    expect(r96.nyquistIn).toBe(48000);
    expect(r96.fMin).toBeCloseTo(35040, 2);
    expect(r96.fMax).toBeCloseTo(56640, 2);
  });

  it("maps linear Nyquist zoom X and log full-range X coordinates", () => {
    expect(zoomXOf(16000, 16000, 26000, 50, 400)).toBe(50);
    expect(zoomXOf(21000, 16000, 26000, 50, 400)).toBe(250);
    expect(zoomXOf(26000, 16000, 26000, 50, 400)).toBe(450);

    expect(logXOf(20, 20, 20000, 40, 300)).toBeCloseTo(40, 6);
    expect(logXOf(200, 20, 20000, 40, 300)).toBeCloseTo(140, 6);
    expect(logXOf(20000, 20, 20000, 40, 300)).toBeCloseTo(340, 6);
  });

  it("maps magnitude dB across 0..-150 dB", () => {
    expect(responseYOf(UPS_Y_MAX_DB, 20, 150)).toBe(20);
    expect(responseYOf(-75, 20, 150)).toBe(95);
    expect(responseYOf(UPS_Y_MIN_DB, 20, 150)).toBe(170);
    expect(responseYOf(-220, 20, 150)).toBe(170);
  });

  it("picks round kHz ticks and slices curves with boundary neighbours", () => {
    expect(zoomTicks(16096.5, 26019)).toEqual([18000, 20000, 22000, 24000, 26000]);
    const sliced = sliceFreqRange([10000, 15000, 17000, 22050, 25000, 27000, 40000], [0, -0.01, -0.05, -3, -130, -140, -145], 16000, 26000);
    expect(sliced.freqs).toEqual([15000, 17000, 22050, 25000, 27000]);
    expect(sliced.mags).toEqual([-0.01, -0.05, -3, -130, -140]);
  });
});

describe("zoom windows", () => {
  const a = { f_lo_hz: 20000, f_hi_hz: 23000, points: 800 };
  const b = { f_lo_hz: 21000, f_hi_hz: 22000, points: 800 };
  it("fetches what is wanted and not yet in", () => {
    expect(nextWindowToFetch({ trans: a, full: null }, { trans: null, full: null }, { trans: null, full: null }, "k")).toBe("trans");
    expect(nextWindowToFetch({ trans: a, full: b }, { trans: { key: "k", req: a }, full: null }, { trans: null, full: null }, "k")).toBe("full");
    expect(nextWindowToFetch({ trans: a, full: null }, { trans: { key: "k", req: a }, full: null }, { trans: null, full: null }, "k")).toBeNull();
    // A window fetched for another design is fetched again.
    expect(nextWindowToFetch({ trans: a, full: null }, { trans: { key: "old", req: a }, full: null }, { trans: null, full: null }, "k")).toBe("trans");
  });
  it("does not retry a refused window until the selection or the design changes", () => {
    const failed = { trans: { key: "k", req: a }, full: null };
    expect(nextWindowToFetch({ trans: a, full: null }, { trans: null, full: null }, failed, "k")).toBeNull();
    expect(nextWindowToFetch({ trans: b, full: null }, { trans: null, full: null }, failed, "k")).toBe("trans");
    expect(nextWindowToFetch({ trans: a, full: null }, { trans: null, full: null }, failed, "k2")).toBe("trans");
  });
});

describe("plot range", () => {
  it("runs to the intermediate grid's Nyquist at a fractional ratio", () => {
    // 44.1 kHz -> 48 kHz runs on a 160 x 44.1 kHz grid: 3.528 MHz Nyquist.
    expect(responsePlotTop({ source_rate: 44100, output_rate: 48000, rate_ratio: { up: 160, down: 147 } })).toBe(3528000);
    expect(responsePlotTop({ source_rate: 44100, output_rate: 352800, rate_ratio: { up: 8, down: 1 } })).toBe(176400);
    expect(responsePlotTop({ source_rate: 96000, output_rate: 48000, rate_ratio: { up: 1, down: 2 } })).toBe(48000);
    expect(responsePlotTop({ source_rate: 44100, output_rate: 44100, rate_ratio: { up: 1, down: 1 } })).toBe(22050);
  });
});
