import { useState } from "react";
import { store, useApp } from "../store/store";
import { Logo } from "../components/Icons";

export function EngineDown() {
  const st = store();
  const starting = useApp((s) => s.starting);
  const err = useApp((s) => s.startError);
  const detail = useApp((s) => s.connDetail);
  const [atLogin, setAtLogin] = useState(true);
  return (
    <main className="down">
      <div className="down-card">
        <div className="brand">
          <Logo />
          <span>
            Audivane <b>DSP</b>
          </span>
        </div>
        <h1>Audivane engine is not running</h1>
        <p>The app controls the Audivane DSP engine, which runs in the background as a user service.{detail ? ` ${detail}` : ""}</p>
        <label className="ups-check" title="Enables the engine's user service, so it starts every time you log in.">
          <input type="checkbox" checked={atLogin} onChange={(e) => setAtLogin(e.target.checked)} />
          <span>Also start it when I log in</span>
        </label>
        <button className="btn primary" disabled={starting} onClick={() => void st.startEngine(atLogin)}>
          {starting ? "Starting…" : "Start engine"}
        </button>
        {err && (
          <p className="err" role="alert">
            {err}
          </p>
        )}
      </div>
    </main>
  );
}
