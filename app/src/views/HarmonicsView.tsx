import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { ChebyshevLayer, FluxLayer, HarmonicSign, HarmonicsBiasLimit, HarmonicsConfig, HarmonicsLayer, HarmonicsPreset, HarmonicsAlias, HarmonicsReadout, HarmonicsReadoutPoint, HarmonicsRouting, HarmonicsSystem, HarmonicsTilt, HysteresisLayer, OrderTilt, TiltShelf } from "../api/types";
import {
  BIAS_DEPTH,
  ENVELOPE_MS,
  SAG_DB,
  defaultBias,
  defaultSag,
  layerDynamic,
  withBias,
  withSag,
  FLUX_BOOST,
  FLUX_CAL,
  FLUX_CORNER,
  FLUX_H3,
  HARMONIC_DB,
  HARMONIC_DRIVE,
  HARMONIC_LAYERS_MAX,
  HYSTERESIS_ASYMMETRY,
  HYSTERESIS_CAL,
  HYSTERESIS_EQ_DB,
  HYSTERESIS_EQ_HZ,
  HYSTERESIS_H3,
  defaultHysteresisEq,
  hysteresisEqOn,
  LAYER_KINDS,
  TILT_DB,
  TILT_HIGH_DEFAULT_SLOPE,
  TILT_HIGH_HZ,
  TILT_LOW_DEFAULT_SLOPE,
  TILT_LOW_HZ,
  TILT_SLOPES,
  configFromSystem,
  defaultHarmonics,
  defaultTilt,
  firstAliasedFreq,
  freqCurves,
  harmonicsOn,
  harmonicsSeries,
  layerFromPreset,
  layerDiffers,
  layerModified,
  layerName,
  moveLayer,
  movedIndex,
  orderShelves,
  presetGroups,
  resetLayer,
  stageModified,
  stageSystem,
  sweepCurves,
  systemChain,
  systemLayers,
  withLayer,
  withOrderLevel,
  withOrderSign,
  withOrderTilt,
  withShelfSlope,
  withTilt,
} from "../lib/effects";
import { fmtDb, fmtFreq, fmtRate, minus } from "../lib/format";
import { errorText, store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { CompareButton, InlineError, MissingNotice, StageHead, useStageMissing } from "../components/StageParts";
import { ValueSlider } from "../components/ValueSlider";
import { ResizeHandle } from "../components/ResizeHandle";
import { usePersistentHeight, type PersistentHeightState } from "../lib/plotHeight";
import {
  HARM_PLOT_DEFAULT_HEIGHT,
  HARM_PLOT_HEIGHT_KEY,
  HARM_PLOT_MAX_HEIGHT,
  HARM_PLOT_MIN_HEIGHT,
  HARM_SPLIT_KEY,
  HARM_Y_FLOOR_DB,
  type HarmUnit,
  dbToPct,
  fmtPctNum,
  fmtPctValue,
  harmY,
  harmYTicks,
  loadHarmUnit,
  parsePctLimit,
  pctAxisPad,
  pctAxisTicks,
  pctTick,
  saveHarmUnit,
} from "../lib/harmonicsPlot";
import { bandColor } from "./eqColors";
import { SpectrumSection, useSpectrumState } from "./HarmonicsSpectrumPlot";
import { sameView, type ViewLimits, type ViewRange } from "../lib/plotView";
import { ZoomPlot, type PlotGeom } from "./ZoomPlot";
import { cssVar } from "../theme";
import { LayerExplain } from "./HarmonicsExplain";
import { LiveHistoryChart, LiveSpectrum, LiveStats, StatCard, shownFrame, useHarmonicsLive, useLiveGraphs, type LiveFeed } from "./HarmonicsLivePanel";
import type { HarmonicsLive } from "../api/types";
import { liveFreqs, liveHistory, musicWeights } from "../lib/harmonicsLive";
import { FollowButton, Followed } from "./HarmonicsFollow";
import { MusicSmoother, followStatus, levelGate, toneGate, type FollowStatus } from "../lib/harmonicsFollow";
import type { HarmGraphPrefs } from "../lib/graphPrefs";

type Edit = (fn: (c: HarmonicsConfig) => HarmonicsConfig, mode?: "drag" | "commit") => void;

const ORDINAL = ["2nd", "3rd", "4th", "5th", "6th", "7th", "8th", "9th"];
const ORDER_TITLE = [
  "An octave up: warmth, the classic triode sound.",
  "An octave and a fifth up: density and edge, the pentode sound.",
  "Two octaves up: more warmth on top of the 2nd.",
  "Two octaves and a major third up: more bite on top of the 3rd.",
  "Two octaves and a fifth up.",
  "Two octaves and a (flat) minor seventh up.",
  "Three octaves up.",
  "Three octaves and a whole tone up.",
];
/** What a negative sign does, per order (docs/control-plane.md). */
const SIGN_TITLE = (order: number): string => {
  const k = order + 2;
  if (k % 2 === 0) return "Which half-wave this order flattens; flipping it mirrors the curve's asymmetry.";
  return k === 3 || k === 7 ? "Negative (the usual) compresses at low and moderate levels; positive expands." : "Negative expands at low and moderate levels (the opposite of H3 and H7); positive compresses.";
};

/** 40 → "40 Hz", 1000 → "1 kHz". */
const fmtHz = (f: number) => (f >= 1000 ? `${fmtFreq(f).slice(0, -1)} kHz` : `${fmtFreq(f)} Hz`);
const fmtLevel = (db: number, unit: HarmUnit = "db") => (db <= -199 ? "none" : unit === "pct" ? fmtPctValue(db) : `${fmtDb(db, 1)} dB`);
const fmtAlias = (a: HarmonicsAlias, unit: HarmUnit) => `H${a.order} → ${fmtHz(a.freq_hz)} ${fmtLevel(a.level_db, unit)}`;
/** The aliasing fact's tooltip: what was measured, and where the loudest alias lands. */
const aliasingTitle = (r: HarmonicsReadout) =>
  `Measured: a 0 dBFS ${fmtHz(r.aliasing.tone_hz)} sine through the whole stage at ${fmtRate(r.rate_hz)}; every partial it folds back into 20 Hz–20 kHz, summed, relative to the fundamental.` +
  (r.aliasing.worst_db > -199 ? ` Loudest: ${fmtDb(r.aliasing.worst_db, 1)} dBc at ${fmtHz(r.aliasing.worst_hz)}.` : "") +
  " Warned above −60 dBc (below that it is far under audibility on music).";
/** Each point's folded orders at one level of the readout against frequency, one line per tone. */
const aliasedLines = (sweep: readonly HarmonicsReadoutPoint[] | undefined, levelDbfs: number, unit: HarmUnit) =>
  (sweep ?? [])
    .filter((p) => p.input_dbfs === levelDbfs && p.aliased?.length)
    .map((p) => `${fmtHz(p.freq_hz)}: ${p.aliased!.map((a) => fmtAlias(a, unit)).join(", ")}`)
    .join("\n");
/** ", loudest H5 of 20 kHz → 4 kHz at −58 dB" for the freq-plot note (empty when none). */
const loudestText = (sweep: readonly HarmonicsReadoutPoint[] | undefined, levelDbfs: number, unit: HarmUnit) => {
  let top: { p: HarmonicsReadoutPoint; a: HarmonicsAlias } | undefined;
  for (const p of sweep ?? []) if (p.input_dbfs === levelDbfs) for (const a of p.aliased ?? []) if (!top || a.level_db > top.a.level_db) top = { p, a };
  return top ? `; loudest H${top.a.order} of ${fmtHz(top.p.freq_hz)} → ${fmtHz(top.a.freq_hz)} at ${fmtLevel(top.a.level_db, unit)}` : "";
};
/**
 * The sine table in one line: "THD at 1 kHz / 40 Hz: −33.8 / −31.6 dB at
 * 0 dBFS, −51.9 / −51.9 dB at −18 dBFS" (and a note when orders fold back).
 */
export function pointsSummary(points: readonly HarmonicsReadoutPoint[], unit: HarmUnit): string {
  const freqs = [...new Set(points.map((p) => p.freq_hz))];
  const levels = [...new Set(points.map((p) => p.input_dbfs))];
  if (!freqs.length) return "No test sines.";
  const at = levels.map((l) => {
    const v = freqs.map((f) => points.find((p) => p.freq_hz === f && p.input_dbfs === l)?.thd_db ?? -200);
    const txt = unit === "pct" ? v.map((d) => (d <= -199 ? "none" : fmtPctValue(d))).join(" / ") : `${v.map((d) => (d <= -199 ? "none" : fmtDb(d, 1))).join(" / ")} dB`;
    return `${txt} at ${fmtDb(l, 0, false)} dBFS`;
  });
  const folds = points.some((p) => p.aliased?.length) ? " · some orders fold back" : "";
  return `THD at ${freqs.map(fmtHz).join(" / ")}: ${at.join(", ")}${folds}`;
}

const fmtThd = (db: number, unit: HarmUnit) => (db <= -199 ? "none" : unit === "pct" ? `${fmtPctValue(db)} · ${fmtDb(db, 1)} dB` : `${fmtDb(db, 1)} dB · ${fmtPctValue(db)}`);

/**
 * The engine's readouts of `config` (as if on) with the level sweep at
 * `sweepFreq` Hz and the sweep against frequency at `sweepLevel` dBFS,
 * refreshed shortly after each edit or slider move (latest wins).
 */
function useReadout(config: HarmonicsConfig, skip: boolean, sweepFreq: number, sweepLevel: number): { readout: HarmonicsReadout | null; error: string | null } {
  const st = store();
  const [readout, setReadout] = useState<HarmonicsReadout | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seq = useRef(0);
  const key = JSON.stringify([config, sweepFreq, sweepLevel]);
  useEffect(() => {
    if (skip) return;
    const my = ++seq.current;
    const t = setTimeout(() => {
      st.harmonicsReadout({ ...config, enabled: true }, sweepFreq, sweepLevel)
        .then((r) => {
          if (my !== seq.current) return;
          setReadout(r);
          setError(null);
        })
        .catch((e) => {
          if (my === seq.current) setError(errorText(e));
        });
    }, 150);
    return () => clearTimeout(t);
  }, [key, skip]);
  return { readout, error };
}

/** The sliders' ranges the followed values are kept within. */
const SPEC_LEVEL = { min: -60, max: 0 };
const SWEEP_LEVEL = { min: -40, max: 0 };

/**
 * Following the music: the live frames smoothed, and each followed slider
 * moved (in the store's graph settings, so the usual requests go out) when
 * the music has moved a grid step, at most once a second per slider. A
 * blind test freezes everything; nothing playing holds the last values.
 */
function useMusicFollow(frame: HarmonicsLive | null, blind: boolean): FollowStatus {
  const st = store();
  const followSpec = useApp((a) => a.harmGraph.followSpec);
  const followLevel = useApp((a) => a.harmGraph.followLevel);
  const followFreq = useApp((a) => a.harmGraph.followFreq);
  const sm = useRef(new MusicSmoother());
  const gates = useRef({ specLevel: levelGate(SPEC_LEVEL.min, SPEC_LEVEL.max), specTone: toneGate(), levelTone: toneGate(), freqLevel: levelGate(SWEEP_LEVEL.min, SWEEP_LEVEL.max) });
  const [, bump] = useState(0);
  const apply = (t: number) => {
    const p = st.getState().harmGraph;
    const g = gates.current;
    const m = sm.current;
    const patch: Partial<HarmGraphPrefs> = {};
    const put = <K extends "level" | "freq" | "sweepFreq" | "sweepLevel">(k: K, v: number | null) => {
      if (v !== null && v !== p[k]) patch[k] = v;
    };
    if (p.followSpec !== "off") put("level", g.specLevel.next(m.level, t));
    if (p.followSpec === "both") put("freq", g.specTone.next(m.tone, t));
    if (p.followLevel) put("sweepFreq", g.levelTone.next(m.tone, t));
    if (p.followFreq) put("sweepLevel", g.freqLevel.next(m.level, t));
    if (Object.keys(patch).length) st.setHarmGraph(patch);
  };
  // A follow turned off forgets what it sent; turned on, it takes the held values at once.
  useEffect(() => {
    const g = gates.current;
    if (followSpec === "off") g.specLevel.reset();
    if (followSpec !== "both") g.specTone.reset();
    if (!followLevel) g.levelTone.reset();
    if (!followFreq) g.freqLevel.reset();
    if (!blind) apply(Date.now());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [followSpec, followLevel, followFreq]);
  useEffect(() => {
    if (blind) return;
    const t = Date.now();
    if (sm.current.push(frame, t)) bump((v) => v + 1);
    if (sm.current.playing) apply(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [frame, blind]);
  return followStatus(sm.current, blind, frame?.state === "off");
}

export function HarmonicsView() {
  const st = store();
  useApp((s) => s.config?.chain.harmonics);
  useApp((s) => s.stageDrafts.harmonics);
  const presets = useApp((s) => s.harmonicsPresets);
  const systems = useApp((s) => s.harmonicsSystems);
  const missing = useStageMissing("harmonics");
  const cfg = st.stage("harmonics");
  const on = harmonicsOn(cfg);
  const shown: HarmonicsConfig = cfg ?? defaultHarmonics();
  const [open, setOpen] = useState<number | null>(0);
  const [picking, setPicking] = useState(false);
  const [pickingSystem, setPickingSystem] = useState(false);
  useEffect(() => {
    void st.loadHarmonicsPresets().catch(() => undefined);
    void st.loadHarmonicsSystems().catch(() => undefined);
  }, [st]);
  const sweepFreq = useApp((a) => a.harmGraph.sweepFreq);
  const sweepLevel = useApp((a) => a.harmGraph.sweepLevel);
  const setSweepFreq = (f: number) => st.setHarmGraph({ sweepFreq: f });
  const setSweepLevel = (l: number) => st.setHarmGraph({ sweepLevel: l });
  const { readout, error } = useReadout(shown, missing, sweepFreq, sweepLevel);
  const [unit, setUnitState] = useState<HarmUnit>(loadHarmUnit);
  const setUnit = (u: HarmUnit) => {
    setUnitState(u);
    saveHarmUnit(u);
  };

  const edit: Edit = (fn, mode = "commit") => st.editStage("harmonics", (c) => fn(c ?? { ...defaultHarmonics(), enabled: false }), mode);
  const end = () => st.endStageDrag("harmonics");
  const list = presets ?? [];
  const full = shown.layers.length >= HARMONIC_LAYERS_MAX;
  const asOn = useMemo(() => ({ ...shown, enabled: true }), [shown]);
  const spec = useSpectrumState(asOn, list, missing);
  // A blind test: the live view and its marks pause (they would give the side away).
  const blind = useApp((a) => a.blindRun?.phase === "running");
  const live = useHarmonicsLive(!missing && !blind);
  const nowFrame = !blind && live.frame?.state === "playing" ? live.frame : null;
  const liveFrame = shownFrame(live, blind);
  const livePlaying = liveFrame?.state === "playing" ? liveFrame : null;
  const liveG = useLiveGraphs();
  const follow = useMusicFollow(blind ? null : live.frame, blind);
  const followSpec = useApp((a) => a.harmGraph.followSpec);
  const followLevel = useApp((a) => a.harmGraph.followLevel);
  const followFreq = useApp((a) => a.harmGraph.followFreq);
  const plotH = usePersistentHeight({ storageKey: HARM_PLOT_HEIGHT_KEY, defaultHeight: HARM_PLOT_DEFAULT_HEIGHT, minHeight: HARM_PLOT_MIN_HEIGHT, maxHeight: HARM_PLOT_MAX_HEIGHT });
  // The other graphs' heights in the column, each its own (the slot on top has one of its own).
  const liveH = usePersistentHeight({ storageKey: LIVE_HEIGHT_KEY, defaultHeight: LIVE_DEFAULT_HEIGHT, minHeight: HARM_PLOT_MIN_HEIGHT, maxHeight: HARM_PLOT_MAX_HEIGHT });
  const historyH = usePersistentHeight({ storageKey: HISTORY_HEIGHT_KEY, defaultHeight: HISTORY_DEFAULT_HEIGHT, minHeight: HISTORY_MIN_HEIGHT, maxHeight: HARM_PLOT_MAX_HEIGHT });
  const specH = usePersistentHeight({ storageKey: SPECTRUM_HEIGHT_KEY, defaultHeight: SPECTRUM_DEFAULT_HEIGHT, minHeight: HARM_PLOT_MIN_HEIGHT, maxHeight: HARM_PLOT_MAX_HEIGHT });
  const inCol = (size: PersistentHeightState, fallback: number, label: string) => ({ height: size.height ?? fallback, resize: <PlotResize size={size} label={label} /> });
  const [primary, setPrimaryState] = useState<GraphId | null>(loadPrimary);
  const setPrimary = (id: GraphId | null) => {
    setPrimaryState(id);
    savePrimary(id);
  };
  const unitSeg = <UnitSeg unit={unit} setUnit={setUnit} />;
  /**
   * One graph, in the column (`slot` undefined: its own height and resize,
   * a button to put it on top) or in the slot on top (the slot's height and
   * resize handle). Its state lives here or in the store, so it carries over.
   */
  const graph = (id: GraphId, slot?: { height: number; resize: ReactNode }): ReactNode => {
    const head = slot ? undefined : <OnTopButton id={id} onClick={() => setPrimary(id)} />;
    const sweeps = () => slot ?? inCol(plotH, HARM_PLOT_DEFAULT_HEIGHT, "Resize the harmonics sweeps' height");
    switch (id) {
      case "live": {
        const { height, resize } = slot ?? inCol(liveH, LIVE_DEFAULT_HEIGHT, "Resize the live spectrum's height");
        return <LiveSpectrum frame={livePlaying} g={liveG} height={height} resize={resize} head={head} />;
      }
      case "history": {
        const { height, resize } = slot ?? inCol(historyH, HISTORY_DEFAULT_HEIGHT, "Resize the history's height");
        return <LiveHistoryChart frame={liveFrame} version={live.version} g={liveG} height={height} resize={resize} head={head} onTop={!!slot} />;
      }
      case "spectrum": {
        const { height, resize } = slot ?? inCol(specH, SPECTRUM_DEFAULT_HEIGHT, "Resize the distortion spectrum's height");
        return (
          <SpectrumSection
            s={spec}
            unit={unit}
            height={height}
            resize={resize}
            head={head}
            follow={{ mode: followSpec, status: follow, set: (m) => st.setHarmGraph({ followSpec: m }) }}
          />
        );
      }
      case "level":
        return readout?.sweep?.length ? <LevelSection readout={readout} sweepFreq={sweepFreq} setSweepFreq={setSweepFreq} unit={unit} {...sweeps()} head={head} now={nowFrame} follow={{ on: followLevel, status: follow, set: (v) => st.setHarmGraph({ followLevel: v }) }} /> : null;
      case "freq":
        return readout?.freq_sweep?.length ? <FreqSection readout={readout} sweepLevel={sweepLevel} setSweepLevel={setSweepLevel} unit={unit} {...sweeps()} head={head} now={nowFrame} follow={{ on: followFreq, status: follow, set: (v) => st.setHarmGraph({ followFreq: v }) }} /> : null;
    }
  };
  /** A graph in the column, in its card, or a one-line note while it is on top. */
  const inColumn = (id: GraphId): ReactNode => {
    if (primary === id) return <MovedNote id={id} onBack={() => setPrimary(null)} />;
    const node = graph(id);
    return node ? <div className="harm-card">{node}</div> : null;
  };
  /** What the slot shows when its graph has nothing to draw yet. */
  const slotEmpty = (id: GraphId): string | null => {
    if (id === "live" || id === "history") {
      if (blind) return "Hidden during the blind test: a live readout would give the side away.";
      if (id === "live" && !livePlaying && !liveG.last.current) return "Nothing has played since the page opened.";
      if (id === "history" && !liveFrame && liveHistory.points.length === 0) return "Nothing has played since the page opened.";
      return null;
    }
    if (id === "spectrum") return null;
    if (!readout) return error ? `Readouts unavailable: ${error}` : "Waiting for the engine's readouts…";
    return null;
  };

  const add = (p: HarmonicsPreset) => {
    const n = shown.layers.length;
    edit((c) => ({ ...c, layers: [...c.layers, layerFromPreset(p)] }));
    setOpen(n);
    setPicking(false);
  };
  const loadSystem = (s: HarmonicsSystem) => {
    edit((c) => configFromSystem(s, c.enabled));
    setOpen(null);
    setPickingSystem(false);
    setPicking(false);
  };
  const remove = (i: number) => {
    // The last layer gone: the stage starts from scratch, no system behind it.
    edit((c) => {
      const layers = c.layers.filter((_, j) => j !== i);
      return layers.length ? { ...c, layers } : { ...c, layers, system: undefined };
    });
    setOpen((o) => (o === null || o < i ? o : o === i ? null : o - 1));
  };
  const move = (i: number, to: number) => {
    edit((c) => moveLayer(c, i, to));
    setOpen((o) => movedIndex(o, i, to));
  };
  const series = harmonicsSeries(shown);
  const system = stageSystem(shown, systems);
  const bases = systemLayers(shown, system);
  const systemChanged = system ? stageModified(shown, system) : false;

  return (
    <section className="view" aria-labelledby="harm-title">
      <StageHead id="harm-title" title="Harmonics" part={missing ? undefined : "harmonics"} on={on} onPower={missing ? undefined : () => st.setStageEnabled("harmonics", !on)}>
        {!missing && <CompareButton stage="harmonics" />}
      </StageHead>
      {missing && <MissingNotice id="harmonics" />}
      {primary && !missing && (
        <PrimarySlot id={primary} onPick={setPrimary} unitSeg={primary === "spectrum" || primary === "level" || primary === "freq" ? unitSeg : null}>
          {(height, resize) => {
            const empty = slotEmpty(primary);
            return empty ? <p className="stage-note">{empty}</p> : graph(primary, { height, resize });
          }}
        </PrimarySlot>
      )}
      <HarmLayout
        editor={
          <div className="panel">
            <div className="harm-layers-head">
              <h2>Layers</h2>
              <Seg<HarmonicsRouting>
                label="How the layers combine"
                value={series ? "series" : "parallel"}
                disabled={missing}
                onChange={(r) => edit((c) => ({ ...c, routing: r }))}
                options={[
                  ["parallel", "Parallel", "Every layer hears the input; their harmonics are added together."],
                  ["series", "Series", "Each layer feeds the next, top to bottom, like a preamp into a tape machine."],
                ]}
              />
              <span className="hint">
                {series
                  ? "In series, top to bottom: each layer distorts the one before it at the level it puts out, so the harmonics multiply and the levels compound."
                  : "Added together; each one's levels are for a full-scale sine at drive 0 dB."}
              </span>
            </div>
            {system && (
              <div className="row harm-system-from">
                <span className="hint" title={`${system.gear}\n${system.sound}\n\nEach layer it holds is compared against the system's version of it; layers added by hand against their layer preset.`}>
                  From: <b>{system.name}</b>
                </span>
                {systemChanged && <span className="tag mod">Modified</span>}
                {systemChanged && (
                  <button type="button" className="btn small" disabled={missing} title={`Back to the ${system.name} system as it loads: its layers, settings and routing`} onClick={() => loadSystem(system)}>
                    Reset to system
                  </button>
                )}
              </div>
            )}
            {pickingSystem ? (
              <SystemPicker systems={systems ?? []} presets={list} loading={systems === null} layerCount={shown.layers.length} onPick={loadSystem} onCancel={() => setPickingSystem(false)} />
            ) : (
              (systems === null || systems.length > 0) && (
                <div className="row">
                  <button type="button" className="btn small" disabled={missing} title="Replace the whole stage with the layers of a typical piece of gear. Every layer stays editable." onClick={() => setPickingSystem(true)}>
                    Start from a system…
                  </button>
                </div>
              )
            )}
            {shown.layers.length === 0 && <p className="stage-note">No layers: the stage passes the sound through. Add one from a preset, or start from a system.</p>}
            {shown.layers.map((l, i) => (
              <LayerCard
                key={i}
                index={i}
                layer={l}
                base={bases[i]}
                presets={list}
                open={open === i}
                disabled={missing}
                onToggle={() => setOpen(open === i ? null : i)}
                onRemove={() => remove(i)}
                onMove={(to) => move(i, to)}
                last={i === shown.layers.length - 1}
                series={series}
                edit={edit}
                end={end}
                biasLimit={readout?.bias_limited?.find((b) => b.layer === i)}
                stage={shown}
              />
            ))}
            <InlineError slot="harmonics" />
            {picking ? (
              <PresetPicker presets={list} loading={presets === null} onPick={add} onCancel={() => setPicking(false)} />
            ) : (
              <div className="row">
                <button type="button" className="btn small" disabled={missing || full} title={full ? `A stage holds at most ${HARMONIC_LAYERS_MAX} layers.` : "Add a layer, starting from a preset."} onClick={() => setPicking(true)}>
                  + Add layer
                </button>
                <label className="check harm-dc" title="A 5 Hz high-pass on the even orders' sum, which otherwise adds a DC offset.">
                  <input type="checkbox" checked={shown.dc_block} disabled={missing} onChange={(e) => edit((c) => ({ ...c, dc_block: e.target.checked }))} />
                  <span>Remove DC from even orders</span>
                </label>
              </div>
            )}
          </div>
        }
        readouts={
          <AnalysisPanel
            readout={readout}
            error={error}
            on={on}
            config={shown}
            presets={list}
            unit={unit}
            unitSeg={unitSeg}
            live={missing ? null : live}
            blind={blind}
            liveSpectrum={missing || blind ? null : inColumn("live")}
            history={missing || blind ? null : inColumn("history")}
            spectrum={inColumn("spectrum")}
            level={inColumn("level")}
            freq={inColumn("freq")}
          />
        }
      />
    </section>
  );
}

/** Below this content width the page is one column (editor, then readouts). */
const TWO_COL_MIN = 1040;
const EDITOR_MIN = 520;
const READOUTS_MIN = 420;
const SPLIT_HANDLE = 12;

/**
 * Wide: the layer editor on the left, the graphs on the right, with a
 * draggable split; the page scrolls as one (no inner scroll area). The
 * editor stays in view while the graphs scroll, as long as it fits the
 * window. Narrow: one column.
 */
function HarmLayout({ editor, readouts }: { editor: React.ReactNode; readouts: React.ReactNode }) {
  const wrap = useRef<HTMLDivElement>(null);
  const edRef = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(0);
  const [viewH, setViewH] = useState(0);
  const [edH, setEdH] = useState(0);
  useEffect(() => {
    const el = wrap.current;
    const ed = edRef.current;
    if (!el || !ed) return;
    const scroller = el.closest(".main");
    const ro = new ResizeObserver(() => {
      setW(Math.round(el.getBoundingClientRect().width));
      setEdH(Math.round(ed.getBoundingClientRect().height));
      setViewH(viewportHeight(scroller));
    });
    ro.observe(el);
    ro.observe(ed);
    if (scroller) ro.observe(scroller);
    const onWin = () => setViewH(viewportHeight(scroller));
    window.addEventListener("resize", onWin);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", onWin);
    };
  }, []);
  const split = usePersistentHeight({ storageKey: HARM_SPLIT_KEY, defaultHeight: null, minHeight: EDITOR_MIN });
  const two = w >= TWO_COL_MIN;
  // The scroll area minus its padding.
  const sticky = two && viewH > 0 && edH > 0 && edH <= viewH - 32;
  const maxEd = Math.max(EDITOR_MIN, w - READOUTS_MIN - SPLIT_HANDLE);
  const ed = split.height === null ? null : Math.min(split.height, maxEd);
  const style = two
    ? { gridTemplateColumns: ed === null ? `minmax(${EDITOR_MIN}px, 5fr) ${SPLIT_HANDLE}px minmax(${READOUTS_MIN}px, 6fr)` : `${ed}px ${SPLIT_HANDLE}px minmax(${READOUTS_MIN}px, 1fr)` }
    : undefined;
  const measure = () => edRef.current?.getBoundingClientRect().width ?? EDITOR_MIN;
  return (
    <div ref={wrap} className={"harm-layout" + (two ? " two" : "")} style={style}>
      <div className={"harm-editor" + (sticky ? " sticky" : "")} ref={edRef}>
        {editor}
      </div>
      {two && (
        <ResizeHandle
          axis="x"
          label="Resize the editor and graphs columns"
          value={ed}
          min={EDITOR_MIN}
          max={maxEd}
          onChange={split.setHeight}
          onCommit={split.commitHeight}
          onStep={split.stepHeight}
          onReset={split.resetHeight}
          measure={measure}
        />
      )}
      <div className="harm-side">{readouts}</div>
    </div>
  );
}

type GraphId = "live" | "history" | "spectrum" | "level" | "freq";
const GRAPHS: [GraphId, string, string][] = [
  ["live", "Live spectrum", "The music playing and what the stage adds to it"],
  ["history", "History", "The added level over time"],
  ["spectrum", "Distortion spectrum", "An FFT of a test sine through the stage"],
  ["level", "Against level", "Each harmonic against the test sine's level"],
  ["freq", "Against frequency", "THD and each harmonic against the test sine's frequency"],
];
const graphName = (id: GraphId) => GRAPHS.find((g) => g[0] === id)![1];
const PRIMARY_KEY = "avdsp-harm-primary";
const PRIMARY_FILL_KEY = "avdsp-harm-primary-fill";
const PRIMARY_HEIGHT_KEY = "avdsp-height-harm-primary";
const PRIMARY_DEFAULT_HEIGHT = 560;
const LIVE_HEIGHT_KEY = "avdsp-height-harm-live";
const LIVE_DEFAULT_HEIGHT = 380;
const HISTORY_HEIGHT_KEY = "avdsp-height-harm-history";
/** The whole history chart: the main plot, the music and sag strips, the time axis. */
const HISTORY_DEFAULT_HEIGHT = 380;
const HISTORY_MIN_HEIGHT = 220;
const SPECTRUM_HEIGHT_KEY = "avdsp-height-harm-spectrum";
const SPECTRUM_DEFAULT_HEIGHT = 420;

/** The height a graph can fill: the scroll area's, or the window's when the page itself scrolls. */
const viewportHeight = (scroller: Element | null) => Math.min(scroller?.clientHeight ?? Infinity, window.innerHeight);
const PRIMARY_MIN_HEIGHT = 180;

function loadPrimary(): GraphId | null {
  try {
    const v = localStorage.getItem(PRIMARY_KEY);
    return GRAPHS.some((g) => g[0] === v) ? (v as GraphId) : null;
  } catch {
    return null;
  }
}

function savePrimary(id: GraphId | null): void {
  try {
    if (id) localStorage.setItem(PRIMARY_KEY, id);
    else localStorage.removeItem(PRIMARY_KEY);
  } catch {
    /* storage unavailable */
  }
}

function loadFill(): boolean {
  try {
    return localStorage.getItem(PRIMARY_FILL_KEY) === "1";
  } catch {
    return false;
  }
}

function saveFill(v: boolean): void {
  try {
    localStorage.setItem(PRIMARY_FILL_KEY, v ? "1" : "0");
  } catch {
    /* storage unavailable */
  }
}

function UnitSeg({ unit, setUnit }: { unit: HarmUnit; setUnit: (u: HarmUnit) => void }) {
  return (
    <Seg<HarmUnit>
      label="Harmonic level unit"
      className="harm-unit"
      value={unit}
      onChange={setUnit}
      options={[
        ["db", "dB", "Levels in dB relative to the fundamental"],
        ["pct", "%", "Levels in percent of the fundamental (log axis, as distortion analysers plot them)"],
      ]}
    />
  );
}

/** A graph header's button: put this graph in the slot on top of the page. */
function OnTopButton({ id, onClick }: { id: GraphId; onClick: () => void }) {
  return (
    <button type="button" className="btn small" onClick={onClick} title={`Put the ${graphName(id).toLowerCase()} on top of the page, full width, at a height you set`}>
      ⤒ On top
    </button>
  );
}

/** Where a graph was, while it is on top. */
function MovedNote({ id, onBack }: { id: GraphId; onBack: () => void }) {
  return (
    <p className="stage-note harm-moved">
      <span>{graphName(id)}: on top of the page.</span>
      <button type="button" className="btn small" onClick={onBack}>
        Back here
      </button>
    </p>
  );
}

/**
 * The slot on top of the page: one graph, full width, with a picker to
 * switch graphs or empty the slot, a draggable height and a toggle that
 * fits the slot to the window.
 */
function PrimarySlot({ id, onPick, unitSeg, children }: { id: GraphId; onPick: (id: GraphId | null) => void; unitSeg: ReactNode; children: (height: number, resize: ReactNode) => ReactNode }) {
  const size = usePersistentHeight({ storageKey: PRIMARY_HEIGHT_KEY, defaultHeight: PRIMARY_DEFAULT_HEIGHT, minHeight: PRIMARY_MIN_HEIGHT, maxHeight: HARM_PLOT_MAX_HEIGHT * 2 });
  const [fill, setFillState] = useState(loadFill);
  const setFill = (v: boolean) => {
    setFillState(v);
    saveFill(v);
  };
  const ref = useRef<HTMLDivElement>(null);
  // Everything in the slot but the plot (header, controls, legend), and the window's height.
  const [chrome, setChrome] = useState(0);
  const [viewH, setViewH] = useState(0);
  const base = size.height ?? PRIMARY_DEFAULT_HEIGHT;
  const h = fill && viewH > 0 && chrome > 0 ? Math.max(PRIMARY_MIN_HEIGHT, viewH - 32 - chrome) : base;
  const shownH = useRef(h);
  shownH.current = h;
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const scroller = el.closest(".main");
    const ro = new ResizeObserver(() => {
      setChrome(Math.max(0, Math.round(el.getBoundingClientRect().height) - shownH.current));
      setViewH(viewportHeight(scroller));
    });
    ro.observe(el);
    if (scroller) ro.observe(scroller);
    const onWin = () => setViewH(viewportHeight(scroller));
    window.addEventListener("resize", onWin);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", onWin);
    };
  }, []);
  const resize = (
    <ResizeHandle
      label="Resize the graph on top"
      value={h}
      min={PRIMARY_MIN_HEIGHT}
      max={size.maxHeight}
      onChange={(v) => {
        if (fill) setFill(false);
        size.setHeight(v);
      }}
      onCommit={(v) => {
        if (fill) setFill(false);
        size.commitHeight(v);
      }}
      onStep={(d) => {
        if (fill) setFill(false);
        size.commitHeight(h + d);
      }}
      onReset={() => {
        if (fill) setFill(false);
        size.resetHeight();
      }}
    />
  );
  return (
    <div className="panel harm-primary" ref={ref}>
      <div className="harm-primary-head">
        <Seg<GraphId> label="Graph on top" value={id} onChange={onPick} options={GRAPHS} />
        <div className="spacer" />
        {unitSeg}
        <button
          type="button"
          className="btn small"
          aria-pressed={fill}
          title="Fit the graph to the window's height (dragging the handle under it sets a height of your own)"
          onClick={() => {
            setFill(!fill);
            if (!fill) ref.current?.scrollIntoView({ block: "start", behavior: "smooth" });
          }}
        >
          Fill window
        </button>
        <button type="button" className="btn small" title="Put the graph back in its panel and empty the slot" aria-label="Empty the slot on top" onClick={() => onPick(null)}>
          ✕
        </button>
      </div>
      {children(h, resize)}
    </div>
  );
}

