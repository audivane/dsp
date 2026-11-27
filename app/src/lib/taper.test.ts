import { describe, expect, it } from "vitest";
import { keyStepDb, volumeTaper, wheelStepDb } from "./taper";

/** Every 0.1 dB across the range, the resolution both sliders work at. */
function tenths(min: number, max: number): number[] {
  const out: number[] = [];
  for (let d = min; d <= max + 1e-9; d += 0.1) out.push(Number(d.toFixed(1)));
  return out;
}

describe("volume taper", () => {
  const t = volumeTaper(-80);

  it("round-trips every 0.1 dB step to better than 0.05 dB", () => {
    for (const db of tenths(-80, 0)) {
      expect(Math.abs(t.db(t.pos(db)) - db)).toBeLessThan(0.05);
    }
  });

  it("clamps what is out of range", () => {
    expect(t.pos(-200)).toBe(0);
    expect(t.pos(12)).toBe(1);
    expect(t.db(-1)).toBe(-80);
    expect(t.db(2)).toBe(0);
  });

});

describe("keyboard, wheel and typed steps", () => {
  // A daemon reporting the card's -64 dB floor while digital attenuation
  // holds the volume at -90 dB.
  const t = volumeTaper(-64);

  it("Arrow Down, Shift+Arrow Down, Home and wheel-down below the floor are never a rise", () => {
    for (const [key, shift] of [["ArrowDown", false], ["ArrowDown", true], ["ArrowLeft", false], ["Home", false]] as const) {
      const db = keyStepDb(key, shift, -90, t);
      expect(db, key).not.toBeNull();
      expect(db!, `${key} shift=${shift}`).toBeLessThanOrEqual(-90);
    }
    expect(wheelStepDb(100, false, -90, t)).toBeLessThanOrEqual(-90);
    expect(wheelStepDb(100, true, -90, t)).toBeLessThanOrEqual(-90);
    // Up still goes up (into the range).
    expect(keyStepDb("ArrowUp", false, -90, t)).toBeGreaterThanOrEqual(-90);
    expect(keyStepDb("Tab", false, -90, t)).toBeNull();
  });

});

describe("wheel direction and page keys", () => {
  const t = volumeTaper(-64);

  it("Page Up and Page Down move 6 dB, never against their direction", () => {
    expect(keyStepDb("PageDown", false, -20, t)).toBe(-26);
    expect(keyStepDb("PageUp", false, -20, t)).toBe(-14);
    expect(keyStepDb("PageUp", false, -3, t)).toBe(0);
    expect(keyStepDb("PageDown", false, -62, t)).toBe(-64);
    // Below the slider's floor: Page Down is never a rise.
    expect(keyStepDb("PageDown", false, -90, t)!).toBeLessThanOrEqual(-90);
  });
});
