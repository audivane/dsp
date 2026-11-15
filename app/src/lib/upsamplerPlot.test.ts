import { describe, expect, it } from "vitest";
import {
  UPS_Y_MAX_DB,
  UPS_Y_MIN_DB,
  logXOf,
  responseYOf,
  sliceFreqRange,
  zoomFreqRange,
  zoomTicks,
  zoomXOf,
} from "./upsamplerPlot";
import { upsamplingSummary } from "./summary";

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

  it("summarizes full, apodizing, and stop-at-Nyquist cutoff modes", () => {
    expect(
      upsamplingSummary({ target: { ratio: 4 }, taps: 65536, phase: "linear", kaiser_beta: 14, cutoff: 1, cutoff_mode: "fraction" }),
    ).toBe("×4 · 64k taps · linear");
    expect(
      upsamplingSummary({ target: { ratio: 4 }, taps: 65536, phase: "linear", kaiser_beta: 14, cutoff: 0.95, cutoff_mode: "fraction" }),
    ).toBe("×4 · 64k taps · linear · apodizing");
    expect(
      upsamplingSummary({ target: { ratio: 16 }, taps: 1048576, phase: "minimum", kaiser_beta: 14, cutoff: 1, cutoff_mode: "nyquist_stop" }),
    ).toBe("×16 · 1M taps · minimum · stop at Nyquist");
  });
});
