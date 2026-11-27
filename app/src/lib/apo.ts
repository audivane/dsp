// Equalizer APO / AutoEQ `ParametricEQ.txt` import and export.
//
// The parser follows crates/dsp-core/src/eq_apo.rs (same tokens, same BW and
// S conversions, same defaults): import fills the linked section and warns on
// directives it does not know. `Channel: L` / `Channel: R` put the filters
// (and a Preamp) that follow into that channel's own section, `Channel: all`
// goes back to the linked one. Shelves are written as LSC/HSC (the Q form
// Equalizer APO and AutoEQ use). The tone knobs export as the last two linked
// filters and import back from a matching pair at the end of the linked
// filters.

import type { Band, EqProfile, FilterType, ToneConfig } from "../api/types";
import { isToneShelf, toneBands } from "./tone";

export interface ApoParseResult {
  profile: EqProfile;
  warnings: string[];
}

export class ApoError extends Error {
  constructor(
    public line: number,
    message: string,
  ) {
    super(`Line ${line}: ${message}`);
  }
}

const TYPE_IN: Record<string, FilterType | null> = {
  pk: "peaking",
  peq: "peaking",
  ls: "low_shelf",
  lsc: "low_shelf",
  hs: "high_shelf",
  hsc: "high_shelf",
  lp: "low_pass",
  lpq: "low_pass",
  hp: "high_pass",
  hpq: "high_pass",
  bp: "band_pass",
  no: "notch",
  ap: "all_pass",
  none: null,
};

const TYPE_OUT: Record<FilterType, string> = {
  peaking: "PK",
  low_shelf: "LSC",
  high_shelf: "HSC",
  low_pass: "LP",
  high_pass: "HP",
  band_pass: "BP",
  notch: "NO",
  all_pass: "AP",
};

function num(token: string | undefined, line: number, what: string): number {
  if (token === undefined) throw new ApoError(line, `${what} is missing a value`);
  const v = Number(token);
  if (!/^[+-]?(\d+\.?\d*|\.\d+)(e[+-]?\d+)?$/i.test(token) || !Number.isFinite(v)) {
    throw new ApoError(line, `${what} has an invalid number “${token}”`);
  }
  return v;
}

function parseFilter(spec: string, line: number): Band | null {
  const tokens = spec.split(/\s+/).filter(Boolean);
  let i = 0;
  let token = tokens[i++];
  if (token === undefined) throw new ApoError(line, "empty filter definition");
  let enabled = true;
  if (/^(on|off)$/i.test(token)) {
    enabled = token.toLowerCase() === "on";
    token = tokens[i++];
    if (token === undefined) throw new ApoError(line, "missing filter type");
  }
  const lower = token.toLowerCase();
  if (!(lower in TYPE_IN)) throw new ApoError(line, `unknown filter type “${token}”`);
  const kind = TYPE_IN[lower];
  if (kind === null) return null;

  let freq: number | undefined;
  let gain: number | undefined;
  let q: number | undefined;
  let bw: number | undefined;
  let slope: number | undefined;
  const skip = (unit: string) => {
    if (tokens[i]?.toLowerCase() === unit) i++;
  };
  while (i < tokens.length) {
    const t = tokens[i++].toLowerCase();
    if (t === "fc") {
      freq = num(tokens[i++], line, "Fc");
      skip("hz");
    } else if (t === "gain") {
      gain = num(tokens[i++], line, "Gain");
      skip("db");
    } else if (t === "q") {
      q = num(tokens[i++], line, "Q");
    } else if (t === "bw") {
      if (tokens[i]?.toLowerCase() === "oct") i++;
      bw = num(tokens[i++], line, "BW");
      skip("oct");
    } else if (t === "s") {
      slope = num(tokens[i++], line, "S");
    } else {
      throw new ApoError(line, `unexpected filter parameter “${tokens[i - 1]}”`);
    }
  }
  if (freq === undefined) throw new ApoError(line, "filter is missing Fc");
  const gainDb = gain ?? 0;
  let qq: number;
  if (q !== undefined) qq = q;
  else if (bw !== undefined) {
    if (!(bw > 0)) throw new ApoError(line, "BW must be positive");
    const w = Math.pow(2, bw);
    qq = Math.sqrt(w) / (w - 1);
  } else if (slope !== undefined) {
    if (!(slope > 0)) throw new ApoError(line, "S must be positive");
    const a = Math.pow(10, gainDb / 40);
    const inner = (a + 1 / a) * (1 / slope - 1) + 2;
    if (!(inner > 0) || !Number.isFinite(inner)) throw new ApoError(line, "shelf slope S is out of range for this gain");
    qq = 1 / Math.sqrt(inner);
  } else qq = Math.SQRT1_2;
  if (!(qq > 0) || !Number.isFinite(qq)) throw new ApoError(line, "Q must be a positive number");
  return { kind, freq_hz: freq, gain_db: gainDb, q: qq, enabled };
}

