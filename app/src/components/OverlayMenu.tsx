// The EQ graph's Overlays menu: add a built-in target, import a file, list
// what is on the graph (visibility, colour, offset, delete), choose the
// reference frequency, and the credits for the built-in data.

import { useEffect, useRef, useState } from "react";
import { fmtDb, parseNumber } from "../lib/format";
import {
  BUILTIN_TARGETS,
  OVERLAY_COLOURS,
  OVERLAY_KIND_LABEL,
  flatTarget,
  parseRefHz,
  tiltTarget,
  type Overlay,
  type OverlayKind,
} from "../lib/overlays";
import { store, useApp } from "../store/store";
import { Download, Eye, EyeOff, Layers, Trash } from "./Icons";

const KIND_SHORT: Record<OverlayKind, string> = { target: "target", bounds: "band", measurement: "meas." };

const IMPORT_HELP =
  "Files: REW, squig.link or AutoEq exports, or plain “freq dB” columns, separated by commas, tabs, semicolons or spaces; a header line is optional. Bounds take freq, lower, upper columns, or pick two files (lower, then upper).";

/** Built-ins by group for the picker (names come from the data files). */
function groupOf(name: string): string {
  if (/tilt/i.test(name)) return "Speakers";
  return "Headphones";
}

const NEXT_COLOUR = (colour: string) => {
  const i = OVERLAY_COLOURS.findIndex((c) => c.toLowerCase() === colour.toLowerCase());
  return OVERLAY_COLOURS[(i + 1) % OVERLAY_COLOURS.length];
};

function OverlayRow({ o }: { o: Overlay }) {
  const st = store();
  const [offset, setOffset] = useState<string | null>(null);
  const commitOffset = () => {
    const t = offset;
    setOffset(null);
    if (t === null) return;
    const v = parseNumber(t);
    if (v === null) return;
    const db = Math.round(Math.max(-60, Math.min(60, v)) * 10) / 10;
    if (db !== o.offsetDb) st.updateOverlay(o.id, { offsetDb: db });
  };
  return (
    <div className={"ov-row" + (o.visible ? "" : " off")}>
      <button
        className="icon-btn ov-eye"
        aria-pressed={o.visible}
        aria-label={`${o.visible ? "Hide" : "Show"} ${o.name}`}
        title={o.visible ? "Hide on the graph" : "Show on the graph"}
        onClick={() => st.updateOverlay(o.id, { visible: !o.visible })}
      >
        {o.visible ? <Eye /> : <EyeOff />}
      </button>
      <button
        className="ov-colour"
        style={{ background: o.colour }}
        aria-label={`Colour of ${o.name}`}
        title="Change colour"
        onClick={() => st.updateOverlay(o.id, { colour: NEXT_COLOUR(o.colour) })}
      />
      <span className="ov-name" title={`${OVERLAY_KIND_LABEL[o.kind]}: ${o.name}${o.data.source ? "\n" + o.data.source : ""}`}>
        {o.name}
      </span>
      <span className="ov-kind" aria-hidden="true">
        {KIND_SHORT[o.kind]}
      </span>
      {o.kind === "measurement" && (
        <button
          className="tag"
          aria-pressed={o.predicted}
          title="Draw measurement + current EQ as well"
          onClick={() => st.updateOverlay(o.id, { predicted: !o.predicted })}
        >
          +EQ
        </button>
      )}
      <label className="ov-offset" title="Offset in dB: shifts the whole curve up or down (it stays normalised at the reference frequency)">
        <input
          value={offset ?? fmtDb(o.offsetDb, 1, false)}
          onChange={(e) => setOffset(e.target.value)}
          onBlur={commitOffset}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
            else if (e.key === "Escape") setOffset(null);
          }}
          aria-label={`${o.name} offset in dB`}
        />
        <span>dB</span>
      </label>
      <button className="icon-btn" aria-label={`Delete ${o.name}`} title="Remove this overlay (deletes its file)" onClick={() => void st.removeOverlay(o.id)}>
        <Trash />
      </button>
    </div>
  );
}

