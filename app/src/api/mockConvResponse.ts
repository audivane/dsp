// The mock daemon's `conv_response`: made-up but plausible curves (a room
// correction's bass dip and treble tilt, a sub's low-pass), one per file
// path so each item looks different, combined as the engine combines them
// (items in series add in dB; a matrix output sums its inputs).

import type { ConvolutionConfig, ConvResponse, ConvResponseCurve } from "./types";
import { DRAW_FREQS } from "../lib/overlays";
import { convList } from "../lib/chain";

const hash = (s: string) => {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return (h >>> 0) / 4294967296;
};

/** A complex response sample (linear re/im) of a made-up filter `seed` at `f`. */
function shape(seed: string, ch: number, f: number): [number, number] {
  const r = hash(seed + ch);
  const sub = /sub/i.test(seed);
  let db: number;
  if (sub) {
    // A 4th-order low-pass at 60–100 Hz.
    const fc = 60 + 40 * r;
    db = -10 * Math.log10(1 + Math.pow(f / fc, 8));
  } else {
    const dip = -6 * r * Math.exp(-Math.pow(Math.log2(f / (50 + 80 * r)), 2) * 3);
    const tilt = -1.5 * Math.log2(Math.max(1, f / 1000)) * r;
    const ripple = 0.8 * Math.sin(Math.log2(f) * (5 + 3 * r) + ch);
    db = dip + tilt + ripple;
  }
  const delayMs = sub ? 2 + r : 0.5 + r * 0.2;
  const mag = Math.pow(10, db / 20);
  const ph = -2 * Math.PI * f * delayMs * 1e-3;
  return [mag * Math.cos(ph), mag * Math.sin(ph)];
}

const toDb = (re: number, im: number) => Math.max(-200, 20 * Math.log10(Math.hypot(re, im) || 1e-10));

function curveOf(points: [number, number][], freqs: number[], input?: number, output?: number): ConvResponseCurve {
  const mag_db = points.map(([re, im]) => Math.round(toDb(re, im) * 1000) / 1000);
  // Group delay from the unwrapped phase's slope.
  const ph = points.map(([re, im]) => Math.atan2(im, re));
  for (let i = 1; i < ph.length; i++) while (ph[i] - ph[i - 1] > Math.PI) ph[i] -= 2 * Math.PI;
  const group_delay_ms = ph.map((_, i) => {
    const a = Math.max(0, i - 1);
    const b = Math.min(ph.length - 1, i + 1);
    if (mag_db[i] < -120 || a === b) return null;
    return Math.round((-(ph[b] - ph[a]) / (2 * Math.PI * (freqs[b] - freqs[a]))) * 1e7) / 1e4;
  });
  const c: ConvResponseCurve = { mag_db, group_delay_ms };
  if (input !== undefined) c.in = input;
  if (output !== undefined) c.out = output;
  return c;
}

const mul = (a: [number, number], b: [number, number]): [number, number] => [a[0] * b[0] - a[1] * b[1], a[0] * b[1] + a[1] * b[0]];

export function mockConvResponse(convolution: ConvolutionConfig[] | ConvolutionConfig | null | undefined, rate: number, side: "a" | "b", device: string | null, channelsOf: (path: string) => number): ConvResponse {
  const freqs = DRAW_FREQS.filter((f) => f <= rate / 2);
  const list = convList({ convolution } as never);
  const on = list.map((c, index) => ({ c, index })).filter(({ c }) => c.enabled);
  // The stage as a 2×2 matrix of complex responses per frequency, [out][in].
  const eye = (): [number, number][][][] => freqs.map(() => [[[1, 0], [0, 0]], [[0, 0], [1, 0]]]);
  let stage = eye();
  const items = on.map(({ c, index }) => {
    const g = Math.pow(10, c.gain_db / 20);
    const m: [number, number][][][] = freqs.map(() => [[[0, 0], [0, 0]], [[0, 0], [0, 0]]]);
    const curves: ConvResponseCurve[] = [];
    if (c.matrix) {
      for (const e of c.matrix) {
        const ge = g * Math.pow(10, e.gain_db / 20);
        const pts = freqs.map((f, k) => {
          const [re, im] = shape(e.ir_path, e.channel ?? 0, f);
          const v: [number, number] = [re * ge, im * ge];
          if (e.out < 2 && e.in < 2) m[k][e.out][e.in] = v;
          return v;
        });
        curves.push(curveOf(pts, freqs, e.in, e.out));
      }
    } else {
      const chans = channelsOf(c.ir_path) === 1 ? 1 : 2;
      for (let ch = 0; ch < chans; ch++) {
        const pts = freqs.map((f, k) => {
          const [re, im] = shape(c.ir_path, ch, f);
          const v: [number, number] = [re * g, im * g];
          if (chans === 1) m[k][0][0] = m[k][1][1] = v;
          else m[k][ch][ch] = v;
          return v;
        });
        curves.push(chans === 1 ? curveOf(pts, freqs) : curveOf(pts, freqs, ch, ch));
      }
    }
    stage = stage.map((s, k) =>
      [0, 1].map((o) =>
        [0, 1].map((i) => {
          const a = mul(m[k][o][0], s[0][i]);
          const b = mul(m[k][o][1], s[1][i]);
          return [a[0] + b[0], a[1] + b[1]] as [number, number];
        }),
      ),
    );
    return { index, label: c.name || (c.matrix ? "Filter matrix" : (c.ir_path.split("/").pop() ?? c.ir_path)), ...(c.calibration ? { calibration: true } : {}), taps: 32768, curves };
  });
  const outputs = on.length === 0 ? [] : [0, 1].map((o) => curveOf(stage.map((s) => [s[o][0][0] + s[o][1][0], s[o][0][1] + s[o][1][1]]), freqs, undefined, o));
  return { rate_hz: rate, side, ...(device ? { device } : {}), freqs_hz: freqs, combined_taps: on.length ? 32768 * on.length - (on.length - 1) : 0, items, outputs };
}
