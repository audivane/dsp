import { useEffect, useRef, useState } from "react";
import type { BypassResampler, DeviceLostAction, RateConflict } from "../api/types";
import { formatBits, rateSupportNote, usedBits } from "../lib/deviceCaps";
import { rateChips, type RateChip } from "../lib/rates";
import { fmtRate, fmtRateShort, fmtSampleFormat } from "../lib/format";
import { store, useApp } from "../store/store";
import { DevicePicker } from "../components/TopBar";
import { Seg } from "../components/Seg";
import { Refresh } from "../components/Icons";
import { VolumeSettings } from "./VolumeSettings";
import { TraySettings } from "./TraySettings";
import { FollowersPanel } from "./FollowersPanel";
import { nameOf } from "../lib/devices";
import { OutputPairPicker } from "./OutputPairPicker";

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
  const chips = rateChips({
    policy: cfg.rate,
    playable: status?.playable_rates,
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
          aria-label={`${fmtRate(c.rate)}${c.isDefault ? ", default" : ""}${c.playing ? ", playing" : ""}${c.unsupported ? (c.enabled ? ", kept, converted by PipeWire on this device" : ", not played by this device") : ""}`}
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
  const devicesKnown = useApp((s) => s.caps.devices);
  const conflict = useApp((s) => s.deviceConflict);
  // Cards come and go (a DAC plugged in or out): read the list when the page opens.
  useEffect(() => {
    void store().refreshDevices();
  }, []);
  if (!cfg) return null;
  const dev = devices.length ? st.currentDevice() : undefined;
  const released = status?.state === "idle";
  const hw = status?.volume ? (status.volume.hardware ? "Yes" : "No (digital)") : "-";
  const effectiveRates = status?.effective_rates ?? [];
  const rateNote = rateSupportNote(cfg.rate.allowed_rates, effectiveRates);
  // An engine without the setting sends no field (older than this option).
  const bypassKnown = cfg.rate.bypass_resampler !== undefined;
  const bypass = status?.bypass_resampler;
  const defaultRate = status?.effective_default_rate ?? cfg.rate.default_rate;
  const bits = usedBits(status?.device_caps, status?.output_rate, status?.format);
  const probed = (status?.device_caps?.rates ?? []).filter((c) => c.formats.length > 0).map((c) => c.rate);
  const dacPlays = probed.length ? probed.map((r) => fmtRateShort(r)).join(", ") : "-";
  const bitsTitle =
    bits !== null && status?.format
      ? `The DAC takes ${fmtSampleFormat(status.format)} samples and converts ${bits} bits of each` +
        (bits < formatBits(status.format) ? `; it ignores the lowest ${formatBits(status.format) - bits}.` : ".")
      : undefined;
  return (
    <section className="view" aria-labelledby="dev-title">
      <div className="view-head">
        <h1 id="dev-title">Device</h1>
      </div>
      <div className="panel">
        <div className="row">
          <span className="k">Output device</span>
          <DevicePicker className="chip" id="device-page" />
          <button
            className="icon-btn"
            aria-label="Refresh the device list"
            title="Refresh the device list"
            disabled={devicesKnown !== true}
            onClick={() => void st.refreshDevices()}
          >
            <Refresh />
          </button>
        </div>
        {status?.device_caps && status.device_caps.device === cfg.device && (
          <OutputPairPicker
            id="device-pair"
            label="Output channels of the main device"
            opened={status.device_caps.channels}
            channels={status.device_caps.channels_max}
            pair={cfg.output_channels}
            onChange={(p) => void st.setOutputChannels(p)}
          />
        )}
        {conflict && (
          <div className="device-conflict" role="alert">
            <span>
              {nameOf(devices, conflict)} already plays under "Also play on". A device plays one role at a time: remove it there to make it the main device.
            </span>
            <button className="btn small" onClick={() => void st.moveDeviceToFollower(conflict)}>
              Remove it and switch
            </button>
            <button className="btn small" onClick={() => st.clearDeviceConflict()}>
              Keep as is
            </button>
          </div>
        )}
        <div className="row">
          <span
            className="k"
            title="Audivane's outputs stay up while the DAC is unplugged, so apps never move to the speakers. This picks what the music does meanwhile. Playback goes back to the DAC, faded in, once it is plugged in again."
          >
            When the DAC is unplugged
          </span>
          <Seg<DeviceLostAction>
            label="What the music does while the DAC is unplugged"
            value={cfg.device_lost ?? "pause"}
            onChange={(action) => st.setDeviceLost(action)}
            disabled={cfg.device_lost === undefined}
            options={[
              ["pause", "Pause", "Pause the player (players that support media controls). It is not resumed when the DAC is back."],
              ["keep_playing", "Keep playing", "Players keep playing into silence, so their position moves on as usual."],
            ]}
          />
        </div>
        <div className="facts">
          <div className="fact">
            <div className="k">Card</div>
            <div className="v" title={cfg.device}>{dev?.card ?? cfg.device}</div>
          </div>
          <div className="fact">
            <div className="k">Format in use</div>
            <div className="v" title={bitsTitle}>
              {fmtSampleFormat(status?.format)}
              {bits !== null ? ` · ${bits}-bit` : ""}
            </div>
          </div>
          <div className="fact">
            <div className="k">Rate now</div>
            <div className="v">{status?.output_rate ? fmtRate(status.output_rate) : "-"}</div>
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
            <div className="v">{status?.version ?? "-"}</div>
          </div>
        </div>
        <div className="row">
          <span className="k" title="The rates this device itself plays, as the engine probed it when it opened the device. Other rates are converted to one of these.">
            Hardware rates
          </span>
          <span className="hw-rates">{dacPlays}</span>
        </div>
        <div className="row">
          <span className="k" title="Rates the Audivane engine accepts from apps (before upsampling), not what the device plays; click one to allow or block it. An app at a blocked rate is resampled by PipeWire. 352.8 kHz and up are off by default.">
            Engine accepts
          </span>
          <RateChips />
          <span className="hint rate-legend">
            <span className="def-dot" aria-hidden="true" /> default · right-click a rate to make it the default
          </span>
          {rateNote && <span className="hint">{rateNote.text}</span>}
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
          <span
            className="k"
            title="Who converts the rate for Audivane Direct and DSP off when the DAC does not play the source's rate (for example 44.1 kHz on a DAC that only takes 48 kHz). A rate the DAC plays is always sent unchanged (bit-perfect), whichever you pick."
          >
            When the DAC lacks the rate
          </span>
          <Seg<BypassResampler>
            label="Who converts the rate for Direct and DSP off when the DAC lacks it"
            value={cfg.rate.bypass_resampler ?? "pipewire"}
            onChange={(mode) => st.setBypassResampler(mode)}
            disabled={!bypassKnown}
            options={[
              [
                "engine",
                "Engine",
                "Audivane keeps the source rate and converts it to the nearest rate the DAC plays, with its recommended filter (linear phase, sized to the output's resolution; it adds the filter's delay and a little CPU). Not bit-perfect, but the converter is ours. Falls back to PipeWire for a rate it cannot convert.",
              ],
              [
                "pipewire",
                "PipeWire",
                "PipeWire moves the playback to a rate the DAC plays and resamples the app with its own resampler. Not bit-perfect either.",
              ],
            ]}
          />
          {!bypassKnown && <span className="hint">This engine always lets PipeWire convert; update the engine for this option.</span>}
          {bypass?.active && status && (
            <span className="hint" role="status">
              The engine converts {fmtRate(status.graph_rate)} to {fmtRate(status.output_rate)} now (not bit-perfect).
            </span>
          )}
          {bypass && !bypass.active && bypass.reason && <span className="hint">{bypass.reason[0].toUpperCase() + bypass.reason.slice(1)}.</span>}
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
      <FollowersPanel />
      <VolumeSettings />
      <TraySettings />
    </section>
  );
}
