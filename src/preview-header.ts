// Preview control window logic. Bundled by Vite (NOT a plain public/ file),
// so we import `invoke` from @tauri-apps/api like the main app does — the
// window.__TAURI__ global is never injected (withGlobalTauri stays false on
// purpose, so site webviews never get the IPC bridge).
import "./tauriBrowserShim";
import { invoke } from "@tauri-apps/api/core";

declare global {
  interface Window {
    __APPMAKA_PREVIEW_ID__?: string;
    __APPMAKA_PREVIEW_URL__?: string;
  }
}

const id = window.__APPMAKA_PREVIEW_ID__;
const url = window.__APPMAKA_PREVIEW_URL__ || "";

const msg = document.getElementById("msg") as HTMLElement;
const addBtn = document.getElementById("add") as HTMLButtonElement;
const discardBtn = document.getElementById("discard") as HTMLButtonElement;
const reloadBtn = document.getElementById("reload") as HTMLButtonElement;
const labelInput = document.getElementById("label") as HTMLInputElement;
(document.getElementById("url") as HTMLElement).textContent = url;
(document.getElementById("dismiss") as HTMLButtonElement).addEventListener("click", () => {
  (document.getElementById("banner") as HTMLElement).classList.add("hidden");
});

function setBusy(busy: boolean): void {
  addBtn.disabled = busy;
  discardBtn.disabled = busy;
  reloadBtn.disabled = busy;
}

function fail(text?: string): void {
  msg.textContent = text || "Something went wrong.";
  setBusy(false);
}

if (!id) {
  fail("Preview bridge unavailable — close this window and try again.");
} else {
  addBtn.addEventListener("click", () => {
    setBusy(true);
    msg.textContent = "";
    const label = labelInput.value.trim();
    // The backend closes this window on success, so this promise may never
    // resolve — that is the happy path, not an error.
    invoke("preview_add", { previewId: id, label: label || null }).catch((e) =>
      fail(String(e)),
    );
  });

  discardBtn.addEventListener("click", () => {
    setBusy(true);
    invoke("preview_discard", { previewId: id }).catch((e) => fail(String(e)));
  });

  reloadBtn.addEventListener("click", () => {
    msg.textContent = "";
    invoke("preview_reload", { previewId: id }).catch((e) => fail(String(e)));
  });
}
