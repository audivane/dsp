// Device → Tray and volume keys: what a left click on the tray icon does,
// starting in the tray at login, the keyboard's volume keys (off by
// default) and the on-screen display. These are the app's own settings,
// kept by the app process (src-tauri/src/ui_settings.rs), not the engine's.

import { VOLUME_STEPS_DB, type KeysState, type OsdMode, type TrayClick } from "../api/shell";
import { Seg } from "../components/Seg";
import { useEffect, useState } from "react";
import { Close } from "../components/Icons";
import { setAutostart, setUiSettings, useShell } from "../store/shell";
import { store, useApp } from "../store/store";

const ACTION_NAME = { up: "Volume up", down: "Volume down", mute: "Mute" } as const;

const HIDE_MISSING_KEY = "avdsp-hide-missing-keys";

function missingHidden(): boolean {
  try {
    return localStorage.getItem(HIDE_MISSING_KEY) === "1";
  } catch {
    return false;
  }
}

function KeysStatus({ keys }: { keys: KeysState }) {
  const [hideMissing, setHideMissing] = useState(missingHidden);
  const dismissMissing = () => {
    setHideMissing(true);
    try {
      localStorage.setItem(HIDE_MISSING_KEY, "1");
    } catch {
      /* storage unavailable */
    }
  };
  switch (keys.state) {
    case "off":
      return null;
    case "starting":
      return (
        <span className="hint" role="status">
          Asking the desktop… it may show a dialog to confirm the keys.
        </span>
      );
    case "declined":
      return (
        <span className="notice warn" role="status">
          You said no in the desktop's dialog, and the desktop keeps that: the keys stay unbound. To use them, bind them in System Settings → Keyboard → Shortcuts (Audivane DSP: volume up, volume down, mute).
        </span>
      );
    case "unavailable":
      return (
        <span className="notice warn" role="status">
          {keys.reason}
        </span>
      );
    case "active": {
      const missing = keys.bound.filter((b) => !b.trigger);
      const via = keys.via === "portal" ? "through the desktop's shortcuts" : "by grabbing the keys (X11)";
      return (
        <span className="keys-status" role="status">
          <span className="hint">
            On, {via}: {keys.bound.map((b) => `${ACTION_NAME[b.action]} → ${b.trigger || "no key"}`).join(" · ")}
          </span>
          {missing.length > 0 && !hideMissing && (
            <span className="notice warn keys-missing">
              The desktop gave no key to {missing.map((b) => ACTION_NAME[b.action].toLowerCase()).join(", ")}: the key is probably still bound to the desktop's own volume. Unbind it in System Settings → Shortcuts, then assign it to “Audivane DSP: {missing[0].action === "mute" ? "mute" : ACTION_NAME[missing[0].action].toLowerCase()}”.
              <button className="icon-btn" aria-label="Dismiss" title="Don't show this again" onClick={dismissMissing}>
                <Close />
              </button>
            </span>
          )}
        </span>
      );
    }
  }
}

function EngineAtLogin() {
  const atLogin = useApp((s) => s.engineAtLogin);
  const err = useApp((s) => s.engineAtLoginError);
  useEffect(() => {
    void store().loadEngineAtLogin();
  }, []);
  return (
    <>
      <label className="ups-check" title="Enables the engine's user service (systemctl --user enable audivane-dsp), so it starts every time you log in.">
        <input type="checkbox" checked={atLogin === true} disabled={atLogin === null} onChange={(e) => void store().setEngineAtLogin(e.target.checked)} />
        <span>Start the engine when I log in</span>
      </label>
      {err && <span className="notice warn">{err}</span>}
    </>
  );
}

