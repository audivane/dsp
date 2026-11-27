// App state fed by daemon events. The daemon is the authority: everything
// shown comes from `config`/`status`/… as the daemon echoes them. The only
// local copies are short-lived drafts that keep a drag smooth (EQ handles,
// volume slider) until the daemon's echo replaces them.
//
// Every change goes through one queue (lib/queue.ts), one at a time. A change
// is an intent, evaluated when its turn comes against the config the daemon
// holds then (`get_config` inside the job), never against the snapshot the UI
// had when the user clicked. Targeted calls (`set_eq`, `set_volume`, …) are
// used wherever one exists, so a change never resends fields it is not about.

import { useSyncExternalStore } from "react";
import { toggleRate, withDefaultRate } from "../lib/rates";
import { fmtRate } from "../lib/format";
import { hasRateRatio, normalizeStatus, normalizeUpsamplerResponse } from "../api/legacy";
import { type AppImageSetup, type AppUpdateState, DaemonError, ErrorCode, isUnknownMethod, type ConnState, type IrInfo, type Transport } from "../api/transport";
import type {
  AbB,
  AbConfig,
  AbSide,
  Band,
  ChainConfig,
  ConvolutionConfig,
  ConvolutionSafety,
  DaemonEvent,
  DeviceAssignment,
  DeviceInfo,
  DeviceLostAction,
  DitherKind,
  EngineConfig,
  EqProfile,
  FilterPreset,
  LevelMatch,
  NowPlaying,
  OutputConfig,
  Preset,
  PresetInfo,
  PresetPart,
  RateTarget,
  RoutingMode,
  RateConflict,
  BypassResampler,
  SampleFormat,
  SelfTestResult,
  SinkChoice,
  Status,
  StreamInfo,
  StreamVolume,
  UpsamplerConfig,
  ResponseWindowRequest,
  UpsamplerResponse,
  VarispeedConfig,
  VarispeedStatus,
  SetVarispeedParams,
  VolumeConfig,
  VolumeStatus,
} from "../api/types";
import { parseApo, presetNameFromFile, toApo } from "../lib/apo";
import { applyPreset, PART_NAME, PRESET_PARTS, presetCovers } from "../lib/presetParts";
import {
  EQ_PROFILE_VERSION,
  addSection,
  clearSection,
  convList,
  convSafety,
  irKey,
  irPaths,
  deepEqual,
  emptyEq,
  hasSection,
  removeSection,
  sectionBands,
  sectionEnabled,
  sectionSet,
  setSectionEnabled,
  withSectionBands,
  type EqSection,
} from "../lib/chain";
import { freeBandFreq, profileAutoPreampDb } from "../lib/eqmath";
import { History } from "../lib/history";
import {
  OVERLAY_COLOURS,
  OverlayError,
  boundsFromCurves,
  overlayData,
  overlayFile,
  overlayNameFromFile,
  parseOverlayText,
  toOverlay,
  uniqueName,
  type Overlay,
  type OverlayData,
  type OverlayKind,
} from "../lib/overlays";
import { MutationQueue } from "../lib/queue";
import { rateKind, targetForEngine, type RateKind } from "../lib/upsamplerRate";
import { loadEqRange, loadHarmGraph, saveEqRange, saveHarmGraph, type EqRange, type HarmGraphPrefs } from "../lib/graphPrefs";
import { loadSpectrumPrefs, saveSpectrumPrefs, type SpectrumPrefs } from "../lib/spectrum";
import { throttleLatest, type Throttle } from "../lib/throttle";
import { DEFAULT_VARISPEED } from "../lib/varispeed";
import { followerOn, type FollowerPatch } from "../lib/followers";
import { DEFAULT_PAIR, isDefaultPair } from "../lib/outputChannels";
import { FLAT_PATCH } from "../lib/abPatch";
import { MEASURE_MAX_DEFAULT } from "../lib/abMeasure";
import { clampTrials, scoreRun, scoreTrial, sideFor, EXPORT_FORMAT, MODE_TEXT, type BlindButton, type BlindChoice, type BlindExport, type BlindPlan, type TrialRecord } from "../lib/abBlind";
import { commitSecret, createSecret, secretToHex, xSequence } from "../lib/abx/commitment";
import { loadSaved, storeSaved, withSaved, type CustomSel, type SavedCustomB } from "../lib/abCustom";
import { abStoppedByOverload, comparing, filterDesignPatch, filterPatch, outputPatch, stagePatch, type BPatch, type CompareMemory, type CompareStage } from "../lib/abCompare";
import type { ConvResponse, FollowerConfig, HarmonicsConfig, HarmonicsLive, HarmonicsPreset, HarmonicsReadout, HarmonicsSpectrum, HarmonicsSpectrumTrace, HarmonicsSystem, OutputChannels, OutputsStatus } from "../api/types";
import { STREAM_VOLUME_NEEDS_UPDATE, reportsStreamLevels, streamLimitNote, type StreamLevelEdit, type StreamMessage } from "../lib/streamVolume";
import { meterStore } from "./meters";
import { defaultCrossfeed, defaultHarmonics, defaultLimiter, withOrderLevel } from "../lib/effects";
import { liveHistory } from "../lib/harmonicsLive";
import { guardRiseLimit, volumeSettingsProblem } from "../lib/volumeSettings";
import { RECOMMENDED_TARGET, recommendedUpsampler } from "../lib/upsamplerRecommended";
import { defaultDesign, designKey, patchUpsampler, RECOMMENDED_PRESET } from "../lib/filterDesign";
import type { DesignCheck, FilterDesign } from "../api/types";

/** Reads behind a timed-out `set_upsampler` (each with the build timeout)
 *  before its outcome is left to later config updates. */
const UPS_OUTCOME_WAITS = 3;

/** Two upsampler settings are the same (key order aside). */
function sameUpsampler(a: UpsamplerConfig, b: UpsamplerConfig): boolean {
  return stableJson(a) === stableJson(b);
}

function stableJson(v: unknown): string {
  return JSON.stringify(v, (_, x: unknown) =>
    x && typeof x === "object" && !Array.isArray(x)
      ? Object.fromEntries(Object.entries(x as Record<string, unknown>).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)))
      : x,
  );
}

/** A queued upsampler edit, with the results of every edit folded into it. */
interface UpsEdit {
  fn: (cur: UpsamplerConfig) => UpsamplerConfig;
  okText?: string;
  results: { ok: boolean }[];
}

/** An upsampler edit: the fields to change (a preset clears the design, a design the preset). */
export type UpsamplerPatch = Partial<UpsamplerConfig>;
import { keyPatch, osdPayload, volumeOnDac } from "../lib/volumeKeys";
import type { KeyAction, OsdPayload } from "../api/shell";

import { spectrumStore } from "./spectrum";

/** An EQ edit as a function of the EQ it applies to. */
export type EqEdit = (eq: EqProfile | null) => EqProfile | null;

export type ViewId = "eq" | "conv" | "ups" | "out" | "routing" | "ab" | "device" | StageId;

/**
 * The effect stages with a targeted setter each. A stage's view id is its
 * id, so the sidebar item and the view share one name.
 */
export type StageId = "dc_block" | "pitch" | "crossfeed" | "harmonics" | "limiter";

/** What each stage's setter takes, as the chain holds it. */
export interface StageValues {
  dc_block: ChainConfig["dc_block"] | null;
  pitch: ChainConfig["pitch"];
  crossfeed: ChainConfig["crossfeed"] | null;
  harmonics: ChainConfig["harmonics"] | null;
  limiter: ChainConfig["limiter"];
}

export type StageEdit<K extends StageId> = (cur: StageValues[K]) => StageValues[K];

const STAGE_METHOD: Record<StageId, string> = {
  dc_block: "set_dc_block",
  pitch: "set_pitch",
  crossfeed: "set_crossfeed",
  harmonics: "set_harmonics",
  limiter: "set_limiter",
};

export const STAGE_NAME: Record<StageId, string> = {
  dc_block: "DC block",
  pitch: "Pitch",
  crossfeed: "Stereo image",
  harmonics: "Harmonics",
  limiter: "Limiter",
};

function readStage<K extends StageId>(chain: ChainConfig, id: K): StageValues[K] {
  return ((chain as unknown as Record<string, unknown>)[id] ?? null) as StageValues[K];
}

/** Where an inline error is shown: a stage, or a settings group. */
export type ErrorSlot = StageId | "volume" | "eq";

export interface Caps {
  /** null = not known yet. */
  presets: boolean | null;
  devices: boolean | null;
  volume: boolean | null;
  perChannel: boolean | null;
  convChain: boolean | null;
  /** The engine runs fractional and decimating rates (`{rate}` and
   *  `max_rate_any` targets): its status reports `rate_ratio`. */
  rationalRates: boolean | null;
  /** `set_stream_volume`: its streams report `volume_db`, or the call
   *  was refused as unknown (false). */
  streamVolume: boolean | null;
  /** `resolve_design` and `check_design` (filter design v2): false once
   *  the engine refused them as unknown. */
  designs?: boolean | null;
}

export interface Toast {
  id: number;
  text: string;
  tone: "info" | "error";
}

export interface SelfTestState {
  state: "idle" | "running" | "done" | "error";
  result?: SelfTestResult;
  error?: string;
}

export interface VolumeDraft {
  volume_db: number;
  balance_db: number;
  muted: boolean;
}

/** What a volume control shows: the level and the level state it came
 *  from. A gesture begins from the view its control rendered, so a step
 *  is always conditional on the state it was computed from. */
export interface VolumeView {
  level: VolumeDraft;
  /** The status (instance, revision) shown, or the chain whose draft is. */
  readonly from: VolumeBase | VolumeChain;
  readonly epoch: number;
}

/** One volume gesture: a drag from pointer-down to release, a wheel burst,
 *  a key press, a mute or reset click. The handle a control passes back
 *  with each of its changes. */
export interface VolumeGesture {
  readonly id: number;
}

/** A line of volume writes, each conditional on the one before: the
 *  (instance, revision) captured when its first gesture began, then each
 *  accepted echo. A gesture from a draft continues that draft's chain (its
 *  value includes the draft), one from the status starts a new chain. A
 *  refusal kills the chain: its queued and later changes are dropped. */
interface VolumeChain {
  readonly id: number;
  readonly epoch: number;
  base: VolumeBase;
  dead: boolean;
  /** Jobs handed to the queue and not yet run. */
  jobs: number;
  /** Gestures of this chain still held (a drag before its release). */
  held: Set<Gesture>;
}

interface Gesture extends VolumeGesture {
  readonly chain: VolumeChain;
  /** The level shown when it began. */
  readonly start: VolumeDraft;
  /** The fields this gesture edits, at their latest values (a volume
   *  gesture edits the volume only, a mute click the mute only). */
  edits: Partial<VolumeDraft>;
  /** What of `edits` has been handed on to be written. */
  written: Partial<VolumeDraft>;
  /** Released (or a one-shot change made): later changes are ignored. */
  done: boolean;
}

/** A write of only the fields given; the daemon keeps the others at the
 *  chain's acknowledged state (the revision the write is guarded by). */
interface VolumeJob {
  chain: VolumeChain;
  v: Partial<VolumeDraft>;
}

const VOLUME_FIELDS = ["volume_db", "balance_db", "muted"] as const;

function isBase(x: VolumeBase | VolumeChain): x is VolumeBase {
  return "instance" in x;
}

/** The level state a volume write is conditional on (`if_instance`,
 *  `if_revision`). */
interface VolumeBase {
  instance: string;
  revision: number;
}

/** A status volume the app may write from: one with an instance and a
 *  revision. A daemon that predates them gets no volume writes. */
export function volumeWritable(v: VolumeStatus | null | undefined): v is VolumeStatus & VolumeBase {
  return !!v && typeof v.instance === "string" && v.instance.length > 0 && typeof v.revision === "number";
}

export interface AppState {
  transport: Transport["kind"];
  conn: ConnState;
  connDetail: string | null;
  everConnected: boolean;
  config: EngineConfig | null;
  status: Status | null;
  streams: StreamInfo[];
  nowPlaying: NowPlaying | null;
  presets: PresetInfo[];
  devices: DeviceInfo[];
  caps: Caps;
  irInfo: Record<string, IrInfo | null>;
  view: ViewId;
  spectrum: SpectrumPrefs;
  /** The Harmonics page's plot settings (tone, level, ranges, zoom, sweeps). */
  harmGraph: HarmGraphPrefs;
  /** The EQ graph's frequency range. */
  eqRange: EqRange;
  eqMode: EqSection;
  selectedBand: number;
  eqDraft: EqProfile | null;
  /** The EQ shown and edited while it is off (kept here; power on sends it). */
  eqOff: EqProfile | null;
  autoPreamp: boolean;
  canUndo: boolean;
  canRedo: boolean;
  volumeDraft: VolumeDraft | null;
  /** Bumped at every connection change. Status read or pushed under an
   *  older one is dropped, and the sliders are keyed by it, so a drag or
   *  wheel from before a reconnect cannot commit afterwards. */
  connEpoch: number;
  /** The held status volume is current enough to write from: read on this
   *  connection, and not refused as stale since. Until then volume, balance
   *  and mute show no level and send nothing. */
  volumeFresh: boolean;
  /** Curves drawn behind the EQ: targets, preference bounds, measurements. */
  overlays: Overlay[];
  /** Overlays are normalised at this frequency (Hz) before their own offset. */
  overlayRef: number;
  toast: Toast | null;
  /** The quantizer clipped while the limiter was off (cleared when the
   *  limiter comes on or the warning is dismissed). */
  clipNoLimiter: boolean;
  /** The limiter mode picked or last used while it is off (power on restores it). */
  limiterOffMode: ChainConfig["limiter"]["mode"] | null;
  /** The rate target shown and kept while upsampling is off (power on uses it). */
  upsOffTarget: RateTarget | null;
  /** Per stream node: a level or mute on its way, shown until the answer. */
  streamDrafts: Record<number, StreamLevelEdit>;
  /** Per stream node: the engine's refusal, or a note on the level it
   *  played at, shown beside that stream's volume. */
  streamMessages: Record<number, StreamMessage>;
  /** Drafts of stage edits on their way (a slider drag), shown until the echo. */
  stageDrafts: Partial<{ [K in StageId]: StageValues[K] }>;
  /** A varispeed edit on its way (a slider drag), shown until the echo. */
  varispeedDraft: VarispeedConfig | null;
  /** The daemon's refusal of the last `set_varispeed`. */
  varispeedError: string | null;
  /** Per follower device: an edit on its way (a slider drag), shown until the echo. */
  followerDrafts: Record<string, FollowerPatch>;
  /** Per follower device: the daemon's refusal of its last `set_follower`
   *  ("" for an add that did not go through). */
  followerErrors: Record<string, string>;
  /** The main device was asked to move to this follower's device: refused
   *  until the follower is removed (the Device page offers to do that). */
  deviceConflict: string | null;
  /** The daemon's refusal of the last edit, shown next to its controls. */
  stageErrors: Partial<Record<ErrorSlot, string>>;
  /** Per stage: the engine has it (true), lacks it (false), or not known yet (null). */
  stageCaps: Record<StageId | "tilt" | "volumeSettings" | "ditherKinds", boolean | null>;
  selfTest: SelfTestState;
  starting: boolean;
  startError: string | null;
  /** The engine starts at login (null: not known). */
  engineAtLogin: boolean | null;
  engineAtLoginError: string | null;
  /** This app's version (null until known, or in an app that cannot tell). */
  appVersion: string | null;
  /** An upgrade of the app: "ready" relaunches once the user leaves the
   *  window, "failed" could not relaunch (restart by hand). */
  updatePending: AppUpdateState | null;
  /** Running from the AppImage: its per-user setup offer (null otherwise). */
  appImage: AppImageSetup | null;
  /** The AppImage's setup is running. */
  installing: boolean;
  /** The update banner was closed for this engine version ("pending": the
   *  restart notice without a version difference). */
  updateDismissed: string | null;
  /** "Restart" in the update banner is in progress. */
  restarting: boolean;
  /** The daemon's refusal of the last upsampler edit, shown next to the
   *  controls; cleared by the next edit that goes through. */
  upsamplerError: string | null;
  /** An upsampler change the engine is still working on: a custom
   *  design's check, then the `set_upsampler` the engine answers once the
   *  filter is designed (up to tens of seconds for the longest kernels).
   *  `since`: when it started (ms, `Date.now()`); `waiting`: the call
   *  timed out, so the app is asking again for the outcome. */
  upsBuilding: { since: number; waiting: boolean } | null;
  /** Counts finished upsampler builds: a design check that failed while
   *  one ran is made again once it is done. */
  upsBuilds: number;
  /** The last filter preset used, so Custom → Preset returns to it (the
   *  manual design stays in the config while a preset runs). */
  lastUpsPreset: FilterPreset | null;
  /** The last rate target of each kind, so switching kinds and back
   *  returns to it. */
  lastUpsTargets: Partial<Record<RateKind, RateTarget>>;
  /** A custom design that does not play: it failed the check or the
   *  engine refused it. What played before keeps playing until the design
   *  is fixed (each edit is checked and sent) or discarded. */
  upsDraft: UpsamplerConfig | null;
  /** Catalogue expansions read with `resolve_design`, by name: what a
   *  custom design's `based_on` is compared against ("Modified from"). */
  designBases: Record<string, FilterDesign>;
  /** The built-in harmonics presets, once fetched (`[]`: the engine has none). */
  harmonicsPresets: HarmonicsPreset[] | null;
  /** The built-in harmonics system presets (whole stages), once fetched (`[]`: the engine has none). */
  harmonicsSystems: HarmonicsSystem[] | null;
  /** The blind test in the A/B view: running, or its result until closed. */
  blindRun: BlindRun | null;
  /**
   * The A/B view's side-B editor: changes not applied yet, or null when
   * the rows show side B as it plays.
   */
  abCustomSel: CustomSel | null;
  /** Named custom Bs this viewer saved (browser storage, not the engine config). */
  abCustomSaved: SavedCustomB[];
}

