// Pure helpers for the effect stages (DC block, pitch, crossfeed, harmonics,
// limiter) and the dither list: defaults, sidebar summaries, and the small
// pieces of the engine's maths the views show (crossfeed level and
// response, harmonic THD, DC-block response). Ranges and formulas follow
// docs/control-plane.md and crates/dsp-core.

import type {
  ChainConfig,
  CrossfeedConfig,
  CrossfeedLevel,
  CrossfeedMode,
  DcBlockConfig,
  DitherKind,
  EngineState,
  HarmonicsCharacter,
  HarmonicsConfig,
  LimiterConfig,
  LimiterMode,
  LimiterStatus,
  PitchConfig,
  PitchQuality,
  VarispeedConfig,
} from "../api/types";
import { fmtRate } from "./format";
import { fmtSpeedPercent } from "./varispeed";

// ---------------------------------------------------------------- ranges

export const DC_CUTOFF = { min: 1, max: 20, def: 5 } as const;
export const CROSSFEED_CUTOFF = { min: 300, max: 2000 } as const;
export const CROSSFEED_FEED = { min: 1, max: 15 } as const;
export const HARMONIC_DB = { min: -120, max: -20 } as const;
export const PITCH_CENTS = { min: -1200, max: 1200 } as const;
export const LIMITER_CEILING = { min: -6, max: 0, def: -0.5 } as const;
export const LIMITER_LOOKAHEAD = { min: 5, max: 20, def: 5 } as const;
export const LIMITER_RELEASE = { min: 10, max: 2000, def: 100 } as const;

const sign = (v: number, digits = 1) => (v > 0 ? "+" : v < 0 ? "−" : "") + Math.abs(v).toFixed(digits);
const trim = (v: number, digits = 1) => Number(v.toFixed(digits)).toString();

// ---------------------------------------------------------------- DC block

export function dcBlockOn(c: DcBlockConfig | null | undefined): boolean {
  return !!c && (c.enabled ?? true);
}

export function dcBlockSummary(c: DcBlockConfig | null | undefined): string {
  if (!dcBlockOn(c)) return "Off";
  return `${trim(c!.cutoff_hz)} Hz high-pass`;
}

/** Magnitude of the 2nd-order Butterworth high-pass at `f` (analog prototype; the engine prewarps), dB. */
export function dcBlockDb(cutoffHz: number, f: number): number {
  const r = Math.pow(f / cutoffHz, 4);
  return 10 * Math.log10(r / (1 + r));
}

// ---------------------------------------------------------------- pitch

export const PITCH_QUALITY: Record<PitchQuality, { label: string; latencyMs: number; title: string }> = {
  low: { label: "Low", latencyMs: 120, title: "Fastest: 120 ms window, 30 ms hop. Least CPU; can smear transients." },
  high: { label: "High", latencyMs: 120, title: "The default: 120 ms window, 15 ms hop. Runs as Low above 192 kHz." },
  max: { label: "Max", latencyMs: 240, title: "240 ms window: the smoothest tone, twice the delay. Runs as High above 96 kHz." },
};

/** The quality the engine actually runs at a source rate (it steps down at high rates). */
export function effectivePitchQuality(q: PitchQuality, rate: number): PitchQuality {
  if (q === "max" && rate > 96000) return effectivePitchQuality("high", rate);
  if (q === "high" && rate > 192000) return "low";
  return q;
}

/** Effective shift in cents (same rule as `PitchConfig::effective_cents`). */
export function pitchCents(p: PitchConfig | null | undefined): number {
  if (!p) return 0;
  if (p.reference_hz != null && p.target_hz != null && p.reference_hz > 0 && p.target_hz > 0) {
    return 1200 * Math.log2(p.target_hz / p.reference_hz);
  }
  if (p.semitones != null) return p.semitones * 100;
  return p.cents;
}

/** Whether the shifter changes the sound (the engine's `is_active`): on, with a shift. */
export function pitchActive(p: PitchConfig | null | undefined): boolean {
  return !!p && p.enabled && pitchCents(p) !== 0;
}

/** On with no shift: the engine runs it as a no-op, so the samples pass untouched. */
export function pitchIdle(p: PitchConfig | null | undefined): boolean {
  return !!p && p.enabled && pitchCents(p) === 0;
}

export function pitchUsesTuning(p: PitchConfig | null | undefined): boolean {
  return !!p && p.reference_hz != null && p.target_hz != null;
}

