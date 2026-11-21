import { describe, expect, it } from "vitest";
import type { FilterDesign } from "../api/types";
import { catalogueDesign } from "../api/mockCatalogue";
import { checkAll } from "../api/mockDesignCheck";
import {
  defaultDesign,
  designKey,
  designSummary,
  errorGroup,
  groupErrors,
  lowLatencyDesign,
  patchUpsampler,
  planLatencyMs,
  planRows,
  refusalField,
  sameDesign,
  setCutoffMode,
  setKind,
  setLengthRule,
  setPath,
  setPhaseMethod,
  setPhaseType,
  setWindowType,
  sidelobeDb,
  sidelobeRatio,
  steppedAt,
  steppedText,
  applyBetaRule,
  parseTaps,
  tapPresets,
  windowValueGiven,
  type SincDesign,
  type WindowType,
} from "./filterDesign";

const sinc = (d: FilterDesign) => d as SincDesign;
const MAX = 8_388_608;
const passes = (d: FilterDesign) => expect(checkAll(d, MAX)).toEqual([]);

describe("design edits", () => {
  it("starts from a design that passes", () => {
    passes(defaultDesign());
    passes(lowLatencyDesign());
  });

  it("moves to a path that builds the window, with the fields that path needs", () => {
    for (const t of ["kaiser", "gaussian", "flattop", "chebyshev", "nuttall", "hann", "tukey"] as WindowType[]) {
      const d = setWindowType(defaultDesign(), t);
      expect(d.window.type).toBe(t);
      passes(d);
    }
    const flat = setWindowType(defaultDesign(), "flattop");
    expect(flat.numerics).toEqual({ path: "fir", grid_cutoff: "scaled" });
    // Back to Kaiser stays on the path the user has now.
    expect(setWindowType(flat, "kaiser").numerics.path).toBe("fir");
    // A Gaussian takes alpha off the standard path, a stopband on it.
    expect(setWindowType(flat, "gaussian").window).toMatchObject({ alpha: 0.125, sampling: "integer_centre" });
    expect(setWindowType(defaultDesign(), "gaussian").window).toMatchObject({ stopband_db: 150, sampling: "symmetric" });
  });

  it("gives the Tukey window linear phase and a manual length and cutoff", () => {
    const rec = sinc(catalogueDesign("recommended")!);
    const t = setWindowType(rec, "tukey");
    expect(t.phase).toEqual({ type: "linear" });
    expect(t.cutoff.mode).toBe("fraction");
    expect(t.length.rule).toBe("fixed");
    passes(t);
  });

  it("couples a band or automatic length with a band cutoff, and β with it", () => {
    const auto = setLengthRule(defaultDesign(), "auto");
    expect(auto.cutoff).toMatchObject({ mode: "band", passband_end: 0.9, stopband_start: 1 });
    expect(auto.window).toMatchObject({ type: "kaiser", beta: null });
    passes(auto);
    const back = setLengthRule(auto, "fixed");
    expect(back.cutoff).toEqual({ mode: "fraction", value: 1 });
    expect(back.window).toMatchObject({ beta: 14 });
    passes(back);
    // Picking the band cutoff gives a band length; leaving it, a manual one.
    const band = setCutoffMode(defaultDesign(), "band");
    expect(band.length).toEqual({ rule: "band" });
    passes(band);
    const frac = setCutoffMode(band, "nyquist_stop");
    expect(frac.length.rule).toBe("fixed");
    passes(frac);
  });

  it("remembers the last value of each length rule", () => {
    const d = { ...defaultDesign(), length: { rule: "fixed" as const, taps: 12345 } };
    const prev = { fixed: d.length };
    const per = setLengthRule(d, "per_input", prev);
    expect(per.length).toEqual({ rule: "per_input", taps: 511 });
    expect(setLengthRule(per, "fixed", prev).length).toEqual({ rule: "fixed", taps: 12345 });
  });

  it("picks the phase method of the path and keeps the blend", () => {
    const min = setPhaseType(defaultDesign(), "minimum");
    expect(min.phase).toEqual({ type: "minimum", method: "reflect" });
    const flat = setPhaseType(setWindowType(defaultDesign(), "flattop"), "intermediate");
    expect(flat.phase).toEqual({ type: "intermediate", mix: 0.75, method: "cepstral" });
    passes(flat);
    const ip = setPhaseType({ ...min, phase: { type: "intermediate", mix: 0.3, method: "reflect" } }, "minimum");
    expect(setPhaseType(ip, "intermediate").phase).toMatchObject({ mix: 0.75 });
    // A reflected phase on a preset path needs a symmetric window.
    const reflected = setPhaseMethod(flat, "reflect");
    expect(reflected.phase).toMatchObject({ method: "reflect" });
    expect(sinc(reflected).window.sampling).toBe("symmetric");
    passes(reflected);
    // Keeping the input samples needs linear phase: a phase change turns it off.
    expect(setPhaseType({ ...defaultDesign(), preserve_samples: true }, "minimum").preserve_samples).toBe(false);
  });

  it("moves the fields tied to the arithmetic path with it", () => {
    const g = setWindowType(defaultDesign(), "gaussian");
    const fir = setPath(g, "fir");
    expect(fir.numerics).toEqual({ path: "fir", grid_cutoff: "scaled" });
    expect(sinc(fir).window).toMatchObject({ alpha: 0.125 });
    passes(fir);
    const flat = sinc(catalogueDesign("flattop_long")!);
    const std = setPath({ ...flat, window: { type: "kaiser", beta: 10, sampling: "symmetric" } }, "standard");
    expect(std.decimation).toEqual({ length: "keep" });
    expect(std.numerics.grid_cutoff).toBe("design_ratio");
    passes(std);
  });

  it("switches kinds, keeping what applies and restoring what was left", () => {
    const flat = catalogueDesign("flattop_long")!;
    const memory = { windowed_sinc: flat };
    const lz = setKind(flat, "lanczos");
    expect(lz).toMatchObject({ kind: "lanczos", lobes: 23, based_on: "flattop_long", numerics: { path: "fir" } });
    passes(lz);
    // From the standard path, Lanczos moves to the fir path.
    const lz2 = setKind(lowLatencyDesign(), "lanczos");
    expect(lz2).toMatchObject({ numerics: { path: "fir" }, phase: { method: "cepstral" } });
    passes(lz2);
    expect(setKind(lz, "windowed_sinc", memory)).toEqual(flat);
    for (const k of ["cascade", "interpolator", "block_fft"] as const) passes(setKind(flat, k));
    expect(setKind(flat, "block_fft")).toEqual({ based_on: "flattop_long", kind: "block_fft", block: { input: 16384 }, overlap: "quarter_average" });
  });

  it("passes every catalogue expansion through the check", () => {
    for (const name of ["recommended", "flattop_long", "chebyshev_mp", "gaussian_compact", "lanczos23_mp", "flattop_mega", "block_fft_large"]) passes(catalogueDesign(name)!);
  });
});

