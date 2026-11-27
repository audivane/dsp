import { useEffect, useRef, useState } from "react";
import type { PresetInfo, PresetPart } from "../api/types";
import { PART_HOLDS, PART_NAME, PRESET_PARTS, presetCovers } from "../lib/presetParts";
import { store, useApp } from "../store/store";
import { ImportApoButton } from "../views/EqView";
import { Check, ChevronDown } from "./Icons";

type Pane = { kind: "main" } | { kind: "rename" } | { kind: "delete" };

function bandsText(n: number) {
  return n === 1 ? "1 band" : `${n} bands`;
}

/** What a menu entry holds, in a few words. */
function presetSub(p: PresetInfo, part?: PresetPart): string {
  if (part === "eq") return bandsText(p.bands);
  if (part === "convolution") return p.convolution === 1 ? "1 IR" : `${p.convolution} IRs`;
  if (part) return "";
  const parts = presetCovers(p);
  if (parts.length < PRESET_PARTS.length) return parts.length <= 2 ? parts.map((x) => SHORT_PART[x]).join(" + ") : `${parts.length} parts`;
  return bandsText(p.bands) + (p.convolution > 0 ? ` · ${p.convolution} IR` : "");
}

const SHORT_PART: Record<PresetPart, string> = {
  eq: "EQ",
  convolution: "Convolution",
  stereo: "Stereo",
  harmonics: "Harmonics",
  upsampling: "Upsampling",
  output: "Output side",
};

/**
 * A preset picker. Without `part`: the global presets in the top bar, which
 * can hold any set of parts (ticked when saving). With `part`: the page's
 * own saved settings, in its header; loading one changes that part only.
 * The output part spans four pages (DC block, pitch, limiter, output): the
 * same picker shows on each, labelled "Output saved".
 */
