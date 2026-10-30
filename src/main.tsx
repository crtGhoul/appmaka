import React from "react";
import ReactDOM from "react-dom/client";
import "./tauriBrowserShim";
import App from "./App";
import "./styles.css";
import "./visibility";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
