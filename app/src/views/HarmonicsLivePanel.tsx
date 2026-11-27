// The Harmonics page's live analysis: what the playing side's stage does to
// the music actually playing (engine `harmonics_live`, polled a few times a
// second while the page is open): the numbers now (cards in the Analysis
// panel), the music's spectrum with the added content over it, and the
// added level's history with the sag.

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { HarmonicsLive, HarmonicsReadout } from "../api/types";
import { fmtDb, fmtFreq } from "../lib/format";
import { HISTORY_RANGES, type HistoryRange, type LiveBucket, bucketHistory, gmAt, holdSpectrum, liveFreqs, liveHistory, partHideKey, partLabel, partTitle } from "../lib/harmonicsLive";
import { sameView, type ViewLimits, type ViewRange } from "../lib/plotView";
import { X_PRESETS, Y_PRESETS, loadRangeChoice, saveRangeChoice, xPresetSpan, type RangeChoice, type XPreset, type YPreset } from "../lib/graphPrefs";
import { SPECTRUM_SMOOTHINGS, SpectrumAnimator, monotonePathD, smoothFractionalOctave, spectrumCurvePoints, traceMonotone, type SpectrumSmoothing } from "../lib/spectrum";
import { RangePresets } from "../components/RangePresets";
import { SmoothingSelect } from "./SpectrumLayer";
import { errorText, store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import type { HarmUnit } from "../lib/harmonicsPlot";
import { cssVar } from "../theme";
import { bandColor } from "./eqColors";
import { ZoomPlot, type PlotGeom } from "./ZoomPlot";

/** How often the panel asks the engine for a frame, ms. */
const POLL_MS = 250;
/** The history chart's narrowest bucket. */
const MIN_BUCKET_MS = 2 * POLL_MS;

/**
 * The latest live frame, polled while `enabled` and the window is visible,
 * each new frame added to the session's history. `version` moves with the
 * history.
 */
export function useHarmonicsLive(enabled: boolean): { frame: HarmonicsLive | null; error: string | null; version: number } {
  const st = store();
  const [frame, setFrame] = useState<HarmonicsLive | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [version, setVersion] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    let busy = false;
    const tick = () => {
      if (busy || document.hidden) return;
      busy = true;
      st.harmonicsLive()
        .then((f) => {
          if (!alive) return;
          setFrame(f);
          setError(null);
          // Hidden (a blind test): nothing from before it stays either.
          if (f.state === "hidden") {
            liveHistory.clear();
            setVersion((v) => v + 1);
          } else if (liveHistory.push(f, Date.now())) setVersion((v) => v + 1);
        })
        .catch((e) => alive && setError(errorText(e)))
        .finally(() => {
          busy = false;
        });
    };
    tick();
    const id = setInterval(tick, POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [enabled, st]);
  return { frame, error, version };
}

/** Width of an element, followed as it resizes. */
function useWidth(): [React.RefObject<HTMLDivElement | null>, number] {
  const wrap = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(480);
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setW(Math.max(240, Math.round(el.getBoundingClientRect().width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [wrap, w];
}

function loadPref<T extends string>(key: string, allowed: readonly T[], fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return (allowed as readonly string[]).includes(v ?? "") ? (v as T) : fallback;
  } catch {
    return fallback;
  }
}

function savePref(key: string, v: string): void {
  try {
    localStorage.setItem(key, v);
  } catch {
    /* storage unavailable */
  }
}

const sgn = (v: number, digits: number) => fmtDb(v, digits);
const fmtPct = (p: number) => (p >= 10 ? p.toFixed(0) : p >= 1 ? p.toFixed(1) : p >= 0.1 ? p.toFixed(2) : p.toFixed(3));
const fmtHz = (f: number) => (f >= 1000 ? `${fmtFreq(f).slice(0, -1)} kHz` : `${fmtFreq(f)} Hz`);

const STATE_TEXT: Record<Exclude<HarmonicsLive["state"], "playing">, string> = {
  waiting: "Starting the analysis…",
  idle: "Nothing is playing through the DSP chain.",
  off: "The harmonics stage is off: turn it on to see what it adds to the music.",
  silent: "Silence: nothing to add to.",
  hidden: "Hidden during the blind test: a live readout would give the side away.",
};

/** The shared hide set's keys of the history's own lines. */
const KEY_TOTAL = 400;
const KEY_MUSIC = 200;
const KEY_SAG = 300;

export type LiveFeed = { frame: HarmonicsLive | null; error: string | null; version: number };

/** The frame to show: during a blind test nothing live, whatever the last frame said. */
export const shownFrame = (live: LiveFeed, blind: boolean): HarmonicsLive | null => (blind ? (live.frame ? { ...live.frame, state: "hidden" } : null) : live.frame);

/** A stat card: its name, value and a line under it. */
export function StatCard({ k, v, sub, title, accent = false }: { k: string; v: ReactNode; sub?: ReactNode; title?: string; accent?: boolean }) {
  return (
    <div className={"fact harm-stat" + (accent ? " accent" : "")} title={title}>
      <div className="k">{k}</div>
      <div className="v">{v}</div>
      {sub ? <div className="sub">{sub}</div> : null}
    </div>
  );
}

/**
 * The cards of the music playing now (what the stage adds, the level
 * change and sag, the music's level), or what stops them (nothing playing,
 * the stage off, a blind test).
 */
export function LiveStats({ live, blind = false, unit, readout }: { live: LiveFeed; blind?: boolean; unit: HarmUnit; readout?: HarmonicsReadout | null }) {
  const frame = shownFrame(live, blind);
  if (blind && !frame) return <p className="stage-note harm-live-state">{STATE_TEXT.hidden}</p>;
  if (!frame) return live.error ? null : <p className="stage-note harm-live-state">{STATE_TEXT.waiting}</p>;
  if (frame.state !== "playing") return <p className="stage-note harm-live-state">{STATE_TEXT[frame.state as keyof typeof STATE_TEXT]}</p>;
  const f = frame;
  const sag = f.layers.reduce((m, l) => Math.max(m, l.sag_db), 0);
  const biased = f.layers.filter((l) => l.bias !== 0);
  const db = `${sgn(f.added_db, 0)} dB`;
  // Gm at the music's peak, from the "Against level" sweep (under load when
  // the stage has sag or bias, the state the music keeps it in).
  const sweep = readout?.under_load?.length ? readout.under_load : (readout?.sweep ?? []);
  const gm = gmAt(sweep, f.music_peak_dbfs);
  const gmTone = sweep[0]?.freq_hz;
  const pct = `${fmtPct(f.added_pct)} %`;
  return (
    <div className="harm-stat-cards" aria-live="off">
      <StatCard
        k="Adding"
        accent
        v={unit === "pct" ? pct : db}
        sub={unit === "pct" ? db : pct}
        title={`The added content: the stage's output minus its linear part (the input at the stage's gain, fitted over about the last second), relative to the music coming out. Loudest part of the window: ${sgn(f.added_peak_db, 1)} dB.`}
      />
      <StatCard
        k="Level change"
        v={`${sgn(f.level_change_db, 2)} dB`}
        sub="against quiet signals"
        title="How much the stage turns the music's level down or up now, against its gain for quiet signals: sag and the curve's own compression. Counted apart from the added content."
      />
      {f.layers.length > 0 && (
        <StatCard
          k="Sag"
          v={sag > 0.005 ? `${sgn(-sag, 2)} dB` : "none"}
          sub={biased.length ? biased.map((l) => `bias L${l.layer + 1} ${(100 * l.bias).toFixed(1)} %`).join(" · ") : "the deepest layer"}
          title="How far the sag turns the drive down now (the deepest layer). Bias: each layer's operating point now, as a fraction of full scale."
        />
      )}
      {gm !== null && gmTone !== undefined && (
        <StatCard
          k="Gm"
          v={gm.toFixed(3)}
          sub={`a ${fmtHz(gmTone)} sine at the music's peak`}
          title={`GedLee Gm of the stage's curve over the swing the music uses now: read off the "Against level" sweep (a ${fmtHz(gmTone)} sine) at the music's peak, ${sgn(f.music_peak_dbfs, 1)} dBFS${readout?.under_load?.length ? ", under load" : ""}. Gm weighs the curve by where a signal spends its time, so it reads the curve, not the music: the test sine's tone sets which layers' frequency shaping counts. Lower is cleaner.`}
        />
      )}
      <StatCard
        k="Music"
        v={`${sgn(f.music_level_dbfs, 1)} dBFS`}
        sub={`peak ${sgn(f.music_peak_dbfs, 1)} · centre ${fmtHz(f.centroid_hz)}`}
        title="The music's level now: RMS + 3 dB (the sine level the sweeps use), its sample peak, and where its energy is centred (power-weighted, 20 Hz–20 kHz)."
      />
    </div>
  );
}

type SpecUnit = "dbfs" | "dbr";
const SPEC_UNIT_KEY = "avdsp-harm-live-unit";
const SPEC_SMOOTH_KEY = "avdsp-harm-live-smoothing";
const SPEC_RANGE_KEY = "avdsp-harm-live-presets";
/** How long the spectrum keeps the last frame once the music stops (then it falls away). */
const PAUSE_KEEP_MS = 1500;
const LIMITS: ViewLimits = { x0: 5, x1: 1_600_000, y0: -250, y1: 60, minSpanX: 1.05, minSpanY: 1 };
/** The animation's floor and ceiling (dB in the unit shown). */
const ANIM_FLOOR = -250;
const ANIM_CEIL = 60;
/** The top of the level presets: a little over a full-scale band, or over the music's level. */
const yTop = (unit: SpecUnit) => (unit === "dbr" ? 10 : 5);

/** The live spectrum's level span for a preset (Fit: down to just under the quietest trace, `fit`). */
export function liveYRange(p: YPreset, unit: SpecUnit, fit: number | null): { y0: number; y1: number } {
  const y1 = yTop(unit);
  if (p === "fit") return { y0: fit ?? (unit === "dbr" ? -160 : -180), y1 };
  return { y0: -Number(p), y1 };
}

/** Fit: 10 dB under the quietest level of the traces between `lo` and `hi` Hz, in 10 dB steps. */
export function liveFit(traces: readonly (readonly number[])[], freqs: readonly number[], lo: number, hi: number): number | null {
  let min = Infinity;
  for (const t of traces)
    for (let i = 0; i < freqs.length; i++) if (freqs[i] >= lo && freqs[i] <= hi && Number.isFinite(t[i])) min = Math.min(min, t[i]);
  return Number.isFinite(min) ? Math.max(LIMITS.y0, Math.floor(min / 10) * 10 - 10) : null;
}

type HistMode = "total" | "parts";
const RANGE_KEY = "avdsp-harm-live-range";
const MODE_KEY = "avdsp-harm-live-mode";

/**
 * The live graphs' state, held by the page so a graph keeps it (its hold,
 * zoom, animation, last frame, sticky level axis) when it moves between the
 * column and the slot on top.
 */
export function useLiveGraphs() {
  const [unit, setUnitState] = useState<SpecUnit>(() => loadPref(SPEC_UNIT_KEY, ["dbfs", "dbr"], "dbfs"));
  const [view, setView] = useState<ViewRange | null>(null);
  const [smoothing, setSmoothingState] = useState<SpectrumSmoothing>(() => Number(loadPref(SPEC_SMOOTH_KEY, SPECTRUM_SMOOTHINGS.map(String), "12")) as SpectrumSmoothing);
  const setSmoothing = (v: SpectrumSmoothing) => {
    setSmoothingState(v);
    savePref(SPEC_SMOOTH_KEY, String(v));
  };
  // The range presets (remembered); a zoom is `view`, for the session.
  const [presets, setPresetsState] = useState<RangeChoice>(() => loadRangeChoice(SPEC_RANGE_KEY, { x: "audio", y: "160" }));
  const setPresets = (c: RangeChoice) => {
    setPresetsState(c);
    saveRangeChoice(SPEC_RANGE_KEY, c);
  };
  /** Fit's floor, taken from the traces when Fit is picked (or first drawn), kept until picked again. */
  const fit = useRef<number | null>(null);
  const anim = useRef({ animator: new SpectrumAnimator(), targets: null as unknown, at: 0, cleared: null as unknown, running: false });
  const setUnit = (u: SpecUnit) => {
    setUnitState(u);
    savePref(SPEC_UNIT_KEY, u);
    setView(null);
    fit.current = null;
    anim.current.animator.reset();
  };
  const [hold, setHold] = useState(false);
  const held = useRef<{ music: number[] | null; added: number[] | null }>({ music: null, added: null });
  // The last frame drawn, kept through a short gap (a quiet moment, a
  // track change) so the plot doesn't flicker; once nothing has played
  // for PAUSE_KEEP_MS the traces fall away (a peak hold stays). The axes stay.
  const last = useRef<HarmonicsLive | null>(null);
  const lastAt = useRef(0);
  const [range, setRangeState] = useState<HistoryRange>(() => Number(loadPref(RANGE_KEY, HISTORY_RANGES.map(String), "30000")) as HistoryRange);
  const setRange = (r: HistoryRange) => {
    setRangeState(r);
    savePref(RANGE_KEY, String(r));
  };
  const [mode, setModeState] = useState<HistMode>(() => loadPref(MODE_KEY, ["total", "parts"], "total"));
  const setMode = (m: HistMode) => {
    setModeState(m);
    savePref(MODE_KEY, m);
  };
  const axis = useRef<{ key: string; y0: number; y1: number } | null>(null);
  return { unit, setUnit, view, setView, smoothing, setSmoothing, presets, setPresets, fit, anim, hold, setHold, held, last, lastAt, range, setRange, mode, setMode, axis };
}

export type LiveGraphs = ReturnType<typeof useLiveGraphs>;

const now = () => (typeof performance !== "undefined" ? performance.now() : Date.now());

/** The music's live spectrum with the added content over it. `head` goes at the end of its header row; `height` overrides the plot's. */
export function LiveSpectrum({ frame, g: s, height, resize, head }: { frame: HarmonicsLive | null; g: LiveGraphs; height?: number; resize?: ReactNode; head?: ReactNode }) {
  const { unit, setUnit, hold, setHold, held, last, lastAt, view, setView, smoothing, setSmoothing, presets, setPresets, fit, anim } = s;
  if (frame) {
    last.current = frame;
    lastAt.current = Date.now();
  }
  const f = last.current;
  const cleared = !frame && !hold && Date.now() - lastAt.current > PAUSE_KEEP_MS;
  const shift = unit === "dbr" && f ? -f.music_level_dbfs : 0;
  // Hold: per bin the largest level seen since it was turned on.
  const music = useMemo(() => {
    if (!frame) return held.current.music ?? f?.music ?? [];
    held.current.music = hold ? holdSpectrum(held.current.music, frame.music) : null;
    return hold ? held.current.music! : frame.music;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [frame, hold]);
  const added = useMemo(() => {
    if (!frame) return held.current.added ?? f?.added ?? [];
    held.current.added = hold ? holdSpectrum(held.current.added, frame.added) : null;
    return hold ? held.current.added! : frame.added;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [frame, hold]);
  const bpo = f?.bins_per_octave ?? 24;
  // What the traces move toward: smoothed, in the unit shown.
  const targets = useMemo(() => {
    const sm = (v: readonly number[]) => smoothFractionalOctave(v, bpo, smoothing, ANIM_FLOOR).map((d) => d + shift);
    return { music: sm(music), added: sm(added) };
  }, [music, added, smoothing, bpo, shift]);
  const a = anim.current;
  if (a.targets !== targets) {
    a.targets = targets;
    a.at = now();
  }
  const freqs = useMemo(() => (f ? liveFreqs(f, f.music.length) : []), [f]);
  const nyq = f ? f.rate / 2 : 384000;
  const xSpan = xPresetSpan(presets.x, nyq);
  if (presets.y === "fit" && fit.current === null && freqs.length) fit.current = liveFit([targets.music, targets.added], freqs, xSpan.lo, xSpan.hi);
  const home: ViewRange = { x0: xSpan.lo, x1: xSpan.hi, ...liveYRange(presets.y, unit, fit.current) };
  const shown = view ?? home;
  const pickX = (x: XPreset) => {
    const sp = xPresetSpan(x, nyq);
    setPresets({ ...presets, x });
    const next = { ...home, x0: sp.lo, x1: sp.hi };
    setView(view ? (sameView({ ...view, x0: sp.lo, x1: sp.hi }, next) ? null : { ...view, x0: sp.lo, x1: sp.hi }) : null);
  };
  const pickY = (y: YPreset) => {
    if (y === "fit") fit.current = liveFit([targets.music, targets.added], freqs, shown.x0, shown.x1);
    const r = liveYRange(y, unit, fit.current);
    setPresets({ ...presets, y });
    const next = { ...home, ...r };
    setView(view ? (sameView({ ...view, ...r }, next) ? null : { ...view, ...r }) : null);
  };
  // The preset that matches the view is lit (the remembered one first, when two give the same span).
  const xMatch = (p: XPreset) => {
    const sp = xPresetSpan(p, nyq);
    return sameView({ ...shown, x0: sp.lo, x1: sp.hi }, shown);
  };
  const yMatch = (p: YPreset) => sameView({ ...shown, ...liveYRange(p, unit, fit.current) }, shown);
  const xOn = xMatch(presets.x) ? presets.x : X_PRESETS.find(xMatch);
  const yOn = yMatch(presets.y) ? presets.y : Y_PRESETS.find((p) => p !== "fit" && yMatch(p));
  if (!f || freqs.length === 0) return null;
  const draw = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    a.running = false;
    if (a.cleared === targets || a.targets !== targets) return;
    const t = now();
    // Hold keeps its traces: they never go stale, and once no new frame
    // has come for a while (settled) they stay as drawn.
    const settled = hold && a.animator.started && t - a.at > PAUSE_KEEP_MS;
    if (!settled) {
      const step = a.animator.step({ frameKey: targets, frameAtMs: hold ? t : a.at, nowMs: t, tap: "both", peakHold: false, preTarget: targets.music, postTarget: targets.added, floorDb: ANIM_FLOOR, ceilDb: ANIM_CEIL, staleMs: PAUSE_KEEP_MS });
      if (step.clearFrame) {
        a.animator.reset();
        a.cleared = targets;
        return;
      }
      a.running = step.animating;
    }
    const bottom = g.top + g.h;
    const yOf = (db: number) => Math.max(g.top - 2000, Math.min(bottom + 2000, g.yOf(db)));
    const pts = (bins: Float64Array) => spectrumCurvePoints(bins, f.f_min_hz, f.bins_per_octave, g.xOf, yOf, g.left + g.w, nyq);
    // The music, grey and filled faintly; the added content in the accent.
    const m = pts(a.animator.pre);
    if (m.xs.length >= 2) {
      ctx.save();
      ctx.fillStyle = cssVar("--muted");
      ctx.globalAlpha = 0.12;
      ctx.beginPath();
      traceMonotone(ctx, m.xs, m.ys);
      ctx.lineTo(m.xs[m.xs.length - 1], bottom);
      ctx.lineTo(m.xs[0], bottom);
      ctx.closePath();
      ctx.fill();
      ctx.globalAlpha = 0.9;
      ctx.strokeStyle = cssVar("--muted");
      ctx.lineWidth = 1.25;
      ctx.beginPath();
      traceMonotone(ctx, m.xs, m.ys);
      ctx.stroke();
      ctx.restore();
    }
    const ad = pts(a.animator.post);
    if (ad.xs.length >= 2) {
      ctx.strokeStyle = cssVar("--accent");
      ctx.lineWidth = 1.75;
      ctx.beginPath();
      traceMonotone(ctx, ad.xs, ad.ys);
      ctx.stroke();
    }
  };
  const decorate = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    if (nyq < g.view.x1) {
      const x = Math.max(g.left, g.xOf(nyq));
      ctx.save();
      ctx.fillStyle = cssVar("--muted");
      ctx.globalAlpha = 0.1;
      ctx.fillRect(x, g.top, g.left + g.w - x, g.h);
      ctx.restore();
    }
  };
  const annotate = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    ctx.save();
    ctx.font = "10.5px " + cssVar("--mono");
    ctx.fillStyle = cssVar("--muted");
    ctx.textAlign = "right";
    ctx.textBaseline = "middle";
    ctx.fillText(unit === "dbr" ? "dBr" : "dBFS", g.left - 6, g.top - 12);
    ctx.restore();
  };
  const nearest = (x: number) => {
    let i = 0;
    for (let j = 1; j < freqs.length; j++) if (Math.abs(Math.log(freqs[j] / x)) < Math.abs(Math.log(freqs[i] / x))) i = j;
    return i;
  };
  const readout = (x: number) => {
    if (cleared) return { x, y: null, text: "Nothing playing" };
    const i = nearest(x);
    const u = unit === "dbr" ? "dBr" : "dBFS";
    const mu = targets.music[i];
    const ad = targets.added[i];
    const d = ad - mu;
    return { x: freqs[i], y: ad, text: `${fmtHz(freqs[i])}  music ${sgn(mu, 1)} ${u}  added ${sgn(ad, 1)} ${u}  (${Math.abs(d).toFixed(1)} dB ${d > 0 ? "over" : "under"} the music)` };
  };
  const yName = (db: number) => `${sgn(db, 0)} ${unit === "dbr" ? "dBr" : "dBFS"}`;
  return (
    <div className="harm-live-spectrum">
      <div className="row harm-graph-head">
        <span className="k harm-graph-title" title="Band power of the music going into the stage (grey) and of what the stage adds to it (blue), on 1/24-octave bins, smoothed to the fraction of an octave chosen: 0 dBFS is a full-scale sine. dBr puts the music's level at 0.">
          Live spectrum
        </span>
        <Seg<SpecUnit>
          label="Spectrum level unit"
          value={unit}
          onChange={setUnit}
          options={[
            ["dbfs", "dBFS", "Levels against full scale"],
            ["dbr", "dBr", "Levels against the music's current level (0 dBr)"],
          ]}
        />
        <SmoothingSelect value={smoothing} onChange={setSmoothing} finest={f.bins_per_octave} />
        <button
          type="button"
          className="btn small"
          aria-pressed={hold}
          title="Peak hold: keep each bin's loudest level since it was turned on"
          onClick={() => {
            held.current = { music: null, added: null };
            setHold(!hold);
          }}
        >
          Hold
        </button>
        <div className="spacer" />
        {head}
      </div>
      <div className="row harm-graph-controls">
        <span className="k">Range</span>
        <RangePresets
          x={xOn ?? null}
          onX={pickX}
          y={yOn ?? null}
          onY={pickY}
          yTitle={(p) => (p === "fit" ? `${yName(yTop(unit))} down to just under the quietest trace now (picking it again fits again)` : `${yName(yTop(unit))} down to ${yName(-Number(p))}`)}
          xTitle={{ nyquist: "Up to half the output rate: what the stage adds above the audio band" }}
        />
      </div>
      <ZoomPlot
        ariaLabel="Spectrum of the music playing and of what the harmonics stage adds to it"
        className="ups-canvas-wrap harm-live-plot"
        style={height ? { height } : undefined}
        scale="log"
        view={shown}
        home={home}
        limits={LIMITS}
        onView={(v) => setView(sameView(v, home) ? null : v)}
        freqs={freqs}
        mags={targets.added}
        mode="box"
        boxAxis="x"
        drawCurve={draw}
        decorate={decorate}
        annotate={annotate}
        readout={readout}
        animating={() => a.running}
        pad={{ l: 46, r: 14, t: 22, b: 24 }}
      />
      {resize}
      <div className="legend mp-legend">
        <span>
          <i style={{ background: "var(--muted)" }} />
          music (into the stage)
        </span>
        <span>
          <i style={{ background: "var(--accent)" }} />
          added by the stage
        </span>
        <span className="hint">{hold ? "peak hold" : cleared ? "nothing playing" : `${Math.round(f.window_ms)} ms window, every ${Math.round(f.interval_ms)} ms`}</span>
      </div>
    </div>
  );
}

const RANGE_TEXT: Record<HistoryRange, string> = { 30000: "30 s", 120000: "2 min", 600000: "10 min" };

const MAIN_H = 150;
const SAG_H = 46;
const MUSIC_H = 46;
const GAP = 10;
const AXIS_H = 18;
const PAD_L = 46;
const PAD_R = 14;
const PAD_T = 8;

/** Time-axis ticks for a span: every 5 s, 20 s or a minute. */
function timeTicks(t0: number, t1: number): number[] {
  const span = t1 - t0;
  const step = span <= 30000 ? 5000 : span <= 120000 ? 20000 : 60000;
  const out: number[] = [];
  for (let t = Math.ceil(t0 / step) * step; t <= t1; t += step) out.push(t);
  return out;
}

const clock = (t: number) => new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });

/** The buckets' values as runs of points at their centres, broken at gaps. */
function runs(buckets: readonly LiveBucket[], value: (b: LiveBucket) => number | null, x: (t: number) => number, y: (v: number) => number): { xs: number[]; ys: number[]; b: LiveBucket[] }[] {
  const out: { xs: number[]; ys: number[]; b: LiveBucket[] }[] = [];
  let run: { xs: number[]; ys: number[]; b: LiveBucket[] } | null = null;
  for (const b of buckets) {
    const v = value(b);
    if (v === null) {
      run = null;
      continue;
    }
    if (!run) out.push((run = { xs: [], ys: [], b: [] }));
    run.xs.push(x((b.t0 + b.t1) / 2));
    run.ys.push(y(v));
    run.b.push(b);
  }
  return out;
}

/** A smooth line through the buckets' values (a monotone cubic through their centres), broken at gaps. */
function path(buckets: readonly LiveBucket[], value: (b: LiveBucket) => number | null, x: (t: number) => number, y: (v: number) => number): string {
  return runs(buckets, value, x, y)
    .map((r) => monotonePathD(r.xs, r.ys))
    .join("");
}

/**
 * The area from `bottom` up to a smooth envelope through the buckets'
 * values (the peak band, the strips' fills), broken at gaps. A lone bucket
 * is its own column.
 */