/** "+3 st −14 ¢", "−50 ¢", "0". */
export function fmtCents(cents: number): string {
  const c = Math.round(cents * 10) / 10;
  if (c === 0) return "0 ¢";
  const st = Math.trunc(c / 100);
  const rest = Math.round((c - st * 100) * 10) / 10;
  const parts: string[] = [];
  if (st !== 0) parts.push(`${st > 0 ? "+" : "−"}${Math.abs(st)} st`);
  if (rest !== 0) parts.push(`${rest > 0 ? "+" : "−"}${trim(Math.abs(rest))} ¢`);
  return parts.join(" ");
}

/** A shift as cents only (the other ways of stating it cleared). */
export function pitchWithCents(p: PitchConfig, cents: number): PitchConfig {
  const c = Math.max(PITCH_CENTS.min, Math.min(PITCH_CENTS.max, Math.round(cents * 10) / 10));
  return { ...p, cents: c, semitones: null, reference_hz: null, target_hz: null };
}

/** A shift as a tuning change, reference → target Hz. */
export function pitchWithTuning(p: PitchConfig, referenceHz: number, targetHz: number): PitchConfig {
  return { ...p, cents: 0, semitones: null, reference_hz: referenceHz, target_hz: targetHz };
}

/**
 * The same shift as a tuning change (reference 440 Hz), for the Tuning
 * editor. The shift and the on/off state stay exactly as they were.
 */
export function pitchAsTuning(p: PitchConfig): PitchConfig {
  if (pitchUsesTuning(p)) return p;
  const c = pitchCents(p);
  return { ...p, cents: 0, semitones: null, reference_hz: 440, target_hz: 440 * Math.pow(2, c / 1200) };
}

/** The same shift in cents, for the Cents editor; the on/off state stays. */
export function pitchAsCents(p: PitchConfig): PitchConfig {
  if (!pitchUsesTuning(p) && p.semitones == null) return p;
  // Rounded only to 1e-9 cents, which clears float noise from the Hz round trip
  // but keeps the shift that was playing (the field rounds for display only).
  const c = Math.max(PITCH_CENTS.min, Math.min(PITCH_CENTS.max, Math.round(pitchCents(p) * 1e9) / 1e9));
  return { ...p, cents: c, semitones: null, reference_hz: null, target_hz: null };
}

export function pitchSummary(p: PitchConfig | null | undefined): string {
  if (!p || !p.enabled) return "Off";
  const c = pitchCents(p);
  if (c === 0) return "On · no shift";
  const q = PITCH_QUALITY[p.quality]?.label ?? p.quality;
  if (pitchUsesTuning(p)) return `${trim(p.reference_hz!)} → ${trim(p.target_hz!)} Hz · ${q.toLowerCase()}`;
  return `${fmtCents(c)} · ${q.toLowerCase()}`;
}

/** The Pitch page's one switch reads on when either pitch shift or varispeed is on. */
export function pitchPageOn(p: PitchConfig | null | undefined, v: VarispeedConfig | null | undefined): boolean {
  return !!p?.enabled || !!v?.enabled;
}

/** The sidebar line for the Pitch page: the shift and the speed, when set. */
export function pitchPageSummary(p: PitchConfig | null | undefined, v: VarispeedConfig | null | undefined): string {
  if (!pitchPageOn(p, v)) return "Off";
  const parts: string[] = [];
  if (p?.enabled && pitchCents(p) !== 0) parts.push(pitchSummary(p));
  if (v?.enabled && Math.abs(v.speed - 1) > 1e-9) parts.push(`${parts.length ? "speed" : "Speed"} ${fmtSpeedPercent(v.speed)}`);
  return parts.length ? parts.join(" · ") : "On · no shift";
}

// ---------------------------------------------------------------- crossfeed

export const CROSSFEED_MODES: { mode: CrossfeedMode; label: string; title: string }[] = [
  { mode: "bs2b_default", label: "Default", title: "bs2b default: 700 Hz, 4.5 dB. A gentle, natural blend." },
  { mode: "bs2b_chu_moy", label: "Chu Moy", title: "bs2b Chu Moy: 700 Hz, 6 dB. Closer to speakers in a room." },
  { mode: "bs2b_jan_meier", label: "Jan Meier", title: "bs2b Jan Meier: 650 Hz, 9.5 dB. The lightest blend." },
  { mode: "linkwitz", label: "Linkwitz", title: "Linkwitz-style: 796 Hz, 6 dB, 0.3 ms delay at low frequencies. Mono stays exactly flat." },
  { mode: "bs2b_custom", label: "Custom", title: "Choose the cutoff and the feed level yourself." },
];

