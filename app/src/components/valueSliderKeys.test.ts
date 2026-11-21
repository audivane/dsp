import { expect, it } from "vitest";
import { keyStepValue, stepValue } from "./valueSliderKeys";

it("an arrow key moves one displayed step, also on a log slider", () => {
  const cutoff = { min: 300, max: 2000, step: 5 };
  expect(keyStepValue("ArrowRight", 700, cutoff)).toBe(705);
  expect(keyStepValue("ArrowUp", 700, cutoff)).toBe(705);
  expect(keyStepValue("ArrowLeft", 700, cutoff)).toBe(695);
  expect(keyStepValue("PageUp", 700, cutoff)).toBe(750);
  expect(keyStepValue("Home", 700, cutoff)).toBe(300);
  expect(keyStepValue("End", 700, cutoff)).toBe(2000);
  expect(keyStepValue("ArrowRight", 2000, cutoff)).toBe(2000);
  // An off-grid value (Linkwitz 796.4 Hz) steps to the next grid point in that direction.
  expect(keyStepValue("ArrowRight", 796.4, cutoff)).toBe(800);
  expect(keyStepValue("ArrowLeft", 796.4, cutoff)).toBe(795);
  expect(keyStepValue("Tab", 700, cutoff)).toBeNull();
  expect(keyStepValue("ArrowRight", -0.5, { min: -6, max: 0, step: 0.1 })).toBe(-0.4);
});

it("never steps past a bound that is not on the grid", () => {
  // Tilt slope on a 20 Hz–20 kHz band: max 24 / log2(1000) = 2.408239965… dB/oct.
  const max = 24 / Math.log2(1000);
  const slope = { min: -max, max, step: 0.01 };
  const up = keyStepValue("ArrowRight", 2.4, slope)!;
  expect(up).toBeLessThanOrEqual(max);
  expect(up).toBeGreaterThan(2.4);
  expect(keyStepValue("End", 0, slope)!).toBeLessThanOrEqual(max);
  expect(keyStepValue("ArrowLeft", -2.4, slope)!).toBeGreaterThanOrEqual(-max);
});

it("stepValue moves by whole displayed steps and clamps cleanly", () => {
  const cutoff = { min: 300, max: 2000, step: 5 };
  expect(stepValue(0, 700, cutoff)).toBe(700);
  expect(stepValue(1, 700, cutoff)).toBe(705);
  expect(stepValue(3, 700, cutoff)).toBe(715);
  expect(stepValue(-2, 700, cutoff)).toBe(690);
  expect(stepValue(1, 2000, cutoff)).toBe(2000);
  expect(stepValue(-1, 300, cutoff)).toBe(300);
  expect(stepValue(1, 796.4, cutoff)).toBe(800);
  expect(stepValue(-1, 796.4, cutoff)).toBe(795);
});
