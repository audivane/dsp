import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { FilterPreset } from "../api/types";
import { dimOptions, dimValues, familyDims, familyPick, FAMILIES, PRESETS, presetBlockReason, presetDetail, presetEntry, presetRationalCost, presetRatioNeed, presetSummary } from "./filterPresets";
import { rateRatioOf } from "./upsamplerRate";

/** The names in the generated `FilterPreset` union (what the engine accepts). */
function engineNames(): string[] {
  const src = readFileSync(fileURLToPath(new URL("../api/generated/FilterPreset.ts", import.meta.url)), "utf8");
  const union = src.slice(src.indexOf("export type FilterPreset"));
  return [...union.matchAll(/"([a-z0-9_]+)"/g)].map((m) => m[1]);
}

const none = () => null;

describe("preset table", () => {
  it("has exactly one entry per engine preset, and no others", () => {
    const names = engineNames();
    expect(names.length).toBeGreaterThanOrEqual(66);
    const table = PRESETS.map((p) => p.name as string);
    expect(new Set(table).size).toBe(table.length);
    expect([...table].sort()).toEqual([...names].sort());
  });

  it("puts every entry in a known family, and each family has its default", () => {
    const ids = new Set(FAMILIES.map((f) => f.id));
    for (const p of PRESETS) expect(ids.has(p.family)).toBe(true);
    for (const f of FAMILIES) expect(presetEntry(f.default)?.family).toBe(f.id);
    for (const f of FAMILIES) expect(PRESETS.some((p) => p.family === f.id)).toBe(true);
  });

  it("names every family's shapes", () => {
    for (const p of PRESETS) {
      if (p.shape === null) continue;
      expect(FAMILIES.find((f) => f.id === p.family)?.shapes?.[p.shape], `${p.name} shape ${p.shape}`).toBeDefined();
    }
  });

  it("tells presets apart by family, length, shape and phase", () => {
    const keys = PRESETS.map((p) => `${p.family}|${p.length}|${p.shape}|${p.phase}`);
    const dup = keys.filter((k, i) => keys.indexOf(k) !== i);
    expect(dup).toEqual([]);
  });

  it("reads the phase from the name", () => {
    for (const p of PRESETS) {
      if (p.name.endsWith("_mp")) expect(p.phase).toBe("minimum");
      if (p.name.endsWith("_ip")) expect(p.phase).toBe("intermediate");
    }
    expect(presetEntry("cascade_elliptic")?.phase).toBe("minimum");
    expect(presetEntry("cascade_fir_asym")?.phase).toBe("intermediate");
  });
});

