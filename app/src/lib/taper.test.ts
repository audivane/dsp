import { describe, expect, it } from "vitest";
import { balanceTaper, keyStepDb, quantiseDb, stepDb, volumeSliderTaper, volumeTaper, wheelStepDb } from "./taper";
import volumeTaperVectors from "./__fixtures__/volume-taper.json";

/** Every 0.1 dB across the range, the resolution both sliders work at. */
function tenths(min: number, max: number): number[] {
  const out: number[] = [];
  for (let d = min; d <= max + 1e-9; d += 0.1) out.push(Number(d.toFixed(1)));
  return out;
}

describe("volume taper", () => {
  const t = volumeTaper(-80);

  it("gives -40..0 dB the top 80% of the travel", () => {
    expect(t.min).toBe(-80);
    expect(t.max).toBe(0);
    expect(t.pos(-40)).toBeCloseTo(0.2, 9);
    expect(t.pos(0)).toBeCloseTo(1, 9);
    expect(t.pos(-80)).toBeCloseTo(0, 9);
    // Halfway along the lower fifth (-80..-40) is -60; halfway along the upper four-fifths (-40..0) is -20.
    expect(t.db(0.1)).toBeCloseTo(-60, 9);
    expect(t.db(0.6)).toBeCloseTo(-20, 9);
    expect(t.db(0)).toBe(-80);
    expect(t.db(1)).toBe(0);
  });

  it("round-trips every 0.1 dB step to better than 0.05 dB", () => {
    for (const db of tenths(-80, 0)) {
      expect(Math.abs(t.db(t.pos(db)) - db)).toBeLessThan(0.05);
    }
  });

  it("rises all the way across the travel", () => {
    let prev = -Infinity;
    for (let i = 0; i <= 1000; i++) {
      const db = t.db(i / 1000);
      expect(db).toBeGreaterThanOrEqual(prev);
      prev = db;
    }
    expect(prev).toBe(0);
  });

  it("follows the engine's own minimum", () => {
    const deep = volumeTaper(-120);
    expect(deep.pos(-40)).toBeCloseTo(0.2, 9);
    expect(deep.db(0.2)).toBeCloseTo(-40, 9);
    expect(deep.db(0)).toBe(-120);
    const shallow = volumeTaper(-60);
    expect(shallow.db(0.2)).toBeCloseTo(-40, 9);
    expect(shallow.pos(-60)).toBe(0);
  });

  it("clamps what is out of range", () => {
    expect(t.pos(-200)).toBe(0);
    expect(t.pos(12)).toBe(1);
    expect(t.db(-1)).toBe(-80);
    expect(t.db(2)).toBe(0);
  });

  it("quantises to 0.1 dB inside the range", () => {
    expect(quantiseDb(-12.345, t)).toBe(-12.3);
    expect(quantiseDb(-12.36, t)).toBe(-12.4);
    expect(quantiseDb(-500, t)).toBe(-80);
    expect(quantiseDb(3, t)).toBe(0);
  });

  it("matches the shared test vector pinned with Rust plasma_osd", () => {
    for (const v of volumeTaperVectors) {
      const pos = volumeSliderTaper(v.min_db, v.max_db, v.db).pos(v.db);
      expect(pos, `${v.db} dB in [${v.min_db}, ${v.max_db}]`).toBeCloseTo(v.pos, 9);
      expect(Math.round(pos * 100), `percent for ${v.db} dB in [${v.min_db}, ${v.max_db}]`).toBe(v.percent);
    }
  });
});