describe("comparing designs", () => {
  it("compares values, not key order or the basis name", () => {
    const a = catalogueDesign("chebyshev_mp")!;
    const b = JSON.parse(JSON.stringify(a)) as FilterDesign;
    const reordered = Object.fromEntries(Object.entries(b).reverse()) as FilterDesign;
    expect(sameDesign(a, reordered)).toBe(true);
    expect(sameDesign(a, { ...a, based_on: "other" })).toBe(true);
    expect(sameDesign(a, setPhaseType(sinc(a), "linear"))).toBe(false);
    expect(designKey(a)).not.toContain("based_on");
  });
});

describe("stepped values", () => {
  const taps = { by_source_rate: [[50000, 167], [100000, 83], [null, 63]] as [number | null, number][] };
  it("takes the step for a source rate", () => {
    expect(steppedAt(taps, 44100)).toBe(167);
    expect(steppedAt(taps, 96000)).toBe(83);
    expect(steppedAt(taps, 192000)).toBe(63);
    expect(steppedAt(511, 192000)).toBe(511);
  });
  it("reads the steps in plain words", () => {
    expect(steppedText(taps, String)).toBe("167 below 50 kHz, 83 below 100 kHz, 63 above");
    expect(steppedText(511, String)).toBe("511");
  });
});

