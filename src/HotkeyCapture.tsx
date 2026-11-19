import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/**
 * HotkeyCapture (v0.8.1): a "press it, don't type it" hotkey field.
 *
 * Typing key names ("Fn+z") was the #1 hotkey support issue — users can't
 * be expected to know the parser's key vocabulary. This component replaces
 * every hotkey text input in the app (launcher summon hotkey, routine
 * hotkeys, per-command hotkey bindings).
 *
 * Interaction:
 * - Click the field -> it shows "Press your hotkey…" and a keydown
 *   listener captures the actual pressed combination.
 * - The field fills with the friendly form ("Ctrl + Alt + Z"); the value
 *   passed to onChange is the canonical backend form ("Ctrl+Alt+Z").
 * - Lone modifier presses (just Ctrl) keep listening and show a live
 *   preview; they are never saved.
 * - A main key with no modifier shows a hint and keeps listening — a
 *   modifier-free global hotkey would fire while the user types.
 * - Escape cancels capture without saving.
 * - The captured combination is validated against the backend's hotkey
 *   parser immediately (the same parser save-time registration uses). A
 *   rejection shows a plain-language inline error right away.
 *
 * Notes:
 * - The Fn key produces no keydown event on most hardware (handled below
 *   the OS), so it can never be captured. Not supported, by physics.
 * - Canonical form uses the exact tokens the backend parser accepts
 *   (see global-hotkey's parse_key): modifiers Ctrl/Alt/Shift/Super first
 *   in a fixed order, one main key last. "Super" is shown as "Win".
 */

function modsFromEvent(e: KeyboardEvent): string[] {
  const mods: string[] = [];
  if (e.ctrlKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (e.shiftKey) mods.push("Shift");
  if (e.metaKey) mods.push("Super");
  return mods;
}

function isModifierKey(key: string): boolean {
  return (
    key === "Control" || key === "Alt" || key === "Shift" || key === "Meta"
  );
}

/**
 * Map a KeyboardEvent.code to a backend-parser key token.
 * Only tokens global-hotkey's parser accepts are produced, so anything
 * this returns is guaranteed to parse (the backend validate call is the
 * final word).
 */
function codeToKey(code: string): string | null {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3); // KeyZ -> "Z"
  if (/^Digit[0-9]$/.test(code)) return code.slice(5); // Digit5 -> "5"
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code; // F1..F24
  if (/^Numpad[0-9]$/.test(code)) return "Num" + code.slice(6);
  switch (code) {
    case "Space":
    case "Enter":
    case "Tab":
    case "Backspace":
    case "Delete":
    case "Home":
    case "End":
    case "PageUp":
    case "PageDown":
    case "Insert":
      return code;
    case "ArrowUp":
      return "Up";
    case "ArrowDown":
      return "Down";
    case "ArrowLeft":
      return "Left";
    case "ArrowRight":
      return "Right";
    case "Minus":
      return "-";
    case "Equal":
      return "=";
    case "BracketLeft":
      return "[";
    case "BracketRight":
      return "]";
    case "Backslash":
      return "\\";
    case "Semicolon":
      return ";";
    case "Quote":
      return "'";
    case "Comma":
      return ",";
    case "Period":
      return ".";
    case "Slash":
      return "/";
    case "Backquote":
      return "`";
    default:
      return null;
  }
}

/** "Ctrl+Alt+Z" -> "Ctrl + Alt + Z". Tolerates legacy spacing/casing. */
export function friendlyHotkey(canonical: string): string {
  const trimmed = canonical.trim();
  if (!trimmed) return "";
  return trimmed
    .split("+")
    .map((t) => {
      const tok = t.trim();
      const low = tok.toLowerCase();
      if (
        low === "super" ||
        low === "cmd" ||
        low === "command" ||
        low === "meta"
      )
        return "Win";
      if (low === "control") return "Ctrl";
      if (tok.length <= 1) return tok.toUpperCase();
      return tok.charAt(0).toUpperCase() + tok.slice(1);
    })
    .join(" + ");
}