function LayerCard({
  index,
  layer,
  base,
  presets,
  open,
  disabled,
  onToggle,
  onRemove,
  onMove,
  last,
  series,
  edit,
  end,
  biasLimit,
  stage,
}: {
  index: number;
  layer: HarmonicsLayer;
  /** The system's version of this layer (the stage came from a system that holds it): the baseline instead of the preset. */
  base?: HarmonicsLayer;
  presets: HarmonicsPreset[];
  open: boolean;
  disabled: boolean;
  onToggle: () => void;
  onRemove: () => void;
  onMove: (to: number) => void;
  last: boolean;
  series: boolean;
  edit: Edit;
  end: () => void;
  biasLimit?: HarmonicsBiasLimit;
  /** The whole stage (the tape explainer traces the layer through the engine). */
  stage: HarmonicsConfig;
}) {
  const modified = base ? layerDiffers(layer, base) : layerModified(layer, presets);
  const preset = layer.preset ? presets.find((p) => p.id === layer.preset) : undefined;
  const name = layerName(layer, presets);
  const bodyId = `harm-layer-${index}`;
  return (
    <div className={"harm-layer" + (layer.enabled ? "" : " off")}>
      <div className="harm-layer-head">
        <label className="check" title={layer.enabled ? "Turn this layer off (kept, not applied)" : "Turn this layer on"}>
          <input type="checkbox" checked={layer.enabled} disabled={disabled} onChange={(e) => edit((c) => withLayer(c, index, (l) => ({ ...l, enabled: e.target.checked })))} />
          <span className="sr-only">{name} on</span>
        </label>
        <button type="button" className="harm-layer-name" aria-expanded={open} aria-controls={bodyId} onClick={onToggle} title={preset?.source}>
          <span className="name">{name}</span>
          <span className="tag">{LAYER_KINDS[layer.kind].tag}</span>
          {layerDynamic(layer) && (
            <span className="tag" title="Sag or bias on: the curve's operating point moves with the music.">
              Dynamic
            </span>
          )}
          {modified && <span className="tag mod">Modified</span>}
        </button>
        {preset && <span className="hint harm-ref" title="What 0 dBFS at drive 0 dB stands for on the device.">{preset.reference}</span>}
        <div className="spacer" />
        {modified && (
          <button
            type="button"
            className="btn small"
            disabled={disabled}
            title={base ? "Back to this layer's settings in the system the stage started from" : `Back to the ${preset?.name ?? ""} preset's settings`}
            onClick={() => edit((c) => withLayer(c, index, (l) => (base ? ({ ...structuredClone(base), enabled: l.enabled } as HarmonicsLayer) : resetLayer(l, presets))))}
          >
            {base ? "Reset to system" : "Reset to preset"}
          </button>
        )}
        <span className="harm-move">
          <button type="button" className="btn small" disabled={disabled || index === 0} aria-label={`Move ${name} up`} title={series ? "Move up: earlier in the chain" : "Move up (the order matters in series)"} onClick={() => onMove(index - 1)}>
            ↑
          </button>
          <button type="button" className="btn small" disabled={disabled || last} aria-label={`Move ${name} down`} title={series ? "Move down: later in the chain" : "Move down (the order matters in series)"} onClick={() => onMove(index + 1)}>
            ↓
          </button>
        </span>
        <button type="button" className="btn small danger" disabled={disabled} title="Remove this layer" onClick={onRemove}>
          Remove
        </button>
      </div>
      {open && (
        <div className="harm-layer-body" id={bodyId}>
          {layer.kind === "chebyshev" ? (
            <ChebyshevEditor index={index} layer={layer} disabled={disabled} edit={edit} end={end} biasLimit={biasLimit} />
          ) : layer.kind === "flux" ? (
            <FluxEditor index={index} layer={layer} disabled={disabled} edit={edit} end={end} />
          ) : (
            <HysteresisEditor index={index} layer={layer} disabled={disabled} edit={edit} end={end} />
          )}
          <LayerExplain layer={layer} index={index} stage={stage} />
        </div>
      )}
    </div>
  );
}

