import type { Band } from "../api/types";
import { isGainless } from "../lib/eqmath";
import { clamp, minus } from "../lib/format";
import { NumField } from "../components/NumField";
import { store } from "../store/store";
import { Close, FilterIcon, FILTER_TYPES, Power, Trash } from "../components/Icons";
import { bandColor } from "./eqColors";

const fmtF = (v: number) => (v < 100 ? String(Math.round(v * 10) / 10) : String(Math.round(v)));
const fmtG = (v: number) => minus(v.toFixed(1));
const fmtQ = (v: number) => v.toFixed(2);

/** Outside the editor's text fields, a press after a double-click (or a
 *  native drag) would select or drag text across the page. */
export function guardEditorChrome(e: { target: unknown; detail?: number; type: string; preventDefault(): void }) {
  const t = e.target as { tagName?: string } | null;
  if (t?.tagName === "INPUT" || t?.tagName === "TEXTAREA") return;
  if (e.type === "dragstart" || (e.detail ?? 0) > 1) e.preventDefault();
}

export const EDITOR_W = 452;
export const EDITOR_H = 96;

export function BandEditor({ band, index, channelNote, style }: { band: Band; index: number; channelNote: string; style?: React.CSSProperties }) {
  const st = store();
  const gl = isGainless(band.kind);
  const set = (patch: Partial<Band>) => st.updateBand(index, patch);
  return (
    <div className="editor" role="group" aria-label={`Band ${index + 1} editor`} style={style} onPointerDown={(e) => e.stopPropagation()} onMouseDown={guardEditorChrome} onDragStart={guardEditorChrome}>
      <div className="ed-top">
        <span className="ed-num">
          <i style={{ background: bandColor(index) }} />
          <span>
            Band {index + 1}
            {channelNote}
          </span>
        </span>
        <div className="seg types" role="group" aria-label="Filter type">
          {FILTER_TYPES.map((t) => (
            <button
              key={t.kind}
              title={t.label}
              aria-label={t.label}
              aria-pressed={band.kind === t.kind}
              onClick={() => {
                const patch: Partial<Band> = { kind: t.kind };
                if ((t.kind === "low_pass" || t.kind === "high_pass") && !(band.kind === "low_pass" || band.kind === "high_pass")) patch.q = 0.71;
                set(patch);
              }}
            >
              <FilterIcon kind={t.kind} />
            </button>
          ))}
        </div>
        <div className="tools">
          <button className="pwr" aria-pressed={band.enabled} aria-label={`Band ${index + 1} on`} title={band.enabled ? "Turn band off" : "Turn band on"} onClick={() => set({ enabled: !band.enabled })}>
            <Power />
          </button>
          <button className="icon-btn" aria-label="Delete band" title="Delete band (Delete)" onClick={() => st.deleteBand(index)}>
            <Trash />
          </button>
          <button className="icon-btn" aria-label="Close editor" title="Close" onClick={() => st.selectBand(-1)}>
            <Close />
          </button>
        </div>
      </div>
      <div className="ed-fields">
        <NumField label="Freq" unit="Hz" value={band.freq_hz} format={fmtF} onCommit={(v) => set({ freq_hz: clamp(v, 10, 22000) })} />
        <NumField label="Gain" unit="dB" value={band.gain_db} format={fmtG} disabled={gl} onCommit={(v) => set({ gain_db: Math.round(clamp(v, -30, 30) * 10) / 10 })} />
        <NumField label="Q" value={band.q} format={fmtQ} onCommit={(v) => set({ q: Math.round(clamp(v, 0.1, 30) * 100) / 100 })} />
      </div>
    </div>
  );
}
