import { describe, expect, it } from "vitest";
import type { AbStatus } from "../api/types";
import { measureErrorText, measurePhase, runningText } from "./abMeasure";

const status = (over: Partial<AbStatus> = {}): AbStatus => ({
  enabled: true,
  current: "a",
  blind: false,
  offset_db: 0,
  gain_a_db: 0,
  gain_b_db: 0,
  headroom_db: 0,
  overs: { a: 0, b: 0 },
  level: { mode: "analytic", weighting: "k" },
  measuring: null,
  measure_error: null,
  error: null,
  ...over,
});

describe("measured level matching", () => {
  it("walks from waiting through the minimum and settling to locking", () => {
    const m = (seconds: number, offset_db: number | null, stable = false, max_seconds = 60) => runningText({ seconds, offset_db, stable, max_seconds });
    const waiting = m(0, null);
    expect(waiting).toMatchObject({ step: "waiting", fraction: 0, canAccept: false });
    const early = m(8, -0.8);
    expect(early).toMatchObject({ step: "minimum", canAccept: true });
    expect(early.text).toBe("Listening: 8 s of the 20 s minimum.");
    expect(early.fraction).toBeCloseTo(8 / 30);
    const settling = m(24, -0.84);
    expect(settling.step).toBe("settling");
    expect(settling.text).toContain("24 s so far");
    expect(settling.text).toContain("or at 60 s");
    // The bar never reads full before the engine says it holds.
    expect(m(45, -0.84).fraction).toBe(0.99);
    expect(m(31, -0.84, true)).toMatchObject({ step: "locking", fraction: 1 });
    // A short cap shortens the bar and the minimum.
    expect(m(5, -1, false, 10)).toMatchObject({ step: "minimum", fraction: 0.5 });
    expect(m(5, -1, false, 10).text).toContain("of the 10 s minimum");
  });

  it("says why a run ended in plain words", () => {
    expect(measureErrorText("no match: B stayed silent over 60 s of A")).toBe("No match: side B stayed silent over 60 s of A.");
    expect(measureErrorText("cancelled: the track changed")).toBe("Cancelled: the track changed.");
    expect(measureErrorText("  ")).toBe("The measurement stopped.");
  });

  it("reads the phase from A/B status and the level in force", () => {
    expect(measurePhase(null, null)).toEqual({ phase: "idle" });
    expect(measurePhase(status(), { mode: "analytic", weighting: "k" })).toEqual({ phase: "idle" });
    expect(measurePhase(status({ measuring: { seconds: 3, offset_db: -1, stable: false, max_seconds: 60 } }), { mode: "measured", offset_db: -2, seconds: 30 }).phase).toBe("running");
    expect(measurePhase(status({ measure_error: "cancelled: the sample rate changed" }), null)).toEqual({ phase: "failed", text: "Cancelled: the sample rate changed." });
    const locked = measurePhase(status(), { mode: "measured", offset_db: -0.84, seconds: 31.5 });
    expect(locked).toMatchObject({ phase: "locked", offsetDb: -0.84, seconds: 31.5 });
    if (locked.phase === "locked") expect(locked.text).toBe("Measured: B −0.84 dB over 31.5 s of this track");
  });
});
