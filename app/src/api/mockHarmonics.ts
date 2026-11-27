// The mock daemon's `harmonics_presets` and `harmonics_readout`: the
// engine's preset data, and closed-form estimates of the readouts (the
// engine simulates the stage; these only have to look plausible in the
// browser).

import presetData from "../../../crates/protocol/src/harmonics_presets.json";
import systemData from "../../../crates/protocol/src/harmonics_systems.json";
import { HYSTERESIS_MIN_CLEAN_RATE_HZ, harmonicsPeakDb, layerDynamic, orderShelves, sagOn } from "../lib/effects";
import type { DitherKind, HarmonicsAliasing, HarmonicsConfig, HarmonicsPreset, HarmonicsReadout, HarmonicsReadoutPoint, HarmonicsSpectrum, HarmonicsSpectrumLine, HarmonicsSpectrumPeak, HarmonicsSpectrumTrace, HarmonicsSpectrumWord, HarmonicsSystem, OrderTilt } from "./types";

export const MOCK_HARMONICS_PRESETS = presetData as unknown as HarmonicsPreset[];
export const MOCK_HARMONICS_SYSTEMS = systemData as unknown as HarmonicsSystem[];

const FLOOR = -200;
/** The engine's third-octave frequencies, 20 Hz to 20 kHz. */
const SWEEP_FREQS = [20, 25, 31.5, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600, 2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000, 20000];
const N = 256;
const db = (v: number) => Math.max(FLOOR, 20 * Math.log10(Math.abs(v)) || FLOOR);

/** Cosine-phase amplitudes [DC, H1..H9] of `f(cos θ)`. */
function project(f: (x: number) => number): number[] {
  const out = new Array(10).fill(0);
  for (let i = 0; i < N; i++) {
    const th = (2 * Math.PI * i) / N;
    const y = f(Math.cos(th));
    for (let k = 0; k < 10; k++) out[k] += y * Math.cos(k * th);
  }
  return out.map((v, k) => (v * (k === 0 ? 1 : 2)) / N);
}

/** |G_k(k f)| of a tilt's shelves (analog Butterworth shelves of order slope / 6, as fundamental frequencies), unity at 1 kHz. */
function tiltGain(o: OrderTilt, f: number): number {
  const shelf = (f: number) => {
    let g = 1;
    if (o.low.db !== 0) {
      const n = (o.low.slope ?? 12) / 6;
      const near = o.low.hz;
      const far = near / Math.pow(10, Math.abs(o.low.db) / (20 * n));
      const [z, p] = o.low.db > 0 ? [near, far] : [far, near];
      g *= Math.sqrt((z ** (2 * n) + f ** (2 * n)) / (p ** (2 * n) + f ** (2 * n)));
    }
    if (o.high.db !== 0) {
      const n = (o.high.slope ?? 6) / 6;
      const near = o.high.hz;
      const far = near * Math.pow(10, Math.abs(o.high.db) / (20 * n));
      const [z, p] = o.high.db > 0 ? [near, far] : [far, near];
      g *= Math.sqrt((1 + (f / z) ** (2 * n)) / (1 + (f / p) ** (2 * n)));
    }
    return g;
  };
  return shelf(f) / shelf(1000);
}

