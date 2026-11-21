import { describe, expect, it } from "vitest";
import type { WindowKind } from "../api/types";
import {
  betaControl,
  tukeyReferenceReadout,
  tukeyReferenceTooltip,
  bandDesignTaps,
  manualRationalProblem,
  rationalLengthFix,
  maxRationalTapsPerInput,
  nyquistStopMinTapsFor,
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
    for (const { beta, name } of KAISER_BETA_PRESETS) {
      const tip = kaiserPresetTooltip(beta, name);
      expect(tip.startsWith(`${name} (β = ${beta},`)).toBe(true);
      expect(tip).toContain(String(Math.round(kaiserAttenuationDb(beta))));
      expect(tip).toContain("transition");
      expect(tip).toContain("stopband");
      expect(tip).toContain("24-bit");
      expect(tip).toContain("136 dB");
    }
    // β 24: 24 / 0.1102 + 8.7 ≈ 226.5 dB.
    expect(kaiserPresetTooltip(24, "Deepest")).toContain("≈ 226 dB stopband");
    expect(kaiserPresetTooltip(10, "Steep")).toContain("steeper transition");
    expect(kaiserPresetTooltip(10, "Steep")).toContain("shallower stopband");
    expect(kaiserPresetTooltip(20, "Deeper")).toContain("wider transition");
  });

  it("labels presets with their β, from steep to deepest", () => {
    expect(KAISER_BETA_PRESETS.map((p) => p.label)).toEqual(["β 10 · Steep", "β 14 · Standard", "β 17 · Deep", "β 20 · Deeper", "β 24 · Deepest"]);
    // Stopband depth rises monotonically with the order of the buttons.
    const depth = KAISER_BETA_PRESETS.map((p) => kaiserAttenuationDb(p.beta));
    expect(depth).toEqual([...depth].sort((a, b) => a - b));
  });
});

describe("Stop at Nyquist, as the engine checks it", () => {
  // From dsp-core `nyquist_stop_min_taps_for` (ratio, beta, Gaussian, Kaiser).
  const MIN_TAPS: [number, number, number, number][] = [
    [2, 5, 30, 21],
    [2, 10, 57, 40],
    [2, 14, 78, 55],
    [2, 20, 111, 80],
    [2, 24, 132, 105],
    [4, 5, 59, 40],
    [4, 10, 112, 79],
    [4, 14, 155, 109],
    [4, 20, 220, 158],
    [4, 24, 263, 208],
    [16, 5, 231, 157],
    [16, 10, 445, 311],
    [16, 14, 616, 433],
    [16, 20, 874, 628],
    [16, 24, 1046, 827],
    [160, 5, 2301, 1560],
    [160, 10, 4435, 3094],
    [160, 14, 6149, 4321],
    [160, 20, 8728, 6265],
    [160, 24, 10450, 8257],
    [1280, 5, 18400, 12473],
    [1280, 10, 35472, 24741],
    [1280, 14, 49185, 34556],
    [1280, 20, 69813, 50107],
    [1280, 24, 83590, 66045],
  ];

  it("has the engine's fewest taps per window", () => {
    for (const [ratio, beta, gauss, kaiser] of MIN_TAPS) {
      expect(nyquistStopMinTapsFor("gaussian", ratio, beta), `gaussian x${ratio} β${beta}`).toBe(gauss);
      expect(nyquistStopMinTapsFor("kaiser", ratio, beta), `kaiser x${ratio} β${beta}`).toBe(kaiser);
    }
  });

  const base = { target: { ratio: 4 }, taps: 64, taps_unit: "input", kaiser_beta: 14, window: "kaiser" } as const;
  it("counts input-sample lengths at the ratio, like manual_taps", () => {
    // 64 per input sample at x4 is 256 output taps: above the 109-tap minimum.
    expect(nyquistStopBlockReason(base, [44100])).toBeNull();
    expect(nyquistStopBlockReason({ ...base, taps: 27 }, [44100])).toContain("109");
    expect(nyquistStopBlockReason({ ...base, taps_unit: "output", taps: 64 }, [44100])).toContain("109");
  });

  it("uses the Gaussian window's own rule", () => {
    // x4 β14: Kaiser needs 109 taps, Gaussian 155.
    expect(nyquistStopBlockReason({ ...base, taps_unit: "output", taps: 120 }, [44100])).toBeNull();
    expect(nyquistStopBlockReason({ ...base, taps_unit: "output", taps: 120, window: "gaussian" }, [44100])).toContain("155");
  });

  it("checks a fractional ratio on its grid at the allowed rates", () => {
    // {rate: 48000} from 44.1 kHz: 147 grid samples per output frame, design ratio 160 (min 4321 at β14).
    const frac = { ...base, target: { rate: 48000 }, taps_unit: "output", taps: 29 } as const;
    expect(nyquistStopBlockReason(frac, [44100])).toContain("44.1 kHz"); // 29 × 147 = 4263 grid taps < 4321
    expect(nyquistStopBlockReason({ ...frac, taps: 30 }, [44100])).toBeNull(); // 4410
    // Not checked at a rate the policy does not allow.
    expect(nyquistStopBlockReason(frac, [48000])).toBeNull();
  });
});