/** A blind test (ABX or blind A/B) and, once over, its outcome. */
export interface BlindRun {
  phase: "running" | "done" | "abandoned";
  plan: BlindPlan;
  /** What sides A and B are, as the view names them. */
  sideA: string;
  sideB: string;
  level: LevelMatch;
  secretHex: string;
  commitment: string;
  /** The hidden side of each trial; never shown before the run ends. */
  truths: readonly ("A" | "B")[];
  /** 0-based index of the trial being answered. */
  trial: number;
  /** The button playing in this trial (null: none pressed yet). */
  playing: BlindButton | null;
  trialStart: number;
  switches: number;
  records: TrialRecord[];
  startedAt: string;
  endedAt: string | null;
  /** Why the run ended early (abandoned runs are never scored). */
  reason: string | null;
  /** Chains and A/B config at the start: any change voids the run. */
  fingerprint: string;
  /** Status has shown the mask set; later losing it ends the run. */
  maskSeen: boolean;
}

/** What a config change must leave alone for a blind run to stand. */
function blindFingerprint(cfg: EngineConfig): string {
  return JSON.stringify([cfg.chain, cfg.ab, cfg.dsp_enabled, cfg.device, cfg.rate]);
}

const UI_KEY = "avdsp-ui";

function loadUi(): Partial<Pick<AppState, "view" | "autoPreamp" | "overlayRef">> {
  try {
    const raw = localStorage.getItem(UI_KEY);
    return raw ? (JSON.parse(raw) as Partial<AppState>) : {};
  } catch {
    return {};
  }
}

/** Settings a stage had before it was turned off. Kept in memory too, so
 *  a stage still comes back as it was where storage is unavailable. */
const stashMemory = new Map<string, string>();

/** Forget every remembered setting (tests: each store starts fresh). */
export function clearStash(): void {
  for (const key of stashMemory.keys()) {
    try {
      localStorage.removeItem("avdsp-stash-" + key);
    } catch {
      /* storage unavailable */
    }
  }
  stashMemory.clear();
}

function stash<T>(key: string, value?: T): T | null {
  if (value === undefined) {
    let raw: string | null = null;
    try {
      raw = localStorage.getItem("avdsp-stash-" + key);
    } catch {
      /* storage unavailable */
    }
    raw ??= stashMemory.get(key) ?? null;
    return raw ? (JSON.parse(raw) as T) : null;
  }
  stashMemory.set(key, JSON.stringify(value));
  try {
    localStorage.setItem("avdsp-stash-" + key, JSON.stringify(value));
  } catch {
    /* storage unavailable */
  }
  return null;
}

/** Daemon error → one plain sentence. */
export function errorText(e: unknown): string {
  if (e instanceof DaemonError) {
    if (e.code === ErrorCode.DISCONNECTED) return "The engine is not reachable.";
    if (e.code === ErrorCode.TIMEOUT) return "The engine did not answer in time.";
    const m = e.message.trim();
    return m ? m.charAt(0).toUpperCase() + m.slice(1).replace(/\.?$/, ".") : "The engine refused the change.";
  }
  return e instanceof Error ? e.message : "Something went wrong.";
}

const EQ_INTERVAL_MS = 40; // ~25 Hz while dragging
const VOLUME_INTERVAL_MS = 40;

export class AppStore {
  private state: AppState;
  private listeners = new Set<() => void>();
  private history = new History<EqProfile | null>(100);
  private queue = new MutationQueue();
  private eqThrottle: Throttle<true>;
  /** EQ edits made since the throttle last let them through, composed. */
  private eqPending: EqEdit | null = null;
  private eqDragging = false;
  private volThrottle: Throttle<VolumeJob>;
  /** The chain whose throttled value is pending, if any. */
  private volThrottleChain: VolumeChain | null = null;
  /** The value in the throttle, if any. */
  private volPending: VolumeJob | null = null;
  /** The chain the shown draft belongs to. */
  private volDraftChain: VolumeChain | null = null;
  /** The last gesture begun: what a change without a gesture continues. */
  private volLast: Gesture | null = null;
  private volSeq = 0;
  private toastSeq = 0;
  private toastTimer: ReturnType<typeof setTimeout> | null = null;
  private stop: (() => void) | null = null;
  private spectrumSubscribed: boolean | null = null;
  private overlayChains = new Map<string, Promise<void>>();

  /**
   * `secondary`: a store for the tray popup's own window. It reads and writes
   * the volume like the main window (its own guarded chains) but leaves what
   * belongs to the main window alone: the spectrum subscription, the
   * remembered view, presets, devices and overlays.
   */
  constructor(
    private transport: Transport,
    private opts: { secondary?: boolean } = {},
  ) {
    const ui = loadUi();
    this.state = {
      transport: transport.kind,
      conn: "connecting",
      connDetail: null,
      everConnected: false,
      config: null,
      status: null,
      streams: [],
      nowPlaying: null,
      presets: [],
      devices: [],
      caps: { presets: null, devices: null, volume: null, perChannel: null, convChain: null, rationalRates: null, streamVolume: null },
      irInfo: {},
      view: ui.view ?? "eq",
      spectrum: loadSpectrumPrefs(),
      harmGraph: loadHarmGraph(),
      eqRange: loadEqRange(),
      eqMode: "linked",
      selectedBand: 0,
      eqDraft: null,
      eqOff: stash<EqProfile>("eq") ?? null,
      autoPreamp: ui.autoPreamp ?? true,
      canUndo: false,
      canRedo: false,
      volumeDraft: null,
      connEpoch: 0,
      volumeFresh: false,
      overlays: [],
      overlayRef: Number.isFinite(ui.overlayRef) && (ui.overlayRef ?? 0) >= 10 ? (ui.overlayRef as number) : 1000,
      toast: null,
      clipNoLimiter: false,
      limiterOffMode: stash<ChainConfig["limiter"]["mode"]>("limiter") ?? null,
      upsOffTarget: stash<RateTarget>("ups") ?? null,
      streamDrafts: {},
      streamMessages: {},
      stageDrafts: {},
      varispeedDraft: null,
      varispeedError: null,
      followerDrafts: {},
      followerErrors: {},
      deviceConflict: null,
      stageErrors: {},
      stageCaps: { dc_block: null, pitch: null, crossfeed: null, harmonics: null, limiter: null, tilt: null, volumeSettings: null, ditherKinds: null },
      selfTest: { state: "idle" },
      starting: false,
      startError: null,
      engineAtLogin: null,
      engineAtLoginError: null,
      appVersion: null,
      updatePending: null,
      appImage: null,
      installing: false,
      updateDismissed: null,
      restarting: false,
      upsamplerError: null,
      upsBuilding: null,
      upsBuilds: 0,
      lastUpsPreset: stash<FilterPreset>("ups-preset"),
      lastUpsTargets: stash<Partial<Record<RateKind, RateTarget>>>("ups-targets") ?? {},
      upsDraft: null,
      designBases: {},
      harmonicsPresets: null,
      harmonicsSystems: null,
      blindRun: null,
      abCustomSel: null,
      abCustomSaved: loadSaved(),
    };
    this.eqThrottle = throttleLatest(() => this.flushEq(), EQ_INTERVAL_MS);
    this.volThrottle = throttleLatest((job) => {
      this.volThrottleChain = null;
      this.volPending = null;
      this.sendVolume(job);
    }, VOLUME_INTERVAL_MS);
  }

  // ------------------------------------------------------------ plumbing

  getState = (): AppState => this.state;

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  private set(patch: Partial<AppState>) {
    this.state = { ...this.state, ...patch };
    if (this.opts.secondary) {
      for (const l of this.listeners) l();
      return;
    }
    if ("view" in patch || "autoPreamp" in patch || "overlayRef" in patch) {
      try {
        localStorage.setItem(UI_KEY, JSON.stringify({ view: this.state.view, autoPreamp: this.state.autoPreamp, overlayRef: this.state.overlayRef }));
      } catch {
        /* ignore */
      }
    }
    if ("view" in patch || "spectrum" in patch || "conn" in patch) {
      this.syncSpectrumSubscription();
    }
    for (const l of this.listeners) l();
  }

  private syncSpectrumSubscription() {
    const want = this.state.conn === "up" && this.state.view === "eq" && this.state.spectrum.enabled;
    if (this.spectrumSubscribed === want) return;
    this.spectrumSubscribed = want;
    this.transport.setSpectrumEnabled(want);
    if (!want) spectrumStore.clear();
  }

  toast(text: string, tone: Toast["tone"] = "info") {
    const id = ++this.toastSeq;
    this.set({ toast: { id, text, tone } });
    if (this.toastTimer) clearTimeout(this.toastTimer);
    this.toastTimer = setTimeout(() => {
      if (this.state.toast?.id === id) this.set({ toast: null });
    }, tone === "error" ? 5000 : 2200);
  }

  dismissToast() {
    this.set({ toast: null });
  }

  private call<T>(method: string, params?: unknown): Promise<T> {
    return this.transport.call<T>(method, params);
  }

  /**
   * Queue a mutation; when it runs, re-read config (the daemon's echo is what
   * we show) and then call `onOk`. Resolves once it has run.
   */
  private mutate(fn: () => Promise<unknown>, okText?: string, onOk?: () => void, onError?: (e: unknown) => boolean): Promise<boolean> {
    return this.queue.run(async () => {
      try {
        await fn();
        await this.refreshConfig();
        onOk?.();
        if (okText) this.toast(okText);
        return true;
      } catch (e) {
        if (!onError?.(e)) this.toast(errorText(e), "error");
        await this.refreshConfig().catch(() => undefined);
        return false;
      }
    });
  }

  /** The daemon's config now. Read inside a queued job, right before an edit. */
  private async freshConfig(): Promise<EngineConfig> {
    const config = await this.call<EngineConfig>("get_config");
    this.applyConfig(config);
    return structuredClone(config);
  }

  /** Resolves once no change is queued, running or held by a throttle. */
  async whenIdle(): Promise<void> {
    for (;;) {
      if (this.eqThrottle.pending) this.eqThrottle.flush();
      if (this.volThrottle.pending) this.volThrottle.flush();
      for (const t of this.stageThrottles.values()) if (t.pending) t.flush();
      if (this.varispeedThrottle.pending) this.varispeedThrottle.flush();
      for (const t of this.followerThrottles.values()) if (t.pending) t.flush();
      await this.queue.drained();
      if (this.overlayChains.size > 0) await Promise.all([...this.overlayChains.values()]);
      const stagePending = [...this.stageThrottles.values(), ...this.followerThrottles.values()].some((t) => t.pending);
      if (this.queue.idle && !this.eqThrottle.pending && !this.volThrottle.pending && !this.varispeedThrottle.pending && !stagePending && this.overlayChains.size === 0) return;
    }
  }

  // ------------------------------------------------------------ connection

  connect() {
    if (this.stop) return;
    void this.transport
      .appVersion()
      .then((appVersion) => this.set({ appVersion }))
      .catch(() => undefined);
    void this.transport
      .appImageSetup()
      .then((appImage) => this.set({ appImage }))
      .catch(() => undefined);
    void this.transport
      .appUpdatePending()
      .then((pending) => {
        if (pending) this.markUpdatePending(pending);
      })
      .catch(() => undefined);
    this.stop = this.transport.start({
      onEvent: (e) => this.onEvent(e),
      onConnection: (state, detail) => this.onConnection(state, detail),
    });
  }

  disconnect() {
    this.stop?.();
    this.stop = null;
    this.spectrumSubscribed = null;
    meterStore.clear();
    spectrumStore.clear();
  }

  private onConnection(state: ConnState, detail?: string) {
    if (state !== this.state.conn) this.endVolumeGesture();
    if (state !== "up" && this.state.blindRun?.phase === "running") {
      // The engine may still hold the mask: clear it once connected again.
      this.blindMaskStale = true;
      this.blindAbandon("The connection to the engine was lost (engine restarted or stopped).", false);
    }
    if (state === "up") {
      meterStore.clear();
      spectrumStore.clear();
      this.set({ conn: "up", connDetail: null, everConnected: true, starting: false, startError: null });
      void this.loadAll();
    } else {
      if (state === "down") {
        meterStore.clear();
        spectrumStore.clear();
      }
      this.set({ conn: state, connDetail: detail ?? null });
    }
  }

  private async loadAll() {
    const epoch = this.state.connEpoch;
    const [config, status, streams, nowPlaying] = await Promise.all([
      this.call<EngineConfig>("get_config").catch(() => null),
      this.call<Status>("status").catch(() => null),
      this.call<StreamInfo[]>("list_streams").catch(() => []),
      this.call<NowPlaying | null>("now_playing").catch(() => null),
    ]);
    // Answers from a connection that has since gone are not this daemon's.
    if (epoch !== this.state.connEpoch) return;
    if (config) this.applyConfig(config);
    if (status) this.applyStatus(status, epoch);
    this.setStreams(streams ?? []);
    this.set({ nowPlaying });
    void this.refreshVolume();
    if (this.opts.secondary) return;
    void this.refreshPresets();
    void this.refreshDevices();
    void this.refreshOverlays();
  }

  async refreshConfig() {
    const config = await this.call<EngineConfig>("get_config");
    this.applyConfig(config);
  }

  private async refreshPresets() {
    try {
      const presets = await this.call<PresetInfo[]>("list_presets");
      this.set({ presets, caps: { ...this.state.caps, presets: true } });
    } catch (e) {
      if (isUnknownMethod(e)) this.set({ caps: { ...this.state.caps, presets: false } });
    }
  }

  /** `config.device` the device list was last read for (its `active` flags). */
  private devicesFor: string | null = null;
  private lastCustomDesign: FilterDesign | null = null;
  /** The preset (Recommended included) that played before Custom: Preset returns to it. */
  private presetBeforeCustom: string | null = null;

  async refreshDevices() {
    try {
      this.devicesFor = this.state.config?.device ?? null;
      const devices = await this.call<DeviceInfo[]>("list_devices");
      this.set({ devices, caps: { ...this.state.caps, devices: true } });
    } catch (e) {
      if (isUnknownMethod(e)) this.set({ caps: { ...this.state.caps, devices: false } });
    }
  }

  private onEvent(e: DaemonEvent) {
    switch (e.event) {
      case "meters": {
        const before = meterStore.prevClipped;
        meterStore.push(e.data, typeof performance !== "undefined" ? performance.now() : Date.now());
        // The quantizer's own count rising (not a daemon restart) with the
        // limiter off: offer to turn it on.
        if (!this.state.clipNoLimiter && before !== null && e.data.clipped > before && this.state.config?.chain.limiter?.mode === "off") {
          this.set({ clipNoLimiter: true });
        }
        break;
      }
      case "spectrum":
        spectrumStore.push(e.data, typeof performance !== "undefined" ? performance.now() : Date.now());
        break;
      case "status":
        // Only the live connection's: nothing from before a reconnect.
        if (this.state.conn === "up") this.applyStatus(e.data);
        break;
      case "streams":
        this.setStreams(e.data);
        break;
      case "now_playing":
        this.set({ nowPlaying: e.data });
        break;
      case "config":
        this.applyConfig(e.data);
        break;
      case "presets":
        this.set({ presets: e.data, caps: { ...this.state.caps, presets: true } });
        break;
      case "devices":
        // A sound card came or went (a DAC plugged in or out).
        this.devicesFor = this.state.config?.device ?? null;
        this.set({ devices: e.data });
        break;
    }
  }

  private applyConfig(config: EngineConfig) {
    const caps = { ...this.state.caps };
    // `volume` arrived with split EQ and the IR chain: a config
    // that has it comes from a daemon with all three.
    const phase4a = config.volume !== undefined && config.volume !== null;
    if (phase4a || Array.isArray(config.chain.convolution)) caps.convChain = true;
    if (phase4a || config.chain.eq?.right || config.chain.eq?.left) caps.perChannel = true;
    const patch: Partial<AppState> = { config, caps };
    const run = this.state.blindRun;
    if (run?.phase === "running") {
      if (!config.ab) queueMicrotask(() => this.blindAbandon("A/B was turned off."));
      else if (blindFingerprint(config) !== run.fingerprint) queueMicrotask(() => this.blindAbandon("Side A or side B changed during the run."));
    }
    if (this.state.clipNoLimiter && config.chain.limiter && config.chain.limiter.mode !== "off") patch.clipNoLimiter = false;
    const ups = config.chain.upsampler;
    // Cleared only by the draft itself arriving: a build of an older
    // design that lands while a newer draft waits (failing its check,
    // say) leaves the draft and its errors.
    const draft = this.state.upsDraft?.design;
    if (ups && !ups.preset && draft && ups.design && designKey(ups.design) === designKey(draft)) patch.upsDraft = null;
    this.reconcileUps(ups, patch);
    if (ups && !ups.preset && ups.design) this.lastCustomDesign = ups.design;
    if (ups?.preset && ups.preset !== RECOMMENDED_PRESET && ups.preset !== this.state.lastUpsPreset) {
      patch.lastUpsPreset = ups.preset as FilterPreset;
      stash("ups-preset", ups.preset);
    }
    if (ups && ups.target !== "off") {
      const kind = rateKind(ups.target);
      if (JSON.stringify(this.state.lastUpsTargets[kind]) !== JSON.stringify(ups.target)) {
        patch.lastUpsTargets = { ...this.state.lastUpsTargets, [kind]: ups.target };
        stash("ups-targets", patch.lastUpsTargets);
      }
    }
    const sc = this.stageCapsFromConfig(config);
    if (sc) patch.stageCaps = sc;
    const eq = config.chain.eq;
    if (!this.state.eqDraft) {
      if (!eq && this.state.eqMode !== "linked") patch.eqMode = "linked";
      else if (this.state.eqMode !== "linked" && !hasSection(eq, this.state.eqMode)) patch.eqMode = "linked";
    }
    this.set(patch);
    // `active` in the device list follows the configured device; re-read it when that changes.
    if (this.state.caps.devices && config.device !== this.devicesFor) void this.refreshDevices();
    for (const c of convList(config.chain)) for (const p of irPaths(c)) void this.loadIrInfo(p);
  }

