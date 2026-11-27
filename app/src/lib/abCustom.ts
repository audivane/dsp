// "Custom B": side B as chain A plus a checklist of changes, combined into
// one JSON merge patch (the daemon's `set_ab` takes any number of fields at
// once). The selection names target states ("crossfeed off"), not flips, so
// a saved custom B means the same thing against whatever A is later.

import type { ChainConfig, DitherKind, FilterDesign, HarmonicsSystem } from "../api/types";
import { COMPARE_STAGE_NAME, filterName, stagePatch, type CompareMemory, type CompareStage } from "./abCompare";
import { deepEqual } from "./chain";
import { RECOMMENDED_PRESET } from "./filterDesign";
import { patchError } from "./abPatch";
import { configFromSystem, ditherInfo } from "./effects";

/** The stages the builder switches on or off, in the order it lists them. */
export const CUSTOM_STAGES: readonly CompareStage[] = ["harmonics", "eq", "tilt", "crossfeed", "conv", "dc", "pitch"];

/** The NOS filter preset: the seamless stand-in for "upsampling off". */
export const NOS_PRESET = "nos";

export type StageTarget = "on" | "off";

/** Side B's upsampling filter: a preset (NOS included) or a custom design. */
export type CustomFilter = { preset: string } | { design: FilterDesign };

/** What a custom B changes against chain A. Absent fields: same as A. */
export interface CustomSel {
  stages: Partial<Record<CompareStage, StageTarget>>;
  /** Harmonics on with this system's layers (wins over `stages.harmonics`). */
  harmonicsSystem?: string;
  filter?: CustomFilter;
  output?: { bits: number; dither: DitherKind };
  /** The user's own label; absent: built from the summary. */
  label?: string;
}

export const EMPTY_SEL: CustomSel = { stages: {} };

/** Every optional stage off, upsampling NOS: "B = A with everything off". */
export function everythingOffSel(): CustomSel {
  return { stages: Object.fromEntries(CUSTOM_STAGES.map((s) => [s, "off"])) as CustomSel["stages"], filter: { preset: NOS_PRESET } };
}

export interface CustomBuild {
  /** The combined patch; null when nothing differs from A. */
  patch: Record<string, unknown> | null;
  /** What B changes, in plain words ("harmonics off", "upsampling NOS"). */
  changes: string[];
  /** Choices that change nothing against A now ("crossfeed is already off in A"). */
  notes: string[];
  /** Choices that cannot be built (a stage with no settings to turn on). */
  errors: string[];
  /** The label B gets, in the A/B view and blind tests. */
  label: string;
}

type Obj = Record<string, unknown>;

