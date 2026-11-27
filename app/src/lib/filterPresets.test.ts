import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { FilterPreset } from "../api/types";
import { PRESETS, presetBlockReason, presetRationalCost, presetRatioNeed } from "./filterPresets";
import { rateRatioOf } from "./upsamplerRate";

/** The names in the generated `FilterPreset` union (what the engine accepts). */
function engineNames(): string[] {
  const src = readFileSync(fileURLToPath(new URL("../api/generated/FilterPreset.ts", import.meta.url)), "utf8");
  const union = src.slice(src.indexOf("export type FilterPreset"));
  return [...union.matchAll(/"([a-z0-9_]+)"/g)].map((m) => m[1]);
}

describe("preset table", () => {
  it("has exactly one entry per engine preset, and no others", () => {
    const names = engineNames();
    expect(names.length).toBeGreaterThanOrEqual(66);
    const table = PRESETS.map((p) => p.name as string);
    expect(new Set(table).size).toBe(table.length);
    expect([...table].sort()).toEqual([...names].sort());
  });

});

describe("ratio rules", () => {
  it("mirrors the engine's check_rate_ratio", () => {
    expect(presetRatioNeed("gaussian_long", { up: 3, down: 1 })).toBeNull();
    expect(presetRatioNeed("gaussian_long", { up: 64, down: 1 })).toBeNull();
    expect(presetRatioNeed("gaussian_long", { up: 65, down: 1 })).toBe("a ratio from 2 to 64");
    expect(presetRatioNeed("cascade_fir", { up: 48, down: 1 })).toBeNull();
    expect(presetRatioNeed("block_fft_large", { up: 64, down: 1 })).toBeNull();
    // The one kernel past the engine's 4,194,304-tap limit above ×32.
    expect(presetRatioNeed("lanczos65535", { up: 32, down: 1 })).toBeNull();
    expect(presetRatioNeed("lanczos65535", { up: 64, down: 1 })).toBe("a ratio of at most ×32 (its kernel would be longer than 4,194,304 taps)");
    // An engine that publishes the 8 Mi-tap limit runs it up to ×64.
    expect(presetRatioNeed("lanczos65535", { up: 64, down: 1 }, 8_388_608)).toBeNull();
    expect(presetBlockReason("lanczos65535", { ratio: 64 }, [44100], 2, 8_388_608)).toBeNull();
    expect(presetBlockReason("lanczos65535", { ratio: 64 }, [44100])).toContain("×32");
    expect(presetRatioNeed("lanczos32767", { up: 64, down: 1 })).toBeNull();
    expect(presetRatioNeed("cascade_fir", { up: 12, down: 1 })).toBeNull();
    expect(presetRatioNeed("cascade_fir", { up: 18, down: 1 })).toBeNull();
    expect(presetRatioNeed("cascade_fir", { up: 3, down: 1 })).toContain("even ratio");
    expect(presetRatioNeed("cascade_fir", { up: 10, down: 1 })).toContain("even ratio");
    expect(presetRatioNeed("block_fft", { up: 16, down: 1 })).toBeNull();
    expect(presetRatioNeed("block_fft", { up: 6, down: 1 })).toContain("power-of-two");
    expect(presetRatioNeed("nos", { up: 160, down: 147 })).toContain("integer");
    expect(presetRatioNeed("nos", { up: 64, down: 1 })).toBeNull();
    expect(presetRatioNeed("cosine", { up: 160, down: 147 })).toContain("integer");
    expect(presetRatioNeed("lanczos2047", { up: 160, down: 147 })).toBeNull();
    expect(presetRatioNeed("lanczos2047", { up: 1, down: 2 })).toContain("at or above");
    expect(presetRatioNeed("halfband_kaiser", { up: 1, down: 2 })).toContain("at or above");
    expect(presetRatioNeed("gaussian_long", { up: 1, down: 2 })).toBeNull();
    expect(presetRatioNeed("gaussian_huge", { up: 160, down: 147 })).toContain("integer");
    expect(presetRatioNeed("gaussian_long", { up: 1, down: 1 })).toBeNull();
  });

});

