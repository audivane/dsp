// Slider tapers: the volume and balance sliders work in dB, not in travel.
//
// A `Taper` maps a slider position (0..1, left to right) to a dB value and
// back. Both directions are exact inverses, so the value a person sees is the
// value they dragged to, and keyboard/wheel steps move it by whole 0.1 dB.

export interface Taper {
  /** Smallest and largest dB the slider can reach. */
  min: number;
  max: number;
  /** Position 0..1 → dB, clamped. */
  db(pos: number): number;
  /** dB → position 0..1, clamped. */
  pos(db: number): number;
}

const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

/** A taper whose two straight segments meet at (pos, db). */
function twoSegment(min: number, kneeDb: number, kneePos: number, max: number): Taper {
  const loPos = clamp(kneePos, 0, 1);
  const db = (p: number) => {
    const t = clamp(p, 0, 1);
    if (t <= loPos) return loPos === 0 ? min : min + ((kneeDb - min) * t) / loPos;
    return max === kneeDb ? kneeDb : kneeDb + ((max - kneeDb) * (t - loPos)) / (1 - loPos);
  };
  const pos = (d: number) => {
    const v = clamp(d, min, max);
    if (v <= kneeDb) return kneeDb === min ? 0 : (loPos * (v - min)) / (kneeDb - min);
    return max === kneeDb ? 1 : loPos + ((1 - loPos) * (v - kneeDb)) / (max - kneeDb);
  };
  return { min, max, db, pos };
}

/**
 * Volume: -40..0 dB takes about 80% of the travel, the rest reaches the
 * engine's minimum (-80 dB by default, but follow whatever it reports).
 */
export function volumeTaper(minDb: number, maxDb = 0, kneeDb = -40, kneePos = 0.2): Taper {
  const min = Math.min(minDb, maxDb);
  const max = Math.max(minDb, maxDb);
  const knee = clamp(kneeDb, min, max);
  return twoSegment(min, knee, kneePos, max);
}

/**
 * Balance: ±3 dB takes 70% of the travel, so the small corrections people
 * actually make are easy to hit, and the outer 15% each side reaches the
 * daemon's ±BALANCE_MAX_DB.
 */
export function balanceTaper(maxDb: number, innerDb = 3, innerFrac = 0.7): Taper {
  const max = Math.abs(maxDb);
  const inner = clamp(Math.abs(innerDb), 0, max);
  const frac = clamp(innerFrac, 0, 1);
  const pos = (d: number) => {
    const v = clamp(d, -max, max);
    const a = Math.abs(v);
    const half = frac / 2;
    const t = a <= inner ? (inner === 0 ? 0 : half * (a / inner)) : half + ((1 - frac) / 2) * ((a - inner) / (max - inner || 1));
    return 0.5 + Math.sign(v) * t;
  };
  const db = (p: number) => {
    const t = clamp(p, 0, 1) - 0.5;
    const a = Math.abs(t);
    const half = frac / 2;
    const v = a <= half ? (half === 0 ? 0 : inner * (a / half)) : inner + (max - inner) * ((a - half) / ((1 - frac) / 2 || 1));
    return Math.sign(t) * Math.min(max, v);
  };
  return { min: -max, max, db, pos };
}

/** Round a dB value to the slider's resolution and keep it in range. */
export function quantiseDb(db: number, taper: Taper, step = 0.1): number {
  const v = Math.round(db / step) * step;
  return clamp(Number(v.toFixed(4)), taper.min, taper.max);
}
