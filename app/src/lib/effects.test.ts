import { describe, expect, it } from "vitest";
import { crossfeedLevelDb, crossfeedLoudnessDb, crossfeedPeakDb, defaultHarmonics, HARMONIC_DEFAULT_SIGNS, harmonicsPeakDb, pitchActive, pitchCents } from "./effects";
import type { ChebyshevLayer, CrossfeedConfig, HarmonicsConfig, PitchConfig } from "../api/types";

const xf = (mode: CrossfeedConfig["mode"], cutoff_hz = 700, feed_db = 4.5): CrossfeedConfig => ({
  enabled: true,
  mode,
  cutoff_hz,
  feed_db,
  width: 1,
  width_shape: "flat",
  width_corner_hz: 700,
});
const harm = (p: Partial<HarmonicsConfig>): HarmonicsConfig => ({ ...defaultHarmonics(), ...p });
const pitch = (p: Partial<PitchConfig> = {}): PitchConfig => ({ enabled: true, cents: 0, quality: "high", ...p });

describe("stereo width", () => {
  const w = (mode: CrossfeedConfig["mode"], width: number, shape: CrossfeedConfig["width_shape"] = "flat"): CrossfeedConfig => ({
    ...xf(mode, 700, 4.5),
    width,
    width_shape: shape,
  });
  it("matches the engine's peak bound, level and loudness (48 kHz)", () => {
    // From dsp-core's crossfeed::tests::width_report: [config, level, peak, loudness].
    const cases: [CrossfeedConfig, number, number, number][] = [
      [w("none", 0), 0, 0, -1.25],
      [w("none", 1.25), 0, 1.94, 0.57],
      [w("none", 1.5), 0, 3.52, 1.18],
      [w("none", 1.25, "above_bass"), 0, 3.39, 0.47],
      [w("none", 1.5, "above_bass"), 0, 5.83, 0.98],
      [w("linkwitz", 1.25), 0.21, 6.24, 0.49],
      [w("bs2b_default", 2), 0.04, 9.87, 1.95],
      [w("bs2b_default", 0, "above_bass"), 0.04, 3.98, -0.79],
    ];
    for (const [c, level, peak, loud] of cases) {
      expect(crossfeedLevelDb(c, 48000)).toBeCloseTo(level, 1);
      expect(crossfeedPeakDb(c, 48000)).toBeCloseTo(peak, 1);
      expect(crossfeedLoudnessDb(c, 48000)).toBeCloseTo(loud, 1);
    }
  });
});

describe("harmonics", () => {
  /** One custom layer with levels H2..H9 (missing = off). */
  const custom = (levels: (number | null)[], signs: ChebyshevLayer["signs"] = [...HARMONIC_DEFAULT_SIGNS]): HarmonicsConfig =>
    harm({
      layers: [{ kind: "chebyshev", enabled: true, levels_db: Array.from({ length: 8 }, (_, i) => levels[i] ?? null) as ChebyshevLayer["levels_db"], signs, drive_db: 0, amount: 1 }],
    });
  it("counts H6–H9 and the signs like the engine", () => {
    // Engine (certified bound): H2–H5 at −20 dB → +2.362 dB, H2–H9 → +4.871 dB.
    expect(harmonicsPeakDb(custom([-20, -20, -20, -20]))).toBeCloseTo(2.362, 2);
    const all = custom([-20, -20, -20, -20, -20, -20, -20, -20]);
    expect(harmonicsPeakDb(all)).toBeCloseTo(4.871, 2);
    // H3 positive instead of negative: expands, so the peak changes.
    const plus = custom([-20, -20, -20, -20, -20, -20, -20, -20], ["+", "+", "+", "+", "+", "+", "+", "+"]);
    expect(harmonicsPeakDb(plus)).not.toBeCloseTo(harmonicsPeakDb(all), 2);
  });
});

describe("pitch", () => {
  it("resolves the shift like the engine", () => {
    expect(pitchCents(pitch({ cents: -50 }))).toBe(-50);
    expect(pitchCents(pitch({ semitones: 2 }))).toBe(200);
    expect(pitchCents(pitch({ reference_hz: 440, target_hz: 432 }))).toBeCloseTo(-31.77, 2);
    expect(pitchActive(pitch())).toBe(false);
    expect(pitchActive(pitch({ cents: 10, enabled: false }))).toBe(false);
  });
});

