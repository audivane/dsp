// Per-layer explainers in the Harmonics page's layer cards (collapsed by
// default): what the layer's curve does to a sine, how sag and bias move
// over a test signal, a flux layer's soft S and its H3 against frequency,
// and a tape layer's loop (traced by the engine).

import { useEffect, useMemo, useRef, useState } from "react";
import type { ChebyshevLayer, FluxLayer, HarmonicsConfig, HarmonicsLayer, HarmonicsSpectrum, HysteresisLayer } from "../api/types";
import { sagOn } from "../lib/effects";
import { fmtDb, fmtFreq, minus } from "../lib/format";
import { SAG_TEST, SAG_TEST_SECONDS, chebApplied, chebCurve, fluxCurve, fluxDrive, fluxH3Db, harmonicsDb, niceGain, sagTrace } from "../lib/harmonicsCurves";
import { errorText, store } from "../store/store";
import { BAND_COLORS } from "./eqColors";

const fmtHz = (f: number) => (f >= 1000 ? `${fmtFreq(f).slice(0, -1)} kHz` : `${fmtFreq(f)} Hz`);

interface Series {
  xs: readonly number[];
  ys: readonly number[];
  colour: string;
  dash?: boolean;
  width?: number;
  label?: string;
}

interface Axis {
  min: number;
  max: number;
  ticks: readonly number[];
  fmt?: (v: number) => string;
  log?: boolean;
}

const PL = 38;
const PR = 8;
const PT = 8;
const PB = 18;

