import { useEffect, useRef, useState } from "react";
import type { RateConflict } from "../api/types";
import { directRateNote, rateSupportNote } from "../lib/deviceCaps";
import { rateChips, type RateChip } from "../lib/rates";
import { fmtRate, fmtRateShort, fmtSampleFormat } from "../lib/format";
import { store, useApp } from "../store/store";
import { DevicePicker } from "../components/TopBar";
import { Seg } from "../components/Seg";

/** "Use as default" for one chip; Escape, a click elsewhere or Tab closes it. */
function RateMenu({ chip, x, y, onClose }: { chip: RateChip; x: number; y: number; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.querySelector("button")?.focus();
    const away = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", away);
    return () => document.removeEventListener("mousedown", away);
  }, [onClose]);
  return (
    <div
      ref={ref}
      className="rate-menu"
      role="menu"
      aria-label={`${fmtRate(chip.rate)} options`}
      style={{ left: x, top: y }}
      onKeyDown={(e) => {
        if (e.key === "Escape" || e.key === "Tab") {
          e.preventDefault();
          onClose();
        }
      }}
    >
      <button
        role="menuitem"
        disabled={chip.isDefault}
        onClick={() => {
          store().setDefaultRate(chip.rate);
          onClose();
        }}
      >
        {chip.isDefault ? "Default rate" : "Use as default"}
      </button>
    </div>
  );
}

/** One toggle per engine rate: allowed or not, the default, the playing one. */
function RateChips() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const status = useApp((s) => s.status);
  const listRef = useRef<HTMLSpanElement>(null);
  const [menu, setMenu] = useState<{ rate: number; x: number; y: number } | null>(null);
  if (!cfg) return null;
  const up = cfg.chain.upsampler.target;
  const maxRatio = !cfg.dsp_enabled || up === "off" ? 1 : "ratio" in up ? up.ratio : 16;
  const chips = rateChips({
    policy: cfg.rate,
    effective: status?.effective_rates,
    caps: status?.device_caps,
    maxRatio,
    state: status?.state,
    graphRate: status?.graph_rate,
  });
  // Opens under the chip (mouse or keyboard alike), so it never hides the others in the row.
  const openMenu = (chip: RateChip, el: HTMLElement) => {
    if (!chip.enabled) return;
    const box = listRef.current?.getBoundingClientRect();
    const r = el.getBoundingClientRect();
    setMenu({ rate: chip.rate, x: r.left - (box?.left ?? 0), y: r.bottom + 4 - (box?.top ?? 0) });
  };
  const menuChip = menu ? chips.find((c) => c.rate === menu.rate) : undefined;
  return (
    <span className="rate-list" role="group" aria-label="Allowed sample rates" ref={listRef}>
      {chips.map((c) => (
        <button
          key={c.rate}
          type="button"
          className={"rate-chip" + (c.unsupported ? " unsupported" : "") + (c.playing ? " playing" : "")}
          aria-pressed={c.enabled}
          aria-haspopup={c.enabled ? "menu" : undefined}
          aria-label={`${fmtRate(c.rate)}${c.isDefault ? ", default" : ""}${c.playing ? ", playing" : ""}${c.unsupported ? ", not supported by this device" : ""}`}
          title={c.title}
          disabled={!c.canToggle}
          onClick={() => c.canToggle && st.toggleAllowedRate(c.rate)}
          onContextMenu={(e) => {
            e.preventDefault();
            openMenu(c, e.currentTarget);
          }}
          onKeyDown={(e) => {
            if (e.key === "ContextMenu" || (e.shiftKey && e.key === "F10")) {
              e.preventDefault();
              openMenu(c, e.currentTarget);
            }
          }}
        >
          {c.isDefault && <span className="def-dot" aria-hidden="true" />}
          {fmtRateShort(c.rate)}
        </button>
      ))}
      {menu && menuChip && <RateMenu chip={menuChip} x={menu.x} y={menu.y} onClose={() => setMenu(null)} />}
    </span>
  );
}

