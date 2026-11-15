// One-line summaries for the sidebar and facts, from daemon state.

import type { ConvolutionConfig, EngineConfig, RateTarget, UpsamplerConfig } from "../api/types";
import type { IrInfo } from "../api/transport";
import { convList } from "./chain";
import { fmtSampleFormat, fmtTaps } from "./format";

export function ratioText(t: RateTarget): string {
  if (t === "off") return "Off";
  if ("ratio" in t) return `×${t.ratio}`;
  return `up to ${t.max_rate / 1000}k`;
}

/** Integer ratio for a source rate (same rule as `resolve_ratio` in the protocol crate). */
export function resolveRatio(t: RateTarget, rate: number): number {
  if (t === "off") return 1;
  if ("ratio" in t) return Math.max(1, t.ratio);
  let r = 1;
  while (rate * r * 2 <= t.max_rate) r *= 2;
  return r;
}

export function upsamplingSummary(u: UpsamplerConfig): string {
  if (u.target === "off") return "Off";
  const cut = u.cutoff_mode === "nyquist_stop" ? " · stop at Nyquist" : u.cutoff < 1 ? " · apodizing" : "";
  return `${ratioText(u.target)} · ${fmtTaps(u.taps)} taps · ${u.phase === "linear" ? "linear" : "minimum"}${cut}`;
}

/** Added latency of the upsampling filter at a source rate, ms (group delay of a linear-phase FIR). */
export function upsamplingLatencyMs(u: UpsamplerConfig, rate: number): number {
  const ratio = resolveRatio(u.target, rate);
  if (ratio <= 1) return 0;
  if (u.phase === "minimum") return 0;
  return ((u.taps - 1) / 2 / (rate * ratio)) * 1000;
}

export const DITHER_TEXT: Record<string, string> = { none: "no dither", tpdf: "TPDF dither", shaped: "noise-shaped dither" };

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
