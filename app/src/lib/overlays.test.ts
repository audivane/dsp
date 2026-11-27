import { describe, expect, it } from "vitest";
import { parseOverlayText } from "./overlays";

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