describe("field errors", () => {
  it("puts each path where its field shows", () => {
    expect(errorGroup("design.window.beta")).toBe("window");
    expect(errorGroup("design.rates.fallback_window.window.sampling")).toBe("rates");
    expect(errorGroup("design.length.taps[1]")).toBe("length");
    expect(errorGroup("design.lobes")).toBe("length");
    expect(errorGroup("design.block")).toBe("length");
    expect(errorGroup("design.cutoff.passband_end")).toBe("cutoff");
    expect(errorGroup("design.phase.mix")).toBe("phase");
    expect(errorGroup("design.preserve_samples")).toBe("samples");
    expect(errorGroup("design.numerics.path")).toBe("numerics");
    expect(errorGroup("target")).toBe("target");
    expect(errorGroup("design")).toBe("general");
    const g = groupErrors([
      { path: "design.window.beta", message: "a" },
      { path: "design.window.sampling", message: "b" },
      { path: "design", message: "c" },
    ]);
    expect(g.window?.map((e) => e.message)).toEqual(["a", "b"]);
    expect(g.general?.length).toBe(1);
  });

  it("reads the field a daemon refusal names", () => {
    expect(refusalField("Chain.upsampler.design.length: at 44100 Hz (160/147), its kernel would have 9633792 taps, more than 8388608.")).toEqual({
      path: "design.length",
      message: "at 44100 Hz (160/147), its kernel would have 9633792 taps, more than 8388608",
    });
    expect(refusalField("chain.upsampler.design.window.beta: must be between 0 and 40 (got 41)")).toEqual({ path: "design.window.beta", message: "must be between 0 and 40 (got 41)" });
    expect(refusalField("the engine is not running")).toBeNull();
  });
});

describe("plans", () => {
  const plan = { grid_up: 4, grid_down: 1, grid_taps: 65536, grid_taps_exact: true, workspace_bytes: 4 * 2 ** 20, resident_bytes: 1536 * 1024, cpu_load: 0.034 };
  it("reads one row per rate, lowest first", () => {
    const rows = planRows([
      { source_rate: 96000, ratio: { up: 8, down: 1 }, plan: { ...plan, grid_taps_exact: false } },
      { source_rate: 44100, ratio: { up: 160, down: 147 }, plan },
    ]);
    expect(rows.map((r) => [r.rateText, r.ratioText, r.tapsText, r.memoryText, r.cpuText])).toEqual([
      ["44.1 kHz", "160/147", "64k", "1.5 MiB", "3.4%"],
      ["96 kHz", "×8", "≤ 64k", "1.5 MiB", "3.4%"],
    ]);
  });
  it("gives the delay of a linear kernel of known length, and nothing else", () => {
    expect(planLatencyMs(defaultDesign(), plan, 44100)).toBeCloseTo((65535 / 2 / 176400) * 1000, 9);
    expect(planLatencyMs(lowLatencyDesign(), plan, 44100)).toBeNull();
    expect(planLatencyMs(defaultDesign(), { ...plan, grid_taps_exact: false }, 44100)).toBeNull();
    expect(planLatencyMs({ kind: "cascade", stage: "fir" }, { ...plan, grid_taps: 0 }, 44100)).toBeNull();
  });
});

describe("helpers", () => {
  it("edits Chebyshev sidelobes in dB", () => {
    expect(sidelobeDb(2 ** -32)).toBeCloseTo(192.66, 2);
    expect(sidelobeDb(sidelobeRatio(150))).toBeCloseTo(150, 9);
  });
  it("summarises a design", () => {
    expect(designSummary(defaultDesign())).toBe("Kaiser 4k · linear");
    expect(designSummary(catalogueDesign("gaussian_compact")!)).toBe("Gaussian compact · linear");
    expect(designSummary(catalogueDesign("flattop_huge")!)).toBe("Flat-top constant time · linear");
    expect(designSummary(catalogueDesign("table_sinc")!)).toBe("Table sinc");
  });
  it("keeps exactly one of preset and design", () => {
    const cur = { target: { ratio: 4 }, preset: "flattop_long", design: null };
    expect(patchUpsampler(cur, { design: defaultDesign() })).toMatchObject({ preset: null, design: { kind: "windowed_sinc" } });
    expect(patchUpsampler({ ...cur, preset: null, design: defaultDesign() }, { preset: "cosine" })).toEqual({ target: { ratio: 4 }, preset: "cosine", design: null });
    expect(patchUpsampler(cur, { target: { ratio: 8 } })).toEqual({ ...cur, target: { ratio: 8 } });
  });
});

