// Quick stage compare: the side-B patches the stage headers and the
// Upsampling view send, mirroring `avdsp ctl ab stage` and `ab filter`
// (crates/cli/src/ctl.rs `stage_patch`), and the words the A/B view and the
// top bar use for side B and its level.

import type { AbConfig, AbStatus, ChainConfig, DitherKind, EqProfile, FilterDesign, FilterPreset, OverloadReport, TiltConfig } from "../api/types";
import { deepEqual } from "./chain";
import { ditherInfo, pitchActive, pitchCents } from "./effects";
import { presetEntry } from "./filterPresets";
import { RECOMMENDED_PRESET } from "./filterDesign";
import { fmtDb } from "./format";
import { tiltSlope } from "./tilt";
import { toneOn } from "./tone";
import { FLAT_PATCH } from "./abPatch";

/** The stages a header Compare button can take out of (or put into) side B. */
export type CompareStage = "crossfeed" | "eq" | "tilt" | "conv" | "dc" | "pitch" | "harmonics";

export const COMPARE_STAGE_NAME: Record<CompareStage, string> = {
  crossfeed: "Stereo image",
  eq: "EQ",
  tilt: "Tilt",
  conv: "Convolution",
  dc: "DC block",
  pitch: "Pitch",
  harmonics: "Harmonics",
};

/** A side B ready to send: the merge patch and its label. */
export interface BPatch {
  patch: Record<string, unknown>;
  label: string;
}

export type StagePatchResult = BPatch | { error: string };

/** Last settings of a stage that is off and empty in chain A ("on" compares start from them). */
export interface CompareMemory {
  eq?: EqProfile | null;
}

function toggle(key: string, name: string, active: boolean, canEnable: boolean): StagePatchResult {
  if (active) return { patch: { [key]: { enabled: false } }, label: `${name} off` };
  if (canEnable) return { patch: { [key]: { enabled: true } }, label: `${name} on` };
  return { error: `${name} has no settings in chain A to compare; set it up first.` };
}

function eqBandsActive(eq: EqProfile | null | undefined): boolean {
  if (!eq) return false;
  const ch = (c: EqProfile["left"]) => !!c && c.enabled && (c.preamp_db !== 0 || c.bands.some((b) => b.enabled));
  return eq.preamp_db !== 0 || ((eq.linked_enabled ?? true) && eq.bands.some((b) => b.enabled)) || ch(eq.left) || ch(eq.right) || toneOn(eq);
}

/**
 * Side B for a stage compare: the stage off when chain A runs it, on when A
 * has it set up but switched off (the CLI's `ab stage`). EQ and convolution
 * have no on/off flag of their own: EQ off drops the bands and preamp (the
 * tilt is its own stage), convolution off empties the IR list. Turning them
 * on in B needs settings A still holds: the IRs A has switched off, or the
 * EQ the app remembers from before it was switched off.
 */
