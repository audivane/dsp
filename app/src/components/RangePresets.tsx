// The range presets every frequency graph shares (the live spectrum, the
// distortion spectrum, the PEQ graph): the same labels in the same order.
// The level presets apply to the dB spectra only.

import type { ReactNode } from "react";
import { X_PRESETS, Y_PRESETS, type XPreset, type YPreset } from "../lib/graphPrefs";
import { Seg } from "./Seg";

export const X_PRESET_LABEL: Record<XPreset, string> = {
  audio: "20 Hz–20 kHz",
  "50k": "20 Hz–50 kHz",
  "100k": "10 Hz–100 kHz",
  nyquist: "To Nyquist",
};

const X_PRESET_TITLE: Record<XPreset, string> = {
  audio: "The audio band",
  "50k": "Past the audio band to 50 kHz (shaded above Nyquist)",
  "100k": "10 Hz to 100 kHz, 1 kHz in the middle (shaded above Nyquist)",
  nyquist: "Up to half the rate: everything the graph has",
};

/**
 * The presets as segmented buttons. `x`/`y` light the preset the graph
 * shows (null: none, a zoom of its own). The level presets appear with
 * `onY`; `yLabel`/`yTitle` name them in the graph's unit. `children` go at
 * the end (a scale toggle).
 */
export function RangePresets({
  x,
  onX,
  xTitle,
  y,
  onY,
  yLabel,
  yTitle,
  className,
  children,
}: {
  x: XPreset | null;
  onX: (p: XPreset) => void;
  xTitle?: Partial<Record<XPreset, string>>;
  y?: YPreset | null;
  onY?: (p: YPreset) => void;
  yLabel?: (p: YPreset) => string;
  yTitle?: (p: YPreset) => string;
  className?: string;
  children?: ReactNode;
}) {
  const yText = (p: YPreset) => yLabel?.(p) ?? (p === "fit" ? "Fit" : `−${p}`);
  return (
    <div className={"range-presets" + (className ? " " + className : "")} role="group" aria-label="Graph range">
      {onY && (
        <Seg<YPreset>
          label="Level range"
          value={y ?? null}
          onChange={onY}
          options={Y_PRESETS.map((p) => [p, yText(p), yTitle?.(p) ?? (p === "fit" ? "Fitted to what the graph shows" : `Down to ${yText(p)}`)])}
        />
      )}
      <Seg<XPreset> label="Frequency range" value={x} onChange={onX} options={X_PRESETS.map((p) => [p, X_PRESET_LABEL[p], xTitle?.[p] ?? X_PRESET_TITLE[p]])} />
      {children}
    </div>
  );
}
