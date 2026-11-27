import { describe, expect, it } from "vitest";
import fixture from "./__fixtures__/dsp-core-response.json";
import type { Band, EqProfile } from "../api/types";
import { autoPreampDb, design, magDb, profileAutoPreampDb, responseDb } from "./eqmath";

// Reference values come from crates/dsp-core (app/src-tauri/examples/eq_fixture.rs).
interface Case {
  name: string;
  sample_rate: number;
  preamp_db: number;
  bands: Band[];
  freqs: number[];
  total_db: number[];
  band_db: number[][];
  auto_preamp_db: number;
}
const cases = (fixture as { cases: Case[] }).cases;

describe("EQ curve matches dsp-core", () => {
  it("has fixture cases", () => expect(cases.length).toBeGreaterThanOrEqual(4));

  for (const c of cases) {
    it(`total response: ${c.name}`, () => {
      const got = responseDb(c.bands, c.preamp_db, c.sample_rate, c.freqs);
      got.forEach((v, i) => expect(Math.abs(v - c.total_db[i])).toBeLessThan(1e-9));
    });
    it(`per-band response: ${c.name}`, () => {
      c.bands.forEach((b, j) => {
        const k = design(b, c.sample_rate);
        c.freqs.forEach((f, i) => expect(Math.abs(magDb(k, f, c.sample_rate) - c.band_db[j][i])).toBeLessThan(1e-9));
      });
    });
    it(`auto preamp: ${c.name}`, () => {
      expect(Math.abs(autoPreampDb(c.bands, c.sample_rate) - c.auto_preamp_db)).toBeLessThan(1e-9);
    });
  }

});

describe("layered EQ response", () => {
  const b1: Band = { kind: "peaking", freq_hz: 200, gain_db: 3, q: 1.4, enabled: true };

  it("computes auto preamp across the worst boost of L and R combined", () => {
    const eq: EqProfile = {
      name: "auto",
      version: 2,
      preamp_db: 0,
      linked_enabled: true,
      bands: [b1], // +3 dB at 200 Hz
      left: { enabled: true, preamp_db: 0, bands: [{ kind: "peaking", freq_hz: 200, gain_db: 4, q: 2, enabled: true }] },
      right: { enabled: false, preamp_db: 0, bands: [{ kind: "peaking", freq_hz: 200, gain_db: 9, q: 2, enabled: true }] },
    };
    const got = profileAutoPreampDb(eq, 48000);
    const expected = autoPreampDb([b1, ...eq.left!.bands], 48000);
    expect(got).toBeCloseTo(expected, 9);
    expect(got).toBeLessThan(-6.5);
  });

});

