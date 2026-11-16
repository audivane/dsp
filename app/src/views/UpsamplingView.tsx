import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { DaemonError, ErrorCode } from "../api/transport";
import type { RateTarget, ResponseWindow, ResponseWindowRequest, UpsamplerConfig, UpsamplerResponse } from "../api/types";
import { Chevron } from "../components/Icons";
import { ResizeHandle } from "../components/ResizeHandle";
import { Seg } from "../components/Seg";
import { fmtInt, fmtMs, fmtRate, fmtRateShort, fmtTaps, minus } from "../lib/format";
import {
  MAX_PLOT_HEIGHT,
  PLOT_HEIGHT_KEYS,
  UPS_SUB_DEFAULT_HEIGHT,
  UPS_SUB_MIN_HEIGHT,
  UPS_TRANS_DEFAULT_HEIGHT,
  UPS_TRANS_MIN_HEIGHT,
  usePersistentHeight,
} from "../lib/plotHeight";
import {
  DEFAULT_MIN_SPAN_X_HZ,
  DEFAULT_MIN_SPAN_Y_DB,
  decimalsFor,
  linearTicks,
  sameView,
  spliceWindow,
  steepnessText,
  viewForCrossing,
  type ViewLimits,
  type ViewRange,
} from "../lib/plotView";
import { cutoffTips, fmtHzStat, fmtRipple, windowRequestFor } from "../lib/upsamplerPlot";
import {
  KAISER_BETA_HINT,
  KAISER_BETA_PRESETS,
  kaiserAttenuationDb,
  kaiserBetaBlockReason,
  kaiserPresetTooltip,
  kaiserTransitionWidthHz,
  minimumPhaseBlockReason,
  nyquistStopBlockReason,
  nyquistStopMinTaps,
  resolveRatio,
  selectedKaiserPreset,
  worstCaseRatio,
} from "../lib/upsamplerLimits";
import { chosenPreviewRate, viewSourceRate } from "../lib/upsamplerRate";
import { previewRates } from "../lib/rates";
import { errorText, store, useApp } from "../store/store";
import { cssVar, useSkin } from "../theme";
import { ZoomPlot, type BoxAxis, type DragMode, type PlotGeom } from "./ZoomPlot";

type RatioKey = "off" | "2" | "4" | "8" | "16" | "max384" | "max768";
const ADVANCED_OPEN_KEY = "avdsp-upsampling-advanced";

function loadAdvancedOpen(): boolean {
  try {
    return localStorage.getItem(ADVANCED_OPEN_KEY) === "1";
  } catch {
    return false;
  }
}

function saveAdvancedOpen(open: boolean): void {
  try {
    localStorage.setItem(ADVANCED_OPEN_KEY, open ? "1" : "0");
  } catch {
    /* storage unavailable */
  }
}

function formatBeta(beta: number): string {
  return Number.isInteger(beta) ? String(beta) : String(Number(beta.toFixed(3)));
}

function fmtTransitionHz(hz: number): string {
  const digits = hz >= 10 ? 1 : hz >= 1 ? 2 : 3;
  return fmtHzStat(hz, digits);
}

const toKey = (t: RateTarget): RatioKey | null => {
  if (t === "off") return "off";
  if ("ratio" in t) return ["2", "4", "8", "16"].includes(String(t.ratio)) ? (String(t.ratio) as RatioKey) : null;
  return t.max_rate === 384000 ? "max384" : t.max_rate === 768000 ? "max768" : null;
};
const fromKey = (k: RatioKey): RateTarget => (k === "off" ? "off" : k === "max384" ? { max_rate: 384000 } : k === "max768" ? { max_rate: 768000 } : { ratio: Number(k) });

function fmtNyquistKhz(hz: number): string {
  const k = hz / 1000;
  return `${Number(k.toFixed(2))} kHz`;
}

function PlotCanvas({
  ariaLabel,
  className = "ups-canvas-wrap",
  style,
  draw,
}: {
  ariaLabel: string;
  className?: string;
  style?: React.CSSProperties;
  draw: (ctx: CanvasRenderingContext2D, w: number, h: number) => void;
}) {
  const ref = useRef<HTMLCanvasElement>(null);
  const drawRef = useRef(draw);
  drawRef.current = draw;
  const renderRef = useRef<(() => void) | null>(null);
  const skin = useSkin();

  useEffect(() => {
    const cv = ref.current;
    if (!cv) return;
    const ctx = cv.getContext("2d");
    if (!ctx) return;
    const render = () => {
      const r = cv.getBoundingClientRect();
      const w = r.width;
      const h = r.height;
      if (w <= 0 || h <= 0) return;
      const d = window.devicePixelRatio || 1;
      const pw = Math.max(1, Math.round(w * d));
      const ph = Math.max(1, Math.round(h * d));
      if (cv.width !== pw || cv.height !== ph) {
        cv.width = pw;
        cv.height = ph;
      }
      ctx.setTransform(d, 0, 0, d, 0, 0);
      drawRef.current(ctx, w, h);
    };
    renderRef.current = render;
    const ro = new ResizeObserver(render);
    ro.observe(cv);
    render();
    return () => {
      renderRef.current = null;
      ro.disconnect();
    };
  }, []);

  useEffect(() => {
    renderRef.current?.();
  }, [draw, skin]);

  return (
    <div className={className} style={style}>
      <canvas ref={ref} role="img" aria-label={ariaLabel} />
    </div>
  );
}

