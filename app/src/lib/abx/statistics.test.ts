// Copied from audivane/abx 1.1.2 (35bd3aa, `packages/experiment/test/statistics.test.ts`),
// AGPL-3.0-only, same author; keep in sync by hand.
import assert from "node:assert/strict";
import { test } from "vitest";

import { CHANCE, CI_DECIMALS, CI_ABS_TOLERANCE, HOLM_DECIMALS, P_VALUE_ABS_TOLERANCE, binomialCi, binomTail, holmBonferroni, isSignificant, minCorrectForSignificance, oneTailedPValue } from "./statistics";

function closeTo(actual: number, expected: number, tolerance: number): void {
  assert.ok(
    Math.abs(actual - expected) <= tolerance,
    `expected ${actual} to be within ${tolerance} of ${expected}`,
  );
}

test("published ABX tables agree within 1e-12", () => {
  const expected: ReadonlyArray<readonly [number, number, number]> = [
    [8, 7, 0.03515625],
    [8, 8, 0.00390625],
    [10, 8, 0.0546875],
    [10, 9, 0.0107421875],
    [13, 10, 0.046142578125],
    [16, 12, 0.0384063720703125],
    [16, 13, 0.0106353759765625],
    [20, 15, 0.020694732666015625],
    [25, 18, 0.021642625331878662],
    [10, 10, 0.0009765625],
    [1, 1, 0.5],
    [2, 2, 0.25],
  ];
  for (const [n, k, value] of expected) {
    closeTo(oneTailedPValue(n, k), value, P_VALUE_ABS_TOLERANCE);
  }
});

test("exhaustive small-n tails match direct binomial arithmetic", () => {
  // Independent direct expansion of sum_{i=k..n} C(n,i) p^i (1-p)^(n-i).
  for (let n = 0; n <= 24; n += 1) {
    for (let k = 0; k <= n; k += 1) {
      let direct = 0;
      let combination = 1;
      for (let i = 0; i <= n; i += 1) {
        if (i >= k) direct += combination * CHANCE ** i * (1 - CHANCE) ** (n - i);
        combination = (combination * (n - i)) / (i + 1);
      }
      closeTo(oneTailedPValue(n, k), direct, P_VALUE_ABS_TOLERANCE);
      closeTo(binomTail(n, k, CHANCE), direct, P_VALUE_ABS_TOLERANCE);
    }
  }
});

test("thresholds are the smallest strictly passing count", () => {
  const expected: ReadonlyArray<readonly [number, number | null]> = [
    [0, null],
    [1, null],
    [2, null],
    [3, null],
    [5, 5],
    [8, 7],
    [10, 9],
    [13, 10],
    [20, 15],
    [60, 37],
  ];
  for (const [n, threshold] of expected) {
    assert.equal(minCorrectForSignificance(n), threshold, `n=${n}`);
    if (threshold !== null) {
      assert.equal(isSignificant(n, threshold), true);
      assert.equal(isSignificant(n, threshold - 1), false);
    }
  }
  for (let n = 3; n <= 60; n += 1) {
    const threshold = minCorrectForSignificance(n);
    if (threshold === null) {
      continue;
    }
    assert.equal(isSignificant(n, threshold), true);
    assert.equal(isSignificant(n, threshold - 1), false);
  }
  assert.equal(minCorrectForSignificance(1), null);
  assert.equal(minCorrectForSignificance(2), null);
});

test("alpha equality uses strict p < alpha, never <=", () => {
  const boundaries: ReadonlyArray<readonly [number, number, number]> = [
    [1, 1, 0.5],
    [2, 2, 0.25],
    [3, 3, 0.125],
    [4, 4, 0.0625],
    [5, 5, 0.03125],
    [5, 4, 0.1875],
  ];
  for (const [n, k, alpha] of boundaries) {
    const p = oneTailedPValue(n, k);
    assert.equal(p, alpha, "exact dyadic tail should be represented exactly");
    assert.equal(isSignificant(n, k, alpha), false, "exact equality must not pass");
    assert.equal(
      isSignificant(n, k, alpha - 1e-15),
      false,
      "a smaller alpha cannot make the same p significant",
    );
    assert.equal(
      isSignificant(n, k, alpha + 1e-15),
      true,
      "a slightly larger alpha makes the same p strictly significant",
    );
    const threshold = minCorrectForSignificance(n, alpha);
    const thresholdBelow = minCorrectForSignificance(n, alpha - 1e-15);
    const thresholdAbove = minCorrectForSignificance(n, alpha + 1e-15);
    if (threshold !== null) {
      assert.equal(isSignificant(n, threshold, alpha), true);
      assert.equal(isSignificant(n, threshold - 1, alpha), false);
    }
    assert.equal(thresholdBelow, threshold);
    assert.equal(thresholdAbove, k);
  }
});

