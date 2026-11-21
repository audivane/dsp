// One-line summaries for the sidebar and facts, from daemon state.

import type { ConvolutionConfig, DesignPlan, EngineConfig, FilterPreset, RateRatio, RateTarget, UpsamplerConfig } from "../api/types";
import type { IrInfo } from "../api/transport";
import { convList } from "./chain";
import { presetSummary } from "./filterPresets";
import { RECOMMENDED_PRESET, designSummary, planLatencyMs } from "./filterDesign";
import { RECOMMENDED_LABEL, isRecommendedUpsampler } from "./upsamplerRecommended";
import { fmtRate, fmtSampleFormat, fmtTaps } from "./format";

/** A running ratio (`rate_ratio` in status and responses) as a number. */
export function rateRatioValue(r: RateRatio): number {
  return r.up / Math.max(1, r.down);
}

/** "×4" for integer ratios, "160/147" otherwise. */
export function rateRatioText(r: RateRatio): string {
  return r.down === 1 ? `×${r.up}` : `${r.up}/${r.down}`;
}

/** Whether the ratio changes the rate at all (not ×1). */
export function isResampling(r: RateRatio): boolean {
  return r.up !== r.down;
}

/**
 * The note shown when the device runs a smaller ratio than the target
 * wants at `graphRate` (it cannot take source × ratio), or null. `running`
 * is `Status.rate_ratio`: `Status.ratio` is 0 while a fractional ratio runs.
 */
export function ratioCapNote(graphRate: number, wantedRatio: number, running: RateRatio): string | null {
  const now = rateRatioValue(running);
  if (!(now < wantedRatio * (1 - 1e-9))) return null;
  const top = fmtRate(Math.round(graphRate * now));
  return isResampling(running)
    ? `This device plays up to ${top} from a ${fmtRate(graphRate)} source: it runs ${rateRatioText(running)}.`
    : `This device plays up to ${top} from a ${fmtRate(graphRate)} source, so nothing is upsampled.`;
}

export function ratioText(t: RateTarget): string {
  if (t === "off") return "Off";
  if ("ratio" in t) return `×${t.ratio}`;
  if ("rate" in t) return `to ${t.rate / 1000}k`;
  if ("max_rate_any" in t) return `best up to ${t.max_rate_any / 1000}k`;
  return `up to ${t.max_rate / 1000}k`;
}

/** Output/input rate ratio for a source rate (same rule as `resolve_rate_ratio`
 * in the protocol crate; an explicit rate can give a fraction). */
export function resolveRatio(t: RateTarget, rate: number): number {
  if (t === "off") return 1;
  if ("ratio" in t) return Math.max(1, t.ratio);
  if ("rate" in t) return t.rate / rate;
  const cap = "max_rate" in t ? t.max_rate : t.max_rate_any;
  let r = 1;
  while (rate * r * 2 <= cap) r *= 2;
  return r;
}

/** The rate part of the Upsampling summary: "×8", "44.1→48k", "up to 768k". */
function rateSummary(t: RateTarget, running: { graphRate: number; outputRate: number; ratio: RateRatio } | null): string {
  if (running && isResampling(running.ratio) && running.graphRate > 0) {
    if (running.ratio.down === 1) return `×${running.ratio.up}`;
    return `${running.graphRate / 1000}→${running.outputRate / 1000}k`;
  }
  return ratioText(t);
}

/**
 * Sidebar line for the Upsampling stage: "Flat-top xlong · linear · ×8" for
 * a preset, "Custom Kaiser 64k · minimum · 44.1→48k" for a custom design. `running`
 * is what the engine runs now (status), shown instead of the target when
 * known.
 */
export function upsamplingSummary(u: UpsamplerConfig, running: { graphRate: number; outputRate: number; ratio: RateRatio } | null = null): string {
  if (u.target === "off") return "Off";
  const rate = rateSummary(u.target, running);
  if (isRecommendedUpsampler(u)) return `${RECOMMENDED_LABEL} · ${rate}`;
  if (u.preset === RECOMMENDED_PRESET || (u.preset === null && u.design === null)) return `${RECOMMENDED_LABEL} · ${rate}`;
  if (u.preset) return `${presetSummary(u.preset as FilterPreset)} · ${rate}`;
  return `Custom ${designSummary(u.design!)} · ${rate}`;
}

/**
 * Added latency of the upsampling filter at a source rate, ms. The engine's
 * measurement (`upsampler_response` for that rate) when there is one, else
 * what the design's plan at that rate tells (`check_design`: half the
 * kernel for a linear-phase design of known length). Null when neither can
 * tell (a preset, minimum or intermediate phase before it is measured).
 */
export function upsamplingLatencyMs(
  u: UpsamplerConfig,
  rate: number,
  measured: { source_rate: number; output_rate: number; latency_samples: number } | null = null,
  plan: DesignPlan | null = null,
): number | null {
  if (measured && measured.source_rate === rate && measured.output_rate > 0) return (measured.latency_samples / measured.output_rate) * 1000;
  const ratio = resolveRatio(u.target, rate);
  if (ratio === 1) return 0;
  if (u.preset || !u.design || !plan) return null;
  return planLatencyMs(u.design, plan, rate);
}

export const DITHER_TEXT: Record<string, string> = {
  none: "no dither",
  tpdf: "TPDF dither",
  shaped: "noise-shaped dither",
  headroom_tpdf: "TPDF + headroom",
  rectangular: "rectangular dither",
  gaussian: "Gaussian dither",
  highpass_tpdf: "high-pass TPDF",
  feedback1: "gentle shaping",
  feedback5: "broad shaping",
  resonant5: "resonant 5 shaping",
  resonant9: "resonant 9 shaping",
  integrator15: "deep 15 shaping",
};

export function outputSummary(cfg: EngineConfig, negotiated: string | null | undefined): string {
  const fmt = fmtSampleFormat(negotiated ?? cfg.formats[0]);
  return `${fmt} · ${DITHER_TEXT[cfg.chain.output.dither] ?? cfg.chain.output.dither}`;
}

/** Taps of one IR, if known. */
export function irTaps(c: ConvolutionConfig, info: Record<string, IrInfo | null>): number | null {
  return info[c.ir_path]?.frames ?? null;
}

/** Length of the single combined filter the engine builds from the enabled IRs. */
export function combinedTaps(list: ConvolutionConfig[], info: Record<string, IrInfo | null>): number | null {
  const on = list.filter((c) => c.enabled);
  if (on.length === 0) return 0;
  let sum = 0;
  for (const c of on) {
    const t = irTaps(c, info);
    if (t === null) return null;
    sum += t;
  }
  return sum - (on.length - 1);
}

export function convSummary(cfg: EngineConfig, info: Record<string, IrInfo | null>): string {
  const list = convList(cfg.chain);
  if (list.length === 0) return "No filters";
  const on = list.filter((c) => c.enabled).length;
  if (on === 0) return `Off · ${list.length} ${list.length === 1 ? "filter" : "filters"}`;
  const t = combinedTaps(list, info);
  return `${on} of ${list.length} ${list.length === 1 ? "filter" : "filters"}${t ? ` · ${fmtTaps(t)} taps` : ""}`;
}