/** Parse a ParametricEQ.txt into the linked EQ section. Throws `ApoError` on malformed Preamp/Filter lines. */
export function parseApo(text: string, name = ""): ApoParseResult {
  const sets = { all: { preamp_db: 0, bands: [] as Band[] }, L: { preamp_db: 0, bands: [] as Band[] }, R: { preamp_db: 0, bands: [] as Band[] } };
  let ch: keyof typeof sets = "all";
  const warnings: string[] = [];
  text.split(/\r?\n/).forEach((raw, idx) => {
    const lineNo = idx + 1;
    const line = (raw.split("#")[0] ?? "").trim();
    if (!line) return;
    const colon = line.indexOf(":");
    if (colon < 0) {
      const first = line.split(/\s+/)[0].toLowerCase();
      if (first === "preamp" || first === "filter") throw new ApoError(lineNo, "expected “:” after the directive name");
      warnings.push(`Line ${lineNo}: ignored “${first}”`);
      return;
    }
    const key = line.slice(0, colon).trim().toLowerCase();
    const rest = line.slice(colon + 1).trim();
    if (key === "preamp") {
      const toks = rest.split(/\s+/).filter(Boolean);
      const v = num(toks[0], lineNo, "Preamp");
      if (toks[1] !== undefined && toks[1].toLowerCase() !== "db") throw new ApoError(lineNo, `unexpected “${toks[1]}” after the Preamp value`);
      sets[ch].preamp_db = v;
    } else if (key === "filter" || /^filter\s*\d+$/.test(key)) {
      const band = parseFilter(rest, lineNo);
      if (band === null) warnings.push(`Line ${lineNo}: skipped a None filter`);
      else sets[ch].bands.push(band);
    } else if (key === "channel") {
      const v = rest.toUpperCase();
      if (v === "L" || v === "R") ch = v;
      else {
        ch = "all";
        if (v !== "ALL") warnings.push(`Line ${lineNo}: channel “${rest}” read as all channels`);
      }
    } else {
      warnings.push(`Line ${lineNo}: ignored “${line.slice(0, colon).trim()}”`);
    }
  });
  const profile: EqProfile = {
    name,
    version: 2,
    preamp_db: sets.all.preamp_db,
    linked_enabled: true,
    bands: sets.all.bands,
  };
  const tone = takeTone(profile.bands);
  if (tone) profile.tone = tone;
  const side = (s: { preamp_db: number; bands: Band[] }) => (s.bands.length || s.preamp_db ? { enabled: true, ...s } : undefined);
  const left = side(sets.L);
  const right = side(sets.R);
  if (left || right) {
    profile.left = left ?? { enabled: true, preamp_db: 0, bands: [] };
    profile.right = right ?? { enabled: true, preamp_db: 0, bands: [] };
  }
  return { profile, warnings };
}

/** Pull the tone shelves (bass then treble, either alone) off the end of `bands`, as `toApo` writes them. */
function takeTone(bands: Band[]): ToneConfig | null {
  const tone: ToneConfig = { bass_db: 0, treble_db: 0 };
  for (const knob of ["treble_db", "bass_db"] as const) {
    const last = bands[bands.length - 1];
    if (last && isToneShelf(last) === knob) {
      tone[knob] = last.gain_db;
      bands.pop();
    }
  }
  return tone.bass_db || tone.treble_db ? tone : null;
}

function writeSet(lines: string[], preamp: number, bands: readonly Band[]) {
  lines.push(`Preamp: ${String(preamp)} dB`);
  bands.forEach((b, i) => {
    let s = `Filter ${i + 1}: ${b.enabled ? "ON" : "OFF"} ${TYPE_OUT[b.kind]} Fc ${String(b.freq_hz)} Hz`;
    const usesGain = b.kind === "peaking" || b.kind === "low_shelf" || b.kind === "high_shelf";
    if (usesGain || b.gain_db !== 0) s += ` Gain ${String(b.gain_db)} dB`;
    s += ` Q ${String(b.q)}`;
    lines.push(s);
  });
}

/** Serialize the EQ to ParametricEQ.txt: the linked section, then any
 *  per-channel bands under `Channel: L` / `Channel: R`. */
export function toApo(profile: EqProfile): string {
  const lines: string[] = [];
  writeSet(lines, profile.preamp_db, [...profile.bands, ...toneBands(profile.tone)]);
  for (const [name, c] of [["L", profile.left], ["R", profile.right]] as const) {
    if (!c || !c.enabled || (!c.bands.length && !c.preamp_db)) continue;
    lines.push(`Channel: ${name}`);
    if (c.preamp_db) lines.push(`Preamp: ${String(c.preamp_db)} dB`);
    c.bands.forEach((b, i) => {
      const one: string[] = [];
      writeSet(one, 0, [b]);
      lines.push(one[1].replace(/^Filter 1:/, `Filter ${i + 1}:`));
    });
  }
  if (profile.left || profile.right) lines.push("Channel: all");
  return lines.join("\n") + "\n";
}

/** "HD650 ParametricEQ.txt" → "HD650". */
export function presetNameFromFile(fileName: string): string {
  const base = fileName.replace(/\.txt$/i, "").replace(/[\s_-]*ParametricEQ$/i, "").trim();
  return (base || "Imported").slice(0, 64);
}
