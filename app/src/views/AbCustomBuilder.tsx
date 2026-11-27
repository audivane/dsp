// The A/B view's "Side B" panel: one picker (Off, Flat, the presets, saved
// custom Bs, custom) and one editor whose rows always show what B is: what
// plays now, or the changes not applied yet. B as a custom is chain A plus a
// set of changes (harmonics, the upsampling filter, EQ, tilt, crossfeed,
// convolution, DC block, pitch, output bits and dither), sent as one merge
// patch.

import { useEffect, useState } from "react";
import type { AbConfig, ChainConfig, DitherKind, FilterPreset } from "../api/types";
import { COMPARE_STAGE_NAME, comparing, describeB, filterName, stagePatch, type BKind, type CompareMemory, type CompareStage } from "../lib/abCompare";
import { chainB } from "../lib/abPatch";
import {
  buildCustom,
  CUSTOM_STAGES,
  EMPTY_SEL,
  everythingOffSel,
  NOS_PRESET,
  selFromChain,
  selIsEmpty,
  stageState,
  type CustomSel,
  type StageTarget,
} from "../lib/abCustom";
import { deepEqual } from "../lib/chain";
import { DITHERS, ditherInfo } from "../lib/effects";
import { engineLimits } from "../lib/engineLimits";
import { FAMILIES, familyPresets, PHASE_LABEL, presetBlockReason, rationalPipelines } from "../lib/filterPresets";
import { RECOMMENDED_PRESET } from "../lib/filterDesign";
import type { HarmonicsSystem } from "../api/types";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";

const NOS_TIP =
  "NOS: no reconstruction filter (sample hold), at the same output rate as A. A real “upsampling off” would play at the source rate and leave the filtering to the DAC, but both sides share one output rate, so it needs the DAC reopened at a new rate, with a gap. NOS is the seamless stand-in.";

const KIND_TAG: Record<BKind, string> = { flat: "Flat", preset: "Preset", stage: "Stage", filter: "Filter", output: "Output", custom: "Custom", patch: "Changed" };

/** Side B as it plays now, as the editor's rows (label kept when it is the user's own). */
function playingSel(a: ChainConfig, ab: AbConfig | null, systems: readonly HarmonicsSystem[], memory: CompareMemory): CustomSel {
  if (!ab) return EMPTY_SEL;
  const sel = selFromChain(a, chainB(ab, a), systems, memory);
  const auto = buildCustom(a, sel, systems, memory).label;
  return ab.label && ab.label !== auto ? { ...sel, label: ab.label } : sel;
}

