import { useEffect, useRef, useState } from "react";
import type { Band } from "../api/types";
import { hasSection, sectionEnabled, sectionSet } from "../lib/chain";
import { channelShapeDb, design, isGainless, isIdentity, magSq, sectionShapeDb } from "../lib/eqmath";
import { tiltStageDb, tiltStageOn } from "../lib/tilt";
import { clamp, fmtDb, fmtFreq, minus } from "../lib/format";
import { store, useApp } from "../store/store";
import { cssVar, useSkin } from "../theme";
import { BAND_F_MAX, BAND_F_MIN, BandEditor, EDITOR_H, EDITOR_W, Q_MAX, Q_MIN } from "./BandEditor";
import { xPresetSpan } from "../lib/graphPrefs";
import { bandColor, hexA } from "./eqColors";
import { paintOverlays, useOverlayRedraw } from "./OverlayLayer";
import { paintSpectrum, spectrumAnimating, useSpectrumLayer } from "./SpectrumLayer";

const DBR = 15;
const PL = 42;
/** Right gutter: holds the spectrum's dBFS scale, clear of the legend. */
const PR = 40;
const PT = 14;
const PB = 26;

/** Grid lines (1–9 per decade) and labels (1, 2, 5) over [lo, hi] Hz. */
export function eqGrid(lo: number, hi: number): { f: number; label: string | null }[] {
  const out: { f: number; label: string | null }[] = [];
  for (let e = Math.floor(Math.log10(lo)); Math.pow(10, e) <= hi; e++)
    for (let m = 1; m <= 9; m++) {
      const f = m * Math.pow(10, e);
      if (f < lo * 0.999 || f > hi * 1.001) continue;
      const label = m === 1 || m === 2 || m === 5 || f === lo || f === hi ? (f >= 1000 ? `${f / 1000}k` : String(f)) : null;
      out.push({ f, label });
    }
  return out;
}

interface Geo {
  w: number;
  h: number;
}
/** The graph's frequency span (the range choice), read at paint and input time. */
const span = () => {
  const st = store().getState();
  return xPresetSpan(st.eqRange, (st.status?.graph_rate || 48000) / 2);
};
const xOf = (g: Geo, f: number) => {
  const { lo, hi } = span();
  return PL + (Math.log(f / lo) / Math.log(hi / lo)) * (g.w - PL - PR);
};
const fOf = (g: Geo, x: number) => {
  const { lo, hi } = span();
  return lo * Math.pow(hi / lo, (x - PL) / (g.w - PL - PR));
};
/** Where a band frequency can go: what the editor takes, inside the graph's span. */
const fLimits = () => {
  const { lo, hi } = span();
  return [Math.max(lo, BAND_F_MIN), Math.min(hi, BAND_F_MAX)] as const;
};
const clampF = (f: number) => {
  const [a, b] = fLimits();
  return clamp(f, a, b);
};
const yOf = (g: Geo, db: number) => PT + ((DBR - db) / (2 * DBR)) * (g.h - PT - PB);
const dbOf = (g: Geo, y: number) => DBR - ((y - PT) / (g.h - PT - PB)) * 2 * DBR;
const handleY = (g: Geo, b: Band) => (isGainless(b.kind) ? yOf(g, 0) : yOf(g, clamp(b.gain_db, -DBR, DBR)));

/** Handle radius (the selected one is drawn at 9) and how far off a press still catches the nearest. */
const HANDLE_R = 9;
const HIT_R = 14;

/** Handles in paint order: by index, the selected one last (on top). */
export function paintOrder(n: number, selected: number): number[] {
  const order = Array.from({ length: n }, (_, i) => i).filter((i) => i !== selected);
  if (selected >= 0 && selected < n) order.push(selected);
  return order;
}

/**
 * The band a press at (x, y) takes: the topmost handle drawn under the
 * pointer, as the user sees it (stacked bands, such as several added at
 * the same spot, give the one on top); else the nearest within reach.
 */
export function pickHandle(pts: readonly { x: number; y: number }[], x: number, y: number, selected: number): number {
  const order = paintOrder(pts.length, selected);
  let best = -1;
  let bd = HIT_R;
  for (let k = order.length - 1; k >= 0; k--) {
    const i = order[k];
    const d = Math.hypot(pts[i].x - x, pts[i].y - y);
    if (d <= HANDLE_R) return i;
    if (d < bd) {
      bd = d;
      best = i;
    }
  }
  return best;
}