export function stagePatch(a: ChainConfig, stage: CompareStage, memory: CompareMemory = {}): StagePatchResult {
  switch (stage) {
    case "crossfeed":
    {
      // A missing crossfeed turns on with the engine's defaults. None with
      // width 0 (k = 1) does nothing either way, so there is nothing to compare.
      const cf = a.crossfeed;
      const shaping = !cf || cf.mode !== "none" || (cf.width ?? 1) !== 1;
      return toggle("crossfeed", "Stereo image", !!cf?.enabled && shaping, shaping);
    }
    case "dc":
      return toggle("dc_block", "DC block", !!a.dc_block?.enabled, true);
    case "harmonics":
      return toggle("harmonics", "Harmonics", !!a.harmonics?.enabled, true);
    case "pitch":
      return toggle("pitch", "Pitch", pitchActive(a.pitch), pitchCents(a.pitch) !== 0);
    case "tilt": {
      // Both sections of a two-section tilt switch together.
      const keys = (["tilt", "tilt_high"] as const).filter((k) => a.eq?.[k]);
      const shapedOf = (t: TiltConfig) => (tiltSlope(t) ?? 0) !== 0;
      const shaped = keys.some((k) => shapedOf(a.eq![k]!));
      const running = keys.some((k) => a.eq![k]!.enabled && shapedOf(a.eq![k]!));
      // Off removes both sections from B, whatever shape A takes later (an
      // `enabled: false` merged onto a section A drops would leave B a section
      // without a slope). On snapshots the full section(s): dropping a section
      // on A later must not leave B with a partial `{ enabled: true }`.
      if (running) return { patch: { eq: { tilt: null, tilt_high: null } }, label: "Tilt off" };
      if (shaped) {
        return {
          patch: { eq: Object.fromEntries(keys.map((k) => [k, { ...structuredClone(a.eq![k]!), enabled: true }])) },
          label: "Tilt on",
        };
      }
      return { error: "Tilt has no settings in chain A to compare; set a slope first." };
    }
    case "eq": {
      if (eqBandsActive(a.eq)) return { patch: { eq: { preamp_db: 0, bands: [], left: null, right: null, tone: null } }, label: "EQ off" };
      const last = memory.eq;
      if (!a.eq && last && eqBandsActive(last)) {
        return { patch: { eq: structuredClone(last) as unknown as Record<string, unknown> }, label: "EQ on" };
      }
      return { error: "EQ is off in chain A; nothing to compare." };
    }
    case "conv": {
      const list = a.convolution ?? [];
      if (list.some((c) => c.enabled)) return { patch: { convolution: [] }, label: "Convolution off" };
      if (list.length) return { patch: { convolution: list.map((c) => ({ ...c, enabled: true })) }, label: "Convolution on" };
      return { error: "Convolution has no impulse response in chain A; add one first." };
    }
  }
}

/** The friendly name of a filter preset ("Flat-top xlong apod"), else its engine name. */
export function filterName(preset: string): string {
  if (preset === RECOMMENDED_PRESET) return "Recommended";
  return presetEntry(preset as FilterPreset)?.label ?? preset;
}

/** Label for a custom filter design pinned as side B. */
export function customFilterLabel(design: FilterDesign | null | undefined): string {
  const base = design?.based_on ? filterName(design.based_on) : null;
  return base ? `Filter: Custom (${base})` : "Filter: Custom";
}

/** Side B for a filter compare: A with another upsampling filter (the CLI's `ab filter`). */
export function filterPatch(preset: string): BPatch {
  return { patch: { upsampler: { preset, design: null } }, label: `Filter: ${filterName(preset)}` };
}

/** Side B for a custom filter compare: A with a pinned custom filter design. */
export function filterDesignPatch(design: FilterDesign, label?: string): BPatch {
  return { patch: { upsampler: { preset: null, design: structuredClone(design) } }, label: label ?? customFilterLabel(design) };
}

/** Label for an output compare side B ("Output: 16-bit TPDF"). */
export function outputLabel(bits: number, dither: DitherKind): string {
  const d = ditherInfo(dither)?.label ?? dither;
  return `Output: ${bits}-bit ${d}`;
}

/** Side B for an output compare: A with another bit depth and/or dither. */
export function outputPatch(bits: number, dither: DitherKind): BPatch {
  return { patch: { output: { bits, dither } }, label: outputLabel(bits, dither) };
}

/** Whether A/B already compares against this side B (the button then stops it). */
export function comparing(ab: AbConfig | null | undefined, patch: Record<string, unknown> | null): boolean {
  if (!ab || !patch || !("patch" in ab.b)) return false;
  return deepEqual(ab.b.patch, patch);
}

/** The stage a running stage compare is about, or null. */
export function comparedStage(ab: AbConfig | null | undefined, a: ChainConfig, memory: CompareMemory = {}): CompareStage | null {
  if (!ab || !("patch" in ab.b)) return null;
  for (const s of Object.keys(COMPARE_STAGE_NAME) as CompareStage[]) {
    const r = stagePatch(a, s, memory);
    if ("patch" in r && deepEqual(ab.b.patch, r.patch)) return s;
  }
  return null;
}

