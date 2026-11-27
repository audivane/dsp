import { describe, expect, it } from "vitest";
import { polyphasePlan, stageParts } from "./polyphasePlan";

/** `block x partitions` per stage, as the daemon's `Upsampler::stages` prints them. */
function stages(base: number, r: number, taps: number): string {
  const len = Math.ceil(taps / r);
  return polyphasePlan(base, r, len)
    .map((st, i) => `${st.block}x${Math.max(i === 0 ? 1 : 0, stageParts(st, len))}`)
    .join("+");
}

describe("FFT polyphase partition schedule (mirror of nupc::polyphase_plan)", () => {
  it("matches the engine's schedules", () => {
    // Taken from the engine (bench_upsampler_nupc, bench_plan_crossover).
    expect(stages(512, 16, 1 << 20)).toBe("512x8+2048x6+8192x6");
    expect(stages(1024, 16, 1 << 20)).toBe("1024x8+4096x14");
    expect(stages(512, 16, 1 << 23)).toBe("512x8+2048x6+8192x6+32768x14");
    expect(stages(1024, 16, 1 << 23)).toBe("1024x8+4096x14+32768x14");
    expect(stages(1024, 8, 1 << 23)).toBe("1024x8+4096x14+32768x30");
    expect(stages(1024, 4, 1 << 23)).toBe("1024x8+4096x14+32768x62");
    expect(stages(1024, 64, 128 * 1024 * 64)).toBe("1024x16+8192x14");
    expect(stages(512, 16, 24 * 512 * 16)).toBe("512x8+2048x4");
  });

});
