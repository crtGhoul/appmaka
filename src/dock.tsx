import { useCallback, useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  Account,
  LauncherSettings,
  NativeProgram,
  WebApp,
} from "./types";
import {
  browseAll,
  recordLaunch,
  resolveWorkspaceTargets,
  ProgramIcon,
  SearchTileIcon,
  WorkspaceTileIcon,
  type SearchResult,
} from "./Launcher";
import "./App.css";

/**
 * Workspace dock window (v0.17.0). A small floating bar of the user's
 * pinned tiles on one screen edge; the backend (dock.rs) owns the
 * window's placement and lifecycle, this page only renders and opens.
 *
 * Contents are exactly the launcher's pinned set, in pin order — pin
 * something in the launcher and it shows up here. Data comes from the
 * same invokes the launcher uses; the page refreshes when the backend
 * or the settings panel says pins/apps/workspaces changed, and when
 * the window regains focus.
 *
 * Bundled separately (vite.config.ts "dock" input, loaded by the
 * backend as dock.html).
 */

// Minimal shape of the list_workspaces result — only what the dock
// reads. The full Workspace type lives in WorkspacesSection.tsx.
interface WorkspaceShape {
  id: string;
  name: string;
  members: Array<{ app_id: string; account_id: string | null }>;
}
interface WorkspaceListShape {
  workspaces: WorkspaceShape[];
}

function errMsg(err: unknown): string {
  return typeof err === "string" ? err : "Something went wrong.";
}

/** The app's color if it is a plain hex value, else the default accent. */
function safeColor(color: string | undefined): string {
  return color && /^#[0-9a-fA-F]{6}$/.test(color) ? color : "#6366f1";
}

/** The site's own /favicon.ico, the launcher icon's fallback source. */
function faviconUrl(appUrl: string): string | null {
  try {
    return new URL("/favicon.ico", appUrl).toString();
  } catch {
    return null;
  }
}

