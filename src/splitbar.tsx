import { useCallback, useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import "./tauriBrowserShim";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Split bar (v0.18.7): group controls docked above a split view's panes.
 *
 * Our own trusted UI (IPC on), like the tab strip. Reads its group id
 * from its own window label ("splitbar-{groupId}"), renders the group's
 * panes and the layouts that fit its pane count, and drives
 * `focus_split_pane` / `set_split_layout` / `remove_split_pane` /
 * `close_split_window`. Refreshes on the `appmaka:split-changed` event
 * (a pane closed from its own X reflows the group) and disappears with
 * the group — a missing group renders nothing.
 *
 * Bundled separately (vite.config.ts "splitbar" input, loaded by the
 * backend as splitbar.html).
 */

type SplitLayout = "vertical" | "horizontal" | "three" | "four";

interface SplitPaneInfo {
  appId: string;
  accountId: string;
  appName: string;
  accountLabel: string;
}

interface SplitInfo {
  id: string;
  layout: SplitLayout;
  panes: SplitPaneInfo[];
  frame: { x: number; y: number; w: number; h: number };
}

interface SplitList {
  layouts: unknown[];
  open: SplitInfo[];
}

const LAYOUT_LABELS: Record<SplitLayout, string> = {
  vertical: "Side by side",
  horizontal: "Stacked",
  three: "Three",
  four: "Four",
};

function layoutsForCount(count: number): SplitLayout[] {
  if (count === 2) return ["vertical", "horizontal"];
  if (count === 3) return ["three"];
  if (count === 4) return ["four"];
  return [];
}

function groupIdFromLabel(label: string): string {
  return label.startsWith("splitbar-") ? label.slice("splitbar-".length) : "";
}

function paneDisplay(p: SplitPaneInfo): string {
  return p.accountLabel ? `${p.appName} — ${p.accountLabel}` : p.appName;
}

function App() {
  const [groupId] = useState(() => groupIdFromLabel(getCurrentWindow().label));
  const [group, setGroup] = useState<SplitInfo | null>(null);

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<SplitList>("list_splits");
      setGroup(list.open.find((g) => g.id === groupId) ?? null);
    } catch {
      /* the group is gone; render nothing */
    }
  }, [groupId]);

  useEffect(() => {
    void refresh();
    const unlisten = listen("appmaka:split-changed", () => void refresh());
    return () => {
      void unlisten.then((f) => f());
    };
  }, [refresh]);

  const closeAll = useCallback(async () => {
    try {
      await invoke("close_split_window", { groupId });
    } catch {
      /* already gone */
    }
  }, [groupId]);

  if (!group) {
    return <span className="loading">Split view</span>;
  }
  const layouts = layoutsForCount(group.panes.length);
  return (
    <>
      <div className="panes">
        {group.panes.map((p, i) => (
          <button
            key={`${p.appId}/${p.accountId}`}
            type="button"
            className="pane"
            title={paneDisplay(p)}
            onClick={() =>
              void invoke("focus_split_pane", { groupId, index: i }).catch(
                () => undefined,
              )
            }
          >
            {paneDisplay(p)}
          </button>
        ))}
      </div>
      {layouts.length > 1 && <span className="sep" />}
      {layouts.map((l) => (
        <button
          key={l}
          type="button"
          className={`layout${group.layout === l ? " active" : ""}`}
          onClick={() =>
            void invoke("set_split_layout", { groupId, layout: l }).catch(
              () => undefined,
            )
          }
        >
          {LAYOUT_LABELS[l]}
        </button>
      ))}
      <span className="sep" />
      <button type="button" className="close" onClick={() => void closeAll()}>
        Close all
      </button>
    </>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
