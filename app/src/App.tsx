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
      <Sidebar />
      <main className="main" id="main">
        <MainView />
      </main>
      <StatusBar />
      <Toast />
    </div>
  );
}
