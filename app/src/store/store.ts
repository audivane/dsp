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
import { DaemonError, ErrorCode, isUnknownMethod, type ConnState, type IrInfo, type Transport } from "../api/transport";
import type {
  AbConfig,
  AbSide,
  Band,
  ChainConfig,
  ConvolutionConfig,
  DaemonEvent,
  DeviceInfo,
  EngineConfig,
  EqProfile,
  LevelMatch,
  NowPlaying,
  OutputConfig,
  Preset,
  PresetInfo,
  RoutingMode,
  RateConflict,
  SampleFormat,
  SelfTestResult,
  SinkChoice,
  Status,
  StreamInfo,
  UpsamplerConfig,
  ResponseWindowRequest,
  UpsamplerResponse,
  VolumeStatus,
} from "../api/types";
import { parseApo, presetNameFromFile, toApo } from "../lib/apo";
import {
  EQ_PROFILE_VERSION,
  addSection,
  clearSection,
  convList,
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
import { profileAutoPreampDb } from "../lib/eqmath";
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
import { loadSpectrumPrefs, saveSpectrumPrefs, type SpectrumPrefs } from "../lib/spectrum";
import { throttleLatest, type Throttle } from "../lib/throttle";
import { meterStore } from "./meters";
import { spectrumStore } from "./spectrum";

/** An EQ edit as a function of the EQ it applies to. */
export type EqEdit = (eq: EqProfile | null) => EqProfile | null;

export type ViewId = "eq" | "conv" | "ups" | "out" | "routing" | "ab" | "device";

export interface Caps {
  /** null = not known yet. */
  presets: boolean | null;
  devices: boolean | null;
  volume: boolean | null;
  perChannel: boolean | null;
  convChain: boolean | null;
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
  eqMode: EqSection;
  selectedBand: number;
  eqDraft: EqProfile | null;
  autoPreamp: boolean;
  canUndo: boolean;
  canRedo: boolean;
  volumeDraft: VolumeDraft | null;
  /** Curves drawn behind the EQ: targets, preference bounds, measurements. */
  overlays: Overlay[];
  /** Overlays are normalised at this frequency (Hz) before their own offset. */
  overlayRef: number;
  toast: Toast | null;
  selfTest: SelfTestState;
  starting: boolean;
  startError: string | null;
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

function stash<T>(key: string, value?: T): T | null {
  try {
    if (value === undefined) {
      const raw = localStorage.getItem("avdsp-stash-" + key);
      return raw ? (JSON.parse(raw) as T) : null;
    }
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
  private volThrottle: Throttle<VolumeDraft>;
  private volDragging = false;
  private toastSeq = 0;
  private toastTimer: ReturnType<typeof setTimeout> | null = null;
  private stop: (() => void) | null = null;
  private spectrumSubscribed: boolean | null = null;
  private overlayChains = new Map<string, Promise<void>>();

  constructor(private transport: Transport) {
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
      caps: { presets: null, devices: null, volume: null, perChannel: null, convChain: null },
      irInfo: {},
      view: ui.view ?? "eq",
      spectrum: loadSpectrumPrefs(),
      eqMode: "linked",
      selectedBand: 0,
      eqDraft: null,
      autoPreamp: ui.autoPreamp ?? true,
      canUndo: false,
      canRedo: false,
      volumeDraft: null,
      overlays: [],
      overlayRef: Number.isFinite(ui.overlayRef) && (ui.overlayRef ?? 0) >= 10 ? (ui.overlayRef as number) : 1000,
      toast: null,
      selfTest: { state: "idle" },
      starting: false,
      startError: null,
    };
    this.eqThrottle = throttleLatest(() => this.flushEq(), EQ_INTERVAL_MS);
    this.volThrottle = throttleLatest((v) => this.sendVolume(v), VOLUME_INTERVAL_MS);
  }

  // ------------------------------------------------------------ plumbing

  getState = (): AppState => this.state;

  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  private set(patch: Partial<AppState>) {
    this.state = { ...this.state, ...patch };
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
  private mutate(fn: () => Promise<unknown>, okText?: string, onOk?: () => void): Promise<boolean> {
    return this.queue.run(async () => {
      try {
        await fn();
        await this.refreshConfig();
        onOk?.();
        if (okText) this.toast(okText);
        return true;
      } catch (e) {
        this.toast(errorText(e), "error");
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
      await this.queue.drained();
      if (this.overlayChains.size > 0) await Promise.all([...this.overlayChains.values()]);
      if (this.queue.idle && !this.eqThrottle.pending && !this.volThrottle.pending && this.overlayChains.size === 0) return;
    }
  }

  // ------------------------------------------------------------ connection

  connect() {
    if (this.stop) return;
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
    const [config, status, streams, nowPlaying] = await Promise.all([
      this.call<EngineConfig>("get_config").catch(() => null),
      this.call<Status>("status").catch(() => null),
      this.call<StreamInfo[]>("list_streams").catch(() => []),
      this.call<NowPlaying | null>("now_playing").catch(() => null),
    ]);
    if (config) this.applyConfig(config);
    if (status) this.applyStatus(status);
    this.set({ streams: streams ?? [], nowPlaying });
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
      case "meters":
        meterStore.push(e.data, typeof performance !== "undefined" ? performance.now() : Date.now());
        break;
      case "spectrum":
        spectrumStore.push(e.data, typeof performance !== "undefined" ? performance.now() : Date.now());
        break;
      case "status":
        this.applyStatus(e.data);
        break;
      case "streams":
        this.set({ streams: e.data });
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
    const eq = config.chain.eq;
    if (!this.state.eqDraft) {
      if (!eq && this.state.eqMode !== "linked") patch.eqMode = "linked";
      else if (this.state.eqMode !== "linked" && !hasSection(eq, this.state.eqMode)) patch.eqMode = "linked";
    }
    this.set(patch);
    // `active` in the device list follows the configured device; re-read it when that changes.
    if (this.state.caps.devices && config.device !== this.devicesFor) void this.refreshDevices();
    for (const c of convList(config.chain)) void this.loadIrInfo(c.ir_path);
  }

  private applyStatus(status: Status) {
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
    const volume = status.volume !== undefined && status.volume !== null;
    this.set({ status, caps: caps.volume === volume ? caps : { ...caps, volume } });
  }

  private async loadIrInfo(path: string) {
    if (path in this.state.irInfo) return;
    this.set({ irInfo: { ...this.state.irInfo, [path]: null } });
    const info = await this.transport.irInfo(path);
    this.set({ irInfo: { ...this.state.irInfo, [path]: info } });
  }

  async startEngine() {
    this.set({ starting: true, startError: null });
    try {
      await this.transport.startEngine();
      // The subscription reconnects on its own; give it time before giving up.
      setTimeout(() => {
        if (this.state.conn !== "up" && this.state.starting) {
          this.set({ starting: false, startError: "The engine was started but is not answering yet. Check `journalctl --user -u audivane-dsp`." });
        }
      }, 8000);
    } catch (e) {
      this.set({ starting: false, startError: errorText(e) });
    }
  }

  // ------------------------------------------------------------ navigation

  setView(view: ViewId) {
    this.set({ view });
  }

  setSpectrumPrefs(patch: Partial<SpectrumPrefs>) {
    const spectrum = { ...this.state.spectrum, ...patch };
    saveSpectrumPrefs(spectrum);
    this.set({ spectrum });
  }

  // ------------------------------------------------------------ EQ

  /** The EQ as shown: the drag draft, else the daemon's. */
  eq(): EqProfile | null {
    return this.state.eqDraft ?? this.state.config?.chain.eq ?? null;
  }

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
      this.set({ eqDraft: null });
    } catch (e) {
      this.toast(errorText(e), "error");
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
    this.set({ eqDraft: shown });
    const prev = this.eqPending;
    this.eqPending = prev ? (eq) => fn(prev(eq)) : fn;
    this.eqThrottle.push(true);
    if (mode === "commit") this.eqThrottle.flush();
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
    const added: Band = { kind: "peaking" as const, freq_hz: 1000, gain_db: 0, q: 1, enabled: true, ...band };
    this.editBands((bands) => [...bands, added]);
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
        if (cur) stash("eq", cur);
        return null;
      });
    } else {
      this.editEq((cur) => cur ?? stash<EqProfile>("eq") ?? emptyEq(fallback));
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

  /** Queue an edit of the IR chain, applied to the chain the daemon holds when it runs. */
  private editConvolution(fn: (list: ConvolutionConfig[]) => ConvolutionConfig[], okText?: string) {
    return this.mutate(async () => {
      const cfg = await this.freshConfig();
      await this.sendConvolution(fn(convList(cfg.chain)));
    }, okText);
  }

  /** Index of the IR shown at `i` (by path) in `list`, which may have changed since. */
  private irIndex(list: ConvolutionConfig[], i: number, path: string | undefined): number {
    if (path === undefined) return -1;
    return list[i]?.ir_path === path ? i : list.findIndex((c) => c.ir_path === path);
  }

  convolution(): ConvolutionConfig[] {
    return convList(this.state.config?.chain);
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
    void this.editConvolution((list) => [...list, { enabled: true, ir_path: path, gain_db: 0, name: null }], "Added an impulse response");
  }

  setIr(i: number, patch: Partial<ConvolutionConfig>) {
    const path = this.convolution()[i]?.ir_path;
    void this.editConvolution((list) => {
      const at = this.irIndex(list, i, path);
      return list.map((c, j) => (j === at ? { ...c, ...patch } : c));
    });
  }

  removeIr(i: number) {
    const path = this.convolution()[i]?.ir_path;
    void this.editConvolution((list) => {
      const at = this.irIndex(list, i, path);
      return list.filter((_, j) => j !== at);
    });
  }

  upsampler(): UpsamplerConfig | null {
    return this.state.config?.chain.upsampler ?? null;
  }

  /** Queue an upsampler edit, applied to the daemon's upsampler when it runs. */
  private editUpsampler(fn: (cur: UpsamplerConfig) => UpsamplerConfig, okText?: string) {
    if (!this.upsampler()) return;
    void this.mutate(async () => {
      const cfg = await this.freshConfig();
      await this.call("set_upsampler", fn(cfg.chain.upsampler));
    }, okText);
  }

  setUpsampler(patch: Partial<UpsamplerConfig>, okText?: string) {
    if (patch.target && patch.target !== "off") stash("ups", patch.target);
    this.editUpsampler((cur) => ({ ...cur, ...patch }), okText);
  }

  setUpsamplingEnabled(on: boolean) {
    this.editUpsampler((cur) => {
      if (!on) {
        if (cur.target !== "off") stash("ups", cur.target);
        return { ...cur, target: "off" };
      }
      if (cur.target !== "off") return cur;
      return { ...cur, target: stash<UpsamplerConfig["target"]>("ups") ?? { ratio: 4 } };
    });
  }

  upsamplerResponse(config: UpsamplerConfig | null, sourceRate: number | null, window?: ResponseWindowRequest): Promise<UpsamplerResponse> {
    return this.transport.upsamplerResponse(config, sourceRate, window);
  }

  /**
   * Changes without a dedicated call go through `set_config`. `fn` gets the
   * config the daemon holds when the job runs (read right before the write),
   * so nothing another change set in the meantime is written back.
   */
  private editConfig(fn: (c: EngineConfig) => EngineConfig, okText?: string) {
    if (!this.state.config) return Promise.resolve(false);
    return this.mutate(async () => {
      const cur = await this.freshConfig();
      await this.call("set_config", fn(cur));
    }, okText);
  }

  setOutput(patch: Partial<OutputConfig>) {
    void this.editConfig((c) => ({ ...c, chain: { ...c.chain, output: { ...c.chain.output, ...patch } } }));
  }

  setPreferredFormat(f: SampleFormat) {
    void this.editConfig((c) => ({ ...c, formats: [f, ...c.formats.filter((x) => x !== f)] }));
  }

  /** What the engine does when apps play into the DSP sink at different rates. */
  setRateConflict(conflict: RateConflict) {
    void this.editConfig((c) => ({ ...c, rate: { ...c.rate, conflict } }));
  }

  // ------------------------------------------------------------ bypass / volume

  bypassed(): boolean {
    return this.state.config ? !this.state.config.dsp_enabled : false;
  }

  setBypass(on: boolean) {
    void this.mutate(() => this.call("set_dsp_enabled", { enabled: !on }), on ? "Bypass on: audio goes to the DAC untouched" : undefined);
  }

  volume(): VolumeDraft | null {
    if (this.state.volumeDraft) return this.state.volumeDraft;
    const v: VolumeStatus | null | undefined = this.state.status?.volume;
    return v ? { volume_db: v.volume_db, balance_db: v.balance_db, muted: v.muted } : null;
  }

  /** Hand the slider's value to the queue (one waiting job at most; it carries the newest value). */
  private sendVolume(v: VolumeDraft) {
    void this.queue.latest("volume", v, (x) => this.runVolume(x));
  }

  private async runVolume(v: VolumeDraft) {
    const busy = () => this.volDragging || this.volThrottle.pending || this.queue.waiting("volume") > 0;
    try {
      const echo = await this.call<VolumeStatus>("set_volume", v);
      const status = this.state.status;
      if (status) this.set({ status: { ...status, volume: echo } });
      if (!busy()) this.set({ volumeDraft: null });
    } catch (e) {
      this.toast(errorText(e), "error");
      if (!busy()) this.set({ volumeDraft: null });
    }
  }

  /** Volume/balance/mute. `drag` keeps a local draft and throttles. */
  setVolume(patch: Partial<VolumeDraft>, mode: "drag" | "commit" = "commit") {
    const cur = this.volume();
    if (!cur) return;
    const next = { ...cur, ...patch };
    this.volDragging = mode === "drag";
    this.set({ volumeDraft: next });
    this.volThrottle.push(next);
    if (mode === "commit") this.volThrottle.flush();
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

  activePreset(): string | null {
    return this.state.status?.preset?.name ?? this.state.config?.active_preset ?? null;
  }

  presetModified(): boolean {
    return !!this.state.status?.preset?.modified;
  }

  private afterPresetLoad() {
    this.history.clear();
    this.syncHistory();
    this.set({ selectedBand: 0, eqDraft: null });
  }

  async loadPreset(name: string) {
    await this.mutate(() => this.call("load_preset", { name }), `Loaded “${name}”`, () => this.afterPresetLoad());
  }

  /** Put a `PresetInfo` reply into the list now; the `presets` event confirms it. */
  private upsertPreset(info: PresetInfo, replaces?: string) {
    const list = this.state.presets.filter((p) => p.name !== info.name && p.name !== replaces);
    list.push(info);
    list.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    this.set({ presets: list });
  }

  async savePreset() {
    const name = this.activePreset();
    if (!name) return;
    await this.mutate(async () => this.upsertPreset(await this.call<PresetInfo>("save_preset", { name, overwrite: true })), `Saved “${name}”`);
  }

  async savePresetAs(name: string): Promise<boolean> {
    const n = name.trim();
    if (!n) return false;
    return this.mutate(async () => {
      const exists = this.state.presets.some((p) => p.name === n);
      this.upsertPreset(await this.call<PresetInfo>("save_preset", { name: n, overwrite: exists }));
    }, `Saved “${n}”`);
  }

  async renamePreset(from: string, to: string): Promise<boolean> {
    const t = to.trim();
    if (!t || t === from) return false;
    return this.mutate(async () => this.upsertPreset(await this.call<PresetInfo>("rename_preset", { from, to: t }), from), `Renamed to “${t}”`);
  }

  async deletePreset(name: string) {
    await this.mutate(async () => {
      await this.call("delete_preset", { name });
      this.set({ presets: this.state.presets.filter((p) => p.name !== name) });
    }, `Deleted “${name}”`);
  }

  async revertPreset() {
    const name = this.activePreset();
    if (!name) return;
    await this.mutate(() => this.call("load_preset", { name }), `Reverted to “${name}”`, () => this.afterPresetLoad());
  }

  /**
   * Preset menu import: the daemon stores the file as a new preset, then we
   * load it. An imported preset is EQ only (no convolution, default
   * upsampling and output), so loading it can switch those stages off; the
   * toast says so.
   */
  async importApoPreset(text: string, fileName: string) {
    if (this.state.caps.presets === false) {
      this.importApoToEq(text, fileName);
      return;
    }
    // The daemon stores text without filters as an empty preset; catch that here.
    try {
      if (parseApo(text, "x").profile.bands.length === 0) {
        this.toast(`No filters found in ${fileName}.`, "error");
        return;
      }
    } catch (e) {
      this.toast(`Could not read ${fileName}. ${errorText(e)}`, "error");
      return;
    }
    await this.queue.run(async () => {
      let name = presetNameFromFile(fileName);
      const taken = new Set(this.state.presets.map((p) => p.name));
      for (let i = 2; taken.has(name); i++) name = `${presetNameFromFile(fileName).slice(0, 58)} (${i})`;
      try {
        const before = (await this.freshConfig()).chain;
        const info = await this.call<PresetInfo>("import_apo", { name, text });
        this.upsertPreset(info);
        await this.call("load_preset", { name: info.name });
        await this.refreshConfig();
        await this.refreshPresets();
        this.afterPresetLoad();
        const after = this.state.config?.chain;
        const off: string[] = [];
        if (after) {
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
        await this.call("ab_select", { side, crossfade_ms: 10 });
        const status = await this.call<Status>("status");
        this.applyStatus(status);
      } catch (e) {
        this.toast(errorText(e), "error");
      }
    });
  }

  /** Side B: a stored preset, flat (no processing besides A's upsampling), or off. */
  async setAbSource(source: { preset: string } | "flat" | "off") {
    if (!this.state.config) return;
    if (source === "off") {
      await this.mutate(() => this.call("set_ab", null), "A/B compare is off");
      return;
    }
    await this.mutate(async () => {
      const cfg = await this.freshConfig();
      const level: LevelMatch = cfg.ab?.level ?? { mode: "analytic" };
      let chain: ChainConfig;
      if (source === "flat") {
        chain = { ...cfg.chain, eq: emptyEq("Flat"), convolution: [] };
      } else {
        const preset = await this.call<Preset>("get_preset", { name: source.preset });
        chain = preset.chain as ChainConfig;
        if (chain.eq) chain.eq = { ...chain.eq, name: source.preset };
        else chain.eq = emptyEq(source.preset);
      }
      // An engine without the IR chain takes one object or null.
      if (this.state.caps.convChain === false) (chain as { convolution: unknown }).convolution = convList(chain)[0] ?? null;
      const ab: AbConfig = { chain_b: chain, level };
      await this.call("set_ab", ab);
    });
  }

  async setLevelMatch(level: LevelMatch) {
    if (!this.state.config?.ab) return;
    await this.mutate(async () => {
      const ab = (await this.freshConfig()).ab;
      if (!ab) return;
      await this.call("set_ab", { ...ab, level });
      const status = await this.call<Status>("status").catch(() => null);
      if (status) this.applyStatus(status);
    });
  }

  // ------------------------------------------------------------ routing / device

  private async refreshStreams() {
    const streams = await this.call<StreamInfo[]>("list_streams").catch(() => null);
    if (streams) this.set({ streams });
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
    await this.editConfig((c) => ({ ...c, device: id }), "Output device changed");
    void this.refreshDevices();
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

export function initStore(transport: Transport): AppStore {
  storeInstance = new AppStore(transport);
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
