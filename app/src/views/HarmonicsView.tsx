import type { HarmonicsCharacter, HarmonicsConfig } from "../api/types";
import { HARMONIC_CHARACTERS, HARMONIC_DB, defaultHarmonics, fmtPercent, harmonicLevels, harmonicsLevelDb, harmonicsOn, harmonicsThdPercent } from "../lib/effects";
import { fmtDb } from "../lib/format";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { CompareButton, InlineError, MissingNotice, StageHead, useStageMissing } from "../components/StageParts";
import { ValueSlider } from "../components/ValueSlider";

type Key = "h2_db" | "h3_db" | "h4_db" | "h5_db";
const ROWS: { key: Key; n: number; title: string; optional: boolean }[] = [
  { key: "h2_db", n: 2, title: "2nd harmonic (an octave up): warmth, the classic triode sound.", optional: false },
  { key: "h3_db", n: 3, title: "3rd harmonic (an octave and a fifth up): density and edge, the pentode sound.", optional: false },
  { key: "h4_db", n: 4, title: "4th harmonic: more warmth on top of the 2nd. Optional.", optional: true },
  { key: "h5_db", n: 5, title: "5th harmonic: more bite on top of the 3rd. Optional.", optional: true },
];

/** The harmonic levels as bars, dBc at full scale. */
function Spectrum({ levels }: { levels: (number | null)[] }) {
  const pct = (db: number | null) => (db === null ? 0 : Math.max(0, Math.min(100, ((db + 120) / 120) * 100)));
  return (
    <div className="harm-bars" role="img" aria-label={`Harmonic levels: ${levels.map((l, i) => `H${i + 2} ${l === null ? "off" : `${l} dB`}`).join(", ")}`}>
      <div className="harm-bar fund">
        <span className="bar" style={{ height: "100%" }} />
        <span className="lbl">H1</span>
      </div>
      {levels.map((l, i) => (
        <div key={i} className={"harm-bar" + (l === null ? " off" : "")}>
          <span className="bar" style={{ height: `${pct(l)}%` }} />
          <span className="lbl">H{i + 2}</span>
          <span className="val mono">{l === null ? "off" : fmtDb(l, 0)}</span>
        </div>
      ))}
    </div>
  );
}

export function HarmonicsView() {
  const st = store();
  useApp((s) => s.config?.chain.harmonics);
  useApp((s) => s.stageDrafts.harmonics);
  const missing = useStageMissing("harmonics");
  const cfg = st.stage("harmonics");
  const on = harmonicsOn(cfg);
  const shown: HarmonicsConfig = cfg ?? defaultHarmonics();
  const levels = harmonicLevels(shown);
  const thd = harmonicsThdPercent(shown);
  const level = harmonicsLevelDb(shown);
  const custom = shown.character === "custom";
  const edit = (fn: (c: HarmonicsConfig) => HarmonicsConfig, mode: "drag" | "commit" = "commit") => st.editStage("harmonics", (c) => fn(c ?? { ...defaultHarmonics(), enabled: false }), mode);
  /** Switch to Custom starting from the levels shown now. */
  const asCustom = (c: HarmonicsConfig): HarmonicsConfig => {
    if (c.character === "custom") return c;
    const [h2, h3, h4, h5] = harmonicLevels(c);
    return { ...c, character: "custom", h2_db: h2, h3_db: h3, h4_db: h4, h5_db: h5 };
  };
  return (
    <section className="view" aria-labelledby="harm-title">
      <StageHead id="harm-title" title="Harmonics" on={on} onPower={missing ? undefined : () => st.setStageEnabled("harmonics", !on)}>
        {!missing && <CompareButton stage="harmonics" />}
      </StageHead>
      {missing && <MissingNotice id="harmonics" />}
      <div className="panel">
        <div className="row">
          <span className="k" title="A tube-like preset, or your own levels.">
            Character
          </span>
          <Seg<HarmonicsCharacter>
            label="Harmonics character"
            value={shown.character}
            disabled={missing}
            onChange={(m) => edit((c) => (m === "custom" ? asCustom(c) : { ...c, character: m }))}
            options={HARMONIC_CHARACTERS.map((h) => [h.character, h.label, h.title] as [HarmonicsCharacter, string, string])}
          />
        </div>
        {ROWS.map((r, i) => {
          const v = levels[i];
          const present = v !== null;
          return (
            <div className="row" key={r.key}>
              <div className="k harm-k">
                <label htmlFor={`harm-${r.n}`} title={`${r.title} Level relative to the fundamental for a full-scale sine, −120 to −20 dB.`}>
                  {r.n === 2 ? "2nd" : r.n === 3 ? "3rd" : `${r.n}th`} harmonic
                </label>
                {r.optional && (
                  <label className="check" title={present ? `Turn the ${r.n}th harmonic off` : `Add a ${r.n}th harmonic`}>
                    <input
                      type="checkbox"
                      checked={present}
                      disabled={missing}
                      onChange={(e) => st.setOptionalHarmonic(r.key as "h4_db" | "h5_db", e.target.checked)}
                    />
                    <span className="sr-only">{r.n}th harmonic on</span>
                  </label>
                )}
              </div>
              <ValueSlider
                id={`harm-${r.n}`}
                label={`${r.n}th harmonic level`}
                value={v ?? -70}
                min={HARMONIC_DB.min}
                max={HARMONIC_DB.max}
                step={1}
                disabled={missing || !present}
                format={(x) => (present ? `${fmtDb(x, 0)} dB` : "off")}
                title={custom ? "−120 to −20 dB below the fundamental" : "Moving it switches to Custom"}
                onChange={(x, mode) => edit((c) => ({ ...asCustom(c), [r.key]: x }), mode)}
                onEnd={() => st.endStageDrag("harmonics")}
              />
            </div>
          );
        })}
        <InlineError slot="harmonics" />
        <div className="harm-summary">
          <Spectrum levels={levels} />
          <div className="facts">
            <div className="fact" title="Total harmonic distortion of a full-scale sine: the harmonics' combined level relative to the fundamental. Quieter music gets much less (each harmonic falls with the level).">
              <div className="k">THD at full scale</div>
              <div className="v">{fmtPercent(thd)}</div>
            </div>
            <div className="fact" title="The stage lowers its output just enough that a full-scale input stays within full scale (an estimate of the engine's exact bound).">
              <div className="k">Level change</div>
              <div className="v">{fmtDb(level, 2)} dB</div>
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}
