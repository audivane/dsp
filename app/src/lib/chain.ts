// Helpers over the chain/EQ wire types that hide the protocol's variants
// (linked vs split EQ) and an older daemon's single-IR convolution shape.

import type { Band, ChainConfig, ChannelEq, ConvolutionConfig, EqProfile } from "../api/types";

export type EqSection = "linked" | "left" | "right";

export const EQ_PROFILE_VERSION = 2;

/**
 * `chain.convolution` as a list. The protocol type is an array, but a daemon
 * from before the IR chain sends `null` or one object; both are accepted.
 */
export function convList(chain: ChainConfig | null | undefined): ConvolutionConfig[] {
  const c = chain?.convolution as ConvolutionConfig[] | ConvolutionConfig | null | undefined;
  if (!c) return [];
  return Array.isArray(c) ? c : [c];
}

export function emptyEq(name = ""): EqProfile {
  return { name, version: EQ_PROFILE_VERSION, preamp_db: 0, linked_enabled: true, bands: [] };
}

export function normalizeEq(eq: EqProfile): EqProfile {
  const raw = eq as EqProfile & { version?: number; linked_enabled?: boolean };
  const version = raw.version ?? 0;
  if (version < 2 && raw.right) {
    return {
      name: raw.name,
      version: EQ_PROFILE_VERSION,
      preamp_db: 0,
      linked_enabled: raw.linked_enabled ?? true,
      bands: [],
      left: {
        enabled: true,
        preamp_db: raw.preamp_db,
        bands: cloneBands(raw.bands),
      },
      right: {
        enabled: raw.right.enabled ?? true,
        preamp_db: raw.right.preamp_db,
        bands: cloneBands(raw.right.bands),
      },
      ...(raw.tilt ? { tilt: { ...raw.tilt } } : {}),
      ...(raw.tilt_high ? { tilt_high: { ...raw.tilt_high } } : {}),
    };
  }
  const out: EqProfile = {
    name: raw.name,
    version: EQ_PROFILE_VERSION,
    preamp_db: raw.preamp_db,
    linked_enabled: raw.linked_enabled ?? true,
    bands: cloneBands(raw.bands),
  };
  if (raw.left) {
    out.left = { enabled: raw.left.enabled ?? true, preamp_db: raw.left.preamp_db, bands: cloneBands(raw.left.bands) };
  }
  if (raw.right) {
    out.right = { enabled: raw.right.enabled ?? true, preamp_db: raw.right.preamp_db, bands: cloneBands(raw.right.bands) };
  }
  if (raw.tilt) out.tilt = { ...raw.tilt };
  if (raw.tilt_high) out.tilt_high = { ...raw.tilt_high };
  return out;
}

export function hasSection(eq: EqProfile | null | undefined, section: EqSection): boolean {
  if (!eq) return false;
  if (section === "linked") return true;
  return section === "left" ? !!eq.left : !!eq.right;
}

export function sectionEnabled(eq: EqProfile | null | undefined, section: EqSection): boolean {
  if (!eq) return false;
  if (section === "linked") return eq.linked_enabled ?? true;
  return section === "left" ? !!eq.left?.enabled : !!eq.right?.enabled;
}

export function sectionBands(eq: EqProfile | null | undefined, section: EqSection): Band[] {
  if (!eq) return [];
  if (section === "linked") return eq.bands;
  return (section === "left" ? eq.left?.bands : eq.right?.bands) ?? [];
}

export function sectionPreamp(eq: EqProfile | null | undefined, section: EqSection): number {
  if (!eq) return 0;
  if (section === "linked") return eq.preamp_db;
  return (section === "left" ? eq.left?.preamp_db : eq.right?.preamp_db) ?? 0;
}

export function sectionSet(eq: EqProfile | null | undefined, section: EqSection): ChannelEq {
  if (!eq) return { enabled: false, preamp_db: 0, bands: [] };
  if (section === "linked") return { enabled: eq.linked_enabled ?? true, preamp_db: eq.preamp_db, bands: eq.bands };
  const ch = section === "left" ? eq.left : eq.right;
  return ch ?? { enabled: false, preamp_db: 0, bands: [] };
}

export function setSectionEnabled(eq: EqProfile, section: EqSection, enabled: boolean): EqProfile {
  if (section === "linked") {
    return { ...eq, version: EQ_PROFILE_VERSION, linked_enabled: enabled };
  }
  const prev = eq[section] ?? { enabled, preamp_db: 0, bands: [] };
  return { ...eq, version: EQ_PROFILE_VERSION, [section]: { ...prev, enabled } };
}

export function addSection(eq: EqProfile, section: "left" | "right"): EqProfile {
  if (eq[section]) return eq;
  return {
    ...eq,
    version: EQ_PROFILE_VERSION,
    [section]: { enabled: true, preamp_db: 0, bands: [] },
  };
}

export function withSectionBands(eq: EqProfile, section: EqSection, bands: Band[]): EqProfile {
  if (section === "linked") {
    return { ...eq, version: EQ_PROFILE_VERSION, bands };
  }
  const prev = eq[section] ?? { enabled: true, preamp_db: 0, bands: [] };
  return { ...eq, version: EQ_PROFILE_VERSION, [section]: { ...prev, bands } };
}

export function clearSection(eq: EqProfile, section: EqSection): EqProfile {
  if (section === "linked") {
    return { ...eq, version: EQ_PROFILE_VERSION, preamp_db: 0, bands: [] };
  }
  const prev = eq[section];
  if (!prev) return eq;
  return { ...eq, version: EQ_PROFILE_VERSION, [section]: { enabled: prev.enabled, preamp_db: 0, bands: [] } };
}

export function removeSection(eq: EqProfile, section: "left" | "right"): EqProfile {
  const out: EqProfile = { ...eq, version: EQ_PROFILE_VERSION };
  delete out[section];
  return out;
}

export function eqIsActive(eq: EqProfile | null | undefined): boolean {
  if (!eq) return false;
  const linkedOn = (eq.linked_enabled ?? true) && eq.bands.some((b) => b.enabled);
  const leftOn = !!eq.left?.enabled && (eq.left.preamp_db !== 0 || eq.left.bands.some((b) => b.enabled));
  const rightOn = !!eq.right?.enabled && (eq.right.preamp_db !== 0 || eq.right.bands.some((b) => b.enabled));
  return eq.preamp_db !== 0 || linkedOn || leftOn || rightOn || !!eq.tilt?.enabled || !!eq.tilt_high?.enabled;
}

export function cloneBands(bands: readonly Band[]): Band[] {
  return bands.map((b) => ({ ...b }));
}

export function bandCount(eq: EqProfile | null | undefined): number {
  if (!eq) return 0;
  return eq.bands.length + (eq.left?.bands.length ?? 0) + (eq.right?.bands.length ?? 0);
}

/** Structural equality for plain JSON values (key order independent). */
export function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b || a === null || b === null || typeof a !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    const bb = b as unknown[];
    return a.length === bb.length && a.every((x, i) => deepEqual(x, bb[i]));
  }
  const ao = a as Record<string, unknown>;
  const bo = b as Record<string, unknown>;
  const ak = Object.keys(ao).filter((k) => ao[k] !== undefined);
  const bk = Object.keys(bo).filter((k) => bo[k] !== undefined);
  return ak.length === bk.length && ak.every((k) => deepEqual(ao[k], bo[k]));
}

export function fileName(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

/** Display name of an IR: its `name`, else the file name. */
export function irName(c: ConvolutionConfig): string {
  return c.name || fileName(c.ir_path);
}
