import { describe, expect, it } from "vitest";
import {
  crossfeedAsRun,
  crossfeedLevel,
  withCrossfeedLevel,
  crossfeedLevelDb,
  crossfeedPeakDb,
  crossfeedResponse,
  crossfeedSummary,
  dcBlockDb,
  dcBlockSummary,
  ditherRateWarning,
  effectivePitchQuality,
  fmtCents,
  harmonicsLevelDb,
  harmonicsSummary,
  harmonicsThdPercent,
  limiterDelay,
  limiterEventLabel,
  limiterMeterState,
  limiterSummary,
  pitchActive,
  pitchCents,
  pitchPageOn,
  pitchPageSummary,
  pitchSummary,
  pitchIdle,
  pitchWithCents,
  pitchWithTuning,
  stagesOn,
} from "./effects";
import type { ChainConfig, CrossfeedConfig, HarmonicsConfig, PitchConfig } from "../api/types";

const xf = (mode: CrossfeedConfig["mode"], cutoff_hz = 700, feed_db = 4.5, level: CrossfeedConfig["level"] = "peak_safe"): CrossfeedConfig => ({
  enabled: true,
  mode,
  cutoff_hz,
  feed_db,
  level,
});
const harm = (p: Partial<HarmonicsConfig>): HarmonicsConfig => ({ enabled: true, character: "triode", h2_db: -40, h3_db: -58, h4_db: null, h5_db: null, dc_block: true, ...p });
const pitch = (p: Partial<PitchConfig> = {}): PitchConfig => ({ enabled: true, cents: 0, quality: "high", ...p });

describe("DC block", () => {
  it("is inaudible at the default cutoff and −3 dB at the corner", () => {
    expect(dcBlockDb(5, 20)).toBeCloseTo(-0.017, 2);
    expect(dcBlockDb(5, 5)).toBeCloseTo(-3.01, 1);
    expect(dcBlockSummary(null)).toBe("Off");
    expect(dcBlockSummary({ enabled: false, cutoff_hz: 5 })).toBe("Off");
    expect(dcBlockSummary({ enabled: true, cutoff_hz: 7.5 })).toBe("7.5 Hz high-pass");
  });
});

