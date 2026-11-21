// Meter data lives outside the React store: it arrives at 30 Hz and is only
// read by canvas painters in requestAnimationFrame.

import type { Meters } from "../api/types";

export interface MeterFrame {
  meters: Meters;
  at: number;
}

/**
 * Compute per-channel clip counts since `baselines`, initializing missing
 * channels to their current count and resetting all baselines to 0 if the
 * daemon restarted (any channel decreased vs `prevCounts` or `baselines`).
 */
export function computeClipDeltas(
  counts: readonly number[],
  baselines: readonly number[],
  prevCounts: readonly number[] = baselines,
): { deltas: number[]; nextBaselines: number[] } {
  const restarted = counts.some((c, i) => {
    const p = prevCounts[i];
    const b = baselines[i];
    return (p !== undefined && c < p) || (b !== undefined && c < b);
  });
  const nextBaselines = counts.map((c, i) => {
    if (restarted) return 0;
    const b = baselines[i];
    return b === undefined ? c : b;
  });
  const deltas = counts.map((c, i) => Math.max(0, c - nextBaselines[i]));
  return { deltas, nextBaselines };
}

/**
 * Chain gain in dB for the hottest active channel: the chain output peak
 * before the master volume (`chain_peak_db`) minus the chain input peak. It
 * is a relative gain (what EQ, convolution and the upsampler add at the
 * peaks), not a level. Null when silent or when the chain is not in the path
 * (Direct sink, bypass: `chain_peak_db` is empty).
 */
export function meterChainGainDb(m: Meters): number | null {
  const inp = m.input_peak_db;
  const out = m.chain_peak_db;
  if (!inp || inp.length === 0 || !out || out.length === 0) return null;
  let bestOut = -Infinity;
  let bestDelta: number | null = null;
  const n = Math.min(out.length, inp.length);
  for (let i = 0; i < n; i++) {
    const o = out[i];
    const x = inp[i];
    if (!Number.isFinite(o) || !Number.isFinite(x) || x <= -75 || o <= -90) continue;
    if (o > bestOut) {
      bestOut = o;
      bestDelta = o - x;
    }
  }
  return bestDelta;
}

/**
 * A report that draws nothing: every channel's peak and RMS at or below
 * `floorDb` and no limiter gain reduction. The meters may sleep through a
 * run of these (silence still arrives at 30 Hz).
 */
export function quietMeters(m: Meters, floorDb: number): boolean {
  const low = (v: number[] | undefined) => !v || v.every((db) => !Number.isFinite(db) || db <= floorDb);
  return low(m.peak_db) && low(m.rms_db) && !(m.limiter?.active && m.limiter.gain_reduction_db >= 0.05);
}

/** Bar state that has nothing left to animate: peak, RMS and hold all rest at the floor. */
export function metersSettled(ch: readonly { peak: number; rms: number; hold: number }[], floorDb: number): boolean {
  return ch.every((c) => c.peak <= floorDb && c.hold <= floorDb && c.rms - floorDb < 0.05);
}

class MeterStore {
  latest: MeterFrame | null = null;
  private listeners = new Set<() => void>();