  /**
   * Apply a status read or pushed under connection `epoch`. One rule keeps
   * an old level out: only the live connection's status counts, and its
   * volume replaces the held one unless it is older (the same daemon
   * instance with a lower revision: a late reply or snapshot).
   */
  private applyStatus(raw: Status, epoch = this.state.connEpoch) {
    if (epoch !== this.state.connEpoch) return;
    const rationalRates = hasRateRatio(raw);
    let status = normalizeStatus(raw);
    const held = this.state.status?.volume ?? null;
    const incoming = status.volume ?? null;
    const older =
      volumeWritable(incoming) && volumeWritable(held) && incoming.instance === held.instance && incoming.revision < held.revision;
    const volume = older ? held : incoming;
    const volumeFresh = older ? this.state.volumeFresh : volumeWritable(incoming);
    status = { ...status, volume };
    const prev = this.state.status;
    if (
      status.state === "idle" ||
      status.state === "switching" ||
      status.state === "error" ||
      (prev && prev.graph_rate !== status.graph_rate)
    ) {
      spectrumStore.clear();
    }
    const caps = this.state.caps;
    const hasVolume = volume !== null;
    const capsNow = caps.volume === hasVolume && caps.rationalRates === rationalRates ? caps : { ...caps, volume: hasVolume, rationalRates };
    const patch: Partial<AppState> = { status, volumeFresh, caps: capsNow };
    // `eq_tilt` came with the tilt: a status without the key is from an engine without it.
    const tilt = "eq_tilt" in status;
    if (this.state.stageCaps.tilt !== tilt) patch.stageCaps = { ...this.state.stageCaps, tilt };
    this.set(patch);
    this.blindWatchStatus(status);
  }

  private async loadIrInfo(path: string) {
    if (path in this.state.irInfo) return;
    this.set({ irInfo: { ...this.state.irInfo, [path]: null } });
    const info = await this.transport.irInfo(path);
    this.set({ irInfo: { ...this.state.irInfo, [path]: info } });
  }

  async loadEngineAtLogin() {
    this.set({ engineAtLogin: await this.transport.engineAtLogin() });
  }

  async setEngineAtLogin(on: boolean) {
    this.set({ engineAtLoginError: null });
    try {
      await this.transport.setEngineAtLogin(on);
    } catch (e) {
      this.set({ engineAtLoginError: e instanceof Error ? e.message : String(e) });
    }
    await this.loadEngineAtLogin();
  }

  /** Start the engine; `atLogin` also enables it for every login. */
  async startEngine(atLogin = false) {
    this.set({ starting: true, startError: null });
    if (atLogin) void this.setEngineAtLogin(true);
    try {
      await this.transport.startEngine();
      // The subscription reconnects on its own; give it time before giving up.
      setTimeout(() => {
        if (this.state.conn !== "up" && this.state.starting) {
          void this.transport.engineLastError().then((reason) => {
            if (this.state.conn === "up" || !this.state.starting) return;
            this.set({
              starting: false,
              startError: reason
                ? `The engine did not start: ${reason}`
                : "The engine was started but is not answering yet. `journalctl --user -u audivane-dsp` shows why.",
            });
          });
        }
      }, 8000);
    } catch (e) {
      this.set({ starting: false, startError: errorText(e) });
    }
  }

  /** The AppImage's per-user setup; on success the app relaunches into the
   *  installed copy (with the engine started), so this returns only on failure. */
  async installAppImage(loginStart: boolean) {
    this.set({ installing: true, startError: null });
    try {
      await this.transport.installAppImage(loginStart);
    } catch (e) {
      this.set({ startError: errorText(e) });
    }
    this.set({ installing: false });
  }

  /** Relaunch the app so it runs the newly installed code. */
  async restartApp() {
    this.set({ restarting: true });
    try {
      await this.whenIdle();
      await this.transport.restartApp();
    } catch (e) {
      this.set({ restarting: false });
      this.toast(errorText(e), "error");
    }
  }

  /** Restart the engine service, still on the old version after an upgrade. */
  async restartEngine() {
    this.set({ restarting: true });
    try {
      await this.whenIdle();
      await this.transport.restartEngine();
    } catch (e) {
      this.toast(errorText(e), "error");
    } finally {
      this.set({ restarting: false });
    }
  }

  /** Close the update banner: for this engine version, or ("pending")
   *  the restart notice shown without a version difference. */
  dismissUpdate(key: string | null = this.state.status?.version ?? null) {
    this.set({ updateDismissed: key });
  }

  markUpdatePending(state: AppUpdateState) {
    // A failure is final; "ready" never replaces it.
    if (this.state.updatePending === "failed") return;
    this.set({ updatePending: state });
  }

  // ------------------------------------------------------------ navigation

  setView(view: ViewId) {
    if (view !== "ab" && this.state.blindRun?.phase === "running") this.blindAbandon("You left the A/B view.");
    this.set({ view });
  }

  setSpectrumPrefs(patch: Partial<SpectrumPrefs>) {
    const spectrum = { ...this.state.spectrum, ...patch };
    saveSpectrumPrefs(spectrum);
    this.set({ spectrum });
  }

  setHarmGraph(patch: Partial<HarmGraphPrefs>) {
    const harmGraph = { ...this.state.harmGraph, ...patch };
    saveHarmGraph(harmGraph);
    this.set({ harmGraph });
  }

  setEqRange(eqRange: EqRange) {
    saveEqRange(eqRange);
    this.set({ eqRange });
  }

  // ------------------------------------------------------------ EQ

  /** The EQ as shown: the drag draft, else the daemon's. */
  eq(): EqProfile | null {
    const cfg = this.state.config;
    if (cfg && !cfg.chain.eq && !this.state.eqDraft) return this.state.eqOff ?? emptyEq(cfg.active_preset ?? "");
    return this.state.eqDraft ?? cfg?.chain.eq ?? null;
  }

  /** Whether the EQ is on in the engine's config. */
  eqOn(): boolean {
    return !!this.state.config?.chain.eq;
  }

  private eqPowering = false;

  graphRate(): number {
    return this.state.status?.graph_rate || this.state.config?.rate.default_rate || 48000;
  }

  /** Apply global auto preamp across combined L/R responses when it is on. */
  private withAutoPreamp(eq: EqProfile | null): EqProfile | null {
    if (!eq || !this.state.autoPreamp) return eq;
    const fs = this.graphRate();
    const preamp_db = Math.floor(profileAutoPreampDb(eq, fs) * 10) / 10 || 0;
    return { ...eq, version: EQ_PROFILE_VERSION, preamp_db };
  }

  /** Let the composed pending edits through to the queue. */
  private flushEq() {
    const fn = this.eqPending;
    this.eqPending = null;
    if (!fn) return;
    void this.queue.latest<EqEdit>(
      "eq",
      fn,
      (f) => this.runEq(f),
      (older, newer) => (eq) => newer(older(eq)),
    );
  }

  /** More EQ edits are on their way: leave the draft up. */
  private eqBusy(): boolean {
    return this.eqDragging || this.eqPending !== null || this.queue.waiting("eq") > 0;
  }

  /** Queued job: apply `fn` to the daemon's EQ as it is now. */
  private async runEq(fn: EqEdit) {
    try {
      const cfg = await this.freshConfig();
      const eq = this.withAutoPreamp(fn(cfg.chain.eq ?? null));
      await this.call("set_eq", eq);
      if (this.eqBusy()) return;
      await this.refreshConfig();
      if (this.eqBusy()) return;
      const echoed = this.state.config?.chain.eq ?? null;
      if ((eq?.left && !echoed?.left) || (eq?.right && !echoed?.right)) {
        this.set({ caps: { ...this.state.caps, perChannel: false }, eqMode: "linked" });
        this.toast("This engine version cannot set the channels separately yet.", "error");
      }
      this.setStageError("eq", null);
      this.set({ eqDraft: null });
    } catch (e) {
      this.toast(errorText(e), "error");
      this.setStageError("eq", errorText(e));
      if (this.eqBusy()) return;
      this.set({ eqDraft: null });
      await this.refreshConfig().catch(() => undefined);
    }
  }

  private syncHistory() {
    this.set({ canUndo: this.history.canUndo, canRedo: this.history.canRedo });
  }

  /**
   * One EQ edit. The draft shows it at once (applied to what is shown); the
   * daemon gets it when its turn in the queue comes, applied to the EQ the
   * daemon holds then. `drag` sends at most ~25 times a second.
   */
  editEq(fn: EqEdit, mode: "commit" | "drag" = "commit", record = mode === "commit") {
    if (record) {
      this.history.record(this.eq());
      this.syncHistory();
    }
    const shown = this.withAutoPreamp(fn(this.eq()));
    // Off: edits stay here (and in local storage) until the power button.
    if (!this.eqOn() && !this.eqPowering && !this.state.eqDraft) {
      if (shown) {
        stash("eq", shown);
        this.set({ eqOff: shown });
      }
      return;
    }
    this.set({ eqDraft: shown });
    const prev = this.eqPending;
    this.eqPending = prev ? (eq) => fn(prev(eq)) : fn;
    this.eqThrottle.push(true);
    if (mode === "commit") {
      this.eqDragging = false;
      this.eqThrottle.flush();
    }
  }

  /** Commit a whole EQ (records undo unless `record` is false). */
  commitEq(next: EqProfile | null, record = true) {
    this.editEq(() => next, "commit", record);
  }

  /** Start a drag: one undo step for the whole gesture. */
  beginEqDrag() {
    this.history.record(this.eq());
    this.syncHistory();
    this.eqDragging = true;
  }

  /** Release: the final value always goes out. */
  endEqDrag() {
    if (!this.eqDragging) return;
    this.eqDragging = false;
    if (this.eqThrottle.pending) {
      this.eqThrottle.flush();
      return;
    }
    // Everything was sent while dragging (so no echo cleared the draft): settle now.
    void this.queue.run(async () => {
      if (this.eqBusy() || !this.state.eqDraft) return;
      await this.refreshConfig().catch(() => undefined);
      if (!this.eqBusy()) this.set({ eqDraft: null });
    });
  }

  /** The section currently being edited. */
  editedSet() {
    return sectionSet(this.eq(), this.state.eqMode);
  }

  /** Edit the bands of the selected section. */
  private editBands(fn: (bands: Band[]) => Band[], mode: "commit" | "drag" = "commit") {
    const section = this.state.eqMode;
    const fallback = this.state.config?.active_preset ?? "";
    this.editEq((cur) => {
      const eq = cur ?? emptyEq(fallback);
      return withSectionBands(eq, section, fn(sectionBands(eq, section)));
    }, mode);
  }

  updateBand(i: number, patch: Partial<Band>, mode: "commit" | "drag" = "commit") {
    this.editBands((bands) => bands.map((b, j) => (j === i ? { ...b, ...patch } : b)), mode);
  }

  addBand(band: Partial<Band> = {}) {
    this.editBands((bands) => [...bands, { kind: "peaking" as const, freq_hz: freeBandFreq(bands), gain_db: 0, q: 1, enabled: true, ...band }]);
    this.set({ selectedBand: this.editedSet().bands.length - 1 });
  }

  deleteBand(i: number) {
    this.editBands((bands) => bands.filter((_, j) => j !== i));
    const n = this.editedSet().bands.length;
    const sel = this.state.selectedBand;
    this.set({ selectedBand: sel > i ? sel - 1 : sel === i ? Math.min(i, n - 1) : sel });
  }

  selectBand(i: number) {
    this.set({ selectedBand: i });
  }

  setManualPreamp(db: number) {
    this.set({ autoPreamp: false });
    this.editEq((cur) => ({ ...(cur ?? emptyEq()), version: EQ_PROFILE_VERSION, preamp_db: db }));
  }

  setAutoPreamp(on: boolean) {
    this.set({ autoPreamp: on });
    if (on) this.editEq((eq) => eq, "commit", false);
  }

  setEqSection(section: EqSection) {
    const eq = this.eq();
    if (this.state.caps.perChannel === false && section !== "linked") {
      this.toast("This engine version cannot set the channels separately yet.", "error");
      return;
    }
    if (section !== "linked" && eq && !hasSection(eq, section)) {
      this.addSection(section);
      return;
    }
    const bands = sectionBands(eq, section).length;
    this.set({
      eqMode: section,
      selectedBand: bands > 0 ? Math.min(Math.max(0, this.state.selectedBand), bands - 1) : -1,
    });
  }

  addSection(section: "left" | "right") {
    if (this.state.caps.perChannel === false) {
      this.toast("This engine version cannot set the channels separately yet.", "error");
      return;
    }
    const fallback = this.state.config?.active_preset ?? "";
    this.editEq((cur) => addSection(cur ?? emptyEq(fallback), section));
    this.set({ eqMode: section, selectedBand: -1 });
  }

  toggleSectionEnabled(section: EqSection) {
    const eq = this.eq();
    if (!eq || !hasSection(eq, section)) return;
    this.editEq((cur) => (cur ? setSectionEnabled(cur, section, !sectionEnabled(cur, section)) : cur));
  }

  clearSection(section: EqSection) {
    const eq = this.eq();
    if (!eq || !hasSection(eq, section)) return;
    this.editEq((cur) => (cur ? clearSection(cur, section) : cur));
    if (this.state.eqMode === section) this.set({ selectedBand: -1 });
  }

  removeSection(section: "left" | "right") {
    const eq = this.eq();
    if (!eq || !hasSection(eq, section)) return;
    this.editEq((cur) => (cur ? removeSection(cur, section) : cur));
    if (this.state.eqMode === section) {
      const bands = sectionBands(eq, "linked").length;
      this.set({ eqMode: "linked", selectedBand: bands > 0 ? 0 : -1 });
    }
  }

  undo() {
    const prev = this.history.undo(this.eq());
    if (prev === null && !this.history.canRedo) return;
    this.syncHistory();
    this.commitEq(prev, false);
  }

  redo() {
    if (!this.history.canRedo) return;
    const next = this.history.redo(this.eq());
    this.syncHistory();
    this.commitEq(next, false);
  }

  /** Replace the EQ with a ParametricEQ.txt (the EQ header's Import). */
  importApoToEq(text: string, fileName: string) {
    try {
      const { profile, warnings } = parseApo(text, presetNameFromFile(fileName));
      if (profile.bands.length === 0) {
        this.toast(`No filters found in ${fileName}.`, "error");
        return;
      }
      const keepPreamp = profile.preamp_db !== 0;
      if (keepPreamp) this.set({ autoPreamp: false });
      this.commitEq(profile);
      this.set({ eqMode: "linked", selectedBand: 0, view: "eq" });
      this.toast(`Imported ${profile.bands.length} filters from ${fileName}` + (warnings.length ? ` (${warnings.length} lines skipped)` : ""));
    } catch (e) {
      this.toast(`Could not read ${fileName}. ${errorText(e)}`, "error");
    }
  }

  exportApoText(): string {
    return toApo(this.eq() ?? emptyEq());
  }

  async copyApo() {
    try {
      await this.transport.copyText(this.exportApoText());
      this.toast(`Copied ${this.eq()?.bands.length ?? 0} filters as ParametricEQ.txt`);
    } catch {
      this.toast("Copying is blocked here.", "error");
    }
  }

  // ------------------------------------------------------------ stages

  eqEnabled(): boolean {
    return !!this.state.config?.chain.eq;
  }

  setEqEnabled(on: boolean) {
    const fallback = this.state.config?.active_preset ?? "";
    if (!on) {
      this.editEq((cur) => {
        if (cur) {
          stash("eq", cur);
          this.set({ eqOff: cur });
        }
        return null;
      });
    } else {
      this.eqPowering = true;
      try {
        this.editEq((cur) => cur ?? stash<EqProfile>("eq") ?? emptyEq(fallback));
      } finally {
        this.eqPowering = false;
      }
    }
  }

  private async sendConvolution(list: ConvolutionConfig[]) {
    const caps = this.state.caps;
    try {
      await this.call("set_convolution", list);
    } catch (e) {
      // An engine without the IR chain takes one object or null.
      if (caps.convChain !== true && list.length <= 1 && e instanceof DaemonError && e.code === ErrorCode.INVALID_PARAMS) {
        const single = list[0] ? { enabled: list[0].enabled, ir_path: list[0].ir_path, gain_db: list[0].gain_db } : null;
        await this.call("set_convolution", single);
        this.set({ caps: { ...this.state.caps, convChain: false } });
        return;
      }
      if (caps.convChain !== true && list.length > 1 && e instanceof DaemonError && e.code === ErrorCode.INVALID_PARAMS) {
        throw new DaemonError(e.code, "This engine version supports one impulse response at a time");
      }
      throw e;
    }
  }

