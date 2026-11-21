// RBJ Audio-EQ-Cookbook biquads, written to match
// crates/dsp-core/src/biquad.rs (`Coeffs::design`, `Coeffs::response`,
// `Eq::auto_preamp_db`) term for term, so the graph shows what the engine
// runs. `eqmath.test.ts` checks this against values produced by dsp-core.

import type { Band, EqProfile, FilterType } from "../api/types";
import { effectiveEndHz, tiltSections, tiltStageDb, tiltStageOn } from "./tilt";

export interface Coeffs {
  b0: number;
  b1: number;
  b2: number;
  a1: number;
  a2: number;
}

export const IDENTITY: Coeffs = { b0: 1, b1: 0, b2: 0, a1: 0, a2: 0 };

export function isIdentity(c: Coeffs): boolean {
  return c.b0 === 1 && c.b1 === 0 && c.b2 === 0 && c.a1 === 0 && c.a2 === 0;
}

/** Design one band; disabled or degenerate bands give `IDENTITY` (as dsp-core). */
export function design(band: Band, fs: number): Coeffs {
  if (!band.enabled) return IDENTITY;
  const f0 = band.freq_hz;
  const q = band.q;
  const gainDb = band.gain_db;
  if (!Number.isFinite(fs) || fs <= 0) return IDENTITY;
  if (!Number.isFinite(f0) || f0 <= 0 || f0 >= 0.5 * fs) return IDENTITY;
  if (!Number.isFinite(q) || q <= 0 || !Number.isFinite(gainDb)) return IDENTITY;

  const w0 = (2 * Math.PI * f0) / fs;
  const s = Math.sin(w0);
  const c = Math.cos(w0);
  const alpha = s / (2 * q);
  const a = Math.pow(10, gainDb / 40);
  let b0: number, b1: number, b2: number, a0: number, a1: number, a2: number;
  switch (band.kind) {
    case "peaking":
      [b0, b1, b2, a0, a1, a2] = [1 + alpha * a, -2 * c, 1 - alpha * a, 1 + alpha / a, -2 * c, 1 - alpha / a];
      break;
    case "low_shelf": {
      const t = 2 * Math.sqrt(a) * alpha;
      [b0, b1, b2, a0, a1, a2] = [
        a * (a + 1 - (a - 1) * c + t),
        2 * a * (a - 1 - (a + 1) * c),
        a * (a + 1 - (a - 1) * c - t),
        a + 1 + (a - 1) * c + t,
        -2 * (a - 1 + (a + 1) * c),
        a + 1 + (a - 1) * c - t,
      ];
      break;
    }
    case "high_shelf": {
      const t = 2 * Math.sqrt(a) * alpha;
      [b0, b1, b2, a0, a1, a2] = [
        a * (a + 1 + (a - 1) * c + t),
        -2 * a * (a - 1 + (a + 1) * c),
        a * (a + 1 + (a - 1) * c - t),
        a + 1 - (a - 1) * c + t,
        2 * (a - 1 - (a + 1) * c),
        a + 1 - (a - 1) * c - t,
      ];
      break;
    }
    case "low_pass":
      [b0, b1, b2, a0, a1, a2] = [(1 - c) * 0.5, 1 - c, (1 - c) * 0.5, 1 + alpha, -2 * c, 1 - alpha];
      break;
    case "high_pass":
      [b0, b1, b2, a0, a1, a2] = [(1 + c) * 0.5, -(1 + c), (1 + c) * 0.5, 1 + alpha, -2 * c, 1 - alpha];
      break;
    case "band_pass":
      [b0, b1, b2, a0, a1, a2] = [alpha, 0, -alpha, 1 + alpha, -2 * c, 1 - alpha];
      break;
    case "notch":
      [b0, b1, b2, a0, a1, a2] = [1, -2 * c, 1, 1 + alpha, -2 * c, 1 - alpha];
      break;
    case "all_pass":
      [b0, b1, b2, a0, a1, a2] = [1 - alpha, -2 * c, 1 + alpha, 1 + alpha, -2 * c, 1 - alpha];
      break;
    default:
      return IDENTITY;
  }
  if (!Number.isFinite(a0) || a0 === 0) return IDENTITY;
  const inv = 1 / a0;
  const out = { b0: b0 * inv, b1: b1 * inv, b2: b2 * inv, a1: a1 * inv, a2: a2 * inv };
  if (![out.b0, out.b1, out.b2, out.a1, out.a2].every(Number.isFinite)) return IDENTITY;
  return out;
}

