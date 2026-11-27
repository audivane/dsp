import { describe, expect, it } from "vitest";
import type { FilterDesign } from "../api/types";
import { catalogueDesign } from "../api/mockCatalogue";
import { checkAll, checkDesign } from "../api/mockDesignCheck";
import { bandRate, upsamplerEngineBlock } from "./filterDesign";

const MAX = 8_388_608;
const passes = (d: FilterDesign) => expect(checkAll(d, MAX)).toEqual([]);

describe("design edits", () => {

  it("passes every catalogue expansion through the check", () => {
    for (const name of ["recommended", "craven_apod", "flattop_long", "chebyshev_mp", "gaussian_compact", "lanczos23_mp", "flattop_mega", "block_fft_large"]) passes(catalogueDesign(name)!);
  });
});

describe("plans", () => {
  it("sizes the engine block as the daemon does", () => {
    expect(upsamplerEngineBlock(1024, 44100)).toBe(512);
    expect(upsamplerEngineBlock(1024, 48000)).toBe(1024);
    expect(upsamplerEngineBlock(1000, 48000)).toBe(1024);
    expect(upsamplerEngineBlock(256, 48000)).toBe(512);
    expect(upsamplerEngineBlock(8192, 384000)).toBe(1024);
  });
});

describe("a passband end in Hz", () => {
  const base = catalogueDesign("recommended")!;
  if (base.kind !== "windowed_sinc" || base.cutoff.mode !== "band") throw new Error("a band design");
  const ctx = (rates: number[]) => ({ rates, ratioAt: () => ({ up: 2, down: 1 }), maxKernelTaps: 1 << 22, autoDb: 141.47, block: () => 1024, pipelines: 2 });

  it("converts at the output rate when decimating, on every path", () => {
    expect(bandRate(96000, { up: 1, down: 2 })).toBe(48000);
    expect(bandRate(44100, { up: 2, down: 1 })).toBe(44100);
    const craven = catalogueDesign("craven_apod")!;
    const down = (up: number, d: number) => ({ ...ctx([96000]), ratioAt: () => ({ up, down: d }) });
    // 96 -> 48 kHz: 20 kHz of the 24 kHz Nyquist fits.
    expect(checkDesign(craven, down(1, 2)).notes).toEqual([]);
    // 96 -> 32 kHz: 20 kHz is past the 16 kHz Nyquist, so the note names
    // the fraction of the lower Nyquist.
    const n = checkDesign(craven, down(1, 3)).notes;
    expect(n.map((x) => x.source_rate)).toEqual([96000]);
    expect(n[0].message).toContain("1.250 of the Nyquist");
  });

  it("sizes a decimating band at the lower Nyquist, as the daemon does", () => {
    // A Custom craven band pinned at 23.8 kHz, 192 -> 48 kHz: 23.8 of the
    // 24 kHz Nyquist is a narrow transition, a long kernel past the
    // rational budget (the daemon estimates 10,635 taps and refuses it).
    const craven = catalogueDesign("craven_apod")!;
    if (craven.kind !== "windowed_sinc" || craven.cutoff.mode !== "band") throw new Error("a band design");
    const pinned: FilterDesign = { ...craven, cutoff: { ...craven.cutoff, passband_end_hz: 23800 } };
    const c = checkDesign(pinned, { ...ctx([192000]), ratioAt: () => ({ up: 1, down: 4 }) });
    expect(c.plans).toEqual([]);
    expect(c.errors.map((e) => e.path)).toEqual(["design.length"]);
  });
});