function isObj(v: unknown): v is Obj {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Merge two merge patches into one that does both (the later wins on a clash). */
export function combinePatches(a: Obj, b: Obj): Obj {
  const out: Obj = { ...a };
  for (const [k, v] of Object.entries(b)) out[k] = isObj(out[k]) && isObj(v) ? combinePatches(out[k] as Obj, v) : structuredClone(v);
  return out;
}

const lower = (s: string) => (s === "EQ" || s === "DC block" ? s : s.toLowerCase());

/** A filter's words in the summary. */
export function customFilterText(f: CustomFilter): string {
  if ("design" in f) return "custom filter";
  return f.preset === NOS_PRESET ? "upsampling NOS" : `filter ${filterName(f.preset)}`;
}

function sameDesign(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/**
 * Build side B's patch from the selection against chain A as it is now.
 * `systems` resolves a harmonics system id; `memory` is what an EQ "on"
 * starts from when A has dropped its EQ.
 */
export function buildCustom(a: ChainConfig, sel: CustomSel, systems: readonly HarmonicsSystem[] = [], memory: CompareMemory = {}): CustomBuild {
  let patch: Obj = {};
  const changes: string[] = [];
  const notes: string[] = [];
  const errors: string[] = [];
  const add = (p: Obj, words: string) => {
    patch = combinePatches(patch, p);
    changes.push(words);
  };

  for (const stage of CUSTOM_STAGES) {
    const name = COMPARE_STAGE_NAME[stage];
    if (stage === "harmonics" && sel.harmonicsSystem) {
      const sys = systems.find((s) => s.id === sel.harmonicsSystem);
      if (!sys) {
        errors.push(`Harmonics system “${sel.harmonicsSystem}” is not offered by this engine.`);
        continue;
      }
      // The whole stage from the system: its layers and routing replace A's.
      const cfg = configFromSystem(sys, true);
      add({ harmonics: { ...cfg, dc_block: a.harmonics?.dc_block ?? cfg.dc_block, routing: cfg.routing ?? null } }, `harmonics ${sys.name}`);
      continue;
    }
    const target = sel.stages[stage];
    if (!target) continue;
    const flip = stagePatch(a, stage, memory);
    if ("error" in flip) {
      // Nothing set up in A: off is already the case; on cannot be built.
      if (target === "off") notes.push(`${name} is already off in A.`);
      else errors.push(flip.error);
      continue;
    }
    const dir = flip.label.endsWith(" off") ? "off" : "on";
    if (dir !== target) {
      notes.push(`${name} is already ${target} in A.`);
      continue;
    }
    add(flip.patch, `${lower(name)} ${target}`);
  }

  if (sel.filter) {
    const u = a.upsampler;
    const f = sel.filter;
    if (u.target === "off") notes.push("Upsampling is off in A: a filter change in B needs A to upsample.");
    else if ("design" in f) {
      if (!u.preset && sameDesign(u.design, f.design)) notes.push("A already runs this custom filter.");
      else add({ upsampler: { preset: null, design: structuredClone(f.design) } }, customFilterText(f));
    } else if ((u.preset ?? null) === f.preset && !u.design) notes.push(`A already runs ${f.preset === NOS_PRESET ? "NOS" : filterName(f.preset)}.`);
    else add({ upsampler: { preset: f.preset, design: null } }, customFilterText(f));
  }

  if (sel.output) {
    const { bits, dither } = sel.output;
    if (bits === a.output.bits && dither === a.output.dither) notes.push("Output bits and dither are the same as A's.");
    else {
      const d = ditherInfo(dither)?.label ?? dither;
      add({ output: { bits, dither } }, `output ${bits}-bit ${d}`);
    }
  }

  const err = changes.length ? patchError(patch) : null;
  if (err) errors.push(err);
  const auto = changes.length ? changes[0].charAt(0).toUpperCase() + changes[0].slice(1) + (changes.length > 1 ? ", " + changes.slice(1).join(", ") : "") : "";
  return { patch: changes.length ? patch : null, changes, notes, errors, label: sel.label?.trim() || auto || "Custom" };
}

/** A stage's state in a chain: on, off, or off with nothing set up to turn on. */
export type StageState = "on" | "off" | "none";

export function stageState(c: ChainConfig, stage: CompareStage, memory: CompareMemory = {}): StageState {
  const r = stagePatch(c, stage, memory);
  if ("error" in r) return "none";
  return r.label.endsWith(" off") ? "on" : "off";
}

/**
 * The selection that describes chain `b` against chain `a`: each stage B runs
 * differently, B's filter and output when they differ. Lets the builder show
 * what side B is now, whatever made it (a stage compare, a filter compare,
 * Flat, a preset). Only on/off is seen per stage: a preset's EQ bands are not.
 */
export function selFromChain(a: ChainConfig, b: ChainConfig, systems: readonly HarmonicsSystem[] = [], memory: CompareMemory = {}): CustomSel {
  const sel: CustomSel = { stages: {} };
  for (const stage of CUSTOM_STAGES) {
    const sa = stageState(a, stage, memory) === "on";
    const sb = stageState(b, stage, memory) === "on";
    if (sa !== sb) sel.stages[stage] = sb ? "on" : "off";
  }
  const hb = b.harmonics;
  if (hb?.enabled && hb.system && systems.some((s) => s.id === hb.system) && !deepEqual(a.harmonics, hb)) {
    delete sel.stages.harmonics;
    sel.harmonicsSystem = hb.system;
  }
  // The engine runs "recommended" when neither a preset nor a design is set.
  const filterOf = (u: ChainConfig["upsampler"]): CustomFilter => (!u.preset && u.design ? { design: structuredClone(u.design) } : { preset: u.preset ?? RECOMMENDED_PRESET });
  if (a.upsampler.target !== "off") {
    const fb = filterOf(b.upsampler);
    if (!deepEqual(filterOf(a.upsampler), fb)) sel.filter = fb;
  }
  if (b.output.bits !== a.output.bits || b.output.dither !== a.output.dither) sel.output = { bits: b.output.bits, dither: b.output.dither };
  return sel;
}

/** Whether the selection picks anything at all. */
export function selIsEmpty(sel: CustomSel): boolean {
  return !sel.harmonicsSystem && !sel.filter && !sel.output && !Object.values(sel.stages).some(Boolean);
}

// ------------------------------------------------------------ saved custom Bs

export interface SavedCustomB {
  name: string;
  sel: CustomSel;
}

export const SAVED_KEY = "avdsp-ab-custom";
export const SAVED_MAX = 12;

export function loadSaved(): SavedCustomB[] {
  try {
    const raw = localStorage.getItem(SAVED_KEY);
    const list = raw ? (JSON.parse(raw) as unknown) : [];
    return Array.isArray(list) ? list.filter((s): s is SavedCustomB => isObj(s) && typeof s.name === "string" && isObj(s.sel) && isObj((s.sel as Obj).stages)) : [];
  } catch {
    return [];
  }
}

export function storeSaved(list: SavedCustomB[]): void {
  try {
    localStorage.setItem(SAVED_KEY, JSON.stringify(list));
  } catch {
    /* storage unavailable: the list lives for this session */
  }
}

/** The list with `entry` saved under its name (replacing one of that name), newest first. */
export function withSaved(list: readonly SavedCustomB[], entry: SavedCustomB): SavedCustomB[] {
  return [structuredClone(entry), ...list.filter((s) => s.name !== entry.name)].slice(0, SAVED_MAX);
}
