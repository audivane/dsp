import { useEffect, useRef, useState } from "react";
import { store, useApp } from "../store/store";
import { ImportApoButton } from "../views/EqView";
import { Check, ChevronDown } from "./Icons";

type Pane = { kind: "main" } | { kind: "rename" } | { kind: "delete" };

function bandsText(n: number) {
  return n === 1 ? "1 band" : `${n} bands`;
}

export function PresetControl() {
  const st = store();
  const presets = useApp((s) => s.presets);
  const caps = useApp((s) => s.caps.presets);
  const statusPreset = useApp((s) => s.status?.preset);
  const cfgPreset = useApp((s) => s.config?.active_preset);
  const active = statusPreset?.name ?? cfgPreset ?? null;
  const dirty = !!statusPreset?.modified;
  const [open, setOpen] = useState(false);
  const [pane, setPane] = useState<Pane>({ kind: "main" });
  const [saveName, setSaveName] = useState("");
  const [renameTo, setRenameTo] = useState("");
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
  const label = caps === false ? "Presets unavailable" : active ?? "No preset";

  return (
    <div className="preset" ref={root}>
      <button
        ref={btn}
        className="btn preset-btn"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={`Preset: ${label}${dirty ? ", unsaved changes" : ""}`}
        onClick={() => {
          setPane({ kind: "main" });
          setOpen(!open);
        }}
      >
        <span className="name">{label}</span>
        {dirty && <span className="dirty" title="Unsaved changes" />}
        <ChevronDown />
      </button>
      <button className="btn primary" disabled={!dirty || !active} onClick={() => void st.savePreset()} title={active ? `Save changes to “${active}”` : "Save as a new preset from the menu"}>
        Save
      </button>
      {open && (
        <div className="menu" role="menu" ref={menu} aria-label="Presets">
          {pane.kind === "main" && (
            <>
              {caps === false ? (
                <p className="note">This engine version does not store presets yet. Import and copy still work on the current EQ.</p>
              ) : (
                <>
                  <div className="label">Presets</div>
                  {presets.length === 0 && <p className="note">No presets yet.</p>}
                  {presets.map((p) => (
                    <button
                      key={p.name}
                      className="item"
                      role="menuitemradio"
                      aria-checked={p.name === active}
                      onClick={() => {
                        close();
                        void st.loadPreset(p.name);
                      }}
                    >
                      <span className="check">{p.name === active && <Check />}</span>
                      {p.name}
                      <span className="sub">
                        {bandsText(p.bands)}
                        {p.convolution > 0 ? ` · ${p.convolution} IR` : ""}
                      </span>
                    </button>
                  ))}
                  <hr />
                  <div className="label">Save current as</div>
                  <form
                    className="inline-form"
                    onSubmit={(e) => {
                      e.preventDefault();
                      const n = saveName.trim();
                      if (!n) return;
                      void st.savePresetAs(n).then((ok) => {
                        if (ok) {
                          setSaveName("");
                          close();
                        }
                      });
                    }}
                  >
                    <input className="text-input" value={saveName} maxLength={64} placeholder="Preset name" aria-label="New preset name" onChange={(e) => setSaveName(e.target.value)} />
                    <button className="btn primary small" type="submit" disabled={!saveName.trim()}>
                      Save
                    </button>
                  </form>
                  <hr />
                  <button className="item" role="menuitem" disabled={!active} onClick={() => { setRenameTo(active ?? ""); setPane({ kind: "rename" }); }}>
                    Rename…
                  </button>
                  <button className="item" role="menuitem" disabled={!active || !dirty} onClick={() => { close(); void st.revertPreset(); }}>
                    Revert unsaved changes
                  </button>
                  <button className="item danger" role="menuitem" disabled={!active} onClick={() => setPane({ kind: "delete" })}>
                    Delete…
                  </button>
                  <hr />
                </>
              )}
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
          {pane.kind === "rename" && active && (
            <form
              className="confirm"
              onSubmit={(e) => {
                e.preventDefault();
                void st.renamePreset(active, renameTo).then((ok) => ok && close());
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
                <button className="btn small danger" onClick={() => { close(); void st.deletePreset(active); }}>
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
