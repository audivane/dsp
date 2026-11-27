import { describe, expect, it } from "vitest";
import { NYQUIST_STOP_MIN_TAPS_CASES, nyquistStopBlockReason, nyquistStopMinTaps } from "./upsamplerLimits";

describe("upsampler limits (mirror of dsp-core)", () => {
  it("matches the Rust minimum taps", () => {
    for (const [ratio, beta, taps] of NYQUIST_STOP_MIN_TAPS_CASES) {
      expect(nyquistStopMinTaps(ratio, beta), `x${ratio} beta ${beta}`).toBe(taps);
    }
    expect(nyquistStopMinTaps(4, 30)).toBeNull();
    expect(nyquistStopMinTaps(1, 14)).toBe(1);
  });

});

describe("Stop at Nyquist, as the engine checks it", () => {

  const base = { target: { ratio: 4 }, taps: 64, taps_unit: "input", kaiser_beta: 14, window: "kaiser" } as const;
  it("counts input-sample lengths at the ratio, like manual_taps", () => {
    // 64 per input sample at x4 is 256 output taps: above the 109-tap minimum.
    expect(nyquistStopBlockReason(base, [44100])).toBeNull();
    expect(nyquistStopBlockReason({ ...base, taps: 27 }, [44100])).toContain("109");
    expect(nyquistStopBlockReason({ ...base, taps_unit: "output", taps: 64 }, [44100])).toContain("109");
  });

});

