import { describe, expect, it } from "vitest";
import type { EqProfile } from "../api/types";
import { parseApo, toApo } from "./apo";
import { stagePatch } from "./abCompare";
import { channelShapeDb, profileAutoPreampDb } from "./eqmath";
import { snapTone, toneBands, withTone } from "./tone";

const base = (): EqProfile => ({ name: "", version: 2, preamp_db: 0, linked_enabled: true, bands: [] });

describe("tone knobs", () => {
  it("draws the curve dsp-core runs (bass +6, treble -3.5 at 48 kHz)", () => {
    // From dsp-core's `Eq::response_channel` for the same profile.
    const want: [number, number][] = [
      [20, 5.858329],
      [100, 3.896297],
      [140, 2.9971],
      [300, 1.161037],
      [1000, -0.005757],
      [3000, -0.936246],
      [4900, -1.744557],
      [10000, -2.918867],
      [20000, -3.47004],
    ];
    const eq = { ...base(), tone: { bass_db: 6, treble_db: -3.5 } };
    for (const ch of [0, 1]) {
      const got = channelShapeDb(eq, ch, 48000, want.map(([f]) => f));
      want.forEach(([, db], i) => expect(got[i]).toBeCloseTo(db, 5));
    }
    expect(profileAutoPreampDb(eq, 48000)).toBeCloseTo(-6, 4);
  });

  it("adds nothing at 0/0, also with the linked section off", () => {
    expect(toneBands({ bass_db: 0, treble_db: 0 })).toEqual([]);
    const off = { ...base(), linked_enabled: false, tone: { bass_db: 3, treble_db: 0 } };
    expect(channelShapeDb(off, 1, 48000, [20])[0]).toBeGreaterThan(2.5);
  });

  it("snaps, clamps and drops a flat tone", () => {
    expect(snapTone(2.26)).toBe(2.5);
    expect(snapTone(-9)).toBe(-6);
    const one = withTone(base(), "bass_db", 1.5);
    expect(one.tone).toEqual({ bass_db: 1.5, treble_db: 0 });
    expect("tone" in withTone(one, "bass_db", 0)).toBe(false);
  });

  it("round-trips through ParametricEQ.txt as two shelves", () => {
    const eq: EqProfile = {
      ...base(),
      bands: [{ kind: "peaking", freq_hz: 1000, gain_db: 2, q: 1, enabled: true }],
      tone: { bass_db: 3.5, treble_db: -2 },
    };
    const text = toApo(eq);
    expect(text).toContain("Filter 2: ON LSC Fc 140 Hz Gain 3.5 dB Q 0.48");
    expect(text).toContain("Filter 3: ON HSC Fc 4900 Hz Gain -2 dB Q 0.49");
    expect(parseApo(text).profile).toEqual(eq);
    expect(parseApo("Filter 1: ON LSC Fc 140 Hz Gain 3.3 dB Q 0.48\n").profile.tone).toBeUndefined();
  });

  it("goes off with the EQ in an A/B stage compare", () => {
    const chain = { eq: { ...base(), tone: { bass_db: 2, treble_db: 0 } } } as never;
    expect(stagePatch(chain, "eq")).toMatchObject({ patch: { eq: { tone: null } }, label: "EQ off" });
  });
});
