import { useState } from "react";
import {
  buttonsFor,
  choicesFor,
  clampTrials,
  FEEDBACK_TEXT,
  fmtCi,
  fmtP,
  fmtTrialMs,
  MODE_TEXT,
  neededText,
  runningText,
  scoreRun,
  TRIALS_DEFAULT,
  TRIALS_MAX,
  TRIALS_MIN,
  type BlindButton,
  type BlindChoice,
  type BlindFeedback,
  type BlindMode,
  type BlindPlan,
} from "../lib/abBlind";
import { store, type BlindRun } from "../store/store";
import { Seg } from "../components/Seg";
import { NumField } from "../components/StageParts";

const BUTTON_TIP: Record<BlindButton, string> = {
  a: "Listen to A (key A)",
  b: "Listen to B (key B)",
  x: "Listen to X: secretly A or B, fixed for this trial (key X)",
  "1": "Listen to 1 (key 1)",
  "2": "Listen to 2 (key 2)",
};

function choiceLabel(mode: BlindMode, c: BlindChoice): string {
  if (mode === "abx") return `X is ${c}`;
  return mode === "identify" ? `${c} is B` : `I prefer ${c}`;
}

/** The setup row in the A/B view: mode, trials and feedback, fixed before the start. */
export function BlindSetup({ disabled, sideA, sideB }: { disabled: boolean; sideA: string; sideB: string }) {
  const st = store();
  const [plan, setPlan] = useState<BlindPlan>({ mode: "abx", trials: TRIALS_DEFAULT, feedback: "end" });
  const [err, setErr] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  return (
    <div className="panel blind-setup">
      <h2>Blind test</h2>
      <div className="row">
        <span className="k" title="ABX: switch between A, B and X, then say whether X is A or B. Blind A/B: two shuffled buttons, 1 and 2.">
          Test
        </span>
        <Seg
          label="Blind test kind"
          value={plan.mode}
          onChange={(mode) => setPlan({ ...plan, mode })}
          options={[
            ["abx", "ABX", "Switch freely between A, B and X (secretly A or B), then answer: is X A or B?"],
            ["identify", "Which is B?", "Two buttons, 1 and 2, are A and B in a shuffled order each trial: say which one is B."],
            ["prefer", "Preference", "Two buttons, 1 and 2, are A and B in a shuffled order each trial: say which one you prefer."],
          ]}
        />
      </div>
      <div className="row">
        <span className="k" title={`Fixed before the start, ${TRIALS_MIN} to ${TRIALS_MAX}. A run stopped early is not scored.`}>
          Trials
        </span>
        <NumField
          label="Trials"
          value={plan.trials}
          digits={0}
          title={`${TRIALS_MIN} to ${TRIALS_MAX} trials`}
          check={(v) => (v === null || !Number.isInteger(v) ? "Enter a whole number" : v < TRIALS_MIN || v > TRIALS_MAX ? `${TRIALS_MIN} to ${TRIALS_MAX}` : null)}
          onCommit={(v) => v !== null && setPlan({ ...plan, trials: clampTrials(v) })}
        />
        <span className="hint">{neededText(plan)}</span>
      </div>
      <div className="row">
        <span className="k" title="When you see whether your answers were right. Fixed for the run; either way the run has its full number of trials and a run stopped early is not scored.">
          Feedback
        </span>
        <Seg<BlindFeedback>
          label="Feedback"
          value={plan.feedback}
          onChange={(feedback) => setPlan({ ...plan, feedback })}
          options={[
            ["end", FEEDBACK_TEXT.end, "Answers stay hidden until the last trial. Use this for a real test."],
            ["each", FEEDBACK_TEXT.each, "Shows right or wrong and the running score after each answer: training, to learn what to listen for."],
          ]}
        />
      </div>
      <div className="row">
        <span className="k" />
        <button
          type="button"
          className="btn primary"
          disabled={disabled || starting}
          title={disabled ? "Turn A/B on (choose side B above) first." : "The level match and both sides are locked for the run; changing either ends it."}
          onClick={async () => {
            setStarting(true);
            setErr(await st.blindStart(plan, sideA, sideB));
            setStarting(false);
          }}
        >
          Start blind test
        </button>
        <span className="hint">Uses the music playing now. The level match stays as it is; the meters and over counts are hidden until the end.</span>
        {err && (
          <span className="inline-err" role="alert">
            <span>{err}</span>
          </span>
        )}
      </div>
    </div>
  );
}

/** A running blind test, or its result. */
export function BlindRunPanel({ run }: { run: BlindRun }) {
  if (run.phase === "running") return <Running run={run} />;
  return <Outcome run={run} />;
}