  /** Called after every report, clip reset or clear (wakes a sleeping painter). */
  onChange(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private changed() {
    for (const fn of this.listeners) fn();
  }
  /** `clipped` count at the time the clip light was last reset. */
  clipBaseline: number | null = null;
  /** Per-channel `clipped_ch` counts at the time each channel LED was last reset. */
  clipBaselines: number[] = [];
  /** Previous `clipped` and `clipped_ch` counts seen, used to detect daemon restarts. */
  prevClipped: number | null = null;
  prevClippedCh: number[] = [];
  /** performance.now() of the last increase of `clipped`. */
  lastClipAt = -Infinity;
  /**
   * Per channel: meter frames whose peak reached 0 dBFS without the daemon
   * counting a clipped sample (a sample exactly at full scale, or a level the
   * integer path cannot show). They light the clip LED like a clip.
   */
  overs: number[] = [];

  push(m: Meters, now: number) {
    const prev = this.prevClipped;
    const chRestarted =
      !!m.clipped_ch &&
      m.clipped_ch.some((c, i) => {
        const p = this.prevClippedCh[i];
        const b = this.clipBaselines[i];
        return (p !== undefined && c < p) || (b !== undefined && c < b);
      });
    const totalDecreased =
      (prev !== null && m.clipped < prev) ||
      (this.clipBaseline !== null && m.clipped < this.clipBaseline);
    const restarted = totalDecreased || chRestarted;

    if (restarted) {
      this.clipBaseline = 0;
    } else if (this.clipBaseline === null) {
      this.clipBaseline = m.clipped;
    }

    const prevCh = this.prevClippedCh;
    m.peak_db.forEach((db, i) => {
      if (!(Number.isFinite(db) && db >= 0)) return;
      const counted = m.clipped_ch?.[i] !== undefined && prevCh[i] !== undefined && m.clipped_ch[i] > prevCh[i];
      if (!counted) {
        while (this.overs.length <= i) this.overs.push(0);
        this.overs[i]++;
        this.lastClipAt = now;
      }
    });

    if (m.clipped_ch && m.clipped_ch.length > 0) {
      if (restarted) {
        this.clipBaselines = m.clipped_ch.map(() => 0);
      } else {
        const { nextBaselines } = computeClipDeltas(m.clipped_ch, this.clipBaselines, this.prevClippedCh);
        this.clipBaselines = nextBaselines;
      }
      this.prevClippedCh = [...m.clipped_ch];
    } else {
      this.prevClippedCh = [];
    }

    if ((prev !== null && m.clipped > prev) || (restarted && m.clipped > 0)) {
      this.lastClipAt = now;
    }
    this.prevClipped = m.clipped;
    this.latest = { meters: m, at: now };
    this.changed();
  }

  /** Clipped samples per channel since each channel LED was last reset. */
  clipsPerChannel(channels = 2): number[] {
    const m = this.latest?.meters;
    if (!m) return Array(channels).fill(0);
    const out: number[] =
      m.clipped_ch && m.clipped_ch.length > 0 ? computeClipDeltas(m.clipped_ch, this.clipBaselines, this.prevClippedCh).deltas : [this.clipsSinceReset()];
    while (out.length < channels) out.push(0);
    return out.map((c, i) => c + (this.overs[i] ?? 0));
  }

  /** Clipped samples since the light was last reset. */
  clipsSinceReset(): number {
    if (!this.latest) return 0;
    const ch = this.latest.meters.clipped_ch;
    if (ch && ch.length > 0) {
      return computeClipDeltas(ch, this.clipBaselines, this.prevClippedCh).deltas.reduce((a, b) => a + b, 0);
    }
    if (this.clipBaseline === null) return 0;
    return Math.max(0, this.latest.meters.clipped - this.clipBaseline);
  }

  resetClip(channel?: number) {
    const m = this.latest?.meters;
    if (channel === undefined) this.overs = [];
    else if (channel < this.overs.length) this.overs[channel] = 0;
    if (channel === undefined) {
      this.clipBaseline = m?.clipped ?? null;
      this.clipBaselines = m?.clipped_ch ? [...m.clipped_ch] : [];
      this.lastClipAt = -Infinity;
      this.changed();
      return;
    }
    if (m?.clipped_ch && channel >= 0 && channel < m.clipped_ch.length) {
      const next = [...this.clipBaselines];
      while (next.length < m.clipped_ch.length) next.push(m.clipped_ch[next.length]);
      next[channel] = m.clipped_ch[channel];
      this.clipBaselines = next;
    }
    if (this.clipsSinceReset() === 0) {
      this.clipBaseline = m?.clipped ?? null;
      this.lastClipAt = -Infinity;
    }
    this.changed();
  }

  clear() {
    this.latest = null;
    this.overs = [];
    this.clipBaseline = null;
    this.clipBaselines = [];
    this.prevClipped = null;
    this.prevClippedCh = [];
    this.lastClipAt = -Infinity;
    this.changed();
  }
}

export const meterStore = new MeterStore();