function drawImpulse(ctx: CanvasRenderingContext2D, w: number, h: number, resp: UpsamplerResponse) {
  const PL = 42;
  const PR = 12;
  const PT = 14;
  const PB = 24;
  const plotW = w - PL - PR;
  const plotH = h - PT - PB;
  ctx.clearRect(0, 0, w, h);
  if (plotW <= 0 || plotH <= 0) return;

  const mono = cssVar("--mono");
  const muted = cssVar("--muted");
  const grid = cssVar("--grid");
  const gridS = cssVar("--grid-strong");
  const accent = cssVar("--accent");
  const warn = cssVar("--warn");

  const imp = resp.impulse;
  if (imp.length === 0) return;

  let minV = -0.3;
  let maxV = 1.05;
  for (const v of imp) {
    if (v < minV) minV = v * 1.1;
    if (v > maxV) maxV = v * 1.05;
  }
  const yOfVal = (v: number) => PT + ((maxV - v) / (maxV - minV)) * plotH;

  ctx.font = "10.5px " + mono;
  ctx.textBaseline = "middle";
  ctx.lineWidth = 1;

  const yt = linearTicks(minV, maxV, Math.max(3, Math.round(plotH / 36)));
  const yd = decimalsFor(yt.step);
  for (const v of yt.ticks) {
    if (v < minV || v > maxV) continue;
    const y = Math.round(yOfVal(v)) + 0.5;
    const isZero = Math.abs(v) < yt.step * 1e-6;
    ctx.strokeStyle = isZero ? gridS : grid;
    ctx.beginPath();
    ctx.moveTo(PL, y);
    ctx.lineTo(PL + plotW, y);
    ctx.stroke();
    ctx.fillStyle = muted;
    ctx.textAlign = "right";
    const s = (isZero ? 0 : v).toFixed(yd);
    ctx.fillText(minus(s), PL - 5, y);
  }

  const n = imp.length;
  const peakIdx = Math.min(n - 1, Math.max(0, resp.impulse_peak));
  const xOfIdx = (i: number) => (n === 1 ? PL + plotW / 2 : PL + (i / (n - 1)) * plotW);

  const px = Math.round(xOfIdx(peakIdx)) + 0.5;
  ctx.save();
  ctx.strokeStyle = warn;
  ctx.setLineDash([3, 3]);
  ctx.beginPath();
  ctx.moveTo(px, PT);
  ctx.lineTo(px, PT + plotH);
  ctx.stroke();
  ctx.restore();

  ctx.fillStyle = muted;
  ctx.textAlign = "center";
  ctx.fillText("0", px, h - PB / 2);
  if (n > 8) {
    ctx.textAlign = "left";
    ctx.fillText(minus(String(-peakIdx)), PL, h - PB / 2);
    ctx.textAlign = "right";
    ctx.fillText("+" + String(n - 1 - peakIdx), PL + plotW, h - PB / 2);
  }

  ctx.save();
  ctx.beginPath();
  ctx.rect(PL, PT - 2, plotW, plotH + 4);
  ctx.clip();
  ctx.strokeStyle = accent;
  ctx.lineWidth = 1.75;
  ctx.lineJoin = "round";
  if (n === 1) {
    ctx.beginPath();
    ctx.moveTo(px, yOfVal(0));
    ctx.lineTo(px, yOfVal(imp[0]));
    ctx.stroke();
  } else {
    ctx.beginPath();
    for (let i = 0; i < n; i++) {
      const x = xOfIdx(i);
      const y = yOfVal(imp[i]);
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }
  ctx.restore();
}


type PlotId = "trans" | "full";
const WINDOW_DEBOUNCE_MS = 220;

/** Home views and limits of the two zoomable plots for a response. */
function plotFrames(r: UpsamplerResponse): Record<PlotId, { home: ViewRange; limits: ViewLimits }> {
  const nyqIn = r.source_rate / 2;
  const nyqOut = Math.max(nyqIn, r.output_rate / 2);
  return {
    trans: {
      home: { x0: 0.73 * nyqIn, x1: Math.min(nyqOut, 1.18 * nyqIn), y0: -150, y1: 3 },
      limits: { x0: 0, x1: nyqOut, y0: -220, y1: 20, minSpanX: DEFAULT_MIN_SPAN_X_HZ, minSpanY: DEFAULT_MIN_SPAN_Y_DB },
    },
    full: {
      home: { x0: 20, x1: nyqOut, y0: -150, y1: 3 },
      limits: { x0: 10, x1: nyqOut, y0: -220, y1: 20, minSpanX: 1.00001, minSpanY: DEFAULT_MIN_SPAN_Y_DB },
    },
  };
}

function drawNyquistMarker(ctx: CanvasRenderingContext2D, g: PlotGeom, nyqIn: number) {
  const x = Math.round(g.xOf(nyqIn)) + 0.5;
  if (x < g.left || x > g.left + g.w) return;
  ctx.save();
  ctx.strokeStyle = cssVar("--warn");
  ctx.lineWidth = 1.25;
  ctx.setLineDash([5, 4]);
  ctx.beginPath();
  ctx.moveTo(x, g.top);
  ctx.lineTo(x, g.top + g.h);
  ctx.stroke();
  ctx.restore();
}

function drawTransitionBand(ctx: CanvasRenderingContext2D, g: PlotGeom, r: UpsamplerResponse) {
  if (r.ratio <= 1 || !(r.minus_0_1_db_hz > 0) || !(r.stopband_start_hz > r.minus_0_1_db_hz)) return;
  const x0 = Math.max(g.left, g.xOf(r.minus_0_1_db_hz));
  const x1 = Math.min(g.left + g.w, g.xOf(r.stopband_start_hz));
  if (x1 <= x0) return;
  ctx.fillStyle = cssVar("--accent-soft");
  ctx.fillRect(x0, g.top, Math.max(2, x1 - x0), g.h);
}

function drawCrossingDots(ctx: CanvasRenderingContext2D, g: PlotGeom, r: UpsamplerResponse, selected: number | null) {
  ctx.save();
  r.crossings.forEach((c, i) => {
    const x = g.xOf(c.freq_hz);
    const y = g.yOf(c.level_db);
    if (x < g.left - 4 || x > g.left + g.w + 4 || y < g.top - 4 || y > g.top + g.h + 4) return;
    ctx.fillStyle = i === selected ? cssVar("--warn") : cssVar("--fg");
    ctx.globalAlpha = i === selected ? 1 : 0.6;
    ctx.beginPath();
    ctx.arc(x, y, i === selected ? 3.5 : 2.25, 0, Math.PI * 2);
    ctx.fill();
  });
  ctx.restore();
}

function drawChip(ctx: CanvasRenderingContext2D, text: string, x: number, y: number, color: string) {
  ctx.font = "500 11px " + cssVar("--font");
  const tw = ctx.measureText(text).width;
  ctx.fillStyle = cssVar("--graph-bg");
  ctx.globalAlpha = 0.88;
  ctx.fillRect(x - 4, y - 8, tw + 8, 17);
  ctx.globalAlpha = 1;
  ctx.fillStyle = color;
  ctx.textAlign = "left";
  ctx.textBaseline = "middle";
  ctx.fillText(text, x, y);
}

/**
 * Zoom windows for the two plots, fetched from the daemon one at a time
 * (the analysis connection runs one call at a time; a newer one supersedes),
 * debounced, and only after the base measurement for the current design is
 * in. Replies for a window that is no longer wanted are dropped.
 */
function useResponseWindows(
  base: UpsamplerResponse | null,
  baseKey: string,
  ready: boolean,
  wanted: Record<PlotId, ResponseWindowRequest | null>,
  fetchWindow: (w: ResponseWindowRequest) => Promise<UpsamplerResponse>,
): Record<PlotId, ResponseWindow | null> {
  const [windows, setWindows] = useState<Record<PlotId, { key: string; req: ResponseWindowRequest; win: ResponseWindow } | null>>({ trans: null, full: null });
  const inflight = useRef(false);
  const [tick, setTick] = useState(0);
  const wantedRef = useRef(wanted);
  wantedRef.current = wanted;
  const keyRef = useRef(baseKey);
  keyRef.current = baseKey;

  const same = (a: ResponseWindowRequest | null | undefined, b: ResponseWindowRequest | null | undefined) =>
    !!a && !!b && a.f_lo_hz === b.f_lo_hz && a.f_hi_hz === b.f_hi_hz && a.points === b.points;

  const wantKey = `${JSON.stringify(wanted.trans)}|${JSON.stringify(wanted.full)}`;
  useEffect(() => {
    if (!base || !ready || inflight.current) return;
    const pick = (["trans", "full"] as const).find((id) => {
      const w = wanted[id];
      const have = windows[id];
      return w !== null && !(have && have.key === baseKey && same(have.req, w));
    });
    if (!pick) return;
    const req = wanted[pick]!;
    const timer = setTimeout(() => {
      inflight.current = true;
      fetchWindow(req)
        .then((res) => {
          if (keyRef.current !== baseKey || !same(wantedRef.current[pick], req) || !res.window) return;
          const win = res.window;
          setWindows((prev) => ({ ...prev, [pick]: { key: baseKey, req, win } }));
        })
        .catch(() => undefined)
        .finally(() => {
          inflight.current = false;
          setTick((t) => t + 1);
        });
    }, WINDOW_DEBOUNCE_MS);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base, baseKey, ready, wantKey, windows, tick]);

  const valid = (id: PlotId) => {
    const w = windows[id];
    return w && w.key === baseKey && wanted[id] !== null ? w.win : null;
  };
  return { trans: valid("trans"), full: valid("full") };
}

export function UpsamplingView() {
  const st = store();
  const u = useApp((s) => s.config?.chain.upsampler);
  const statusState = useApp((s) => s.status?.state);
  const rawGraphRate = useApp((s) => s.status?.graph_rate);
  const effectiveDefaultRate = useApp((s) => s.status?.effective_default_rate ?? null);
  const statusRatio = useApp((s) => s.status?.ratio ?? null);
  const activeSink = useApp((s) => s.status?.active_sink ?? null);
  // A reopen (filter length, ratio...) reports "switching" for a moment: keep
  // the last rate so the readout, plot and measurement do not jump to another
  // rate and back.
  const effectiveRates = useApp((s) => s.status?.effective_rates);
  const allowedRates = useApp((s) => s.config?.rate.allowed_rates);
  const rates = useMemo(() => previewRates(effectiveRates, allowedRates), [effectiveRates, allowedRates]);
  const heldRate = useRef<number | null>(null);
  const playingRate = viewSourceRate({ state: statusState, graphRate: rawGraphRate, effectiveDefaultRate, held: heldRate.current, rates });
  if (statusState !== "switching") heldRate.current = playingRate;
  // "Preview at" is only a view: a pick that is no longer offered (the rate
  // was turned off on the Device page) falls back to following playback.
  const [pickedRate, setPickedRate] = useState<number | null>(null);
  const chosenRate = chosenPreviewRate(pickedRate, rates);
  useEffect(() => {
    if (pickedRate !== null && chosenRate === null) setPickedRate(null);
  }, [pickedRate, chosenRate]);
  const sourceRate = chosenRate ?? playingRate;
  const playingNow = statusState === "playing" && rawGraphRate ? rawGraphRate : null;
  const [recentPlayingRate, setRecentPlayingRate] = useState<number | null>(() =>
    statusState === "playing" && rawGraphRate && rawGraphRate > 0 ? rawGraphRate : null,
  );
  useEffect(() => {
    if (statusState === "playing" && rawGraphRate && rawGraphRate > 0) {
      setRecentPlayingRate(rawGraphRate);
    }
  }, [statusState, rawGraphRate]);
  const [advancedOpen, setAdvancedOpen] = useState<boolean>(loadAdvancedOpen);
  const toggleAdvanced = () => {
    const next = !advancedOpen;
    setAdvancedOpen(next);
    saveAdvancedOpen(next);
  };

  const [response, setResponse] = useState<UpsamplerResponse | null>(null);
  const [responseKey, setResponseKey] = useState("");
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [retryTick, setRetryTick] = useState(0);
  const reqSeq = useRef(0);

  const [mode, setMode] = useState<DragMode>("box");
  const [boxAxis, setBoxAxis] = useState<BoxAxis>("x");
  const [views, setViews] = useState<Record<PlotId, ViewRange | null>>({ trans: null, full: null });
  const [selected, setSelected] = useState<number | null>(null);
  const transHeight = usePersistentHeight({
    storageKey: PLOT_HEIGHT_KEYS.upsTrans,
    defaultHeight: UPS_TRANS_DEFAULT_HEIGHT,
    minHeight: UPS_TRANS_MIN_HEIGHT,
    maxHeight: MAX_PLOT_HEIGHT,
  });
  const subHeight = usePersistentHeight({
    storageKey: PLOT_HEIGHT_KEYS.upsSub,
    defaultHeight: UPS_SUB_DEFAULT_HEIGHT,
    minHeight: UPS_SUB_MIN_HEIGHT,
    maxHeight: MAX_PLOT_HEIGHT,
  });

  const uKey = u ? JSON.stringify(u) : "";
  const key = `${uKey}@${sourceRate}`;
  useEffect(() => {
    if (!u) return;
    const seq = ++reqSeq.current;
    setLoading(true);
    setErr(null);
    const timer = setTimeout(() => {
      void st
        .upsamplerResponse(u, sourceRate)
        .then((res) => {
          if (reqSeq.current !== seq) return;
          setResponse(res);
          setResponseKey(key);
          setLoading(false);
        })
        .catch((e: unknown) => {
          if (reqSeq.current !== seq) return;
          if (e instanceof DaemonError && e.code === ErrorCode.SUPERSEDED) return;
          const msg =
            e instanceof DaemonError && e.code === ErrorCode.TIMEOUT
              ? "Filter measurement timed out (long filters can take a while)."
              : errorText(e);
          setErr(msg);
          setLoading(false);
        });
    }, 140);
    return () => {
      clearTimeout(timer);
      reqSeq.current++;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [uKey, sourceRate, retryTick]);

  // A new source rate or ratio changes the axes: start from the home views.
  const frameKey = response ? `${response.source_rate}/${response.output_rate}` : "";
  useEffect(() => {
    setViews({ trans: null, full: null });
    setSelected(null);
  }, [frameKey]);

  const frames = useMemo(() => (response ? plotFrames(response) : null), [response]);
  const viewOf = (id: PlotId) => views[id] ?? frames?.[id].home ?? { x0: 0, x1: 1, y0: -150, y1: 0 };
  const transView = viewOf("trans");
  const fullView = viewOf("full");

  const zoomed = (id: PlotId) => !!frames && views[id] !== null && !sameView(views[id]!, frames[id].home);
  const wanted = useMemo(
    () => ({
      trans: response && frames && zoomed("trans") ? windowRequestFor(transView, "lin", response.output_rate / 2) : null,
      full: response && frames && zoomed("full") ? windowRequestFor(fullView, "log", response.output_rate / 2) : null,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [response, frames, views],
  );
  const fetchWindow = useCallback((w: ResponseWindowRequest) => st.upsamplerResponse(u ?? null, sourceRate, w), [st, u, sourceRate]);
  const windows = useResponseWindows(response, responseKey, !loading && responseKey === key, wanted, fetchWindow);

  if (!u) return null;
  const set = (p: Partial<UpsamplerConfig>, ok?: string) => st.setUpsampler(p, ok);
  const off = u.target === "off";
  const r = response;
  const ratioOff = r ? r.ratio <= 1 : off;
  const latMs = r && r.output_rate > 0 ? (r.latency_samples / r.output_rate) * 1000 : 0;
  const tips = cutoffTips(sourceRate, r && r.source_rate === sourceRate ? r : null, u.cutoff_mode);
  // Options the daemon would reject for the current settings (see upsamplerLimits).
  const nyqBlock = nyquistStopBlockReason(u);
  const minPhaseBlock = minimumPhaseBlockReason(u.kaiser_beta);
  const nyqMinTaps = u.cutoff_mode === "nyquist_stop" ? nyquistStopMinTaps(worstCaseRatio(u.target), u.kaiser_beta) : null;
  const tapsOption = (taps: number, text: string, title: string): [number, string, string, boolean] =>
    nyqMinTaps !== null && taps < nyqMinTaps
      ? [taps, text, `${title}. Not available with Stop at Nyquist here: it is only guaranteed from ${nyqMinTaps.toLocaleString("en-US")} taps.`, true]
      : [taps, text, title, false];
  const knownSourceRate = chosenRate ?? (statusState === "playing" && rawGraphRate && rawGraphRate > 0 ? rawGraphRate : recentPlayingRate);
  const readoutSourceRate = knownSourceRate ?? playingRate;
  const isFallbackRate = knownSourceRate === null;
  const readoutRatio = resolveRatio(u.target, readoutSourceRate);
  const readoutOutRate = readoutSourceRate * readoutRatio;
  // The device may not take source × ratio: the engine then fell back to a
  // smaller ratio (down to ×1) and says so in status.
  const engineLive = (statusState === "playing" || statusState === "ready") && activeSink !== "direct";
  const wantedRatio = rawGraphRate ? resolveRatio(u.target, rawGraphRate) : 1;
  const ratioCapped = !off && engineLive && !!rawGraphRate && statusRatio !== null && statusRatio < wantedRatio;
  const ratioCapNote = ratioCapped
    ? `This device cannot run at ${fmtRate(rawGraphRate! * wantedRatio)}: ${statusRatio! <= 1 ? "upsampling is off (×1)" : `upsampling at ×${statusRatio} (${fmtRate(rawGraphRate! * statusRatio!)})`} at a ${fmtRate(rawGraphRate!)} source.`
    : null;
  const designedAttenDb = kaiserAttenuationDb(u.kaiser_beta);
  const designedTwHz = !off && readoutRatio > 1 ? kaiserTransitionWidthHz(u.kaiser_beta, u.taps, readoutOutRate) : null;
  const selectedBeta = selectedKaiserPreset(u.kaiser_beta);
  const betaOptions: [number, string, string, boolean][] = KAISER_BETA_PRESETS.map(({ beta, name, label }) => {
    const block = kaiserBetaBlockReason(u, beta);
    return [beta, label, block ?? kaiserPresetTooltip(beta, name), block !== null && selectedBeta !== beta];
  });
  const steep = r ? steepnessText(r.crossings) : null;
  const setView = (id: PlotId) => (v: ViewRange) => setViews((prev) => ({ ...prev, [id]: v }));
  const transData = r ? spliceWindow(r.freqs_hz, r.magnitude_db, windows.trans) : { freqs: [], mags: [] };
  const fullData = r ? spliceWindow(r.freqs_hz, r.magnitude_db, windows.full) : { freqs: [], mags: [] };
  const hzDigits = r && r.crossings.length > 1 && r.crossings[r.crossings.length - 1].freq_hz - r.crossings[0].freq_hz < 200 ? 3 : 1;
  const resetAll = () => {
    setViews({ trans: null, full: null });
    setSelected(null);
  };

  return (
    <section className="view" aria-labelledby="ups-title">
      <div className="view-head">
        <h1 id="ups-title">Upsampling</h1>
        <span className="preview-at">
          <span className="preview-label" id="ups-preview-label" title="The source rate the readouts and plots below are shown for. Only a view: it changes nothing in the engine.">
            Preview at
          </span>
          <Seg
            className="preview-seg"
            label="Preview at source rate"
            value={sourceRate}
            onChange={(rate) => setPickedRate(rate === playingNow ? null : rate)}
            options={rates.map((rate) => [rate, fmtRateShort(rate), rate === playingNow ? "Playing now" : `Show the filter for a ${fmtRate(rate)} source`])}
          />
          {playingNow !== null && sourceRate !== playingNow && (
            <button type="button" className="preview-playing" title="Follow the playing rate again" onClick={() => setPickedRate(null)}>
              Playing: {fmtRateShort(playingNow)}
            </button>
          )}
        </span>
        {loading && (
          <span className="ups-loading" role="status">
            Measuring… (long filters take a while)
          </span>
        )}
        <div className="spacer" />
        <div className="seg" role="group" aria-label="Presets">
          <button title="256k-tap linear-phase filter, full band (×4 if upsampling is off)" onClick={() => set({ taps: 262144, phase: "linear", cutoff: 1, cutoff_mode: "fraction", target: off ? { ratio: 4 } : u.target }, "Music: long linear-phase filter")}>Music preset</button>
          <button title="16k-tap minimum-phase filter, full band (×2 if upsampling is off): for games and video" onClick={() => set({ taps: 16384, phase: "minimum", cutoff: 1, cutoff_mode: "fraction", target: off ? { ratio: 2 } : u.target }, "Low latency: short minimum-phase filter")}>Low-latency preset</button>
        </div>
      </div>

      <div className="panel">
        <div className="row">
          <span className="k">Ratio</span>
          <Seg
            label="Ratio"
            value={toKey(u.target)}
            onChange={(k) => set({ target: fromKey(k) })}
            options={[
              ["off", "Off", "No upsampling: the DAC gets the source rate and its own filter does the reconstruction"],
              ["2", "×2", "Twice the source rate (44.1k → 88.2k, 48k → 96k)"],
              ["4", "×4", "Four times the source rate (44.1k → 176.4k, 48k → 192k)"],
              ["8", "×8", "Eight times the source rate (44.1k → 352.8k, 48k → 384k)"],
              ["16", "×16", "Sixteen times the source rate (44.1k → 705.6k, 48k → 768k)"],
              ["max384", "Up to 384k", "Highest power-of-two multiple up to 384 kHz"],
              ["max768", "Up to 768k", "Highest power-of-two multiple up to 768 kHz"],
            ]}
          />
          {ratioCapNote && <span className="hint">{ratioCapNote}</span>}
        </div>
        <div className="row">
          <span className="k">Filter length</span>
          <Seg
            label="Filter length"
            value={u.taps}
            disabled={off}
            onChange={(taps) => set({ taps })}
            options={[
              tapsOption(16384, "16k", "16,384 taps: lightest on CPU, lowest latency, widest transition band"),
              tapsOption(32768, "32k", "32,768 taps: half the transition band of 16k at little CPU cost. Linear-phase delay is half the length: 21 ms at 768 kHz, 0.17 s at 96 kHz"),
              tapsOption(65536, "64k", "65,536 taps: a steep filter at moderate CPU cost"),
              tapsOption(131072, "128k", "131,072 taps: steeper, more CPU. Linear-phase delay is half the length: 85 ms at 768 kHz, 0.68 s at 96 kHz"),
              tapsOption(262144, "256k", "262,144 taps: very steep, more CPU. Linear-phase delay is half the length: 0.17 s at 768 kHz, 1.4 s at 96 kHz"),
              tapsOption(524288, "512k", "524,288 taps: very steep, heavy on CPU. Linear-phase delay is half the length: 0.34 s at 768 kHz, 2.7 s at 96 kHz"),
              tapsOption(1048576, "1M", "1,048,576 taps: very steep, heavy on CPU. Linear-phase delay is half the length: 0.68 s at 768 kHz, 5.5 s at 96 kHz"),
              tapsOption(2097152, "2M", "⚠ 2,097,152 taps: extreme. Linear-phase delay 1.4 s at 768 kHz, 10.9 s at 96 kHz, so music only. Watch DSP % and xruns: it may not keep up in real time on every CPU. Measuring the response takes a few seconds."),
              tapsOption(4194304, "4M", "⚠ 4,194,304 taps (the maximum): extreme. Linear-phase delay 2.7 s at 768 kHz, 21.8 s at 96 kHz, so music only. Watch DSP % and xruns: it may not keep up in real time on every CPU. Measuring the response takes a few seconds."),
            ]}
          />
          {![16384, 32768, 65536, 131072, 262144, 524288, 1048576, 2097152, 4194304].includes(u.taps) && <span className="hint">{u.taps.toLocaleString("en-US")} taps (set elsewhere)</span>}
        </div>
        <div className="row">
          <span className="k">Phase</span>
          <Seg
            label="Phase"
            value={u.phase}
            disabled={off}
            onChange={(phase) => set({ phase })}
            options={[
              ["linear", "Linear", "No phase shift; adds latency"],
              ["minimum", "Minimum", minPhaseBlock ?? "Almost no latency; shifts phase near the cutoff", minPhaseBlock !== null && u.phase !== "minimum"],
            ]}
          />
        </div>
        <div className="row">
          <span className="k">Cutoff</span>
          <Seg<"full" | "nyquist_stop" | "apod">
            label="Cutoff"
            value={u.cutoff_mode === "nyquist_stop" ? "nyquist_stop" : u.cutoff >= 1 ? "full" : "apod"}
            disabled={off}
            onChange={(v) =>
              set(
                v === "nyquist_stop"
                  ? { cutoff_mode: "nyquist_stop" }
                  : { cutoff_mode: "fraction", cutoff: v === "full" ? 1 : 0.95 },
              )
            }
            options={[
              ["full", "Full band", tips.full],
              ["nyquist_stop", "Stop at Nyquist", nyqBlock ?? tips.nyquistStop, nyqBlock !== null && u.cutoff_mode !== "nyquist_stop"],
              ["apod", "Apodizing", tips.apod],
            ]}
          />
          {u.cutoff_mode === "nyquist_stop" && r && !ratioOff && (
            <span className="hint">
              {r.effective_cutoff.toFixed(6)}× Nyquist ({fmtHzStat(r.effective_cutoff * (r.source_rate / 2), hzDigits)} Hz)
            </span>
          )}
        </div>
        <div className="ups-adv">
          <button
            type="button"
            className="ups-adv-toggle"
            aria-expanded={advancedOpen}
            aria-controls="ups-adv-body"
            onClick={toggleAdvanced}
          >
            <Chevron className={"chev" + (advancedOpen ? " open" : "")} />
            <span>Advanced</span>
          </button>
          {advancedOpen && (
            <div id="ups-adv-body" className="ups-adv-body">
              <div className="row">
                <span className="k">Filter shape (Kaiser β)</span>
                <Seg<number>
                  className="beta-seg"
                  label="Filter shape (Kaiser β)"
                  value={selectedBeta}
                  disabled={off}
                  onChange={(kaiser_beta) => set({ kaiser_beta })}
                  options={betaOptions}
                />
                {selectedBeta === null && <span className="hint">Custom β = {formatBeta(u.kaiser_beta)} (set from CLI)</span>}
              </div>
              <div className="ups-adv-readout">{KAISER_BETA_HINT}</div>
              <div className="ups-adv-readout" aria-label="Kaiser filter design estimate">
                Designed stopband ≈ {designedAttenDb.toFixed(1)} dB · transition width{" "}
                {off
                  ? "— (upsampling is off)"
                  : readoutRatio <= 1 || designedTwHz === null
                    ? "—"
                    : `≈ ${fmtTransitionHz(designedTwHz)} Hz (at ${fmtRate(readoutOutRate)}${isFallbackRate ? `, assuming a ${fmtRate(readoutSourceRate)} source` : ""})`}
              </div>
            </div>
          )}
        </div>
      </div>

      {err && (
        <div className="notice warn ups-err" role="alert">
          <span>{err}</span>
          <button type="button" className="btn small" onClick={() => setRetryTick((n) => n + 1)}>
            Retry
          </button>
        </div>
      )}

      {r && frames && (
        <>
          <div className="ups-plots">
            <div className="ups-plot-card resizable">
              <div className="ups-plot-head">
                <span className="ups-plot-title">Nyquist transition</span>
                <span className="ups-plot-sub">
                  {ratioOff
                    ? `Upsampling is off (1:1 bypass at ${fmtRate(r.source_rate)})`
                    : `${fmtRate(r.source_rate)} → ${fmtRate(r.output_rate)} (×${r.ratio})`}
                </span>
                <div className="spacer" />
                <div
                  className="plot-tools"
                  title="Drag: zoom to a box (Shift swaps X / X+Y). Ctrl, Alt or middle-drag: pan. Wheel: zoom frequency (Shift: level). Double-click: reset."
                >
                  <Seg<DragMode>
                    label="Drag mode"
                    value={mode}
                    onChange={setMode}
                    options={[
                      ["box", "Zoom", "Drag a box to zoom"],
                      ["pan", "Pan", "Drag to move the view"],
                    ]}
                  />
                  <Seg<BoxAxis>
                    label="Box zoom axes"
                    value={boxAxis}
                    onChange={setBoxAxis}
                    options={[
                      ["x", "X", "Box zoom changes frequency only"],
                      ["xy", "X+Y", "Box zoom changes frequency and level"],
                    ]}
                  />
                  <button type="button" className="btn small" disabled={!zoomed("trans") && !zoomed("full")} onClick={resetAll} title="Reset both plots (or double-click a plot)">
                    Reset
                  </button>
                </div>
              </div>
              <ZoomPlot
                ariaLabel={`Upsampler response around the source Nyquist, ${fmtInt(transView.x0)} to ${fmtInt(transView.x1)} Hz`}
                style={transHeight.height !== null ? { height: transHeight.height } : undefined}
                scale="lin"
                view={transView}
                home={frames.trans.home}
                limits={frames.trans.limits}
                onView={setView("trans")}
                freqs={transData.freqs}
                mags={transData.mags}
                mode={mode}
                boxAxis={boxAxis}
                lineWidth={2.25}
                decorate={(ctx, g) => {
                  drawTransitionBand(ctx, g, r);
                  drawNyquistMarker(ctx, g, r.source_rate / 2);
                  drawCrossingDots(ctx, g, r, selected);
                }}
                annotate={(ctx, g) => {
                  const nyq = r.source_rate / 2;
                  const nx = g.xOf(nyq);
                  if (nx >= g.left && nx <= g.left + g.w) {
                    ctx.save();
                    ctx.font = "600 11px " + cssVar("--mono");
                    ctx.fillStyle = cssVar("--warn");
                    const right = nx > g.left + g.w - 110;
                    ctx.textAlign = right ? "right" : "left";
                    ctx.textBaseline = "bottom";
                    ctx.fillText(`${fmtNyquistKhz(nyq)} Nyquist`, nx + (right ? -6 : 6), g.top - 4);
                    ctx.restore();
                  }
                  ctx.save();
                  drawChip(ctx, r.ratio > 1 ? `Flat to ${fmtHzStat(r.minus_0_1_db_hz, hzDigits)} Hz (−0.1 dB)` : "Flat 1:1 (upsampling off)", g.left + 10, g.top + 13, cssVar("--accent"));
                  if (steep) drawChip(ctx, steep, g.left + 10, g.top + 33, cssVar("--fg"));
                  ctx.restore();
                }}
              />
              {r.crossings.length > 0 && (
                <div className="crossings" aria-label="Threshold crossings">
                  <span className="crossings-title" title="First frequency where the measured response falls to each level, refined to under 0.001 Hz. Click one to zoom to it.">
                    Crossings
                  </span>
                  {r.crossings.map((c, i) => (
                    <button
                      key={c.level_db}
                      type="button"
                      className="crossing"
                      aria-pressed={selected === i}
                      title={`Zoom to the ${minus(String(c.level_db))} dB crossing`}
                      onClick={() => {
                        setSelected(i);
                        setViews((prev) => ({ ...prev, trans: viewForCrossing(r.crossings, i, frames.trans.limits) }));
                      }}
                    >
                      <span className="lvl">{minus(String(c.level_db))}</span>
                      <span className="hz">{fmtHzStat(c.freq_hz, 3)}</span>
                    </button>
                  ))}
                </div>
              )}
              <ResizeHandle
                className="ups-card-handle"
                label="Resize Nyquist transition plot height"
                value={transHeight.height}
                min={transHeight.minHeight}
                max={transHeight.maxHeight}
                onChange={transHeight.setHeight}
                onCommit={transHeight.commitHeight}
                onStep={transHeight.stepHeight}
                onReset={transHeight.resetHeight}
              />
            </div>

            <div className="ups-sub-group">
              <div className="ups-sub-plots">
                <div className="ups-plot-card">
                  <div className="ups-plot-head">
                    <span className="ups-plot-title">Full-range response</span>
                    <span className="ups-plot-sub">20 Hz – {fmtNyquistKhz(r.output_rate / 2)}</span>
                  </div>
                  <ZoomPlot
                    className="ups-canvas-wrap sm"
                    style={subHeight.height !== null ? { height: subHeight.height } : undefined}
                    ariaLabel={`Full-range log frequency response up to ${fmtInt(r.output_rate / 2)} Hz`}
                    scale="log"
                    view={fullView}
                    home={frames.full.home}
                    limits={frames.full.limits}
                    onView={setView("full")}
                    freqs={fullData.freqs}
                    mags={fullData.mags}
                    mode={mode}
                    boxAxis={boxAxis}
                    pad={{ l: 58, r: 12, t: 12, b: 24 }}
                    decorate={(ctx, g) => drawNyquistMarker(ctx, g, r.source_rate / 2)}
                  />
                </div>

                <div className="ups-plot-card">
                  <div className="ups-plot-head">
                    <span className="ups-plot-title">Impulse response</span>
                    <span className="ups-plot-sub">
                      {r.phase === "linear" ? "symmetric linear phase" : "causal minimum phase"} · {r.impulse.length} samples
                    </span>
                  </div>
                  <PlotCanvas
                    className="ups-canvas-wrap sm"
                    style={subHeight.height !== null ? { height: subHeight.height } : undefined}
                    ariaLabel="Measured upsampler impulse response around peak"
                    draw={(ctx, w, h) => drawImpulse(ctx, w, h, r)}
                  />
                </div>
              </div>
              <ResizeHandle
                className="ups-row-handle"
                label="Resize Full-range and Impulse plots height"
                value={subHeight.height}
                min={subHeight.minHeight}
                max={subHeight.maxHeight}
                onChange={subHeight.setHeight}
                onCommit={subHeight.commitHeight}
                onStep={subHeight.stepHeight}
                onReset={subHeight.resetHeight}
              />
            </div>
          </div>

          <div className="facts" aria-label="Measured filter statistics">
            <div className="fact">
              <div className="k">Output rate</div>
              <div className="v">{ratioOff ? `Off · ${fmtRate(r.output_rate)}` : `×${r.ratio} · ${fmtRate(r.output_rate)}`}</div>
            </div>
            <div className="fact" title="Largest minus smallest level below 90% of the source Nyquist">
              <div className="k">Passband ripple</div>
              <div className="v">{ratioOff ? "0 dB" : fmtRipple(r.passband_ripple_db)}</div>
            </div>
            <div className="fact" title="First frequency where the response falls to −0.1 dB">
              <div className="k">−0.1 dB at</div>
              <div className="v">{ratioOff ? "—" : `${fmtHzStat(r.minus_0_1_db_hz, hzDigits)} Hz`}</div>
            </div>
            <div className="fact" title="First frequency where the response falls to −3 dB">
              <div className="k">−3 dB at</div>
              <div className="v">{ratioOff ? "—" : `${fmtHzStat(r.minus_3_db_hz, hzDigits)} Hz`}</div>
            </div>
            <div className="fact" title="From here to the output Nyquist the response stays at or below the stopband attenuation">
              <div className="k">Stopband from</div>
              <div className="v">{ratioOff ? "—" : `${fmtHzStat(r.stopband_start_hz, 1)} Hz`}</div>
            </div>
            <div className="fact" title="Worst level from the stopband start to the output Nyquist">
              <div className="k">Stopband atten.</div>
              <div className="v">{ratioOff ? "—" : `${r.stopband_atten_db.toFixed(1)} dB`}</div>
            </div>
            <div className="fact" title={`${fmtInt(r.latency_samples)} samples at ${fmtRate(r.output_rate)}`}>
              <div className="k">Latency</div>
              <div className="v">{fmtMs(latMs)}</div>
            </div>
            <div className="fact">
              <div className="k">Taps · phase</div>
              <div className="v">
                {fmtTaps(r.taps)} · {r.phase}
              </div>
            </div>
          </div>

          <p className="ups-caption">
            Measured by feeding a full-scale unit impulse through the engine&apos;s own upsampler filter. Zoomed views and crossings are evaluated directly from that impulse.
          </p>
        </>
      )}
    </section>
  );
}
