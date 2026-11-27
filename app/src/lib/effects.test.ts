import { describe, expect, it } from "vitest";
import {
  firstAliasedFreq,
  harmonicsSeries,
  moveLayer,
  movedIndex,
  crossfeedLevelDb,
  crossfeedLoudnessDb,
  crossfeedPeakDb,
  stereoWidthLabel,
  stereoWidthSideDb,
  crossfeedResponse,
  crossfeedSummary,
  dcBlockDb,
  dcBlockSummary,
  ditherRateWarning,
  effectivePitchQuality,
  fmtCents,
  defaultHarmonics,
  HARMONIC_DEFAULT_SIGNS,
  layerFromPreset,
  layerModified,
  layerDiffers,
  stageModified,
  stageSystem,
  systemLayers,
  configFromSystem,
  systemChain,
  layerName,
  presetGroups,
  resetLayer,
  sweepCurves,
  withOrderLevel,
  withOrderSign,
  defaultTilt,
  freqCurves,
  orderShelves,
  tiltFlat,
  withOrderTilt,
  withShelfSlope,
  withTilt,
  defaultBias,
  layerDynamic,
  withBias,
  withSag,
  harmonicsPeakDb,
  harmonicsSummary,
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
import { MOCK_HARMONICS_PRESETS, MOCK_HARMONICS_SYSTEMS, mockHarmonicsReadout } from "../api/mockHarmonics";
import type { ChainConfig, ChebyshevLayer, CrossfeedConfig, HarmonicsConfig, HarmonicsLayer, PitchConfig } from "../api/types";

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
const preset = (id: string) => MOCK_HARMONICS_PRESETS.find((p) => p.id === id)!;
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
  it("keeps the loudness: the engine's gains and worst-case peaks (44.1 kHz)", () => {
    // From dsp-core's crossfeed::tests::level_report.
    const cases: [CrossfeedConfig, number, number][] = [
      [xf("bs2b_default", 700, 4.5), 0.04, 4.27],
      [xf("bs2b_chu_moy", 700, 4.5), 0.06, 3.87],
      [xf("bs2b_jan_meier", 700, 4.5), 0.07, 2.98],
      [xf("linkwitz", 700, 4.5), 0.21, 4.47],
      [xf("bs2b_custom", 300, 1), 0.01, 5.53],
      [xf("bs2b_custom", 2000, 1), -0.48, 4.26],
      [xf("bs2b_custom", 2000, 15), -0.09, 1.35],
    ];
    for (const [c, level, peak] of cases) {
      expect(crossfeedLevelDb(c, 44100)).toBeCloseTo(level, 1);
      expect(crossfeedPeakDb(c, 44100)).toBeCloseTo(peak, 1);
    }
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
    expect(crossfeedSummary(xf("bs2b_chu_moy"))).toBe("Chu Moy · 700 Hz, 6 dB");
    expect(crossfeedSummary(xf("bs2b_custom", 650, 9))).toBe("Custom · 650 Hz, 9 dB");
    expect(crossfeedSummary({ ...xf("linkwitz"), enabled: false })).toBe("Off");
  });
});

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
  it("width 1 leaves the crossfeed as it was", () => {
    for (const shape of ["flat", "above_bass"] as const) {
      const c = w("bs2b_default", 1, shape);
      expect(crossfeedPeakDb(c, 44100)).toBeCloseTo(4.27, 2);
      expect(crossfeedResponse(c, 44100, [10, 5000])).toEqual(crossfeedResponse(xf("bs2b_default", 700, 4.5), 44100, [10, 5000]));
    }
  });
  it("plots a hard-panned source: +1.9 dB own ear, −12 dB other ear at +2", () => {
    const r = crossfeedResponse(w("none", 1.5), 48000, [100, 5000]);
    expect(r.direct[0]).toBeCloseTo(1.94, 2);
    expect(r.cross[1]).toBeCloseTo(-12.04, 2);
    // Above the bass: the bass keeps its width.
    const b = crossfeedResponse(w("none", 1.5, "above_bass"), 48000, [20, 20000]);
    expect(b.direct[0]).toBeCloseTo(0, 1);
    expect(b.direct[1]).toBeCloseTo(1.94, 1);
  });
  it("names the steps and the side gain", () => {
    expect(stereoWidthLabel(0)).toBe("Mono");
    expect(stereoWidthLabel(1.25)).toBe("+1");
    expect(stereoWidthLabel(1.3)).toBe("1.30");
    expect(stereoWidthSideDb(1.5)).toBe("+3.5 dB");
    expect(stereoWidthSideDb(0.5)).toBe("−6.0 dB");
    expect(stereoWidthSideDb(0)).toBe("−∞ dB");
    expect(crossfeedSummary(w("none", 1.5))).toBe("Width +2");
    expect(crossfeedSummary(w("none", 1))).toBe("No crossfeed, width 0");
    expect(crossfeedSummary(w("linkwitz", 0, "above_bass"))).toBe("Linkwitz · 796 Hz, 6 dB · Mono above bass");
    expect(crossfeedSummary(w("bs2b_chu_moy", 1.25, "flat"))).toBe("Chu Moy · 700 Hz, 6 dB · Width +1");
  });
});

