// In-memory stand-in for the daemon, used when the app runs in a plain
// browser (`npm run dev`, Playwright, vitest). It implements the protocol
// with the daemon's semantics (docs/control-plane.md): presets, devices,
// volume, split EQ, the IR chain and config/presets events. It echoes state
// like the daemon does and simulates meters at 30 Hz.
//
// URL switches (dev only): `?engine=down` starts with the engine "not
// running"; `?apps` starts in Selected-apps routing; `?split` starts with a
// split L/R EQ; `?overlays=demo` adds the built-in Harman target next to the
// simulated measurement the mock otherwise always has, and `?overlays=none`
// starts with no overlays at all (clean screenshots); `?digital` simulates a
// DAC without a hardware mixer (digital volume); `?measure=silent` makes
// "Match to this track" hear side B as silent (its no-match path).

import { DEFAULT_UI_SETTINGS, snapVolumeStep, type KeysState, type MainViewMemory, type OsdPayload, type ShellEvent, type ShellView, type UiSettings, type WindowLabel } from "./shell";
import { autoPreampDb, profileAutoPreampDb, profileChannelResponseDb } from "../lib/eqmath";
import { bandRationalProblem, kaiserAttenuationDb, manualRationalProblem, minimumPhaseBlockReason, nyquistStopBlockReason, nyquistStopCutoff } from "../lib/upsamplerLimits";
import { parseApo } from "../lib/apo";
import { normalizeRatePolicy } from "../lib/rates";
import { cpuRefusal, rateAdmissions } from "../lib/cpuBudget";
import { DEFAULT_CUSTOM_FIELDS, fromUpsamplerConfig, legacyToDesign, RECOMMENDED_PRESET, type LegacyUpsampler } from "../lib/upsamplerAdapter";
import { presetCutoffFraction, presetEntry, presetRationalCost, presetRationalTaps, presetRatioNeed, presetTapsPerInput, rationalPipelines } from "../lib/filterPresets";
import { ALLOWED_SOURCE_RATES } from "../lib/upsamplerLimits";
import { HIGH_OUTPUT_RATES, MAX_INTEGER_RATIO, MAX_OUTPUT_RATE, rateRatioOf, resolveRateRatio } from "../lib/upsamplerRate";
import { BUILTIN_TARGETS, DRAW_FREQS, OVERLAY_COLOURS, type OverlayData } from "../lib/overlays";
import { bandCount, convList, deepEqual, emptyEq, eqIsActive, normalizeEq } from "../lib/chain";
import { chainB as rawChainB, FLAT_PATCH, materializeB, patchError } from "../lib/abPatch";
import { effectiveEndHz, normalizeTilt, tiltConfigError, tiltPairProblem, tiltProblem } from "../lib/tilt";
import { crossfeedActive, defaultHarmonics, limiterDelay, pitchActive as stagePitchActive, pitchCents } from "../lib/effects";
import { DaemonError, ErrorCode, type IrInfo, type Transport, type TransportHandlers } from "./transport";
import { catalogueDesign } from "./mockCatalogue";
import { MOCK_HARMONICS_PRESETS, MOCK_HARMONICS_SYSTEMS, mockHarmonicsReadout, mockHarmonicsSpectrum } from "./mockHarmonics";
import { mockHarmonicsLive } from "./mockHarmonicsLive";
import { mockConvResponse } from "./mockConvResponse";
import { bandPassbandEnd, checkDesign as mockCheckDesign, designFallbacks as mockDesignFallbacks, type MockCheckContext } from "./mockDesignCheck";
import { bandRate, sameDesign } from "../lib/filterDesign";
import { applyPreset, PRESET_PARTS, presetCovers, presetMatches } from "../lib/presetParts";
import type {
  CalibrationStatus,
  TiltConfig,
  RateAdmission,
  HarmonicsSpectrumTrace,
  AbB,
  AbConfig,
  AbStatus,
  AbSide,
  Band,
  ChainConfig,
  ConvolutionConfig,
  DaemonEvent,
  DcBlockConfig,
  DeviceCapabilities,
  DeviceInfo,
  EngineConfig,
  EngineLimits,
  FilterPhase,
  FilterPreset,
  Meters,
  NowPlaying,
  OverloadReport,
  PassthroughBreak,
  Preset,
  PresetInfo,
  PresetPart,
  RateRatio,
  RateTarget,
  FilterDesign,
  DesignPlan,
  ResponseWindowRequest,
  SampleFormat,
  SelfTestResult,
  SetStreamVolumeParams,
  SetVolumeGuardedParams,
  SetVarispeedParams,
  VarispeedStatus,
  SetVolumeParams,
  SinkChoice,
  FollowerState,
  FollowerStatus,
  PathLatency,
  OutputsStatus,
  Spectrum,
  Status,
  StreamInfo,
  StreamVolume,
  Topic,
  UpsamplerConfig,
  UpsamplerResponse,
  VolumeStatus,
} from "./types";
import { BALANCE_MAX_DB, DC_BLOCK_CUTOFF_MAX_HZ, DC_BLOCK_CUTOFF_MIN_HZ, VOLUME_MAX_DB, VOLUME_MIN_DB } from "./types";

/** The mock app's own version (and the mock engine's, unless `?engine_version=`). */
export const MOCK_APP_VERSION = "0.1.0";

const clone = <T>(x: T): T => JSON.parse(JSON.stringify(x)) as T;

const B = (kind: Band["kind"], freq_hz: number, gain_db: number, q: number): Band => ({ kind, freq_hz, gain_db, q, enabled: true });

const HD650: Band[] = [
  B("low_shelf", 105, 5.5, 0.7),
  B("peaking", 57, 1.8, 0.9),
  B("peaking", 153, -1.0, 1.02),
  B("peaking", 1280, -1.1, 2.9),
  B("peaking", 2550, 1.6, 1.95),
  B("peaking", 4480, 1.5, 4.0),
  B("high_shelf", 10000, -2.5, 0.7),
];
const ROOM: Band[] = [B("low_shelf", 80, 3.0, 0.7), B("peaking", 42, -4.0, 5.0), B("peaking", 118, -3.2, 4.2), B("high_shelf", 8000, -1.5, 0.7)];

const IR_DIR = "/home/you/Music/Impulse responses/";
const IR_FACTS: Record<string, IrInfo> = {
  "Living room correction.wav": { channels: 2, frames: 65536, rate: 48000 },
  "HD650 crossfeed.wav": { channels: 2, frames: 4096, rate: 44100 },
  "Near-field speakers.wav": { channels: 1, frames: 32768, rate: 48000 },
  "Subwoofer integration.wav": { channels: 2, frames: 16384, rate: 96000 },
};

function eqProfile(name: string, bands: Band[]) {
  return { name, version: 2, preamp_db: Math.floor(autoPreampDb(bands, 48000) * 10) / 10 || 0, linked_enabled: true, bands: clone(bands) };
}

/** The preset a design is the expansion of (runtime kinds, unchanged copies), for the mock's model. */
function modelPreset(d: FilterDesign | null): string | null {
  if (!d) return null;
  if (d.kind === "cascade") return ({ fir: "cascade_fir", fir_minimum: "cascade_fir_mp", fir_asymmetric: "cascade_fir_asym", frequency_sampled: "cascade_fft", elliptic: "cascade_elliptic", inverse_chebyshev: "cascade_inverse_chebyshev" } as const)[d.stage];
  if (d.kind === "interpolator") return d.method;
  if (d.kind === "block_fft") return "input" in d.block ? (d.overlap === "half" ? "block_fft_fast" : "block_fft") : "block_fft_large";
  if (!d.based_on || d.based_on === RECOMMENDED_PRESET) return null;
  const base = catalogueDesign(d.based_on);
  return base && sameDesign(base, d) ? d.based_on : null;
}

function baseChain(): ChainConfig {
  return {
    eq: eqProfile("HD650 AutoEQ", HD650),
    limiter: { mode: "off", ceiling_dbfs: -0.5, lookahead_ms: 5, release_ms: 100 },
    convolution: [
      { enabled: true, ir_path: IR_DIR + "Living room correction.wav", gain_db: 0, name: null },
      { enabled: true, ir_path: IR_DIR + "HD650 crossfeed.wav", gain_db: -1, name: null },
    ],
    pitch: { enabled: true, cents: 0, quality: "high" },
    upsampler: {
      target: { ratio: 4 },
      preset: null,
      design: legacyToDesign(DEFAULT_CUSTOM_FIELDS),
    },
    output: { bits: 24, dither: "tpdf", headroom_db: 3 },
  };
}

/** `ChainConfig::default()` with an EQ: what `import_apo` stores. */
function defaultChain(eq: ChainConfig["eq"]): ChainConfig {
  return {
    eq,
    limiter: { mode: "auto_headroom", ceiling_dbfs: -0.5, lookahead_ms: 5, release_ms: 100 },
    convolution: [],
    pitch: { enabled: true, cents: 0, quality: "high" },
    upsampler: {
      target: "off",
      preset: null,
      design: legacyToDesign(DEFAULT_CUSTOM_FIELDS),
    },
    output: { bits: 24, dither: "tpdf", headroom_db: 3 },
  };
}

function flatChain(): ChainConfig {
  const c = baseChain();
  return { ...c, eq: emptyEq("Flat"), convolution: [] };
}

const DEVICES: DeviceInfo[] = [
  { id: "hw:CARD=DAC,DEV=0", card: "DAC", name: "USB Audio DAC", hdmi: false, loopback: false, active: true, channels: [] },
  { id: "hw:CARD=PCH,DEV=0", card: "PCH", name: "Built-in Audio", hdmi: false, loopback: false, active: false, channels: [] },
  { id: "hw:CARD=HDMI,DEV=3", card: "HDMI", name: "HDMI 0 (monitor)", hdmi: true, loopback: false, active: false, channels: [] },
  { id: "hw:CARD=Loopback,DEV=0", card: "Loopback", name: "Loopback", hdmi: false, loopback: true, active: false, channels: [] },
  { id: "hw:CARD=SPK,DEV=0", card: "SPK", name: "USB speaker (48 kHz, 16-bit)", hdmi: false, loopback: false, active: false, channels: [] },
  { id: "hw:CARD=USB,DEV=0", card: "USB", name: "Hi-res USB DAC", hdmi: false, loopback: false, active: false, channels: [] },
  { id: "hw:CARD=USB48,DEV=0", card: "USB48", name: "USB DAC (48 kHz only)", hdmi: false, loopback: false, active: false, channels: [] },
  { id: "hw:CARD=IFACE,DEV=0", card: "IFACE", name: "USB audio interface (8 out)", hdmi: false, loopback: false, active: false, channels: [8] },
];

const RATES_44 = [44100, 88200, 176400, 352800, 705600];
const RATES_48 = [48000, 96000, 192000, 384000, 768000];
/** ×32 and ×64 of 44.1 and 48 kHz, like the protocol's `HIGH_OUTPUT_RATES`. */
const RATES_HIGH = [...HIGH_OUTPUT_RATES];
/** Largest term of a rational ratio the engine opens (`MAX_RATIONAL_TERM`). */
const MAX_RATIONAL_TERM = 4096;

/** The engine's limits on the mock "machine": 32 GB of RAM, so the 4 GiB
 * design workspace ceiling (a quarter of the RAM, 1.5 to 4 GiB). */
export const MOCK_LIMITS: EngineLimits = {
  max_kernel_taps: 8_388_608,
  intermediate_max_taps: 2_097_152,
  design_workspace_bytes: 4 * 2 ** 30,
  cpu_share: 0.9,
};

/**
 * What each mock card accepts, like the engine's probe (`status.device_caps`):
 * - DAC: a typical USB DAC: S16/S24/S32 (24 significant bits) up to 768k,
 *   S24 3-byte only up to 192k.
 * - SPK: a USB speaker that plays 48 kHz S16 only (`?device=SPK` in the
 *   browser).
 * - USB: a hi-res DAC that also takes 1.4112 / 1.536 / 2.8224 / 3.072 MHz
 *   (×32 and ×64) in S32 (`?device=USB`).
 * - USB48: a USB DAC that plays 48 kHz only, S16/S24/S32 (24 significant
 *   bits), for the engine bypass resampler (`?device=USB48`).
 * - IFACE: an audio interface that plays 8 channels only (no stereo mode),
 *   44.1–192 kHz S32 (`?device=IFACE`): the output channel selector.
 */
function capsFor(device: string): DeviceCapabilities | null {
  const card = DEVICES.find((d) => sameDevice(d, device))?.card ?? "";
  const row = (rate: number, formats: [SampleFormat, number | null][]) => ({ rate, formats: formats.map(([format, sbits]) => ({ format, sbits })) });
  const sorted = (rates: number[]) => [...rates].sort((a, b) => a - b);
  const width = card === "IFACE" ? 8 : 2;
  const caps = (rows: ReturnType<typeof row>[]): DeviceCapabilities => ({
    device,
    channels: width,
    channels_min: width,
    channels_max: width,
    rates: rows,
    rate_min: rows[0]?.rate ?? 0,
    rate_max: rows[rows.length - 1]?.rate ?? 0,
    formats: (["s16", "s24", "s24_3", "s32"] as SampleFormat[]).filter((f) => rows.some((r) => r.formats.some((c) => c.format === f))),
  });
  switch (card) {
    case "DAC":
      return caps(
        sorted([...RATES_44, ...RATES_48]).map((r) =>
          row(r, [["s16", 16], ["s24", 24], ...(r <= 192000 ? ([["s24_3", 24]] as [SampleFormat, number][]) : []), ["s32", 24]]),
        ),
      );
    case "PCH":
      return caps([44100, 48000, 96000, 192000].map((r) => row(r, [["s16", 16], ["s24", 24], ["s32", 32]])));
    case "HDMI":
      return caps([44100, 48000, 96000, 192000].map((r) => row(r, [["s16", 16], ["s24", 24], ["s32", 24]])));
    case "Loopback":
      return caps(sorted([...RATES_44, ...RATES_48]).map((r) => row(r, [["s16", 16], ["s24", 24], ["s24_3", 24], ["s32", 32]])));
    case "SPK":
      return caps([row(48000, [["s16", 16]])]);
    case "USB48":
      return caps([row(48000, [["s16", 16], ["s24", 24], ["s32", 24]])]);
    case "IFACE":
      return caps([44100, 48000, 88200, 96000, 176400, 192000].map((r) => row(r, [["s32", 24]])));
    case "USB":
      return caps(
        sorted([...RATES_44, ...RATES_48, ...RATES_HIGH]).map((r) => row(r, [...(r <= 768000 ? ([["s16", 16], ["s24", 24]] as [SampleFormat, number][]) : []), ["s32", 32]])),
      );
    default:
      return null;
  }
}

/** Ratio ladder like io-alsa `ratio_ladder`. */
function ladder(desired: number): number[] {
  const out: number[] = [];
  let r = Math.max(1, desired);
  for (;;) {
    out.push(r);
    if (r === 1) return out;
    r = (r & (r - 1)) === 0 ? r / 2 : r - 1;
  }
}

const ONE: RateRatio = { up: 1, down: 1 };

/** A design as the mock pictures it (see `MockDaemon.design`). */
interface MockDesign {
  ratio: RateRatio;
  taps: number;
  phase: FilterPhase;
  cutoff: number;
  atten: number;
  beta: number;
  tw: number;
  /** Output frames. */
  latency: number;
  /** A band design's edges, as fractions of the lower Nyquist. */
  band?: { pass: number; stop: number };
}

/** Rough stopband depth of each preset family, dB (README), for the mock's plots. */
const PRESET_ATTEN_DB: Record<string, number> = {
  gaussian: 142,
  flattop: 210,
  chebyshev: 190,
  kaiser: 200,
  halfband: 145,
  lanczos: 90,
  nuttall: 110,
  hann: 120,
  leaky: 112,
  cascade: 100,
  interp: 60,
  block: 70,
};
const outRate = (rate: number, r: RateRatio) => Math.round((rate * r.up) / r.down);
const ratioStr = (r: RateRatio) => (r.down === 1 ? `x${r.up}` : `${r.up}/${r.down}`);

/** Whether the device plays `rate` in one of `formats` (empty: any). */
function devicePlaysAt(caps: DeviceCapabilities, rate: number, formats: SampleFormat[]): SampleFormat | null {
  const at = caps.rates.find((r) => r.rate === rate);
  if (!at) return null;
  if (formats.length === 0) return at.formats[0]?.format ?? null;
  return formats.find((x) => at.formats.some((c) => c.format === x)) ?? null;
}

/**
 * The ratios an open tries for a target at `rate`, like io-alsa's ladders:
 * integer targets halve (powers of two) or step down to 1, a fractional or
 * decimating ratio is tried then 1, and `max_rate_any` follows its family
 * rule (README "Rate policy"), skipping what the chain cannot be built for.
 */
function ratioCandidates(chain: ChainConfig, rate: number, caps: DeviceCapabilities | null, formats: SampleFormat[]): RateRatio[] {
  const t = chain.upsampler.target;
  const want = resolveRateRatio(t, rate);
  const pName = chain.upsampler.preset;
  const buildable = (r: RateRatio) => !pName || pName === RECOMMENDED_PRESET || presetRatioNeed(pName as FilterPreset, r, MOCK_LIMITS.max_kernel_taps) === null;
  if (want.down !== 1) return [want, ONE].filter(buildable);
  if (t !== "off" && "max_rate_any" in t && caps) {
    const cap = t.max_rate_any;
    const plays = (r: number) => devicePlaysAt(caps, r, formats) !== null;
    const same: RateRatio[] = [];
    for (let k = MAX_INTEGER_RATIO; k >= 2; k--) if (rate * k <= cap && plays(rate * k)) same.push({ up: k, down: 1 });
    const fam = (r: number) => (r % 11025 === 0 ? 44100 : 48000);
    const other = [...RATES_44, ...RATES_48, ...RATES_HIGH]
      .filter((r) => fam(r) !== fam(rate) && r > rate && r <= cap && plays(r))
      .sort((a, b) => b - a)
      .map((r) => rateRatioOf(rate, r))
      .filter((r) => r.up <= MAX_RATIONAL_TERM && r.down <= MAX_RATIONAL_TERM)
      .filter(buildable);
    const sameBest = same.length ? rate * same[0].up : rate;
    const otherBest = other.length ? outRate(rate, other[0]) : 0;
    const cross = other.length > 0 && (same.length === 0 || otherBest >= 2 * sameBest);
    return cross ? [other[0], ...same, ...other.slice(1), ONE] : [...same, ...other, ONE];
  }
  return ladder(want.up).map((k) => ({ up: k, down: 1 }));
}

/** Like io-alsa `plan_open`: the first ratio on the ladder with a usable format. */
function planOpen(caps: DeviceCapabilities, rate: number, candidates: RateRatio[], formats: SampleFormat[]): { ratio: RateRatio; format: SampleFormat } | null {
  for (const r of candidates.length ? candidates : [ONE]) {
    const f = devicePlaysAt(caps, outRate(rate, r), formats);
    if (f) return { ratio: r, format: f };
  }
  return null;
}

/** Like the daemon's `mark_active`: `hw:CARD=DAC,DEV=0`, `hw:DAC,0`, `hw:DAC`, `hw:DAC,0,0` are one device. */
function sameDevice(d: DeviceInfo, configured: string): boolean {
  const m = /^hw:(?:CARD=)?([^,]+)(?:,(?:DEV=)?(\d+))?(?:,(?:SUBDEV=)?\d+)?$/i.exec(configured.trim());
  return !!m && m[1].toLowerCase() === d.card.toLowerCase() && d.id.endsWith(`DEV=${m[2] ?? "0"}`);
}

function pitchIsActive(p: ChainConfig["pitch"]): boolean {
  if (!p || !p.enabled) return false;
  if (p.reference_hz != null && p.target_hz != null && p.reference_hz > 0 && p.target_hz > 0) {
    return p.reference_hz !== p.target_hz;
  }
  if (p.semitones != null) return p.semitones !== 0;
  return p.cents !== 0;
}

/** Explicit fixed-coefficient dither modes run the quantizer on their own. */
function ditherEnablesProcessing(d: ChainConfig["output"]["dither"]): boolean {
  return d !== "none" && d !== "tpdf" && d !== "shaped";
}

/** Mirrors `DcBlockConfig` serde decoding (object, sequence/array, or null/omitted). */
function normalizeDcBlock(raw: unknown): DcBlockConfig | null {
  if (raw === null || raw === undefined) return null;
  if (Array.isArray(raw)) {
    if (raw.length > 2) {
      throw new DaemonError(ErrorCode.INVALID_PARAMS, "dc_block array must have at most 2 elements");
    }
    let enabled = true;
    if (raw.length >= 1) {
      if (typeof raw[0] !== "boolean") {
        throw new DaemonError(ErrorCode.INVALID_PARAMS, "dc_block.enabled must be a boolean");
      }
      enabled = raw[0];
    }
    let cutoff_hz = 5.0;
    if (raw.length >= 2) {
      if (typeof raw[1] !== "number") {
        throw new DaemonError(ErrorCode.INVALID_PARAMS, "dc_block.cutoff_hz must be a number");
      }
      cutoff_hz = raw[1];
    }
    return { enabled, cutoff_hz };
  }
  if (typeof raw !== "object") {
    throw new DaemonError(ErrorCode.INVALID_PARAMS, "dc_block must be an object or null");
  }
  const r = raw as Record<string, unknown>;
  let enabled = true;
  if (r.enabled !== undefined) {
    if (typeof r.enabled !== "boolean") {
      throw new DaemonError(ErrorCode.INVALID_PARAMS, "dc_block.enabled must be a boolean");
    }
    enabled = r.enabled;
  }
  let cutoff_hz = 5.0;
  if (r.cutoff_hz !== undefined) {
    if (typeof r.cutoff_hz !== "number") {
      throw new DaemonError(ErrorCode.INVALID_PARAMS, "dc_block.cutoff_hz must be a number");
    }
    cutoff_hz = r.cutoff_hz;
  }
  return { enabled, cutoff_hz };
}

