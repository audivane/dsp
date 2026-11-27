// Pure helpers for the effect stages (DC block, pitch, crossfeed, harmonics,
// limiter) and the dither list: defaults, sidebar summaries, and the small
// pieces of the engine's maths the views show (crossfeed level and
// response, harmonics layers, DC-block response). Ranges and formulas follow
// docs/control-plane.md and crates/dsp-core.

import type {
  ChainConfig,
  CrossfeedConfig,
  CrossfeedMode,
  StereoWidthShape,
  DcBlockConfig,
  DitherKind,
  EngineState,
  ChebyshevLayer,
  HarmonicSign,
  HarmonicsBias,
  HarmonicsConfig,
  HarmonicsLayer,
  HysteresisBassEq,
  HarmonicsPreset,
  HarmonicsSystem,
  HarmonicsReadoutPoint,
  HarmonicsSag,
  HarmonicsTilt,
  OrderTilt,
  TiltShelf,
  LimiterConfig,
  LimiterMode,
  LimiterStatus,
  PitchConfig,
  PitchQuality,
  VarispeedConfig,
} from "../api/types";
import { fmtRate } from "./format";
import { fmtSpeedPercent } from "./varispeed";
import presetData from "../../../crates/protocol/src/harmonics_presets.json";

// ---------------------------------------------------------------- ranges

export const DC_CUTOFF = { min: 1, max: 20, def: 5 } as const;
export const CROSSFEED_CUTOFF = { min: 300, max: 2000 } as const;
export const CROSSFEED_FEED = { min: 1, max: 15 } as const;
/** Stereo width: the side gain `k` (1 = unchanged). */
export const STEREO_WIDTH = { min: 0, max: 2 } as const;
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
  low: { label: "Low", latencyMs: 120, title: "Fastest: 120 ms window, 30 ms hop, least CPU. Adds an audible warble on pure tones (faint sidebands about 33 Hz either side, around −50 dB, and a few cents of pitch drift) and can smear transients. Fine for speech or a quick preview; High and Max are far cleaner." },
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
  { mode: "none", label: "None", title: "No crossfeed: only the width acts." },
  { mode: "bs2b_default", label: "Default", title: "bs2b default: 700 Hz, 4.5 dB. A gentle, natural blend." },
  { mode: "bs2b_chu_moy", label: "Chu Moy", title: "bs2b Chu Moy: 700 Hz, 6 dB. Closer to speakers in a room." },
  { mode: "bs2b_jan_meier", label: "Jan Meier", title: "bs2b Jan Meier: 650 Hz, 9.5 dB. The lightest blend." },
  { mode: "linkwitz", label: "Linkwitz", title: "Linkwitz-style: 796 Hz, 6 dB, 0.3 ms delay at low frequencies. Mono stays exactly flat." },
  { mode: "bs2b_custom", label: "Custom", title: "Choose the cutoff and the feed level yourself." },
];

/** Inter-channel correlation of the stereo reference the engine matches loudness for (`MATCHED_CORRELATION`). */
export const CROSSFEED_MATCHED_CORRELATION = 0.5;

export function defaultCrossfeed(): CrossfeedConfig {
  return { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, width: 1, width_shape: "flat", width_corner_hz: 700 };
}

/** The width's named steps: Mono, two narrower, unchanged, two wider. */
export const STEREO_WIDTH_DETENTS: { value: number; label: string }[] = [
  { value: 0, label: "Mono" },
  { value: 0.5, label: "−2" },
  { value: 0.75, label: "−1" },
  { value: 1, label: "0" },
  { value: 1.25, label: "+1" },
  { value: 1.5, label: "+2" },
];

export const STEREO_WIDTH_SHAPES: { shape: StereoWidthShape; label: string; title: string }[] = [
  { shape: "flat", label: "Flat", title: "The same width at every frequency, as on an M/S side level control." },
  { shape: "above_bass", label: "Above the bass", title: "The bass keeps its width; the width acts above about 700 Hz. Spares the ears inverted, wide bass." },
];

/** Side gain `k` of a config (an engine before the width has none: 1). */
export function stereoWidth(c: CrossfeedConfig): number {
  return c.width ?? 1;
}

/** The detent's name for `k` ("+1", "Mono"), else the number. */
export function stereoWidthLabel(k: number): string {
  return STEREO_WIDTH_DETENTS.find((d) => Math.abs(d.value - k) < 1e-9)?.label ?? k.toFixed(2);
}