export function PresetControl({ part }: { part?: PresetPart }) {
  const st = store();
  const all = useApp((s) => s.presets);
  const presets = all.filter((p) => (p.part ?? undefined) === part);
  const caps = useApp((s) => s.caps.presets);
  const statusPreset = useApp((s) => (part ? s.status?.part_presets?.find((x) => x.part === part) : s.status?.preset));
  const cfgPreset = useApp((s) => (part ? s.config?.active_part_presets?.[part] : s.config?.active_preset));
  const active = statusPreset?.name ?? cfgPreset ?? null;
  const dirty = !!statusPreset?.modified;
  const [open, setOpen] = useState(false);
  const [pane, setPane] = useState<Pane>({ kind: "main" });
  const [saveName, setSaveName] = useState("");
  const [renameTo, setRenameTo] = useState("");
  const [ticked, setTicked] = useState<PresetPart[]>(PRESET_PARTS);
  // A page's menu opens toward the side with room (the header wraps).
  const [alignRight, setAlignRight] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const btn = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        btn.current?.focus();
      }
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    menu.current?.querySelector<HTMLElement>("button:not(:disabled), input")?.focus();
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, pane]);

  const close = () => {
    setOpen(false);
    setPane({ kind: "main" });
  };
  // A page's picker is hidden on an engine without presets (the top bar says so).
  if (part && caps === false) return null;
  const what = part ? `Saved ${PART_NAME[part]} settings` : "Presets";
  const shared = part === "output";
  const label = caps === false ? "Presets unavailable" : active ? (shared ? `Output: ${active}` : active) : part ? (shared ? "Output saved" : "Saved") : "No preset";
  const allTicked = ticked.length === PRESET_PARTS.length;

  return (
    <div className={"preset" + (part ? " page" : "")} ref={root}>
      <button
        ref={btn}
        className={"btn preset-btn" + (part ? " small" : "")}
        aria-haspopup="menu"
        aria-expanded={open}
        title={part ? `${what}: ${PART_HOLDS[part]}. Loading one changes ${shared ? "these four pages" : "this page"} only.` : undefined}
        aria-label={`${part ? what : "Preset"}: ${label}${dirty ? ", unsaved changes" : ""}`}
        onClick={() => {
          setPane({ kind: "main" });
          const r = btn.current?.getBoundingClientRect();
          setAlignRight(!!r && r.left + 280 > window.innerWidth);
          setOpen(!open);
        }}
      >
        <span className="name">{label}</span>
        {dirty && <span className="dirty" title="Unsaved changes" />}
        <ChevronDown />
      </button>
      {(!part || (dirty && active)) && (
        <button className={"btn primary" + (part ? " small" : "")} disabled={!dirty || !active} onClick={() => void st.savePreset(part)} title={active ? `Save changes to “${active}”` : "Save as a new preset from the menu"}>
          Save
        </button>
      )}
      {open && (
        <div className={"menu" + (part ? " page-presets" + (alignRight ? " right" : "") : "")} role="menu" ref={menu} aria-label={what}>
          {pane.kind === "main" && (
            <>
              {caps === false ? (
                <p className="note">This engine version does not store presets yet. Import and copy still work on the current EQ.</p>
              ) : (
                <>
                  <div className="label">{what}</div>
                  {shared && <p className="note">DC block, pitch, limiter and output, saved together.</p>}
                  {presets.length === 0 && <p className="note">{part ? "Nothing saved for this page yet." : "No presets yet."}</p>}
                  {presets.map((p) => (
                    <button
                      key={p.name}
                      className="item"
                      role="menuitemradio"
                      aria-checked={p.name === active}
                      onClick={() => {
                        close();
                        void st.loadPreset(p.name, part);
                      }}
                    >
                      <span className="check">{p.name === active && <Check />}</span>
                      {p.name}
                      <span className="sub">{presetSub(p, part)}</span>
                    </button>
                  ))}
                  {part && (
                    <button
                      className="item"
                      role="menuitem"
                      title={`${shared ? "These four pages" : "This page"} back to the defaults, with no preset active. Your saved ones stay.`}
                      onClick={() => {
                        close();
                        void st.blankPreset(part);
                      }}
                    >
                      <span className="check" />
                      New (blank)
                    </button>
                  )}
                  <hr />
                  <div className="label">Save current as</div>
                  <form
                    className="inline-form"
                    onSubmit={(e) => {
                      e.preventDefault();
                      const n = saveName.trim();
                      if (!n) return;
                      if (!part && ticked.length === 0) return;
                      void st.savePresetAs(n, part, part ? undefined : ticked).then((ok) => {
                        if (ok) {
                          setSaveName("");
                          close();
                        }
                      });
                    }}
                  >
                    <input className="text-input" value={saveName} maxLength={64} placeholder={part ? "Name" : "Preset name"} aria-label="New preset name" onChange={(e) => setSaveName(e.target.value)} />
                    <button className="btn primary small" type="submit" disabled={!saveName.trim() || (!part && ticked.length === 0)}>
                      Save
                    </button>
                  </form>
                  {!part && (
                    <fieldset className="preset-parts">
                      <legend>
                        Holds{" "}
                        <button type="button" className="link-btn" onClick={() => setTicked(allTicked ? [] : PRESET_PARTS)}>
                          {allTicked ? "none" : "all"}
                        </button>
                      </legend>
                      {PRESET_PARTS.map((x) => (
                        <label key={x} title={`Loading the preset sets ${PART_HOLDS[x]}`}>
                          <input type="checkbox" checked={ticked.includes(x)} onChange={(e) => setTicked(e.target.checked ? PRESET_PARTS.filter((y) => y === x || ticked.includes(y)) : ticked.filter((y) => y !== x))} />
                          {SHORT_PART[x]}
                        </label>
                      ))}
                    </fieldset>
                  )}
                  <hr />
                  <button className="item" role="menuitem" disabled={!active} onClick={() => { setRenameTo(active ?? ""); setPane({ kind: "rename" }); }}>
                    Rename…
                  </button>
                  <button className="item" role="menuitem" disabled={!active || !dirty} onClick={() => { close(); void st.revertPreset(part); }}>
                    Revert unsaved changes
                  </button>
                  <button className="item danger" role="menuitem" disabled={!active} onClick={() => setPane({ kind: "delete" })}>
                    Delete…
                  </button>
                  {(!part || part === "eq") && <hr />}
                </>
              )}
              {(!part || part === "eq") && (
                <>
                  <ImportApoButton
                    className="item"
                    label="Import ParametricEQ.txt…"
                    onText={(t, n) => {
                      close();
                      void st.importApoPreset(t, n);
                    }}
                  />
                  <button className="item" role="menuitem" onClick={() => { close(); void st.copyApo(); }}>
                    Copy as ParametricEQ.txt
                  </button>
                </>
              )}
            </>
          )}
          {pane.kind === "rename" && active && (
            <form
              className="confirm"
              onSubmit={(e) => {
                e.preventDefault();
                void st.renamePreset(active, renameTo, part).then((ok) => ok && close());
              }}
            >
              <div className="label" style={{ padding: 0 }}>Rename “{active}”</div>
              <input className="text-input" value={renameTo} maxLength={64} aria-label="New name" onChange={(e) => setRenameTo(e.target.value)} />
              <div className="row">
                <button className="btn primary small" type="submit" disabled={!renameTo.trim() || renameTo.trim() === active}>
                  Rename
                </button>
                <button className="btn small" type="button" onClick={() => setPane({ kind: "main" })}>
                  Cancel
                </button>
              </div>
            </form>
          )}
          {pane.kind === "delete" && active && (
            <div className="confirm">
              <span>Delete “{active}”? The current sound stays as it is.</span>
              <div className="row">
                <button className="btn small danger" onClick={() => { close(); void st.deletePreset(active, part); }}>
                  Delete
                </button>
                <button className="btn small" onClick={() => setPane({ kind: "main" })}>
                  Cancel
                </button>
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
