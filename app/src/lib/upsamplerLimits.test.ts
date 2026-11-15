import { describe, expect, it } from "vitest";
import {
  KAISER_BETA_PRESETS,
  NYQUIST_STOP_MIN_TAPS_CASES,
  kaiserAttenuationDb,
  kaiserBetaBlockReason,
  kaiserPresetTooltip,
  kaiserTransitionWidthHz,
  minimumPhaseBlockReason,
  nyquistStopBlockReason,
  nyquistStopMinTaps,
  selectedKaiserPreset,
  worstCaseRatio,
} from "./upsamplerLimits";

describe("upsampler limits (mirror of dsp-core)", () => {
  it("matches the Rust minimum taps", () => {
    for (const [ratio, beta, taps] of NYQUIST_STOP_MIN_TAPS_CASES) {
      expect(nyquistStopMinTaps(ratio, beta), `x${ratio} beta ${beta}`).toBe(taps);
    }
    expect(nyquistStopMinTaps(4, 30)).toBeNull();
    expect(nyquistStopMinTaps(1, 14)).toBe(1);
  });

  it("uses the worst-case ratio over source rates", () => {
    expect(worstCaseRatio("off")).toBe(1);
    expect(worstCaseRatio({ ratio: 8 })).toBe(8);
    expect(worstCaseRatio({ max_rate: 768000 })).toBe(16);
    expect(worstCaseRatio({ max_rate: 384000 })).toBe(8);
  });

  it("explains blocked options", () => {
    expect(nyquistStopBlockReason({ target: { ratio: 16 }, taps: 16384, kaiser_beta: 14 })).toBeNull();
    expect(nyquistStopBlockReason({ target: "off", taps: 64, kaiser_beta: 14 })).toBeNull();
    // Upsampling off: the daemon accepts any β, so the UI must not block it.
    expect(nyquistStopBlockReason({ target: "off", taps: 64, kaiser_beta: 30 })).toBeNull();
    const short = nyquistStopBlockReason({ target: { ratio: 16 }, taps: 64, kaiser_beta: 20 });
    expect(short).toContain("only guaranteed from 628 taps");
    expect(short).toContain("×16");
    expect(nyquistStopBlockReason({ target: { ratio: 2 }, taps: 65536, kaiser_beta: 30 })).toContain("between 5 and 24");
    expect(minimumPhaseBlockReason(14)).toBeNull();
    expect(minimumPhaseBlockReason(1)).toContain("between 2 and 20");
    expect(minimumPhaseBlockReason(22)).toContain("between 2 and 20");
  });

  it("identifies preset vs custom Kaiser beta values", () => {
    expect(KAISER_BETA_PRESETS.map((p) => p.beta)).toEqual([10, 14, 17, 20, 24]);
    expect(selectedKaiserPreset(24)).toBe(24);
    expect(selectedKaiserPreset(10)).toBe(10);
    expect(selectedKaiserPreset(14)).toBe(14);
    expect(selectedKaiserPreset(17)).toBe(17);
    expect(selectedKaiserPreset(20)).toBe(20);
    expect(selectedKaiserPreset(12)).toBeNull();
    expect(selectedKaiserPreset(14.5)).toBeNull();
    expect(selectedKaiserPreset(0)).toBeNull();
    expect(selectedKaiserPreset(30)).toBeNull();
    expect(selectedKaiserPreset(Number.NaN)).toBeNull();
  });

  it("computes Kaiser beta disabled reasons for phase and cutoff modes", () => {
    const base = { target: { ratio: 16 } as const, taps: 512, phase: "linear" as const, cutoff_mode: "nyquist_stop" as const };
    // At x16 and 512 taps, Stop at Nyquist allows beta 10 (min 307) and 14 (min 433),
    // but blocks beta 17 and 20 (min 628).
    expect(kaiserBetaBlockReason(base, 10)).toBeNull();
    expect(kaiserBetaBlockReason(base, 14)).toBeNull();
    expect(kaiserBetaBlockReason(base, 17)).toContain("only guaranteed from");
    expect(kaiserBetaBlockReason(base, 20)).toContain("only guaranteed from 628 taps");

    // In fraction cutoff mode, taps do not restrict beta.
    expect(kaiserBetaBlockReason({ ...base, cutoff_mode: "fraction" }, 20)).toBeNull();

    // Minimum phase blocks beta outside 2..20: presets 10..20 are fine, 24 is not.
    const minPhase = { target: { ratio: 4 } as const, taps: 16384, phase: "minimum" as const, cutoff_mode: "fraction" as const };
    for (const { beta } of KAISER_BETA_PRESETS.filter((p) => p.beta <= 20)) {
      expect(kaiserBetaBlockReason(minPhase, beta)).toBeNull();
    }
    expect(kaiserBetaBlockReason(minPhase, 24)).toContain("between 2 and 20");
    // β 24 is fine for linear phase, also with Stop at Nyquist (range 5..24).
    expect(kaiserBetaBlockReason({ ...minPhase, phase: "linear" }, 24)).toBeNull();
    expect(kaiserBetaBlockReason({ ...minPhase, phase: "linear", cutoff_mode: "nyquist_stop" }, 24)).toBeNull();
    expect(kaiserBetaBlockReason(minPhase, 1)).toContain("between 2 and 20");
    expect(kaiserBetaBlockReason(minPhase, 22)).toContain("between 2 and 20");

    // When both minimum phase and nyquist_stop fail, both reasons are included.
    const both = kaiserBetaBlockReason({ target: { ratio: 4 }, taps: 64, phase: "minimum", cutoff_mode: "nyquist_stop" }, 30);
    expect(both).toContain("between 2 and 20");
    expect(both).toContain("between 5 and 24");
  });

  it("computes Kaiser transition width Δf = (A - 7.95) / (14.36 * (N - 1)) * f_out", () => {
    const a14 = kaiserAttenuationDb(14);
    expect(a14).toBeCloseTo(135.741742, 4);
    const df14 = kaiserTransitionWidthHz(14, 65536, 176400);
    expect(df14).toBeCloseTo(((a14 - 7.95) / (14.36 * 65535)) * 176400, 8);
    expect(df14!).toBeCloseTo(23.952, 2);

    const df10 = kaiserTransitionWidthHz(10, 65536, 176400)!;
    const df17 = kaiserTransitionWidthHz(17, 65536, 176400)!;
    const df20 = kaiserTransitionWidthHz(20, 65536, 176400)!;
    expect(df10).toBeLessThan(df14!);
    expect(df14!).toBeLessThan(df17);
    expect(df17).toBeLessThan(df20);

    // Edge cases
    expect(kaiserTransitionWidthHz(14, 1, 176400)).toBeNull();
    expect(kaiserTransitionWidthHz(14, 65536, 0)).toBeNull();
    expect(kaiserTransitionWidthHz(Number.NaN, 65536, 176400)).toBeNull();
  });

  it("builds tooltips using computed attenuation and audibility note", () => {
    for (const { beta, label } of KAISER_BETA_PRESETS) {
      const tip = kaiserPresetTooltip(beta, label);
      expect(tip).toContain(String(Math.round(kaiserAttenuationDb(beta))));
      expect(tip).toContain("transition");
      expect(tip).toContain("stopband");
      expect(tip).toContain("24-bit");
      expect(tip).toContain("136 dB");
    }
    // β 24: 24 / 0.1102 + 8.7 ≈ 226.5 dB.
    expect(kaiserPresetTooltip(24, "Extreme")).toContain("≈ 226 dB stopband");
  });
});