export function OverlayMenu() {
  const st = store();
  const overlays = useApp((s) => s.overlays);
  const refHz = useApp((s) => s.overlayRef);
  const [open, setOpen] = useState(false);
  const [sources, setSources] = useState(false);
  const [alignLeft, setAlignLeft] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const [builtin, setBuiltin] = useState(BUILTIN_TARGETS[0]?.name ?? "__flat");
  const [tilt, setTilt] = useState("-0.7");
  const [kind, setKind] = useState<OverlayKind>("measurement");
  const [refText, setRefText] = useState<string | null>(null);
  const panel = useRef<HTMLDivElement>(null);
  const file = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!open) return;
    const off = (e: MouseEvent) => !panel.current?.parentElement?.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", off);
    document.addEventListener("keydown", esc);
    panel.current?.focus();
    return () => {
      document.removeEventListener("mousedown", off);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);

  const addTarget = () => {
    if (builtin === "__flat") {
      st.addOverlay(flatTarget());
      return;
    }
    if (builtin === "__tilt") {
      const v = parseNumber(tilt);
      if (v === null || Math.abs(v) > 12) {
        st.toast("Type a tilt in dB per octave, between −12 and +12.", "error");
        return;
      }
      st.addOverlay(tiltTarget(Math.round(v * 10) / 10));
      return;
    }
    const t = BUILTIN_TARGETS.find((x) => x.name === builtin);
    if (t) st.addOverlay(t);
  };

  const onFiles = async (files: FileList | null, input: HTMLInputElement) => {
    const list = files ? [...files] : [];
    input.value = "";
    if (!list.length) return;
    if (kind === "bounds" && list.length >= 2) {
      const [lower, upper] = await Promise.all([list[0].text(), list[1].text()]);
      st.importBounds({ text: lower, name: list[0].name }, { text: upper, name: list[1].name });
      return;
    }
    st.importOverlay(await list[0].text(), list[0].name, kind);
  };

  const commitRef = () => {
    const t = refText;
    setRefText(null);
    if (t === null) return;
    const hz = parseRefHz(t);
    if (hz !== null) st.setOverlayRef(hz);
  };

  const groups = new Map<string, string[]>();
  for (const t of BUILTIN_TARGETS) {
    const g = groupOf(t.name);
    groups.set(g, [...(groups.get(g) ?? []), t.name]);
  }

  const count = overlays.filter((o) => o.visible).length;
  return (
    <div className="overlay-menu" ref={root}>
      <button
        className="btn"
        aria-haspopup="dialog"
        aria-expanded={open}
        title="Target curves, preference bounds and measurements on the graph"
        onClick={() => {
          // Open towards the side with room (the button can wrap to the left edge).
          const r = root.current?.getBoundingClientRect();
          setAlignLeft(!!r && r.right < Math.min(400, window.innerWidth - 32) + 16);
          setOpen(!open);
        }}
      >
        <Layers />
        Overlays
        {count > 0 && <span className="ov-count">{count}</span>}
      </button>
      {open && (
        <div className={"menu overlays" + (alignLeft ? " align-left" : "")} role="dialog" aria-label="Overlays" tabIndex={-1} ref={panel}>
          <div className="ov-line">
            <span className="ov-label">Target</span>
            <select aria-label="Built-in target" value={builtin} onChange={(e) => setBuiltin(e.target.value)}>
              {[...groups].map(([g, names]) => (
                <optgroup key={g} label={g}>
                  {names.map((n) => (
                    <option key={n} value={n}>
                      {n}
                    </option>
                  ))}
                </optgroup>
              ))}
              <optgroup label="Generated">
                <option value="__tilt">Tilt (dB per octave)</option>
                <option value="__flat">Flat</option>
              </optgroup>
            </select>
            {builtin === "__tilt" && (
              <label className="ov-tilt" title="A straight line of this many dB per octave through 0 dB at 1 kHz">
                <input value={tilt} onChange={(e) => setTilt(e.target.value)} aria-label="Tilt in dB per octave" />
                <span>dB/oct</span>
              </label>
            )}
            <button className="btn small" onClick={addTarget}>
              Add
            </button>
          </div>

          <div className="ov-line">
            <span className="ov-label">Import</span>
            <select aria-label="Import as" value={kind} onChange={(e) => setKind(e.target.value as OverlayKind)} title={IMPORT_HELP}>
              <option value="measurement">Measurement</option>
              <option value="target">Target</option>
              <option value="bounds">Preference bounds</option>
            </select>
            <button className="btn small" onClick={() => file.current?.click()} title={IMPORT_HELP}>
              <Download />
              File…
            </button>
          </div>
          <input
            ref={file}
            type="file"
            accept=".txt,.csv,.tsv,text/plain,text/csv,text/tab-separated-values"
            multiple={kind === "bounds"}
            hidden
            onChange={(e) => void onFiles(e.target.files, e.target)}
          />

          <div className="ov-list" aria-label="Curves on the graph">
            {overlays.length === 0 ? <div className="ov-empty">No curves yet.</div> : overlays.map((o) => <OverlayRow key={o.id} o={o} />)}
          </div>

          <div className="ov-foot">
            <label className="ov-ref" title="Every overlay is shifted to sit on 0 dB at this frequency">
              <span>0 dB at</span>
              <input
                value={refText ?? String(refHz)}
                onChange={(e) => setRefText(e.target.value)}
                onBlur={commitRef}
                onKeyDown={(e) => {
                  if (e.key === "Enter") (e.target as HTMLInputElement).blur();
                  else if (e.key === "Escape") setRefText(null);
                }}
                aria-label="Reference frequency in Hz"
              />
              <span>Hz</span>
            </label>
            <button type="button" className="link-btn" aria-expanded={sources} onClick={() => setSources(!sources)}>
              Sources
            </button>
          </div>

          {sources && (
            <div className="ov-sources" role="note">
              <p>
                <b>Headphone targets</b> from{" "}
                <a href="https://github.com/jaakkopasanen/AutoEq" target="_blank" rel="noreferrer">
                  AutoEq
                </a>{" "}
                by Jaakko Pasanen (MIT, © 2018–2022): Harman over-ear 2018, Harman in-ear 2019 (v2), diffuse field ISO 11904-1, diffuse field
                B&amp;K 5128, diffuse field GRAS KEMAR and oratory1990 Optimum HiFi, taken from AutoEq revision 7ae0f56 and resampled to 1/24 octave;
                nothing else is changed.
              </p>
              <p>
                <b>Speaker tilt exploration</b> (a band from −1.0 to −0.5 dB/octave through 1 kHz), <b>Tilt</b> and <b>Flat</b> are generated for
                Audivane (MIT). The slopes are engineering choices, not published preference limits.
              </p>
              <p>
                No preference bounds are built in: no published bounds come with a licence that allows shipping them. Import your own.
              </p>
              <p className="mono">app/src/assets/targets/SOURCES.md · app/src/assets/research-targets/SOURCES.md</p>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
