// A small frequency-response plot (log frequency, dB) in SVG for the effect
// views: a few curves, a light grid, theme colours from the tokens.

import { useEffect, useRef, useState } from "react";
import { fmtFreq, minus } from "../lib/format";
import { logSpace } from "../lib/eqmath";

export interface Curve {
  label: string;
  /** dB at each of the plot's frequencies. */
  db: readonly number[];
  colour: string;
  dash?: boolean;
}

export interface MiniPlotProps {
  label: string;
  fMin?: number;
  fMax?: number;
  yMin: number;
  yMax: number;
  yStep: number;
  /** The frequencies the curves are sampled at (from `plotFreqs`). */
  freqs: readonly number[];
  curves: Curve[];
  /** Vertical markers (a cutoff). */
  marks?: { hz: number; label: string }[];
  height?: number;
}

export function plotFreqs(fMin = 10, fMax = 20000, n = 160): number[] {
  return logSpace(fMin, fMax, n);
}

const PL = 38;
const PR = 10;
const PT = 8;
const PB = 20;
const DECADES = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];

export function MiniPlot({ label, fMin = 10, fMax = 20000, yMin, yMax, yStep, freqs, curves, marks = [], height = 150 }: MiniPlotProps) {
  const wrap = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(480);
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setW(Math.max(200, Math.round(el.getBoundingClientRect().width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const h = height;
  const x = (f: number) => PL + (Math.log(f / fMin) / Math.log(fMax / fMin)) * (w - PL - PR);
  const y = (db: number) => PT + ((yMax - Math.max(yMin, Math.min(yMax, db))) / (yMax - yMin)) * (h - PT - PB);
  const ticks: number[] = [];
  for (let v = Math.ceil(yMin / yStep) * yStep; v <= yMax + 1e-9; v += yStep) ticks.push(Number(v.toFixed(6)));
  const path = (db: readonly number[]) => db.map((v, i) => `${i ? "L" : "M"}${x(freqs[i]).toFixed(1)},${y(v).toFixed(1)}`).join("");
  return (
    <div className="miniplot" ref={wrap}>
      <svg width={w} height={h} role="img" aria-label={label}>
        <rect x={PL} y={PT} width={Math.max(0, w - PL - PR)} height={h - PT - PB} className="mp-bg" />
        {ticks.map((t) => (
          <g key={"y" + t}>
            <line x1={PL} x2={w - PR} y1={y(t)} y2={y(t)} className={t === 0 ? "mp-grid strong" : "mp-grid"} />
            <text x={PL - 6} y={y(t)} className="mp-text" textAnchor="end" dominantBaseline="middle">
              {(t > 0 ? "+" : "") + minus(String(t))}
            </text>
          </g>
        ))}
        {DECADES.filter((f) => f >= fMin && f <= fMax).map((f) => (
          <g key={"x" + f}>
            <line x1={x(f)} x2={x(f)} y1={PT} y2={h - PB} className="mp-grid" />
            <text x={x(f)} y={h - 6} className="mp-text" textAnchor="middle">
              {fmtFreq(f)}
            </text>
          </g>
        ))}
        {marks.map((m) => (
          <g key={"m" + m.hz}>
            <line x1={x(m.hz)} x2={x(m.hz)} y1={PT} y2={h - PB} className="mp-mark" />
            <text x={x(m.hz) + 4} y={PT + 10} className="mp-text">
              {m.label}
            </text>
          </g>
        ))}
        {curves.map((c) => (
          <path key={c.label} d={path(c.db)} fill="none" style={{ stroke: c.colour }} strokeWidth={2} strokeDasharray={c.dash ? "5 4" : undefined} strokeLinejoin="round" />
        ))}
      </svg>
      {curves.length > 1 && (
        <div className="legend mp-legend">
          {curves.map((c) => (
            <span key={c.label}>
              <i className={c.dash ? "dash" : undefined} style={{ background: c.dash ? undefined : c.colour, color: c.colour }} />
              {c.label}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
