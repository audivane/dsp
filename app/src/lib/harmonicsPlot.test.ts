import { describe, expect, it } from "vitest";
import { dbToPct, fmtPctNum, fmtPctValue, harmY, harmYTicks, parsePctLimit, pctAxisTicks, pctTick } from "./harmonicsPlot";

describe("harmonics plot axis", () => {
  it("converts dB to percent", () => {
    expect(dbToPct(0)).toBeCloseTo(100);
    expect(dbToPct(-40)).toBeCloseTo(1);
    expect(dbToPct(-120)).toBeCloseTo(0.0001);
  });

  it("formats small percentages without exponents", () => {
    expect(fmtPctValue(-42.64)).toBe("0.738 %");
    expect(fmtPctValue(-120)).toBe("0.00010 %");
    expect(fmtPctValue(-140)).toBe("0.000010 %");
  });

  it("dB ticks run from 0 to the floor, denser when tall", () => {
    const short = harmYTicks("db", 150);
    expect(short[0].db).toBe(0);
    expect(short[short.length - 1].db).toBe(-140);
    expect(short).toHaveLength(8);
    expect(harmYTicks("db", 400)).toHaveLength(15);
  });

  it("% ticks label decades from 100 % to 0.0001 %", () => {
    const t = harmYTicks("pct", 200);
    const labels = t.filter((x) => x.label !== null).map((x) => x.label);
    expect(labels).toEqual(["100", "10", "1", "0.1", "0.01", "0.001", "0.0001"]);
    expect(t[0].db).toBeCloseTo(0);
    expect(t[t.length - 1].db).toBeCloseTo(-120);
    const tall = harmYTicks("pct", 600).filter((x) => x.label !== null).map((x) => x.label);
    expect(tall.slice(0, 4)).toEqual(["100", "50", "20", "10"]);
  });

  it("maps and clamps levels to the plot", () => {
    expect(harmY("db", 0, 10, 150)).toBe(10);
    expect(harmY("db", -140, 10, 150)).toBe(150);
    expect(harmY("db", -200, 10, 150)).toBe(150);
    expect(harmY("pct", -60, 0, 120)).toBeCloseTo(60);
  });
});

describe("log-% axis", () => {
  it("labels percentages without exponents", () => {
    expect(pctTick(0)).toBe("100");
    expect(pctTick(-20)).toBe("10");
    expect(pctTick(-60)).toBe("0.1");
    expect(pctTick(-140)).toBe("0.00001");
    expect(pctTick(-200)).toBe("0.00000001");
    expect(fmtPctNum(0.0123456)).toBe("0.0123");
    expect(fmtPctNum(177.8)).toBe("178");
  });
  it("puts a tick on each decade, 2 and 5 between when there is room", () => {
    const { ticks } = pctAxisTicks(-160, 5, 8);
    expect(ticks.map((d) => pctTick(d))).toEqual(["0.000001", "0.00001", "0.0001", "0.001", "0.01", "0.1", "1", "10", "100"]);
    const dense = pctAxisTicks(-40, 5, 12).ticks.map((d) => pctTick(d));
    expect(dense).toContain("5");
    expect(dense).toContain("2");
    const sparse = pctAxisTicks(-260, 20, 4).ticks;
    expect(sparse.length).toBeLessThan(10);
  });
  it("falls back to round values when zoomed into a decade", () => {
    const t = pctAxisTicks(-42, -38, 6).ticks.map((d) => pctTick(d));
    expect(t.length).toBeGreaterThan(2);
    expect(t.every((s) => !s.includes("e"))).toBe(true);
  });
  it("reads typed % limits", () => {
    expect(parsePctLimit("1")).toBeCloseTo(-40);
    expect(parsePctLimit("0.001 %")).toBeCloseTo(-100);
    expect(parsePctLimit("-1")).toBeNull();
    expect(parsePctLimit("x")).toBeNull();
  });
});
