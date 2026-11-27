import { describe, expect, it } from "vitest";
import { createSecret, commitSecret, secretToHex, xSequence } from "./abx/commitment";
import { clampTrials, EXPORT_FORMAT, neededText, preferNeeded, scoreTrial, twoSidedP, verifyExport, type BlindExport, type TrialRecord } from "./abBlind";

const rec = (index: number, truth: "A" | "B", choice: TrialRecord["choice"], mode: "abx" | "identify" | "prefer"): TrialRecord => ({
  index,
  truth,
  choice,
  ...scoreTrial(mode, truth, choice),
  ms: 1000,
  switches: 2,
});

describe("blind test", () => {

  it("scores answers", () => {
    expect(scoreTrial("abx", "A", "A")).toEqual({ answer: "A", correct: true });
    expect(scoreTrial("abx", "A", "B")).toEqual({ answer: "B", correct: false });
    // Button 1 plays B: picking 1 as B is right.
    expect(scoreTrial("identify", "B", "1")).toEqual({ answer: "B", correct: true });
    expect(scoreTrial("identify", "B", "2")).toEqual({ answer: "A", correct: false });
    expect(scoreTrial("prefer", "A", "2")).toEqual({ answer: "B", correct: null });
  });

  it("plans the trial count and says what passes", () => {
    expect([clampTrials(0), clampTrials(500), clampTrials(15.6), clampTrials(NaN)]).toEqual([1, 100, 16, 16]);
    expect(neededText({ mode: "abx", trials: 16, feedback: "end" })).toBe("12 of 16 correct needed to pass (α = 0.05).");
    expect(neededText({ mode: "abx", trials: 4, feedback: "end" })).toMatch(/cannot pass/);
    expect(preferNeeded(16)).toBe(13);
    expect(preferNeeded(5)).toBeNull();
    expect(twoSidedP(16, 8)).toBe(1);
  });

  it("verifies an export against its commitment", async () => {
    const secret = createSecret();
    const truths = await xSequence(secret, 3);
    const e: BlindExport = {
      format: EXPORT_FORMAT,
      mode: "abx",
      question: "Is X A or B?",
      feedback: "end",
      planned_trials: 3,
      outcome: "completed",
      abandoned_reason: null,
      started_at: "",
      ended_at: "",
      side_a: "A",
      side_b: "B",
      level: null,
      commitment: await commitSecret(secret),
      secret: secretToHex(secret),
      trials: truths.map((t, i) => rec(i + 1, t, "A", "abx")),
      result: null,
    };
    expect(await verifyExport(e)).toBe(true);
    const flipped = { ...e, trials: e.trials.map((t, i) => (i === 1 ? { ...t, truth: t.truth === "A" ? ("B" as const) : ("A" as const) } : t)) };
    expect(await verifyExport(flipped)).toBe(false);
  });
});