export interface HotkeyCaptureProps {
  /** Canonical value ("Ctrl+Alt+Z") or "" for none. */
  value: string;
  onChange: (canonical: string) => void;
  ariaLabel: string;
  placeholder?: string;
  disabled?: boolean;
  /** Show a small clear button when a value is set (for optional hotkeys). */
  allowClear?: boolean;
}

export function HotkeyCapture({
  value,
  onChange,
  ariaLabel,
  placeholder = "Click to set…",
  disabled = false,
  allowClear = false,
}: HotkeyCaptureProps) {
  const [capturing, setCapturing] = useState(false);
  const [preview, setPreview] = useState<string[]>([]);
  const [hint, setHint] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const fieldRef = useRef<HTMLButtonElement>(null);
  const checkingRef = useRef(false);

  function start() {
    if (disabled) return;
    setError(null);
    setHint(null);
    setPreview([]);
    setCapturing(true);
  }

  function cancel() {
    setCapturing(false);
    setPreview([]);
    setHint(null);
  }

  useEffect(() => {
    if (!capturing) return;

    async function finish(canonical: string) {
      checkingRef.current = true;
      setChecking(true);
      setPreview([]);
      setHint(null);
      try {
        // camelCase invoke arg for the snake_case Rust param `hotkey`.
        await invoke("validate_hotkey", { hotkey: canonical });
        setError(null);
        onChange(canonical);
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        checkingRef.current = false;
        setChecking(false);
        setCapturing(false);
      }
    }

    function onKeyDown(e: KeyboardEvent) {
      if (checkingRef.current) return;
      // Swallow everything while capturing: Tab must not move focus,
      // Space must not scroll, the combo must not trigger page actions.
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape") {
        cancel();
        return;
      }
      const mods = modsFromEvent(e);
      if (isModifierKey(e.key)) {
        // Lone modifier: keep listening, show what we have so far.
        setHint(null);
        setPreview(mods);
        return;
      }
      if (mods.length === 0) {
        setHint(
          "Add Ctrl, Alt, Shift or Win too — a hotkey needs at least one modifier."
        );
        setPreview([]);
        return;
      }
      const key = codeToKey(e.code);
      if (!key) {
        setHint(
          `"${e.key}" can't be a hotkey key. Try a letter, number, or function key.`
        );
        return;
      }
      const canonical = [...mods, key].join("+");
      void finish(canonical);
    }

    window.addEventListener("keydown", onKeyDown, true);
    return () => window.removeEventListener("keydown", onKeyDown, true);
  }, [capturing, onChange]);

  const shown = capturing
    ? checking
      ? "Checking…"
      : preview.length > 0
        ? `${preview
            .map((m) => (m === "Super" ? "Win" : m))
            .join(" + ")} + …`
        : "Press your hotkey…"
    : value
      ? friendlyHotkey(value)
      : placeholder;

  return (
    <>
      <span className="hotkey-capture">
        <button
          ref={fieldRef}
          type="button"
          className={`hotkey-capture-field${capturing ? " capturing" : ""}${
            !value && !capturing ? " empty" : ""
          }`}
          onClick={start}
          onBlur={() => {
            if (capturing) cancel();
          }}
          aria-label={ariaLabel}
          disabled={disabled}
          title={
            capturing
              ? "Press your hotkey, or Escape to cancel"
              : "Click, then press your hotkey"
          }
        >
          {shown}
        </button>
        {allowClear && value && !capturing && !disabled && (
          <button
            type="button"
            className="hotkey-clear"
            aria-label={`Clear ${ariaLabel}`}
            title="Clear"
            onClick={() => {
              setError(null);
              onChange("");
            }}
          >
            ×
          </button>
        )}
      </span>
      {capturing && hint && (
        <span className="muted small hotkey-hint">{hint}</span>
      )}
      {error && (
        <span className="form-error hotkey-hint" role="alert">
          {error}
        </span>
      )}
    </>
  );
}
