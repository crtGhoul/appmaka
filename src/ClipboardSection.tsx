import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { HotkeyCapture } from "./HotkeyCapture";
import type { ClipboardSettings } from "./types";

/** Built-in dictation status (v0.14.0, dictation.rs). */
interface DictationStatus {
  ready: boolean;
  binaryInstalled: boolean;
  model: string;
  modelInstalled: boolean;
  recording: boolean;
  transcribing: boolean;
  hotkey: string;
  holdToTalk: boolean;
}

/**
 * Clipboard history settings (v0.9.2, text + images, Win-key tap opt-in).
 *
 * Backend contract (src-tauri/src/clipboard.rs): `get_clipboard_settings`,
 * `set_clipboard_hotkey` (goes through the shared hotkey registry, so
 * conflicts come back as plain-language errors), `set_clipboard_win_tap`
 * (opt-in bare-Windows-key summon, Windows only — the capture field can't
 * capture a bare Win press, so this is a deliberate choice, not a capture),
 * `set_clipboard_cap` (text entries; images keep a fixed cap of 25),
 * `clear_clipboard` (wipes text + images), `clipboard_hotkey_status`.
 */

function errMsg(err: unknown): string {
  return typeof err === "string" ? err : "Something went wrong.";
}

interface HotkeyStatus {
  hotkey: string;
  registered: boolean;
  error: string | null;
}

