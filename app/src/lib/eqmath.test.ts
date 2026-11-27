import { describe, expect, it } from "vitest";
import fixture from "./__fixtures__/dsp-core-response.json";
import type { Band, EqProfile } from "../api/types";
import { normalizeEq } from "./chain";
import {
  autoPreampDb,
  channelShapeDb,
  channelsMatch,
  design,
  freeBandFreq,
  magDb,
  profileAutoPreampDb,
  profileChannelResponseDb,
  responseDb,
  sectionShapeDb,
} from "./eqmath";

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

  it("treats disabled and degenerate bands as identity", () => {
    const b: Band = { kind: "peaking", freq_hz: 1000, gain_db: 6, q: 1, enabled: false };
    expect(design(b, 48000)).toEqual({ b0: 1, b1: 0, b2: 0, a1: 0, a2: 0 });
    expect(design({ ...b, enabled: true, freq_hz: 30000 }, 48000).b1).toBe(0);
    expect(design({ ...b, enabled: true, q: 0 }, 48000).b1).toBe(0);
  });
});

describe("layered EQ response", () => {
  const b1: Band = { kind: "peaking", freq_hz: 200, gain_db: 3, q: 1.4, enabled: true };
  const bL: Band = { kind: "peaking", freq_hz: 1000, gain_db: 4, q: 2.0, enabled: true };
  const bR: Band = { kind: "peaking", freq_hz: 4000, gain_db: -2.5, q: 1.5, enabled: true };
  const freqs = [50, 200, 1000, 4000, 12000];

  it("combines global preamp + enabled linked + enabled channel section per channel", () => {
    const eq: EqProfile = {
      name: "layered",
      version: 2,
      preamp_db: -2,
      linked_enabled: true,
      bands: [b1],
      left: { enabled: true, preamp_db: 1, bands: [bL] },
      right: { enabled: true, preamp_db: 0, bands: [bR] },
    };

    const expL = responseDb([b1, bL], -2 + 1, 48000, freqs);
    const expR = responseDb([b1, bR], -2, 48000, freqs);
    const expCh2 = responseDb([b1], -2, 48000, freqs);

    profileChannelResponseDb(eq, 0, 48000, freqs).forEach((v, i) => expect(v).toBeCloseTo(expL[i], 10));
    profileChannelResponseDb(eq, 1, 48000, freqs).forEach((v, i) => expect(v).toBeCloseTo(expR[i], 10));
    profileChannelResponseDb(eq, 2, 48000, freqs).forEach((v, i) => expect(v).toBeCloseTo(expCh2[i], 10));

    // channelShapeDb omits global preamp_db (-2), keeps section preamp_db (+1 on L).
    const shapeL = channelShapeDb(eq, 0, 48000, freqs);
    shapeL.forEach((v, i) => expect(v).toBeCloseTo(expL[i] + 2, 10));

    // sectionShapeDb returns each section on its own.
    const secLinked = sectionShapeDb(eq, "linked", 48000, freqs);
    const secLeft = sectionShapeDb(eq, "left", 48000, freqs);
    responseDb([b1], 0, 48000, freqs).forEach((v, i) => expect(secLinked[i]).toBeCloseTo(v, 10));
    responseDb([bL], 1, 48000, freqs).forEach((v, i) => expect(secLeft[i]).toBeCloseTo(v, 10));

    expect(channelsMatch(eq, 48000)).toBe(false);
  });

  it("ignores disabled sections so they match absent sections", () => {
    const disabled: EqProfile = {
      name: "dis",
      version: 2,
      preamp_db: -1,
      linked_enabled: false,
      bands: [b1],
      left: { enabled: false, preamp_db: 3, bands: [bL] },
      right: { enabled: false, preamp_db: -3, bands: [bR] },
    };
    const absent: EqProfile = {
      name: "abs",
      version: 2,
      preamp_db: -1,
      linked_enabled: true,
      bands: [],
    };
    expect(profileChannelResponseDb(disabled, 0, 48000, freqs)).toEqual(profileChannelResponseDb(absent, 0, 48000, freqs));
    expect(profileChannelResponseDb(disabled, 1, 48000, freqs)).toEqual(profileChannelResponseDb(absent, 1, 48000, freqs));
    expect(channelsMatch(disabled, 48000)).toBe(true);
  });

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

  it("legacy split migration preserves independent L and R responses", () => {
    const legacy = {
      name: "old-split",
      preamp_db: -1.5,
      bands: [bL],
      right: { preamp_db: 0.5, bands: [bR] },
    } as unknown as EqProfile;
    const migrated = normalizeEq(legacy);
    expect(migrated.version).toBe(2);
    expect(migrated.preamp_db).toBe(0);
    expect(migrated.bands).toEqual([]);
    const lExp = responseDb([bL], -1.5, 48000, freqs);
    const rExp = responseDb([bR], 0.5, 48000, freqs);
    profileChannelResponseDb(migrated, 0, 48000, freqs).forEach((v, i) => expect(v).toBeCloseTo(lExp[i], 10));
    profileChannelResponseDb(migrated, 1, 48000, freqs).forEach((v, i) => expect(v).toBeCloseTo(rExp[i], 10));
  });
});

describe("freeBandFreq", () => {
  const at = (...fs: number[]): Band[] => fs.map((f) => ({ kind: "peaking", freq_hz: f, gain_db: 0, q: 1, enabled: true }));
  it("starts at 1 kHz with no bands", () => {
    expect(freeBandFreq([])).toBe(1000);
  });
  it("puts the next band in the widest gap, not on top of the last one", () => {
    // 20 Hz to 1 kHz is the widest gap (5.6 octaves): its middle is 141 Hz.
    expect(freeBandFreq(at(1000))).toBe(141);
    const bands = at(1000);
    for (let i = 0; i < 6; i++) bands.push(...at(freeBandFreq(bands)));
    const fs = bands.map((b) => b.freq_hz);
    expect(new Set(fs).size).toBe(fs.length);
  });
  it("fills an inner gap when it is the widest", () => {
    expect(freeBandFreq(at(30, 50, 15000, 19000))).toBe(866);
  });
});