export const CROSSFEED_LEVELS: { level: CrossfeedLevel; label: string; title: string }[] = [
  {
    level: "matched",
    label: "Matched loudness",
    title:
      "Music sounds as loud with crossfeed on as off. Rare worst-case peaks can go up to 5.6 dB over full scale and clip, unless the limiter is on or the digital volume is turned down enough. Hardware volume does not help.",
  },
  {
    level: "peak_safe",
    label: "Peak-safe",
    title: "Lowered just enough that nothing can ever go over full scale, whatever the music. Plays 1.4 to 5.6 dB quieter than with crossfeed off.",
  },
];

/** Inter-channel correlation of the stereo reference the engine matches loudness for (`MATCHED_CORRELATION`). */
export const CROSSFEED_MATCHED_CORRELATION = 0.5;

export function defaultCrossfeed(): CrossfeedConfig {
  return { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, level: "matched" };
}

/**
 * The level policy. A config without `level` comes from an engine that
 * predates it, and those scale by the worst-case peak gain: `peak_safe`.
 */
export function crossfeedLevel(c: CrossfeedConfig): CrossfeedLevel {
  return c.level ?? "peak_safe";
}

/**
 * The config as the engine runs it: an engine known to lack the level
 * policy (`levelCap === false`) runs peak-safe whatever a draft says.
 */
export function crossfeedAsRun(c: CrossfeedConfig, levelCap: boolean | null): CrossfeedConfig {
  if (levelCap !== false) return c;
  const { level: _level, ...rest } = c;
  return rest as CrossfeedConfig;
}

/**
 * Choose the level policy, keeping the power state: with no config yet the
 * result is the default stage, off, with that level.
 */
export function withCrossfeedLevel(c: CrossfeedConfig | null | undefined, level: CrossfeedLevel): CrossfeedConfig {
  return { ...(c ?? { ...defaultCrossfeed(), enabled: false }), level };
}

/** Effective `(cutoff_hz, feed_db)` (same as `CrossfeedConfig::effective_params`). */
export function crossfeedParams(c: CrossfeedConfig): { cutoffHz: number; feedDb: number } {
  switch (c.mode) {
    case "bs2b_default":
      return { cutoffHz: 700, feedDb: 4.5 };
    case "bs2b_chu_moy":
      return { cutoffHz: 700, feedDb: 6 };
    case "bs2b_jan_meier":
      return { cutoffHz: 650, feedDb: 9.5 };
    case "linkwitz":
      return { cutoffHz: 796.4, feedDb: 6 };
    default:
      return { cutoffHz: c.cutoff_hz, feedDb: c.feed_db };
  }
}

export function crossfeedOn(c: CrossfeedConfig | null | undefined): boolean {
  return !!c && c.enabled;
}

export function crossfeedLabel(mode: CrossfeedMode): string {
  return CROSSFEED_MODES.find((m) => m.mode === mode)?.label ?? mode;
}

export function crossfeedSummary(c: CrossfeedConfig | null | undefined): string {
  if (!crossfeedOn(c)) return "Off";
  const { cutoffHz, feedDb } = crossfeedParams(c!);
  const safe = crossfeedLevel(c!) === "peak_safe" ? " · peak-safe" : "";
  return `${crossfeedLabel(c!.mode)} · ${Math.round(cutoffHz)} Hz, ${trim(feedDb)} dB${safe}`;
}

interface OnePole {
  b0: number;
  b1: number;
  a1: number;
}

/** Exact L1 norm of a stable first-order section (as dsp-core's `l1_norm`). */
function l1(c: OnePole): number {
  const p = -c.a1;
  return Math.abs(c.b0) + Math.abs(c.b1 + c.b0 * p) / (1 - Math.abs(p));
}

