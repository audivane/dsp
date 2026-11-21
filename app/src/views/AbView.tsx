import { useState } from "react";
import { fmtDb, fmtMs } from "../lib/format";
import { MEASURE_MAX_DEFAULT, MEASURE_MIN_SECONDS, MEASURE_STABLE_DB, MEASURE_STABLE_SECONDS, measuredText, measurePhase } from "../lib/abMeasure";
import { abStoppedByOverload, describeB, levelText, oversText, type BKind } from "../lib/abCompare";
import type { AbConfig, AbStatus, LevelMatch } from "../api/types";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { NumField } from "../components/StageParts";

const KIND_TAG: Record<BKind, string> = { flat: "Flat", preset: "Preset", stage: "Stage", filter: "Filter", patch: "Changed" };

const HEADROOM_MAX_DB = 12;
const OFFSET_MAX_DB = 60;

export function AbView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const ab = useApp((s) => s.status?.ab ?? null);
  const overload = useApp((s) => s.status?.overload ?? null);
  const filterLatency = useApp((s) => s.status?.filter_latency_ms ?? null);
  const presets = useApp((s) => s.presets);
  const presetsCap = useApp((s) => s.caps.presets);
  const active = useApp((s) => s.status?.preset?.name ?? s.config?.active_preset ?? null);
  if (!cfg) return null;
  const abCfg: AbConfig | null = cfg.ab ?? null;
  const level: LevelMatch = abCfg?.level ?? { mode: "analytic", weighting: "k" };
  const weighting = level.mode === "analytic" ? level.weighting : "k";
  const headroom = abCfg?.headroom ?? { mode: "auto" as const };
  const b = abCfg ? describeB(abCfg) : null;
  // A stage compare ("Crossfeed on"): say what each side has.
  const flip = b?.kind === "stage" ? /^(.*) (off|on)$/.exec(b.title) : null;
  const aText = flip ? `${flip[1]} ${flip[2] === "on" ? "off" : "on"}` : (active ?? "Your settings");
  const stopped = abStoppedByOverload(abCfg, overload);
  const presetB = abCfg && "chain" in abCfg.b && b && presets.some((p) => p.name === b.title) ? b.title : null;
  const sourceValue = !abCfg ? "off" : b?.kind === "flat" ? "flat" : presetB ? "p:" + presetB : "cur";
  const overs = ab ? oversText(ab.overs) : null;
  // Measured matching ("Match to this track") comes with an engine that
  // reports a measurement in its A/B status; older engines leave it out.
  const canMeasure = !!ab && "measuring" in ab;
  const measuring = !!ab?.measuring;

  return (
    <section className="view" aria-labelledby="ab-title">
      <div className="view-head">
        <h1 id="ab-title">A/B compare</h1>
        <span className={"tag" + (abCfg && !stopped ? " on" : "")}>{!abCfg ? "Off" : stopped ? "Stopped" : "On"}</span>
        <div className="spacer" />
        {abCfg && (
          <button type="button" className="btn small" title="Turn A/B off: only A plays, and B's CPU is freed." onClick={() => void st.setAbSource("off")}>
            Stop compare
          </button>
        )}
      </div>
      {stopped && overload && (
        <div className="notice warn" role="status" title="Running both sides needed more CPU than real time allows. The engine keeps A playing; side B is kept, and choosing a new side B (a lighter filter, say) tries again.">
          <span>{overload.message}</span>
        </div>
      )}
      <div className="big-ab">
        {(["a", "b"] as const).map((side) => (
          <button
            key={side}
            aria-pressed={!!abCfg && !stopped && (ab?.current ?? "a") === side}
            disabled={!abCfg || stopped}
            title={side === "a" ? "Listen to A, your settings (key A)" : "Listen to B (key B)"}
            onClick={() => void st.abSelect(side)}
          >
            {side.toUpperCase()}
            <small>{side === "a" ? aText : (b?.title ?? "Not set")}</small>
          </button>
        ))}
      </div>
      <p className="lede">
        Press <kbd>A</kbd> or <kbd>B</kbd> to pick a side, <kbd>`</kbd> to flip. <b>Compare</b> on a stage's page makes B the same as A with only that stage flipped: off if A has it on, on with that page's settings if A has it off.
      </p>
      <div className="panel">
        <div className="row">
          <span className="k" title="What side B plays. B always runs at A's upsampling ratio, limiter and output.">
            Side B
          </span>
          <select
            className="plain"
            aria-label="Side B"
            value={sourceValue}
            title="Flat and the stage compares follow A as you change it; a preset is fixed."
            onChange={(e) => {
              const v = e.target.value;
              if (v === "cur") return;
              void st.setAbSource(v === "off" ? "off" : v === "flat" ? "flat" : { preset: v.slice(2) });
            }}
          >
            <option value="off">Off (no A/B)</option>
            {sourceValue === "cur" && b && <option value="cur">{b.title}</option>}
            <option value="flat">Flat (no EQ, no convolution)</option>
            {presetsCap !== false &&
              presets.map((p) => (
                <option key={p.name} value={"p:" + p.name}>
                  Preset: {p.name}
                </option>
              ))}
          </select>
        </div>
        {b && (
          <div className="row">
            <span className="k" />
            <span className="ab-desc">
              <span>
                <span className="tag ab-kind">{KIND_TAG[b.kind]}</span>
                <b>{b.title}</b>
              </span>
              <span className="hint">{b.detail}</span>
            </span>
          </div>
        )}
        <div className="row">
          <span className="k" title="How B's level is matched to A's so the louder side does not win. Nothing is ever boosted: the louder side is turned down.">
            Level match
          </span>
          <Seg
            label="Level match"
            value={level.mode}
            disabled={!abCfg}
            onChange={(m) =>
              m === "measured"
                ? void st.abMeasure("start")
                : void st.setLevelMatch(m === "manual" ? { mode: "manual", offset_db: ab ? Math.round(ab.offset_db * 10) / 10 : 0 } : m === "off" ? { mode: "off" } : { mode: "analytic", weighting })
            }
            options={[
              ["analytic", "Analytic", "Matches loudness from the stages' frequency responses, on pink noise. Instant, and right for most music."],
              ...(canMeasure || level.mode === "measured"
                ? [["measured", "Measured", "Measures both sides on the music playing now and locks the offset (Match to this track). Picking it starts a measurement."] as ["measured", string, string]]
                : []),
              ["manual", "Manual", `Your own offset for B, ±${OFFSET_MAX_DB} dB`],
              ["off", "Off", "No matching: hear the real level change (the louder side is still never boosted)"],
            ]}
          />
          {level.mode === "analytic" && (
            <Seg
              label="Weighting"
              className="quiet"
              value={weighting}
              disabled={!abCfg}
              onChange={(w) => void st.setLevelMatch({ mode: "analytic", weighting: w })}
              options={[
                ["k", "K (LUFS)", "K-weighting, as loudness meters (LUFS) use: hears bass changes well. Recommended."],
                ["a", "A", "A-weighting: under-reads bass, so a bass boost can stay louder after matching."],
              ]}
            />
          )}
          {level.mode === "manual" && (
            <NumField
              label="B offset"
              value={level.offset_db}
              unit="dB"
              digits={2}
              title={`dB added to B so it is as loud as A, ±${OFFSET_MAX_DB} dB (negative: B is louder). The louder side is turned down, never up.`}
              check={(v) => (v === null ? "Enter a number" : Math.abs(v) > OFFSET_MAX_DB ? `At most ±${OFFSET_MAX_DB} dB` : null)}
              onCommit={(v) => v !== null && void st.setLevelMatch({ mode: "manual", offset_db: v })}
            />
          )}
        </div>
        {canMeasure && ab && <MatchRow ab={ab} level={level} />}
        <div className="row">
          <span className="k" title="The gains the engine applies now. Both are 0 dB or below: the louder side is cut, and the shared headroom cuts both sides equally.">
            Level
          </span>
          {ab && abCfg ? (
            <span className="ab-desc" style={{ flex: 1 }}>
              <span>{levelText(ab)}</span>
              {level.mode === "measured" && (
                <span className="hint" title="The offset the last measurement locked. It stays until you change the level match or measure again.">
                  {measuredText(level)}.
                </span>
              )}
              {measuring && <span className="hint">Measuring: the current offset stays in force until the measurement locks.</span>}
              <span className="ab-nums">
                <span title="B's level offset: dB that make B as loud as A">
                  <span className="k">B offset</span>
                  <span className="mono">{fmtDb(ab.offset_db, 2)} dB</span>
                </span>
                <span title="Gain on A before the shared headroom (0 dB or below)">
                  <span className="k">A gain</span>
                  <span className="mono">{fmtDb(ab.gain_a_db, 2)} dB</span>
                </span>
                <span title="Gain on B before the shared headroom (0 dB or below)">
                  <span className="k">B gain</span>
                  <span className="mono">{fmtDb(ab.gain_b_db, 2)} dB</span>
                </span>
                <span title="Cut on both sides so B's own boosts do not clip">
                  <span className="k">Headroom</span>
                  <span className="mono">{fmtDb(-ab.headroom_db, 2)} dB</span>
                </span>
              </span>
              {ab.error && (
                <span className="inline-err" role="alert">
                  <span>Level matching is off: {ab.error}</span>
                </span>
              )}
            </span>
          ) : (
            <span className="hint">Shown while A/B is on.</span>
          )}
        </div>
        <div className="row">
          <span
            className="k"
            title="A cut on both sides, after level matching, so B's own boosts (a bass shelf, say) do not clip. It never changes the comparison, only how loud both play."
          >
            Headroom
          </span>
          <Seg
            label="Shared headroom"
            value={headroom.mode}
            disabled={!abCfg}
            onChange={(m) => void st.setAbHeadroom(m === "auto" ? { mode: "auto" } : { mode: "fixed", db: ab ? Math.round(ab.headroom_db * 10) / 10 : 3 })}
            options={[
              ["auto", "Auto", `From each side's loudest possible peak, up to ${HEADROOM_MAX_DB} dB. Recommended.`],
              ["fixed", "Fixed", `Your own cut, 0 to ${HEADROOM_MAX_DB} dB`],
            ]}
          />
          {headroom.mode === "fixed" && (
            <NumField
              label="Headroom"
              value={headroom.db}
              unit="dB"
              title={`Cut on both sides, 0 to ${HEADROOM_MAX_DB} dB`}
              check={(v) => (v === null ? "Enter a number" : v < 0 || v > HEADROOM_MAX_DB ? `0 to ${HEADROOM_MAX_DB} dB` : null)}
              onCommit={(v) => v !== null && void st.setAbHeadroom({ mode: "fixed", db: v })}
            />
          )}
        </div>
        <div className="row">
          <span className="k" title="The two sides can have different delays (a longer filter on B, say). The engine delays the faster side so both line up and a switch does not jump in time.">
            Latency
          </span>
          {abCfg && filterLatency !== null ? (
            <span className="hint">Both sides aligned to the slower one: {fmtMs(filterLatency)} of filter delay.</span>
          ) : (
            <span className="hint">Both sides are aligned to the slower one.</span>
          )}
        </div>
        {overs && (
          <div className="row">
            <span className="k">Overs</span>
            <span className="ab-desc">
              <span className="inline-err ab-overs" role="status">
                <span>{overs} since the last switch.</span>
              </span>
              <span className="hint">The limiter acted on that side, so the comparison is not clean: you may hear the limiter, not the change. Raise the headroom (Fixed) and switch again.</span>
            </span>
          </div>
        )}
        <div className="row">
          <span className="k">Blind test</span>
          <span className="hint">Coming later, using Audivane ABX trials</span>
        </div>
      </div>
    </section>
  );
}