function DockAppIcon({ app }: { app: WebApp }) {
  // Same fallback chain as the launcher's AppIcon: cached icon file,
  // then the site's live favicon, then a letter tile.
  const [stage, setStage] = useState(0);
  const src =
    stage === 0 && app.icon
      ? convertFileSrc(app.icon)
      : stage <= 1
        ? faviconUrl(app.url)
        : null;
  if (!src) {
    return (
      <span
        className="app-icon-fallback"
        aria-hidden="true"
        style={{
          backgroundColor: `${safeColor(app.color)}22`,
          color: safeColor(app.color),
        }}
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
      onError={() => setStage((s) => s + 1)}
    />
  );
}

function DockAccountThumb({ account, app }: { account: Account; app: WebApp }) {
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

function tileIcon(r: SearchResult): React.ReactNode {
  if (r.kind === "program") return <ProgramIcon program={r.program} />;
  if (r.kind === "search") return <SearchTileIcon />;
  if (r.kind === "workspace") return <WorkspaceTileIcon />;
  if (r.kind === "account")
    return <DockAccountThumb account={r.account} app={r.app} />;
  if (r.kind === "app") return <DockAppIcon app={r.app} />;
  // Routines are never pinned, so browseAll never hands the dock one;
  // the type still allows it, so fall back to a letter tile.
  return (
    <span className="app-icon-fallback" aria-hidden="true">
      {r.title.charAt(0).toUpperCase() || "?"}
    </span>
  );
}

function Dock() {
  const [settings, setSettings] = useState<LauncherSettings | null>(null);
  const [apps, setApps] = useState<WebApp[]>([]);
  const [programs, setPrograms] = useState<NativeProgram[]>([]);
  const [workspaces, setWorkspaces] = useState<WorkspaceShape[]>([]);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [s, a, p, w] = await Promise.all([
        invoke<LauncherSettings>("get_launcher_settings"),
        invoke<WebApp[]>("list_apps"),
        invoke<NativeProgram[]>("list_programs"),
        invoke<WorkspaceListShape>("list_workspaces"),
      ]);
      setSettings(s);
      setApps(a);
      setPrograms(p);
      setWorkspaces(w.workspaces ?? []);
      // The panel's light/dark follows the saved theme, like the
      // launcher: "system" resolves against the OS preference.
      const theme = s.theme ?? "system";
      document.documentElement.dataset.theme =
        theme === "system"
          ? window.matchMedia("(prefers-color-scheme: dark)").matches
            ? "dark"
            : "light"
          : theme;
    } catch {
      /* A failed load leaves the previous tiles up; the next refresh
         (pin change, focus) retries. */
    }
  }, []);

  useEffect(() => {
    void load();
    const unlisteners = [
      listen("appmaka:pins-changed", () => void load()),
      listen("appmaka:apps-changed", () => void load()),
      listen("appmaka:workspace-changed", () => void load()),
      listen("appmaka:dock-settings-changed", () => void load()),
    ];
    const onFocus = () => void load();
    window.addEventListener("focus", onFocus);
    return () => {
      window.removeEventListener("focus", onFocus);
      for (const p of unlisteners) void p.then((un) => un());
    };
  }, [load]);

  async function openAccount(app: WebApp, account: Account) {
    await invoke("open_account", {
      appId: app.id,
      accountId: account.id,
      isPrivate: false,
    });
  }

  async function activate(r: SearchResult) {
    setError(null);
    try {
      if (r.kind === "program") {
        await invoke("launch_program", { id: r.program.id });
      } else if (r.kind === "account") {
        await openAccount(r.app, r.account);
      } else if (r.kind === "search") {
        await invoke("open_web_search", { query: r.query });
      } else if (r.kind === "workspace") {
        const ws = workspaces.find((w) => w.id === r.workspaceId);
        if (ws) {
          for (const { app, account } of resolveWorkspaceTargets(
            ws.members,
            apps
          )) {
            await openAccount(app, account);
          }
        }
      } else if (r.kind === "app") {
        // App row: open its most recently used account, like the launcher.
        const acct =
          [...r.app.accounts].sort(
            (a, b) => (b.last_opened ?? 0) - (a.last_opened ?? 0)
          )[0] ?? r.app.accounts[0];
        if (!acct) return;
        await openAccount(r.app, acct);
      }
      recordLaunch(r.id);
    } catch (err) {
      setError(errMsg(err));
    }
  }

  async function hideDock() {
    try {
      await invoke("set_dock_enabled", { enabled: false });
    } catch (err) {
      setError(errMsg(err));
    }
  }

  const pinned = settings?.pinned ?? [];
  const pinnedSet = new Set(pinned);
  const items = browseAll(apps, programs, {
    pinned,
    usage: settings?.usage,
    hiddenProgramIds: settings?.hidden_programs,
    workspaces,
  }).filter((r) => pinnedSet.has(r.id));
  const edge = settings?.dock_edge ?? "bottom";

  return (
    <div id="root-inner" className={`dock-root edge-${edge}`}>
      <div className="dock-bar" role="toolbar" aria-label="Workspace dock">
        {error ? (
          <span className="dock-error" role="alert">
            {error}
          </span>
        ) : items.length === 0 ? (
          <span className="dock-empty">
            Nothing pinned yet. Pin tiles in the launcher and they will
            live here.
          </span>
        ) : (
          items.map((r) => (
            <button
              key={r.id}
              type="button"
              className="icon-tile"
              title={r.kind === "account" ? `${r.title} — ${r.context}` : r.title}
              onClick={() => void activate(r)}
            >
              <span className="tile-icon">{tileIcon(r)}</span>
              <span className="tile-label">{r.title}</span>
              {r.kind === "account" && (
                <span className="tile-sub">{r.context}</span>
              )}
            </button>
          ))
        )}
        <button
          type="button"
          className="dock-hide"
          title="Turn the dock off (Launcher settings turns it back on)"
          onClick={() => void hideDock()}
        >
          Hide
        </button>
      </div>
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(<Dock />);
