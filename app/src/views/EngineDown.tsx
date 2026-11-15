import { store, useApp } from "../store/store";
import { Logo } from "../components/Icons";

export function EngineDown() {
  const st = store();
  const starting = useApp((s) => s.starting);
  const err = useApp((s) => s.startError);
  const detail = useApp((s) => s.connDetail);
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
        <button className="btn primary" disabled={starting} onClick={() => void st.startEngine()}>
          {starting ? "Starting…" : "Start engine"}
        </button>
        {err && (
          <p className="err" role="alert">
            {err}
          </p>
        )}
        <p>
          To start it at every login, run <code>systemctl --user enable --now audivane-dsp</code>.
        </p>
      </div>
    </main>
  );
}
