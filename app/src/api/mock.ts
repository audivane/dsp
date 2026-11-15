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
// DAC without a hardware mixer (digital volume).

import { autoPreampDb, profileAutoPreampDb, profileChannelResponseDb } from "../lib/eqmath";
import { kaiserAttenuationDb, minimumPhaseBlockReason, nyquistStopBlockReason, nyquistStopCutoff } from "../lib/upsamplerLimits";
import { parseApo } from "../lib/apo";
import { BUILTIN_TARGETS, DRAW_FREQS, OVERLAY_COLOURS, type OverlayData } from "../lib/overlays";
import { bandCount, convList, deepEqual, emptyEq, eqIsActive, normalizeEq } from "../lib/chain";
import { DaemonError, ErrorCode, type IrInfo, type Transport, type TransportHandlers } from "./transport";
import type {
  AbConfig,
  AbSide,
  Band,
  ChainConfig,
  DaemonEvent,
  DeviceInfo,
  EngineConfig,
  Meters,
  NowPlaying,
  PassthroughBreak,
  Preset,
  PresetInfo,
  ResponseWindowRequest,
  SelfTestResult,
  SetVolumeParams,
  SinkChoice,
  Spectrum,
  Status,
  StreamInfo,
  Topic,
  UpsamplerConfig,
  UpsamplerResponse,
  VolumeStatus,
} from "./types";
import { BALANCE_MAX_DB, VOLUME_MAX_DB, VOLUME_MIN_DB } from "./types";

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

function baseChain(): ChainConfig {
  return {
    eq: eqProfile("HD650 AutoEQ", HD650),
    convolution: [
      { enabled: true, ir_path: IR_DIR + "Living room correction.wav", gain_db: 0, name: null },
      { enabled: true, ir_path: IR_DIR + "HD650 crossfeed.wav", gain_db: -1, name: null },
    ],
    upsampler: { target: { ratio: 4 }, taps: 65536, phase: "linear", kaiser_beta: 14, cutoff: 1, cutoff_mode: "fraction" },
    output: { bits: 24, dither: "tpdf" },
  };
}

/** `ChainConfig::default()` with an EQ: what `import_apo` stores. */
function defaultChain(eq: ChainConfig["eq"]): ChainConfig {
  return {
    eq,
    convolution: [],
    upsampler: { target: "off", taps: 65536, phase: "linear", kaiser_beta: 14, cutoff: 1, cutoff_mode: "fraction" },
    output: { bits: 24, dither: "tpdf" },
  };
}

function flatChain(): ChainConfig {
  const c = baseChain();
  return { ...c, eq: emptyEq("Flat"), convolution: [] };
}

const DEVICES: DeviceInfo[] = [
  { id: "hw:CARD=DAC,DEV=0", card: "DAC", name: "USB Audio DAC", hdmi: false, loopback: false, active: true },
  { id: "hw:CARD=PCH,DEV=0", card: "PCH", name: "Built-in Audio", hdmi: false, loopback: false, active: false },
  { id: "hw:CARD=HDMI,DEV=3", card: "HDMI", name: "HDMI 0 (monitor)", hdmi: true, loopback: false, active: false },
  { id: "hw:CARD=Loopback,DEV=0", card: "Loopback", name: "Loopback", hdmi: false, loopback: true, active: false },
];

/** Like the daemon's `mark_active`: `hw:CARD=DAC,DEV=0`, `hw:DAC,0`, `hw:DAC`, `hw:DAC,0,0` are one device. */
function sameDevice(d: DeviceInfo, configured: string): boolean {
  const m = /^hw:(?:CARD=)?([^,]+)(?:,(?:DEV=)?(\d+))?(?:,(?:SUBDEV=)?\d+)?$/i.exec(configured.trim());
  return !!m && m[1].toLowerCase() === d.card.toLowerCase() && d.id.endsWith(`DEV=${m[2] ?? "0"}`);
}

