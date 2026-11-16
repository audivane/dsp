import { useEffect, useState } from "react";
import { StatusBar } from "./components/StatusBar";
import { Sidebar } from "./components/Sidebar";
import { TopBar } from "./components/TopBar";
import { Close } from "./components/Icons";
import { store, useApp } from "./store/store";
import { AbView } from "./views/AbView";
import { ConvolutionView } from "./views/ConvolutionView";
import { DeviceView } from "./views/DeviceView";
import { EngineDown } from "./views/EngineDown";
import { EqView } from "./views/EqView";
import { OutputView } from "./views/OutputView";
import { RoutingView } from "./views/RoutingView";
import { UpsamplingView } from "./views/UpsamplingView";
import { versionSkew } from "./lib/versions";

function Toast() {
  const t = useApp((s) => s.toast);
  if (!t) return null;
  return (
    <div className={"toast " + t.tone} role={t.tone === "error" ? "alert" : "status"}>
      <span>{t.text}</span>
      <button className="icon-btn" aria-label="Dismiss" onClick={() => store().dismissToast()}>
        <Close />
      </button>
    </div>
  );
}

/** After a package upgrade: the app or the engine still runs the old code. */
function UpdateBanner() {
  const app = useApp((s) => s.appVersion);
  const engine = useApp((s) => s.status?.version);
  const dismissed = useApp((s) => s.updateDismissed);
  const restarting = useApp((s) => s.restarting);
  const skew = versionSkew(app, engine);
  if (!skew || dismissed === skew.engine) return null;
  const outdatedApp = skew.kind === "app-outdated";
  return (
    <div className="update-banner" role="status">
      <span className="text">
        {outdatedApp
          ? `Audivane DSP was updated to ${skew.engine}. Restart the app to finish.`
          : `The engine is still running ${skew.engine}; this app is ${skew.app}. Restart the engine to finish the update.`}
      </span>
      <button
        className="btn small primary"
        disabled={restarting}
        onClick={() => void (outdatedApp ? store().restartApp() : store().restartEngine())}
      >
        {restarting ? "Restarting…" : outdatedApp ? "Restart" : "Restart engine"}
      </button>
      <button className="icon-btn" aria-label="Dismiss" title="Not now" onClick={() => store().dismissUpdate()}>
        <Close />
      </button>
    </div>
  );
}

/**
 * The engine runs but cannot play, e.g. its DAC is unplugged or the card
 * was renumbered after a reboot. Takes the banner slot before the update
 * banner: nothing plays until it is fixed.
 */
function EngineErrorBanner() {
  const state = useApp((s) => s.status?.state);
  const error = useApp((s) => s.status?.error);
  const view = useApp((s) => s.view);
  if (state !== "error" || !error) return <UpdateBanner />;
  return (
    <div className="update-banner bad" role="alert">
      <span className="text">{error}</span>
      {view !== "device" && (
        <button className="btn small primary" onClick={() => store().setView("device")}>
          Choose output
        </button>
      )}
    </div>
  );
}

function isTyping(el: EventTarget | null) {
  const t = el as HTMLElement | null;
  if (!t) return false;
  const tag = t.tagName?.toLowerCase();
  // The fine sliders are divs with role="slider": while one has focus, arrows
  // and typing are its own, not the app's A/B and Delete shortcuts.
  return tag === "input" || tag === "select" || tag === "textarea" || t.isContentEditable || t.getAttribute("role") === "slider";
}

function useShortcuts() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const st = store();
      if (st.getState().conn !== "up" || isTyping(e.target)) return;
      const k = e.key.toLowerCase();
      if ((e.ctrlKey || e.metaKey) && k === "z") {
        e.preventDefault();
        if (e.shiftKey) st.redo();
        else st.undo();
        return;
      }
      if ((e.ctrlKey || e.metaKey) && k === "y") {
        e.preventDefault();
        st.redo();
        return;
      }
      if (e.ctrlKey || e.metaKey || e.altKey) return;
      if (k === "a" || k === "b") {
        if (st.getState().config?.ab) void st.abSelect(k);
      } else if ((k === "delete" || k === "backspace") && st.getState().view === "eq") {
        const i = st.getState().selectedBand;
        if (st.editedSet().bands[i]) {
          e.preventDefault();
          st.deleteBand(i);
        }
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);
}

function MainView() {
  const view = useApp((s) => s.view);
  switch (view) {
    case "conv":
      return <ConvolutionView />;
    case "ups":
      return <UpsamplingView />;
    case "out":
      return <OutputView />;
    case "routing":
      return <RoutingView />;
    case "ab":
      return <AbView />;
    case "device":
      return <DeviceView />;
    default:
      return <EqView />;
  }
}

export function App() {
  const conn = useApp((s) => s.conn);
  const hasConfig = useApp((s) => !!s.config);
  const [grace, setGrace] = useState(true);
  useShortcuts();
  useEffect(() => {
    store().connect();
    const t = setTimeout(() => setGrace(false), 1200);
    return () => clearTimeout(t);
  }, []);

  if (conn !== "up" || !hasConfig) {
    // Avoid flashing the "not running" screen during a quick (re)connect.
    if (conn === "down" && !grace) return (<><EngineDown /><Toast /></>);
    return <div className="connecting" role="status">Connecting to the engine…</div>;
  }
  return (
    <div className="app">
      <TopBar />
      <EngineErrorBanner />
      <Sidebar />
      <main className="main" id="main">
        <MainView />
      </main>
      <StatusBar />
      <Toast />
    </div>
  );
}