export function AbCustomBuilder() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const abCfg = useApp((s) => s.config?.ab ?? null);
  const draft = useApp((s) => s.abCustomSel);
  const saved = useApp((s) => s.abCustomSaved);
  const systems = useApp((s) => s.harmonicsSystems);
  const allPresets = useApp((s) => s.presets);
  // Global presets, and the EQ page's (B is chain A with that EQ).
  const presets = allPresets.filter((p) => !p.part);
  const eqPresets = allPresets.filter((p) => p.part === "eq");
  const presetsCap = useApp((s) => s.caps.presets);
  const blindRunning = useApp((s) => s.blindRun?.phase === "running");
  const allowedRates = useApp((s) => s.config?.rate.allowed_rates ?? []);
  const maxTaps = engineLimits(useApp((s) => s.status?.limits)).max_kernel_taps;
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    void st.loadHarmonicsSystems();
  }, [st]);
  if (!cfg) return null;
  const a = cfg.chain;
  const sys = systems ?? [];
  const memory = st.compareMemory();
  const b = abCfg ? describeB(abCfg) : null;
  const playing = playingSel(a, abCfg, sys, memory);
  const sel = draft ?? playing;
  const built = buildCustom(a, sel, sys, memory);
  const autoLabel = buildCustom(a, { ...sel, label: undefined }, sys, memory).label;
  const applied = !!abCfg && !!built.patch && comparing(abCfg, built.patch) && abCfg.label === built.label;
  const pending = draft !== null && !applied && !deepEqual(draft, playing);
  const disabled = busy || blindRunning;

  /** Edit the rows: the first edit starts from what B plays (a preset's or a stage compare's name dropped). */
  const edit = (fn: (s: CustomSel) => CustomSel) => {
    setErr(null);
    const base = draft ?? (b?.kind === "custom" || b?.kind === "patch" ? playing : { ...playing, label: undefined });
    st.setAbCustomSel(fn(base));
  };
  const run = async (patch: Record<string, unknown>, label: string) => {
    setBusy(true);
    setErr(await st.setAbCustom(patch, label));
    setBusy(false);
  };

  // ------------------------------------------------------------ picker
  const presetB = abCfg && "chain" in abCfg.b && b && presets.some((p) => p.name === b.title) ? b.title : null;
  const eqPresetB = !presetB && abCfg && "chain" in abCfg.b && b && eqPresets.some((p) => p.name === b.title) ? b.title : null;
  const savedB = abCfg && "patch" in abCfg.b ? saved.find((s) => abCfg.label === s.name && comparing(abCfg, buildCustom(a, s.sel, sys, memory).patch)) : undefined;
  const pickValue = draft !== null ? "custom" : !abCfg ? "off" : b?.kind === "flat" ? "flat" : presetB ? "p:" + presetB : eqPresetB ? "e:" + eqPresetB : savedB ? "s:" + savedB.name : "cur";
  const pick = (v: string) => {
    setErr(null);
    if (v === "cur") return st.setAbCustomSel(null);
    if (v === "custom") return edit((s) => s);
    if (v.startsWith("s:")) {
      const s = saved.find((x) => x.name === v.slice(2));
      if (!s) return;
      const sb = buildCustom(a, { ...s.sel, label: s.name }, sys, memory);
      // Nothing to send, or it cannot be built: show it in the rows instead.
      if (!sb.patch || sb.errors.length) return st.setAbCustomSel({ ...structuredClone(s.sel), label: s.name });
      return void run(sb.patch, sb.label);
    }
    void st.setAbSource(v === "off" ? "off" : v === "flat" ? "flat" : v.startsWith("e:") ? { preset: v.slice(2), part: "eq" } : { preset: v.slice(2) });
  };

  // ------------------------------------------------------------ rows
  const setStage = (stage: CompareStage, v: StageTarget, aOn: boolean) =>
    edit((s) => {
      const stages = { ...s.stages };
      if ((v === "on") === aOn) delete stages[stage];
      else stages[stage] = v;
      return { ...s, stages, ...(stage === "harmonics" ? { harmonicsSystem: undefined } : {}) };
    });
  const ups = a.upsampler;
  const design = st.customDesignForB();
  const aFilter = ups.target === "off" ? "Off" : !ups.preset && ups.design ? "Custom" : filterName(ups.preset ?? RECOMMENDED_PRESET);
  const filterValue = !sel.filter ? "" : "design" in sel.filter ? "design" : "p:" + sel.filter.preset;
  const aHarm = stageState(a, "harmonics", memory) === "on";
  const harmValue = sel.harmonicsSystem ? "s:" + sel.harmonicsSystem : (sel.stages.harmonics ?? (aHarm ? "on" : "off"));
  const aSystem = a.harmonics?.system ? sys.find((s) => s.id === a.harmonics!.system) : undefined;
  const bits = sel.output?.bits ?? a.output.bits;
  const dither = sel.output?.dither ?? a.output.dither;
  const setOutput = (nb: number, d: DitherKind) => edit((s) => ({ ...s, output: nb === a.output.bits && d === a.output.dither ? undefined : { bits: nb, dither: d } }));
  const aDither = ditherInfo(a.output.dither)?.label ?? a.output.dither;

  return (
    <div className="panel ab-sideb">
      <div className="ab-panel-head">
        <h2 title="What side B plays. B always runs at A's upsampling ratio, limiter and output headroom.">Side B</h2>
        <select className="plain ab-pick" aria-label="Side B" value={pickValue} disabled={disabled} title="Flat, the stage compares and custom Bs follow A as you change it; a preset is fixed." onChange={(e) => pick(e.target.value)}>
          <option value="off">Off (no A/B)</option>
          <option value="flat">Flat (no EQ, no convolution)</option>
          {pickValue === "cur" && b && (
            <option value="cur">
              {KIND_TAG[b.kind]}: {b.title}
            </option>
          )}
          {presetsCap !== false && presets.length > 0 && (
            <optgroup label="Presets">
              {presets.map((p) => (
                <option key={p.name} value={"p:" + p.name}>
                  {p.name}
                </option>
              ))}
            </optgroup>
          )}
          {presetsCap !== false && eqPresets.length > 0 && (
            <optgroup label="EQ presets">
              {eqPresets.map((p) => (
                <option key={p.name} value={"e:" + p.name}>
                  {p.name}
                </option>
              ))}
            </optgroup>
          )}
          {saved.length > 0 && (
            <optgroup label="Saved custom">
              {saved.map((s) => (
                <option key={s.name} value={"s:" + s.name}>
                  {s.name}
                </option>
              ))}
            </optgroup>
          )}
          <option value="custom">Custom (edit the rows)</option>
        </select>
        <span className="spacer" />
        <button type="button" className="btn small" disabled={disabled} title="Every optional stage off, and upsampling NOS (no filter, same rate). The limiter and output stay as A's. Not applied until you press Apply." onClick={() => edit((s) => ({ ...everythingOffSel(), label: s.label }))}>
          Everything off
        </button>
        <button type="button" className="btn small" disabled={disabled || selIsEmpty(sel)} title="Every row back to A's setting (B = A). Not applied until you press Apply." onClick={() => edit(() => ({ ...EMPTY_SEL }))}>
          Reset to A
        </button>
      </div>
      {b ? (
        <p className="ab-now">
          <span className="tag">{KIND_TAG[b.kind]}</span>
          <b>{b.title}</b>
          <span className="hint">{b.detail}</span>
        </p>
      ) : (
        <p className="ab-now hint">A/B is off. Pick a side B above, change a row and press Compare, or press Compare on a stage's page.</p>
      )}
      <div className="ab-rows" role="table" aria-label="Side B against A">
        {[0, 1].map((i) => (
          <div key={i} className={"ab-row ab-row-head" + (i ? " second" : "")} role="row">
            <span role="columnheader">Stage</span>
            <span role="columnheader">A</span>
            <span role="columnheader">B</span>
          </div>
        ))}
        <Row name="Harmonics" tip="Harmonics off, on with A's own layers, or on with one of the built-in systems (its layers replace A's in B)." a={aHarm ? (aSystem?.name ?? "On") : "Off"} diff={!!sel.harmonicsSystem || (sel.stages.harmonics !== undefined && (sel.stages.harmonics === "on") !== aHarm)}>
          <select
            className="plain"
            aria-label="Harmonics in B"
            disabled={disabled}
            value={harmValue}
            onChange={(e) => {
              const v = e.target.value;
              edit((s) => {
                const stages = { ...s.stages };
                delete stages.harmonics;
                if ((v === "on" || v === "off") && (v === "on") !== aHarm) stages.harmonics = v;
                return { ...s, stages, harmonicsSystem: v.startsWith("s:") ? v.slice(2) : undefined };
              });
            }}
          >
            <option value="off">Off</option>
            <option value="on">On (A's layers)</option>
            {sys.map((s) => (
              <option key={s.id} value={"s:" + s.id} title={s.sound}>
                System: {s.name}
              </option>
            ))}
          </select>
        </Row>
        <Row name="Upsampling" tip={NOS_TIP} a={aFilter} diff={!!sel.filter}>
          <select
            className="plain"
            aria-label="Upsampling filter in B"
            disabled={disabled || ups.target === "off"}
            value={filterValue}
            title={ups.target === "off" ? "A does not upsample: B cannot change the filter." : NOS_TIP}
            onChange={(e) => {
              const v = e.target.value;
              edit((s) => {
                const pinned = s.filter && "design" in s.filter ? s.filter.design : design;
                return { ...s, filter: v === "" ? undefined : v === "design" ? (pinned ? { design: pinned } : undefined) : { preset: v.slice(2) } };
              });
            }}
          >
            <option value="">Same as A</option>
            <option value={"p:" + NOS_PRESET} title={NOS_TIP}>
              NOS (no filter, same rate)
            </option>
            {(design || (sel.filter && "design" in sel.filter)) && <option value="design">Custom design</option>}
            <option value={"p:" + RECOMMENDED_PRESET}>Recommended</option>
            {FAMILIES.map((f) => {
              const list = familyPresets(f.id).filter((p) => p.name !== NOS_PRESET);
              if (!list.length) return null;
              return (
                <optgroup key={f.id} label={f.label}>
                  {list.map((p) => {
                    const why = presetBlockReason(p.name as FilterPreset, ups.target, allowedRates, rationalPipelines(true), maxTaps);
                    const shared = familyPresets(f.id).some((o) => o.name !== p.name && o.label === p.label);
                    return (
                      <option key={p.name} value={"p:" + p.name} disabled={!!why} title={why ?? undefined}>
                        {p.label}
                        {shared ? ` · ${PHASE_LABEL[p.phase].toLowerCase()}` : ""}
                      </option>
                    );
                  })}
                </optgroup>
              );
            })}
          </select>
        </Row>
        {CUSTOM_STAGES.filter((s) => s !== "harmonics").map((stage) => {
          const name = COMPARE_STAGE_NAME[stage];
          const aSt = stageState(a, stage, memory);
          const aOn = aSt === "on";
          const bVal = sel.stages[stage] ?? (aOn ? "on" : "off");
          const why = aSt === "none" ? (stagePatch(a, stage, memory) as { error: string }).error : null;
          return (
            <Row key={stage} name={name} a={aSt === "none" ? "Off" : aOn ? "On" : "Off"} aTip={aSt === "none" ? "Not set up in A" : undefined} diff={(bVal === "on") !== aOn}>
              <Seg
                label={`${name} in B`}
                disabled={disabled}
                value={bVal}
                onChange={(v) => setStage(stage, v, aOn)}
                options={[
                  ["off", "Off", `${name} off in B`],
                  ["on", "On", why ?? `${name} on in B, with A's settings`, !!why && bVal !== "on"],
                ]}
              />
            </Row>
          );
        })}
        <Row name="Output" tip="B's own bit depth and dither: a second quantizer runs on the same mix. A switch between them is a hard cut." a={`${a.output.bits}-bit`} aTip={`${a.output.bits}-bit, ${aDither}`} diff={!!sel.output} wide>
          <span className="ab-out">
            <label className="chip">
              <select aria-label="B bit depth" disabled={disabled} value={bits} onChange={(e) => setOutput(Number(e.target.value), dither)}>
                {[16, 20, 24, 32].map((nb) => (
                  <option key={nb} value={nb}>
                    {nb}-bit{nb === a.output.bits ? " (A)" : ""}
                  </option>
                ))}
              </select>
            </label>
            <label className="chip">
              <select aria-label="B dither" disabled={disabled} value={dither} onChange={(e) => setOutput(bits, e.target.value as DitherKind)}>
                {DITHERS.map((d) => (
                  <option key={d.kind} value={d.kind}>
                    {d.label}
                    {d.kind === a.output.dither ? " (A)" : ""}
                  </option>
                ))}
              </select>
            </label>
          </span>
        </Row>
      </div>
      {(built.notes.length > 0 || built.errors.length > 0 || err) && (
        <div className="ab-msgs">
          {built.notes.map((n) => (
            <span key={n} className="hint">
              {n}
            </span>
          ))}
          {built.errors.map((e) => (
            <span key={e} className="inline-err" role="alert">
              <span>{e}</span>
            </span>
          ))}
          {err && (
            <span className="inline-err" role="alert">
              <span>{err}</span>
            </span>
          )}
        </div>
      )}
      {pending && (
        <div className="ab-pending" role="status">
          <span className="tag warn">Not applied yet</span>
          <span className="ab-pending-what">{built.changes.length ? "B: " + built.changes.join(", ") : "B: same as A (change a row)"}</span>
          <span className="spacer" />
          <button type="button" className="btn small" disabled={busy} title="Drop these changes: the rows show B as it plays." onClick={() => (setErr(null), st.setAbCustomSel(null))}>
            Discard
          </button>
          <button
            type="button"
            className="btn primary small"
            disabled={disabled || !built.patch || built.errors.length > 0}
            title="Make this side B and turn A/B on (a filter change builds the filter first). Blind tests use it as it is."
            onClick={() => built.patch && void run(built.patch, built.label)}
          >
            {busy ? "Applying…" : abCfg ? "Apply" : "Compare"}
          </button>
        </div>
      )}
      <div className="ab-name">
        <input
          className="text-input"
          aria-label="Name of B"
          title="What B is called in the A/B switch and blind test results, and the name it is saved under. Empty: made from the changes."
          disabled={disabled}
          value={sel.label ?? ""}
          placeholder={autoLabel}
          maxLength={40}
          onChange={(e) => edit((s) => ({ ...s, label: e.target.value || undefined }))}
        />
        <button
          type="button"
          className="btn small"
          disabled={selIsEmpty(sel)}
          title="Save these changes under this name, kept in this app on this computer (an existing one of that name is replaced). Saved Bs are in the Side B list, applied to A as it is when you pick one."
          onClick={() => st.saveCustomB((sel.label?.trim() || autoLabel).slice(0, 40), sel)}
        >
          Save
        </button>
        {saved.length > 0 && (
          <span className="ab-saved-list">
            <span className="hint">Saved:</span>
            {saved.map((s) => (
              <span key={s.name} className="ab-saved">
                <button type="button" className="btn small" disabled={disabled} aria-pressed={savedB?.name === s.name} title={`Make “${s.name}” side B`} onClick={() => pick("s:" + s.name)}>
                  {s.name}
                </button>
                <button type="button" className="btn small" aria-label={`Delete ${s.name}`} title={`Delete “${s.name}”`} onClick={() => st.deleteCustomB(s.name)}>
                  ×
                </button>
              </span>
            ))}
          </span>
        )}
      </div>
    </div>
  );
}

function Row({ name, tip, a, aTip, diff, wide, children }: { name: string; tip?: string; a: string; aTip?: string; diff: boolean; wide?: boolean; children: React.ReactNode }) {
  return (
    <div className={"ab-row" + (diff ? " diff" : "") + (wide ? " wide" : "")} role="row">
      <span className="ab-row-name" role="rowheader" title={tip}>
        {name}
      </span>
      <span className="ab-row-a" role="cell" title={aTip ?? a}>
        {a}
      </span>
      <span className="ab-row-b" role="cell">
        {children}
      </span>
    </div>
  );
}