describe("the taps field", () => {
  it("reads plain numbers, powers of two and binary sizes", () => {
    expect(parseTaps("65536")).toBe(65536);
    expect(parseTaps(" 1,048,576 ")).toBe(1048576);
    expect(parseTaps("2^20")).toBe(1048576);
    expect(parseTaps("2^16")).toBe(65536);
    expect(parseTaps("64k")).toBe(65536);
    expect(parseTaps("256K")).toBe(262144);
    expect(parseTaps("1M")).toBe(1048576);
    expect(parseTaps("1.5m")).toBe(1572864);
    expect(parseTaps("4095")).toBe(4095);
  });

  it("refuses what is not a tap count", () => {
    for (const t of ["", "abc", "-5", "0", "2^", "3^4", "1G", "64 kHz"]) expect(parseTaps(t)).toBeNull();
  });

  it("offers powers of two from 16k up to the engine's limit", () => {
    expect(tapPresets(4_194_304)).toEqual([16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152, 4194304]);
    expect(tapPresets(1_048_576).at(-1)).toBe(1048576);
  });
});

describe("the window value rule", () => {
  const kaiser = () => sinc(defaultDesign());
  it("gives β with a fraction or stop-at-Nyquist cutoff and derives it with a band cutoff", () => {
    const d = kaiser();
    expect(d.window).toMatchObject({ type: "kaiser", beta: 16 });
    const band = setCutoffMode(d, "band");
    expect(windowValueGiven(band)).toBe(false);
    expect(band.window).toMatchObject({ beta: null });
    passes(band);
    for (const m of ["fraction", "nyquist_stop"] as const) {
      const back = setCutoffMode(band, m);
      expect(windowValueGiven(back)).toBe(true);
      expect(back.window).toMatchObject({ beta: 14 });
      passes(back);
    }
    passes(setCutoffMode(d, "nyquist_stop"));
  });

  it("follows the length rule, which moves the cutoff", () => {
    const auto = setLengthRule(kaiser(), "auto");
    expect(auto.cutoff.mode).toBe("band");
    expect(auto.window).toMatchObject({ beta: null });
    passes(auto);
    const fixed = setLengthRule(auto, "fixed");
    expect(fixed.window).toMatchObject({ beta: 14 });
    passes(fixed);
  });

  it("puts a missing β back for a fraction cutoff, and drops a given one for a band", () => {
    const d = kaiser();
    const missing = { ...d, window: { type: "kaiser", beta: null, sampling: "symmetric" } } as SincDesign;
    expect(applyBetaRule(missing).window).toMatchObject({ beta: 14 });
    const band = setCutoffMode(d, "band");
    const given = { ...band, window: { type: "kaiser", beta: 9, sampling: "symmetric" } } as SincDesign;
    expect(applyBetaRule(given).window).toMatchObject({ beta: null });
    expect(applyBetaRule(d)).toBe(d);
  });

  it("does the same for the standard path's Gaussian stopband", () => {
    const g = setWindowType(kaiser(), "gaussian");
    expect(g.window).toMatchObject({ stopband_db: 150 });
    const band = setCutoffMode(g, "band");
    expect(band.window).toMatchObject({ stopband_db: null });
    passes(band);
    expect(setCutoffMode(band, "fraction").window).toMatchObject({ stopband_db: 150 });
    // A new window on a band design is derived from the start.
    expect(setWindowType(setCutoffMode(kaiser(), "band"), "gaussian").window).toMatchObject({ stopband_db: null });
  });
});
