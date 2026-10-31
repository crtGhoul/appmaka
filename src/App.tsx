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
import { UserScriptsSection } from "./UserScriptsSection";
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
  formatMb,
  IconGrid,
  LauncherCommandRows,
  LauncherSettingsPanel,
  QuickSwitchPalette,
  NoMatchesHint,
  ProgramIcon,
  RescanButton,
  SearchBar,
  SearchTileIcon,
  WorkspaceTileIcon,
  browseAll,
  buildResults,
  collectSearchEntries,
  matchLauncherCommand,
  primeSearchEntries,
  searchCacheAppDescriptors,
  recordLaunch,
  searchTag,
  resolveWorkspaceTargets,
  useProgramsScannedRefresh,
  useTileMenu,
} from "./Launcher";
import type { LauncherCommandRowsHandle, SearchResult } from "./Launcher";
import {
  buildSearchCachePayload,
  parseSearchCachePayload,
  type SearchCachePayload,
} from "./quickswitch";
import type {
  Account,
  AddAppOutcome,
  AccountMemory,
  AppSettings,
  LauncherSettings,
  NativeProgram,
  OpenAccountWindow,
  PlatformInfo,
  PreviewAddOutcome,
  PreviewStart,
  Routine,
  SessionRestoreOffer,
  SleepInfo,
  TabbedWindowInfo,
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
          Preview opened — sign in on the real site, then click “Add as app” in
          its header. Nothing is kept until you do.
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
  unread,
  ramKb,
  sleeping,
  suspended,
  onOpen,
  onManage,
  onRename,
}: {
  account: Account;
  app: WebApp;
  /** v0.15.0: unread count for the badge; undefined draws none. */
  unread?: number;
  /** v0.15.0: attributed RAM in KiB; undefined draws no line. */
  ramKb?: number;
  /** v0.15.0: closed by automatic reclaim; shows the Sleeping chip. */
  sleeping?: boolean;
  /** v0.16.4: open but frozen (Windows-only); shows the Suspended chip. */
  suspended?: boolean;
  onOpen: () => void;
  /** v0.15.0: open the per-account settings panel. */
  onManage: () => void;
  onRename: (label: string) => void;
}) {
  return (
    <li className="account-row">
      <AccountThumb account={account} app={app} />
      <div className="account-meta">
        <span className="account-label-row">
          <InlineEdit
            value={account.label}
            onSave={onRename}
            className="account-label"
            maxLength={60}
          />
          {unread != null && unread > 0 && (
            <span
              className="account-unread-badge"
              aria-label={`${unread} unread`}
              title={`${unread} unread`}
            >
              {unread > 99 ? "99+" : unread}
            </span>
          )}
          {/* A window is either open (possibly Suspended) or closed
              (Sleeping), never both; the else-if keeps exactly one chip
              even if the two state polls ever disagreed. */}
          {suspended ? (
            <span
              className="account-sleep-chip"
              title="Open but frozen to save memory — it wakes when you focus it, without reloading"
            >
              Suspended
            </span>
          ) : sleeping ? (
            <span
              className="account-sleep-chip"
              title="Closed automatically to save memory — reopening restores it"
            >
              Sleeping
            </span>
          ) : null}
        </span>
        <span className="account-opened">
          {openedLabel(account.last_opened)}
          {ramKb != null && ramKb > 0 && (
            <span
              className="account-ram"
              title="Memory used by this account's open window"
            >
              {" · "}
              {formatMb(ramKb)}
            </span>
          )}
        </span>
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
        <button className="text-button" onClick={onManage}>
          Manage
        </button>
      </div>
    </li>
  );
}

/**
 * v0.15.0: per-account settings panel — a slide-over in the same window
 * that replaces the old crammed row of eight buttons. Groups what already
 * exists: opening (Open / private / suspend), login (fill / save /
 * forget), and account settings, reusing the existing commands. Esc or
 * the backdrop closes it.
 */
