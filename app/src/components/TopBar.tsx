import { useEffect, useRef, useState } from "react";
import { BALANCE_MAX_DB, VOLUME_MAX_DB, VOLUME_MIN_DB, type VolumeStatus } from "../api/types";
import { fmtDb } from "../lib/format";
import { HeldHeadroomTag } from "./HeldHeadroom";
import { balanceTaper, volumeSliderTaper } from "../lib/taper";
import { volumeOnDac } from "../lib/volumeKeys";
import { abStoppedByOverload, topBarLabel } from "../lib/abCompare";
import { store, useApp, volumeWritable, type VolumeDraft, type VolumeGesture } from "../store/store";
import { SKINS, setSkin, useSkin } from "../theme";
import { FineSlider } from "./FineSlider";
import { WHEEL_HINT } from "./sliderWheel";
import { Arrow, Check, Device, Logo, Speaker, ThemeIcon } from "./Icons";
import { PresetControl } from "./PresetMenu";
import { deviceLabel } from "../lib/devices";

export { deviceLabel } from "../lib/devices";

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
            {devices.length ? deviceLabel(d) : d.name}
            {d.hdmi ? " (HDMI)" : d.loopback ? " (loopback)" : ""}
          </option>
        ))}
      </select>
    </label>
  );
}

/** What a volume control shows and how it may change: shared by the top
 *  bar and the tray popup, so both read and write the level the same way
 *  (`store.volumeView`, guarded gestures). */
export function useVolumeModel() {
  const st = store();
  useApp((s) => s.volumeDraft);
  const vs = useApp((s) => s.status?.volume ?? null);
  // Until the first status arrives the level is unknown: nothing is drawn
  // (never the top of the range, which reads as 0 dB).
  const haveStatus = useApp((s) => s.status !== null);
  // After a reconnect or a refused write the held status may be old: wait
  // for a current one (`store.volume()` is null until then).
  const fresh = useApp((s) => s.volumeFresh);
  // A daemon that predates volume revisions gets no writes from the app.
  const legacy = !!vs && !volumeWritable(vs);
  // A new connection remounts the sliders, ending any gesture in progress.
  const epoch = useApp((s) => s.connEpoch);
  const sink = useApp((s) => s.status?.active_sink ?? null);
  const dspOn = useApp((s) => s.config?.dsp_enabled ?? true);
  // What the controls show, and the level state a gesture begins from.
  const view = st.volumeView();
  const v = view?.level ?? null;
  const begin = () => st.beginVolumeGesture(view);
  // Digital volume lives on the DSP path only: on the Direct sink (or with
  // the DSP bypassed) the signal is bit-perfect and never scaled.
  const bitPerfect = volumeOnDac(vs, sink, dspOn);
  const off = !v || !vs || bitPerfect;
  // Balance is always digital gain on the DSP path, even when the DAC's
  // hardware mixer does the volume, so it does nothing on Direct or Bypass.
  const dspBypassed = sink === "direct" || !dspOn;
  const title = !haveStatus
    ? "Waiting for the engine"
    : legacy
      ? "Update the engine: this version does not report volume revisions, so the app cannot change the volume safely. Use avdsp ctl volume meanwhile."
      : vs && !fresh
        ? "Waiting for the engine"
        : !v || !vs ? "Volume needs a newer engine" : bitPerfect ? BIT_PERFECT_TIP : undefined;
  // -40..ceiling gets 80% of the travel; the rest reaches the engine's minimum.
  // A level below the reported range (an engine that reports its card's
  // floor) still shows where it is.
  const volTaper = volumeSliderTaper(vs?.min_db, vs?.max_db, v && vs ? v.volume_db : null, VOLUME_MIN_DB, VOLUME_MAX_DB);
  const volText = bitPerfect ? "on DAC" : v && vs ? `${fmtDb(v.volume_db, 1, false)} dB` : "-";
  return { st, vs, v, epoch, begin, bitPerfect, off, dspBypassed, title, volTaper, volText };
}

export const BIT_PERFECT_TIP = "Bit-perfect: set the volume on the DAC. The Direct path (and Bypass) never applies digital volume.";

/** HW / DIG next to the volume: where the level is applied, and whether
 *  that keeps the output bit-perfect. */
