// The Custom filter editor: one `FilterDesign` edited field by field. Every
// edit goes through `onEdit` (the store checks it, then sends it or keeps it
// as a draft); the
// live `check_design` result shows each field's error next to it and the
// per-rate plan (taps, memory, CPU) under the controls.

import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import type { DesignCheck, FieldError, FilterDesign, FilterPreset, Length, PhaseMethod, RateTarget, Sampling, UpsamplerResponse, Window } from "../api/types";
import { NumField } from "../components/NumField";
import { Seg } from "../components/Seg";
import { WHEEL_HINT, WheelAccumulator, attachSliderWheelGate, wheelDirection } from "../components/sliderWheel";
import { stepValue } from "../components/valueSliderKeys";
import { whyFor } from "../components/Why";
import { PRESETS, presetEntry } from "../lib/filterPresets";
import { clamp, fmtInt, fmtMs, fmtRate, fmtTaps } from "../lib/format";
import {
  CASCADE_LABEL,
  DEFAULT_BETA,
  DEFAULT_GAUSSIAN_DB,
  FLATTOP_LABEL,
  INTERP_LABEL,
  KIND_LABEL,
  PATH_LABEL,
  RECOMMENDED_PRESET,
  WINDOW_LABEL,
  blockChoices,
  blockSize,
  designKey,
  fieldLabel,
  groupErrors,
  isAdvancedGroup,
  isKernel,
  isStepped,
  lowLatencyDesign,
  musicDesign,
  planLatencyMs,
  planRows,
  sameDesign,
  setCutoffMode,
  setKind,
  setLengthRule,
  setMix,
  setPath,
  setPhaseMethod,
  setPhaseType,
  setWindow,
  setWindowType,
  parseTaps,
  tapPresets,
  windowValueGiven,
  sidelobeDb,
  sidelobeRatio,
  steppedAt,
  steppedText,
  type DesignKind,
  type ErrorGroup,
  type KernelDesign,
  type KindMemory,
  type LengthRule,
  type SincDesign,
  type WindowType,
} from "../lib/filterDesign";
import { engineLimits } from "../lib/engineLimits";
import { kaiserAttenuationDb, ENGINE_UPDATE_TIP } from "../lib/upsamplerLimits";
import { resolveRateRatio } from "../lib/upsamplerRate";
import { errorText, store, useApp } from "../store/store";

type Edit = (d: FilterDesign, ok?: string) => void;

function Row({ label, title, children }: { label: string; title?: string; children: React.ReactNode }) {
  return (
    <div className="row">
      <span className="k" title={title}>
        {label}
      </span>
      <div className="row-ctl">{children}</div>
    </div>
  );
}

/**
 * Two short rows on one line when both fit at their natural width, stacked
 * (each with the usual label column) otherwise. Measured, since the rows'
 * widths change with what they show.
 */
