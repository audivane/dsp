// The overlay layer of the EQ graph: target curves (dashed), preference bounds
// (a translucent band) and measurements (thin solid lines, plus the predicted
// measurement + current EQ in a stronger stroke). It is a pure painter plus a
// tiny hook, so `EqGraph.tsx` only calls it and everything about overlays
// stays here.

import { DRAW_FREQS, boundsNormalisationDb, displayPoints, predictedPoints, resample, sampleAt, type Overlay, type Point } from "../lib/overlays";
import { store, useApp } from "../store/store";
import { cssVar } from "../theme";
import { hexA } from "./eqColors";

/** The plot area and the graph's own axis mappings (from `EqGraph.tsx`). */
export interface OverlayGeometry {
  w: number;
  h: number;
  /** Left inset (the dB labels). */
  left: number;
  /** Right inset. */
  right: number;
  /** Top inset. */
  top: number;
  /** Bottom inset (the frequency labels). */
  bottom: number;
  xOf(f: number): number;
  yOf(db: number): number;
}

/** Re-render (and so repaint) the graph when an overlay or the reference changes. */
export function useOverlayRedraw(): void {
  const overlays = useApp((s) => s.overlays);
  const ref = useApp((s) => s.overlayRef);
  void overlays;
  void ref;
}

const ORDER: Record<Overlay["kind"], number> = { bounds: 0, target: 1, measurement: 2 };

/** Name plus what the line is, for the right-hand labels. */
function labelFor(o: Overlay, predicted: boolean): string {
  return predicted ? `${o.name} + EQ` : o.name;
}

function shortLabel(text: string): string {
  return text.length > 30 ? text.slice(0, 29) + "…" : text;
}

export function paintOverlays(ctx: CanvasRenderingContext2D, g: OverlayGeometry): void {
  const s = store();
  const state = s.getState();
  const overlays = state.overlays.filter((o) => o.visible);
  if (!overlays.length || g.w <= 0) return;

  const x0 = g.left;
  const x1 = g.w - g.right;
  const y0 = g.top;
  const y1 = g.h - g.bottom;
  if (x1 <= x0 || y1 <= y0) return;

  const ref = state.overlayRef;
  const eq = s.eq();
  const fs = s.graphRate();
  const bg = cssVar("--graph-bg");
  const labels: { text: string; colour: string; y: number }[] = [];

  const trace = (points: readonly Point[], stroke: boolean) => {
    if (points.length < 2) return;
    let started = false;
    for (const [f, db] of points) {
      if (!(f > 0) || !Number.isFinite(db)) continue;
      const x = g.xOf(f);
      const y = g.yOf(db);
      if (!started) {
        ctx.moveTo(x, y);
        started = true;
      } else {
        ctx.lineTo(x, y);
      }
    }
    if (stroke) ctx.stroke();
  };

  const line = (points: readonly Point[], colour: string, width: number, dash: number[], alpha: number) => {
    if (points.length < 2) return;
    ctx.save();
    ctx.globalAlpha = alpha;
    ctx.strokeStyle = colour;
    ctx.lineWidth = width;
    ctx.setLineDash(dash);
    ctx.beginPath();
    trace(points, true);
    ctx.restore();
  };

  const band = (o: Overlay) => {
    const lower = o.data.lower;
    const upper = o.data.upper;
    if (!lower || !upper) return;
    const norm = boundsNormalisationDb(lower, upper, ref) - o.offsetDb;
    const lo = resample(lower, DRAW_FREQS);
    const up = resample(upper, DRAW_FREQS);
    ctx.save();
    ctx.beginPath();
    let started = false;
    for (let i = 0; i < DRAW_FREQS.length; i++) {
      const a = lo[i];
      if (a === null) continue;
      const x = g.xOf(DRAW_FREQS[i]);
      const y = g.yOf(a - norm);
      if (!started) {
        ctx.moveTo(x, y);
        started = true;
      } else {
        ctx.lineTo(x, y);
      }
    }
    for (let i = DRAW_FREQS.length - 1; i >= 0; i--) {
      const b = up[i];
      if (b === null) continue;
      ctx.lineTo(g.xOf(DRAW_FREQS[i]), g.yOf(b - norm));
    }
    if (started) {
      ctx.closePath();
      ctx.fillStyle = hexA(o.colour, 0.16);
      ctx.fill();
    }
    ctx.restore();
    const shift = (pts: readonly Point[]) => pts.map(([f, db]) => [f, db - norm] as Point);
    line(shift(lower), o.colour, 1, [], 0.45);
    line(shift(upper), o.colour, 1, [], 0.45);
    const l = sampleAt(lower, 20000) ?? 0;
    const u = sampleAt(upper, 20000) ?? 0;
    labels.push({ text: o.name, colour: o.colour, y: g.yOf((l + u) / 2 - norm) });
  };

  ctx.save();
  ctx.beginPath();
  ctx.rect(x0, y0, x1 - x0, y1 - y0);
  ctx.clip();
  ctx.lineJoin = "round";
  ctx.lineCap = "round";

  const sorted = [...overlays].sort((a, b) => ORDER[a.kind] - ORDER[b.kind]);
  for (const o of sorted) {
    if (o.kind === "bounds") {
      band(o);
    } else if (o.kind === "target") {
      const pts = displayPoints(o.data.points ?? [], ref, o.offsetDb);
      line(pts, o.colour, 1.6, [7, 4], 0.95);
      const last = pts[pts.length - 1];
      if (last) labels.push({ text: labelFor(o, false), colour: o.colour, y: g.yOf(last[1]) });
    } else {
      const raw = o.data.points ?? [];
      const measured = displayPoints(raw, ref, o.offsetDb);
      if (o.predicted) {
        const predicted = predictedPoints(raw, eq, fs, ref, o.offsetDb);
        const last = predicted[predicted.length - 1];
        line(predicted, o.colour, 2.4, [], 0.95);
        if (last) labels.push({ text: labelFor(o, true), colour: o.colour, y: g.yOf(last[1]) });
      }
      const last = measured[measured.length - 1];
      line(measured, o.colour, 1.25, [], 0.7);
      if (last) labels.push({ text: labelFor(o, false), colour: o.colour, y: g.yOf(last[1]) });
    }
  }
  ctx.restore();

  // Right-hand labels, pushed apart so two curves that meet are both readable.
  labels.sort((a, b) => a.y - b.y);
  let prev = -Infinity;
  const placed = labels.map((l) => {
    const y = Math.max(y0 + 8, Math.min(y1 - 8, Math.max(l.y, prev + 13)));
    prev = y;
    return { ...l, y };
  });
  ctx.save();
  ctx.beginPath();
  ctx.rect(x0, y0, x1 - x0, y1 - y0);
  ctx.clip();
  ctx.font = "11px " + cssVar("--mono");
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  // A chip behind the text, so a label stays readable where it crosses a curve.
  for (const l of placed) {
    const text = shortLabel(l.text);
    const w = ctx.measureText(text).width;
    const x = x1 - 6;
    ctx.globalAlpha = 0.9;
    ctx.fillStyle = bg;
    ctx.fillRect(x - w - 4, l.y - 7.5, w + 8, 15);
    ctx.globalAlpha = 1;
    ctx.fillStyle = l.colour;
    ctx.fillText(text, x, l.y);
  }
  ctx.restore();
}