/** Direct and cross paths, unscaled (ports `bs2b_design` / `complementary_design`). */
function crossfeedDesign(c: CrossfeedConfig, fs: number): { direct: OnePole; cross: OnePole } {
  const { cutoffHz, feedDb } = crossfeedParams(c);
  if (c.mode === "linkwitz") {
    const fc = Math.min(cutoffHz, 0.49 * fs);
    const r = Math.pow(10, feedDb / 20);
    const gc = 1 / (1 + r);
    const gd = r / (1 + r);
    const k = Math.tan((Math.PI * fc) / fs);
    const inv = 1 / (k + 1);
    const a1 = (k - 1) * inv;
    return {
      cross: { b0: gc * k * inv, b1: gc * k * inv, a1 },
      direct: { b0: (gd * k + 1) * inv, b1: (gd * k - 1) * inv, a1 },
    };
  }
  const gbLo = (feedDb * -5) / 6 - 3;
  const gbHi = feedDb / 6 - 3;
  const gLo = Math.pow(10, gbLo / 20);
  const gHi = 1 - Math.pow(10, gbHi / 20);
  const fcHi = cutoffHz * Math.pow(2, (gbLo - 20 * Math.log10(gHi)) / 12);
  const x = Math.exp((-2 * Math.PI * cutoffHz) / fs);
  const xh = Math.exp((-2 * Math.PI * fcHi) / fs);
  return {
    cross: { b0: gLo * (1 - x), b1: 0, a1: -x },
    direct: { b0: 1 - gHi * (1 - xh), b1: -xh, a1: -xh },
  };
}

/** Complex response of a first-order section at `f`: `[re, im]`. */
function onePoleResponse(c: OnePole, f: number, fs: number): [number, number] {
  const w = (2 * Math.PI * f) / fs;
  const cr = Math.cos(w);
  const si = Math.sin(w);
  const nr = c.b0 + c.b1 * cr;
  const ni = -c.b1 * si;
  const dr = 1 + c.a1 * cr;
  const di = -c.a1 * si;
  const den = dr * dr + di * di;
  return [(nr * dr + ni * di) / den, (ni * dr - nr * di) / den];
}

/** IEC 61672 A-weighting power at `f` (1 at 1 kHz), as `loudness::a_weighting_db`. */
function aWeightPower(f: number): number {
  const f2 = f * f;
  const num = 12194 ** 2 * f2 * f2;
  const den = (f2 + 20.6 ** 2) * Math.sqrt((f2 + 107.7 ** 2) * (f2 + 737.9 ** 2)) * (f2 + 12194 ** 2);
  return Math.pow(10, (20 * Math.log10(num / den) + 1.9998584723051205) / 10);
}

/**
 * A-weighted, pink-referenced power gain of one output channel for stereo
 * input with inter-channel correlation `rho` (as dsp-core's
 * `analytic_offset_db` over `|H_d|² + |H_c|² + 2ρ·Re(H_d·conj H_c)`).
 */
function stereoLoudnessGain(direct: OnePole, cross: OnePole, fs: number, rho: number): number {
  const fMax = Math.min(0.5 * fs, 20000);
  const points = 2048;
  const u0 = Math.log(20);
  const du = (Math.log(fMax) - u0) / points;
  let ref = 0;
  let out = 0;
  for (let i = 0; i < points; i++) {
    const f = Math.exp(u0 + (i + 0.5) * du);
    const w = aWeightPower(f);
    const [dr, di] = onePoleResponse(direct, f, fs);
    const [cr, ci] = onePoleResponse(cross, f, fs);
    ref += w;
    out += w * (dr * dr + di * di + cr * cr + ci * ci + 2 * rho * (dr * cr + di * ci));
  }
  return out / ref;
}

/** The constant gain the stage applies to both paths (linear), per its level policy. */
function crossfeedScale(c: CrossfeedConfig, fs: number): number {
  const { direct, cross } = crossfeedDesign(c, fs);
  return crossfeedLevel(c) === "peak_safe" ? 1 / (l1(direct) + l1(cross)) : 1 / Math.sqrt(stereoLoudnessGain(direct, cross, fs, CROSSFEED_MATCHED_CORRELATION));
}

/** The constant gain (dB) the stage applies: about 0 for `matched`, −1.4 to −5.6 dB for `peak_safe`. */
export function crossfeedLevelDb(c: CrossfeedConfig, fs: number): number {
  return 20 * Math.log10(crossfeedScale(c, fs));
}

/** How far over the input's peak a worst-case signal can go, dB (0 for `peak_safe`). */
export function crossfeedPeakDb(c: CrossfeedConfig, fs: number): number {
  const { direct, cross } = crossfeedDesign(c, fs);
  return 20 * Math.log10(crossfeedScale(c, fs) * (l1(direct) + l1(cross)));
}

