import { describe, expect, it } from "vitest";
import { mockConvResponse } from "./mockConvResponse";

describe("mock conv_response", () => {
  it("has one curve per channel or route and a combined curve per output", () => {
    const r = mockConvResponse(
      [
        { enabled: true, ir_path: "/x/room.wav", gain_db: 0 },
        { enabled: false, ir_path: "/x/off.wav", gain_db: 0 },
        { enabled: true, ir_path: "", gain_db: -3, matrix: [{ in: 0, out: 0, ir_path: "/x/sub_l.wav", gain_db: 0 }, { in: 1, out: 0, ir_path: "/x/sub_r.wav", gain_db: 0 }] },
      ],
      48000,
      "a",
      null,
      () => 2,
    );
    expect(r.items.map((i) => i.index)).toEqual([0, 2]);
    expect(r.items[0].curves).toHaveLength(2);
    expect(r.items[1].curves.map((c) => [c.in, c.out])).toEqual([
      [0, 0],
      [1, 0],
    ]);
    expect(r.outputs).toHaveLength(2);
    expect(r.freqs_hz).toHaveLength(r.outputs[0].mag_db.length);
    // Output 1 has no route in the matrix: silent.
    expect(r.outputs[1].mag_db.every((m) => m === -200)).toBe(true);
    expect(mockConvResponse([], 48000, "a", null, () => 2).outputs).toEqual([]);
  });
});
