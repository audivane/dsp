import { useEffect, useRef, useState } from "react";
import type { Band } from "../api/types";
import { hasSection, sectionEnabled, sectionSet } from "../lib/chain";
import { channelShapeDb, design, isGainless, isIdentity, magSq, sectionShapeDb } from "../lib/eqmath";
import { clamp, fmtDb, fmtFreq, minus } from "../lib/format";
import { store, useApp } from "../store/store";
import { cssVar, useSkin } from "../theme";
import { BandEditor, EDITOR_H, EDITOR_W } from "./BandEditor";
import { bandColor, hexA } from "./eqColors";
import { paintOverlays, useOverlayRedraw } from "./OverlayLayer";
import { paintSpectrum, spectrumAnimating, useSpectrumLayer } from "./SpectrumLayer";

const DBR = 15;
const PL = 42;
/** Right gutter: holds the spectrum's dBFS scale, clear of the legend. */
const PR = 40;
const PT = 14;
const PB = 26;
const F_MIN = 20;
const F_MAX = 20000;
const GRID_F = [20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 10000, 20000];
const LABELS: Record<number, string> = { 20: "20", 50: "50", 100: "100", 200: "200", 500: "500", 1000: "1k", 2000: "2k", 5000: "5k", 10000: "10k", 20000: "20k" };

interface Geo {
  w: number;
  h: number;
}
const xOf = (g: Geo, f: number) => PL + (Math.log10(f / F_MIN) / 3) * (g.w - PL - PR);
const fOf = (g: Geo, x: number) => F_MIN * Math.pow(10, (3 * (x - PL)) / (g.w - PL - PR));
const yOf = (g: Geo, db: number) => PT + ((DBR - db) / (2 * DBR)) * (g.h - PT - PB);
const dbOf = (g: Geo, y: number) => DBR - ((y - PT) / (g.h - PT - PB)) * 2 * DBR;
const handleY = (g: Geo, b: Band) => (isGainless(b.kind) ? yOf(g, 0) : yOf(g, clamp(b.gain_db, -DBR, DBR)));

const roundF = (f: number) => (f < 100 ? Math.round(f * 10) / 10 : Math.round(f));
const roundG = (g: number) => Math.round(g * 10) / 10;
const roundQ = (q: number) => Math.round(q * 100) / 100;

/** Float the editor beside the selected handle, never on top of it. */
function editorPos(g: Geo, b: Band): React.CSSProperties {
  const hx = xOf(g, clamp(b.freq_hz, F_MIN, F_MAX));
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
  const dirty = useRef(true);
  const drag = useRef<{ i: number; pointer: number } | null>(null);
  const wheelTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [size, setSize] = useState<Geo>({ w: 0, h: 0 });
  const skin = useSkin();
  useOverlayRedraw();
  useSpectrumLayer(() => {
    dirty.current = true;
  });
  // Re-render on the inputs the painter reads (also marks the canvas dirty).
  const eqDraft = useApp((s) => s.eqDraft);
  const eqCfg = useApp((s) => s.config?.chain.eq);
  const mode = useApp((s) => s.eqMode);
  const sel = useApp((s) => s.selectedBand);
  const dsp = useApp((s) => s.config?.dsp_enabled ?? true);
  const rate = useApp((s) => s.status?.graph_rate ?? 48000);
  dirty.current = true;
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
      dirty.current = false;
      paint(ctx, geo.current);
      setSize((prev) => (prev.w === r.width && prev.h === r.height ? prev : { w: r.width, h: r.height }));
    };
    const ro = new ResizeObserver(resize);
    ro.observe(cv);
    resize();
    const loop = () => {
      raf = requestAnimationFrame(loop);
      if (!dirty.current && !spectrumAnimating()) return;
      dirty.current = false;
      paint(ctx, geo.current);
    };
    raf = requestAnimationFrame(loop);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

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
    for (const f of GRID_F) {
      const x = Math.round(xOf(g, f)) + 0.5;
      ctx.strokeStyle = f === 100 || f === 1000 || f === 10000 ? gridS : grid;
      ctx.beginPath();
      ctx.moveTo(x, PT);
      ctx.lineTo(x, g.h - PB);
      ctx.stroke();
      if (LABELS[f]) {
        ctx.fillStyle = muted;
        ctx.textAlign = f === 20 ? "left" : f === 20000 ? "right" : "center";
        ctx.fillText(LABELS[f], x, g.h - PB / 2);
      }
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
    for (let x = PL; x <= g.w - PR; x += 1.5) {
      xs.push(x);
      fsAt.push(fOf(g, x));
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
    if (hasSection(eqNow, m) && (m !== "linked" || !same || !secOn)) {
      const secColor = m === "left" ? chL : m === "right" ? chR : same ? accent : fg;
      drawDb(sectionShapeDb(eqNow, m, fs, fsAt), secColor, 1.75, [5, 4], active ? (secOn ? 0.85 : 0.45) : 0.3);
    }

    // handles for the selected section
    set.bands.forEach((b, i) => {
      const x = xOf(g, clamp(b.freq_hz, F_MIN, F_MAX));
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
    });
  }

  // ---------------------------------------------------------------- input

  const local = (e: { clientX: number; clientY: number }) => {
    const r = canvas.current!.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };
  const hit = (x: number, y: number) => {
    let best = -1;
    let bd = 14;
    store()
      .editedSet()
      .bands.forEach((b, i) => {
        const d = Math.hypot(xOf(geo.current, clamp(b.freq_hz, F_MIN, F_MAX)) - x, handleY(geo.current, b) - y);
        if (d < bd) {
          bd = d;
          best = i;
        }
      });
    return best;
  };

  const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!eq) return;
    const p = local(e);
    const i = hit(p.x, p.y);
    if (i < 0) return;
    e.preventDefault();
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
    const patch: Partial<Band> = { freq_hz: roundF(clamp(fOf(geo.current, p.x), F_MIN, F_MAX)) };
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
    st.addBand({ freq_hz: roundF(clamp(fOf(geo.current, p.x), F_MIN, F_MAX)), gain_db: roundG(clamp(dbOf(geo.current, p.y), -DBR, DBR)), q: 1 });
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
      s.updateBand(i, { q: roundQ(clamp(b.q * (e.deltaY < 0 ? 1.08 : 1 / 1.08), 0.1, 20)) }, "drag");
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
    if (e.key === "ArrowLeft") patch = { freq_hz: roundF(clamp(b.freq_hz / Math.pow(2, step / 24), F_MIN, F_MAX)) };
    else if (e.key === "ArrowRight") patch = { freq_hz: roundF(clamp(b.freq_hz * Math.pow(2, step / 24), F_MIN, F_MAX)) };
    else if (e.key === "ArrowUp" && !isGainless(b.kind)) patch = { gain_db: roundG(clamp(b.gain_db + 0.5 * step, -DBR, DBR)) };
    else if (e.key === "ArrowDown" && !isGainless(b.kind)) patch = { gain_db: roundG(clamp(b.gain_db - 0.5 * step, -DBR, DBR)) };
    else if (e.key === "PageUp") patch = { q: roundQ(clamp(b.q * 1.1, 0.1, 20)) };
    else if (e.key === "PageDown") patch = { q: roundQ(clamp(b.q / 1.1, 0.1, 20)) };
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
