// The flat custom fields of the pre-v2 upsampler, read from a v2 wire
// `UpsamplerConfig` (`preset` + `design`). The app's Custom editor works on
// `FilterDesign` directly (`lib/filterDesign.ts`); this mapping only feeds
// the mock engine's approximate model of a standard-path design (its
// plots, latency and CPU price) and the tests that build such designs.

import type {
  BandSpec,
  Cutoff,
  CutoffMode,
  FilterDesign,
  FilterErrorBudget,
  FilterPhase,
  Length,
  Phase,
  RateTarget,
  TapsUnit,
  UpsamplerConfig,
  Window,
  WindowKind,
} from "../api/types";

export const RECOMMENDED_PRESET = "recommended";

export interface LegacyCustomFields {
  taps: number;
  taps_unit: TapsUnit;
  phase: FilterPhase;
  phase_mix: number;
  kaiser_beta: number;
  cutoff: number;
  cutoff_mode: CutoffMode;
  band: BandSpec;
  auto_taps: FilterErrorBudget | null;
  preserve_samples: boolean;
  window: WindowKind;
  taper: number;
}

export interface LegacyUpsampler extends LegacyCustomFields {
  target: RateTarget;
  preset: string | null;
}

export const DEFAULT_BAND: BandSpec = {
  passband_end: 0.9,
  stopband_start: 1.0,
  attenuation_db: 150,
};

export const DEFAULT_CUSTOM_FIELDS: LegacyCustomFields = {
  taps: 65536,
  taps_unit: "output",
  phase: "linear",
  phase_mix: 0.5,
  kaiser_beta: 14.0,
  cutoff: 1.0,
  cutoff_mode: "fraction",
  band: { ...DEFAULT_BAND },
  auto_taps: null,
  preserve_samples: false,
  window: "kaiser",
  taper: 0.25,
};

export const RECOMMENDED_CUSTOM_FIELDS: LegacyCustomFields = {
  ...DEFAULT_CUSTOM_FIELDS,
  window: "kaiser",
  cutoff_mode: "band",
  band: { ...DEFAULT_BAND },
  auto_taps: "match_output",
  phase: "minimum",
  taps_unit: "output",
  preserve_samples: false,
};

function betaToStopbandDb(beta: number): number {
  return beta / 0.1102 + 8.7;
}

function stopbandDbToBeta(db: number): number {
  return 0.1102 * (db - 8.7);
}

/** Extract legacy UI fields from a wire `UpsamplerConfig`. */
export function fromUpsamplerConfig(u: UpsamplerConfig): LegacyUpsampler {
  const base: LegacyCustomFields =
    u.preset === RECOMMENDED_PRESET || (u.preset === null && u.design === null)
      ? { ...RECOMMENDED_CUSTOM_FIELDS, band: { ...DEFAULT_BAND } }
      : { ...DEFAULT_CUSTOM_FIELDS, band: { ...DEFAULT_BAND } };

  const d = u.design;
  if (d && d.kind === "windowed_sinc") {
    base.preserve_samples = d.preserve_samples;

    // Phase
    if (d.phase.type === "linear") {
      base.phase = "linear";
    } else if (d.phase.type === "minimum") {
      base.phase = "minimum";
    } else {
      base.phase = "intermediate";
      base.phase_mix = d.phase.mix;
    }

    // Window
    if (d.window.type === "kaiser") {
      base.window = "kaiser";
      if (d.window.beta !== null) base.kaiser_beta = d.window.beta;
    } else if (d.window.type === "gaussian") {
      base.window = "gaussian";
      if ("stopband_db" in d.window && d.window.stopband_db !== null) {
        base.kaiser_beta = stopbandDbToBeta(d.window.stopband_db);
      }
    } else if (d.window.type === "tukey") {
      base.window = "tukey";
      base.taper = d.window.taper;
    }

    // Length
    if (d.length.rule === "fixed") {
      base.taps = d.length.taps;
      base.taps_unit = "output";
      base.auto_taps = null;
    } else if (d.length.rule === "per_input") {
      if (typeof d.length.taps === "number") base.taps = d.length.taps;
      base.taps_unit = "input";
      base.auto_taps = null;
    } else if (d.length.rule === "band") {
      base.auto_taps = null;
    } else if (d.length.rule === "auto") {
      base.auto_taps = d.length.budget;
    }

    // Cutoff
    if (d.cutoff.mode === "fraction") {
      base.cutoff_mode = "fraction";
      if (typeof d.cutoff.value === "number") base.cutoff = d.cutoff.value;
    } else if (d.cutoff.mode === "nyquist_stop") {
      base.cutoff_mode = "nyquist_stop";
    } else if (d.cutoff.mode === "band") {
      base.cutoff_mode = "band";
      base.band = {
        passband_end: d.cutoff.passband_end,
        stopband_start: d.cutoff.stopband_start,
        attenuation_db: d.cutoff.attenuation_db,
      };
    }
  }

  return {
    ...base,
    target: u.target,
    preset: u.preset,
  };
}

