import { useState } from "react";
import { store, useApp } from "../store/store";
import { Logo } from "../components/Icons";

export function EngineDown() {
  const st = store();
  const starting = useApp((s) => s.starting);
  const installing = useApp((s) => s.installing);
  const appImage = useApp((s) => s.appImage);
  const err = useApp((s) => s.startError);
  const detail = useApp((s) => s.connDetail);
  const [atLogin, setAtLogin] = useState(true);
  // From the AppImage with nothing installed yet: no engine service exists,
  // so the page offers the per-user setup instead of "Start engine".
  const offerSetup = appImage !== null && !appImage.systemPackage && appImage.installed === null;
  return (
    <main className="down">
      <div className="down-card">
        <div className="brand">
          <Logo />
          <span>
            Audivane <b>DSP</b>
          </span>
        </div>
        {offerSetup ? (
          <>
            <h1>Set up Audivane DSP</h1>
            <p>
              The app controls the Audivane DSP engine, which runs in the background as a user service. Setting up installs it for your user only,
              without admin rights: the engine, this app and the <code>avdsp</code> command go to <code>~/.local</code>, with the engine's service and
              a menu entry. Run this AppImage with <code>--uninstall</code> to remove them again.
            </p>
            <label className="ups-check" title="Enables the engine's user service, so it starts every time you log in.">
              <input type="checkbox" checked={atLogin} onChange={(e) => setAtLogin(e.target.checked)} />
              <span>Start the engine when I log in</span>
            </label>
            <button className="btn primary" disabled={installing} onClick={() => void st.installAppImage(atLogin)}>
              {installing ? "Setting up…" : "Set up and start"}
            </button>
          </>
        ) : (
          <>
            <h1>Audivane engine is not running</h1>
            <p>The app controls the Audivane DSP engine, which runs in the background as a user service.{detail ? ` ${detail}` : ""}</p>
            {appImage?.systemPackage && <p>A system package of Audivane DSP is installed: this AppImage uses its engine.</p>}
            <label className="ups-check" title="Enables the engine's user service, so it starts every time you log in.">
              <input type="checkbox" checked={atLogin} onChange={(e) => setAtLogin(e.target.checked)} />
              <span>Also start it when I log in</span>
            </label>
            <button className="btn primary" disabled={starting} onClick={() => void st.startEngine(atLogin)}>
              {starting ? "Starting…" : "Start engine"}
            </button>
          </>
        )}
        {err && (
          <p className="err" role="alert">
            {err}
          </p>
        )}
      </div>
    </main>
  );
}
