import { describe, expect, it } from "vitest";
import { balanceTaper, quantiseDb, volumeTaper } from "./taper";

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
