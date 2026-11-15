// Number formatting for the UI: real minus signs, tabular-friendly strings.

export function minus(s: string): string {
  return s.replace(/^-/, "−");
}

export function fmtDb(db: number, digits = 1, signed = true): string {
  if (!Number.isFinite(db)) return db < 0 ? "−∞" : "∞";
  const v = Math.abs(db) < 0.5 * Math.pow(10, -digits) ? 0 : db;
  const s = v.toFixed(digits);
  return (signed && v > 0 ? "+" : "") + minus(s);
}

export function fmtFreq(f: number): string {
  if (f >= 1000) {
    const k = f / 1000;
    return (k >= 10 ? k.toFixed(1) : k.toFixed(2)).replace(/\.?0+$/, "") + "k";
  }
  return String(Math.round(f));
}

export function fmtQ(q: number): string {
  return q.toFixed(2);
}

/** 44100 → "44.1k", 48000 → "48k". */
export function fmtRateShort(r: number): string {
  const k = r / 1000;
  return (Number.isInteger(k) ? String(k) : k.toFixed(1)) + "k";
}

export function fmtRate(r: number): string {
  const k = r / 1000;
  return (Number.isInteger(k) ? String(k) : k.toFixed(1)) + " kHz";
}

export function fmtTaps(n: number): string {
  if (n >= 1048576 && n % 1048576 === 0) return n / 1048576 + "M";
  if (n >= 1024 && n % 1024 === 0) return n / 1024 + "k";
  if (n >= 1000) return Math.round(n / 1000) + "k";
  return String(n);
}

export function fmtInt(n: number): string {
  return Math.round(n).toLocaleString("en-US");
}

export function fmtMs(ms: number): string {
  if (!Number.isFinite(ms)) return "—";
  return (ms < 10 ? ms.toFixed(1) : ms.toFixed(0)) + " ms";
}

const FORMAT_NAMES: Record<string, string> = { s16: "S16", s24: "S24", s24_3: "S24 (3-byte)", s32: "S32" };
export function fmtSampleFormat(f: string | null | undefined): string {
  if (!f) return "—";
  return FORMAT_NAMES[f] ?? f.toUpperCase();
}

/** Parse what a person types into a number field: "−3.5", "1.2k", "1,000". */
export function parseNumber(text: string): number | null {
  const t = text.trim().replace(/−/g, "-").replace(/,/g, "").replace(/\s+/g, "");
  const m = t.match(/^([+-]?\d*\.?\d+(?:e[+-]?\d+)?)(k)?(hz|db)?$/i);
  if (!m) return null;
  const v = parseFloat(m[1]) * (m[2] ? 1000 : 1);
  return Number.isFinite(v) ? v : null;
}

export function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}
