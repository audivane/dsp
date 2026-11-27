// Closed-form sketches of single harmonics layers for the layer cards'
// explainers: a Chebyshev layer's curve (as the engine applies it), its sag
// and bias envelopes on a test signal, and a flux layer's saturator with its
// shelf. The engine runs the real thing; these follow its model
// (dsp-core harmonics.rs, tube_dynamics.rs, flux.rs) in analog form.

import type { ChebyshevLayer, FluxLayer } from "../api/types";

/** T_k(x) by the recurrence (valid beyond ±1, where the bias reaches). */
export function chebT(k: number, x: number): number {
  if (k === 0) return 1;
  let a = 1;
  let b = x;
  for (let i = 1; i < k; i++) [a, b] = [b, 2 * x * b - a];
  return b;
}

/** Cosine amplitudes [DC, H1, …, H`orders`] of one period of `f(cos θ)`. */
export function project(f: (x: number) => number, orders = 9, n = 128): number[] {
  const out = new Array(orders + 1).fill(0);
  for (let i = 0; i < n; i++) {
    const th = (2 * Math.PI * i) / n;
    const y = f(Math.cos(th));
    for (let k = 0; k <= orders; k++) out[k] += y * Math.cos(k * th);
  }
  return out.map((v, k) => (v * (k === 0 ? 1 : 2)) / n);
}

/** The signed reference amplitudes [a2..a9] of the layer's levels (drive 0 dB, amount 1). */
export function chebReference(l: ChebyshevLayer): number[] {
  return l.levels_db.map((v, i) => {
    if (v === null || v === undefined) return 0;
    let s = l.signs[i] === "-" ? -1 : 1;
    if (l.invert_even && i % 2 === 0) s = -s;
    return s * Math.pow(10, v / 20);
  });
}

/** x + Σ a_k T_k(x) with `a` = [a2..a9]. */
export function chebPoly(a: readonly number[], x: number): number {
  return a.reduce((y, ak, i) => y + ak * chebT(i + 2, x), x);
}

/**
 * The amplitudes [a2..a9] the stage applies (the engine's
 * `layer_amplitudes`): the reference curve's harmonics at the drive's level
 * relative to its fundamental there, times the amount.
 */
export function chebApplied(l: ChebyshevLayer): number[] {
  const ref = chebReference(l);
  const d = Math.pow(10, Math.min(0, l.drive_db) / 20);
  if (d >= 1) return ref.map((a) => a * l.amount);
  const h = project((x) => chebPoly(ref, d * x));
  if (Math.abs(h[1]) < 1e-9) return ref.map(() => 0);
  return ref.map((_, i) => (h[i + 2] / h[1]) * l.amount);
}

/** The static curve c(v) = v + S(v) − S(0) (through the origin). */
export function chebCurve(a: readonly number[], v: number): number {
  return chebPoly(a, v) - chebPoly(a, 0);
}

/** Harmonic levels H2..H`orders` (dB relative to the fundamental) of `f` for a cosine at peak `amp`; −200 when none. */
export function harmonicsDb(f: (x: number) => number, amp: number, orders = 5): number[] {
  const h = project((x) => f(amp * x), orders);
  const h1 = Math.abs(h[1]) || 1e-30;
  return h.slice(2).map((v) => Math.max(-200, 20 * Math.log10(Math.abs(v) / h1 + 1e-300)));
}

/** One envelope-follower step (the engine's `follow`). */
const follow = (e: number, r: number, att: number, rel: number) => e + (r > e ? att : rel) * (r - e);
const onePole = (ms: number, fs: number) => -Math.expm1(-1 / (ms * 1e-3 * fs));

/** The quiet → loud → short hit test signal: [start s, level dBFS] steps, 4 s in all. */
export const SAG_TEST: readonly (readonly [number, number])[] = [
  [0, -20],
  [0.5, 0],
  [1.9, -20],
  [2.7, 0],
  [2.85, -20],
];
export const SAG_TEST_SECONDS = 4;

export interface SagTrace {
  /** Times of the points, s. */
  t: number[];
  /** The input's peak level, dBFS. */
  inputDb: number[];
  /** The level into the curve after the sag, dB (the input times the mean drive). */
  driveDb: number[];
  /** The operating point the bias moved to (fraction of full scale). */
  bias: number[];
  /** H2 and H3 (dB rel. the fundamental) with the envelopes, and with them at rest. */
  h2: number[];
  h3: number[];
  h2Rest: number[];
  h3Rest: number[];
}

/**
 * The layer's sag and bias on {@link SAG_TEST} (a 100 Hz tone at 9.6 kHz, one
 * point per cycle): the engine's envelopes of |x| (attack, release), drive
 * d = 1 − (1 − 10^(−D/20)) e_s and bias point b = β e_b, D and β times the
 * amount, and the harmonics of the curve at that drive and operating point,
 * tangent-compensated (the bias reach limit is ignored).
 */
