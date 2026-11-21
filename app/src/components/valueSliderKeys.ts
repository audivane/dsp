// Keyboard and wheel steps for ValueSlider: one displayed step per arrow key
// or wheel notch, also on a log scale (where the native input's own step
// would be a fraction of it at the top of the range and snap back).

export interface StepRange {
  min: number;
  max: number;
  step: number;
}

// Six decimals for display, rounded toward the inside so a clamped bound
// (e.g. 2.408239965… dB/oct) never rounds up past the limit and gets refused.
function tidy(v: number, min: number, max: number): number {
  const t = Number(v.toFixed(6));
  if (t > max) return Math.floor(max * 1e6) / 1e6;
  if (t < min) return Math.ceil(min * 1e6) / 1e6;
  return t;
}

/**
 * Move `value` by `steps` displayed steps (+up / -down), clamping to `r`.
 * Off-grid values snap to the next grid point in the step's direction.
 */
export function stepValue(steps: number, value: number, r: StepRange): number {
  const { min, max, step } = r;
  if (steps === 0) return value;
  const clamp = (v: number) => Math.max(min, Math.min(max, v));
  const onGrid = Math.abs(value / step - Math.round(value / step)) < 1e-6;
  if (steps > 0) {
    return tidy(clamp((onGrid ? Math.round(value / step) : Math.floor(value / step)) * step + steps * step), min, max);
  }
  return tidy(clamp((onGrid ? Math.round(value / step) : Math.ceil(value / step)) * step + steps * step), min, max);
}

export function keyStepValue(key: string, value: number, r: StepRange): number | null {
  switch (key) {
    case "ArrowRight":
    case "ArrowUp":
      return stepValue(1, value, r);
    case "ArrowLeft":
    case "ArrowDown":
      return stepValue(-1, value, r);
    case "PageUp":
      return stepValue(10, value, r);
    case "PageDown":
      return stepValue(-10, value, r);
    case "Home":
      return tidy(r.min, r.min, r.max);
    case "End":
      return tidy(r.max, r.min, r.max);
    default:
      return null;
  }
}