const roundF = (f: number) => (f < 100 ? Math.round(f * 10) / 10 : Math.round(f));
const roundG = (g: number) => Math.round(g * 10) / 10;
const roundQ = (q: number) => Math.round(q * 100) / 100;

/** Float the editor beside the selected handle, never on top of it. */
function editorPos(g: Geo, b: Band): React.CSSProperties {
  const hx = xOf(g, clampF(b.freq_hz));
  const hy = handleY(g, b);
  const w = Math.min(EDITOR_W, g.w - 16);
  const left = clamp(hx - w / 2, 8, Math.max(8, g.w - w - 8));
  const below = hy + 20;
  const above = hy - 20 - EDITOR_H;
  const top = hy < g.h / 2 ? (below + EDITOR_H <= g.h - PB ? below : above) : above >= PT ? above : below;
  return { left, top: clamp(top, 4, Math.max(4, g.h - EDITOR_H - 4)), width: w };
}

export function EqGraph() {
  const canvas = useRef<HTMLCanvasElement>(null);
  const geo = useRef<Geo>({ w: 0, h: 0 });
  // Asks for one repaint; the frame loop only runs while there is something
  // new to draw (a store change, a spectrum frame, its decay), then sleeps.
  const kick = useRef<() => void>(() => {});
  const drag = useRef<{ i: number; pointer: number } | null>(null);
  const wheelTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [size, setSize] = useState<Geo>({ w: 0, h: 0 });
  const skin = useSkin();
  useOverlayRedraw();
  useSpectrumLayer(() => kick.current());
  // Re-render on the inputs the painter reads; every render repaints once.
  const eqDraft = useApp((s) => s.eqDraft);
  const eqCfg = useApp((s) => s.config?.chain.eq);
  useApp((s) => s.eqOff);
  const mode = useApp((s) => s.eqMode);
  const sel = useApp((s) => s.selectedBand);
  const dsp = useApp((s) => s.config?.dsp_enabled ?? true);
  const rate = useApp((s) => s.status?.graph_rate ?? 48000);
  useApp((s) => s.eqRange);
  void eqDraft;
  void eqCfg;
  void skin;

  const st = store();
  const eq = st.eq();
  const eqOn = !!eqCfg || !!eqDraft;
  const edited = sectionSet(eq, mode);

  useEffect(() => {
    const cv = canvas.current!;
    const ctx = cv.getContext("2d")!;
    let raf = 0;
    const resize = () => {
      const r = cv.getBoundingClientRect();
      const d = window.devicePixelRatio || 1;
      geo.current = { w: r.width, h: r.height };
      cv.width = Math.max(1, Math.round(r.width * d));
      cv.height = Math.max(1, Math.round(r.height * d));
      ctx.setTransform(d, 0, 0, d, 0, 0);
      paint(ctx, geo.current);
      setSize((prev) => (prev.w === r.width && prev.h === r.height ? prev : { w: r.width, h: r.height }));
    };
    const ro = new ResizeObserver(resize);
    ro.observe(cv);
    resize();
    const loop = () => {
      paint(ctx, geo.current);
      raf = spectrumAnimating() ? requestAnimationFrame(loop) : 0;
    };
    kick.current = () => {
      if (!raf) raf = requestAnimationFrame(loop);
    };
    return () => {
      cancelAnimationFrame(raf);
      raf = 0;
      kick.current = () => {};
      ro.disconnect();
    };
  }, []);
  useEffect(() => kick.current());

  function paint(ctx: CanvasRenderingContext2D, g: Geo) {
    if (!g.w || !g.h) return;
    const s = store();
    const state = s.getState();
    const eqNow = s.eq();
    const m = state.eqMode;
    const fs = state.status?.graph_rate || 48000;
    const active = !!eqNow && (state.config?.dsp_enabled ?? true);
    const muted = cssVar("--muted");
    const grid = cssVar("--grid");
    const gridS = cssVar("--grid-strong");
    const accent = cssVar("--accent");
    const chL = cssVar("--ch-l");
    const chR = cssVar("--ch-r");
    const fg = cssVar("--fg");
    const gbg = cssVar("--graph-bg");
    const mono = cssVar("--mono");
    const font = cssVar("--font");
    ctx.clearRect(0, 0, g.w, g.h);
    ctx.font = "11px " + mono;
    ctx.textBaseline = "middle";
    ctx.lineWidth = 1;
    const { lo, hi } = span();
    const nyq = fs / 2;
    for (const { f, label } of eqGrid(lo, hi)) {
      const x = Math.round(xOf(g, f)) + 0.5;
      ctx.strokeStyle = f === 100 || f === 1000 || f === 10000 ? gridS : grid;
      ctx.beginPath();
      ctx.moveTo(x, PT);
      ctx.lineTo(x, g.h - PB);
      ctx.stroke();
      if (label) {
        ctx.fillStyle = muted;
        ctx.textAlign = f === lo ? "left" : f === hi ? "right" : "center";
        ctx.fillText(label, x, g.h - PB / 2);
      }
    }
    // Above the output's Nyquist: shaded, no response.
    const nx = nyq < hi ? xOf(g, nyq) : g.w - PR;
    if (nyq < hi) {
      ctx.save();
      ctx.fillStyle = muted;
      ctx.globalAlpha = 0.1;
      ctx.fillRect(nx, PT, g.w - PR - nx, g.h - PT - PB);
      ctx.globalAlpha = 1;
      if (g.w - PR - nx >= 90) {
        ctx.textAlign = "center";
        ctx.fillText("above Nyquist", (nx + g.w - PR) / 2, PT + 12);
      }
      ctx.restore();
    }
    const plotH = g.h - PT - PB;
    const dbStep = plotH >= 780 ? 1 : plotH >= 360 ? 2.5 : 5;
    const steps = Math.round((2 * DBR) / dbStep);
    for (let k = 0; k <= steps; k++) {
      const d = Number((-DBR + k * dbStep).toFixed(2));
      const y = Math.round(yOf(g, d)) + 0.5;
      ctx.strokeStyle = d === 0 ? gridS : grid;
      ctx.beginPath();
      ctx.moveTo(PL, y);
      ctx.lineTo(g.w - PR, y);
      ctx.stroke();
      ctx.fillStyle = muted;
      ctx.textAlign = "right";
      ctx.fillText((d > 0 ? "+" : "") + minus(String(d)), PL - 8, y);
    }
    const og = { w: g.w, h: g.h, left: PL, right: PR, top: PT, bottom: PB, xOf: (f: number) => xOf(g, f), yOf: (db: number) => yOf(g, db) };
    paintSpectrum(ctx, og);
    paintOverlays(ctx, og);
    if (!eqNow) return;

    const set = sectionSet(eqNow, m);
    const secOn = sectionEnabled(eqNow, m);
    const secActive = active && hasSection(eqNow, m) && secOn;
    const xs: number[] = [];
    const fsAt: number[] = [];
    for (let x = PL; x <= nx; x += 1.5) {
      xs.push(x);
      fsAt.push(fOf(g, x));
    }
    if (xs.length && xs[xs.length - 1] < nx - 0.01) {
      xs.push(nx);
      fsAt.push(Math.min(fOf(g, nx), nyq * 0.9999));
    }
    const ks = set.bands.map((b) => design(b, fs));
    const fillAlpha = secActive ? 1 : 0.35;

    // per-band tinted fills for the selected section
    const y0 = yOf(g, 0);
    set.bands.forEach((b, i) => {
      if (!b.enabled || isIdentity(ks[i])) return;
      ctx.save();
      ctx.globalAlpha = fillAlpha;
      ctx.beginPath();
      ctx.moveTo(PL, y0);
      for (let j = 0; j < xs.length; j++) {
        const db = 10 * Math.log10(magSq(ks[i], fsAt[j], fs));
        ctx.lineTo(xs[j], clamp(yOf(g, db), PT, g.h - PB));
      }
      ctx.lineTo(xs[xs.length - 1], y0);
      ctx.closePath();
      ctx.fillStyle = hexA(bandColor(i), i === state.selectedBand ? 0.26 : 0.12);
      ctx.fill();
      ctx.restore();
    });

    const drawDb = (vals: readonly number[], color: string, width: number, dash: number[], a: number) => {
      ctx.save();
      ctx.globalAlpha = a;
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.setLineDash(dash);
      ctx.lineJoin = "round";
      ctx.beginPath();
      for (let j = 0; j < xs.length; j++) {
        const y = clamp(yOf(g, vals[j]), PT - 4, g.h - PB + 4);
        if (j === 0) ctx.moveTo(xs[j], y);
        else ctx.lineTo(xs[j], y);
      }
      ctx.stroke();
      ctx.restore();
    };
    // Curves show the filter shape (global preamp is a flat offset, shown in the header).
    const lDb = channelShapeDb(eqNow, 0, fs, fsAt);
    const rDb = channelShapeDb(eqNow, 1, fs, fsAt);
    const same = lDb.every((v, j) => Math.abs(v - rDb[j]) < 1e-6);
    const curveAlpha = active ? 1 : 0.4;
    if (same) {
      drawDb(lDb, accent, 2.5, [], curveAlpha);
    } else {
      drawDb(lDb, chL, 2.25, [], curveAlpha);
      drawDb(rDb, chR, 2.25, [], curveAlpha);
    }
    // The tilt on its own, thin and dotted, under the combined curve that includes it.
    if (tiltStageOn(eqNow)) drawDb(tiltStageDb(eqNow, fs, fsAt), muted, 1.25, [2, 3], active ? 0.8 : 0.35);
    if (hasSection(eqNow, m) && (m !== "linked" || !same || !secOn)) {
      const secColor = m === "left" ? chL : m === "right" ? chR : same ? accent : fg;
      drawDb(sectionShapeDb(eqNow, m, fs, fsAt), secColor, 1.75, [5, 4], active ? (secOn ? 0.85 : 0.45) : 0.3);
    }

    // handles for the selected section, the selected one on top
    for (const i of paintOrder(set.bands.length, state.selectedBand)) {
      const b = set.bands[i];
      const x = xOf(g, clampF(b.freq_hz));
      const y = handleY(g, b);
      const col = bandColor(i);
      const selected = i === state.selectedBand;
      ctx.save();
      ctx.globalAlpha = secActive ? 1 : 0.45;
      if (selected) {
        ctx.beginPath();
        ctx.arc(x, y, 13, 0, 7);
        ctx.fillStyle = hexA(col, 0.22);
        ctx.fill();
      }
      ctx.beginPath();
      ctx.arc(x, y, selected ? 9 : 8, 0, 7);
      ctx.fillStyle = b.enabled ? col : gbg;
      ctx.fill();
      ctx.lineWidth = 2;
      ctx.strokeStyle = b.enabled ? (selected ? fg : col) : col;
      ctx.stroke();
      ctx.fillStyle = b.enabled ? "#0b1020" : col;
      ctx.font = "600 10px " + font;
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText(String(i + 1), x, y + 0.5);
      ctx.restore();
    }
  }

  // ---------------------------------------------------------------- input

  const local = (e: { clientX: number; clientY: number }) => {
    const r = canvas.current!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };
  const hit = (x: number, y: number) => {
    const s = store();
    const pts = s.editedSet().bands.map((b) => ({ x: xOf(geo.current, clampF(b.freq_hz)), y: handleY(geo.current, b) }));
    return pickHandle(pts, x, y, s.getState().selectedBand);
  };

  const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!eq) return;
    const p = local(e);
    const i = hit(p.x, p.y);
    if (i < 0) return;
    e.preventDefault();
    // A selection left by an earlier double-click would follow the drag.
    window.getSelection()?.removeAllRanges();
    canvas.current!.focus();
    st.selectBand(i);
    st.beginEqDrag();
    drag.current = { i, pointer: e.pointerId };
    canvas.current!.setPointerCapture(e.pointerId);
  };
  const onPointerMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const p = local(e);
    const cv = canvas.current!;
    if (!drag.current) {
      cv.style.cursor = eq && hit(p.x, p.y) >= 0 ? "grab" : "crosshair";
      return;
    }
    cv.style.cursor = "grabbing";
    const b = st.editedSet().bands[drag.current.i];
    if (!b) return;
    const patch: Partial<Band> = { freq_hz: roundF(clampF(fOf(geo.current, p.x))) };
    if (!isGainless(b.kind)) patch.gain_db = roundG(clamp(dbOf(geo.current, p.y), -DBR, DBR));
    st.updateBand(drag.current.i, patch, "drag");
  };
  const endDrag = () => {
    if (!drag.current) return;
    drag.current = null;
    st.endEqDrag();
  };
  const onDoubleClick = (e: React.MouseEvent<HTMLCanvasElement>) => {
    if (!eq) return;
    const p = local(e);
    if (hit(p.x, p.y) >= 0) return;
    st.addBand({ freq_hz: roundF(clampF(fOf(geo.current, p.x))), gain_db: roundG(clamp(dbOf(geo.current, p.y), -DBR, DBR)), q: 1 });
  };

  useEffect(() => {
    const cv = canvas.current!;
    const onWheel = (e: WheelEvent) => {
      const s = store();
      if (!s.eq()) return;
      const p = local(e);
      let i = hit(p.x, p.y);
      if (i < 0) i = s.getState().selectedBand;
      const b = s.editedSet().bands[i];
      if (!b) return;
      e.preventDefault();
      if (!wheelTimer.current) s.beginEqDrag();
      else clearTimeout(wheelTimer.current);
      wheelTimer.current = setTimeout(() => {
        wheelTimer.current = null;
        store().endEqDrag();
      }, 400);
      s.selectBand(i);
      s.updateBand(i, { q: roundQ(clamp(b.q * (e.deltaY < 0 ? 1.08 : 1 / 1.08), Q_MIN, Q_MAX)) }, "drag");
    };
    cv.addEventListener("wheel", onWheel, { passive: false });
    return () => cv.removeEventListener("wheel", onWheel);
  }, []);

  const onKeyDown = (e: React.KeyboardEvent<HTMLCanvasElement>) => {
    const bands = st.editedSet().bands;
    const i = st.getState().selectedBand;
    const b = bands[i];
    if (!b) return;
    const step = e.shiftKey ? 4 : 1;
    let patch: Partial<Band> | null = null;
    if (e.key === "ArrowLeft") patch = { freq_hz: roundF(clamp(b.freq_hz / Math.pow(2, step / 24), BAND_F_MIN, BAND_F_MAX)) };
    else if (e.key === "ArrowRight") patch = { freq_hz: roundF(clamp(b.freq_hz * Math.pow(2, step / 24), BAND_F_MIN, BAND_F_MAX)) };
    else if (e.key === "ArrowUp" && !isGainless(b.kind)) patch = { gain_db: roundG(clamp(b.gain_db + 0.5 * step, -DBR, DBR)) };
    else if (e.key === "ArrowDown" && !isGainless(b.kind)) patch = { gain_db: roundG(clamp(b.gain_db - 0.5 * step, -DBR, DBR)) };
    else if (e.key === "PageUp") patch = { q: roundQ(clamp(b.q * 1.1, Q_MIN, Q_MAX)) };
    else if (e.key === "PageDown") patch = { q: roundQ(clamp(b.q / 1.1, Q_MIN, Q_MAX)) };
    else if (e.key === "Tab" && !e.shiftKey && i < bands.length - 1) {
      e.preventDefault();
      st.selectBand(i + 1);
      return;
    } else if (e.key === "Tab" && e.shiftKey && i > 0) {
      e.preventDefault();
      st.selectBand(i - 1);
      return;
    }
    if (patch) {
      e.preventDefault();
      st.updateBand(i, patch);
    }
  };

  const selBand = edited.bands[sel];
  const label = selBand
    ? `EQ frequency response. Band ${sel + 1}: ${fmtFreq(selBand.freq_hz)} Hz, ${fmtDb(selBand.gain_db)} dB, Q ${selBand.q.toFixed(2)}. Arrow keys move it, Page Up and Page Down change Q.`
    : "EQ frequency response. Double-click to add a band.";

  return (
    <>
      <canvas
        ref={canvas}
        tabIndex={0}
        role="img"
        aria-label={label}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={endDrag}
        onPointerCancel={endDrag}
        onLostPointerCapture={endDrag}
        onDoubleClick={onDoubleClick}
        onMouseDown={(e) => e.detail > 1 && e.preventDefault()}
        onDragStart={(e) => e.preventDefault()}
        onKeyDown={onKeyDown}
        data-rate={rate}
      />
      {selBand && eq && size.w > 0 && (
        <BandEditor
          band={selBand}
          index={sel}
          channelNote={mode === "linked" ? "" : mode === "left" ? " · L" : " · R"}
          style={editorPos(size, selBand)}
        />
      )}
      {!eqOn && <div className="off-banner">EQ is off</div>}
      {eqOn && !dsp && <div className="off-banner">Bypass is on. The EQ is not applied.</div>}
    </>
  );
}