/** Serde defaults and the daemon's `normalize` of one stored chain. */
function normalizeChainFields(raw: Partial<ChainConfig>): ChainConfig {
  const chain: ChainConfig = { ...defaultChain(null), ...raw };
  if (chain.dc_block !== undefined) chain.dc_block = normalizeDcBlock(chain.dc_block);
  // Serde field defaults: a patch like `{"crossfeed": {"enabled": true}}` on a
  // chain without the stage fills the rest in.
  if (chain.crossfeed) chain.crossfeed = { enabled: true, mode: "bs2b_default", cutoff_hz: 700, feed_db: 4.5, width: 1, width_shape: "flat", width_corner_hz: 700, ...(chain.crossfeed as Partial<NonNullable<ChainConfig["crossfeed"]>>) };
  if (chain.harmonics) chain.harmonics = { ...defaultHarmonics(), ...(chain.harmonics as Partial<NonNullable<ChainConfig["harmonics"]>>) };
  // A removed `Option` field (`{"design": null}` in a merge patch) reads back as None.
  if (chain.upsampler) chain.upsampler = { ...chain.upsampler, preset: chain.upsampler.preset ?? null, design: chain.upsampler.design ?? null };
  if (chain.pitch) chain.pitch = { enabled: true, cents: 0, quality: "high", ...(chain.pitch as Partial<NonNullable<ChainConfig["pitch"]>>) };
  if (chain.eq) chain.eq = normalizeEq(chain.eq);
  if (chain.eq?.tilt) chain.eq.tilt = normalizeTilt(chain.eq.tilt);
  if (chain.eq?.tilt_high) chain.eq.tilt_high = normalizeTilt(chain.eq.tilt_high);
  return chain;
}

/**
 * Mirrors Serde field defaults + the daemon's `set_ab` check and
 * `normalize_ab`: B is materialised from `b` and chain A. A full chain B
 * keeps its filter; its target, limiter and output follow chain A (`notes`
 * says so). A patch on a shared field is refused.
 */
function normalizeAbWithChainA(raw: unknown, chainA: ChainConfig): { ab: AbConfig; notes: string[] } {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new DaemonError(ErrorCode.INVALID_PARAMS, "ab must be an object or null");
  }
  const rawAb = raw as Partial<AbConfig>;
  const rawB = rawAb.b as unknown;
  if (typeof rawB !== "object" || rawB === null || Array.isArray(rawB) || (!("patch" in rawB) && !("chain" in rawB))) {
    throw new DaemonError(ErrorCode.INVALID_PARAMS, "ab.b must be {patch} or {chain}");
  }
  const notes: string[] = [];
  let b: AbB;
  if ("chain" in rawB) {
    const chain = normalizeChainFields(clone((rawB as { chain: Partial<ChainConfig> }).chain));
    const changed = [
      !deepEqual(chain.upsampler.target, chainA.upsampler.target) && "upsampler.target",
      !deepEqual(chain.limiter, chainA.limiter) && "limiter",
      !deepEqual(chain.output, chainA.output) && "output",
    ].filter(Boolean);
    if (changed.length) notes.push(`side B's ${changed.join(" and ")} now follow chain A`);
    b = { chain: materializeB(chainA, { chain }) };
  } else {
    const patch = (rawB as { patch: unknown }).patch;
    const err = patchError(patch);
    if (err) throw new DaemonError(ErrorCode.INVALID_PARAMS, err);
    b = { patch: clone(patch) as AbB extends { patch: infer P } ? P : never };
  }
  const chain_b = normalizeChainFields(materializeB(chainA, b));
  const level: AbConfig["level"] =
    rawAb.level === undefined
      ? { mode: "analytic", weighting: "k" }
      : rawAb.level.mode === "analytic"
        ? { mode: "analytic", weighting: rawAb.level.weighting ?? "k" }
        : clone(rawAb.level);
  const headroom: AbConfig["headroom"] = rawAb.headroom === undefined ? { mode: "auto" } : clone(rawAb.headroom);
  const ab: AbConfig = { b, chain_b, level, headroom };
  if (typeof rawAb.label === "string") ab.label = rawAb.label;
  return { ab, notes };
}

/** Chain B as the daemon deserialises it: materialised, then Serde's field defaults. */
function chainB(ab: AbConfig, a: ChainConfig): ChainConfig {
  return normalizeChainFields(rawChainB(ab, a));
}

/** Mirrors `ChainConfig::is_passthrough` in the daemon. */
function chainIsPassthrough(chain: ChainConfig): boolean {
  const eqActive = eqIsActive(chain.eq);
  const conv = convList(chain).some((c) => c.enabled);
  const dcBlockActive = chain.dc_block != null && (chain.dc_block.enabled ?? true);
  return (
    !dcBlockActive &&
    !crossfeedActive(chain.crossfeed) &&
    !eqActive &&
    !conv &&
    !pitchIsActive(chain.pitch) &&
    !(chain.harmonics?.enabled ?? false) &&
    (chain.limiter?.mode ?? "off") === "off" &&
    chain.upsampler.target === "off" &&
    !ditherEnablesProcessing(chain.output.dither) &&
    chain.output.headroom_db === 0
  );
}

function bandsCountOfChain(chain: ChainConfig): number {
  return bandCount(chain.eq);
}

/** A plausible in-room measurement for the browser mock (never a real one). */
function mockMeasurement(): OverlayData {
  const bump = (f: number, centre: number, width: number, gain: number) =>
    gain * Math.exp(-Math.pow(Math.log2(f / centre) / width, 2));
  const points: OverlayData["points"] = DRAW_FREQS.map((f) => {
    const db =
      bump(f, 52, 0.5, 3.2) + bump(f, 210, 0.7, -2.4) + bump(f, 780, 0.55, 2.2) + bump(f, 2800, 0.7, -3.4) + bump(f, 7200, 0.9, -2.2) - 0.9 * Math.max(0, Math.log2(f / 9000));
    return [Math.round(f * 10) / 10, Math.round(db * 10) / 10];
  });
  return {
    version: 1,
    name: "Desk monitor (mock room)",
    kind: "measurement",
    source: "Simulated: this is the browser mock, not a real measurement.",
    points,
    view: { colour: OVERLAY_COLOURS[1], visible: true, offset_db: 0, predicted: true },
  };
}

type Listener = (e: DaemonEvent) => void;

/** A daemon instance id like the daemon's: 128 random bits in hex. */
function newInstance(): string {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}


type MockPreset = Preset & { modified_unix: number };

function presetKey(part: PresetPart | undefined, name: string): string {
  return `${part ?? ""}/${name}`;
}

export class MockDaemon {
  config: EngineConfig;
  /** Keyed by `presetKey(part, name)`. */
  presets = new Map<string, MockPreset>();
  /** When each stream started playing (a counter), for the arbiter's
   * first-started rule. Kept while it plays, across sink moves. */
  private startedAt = new Map<number, number>();
  /** `harmonics_live`: frames counted, and the clock the mock's music follows. */
  private liveSeq = 0;
  private liveStart = Date.now();
  private startSeq = 0;
  private streamList: StreamInfo[] = [];
  nowPlaying: NowPlaying | null;
  volume: VolumeStatus = {
    volume_db: -12, balance_db: 0, muted: false, hardware: true, min_db: VOLUME_MIN_DB, max_db: 0,
    mode: "auto", hardware_db: -12, software_db: 0, hardware_unavailable: false, ceiling_db: null,
    sink_volume: "ignore", guard: "off", blocked: 0, last_blocked: null,
  };
  abSide: AbSide = "a";
  /** The blind mask (`ab_blind`): status hides the side. */
  abBlind = false;
  released = false;
  running: boolean;
  /** `Status.overload`: the fallback in effect after a simulated overload. */
  overload: OverloadReport | null = null;
  /** Source rates where the request overloaded (the per-rate fallback's `cause: overload`). */
  trippedRates: number[] = [];
  private overloadEpisodes = 0;
  /** `VolumeStatus.instance`: new on every (re)start. */
  instance = newInstance();
  /** `VolumeStatus.revision`: bumped on every change of the level state. */
  revision = 1;
  /** Leave out `instance`/`revision`, like a daemon that predates them. */
  legacyVolume = false;
  /** No `set_stream_volume` and no `StreamInfo.volume_db`, like a daemon
   *  that predates them. */
  legacyStreamVolume = false;
  /** `status.version`; the mock app is MOCK_APP_VERSION (`?engine_version=` to differ). */
  engineVersion: string;
  /** Overlay files, as the app process would keep them on disk. */
  overlays = new Map<string, OverlayData>();
  private listeners = new Set<Listener>();
  private connListeners = new Set<(up: boolean) => void>();
  private meterTimer: ReturnType<typeof setInterval> | null = null;
  private phase = 0;
  private tickCount = 0;
  private spectrumEnabled = false;
  private clipped = 0;
  private clippedCh = [0, 0];
  private lastStatusJson = "";
  private latency = 40;
  /** The card has a hardware mixer (`?digital`: no). */
  private hasMixer = true;
  /** Limiter telemetry: running event total, held automatic headroom, in reduction. */
  private limEvents = 0;
  private limHeadroom = 0;
  /** `Status.limiter_held_db`: the held headroom while the limiter is in the path. */
  private limHeld = 0;
  private limReducing = false;
  /** Followers: when each one (re)started, for its opening → locking → locked walk. */
  private followerSince = new Map<string, number>();
  /** Follower devices reported `lost` (tests, `?follower_lost=`). */
  lostFollowers = new Set<string>();
  /** The clock the follower walk reads (tests replace it). */
  followerClock: () => number = () => Date.now();
  /** `Status.error`: a recoverable engine error while the streams keep playing. */
  engineError: string | null = null;
  /** `Status.xruns`: dropouts since the engine started. */
  xruns = 0;

  /** Behave like an engine before fractional rates (0.9.4): no `rate_ratio`, integer targets only. */
  legacyRatio = false;
  /** A crossfeed config as the engine keeps it: serde drops fields it does not know (an old client's `level`). */
  private crossfeedAsStored(xf: NonNullable<ChainConfig["crossfeed"]>): NonNullable<ChainConfig["crossfeed"]> {
    const { level: _level, ...rest } = xf as NonNullable<ChainConfig["crossfeed"]> & { level?: unknown };
    return rest;
  }

  /** An engine error to report, pushed at once like the daemon's (`null` clears it). */
  setEngineError(error: string | null) {
    this.engineError = error;
    this.emitStatus(true);
  }

  /** xruns so far, pushed at once like the daemon's. */
  setXruns(n: number) {
    this.xruns = Math.max(0, Math.round(n));
    this.emitStatus(true);
  }

  constructor(opts: { running?: boolean; apps?: boolean; split?: boolean; overlays?: boolean | "none"; digitalVolume?: boolean; device?: string; engineVersion?: string; playRate?: number; legacyRatio?: boolean; legacyStreamVolume?: boolean; followers?: string[] } = {}) {
    this.legacyRatio = opts.legacyRatio ?? false;
    this.legacyStreamVolume = opts.legacyStreamVolume ?? false;
    this.running = opts.running ?? true;
    this.engineVersion = opts.engineVersion ?? MOCK_APP_VERSION;
    if (opts.digitalVolume) {
      this.volume = { ...this.volume, hardware: false };
      this.hasMixer = false;
    }
    if (opts.overlays !== "none") {
      const measured = mockMeasurement();
      this.overlays.set(measured.name, measured);
    }
    if (opts.overlays === true && BUILTIN_TARGETS[0]) {
      const target = BUILTIN_TARGETS[0];
      this.overlays.set(target.name, { ...target, view: { colour: OVERLAY_COLOURS[0], visible: true, offset_db: 0, predicted: false } });
    }
    const chain = baseChain();
    if (opts.split && chain.eq) {
      chain.eq.right = { enabled: true, preamp_db: 0, bands: [B("peaking", 138, -4.0, 3.0)] };
      chain.eq.preamp_db = Math.floor(profileAutoPreampDb(chain.eq, 48000) * 10) / 10 || 0;
    }
    this.config = {
      device: (opts.device && DEVICES.find((d) => d.card === opts.device || sameDevice(d, opts.device!))?.id) || DEVICES[0].id,
      formats: ["s32", "s24", "s24_3", "s16"],
      dsp_enabled: true,
      chain,
      ab: { b: { patch: FLAT_PATCH }, label: "Flat", chain_b: flatChain(), level: { mode: "analytic", weighting: "k" }, headroom: { mode: "auto" } },
      rate: { follow_source: true, allowed_rates: [44100, 48000, 88200, 96000, 176400, 192000], default_rate: 48000, conflict: "keep_current", bypass_resampler: "engine" },
      quantum: 1024,
      routing: opts.apps ? { mode: "apps", rules: [{ app: "Strawberry", sink: "dsp" }] } : { mode: "system" },
      volume: {
        volume_db: -12, balance_db: 0, muted: false, mode: "auto", hardware_db: null, max_volume_db: null,
        sink_volume: "ignore", guard: { mode: "off", max_rise_db_per_s: 6, stream_max_db: null },
      },
      varispeed: { enabled: false, speed: 1, ramp_ms: 200, keep_pitch: false },
      device_lost: "pause",
      outputs: { followers: [] },
      channel_workers: true,
      active_preset: "HD650 AutoEQ",
      active_part_presets: { eq: "HD650 AutoEQ" },
    };
    // Preloaded followers (`?followers=USB,SPK`): cards other than the main one.
    for (const card of opts.followers ?? []) {
      const d = DEVICES.find((x) => x.card === card || x.id === card);
      if (d && d.id !== this.config.device && !this.config.outputs.followers.some((f) => f.device === d.id) && this.config.outputs.followers.length < 3) {
        this.config.outputs.followers.push({ device: d.id, enabled: true, trim_ms: 0, volume_db: 0, follow_volume: false });
      }
    }
    // Already playing (and locked) when the preview opens.
    for (const f of this.config.outputs.followers) this.followerSince.set(f.device, Date.now() - 60_000);
    const now = Math.floor(Date.now() / 1000);
    this.presets.set(presetKey(undefined, "HD650 AutoEQ"), { version: 1, name: "HD650 AutoEQ", chain: clone(chain), modified_unix: now - 86400 * 3 });
    const room = baseChain();
    room.eq = eqProfile("Room + bass shelf", ROOM);
    this.presets.set(presetKey(undefined, "Room + bass shelf"), { version: 1, name: "Room + bass shelf", chain: room, modified_unix: now - 86400 * 12 });
    this.presets.set(presetKey(undefined, "Flat"), { version: 1, name: "Flat", chain: flatChain(), modified_unix: now - 86400 * 40 });
    // Each page's own: two EQs to switch between, an upsampling setting.
    this.presets.set(presetKey("eq", "HD650 AutoEQ"), { version: 1, name: "HD650 AutoEQ", part: "eq", chain: clone(chain), modified_unix: now - 86400 * 3 });
    this.presets.set(presetKey("eq", "Room + bass shelf"), { version: 1, name: "Room + bass shelf", part: "eq", chain: clone(room), modified_unix: now - 86400 * 12 });
    const desk = clone(chain);
    desk.upsampler = { ...desk.upsampler, target: { ratio: 4 } } as typeof desk.upsampler;
    this.presets.set(presetKey("upsampling", "Desk ×4"), { version: 1, name: "Desk ×4", part: "upsampling", chain: desk, modified_unix: now - 86400 * 5 });
    this.streams = [
      { node: 81, app_name: "Strawberry", binary: "strawberry", media_name: "Nils Frahm · Says", rate: opts.playRate ?? 44100, channels: 2, format: "S24LE", sink: "dsp", volume: 1, muted: false, volume_db: 0 },
      { node: 94, app_name: "Firefox", binary: "firefox", media_name: "YouTube", rate: 48000, channels: 2, format: "F32LE", sink: null, volume: 1, muted: false, volume_db: 0 },
      { node: 102, app_name: "Discord", binary: "Discord", media_name: "Voice", rate: 48000, channels: 2, format: "F32LE", sink: null, volume: 0.8, muted: false, volume_db: -1.94 },
    ];
    this.nowPlaying = { player: "org.mpris.MediaPlayer2.strawberry", title: "Says", artist: "Nils Frahm", album: "Spaces", status: "Playing", native_rate: 44100, native_bits: 24 };
    this.applyGain();
  }

