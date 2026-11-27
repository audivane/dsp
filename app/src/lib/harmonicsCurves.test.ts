import { describe, expect, it } from "vitest";
import type { ChebyshevLayer, FluxLayer } from "../api/types";
import { MOCK_HARMONICS_PRESETS } from "../api/mockHarmonics";
import { chebApplied, chebPoly, chebReference, fluxDrive, fluxH3Db, project } from "./harmonicsCurves";

const preset = <T>(id: string) => MOCK_HARMONICS_PRESETS.find((p) => p.id === id)!.layer as unknown as T;

describe("harmonicsCurves", () => {

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

});
