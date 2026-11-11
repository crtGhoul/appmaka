import React from "react";
import ReactDOM from "react-dom/client";
// Install the browser preview shim before any @tauri-apps/api calls run.
// No-op inside the real Tauri desktop binary.
import "./tauriBrowserShim";
import App from "./App";
import { NowPlayingBar } from "./NowPlayingBar";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
    <NowPlayingBar />
  </React.StrictMode>,
);
