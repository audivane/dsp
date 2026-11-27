// The Upsampling view's filter compare: side B is chain A with another
// filter preset at the same ratio (the CLI's `ab filter`).

import type { FilterDesign, FilterPreset, UpsamplerConfig } from "../api/types";
import { comparing, customFilterLabel, filterDesignPatch, filterName, filterPatch } from "../lib/abCompare";
import { engineLimits } from "../lib/engineLimits";
import { RECOMMENDED_PRESET } from "../lib/filterDesign";
import { FAMILIES, familyPresets, PHASE_LABEL, presetBlockReason, rationalPipelines } from "../lib/filterPresets";
import { store, useApp } from "../store/store";

export function FilterCompare({ u }: { u: UpsamplerConfig }) {
  const st = store();
  const ab = useApp((s) => s.config?.ab ?? null);
  const allowedRates = useApp((s) => s.config?.rate.allowed_rates ?? []);
  const maxTaps = engineLimits(useApp((s) => s.status?.limits)).max_kernel_taps;
  if (u.target === "off") return null;
  // A filter compare that runs now: the button stops it.
  const runningUps = ab && "patch" in ab.b ? ((ab.b.patch as { upsampler?: { preset?: string | null; design?: FilterDesign | null } } | null)?.upsampler ?? null) : null;
  const runningPreset = runningUps?.preset ?? null;
  if (runningPreset !== null && comparing(ab, filterPatch(runningPreset).patch)) {
    return (
      <button
        type="button"
        className="btn small compare-btn on"
        aria-pressed
        title={`Stop comparing with ${filterName(runningPreset)}: turns A/B off and plays A.`}
        onClick={() => void st.compareFilter(runningPreset)}
      >
        Stop compare
      </button>
    );
  }
  const runningDesign = runningPreset === null ? (runningUps?.design ?? null) : null;
  if (runningDesign !== null && comparing(ab, filterDesignPatch(runningDesign).patch)) {
    return (
      <button
        type="button"
        className="btn small compare-btn on"
        aria-pressed
        title={`Stop comparing with ${ab?.label || customFilterLabel(runningDesign)}: turns A/B off and plays A.`}
        onClick={() => void st.compareFilterDesign(runningDesign)}
      >
        Stop compare
      </button>
    );
  }
  const current = u.design ? null : (u.preset ?? RECOMMENDED_PRESET);
  // A/B runs both chains: a preset must fit with four pipelines.
  const blocked = (name: FilterPreset) => presetBlockReason(name, u.target, allowedRates, rationalPipelines(true), maxTaps);
  return (
    <>
      {u.design && (
        <button
          type="button"
          className="btn small compare-btn"
          title="Pin the current custom filter design as side B and turn A/B on, so you can edit A or switch A to a preset while comparing."
          onClick={() => void st.compareFilterDesign(u.design!)}
        >
          Pin custom as B
        </button>
      )}
      <label className="chip compare-chip" title="Compare A with B = A using another filter at the same ratio: picks the filter and turns A/B on. Both filters run at once, so this costs about twice the upsampler's CPU while on.">
        <span>Compare</span>
        <select
          aria-label="Compare with another filter"
          value=""
          onChange={(e) => {
            if (e.target.value) void st.compareFilter(e.target.value);
          }}
        >
          <option value="">Filter…</option>
          {current !== RECOMMENDED_PRESET && <option value={RECOMMENDED_PRESET}>Recommended</option>}
          {FAMILIES.map((f) => {
            const list = familyPresets(f.id).filter((p) => p.name !== current);
            if (!list.length) return null;
            return (
              <optgroup key={f.id} label={f.label}>
                {list.map((p) => {
                  const why = blocked(p.name);
                  // Variants that differ only by phase share a label: name the phase.
                  const shared = familyPresets(f.id).some((o) => o.name !== p.name && o.label === p.label);
                  return (
                    <option key={p.name} value={p.name} disabled={!!why} title={why ?? undefined}>
                      {p.label}
                      {shared ? ` · ${PHASE_LABEL[p.phase].toLowerCase()}` : ""}
                    </option>
                  );
                })}
              </optgroup>
            );
          })}
        </select>
      </label>
    </>
  );
}
