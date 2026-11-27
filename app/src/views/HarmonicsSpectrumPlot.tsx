// The Harmonics page's distortion spectrum: an FFT of a sine through the stage
// (engine `harmonics_spectrum`), fundamental at 0 dBr, every harmonic and alias
// where it lands, as measurement sites plot it. Zoomable like the Upsampling
// page's plots; optionally each layer alone or, in series, the chain after each
// layer, overlaid. Its state lives in `useSpectrumState` so the section can move
// between the Readouts panel and the slot on top.

import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { HarmonicsConfig, HarmonicsPreset, HarmonicsSpectrum, HarmonicsSpectrumLine, HarmonicsSpectrumPeak, HarmonicsSpectrumTrace } from "../api/types";
import { harmonicsSeries, layerName } from "../lib/effects";
import { fmtDb, fmtFreq, fmtRate, minus } from "../lib/format";
import { type HarmUnit, dbToPct, fmtPctNum, fmtPctValue, parsePctLimit, pctAxisPad, pctAxisTicks, pctTick } from "../lib/harmonicsPlot";
import { xPresetSpan, X_PRESETS, Y_PRESETS, type HarmCompare, type HarmFollowSpec, type XPreset, type YPreset } from "../lib/graphPrefs";
import type { FollowStatus } from "../lib/harmonicsFollow";
import { FollowButton, Followed } from "./HarmonicsFollow";
import { DEFAULT_MIN_SPAN_Y_DB, interpAt, sameView, type ViewLimits, type ViewRange, type XScale } from "../lib/plotView";
import { DITHER_TEXT } from "../lib/summary";
import { errorText, store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { RangePresets } from "../components/RangePresets";
import { ValueSlider } from "../components/ValueSlider";
import { cssVar } from "../theme";
import { BAND_COLORS } from "./eqColors";
import { ZoomPlot, type PlotGeom } from "./ZoomPlot";

type Compare = HarmCompare;

/** Compared lines' colours (the band palette without its blue, the whole stage's accent). */
const COMPARE_COLOURS = [0, 1, 2, 3, 5, 6, 7, 9].map((i) => BAND_COLORS[i]);

const fmtHz = (f: number) => (f >= 1000 ? `${fmtFreq(f).slice(0, -1)} kHz` : `${fmtFreq(f)} Hz`);
/** A snapped tone to a tenth of a hertz: "996.1 Hz", "10.002 kHz". */
export const fmtHzExact = (f: number) => (f >= 1000 ? `${(f / 1000).toFixed(3).replace(/0+$/, "").replace(/\.$/, "")} kHz` : `${f.toFixed(1)} Hz`);
const fmtLevel = (db: number, unit: HarmUnit) => (db <= -199 ? "none" : unit === "pct" ? fmtPctValue(db) : `${fmtDb(db, 1)} dB`);

const STATS_KEY = "avdsp-harm-spectrum-stats";

function loadFlag(key: string, fallback: boolean): boolean {
  try {
    const v = localStorage.getItem(key);
    return v === null ? fallback : v === "1";
  } catch {
    return fallback;
  }
}

function saveFlag(key: string, v: boolean): void {
  try {
    localStorage.setItem(key, v ? "1" : "0");
  } catch {
    /* storage unavailable */
  }
}

/** The traces a comparison asks for, with their labels (the whole stage last, drawn on top). */
function tracesFor(config: HarmonicsConfig, compare: Compare, presets: HarmonicsPreset[]): { trace: HarmonicsSpectrumTrace; label: string }[] {
  const active = config.layers.map((l, i) => ({ l, i })).filter(({ l }) => l.enabled);
  const stage = { trace: { kind: "stage" } as HarmonicsSpectrumTrace, label: "Whole stage" };
  if (compare === "alone" && active.length > 1) return [...active.map(({ l, i }) => ({ trace: { kind: "layer", layer: i } as HarmonicsSpectrumTrace, label: `${layerName(l, presets)} alone` })), stage];
  if (compare === "after" && active.length > 1 && harmonicsSeries(config))
    return [...active.slice(0, -1).map(({ l, i }) => ({ trace: { kind: "after", layer: i } as HarmonicsSpectrumTrace, label: `After ${layerName(l, presets)}` })), stage];
  return [stage];
}

/** The engine's spectrum of `config` for the tone, refreshed shortly after each change. */
function useSpectrum(config: HarmonicsConfig, freq: number, level: number, traces: HarmonicsSpectrumTrace[], skip: boolean) {
  const st = store();
  const [data, setData] = useState<HarmonicsSpectrum | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seq = useRef(0);
  const key = JSON.stringify([config, freq, level, traces]);
  useEffect(() => {
    if (skip) return;
    const my = ++seq.current;
    const t = setTimeout(() => {
      st.harmonicsSpectrum(config, freq, level, traces, () => my === seq.current)
        .then((r) => {
          if (my !== seq.current) return;
          setData(r);
          setError(null);
        })
        .catch((e) => {
          if (my === seq.current) setError(errorText(e));
        });
    }, 200);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, skip]);
  return { data, error };
}

/** The spectrum's x span for a preset (Nyquist: up to half the output rate; a linear axis starts at 0 Hz). */
export function xPresetRange(p: XPreset, scale: XScale, nyq: number): { x0: number; x1: number } {
  const { lo, hi } = xPresetSpan(p, nyq);
  return { x0: scale === "log" ? lo : 0, x1: hi };
}

/** The y span of a preset: +5 dBr down to −160, −200, or a little under the output word's floor (Fit). */
export function yPresetRange(p: YPreset, floorDb: number): { y0: number; y1: number } {
  const y0 = p === "160" ? -160 : p === "200" ? -200 : Math.max(-250, Math.floor((floorDb - 10) / 10) * 10);
  return { y0, y1: 5 };
}

/** The default view: +5 to −160 dBr over 20 Hz–20 kHz (0 Hz on a linear axis). */
export function spectrumHome(scale: XScale): ViewRange {
  return { ...xPresetRange("audio", scale, 48000), ...yPresetRange("160", 0) };
}

/** The view after a scale change: x starts at 0 Hz on a linear axis, at 10 or 20 Hz (or where it was) on a log one. */
export function viewForScale(v: ViewRange, scale: XScale): ViewRange {
  if (scale === "lin") return { ...v, x0: 0 };
  return { ...v, x0: v.x0 > 0 ? v.x0 : v.x1 >= 100000 ? 10 : 20 };
}

/** A view that reached the old Nyquist follows the new one (another output rate); any other stays. */
export function followNyquist(v: ViewRange, oldNyq: number, nyq: number): ViewRange {
  return Math.abs(v.x1 - oldNyq) < 1e-6 ? { ...v, x1: nyq } : v;
}

/**
 * The spectrum's state. Its settings (tone, level, lines, scale, view) live
 * in the app store, so they survive leaving the page and, per viewer,
 * restarts; only the data is fetched again.
 */
export function useSpectrumState(config: HarmonicsConfig, presets: HarmonicsPreset[], skip: boolean) {
  const st = store();
  const prefs = useApp((a) => a.harmGraph);
  const { freq, level, compare, scale } = prefs;
  const setFreq = (f: number) => st.setHarmGraph({ freq: f });
  const setLevel = (l: number) => st.setHarmGraph({ level: l });
  const setCompare = (c: Compare) => st.setHarmGraph({ compare: c });
  const [stats, setStatsState] = useState(() => loadFlag(STATS_KEY, true));
  const series = harmonicsSeries(config);
  const activeCount = config.layers.filter((l) => l.enabled).length;
  const effCompare: Compare = activeCount < 2 || (compare === "after" && !series) ? "stage" : compare;
  const named = useMemo(() => tracesFor(config, effCompare, presets), [config, effCompare, presets]);
  const traces = useMemo(() => named.map((n) => n.trace), [named]);
  const { data, error } = useSpectrum(config, freq, level, traces, skip);
  const nyq = data ? data.rate_hz / 2 : prefs.nyq;
  const stageLine = data?.lines[data.lines.length - 1];
  const floor = stageLine?.floor_db ?? -170;
  const home = useMemo(() => spectrumHome(scale), [scale]);
  const limits: ViewLimits = { x0: scale === "log" ? 10 : 0, x1: Math.max(nyq, 100000), y0: -260, y1: 20, minSpanX: scale === "log" ? 1.05 : 10, minSpanY: Math.max(1, DEFAULT_MIN_SPAN_Y_DB) };
  const view = prefs.view ?? home;
  const setView = (v: ViewRange) => st.setHarmGraph({ view: sameView(v, home) ? null : v, nyq });
  const setScale = (sc: XScale) => {
    const next = spectrumHome(sc);
    const v = prefs.view ? viewForScale(prefs.view, sc) : null;
    st.setHarmGraph({ scale: sc, view: v && !sameView(v, next) ? v : null });
  };
  // Another output rate: a view up to Nyquist follows it.
  useEffect(() => {
    const p = st.getState().harmGraph;
    if (!data || p.nyq === nyq) return;
    st.setHarmGraph({ nyq, view: p.view ? followNyquist(p.view, p.nyq, nyq) : null });
  }, [nyq, data, st]);
  const setStats = (v: boolean) => {
    setStatsState(v);
    saveFlag(STATS_KEY, v);
  };
  return { freq, setFreq, level, setLevel, effCompare, setCompare, scale, setScale, view, setView, home, limits, nyq, floor, data, error, named, stageLine, series, activeCount, stats, setStats };
}

export type SpectrumState = ReturnType<typeof useSpectrumState>;

const peakName = (p: HarmonicsSpectrumPeak) => (p.aliased ? `H${p.order} alias` : `H${p.order}`);

/** The level of order `k` in a line (−200: none). */
const orderDb = (line: HarmonicsSpectrumLine, k: number) => line.peaks.find((p) => p.order === k)?.level_db ?? -200;

/** The output word as a caption: "24-bit, TPDF dither, 3 dB headroom". */
const wordText = (w: HarmonicsSpectrum["word"]) => `${w.bits}-bit, ${DITHER_TEXT[w.dither] ?? w.dither}, ${fmtDb(w.headroom_db, 1, false)} dB headroom`;

type Line = { line: HarmonicsSpectrumLine; label: string; colour: string };

/** The analyser-style stats of the whole stage's line (and each compared line's THD), beside the plot or under it. */
function StatsInset({ data, lines, unit, beside }: { data: HarmonicsSpectrum; lines: Line[]; unit: HarmUnit; beside: boolean }) {
  const stage = lines[lines.length - 1]?.line;
  if (!stage) return null;
  const row = (label: string, v: string, title?: string) => (
    <div className="harm-stats-cell" title={title}>
      <span className="k">{label}</span>
      <span className="v">{v}</span>
    </div>
  );
  return (
    <div className={"harm-stats " + (beside ? "beside" : "under")} aria-label="Measurements of the whole stage">
      <div className="harm-stats-title">
        {fmtHz(data.freq_hz)} at {fmtDb(data.level_dbfs, 0, false)} dBFS · {fmtRate(data.rate_hz)}
      </div>
      <div className="harm-stats-grid">
        {row("THD", fmtLevel(stage.thd_db, unit), "Every harmonic below Nyquist (any order), relative to the fundamental.")}
        {row(
          "THD+N",
          fmtLevel(stage.thd_n_db, unit),
          `Everything but the fundamental in 20 Hz–20 kHz (harmonics and aliases) plus the output word's noise: ${wordText(data.word)} (volume at 0 dB; noise-shaped dither counted flat, an upper bound).`,
        )}
        {[2, 3, 4, 5, 6, 7, 8, 9].map((k) => (
          <Fragment key={k}>{row(`H${k}`, fmtLevel(orderDb(stage, k), unit))}</Fragment>
        ))}
        {row("Gain", `${fmtDb(stage.gain_db, 2)} dB`, "The fundamental out over in.")}
        {row("Floor", `${fmtDb(stage.floor_db, 0)} dBr`, "The output word's noise per FFT bin: the dashed line.")}
      </div>
      {lines.length > 1 && (
        <div className="harm-stats-lines">
          {lines.map(({ line, label, colour }) => (
            <div key={label}>
              <i className="harm-swatch" style={{ background: colour }} />
              {label}: THD {fmtLevel(line.thd_db, unit)}
            </div>
          ))}
        </div>
      )}
      <div className="harm-stats-word">Word: {wordText(data.word)}</div>
    </div>
  );
}

/** Below this section width the stats go under the plot instead of beside it. */
const STATS_BESIDE_MIN = 980;

/**
 * The distortion spectrum with its controls. `head` goes at the end of its
 * header row; `height` overrides the plot's; `resize` sits under the plot.
 */
export function SpectrumSection({
  s,
  unit,
  height,
  resize,
  head,
  follow,
}: {
  s: SpectrumState;
  unit: HarmUnit;
  height?: number;
  resize?: ReactNode;
  head?: ReactNode;
  /** Following the music: its level, or its level and centre. */
  follow: { mode: HarmFollowSpec; status: FollowStatus; set: (m: HarmFollowSpec) => void };
}) {
  const followLevel = follow.mode !== "off";
  const followTone = follow.mode === "both";
  const { data, error, named, stageLine, view, scale, nyq } = s;
  const wrap = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(0);
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setW(Math.round(el.getBoundingClientRect().width)));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const beside = w >= STATS_BESIDE_MIN;
  // Data mismatch (the lines array lags the traces while a call runs).
  const lines: Line[] = data
    ? data.lines.map((l, i) => {
        const last = i === data.lines.length - 1;
        const label = data.lines.length === named.length ? named[i].label : last ? "Whole stage" : `Line ${i + 1}`;
        return { line: l, label, colour: last ? "var(--accent)" : COMPARE_COLOURS[i % COMPARE_COLOURS.length] };
      })
    : [];
  const freqs = data?.freqs_hz ?? [];
  const fmtY = (db: number) => (unit === "pct" ? `${fmtPctValue(db)} (${fmtDb(db, 1)} dBr)` : `${fmtDb(db, 1)} dBr`);

  const drawCurves = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    const v = g.view;
    const bottom = g.top + g.h;
    for (const { line, colour } of lines) {
      ctx.strokeStyle = colour.startsWith("var(") ? cssVar(colour.slice(4, -1)) : colour;
      ctx.lineWidth = 1;
      ctx.globalAlpha = line === stageLine || lines.length === 1 ? 1 : 0.85;
      ctx.beginPath();
      if (!line.windowed) {
        // Exact: every order is one bin, drawn as a 1 px line up from the
        // bottom (the points between orders sit at the −250 dB floor).
        for (let i = 0; i < freqs.length; i++) {
          const f = freqs[i];
          if (f < v.x0 || f > v.x1 || line.levels_db[i] <= v.y0) continue;
          const x = Math.round(g.xOf(f)) + 0.5;
          ctx.moveTo(x, bottom);
          ctx.lineTo(x, Math.max(g.top - 2, g.yOf(line.levels_db[i])));
        }
        ctx.stroke();
        continue;
      }
      let started = false;
      for (let i = 0; i < freqs.length; i++) {
        const f = freqs[i];
        if (freqs[i + 1] !== undefined && freqs[i + 1] < v.x0) continue;
        if (freqs[i - 1] !== undefined && freqs[i - 1] > v.x1) break;
        const x = g.xOf(f);
        const y = Math.max(g.top - 50, Math.min(bottom + 50, g.yOf(line.levels_db[i])));
        if (!started) {
          ctx.moveTo(x, y);
          started = true;
        } else ctx.lineTo(x, y);
      }
      if (started) ctx.stroke();
    }
    ctx.globalAlpha = 1;
    // Compared lines share the peaks' frequencies: a dot on each line's
    // peaks shows where the levels differ (H2 adding or cancelling).
    if (lines.length > 1)
      for (const { line, colour } of lines) {
        ctx.fillStyle = colour.startsWith("var(") ? cssVar(colour.slice(4, -1)) : colour;
        for (const p of line.peaks) {
          if (p.freq_hz < v.x0 || p.freq_hz > v.x1 || p.level_db < v.y0 || p.level_db > v.y1) continue;
          ctx.beginPath();
          ctx.arc(g.xOf(p.freq_hz), g.yOf(p.level_db), line === stageLine ? 3.5 : 3, 0, Math.PI * 2);
          ctx.fill();
        }
      }
  };

  const decorate = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    // Above Nyquist: no data, shaded.
    if (nyq < g.view.x1) {
      const x = Math.max(g.left, g.xOf(nyq));
      ctx.save();
      ctx.fillStyle = cssVar("--muted");
      ctx.globalAlpha = 0.1;
      ctx.fillRect(x, g.top, g.left + g.w - x, g.h);
      ctx.globalAlpha = 0.6;
      ctx.strokeStyle = cssVar("--muted");
      ctx.beginPath();
      ctx.moveTo(Math.round(x) + 0.5, g.top);
      ctx.lineTo(Math.round(x) + 0.5, g.top + g.h);
      ctx.stroke();
      ctx.restore();
    }
    if (!stageLine) return;
    const y = Math.round(g.yOf(stageLine.floor_db)) + 0.5;
    ctx.save();
    ctx.strokeStyle = cssVar("--muted");
    ctx.setLineDash([5, 4]);
    ctx.beginPath();
    ctx.moveTo(g.left, y);
    ctx.lineTo(g.left + g.w, y);
    ctx.stroke();
    ctx.restore();
  };

  const annotate = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    ctx.save();
    ctx.font = "10.5px " + cssVar("--mono");
    ctx.fillStyle = cssVar("--muted");
    ctx.textAlign = "right";
    ctx.textBaseline = "middle";
    ctx.fillText(unit === "pct" ? "%" : "dBr", g.left - 6, g.top - 12);
    const nx = g.xOf(nyq);
    if (nyq < g.view.x1 && g.left + g.w - nx >= 90) {
      ctx.textAlign = "center";
      ctx.fillText("above Nyquist", (Math.max(g.left, nx) + g.left + g.w) / 2, g.top + 12);
      ctx.textAlign = "right";
    }
    if (!stageLine || !data) {
      ctx.restore();
      return;
    }
    const v = g.view;
    ctx.font = "600 10.5px " + cssVar("--mono");
    ctx.textAlign = "center";
    ctx.textBaseline = "bottom";
    // Peak labels, loudest first, skipping ones that would overlap.
    const placed: number[] = [];
    const peaks = [...stageLine.peaks].filter((p) => p.freq_hz >= v.x0 && p.freq_hz <= v.x1 && p.level_db >= v.y0 && p.level_db <= v.y1).sort((a, b) => b.level_db - a.level_db);
    const fx = g.xOf(data.freq_hz);
    if (fx >= g.left && fx <= g.left + g.w) placed.push(fx);
    for (const p of peaks) {
      if (placed.length >= 24) break;
      const x = g.xOf(p.freq_hz);
      if (placed.some((q) => Math.abs(q - x) < 18)) continue;
      placed.push(x);
      const y = Math.max(g.top + 12, g.yOf(p.level_db) - 3);
      ctx.fillStyle = cssVar(p.aliased ? "--warn" : "--fg");
      ctx.fillText(p.aliased ? `↶${p.order}` : `H${p.order}`, x, y);
    }
    ctx.restore();
  };

  const readout = (x: number, g: PlotGeom) => {
    // The nearest labelled peak (or the fundamental) within 8 px, else the curve.
    let best: { f: number; db: number; text: string; px: number } | null = null;
    for (const { line, label } of [...lines].reverse()) {
      const who = lines.length > 1 ? `${label}: ` : "";
      const cands = [{ order: 1, freq_hz: data?.freq_hz ?? 0, level_db: 0, aliased: false }, ...line.peaks];
      for (const p of cands) {
        const px = Math.abs(g.xOf(p.freq_hz) - g.xOf(x));
        if (px <= 8 && (!best || px < best.px - 0.5)) {
          const name = p.order === 1 ? "Fundamental" : peakName(p);
          const lvl = p.order === 1 ? "0 dBr" : fmtY(p.level_db);
          best = { f: p.freq_hz, db: p.level_db, px, text: `${who}${name}  ${fmtHz(p.freq_hz)}  ${lvl}` };
        }
      }
    }
    if (best) return { x: best.f, y: best.db, text: best.text };
    const db = stageLine ? interpAt(freqs, stageLine.levels_db, x) : null;
    return { x, y: db !== null && db <= -249 ? null : db, text: `${fmtHz(x)}  ${db === null ? "-" : db <= -249 ? "nothing" : fmtY(db)}` };
  };

  const yPreset = (p: YPreset): ViewRange => ({ ...view, ...yPresetRange(p, s.floor) });
  const xPreset = (p: XPreset): ViewRange => ({ ...view, ...xPresetRange(p, scale, nyq) });
  const yOn = Y_PRESETS.find((p) => sameView(yPreset(p), view));
  const xOn = X_PRESETS.find((p) => sameView(xPreset(p), view));
  const pct = unit === "pct";
  const yName = (db: number) => (pct ? `${pctTick(db)} %` : `${minus(String(db))} dBr`);
  const aliased = lines.some((l) => l.line.peaks.some((p) => p.aliased));
  const yLabel = (db: number) => (pct ? `${pctTick(db)} %` : minus(String(db)));
  const fitY = yPresetRange("fit", s.floor).y0;
  const padL = pct ? pctAxisPad(view.y0, 52) : 52;
  // The tone the analyser plays (snapped to its grid), when the data is for this slider position.
  const plays = data && Math.abs(data.freq_hz - s.freq) >= 0.05 && Math.abs(data.freq_hz - s.freq) <= data.bin_hz ? data.freq_hz : null;

  return (
    <div className="harm-spectrum" ref={wrap}>
      <div className="row harm-graph-head">
        <span className="k harm-graph-title" title="An FFT of a sine through the stage at the output rate, as a distortion analyser shows it: the fundamental at 0 dBr, every harmonic where it lands, and what folds back above Nyquist (aliases, marked ↶ with their order).">
          Distortion spectrum
        </span>
        <button type="button" className="btn small" aria-pressed={s.stats} onClick={() => s.setStats(!s.stats)} title="Show the measurements (THD, THD+N, H2–H9, gain, the output word) beside the plot, or under it when the plot is narrow">
          Stats
        </button>
        <span className="hint">{data ? `${data.bin_hz.toFixed(2)} Hz bins` : ""}</span>
        <div className="spacer" />
        {head}
      </div>
      <div className="harm-spectrum-controls">
        <div className="row">
          <label className="k" htmlFor="harm-spec-freq" title="The test tone's frequency (snapped to the analyser's grid).">
            Tone
          </label>
          <Followed on={followTone}>
            <ValueSlider
              id="harm-spec-freq"
              label="Tone frequency"
              value={s.freq}
              min={20}
              max={20000}
              step={1}
              log
              format={(f) => fmtHz(f)}
              onChange={(f) => {
                if (followTone) follow.set("level");
                s.setFreq(Math.round(f));
              }}
            />
          </Followed>
          {followLevel && (
            <label className="check" title="The tone follows the music's spectral centre too (music is not a sine there: it is the nearest sine to what is playing).">
              <input type="checkbox" checked={followTone} onChange={(e) => follow.set(e.target.checked ? "both" : "level")} />
              <span>Follow its centre</span>
            </label>
          )}
          {plays !== null && (
            <span className="hint mono" title={`The analyser plays the nearest frequency that fits a whole number of cycles in its FFT window (a ${data?.bin_hz.toFixed(2)} Hz grid), so every harmonic lands on one bin.`}>
              plays {fmtHzExact(plays)}
            </span>
          )}
        </div>
        <div className="row">
          <label className="k" htmlFor="harm-spec-level" title="The test tone's peak level.">
            Level
          </label>
          <Followed on={followLevel}>
            <ValueSlider
              id="harm-spec-level"
              label="Tone level"
              value={s.level}
              min={-60}
              max={0}
              step={1}
              format={(l) => `${fmtDb(l, 0, false)} dBFS`}
              onChange={(l) => {
                if (followLevel) follow.set("off");
                s.setLevel(l);
              }}
            />
          </Followed>
          <FollowButton on={followLevel} status={follow.status} what="its level (and, if you tick it, its spectral centre as the tone)" onClick={() => follow.set(followLevel ? "off" : "level")} />
        </div>
        <div className="row">
          <span className="k">Show</span>
          <Seg<Compare>
            label="Lines"
            value={s.effCompare}
            onChange={s.setCompare}
            options={[
              ["stage", "Stage", "The whole stage: what plays"],
              ["alone", "+ each layer", s.activeCount < 2 ? "Needs two active layers" : "Each active layer alone, fed the clean tone, under the whole stage"],
              ["after", "+ after each", !s.series ? "Series routing only" : s.activeCount < 2 ? "Needs two active layers" : "The chain after each layer, in order: how each layer adds to or cancels the ones before"],
            ]}
          />
        </div>
        <div className="row">
          <span className="k">Range</span>
          <RangePresets
            y={yOn ?? null}
            onY={(p) => s.setView(yPreset(p))}
            yLabel={(p) => (p === "fit" ? "Fit" : yLabel(-Number(p)))}
            yTitle={(p) => (p === "fit" ? `${yName(5)} down to just below the output word's floor (${yName(fitY)})` : `${yName(5)} down to ${yName(-Number(p))}`)}
            x={xOn ?? null}
            onX={(p) => s.setView(xPreset(p))}
            xTitle={{ nyquist: "Up to half the output rate: harmonics above 20 kHz, and where aliases come from" }}
          >
            <Seg<XScale>
              label="Frequency axis"
              value={scale}
              onChange={s.setScale}
              options={[
                ["log", "Log", "Logarithmic frequency axis"],
                ["lin", "Linear", "Linear frequency axis"],
              ]}
            />
          </RangePresets>
        </div>
      </div>
      {error && <div className="notice warn">Spectrum unavailable: {error}</div>}
      {data && (
        <>
          <div className={"harm-spectrum-body" + (s.stats && beside ? " beside" : "")}>
          <div className="harm-spectrum-main">
          <ZoomPlot
            ariaLabel={`Distortion spectrum of a ${fmtHz(data.freq_hz)} sine at ${fmtDb(data.level_dbfs, 0, false)} dBFS`}
            className="ups-canvas-wrap harm-spectrum-plot"
            style={height ? { height } : undefined}
            scale={scale}
            view={view}
            home={s.home}
            limits={s.limits}
            onView={s.setView}
            freqs={freqs}
            mags={stageLine?.levels_db ?? []}
            mode="box"
            boxAxis="x"
            drawCurve={drawCurves}
            decorate={decorate}
            annotate={annotate}
            readout={readout}
            yTicks={pct ? pctAxisTicks : undefined}
            yTick={pct ? (v) => pctTick(v) : (v, step) => minus(v.toFixed(step < 1 ? 1 : 0))}
            yLimitText={pct ? (v) => `${fmtPctNum(dbToPct(v))} %` : undefined}
            parseY={pct ? parsePctLimit : undefined}
            pad={{ l: padL, r: 14, t: 24, b: 26 }}
          />
          {resize}
          </div>
          {s.stats && <StatsInset data={data} lines={lines} unit={unit} beside={beside} />}
          </div>
          <div className="legend mp-legend">
            {lines.length > 1 &&
              lines.map(({ label, colour }) => (
                <span key={label}>
                  <i style={{ background: colour }} />
                  {label}
                </span>
              ))}
            <span title="The output word's noise per FFT bin.">
              <i className="dash harm-key-floor" />
              {data.word.bits}-bit {data.word.dither === "none" ? "undithered" : "dithered"} floor{stageLine ? `, ${fmtDb(stageLine.floor_db, 0)} dBr` : ""}
            </span>
            {aliased && (
              <span title="A harmonic above half the output rate folds back to another frequency: aliasing, not a harmonic of the tone.">
                <span className="harm-key-alias">↶k</span>
                order k folded back from above Nyquist
              </span>
            )}
            <span
              title={
                stageLine?.windowed
                  ? "The stage's steady state doesn't quite repeat over the analysis window (an envelope still moving), so the plot uses a 7-term Blackman-Harris window, as an analyser would: each peak is a few bins wide."
                  : "The stage's steady state repeats exactly over the analysis window, so the plot is the exact (unwindowed) spectrum: each harmonic is one bin, a hairline."
              }
            >
              {stageLine?.windowed ? "Blackman-Harris window" : "exact, no window"}
            </span>
          </div>
          <p className="harm-plot-help">drag to zoom, shift-drag for both axes, ctrl-drag to pan · over an axis the wheel zooms it and a drag pans it, click its end to type a limit · double-click to reset</p>
        </>
      )}
    </div>
  );
}
