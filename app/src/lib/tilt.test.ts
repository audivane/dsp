import { describe, expect, it } from "vitest";
import type { EqProfile, TiltConfig } from "../api/types";
import { profileAutoPreampDb, profileChannelResponseDb } from "./eqmath";
import { normalizeEq, eqIsActive, emptyEq } from "./chain";
import { clampCorner, tiltDb, tiltProblem, tiltShape, tiltSlope, tiltSummary, tiltTotal, withShape } from "./tilt";

const tilt = (p: Partial<TiltConfig>): TiltConfig => ({ enabled: true, slope_db_per_oct: -1, total_db: null, start_hz: 20, end_hz: 20000, pivot: "start", ...p });

describe("tilt target", () => {
  it("is flat outside the band, sloped inside, pivot at 0 dB", () => {
    const t = tilt({ slope_db_per_oct: -1, start_hz: 100, end_hz: 1600, pivot: "start" });
    const [below, mid, end, above] = tiltDb(t, 48000, [50, 400, 1600, 10000]);
    expect(below).toBe(0);
    expect(mid).toBeCloseTo(-2, 9);
    expect(end).toBeCloseTo(-4, 9);
    expect(above).toBeCloseTo(-4, 9);
    const [lo, hi] = tiltDb({ ...t, pivot: "end" }, 48000, [10, 20000]);
    expect(lo).toBeCloseTo(4, 9);
    expect(hi).toBeCloseTo(0, 9);
    expect(tiltDb({ ...t, pivot: "center" }, 48000, [400])[0]).toBeCloseTo(0, 9);
    expect(tiltDb({ ...t, pivot: { hz: 200 } }, 48000, [200])[0]).toBeCloseTo(0, 9);
  });
  it("derives the slope from a total over the requested band", () => {
    const t = tilt({ slope_db_per_oct: null, total_db: -6, start_hz: 100, end_hz: 1600 });
    expect(tiltSlope(t)).toBeCloseTo(-1.5, 9);
    expect(tiltTotal(t)).toBeCloseTo(-6, 9);
    expect(tiltSummary(t)).toBe("tilt −6.0 dB above 100 Hz");
  });
  it("clamps the end to 0.45 fs and keeps the slope", () => {
    const t = tilt({ slope_db_per_oct: 1 });
    const v = tiltDb(t, 44100, [19845, 21000]);
    expect(v[0]).toBeCloseTo(Math.log2(19845 / 20), 6);
    expect(v[1]).toBeCloseTo(v[0], 9);
  });
  it("applies the engine's limits", () => {
    expect(tiltProblem(tilt({}), 48000)).toBeNull();
    expect(tiltProblem(tilt({ slope_db_per_oct: 7 }), 48000)).toContain("±6");
    expect(tiltProblem(tilt({ slope_db_per_oct: null, total_db: 30 }), 48000)).not.toBeNull();
    expect(tiltProblem(tilt({ start_hz: 1000, end_hz: 1050 }), 48000)).toContain("1/6 octave");
    expect(tiltProblem(tilt({ pivot: { hz: 30000 } }), 48000)).toContain("inside the band");
    expect(tiltProblem(tilt({ total_db: -3 }), 48000)).toContain("not both");
    expect(tiltDb(tilt({ slope_db_per_oct: 7 }), 48000, [1000])[0]).toBe(0);
  });
});

describe("tilt in the EQ", () => {
  it("is part of the response and the auto preamp", () => {
    const eq: EqProfile = { ...emptyEq("t"), tilt: tilt({ slope_db_per_oct: 1, pivot: "start" }) };
    const [r] = profileChannelResponseDb(eq, 0, 48000, [20000]);
    expect(r).toBeCloseTo(Math.log2(1000), 6);
    // +~10 dB at the top: the preamp takes it all off.
    expect(profileAutoPreampDb(eq, 48000)).toBeCloseTo(-Math.log2(1000), 1);
    expect(profileAutoPreampDb({ ...eq, tilt: { ...eq.tilt!, enabled: false } }, 48000)).toBe(0);
    expect(eqIsActive(eq)).toBe(true);
  });
  it("survives normalisation", () => {
    const eq: EqProfile = { ...emptyEq("t"), tilt: tilt({}) };
    expect(normalizeEq(eq).tilt).toEqual(eq.tilt);
  });
});

