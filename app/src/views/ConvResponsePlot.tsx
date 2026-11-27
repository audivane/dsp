// The Convolution page's frequency response (engine `conv_response`): the
// whole stage per output, and each item's channels or routes, from the IRs
// the engine plays at the graph rate. Magnitude or group delay, optional
// 1/N-octave smoothing; series toggle from the legend. Zoomable like the
// other response plots.

import { useEffect, useMemo, useRef, useState } from "react";
import type { AbSide, ConvResponse, EngineConfig } from "../api/types";
import { convList, deepEqual } from "../lib/chain";
import { convSeries, fitRange, seriesShown, seriesValues, type ConvQuantity, type ConvSeries, type ConvSmoothing } from "../lib/convResponse";
import { fmtDb, fmtFreq, fmtInt, fmtRate, minus } from "../lib/format";
import { sameView, type ViewLimits, type ViewRange } from "../lib/plotView";
import { usePersistentHeight } from "../lib/plotHeight";
import { errorText, store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { ResizeHandle } from "../components/ResizeHandle";
import { cssVar } from "../theme";
import { BAND_COLORS } from "./eqColors";
import { ZoomPlot, type PlotGeom } from "./ZoomPlot";

const fmtHz = (f: number) => (f >= 1000 ? `${fmtFreq(f).slice(0, -1)} kHz` : `${fmtFreq(f)} Hz`);

/** Combined outputs: the accent, then a second colour; items take the band palette. */
const OUTPUT_COLOURS = ["var(--accent)", "#e0598b", "#7bc86c", "#c9a227"];
const ITEM_COLOURS = [0, 1, 3, 4, 6, 8, 9].map((i) => BAND_COLORS[i]);
const colourOf = (c: string) => (c.startsWith("var(") ? cssVar(c.slice(4, -1)) : c);

const HEIGHT_KEY = "avdsp-conv-response-height";
const PREFS_KEY = "avdsp-conv-response";

interface Prefs {
  quantity: ConvQuantity;
  smoothing: ConvSmoothing;
  toggled: Record<string, boolean>;
}

function loadPrefs(): Prefs {
  try {
    const p = JSON.parse(localStorage.getItem(PREFS_KEY) ?? "null") as Partial<Prefs> | null;
    return {
      quantity: p?.quantity === "gd" ? "gd" : "mag",
      smoothing: p?.smoothing === 3 || p?.smoothing === 6 ? p.smoothing : 0,
      toggled: p?.toggled && typeof p.toggled === "object" ? p.toggled : {},
    };
  } catch {
    return { quantity: "mag", smoothing: 0, toggled: {} };
  }
}

function savePrefs(p: Prefs): void {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(p));
  } catch {
    /* storage unavailable */
  }
}

/** Which chain the plot reads: the master (chain A or side B) or a follower's own chain. */
export interface ConvTarget {
  side: AbSide;
  device: string | null;
}

/** The chains the plot can show: the master, A/B side B when its convolution differs, each follower. */
export function convTargets(cfg: EngineConfig | null): { key: string; target: ConvTarget; label: string; title: string }[] {
  if (!cfg) return [];
  const out: { key: string; target: ConvTarget; label: string; title: string }[] = [{ key: "a", target: { side: "a", device: null }, label: "Main", title: "Chain A on the main output" }];
  const b = cfg.ab?.chain_b;
  if (b && !deepEqual(convList(b), convList(cfg.chain))) out.push({ key: "b", target: { side: "b", device: null }, label: "A/B side B", title: "A/B side B's convolution on the main output" });
  for (const f of cfg.outputs?.followers ?? [])
    out.push({ key: `f:${f.device}`, target: { side: "a", device: f.device }, label: f.device, title: `${f.device}'s own chain (its chain patch applied; without one, chain A without the room calibration)` });
  return out;
}

/**
 * The engine's response of `target`, fetched again shortly after the
 * convolution of any chain, the graph rate or the IR files' facts change.
 */
export function useConvResponse(target: ConvTarget) {
  const st = store();
  const cfg = useApp((s) => s.config);
  const rate = useApp((s) => s.status?.graph_rate ?? 0);
  const info = useApp((s) => s.irInfo);
  const [data, setData] = useState<ConvResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seq = useRef(0);
  const key = JSON.stringify([
    target,
    rate,
    convList(cfg?.chain),
    cfg?.ab?.chain_b ? convList(cfg.ab.chain_b) : null,
    (cfg?.outputs?.followers ?? []).map((f) => f.chain_patch ?? null),
    info,
  ]);
  useEffect(() => {
    if (!cfg) return;
    const my = ++seq.current;
    const t = setTimeout(() => {
      st.convResponse(target.side === "b" ? "b" : null, target.device, () => my === seq.current)
        .then((r) => {
          if (my !== seq.current) return;
          setData(r);
          setError(null);
        })
        .catch((e) => {
          if (my === seq.current && !(e instanceof Error && e.message === "stale")) setError(errorText(e));
        });
    }, 250);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, !!cfg]);
  // A reply for another target is not shown.
  const fits = data && data.side === target.side && (data.device ?? null) === target.device;
  return { data: fits ? data : null, error };
}

