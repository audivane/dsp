import { Power } from "../components/Icons";
import { RECOMMENDED_TARGET } from "../lib/upsamplerRecommended";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { DaemonError, ErrorCode } from "../api/transport";
import type { RateRatio, RateTarget, ResponseWindow, ResponseWindowRequest, UpsamplerConfig, UpsamplerResponse } from "../api/types";
import { ResizeHandle } from "../components/ResizeHandle";
import { Seg } from "../components/Seg";
import { fmtInt, fmtMs, fmtRate, fmtRateShort, minus } from "../lib/format";
import { isResampling, rateRatioText, upsamplingLatencyMs } from "../lib/summary";
import {
  MAX_PLOT_HEIGHT,
  PLOT_HEIGHT_KEYS,
  UPS_IMPULSE_DEFAULT_HEIGHT,
  UPS_IMPULSE_MIN_HEIGHT,
  UPS_TRANS_DEFAULT_HEIGHT,
  UPS_TRANS_MIN_HEIGHT,
  usePersistentHeight,
} from "../lib/plotHeight";
import {
  DEFAULT_MIN_SPAN_X_HZ,
  DEFAULT_MIN_SPAN_Y_DB,
  sameView,
  spliceWindow,
  steepnessText,
  viewForCrossing,
  type ViewLimits,
  type ViewRange,
} from "../lib/plotView";
import { cutoffTips, fmtHzStat, fmtRipple, nextWindowToFetch, responsePlotTop, windowRequestFor } from "../lib/upsamplerPlot";
import { chosenPreviewRate, dacRates, deviceResampleNote, landsLowerNote, isUnmeasuredRatio, viewSourceRate } from "../lib/upsamplerRate";
import { previewRates } from "../lib/rates";
import { buildEnvelope, fmtAmplitude, fmtSampleOffset, impulseFrame, rangeMinMax, type Envelope } from "../lib/impulsePlot";
import { errorText, store, useApp } from "../store/store";
import { cssVar } from "../theme";
import { FilterControls, RateControls, type RateLive } from "./UpsamplingControls";
import { FilterCompare } from "./FilterCompare";
import { ZoomPlot, type BoxAxis, type DragMode, type PlotGeom } from "./ZoomPlot";

function fmtNyquistKhz(hz: number): string {
  const k = hz / 1000;
  return `${Number(k.toFixed(2))} kHz`;
}

/** The impulse, one min/max per pixel column once samples are closer than a pixel. */
function drawImpulseTrace(ctx: CanvasRenderingContext2D, g: PlotGeom, imp: readonly number[], peak: number, env: Envelope) {
  const n = imp.length;
  if (n === 0) return;
  const v = g.view;
  const yOf = (a: number) => Math.max(g.top - 2000, Math.min(g.top + g.h + 2000, g.yOf(a)));
  const spp = (v.x1 - v.x0) / Math.max(1, g.w);
  ctx.beginPath();
  if (n === 1) {
    const x = g.xOf(-peak);
    ctx.moveTo(x, yOf(0));
    ctx.lineTo(x, yOf(imp[0]));
    ctx.stroke();
    return;
  }
  if (spp <= 1) {
    const i0 = Math.max(0, Math.floor(v.x0 + peak) - 1);
    const i1 = Math.min(n - 1, Math.ceil(v.x1 + peak) + 1);
    for (let i = i0; i <= i1; i++) {
      const x = g.xOf(i - peak);
      if (i === i0) ctx.moveTo(x, yOf(imp[i]));
      else ctx.lineTo(x, yOf(imp[i]));
    }
    ctx.stroke();
    // Far enough apart to tell the samples: mark each one.
    if (spp < 1 / 8) {
      for (let i = i0; i <= i1; i++) {
        ctx.beginPath();
        ctx.arc(g.xOf(i - peak), yOf(imp[i]), 2.25, 0, Math.PI * 2);
        ctx.fill();
      }
    }
    return;
  }
  let started = false;
  for (let c = 0; c <= g.w; c++) {
    const a = v.x0 + peak + c * spp;
    const mm = rangeMinMax(env, a, a + spp);
    if (!mm) continue;
    const x = g.left + c + 0.5;
    if (!started) {
      ctx.moveTo(x, yOf(mm.max));
      started = true;
    } else {
      ctx.lineTo(x, yOf(mm.max));
    }
    ctx.lineTo(x, yOf(mm.min));
  }
  if (started) ctx.stroke();
}