  /** Queue an edit of the IR chain, applied to the chain the daemon holds when it
   *  runs. The room calibration's item is not the user's to edit: the daemon keeps
   *  it in place. */
  private editConvolution(fn: (list: ConvolutionConfig[]) => ConvolutionConfig[], okText?: string) {
    return this.mutate(async () => {
      const cfg = await this.freshConfig();
      await this.sendConvolution(fn(convList(cfg.chain).filter((c) => !c.calibration)));
    }, okText);
  }

  /** Pick a room-calibration bundle manifest and load it (replacing the loaded one). */
  async loadCalibration() {
    const path = await this.transport.pickCalibrationBundle();
    if (!path) return;
    await this.mutate(async () => {
      const reply = await this.call<{ label?: string; warnings?: string[] } | null>("bundle_load", { path });
      for (const w of reply?.warnings ?? []) this.toast(w, "info");
    }, "Room calibration loaded");
  }

  unloadCalibration() {
    return this.mutate(() => this.call("bundle_unload"), "Room calibration removed");
  }

  /** One room-calibration part's level, dB: `device` a follower part (`null`: the main one); `gain_db` `null` goes back to the bundle's. */
  setCalibrationGain(device: string | null, gain_db: number | null) {
    return this.mutate(() => this.call("bundle_gain", { ...(device ? { device } : {}), gain_db }));
  }

  /** Switch the loaded room calibration on or off: off, the main speakers play unfiltered and its followers are muted. */
  setCalibrationEnabled(on: boolean) {
    return this.mutate(() => this.call("bundle_enable", { on }), on ? "Room calibration on" : "Room calibration off");
  }

  /** Index of the IR shown at `i` (by `irKey`) in `list`, which may have changed since. */
  private irIndex(list: ConvolutionConfig[], i: number, key: string | undefined): number {
    if (key === undefined) return -1;
    return list[i] && irKey(list[i]) === key ? i : list.findIndex((c) => irKey(c) === key);
  }

  /** The user's impulse responses (the room calibration's item is shown on its own). */
  convolution(): ConvolutionConfig[] {
    return convList(this.state.config?.chain).filter((c) => !c.calibration);
  }

  setConvEnabled(on: boolean) {
    void this.editConvolution((list) => {
      if (!on) {
        stash("conv", list.map((c) => c.enabled));
        return list.map((c) => ({ ...c, enabled: false }));
      }
      const was = stash<boolean[]>("conv");
      return list.map((c, i) => ({ ...c, enabled: was && was.length === list.length && was.some(Boolean) ? !!was[i] : true }));
    });
  }

  async addImpulseResponse() {
    const path = await this.transport.pickImpulseResponse();
    if (!path) return;
    // A new IR follows the stage's power: on only when convolution is on.
    void this.editConvolution((list) => [...list, { enabled: list.length === 0 || list.some((c) => c.enabled), ir_path: path, gain_db: 0, name: null }], "Added an impulse response");
  }

  setIr(i: number, patch: Partial<ConvolutionConfig>) {
    const c = this.convolution()[i];
    const path = c && irKey(c);
    return this.editConvolution((list) => {
      const at = this.irIndex(list, i, path);
      return list.map((c, j) => (j === at ? { ...c, ...patch } : c));
    });
  }

  removeIr(i: number) {
    const c = this.convolution()[i];
    const path = c && irKey(c);
    void this.editConvolution((list) => {
      const at = this.irIndex(list, i, path);
      return list.filter((_, j) => j !== at);
    });
  }

  upsampler(): UpsamplerConfig | null {
    return this.state.config?.chain.upsampler ?? null;
  }

  /**
   * Queue an upsampler edit, applied to the daemon's upsampler when it runs.
   * A refusal is shown next to the controls (`upsamplerError`), not as a toast;
   * the next edit that goes through clears it. Edits made while one waits in
   * the queue (a build is out) are folded into it: only the latest is
   * designed, and every caller gets its outcome.
   */
  private editUpsampler(fn: (cur: UpsamplerConfig) => UpsamplerConfig, okText?: string): Promise<boolean> {
    if (!this.upsampler()) return Promise.resolve(false);
    const result = { ok: false };
    const edit: UpsEdit = { fn, okText, results: [result] };
    return this.queue
      .latest("upsampler", edit, (e) => this.runUpsamplerEdit(e), (older, newer) => ({
        fn: (c) => newer.fn(older.fn(c)),
        okText: newer.okText ?? older.okText,
        results: [...older.results, ...newer.results],
      }))
      .then(() => result.ok);
  }

  /** One (folded) upsampler edit, as `mutate` runs a job. */
  private async runUpsamplerEdit(edit: UpsEdit): Promise<void> {
    let ok = false;
    try {
      const cfg = await this.freshConfig();
      const outcome = await this.sendUpsampler(edit.fn(cfg.chain.upsampler), cfg.chain.upsampler);
      await this.refreshConfig();
      if (this.state.upsamplerError !== null) this.set({ upsamplerError: null });
      if (outcome === "applied") {
        if (edit.okText) this.toast(edit.okText);
        ok = true;
      }
    } catch (e) {
      this.set({ upsamplerError: errorText(e) });
      await this.refreshConfig().catch(() => undefined);
    } finally {
      for (const r of edit.results) r.ok = ok;
    }
  }

  /**
   * `set_upsampler`, shown as building while it is out (the engine designs the
   * filter before it answers). A call that timed out is never sent again (by
   * then it may undo a newer change): the engine answers a connection in
   * order, so a read sent behind it there (`afterUpsamplerBuild`) answers once
   * it is done, and the config then says how it went: the ask plays (applied),
   * something else than before plays (superseded by a later change), or what
   * played before still plays (refused; the reason went with the lost reply).
   * If even that read times out, the request stays unresolved and later config
   * updates settle it (`reconcileUps`).
   */
  private async sendUpsampler(next: UpsamplerConfig, before: UpsamplerConfig): Promise<"applied" | "superseded"> {
    this.upsBusyStart();
    // A newer change settles what an unresolved older one left showing.
    this.upsUnresolved = null;
    try {
      try {
        await this.call("set_upsampler", next);
        return "applied";
      } catch (e) {
        if (!(e instanceof DaemonError && e.code === ErrorCode.TIMEOUT)) throw e;
      }
      for (let wait = 0; wait < UPS_OUTCOME_WAITS; wait++) {
        const building = this.state.upsBuilding;
        if (building && !building.waiting) this.set({ upsBuilding: { ...building, waiting: true } });
        let now: UpsamplerConfig;
        try {
          now = (await this.transport.afterUpsamplerBuild()).chain.upsampler;
        } catch (e) {
          if (e instanceof DaemonError && e.code === ErrorCode.TIMEOUT) continue;
          throw e;
        }
        if (sameUpsampler(now, next)) return "applied";
        if (!sameUpsampler(now, before)) return "superseded";
        throw new DaemonError(ErrorCode.INTERNAL, "The engine did not apply the filter (its reason was lost when the app stopped waiting). Try it again to see why.");
      }
      this.upsUnresolved = { next, before };
      throw new DaemonError(ErrorCode.INTERNAL, "The engine has not finished the filter yet; the one before plays until it does.");
    } finally {
      this.upsBusyEnd();
    }
  }

  /** A change whose outcome the app stopped waiting for: config updates settle it. */
  private upsUnresolved: { next: UpsamplerConfig; before: UpsamplerConfig } | null = null;

  /** Settle [`upsUnresolved`] from a config update: applied or superseded, its error goes. */
  private reconcileUps(ups: UpsamplerConfig | undefined, patch: Partial<AppState>) {
    const open = this.upsUnresolved;
    if (!open || !ups || sameUpsampler(ups, open.before)) return;
    this.upsUnresolved = null;
    patch.upsamplerError = null;
  }

  /** Upsampler applies under way (a custom design's check, then its build). */
  private upsBusy = 0;

  private upsBusyStart() {
    if (this.upsBusy++ === 0) this.set({ upsBuilding: { since: Date.now(), waiting: false } });
  }

  private upsBusyEnd() {
    if (--this.upsBusy === 0) this.set({ upsBuilding: null, upsBuilds: this.state.upsBuilds + 1 });
  }

  setUpsampler(patch: UpsamplerPatch, okText?: string) {
    if (patch.target && patch.target !== "off") stash("ups", patch.target);
    return this.editUpsampler((cur) => patchUpsampler(cur, patch), okText);
  }

  /**
   * The Recommended setting: its rate target and `"recommended"` preset in one
   * `set_upsampler`.
   */
  setUpsamplerRecommended() {
    if (this.state.upsDraft) this.set({ upsDraft: null });
    this.lastCustomDesign = null;
    stash("ups", RECOMMENDED_TARGET);
    // Off stays off: the rate it will turn on with becomes Recommended's.
    if (this.upsampler()?.target === "off") this.setUpsOffTarget(RECOMMENDED_TARGET);
    return this.editUpsampler((cur) => recommendedUpsampler(cur), "Recommended upsampling");
  }

  /** Run a named filter preset (the last custom design is remembered in the UI). */
  setUpsamplerPreset(preset: FilterPreset) {
    const cur = this.upsampler();
    if (cur?.design) this.lastCustomDesign = cur.design;
    return this.setUpsampler({ preset, design: null });
  }

  /**
   * The design a catalogue name (a preset or `"recommended"`) expands to,
   * with `based_on` set (`resolve_design`); kept for "Modified from".
   */
  async resolveDesign(name: string): Promise<FilterDesign> {
    const known = this.state.designBases[name];
    if (known) return known;
    try {
      const design = await this.call<FilterDesign>("resolve_design", { preset: name });
      this.set({ designBases: { ...this.state.designBases, [name]: design }, caps: this.state.caps.designs ? this.state.caps : { ...this.state.caps, designs: true } });
      return design;
    } catch (e) {
      if (isUnknownMethod(e)) this.set({ caps: { ...this.state.caps, designs: false } });
      throw e;
    }
  }

  /**
   * `check_design`: the field errors and per-rate plans of a design, against
   * the current config (or `target`). Never designs a kernel: cheap enough
   * to call as the user types.
   */
  async checkDesign(design: FilterDesign, target?: RateTarget): Promise<DesignCheck> {
    try {
      const res = await this.call<DesignCheck>("check_design", target === undefined ? { design } : { design, target });
      if (this.state.caps.designs !== true) this.set({ caps: { ...this.state.caps, designs: true } });
      return res;
    } catch (e) {
      if (isUnknownMethod(e)) this.set({ caps: { ...this.state.caps, designs: false } });
      throw e;
    }
  }

  /**
   * "Edit as custom": the preset's expansion, with `based_on` set, plays as
   * the custom design (the same sound, now editable).
   */
  async editAsCustom(name: string): Promise<boolean> {
    let design: FilterDesign;
    try {
      design = await this.resolveDesign(name);
    } catch (e) {
      this.set({ upsamplerError: errorText(e) });
      return false;
    }
    return this.editCustomDesign(design);
  }

  /** Seq of the latest custom edit: an older one's check or send does not apply. */
  private designSeq = 0;

  /**
   * One edit of the Custom design, checked first: one that passes is sent
   * at once, one that does not stays a draft (shown with its field errors)
   * until it is fixed or discarded, and what played before keeps playing.
   */
  async editCustomDesign(design: FilterDesign, okText?: string): Promise<boolean> {
    const cur = this.upsampler();
    if (!cur) return false;
    const seq = ++this.designSeq;
    this.set({ upsDraft: { ...cur, preset: null, design }, upsamplerError: null });
    // Shown as building from the check on: an automatic length's check
    // searches it at every rate the device lands on.
    this.upsBusyStart();
    try {
      let check: DesignCheck | null = null;
      try {
        check = await this.checkDesign(design);
      } catch {
        // An engine without check_design: let set_upsampler say.
      }
      if (seq !== this.designSeq) return false;
      if (check && check.errors.length > 0) return false;
      const ok = await this.editUpsampler((c) => ({ ...c, preset: null, design }), okText);
      if (ok && seq === this.designSeq) this.set({ upsDraft: null });
      return ok;
    } finally {
      this.upsBusyEnd();
    }
  }

  /** Drop the draft: the design that plays stays as it is. */
  discardUpsDraft() {
    this.designSeq++;
    if (this.state.upsDraft || this.state.upsamplerError) this.set({ upsDraft: null, upsamplerError: null });
  }

  /** Run the preset a custom design was copied from ("Revert to preset", "Use the preset"). */
  useBasePreset(name: string) {
    this.discardUpsDraft();
    const cur = this.upsampler();
    if (cur?.design) this.lastCustomDesign = cur.design;
    if (name === RECOMMENDED_PRESET) return this.editUpsampler((c) => ({ ...c, preset: RECOMMENDED_PRESET, design: null }));
    return this.setUpsampler({ preset: name, design: null });
  }

  /**
   * Preset or Custom. Custom plays the last custom design, or else a copy of
   * the running preset (or Recommended), as `editCustomDesign` does; Preset runs
   * `preset` (the view's pick: the last preset used when the rate setting
   * allows it), else the last one used.
   */
  async setUpsamplerMode(mode: "preset" | "custom", preset: FilterPreset | null = null): Promise<boolean> {
    const cur = this.upsampler();
    if (!cur) return false;
    // While a change is being checked or built, what plays now is not what
    // will play: the pick is sent (after it) so the last one wins.
    const settled = this.upsBusy === 0 && this.queue.waiting("upsampler") === 0;
    if (mode === "custom") {
      if (settled && !cur.preset && cur.design) return true;
      if (cur.preset || !cur.design) this.presetBeforeCustom = cur.preset ?? RECOMMENDED_PRESET;
      const design = this.lastCustomDesign;
      return this.editCustomDesign(design ?? defaultDesign());
    }
    if (this.state.upsDraft) this.discardUpsDraft();
    // A preset (Recommended included) already plays.
    if (settled && cur.preset) return true;
    if (cur.design) this.lastCustomDesign = cur.design;
    const back = preset ?? this.presetBeforeCustom ?? this.state.lastUpsPreset ?? RECOMMENDED_PRESET;
    return this.setUpsampler({ preset: back, design: null });
  }

  /** The last rate target of a kind used, if any. */
  lastUpsamplerTarget(kind: RateKind): RateTarget | undefined {
    return this.state.lastUpsTargets[kind];
  }

  setUpsamplingEnabled(on: boolean) {
    this.editUpsampler((cur) => {
      if (!on) {
        if (cur.target !== "off") {
          stash("ups", cur.target);
          this.set({ upsOffTarget: cur.target });
        }
        return { ...cur, target: "off" };
      }
      if (cur.target !== "off") return cur;
      return { ...cur, target: targetForEngine(stash<UpsamplerConfig["target"]>("ups") ?? { max_rate: 768000 }, this.state.caps.rationalRates !== false) };
    });
  }

  /** A rate picked while upsampling is off: kept for the power button, not sent. */
  setUpsOffTarget(target: RateTarget) {
    if (target === "off") return;
    stash("ups", target);
    this.set({ upsOffTarget: target });
  }

  upsamplerResponse(config: UpsamplerConfig | null, sourceRate: number | null, window?: ResponseWindowRequest): Promise<UpsamplerResponse> {
    return this.transport.upsamplerResponse(config, sourceRate, window).then(normalizeUpsamplerResponse);
  }

  /**
   * An edit of one config field with its targeted setter (`set_output`,
   * `set_formats`, `set_rate`, `set_device`), applied to the field the
   * daemon holds when the job runs. An engine that predates the setter
   * gets the same edit through `set_config`.
   */
  private editField<T>(method: string, read: (c: EngineConfig) => T, fn: (cur: T) => T, write: (c: EngineConfig, v: T) => EngineConfig, okText?: string) {
    if (!this.state.config) return Promise.resolve(false);
    return this.mutate(async () => {
      const cur = await this.freshConfig();
      const next = fn(read(cur));
      try {
        await this.call(method, next);
      } catch (e) {
        if (!isUnknownMethod(e)) throw e;
        await this.call("set_config", write(cur, next));
      }
    }, okText);
  }

  setOutput(patch: Partial<OutputConfig>) {
    void this.editField(
      "set_output",
      (c) => c.chain.output,
      (o) => ({ ...o, ...patch }),
      (c, output) => ({ ...c, chain: { ...c.chain, output } }),
    );
  }

  setPreferredFormat(f: SampleFormat) {
    void this.editField(
      "set_formats",
      (c) => c.formats,
      (formats) => [f, ...formats.filter((x) => x !== f)],
      (c, formats) => ({ ...c, formats }),
    );
  }

  private editRate(fn: (r: EngineConfig["rate"]) => EngineConfig["rate"], okText?: string) {
    return this.editField("set_rate", (c) => c.rate, fn, (c, rate) => ({ ...c, rate }), okText);
  }

  /** Allow or block a graph rate; a blocked default moves to the nearest allowed rate. */
  toggleAllowedRate(rate: number) {
    const cur = this.state.config?.rate;
    if (!cur || !toggleRate(cur, rate)) return;
    const on = !cur.allowed_rates.includes(rate);
    void this.editRate((r) => toggleRate(r, rate) ?? r, `${fmtRate(rate)} ${on ? "allowed" : "no longer used"}`);
  }

  /** Make an allowed graph rate the default (idle, and rate following off). */
  setDefaultRate(rate: number) {
    void this.editRate((r) => withDefaultRate(r, rate), `Default rate: ${fmtRate(rate)}`);
  }

  /** What the engine does when apps play into the DSP sink at different rates. */
  /** What the music does while the output device is unplugged. */
  setDeviceLost(device_lost: DeviceLostAction) {
    if (!this.state.config) return;
    void this.mutate(async () => {
      const cur = await this.freshConfig();
      await this.call("set_config", { ...cur, device_lost });
    });
  }