type Shown = ConvSeries & { colour: string; values: (number | null)[] };

export function ConvResponsePanel({ target, setTarget, data, error }: { target: ConvTarget; setTarget: (t: ConvTarget) => void; data: ConvResponse | null; error: string | null }) {
  const cfg = useApp((s) => s.config);
  const [prefs, setPrefsState] = useState(loadPrefs);
  const setPrefs = (patch: Partial<Prefs>) => {
    const next = { ...prefs, ...patch };
    setPrefsState(next);
    savePrefs(next);
  };
  const { quantity, smoothing, toggled } = prefs;
  const [userView, setUserView] = useState<ViewRange | null>(null);
  const size = usePersistentHeight({ storageKey: HEIGHT_KEY, defaultHeight: 300, minHeight: 160, maxHeight: 900 });
  const targets = convTargets(cfg);
  const targetKey = target.side === "b" ? "b" : target.device ? `f:${target.device}` : "a";
  // A follower removed or side B gone: back to the main chain.
  const gone = cfg !== null && !targets.some((t) => t.key === targetKey);
  useEffect(() => {
    if (gone) setTarget({ side: "a", device: null });
  }, [gone, setTarget]);

  const all = useMemo(() => (data ? convSeries(data) : []), [data]);
  const freqs = data?.freqs_hz ?? [];
  const shown: Shown[] = useMemo(() => {
    let item = 0;
    const out: Shown[] = [];
    for (const s of all) {
      const colour = s.combined ? OUTPUT_COLOURS[(s.curve.out ?? 0) % OUTPUT_COLOURS.length] : ITEM_COLOURS[item++ % ITEM_COLOURS.length];
      if (seriesShown(s, toggled)) out.push({ ...s, colour, values: seriesValues(freqs, s.curve, quantity, smoothing) });
    }
    return out;
  }, [all, toggled, quantity, smoothing, freqs]);
  const legend = useMemo(() => {
    let item = 0;
    return all.map((s) => ({ s, colour: s.combined ? OUTPUT_COLOURS[(s.curve.out ?? 0) % OUTPUT_COLOURS.length] : ITEM_COLOURS[item++ % ITEM_COLOURS.length] }));
  }, [all]);

  const nyq = data ? data.rate_hz / 2 : 24000;
  const fit = fitRange(
    shown.map((s) => s.values),
    quantity,
  );
  const home: ViewRange = { x0: 20, x1: Math.min(20000, nyq), ...fit };
  const limits: ViewLimits = quantity === "mag" ? { x0: 10, x1: Math.max(nyq, 24000), y0: -210, y1: 80, minSpanX: 1.05, minSpanY: 0.1 } : { x0: 10, x1: Math.max(nyq, 24000), y0: -2000, y1: 2000, minSpanX: 1.05, minSpanY: 0.01 };
  const view = userView ?? home;
  const setView = (v: ViewRange) => setUserView(sameView(v, home) ? null : v);
  // Another quantity has another axis: back to its fit.
  useEffect(() => setUserView(null), [quantity]);

  const unit = quantity === "mag" ? "dB" : "ms";
  const fmtV = (v: number | null) => (v === null ? "-" : quantity === "mag" ? (v <= -199 ? "silent" : `${fmtDb(v, 1)} dB`) : `${minus(v.toFixed(Math.abs(v) < 10 ? 2 : 1))} ms`);

  const drawCurves = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    const v = g.view;
    const bottom = g.top + g.h;
    // Items under, the combined outputs on top.
    const order = [...shown.filter((s) => !s.combined), ...shown.filter((s) => s.combined)];
    for (const s of order) {
      ctx.strokeStyle = colourOf(s.colour);
      ctx.lineWidth = s.combined ? 2 : 1.25;
      ctx.globalAlpha = s.combined ? 1 : 0.9;
      ctx.beginPath();
      let started = false;
      for (let i = 0; i < freqs.length; i++) {
        const y = s.values[i];
        if (freqs[i + 1] !== undefined && freqs[i + 1] < v.x0) continue;
        if (freqs[i - 1] !== undefined && freqs[i - 1] > v.x1) break;
        if (y === null || (quantity === "mag" && y <= -199)) {
          started = false;
          continue;
        }
        const x = g.xOf(freqs[i]);
        const py = Math.max(g.top - 50, Math.min(bottom + 50, g.yOf(y)));
        if (!started) {
          ctx.moveTo(x, py);
          started = true;
        } else ctx.lineTo(x, py);
      }
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
  };

  const decorate = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    if (quantity !== "mag" || g.view.y0 > 0 || g.view.y1 < 0) return;
    const y = Math.round(g.yOf(0)) + 0.5;
    ctx.save();
    ctx.strokeStyle = cssVar("--muted");
    ctx.globalAlpha = 0.6;
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
    ctx.fillText(unit, g.left - 6, g.top - 12);
    ctx.restore();
  };

  const readout = (x: number) => {
    // The nearest grid point; each shown curve's value there (combined first).
    let k = 0;
    for (let i = 1; i < freqs.length; i++) if (Math.abs(Math.log(freqs[i] / x)) < Math.abs(Math.log(freqs[k] / x))) k = i;
    const list = [...shown.filter((s) => s.combined), ...shown.filter((s) => !s.combined)];
    const first = list[0]?.values[k] ?? null;
    const parts = list.slice(0, 4).map((s) => `${s.label} ${fmtV(s.values[k])}`);
    if (list.length > 4) parts.push(`+${list.length - 4}`);
    return { x: freqs[k] ?? x, y: first !== null && !(quantity === "mag" && first <= -199) ? first : null, text: `${fmtHz(freqs[k] ?? x)}  ${parts.join(" · ")}` };
  };

  const toggle = (s: ConvSeries) => setPrefs({ toggled: { ...toggled, [s.key]: !seriesShown(s, toggled) } });

  return (
    <div className="panel conv-response">
      <div className="row harm-graph-head">
        <span className="k harm-graph-title" title="What the enabled filters do, from the files the engine plays at the current graph rate (resampled, every gain applied). Combined: all enabled filters in series per output; a matrix output sums its inputs.">
          Frequency response
        </span>
        <span className="hint">{data ? `at ${fmtRate(data.rate_hz)} · combined ${fmtInt(data.combined_taps)} taps` : ""}</span>
        <div className="spacer" />
        {targets.length > 1 && (
          <Seg<string>
            label="Chain"
            value={targetKey}
            onChange={(k) => {
              const t = targets.find((x) => x.key === k);
              if (t) setTarget(t.target);
            }}
            options={targets.map((t) => [t.key, t.label, t.title])}
          />
        )}
      </div>
      <div className="row">
        <span className="k">Show</span>
        <Seg<ConvQuantity>
          label="Quantity"
          value={quantity}
          onChange={(q) => setPrefs({ quantity: q })}
          options={[
            ["mag", "Magnitude", "Level against frequency, dB"],
            ["gd", "Group delay", "How late each frequency comes out, ms (none where a curve is far down)"],
          ]}
        />
        <span className="k">Smoothing</span>
        <Seg<ConvSmoothing>
          label="Smoothing"
          value={smoothing}
          onChange={(n) => setPrefs({ smoothing: n })}
          options={[
            [0, "None", "Every point as the engine computed it (1/24 octave apart)"],
            [6, "1/6 oct", "Power average over 1/6 octave"],
            [3, "1/3 oct", "Power average over 1/3 octave"],
          ]}
        />
      </div>
      {error && <div className="notice warn">Response unavailable: {error}</div>}
      {data && data.items.length === 0 && <p className="lede">No filters on: the convolution passes the sound unchanged.</p>}
      {data && data.items.length > 0 && (
        <>
          <ZoomPlot
            ariaLabel="Frequency response of the convolution"
            className="ups-canvas-wrap"
            style={{ height: size.height ?? 300 }}
            scale="log"
            view={view}
            home={home}
            limits={limits}
            onView={setView}
            freqs={freqs}
            mags={[]}
            mode="box"
            boxAxis="xy"
            drawCurve={drawCurves}
            decorate={decorate}
            annotate={annotate}
            readout={readout}
            yTick={(v, step) => minus(v.toFixed(step < 1 ? (step < 0.1 ? 2 : 1) : 0))}
            pad={{ l: 52, r: 14, t: 24, b: 26 }}
          />
          <ResizeHandle
            label="Resize the response plot"
            value={size.height}
            min={size.minHeight}
            max={size.maxHeight}
            onChange={size.setHeight}
            onCommit={size.commitHeight}
            onStep={size.stepHeight}
            onReset={size.resetHeight}
          />
          <div className="legend mp-legend">
            {legend.map(({ s, colour }) => (
              <button key={s.key} type="button" className="legend-toggle" aria-pressed={seriesShown(s, toggled)} title={seriesShown(s, toggled) ? `Hide ${s.label}` : `Show ${s.label}`} onClick={() => toggle(s)}>
                <i style={{ background: colour }} />
                {s.label}
              </button>
            ))}
          </div>
          <p className="harm-plot-help">click a name to show or hide its curve · drag to zoom, ctrl-drag to pan · double-click to reset</p>
        </>
      )}
    </div>
  );
}
