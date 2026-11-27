import { describe, expect, it } from "vitest";
import { eqGrid, paintOrder, pickHandle } from "./EqGraph";

describe("EQ graph grid", () => {
  it("labels 1-2-5 and both ends over the chosen range", () => {
    const labels = (lo: number, hi: number) => eqGrid(lo, hi).filter((t) => t.label).map((t) => t.label);
    expect(labels(20, 20000)).toEqual(["20", "50", "100", "200", "500", "1k", "2k", "5k", "10k", "20k"]);
    expect(labels(20, 50000).slice(-2)).toEqual(["20k", "50k"]);
    expect(labels(10, 100000)[0]).toBe("10");
    expect(labels(10, 100000).at(-1)).toBe("100k");
    expect(eqGrid(20, 20000)).toHaveLength(28);
  });
});

describe("EQ graph handle picking", () => {
  const at = (x: number, y: number) => ({ x, y });

  it("takes the handle drawn on top when bands are stacked (e.g. added at the same spot)", () => {
    // 20 bands spread out, bands 21-25 all at one spot: the last is drawn on top.
    const pts = [...Array.from({ length: 20 }, (_, i) => at(10 + i * 30, 50)), ...Array.from({ length: 5 }, () => at(700, 100))];
    expect(pickHandle(pts, 700, 100, -1)).toBe(24);
    expect(pickHandle(pts, 703, 98, 19)).toBe(24);
  });

  it("keeps the selected band on top, as it is drawn", () => {
    const pts = [at(100, 100), at(100, 100), at(100, 100)];
    expect(paintOrder(3, 0)).toEqual([1, 2, 0]);
    expect(pickHandle(pts, 100, 100, 0)).toBe(0);
  });

  it("takes the visible upper handle where two overlap, not the nearer centre beneath", () => {
    // Band 20 (index 19) at x=100, band 21 drawn over it at x=110; a press at
    // x=104 is nearer 20's centre but lands on 21's disc.
    const pts = [...Array.from({ length: 19 }, (_, i) => at(400 + i * 30, 300)), at(100, 100), at(110, 100)];
    expect(pickHandle(pts, 104, 100, -1)).toBe(20);
  });

  it("still catches the nearest handle within reach when the press misses every disc", () => {
    const pts = [at(100, 100), at(200, 100)];
    expect(pickHandle(pts, 112, 100, -1)).toBe(0);
    expect(pickHandle(pts, 150, 100, -1)).toBe(-1);
  });
});
