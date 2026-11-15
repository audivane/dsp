import { useEffect, useRef, useState } from "react";
import { BALANCE_MAX_DB, VOLUME_MAX_DB, VOLUME_MIN_DB } from "../api/types";
import { fmtDb } from "../lib/format";
import { balanceTaper, volumeTaper } from "../lib/taper";
import { store, useApp } from "../store/store";
import { SKINS, setSkin, useSkin } from "../theme";
import { FineSlider } from "./FineSlider";
import { Arrow, Check, Device, Logo, Speaker, ThemeIcon } from "./Icons";
import { PresetControl } from "./PresetMenu";

function deviceLabel(name: string, id: string) {
  return `${name} · ${id.replace(/^hw:CARD=([^,]+),DEV=(\d+)$/, "hw:$1,$2")}`;
}

export function DevicePicker({ className = "chip", id = "device" }: { className?: string; id?: string }) {
  const st = store();
  const device = useApp((s) => s.config?.device ?? "");
  const devices = useApp((s) => s.devices);
  const caps = useApp((s) => s.caps.devices);
  const list = devices.length ? devices : [{ id: device, name: device || "No device", card: "", hdmi: false, loopback: false, active: true }];
  const value = (devices.length && st.currentDevice()?.id) || device;
  const known = list.some((d) => d.id === value);
  return (
    <label className={className} htmlFor={id} title={caps === false ? "This engine version cannot list devices yet" : "Output device"}>
      <Device />
      <select id={id} aria-label="Output device" value={value} disabled={caps !== true} onChange={(e) => void st.setDevice(e.target.value)}>
        {!known && device && <option value={device}>{device}</option>}
        {list.map((d) => (
          <option key={d.id} value={d.id}>
            {devices.length ? deviceLabel(d.name, d.id) : d.name}
            {d.hdmi ? " (HDMI)" : d.loopback ? " (loopback)" : ""}
          </option>
        ))}
      </select>
    </label>
  );
}

function VolumeControls() {
  const st = store();
  useApp((s) => s.volumeDraft);
  const vs = useApp((s) => s.status?.volume ?? null);
  const sink = useApp((s) => s.status?.active_sink ?? null);
  const dspOn = useApp((s) => s.config?.dsp_enabled ?? true);
  const v = st.volume();
  // Digital volume lives on the DSP path only: on the Direct sink (or with
  // the DSP bypassed) the signal is bit-perfect and never scaled.
  const bitPerfect = !!vs && !vs.hardware && (sink === "direct" || !dspOn);
  const BIT_PERFECT_TIP = "Bit-perfect: set the volume on the DAC. The Direct path (and Bypass) never applies digital volume.";
  const off = !v || !vs || bitPerfect;
  // Balance is always digital gain on the DSP path, even when the DAC's
  // hardware mixer does the volume, so it does nothing on Direct or Bypass.
  const dspBypassed = sink === "direct" || !dspOn;
  const balOff = off || dspBypassed;
  const BAL_BYPASS_TIP = "Balance is applied on the DSP path only; Direct playback and Bypass stay bit-perfect.";
  const title = !v || !vs ? "Volume needs a newer engine" : bitPerfect ? BIT_PERFECT_TIP : undefined;
  const bal = v?.balance_db ?? 0;
  const balText = bal === 0 ? "C" : (bal < 0 ? "L " : "R ") + Math.abs(bal).toFixed(1);
  // -40..0 dB gets 80% of the travel; the rest reaches the engine's minimum.
  const lo = Math.max(vs?.min_db ?? -80, VOLUME_MIN_DB);
  const hi = Math.min(vs?.max_db ?? VOLUME_MAX_DB, VOLUME_MAX_DB);
  const volTaper = volumeTaper(lo, hi);
  const balTaper = balanceTaper(BALANCE_MAX_DB);
  const volText = bitPerfect ? "on DAC" : v ? `${fmtDb(v.volume_db, 1, false)} dB` : "—";
  const hint = "Arrow keys: 0.5 dB, Shift+arrows: 0.1 dB. Double-click the value to type it.";
  return (
    <>
      <div className={"ctl" + (bitPerfect ? " bit-perfect" : "")} title={off ? title : hint}>
        <label htmlFor="vol">Vol</label>
        <FineSlider
          id="vol"
          label="Volume"
          value={v?.volume_db ?? hi}
          taper={volTaper}
          disabled={off}
          text={volText}
          title={off ? title : hint}
          onChange={(db, mode) => st.setVolume({ volume_db: db }, mode)}
        />
        {vs &&
          (vs.hardware ? (
            <span className="hw" title="Hardware volume: the DAC's own volume control is used, so the output stays bit-perfect">
              HW
            </span>
          ) : bitPerfect ? null : (
            <span
              className="sw"
              title="Digital volume: the DAC has no hardware volume control, so the engine scales the signal in 64-bit float before dither. The Direct path is never scaled."
            >
              DIG
            </span>
          ))}
      </div>
      <div className="ctl" title={off ? title : dspBypassed ? BAL_BYPASS_TIP : `${hint} Double-click the slider to centre.`}>
        <label htmlFor="bal">Bal</label>
        <FineSlider
          id="bal"
          label="Balance"
          value={bal}
          taper={balTaper}
          disabled={balOff}
          fillFrom="centre"
          text={balText}
          title={off ? title : dspBypassed ? BAL_BYPASS_TIP : "±3 dB takes 70% of the travel. Double-click to centre."}
          onChange={(db, mode) => st.setVolume({ balance_db: db }, mode)}
          onReset={() => st.setVolume({ balance_db: 0 }, "commit")}
        />
      </div>
      <button
        className="icon-btn mute"
        aria-pressed={!!v?.muted}
        aria-label="Mute"
        title={!v || !vs ? title : v?.muted ? "Unmute" : "Mute"}
        disabled={!v || !vs}
        onClick={() => st.setVolume({ muted: !v?.muted })}
      >
        <Speaker muted={!!v?.muted} />
      </button>
    </>
  );
}