type PlotId = "trans" | "imp";
/** The plot that fetches denser zoom windows from the engine. */
type WinId = "trans";
const WINDOW_DEBOUNCE_MS = 220;

/** The band edge: the source Nyquist, or the output's when downsampling. */
function bandEdge(r: UpsamplerResponse): number {
  return Math.min(r.source_rate, r.output_rate) / 2;
}

/** The top of the plots: the Nyquist the engine measures to (the
 *  intermediate grid's at a fractional ratio). */
const plotTop = responsePlotTop;

/** The target that makes the engine design exactly what it runs now. */
function landingTarget(ratio: RateRatio, outputRate: number): RateTarget {
  if (ratio.up === ratio.down) return "off";
  if (ratio.down === 1) return { ratio: ratio.up };
  return { rate: outputRate };
}

/** Home views and limits of the two zoomable plots for a response. */
function plotFrames(r: UpsamplerResponse): Record<PlotId, { home: ViewRange; limits: ViewLimits }> {
  const nyqIn = bandEdge(r);
  const nyqOut = plotTop(r);
  return {
    trans: {
      home: { x0: 0.73 * nyqIn, x1: Math.min(nyqOut, 1.18 * nyqIn), y0: -150, y1: 3 },
      limits: { x0: 0, x1: nyqOut, y0: -220, y1: 20, minSpanX: DEFAULT_MIN_SPAN_X_HZ, minSpanY: DEFAULT_MIN_SPAN_Y_DB },
    },
    imp: impulseFrame(r.impulse, r.impulse_peak),
  };
}

function drawPeakMarker(ctx: CanvasRenderingContext2D, g: PlotGeom) {
  const x = Math.round(g.xOf(0)) + 0.5;
  if (x < g.left || x > g.left + g.w) return;
  ctx.save();
  ctx.strokeStyle = cssVar("--warn");
  ctx.lineWidth = 1;
  ctx.setLineDash([3, 3]);
  ctx.beginPath();
  ctx.moveTo(x, g.top);
  ctx.lineTo(x, g.top + g.h);
  ctx.stroke();
  ctx.restore();
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
  if (!isResampling(r.rate_ratio) || !(r.minus_0_1_db_hz > 0) || !(r.stopband_start_hz > r.minus_0_1_db_hz)) return;
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
  wanted: Record<WinId, ResponseWindowRequest | null>,
  fetchWindow: (w: ResponseWindowRequest) => Promise<UpsamplerResponse>,
): Record<WinId, ResponseWindow | null> {
  const [windows, setWindows] = useState<Record<WinId, { key: string; req: ResponseWindowRequest; win: ResponseWindow } | null>>({ trans: null });
  // A window the engine refused: not asked again until the view or the design changes.
  const [failed, setFailed] = useState<Record<WinId, { key: string; req: ResponseWindowRequest } | null>>({ trans: null });
  const inflight = useRef(false);
  const [tick, setTick] = useState(0);
  const wantedRef = useRef(wanted);
  wantedRef.current = wanted;
  const keyRef = useRef(baseKey);
  keyRef.current = baseKey;

  const same = (a: ResponseWindowRequest | null | undefined, b: ResponseWindowRequest | null | undefined) =>
    !!a && !!b && a.f_lo_hz === b.f_lo_hz && a.f_hi_hz === b.f_hi_hz && a.points === b.points;

  const wantKey = JSON.stringify(wanted.trans);
  useEffect(() => {
    if (!base || !ready || inflight.current) return;
    const pick = nextWindowToFetch(wanted, windows, failed, baseKey);
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
        .catch((e: unknown) => {
          // Superseded by a newer call: try again; refused: wait for a new selection.
          if (e instanceof DaemonError && e.code === ErrorCode.SUPERSEDED) return;
          setFailed((prev) => ({ ...prev, [pick]: { key: baseKey, req } }));
        })
        .finally(() => {
          inflight.current = false;
          setTick((t) => t + 1);
        });
    }, WINDOW_DEBOUNCE_MS);
    return () => clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base, baseKey, ready, wantKey, windows, failed, tick]);

  const valid = (id: WinId) => {
    const w = windows[id];
    return w && w.key === baseKey && wanted[id] !== null ? w.win : null;
  };
  return { trans: valid("trans") };
}

