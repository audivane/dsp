import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { createTransport } from "./api";
import { initStore } from "./store/store";
import { initTheme } from "./theme";
import "./styles/fonts.css";
import "./styles/tokens.css";
import "./styles/app.css";

initTheme();
const store = initStore(createTransport());
// Browser/mock runs expose the store for tests and poking around.
if (store.getState().transport === "mock") (window as unknown as { avdsp: unknown }).avdsp = store;

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