describe("crossfeed", () => {
  it("peak-safe applies the engine's worst-case gain: −2.9 to −4.4 dB for the presets", () => {
    for (const mode of ["bs2b_default", "bs2b_chu_moy", "bs2b_jan_meier", "linkwitz"] as const) {
      expect(crossfeedPeakDb(xf(mode), 48000)).toBeCloseTo(0, 9);
      const h = crossfeedLevelDb(xf(mode), 48000);
      expect(h).toBeLessThanOrEqual(-2.85);
      expect(h).toBeGreaterThanOrEqual(-4.45);
    }
  });
  it("stays within −1.4 to −5.6 dB across custom settings", () => {
    for (const c of [300, 700, 2000]) {
      for (const f of [1, 4.5, 15]) {
        const h = crossfeedLevelDb(xf("bs2b_custom", c, f), 48000);
        expect(h).toBeLessThanOrEqual(-1.35);
        expect(h).toBeGreaterThanOrEqual(-5.65);
      }
    }
  });
  it("matched keeps the loudness: the engine's gains and worst-case peaks (44.1 kHz)", () => {
    // From dsp-core's crossfeed::tests::level_report.
    const cases: [CrossfeedConfig, number, number][] = [
      [xf("bs2b_default", 700, 4.5, "matched"), 0.04, 4.27],
      [xf("bs2b_chu_moy", 700, 4.5, "matched"), 0.06, 3.87],
      [xf("bs2b_jan_meier", 700, 4.5, "matched"), 0.07, 2.98],
      [xf("linkwitz", 700, 4.5, "matched"), 0.21, 4.47],
      [xf("bs2b_custom", 300, 1, "matched"), 0.01, 5.53],
      [xf("bs2b_custom", 2000, 1, "matched"), -0.48, 4.26],
      [xf("bs2b_custom", 2000, 15, "matched"), -0.09, 1.35],
    ];
    for (const [c, level, peak] of cases) {
      expect(crossfeedLevelDb(c, 44100)).toBeCloseTo(level, 1);
      expect(crossfeedPeakDb(c, 44100)).toBeCloseTo(peak, 1);
    }
  });
  it("a config from an engine without `level` is shown and plotted as peak-safe", () => {
    const { level: _level, ...legacy } = xf("bs2b_default", 700, 4.5, "matched");
    const old = legacy as CrossfeedConfig;
    expect(crossfeedLevel(old)).toBe("peak_safe");
    expect(crossfeedLevelDb(old, 44100)).toBeCloseTo(crossfeedLevelDb(xf("bs2b_default"), 44100), 9);
    expect(crossfeedLevelDb(old, 44100)).toBeCloseTo(-4.23, 2);
    expect(crossfeedResponse(old, 44100, [10])).toEqual(crossfeedResponse(xf("bs2b_default"), 44100, [10]));
    expect(crossfeedSummary(old)).toBe("Default · 700 Hz, 4.5 dB · peak-safe");
    // An engine known to lack it runs peak-safe even under a matched draft;
    // an unknown or capable engine runs what the config says.
    const draft = xf("bs2b_default", 700, 4.5, "matched");
    expect(crossfeedLevel(crossfeedAsRun(draft, false))).toBe("peak_safe");
    expect(crossfeedLevel(crossfeedAsRun(draft, null))).toBe("matched");
    expect(crossfeedLevel(crossfeedAsRun(draft, true))).toBe("matched");
  });
  it("choosing a level keeps the power state", () => {
    const fromNull = withCrossfeedLevel(null, "peak_safe");
    expect(fromNull.enabled).toBe(false);
    expect(fromNull.level).toBe("peak_safe");
    const off = { ...xf("bs2b_jan_meier"), enabled: false };
    expect(withCrossfeedLevel(off, "matched")).toEqual({ ...off, level: "matched" });
    expect(withCrossfeedLevel(xf("linkwitz"), "matched")).toEqual({ ...xf("linkwitz"), level: "matched" });
  });
  it("keeps the feed level between direct and cross at low frequencies", () => {
    for (const mode of ["bs2b_default", "bs2b_jan_meier", "linkwitz"] as const) {
      const r = crossfeedResponse(xf(mode), 48000, [10]);
      const feed = mode === "bs2b_jan_meier" ? 9.5 : mode === "linkwitz" ? 6 : 4.5;
      expect(r.direct[0] - r.cross[0]).toBeCloseTo(feed, 1);
    }
  });
  it("summarises the mode and its effective parameters", () => {
    expect(crossfeedSummary(null)).toBe("Off");
    expect(crossfeedSummary(xf("bs2b_chu_moy"))).toBe("Chu Moy · 700 Hz, 6 dB · peak-safe");
    expect(crossfeedSummary(xf("bs2b_custom", 650, 9))).toBe("Custom · 650 Hz, 9 dB · peak-safe");
    expect(crossfeedSummary({ ...xf("linkwitz"), enabled: false })).toBe("Off");
    expect(crossfeedSummary(xf("bs2b_chu_moy", 700, 4.5, "matched"))).toBe("Chu Moy · 700 Hz, 6 dB");
  });
});

