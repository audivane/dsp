import { describe, expect, it } from "vitest";
import type { ChebyshevLayer, FluxLayer } from "../api/types";
import { MOCK_HARMONICS_PRESETS } from "../api/mockHarmonics";
import { chebApplied, chebCurve, chebPoly, chebReference, chebT, fluxDrive, fluxH3Db, harmonicsDb, niceGain, project, sagTrace } from "./harmonicsCurves";

const preset = <T>(id: string) => MOCK_HARMONICS_PRESETS.find((p) => p.id === id)!.layer as unknown as T;

describe("harmonicsCurves", () => {
  it("evaluates Chebyshev polynomials inside and beyond ±1", () => {
    expect(chebT(3, 0.5)).toBeCloseTo(4 * 0.125 - 3 * 0.5, 12);
    expect(chebT(2, 1.25)).toBeCloseTo(2 * 1.5625 - 1, 12);
    expect(chebT(5, Math.cos(0.3))).toBeCloseTo(Math.cos(1.5), 12);
  });

  it("gives a layer its set levels at 0 dBFS, drive 0 dB", () => {
    const l = preset<ChebyshevLayer>("se_triode_high_mu");
    const a = chebApplied({ ...l, drive_db: 0, amount: 1 });
    expect(a).toEqual(chebReference(l));
    const h = harmonicsDb((v) => chebCurve(a, v), 1, 3);
    expect(h[0]).toBeCloseTo(l.levels_db[0]!, 1);
    expect(h[1]).toBeCloseTo(l.levels_db[1]!, 1);
  });

  it("reads the drive as the engine does: the curve's harmonics at that level", () => {
    const l = { ...preset<ChebyshevLayer>("se_triode_high_mu"), drive_db: -12, amount: 1 };
    const a = chebApplied(l);
    const d = Math.pow(10, -12 / 20);
    const h = project((x) => chebPoly(chebReference(l), d * x));
    expect(a[0]).toBeCloseTo(h[2] / h[1], 9);
    // H2 falls about 1 dB per dB of drive.
    expect(20 * Math.log10(Math.abs(a[0]))).toBeCloseTo(l.levels_db[0]! - 12, 0);
  });

  it("calibrates a flux layer's drive to its H3 target", () => {
    const l = preset<FluxLayer>("output_transformer");
    const g = fluxDrive(l);
    expect(fluxH3Db(l, g, l.calibration_hz, 1)).toBeCloseTo(l.target_h3_db, 1);
    // Lower in level and above the corner, H3 falls.
    expect(fluxH3Db(l, g, l.calibration_hz, 0.125)).toBeLessThan(l.target_h3_db - 30);
    expect(fluxH3Db(l, g, 5000, 1)).toBeLessThan(l.target_h3_db - 20);
  });

  it("traces sag: the drive drops on the loud passage and recovers", () => {
    const l: ChebyshevLayer = { ...preset<ChebyshevLayer>("se_triode_high_mu"), sag: { depth_db: 3, attack_ms: 40, release_ms: 500 } };
    const tr = sagTrace(l);
    const at = (t: number) => tr.t.findIndex((x) => x >= t);
    expect(tr.driveDb[at(0.4)]).toBeGreaterThan(-20.6);
    // End of the long loud passage: close to the full 3 dB.
    expect(tr.driveDb[at(1.85)]).toBeLessThan(-2.5);
    expect(tr.driveDb[at(1.85)]).toBeGreaterThan(-3.1);
    // Under sag the loud passage is cleaner than at rest.
    expect(tr.h2[at(1.85)]).toBeLessThan(tr.h2Rest[at(1.85)] - 1);
    // No sag or bias: the drive is the input.
    const flat = sagTrace({ ...l, sag: undefined });
    expect(flat.driveDb.every((v, i) => Math.abs(v - flat.inputDb[i]) < 1e-9)).toBe(true);
  });

  it("picks nice magnifications", () => {
    expect(niceGain(0.01, 0.8)).toBe(50);
    expect(niceGain(0.5, 0.8)).toBe(1);
    expect(niceGain(0, 0.8)).toBe(1);
  });
});
