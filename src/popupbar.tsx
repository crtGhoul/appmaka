import { useCallback, useState } from "react";
import ReactDOM from "react-dom/client";
import "./tauriBrowserShim";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Popup bar (v0.18.7): one small button docked under a popup window.
 *
 * Popups are raw site windows with no AppMaka chrome and no IPC, so the
 * button lives in our own trusted window instead of in the page — the
 * same reason tabbed windows get a strip window. Reads its popup label
 * from its own window label ("popupbar-{popupLabel}") and drives the
 * existing `popup_add_to_applications` command. On success the popup
 * closes itself (the command opens the new app window and closes the
 * popup); this bar closes with it.
 *
 * Bundled separately (vite.config.ts "popupbar" input, loaded by the
 * backend as popupbar.html).
 */

interface PopupAddOutcome {
  alreadyAdded: boolean;
  added: boolean;
  appName: string;
}

function popupLabelFromBar(label: string): string {
  return label.startsWith("popupbar-") ? label.slice("popupbar-".length) : "";
}

function App() {
  const [popupLabel] = useState(() =>
    popupLabelFromBar(getCurrentWindow().label),
  );
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const closeBar = useCallback(async () => {
    try {
      await getCurrentWindow().close();
    } catch {
      /* already gone; nothing to do */
    }
  }, []);

  const save = useCallback(async () => {
    if (busy || !popupLabel) return;
    setBusy(true);
    try {
      const outcome = await invoke<PopupAddOutcome>(
        "popup_add_to_applications",
        { label: popupLabel },
      );
      if (outcome.alreadyAdded) {
        setNote("This site is already in your applications.");
      } else {
        setNote(`Added to your applications: ${outcome.appName}`);
      }
      window.setTimeout(() => void closeBar(), 1200);
    } catch {
      setNote("Couldn't add this site.");
      setBusy(false);
    }
  }, [busy, popupLabel, closeBar]);

  if (note !== null) {
    return <span className="note">{note}</span>;
  }
  return (
    <>
      <button
        type="button"
        className="save"
        onClick={() => void save()}
        disabled={busy || !popupLabel}
      >
        Save as app
      </button>
      <button
        type="button"
        className="dismiss"
        aria-label="Dismiss"
        onClick={() => void closeBar()}
      >
        ×
      </button>
    </>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