describe("tilt review fixes", () => {
  it("validates the stored fields even when disabled, like config.rs", async () => {
    const { tiltConfigError, normalizeTilt } = await import("./tilt");
    expect(tiltConfigError(tilt({ enabled: false, start_hz: 1 }))).toMatch(/between 20 and 20000 Hz/);
    expect(tiltConfigError(tilt({ enabled: false, end_hz: 400000 }))).toMatch(/end_hz/);
    expect(tiltConfigError(tilt({ enabled: false, total_db: -3 }))).toMatch(/exactly one/);
    expect(tiltConfigError(tilt({ enabled: false }))).toBeNull();
    // Serde defaults: enabled, 20 Hz–20 kHz, centre pivot.
    expect(normalizeTilt({ slope_db_per_oct: 1 } as unknown as TiltConfig)).toEqual({
      enabled: true,
      slope_db_per_oct: 1,
      total_db: null,
      start_hz: 20,
      end_hz: 20000,
      pivot: "center",
    });
  });
  it("bounds slope and total by the band, and converts at full precision", async () => {
    const { tiltRanges, tiltAs } = await import("./tilt");
    const t = tilt({ slope_db_per_oct: null, total_db: 24 });
    const r = tiltRanges(t);
    expect(r.slope).toBeCloseTo(24 / Math.log2(1000), 9);
    expect(r.total).toBe(24);
    const s = tiltAs(t, "slope");
    expect(s.total_db).toBeNull();
    expect(s.slope_db_per_oct).toBeCloseTo(24 / Math.log2(1000), 12);
    expect(tiltProblem(s, 48000)).toBeNull();
    expect(tiltAs(s, "total").total_db).toBeCloseTo(24, 9);
    // A narrow band caps the total by the slope limit.
    expect(tiltRanges(tilt({ start_hz: 1000, end_hz: 4000 }))).toEqual({ slope: 6, total: 12 });
  });
});

describe("tilt shapes", () => {
  const base = tilt({ pivot: { hz: 1000 } });
  it("maps each shape onto the band and pivot", () => {
    const above = withShape(base, "above", 1000);
    expect(above).toMatchObject({ start_hz: 1000, end_hz: 20000, pivot: "start" });
    expect(tiltDb(above, 48000, [500, 1000])).toEqual([0, 0]);
    expect(tiltDb(above, 48000, [2000])[0]).toBeCloseTo(-1, 9);
    const below = withShape(base, "below", 200);
    expect(below).toMatchObject({ start_hz: 20, end_hz: 200, pivot: "end" });
    expect(tiltDb(below, 48000, [200, 5000]).map((v) => v + 0)).toEqual([0, 0]);
    expect(tiltDb(below, 48000, [100])[0]).toBeCloseTo(1, 9);
    expect(withShape(below, "whole", 200)).toMatchObject({ start_hz: 20, end_hz: 20000, pivot: "center" });
    expect(withShape(base, "whole", 0)).toMatchObject({ pivot: { hz: 1000 } });
  });
  it("reads a config back as its shape, or the closest one", () => {
    expect(tiltShape(base)).toEqual({ shape: "whole", hz: 1000 });
    expect(tiltShape(withShape(base, "above", 1000))).toEqual({ shape: "above", hz: 1000 });
    expect(tiltShape(withShape(base, "below", 200))).toEqual({ shape: "below", hz: 200 });
    expect(tiltShape(tilt({ start_hz: 300, end_hz: 16000, pivot: "center" })).shape).toBe("above");
    expect(tiltShape(tilt({ start_hz: 25, end_hz: 2000, pivot: "center" }))).toEqual({ shape: "below", hz: 2000 });
    expect(tiltShape(tilt({ start_hz: 22, end_hz: 19000, pivot: "start" })).shape).toBe("above");
    expect(tiltShape(tilt({ start_hz: 21, end_hz: 19000, pivot: "center" })).shape).toBe("whole");
  });
  it("keeps the corner in 20 Hz–20 kHz with room to slope, and the amount in range", () => {
    expect(clampCorner("above", 20000)).toBe(17679);
    expect(clampCorner("below", 20)).toBe(23);
    expect(clampCorner("above", 5)).toBe(20);
    expect(tiltProblem(withShape(base, "above", 20000), 44100)).toBeNull();
    expect(tiltProblem(withShape(base, "below", 20), 48000)).toBeNull();
    const steep = withShape(tilt({ slope_db_per_oct: 6, start_hz: 1000, pivot: "start" }), "whole", 0);
    expect(steep.slope_db_per_oct).toBeCloseTo(24 / Math.log2(1000), 9);
    expect(tiltProblem(steep, 48000)).toBeNull();
  });
  it("names the shape in the summary", () => {
    expect(tiltSummary(withShape(base, "above", 1000))).toBe("tilt −1.0 dB/oct above 1 kHz");
    expect(tiltSummary(withShape(base, "below", 200))).toBe("tilt −1.0 dB/oct below 200 Hz");
    expect(tiltSummary(base)).toBe("tilt −1.0 dB/oct");
  });
});