function ChebyshevEditor({ index, layer, disabled, edit, end, biasLimit }: { index: number; layer: ChebyshevLayer; disabled: boolean; edit: Edit; end: () => void; biasLimit?: HarmonicsBiasLimit }) {
  const st = store();
  const set = (patch: Partial<ChebyshevLayer>, mode: "drag" | "commit" = "commit") => edit((c) => withLayer(c, index, (l) => ({ ...l, ...patch }) as HarmonicsLayer), mode);
  return (
    <>
      {layer.levels_db.map((v, k) => {
        const present = v !== null && v !== undefined;
        const id = `harm-${index}-h${k + 2}`;
        return (
          <div className="row harm-order" key={k}>
            <div className="k harm-k">
              <label htmlFor={id} title={`${ORDER_TITLE[k]} Level relative to the fundamental for a full-scale sine at drive 0 dB, −120 to −20 dB.`}>
                H{k + 2} <span className="hint">{ORDINAL[k]}</span>
              </label>
              <label className="check" title={present ? `Turn H${k + 2} off` : `Turn H${k + 2} on`}>
                <input type="checkbox" checked={present} disabled={disabled} onChange={(e) => st.setHarmonicOrder(index, k, e.target.checked)} />
                <span className="sr-only">H{k + 2} on</span>
              </label>
            </div>
            <ValueSlider
              id={id}
              label={`H${k + 2} level`}
              value={v ?? -70}
              min={HARMONIC_DB.min}
              max={HARMONIC_DB.max}
              step={0.5}
              disabled={disabled || !present}
              format={(x) => (present ? `${fmtDb(x, 1)} dB` : "off")}
              title="−120 to −20 dB below the fundamental"
              onChange={(x, mode) => edit((c) => withOrderLevel(c, index, k, x), mode)}
              onEnd={end}
            />
            <Seg<HarmonicSign>
              label={`H${k + 2} sign`}
              className="harm-sign"
              value={layer.signs[k]}
              disabled={disabled || !present}
              onChange={(s) => edit((c) => withOrderSign(c, index, k, s))}
              options={[
                ["+", "+", SIGN_TITLE(k)],
                ["-", minus("-"), SIGN_TITLE(k)],
              ]}
            />
          </div>
        );
      })}
      <div className="row">
        <span className="k" title="Flips every even order's sign at once: which half-wave is flattened.">
          Polarity
        </span>
        <Seg<"normal" | "inverted">
          label="Even-order polarity"
          value={layer.invert_even ? "inverted" : "normal"}
          disabled={disabled}
          onChange={(m) => set({ invert_even: m === "inverted" })}
          options={[
            ["normal", "Normal", "Even orders as signed above."],
            ["inverted", "Even inverted", "Every even order's sign flipped."],
          ]}
        />
      </div>
      <div className="row">
        <label className="k" htmlFor={`harm-${index}-drive`} title="Where a full-scale sine sits on the curve, below the preset's reference. Lower drive moves down the curve: each harmonic H_k falls by about k − 1 dB per dB.">
          Drive
        </label>
        <ValueSlider id={`harm-${index}-drive`} label="Drive" value={layer.drive_db} min={HARMONIC_DRIVE.min} max={HARMONIC_DRIVE.max} step={0.5} disabled={disabled} format={(x) => `${fmtDb(x, 1)} dB`} onChange={(x, mode) => set({ drive_db: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={`harm-${index}-amount`} title="Scales the layer's distortion terms: 100 % is the levels as set, 0 % none.">
          Amount
        </label>
        <ValueSlider
          id={`harm-${index}-amount`}
          label="Amount"
          value={Math.round(layer.amount * 100)}
          min={0}
          max={100}
          step={1}
          disabled={disabled}
          format={(x) => `${x.toFixed(0)} %`}
          onChange={(x, mode) => set({ amount: x / 100 }, mode)}
          onEnd={end}
        />
      </div>
      <TiltEditor index={index} layer={layer} disabled={disabled} edit={edit} end={end} />
      <DynamicsEditor index={index} layer={layer} disabled={disabled} edit={edit} end={end} biasLimit={biasLimit} />
    </>
  );
}

/** 0.5 → "0.5 ms", 250 → "250 ms", 2000 → "2.0 s". */
const fmtMs = (ms: number) => (ms >= 1000 ? `${(ms / 1000).toFixed(ms >= 10000 ? 0 : 1)} s` : ms >= 10 ? `${Math.round(ms)} ms` : `${ms.toFixed(1)} ms`);
/** A bias depth (fraction of full scale) as a signed percentage. */
const fmtBias = (d: number) => (d === 0 ? "off" : `${d > 0 ? "+" : minus("-")}${Math.abs(100 * d).toFixed(1)} %`);

/** An envelope's attack and release: two log slider rows. */
function EnvelopeRows({ id, name, attack, release, disabled, onChange, end }: { id: string; name: string; attack: number; release: number; disabled: boolean; onChange: (attack: number, release: number, mode: "drag" | "commit") => void; end: () => void }) {
  return (
    <>
      <div className="row">
        <label className="k" htmlFor={`${id}-att`} title={`How fast the ${name.toLowerCase()} follows a rise in level (time constant).`}>
          {name} attack
        </label>
        <ValueSlider id={`${id}-att`} label={`${name} attack`} value={attack} min={ENVELOPE_MS.min} max={ENVELOPE_MS.max} step={0.1} log disabled={disabled} format={fmtMs} onChange={(x, mode) => onChange(x, release, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={`${id}-rel`} title={`How fast the ${name.toLowerCase()} recovers when the level falls (time constant).`}>
          {name} release
        </label>
        <ValueSlider id={`${id}-rel`} label={`${name} release`} value={release} min={ENVELOPE_MS.min} max={ENVELOPE_MS.max} step={0.1} log disabled={disabled} format={fmtMs} onChange={(x, mode) => onChange(attack, x, mode)} onEnd={end} />
      </div>
    </>
  );
}

/** Supply sag and bias shift: the curve's operating point moving with the music. */
function DynamicsEditor({ index, layer, disabled, edit, end, biasLimit }: { index: number; layer: ChebyshevLayer; disabled: boolean; edit: Edit; end: () => void; biasLimit?: HarmonicsBiasLimit }) {
  const sag = layer.sag ?? defaultSag();
  const bias = layer.bias ?? defaultBias();
  const id = (n: string) => `harm-${index}-${n}`;
  return (
    <div className="harm-tilt">
      <div className="side-label" title="The operating point on the curve moves with the music, as a tube stage's supply and bias do. Off at depth 0.">
        Under load
      </div>
      <div className="row">
        <label className="k" htmlFor={id("sag")} title="Supply sag: loud passages lower the drive into the curve by up to this much (times the amount), so they lean toward the lower orders. Sag layers share one supply: the level is turned down once, by the deepest sag. 0 = off.">
          Sag
        </label>
        <ValueSlider id={id("sag")} label="Sag depth" value={sag.depth_db} min={SAG_DB.min} max={SAG_DB.max} step={0.1} disabled={disabled} format={(x) => (x === 0 ? "off" : `${minus("-")}${x.toFixed(1)} dB`)} onChange={(x, mode) => edit((c) => withSag(c, index, (s) => ({ ...s, depth_db: x })), mode)} onEnd={end} />
      </div>
      <EnvelopeRows id={id("sag")} name="Sag" attack={sag.attack_ms} release={sag.release_ms} disabled={disabled || sag.depth_db === 0} onChange={(attack_ms, release_ms, mode) => edit((c) => withSag(c, index, (s) => ({ ...s, attack_ms, release_ms })), mode)} end={end} />
      <div className="row">
        <label className="k" htmlFor={id("bias")} title="Bias shift: sustained loud passages move the operating point up (+) or down (−) the curve, so odd orders turn partly into even ones (times the amount). The level never changes. 0 = off.">
          Bias shift
        </label>
        <ValueSlider id={id("bias")} label="Bias depth" value={Math.round(bias.depth * 1000) / 10} min={100 * BIAS_DEPTH.min} max={100 * BIAS_DEPTH.max} step={0.5} disabled={disabled} format={(x) => fmtBias(x / 100)} onChange={(x, mode) => edit((c) => withBias(c, index, (b) => ({ ...b, depth: x / 100 })), mode)} onEnd={end} />
      </div>
      {biasLimit && (
        <div className="notice warn" role="status">
          Bias limited to {fmtBias(biasLimit.reach)} of {fmtBias(biasLimit.depth)} (the depth times the amount): further out, this curve's harmonics would raise its worst-case peak by more than 1 dB.
        </div>
      )}
      <EnvelopeRows id={id("bias")} name="Bias" attack={bias.attack_ms} release={bias.release_ms} disabled={disabled || bias.depth === 0} onChange={(attack_ms, release_ms, mode) => edit((c) => withBias(c, index, (b) => ({ ...b, attack_ms, release_ms })), mode)} end={end} />
    </div>
  );
}

const fmtRise = (db: number) => (db === 0 ? "off" : `${fmtDb(db, 1)} dB`);

const SLOPE_TITLE =
  "How steep the rise can get, in dB per octave (the shelf's filter order × 6). It is the limit the slope approaches: a rise of a few dB never gets that steep; a higher setting then makes the same rise over fewer octaves, closer to the corner. A 24 dB rise reaches about 88% of it.";

/** A shelf's rise, corner and slope: two slider rows and a slope picker. */
function ShelfRows({
  id,
  name,
  shelf,
  range,
  defaultSlope,
  cornerTitle,
  riseTitle,
  disabled,
  onChange,
  end,
}: {
  id: string;
  name: string;
  shelf: TiltShelf;
  range: { min: number; max: number };
  defaultSlope: number;
  cornerTitle: string;
  riseTitle: string;
  disabled: boolean;
  onChange: (s: TiltShelf, mode: "drag" | "commit") => void;
  end: () => void;
}) {
  const slope = shelf.slope ?? defaultSlope;
  return (
    <>
      <div className="row">
        <label className="k" htmlFor={`${id}-db`} title={riseTitle}>
          {name} rise
        </label>
        <ValueSlider id={`${id}-db`} label={`${name} rise`} value={shelf.db} min={TILT_DB.min} max={TILT_DB.max} step={0.5} disabled={disabled} format={fmtRise} onChange={(x, mode) => onChange({ ...shelf, db: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={`${id}-hz`} title={cornerTitle}>
          {name} corner
        </label>
        <ValueSlider id={`${id}-hz`} label={`${name} corner`} value={shelf.hz} min={range.min} max={range.max} step={1} log disabled={disabled || shelf.db === 0} format={fmtHz} onChange={(x, mode) => onChange({ ...shelf, hz: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <span className="k" title={SLOPE_TITLE}>
          {name} slope
        </span>
        <Seg<number>
          label={`${name} slope`}
          value={slope}
          disabled={disabled || shelf.db === 0}
          onChange={(v) => onChange(withShelfSlope(shelf, v, defaultSlope), "commit")}
          options={TILT_SLOPES.map((v) => [v, `${v}`, `Up to ${v} dB per octave${v === defaultSlope ? " (default)" : ""}`] as [number, string, string])}
        />
        <span className="hint">dB/oct</span>
      </div>
    </>
  );
}

const LOW_RISE_TITLE = "How far the harmonics rise toward the bass (below the corner, at up to the slope, until they reach this), as a transformer core nearing saturation does. Negative falls instead. Levels hold at 1 kHz.";
const LOW_CORNER_TITLE = "Below this fundamental frequency the harmonics start to rise.";
const HIGH_RISE_TITLE = "How far the harmonics rise toward the treble (above the corner, at up to the slope, until they reach this), as in an amplifier whose feedback falls with frequency. Negative falls instead. Levels hold at 1 kHz.";
const HIGH_CORNER_TITLE = "Above this fundamental frequency the harmonics start to rise.";

/** The layer's frequency tilt: bass and treble shelves, and per-order overrides under "Advanced". */
function TiltEditor({ index, layer, disabled: off, edit, end }: { index: number; layer: ChebyshevLayer; disabled: boolean; edit: Edit; end: () => void }) {
  const t: HarmonicsTilt = layer.tilt ?? defaultTilt();
  const setTilt = (fn: (t: HarmonicsTilt) => HarmonicsTilt, mode: "drag" | "commit" = "commit") => edit((c) => withTilt(c, index, fn), mode);
  const id = (n: string) => `harm-${index}-tilt-${n}`;
  const overrides = (t.orders ?? []).filter((o) => o).length;
  const disabled = off;
  return (
    <div className="harm-tilt">
      <div className="side-label" title="Harmonic levels that change with the fundamental's frequency. Small signals keep a flat response.">
        Against frequency
      </div>
      <ShelfRows id={id("low")} name="Bass" shelf={t.low} range={TILT_LOW_HZ} defaultSlope={TILT_LOW_DEFAULT_SLOPE} cornerTitle={LOW_CORNER_TITLE} riseTitle={LOW_RISE_TITLE} disabled={disabled} onChange={(low, mode) => setTilt((x) => ({ ...x, low }), mode)} end={end} />
      <div className="row">
        <span className="k" />
        <label className="check" title="By default the bass rise applies to the odd orders only, as core saturation (symmetrical) adds mostly odd harmonics.">
          <input type="checkbox" checked={!!t.low_even} disabled={disabled || t.low.db === 0} onChange={(e) => setTilt((x) => ({ ...x, low_even: e.target.checked }))} />
          <span>Even orders too</span>
        </label>
      </div>
      <ShelfRows id={id("high")} name="Treble" shelf={t.high} range={TILT_HIGH_HZ} defaultSlope={TILT_HIGH_DEFAULT_SLOPE} cornerTitle={HIGH_CORNER_TITLE} riseTitle={HIGH_RISE_TITLE} disabled={disabled} onChange={(high, mode) => setTilt((x) => ({ ...x, high }), mode)} end={end} />
      <details className="harm-advanced">
        <summary>
          Advanced: per-order tilt{overrides > 0 && <span className="hint"> · {overrides} overridden</span>}
        </summary>
        <p className="stage-note">An order with its own tilt ignores the layer's (its bass rise applies whatever its order).</p>
        {layer.levels_db.map((v, i) => {
          const k = i + 2;
          const own = t.orders?.[i] ?? null;
          const setOwn = (o: OrderTilt | null, mode: "drag" | "commit" = "commit") => setTilt((x) => withOrderTilt(x, k, o), mode);
          return (
            <div className="harm-order-tilt" key={k}>
              <label className="check" title={own ? `H${k} back to the layer's tilt` : `Give H${k} its own tilt, starting from what it gets now`}>
                <input type="checkbox" checked={own !== null} disabled={disabled} onChange={(e) => setOwn(e.target.checked ? orderShelves(t, k) : null)} />
                <span>
                  H{k} own tilt{v === null || v === undefined ? <span className="hint"> (order off)</span> : null}
                </span>
              </label>
              {own && (
                <>
                  <ShelfRows id={id(`h${k}-low`)} name={`H${k} bass`} shelf={own.low} range={TILT_LOW_HZ} defaultSlope={TILT_LOW_DEFAULT_SLOPE} cornerTitle={LOW_CORNER_TITLE} riseTitle={LOW_RISE_TITLE} disabled={disabled} onChange={(low, mode) => setOwn({ ...own, low }, mode)} end={end} />
                  <ShelfRows id={id(`h${k}-high`)} name={`H${k} treble`} shelf={own.high} range={TILT_HIGH_HZ} defaultSlope={TILT_HIGH_DEFAULT_SLOPE} cornerTitle={HIGH_CORNER_TITLE} riseTitle={HIGH_RISE_TITLE} disabled={disabled} onChange={(high, mode) => setOwn({ ...own, high }, mode)} end={end} />
                </>
              )}
            </div>
          );
        })}
      </details>
    </div>
  );
}

function FluxEditor({ index, layer, disabled, edit, end }: { index: number; layer: FluxLayer; disabled: boolean; edit: Edit; end: () => void }) {
  const set = (patch: Partial<FluxLayer>, mode: "drag" | "commit" = "commit") => edit((c) => withLayer(c, index, (l) => ({ ...l, ...patch }) as HarmonicsLayer), mode);
  const id = (n: string) => `harm-${index}-${n}`;
  return (
    <>
      <p className="stage-note">Core saturation: H3 (and other odd orders) rising about 12 dB per octave below the corner and 2 dB per dB of level; small signals pass flat.</p>
      <div className="row">
        <label className="k" htmlFor={id("h3")} title="H3 relative to the fundamental for a full-scale sine at the calibration frequency.">
          H3 level
        </label>
        <ValueSlider id={id("h3")} label="H3 level" value={layer.target_h3_db} min={FLUX_H3.min} max={FLUX_H3.max} step={0.5} disabled={disabled} format={(x) => `${fmtDb(x, 1)} dB`} onChange={(x, mode) => set({ target_h3_db: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("cal")} title="The frequency the H3 level is set at.">
          At frequency
        </label>
        <ValueSlider id={id("cal")} label="Calibration frequency" value={layer.calibration_hz} min={FLUX_CAL.min} max={FLUX_CAL.max} step={1} log disabled={disabled} format={fmtHz} onChange={(x, mode) => set({ calibration_hz: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("corner")} title="Below this frequency the core's flux, and so the distortion, rises.">
          Corner
        </label>
        <ValueSlider id={id("corner")} label="Corner frequency" value={layer.corner_hz} min={FLUX_CORNER.min} max={FLUX_CORNER.max} step={1} log disabled={disabled} format={fmtHz} onChange={(x, mode) => set({ corner_hz: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("boost")} title="How far the flux rises below the corner before it levels off (as a winding's resistance limits it).">
          Max rise
        </label>
        <ValueSlider id={id("boost")} label="Maximum rise" value={layer.max_boost_db} min={FLUX_BOOST.min} max={FLUX_BOOST.max} step={0.5} disabled={disabled} format={(x) => `${fmtDb(x, 1, false)} dB`} onChange={(x, mode) => set({ max_boost_db: x }, mode)} onEnd={end} />
      </div>
    </>
  );
}

function HysteresisEditor({ index, layer, disabled, edit, end }: { index: number; layer: HysteresisLayer; disabled: boolean; edit: Edit; end: () => void }) {
  const set = (patch: Partial<HysteresisLayer>, mode: "drag" | "commit" = "commit") => edit((c) => withLayer(c, index, (l) => ({ ...l, ...patch }) as HarmonicsLayer), mode);
  const id = (n: string) => `harm-${index}-${n}`;
  // Switched off, the EQ keeps its values (shown greyed) for turning it back on.
  const eq = layer.bass_eq ?? { ...defaultHysteresisEq(), enabled: false };
  return (
    <>
      <p className="stage-note">Tape magnetic hysteresis (Jiles-Atherton): loop pinning and soft saturation across levels; small signals pass flat. Best at 176.4 kHz (4×) or higher.</p>
      <div className="row">
        <label className="k" htmlFor={id("h3")} title="H3 relative to the fundamental for a full-scale sine at the calibration frequency (at 100 % amount).">
          H3 level
        </label>
        <ValueSlider id={id("h3")} label="H3 level" value={layer.target_h3_db} min={HYSTERESIS_H3.min} max={HYSTERESIS_H3.max} step={0.5} disabled={disabled} format={(x) => `${fmtDb(x, 1)} dB`} onChange={(x, mode) => set({ target_h3_db: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("cal")} title="The frequency the H3 level is calibrated at.">
          At frequency
        </label>
        <ValueSlider id={id("cal")} label="Calibration frequency" value={layer.calibration_hz} min={HYSTERESIS_CAL.min} max={HYSTERESIS_CAL.max} step={1} log disabled={disabled} format={fmtHz} onChange={(x, mode) => set({ calibration_hz: x }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("width")} title="Magnetic loop width: 0 % is anhysteretic Langevin saturation, 100 % is a wide pinning loop.">
          Width
        </label>
        <ValueSlider
          id={id("width")}
          label="Loop width"
          value={Math.round(layer.width * 100)}
          min={0}
          max={100}
          step={1}
          disabled={disabled}
          format={(x) => `${x.toFixed(0)} %`}
          onChange={(x, mode) => set({ width: x / 100 }, mode)}
          onEnd={end}
        />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("sat")} title="Lower values emphasize Rayleigh pinning across programme levels; higher values push deeper into the soft saturation knee near full scale.">
          Saturation
        </label>
        <ValueSlider
          id={id("sat")}
          label="Saturation character"
          value={Math.round(layer.saturation * 100)}
          min={0}
          max={100}
          step={1}
          disabled={disabled}
          format={(x) => `${x.toFixed(0)} %`}
          onChange={(x, mode) => set({ saturation: x / 100 }, mode)}
          onEnd={end}
        />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("amount")} title="Scales the layer's distortion residual: 100 % is the calibrated level, 0 % none.">
          Amount
        </label>
        <ValueSlider
          id={id("amount")}
          label="Amount"
          value={Math.round(layer.amount * 100)}
          min={0}
          max={100}
          step={1}
          disabled={disabled}
          format={(x) => `${x.toFixed(0)} %`}
          onChange={(x, mode) => set({ amount: x / 100 }, mode)}
          onEnd={end}
        />
      </div>
      <div className="row">
        <span className="k" />
        <label className="check" title="A record/playback EQ pair around the magnetic core: bass goes into the core boosted and the distortion leaves through the exact inverse, so the music stays flat and only the distortion rises toward the bass, as on a real tape machine.">
          <input type="checkbox" checked={hysteresisEqOn(eq)} disabled={disabled} onChange={(e) => set({ bass_eq: { ...eq, enabled: e.target.checked, db: eq.db === 0 ? defaultHysteresisEq().db : eq.db } })} />
          <span>Bass EQ (distortion rises toward the bass)</span>
        </label>
      </div>
      <div className="row">
        <label className="k" htmlFor={id("eq-db")} title="How much the bass is boosted into the core (12 dB per octave below the corner, until it reaches this). H3 at 40 Hz rises about three times this.">
          Bass boost
        </label>
        <ValueSlider id={id("eq-db")} label="Bass boost into the core" value={eq.db} min={HYSTERESIS_EQ_DB.min} max={HYSTERESIS_EQ_DB.max} step={0.5} disabled={disabled || eq.enabled === false} format={(x) => `${fmtDb(x, 1, false)} dB`} onChange={(x, mode) => set({ bass_eq: { ...eq, db: x } }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("eq-hz")} title="Below this frequency the bass is boosted into the core.">
          Bass corner
        </label>
        <ValueSlider id={id("eq-hz")} label="Bass EQ corner" value={eq.hz} min={HYSTERESIS_EQ_HZ.min} max={HYSTERESIS_EQ_HZ.max} step={1} log disabled={disabled || !hysteresisEqOn(eq)} format={fmtHz} onChange={(x, mode) => set({ bass_eq: { ...eq, hz: x } }, mode)} onEnd={end} />
      </div>
      <div className="row">
        <label className="k" htmlFor={id("asym")} title="A DC offset of the magnetic field (as a misaligned bias leaves it), in percent of the core's saturation knee: adds H2 and the other even orders, with a phase that holds across levels. 0 is symmetric; the sign flips the H2 polarity.">
          Asymmetry
        </label>
        <ValueSlider
          id={id("asym")}
          label="Field asymmetry"
          value={Math.round((layer.asymmetry ?? 0) * 100)}
          min={HYSTERESIS_ASYMMETRY.min * 100}
          max={HYSTERESIS_ASYMMETRY.max * 100}
          step={1}
          disabled={disabled}
          format={(x) => `${x > 0 ? "+" : ""}${x.toFixed(0)} %`}
          onChange={(x, mode) => set({ asymmetry: x / 100 }, mode)}
          onEnd={end}
        />
      </div>
    </>
  );
}

function PresetPicker({ presets, loading, onPick, onCancel }: { presets: HarmonicsPreset[]; loading: boolean; onPick: (p: HarmonicsPreset) => void; onCancel: () => void }) {
  const groups = presetGroups(presets);
  return (
    <div className="harm-picker" role="group" aria-label="Add a layer from a preset">
      <div className="harm-picker-head">
        <span>Add a layer from a preset</span>
        <div className="spacer" />
        <button type="button" className="btn small" onClick={onCancel}>
          Cancel
        </button>
      </div>
      {loading && <p className="stage-note">Loading presets…</p>}
      {!loading && groups.length === 0 && <p className="stage-note">This engine lists no presets.</p>}
      {groups.map((g) => (
        <div key={g.origin} className="harm-picker-group">
          <div className="side-label" title={g.title}>
            {g.label}
          </div>
          {g.presets.map((p) => (
            <button key={p.id} type="button" className="harm-preset" title={p.source} onClick={() => onPick(p)}>
              <span className="name">{p.name}</span>
              <span className="tag">{LAYER_KINDS[p.layer.kind].tag}</span>
              <span className="ref">{p.reference}</span>
            </button>
          ))}
        </div>
      ))}
    </div>
  );
}

/**
 * Pick a system preset (a whole stage). Picking one over a stage that has
 * layers asks first, since it replaces them.
 */
function SystemPicker({ systems, presets, loading, layerCount, onPick, onCancel }: { systems: HarmonicsSystem[]; presets: HarmonicsPreset[]; loading: boolean; layerCount: number; onPick: (s: HarmonicsSystem) => void; onCancel: () => void }) {
  const [pending, setPending] = useState<HarmonicsSystem | null>(null);
  const pick = (s: HarmonicsSystem) => (layerCount > 0 ? setPending(s) : onPick(s));
  return (
    <div className="harm-picker" role="group" aria-label="Start from a system">
      <div className="harm-picker-head">
        <span>Start from a system</span>
        <div className="spacer" />
        <button type="button" className="btn small" onClick={onCancel}>
          Cancel
        </button>
      </div>
      {pending && (
        <div className="notice warn harm-system-confirm" role="alert">
          <span>
            Replace the {layerCount === 1 ? "layer" : `${layerCount} layers`} with {pending.name}?
          </span>
          <button type="button" className="btn small" onClick={() => onPick(pending)}>
            Replace
          </button>
          <button type="button" className="btn small" onClick={() => setPending(null)}>
            Keep mine
          </button>
        </div>
      )}
      {loading && <p className="stage-note">Loading systems…</p>}
      {!loading && systems.length === 0 && <p className="stage-note">This engine lists no systems.</p>}
      {systems.map((s) => (
        <button key={s.id} type="button" className={"harm-system" + (pending?.id === s.id ? " on" : "")} aria-pressed={pending?.id === s.id} onClick={() => pick(s)}>
          <span className="name">{s.name}</span>
          <span className="chain">{systemChain(s, presets)}</span>
          <span className="desc">{s.gear}</span>
          <span className="desc">{s.sound}</span>
        </button>
      ))}
    </div>
  );
}

/**
 * The page's one analysis panel: a header (the rate, the dB/% toggle), the
 * numbers in two groups of cards (now, on the music playing; on the
 * engine's test signals), then the graphs in cards under two subheadings
 * (the music playing; a test sine).
 */
function AnalysisPanel({
  readout,
  error,
  on,
  config,
  presets,
  unit,
  unitSeg,
  live,
  blind,
  liveSpectrum,
  history,
  spectrum,
  level,
  freq,
}: {
  readout: HarmonicsReadout | null;
  error: string | null;
  on: boolean;
  config: HarmonicsConfig;
  presets: HarmonicsPreset[];
  unit: HarmUnit;
  unitSeg: ReactNode;
  /** The live feed; null without a stage to analyse. */
  live: LiveFeed | null;
  blind: boolean;
  liveSpectrum: ReactNode;
  history: ReactNode;
  spectrum: ReactNode;
  level: ReactNode;
  freq: ReactNode;
}) {
  const layers = config.layers;
  const named = (i: number) => (layers[i] ? `${layerName(layers[i], presets)} (layer ${i + 1})` : `Layer ${i + 1}`);
  const rate = readout?.rate_hz ?? (live?.frame && live.frame.rate > 0 ? live.frame.rate : null);
  return (
    <div className="panel harm-analysis">
      <div className="harm-panel-head">
        <h2>Analysis</h2>
        {rate !== null && (
          <span className="hint" title="The rate the stage runs at: the live analysis and the computed readouts both use it.">
            {fmtRate(rate)}
          </span>
        )}
        <div className="spacer" />
        {unitSeg}
      </div>
      {live?.error && <div className="notice warn">Live analysis unavailable: {live.error}</div>}
      {error && <div className="notice warn">Readouts unavailable: {error}</div>}
      {readout?.aliasing.warn && (
        <div className="notice warn" role="status">
          Aliasing: a 0 dBFS {fmtHz(readout.aliasing.tone_hz)} tone folds {fmtDb(readout.aliasing.level_db, 1)} dBc back into the audio band at {fmtRate(readout.rate_hz)} (loudest {fmtDb(readout.aliasing.worst_db, 1)} dBc at {fmtHz(readout.aliasing.worst_hz)}).
          {(readout.below_clean_rate ?? []).map((b) => ` ${named(b.layer)} aliases most below ${fmtRate(b.clean_rate_hz)}.`).join("")} Upsample further to lower it.
        </div>
      )}
      {(readout?.bias_limited ?? []).map((b) => (
        <div className="notice warn" role="status" key={`bias-${b.layer}`}>
          {named(b.layer)}: bias limited to {fmtBias(b.reach)} of {fmtBias(b.depth)} (further out it would raise the peak bound by more than 1 dB).
        </div>
      ))}
      <div className="harm-stat-groups">
        {live && (
          <section className="harm-stat-group now" aria-label="Now, on the music playing">
            <div className="harm-group-k" title="Measured on the music playing now: the side playing, both channels summed, over about the last second.">
              <i className="harm-live-dot" aria-hidden />
              Now, on the music
            </div>
            <LiveStats live={live} blind={blind} unit={unit} readout={readout} />
          </section>
        )}
        {readout && (
          <section className="harm-stat-group sine" aria-label="On test signals">
            <div className="harm-group-k" title={`Computed by the engine for its test signals (sines; loud pink noise for the loudness)${on ? "" : ", as if the stage were on"}.`}>
              <span className="harm-sine-glyph" aria-hidden>
                ∿
              </span>
              On test signals{on ? "" : ", as if the stage were on"}
            </div>
            <div className="harm-stat-cards">
              <StatCard k="Small-signal gain" v={`${fmtDb(readout.small_signal_gain_db, 2)} dB`} sub="quiet signals" title="Gain for quiet signals." />
              <StatCard k="Peak gain" v={`${fmtDb(readout.peak_gain_db, 2)} dB`} sub="worst case, 0 dBFS in" title="How far over full scale the stage can take a full-scale input, at worst. The stage is not turned down for it: the limiter and the output headroom handle what goes over." />
              <StatCard k="Aliasing" v={readout.aliasing.level_db <= -199 ? "none" : `${fmtDb(readout.aliasing.level_db, 1)} dBc`} sub={`${fmtHz(readout.aliasing.tone_hz)}, 0 dBFS`} title={aliasingTitle(readout)} />
              <StatCard k="Loudness change" v={`${fmtDb(readout.level_change_db, 2)} dB`} sub="loud pink noise, A/B" title="The stage's loudness change on loud pink noise (BS.1770): what A/B level matching counts for it. The live level change above is the music's, now." />
            </div>
          <details className="harm-points-more">
            <summary title="The fixed test sines: THD, GedLee Gm, H2, H3 and the loudest of H10–H15. Click for the table.">{pointsSummary(readout.points, unit)}</summary>
            <div className="table-wrap">
              <table className="harm-points">
                <thead>
                  <tr>
                    <th>Sine</th>
                    <th title="Total harmonic distortion (H2–H15 below Nyquist) relative to the fundamental.">THD</th>
                    <th title="GedLee metric: weights the curve's higher-order bending, which is heard more than low-order distortion. Below about 1 is usually inaudible.">Gm</th>
                    <th>H2</th>
                    <th>H3</th>
                    <th title="The loudest of H10–H15: series cross terms and tape layers reach past H9.">H10–H15</th>
                  </tr>
                </thead>
                <tbody>
                  {readout.points.map((p) => (
                    <Fragment key={`${p.freq_hz}-${p.input_dbfs}`}>
                      <tr className={p.aliased?.length ? "has-alias" : undefined}>
                        <td>
                          {fmtHz(p.freq_hz)}, {fmtDb(p.input_dbfs, 0, false)} dBFS
                        </td>
                        <td className="mono">{fmtThd(p.thd_db, unit)}</td>
                        <td className="mono">{p.gedlee_gm.toFixed(3)}</td>
                        <td className="mono">{fmtLevel(p.harmonics_db[0], unit)}</td>
                        <td className="mono">{fmtLevel(p.harmonics_db[1], unit)}</td>
                        <HighOrdersCell levels={p.harmonics_db} unit={unit} />
                      </tr>
                      {p.aliased?.length ? (
                        <tr className="harm-alias-row">
                          <td colSpan={6} className="hint" title="Orders above half the output rate fold back to these frequencies: aliasing, left out of THD.">
                            Folds back: {p.aliased.map((a) => fmtAlias(a, unit)).join(", ")}
                          </td>
                        </tr>
                      ) : null}
                    </Fragment>
                  ))}
                </tbody>
              </table>
            </div>
          </details>
          </section>
        )}
      </div>
      {(liveSpectrum || history) && (
        <>
          <h3 className="harm-subhead">The music playing</h3>
          {liveSpectrum}
          {history}
        </>
      )}
      {readout && (
        <>
          <h3 className="harm-subhead">A test sine</h3>
          {spectrum}
          {(level || freq) && (
            <div className="harm-plots">
              {level}
              {freq}
            </div>
          )}
        </>
      )}
    </div>
  );
}

interface FollowProps {
  on: boolean;
  status: FollowStatus;
  set: (on: boolean) => void;
}

/** The level sweep: its tone slider over the plot. */
function LevelSection({ readout, sweepFreq, setSweepFreq, unit, height, resize, head, now, follow }: { readout: HarmonicsReadout; sweepFreq: number; setSweepFreq: (f: number) => void; unit: HarmUnit; height: number; resize: ReactNode; head?: ReactNode; now: HarmonicsLive | null; follow: FollowProps }) {
  // The sweep as the engine last answered (the slider may be ahead of it).
  const shownFreq = readout.sweep?.[0]?.freq_hz ?? sweepFreq;
  return (
    <div className="harm-plot-section">
      <div className="row harm-graph-head">
        <span className="k harm-graph-title" title="Each harmonic's level relative to the fundamental, for a sine from −40 to 0 dBFS. With sag or bias: solid at rest, dashed under load (a steady sine after the envelopes settle).">
          Against level
        </span>
        <label className="k" htmlFor="harm-sweep-freq" title="The test sine's frequency.">
          Tone
        </label>
        <Followed on={follow.on}>
          <ValueSlider
            id="harm-sweep-freq"
            label="Test tone of the level sweep"
            value={sweepFreq}
            min={20}
            max={20000}
            step={1}
            log
            format={(f) => fmtHz(f)}
            onChange={(f) => {
              if (follow.on) follow.set(false);
              setSweepFreq(Math.round(f));
            }}
          />
        </Followed>
        <FollowButton on={follow.on} status={follow.status} what="its spectral centre" onClick={() => follow.set(!follow.on)} />
        <div className="spacer" />
        {head}
      </div>
      <LevelPlot readout={readout} freq={shownFreq} unit={unit} height={height} resize={resize} now={now} />
    </div>
  );
}

/** The frequency sweep: its level slider over the plot. */
function FreqSection({ readout, sweepLevel, setSweepLevel, unit, height, resize, head, now, follow }: { readout: HarmonicsReadout; sweepLevel: number; setSweepLevel: (l: number) => void; unit: HarmUnit; height: number; resize: ReactNode; head?: ReactNode; now: HarmonicsLive | null; follow: FollowProps }) {
  const shownLevel = readout.freq_sweep?.[0]?.input_dbfs ?? sweepLevel;
  return (
    <div className="harm-plot-section">
      <div className="row harm-graph-head">
        <span className="k harm-graph-title" title="THD and each harmonic relative to the fundamental, for sines from 20 Hz to 20 kHz.">
          Against frequency
        </span>
        <label className="k" htmlFor="harm-sweep-level" title="The test sine's level.">
          Level
        </label>
        <Followed on={follow.on}>
          <ValueSlider
            id="harm-sweep-level"
            label="Sine level of the frequency sweep"
            value={sweepLevel}
            min={SWEEP_LEVEL.min}
            max={SWEEP_LEVEL.max}
            step={1}
            format={(l) => `${fmtDb(l, 0, false)} dBFS`}
            onChange={(l) => {
              if (follow.on) follow.set(false);
              setSweepLevel(l);
            }}
          />
        </Followed>
        <FollowButton on={follow.on} status={follow.status} what="its level" onClick={() => follow.set(!follow.on)} />
        <div className="spacer" />
        {head}
      </div>
      <FreqPlot readout={readout} levelDbfs={shownLevel} unit={unit} height={height} resize={resize} now={now} />
    </div>
  );
}

const PL = 50;
const PR = 10;
const PT = 10;
const PB = 20;

/** Width of a plot's container, followed as it resizes. */
function useWidth(): [React.RefObject<HTMLDivElement | null>, number] {
  const wrap = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(480);
  useEffect(() => {
    const el = wrap.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setW(Math.max(200, Math.round(el.getBoundingClientRect().width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [wrap, w];
}

/** Horizontal grid lines and labels of the level axis (dB or log %). */
function YAxis({ unit, w, h }: { unit: HarmUnit; w: number; h: number }) {
  const ticks = harmYTicks(unit, h - PT - PB);
  const suffix = unit === "pct" ? " %" : " dB";
  return (
    <>
      {ticks.map((t, i) => {
        const y = harmY(unit, t.db, PT, h - PB);
        return (
          <g key={"y" + t.db.toFixed(3)}>
            <line x1={PL} x2={w - PR} y1={y} y2={y} className={"mp-grid" + (t.major ? "" : " minor")} />
            {t.label !== null && (
              <text x={PL - 6} y={y} className="mp-text" textAnchor="end" dominantBaseline="middle">
                {minus(t.label) + (i === 0 ? suffix : "")}
              </text>
            )}
          </g>
        );
      })}
    </>
  );
}

const floorText = (unit: HarmUnit) => (unit === "pct" ? fmtPctValue(HARM_Y_FLOOR_DB.pct) : `${minus(String(HARM_Y_FLOOR_DB.db))} dB`);

/** The drag handle under a plot: one height shared by both plots. */
function PlotResize({ size, label }: { size: PersistentHeightState; label: string }) {
  return (
    <ResizeHandle
      label={label}
      value={size.height}
      min={size.minHeight}
      max={size.maxHeight}
      onChange={size.setHeight}
      onCommit={size.commitHeight}
      onStep={size.stepHeight}
      onReset={size.resetHeight}
    />
  );
}

/** The loudest of H10–H15 in a readout point (`levels` is H2..H15), with its order; every level in the tooltip. */
function HighOrdersCell({ levels, unit }: { levels: readonly number[]; unit: HarmUnit }) {
  const high = levels.slice(8).map((db, i) => ({ k: i + 10, db }));
  const top = high.reduce((a, b) => (b.db > a.db ? b : a), high[0]);
  if (!top || top.db <= -199) return <td className="mono">-</td>;
  return (
    <td className="mono" title={high.map((h) => `H${h.k} ${fmtLevel(h.db, unit)}`).join("\n")}>
      {fmtLevel(top.db, unit)} <span className="hint">H{top.k}</span>
    </td>
  );
}

/** The level plot's home: −40..0 dBFS, from 10 dB over the loudest curve down to 10 dB under the quietest (−200 at most). */
export function levelHome(curves: readonly (readonly number[])[]): ViewRange {
  let lo = Infinity;
  let hi = -Infinity;
  for (const c of curves)
    for (const v of c)
      if (v > -199) {
        lo = Math.min(lo, v);
        hi = Math.max(hi, v);
      }
  if (!Number.isFinite(lo)) return { x0: -40, x1: 0, y0: -140, y1: 0 };
  return { x0: -40, x1: 0, y0: Math.max(-200, Math.floor(lo / 10) * 10 - 10), y1: Math.min(10, Math.ceil(hi / 10) * 10 + 10) };
}

const LEVEL_LIMITS: ViewLimits = { x0: -40, x1: 0, y0: -220, y1: 20, minSpanX: 1, minSpanY: 1 };

/** The orders hidden from both sweep plots (1: THD), shared through the store, and a toggle. */
function useHiddenOrders(): [Set<number>, (k: number) => void] {
  const hidden = useApp((a) => a.harmGraph.hidden);
  const set = useMemo(() => new Set(hidden), [hidden]);
  const toggle = (k: number) => store().setHarmGraph({ hidden: set.has(k) ? hidden.filter((h) => h !== k) : [...hidden, k] });
  return [set, toggle];
}

/** A plot legend whose entries hide or show their curve on click. */
function ToggleLegend({ items, hidden, onToggle }: { items: { k: number; label: string; colour: string }[]; hidden: Set<number>; onToggle: (k: number) => void }) {
  return (
    <>
      {items.map((c) => (
        <button key={c.k} type="button" className="legend-toggle" aria-pressed={!hidden.has(c.k)} title={hidden.has(c.k) ? `Show ${c.label}` : `Hide ${c.label}`} onClick={() => onToggle(c.k)}>
          <i style={{ background: c.colour }} />
          {c.label}
        </button>
      ))}
    </>
  );
}

/** The y axis props of a zoom plot in dB or log %. */
function yAxisProps(unit: HarmUnit, y0: number, padL: number) {
  if (unit === "db") return { yTick: (v: number, step: number) => minus(v.toFixed(step < 1 ? 1 : 0)), padL };
  return { yTicks: pctAxisTicks, yTick: (v: number) => pctTick(v), yLimitText: (v: number) => `${fmtPctNum(dbToPct(v))} %`, parseY: parsePctLimit, padL: pctAxisPad(y0, padL) };
}

/** H2–H15 (dBc) against input level (dBFS) from the readout's sweep, zoomable like the spectrum. */
function LevelPlot({ readout, freq, unit, height: h, resize, now }: { readout: HarmonicsReadout; freq: number; unit: HarmUnit; height: number; resize: ReactNode; now: HarmonicsLive | null }) {
  const { levels, orders } = sweepCurves(readout.sweep, freq);
  // Under load (sag/bias): the same tones simulated with the envelopes settled.
  const load = readout.under_load?.length ? sweepCurves(readout.under_load, freq) : null;
  const [hidden, toggle] = useHiddenOrders();
  const all = orders.map((db, k) => ({ k: k + 2, label: `H${k + 2}`, db, colour: bandColor(k), width: k + 2 > 9 ? 1 : 2 })).filter((c) => c.db.some((v) => v > -199));
  const curves = all.filter((c) => !hidden.has(c.k));
  const home = useMemo(() => levelHome([...orders, ...(load?.orders ?? [])]), [orders, load]);
  // The zoom lives in the store (null: fitted to the curves).
  const stored = useApp((a) => a.harmGraph.levelView);
  const view = stored ?? home;
  const setView = (v: ViewRange) => store().setHarmGraph({ levelView: sameView(v, home) ? null : v });
  if (levels.length < 2) return null;
  // How much quieter a full-scale sine comes out under load (the sag).
  const gainAt = (pts: HarmonicsReadout["sweep"]) => pts?.find((p) => p.freq_hz === freq && p.input_dbfs === 0)?.gain_db;
  const restGain = gainAt(readout.sweep);
  const loadGain = gainAt(readout.under_load);
  const sagDb = restGain !== undefined && loadGain !== undefined ? restGain - loadGain : 0;
  const draw = (ctx: CanvasRenderingContext2D, g: PlotGeom) => {
    const line = (db: number[], at: number[], colour: string, width: number, dash: number[]) => {
      ctx.strokeStyle = colour;
      ctx.lineWidth = width;
      ctx.setLineDash(dash);
      ctx.beginPath();
      db.forEach((v, i) => {
        const x = g.xOf(at[i]);
        const y = Math.min(g.top + g.h + 50, g.yOf(Math.max(-250, v)));
        if (i) ctx.lineTo(x, y);
        else ctx.moveTo(x, y);
      });
      ctx.stroke();
    };
    for (const c of curves) line(c.db, levels, c.colour, c.width, []);
    if (load && load.levels.length > 1) for (const c of curves) line(load.orders[c.k - 2], load.levels, c.colour, 1.5, [5, 3]);
    ctx.setLineDash([]);
    // You are here: the music's level now (its sine-equivalent level, the
    // sweep's scale), at the slider's tone.
    if (now && now.music_level_dbfs >= g.view.x0 - 20) {
      const at = Math.min(g.view.x1, Math.max(g.view.x0, now.music_level_dbfs));
      const xm = Math.round(g.xOf(at)) + 0.5;
      ctx.save();
      ctx.strokeStyle = cssVar("--accent");
      ctx.globalAlpha = 0.85;
      ctx.lineWidth = 1.5;
      ctx.setLineDash([3, 3]);
      ctx.beginPath();
      ctx.moveTo(xm, g.top);
      ctx.lineTo(xm, g.top + g.h);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.fillStyle = cssVar("--accent");
      ctx.font = "600 10.5px " + cssVar("--mono");
      ctx.textAlign = xm > g.left + g.w - 170 ? "right" : "left";
      ctx.textBaseline = "top";
      ctx.fillText(now.music_level_dbfs < g.view.x0 ? "◂ now: the music's level" : "now: the music's level", xm + (ctx.textAlign === "right" ? -4 : 4), g.top + 4);
      ctx.restore();
    }
  };
  const fmtV = (db: number) => (db <= -199 ? "none" : unit === "pct" ? fmtPctValue(db) : `${fmtDb(db, 1)} dB`);
  const readoutAt = (x: number) => {
    // The nearest swept level: its loudest three orders.
    let i = 0;
    for (let j = 1; j < levels.length; j++) if (Math.abs(levels[j] - x) < Math.abs(levels[i] - x)) i = j;
    const top = curves
      .map((c) => ({ k: c.k, db: c.db[i] }))
      .sort((a, b) => b.db - a.db)
      .slice(0, 3);
    return { x: levels[i], y: top[0]?.db ?? null, text: `${fmtDb(levels[i], 0, false)} dBFS  ${top.map((t) => `H${t.k} ${fmtV(t.db)}`).join("  ")}` };
  };
  const { padL, ...yAxis } = yAxisProps(unit, view.y0, 48);
  return (
    <div className="miniplot">
      <ZoomPlot
        ariaLabel={`Harmonic levels against input level at ${fmtHz(freq)}${load ? ", at rest and under load" : ""}`}
        className="ups-canvas-wrap harm-level-plot"
        style={{ height: h }}
        scale="lin"
        view={view}
        home={home}
        limits={LEVEL_LIMITS}
        onView={setView}
        freqs={[]}
        mags={[]}
        mode="box"
        boxAxis="xy"
        drawCurve={draw}
        readout={readoutAt}
        xTick={(v) => (v === 0 ? "0 dBFS" : minus(String(Math.round(v * 10) / 10)))}
        {...yAxis}
        xMinStep={1}
        pad={{ l: padL, r: 14, t: 12, b: 24 }}
      />
      {resize}
      {all.length > 0 ? (
        <div className="legend mp-legend">
          <ToggleLegend items={all} hidden={hidden} onToggle={toggle} />
          <span>
            {fmtHz(freq)} · {unit === "pct" ? "% of" : "dB relative to"} the fundamental
          </span>
          {now && (
            <span title={`The dotted line is the music playing now, at its level (${fmtDb(now.music_level_dbfs, 1, false)} dBFS: RMS + 3 dB, the level of a sine of the same power). The curves are for a single ${fmtHz(freq)} sine at each level, not for the music: read them at the line to see what a ${fmtHz(freq)} tone at the music's level would get.`}>
              <i className="dash" style={{ color: "var(--accent)" }} />
              now: the music's level, {fmtDb(now.music_level_dbfs, 0, false)} dBFS; the curves are a {fmtHz(freq)} sine
            </span>
          )}
          {load && (
            <>
              <span title="Sag and bias idle: a note that starts after a quiet passage.">
                <i className="harm-key-line" />
                at rest
              </span>
              <span title="A steady sine after the sag and bias have settled on it: a sustained loud passage.">
                <i className="dash harm-key-line" />
                under load
              </span>
            </>
          )}
        </div>
      ) : (
        <p className="stage-note">No harmonics at this tone.</p>
      )}
      {all.length > 0 && <p className="harm-plot-help">click an order to hide it · drag to zoom · wheel or drag over an axis · double-click to reset</p>}
      {load && Math.abs(sagDb) >= 0.05 && (
        <p className="stage-note" title="Under load: a steady sine after the sag envelope settles. The sag lowers the drive, so loud passages come out quieter.">
          Under load, a full-scale sine comes out {fmtDb(sagDb, 2, false)} dB quieter than at rest.
        </p>
      )}
    </div>
  );
}

const F_MIN = 20;
const F_MAX = 20000;

/**
 * THD and H2–H15 (dBc) against frequency at one sine level, from the
 * readout's `freq_sweep`. An order's line ends where it lands above Nyquist
 * (it folds back: not a harmonic of the tone there).
 */
function FreqPlot({ readout, levelDbfs, unit, height: h, resize, now }: { readout: HarmonicsReadout; levelDbfs: number; unit: HarmUnit; height: number; resize: ReactNode; now: HarmonicsLive | null }) {
  const [wrap, w] = useWidth();
  const floor = HARM_Y_FLOOR_DB[unit];
  const { freqs, thd, orders, above, gain } = freqCurves(readout.freq_sweep, levelDbfs);
  const [hidden, toggle] = useHiddenOrders();
  if (freqs.length < 2) return null;
  const x = (f: number) => PL + (Math.log(f / F_MIN) / Math.log(F_MAX / F_MIN)) * (w - PL - PR);
  const y = (db: number) => harmY(unit, db, PT, h - PB);
  const all = [
    { k: 1, label: "THD", db: thd, gap: thd.map(() => false), colour: "var(--fg)", width: 2.5 },
    ...orders.map((db, i) => ({ k: i + 2, label: `H${i + 2}`, db, gap: above[i], colour: bandColor(i), width: i + 2 > 9 ? 1 : 1.5 })),
  ].filter((c) => c.db.some((v, i) => v > floor && !c.gap[i]));
  const curves = all.filter((c) => !hidden.has(c.k));
  const xTicks = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
  // Breaks the line where the order is above Nyquist.
  const path = (db: number[], gap: boolean[]) => {
    let d = "";
    let pen = false;
    db.forEach((v, i) => {
      if (gap[i]) {
        pen = false;
        return;
      }
      d += `${pen ? "L" : "M"}${x(freqs[i]).toFixed(1)},${y(v).toFixed(1)}`;
      pen = true;
    });
    return d;
  };
  // How far a full-scale fundamental's level moves across the band (odd
  // orders' compression follows their tilt).
  const spread = gain.length ? Math.max(...gain) - Math.min(...gain) : 0;
  const aliasedFrom = firstAliasedFreq(readout.freq_sweep, levelDbfs);
  // You are here: where the music's energy is now, shaded (half-bin columns of the live spectrum).
  const shade = (() => {
    if (!now || now.music.length === 0) return [];
    const fs = liveFreqs(now, now.music.length);
    const wts = musicWeights(now.music, fs);
    const half = Math.pow(2, 0.5 / now.bins_per_octave);
    return fs
      .map((f, i) => ({ x0: x(Math.max(F_MIN, f / half)), x1: x(Math.min(F_MAX, f * half)), w: wts[i] }))
      .filter((r) => r.w > 0 && r.x1 > r.x0);
  })();
  return (
    <div className="miniplot" ref={wrap}>
      <svg width={w} height={h} role="img" aria-label={`THD and H2 to H15 against frequency at ${fmtDb(levelDbfs, 0, false)} dBFS`}>
        <rect x={PL} y={PT} width={Math.max(0, w - PL - PR)} height={h - PT - PB} className="mp-bg" />
        {shade.map((r, i) => (
          <rect key={"s" + i} x={r.x0} width={r.x1 - r.x0} y={PT} height={h - PT - PB} className="harm-now-shade" style={{ opacity: 0.28 * r.w }} />
        ))}
        {now && now.centroid_hz >= F_MIN && now.centroid_hz <= F_MAX && <line x1={x(now.centroid_hz)} x2={x(now.centroid_hz)} y1={PT} y2={h - PB} className="harm-now-centre" />}
        <YAxis unit={unit} w={w} h={h} />
        {xTicks.map((t) => (
          <g key={"x" + t}>
            <line x1={x(t)} x2={x(t)} y1={PT} y2={h - PB} className={"mp-grid" + (t === 1000 ? " strong" : "")} />
            <text x={x(t)} y={h - 6} className="mp-text" textAnchor={t === F_MAX ? "end" : "middle"}>
              {t >= 1000 ? `${t / 1000}k` : t}
            </text>
          </g>
        ))}
        {[...curves].reverse().map((c) => (
          <path key={c.k} d={path(c.db, c.gap)} fill="none" style={{ stroke: c.colour }} strokeWidth={c.width} strokeLinejoin="round" />
        ))}
      </svg>
      {resize}
      {all.length > 0 ? (
        <div className="legend mp-legend">
          <ToggleLegend items={all} hidden={hidden} onToggle={toggle} />
          <span>
            {fmtDb(levelDbfs, 0, false)} dBFS · {unit === "pct" ? "% of" : "dB relative to"} the fundamental
          </span>
          {aliasedFrom !== undefined && <span>a line ends where its order lands above Nyquist</span>}
          {now && (
            <span title={`Shaded: where the music playing now has its energy (darker is louder, within 30 dB of its loudest band); the dotted line marks its centre, ${fmtHz(now.centroid_hz)}. The curves are for a single sine at ${fmtDb(levelDbfs, 0, false)} dBFS swept across the band, not for the music.`}>
              <i className="harm-now-key" />
              shaded: where the music is now; the curves are a {fmtDb(levelDbfs, 0, false)} dBFS sine
            </span>
          )}
        </div>
      ) : (
        <p className="stage-note">No harmonics above {floorText(unit)} at this level.</p>
      )}
      {all.length > 0 && <p className="harm-plot-help">click an order to hide it</p>}
      {levelDbfs === 0 && spread >= 0.05 && (
        <p className="stage-note" title="Odd orders also compress (or expand) the fundamental; with tilt that changes with frequency too. Small signals stay flat.">
          A full-scale fundamental's level varies by {fmtDb(spread, 2, false)} dB across the band.
        </p>
      )}
      {aliasedFrom !== undefined && (
        <p className="stage-note" title={"A harmonic above half the output rate folds back to another frequency: it is aliasing, not a harmonic of the tone, so it is left out of THD and the curves.\n\n" + aliasedLines(readout.freq_sweep, levelDbfs, unit)}>
          From {fmtHz(aliasedFrom)} up, some orders land above Nyquist at {fmtRate(readout.rate_hz)} and fold back (left out of THD){loudestText(readout.freq_sweep, levelDbfs, unit)}.
        </p>
      )}
    </div>
  );
}
