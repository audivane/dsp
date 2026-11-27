import { describe, expect, it } from "vitest";
import type { TiltConfig } from "../api/types";
import { tiltDb, tiltProblem } from "./tilt";

const tilt = (p: Partial<TiltConfig>): TiltConfig => ({ enabled: true, slope_db_per_oct: -1, total_db: null, start_hz: 20, end_hz: 20000, pivot: "start", ...p });

describe("tilt target", () => {
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

describe("two-section tilt (Both)", () => {
  const low = tilt({ slope_db_per_oct: 2, start_hz: 20, end_hz: 200, pivot: "end" });
  const high = tilt({ slope_db_per_oct: -1, start_hz: 1000, end_hz: 20000, pivot: "start" });

  it("checks the pair like the engine", async () => {
    const { tiltPairProblem, tiltConfigError } = await import("./tilt");
    expect(tiltPairProblem(low, { ...high, start_hz: 150 })).toContain("below the treble");
    expect(tiltPairProblem({ ...low, pivot: "center" }, high)).toContain("pivots");
    expect(tiltConfigError({ ...high, end_hz: 20001 })).toMatch(/end_hz/);
  });
});
