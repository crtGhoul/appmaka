import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { check } from "@tauri-apps/plugin-updater";
import type { Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { revealItemInDir } from "@tauri-apps/plugin-opener";
import { isAppHidden } from "./visibility";
import { RamDashboard } from "./RamDashboard";
import { ForgetLoginDialog, forgetLogin } from "./ForgetLoginDialog";
import { ErrorDialog } from "./ErrorDialog";
import type { ErrorEntry } from "./ErrorDialog";
import LinkPicker from "./LinkPicker";
import type { LinkPickerAccount } from "./LinkPicker";
import LinkRules from "./LinkRules";
import {
  DownloadsProvider,
  useDownloads,
  DownloadsPage,
  DownloadToolbarButton,
} from "./downloads";
import { RoutinesSection } from "./RoutinesSection";
import { ClipboardSection } from "./ClipboardSection";
import WorkspacesSection from "./WorkspacesSection";
import { filterAppsByWorkspace } from "./WorkspacesSection";
import type { WorkspaceList } from "./WorkspacesSection";
import { useHotkeyDispatch } from "./useHotkeyDispatch";
import { CmdHotkeysSection } from "./CmdHotkeysSection";
import "./App.css";
import {
  AddProgramButton,
  ContextMenuGuard,
  EDIT_APP_EVENT,
  GRID_COLUMNS,
  IconGrid,
  LauncherCommandRows,
  LauncherSettingsPanel,
  NoMatchesHint,
  ProgramIcon,
  RescanButton,
  SearchBar,
  SearchTileIcon,
  WorkspaceTileIcon,
  browseAll,
  buildResults,
  matchLauncherCommand,
  recordLaunch,
  searchTag,
  resolveWorkspaceTargets,
  useProgramsScannedRefresh,
  useTileMenu,
} from "./Launcher";
import type { LauncherCommandRowsHandle, SearchResult } from "./Launcher";
import type {
  Account,
  AddAppOutcome,
  AppSettings,
  LauncherSettings,
  NativeProgram,
  OpenAccountWindow,
  PlatformInfo,
  PreviewAddOutcome,
  PreviewStart,
  Routine,
  SessionRestoreOffer,
  WebApp,
} from "./types";

const DEFAULT_SETTINGS: AppSettings = {
  popup_policy: "block",
  popup_allowlist: [],
  adblock_enabled: true,
  auto_suspend_minutes: 30,
  auto_close_minutes: 30,
};

const DEFAULT_COLOR = "#64748b";

function isValidHexColor(s: string | null | undefined): s is string {
  return typeof s === "string" && /^#[0-9a-fA-F]{6}$/.test(s);
}

function safeColor(s: string | null | undefined): string {
  return isValidHexColor(s) ? s : DEFAULT_COLOR;
}

/**
 * Normalize a user-typed URL. Adds https:// when no scheme is present and
 * rejects anything that is not a valid http(s) URL. Returns null when invalid.
 */
function normalizeUrl(input: string): string | null {
  let s = input.trim();
  if (!s) return null;
  if (!/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(s)) {
    s = "https://" + s;
  }
  try {
    const u = new URL(s);
    if (u.protocol !== "http:" && u.protocol !== "https:") return null;
    if (!u.hostname) return null;
    return u.toString();
  } catch {
    return null;
  }
}

function faviconUrl(appUrl: string): string | null {
  try {
    return new URL("/favicon.ico", appUrl).toString();
  } catch {
    return null;
  }
}

function hostOf(appUrl: string): string {
  try {
    return new URL(appUrl).hostname;
  } catch {
    return appUrl;
  }
}

function errMsg(err: unknown): string {
  return typeof err === "string" ? err : "Something went wrong.";
}

/**
 * Inline rename: click the text to edit it, Enter to save, Esc to cancel,
 * clicking away saves too. Empty input reverts instead of saving blank.
 */
function InlineEdit({
  value,
  onSave,
  className,
  maxLength,
}: {
  value: string;
  onSave: (next: string) => void;
  className?: string;
  maxLength?: number;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);

  function start() {
    setDraft(value);
    setEditing(true);
  }

  function commit() {
    const clean = draft.trim();
    setEditing(false);
    if (clean && clean !== value) onSave(clean);
  }

  function cancel() {
    setEditing(false);
    setDraft(value);
  }

  if (!editing) {
    return (
      <span
        className={`inline-edit${className ? ` ${className}` : ""}`}
        onClick={start}
        title="Click to rename"
        role="button"
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "Enter") start();
        }}
      >
        {value}
      </span>
    );
  }
  return (
    <input
      className="inline-edit-input"
      value={draft}
      maxLength={maxLength ?? 80}
      autoFocus
      onFocus={(e) => e.target.select()}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") commit();
        else if (e.key === "Escape") cancel();
      }}
      aria-label="Rename"
    />
  );
}

/**
 * Quick-add: paste a URL, the app is named from the page title (best-effort;
 * falls back to a prettified domain when the title can't be fetched). The
 * manual name+URL form stays available as a secondary path in the library.
 */
function QuickAddForm({
  onAdded,
  onError,
}: {
  onAdded: (outcome: AddAppOutcome) => void;
  onError: (msg: string) => void;
}) {
  const [quickUrl, setQuickUrl] = useState("");
  const [quickAdding, setQuickAdding] = useState(false);
  const [quickError, setQuickError] = useState<string | null>(null);

  function prettifiedDomain(rawUrl: string): string {
    try {
      const host = new URL(rawUrl).hostname.replace(/^www\./, "");
      if (!host) return rawUrl;
      return host.charAt(0).toUpperCase() + host.slice(1);
    } catch {
      return rawUrl;
    }
  }

  async function handleQuickAdd(e: React.FormEvent) {
    e.preventDefault();
    setQuickError(null);
    const cleanUrl = normalizeUrl(quickUrl);
    if (!cleanUrl) {
      setQuickError("Enter a valid URL, e.g. https://example.com");
      return;
    }
    setQuickAdding(true);
    try {
      let name: string;
      try {
        const title = await invoke<string>("fetch_page_title", { url: cleanUrl });
        name = title.trim() || prettifiedDomain(cleanUrl);
      } catch {
        name = prettifiedDomain(cleanUrl);
      }
      const created = await invoke<AddAppOutcome>("add_app", { name, url: cleanUrl });
      onAdded(created);
      setQuickUrl("");
    } catch (err) {
      const msg = errMsg(err);
      setQuickError(msg);
      onError(msg);
    } finally {
      setQuickAdding(false);
    }
  }

  return (
    <form className="quick-add-form" onSubmit={(e) => void handleQuickAdd(e)}>
      <input
        value={quickUrl}
        onChange={(e) => setQuickUrl(e.target.value)}
        placeholder="Paste a website URL, e.g. https://mail.google.com"
        inputMode="url"
        autoComplete="off"
        aria-label="Website URL"
      />
      <button type="submit" disabled={quickAdding}>
        {quickAdding ? "Adding…" : "Add app"}
      </button>
      {quickError && (
        <p className="form-error" role="alert">
          {quickError}
        </p>
      )}
    </form>
  );
}

/**
 * Preview & sign in: for sites that need a login, open the real site in a
 * throwaway preview window. The user signs in there directly (we never touch
 * credentials), then clicks "Add as app" in the preview's header — the app
 * is created with its first account already signed in. "Discard" (or closing
 * the preview window) throws the session away.
 */
function PreviewSignInForm({ onError }: { onError: (msg: string) => void }) {
  const [open, setOpen] = useState(false);
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [previewOpen, setPreviewOpen] = useState(false);

  // The control window closes previews on its own (Add/Discard/X) — clear
  // the "preview opened" notice when that happens.
  useEffect(() => {
    let off: (() => void) | undefined;
    listen("appmaka:preview-closed", () => setPreviewOpen(false))
      .then((unlisten) => {
        off = unlisten;
      })
      .catch(() => {});
    return () => off?.();
  }, []);

  async function handleOpenPreview(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    const cleanUrl = normalizeUrl(url);
    if (!cleanUrl) {
      setFormError("Enter a valid URL, e.g. https://example.com");
      return;
    }
    setBusy(true);
    try {
      // Tauri exposes Rust snake_case params as camelCase to JS.
      await invoke<PreviewStart>("preview_start", { url: cleanUrl });
      setPreviewOpen(true);
      setUrl("");
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
    } finally {
      setBusy(false);
    }
  }

  if (!open) {
    return (
      <button className="text-button" onClick={() => setOpen(true)}>
        Preview &amp; sign in instead
      </button>
    );
  }

  return (
    <div className="preview-signin">
      <form className="quick-add-form" onSubmit={(e) => void handleOpenPreview(e)}>
        <input
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          placeholder="Paste a website URL to preview & sign in"
          inputMode="url"
          autoComplete="off"
          aria-label="Website URL to preview"
        />
        <button type="submit" disabled={busy}>
          {busy ? "Opening…" : "Open preview"}
        </button>
        {formError && (
          <p className="form-error" role="alert">
            {formError}
          </p>
        )}
      </form>
      {previewOpen && (
        <p className="muted small">
          Preview opened — sign in on the real site, then click “Add to the
          Forge” in its header. Nothing is kept until you do.
        </p>
      )}
      <p className="muted small">
        Best for sites that need a login. For sites that don’t, the quick add
        above is faster.
      </p>
    </div>
  );
}

/** "opened 5 minutes ago" / "opened 2 days ago" / "never opened" for last_opened (0 = never). */
function openedLabel(lastOpened: number): string {
  if (!lastOpened) return "never opened";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000 - lastOpened));
  if (seconds < 60) return "opened just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `opened ${minutes} minute${minutes === 1 ? "" : "s"} ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `opened ${hours} hour${hours === 1 ? "" : "s"} ago`;
  const days = Math.floor(hours / 24);
  return `opened ${days} day${days === 1 ? "" : "s"} ago`;
}

function AppIcon({ app }: { app: WebApp }) {
  const [failed, setFailed] = useState(false);
  // `app.icon` is a locally cached logo file (absolute path); fall back to
  // the live /favicon.ico, then to a letter tile when all fetching fails.
  const src = !failed
    ? app.icon
      ? convertFileSrc(app.icon)
      : faviconUrl(app.url)
    : null;
  if (!src) {
    return (
      <span
        className="app-icon-fallback"
        aria-hidden="true"
        style={{ backgroundColor: `${safeColor(app.color)}22`, color: safeColor(app.color) }}
      >
        {app.name.charAt(0).toUpperCase() || "?"}
      </span>
    );
  }
  return (
    <img
      className="app-icon"
      src={src}
      alt=""
      loading="lazy"
      onError={() => setFailed(true)}
    />
  );
}

/**
 * Account tile thumbnail: the self-generated og:image when the backend
 * managed to fetch one, else the app's cached logo, else a letter tile.
 * Each failed stage falls through to the next.
 */
function AccountThumb({ account, app }: { account: Account; app: WebApp }) {
  const [thumbFailed, setThumbFailed] = useState(false);
  const [iconFailed, setIconFailed] = useState(false);
  const thumbSrc =
    !thumbFailed && account.thumbnail ? convertFileSrc(account.thumbnail) : null;
  const appIconSrc = !iconFailed && app.icon ? convertFileSrc(app.icon) : null;
  const src = thumbSrc ?? appIconSrc;
  if (!src) {
    return (
      <span
        className="account-thumb-fallback"
        aria-hidden="true"
        style={{
          backgroundColor: `${safeColor(app.color)}22`,
          color: safeColor(app.color),
        }}
      >
        {account.label.charAt(0).toUpperCase() || "?"}
      </span>
    );
  }
  return (
    <img
      className="account-thumb"
      src={src}
      alt=""
      loading="lazy"
      onError={() => {
        if (thumbSrc) setThumbFailed(true);
        else setIconFailed(true);
      }}
    />
  );
}

