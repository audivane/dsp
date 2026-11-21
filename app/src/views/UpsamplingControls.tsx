// Upsampling settings: the rate target, Preset or Custom, and the shared
// Advanced disclosure. Every control writes through `store.setUpsampler`
// (`set_upsampler`); the daemon's refusal shows under the controls.

import { useEffect, useId, useRef, useState } from "react";
import type { FieldError, FilterDesign, FilterPreset, RateRatio, RateTarget, UpsamplerConfig, UpsamplerResponse } from "../api/types";
import { Chevron } from "../components/Icons";
import { NumField } from "../components/NumField";
import { Seg } from "../components/Seg";
import { whyFor } from "../components/Why";
import {
  dimLabel,
  dimOptions,
  familyDims,
  familyInfo,
  familyPick,
  FAMILIES,
  presetBlockReason,
  presetDetail,
  presetDimValue,
  presetEntry,
  type FamilyId,
} from "../lib/filterPresets";
import { engineLimits } from "../lib/engineLimits";
import { clamp, fmtRate, fmtRateShort } from "../lib/format";
import { rateRatioText } from "../lib/summary";
import { type cutoffTips } from "../lib/upsamplerPlot";
import { ENGINE_UPDATE_TIP, upsamplerFeatures } from "../lib/upsamplerLimits";
import { aboveDeviceBlock,
  dacHighRateBlock,
  dacRateBlock,
  type DacRates,
  HIGH_OUTPUT_RATES,
  isTermLimitMessage,
  MAX_INTEGER_RATIO,
  MAX_OUTPUT_RATE,
  pastTermLimit,
  rateKind,
  resolveRateRatio,
  TARGET_RATES,
  targetForKind,
  type RateKind,
  unreachableNote,
  unreachableSources,
} from "../lib/upsamplerRate";
import { isRecommendedUpsampler, RECOMMENDED_BLURB, RECOMMENDED_DETAIL, RECOMMENDED_LABEL, RECOMMENDED_TIP } from "../lib/upsamplerRecommended";
import { designKey, RECOMMENDED_PRESET, refusalField } from "../lib/filterDesign";
import { AdvancedDesign, BasisNote, CustomDesignControls, useDesignCheck } from "./CustomDesign";
import { store, useApp } from "../store/store";

type Set = (p: Partial<UpsamplerConfig>, ok?: string) => Promise<boolean>;

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

const khz = (r: number) => `${Number((r / 1000).toFixed(1))} kHz`;

// ------------------------------------------------------------ rate

export interface RateLive {
  /** The engine is running the DSP path now (playing or ready, not Direct). */
  live: boolean;
  graphRate: number;
  outputRate: number;
  ratio: RateRatio | null;
  /** What the DAC can be opened at with the configured formats, when probed. */
  dac: DacRates | null;
  /** The selected target asks for a rate the DAC cannot take (e.g. after the
   *  formats changed), so the engine lands lower: why, and where. */
  lower: string | null;
}

const RATIO_CHOICES = [2, 3, 4, 6, 8, 16, 32, 64];
const RATIO_TIPS: Record<number, string> = {
  2: "Twice the source rate (44.1k → 88.2k, 48k → 96k)",
  3: "Three times the source rate (44.1k → 132.3k, 48k → 144k): only for a DAC that plays those rates",
  4: "Four times the source rate (44.1k → 176.4k, 48k → 192k)",
  6: "Six times the source rate (44.1k → 264.6k, 48k → 288k): only for a DAC that plays those rates",
  8: "Eight times the source rate (44.1k → 352.8k, 48k → 384k)",
  16: "Sixteen times the source rate (44.1k → 705.6k, 48k → 768k)",
  32: "32 times the source rate (44.1k → 1411.2k, 48k → 1536k): only for a DAC that plays those rates. Heavy on CPU",
  64: "64 times the source rate (44.1k → 2822.4k, 48k → 3072k): only for a DAC that plays those rates. Heavy on CPU",
};
const CAP_CHOICES = [96000, 192000, 384000, 768000, 1536000, 3072000];
const CAP_LABELS: Record<number, string> = { 1536000: "1.5M", 3072000: "3M" };
const CAP_TIPS: Record<number, string> = {
  1536000: "Never above 1536 kHz: ×32 from 44.1 kHz (1411.2 kHz) or 48 kHz (1536 kHz), for a DAC that plays them. Heavy on CPU",
  3072000: "Never above 3072 kHz: ×64 from 44.1 kHz (2822.4 kHz) or 48 kHz (3072 kHz), for a DAC that plays them. Heavy on CPU",
};