/**
 * "Match to this track": measure both sides on the music playing now and
 * lock B's offset. Progress, the running offset and the outcome come from
 * the engine's A/B status.
 */
function MatchRow({ ab, level }: { ab: AbStatus; level: LevelMatch }) {
  const st = store();
  const [callErr, setCallErr] = useState<string | null>(null);
  const m = measurePhase(ab, level);
  const run = async (action: "start" | "accept" | "cancel") => setCallErr(await st.abMeasure(action));
  const startTitle = `Plays on as now and measures both sides on this track (K-weighted loudness, as LUFS), then locks B's offset: at least ${MEASURE_MIN_SECONDS} s of music, and the offset holding within ${MEASURE_STABLE_DB} dB for ${MEASURE_STABLE_SECONDS} s, or ${MEASURE_MAX_DEFAULT} s at the latest. Silence and quiet passages do not count. No extra CPU.`;
  return (
    <div className="row ab-match">
      <span className="k" title="Matches B's level to A on the music you are playing, instead of on pink noise.">
        Match
      </span>
      <span className="ab-desc" style={{ flex: 1 }}>
        {m.phase === "running" ? (
          <>
            <span className="ab-match-bar" role="progressbar" aria-label="Measurement progress" aria-valuemin={0} aria-valuemax={m.maxSeconds} aria-valuenow={Math.round(m.seconds)} aria-valuetext={m.text}>
              <span style={{ width: `${Math.round(m.fraction * 100)}%` }} />
              <i style={{ left: `${(Math.min(MEASURE_MIN_SECONDS, m.maxSeconds) / Math.min(m.maxSeconds, MEASURE_MIN_SECONDS + MEASURE_STABLE_SECONDS)) * 100}%` }} aria-hidden />
            </span>
            <span className="ab-match-line">
              <span role="status">{m.text}</span>
              <span className="mono" title="B's offset as measured so far. It is not applied until it locks.">
                {m.offsetDb === null ? "B: –" : `B ${fmtDb(m.offsetDb, 2)} dB`}
              </span>
            </span>
            <span className="ab-match-actions">
              <button
                type="button"
                className="btn small"
                disabled={!m.canAccept}
                title={m.canAccept ? "Lock the offset measured so far, now. Less music measured makes it less exact." : "Nothing measured yet: play some music first."}
                onClick={() => void run("accept")}
              >
                Use now
              </button>
              <button type="button" className="btn small" title="Stop measuring and keep the current level." onClick={() => void run("cancel")}>
                Cancel
              </button>
            </span>
          </>
        ) : (
          <>
            <span className="ab-match-line">
              <button type="button" className="btn small" title={startTitle} onClick={() => void run("start")}>
                {m.phase === "idle" ? "Match to this track" : m.phase === "locked" ? "Measure again" : "Try again"}
              </button>
              {m.phase === "idle" && <span className="hint">Measures both sides on the music playing now, then locks the offset.</span>}
              {m.phase === "locked" && <span className="hint">Locked for this track. Measure again after a change of track or of side B.</span>}
            </span>
            {m.phase === "failed" && (
              <span className="inline-err" role="alert">
                <span>{m.text}</span>
              </span>
            )}
          </>
        )}
        {callErr && (
          <span className="inline-err" role="alert">
            <span>{callErr}</span>
          </span>
        )}
      </span>
    </div>
  );
}