function AccountRow({
  account,
  app,
  onOpen,
  onOpenPrivate,
  onFillLogin,
  onSaveLogin,
  onSuspend,
  onRemove,
  onRename,
  onEdit,
  onForgetLogin,
}: {
  account: Account;
  app: WebApp;
  onOpen: () => void;
  onOpenPrivate: () => void;
  onFillLogin: () => void;
  onSaveLogin: () => void;
  onSuspend: () => void;
  onRemove: () => void;
  onRename: (label: string) => void;
  onEdit: () => void;
  onForgetLogin: () => void;
}) {
  return (
    <li className="account-row">
      <AccountThumb account={account} app={app} />
      <div className="account-meta">
        <InlineEdit
          value={account.label}
          onSave={onRename}
          className="account-label"
          maxLength={60}
        />
        <span className="account-opened">{openedLabel(account.last_opened)}</span>
        {account.popup_policy && (
          <span
            className="popup-badge"
            title="Per-account popup override (inherits app setting otherwise)"
          >
            Popups: {account.popup_policy === "allow" ? "allowed" : "blocked"}
          </span>
        )}
      </div>
      <div className="app-actions">
        <button onClick={onOpen}>Open</button>
        <button
          className="text-button"
          onClick={onOpenPrivate}
          title="Open in a private window — nothing is saved after you close it."
        >
          Private
        </button>
        <button
          className="text-button"
          onClick={onFillLogin}
          title="Fill the saved login into this account's open window. Only works on the site it was saved for."
        >
          Fill login
        </button>
        <button
          className="text-button"
          onClick={onSaveLogin}
          title="Save this account's username and password in your system vault."
        >
          Save login
        </button>
        <button onClick={onSuspend}>Suspend</button>
        <button className="text-button" onClick={onEdit}>
          Edit
        </button>
        <button
          className="text-button danger"
          onClick={onForgetLogin}
          title="Sign this account out by wiping its session. Other accounts are untouched."
        >
          Forget login
        </button>
        <button className="danger" onClick={onRemove}>
          Remove
        </button>
      </div>
    </li>
  );
}

/**
 * Per-account edit dialog: label, color, a three-way popup policy choice,
 * and per-account overrides for the idle timers and ad blocking — each
 * defaulting to "use the app setting" (inherit), exactly like popups.
 */