describe("harmonics", () => {
  it("THD at full scale is the root sum of the harmonic levels", () => {
    // Triode: −40, −58, −72 dB.
    const triode = Math.sqrt(1e-4 + 10 ** (-5.8) + 10 ** (-7.2)) * 100;
    expect(harmonicsThdPercent(harm({}))).toBeCloseTo(triode, 6);
    expect(harmonicsThdPercent(harm({ character: "custom", h2_db: -20, h3_db: -120 }))).toBeCloseTo(10, 3);
    expect(harmonicsSummary(harm({}))).toBe("Triode · THD 1.01%");
    expect(harmonicsSummary(null)).toBe("Off");
  });
  it("never raises the level", () => {
    expect(harmonicsLevelDb(harm({}))).toBeLessThanOrEqual(0);
    expect(harmonicsLevelDb(harm({ character: "custom", h2_db: -20, h3_db: -20, h4_db: -20, h5_db: -20 }))).toBeLessThan(-0.5);
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
  it("writes one way of stating the shift at a time", () => {
    const t = pitchWithTuning(pitch({ cents: 30 }), 440, 432);
    expect(t).toMatchObject({ cents: 0, semitones: null, reference_hz: 440, target_hz: 432, enabled: true });
    const c = pitchWithCents(t, 1500);
    expect(c).toMatchObject({ cents: 1200, reference_hz: null, target_hz: null });
  });
  it("formats cents with semitones", () => {
    expect(fmtCents(0)).toBe("0 ¢");
    expect(fmtCents(-50)).toBe("−50 ¢");
    expect(fmtCents(314)).toBe("+3 st +14 ¢");
    expect(fmtCents(-1200)).toBe("−12 st");
    expect(pitchSummary(pitch({ cents: 100, quality: "max" }))).toBe("+1 st · max");
    expect(pitchSummary(pitch({ reference_hz: 440, target_hz: 432 }))).toBe("440 → 432 Hz · high");
    expect(pitchSummary(pitch())).toBe("On · no shift");
    expect(pitchSummary(pitch({ enabled: false }))).toBe("Off");
    expect(pitchSummary(pitch({ enabled: false, cents: 30 }))).toBe("Off");
  });
  it("sums up the Pitch page from pitch and varispeed", () => {
    const vs = (enabled: boolean, speed = 1) => ({ enabled, speed, ramp_ms: 200, keep_pitch: false });
    expect(pitchPageOn(pitch({ enabled: false }), vs(true))).toBe(true);
    expect(pitchPageOn(pitch({ enabled: false }), vs(false))).toBe(false);
    expect(pitchPageSummary(pitch({ enabled: false }), vs(false, 1.1))).toBe("Off");
    expect(pitchPageSummary(pitch(), vs(true))).toBe("On · no shift");
    expect(pitchPageSummary(pitch({ cents: 100 }), vs(true, 1.02))).toBe("+1 st · high · speed +2.0 %");
    expect(pitchPageSummary(pitch({ enabled: false }), vs(true, 0.95))).toBe("Speed −5.0 %");
  });
  it("steps quality down at high rates", () => {
    expect(effectivePitchQuality("max", 96000)).toBe("max");
    expect(effectivePitchQuality("max", 192000)).toBe("high");
    expect(effectivePitchQuality("max", 384000)).toBe("low");
    expect(effectivePitchQuality("high", 192000)).toBe("high");
  });
});

describe("limiter", () => {
  it("adds whole frames of look-ahead", () => {
    expect(limiterDelay(5, 44100).frames).toBe(221);
    expect(limiterDelay(5, 48000)).toEqual({ frames: 240, ms: 5 });
  });
  it("names the events per mode", () => {
    expect(limiterEventLabel("limit")).toBe("onsets");
    expect(limiterEventLabel("auto_headroom")).toBe("headroom steps");
    expect(limiterSummary({ mode: "limit", ceiling_dbfs: -0.5, lookahead_ms: 5, release_ms: 100 })).toBe("Limit · −0.5 dBTP");
    expect(limiterSummary({ mode: "off", ceiling_dbfs: -0.5, lookahead_ms: 5, release_ms: 100 })).toBe("Off");
    expect(limiterSummary({ mode: "auto_headroom", ceiling_dbfs: -0.5, lookahead_ms: 5, release_ms: 100 }, 3)).toBe("Auto headroom · −0.5 dBTP · −3 dB");
    expect(limiterSummary({ mode: "off", ceiling_dbfs: -0.5, lookahead_ms: 5, release_ms: 100 }, 2.5)).toBe("Off · −2.5 dB");
  });
  it("reports the gain-reduction meter state for playing, reducing and inactive paths", () => {
    expect(limiterMeterState(undefined, "playing")).toEqual({ inPath: false, text: "No reading yet" });
    expect(limiterMeterState({ active: true, headroom_db: 0, gain_reduction_db: 0, events: 0 }, "playing")).toEqual({
      inPath: true,
      text: "Playing, below the ceiling: no reduction",
    });
    expect(limiterMeterState({ active: true, headroom_db: 0, gain_reduction_db: 1.8, events: 3 }, "playing")).toEqual({
      inPath: true,
      text: "Reducing",
    });
    expect(limiterMeterState({ active: false, headroom_db: 0, gain_reduction_db: 0, events: 2 }, "playing")).toEqual({
      inPath: false,
      text: "Not in the path (Direct, bypass or nothing playing)",
    });
    // Stale `active: true` after playback stops still reports not in the path.
    expect(limiterMeterState({ active: true, headroom_db: 0, gain_reduction_db: 0, events: 2 }, "ready")).toEqual({
      inPath: false,
      text: "Not in the path (Direct, bypass or nothing playing)",
    });
    expect(limiterMeterState({ active: true, headroom_db: 0, gain_reduction_db: 0, events: 2 }, "idle")).toEqual({
      inPath: false,
      text: "Not in the path (Direct, bypass or nothing playing)",
    });
  });
});

describe("dither", () => {
  it("warns when a shaper runs below its rate", () => {
    expect(ditherRateWarning("tpdf", 44100)).toBeNull();
    expect(ditherRateWarning("feedback5", 176400)).toBeNull();
    expect(ditherRateWarning("feedback5", 48000)).toContain("88.2 kHz");
    expect(ditherRateWarning("integrator15", 176400)).toContain("352.8 kHz");
  });
});

it("reports which stages are on", () => {
  const chain = {
    dc_block: { enabled: true, cutoff_hz: 5 },
    pitch: pitch({ cents: 0 }),
    crossfeed: null,
    harmonics: harm({ enabled: false }),
    limiter: { mode: "auto_headroom", ceiling_dbfs: -1, lookahead_ms: 5, release_ms: 100 },
  } as unknown as ChainConfig;
  // Pitch on at 0 cents shows on (a no-op the engine skips), off shows off.
  expect(stagesOn(chain)).toEqual({ dc_block: true, pitch: true, crossfeed: false, harmonics: false, limiter: true });
  expect(stagesOn({ ...chain, pitch: pitch({ enabled: false, cents: 50 }) }).pitch).toBe(false);
});

it("pitch on at 0 cents is idle, not active", () => {
  expect(pitchIdle(pitch())).toBe(true);
  expect(pitchActive(pitch())).toBe(false);
  expect(pitchIdle(pitch({ cents: 5 }))).toBe(false);
  expect(pitchIdle(pitch({ enabled: false }))).toBe(false);
  // Reaching 0 keeps the stage on.
  expect(pitchWithCents(pitch({ cents: 40 }), 0)).toMatchObject({ enabled: true, cents: 0 });
});

describe("review fixes", () => {
  it("switching the pitch editor keeps the shift and the on/off state", async () => {
    const { pitchAsCents, pitchAsTuning } = await import("./effects");
    const on = pitch({ cents: 100 });
    const t = pitchAsTuning(on);
    expect(pitchCents(t)).toBeCloseTo(100, 6);
    expect(t.enabled).toBe(true);
    expect(t.cents).toBe(0);
    const off = pitchAsTuning(pitch({ cents: 100, enabled: false }));
    expect(off.enabled).toBe(false);
    expect(pitchCents(off)).toBeCloseTo(100, 6);
    const back = pitchAsCents({ ...pitchWithTuning(pitch(), 440, 432), enabled: false });
    expect(back.enabled).toBe(false);
    expect(back.reference_hz).toBeNull();
    expect(pitchCents(back)).toBeCloseTo(pitchCents(pitchWithTuning(pitch(), 440, 432)), 9);
    // Zero stays zero.
    expect(pitchCents(pitchAsTuning(pitch()))).toBe(0);
  });

  it("keeps a real rolling maximum", async () => {
    const { RollingMax } = await import("./effects");
    const r = new RollingMax(3000);
    r.push(0, 5);
    r.push(1000, 2);
    r.push(2000, 1);
    expect(r.max(2500)).toBe(5);
    // The 5 dB peak expired; the newer 2 dB one is still in the window.
    r.push(3100, 0);
    expect(r.max(3100)).toBe(2);
    r.push(4500, 0);
    expect(r.max(4500)).toBe(1);
    expect(r.max(9000)).toBe(0);
  });
});
