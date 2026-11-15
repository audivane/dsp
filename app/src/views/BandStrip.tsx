import type { Band } from "../api/types";
import { isGainless } from "../lib/eqmath";
import { fmtDb, fmtFreq, fmtQ } from "../lib/format";
import { store } from "../store/store";
import { FilterIcon, filterLabel, Plus, Power } from "../components/Icons";
import { bandColor } from "./eqColors";

/** Compact chips under the graph: colour + number (as on the handle), type, values, power. */
export function BandStrip({ bands, selected, disabled }: { bands: Band[]; selected: number; disabled: boolean }) {
  const st = store();
  return (
    <div className="bandstrip" role="list" aria-label="Bands">
      {bands.map((b, i) => {
        const col = bandColor(i);
        const gl = isGainless(b.kind);
        const desc = `${filterLabel(b.kind)}, ${fmtFreq(b.freq_hz)} Hz${gl ? "" : `, ${fmtDb(b.gain_db)} dB`}, Q ${fmtQ(b.q)}${b.enabled ? "" : ", off"}`;
        return (
          <div
            key={i}
            role="listitem"
            className={"bchip" + (i === selected ? " sel" : "") + (b.enabled ? "" : " off")}
            style={{ ["--band" as string]: col }}
          >
            <button
              className="sel-btn"
              aria-label={`Band ${i + 1}: ${desc}`}
              aria-pressed={i === selected}
              onClick={() => st.selectBand(i)}
              disabled={disabled}
            />
            <span className="dot" style={{ background: col, color: b.enabled ? "#0b1020" : col }} aria-hidden="true">
              {i + 1}
            </span>
            <span className="ty" title={filterLabel(b.kind)} aria-hidden="true">
              <FilterIcon kind={b.kind} />
            </span>
            <span className="vals" aria-hidden="true">
              <span className="f">{fmtFreq(b.freq_hz)} Hz</span>
              <span className="gq">
                {gl ? "" : fmtDb(b.gain_db) + " dB · "}Q {fmtQ(b.q)}
              </span>
            </span>
            <button
              className="pwr"
              aria-pressed={b.enabled}
              aria-label={`Band ${i + 1} on`}
              title={b.enabled ? "Turn band off" : "Turn band on"}
              onClick={() => st.updateBand(i, { enabled: !b.enabled })}
              disabled={disabled}
            >
              <Power />
            </button>
          </div>
        );
      })}
      <button className="bchip add" onClick={() => st.addBand()} disabled={disabled}>
        <Plus />
        Add band
      </button>
    </div>
  );
}