  /** The convolution guards (`convolution_safety`): boost cap and subsonic limit. */
  setConvolutionSafety(patch: Partial<ConvolutionSafety>) {
    if (!this.state.config) return Promise.resolve(false);
    return this.mutate(async () => {
      const cur = await this.freshConfig();
      await this.call("set_convolution_safety", { ...convSafety(cur), ...patch });
    });
  }

  setRateConflict(conflict: RateConflict) {
    void this.editRate((r) => ({ ...r, conflict }));
  }

  /** Who converts the rate for Direct and DSP off when the DAC lacks it. */
  setBypassResampler(bypass_resampler: BypassResampler) {
    void this.editRate((r) => ({ ...r, bypass_resampler }));
  }

  // ------------------------------------------------------------ effect stages

  /**
   * What a config tells about the stages: a field the engine sent is a
   * stage it has. The optional ones (DC block, crossfeed, harmonics) are
   * left out while off, so their absence says nothing.
   */
  private stageCapsFromConfig(config: EngineConfig): AppState["stageCaps"] | null {
    const cur = this.state.stageCaps;
    const next = { ...cur };
    const chain = config.chain as unknown as Record<string, unknown>;
    for (const id of Object.keys(STAGE_METHOD) as StageId[]) {
      if (chain[id] !== undefined && chain[id] !== null) next[id] = true;
    }
    // Pitch and the limiter are always in a config from an engine that has
    // them; without the field the engine predates the stage.
    if (chain.pitch === undefined) next.pitch = false;
    if (chain.limiter === undefined) next.limiter = false;
    // The noise-shaping dither kinds came with the limiter (both after 0.9.4).
    next.ditherKinds = chain.limiter !== undefined;
    const vol = config.volume as Partial<VolumeConfig> | null | undefined;
    // The settings came after the level: a volume without `guard` cannot take them.
    next.volumeSettings = !!vol && !!vol.guard && typeof vol.guard.mode === "string";
    return deepEqual(next, cur) ? null : next;
  }

  /** A stage's settings as shown: the draft of an edit on its way, else the daemon's. */
  stage<K extends StageId>(id: K): StageValues[K] | null {
    const draft = this.state.stageDrafts[id];
    if (draft !== undefined) return draft as StageValues[K];
    const chain = this.state.config?.chain;
    return chain ? readStage(chain, id) : null;
  }

  private stageThrottles = new Map<StageId, Throttle<true>>();
  private stagePending = new Map<StageId, StageEdit<StageId>>();
  private stageDragging = new Set<StageId>();

  private stageBusy(id: StageId): boolean {
    return this.stageDragging.has(id) || this.stagePending.has(id) || this.queue.waiting(`stage:${id}`) > 0;
  }

  private setStageError(slot: ErrorSlot, text: string | null) {
    const cur = this.state.stageErrors;
    if ((cur[slot] ?? null) === text) return;
    const next = { ...cur };
    if (text === null) delete next[slot];
    else next[slot] = text;
    this.set({ stageErrors: next });
  }

  private setStageDraft<K extends StageId>(id: K, value: StageValues[K] | undefined) {
    const next = { ...this.state.stageDrafts } as Record<string, unknown>;
    if (value === undefined) delete next[id];
    else next[id] = value;
    this.set({ stageDrafts: next as AppState["stageDrafts"] });
  }

  private flushStage(id: StageId) {
    const fn = this.stagePending.get(id);
    this.stagePending.delete(id);
    if (!fn) return;
    void this.queue.latest<StageEdit<StageId>>(
      `stage:${id}`,
      fn,
      (f) => this.runStage(id, f),
      (older, newer) => (v) => newer(older(v)),
    );
  }

  /** Queued job: apply `fn` to the stage as the daemon holds it now. */
  private async runStage(id: StageId, fn: StageEdit<StageId>) {
    try {
      const cfg = await this.freshConfig();
      const next = fn(readStage(cfg.chain, id));
      await this.call(STAGE_METHOD[id], next);
      if (this.state.stageCaps[id] !== true) this.set({ stageCaps: { ...this.state.stageCaps, [id]: true } });
      if (this.stageBusy(id)) return;
      await this.refreshConfig();
      if (this.stageBusy(id)) return;
      this.setStageDraft(id, undefined);
      this.setStageError(id, null);
    } catch (e) {
      if (isUnknownMethod(e)) {
        this.set({ stageCaps: { ...this.state.stageCaps, [id]: false } });
        this.setStageError(id, `This engine version has no ${STAGE_NAME[id].toLowerCase()}. Update the engine to use it.`);
      } else {
        this.setStageError(id, errorText(e));
      }
      if (this.stageBusy(id)) return;
      this.setStageDraft(id, undefined);
      await this.refreshConfig().catch(() => undefined);
    }
  }

  /**
   * One edit of a stage, sent with its targeted call (`set_pitch`, …). The
   * draft shows it at once; the daemon gets it when its turn comes, applied
   * to the stage the daemon holds then. `drag` sends at most ~25 times a
   * second and keeps the draft up; `commit` sends now and ends the drag.
   */
  editStage<K extends StageId>(id: K, fn: StageEdit<K>, mode: "commit" | "drag" = "commit") {
    if (!this.state.config) return;
    const shown = this.stage(id);
    this.setStageDraft(id, fn(shown as StageValues[K]));
    const prev = this.stagePending.get(id) as StageEdit<K> | undefined;
    this.stagePending.set(id, (prev ? (v: StageValues[K]) => fn(prev(v)) : fn) as unknown as StageEdit<StageId>);
    let t = this.stageThrottles.get(id);
    if (!t) {
      t = throttleLatest(() => this.flushStage(id), EQ_INTERVAL_MS);
      this.stageThrottles.set(id, t);
    }
    if (mode === "drag") this.stageDragging.add(id);
    else this.stageDragging.delete(id);
    t.push(true);
    if (mode === "commit") t.flush();
  }

  /**
   * End a stage drag or wheel burst: flush any throttled edit that is still
   * pending and settle the draft once the daemon answers, without replaying
   * the edit. If another command already committed on this stage, do nothing.
   */
  endStageDrag(id: StageId) {
    if (!this.stageDragging.delete(id)) return;
    const t = this.stageThrottles.get(id);
    if (t?.pending) {
      t.flush();
      return;
    }
    // Everything was sent while dragging (so no echo cleared the draft): settle now.
    void this.queue.run(async () => {
      if (this.stageBusy(id) || !(id in this.state.stageDrafts)) return;
      await this.refreshConfig().catch(() => undefined);
      if (!this.stageBusy(id)) this.setStageDraft(id, undefined);
    });
  }

  /** Set a stage's settings outright (a preset button, a reset). */
  setStage<K extends StageId>(id: K, value: StageValues[K]) {
    this.editStage(id, () => value);
  }

  /** Turn a stage on or off, keeping its settings for the next time it is on. */
  setStageEnabled(id: StageId, on: boolean) {
    switch (id) {
      case "dc_block":
        this.editStage("dc_block", (c) => ({ enabled: on, cutoff_hz: c?.cutoff_hz ?? 5 }));
        break;
      case "pitch":
        this.setPitchPageEnabled(on);
        break;
      case "crossfeed":
        this.editStage("crossfeed", (c) => ({ ...(c ?? defaultCrossfeed()), enabled: on }));
        break;
      case "harmonics":
        this.editStage("harmonics", (c) => ({ ...(c ?? defaultHarmonics()), enabled: on }));
        break;
      case "limiter":
        if (this.state.stageCaps.limiter === false) return;
        this.editStage("limiter", (c) => {
          if (!c) return c;
          if (!on) {
            if (c.mode !== "off") {
              stash("limiter", c.mode);
              const mode = c.mode;
              queueMicrotask(() => this.set({ limiterOffMode: mode }));
            }
            return { ...c, mode: "off" };
          }
          if (c.mode !== "off") return c;
          return { ...c, mode: stash<ChainConfig["limiter"]["mode"]>("limiter") ?? "auto_headroom" };
        });
        break;
    }
  }

  /**
   * The Pitch page's one switch: pitch shift and varispeed on or off
   * together (two targeted calls, only for the ones that change). Their
   * settings are kept; at 0 cents and 1× both are no-ops.
   */
  setPitchPageEnabled(on: boolean) {
    if (!this.state.config) return;
    if (this.stage("pitch")?.enabled !== on) this.editStage("pitch", (c) => ({ ...(c ?? { cents: 0, quality: "high" as const }), enabled: on }));
    if (this.varispeed().enabled !== on) this.editVarispeed({ enabled: on });
  }

  /** The Pitch page shows as on when either pitch shift or varispeed is on. */
  pitchPageOn(): boolean {
    return !!this.stage("pitch")?.enabled || this.varispeed().enabled;
  }

  /** The clip warning's one click: the limiter on in auto headroom. */
  turnOnLimiter() {
    this.set({ clipNoLimiter: false });
    this.editStage("limiter", (c) => ({ ...(c ?? defaultLimiter()), mode: "auto_headroom" }));
  }

  dismissClipWarning() {
    this.set({ clipNoLimiter: false });
  }

  /**
   * The limiter's mode, from its view. Off goes through the same path as the
   * power button, so the mode it had comes back with either control.
   */
  setLimiterMode(mode: ChainConfig["limiter"]["mode"]) {
    // While off, a mode is only remembered for the power button.
    if (this.stage("limiter")?.mode === "off" && mode !== "off") {
      stash("limiter", mode);
      this.set({ limiterOffMode: mode });
      return;
    }
    if (mode === "off") this.setStageEnabled("limiter", false);
    else this.editStage("limiter", (c) => (c ? { ...c, mode } : c));
  }

  /**
   * Turn one Chebyshev order of harmonics layer `layer` on or off (index 0
   * is H2). Off remembers its level, so it comes back where it was.
   */
  setHarmonicOrder(layer: number, order: number, on: boolean) {
    this.editStage("harmonics", (c) => {
      const base = c ?? { ...defaultHarmonics(), enabled: false };
      const l = base.layers[layer];
      if (l?.kind !== "chebyshev") return base;
      const cur = l.levels_db[order] ?? null;
      const key = `harmonics-${layer}-${order}`;
      if (!on) {
        if (cur !== null) stash(key, cur);
        return withOrderLevel(base, layer, order, null);
      }
      return withOrderLevel(base, layer, order, cur ?? stash<number>(key) ?? -70);
    });
  }

  /** The built-in harmonics presets (`harmonics_presets`), fetched once; [] from an engine without them. */
  async loadHarmonicsPresets(): Promise<HarmonicsPreset[]> {
    if (this.state.harmonicsPresets) return this.state.harmonicsPresets;
    try {
      const list = await this.call<HarmonicsPreset[]>("harmonics_presets");
      this.set({ harmonicsPresets: list });
      return list;
    } catch (e) {
      if (isUnknownMethod(e)) {
        this.set({ harmonicsPresets: [] });
        return [];
      }
      throw e;
    }
  }

  /** The built-in harmonics system presets (`harmonics_systems`), fetched once; [] from an engine without them. */
  async loadHarmonicsSystems(): Promise<HarmonicsSystem[]> {
    if (this.state.harmonicsSystems) return this.state.harmonicsSystems;
    try {
      const list = await this.call<HarmonicsSystem[]>("harmonics_systems");
      this.set({ harmonicsSystems: list });
      return list;
    } catch (e) {
      if (isUnknownMethod(e)) {
        this.set({ harmonicsSystems: [] });
        return [];
      }
      throw e;
    }
  }

  /**
   * `harmonics_readout` of `config` with the sweeps (the level sweep at
   * `sweepFreq` Hz, the sweep against frequency at `sweepLevel` dBFS; each
   * omitted: the engine's fixed tones): computed by the engine, nothing changes.
   */
  harmonicsReadout(config: HarmonicsConfig, sweepFreq?: number, sweepLevel?: number): Promise<HarmonicsReadout> {
    return this.call<HarmonicsReadout>("harmonics_readout", { config, sweep: true, sweep_freq_hz: sweepFreq, sweep_level_dbfs: sweepLevel });
  }

  /**
   * `harmonics_live`: the latest frame of what chain A's harmonics stage
   * does to the music playing (cheap; each call keeps the engine's
   * analysis on for a few seconds).
   */
  harmonicsLive(): Promise<HarmonicsLive> {
    return this.call<HarmonicsLive>("harmonics_live");
  }

  private spectrumQueue: Promise<unknown> = Promise.resolve();

  /**
   * `harmonics_spectrum` of `config` for one sine, one line per trace. Calls
   * run one at a time (the engine answers a call that waits behind another
   * `SUPERSEDED`); a queued call whose `current()` has turned false is
   * dropped unsent and rejects with `"stale"`.
   */
  harmonicsSpectrum(config: HarmonicsConfig, freqHz: number, levelDbfs: number, traces: HarmonicsSpectrumTrace[], current: () => boolean = () => true): Promise<HarmonicsSpectrum> {
    const run = () => (current() ? this.call<HarmonicsSpectrum>("harmonics_spectrum", { config, freq_hz: freqHz, level_dbfs: levelDbfs, traces }) : Promise.reject(new Error("stale")));
    const p = this.spectrumQueue.then(run, run);
    this.spectrumQueue = p.catch(() => undefined);
    return p;
  }

  private convResponseQueue: Promise<unknown> = Promise.resolve();

  /**
   * `conv_response`: the convolution's frequency response of chain A (or
   * A/B side B, or a follower's own chain). One call at a time, as
   * `harmonicsSpectrum`; a queued call whose `current()` has turned false
   * is dropped unsent and rejects with `"stale"`.
   */
  convResponse(side: AbSide | null, device: string | null, current: () => boolean = () => true): Promise<ConvResponse> {
    const params: { side?: AbSide; device?: string } = {};
    if (side) params.side = side;
    if (device) params.device = device;
    const run = () => (current() ? this.call<ConvResponse>("conv_response", params) : Promise.reject(new Error("stale")));
    const p = this.convResponseQueue.then(run, run);
    this.convResponseQueue = p.catch(() => undefined);
    return p;
  }

  // ------------------------------------------------------------ varispeed

  /** Varispeed as shown: the draft while an edit is on its way, else the config. */
  varispeed(): VarispeedConfig {
    return this.state.varispeedDraft ?? this.state.config?.varispeed ?? DEFAULT_VARISPEED;
  }

  private varispeedPending: SetVarispeedParams | null = null;
  private varispeedDragging = false;
  private varispeedThrottle: Throttle<true> = throttleLatest(() => this.flushVarispeed(), EQ_INTERVAL_MS);

  private varispeedBusy(): boolean {
    return this.varispeedDragging || this.varispeedPending !== null || this.queue.waiting("varispeed") > 0;
  }

  /**
   * One live varispeed edit (`set_varispeed`, only the fields given). `drag`
   * sends at most ~25 times a second, like the stage sliders; `commit`
   * sends now and ends the drag.
   */
  editVarispeed(patch: SetVarispeedParams, mode: "commit" | "drag" = "commit") {
    if (!this.state.config) return;
    this.set({ varispeedDraft: { ...this.varispeed(), ...patch } });
    this.varispeedPending = { ...(this.varispeedPending ?? {}), ...patch };
    this.varispeedDragging = mode === "drag";
    this.varispeedThrottle.push(true);
    if (mode === "commit") this.varispeedThrottle.flush();
  }

  /** End a varispeed drag or wheel burst (see `endStageDrag`). */
  endVarispeedDrag() {
    if (!this.varispeedDragging) return;
    this.varispeedDragging = false;
    if (this.varispeedThrottle.pending) {
      this.varispeedThrottle.flush();
      return;
    }
    void this.queue.run(async () => {
      if (this.varispeedBusy() || !this.state.varispeedDraft) return;
      await this.refreshConfig().catch(() => undefined);
      if (!this.varispeedBusy()) this.set({ varispeedDraft: null });
    });
  }

  private flushVarispeed() {
    const patch = this.varispeedPending;
    this.varispeedPending = null;
    if (!patch) return;
    void this.queue.latest<SetVarispeedParams>(
      "varispeed",
      patch,
      (p) => this.runVarispeed(p),
      (older, newer) => ({ ...older, ...newer }),
    );
  }

  private async runVarispeed(patch: SetVarispeedParams) {
    const epoch = this.state.connEpoch;
    try {
      const echo = await this.call<VarispeedStatus>("set_varispeed", patch);
      const status = this.state.status;
      if (status && epoch === this.state.connEpoch) this.set({ status: { ...status, varispeed: echo } });
      if (this.state.varispeedError !== null) this.set({ varispeedError: null });
      if (this.varispeedBusy()) return;
      await this.refreshConfig();
      if (!this.varispeedBusy()) this.set({ varispeedDraft: null });
    } catch (e) {
      this.set({ varispeedError: isUnknownMethod(e) ? "This engine version has no varispeed. Update the engine to use it." : errorText(e) });
      if (this.varispeedBusy()) return;
      this.set({ varispeedDraft: null });
      await this.refreshConfig().catch(() => undefined);
    }
  }

  clearVarispeedError() {
    this.set({ varispeedError: null });
  }

  /** Clear an inline error (the person dismissed it). */
  clearStageError(slot: ErrorSlot) {
    this.setStageError(slot, null);
  }

  /**
   * The guard's rise limit on or off. Off sends `max_rise_db_per_s` 0 (only
   * the caps apply); on restores the last non-zero rate (else 6 dB/s).
   */
  setGuardRiseLimit(limited: boolean) {
    return this.setVolumeSettings((v) => {
      // The rate it had, on the engine's fresh config, to come back to.
      if (!limited && v.guard.max_rise_db_per_s > 0) stash("vol-rise", v.guard.max_rise_db_per_s);
      return { ...v, guard: guardRiseLimit(v.guard, limited, stash<number>("vol-rise")) };
    });
  }

