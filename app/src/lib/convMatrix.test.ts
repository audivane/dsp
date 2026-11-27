import { expect, it } from "vitest";
import type { ConvolutionConfig } from "../api/types";
import type { IrInfo } from "../api/transport";
import { irKey, irName, irPaths, isMatrix, matrixText } from "./chain";
import { combinedTaps, irTaps } from "./summary";

const plain: ConvolutionConfig = { enabled: true, ir_path: "/irs/room.wav", gain_db: 0 };
const sub: ConvolutionConfig = {
  enabled: true,
  ir_path: "",
  gain_db: -1,
  matrix: [
    { in: 0, out: 0, ir_path: "/irs/sub.wav", channel: 0, gain_db: 0 },
    { in: 1, out: 0, ir_path: "/irs/sub.wav", channel: 1, gain_db: 0 },
    { in: 1, out: 1, ir_path: "/irs/r.wav", gain_db: 0 },
  ],
};
const info = (frames: number): IrInfo => ({ channels: 2, frames, rate: 48000 });

it("shows a matrix item read-only, with a key of its own", () => {
  expect(isMatrix(plain)).toBe(false);
  expect(isMatrix(sub)).toBe(true);
  expect(irName(sub)).toBe("Filter matrix");
  expect(irName({ ...sub, name: "Sub" })).toBe("Sub");
  expect(matrixText(sub)).toBe("Matrix: 3 filters (L→L, R→L, R→R)");
  expect(matrixText(plain)).toBeNull();
  expect(irPaths(sub)).toEqual(["/irs/sub.wav", "/irs/sub.wav", "/irs/r.wav"]);
  expect(irKey(plain)).toBe("/irs/room.wav");
  expect(irKey(sub)).not.toBe(irKey({ ...sub, matrix: sub.matrix!.slice(1) }));
});

it("counts a matrix by its longest entry", () => {
  const all = { "/irs/sub.wav": info(4000), "/irs/r.wav": info(1000), "/irs/room.wav": info(500) };
  expect(irTaps(sub, all)).toBe(4000);
  expect(irTaps(sub, { "/irs/sub.wav": info(4000) })).toBeNull();
  expect(combinedTaps([plain, sub], all)).toBe(4499);
});