describe("manual designs at a fractional ratio", () => {
  const u = { target: { rate: 48000 }, taps: 65536, taps_unit: "output", phase: "linear", phase_mix: 0.5, cutoff_mode: "fraction", auto_taps: null } as const;
  it("names the daemon's longest fitting length per input sample", () => {
    // The daemon's example: 65,536 output taps from 44.1 kHz need 9.6 M grid taps; at most 6553 per input sample.
    const p = manualRationalProblem(u, [44100, 48000], 2)!;
    expect(p.rate).toBe(44100);
    expect(p.gridTaps).toBe(65536 * 147);
    expect(p.maxPerInput).toBe(6553);
    expect(manualRationalProblem({ ...u, taps_unit: "input", taps: 6553 }, [44100, 48000], 2)).toBeNull();
    expect(manualRationalProblem({ ...u, taps_unit: "input", taps: 6554 }, [44100, 48000], 2)).not.toBeNull();
    // Mixed intermediate phase grows the kernel up to 4x.
    expect(manualRationalProblem({ ...u, taps_unit: "input", taps: 6553, phase: "intermediate" }, [44100, 48000], 2)?.maxPerInput).toBe(1638);
    // Integer ratios and automatic designs are not this check's business.
    expect(manualRationalProblem({ ...u, target: { ratio: 4 } }, [44100], 2)).toBeNull();
    expect(manualRationalProblem({ ...u, auto_taps: "match_output" }, [44100], 2)).toBeNull();
    expect(maxRationalTapsPerInput(44100, { up: 160, down: 147 }, 4, false)).toBe(4166);
  });
});

describe("the one-click length fix for a fractional rate", () => {
  const u = { target: { ratio: 8 } as const, taps: 65536, taps_unit: "output", phase: "linear", phase_mix: 0.5, cutoff_mode: "fraction", auto_taps: null, kaiser_beta: 14 } as const;
  it("counts the length in input samples and keeps it as long as fits", () => {
    // 65,536 output taps at x8 last 8,192 input samples; 160/147 takes at most 6,553.
    expect(rationalLengthFix(u, { rate: 48000 }, [44100, 48000], 2)).toEqual({ taps_unit: "input", taps: 6553 });
    // A shorter design keeps its own duration (16,384 at x8: 2,048 per input sample).
    expect(rationalLengthFix({ ...u, taps: 16384 }, { rate: 48000 }, [44100, 48000], 2)).toEqual({ taps_unit: "input", taps: 2048 });
    // A design that already fits needs no fix.
    expect(rationalLengthFix({ ...u, taps: 2048, taps_unit: "input" }, { rate: 48000 }, [44100, 48000], 2)).toBeNull();
    expect(rationalLengthFix({ ...u, taps: 65536, taps_unit: "input" }, { rate: 48000 }, [44100, 48000], 4)).toEqual({ taps_unit: "input", taps: 4166 });
    // Every allowed rate must fit, not just the first one that does not.
    const fix = rationalLengthFix({ ...u, phase: "intermediate" }, { rate: 48000 }, [44100, 48000, 88200], 2) as { taps_unit: "input"; taps: number };
    expect(manualRationalProblem({ ...u, phase: "intermediate", target: { rate: 48000 }, ...fix }, [44100, 48000, 88200], 2)).toBeNull();
    expect(fix.taps).toBe(1638);
  });
});