function Running({ run }: { run: BlindRun }) {
  const st = store();
  const mode = run.plan.mode;
  const last = run.records[run.records.length - 1];
  const showLast = run.plan.feedback === "each" && last;
  return (
    <div className="panel blind-run">
      <div className="row blind-head">
        <h2>
          {MODE_TEXT[mode].title} · Trial {run.trial + 1} of {run.plan.trials}
        </h2>
        <span className="tag">{run.plan.feedback === "each" ? "Practice" : "Answers hidden until the end"}</span>
        <div className="spacer" />
        <button type="button" className="btn small" title="Stop now. A run stopped early is not scored." onClick={() => st.blindAbandon("Stopped before the last trial.")}>
          Abandon
        </button>
      </div>
      <div className="big-ab">
        {buttonsFor(mode).map((b) => (
          <button key={b} aria-pressed={run.playing === b} title={BUTTON_TIP[b]} onClick={() => void st.blindPlay(b)}>
            {b.toUpperCase()}
            <small>{b === "a" ? run.sideA : b === "b" ? run.sideB : b === "x" ? "A or B?" : "A or B"}</small>
          </button>
        ))}
      </div>
      <div className="row blind-answer">
        <span className="k">{MODE_TEXT[mode].question}</span>
        {choicesFor(mode).map((c, i) => (
          <button key={c} type="button" className="btn" disabled={!run.playing} title={`${choiceLabel(mode, c)} (key ${i === 0 ? "F" : "J"})`} onClick={() => st.blindAnswer(c)}>
            {choiceLabel(mode, c)} <kbd>{i === 0 ? "F" : "J"}</kbd>
          </button>
        ))}
      </div>
      {showLast && (
        <div className={"notice" + (last.correct === false ? " warn" : "")} role="status">
          Trial {last.index}: {last.correct === null ? `you preferred ${last.answer}` : last.correct ? "correct" : "wrong"}
          {mode === "abx" ? ` (X was ${last.truth})` : mode === "identify" ? ` (B was ${last.truth === "B" ? "1" : "2"})` : ""} · {runningText(mode, run.records)}
        </div>
      )}
      <p className="lede">
        Press {mode === "abx" ? <><kbd>A</kbd>, <kbd>B</kbd> or <kbd>X</kbd></> : <><kbd>1</kbd> or <kbd>2</kbd></>} to listen, as often as you like, then answer with <kbd>F</kbd> or <kbd>J</kbd>. Leaving this view, turning A/B off or changing either side ends the run unscored.
      </p>
      <div className="row">
        <span className="k" title="SHA-256 of a secret drawn at the start, which fixes every trial's hidden side. The secret is revealed at the end, so the run can be checked.">
          Commitment
        </span>
        <span className="mono blind-hash">{run.commitment}</span>
      </div>
    </div>
  );
}

function Outcome({ run }: { run: BlindRun }) {
  const st = store();
  const mode = run.plan.mode;
  const done = run.phase === "done";
  const r = done ? scoreRun(mode, run.records) : null;
  return (
    <div className="panel blind-run">
      <div className="row blind-head">
        <h2>
          {MODE_TEXT[mode].title}: {done ? "result" : "abandoned"}
        </h2>
        <span className="tag">Feedback: {FEEDBACK_TEXT[run.plan.feedback]}</span>
      </div>
      {r ? (
        <div className={r.pass ? "result" : "notice"} role="status">
          <b>{mode === "prefer" ? `B preferred in ${r.k} of ${r.n}` : `${r.k} of ${r.n} correct`}</b> · {fmtP(r.p)}
          {mode === "prefer" ? " (two-sided)" : " (one-tailed)"} · 95 % interval {fmtCi(r.ci)}
          <br />
          {mode === "prefer"
            ? r.pass
              ? `A preference for ${r.preferred} (α = 0.05).`
              : `No clear preference (α = 0.05${r.needed === null ? "" : `; ${r.needed} of ${r.n} the same way needed`}).`
            : r.pass
              ? "Passed (α = 0.05): you told them apart."
              : `Not passed (α = 0.05${r.needed === null ? ": too few trials to pass" : `; ${r.needed} correct needed`}): no reliable difference shown.`}
        </div>
      ) : (
        <div className="notice warn" role="status">
          {run.reason} Not scored: {run.records.length} of {run.plan.trials} trials answered. A run counts only with all its trials.
        </div>
      )}
      {run.records.length > 0 && (
        <div className="table-wrap">
          <table className="streams blind-trials">
            <thead>
              <tr>
                <th>Trial</th>
                <th>Answer</th>
                <th>{mode === "abx" ? "X was" : "1 was"}</th>
                <th>{mode === "prefer" ? "" : "Right"}</th>
                <th>Time</th>
                <th>Switches</th>
              </tr>
            </thead>
            <tbody>
              {run.records.map((t) => (
                <tr key={t.index}>
                  <td className="mono">{t.index}</td>
                  <td>{choiceLabel(mode, t.choice)}{mode === "abx" ? "" : ` (${t.answer})`}</td>
                  <td className="mono">{t.truth}</td>
                  <td>{t.correct === null ? "" : t.correct ? "✓" : "✗"}</td>
                  <td className="mono">{fmtTrialMs(t.ms)}</td>
                  <td className="mono">{t.switches}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <div className="row">
        <span className="k" title="Shown before the first trial: SHA-256 of the secret below.">
          Commitment
        </span>
        <span className="mono blind-hash">{run.commitment}</span>
      </div>
      <div className="row">
        <span className="k" title="Revealed now. Each trial's hidden side is the low bit of SHA-256(secret, trial number); the export lets anyone check it against the commitment.">
          Secret
        </span>
        <span className="mono blind-hash">{run.secretHex}</span>
      </div>
      <div className="row">
        <span className="k" />
        <button type="button" className="btn primary" onClick={() => st.blindClose()}>
          New test
        </button>
        <button type="button" className="btn" title="The run, its answers, the commitment and the secret, as JSON, to keep or check later." onClick={() => void st.blindCopyExport()}>
          Copy as JSON
        </button>
      </div>
    </div>
  );
}