function EditAccountDialog({
  app,
  account,
  onClose,
  onSaved,
}: {
  app: WebApp;
  account: Account;
  onClose: () => void;
  onSaved: (app: WebApp) => void;
}) {
  const [label, setLabel] = useState(account.label);
  const [color, setColor] = useState(safeColor(account.color));
  const [popupChoice, setPopupChoice] = useState<"inherit" | "block" | "allow">(
    account.popup_policy ?? "inherit"
  );
  const [suspendChoice, setSuspendChoice] = useState<"inherit" | "custom">(
    account.auto_suspend_minutes == null ? "inherit" : "custom"
  );
  const [suspendCustom, setSuspendCustom] = useState(
    String(account.auto_suspend_minutes ?? app.settings.auto_suspend_minutes)
  );
  const [closeChoice, setCloseChoice] = useState<"inherit" | "custom">(
    account.auto_close_minutes == null ? "inherit" : "custom"
  );
  const [closeCustom, setCloseCustom] = useState(
    String(account.auto_close_minutes ?? app.settings.auto_close_minutes)
  );
  const [adblockChoice, setAdblockChoice] = useState<"inherit" | "on" | "off">(
    account.adblock_enabled == null
      ? "inherit"
      : account.adblock_enabled
        ? "on"
        : "off"
  );
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  // Escape closes the dialog.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  async function handleSave(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    const cleanLabel = label.trim();
    if (!cleanLabel) {
      setFormError("Give the account a label.");
      return;
    }
    if (!isValidHexColor(color)) {
      setFormError("Color must look like #6366f1.");
      return;
    }
    setSaving(true);
    try {
      // Tauri exposes Rust snake_case params as camelCase to JS; null means
      // "inherit the app setting" and deserializes to Rust's None.
      const updated = await invoke<WebApp>("update_account", {
        appId: app.id,
        accountId: account.id,
        label: cleanLabel,
        color,
        popupPolicy: popupChoice === "inherit" ? null : popupChoice,
        autoSuspendMinutes:
          suspendChoice === "inherit"
            ? null
            : Math.max(0, parseInt(suspendCustom, 10) || 0),
        autoCloseMinutes:
          closeChoice === "inherit"
            ? null
            : Math.max(0, parseInt(closeCustom, 10) || 0),
        adblockEnabled:
          adblockChoice === "inherit" ? null : adblockChoice === "on",
      });
      onSaved(updated);
    } catch (err) {
      setFormError(errMsg(err));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div
      className="dialog-overlay"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label={`Edit account ${account.label}`}
    >
      <div className="dialog" onClick={(e) => e.stopPropagation()}>
        <h3>Edit account</h3>
        <form onSubmit={(e) => void handleSave(e)}>
          <label>
            <span>Label</span>
            <input
              value={label}
              onChange={(e) => setLabel(e.target.value)}
              maxLength={60}
              autoComplete="off"
            />
          </label>
          <label>
            <span>Color</span>
            <span className="color-row">
              <input
                type="color"
                value={safeColor(color)}
                onChange={(e) => setColor(e.target.value)}
                aria-label="Account color"
              />
              <input
                value={color}
                onChange={(e) => setColor(e.target.value)}
                maxLength={7}
                autoComplete="off"
                spellCheck={false}
                aria-label="Color hex value"
              />
            </span>
          </label>
          <fieldset className="radio-group">
            <legend>Popup blocking</legend>
            <label className="radio-row">
              <input
                type="radio"
                name="account-popup-policy"
                checked={popupChoice === "inherit"}
                onChange={() => setPopupChoice("inherit")}
              />
              <span>Use app setting (currently {app.settings.popup_policy})</span>
            </label>
            <label className="radio-row">
              <input
                type="radio"
                name="account-popup-policy"
                checked={popupChoice === "allow"}
                onChange={() => setPopupChoice("allow")}
              />
              <span>Allow popups</span>
            </label>
            <label className="radio-row">
              <input
                type="radio"
                name="account-popup-policy"
                checked={popupChoice === "block"}
                onChange={() => setPopupChoice("block")}
              />
              <span>Block popups</span>
            </label>
            <span className="help">
              Close and reopen the account window for this change to take effect.
            </span>
          </fieldset>
          <fieldset className="radio-group">
            <legend>Suspend when idle</legend>
            <label className="radio-row">
              <input
                type="radio"
                name="account-suspend"
                checked={suspendChoice === "inherit"}
                onChange={() => setSuspendChoice("inherit")}
              />
              <span>
                Use app setting (currently {app.settings.auto_suspend_minutes}{" "}
                min)
              </span>
            </label>
            <label className="radio-row">
              <input
                type="radio"
                name="account-suspend"
                checked={suspendChoice === "custom"}
                onChange={() => setSuspendChoice("custom")}
              />
              <span>
                Custom:{" "}
                <input
                  type="number"
                  min={0}
                  step={1}
                  value={suspendCustom}
                  onChange={(e) => setSuspendCustom(e.target.value)}
                  onFocus={() => setSuspendChoice("custom")}
                  disabled={suspendChoice !== "custom"}
                  aria-label="Custom idle suspend minutes"
                  style={{ width: 80 }}
                />{" "}
                min
              </span>
            </label>
            <span className="help">
              Suspended windows stay signed in and wake when focused. 0 =
              never suspend.
            </span>
          </fieldset>
          <fieldset className="radio-group">
            <legend>Close when idle</legend>
            <label className="radio-row">
              <input
                type="radio"
                name="account-close"
                checked={closeChoice === "inherit"}
                onChange={() => setCloseChoice("inherit")}
              />
              <span>
                Use app setting (currently {app.settings.auto_close_minutes}{" "}
                min)
              </span>
            </label>
            <label className="radio-row">
              <input
                type="radio"
                name="account-close"
                checked={closeChoice === "custom"}
                onChange={() => setCloseChoice("custom")}
              />
              <span>
                Custom:{" "}
                <input
                  type="number"
                  min={0}
                  step={1}
                  value={closeCustom}
                  onChange={(e) => setCloseCustom(e.target.value)}
                  onFocus={() => setCloseChoice("custom")}
                  disabled={closeChoice !== "custom"}
                  aria-label="Custom idle close minutes"
                  style={{ width: 80 }}
                />{" "}
                min
              </span>
            </label>
            <span className="help">
              Closed windows free the most memory; reopening restores your
              login. 0 = never close.
            </span>
          </fieldset>
          <fieldset className="radio-group">
            <legend>Ad blocking</legend>
            <label className="radio-row">
              <input
                type="radio"
                name="account-adblock"
                checked={adblockChoice === "inherit"}
                onChange={() => setAdblockChoice("inherit")}
              />
              <span>
                Use app setting (currently{" "}
                {app.settings.adblock_enabled ? "on" : "off"})
              </span>
            </label>
            <label className="radio-row">
              <input
                type="radio"
                name="account-adblock"
                checked={adblockChoice === "on"}
                onChange={() => setAdblockChoice("on")}
              />
              <span>On for this account</span>
            </label>
            <label className="radio-row">
              <input
                type="radio"
                name="account-adblock"
                checked={adblockChoice === "off"}
                onChange={() => setAdblockChoice("off")}
              />
              <span>Off for this account</span>
            </label>
            <span className="help">
              Takes effect right away, even if the account window is open.
            </span>
          </fieldset>
          {formError && (
            <p className="form-error" role="alert">
              {formError}
            </p>
          )}
          <div className="dialog-actions">
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" disabled={saving}>
              {saving ? "Saving…" : "Save"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

function SaveLoginDialog({
  app,
  account,
  onClose,
  onSaved,
}: {
  app: WebApp;
  account: Account;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [existing, setExisting] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  // Load existing (non-secret) metadata so the dialog can show what's saved.
  useEffect(() => {
    let cancelled = false;
    invoke<{ domain: string; username: string } | null>("credential_info", {
      appId: app.id,
      accountId: account.id,
    })
      .then((info) => {
        if (!cancelled && info) {
          setExisting(`${info.username} @ ${info.domain}`);
          setUsername(info.username);
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [app.id, account.id]);

  async function handleSave(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    setSaving(true);
    try {
      await invoke("save_credential", {
        appId: app.id,
        accountId: account.id,
        username,
        password,
      });
      onSaved();
    } catch (err) {
      setFormError(errMsg(err));
    } finally {
      setSaving(false);
    }
  }

  async function handleDelete() {
    setFormError(null);
    try {
      await invoke("delete_credential", {
        appId: app.id,
        accountId: account.id,
      });
      onSaved();
    } catch (err) {
      setFormError(errMsg(err));
    }
  }

  return (
    <div
      className="dialog-overlay"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label={`Save login for ${account.label}`}
    >
      <div className="dialog" onClick={(e) => e.stopPropagation()}>
        <h3>Save login</h3>
        <p className="muted small">
          Stored in your system vault (Windows Credential Manager), never in
          AppMaka's files. Fills only when you click Fill login, and only on
          this site.
        </p>
        {existing && (
          <p className="small">
            Saved login: <strong>{existing}</strong>
          </p>
        )}
        <form onSubmit={(e) => void handleSave(e)}>
          <label>
            <span>Username or email</span>
            <input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              maxLength={512}
              autoComplete="off"
            />
          </label>
          <label>
            <span>Password</span>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              maxLength={4096}
              autoComplete="new-password"
            />
          </label>
          {formError && (
            <p className="form-error" role="alert">
              {formError}
            </p>
          )}
          <div className="dialog-actions">
            {existing && (
              <button
                type="button"
                className="danger"
                onClick={() => void handleDelete()}
              >
                Delete saved login
              </button>
            )}
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" disabled={saving}>
              {saving ? "Saving…" : "Save to vault"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

function AddAccountForm({
  app,
  onAdded,
  onError,
}: {
  app: WebApp;
  onAdded: (account: Account) => void;
  onError: (msg: string) => void;
}) {
  const [label, setLabel] = useState("");
  const [color, setColor] = useState(safeColor(app.color));
  const [adding, setAdding] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  async function handleAdd(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    const clean = label.trim();
    if (!clean) {
      setFormError("Give the account a label.");
      return;
    }
    setAdding(true);
    try {
      const account = await invoke<Account>("add_account", {
        // Tauri exposes Rust snake_case params as camelCase to JS.
        appId: app.id,
        label: clean,
        color: isValidHexColor(color) ? color : null,
      });
      onAdded(account);
      setLabel("");
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
    } finally {
      setAdding(false);
    }
  }

  return (
    <form className="inline-form" onSubmit={(e) => void handleAdd(e)}>
      <label>
        <span>New account label</span>
        <input
          value={label}
          onChange={(e) => setLabel(e.target.value)}
          placeholder="e.g. Work, Personal"
          maxLength={60}
          autoComplete="off"
        />
      </label>
      <label className="color-label">
        <span>Color</span>
        <input
          type="color"
          value={safeColor(color)}
          onChange={(e) => setColor(e.target.value)}
          aria-label="Account color"
        />
      </label>
      <button type="submit" disabled={adding}>
        {adding ? "Adding…" : "Add account"}
      </button>
      {formError && (
        <p className="form-error" role="alert">
          {formError}
        </p>
      )}
    </form>
  );
}

function adblockStateLabel(adblockEnabled: boolean, networkAdblock: boolean): string {
  if (!adblockEnabled) return "Ad blocking: off";
  if (networkAdblock) return "Ad blocking: on — network + cosmetic";
  return "Ad blocking: on — cosmetic only (network blocking is Windows-only)";
}

function AppSettingsForm({
  app,
  networkAdblock,
  onSaved,
  onError,
}: {
  app: WebApp;
  networkAdblock: boolean;
  onSaved: (app: WebApp) => void;
  onError: (msg: string) => void;
}) {
  const settings = app.settings ?? DEFAULT_SETTINGS;
  const [name, setName] = useState(app.name);
  const [url, setUrl] = useState(app.url);
  const [color, setColor] = useState(safeColor(app.color));
  const [popupPolicy, setPopupPolicy] = useState<"block" | "allow">(settings.popup_policy);
  const [allowlist, setAllowlist] = useState(settings.popup_allowlist.join("\n"));
  const [adblockEnabled, setAdblockEnabled] = useState(settings.adblock_enabled);
  const [suspendMinutes, setSuspendMinutes] = useState(String(settings.auto_suspend_minutes));
  const [closeMinutes, setCloseMinutes] = useState(String(settings.auto_close_minutes ?? 30));
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  async function handleSave(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    const cleanName = name.trim();
    if (!cleanName) {
      setFormError("Give the app a name.");
      return;
    }
    const cleanUrl = normalizeUrl(url);
    if (!cleanUrl) {
      setFormError("Enter a valid URL, e.g. https://example.com");
      return;
    }
    const minutes = Math.max(0, parseInt(suspendMinutes, 10) || 0);
    const closeMins = Math.max(0, parseInt(closeMinutes, 10) || 0);
    const newSettings: AppSettings = {
      popup_policy: popupPolicy,
      popup_allowlist: allowlist
        .split("\n")
        .map((s) => s.trim().toLowerCase())
        .filter(Boolean),
      adblock_enabled: adblockEnabled,
      auto_suspend_minutes: minutes,
      auto_close_minutes: closeMins,
    };
    setSaving(true);
    try {
      const updated = await invoke<WebApp>("update_app", {
        appId: app.id,
        name: cleanName,
        url: cleanUrl,
        color: safeColor(color),
      });
      const savedSettings = await invoke<AppSettings>("update_app_settings", {
        id: app.id,
        settings: newSettings,
      });
      onSaved({ ...updated, settings: savedSettings });
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
    } finally {
      setSaving(false);
    }
  }

  return (
    <form className="settings-form" onSubmit={(e) => void handleSave(e)}>
      <h3>Settings</h3>
      <label>
        <span>Name</span>
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          maxLength={80}
          autoComplete="off"
        />
      </label>
      <label>
        <span>URL</span>
        <input
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          inputMode="url"
          autoComplete="off"
        />
      </label>
      <label className="color-label">
        <span>Color</span>
        <input
          type="color"
          value={safeColor(color)}
          onChange={(e) => setColor(e.target.value)}
          aria-label="App color"
        />
      </label>
      <label>
        <span>Popups</span>
        <select
          value={popupPolicy}
          onChange={(e) => setPopupPolicy(e.target.value === "allow" ? "allow" : "block")}
        >
          <option value="block">Block all popups</option>
          <option value="allow">Allow popups as contained windows</option>
        </select>
        <span className="help">
          Close and reopen the account window for this change to take effect.
        </span>
      </label>
      <label>
        <span>Popup allowlist</span>
        <textarea
          value={allowlist}
          onChange={(e) => setAllowlist(e.target.value)}
          rows={3}
          placeholder="accounts.google.com"
          spellCheck={false}
          autoComplete="off"
        />
        <span className="help">
          Sites allowed to open sign-in popups even when blocking, e.g. accounts.google.com.
          One hostname per line.
        </span>
      </label>
      <label className="check-row">
        <input
          type="checkbox"
          checked={adblockEnabled}
          onChange={(e) => setAdblockEnabled(e.target.checked)}
        />
        <span>{adblockStateLabel(adblockEnabled, networkAdblock)}</span>
      </label>
      <label>
        <span>Auto-suspend idle windows (minutes)</span>
        <input
          type="number"
          min={0}
          step={1}
          value={suspendMinutes}
          onChange={(e) => setSuspendMinutes(e.target.value)}
        />
        <span className="help">Idle account windows are suspended to save RAM. 0 = never suspend.</span>
      </label>
      <label>
        <span>Close idle window after (minutes)</span>
        <input
          type="number"
          min={0}
          step={1}
          value={closeMinutes}
          onChange={(e) => setCloseMinutes(e.target.value)}
        />
        <span className="help">Idle account windows are closed to free RAM; their login survives, reopening restores it. 0 = never close.</span>
      </label>
      <div className="form-actions">
        <button type="submit" disabled={saving}>
          {saving ? "Saving…" : "Save settings"}
        </button>
      </div>
      {formError && (
        <p className="form-error" role="alert">
          {formError}
        </p>
      )}
    </form>
  );
}

/**
 * Edit-app dialog: rename, change the URL/color, and manage the logo —
 * upload a custom image, fetch the site's icon, or remove it.
 */
function EditAppDialog({
  app,
  onClose,
  onSaved,
}: {
  app: WebApp;
  onClose: () => void;
  onSaved: (app: WebApp) => void;
}) {
  const [name, setName] = useState(app.name);
  const [url, setUrl] = useState(app.url);
  const [color, setColor] = useState(safeColor(app.color));
  const [icon, setIcon] = useState<string | null>(app.icon);
  const [saving, setSaving] = useState(false);
  const [working, setWorking] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  // Escape closes the dialog.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  async function handleSave(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    const cleanName = name.trim();
    if (!cleanName) {
      setFormError("Give the app a name.");
      return;
    }
    if (!isValidHexColor(color)) {
      setFormError("Color must look like #6366f1.");
      return;
    }
    const cleanUrl = normalizeUrl(url);
    if (!cleanUrl) {
      setFormError("Enter a valid URL, e.g. https://example.com");
      return;
    }
    setSaving(true);
    try {
      const updated = await invoke<WebApp>("update_app", {
        appId: app.id,
        name: cleanName,
        url: cleanUrl,
        color,
      });
      onSaved(updated);
    } catch (err) {
      setFormError(errMsg(err));
    } finally {
      setSaving(false);
    }
  }

  async function handleFile(file: File) {
    setFormError(null);
    setWorking(true);
    try {
      const buf = new Uint8Array(await file.arrayBuffer());
      const ext = (file.name.split(".").pop() ?? "").toLowerCase();
      const path = await invoke<string>("set_app_icon_data", {
        appId: app.id,
        data: Array.from(buf),
        ext,
      });
      setIcon(path);
    } catch (err) {
      setFormError(errMsg(err));
    } finally {
      setWorking(false);
      if (fileRef.current) fileRef.current.value = "";
    }
  }

  async function handleFetchIcon() {
    setFormError(null);
    setWorking(true);
    try {
      const path = await invoke<string | null>("fetch_favicon", {
        appId: app.id,
      });
      if (path) {
        setIcon(path);
      } else {
        setFormError("Couldn't find a logo on that site.");
      }
    } catch (err) {
      setFormError(errMsg(err));
    } finally {
      setWorking(false);
    }
  }

  async function handleRemoveIcon() {
    setFormError(null);
    setWorking(true);
    try {
      await invoke("clear_app_icon", { appId: app.id });
      setIcon(null);
    } catch (err) {
      setFormError(errMsg(err));
    } finally {
      setWorking(false);
    }
  }

  return (
    <div
      className="dialog-overlay"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label={`Edit ${app.name}`}
    >
      <div className="dialog" onClick={(e) => e.stopPropagation()}>
        <h3>Edit app</h3>
        <form onSubmit={(e) => void handleSave(e)}>
          <label>
            <span>Name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              maxLength={80}
              autoComplete="off"
            />
          </label>
          <label>
            <span>Website URL</span>
            <input
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              inputMode="url"
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          <label>
            <span>Color</span>
            <span className="color-row">
              <input
                type="color"
                value={safeColor(color)}
                onChange={(e) => setColor(e.target.value)}
                aria-label="App color"
              />
              <input
                value={color}
                onChange={(e) => setColor(e.target.value)}
                maxLength={7}
                autoComplete="off"
                spellCheck={false}
                aria-label="Color hex value"
              />
            </span>
          </label>
          <div className="logo-section">
            <span className="field-label">Logo</span>
            <div className="logo-row">
              <span className="logo-preview">
                <AppIcon app={{ ...app, icon }} />
              </span>
              <div className="logo-actions">
                <input
                  ref={fileRef}
                  type="file"
                  accept="image/png,image/jpeg,image/gif,image/webp,.ico"
                  onChange={(e) => {
                    const f = e.target.files?.[0];
                    if (f) void handleFile(f);
                  }}
                  aria-label="Upload a logo"
                />
                <div className="logo-buttons">
                  <button
                    type="button"
                    onClick={() => void handleFetchIcon()}
                    disabled={working}
                  >
                    {working ? "Working…" : "Fetch from site"}
                  </button>
                  {icon && (
                    <button
                      type="button"
                      className="danger"
                      onClick={() => void handleRemoveIcon()}
                      disabled={working}
                    >
                      Remove logo
                    </button>
                  )}
                </div>
              </div>
            </div>
            <span className="help">
              PNG, JPEG, GIF, WebP or ICO, up to 2 MiB. Without a logo the
              tile shows the site's icon, then a letter.
            </span>
          </div>
          {formError && (
            <p className="form-error" role="alert">
              {formError}
            </p>
          )}
          <div className="dialog-actions">
            <button type="button" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" disabled={saving}>
              {saving ? "Saving…" : "Save"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

/**
 * The rest of the launcher preferences: which monitor the launcher
 * summons on, automatic update checks, and the hidden-programs list.
 * Rendered under LauncherSettingsPanel in the library's Launcher section
 * (Launcher.tsx stays untouched).
 *
 * ID shapes, kept straight: `hidden_programs` holds RAW program ids
 * (e.g. `ab12cd`), passed to set_program_hidden as `programId`. Pins and
 * usage use tagged ids (`app:<id>`, `account:<id>`, `program:<id>`).
 */
function LauncherExtras({
  settings,
  programs,
  onSaved,
  onProgramsRefreshed,
  onError,
}: {
  settings: LauncherSettings;
  programs: NativeProgram[];
  onSaved: (s: LauncherSettings) => void;
  onProgramsRefreshed: (programs: NativeProgram[]) => void;
  onError: (msg: string) => void;
}) {
  const [formError, setFormError] = useState<string | null>(null);
  const [unhiding, setUnhiding] = useState<string | null>(null);

  // Every mutation refreshes from the command's returned settings object.
  async function mutate(run: () => Promise<LauncherSettings>, label: string) {
    setFormError(null);
    try {
      onSaved(await run());
    } catch (err) {
      const msg = `${label}: ${errMsg(err)}`;
      setFormError(msg);
      onError(msg);
    }
  }

  function handleMonitorMode(mode: "cursor" | "primary") {
    if (mode === (settings.monitor_mode ?? "cursor")) return;
    void mutate(
      () => invoke<LauncherSettings>("set_monitor_mode", { mode }),
      "Could not change the monitor"
    );
  }

  function handleAutoUpdateCheck(enabled: boolean) {
    void mutate(
      () => invoke<LauncherSettings>("set_auto_update_check", { enabled }),
      "Could not change update checks"
    );
  }

  async function handleUnhide(programId: string) {
    setUnhiding(programId);
    try {
      await mutate(
        () =>
          invoke<LauncherSettings>("set_program_hidden", {
            programId,
            hidden: false,
          }),
        "Could not unhide the program"
      );
      // The id is visible again — pull the fresh program list so its tile returns.
      const list = await invoke<NativeProgram[]>("list_programs");
      onProgramsRefreshed(list);
    } catch (err) {
      onError(errMsg(err));
    } finally {
      setUnhiding(null);
    }
  }

  const hidden = settings.hidden_programs ?? [];
  const monitorMode = settings.monitor_mode ?? "cursor";
  // Collapse toggle for the hidden-programs list. `null` = untouched this
  // session; the persisted pref wins, and with neither set the list starts
  // collapsed whenever it is non-empty (it only renders when non-empty).
  const [collapsedChoice, setCollapsedChoice] = useState<boolean | null>(null);
  const hiddenCollapsed =
    collapsedChoice ?? settings.hidden_section_collapsed ?? hidden.length > 0;

  async function handleToggleHiddenCollapsed() {
    const next = !hiddenCollapsed;
    setCollapsedChoice(next);
    try {
      onSaved(
        await invoke<LauncherSettings>("set_hidden_section_collapsed", {
          collapsed: next,
        })
      );
    } catch (err) {
      const msg = `Could not save the list state: ${errMsg(err)}`;
      setFormError(msg);
      onError(msg);
    }
  }

  return (
    <div className="launcher-extras">
      <fieldset className="radio-group">
        <legend>Open on</legend>
        <label className="radio-row">
          <input
            type="radio"
            name="monitor-mode"
            checked={monitorMode === "cursor"}
            onChange={() => handleMonitorMode("cursor")}
          />
          <span>Monitor with cursor</span>
        </label>
        <label className="radio-row">
          <input
            type="radio"
            name="monitor-mode"
            checked={monitorMode === "primary"}
            onChange={() => handleMonitorMode("primary")}
          />
          <span>Primary monitor</span>
        </label>
      </fieldset>

      <label className="check-row">
        <input
          type="checkbox"
          checked={settings.auto_update_check ?? false}
          onChange={(e) => handleAutoUpdateCheck(e.target.checked)}
        />
        <span>Check for updates automatically</span>
      </label>
      <span className="help">
        Checks quietly in the background. Never downloads or restarts without you.
      </span>

      {hidden.length > 0 && (
        <div className="hidden-programs">
          <button
            type="button"
            className="collapse-head"
            onClick={() => void handleToggleHiddenCollapsed()}
            aria-expanded={!hiddenCollapsed}
          >
            <span className="field-label">
              Hidden programs ({hidden.length})
            </span>
            <span className="collapse-chevron" aria-hidden="true">
              {hiddenCollapsed ? "▸" : "▾"}
            </span>
          </button>
          {!hiddenCollapsed && (
          <ul className="hidden-list">
            {hidden.map((id) => {
              const name = programs.find((p) => p.id === id)?.name;
              return (
                <li key={id} className="hidden-row">
                  <span className="hidden-name" title={id}>
                    {name ?? `Hidden program (${id.slice(0, 8)}…)`}
                  </span>
                  <button
                    className="text-button"
                    disabled={unhiding === id}
                    onClick={() => void handleUnhide(id)}
                  >
                    {unhiding === id ? "Unhiding…" : "Unhide"}
                  </button>
                </li>
              );
            })}
          </ul>
          )}
        </div>
      )}

      {formError && (
        <p className="form-error" role="alert">
          {formError}
        </p>
      )}
    </div>
  );
}

/**
 * First-run 101: four short, honest cards over the library. Dismissed once
 * and never nagged again; the header "?" reopens it anytime.
 */
function IntroOverlay({ onGotIt }: { onGotIt: () => void }) {
  const cards = [
    {
      title: "Open it anywhere",
      body: "Press Alt+Space from anywhere — the launcher pops up over your work. Esc hides it again.",
    },
    {
      title: "Websites become apps",
      body: "Paste a URL and it joins your library, each with its own isolated accounts and logins.",
    },
    {
      title: "Sign in safely",
      body: "\u201CPreview & sign in\u201D opens the real site in a throwaway window. You sign in there yourself — AppMaka never sees your password.",
    },
    {
      title: "Tidy the launcher",
      body: "Right-click any tile to pin it to the top, hide it, or edit it. Unhide hidden programs in the Launcher settings below.",
    },
    {
      title: "Launcher shortcuts",
      body: "Type =2+2 to calculate, >lock to lock your PC (destructive commands ask first), ?cats for a web search. Paste any URL to add it as an app.",
    },
    {
      title: "Routines open your whole morning at once",
      body: "A routine opens a set of apps and accounts with one click or one keystroke. Set them up in the Routines section below. For example, 'Morning' can open your work email, your main chat account, and a dashboard.",
    },
  ];
  return (
    <div
      className="dialog-overlay"
      role="dialog"
      aria-modal="true"
      aria-label="Welcome to AppMaka"
    >
      <div className="dialog intro-dialog">
        <h3>The 30-second tour</h3>
        <div className="intro-cards">
          {cards.map((card, i) => (
            <div key={card.title} className="intro-card">
              <h4>
                <span className="step" aria-hidden="true">
                  {i + 1}
                </span>
                {card.title}
              </h4>
              <p>{card.body}</p>
            </div>
          ))}
        </div>
        <div className="dialog-actions">
          <button onClick={onGotIt}>Got it</button>
        </div>
      </div>
    </div>
  );
}

type UpdateStatus =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "uptodate" }
  | { kind: "none" }
  | { kind: "available"; version: string }
  | { kind: "downloading"; version: string; progress: number | null }
  | { kind: "ready"; version: string }
  | { kind: "launched"; version: string }
  | { kind: "failed"; message: string };

/**
 * "AppMaka vX.Y.Z", read from the running binary at runtime — never
 * hardcoded. Falls back to just "AppMaka" when the read fails.
 */
function VersionLine() {
  const [version, setVersion] = useState<string | null>(null);

  useEffect(() => {
    getVersion()
      .then(setVersion)
      .catch(() => setVersion(null));
  }, []);

  return (
    <p className="muted small">{version ? `AppMaka v${version}` : "AppMaka"}</p>
  );
}

/**
 * Self-update UI. Manual "Check for updates" → download with progress →
 * installer handoff (never force-restarted). A failed check says
 * "Couldn't reach the update server." — never a silent "no updates" and
 * never an error popup. Download/install failures stay visible as a
 * plain-language message — never a silent return to the button.
 *
 * v0.8.2: on Windows the install type is detected at runtime. MSI installs
 * (Program Files) download the matching MSI, verify its signature, and hand
 * it to the Windows installer; everything else keeps the NSIS
 * downloadAndInstall flow. On Windows the post-download copy is
 * "Installer launched — follow its steps." (the restart button is Linux-only).
 *
 * The automatic check also lives here: once on mount when `autoCheck` is
 * on, then every 24h (skipped while the window is hidden). A found update
 * lands in the same "available" state as a manual check — nothing is
 * silently swallowed. Auto-check failures show nothing at all.
 */
function UpdaterSection({
  autoCheck,
  isWindows,
}: {
  autoCheck: boolean;
  isWindows: boolean;
}) {
  const [status, setStatus] = useState<UpdateStatus>({ kind: "idle" });
  const pending = useRef<Update | null>(null);
  const downloadedBytes = useRef(0);
  const totalBytes = useRef<number | null>(null);
  const autoRan = useRef(false);

  async function adoptFoundUpdate() {
    try {
      const update = await check({ timeout: 30_000 });
      if (update) {
        pending.current = update;
        setStatus({ kind: "available", version: update.version });
      }
    } catch {
      /* auto-check failures stay silent */
    }
  }

  useEffect(() => {
    if (!autoCheck) return;
    // Once per session: StrictMode double-mounts in dev, so guard with a ref.
    if (!autoRan.current) {
      autoRan.current = true;
      void adoptFoundUpdate();
    }
    const id = window.setInterval(() => {
      if (isAppHidden()) return;
      void adoptFoundUpdate();
    }, 24 * 60 * 60 * 1000);
    return () => window.clearInterval(id);
  }, [autoCheck]);

  async function handleCheck() {
    setStatus({ kind: "checking" });
    pending.current = null;
    try {
      const update = await check({ timeout: 30_000 });
      if (!update) {
        setStatus({ kind: "uptodate" });
      } else {
        pending.current = update;
        setStatus({ kind: "available", version: update.version });
      }
    } catch {
      setStatus({ kind: "none" });
    }
  }

  async function handleDownload() {
    const update = pending.current;
    if (!update) return;
    downloadedBytes.current = 0;
    totalBytes.current = null;
    setStatus({ kind: "downloading", version: update.version, progress: null });
    try {
      // v0.8.2: MSI-installed copies must get the MSI, not the NSIS setup.exe
      // that latest.json points at — otherwise the update never takes.
      const installType = await invoke<string>("get_install_type");
      if (installType === "msi" && isWindows) {
        await handleMsiDownload(update);
      } else {
        await update.downloadAndInstall((event) => {
          if (event.event === "Started") {
            totalBytes.current = event.data.contentLength ?? null;
          } else if (event.event === "Progress") {
            downloadedBytes.current += event.data.chunkLength;
            const total = totalBytes.current;
            const progress = total
              ? Math.min(99, Math.round((downloadedBytes.current / total) * 100))
              : null;
            setStatus((prev) =>
              prev.kind === "downloading" ? { ...prev, progress } : prev
            );
          }
        });
      }
      setStatus({ kind: "ready", version: update.version });
    } catch (e) {
      // Plain-language, and it stays visible — never a silent return to the
      // "Download & install" button.
      setStatus({ kind: "failed", message: plainUpdateError(e) });
    }
  }

  /**
   * v0.8.2 MSI path: download the matching MSI (derived from the NSIS URL in
   * latest.json), verify its signature, and hand it to the Windows installer.
   * The backend emits progress events and exits into the installer wizard.
   */
  async function handleMsiDownload(update: Update) {
    const platforms = (update.rawJson as { platforms?: Record<string, { url?: string }> })
      .platforms;
    const nsisUrl = platforms?.["windows-x86_64"]?.url;
    if (!nsisUrl) {
      throw new Error("Couldn't find the update download link.");
    }
    const unlistenProgress = await listen<{ downloaded: number; total: number | null }>(
      "msi-update-progress",
      (event) => {
        downloadedBytes.current = event.payload.downloaded;
        totalBytes.current = event.payload.total;
        const total = event.payload.total;
        const progress = total
          ? Math.min(99, Math.round((event.payload.downloaded / total) * 100))
          : null;
        // v0.8.3: the backend exits into the installer the moment the
        // download is complete and verified, so once the bytes add up the
        // installer is effectively launched — say so instead of sticking
        // at 99%. (The msi-update-launched event below is a backup; it may
        // not arrive before the app exits.)
        if (total !== null && event.payload.downloaded >= total) {
          setStatus({ kind: "launched", version: update.version });
        } else {
          setStatus((prev) =>
            prev.kind === "downloading" ? { ...prev, progress } : prev
          );
        }
      }
    );
    const unlistenLaunched = await listen("msi-update-launched", () => {
      setStatus({ kind: "launched", version: update.version });
    });
    try {
      // The backend exits the app after launching the installer, so this
      // only returns when something went wrong (thrown as a plain message).
      await invoke("install_msi_update", { nsisUrl, version: update.version });
      setStatus({ kind: "launched", version: update.version });
    } finally {
      unlistenProgress();
      unlistenLaunched();
    }
  }

  /** Whatever the updater threw, turn it into one plain sentence. */
  function plainUpdateError(e: unknown): string {
    if (typeof e === "string" && e.trim()) return e;
    if (
      e !== null &&
      typeof e === "object" &&
      "message" in e &&
      typeof (e as { message: unknown }).message === "string" &&
      ((e as { message: string }).message.trim())
    ) {
      return (e as { message: string }).message;
    }
    return "The update couldn't be downloaded or installed.";
  }

  async function handleRestart() {
    try {
      await relaunch();
    } catch {
      /* the app is going away anyway — nothing honest left to say */
    }
  }

  const busy = status.kind === "checking" || status.kind === "downloading";

  return (
    <div className="updater">
      <div className="updater-row">
        <button onClick={() => void handleCheck()} disabled={busy}>
          {status.kind === "checking" ? "Checking…" : "Check for updates"}
        </button>
        <span className="muted small" role="status">
          {status.kind === "idle" && "Never checked this session."}
          {status.kind === "checking" && "Checking…"}
          {status.kind === "uptodate" && "You're up to date."}
          {status.kind === "none" && "Couldn't reach the update server."}
          {status.kind === "available" &&
            `Version ${status.version} is available.`}
          {status.kind === "downloading" &&
            (status.progress === null
              ? `Downloading ${status.version}…`
              : `Downloading ${status.version}… ${status.progress}%`)}
          {status.kind === "ready" &&
            (isWindows
              ? "Installer launched — follow its steps."
              : `${status.version} installed — restart to finish.`)}
          {status.kind === "launched" && "Installer launched — follow its steps."}
          {status.kind === "failed" && status.message}
        </span>
      </div>
      {status.kind === "available" && (
        <div>
          <button onClick={() => void handleDownload()}>
            Download &amp; install {status.version}
          </button>
        </div>
      )}
      {status.kind === "downloading" && status.progress !== null && (
        <div
          className="progress-track"
          role="progressbar"
          aria-valuenow={status.progress}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-label="Download progress"
        >
          <div
            className="progress-fill"
            style={{ width: `${status.progress}%` }}
          />
        </div>
      )}
      {status.kind === "ready" && !isWindows && (
        <div>
          <button onClick={() => void handleRestart()}>
            Restart to finish
          </button>
        </div>
      )}
      <p className="muted small">
        Updates never download or restart on their own — you stay in charge.
      </p>
    </div>
  );
}

export default function App() {
  return (
    <DownloadsProvider>
      <AppShell />
    </DownloadsProvider>
  );
}

// v0.9.8: the whole app lives inside DownloadsProvider so download
// progress keeps flowing when the downloads page is closed.
function AppShell() {
  const [apps, setApps] = useState<WebApp[]>([]);
  const [routines, setRoutines] = useState<Routine[]>([]);
  const [platform, setPlatform] = useState<PlatformInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // v0.9.8: quiet "Download finished" notice, toggleable in Downloads.
  const { lastCompleted, settings: downloadSettings } = useDownloads();
  useEffect(() => {
    if (
      lastCompleted &&
      downloadSettings?.showCompletionNotice !== false
    ) {
      setNotice(`Download finished: ${lastCompleted.filename}`);
    }
  }, [lastCompleted, downloadSettings]);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [settingsOpen, setSettingsOpen] = useState<Set<string>>(new Set());
  const [editingApp, setEditingApp] = useState<WebApp | null>(null);
  const [editingAccount, setEditingAccount] = useState<{
    app: WebApp;
    account: Account;
  } | null>(null);
  // v0.7.0: "Forget this login" confirm dialog target (tile menu / account row).
  const [forgetLoginTarget, setForgetLoginTarget] = useState<{
    app: WebApp;
    account: Account;
  } | null>(null);
  // Password vault: "Save login" dialog target.
  const [saveLoginTarget, setSaveLoginTarget] = useState<{
    app: WebApp;
    account: Account;
  } | null>(null);
  // v0.7.0: link-dispatcher picker target (URL with no matching rule).
  const [linkPickerUrl, setLinkPickerUrl] = useState<string | null>(null);
  // v0.7.0: RAM dashboard visibility.
  const [ramOpen, setRamOpen] = useState(false);

  // Launcher: hotkey-summoned search over apps, accounts, and programs.
  const [programs, setPrograms] = useState<NativeProgram[]>([]);
  const [launcherSettings, setLauncherSettings] = useState<LauncherSettings | null>(null);
  const [launcherPanelOpen, setLauncherPanelOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  // v0.8.0 workspaces: owned here so the launcher filter and the library
  // section share it. Refreshed on `appmaka:workspace-changed` (fired by
  // set_active_workspace, including workspace hotkey presses).
  const [workspaceList, setWorkspaceList] = useState<WorkspaceList | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);

  // v0.11.0: copyable error dialogs. The backend emits `appmaka:error`
  // for failures with no other surface (a popup that wouldn't open, a
  // restore window that died). The latest error wins; the ring buffer
  // keeps the full history for Copy diagnostics.
  const [errorEntry, setErrorEntry] = useState<ErrorEntry | null>(null);
  useEffect(() => {
    let off: (() => void) | undefined;
    listen<ErrorEntry>("appmaka:error", (event) => setErrorEntry(event.payload))
      .then((unlisten) => {
        off = unlisten;
      })
      .catch(() => {});
    return () => off?.();
  }, []);

  // Refresh the program list whenever a scan finishes (startup scan or a
  // manual rescan), instead of only the one ~8s re-poll below.
  useProgramsScannedRefresh(setPrograms);

  // The launcher overlay asks the library view to open the Edit dialog for
  // an app (right-click → Edit). The dialog itself is App.tsx state, so we
  // pick up the event here.
  useEffect(() => {
    const onEditApp = (e: Event) => {
      const id = (e as CustomEvent).detail?.appId as string | undefined;
      const app = apps.find((a) => a.id === id);
      if (app) setEditingApp(app);
    };
    window.addEventListener(EDIT_APP_EVENT, onEditApp);
    return () => window.removeEventListener(EDIT_APP_EVENT, onEditApp);
  }, [apps]);

  // First-run 101 overlay: shows over the library until dismissed once,
  // reopenable anytime with the header "?". Never nags after dismissal.
  const [introOpen, setIntroOpen] = useState(false);
  const showIntro =
    introOpen || (launcherSettings !== null && !launcherSettings.seen_intro);

  async function dismissIntro() {
    // Never nag: even if the backend write fails, treat it as seen locally.
    try {
      const updated = await invoke<LauncherSettings>("set_seen_intro", {
        seen: true,
      });
      setLauncherSettings(updated);
    } catch {
      setLauncherSettings((prev) =>
        prev ? { ...prev, seen_intro: true } : prev
      );
    }
    setIntroOpen(false);
  }

  // The automatic update check lives inside UpdaterSection now (single place,
  // no silent double-check).

  // Two views: the hotkey-summoned spotlight overlay ("launcher") and the
  // full management window ("library"). The hotkey always lands on the
  // launcher; the tray menu and the in-overlay button open the library.
  // v0.9.8: "downloads" is the full-page downloads view (Ctrl+J).
  const [view, setView] = useState<"launcher" | "library" | "downloads">(
    "launcher"
  );
  // Bumped on every hotkey summon so the panel entrance animation replays
  // (the React tree stays mounted while the window just hides/shows).
  const [summonCount, setSummonCount] = useState(0);

  // v0.9.5: "Ask me" session-restore offer — one-time per launch, enforced
  // by the backend, so it's safe to ask on mount and on every summon.
  const [restoreOffer, setRestoreOffer] = useState<SessionRestoreOffer | null>(
    null
  );
  const [restoring, setRestoring] = useState(false);

  async function checkRestoreOffer() {
    try {
      const offer = await invoke<SessionRestoreOffer | null>(
        "get_pending_session_restore"
      );
      if (offer) setRestoreOffer(offer);
    } catch {
      /* older backend without the command; no banner */
    }
  }

  // v0.9.9: "Don't close this window" — which account windows are open and
  // which are pinned, refreshed on mount and every summon so the tile-menu
  // checkmarks are current. Labels are `acct-<appId>-<accountId>`.
  const [openLabels, setOpenLabels] = useState<Set<string>>(new Set());
  const [pinnedLabels, setPinnedLabels] = useState<Set<string>>(new Set());
  const refreshWindowPins = useCallback(async () => {
    try {
      const wins = await invoke<OpenAccountWindow[]>(
        "list_open_account_windows"
      );
      setOpenLabels(new Set(wins.map((w) => w.label)));
      setPinnedLabels(
        new Set(wins.filter((w) => w.pinned).map((w) => w.label))
      );
    } catch {
      /* older backend without the command; the menu entry stays hidden */
    }
  }, []);

  async function handleToggleWindowPin(label: string) {
    const next = !pinnedLabels.has(label);
    try {
      await invoke("set_window_pinned", { label, pinned: next });
      await refreshWindowPins();
    } catch (err) {
      setError(errMsg(err));
    }
  }

  // v0.9.10: tile-menu "Close window" — the backend closes the exact
  // label; pinned windows get the standard confirm there.
  async function handleCloseWindow(label: string) {
    try {
      await invoke("close_open_window", { label });
      await refreshWindowPins();
    } catch (err) {
      setError(errMsg(err));
    }
  }

  async function handleRestoreSession() {
    if (restoring) return;
    setRestoring(true);
    setRestoreOffer(null);
    try {
      const n = await invoke<number>("restore_session");
      if (n === 0) setNotice("Nothing saved from last time.");
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setRestoring(false);
    }
  }

  async function handleDismissRestore() {
    setRestoreOffer(null);
    try {
      await invoke("dismiss_session_restore");
    } catch {
      /* already hidden locally */
    }
  }

  // v0.10.0: one obvious entry point for tabbed windows. Opens an empty
  // tabbed window (about:blank); the user adds apps via the + picker.
  // Spotlight behavior: a successful open dismisses the overlay.
  const [openingTabs, setOpeningTabs] = useState(false);
  async function handleOpenTabbedWindow() {
    if (openingTabs) return;
    setOpeningTabs(true);
    setError(null);
    try {
      await invoke("open_tabbed_window", {});
      setQuery("");
      await invoke("hide_library");
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setOpeningTabs(false);
    }
  }

  useEffect(() => {
    void checkRestoreOffer();
    void refreshWindowPins();
    let off: (() => void) | undefined;
    listen("appmaka:show-launcher", () => {
      void checkRestoreOffer();
      void refreshWindowPins();
    })
      .then((f) => {
        off = f;
      })
      .catch(() => {});
    return () => off?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // v0.9.11: transient backend notices (e.g. "This site is already in your
  // applications." from the popup system-menu / Ctrl+Shift+A flows, which
  // have no invoking frontend). Reuses the auto-clearing banner.
  useEffect(() => {
    let off: (() => void) | undefined;
    listen("appmaka:notice", (e) => {
      const msg = (e.payload as { message?: string } | null)?.message;
      if (typeof msg === "string" && msg.length > 0) setNotice(msg);
    })
      .then((f) => {
        off = f;
      })
      .catch(() => {});
    return () => off?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Add-app form
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [color, setColor] = useState(DEFAULT_COLOR);
  const [adding, setAdding] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [list, info, progList, launchSettings, routineList] =
        await Promise.all([
          invoke<WebApp[]>("list_apps"),
          invoke<PlatformInfo>("platform_info"),
          invoke<NativeProgram[]>("list_programs"),
          invoke<LauncherSettings>("get_launcher_settings"),
          invoke<Routine[]>("list_routines"),
        ]);
      setApps(list.map((a) => ({ ...a, accounts: a.accounts ?? [], settings: a.settings ?? DEFAULT_SETTINGS })));
      setPlatform(info);
      setPrograms(progList);
      setLauncherSettings(launchSettings);
      setRoutines(routineList);
      // Backfill logos for apps added before icon caching existed (or where
      // the fetch failed last time). Best-effort, in the background.
      for (const a of list) {
        if (!a.icon) void refreshAppIcon(a.id);
      }
    } catch (err) {
      setError(errMsg(err) === "Something went wrong." ? "Could not load your apps." : errMsg(err));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The program scan runs in the background at startup; pick up its results
  // a few seconds later so search finds everything.
  useEffect(() => {
    const t = setTimeout(() => {
      invoke<NativeProgram[]>("list_programs")
        .then(setPrograms)
        .catch(() => {});
    }, 8000);
    return () => clearTimeout(t);
  }, []);

  // Notices ("already in your library") are transient — clear after a while
  // so a stale one can't confuse a later action.
  useEffect(() => {
    if (!notice) return;
    const t = setTimeout(() => setNotice(null), 9000);
    return () => clearTimeout(t);
  }, [notice]);

  // Focus the search box every time the window is summoned.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        if (focused) {
          searchRef.current?.focus();
          searchRef.current?.select();
        }
      })
      .then((u) => {
        unlisten = u;
      })
      .catch(() => {});
    return () => unlisten?.();
  }, []);

  // The panel remounts on every summon (key bump replays the entrance
  // animation) — focus the fresh input after each remount, since the
  // window-focus event can race the remount.
  useEffect(() => {
    searchRef.current?.focus();
    searchRef.current?.select();
  }, [summonCount]);

  // v0.8.0 workspaces: load once; the backend emits
  // `appmaka:workspace-changed` whenever the active workspace changes
  // (including via a workspace hotkey while the library section is
  // unmounted). Lives here — not in WorkspacesSection — so the launcher
  // filter updates in every view.
  useEffect(() => {
    let alive = true;
    const refresh = () => {
      invoke<WorkspaceList>("list_workspaces")
        .then((l) => {
          if (alive) setWorkspaceList(l);
        })
        .catch((err) => {
          if (alive) setError(`Could not load workspaces: ${errMsg(err)}`);
        });
    };
    refresh();
    let off: (() => void) | undefined;
    listen("appmaka:workspace-changed", refresh)
      .then((u) => {
        off = u;
      })
      .catch(() => {});
    return () => {
      alive = false;
      off?.();
    };
  }, []);

  // v0.8.0: dispatch global hotkey presses (routines / workspaces /
  // per-command hotkeys) to the existing invoke commands.
  useHotkeyDispatch(apps, setError);

  // View-switch events from the backend: tray "Show library" and the hotkey
  // summon (which always resets to the spotlight view).
  // v0.9.8: Ctrl+J opens the downloads page (standard everywhere).
  useEffect(() => {
    let offLibrary: (() => void) | undefined;
    let offLauncher: (() => void) | undefined;
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "j") {
        e.preventDefault();
        setView("downloads");
      }
    };
    window.addEventListener("keydown", onKey);
    listen("appmaka:show-library", () => setView("library"))
      .then((off) => {
        offLibrary = off;
      })
      .catch(() => {});
    listen("appmaka:show-launcher", () => {
      setView("launcher");
      setSummonCount((c) => c + 1);
    })
      .then((off) => {
        offLauncher = off;
      })
      .catch(() => {});
    return () => {
      offLibrary?.();
      offLauncher?.();
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  // Link dispatcher (v0.7.0): an incoming https URL with no matching
  // domain→account rule opens the small picker instead of going nowhere.
  useEffect(() => {
    let off: (() => void) | undefined;
    listen<{ url: string }>("appmaka:link-no-rule", (event) => {
      setLinkPickerUrl(event.payload.url);
    })
      .then((u) => {
        off = u;
      })
      .catch(() => {});
    return () => off?.();
  }, []);

  // A preview that was added as an app: pick it up in the library and open
  // the account that adopted the signed-in session. When the site was
  // already in the library, no duplicate app is created — the session
  // becomes a new account on the existing app instead.
  useEffect(() => {
    let off: (() => void) | undefined;
    listen<PreviewAddOutcome>("appmaka:preview-added", (event) => {
      const created = {
        ...event.payload.app,
        accounts: event.payload.app.accounts ?? [],
        settings: event.payload.app.settings ?? DEFAULT_SETTINGS,
      };
      // The account carrying the fresh session: the backend names it, with
      // the newest account on the card as fallback.
      const adopted =
        event.payload.addedAccount ??
        created.accounts[created.accounts.length - 1];
      if (!event.payload.created) {
        setApps((prev) => prev.map((a) => (a.id === created.id ? created : a)));
        setExpanded((prev) => {
          const next = new Set(prev);
          next.add(created.id);
          return next;
        });
        setError(null);
        setNotice(`Added as another account under "${created.name}".`);
        if (adopted) {
          invoke("open_account", { appId: created.id, accountId: adopted.id }).catch(
            (err) => setError(`Could not open "${created.name}". ${errMsg(err)}`)
          );
        }
        return;
      }
      setApps((prev) => [...prev, created]);
      // Expand the new card so the rename affordance and the new account
      // are visible right away.
      setExpanded((prev) => {
        const next = new Set(prev);
        next.add(created.id);
        return next;
      });
      if (adopted) {
        setError(null);
        invoke("open_account", { appId: created.id, accountId: adopted.id }).catch(
          (err) => setError(`Could not open "${created.name}". ${errMsg(err)}`)
        );
      }
      // The new tile gets its logo in the background.
      void refreshAppIcon(created.id);
    })
      .then((unlisten) => {
        off = unlisten;
      })
      .catch(() => {});
    return () => off?.();
  }, []);

  // Window chrome per view: the launcher is a small frameless spotlight
  // overlay; the library is a full window. Best-effort — if the window
  // manager refuses, the window still works.
  useEffect(() => {
    const win = getCurrentWindow();
    // The main window is transparent-capable (tauri.conf.json). The
    // launcher overlay must leave the canvas unpainted so the desktop shows
    // through the frosted panel; the library view keeps the normal opaque
    // background from styles.css.
    document.documentElement.style.background =
      view === "launcher" ? "transparent" : "";
    void (async () => {
      try {
        if (view === "launcher") {
          await win.setSize(new LogicalSize(680, 480));
          await win.setDecorations(false);
        } else {
          await win.setSize(new LogicalSize(1020, 720));
          await win.setDecorations(true);
        }
        await win.center();
      } catch {
        /* non-fatal */
      }
    })();
  }, [view]);

  function toggleExpanded(id: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function toggleSettings(id: string) {
    setSettingsOpen((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function replaceApp(updated: WebApp) {
    setApps((prev) => prev.map((a) => (a.id === updated.id ? { ...updated, accounts: updated.accounts ?? [], settings: updated.settings ?? DEFAULT_SETTINGS } : a)));
  }

  /**
   * Reveal an already-existing app instead of creating a duplicate: make
   * sure it's in the list, expand its card, and say what's happening.
   */
  function revealApp(app: WebApp, message: string) {
    const full = { ...app, accounts: app.accounts ?? [], settings: app.settings ?? DEFAULT_SETTINGS };
    setApps((prev) => (prev.some((a) => a.id === full.id) ? prev : [...prev, full]));
    setExpanded((prev) => {
      const next = new Set(prev);
      next.add(full.id);
      return next;
    });
    setError(null);
    setNotice(message);
  }

  async function handleAdd(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    const cleanName = name.trim();
    if (!cleanName) {
      setFormError("Give the app a name.");
      return;
    }
    const cleanUrl = normalizeUrl(url);
    if (!cleanUrl) {
      setFormError("Enter a valid URL, e.g. https://example.com");
      return;
    }
    setAdding(true);
    try {
      const outcome = await invoke<AddAppOutcome>("add_app", {
        name: cleanName,
        url: cleanUrl,
      });
      if (!outcome.created) {
        revealApp(outcome.app, `"${outcome.app.name}" is already in your library — showing it instead of adding a duplicate.`);
      } else {
        const created = outcome.app;
        setNotice(null);
        setApps((prev) => [
          ...prev,
          { ...created, accounts: created.accounts ?? [], settings: created.settings ?? DEFAULT_SETTINGS },
        ]);
      }
      setName("");
      setUrl("");
      setColor(DEFAULT_COLOR);
    } catch (err) {
      setFormError(errMsg(err));
    } finally {
      setAdding(false);
    }
  }

  async function handleRemoveApp(app: WebApp) {
    const n = app.accounts.length;
    const accountWord = n === 1 ? "its 1 account" : `all ${n} of its accounts`;
    if (
      !window.confirm(
        `Remove "${app.name}" and ${accountWord}? This deletes their local session data (cookies, logins, cache) on this PC. It does not delete anything on the sites' servers.`
      )
    ) {
      return;
    }
    setError(null);
    try {
      await invoke("remove_app", { id: app.id });
      setApps((prev) => prev.filter((a) => a.id !== app.id));
      setExpanded((prev) => {
        const next = new Set(prev);
        next.delete(app.id);
        return next;
      });
    } catch (err) {
      setError(errMsg(err));
    }
  }

  async function handleFillLogin(app: WebApp, account: Account) {
    setError(null);
    try {
      const msg = await invoke<string>("fill_login", {
        appId: app.id,
        accountId: account.id,
      });
      setNotice(msg);
    } catch (err) {
      setError(errMsg(err));
    }
  }

  async function handleOpenAccount(
    app: WebApp,
    account: Account,
    forcePlain = false,
    isPrivate = false
  ): Promise<boolean> {
    setError(null);
    // v0.13.0: the "open as tabbed" setting makes launcher opens create
    // tabbed windows; the tile menu's explicit Open passes forcePlain.
    // Private windows always open plain (never tabbed).
    const asTabbed =
      !forcePlain && !isPrivate && (launcherSettings?.open_as_tabbed ?? false);
    try {
      if (asTabbed) {
        await invoke("open_app_in_tabbed_window", {
          appId: app.id,
          accountId: account.id,
        });
      } else {
        await invoke("open_account", {
          appId: app.id,
          accountId: account.id,
          isPrivate,
        });
      }
      // Refresh last_opened display.
      const now = Math.floor(Date.now() / 1000);
      setApps((prev) =>
        prev.map((a) =>
          a.id === app.id
            ? {
                ...a,
                accounts: a.accounts.map((acc) =>
                  acc.id === account.id ? { ...acc, last_opened: now } : acc
                ),
              }
            : a
        )
      );
      return true;
    } catch (err) {
      setError(`Could not open "${account.label}". ${errMsg(err)}`);
      return false;
    }
  }

  async function handleSuspendAccount(app: WebApp, account: Account) {
    setError(null);
    try {
      await invoke("suspend_account", { appId: app.id, accountId: account.id });
    } catch (err) {
      setError(`Could not suspend "${account.label}". ${errMsg(err)}`);
    }
  }

  /** v0.7.0: "Forget this login" confirm dialog's Confirm button. */
  async function handleForgetLoginConfirm() {
    const target = forgetLoginTarget;
    setForgetLoginTarget(null);
    if (!target) return;
    setError(null);
    try {
      await forgetLogin(target.app.id, target.account.id);
      setNotice(
        `Signed out "${target.account.label}". Open it again to sign back in.`
      );
    } catch (err) {
      setError(`Could not forget "${target.account.label}". ${errMsg(err)}`);
    }
  }

  /** v0.7.0: link-dispatcher picker choice. Optionally saves a domain rule. */
  async function handleLinkPick(appId: string, accountId: string, remember: boolean) {
    const url = linkPickerUrl;
    setLinkPickerUrl(null);
    if (!url) return;
    setError(null);
    try {
      if (remember) {
        const domain = new URL(url).hostname;
        await invoke("add_link_rule", { domain, appId, accountId });
      }
      await invoke("open_link_in_account", { appId, accountId, url });
    } catch (err) {
      setError(`Could not open the link. ${errMsg(err)}`);
    }
  }

  /** v0.7.0: flattened account list for the link picker. */
  const pickerAccounts: LinkPickerAccount[] = useMemo(
    () =>
      apps.flatMap((a) =>
        a.accounts.map((acct) => ({
          appId: a.id,
          appName: a.name,
          accountId: acct.id,
          accountLabel: acct.label,
        }))
      ),
    [apps]
  );

  async function handleRemoveAccount(app: WebApp, account: Account) {
    if (
      !window.confirm(
        `Remove account "${account.label}"? This deletes its local session data (cookies, logins, cache) on this PC. It does not delete anything on the site's servers.`
      )
    ) {
      return;
    }
    setError(null);
    try {
      await invoke("remove_account", { appId: app.id, accountId: account.id });
      setApps((prev) =>
        prev.map((a) =>
          a.id === app.id
            ? { ...a, accounts: a.accounts.filter((acc) => acc.id !== account.id) }
            : a
        )
      );
    } catch (err) {
      setError(errMsg(err));
    }
  }

  async function handleRenameApp(app: WebApp, name: string) {
    setError(null);
    try {
      // Tauri exposes Rust snake_case params as camelCase to JS.
      const updated = await invoke<WebApp>("rename_app", { id: app.id, name });
      replaceApp(updated);
    } catch (err) {
      setError(errMsg(err));
    }
  }

  async function handleRenameAccount(app: WebApp, account: Account, label: string) {
    setError(null);
    try {
      const updated = await invoke<Account>("rename_account", {
        appId: app.id,
        accountId: account.id,
        label,
      });
      setApps((prev) =>
        prev.map((a) =>
          a.id === app.id
            ? {
                ...a,
                accounts: a.accounts.map((acc) =>
                  acc.id === account.id ? { ...acc, label: updated.label } : acc
                ),
              }
            : a
        )
      );
    } catch (err) {
      setError(errMsg(err));
    }
  }

  /**
   * Best-effort logo fetch: ask the backend to download the site's icon and
   * cache it locally, then paint it onto the app's tiles. Failures are
   * silent by design — the tiles keep their fallbacks.
   */
  const refreshAppIcon = useCallback(async (appId: string) => {
    try {
      const icon = await invoke<string | null>("fetch_favicon", { appId });
      if (icon) {
        setApps((prev) =>
          prev.map((a) => (a.id === appId ? { ...a, icon } : a))
        );
      }
    } catch {
      /* best-effort: keep the fallback logo */
    }
  }, []);

  // --- launcher search -------------------------------------------------

  // Empty query shows the whole phone-folder grid; typing filters it with
  // the fuzzy matcher. Pinned tiles sort first (in pin order), then
  // usage-ranked, then the existing order.
  const sortOpts = useMemo(
    () => ({
      pinned: launcherSettings?.pinned,
      usage: launcherSettings?.usage,
      hiddenProgramIds: launcherSettings?.hidden_programs,
      // v0.9.6: pinned workspace tiles materialize from these.
      workspaces: workspaceList?.workspaces.map((w) => ({
        id: w.id,
        name: w.name,
      })),
    }),
    [launcherSettings, workspaceList]
  );
  // v0.8.0 workspaces: filter apps/accounts to the active workspace.
  // Programs and launcher commands stay visible (programs can't be
  // workspace members; hiding them would strand access).
  const activeWorkspace = useMemo(
    () =>
      workspaceList?.workspaces.find(
        (w) => w.id === workspaceList.active_workspace_id
      ) ?? null,
    [workspaceList]
  );
  const items = useMemo(() => {
    const scopedApps = filterAppsByWorkspace(apps, activeWorkspace);
    return query.trim()
      ? buildResults(query, scopedApps, programs, routines, sortOpts)
      : browseAll(scopedApps, programs, sortOpts);
  }, [query, apps, programs, routines, sortOpts, activeWorkspace]);

  // v0.7.0: built-in launcher commands (=calc, >system, ?web-search,
  // URL quick-add). Prefix commands take precedence over app/program
  // matches; quick-add only appears when nothing else matched.
  const command = useMemo(
    () => matchLauncherCommand(query, query.trim() !== "" && items.length === 0),
    [query, items]
  );
  const cmdRef = useRef<LauncherCommandRowsHandle>(null);

  // Right-click tile menu (Open / Pin / Hide / Edit / Remove).
  // v0.9.6: handleTogglePin is shared with the Workspaces section's
  // Pin/Unpin buttons.
  async function handleTogglePin(itemId: string) {
    try {
      const updated = await invoke<LauncherSettings>("toggle_pin", {
        itemId,
      });
      setLauncherSettings(updated);
    } catch (err) {
      setError(errMsg(err));
    }
  }
  const { tileMenuNode, openTileMenu } = useTileMenu({
    isPinned: (id) => launcherSettings?.pinned?.includes(id) ?? false,
    windowPin: (label) =>
      openLabels.has(label)
        ? { open: true, pinned: pinnedLabels.has(label) }
        : null,
    actions: {
      onOpen: (r) => void activateResult(r, true),
      onOpenAccount: (app, acct) => void handleOpenAccount(app, acct),
      // v0.13.0: open an app/account as the first tab of a new tabbed
      // window. Spotlight behavior: dismiss the overlay on success.
      onOpenAsTabbed: (app, acct) => {
        void (async () => {
          setError(null);
          try {
            await invoke("open_app_in_tabbed_window", {
              appId: app.id,
              accountId: acct.id,
            });
            setQuery("");
            await invoke("hide_library");
          } catch (err) {
            setError(errMsg(err));
          }
        })();
      },
      onTogglePin: (itemId) => void handleTogglePin(itemId),
      onToggleWindowPin: (label) => void handleToggleWindowPin(label),
      onCloseWindow: (label) => void handleCloseWindow(label),
      onEditApp: (app) => {
        // The Edit dialog lives in the library view: switch there first,
        // then open it. Both state updates batch into one render.
        void invoke("show_library").catch(() => {});
        const found = apps.find((a) => a.id === app.id);
        if (found) setEditingApp(found);
      },
      onRemoveApp: (app) => void handleRemoveApp(app),
      onForgetLogin: (app, account) => setForgetLoginTarget({ app, account }),
      onHideProgram: async (programId) => {
        try {
          const updated = await invoke<LauncherSettings>("set_program_hidden", {
            programId,
            hidden: true,
          });
          setLauncherSettings(updated);
          setPrograms(await invoke<NativeProgram[]>("list_programs"));
        } catch (err) {
          setError(errMsg(err));
        }
      },
      onRevealProgramLocation: async (program) => {
        try {
          await revealItemInDir(program.exe_path);
        } catch (err) {
          setError(errMsg(err));
        }
      },
      onRemoveCustomProgram: async (programId) => {
        try {
          await invoke("remove_custom_program", { programId });
          setPrograms(await invoke<NativeProgram[]>("list_programs"));
        } catch (err) {
          setError(errMsg(err));
        }
      },
    },
  });

  useEffect(() => {
    setActiveIndex(0);
  }, [query]);

  // Keep the highlight inside the list when the items change underneath it
  // (e.g. apps finishing loading while the overlay is open).
  useEffect(() => {
    setActiveIndex((i) => Math.min(i, Math.max(0, items.length - 1)));
  }, [items]);

  // Keep the highlighted tile visible while arrow-keying through the grid.
  useEffect(() => {
    document
      .querySelector(".icon-tile.is-active")
      ?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  function mostRecentAccount(app: WebApp): Account | undefined {
    return [...app.accounts].sort((a, b) => b.last_opened - a.last_opened)[0];
  }

  async function activateResult(r: SearchResult, forcePlain = false) {
    setError(null);
    try {
      if (r.kind === "routine") {
        // camelCase invoke arg for the snake_case Rust param `routine_id`.
        const summary = await invoke<string>("run_routine", {
          routineId: r.routine.id,
        });
        setNotice(`${r.routine.name}: ${summary}`);
      } else if (r.kind === "program") {
        await invoke("launch_program", { id: r.program.id });
      } else if (r.kind === "account") {
        const ok = await handleOpenAccount(r.app, r.account, forcePlain);
        if (!ok) return;
      } else if (r.kind === "search") {
        // v0.9.3: pinned web search — re-run the query in the shared
        // in-app search window.
        await invoke("open_web_search", { query: r.query });
      } else if (r.kind === "workspace") {
        // v0.9.6: pinned workspace tile — open every member window at
        // once through the normal open-account path (default placement;
        // session restore handles exact geometry). One failure doesn't
        // stop the rest.
        const ws = workspaceList?.workspaces.find(
          (w) => w.id === r.workspaceId
        );
        if (ws) {
          for (const { app, account } of resolveWorkspaceTargets(
            ws.members,
            apps
          ))
            await handleOpenAccount(app, account);
        }
      } else {
        // Web app row: open the most recently used account.
        const acct = mostRecentAccount(r.app) ?? r.app.accounts[0];
        if (!acct) return;
        const ok = await handleOpenAccount(r.app, acct, forcePlain);
        if (!ok) return;
      }
      // Usage ranking: the tile's tagged id (app:/account:/program:/
      // search:) feeds the pinned-first, usage-ranked sort. Fire-and-forget.
      recordLaunch(r.id);
      // Spotlight behavior: a successful activation dismisses the overlay.
      setQuery("");
      await invoke("hide_library");
    } catch (err) {
      setError(errMsg(err));
    }
  }

  function onSearchKeyDown(e: React.KeyboardEvent) {
    const last = items.length - 1;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActiveIndex((i) => Math.min(i + GRID_COLUMNS, last));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActiveIndex((i) => Math.max(i - GRID_COLUMNS, 0));
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      setActiveIndex((i) => Math.min(i + 1, last));
    } else if (e.key === "ArrowLeft") {
      e.preventDefault();
      setActiveIndex((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      // A shown command row owns Enter; otherwise activate the tile.
      if (command) {
        cmdRef.current?.activate();
      } else {
        const r = items[activeIndex];
        if (r) void activateResult(r);
      }
    } else if (e.key === "Escape") {
      if (query) {
        setQuery("");
      } else {
        void invoke("hide_library").catch((err) => setError(errMsg(err)));
      }
    }
  }

  function renderResultIcon(r: SearchResult): React.ReactNode {
    if (r.kind === "program") return <ProgramIcon program={r.program} />;
    if (r.kind === "search") return <SearchTileIcon />;
    if (r.kind === "workspace") return <WorkspaceTileIcon />;
    if (r.kind === "routine")
      return (
        <span className="app-icon-fallback" aria-hidden="true">
          {r.routine.name.charAt(0).toUpperCase() || "?"}
        </span>
      );
    if (r.kind === "account")
      return <AccountThumb account={r.account} app={r.app} />;
    return <AppIcon app={r.app} />;
  }

  const banners = (
    <>
      {error && (
        <div className="banner banner-error" role="alert">
          {error}
        </div>
      )}
      {notice && (
        <div className="banner banner-info" role="status">
          {notice}
        </div>
      )}
      {platform && !platform.network_adblock && (
        <div className="banner banner-info" role="status">
          Network-level ad blocking is Windows-only in this build. On this device you still
          get popup blocking and cosmetic ad hiding.
        </div>
      )}
      {platform && platform.network_adblock && !platform.filter_lists_loaded && (
        <div className="banner banner-info" role="status">
          Ad filter lists are still downloading — blocking starts automatically when ready.
        </div>
      )}
    </>
  );

  const addCreatedApp = (outcome: AddAppOutcome) => {
    if (!outcome.created) {
      revealApp(outcome.app, `"${outcome.app.name}" is already in your library — showing it instead of adding a duplicate.`);
      return;
    }
    const created = outcome.app;
    setNotice(null);
    setApps((prev) => [
      ...prev,
      { ...created, accounts: created.accounts ?? [], settings: created.settings ?? DEFAULT_SETTINGS },
    ]);
    // Fetch the site's logo in the background so the new tile gets its icon.
    void refreshAppIcon(created.id);
  };

  // Phone-folder overlay: search field on top, grid of app icons below.
  // Management lives one click away in the library view.
  if (view === "launcher") {
    return (
      <div className="launcher-shell">
        <ContextMenuGuard />
        <div
          className="folder-panel"
          key={summonCount}
          style={{ "--folder-alpha": String(launcherSettings?.panel_opacity ?? 0.55) } as React.CSSProperties}
        >
          <SearchBar
            query={query}
            onQuery={setQuery}
            onKeyDown={onSearchKeyDown}
            inputRef={searchRef}
          />
          {restoreOffer && (
            <div className="banner banner-info restore-offer" role="status">
              <span>
                Restore {restoreOffer.windowCount}{" "}
                {restoreOffer.windowCount === 1 ? "window" : "windows"} from
                last time?
              </span>
              {!!restoreOffer.staleRestore && (
                <span className="muted small">
                  {" "}
                  The app closed while reopening windows last time, so it
                  didn't try again.
                </span>
              )}
              {restoreOffer.names.length > 0 && (
                <span className="muted small">
                  {" "}
                  {restoreOffer.names.join(", ")}
                  {restoreOffer.windowCount > restoreOffer.names.length
                    ? ", …"
                    : ""}
                </span>
              )}
              <span className="restore-offer-actions">
                <button
                  type="button"
                  onClick={() => void handleRestoreSession()}
                  disabled={restoring}
                >
                  {restoring ? "Restoring…" : "Restore"}
                </button>
                <button
                  type="button"
                  className="text-button"
                  onClick={() => void handleDismissRestore()}
                >
                  Dismiss
                </button>
              </span>
            </div>
          )}
          {workspaceList && workspaceList.workspaces.length > 0 && (
            <div
              className="workspace-chips"
              role="group"
              aria-label="Workspace filter"
            >
              <button
                className={`workspace-chip${
                  workspaceList.active_workspace_id === null
                    ? " is-active"
                    : ""
                }`}
                onClick={() =>
                  void invoke<WorkspaceList>("set_active_workspace", {
                    workspaceId: null,
                  })
                    .then(setWorkspaceList)
                    .catch((err) => setError(errMsg(err)))
                }
              >
                All
              </button>
              {workspaceList.workspaces.map((ws) => (
                <button
                  key={ws.id}
                  className={`workspace-chip${
                    workspaceList.active_workspace_id === ws.id
                      ? " is-active"
                      : ""
                  }`}
                  title={ws.hotkey ? `Hotkey: ${ws.hotkey}` : undefined}
                  onClick={() =>
                    void invoke<WorkspaceList>("set_active_workspace", {
                      workspaceId: ws.id,
                    })
                      .then(setWorkspaceList)
                      .catch((err) => setError(errMsg(err)))
                  }
                >
                  {ws.name}
                </button>
              ))}
            </div>
          )}
          {banners}
          <div className="restore-row">
            <button
              type="button"
              className="text-button"
              title="Reopen windows from your last session"
              onClick={() => void handleRestoreSession()}
            >
              Restore session
            </button>
            <button
              type="button"
              className="text-button"
              title="Open one window with tabs for several apps"
              onClick={() => void handleOpenTabbedWindow()}
              disabled={openingTabs}
            >
              {openingTabs ? "Opening…" : "Open tabbed window"}
            </button>
          </div>
          <div className="folder-grid-wrap">
            {command && (
              <LauncherCommandRows
                ref={cmdRef}
                command={command}
                onDone={() => {
                  setQuery("");
                  void invoke("hide_library").catch((err) =>
                    setError(errMsg(err))
                  );
                }}
                onError={setError}
                onPinSearch={(q) => {
                  // v0.9.3: pin this `?query` as a launcher tile. Same
                  // toggle_pin machinery as every other tile.
                  invoke<LauncherSettings>("toggle_pin", {
                    itemId: searchTag(q),
                  })
                    .then(setLauncherSettings)
                    .catch((err) => setError(errMsg(err)));
                }}
                searchPinned={
                  command.kind === "web-search" &&
                  (launcherSettings?.pinned?.includes(searchTag(command.query)) ?? false)
                }
              />
            )}
            {loading ? (
              <p className="muted folder-hint">Loading…</p>
            ) : items.length === 0 && query.trim() ? (
              <NoMatchesHint
                onProgramsRefreshed={setPrograms}
                onError={setError}
              />
            ) : items.length === 0 ? (
              <p className="muted folder-hint">
                Nothing here yet — add your first web app from Manage apps below.
              </p>
            ) : (
              <IconGrid
                items={items}
                activeIndex={activeIndex}
                onHover={setActiveIndex}
                onActivate={(r) => void activateResult(r)}
                renderIcon={renderResultIcon}
                pinnedIds={new Set(launcherSettings?.pinned ?? [])}
                onTileContextMenu={(r, x, y) => openTileMenu(r, x, y)}
              />
            )}
            {tileMenuNode}
          </div>
          <footer className="launcher-foot">
            <button className="text-button" onClick={() => setView("library")}>
              Manage apps →
            </button>
            <span className="launcher-foot-actions">
              <DownloadToolbarButton onOpen={() => setView("downloads")} />
              <AddProgramButton
                onAdded={() =>
                  invoke<NativeProgram[]>("list_programs")
                    .then(setPrograms)
                    .catch((err) => setError(errMsg(err)))
                }
              />
              <RescanButton
                onRefreshed={setPrograms}
                onError={setError}
              />
            </span>
            <span className="muted small">
              {apps.length} {apps.length === 1 ? "web app" : "web apps"} ·{" "}
              {programs.length} {programs.length === 1 ? "program" : "programs"}
            </span>
          </footer>
        </div>
      </div>
    );
  }

  // v0.9.8: full-page downloads view (Ctrl+J, Settings → Downloads,
  // or the launcher toolbar button).
  if (view === "downloads") {
    return (
      <div className="shell">
        <ContextMenuGuard />
        <button className="text-button library-back" onClick={() => setView("launcher")}>
          ← Launcher
        </button>
        <header className="header">
          <div className="header-row">
            <div>
              <h1>Downloads</h1>
              <p className="subtitle">
                Files you downloaded in AppMaka, with progress while they
                download.
              </p>
            </div>
          </div>
        </header>

        {banners}

        <DownloadsPage />
      </div>
    );
  }

  return (
    <div className="shell">
      <ContextMenuGuard />
      {showIntro && <IntroOverlay onGotIt={() => void dismissIntro()} />}
      <button className="text-button library-back" onClick={() => setView("launcher")}>
        ← Launcher
      </button>
      <header className="header">
        <div className="header-row">
          <div>
            <h1>AppMaka</h1>
            <p className="subtitle">Your web apps, each with its own isolated accounts.</p>
          </div>
          <button
            className="intro-help"
            onClick={() => setIntroOpen(true)}
            aria-label="Show the quick tour"
            title="Quick tour"
          >
            ?
          </button>
        </div>
      </header>

      {banners}

      <section className="panel">
        <h2>Add a web app</h2>
        <QuickAddForm onAdded={addCreatedApp} onError={setError} />
        <PreviewSignInForm onError={setError} />
        <details className="manual-add">
          <summary>Add manually instead</summary>
          <form className="add-form" onSubmit={(e) => void handleAdd(e)}>
            <label>
              <span>Name</span>
              <input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. Gmail"
                maxLength={80}
                autoComplete="off"
              />
            </label>
            <label>
              <span>URL</span>
              <input
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                placeholder="https://mail.google.com"
                inputMode="url"
                autoComplete="off"
              />
            </label>
            <label className="color-label">
              <span>Color</span>
              <input
                type="color"
                value={safeColor(color)}
                onChange={(e) => setColor(e.target.value)}
                aria-label="App color"
              />
            </label>
            <button type="submit" disabled={adding}>
              {adding ? "Adding…" : "Add app"}
            </button>
          </form>
          {formError && (
            <p className="form-error" role="alert">
              {formError}
            </p>
          )}
        </details>
      </section>

      <section className="panel">
        <h2>Library</h2>
        {loading ? (
          <p className="muted">Loading…</p>
        ) : apps.length === 0 ? (
          <p className="muted">No apps yet. Add your first web app above.</p>
        ) : (
          <ul className="app-grid">
            {apps.map((app) => {
              const isExpanded = expanded.has(app.id);
              const showSettings = settingsOpen.has(app.id);
              return (
                <li key={app.id} className={`app-card${isExpanded ? " is-expanded" : ""}`}>
                  <div className="card-head">
                    <AppIcon app={app} />
                    <div className="app-meta">
                      <InlineEdit
                        value={app.name}
                        onSave={(name) => void handleRenameApp(app, name)}
                        className="app-name"
                        maxLength={80}
                      />
                      <span className="app-url" title={app.url}>
                        {hostOf(app.url)}
                      </span>
                    </div>
                    <span
                      className="color-badge"
                      aria-label="App color"
                      title="App color"
                      style={{ backgroundColor: safeColor(app.color) }}
                    />
                    <div className="app-actions">
                      <button
                        className="text-button"
                        onClick={() => toggleExpanded(app.id)}
                        aria-expanded={isExpanded}
                      >
                        {isExpanded
                          ? "Hide"
                          : `Accounts (${app.accounts.length})`}
                      </button>
                      <button
                        className="text-button"
                        onClick={() => setEditingApp(app)}
                      >
                        Edit
                      </button>
                      <button className="danger" onClick={() => void handleRemoveApp(app)}>
                        Remove
                      </button>
                    </div>
                  </div>

                  {isExpanded && (
                    <div className="card-body">
                      {app.accounts.length === 0 ? (
                        <p className="muted small">No accounts yet.</p>
                      ) : (
                        <ul className="account-list">
                          {app.accounts.map((account) => (
                            <AccountRow
                              key={account.id}
                              account={account}
                              app={app}
                              onOpen={() => void handleOpenAccount(app, account)}
                              onOpenPrivate={() =>
                                void handleOpenAccount(app, account, true, true)
                              }
                              onFillLogin={() => void handleFillLogin(app, account)}
                              onSaveLogin={() =>
                                setSaveLoginTarget({ app, account })
                              }
                              onSuspend={() => void handleSuspendAccount(app, account)}
                              onRemove={() => void handleRemoveAccount(app, account)}
                              onRename={(label) => void handleRenameAccount(app, account, label)}
                              onEdit={() => setEditingAccount({ app, account })}
                              onForgetLogin={() => setForgetLoginTarget({ app, account })}
                            />
                          ))}
                        </ul>
                      )}

                      <AddAccountForm
                        app={app}
                        onAdded={(account) =>
                          setApps((prev) =>
                            prev.map((a) =>
                              a.id === app.id ? { ...a, accounts: [...a.accounts, account] } : a
                            )
                          )
                        }
                        onError={setError}
                      />

                      <button
                        className="text-button settings-toggle"
                        onClick={() => toggleSettings(app.id)}
                        aria-expanded={showSettings}
                      >
                        {showSettings ? "Hide settings" : "Settings"}
                      </button>

                      {showSettings && (
                        <AppSettingsForm
                          app={app}
                          networkAdblock={platform?.network_adblock ?? false}
                          onSaved={replaceApp}
                          onError={setError}
                        />
                      )}
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Workspaces</h2>
        </div>
        <WorkspacesSection
          apps={apps}
          list={workspaceList}
          onList={setWorkspaceList}
          onError={setError}
          pinned={launcherSettings?.pinned ?? []}
          onTogglePin={(itemId) => void handleTogglePin(itemId)}
        />
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Memory</h2>
          <button
            className="text-button"
            onClick={() => setRamOpen(true)}
          >
            Open memory dashboard
          </button>
        </div>
        <p className="muted small">
          See how much memory each open account window uses (approximate),
          and close them all at once.
        </p>
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Downloads</h2>
          <button
            className="text-button"
            onClick={() => setView("downloads")}
          >
            Open downloads
          </button>
        </div>
        <p className="muted small">
          Files you downloaded from inside AppMaka, with progress while they
          download.
        </p>
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Link handling</h2>
        </div>
        <LinkRules />
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Launcher</h2>
          <button
            className="text-button"
            onClick={() => setLauncherPanelOpen((v) => !v)}
            aria-expanded={launcherPanelOpen}
          >
            {launcherPanelOpen ? "Hide" : "Show"}
          </button>
        </div>
        {launcherPanelOpen &&
          (launcherSettings ? (
            <>
              <LauncherSettingsPanel
                settings={launcherSettings}
                onSaved={setLauncherSettings}
                programsCount={programs.length}
                onProgramsRefreshed={setPrograms}
                onError={setError}
                isWindows={platform?.os === "windows"}
              />
              <LauncherExtras
                settings={launcherSettings}
                programs={programs}
                onSaved={setLauncherSettings}
                onProgramsRefreshed={setPrograms}
                onError={setError}
              />
              <CmdHotkeysSection apps={apps} onError={setError} />
            </>
          ) : (
            <p className="muted">Loading…</p>
          ))}
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Routines</h2>
        </div>
        <p className="muted small">
          Open a set of apps and accounts with one click or one keystroke.
          For example, "Morning" can open your work email, your main chat
          account, and a dashboard.
        </p>
        <RoutinesSection
          apps={apps}
          programs={programs}
          onChanged={(list) => setRoutines(list)}
        />
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Clipboard</h2>
        </div>
        <p className="muted small">
          Keep a searchable history of everything you copy, on this PC only.
        </p>
        <ClipboardSection />
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>Updates</h2>
        </div>
        <VersionLine />
        <UpdaterSection
          autoCheck={launcherSettings?.auto_update_check ?? false}
          isWindows={platform?.os === "windows"}
        />
      </section>

      <footer className="footer muted">
        Accounts are isolated browser sessions stored on this PC. Network ad blocking:{" "}
        {platform
          ? platform.network_adblock
            ? "available"
            : "Windows only"
          : "checking…"}
        .
      </footer>

      {editingApp && (
        <EditAppDialog
          app={editingApp}
          onClose={() => setEditingApp(null)}
          onSaved={(updated) => {
            replaceApp(updated);
            setEditingApp(null);
          }}
        />
      )}

      {editingAccount && (
        <EditAccountDialog
          app={editingAccount.app}
          account={editingAccount.account}
          onClose={() => setEditingAccount(null)}
          onSaved={(updated) => {
            replaceApp(updated);
            setEditingAccount(null);
          }}
        />
      )}

      {saveLoginTarget && (
        <SaveLoginDialog
          app={saveLoginTarget.app}
          account={saveLoginTarget.account}
          onClose={() => setSaveLoginTarget(null)}
          onSaved={() => setSaveLoginTarget(null)}
        />
      )}

      {forgetLoginTarget && (
        <ForgetLoginDialog
          appName={forgetLoginTarget.app.name}
          accountLabel={forgetLoginTarget.account.label}
          onConfirm={() => void handleForgetLoginConfirm()}
          onCancel={() => setForgetLoginTarget(null)}
        />
      )}

      {errorEntry && (
        <ErrorDialog
          entry={errorEntry}
          onClose={() => setErrorEntry(null)}
        />
      )}

      {linkPickerUrl && (
        <LinkPicker
          url={linkPickerUrl}
          accounts={pickerAccounts}
          onPick={(appId, accountId, remember) =>
            void handleLinkPick(appId, accountId, remember)
          }
          onClose={() => setLinkPickerUrl(null)}
        />
      )}

      {ramOpen && (
        <RamDashboard open={ramOpen} onClose={() => setRamOpen(false)} />
      )}
    </div>
  );
}