function AccountPanel({
  app,
  account,
  onClose,
  onOpen,
  onOpenPrivate,
  onFillLogin,
  onSaveLogin,
  onSuspend,
  onFixBlank,
  onEdit,
  onForgetLogin,
  onRemove,
}: {
  app: WebApp;
  account: Account;
  onClose: () => void;
  onOpen: () => void;
  onOpenPrivate: () => void;
  onFillLogin: () => void;
  onSaveLogin: () => void;
  onSuspend: () => void;
  onFixBlank: () => void;
  onEdit: () => void;
  onForgetLogin: () => void;
  onRemove: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  const done = (fn: () => void) => () => {
    fn();
    onClose();
  };
  return (
    <div className="account-panel-backdrop" onClick={onClose}>
      <aside
        className="account-panel"
        role="dialog"
        aria-modal="false"
        aria-label={`${account.label} settings`}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="account-panel-head">
          <AccountThumb account={account} app={app} />
          <div className="account-panel-titles">
            <div className="account-panel-title">{account.label}</div>
            <div className="muted small">{app.name}</div>
          </div>
          <button
            type="button"
            className="text-button"
            onClick={onClose}
            aria-label="Close panel"
          >
            Close
          </button>
        </div>
        <div className="account-panel-group">
          <p className="muted small account-panel-label">Open</p>
          <button onClick={done(onOpen)}>Open</button>
          <button
            className="text-button"
            onClick={done(onOpenPrivate)}
            title="Open in a private window — nothing is saved after you close it."
          >
            Private window
          </button>
          <button className="text-button" onClick={done(onSuspend)}>
            Suspend
          </button>
          <button
            className="text-button"
            onClick={done(onFixBlank)}
            title="Close this window and reopen it fresh. Your login and session data are kept — only the renderer is rebuilt."
          >
            Fix blank page
          </button>
        </div>
        <div className="account-panel-group">
          <p className="muted small account-panel-label">Login</p>
          <button
            className="text-button"
            onClick={done(onFillLogin)}
            title="Fill the saved login into this account's open window. Only works on the site it was saved for."
          >
            Fill login
          </button>
          <button
            className="text-button"
            onClick={done(onSaveLogin)}
            title="Save this account's username and password in your system vault."
          >
            Save login
          </button>
          <button
            className="text-button danger"
            onClick={done(onForgetLogin)}
            title="Sign this account out by wiping its session. Other accounts are untouched."
          >
            Forget login
          </button>
        </div>
        <div className="account-panel-group">
          <p className="muted small account-panel-label">Settings</p>
          <button className="text-button" onClick={done(onEdit)}>
            Edit account…
          </button>
        </div>
        <div className="account-panel-group">
          <p className="muted small account-panel-label">Remove</p>
          <button className="danger" onClick={done(onRemove)}>
            Remove account
          </button>
        </div>
      </aside>
    </div>
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
  return "Ad blocking: on — cosmetic + scriptlets (network blocking is Windows-only)";
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
        <span className="help">Idle account windows are suspended to save RAM. On Windows, only minimized or hidden windows truly suspend; a visible window only gets a low-memory hint. The idle watcher covers account windows only, not tabbed windows or popups. 0 = never suspend.</span>
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

  function handleAutoUpdateDownload(enabled: boolean) {
    void mutate(
      () => invoke<LauncherSettings>("set_auto_update_download", { enabled }),
      "Could not change update downloads"
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
        Checks quietly in the background. Installing never starts without you.
      </span>

      <label className="check-row">
        <input
          type="checkbox"
          checked={settings.auto_update_download}
          onChange={(e) => handleAutoUpdateDownload(e.target.checked)}
        />
        <span>Download updates in the background</span>
      </label>
      <span className="help">
        Updates download quietly when found. Installing still waits for you.
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
  | { kind: "downloaded"; version: string }
  | { kind: "ready"; version: string }
  | { kind: "launched"; version: string }
  | { kind: "failed"; message: string };

/** Dotted-numeric compare: is `current` at least `target`? */
function versionAtLeast(current: string, target: string): boolean {
  const a = current.split(".").map((p) => parseInt(p, 10) || 0);
  const b = target.split(".").map((p) => parseInt(p, 10) || 0);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const d = (a[i] ?? 0) - (b[i] ?? 0);
    if (d !== 0) return d > 0;
  }
  return true;
}

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
 * "Couldn't check for updates right now." — never a silent "no updates"
 * and never an error popup. Download/install failures stay visible as a
 * plain-language message — never a silent return to the button.
 *
 * v0.8.2: on Windows the install type is detected at runtime. MSI installs
 * (Program Files) download the matching MSI and verify its signature;
 * everything else uses the plugin's NSIS flow. On Windows the
 * post-download copy is "Installer launched — follow its steps." (the
 * restart button is Linux-only).
 *
 * Background downloads: when automatic checks and background downloads
 * are both on, a found update downloads right away and waits in a
 * "downloaded" state — installing still takes a click. Manual checks
 * follow the same rule. The plugin can't keep downloaded bytes across a
 * quit, so a completed download persists only its version
 * (`pending_update_version`); on the next launch the panel re-checks and
 * re-downloads that version to land back on "downloaded". A failed
 * background download falls back to "available" with the manual button —
 * the user is never left stuck. The one deliberate exception to the
 * background rule is the MSI path in manual mode: it downloads, then
 * offers the install button — the installer never launches as a side
 * effect of a download click.
 *
 * The automatic check also lives here: once on mount when `autoCheck` is
 * on, then every 24h (skipped while the window is hidden). Auto-check
 * failures show nothing at all.
 */
function UpdaterSection({
  autoCheck,
  isWindows,
}: {
  autoCheck: boolean;
  isWindows: boolean;
}) {
  const [status, setStatus] = useState<UpdateStatus>({ kind: "idle" });
  // Windows only: whether the running install is MSI (per-machine). Only
  // the button label reads it — the download paths re-detect at click
  // time, like they always have.
  const [isMsi, setIsMsi] = useState(false);
  const pending = useRef<Update | null>(null);
  const downloadedBytes = useRef(0);
  const totalBytes = useRef<number | null>(null);
  const autoRan = useRef(false);
  // The update settings, read once on mount (the toggles live in
  // Launcher settings; changing them applies the next time this panel
  // mounts).
  const autoDownload = useRef(true);
  const pendingRecord = useRef<string | null>(null);
  const settingsReady = useRef<Promise<void> | null>(null);
  // The version `pending.current` has actually downloaded this session.
  // install() only works on the Update object that downloaded, so a
  // re-check that offers the same version must not swap the object out.
  const downloadedVersion = useRef<string | null>(null);
  // A download (background or manual) owns the status while it runs; a
  // check that lands mid-download must not start a second one over it.
  const downloadInFlight = useRef(false);

  function loadUpdateSettings(): Promise<void> {
    if (!settingsReady.current) {
      settingsReady.current = (async () => {
        try {
          const s = await invoke<LauncherSettings>("get_launcher_settings");
          autoDownload.current = s.auto_update_download;
          pendingRecord.current = s.pending_update_version;
        } catch {
          /* built-in defaults stand */
        }
        if (isWindows) {
          try {
            setIsMsi((await invoke<string>("get_install_type")) === "msi");
          } catch {
            /* the NSIS label is the safe default */
          }
        }
      })();
    }
    return settingsReady.current;
  }

  async function savePendingRecord(version: string) {
    pendingRecord.current = version;
    try {
      await invoke("set_pending_update_version", { version });
    } catch {
      /* the in-session flow still works; only the cross-quit note is lost */
    }
  }

  async function clearPendingRecord() {
    pendingRecord.current = null;
    try {
      await invoke("set_pending_update_version", { version: null });
    } catch {
      /* a stale note only costs one re-check next launch */
    }
  }

  /**
   * Download `update` without installing it and land on "downloaded".
   * Shared by the background rule and the manual MSI path.
   */
  async function downloadOnly(update: Update) {
    downloadInFlight.current = true;
    downloadedBytes.current = 0;
    totalBytes.current = null;
    setStatus({ kind: "downloading", version: update.version, progress: null });
    try {
      const installType = await invoke<string>("get_install_type");
      if (installType === "msi" && isWindows) {
        await downloadMsi(update);
      } else {
        await update.download((event) => {
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
    } finally {
      downloadInFlight.current = false;
    }
    await savePendingRecord(update.version);
    downloadedVersion.current = update.version;
    setStatus({ kind: "downloaded", version: update.version });
  }

  /**
   * One check for every caller (mount, 24h interval, quit-restore,
   * manual button). A found update follows the one rule: with automatic
   * checks and background downloads both on, it downloads right away;
   * otherwise it waits in "available" for the button.
   */
  async function runCheck(manual: boolean) {
    await loadUpdateSettings();
    if (manual) setStatus({ kind: "checking" });
    try {
      const update = await check({ timeout: 30_000 });
      if (!update) {
        if (manual) setStatus({ kind: "uptodate" });
        // A check that succeeds and offers nothing retires the note:
        // the recorded version was installed another way or pulled.
        if (pendingRecord.current) void clearPendingRecord();
        return;
      }
      if (downloadInFlight.current) return;
      if (downloadedVersion.current === update.version && pending.current) {
        // Already downloaded this session by the object in `pending` —
        // keep that object (install() needs it) and just re-show the state.
        setStatus({ kind: "downloaded", version: update.version });
        return;
      }
      pending.current = update;
      if (autoCheck && autoDownload.current) {
        try {
          await downloadOnly(update);
        } catch {
          // Never leave the user stuck: the manual button takes over.
          setStatus({ kind: "available", version: update.version });
        }
      } else {
        setStatus({ kind: "available", version: update.version });
      }
    } catch {
      // Manual checks say so; automatic ones stay silent.
      if (manual) setStatus({ kind: "none" });
    }
  }

  useEffect(() => {
    if (!autoCheck) return;
    // Once per session: StrictMode double-mounts in dev, so guard with a ref.
    if (!autoRan.current) {
      autoRan.current = true;
      void runCheck(false);
    }
    const id = window.setInterval(() => {
      if (isAppHidden()) return;
      void runCheck(false);
    }, 24 * 60 * 60 * 1000);
    return () => window.clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoCheck]);

  // Quit-restore: a previous session downloaded an update but never
  // installed it. Only the version note survived, so verify the note
  // against the running version, then let a normal check re-download it.
  useEffect(() => {
    let alive = true;
    void (async () => {
      await loadUpdateSettings();
      if (!alive) return;
      const recorded = pendingRecord.current;
      if (!recorded) return;
      try {
        const current = await getVersion();
        if (!alive) return;
        if (versionAtLeast(current, recorded)) {
          await clearPendingRecord();
          return;
        }
      } catch {
        return; // can't compare — leave the note for the next check
      }
      // Take the session's initial-check slot so the auto-check effect
      // doesn't stack a second check on top of this one (it only matters
      // when autoCheck is off — otherwise that effect already ran it).
      if (!autoRan.current) {
        autoRan.current = true;
        await runCheck(false);
      }
    })();
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function handleDownload() {
    const update = pending.current;
    if (!update || downloadInFlight.current) return;
    downloadInFlight.current = true;
    downloadedBytes.current = 0;
    totalBytes.current = null;
    setStatus({ kind: "downloading", version: update.version, progress: null });
    try {
      // v0.8.2: MSI-installed copies must get the MSI, not the NSIS setup.exe
      // that latest.json points at — otherwise the update never takes.
      const installType = await invoke<string>("get_install_type");
      if (installType === "msi" && isWindows) {
        // Two steps on purpose: download and verify now, launch from the
        // install button — never as a side effect of this click.
        await downloadMsi(update);
        await savePendingRecord(update.version);
        downloadedVersion.current = update.version;
        setStatus({ kind: "downloaded", version: update.version });
        return;
      }
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
      setStatus({ kind: "ready", version: update.version });
    } catch (e) {
      // Plain-language, and it stays visible — never a silent return to the
      // "Download & install" button.
      setStatus({ kind: "failed", message: plainUpdateError(e) });
    } finally {
      downloadInFlight.current = false;
    }
  }

  async function handleInstallDownloaded() {
    if (status.kind !== "downloaded") return;
    const version = status.version;
    const update = pending.current;
    // Installing retires the note even though the app is about to exit:
    // if the install is cancelled in the wizard, the next launch simply
    // offers the update again.
    downloadedVersion.current = null;
    await clearPendingRecord();
    try {
      const installType = await invoke<string>("get_install_type");
      if (installType === "msi" && isWindows) {
        // The backend exits the app after launching the installer, so the
        // invoke may never resolve — the launched event is the backup
        // signal, same as the old fused flow had.
        const unlistenLaunched = await listen("msi-update-launched", () => {
          setStatus({ kind: "launched", version });
        });
        try {
          await invoke("launch_msi_update");
          setStatus({ kind: "launched", version });
        } finally {
          unlistenLaunched();
        }
        return;
      }
      if (!update) {
        setStatus({
          kind: "failed",
          message: "The downloaded update was lost — check for updates again.",
        });
        return;
      }
      await update.install();
      if (isWindows) {
        setStatus({ kind: "launched", version });
      } else {
        // Linux: install() swaps the files; relaunch runs the new build.
        await relaunch();
      }
    } catch (e) {
      setStatus({ kind: "failed", message: plainUpdateError(e) });
    }
  }

  /**
   * v0.8.2 MSI path, download half: download the matching MSI (derived
   * from the NSIS URL in latest.json) and verify its signature. The
   * verified file waits in app-data; `launch_msi_update` is the separate
   * install step. Emits progress events while downloading.
   */
  async function downloadMsi(update: Update) {
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
        setStatus((prev) =>
          prev.kind === "downloading" ? { ...prev, progress } : prev
        );
      }
    );
    try {
      await invoke<string>("download_msi_update", {
        nsisUrl,
        version: update.version,
      });
    } finally {
      unlistenProgress();
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
        <button onClick={() => void runCheck(true)} disabled={busy}>
          {status.kind === "checking" ? "Checking…" : "Check for updates"}
        </button>
        <span className="muted small" role="status">
          {status.kind === "idle" && "Never checked this session."}
          {status.kind === "checking" && "Checking…"}
          {status.kind === "uptodate" && "You're up to date."}
          {status.kind === "none" && "Couldn't check for updates right now."}
          {status.kind === "available" &&
            `Version ${status.version} is available.`}
          {status.kind === "downloading" &&
            (status.progress === null
              ? `Downloading ${status.version}…`
              : `Downloading ${status.version}… ${status.progress}%`)}
          {status.kind === "downloaded" &&
            `Version ${status.version} is downloaded.`}
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
            {isMsi
              ? `Download ${status.version}`
              : `Download & install ${status.version}`}
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
      {status.kind === "downloaded" && (
        <div>
          <button onClick={() => void handleInstallDownloaded()}>
            Restart &amp; install {status.version}
          </button>
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
        When background downloads are on, a found update downloads on its
        own. Installing and restarting always wait for your click.
      </p>
    </div>
  );
}

/**
 * v0.13.17: accent palettes for the appearance settings. Each maps the
 * CSS variables declared in App.css (indigo defaults) to one accent's
 * tones: base / hover / mid / bright / text + RGB triplets for rgba().
 */
const ACCENT_PALETTES: Record<string, Record<string, string>> = {
  indigo: {
    "--accent": "#4f46e5", "--accent-hover": "#4338ca", "--accent-mid": "#6366f1",
    "--accent-bright": "#818cf8", "--accent-text": "#a5b4fc",
    "--accent-rgb": "79, 70, 229", "--accent-mid-rgb": "99, 102, 241", "--accent-bright-rgb": "129, 140, 248",
  },
  violet: {
    "--accent": "#7c3aed", "--accent-hover": "#6d28d9", "--accent-mid": "#8b5cf6",
    "--accent-bright": "#a78bfa", "--accent-text": "#c4b5fd",
    "--accent-rgb": "124, 58, 237", "--accent-mid-rgb": "139, 92, 246", "--accent-bright-rgb": "167, 139, 250",
  },
  blue: {
    "--accent": "#2563eb", "--accent-hover": "#1d4ed8", "--accent-mid": "#3b82f6",
    "--accent-bright": "#60a5fa", "--accent-text": "#93c5fd",
    "--accent-rgb": "37, 99, 235", "--accent-mid-rgb": "59, 130, 246", "--accent-bright-rgb": "96, 165, 250",
  },
  teal: {
    "--accent": "#0d9488", "--accent-hover": "#0f766e", "--accent-mid": "#14b8a6",
    "--accent-bright": "#2dd4bf", "--accent-text": "#5eead4",
    "--accent-rgb": "13, 148, 136", "--accent-mid-rgb": "20, 184, 166", "--accent-bright-rgb": "45, 212, 191",
  },
  green: {
    "--accent": "#16a34a", "--accent-hover": "#15803d", "--accent-mid": "#22c55e",
    "--accent-bright": "#4ade80", "--accent-text": "#86efac",
    "--accent-rgb": "22, 163, 74", "--accent-mid-rgb": "34, 197, 94", "--accent-bright-rgb": "74, 222, 128",
  },
  amber: {
    "--accent": "#d97706", "--accent-hover": "#b45309", "--accent-mid": "#f59e0b",
    "--accent-bright": "#fbbf24", "--accent-text": "#fcd34d",
    "--accent-rgb": "217, 119, 6", "--accent-mid-rgb": "245, 158, 11", "--accent-bright-rgb": "251, 191, 36",
  },
  rose: {
    "--accent": "#e11d48", "--accent-hover": "#be123c", "--accent-mid": "#f43f5e",
    "--accent-bright": "#fb7185", "--accent-text": "#fda4af",
    "--accent-rgb": "225, 29, 72", "--accent-mid-rgb": "244, 63, 94", "--accent-bright-rgb": "251, 113, 133",
  },
};

/**
 * v0.13.17: push the saved appearance onto <html>: the resolved theme
 * ("system" follows the OS live), the tile size, and the accent palette
 * as CSS variables. The CSS keys everything off these attributes/vars.
 */
function applyAppearance(s: LauncherSettings | null) {
  const root = document.documentElement;
  const theme = s?.theme ?? "system";
  const resolved =
    theme === "system"
      ? window.matchMedia("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light"
      : theme;
  root.dataset.theme = resolved;
  root.dataset.tileSize = s?.tile_size ?? "cozy";
  root.dataset.tileCorners = s?.tile_corners ?? "round";
  root.dataset.launcherBg = s?.launcher_background ?? "frosted";
  const pal = ACCENT_PALETTES[s?.accent ?? "indigo"] ?? ACCENT_PALETTES.indigo;
  for (const [k, v] of Object.entries(pal)) root.style.setProperty(k, v);
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
  // v0.16.1: platform info is snapshotted once at mount, but the filter
  // lists usually finish after that — re-poll while they load so the
  // "still downloading" banner clears (or turns into the honest failed
  // state) without an app restart. Bounded: ~2 minutes of polling.
  const listsState = platform?.filter_lists_state;
  useEffect(() => {
    if (listsState !== "loading") return;
    let tries = 0;
    const id = window.setInterval(() => {
      tries += 1;
      if (tries > 40) {
        window.clearInterval(id);
        return;
      }
      invoke<PlatformInfo>("platform_info")
        .then((p) => {
          setPlatform(p);
          if (p.filter_lists_state !== "loading") window.clearInterval(id);
        })
        .catch(() => {});
    }, 3000);
    return () => window.clearInterval(id);
  }, [listsState]);
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
  const [settingsOpen, setSettingsOpen] = useState<Set<string>>(new Set());
  const [editingApp, setEditingApp] = useState<WebApp | null>(null);
  const [editingAccount, setEditingAccount] = useState<{
    app: WebApp;
    account: Account;
  } | null>(null);
  // v0.15.0: per-account settings panel target.
  const [panelAccount, setPanelAccount] = useState<{
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
  // v0.13.17: apply the saved appearance (accent/theme/tile size) to
  // <html> whenever settings load or change, and re-resolve "system"
  // when the OS theme flips.
  const launcherSettingsRef = useRef<LauncherSettings | null>(null);
  launcherSettingsRef.current = launcherSettings;
  useEffect(() => {
    applyAppearance(launcherSettings);
  }, [launcherSettings]);
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = () => applyAppearance(launcherSettingsRef.current);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
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
  // v0.15.0: unread counts keyed `${appId}:${accountId}`, for the tile
  // and Library badges. Rides the same poll — no new timer.
  const [unreadCounts, setUnreadCounts] = useState<Map<string, number>>(
    new Map()
  );
  // v0.15.0: per-account RAM keyed `${appId}:${accountId}`. The scan can
  // stall on some PCs, so it races a timeout and never delays the poll.
  const [accountRam, setAccountRam] = useState<Map<string, number>>(new Map());
  // v0.15.0: sleeping accounts (idle auto-close / LRU cap) keyed the same
  // way, with their last attributed RAM for the "Memory saved" total.
  const [sleeping, setSleeping] = useState<Map<string, number>>(new Map());
  // v0.16.4: open-but-frozen windows (Windows-only), keyed the same way.
  // Disjoint from `sleeping` by construction: an account's window is
  // either open (possibly suspended) or closed by reclaim (sleeping) —
  // never both at once.
  const [suspendedKeys, setSuspendedKeys] = useState<Set<string>>(new Set());
  // v0.18.5: Now Playing titles, Keep Awake locks, and live adblock
  // flags for open windows, keyed the same way. Only open windows
  // appear in the poll, so presence here implies open.
  const [playingTitles, setPlayingTitles] = useState<Map<string, string>>(
    new Map()
  );
  const [awakeKeys, setAwakeKeys] = useState<Set<string>>(new Set());
  const [adblockKeys, setAdblockKeys] = useState<Set<string>>(new Set());
  const refreshWindowPins = useCallback(async () => {
    try {
      const ramTimeout = new Promise<AccountMemory[]>((resolve) =>
        setTimeout(() => resolve([]), 5000)
      );
      const [wins, ram, sleep, tabbed] = await Promise.all([
        invoke<OpenAccountWindow[]>("list_open_account_windows"),
        Promise.race([
          invoke<AccountMemory[]>("account_memory").catch(
            (): AccountMemory[] => []
          ),
          ramTimeout,
        ]),
        invoke<SleepInfo[]>("sleep_state").catch((): SleepInfo[] => []),
        invoke<TabbedWindowInfo[]>("list_tabbed_windows").catch(
          (): TabbedWindowInfo[] => []
        ),
      ]);
      setOpenLabels(new Set(wins.map((w) => w.label)));
      setPinnedLabels(
        new Set(wins.filter((w) => w.pinned).map((w) => w.label))
      );
      const unread = new Map<string, number>();
      for (const w of wins) {
        if (w.unread != null && w.unread > 0) {
          unread.set(`${w.appId}:${w.accountId}`, w.unread);
        }
      }
      // Tabbed tabs fill in counts no standalone window reported; a
      // standalone window's count for the same account wins.
      for (const g of tabbed) {
        for (const t of g.tabs) {
          const key = `${t.appId}:${t.accountId}`;
          if (t.unread != null && !unread.has(key)) {
            unread.set(key, t.unread);
          }
        }
      }
      setUnreadCounts(unread);
      setSuspendedKeys(
        new Set(
          wins
            .filter((w) => w.suspended)
            .map((w) => `${w.appId}:${w.accountId}`)
        )
      );
      // v0.18.5: Now Playing / Awake / Adblock state for open windows.
      const playing = new Map<string, string>();
      const awake = new Set<string>();
      const adblocked = new Set<string>();
      for (const w of wins) {
        if (!w.accountId) continue;
        const key = `${w.appId}:${w.accountId}`;
        if (w.playingAudio) playing.set(key, w.pageTitle);
        if (w.keepAwake) awake.add(key);
        if (w.adblockOn) adblocked.add(key);
      }
      setPlayingTitles(playing);
      setAwakeKeys(awake);
      setAdblockKeys(adblocked);
      const ramMap = new Map<string, number>();
      for (const m of ram) {
        if (m.rssKb > 0) {
          ramMap.set(`${m.appId}:${m.accountId}`, m.rssKb);
        }
      }
      setAccountRam(ramMap);
      const sleepMap = new Map<string, number>();
      for (const s of sleep) {
        sleepMap.set(`${s.appId}:${s.accountId}`, s.rssKb);
      }
      setSleeping(sleepMap);
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

  // A window promoted to an application (popup system menu,
  // Ctrl+Shift+A, RAM dashboard) lands in the store behind this
  // component's mount-time snapshot — re-list when the backend says
  // the library changed so it appears without a restart.
  useEffect(() => {
    let off: (() => void) | undefined;
    listen("appmaka:apps-changed", () => {
      invoke<WebApp[]>("list_apps")
        .then(setApps)
        .catch(() => {});
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

  // v0.18.0: the persisted search-index snapshot (quickswitch.ts),
  // shared by the cache-load effect and load() below — whichever
  // lands second primes the fuzzy entries, so both race orders end
  // warm. Refs, because the priming targets are state-owned objects.
  const searchCacheRef = useRef<SearchCachePayload | null>(null);
  const liveLoadedRef = useRef(false);
  const lastCacheFingerprintRef = useRef<string | null>(null);
  const appsRef = useRef<WebApp[]>([]);
  useEffect(() => {
    appsRef.current = apps;
  }, [apps]);
  const programsRef = useRef<NativeProgram[]>([]);
  useEffect(() => {
    programsRef.current = programs;
  }, [programs]);
  const routinesRef = useRef<Routine[]>([]);
  useEffect(() => {
    routinesRef.current = routines;
  }, [routines]);

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
      const normalizedApps = list.map((a) => ({ ...a, accounts: a.accounts ?? [], settings: a.settings ?? DEFAULT_SETTINGS }));
      setApps(normalizedApps);
      setPlatform(info);
      // v0.18.0: an empty live list means the backend scan hasn't
      // landed yet — keep the cache seed until it does (the refetch
      // below and the programs-scanned listener replace it later).
      const effectivePrograms =
        progList.length > 0 || !searchCacheRef.current
          ? progList
          : searchCacheRef.current.programs;
      setPrograms(effectivePrograms);
      setLauncherSettings(launchSettings);
      setRoutines(routineList);
      // v0.18.0: the cache beat live data here — warm the fuzzy-entry
      // cache from it (entries re-verified per item before use).
      if (searchCacheRef.current) {
        primeSearchEntries(
          searchCacheRef.current.entries,
          normalizedApps,
          effectivePrograms,
          routineList
        );
      }
      liveLoadedRef.current = true;
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

  // v0.18.0: seed search from the persisted index before live data
  // lands. The backend's startup program scan can take seconds; the
  // cache makes the previous session's program list — and its
  // prebuilt fuzzy entries — searchable immediately. An unusable
  // cache parses to null and this is exactly the pre-cache cold
  // start; no error is ever shown for it.
  useEffect(() => {
    invoke<string | null>("load_search_index")
      .then((raw) => {
        if (!raw) return;
        const cache = parseSearchCachePayload(raw);
        if (!cache) return;
        searchCacheRef.current = cache;
        lastCacheFingerprintRef.current = cache.fingerprint;
        // Only fill a still-empty list: live data that already
        // landed is fresher and wins.
        setPrograms((prev) => (prev.length > 0 ? prev : cache.programs));
        primeSearchEntries(
          cache.entries,
          appsRef.current,
          programsRef.current.length > 0 ? programsRef.current : cache.programs,
          routinesRef.current
        );
      })
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // v0.18.0: stale-while-revalidate's write half — refresh the cache
  // whenever the live lists change (first load, rescans, library
  // edits). The fingerprint gate keeps routine state churn (icon
  // backfills, usage bumps) from rewriting megabytes of identical
  // JSON, and an empty program list never overwrites a good snapshot
  // (it means the backend scan hasn't landed yet).
  useEffect(() => {
    if (!liveLoadedRef.current) return;
    if (programs.length === 0) return;
    const payload = buildSearchCachePayload(
      searchCacheAppDescriptors(apps),
      programs,
      routines.map((r) => ({ id: r.id, name: r.name })),
      collectSearchEntries(apps, programs, routines),
      Date.now()
    );
    if (payload.fingerprint === lastCacheFingerprintRef.current) return;
    lastCacheFingerprintRef.current = payload.fingerprint;
    invoke("save_search_index", { contents: JSON.stringify(payload) }).catch(
      () => {}
    );
  }, [apps, programs, routines]);

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
      setPaletteOpen(false);
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

  // v0.15.0: Ctrl+K quick-switch — opens the palette while the launcher
  // is focused. In-window only; no global hotkey hook.
  const [paletteOpen, setPaletteOpen] = useState(false);
  useEffect(() => {
    if (view !== "launcher") return;
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k" && !e.repeat) {
        e.preventDefault();
        setPaletteOpen(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [view]);

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
      const msg = `Could not open "${account.label}". ${errMsg(err)}`;
      setError(msg);
      // The inline error above vanishes when the launcher closes, so
      // record it where the settings error history can show it later.
      // Fire-and-forget: recording must never break the open flow.
      void invoke("record_frontend_error", {
        kind: "window-open",
        message: msg,
        technical: errMsg(err),
      }).catch(() => {});
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

  /** v0.18.5: tile hover actions (reload / shield / keep-awake). */
  async function handleTileAction(r: SearchResult, action: string) {
    if (r.kind !== "account") return;
    setError(null);
    try {
      await invoke("account_window_command", {
        appId: r.app.id,
        accountId: r.account.id,
        action,
      });
      await refreshWindowPins();
    } catch (err) {
      setError(errMsg(err));
    }
  }

  /** v0.18.5: close an account window and reopen it fresh — the
      no-data-loss fix for a stuck or blank window. */
  async function handleFixBlankAccount(app: WebApp, account: Account) {
    setError(null);
    try {
      await invoke("fix_account_window", {
        appId: app.id,
        accountId: account.id,
      });
      await refreshWindowPins();
      setNotice(`Reopened "${account.label}" fresh.`);
    } catch (err) {
      setError(`Could not fix "${account.label}". ${errMsg(err)}`);
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
  // v0.15.0: hoisted so the Ctrl+K palette sees the same
  // workspace-scoped apps as the tile grid.
  const scopedApps = useMemo(
    () => filterAppsByWorkspace(apps, activeWorkspace),
    [apps, activeWorkspace]
  );
  const items = useMemo(() => {
    return query.trim()
      ? buildResults(query, scopedApps, programs, routines, sortOpts)
      : browseAll(scopedApps, programs, sortOpts);
  }, [query, scopedApps, programs, routines, sortOpts]);

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
      onLaunchProgramAdmin: platform?.os === "windows"
        ? async (program) => {
            try {
              await invoke("launch_program_admin", { id: program.id });
            } catch (err) {
              setError(errMsg(err));
            }
          }
        : undefined,
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
    // v0.17.0: Alt+1..9 opens that result directly — the number hints
    // on the tiles name the same chord (e.code: layout-proof digits).
    if (e.altKey && /^Digit[1-9]$/.test(e.code)) {
      e.preventDefault();
      const r = items[Number(e.code.slice(5)) - 1];
      if (r) void activateResult(r);
      return;
    }
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
          get popup blocking, cosmetic ad hiding, and scriptlet blocking.
        </div>
      )}
      {platform && platform.network_adblock && platform.filter_lists_state === "loading" && (
        <div className="banner banner-info" role="status">
          Ad filter lists are still downloading — blocking starts automatically when ready.
        </div>
      )}
      {platform && platform.network_adblock && platform.filter_lists_state === "failed" && (
        <div className="banner banner-info" role="status">
          Ad filter lists couldn't be downloaded — ad blocking is off for now. AppMaka keeps
          retrying in the background.
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
        {paletteOpen && (
          <QuickSwitchPalette
            items={browseAll(scopedApps, programs, sortOpts)}
            onActivate={(r) => {
              setPaletteOpen(false);
              void activateResult(r);
            }}
            onClose={() => setPaletteOpen(false)}
            renderIcon={renderResultIcon}
            playingFor={(r) =>
              r.kind === "account"
                ? playingTitles.get(`${r.app.id}:${r.account.id}`)
                : undefined
            }
          />
        )}
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
                numberHints={query.trim() !== ""}
                activeIndex={activeIndex}
                onHover={setActiveIndex}
                onActivate={(r) => void activateResult(r)}
                renderIcon={renderResultIcon}
                pinnedIds={new Set(launcherSettings?.pinned ?? [])}
                badgeFor={(r) =>
                  (launcherSettings?.show_badges ?? true) && r.kind === "account"
                    ? unreadCounts.get(`${r.app.id}:${r.account.id}`)
                    : undefined
                }
                ramFor={(r) =>
                  (launcherSettings?.show_tile_ram ?? true) && r.kind === "account"
                    ? accountRam.get(`${r.app.id}:${r.account.id}`)
                    : undefined
                }
                sleepingFor={(r) =>
                  (launcherSettings?.show_sleep_labels ?? true) &&
                  r.kind === "account"
                    ? sleeping.has(`${r.app.id}:${r.account.id}`)
                    : false
                }
                suspendedFor={(r) =>
                  (launcherSettings?.show_sleep_labels ?? true) &&
                  r.kind === "account"
                    ? suspendedKeys.has(`${r.app.id}:${r.account.id}`)
                    : false
                }
                playingFor={(r) =>
                  r.kind === "account"
                    ? playingTitles.get(`${r.app.id}:${r.account.id}`)
                    : undefined
                }
                awakeFor={(r) =>
                  r.kind === "account"
                    ? awakeKeys.has(`${r.app.id}:${r.account.id}`)
                    : false
                }
                adblockFor={(r) =>
                  r.kind === "account"
                    ? adblockKeys.has(`${r.app.id}:${r.account.id}`)
                    : false
                }
                openFor={(r) =>
                  r.kind === "account"
                    ? openLabels.has(`acct-${r.app.id}-${r.account.id}`)
                    : false
                }
                onTileAction={(r, action) => void handleTileAction(r, action)}
                onTileContextMenu={(r, x, y) => openTileMenu(r, x, y)}
              />
            )}
            {tileMenuNode}
          </div>
          <footer className="launcher-foot">
            <div className="launcher-foot-primary">
              <button className="text-button" onClick={() => setView("library")}>
                Manage apps
              </button>
              <button
                type="button"
                className="text-button"
                title="Open one window with tabs for several apps"
                onClick={() => void handleOpenTabbedWindow()}
                disabled={openingTabs}
              >
                {openingTabs ? "Opening…" : "Tabbed window"}
              </button>
              <button
                type="button"
                className="text-button"
                title="Reopen windows from your last session"
                onClick={() => void handleRestoreSession()}
              >
                Restore session
              </button>
            </div>
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
        {(() => {
          const total = [...sleeping.values()].reduce((a, b) => a + b, 0);
          return total > 0 && (launcherSettings?.show_sleep_labels ?? true) ? (
            <p className="muted small memory-saved">
              Memory saved: {formatMb(total)} — reclaimed automatically from
              idle apps.
            </p>
          ) : null;
        })()}
        {loading ? (
          <p className="muted">Loading…</p>
        ) : apps.length === 0 ? (
          <p className="muted">No apps yet. Add your first web app above.</p>
        ) : (
          <ul className="app-grid">
            {apps.map((app) => {
              const showSettings = settingsOpen.has(app.id);
              return (
                <li key={app.id} className="app-card">
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
                        onClick={() => setEditingApp(app)}
                      >
                        Edit
                      </button>
                      <button className="danger" onClick={() => void handleRemoveApp(app)}>
                        Remove
                      </button>
                    </div>
                  </div>

                  {/* v0.13.18: accounts are always listed under their app —
                      no expander click to reach the account you want. */}
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
                              unread={
                                (launcherSettings?.show_badges ?? true)
                                  ? unreadCounts.get(`${app.id}:${account.id}`)
                                  : undefined
                              }
                              ramKb={
                                (launcherSettings?.show_tile_ram ?? true)
                                  ? accountRam.get(`${app.id}:${account.id}`)
                                  : undefined
                              }
                              sleeping={
                                (launcherSettings?.show_sleep_labels ?? true) &&
                                sleeping.has(`${app.id}:${account.id}`)
                              }
                              suspended={
                                (launcherSettings?.show_sleep_labels ?? true) &&
                                suspendedKeys.has(`${app.id}:${account.id}`)
                              }
                              onOpen={() => void handleOpenAccount(app, account)}
                              onManage={() => setPanelAccount({ app, account })}
                              onRename={(label) => void handleRenameAccount(app, account, label)}
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
          <h2>User scripts</h2>
        </div>
        <p className="muted small">
          Your own small scripts, run on the pages you choose — a
          Tampermonkey-style header says which sites each one touches.
          Scripts using risky constructs are listed but never run.
        </p>
        <UserScriptsSection />
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

      {panelAccount && (
        <AccountPanel
          app={panelAccount.app}
          account={panelAccount.account}
          onClose={() => setPanelAccount(null)}
          onOpen={() => void handleOpenAccount(panelAccount.app, panelAccount.account)}
          onOpenPrivate={() =>
            void handleOpenAccount(panelAccount.app, panelAccount.account, true, true)
          }
          onFillLogin={() => void handleFillLogin(panelAccount.app, panelAccount.account)}
          onSaveLogin={() =>
            setSaveLoginTarget({ app: panelAccount.app, account: panelAccount.account })
          }
          onSuspend={() =>
            void handleSuspendAccount(panelAccount.app, panelAccount.account)
          }
          onFixBlank={() =>
            void handleFixBlankAccount(panelAccount.app, panelAccount.account)
          }
          onEdit={() =>
            setEditingAccount({ app: panelAccount.app, account: panelAccount.account })
          }
          onForgetLogin={() =>
            setForgetLoginTarget({ app: panelAccount.app, account: panelAccount.account })
          }
          onRemove={() => void handleRemoveAccount(panelAccount.app, panelAccount.account)}
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