/** Signed harmonic amplitudes H2..H15 relative to the fundamental, for a sine at `amp` and `freq`. */
function harmonics(c: HarmonicsConfig, amp: number, freq: number): number[] {
  const h = new Array(14).fill(0);
  for (const l of c.layers) {
    if (!l.enabled) continue;
    if (l.kind === "chebyshev") {
      const a = l.levels_db.map((v, i) => {
        if (v === null || v === undefined) return 0;
        let s = l.signs[i] === "-" ? -1 : 1;
        if (l.invert_even && i % 2 === 0) s = -s;
        return s * l.amount * Math.pow(10, v / 20);
      });
      // The curve x + Σ a_k T_k(x), read at the drive's operating point.
      const curve = (x: number) => a.reduce((y, ak, i) => y + ak * Math.cos((i + 2) * Math.acos(Math.max(-1, Math.min(1, x)))), x);
      const d = Math.pow(10, l.drive_db / 20);
      const fund = project((x) => curve(d * x))[1];
      const at = project((x) => curve(d * amp * x) / fund);
      for (let k = 0; k < 8; k++) h[k] += (at[k + 2] / amp) * (l.tilt ? tiltGain(orderShelves(l.tilt, k + 2), freq) : 1);
    } else if (l.kind === "flux") {
      const boost = Math.pow(10, l.max_boost_db / 20);
      const shelf = (f: number) => Math.min(boost, Math.max(1, l.corner_hz / f));
      const r = shelf(freq) / shelf(l.calibration_hz);
      h[1] -= Math.pow(10, l.target_h3_db / 20) * amp * amp * r * r;
    } else if (l.amount > 0) {
      // Blend Rayleigh pinning (~amp) and Langevin knee (~amp²) by width/saturation.
      const base = Math.pow(10, l.target_h3_db / 20) * l.amount;
      const pin = l.width * (1 - 0.6 * l.saturation);
      h[1] -= base * (pin * amp + (1 - pin) * amp * amp);
      h[3] += base * 0.15 * amp * amp;
    }
  }
  // Series: a rough stand-in for the cross terms above H9.
  if (c.routing === "series" && c.layers.filter((l) => l.enabled).length > 1)
    for (let k = 8; k < 14; k++) h[k] += (1e-4 * amp * amp) / (k - 6);
  return h;
}

/** GedLee Gm of the static curve with harmonic levels `h` (H2..H15). */
function gedleeGm(h: number[]): number {
  const c = (x: number) => h.reduce((y, hk, i) => y + hk * Math.cos((i + 2) * Math.acos(x)), x);
  const M = 400;
  const e = 1e-3;
  let sum = 0;
  for (let i = 1; i < M; i++) {
    const x = -1 + (2 * i) / M;
    if (Math.abs(x) > 1 - 2 * e) continue;
    const d2 = (c(x + e) - 2 * c(x) + c(x - e)) / (e * e);
    const w = Math.cos((Math.PI * x) / 2) ** 2;
    sum += (w * d2) ** 2 * (2 / M);
  }
  return Math.sqrt(sum);
}

function point(c: HarmonicsConfig, input_dbfs: number, freq_hz: number, rate: number): HarmonicsReadoutPoint {
  const all = c.enabled ? harmonics(c, Math.pow(10, input_dbfs / 20), freq_hz) : new Array(14).fill(0);
  // Orders above Nyquist fold back: listed apart, out of the harmonics and THD.
  const above = (i: number) => (i + 2) * freq_hz > rate / 2;
  const h = all.map((v, i) => (above(i) ? 0 : v));
  const aliased = all.flatMap((v, i) => {
    if (!above(i) || db(v) <= FLOOR) return [];
    const f = ((i + 2) * freq_hz) % rate;
    return [{ order: i + 2, freq_hz: Math.min(f, rate - f), level_db: db(v) }];
  });
  const thd = Math.sqrt(h.reduce((s, v) => s + v * v, 0));
  // Nothing trims the stage: the fundamental passes at about unity.
  const gain_db = 0;
  const p: HarmonicsReadoutPoint = { input_dbfs, freq_hz, thd_db: db(thd), gedlee_gm: gedleeGm(h), harmonics_db: h.map(db) as HarmonicsReadoutPoint["harmonics_db"], gain_db };
  if (aliased.length) p.aliased = aliased;
  return p;
}

/** The measured aliasing, roughly: the orders of a 0 dBFS 20 kHz tone that
 * fold into 20 Hz–20 kHz (H2–H15 of the parallel sum), and a tape layer below
 * its clean rate at about −40 dBc. */
function mockAliasing(c: HarmonicsConfig, rate: number, tapeBelowClean: boolean): HarmonicsAliasing {
  const tone_hz = 20000;
  const folded = c.enabled ? (point(c, 0, tone_hz, rate).aliased ?? []).filter((a) => a.freq_hz >= 20 && a.freq_hz <= 20000) : [];
  if (tapeBelowClean) folded.push({ order: 5, freq_hz: 4000, level_db: -40 });
  const power = folded.reduce((s, a) => s + Math.pow(10, a.level_db / 10), 0);
  const worst = folded.reduce<(typeof folded)[number] | undefined>((t, a) => (!t || a.level_db > t.level_db ? a : t), undefined);
  const level_db = power > 0 ? Math.max(FLOOR, 10 * Math.log10(power)) : FLOOR;
  return { tone_hz, level_db, worst_hz: worst?.freq_hz ?? 0, worst_db: worst?.level_db ?? FLOOR, warn: level_db > -60 };
}

