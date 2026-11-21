import { describe, expect, it } from "vitest";
import { buildEnvelope, fmtAmplitude, fmtSampleOffset, impulseFrame, rangeMinMax } from "./impulsePlot";

describe("impulse plot", () => {
  it("frames the whole impulse around its peak", () => {
    const f = impulseFrame([0, 0.2, 1, -0.5, 0], 2);
    expect(f.home.x0).toBe(-2.5);
    expect(f.home.x1).toBe(2.5);
    expect(f.home.y0).toBeCloseTo(-0.55);
    expect(f.home.y1).toBe(1.05);
    expect(f.limits.minSpanX).toBe(5);
    // A flat 1:1 reply is one sample: the frame still has a width.
    const one = impulseFrame([1], 0);
    expect(one.home.x1 - one.home.x0).toBe(1);
  });

  it("matches a direct min/max over any range", () => {
    const vals = Array.from({ length: 1000 }, (_, i) => Math.sin(i * 0.37) * Math.exp(-i / 300));
    const env = buildEnvelope(vals);
    for (const [a, b] of [[0, 1], [3, 4], [0, 1000], [17, 401], [998, 1200], [-5, 2]]) {
      const got = rangeMinMax(env, a, b)!;
      const slice = vals.slice(Math.max(0, a), Math.min(1000, b));
      // Blocks at the ends may reach past the range, never inside it.
      expect(got.min).toBeLessThanOrEqual(Math.min(...slice));
      expect(got.max).toBeGreaterThanOrEqual(Math.max(...slice));
    }
    expect(rangeMinMax(env, 3, 4)).toEqual({ min: vals[3], max: vals[3] });
    expect(rangeMinMax(env, 1000, 1010)).toBeNull();
  });

  it("formats offsets and amplitudes", () => {
    expect(fmtSampleOffset(0)).toBe("0");
    expect(fmtSampleOffset(12)).toBe("+12");
    expect(fmtSampleOffset(-1024)).toBe("−1,024");
    expect(fmtAmplitude(-0.123456, 1.35)).toBe("−0.1235");
    expect(fmtAmplitude(0.123456, 0.01)).toBe("0.123456");
  });
});