function area(buckets: readonly LiveBucket[], value: (b: LiveBucket) => number | null, x: (t: number) => number, y: (v: number) => number, bottom: number): string {
  const f = (v: number) => v.toFixed(1);
  return runs(buckets, value, x, y)
    .map((r) => {
      if (r.xs.length === 1) {
        const b = r.b[0];
        return `M${f(x(b.t0))},${bottom}L${f(x(b.t0))},${f(r.ys[0])}L${f(x(b.t1))},${f(r.ys[0])}L${f(x(b.t1))},${bottom}Z`;
      }
      return `M${f(r.xs[0])},${bottom}${monotonePathD(r.xs, r.ys, true)}L${f(r.xs[r.xs.length - 1])},${bottom}Z`;
    })
    .join("");
}

function useHidden(): [Set<number>, (k: number) => void] {
  const hidden = useApp((a) => a.harmGraph.hidden);
  const set = useMemo(() => new Set(hidden), [hidden]);
  const toggle = (k: number) => store().setHarmGraph({ hidden: set.has(k) ? hidden.filter((h) => h !== k) : [...hidden, k] });
  return [set, toggle];
}

/**
 * The added level over time, with the music's level and the sag in their own
 * strips under it (one time axis). `height` overrides the whole chart's;
 * `onTop` (shown in the slot away from the live cards) repeats the added
 * level, sag and music level in the legend, which the numbers above already
 * give in the column.
 */