describe("harmonics", () => {
  /** One custom layer with levels H2..H9 (missing = off). */
  const custom = (levels: (number | null)[], signs: ChebyshevLayer["signs"] = [...HARMONIC_DEFAULT_SIGNS]): HarmonicsConfig =>
    harm({
      layers: [{ kind: "chebyshev", enabled: true, levels_db: Array.from({ length: 8 }, (_, i) => levels[i] ?? null) as ChebyshevLayer["levels_db"], signs, drive_db: 0, amount: 1 }],
    });
  it("moves a layer and follows the open card", () => {
    const three = harm({});
    const ids = (c: HarmonicsConfig) => c.layers.map((l) => (l.kind === "chebyshev" ? l.levels_db[0] : l.kind));
    const c = { ...three, layers: [custom([-30]).layers[0], custom([-40]).layers[0], custom([-50]).layers[0]] };
    expect(ids(moveLayer(c, 0, 2))).toEqual([-40, -50, -30]);
    expect(ids(moveLayer(c, 2, 0))).toEqual([-50, -30, -40]);
    expect(moveLayer(c, 1, 1)).toBe(c);
    expect(moveLayer(c, 0, 3)).toBe(c);
    expect(moveLayer(c, -1, 0)).toBe(c);
    // The open card goes with its layer, or shifts past the moved one.
    expect(movedIndex(0, 0, 2)).toBe(2);
    expect(movedIndex(1, 0, 2)).toBe(0);
    expect(movedIndex(1, 2, 0)).toBe(2);
    expect(movedIndex(2, 0, 1)).toBe(2);
    expect(movedIndex(null, 0, 1)).toBeNull();
    expect(harmonicsSeries(c)).toBe(false);
    expect(harmonicsSeries({ ...c, routing: "series" })).toBe(true);
    expect(harmonicsSeries(null)).toBe(false);
  });
  it("summarises the layers that are on", () => {
    expect(harmonicsSummary(harm({}))).toBe("se_triode_high_mu");
    expect(harmonicsSummary(harm({}), MOCK_HARMONICS_PRESETS)).toBe("Single-ended triode, high-mu");
    expect(harmonicsSummary(null)).toBe("Off");
    const three = harm({ layers: [...harm({}).layers, layerFromPreset(preset("tape")), custom([-30]).layers[0]] });
    expect(harmonicsSummary(three, MOCK_HARMONICS_PRESETS)).toBe("Single-ended triode, high-mu + 2 more");
    expect(harmonicsSummary(harm({ layers: [three.layers[1], three.layers[2]] }), MOCK_HARMONICS_PRESETS)).toBe("Tape, hot + Custom Chebyshev");
    expect(harmonicsSummary(harm({ layers: [{ ...three.layers[0], enabled: false }] }))).toBe("No layers on");
  });
  it("reports a peak gain and never cuts for it", () => {
    expect(harmonicsPeakDb(harm({}))).toBeGreaterThanOrEqual(0);
    expect(harmonicsPeakDb(custom([-20, -20, -20, -20]))).toBeGreaterThan(0.5);
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
  it("marks a layer modified against its preset and resets it", () => {
    const presets = MOCK_HARMONICS_PRESETS;
    const tri = layerFromPreset(preset("triode"));
    expect(tri.preset).toBe("triode");
    expect(tri.enabled).toBe(true);
    expect(layerModified(tri, presets)).toBe(false);
    // Turning a layer off is not a modification.
    expect(layerModified({ ...tri, enabled: false }, presets)).toBe(false);
    const c = withOrderLevel(harm({ layers: [tri] }), 0, 4, -80);
    const edited = c.layers[0];
    expect(edited.kind === "chebyshev" && edited.levels_db).toEqual([-40, -58, -72, null, -80, null, null, null]);
    expect(layerModified(edited, presets)).toBe(true);
    const signed = withOrderSign(harm({ layers: [tri] }), 0, 1, "+").layers[0];
    expect(layerModified(signed, presets)).toBe(true);
    expect(layerModified({ ...tri, invert_even: true } as typeof tri, presets)).toBe(true);
    expect(layerModified({ ...tri, invert_even: false } as typeof tri, presets)).toBe(false);
    const reset = resetLayer({ ...edited, enabled: false }, presets);
    expect(layerModified(reset, presets)).toBe(false);
    expect(reset.enabled).toBe(false);
    // Flux and hysteresis too; no preset: no mark, nothing to reset.
    const tape = layerFromPreset(preset("tape"));
    expect(layerModified(tape, presets)).toBe(false);
    expect(layerModified(tape.kind === "flux" ? { ...tape, corner_hz: 300 } : tape, presets)).toBe(true);
    const studio = layerFromPreset(preset("tape_studio"));
    expect(studio.kind).toBe("hysteresis");
    expect(layerModified(studio, presets)).toBe(false);
    const editedHyst = studio.kind === "hysteresis" ? { ...studio, width: 0.45 } : studio;
    expect(layerModified(editedHyst, presets)).toBe(true);
    const resetHyst = resetLayer({ ...editedHyst, enabled: false }, presets);
    expect(layerModified(resetHyst, presets)).toBe(false);
    // The bass EQ and the field offset count; switching the EQ off is a
    // change, but a hand-built layer with an EQ off sounds as none.
    if (studio.kind === "hysteresis") {
      expect(studio.bass_eq?.db).toBeGreaterThan(0);
      expect(studio.asymmetry).toBeGreaterThan(0);
      expect(layerModified({ ...studio, bass_eq: { ...studio.bass_eq!, enabled: false } }, presets)).toBe(true);
      expect(layerModified({ ...studio, asymmetry: 0 }, presets)).toBe(true);
      expect(layerModified({ ...studio, bass_eq: { ...studio.bass_eq!, hz: 200 } }, presets)).toBe(true);
      const plain = { ...studio, bass_eq: undefined, asymmetry: undefined };
      const plainPreset = [{ ...preset("tape_studio"), layer: plain }];
      expect(layerModified({ ...plain, bass_eq: { enabled: false, hz: 90, db: 6 }, asymmetry: 0 }, plainPreset)).toBe(false);
    }
    expect(resetHyst.enabled).toBe(false);
    expect(layerName(studio, presets)).toBe("Studio tape");
    expect(layerName({ ...studio, preset: undefined })).toBe("Custom hysteresis");
    const own = custom([-30]).layers[0];
    expect(layerModified(own, presets)).toBeNull();
    expect(resetLayer(own, presets)).toBe(own);
    expect(layerName(own)).toBe("Custom Chebyshev");
    expect(layerName(tape, presets)).toBe("Tape, hot");
  });
  it("lists layers below their clean rate in readouts", () => {
    const studio = layerFromPreset(preset("tape_studio"));
    const cfg = harm({ layers: [layerFromPreset(preset("triode")), studio] });
    expect(mockHarmonicsReadout(cfg, 44100, false).below_clean_rate).toEqual([{ layer: 1, clean_rate_hz: 176400 }]);
    expect(mockHarmonicsReadout(cfg, 96000, false).below_clean_rate).toHaveLength(1);
    expect(mockHarmonicsReadout(cfg, 176400, false).below_clean_rate).toBeUndefined();
    expect(mockHarmonicsReadout(cfg, 192000, false).below_clean_rate).toBeUndefined();
    const zeroAmt = harm({ layers: [studio.kind === "hysteresis" ? { ...studio, amount: 0 } : studio] });
    expect(mockHarmonicsReadout(zeroAmt, 44100, false).below_clean_rate).toBeUndefined();
  });
  it("readouts reach H15, fold orders above Nyquist and measure the aliasing", () => {
    const a = custom([-30, -40]).layers[0];
    const b = custom([-30, -40, -50, -60]).layers[0];
    const series = harm({ layers: [a, b], routing: "series" });
    const r = mockHarmonicsReadout(series, 192000, true);
    expect(r.points[0].harmonics_db).toHaveLength(14);
    // H2–H5 of 20 kHz at 192 kHz: none folds into the band in parallel;
    // in series the cross terms from H9 up do.
    const par = mockHarmonicsReadout(harm({ layers: [a, b] }), 192000, false);
    expect(par.aliasing.warn).toBe(false);
    expect(par.aliasing.level_db).toBe(-200);
    expect(r.aliasing.level_db).toBeGreaterThan(-200);
    // At 96 kHz H4 and H5 fold to 16 and 4 kHz.
    const r96 = mockHarmonicsReadout(harm({ layers: [b] }), 96000, false);
    expect(r96.aliasing.warn).toBe(true);
    expect(r96.aliasing.worst_db).toBeGreaterThan(-70);
    // 20 kHz at 44.1 kHz: H2 (40 kHz) folds to 4.1 kHz, out of THD.
    const lo = mockHarmonicsReadout(harm({ layers: [a] }), 44100, true);
    const p = lo.freq_sweep!.find((q) => q.freq_hz === 20000 && q.input_dbfs === 0)!;
    expect(p.harmonics_db[0]).toBe(-200);
    expect(p.aliased?.find((x) => x.order === 2)?.freq_hz).toBeCloseTo(4100);
    expect(firstAliasedFreq(lo.freq_sweep, 0)).toBeLessThanOrEqual(12500);
  });
  it("sag and bias: depth 0 is no change, edits mark the layer, under-load sweep", () => {
    const presets = MOCK_HARMONICS_PRESETS;
    const triode = layerFromPreset(preset("se_triode_high_mu"));
    expect(layerDynamic(triode)).toBe(false);
    let cfg = harm({ layers: [triode] });
    // Depth 0 (the controls' starting point) sounds as none.
    cfg = withSag(cfg, 0, (s) => ({ ...s, attack_ms: 5 }));
    expect(layerDynamic(cfg.layers[0])).toBe(false);
    expect(layerModified(cfg.layers[0], presets)).toBe(false);
    expect(mockHarmonicsReadout(cfg, 96000, true).under_load).toBeUndefined();
    cfg = withSag(cfg, 0, (s) => ({ ...s, depth_db: 3 }));
    cfg = withBias(cfg, 0, (b) => ({ ...b, depth: -0.1 }));
    const l = cfg.layers[0];
    expect(l.kind === "chebyshev" && l.sag).toEqual({ depth_db: 3, attack_ms: 5, release_ms: 200 });
    expect(l.kind === "chebyshev" && l.bias).toEqual({ ...defaultBias(), depth: -0.1 });
    expect(layerDynamic(l)).toBe(true);
    // Both depths scale with the amount: amount 0 is off.
    expect(layerDynamic({ ...l, amount: 0 } as typeof l)).toBe(false);
    expect(layerModified(l, presets)).toBe(true);
    expect(layerModified(resetLayer(l, presets), presets)).toBe(false);
    const r = mockHarmonicsReadout(cfg, 96000, true);
    expect(r.under_load).toHaveLength(r.sweep!.length);
    const at = (pts: typeof r.sweep, db: number) => pts!.find((p) => p.freq_hz === 1000 && p.input_dbfs === db)!;
    expect(at(r.under_load, 0).gain_db).toBeLessThan(at(r.sweep, 0).gain_db - 2);
    expect(sweepCurves(r.under_load, 1000).levels).toHaveLength(21);
    // Flux and hysteresis layers take no sag.
    const tape = harm({ layers: [layerFromPreset(preset("tape"))] });
    expect(withSag(tape, 0, (s) => ({ ...s, depth_db: 3 }))).toEqual(tape);
  });
  it("groups presets by origin and reads the level sweep", () => {
    const groups = presetGroups(MOCK_HARMONICS_PRESETS);
    expect(groups.map((g) => g.origin)).toEqual(["hand_picked", "computed", "archetype"]);
    expect(groups[0].presets.map((p) => p.id)).toEqual(["triode", "pentode"]);
    const r = mockHarmonicsReadout(harm({}), 96000, true);
    const s = sweepCurves(r.sweep, 1000);
    expect(s.levels).toHaveLength(21);
    expect(s.levels[0]).toBe(-40);
    expect(s.levels[20]).toBe(0);
    // The default (high-mu triode) H2 at full scale, falling about 1 dB per dB below it.
    expect(s.orders[0][20]).toBeCloseTo(-25.4, 1);
    expect(Math.abs(s.orders[0][10] + 45.4)).toBeLessThan(1.5);
    expect(sweepCurves(undefined, 1000).levels).toEqual([]);
  });
  it("edits a layer's tilt, keeps a flat one unmodified, and reads the frequency sweep", () => {
    const presets = MOCK_HARMONICS_PRESETS;
    const c = harm({ layers: [layerFromPreset(preset("triode"))] });
    // A flat tilt (moved corner, no rise) is not a modification.
    const flat = withTilt(c, 0, (t) => ({ ...t, low: { ...t.low, hz: 300 } }));
    expect(flat.layers[0].kind === "chebyshev" && flat.layers[0].tilt?.low.hz).toBe(300);
    expect(layerModified(flat.layers[0], presets)).toBe(false);
    const bass = withTilt(c, 0, (t) => ({ ...t, low: { hz: 60, db: 12 } }));
    expect(layerModified(bass.layers[0], presets)).toBe(true);
    const l = bass.layers[0];
    if (l.kind !== "chebyshev" || !l.tilt) throw new Error("tilt");
    // The bass rise: odd orders only unless low_even; the treble rise everywhere.
    expect(orderShelves(l.tilt, 3).low.db).toBe(12);
    expect(orderShelves(l.tilt, 2).low.db).toBe(0);
    expect(orderShelves({ ...l.tilt, low_even: true }, 2).low.db).toBe(12);
    // Overrides: set one, then clear it (the `orders` field goes away).
    const own = withOrderTilt(l.tilt, 4, { low: { hz: 200, db: 6 }, high: { hz: 5000, db: -3 } });
    expect(own.orders?.[2]).toEqual({ low: { hz: 200, db: 6 }, high: { hz: 5000, db: -3 } });
    expect(orderShelves(own, 4).high.db).toBe(-3);
    expect(withOrderTilt(own, 4, null).orders).toBeUndefined();
    expect(tiltFlat(defaultTilt())).toBe(true);
    expect(tiltFlat(own)).toBe(false);
    expect(tiltFlat(withOrderTilt(defaultTilt(), 3, { low: { hz: 100, db: 0 }, high: { hz: 2000, db: 0 } }))).toBe(true);
    // The mock's frequency sweep follows the tilt: H3 up in the bass, H2 not.
    const r = mockHarmonicsReadout(bass, 96000, true);
    const f = freqCurves(r.freq_sweep, 0);
    expect(f.freqs).toHaveLength(31);
    expect(f.freqs[0]).toBe(20);
    expect(f.freqs[30]).toBe(20000);
    const at = (hz: number) => f.freqs.indexOf(hz);
    expect(f.orders[1][at(20)] - f.orders[1][at(1000)]).toBeGreaterThan(10);
    expect(Math.abs(f.orders[0][at(20)] - f.orders[0][at(1000)])).toBeLessThan(0.5);
    expect(f.orders[1][at(1000)]).toBeCloseTo(-58, 1);
    expect(freqCurves(r.freq_sweep, -18).freqs).toHaveLength(31);
    expect(freqCurves(undefined, 0).freqs).toEqual([]);
  });
  it("sets shelf slopes, stores the default as none, and compares them by sound", () => {
    const presets = MOCK_HARMONICS_PRESETS;
    const c = harm({ layers: [layerFromPreset(preset("triode"))] });
    expect(withShelfSlope({ hz: 60, db: 12 }, 24, 12)).toEqual({ hz: 60, db: 12, slope: 24 });
    expect(withShelfSlope({ hz: 60, db: 12, slope: 24 }, 12, 12)).toEqual({ hz: 60, db: 12 });
    const bass = withTilt(c, 0, (t) => ({ ...t, low: { hz: 60, db: 12 } }));
    const steep = withTilt(bass, 0, (t) => ({ ...t, low: withShelfSlope(t.low, 24, 12) }));
    const l = steep.layers[0];
    if (l.kind !== "chebyshev" || !l.tilt) throw new Error("tilt");
    // The layer's bass slope reaches the orders it applies to.
    expect(orderShelves(l.tilt, 3).low).toEqual({ hz: 60, db: 12, slope: 24 });
    // An explicit default slope sounds the same as none; a slope on a 0 dB shelf too.
    const pp = harm({ layers: [layerFromPreset(preset("pp_pentode_feedback"))] });
    const same = withTilt(pp, 0, (t) => ({ ...t, low: { ...t.low, slope: 12 }, high: { ...t.high, slope: 6 } }));
    expect(layerModified(same.layers[0], presets)).toBe(false);
    const steeper = withTilt(pp, 0, (t) => ({ ...t, high: withShelfSlope(t.high, 12, 6) }));
    expect(layerModified(steeper.layers[0], presets)).toBe(true);
    const flatSteep = withTilt(c, 0, (t) => ({ ...t, low: { hz: 60, db: 0, slope: 24 } }));
    expect(layerModified(flatSteep.layers[0], presets)).toBe(false);
    // The mock's sweep is steeper at 24 dB/oct: an octave below the corner, H3 rises more.
    const f12 = freqCurves(mockHarmonicsReadout(bass, 96000, true).freq_sweep, 0);
    const f24 = freqCurves(mockHarmonicsReadout(steep, 96000, true).freq_sweep, 0);
    const i = f12.freqs.findIndex((hz) => hz >= 30);
    expect(f24.orders[1][i]).toBeGreaterThan(f12.orders[1][i] + 0.5);
    // Both reach the same full rise far below the corner (within the 20 Hz end).
    expect(Math.abs(f24.orders[1][0] - f12.orders[1][0])).toBeLessThan(3);
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

describe("harmonics systems", () => {
  it("load as a whole stage of ordinary preset layers", () => {
    expect(MOCK_HARMONICS_SYSTEMS.length).toBeGreaterThanOrEqual(5);
    for (const s of MOCK_HARMONICS_SYSTEMS) {
      const cfg = configFromSystem(s);
      expect(cfg.enabled).toBe(true);
      expect(cfg.dc_block).toBe(true);
      expect(cfg.routing ?? "parallel").toBe(s.routing ?? "parallel");
      expect(cfg.layers.length).toBe(s.layers.length);
      for (const l of cfg.layers) {
        expect(l.enabled).toBe(true);
        expect(MOCK_HARMONICS_PRESETS.some((p) => p.id === l.preset && p.layer.kind === l.kind)).toBe(true);
        // Modified or not, the layer resets to its preset as any other.
        expect(layerModified(l, MOCK_HARMONICS_PRESETS)).not.toBeNull();
      }
    }
    const set = MOCK_HARMONICS_SYSTEMS.find((s) => s.id === "set_amp")!;
    const cfg = configFromSystem(set, false);
    expect(cfg.enabled).toBe(false);
    expect(harmonicsSeries(cfg)).toBe(true);
    expect(layerModified(cfg.layers[1], MOCK_HARMONICS_PRESETS)).toBe(true);
    // Loading copies: editing the stage leaves the system untouched.
    (cfg.layers[0] as ChebyshevLayer).drive_db = -3;
    expect((set.layers[0] as ChebyshevLayer).drive_db).toBe(-12);
    expect(systemChain(set, MOCK_HARMONICS_PRESETS)).toBe("Single-ended triode, high-mu → Single-ended triode, low-mu → Output transformer");
    const pp = MOCK_HARMONICS_SYSTEMS.find((s) => s.id === "pp_amp")!;
    expect(configFromSystem(pp).routing).toBeUndefined();
    expect(layerModified(configFromSystem(pp).layers[0], MOCK_HARMONICS_PRESETS)).toBe(false);
  });

  it("are the baseline of a stage loaded from one", () => {
    const set = MOCK_HARMONICS_SYSTEMS.find((s) => s.id === "set_amp")!;
    const cfg = configFromSystem(set);
    expect(cfg.system).toBe("set_amp");
    expect(stageSystem(cfg, MOCK_HARMONICS_SYSTEMS)).toBe(set);
    expect(stageSystem({ ...cfg, system: "nope" }, MOCK_HARMONICS_SYSTEMS)).toBeUndefined();
    // Freshly loaded: nothing modified, though layer 1 differs from its preset.
    const bases = systemLayers(cfg, set);
    expect(bases.every((b, i) => b !== undefined && !layerDiffers(cfg.layers[i], b))).toBe(true);
    expect(stageModified(cfg, set)).toBe(false);
    // An edit marks that layer and the stage.
    const edited = { ...cfg, layers: cfg.layers.map((l, i) => (i === 0 ? ({ ...l, drive_db: -3 } as HarmonicsLayer) : l)) };
    expect(layerDiffers(edited.layers[0], systemLayers(edited, set)[0]!)).toBe(true);
    expect(stageModified(edited, set)).toBe(true);
    // Moving keeps the pairing; the order counts for the stage.
    const moved = { ...cfg, layers: [cfg.layers[2], cfg.layers[0], cfg.layers[1]] };
    expect(systemLayers(moved, set).map((b) => b?.preset)).toEqual(["output_transformer", "se_triode_high_mu", "se_triode_low_mu"]);
    expect(stageModified(moved, set)).toBe(true);
    // A layer added by hand has no system version; switching one off or another routing modifies the stage.
    const added = { ...cfg, layers: [...cfg.layers, cfg.layers[0]] };
    expect(systemLayers(added, set)[3]).toBeUndefined();
    expect(stageModified(added, set)).toBe(true);
    expect(stageModified({ ...cfg, layers: cfg.layers.map((l, i) => (i === 2 ? { ...l, enabled: false } : l)) }, set)).toBe(true);
    expect(stageModified({ ...cfg, routing: "parallel" }, set)).toBe(true);
    expect(systemLayers(cfg, undefined)).toEqual([undefined, undefined, undefined]);
  });
});