/** `sweepFreq` / `sweepLevel`: the sweeps' one tone and level (as `sweep_freq_hz` / `sweep_level_dbfs`). */
export function mockHarmonicsReadout(c: HarmonicsConfig, rate: number, sweep: boolean, sweepFreq?: number, sweepLevel?: number): HarmonicsReadout {
  const freqs = [1000, 40];
  const points = [0, -18].flatMap((l) => freqs.map((f) => point(c, l, f, rate)));
  const belowClean: { layer: number; clean_rate_hz: number }[] = [];
  if (c.enabled)
    c.layers.forEach((l, j) => {
      if (l.kind === "hysteresis" && l.enabled && l.amount > 0 && rate < HYSTERESIS_MIN_CLEAN_RATE_HZ) belowClean.push({ layer: j, clean_rate_hz: HYSTERESIS_MIN_CLEAN_RATE_HZ });
    });
  const res: HarmonicsReadout = {
    rate_hz: rate,
    level_change_db: 0,
    peak_gain_db: c.enabled ? harmonicsPeakDb(c) : 0,
    small_signal_gain_db: 0,
    aliasing: mockAliasing(c, rate, belowClean.length > 0),
    points,
  };
  if (belowClean.length) res.below_clean_rate = belowClean;
  if (sweep) {
    if (sweepFreq !== undefined && !(sweepFreq >= 10 && sweepFreq <= 20000)) throw new Error(`harmonics: sweep_freq_hz must be in 10..=20000 (got ${sweepFreq})`);
    if (sweepLevel !== undefined && !(sweepLevel >= -120 && sweepLevel <= 0)) throw new Error(`harmonics: sweep_level_dbfs must be in -120..=0 (got ${sweepLevel})`);
    res.sweep = (sweepFreq !== undefined ? [sweepFreq] : freqs).flatMap((f) => Array.from({ length: 21 }, (_, i) => point(c, -40 + 2 * i, f, rate)));
    res.freq_sweep = (sweepLevel !== undefined ? [sweepLevel] : [0, -18]).flatMap((l) => SWEEP_FREQS.map((f) => point(c, l, f, rate)));
    // Under load, roughly: the deepest sag (depth × amount; one shared
    // supply) lowers the drive by its depth times the sine's level, so H_k
    // falls by (k − 1) dB per dB of it.
    const sag = Math.max(0, ...c.layers.map((l) => (l.kind === "chebyshev" && l.enabled && sagOn(l.sag) ? l.sag!.depth_db * l.amount : 0)));
    if (c.enabled && c.layers.some(layerDynamic))
      res.under_load = res.sweep.map((p) => {
        const drop = sag * Math.pow(10, p.input_dbfs / 20);
        const harmonics_db = p.harmonics_db.map((h, k) => (h <= -199 ? h : h - (k + 1) * drop)) as HarmonicsReadoutPoint["harmonics_db"];
        return { ...p, gain_db: p.gain_db - drop, thd_db: p.thd_db - drop, harmonics_db };
      });
  }
  return res;
}

/** The engine's dither variances (LSB², unshaped), for the mock floor. */
const DITHER_VAR: Partial<Record<DitherKind, number>> = { none: 0, feedback1: 1 / 24, rectangular: 1 / 3, gaussian: 0.108149, highpass_tpdf: 0.0457 };
const SPECTRUM_POINTS = 2048;

/** The config one spectrum trace runs (as the engine builds it). */
function traceConfig(c: HarmonicsConfig, t: HarmonicsSpectrumTrace): HarmonicsConfig {
  if (t.kind === "layer") {
    const l = c.layers[t.layer];
    if (!l) throw new Error(`harmonics: trace: no layer ${t.layer}`);
    return { ...c, enabled: true, layers: [{ ...l, enabled: true }] };
  }
  if (t.kind === "after") {
    if (!c.layers[t.layer]) throw new Error(`harmonics: trace: no layer ${t.layer}`);
    if (c.routing !== "series") throw new Error("harmonics: trace: `after` needs series routing");
    return { ...c, layers: c.layers.slice(0, t.layer + 1) };
  }
  return c;
}

/**
 * The mock daemon's `harmonics_spectrum`: H2–H15 of the readout's estimate
 * where they land (folded above Nyquist), each one bin of the exact
 * (unwindowed) spectrum, and the output word's floor as the engine computes it.
 */
