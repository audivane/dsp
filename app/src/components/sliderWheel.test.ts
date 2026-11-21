import { beforeEach, describe, expect, it } from "vitest";
import {
  POINTER_REST_MS,
  SCROLL_GUARD_MS,
  WHEEL_IDLE_MS,
  WheelAccumulator,
  canSliderTakeWheel,
  deltaToNotchPx,
  recordScroll,
  resetScrollGuard,
  wasAncestorScrolledRecently,
  wheelDirection,
} from "./sliderWheel";

describe("deltaToNotchPx and wheelDirection", () => {
  it("normalises pixel, 120px discrete, line and page wheel units to one notch", () => {
    expect(deltaToNotchPx(-100, 0)).toBe(-100);
    expect(deltaToNotchPx(100, 0)).toBe(100);
    expect(deltaToNotchPx(-120, 0)).toBe(-100);
    expect(deltaToNotchPx(240, 0)).toBe(200);
    expect(deltaToNotchPx(-3, 1)).toBeCloseTo(-100, 6);
    expect(deltaToNotchPx(1, 2)).toBe(100);
    expect(deltaToNotchPx(0, 0)).toBe(0);
    expect(deltaToNotchPx(Number.NaN, 0)).toBe(0);
  });

  it("ignores zero or horizontal-dominant wheel gestures", () => {
    expect(wheelDirection(0, 0)).toBe(0);
    expect(wheelDirection(0, 50)).toBe(0);
    expect(wheelDirection(-10, 40)).toBe(0);
    expect(wheelDirection(-40, 10)).toBe(1);
    expect(wheelDirection(40, -10)).toBe(-1);
  });
});

describe("WheelAccumulator", () => {
  it("emits one step per discrete notch (100px, 120px, 3 lines)", () => {
    const acc = new WheelAccumulator();
    expect(acc.push(-100, 0, 1000)).toEqual({ steps: 1, dir: 1 });
    expect(acc.push(100, 0, 1050)).toEqual({ steps: -1, dir: -1 });
    // Five 120px clicks in a row never emit a phantom 6th step.
    let total = 0;
    for (let i = 0; i < 5; i++) {
      total += acc.push(-120, 0, 2000 + i * 50).steps;
    }
    expect(total).toBe(5);
    expect(acc.push(-3, 1, 3000)).toEqual({ steps: 1, dir: 1 });
    expect(acc.push(1, 2, 3100)).toEqual({ steps: -1, dir: -1 });
  });

  it("accumulates fractional touchpad deltas into whole steps", () => {
    const acc = new WheelAccumulator();
    for (let i = 0; i < 4; i++) {
      expect(acc.push(-20, 0, 1000 + i * 16)).toEqual({ steps: 0, dir: 1 });
    }
    // 5th event hits 100px -> 1 step.
    expect(acc.push(-20, 0, 1064)).toEqual({ steps: 1, dir: 1 });
    // Carry remainder: 3 × 35px = 105px -> 1 step + 5px remainder, then 95px -> 1 step.
    expect(acc.push(35, 0, 1200)).toEqual({ steps: 0, dir: -1 });
    expect(acc.push(35, 0, 1216)).toEqual({ steps: 0, dir: -1 });
    expect(acc.push(35, 0, 1232)).toEqual({ steps: -1, dir: -1 });
    expect(acc.push(95, 0, 1248)).toEqual({ steps: -1, dir: -1 });
  });

  it("clears partial accumulation on direction reversal or idle gap", () => {
    const acc = new WheelAccumulator();
    expect(acc.push(-70, 0, 1000)).toEqual({ steps: 0, dir: 1 });
    // Reversing immediately clears the +70 remainder.
    expect(acc.push(100, 0, 1020)).toEqual({ steps: -1, dir: -1 });
    // Partial accumulation followed by an idle pause starts fresh.
    expect(acc.push(-80, 0, 2000)).toEqual({ steps: 0, dir: 1 });
    expect(acc.push(-50, 0, 2000 + WHEEL_IDLE_MS + 10)).toEqual({ steps: 0, dir: 1 });
    expect(acc.push(-50, 0, 2000 + WHEEL_IDLE_MS + 30)).toEqual({ steps: 1, dir: 1 });
  });
});

describe("canSliderTakeWheel and ancestor scroll guard", () => {
  beforeEach(() => {
    resetScrollGuard();
  });

  it("allows wheel immediately when non-scrollable or focused", () => {
    expect(
      canSliderTakeWheel({
        focused: false,
        scrollableAncestor: false,
        restSince: null,
        ancestorScrolledRecently: true,
        now: 1000,
      }),
    ).toBe(true);
    expect(
      canSliderTakeWheel({
        focused: true,
        scrollableAncestor: true,
        restSince: null,
        ancestorScrolledRecently: true,
        now: 1000,
      }),
    ).toBe(true);
  });

  it("requires pointer rest >= 300 ms with no recent scroll when unfocused in a scrollable container", () => {
    // Pointer never rested on slider -> page scrolls.
    expect(
      canSliderTakeWheel({
        focused: false,
        scrollableAncestor: true,
        restSince: null,
        ancestorScrolledRecently: false,
        now: 1000,
      }),
    ).toBe(false);
    // Pointer just entered (< 300 ms) -> page scrolls.
    expect(
      canSliderTakeWheel({
        focused: false,
        scrollableAncestor: true,
        restSince: 1000,
        ancestorScrolledRecently: false,
        now: 1000 + POINTER_REST_MS - 1,
      }),
    ).toBe(false);
    // Pointer rested >= 300 ms with no scroll -> slider takes wheel.
    expect(
      canSliderTakeWheel({
        focused: false,
        scrollableAncestor: true,
        restSince: 1000,
        ancestorScrolledRecently: false,
        now: 1000 + POINTER_REST_MS,
      }),
    ).toBe(true);
    // A page scroll restarts the 300 ms guard even if pointer rested earlier.
    expect(
      canSliderTakeWheel({
        focused: false,
        scrollableAncestor: true,
        restSince: 500,
        ancestorScrolledRecently: true,
        now: 1200,
      }),
    ).toBe(false);
  });

  it("blocks wheel events on descendants of a scrolled container for SCROLL_GUARD_MS", () => {
    const main = { parentNode: null };
    const panel = { parentNode: main };
    const slider = { parentNode: panel };
    const topBar = { parentNode: null };
    const topSlider = { parentNode: topBar };

    expect(wasAncestorScrolledRecently(slider, 1000)).toBe(false);
    recordScroll(main as unknown as EventTarget, 1000);
    expect(wasAncestorScrolledRecently(slider, 1050)).toBe(true);
    expect(wasAncestorScrolledRecently(slider, 1000 + SCROLL_GUARD_MS - 1)).toBe(true);
    // An unrelated container (such as the top bar) is not blocked by `.main` scrolling.
    expect(wasAncestorScrolledRecently(topSlider, 1050)).toBe(false);
    // After the guard window expires, wheeling works again.
    expect(wasAncestorScrolledRecently(slider, 1000 + SCROLL_GUARD_MS)).toBe(false);
  });

  it("blocks all sliders briefly after a document/window scroll", () => {
    const slider = { parentNode: null };
    recordScroll(null, 500);
    expect(wasAncestorScrolledRecently(slider, 500 + SCROLL_GUARD_MS - 1)).toBe(true);
    expect(wasAncestorScrolledRecently(slider, 500 + SCROLL_GUARD_MS)).toBe(false);
  });
});