export function VolumeModeTag({ vs, v, bitPerfect }: { vs: VolumeStatus | null; v: VolumeDraft | null; bitPerfect: boolean }) {
  if (!vs || !v) return null;
  return vs.hardware && vs.software_db !== 0 ? (
    // Below the card's range (or with hybrid's split) part of the
    // level is digital: not bit-perfect.
    <span
      className="hw"
      title={`Hardware and digital volume: the DAC's control is at ${fmtDb(vs.hardware_db ?? 0, 1, false)} dB and the engine applies a further ${fmtDb(vs.software_db, 1, false)} dB digitally (64-bit float, before dither), so the output is not bit-perfect.`}
    >
      HW+DIG
    </span>
  ) : vs.hardware && vs.muted ? (
    // Muted, the signal is not passed through untouched: no
    // bit-perfect claim. The status does not say which mute is used.
    <span
      className="hw"
      title="Hardware volume: the DAC's own volume control carries the master volume. Muted: the output is silenced on the DAC's mute switch when it has one, otherwise digitally by the engine, so nothing is passed through bit-perfect."
    >
      HW
    </span>
  ) : vs.hardware && vs.balance_db === 0 ? (
    <span className="hw" title="Hardware volume: the DAC's own volume control is used, so the output stays bit-perfect">
      HW
    </span>
  ) : vs.hardware ? (
    // The master volume is in hardware, but the balance is digital.
    <span
      className="hw"
      title="Hardware volume: the DAC's own volume control carries the master volume. The balance is applied digitally on the DSP path, so that output is not bit-perfect."
    >
      HW
    </span>
  ) : bitPerfect ? null : (
    <span
      className="sw"
      title="Digital volume: the DAC has no hardware volume control, so the engine scales the signal in 64-bit float before dither. The Direct path is never scaled."
    >
      DIG
    </span>
  );
}

export function VolumeControls() {
  const { st, vs, v, epoch, begin, bitPerfect, off, dspBypassed, title, volTaper, volText } = useVolumeModel();
  const balOff = off || dspBypassed;
  const BAL_BYPASS_TIP = "Balance is applied on the DSP path only; Direct playback and Bypass stay bit-perfect.";
  const bal = v?.balance_db ?? 0;
  const balText = !v ? "-" : bal === 0 ? "C" : (bal < 0 ? "L " : "R ") + Math.abs(bal).toFixed(1);
  const balTaper = balanceTaper(BALANCE_MAX_DB);
  const hint = `Arrow keys: 0.5 dB, Shift+arrows: 0.1 dB, Page Up/Down: 6 dB. Double-click the value to type it. ${WHEEL_HINT}`;
  return (
    <>
      <div className={"ctl" + (bitPerfect ? " bit-perfect" : "")} title={off ? title : hint}>
        <label htmlFor="vol">Vol</label>
        <FineSlider
          key={`vol-${epoch}`}
          id="vol"
          label="Volume"
          value={v && vs ? v.volume_db : null}
          taper={volTaper}
          disabled={off}
          text={volText}
          title={off ? title : hint}
          onBegin={begin}
          onChange={(db, mode, g) => st.setVolume({ volume_db: db }, mode, g as VolumeGesture | null)}
        />
        <VolumeModeTag vs={vs} v={v} bitPerfect={bitPerfect} />
        {!bitPerfect && <HeldHeadroomTag />}
      </div>
      <div className="ctl" title={off ? title : dspBypassed ? BAL_BYPASS_TIP : `${hint} Double-click the slider to centre.`}>
        <label htmlFor="bal">Bal</label>
        <FineSlider
          key={`bal-${epoch}`}
          id="bal"
          label="Balance"
          value={v ? bal : null}
          taper={balTaper}
          disabled={balOff}
          fillFrom="centre"
          text={balText}
          title={off ? title : dspBypassed ? BAL_BYPASS_TIP : `±3 dB takes 70% of the travel. Double-click to centre. ${WHEEL_HINT}`}
          onBegin={begin}
          onChange={(db, mode, g) => st.setVolume({ balance_db: db }, mode, g as VolumeGesture | null)}
          onReset={() => st.setVolume({ balance_db: 0 }, "commit", begin())}
        />
      </div>
      <button
        className="icon-btn mute"
        aria-pressed={!!v?.muted}
        aria-label="Mute"
        title={!v || !vs ? title : v?.muted ? "Unmute" : "Mute"}
        disabled={!v || !vs}
        onClick={() => v && st.setVolume({ muted: !v.muted }, "commit", begin())}
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
  const abConfig = useApp((s) => s.config?.ab ?? null);
  const abCfg = !!abConfig;
  const abOff = useApp((s) => abStoppedByOverload(s.config?.ab, s.status?.overload));
  const bLabel = topBarLabel(abConfig);
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
              aria-pressed={abCfg && !ab?.blind && (ab?.current ?? "a") === side}
              disabled={!!ab?.blind}
              title={ab?.blind ? "A blind test is running: use its buttons in A/B compare" : abCfg ? `Listen to side ${side.toUpperCase()} (key ${side.toUpperCase()}; \` flips)` : "Set up side B in A/B compare"}
              onClick={() => (abCfg ? void st.abSelect(side) : st.setView("ab"))}
            >
              {side.toUpperCase()}
            </button>
          ))}
        </div>
        {bLabel && (
          <button
            type="button"
            className={"ab-label" + (abOff ? " warn" : "")}
            title={abOff ? "A/B is off after a DSP overload: only A plays. Open A/B compare for details." : `Side B is ${bLabel.slice(3)}. Press \` to flip between A and B; click for the level and settings.`}
            onClick={() => st.setView("ab")}
          >
            {abOff ? "A/B off: overload" : bLabel}
          </button>
        )}
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