export function ClipboardSection() {
  const [settings, setSettings] = useState<ClipboardSettings | null>(null);
  const [hotkey, setHotkey] = useState("");
  const [status, setStatus] = useState<HotkeyStatus | null>(null);
  const [hotkeyError, setHotkeyError] = useState<string | null>(null);
  const [capInput, setCapInput] = useState("100");
  const [capError, setCapError] = useState<string | null>(null);
  const [capSaved, setCapSaved] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [cleared, setCleared] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [winTap, setWinTap] = useState(false);
  const [winTapSupported, setWinTapSupported] = useState(false);
  const [winTapBusy, setWinTapBusy] = useState(false);
  // Dictation (v0.14.0).
  const [dict, setDict] = useState<DictationStatus | null>(null);
  const [dictHotkey, setDictHotkey] = useState("");
  const [dictStatus, setDictStatus] = useState<HotkeyStatus | null>(null);
  const [dictError, setDictError] = useState<string | null>(null);
  const [downloading, setDownloading] = useState(false);

  const refreshDict = useCallback(async () => {
    try {
      const [st, hs] = await Promise.all([
        invoke<DictationStatus>("get_dictation_status"),
        invoke<HotkeyStatus | null>("dictation_hotkey_status"),
      ]);
      setDict(st);
      setDictHotkey(st.hotkey);
      setDictStatus(hs);
    } catch {
      // Dictation is additive; a failure here must not break the section.
    }
  }, []);

  useEffect(() => {
    void refreshDict();
    const un = listen("dictation-state", () => void refreshDict());
    return () => {
      void un.then((u) => u());
    };
  }, [refreshDict]);

  async function saveDictHotkey(next: string) {
    setDictHotkey(next);
    setDictError(null);
    try {
      const st = await invoke<HotkeyStatus>("set_dictation_hotkey", {
        hotkey: next.trim(),
      });
      setDictStatus(st);
      if (st) setDictHotkey(st.hotkey);
    } catch (err) {
      setDictError(errMsg(err));
    }
  }

  async function downloadDictation() {
    setDownloading(true);
    setDictError(null);
    try {
      const st = await invoke<DictationStatus>("download_dictation");
      setDict(st);
    } catch (err) {
      setDictError(errMsg(err));
    } finally {
      setDownloading(false);
    }
  }

  async function saveDictHold(enabled: boolean) {
    setDictError(null);
    try {
      const st = await invoke<DictationStatus>("set_dictation_hold", {
        enabled,
      });
      setDict(st);
    } catch (err) {
      setDictError(errMsg(err));
    }
  }

  async function chooseModel(model: string) {
    setDictError(null);
    try {
      const st = await invoke<DictationStatus>("set_dictation_model", {
        model,
      });
      setDict(st);
    } catch (err) {
      setDictError(errMsg(err));
    }
  }

  const refresh = useCallback(async () => {
    try {
      const [s, st] = await Promise.all([
        invoke<ClipboardSettings>("get_clipboard_settings"),
        invoke<HotkeyStatus>("clipboard_hotkey_status"),
      ]);
      setSettings(s);
      setHotkey(s.hotkey);
      setCapInput(String(s.cap));
      setStatus(st);
      setWinTap(s.winTap);
      setWinTapSupported(s.winTapSupported);
      setLoadError(null);
    } catch (err) {
      setLoadError(errMsg(err));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function saveHotkey(next: string) {
    setHotkey(next);
    setHotkeyError(null);
    try {
      // Empty clears the binding (backend removes it).
      const saved = await invoke<string>("set_clipboard_hotkey", {
        hotkey: next.trim(),
      });
      setHotkey(saved);
      const st = await invoke<HotkeyStatus>("clipboard_hotkey_status");
      setStatus(st);
    } catch (err) {
      setHotkeyError(errMsg(err));
    }
  }

  async function setWinTapMode(enabled: boolean) {
    setWinTapBusy(true);
    setHotkeyError(null);
    try {
      const on = await invoke<boolean>("set_clipboard_win_tap", { enabled });
      setWinTap(on);
      // The combo binding is retired while the Win key summons; refresh
      // its status line so it doesn't claim to be live.
      const st = await invoke<HotkeyStatus>("clipboard_hotkey_status");
      setStatus(st);
    } catch (err) {
      setHotkeyError(errMsg(err));
    } finally {
      setWinTapBusy(false);
    }
  }

  async function saveCap() {
    setCapError(null);
    setCapSaved(false);
    const n = Number(capInput);
    if (!Number.isInteger(n) || n < 10 || n > 1000) {
      setCapError("Enter a whole number between 10 and 1000.");
      return;
    }
    try {
      const cap = await invoke<number>("set_clipboard_cap", { cap: n });
      setCapInput(String(cap));
      setCapSaved(true);
    } catch (err) {
      setCapError(errMsg(err));
    }
  }

  async function clearHistory() {
    setClearing(true);
    try {
      await invoke("clear_clipboard");
      setCleared(true);
    } catch (err) {
      setHotkeyError(errMsg(err));
    } finally {
      setClearing(false);
    }
  }

  if (loadError) {
    return <p className="modal-error">{loadError}</p>;
  }
  if (!settings) {
    return <p className="muted small">Loading clipboard settings…</p>;
  }

  return (
    <div>
      <span className="field-label" style={{ display: "block", marginBottom: 8 }}>
        How to open it
      </span>
      <div role="radiogroup" aria-label="Clipboard summon method">
        <label style={{ display: "block", marginBottom: 6, cursor: "pointer" }}>
          <input
            type="radio"
            name="clipboard-summon"
            checked={!winTap}
            disabled={winTapBusy}
            onChange={() => {
              if (winTap) void setWinTapMode(false);
            }}
            style={{ marginRight: 8 }}
          />
          Custom combo
        </label>
        {winTapSupported && (
          <label style={{ display: "block", marginBottom: 6, cursor: "pointer" }}>
            <input
              type="radio"
              name="clipboard-summon"
              checked={winTap}
              disabled={winTapBusy}
              onChange={() => {
                if (!winTap) void setWinTapMode(true);
              }}
              style={{ marginRight: 8 }}
            />
            Windows key (single tap)
          </label>
        )}
      </div>
      {!winTap ? (
        <label style={{ display: "block", fontSize: 14, marginBottom: 8 }}>
          <span className="field-label">Popup hotkey</span>
          <div style={{ marginTop: 4 }}>
            <HotkeyCapture
              value={hotkey}
              onChange={(v) => void saveHotkey(v)}
              ariaLabel="Clipboard history hotkey"
              placeholder="Click to set…"
              allowClear
            />
          </div>
        </label>
      ) : (
        <p className="muted small" style={{ margin: "4px 0 12px" }}>
          Single press opens the clipboard. Press twice for the Start menu
          (Ctrl+Esc works too). Win+letter shortcuts keep working.
        </p>
      )}
      {hotkeyError && (
        <p className="modal-error" role="alert">
          {hotkeyError}
        </p>
      )}
      {!winTap && status && !status.registered && !hotkeyError && (
        <p className="muted small" role="status">
          {status.error
            ? `Couldn't register "${status.hotkey}": ${status.error}`
            : "No popup hotkey set."}
        </p>
      )}
      <p className="muted small" style={{ margin: "0 0 12px" }}>
        Press it anywhere to search everything you've copied. Click an entry
        (or press Enter) to copy it back, then paste as usual. Screenshots
        and copied images are kept too — the last 25.
      </p>

      <label style={{ display: "block", fontSize: 14, marginBottom: 8 }}>
        <span className="field-label">Keep this many text entries</span>
        <div style={{ display: "flex", gap: 8, marginTop: 4 }}>
          <input
            type="number"
            min={10}
            max={1000}
            value={capInput}
            onChange={(e) => {
              setCapInput(e.target.value);
              setCapSaved(false);
            }}
            style={{ width: 100 }}
            aria-label="Clipboard history size"
          />
          <button onClick={() => void saveCap()}>Apply</button>
          {capSaved && (
            <span className="muted small" role="status">
              Saved.
            </span>
          )}
        </div>
      </label>
      {capError && (
        <p className="modal-error" role="alert">
          {capError}
        </p>
      )}

      <div style={{ marginTop: 12 }}>
        <button onClick={() => void clearHistory()} disabled={clearing}>
          {clearing ? "Clearing…" : "Clear history"}
        </button>
        {cleared && (
          <span className="muted small" role="status" style={{ marginLeft: 8 }}>
            History cleared.
          </span>
        )}
      </div>

      <p className="muted small" style={{ marginTop: 12 }}>
        Clipboard history stays on this PC — text and screenshots are never
        sent anywhere. It records everything you copy, including passwords,
        so clear it here if you have copied something sensitive. Win+V is
        Windows' own clipboard (off by default, can sync to your Microsoft
        account); this one is separate and stays here.
      </p>

      <div style={{ marginTop: 20, borderTop: "1px solid var(--border, #e4e6ea)", paddingTop: 14 }}>
        <span className="field-label" style={{ display: "block", marginBottom: 8 }}>
          Dictation
        </span>
        <p className="muted small" style={{ margin: "0 0 12px" }}>
          Use the dictation hotkey in any window — hold it while you
          speak, or press to start and stop (see Hold to talk below).
          What you said is pasted where you were typing, copied to the
          clipboard, and kept under the clipboard popup's Dictation tab —
          your last 50 transcripts. It all runs on this PC; the speech model
          only loads while you dictate, so it costs no memory the rest of
          the time.
        </p>
        <label style={{ display: "block", fontSize: 14, marginBottom: 8 }}>
          <span className="field-label">Dictation hotkey</span>
          <div style={{ marginTop: 4 }}>
            <HotkeyCapture
              value={dictHotkey}
              onChange={(v) => void saveDictHotkey(v)}
              ariaLabel="Dictation hotkey"
              placeholder="Click to set…"
              allowClear
            />
          </div>
        </label>
        {dictError && (
          <p className="modal-error" role="alert">
            {dictError}
          </p>
        )}
        {dictStatus && !dictStatus.registered && !dictError && (
          <p className="muted small" role="status">
            {dictStatus.error
              ? `Couldn't register "${dictStatus.hotkey}": ${dictStatus.error}`
              : "No dictation hotkey set."}
          </p>
        )}
        <label style={{ display: "block", fontSize: 14, marginBottom: 4, cursor: "pointer" }}>
          <input
            type="checkbox"
            checked={dict?.holdToTalk ?? true}
            onChange={(e) => void saveDictHold(e.target.checked)}
            style={{ marginRight: 8 }}
          />
          Hold to talk
        </label>
        <p className="muted small" style={{ margin: "0 0 12px" }}>
          On: hold the shortcut down, speak, and let go to send. Off:
          press once to start, and again (or Esc) to stop. The clipboard
          popup's Dictate button always works as press-to-start,
          press-to-stop.
        </p>
        <div role="radiogroup" aria-label="Speech model" style={{ marginBottom: 8 }}>
          <label style={{ display: "block", marginBottom: 6, cursor: "pointer" }}>
            <input
              type="radio"
              name="dictation-model"
              checked={!dict || dict.model !== "base"}
              onChange={() => void chooseModel("tiny")}
              style={{ marginRight: 8 }}
            />
            Tiny — lightest download, good for clear speech
          </label>
          <label style={{ display: "block", cursor: "pointer" }}>
            <input
              type="radio"
              name="dictation-model"
              checked={dict?.model === "base"}
              onChange={() => void chooseModel("base")}
              style={{ marginRight: 8 }}
            />
            Base — hears better, bigger download
          </label>
        </div>
        {dict && !dict.ready ? (
          <div>
            <button onClick={() => void downloadDictation()} disabled={downloading}>
              {downloading
                ? "Downloading…"
                : `Download speech model (about ${dict.model === "base" ? "150" : "85"} MB)`}
            </button>
            <p className="muted small" style={{ margin: "6px 0 0" }}>
              One-time download. Nothing is installed or loaded until this
              is done.
            </p>
          </div>
        ) : dict ? (
          <p className="muted small" role="status" style={{ margin: 0 }}>
            {dict.recording
              ? "Listening now…"
              : dict.transcribing
                ? "Transcribing…"
                : "Speech model ready."}
          </p>
        ) : null}
      </div>
    </div>
  );
}