describe("balance taper", () => {
  const t = balanceTaper(24);

  it("puts 0 dB in the centre and ±3 dB at 70% of the travel", () => {
    expect(t.min).toBe(-24);
    expect(t.max).toBe(24);
    expect(t.pos(0)).toBeCloseTo(0.5, 9);
    expect(t.pos(3)).toBeCloseTo(0.85, 9);
    expect(t.pos(-3)).toBeCloseTo(0.15, 9);
    expect(t.pos(24)).toBeCloseTo(1, 9);
    expect(t.pos(-24)).toBeCloseTo(0, 9);
    expect(t.db(0.5)).toBeCloseTo(0, 9);
    expect(t.db(0.85)).toBeCloseTo(3, 9);
    expect(t.db(0.15)).toBeCloseTo(-3, 9);
    expect(t.db(1)).toBe(24);
    expect(t.db(0)).toBe(-24);
    // Centred, so the numbers read the same either side.
    expect(t.db(0.5 + 0.1)).toBeCloseTo(-t.db(0.5 - 0.1), 9);
  });

  it("round-trips every 0.1 dB step to better than 0.05 dB", () => {
    for (const db of tenths(-24, 24)) {
      expect(Math.abs(t.db(t.pos(db)) - db)).toBeLessThan(0.05);
    }
  });

  it("is fine near the centre: 0.1 dB is a small part of the travel", () => {
    const step = t.pos(0.1) - t.pos(0);
    expect(step).toBeLessThan(0.02);
    expect(step).toBeGreaterThan(0);
  });

  it("clamps out-of-range values", () => {
    expect(t.pos(40)).toBe(1);
    expect(t.pos(-40)).toBe(0);
    expect(t.db(-0.2)).toBe(-24);
    expect(t.db(1.2)).toBe(24);
  });

  it("works for a daemon with a smaller balance limit", () => {
    const small = balanceTaper(6);
    expect(small.pos(3)).toBeCloseTo(0.85, 9);
    expect(small.pos(6)).toBeCloseTo(1, 9);
    for (const db of tenths(-6, 6)) expect(Math.abs(small.db(small.pos(db)) - db)).toBeLessThan(0.05);
  });
});

describe("keyboard, wheel and typed steps", () => {
  // A daemon reporting the card's -64 dB floor while digital attenuation
  // holds the volume at -90 dB.
  const t = volumeTaper(-64);

  it("a step down below the slider's floor is never a rise", () => {
    expect(stepDb(-90, -90.5, t)).toBeLessThanOrEqual(-90);
    expect(stepDb(-90, -90.1, t)).toBeLessThanOrEqual(-90);
    // A typed value below the floor: never above where it was.
    expect(stepDb(-90, -100, t)).toBeLessThanOrEqual(-90);
  });

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

  it("keys and wheel inside the range step as before", () => {
    expect(keyStepDb("ArrowDown", false, -20, t)).toBe(-20.5);
    expect(keyStepDb("ArrowUp", true, -20, t)).toBe(-19.9);
    expect(keyStepDb("Home", false, -20, t)).toBe(-64);
    expect(keyStepDb("End", false, -20, t)).toBe(0);
    expect(wheelStepDb(-100, false, -20, t)).toBe(-19.5);
    expect(wheelStepDb(100, false, -20, t)).toBe(-20.5);
  });

  it("a step up above the slider's top is never a fall", () => {
    const c = volumeTaper(-80, -20);
    expect(stepDb(-10, -9.5, c)).toBeGreaterThanOrEqual(-10);
  });

  it("steps inside the range are quantised and clamped as before", () => {
    expect(stepDb(-20, -20.5, t)).toBe(-20.5);
    expect(stepDb(-20, -19.95, t)).toBe(-19.9);
    expect(stepDb(-63.8, -64.3, t)).toBe(-64);
    expect(stepDb(-0.2, 0.3, t)).toBe(0);
  });
});

describe("wheel direction and page keys", () => {
  const t = volumeTaper(-64);

  it("a horizontal wheel gesture (no vertical delta) does nothing", () => {
    expect(wheelStepDb(0, false, -20, t)).toBeNull();
    expect(wheelStepDb(0, true, -20, t)).toBeNull();
  });

  it("Page Up and Page Down move 6 dB, never against their direction", () => {
    expect(keyStepDb("PageDown", false, -20, t)).toBe(-26);
    expect(keyStepDb("PageUp", false, -20, t)).toBe(-14);
    expect(keyStepDb("PageUp", false, -3, t)).toBe(0);
    expect(keyStepDb("PageDown", false, -62, t)).toBe(-64);
    // Below the slider's floor: Page Down is never a rise.
    expect(keyStepDb("PageDown", false, -90, t)!).toBeLessThanOrEqual(-90);
  });
});
