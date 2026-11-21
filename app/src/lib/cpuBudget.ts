// The mock engine's CPU admission check: the daemon's cost model
// (`control::cpu_budget`) for custom designs, on a fixed "machine" that
// does 5 G multiply-adds per second per core, 7.5 G sustained in real time
// at the upsampler's share (a desktop CPU, with a long kernel's spectra
// streamed from memory). Presets are
// light and always admitted here. The real engine measures its CPU at
// start and sends its own message; the app only shows it.

import type { EngineConfig, UpsamplerConfig } from "../api/types";
import { fromUpsamplerConfig } from "./upsamplerAdapter";
import { resolveRateRatio } from "./upsamplerRate";
import { chainB } from "./abPatch";

/** Share of one core the upsampler may take (the mock's `status.limits.cpu_share`). */
export const RT_BUDGET = 0.9;
/** The mock machine: in-cache dot multiply-adds per second per channel. */
const DOT_RATE = 5.0e9;
/** Spectral bins per second of the FFT polyphase engine. */
const SPECTRAL_RATE = 0.5e9;
const CHANNELS = 2;

/** `scaled_quantum`: the graph quantum at `rate` (a power of two). */
function engineBlock(quantum: number, rate: number): number {
  const raw = Math.min(8192, Math.max(32, Math.floor((quantum * rate) / 48000)));
  const q = 2 ** Math.floor(Math.log2(raw));
  return Math.min(1024, Math.max(64, Math.max(512, q)));
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
    const b = engineBlock(quantum, rate);
    const parts = Math.max(1, Math.ceil(Math.ceil(taps / r.up) / b));
    const bins = (rate / b) * parts * r.up * (b + 1);
    return CHANNELS * (bins / SPECTRAL_RATE) * 1.15;
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

/** The daemon's refusal for `next`, or null when it is admitted (or brings no new upsampler). */
export function cpuRefusal(old: EngineConfig, next: EngineConfig): string | null {
  if (!next.dsp_enabled || upsamplerPart(old) === upsamplerPart(next)) return null;
  let worst = { load: 0, out: 0 };
  for (const rate of next.rate.allowed_rates) {
    const chains = [next.chain, ...(next.ab ? [chainB(next.ab, next.chain)] : [])];
    const load = chains.reduce((sum, c) => sum + upsamplerLoad(c.upsampler, rate, next.quantum), 0);
    const r = resolveRateRatio(next.chain.upsampler.target, rate);
    if (load > worst.load) worst = { load, out: (rate * r.up) / r.down };
  }
  if (worst.load <= RT_BUDGET) return null;
  const needed = worst.load * CHANNELS * DOT_RATE;
  const sustained = RT_BUDGET * CHANNELS * DOT_RATE;
  return (
    `Too heavy for this CPU at ${khz(worst.out)}: about ${(needed / 1e9).toFixed(1)} G MAC/s needed ` +
    `(${Math.round(worst.load * 100)}% of one core), this machine sustains about ${(sustained / 1e9).toFixed(1)} G MAC/s in real time. ` +
    "Use fewer taps or a lower output rate."
  );
}
