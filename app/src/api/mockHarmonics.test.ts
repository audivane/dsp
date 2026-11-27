import { describe, expect, it } from "vitest";
import type { HarmonicsConfig } from "./types";
import { MOCK_HARMONICS_PRESETS, mockHarmonicsSpectrum } from "./mockHarmonics";

const layer = (id: string) => ({ ...MOCK_HARMONICS_PRESETS.find((p) => p.id === id)!.layer, enabled: true }) as HarmonicsConfig["layers"][number];
const WORD = { bits: 24, dither: "tpdf" as const, headroom_db: 3 };

describe("mockHarmonicsSpectrum", () => {
  it("draws the fundamental at 0 dBr and the harmonics where they land", () => {
    const c: HarmonicsConfig = { enabled: true, dc_block: true, layers: [layer("se_triode_high_mu")] };
    const s = mockHarmonicsSpectrum(c, 48000, 10000, 0, [], WORD);
    expect(s.freqs_hz).toHaveLength(2048);
    const line = s.lines[0];
    expect(line.levels_db).toHaveLength(2048);
    expect(Math.max(...line.levels_db)).toBeCloseTo(0, 0);
    const h2 = line.peaks.find((p) => p.order === 2)!;
    expect(h2.aliased).toBe(false);
    expect(h2.freq_hz).toBeCloseTo(2 * s.freq_hz, 6);
    const h3 = line.peaks.find((p) => p.order === 3)!;
    expect(h3.aliased).toBe(true);
    expect(h3.freq_hz).toBeCloseTo(48000 - 3 * s.freq_hz, 6);
    expect(line.thd_n_db).toBeGreaterThan(line.thd_db);
    expect(line.floor_db).toBeLessThan(-150);
  });

  it("runs per-layer and series traces, and refuses `after` in parallel", () => {
    const c: HarmonicsConfig = { enabled: true, dc_block: true, layers: [layer("se_triode_high_mu"), layer("diff_pair")], routing: "series" };
    const s = mockHarmonicsSpectrum(c, 96000, 1000, -6, [{ kind: "layer", layer: 1 }, { kind: "after", layer: 0 }, { kind: "stage" }], WORD);
    expect(s.lines.map((l) => l.trace.kind)).toEqual(["layer", "after", "stage"]);
    expect(() => mockHarmonicsSpectrum({ ...c, routing: "parallel" }, 96000, 1000, 0, [{ kind: "after", layer: 0 }], WORD)).toThrow();
    expect(() => mockHarmonicsSpectrum(c, 96000, 1000, 0, [{ kind: "layer", layer: 5 }], WORD)).toThrow();
  });
});
