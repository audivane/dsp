import { describe, expect, it } from "vitest";
import { heaviestWindow, polyphasePlan, polyphaseSeconds, stageParts } from "./polyphasePlan";

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

  it("keeps one stage for short kernels", () => {
    for (const base of [512, 1024]) {
      for (let r = 2; r <= 64; r++) {
        expect(polyphasePlan(base, r, 16 * base)).toHaveLength(1);
      }
    }
  });

  it("prices one stage as the uniform engine and long kernels far below it", () => {
    const uniform = (taps: number, r: number, b: number, fs: number) => ((fs / b) * Math.max(1, Math.ceil(Math.ceil(taps / r) / b)) * r * (b + 1) * 1.15) / 0.5e9;
    expect(polyphaseSeconds(65536, 16, 512, 512, 44100)).toBeCloseTo(uniform(65536, 16, 512, 44100), 12);
    const long = polyphaseSeconds(1 << 23, 16, 512, 512, 44100);
    expect(long).toBeLessThan(uniform(1 << 23, 16, 512, 44100) / 5);
    // 44.1 kHz on 512-frame blocks no longer costs about twice 48 kHz on 1024.
    expect(long / polyphaseSeconds(1 << 23, 16, 1024, 1024, 48000)).toBeLessThan(1.3);
  });

  it("prices the heaviest window anywhere in the period", () => {
    const t = [1, 0, 0, 5];
    expect(heaviestWindow(t, 1)).toBe(5);
    expect(heaviestWindow(t, 2)).toBe(6);
    expect(heaviestWindow(t, 4)).toBe(6);
    expect(heaviestWindow(t, 6)).toBe(12);
  });
});
