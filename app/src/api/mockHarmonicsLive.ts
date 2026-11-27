// The mock daemon's `harmonics_live`: animated, plausible frames of music
// through the stage (a chord progression whose loudness breathes, the
// stage's harmonics following the level as a curve's do), so the "Now
// playing" panel works in the browser without an engine. Nothing here is
// measured.

import type { HarmonicsConfig, HarmonicsLive, HarmonicsLiveLayer, HarmonicsLivePart, HarmonicsLiveSplit, HarmonicsLiveState } from "./types";

const F_MIN = 10;
const BPO = 24;
const FLOOR = -160;

/** Chords (root frequencies, Hz) the mock "plays", two seconds each. */
const CHORDS = [
  [110, 220, 261.6, 329.6],
  [87.3, 174.6, 261.6, 349.2],
  [130.8, 196, 261.6, 392],
  [98, 196, 246.9, 293.7],
];

const pow = (db: number) => Math.pow(10, db / 10);
const dbOf = (p: number) => (p > 0 ? Math.max(FLOOR, 10 * Math.log10(p)) : FLOOR);

function emptyFrame(state: HarmonicsLiveState, seq: number, rate: number): HarmonicsLive {
  return {
    state,
    seq,
    rate,
    window_ms: 0,
    interval_ms: 200,
    f_min_hz: F_MIN,
    bins_per_octave: BPO,
    music: [],
    added: [],
    added_db: FLOOR,
    added_pct: 0,
    added_peak_db: FLOOR,
    level_change_db: 0,
    linear_gain_db: 0,
    music_rms_dbfs: FLOOR,
    music_level_dbfs: FLOOR,
    music_peak_dbfs: FLOOR,
    centroid_hz: 0,
    layers: [],
    split: "total",
    parts: [],
    analysis_ms: 0,
  };
}

/** Each order's level at 0 dBFS, dBc, power-summed over the stage's Chebyshev layers (H2..H9), and how the split would go. */
function stageOrders(c: HarmonicsConfig): { orders: number[]; split: HarmonicsLiveSplit; note?: string; sag: { layer: number; depth: number }[]; memory: boolean } {
  const p = new Array(8).fill(0);
  let memory = false;
  let note: string | undefined;
  const sag: { layer: number; depth: number }[] = [];
  c.layers.forEach((l, i) => {
    if (l.enabled === false) return;
    if (l.kind === "chebyshev") {
      if (l.amount <= 0) return;
      const amt = 20 * Math.log10(l.amount);
      l.levels_db.forEach((v, k) => {
        if (v !== null && v !== undefined) p[k] += pow(v + amt + (k + 1) * (l.drive_db ?? 0));
      });
      if ((l.sag?.depth_db ?? 0) > 0 || (l.bias?.depth ?? 0) !== 0) {
        memory = true;
        note ??= "sag and bias move the curve with the music's envelope";
        if ((l.sag?.depth_db ?? 0) > 0) sag.push({ layer: i, depth: (l.sag?.depth_db ?? 0) * l.amount });
      }
      if (l.tilt) {
        memory = true;
        note ??= "tilted orders pass through filters, which have memory";
      }
    } else {
      memory = true;
      // Flux and tape: H3 mostly, some H2 for tape.
      const h3 = l.target_h3_db;
      p[1] += pow(h3);
      p[0] += pow(h3 - 12);
      note ??= l.kind === "flux" ? "a transformer layer has memory (its shelves): its products depend on what came before" : "a tape layer has memory (hysteresis): its products depend on what came before";
    }
  });
  const series = c.routing === "series";
  const active = c.layers.filter((l) => l.enabled !== false && (l.kind !== "chebyshev" || l.amount > 0)).length;
  const split: HarmonicsLiveSplit = memory ? "total" : series && active > 1 ? "even_odd" : "orders";
  return { orders: p.map(dbOf), split, note: memory ? note : undefined, sag, memory };
}

/**
 * A frame at `tMs` (ms since the mock started) for chain A's stage `c` at
 * the output `rate`: `playing` whether music runs through the chain.
 */
