import { describe, expect, it } from "vitest";
import type { EqProfile } from "../api/types";
import { channelShapeDb, profileAutoPreampDb } from "./eqmath";

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

});