export type BKind = "flat" | "preset" | "stage" | "filter" | "output" | "custom" | "patch";

/** What side B is, for the A/B view: a kind and plain words. */
export function describeB(ab: AbConfig): { kind: BKind; title: string; detail: string } {
  if ("chain" in ab.b) {
    const name = ab.label || ab.b.chain.eq?.name || "Preset";
    return { kind: "preset", title: name, detail: "A stored preset, with its own stages and filter. It runs at A's upsampling ratio, limiter and output." };
  }
  const patch = ab.b.patch as Record<string, unknown> | null;
  if (deepEqual(patch, FLAT_PATCH)) {
    return { kind: "flat", title: "Flat", detail: "A without EQ and convolution. Every other stage follows A." };
  }
  const keys = patch && typeof patch === "object" ? Object.keys(patch) : [];
  if (keys.length === 1 && keys[0] === "upsampler") {
    const ups = (patch as { upsampler: { preset?: string | null; design?: FilterDesign | null } }).upsampler;
    const title = ab.label || (ups.preset ? `Filter: ${filterName(ups.preset)}` : ups.design ? customFilterLabel(ups.design) : "Another filter");
    return { kind: "filter", title, detail: "A with another upsampling filter at the same ratio. Every other stage follows A." };
  }
  if (keys.length === 1 && keys[0] === "output") {
    const out = (patch as { output: { bits?: number; dither?: DitherKind } }).output;
    const fallback = out.bits !== undefined && out.dither !== undefined ? outputLabel(out.bits, out.dither) : "Output";
    return { kind: "output", title: ab.label || fallback, detail: "A with another output bit depth or dither. Every other stage follows A." };
  }
  const title = ab.label || "Changed";
  // A custom B (the A/B view's builder) changes several stages at once.
  if (keys.length > 1) {
    return { kind: "custom", title, detail: "A with several settings changed at once. Everything else follows A." };
  }
  const stage = /^(.*) (off|on)$/.exec(title);
  if (stage && (Object.values(COMPARE_STAGE_NAME) as string[]).includes(stage[1])) {
    return { kind: "stage", title, detail: `A with ${stage[1].toLowerCase()} switched ${stage[2]}. Every other setting follows A as you change it.` };
  }
  return { kind: "patch", title, detail: "A with some settings changed. Everything else follows A." };
}

/** The level line: which side is cut and by how much (never boost), and the shared headroom. */
export function levelText(ab: AbStatus): string {
  const o = ab.offset_db;
  const main =
    Math.abs(o) < 0.05
      ? "A and B play at the same level"
      : o < 0
        ? `B is ${fmtDb(-o, 2, false)} dB louder, so B is cut ${fmtDb(-o, 2, false)} dB`
        : `B is ${fmtDb(o, 2, false)} dB quieter, so A is cut ${fmtDb(o, 2, false)} dB`;
  return `${main} · headroom ${fmtDb(-ab.headroom_db, 2)} dB`;
}

/** "B was limited 3×", from the attributed overs, or null when there were none. */
export function oversText(overs: AbStatus["overs"]): string | null {
  const n = (v: number) => (v >= 10 ? Math.round(v) : Math.round(v * 10) / 10);
  const a = n(overs.a);
  const b = n(overs.b);
  if (a <= 0 && b <= 0) return null;
  const side = (s: string, v: number) => `${s} was limited ${v}×`;
  return [a > 0 && side("A", a), b > 0 && side("B", b)].filter(Boolean).join(", ");
}

/**
 * Whether the overload fallback has A/B off: its first step, and every
 * later chain step keeps it off. The config keeps side B, so A/B comes
 * back with a new side B.
 */
export function abStoppedByOverload(ab: AbConfig | null | undefined, overload: OverloadReport | null | undefined): boolean {
  if (!ab || !overload) return false;
  return !overload.action.startsWith("varispeed");
}

/** The top bar's side-B text: "B: Stereo image off". */
export function topBarLabel(ab: AbConfig | null | undefined): string | null {
  if (!ab) return null;
  return `B: ${describeB(ab).title}`;
}