/** |H(f)|² of one section. */
export function magSq(k: Coeffs, f: number, fs: number): number {
  const w = (2 * Math.PI * f) / fs;
  const c1 = Math.cos(w);
  const s1 = Math.sin(w);
  const c2 = Math.cos(2 * w);
  const s2 = Math.sin(2 * w);
  const nr = k.b0 + k.b1 * c1 + k.b2 * c2;
  const ni = -(k.b1 * s1 + k.b2 * s2);
  const dr = 1 + k.a1 * c1 + k.a2 * c2;
  const di = -(k.a1 * s1 + k.a2 * s2);
  return (nr * nr + ni * ni) / (dr * dr + di * di);
}

export function magDb(k: Coeffs, f: number, fs: number): number {
  return 10 * Math.log10(magSq(k, f, fs));
}

/** Response of a whole band list in dB (preamp included) at each frequency. */
export function responseDb(bands: readonly Band[], preampDb: number, fs: number, freqs: readonly number[]): number[] {
  const ks = bands.map((b) => design(b, fs)).filter((k) => !isIdentity(k));
  return freqs.map((f) => {
    let p = 1;
    for (const k of ks) p *= magSq(k, f, fs);
    return preampDb + 10 * Math.log10(p);
  });
}

function channelLayers(eq: EqProfile, ch: number): { sectionPreampDb: number; bands: Band[]; tilt: EqProfile } {
  const bands: Band[] = (eq.linked_enabled ?? true) ? [...eq.bands] : [];
  const extra = ch === 0 ? eq.left : ch === 1 ? eq.right : null;
  let sectionPreampDb = 0;
  if (extra?.enabled) {
    sectionPreampDb = extra.preamp_db;
    bands.push(...extra.bands);
  }
  return { sectionPreampDb, bands, tilt: eq };
}

/** `a + tilt` at each frequency (the tilt is linked: every channel has it; both sections of a two-section tilt). */
function withTilt(db: number[], tilt: EqProfile | null, fs: number, freqs: readonly number[]): number[] {
  if (!tiltStageOn(tilt)) return db;
  const t = tiltStageDb(tilt, fs, freqs);
  return db.map((v, i) => v + t[i]);
}

/** Effective filter shape of one channel in dB (omitting global `eq.preamp_db`, including enabled section preamp). */
export function channelShapeDb(eq: EqProfile, ch: number, fs: number, freqs: readonly number[]): number[] {
  const { sectionPreampDb, bands, tilt } = channelLayers(eq, ch);
  return withTilt(responseDb(bands, sectionPreampDb, fs, freqs), tilt, fs, freqs);
}

/** Combined response of one channel in dB (global preamp + enabled linked + enabled channel section). */
export function profileChannelResponseDb(eq: EqProfile, ch: number, fs: number, freqs: readonly number[]): number[] {
  const { sectionPreampDb, bands, tilt } = channelLayers(eq, ch);
  return withTilt(responseDb(bands, eq.preamp_db + sectionPreampDb, fs, freqs), tilt, fs, freqs);
}

/** Filter shape of a single section in dB (omits global `eq.preamp_db`; includes section `preamp_db` for L/R). */
export function sectionShapeDb(eq: EqProfile, section: "linked" | "left" | "right", fs: number, freqs: readonly number[]): number[] {
  if (section === "linked") return responseDb(eq.bands, 0, fs, freqs);
  const ch = section === "left" ? eq.left : eq.right;
  if (!ch) return freqs.map(() => 0);
  return responseDb(ch.bands, ch.preamp_db, fs, freqs);
}

/** Auto preamp for one band list + optional section preamp offset (matches `Eq::channel_auto_preamp_db`). */
function channelAutoPreampDb(bands: readonly Band[], sectionPreampDb: number, fs: number, tilt: EqProfile | null = null): number {
  const nyquist = 0.5 * fs;
  if (!Number.isFinite(fs) || nyquist <= 10) return 0;
  const ks = bands.map((b) => design(b, fs)).filter((k) => !isIdentity(k));
  const tilted = tiltStageOn(tilt);
  if (ks.length === 0 && !tilted) {
    const pre = Math.min(0, -sectionPreampDb);
    return !Number.isFinite(pre) || pre > -1e-9 ? 0 : pre;
  }
  const GRID = 16384;
  const freqs: number[] = [0];
  const uEnd = Math.log(nyquist / 10);
  for (let i = 0; i < GRID; i++) freqs.push(10 * Math.exp((uEnd * i) / (GRID - 1)));
  for (const b of bands) if (b.enabled && b.freq_hz > 10 && b.freq_hz < nyquist) freqs.push(b.freq_hz);
  for (const s of tilted ? tiltSections(tilt).filter((s) => s.enabled) : []) {
    // Each section's corners, and its flat bass below 10 Hz.
    freqs.push(Math.max(1, Math.min(10, s.start_hz / 8)), s.start_hz, effectiveEndHz(s, fs));
  }
  const t = tilted ? tiltStageDb(tilt, fs, freqs) : null;
  let peakSq = 0;
  for (let i = 0; i < freqs.length; i++) {
    const f = freqs[i];
    let m = 1;
    for (const k of ks) m *= magSq(k, f, fs);
    if (t) m *= Math.pow(10, t[i] / 10);
    if (Number.isFinite(m) && m > peakSq) peakSq = m;
  }
  if (!(peakSq > 0) || !Number.isFinite(peakSq)) return 0;
  const pre = Math.min(0, -(sectionPreampDb + 10 * Math.log10(peakSq)));
  return pre > -1e-9 ? 0 : pre;
}

