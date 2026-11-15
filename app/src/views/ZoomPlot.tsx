// A response plot with box zoom, wheel zoom, pan, double-click reset and a
// crosshair readout. The view range is controlled by the parent so other
// controls (the crossings list) can zoom it and so it can fetch denser data.

import { useEffect, useRef } from "react";
import {
  axisFrac,
  axisValue,
  boxZoom,
  decimalsFor,
  fmtDbPrecise,
  fmtHzPrecise,
  interpAt,
  linearTicks,
  logTicks,
  panBy,
  zoomAt,
  type ViewLimits,
  type ViewRange,
  type XScale,
} from "../lib/plotView";
import { cssVar, useSkin } from "../theme";

export type DragMode = "box" | "pan";
export type BoxAxis = "x" | "xy";

export interface PlotGeom {
  left: number;
  top: number;
  w: number;
  h: number;
  view: ViewRange;
  xOf: (f: number) => number;
  yOf: (db: number) => number;
}

export interface ZoomPlotProps {
  ariaLabel: string;
  className?: string;
  scale: XScale;
  view: ViewRange;
  home: ViewRange;
  limits: ViewLimits;
  onView: (v: ViewRange) => void;
  freqs: readonly number[];
  mags: readonly number[];
  mode: DragMode;
  boxAxis: BoxAxis;
  /** Extra drawing under the curve (markers, bands), clipped to the plot. */
  decorate?: (ctx: CanvasRenderingContext2D, g: PlotGeom) => void;
  /** Extra drawing over the curve, not clipped (labels, chips). */
  annotate?: (ctx: CanvasRenderingContext2D, g: PlotGeom) => void;
  lineWidth?: number;
  pad?: { l: number; r: number; t: number; b: number };
  style?: React.CSSProperties;
}

interface Drag {
  kind: DragMode;
  axis: BoxAxis;
  x0: number;
  y0: number;
  x1: number;
  y1: number;
  start: ViewRange;
  pointer: number;
}

const DEFAULT_PAD = { l: 64, r: 14, t: 22, b: 26 };