  // ------------------------------------------------------------ events

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    this.ensureMeters();
    return () => {
      this.listeners.delete(fn);
    };
  }
  setSpectrumEnabled(enabled: boolean) {
    this.spectrumEnabled = enabled;
  }
  onConnection(fn: (up: boolean) => void): () => void {
    this.connListeners.add(fn);
    return () => this.connListeners.delete(fn);
  }
  private emit(e: DaemonEvent) {
    for (const l of this.listeners) l(clone(e));
  }
  private emitStatus(force = false) {
    const st = this.status();
    const json = JSON.stringify(st);
    if (!force && json === this.lastStatusJson) return;
    this.lastStatusJson = json;
    this.emit({ event: "status", data: st });
  }
  private emitConfig() {
    this.emit({ event: "config", data: this.config });
    this.emitStatus();
  }
  private emitPresets() {
    this.emit({ event: "presets", data: this.listPresets() });
    this.emitStatus();
  }
  snapshot(topics: Topic[]) {
    if (topics.includes("status")) this.emit({ event: "status", data: this.status() });
    if (topics.includes("streams")) this.emit({ event: "streams", data: this.visibleStreams() });
    if (topics.includes("now_playing")) this.emit({ event: "now_playing", data: this.nowPlaying });
    if (topics.includes("config")) this.emit({ event: "config", data: this.config });
    if (topics.includes("presets")) this.emit({ event: "presets", data: this.listPresets() });
    if (topics.includes("devices")) this.emit({ event: "devices", data: this.listDevices() });
  }

  /** The volume status as reported: with this instance and revision. */
  volumeStatus(): VolumeStatus {
    if (this.legacyVolume) {
      const { instance: _i, revision: _r, ...legacy } = clone(this.volume);
      return legacy as VolumeStatus;
    }
    return { ...clone(this.volume), instance: this.instance, revision: this.revision };
  }

  /** Run `change`, then work out what is applied; bump the revision when
   *  the level state or the applied split moved. */
  private levelChange(change: () => void) {
    const key = () => JSON.stringify([this.volume.volume_db, this.volume.balance_db, this.volume.muted, this.volume.software_db, this.volume.hardware_db]);
    const before = key();
    change();
    this.applyGain();
    if (key() !== before) this.revision++;
  }

  /**
   * The split between the card and the digital part, like the engine's
   * `volume::resolve` (a mock mixer reaching down to −100 dB): `auto` and
   * `hardware` put the volume on the card, `software` is all digital,
   * `hybrid` keeps the card at `hardware_db` and attenuates the rest
   * digitally; without a mixer everything is digital.
   */
  private applyGain() {
    const v = this.volume;
    const target = v.volume_db;
    const residue = (hw: number) => {
      const r = Math.min(0, target - hw);
      return r > -0.02 ? 0 : r;
    };
    const cfgHw = this.config?.volume?.hardware_db ?? null;
    if (!this.hasMixer || v.mode === "software") {
      this.volume = { ...v, hardware: false, software_db: target, hardware_db: this.hasMixer ? cfgHw : null };
    } else if (v.mode === "hybrid") {
      const hw = Math.max(cfgHw ?? target, target);
      this.volume = { ...v, hardware: true, hardware_db: hw, software_db: residue(hw) };
    } else {
      const hw = Math.max(-100, target);
      this.volume = { ...v, hardware: true, hardware_db: hw, software_db: residue(hw) };
    }
  }

  /** Replace the config as another client or an older engine would (tests). */
  snapshotConfig(config: EngineConfig) {
    this.config = clone(config);
    this.emitConfig();
  }

  /** A level change from outside the app (the CLI, the desktop). */
  setLevel(patch: Partial<Pick<VolumeStatus, "volume_db" | "balance_db" | "muted">>) {
    this.levelChange(() => {
      this.volume = { ...this.volume, ...patch };
      if (this.config.volume) this.config.volume = { ...this.config.volume, ...patch };
    });
    this.emitConfig();
  }

  startEngine() {
    this.running = true;
    for (const l of this.connListeners) l(true);
  }

  /** Like a service restart after an upgrade: down, then up on the app's version. */
  restartEngine() {
    // Calls on the old connection fail while it is down, as with a real
    // daemon whose socket went away.
    this.running = false;
    for (const l of this.connListeners) l(false);
    this.engineVersion = MOCK_APP_VERSION;
    setTimeout(() => {
      this.instance = newInstance();
      this.revision = 1;
      this.running = true;
      for (const l of this.connListeners) l(true);
    }, 300);
  }

  private ensureMeters() {
    if (this.meterTimer) return;
    this.meterTimer = setInterval(() => {
      if (!this.running || this.listeners.size === 0) return;
      this.tickCount++;
      this.emit({ event: "meters", data: this.meters() });
      // Followers lock over a few seconds: push their status as it moves.
      if (this.config.outputs.followers.length && this.tickCount % 15 === 0) this.emitStatus();
      if (this.spectrumEnabled && this.tickCount % 2 === 0 && this.feeding().length > 0 && !this.released && !this.volume.muted) {
        this.emit({ event: "spectrum", data: this.spectrum() });
      }
    }, 33);
  }

  stop() {
    if (this.meterTimer) clearInterval(this.meterTimer);
    this.meterTimer = null;
    if (this.abMeasure) clearInterval(this.abMeasure.timer);
  }

  // ------------------------------------------------------------ derived state

  /** The active sink's clients, oldest first (as the arbiter sorts them:
   * the first started wins a conflict it cannot keep). */
  private feeding(): StreamInfo[] {
    const sink = this.activeSink();
    const since = (s: StreamInfo) => this.startedAt.get(s.node) ?? 0;
    return this.streams.filter((s) => s.sink !== null && s.sink === sink).sort((a, b) => since(a) - since(b) || a.node - b.node);
  }

  get streams(): StreamInfo[] {
    return this.streamList;
  }

  /** Replace the streams. A stream new to the list starts playing (its age
   * counts from now); one gone from it stopped. Moving sinks, or any other
   * change, keeps a stream's age, as the arbiter keeps a running client's. */
  set streams(next: StreamInfo[]) {
    const playing = new Set(next.map((s) => s.node));
    for (const node of [...this.startedAt.keys()]) if (!playing.has(node)) this.startedAt.delete(node);
    for (const s of next) if (!this.startedAt.has(s.node)) this.startedAt.set(s.node, ++this.startSeq);
    this.streamList = next;
  }

  private activeSink(): SinkChoice | null {
    if (this.released) return null;
    const hasDirect = this.streams.some((s) => s.sink === "direct");
    const hasDsp = this.streams.some((s) => s.sink === "dsp");
    return hasDirect && !hasDsp ? "direct" : hasDsp ? "dsp" : null;
  }

  private chainOn(): ChainConfig {
    const ab = this.config.ab;
    return this.abSide === "b" && ab ? chainB(ab, this.config.chain) : this.config.chain;
  }

  /** Rate the graph last ran at while something played (for `keep_current`). */
  private lastRate = 0;

  /**
   * Like the engine's io-pw `rate_caps`: the configured rates the device can
   * play without the engine bypass resampler (natively, or upsampled with
   * DSP on, with some format), widened to every such engine rate when none
   * is, and the default rate moved to the nearest of them (same family
   * first, then 48k): the DSP sink's native base (`native`). With DSP off,
   * the configured rates the engine converts are added on top, never in
   * its place.
   */
  effectivePolicy(): { allowed: number[]; defaultRate: number; native: number[] } {
    const policy = this.config.rate;
    const caps = capsFor(this.config.device);
    if (!caps) return { allowed: policy.allowed_rates, defaultRate: policy.default_rate, native: policy.allowed_rates };
    const chain = this.chainOn();
    const playable = (r: number) => planOpen(caps, r, this.config.dsp_enabled ? ratioCandidates(chain, r, caps, this.config.formats) : [ONE], this.config.formats) !== null;
    let native = policy.allowed_rates.filter(playable);
    if (native.length === 0) native = [...RATES_44, ...RATES_48].filter(playable);
    native = [...native].sort((a, b) => a - b);
    const fam = (r: number) => (r % 11025 === 0 ? 44100 : 48000);
    const defaultRate = native.includes(policy.default_rate)
      ? policy.default_rate
      : ([...native].sort(
          (a, b) =>
            Number(fam(a) !== fam(policy.default_rate)) - Number(fam(b) !== fam(policy.default_rate)) ||
            Math.abs(a - policy.default_rate) - Math.abs(b - policy.default_rate) ||
            Number(a !== 48000) - Number(b !== 48000),
        )[0] ?? policy.default_rate);
    const converted = this.config.dsp_enabled ? [] : policy.allowed_rates.filter((r) => !native.includes(r) && this.bypassLanding(r) !== null);
    const allowed = [...native, ...converted].sort((a, b) => a - b);
    return { allowed, defaultRate, native };
  }

  /**
   * Like io-pw `rate_caps::playable_rates`: every engine rate the device
   * plays with the current settings, allowed or not (the DSP sink's rule
   * and the engine bypass resampler's). Empty without a probe.
   */
  playableRates(): number[] {
    const caps = capsFor(this.config.device);
    if (!caps) return [];
    const formats = this.openFormats();
    const chain = this.chainOn();
    const engineRates = [...RATES_44, ...RATES_48].sort((a, b) => a - b);
    const dsp = (r: number) => planOpen(caps, r, this.config.dsp_enabled ? ratioCandidates(chain, r, caps, formats) : [ONE], formats) !== null;
    return engineRates.filter((r) => dsp(r) || this.bypassLanding(r) !== null);
  }

  /** The formats the open plan tries (empty: the driver's preference). */
  private openFormats(): SampleFormat[] {
    return this.config.formats.length ? this.config.formats : (["s32", "s24", "s24_3", "s16"] as SampleFormat[]);
  }

  /**
   * Like io-pw `DesiredDspConfig::bypass_landing`: the native rate the engine
   * bypass resampler converts `rate` to for Direct and DSP off (the allowed
   * rate the device plays at or above it, else the closest; any native
   * engine rate when no allowed one is native). Null when the device plays
   * `rate` natively, with `bypass_resampler: "pipewire"`, or without a
   * probe. The mock builds every pair within the rational term limit.
   */
  bypassLanding(rate: number): number | null {
    if ((this.config.rate.bypass_resampler ?? "engine") !== "engine") return null;
    const caps = capsFor(this.config.device);
    if (!caps) return null;
    const formats = this.openFormats();
    const native = (r: number) => devicePlaysAt(caps, r, formats) !== null;
    if (native(rate)) return null;
    const engineRates = [...RATES_44, ...RATES_48].sort((a, b) => a - b);
    const pick = (list: number[]) => {
      if (list.length === 0) return null;
      return list.find((r) => r >= rate) ?? [...list].sort((a, b) => Math.abs(a - rate) - Math.abs(b - rate))[0];
    };
    const out = pick(engineRates.filter((r) => native(r) && this.config.rate.allowed_rates.includes(r))) ?? pick(engineRates.filter(native));
    if (out === null) return null;
    const r = rateRatioOf(rate, out);
    return r.up <= MAX_RATIONAL_TERM && r.down <= MAX_RATIONAL_TERM ? out : null;
  }

  /** Like io-pw's ladder for Direct and DSP off: the bypass resampler's ratio, then x1. */
  private bypassLadder(rate: number): RateRatio[] {
    const out = this.bypassLanding(rate);
    return out === null ? [ONE] : [rateRatioOf(rate, out), ONE];
  }

  /**
   * Like io-pw `direct_rate_policy`: effective rates the device plays
   * natively (`native`; every native engine rate when none is, with the
   * engine bypass resampler), and the configured or effective ones the
   * engine converts on top.
   */
  directPolicy(): { allowed: number[]; native: number[] } {
    const caps = capsFor(this.config.device);
    const allowed = this.effectivePolicy().allowed;
    if (!caps) return { allowed, native: allowed };
    const formats = this.openFormats();
    const plays = (r: number) => formats.some((f) => caps.rates.some((c) => c.rate === r && c.formats.some((x) => x.format === f)));
    let native = allowed.filter(plays);
    if (native.length === 0 && (this.config.rate.bypass_resampler ?? "engine") === "engine") native = [...RATES_44, ...RATES_48].filter(plays).sort((a, b) => a - b);
    const converted = [...new Set([...this.config.rate.allowed_rates, ...allowed])].filter((r) => !native.includes(r) && this.bypassLanding(r) !== null);
    return { allowed: [...native, ...converted].sort((a, b) => a - b), native };
  }

  directRates(): number[] {
    return this.directPolicy().allowed;
  }

  /**
   * Like io-pw `rate_arbiter::sink_rate`: a client's rate when the sink
   * plays it, else the next higher (or closest) of the sink's native rates.
   * A converted rate is only chosen for a client at exactly that rate, so
   * nothing is converted twice and a native rate never.
   */
  private sinkRate(r: number, allowed: number[], native: number[]): number {
    if (allowed.includes(r)) return r;
    const base = native.length ? native : allowed;
    if (base.length === 0) return r;
    return base.find((a) => a >= r) ?? [...base].sort((a, b) => Math.abs(a - r) - Math.abs(b - r))[0];
  }

  /**
   * The graph rate: like the engine's rate arbiter, without its timers. One
   * app (or several that agree) is followed; when they disagree
   * `rate.conflict` decides. Streams are listed in start order.
   */
  private graphRate(): number {
    const policy = this.config.rate;
    const eff = this.effectivePolicy();
    if (!policy.follow_source) return eff.defaultRate;
    // A stream at a rate the device cannot play maps onto the next allowed
    // rate (PipeWire resamples it), like `choose_allowed_rate`. Direct's
    // clients map through Direct's rates (the engine bypass resampler's
    // included), the DSP sink's through its own, like the arbiter.
    const onDirect = this.activeSink() === "direct";
    const direct = onDirect ? this.directPolicy() : { allowed: [], native: [] };
    const [allowed, native] = direct.allowed.length ? [direct.allowed, direct.native] : [eff.allowed, eff.native];
    // Like the arbiter: every client's rate maps onto the sink's rates.
    const wanted = this.feeding()
      .map((s) => s.rate)
      .filter((r): r is number => !!r)
      .map((r) => this.sinkRate(r, allowed, native));
    // Direct keeps the current rate while a client still plays at it
    // (`keep_current`, whatever `rate.conflict` says), as the arbiter does.
    const conflict = onDirect ? "keep_current" : policy.conflict;
    let rate: number;
    if (wanted.length === 0) rate = eff.allowed.includes(this.lastRate) ? this.lastRate : eff.defaultRate;
    else if (wanted.every((r) => r === wanted[0])) rate = wanted[0];
    else if (conflict === "highest") rate = Math.max(...wanted);
    else if (conflict === "default_rate") rate = eff.defaultRate;
    else rate = wanted.includes(this.lastRate) ? this.lastRate : wanted[0];
    this.lastRate = rate;
    return rate;
  }

  /** As the daemon: a chain fallback is per source rate; another rate ends it, and the rate left counts as overloaded. */
  private perRateEnd() {
    const o = this.overload;
    if (!o || !o.source_rate || o.action.startsWith("varispeed") || this.graphRate() === o.source_rate) return;
    if (!this.trippedRates.includes(o.source_rate)) this.trippedRates.push(o.source_rate);
    this.overload = null;
  }

  status(): Status {
    this.perRateEnd();
    const st = this.currentStatus();
    if (!this.legacyRatio) return st;
    // A daemon that predates `rate_ratio`: none (and an integer `ratio`).
    const { rate_ratio: _gone, ...old } = st;
    void _gone;
    return old as Status;
  }

  private currentStatus(): Status {
    // The engine runs the overload fallback, not the saved upsampler.
    const saved = this.chainOn();
    const chain = this.overload?.action === "upsampler_off" ? { ...saved, upsampler: { ...saved.upsampler, target: "off" as const } } : saved;
    const sink = this.activeSink();
    // Direct never upsamples: it moves the graph to a native rate.
    const direct = this.directPolicy();
    const directRates = direct.allowed;
    let rate = this.graphRate();
    if (sink === "direct" && directRates.length && !directRates.includes(rate)) rate = this.sinkRate(rate, directRates, direct.native);
    const dspPath = this.config.dsp_enabled && this.overload?.action !== "bypass" && sink !== "direct";
    const upsamplerRates = this.upsamplerRates();
    const upsamplerFallbackNow = dspPath && this.feeding().length > 0 ? upsamplerRates.find((r) => r.source_rate === rate && r.fallback) : undefined;
    const caps = capsFor(this.config.device);
    const candidates = dspPath ? ratioCandidates(chain, rate, caps, this.config.formats) : this.bypassLadder(rate);
    const plan = caps ? planOpen(caps, rate, candidates, this.config.formats) : { ratio: candidates[0] ?? ONE, format: this.config.formats[0] ?? null };
    const rateRatio = plan?.ratio ?? ONE;
    const outputRate = outRate(rate, rateRatio);
    const ratio = rateRatio.down === 1 ? rateRatio.up : 0;
    const format = this.released ? null : (plan?.format ?? null);
    const eff = this.effectivePolicy();
    const feeding = this.feeding();
    const breaks: PassthroughBreak[] = [];
    if (feeding.length === 0) breaks.push({ reason: "not_playing" });
    // The engine bypass resampler's conversion processes the samples too
    // (it resamples and dithers, at unity volume included).
    const converting = !dspPath && rateRatio.up !== rateRatio.down && feeding.length > 0;
    const varispeed = this.varispeedStatus(dspPath && !this.released);
    const dspActive = (dspPath && !chainIsPassthrough(chain)) || converting || varispeed.active;
    if (dspActive) breaks.push({ reason: "dsp_active" });
    if (ratio > 1) breaks.push({ reason: "upsampling", ratio });
    else if (rateRatio.up !== rateRatio.down) breaks.push({ reason: "rate_conversion", graph_rate: rate, output_rate: outputRate });
    if (varispeed.active) breaks.push({ reason: "varispeed", speed: varispeed.speed });
    // A non-unity digital part marks the engine DSP-active on every path:
    // DSP, Direct and bypass (control-plane.md, "Digital volume & balance").
    const digitalPart = Math.abs(this.volume.software_db) >= 0.02 || (!this.volume.hardware && this.volume.muted);
    if (!dspActive && digitalPart) breaks.push({ reason: "dsp_active" });
    for (const s of feeding) if ((s.volume ?? 1) !== 1 || s.muted) breaks.push({ reason: "volume", node: s.node, volume: s.muted ? 0 : (s.volume ?? 1) });
    if (feeding.length > 1) breaks.push({ reason: "mixing", streams: feeding.length });
    for (const s of feeding) if (s.rate && s.rate !== rate) breaks.push({ reason: "resampled", stream_rate: s.rate, graph_rate: rate });
    const resampling = rateRatio.up !== rateRatio.down;
    const design = dspPath && resampling ? this.design(chain.upsampler, rate, rateRatio) : null;
    const upTaps = design ? design.taps : 0;
    const upLat = design ? (design.latency / outputRate) * 1000 : 0;
    const convLat = dspPath && convList(chain).some((c) => c.enabled) ? 2.7 : 0;
    const pitchLat = dspPath && stagePitchActive(chain.pitch) ? (chain.pitch.quality === "max" && rate <= 96000 ? 240 : 120) : 0;
    const limLat = dspPath && chain.limiter && chain.limiter.mode !== "off" ? limiterDelay(chain.limiter.lookahead_ms, outputRate).ms : 0;
    this.latency = (this.config.quantum / 48000) * 1000 * 2 + upLat + convLat + pitchLat + limLat;
    const loadBase = !dspPath ? 0.004 : 0.012 + convList(chain).filter((c) => c.enabled).length * 0.02 + (resampling ? (upTaps / 65536) * 0.03 * (Math.max(1, rateRatio.up / rateRatio.down) / 4) : 0);
    const partPresets = PRESET_PARTS.flatMap((part) => {
      const state = this.presetState(part);
      return state ? [{ part, ...state }] : [];
    });
    return {
      version: this.engineVersion,
      state: !this.running ? "error" : this.released ? "idle" : feeding.length ? "playing" : "ready",
      conv_warnings: [],
      calibration: this.calibrationStatus(),
      device: this.config.device,
      error: this.engineError,
      format,
      graph_rate: rate,
      output_rate: outputRate,
      rate_ratio: rateRatio,
      ratio,
      quantum: this.config.quantum,
      latency_ms: Math.round(this.latency * 10) / 10,
      filter_latency_ms: Math.round((upLat + convLat + limLat) * 10) / 10,
      conv_latency_ms: convLat,
      xruns: this.xruns,
      dsp_load: Math.round(loadBase * 1000) / 1000,
      dsp_enabled: this.config.dsp_enabled,
      active_sink: sink,
      passthrough: {
        intact: breaks.length === 0,
        broken_because: breaks,
        caveats: [],
      },
      ab: this.config.ab ? this.abStatus() : null,
      volume: this.volumeStatus(),
      preset: this.presetState(undefined),
      part_presets: partPresets,
      device_caps: this.released ? null : caps,
      effective_rates: this.released ? [] : [...new Set([...eff.allowed, ...directRates])].sort((a, b) => a - b),
      direct_rates: this.released ? [] : directRates,
      playable_rates: this.released ? [] : this.playableRates(),
      bypass_resampler: this.released ? null : this.bypassStatus(sink, rate, rateRatio, feeding),
      effective_default_rate: this.released ? null : eff.defaultRate,
      dither_bits: format ? Math.min(this.config.chain.output.bits, format === "s16" ? 16 : format === "s32" ? 32 : 24) : null,
      limiter_held_db: this.config.chain.limiter?.mode !== "off" ? this.limHeld : 0,
      // System-wide routing makes Audivane DSP the default, so there is no
      // other "normal" output to name.
      normal_output: this.config.routing.mode === "apps" ? "Built-in Audio Analog Stereo" : null,
      eq_tilt: this.tiltStatus(this.config.chain, rate),
      eq_tilt_b: this.config.ab ? this.tiltStatus(chainB(this.config.ab, this.config.chain), rate) : null,
      overload: this.overload ? clone(this.overload) : null,
      upsampler_rates: upsamplerRates,
      ...(upsamplerFallbackNow ? { upsampler_fallback_now: upsamplerFallbackNow } : {}),
      rt_load_max: 0,
      varispeed,
      limits: { ...MOCK_LIMITS },
      waiting_for_device: false,
      outputs: this.outputsStatus(sink, outputRate),
    };
  }

  /**
   * `status.outputs`, like the engine's: each follower opens, locks over
   * a few seconds (muted), then plays in sync. The SPK card's large periods
   * leave it unaligned (late by 42 ms) once the master's alignment settles
   * at 60 ms; a device missing from the list, or in `lostFollowers`, is lost.
   */
  private outputsStatus(sink: SinkChoice | null, masterRate: number): OutputsStatus {
    const list = this.config.outputs.followers;
    const now = this.followerClock();
    const heard = sink !== "direct" && this.config.dsp_enabled;
    const followers = list.map((f): FollowerStatus => {
      const listed = DEVICES.find((d) => d.id === f.device);
      const card = listed?.card ?? "";
      const lost = !listed || this.lostFollowers.has(f.device);
      const age = now - (this.followerSince.get(f.device) ?? now);
      const running = this.running && !this.released;
      const state: FollowerState = !running ? "off" : lost ? "lost" : age < 400 ? "opening" : age < 3000 ? "locking" : !f.enabled || !heard ? "muted" : card === "SPK" ? "unaligned" : "locked";
      const open = state !== "off" && state !== "lost" && state !== "opening";
      // A steady clock offset per card, settling while it locks.
      const seed = [...f.device].reduce((a, c) => (a * 31 + c.charCodeAt(0)) % 997, 7);
      const ppm = ((seed % 120) - 60) / 1.7;
      const settle = state === "locking" ? 1 + (3000 - age) / 600 : 1;
      const caps = capsFor(f.device);
      const rate = caps?.rates.some((r) => r.rate === masterRate) ? masterRate : (caps?.rates[caps.rates.length - 1]?.rate ?? masterRate);
      const period = card === "SPK" ? 4096 : 1024;
      return {
        device: f.device,
        enabled: f.enabled,
        trim_ms: f.trim_ms,
        volume_db: f.volume_db,
        follow_volume: f.follow_volume ?? false,
        level_db: f.volume_db,
        state,
        offset_ms: state === "unaligned" || (state === "muted" && card === "SPK") ? 42 : 0,
        ppm: open ? Math.round(ppm * settle * 10) / 10 : 0,
        phase_error_frames: open ? (state === "locking" ? 3.2 : 0.1) : 0,
        bandwidth_hz: open ? (state === "locking" ? 2 : 0.05) : 0,
        skips: open && state !== "locking" ? 1 : 0,
        xruns: 0,
        latency_ms: open ? Math.round(((period * 3) / rate) * 1000 * 10) / 10 : 0,
        device_rate: open ? rate : 0,
        period: open ? period : 0,
        output_channels: f.output_channels,
        device_channels: open ? Math.max(caps?.channels ?? 2, f.output_channels ? Math.max(...f.output_channels) + 1 : 0) : 0,
        device_channels_max: open ? (caps?.channels_max ?? 2) : 0,
        error: state === "lost" ? "the device is not connected" : null,
        own_chain: f.chain_patch ? Object.keys(f.chain_patch) : [],
        latency: zeroLatency(open ? rate : 0),
      };
    });
    const alignment = followers.length === 0 ? 0 : followers.some((f) => f.offset_ms > 0) ? 60 : 6;
    return { alignment_ms: alignment, master: zeroLatency(followers.length ? masterRate : 0), followers, settling: null };
  }

  /** Like the daemon's `validate_outputs`. */
  private validateOutputs(c: EngineConfig) {
    const list = c.outputs?.followers ?? [];
    if (list.length > 3) this.bad(`outputs.followers: at most 3 followers (got ${list.length})`);
    list.forEach((f, i) => {
      const at = `outputs.followers[${i}]`;
      if (!f.device.startsWith("hw:") || f.device.trim() !== f.device) this.bad(`${at}.device "${f.device}" must be an ALSA \`hw:\` device such as "hw:CARD=USB,DEV=0"`);
      if (f.device === c.device) this.bad(`${at}.device ${f.device} is the main output device; a follower plays another one`);
      if (list.slice(0, i).some((g) => g.device === f.device)) this.bad(`${at}.device ${f.device} is listed twice`);
      if (!Number.isFinite(f.trim_ms) || Math.abs(f.trim_ms) > 50) this.bad(`${at}.trim_ms must be between -50 and 50 ms (got ${f.trim_ms})`);
      if (!Number.isFinite(f.volume_db) || f.volume_db < -120 || f.volume_db > 0) this.bad(`${at}.volume_db must be between -120 and 0 dB (got ${f.volume_db})`);
    });
    if (list.length && c.varispeed.enabled && c.varispeed.speed > 1.996) {
      this.bad(`varispeed.speed is at most 1.996 while followers play (got ${c.varispeed.speed}): each follower corrects its clock on top of it`);
    }
  }

  /** Start the walk of followers new to the list; forget removed ones. */
  private syncFollowers() {
    const now = this.followerClock();
    const list = this.config.outputs.followers.map((f) => f.device);
    for (const d of [...this.followerSince.keys()]) if (!list.includes(d)) this.followerSince.delete(d);
    for (const d of list) if (!this.followerSince.has(d)) this.followerSince.set(d, now);
  }

  /**
   * Like the engine's budget: the time stretch alone is over it at
   * 176.4 kHz graph rates and above (it fits up to 96 kHz).
   */
  private checkKeepPitch(rate: number) {
    if (rate >= 176_400) {
      const macs = Math.round((1200 * rate) / 192_000);
      this.bad(
        `Keep pitch is not available at ${rate} Hz: the time stretch alone needs ${macs} M multiply-adds per second per channel at its slowest speed, more than the 800 M budget; play at a lower rate or turn Keep pitch off`,
      );
    }
  }

  /**
   * The varispeed stage, applied at once (no ramp). A quantum below 256 caps
   * the speed at 1.25× (`quantum_too_small`), so the cap note can be seen in
   * the preview.
   */
  private varispeedStatus(dspPath: boolean): VarispeedStatus {
    const v = this.config.varispeed;
    const rate = this.graphRate();
    // Keep pitch at a rate over the budget: the request stays, the stage
    // is off (as the engine after a renegotiation).
    const keepPitchOver = v.enabled && v.keep_pitch && rate >= 176_400;
    const active = v.enabled && dspPath && !keepPitchOver;
    const capped = this.config.quantum < 256;
    const speedLimit = capped ? 1.25 : 2;
    const target = active ? Math.min(v.speed, speedLimit) : 1;
    // The stretch's delay is about 135 ms (6 691 frames at 48 kHz).
    const latency = v.keep_pitch ? Math.round(0.1394 * rate) : 507.5;
    return {
      enabled: v.enabled,
      keep_pitch: v.keep_pitch,
      active,
      speed: target,
      target,
      requested: v.speed,
      speed_limit: speedLimit,
      cap_reason: capped ? "quantum_too_small" : null,
      ramping: false,
      fading: false,
      latency_frames: active ? latency : 0,
      latency_exact: true,
      held: null,
      overload_hold: "none",
      underfeed_events: 0,
      underfeed_frames: 0,
      short_cycles: 0,
      gap_fades: 0,
      error: keepPitchOver ? `Keep pitch is not available at ${rate} Hz: the time stretch is over the CPU budget at this rate` : null,
    };
  }

  /**
   * Like the daemon after a second over real time: the upsampler is
   * switched off in the engine (the saved config is kept) and an
   * `overload` event goes to status subscribers. Mock only (tests, demo).
   */
  simulateOverload(load = 1.4) {
    const upsampling = this.config.chain.upsampler.target !== "off";
    const pct = `${Math.round(load * 100)}%`;
    this.overloadEpisodes += 1;
    this.overload = {
      stage: upsampling ? "upsampler" : "chain",
      output_rate: this.status().output_rate,
      load,
      action: upsampling ? "upsampler_off" : "bypass",
      message: upsampling
        ? `The engine switched the upsampler off: it needed more CPU than real time allows (load ${pct}). Your setting is kept; lower the taps or the output rate.`
        : `The engine switched DSP off: the chain needed more CPU than real time allows (load ${pct}). Your settings are kept; make the chain lighter.`,
      episode: this.overloadEpisodes,
      applied: true,
      source_rate: this.status().graph_rate,
    };
    this.emit({ event: "overload", data: clone(this.overload) as OverloadReport });
    this.emitStatus();
  }

  /**
   * The fallback's first step while A/B is on: A/B off, chain A kept (the
   * config keeps side B). Mock only (tests, demo).
   */
  simulateAbOverload(load = 1.4) {
    if (!this.config.ab) return this.simulateOverload(load);
    this.overloadEpisodes += 1;
    this.overload = {
      stage: "ab",
      output_rate: this.status().output_rate,
      load,
      action: "ab_off",
      message: `The engine turned A/B off and kept chain A: both sides together needed more CPU than real time allows (load ${Math.round(load * 100)}%). Your settings are kept; a new side B (a lighter filter, say) tries again.`,
      episode: this.overloadEpisodes,
      applied: true,
      source_rate: this.status().graph_rate,
    };
    this.abSide = "a";
    this.emit({ event: "overload", data: clone(this.overload) as OverloadReport });
    this.emitStatus();
  }

  /** Overs counted to each side, as after limiter events (until the next `set_ab` or `ab_select`). Mock only. */
  simulateAbOvers(a: number, b: number) {
    this.abOvers = { a, b };
    this.emitStatus();
  }

  private abOvers = { a: 0, b: 0 };

  /**
   * A running `ab_measure`. No audio in the mock: it hears about 8 s of
   * music per real second, its running offset wobbling into the estimate,
   * and follows the engine's lock rule (20 s, then within 0.1 dB over the
   * last 10 s, or `max_seconds`).
   */
  private abMeasure: { seconds: number; max: number; target: number; offset: number | null; stable: boolean; history: [number, number][]; pair: string; timer: ReturnType<typeof setInterval> } | null = null;
  private abMeasureError: string | null = null;
  /** Mock only: the next measurement hears side B as silent (its no-match path; also `?measure=silent`). */
  measureSilentB = false;

  private abPair(): string | null {
    const ab = this.config.ab;
    return ab ? JSON.stringify([this.config.chain, ab.b, ab.headroom]) : null;
  }

  private abMeasureStart(max: number) {
    this.abMeasureEnd(null);
    const silent = this.measureSilentB || (typeof location !== "undefined" && new URLSearchParams(location.search).get("measure") === "silent");
    const timer = setInterval(() => this.abMeasureTick(), 250);
    this.abMeasure = { seconds: 0, max, target: this.abOffset(), offset: null, stable: false, history: [], pair: this.abPair() ?? "", timer };
    if (silent) this.abMeasure.target = Number.NaN;
  }

  private abMeasureEnd(error: string | null) {
    if (this.abMeasure) clearInterval(this.abMeasure.timer);
    this.abMeasure = null;
    this.abMeasureError = error;
  }

  private abMeasureLock(offset: number, seconds: number) {
    this.abMeasureEnd(null);
    if (this.config.ab) this.config.ab.level = { mode: "measured", offset_db: Math.round(offset * 100) / 100, seconds: Math.round(seconds * 10) / 10 };
    this.emitConfig();
  }

  /** One control-plane tick: 2 s more of contributing audio. */
  private abMeasureTick() {
    const m = this.abMeasure;
    if (!m || !this.running) return;
    if (m.stable && m.offset !== null) return this.abMeasureLock(m.offset, m.seconds);
    m.seconds = Math.min(m.max, m.seconds + 2);
    if (!Number.isNaN(m.target)) {
      m.offset = m.target + 0.9 * Math.exp(-m.seconds / 5) * Math.cos(m.seconds * 0.8);
      m.history.push([m.seconds, m.offset]);
      const recent = m.history.filter(([t]) => t >= m.seconds - 10).map(([, o]) => o);
      m.stable = m.seconds >= 20 && Math.max(...recent) - Math.min(...recent) < 0.1;
    }
    if (m.seconds >= m.max) {
      if (m.offset === null) {
        this.abMeasureEnd(`no match: B stayed silent over ${m.seconds.toFixed(0)} s of A`);
        this.emitStatus();
      } else this.abMeasureLock(m.offset, m.seconds);
      return;
    }
    this.emitStatus();
  }

  /** Mock only: the player moves to another track (a running measurement is cancelled). */
  simulateTrackChange(title = "Hammers") {
    if (this.nowPlaying) this.nowPlaying = { ...this.nowPlaying, title };
    this.emit({ event: "now_playing", data: this.nowPlaying });
    if (this.abMeasure) this.abMeasureEnd("cancelled: the track changed");
    this.emitStatus();
  }

  /** The daemon's CPU admission of `next`, and the end of a fallback it replaces. */
  private admitCpu(next: EngineConfig) {
    const st = this.status();
    const refusal = cpuRefusal(this.config, next, st.state === "playing" ? st.graph_rate : null);
    if (refusal) throw new DaemonError(ErrorCode.INVALID_PARAMS, refusal);
    const o = this.overload;
    if (!o) return;
    const part = (c: EngineConfig) => JSON.stringify([c.chain.upsampler, c.chain.output, c.ab !== null]);
    // A/B off ends with a new side B (not B following an edit of A).
    if (o.action === "ab_off") {
      if (JSON.stringify(next.ab?.b ?? null) !== JSON.stringify(this.config.ab?.b ?? null)) this.overload = null;
      return;
    }
    const ends = o.action === "bypass" ? JSON.stringify([next.dsp_enabled, next.chain, next.ab]) !== JSON.stringify([this.config.dsp_enabled, this.config.chain, this.config.ab]) : part(next) !== part(this.config);
    if (ends) {
      this.overload = null;
      this.trippedRates = [];
    }
  }

  /**
   * `Status.bypass_resampler`, like the engine's: active while Direct or DSP
   * off converts, else why PipeWire still resamples a stream there.
   */
  private bypassStatus(sink: SinkChoice | null, rate: number, ratio: RateRatio, feeding: { rate?: number | null }[]): Status["bypass_resampler"] {
    const mode = this.config.rate.bypass_resampler ?? "engine";
    const bypassPath = sink === "direct" || !this.config.dsp_enabled;
    const active = bypassPath && ratio.up !== ratio.down && feeding.length > 0;
    let reason: string | null = null;
    const other = feeding.map((s) => s.rate).find((r): r is number => !!r && r !== rate);
    if (mode === "engine" && bypassPath && !active && other) {
      const why = !this.config.rate.follow_source
        ? "rate following is off"
        : !this.config.rate.allowed_rates.includes(other)
          ? `${other} Hz is not in the allowed rates`
          : feeding.some((s) => s.rate === rate)
            ? `another stream plays at ${rate} Hz`
            : this.bypassLanding(other) === null
              ? `the engine cannot convert ${other} Hz to a rate the DAC plays`
              : "the graph has not moved yet";
      reason = `PipeWire resamples the ${other} Hz stream: ${why}`;
    }
    return { mode, active, reason };
  }

  /** `Status.eq_tilt`: the tilt stage's effective band at the graph rate (both sections), like the engine's. */
  private tiltStatus(chain: ChainConfig, rate: number): Status["eq_tilt"] {
    const eq = chain.eq;
    const on = (
      [
        ["tilt", eq?.tilt],
        ["tilt_high", eq?.tilt_high],
      ] as const
    ).filter((e): e is readonly ["tilt" | "tilt_high", TiltConfig] => !!e[1]?.enabled);
    if (on.length === 0) return null;
    const first = on[0][1];
    const last = on[on.length - 1][1];
    const notes = on
      .filter(([, t]) => effectiveEndHz(t, rate) < t.end_hz)
      .map(([k, t]) => `${k} end_hz clamped from ${t.end_hz.toFixed(3)} to ${effectiveEndHz(t, rate).toFixed(3)} Hz at ${rate} Hz (0.45 * fs)`);
    const pair = eq?.tilt && eq.tilt_high ? tiltPairProblem(eq.tilt, eq.tilt_high) : null;
    const why = pair ? `tilt: ${pair}` : on.map(([k, t]) => (tiltProblem(t, rate) ? `${k}: ${tiltProblem(t, rate)}` : null)).find((e) => e) ?? null;
    return {
      sample_rate: rate,
      effective_start_hz: first.start_hz,
      effective_end_hz: effectiveEndHz(last, rate),
      clamp_note: notes.length ? notes.join("; ") : null,
      error: why,
    };
  }

  /** Never boost: the louder side is cut (no shared headroom in the mock). */
  private abStatus(): AbStatus {
    const o = this.abOffset();
    const gain_a_db = Math.min(0, -o);
    if (!this.config.ab) this.abBlind = false;
    const blind = this.abBlind;
    return {
      enabled: true,
      current: blind ? null : this.abSide,
      blind,
      offset_db: o,
      gain_a_db,
      gain_b_db: Math.min(0, gain_a_db + o),
      headroom_db: 0,
      overs: blind ? { a: 0, b: 0 } : { ...this.abOvers },
      level: this.config.ab?.level ?? { mode: "analytic", weighting: "k" },
      measuring: this.abMeasure
        ? { seconds: this.abMeasure.seconds, offset_db: this.abMeasure.offset === null ? null : Math.round(this.abMeasure.offset * 1000) / 1000, stable: this.abMeasure.stable, max_seconds: this.abMeasure.max }
        : null,
      measure_error: this.abMeasureError,
      error: null,
    };
  }

  private abOffset(): number {
    const ab = this.config.ab;
    if (!ab) return 0;
    if (ab.level.mode === "off") return 0;
    if (ab.level.mode === "manual" || ab.level.mode === "measured") return ab.level.offset_db;
    const loud = (c: ChainConfig) => {
      const eq = c.eq;
      if (!eq) return 0;
      const freqs = [100, 300, 1000, 3000, 6000];
      const l = profileChannelResponseDb(eq, 0, 48000, freqs);
      const r = profileChannelResponseDb(eq, 1, 48000, freqs);
      return (l.reduce((a, b) => a + b, 0) + r.reduce((a, b) => a + b, 0)) / (2 * freqs.length);
    };
    return Math.round((loud(this.config.chain) - loud(chainB(ab, this.config.chain))) * 10) / 10;
  }

  private meters(): Meters {
    this.phase += 0.033;
    const playing = this.feeding().length > 0 && !this.released;
    const muted = this.volume.muted;
    const chain = this.chainOn();
    const eqGain = this.config.dsp_enabled && chain.eq ? chain.eq.preamp_db + 3 : 0;
    const rawBase = -16 + 4 * Math.sin(this.phase * 0.7) + 2 * Math.sin(this.phase * 2.3);
    const digital = !this.volume.hardware && this.activeSink() !== "direct" && this.config.dsp_enabled;
    const ch = (i: number) => {
      if (!playing || muted) return { inP: -120, chain: -120, p: -120, r: -120 };
      const bal = i === 0 ? Math.min(0, -this.volume.balance_db) : Math.min(0, this.volume.balance_db);
      const inP = Math.min(0, rawBase + Math.random() * 7 - (i ? 0.8 : 0));
      const chainP = inP + eqGain;
      // Float peak before the quantizer: after the digital volume, may exceed 0 dBFS.
      const p = chainP + (digital ? this.volume.volume_db + bal : 0);
      return { inP, chain: chainP, p, r: Math.min(0, p) - 9 - Math.random() * 1.5 };
    };
    const l = ch(0);
    const r = ch(1);
    const lim = chain.limiter;
    const dspPath = playing && this.config.dsp_enabled && this.activeSink() !== "direct";
    let limiter: Meters["limiter"];
    if (lim && lim.mode !== "off") {
      // Simulated with a hotter crest than the bars, so the stage has work to do.
      const peak = Math.max(l.p, r.p) + 12;
      let gr = 0;
      if (dspPath) {
        if (lim.mode === "auto_headroom") {
          while (peak - this.limHeadroom > lim.ceiling_dbfs && this.limHeadroom < 12) {
            this.limHeadroom += 0.5;
            this.limEvents++;
          }
          gr = Math.max(this.limHeadroom, peak - lim.ceiling_dbfs);
        } else {
          gr = Math.max(0, peak - lim.ceiling_dbfs);
          if (gr > 0.01 && !this.limReducing) this.limEvents++;
          this.limReducing = gr > 0.01;
        }
        for (const c of [l, r]) c.p = Math.min(c.p, lim.ceiling_dbfs);
      }
      limiter = { active: dspPath, headroom_db: dspPath ? this.limHeadroom : 0, gain_reduction_db: Math.round(gr * 100) / 100, events: this.limEvents };
    }
    const held = limiter?.active ? limiter.headroom_db : 0;
    if (held !== this.limHeld) {
      this.limHeld = held;
      this.emitStatus();
    }
    if (l.p >= 0) this.clippedCh[0]++;
    if (r.p >= 0) this.clippedCh[1]++;
    this.clipped = this.clippedCh[0] + this.clippedCh[1];
    return {
      peak_db: [l.p, r.p],
      rms_db: [l.r, r.r],
      clipped: this.clipped,
      clipped_ch: [...this.clippedCh],
      input_peak_db: [l.inP, r.inP],
      chain_peak_db: playing && this.config.dsp_enabled && this.activeSink() !== "direct" ? [l.chain, r.chain] : [],
      xruns: 0,
      dsp_load: this.status().dsp_load,
      ...(limiter ? { limiter } : {}),
    };
  }

  private spectrum(): Spectrum {
    const rate = this.graphRate();
    const outputRate = this.status().output_rate || rate;
    const fMin = 10;
    const bpo = 24;
    // Like the daemon: each tap up to its own Nyquist (less a little) or
    // 100 kHz, no bin straddling the top.
    const binCount = (r: number) => {
      const top = Math.min(r / 2 - 100, 100000);
      return Math.max(1, Math.floor(bpo * Math.log2(top / fMin) - 0.5) + 1);
    };
    const t = this.phase;
    const levelAt = (i: number, f: number) => {
      const oct = Math.log2(f / 55);
      const tilt = oct > 0 ? -3.0 * oct : -1.2 * Math.abs(oct);
      // Steeper above 20 kHz: little real content up there.
      const ultra = f > 20000 ? -30 * Math.log2(f / 20000) : 0;
      const motion =
        4.5 * Math.sin(t * 1.5 + i * 0.08) +
        3.0 * Math.cos(t * 2.4 - i * 0.13) +
        2.0 * Math.sin(t * 0.7 + Math.log2(f) * 2.0);
      return Math.max(-88, Math.min(-3, -24 + tilt + ultra + motion));
    };
    const tap = (n: number) => {
      const freqs: number[] = [];
      const db: number[] = [];
      for (let i = 0; i < n; i++) {
        const f = fMin * Math.pow(2, i / bpo);
        freqs.push(f);
        db.push(levelAt(i, f));
      }
      return { freqs, db };
    };
    const input = tap(binCount(rate));
    const pre = input.db.map((v) => Math.round(v * 10) / 10);
    // Past the source's Nyquist the output holds only the upsampler's stopband.
    const output = tap(binCount(outputRate));
    const outDb = output.db.map((v, i) => (output.freqs[i] < rate / 2 ? v : -88));
    const chain = this.chainOn();
    let post: number[];
    if (this.config.dsp_enabled && chain.eq) {
      // The EQ runs at the graph rate, below its Nyquist.
      const inBand = output.freqs.filter((f) => f < rate / 2);
      const l = profileChannelResponseDb(chain.eq, 0, rate, inBand);
      const r = profileChannelResponseDb(chain.eq, 1, rate, inBand);
      post = outDb.map((v, i) => Math.round(Math.max(-90, Math.min(0, v + (i < inBand.length ? (l[i] + r[i]) * 0.5 : 0))) * 10) / 10);
    } else {
      post = outDb.map((v) => Math.round(v * 10) / 10);
    }
    return { f_min_hz: fMin, bins_per_octave: bpo, pre, post, rate };
  }

  /** The error budget an automatic design aims at, dB (`match_output` from chain A's output, like the daemon). */
  private autoBudgetDb(cfg: LegacyUpsampler): number {
    const b = cfg.auto_taps;
    if (!b) return kaiserAttenuationDb(cfg.kaiser_beta);
    if (b !== "match_output") return b.db;
    const bits = Math.min(24, this.config.chain.output.bits);
    const dithered = this.config.chain.output.dither !== "none";
    return Math.min(6.02 * bits + 1.76 - (dithered ? 4.7712 : 0), dithered ? 141.47 : 146.24);
  }

  /**
   * Roughly what the engine builds for `cfg` from `sourceRate` at `ratio`:
   * enough for plausible plots, latency and load, not the real design.
   * `taps` is on the intermediate grid (the output rate for integer ratios),
   * `cutoff` a fraction of the lower of the two Nyquists, `tw` in Hz.
   */
  private design(cfg: UpsamplerConfig, sourceRate: number, ratio: RateRatio = resolveRateRatio(cfg.target, sourceRate)): MockDesign {
    const out = outRate(sourceRate, ratio);
    const edge = Math.min(sourceRate, out) / 2;
    const perOut = out / sourceRate;
    const block = Math.round(1024 * perOut);
    const latencyFor = (taps: number, phase: FilterPhase) =>
      (phase === "linear" ? Math.floor((taps - 1) / 2 / ratio.down) : phase === "intermediate" ? Math.floor((0.375 * taps) / ratio.down) : 0) + block;
    const modelled = cfg.preset ?? modelPreset(cfg.design);
    if (modelled && modelled !== RECOMMENDED_PRESET) {
      const preset = modelled as FilterPreset;
      const p = presetEntry(preset);
      const t = ratio.down === 1 ? presetTapsPerInput(preset, sourceRate, ratio.up, out) : presetRationalTaps(preset, sourceRate, ratio);
      const taps = t * ratio.up * (p?.phase === "intermediate" && ratio.down !== 1 ? 4 : 1);
      const phase = p?.phase ?? "linear";
      const atten = PRESET_ATTEN_DB[p?.family ?? "gaussian"] ?? 140;
      const tw = Math.max(0.5, Math.min(0.24 * edge, (9 * 2 * edge) / Math.max(2, t)));
      return { ratio, taps, phase, cutoff: presetCutoffFraction(preset, sourceRate), atten, beta: 0, tw, latency: latencyFor(taps, phase) };
    }
    const lu = fromUpsamplerConfig(cfg);
    const scale = Math.max(ratio.up, ratio.down);
    const window = lu.window === "gaussian" ? 2 : 1;
    const mixed = lu.phase === "intermediate" && lu.phase_mix > 0 && lu.phase_mix < 1;
    if (lu.auto_taps || lu.cutoff_mode === "band") {
      const atten = lu.auto_taps ? this.autoBudgetDb(lu) : lu.band.attenuation_db;
      const pass = bandPassbandEnd(lu.band, bandRate(sourceRate, ratio));
      const delta = Math.max(0.005, lu.band.stopband_start - pass);
      const k = Math.ceil(((atten - 7.95) / (14.36 * delta)) * window);
      let taps = Math.min(4_194_304, 2 * k * scale + 1);
      if (mixed) taps = Math.round(taps * 1.45);
      const beta = atten > 50 ? 0.1102 * (atten - 8.7) : 0.5842 * Math.pow(atten - 21, 0.4) + 0.07886 * (atten - 21);
      const tw = Math.max(0.5, delta * edge);
      const cutoff = (pass + lu.band.stopband_start) / 2;
      const band = { pass, stop: pass + delta };
      return { ratio, taps, phase: lu.phase, cutoff, atten, beta, tw, latency: latencyFor(taps, lu.phase), band };
    }
    const base = lu.taps_unit === "input" ? lu.taps * ratio.up : ratio.down > 1 ? lu.taps * ratio.down : lu.taps;
    const taps = mixed ? Math.round(base * 1.45) : base;
    const cutoff = lu.cutoff_mode === "nyquist_stop" ? nyquistStopCutoff(base, ratio.up, lu.kaiser_beta) : Math.max(0.5, Math.min(1, lu.cutoff));
    const tw = Math.max(0.5, Math.min(0.24 * edge, ((lu.kaiser_beta + 2) / (Math.PI * Math.max(256, base / window))) * sourceRate * ratio.up * 2.5));
    // Stop at Nyquist reaches its full depth by the source Nyquist, as the
    // engine's design does: drawn as a band ending there, centred on the cutoff.
    const band = lu.cutoff_mode === "nyquist_stop" ? { pass: Math.max(0.5, 2 * cutoff - 1), stop: 1 } : undefined;
    return { ratio, taps, phase: lu.phase, cutoff, atten: kaiserAttenuationDb(lu.kaiser_beta), beta: lu.kaiser_beta, tw, latency: latencyFor(taps, lu.phase), band };
  }

  /** Smooth analytic stand-in for the measured magnitude (dB) of a design. */
  private mockMagnitude(cfg: UpsamplerConfig, sourceRate: number): (f: number) => number {
    const src = Math.max(1, Math.round(sourceRate));
    const ratio = resolveRateRatio(cfg.target, src);
    if (ratio.up === ratio.down) return () => 0;
    const d = this.design(cfg, src, ratio);
    const edge = Math.min(src, outRate(src, ratio)) / 2;
    const fc = d.cutoff * edge;
    const atten = Math.min(230, Math.max(40, d.atten));
    const tw = d.tw;
    const stopFloor = Math.pow(10, -atten / 20);
    // A band design is symmetric about the band's middle, like a windowed
    // sinc: the stopband depth at the stop edge and the same tiny deviation
    // at the pass edge.
    let centre = fc;
    let scale = tw * 0.14;
    if (d.band) {
      const xStop = Math.log(Math.pow(10, atten / 20) - 1);
      centre = ((d.band.pass + d.band.stop) / 2) * edge;
      scale = Math.max(1e-3, ((d.band.stop - d.band.pass) * edge) / 2 / xStop);
    }
    return (f: number): number => {
      if (f <= 0) return 0;
      const trans = 1 / (1 + Math.exp((f - centre) / scale));
      const passRipple = 1 + 0.000002 * Math.sin(f / 320);
      const stopLobe = stopFloor * (0.45 + 0.55 * Math.abs(Math.sin((f - fc) / Math.max(0.2, tw * 0.4))));
      const lin = Math.max(1e-12, trans * passRipple + (1 - trans) * stopLobe);
      return 20 * Math.log10(lin);
    };
  }

  /** Threshold crossings of the analytic magnitude, by scan + bisection. */
  private mockCrossings(mag: (f: number) => number, nyquistOut: number, atten: number): { level_db: number; freq_hz: number }[] {
    const levels = [-0.0001, -0.001, -0.01, -0.1, -0.5, -1, -3, -6, -20, -60, -100, -120, -150];
    const out: { level_db: number; freq_hz: number }[] = [];
    const steps = 200000;
    for (const level of levels) {
      if (level < -(atten - 3)) continue;
      let prev = 0;
      for (let i = 1; i <= steps; i++) {
        const f = (nyquistOut * i) / steps;
        if (mag(f) <= level) {
          let lo = prev;
          let hi = f;
          for (let k = 0; k < 60 && hi - lo > 1e-4; k++) {
            const mid = (lo + hi) / 2;
            if (mag(mid) > level) lo = mid;
            else hi = mid;
          }
          out.push({ level_db: level, freq_hz: (lo + hi) / 2 });
          break;
        }
        prev = f;
      }
    }
    return out;
  }

  private buildUpsamplerResponse(cfg: UpsamplerConfig, sourceRate: number): UpsamplerResponse {
    const source_rate = Math.max(1, Math.round(sourceRate));
    const rate_ratio = resolveRateRatio(cfg.target, source_rate);
    const output_rate = outRate(source_rate, rate_ratio);
    const ratio = rate_ratio.down === 1 ? rate_ratio.up : 0;
    // The band edge is the lower Nyquist; the plot runs, like the daemon's,
    // to the Nyquist of the grid it measures on: source x up (the output's
    // for an integer ratio, the intermediate grid's for a fractional one).
    const nyquistIn = Math.min(source_rate, output_rate) / 2;
    const nyquistOut = (source_rate * rate_ratio.up) / 2;
    // The statistics scan to the higher of the two Nyquists (fine enough
    // steps; above it a fractional grid's response is stopband anyway).
    const statTop = Math.max(source_rate, output_rate) / 2;

    const rawFreqs: number[] = [0];
    const lo = Math.max(1, Math.min(20, nyquistOut / 2));
    for (let i = 0; i < 600; i++) {
      rawFreqs.push(lo * Math.pow(nyquistOut / lo, i / 599));
    }
    const zFrom = 0.8 * nyquistIn;
    const zTo = Math.min(1.25 * nyquistIn, nyquistOut);
    if (zTo > zFrom) {
      for (let i = 0; i <= 400; i++) {
        rawFreqs.push(zFrom + (zTo - zFrom) * (i / 400));
      }
    }
    rawFreqs.sort((a, b) => a - b);
    const freqs_hz: number[] = [];
    for (const f of rawFreqs) {
      if (f >= 0 && f <= nyquistOut + 1e-9 && (freqs_hz.length === 0 || Math.abs(f - freqs_hz[freqs_hz.length - 1]) > 1e-9)) {
        freqs_hz.push(f);
      }
    }

    const lu = fromUpsamplerConfig(cfg);
    if (rate_ratio.up === rate_ratio.down) {
      return {
        source_rate,
        device_landings: [],
        output_rate,
        ratio: 1,
        rate_ratio: { up: 1, down: 1 },
        taps: lu.taps,
        phase: lu.phase,
        filter_window: lu.window,
        effective_cutoff: 1,
        effective_kaiser_beta: lu.kaiser_beta,
        sample_leak_db: -400,
        freqs_hz,
        magnitude_db: freqs_hz.map(() => 0),
        impulse: [1],
        impulse_peak: 0,
        passband_ripple_db: 0,
        minus_0_1_db_hz: 0,
        minus_3_db_hz: 0,
        stopband_start_hz: 0,
        stopband_atten_db: 0,
        latency_samples: 0,
        crossings: [],
      };
    }

    const d = this.design(cfg, source_rate, rate_ratio);
    const cutoff = d.cutoff;
    const fc = cutoff * nyquistIn;
    const tw = d.tw;
    const magDbAt = this.mockMagnitude(cfg, source_rate);
    const perOut = output_rate / source_rate;

    const magnitude_db = freqs_hz.map(magDbAt);
    const STAT_N = 16384;
    let minPb = Infinity;
    let maxPb = -Infinity;
    let minus_0_1_db_hz = fc;
    let minus_3_db_hz = fc;
    let found01 = false;
    let found3 = false;
    let worstStopDb = -Infinity;
    const stopFrom = 1.2 * nyquistIn;
    const statFreq: number[] = [];
    const statDb: number[] = [];
    for (let i = 1; i <= STAT_N; i++) {
      const f = (statTop * i) / STAT_N;
      const db = magDbAt(f);
      statFreq.push(f);
      statDb.push(db);
      if (f <= 0.9 * nyquistIn) {
        if (db < minPb) minPb = db;
        if (db > maxPb) maxPb = db;
      }
      if (!found01 && db <= -0.1) {
        minus_0_1_db_hz = f;
        found01 = true;
      }
      if (!found3 && db <= -3.0) {
        minus_3_db_hz = f;
        found3 = true;
      }
      if (f >= stopFrom && db > worstStopDb) worstStopDb = db;
    }
    const stopband_atten_db = Math.max(0, -worstStopDb);
    const threshold = -stopband_atten_db;
    let stopband_start_hz = Math.min(statTop, fc + tw * 0.85);
    for (let i = statDb.length - 1; i >= 0; i--) {
      if (statFreq[i] >= nyquistIn && statDb[i] > threshold) {
        stopband_start_hz = statFreq[Math.min(statFreq.length - 1, i + 1)];
        break;
      }
    }

    const crossings = this.mockCrossings(magDbAt, statTop, stopband_atten_db);
    const crossingAt = (level: number) => crossings.find((c) => c.level_db === level)?.freq_hz;
    minus_0_1_db_hz = crossingAt(-0.1) ?? minus_0_1_db_hz;
    minus_3_db_hz = crossingAt(-3) ?? minus_3_db_hz;

    const impulseLen = 256;
    const impulse: number[] = [];
    const impulse_peak = d.phase === "linear" ? 128 : d.phase === "intermediate" ? 96 : 24;
    for (let i = 0; i < impulseLen; i++) {
      const n = i - impulse_peak;
      if (d.phase === "minimum" && n < -6) {
        impulse.push(0);
        continue;
      }
      const w = 0.5 - 0.5 * Math.cos((2 * Math.PI * i) / (impulseLen - 1));
      const arg = (Math.PI * cutoff * n) / Math.max(perOut, 1);
      const sinc = arg === 0 ? 1 : Math.sin(arg) / arg;
      const decay = d.phase !== "linear" && n > 0 ? Math.exp(-n / (d.phase === "minimum" ? 90 : 140)) : d.phase === "intermediate" && n < 0 ? Math.exp(n / 40) : 1;
      impulse.push(sinc * w * decay);
    }

    const isNamedPreset = cfg.preset !== null && cfg.preset !== RECOMMENDED_PRESET;
    const namedPreset = isNamedPreset ? (cfg.preset as FilterPreset) : null;
    return {
      source_rate,
      device_landings: [],
      output_rate,
      ratio,
      rate_ratio,
      taps: d.taps,
      phase: d.phase,
      // Presets report neutral placeholders here, like the daemon.
      filter_window: namedPreset ? "kaiser" : lu.window,
      effective_cutoff: namedPreset ? 1 : cutoff,
      effective_kaiser_beta: namedPreset ? 0 : d.beta,
      sample_leak_db: rate_ratio.up < rate_ratio.down ? 0 : namedPreset ? (presetEntry(namedPreset)?.cutoff === "nyquist" ? -300 : -40) : lu.preserve_samples ? -300 : -40,
      freqs_hz,
      magnitude_db,
      impulse,
      impulse_peak,
      passband_ripple_db: Number.isFinite(minPb) ? Math.max(0, maxPb - minPb) : 0,
      minus_0_1_db_hz,
      minus_3_db_hz,
      stopband_start_hz,
      stopband_atten_db,
      latency_samples: d.latency,
      crossings,
      preset: namedPreset,
      ...this.responseDesign(cfg, source_rate),
    };
  }

  /** The `design` a response measured and its `plan` (with the bounded-input peak). */
  private responseDesign(cfg: UpsamplerConfig, rate: number): { design?: FilterDesign; plan?: DesignPlan } {
    const design = cfg.design ?? catalogueDesign(cfg.preset ?? RECOMMENDED_PRESET);
    if (!design) return {};
    const plan = mockCheckDesign(design, this.checkContext({ ...cfg, design, preset: null }, [rate])).plans[0]?.plan;
    if (!plan) return { design };
    const kernel = design.kind === "windowed_sinc" || design.kind === "lanczos" || design.kind === "optimal";
    return { design, plan: { ...plan, grid_taps_exact: true, ...(kernel ? { bounded_peak_db: design.phase.type === "linear" ? 2.1 : 2.6 } : {}) } };
  }

  /**
   * `status.upsampler_rates`: the CPU admission's rows, with Recommended at
   * the rates chain A's custom design does not build at (`cause: design`).
   */
  private upsamplerRates(): RateAdmission[] {
    const rows = rateAdmissions(this.config, this.trippedRates);
    const u = this.config.chain.upsampler;
    if (!u.design || u.preset || !this.config.dsp_enabled || u.target === "off") return rows;
    const falls = mockDesignFallbacks(u.design, this.checkContext(u, this.config.rate.allowed_rates));
    return rows.map((row) => {
      const f = falls.find((x) => x.source_rate === row.source_rate);
      return f ? { ...row, fallback: { action: "recommended" as const, cause: "design" as const, load: 0.05, reason: f.reason } } : row;
    });
  }

  /** What `check_design` checks against: chain A's target and output, the given rates. */
  private checkContext(u: UpsamplerConfig, rates: readonly number[]): MockCheckContext {
    const bits = Math.min(24, this.config.chain.output.bits);
    const dithered = this.config.chain.output.dither !== "none";
    return {
      rates,
      ratioAt: (rate) => resolveRateRatio(u.target, rate),
      maxKernelTaps: MOCK_LIMITS.max_kernel_taps,
      autoDb: Math.min(6.02 * bits + 1.76 - (dithered ? 4.7712 : 0), dithered ? 141.47 : 146.24),
      pipelines: rationalPipelines(!!this.config.ab),
      block: (rate) => Math.min(8192, Math.max(512, 2 ** Math.floor(Math.log2(Math.max(32, Math.floor((this.config.quantum * rate) / 48000)))))),
    };
  }

  private listDevices(): DeviceInfo[] {
    return DEVICES.map((d) => ({ ...d, active: sameDevice(d, this.config.device) }));
  }

  private listPresets(): PresetInfo[] {
    const order = (part: PresetPart | undefined) => (part ? PRESET_PARTS.indexOf(part) + 1 : 0);
    return [...this.presets.values()].map((p) => this.info(p)).sort((a, b) => order(a.part) - order(b.part) || a.name.localeCompare(b.name));
  }

  private info(p: MockPreset): PresetInfo {
    const parts = presetCovers(p);
    return {
      name: p.name,
      ...(p.part ? { part: p.part } : {}),
      parts,
      modified_unix: p.modified_unix,
      bands: parts.includes("eq") ? bandsCountOfChain(p.chain) : 0,
      convolution: parts.includes("convolution") ? convList(p.chain).length : 0,
    };
  }

  private presetPart(part: unknown): PresetPart | undefined {
    if (part === undefined || part === null) return undefined;
    if (!PRESET_PARTS.includes(part as PresetPart)) this.bad(`unknown preset part ${JSON.stringify(part)}`);
    return part as PresetPart;
  }

  private activeName(part: PresetPart | undefined): string | null {
    return part ? (this.config.active_part_presets?.[part] ?? null) : this.config.active_preset;
  }

  private setActiveName(part: PresetPart | undefined, name: string | null) {
    if (!part) {
      this.config.active_preset = name;
      return;
    }
    const map = { ...(this.config.active_part_presets ?? {}) };
    if (name) map[part] = name;
    else delete map[part];
    this.config.active_part_presets = map;
  }

  private presetState(part: PresetPart | undefined): { name: string; modified: boolean } | null {
    const name = this.activeName(part);
    const preset = name ? this.presets.get(presetKey(part, name)) : undefined;
    return preset ? { name: preset.name, modified: !presetMatches(preset, this.config.chain) } : null;
  }

  // ------------------------------------------------------------ calls

  /** Streams as the engine reports them (an older one has no `volume_db`). */
  private visibleStreams(): StreamInfo[] {
    if (!this.legacyStreamVolume) return this.streams;
    return this.streams.map(({ volume_db: _, ...s }) => s);
  }

  /** `set_stream_volume`, as the daemon does it for streams whose app
   *  volume is 100 % (the mock's): capped by the stream maximum. */
  private setStreamVolume(p: SetStreamVolumeParams): StreamVolume {
    const node = Number(p?.node);
    const db = p?.volume_db;
    if (db !== undefined && db !== null && !(typeof db === "number" && Number.isFinite(db) && db >= VOLUME_MIN_DB && db <= VOLUME_MAX_DB)) {
      this.bad(`volume_db must be between ${VOLUME_MIN_DB} and ${VOLUME_MAX_DB} dB (got ${db})`);
    }
    const s = this.streams.find((x) => x.node === node);
    if (!s) this.bad(`no stream with node id ${node} (see \`avdsp ctl streams\`)`);
    if (s.sink === null) {
      this.conflict(
        `stream ${node} (${s.app_name ?? s.binary ?? "unknown app"}) is not playing on an Audivane sink; only streams on Audivane DSP or Audivane Direct can be set here (move it with \`avdsp ctl route\`)`,
      );
    }
    const cap = Math.min(this.config.volume.guard?.stream_max_db ?? 0, 0);
    let volume_db = s.volume_db ?? 0;
    let limited: StreamVolume["limited_by"];
    if (typeof db === "number") {
      volume_db = Math.round(Math.max(VOLUME_MIN_DB, Math.min(db, cap)) * 100) / 100;
      if (db > cap + 0.002) {
        limited = "stream_max";
        this.volume = {
          ...this.volume,
          blocked: this.volume.blocked + 1,
          last_blocked: { source: { kind: "stream", node, app: s.app_name ?? null }, requested_db: db, applied_db: volume_db, reason: "capped", at_ms: Date.now() },
        };
      }
    }
    const muted = typeof p.muted === "boolean" ? p.muted : s.muted;
    const next = { ...s, volume_db, volume: Math.round(10 ** (volume_db / 20) * 1e6) / 1e6, muted };
    if (typeof db === "number" || typeof p.muted === "boolean") {
      this.streams = this.streams.map((x) => (x.node === node ? next : x));
      this.emit({ event: "streams", data: this.visibleStreams() });
      this.emitStatus();
    }
    return { node, volume_db, muted, max_db: cap, ...(limited ? { limited_by: limited } : {}) };
  }

  private calibrationStatus(): CalibrationStatus | null {
    const c = this.config.calibration;
    if (!c) return null;
    return {
      label: c.label,
      path: c.path,
      design_rate_hz: c.design_rate_hz,
      enabled: c.enabled ?? true,
      devices: [
        ...(c.master ? [{ role: "master", device: this.config.device, entries: c.master.matrix?.length ?? 0, gain_db: c.master.gain_db, manifest_gain_db: c.master_manifest_gain_db ?? c.master.gain_db, trim_ms: 0, installed: true }] : []),
        ...c.followers.map((f) => ({ role: "follower", device: f.device, entries: f.item.matrix?.length ?? 0, gain_db: f.item.gain_db, manifest_gain_db: f.manifest_gain_db ?? f.item.gain_db, trim_ms: f.trim_ms, installed: true })),
      ],
      warnings: [],
    };
  }

  private bad(msg: string): never {
    throw new DaemonError(ErrorCode.INVALID_PARAMS, msg);
  }

  private conflict(msg: string): never {
    throw new DaemonError(ErrorCode.BAD_STATE, msg);
  }

  private presetName(name: unknown): string {
    if (typeof name !== "string" || name.trim().length < 1 || name.length > 64) this.bad("preset names are 1 to 64 characters");
    return name.trim();
  }

  private validateBands(bands: readonly Band[]) {
    for (const b of bands) {
      if (!(b.freq_hz > 0 && b.freq_hz < 100000)) this.bad("band frequency out of range");
      if (!(b.q > 0 && b.q <= 100)) this.bad("band Q out of range");
    }
  }

  private validateChain(chain: ChainConfig, allowedRates: readonly number[] = this.config.rate.allowed_rates) {
    if (chain.dc_block) {
      if (
        !Number.isFinite(chain.dc_block.cutoff_hz) ||
        chain.dc_block.cutoff_hz < DC_BLOCK_CUTOFF_MIN_HZ ||
        chain.dc_block.cutoff_hz > DC_BLOCK_CUTOFF_MAX_HZ
      ) {
        this.bad(`dc_block.cutoff_hz must be between ${DC_BLOCK_CUTOFF_MIN_HZ} and ${DC_BLOCK_CUTOFF_MAX_HZ} Hz (got ${chain.dc_block.cutoff_hz})`);
      }
    }
    const xf = chain.crossfeed;
    if (xf && xf.mode === "bs2b_custom") {
      if (!(xf.cutoff_hz >= 300 && xf.cutoff_hz <= 2000)) this.bad(`crossfeed cutoff_hz must be within 300..=2000 Hz (got ${xf.cutoff_hz})`);
      if (!(xf.feed_db >= 1 && xf.feed_db <= 15)) this.bad(`crossfeed feed_db must be within 1..=15 dB (got ${xf.feed_db})`);
    }
    const h = chain.harmonics;
    if (h) {
      if (!Array.isArray(h.layers) || h.layers.length > 8) this.bad("harmonics layers must be a list of at most 8");
      if (h.routing !== undefined && h.routing !== "parallel" && h.routing !== "series") this.bad(`harmonics routing must be "parallel" or "series" (got ${JSON.stringify(h.routing)})`);
      h.layers.forEach((l, n) => {
        if (l.kind === "chebyshev") {
          if (!Array.isArray(l.levels_db) || l.levels_db.length !== 8) this.bad(`harmonics layers[${n}].levels_db must have 8 entries`);
          l.levels_db.forEach((v, i) => {
            if (v === null || v === undefined) return;
            if (!(v >= -120 && v <= -20)) this.bad(`harmonics layers[${n}].levels_db[${i}] (H${i + 2}) must be within -120..=-20 dB (got ${v})`);
          });
          if (l.signs && (l.signs.length !== 8 || l.signs.some((x) => x !== "+" && x !== "-"))) this.bad("harmonics signs must be 8 of `+` or `-`");
          if (!(l.drive_db >= -40 && l.drive_db <= 0)) this.bad(`harmonics layers[${n}].drive_db must be within -40..=0 dB (got ${l.drive_db})`);
          if (!(l.amount >= 0 && l.amount <= 1)) this.bad(`harmonics layers[${n}].amount must be within 0..=1 (got ${l.amount})`);
        } else if (l.kind === "flux") {
          if (!(l.corner_hz >= 1 && l.corner_hz <= 5000)) this.bad(`harmonics layers[${n}].corner_hz must be within 1..=5000 Hz`);
          if (!(l.max_boost_db >= 0 && l.max_boost_db <= 40)) this.bad(`harmonics layers[${n}].max_boost_db must be within 0..=40 dB`);
          if (!(l.target_h3_db >= -120 && l.target_h3_db <= -12)) this.bad(`harmonics layers[${n}].target_h3_db must be within -120..=-12 dBc`);
          if (!(l.calibration_hz >= 5 && l.calibration_hz <= 1000)) this.bad(`harmonics layers[${n}].calibration_hz must be within 5..=1000 Hz`);
        } else if (l.kind === "hysteresis") {
          if (!(l.saturation >= 0 && l.saturation <= 1)) this.bad(`harmonics layers[${n}].saturation must be within 0..=1`);
          if (!(l.width >= 0 && l.width <= 1)) this.bad(`harmonics layers[${n}].width must be within 0..=1`);
          if (!(l.amount >= 0 && l.amount <= 1)) this.bad(`harmonics layers[${n}].amount must be within 0..=1`);
          if (!(l.target_h3_db >= -120 && l.target_h3_db <= -12)) this.bad(`harmonics layers[${n}].target_h3_db must be within -120..=-12 dBc`);
          if (!(l.calibration_hz >= 20 && l.calibration_hz <= 5000)) this.bad(`harmonics layers[${n}].calibration_hz must be within 20..=5000 Hz`);
        } else {
          this.bad(`harmonics layers[${n}]: unknown kind`);
        }
      });
    }
    const pc = chain.pitch;
    if (pc) {
      const ways = [pc.cents !== 0, pc.semitones != null, pc.reference_hz != null || pc.target_hz != null].filter(Boolean).length;
      if (ways > 1) this.bad("pitch: set at most one of cents, semitones, or reference_hz/target_hz");
      if (!(Math.abs(pc.cents) <= 1200)) this.bad(`pitch cents must be within -1200..=1200 (got ${pc.cents})`);
      if (pc.semitones != null && !(Math.abs(pc.semitones) <= 12)) this.bad(`pitch semitones must be within -12..=12 (got ${pc.semitones})`);
      if (pc.reference_hz != null || pc.target_hz != null) {
        if (!(pc.reference_hz != null && pc.target_hz != null && pc.reference_hz > 0 && pc.target_hz > 0)) this.bad("pitch reference_hz and target_hz must both be > 0");
        const c = pitchCents(pc);
        if (!(Math.abs(c) <= 1200)) this.bad(`pitch shift from reference/target must be within -1200..=1200 cents (got ${c.toFixed(1)})`);
      }
    }
    const lim = chain.limiter;
    if (lim) {
      if (!(lim.ceiling_dbfs >= -6 && lim.ceiling_dbfs <= 0)) this.bad(`limiter ceiling_dbfs must be within -6..=0 (got ${lim.ceiling_dbfs})`);
      if (!(lim.lookahead_ms >= 5 && lim.lookahead_ms <= 20)) this.bad(`limiter lookahead_ms must be within 5..=20 (got ${lim.lookahead_ms})`);
      if (!(lim.release_ms >= 10 && lim.release_ms <= 2000)) this.bad(`limiter release_ms must be within 10..=2000 (got ${lim.release_ms})`);
    }
    for (const key of ["tilt", "tilt_high"] as const) {
      const tilt = chain.eq?.[key];
      if (!tilt) continue;
      // Config validation checks every stored tilt, on or off (`validate_eq_tilt`).
      const err = tiltConfigError(tilt);
      if (err) this.bad(`chain.eq.${err.replace(/^tilt/, key)}`);
      // The design at the stream rate only matters for one that runs.
      const why = tilt.enabled ? tiltProblem(tilt, this.graphRate()) : null;
      if (why) this.bad(`${key}: ${why}`);
    }
    if (chain.eq?.tilt && chain.eq.tilt_high) {
      const pair = tiltPairProblem(chain.eq.tilt, chain.eq.tilt_high);
      if (pair) this.bad(`chain.eq: ${pair}`);
    }
    if (chain.eq) {
      this.validateBands(chain.eq.bands);
      if (chain.eq.left) this.validateBands(chain.eq.left.bands);
      if (chain.eq.right) this.validateBands(chain.eq.right.bands);
    }
    this.validateUpsampler(chain.upsampler, allowedRates);
  }

  /**
   * The daemon's `validate_upsampler`, in the parts the app can reach:
   * target ranges, presets and their ratio rules, automatic and band
   * designs, the phase and window rules, and the rational grid limit.
   */
  private validateUpsampler(u: UpsamplerConfig, allowedRates: readonly number[]) {
    const f = "chain.upsampler";
    const t = u.target;
    if (this.legacyRatio && t !== "off" && ("rate" in t || "max_rate_any" in t)) {
      this.bad(`unknown variant \`${"rate" in t ? "rate" : "max_rate_any"}\`, expected one of \`ratio\`, \`max_rate\`, \`off\``);
    }
    if (t !== "off") {
      if ("ratio" in t && !(t.ratio >= 1 && t.ratio <= 64)) this.bad(`${f}.target ratio must be between 1 and 64 (got ${t.ratio})`);
      if ("rate" in t && !([...ALLOWED_SOURCE_RATES, ...RATES_HIGH] as number[]).includes(t.rate)) {
        this.bad(`${f}.target rate must be one of [${ALLOWED_SOURCE_RATES.join(", ")}] or [${RATES_HIGH.join(", ")}] Hz (got ${t.rate})`);
      }
      const max = "max_rate" in t ? t.max_rate : "max_rate_any" in t ? t.max_rate_any : null;
      if (max !== null && !(max >= 8000 && max <= MAX_OUTPUT_RATE)) this.bad(`${f}.target max rate must be between 8000 and ${MAX_OUTPUT_RATE} Hz (got ${max})`);
      if (max !== null && max < 44100) this.bad(`${f}.target max rate ${max} Hz is below the lowest source rate`);
    }
    // Integer ratios at every engine rate; rational ones at the allowed rates.
    const rational = ALLOWED_SOURCE_RATES.map((rate) => ({ rate, r: resolveRateRatio(t, rate) })).filter(({ r }) => r.up !== r.down);
    const checked = rational.filter(({ rate, r }) => r.down === 1 || allowedRates.includes(rate));
    for (const { rate, r } of checked) {
      if (r.up > MAX_RATIONAL_TERM || r.down > MAX_RATIONAL_TERM) this.bad(`${f}.target: at ${rate} Hz the ratio ${r.up}/${r.down} has a term above ${MAX_RATIONAL_TERM}`);
    }
    const hint = (rate: number) => `(or remove ${rate} Hz from rate.allowed_rates)`;
    const MAX_GRID = 1_048_576;
    const pipelines = rationalPipelines(!!this.config.ab);
    if (u.preset !== null && u.design !== null) {
      this.bad(`${f}: preset and design cannot both be set`);
    }
    if (u.preset === RECOMMENDED_PRESET) {
      return;
    }
    if (u.design) {
      const { errors } = mockCheckDesign(u.design, this.checkContext(u, allowedRates));
      if (errors.length > 0) this.bad(`${f}.${errors[0].path}: ${errors[0].message}`);
      // The legacy model below only knows the standard path's windowed sinc.
      const d = u.design;
      if (d.kind !== "windowed_sinc" || d.numerics.path !== "standard") return;
    }
    if (u.preset) {
      const preset = u.preset as FilterPreset;
      if (!presetEntry(preset)) this.bad(`${f}.preset: unknown filter preset ${u.preset}`);
      for (const { rate, r } of checked) {
        const need = presetRatioNeed(preset, r, MOCK_LIMITS.max_kernel_taps);
        if (need) this.bad(`${f}.preset: at ${rate} Hz, filter preset ${u.preset} does not run at ${ratioStr(r)}: it needs ${need}`);
        if (r.down === 1) continue;
        const { gridTaps, macs } = presetRationalCost(preset, rate, r, pipelines);
        if (gridTaps > MAX_GRID || macs > 800e6) {
          this.bad(`${f}.preset: at ${rate} Hz, ${u.preset} needs ${gridTaps} taps on the ${r.up}x grid: use a shorter preset or a lower output rate ${hint(rate)}`);
        }
      }
      return;
    }
    const lu = fromUpsamplerConfig(u);
    if (!(lu.phase_mix >= 0 && lu.phase_mix <= 1)) this.bad(`${f}.phase_mix must be between 0 and 1 (got ${lu.phase_mix})`);
    const bandEdges = () => {
      const b = lu.band;
      if (!(b.passband_end >= 0.5 && b.passband_end <= 1)) this.bad(`${f}.band: passband end must be between 0.5 and 1 (got ${b.passband_end})`);
      if (b.passband_end_hz != null && !(b.passband_end_hz > 0)) this.bad(`${f}.band: passband end in Hz must be above 0 (got ${b.passband_end_hz})`);
      if (!(b.stopband_start > b.passband_end && b.stopband_start <= 1.5)) this.bad(`${f}.band: stopband start must be above the passband end and at most 1.5 (got ${b.stopband_start})`);
    };
    if (lu.auto_taps) {
      const a = this.autoBudgetDb(lu);
      if (!(a >= 55 && a <= 200)) this.bad(`${f}.auto_taps: error budget must be between 55 and 200 dB (got ${a})`);
      if (lu.window === "tukey" || lu.preserve_samples) this.bad(`${f}.auto_taps: automatic taps needs Kaiser or Gaussian without preserve_samples`);
      bandEdges();
      return;
    }
    if (lu.cutoff_mode === "band") {
      bandEdges();
      const a = lu.band.attenuation_db;
      if (!(a >= 55 && a <= 225)) this.bad(`${f}.band: attenuation must be between 55 and 225 dB (got ${a})`);
      if (lu.phase !== "linear" && a > 190.2) this.bad(`${f}: ${lu.phase} phase supports a band attenuation up to 190.2 dB (got ${a})`);
      // Like the daemon's `check_rational_at` for band designs (config.rs).
      const bp = bandRationalProblem(lu, allowedRates, pipelines);
      if (bp) {
        const { rate, ratio: r, gridTaps } = bp;
        const fix = `widen the band's transition or lower its attenuation, or use a preset ${hint(rate)}`;
        if (gridTaps > MAX_GRID) this.bad(`${f}: at ${rate} Hz (${ratioStr(r)}) the design needs ${gridTaps} taps on the ${r.up}x grid, more than ${MAX_GRID}: ${fix}`);
        const out = (rate * r.up) / r.down;
        const one = out * Math.ceil(gridTaps / r.up);
        this.bad(
          `${f}: at ${rate} Hz, the ${ratioStr(r)} kernel (${Math.ceil(gridTaps / r.up)} taps per output frame at ${out} Hz) needs ${Math.round(one / 1e6)} M multiply-adds per second per channel, ${Math.round((one * pipelines) / 1e6)} M with the ${pipelines} pipelines that can run at once (a swap or crossfade runs two, A/B doubles it), more than the 800 M budget: ${fix}`,
        );
      }
    } else {
      const maxTaps = MOCK_LIMITS.max_kernel_taps;
      if (!(lu.taps >= 1 && lu.taps <= maxTaps)) this.bad(`${f}.taps must be between 1 and ${maxTaps} (got ${lu.taps})`);
      if (!Number.isFinite(lu.kaiser_beta) || lu.kaiser_beta < 0 || lu.kaiser_beta > 40) this.bad(`${f}.kaiser_beta must be between 0 and 40 (got ${lu.kaiser_beta})`);
      if (!(lu.cutoff > 0 && lu.cutoff <= 1)) this.bad(`${f}.cutoff must be in (0, 1] (got ${lu.cutoff})`);
      if (lu.phase === "minimum") {
        const why = minimumPhaseBlockReason(lu.kaiser_beta);
        if (why) this.bad(why);
      }
      if (lu.phase === "intermediate" && (lu.kaiser_beta < 2 || lu.kaiser_beta > 20)) this.bad(`${f}: intermediate phase needs a Kaiser beta between 2 and 20 (got ${lu.kaiser_beta})`);
      if (lu.cutoff_mode === "nyquist_stop") {
        const why = nyquistStopBlockReason(lu, allowedRates);
        if (why) this.bad(why);
      }
    }
    if (lu.window === "tukey") {
      if (lu.phase !== "linear" || lu.cutoff_mode !== "fraction") this.bad(`${f}: the Tukey window needs linear phase with cutoff_mode fraction (its stopband has no calibrated formula)`);
      if (!(lu.taper > 0 && lu.taper <= 1)) this.bad(`${f}.taper must be in (0, 1] (got ${lu.taper})`);
    }
    if (lu.preserve_samples) {
      if (lu.phase !== "linear" || lu.cutoff_mode !== "fraction" || lu.cutoff !== 1) {
        this.bad(`${f}.preserve_samples needs linear phase with cutoff_mode fraction and cutoff 1.0 (a filter that passes the input samples through is -6 dB at the source Nyquist)`);
      }
      const dec = checked.find(({ r }) => r.up < r.down);
      if (dec) this.bad(`${f}.preserve_samples needs an output rate at or above the input rate (at ${dec.rate} Hz the target resamples by ${ratioStr(dec.r)})`);
    }
    // Rational ratios: the manual design lives on the intermediate grid.
    const problem = manualRationalProblem(lu, allowedRates, pipelines);
    if (problem) {
      const { rate, ratio: r, gridTaps, maxPerInput: n } = problem;
      const fix = n === 0 ? `use a preset ${hint(rate)}` : `use taps_unit "input" with at most ${n} taps per input sample, or a preset ${hint(rate)}`;
      if (gridTaps > MAX_GRID) this.bad(`${f}: at ${rate} Hz (${ratioStr(r)}) the design needs ${gridTaps} taps on the ${r.up}x grid, more than ${MAX_GRID}: ${fix}`);
      // Like the daemon's `check_rational_budget`: within the size, over the cost.
      const out = (rate * r.up) / r.down;
      const one = out * Math.ceil(gridTaps / r.up);
      this.bad(
        `${f}: at ${rate} Hz, the ${ratioStr(r)} kernel (${Math.ceil(gridTaps / r.up)} taps per output frame at ${out} Hz) needs ${Math.round(one / 1e6)} M multiply-adds per second per channel, ${Math.round((one * pipelines) / 1e6)} M with the ${pipelines} pipelines that can run at once (a swap or crossfade runs two, A/B doubles it), more than the 800 M budget: ${fix}`,
      );
    }
  }

  /** `VolumeConfig` validation (control-plane.md, "Volume, balance, and mute"). */
  private validateVolume(v: EngineConfig["volume"]) {
    const inRange = (x: number | null, lo: number, hi: number) => x === null || (Number.isFinite(x) && x >= lo && x <= hi);
    if (!inRange(v.hardware_db, -120, 0)) this.bad(`volume.hardware_db must be within -120..=0 dB (got ${v.hardware_db})`);
    if (!inRange(v.max_volume_db, -120, 0)) this.bad(`volume.max_volume_db must be within -120..=0 dB (got ${v.max_volume_db})`);
    if (!inRange(v.guard.stream_max_db, -120, 0)) this.bad(`volume.guard.stream_max_db must be within -120..=0 dB (got ${v.guard.stream_max_db})`);
    if (!inRange(v.guard.max_rise_db_per_s, 0, 1000)) this.bad(`volume.guard.max_rise_db_per_s must be within 0..=1000 (got ${v.guard.max_rise_db_per_s})`);
    if (v.mode === "hybrid" && v.hardware_db === null) this.bad("volume.mode hybrid requires volume.hardware_db");
  }

  /** What the engine reports after a settings change: mode, mixer use, ceiling. */
  private applyVolumeSettings(v: EngineConfig["volume"]) {
    this.levelChange(() => {
      const wantsHw = v.mode === "hardware" || v.mode === "hybrid";
      const hardware = this.hasMixer && v.mode !== "software";
      const ceiling = v.max_volume_db;
      const level = ceiling !== null ? Math.min(this.volume.volume_db, ceiling) : this.volume.volume_db;
      this.volume = {
        ...this.volume,
        mode: v.mode,
        hardware,
        hardware_unavailable: wantsHw && !this.hasMixer,
        max_db: ceiling ?? 0,
        ceiling_db: ceiling,
        sink_volume: v.sink_volume,
        guard: v.guard.mode,
        volume_db: level,
      };
    });
    this.config.volume = { ...this.config.volume, volume_db: this.volume.volume_db };
  }

  /** Chain B is re-materialised from `b` after every edit, like the daemon. */
  private normalizeB() {
    const ab = this.config.ab;
    if (!ab) return;
    try {
      this.config.ab = normalizeAbWithChainA(ab, this.config.chain).ab;
    } catch {
      this.config.ab = null;
      this.abSide = "a";
    }
  }

  handle(method: string, params: unknown): unknown {
    if (!this.running) throw new DaemonError(ErrorCode.DISCONNECTED, "the engine is not running");
    const p = params as Record<string, unknown>;
    const cfg = this.config;
    const mutate = (fn: () => void) => {
      fn();
      this.normalizeB();
      // A measurement is of one pair: a change to either side ends it (a level or label change does not).
      if (this.abMeasure && this.abMeasure.pair !== this.abPair()) this.abMeasureEnd("cancelled: the compared chains changed");
      this.emitConfig();
      return null;
    };
    switch (method) {
      case "status":
        return this.status();
      case "get_config":
        return clone(cfg);
      case "set_config":
        return mutate(() => {
          const next = clone(params as EngineConfig);
          if (next.chain.dc_block !== undefined) next.chain.dc_block = normalizeDcBlock(next.chain.dc_block);
          if (next.chain.eq) next.chain.eq = normalizeEq(next.chain.eq);
          if (next.chain.eq?.tilt) next.chain.eq.tilt = normalizeTilt(next.chain.eq.tilt);
          if (next.chain.eq?.tilt_high) next.chain.eq.tilt_high = normalizeTilt(next.chain.eq.tilt_high);
          if (next.ab) {
            next.ab = normalizeAbWithChainA(next.ab, next.chain).ab;
          }
          // Like the daemon's `normalize_rates` + `validate`.
          if (!next.rate.allowed_rates.length) throw new DaemonError(ErrorCode.INVALID_PARAMS, "rate.allowed_rates must not be empty");
          next.rate = normalizeRatePolicy(next.rate);
          this.validateChain(next.chain, next.rate.allowed_rates);
          if (next.ab) this.validateChain(chainB(next.ab, next.chain), next.rate.allowed_rates);
          this.admitCpu(next);
          next.outputs ??= { followers: [] };
          this.validateOutputs(next);
          // Like the daemon: the level belongs to `set_volume`, so the
          // incoming volume, balance and mute are ignored.
          if (next.volume) {
            next.volume = { ...next.volume, volume_db: this.volume.volume_db, balance_db: this.volume.balance_db, muted: this.volume.muted };
            this.validateVolume(next.volume);
          }
          this.config = next;
          this.syncFollowers();
          if (next.volume) this.applyVolumeSettings(next.volume);
        });
      case "set_eq":
        return mutate(() => {
          const rawEq = params === null ? null : clone(params as NonNullable<ChainConfig["eq"]>);
          const eq = rawEq ? normalizeEq(rawEq) : null;
          if (eq?.tilt) eq.tilt = normalizeTilt(eq.tilt);
          if (eq?.tilt_high) eq.tilt_high = normalizeTilt(eq.tilt_high);
          const chain = { ...cfg.chain, eq };
          this.validateChain(chain);
          cfg.chain = chain;
        });
      case "set_convolution":
        return mutate(() => {
          const v = params as ChainConfig["convolution"];
          const list = (v === null ? [] : Array.isArray(v) ? clone(v) : [clone(v)]).filter((c) => !c.calibration);
          // As the daemon: the room calibration's item stays in place.
          const kept = convList(cfg.chain).filter((c) => c.calibration);
          cfg.chain.convolution = [...list, ...(cfg.calibration ? kept : [])];
        });
      case "bundle_load":
        return mutate(() => {
          const path = String((params as { path?: unknown } | null)?.path ?? "");
          if (!path.endsWith(".json")) this.bad(`bundle_load: ${path}: not a bundle manifest`);
          const item = {
            name: "Room calibration: desk",
            enabled: true,
            ir_path: "",
            gain_db: 0,
            calibration: true,
            matrix: [
              { in: 0, out: 0, ir_path: IR_DIR + "mains_LL.wav", gain_db: 0 },
              { in: 1, out: 1, ir_path: IR_DIR + "mains_RR.wav", gain_db: 0 },
            ],
          };
          cfg.chain.convolution = [...convList(cfg.chain).filter((c) => !c.calibration), item];
          cfg.calibration = { path, label: "desk", design_rate_hz: 48000, master: item, followers: [] };
        });
      case "bundle_unload":
        return mutate(() => {
          if (!cfg.calibration) this.bad("no room calibration is loaded");
          cfg.chain.convolution = convList(cfg.chain).filter((c) => !c.calibration);
          delete cfg.calibration;
        });
      case "bundle_gain":
        mutate(() => {
          const p = (params ?? {}) as { device?: string; gain_db?: number | null };
          const c = cfg.calibration;
          if (!c) this.conflict("no room calibration is loaded");
          const g = p.gain_db ?? null;
          if (g !== null && !(g >= -60 && g <= 24)) this.bad(`bundle_gain: gain_db must be between -60 and 24 dB (got ${g})`);
          const main = !p.device || p.device === "master" || p.device === cfg.device;
          const f = main ? null : c.followers.find((x) => x.device === p.device);
          if (!main && !f) this.bad(`bundle_gain: ${p.device} is not a part of the loaded calibration`);
          const item = main ? c.master : f!.item;
          if (!item) this.bad("bundle_gain: the loaded calibration has no master part");
          const base = (main ? c.master_manifest_gain_db : f!.manifest_gain_db) ?? item.gain_db;
          item.gain_db = g ?? base;
          const keep = item.gain_db !== base ? base : undefined;
          if (main) {
            if (keep === undefined) delete c.master_manifest_gain_db;
            else c.master_manifest_gain_db = keep;
            cfg.chain.convolution = convList(cfg.chain).map((x) => (x.calibration ? { ...x, gain_db: item.gain_db } : x));
          } else if (keep === undefined) delete f!.manifest_gain_db;
          else f!.manifest_gain_db = keep;
        });
        return this.calibrationStatus();
      case "bundle_enable":
        mutate(() => {
          const c = cfg.calibration;
          if (!c) this.bad("bundle_enable: nothing changed: no room calibration is loaded");
          const on = Boolean((params as { on?: unknown } | null)?.on);
          c.enabled = on;
          // As the daemon: off, the placed copies are disabled; on, they play.
          cfg.chain.convolution = convList(cfg.chain).map((x) => (x.calibration ? { ...x, enabled: on } : x));
        });
        return this.calibrationStatus();
      case "bundle_show":
        return this.calibrationStatus();
      case "set_convolution_safety":
        return mutate(() => {
          cfg.convolution_safety = clone(params as NonNullable<EngineConfig["convolution_safety"]>);
        });
      case "set_upsampler":
        return mutate(() => {
          const next = { ...cfg.chain, upsampler: clone(params as ChainConfig["upsampler"]) };
          this.validateChain(next);
          this.admitCpu({ ...cfg, chain: next, ab: cfg.ab ? normalizeAbWithChainA(cfg.ab, next).ab : null });
          cfg.chain = next;
        });
      case "set_dc_block":
        return mutate(() => {
          const next = normalizeDcBlock(params);
          const chain = { ...cfg.chain, dc_block: next };
          this.validateChain(chain);
          cfg.chain = chain;
        });
      case "set_crossfeed":
        return mutate(() => {
          const chain = { ...cfg.chain, crossfeed: params === null ? null : this.crossfeedAsStored(clone(params as NonNullable<ChainConfig["crossfeed"]>)) };
          this.validateChain(chain);
          cfg.chain = chain;
        });
      case "set_varispeed": {
        const given = (params ?? {}) as SetVarispeedParams;
        for (const k of ["enabled", "speed", "ramp_ms", "keep_pitch"] as const) {
          if (k in given && given[k] === null) this.bad(`invalid type: null, expected ${k === "enabled" || k === "keep_pitch" ? "a boolean" : "f64"}`);
        }
        const next = { ...cfg.varispeed, ...given };
        if (next.enabled && next.keep_pitch) this.checkKeepPitch(this.graphRate());
        if (!(Number.isFinite(next.speed) && next.speed >= 0.5 && next.speed <= 2)) this.bad(`varispeed.speed must be between 0.5 and 2 (got ${next.speed})`);
        if (!(Number.isFinite(next.ramp_ms) && next.ramp_ms >= 0 && next.ramp_ms <= 10000)) this.bad(`varispeed.ramp_ms must be between 0 and 10000 ms (got ${next.ramp_ms})`);
        this.validateOutputs({ ...cfg, varispeed: next });
        cfg.varispeed = next;
        this.emitConfig();
        return this.currentStatus().varispeed;
      }
      case "set_pitch":
        return mutate(() => {
          const chain = { ...cfg.chain, pitch: { ...{ enabled: true, cents: 0, quality: "high" as const }, ...clone(params as ChainConfig["pitch"]) } };
          this.validateChain(chain);
          cfg.chain = chain;
        });
      case "set_harmonics":
        return mutate(() => {
          const chain = { ...cfg.chain, harmonics: params === null ? null : clone(params as NonNullable<ChainConfig["harmonics"]>) };
          this.validateChain(chain);
          cfg.chain = chain;
        });
      case "harmonics_presets":
        return clone(MOCK_HARMONICS_PRESETS);
      case "harmonics_systems":
        return clone(MOCK_HARMONICS_SYSTEMS);
      case "harmonics_readout": {
        const q = (params ?? {}) as { config?: NonNullable<ChainConfig["harmonics"]>; sweep?: boolean; sweep_freq_hz?: number; sweep_level_dbfs?: number };
        if (q.config) this.validateChain({ ...cfg.chain, harmonics: q.config });
        const h = q.config ?? cfg.chain.harmonics ?? { ...defaultHarmonics(), enabled: false };
        const rate = this.status().output_rate || 96000;
        return mockHarmonicsReadout(h, rate, !!q.sweep, q.sweep_freq_hz, q.sweep_level_dbfs);
      }
      case "harmonics_spectrum": {
        const q = (params ?? {}) as { config?: NonNullable<ChainConfig["harmonics"]>; freq_hz: number; level_dbfs: number; traces?: HarmonicsSpectrumTrace[] };
        if (q.config) this.validateChain({ ...cfg.chain, harmonics: q.config });
        const h = q.config ?? cfg.chain.harmonics ?? { ...defaultHarmonics(), enabled: false };
        const rate = this.status().output_rate || 96000;
        const out = cfg.chain.output;
        return mockHarmonicsSpectrum(h, rate, q.freq_hz, q.level_dbfs, q.traces ?? [], { bits: out.bits, dither: out.dither, headroom_db: out.headroom_db });
      }
      case "harmonics_live": {
        const st = this.status();
        const playing = st.state === "playing" && this.config.dsp_enabled;
        this.liveSeq++;
        // A blind test hides it (it would give the side away).
        if (this.abBlind) return { ...mockHarmonicsLive(null, st.output_rate || 96000, false, 0, 0), state: "hidden" };
        // The side playing.
        const h = cfg.ab && this.abSide === "b" ? cfg.ab.chain_b?.harmonics : cfg.chain.harmonics;
        return mockHarmonicsLive(h, st.output_rate || 96000, playing, Date.now() - this.liveStart, this.liveSeq);
      }
      case "set_limiter":
        return mutate(() => {
          // Shared with chain B (normalizeB), like the upsampler and output.
          const chain = { ...cfg.chain, limiter: clone(params as ChainConfig["limiter"]) };
          this.validateChain(chain);
          if (chain.limiter?.mode !== cfg.chain.limiter?.mode) this.limHeadroom = 0;
          cfg.chain = chain;
        });
      case "meters":
        return this.meters();
      case "set_output":
        return mutate(() => {
          const chain = { ...cfg.chain, output: clone(params as ChainConfig["output"]) };
          if (![16, 20, 24, 32].includes(chain.output.bits)) this.bad(`output.bits must be 16, 20, 24 or 32 (got ${chain.output.bits})`);
          const h = chain.output.headroom_db;
          if (!(h >= 0 && h <= 12)) this.bad(`chain.output.headroom_db must be between 0 and 12 dB (got ${h})`);
          this.validateChain(chain);
          cfg.chain = chain;
        });
      case "set_formats":
        return mutate(() => {
          const formats = clone(params as EngineConfig["formats"]);
          if (!Array.isArray(formats) || formats.length === 0) this.bad("formats must not be empty");
          cfg.formats = formats;
        });
      case "set_rate":
        return mutate(() => {
          const rate = clone(params as EngineConfig["rate"]);
          if (!rate.allowed_rates.length) this.bad("rate.allowed_rates must not be empty");
          // Like serde: a missing field is the default, an unknown one an error.
          rate.bypass_resampler ??= "engine";
          if (rate.bypass_resampler !== "engine" && rate.bypass_resampler !== "pipewire") {
            this.bad(`unknown variant \`${String(rate.bypass_resampler)}\`, expected \`engine\` or \`pipewire\``);
          }
            const next = normalizeRatePolicy(rate);
          // Like the daemon's `edit`: the whole config is validated, and the
          // upsampler's fractional limits depend on the allowed rates.
          this.validateChain(cfg.chain, next.allowed_rates);
          if (cfg.ab) this.validateChain(chainB(cfg.ab, cfg.chain), next.allowed_rates);
          cfg.rate = next;
        });
      case "set_device":
        return mutate(() => {
          const device = String((params as { device?: unknown })?.device ?? "");
          if (device !== "" && !device.startsWith("hw:")) this.bad(`device must be an ALSA hw: device (got "${device}")`);
          const pair = (params as { output_channels?: unknown })?.output_channels;
          const next = device || DEVICES[0].id;
          if (pair !== undefined) {
            if (!Array.isArray(pair) || pair.length !== 2 || pair[0] === pair[1]) this.bad("output_channels: left and right need two different channels");
            const max = capsFor(next)?.channels_max ?? 0;
            const [l, r] = pair as [number, number];
            if (max && Math.max(l, r) >= max) this.bad(`output_channels [${l}, ${r}]: device '${next}' has ${max} channels (0 to ${max - 1})`);
          }
          this.validateOutputs({ ...cfg, device: next });
          if (pair !== undefined || next !== cfg.device) {
            const p = pair as [number, number] | undefined;
            if (!p || (p[0] === 0 && p[1] === 1)) delete cfg.output_channels;
            else cfg.output_channels = p;
          }
          cfg.device = next;
        });
      case "assign_devices":
        return mutate(() => {
          const list = ((params ?? {}) as { assignments?: { output: string; device: string }[] }).assignments ?? [];
          const next = structuredClone(cfg);
          for (const a of list) {
            if (!DEVICES.some((d) => d.id === a.device)) this.bad(`${a.device} is not a connected USB card`);
            if (a.output === "master") next.device = a.device;
            else {
              const f = next.outputs?.followers.find((x) => x.device === a.output);
              if (!f) this.bad(`${a.output} is not a follower`);
              else f.device = a.device;
            }
          }
          this.validateOutputs(next);
          cfg.device = next.device;
          if (next.outputs) cfg.outputs = next.outputs;
        });
      case "set_follower": {
        const given = (params ?? {}) as { device?: unknown; remove?: unknown; enabled?: unknown; trim_ms?: unknown; volume_db?: unknown; follow_volume?: unknown; output_channels?: unknown; chain_patch?: unknown };
        for (const k of ["remove", "enabled", "trim_ms", "volume_db", "follow_volume"] as const) {
          if (k in given && given[k] === null) this.bad(`invalid type: null, expected ${k === "remove" || k === "enabled" || k === "follow_volume" ? "a boolean" : "f64"}`);
        }
        const device = String(given.device ?? "");
        const followers = clone(cfg.outputs.followers);
        const at = followers.findIndex((f) => f.device === device);
        if (given.remove === true) {
          if (at < 0) this.bad(`${device} is not a follower`);
          followers.splice(at, 1);
        } else {
          const i = at >= 0 ? at : followers.push({ device, enabled: true, trim_ms: 0, volume_db: 0, follow_volume: false }) - 1;
          const f = followers[i];
          if (typeof given.enabled === "boolean") f.enabled = given.enabled;
          if (typeof given.trim_ms === "number") f.trim_ms = given.trim_ms;
          if (typeof given.volume_db === "number") f.volume_db = given.volume_db;
          if (Array.isArray(given.output_channels)) {
            const [l, r] = given.output_channels as [number, number];
            if (l === r) this.bad("output_channels: left and right need two different channels");
            if (l === 0 && r === 1) delete f.output_channels;
            else f.output_channels = [l, r];
          }
          if (typeof given.follow_volume === "boolean") f.follow_volume = given.follow_volume;
          if ("chain_patch" in given) {
            const p = given.chain_patch;
            if (p === null || (typeof p === "object" && Object.keys(p as object).length === 0)) delete f.chain_patch;
            else if (typeof p === "object" && !Array.isArray(p)) f.chain_patch = p as Record<string, unknown>;
            else this.bad("outputs.followers[0].chain_patch must be a JSON object (a merge patch on chain A)");
          }
        }
        this.validateOutputs({ ...cfg, outputs: { followers } });
        cfg.outputs = { followers };
        this.syncFollowers();
        this.emitConfig();
        return this.currentStatus().outputs;
      }
      case "set_dsp_enabled":
        return mutate(() => {
          cfg.dsp_enabled = !!p.enabled;
        });
      case "set_ab": {
        const notes: string[] = [];
        mutate(() => {
          if (params === undefined || params === null) {
            cfg.ab = null;
            this.abSide = "a";
            return;
          }
          this.abOvers = { a: 0, b: 0 };
          this.abBlind = false;
          const { ab: next, notes: abNotes } = normalizeAbWithChainA(params, cfg.chain);
          this.validateChain(chainB(next, cfg.chain));
          this.admitCpu({ ...cfg, ab: next });
          notes.push(...abNotes);
          cfg.ab = next;
        });
        return { ...(this.status().ab ?? { enabled: false, current: "a", blind: false, offset_db: 0, error: null }), ...(notes.length ? { notes } : {}) };
      }
      case "ab_measure": {
        if (!cfg.ab) throw new DaemonError(ErrorCode.INVALID_PARAMS, "A/B is not configured");
        if (p.cancel && p.accept) throw new DaemonError(ErrorCode.INVALID_PARAMS, "ab_measure: cancel and accept exclude each other");
        if (p.cancel) {
          if (this.abMeasure) this.abMeasureEnd(null);
          this.emitStatus();
          return this.status().ab;
        }
        if (p.accept) {
          const m = this.abMeasure;
          if (!m) throw new DaemonError(ErrorCode.BAD_STATE, "no A/B level measurement is running");
          if (m.offset === null) throw new DaemonError(ErrorCode.BAD_STATE, "nothing measured yet: play some music first");
          this.abMeasureLock(m.offset, m.seconds);
          return this.status().ab;
        }
        if (this.abBlind) throw new DaemonError(ErrorCode.BAD_STATE, "a blind test is running: the level is locked");
        const max = p.max_seconds === undefined ? 60 : Number(p.max_seconds);
        if (!Number.isFinite(max) || max < 10 || max > 300) throw new DaemonError(ErrorCode.INVALID_PARAMS, "ab_measure: max_seconds must be 10 to 300");
        this.abMeasureStart(max);
        this.emitStatus();
        return this.status().ab;
      }
      case "ab_blind": {
        if (p.enabled && !cfg.ab) throw new DaemonError(ErrorCode.INVALID_PARAMS, "A/B is not configured");
        if (p.enabled && this.abMeasure) this.abMeasureEnd(null);
        this.abBlind = !!p.enabled && !!cfg.ab;
        this.emitStatus();
        return this.status().ab ?? { enabled: false, current: "a", blind: false, offset_db: 0, error: null };
      }
      case "ab_select": {
        if (!cfg.ab) throw new DaemonError(ErrorCode.INVALID_PARAMS, "A/B is not configured");
        this.abSide = p.side === "b" ? "b" : "a";
        this.abOvers = { a: 0, b: 0 };
        this.emitStatus();
        return this.status().ab;
      }
      case "list_streams":
        return clone(this.visibleStreams());
      case "set_stream_volume":
        if (this.legacyStreamVolume) throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
        return this.setStreamVolume(params as SetStreamVolumeParams);
      case "route": {
        const target = p.target as { node: number } | { app: string };
        const sink = p.sink as SinkChoice;
        this.streams = this.streams.map((s) => {
          const hit = "node" in target ? s.node === target.node : s.app_name?.toLowerCase() === target.app.toLowerCase();
          return hit ? { ...s, sink: sink === "default" ? (cfg.routing.mode === "system" ? "dsp" : null) : sink } : s;
        });
        this.emit({ event: "streams", data: this.visibleStreams() });
        this.emitStatus();
        return null;
      }
      case "set_routing_mode":
        return mutate(() => {
          cfg.routing = clone(params as EngineConfig["routing"]);
          this.streams = this.streams.map((s) => {
            if (cfg.routing.mode === "system") return { ...s, sink: s.sink === "direct" ? "direct" : "dsp" };
            const rule = cfg.routing.rules.find((r) => r.app.toLowerCase() === (s.app_name ?? "").toLowerCase());
            return { ...s, sink: rule ? (rule.sink === "default" ? null : rule.sink) : s.sink === "direct" ? "direct" : null };
          });
          this.emit({ event: "streams", data: this.visibleStreams() });
        });
      case "now_playing":
        return clone(this.nowPlaying);
      case "self_test": {
        if (this.released) throw new DaemonError(ErrorCode.BAD_STATE, "the device is released");
        const r: SelfTestResult = {
          passed: true,
          frames: 96000,
          rate: 44100,
          format: cfg.formats[0] ?? "s32",
          expected_hash: "9f2c41ad07e3b5d1",
          actual_hash: "9f2c41ad07e3b5d1",
          detail: "96000 frames matched",
        };
        return new Promise((res) => setTimeout(() => res(r), 1200));
      }
      case "release_device":
        this.released = true;
        this.emitStatus();
        return null;
      case "acquire_device":
        this.released = false;
        // The followers reopen and lock again.
        for (const d of this.followerSince.keys()) this.followerSince.set(d, this.followerClock());
        this.emitStatus();
        return null;
      case "subscribe":
        return null;
      case "shutdown":
        this.running = false;
        for (const l of this.connListeners) l(false);
        return null;
      case "set_volume_guarded":
      case "set_volume": {
        const given = params as SetVolumeParams | SetVolumeGuardedParams;
        // A guarded write changes only the fields it carries.
        const v = {
          volume_db: given.volume_db ?? this.volume.volume_db,
          balance_db: given.balance_db ?? this.volume.balance_db,
          muted: given.muted ?? this.volume.muted,
        };
        if (method === "set_volume_guarded") {
          // A daemon that predates guarded writes does not know the method.
          if (this.legacyVolume) throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
          const g = given as SetVolumeGuardedParams;
          if (g.if_instance !== this.instance) {
            throw new DaemonError(ErrorCode.STALE_INSTANCE, `this volume change was made for engine instance "${g.if_instance}", which has since restarted`);
          }
          if (g.if_revision !== this.revision) {
            throw new DaemonError(ErrorCode.STALE_REVISION, `the volume changed since revision ${g.if_revision} (now ${this.revision})`);
          }
        }
        if (!(v.volume_db >= VOLUME_MIN_DB && v.volume_db <= VOLUME_MAX_DB)) this.bad(`volume_db must be within ${VOLUME_MIN_DB}..=${VOLUME_MAX_DB} dB`);
        if (!(Math.abs(v.balance_db) <= BALANCE_MAX_DB)) this.bad(`balance_db must be within ±${BALANCE_MAX_DB} dB`);
        this.levelChange(() => {
          this.volume = { ...this.volume, volume_db: Math.max(this.volume.min_db, Math.min(this.volume.max_db, v.volume_db)), balance_db: v.balance_db, muted: v.muted };
        });
        cfg.volume = { ...cfg.volume, volume_db: this.volume.volume_db, balance_db: v.balance_db, muted: v.muted };
        this.emitConfig();
        return this.volumeStatus();
      }
      case "list_presets":
        return this.listPresets();
      case "get_preset": {
        const part = this.presetPart(p.part);
        const pr = this.presets.get(presetKey(part, this.presetName(p.name)));
        if (!pr) this.bad(`no preset named "${String(p.name)}"`);
        const { modified_unix: _, ...preset } = pr;
        return clone(preset);
      }
      case "save_preset": {
        const part = this.presetPart(p.part);
        const name = this.presetName(p.name);
        const key = presetKey(part, name);
        if (this.presets.has(key) && !p.overwrite) this.conflict(`a preset named "${name}" already exists; pass overwrite to replace it`);
        let parts: PresetPart[] | undefined;
        if (!part && Array.isArray(p.parts)) {
          parts = PRESET_PARTS.filter((x) => (p.parts as unknown[]).includes(x));
          if (!parts.length) this.bad("a preset must hold at least one part");
          if (parts.length === PRESET_PARTS.length) parts = undefined;
        }
        // Chain A is stored as it is (the EQ keeps its own name), like the daemon.
        const pr: MockPreset = { version: 1, name, ...(part ? { part } : {}), ...(parts ? { parts } : {}), chain: clone(cfg.chain), modified_unix: Math.floor(Date.now() / 1000) };
        this.presets.set(key, pr);
        this.setActiveName(part, name);
        this.emitPresets();
        this.emitConfig();
        return this.info(pr);
      }
      case "load_preset": {
        const part = this.presetPart(p.part);
        const pr = this.presets.get(presetKey(part, this.presetName(p.name)));
        if (!pr) this.bad(`no preset named "${String(p.name)}"`);
        return mutate(() => {
          cfg.chain = applyPreset(cfg.chain, pr);
          this.setActiveName(part, pr.name);
          // A global preset replaces its parts: their pages' names go, like the daemon.
          if (!part) for (const covered of presetCovers(pr)) this.setActiveName(covered, null);
        });
      }
      case "blank_preset": {
        const part = this.presetPart(p.part);
        if (!part) this.bad("blank_preset needs a part");
        return mutate(() => {
          cfg.chain = applyPreset(cfg.chain, { version: 1, name: "", part, chain: defaultChain(emptyEq()) });
          this.setActiveName(part, null);
        });
      }
      case "delete_preset": {
        const part = this.presetPart(p.part);
        const name = this.presetName(p.name);
        if (!this.presets.delete(presetKey(part, name))) this.bad(`no preset named "${name}"`);
        if (this.activeName(part) === name) this.setActiveName(part, null);
        this.emitPresets();
        this.emitConfig();
        return null;
      }
      case "rename_preset": {
        const part = this.presetPart(p.part);
        const from = this.presetName(p.from);
        const to = this.presetName(p.to);
        const pr = this.presets.get(presetKey(part, from));
        if (!pr) this.bad(`no preset named "${from}"`);
        if (from !== to && this.presets.has(presetKey(part, to))) this.conflict(`a preset named "${to}" already exists`);
        this.presets.delete(presetKey(part, from));
        const next = { ...pr, name: to, modified_unix: Math.floor(Date.now() / 1000) };
        this.presets.set(presetKey(part, to), next);
        if (this.activeName(part) === from) this.setActiveName(part, to);
        this.emitPresets();
        this.emitConfig();
        return this.info(next);
      }
      case "import_apo": {
        const name = this.presetName(p.name);
        let parsed;
        try {
          parsed = parseApo(String(p.text), name);
        } catch (e) {
          this.bad(e instanceof Error ? e.message : "could not read the file");
        }
        if (parsed.profile.bands.length === 0) this.bad("cannot parse the EQ text: no filters found");
        if (!parsed.profile.name) parsed.profile.name = name;
        // An EQ preset: loading it touches nothing else. Not loaded; an existing name is replaced.
        const pr: MockPreset = { version: 1, name, part: "eq", chain: defaultChain(parsed.profile), modified_unix: Math.floor(Date.now() / 1000) };
        this.presets.set(presetKey("eq", name), pr);
        this.emitPresets();
        return this.info(pr);
      }
      case "list_devices":
        return this.listDevices();
      case "resolve_design": {
        const name = (p ?? {}).preset;
        const design = typeof name === "string" ? catalogueDesign(name) : null;
        if (!design) this.bad(`unknown filter preset ${String(name)}`);
        return design;
      }
      case "check_design": {
        const req = (p ?? {}) as { design?: FilterDesign; target?: RateTarget; source_rate?: number };
        if (!req.design) this.bad("check_design needs a design");
        const a = this.config.chain.upsampler;
        const target = req.target ?? a.target;
        if (target !== "off" && "ratio" in target && !(target.ratio >= 1 && target.ratio <= 64)) {
          return { errors: [{ path: "target", message: `ratio must be between 1 and 64 (got ${target.ratio})` }], plans: [], notes: [] };
        }
        const rates = req.source_rate !== undefined ? [req.source_rate] : this.config.rate.allowed_rates;
        return mockCheckDesign(req.design, this.checkContext({ ...a, target }, rates));
      }
      case "conv_response": {
        const q = (params ?? {}) as { side?: "a" | "b"; device?: string };
        const side = q.side ?? "a";
        const device = q.device && q.device !== cfg.device ? q.device : null;
        if (side === "b" && device) this.bad("A/B side B is the master's; a follower has no side B");
        let conv = convList(cfg.chain);
        if (side === "b") {
          if (!cfg.ab) this.bad("A/B is not configured");
          conv = convList(cfg.ab!.chain_b ?? cfg.chain);
        } else if (device) {
          const f = (cfg.outputs?.followers ?? []).find((x) => x.device === device);
          if (!f) this.bad(`no follower "${device}"`);
          const own = (f!.chain_patch as { convolution?: ConvolutionConfig[] } | undefined)?.convolution;
          conv = own ?? conv.filter((c) => !c.calibration);
        }
        const channels = (path: string) => IR_FACTS[path.split("/").pop() ?? ""]?.channels ?? 2;
        return mockConvResponse(conv, this.graphRate(), side, device, channels);
      }
      case "upsampler_response": {
        const req = (params ?? {}) as { config?: UpsamplerConfig | null; source_rate?: number | null; window?: ResponseWindowRequest | null };
        const upCfg = req.config ?? this.chainOn().upsampler;
        const srcRate = req.source_rate ?? this.graphRate();
        const res = this.buildUpsamplerResponse(upCfg, srcRate);
        if (this.legacyRatio) delete (res as Partial<UpsamplerResponse>).rate_ratio;
        const w = req.window;
        if (w) {
          // Like the daemon: up to the grid's Nyquist (source x up).
          const nyqOut = (res.source_rate * (res.rate_ratio?.up ?? res.ratio)) / 2;
          if (!Number.isFinite(w.f_lo_hz) || !Number.isFinite(w.f_hi_hz)) this.bad("window frequencies must be finite");
          if (w.f_lo_hz < 0 || w.f_hi_hz > nyqOut) this.bad(`window must lie within 0 .. ${nyqOut} Hz`);
          if (w.f_lo_hz >= w.f_hi_hz) this.bad("window needs f_lo_hz < f_hi_hz");
          const n = Math.max(2, Math.min(4096, Math.round(w.points)));
          const freqs_hz = Array.from({ length: n }, (_, i) => (i === n - 1 ? w.f_hi_hz : w.f_lo_hz + ((w.f_hi_hz - w.f_lo_hz) * i) / (n - 1)));
          const mag = this.mockMagnitude(upCfg, srcRate);
          res.window = { f_lo_hz: w.f_lo_hz, f_hi_hz: w.f_hi_hz, freqs_hz, magnitude_db: freqs_hz.map(mag) };
        }
        return res;
      }
      default:
        throw new DaemonError(ErrorCode.INVALID_REQUEST, `unknown method "${method}"`);
    }
  }
}

