import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/**
 * One entry from the backend's recent-errors ring buffer (errors.rs).
 * `technical` is sanitized server-side — safe to display and copy.
 */
export interface ErrorEntry {
  id: number;
  unix_secs: number;
  kind: string;
  message: string;
  technical: string;
}

/**
 * Copyable error dialog (v0.11.0). The backend emits `appmaka:error` for
 * failures the user would otherwise never see (a popup that wouldn't
 * open, a restore window that died). Plain-language message up top,
 * expandable technical details, and a Copy button that copies the full
 * report (message + details + app version + OS) so it can be pasted
 * straight into chat.
 */
export function ErrorDialog({
  entry,
  onClose,
}: {
  entry: ErrorEntry;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const [copyFailed, setCopyFailed] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  // A newer error replaces the dialog: reset the copy feedback with it.
  useEffect(() => {
    setCopied(false);
    setCopyFailed(false);
  }, [entry.id]);

  async function handleCopy() {
    setCopyFailed(false);
    try {
      await invoke("copy_error_details", { id: entry.id });
      setCopied(true);
    } catch {
      setCopyFailed(true);
    }
  }

  const when = new Date(entry.unix_secs * 1000).toLocaleString();

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="modal-card"
        role="alertdialog"
        aria-modal="true"
        aria-label="Something went wrong"
      >
        <h2 className="modal-title">Something went wrong</h2>
        <p className="modal-sub">{entry.message}</p>
        <details className="error-details">
          <summary>Technical details</summary>
          <div className="error-details-body">
            <div className="muted small">
              {when} · {entry.kind}
            </div>
            <pre>{entry.technical || "No further details."}</pre>
          </div>
        </details>
        {copyFailed && (
          <p className="form-error" role="alert">
            Couldn&apos;t copy — you can still select the text above.
          </p>
        )}
        <div className="modal-actions">
          <button type="button" className="text-button" onClick={onClose}>
            Close
          </button>
          <button
            type="button"
            className="modal-primary"
            onClick={() => void handleCopy()}
          >
            {copied ? "Copied" : "Copy"}
          </button>
        </div>
      </div>
    </div>
  );
}
