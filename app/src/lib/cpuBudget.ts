// The mock engine's CPU admission check: the daemon's cost model
// (`control::cpu_budget`) for custom designs, on a fixed "machine" that does
// 5 G multiply-adds per second per core, 7.5 G sustained in real time at the
// upsampler's share (`polyphasePlan.ts` prices the engine's schedule).
// Presets are light and always admitted. The real engine measures its own CPU
// and sends the figure; the app only shows it.
//
// As the daemon: a change is admitted when it fits the source rate playing now
// (else the last one played, else every allowed rate); the other rates over
// budget fall back per rate (`rateAdmissions`, `status.upsampler_rates`).

import type { EngineConfig, RateAdmission, UpsamplerConfig } from "../api/types";
import { fromUpsamplerConfig } from "./upsamplerAdapter";
import { resolveRateRatio } from "./upsamplerRate";
import { chainB } from "./abPatch";
import { polyphaseSeconds } from "./polyphasePlan";

/** Share of one core the upsampler may take (the mock's `status.limits.cpu_share`). */
export const RT_BUDGET = 0.9;
/** The mock machine: in-cache dot multiply-adds per second per channel. */
const DOT_RATE = 5.0e9;
const CHANNELS = 2;

/** The chain block at `rate`: `scaled_quantum` (a power of two), 512 ..= 8192. */
function chainBlock(quantum: number, rate: number): number {
  const raw = Math.min(8192, Math.max(32, Math.floor((quantum * rate) / 48000)));
  return Math.max(512, 2 ** Math.floor(Math.log2(raw)));
}

/** The FFT polyphase engine's input block for the chain block at `rate`. */
function engineBlock(quantum: number, rate: number): number {
  return Math.min(1024, chainBlock(quantum, rate));
}

/** Fraction of one core the custom upsampler `u` takes at `rate` (0 for presets and unity). */
export function upsamplerLoad(u: UpsamplerConfig, rate: number, quantum: number): number {
  if (u.preset || !u.design || u.target === "off") return 0;
  // The mock prices the standard path's designs only (the others are light).
  if (u.design.kind !== "windowed_sinc" || u.design.numerics.path !== "standard") return 0;
  const leg = fromUpsamplerConfig(u);
  const r = resolveRateRatio(u.target, rate);
  if (r.up === r.down) return 0;
  const out = (rate * r.up) / r.down;
  if (r.down === 1) {
    const taps = leg.taps_unit === "input" ? leg.taps * r.up : leg.taps;
    return CHANNELS * polyphaseSeconds(taps, r.up, engineBlock(quantum, rate), chainBlock(quantum, rate), rate);
  }
  const grid = leg.taps * (leg.taps_unit === "input" ? r.up : r.down);
  return (CHANNELS * out * Math.ceil(grid / r.up)) / DOT_RATE;
}

/** "768 kHz", "705.6 kHz". */
function khz(rate: number): string {
  const k = rate / 1000;
  return (Number.isInteger(k) ? String(k) : String(Math.round(k * 1000) / 1000)) + " kHz";
}

function upsamplerPart(c: EngineConfig): string {
  return JSON.stringify([c.dsp_enabled, c.chain.upsampler, c.chain.output, c.ab ? chainB(c.ab, c.chain).upsampler : null, c.rate.allowed_rates, c.quantum, c.device]);
}

interface Landing {
  rate: number;
  out: number;
  load: number;
  loadB: number;
}

/** `cfg` priced at each allowed source rate (chain A and B together). */
function landings(cfg: EngineConfig): Landing[] {
  return cfg.rate.allowed_rates.map((rate) => {
    const loadA = upsamplerLoad(cfg.chain.upsampler, rate, cfg.quantum);
    const loadB = cfg.ab ? upsamplerLoad(chainB(cfg.ab, cfg.chain).upsampler, rate, cfg.quantum) : 0;
    const r = resolveRateRatio(cfg.chain.upsampler.target, rate);
    return { rate, out: (rate * r.up) / r.down, load: loadA + loadB, loadB };
  });
}

/**
 * The daemon's refusal for `next`, or null when it is admitted (or brings no
 * new upsampler). Priced at `playing` (the source rate playing, or the last
 * one played) when `next` offers it, else at the heaviest allowed rate.
 */
export function cpuRefusal(old: EngineConfig, next: EngineConfig, playing: number | null = null): string | null {
  if (!next.dsp_enabled || upsamplerPart(old) === upsamplerPart(next)) return null;
  const all = landings(next);
  const at = all.find((l) => l.rate === playing) ?? all.reduce<Landing | null>((w, l) => (!w || l.load > w.load ? l : w), null);
  if (!at || at.load <= RT_BUDGET) return null;
  const needed = at.load * CHANNELS * DOT_RATE;
  const sustained = RT_BUDGET * CHANNELS * DOT_RATE;
  if (at.loadB > 0 && at.load - at.loadB <= RT_BUDGET) {
    const neededB = at.loadB * CHANNELS * DOT_RATE;
    return (
      `Too heavy for this CPU at ${khz(at.out)} from a ${khz(at.rate)} source with A/B: side B's upsampler ` +
      `(and its own quantizer, when its bits or dither differ) needs about ${(neededB / 1e9).toFixed(1)} G MAC/s ` +
      `(${Math.round(at.loadB * 100)}% of one core) on top of chain A's, ${(needed / 1e9).toFixed(1)} G MAC/s in all, ` +
      `and this machine sustains about ${(sustained / 1e9).toFixed(1)} G MAC/s in real time. ` +
      "Pick a lighter filter for side B, or turn A/B off."
    );
  }
  return (
    `Too heavy for this CPU at ${khz(at.out)} from a ${khz(at.rate)} source: about ${(needed / 1e9).toFixed(1)} G MAC/s needed ` +
    `(${Math.round(at.load * 100)}% of one core), this machine sustains about ${(sustained / 1e9).toFixed(1)} G MAC/s in real time. ` +
    "Use fewer taps or a lower output rate."
  );
}

/**
 * `status.upsampler_rates` for `cfg`: each allowed rate the upsampler
 * resamples, its load, and the light filter where it is over budget by the
 * estimate or overloaded there (`tripped`). Empty with DSP or the upsampler off.
 */
export function rateAdmissions(cfg: EngineConfig, tripped: readonly number[] = []): RateAdmission[] {
  if (!cfg.dsp_enabled || cfg.chain.upsampler.target === "off") return [];
  return landings(cfg)
    .filter((l) => l.out !== l.rate)
    .sort((a, b) => a.rate - b.rate)
    .map(({ rate, out, load }) => {
      const cause = tripped.includes(rate) ? ("overload" as const) : load > RT_BUDGET ? ("estimate" as const) : null;
      return { source_rate: rate, output_rate: out, load, ...(cause ? { fallback: { action: "light_upsampler" as const, cause, load: 0.01 } } : {}) };
    });
}