/** Fixed output rates offered: up to 768 kHz, then ×32 and ×64 of 44.1 and 48 kHz. */
const FIXED_RATES: readonly number[] = [...TARGET_RATES, ...HIGH_OUTPUT_RATES];

/** "from 44.1 kHz by 160/147", or why the engine refuses that source. */
function fixedFrom(rate: number, source: number): string {
  const r = resolveRateRatio({ rate }, source);
  if (r.down !== 1 && pastTermLimit(r)) return `not from ${khz(source)} (${r.up}/${r.down} has a term above the engine's limit)`;
  return `from ${khz(source)} by ${rateRatioText(r)}`;
}

/** A rate the custom design does not fit, held back with the engine's reasons. */
interface RateOffer {
  target: RateTarget;
  text: string;
}

export function RateControls({ u, set, live, notes, allowedRates, pipelines }: { u: UpsamplerConfig; set: Set; live: RateLive; notes: string[]; allowedRates: readonly number[]; pipelines: number }) {
  const st = store();
  const last = useApp((s) => s.lastUpsTargets);
  const maxTaps = engineLimits(useApp((s) => s.status?.limits)).max_kernel_taps;
  // An engine from before fractional rates refuses `{rate}` and `max_rate_any`.
  const rational = useApp((s) => s.caps.rationalRates) !== false;
  // Off: the rate controls show and edit the target kept for the power
  // button; nothing is sent until upsampling is turned on.
  const isOff = u.target === "off";
  const offTarget = useApp((s) => s.upsOffTarget) ?? { max_rate: 768000 };
  const t = isOff ? offTarget : u.target;
  // "Fixed rate" waits for a rate to be picked (a default one may not suit
  // the filter); the others send their last or default target at once and
  // stay shown while the engine considers it (or refused it).
  const [pending, setPending] = useState<RateKind | null>(null);
  const kind = pending ?? rateKind(t);
  useEffect(() => {
    if (pending !== null && rateKind(t) === pending) setPending(null);
  }, [t, pending]);
  const [offer, setOffer] = useState<RateOffer | null>(null);
  // The fixed rate last sent: a refused one stays named beside the control.
  const [tried, setTried] = useState<number | null>(null);
  const err = useApp((s) => s.upsamplerError);
  const key = JSON.stringify([t, u.preset, u.design ? designKey(u.design) : null, pipelines]);
  useEffect(() => setOffer(null), [key]);
  // A custom design that does not run at the new rate would be refused:
  // hold the rate back and say why (the engine's design check).
  const send = async (target: RateTarget) => {
    if (isOff) {
      st.setUpsOffTarget(target);
      return;
    }
    setTried(target !== "off" && "rate" in target ? target.rate : null);
    // A rate some allowed source cannot reach is refused whatever the design:
    // send it, so the engine's refusal is shown with the term note.
    const unreachable = unreachableSources(target, allowedRates).length > 0;
    if (u.design && !u.preset && target !== "off" && !unreachable) {
      const check = await st.checkDesign(u.design, target).catch(() => null);
      if (check && check.errors.length > 0) {
        setOffer({ target, text: check.errors.map((e) => e.message).join(" ") });
        return;
      }
    }
    setOffer(null);
    void set({ target });
  };
  const pick = (k: RateKind) => {
    if (k === rateKind(t)) return setPending(null);
    setPending(k === "off" ? null : k);
    if (k === "rate" && !last.rate) return;
    void send(targetForKind(k, last, live.live && live.ratio && live.ratio.up !== live.ratio.down ? live.outputRate : null, rational));
  };
  const presetWhy = (target: RateTarget) =>
    u.preset && u.preset !== RECOMMENDED_PRESET ? presetBlockReason(u.preset as FilterPreset, target, allowedRates, pipelines, maxTaps) : null;
  const ratioVal = t !== "off" && "ratio" in t ? t.ratio : null;
  const cap = t !== "off" && "max_rate" in t ? t.max_rate : t !== "off" && "max_rate_any" in t ? t.max_rate_any : null;
  const anyFamily = t !== "off" && "max_rate_any" in t;
  const capTarget = (c: number): RateTarget => (anyFamily ? { max_rate_any: c } : { max_rate: c });
  const fixed = t !== "off" && "rate" in t ? t.rate : null;
  // A fixed rate some allowed source rate cannot reach (past the term
  // limit): said in plain words, with the engine's refusal when it gave one.
  const shownFixed = tried ?? fixed;
  const termNote = kind === "rate" && shownFixed !== null ? unreachableNote({ rate: shownFixed }, allowedRates) : null;
  const termErr = err && isTermLimitMessage(err) ? err : null;
  const lower = live.lower;
  const blank = (v: number, f: (v: number) => string) => (Number.isFinite(v) ? f(v) : "");
  return (
    <>
      <Row label="Rate">
        <Seg<RateKind>
          label="Rate"
          value={kind}
          onChange={pick}
          options={[
            ["ratio", "Multiply", "A fixed whole multiple of the source rate, ×1 to ×64"],
            ["max", "Highest", "The highest rate up to a limit you set: a power-of-two multiple of the source, or the best rate the DAC plays"],
            [
              "rate",
              "Fixed rate",
              rational
                ? "Always one output rate, whatever the source: 48 kHz for a DAC that only plays the 48 kHz family turns 44.1 kHz into 48 kHz (a fractional ratio, 160/147) and 96 kHz into 48 kHz. Long custom filters do not fit a fractional ratio: use a preset, or count the length in input samples (Advanced)"
                : ENGINE_UPDATE_TIP,
              !rational && kind !== "rate",
            ],
          ]}
        />
        {kind === "max" && (
          <Seg<"same" | "any">
            label="Rate family"
            value={anyFamily ? "any" : "same"}
            onChange={(f) => void send(f === "any" ? { max_rate_any: cap ?? 768000 } : { max_rate: cap ?? 768000 })}
            options={[
              ["same", "Same family", "Power-of-two multiples of the source only (44.1 kHz → 88.2, 176.4, 352.8 kHz): never a fractional ratio"],
              [
                "any",
                "Best for DAC",
                rational
                  ? "Multiples of the source first, but cross to the other rate family (a fractional ratio, e.g. 44.1 → 192 kHz) when the DAC plays no multiple, or when the other family's best rate is at least twice as high. Uses the DAC's rate list from when the engine opened it"
                  : ENGINE_UPDATE_TIP,
                !rational && !anyFamily,
              ],
            ]}
          />
        )}
      </Row>
      {kind === "ratio" && (
        <Row label="Ratio">
          <Seg
            label="Ratio"
            value={ratioVal}
            onChange={(ratio) => void send({ ratio })}
            options={RATIO_CHOICES.map((n) => {
              const why = aboveDeviceBlock(44100 * n, live.dac) ?? dacHighRateBlock({ ratio: n }, live.dac) ?? presetWhy({ ratio: n });
              // Unavailable even when selected (kept pressed, so what is set shows).
              return [n, `×${n}`, why ?? RATIO_TIPS[n], why !== null];
            })}
          />
          <NumField
            className="ups-num ups-num-short"
            label="Other ×"
            value={ratioVal ?? NaN}
            format={(v) => blank(v, (x) => String(x))}
            title="Any whole multiple the engine takes, from ×1 to ×64 (×5 for 44.1 kHz → 220.5 kHz). The DAC must play the result."
            onCommit={(v) => void send({ ratio: Math.round(clamp(v, 1, MAX_INTEGER_RATIO)) })}
          />
        </Row>
      )}
      {kind === "max" && (
        <Row label="Up to">
          <Seg
            label="Up to"
            value={cap}
            onChange={(c) => void send(capTarget(c))}
            options={CAP_CHOICES.map((c) => {
              const why = aboveDeviceBlock(c, live.dac) ?? dacHighRateBlock(capTarget(c), live.dac) ?? presetWhy(capTarget(c));
              return [c, CAP_LABELS[c] ?? fmtRateShort(c), why ?? CAP_TIPS[c] ?? `Never above ${fmtRate(c)}`, why !== null];
            })}
          />
          <NumField
            className="ups-num ups-num-short"
            label="Other"
            unit="kHz"
            value={cap === null ? NaN : cap / 1000}
            format={(v) => blank(v, (x) => String(Number(x.toFixed(3))))}
            title="Any limit the engine takes, from 44.1 to 3,072 kHz"
            onCommit={(v) => void send(capTarget(Math.round(clamp(v, 44.1, MAX_OUTPUT_RATE / 1000) * 1000)))}
          />
        </Row>
      )}

      {kind === "rate" && (
        <Row label="Output rate">
          <Seg
            className="preview-seg"
            label="Output rate"
            value={fixed}
            onChange={(rate) => void send({ rate })}
            options={FIXED_RATES.map((r) => {
              const tip = `Always ${fmtRate(r)}: ${fixedFrom(r, 44100)}, ${fixedFrom(r, 48000)}`;
              // Past the term limit from an allowed source: the note says why, not the preset.
              const unreachable = unreachableSources({ rate: r }, allowedRates).length > 0;
              const why = dacRateBlock([r], live.dac) ?? (unreachable ? null : presetWhy({ rate: r }));
              return [r, fmtRateShort(r), why ?? tip, why !== null];
            })}
          />
          {fixed === null && !offer && !termNote && <span className="hint">Pick the output rate</span>}
        </Row>
      )}
      {(termNote || termErr) && (
        <div className="notice warn ups-term-note" role="alert">
          {termNote && <span>{termNote}</span>}
          {termErr && <span className="hint">The engine said: {termErr}</span>}
        </div>
      )}
      {offer && (
        <div className="notice warn ups-offer" role="alert">
          <span>Not sent: the custom design does not run at this rate setting. {offer.text} Use a preset, or change the design (a length per input sample suits fractional ratios).</span>
          <button type="button" className="btn small" onClick={() => setOffer(null)}>
            Cancel
          </button>
        </div>
      )}
      {notes.length > 0 || lower ? (
        <div className="ups-landing" aria-live="polite">
          {lower && <span className="hint ups-lands-lower">{lower}</span>}
          {notes.map((n) => (
            <span key={n} className="hint">
              {n}
            </span>
          ))}
        </div>
      ) : null}
    </>
  );
}

