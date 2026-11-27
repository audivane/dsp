// Blind tests in the A/B view, on whatever plays through the engine: ABX
// ("is X A or B?") and blind A/B (two shuffled buttons 1 and 2, "which is
// B?" or "which do you prefer?"). The app owns the run: it draws the hidden
// sides from a committed secret (`abx/commitment`), switches with
// `ab_select` and scores with `abx/statistics`; the engine only hides the
// side from other clients (`ab_blind`).

import type { AbSide } from "../api/types";
import { binomialCi, minCorrectForSignificance, oneTailedPValue } from "./abx/statistics";
import { verifyReveal, type DisplayAnswer } from "./abx/commitment";

export type BlindMode = "abx" | "identify" | "prefer";
/** When the answers show: after the last trial, or right after each one (practice). */
export type BlindFeedback = "end" | "each";
export type BlindButton = "a" | "b" | "x" | "1" | "2";
/** An answer: "X is A/B" in ABX, button 1 or 2 in blind A/B. */
export type BlindChoice = "A" | "B" | "1" | "2";

export const TRIALS_DEFAULT = 16;
export const TRIALS_MIN = 1;
export const TRIALS_MAX = 100;
export const ALPHA = 0.05;
export const EXPORT_FORMAT = "audivane-blind-1";

export interface BlindPlan {
  mode: BlindMode;
  trials: number;
  feedback: BlindFeedback;
}

export const MODE_TEXT: Record<BlindMode, { title: string; question: string }> = {
  abx: { title: "ABX", question: "Is X A or B?" },
  identify: { title: "Blind A/B: which is B?", question: "Which one is B?" },
  prefer: { title: "Blind A/B: preference", question: "Which one do you prefer?" },
};

export const FEEDBACK_TEXT: Record<BlindFeedback, string> = {
  end: "At the end",
  each: "After each trial (practice)",
};

/** One answered trial. */
export interface TrialRecord {
  /** 1-based, as in the commitment. */
  index: number;
  /** The hidden side: X in ABX, the side on button 1 in blind A/B. */
  truth: DisplayAnswer;
  /** What was pressed: "A"/"B" for "X is A/B", "1"/"2" in blind A/B. */
  choice: BlindChoice;
  /** The side the answer names: X's side in ABX, the side picked as B or preferred in blind A/B. */
  answer: DisplayAnswer;
  /** Null in a preference test, which has no right answer. */
  correct: boolean | null;
  /** From the trial's start to the answer. */
  ms: number;
  /** Switches made during the trial. */
  switches: number;
}

export function clampTrials(n: number): number {
  if (!Number.isFinite(n)) return TRIALS_DEFAULT;
  return Math.min(TRIALS_MAX, Math.max(TRIALS_MIN, Math.round(n)));
}

/** The side a button plays in a trial whose hidden side is `truth`. */
export function sideFor(mode: BlindMode, truth: DisplayAnswer, button: BlindButton): AbSide {
  const t: AbSide = truth === "A" ? "a" : "b";
  const other: AbSide = t === "a" ? "b" : "a";
  if (mode === "abx") return button === "x" ? t : button === "b" ? "b" : "a";
  return button === "1" ? t : other;
}

/** The buttons a mode plays. */
export function buttonsFor(mode: BlindMode): BlindButton[] {
  return mode === "abx" ? ["a", "b", "x"] : ["1", "2"];
}

export function choicesFor(mode: BlindMode): BlindChoice[] {
  return mode === "abx" ? ["A", "B"] : ["1", "2"];
}

/** Score one answer. */
export function scoreTrial(mode: BlindMode, truth: DisplayAnswer, choice: BlindChoice): { answer: DisplayAnswer; correct: boolean | null } {
  if (mode === "abx") {
    const answer: DisplayAnswer = choice === "B" ? "B" : "A";
    return { answer, correct: answer === truth };
  }
  const side = sideFor(mode, truth, choice === "2" ? "2" : "1");
  const answer: DisplayAnswer = side === "a" ? "A" : "B";
  return { answer, correct: mode === "identify" ? answer === "B" : null };
}

/**
 * Correct answers needed out of `n` for p < α (exact one-tailed binomial),
 * or null when `n` is too small to ever pass.
 */
export function neededCorrect(n: number): number | null {
  return minCorrectForSignificance(clampTrials(n), ALPHA);
}