describe("the length fix with Stop at Nyquist", () => {
  // Custom x32, 2,048 output taps (64 per input sample), Kaiser beta 24, Stop at Nyquist.
  const u = { target: { ratio: 32 } as const, taps: 2048, taps_unit: "output", phase: "linear", phase_mix: 0.5, cutoff_mode: "nyquist_stop", auto_taps: null, kaiser_beta: 24, window: "kaiser" } as const;
  const rates = [44100, 48000, 88200, 96000, 176400, 192000];
  it("picks a length that also meets the window's Nyquist minimum at every rate", () => {
    // 2,048 output taps do not fit every rate down to 44.1 kHz; 64 per input sample would.
    expect(manualRationalProblem({ ...u, target: { rate: 44100 } }, rates, 4)).not.toBeNull();
    const fix = rationalLengthFix(u, { rate: 44100 }, rates, 4);
    expect(fix && "taps" in fix).toBe(true);
    const fixed = { ...u, target: { rate: 44100 }, ...(fix as { taps_unit: "input"; taps: number }) };
    // 88.2 -> 44.1 kHz alone needs more than 64 per input sample.
    expect(nyquistStopBlockReason({ ...fixed, taps: 64 }, rates)).not.toBeNull();
    expect(nyquistStopBlockReason(fixed, rates)).toBeNull();
    expect(manualRationalProblem(fixed, rates, 4)).toBeNull();
  });
  it("offers no length when none meets both, and says why", () => {
    // So many pipelines that the cost limit sits below the Nyquist minimum.
    const fix = rationalLengthFix(u, { rate: 44100 }, rates, 400);
    expect(fix && "why" in fix ? fix.why : "").toMatch(/Stop at Nyquist/);
    expect(fix && "why" in fix ? fix.why : "").toMatch(/allowed rates/);
  });
});

describe("band design length, as the engine computes it", () => {
  // dsp-core `band_design_for` (window, passband end, stopband start, dB, design ratio) -> taps.
  const CASES: [WindowKind, number, number, number, number, number][] = [
    ["kaiser", 0.9, 1.0, 120, 160, 30721],
    ["kaiser", 0.99, 1.0, 200, 1280, 4268613],
    ["kaiser", 0.95, 1.05, 150, 8, 1949],
    ["gaussian", 0.9, 1.0, 120, 160, 54042],
    ["gaussian", 0.95, 1.05, 150, 8, 3413],
  ];
  it.each(CASES)("%s %f..%f at %f dB, ratio %i", (window, passband_end, stopband_start, attenuation_db, ratio, taps) => {
    expect(bandDesignTaps(window, { passband_end, stopband_start, attenuation_db }, ratio)).toBe(taps);
  });
});

describe("fractional limits past the term limit", () => {
  it("are left to the term note", () => {
    const u = { target: { rate: 3072000 }, taps: 4194304, taps_unit: "output", phase: "linear", phase_mix: 0.5, cutoff_mode: "fraction", auto_taps: null } as const;
    expect(manualRationalProblem(u, [44100], 2)).toBeNull();
    // 48 kHz -> 1411.2 kHz (147/5) is within the terms and still checked.
    expect(manualRationalProblem({ ...u, target: { rate: 1411200 } }, [48000], 2)?.text).toContain("147/5");
  });
});

describe("the β control's label follows the window", () => {
  it("names what β does with each window", () => {
    expect(betaControl("kaiser").label).toBe("Filter shape (Kaiser β)");
    expect(betaControl("gaussian").label).toBe("Gaussian width (β)");
    expect(betaControl("tukey").label).toBe("Stopband reference (β)");
    expect(betaControl("tukey").hint).toBeNull();
    for (const w of ["gaussian", "tukey"] as WindowKind[]) expect(betaControl(w).label).not.toMatch(/Kaiser/);
    for (const w of ["kaiser", "gaussian", "tukey"] as WindowKind[]) expect(betaControl(w).title.length).toBeGreaterThan(20);
  });
});

describe("β with the Tukey window is only a measurement reference", () => {
  it("promises no change of filter shape", () => {
    const tip = tukeyReferenceTooltip(17);
    expect(tip).toContain("163 dB");
    expect(tip).toMatch(/does not change/);
    expect(tip).not.toMatch(/transition|steeper|deeper/);
    expect(tukeyReferenceReadout(17)).toBe("Stopband edge and depth are measured against ≈ 163.0 dB.");
    expect(tukeyReferenceReadout(14)).not.toMatch(/transition|Designed/);
  });
});