  /** The guard's max rise typed in: 0 is the same as "No rise limit" (the old rate is kept to come back to). */
  setGuardRiseRate(dbPerS: number) {
    if (dbPerS === 0) return this.setGuardRiseLimit(false);
    return this.setVolumeSettings((v) => ({ ...v, guard: { ...v.guard, max_rise_db_per_s: dbPerS } }));
  }

  /**
   * Volume settings (mode, hardware level, ceiling, sink policy, guard) have
   * no targeted call: `get_config` + `set_config`, like `avdsp ctl volume`.
   * The level, balance and mute are never written here (they belong to
   * `set_volume_guarded`).
   */
  setVolumeSettings(fn: (v: VolumeConfig) => VolumeConfig) {
    if (!this.state.config?.volume || this.state.stageCaps.volumeSettings === false) return Promise.resolve(false);
    return this.queue.run(async () => {
      try {
        const cur = await this.freshConfig();
        const { volume_db, balance_db, muted, ...settings } = fn(cur.volume);
        void volume_db;
        void balance_db;
        void muted;
        const problem = volumeSettingsProblem(settings);
        if (problem) {
          this.setStageError("volume", problem.text);
          return false;
        }
        // No level keys at all: the level belongs to `set_volume_guarded`
        // (and the daemon keeps its live one for a config without them).
        const { volume: _old, ...rest } = cur;
        void _old;
        await this.call("set_config", { ...rest, volume: settings });
        await this.refreshConfig();
        const epoch = this.state.connEpoch;
        const status = await this.call<Status>("status").catch(() => null);
        if (status) this.applyStatus(status, epoch);
        this.setStageError("volume", null);
        return true;
      } catch (e) {
        this.setStageError("volume", errorText(e));
        await this.refreshConfig().catch(() => undefined);
        return false;
      }
    });
  }

  // ------------------------------------------------------------ bypass / volume

  bypassed(): boolean {
    return this.state.config ? !this.state.config.dsp_enabled : false;
  }

  setBypass(on: boolean) {
    void this.mutate(() => this.call("set_dsp_enabled", { enabled: !on }), on ? "Bypass on: audio goes to the DAC untouched" : undefined);
  }

  volume(): VolumeDraft | null {
    return this.volumeView()?.level ?? null;
  }

  /** The level a volume control shows and the state it came from; null
   *  until a current status with an instance and a revision (never the
   *  last daemon's level, nor a stale one). */
  volumeView(): VolumeView | null {
    const v = this.state.status?.volume;
    if (!this.state.volumeFresh || !volumeWritable(v)) return null;
    const epoch = this.state.connEpoch;
    const chain = this.volDraftChain;
    if (this.state.volumeDraft && chain && !chain.dead && chain.epoch === epoch) {
      return { level: this.state.volumeDraft, from: chain, epoch };
    }
    return {
      level: { volume_db: v.volume_db, balance_db: v.balance_db, muted: v.muted },
      from: { instance: v.instance, revision: v.revision },
      epoch,
    };
  }

  /** Begin a gesture from what a control rendered (`view`), captured when
   *  the gesture starts (pointer-down, the first wheel step, a key or a
   *  click), even if its value never changes. Null when there is nothing
   *  to write from. */
  beginVolumeGesture(view: VolumeView | null = this.volumeView()): VolumeGesture | null {
    if (!view || view.epoch !== this.state.connEpoch) return null;
    let chain: VolumeChain;
    if (isBase(view.from)) {
      chain = { id: ++this.volSeq, epoch: view.epoch, base: { ...view.from }, dead: false, jobs: 0, held: new Set() };
    } else {
      if (view.from.dead) return null;
      chain = view.from;
    }
    const g: Gesture = { id: ++this.volSeq, chain, start: { ...view.level }, edits: {}, written: {}, done: false };
    this.volLast = g;
    return g;
  }

  /** A connection change ends every volume gesture; the sliders remount on
   *  the new epoch, so the old pointer is gone. */
  private endVolumeGesture() {
    this.volThrottle.cancel();
    this.volThrottleChain = null;
    this.volPending = null;
    this.volDraftChain = null;
    this.volLast = null;
    this.set({ volumeDraft: null, connEpoch: this.state.connEpoch + 1, volumeFresh: false });
  }

  /** True while `chain` has a change on its way or a gesture held. */
  private chainBusy(chain: VolumeChain) {
    return chain.jobs > 0 || chain.held.size > 0 || this.volThrottleChain === chain;
  }

  /** Once its chain is idle, the draft gives way to the reported level. */
  private settleVolume(chain: VolumeChain) {
    if (this.volDraftChain === chain && !this.chainBusy(chain)) {
      this.volDraftChain = null;
      this.set({ volumeDraft: null });
    }
  }

  /** A refusal: drop this chain (its pending and queued changes, its
   *  draft), and read the level again before anything is sent. Other
   *  chains, and gestures begun later, are not touched. */
  private refuseChain(chain: VolumeChain) {
    if (chain.dead) return;
    chain.dead = true;
    if (this.volThrottleChain === chain) {
      this.volThrottle.cancel();
      this.volThrottleChain = null;
      this.volPending = null;
    }
    if (this.volDraftChain === chain) {
      this.volDraftChain = null;
      this.set({ volumeDraft: null });
    }
    this.set({ volumeFresh: false });
    void this.refreshVolume();
  }

  /** Hand a chain's value to the queue (one waiting job per chain at most;
   *  it carries the newest value). */
  private sendVolume(job: VolumeJob) {
    const { chain } = job;
    chain.jobs++;
    void this.queue
      .latest(
        `volume:${chain.id}`,
        job,
        (x) => this.runVolume(x),
        (older, newer) => ({ chain, v: { ...older.v, ...newer.v } }),
      )
      .then(() => {
        chain.jobs--;
        this.settleVolume(chain);
      });
  }

  private async runVolume({ chain, v }: VolumeJob) {
    const stale = () => chain.epoch !== this.state.connEpoch;
    if (stale() || chain.dead) return;
    const base = chain.base;
    try {
      // Compare-and-set on the chain's own base: the daemon applies it
      // only at this instance and revision, so no old level can land.
      const echo = await this.call<VolumeStatus>("set_volume_guarded", { ...v, if_instance: base.instance, if_revision: base.revision });
      if (stale()) return;
      if (volumeWritable(echo)) chain.base = { instance: echo.instance, revision: echo.revision };
      const status = this.state.status;
      if (status) this.applyStatus({ ...status, volume: echo }, chain.epoch);
    } catch (e) {
      if (stale()) return;
      // The level changed under the chain (another daemon, another
      // writer), or the daemon predates guarded writes (it would not
      // check the condition): never fall back to an unconditional write.
      if (e instanceof DaemonError && (e.code === ErrorCode.STALE_INSTANCE || e.code === ErrorCode.STALE_REVISION || isUnknownMethod(e))) {
        this.refuseChain(chain);
        return;
      }
      this.toast(errorText(e), "error");
    }
  }

  private volumeRefreshing = false;

  /** While connected and the held volume is not current, read the status
   *  again with a bounded backoff, so the controls never stay disabled
   *  while the daemon answers. Stops for a daemon without revisions. */
  private async refreshVolume() {
    if (this.volumeRefreshing) return;
    this.volumeRefreshing = true;
    try {
      let delay = 50;
      while (this.state.conn === "up" && !this.state.volumeFresh) {
        const epoch = this.state.connEpoch;
        try {
          const status = await this.call<Status>("status");
          this.applyStatus(status, epoch);
          if (epoch === this.state.connEpoch && !volumeWritable(status.volume)) break;
        } catch {
          /* retried below */
        }
        if (this.state.volumeFresh) break;
        await new Promise((r) => setTimeout(r, delay));
        delay = Math.min(delay * 2, 1000);
      }
    } finally {
      this.volumeRefreshing = false;
    }
  }

  /**
   * Volume/balance/mute. `drag` keeps a local draft and throttles; `commit`
   * sends now and ends the gesture. `gesture` is the one the control began
   * (`beginVolumeGesture`); without one the last gesture continues, or one
   * begins from the current view. False when the change was dropped (its chain
   * was refused, or there is nothing to write from).
   */
  setVolume(patch: Partial<VolumeDraft>, mode: "drag" | "commit" = "commit", gesture?: VolumeGesture | null): boolean {
    let g = gesture as Gesture | null | undefined;
    if (g === undefined) {
      const last = this.volLast;
      g = last && !last.done && last.chain.epoch === this.state.connEpoch ? last : (this.beginVolumeGesture() as Gesture | null);
    }
    if (!g) return false;
    const chain = g.chain;
    if (g.done || chain.dead || chain.epoch !== this.state.connEpoch) {
      // The rest of a dropped gesture is ignored; its release ends it,
      // and only it.
      if (mode === "commit") {
        g.done = true;
        chain.held.delete(g);
      }
      return false;
    }
    g.edits = { ...g.edits, ...patch };
    if (mode === "drag") chain.held.add(g);
    else {
      g.done = true;
      chain.held.delete(g);
    }
    // Only this gesture's fields that differ from what it already wrote
    // (or, before its first write, from where it began): a release whose
    // last step is already written, or one where the press began, writes
    // nothing.
    const done: Partial<VolumeDraft> = { ...g.start, ...g.written };
    const changed: Partial<VolumeDraft> = {};
    for (const k of VOLUME_FIELDS) {
      if (g.edits[k] !== undefined && g.edits[k] !== done[k]) (changed as Record<string, unknown>)[k] = g.edits[k];
    }
    if (Object.keys(changed).length === 0) {
      // A release whose final step is still in the throttle sends it now,
      // ahead of anything queued after the release (once: the throttle
      // holds it only once).
      if (mode === "commit" && this.volThrottleChain === chain) this.volThrottle.flush();
      this.settleVolume(chain);
      return true;
    }
    g.written = { ...g.written, ...changed };
    // Another chain's pending value goes out first, on its own base.
    if (this.volThrottleChain && this.volThrottleChain !== chain) this.volThrottle.flush();
    // The draft is what is shown plus this change; other gestures' fields
    // stay as they are.
    const shown = this.volumeView()?.level ?? g.start;
    this.volDraftChain = chain;
    this.set({ volumeDraft: { ...shown, ...changed } });
    // A pending value of this chain is merged with, not replaced by, this
    // one: each carries only its own fields.
    const v = this.volThrottleChain === chain && this.volPending ? { ...this.volPending.v, ...changed } : changed;
    this.volThrottleChain = chain;
    this.volPending = { chain, v };
    this.volThrottle.push(this.volPending);
    if (mode === "commit") this.volThrottle.flush();
    return true;
  }

  /**
   * One volume key or tray-wheel step, as a gesture of its own through the
   * same guarded write as the sliders (it continues a draft still on its way,
   * so quick presses add up). Mute toggles only the mute; up and down only the
   * volume, within the engine's range and ceiling. Returns what the on-screen
   * display shows, or null while the level is not known (nothing is written).
   */
  volumeKey(action: KeyAction, stepDb: number): OsdPayload | null {
    const view = this.volumeView();
    const vs = this.state.status?.volume;
    if (!view || !volumeWritable(vs)) return null;
    if (action !== "mute" && volumeOnDac(vs, this.state.status?.active_sink, this.state.config?.dsp_enabled ?? true)) {
      return { ...osdPayload(view.level, vs), note: "Bit-perfect: set the volume on the DAC" };
    }
    const patch = keyPatch(view.level, vs, action, stepDb);
    if (patch) this.setVolume(patch, "commit", this.beginVolumeGesture(view));
    return osdPayload(this.volumeView()?.level ?? view.level, vs);
  }

  // ------------------------------------------------------------ overlays

  /** Read the overlay files stored by the app process (in memory in the browser). */
  async refreshOverlays() {
    try {
      const raw = await this.transport.listOverlays();
      const seen = new Set<string>();
      const list: Overlay[] = [];
      for (const r of raw) {
        const d = overlayData(r);
        // A hand-edited file can repeat a name; the first one wins.
        if (!d || seen.has(d.name)) continue;
        seen.add(d.name);
        list.push(toOverlay(d));
      }
      this.set({ overlays: list });
    } catch (e) {
      this.set({ overlays: [] });
      this.toast(`Could not read the overlay files. ${errorText(e)}`, "error");
    }
  }

  /** A colour no other overlay uses yet. */
  private nextColour(): string {
    const used = new Set(this.state.overlays.map((o) => o.colour.toLowerCase()));
    return OVERLAY_COLOURS.find((c) => !used.has(c.toLowerCase())) ?? OVERLAY_COLOURS[this.state.overlays.length % OVERLAY_COLOURS.length];
  }

  private enqueueOverlayIo(name: string, job: () => Promise<void>): Promise<void> {
    const prev = this.overlayChains.get(name) ?? Promise.resolve();
    const run = prev.catch(() => undefined).then(job);
    const tail = run.finally(() => {
      if (this.overlayChains.get(name) === tail) this.overlayChains.delete(name);
    });
    this.overlayChains.set(name, tail);
    return run;
  }

  private writeOverlay(o: Overlay): Promise<void> {
    const file = overlayFile(o);
    return this.enqueueOverlayIo(o.name, async () => {
      try {
        await this.transport.saveOverlay(o.name, file);
      } catch (e) {
        this.toast(`Could not save “${o.name}”. ${errorText(e)}`, "error");
      }
    });
  }

  /** Put a curve on the graph and in the overlay folder. */
  addOverlay(data: OverlayData, okText?: string): boolean {
    const existing = this.state.overlays.find((o) => o.name === data.name);
    if (existing) {
      if (!existing.visible) this.updateOverlay(existing.id, { visible: true });
      this.toast(`“${data.name}” is already on the graph.`);
      return false;
    }
    const o = toOverlay(data, { colour: this.nextColour() });
    this.set({ overlays: [...this.state.overlays, o] });
    void this.writeOverlay(o);
    this.toast(okText ?? `Added “${o.name}”`);
    return true;
  }

  /** Show/hide, recolour, offset or predict; the file keeps the change. */
  updateOverlay(id: string, patch: Partial<Pick<Overlay, "visible" | "colour" | "offsetDb" | "predicted">>) {
    const next = this.state.overlays.map((o) => (o.id === id ? { ...o, ...patch } : o));
    const o = next.find((x) => x.id === id);
    this.set({ overlays: next });
    if (o) void this.writeOverlay(o);
  }

  async removeOverlay(id: string) {
    const o = this.state.overlays.find((x) => x.id === id);
    if (!o) return;
    this.set({ overlays: this.state.overlays.filter((x) => x.id !== id) });
    await this.enqueueOverlayIo(o.name, async () => {
      try {
        await this.transport.deleteOverlay(o.name);
        this.toast(`Removed “${o.name}”`);
      } catch (e) {
        this.toast(`Could not remove “${o.name}”. ${errorText(e)}`, "error");
        void this.refreshOverlays();
      }
    });
  }

  /** Import a target or measurement file (or a three-column bounds file). */
  importOverlay(text: string, fileName: string, kind: OverlayKind) {
    const taken = this.state.overlays.map((o) => o.name);
    const name = uniqueName(overlayNameFromFile(fileName), taken);
    try {
      const data = parseOverlayText(text, { kind, name: fileName });
      data.name = name;
      this.addOverlay(data, `Imported “${name}”`);
    } catch (e) {
      this.toast(e instanceof OverlayError ? e.message : `Could not read ${fileName}. ${errorText(e)}`, "error");
    }
  }

  /** Preference bounds from two single-curve files. */
  importBounds(lower: { text: string; name: string }, upper: { text: string; name: string }) {
    const taken = this.state.overlays.map((o) => o.name);
    const name = uniqueName(`${overlayNameFromFile(lower.name)} bounds`, taken);
    try {
      this.addOverlay(boundsFromCurves(lower.text, upper.text, name), `Imported bounds “${name}”`);
    } catch (e) {
      this.toast(e instanceof OverlayError ? e.message : `Could not read ${lower.name}. ${errorText(e)}`, "error");
    }
  }

  setOverlayRef(hz: number) {
    if (!Number.isFinite(hz)) return;
    this.set({ overlayRef: Math.round(Math.max(10, Math.min(20000, hz))) });
  }

  // ------------------------------------------------------------ presets
  //
  // `part` picks a page's presets (docs/control-plane.md "Presets"); without
  // it, the global ones in the top bar.

  activePreset(part?: PresetPart): string | null {
    if (part) return this.state.status?.part_presets?.find((x) => x.part === part)?.name ?? this.state.config?.active_part_presets?.[part] ?? null;
    return this.state.status?.preset?.name ?? this.state.config?.active_preset ?? null;
  }

  presetModified(part?: PresetPart): boolean {
    if (part) return !!this.state.status?.part_presets?.find((x) => x.part === part)?.modified;
    return !!this.state.status?.preset?.modified;
  }

  /** The list entries of one picker: a page's presets, or the global ones. */
  presetsOf(part?: PresetPart): PresetInfo[] {
    return this.state.presets.filter((p) => (p.part ?? undefined) === part);
  }

  /** The EQ editor's undo history and draft belong to the EQ: a load that replaces it drops them. */
  private afterPresetLoad(parts: PresetPart[] = ["eq"]) {
    if (!parts.includes("eq")) return;
    this.history.clear();
    this.syncHistory();
    this.set({ selectedBand: 0, eqDraft: null });
  }

  private partsOfPreset(name: string, part?: PresetPart): PresetPart[] {
    if (part) return [part];
    const info = this.state.presets.find((p) => !p.part && p.name === name);
    return info ? presetCovers(info) : [...PRESET_PARTS];
  }