/** A small SVG line chart (fixed axes, no interaction). */
function Chart({ x, y, series, height = 170, label, square, marks }: { x: Axis; y: Axis; series: Series[]; height?: number; label: string; square?: boolean; marks?: { x: number; y: number; text: string; colour: string; below?: boolean }[] }) {
  const wrap = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(300);
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setW(Math.max(160, Math.round(el.getBoundingClientRect().width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const h = square ? Math.min(Math.max(160, w - PL - PR + PT + PB), 300) : height;
  const pw = square ? h - PT - PB : w - PL - PR;
  const fx = (v: number) => (x.log ? Math.log(v / x.min) / Math.log(x.max / x.min) : (v - x.min) / (x.max - x.min));
  const X = (v: number) => PL + fx(v) * pw;
  const Y = (v: number) => PT + (1 - (Math.max(y.min - 1e3, Math.min(y.max + 1e3, v)) - y.min) / (y.max - y.min)) * (h - PT - PB);
  const path = (s: Series) => s.xs.map((xv, i) => `${i ? "L" : "M"}${X(xv).toFixed(1)},${Y(s.ys[i]).toFixed(1)}`).join("");
  const clip = useMemo(() => `hx-${Math.random().toString(36).slice(2)}`, []);
  const fmt = (a: Axis, v: number) => minus(a.fmt ? a.fmt(v) : String(v));
  return (
    <div className="miniplot" ref={wrap}>
      <svg width={square ? pw + PL + PR : w} height={h} role="img" aria-label={label}>
        <defs>
          <clipPath id={clip}>
            <rect x={PL} y={PT} width={Math.max(0, pw)} height={h - PT - PB} />
          </clipPath>
        </defs>
        <rect x={PL} y={PT} width={Math.max(0, pw)} height={h - PT - PB} className="mp-bg" />
        {y.ticks.map((t) => (
          <g key={"y" + t}>
            <line x1={PL} x2={PL + pw} y1={Y(t)} y2={Y(t)} className={"mp-grid" + (t === 0 ? " strong" : "")} />
            <text x={PL - 5} y={Y(t)} className="mp-text" textAnchor="end" dominantBaseline="middle">
              {fmt(y, t)}
            </text>
          </g>
        ))}
        {x.ticks.map((t, i) => (
          <g key={"x" + t}>
            <line x1={X(t)} x2={X(t)} y1={PT} y2={h - PB} className={"mp-grid" + (t === 0 && !x.log ? " strong" : "")} />
            <text x={X(t)} y={h - 5} className="mp-text" textAnchor={i === x.ticks.length - 1 ? "end" : i === 0 ? "start" : "middle"}>
              {fmt(x, t)}
            </text>
          </g>
        ))}
        <g clipPath={`url(#${clip})`}>
          {series.map((s, i) => (
            <path key={i} d={path(s)} className="mp-line" style={{ stroke: s.colour }} strokeWidth={s.width ?? 1.8} strokeDasharray={s.dash ? "4 3" : undefined} />
          ))}
        </g>
        {marks?.map((m) => (
          <g key={m.text}>
            <circle cx={X(m.x)} cy={Y(m.y)} r={3.5} style={{ fill: m.colour }} />
            <text x={X(m.x) + 6} y={Y(m.y) + (m.below ? 13 : -6)} className="mp-text">
              {m.text}
            </text>
          </g>
        ))}
      </svg>
      {series.some((s) => s.label) && (
        <div className="legend mp-legend">
          {series
            .filter((s) => s.label)
            .map((s) => (
              <span key={s.label}>
                <i style={{ background: s.colour, opacity: s.dash ? 0.6 : 1 }} />
                {s.label}
              </span>
            ))}
        </div>
      )}
    </div>
  );
}

const ACCENT = "var(--accent)";
const MUTED = "var(--muted)";
const UNIT = [-1, -0.5, 0, 0.5, 1];
const unitAxis: Axis = { min: -1, max: 1, ticks: UNIT };
const grid = (n: number, f: (x: number) => number, lo = -1, hi = 1) => {
  const xs = Array.from({ length: n }, (_, i) => lo + ((hi - lo) * i) / (n - 1));
  return { xs, ys: xs.map(f) };
};
const cycle = (n = 256) => Array.from({ length: n }, (_, i) => (2 * Math.PI * i) / (n - 1));

/** The explainer under a layer's editor: collapsed by default, drawn only when open. */
export function LayerExplain({ layer, index, stage }: { layer: HarmonicsLayer; index: number; stage: HarmonicsConfig }) {
  const [open, setOpen] = useState(false);
  return (
    <details className="harm-explain" open={open} onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}>
      <summary>What this layer does to a sine</summary>
      {open && (layer.kind === "chebyshev" ? <ChebyshevExplain layer={layer} /> : layer.kind === "flux" ? <FluxExplain layer={layer} /> : <TapeExplain layer={layer} index={index} stage={stage} />)}
    </details>
  );
}

function BendToggle({ on, set }: { on: boolean; set: (v: boolean) => void }) {
  return (
    <label className="check" title="Draw the curve's bend ten times deeper, so it shows">
      <input type="checkbox" checked={on} onChange={(e) => set(e.target.checked)} /> Bend ×10
    </label>
  );
}

function ChebyshevExplain({ layer }: { layer: ChebyshevLayer }) {
  const [bend, setBend] = useState(false);
  const a = useMemo(() => chebApplied(layer), [layer]);
  const curve = (v: number) => chebCurve(a, v);
  const ex = bend ? 10 : 1;
  const tc = grid(161, (x) => x + ex * (curve(x) - x));
  // Two cycles of a 0 dBFS cosine, the output with its DC removed (the
  // stage's blocker), and the output minus the scaled input.
  const th = cycle(257).map((t) => 2 * t);
  const out = th.map((t) => curve(Math.cos(t)));
  const dc = out.slice(0, -1).reduce((s, v) => s + v, 0) / (out.length - 1);
  const h1 = (2 / (th.length - 1)) * out.slice(0, -1).reduce((s, v, i) => s + (v - dc) * Math.cos(th[i]), 0);
  const added = out.map((v, i) => v - dc - h1 * Math.cos(th[i]));
  const peak = Math.max(...added.map(Math.abs));
  const mag = niceGain(peak, 0.8);
  const hs = harmonicsDb(curve, 1, 5);
  const xs = th.map((t) => t / (2 * Math.PI));
  const dyn = sagOn(layer.sag) || (layer.bias?.depth ?? 0) !== 0;
  return (
    <>
      <p className="stage-note">
        At 0 dBFS: H2 {fmtDb(hs[0], 1)} dB, H3 {fmtDb(hs[1], 1)} dB, H4 {fmtDb(hs[2], 1)} dB (the curve alone, flat in frequency{layer.tilt ? "; tilt left out" : ""}{dyn ? "; sag and bias at rest" : ""}).
      </p>
      <div className="harm-explain-grid">
        <div>
          <div className="row">
            <span className="k">Transfer curve</span>
            <BendToggle on={bend} set={setBend} />
          </div>
          <Chart label="Transfer curve against the straight line" square x={unitAxis} y={unitAxis} series={[{ ...grid(2, (x) => x), colour: MUTED, dash: true, width: 1.2 }, { ...tc, colour: ACCENT }]} />
          <p className="stage-note">Dashed: a perfect amplifier. Solid: this layer, input across, output up{bend ? ", its bend drawn ten times deeper" : ""}.</p>
        </div>
        <div>
          <span className="k">Wave in and out</span>
          <Chart label="Two cycles of a full-scale sine in and out" height={120} x={{ min: 0, max: 2, ticks: [0, 0.5, 1, 1.5, 2] }} y={{ min: -1.2, max: 1.2, ticks: [-1, 0, 1] }} series={[{ xs, ys: th.map(Math.cos), colour: MUTED, dash: true, width: 1.2, label: "input" }, { xs, ys: out.map((v) => v - dc), colour: ACCENT, label: "output" }]} />
          <span className="k">What the curve added {mag > 1 && <span className="tag">magnified ×{mag}</span>}</span>
          <Chart label="The output minus the clean sine" height={100} x={{ min: 0, max: 2, ticks: [0, 0.5, 1, 1.5, 2] }} y={{ min: -1, max: 1, ticks: [-1, 0, 1] }} series={[{ xs, ys: added.map((v) => v * mag), colour: BAND_COLORS[4] }]} />
          <p className="stage-note">The output minus the clean sine: the harmonics on their own.</p>
        </div>
      </div>
      {dyn && <SagExplain layer={layer} />}
    </>
  );
}

function SagExplain({ layer }: { layer: ChebyshevLayer }) {
  const tr = useMemo(() => sagTrace(layer), [layer]);
  const sag = sagOn(layer.sag);
  const bias = (layer.bias?.depth ?? 0) !== 0;
  const lo = Math.min(-24, Math.floor((Math.min(...tr.driveDb) - 2) / 6) * 6);
  const hAll = [...tr.h2, ...tr.h3, ...tr.h2Rest, ...tr.h3Rest].filter((v) => v > -199);
  const hMax = Math.min(0, Math.ceil((Math.max(...hAll, -60) + 5) / 20) * 20);
  const hMin = Math.max(-160, Math.floor((Math.min(...hAll, -60) - 5) / 20) * 20);
  const hTicks: number[] = [];
  for (let v = hMax; v >= hMin; v -= 20) hTicks.push(v);
  const yTicks: number[] = [];
  for (let v = 0; v >= lo; v -= 6) yTicks.push(v);
  const tAxis: Axis = { min: 0, max: SAG_TEST_SECONDS, ticks: [0, 1, 2, 3, 4], fmt: (v) => `${v} s` };
  const maxBias = Math.max(...tr.bias.map(Math.abs));
  return (
    <div className="harm-explain-grid">
      <div>
        <span className="k">{sag ? "Level into the curve (dB)" : "Operating point"}</span>
        {sag ? (
          <Chart label="Input level and the drive after sag over the test signal" height={140} x={tAxis} y={{ min: lo, max: 0, ticks: yTicks }} series={[{ xs: tr.t, ys: tr.inputDb, colour: MUTED, dash: true, width: 1.2, label: "input" }, { xs: tr.t, ys: tr.driveDb, colour: ACCENT, label: "drive after sag" }]} />
        ) : (
          <Chart label="The bias operating point over the test signal" height={140} x={tAxis} y={{ min: -Math.max(0.01, maxBias) * 1.2, max: Math.max(0.01, maxBias) * 1.2, ticks: [0], fmt: () => "0" }} series={[{ xs: tr.t, ys: tr.bias, colour: ACCENT, label: "operating point" }]} />
        )}
        <p className="stage-note">
          Test signal: quiet ({SAG_TEST[0][1]} dBFS), a long loud passage (0 dBFS), quiet, then a short loud hit.{" "}
          {sag && `Sag ${fmtDb(-(layer.sag!.depth_db * layer.amount), 1)} dB at full level, attack ${layer.sag!.attack_ms} ms, release ${layer.sag!.release_ms} ms. `}
          {bias && `Bias moves the operating point to ${(100 * Math.abs(maxBias)).toFixed(1)} % of full scale on loud passages (attack ${layer.bias!.attack_ms} ms, release ${layer.bias!.release_ms} ms).`}
        </p>
      </div>
      <div>
        <span className="k">Harmonics (dB)</span>
        <Chart
          label="H2 and H3 over the test signal, with and without the envelopes"
          height={140}
          x={tAxis}
          y={{ min: hMin, max: hMax, ticks: hTicks }}
          series={[
            { xs: tr.t, ys: tr.h2Rest, colour: BAND_COLORS[0], dash: true, width: 1.2 },
            { xs: tr.t, ys: tr.h3Rest, colour: BAND_COLORS[1], dash: true, width: 1.2 },
            { xs: tr.t, ys: tr.h2, colour: BAND_COLORS[0], label: "H2" },
            { xs: tr.t, ys: tr.h3, colour: BAND_COLORS[1], label: "H3" },
          ]}
        />
        <p className="stage-note">Dashed: without {sag && bias ? "sag and bias" : sag ? "sag" : "bias"}. {sag ? "A short hit ends before a slow attack reacts; loud passages come out a little quieter and cleaner." : "Sustained loud passages shift the even/odd balance; the level stays put."}</p>
      </div>
    </div>
  );
}

const F_AXIS: Axis = { min: 10, max: 20000, ticks: [10, 100, 1000, 10000], log: true, fmt: (v) => (v >= 1000 ? `${v / 1000}k` : String(v)) };
const logFreqs = (n: number) => Array.from({ length: n }, (_, i) => 10 * Math.pow(2000, i / (n - 1)));

function FluxExplain({ layer }: { layer: FluxLayer }) {
  const [bend, setBend] = useState(false);
  const g = useMemo(() => fluxDrive(layer), [layer]);
  const ex = bend ? 10 : 1;
  const at = (f: number) => grid(161, (x) => x + ex * (fluxCurve(layer, g, f, x) - x));
  const fs = logFreqs(64);
  const h0 = fs.map((f) => fluxH3Db(layer, g, f, 1));
  const h18 = fs.map((f) => fluxH3Db(layer, g, f, Math.pow(10, -18 / 20)));
  const top = Math.min(0, Math.ceil((Math.max(...h0) + 5) / 20) * 20);
  const bottom = Math.max(-200, Math.floor((Math.min(...h18) - 5) / 20) * 20);
  const ticks: number[] = [];
  for (let v = top; v >= bottom; v -= 20) ticks.push(v);
  const cal = layer.calibration_hz;
  return (
    <div className="harm-explain-grid">
      <div>
        <div className="row">
          <span className="k">Transfer curve</span>
          <BendToggle on={bend} set={setBend} />
        </div>
        <Chart
          label="The flux layer's soft S-curve at the calibration frequency and above the corner"
          square
          x={unitAxis}
          y={unitAxis}
          series={[
            { ...grid(2, (x) => x), colour: MUTED, dash: true, width: 1.2 },
            { ...at(20000), colour: BAND_COLORS[4], width: 1.2, label: "above the corner" },
            { ...at(cal), colour: ACCENT, label: `at ${fmtHz(cal)}` },
          ]}
        />
        <p className="stage-note">A soft S with no memory: the core saturates symmetrically, so mostly odd orders. Below the corner the bass drives the core harder (the curve bends more); above it the music passes almost straight.</p>
      </div>
      <div>
        <span className="k">H3 against frequency (dB)</span>
        <Chart
          label="H3 against frequency at 0 and −18 dBFS"
          height={170}
          x={F_AXIS}
          y={{ min: bottom, max: top, ticks }}
          series={[
            { xs: fs, ys: h0, colour: ACCENT, label: "0 dBFS" },
            { xs: fs, ys: h18, colour: BAND_COLORS[1], label: minus("-18 dBFS") },
          ]}
          marks={[{ x: cal, y: layer.target_h3_db, text: `${fmtDb(layer.target_h3_db, 0)} dB`, colour: ACCENT }]}
        />
        <p className="stage-note">H3 rises about 12 dB per octave below the corner ({fmtHz(layer.corner_hz)}) until the boost levels off, and 2 dB per dB of level. A sketch from the model in analog form; the readouts are the engine&apos;s.</p>
      </div>
    </div>
  );
}

/** One cycle through the tape layer alone at its calibration frequency, 0 dBFS, from the engine. */
function useLoop(stage: HarmonicsConfig, index: number, freq: number) {
  const st = store();
  const [data, setData] = useState<HarmonicsSpectrum | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seq = useRef(0);
  const key = JSON.stringify([stage.layers[index], stage.dc_block, freq]);
  useEffect(() => {
    const my = ++seq.current;
    const t = setTimeout(() => {
      st.harmonicsSpectrum({ ...stage, enabled: true }, freq, 0, [{ kind: "layer", layer: index }], () => my === seq.current)
        .then((r) => {
          if (my === seq.current) {
            setData(r);
            setError(null);
          }
        })
        .catch((e) => {
          if (my === seq.current) setError(errorText(e));
        });
    }, 250);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return { data, error };
}

function TapeExplain({ layer, index, stage }: { layer: HysteresisLayer; index: number; stage: HarmonicsConfig }) {
  const freq = Math.min(20000, Math.max(20, layer.calibration_hz));
  const { data, error } = useLoop(stage, index, freq);
  const line = data?.lines[0];
  const loop = useMemo(() => {
    if (!line) return null;
    const n = line.wave.length;
    const amp = Math.pow(10, (data?.level_dbfs ?? 0) / 20);
    const xs = Array.from({ length: n + 1 }, (_, i) => amp * Math.cos((2 * Math.PI * i) / n));
    const ys = Array.from({ length: n + 1 }, (_, i) => line.wave[i % n]);
    // The in-phase gain; what is left (the distortion and the lag that opens the loop) is magnified.
    const gc = (2 / n) * ys.slice(0, n).reduce((s, y, i) => s + y * Math.cos((2 * Math.PI * i) / n), 0) / amp;
    const dev = ys.map((y, i) => y - gc * xs[i]);
    const mag = niceGain(Math.max(...dev.map(Math.abs)), 0.5 * amp * Math.abs(gc));
    const yy = ys.map((_, i) => (gc * xs[i] + mag * dev[i]) / (amp * gc));
    const xx = xs.map((x) => x / amp);
    return { xx, yy, mag, down: n / 4, up: (3 * n) / 4 };
  }, [line, data?.level_dbfs]);
  const h3 = line?.peaks.find((p) => p.order === 3);
  const h2 = line?.peaks.find((p) => p.order === 2);
  return (
    <div className="harm-explain-grid">
      <div>
        <span className="k">
          Loop for one cycle {loop && loop.mag > 1 && <span className="tag">bend ×{loop.mag}</span>}
        </span>
        {error && <div className="notice warn">Loop unavailable: {error}</div>}
        {loop ? (
          <Chart
            label="Output against input for one cycle of a full-scale sine through the tape layer"
            square
            x={unitAxis}
            y={{ min: -1.25, max: 1.25, ticks: UNIT }}
            series={[
              { ...grid(2, (x) => x), colour: MUTED, dash: true, width: 1.2 },
              { xs: loop.xx, ys: loop.yy, colour: ACCENT },
            ]}
            marks={[
              { x: loop.xx[loop.down], y: loop.yy[loop.down], text: "coming down", colour: BAND_COLORS[4] },
              { x: loop.xx[loop.up], y: loop.yy[loop.up], text: "going up", colour: BAND_COLORS[1], below: true },
            ]}
          />
        ) : (
          !error && <p className="stage-note">Tracing…</p>
        )}
      </div>
      <div>
        <p className="stage-note">
          A real trace: the engine runs a 0 dBFS {fmtHz(data?.freq_hz ?? freq)} sine (the calibration frequency) through this layer alone at {data ? `${Math.round(data.rate_hz / 1000)} kHz` : "the output rate"}, and this is one cycle of its steady state, output against input. A magnetic core remembers where the signal has been, so the way down and the way up differ: at input 0 the output depends on the direction (the two dots).
        </p>
        <p className="stage-note">
          The straight part (the music) is removed and what is left is magnified{loop && loop.mag > 1 ? ` ×${loop.mag}` : ""} so the loop shows. Width 0 % has no loop; a wider loop means more memory and more low-order distortion.
          {h3 && ` Here: H3 ${fmtDb(h3.level_db, 1)} dB`}
          {h2 && `, H2 ${fmtDb(h2.level_db, 1)} dB`}
          {h3 && "."}
        </p>
      </div>
    </div>
  );
}