function ratioOf(chain: ChainConfig, rate: number): number {
  const t = chain.upsampler.target;
  if (t === "off") return 1;
  if ("ratio" in t) return Math.max(1, t.ratio);
  let r = 1;
  while (rate * r * 2 <= t.max_rate) r *= 2;
  return r;
}

function chainIsPassthrough(chain: ChainConfig): boolean {
  const eqActive = eqIsActive(chain.eq);
  const conv = convList(chain).some((c) => c.enabled);
  return !eqActive && !conv && chain.upsampler.target === "off";
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
    name: "Kali IN-5 (mock room)",
    kind: "measurement",
    source: "Simulated: this is the browser mock, not a real measurement.",
    points,
    view: { colour: OVERLAY_COLOURS[1], visible: true, offset_db: 0, predicted: true },
  };
}

type Listener = (e: DaemonEvent) => void;

export class MockDaemon {
  config: EngineConfig;
  presets = new Map<string, Preset & { modified_unix: number }>();
  streams: StreamInfo[];
  nowPlaying: NowPlaying | null;
  volume: VolumeStatus = { volume_db: -12, balance_db: 0, muted: false, hardware: true, min_db: -80, max_db: 0 };
  abSide: AbSide = "a";
  released = false;
  running: boolean;
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

  constructor(opts: { running?: boolean; apps?: boolean; split?: boolean; overlays?: boolean | "none"; digitalVolume?: boolean } = {}) {
    this.running = opts.running ?? true;
    if (opts.digitalVolume) this.volume = { ...this.volume, hardware: false };
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
      device: DEVICES[0].id,
      formats: ["s32", "s24", "s24_3", "s16"],
      dsp_enabled: true,
      chain,
      ab: { chain_b: flatChain(), level: { mode: "analytic" } },
      rate: { follow_source: true, allowed_rates: [44100, 48000, 88200, 96000, 176400, 192000], default_rate: 48000, conflict: "keep_current" },
      quantum: 1024,
      routing: opts.apps ? { mode: "apps", rules: [{ app: "Strawberry", sink: "dsp" }] } : { mode: "system" },
      volume: { volume_db: -12, balance_db: 0, muted: false },
      active_preset: "HD650 AutoEQ",
    };
    const now = Math.floor(Date.now() / 1000);
    this.presets.set("HD650 AutoEQ", { version: 1, name: "HD650 AutoEQ", chain: clone(chain), modified_unix: now - 86400 * 3 });
    const room = baseChain();
    room.eq = eqProfile("Room + bass shelf", ROOM);
    this.presets.set("Room + bass shelf", { version: 1, name: "Room + bass shelf", chain: room, modified_unix: now - 86400 * 12 });
    this.presets.set("Flat", { version: 1, name: "Flat", chain: flatChain(), modified_unix: now - 86400 * 40 });
    this.streams = [
      { node: 81, app_name: "Strawberry", binary: "strawberry", media_name: "Nils Frahm · Says", rate: 44100, channels: 2, format: "S24LE", sink: "dsp", volume: 1, muted: false },
      { node: 94, app_name: "Firefox", binary: "firefox", media_name: "YouTube", rate: 48000, channels: 2, format: "F32LE", sink: null, volume: 1, muted: false },
      { node: 102, app_name: "Discord", binary: "Discord", media_name: "Voice", rate: 48000, channels: 2, format: "F32LE", sink: null, volume: 0.8, muted: false },
    ];
    this.nowPlaying = { player: "org.mpris.MediaPlayer2.strawberry", title: "Says", artist: "Nils Frahm", album: "Spaces", status: "Playing", native_rate: 44100, native_bits: 24 };
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
    if (topics.includes("streams")) this.emit({ event: "streams", data: this.streams });
    if (topics.includes("now_playing")) this.emit({ event: "now_playing", data: this.nowPlaying });
    if (topics.includes("config")) this.emit({ event: "config", data: this.config });
    if (topics.includes("presets")) this.emit({ event: "presets", data: this.listPresets() });
  }

  startEngine() {
    this.running = true;
    for (const l of this.connListeners) l(true);
  }

  private ensureMeters() {
    if (this.meterTimer) return;
    this.meterTimer = setInterval(() => {
      if (!this.running || this.listeners.size === 0) return;
      this.tickCount++;
      this.emit({ event: "meters", data: this.meters() });
      if (this.spectrumEnabled && this.tickCount % 2 === 0 && this.feeding().length > 0 && !this.released && !this.volume.muted) {
        this.emit({ event: "spectrum", data: this.spectrum() });
      }
    }, 33);
  }

  stop() {
    if (this.meterTimer) clearInterval(this.meterTimer);
    this.meterTimer = null;
  }

  // ------------------------------------------------------------ derived state

  private feeding(): StreamInfo[] {
    const sink = this.activeSink();
    return this.streams.filter((s) => s.sink !== null && s.sink === sink);
  }

  private activeSink(): SinkChoice | null {
    if (this.released) return null;
    const hasDirect = this.streams.some((s) => s.sink === "direct");
    const hasDsp = this.streams.some((s) => s.sink === "dsp");
    return hasDirect && !hasDsp ? "direct" : hasDsp ? "dsp" : null;
  }

  private chainOn(): ChainConfig {
    const ab = this.config.ab;
    return this.abSide === "b" && ab ? ab.chain_b : this.config.chain;
  }

  /** Rate the graph last ran at while something played (for `keep_current`). */
  private lastRate = 0;

  /**
   * Like the engine's rate arbiter, without its timers: one app (or several
   * that agree) is followed; when they disagree `rate.conflict` decides.
   * Streams are listed in start order, so the first one started first.
   */
  private graphRate(): number {
    const policy = this.config.rate;
    if (!policy.follow_source) return policy.default_rate;
    const wanted = this.feeding()
      .map((s) => s.rate)
      .filter((r): r is number => !!r && policy.allowed_rates.includes(r));
    let rate: number;
    if (wanted.length === 0) rate = this.lastRate || policy.default_rate;
    else if (wanted.every((r) => r === wanted[0])) rate = wanted[0];
    else if (policy.conflict === "highest") rate = Math.max(...wanted);
    else if (policy.conflict === "default_rate") rate = policy.default_rate;
    else rate = wanted.includes(this.lastRate) ? this.lastRate : wanted[0];
    this.lastRate = rate;
    return rate;
  }

  status(): Status {
    const chain = this.chainOn();
    const rate = this.graphRate();
    const sink = this.activeSink();
    const dspPath = this.config.dsp_enabled && sink !== "direct";
    const ratio = dspPath ? ratioOf(chain, rate) : 1;
    const feeding = this.feeding();
    const breaks: PassthroughBreak[] = [];
    if (feeding.length === 0) breaks.push({ reason: "not_playing" });
    const dspActive = dspPath && !chainIsPassthrough(chain);
    if (dspActive) breaks.push({ reason: "dsp_active" });
    if (ratio > 1) breaks.push({ reason: "upsampling", ratio });
    if (!this.volume.hardware && dspPath && this.volume.volume_db !== 0) breaks.push({ reason: "volume", node: 40, volume: Math.pow(10, this.volume.volume_db / 20) });
    for (const s of feeding) if ((s.volume ?? 1) !== 1 || s.muted) breaks.push({ reason: "volume", node: s.node, volume: s.muted ? 0 : (s.volume ?? 1) });
    if (feeding.length > 1) breaks.push({ reason: "mixing", streams: feeding.length });
    for (const s of feeding) if (s.rate && s.rate !== rate) breaks.push({ reason: "resampled", stream_rate: s.rate, graph_rate: rate });
    const upTaps = chain.upsampler.target === "off" ? 0 : chain.upsampler.taps;
    const upLat = dspPath && ratio > 1 ? (chain.upsampler.phase === "linear" ? upTaps / 2 / (rate * ratio) * 1000 : 0.4) : 0;
    const convLat = dspPath && convList(chain).some((c) => c.enabled) ? 2.7 : 0;
    this.latency = (this.config.quantum / 48000) * 1000 * 2 + upLat + convLat;
    const loadBase = !dspPath ? 0.004 : 0.012 + convList(chain).filter((c) => c.enabled).length * 0.02 + (ratio > 1 ? (upTaps / 65536) * 0.03 * (ratio / 4) : 0);
    const presetName = this.config.active_preset ?? null;
    const preset = presetName ? this.presets.get(presetName) : undefined;
    return {
      version: "0.1.0 (mock)",
      state: !this.running ? "error" : this.released ? "idle" : feeding.length ? "playing" : "ready",
      device: this.config.device,
      error: null,
      format: this.released ? null : this.config.formats[0] ?? null,
      graph_rate: rate,
      output_rate: rate * ratio,
      ratio,
      quantum: this.config.quantum,
      latency_ms: Math.round(this.latency * 10) / 10,
      filter_latency_ms: Math.round((upLat + convLat) * 10) / 10,
      xruns: 0,
      dsp_load: Math.round(loadBase * 1000) / 1000,
      dsp_enabled: this.config.dsp_enabled,
      active_sink: sink,
      passthrough: {
        intact: breaks.length === 0,
        broken_because: breaks,
        caveats: [],
      },
      ab: this.config.ab ? { enabled: true, current: this.abSide, offset_db: this.abOffset() } : null,
      volume: clone(this.volume),
      preset: preset ? { name: preset.name, modified: !deepEqual(preset.chain, this.config.chain) } : null,
    };
  }

  private abOffset(): number {
    const ab = this.config.ab;
    if (!ab) return 0;
    if (ab.level.mode === "off") return 0;
    if (ab.level.mode === "manual") return ab.level.offset_db;
    const loud = (c: ChainConfig) => {
      const eq = c.eq;
      if (!eq) return 0;
      const freqs = [100, 300, 1000, 3000, 6000];
      const l = profileChannelResponseDb(eq, 0, 48000, freqs);
      const r = profileChannelResponseDb(eq, 1, 48000, freqs);
      return (l.reduce((a, b) => a + b, 0) + r.reduce((a, b) => a + b, 0)) / (2 * freqs.length);
    };
    return Math.round((loud(this.config.chain) - loud(ab.chain_b)) * 10) / 10;
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
    };
  }

  private spectrum(): Spectrum {
    const rate = this.graphRate();
    const fMin = 20;
    const bpo = 24;
    const maxF = Math.min(20000, rate / 2);
    const nBins = Math.max(1, Math.floor(bpo * Math.log2(maxF / fMin)) + 1);
    const freqs: number[] = [];
    const pre: number[] = [];
    const t = this.phase;
    for (let i = 0; i < nBins; i++) {
      const f = fMin * Math.pow(2, i / bpo);
      freqs.push(f);
      const oct = Math.log2(f / 55);
      const tilt = oct > 0 ? -3.0 * oct : -1.2 * Math.abs(oct);
      const motion =
        4.5 * Math.sin(t * 1.5 + i * 0.08) +
        3.0 * Math.cos(t * 2.4 - i * 0.13) +
        2.0 * Math.sin(t * 0.7 + Math.log2(f) * 2.0);
      const db = Math.max(-88, Math.min(-3, -24 + tilt + motion));
      pre.push(Math.round(db * 10) / 10);
    }
    const chain = this.chainOn();
    let post: number[];
    if (this.config.dsp_enabled && chain.eq) {
      const l = profileChannelResponseDb(chain.eq, 0, rate, freqs);
      const r = profileChannelResponseDb(chain.eq, 1, rate, freqs);
      post = pre.map((v, i) => Math.round(Math.max(-90, Math.min(0, v + (l[i] + r[i]) * 0.5)) * 10) / 10);
    } else {
      post = [...pre];
    }
    return { f_min_hz: fMin, bins_per_octave: bpo, pre, post, rate };
  }

  private mockRatio(cfg: UpsamplerConfig, sourceRate: number): number {
    const t = cfg.target;
    let ratio = 1;
    if (t !== "off") {
      if ("ratio" in t) ratio = Math.max(1, t.ratio);
      else while (sourceRate * ratio * 2 <= t.max_rate) ratio *= 2;
    }
    return ratio;
  }

  private nyquistStopCutoff(taps: number, ratio: number, kaiserBeta: number): number {
    return nyquistStopCutoff(taps, ratio, kaiserBeta);
  }

  private effectiveCutoff(cfg: UpsamplerConfig, ratio: number): number {
    if (ratio <= 1) return 1;
    if (cfg.cutoff_mode === "nyquist_stop") {
      return this.nyquistStopCutoff(cfg.taps, ratio, cfg.kaiser_beta);
    }
    return Math.max(0.5, Math.min(1, cfg.cutoff));
  }

  private mockTransitionWidth(cfg: UpsamplerConfig, sourceRate: number, ratio: number): number {
    const nyquistIn = sourceRate / 2;
    return Math.max(0.5, Math.min(0.24 * nyquistIn, ((cfg.kaiser_beta + 2) / (Math.PI * Math.max(256, cfg.taps))) * sourceRate * ratio * 2.5));
  }

  /** Smooth analytic stand-in for the measured magnitude (dB) of a design. */
  private mockMagnitude(cfg: UpsamplerConfig, sourceRate: number): (f: number) => number {
    const ratio = this.mockRatio(cfg, Math.max(1, Math.round(sourceRate)));
    if (ratio <= 1) return () => 0;
    const nyquistIn = sourceRate / 2;
    const fc = this.effectiveCutoff(cfg, ratio) * nyquistIn;
    const atten = Math.min(230, Math.max(40, kaiserAttenuationDb(cfg.kaiser_beta)));
    const tw = this.mockTransitionWidth(cfg, sourceRate, ratio);
    const stopFloor = Math.pow(10, -atten / 20);
    return (f: number): number => {
      if (f <= 0) return 0;
      const trans = 1 / (1 + Math.exp((f - fc) / (tw * 0.14)));
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
    const t = cfg.target;
    let ratio = 1;
    if (t !== "off") {
      if ("ratio" in t) ratio = Math.max(1, t.ratio);
      else while (source_rate * ratio * 2 <= t.max_rate) ratio *= 2;
    }
    const output_rate = source_rate * ratio;
    const nyquistIn = source_rate / 2;
    const nyquistOut = output_rate / 2;

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

    if (ratio === 1) {
      return {
        source_rate,
        output_rate,
        ratio: 1,
        taps: cfg.taps,
        phase: cfg.phase,
        effective_cutoff: 1,
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

    const cutoff = this.effectiveCutoff(cfg, ratio);
    const fc = cutoff * nyquistIn;
    const tw = this.mockTransitionWidth(cfg, source_rate, ratio);
    const magDbAt = this.mockMagnitude(cfg, source_rate);

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
      const f = (nyquistOut * i) / STAT_N;
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
    let stopband_start_hz = Math.min(nyquistOut, fc + tw * 0.85);
    for (let i = statDb.length - 1; i >= 0; i--) {
      if (statFreq[i] >= nyquistIn && statDb[i] > threshold) {
        stopband_start_hz = statFreq[Math.min(statFreq.length - 1, i + 1)];
        break;
      }
    }

    const crossings = this.mockCrossings(magDbAt, nyquistOut, stopband_atten_db);
    const crossingAt = (level: number) => crossings.find((c) => c.level_db === level)?.freq_hz;
    minus_0_1_db_hz = crossingAt(-0.1) ?? minus_0_1_db_hz;
    minus_3_db_hz = crossingAt(-3) ?? minus_3_db_hz;

    const impulseLen = 256;
    const impulse: number[] = [];
    const impulse_peak = cfg.phase === "linear" ? 128 : 24;
    for (let i = 0; i < impulseLen; i++) {
      const n = i - impulse_peak;
      if (cfg.phase === "minimum" && n < -6) {
        impulse.push(0);
        continue;
      }
      const w = 0.5 - 0.5 * Math.cos((2 * Math.PI * i) / (impulseLen - 1));
      const arg = (Math.PI * cutoff * n) / ratio;
      const sinc = arg === 0 ? 1 : Math.sin(arg) / arg;
      const decay = cfg.phase === "minimum" && n > 0 ? Math.exp(-n / 90) : 1;
      impulse.push(sinc * w * decay);
    }

    return {
      source_rate,
      output_rate,
      ratio,
      taps: cfg.taps,
      phase: cfg.phase,
      effective_cutoff: cutoff,
      freqs_hz,
      magnitude_db,
      impulse,
      impulse_peak,
      passband_ripple_db: Number.isFinite(minPb) ? Math.max(0, maxPb - minPb) : 0,
      minus_0_1_db_hz,
      minus_3_db_hz,
      stopband_start_hz,
      stopband_atten_db,
      latency_samples: cfg.phase === "linear" ? Math.floor((cfg.taps - 1) / 2) + 1024 * ratio : 1024 * ratio,
      crossings,
    };
  }

  private listPresets(): PresetInfo[] {
    return [...this.presets.values()]
      .map((p) => ({ name: p.name, modified_unix: p.modified_unix, bands: bandsCountOfChain(p.chain), convolution: convList(p.chain).length }))
      .sort((a, b) => a.name.localeCompare(b.name));
  }

  private info(p: Preset & { modified_unix: number }): PresetInfo {
    return { name: p.name, modified_unix: p.modified_unix, bands: bandsCountOfChain(p.chain), convolution: convList(p.chain).length };
  }

  // ------------------------------------------------------------ calls

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

  private validateChain(chain: ChainConfig) {
    if (chain.eq) {
      this.validateBands(chain.eq.bands);
      if (chain.eq.left) this.validateBands(chain.eq.left.bands);
      if (chain.eq.right) this.validateBands(chain.eq.right.bands);
    }
    if (chain.upsampler.taps < 1 || chain.upsampler.taps > 4_194_304) this.bad("taps must be 1..=4194304");
    if (!Number.isFinite(chain.upsampler.kaiser_beta) || chain.upsampler.kaiser_beta < 0 || chain.upsampler.kaiser_beta > 40) {
      this.bad(`kaiser_beta must be between 0 and 40 (got ${chain.upsampler.kaiser_beta})`);
    }
    // Same rules as the daemon's validate_upsampler.
    if (chain.upsampler.phase === "minimum") {
      const why = minimumPhaseBlockReason(chain.upsampler.kaiser_beta);
      if (why) this.bad(why);
    }
    if (chain.upsampler.cutoff_mode === "nyquist_stop") {
      const why = nyquistStopBlockReason(chain.upsampler);
      if (why) this.bad(why);
    }
  }

  /** Chain B follows chain A's upsampler and output, like the daemon. */
  private normalizeB() {
    const ab = this.config.ab;
    if (!ab) return;
    ab.chain_b.upsampler = clone(this.config.chain.upsampler);
    ab.chain_b.output = clone(this.config.chain.output);
  }

  handle(method: string, params: unknown): unknown {
    if (!this.running) throw new DaemonError(ErrorCode.DISCONNECTED, "the engine is not running");
    const p = params as Record<string, unknown>;
    const cfg = this.config;
    const mutate = (fn: () => void) => {
      fn();
      this.normalizeB();
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
          if (next.chain.eq) next.chain.eq = normalizeEq(next.chain.eq);
          this.validateChain(next.chain);
          // Like the daemon, the whole config is applied, volume included.
          if (next.volume) {
            const db = Math.max(this.volume.min_db, Math.min(this.volume.max_db, next.volume.volume_db));
            this.volume = { ...this.volume, volume_db: db, balance_db: next.volume.balance_db, muted: next.volume.muted };
            next.volume = { ...next.volume, volume_db: db };
          }
          this.config = next;
        });
      case "set_eq":
        return mutate(() => {
          const rawEq = params === null ? null : clone(params as NonNullable<ChainConfig["eq"]>);
          const eq = rawEq ? normalizeEq(rawEq) : null;
          const chain = { ...cfg.chain, eq };
          this.validateChain(chain);
          cfg.chain = chain;
        });
      case "set_convolution":
        return mutate(() => {
          const v = params as ChainConfig["convolution"];
          cfg.chain.convolution = v === null ? [] : Array.isArray(v) ? clone(v) : [clone(v)];
        });
      case "set_upsampler":
        return mutate(() => {
          const next = { ...cfg.chain, upsampler: clone(params as ChainConfig["upsampler"]) };
          this.validateChain(next);
          cfg.chain = next;
        });
      case "set_dsp_enabled":
        return mutate(() => {
          cfg.dsp_enabled = !!p.enabled;
        });
      case "set_ab": {
        const notes: string[] = [];
        mutate(() => {
          cfg.ab = params === null ? null : clone(params as AbConfig);
          if (cfg.ab && !deepEqual(cfg.ab.chain_b.upsampler, cfg.chain.upsampler)) notes.push("Side B uses side A's upsampling.");
          if (!cfg.ab) this.abSide = "a";
        });
        return { ...(this.status().ab ?? { enabled: false, current: "a", offset_db: 0 }), ...(notes.length ? { notes } : {}) };
      }
      case "ab_select": {
        if (!cfg.ab) throw new DaemonError(ErrorCode.INVALID_PARAMS, "A/B is not configured");
        this.abSide = p.side === "b" ? "b" : "a";
        this.emitStatus();
        return this.status().ab;
      }
      case "list_streams":
        return clone(this.streams);
      case "route": {
        const target = p.target as { node: number } | { app: string };
        const sink = p.sink as SinkChoice;
        this.streams = this.streams.map((s) => {
          const hit = "node" in target ? s.node === target.node : s.app_name?.toLowerCase() === target.app.toLowerCase();
          return hit ? { ...s, sink: sink === "default" ? (cfg.routing.mode === "system" ? "dsp" : null) : sink } : s;
        });
        this.emit({ event: "streams", data: this.streams });
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
          this.emit({ event: "streams", data: this.streams });
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
        this.emitStatus();
        return null;
      case "subscribe":
        return null;
      case "shutdown":
        this.running = false;
        for (const l of this.connListeners) l(false);
        return null;
      case "set_volume": {
        const v = params as SetVolumeParams;
        if (!(v.volume_db >= VOLUME_MIN_DB && v.volume_db <= VOLUME_MAX_DB)) this.bad(`volume_db must be within ${VOLUME_MIN_DB}..=${VOLUME_MAX_DB} dB`);
        if (!(Math.abs(v.balance_db) <= BALANCE_MAX_DB)) this.bad(`balance_db must be within ±${BALANCE_MAX_DB} dB`);
        this.volume = { ...this.volume, volume_db: Math.max(this.volume.min_db, Math.min(this.volume.max_db, v.volume_db)), balance_db: v.balance_db, muted: v.muted };
        cfg.volume = { volume_db: this.volume.volume_db, balance_db: v.balance_db, muted: v.muted };
        this.emitConfig();
        return clone(this.volume);
      }
      case "list_presets":
        return this.listPresets();
      case "get_preset": {
        const pr = this.presets.get(this.presetName(p.name));
        if (!pr) this.bad(`no preset named "${String(p.name)}"`);
        return { version: 1, name: pr.name, chain: clone(pr.chain) };
      }
      case "save_preset": {
        const name = this.presetName(p.name);
        if (this.presets.has(name) && !p.overwrite) this.conflict(`a preset named "${name}" already exists; pass overwrite to replace it`);
        // Chain A is stored as it is (the EQ keeps its own name), like the daemon.
        const pr = { version: 1, name, chain: clone(cfg.chain), modified_unix: Math.floor(Date.now() / 1000) };
        this.presets.set(name, pr);
        cfg.active_preset = name;
        this.emitPresets();
        this.emitConfig();
        return this.info(pr);
      }
      case "load_preset": {
        const pr = this.presets.get(this.presetName(p.name));
        if (!pr) this.bad(`no preset named "${String(p.name)}"`);
        return mutate(() => {
          cfg.chain = clone(pr.chain);
          cfg.active_preset = pr.name;
        });
      }
      case "delete_preset": {
        const name = this.presetName(p.name);
        if (!this.presets.delete(name)) this.bad(`no preset named "${name}"`);
        if (cfg.active_preset === name) cfg.active_preset = null;
        this.emitPresets();
        this.emitConfig();
        return null;
      }
      case "rename_preset": {
        const from = this.presetName(p.from);
        const to = this.presetName(p.to);
        const pr = this.presets.get(from);
        if (!pr) this.bad(`no preset named "${from}"`);
        if (from !== to && this.presets.has(to)) this.conflict(`a preset named "${to}" already exists`);
        this.presets.delete(from);
        const next = { ...pr, name: to, modified_unix: Math.floor(Date.now() / 1000) };
        this.presets.set(to, next);
        if (cfg.active_preset === from) cfg.active_preset = to;
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
        // A flat preset: the default chain with only this EQ. Not loaded; an existing name is replaced.
        const pr = { version: 1, name, chain: defaultChain(parsed.profile), modified_unix: Math.floor(Date.now() / 1000) };
        this.presets.set(name, pr);
        this.emitPresets();
        return this.info(pr);
      }
      case "list_devices":
        return DEVICES.map((d) => ({ ...d, active: sameDevice(d, cfg.device) }));
      case "upsampler_response": {
        const req = (params ?? {}) as { config?: UpsamplerConfig | null; source_rate?: number | null; window?: ResponseWindowRequest | null };
        const upCfg = req.config ?? this.chainOn().upsampler;
        const srcRate = req.source_rate ?? this.graphRate();
        const res = this.buildUpsamplerResponse(upCfg, srcRate);
        const w = req.window;
        if (w) {
          const nyqOut = res.output_rate / 2;
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
}

/** Transport backed by a MockDaemon (with a little latency, like a socket). */
export class MockTransport implements Transport {
  readonly kind = "mock" as const;
  private latency: { request: number; reply: number; event: number; analysis?: number; analysisTimeout?: boolean };
  private analysisSeq = 0;
  private cancelPendingAnalysis: (() => void) | null = null;
  constructor(
    public daemon: MockDaemon = new MockDaemon(),
    latency: MockLatency = {},
  ) {
    this.latency = { request: 4, reply: 0, event: 0, ...latency };
  }

  async call<T>(method: string, params?: unknown): Promise<T> {
    await new Promise((r) => setTimeout(r, this.latency.request));
    const result = await this.daemon.handle(method, params === undefined ? undefined : clone(params));
    if (this.latency.reply > 0) await new Promise((r) => setTimeout(r, this.latency.reply));
    return result as T;
  }

  start(h: TransportHandlers): () => void {
    let unsub: (() => void) | null = null;
    let live = true;
    const topics: Topic[] = ["meters", "status", "streams", "now_playing", "config", "presets"];
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
}