export function neededText(plan: BlindPlan): string {
  const n = clampTrials(plan.trials);
  if (plan.mode === "prefer") {
    const k = preferNeeded(n);
    return k === null ? `${n} trials cannot show a preference at α = 0.05: take at least 6.` : `A preference shows at α = 0.05 with ${k} of ${n} the same way.`;
  }
  const k = neededCorrect(n);
  return k === null ? `${n} trials cannot pass at α = 0.05: take at least 5.` : `${k} of ${n} correct needed to pass (α = 0.05).`;
}

/** Two-sided exact binomial p against 0.5 for `k` of `n`. */
export function twoSidedP(n: number, k: number): number {
  if (n === 0) return 1;
  return Math.min(1, 2 * Math.min(oneTailedPValue(n, k), oneTailedPValue(n, n - k)));
}

/** Smallest count one way that shows a two-sided preference, or null. */
export function preferNeeded(n: number): number | null {
  for (let k = Math.ceil(n / 2); k <= n; k++) if (k > 0 && twoSidedP(n, k) < ALPHA) return k;
  return null;
}

export interface BlindResult {
  n: number;
  /** Correct answers (ABX, which is B), or trials that preferred B. */
  k: number;
  p: number;
  ci: readonly [number, number];
  /** Correct (or same-way) answers the run needed; null when it could not pass. */
  needed: number | null;
  pass: boolean;
  /** Preference only: which side won, when it passed. */
  preferred: DisplayAnswer | null;
}

export function scoreRun(mode: BlindMode, trials: readonly TrialRecord[]): BlindResult {
  const n = trials.length;
  if (mode === "prefer") {
    const k = trials.filter((t) => t.answer === "B").length;
    const p = twoSidedP(n, k);
    const pass = p < ALPHA;
    return { n, k, p, ci: binomialCi(n, k), needed: preferNeeded(n), pass, preferred: pass ? (k * 2 > n ? "B" : "A") : null };
  }
  const k = trials.filter((t) => t.correct).length;
  const p = n === 0 ? 1 : oneTailedPValue(n, k);
  return { n, k, p, ci: binomialCi(n, k), needed: n === 0 ? null : minCorrectForSignificance(n, ALPHA), pass: p < ALPHA, preferred: null };
}

export function fmtP(p: number): string {
  if (p < 0.0001) return "p < 0.0001";
  return `p = ${p < 0.01 ? p.toFixed(4) : p.toFixed(3)}`;
}

export function fmtCi(ci: readonly [number, number]): string {
  return `${Math.round(ci[0] * 100)}–${Math.round(ci[1] * 100)} %`;
}

export function fmtTrialMs(ms: number): string {
  return ms < 60_000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.floor(ms / 60_000)} min ${Math.round((ms % 60_000) / 1000)} s`;
}

/** The running score shown after each trial in practice runs. */
export function runningText(mode: BlindMode, trials: readonly TrialRecord[]): string {
  if (mode === "prefer") {
    const b = trials.filter((t) => t.answer === "B").length;
    return `B preferred ${b} of ${trials.length}`;
  }
  return `${trials.filter((t) => t.correct).length} of ${trials.length} correct`;
}

/** What the JSON export holds: enough to check the run against its commitment. */
export interface BlindExport {
  format: typeof EXPORT_FORMAT;
  mode: BlindMode;
  question: string;
  feedback: BlindFeedback;
  planned_trials: number;
  outcome: "completed" | "abandoned";
  abandoned_reason: string | null;
  started_at: string;
  ended_at: string;
  side_a: string;
  side_b: string;
  level: unknown;
  /** hex(SHA-256("ABX1\0" || secret)), shown before the first trial. */
  commitment: string;
  /** Revealed at the end; with the commitment it proves the hidden sides were fixed beforehand. */
  secret: string;
  /** "truth" of trial i is X's side (ABX) or button 1's side (blind A/B): low bit of SHA-256(secret || uint64BE(i)), 0 → A. */
  trials: TrialRecord[];
  result: BlindResult | null;
}

/** True when the export's hidden sides match its commitment and secret. */
export async function verifyExport(e: BlindExport): Promise<boolean> {
  if (e.format !== EXPORT_FORMAT) return false;
  return verifyReveal(
    e.secret,
    e.commitment,
    e.trials.map((t) => t.truth),
  );
}