test("Clopper-Pearson intervals match known values and endpoints", () => {
  const expected: ReadonlyArray<readonly [number, number, number, number, number]> = [
    [0, 0, 0.95, 0, 1],
    [1, 0, 0.95, 0, 0.975],
    [1, 1, 0.95, 0.025, 1],
    [10, 0, 0.95, 0, 0.308497],
    [10, 10, 0.95, 0.691503, 1],
    [10, 7, 0.95, 0.347547, 0.93326],
    [10, 7, 0.9, 0.393376, 0.912736],
    [16, 13, 0.95, 0.543543, 0.959526],
  ];
  for (const [n, k, confidence, lower, upper] of expected) {
    const interval = binomialCi(n, k, confidence);
    closeTo(interval[0], lower, CI_ABS_TOLERANCE);
    closeTo(interval[1], upper, CI_ABS_TOLERANCE);
    assert.ok(interval[0] >= 0 && interval[0] <= interval[1] && interval[1] <= 1);
    assert.match(String(interval[0]), new RegExp(`^\\d+(\\.\\d{0,${CI_DECIMALS}})?$`));
    assert.match(String(interval[1]), new RegExp(`^\\d+(\\.\\d{0,${CI_DECIMALS}})?$`));
  }

  assert.deepEqual(binomialCi(1, 0), [0, 0.975]);
  assert.deepEqual(binomialCi(1, 1), [0.025, 1]);
  assert.deepEqual(binomialCi(10, 10), [0.691503, 1]);
  assert.deepEqual(binomialCi(16, 13), [0.543543, 0.959526]);
  assert.deepEqual(binomialCi(0, 0), [0, 1]);
});

test("Holm correction preserves ordering, rounding and decisions", () => {
  const families: ReadonlyArray<{
    readonly pValues: readonly number[];
    readonly alpha: number;
    readonly results: readonly { index: number; adjustedP: number; reject: boolean }[];
  }> = [
    { pValues: [], alpha: 0.05, results: [] },
    { pValues: [0.001], alpha: 0.05, results: [{ index: 0, adjustedP: 0.001, reject: true }] },
    { pValues: [0.05], alpha: 0.05, results: [{ index: 0, adjustedP: 0.05, reject: false }] },
    {
      pValues: [0.001, 0.02, 0.04, 0.5],
      alpha: 0.05,
      results: [
        { index: 0, adjustedP: 0.004, reject: true },
        { index: 1, adjustedP: 0.06, reject: false },
        { index: 2, adjustedP: 0.08, reject: false },
        { index: 3, adjustedP: 0.5, reject: false },
      ],
    },
    {
      pValues: [0.01, 0.01, 0.01, 0.2],
      alpha: 0.05,
      results: [
        { index: 0, adjustedP: 0.04, reject: true },
        { index: 1, adjustedP: 0.04, reject: true },
        { index: 2, adjustedP: 0.04, reject: true },
        { index: 3, adjustedP: 0.2, reject: false },
      ],
    },
    {
      pValues: [0.01, 0.04, 0.03, 0.002],
      alpha: 0.1,
      results: [
        { index: 3, adjustedP: 0.008, reject: true },
        { index: 0, adjustedP: 0.03, reject: true },
        { index: 2, adjustedP: 0.06, reject: true },
        { index: 1, adjustedP: 0.06, reject: true },
      ],
    },
    {
      pValues: [0.2, 0.1, 0.05],
      alpha: 0.05,
      results: [
        { index: 2, adjustedP: 0.15, reject: false },
        { index: 1, adjustedP: 0.2, reject: false },
        { index: 0, adjustedP: 0.2, reject: false },
      ],
    },
  ];
  for (const family of families) {
    const input = family.pValues.slice();
    const result = holmBonferroni(input, family.alpha);
    assert.deepEqual(result, family.results);
    assert.deepEqual(input, family.pValues, "input family must not be mutated");
  }

  const ties = holmBonferroni([0.01, 0.01, 0.01, 0.2]);
  assert.deepEqual(
    ties.map((entry) => entry.index),
    [0, 1, 2, 3],
    "stable ordering preserves family order for equal p-values",
  );
  assert.equal(ties[0]?.adjustedP, 0.04);
  assert.equal(ties[1]?.adjustedP, 0.04);
  assert.equal(ties[2]?.adjustedP, 0.04);
  assert.equal(HOLM_DECIMALS, 8);
});

