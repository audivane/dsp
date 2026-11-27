import { describe, expect, it } from "vitest";
import type { AbStatus } from "../api/types";
import { levelText } from "./abCompare";

describe("side B in words", () => {

  it("says which side is cut, never boosted", () => {
    const s = (o: number, h = 0): AbStatus => ({ enabled: true, current: "a", blind: false, offset_db: o, gain_a_db: Math.min(0, -o), gain_b_db: Math.min(0, -o) + o, headroom_db: h, overs: { a: 0, b: 0 }, error: null, level: { mode: "analytic", weighting: "k" }, measuring: null, measure_error: null });
    expect(levelText(s(4.16))).toBe("B is 4.16 dB quieter, so A is cut 4.16 dB · headroom 0.00 dB");
    expect(levelText(s(-2))).toBe("B is 2.00 dB louder, so B is cut 2.00 dB · headroom 0.00 dB");
    expect(levelText(s(0, 1.2))).toBe("A and B play at the same level · headroom −1.20 dB");
  });

});
