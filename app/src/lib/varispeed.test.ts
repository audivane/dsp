import { describe, expect, it } from "vitest";
import type { VarispeedStatus } from "../api/types";
import { clampSpeed, fmtSemitones, fmtSpeedPercent, keepPitchSupported, rangeBounds, rangeFor, speedReadout, speedSemitones, transientsSoften, varispeedNote } from "./varispeed";

const status = (over: Partial<VarispeedStatus> = {}): VarispeedStatus => ({
  enabled: true, keep_pitch: false, active: true, speed: 1, target: 1, requested: 1, speed_limit: 2, cap_reason: null,
  ramping: false, fading: false, latency_frames: 0, latency_exact: true, held: null, overload_hold: "none",
  underfeed_events: 0, underfeed_frames: 0, short_cycles: 0, gap_fades: 0, error: null, ...over,
});

describe("varispeed formatting", () => {
  it("formats the speed as a signed percent", () => {
    expect(fmtSpeedPercent(1.02)).toBe("+2.0 %");
    expect(fmtSpeedPercent(0.95)).toBe("−5.0 %");
    expect(fmtSpeedPercent(1)).toBe("±0.0 %");
    expect(fmtSpeedPercent(1.00001)).toBe("±0.0 %");
  });

  it("formats semitones as 12·log2(speed)", () => {
    expect(speedSemitones(2)).toBeCloseTo(12);
    expect(speedSemitones(0.5)).toBeCloseTo(-12);
    expect(fmtSemitones(1.02)).toBe("+0.34 st");
    expect(fmtSemitones(0.92)).toBe("−1.44 st");
    expect(fmtSemitones(1)).toBe("±0.00 st");
  });
});

describe("varispeed ranges", () => {
  it("bounds each range", () => {
    expect(rangeBounds(8)).toEqual({ min: 0.92, max: 1.08 });
    expect(rangeBounds(50)).toEqual({ min: 0.5, max: 1.5 });
  });

  it("clamps into the range", () => {
    expect(clampSpeed(1.2, 8)).toBe(1.08);
    expect(clampSpeed(0.7, 16)).toBe(0.84);
    expect(clampSpeed(1.03, 8)).toBe(1.03);
    expect(clampSpeed(1.9, 50)).toBe(1.5);
    expect(clampSpeed(NaN, 8)).toBe(1);
  });

  it("picks the narrowest range that holds the speed", () => {
    expect(rangeFor(1)).toBe(8);
    expect(rangeFor(1.08)).toBe(8);
    expect(rangeFor(0.9)).toBe(16);
    expect(rangeFor(1.3)).toBe(50);
    expect(rangeFor(1.8)).toBe(50);
  });
});

describe("varispeedNote", () => {
  it("says nothing when off or free", () => {
    expect(varispeedNote(null)).toBeNull();
    expect(varispeedNote(status({ enabled: false, held: "underfeed" }))).toBeNull();
    expect(varispeedNote(status({ requested: 1.05 }))).toBeNull();
  });

  it("explains a cap, a hold and an overload", () => {
    expect(varispeedNote(status({ requested: 1.4, speed_limit: 1.25, cap_reason: "quantum_too_small" }))).toMatch(/quantum.*\+25\.0 %/);
    expect(varispeedNote(status({ held: "underfeed" }))).toMatch(/held at 1×/);
    expect(varispeedNote(status({ overload_hold: "off" }))).toMatch(/varispeed is off/);
  });
});

describe("keep pitch", () => {
  it("reads semitones off, pitch held on", () => {
    expect(speedReadout(1.02, false)).toBe("+2.0 % · +0.34 st");
    expect(speedReadout(1.02, true)).toBe("+2.0 % · pitch held");
  });

  it("notes softened transients beyond ±25 % with Keep pitch only", () => {
    expect(transientsSoften(1.25, true)).toBe(false);
    expect(transientsSoften(1.3, true)).toBe(true);
    expect(transientsSoften(0.7, true)).toBe(true);
    expect(transientsSoften(1.3, false)).toBe(false);
  });

  it("knows an engine without the field", () => {
    expect(keepPitchSupported({ enabled: false, speed: 1, ramp_ms: 200, keep_pitch: false })).toBe(true);
    expect(keepPitchSupported({ enabled: false, speed: 1, ramp_ms: 200 } as never)).toBe(false);
    expect(keepPitchSupported(null)).toBe(false);
  });

  it("explains a rate too high for the stretch", () => {
    const n = varispeedNote(status({ keep_pitch: true, active: false, error: "Keep pitch is not available at 192000 Hz: …" }));
    expect(n).toMatch(/fits up to 96 kHz/);
  });
});