// ------------------------------------------------------------ filter

export interface FilterProps {
  u: UpsamplerConfig;
  set: Set;
  allowedRates: readonly number[];
  /** The measured response, when it is for the current design. */
  measured: UpsamplerResponse | null;
  tips: ReturnType<typeof cutoffTips>;
  /** Source rate the readouts are for, and whether it is only assumed (nothing played yet). */
  readoutRate: number;
  readoutAssumed: boolean;
  /** Filter pipelines the engine runs (2, or 4 with A/B on): the fractional cost scales with it. */
  pipelines: number;
}

export function FilterControls(props: FilterProps) {
  const { set } = props;
  // Neither a preset nor a design: the engine runs Recommended.
  const u = !props.u.preset && !props.u.design ? { ...props.u, preset: RECOMMENDED_PRESET } : props.u;
  const st = store();
  const maxTaps = engineLimits(useApp((s) => s.status?.limits)).max_kernel_taps;
  const features = upsamplerFeatures(u);
  const blocked = (n: FilterPreset) => presetBlockReason(n, u.target, props.allowedRates, props.pipelines, maxTaps);
  const err = useApp((s) => s.upsamplerError);
  // A custom design that does not play (yet): being checked, or failed the
  // check or the engine. What played before (a preset too) plays until then.
  const draft = useApp((s) => s.upsDraft);
  // A fresh config has neither a preset nor a design: show the presets.
  const mode = draft === null && (u.preset || !u.design) ? "preset" : "custom";
  const design = draft?.design ?? u.design;
  const check = useDesignCheck(mode === "custom" ? design : null, u.target, props.allowedRates.join(","));
  const refusal = err ? refusalField(err) : null;
  const errors: FieldError[] = [...(check.pending ? [] : check.check?.errors ?? []), ...(refusal && refusal.path !== "target" ? [refusal] : [])];
  const [advancedOpen, setAdvancedOpen] = useState<boolean>(loadAdvancedOpen);
  const toggleAdvanced = () => {
    const next = !advancedOpen;
    setAdvancedOpen(next);
    saveAdvancedOpen(next);
  };
  const edit = (d: FilterDesign, ok?: string) => void st.editCustomDesign(d, ok);
  const liveDraft = draft !== null && !check.pending && (errors.length > 0 || !!err);
  const measured = draft ? null : props.measured;
  return (
    <>
      <Row label="Filter">
        <Seg<"preset" | "custom">
          label="Filter design"
          value={mode}
          onChange={(m) => void st.setUpsamplerMode(m)}
          options={[
            [
              "preset",
              "Preset",
              features.preset ? "Recommended, or a named filter with a fixed recipe: pick a family, then its length, roll-off and phase. Your custom design is kept for when you switch back." : ENGINE_UPDATE_TIP,
              !features.preset,
            ],
            ["custom", "Custom", "Design your own filter: type, window, length, cutoff and phase. It plays as soon as it is valid. With no custom design yet, it starts as a copy of the running preset, so the sound does not change. The last preset is kept for when you switch back."],
          ]}
        />
        {mode === "custom" && design && <BasisNote design={design} />}
      </Row>
      {liveDraft && (
        <div className="notice warn ups-draft" role="status">
          <span>Not applied: {errors.length > 0 ? "fix the marked fields." : "the engine refused it."} The design that plays now stays until then.</span>
          <button type="button" className="btn small" title="Go back to the design that plays now" onClick={() => st.discardUpsDraft()}>
            Discard changes
          </button>
        </div>
      )}
      {mode === "preset" ? (
        <PresetControls u={u} set={set} blocked={blocked} />
      ) : design ? (
        <CustomDesignControls design={design} onEdit={edit} errors={errors} check={check} target={u.target} off={false} measured={measured} readoutRate={props.readoutRate} advancedOpen={advancedOpen} />
      ) : null}
      {mode === "custom" && <div className="ups-adv">
        <button type="button" className="ups-adv-toggle" aria-expanded={advancedOpen} aria-controls="ups-adv-body" onClick={toggleAdvanced}>
          <Chevron className={"chev" + (advancedOpen ? " open" : "")} />
          <span>Advanced</span>
        </button>
        {advancedOpen && (
          <div id="ups-adv-body" className="ups-adv-body">
            {mode === "custom" && design && <AdvancedDesign design={design} onEdit={edit} off={false} errors={errors} measured={measured} />}
          </div>
        )}
      </div>}
      {err && !isTermLimitMessage(err) && !(refusal && refusal.path !== "target" && mode === "custom") && (
        <div className="notice warn ups-engine-err" role="alert">
          <span>The engine refused the change: {err}</span>
        </div>
      )}
    </>
  );
}

