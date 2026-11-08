import { useCallback, useEffect, useRef, useState } from "react";
import ReactDOM from "react-dom/client";
import "./tauriBrowserShim";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * Split bar (v0.18.8): the one window's top strip. The split is a
 * single frame window now — this bar is its first child webview, so
 * `getCurrentWindow()` is the frame itself: the drag region moves it,
 * and the minimize / maximize / close buttons act on it.
 *
 * Our own trusted UI (IPC on), like the tab strip. Reads its group id
 * from its own window label ("split-{groupId}"), renders the group's
 * panes and the layouts that fit its pane count, and drives
 * `focus_split_pane` / `set_split_layout` / `remove_split_pane` /
 * `close_split_window`. Refreshes on the `appmaka:split-changed` event
 * and disappears with the group — a missing group renders nothing.
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
  activePane: number;
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
  // A child webview's current window is its parent frame ("split-…");
  // the webview's own label ("splitbar-…") is the fallback. Check the
  // webview label first — it also starts with "split-".
  if (label.startsWith("splitbar-")) return label.slice("splitbar-".length);
  if (label.startsWith("split-")) return label.slice("split-".length);
  return "";
}

function paneDisplay(p: SplitPaneInfo): string {
  return p.accountLabel ? `${p.appName} — ${p.accountLabel}` : p.appName;
}

function App() {
  const [groupId] = useState(() => groupIdFromLabel(getCurrentWindow().label));
  const [group, setGroup] = useState<SplitInfo | null>(null);
  const [maximized, setMaximized] = useState(false);
  // v0.18.10: drag a pane to swap positions, or drag it out of the bar
  // into its own window. The ref carries the source index into dragend
  // (state may already have cleared by then); dragOver is the visual
  // drop target.
  const dragFromRef = useRef<number | null>(null);
  const [dragFrom, setDragFrom] = useState<number | null>(null);
  const [dragOver, setDragOver] = useState<number | null>(null);

  const clearDrag = useCallback(() => {
    dragFromRef.current = null;
    setDragFrom(null);
    setDragOver(null);
  }, []);

  const popOut = useCallback(
    async (index: number) => {
      try {
        await invoke("pop_out_split_pane", { groupId, index });
      } catch {
        /* already gone */
      }
    },
    [groupId],
  );

  const onDragEnd = useCallback(
    (e: React.DragEvent) => {
      const from = dragFromRef.current;
      clearDrag();
      if (from == null || e.dataTransfer.dropEffect !== "none") return;
      // Dropped outside any chip. Inside the bar's own empty space is
      // a cancelled drag, not a pop-out: only a pointer outside the
      // 36px bar (or 0,0, which is how leaving the window reports)
      // pops the pane out.
      const { clientX, clientY } = e;
      const outside =
        (clientX === 0 && clientY === 0) ||
        clientX < 0 ||
        clientY < 0 ||
        clientX > window.innerWidth ||
        clientY > window.innerHeight;
      if (outside) void popOut(from);
    },
    [clearDrag, popOut],
  );

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<SplitList>("list_splits");
      setGroup(list.open.find((g) => g.id === groupId) ?? null);
      setMaximized(await getCurrentWindow().isMaximized());
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
  const win = getCurrentWindow();
  return (
    <div className="bar" data-tauri-drag-region>
      <div className="panes">
        {group.panes.map((p, i) => (
          <span
            key={`${p.appId}/${p.accountId}`}
            className={`pane-wrap${i === group.activePane ? " active" : ""}${dragFrom === i ? " dragging" : ""}${dragOver === i ? " drag-over" : ""}`}
            draggable
            onDragStart={(e) => {
              dragFromRef.current = i;
              setDragFrom(i);
              e.dataTransfer.effectAllowed = "move";
              e.dataTransfer.setData("text/plain", String(i));
            }}
            onDragOver={(e) => {
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
              setDragOver(i);
            }}
            onDragLeave={() => {
              setDragOver((prev) => (prev === i ? null : prev));
            }}
            onDrop={(e) => {
              e.preventDefault();
              const from = dragFromRef.current;
              clearDrag();
              if (from != null && from !== i) {
                void invoke("move_split_pane", {
                  groupId,
                  from,
                  to: i,
                }).catch(() => undefined);
              }
            }}
            onDragEnd={onDragEnd}
          >
            <button
              type="button"
              className="pane"
              title={`${paneDisplay(p)} — drag to reorder, drag out to pop out`}
              onClick={() =>
                void invoke("focus_split_pane", { groupId, index: i }).catch(
                  () => undefined,
                )
              }
            >
              {paneDisplay(p)}
            </button>
            <button
              type="button"
              className="pane-popout"
              title="Open in its own window"
              aria-label={`Open ${paneDisplay(p)} in its own window`}
              onClick={() => void popOut(i)}
            >
              ⧉
            </button>
            <button
              type="button"
              className="pane-close"
              title="Close this pane"
              aria-label={`Close ${paneDisplay(p)}`}
              onClick={() =>
                void invoke("remove_split_pane", { groupId, index: i }).catch(
                  () => undefined,
                )
              }
            >
              ×
            </button>
          </span>
        ))}
        {group.panes.length < 4 && (
          <button
            type="button"
            className="pane-add"
            title="Add an app to this split view"
            aria-label="Add an app to this split view"
            onClick={() =>
              void emit("appmaka:add-pane-to-split", { groupId })
            }
          >
            +
          </button>
        )}
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
      <span className="sep" />
      <div className="win-controls">
        <button
          type="button"
          className="win-btn"
          title="Minimize"
          aria-label="Minimize"
          onClick={() => void win.minimize().catch(() => undefined)}
        >
          —
        </button>
        <button
          type="button"
          className="win-btn"
          title={maximized ? "Restore" : "Maximize"}
          aria-label={maximized ? "Restore" : "Maximize"}
          onClick={() =>
            void win
              .toggleMaximize()
              .then(() => void refresh())
              .catch(() => undefined)
          }
        >
          {maximized ? "❐" : "▢"}
        </button>
        <button
          type="button"
          className="win-btn win-close"
          title="Close"
          aria-label="Close"
          onClick={() => void closeAll()}
        >
          ×
        </button>
      </div>
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(<App />);
