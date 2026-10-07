import { useCallback, useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Tab strip window (v0.10.0, Linux only — Windows paints a native strip).
 *
 * Our own trusted UI (IPC on), docked by the backend directly above the
 * tabbed page window. Reads its group id from its own window label
 * ("tabstrip-{groupId}"), renders tabs from `list_tabbed_windows`, and
 * drives `switch_tab` / `close_tab` / `add_tab` / `close_tabbed_window`.
 *
 * The backend pushes tint updates via `window.__setTint(hex|null)` and
 * nudges re-renders via `window.__refreshTabs()` after non-command
 * changes (e.g. a tab closed from the dashboard).
 *
 * Bundled separately (vite.config.ts "tabstrip" input, loaded by the
 * backend as tabstrip.html).
 */

interface TabSummary {
  appId: string;
  accountId: string;
  appName: string;
  accountLabel: string;
}

interface TabbedWindowInfo {
  groupId: string;
  label: string;
  tabs: TabSummary[];
  active: number;
  tint: string | null;
  focused: boolean;
  pinned: boolean;
}

interface AppAccount {
  appId: string;
  accountId: string;
  display: string;
}

interface LauncherApp {
  id: string;
  name: string;
  accounts: Array<{ id: string; label: string }>;
}

declare global {
  interface Window {
    __refreshTabs?: () => void;
    __setTint?: (hex: string | null) => void;
  }
}

function groupIdFromLabel(label: string): string {
  return label.startsWith("tabstrip-") ? label.slice("tabstrip-".length) : "";
}

function tabDisplay(t: TabSummary): string {
  return t.accountLabel ? `${t.appName} — ${t.accountLabel}` : t.appName;
}

function App() {
  const [groupId] = useState(() => groupIdFromLabel(getCurrentWindow().label));
  const [group, setGroup] = useState<TabbedWindowInfo | null>(null);
  const [tint, setTint] = useState<string | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [apps, setApps] = useState<AppAccount[]>([]);

  const refresh = useCallback(async () => {
    try {
      const groups = await invoke<TabbedWindowInfo[]>("list_tabbed_windows");
      const g = groups.find((x) => x.groupId === groupId) ?? null;
      setGroup(g);
      if (g && g.tint !== undefined) setTint(g.tint);
      if (!g) {
        // The group is gone (closed from elsewhere): close the strip too.
        await getCurrentWindow().close();
      }
    } catch {
      /* backend not ready yet; retry on the next tick */
    }
  }, [groupId]);

  useEffect(() => {
    void refresh();
    const timer = setInterval(() => void refresh(), 2000);
    window.__refreshTabs = () => void refresh();
    window.__setTint = (hex) => setTint(hex);
    return () => {
      clearInterval(timer);
      window.__refreshTabs = undefined;
      window.__setTint = undefined;
    };
  }, [refresh]);

  async function openPicker() {
    if (!pickerOpen) {
      try {
        const list = await invoke<LauncherApp[]>("list_apps");
        const flat: AppAccount[] = [];
        for (const a of list) {
          const multi = a.accounts.length > 1;
          for (const ac of a.accounts) {
            flat.push({
              appId: a.id,
              accountId: ac.id,
              display: multi ? `${a.name} — ${ac.label}` : a.name,
            });
          }
        }
        setApps(flat);
      } catch {
        setApps([]);
      }
    }
    setPickerOpen(!pickerOpen);
  }

  async function run<T>(fn: () => Promise<T>): Promise<T | null> {
    try {
      return await fn();
    } catch {
      return null;
    } finally {
      setPickerOpen(false);
      await refresh();
    }
  }

  if (!group) {
    return <div className="loading">Loading tabs…</div>;
  }

  return (
    <>
      <div className="tabs" role="tablist" aria-label="App tabs">
        {group.tabs.map((t, i) => (
          <div
            key={`${t.appId}:${t.accountId}`}
            className={i === group.active ? "tab active" : "tab"}
            role="tab"
            aria-selected={i === group.active}
          >
            <button
              className="name"
              style={{ all: "unset", flex: "1 1 auto", minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", cursor: "pointer" }}
              onClick={() =>
                void run(() => invoke("switch_tab", { groupId, index: i }))
              }
              title={tabDisplay(t)}
            >
              <span className="name">{tabDisplay(t)}</span>
            </button>
            <button
              className="x"
              aria-label={`Close ${tabDisplay(t)}`}
              onClick={() =>
                void run(() => invoke("close_tab", { groupId, index: i }))
              }
            >
              ×
            </button>
          </div>
        ))}
        <button className="add" aria-label="Add app tab" onClick={() => void openPicker()}>
          +
        </button>
      </div>
      <button
        className="winx"
        aria-label="Close tabbed window"
        onClick={() => void run(() => invoke("close_tabbed_window", { groupId }))}
      >
        ×
      </button>
      {pickerOpen && (
        <div className="picker" role="menu" aria-label="Add app tab">
          {apps.length === 0 && <div className="empty">No apps yet — add one in the launcher first.</div>}
          {apps.map((a) => (
            <button
              key={`${a.appId}:${a.accountId}`}
              role="menuitem"
              onClick={() =>
                void run(() =>
                  invoke("add_tab", {
                    groupId,
                    appId: a.appId,
                    accountId: a.accountId,
                  }),
                )
              }
            >
              {a.display}
            </button>
          ))}
        </div>
      )}
      <style>{tint ? `body { background: ${tint} !important; }` : ""}</style>
    </>
  );
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);
