import { describe, expect, it } from "vitest";
import type { EqProfile } from "../api/types";
import {
  BUILTIN_TARGETS,
  DRAW_FREQS,
  OverlayError,
  boundsFromCurves,
  displayBounds,
  displayPoints,
  flatTarget,
  normalisationDb,
  overlayData,
  parseOverlayText,
  parseRefHz,
  predictedPoints,
  resample,
  sampleAt,
  tiltTarget,
  toOverlay,
  uniqueName,
} from "./overlays";

/** Twelve rows, 100 Hz … 1.2 kHz. */
function rows(values: number[] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]) {
  return values.map((v, i) => `${100 * Math.pow(1.2, i)} ${v}`);
}

describe("parseOverlayText", () => {
  it("reads a squig.link / AutoEq CSV with a header", () => {
    const text = "frequency,raw\n" + rows().map((r) => r.replace(" ", ",")).join("\n") + "\n";
    const d = parseOverlayText(text, { kind: "target", name: "harman.csv" });
    expect(d.kind).toBe("target");
    expect(d.points).toHaveLength(12);
    expect(d.points![0]).toEqual([100, 0]);
    expect(d.points![11][0]).toBeCloseTo(100 * Math.pow(1.2, 11), 6);
  });

  it("reads REW text with tabs, comments and a third (phase) column", () => {
    const text = [
      "* Measurement: Desk monitor",
      "* Frequency(Hz)\tSPL(dB)\tPhase(degrees)",
      ...rows().map((r, i) => r.replace(" ", "\t") + `\t${i * 2}`),
    ].join("\r\n");
    const d = parseOverlayText(text, { kind: "measurement", name: "rew.txt" });
    expect(d.points).toHaveLength(12);
    expect(d.points![3][1]).toBe(3);
  });

  it("reads semicolon-separated and space-separated lists", () => {
    const semi = parseOverlayText(rows().join("\n").replace(/ /g, ";"), { kind: "target", name: "a" });
    const space = parseOverlayText(rows().join("\n"), { kind: "target", name: "b" });
    expect(semi.points).toEqual(space.points);
  });

  it("sorts by frequency and drops non-finite or column-shifted values", () => {
    const shuffled = ["1000 0", "100 -1", "200 nan", "300 2", "400 Infinity", ...rows().slice(4)];
    const d = parseOverlayText(shuffled.join("\n"), { kind: "target", name: "mixed" });
    const fs = d.points!.map((p) => p[0]);
    expect(fs).toEqual([...fs].sort((a, b) => a - b));
    expect(fs).not.toContain(200);
    expect(fs).not.toContain(400);
    expect(d.points!.every((p) => Number.isFinite(p[1]))).toBe(true);
  });

  it("keeps one point per frequency", () => {
    const text = rows().join("\n") + "\n100.0 99\n";
    const d = parseOverlayText(text, { kind: "target", name: "dup" });
    expect(d.points!.filter((p) => p[0] === 100)).toEqual([[100, 99]]);
  });

  it("rejects a file with fewer than 10 usable points, naming it", () => {
    let err: unknown;
    try {
      parseOverlayText("20 1\n30 2\n40 3", { kind: "target", name: "short.csv" });
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(OverlayError);
    expect((err as Error).message).toContain("short.csv");
    expect((err as Error).message).toContain("at least 10");
  });

  it("reads a three-column bounds file", () => {
    const text = rows()
      .map((r, i) => {
        const [f] = r.split(" ");
        return `${f},${i - 2},${i + 2}`;
      })
      .join("\n");
    const d = parseOverlayText(text, { kind: "bounds", name: "bounds.csv" });
    expect(d.kind).toBe("bounds");
    expect(d.lower![0]).toEqual([100, -2]);
    expect(d.upper![0]).toEqual([100, 2]);
  });

  it("asks for two files when a bounds file has only one level column", () => {
    expect(() => parseOverlayText(rows().join("\n"), { kind: "bounds", name: "one.csv" })).toThrow(/two files|three columns/);
  });

  it("builds bounds from two single-curve files", () => {
    const lower = parseOverlayText(rows().join("\n"), { kind: "target", name: "l" }).points!;
    const d = boundsFromCurves(rows().join("\n"), rows([5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]).join("\n"), "Preference");
    expect(d.lower).toEqual(lower);
    expect(d.upper![0][1]).toBe(5);
  });

  it("ignores extra columns for targets but uses them for bounds", () => {
    const text = rows()
      .map((r) => {
        const [f, db] = r.split(" ");
        return `${f},${db},99,123`;
      })
      .join("\n");
    expect(parseOverlayText(text, { kind: "target", name: "x" }).points![0][1]).toBe(0);
    expect(parseOverlayText(text, { kind: "bounds", name: "x" }).upper![0][1]).toBe(99);
  });

  it("does not shift columns or read blank cells as zero in delimited files", () => {
    const base = rows();
    // Blank level column in col 1 with value in col 2 across comma, semicolon, and tab.
    for (const sep of [",", ";", "\t"]) {
      const mixed = [
        ...base.map((r) => r.replace(" ", sep)),
        `2500${sep}${sep}120`,
        `3000${sep} ${sep}130`,
      ].join("\n");
      const target = parseOverlayText(mixed, { kind: "target", name: "blank" });
      expect(target.points!.find((p) => p[0] === 2500)).toBeUndefined();
      expect(target.points!.find((p) => p[0] === 3000)).toBeUndefined();
      expect(target.points).toHaveLength(12);
    }

    // Leading tab leaves frequency empty; row is ignored rather than shifting col 1 into frequency.
    const leadingTab = [...base.map((r) => r.replace(" ", "\t")), "\t999\t12"].join("\n");
    const parsedTab = parseOverlayText(leadingTab, { kind: "measurement", name: "lead-tab" });
    expect(parsedTab.points!.find((p) => p[0] === 999)).toBeUndefined();

    // All-blank column 1 with populated column 2 does not promote col 2 to col 1.
    const allBlankCol1 = base.map((r) => `${r.split(" ")[0]},,99`).join("\n");
    expect(() => parseOverlayText(allBlankCol1, { kind: "target", name: "empty-col1" })).toThrow(/usable points/);

    // Bounds file where upper column is blank on some rows drops those rows from upper without reading 0.
    const boundsBlankUpper = base
      .map((r, i) => {
        const [f] = r.split(" ");
        return i === 0 ? `${f},-5,` : `${f},-5,5`;
      })
      .join("\n");
    const b = parseOverlayText(boundsBlankUpper, { kind: "bounds", name: "bounds-blank" });
    expect(b.lower).toHaveLength(12);
    expect(b.upper).toHaveLength(11);
    expect(b.upper!.every((p) => p[1] === 5)).toBe(true);
  });
});

describe("resampling and normalisation", () => {
  it("interpolates in log frequency", () => {
    const points: [number, number][] = [
      [100, 0],
      [1000, 10],
    ];
    expect(sampleAt(points, 100)).toBe(0);
    expect(sampleAt(points, 1000)).toBe(10);
    expect(sampleAt(points, Math.sqrt(100 * 1000))).toBeCloseTo(5, 6);
    // Outside the file: the end value, so a short file still draws.
    expect(sampleAt(points, 20)).toBe(0);
    expect(sampleAt(points, 20000)).toBe(10);
  });

  it("resamples onto the shared 1/24-octave grid", () => {
    const points: [number, number][] = [
      [20, 0],
      [20000, 30],
    ];
    const out = resample(points, DRAW_FREQS);
    expect(out).toHaveLength(DRAW_FREQS.length);
    expect(out[0]).toBe(0);
    expect(out[out.length - 1]).toBeCloseTo(30, 6);
    expect(resample(undefined, [20, 30])).toEqual([null, null]);
  });

  it("puts the curve on 0 dB at the reference frequency, then applies the offset", () => {
    const points: [number, number][] = [
      [100, -6],
      [1000, 2],
      [10000, 8],
    ];
    expect(normalisationDb(points, 1000)).toBe(2);
    const shown = displayPoints(points, 1000, 1.5);
    expect(shown[1][1]).toBeCloseTo(1.5, 9);
    expect(shown[0][1]).toBeCloseTo(-8 + 1.5, 9);
  });

  it("shifts preference bounds as one band, keeping the width at the reference", () => {
    const lower: [number, number][] = [
      [20, -6],
      [1000, -2],
      [20000, 1],
    ];
    const upper: [number, number][] = [
      [20, -2],
      [1000, 2],
      [20000, 5],
    ];
    const at = (b: { lower: [number, number][]; upper: [number, number][] }) => [b.lower[1][1], b.upper[1][1]];
    expect(at(displayBounds(lower, upper, 1000))).toEqual([-2, 2]);
    expect(at(displayBounds(lower, upper, 1000, 1))).toEqual([-1, 3]);
  });

  it("draws on the shared 1/24-octave grid", () => {
    const grid = DRAW_FREQS;
    expect(grid[0]).toBeCloseTo(20, 6);
    expect(grid[grid.length - 1]).toBeCloseTo(20000, 6);
    for (let i = 1; i < grid.length; i++) {
      const octaves = Math.log2(grid[i] / grid[i - 1]);
      expect(octaves).toBeCloseTo(1 / 24, 2);
    }
  });
});

describe("measurement + current EQ", () => {
  const flat: [number, number][] = DRAW_FREQS.map((f) => [f, -3] as [number, number]);
  const eq: EqProfile = {
    name: "test",
    version: 2,
    preamp_db: 0,
    linked_enabled: true,
    bands: [{ kind: "peaking", freq_hz: 1000, gain_db: 6, q: 2, enabled: true }],
  };

  it("adds the EQ response to the measurement", () => {
    const predicted = predictedPoints(flat, eq, 48000, 1000);
    const at = (f: number) => sampleAt(predicted, f)!;
    // Normalised with the measurement: 6 dB of boost at 1 kHz is visible.
    expect(at(1000)).toBeCloseTo(6, 1);
    expect(at(20)).toBeCloseTo(0, 2);
    expect(at(20000)).toBeCloseTo(0, 2);
  });

  it("follows the measurement's own reference, not the prediction's", () => {
    const measured: [number, number][] = [
      [20, 0],
      [1000, 4],
      [20000, -2],
    ];
    const predicted = predictedPoints(measured, eq, 48000, 1000);
    // 4 dB at 1 kHz is removed; the 6 dB peaking boost is not.
    expect(sampleAt(predicted, 1000)!).toBeCloseTo(6, 1);
  });

  it("captures narrow high-Q EQ bands between sparse measurement points", () => {
    const sparse: [number, number][] = [
      [20, 0],
      [500, 0],
      [2000, 0],
      [20000, 0],
    ];
    const narrowEq: EqProfile = {
      name: "narrow",
      version: 2,
      preamp_db: 0,
      linked_enabled: true,
      bands: [{ kind: "peaking", freq_hz: 1000, gain_db: 12, q: 10, enabled: true }],
    };
    const predicted = predictedPoints(sparse, narrowEq, 48000, 20);
    expect(sampleAt(predicted, 1000)!).toBeCloseTo(12, 1);
    expect(sampleAt(predicted, 1050)!).toBeGreaterThan(5);
    expect(sampleAt(predicted, 500)!).toBeLessThan(0.5);
  });

  it("is a plain measurement without an EQ", () => {
    const predicted = predictedPoints(flat, null, 48000, 1000);
    expect(predicted.every((p) => Math.abs(p[1]) < 1e-9)).toBe(true);
  });
});

describe("generated targets", () => {
  it("tilt is a straight line through 0 dB at 1 kHz", () => {
    const d = tiltTarget(-0.7);
    expect(d.name).toBe("Tilt −0.7 dB/oct");
    const at = (f: number) => sampleAt(d.points!, f)!;
    expect(at(1000)).toBeCloseTo(0, 6);
    expect(at(500)).toBeCloseTo(0.7, 6);
    expect(at(2000)).toBeCloseTo(-0.7, 6);
    expect(at(20)).toBeCloseTo(0.7 * Math.log2(1000 / 20), 6);
  });

  it("tilt can be gentle, steep, positive or zero", () => {
    expect(tiltTarget(0).name).toBe("Tilt 0.0 dB/oct");
    expect(tiltTarget(1.5).name).toBe("Tilt +1.5 dB/oct");
    expect(sampleAt(tiltTarget(1.5).points!, 2000)).toBeCloseTo(1.5, 6);
  });

  it("flat is zero everywhere", () => {
    expect(flatTarget().points!.every((p) => p[1] === 0)).toBe(true);
  });
});

describe("built-in targets", () => {
  it("ships the AutoEq targets and the researched curves, whole and inside 20 Hz–20 kHz", () => {
    expect(BUILTIN_TARGETS.map((t) => t.name).slice(0, 3)).toEqual(["Harman over-ear 2018", "Harman in-ear 2019", "Diffuse field (ISO 11904-1)"]);
    expect(BUILTIN_TARGETS.length).toBe(7);
    for (const t of BUILTIN_TARGETS) {
      const pts = t.kind === "bounds" ? t.lower! : t.points!;
      expect(pts.length).toBeGreaterThanOrEqual(200);
      expect(pts[0][0]).toBeCloseTo(20, 1);
      expect(pts[pts.length - 1][0]).toBeCloseTo(20000, 0);
      expect(t.source).toBeTruthy();
      if (t.kind === "bounds") {
        expect(t.upper!.length).toBe(pts.length);
        t.upper!.forEach((p, i) => expect(p[1]).toBeGreaterThanOrEqual(pts[i][1]));
      } else {
        expect(t.kind).toBe("target");
        // A target is a level curve, not a copy of another one.
        expect(new Set(pts.map((p) => p[1])).size).toBeGreaterThan(20);
      }
    }
    expect(BUILTIN_TARGETS.filter((t) => t.kind === "bounds").map((t) => t.name)).toEqual([
      "Speaker tilt exploration -1.0 to -0.5 dB/oct (not preference limits)",
    ]);
  });

  it("starts every built-in at the source's own values", () => {
    // Spot checks against the AutoEq CSVs (master, 2025-07-20).
    const at = (d: { points?: [number, number][] }, f: number) => sampleAt(d.points!, f)!;
    expect(at(BUILTIN_TARGETS[0], 20)).toBeCloseTo(3.86, 1);
    expect(at(BUILTIN_TARGETS[1], 20)).toBeCloseTo(7.61, 1);
    expect(at(BUILTIN_TARGETS[2], 20)).toBeCloseTo(-4.1, 1);
  });
});

describe("runtime model", () => {
  it("reads a stored overlay and keeps its view settings", () => {
    const d = tiltTarget(-0.7);
    const o = toOverlay({ ...d, view: { colour: "#123456", visible: false, offset_db: -1.5, predicted: false } });
    expect(o.id).toBe(d.name);
    expect(o.colour).toBe("#123456");
    expect(o.visible).toBe(false);
    expect(o.offsetDb).toBe(-1.5);
    expect(o.predicted).toBe(false);
  });

  it("rejects things that are not overlays", () => {
    expect(overlayData(null)).toBeNull();
    expect(overlayData({ name: "x", kind: "nope", points: [] })).toBeNull();
    expect(overlayData({ name: "", kind: "target", points: [] })).toBeNull();
    expect(overlayData({ name: "x", kind: "target", points: [] })).toBeNull();
  });

  it("makes names unique", () => {
    expect(uniqueName("Desk", [])).toBe("Desk");
    expect(uniqueName("Desk", ["Desk"])).toBe("Desk (2)");
    expect(uniqueName("Desk", ["Desk", "Desk (2)"])).toBe("Desk (3)");
  });

  it("parses a reference frequency", () => {
    expect(parseRefHz("1k")).toBe(1000);
    expect(parseRefHz("500")).toBe(500);
    expect(parseRefHz("5")).toBeNull();
    expect(parseRefHz("nope")).toBeNull();
  });
});