/**
 * Preamp (dB, <= 0) that keeps the largest boost from clipping a full-scale
 * sine. Same grid as dsp-core: DC, 16384 log points 10 Hz..Nyquist and every
 * enabled band centre.
 */
export function autoPreampDb(bands: readonly Band[], fs: number): number {
  return channelAutoPreampDb(bands, 0, fs);
}

/**
 * Global auto preamp (dB, <= 0) across both channels' combined responses
 * (linked + enabled L / R sections), matching `dsp_core::biquad::Eq::auto_preamp_db`.
 */
export function profileAutoPreampDb(eq: EqProfile, fs: number): number {
  const left = channelLayers(eq, 0);
  const right = channelLayers(eq, 1);
  return Math.min(
    channelAutoPreampDb(left.bands, left.sectionPreampDb, fs, left.tilt),
    channelAutoPreampDb(right.bands, right.sectionPreampDb, fs, right.tilt),
  );
}

/** Filter types whose gain is meaningless (graph handles sit on 0 dB). */
export function isGainless(kind: FilterType): boolean {
  return kind === "low_pass" || kind === "high_pass" || kind === "band_pass" || kind === "notch" || kind === "all_pass";
}

/** Log-spaced frequencies, inclusive. */
export function logSpace(from: number, to: number, n: number): number[] {
  const out: number[] = [];
  const a = Math.log(from);
  const b = Math.log(to);
  for (let i = 0; i < n; i++) out.push(Math.exp(a + ((b - a) * i) / (n - 1)));
  return out;
}

/** Whether effective L and R filter shapes match across the audible band. */
export function channelsMatch(eq: EqProfile, fs: number): boolean {
  const lOn = !!eq.left?.enabled && (eq.left.preamp_db !== 0 || eq.left.bands.some((b) => b.enabled));
  const rOn = !!eq.right?.enabled && (eq.right.preamp_db !== 0 || eq.right.bands.some((b) => b.enabled));
  if (!lOn && !rOn) return true;
  const freqs = logSpace(20, 20000, 64);
  for (const b of eq.left?.bands ?? []) if (b.enabled) freqs.push(b.freq_hz);
  for (const b of eq.right?.bands ?? []) if (b.enabled) freqs.push(b.freq_hz);
  const l = channelShapeDb(eq, 0, fs, freqs);
  const r = channelShapeDb(eq, 1, fs, freqs);
  return l.every((v, i) => Math.abs(v - r[i]) < 1e-6);
}

/**
 * Maximum combined EQ response (dB, including global + section preamps) across
 * left and right channels. Values > 0 dB mean full-scale audio can clip.
 */
export function profilePeakGainDb(eq: EqProfile, fs: number): number {
  const top = Math.min(20000, Math.max(100, fs * 0.495));
  const freqs = logSpace(20, top, 256);
  const pushBands = (bands: readonly Band[] | undefined) => {
    for (const b of bands ?? []) {
      if (b.enabled && b.freq_hz > 0 && b.freq_hz < fs * 0.5) freqs.push(b.freq_hz);
    }
  };
  if (eq.linked_enabled ?? true) pushBands(eq.bands);
  if (eq.left?.enabled) pushBands(eq.left.bands);
  if (eq.right?.enabled) pushBands(eq.right.bands);
  const l = profileChannelResponseDb(eq, 0, fs, freqs);
  const r = profileChannelResponseDb(eq, 1, fs, freqs);
  let maxDb = -Infinity;
  for (let i = 0; i < freqs.length; i++) {
    if (l[i] > maxDb) maxDb = l[i];
    if (r[i] > maxDb) maxDb = r[i];
  }
  return maxDb;
}
