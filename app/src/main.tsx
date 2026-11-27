import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { createTransport } from "./api";
import { initShell } from "./store/shell";
import { currentMainView } from "./store/mainView";
import { initStore } from "./store/store";
import { initTheme } from "./theme";
import { TrayPopup } from "./views/TrayPopup";
import { Osd } from "./views/Osd";
import "./styles/fonts.css";
import "./styles/tokens.css";
import "./styles/app.css";

initTheme();
const transport = createTransport();
// One frontend, three windows (see src-tauri/src/app.rs): the app, the
// tray's volume popup and the on-screen display.
const label = transport.windowLabel();
document.documentElement.dataset.window = label;
const root = createRoot(document.getElementById("root")!);

if (label === "osd") {
  if (transport.kind === "mock") {
    (window as unknown as { avdspShell: unknown }).avdspShell = transport;
  }
  root.render(
    <StrictMode>
      <Osd transport={transport} />
    </StrictMode>,
  );
} else if (label === "popup") {
  const store = initStore(transport, { secondary: true });
  if (store.getState().transport === "mock") {
    const w = window as unknown as { avdsp: unknown; avdspShell: unknown };
    w.avdsp = store;
    w.avdspShell = transport;
  }
  store.connect();
  root.render(
    <StrictMode>
      <TrayPopup transport={transport} />
    </StrictMode>,
  );
} else {
  const store = initStore(transport);
  initShell(transport);
  // Browser/mock runs expose the store (and the shell) for tests and poking around.
  if (store.getState().transport === "mock") {
    const w = window as unknown as { avdsp: unknown; avdspShell: unknown };
    w.avdsp = store;
    w.avdspShell = transport;
  }
  // Closing to the tray destroys this page: hand over where it is first
  // (the debounced scroll save may not have gone out yet).
  transport.onShell((e) => {
    if (e.kind === "main-closing") transport.mainCloseReady(currentMainView());
    if (e.kind === "update-pending") store.markUpdatePending(e.state);
  });
  // Volume keys and the tray wheel: in the app, the app process is their
  // one writer (src-tauri/src/volume_step.rs) and this window only shows
  // the result. The browser mock has no app process, so there the store
  // stands in for it with the same rules (`volumeKey`).
  if (transport.kind === "mock") {
    transport.onShell((e) => {
      if (e.kind !== "volume-key") return;
      const shown = store.volumeKey(e.action, e.step_db);
      if (shown) transport.osdShow(shown, e.source);
    });
  }
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