/** Delays of the mock socket, in ms. */
export interface MockLatency {
  /** Until the daemon handles a request (requests are handled in the order sent). */
  request?: number;
  /** From handling a request to its reply arriving. */
  reply?: number;
  /** From the daemon emitting an event to it arriving (events have their own connection). */
  event?: number;
  /** Separate delay for heavy analysis calls (`upsampler_response`). */
  analysis?: number;
  /** When true, analysis calls reject with `ErrorCode.TIMEOUT`. */
  analysisTimeout?: boolean;
  /** How long `set_upsampler` takes to design the filter (ms) before the
   *  daemon applies it and answers, one build at a time (`?slow_build=<s>`). */
  build?: number;
  /** The client's timeout on `set_upsampler` (ms): a longer build rejects
   *  with `ErrorCode.TIMEOUT` while the daemon goes on and applies it
   *  (`?build_timeout=<s>`). */
  buildTimeout?: number;
}

/** Transport backed by a MockDaemon (with a little latency, like a socket). */
export class MockTransport implements Transport {
  readonly kind = "mock" as const;
  private latency: { request: number; reply: number; event: number; analysis?: number; analysisTimeout?: boolean; build?: number; buildTimeout?: number };
  /** The build the mock daemon is busy with: the next one starts after it. */
  private building: Promise<unknown> = Promise.resolve();
  private analysisSeq = 0;
  private cancelPendingAnalysis: (() => void) | null = null;
  constructor(
    public daemon: MockDaemon = new MockDaemon(),
    latency: MockLatency = {},
  ) {
    this.latency = { request: 4, reply: 0, event: 0, ...latency };
  }