export function DeviceView() {
  const st = store();
  const cfg = useApp((s) => s.config);
  const status = useApp((s) => s.status);
  const devices = useApp((s) => s.devices);
  const test = useApp((s) => s.selfTest);
  if (!cfg) return null;
  const dev = devices.length ? st.currentDevice() : undefined;
  const released = status?.state === "idle";
  const hw = status?.volume ? (status.volume.hardware ? "Yes" : "No (digital)") : "—";
  const effectiveRates = status?.effective_rates ?? [];
  const rateNote = rateSupportNote(cfg.rate.allowed_rates, effectiveRates);
  const directNote = directRateNote(effectiveRates, status?.direct_rates);
  const defaultRate = status?.effective_default_rate ?? cfg.rate.default_rate;
  return (
    <section className="view" aria-labelledby="dev-title">
      <div className="view-head">
        <h1 id="dev-title">Device</h1>
      </div>
      <div className="panel">
        <div className="row">
          <span className="k">Output device</span>
          <DevicePicker className="chip" id="device-page" />
        </div>
        <div className="facts">
          <div className="fact">
            <div className="k">Card</div>
            <div className="v" title={cfg.device}>{dev?.card ?? cfg.device}</div>
          </div>
          <div className="fact">
            <div className="k">Format in use</div>
            <div className="v">{fmtSampleFormat(status?.format)}</div>
          </div>
          <div className="fact">
            <div className="k">Rate now</div>
            <div className="v">{status?.output_rate ? fmtRate(status.output_rate) : "—"}</div>
          </div>
          <div className="fact">
            <div className="k">Hardware volume</div>
            <div className="v">{hw}</div>
          </div>
          <div className="fact">
            <div className="k">Quantum</div>
            <div className="v">{cfg.quantum}</div>
          </div>
          <div className="fact">
            <div className="k">Engine</div>
            <div className="v">{status?.version ?? "—"}</div>
          </div>
        </div>
        <div className="row">
          <span className="k" title="Sample rates the graph may run at (before upsampling); click one to allow or block it. An app at a blocked rate is resampled by PipeWire. 352.8 kHz and up are off by default.">
            Sample rates
          </span>
          <RateChips />
          <span className="hint rate-legend">
            <span className="def-dot" aria-hidden="true" /> default · right-click a rate to make it the default
          </span>
          {rateNote && <span className="hint">{rateNote.text}</span>}
          {directNote && <span className="hint">{directNote}</span>}
        </div>
        <div className="row">
          <span className="k" title="When apps play into Audivane DSP at different sample rates, the DAC can run at only one of them; PipeWire resamples the others. Apps on the Direct (bit-perfect) path always set the rate themselves.">
            Mixed rates
          </span>
          <Seg<RateConflict>
            label="When apps play at different rates"
            value={cfg.rate.conflict}
            onChange={(conflict) => st.setRateConflict(conflict)}
            disabled={!cfg.rate.follow_source}
            options={[
              ["keep_current", "Keep current", "Keep the current rate while any app still plays at it; otherwise follow the app that started first."],
              ["highest", "Use highest", "Run at the highest rate any of the apps plays at."],
              ["default_rate", "Use default", `Run at the default rate (${fmtRate(defaultRate)}) while the apps disagree. A single app is still followed.`],
            ]}
          />
          {!cfg.rate.follow_source && <span className="hint">Rate following is off: always {fmtRate(defaultRate)}</span>}
        </div>
        <div className="row">
          {released ? (
            <button className="btn" onClick={() => void st.acquireDevice()}>
              Take the DAC back
            </button>
          ) : (
            <button className="btn" onClick={() => void st.releaseDevice()} title="Hand the DAC to other apps; the engine idles">
              Release DAC to the system
            </button>
          )}
          <button className="btn primary" disabled={test.state === "running" || released} onClick={() => void st.runSelfTest()}>
            Run bit-perfect self-test
          </button>
        </div>
        {test.state === "running" && <div className="result pending" role="status">Playing a test signal through the Direct path…</div>}
        {test.state === "done" && test.result && (
          <div className={"result" + (test.result.passed ? "" : " bad")} role="status">
            {test.result.passed ? "✓ Passed. " : "✗ Failed. "}
            {test.result.frames.toLocaleString("en-US")} frames at {fmtRate(test.result.rate)}
            {test.result.format ? ` (${fmtSampleFormat(test.result.format)})` : ""} {test.result.passed ? "reached the DAC unchanged." : "did not reach the DAC unchanged."}
            {!test.result.passed && test.result.detail ? ` ${test.result.detail}` : ""}
          </div>
        )}
        {test.state === "error" && (
          <div className="result bad" role="alert">
            The self-test could not run. {test.error}
          </div>
        )}
      </div>
    </section>
  );
}