describe("two-section tilt (Both)", () => {
  const low = tilt({ slope_db_per_oct: 2, start_hz: 20, end_hz: 200, pivot: "end" });
  const high = tilt({ slope_db_per_oct: -1, start_hz: 1000, end_hz: 20000, pivot: "start" });
  const eq = (p: Partial<EqProfile> = {}): EqProfile => ({ ...emptyEq(), tilt: low, tilt_high: high, ...p });

  it("sums both sections, flat between, in the curve and the auto preamp", async () => {
    const { tiltStageDb } = await import("./tilt");
    const [bass, mid1, mid2, top] = tiltStageDb(eq(), 48000, [20, 300, 800, 20000]);
    expect(bass).toBeCloseTo(-2 * Math.log2(10), 9);
    expect(mid1).toBe(0);
    expect(mid2).toBe(0);
    expect(top).toBeCloseTo(-Math.log2(20), 9);
    expect(profileChannelResponseDb(eq(), 0, 48000, [20])[0]).toBeCloseTo(bass, 9);
    // A treble boost sets the preamp.
    const boost = eq({ tilt_high: { ...high, slope_db_per_oct: 1 } });
    expect(profileAutoPreampDb(boost, 48000)).toBeCloseTo(-Math.log2(20), 1);
    // Overlapping bands: the engine refuses, so nothing runs.
    const bad = eq({ tilt_high: { ...high, start_hz: 100 } });
    expect(tiltStageDb(bad, 48000, [20, 20000])).toEqual([0, 0]);
    expect(eqIsActive(normalizeEq(eq()))).toBe(true);
    expect(normalizeEq(eq()).tilt_high).toEqual(high);
  });

  it("reads as one summary", async () => {
    const { tiltStageSummary } = await import("./tilt");
    expect(tiltStageSummary(eq())).toBe("tilt +2.0 dB/oct below 200 Hz, −1.0 dB/oct above 1 kHz");
    expect(tiltStageSummary(eq({ tilt: { ...low, enabled: false } }))).toBe("tilt −1.0 dB/oct above 1 kHz");
    expect(tiltStageSummary(eq({ tilt_high: null }))).toBe(tiltSummary(low));
  });

  it("switches shapes and keeps the corners apart", async () => {
    const { pairWithShape, withBoth, tiltPairProblem } = await import("./tilt");
    const above = { tilt: withShape(tilt({ slope_db_per_oct: -1 }), "above", 1000), tilt_high: null };
    const both = pairWithShape(above, "both");
    expect(both.tilt).toMatchObject({ start_hz: 20, end_hz: 200, pivot: "end", slope_db_per_oct: -1 });
    expect(both.tilt_high).toMatchObject({ start_hz: 1000, end_hz: 20000, pivot: "start", slope_db_per_oct: -1 });
    expect(tiltPairProblem(both.tilt, both.tilt_high!)).toBeNull();
    // Back out: Above keeps the treble section, Below the bass one.
    expect(pairWithShape(both, "above")).toEqual({ tilt: { ...both.tilt_high!, start_hz: 1000 }, tilt_high: null });
    expect(pairWithShape(both, "below").tilt).toMatchObject({ end_hz: 200, pivot: "end" });
    expect(pairWithShape(both, "whole").tilt).toMatchObject({ start_hz: 20, end_hz: 20000, pivot: "center" });
    // Bass corner pushed past the treble one stops below it, and the reverse.
    const pushed = withBoth(low, high, 5000, 1000, "bass");
    expect([pushed.tilt.end_hz, pushed.tilt_high!.start_hz]).toEqual([999, 1000]);
    const pulled = withBoth(low, high, 200, 50, "treble");
    expect([pulled.tilt.end_hz, pulled.tilt_high!.start_hz]).toEqual([200, 201]);
    // 20 Hz–20 kHz, each with room for its slope; amounts kept within range.
    const edge = withBoth({ ...low, slope_db_per_oct: 6 }, high, 1, 100000, "bass");
    expect(edge.tilt.end_hz).toBe(23);
    expect(edge.tilt_high!.start_hz).toBe(17679);
    expect(edge.tilt.slope_db_per_oct).toBe(6);
  });

  it("checks the pair like the engine", async () => {
    const { tiltPairProblem, tiltConfigError } = await import("./tilt");
    expect(tiltPairProblem(low, { ...high, start_hz: 150 })).toContain("below the treble");
    expect(tiltPairProblem({ ...low, pivot: "center" }, high)).toContain("pivots");
    expect(tiltConfigError({ ...high, end_hz: 20001 })).toMatch(/end_hz/);
  });
});