  async call<T>(method: string, params?: unknown): Promise<T> {
    if (method === "set_upsampler" && this.latency.build) return this.slowBuild<T>(params);
    await new Promise((r) => setTimeout(r, this.latency.request));
    const result = await this.daemon.handle(method, params === undefined ? undefined : clone(params));
    if (this.latency.reply > 0) await new Promise((r) => setTimeout(r, this.latency.reply));
    return result as T;
  }

  afterUpsamplerBuild(): Promise<EngineConfig> {
    const read = this.building.then(() => clone(this.daemon.config));
    const limit = this.latency.buildTimeout;
    if (limit === undefined) return read;
    return Promise.race([
      read,
      new Promise<EngineConfig>((_, reject) => setTimeout(() => reject(new DaemonError(ErrorCode.TIMEOUT, "The engine did not answer in time.")), limit)),
    ]);
  }

  /** `set_upsampler` with [`MockLatency.build`]: designed, then applied, in turn. */
  private slowBuild<T>(params: unknown): Promise<T> {
    const build = this.latency.build ?? 0;
    const run = this.building.then(async () => {
      // Sent again unchanged (the app asking for the outcome): nothing to design.
      const same = JSON.stringify(params) === JSON.stringify(this.daemon.config.chain.upsampler);
      if (!same) await new Promise((r) => setTimeout(r, build));
      return (await this.daemon.handle("set_upsampler", clone(params))) as T;
    });
    this.building = run.catch(() => undefined);
    const limit = this.latency.buildTimeout;
    if (limit === undefined) return run;
    return Promise.race([
      run,
      new Promise<T>((_, reject) => setTimeout(() => reject(new DaemonError(ErrorCode.TIMEOUT, "The engine did not answer in time.")), limit)),
    ]);
  }

