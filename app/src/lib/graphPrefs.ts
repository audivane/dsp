// View settings of the Harmonics page's plots and the EQ graph's frequency
// range: kept in the app store for the session (leaving a page and coming
// back keeps them) and per viewer in localStorage where it works. Not engine
// config: they only change what the plots show.

import type { ViewRange, XScale } from "./plotView";

export type HarmCompare = "stage" | "alone" | "after";
/** The distortion spectrum following the music: off, its level, or its level and tone. */
export type HarmFollowSpec = "off" | "level" | "both";

export interface HarmGraphPrefs {
  /** The spectrum's tone (Hz) and level (dBFS). */
  freq: number;
  level: number;
  compare: HarmCompare;
  scale: XScale;
  /** The spectrum's view; null: its home. */
  view: ViewRange | null;
  /** Nyquist when `view` was set (a view up to Nyquist follows a new rate). */
  nyq: number;
  /** The level sweep's tone (Hz) and the frequency sweep's level (dBFS). */
  sweepFreq: number;
  sweepLevel: number;
  /** The level plot's view; null: fitted to its curves. */
  levelView: ViewRange | null;
  /** Orders hidden from "Against level" and "Against frequency" (k = 2..15; 1 is THD). */
  hidden: number[];
  /** Following the music: the spectrum, the level sweep's tone, the frequency sweep's level. */
  followSpec: HarmFollowSpec;
  followLevel: boolean;
  followFreq: boolean;
}

export const DEFAULT_HARM_GRAPH: HarmGraphPrefs = {
  freq: 1000,
  level: -10,
  compare: "stage",
  scale: "log",
  view: null,
  nyq: 48000,
  sweepFreq: 1000,
  sweepLevel: -10,
  levelView: null,
  hidden: [],
  followSpec: "off",
  followLevel: false,
  followFreq: false,
};

/**
 * The frequency-range presets every frequency graph offers (the live
 * spectrum, the distortion spectrum, the PEQ graph), in this order.
 */
export type XPreset = "audio" | "50k" | "100k" | "nyquist";
export const X_PRESETS: readonly XPreset[] = ["audio", "50k", "100k", "nyquist"];
/** The level-range presets of the dB spectra: down to −160, −200, or fitted. */
export type YPreset = "160" | "200" | "fit";
export const Y_PRESETS: readonly YPreset[] = ["160", "200", "fit"];

/** A preset's frequency span: up to Nyquist is half the graph's rate. */
export function xPresetSpan(p: XPreset, nyq: number): { lo: number; hi: number } {
  if (p === "50k") return { lo: 20, hi: 50000 };
  if (p === "100k") return { lo: 10, hi: 100000 };
  if (p === "nyquist") return { lo: 20, hi: Math.max(1000, nyq) };
  return { lo: 20, hi: 20000 };
}

/** A graph's remembered range choice. */
export interface RangeChoice {
  x: XPreset;
  y: YPreset;
}

export function parseRangeChoice(raw: string | null, fallback: RangeChoice): RangeChoice {
  try {
    const p = raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
    return {
      x: X_PRESETS.includes(p.x as XPreset) ? (p.x as XPreset) : fallback.x,
      y: Y_PRESETS.includes(p.y as YPreset) ? (p.y as YPreset) : fallback.y,
    };
  } catch {
    return { ...fallback };
  }
}

export function loadRangeChoice(key: string, fallback: RangeChoice): RangeChoice {
  try {
    return parseRangeChoice(localStorage.getItem(key), fallback);
  } catch {
    return { ...fallback };
  }
}

export function saveRangeChoice(key: string, c: RangeChoice): void {
  try {
    localStorage.setItem(key, JSON.stringify(c));
  } catch {
    /* storage unavailable */
  }
}

/** The EQ graph's frequency range: one of the shared presets. */
export type EqRange = XPreset;

const HARM_KEY = "avdsp-harm-graph";
const EQ_RANGE_KEY = "avdsp-eq-range";

const num = (v: unknown, lo: number, hi: number, fallback: number) => (typeof v === "number" && Number.isFinite(v) && v >= lo && v <= hi ? v : fallback);

function viewOf(v: unknown): ViewRange | null {
  if (!v || typeof v !== "object") return null;
  const r = v as Record<string, unknown>;
  const ok = ["x0", "x1", "y0", "y1"].every((k) => typeof r[k] === "number" && Number.isFinite(r[k]));
  if (!ok) return null;
  const view = r as unknown as ViewRange;
  return view.x1 > view.x0 && view.y1 > view.y0 ? { x0: view.x0, x1: view.x1, y0: view.y0, y1: view.y1 } : null;
}

/** Stored prefs, each field checked (anything odd falls back to its default). */
export function parseHarmGraph(raw: string | null): HarmGraphPrefs {
  const d = DEFAULT_HARM_GRAPH;
  if (!raw) return { ...d };
  try {
    const p = JSON.parse(raw) as Record<string, unknown>;
    return {
      freq: num(p.freq, 20, 20000, d.freq),
      level: num(p.level, -60, 0, d.level),
      compare: p.compare === "alone" || p.compare === "after" ? p.compare : "stage",
      scale: p.scale === "lin" ? "lin" : "log",
      view: viewOf(p.view),
      nyq: num(p.nyq, 1000, 1e7, d.nyq),
      sweepFreq: num(p.sweepFreq, 20, 20000, d.sweepFreq),
      sweepLevel: num(p.sweepLevel, -40, 0, d.sweepLevel),
      levelView: viewOf(p.levelView),
      hidden: Array.isArray(p.hidden) ? p.hidden.filter((k): k is number => typeof k === "number" && k >= 1 && k <= 15) : [],
      followSpec: p.followSpec === "level" || p.followSpec === "both" ? p.followSpec : "off",
      followLevel: p.followLevel === true,
      followFreq: p.followFreq === true,
    };
  } catch {
    return { ...d };
  }
}

export function loadHarmGraph(): HarmGraphPrefs {
  try {
    return parseHarmGraph(localStorage.getItem(HARM_KEY));
  } catch {
    return { ...DEFAULT_HARM_GRAPH };
  }
}

export function saveHarmGraph(p: HarmGraphPrefs): void {
  try {
    localStorage.setItem(HARM_KEY, JSON.stringify(p));
  } catch {
    /* storage unavailable */
  }
}

const EQ_FALLBACK: RangeChoice = { x: "audio", y: "160" };

export function loadEqRange(): EqRange {
  return loadRangeChoice(EQ_RANGE_KEY, EQ_FALLBACK).x;
}

export function saveEqRange(r: EqRange): void {
  saveRangeChoice(EQ_RANGE_KEY, { ...EQ_FALLBACK, x: r });
}
