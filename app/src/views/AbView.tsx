import { useState } from "react";
import { fmtDb, fmtMs } from "../lib/format";
import { MEASURE_MAX_DEFAULT, MEASURE_MIN_SECONDS, MEASURE_STABLE_DB, MEASURE_STABLE_SECONDS, measuredText, measurePhase } from "../lib/abMeasure";
import { abStoppedByOverload, describeB, levelText, oversText } from "../lib/abCompare";
import type { AbConfig, LevelMatch } from "../api/types";
import { store, useApp } from "../store/store";
import { Seg } from "../components/Seg";
import { NumField } from "../components/StageParts";
import { BlindRunPanel, BlindSetup } from "./BlindTest";
import { AbCustomBuilder } from "./AbCustomBuilder";

const HEADROOM_MAX_DB = 12;
const OFFSET_MAX_DB = 60;

export function AbView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const ab = useApp((s) => s.status?.ab ?? null);
  const overload = useApp((s) => s.status?.overload ?? null);
  const active = useApp((s) => s.status?.preset?.name ?? s.config?.active_preset ?? null);
  const blindRun = useApp((s) => s.blindRun);
  if (!cfg) return null;
  const abCfg: AbConfig | null = cfg.ab ?? null;
  const b = abCfg ? describeB(abCfg) : null;
  // A stage compare ("Crossfeed on"): say what each side has.
  const flip = b?.kind === "stage" ? /^(.*) (off|on)$/.exec(b.title) : null;
  const aText = flip ? `${flip[1]} ${flip[2] === "on" ? "off" : "on"}` : (active ?? "Your settings");
  const stopped = abStoppedByOverload(abCfg, overload);
  const live = !!abCfg && !stopped;

  return (
    <section className="view ab-page" aria-labelledby="ab-title">
      <div className="view-head">
        <h1 id="ab-title">A/B compare</h1>
        <span className={"tag" + (live ? " on" : "")}>{!abCfg ? "Off" : stopped ? "Stopped" : "On"}</span>
        <div className="spacer" />
        {abCfg && blindRun?.phase !== "running" && (
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
      {blindRun?.phase === "running" ? (
        <BlindRunPanel run={blindRun} />
      ) : (
        <>
          <div className="ab-top">
            <div className="ab-switch" role="group" aria-label="Side playing">
              {(["a", "b"] as const).map((side) => {
                const on = live && (ab?.current ?? "a") === side;
                const gain = ab && live ? (side === "a" ? ab.gain_a_db : ab.gain_b_db) : null;
                return (
                  <button
                    key={side}
                    type="button"
                    className="ab-side"
                    aria-pressed={on}
                    disabled={!live}
                    title={side === "a" ? "Listen to A, your settings (key A)" : "Listen to B (key B)"}
                    onClick={() => void st.abSelect(side)}
                  >
                    <span className="ab-side-key" aria-hidden>
                      {side.toUpperCase()}
                    </span>
                    <span className="ab-side-text">
                      <span className="ab-side-name">{side === "a" ? aText : (b?.title ?? "Not set")}</span>
                      <span className="ab-side-meta">
                        {gain !== null && (
                          <span className="mono" title={`Gain on ${side.toUpperCase()} before the shared headroom (0 dB or below)`}>
                            {fmtDb(gain, 2)} dB
                          </span>
                        )}
                        {on && <span className="ab-side-playing">Playing</span>}
                      </span>
                    </span>
                  </button>
                );
              })}
            </div>
            <p
              className="ab-keys"
              title="Compare on a stage's page makes B the same as A with only that stage flipped: off if A has it on, on with that page's settings if A has it off."
            >
              <kbd>A</kbd> <kbd>B</kbd> pick a side · <kbd>`</kbd> flip · or press <b>Compare</b> on any stage's page
            </p>
          </div>
          <div className="ab-layout">
            <AbCustomBuilder />
            <div className="ab-side-col">
              <LevelPanel />
              {blindRun ? <BlindRunPanel run={blindRun} /> : <BlindSetup disabled={!live} sideA={aText} sideB={b?.title ?? "B"} />}
            </div>
          </div>
        </>
      )}
    </section>
  );
}

/** Level matching, the shared headroom and the result: one toolbar and one sentence. */
function LevelPanel() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const ab = useApp((s) => s.status?.ab ?? null);
  const overload = useApp((s) => s.status?.overload ?? null);
  const filterLatency = useApp((s) => s.status?.filter_latency_ms ?? null);
  const [callErr, setCallErr] = useState<string | null>(null);
  if (!cfg) return null;
  const abCfg: AbConfig | null = cfg.ab ?? null;
  const live = !!abCfg && !abStoppedByOverload(abCfg, overload);
  const level: LevelMatch = abCfg?.level ?? { mode: "analytic", weighting: "k" };
  const weighting = level.mode === "analytic" ? level.weighting : "k";
  const headroom = abCfg?.headroom ?? { mode: "auto" as const };
  const overs = ab ? oversText(ab.overs) : null;
  // Measured matching ("Match to this track") comes with an engine that
  // reports a measurement in its A/B status; older engines leave it out.
  const canMeasure = !!ab && "measuring" in ab;
  const m = canMeasure && ab ? measurePhase(ab, level) : null;
  const measure = async (action: "start" | "accept" | "cancel") => setCallErr(await st.abMeasure(action));
  const startTitle = `Plays on as now and measures both sides on this track (K-weighted loudness, as LUFS), then locks B's offset: at least ${MEASURE_MIN_SECONDS} s of music, and the offset holding within ${MEASURE_STABLE_DB} dB for ${MEASURE_STABLE_SECONDS} s, or ${MEASURE_MAX_DEFAULT} s at the latest. Silence and quiet passages do not count. No extra CPU.`;

  return (
    <div className="panel ab-level">
      <div className="ab-panel-head">
        <h2 title="How B's level is matched to A's so the louder side does not win. Nothing is ever boosted: the louder side is turned down.">Level</h2>
      </div>
      <div className="ab-tools">
        <span className="ab-tool">
          <span className="k" title="How B's level is matched to A's so the louder side does not win. Nothing is ever boosted: the louder side is turned down.">
            Match
          </span>
          <Seg
            label="Level match"
            value={level.mode}
            disabled={!abCfg}
            onChange={(md) =>
              md === "measured"
                ? void measure("start")
                : void st.setLevelMatch(md === "manual" ? { mode: "manual", offset_db: ab ? Math.round(ab.offset_db * 10) / 10 : 0 } : md === "off" ? { mode: "off" } : { mode: "analytic", weighting })
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
          {m && m.phase !== "running" && (
            <button type="button" className="btn small" disabled={!abCfg} title={startTitle} onClick={() => void measure("start")}>
              {m.phase === "idle" ? "Match to this track" : m.phase === "locked" ? "Measure again" : "Try again"}
            </button>
          )}
        </span>
        <span className="ab-tool">
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
            onChange={(md) => void st.setAbHeadroom(md === "auto" ? { mode: "auto" } : { mode: "fixed", db: ab ? Math.round(ab.headroom_db * 10) / 10 : 3 })}
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
        </span>
      </div>
      {m?.phase === "running" && (
        <div className="ab-measure">
          <span className="ab-match-bar" role="progressbar" aria-label="Measurement progress" aria-valuemin={0} aria-valuemax={m.maxSeconds} aria-valuenow={Math.round(m.seconds)} aria-valuetext={m.text}>
            <span style={{ width: `${Math.round(m.fraction * 100)}%` }} />
            <i style={{ left: `${(Math.min(MEASURE_MIN_SECONDS, m.maxSeconds) / Math.min(m.maxSeconds, MEASURE_MIN_SECONDS + MEASURE_STABLE_SECONDS)) * 100}%` }} aria-hidden />
          </span>
          <span className="ab-match-line">
            <span role="status">{m.text}</span>
            <span className="mono" title="B's offset as measured so far. It is not applied until it locks.">
              {m.offsetDb === null ? "B: –" : `B ${fmtDb(m.offsetDb, 2)} dB`}
            </span>
            <span className="ab-match-actions">
              <button
                type="button"
                className="btn small"
                disabled={!m.canAccept}
                title={m.canAccept ? "Lock the offset measured so far, now. Less music measured makes it less exact." : "Nothing measured yet: play some music first."}
                onClick={() => void measure("accept")}
              >
                Use now
              </button>
              <button type="button" className="btn small" title="Stop measuring and keep the current level." onClick={() => void measure("cancel")}>
                Cancel
              </button>
            </span>
          </span>
          <span className="hint">Measuring: the current offset stays in force until the measurement locks.</span>
        </div>
      )}
      {ab && live ? (
        <p className="ab-result" title="The gains the engine applies now. Both are 0 dB or below: the louder side is cut, and the shared headroom cuts both sides equally.">
          {levelText(ab)}
        </p>
      ) : (
        <p className="ab-result hint">The level match is shown while A/B is on.</p>
      )}
      {level.mode === "measured" && (
        <span className="hint" title="The offset the last measurement locked. It stays until you change the level match or measure again.">
          {measuredText(level)}.{m?.phase === "locked" ? " Measure again after a change of track or of side B." : ""}
        </span>
      )}
      {m?.phase === "idle" && level.mode !== "measured" && <span className="hint">Match to this track measures both sides on the music playing now, then locks the offset.</span>}
      {m?.phase === "failed" && (
        <span className="inline-err" role="alert">
          <span>{m.text}</span>
        </span>
      )}
      {callErr && (
        <span className="inline-err" role="alert">
          <span>{callErr}</span>
        </span>
      )}
      {ab?.error && abCfg && (
        <span className="inline-err" role="alert">
          <span>Level matching is off: {ab.error}</span>
        </span>
      )}
      {overs && (
        <span className="ab-overs">
          <span className="inline-err" role="status">
            <span>{overs} since the last switch.</span>
          </span>
          <span className="hint">The limiter acted on that side, so the comparison is not clean: you may hear the limiter, not the change. Raise the headroom (Fixed) and switch again.</span>
        </span>
      )}
      <span
        className="hint"
        title="The two sides can have different delays (a longer filter on B, say). The engine delays the faster side so both line up and a switch does not jump in time."
      >
        {abCfg && filterLatency !== null ? `Latency: both sides aligned to the slower one, ${fmtMs(filterLatency)} of filter delay.` : "Latency: both sides are aligned to the slower one."}
      </span>
    </div>
  );
}