  start(h: TransportHandlers): () => void {
    let unsub: (() => void) | null = null;
    let live = true;
    const topics: Topic[] = ["meters", "status", "streams", "now_playing", "config", "presets", "devices"];
    const delay = this.latency.event;
    const onEvent =
      delay > 0
        ? (e: DaemonEvent) =>
            setTimeout(() => {
              if (live) h.onEvent(e);
            }, delay)
        : h.onEvent;
    const up = () => {
      h.onConnection("up");
      unsub = this.daemon.subscribe(onEvent);
      this.daemon.snapshot(topics);
    };
    const offConn = this.daemon.onConnection((isUp) => {
      if (isUp) up();
      else {
        unsub?.();
        h.onConnection("down", "The engine stopped.");
      }
    });
    h.onConnection("connecting");
    const t = setTimeout(() => {
      if (this.daemon.running) up();
      else h.onConnection("down", "No engine is listening on the control socket.");
    }, 60);
    return () => {
      live = false;
      clearTimeout(t);
      offConn();
      unsub?.();
      this.cancelPendingAnalysis?.();
      this.daemon.setSpectrumEnabled(false);
    };
  }

  setSpectrumEnabled(enabled: boolean): void {
    this.daemon.setSpectrumEnabled(enabled);
  }

  upsamplerResponse(config: UpsamplerConfig | null, sourceRate: number | null, window?: ResponseWindowRequest): Promise<UpsamplerResponse> {
    const seq = ++this.analysisSeq;
    this.cancelPendingAnalysis?.();
    const delay = this.latency.analysis ?? this.latency.request;
    return new Promise<UpsamplerResponse>((resolve, reject) => {
      let settled = false;
      let timer: ReturnType<typeof setTimeout> | null = null;
      const cancel = () => {
        if (settled) return;
        settled = true;
        if (timer) clearTimeout(timer);
        if (this.cancelPendingAnalysis === cancel) this.cancelPendingAnalysis = null;
        reject(new DaemonError(ErrorCode.SUPERSEDED, "Superseded by a newer measurement."));
      };
      this.cancelPendingAnalysis = cancel;
      timer = setTimeout(async () => {
        if (settled || seq !== this.analysisSeq) return;
        try {
          if (this.latency.analysisTimeout) {
            throw new DaemonError(ErrorCode.TIMEOUT, "The engine did not answer in time.");
          }
          const result = await this.daemon.handle("upsampler_response", clone(window ? { config, source_rate: sourceRate, window } : { config, source_rate: sourceRate }));
          if (this.latency.analysis === undefined && this.latency.reply > 0) {
            await new Promise((r) => setTimeout(r, this.latency.reply));
          }
          if (settled || seq !== this.analysisSeq) {
            cancel();
            return;
          }
          settled = true;
          if (this.cancelPendingAnalysis === cancel) this.cancelPendingAnalysis = null;
          resolve(result as UpsamplerResponse);
        } catch (e) {
          if (settled) return;
          settled = true;
          if (this.cancelPendingAnalysis === cancel) this.cancelPendingAnalysis = null;
          reject(e);
        }
      }, delay);
    });
  }

