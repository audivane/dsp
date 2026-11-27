import { useEffect, useRef, useState } from "react";
import { StatusBar } from "./components/StatusBar";
import { Sidebar } from "./components/Sidebar";
import { TopBar } from "./components/TopBar";
import { Close } from "./components/Icons";
import { useMainViewMemory } from "./store/mainView";
import { store, useApp } from "./store/store";
import { AbView } from "./views/AbView";
import { ConvolutionView } from "./views/ConvolutionView";
import { DeviceView } from "./views/DeviceView";
import { EngineDown } from "./views/EngineDown";
import { EqView } from "./views/EqView";
import { OutputView } from "./views/OutputView";
import { RoutingView } from "./views/RoutingView";
import { UpsamplingView } from "./views/UpsamplingView";
import { CrossfeedView } from "./views/CrossfeedView";
import { DcBlockView } from "./views/DcBlockView";
import { HarmonicsView } from "./views/HarmonicsView";
import { LimiterView } from "./views/LimiterView";
import { PitchView } from "./views/PitchView";
import { versionSkew } from "./lib/versions";
import { assignments, deviceLabel, heldOutputName, heldWas, initialPicks } from "./lib/devices";
import type { UnresolvedDevice } from "./api/types";

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
  const pending = useApp((s) => s.updatePending);
  const skew = versionSkew(app, engine);
  // The app relaunches itself after an upgrade, once the user leaves this
  // window ("ready"); it asks for a restart only when that failed, or
  // while the versions differ.
  const ready = pending === "ready";
  const key = ready ? "ready" : skew ? skew.engine : pending ? "pending" : null;
  if (!key || dismissed === key) return null;
  const outdatedApp = ready || !skew || skew.kind === "app-outdated";
  return (
    <div className="update-banner" role="status">
      <span className="text">
        {ready
          ? "Audivane DSP was updated. It restarts when you leave this window."
          : !skew
            ? "Audivane DSP was updated. Restart the app to finish."
            : outdatedApp
              ? `Audivane DSP was updated to ${skew.engine}. Restart the app to finish.`
              : `The engine is still running ${skew.engine}; this app is ${skew.app}. Restart the engine to finish the update.`}
      </span>
      <button
        className="btn small primary"
        disabled={restarting}
        onClick={() => void (outdatedApp ? store().restartApp() : store().restartEngine())}
      >
        {restarting ? "Restarting…" : ready ? "Restart now" : outdatedApp ? "Restart" : "Restart engine"}
      </button>
      <button className="icon-btn" aria-label="Dismiss" title="Not now" onClick={() => store().dismissUpdate(key)}>
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
  const held = useApp((s) => s.status?.unresolved_devices);
  if (held?.length) return <HeldDevicesBanner held={held} />;
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

/**
 * Identical DACs without serial numbers both moved USB ports: the engine
 * cannot tell which is which, so it plays nothing on them until the user
 * says (one pick per held output, sent together so two can swap).
 */
function HeldDevicesBanner({ held }: { held: UnresolvedDevice[] }) {
  const devices = useApp((s) => s.devices);
  const key = held.map((u) => `${u.output}=${u.candidates.join("+")}`).join(";");
  const [picked, setPicked] = useState<{ key: string; picks: string[] }>({ key: "", picks: [] });
  const picks = picked.key === key ? picked.picks : initialPicks(held);
  const request = assignments(held, picks);
  return (
    <div className="update-banner bad held-devices" role="alert">
      <span className="text">
        Two identical DACs changed USB ports: pick which is which. Nothing plays on them until then.
        {held.map((u, i) => (
          <span key={u.output} className="held-row">
            <span>
              {heldOutputName(u, devices)} (was on {heldWas(u)}) is
            </span>
            <select
              aria-label={`Device for ${heldOutputName(u, devices)}`}
              value={picks[i]}
              onChange={(e) => setPicked({ key, picks: picks.map((p, j) => (j === i ? e.target.value : p)) })}
            >
              {u.candidates.map((c) => (
                <option key={c} value={c}>
                  {devices.some((d) => d.id === c) ? deviceLabel(devices.find((d) => d.id === c)!) : c}
                </option>
              ))}
            </select>
          </span>
        ))}
      </span>
      <button
        className="btn small primary"
        disabled={typeof request === "string"}
        title={typeof request === "string" ? request : "Play each output on the DAC picked for it"}
        onClick={() => typeof request !== "string" && void store().assignDevices(request)}
      >
        Confirm
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
      const run = st.getState().blindRun;
      if (run?.phase === "running") {
        // A blind run owns the keys: play A, B, X (or 1, 2), answer with F and J.
        const abx = run.plan.mode === "abx";
        if (abx ? k === "a" || k === "b" || k === "x" : k === "1" || k === "2") {
          e.preventDefault();
          void st.blindPlay(k as "a" | "b" | "x" | "1" | "2");
        } else if (k === "f" || k === "j") {
          e.preventDefault();
          st.blindAnswer(abx ? (k === "f" ? "A" : "B") : k === "f" ? "1" : "2");
        }
        return;
      }
      if (k === "a" || k === "b") {
        if (st.getState().config?.ab) void st.abSelect(k);
      } else if (e.key === "`") {
        if (st.getState().config?.ab) {
          e.preventDefault();
          void st.abFlip();
        }
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

/** A DAC plugged in or out while the window was in the background: read the device list again when it comes back. */
function useDevicesOnFocus() {
  useEffect(() => {
    const onFocus = () => {
      const st = store();
      const s = st.getState();
      if (s.conn === "up" && s.caps.devices) void st.refreshDevices();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
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
    case "dc_block":
      return <DcBlockView />;
    case "pitch":
      return <PitchView />;
    case "crossfeed":
      return <CrossfeedView />;
    case "harmonics":
      return <HarmonicsView />;
    case "limiter":
      return <LimiterView />;
    default:
      return <EqView />;
  }
}

/** The view area; its scroll position survives the window being closed to the tray. */
function MainArea() {
  const view = useApp((s) => s.view);
  const ref = useRef<HTMLElement>(null);
  useMainViewMemory(ref, view);
  return (
    <main className="main" id="main" ref={ref}>
      <MainView />
    </main>
  );
}

export function App() {
  const conn = useApp((s) => s.conn);
  const hasConfig = useApp((s) => !!s.config);
  const [grace, setGrace] = useState(true);
  useShortcuts();
  useDevicesOnFocus();
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
      <MainArea />
      <StatusBar />
      <Toast />
    </div>
  );
}