export function UpsamplingView() {
  const st = store();
  const u = useApp((s) => s.config?.chain.upsampler);
  const statusState = useApp((s) => s.status?.state);
  const rawGraphRate = useApp((s) => s.status?.graph_rate);
  const effectiveDefaultRate = useApp((s) => s.status?.effective_default_rate ?? null);
  const statusRateRatio = useApp((s) => s.status?.rate_ratio ?? null);
  const statusOutputRate = useApp((s) => s.status?.output_rate ?? null);
  const deviceCaps = useApp((s) => s.status?.device_caps ?? null);
  const deviceRates = useMemo(() => (deviceCaps ? deviceCaps.rates.filter((c) => c.formats.length > 0).map((c) => c.rate) : null), [deviceCaps]);
  // What the engine can open the DAC at with the configured formats.
  const formats = useApp((s) => s.config?.formats);
  const dac = useMemo(() => dacRates(deviceCaps, formats ?? []), [deviceCaps, formats]);
  const activeSink = useApp((s) => s.status?.active_sink ?? null);
  // A reopen (filter length, ratio...) reports "switching" for a moment: keep
  // the last rate so the readout, plot and measurement do not jump to another
  // rate and back.
  const effectiveRates = useApp((s) => s.status?.effective_rates);
  // The engine's fallback after a DSP overload (the saved setting is kept).
  const overload = useApp((s) => s.status?.overload ?? null);
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

  const [response, setResponse] = useState<UpsamplerResponse | null>(null);
  const [responseKey, setResponseKey] = useState("");
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [retryTick, setRetryTick] = useState(0);
  const reqSeq = useRef(0);

  const [mode, setMode] = useState<DragMode>("box");
  const [boxAxis, setBoxAxis] = useState<BoxAxis>("x");
  const [views, setViews] = useState<Record<PlotId, ViewRange | null>>({ trans: null, imp: null });
  const [selected, setSelected] = useState<number | null>(null);
  const transHeight = usePersistentHeight({
    storageKey: PLOT_HEIGHT_KEYS.upsTrans,
    defaultHeight: UPS_TRANS_DEFAULT_HEIGHT,
    minHeight: UPS_TRANS_MIN_HEIGHT,
    maxHeight: MAX_PLOT_HEIGHT,
  });
  const impHeight = usePersistentHeight({
    storageKey: PLOT_HEIGHT_KEYS.upsImpulse,
    defaultHeight: UPS_IMPULSE_DEFAULT_HEIGHT,
    minHeight: UPS_IMPULSE_MIN_HEIGHT,
    maxHeight: MAX_PLOT_HEIGHT,
  });

  const pipelines = useApp((s) => (s.config?.ab ? 4 : 2));
  // At the rate playing now, measure what the engine runs: the landing in its
  // status (it may have capped the ratio to what the DAC plays). Any other
  // rate, or a stopped engine, shows a preview of the target instead.
  const engineLive = (statusState === "playing" || statusState === "ready") && activeSink !== "direct";
  // With upsampling off the landing is 1:1, and that is what plays too.
  // During an overload fallback the engine runs a lighter setting than this
  // one: the plots then show the saved setting, labelled as such, not the
  // landing of what plays.
  // Upsampling off: the plots preview the filter at the Recommended rate.
  const offPreview = u?.target === "off";
  const offTarget = useApp((s) => s.upsOffTarget);
  const landing = u && !offPreview && engineLive && !overload && rawGraphRate === sourceRate && statusRateRatio && statusOutputRate ? landingTarget(statusRateRatio, statusOutputRate) : null;
  const measuredU = u && landing ? { ...u, target: landing } : u && offPreview ? { ...u, target: offTarget ?? RECOMMENDED_TARGET } : (u ?? null);
  const uKey = measuredU ? JSON.stringify(measuredU) : "";
  const key = `${uKey}@${sourceRate}`;
  const measuredRef = useRef(measuredU);
  measuredRef.current = measuredU;
  useEffect(() => {
    const mu = measuredRef.current;
    if (!mu) return;
    const seq = ++reqSeq.current;
    setLoading(true);
    setErr(null);
    const timer = setTimeout(() => {
      void st
        .upsamplerResponse(mu, sourceRate)
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
    setViews({ trans: null, imp: null });
    setSelected(null);
  }, [frameKey]);

  const frames = useMemo(() => (response ? plotFrames(response) : null), [response]);
  const viewOf = (id: PlotId) => views[id] ?? frames?.[id].home ?? { x0: 0, x1: 1, y0: -150, y1: 0 };
  const transView = viewOf("trans");
  const impView = viewOf("imp");
  const envelope = useMemo(() => (response ? buildEnvelope(response.impulse) : null), [response]);

  const zoomed = (id: PlotId) => !!frames && views[id] !== null && !sameView(views[id]!, frames[id].home);
  const wanted = useMemo(
    () => ({
      trans: response && frames && zoomed("trans") ? windowRequestFor(transView, "lin", plotTop(response)) : null,
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [response, frames, views],
  );
  const fetchWindow = useCallback((w: ResponseWindowRequest) => st.upsamplerResponse(measuredRef.current, sourceRate, w), [st, uKey, sourceRate]);
  const windows = useResponseWindows(response, responseKey, !loading && responseKey === key, wanted, fetchWindow);

  if (!u) return null;
  const set = (p: Partial<UpsamplerConfig>, ok?: string) => st.setUpsampler(p, ok);
  const off = u.target === "off";
  const r = response;
  const ratioOff = r ? !isResampling(r.rate_ratio) : off;
  const current = r !== null && responseKey === key;
  // The plots show the filter the engine runs, or only a preview of the
  // target (the saved setting while an overload fallback runs instead).
  const liveShown = current && landing !== null;
  const savedShown = overload !== null;
  const savedWhy = "not what plays now: after a DSP overload the engine runs a lighter setting until you change this one (see the notice above)";
  const previewWhy = !engineLive
    ? "the engine is not running the DSP path now"
    : rawGraphRate && rawGraphRate !== sourceRate
      ? `the engine plays ${fmtRate(rawGraphRate)} now`
      : "the engine has not reported what it runs yet";
  const latMs = r ? upsamplingLatencyMs(measuredU ?? u, r.source_rate, r, r.plan ?? null) : null;
  const tips = cutoffTips(sourceRate, r && r.source_rate === sourceRate ? r : null, u.design?.kind === "windowed_sinc" ? u.design.cutoff.mode : undefined);
  const knownSourceRate = chosenRate ?? (statusState === "playing" && rawGraphRate && rawGraphRate > 0 ? rawGraphRate : recentPlayingRate);
  const readoutSourceRate = knownSourceRate ?? playingRate;
  const notes: string[] = [];
  // A selected target the DAC cannot take: one note with the reason (the
  // format) and where the engine lands, instead of the bare cap note.
  const lower = off ? null : landsLowerNote(u.target, dac, engineLive && statusOutputRate ? statusOutputRate : null);
  const devNote =
    !off && engineLive && rawGraphRate && statusRateRatio
      ? deviceResampleNote({ graphRate: rawGraphRate, outputRate: statusOutputRate ?? rawGraphRate, ratio: statusRateRatio, target: u.target, deviceRates })
      : null;
  if (devNote) notes.push(devNote);
  const live: RateLive = {
    live: !off && engineLive && !!rawGraphRate,
    graphRate: rawGraphRate ?? 0,
    outputRate: statusOutputRate ?? 0,
    ratio: statusRateRatio,
    dac,
    lower,
  };
  const steep = r ? steepnessText(r.crossings) : null;
  const setView = (id: PlotId) => (v: ViewRange) => setViews((prev) => ({ ...prev, [id]: v }));
  const transData = r ? spliceWindow(r.freqs_hz, r.magnitude_db, windows.trans) : { freqs: [], mags: [] };
  const hzDigits = r && r.crossings.length > 1 && r.crossings[r.crossings.length - 1].freq_hz - r.crossings[0].freq_hz < 200 ? 3 : 1;
  const edge = r ? bandEdge(r) : sourceRate / 2;
  const resetAll = () => {
    setViews({ trans: null, imp: null });
    setSelected(null);
  };

  return (
    <section className="view" aria-labelledby="ups-title">
      <div className="view-head">
        <button className="pwr stage-pwr" aria-pressed={!off} aria-label="Upsampling on" title={off ? "Turn upsampling on" : "Turn upsampling off (the settings are kept)"} onClick={() => st.setUpsamplingEnabled(off)}>
          <Power />
        </button>
        <h1 id="ups-title">Upsampling</h1>
        <span className={"tag" + (off ? "" : " on")}>{off ? "Off" : "On"}</span>
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
            Measuring…
          </span>
        )}
        <span className="spacer" />
        {u && <FilterCompare u={u} />}
      </div>

      {overload && (
        <div
          className="notice warn ups-overload"
          role="status"
          title="The engine measured more work than real time allows and runs a lighter setting until you change the upsampler. The settings below are what you chose; they are saved as they are."
        >
          <span>{overload.message}</span>
        </div>
      )}

      <div className="panel">
        <RateControls u={u} set={set} live={live} notes={notes} allowedRates={allowedRates ?? []} pipelines={pipelines} />
        <div className="ups-sep" />
        <FilterControls
          u={u}
          set={set}
          allowedRates={allowedRates ?? []}
          measured={current ? r : null}
          tips={tips}
          readoutRate={readoutSourceRate}
          readoutAssumed={knownSourceRate === null}
          pipelines={pipelines}
        />
      </div>

      {err && !isUnmeasuredRatio(err) && (
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
                <span
                  className={"ups-badge" + (liveShown ? " live" : "")}
                  title={
                    liveShown
                      ? "Measured from what the engine runs now, as its status reports it"
                      : savedShown
                        ? `Saved setting: what this setting designs for a ${fmtRate(r.source_rate)} source; ${savedWhy}`
                        : `Target preview: what this setting designs for a ${fmtRate(r.source_rate)} source; ${previewWhy}`
                  }
                >
                  {liveShown ? "Playing now" : savedShown ? "Saved setting" : "Target preview"}
                </span>
                <span className="ups-plot-sub">
                  {ratioOff
                    ? `Upsampling is off (1:1 bypass at ${fmtRate(r.source_rate)})`
                    : `${fmtRate(r.source_rate)} → ${fmtRate(r.output_rate)} (${rateRatioText(r.rate_ratio)})`}
                </span>
                <div className="spacer" />
                <div
                  className="plot-tools"
                  title="Drag: zoom to a box (Shift swaps X / X+Y). Ctrl, Alt or middle-drag: pan. Wheel: zoom across (Shift: up and down). Double-click: reset."
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
                      ["x", "X", "Box zoom changes the horizontal axis only"],
                      ["xy", "X+Y", "Box zoom changes both axes"],
                    ]}
                  />
                  <button type="button" className="btn small" disabled={!zoomed("trans") && !zoomed("imp")} onClick={resetAll} title="Reset both plots (or double-click a plot)">
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
                  drawNyquistMarker(ctx, g, edge);
                  drawCrossingDots(ctx, g, r, selected);
                }}
                annotate={(ctx, g) => {
                  const nyq = edge;
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
                  drawChip(ctx, isResampling(r.rate_ratio) ? `Flat to ${fmtHzStat(r.minus_0_1_db_hz, hzDigits)} Hz (−0.1 dB)` : "Flat 1:1 (upsampling off)", g.left + 10, g.top + 13, cssVar("--accent"));
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

            <div className="ups-plot-card resizable">
              <div className="ups-plot-head">
                <span className="ups-plot-title">Impulse response</span>
                <span className="ups-plot-sub">
                  {r.phase === "linear" ? "symmetric linear phase" : r.phase === "intermediate" ? "intermediate phase" : "causal minimum phase"} · {fmtInt(r.impulse.length)} samples
                </span>
              </div>
              <ZoomPlot
                ariaLabel={`Measured upsampler impulse response, ${fmtSampleOffset(Math.round(impView.x0))} to ${fmtSampleOffset(Math.round(impView.x1))} samples from the peak`}
                style={impHeight.height !== null ? { height: impHeight.height } : undefined}
                scale="lin"
                view={impView}
                home={frames.imp.home}
                limits={frames.imp.limits}
                onView={setView("imp")}
                freqs={[]}
                mags={[]}
                mode={mode}
                boxAxis={boxAxis}
                lineWidth={1.75}
                xMinStep={1}
                xTick={(v, step) => fmtSampleOffset(v, step)}
                decorate={drawPeakMarker}
                drawCurve={(ctx, g) => envelope && drawImpulseTrace(ctx, g, r.impulse, r.impulse_peak, envelope)}
                readout={(x, g) => {
                  const n = r.impulse.length;
                  const i = Math.max(0, Math.min(n - 1, Math.round(x + r.impulse_peak)));
                  const off = i - r.impulse_peak;
                  const a = n > 0 ? r.impulse[i] : null;
                  return { x: off, y: a, text: `${fmtSampleOffset(off)} samples   ${a === null ? "—" : fmtAmplitude(a, g.view.y1 - g.view.y0)}` };
                }}
              />
              <ResizeHandle
                className="ups-card-handle"
                label="Resize Impulse response plot height"
                value={impHeight.height}
                min={impHeight.minHeight}
                max={impHeight.maxHeight}
                onChange={impHeight.setHeight}
                onCommit={impHeight.commitHeight}
                onStep={impHeight.stepHeight}
                onReset={impHeight.resetHeight}
              />
            </div>
          </div>

          <div className="facts" aria-label="Measured filter statistics">
            <div className="fact">
              <div className="k">Output rate</div>
              <div className="v">{ratioOff ? `Off · ${fmtRate(r.output_rate)}` : `${rateRatioText(r.rate_ratio)} · ${fmtRate(r.output_rate)}`}</div>
            </div>
            <div className="fact" title="Largest minus smallest level below 90% of the source Nyquist">
              <div className="k">Passband ripple</div>
              <div className="v">{ratioOff ? "0 dB" : fmtRipple(r.passband_ripple_db)}</div>
            </div>
            <div className="fact" title="From here to the output Nyquist the response stays at or below the stopband attenuation">
              <div className="k">Stopband from</div>
              <div className="v">{ratioOff ? "—" : `${fmtHzStat(r.stopband_start_hz, 1)} Hz`}</div>
            </div>
            <div className="fact" title="Worst level from the stopband start to the output Nyquist">
              <div className="k">Stopband atten.</div>
              <div className="v">{ratioOff ? "—" : `${r.stopband_atten_db.toFixed(1)} dB`}</div>
            </div>
            <div className="fact" title={`${liveShown ? "Measured" : savedShown ? "Saved setting" : "Target preview"}: ${fmtInt(r.latency_samples)} samples at ${fmtRate(r.output_rate)}`}>
              <div className="k">Latency</div>
              <div className="v">{latMs === null ? "—" : fmtMs(latMs)}</div>
            </div>
          </div>

          <p className="ups-caption">Measured by feeding a full-scale unit impulse through the engine&apos;s own upsampler filter.</p>
        </>
      )}
      {err && isUnmeasuredRatio(err) && (
        <p className="ups-unmeasured" role="status">
          {err}
        </p>
      )}
    </section>
  );
}