function ThemeMenu() {
  const skin = useSkin();
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const off = (e: MouseEvent) => !root.current?.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", off);
    document.addEventListener("keydown", esc);
    root.current?.querySelector<HTMLElement>('[aria-checked="true"]')?.focus();
    return () => {
      document.removeEventListener("mousedown", off);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);
  const label = SKINS.find((x) => x.id === skin)?.label ?? "System";
  return (
    <div className="theme-menu" ref={root}>
      <button className="icon-btn" aria-haspopup="menu" aria-expanded={open} aria-label={`Theme: ${label}`} title={`Theme: ${label}`} onClick={() => setOpen(!open)}>
        <ThemeIcon />
      </button>
      {open && (
        <div className="menu right" role="menu" aria-label="Theme">
          <div className="label">Theme</div>
          {SKINS.map((s) => (
            <button
              key={s.id}
              className="item"
              role="menuitemradio"
              aria-checked={skin === s.id}
              onClick={() => {
                setSkin(s.id);
                setOpen(false);
              }}
            >
              <span className="check">{skin === s.id && <Check />}</span>
              <span className={`swatch sw-${s.id}`} />
              {s.label}
              {s.id === "system" && <span className="sub">follows the desktop</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

export function TopBar() {
  const st = store();
  const ab = useApp((s) => s.status?.ab ?? null);
  const abCfg = useApp((s) => !!s.config?.ab);
  const bypass = useApp((s) => (s.config ? !s.config.dsp_enabled : false));
  return (
    <header className="top">
      <div className="brand">
        <Logo />
        <span>
          Audivane <b>DSP</b>
        </span>
      </div>
      <DevicePicker />
      <PresetControl />
      <div className="spacer" />
      <div className="top-group">
        <div className="seg ab" role="group" aria-label="A/B compare">
          {(["a", "b"] as const).map((side) => (
            <button
              key={side}
              aria-pressed={abCfg && (ab?.current ?? "a") === side}
              title={abCfg ? `Listen to side ${side.toUpperCase()} (key ${side.toUpperCase()})` : "Set up side B in A/B compare"}
              onClick={() => (abCfg ? void st.abSelect(side) : st.setView("ab"))}
            >
              {side.toUpperCase()}
            </button>
          ))}
        </div>
        <button className="btn bypass" aria-pressed={bypass} onClick={() => st.setBypass(!bypass)} title="Send audio straight to the DAC, untouched">
          <Arrow />
          Bypass
        </button>
      </div>
      <div className="top-group">
        <VolumeControls />
      </div>
      <ThemeMenu />
    </header>
  );
}