describe("variant rows", () => {
  it("shows only the rows that are a choice", () => {
    expect(familyDims("gaussian")).toEqual(["length", "shape", "phase"]);
    expect(familyDims("chebyshev")).toEqual(["length", "phase"]);
    expect(familyDims("nuttall")).toEqual([]);
    expect(familyDims("cascade")).toEqual(["shape", "phase"]);
    expect(familyDims("halfband")).toEqual(["length", "shape"]);
    expect(dimValues("lanczos", "length")).toEqual(["a23", "a511", "a2047", "a8191", "a32767", "a65535"]);
    expect(dimValues("flattop", "length")).toEqual(["short", "compact", "medium", "long", "xlong", "huge", "mega"]);
  });

  it("disables combinations that do not exist, with a reason", () => {
    const cur = presetEntry("gaussian_long")!;
    const shapes = dimOptions(cur, "shape", none);
    const early = shapes.find((o) => o.value === "early")!;
    expect(early.pick).toBeNull();
    expect(early.tip).toContain("no long Gaussian");
    expect(shapes.find((o) => o.value === "nyquist")!.pick).toBe("gaussian_nyquist");
    const phases = dimOptions(presetEntry("gaussian_early")!, "phase", none);
    expect(phases.map((o) => o.pick)).toEqual(["gaussian_early", "gaussian_early_ip", "gaussian_early_mp"]);
  });

  it("keeps the other choices where it can when one changes", () => {
    const cur = presetEntry("flattop_xlong_apod")!;
    const lengths = dimOptions(cur, "length", none);
    expect(lengths.find((o) => o.value === "long")!.pick).toBe("flattop_long");
    expect(lengths.find((o) => o.value === "huge")!.pick).toBe("flattop_huge");
    expect(dimOptions(presetEntry("chebyshev_long_mp")!, "length", none).find((o) => o.value === "short")!.pick).toBe("chebyshev_short_mp");
  });

  it("skips presets the rate setting rules out", () => {
    const blocked = (n: FilterPreset) => presetBlockReason(n, { rate: 48000 }, [44100, 48000]);
    const cur = presetEntry("flattop_xlong")!;
    const xlong = dimOptions(cur, "shape", blocked);
    // flattop_xlong_std is integer-only: 44.1 kHz → 48 kHz is 160/147.
    const std = xlong.find((o) => o.value === "std")!;
    expect(std.pick).toBeNull();
    expect(std.tip).toContain("160/147");
    expect(familyPick("block", null, blocked)).toBeNull();
    expect(familyPick("gaussian", "gaussian_huge", blocked)).toBe("gaussian_long");
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
    expect(presetRatioNeed("cosine", { up: 160, down: 147 })).toContain("integer");
    expect(presetRatioNeed("lanczos2047", { up: 160, down: 147 })).toBeNull();
    expect(presetRatioNeed("lanczos2047", { up: 1, down: 2 })).toContain("at or above");
    expect(presetRatioNeed("halfband_kaiser", { up: 1, down: 2 })).toContain("at or above");
    expect(presetRatioNeed("gaussian_long", { up: 1, down: 2 })).toBeNull();
    expect(presetRatioNeed("gaussian_huge", { up: 160, down: 147 })).toContain("integer");
    expect(presetRatioNeed("gaussian_long", { up: 1, down: 1 })).toBeNull();
  });

  it("leaves a ratio past the term limit to the term note, not a length reason", () => {
    // 44.1 kHz -> 3072 kHz is 10240/147: refused for its terms, whatever the preset.
    expect(presetBlockReason("gaussian_long", { rate: 3072000 }, [44100, 48000])).toBeNull();
    expect(presetBlockReason("gaussian_long", { rate: 3072000 }, [48000])).toBeNull();
    expect(presetBlockReason("block_fft", { rate: 2822400 }, [48000])).toContain("integer");
  });

  it("checks integer ratios at every rate and fractional ones at the allowed rates", () => {
    expect(presetBlockReason("cascade_fir", { ratio: 8 }, [44100])).toBeNull();
    expect(presetBlockReason("block_fft", { ratio: 6 }, [44100])).toContain("×6");
    // {rate: 48000}: 96 kHz → 48 kHz decimates, but only when 96 kHz may play.
    expect(presetBlockReason("lanczos511", { rate: 48000 }, [44100, 48000])).toBeNull();
    expect(presetBlockReason("lanczos511", { rate: 48000 }, [44100, 48000, 96000])).toContain("1/2");
    // max_rate_any resolves like max_rate without a device: integer ratios.
    expect(presetBlockReason("cosine", { max_rate_any: 768000 }, [44100, 48000])).toBeNull();
    expect(presetBlockReason("gaussian_long", "off", [44100])).toBeNull();
  });

  it("summarises a preset in a few words", () => {
    expect(presetSummary("flattop_xlong")).toBe("Flat-top xlong · linear");
    expect(presetSummary("chebyshev_long_mp")).toBe("Chebyshev long · minimum");
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

  it("budgets the pipelines that can run at once", () => {
    // 44.1 → 384 kHz: 4 × 255 × 1280 grid taps, above the 1,048,576 limit.
    expect(presetBlockReason("gaussian_early_ip", { rate: 384000 }, [44100])).toContain("Too long");
    expect(presetBlockReason("gaussian_early", { rate: 384000 }, [44100])).toBeNull();
    // chebyshev_long 44.1 → 48 kHz: 207 taps per output frame at 48 kHz.
    const one = presetRationalCost("chebyshev_long", 44100, rateRatioOf(44100, 48000), 1).macs;
    expect(one).toBeCloseTo(48000 * 207, 3);
    expect(presetRationalCost("chebyshev_long", 44100, rateRatioOf(44100, 48000)).macs).toBeCloseTo(2 * one, 3);
    expect(presetRationalCost("chebyshev_long", 44100, rateRatioOf(44100, 48000), 4).macs).toBeCloseTo(4 * one, 3);
    // kaiser_steep 44.1 → 768 kHz: 577 × 2560 grid taps, over the length limit.
    expect(presetBlockReason("kaiser_steep", { rate: 768000 }, [44100])).toContain("Too long");
    // kaiser_steep 44.1 → 384 kHz: 577 per output frame × 384k = 222 M per pipeline: fine with two (no A/B), not with four (A/B).
    expect(presetBlockReason("kaiser_steep", { rate: 384000 }, [44100], 2)).toBeNull();
    expect(presetBlockReason("kaiser_steep", { rate: 384000 }, [44100], 4)).toContain("Too long");
  });

  it("describes the asymmetric cascade's stage length", () => {
    expect(presetDetail(presetEntry("cascade_fir_asym")!)).toContain("289");
    expect(presetDetail(presetEntry("cascade_fir_asym")!)).toContain("292");
  });
});
