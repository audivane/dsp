// The live FFT spectrum layer behind the EQ curves, plus its header controls.
// Uses module-level Float64Arrays and EqGraph's requestAnimationFrame loop so
// incoming 15-20 Hz spectrum events never trigger React re-renders.

import { useEffect, useRef } from "react";
import { Seg } from "../components/Seg";
import {
  SPECTRUM_SMOOTHINGS,
  SpectrumAnimator,
  smoothFractionalOctave,
  spectrumCurvePoints,
  spectrumDbToY,
  traceMonotone,
  type SpectrumSmoothing,
  type SpectrumTap,
} from "../lib/spectrum";
import { spectrumStore } from "../store/spectrum";
import { store, useApp } from "../store/store";
import { cssVar } from "../theme";
import type { OverlayGeometry } from "./OverlayLayer";

const SCALE_TICKS = [0, -30, -60, -90];

const anim = new SpectrumAnimator();
let lastRate = -1;
let animating = false;
/** Smoothed targets of the latest frame, recomputed only when it changes. */
let smoothedFor: { at: number; smoothing: number; pre: number[]; post: number[] } | null = null;

function resetBuffers() {
  anim.reset();
  lastRate = -1;
  animating = false;
  smoothedFor = null;
}

/** Re-trigger canvas repaints when spectrum settings or incoming frames change. */
export function useSpectrumLayer(markDirty: () => void): void {
  const dirtyRef = useRef(markDirty);
  dirtyRef.current = markDirty;
  const spec = useApp((s) => s.spectrum);
  useEffect(() => {
    dirtyRef.current();
    if (!spec.enabled) resetBuffers();
  }, [spec.enabled, spec.tap, spec.peakHold, spec.smoothing]);

  useEffect(() => {
    const offFrame = spectrumStore.onFrame(() => {
      animating = true;
      dirtyRef.current();
    });
    const offClear = spectrumStore.onClear(() => {
      resetBuffers();
      dirtyRef.current();
    });
    return () => {
      offFrame();
      offClear();
      resetBuffers();
    };
  }, []);
}

/** True while spectrum smoothing or peak decay is still changing frame-to-frame. */
export function spectrumAnimating(): boolean {
  const st = store().getState();
  return st.spectrum.enabled && animating;
}

function traceSpectrumCurve(
  ctx: CanvasRenderingContext2D,
  bins: Float64Array,
  fMinHz: number,
  binsPerOctave: number,
  g: OverlayGeometry,
  y0: number,
  y1: number,
  plotH: number,
  closeToFloor: boolean,
  topHz?: number,
): boolean {
  // Each trace runs to its own last bin (its tap's Nyquist or 100 kHz),
  // or one bin past the graph's right edge, where the clip cuts it.
  const { xs, ys } = spectrumCurvePoints(bins, fMinHz, binsPerOctave, g.xOf, (db) => spectrumDbToY(db, y0, plotH), g.w - g.right, topHz);
  const n = xs.length;
  if (n < 2) return false;
  traceMonotone(ctx, xs, ys);
  if (closeToFloor) {
    ctx.lineTo(xs[n - 1], y1);
    ctx.lineTo(xs[0], y1);
    ctx.closePath();
  }
  return true;
}

function smoothedTargets(at: number, smoothing: SpectrumSmoothing, bpo: number, pre: readonly number[], post: readonly number[]) {
  if (!smoothedFor || smoothedFor.at !== at || smoothedFor.smoothing !== smoothing) {
    smoothedFor = {
      at,
      smoothing,
      pre: smoothFractionalOctave(pre, bpo, smoothing),
      post: smoothFractionalOctave(post, bpo, smoothing),
    };
  }
  return smoothedFor;
}