export function TraySettings() {
  const view = useShell((s) => s.view);
  const error = useShell((s) => s.error);
  if (!view) return null;
  const s = view.settings;
  const keysOn = s.volume_keys;
  return (
    <div className="panel tray-panel" aria-labelledby="tray-title">
      <h2 id="tray-title">Tray and volume keys</h2>
      <div className="row">
        <span className="k" title="What a left click on the Audivane tray icon does. A right click always opens the menu, and the menu has Volume… too.">
          Tray icon click
        </span>
        <Seg<TrayClick>
          label="Left click on the tray icon"
          value={s.tray_click}
          disabled={!view.tray}
          onChange={(tray_click) => void setUiSettings({ tray_click })}
          options={[
            ["popup", "Volume popup", "A small volume control in dB next to the icon. Closes on Escape or a click elsewhere."],
            ["menu", "Menu", "The tray menu, as before (show the window, bypass, A/B, presets, routing)."],
          ]}
        />
        {!view.tray && <span className="hint">No tray icon: this desktop has no tray (StatusNotifier) host.</span>}
      </div>
      <div className="row">
        <span className="k" title="Starts the app hidden in the tray when you log in, so the tray popup and the volume keys are there from the start. The engine starts on its own (systemd user service) either way.">
          Start at login
        </span>
        <label className="ups-check" title="Writes ~/.config/autostart/audivane-dsp-ui.desktop (removed again when you untick this). Nothing outside your home folder is changed.">
          <input type="checkbox" checked={view.autostart} onChange={(e) => void setAutostart(e.target.checked)} />
          <span>Start in the tray when I log in</span>
        </label>
        <EngineAtLogin />
      </div>
      <div className="row keys-row">
        <span className="k" title="The keyboard's volume up, down and mute keys change Audivane's volume in dB, through the same guarded write as the slider (the ceiling and the guard apply). Off by default.">
          Volume keys
        </span>
        <label className="ups-check" title="Registers the three keys as global shortcuts through the desktop's shortcuts portal (KDE Plasma; asks you to confirm), or by grabbing them on X11.">
          <input type="checkbox" checked={keysOn} onChange={(e) => void setUiSettings({ volume_keys: e.target.checked })} />
          <span>Use the keyboard's volume keys for Audivane</span>
        </label>
        <KeysStatus keys={view.keys} />
      </div>
      <div className="row">
        <span className="k" title="How much one volume-key press, or one notch of the mouse wheel over the tray icon, changes the volume.">
          Step
        </span>
        <Seg<number>
          label="Volume step per key press"
          value={s.volume_step_db}
          onChange={(volume_step_db) => void setUiSettings({ volume_step_db })}
          options={VOLUME_STEPS_DB.map((db) => [db, `${db} dB`, `${db} dB per key press or wheel notch`])}
        />
      </div>
      <div className="row">
        <span className="k" title="Which way the mouse wheel over the tray icon turns the volume. Wheel-up is louder on KDE Plasma and GNOME alike; tick this if your tray turns it the wrong way.">
          Tray wheel
        </span>
        <label className="ups-check" title="One volume step per wheel notch over the tray icon; this turns the direction round.">
          <input type="checkbox" checked={s.reverse_wheel} onChange={(e) => void setUiSettings({ reverse_wheel: e.target.checked })} />
          <span>Reverse wheel direction</span>
        </label>
      </div>
      <div className="row">
        <span className="k" title="A small display of the new level that shows for about a second and never takes the focus.">
          On-screen display
        </span>
        <Seg<OsdMode>
          label="On-screen volume display"
          value={s.osd}
          onChange={(osd) => void setUiSettings({ osd })}
          options={[
            ["off", "Off", "Never show it."],
            ["keys", "Volume keys", "After a volume key or the wheel over the tray icon."],
            ["any", "Any change", "After any change of the level: keys, the desktop slider, the command line, a guard step. Not while an Audivane window has the focus."],
          ]}
        />
        <span className="hint osd-where">
          {view.kde
            ? "On KDE, untick 'Show OSD popups for changes to: Audio volume' (right-click the speaker icon → Configure Audio Volume), or both popups will show."
            : "On KDE Plasma the desktop's own display shows it. Elsewhere on Wayland, Audivane's display only appears while an Audivane window has the focus, since it could take the focus from another app."}
        </span>
      </div>
      {error && (
        <div className="inline-err" role="alert">
          <span>{error}</span>
        </div>
      )}
    </div>
  );
}