  async loadPreset(name: string, part?: PresetPart) {
    const parts = this.partsOfPreset(name, part);
    await this.mutate(() => this.call("load_preset", part ? { name, part } : { name }), `Loaded “${name}”`, () => this.afterPresetLoad(parts));
  }

  /** "New (blank)": the page's part back to its defaults, with no preset active, so "Save as" names it. */
  async blankPreset(part: PresetPart) {
    await this.mutate(() => this.call("blank_preset", { part }), `${PART_NAME[part]} reset to defaults, not saved yet`, () => this.afterPresetLoad([part]));
  }

  /** Put a `PresetInfo` reply into the list now; the `presets` event confirms it. */
  private upsertPreset(info: PresetInfo, replaces?: string) {
    const same = (p: PresetInfo) => (p.part ?? undefined) === (info.part ?? undefined);
    const list = this.state.presets.filter((p) => !(same(p) && (p.name === info.name || p.name === replaces)));
    list.push(info);
    const order = (p: PresetInfo) => (p.part ? PRESET_PARTS.indexOf(p.part) + 1 : 0);
    list.sort((a, b) => order(a) - order(b) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    this.set({ presets: list });
  }

  async savePreset(part?: PresetPart) {
    const name = this.activePreset(part);
    if (!name) return;
    // Saving over a global preset keeps the parts it holds.
    const info = part ? undefined : this.state.presets.find((p) => !p.part && p.name === name);
    const parts = info && info.parts?.length && info.parts.length < PRESET_PARTS.length ? info.parts : undefined;
    await this.mutate(
      async () => this.upsertPreset(await this.call<PresetInfo>("save_preset", { name, overwrite: true, ...(part ? { part } : {}), ...(parts ? { parts } : {}) })),
      `Saved “${name}”`,
    );
  }

  /** Save as a new (or replaced) preset; `parts` (global presets only) defaults to every part. */
  async savePresetAs(name: string, part?: PresetPart, parts?: PresetPart[]): Promise<boolean> {
    const n = name.trim();
    if (!n) return false;
    const subset = !part && parts && parts.length < PRESET_PARTS.length ? parts : undefined;
    return this.mutate(async () => {
      const exists = this.presetsOf(part).some((p) => p.name === n);
      this.upsertPreset(await this.call<PresetInfo>("save_preset", { name: n, overwrite: exists, ...(part ? { part } : {}), ...(subset ? { parts: subset } : {}) }));
    }, `Saved “${n}”`);
  }

  async renamePreset(from: string, to: string, part?: PresetPart): Promise<boolean> {
    const t = to.trim();
    if (!t || t === from) return false;
    return this.mutate(async () => this.upsertPreset(await this.call<PresetInfo>("rename_preset", { from, to: t, ...(part ? { part } : {}) }), from), `Renamed to “${t}”`);
  }

  async deletePreset(name: string, part?: PresetPart) {
    await this.mutate(async () => {
      await this.call("delete_preset", part ? { name, part } : { name });
      this.set({ presets: this.state.presets.filter((p) => !((p.part ?? undefined) === part && p.name === name)) });
    }, `Deleted “${name}”`);
  }

  async revertPreset(part?: PresetPart) {
    const name = this.activePreset(part);
    if (!name) return;
    const parts = this.partsOfPreset(name, part);
    await this.mutate(() => this.call("load_preset", part ? { name, part } : { name }), `Reverted to “${name}”`, () => this.afterPresetLoad(parts));
  }

  /**
   * ParametricEQ.txt import: the daemon stores the file as a new EQ preset,
   * then we load it. It changes the EQ only (an older engine without page
   * presets stores a whole-chain preset: the toast then says which stages
   * it turned off).
   */
  async importApoPreset(text: string, fileName: string) {
    if (this.state.caps.presets === false) {
      this.importApoToEq(text, fileName);
      return;
    }
    // The daemon stores text without filters as an empty preset; catch that here.
    try {
      const parsed = parseApo(text, "x").profile;
      if (parsed.bands.length === 0 && !parsed.tone) {
        this.toast(`No filters found in ${fileName}.`, "error");
        return;
      }
    } catch (e) {
      this.toast(`Could not read ${fileName}. ${errorText(e)}`, "error");
      return;
    }
    await this.queue.run(async () => {
      const base = presetNameFromFile(fileName);
      try {
        const before = (await this.freshConfig()).chain;
        // Names are unique per picker; an older engine keeps them all global.
        const taken = new Set(this.state.presets.filter((p) => !p.part || p.part === "eq").map((p) => p.name));
        let name = base;
        for (let i = 2; taken.has(name); i++) name = `${base.slice(0, 58)} (${i})`;
        const info = await this.call<PresetInfo>("import_apo", { name, text });
        this.upsertPreset(info);
        await this.call("load_preset", info.part ? { name: info.name, part: info.part } : { name: info.name });
        await this.refreshConfig();
        await this.refreshPresets();
        this.afterPresetLoad();
        const after = this.state.config?.chain;
        const off: string[] = [];
        if (after && !info.part) {
          if (convList(before).some((c) => c.enabled) && !convList(after).some((c) => c.enabled)) off.push("convolution");
          if (before.upsampler.target !== "off" && after.upsampler.target === "off") off.push("upsampling");
        }
        this.set({ view: "eq", eqMode: "linked" });
        this.toast(`Imported “${info.name}”` + (off.length ? ` (EQ only: ${off.join(" and ")} off)` : ""));
      } catch (e) {
        this.toast(`Could not import ${fileName}. ${errorText(e)}`, "error");
      }
    });
  }

  // ------------------------------------------------------------ A/B

  async abSelect(side: AbSide) {
    if (!this.state.config?.ab) {
      this.toast("Choose a side B preset in A/B compare first.");
      return;
    }
    await this.queue.run(async () => {
      try {
        const epoch = this.state.connEpoch;
        await this.call("ab_select", { side, crossfade_ms: 10 });
        const status = await this.call<Status>("status");
        this.applyStatus(status, epoch);
      } catch (e) {
        this.toast(errorText(e), "error");
      }
    });
  }

  /** Side B: a stored preset, flat (no processing besides A's upsampling), or off. */
  async setAbSource(source: { preset: string; part?: PresetPart } | "flat" | "off") {
    if (!this.state.config) return;
    this.set({ abCustomSel: null });
    if (source === "off") {
      await this.mutate(() => this.call("set_ab", null), "A/B compare is off");
      return;
    }
    await this.mutate(async () => {
      const cfg = await this.freshConfig();
      const level: LevelMatch = cfg.ab?.level ?? { mode: "analytic", weighting: "k" };
      const headroom = cfg.ab?.headroom ?? { mode: "auto" as const };
      let ab: AbConfig;
      if (source === "flat") {
        // A patch on chain A: B keeps following A's other stages.
        ab = { b: { patch: FLAT_PATCH }, label: "Flat", level, headroom };
      } else {
        const preset = await this.call<Preset>("get_preset", source.part ? { name: source.preset, part: source.part } : { name: source.preset });
        // B is chain A with the preset's parts: a partial preset compares only what it holds.
        const chain = applyPreset(cfg.chain, preset);
        if (chain.eq) chain.eq = { ...chain.eq, name: source.preset };
        else chain.eq = emptyEq(source.preset);
        // An engine without the IR chain takes one object or null.
        if (this.state.caps.convChain === false) (chain as { convolution: unknown }).convolution = convList(chain)[0] ?? null;
        ab = { b: { chain }, label: source.preset, level, headroom };
      }
      await this.call("set_ab", ab);
    });
  }

  /** The side-B editor's draft (kept while the view is closed); null drops it. */
  setAbCustomSel(sel: CustomSel | null) {
    this.set({ abCustomSel: sel });
  }

  /** Save a selection under a name (replacing one of that name); the name is its label. */
  saveCustomB(name: string, sel: CustomSel) {
    const list = withSaved(this.state.abCustomSaved, { name, sel: { ...sel, label: name } });
    storeSaved(list);
    this.set({ abCustomSaved: list });
  }

  deleteCustomB(name: string) {
    const list = this.state.abCustomSaved.filter((s) => s.name !== name);
    storeSaved(list);
    this.set({ abCustomSaved: list });
  }

  /** The custom filter design the builder can pin as B: A's own, else the last one run. */
  customDesignForB(): FilterDesign | null {
    const u = this.state.config?.chain.upsampler;
    return u && !u.preset && u.design ? u.design : this.lastCustomDesign;
  }

  /**
   * Side B from the custom builder: one merge patch with every change,
   * turning A/B on. Resolves to the engine's refusal, or null.
   */
  async setAbCustom(patch: Record<string, unknown>, label: string): Promise<string | null> {
    if (!this.state.config) return "Not connected to the engine.";
    let err: string | null = null;
    await this.mutate(
      async () => {
        const cfg = await this.freshConfig();
        const ab: AbConfig = {
          b: { patch } as AbB,
          label,
          level: cfg.ab?.level ?? { mode: "analytic", weighting: "k" },
          headroom: cfg.ab?.headroom ?? { mode: "auto" },
        };
        await this.call("set_ab", ab);
      },
      undefined,
      () => {
        this.set({ abCustomSel: null });
        this.toast(`Comparing with B: ${label}. Press \` to flip.`);
      },
      (e) => {
        err = errorText(e);
        return true;
      },
    );
    await this.queue.run(() => this.refreshStatusNow());
    return err;
  }

  /** What an "on" stage compare starts from when chain A has dropped the stage's settings. */
  compareMemory(): CompareMemory {
    return { eq: stash<EqProfile>("eq") };
  }

  /**
   * A stage header's Compare: side B is chain A with that stage off (or on,
   * when A has it set up but off), and A/B turns on. Pressed again while
   * that compare runs, it turns A/B off.
   */
  async compareStage(stage: CompareStage) {
    await this.comparePatch((chain) => stagePatch(chain, stage, this.compareMemory()));
  }

  /** The Upsampling view's Compare: side B is chain A with another filter preset. */
  async compareFilter(preset: string) {
    await this.comparePatch(() => filterPatch(preset));
  }

  /** Pin a custom filter design as side B (so A can be edited or switched freely). */
  async compareFilterDesign(design: FilterDesign, label?: string) {
    await this.comparePatch(() => filterDesignPatch(design, label));
  }

  /** The Output view's Compare: side B is chain A with another bit depth and/or dither. */
  async compareOutput(bits: number, dither: DitherKind) {
    await this.comparePatch(() => outputPatch(bits, dither));
  }

  private async comparePatch(make: (chain: ChainConfig) => BPatch | { error: string }) {
    if (!this.state.config) return;
    let stopped = false;
    let label = "";
    await this.mutate(async () => {
      const cfg = await this.freshConfig();
      const made = make(cfg.chain);
      if ("error" in made) throw new Error(made.error);
      if (comparing(cfg.ab, made.patch)) {
        stopped = true;
        await this.call("set_ab", null);
        return;
      }
      label = made.label;
      const ab: AbConfig = {
        b: { patch: made.patch } as AbB,
        label: made.label,
        level: cfg.ab?.level ?? { mode: "analytic", weighting: "k" },
        headroom: cfg.ab?.headroom ?? { mode: "auto" },
      };
      await this.call("set_ab", ab);
    }, undefined, () => {
      this.set({ abCustomSel: null });
      this.toast(stopped ? "A/B compare is off" : `Comparing with B: ${label}. Press \` to flip.`);
    });
    await this.queue.run(() => this.refreshStatusNow());
  }

  // ------------------------------------------------------------ blind tests

  /** The engine kept a blind mask from a run this app lost (a dropped connection). */
  private blindMaskStale = false;

  private setBlind(patch: Partial<BlindRun>) {
    const run = this.state.blindRun;
    if (run) this.set({ blindRun: { ...run, ...patch } });
  }

  /**
   * Start a blind run: draw and commit the hidden sides, set the engine's
   * blind mask (which also stops a level measurement), and wait for the
   * first button. Resolves to an error message, or null.
   */
  async blindStart(plan: BlindPlan, sideA: string, sideB: string): Promise<string | null> {
    const cfg = this.state.config;
    if (!cfg?.ab || this.state.conn !== "up") return "Turn A/B on first.";
    if (this.state.blindRun?.phase === "running") return null;
    plan = { ...plan, trials: clampTrials(plan.trials) };
    const secret = createSecret();
    const [commitment, truths] = await Promise.all([commitSecret(secret), xSequence(secret, plan.trials)]);
    try {
      await this.queue.run(() => this.call("ab_blind", { enabled: true }));
    } catch (e) {
      return errorText(e);
    }
    // The live harmonics view keeps nothing across the run (the engine
    // forgets its side too).
    liveHistory.clear();
    const now = this.state.config ?? cfg;
    this.set({
      blindRun: {
        phase: "running",
        plan,
        sideA,
        sideB,
        level: now.ab?.level ?? { mode: "analytic", weighting: "k" },
        secretHex: secretToHex(secret),
        commitment,
        truths,
        trial: 0,
        playing: null,
        trialStart: Date.now(),
        switches: 0,
        records: [],
        startedAt: new Date().toISOString(),
        endedAt: null,
        reason: null,
        fingerprint: blindFingerprint(now),
        maskSeen: false,
      },
    });
    await this.queue.run(() => this.refreshStatusNow());
    return null;
  }

  /** Play a button of the current trial (A, B or X; 1 or 2). */
  async blindPlay(button: BlindButton) {
    const run = this.state.blindRun;
    if (run?.phase !== "running") return;
    if (!(run.plan.mode === "abx" ? ["a", "b", "x"] : ["1", "2"]).includes(button)) return;
    const side = sideFor(run.plan.mode, run.truths[run.trial], button);
    this.setBlind({ playing: button, switches: run.switches + (run.playing === button ? 0 : 1) });
    await this.queue.run(async () => {
      try {
        // The crossfade every A/B switch uses.
        await this.call("ab_select", { side, crossfade_ms: 10 });
      } catch (e) {
        if (this.state.blindRun?.phase === "running") this.blindAbandon(`The engine refused a switch: ${errorText(e)}`);
      }
    });
  }

  /** Answer the current trial; after the last one the run ends and is scored. */
  blindAnswer(choice: BlindChoice) {
    const run = this.state.blindRun;
    // Listen at least once in each trial before answering.
    if (run?.phase !== "running" || !run.playing) return;
    if (!(run.plan.mode === "abx" ? ["A", "B"] : ["1", "2"]).includes(choice)) return;
    const truth = run.truths[run.trial];
    const record: TrialRecord = { index: run.trial + 1, truth, choice, ...scoreTrial(run.plan.mode, truth, choice), ms: Date.now() - run.trialStart, switches: run.switches };
    const records = [...run.records, record];
    if (records.length >= run.plan.trials) {
      this.setBlind({ records, phase: "done", endedAt: new Date().toISOString(), playing: null });
      void this.blindClearMask();
      return;
    }
    this.setBlind({ records, trial: run.trial + 1, playing: null, switches: 0, trialStart: Date.now() });
  }

  /** End a running test early: it is reported as abandoned and never scored. */
  blindAbandon(reason: string, clearMask = true) {
    if (this.state.blindRun?.phase !== "running") return;
    this.setBlind({ phase: "abandoned", reason, endedAt: new Date().toISOString(), playing: null });
    if (clearMask) void this.blindClearMask();
  }

  /** Back to the A/B view (the result is dropped). */
  blindClose() {
    if (this.state.blindRun?.phase === "running") this.blindAbandon("Stopped.");
    this.set({ blindRun: null });
  }

  private async blindClearMask() {
    if (this.state.conn !== "up") return;
    await this.queue.run(async () => {
      await this.call("ab_blind", { enabled: false }).catch(() => undefined);
      liveHistory.clear();
      await this.refreshStatusNow();
    });
  }

  private blindWatchStatus(status: Status) {
    const run = this.state.blindRun;
    const blind = !!status.ab?.blind;
    if (run?.phase === "running") {
      if (blind) {
        if (!run.maskSeen) this.setBlind({ maskSeen: true });
      } else if (run.maskSeen) {
        this.blindAbandon(status.ab ? "The blind mask was cleared: side B was set again elsewhere." : "A/B was turned off.");
      }
      if (abStoppedByOverload(this.state.config?.ab, status.overload)) this.blindAbandon("A DSP overload stopped A/B.");
    } else if (blind && this.blindMaskStale) {
      this.blindMaskStale = false;
      void this.blindClearMask();
    }
  }

  /** The run as JSON, with the secret revealed, for checking it later (`verifyExport`). */
  blindExport(): BlindExport | null {
    const run = this.state.blindRun;
    if (!run || run.phase === "running") return null;
    return {
      format: EXPORT_FORMAT,
      mode: run.plan.mode,
      question: MODE_TEXT[run.plan.mode].question,
      feedback: run.plan.feedback,
      planned_trials: run.plan.trials,
      outcome: run.phase === "done" ? "completed" : "abandoned",
      abandoned_reason: run.reason,
      started_at: run.startedAt,
      ended_at: run.endedAt ?? new Date().toISOString(),
      side_a: run.sideA,
      side_b: run.sideB,
      level: run.level,
      commitment: run.commitment,
      secret: run.secretHex,
      trials: run.records,
      result: run.phase === "done" ? scoreRun(run.plan.mode, run.records) : null,
    };
  }

  async blindCopyExport() {
    const e = this.blindExport();
    if (!e) return;
    try {
      await this.transport.copyText(JSON.stringify(e, null, 2) + "\n");
      this.toast("Copied the run as JSON. Its secret and commitment let anyone check the hidden sides.");
    } catch {
      this.toast("Copying is blocked here.", "error");
    }
  }

  /** The flip key: the other side. */
  async abFlip() {
    if (!this.state.config?.ab) return;
    await this.abSelect((this.state.status?.ab?.current ?? "a") === "a" ? "b" : "a");
  }

  /** A/B's shared headroom: automatic, or a fixed cut (0 to 12 dB). */
  async setAbHeadroom(headroom: AbConfig["headroom"]) {
    if (!this.state.config?.ab) return;
    await this.mutate(async () => {
      const ab = (await this.freshConfig()).ab;
      if (!ab) return;
      await this.call("set_ab", { ...ab, headroom });
    });
    await this.queue.run(() => this.refreshStatusNow());
  }

  private async refreshStatusNow() {
    const epoch = this.state.connEpoch;
    const status = await this.call<Status>("status").catch(() => null);
    if (status) this.applyStatus(status, epoch);
  }

  /**
   * "Match to this track" (`ab_measure`): start measuring both sides on the
   * music playing now, lock the running offset early ("accept"), or stop
   * and keep the current level ("cancel"). Progress comes in status events.
   * Resolves to the daemon's message when the call fails, for the view to
   * show next to the button.
   */
  async abMeasure(action: "start" | "accept" | "cancel", maxSeconds = MEASURE_MAX_DEFAULT): Promise<string | null> {
    if (!this.state.config?.ab) return null;
    return this.queue.run(async () => {
      let err: string | null = null;
      try {
        await this.call("ab_measure", action === "start" ? { max_seconds: maxSeconds } : action === "accept" ? { accept: true } : { cancel: true });
      } catch (e) {
        err = errorText(e);
      }
      // An accept writes the measured level to the config.
      if (action === "accept") await this.refreshConfig().catch(() => undefined);
      await this.refreshStatusNow();
      return err;
    });
  }

  async setLevelMatch(level: LevelMatch) {
    if (!this.state.config?.ab) return;
    await this.mutate(async () => {
      const ab = (await this.freshConfig()).ab;
      if (!ab) return;
      const epoch = this.state.connEpoch;
      await this.call("set_ab", { ...ab, level });
      const status = await this.call<Status>("status").catch(() => null);
      if (status) this.applyStatus(status, epoch);
    });
  }

  // ------------------------------------------------------------ routing / device

  private async refreshStreams() {
    const streams = await this.call<StreamInfo[]>("list_streams").catch(() => null);
    if (streams) this.setStreams(streams);
  }

  /** New stream list: learn whether the engine sets stream levels, and
   *  forget the drafts and messages of streams that are gone. */
  private setStreams(streams: StreamInfo[]) {
    this.streamsSeq++;
    const patch: Partial<AppState> = { streams };
    if (this.state.caps.streamVolume !== true && reportsStreamLevels(streams)) patch.caps = { ...this.state.caps, streamVolume: true };
    const live = new Set(streams.map((s) => s.node));
    const keep = <T,>(rec: Record<number, T>): Record<number, T> | null => {
      const gone = Object.keys(rec).filter((k) => !live.has(Number(k)));
      if (gone.length === 0) return null;
      const next = { ...rec };
      for (const k of gone) delete next[Number(k)];
      return next;
    };
    const drafts = keep(this.state.streamDrafts);
    if (drafts) patch.streamDrafts = drafts;
    const messages = keep(this.state.streamMessages);
    if (messages) patch.streamMessages = messages;
    this.set(patch);
  }

  // ------------------------------------------------------------ stream volume

  /** A stream's level as shown: the draft of an edit on its way, else the engine's. */
  streamLevel(s: StreamInfo): { db: number | null; muted: boolean } {
    const d = this.state.streamDrafts[s.node];
    return { db: d?.volume_db ?? (typeof s.volume_db === "number" ? s.volume_db : null), muted: d?.muted ?? s.muted };
  }

  /** Bumped with every stream list (event or read): a `set_stream_volume`
   *  answer older than the newest list must not overwrite it. */
  private streamsSeq = 0;
  private streamThrottles = new Map<number, Throttle<true>>();
  private streamPending = new Map<number, StreamLevelEdit>();
  private streamDragging = new Set<number>();

  private streamBusy(node: number): boolean {
    return this.streamDragging.has(node) || this.streamPending.has(node) || this.queue.waiting(`stream:${node}`) > 0;
  }

  private setStreamDraft(node: number, edit: StreamLevelEdit | undefined) {
    const next = { ...this.state.streamDrafts };
    if (edit === undefined) delete next[node];
    else next[node] = edit;
    this.set({ streamDrafts: next });
  }

  private setStreamMessage(node: number, message: StreamMessage | null) {
    const cur = this.state.streamMessages[node] ?? null;
    if (cur === message || (cur && message && cur.text === message.text && cur.error === message.error)) return;
    const next = { ...this.state.streamMessages };
    if (message === null) delete next[node];
    else next[node] = message;
    this.set({ streamMessages: next });
  }

  clearStreamMessage(node: number) {
    this.setStreamMessage(node, null);
  }

  /**
   * One edit of an app stream's level or mute, with `set_stream_volume`.
   * The draft shows it at once; `drag` sends at most ~25 times a second
   * and keeps the draft up, `commit` sends now and ends the drag. The
   * engine's answer (the level it plays at) replaces the draft.
   */
  setStreamLevel(node: number, edit: StreamLevelEdit, mode: "drag" | "commit" = "commit") {
    if (this.state.caps.streamVolume === false) return;
    this.setStreamDraft(node, { ...this.state.streamDrafts[node], ...edit });
    this.streamPending.set(node, { ...this.streamPending.get(node), ...edit });
    let t = this.streamThrottles.get(node);
    if (!t) {
      t = throttleLatest(() => this.flushStreamLevel(node), VOLUME_INTERVAL_MS);
      this.streamThrottles.set(node, t);
    }
    if (mode === "drag") this.streamDragging.add(node);
    else this.streamDragging.delete(node);
    t.push(true);
    if (mode === "commit") t.flush();
  }

  private flushStreamLevel(node: number) {
    const edit = this.streamPending.get(node);
    this.streamPending.delete(node);
    if (!edit) return;
    void this.queue.latest<StreamLevelEdit>(
      `stream:${node}`,
      edit,
      (e) => this.runStreamLevel(node, e),
      (older, newer) => ({ ...older, ...newer }),
    );
  }

  /** Queued job: send one stream edit and show what the engine applied. */
  private async runStreamLevel(node: number, edit: StreamLevelEdit) {
    try {
      const seq = this.streamsSeq;
      const r = await this.call<StreamVolume>("set_stream_volume", { node, ...edit });
      const patch: Partial<AppState> = {};
      // The answer is the level playing now, unless a stream list came in
      // meanwhile: that is newer (another client, the desktop, the guard)
      // or carries this write itself, and the next list settles it.
      if (seq === this.streamsSeq) {
        patch.streams = this.state.streams.map((s) => (s.node === node ? { ...s, volume_db: r.volume_db, muted: r.muted } : s));
      }
      if (this.state.caps.streamVolume !== true) patch.caps = { ...this.state.caps, streamVolume: true };
      this.set(patch);
      if (edit.volume_db !== undefined) {
        const note = streamLimitNote(r);
        this.setStreamMessage(node, note ? { text: note, error: false } : null);
      } else if (this.state.streamMessages[node]?.error) {
        this.setStreamMessage(node, null);
      }
      if (this.streamBusy(node)) return;
      this.setStreamDraft(node, undefined);
    } catch (e) {
      if (isUnknownMethod(e)) {
        this.set({ caps: { ...this.state.caps, streamVolume: false } });
        this.setStreamMessage(node, { text: STREAM_VOLUME_NEEDS_UPDATE, error: true });
      } else {
        this.setStreamMessage(node, { text: errorText(e), error: true });
      }
      if (this.streamBusy(node)) return;
      this.setStreamDraft(node, undefined);
      await this.refreshStreams();
    }
  }

  /** A routing mode, or an edit of the mode the daemon holds when the job runs. */
  async setRoutingMode(mode: RoutingMode | ((cur: RoutingMode) => RoutingMode | null)) {
    await this.mutate(async () => {
      const next = typeof mode === "function" ? mode((await this.freshConfig()).routing) : mode;
      if (next) await this.call("set_routing_mode", next);
      await this.refreshStreams();
    });
  }

  /** Move a stream now; in Selected-apps mode also remember it for the app. */
  async routeStream(s: StreamInfo, sink: SinkChoice) {
    await this.mutate(async () => {
      await this.call("route", { target: { node: s.node }, sink });
      const app = s.app_name || s.binary;
      const routing = (await this.freshConfig()).routing;
      if (routing.mode === "apps" && app) {
        const rules = routing.rules.filter((r) => r.app.toLowerCase() !== app.toLowerCase());
        if (sink !== "default") rules.push({ app, sink });
        await this.call("set_routing_mode", { mode: "apps", rules });
      }
      await this.refreshStreams();
    });
  }

  async removeAppRule(app: string) {
    if (this.state.config?.routing.mode !== "apps") return;
    await this.setRoutingMode((cur) => (cur.mode === "apps" ? { mode: "apps", rules: cur.rules.filter((r) => r.app !== app) } : null));
  }

  /**
   * The configured device's list entry. The daemon marks it `active`, which
   * also covers other spellings of the same device (`hw:1,0`, `hw:DAC,0,0`).
   */
  currentDevice(): DeviceInfo | undefined {
    const s = this.state;
    return s.devices.find((d) => d.active) ?? s.devices.find((d) => d.id === s.config?.device);
  }

  async setDevice(id: string) {
    const cfg = this.state.config;
    if (!cfg || cfg.device === id || this.currentDevice()?.id === id) return;
    // The engine refuses a main device that a follower plays: say why and
    // let the Device page offer to remove the follower first.
    if (followerOn(id, this.followers())) {
      this.set({ deviceConflict: id });
      if (this.state.view !== "device") this.toast(`${id} already plays as another device. Remove it from "Also play on" on the Device page first.`, "error");
      return;
    }
    if (this.state.deviceConflict !== null) this.set({ deviceConflict: null });
    await this.editField(
      "set_device",
      (c) => ({ device: c.device }),
      () => ({ device: id }),
      (c, d) => ({ ...c, device: d.device }),
      "Output device changed",
    );
    void this.refreshDevices();
  }

  /**
   * The device channels the main output's left and right play on, for a
   * device with more than two (`[0, 1]`: the first two). The engine reopens
   * the device at the channel count the pair needs.
   */
  async setOutputChannels(pair: OutputChannels) {
    await this.editField(
      "set_device",
      (c) => ({ device: c.device, output_channels: c.output_channels ?? DEFAULT_PAIR }),
      (cur) => ({ device: cur.device, output_channels: pair }),
      (c, d) => ({ ...c, output_channels: isDefaultPair(d.output_channels) ? undefined : d.output_channels }),
      "Output channels changed",
    );
  }

  /** Remove the follower on `id`, then move the main device there. */
  async moveDeviceToFollower(id: string) {
    this.set({ deviceConflict: null });
    if (followerOn(id, this.followers())) {
      const ok = await this.mutate(() => this.call("set_follower", { device: id, remove: true }));
      if (!ok) return;
    }
    await this.setDevice(id);
  }

  clearDeviceConflict() {
    this.set({ deviceConflict: null });
  }

  /** Say which card each held output is (identical DACs that moved USB ports): every output at once. */
  assignDevices(assignments: DeviceAssignment[]) {
    return this.mutate(async () => {
      await this.call("assign_devices", { assignments });
      await this.refreshDevices();
    }, "Outputs assigned");
  }

  // ------------------------------------------------------------ followers (multi-device)

  /** The engine has multi-device output (its config carries `outputs`). */
  followersKnown(): boolean {
    return this.state.config?.outputs !== undefined;
  }

  /** The configured followers, as the daemon holds them (no drafts). */
  followers(): FollowerConfig[] {
    return this.state.config?.outputs?.followers ?? [];
  }

  private followerPending = new Map<string, FollowerPatch & { remove?: boolean }>();
  private followerThrottles = new Map<string, Throttle<true>>();
  private followerDragging = new Set<string>();

  private followerBusy(device: string): boolean {
    return this.followerDragging.has(device) || this.followerPending.has(device) || this.queue.waiting(`follower:${device}`) > 0;
  }

  private followerThrottle(device: string): Throttle<true> {
    let t = this.followerThrottles.get(device);
    if (!t) {
      t = throttleLatest(() => this.flushFollower(device), EQ_INTERVAL_MS);
      this.followerThrottles.set(device, t);
    }
    return t;
  }

  private sendFollower(device: string) {
    const t = this.followerThrottle(device);
    t.push(true);
    t.flush();
  }

  /** Add a follower on `device` (enabled, no trim, 0 dB). */
  addFollower(device: string) {
    if (!this.state.config || !device) return;
    this.followerPending.set(device, {});
    this.sendFollower(device);
  }

  /** Remove the follower on `device`. */
  removeFollower(device: string) {
    if (!this.state.config) return;
    this.followerThrottles.get(device)?.cancel();
    this.followerDragging.delete(device);
    this.followerPending.set(device, { remove: true });
    this.sendFollower(device);
  }

  /**
   * One live follower edit (`set_follower`, only the fields given). `drag`
   * sends at most ~25 times a second; `commit` sends now and ends the drag.
   */
  editFollower(device: string, patch: FollowerPatch, mode: "commit" | "drag" = "commit") {
    if (!this.state.config) return;
    this.set({ followerDrafts: { ...this.state.followerDrafts, [device]: { ...(this.state.followerDrafts[device] ?? {}), ...patch } } });
    this.followerPending.set(device, { ...(this.followerPending.get(device) ?? {}), ...patch });
    if (mode === "drag") this.followerDragging.add(device);
    else this.followerDragging.delete(device);
    const t = this.followerThrottle(device);
    t.push(true);
    if (mode === "commit") t.flush();
  }

  /** End a follower slider drag or wheel burst (see `endVarispeedDrag`). */
  endFollowerDrag(device: string) {
    if (!this.followerDragging.delete(device)) return;
    const t = this.followerThrottles.get(device);
    if (t?.pending) {
      t.flush();
      return;
    }
    void this.queue.run(async () => {
      if (this.followerBusy(device) || !this.state.followerDrafts[device]) return;
      await this.refreshConfig().catch(() => undefined);
      if (!this.followerBusy(device)) this.dropFollowerDraft(device);
    });
  }

  private dropFollowerDraft(device: string) {
    if (!(device in this.state.followerDrafts)) return;
    const { [device]: _gone, ...rest } = this.state.followerDrafts;
    void _gone;
    this.set({ followerDrafts: rest });
  }

  private setFollowerError(device: string, text: string | null) {
    const cur = this.state.followerErrors;
    if (text === null && !(device in cur)) return;
    if (text === null) {
      const { [device]: _gone, ...rest } = cur;
      void _gone;
      this.set({ followerErrors: rest });
    } else this.set({ followerErrors: { ...cur, [device]: text } });
  }

  clearFollowerError(device: string) {
    this.setFollowerError(device, null);
  }

  private flushFollower(device: string) {
    const patch = this.followerPending.get(device);
    this.followerPending.delete(device);
    if (!patch) return;
    void this.queue.latest<FollowerPatch & { remove?: boolean }>(
      `follower:${device}`,
      patch,
      (p) => this.runFollower(device, p),
      // A removal wins over edits queued before it; an edit after a removal re-adds.
      (older, newer) => (newer.remove ? newer : older.remove ? newer : { ...older, ...newer }),
    );
  }

  private async runFollower(device: string, patch: FollowerPatch & { remove?: boolean }) {
    const epoch = this.state.connEpoch;
    const adding = !patch.remove && !followerOn(device, this.followers());
    try {
      const echo = await this.call<OutputsStatus>("set_follower", { device, ...patch });
      const status = this.state.status;
      if (status && echo && epoch === this.state.connEpoch) this.set({ status: { ...status, outputs: echo } });
      this.setFollowerError(device, null);
      if (adding) this.setFollowerError("", null);
      if (this.followerBusy(device)) return;
      await this.refreshConfig();
      if (!this.followerBusy(device)) this.dropFollowerDraft(device);
    } catch (e) {
      const text = isUnknownMethod(e) ? "This engine cannot play on other devices. Update the engine to use it." : errorText(e);
      this.setFollowerError(adding ? "" : device, text);
      if (this.followerBusy(device)) return;
      this.dropFollowerDraft(device);
      await this.refreshConfig().catch(() => undefined);
    }
  }

  async releaseDevice() {
    await this.mutate(
      () => this.call("release_device"),
      "The DAC is free for other apps",
      () => spectrumStore.clear(),
    );
  }

  async acquireDevice() {
    await this.mutate(() => this.call("acquire_device"), "Audivane has the DAC again");
  }

  async runSelfTest() {
    this.set({ selfTest: { state: "running" } });
    try {
      const result = await this.call<SelfTestResult>("self_test");
      this.set({ selfTest: { state: "done", result } });
    } catch (e) {
      this.set({ selfTest: { state: "error", error: errorText(e) } });
    }
  }

  isDirty(): boolean {
    return this.presetModified();
  }

  /** For tests: whether the current EQ equals the daemon's. */
  eqInSync(): boolean {
    return deepEqual(this.eq(), this.state.config?.chain.eq ?? null);
  }
}

let storeInstance: AppStore | null = null;

export function initStore(transport: Transport, opts: { secondary?: boolean } = {}): AppStore {
  storeInstance = new AppStore(transport, opts);
  return storeInstance;
}

export function store(): AppStore {
  if (!storeInstance) throw new Error("store not initialised");
  return storeInstance;
}

/** Subscribe a component to a slice of the state. */
export function useApp<T>(select: (s: AppState) => T): T {
  const s = store();
  return useSyncExternalStore(s.subscribe, () => select(s.getState()));
}