  async startEngine(): Promise<void> {
    await new Promise((r) => setTimeout(r, 700));
    this.daemon.startEngine();
  }

  atLogin = false;

  async engineAtLogin(): Promise<boolean | null> {
    return this.atLogin;
  }

  async setEngineAtLogin(on: boolean): Promise<void> {
    this.atLogin = on;
  }

  async engineLastError(): Promise<string | null> {
    return null;
  }

  async restartEngine(): Promise<void> {
    await new Promise((r) => setTimeout(r, 300));
    this.daemon.restartEngine();
  }

  async appVersion(): Promise<string | null> {
    return MOCK_APP_VERSION;
  }

  async appUpdatePending(): Promise<null> {
    return null;
  }

  async restartApp(): Promise<void> {
    if (typeof location !== "undefined") location.reload();
  }

  async appImageSetup(): Promise<null> {
    return null;
  }

  async installAppImage(): Promise<void> {
    throw new Error("Only the AppImage has a setup.");
  }

  async pickCalibrationBundle(): Promise<string | null> {
    return "/home/me/room-correction/desk/bundle.json";
  }

  async pickImpulseResponse(): Promise<string | null> {
    const names = Object.keys(IR_FACTS);
    const used = new Set(convList(this.daemon.config.chain).map((c) => c.ir_path));
    const name = names.find((n) => !used.has(IR_DIR + n)) ?? names[2];
    return IR_DIR + name;
  }

