import { describe, expect, it } from "vitest";
import { DEFAULT_HARM_GRAPH, parseHarmGraph, parseRangeChoice, xPresetSpan } from "./graphPrefs";

describe("harmonics graph prefs", () => {
  it("defaults to a −10 dBFS tone and the home view", () => {
    expect(parseHarmGraph(null)).toEqual(DEFAULT_HARM_GRAPH);
    expect(DEFAULT_HARM_GRAPH.level).toBe(-10);
    expect(DEFAULT_HARM_GRAPH.view).toBeNull();
  });
  it("keeps valid fields and drops odd ones", () => {
    const p = parseHarmGraph(JSON.stringify({ freq: 40, level: 3, compare: "after", scale: "lin", view: { x0: 0, x1: 50000, y0: -200, y1: 5 }, levelView: { x0: 1, x1: 0, y0: 0, y1: 1 }, hidden: [3, "x", 99] }));
    expect(p.freq).toBe(40);
    expect(p.level).toBe(-10);
    expect(p.compare).toBe("after");
    expect(p.scale).toBe("lin");
    expect(p.view).toEqual({ x0: 0, x1: 50000, y0: -200, y1: 5 });
    expect(p.levelView).toBeNull();
    expect(p.hidden).toEqual([3]);
    expect(parseHarmGraph("{not json")).toEqual(DEFAULT_HARM_GRAPH);
  });
});

describe("range presets", () => {
  it("spans each preset, Nyquist from the rate", () => {
    expect(xPresetSpan("audio", 384000)).toEqual({ lo: 20, hi: 20000 });
    expect(xPresetSpan("50k", 384000)).toEqual({ lo: 20, hi: 50000 });
    expect(xPresetSpan("100k", 384000)).toEqual({ lo: 10, hi: 100000 });
    expect(xPresetSpan("nyquist", 384000)).toEqual({ lo: 20, hi: 384000 });
    expect(xPresetSpan("nyquist", 22050).hi).toBe(22050);
  });
  it("keeps a valid stored choice and falls back field by field", () => {
    const fb = { x: "audio", y: "160" } as const;
    expect(parseRangeChoice(JSON.stringify({ x: "nyquist", y: "fit" }), fb)).toEqual({ x: "nyquist", y: "fit" });
    expect(parseRangeChoice(JSON.stringify({ x: "1M", y: "200" }), fb)).toEqual({ x: "audio", y: "200" });
    expect(parseRangeChoice("100k", fb)).toEqual(fb);
    expect(parseRangeChoice(null, fb)).toEqual(fb);
  });
});