function onePoleDb(c: OnePole, f: number, fs: number, scale: number): number {
  const w = (2 * Math.PI * f) / fs;
  const cr = Math.cos(w);
  const si = Math.sin(w);
  const nr = c.b0 + c.b1 * cr;
  const ni = -c.b1 * si;
  const dr = 1 + c.a1 * cr;
  const di = -c.a1 * si;
  const m = Math.sqrt((nr * nr + ni * ni) / (dr * dr + di * di)) * scale;
  return 20 * Math.log10(Math.max(m, 1e-12));
}

/** Direct (own ear) and cross (other ear) path levels in dB at `freqs`, the stage's level included. */
export function crossfeedResponse(c: CrossfeedConfig, fs: number, freqs: readonly number[]): { direct: number[]; cross: number[] } {
  const { direct, cross } = crossfeedDesign(c, fs);
  const s = crossfeedScale(c, fs);
  return {
    direct: freqs.map((f) => onePoleDb(direct, f, fs, s)),
    cross: freqs.map((f) => onePoleDb(cross, f, fs, s)),
  };
}

// ---------------------------------------------------------------- harmonics

export const HARMONIC_CHARACTERS: { character: HarmonicsCharacter; label: string; title: string }[] = [
  { character: "triode", label: "Triode", title: "Warm: a strong 2nd harmonic (−40 dB), a faint 3rd (−58 dB) and 4th (−72 dB)." },
  { character: "pentode", label: "Pentode", title: "Edgier: a strong 3rd harmonic (−42 dB), a faint 2nd (−56 dB) and 5th (−66 dB)." },
  { character: "custom", label: "Custom", title: "Set each harmonic's level yourself." },
];

export function defaultHarmonics(): HarmonicsConfig {
  return { enabled: true, character: "triode", h2_db: -40, h3_db: -58, h4_db: null, h5_db: null, dc_block: true };
}

/** Effective `[h2, h3, h4, h5]` in dB (null = off), as `effective_levels_db`. */
export function harmonicLevels(c: HarmonicsConfig): [number, number, number | null, number | null] {
  if (c.character === "triode") return [-40, -58, -72, null];
  if (c.character === "pentode") return [-56, -42, null, -66];
  return [c.h2_db, c.h3_db, c.h4_db ?? null, c.h5_db ?? null];
}

/** THD of a full-scale sine through the stage, % (levels are exact at full scale). */
export function harmonicsThdPercent(c: HarmonicsConfig): number {
  const p = harmonicLevels(c).reduce<number>((sum, db) => (db === null ? sum : sum + Math.pow(10, db / 10)), 0);
  return 100 * Math.sqrt(p);
}

/**
 * Level change at full scale, dB (≤ 0): the stage scales its output so a
 * full-scale input stays within full scale. A grid estimate of the
 * engine's certified bound (its DC blocker's first tap taken as 1).
 */
export function harmonicsLevelDb(c: HarmonicsConfig): number {
  const [h2, h3, h4, h5] = harmonicLevels(c);
  const a2 = Math.pow(10, h2 / 20);
  const a3 = -Math.pow(10, h3 / 20);
  const a4 = h4 === null ? 0 : Math.pow(10, h4 / 20);
  const a5 = h5 === null ? 0 : Math.pow(10, h5 / 20);
  let maxO = -Infinity;
  let minO = Infinity;
  let maxE = -Infinity;
  let minE = Infinity;
  let maxAll = 0;
  const N = 4096;
  for (let i = 0; i <= N; i++) {
    const x = -1 + (2 * i) / N;
    const x2 = x * x;
    const t2 = 2 * x2 - 1;
    const t3 = 4 * x2 * x - 3 * x;
    const t4 = 8 * x2 * x2 - 8 * x2 + 1;
    const t5 = 16 * x2 * x2 * x - 20 * x2 * x + 5 * x;
    const e = a2 * (t2 + 1) + a4 * (t4 - 1);
    const o = x + a3 * t3 + a5 * t5;
    maxO = Math.max(maxO, o + e);
    minO = Math.min(minO, o + e);
    maxE = Math.max(maxE, e);
    minE = Math.min(minE, e);
    maxAll = Math.max(maxAll, Math.abs(o + e));
  }
  const bound = c.dc_block ? Math.max(maxO - minE, maxE - minO) : maxAll;
  return bound > 1 ? -20 * Math.log10(bound) : 0;
}

export function harmonicsOn(c: HarmonicsConfig | null | undefined): boolean {
  return !!c && c.enabled;
}