  async irInfo(path: string): Promise<IrInfo | null> {
    const name = path.split("/").pop() ?? "";
    return IR_FACTS[name] ?? { channels: 2, frames: 32768, rate: 48000 };
  }

  async copyText(text: string): Promise<void> {
    await navigator.clipboard.writeText(text);
  }

  // Overlays live in the process, like the real app's config directory.
  async listOverlays(): Promise<unknown[]> {
    await new Promise((r) => setTimeout(r, this.latency.request));
    return clone([...this.daemon.overlays.values()]);
  }

  async saveOverlay(name: string, data: unknown): Promise<void> {
    await new Promise((r) => setTimeout(r, this.latency.request));
    if (!data || typeof data !== "object" || (data as OverlayData).name !== name) {
      throw new DaemonError(ErrorCode.INVALID_PARAMS, "the overlay's name and its file name must match");
    }
    this.daemon.overlays.set(name, clone(data as OverlayData));
  }

  async deleteOverlay(name: string): Promise<void> {
    await new Promise((r) => setTimeout(r, this.latency.request));
    this.daemon.overlays.delete(name);
  }

  // ---------------------------------------------------------------- shell
  // The desktop shell in memory: settings, start at login, the volume keys'
  // state. URL switches: `?window=popup|osd` picks the window,
  // `?keys=none|declined|x11` what turning the keys on leads to, `?notray`
  // a desktop without a tray host, `?kde` a KDE Plasma session.

  shell: ShellView = { settings: { ...DEFAULT_UI_SETTINGS }, autostart: false, keys: { state: "off" }, tray: true, kde: false };
  /** Calls a test can look at: "show-main", "hide-popup", "osd:<source>". */
  shellCalls: string[] = [];
  lastOsd: OsdPayload | null = null;
  private shellHandlers = new Set<(e: ShellEvent) => void>();

  private query(): URLSearchParams {
    return new URLSearchParams(typeof location !== "undefined" ? location.search : "");
  }

  windowLabel(): WindowLabel {
    const w = this.query().get("window");
    return w === "popup" || w === "osd" ? w : "main";
  }

  /** Send a shell message to this page (tests: a volume key, an OSD payload). */
  emitShell(e: ShellEvent): void {
    for (const h of this.shellHandlers) h(clone(e));
  }

  async shellView(): Promise<ShellView> {
    if (this.query().has("notray")) this.shell.tray = false;
    if (this.query().has("kde")) this.shell.kde = true;
    return clone(this.shell);
  }

  async setUiSettings(s: UiSettings): Promise<ShellView> {
    await new Promise((r) => setTimeout(r, this.latency.request));
    const step = snapVolumeStep(s.volume_step_db);
    const was = this.shell.settings.volume_keys;
    this.shell.settings = { ...s, volume_step_db: step };
    if (s.volume_keys !== was) this.setKeys(s.volume_keys);
    const view = clone(this.shell);
    this.emitShell({ kind: "ui-settings", view });
    return view;
  }

  private setKeys(on: boolean) {
    const send = (keys: KeysState) => {
      this.shell.keys = keys;
      this.emitShell({ kind: "keys-state", keys: clone(keys) });
    };
    if (!on) return send({ state: "off" });
    send({ state: "starting" });
    const mode = this.query().get("keys");
    setTimeout(() => {
      if (!this.shell.settings.volume_keys) return;
      if (mode === "none") send({ state: "unavailable", reason: "No desktop portal is running: on Wayland the volume keys need the desktop's global shortcuts portal (KDE Plasma has it)." });
      else if (mode === "declined") send({ state: "declined" });
      else
        send({
          state: "active",
          via: mode === "x11" ? "x11" : "portal",
          bound: [
            { action: "up", trigger: "Volume Up" },
            { action: "down", trigger: "Volume Down" },
            { action: "mute", trigger: "Volume Mute" },
          ],
        });
    }, 300);
  }

  async setAutostart(on: boolean): Promise<ShellView> {
    await new Promise((r) => setTimeout(r, this.latency.request));
    this.shell.autostart = on;
    const view = clone(this.shell);
    this.emitShell({ kind: "ui-settings", view });
    return view;
  }

  onShell(handler: (e: ShellEvent) => void): () => void {
    this.shellHandlers.add(handler);
    return () => this.shellHandlers.delete(handler);
  }

  showMainWindow(): void {
    this.shellCalls.push("show-main");
  }

  hidePopup(): void {
    this.shellCalls.push("hide-popup");
  }

  osdShow(p: OsdPayload, source: "keys" | "scroll"): void {
    this.shellCalls.push(`osd:${source}`);
    this.lastOsd = clone(p);
  }

  async osdReady(): Promise<OsdPayload | null> {
    // A standalone OSD page in the browser shows a sample level, or the
    // one in the URL (`?window=osd&db=-12.5&muted`).
    const q = this.query();
    const db = q.get("db");
    return this.lastOsd ?? { volume_db: db === null ? -23.5 : Number(db), muted: q.has("muted"), min_db: -80, max_db: q.has("ceiling") ? Number(q.get("ceiling")) : 0, note: q.get("note") };
  }

  // The app process keeps the main view's place while its window is closed
  // to the tray; here the tab's session storage stands in for it, so a
  // reload plays the part of the window being made again.
  saveMainView(m: MainViewMemory): void {
    try {
      sessionStorage.setItem(MAIN_VIEW_KEY, JSON.stringify(m));
    } catch {
      /* no storage: nothing is kept */
    }
  }

  mainCloseReady(m: MainViewMemory | null): void {
    if (m) this.saveMainView(m);
    this.shellCalls.push("main-close-ready");
  }

  async loadMainView(): Promise<MainViewMemory | null> {
    try {
      const raw = sessionStorage.getItem(MAIN_VIEW_KEY);
      return raw ? (JSON.parse(raw) as MainViewMemory) : null;
    } catch {
      return null;
    }
  }
}

const MAIN_VIEW_KEY = "avdsp-mock-main-view";

/** A path latency with nothing in it, at `rate`. */
function zeroLatency(rate: number): PathLatency {
  return {
    rate_hz: rate,
    chain_frames: 0,
    chain_us: 0,
    compensation_frames: 0,
    compensation_us: 0,
    alignment_frames: 0,
    alignment_us: 0,
    total_frames: 0,
    total_us: 0,
  };
}