/** Build a `Standard` `windowed_sinc` `FilterDesign` from legacy UI fields. */
export function legacyToDesign(u: LegacyCustomFields): FilterDesign {
  const searched = u.auto_taps !== null || u.cutoff_mode === "band";
  const beta = searched ? null : u.kaiser_beta;

  let window: Window;
  if (u.window === "gaussian") {
    window = {
      type: "gaussian",
      stopband_db: beta === null ? null : betaToStopbandDb(beta),
      sampling: "symmetric",
    };
  } else if (u.window === "tukey") {
    window = {
      type: "tukey",
      taper: u.taper,
      sampling: "symmetric",
    };
  } else {
    window = {
      type: "kaiser",
      beta,
      sampling: "symmetric",
    };
  }

  const manualLength = (): Length =>
    u.taps_unit === "input"
      ? { rule: "per_input", taps: u.taps }
      : { rule: "fixed", taps: u.taps };

  let length: Length;
  let cutoff: Cutoff;
  if (u.auto_taps !== null) {
    length = { rule: "auto", budget: u.auto_taps };
    cutoff = { mode: "band", ...u.band };
  } else if (u.cutoff_mode === "band") {
    length = { rule: "band" };
    cutoff = { mode: "band", ...u.band };
  } else if (u.cutoff_mode === "nyquist_stop") {
    length = manualLength();
    cutoff = { mode: "nyquist_stop" };
  } else {
    length = manualLength();
    cutoff = { mode: "fraction", value: u.cutoff };
  }

  let phase: Phase;
  if (u.phase === "minimum") {
    phase = { type: "minimum", method: "reflect" };
  } else if (u.phase === "intermediate") {
    phase = { type: "intermediate", mix: u.phase_mix, method: "reflect" };
  } else {
    phase = { type: "linear" };
  }

  return {
    kind: "windowed_sinc",
    window,
    length,
    decimation: { length: "keep" },
    cutoff,
    phase,
    preserve_samples: u.preserve_samples,
    rates: { accept: "any", fallback_window: null },
    numerics: { path: "standard", grid_cutoff: "design_ratio" },
  };
}

/** Convert legacy UI state back to the wire `UpsamplerConfig`. */
export function toUpsamplerConfig(u: LegacyUpsampler): UpsamplerConfig {
  if (u.preset !== null) {
    return {
      target: u.target,
      preset: u.preset,
      design: null,
    };
  }
  return {
    target: u.target,
    preset: null,
    design: legacyToDesign(u),
  };
}

export type UpsamplerPatch = Partial<UpsamplerConfig> & Partial<LegacyCustomFields>;

const CUSTOM_KEYS: readonly (keyof LegacyCustomFields)[] = [
  "taps",
  "taps_unit",
  "phase",
  "phase_mix",
  "kaiser_beta",
  "cutoff",
  "cutoff_mode",
  "band",
  "auto_taps",
  "preserve_samples",
  "window",
  "taper",
];

/** Patch a wire `UpsamplerConfig` using either wire fields or legacy UI fields. */
export function patchUpsamplerConfig(
  cur: UpsamplerConfig,
  patch: UpsamplerPatch,
): UpsamplerConfig {
  const target = patch.target ?? cur.target;
  if (patch.preset !== undefined && patch.preset !== null) {
    return { target, preset: patch.preset, design: null };
  }
  if (patch.design !== undefined && patch.design !== null) {
    return { target, preset: null, design: patch.design };
  }
  const hasCustom = CUSTOM_KEYS.some((k) => patch[k] !== undefined);
  if (hasCustom) {
    const base = fromUpsamplerConfig(cur);
    const next: LegacyUpsampler = {
      ...base,
      ...patch,
      target,
      preset: null,
      band: patch.band ? { ...patch.band } : base.band,
    };
    return toUpsamplerConfig(next);
  }
  if (patch.preset === null) {
    return {
      target,
      preset: null,
      design: cur.design ?? legacyToDesign(fromUpsamplerConfig(cur)),
    };
  }
  return { ...cur, target };
}

/** Default wire `UpsamplerConfig` (off). */
export function defaultUpsamplerConfig(): UpsamplerConfig {
  return {
    target: "off",
    preset: null,
    design: null,
  };
}
