// Pieces every effect view shares: the header with the stage's power button,
// the inline error next to the controls, and the notice for an engine without
// the stage.

import { useRef, useState, type ReactNode } from "react";
import { parseNumber } from "../lib/format";
import { STAGE_NAME, store, useApp, type ErrorSlot, type StageId } from "../store/store";
import { Close, Power } from "./Icons";
import { PresetControl } from "./PresetMenu";
import type { PresetPart } from "../api/types";
import { COMPARE_STAGE_NAME, comparing, stagePatch, type CompareStage } from "../lib/abCompare";

export function StageHead({ id, title, on, onPower, powerTitle, part, children }: { id: string; title: string; on: boolean; onPower?: () => void; powerTitle?: string; part?: PresetPart; children?: ReactNode }) {
  return (
    <div className="view-head">
      {onPower && (
        <button className="pwr stage-pwr" aria-pressed={on} aria-label={`${title} on`} title={powerTitle ?? (on ? `Turn ${title} off` : `Turn ${title} on`)} onClick={onPower}>
          <Power />
        </button>
      )}
      <h1 id={id}>{title}</h1>
      <span className={"tag" + (on ? " on" : "")}>{on ? "On" : "Off"}</span>
      <div className="spacer" />
      {children}
      {part && <PresetControl part={part} />}
    </div>
  );
}

/** The daemon's refusal of the last edit, beside the controls it is about. */
export function InlineError({ slot }: { slot: ErrorSlot }) {
  const text = useApp((s) => s.stageErrors[slot]);
  if (!text) return null;
  return (
    <div className="inline-err" role="alert">
      <span>{text}</span>
      <button className="icon-btn" aria-label="Dismiss" onClick={() => store().clearStageError(slot)}>
        <Close />
      </button>
    </div>
  );
}

/** Shown (and the controls disabled) when the engine lacks the stage. */
export function useStageMissing(id: StageId): boolean {
  return useApp((s) => s.stageCaps[id] === false);
}

export function MissingNotice({ id }: { id: StageId }) {
  return <div className="notice warn">This engine version has no {STAGE_NAME[id].toLowerCase()}. Update the engine to use it.</div>;
}

/** A number typed in a box: commits on Enter or when focus leaves, Escape undoes. */
export function NumField({
  id,
  label,
  value,
  unit,
  onCommit,
  disabled,
  title,
  placeholder,
  digits = 1,
  check,
}: {
  id?: string;
  label: string;
  value: number | null;
  unit?: string;
  onCommit: (v: number | null) => void;
  disabled?: boolean;
  title?: string;
  placeholder?: string;
  digits?: number;
  /** The reason a value would be refused, shown beside the box instead of committing it. */
  check?: (v: number | null) => string | null;
}) {
  const [text, setText] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const cancelled = useRef(false);
  const shown = text ?? (value === null ? "" : String(Number(value.toFixed(digits))));
  return (
    <span className="numfield" title={title}>
      <input
        id={id}
        className="num-input"
        inputMode="decimal"
        aria-label={label}
        aria-invalid={problem ? true : undefined}
        value={shown}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(e) => setText(e.target.value)}
        onFocus={(e) => e.currentTarget.select()}
        onBlur={() => {
          const t = cancelled.current ? null : text;
          cancelled.current = false;
          if (t === null) {
            setText(null);
            setProblem(null);
            return;
          }
          const v = t.trim() === "" ? null : parseNumber(t);
          if (v === null && t.trim() !== "") return setText(null);
          const why = check?.(v) ?? null;
          setProblem(why);
          // A refused value stays in the box beside its reason, so it can be corrected.
          if (why) return;
          setText(null);
          if (v !== value) onCommit(v);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          else if (e.key === "Escape") {
            cancelled.current = true;
            (e.target as HTMLInputElement).blur();
          }
        }}
      />
      {unit && <span className="unit">{unit}</span>}
      {problem && (
        <span className="field-err" role="alert">
          {problem}
        </span>
      )}
    </span>
  );
}

/**
 * A stage header's one-click compare: side B becomes chain A with this
 * stage off (or on, when A has it set up but off) and A/B turns on. While
 * that compare runs it reads "Stop compare" and turns A/B off.
 */
export function CompareButton({ stage }: { stage: CompareStage }) {
  const st = store();
  const cfg = useApp((s) => s.config);
  if (!cfg) return null;
  const made = stagePatch(cfg.chain, stage, st.compareMemory());
  const active = "patch" in made && comparing(cfg.ab, made.patch);
  const name = COMPARE_STAGE_NAME[stage];
  const title =
    "error" in made
      ? made.error
      : active
        ? `Stop comparing: turns A/B off and plays A (${name.toLowerCase()} as you set it).`
        : `Compare A with B = ${made.label.toLowerCase().replace(/^eq/, "EQ").replace(/^dc/, "DC")}: turns A/B on, everything else stays as in A. Switch sides with A, B or the \` key. Costs about one more chain of CPU while on.` +
          (cfg.ab ? " Replaces the current side B." : "");
  return (
    <button
      type="button"
      className={"btn small compare-btn" + (active ? " on" : "")}
      aria-pressed={active}
      aria-disabled={"error" in made || undefined}
      title={title}
      onClick={() => {
        if ("error" in made) st.toast(made.error);
        else void st.compareStage(stage);
      }}
    >
      {active ? "Stop compare" : "Compare"}
    </button>
  );
}