describe("rational cost, as the engine computes it", () => {
  // From dsp-core `presets::rational::grid_taps` (intermediate phase is 4x,
  // decimation lengthens and makes the length odd per family).
  const GRID_TAPS: [FilterPreset, number, number, number][] = [
    ["gaussian_early_ip", 44100, 48000, 163200],
    ["gaussian_early_ip", 44100, 384000, 1305600],
    ["gaussian_early_ip", 96000, 48000, 2036],
    ["gaussian_early_ip", 192000, 48000, 4076],
    ["gaussian_early_ip", 88200, 48000, 150080],
    ["gaussian_early_ip", 48000, 44100, 162876],
    ["gaussian_early_ip", 176400, 96000, 150080],
    ["gaussian_long", 44100, 48000, 81760],
    ["gaussian_long", 44100, 384000, 654080],
    ["gaussian_long", 96000, 48000, 1021],
    ["gaussian_long", 192000, 48000, 2043],
    ["gaussian_long", 88200, 48000, 75120],
    ["gaussian_long", 48000, 44100, 81879],
    ["gaussian_long", 176400, 96000, 75120],
    ["chebyshev", 44100, 48000, 18720],
    ["chebyshev", 44100, 384000, 149760],
    ["chebyshev", 96000, 48000, 383],
    ["chebyshev", 192000, 48000, 765],
    ["chebyshev", 88200, 48000, 28080],
    ["chebyshev", 48000, 44100, 30723],
    ["chebyshev", 176400, 96000, 28080],
    ["chebyshev_short", 44100, 48000, 11360],
    ["chebyshev_short", 44100, 384000, 90880],
    ["chebyshev_short", 96000, 48000, 255],
    ["chebyshev_short", 192000, 48000, 509],
    ["chebyshev_short", 88200, 48000, 18800],
    ["chebyshev_short", 48000, 44100, 20433],
    ["chebyshev_short", 176400, 96000, 18800],
    ["chebyshev_long", 44100, 48000, 33120],
    ["chebyshev_long", 44100, 384000, 264960],
    ["chebyshev_long", 96000, 48000, 413],
    ["chebyshev_long", 192000, 48000, 827],
    ["chebyshev_long", 88200, 48000, 30480],
    ["chebyshev_long", 48000, 44100, 33075],
    ["chebyshev_long", 176400, 96000, 30480],
    ["kaiser_steep", 44100, 48000, 92320],
    ["kaiser_steep", 44100, 384000, 738560],
    ["kaiser_steep", 96000, 48000, 1155],
    ["kaiser_steep", 192000, 48000, 2309],
    ["kaiser_steep", 88200, 48000, 84880],
    ["kaiser_steep", 48000, 44100, 92463],
    ["kaiser_steep", 176400, 96000, 84880],
    ["kaiser_steep_short", 44100, 48000, 40800],
    ["kaiser_steep_short", 44100, 384000, 326400],
    ["kaiser_steep_short", 96000, 48000, 511],
    ["kaiser_steep_short", 192000, 48000, 1021],
    ["kaiser_steep_short", 88200, 48000, 37520],
    ["kaiser_steep_short", 48000, 44100, 41013],
    ["kaiser_steep_short", 176400, 96000, 37520],
    ["flattop_xlong", 44100, 48000, 655200],
    ["flattop_xlong", 44100, 384000, 5241600],
    ["flattop_xlong", 96000, 48000, 8189],
    ["flattop_xlong", 192000, 48000, 16379],
    ["flattop_xlong", 88200, 48000, 602000],
    ["flattop_xlong", 48000, 44100, 655179],
    ["flattop_xlong", 176400, 96000, 602000],
    ["gaussian_compact", 44100, 48000, 26720],
    ["gaussian_compact", 44100, 384000, 213760],
    ["chebyshev_long_ip", 44100, 48000, 132480],
    ["chebyshev_long_ip", 44100, 384000, 1059840],
    ["chebyshev_long_ip", 96000, 48000, 1652],
    ["chebyshev_long_ip", 192000, 48000, 3308],
    ["chebyshev_long_ip", 88200, 48000, 121920],
    ["chebyshev_long_ip", 48000, 44100, 132300],
    ["chebyshev_long_ip", 176400, 96000, 121920],
    ["lanczos2047", 44100, 48000, 655200],
    ["lanczos2047", 44100, 384000, 5241600],
    ["short_leaky", 44100, 48000, 7520],
    ["short_leaky", 44100, 384000, 60160],
    ["nuttall", 44100, 48000, 18720],
    ["nuttall", 44100, 384000, 149760],
    ["halfband_large", 44100, 48000, 40800],
    ["halfband_large", 44100, 384000, 326400],
    ["flattop_early_ip", 44100, 48000, 163200],
    ["flattop_early_ip", 44100, 384000, 1305600],
    ["flattop_early_ip", 96000, 48000, 2036],
    ["flattop_early_ip", 192000, 48000, 4076],
    ["flattop_early_ip", 88200, 48000, 150080],
    ["flattop_early_ip", 48000, 44100, 162876],
    ["flattop_early_ip", 176400, 96000, 150080],
  ];

  it("matches the engine's grid lengths", () => {
    for (const [name, input, output, taps] of GRID_TAPS) {
      expect(presetRationalCost(name, input, rateRatioOf(input, output)).gridTaps, `${name} ${input} → ${output}`).toBe(taps);
    }
  });

});