export function harmonicsSummary(c: HarmonicsConfig | null | undefined): string {
  if (!harmonicsOn(c)) return "Off";
  const label = HARMONIC_CHARACTERS.find((x) => x.character === c!.character)?.label ?? c!.character;
  return `${label} · THD ${fmtPercent(harmonicsThdPercent(c!))}`;
}

export function fmtPercent(p: number): string {
  if (p >= 1) return `${p.toFixed(2)}%`;
  if (p >= 0.01) return `${p.toFixed(3)}%`;
  return `${p.toPrecision(2)}%`;
}

// ---------------------------------------------------------------- limiter

export const LIMITER_MODES: { mode: LimiterMode; label: string; title: string }[] = [
  { mode: "off", label: "Off", title: "No limiter: samples above full scale clip in the quantizer." },
  {
    mode: "limit",
    label: "Limit",
    title: "Hold peaks at the ceiling: the gain dips briefly and recovers with the release time. Not bit-perfect.",
  },
  {
    mode: "auto_headroom",
    label: "Auto headroom",
    title: "Learn how much the music needs and lower the level in 0.5 dB steps that stay for the rest of the stream (no pumping); the limiter catches what is left. Not bit-perfect.",
  },
];

export function defaultLimiter(): LimiterConfig {
  return { mode: "auto_headroom", ceiling_dbfs: LIMITER_CEILING.def, lookahead_ms: LIMITER_LOOKAHEAD.def, release_ms: LIMITER_RELEASE.def };
}

/** The Limiter line: mode, ceiling, and the fixed output headroom when there is one ("Off · −3 dB"). */
export function limiterSummary(c: LimiterConfig | null | undefined, headroomDb = 0): string {
  const cut = headroomDb > 0 ? ` · ${sign(-headroomDb, Number.isInteger(headroomDb) ? 0 : 1)} dB` : "";
  if (!c || c.mode === "off") return `Off${cut}`;
  const label = LIMITER_MODES.find((m) => m.mode === c.mode)?.label ?? c.mode;
  return `${label} · ${sign(c.ceiling_dbfs)} dBTP${cut}`;
}

/** The look-ahead delay the engine adds at an output rate: whole frames, rounded up. */
export function limiterDelay(lookaheadMs: number, outputRate: number): { frames: number; ms: number } {
  if (!(outputRate > 0)) return { frames: 0, ms: lookaheadMs };
  const frames = Math.ceil((outputRate * lookaheadMs) / 1000 - 1e-9);
  return { frames, ms: (frames / outputRate) * 1000 };
}

/** "onsets" in limit mode, "headroom steps" in auto mode. */
export function limiterEventLabel(mode: LimiterMode): string {
  return mode === "auto_headroom" ? "headroom steps" : "onsets";
}

// ---------------------------------------------------------------- dither

export interface DitherInfo {
  kind: DitherKind;
  label: string;
  group: "dither" | "shaping";
  text: string;
  /** Lowest output rate the mode is meant for, Hz, or null for any. */
  minRate: number | null;
  /** Rate advice in words, or null. */
  rateNote: string | null;
}