export function LiveHistoryChart({ frame, version, g, height, resize, head, onTop = false }: { frame: HarmonicsLive | null; version: number; g: LiveGraphs; height?: number; resize?: ReactNode; head?: ReactNode; onTop?: boolean }) {
  const [wrap, w] = useWidth();
  const { range, setRange, mode, setMode } = g;
  const [hover, setHover] = useState<number | null>(null);
  const [hidden, toggle] = useHidden();
  const axisRef = g.axis;
  const split = frame?.split ?? "total";
  const canSplit = split !== "total";
  const byParts = mode === "parts" && canSplit;
  // The newest frame's time: the chart's right edge moves with the data.
  const now = Date.now();
  const plotW = Math.max(60, w - PAD_L - PAD_R);
  // At least two polls per bucket: narrower buckets mostly hold no frame,
  // which broke the lines into dots and left the hover readout blinking.
  const n = Math.max(10, Math.min(Math.floor(plotW / 3), 400, Math.floor(range / MIN_BUCKET_MS)));
  // Bucket edges on a fixed grid (multiples of the bucket width), so a point
  // stays in its bucket as the chart scrolls and the past doesn't reshuffle.
  const buckets = useMemo(
    () => {
      const w = range / n;
      return bucketHistory(liveHistory.points, Math.ceil(now / w) * w, range, n);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [version, range, n],
  );
  const partKeys = useMemo(() => {
    const keys = new Set<string>();
    for (const b of buckets) for (const k of Object.keys(b.parts)) keys.add(k);
    for (const p of frame?.parts ?? []) keys.add(p.key);
    return [...keys].sort((a, b) => partHideKey(a) - partHideKey(b));
  }, [buckets, frame]);
  if (liveHistory.points.length === 0 && !frame) return <div className="harm-live-history" ref={wrap} />;
  const t1 = buckets.length ? buckets[buckets.length - 1].t1 : now;
  const t0 = t1 - range;
  const x = (t: number) => PAD_L + ((t - t0) / range) * plotW;
  // The level axis: from the quietest line shown to the loudest peak.
  let lo = Infinity;
  let hi = -Infinity;
  for (const b of buckets) {
    if (b.added === null) continue;
    if (!hidden.has(KEY_TOTAL) || !byParts) {
      lo = Math.min(lo, b.added);
      hi = Math.max(hi, b.peak ?? b.added);
    }
    if (byParts)
      for (const [k, v] of Object.entries(b.parts))
        if (!hidden.has(partHideKey(k)) && v > -150) {
          lo = Math.min(lo, v);
          hi = Math.max(hi, v);
        }
  }
  // In 20 dB steps and sticky, so it doesn't jump as the window scrolls:
  // it widens at once to fit a new level, narrows only once an end has
  // 40 dB unused, keeps its range through silence, and starts afresh when
  // the lines shown or the time range change.
  const axisKey = `${range}|${byParts}|${[...hidden].sort((a, b) => a - b).join(",")}`;
  if (Number.isFinite(lo)) {
    const t0 = Math.max(-160, Math.floor((lo - 5) / 20) * 20);
    const t1 = Math.min(20, Math.ceil((hi + 3) / 20) * 20);
    const a = axisRef.current;
    if (!a || a.key !== axisKey) axisRef.current = { key: axisKey, y0: t0, y1: t1 };
    else {
      if (t0 < a.y0 || t0 - a.y0 >= 40) a.y0 = t0;
      if (t1 > a.y1 || a.y1 - t1 >= 40) a.y1 = t1;
    }
  }
  const y0 = axisRef.current?.y0 ?? -80;
  const y1 = Math.max(y0 + 20, axisRef.current?.y1 ?? -20);
  const hasSag = buckets.some((b) => (b.sag ?? 0) > 0.005) || (frame?.layers.length ?? 0) > 0;
  const showMusic = !hidden.has(KEY_MUSIC);
  // Hiding a strip's line from the legend removes its strip, as for the music.
  const showSag = hasSag && !hidden.has(KEY_SAG);
  const strips = (showMusic ? MUSIC_H + GAP : 0) + (showSag ? SAG_H + GAP : 0);
  const mainH = height ? Math.max(80, height - PAD_T - strips - AXIS_H) : MAIN_H;
  const top = PAD_T;
  const bottom = PAD_T + mainH;
  const y = (v: number) => bottom - ((Math.max(y0, Math.min(y1, v)) - y0) / (y1 - y0)) * mainH;
  const yTicks: number[] = [];
  const tickStep = y1 - y0 > 60 || mainH < 110 ? 20 : 10;
  for (let v = Math.ceil(y0 / tickStep) * tickStep; v <= y1; v += tickStep) yTicks.push(v);
  // The music's level in its own strip (its own scale, −60..0 dBFS), so it
  // doesn't read against the main plot's dBc axis.
  const musicTop = bottom + GAP;
  const yMusic = (v: number) => musicTop + MUSIC_H - ((Math.max(-60, Math.min(0, v)) + 60) / 60) * MUSIC_H;
  // The sag strip: 0 at its top down to the deepest sag seen (at least 1 dB).
  const sagTop = (showMusic ? musicTop + MUSIC_H : bottom) + GAP;
  const sagMax = Math.max(1, Math.ceil(buckets.reduce((m, b) => Math.max(m, b.sag ?? 0), 0) * 2) / 2);
  const ySag = (v: number) => sagTop + (Math.min(sagMax, Math.max(0, v)) / sagMax) * SAG_H;
  const axisTop = showSag ? sagTop + SAG_H : showMusic ? musicTop + MUSIC_H : bottom;
  const h = axisTop + AXIS_H;
  const latest = [...buckets].reverse().find((b) => b.added !== null) ?? null;
  const cur = frame?.state === "playing" ? frame : null;
  const hb = hover !== null ? buckets[hover] : null;
  const accent = byParts ? "var(--fg)" : "var(--accent)";
  const items: { k: number; label: string; value: string; colour: string; title: string }[] = [
    {
      k: KEY_TOTAL,
      label: "Added",
      value: !onTop ? "" : cur ? `${sgn(cur.added_db, 0)} dBc` : latest?.added != null ? `${sgn(latest.added, 0)} dBc` : "-",
      colour: accent,
      title: "The added content against the music out: the line is each interval's average, the shaded band its loudest moment.",
    },
    ...(byParts
      ? partKeys.map((key) => {
          const p = cur?.parts.find((q) => q.key === key);
          return { k: partHideKey(key), label: partLabel(key), value: p ? `${sgn(p.level_db, 0)}` : "", colour: key === "even" ? bandColor(0) : key === "odd" ? bandColor(1) : bandColor(partHideKey(key) - 2), title: partTitle(key) };
        })
      : []),
    ...(hasSag
      ? [
          {
            k: KEY_SAG,
            label: "Sag",
            value: onTop && cur ? `${sgn(-cur.layers.reduce((m, l) => Math.max(m, l.sag_db), 0), 1)} dB` : "",
            colour: "var(--bad)",
            title: "How far the sag turns the drive down (the deepest layer): its own strip at the bottom, in dB.",
          },
        ]
      : []),
    { k: KEY_MUSIC, label: "Music", value: onTop && cur ? `${sgn(cur.music_level_dbfs, 0)} dBFS` : "", colour: "var(--muted)", title: "The music's level (RMS + 3 dB) in its own strip under the chart, −60 to 0 dBFS, to read peaks against loud passages." },
  ];
  const onMove = (e: React.MouseEvent<SVGSVGElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    const px = e.clientX - r.left;
    if (px < PAD_L || px > PAD_L + plotW) return setHover(null);
    // The nearest bucket with music, within a few, so the readout doesn't blink over a short gap.
    const i = Math.min(buckets.length - 1, Math.max(0, Math.floor(((px - PAD_L) / plotW) * buckets.length)));
    for (let d = 0; d <= 3; d++)
      for (const j of [i - d, i + d]) if (buckets[j]?.n) return setHover(j);
    setHover(null);
  };
  const tipX = hb ? x((hb.t0 + hb.t1) / 2) : 0;
  return (
    <div className="harm-live-history" ref={wrap}>
      <div className="row harm-graph-head">
        <span className="k harm-graph-title" title="The added level over time (a few points a second, kept for ten minutes while this page is open), with the sag under it.">
          History
        </span>
        <Seg<HistMode>
          label="Lines"
          value={byParts ? "parts" : "total"}
          onChange={setMode}
          options={[
            ["total", "Total", "The total added level with its peaks"],
            [
              "parts",
              split === "even_odd" ? "Even / odd" : "By order",
              split === "orders"
                ? "Each order's products on the music: H2 of every note plus the intermodulation the 2nd order makes between them, and so on (evaluated per channel from the stage's curve). The odd orders, and a single even order, are within about 0.1 dB of what the stage makes. With several even orders the stage's 5 Hz DC blocker acts on their sum, so in the moments just after a sudden level change in deep bass their lines are estimates (their total stays exact). Where the even orders' terms cancel each other, their total is shown instead"
                : split === "even_odd" && frame?.split_note
                  ? `Even and odd totals: ${frame.split_note}`
                  : split === "even_odd"
                  ? "An estimate. In series the orders mix (cross terms), so the split is by parity: the even and the odd part of the stage's curve. Its 5 Hz DC blocker can only be modelled, so in the moments just after a sudden level change in deep bass the lines can read well off"
                  : `Total only: ${frame?.split_note ?? "this stage has layers with memory"}, so its products cannot be split from its curve alone`,
              !canSplit,
            ],
          ]}
        />
        <Seg<HistoryRange> label="Time range" value={range} onChange={setRange} options={HISTORY_RANGES.map((r) => [r, RANGE_TEXT[r], `The last ${RANGE_TEXT[r]}`])} />
        <div className="spacer" />
        {head}
      </div>
      <div className="harm-live-chart">
        <svg width={w} height={h} role="img" aria-label="The added level and the sag over time" onMouseMove={onMove} onMouseLeave={() => setHover(null)}>
          <rect x={PAD_L} y={top} width={plotW} height={mainH} className="mp-bg" />
          {yTicks.map((v) => (
            <g key={"y" + v}>
              <line x1={PAD_L} x2={PAD_L + plotW} y1={y(v)} y2={y(v)} className="mp-grid" />
              <text x={PAD_L - 6} y={y(v)} className="mp-text" textAnchor="end" dominantBaseline="middle">
                {sgn(v, 0)}
              </text>
            </g>
          ))}
          {timeTicks(t0, t1).map((t) => (
            <g key={"t" + t}>
              <line x1={x(t)} x2={x(t)} y1={top} y2={axisTop} className="mp-grid minor" />
              <text x={x(t)} y={h - 5} className="mp-text" textAnchor="middle">
                {clock(t)}
              </text>
            </g>
          ))}
          {showMusic && (
            <>
              <rect x={PAD_L} y={musicTop} width={plotW} height={MUSIC_H} className="mp-bg" />
              <line x1={PAD_L} x2={PAD_L + plotW} y1={yMusic(-30)} y2={yMusic(-30)} className="mp-grid minor" />
              <text x={PAD_L - 6} y={musicTop + 1} className="mp-text" textAnchor="end" dominantBaseline="hanging">
                music
              </text>
              <text x={PAD_L - 6} y={yMusic(-30)} className="mp-text" textAnchor="end" dominantBaseline="middle">
                {sgn(-30, 0)}
              </text>
              <text x={PAD_L - 6} y={musicTop + MUSIC_H} className="mp-text" textAnchor="end" dominantBaseline="auto">
                {sgn(-60, 0)}
              </text>
              <path d={area(buckets, (b) => b.music, x, yMusic, musicTop + MUSIC_H)} className="harm-live-music-fill" />
              <path d={path(buckets, (b) => b.music, x, yMusic)} fill="none" className="harm-live-music" />
            </>
          )}
          {!hidden.has(KEY_TOTAL) && (
            <>
              <path d={area(buckets, (b) => b.peak, x, y, bottom)} className="harm-live-peak" style={{ fill: accent }} />
              <path d={path(buckets, (b) => b.added, x, y)} fill="none" style={{ stroke: accent }} strokeWidth={2} strokeLinejoin="round" />
            </>
          )}
          {byParts &&
            partKeys
              .filter((k) => !hidden.has(partHideKey(k)))
              .map((k) => (
                <path
                  key={k}
                  d={path(buckets, (b) => (b.parts[k] !== undefined && b.parts[k] > -150 ? b.parts[k] : null), x, y)}
                  fill="none"
                  style={{ stroke: k === "even" ? bandColor(0) : k === "odd" ? bandColor(1) : bandColor(partHideKey(k) - 2) }}
                  strokeWidth={1.5}
                  strokeLinejoin="round"
                />
              ))}
          {showSag && (
            <>
              <rect x={PAD_L} y={sagTop} width={plotW} height={SAG_H} className="mp-bg" />
              <line x1={PAD_L} x2={PAD_L + plotW} y1={ySag(sagMax / 2)} y2={ySag(sagMax / 2)} className="mp-grid minor" />
              <text x={PAD_L - 6} y={sagTop + 1} className="mp-text" textAnchor="end" dominantBaseline="hanging">
                sag
              </text>
              <text x={PAD_L - 6} y={sagTop + SAG_H} className="mp-text" textAnchor="end" dominantBaseline="auto">
                {sgn(-sagMax, sagMax % 1 ? 1 : 0)} dB
              </text>
              <path d={area(buckets, (b) => (b.sag === null ? null : -b.sag), x, (v) => ySag(-v), sagTop)} className="harm-live-sag-fill" />
              <path d={path(buckets, (b) => b.sag, x, ySag)} fill="none" className="harm-live-sag" />
            </>
          )}
          {hb && hb.n > 0 && <line x1={tipX} x2={tipX} y1={top} y2={axisTop} className="harm-live-cursor" />}
        </svg>
        {resize}
        {hb && hb.n > 0 && (
          <div className="harm-live-tip" style={tipX > w / 2 ? { right: w - tipX + 8 } : { left: tipX + 8 }}>
            <div className="hint">{clock((hb.t0 + hb.t1) / 2)}</div>
            <div>
              Added {sgn(hb.added ?? -160, 1)} dBc <span className="hint">peak {sgn(hb.peak ?? -160, 1)}</span>
            </div>
            {byParts &&
              partKeys
                .filter((k) => hb.parts[k] !== undefined)
                .map((k) => (
                  <div key={k}>
                    {partLabel(k)} {sgn(hb.parts[k], 1)} dBc
                  </div>
                ))}
            {hasSag && <div>Sag {sgn(-(hb.sag ?? 0), 2)} dB</div>}
            <div>Music {sgn(hb.music ?? -160, 1)} dBFS</div>
          </div>
        )}
      </div>
      <div className="legend mp-legend">
        {items.map((c) => (
          <button key={c.k} type="button" className="legend-toggle" aria-pressed={!hidden.has(c.k)} title={(hidden.has(c.k) ? `Show ${c.label}. ` : `Hide ${c.label}. `) + c.title} onClick={() => toggle(c.k)}>
            <i style={{ background: c.colour }} />
            {c.label} {c.value && <span className="mono harm-live-val">{c.value}</span>}
          </button>
        ))}
        <span className="hint">dB relative to the music{showMusic ? "; the music's own strip in dBFS" : ""}</span>
      </div>
      {byParts && split === "orders" && <p className="harm-plot-help">each order's line is its harmonics of every note plus the intermodulation it makes between notes · click a line to hide it</p>}
      {!canSplit && frame?.split_note && mode === "parts" && <p className="harm-plot-help">Total only: {frame.split_note}.</p>}
    </div>
  );
}