export function sagTrace(l: ChebyshevLayer): SagTrace {
  const fs = 9600;
  const f0 = 100;
  const per = fs / f0;
  const a = chebApplied(l);
  const sagDb = l.sag ? l.sag.depth_db * l.amount : 0;
  const beta = l.bias ? l.bias.depth * l.amount : 0;
  const sagK = 1 - Math.pow(10, -sagDb / 20);
  const [sa, sr] = l.sag ? [onePole(l.sag.attack_ms, fs), onePole(l.sag.release_ms, fs)] : [0, 0];
  const [ba, br] = l.bias ? [onePole(l.bias.attack_ms, fs), onePole(l.bias.release_ms, fs)] : [0, 0];
  // S(v) = Σ a_k T_k(v); the curve around the operating point b, its tangent removed.
  const S = (v: number) => chebPoly(a, v) - v;
  const dS = (b: number) => (S(b + 1e-5) - S(b - 1e-5)) / 2e-5;
  const L = 1 + (S(1e-5) - S(-1e-5)) / 2e-5;
  const shaped = (b: number) => {
    const sb = S(b);
    const slope = dS(b);
    return (u: number) => L * u + S(b + u) - sb - slope * u;
  };
  const levelAt = (t: number) => SAG_TEST.reduce((lv, [s, db]) => (t >= s ? db : lv), SAG_TEST[0][1]);
  const out: SagTrace = { t: [], inputDb: [], driveDb: [], bias: [], h2: [], h3: [], h2Rest: [], h3Rest: [] };
  const rest = new Map<number, number[]>();
  let es = 0;
  let eb = 0;
  // Start settled on the quiet level (as music that has been playing a while).
  const amp0 = Math.pow(10, SAG_TEST[0][1] / 20);
  for (let i = 0; i < fs * 2; i++) {
    const r = Math.abs(amp0 * Math.sin((2 * Math.PI * i) / per));
    es = follow(es, r, sa, sr);
    eb = follow(eb, r, ba, br);
  }
  const cycles = Math.round(SAG_TEST_SECONDS * f0);
  for (let c = 0; c < cycles; c++) {
    const t = c / f0;
    const db = levelAt(t);
    const amp = Math.pow(10, db / 20);
    let dSum = 0;
    let bSum = 0;
    for (let i = 0; i < per; i++) {
      const r = Math.abs(amp * Math.sin((2 * Math.PI * i) / per));
      es = follow(es, r, sa, sr);
      eb = follow(eb, r, ba, br);
      dSum += 1 - sagK * es;
      bSum += beta * eb;
    }
    const d = dSum / per;
    const b = bSum / per;
    const h = harmonicsDb(shaped(b), d * amp, 3);
    if (!rest.has(db)) rest.set(db, harmonicsDb(shaped(0), amp, 3));
    const hr = rest.get(db)!;
    out.t.push(t);
    out.inputDb.push(db);
    out.driveDb.push(db + 20 * Math.log10(d));
    out.bias.push(b);
    out.h2.push(h[0]);
    out.h3.push(h[1]);
    out.h2Rest.push(hr[0]);
    out.h3Rest.push(hr[1]);
  }
  return out;
}

/** |P(f)| of a flux layer's low shelf: DC gain 10^(boost/20), unity well above the corner. */
export function fluxShelf(l: FluxLayer, f: number): number {
  const g = Math.pow(10, l.max_boost_db / 20);
  const fc = l.corner_hz;
  return Math.sqrt((f * f + fc * fc) / (f * f + (fc / g) * (fc / g)));
}

/** D(u) = tanh(g u)/g − u, the saturator's distortion. */
const fluxD = (g: number) => (u: number) => Math.tanh(g * u) / g - u;

/** H3 (dB rel. the fundamental) of a flux layer with drive `g` for a sine at `freq` and peak `amp`. */
export function fluxH3Db(l: FluxLayer, g: number, freq: number, amp: number): number {
  const p1 = fluxShelf(l, freq);
  const p3 = fluxShelf(l, 3 * freq);
  const h = project((x) => fluxD(g)(amp * p1 * x), 3);
  const fund = Math.abs(amp + h[1] / p1);
  return Math.max(-200, 20 * Math.log10(Math.abs(h[3] / p3) / fund + 1e-300));
}

/** The saturator's drive g that puts H3 at the layer's target for a 0 dBFS sine at its calibration frequency. */
export function fluxDrive(l: FluxLayer): number {
  let lo = Math.log(1e-4);
  let hi = Math.log(1e3);
  for (let i = 0; i < 60; i++) {
    const mid = (lo + hi) / 2;
    if (fluxH3Db(l, Math.exp(mid), l.calibration_hz, 1) < l.target_h3_db) lo = mid;
    else hi = mid;
  }
  return Math.exp((lo + hi) / 2);
}

/** The flux layer's static curve for a sine at `freq`: y = x + D(|P| x) / |P| (phase ignored). */
export function fluxCurve(l: FluxLayer, g: number, freq: number, x: number): number {
  const p = fluxShelf(l, freq);
  return x + fluxD(g)(p * x) / p;
}

/** A "nice" magnification (1, 2, 5 × 10^n) that brings `peak` to about `target`. */
export function niceGain(peak: number, target: number): number {
  if (!(peak > 0)) return 1;
  const raw = target / peak;
  if (raw <= 1) return 1;
  const e = Math.pow(10, Math.floor(Math.log10(raw)));
  const m = raw / e;
  return (m >= 5 ? 5 : m >= 2 ? 2 : 1) * e;
}
