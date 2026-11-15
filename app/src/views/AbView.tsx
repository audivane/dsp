import { useEffect, useState } from "react";
import { fmtDb, parseNumber } from "../lib/format";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";

export function AbView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const ab = useApp((s) => s.status?.ab ?? null);
  const presets = useApp((s) => s.presets);
  const presetsCap = useApp((s) => s.caps.presets);
  const active = useApp((s) => s.status?.preset?.name ?? s.config?.active_preset ?? null);
  const abCfg = cfg?.ab ?? null;
  const level = abCfg?.level ?? { mode: "analytic" as const };
  const [offset, setOffset] = useState(level.mode === "manual" ? String(level.offset_db) : "0");
  useEffect(() => {
    if (level.mode === "manual") setOffset(String(level.offset_db));
  }, [level]);
  if (!cfg) return null;
  const bName = abCfg?.chain_b.eq?.name || (abCfg ? "Flat" : null);
  const sourceValue = !abCfg ? "off" : presets.some((p) => p.name === bName) ? "p:" + bName : "flat";
  return (
    <section className="view" aria-labelledby="ab-title">
      <div className="view-head">
        <h1 id="ab-title">A/B compare</h1>
      </div>
      <div className="big-ab">
        {(["a", "b"] as const).map((side) => (
          <button key={side} aria-pressed={!!abCfg && (ab?.current ?? "a") === side} disabled={!abCfg} onClick={() => void st.abSelect(side)}>
            {side.toUpperCase()}
            <small>{side === "a" ? (active ?? "Current settings") : (bName ?? "Not set")}</small>
          </button>
        ))}
      </div>
      <p className="lede">
        Press <kbd>A</kbd> or <kbd>B</kbd> anywhere to switch. Both sides run all the time, so the switch is a 10 ms crossfade with no gap. Side B always uses side A's upsampling and output.
      </p>
      <div className="panel">
        <div className="row">
          <span className="k">Side B</span>
          <select
            className="plain"
            aria-label="Side B"
            value={sourceValue}
            onChange={(e) => {
              const v = e.target.value;
              void st.setAbSource(v === "off" ? "off" : v === "flat" ? "flat" : { preset: v.slice(2) });
            }}
          >
            <option value="off">Off (no A/B)</option>
            <option value="flat">Flat (no EQ, no convolution)</option>
            {presetsCap !== false &&
              presets.map((p) => (
                <option key={p.name} value={"p:" + p.name}>
                  {p.name}
                </option>
              ))}
          </select>
        </div>
        <div className="row">
          <span className="k">Level match</span>
          <Seg
            label="Level match"
            value={level.mode}
            disabled={!abCfg}
            onChange={(m) => void st.setLevelMatch(m === "manual" ? { mode: "manual", offset_db: parseNumber(offset) ?? 0 } : m === "off" ? { mode: "off" } : { mode: "analytic" })}
            options={[
              ["analytic", "Analytic (A-weighted)", "Matches loudness from the filters' responses"],
              ["manual", "Manual"],
              ["off", "Off"],
            ]}
          />
          {level.mode === "manual" && (
            <label className="row" style={{ gap: 6 }}>
              <span className="hint">Offset</span>
              <input
                className="num-input"
                value={offset}
                onChange={(e) => setOffset(e.target.value)}
                onBlur={() => {
                  const v = parseNumber(offset);
                  if (v !== null && v !== level.offset_db) void st.setLevelMatch({ mode: "manual", offset_db: Math.max(-60, Math.min(60, v)) });
                }}
                onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
              />
              <span className="hint">dB</span>
            </label>
          )}
        </div>
        <div className="row">
          <span className="k">Gain on B</span>
          <span className="mono">{ab ? `${fmtDb(ab.offset_db)} dB` : "—"}</span>
        </div>
        <div className="row">
          <span className="k">Blind mode</span>
          <span className="hint">Coming later, using Audivane ABX trials</span>
        </div>
      </div>
    </section>
  );
}