function RowPair({ children }: { children: React.ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const [paired, setPaired] = useState(false);
  const measure = useRef(() => {});
  measure.current = () => {
    const el = ref.current;
    if (!el) return;
    el.classList.add("measure");
    const rows = Array.from(el.children) as HTMLElement[];
    const gap = parseFloat(getComputedStyle(el).columnGap) || 0;
    const need = rows.reduce((sum, r) => sum + r.getBoundingClientRect().width, 0) + gap * Math.max(0, rows.length - 1);
    const fits = rows.length > 1 && need <= el.clientWidth + 0.5;
    el.classList.remove("measure");
    setPaired(fits);
  };
  useLayoutEffect(() => measure.current());
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    let last = el.clientWidth;
    const ro = new ResizeObserver(() => {
      if (el.clientWidth === last) return;
      last = el.clientWidth;
      measure.current();
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return (
    <div ref={ref} className={"row-pair" + (paired ? " paired" : "")}>
      {children}
    </div>
  );
}

/** The errors of one group, under its control. */
export function FieldErrors({ errors, labelled }: { errors: readonly FieldError[] | undefined; labelled?: boolean }) {
  if (!errors || errors.length === 0) return null;
  return (
    <div className="ups-field-err" role="alert">
      {errors.map((e) => (
        <span key={e.path + e.message}>
          {labelled ? `${fieldLabel(e.path)}: ` : ""}
          {e.message}
        </span>
      ))}
    </div>
  );
}

/** The friendly name of a catalogue design. */
export function baseLabel(name: string): string {
  if (name === RECOMMENDED_PRESET) return "Recommended";
  return presetEntry(name as FilterPreset)?.label ?? name;
}

// ------------------------------------------------------------ live check

export interface CheckState {
  check: DesignCheck | null;
  /** A newer check is on its way (the shown one may be stale). */
  pending: boolean;
  /** The check itself failed (not a refusal of the design). */
  failed: string | null;
}

/** `check_design` on the shown design, debounced, the latest one kept. */
export function useDesignCheck(design: FilterDesign | null, target: RateTarget, ratesKey: string): CheckState {
  const supported = useApp((s) => s.caps.designs) !== false;
  const key = design && target !== "off" && supported ? `${designKey(design)}|${JSON.stringify(target)}|${ratesKey}` : "";
  const [state, setState] = useState<{ key: string; check: DesignCheck | null; failed: string | null }>({ key: "", check: null, failed: null });
  const seq = useRef(0);
  const now = useRef({ design, target });
  now.current = { design, target };
  useEffect(() => {
    if (!key) return;
    const n = ++seq.current;
    const timer = setTimeout(() => {
      const { design: d, target: t } = now.current;
      if (!d) return;
      store()
        .checkDesign(d, t)
        .then((check) => n === seq.current && setState({ key, check, failed: null }))
        .catch((e: unknown) => n === seq.current && setState({ key, check: null, failed: errorText(e) }));
    }, 150);
    return () => clearTimeout(timer);
  }, [key]);
  if (!key) return { check: null, pending: false, failed: null };
  return { check: state.check, pending: state.key !== key, failed: state.key === key ? state.failed : null };
}

// ------------------------------------------------------------ the editor

export interface CustomProps {
  design: FilterDesign;
  onEdit: Edit;
  /** Field errors to show: the live check's, plus a refusal the engine named a field in. */
  errors: readonly FieldError[];
  check: CheckState;
  target: RateTarget;
  off: boolean;
  /** The measured response, when it is for this design. */
  measured: UpsamplerResponse | null;
  /** Source rate the readouts are for. */
  readoutRate: number;
  /** Advanced is open: its errors show there, not in the general list. */
  advancedOpen: boolean;
}

export function CustomDesignControls(props: CustomProps) {
  const { design: d, onEdit, off, errors, advancedOpen } = props;
  const groups = groupErrors(errors);
  const memory = useRef<KindMemory>({});
  const lengths = useRef<Partial<Record<LengthRule, Length>>>({});
  if (d.kind === "windowed_sinc") lengths.current[d.length.rule] = d.length;
  const general: FieldError[] = [...(groups.general ?? []), ...(groups.kind ?? []), ...(groups.target ?? [])];
  if (!advancedOpen) for (const g of Object.keys(groups) as ErrorGroup[]) if (isAdvancedGroup(g)) general.push(...(groups[g] ?? []));
  const switchKind = (kind: DesignKind) => {
    memory.current[d.kind] = d;
    onEdit(setKind(d, kind, memory.current));
  };
  return (
    <>
      <StartFrom onEdit={onEdit} off={off} />
      {/* The design rows, with the cost table beside them on a wide panel (below on a narrow one). */}
      <div className="ups-design">
        <div className="ups-design-grid">
          <div className="ups-design-rows">
            <Row label="Type" title="How the engine makes the filter: a kernel it designs (windowed sinc, Lanczos), or a runtime with fixed internal filters">
              <Seg<DesignKind>
                label="Filter type"
                value={d.kind}
                disabled={off}
                onChange={switchKind}
                options={[
                  ["windowed_sinc", KIND_LABEL.windowed_sinc, "A sinc kernel shaped by a window: you set the window, length, cutoff and phase"],
                  ["lanczos", KIND_LABEL.lanczos, "A sinc shaped by a wider sinc over a whole number of lobes: short and soft, set by the lobe count"],
                  ["cascade", KIND_LABEL.cascade, "Fixed ×2 and ×3 stages in a row. Even integer ratios made of 2 and 3 only (×2, ×4, ×6, ×8…)"],
                  ["interpolator", KIND_LABEL.interpolator, "A short fixed interpolator: very light on CPU, soft top octave. Integer ratios only"],
                  ["block_fft", KIND_LABEL.block_fft, "Spectral zero-padding of overlapping blocks: a brick-wall response. Power-of-two ratios only"],
                ]}
              />
            </Row>
            {d.kind === "windowed_sinc" && <SincControls {...props} design={d} lengths={lengths.current} groups={groups} />}
            {d.kind === "lanczos" && (
              <>
                <RowPair>
                  <Row label="Lobes" title="How many sinc lobes each side of the centre: more is steeper and longer">
                    <NumField
                      className="ups-num"
                      label="Lobes"
                      value={d.lobes}
                      disabled={off}
                      format={(v) => String(Math.round(v))}
                      title={`Sinc lobes each side (1 to 65,535): the kernel is ${fmtInt(2 * d.lobes + 1)} taps per input sample now. More lobes: steeper, more CPU and (linear phase) more delay`}
                      onCommit={(v) => onEdit({ ...d, lobes: Math.round(clamp(v, 1, 65535)) })}
                    />
                    <span className="hint">= {fmtInt(2 * d.lobes + 1)} taps per input sample</span>
                  </Row>
                  <PhaseRow design={d} onEdit={onEdit} off={off} />
                </RowPair>
                <FieldErrors errors={groups.length} />
                <FieldErrors errors={groups.phase} />
              </>
            )}
            {d.kind === "cascade" && (
              <Row label="Stage" title="The filter each ×2 or ×3 stage uses (fixed tables)">
                <Seg
                  label="Cascade stage"
                  value={d.stage}
                  disabled={off}
                  onChange={(stage) => onEdit({ ...d, stage })}
                  options={[
                    ["fir", CASCADE_LABEL.fir, "A 255-tap linear-phase FIR per stage"],
                    ["fir_minimum", CASCADE_LABEL.fir_minimum, "The same FIR in minimum phase: no pre-ringing, little delay"],
                    ["fir_asymmetric", CASCADE_LABEL.fir_asymmetric, "An asymmetric FIR: less pre-ringing than linear, less phase shift than minimum"],
                    ["frequency_sampled", CASCADE_LABEL.frequency_sampled, "A 512-tap frequency-sampled FIR per stage"],
                    ["elliptic", CASCADE_LABEL.elliptic, "A recursive elliptic stage: very light, steep, not linear phase"],
                    ["inverse_chebyshev", CASCADE_LABEL.inverse_chebyshev, "A recursive inverse-Chebyshev stage: flat passband, light, not linear phase"],
                  ]}
                />
              </Row>
            )}
            {d.kind === "interpolator" && (
              <Row label="Method">
                <Seg
                  label="Interpolator method"
                  value={d.method}
                  disabled={off}
                  onChange={(method) => onEdit({ ...d, method })}
                  options={[
                    ["cosine", INTERP_LABEL.cosine, "Cosine interpolation between samples: the lightest, rolls off the top octave"],
                    ["catmull_rom", INTERP_LABEL.catmull_rom, "A cubic Catmull-Rom spline: light, a little sharper than cosine"],
                    ["table_sinc", INTERP_LABEL.table_sinc, "A table-driven sinc: much sharper, 127 multiply-adds per output sample"],
                  ]}
                />
              </Row>
            )}
            {d.kind === "block_fft" && <BlockRow design={d} onEdit={onEdit} off={off} errors={groups.length} />}
          </div>
          <PlanReadout {...props} />
        </div>
      </div>
      <FieldErrors errors={general} labelled />
    </>
  );
}

// ------------------------------------------------------------ basis

/** "Modified from X [Revert to preset]": shown on the Filter row. */
export function BasisNote({ design }: { design: FilterDesign }) {
  const st = store();
  const name = design.based_on;
  const base = useApp((s) => (name ? s.designBases[name] : undefined));
  const designs = useApp((s) => s.caps.designs);
  useEffect(() => {
    if (name && !base && designs !== false) void st.resolveDesign(name).catch(() => undefined);
  }, [name, base, designs, st]);
  if (!name) return null;
  const label = baseLabel(name);
  const same = base ? sameDesign(base, design) : null;
  return (
    <span className="ups-basis">
        {same === null ? `Copied from ${label}` : same ? `Same as ${label}` : `Modified from ${label}`}
        {same !== null && (
          <button
            type="button"
            className="btn small"
            title={same ? `Run the ${label} preset itself: the same filter, stored by name (fixes to the preset reach you)` : `Drop your changes and run the ${label} preset`}
            onClick={() => void st.useBasePreset(name)}
          >
            {same ? "Use the preset" : "Revert to preset"}
          </button>
      )}
    </span>
  );
}

function StartFrom({ onEdit, off }: { onEdit: Edit; off: boolean }) {
  const st = store();
  const designs = useApp((s) => s.caps.designs) !== false;
  const id = useId();
  const recBlock = designs ? null : ENGINE_UPDATE_TIP;
  const recWhy = whyFor(`${id}-rec`, !off && recBlock !== null, recBlock);
  const load = (name: string) =>
    void st
      .resolveDesign(name)
      .then((d) => onEdit(d, `Started from ${baseLabel(name)}`))
      .catch(() => undefined);
  return (
    <Row label="Start from" title="Fill the editor with a starting point; change anything after">
      <div className="ups-starts">
        <button type="button" className="btn small" disabled={off} {...recWhy.attrs} title={recBlock ?? "The Recommended design: Kaiser, automatic length matching the output, linear phase"} onClick={() => !recBlock && load(RECOMMENDED_PRESET)}>
          Recommended
        </button>
        {recWhy.tip}
        <button
          type="button"
          className="btn small"
          disabled={off}
          title="A 256k-tap linear-phase Kaiser filter, full band: steep and transparent, about 0.17 s of delay at 768 kHz. For music."
          onClick={() => onEdit(musicDesign(), "Music: long linear-phase filter")}
        >
          Music
        </button>
        <button type="button" className="btn small" disabled={off} title="A 16k-tap minimum-phase Kaiser filter, full band: almost no delay. For games and video." onClick={() => onEdit(lowLatencyDesign(), "Low latency: short minimum-phase filter")}>
          Low latency
        </button>
        <select
          className="plain ups-from-preset"
          aria-label="Start from a preset"
          title={designs ? "Copy any preset's exact values into the editor" : ENGINE_UPDATE_TIP}
          disabled={off || !designs}
          value=""
          onChange={(e) => e.target.value && load(e.target.value)}
        >
          <option value="">From preset…</option>
          {PRESETS.map((p) => (
            <option key={p.name} value={p.name}>
              {p.label}
            </option>
          ))}
        </select>
      </div>
    </Row>
  );
}

// ------------------------------------------------------------ windowed sinc

const WINDOW_TIPS: Record<WindowType, string> = {
  kaiser: "Kaiser: equal sidelobes; β trades transition width against stopband depth",
  gaussian: "Gaussian: the impulse fades out smoothly instead of ringing; a wider transition for the same length",
  flattop: "Flat-top: very deep stopband from a published coefficient set; a wide transition",
  chebyshev: "Dolph-Chebyshev: every sidelobe at the level you set, the narrowest main lobe for it",
  nuttall: "Nuttall: a four-term cosine window, about −98 dB sidelobes",
  hann: "Hann: a raised cosine, gentle and short; about −31 dB sidelobes",
  tukey: "Tukey: flat in the middle, tapered at the ends. Linear phase and a manual length and cutoff only",
};

function SincControls({ design: d, onEdit, off, lengths, groups, check, readoutRate, measured }: CustomProps & { design: SincDesign; lengths: Partial<Record<LengthRule, Length>>; groups: Partial<Record<ErrorGroup, FieldError[]>> }) {
  const tukey = d.window.type === "tukey";
  const searched = d.length.rule === "band" || d.length.rule === "auto";
  const tukeyWhy = tukey ? "The Tukey window has no calibrated stopband: use a manual length." : null;
  const plan = check.check?.plans.find((p) => p.source_rate === readoutRate) ?? null;
  return (
    <>
      <Row label="Window" title="The window shaping the sinc kernel">
        <Seg<WindowType>
          label="Window"
          value={d.window.type}
          disabled={off}
          onChange={(t) => onEdit(setWindowType(d, t))}
          options={(Object.keys(WINDOW_LABEL) as WindowType[]).map((t) => [t, WINDOW_LABEL[t], WINDOW_TIPS[t]])}
        />
        <WindowParams design={d} onEdit={onEdit} off={off} measured={measured} />
      </Row>
      <FieldErrors errors={groups.window} />
      <Row label="Length" title="How long the kernel is: longer is steeper, costs more CPU and (linear phase) more delay">
        <Seg<LengthRule>
          label="Length rule"
          value={d.length.rule === "band" ? null : d.length.rule}
          disabled={off}
          onChange={(rule) => onEdit(setLengthRule(d, rule, lengths))}
          options={[
            ["fixed", "Taps", "A number of taps at the output rate (on the intermediate grid at a fractional ratio): lasts a shorter time at a higher ratio"],
            ["per_input", "Per input sample", "Taps per input sample (× the ratio at the output): the filter lasts the same time at every ratio. Fractional ratios need this for long filters"],
            ["output_span", "Constant time", "About the same length in time at every output rate (a power of two of output taps): integer ratios only"],
            ["auto", "Automatic", tukeyWhy ?? "The engine finds the shortest filter that meets an accuracy target between the band edges (Cutoff: Band)", tukeyWhy !== null && d.length.rule !== "auto"],
          ]}
        />
        <LengthParams design={d} onEdit={onEdit} off={off} readoutRate={readoutRate} />
      </Row>
      <FieldErrors errors={groups.length} />
      {plan && plan.plan.grid_taps > 0 && !(d.length.rule === "fixed" && plan.ratio.down === 1 && plan.plan.grid_taps === d.length.taps) && (
        <div className="ups-adv-readout">
          {plan.plan.grid_taps_exact ? "" : "≤ "}
          {fmtInt(plan.plan.grid_taps)} taps at {fmtRate(readoutRate)} ({plan.ratio.down === 1 ? `×${plan.ratio.up}` : `${plan.ratio.up}/${plan.ratio.down}`})
          {measured && measured.taps !== plan.plan.grid_taps && measured.source_rate === readoutRate ? ` · the engine picked ${fmtInt(measured.taps)}` : ""}
        </div>
      )}
      <RowPair>
        <Row label="Cutoff" title="Where the filter rolls off, as a fraction of the lower Nyquist (the source's when upsampling)">
          <Seg
            label="Cutoff mode"
            value={d.cutoff.mode}
            disabled={off}
            onChange={(m) => onEdit(setCutoffMode(d, m))}
            options={[
              ["fraction", "Fraction", "Put the −6 dB point at a fraction of the Nyquist you choose: 1 is full band, about 0.95 apodizing"],
              ["nyquist_stop", "Stop at Nyquist", tukeyWhy ?? "Put the end of the transition at the Nyquist (no imaging): the engine places the cutoff from the window and length", tukeyWhy !== null && d.cutoff.mode !== "nyquist_stop"],
              ["band", "Band", tukeyWhy ?? "Give where the passband ends, where the stopband starts and how deep: the engine picks the length (and β)", tukeyWhy !== null && d.cutoff.mode !== "band"],
            ]}
          />
          <CutoffParams design={d} onEdit={onEdit} off={off} readoutRate={readoutRate} />
        </Row>
        <PhaseRow design={d} onEdit={onEdit} off={off} />
      </RowPair>
      <FieldErrors errors={groups.phase} />
      {d.cutoff.mode === "band" && <BandRow design={d} onEdit={onEdit} off={off} />}
      {searched && d.cutoff.mode !== "band" && <span className="hint">A band or automatic length needs the Band cutoff.</span>}
      <FieldErrors errors={groups.cutoff} />
    </>
  );
}

function WindowParams({ design: d, onEdit, off, measured }: { design: SincDesign; onEdit: Edit; off: boolean; measured: UpsamplerResponse | null }) {
  const w = d.window;
  const put = (nw: Window) => onEdit(setWindow(d, nw));
  // A band cutoff derives the window's value from the depth (or the
  // accuracy target); a fraction or stop-at-Nyquist cutoff needs it given.
  const given = windowValueGiven(d);
  const from = d.length.rule === "auto" ? "the accuracy target" : "the band depth";
  const used = measured && measured.effective_kaiser_beta > 0 ? measured.effective_kaiser_beta : null;
  switch (w.type) {
    case "kaiser":
      if (!given) {
        return (
          <span className="ups-derived" title={`With the Band cutoff the engine derives β from ${from}. Pick Fraction or Stop at Nyquist to set β yourself.`}>
            β {used !== null ? `${Number(used.toFixed(2))} ` : ""}from {from}
          </span>
        );
      }
      return (
        <>
          <NumField
            className="ups-num ups-num-short"
            label="Beta"
            value={w.beta ?? DEFAULT_BETA}
            disabled={off}
            format={(v) => String(Number(v.toFixed(4)))}
            title={`Kaiser β, 0 to 40 (minimum and intermediate phase by zero reflection: 2 to 20); now about a ${kaiserAttenuationDb(w.beta ?? DEFAULT_BETA).toFixed(0)} dB stopband. Higher: a deeper stopband and a wider transition. With the Band cutoff the engine derives it instead`}
            onCommit={(v) => put({ ...w, beta: clamp(v, 0, 40) })}
          />
        </>
      );
    case "gaussian":
      if ("alpha" in w) {
        return (
          <NumField
            className="ups-num ups-num-short"
            label="Alpha"
            value={w.alpha}
            disabled={off}
            format={(v) => v.toFixed(3)}
            title="Gaussian α: the width as a fraction of the half length, 0.05 to 0.5. Smaller: a narrower window, a wider transition and a deeper stopband"
            onCommit={(v) => put({ ...w, alpha: clamp(Math.round(v * 1e4) / 1e4, 0.05, 0.5) })}
          />
        );
      }
      if (!given) {
        return (
          <span className="ups-derived" title={`With the Band cutoff the engine sets the window's stopband from ${from}. Pick Fraction or Stop at Nyquist to set it yourself.`}>
            Stopband {used !== null ? `${kaiserAttenuationDb(used).toFixed(0)} dB ` : ""}from {from}
          </span>
        );
      }
      return (
        <NumField
          className="ups-num ups-num-short"
          label="Stopband"
          unit="dB"
          value={w.stopband_db ?? DEFAULT_GAUSSIAN_DB}
          disabled={off}
          format={(v) => v.toFixed(0)}
          title="Where the window is cut: the stopband it aims at, 55 to 225 dB. Deeper: a longer window for the same transition. With the Band cutoff the engine sets it from the depth instead"
          onCommit={(v) => put({ ...w, stopband_db: Math.round(clamp(v, 55, 225)) })}
        />
      );
    case "flattop":
      return (
        <select className="plain" aria-label="Flat-top coefficient set" title="The published flat-top coefficient set: HFT196D, HFT223D, HFT248D (deeper, wider)" disabled={off} value={w.set} onChange={(e) => put({ ...w, set: e.target.value as typeof w.set })}>
          {(Object.keys(FLATTOP_LABEL) as (keyof typeof FLATTOP_LABEL)[]).map((s) => (
            <option key={s} value={s}>
              {FLATTOP_LABEL[s]}
            </option>
          ))}
        </select>
      );
    case "chebyshev":
      return (
        <NumField
          className="ups-num ups-num-short"
          label="Sidelobes"
          unit="dB"
          value={sidelobeDb(w.sidelobe_ratio)}
          disabled={off}
          format={(v) => `−${v.toFixed(1)}`}
          title="The level of every sidelobe below the main lobe, 40 to 220 dB (zero-reflection phase: up to 190). Deeper: a wider main lobe"
          onCommit={(v) => put({ ...w, sidelobe_ratio: sidelobeRatio(clamp(Math.abs(v), 40, 220)) })}
        />
      );
    case "tukey":
      return (
        <NumField
          className="ups-num ups-num-short"
          label="Taper"
          value={w.taper}
          disabled={off}
          format={(v) => v.toFixed(2)}
          title="Fraction of each half of the kernel that tapers (above 0, up to 1; 1 is a Hann window)"
          onCommit={(v) => put({ ...w, taper: clamp(Math.round(v * 1000) / 1000, 0.001, 1) })}
        />
      );
    default:
      return null;
  }
}

const SPAN_CHOICES = Array.from({ length: 11 }, (_, i) => 2 ** (12 + i));

function LengthParams({ design: d, onEdit, off, readoutRate }: { design: SincDesign; onEdit: Edit; off: boolean; readoutRate: number }) {
  const maxTaps = engineLimits(useApp((s) => s.status?.limits)).max_kernel_taps;
  const l = d.length;
  const put = (length: Length) => onEdit({ ...d, length });
  switch (l.rule) {
    case "fixed":
      return (
        <>
          <NumField
            className="ups-num"
            label="Taps"
            value={l.taps}
            disabled={off}
            format={(v) => fmtInt(v)}
            parse={parseTaps}
            title={`Kernel taps at the output rate (on the intermediate grid at a fractional ratio), 1 to ${fmtInt(maxTaps)}. Type a number, a power of two (2^20) or a size: 64k = 65,536 and 1M = 1,048,576 taps (k = 1024, M = 1024 × 1024). Longer: steeper, more CPU, more delay (linear phase)`}
            onCommit={(v) => put({ ...l, taps: Math.round(clamp(v, 1, maxTaps)) })}
          />
          <select
            className="plain ups-taps-menu"
            aria-label="Common tap counts"
            title="Pick a common power-of-two length"
            disabled={off}
            value=""
            onChange={(e) => e.target.value && put({ ...l, taps: Number(e.target.value) })}
          >
            <option value="">Common…</option>
            {tapPresets(maxTaps).map((n) => (
              <option key={n} value={String(n)}>
                {fmtTaps(n)} ({fmtInt(n)})
              </option>
            ))}
          </select>
        </>
      );
    case "per_input":
      return isStepped(l.taps) ? (
        <>
          <span className="hint">{steppedText(l.taps, (t) => fmtInt(t))} per input sample</span>
          <button type="button" className="btn small" disabled={off} title="Use one length at every source rate (the one for the rate shown)" onClick={() => put({ ...l, taps: steppedAt(l.taps, readoutRate) })}>
            Use one value
          </button>
        </>
      ) : (
        <NumField
          className="ups-num"
          label="Taps"
          unit="per input"
          value={l.taps}
          disabled={off}
          format={(v) => fmtInt(v)}
          title="Taps per input sample, 1 to 65,535 (the kernel is this times the ratio). Longer: steeper, more CPU, more delay (linear phase)"
          onCommit={(v) => put({ ...l, taps: Math.round(clamp(v, 1, 65535)) })}
        />
      );
    case "output_span":
      return (
        <select
          className="plain"
          aria-label="Constant-time span"
          title="Ladder: the engine's power-of-two length for each output rate, about 1.5 s. Or one span of output taps at every rate"
          disabled={off}
          value={l.taps === "ladder" ? "ladder" : String(l.taps)}
          onChange={(e) => put({ ...l, taps: e.target.value === "ladder" ? "ladder" : Number(e.target.value) })}
        >
          <option value="ladder">Ladder (by output rate)</option>
          {SPAN_CHOICES.map((n) => (
            <option key={n} value={String(n)}>
              {fmtInt(n)} output taps
            </option>
          ))}
        </select>
      );
    case "auto":
      return (
        <>
          <Seg<"match" | "db">
            label="Accuracy target"
            value={l.budget === "match_output" ? "match" : "db"}
            disabled={off}
            onChange={(m) => put({ ...l, budget: m === "match" ? "match_output" : { db: 140 } })}
            options={[
              ["match", "Match output", "Aim at the output's own resolution: the bit depth and dither set on the Output page (about 141 dB for 24-bit with dither)"],
              ["db", "Set level", "Aim at a level you set: how far below full scale the passband error and the images must stay (55 to 200 dB)"],
            ]}
          />
          {l.budget !== "match_output" && (
            <NumField
              className="ups-num"
              label="Level"
              unit="dB"
              value={l.budget.db}
              disabled={off}
              format={(v) => v.toFixed(0)}
              title="Passband error and image level, in dB below full scale (55 to 200). Higher: a longer filter"
              onCommit={(v) => put({ ...l, budget: { db: Math.round(clamp(v, 55, 200)) } })}
            />
          )}
        </>
      );
    case "band":
      return <span className="hint">Set by the band edges below</span>;
  }
}

function CutoffParams({ design: d, onEdit, off, readoutRate }: { design: SincDesign; onEdit: Edit; off: boolean; readoutRate: number }) {
  const c = d.cutoff;
  if (c.mode !== "fraction") return null;
  const v = c.value;
  if (isStepped(v)) {
    return (
      <>
        <span className="hint">{steppedText(v, (x) => x.toFixed(3))} × Nyquist</span>
        <button type="button" className="btn small" disabled={off} title="Use one cutoff at every source rate (the one for the rate shown)" onClick={() => onEdit({ ...d, cutoff: { ...c, value: steppedAt(v, readoutRate) } })}>
          Use one value
        </button>
      </>
    );
  }
  const keep = d.preserve_samples;
  return (
    <>
      <NumField
        className="ups-num"
        label="Cutoff"
        unit="× Nyquist"
        value={v}
        disabled={off || keep}
        format={(x) => x.toFixed(4)}
        title={keep ? "Keeping the input samples needs the cutoff at 1.0" : "The −6 dB point as a fraction of the Nyquist (above 0, up to 1): 1 is full band, lower rolls off earlier"}
        onCommit={(x) => onEdit({ ...d, cutoff: { ...c, value: clamp(Math.round(x * 1e6) / 1e6, 0.01, 1) } })}
      />
      <span className="hint" title={`The cutoff at the ${fmtRate(readoutRate)} source rate the page previews`}>
        = {fmtInt((v * readoutRate) / 2)} Hz
      </span>
    </>
  );
}

function BandRow({ design: d, onEdit, off }: { design: SincDesign; onEdit: Edit; off: boolean }) {
  const c = d.cutoff;
  if (c.mode !== "band") return null;
  const put = (p: Partial<typeof c>) => onEdit({ ...d, cutoff: { ...c, ...p } });
  const auto = d.length.rule === "auto";
  return (
    <Row label="Band edges" title="Fractions of the Nyquist (1.0 = 22.05 kHz at 44.1 kHz)">
      <NumField
        className="ups-num"
        label="Pass to"
        unit="× Nyq"
        value={c.passband_end}
        disabled={off}
        format={(v) => v.toFixed(3)}
        title="End of the passband (0.5 to 1): up to here the response stays flat within the accuracy target"
        onCommit={(v) => put({ passband_end: clamp(v, 0.5, 1) })}
      />
      <NumField
        className="ups-num"
        label="Stop from"
        unit="× Nyq"
        value={c.stopband_start}
        disabled={off}
        format={(v) => v.toFixed(3)}
        title="Start of the stopband (above the passband end, up to 1.5). 1.0 puts it at the Nyquist (no imaging); above 1 lets images of the top of the band through for a shorter filter"
        onCommit={(v) => put({ stopband_start: clamp(v, 0.5, 1.5) })}
      />
      {!auto && (
        <NumField
          className="ups-num"
          label="Depth"
          unit="dB"
          value={c.attenuation_db}
          disabled={off}
          format={(v) => v.toFixed(0)}
          title="Stopband attenuation, 55 to 225 dB (minimum and intermediate phase by zero reflection: up to 190). Deeper: a longer filter"
          onCommit={(v) => put({ attenuation_db: Math.round(clamp(v, 55, 225) * 10) / 10 })}
        />
      )}
      {auto && <span className="hint">Depth: the accuracy target</span>}
    </Row>
  );
}

// ------------------------------------------------------------ phase

function PhaseRow({ design: d, onEdit, off }: { design: KernelDesign; onEdit: Edit; off: boolean }) {
  const tukey = d.kind === "windowed_sinc" && d.window.type === "tukey";
  const why = tukey ? "The Tukey window needs linear phase." : d.preserve_samples ? "Keeping the input samples needs linear phase." : null;
  return (
    <Row label="Phase" title="How the filter's delay is spread over frequency: linear (symmetric ringing, delay), minimum (no pre-ringing), or a blend">
        <Seg
          label="Phase"
          value={d.phase.type}
          disabled={off}
          onChange={(t) => onEdit(setPhaseType(d, t))}
          options={[
            ["linear", "Linear", "Linear: ringing is symmetric around each transient and nothing is phase-shifted; adds delay (half the filter length)"],
            ["intermediate", "Intermediate", why ?? "A blend of minimum and linear phase (set the blend): less pre-ringing than linear, less phase shift than minimum. The kernel grows (up to 4×)", why !== null && d.phase.type !== "intermediate"],
            ["minimum", "Minimum", why ?? "Minimum: no pre-ringing and almost no delay; shifts phase near the cutoff", why !== null && d.phase.type !== "minimum"],
          ]}
        />
      {d.phase.type === "intermediate" && <MixSlider value={d.phase.mix} disabled={off} onCommit={(mix) => onEdit(setMix(d, mix))} />}
    </Row>
  );
}

// ------------------------------------------------------------ block FFT

function BlockRow({ design: d, onEdit, off, errors }: { design: Extract<FilterDesign, { kind: "block_fft" }>; onEdit: Edit; off: boolean; errors: FieldError[] | undefined }) {
  const { which, size } = blockSize(d.block);
  const choices = blockChoices(which);
  return (
    <>
      <Row label="Block" title="The size of each transformed block: longer is a sharper brick wall and more delay">
        <Seg<"input" | "output">
          label="Block counts"
          value={which}
          disabled={off}
          onChange={(w) => onEdit({ ...d, block: w === "input" ? { input: 16384 } : { output: 1 << 20 } })}
          options={[
            ["input", "Input frames", "The block is a number of input frames (1,024 to 65,536): the same time at every ratio"],
            ["output", "Output frames", "The block is a number of output frames (up to 4M): a shorter time at a higher ratio"],
          ]}
        />
        <select className="plain" aria-label="Block size" title="Frames per block, a power of two" disabled={off} value={String(size)} onChange={(e) => onEdit({ ...d, block: which === "input" ? { input: Number(e.target.value) } : { output: Number(e.target.value) } })}>
          {choices.map((n) => (
            <option key={n} value={String(n)}>
              {fmtInt(n)}
            </option>
          ))}
        </select>
      </Row>
      <FieldErrors errors={errors} />
      <Row label="Overlap">
        <Seg
          label="Block overlap"
          value={d.overlap}
          disabled={off}
          onChange={(overlap) => onEdit({ ...d, overlap })}
          options={[
            ["half", "Half", "Blocks overlap by half: lighter on CPU"],
            ["quarter_average", "Quarter, averaged", "Blocks step by a quarter and average: smoother, twice the work"],
          ]}
        />
      </Row>
    </>
  );
}

// ------------------------------------------------------------ plan

function PlanReadout({ design: d, check, readoutRate, measured, target, off }: CustomProps) {
  const supported = useApp((s) => s.caps.designs) !== false;
  if (off) return null;
  if (!supported) return <div className="ups-adv-readout">{ENGINE_UPDATE_TIP.replace("this yet", "the design check yet")}</div>;
  const rows = planRows(check.check?.plans ?? []);
  const here = check.check?.plans.find((p) => p.source_rate === readoutRate);
  const measuredHere = measured && measured.source_rate === readoutRate ? measured : null;
  const lat = measuredHere ? (measuredHere.latency_samples / measuredHere.output_rate) * 1000 : here ? planLatencyMs(d, here.plan, readoutRate) : null;
  // The engine plans each rate where it would open this device.
  const ratio = here?.ratio ?? resolveRateRatio(target, readoutRate);
  return (
    <div className={"ups-plan" + (check.pending ? " pending" : "")} aria-live="polite" aria-busy={check.pending}>
      <div className="ups-plan-head">
        <span className="ups-plan-title" title="What the engine would build at each allowed source rate on this device: from its design check, before anything is designed">
          Cost per rate
        </span>
        {check.pending && <span className="hint">Checking…</span>}
        {check.failed && <span className="hint">The check failed: {check.failed}</span>}
        {ratio.up !== ratio.down && (
          <span className="hint" title={measuredHere ? "Measured by the engine for this design" : "Half the kernel (linear phase); the measurement gives the other phases"}>
            Latency at {fmtRate(readoutRate)}: {lat === null ? "measured once applied" : `${measuredHere ? "" : "≈ "}${fmtMs(lat)}`}
          </span>
        )}
      </div>
      {rows.length > 0 ? (
        <table className="ups-plan-table">
          <thead>
            <tr>
              <th scope="col">Source</th>
              <th scope="col">Ratio</th>
              <th scope="col" title="Kernel taps on the filter's grid">
                Taps
              </th>
              <th scope="col" title="Memory the running filter keeps (both channels)">
                Memory
              </th>
              <th scope="col" title="Share of one CPU core one pipeline takes, as the engine prices it">
                CPU
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.rate} className={r.rate === readoutRate ? "here" : undefined}>
                <td>{r.rateText}</td>
                <td>{r.ratioText}</td>
                <td title={r.tapsTip}>{r.tapsText}</td>
                <td title={r.memoryTip}>{r.memoryText}</td>
                <td>{r.cpuText}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ) : (
        check.check && check.check.errors.length === 0 && <span className="hint">No allowed source rate is resampled with this rate setting.</span>
      )}
    </div>
  );
}

// ------------------------------------------------------------ advanced

const SAMPLING_TIPS: Record<Sampling, string> = {
  periodic: "Periodic: the window divides by N, as the presets' Kaiser, flat-top and Hann windows do",
  symmetric: "Symmetric: the window divides by N − 1, exactly symmetric about the centre (needed for a zero-reflection phase)",
  integer_centre: "Integer centre: the presets' Gaussian, centred on a whole tap",
};

export function AdvancedDesign({ design: d, onEdit, off, errors, measured }: { design: FilterDesign; onEdit: Edit; off: boolean; errors: readonly FieldError[]; measured: UpsamplerResponse | null }) {
  const groups = groupErrors(errors);
  const peak = measured?.plan?.bounded_peak_db;
  if (!isKernel(d)) return null;
  const k = d;
  const sinc = k.kind === "windowed_sinc" ? k : null;
  const w = sinc?.window ?? null;
  const samplings: Sampling[] = w?.type === "gaussian" && "alpha" in w ? ["symmetric", "integer_centre"] : ["periodic", "symmetric"];
  const keepWhy =
    k.kind === "lanczos"
      ? "A Lanczos kernel's zeros are off the input grid."
      : k.phase.type !== "linear"
        ? "Needs linear phase."
        : sinc && (sinc.cutoff.mode !== "fraction" || sinc.cutoff.value !== 1)
          ? "Needs the Fraction cutoff at 1.0."
          : null;
  const dec = k.decimation;
  const perInputTaps = sinc && sinc.length.rule === "per_input" && !isStepped(sinc.length.taps) ? sinc.length.taps : 255;
  const rates = k.rates;
  const fb = rates?.fallback_window ?? null;
  return (
    <>
      <RowPair>
        {w && (
          <Row label="Window sampling" title="How the window is sampled over the kernel's taps: part of the arithmetic">
            <Seg<Sampling> label="Window sampling" value={w.sampling} disabled={off} onChange={(sampling) => onEdit({ ...sinc!, window: { ...w, sampling } as Window })} options={samplings.map((s) => [s, s === "integer_centre" ? "Integer centre" : s === "periodic" ? "Periodic" : "Symmetric", SAMPLING_TIPS[s]])} />
          </Row>
        )}
        <Row label="Phase method" title={k.phase.type === "linear" ? "How a minimum or intermediate phase is made (linear phase has none)" : "How a minimum or intermediate phase is made"}>
          <Seg<PhaseMethod>
            label="Phase method"
            value={k.phase.type === "linear" ? null : k.phase.method}
            disabled={off || k.phase.type === "linear"}
            onChange={(m) => onEdit(setPhaseMethod(k, m))}
            options={[
              ["reflect", "Zero reflection (checked)", "Reflects the kernel's zeros inside the unit circle and checks the result: the Custom method, on the standard and FIR paths"],
              ["cepstral", "Cepstral", "Builds the phase from the log spectrum: the presets' method"],
            ]}
          />
        </Row>
      </RowPair>
      <FieldErrors errors={groups.phase?.filter((e) => e.path.includes("method"))} />
      <RowPair>
        <Row label="Original samples" title="Whether every input sample passes through unchanged (a Nyquist filter)">
          <KeepCheck checked={k.preserve_samples} off={off} blocked={!k.preserve_samples ? keepWhy : null} onChange={(preserve_samples) => onEdit({ ...k, preserve_samples })} />
        </Row>
        <Row label="Ratios" title="Which ratios the design accepts; at others the engine refuses it">
          <Seg<"default" | "any" | "upsampling" | "integer">
            label="Accepted ratios"
            value={rates === null ? "default" : rates.accept}
            disabled={off}
            onChange={(a) => onEdit({ ...k, rates: a === "default" ? null : { accept: a, fallback_window: fb } })}
            options={[
              ["default", "Default", "What the design's own fields allow: integer for a constant-time length; upsampling for Lanczos, per-rate values and kept samples; any otherwise"],
              ["any", "Any", "Any ratio, fractional and decimating included"],
              ["upsampling", "Upsampling only", "Only ratios that raise the rate"],
              ["integer", "Integer only", "Only whole multiples of the source rate"],
            ]}
          />
        </Row>
      </RowPair>
      <FieldErrors errors={groups.samples} />
      <Row label="Decimation" title="How the length follows when the output rate is below the source rate">
        <Seg<"scale" | "keep">
          label="Decimation length"
          value={dec.length}
          disabled={off}
          onChange={(m) => onEdit({ ...k, decimation: m === "keep" ? { length: "keep" } : { length: "scale", odd: "down", base_taps: null } })}
          options={[
            ["scale", "Scale", "Scale the length with the decimation, so the transition stays as steep relative to the output's Nyquist (the presets)"],
            ["keep", "Keep", "Keep the length as set (the standard path)"],
          ]}
        />
        {dec.length === "scale" && (
          <>
            <Seg<"down" | "up">
              label="Odd length"
              value={dec.odd}
              disabled={off}
              onChange={(odd) => onEdit({ ...k, decimation: { ...dec, odd } })}
              options={[
                ["down", "Round down", "An even scaled length loses a tap"],
                ["up", "Round up", "An even scaled length gains a tap"],
              ]}
            />
            <label className="ups-check" title="Scale another length than the design's own when decimating (taps per input sample)">
              <input type="checkbox" checked={dec.base_taps !== null} disabled={off} onChange={(e) => onEdit({ ...k, decimation: { ...dec, base_taps: e.target.checked ? perInputTaps : null } })} />
              <span>Own base</span>
            </label>
            {dec.base_taps !== null && (
              <NumField className="ups-num ups-num-short" label="Base" value={dec.base_taps} disabled={off} format={(v) => fmtInt(v)} title="The length scaled when decimating, taps per input sample (1 to 65,535)" onCommit={(v) => onEdit({ ...k, decimation: { ...dec, base_taps: Math.round(clamp(v, 1, 65535)) } })} />
            )}
          </>
        )}
      </Row>
      <FieldErrors errors={groups.decimation} />
      {fb && (
        <Row label="Fallback window" title="A window that replaces the design's own at some ratios">
          <span className="hint">
            {WINDOW_LABEL[fb.window.type]}
            {fb.window.type === "kaiser" && fb.window.beta !== null ? ` β ${Number(fb.window.beta.toFixed(3))}` : ""} {fb.when === "not_multiple" ? "when the output is not a multiple of the source" : "when decimating"}
          </span>
          <button type="button" className="btn small" disabled={off} title="Use the design's own window at every ratio" onClick={() => onEdit({ ...k, rates: { accept: rates?.accept ?? "any", fallback_window: null } })}>
            Remove
          </button>
        </Row>
      )}
      <FieldErrors errors={groups.rates} />
      <Row label="Arithmetic" title="Selects the arithmetic the kernel is built with. Some choices also change gain, symmetry and delay: see the readouts">
        <select className="plain" aria-label="Arithmetic path" disabled={off} value={k.numerics.path} title="The kernel builder: Standard is today's Custom path (exactly symmetric, checked phase); the others are the presets' own builders, kept bit for bit" onChange={(e) => onEdit(setPath(k, e.target.value as KernelDesign["numerics"]["path"]))}>
          {(Object.keys(PATH_LABEL) as (keyof typeof PATH_LABEL)[]).map((p) => (
            <option key={p} value={p}>
              {PATH_LABEL[p]}
            </option>
          ))}
        </select>
        <Seg
          label="Grid cutoff"
          value={k.numerics.grid_cutoff}
          disabled={off}
          onChange={(grid_cutoff) => onEdit({ ...k, numerics: { ...k.numerics, grid_cutoff } })}
          options={[
            ["scaled", "Scaled", "On a fractional grid, scale the cutoff by the ratio (the presets)"],
            ["design_ratio", "Design ratio", "Design the kernel as at the larger term of the ratio, then scale it (the standard path)"],
          ]}
        />
      </Row>
      <FieldErrors errors={groups.numerics} />
      {peak !== undefined && (
        <div className="ups-adv-readout" title="The largest output a full-scale input can produce through this kernel (bounded-input worst case), from the engine's measurement">
          Peak for a full-scale input: {peak >= 0 ? "+" : "−"}
          {Math.abs(peak).toFixed(1)} dB
        </div>
      )}
    </>
  );
}

function KeepCheck({ checked, off, blocked, onChange }: { checked: boolean; off: boolean; blocked: string | null; onChange: (on: boolean) => void }) {
  const id = useId();
  const why = whyFor(id, !off && !!blocked, blocked);
  const title = "A Nyquist filter: every original sample passes through unchanged. −6 dB at the source Nyquist, so images of the very top of the band are only partly rejected.";
  return (
    <span className="why-wrap">
      <label className={"ups-check" + (off || blocked ? " disabled" : "")} title={blocked ?? title}>
        <input type="checkbox" checked={checked} disabled={off} {...why.attrs} onChange={(e) => !blocked && onChange(e.target.checked)} />
        <span>Keep them unchanged</span>
      </label>
      {why.tip}
    </span>
  );
}

// ------------------------------------------------------------ blend slider

const MIX_RANGE = { min: 0, max: 1, step: 0.05 } as const;

/** The intermediate-phase blend: a draft while dragging, sent once it rests. */
export function MixSlider({ value, disabled, onCommit }: { value: number; disabled?: boolean; onCommit: (v: number) => void }) {
  const [draft, setDraft] = useState<number | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const lastRef = useRef<number | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const wheelAcc = useRef(new WheelAccumulator());
  const now = useRef({ value, disabled, onCommit });
  now.current = { value, disabled, onCommit };

  const schedule = (v: number) => {
    lastRef.current = v;
    setDraft(v);
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      timer.current = null;
      lastRef.current = null;
      now.current.onCommit(v);
      setDraft(null);
    }, 250);
  };

  useEffect(() => {
    const el = inputRef.current;
    if (!el) return;
    const gate = attachSliderWheelGate(el);
    const onWheel = (e: WheelEvent) => {
      const curP = now.current;
      if (e.ctrlKey || curP.disabled) return;
      const dir = wheelDirection(e.deltaY, e.deltaX);
      if (dir === 0) return;
      const ts = performance.now();
      if (!gate.canTakeWheel(ts)) {
        wheelAcc.current.reset();
        return;
      }
      const cur = lastRef.current ?? curP.value;
      if (stepValue(dir, cur, MIX_RANGE) === cur) {
        wheelAcc.current.reset();
        return;
      }
      const { steps } = wheelAcc.current.push(e.deltaY, e.deltaMode, ts, e.deltaX);
      if (steps === 0) return;
      const next = stepValue(steps, cur, MIX_RANGE);
      if (next === cur) return;
      e.preventDefault();
      schedule(next);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      el.removeEventListener("wheel", onWheel);
      gate.dispose();
      if (timer.current) {
        clearTimeout(timer.current);
        timer.current = null;
      }
    };
  }, []);

  const shown = draft ?? value;
  const text = shown <= 0 ? "minimum" : shown >= 1 ? "linear" : `${Math.round(shown * 100)}% linear`;
  return (
    <span className="ups-mix" title={`Blend from minimum phase (0) to linear phase (1). ${WHEEL_HINT}`}>
      <span className="ups-mix-end">Min</span>
      <input
        ref={inputRef}
        type="range"
        min={MIX_RANGE.min}
        max={MIX_RANGE.max}
        step={MIX_RANGE.step}
        value={shown}
        disabled={disabled}
        aria-label="Phase blend"
        aria-valuetext={text}
        onPointerDown={(e) => {
          if (!disabled) e.currentTarget.focus();
        }}
        onChange={(e) => schedule(Number(e.target.value))}
      />
      <span className="ups-mix-end">Lin</span>
      <output className="mono">{text}</output>
    </span>
  );
}