export function ZoomPlot(props: ZoomPlotProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const propsRef = useRef(props);
  propsRef.current = props;
  const hover = useRef<{ x: number; y: number } | null>(null);
  const drag = useRef<Drag | null>(null);
  const size = useRef({ w: 0, h: 0 });
  const raf = useRef(0);
  const skin = useSkin();

  const geom = (): PlotGeom => {
    const p = propsRef.current;
    const pad = p.pad ?? DEFAULT_PAD;
    const w = Math.max(0, size.current.w - pad.l - pad.r);
    const h = Math.max(0, size.current.h - pad.t - pad.b);
    const v = p.view;
    return {
      left: pad.l,
      top: pad.t,
      w,
      h,
      view: v,
      xOf: (f) => pad.l + axisFrac(f, v.x0, v.x1, p.scale) * w,
      yOf: (db) => pad.t + (1 - (db - v.y0) / Math.max(1e-300, v.y1 - v.y0)) * h,
    };
  };

  const render = () => {
    const cv = canvasRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const p = propsRef.current;
    const { w: W, h: H } = size.current;
    ctx.clearRect(0, 0, W, H);
    const g = geom();
    if (g.w <= 0 || g.h <= 0) return;
    const v = p.view;
    const mono = cssVar("--mono");
    const muted = cssVar("--muted");
    const grid = cssVar("--grid");
    const gridS = cssVar("--grid-strong");
    const accent = cssVar("--accent");
    const fg = cssVar("--fg");
    const gbg = cssVar("--graph-bg");
    ctx.font = "11px " + mono;
    ctx.textBaseline = "middle";
    ctx.lineWidth = 1;

    // Y grid + labels.
    const yt = linearTicks(v.y0, v.y1, Math.max(3, Math.round(g.h / 38)));
    const yd = decimalsFor(yt.step);
    for (const d of yt.ticks) {
      const y = Math.round(g.yOf(d)) + 0.5;
      ctx.strokeStyle = Math.abs(d) < yt.step * 1e-6 ? gridS : grid;
      ctx.beginPath();
      ctx.moveTo(g.left, y);
      ctx.lineTo(g.left + g.w, y);
      ctx.stroke();
      ctx.fillStyle = muted;
      ctx.textAlign = "right";
      ctx.fillText(fmtTick(d, yd), g.left - 6, y);
    }

    // X grid + labels.
    const spanX = v.x1 - v.x0;
    const xs = p.scale === "log" ? logTicks(v.x0, v.x1) : linearTicks(v.x0, v.x1, Math.max(3, Math.round(g.w / 90))).ticks;
    const xStep = xs.length > 1 ? xs[1] - xs[0] : spanX;
    let lastLabelX = -Infinity;
    for (const f of xs) {
      const x = Math.round(g.xOf(f)) + 0.5;
      if (x < g.left - 0.5 || x > g.left + g.w + 0.5) continue;
      ctx.strokeStyle = grid;
      ctx.beginPath();
      ctx.moveTo(x, g.top);
      ctx.lineTo(x, g.top + g.h);
      ctx.stroke();
      const label = p.scale === "log" && spanX > 4 * v.x0 ? fmtLogTick(f) : fmtHzPrecise(f, xStep * 200 >= 2000 ? spanX : xStep * 200);
      const tw = ctx.measureText(label).width;
      if (x - tw / 2 - lastLabelX >= 8) {
        ctx.fillStyle = muted;
        ctx.textAlign = "center";
        ctx.fillText(label, x, g.top + g.h + 13);
        lastLabelX = x + tw / 2;
      }
    }

    ctx.save();
    ctx.beginPath();
    ctx.rect(g.left, g.top, g.w, g.h);
    ctx.clip();
    p.decorate?.(ctx, g);

    // Curve.
    const n = Math.min(p.freqs.length, p.mags.length);
    if (n >= 2) {
      ctx.strokeStyle = accent;
      ctx.lineWidth = p.lineWidth ?? 2;
      ctx.lineJoin = "round";
      ctx.beginPath();
      let started = false;
      for (let i = 0; i < n; i++) {
        const f = p.freqs[i];
        if (p.scale === "log" && f <= 0) continue;
        // Skip points far outside the view (keep one neighbour each side).
        const next = p.freqs[i + 1];
        const prev = p.freqs[i - 1];
        if (next !== undefined && next < v.x0) continue;
        if (prev !== undefined && prev > v.x1) break;
        const x = g.xOf(f);
        const y = Math.max(g.top - 2000, Math.min(g.top + g.h + 2000, g.yOf(p.mags[i])));
        if (!started) {
          ctx.moveTo(x, y);
          started = true;
        } else {
          ctx.lineTo(x, y);
        }
      }
      if (started) ctx.stroke();
    }

    // Box being dragged.
    const d = drag.current;
    if (d && d.kind === "box") {
      const bx0 = Math.min(d.x0, d.x1);
      const bx1 = Math.max(d.x0, d.x1);
      const by0 = d.axis === "xy" ? Math.min(d.y0, d.y1) : g.top;
      const by1 = d.axis === "xy" ? Math.max(d.y0, d.y1) : g.top + g.h;
      ctx.fillStyle = cssVar("--accent-soft");
      ctx.fillRect(bx0, by0, bx1 - bx0, by1 - by0);
      ctx.strokeStyle = accent;
      ctx.lineWidth = 1;
      ctx.setLineDash([4, 3]);
      ctx.strokeRect(bx0 + 0.5, by0 + 0.5, bx1 - bx0 - 1, by1 - by0 - 1);
      ctx.setLineDash([]);
    }
    ctx.restore();

    p.annotate?.(ctx, g);

    // Crosshair readout at the curve value under the cursor.
    const hv = hover.current;
    if (hv && !d && hv.x >= g.left && hv.x <= g.left + g.w && hv.y >= g.top && hv.y <= g.top + g.h) {
      const f = axisValue((hv.x - g.left) / g.w, v.x0, v.x1, p.scale);
      const db = interpAt(p.freqs, p.mags, f);
      ctx.save();
      ctx.strokeStyle = fg;
      ctx.globalAlpha = 0.35;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(Math.round(hv.x) + 0.5, g.top);
      ctx.lineTo(Math.round(hv.x) + 0.5, g.top + g.h);
      if (db !== null) {
        const y = Math.round(g.yOf(db)) + 0.5;
        if (y >= g.top && y <= g.top + g.h) {
          ctx.moveTo(g.left, y);
          ctx.lineTo(g.left + g.w, y);
        }
      }
      ctx.stroke();
      ctx.globalAlpha = 1;
      if (db !== null) {
        const y = g.yOf(db);
        if (y >= g.top && y <= g.top + g.h) {
          ctx.fillStyle = accent;
          ctx.beginPath();
          ctx.arc(hv.x, y, 3, 0, Math.PI * 2);
          ctx.fill();
        }
      }
      const spanHz = p.scale === "log" ? Math.max(1e-9, f * (Math.pow(v.x1 / v.x0, 1 / 200) - 1) * 200) : spanX;
      const text = `${fmtHzPrecise(f, spanHz)} Hz   ${db === null ? "—" : fmtDbPrecise(db, v.y1 - v.y0) + " dB"}`;
      ctx.font = "500 11.5px " + mono;
      const tw = ctx.measureText(text).width;
      const bx = Math.min(g.left + g.w - tw - 12, Math.max(g.left + 4, hv.x + 10));
      const by = Math.max(g.top + 4, Math.min(g.top + g.h - 22, hv.y - 26));
      ctx.fillStyle = gbg;
      ctx.globalAlpha = 0.92;
      ctx.fillRect(bx, by, tw + 10, 19);
      ctx.globalAlpha = 1;
      ctx.strokeStyle = cssVar("--line");
      ctx.strokeRect(bx + 0.5, by + 0.5, tw + 9, 18);
      ctx.fillStyle = fg;
      ctx.textAlign = "left";
      ctx.textBaseline = "middle";
      ctx.fillText(text, bx + 5, by + 10);
      ctx.restore();
    }
  };

  const schedule = () => {
    if (raf.current) return;
    raf.current = requestAnimationFrame(() => {
      raf.current = 0;
      render();
    });
  };

  // Redraw on every prop change.
  useEffect(() => {
    schedule();
  });

  useEffect(() => {
    const cv = canvasRef.current;
    if (!cv) return;
    const ctx = cv.getContext("2d");
    if (!ctx) return;
    const resize = () => {
      const r = cv.getBoundingClientRect();
      const d = window.devicePixelRatio || 1;
      size.current = { w: r.width, h: r.height };
      cv.width = Math.max(1, Math.round(r.width * d));
      cv.height = Math.max(1, Math.round(r.height * d));
      ctx.setTransform(d, 0, 0, d, 0, 0);
      render();
    };
    const ro = new ResizeObserver(resize);
    ro.observe(cv);
    resize();

    const local = (e: { clientX: number; clientY: number }) => {
      const r = cv.getBoundingClientRect();
      return { x: e.clientX - r.left, y: e.clientY - r.top };
    };
    const fracs = (pt: { x: number; y: number }) => {
      const g = geom();
      return { fx: (pt.x - g.left) / Math.max(1, g.w), fy: 1 - (pt.y - g.top) / Math.max(1, g.h) };
    };
    const onWheel = (e: WheelEvent) => {
      const p = propsRef.current;
      e.preventDefault();
      const delta = e.deltaY !== 0 ? e.deltaY : e.deltaX;
      if (delta === 0) return;
      const unit = e.deltaMode === 1 ? 33 : e.deltaMode === 2 ? 300 : 1;
      const factor = Math.exp(Math.max(-1, Math.min(1, delta * unit * 0.0015)));
      const { fx, fy } = fracs(local(e));
      p.onView(zoomAt(p.view, factor, clamp01(fx), clamp01(fy), e.shiftKey ? "y" : "x", p.limits, p.scale));
    };
    const onDown = (e: PointerEvent) => {
      const p = propsRef.current;
      if (e.button !== 0 && e.button !== 1) return;
      const pt = local(e);
      const g = geom();
      if (pt.x < g.left || pt.x > g.left + g.w || pt.y < g.top || pt.y > g.top + g.h) return;
      e.preventDefault();
      const pan = e.button === 1 || e.ctrlKey || e.metaKey || e.altKey ? p.mode !== "pan" : p.mode === "pan";
      const axis: BoxAxis = e.shiftKey ? (p.boxAxis === "x" ? "xy" : "x") : p.boxAxis;
      drag.current = { kind: pan ? "pan" : "box", axis, x0: pt.x, y0: pt.y, x1: pt.x, y1: pt.y, start: p.view, pointer: e.pointerId };
      cv.setPointerCapture(e.pointerId);
      cv.style.cursor = pan ? "grabbing" : "crosshair";
    };
    const onMove = (e: PointerEvent) => {
      const pt = local(e);
      hover.current = pt;
      const d = drag.current;
      if (d && d.pointer === e.pointerId) {
        d.x1 = pt.x;
        d.y1 = pt.y;
        if (d.kind === "pan") {
          const p = propsRef.current;
          const g = geom();
          p.onView(panBy(d.start, (pt.x - d.x0) / Math.max(1, g.w), -(pt.y - d.y0) / Math.max(1, g.h), p.limits, p.scale));
        }
      }
      schedule();
    };
    const onUp = (e: PointerEvent) => {
      const d = drag.current;
      if (!d || d.pointer !== e.pointerId) return;
      drag.current = null;
      cv.style.cursor = "";
      if (cv.hasPointerCapture(e.pointerId)) cv.releasePointerCapture(e.pointerId);
      if (d.kind === "box" && Math.abs(d.x1 - d.x0) >= 4 && (d.axis === "x" || Math.abs(d.y1 - d.y0) >= 4)) {
        const p = propsRef.current;
        const g = geom();
        const v = p.view;
        const fOf = (x: number) => axisValue(clamp01((x - g.left) / Math.max(1, g.w)), v.x0, v.x1, p.scale);
        const dbOf = (y: number) => v.y0 + clamp01(1 - (y - g.top) / Math.max(1, g.h)) * (v.y1 - v.y0);
        p.onView(boxZoom(v, { x0: fOf(d.x0), x1: fOf(d.x1), y0: dbOf(d.y0), y1: dbOf(d.y1) }, d.axis, p.limits, p.scale));
      }
      schedule();
    };
    const onLeave = () => {
      hover.current = null;
      schedule();
    };
    const onDbl = (e: MouseEvent) => {
      e.preventDefault();
      const p = propsRef.current;
      p.onView(p.home);
    };
    cv.addEventListener("wheel", onWheel, { passive: false });
    cv.addEventListener("pointerdown", onDown);
    cv.addEventListener("pointermove", onMove);
    cv.addEventListener("pointerup", onUp);
    cv.addEventListener("pointercancel", onUp);
    cv.addEventListener("pointerleave", onLeave);
    cv.addEventListener("dblclick", onDbl);
    return () => {
      ro.disconnect();
      cancelAnimationFrame(raf.current);
      raf.current = 0;
      cv.removeEventListener("wheel", onWheel);
      cv.removeEventListener("pointerdown", onDown);
      cv.removeEventListener("pointermove", onMove);
      cv.removeEventListener("pointerup", onUp);
      cv.removeEventListener("pointercancel", onUp);
      cv.removeEventListener("pointerleave", onLeave);
      cv.removeEventListener("dblclick", onDbl);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [skin]);

  return (
    <div className={props.className ?? "ups-canvas-wrap"} style={props.style}>
      <canvas ref={canvasRef} role="img" aria-label={props.ariaLabel} className="zoom-canvas" />
    </div>
  );
}

const clamp01 = (v: number) => Math.max(0, Math.min(1, v));

function fmtTick(v: number, decimals: number): string {
  const s = (Math.abs(v) < 0.5 * Math.pow(10, -decimals) ? 0 : v).toFixed(decimals);
  return s.replace(/^-/, "−");
}

function fmtLogTick(f: number): string {
  if (f >= 1000) {
    const k = f / 1000;
    return `${Number(k.toFixed(1))}k`;
  }
  return String(Math.round(f));
}