const ADVANCED_OPEN_KEY = "avdsp-upsampling-advanced";

function loadAdvancedOpen(): boolean {
  try {
    return localStorage.getItem(ADVANCED_OPEN_KEY) === "1";
  } catch {
    return false;
  }
}

function saveAdvancedOpen(open: boolean): void {
  try {
    localStorage.setItem(ADVANCED_OPEN_KEY, open ? "1" : "0");
  } catch {
    /* storage unavailable */
  }
}

// ------------------------------------------------------------ presets

/** "Edit as custom": copy a preset's exact values into the Custom editor. */
function EditAsCustom({ name, off }: { name: string; off: boolean }) {
  const st = store();
  const id = useId();
  const block = useApp((s) => s.caps.designs) === false ? ENGINE_UPDATE_TIP : null;
  const why = whyFor(id, !off && block !== null, block);
  return (
    <span className="why-wrap">
      <button type="button" className="btn small ups-edit-custom" disabled={off} {...why.attrs} title={block ?? "Switch to Custom with this preset's exact values, so the sound does not change, then change any of them. Each valid change plays at once."} onClick={() => !block && void st.editAsCustom(name)}>
        Edit as custom
      </button>
      {why.tip}
    </span>
  );
}

function PresetControls({ u, set, blocked }: { u: UpsamplerConfig; set: Set; blocked: (n: FilterPreset) => string | null }) {
  const st = store();
  const cur = u.preset && u.preset !== RECOMMENDED_PRESET ? presetEntry(u.preset as FilterPreset) : undefined;
  const recommended = isRecommendedUpsampler(u);
  // The variant last used in each family, so going back to one returns to it.
  const remembered = useRef<Partial<Record<FamilyId, FilterPreset>>>({});
  const idBase = useId();
  if (cur) remembered.current[cur.family] = cur.name;
  // Off: the filter can still be picked; it plays once a rate is on.
  const off = false;
  const recBlock: string | null = null;
  const recWhy = whyFor(`${idBase}-rec`, !off && recBlock !== null, recBlock);
  const recCard = (
    <span className="fam-cell fam-rec">
      <button
        type="button"
        className="fam"
        aria-pressed={recommended}
        disabled={off}
        {...recWhy.attrs}
        title={recBlock ?? RECOMMENDED_TIP}
        onClick={() => {
          if (recBlock || recommended) return;
          void st.setUpsamplerRecommended();
        }}
      >
        <span className="fam-name">{RECOMMENDED_LABEL}</span>
        <span className="fam-blurb">{RECOMMENDED_BLURB}</span>
      </button>
      {recWhy.tip}
    </span>
  );
  const grid = (
    <div className="fam-grid" role="group" aria-label="Filter family">
      {recCard}
      {FAMILIES.map((f) => {
        const pick = familyPick(f.id, remembered.current[f.id], blocked);
        const on = f.id === cur?.family;
        const unavailable = !off && !on && pick === null;
        const why = whyFor(`${idBase}-${f.id}`, unavailable, pick === null ? blocked(f.default) : null);
        return (
          <span key={f.id} className="fam-cell">
            <button
              type="button"
              className="fam"
              aria-pressed={on}
              disabled={off}
              {...why.attrs}
              title={(unavailable && blocked(f.default)) || f.blurb}
              onClick={() => pick && !on && void set({ preset: pick })}
            >
              <span className="fam-name">{f.label}</span>
              <span className="fam-blurb">{f.blurb}</span>
            </button>
            {why.tip}
          </span>
        );
      })}
    </div>
  );
  if (recommended) {
    return (
      <>
        {grid}
        <div className="ups-preset-detail">
          <span>{RECOMMENDED_DETAIL}</span>
          <EditAsCustom name={RECOMMENDED_PRESET} off={off} />
        </div>
      </>
    );
  }
  if (!cur) {
    return (
      <>
        {grid}
        <Row label="Preset">
          <span className="hint">
            {u.preset} (this app does not know this preset yet: update the app). It runs as the engine defines it.
          </span>
        </Row>
      </>
    );
  }
  const curBlocked = blocked(cur.name);
  return (
    <>
      {grid}
      {familyDims(cur.family).map((dim) => {
        const value = presetDimValue(cur, dim);
        const opts = dimOptions(cur, dim, blocked);
        return (
          <Row key={dim} label={dimLabel(cur.family, dim)}>
            <Seg
              label={`${familyInfo(cur.family).label} ${dimLabel(cur.family, dim).toLowerCase()}`}
              value={value}
              disabled={off}
              onChange={(v) => {
                const o = opts.find((x) => x.value === v);
                if (o?.pick) void set({ preset: o.pick });
              }}
              options={opts.map((o) => [o.value, o.label, o.tip, o.pick === null && o.value !== value])}
            />
          </Row>
        );
      })}
      <div className="ups-preset-detail">
        <span>{presetDetail(cur)}</span>
        <span className="ups-preset-name mono" title="The engine's name for this preset: what the command line and bug reports use">
          {cur.name}
        </span>
        <EditAsCustom name={cur.name} off={off} />
      </div>
      {curBlocked && <div className="notice warn">{curBlocked}</div>}
    </>
  );
}