export function paintSpectrum(ctx: CanvasRenderingContext2D, g: OverlayGeometry): void {
  const state = store().getState();
  const prefs = state.spectrum;
  if (!prefs.enabled || g.w <= 0) {
    animating = false;
    return;
  }

  const x0 = g.left;
  const x1 = g.w - g.right;
  const y0 = g.top;
  const y1 = g.h - g.bottom;
  const plotH = y1 - y0;
  if (x1 <= x0 || plotH <= 0) return;

  // Discreet dBFS scale for the spectrum layer in the right gutter, outside
  // the plot. Add intermediate ticks when the plot is taller.
  const ticks =
    plotH >= 720
      ? [0, -10, -20, -30, -40, -50, -60, -70, -80, -90]
      : plotH >= 360
        ? [0, -15, -30, -45, -60, -75, -90]
        : SCALE_TICKS;
  ctx.save();
  ctx.font = "10px " + cssVar("--mono");
  ctx.fillStyle = cssVar("--faint");
  ctx.textAlign = "left";
  ctx.textBaseline = "middle";
  for (const db of ticks) {
    const rawY = spectrumDbToY(db, y0, plotH);
    const y = Math.max(y0 + 5, Math.min(y1 - 5, rawY));
    ctx.fillText(db === 0 ? "0" : "−" + String(-db), x1 + 5, y);
  }
  ctx.textBaseline = "top";
  ctx.fillText("dBFS", x1 + 5, y1 + 6);
  ctx.restore();

  const frame = spectrumStore.latest;
  if (!frame) {
    animating = false;
    return;
  }

  const spec = frame.spectrum;
  const sm = smoothedTargets(frame.at, prefs.smoothing, spec.bins_per_octave, spec.pre, spec.post);
  if (spec.pre.length < 2 && spec.post.length < 2) {
    animating = false;
    return;
  }

  if (lastRate !== -1 && spec.rate !== lastRate) {
    resetBuffers();
  }
  lastRate = spec.rate;

  const now = typeof performance !== "undefined" ? performance.now() : Date.now();
  const step = anim.step({
    frameKey: frame.at,
    frameAtMs: frame.at,
    nowMs: now,
    tap: prefs.tap,
    peakHold: prefs.peakHold,
    preTarget: sm.pre,
    postTarget: sm.post,
  });

  if (step.clearFrame) {
    spectrumStore.clear();
    resetBuffers();
    return;
  }
  animating = step.animating;

  ctx.save();
  ctx.beginPath();
  ctx.rect(x0, y0, x1 - x0, plotH);
  ctx.clip();
  ctx.lineJoin = "round";

  const filledBins = prefs.tap === "pre" ? anim.pre : anim.post;
  // Pre runs at the graph rate, whose Nyquist the graph shades from.
  const preTop = spec.rate / 2;
  ctx.beginPath();
  if (traceSpectrumCurve(ctx, filledBins, spec.f_min_hz, spec.bins_per_octave, g, y0, y1, plotH, true, prefs.tap === "pre" ? preTop : undefined)) {
    ctx.fillStyle = cssVar("--spec-fill");
    ctx.fill();
  }

  if (prefs.tap === "both") {
    ctx.beginPath();
    if (traceSpectrumCurve(ctx, anim.pre, spec.f_min_hz, spec.bins_per_octave, g, y0, y1, plotH, false, preTop)) {
      ctx.strokeStyle = cssVar("--spec-pre");
      ctx.lineWidth = 1.25;
      ctx.stroke();
    }
  }

  if (prefs.peakHold && step.peakAlpha > 0) {
    ctx.save();
    ctx.globalAlpha *= step.peakAlpha;
    ctx.beginPath();
    if (traceSpectrumCurve(ctx, anim.peak, spec.f_min_hz, spec.bins_per_octave, g, y0, y1, plotH, false, prefs.tap === "pre" ? preTop : undefined)) {
      ctx.strokeStyle = cssVar("--spec-peak");
      ctx.lineWidth = 1;
      ctx.setLineDash([3, 3]);
      ctx.stroke();
    }
    ctx.restore();
  }

  ctx.restore();
}

export function SpectrumControls() {
  const st = store();
  const spec = useApp((s) => s.spectrum);
  return (
    <div className="spec-ctls" role="group" aria-label="Spectrum analyser">
      <button
        className="tag"
        aria-pressed={spec.enabled}
        title="Show or hide live FFT spectrum behind the EQ curves"
        onClick={() => st.setSpectrumPrefs({ enabled: !spec.enabled })}
      >
        Spectrum
      </button>
      {spec.enabled && (
        <>
          <Seg<SpectrumTap>
            label="Spectrum tap"
            value={spec.tap}
            onChange={(tap) => st.setSpectrumPrefs({ tap })}
            options={[
              ["pre", "Pre", "Input spectrum before EQ"],
              ["post", "Post", "Output spectrum after DSP"],
              ["both", "Both", "Post filled, pre thin line"],
            ]}
          />
          <SmoothingSelect value={spec.smoothing} onChange={(smoothing) => st.setSpectrumPrefs({ smoothing })} />
          <button
            className="tag"
            aria-pressed={spec.peakHold}
            aria-label="Peak hold"
            title="Show decaying spectrum peak-hold trace"
            onClick={() => st.setSpectrumPrefs({ peakHold: !spec.peakHold })}
          >
            Peak
          </button>
        </>
      )}
    </div>
  );
}

/** An analyser's fractional-octave smoothing (1/3 … 1/24 octave). */
export function SmoothingSelect({ value, onChange, finest = 24 }: { value: SpectrumSmoothing; onChange: (v: SpectrumSmoothing) => void; finest?: number }) {
  return (
    <label className="spec-smooth" title="Spectrum smoothing in octaves: each point is the average power over that fraction of an octave. 1/3 is calm, 1/24 shows the most detail.">
      <span className="sr-only">Spectrum smoothing</span>
      <select value={value} aria-label="Spectrum smoothing" onChange={(e) => onChange(Number(e.target.value) as SpectrumSmoothing)}>
        {SPECTRUM_SMOOTHINGS.filter((n) => n <= finest).map((n) => (
          <option key={n} value={n}>
            1/{n}
          </option>
        ))}
      </select>
    </label>
  );
}