export function mockHarmonicsLive(c: HarmonicsConfig | null | undefined, rate: number, playing: boolean, tMs: number, seq: number): HarmonicsLive {
  if (!playing) return emptyFrame("idle", seq, rate);
  if (!c || !c.enabled || c.layers.every((l) => l.enabled === false)) return emptyFrame("off", seq, rate);
  const t = tMs / 1000;
  // A quiet bridge every 30 s (silence for 2 s).
  if (t % 30 > 28) {
    const f = emptyFrame("silent", seq, rate);
    f.music_rms_dbfs = -110;
    f.music_level_dbfs = -107;
    f.music_peak_dbfs = -100;
    return f;
  }
  const level = -16 + 6 * Math.sin((2 * Math.PI * t) / 11) + 3 * Math.sin((2 * Math.PI * t) / 1.7) + 2 * Math.sin((2 * Math.PI * t) / 0.43);
  const chord = CHORDS[Math.floor(t / 2) % CHORDS.length];
  const { orders, split, note, sag } = stageOrders(c);
  const load = Math.max(0, Math.min(1, (level + 30) / 30));
  const sagDb = sag.map((s) => ({ layer: s.layer, sag_db: Math.round(s.depth * Math.pow(load, 1.5) * 1000) / 1000 }));
  const deepest = sagDb.reduce((m, s) => Math.max(m, s.sag_db), 0);
  // Order k relative to the fundamental falls (k − 1) dB per dB below full
  // scale (each layer's levels are at 0 dBFS), a little more under sag.
  const rel = orders.map((v, i) => (v <= FLOOR + 1 ? FLOOR : v + (i + 1) * (level - deepest)));
  const totalPow = rel.reduce((s, v) => s + pow(v), 0);
  const added = dbOf(totalPow);

  const nyq = rate / 2;
  const count = Math.floor(BPO * Math.log2(nyq / F_MIN)) + 1;
  const freqs = Array.from({ length: count }, (_, k) => F_MIN * Math.pow(2, k / BPO));
  // The music: pink-ish with a bass hump, the chord's partials on top, a
  // steep fall above 18 kHz (the recording's band).
  const music = freqs.map((f) => {
    let p = pow(level - 18 - 3 * Math.log2(Math.max(f, 30) / 200) - (f > 18000 ? 60 * Math.log2(f / 18000) : 0) + 4 * Math.exp(-Math.pow(Math.log2(f / 80), 2)));
    for (const root of chord)
      for (let h = 1; h <= 8; h++) {
        const fh = root * h;
        const d = Math.abs(Math.log2(f / fh)) * BPO;
        if (d < 1) p += pow(level - 6 - 4 * (h - 1)) * (1 - d);
      }
    return dbOf(p);
  });
  // The added content: each order k moves the music up by k (its harmonics
  // and the intermodulation around them, smeared over a few bins).
  const addedSpec = freqs.map((f) => {
    let p = 0;
    rel.forEach((r, i) => {
      if (r <= FLOOR) return;
      const k = i + 2;
      const src = f / k;
      if (src < F_MIN) return;
      const j = Math.round(BPO * Math.log2(src / F_MIN));
      const m = music[Math.max(0, Math.min(count - 1, j))];
      p += pow(m + r - 3);
    });
    return dbOf(p);
  });
  const round1 = (v: number) => Math.round(v * 10) / 10;
  let parts: HarmonicsLivePart[] = [];
  if (split === "orders")
    parts = rel
      .map((r, i) => ({ key: `h${i + 2}`, order: i + 2, level_db: Math.round(r * 100) / 100, peak_db: Math.round((r + 4 + 2 * Math.sin(t * 3 + i)) * 100) / 100 }))
      .filter((p) => p.level_db > FLOOR);
  else if (split === "even_odd") {
    const even = dbOf(rel.filter((_, i) => i % 2 === 0).reduce((s, v) => s + pow(v), 0));
    const odd = dbOf(rel.filter((_, i) => i % 2 === 1).reduce((s, v) => s + pow(v), 0));
    parts = [
      { key: "even", level_db: Math.round(even * 100) / 100, peak_db: Math.round((even + 4) * 100) / 100 },
      { key: "odd", level_db: Math.round(odd * 100) / 100, peak_db: Math.round((odd + 4) * 100) / 100 },
    ];
  }
  let num = 0;
  let den = 0;
  music.forEach((v, i) => {
    if (freqs[i] >= 20 && freqs[i] <= 20000) {
      const w = pow(v) * freqs[i] * (Math.pow(2, 1 / BPO) - 1);
      num += freqs[i] * w;
      den += w;
    }
  });
  const layers: HarmonicsLiveLayer[] = sagDb.map((s) => ({ layer: s.layer, sag_db: s.sag_db, bias: 0 }));
  const n = Math.pow(2, Math.floor(Math.log2(rate / 8)));
  return {
    ...emptyFrame("playing", seq, rate),
    window_ms: Math.round((n / rate) * 1e5) / 100,
    music: music.map(round1),
    added: addedSpec.map(round1),
    added_db: Math.round(added * 100) / 100,
    added_pct: Math.round(100 * Math.pow(10, added / 20) * 1e4) / 1e4,
    added_peak_db: Math.round((added + 4 + 2 * Math.sin(t * 2.3)) * 100) / 100,
    level_change_db: Math.round((-deepest - 0.02 * load) * 1e4) / 1e4,
    music_rms_dbfs: Math.round((level - 3.01) * 100) / 100,
    music_level_dbfs: Math.round(level * 100) / 100,
    music_peak_dbfs: Math.round((level + 9) * 100) / 100,
    centroid_hz: den > 0 ? Math.round((num / den) * 10) / 10 : 0,
    layers,
    split,
    split_note: note,
    parts,
    analysis_ms: rate > 400000 ? 4 : 0.5,
  };
}