/** The side gain in dB ("+1.9 dB"; "−∞ dB" for mono). */
export function stereoWidthSideDb(k: number): string {
  return k <= 0 ? "−∞ dB" : `${sign(20 * Math.log10(k), 1)} dB`;
}

/** True when the stage changes the signal (as `CrossfeedConfig::is_active`). */
export function crossfeedActive(c: CrossfeedConfig | null | undefined): boolean {
  return !!c && c.enabled && (c.mode !== "none" || stereoWidth(c) !== 1);
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
  const k = stereoWidth(c!);
  const width = k === 1 ? "" : k === 0 ? "Mono" : `Width ${stereoWidthLabel(k)}`;
  const shaped = width && c!.width_shape === "above_bass" ? `${width} above bass` : width;
  if (c!.mode === "none") return shaped ? shaped : "No crossfeed, width 0";
  const xf = `${crossfeedLabel(c!.mode)} · ${Math.round(cutoffHz)} Hz, ${trim(feedDb)} dB`;
  return shaped ? `${xf} · ${shaped}` : xf;
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
  if (c.mode === "none") return { direct: { b0: 1, b1: 0, a1: 0 }, cross: { b0: 0, b1: 0, a1: 0 } };
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

type Cx = [number, number];

/**
 * The whole stage as dsp-core builds it (`Kernel`): the crossfeed paths
 * scaled by `scale`, then the width `u = g·d` (or `g·HP₁(d)`), `L′ = L + u`.
 */
interface XfStage {
  direct: OnePole;
  cross: OnePole;
  crossfeed: boolean;
  scale: number;
  g: number;
  hp: OnePole | null;
}

function highPass(fc: number, fs: number): OnePole {
  const k = Math.tan((Math.PI * Math.min(fc, 0.49 * fs)) / fs);
  const inv = 1 / (k + 1);
  return { b0: inv, b1: -inv, a1: (k - 1) * inv };
}

function stageOf(c: CrossfeedConfig, fs: number, scale: number): XfStage {
  const { direct, cross } = crossfeedDesign(c, fs);
  const s = (p: OnePole): OnePole => ({ b0: p.b0 * scale, b1: p.b1 * scale, a1: p.a1 });
  return {
    direct: s(direct),
    cross: s(cross),
    crossfeed: c.mode !== "none",
    scale,
    g: (stereoWidth(c) - 1) / 2,
    hp: c.width_shape === "above_bass" ? highPass(c.width_corner_hz ?? 700, fs) : null,
  };
}

/** Direct and cross responses of the stage at `f`: `D′ = d + w`, `C′ = c − w`, `w = g·H·(d − c)`. */
function stagePaths(st: XfStage, f: number, fs: number): [Cx, Cx] {
  const d: Cx = st.crossfeed ? onePoleResponse(st.direct, f, fs) : [st.scale, 0];
  const c: Cx = st.crossfeed ? onePoleResponse(st.cross, f, fs) : [0, 0];
  if (st.g === 0) return [d, c];
  const [hr, hi] = st.hp ? onePoleResponse(st.hp, f, fs) : [1, 0];
  const er = d[0] - c[0];
  const ei = d[1] - c[1];
  const wr = st.g * (hr * er - hi * ei);
  const wi = st.g * (hr * ei + hi * er);
  return [
    [d[0] + wr, d[1] + wi],
    [c[0] - wr, c[1] - wi],
  ];
}

/** Exact worst-case peak gain of the stage, summed from its impulse response (as `Kernel::l1_bound`). */
function stageL1(st: XfStage): number {
  let p = 0;
  if (st.crossfeed) p = Math.max(Math.abs(st.direct.a1), Math.abs(st.cross.a1));
  if (st.hp && st.g !== 0) p = Math.max(p, Math.abs(st.hp.a1));
  const n = p > 0 ? Math.min(1e7, Math.max(1, Math.ceil(Math.log(1e-20) / Math.log(p)))) : 1;
  const z = [0, 0, 0, 0, 0];
  const step = (c: OnePole, i: number, x: number) => {
    const y = c.b0 * x + z[i];
    z[i] = c.b1 * x - c.a1 * y;
    return y;
  };
  let sum = 0;
  for (let i = 0; i <= n; i++) {
    const xl = i === 0 ? 1 : 0;
    let yl = xl * st.scale;
    let yr = 0;
    if (st.crossfeed) {
      yl = step(st.direct, 0, xl) + step(st.cross, 1, 0);
      yr = step(st.direct, 2, 0) + step(st.cross, 3, xl);
    }
    if (st.g !== 0) {
      const d = st.hp ? step(st.hp, 4, yl - yr) : yl - yr;
      yl += st.g * d;
      yr -= st.g * d;
    }
    sum += Math.abs(yl) + Math.abs(yr);
  }
  return sum;
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
function stereoLoudnessGain(st: XfStage, fs: number, rho: number): number {
  const fMax = Math.min(0.5 * fs, 20000);
  const points = 2048;
  const u0 = Math.log(20);
  const du = (Math.log(fMax) - u0) / points;
  let ref = 0;
  let out = 0;
  for (let i = 0; i < points; i++) {
    const f = Math.exp(u0 + (i + 0.5) * du);
    const w = aWeightPower(f);
    const [[dr, di], [cr, ci]] = stagePaths(st, f, fs);
    ref += w;
    out += w * (dr * dr + di * di + cr * cr + ci * ci + 2 * rho * (dr * cr + di * ci));
  }
  return out / ref;
}

/**
 * The constant gain the stage applies (linear): the crossfeed part's
 * loudness match (1 without crossfeed; the width is never compensated).
 * No cut for safety: the limiter and the output headroom handle the peaks.
 */
function crossfeedScale(c: CrossfeedConfig, fs: number): number {
  const unscaled = stageOf(c, fs, 1);
  if (!unscaled.crossfeed) return 1;
  return 1 / Math.sqrt(stereoLoudnessGain({ ...unscaled, g: 0 }, fs, CROSSFEED_MATCHED_CORRELATION));
}

/** The constant gain (dB) the stage applies: about 0 (its loudness match). */
export function crossfeedLevelDb(c: CrossfeedConfig, fs: number): number {
  return 20 * Math.log10(crossfeedScale(c, fs));
}

/** How far over the input's peak a worst-case signal can go, dB, width included. */
export function crossfeedPeakDb(c: CrossfeedConfig, fs: number): number {
  const st = stageOf(c, fs, crossfeedScale(c, fs));
  return 20 * Math.log10(st.g === 0 ? l1(st.direct) + l1(st.cross) : stageL1(st));
}

/** The stage's loudness change (dB) on typical stereo music (pink noise, correlation 0.5, A-weighted), width and level included. */
export function crossfeedLoudnessDb(c: CrossfeedConfig, fs: number): number {
  return 10 * Math.log10(stereoLoudnessGain(stageOf(c, fs, crossfeedScale(c, fs)), fs, CROSSFEED_MATCHED_CORRELATION));
}

const cxDb = ([re, im]: Cx) => 20 * Math.log10(Math.max(Math.hypot(re, im), 1e-12));

/** Direct (own ear) and cross (other ear) path levels in dB at `freqs`, the stage's level and width included. */
export function crossfeedResponse(c: CrossfeedConfig, fs: number, freqs: readonly number[]): { direct: number[]; cross: number[] } {
  const st = stageOf(c, fs, crossfeedScale(c, fs));
  const paths = freqs.map((f) => stagePaths(st, f, fs));
  return { direct: paths.map((p) => cxDb(p[0])), cross: paths.map((p) => cxDb(p[1])) };
}

// ---------------------------------------------------------------- harmonics

/** Ranges of the layer parameters (docs/control-plane.md). */
export const HARMONIC_DRIVE = { min: -40, max: 0 } as const;
export const FLUX_CORNER = { min: 1, max: 5000 } as const;
export const FLUX_BOOST = { min: 0, max: 40 } as const;
export const FLUX_H3 = { min: -120, max: -12 } as const;
export const FLUX_CAL = { min: 5, max: 1000 } as const;
export const HYSTERESIS_H3 = { min: -120, max: -12 } as const;
export const HYSTERESIS_CAL = { min: 20, max: 5000 } as const;
export const HYSTERESIS_MIN_CLEAN_RATE_HZ = 176400;
/** Hysteresis bass EQ pair (corner Hz, boost into the core dB) and field offset (in units of the core's saturation knee). */
export const HYSTERESIS_EQ_HZ = { min: 20, max: 500 } as const;
export const HYSTERESIS_EQ_DB = { min: 0, max: 12 } as const;
export const HYSTERESIS_ASYMMETRY = { min: -1, max: 1 } as const;
/** The most layers a stage holds. */
export const HARMONIC_LAYERS_MAX = 8;
/** Tilt shelf ranges: corners as fundamental frequencies, Hz; rise in dB. */
export const TILT_LOW_HZ = { min: 20, max: 2000 } as const;
export const TILT_HIGH_HZ = { min: 200, max: 20000 } as const;
export const TILT_DB = { min: -24, max: 24 } as const;
/** Tilt shelf slopes on offer, dB/oct (shelf order 1–4), and each shelf's slope when `slope` is omitted. */
export const TILT_SLOPES = [6, 12, 18, 24] as const;
export const TILT_LOW_DEFAULT_SLOPE = 12;
export const TILT_HIGH_DEFAULT_SLOPE = 6;

/** `s` at slope `v` dB/oct; the default slope is stored as no `slope` at all. */
export function withShelfSlope(s: TiltShelf, v: number, defaultSlope: number): TiltShelf {
  const { slope: _old, ...rest } = s;
  return v === defaultSlope ? rest : { ...rest, slope: v };
}
/** Sag and bias ranges: sag depth in dB, bias depth as a fraction of full scale, envelope times in ms. */
export const SAG_DB = { min: 0, max: 6 } as const;
export const BIAS_DEPTH = { min: -0.25, max: 0.25 } as const;
export const ENVELOPE_MS = { min: 0.1, max: 10000 } as const;

/** Default signs for H2..H9: H3 negative (compressive), the others positive. */
export const HARMONIC_DEFAULT_SIGNS: ChebyshevLayer["signs"] = ["+", "-", "+", "+", "+", "+", "+", "+"];

/** The preset a new stage starts from (as the engine's `HarmonicsConfig::default()`). */
export const DEFAULT_HARMONICS_PRESET = "se_triode_high_mu";

export function defaultHarmonics(): HarmonicsConfig {
  const p = (presetData as unknown as HarmonicsPreset[]).find((x) => x.id === DEFAULT_HARMONICS_PRESET);
  if (!p) throw new Error(`missing harmonics preset ${DEFAULT_HARMONICS_PRESET}`);
  return { enabled: true, dc_block: true, layers: [layerFromPreset(p)] };
}

/** A new layer from a preset: its parameters, on, with `preset` set. */
export function layerFromPreset(p: HarmonicsPreset): HarmonicsLayer {
  return { ...structuredClone(p.layer), enabled: true, preset: p.id } as HarmonicsLayer;
}

/**
 * The stage a system preset stands for: its layers (each an ordinary layer
 * from its layer preset, on) and routing, remembering the system (`system`,
 * the editor's baseline); `enabled` as given, DC blocker on.
 */
export function configFromSystem(s: HarmonicsSystem, enabled = true): HarmonicsConfig {
  const layers = s.layers.map((l) => ({ ...structuredClone(l), enabled: true }) as HarmonicsLayer);
  return s.routing && s.routing !== "parallel" ? { enabled, dc_block: true, layers, routing: s.routing, system: s.id } : { enabled, dc_block: true, layers, system: s.id };
}

/** The system the stage was started from (undefined: built by hand, or one this engine does not list). */
export function stageSystem(c: HarmonicsConfig, systems: readonly HarmonicsSystem[] | null | undefined): HarmonicsSystem | undefined {
  return c.system ? systems?.find((s) => s.id === c.system) : undefined;
}

/**
 * The system's version of each layer of `c` (undefined for a layer added
 * by hand): layers pair with the system's by preset, in order, so moving or
 * removing one keeps the others paired.
 */
export function systemLayers(c: HarmonicsConfig, system: HarmonicsSystem | undefined): (HarmonicsLayer | undefined)[] {
  if (!system) return c.layers.map(() => undefined);
  const free: (HarmonicsLayer | undefined)[] = [...system.layers];
  return c.layers.map((l) => {
    const i = free.findIndex((x) => x !== undefined && x.kind === l.kind && (x.preset ?? null) === (l.preset ?? null));
    if (i < 0) return undefined;
    const x = free[i];
    free[i] = undefined;
    return x;
  });
}

/** Whether a layer differs from `base` (its system's version), parameters only. */
export function layerDiffers(l: HarmonicsLayer, base: HarmonicsLayer): boolean {
  return JSON.stringify(layerParams(l)) !== JSON.stringify(layerParams(base));
}

/** Whether the stage differs from the system it was started from: its layers (which, in what order, on or off, their settings), routing or DC blocker. */
export function stageModified(c: HarmonicsConfig, system: HarmonicsSystem): boolean {
  const routing = (r: HarmonicsConfig["routing"]) => r ?? "parallel";
  if (routing(c.routing) !== routing(system.routing) || !c.dc_block || c.layers.length !== system.layers.length) return true;
  return c.layers.some((l, i) => {
    const b = system.layers[i];
    return !l.enabled || l.kind !== b.kind || (l.preset ?? null) !== (b.preset ?? null) || layerDiffers(l, b);
  });
}

/** The layer presets a system chains, by name (an unknown id as itself), e.g. "Single-ended triode, high-mu → Output transformer". */
export function systemChain(s: HarmonicsSystem, presets: readonly HarmonicsPreset[]): string {
  return s.layers.map((l) => (l.preset ? (presets.find((p) => p.id === l.preset)?.name ?? l.preset) : LAYER_KINDS[l.kind].tag)).join(s.routing && s.routing !== "parallel" ? " → " : " + ");
}

/** A flat tilt (both shelves at 0 dB, low corner 100 Hz, high corner 2 kHz), as the engine's default. */
export function defaultTilt(): HarmonicsTilt {
  return { low: { hz: 100, db: 0 }, high: { hz: 2000, db: 0 } };
}

/** The shelves order `k` (2..9) gets: its override, else the layer's (low shelf on odd orders unless `low_even`). */
export function orderShelves(t: HarmonicsTilt, k: number): OrderTilt {
  const o = t.orders?.[k - 2];
  if (o) return o;
  return { low: { ...t.low, db: k % 2 === 1 || t.low_even ? t.low.db : 0 }, high: t.high };
}

/** Whether every order's shelves are at 0 dB (sounds as no tilt). */
export function tiltFlat(t: HarmonicsTilt | null | undefined): boolean {
  if (!t) return true;
  for (let k = 2; k <= 9; k++) {
    const o = orderShelves(t, k);
    if (o.low.db !== 0 || o.high.db !== 0) return false;
  }
  return true;
}

/** Sag at depth 0 with the engine's default times: what a layer without sag shows. */
export function defaultSag(): HarmonicsSag {
  return { depth_db: 0, attack_ms: 20, release_ms: 200 };
}

/** Bias at depth 0 with the engine's default times. */
export function defaultBias(): HarmonicsBias {
  return { depth: 0, attack_ms: 300, release_ms: 2000 };
}

export const sagOn = (s: HarmonicsSag | null | undefined): boolean => !!s && s.depth_db > 0;
export const biasOn = (b: HarmonicsBias | null | undefined): boolean => !!b && b.depth !== 0;

/** Whether a Chebyshev layer has sag or bias set (a depth other than 0), whatever the amount. */
export function chebyshevHasDynamics(l: ChebyshevLayer): boolean {
  return sagOn(l.sag) || biasOn(l.bias);
}

/** Whether sag or bias is set and the amount above 0 (both depths scale with it): the layer runs on its own. */
export function chebyshevDynamic(l: ChebyshevLayer): boolean {
  return l.amount > 0 && chebyshevHasDynamics(l);
}

/** Whether the layer is a Chebyshev layer with sag or bias on. */
export function layerDynamic(l: HarmonicsLayer): boolean {
  return l.kind === "chebyshev" && chebyshevDynamic(l);
}

/** `c` with the sag of Chebyshev layer `i` replaced by `fn` of it (depth 0 when it has none). */
export function withSag(c: HarmonicsConfig, i: number, fn: (s: HarmonicsSag) => HarmonicsSag): HarmonicsConfig {
  return withLayer(c, i, (l) => (l.kind === "chebyshev" ? { ...l, sag: fn(l.sag ?? defaultSag()) } : l));
}

/** `c` with the bias of Chebyshev layer `i` replaced by `fn` of it (depth 0 when it has none). */
export function withBias(c: HarmonicsConfig, i: number, fn: (b: HarmonicsBias) => HarmonicsBias): HarmonicsConfig {
  return withLayer(c, i, (l) => (l.kind === "chebyshev" ? { ...l, bias: fn(l.bias ?? defaultBias()) } : l));
}

/** The tilt with defaults filled in for comparing (`null` when flat). */
function tiltParams(t: HarmonicsTilt | null | undefined): unknown {
  if (!t || tiltFlat(t)) return null;
  // A 0 dB shelf's slope changes nothing.
  const shelf = (s: TiltShelf, d: number) => ({ hz: s.hz, db: s.db, slope: s.db === 0 ? null : (s.slope ?? d) });
  const pair = (o: OrderTilt) => ({ low: shelf(o.low, TILT_LOW_DEFAULT_SLOPE), high: shelf(o.high, TILT_HIGH_DEFAULT_SLOPE) });
  return { ...pair(t), low_even: !!t.low_even, orders: Array.from({ length: 8 }, (_, i) => { const o = t.orders?.[i]; return o ? pair(o) : null; }) };
}

/** The layer's parameters only (no `enabled`, no `preset`), with defaults filled in, for comparing. */
function layerParams(l: HarmonicsLayer): unknown {
  if (l.kind === "chebyshev") {
    return {
      kind: l.kind,
      levels_db: l.levels_db.map((x) => x ?? null),
      signs: [...l.signs],
      invert_even: !!l.invert_even,
      drive_db: l.drive_db,
      amount: l.amount,
      tilt: tiltParams(l.tilt),
      // Depth 0 sounds as none.
      sag: sagOn(l.sag) ? l.sag : null,
      bias: biasOn(l.bias) ? l.bias : null,
    };
  }
  if (l.kind === "flux") {
    return { kind: l.kind, corner_hz: l.corner_hz, max_boost_db: l.max_boost_db, target_h3_db: l.target_h3_db, calibration_hz: l.calibration_hz };
  }
  return {
    kind: l.kind,
    saturation: l.saturation,
    width: l.width,
    amount: l.amount,
    target_h3_db: l.target_h3_db,
    calibration_hz: l.calibration_hz,
    // Switched off, or at 0 dB / 0, sounds as none.
    bass_eq: hysteresisEqOn(l.bass_eq) ? { hz: l.bass_eq!.hz, db: l.bass_eq!.db } : null,
    asymmetry: l.asymmetry ? l.asymmetry : null,
  };
}

/** Whether a hysteresis layer's bass EQ pair changes the sound. */
export function hysteresisEqOn(eq: HysteresisBassEq | null | undefined): boolean {
  // `enabled` defaults to true (the preset data leaves it out).
  return !!eq && eq.enabled !== false && eq.db !== 0;
}

/** The bass EQ a hysteresis layer starts from when switched on (as the studio tape preset's). */
export function defaultHysteresisEq(): HysteresisBassEq {
  return { enabled: true, hz: 110, db: 3.5 };
}

/**
 * Whether the layer differs from the preset it started from (`null`: it
 * names no preset, or one this engine does not list).
 */
export function layerModified(l: HarmonicsLayer, presets: readonly HarmonicsPreset[]): boolean | null {
  const p = l.preset ? presets.find((x) => x.id === l.preset) : undefined;
  if (!p) return null;
  return JSON.stringify(layerParams(l)) !== JSON.stringify(layerParams(p.layer));
}

/** The layer back at its preset's parameters (kept on or off); unchanged when it has none. */
export function resetLayer(l: HarmonicsLayer, presets: readonly HarmonicsPreset[]): HarmonicsLayer {
  const p = l.preset ? presets.find((x) => x.id === l.preset) : undefined;
  return p ? { ...layerFromPreset(p), enabled: l.enabled } : l;
}

/** Per-kind labels: the tag shown on a layer, and the name of one built by hand. */
export const LAYER_KINDS: Record<HarmonicsLayer["kind"], { tag: string; customName: string }> = {
  chebyshev: { tag: "Chebyshev", customName: "Custom Chebyshev" },
  flux: { tag: "Flux", customName: "Custom flux" },
  hysteresis: { tag: "Hysteresis", customName: "Custom hysteresis" },
};

/** The layer's display name: its preset's, else "Custom" and its kind. */
export function layerName(l: HarmonicsLayer, presets: readonly HarmonicsPreset[] = []): string {
  const p = l.preset ? presets.find((x) => x.id === l.preset) : undefined;
  if (p) return p.name;
  if (l.preset) return l.preset;
  return LAYER_KINDS[l.kind].customName;
}

export const PRESET_ORIGINS: { origin: string; label: string; title: string }[] = [
  { origin: "hand_picked", label: "Hand-picked", title: "The stage's original characters, levels picked by ear." },
  { origin: "computed", label: "Computed", title: "From device equations at a stated operating point." },
  { origin: "archetype", label: "Archetypes", title: "Typical behaviour of a kind of device, not one model." },
];

/** Presets grouped by origin, in the order of `PRESET_ORIGINS` (unknown origins last). */
export function presetGroups(presets: readonly HarmonicsPreset[]): { origin: string; label: string; title: string; presets: HarmonicsPreset[] }[] {
  const known = PRESET_ORIGINS.map((o) => ({ ...o, presets: presets.filter((p) => p.origin === o.origin) }));
  const rest = presets.filter((p) => !PRESET_ORIGINS.some((o) => o.origin === p.origin));
  const groups = rest.length ? [...known, { origin: "other", label: "Other", title: "", presets: rest }] : known;
  return groups.filter((g) => g.presets.length > 0);
}

/** `c` with layer `i` replaced by `fn` of it. */
export function withLayer(c: HarmonicsConfig, i: number, fn: (l: HarmonicsLayer) => HarmonicsLayer): HarmonicsConfig {
  return { ...c, layers: c.layers.map((l, j) => (j === i ? fn(l) : l)) };
}

/** `c` with layer `i` moved to position `to` (the others keep their order); `c` itself when either is out of range. */
export function moveLayer(c: HarmonicsConfig, i: number, to: number): HarmonicsConfig {
  const n = c.layers.length;
  if (i === to || i < 0 || to < 0 || i >= n || to >= n) return c;
  const layers = [...c.layers];
  const [l] = layers.splice(i, 1);
  layers.splice(to, 0, l);
  return { ...c, layers };
}

/** Where the open layer card goes after layer `i` moves to `to`. */
export function movedIndex(open: number | null, i: number, to: number): number | null {
  if (open === null) return null;
  if (open === i) return to;
  if (i < open && to >= open) return open - 1;
  if (i > open && to <= open) return open + 1;
  return open;
}

/** Whether the stage runs its layers in series. */
export function harmonicsSeries(c: HarmonicsConfig | null | undefined): boolean {
  return c?.routing === "series";
}

/** `c` with one Chebyshev order of layer `i` set (index 0 is H2; null = off). */
export function withOrderLevel(c: HarmonicsConfig, i: number, order: number, db: number | null): HarmonicsConfig {
  return withLayer(c, i, (l) => {
    if (l.kind !== "chebyshev") return l;
    const levels_db = [...l.levels_db] as ChebyshevLayer["levels_db"];
    levels_db[order] = db;
    return { ...l, levels_db };
  });
}

/** `c` with one Chebyshev order's sign of layer `i` set. */
export function withOrderSign(c: HarmonicsConfig, i: number, order: number, sign: HarmonicSign): HarmonicsConfig {
  return withLayer(c, i, (l) => {
    if (l.kind !== "chebyshev") return l;
    const signs = [...l.signs] as ChebyshevLayer["signs"];
    signs[order] = sign;
    return { ...l, signs };
  });
}

/** `c` with the tilt of Chebyshev layer `i` replaced by `fn` of it (a flat default when it has none). */
export function withTilt(c: HarmonicsConfig, i: number, fn: (t: HarmonicsTilt) => HarmonicsTilt): HarmonicsConfig {
  return withLayer(c, i, (l) => (l.kind === "chebyshev" ? { ...l, tilt: fn(l.tilt ?? defaultTilt()) } : l));
}

/** `t` with order `k`'s (2..9) override set (`null`: back to the layer's shelves); `orders` dropped when none is left. */
export function withOrderTilt(t: HarmonicsTilt, k: number, o: OrderTilt | null): HarmonicsTilt {
  const orders = Array.from({ length: 8 }, (_, i) => (i === k - 2 ? o : (t.orders?.[i] ?? null))) as NonNullable<HarmonicsTilt["orders"]>;
  const { orders: _old, ...rest } = t;
  return orders.some((x) => x) ? { ...rest, orders } : rest;
}

/** Orders in a readout point, H2..H15 (the protocol's `HARMONICS_READOUT_ORDERS`). */
export const HARMONICS_READOUT_ORDERS = 14;

/** The lowest frequency of a readout's `freq_sweep` at one input level whose orders fold past Nyquist (`aliased`), if any. */
export function firstAliasedFreq(freqSweep: readonly HarmonicsReadoutPoint[] | undefined, levelDbfs: number): number | undefined {
  const f = (freqSweep ?? []).filter((p) => p.input_dbfs === levelDbfs && (p.aliased?.length ?? 0) > 0).map((p) => p.freq_hz);
  return f.length ? Math.min(...f) : undefined;
}

/**
 * THD and each order against frequency at one input level, from a readout's
 * `freq_sweep`. `above[k][i]`: order k + 2 lands above Nyquist at point i
 * (listed in its `aliased`, so not a level of that harmonic).
 */
export function freqCurves(
  freqSweep: readonly HarmonicsReadoutPoint[] | undefined,
  levelDbfs: number,
): { freqs: number[]; thd: number[]; orders: number[][]; above: boolean[][]; gain: number[] } {
  const pts = (freqSweep ?? []).filter((p) => p.input_dbfs === levelDbfs).sort((a, b) => a.freq_hz - b.freq_hz);
  return {
    freqs: pts.map((p) => p.freq_hz),
    thd: pts.map((p) => p.thd_db),
    orders: Array.from({ length: HARMONICS_READOUT_ORDERS }, (_, k) => pts.map((p) => p.harmonics_db[k])),
    above: Array.from({ length: HARMONICS_READOUT_ORDERS }, (_, k) => pts.map((p) => (p.aliased ?? []).some((a) => a.order === k + 2))),
    gain: pts.map((p) => p.gain_db),
  };
}

/** Harmonic levels against input level, per order, from a readout's `sweep` at one tone. */
export function sweepCurves(sweep: readonly HarmonicsReadoutPoint[] | undefined, freqHz: number): { levels: number[]; orders: number[][] } {
  const pts = (sweep ?? []).filter((p) => p.freq_hz === freqHz).sort((a, b) => a.input_dbfs - b.input_dbfs);
  return { levels: pts.map((p) => p.input_dbfs), orders: Array.from({ length: HARMONICS_READOUT_ORDERS }, (_, k) => pts.map((p) => p.harmonics_db[k])) };
}

/**
 * Signed amplitudes a2..a9 of the enabled Chebyshev layers, summed (signs,
 * `invert_even` and `amount`; drive taken as 0 dB). Flux layers are not
 * counted: the engine's `harmonics_readout` has the exact figures.
 */
function harmonicAmplitudes(c: HarmonicsConfig): number[] {
  const a = [0, 0, 0, 0, 0, 0, 0, 0];
  for (const l of c.layers) {
    if (l.kind !== "chebyshev" || !l.enabled) continue;
    l.levels_db.forEach((db, i) => {
      if (db === null || db === undefined) return;
      let sign = l.signs[i] === "-" ? -1 : 1;
      if (l.invert_even && i % 2 === 0) sign = -sign;
      a[i] += sign * l.amount * Math.pow(10, db / 20);
    });
  }
  return a;
}

/**
 * Worst-case peak gain for a full-scale input, dB: how far over full scale
 * the stage can go (it is not turned down for it; the limiter and the
 * output headroom are). A grid estimate of the engine's certified bound
 * (its DC blocker's first tap taken as 1) for the Chebyshev layers.
 */
export function harmonicsPeakDb(c: HarmonicsConfig): number {
  // a[k] for k = 2..9, signed.
  const a = [0, 0, ...harmonicAmplitudes(c)];
  let maxO = -Infinity;
  let minO = Infinity;
  let maxE = -Infinity;
  let minE = Infinity;
  let maxAll = 0;
  const N = 4096;
  for (let i = 0; i <= N; i++) {
    const x = -1 + (2 * i) / N;
    // x + Σ a_k (T_k(x) − T_k(0)), T_k by the recurrence; T_k(0) = ±1 for even k.
    let prev = 1;
    let t = x;
    let e = 0;
    let o = x;
    for (let k = 2; k <= 9; k++) {
      const next = 2 * x * t - prev;
      prev = t;
      t = next;
      if (k % 2 === 0) e += a[k] * (t - (k % 4 === 0 ? 1 : -1));
      else o += a[k] * t;
    }
    maxO = Math.max(maxO, o + e);
    minO = Math.min(minO, o + e);
    maxE = Math.max(maxE, e);
    minE = Math.min(minE, e);
    maxAll = Math.max(maxAll, Math.abs(o + e));
  }
  const bound = c.dc_block ? Math.max(maxO - minE, maxE - minO) : maxAll;
  return 20 * Math.log10(bound);
}

export function harmonicsOn(c: HarmonicsConfig | null | undefined): boolean {
  return !!c && c.enabled;
}

/** "Triode", "Triode + Tape, hot", "Triode + 2 more", or "Off" (names from `presets` when known). */
export function harmonicsSummary(c: HarmonicsConfig | null | undefined, presets: readonly HarmonicsPreset[] = []): string {
  if (!harmonicsOn(c)) return "Off";
  const on = c!.layers.filter((l) => l.enabled);
  if (on.length === 0) return "No layers on";
  const names = on.map((l) => layerName(l, presets));
  return names.length <= 2 ? names.join(" + ") : `${names[0]} + ${names.length - 1} more`;
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