export const DITHERS: DitherInfo[] = [
  { kind: "none", label: "Off", group: "dither", text: "Plain rounding: the rounding error stays tied to the signal (low-level distortion).", minRate: null, rateNote: null },
  { kind: "tpdf", label: "TPDF", group: "dither", text: "Triangular dither: flat noise that does not follow the music. The safe default.", minRate: null, rateNote: null },
  { kind: "headroom_tpdf", label: "TPDF + headroom", group: "dither", text: "Triangular dither with 1 LSB of headroom kept free, so rounding never clips.", minRate: null, rateNote: null },
  { kind: "rectangular", label: "Rectangular", group: "dither", text: "Uniform dither: simple, with a higher noise floor than TPDF.", minRate: null, rateNote: null },
  { kind: "gaussian", label: "Gaussian", group: "dither", text: "Gaussian dither: quieter in silence, but the error depends a little on the signal.", minRate: null, rateNote: null },
  {
    kind: "highpass_tpdf",
    label: "High-pass TPDF",
    group: "dither",
    text: "Triangular dither with its noise moved up in frequency (the rounding error itself is not shaped).",
    minRate: 88200,
    rateNote: "Best at 88.2 kHz or higher.",
  },
  {
    kind: "shaped",
    label: "Classic shaped",
    group: "shaping",
    text: "The original noise shaper: adapts to the rate and pushes noise where hearing is least sensitive. Plain TPDF below 44.1 kHz. Most useful at 16 bits.",
    minRate: null,
    rateNote: null,
  },
  { kind: "feedback1", label: "Gentle", group: "shaping", text: "One-tap error feedback: a gentle cut of noise in the audio band.", minRate: 88200, rateNote: "Best at 88.2 kHz or higher." },
  { kind: "feedback5", label: "Broad", group: "shaping", text: "Five-tap error feedback: a broad cut of noise in the audio band (about 9 dB at 352.8 kHz).", minRate: 88200, rateNote: "Best at 88.2 kHz or higher." },
  {
    kind: "resonant5",
    label: "Resonant 5",
    group: "shaping",
    text: "Fifth-order shaper with two resonators: a deep cut in the audio band at very high rates.",
    minRate: 352800,
    rateNote: "Needs 352.8 kHz or higher, strongest at 705.6 kHz; at lower rates it adds audible noise.",
  },
  {
    kind: "resonant9",
    label: "Resonant 9",
    group: "shaping",
    text: "Ninth-order shaper with two resonators: about 24 dB less audio-band noise at 352.8 kHz.",
    minRate: 352800,
    rateNote: "Needs 352.8 kHz or higher; at 176.4 kHz it leaves a noise peak near 18 kHz.",
  },
  {
    kind: "integrator15",
    label: "Deep 15",
    group: "shaping",
    text: "Fifteenth-order shaper: the deepest audio-band cut (about 48 dB at 705.6 kHz), 16 LSB of headroom.",
    minRate: 352800,
    rateNote: "Needs 352.8 kHz or higher, preferably 705.6 kHz; below that it raises audible noise.",
  },
];

export function ditherInfo(kind: DitherKind): DitherInfo | undefined {
  return DITHERS.find((d) => d.kind === kind);
}

/** Advice when the output runs below the rate a mode is made for, or null. */
export function ditherRateWarning(kind: DitherKind, outputRate: number | null | undefined): string | null {
  const d = ditherInfo(kind);
  if (!d?.minRate || !outputRate || outputRate >= d.minRate) return null;
  return `${d.rateNote} The output runs at ${fmtRate(outputRate)} now.`;
}

// ---------------------------------------------------------------- the chain

/** Whether each new stage is doing anything (for the sidebar power buttons). */
export function stagesOn(chain: ChainConfig) {
  return {
    dc_block: dcBlockOn(chain.dc_block),
    // The power button shows `enabled`: on at 0 cents is on, as a no-op.
    pitch: !!chain.pitch?.enabled,
    crossfeed: crossfeedOn(chain.crossfeed),
    harmonics: harmonicsOn(chain.harmonics),
    limiter: !!chain.limiter && chain.limiter.mode !== "off",
  };
}

export { sign as fmtSigned };

// ---------------------------------------------------------------- telemetry

/**
 * State text for the Limiter view's gain-reduction meter.
 *
 * The engine's `MeterBank` retains `limiter.active` from the last processed
 * block after playback stops, so pair it with `status.state === "playing"`
 * (when status is known) to distinguish audio flowing through the limiter
 * from stopped/idle states.
 */
export function limiterMeterState(
  lim: LimiterStatus | undefined,
  engineState: EngineState | undefined,
): { inPath: boolean; text: string } {
  if (!lim) return { inPath: false, text: "No reading yet" };
  const playing = engineState === undefined ? true : engineState === "playing";
  if (!lim.active || !playing) {
    return { inPath: false, text: "Not in the path (Direct, bypass or nothing playing)" };
  }
  const gr = Math.max(0, lim.gain_reduction_db);
  return {
    inPath: true,
    text: gr > 0.05 ? "Reducing" : "Playing, below the ceiling: no reduction",
  };
}

/** The largest value pushed within the last `windowMs` (a real rolling maximum). */
export class RollingMax {
  private items: { t: number; v: number }[] = [];
  constructor(private windowMs: number) {}
  push(t: number, v: number) {
    // Older values no larger than this one can never be the maximum again.
    while (this.items.length && this.items[this.items.length - 1].v <= v) this.items.pop();
    this.items.push({ t, v });
  }
  max(now: number): number {
    while (this.items.length && now - this.items[0].t > this.windowMs) this.items.shift();
    return this.items.length ? this.items[0].v : 0;
  }
}