export function mockHarmonicsSpectrum(c: HarmonicsConfig, rate: number, freqHz: number, levelDbfs: number, traces: HarmonicsSpectrumTrace[], word: HarmonicsSpectrumWord): HarmonicsSpectrum {
  if (!(freqHz >= 10 && freqHz <= 0.45 * rate)) throw new Error(`harmonics: freq_hz must be in 10..=${0.45 * rate} (got ${freqHz})`);
  if (!(levelDbfs >= -120 && levelDbfs <= 0)) throw new Error(`harmonics: level_dbfs must be in -120..=0 (got ${levelDbfs})`);
  const width = Math.min(12, Math.max(1, freqHz / 16));
  const n = Math.min(1 << 19, Math.max(1 << 12, 2 ** Math.ceil(Math.log2(rate / width))));
  const binHz = rate / n;
  let bin = Math.max(1, Math.round(freqHz / binHz));
  if (bin % 2 === 0) bin += freqHz / binHz >= bin ? 1 : -1;
  const f0 = bin * binHz;
  const amp = Math.pow(10, levelDbfs / 20);
  const nyq = rate / 2;
  const freqs = Array.from({ length: SPECTRUM_POINTS }, (_, i) => 10 * Math.pow(nyq / 10, i / (SPECTRUM_POINTS - 1)));
  const lsb = Math.pow(2, 1 - word.bits);
  const noise = lsb * lsb * (1 / 12 + (DITHER_VAR[word.dither] ?? 1 / 6));
  const lines: HarmonicsSpectrumLine[] = (traces.length ? traces : [{ kind: "stage" } as HarmonicsSpectrumTrace]).map((trace) => {
    const tc = traceConfig(c, trace);
    const h = tc.enabled ? harmonics(tc, amp, f0) : new Array(14).fill(0);
    const gain_db = 0;
    const peaks: HarmonicsSpectrumPeak[] = h.flatMap((v, i) => {
      const level_db = db(v);
      if (level_db <= FLOOR) return [];
      const k = i + 2;
      const raw = (k * f0) % rate;
      return [{ order: k, freq_hz: Math.min(raw, rate - raw), level_db, aliased: k * f0 > nyq }];
    });
    const all = [{ freq_hz: f0, level_db: 0 }, ...peaks];
    const half = Math.sqrt(freqs[1] / freqs[0]);
    const levels_db = freqs.map((f) => {
      let v = -250;
      for (const p of all) {
        // Peak held over the point's span, as the engine plots it.
        if (p.freq_hz >= f / half && p.freq_hz < f * half) v = Math.max(v, p.level_db);
      }
      return Math.round(v * 100) / 100;
    });
    const out = amp * Math.pow(10, gain_db / 20) * Math.pow(10, -Math.max(0, word.headroom_db) / 20);
    const fundP = (out * out) / 2;
    const thd = peaks.filter((p) => !p.aliased).reduce((s, p) => s + Math.pow(10, p.level_db / 10), 0);
    const band = peaks.filter((p) => p.freq_hz >= 20 && p.freq_hz <= 20000).reduce((s, p) => s + Math.pow(10, p.level_db / 10), 0);
    const bandFrac = Math.max(0, (Math.min(20000, nyq) - 20) / nyq);
    const p10 = (p: number, floor: number) => (p > 0 ? Math.max(floor, 10 * Math.log10(p)) : floor);
    const wave = Array.from({ length: 512 }, (_, i) => {
      const th = (2 * Math.PI * i) / 512;
      return amp * Math.pow(10, gain_db / 20) * (Math.cos(th) + h.reduce((y, hk, k) => y + hk * Math.cos((k + 2) * th), 0));
    });
    return {
      trace,
      gain_db,
      thd_db: p10(thd, FLOOR),
      thd_n_db: p10(band + (noise * bandFrac) / fundP, FLOOR),
      // Unwindowed: a partial of peak A gives |X| = A n / 2, white noise E|X|² = σ² n.
      floor_db: p10((noise * n) / Math.pow((out * n) / 2, 2), -250),
      levels_db,
      peaks,
      wave,
    };
  });
  return { rate_hz: rate, freq_hz: f0, level_dbfs: levelDbfs, bin_hz: binHz, word, freqs_hz: freqs, lines };
}
