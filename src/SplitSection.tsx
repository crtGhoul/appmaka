import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { WebApp } from "./types";

/**
 * Split view: up to four apps side by side in one window (v0.18.7).
 *
 * Backend contract (src-tauri/src/split.rs — do not extend):
 *   invoke("list_splits") -> SplitList
 *   invoke("open_split_window", { panes, layout }) -> SplitInfo
 *   invoke("open_split_layout", { layoutId }) -> SplitInfo
 *   invoke("close_split_window", { groupId }) -> void
 *   invoke("save_split_layout", { name, groupId }) -> SplitList
 *   invoke("delete_split_layout", { layoutId }) -> SplitList
 *
 * IMPORTANT — invoke argument naming: Rust `layout_id` becomes
 * `layoutId` in JS (tsc cannot catch a mismatch; it fails at runtime
 * with "missing required key layoutId"). Struct FIELDS stay snake_case
 * in JSON (serde default, no rename): `app_id`, `account_id`.
 *
 * Layout is a function of pane count, never a free choice: two apps are
 * side by side or stacked, three are one tall plus two stacked, four
 * are a 2x2 grid.
 */

type SplitLayout = "vertical" | "horizontal" | "three" | "four";

interface SplitPane {
  app_id: string;
  account_id: string;
}

interface SplitPaneInfo extends SplitPane {
  appName: string;
  accountLabel: string;
}

interface SplitInfo {
  id: string;
  layout: SplitLayout;
  panes: SplitPaneInfo[];
  frame: { x: number; y: number; w: number; h: number };
}

interface SavedSplitLayout {
  id: string;
  name: string;
  layout: SplitLayout;
  panes: SplitPane[];
}

interface SplitList {
  layouts: SavedSplitLayout[];
  open: SplitInfo[];
}

interface SplitSectionProps {
  apps: WebApp[];
  onError: (msg: string) => void;
}

function paneDisplay(p: { appName: string; accountLabel: string }): string {
  return p.accountLabel ? `${p.appName} — ${p.accountLabel}` : p.appName;
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export default function SplitSection({ apps, onError }: SplitSectionProps) {
  const [list, setList] = useState<SplitList | null>(null);
  const [selected, setSelected] = useState<SplitPane[]>([]);
  const [layout, setLayout] = useState<SplitLayout>("vertical");
  const [saveNames, setSaveNames] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    void invoke<SplitList>("list_splits")
      .then((l) => {
        if (alive) setList(l);
      })
      .catch(() => undefined);
    const unlisten = listen<SplitList>("appmaka:split-changed", (e) => {
      if (alive) setList(e.payload);
    });
    return () => {
      alive = false;
      void unlisten.then((f) => f());
    };
  }, []);

  const togglePane = (pane: SplitPane) => {
    setSelected((prev) => {
      const at = prev.findIndex(
        (p) => p.app_id === pane.app_id && p.account_id === pane.account_id,
      );
      if (at >= 0) {
        const next = prev.filter((_, i) => i !== at);
        if (next.length < 2) setLayout("vertical");
        return next;
      }
      if (prev.length >= 4) return prev;
      return [...prev, pane];
    });
  };

  const openSplit = async () => {
    if (selected.length < 2 || busy) return;
    setBusy(true);
    try {
      await invoke("open_split_window", {
        panes: selected,
        layout: selected.length === 2 ? layout : null,
      });
      setSelected([]);
      setLayout("vertical");
      const updated = await invoke<SplitList>("list_splits");
      setList(updated);
    } catch (err) {
      onError(errMsg(err));
    } finally {
      setBusy(false);
    }
  };

  const saveLayout = async (groupId: string) => {
    const name = (saveNames[groupId] ?? "").trim();
    if (!name) return;
    try {
      const updated = await invoke<SplitList>("save_split_layout", {
        name,
        groupId,
      });
      setList(updated);
      setSaveNames((prev) => ({ ...prev, [groupId]: "" }));
    } catch (err) {
      onError(errMsg(err));
    }
  };

  return (
    <div>
      <p className="muted">
        Open two to four apps side by side in one window. Each pane keeps
        its own login; drag the bar at the top to move them all together.
      </p>

      <div>
        {apps.flatMap((app) =>
          app.accounts.map((account) => {
            const pane = { app_id: app.id, account_id: account.id };
            const checked = selected.some(
              (p) =>
                p.app_id === pane.app_id && p.account_id === pane.account_id,
            );
            return (
              <label key={`${app.id}/${account.id}`} className="inline-form">
                <input
                  type="checkbox"
                  checked={checked}
                  disabled={!checked && selected.length >= 4}
                  onChange={() => togglePane(pane)}
                  aria-label={`${app.name}${account.label ? ` ${account.label}` : ""}`}
                />
                <span>
                  {app.name}
                  {account.label ? ` — ${account.label}` : ""}
                </span>
              </label>
            );
          }),
        )}
        {apps.length === 0 && (
          <p className="muted">No apps yet — add one above first.</p>
        )}
      </div>

      {selected.length === 2 && (
        <label className="inline-form">
          <span>Layout</span>
          <select
            value={layout}
            onChange={(e) => setLayout(e.target.value as SplitLayout)}
            aria-label="Split layout"
          >
            <option value="vertical">Side by side</option>
            <option value="horizontal">Stacked</option>
          </select>
        </label>
      )}

      <button
        type="button"
        disabled={selected.length < 2 || busy}
        onClick={() => void openSplit()}
      >
        Open split ({selected.length} selected)
      </button>

      {list && list.layouts.length > 0 && (
        <div>
          <h3>Saved layouts</h3>
          <ul>
            {list.layouts.map((saved) => (
              <li key={saved.id}>
                <span>{saved.name}</span>{" "}
                <button
                  type="button"
                  onClick={() =>
                    void invoke("open_split_layout", {
                      layoutId: saved.id,
                    }).catch((err: unknown) => onError(errMsg(err)))
                  }
                >
                  Open
                </button>{" "}
                <button
                  type="button"
                  onClick={() =>
                    void invoke<SplitList>("delete_split_layout", {
                      layoutId: saved.id,
                    })
                      .then(setList)
                      .catch((err: unknown) => onError(errMsg(err)))
                  }
                >
                  Delete
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      {list && list.open.length > 0 && (
        <div>
          <h3>Open splits</h3>
          <ul>
            {list.open.map((group) => (
              <li key={group.id}>
                <span>
                  {group.panes.map(paneDisplay).join(" · ")}
                </span>{" "}
                <input
                  type="text"
                  placeholder="Layout name"
                  value={saveNames[group.id] ?? ""}
                  onChange={(e) =>
                    setSaveNames((prev) => ({
                      ...prev,
                      [group.id]: e.target.value,
                    }))
                  }
                  aria-label="Layout name"
                />{" "}
                <button
                  type="button"
                  disabled={!(saveNames[group.id] ?? "").trim()}
                  onClick={() => void saveLayout(group.id)}
                >
                  Save layout
                </button>{" "}
                <button
                  type="button"
                  onClick={() =>
                    void invoke("close_split_window", {
                      groupId: group.id,
                    }).catch((err: unknown) => onError(errMsg(err)))
                  }
                >
                  Close
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
