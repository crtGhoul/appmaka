import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Mirrors the Rust DownloadEntry (camelCased by serde). */
export type DownloadEntry = {
  id: string;
  filename: string;
  url: string;
  state: "active" | "complete" | "failed" | "interrupted";
  receivedBytes: number;
  totalBytes: number | null;
  path: string;
  startedAt: number;
  pickedByUser: boolean;
  note: string | null;
};

/** Mirrors the Rust DownloadSettingsPayload. */
export type DownloadSettings = {
  downloadDir: string;
  askWhereToSave: boolean;
  showCompletionNotice: boolean;
};

function errMsg(err: unknown): string {
  return typeof err === "string" ? err : "Something went wrong.";
}

function fmtBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const mb = bytes / (1024 * 1024);
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  return `${(mb / 1024).toFixed(2)} GB`;
}

function domainOf(url: string): string {
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
}

function fmtTime(startedAt: number): string {
  if (!startedAt) return "";
  return new Date(startedAt).toLocaleString();
}

type DownloadsContextValue = {
  entries: DownloadEntry[];
  settings: DownloadSettings | null;
  refresh: () => void;
  /** Set only on active→complete transitions (never on initial load). */
  lastCompleted: { id: string; filename: string } | null;
};

const DownloadsContext = createContext<DownloadsContextValue>({
  entries: [],
  settings: null,
  refresh: () => {},
  lastCompleted: null,
});

export function useDownloads(): DownloadsContextValue {
  return useContext(DownloadsContext);
}

/**
 * App-wide download state. Mounted once with the app (not with the
 * downloads page) so progress keeps flowing when the page is closed —
 * the v0.7.0 modal dropped every update it wasn't open for.
 */
export function DownloadsProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [entries, setEntries] = useState<DownloadEntry[]>([]);
  const [settings, setSettings] = useState<DownloadSettings | null>(null);
  const [lastCompleted, setLastCompleted] = useState<{
    id: string;
    filename: string;
  } | null>(null);
  const prevStates = useRef(new Map<string, DownloadEntry["state"]>());
  const settingsRef = useRef<DownloadSettings | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [list, s] = await Promise.all([
        invoke<DownloadEntry[]>("list_downloads"),
        invoke<DownloadSettings>("get_download_settings"),
      ]);
      setEntries(list);
      setSettings(s);
      // Seed without completion notices: these are old news.
      const m = new Map<string, DownloadEntry["state"]>();
      for (const e of list) m.set(e.id, e.state);
      prevStates.current = m;
    } catch {
      // Downloads are never the critical path; stay quiet on failure.
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    settingsRef.current = settings;
  }, [settings]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    void listen<DownloadEntry>("appmaka:download-progress", (event) => {
      const entry = event.payload;
      const prev = prevStates.current.get(entry.id);
      prevStates.current.set(entry.id, entry.state);
      setEntries((prevList) => {
        const idx = prevList.findIndex((e) => e.id === entry.id);
        if (idx >= 0) {
          const next = prevList.slice();
          next[idx] = entry;
          return next;
        }
        return [entry, ...prevList];
      });
      // A completion notice only when a live download actually finishes —
      // and only when the toggle is on (default on; the ref may not have
      // loaded yet, in which case the default wins).
      if (
        prev === "active" &&
        entry.state === "complete" &&
        settingsRef.current?.showCompletionNotice !== false
      ) {
        setLastCompleted({ id: entry.id, filename: entry.filename });
      }
      }).then(
        (fn) => {
          unlisten = fn;
        },
        () => {}
      );
    return () => {
      unlisten?.();
    };
  }, []);

  const value = useMemo(
    () => ({ entries, settings, refresh, lastCompleted }),
    [entries, settings, refresh, lastCompleted]
  );
  return (
    <DownloadsContext.Provider value={value}>
      {children}
    </DownloadsContext.Provider>
  );
}

/**
 * Quiet toolbar download button for the launcher footer: an icon that
 * stays subtle when idle and shows live progress while downloading.
 */
export function DownloadToolbarButton({ onOpen }: { onOpen: () => void }) {
  const { entries } = useDownloads();
  const active = entries.filter((e) => e.state === "active");
  const withTotal = active.filter(
    (e) => e.totalBytes != null && e.totalBytes > 0
  );
  // An honest overall figure only when every active download knows its
  // total; otherwise show the count. Never invent a percentage.
  const pct =
    active.length > 0 && withTotal.length === active.length
      ? Math.round(
          (100 * withTotal.reduce((s, e) => s + e.receivedBytes, 0)) /
            withTotal.reduce((s, e) => s + (e.totalBytes ?? 0), 0)
        )
      : null;

  return (
    <button
      type="button"
      className="text-button dl-toolbar-btn"
      onClick={onOpen}
      title={
        active.length > 0
          ? `${active.length} downloading — open downloads`
          : "Downloads"
      }
      aria-label={
        active.length > 0
          ? `Downloads, ${active.length} in progress`
          : "Downloads"
      }
    >
      <svg
        width="16"
        height="16"
        viewBox="0 0 16 16"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden="true"
      >
        <path d="M8 2v8m0 0L4.5 6.5M8 10l3.5-3.5" />
        <path d="M2.5 12.5h11" />
      </svg>
      {active.length > 0 && (
        <span className="dl-toolbar-status">
          {pct != null ? `${pct}%` : `${active.length}`}
        </span>
      )}
    </button>
  );
}

function statusLine(entry: DownloadEntry): string {
  if (entry.state === "complete") {
    const size = fmtBytes(entry.receivedBytes ?? 0);
    return `Complete · ${size}`;
  }
  if (entry.state === "failed") return "Couldn't finish.";
  if (entry.state === "interrupted")
    return "Was still downloading when the app closed.";
  const received = fmtBytes(entry.receivedBytes ?? 0);
  // Sizes are what the download engine reported; when the total is unknown
  // we never invent a percentage.
  if (entry.totalBytes != null && entry.totalBytes > 0) {
    return `Downloading… ${received} of ${fmtBytes(entry.totalBytes)}`;
  }
  return `Downloading… ${received} so far`;
}

/**
 * The downloads page: every download with live progress, persisted
 * history with search, retry, in-place rename, and the download settings.
 * Replaces the v0.7.0 modal.
 */
export function DownloadsPage() {
  const { entries, settings, refresh } = useDownloads();
  const [query, setQuery] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return entries;
    return entries.filter(
      (e) =>
        e.filename.toLowerCase().includes(q) ||
        e.url.toLowerCase().includes(q)
    );
  }, [entries, query]);

  async function run(label: string, fn: () => Promise<unknown>) {
    setError(null);
    try {
      await fn();
      refresh();
    } catch (err) {
      setError(`${label}: ${errMsg(err)}`);
    }
  }

  function changeFolder() {
    void run("Couldn't change the folder", async () => {
      const picked = await invoke<string | null>("pick_download_dir");
      if (picked == null) return; // the user cancelled the picker
      await invoke("set_download_dir", { path: picked });
    });
  }

  function toggleAskWhereToSave(enabled: boolean) {
    void run("Couldn't save the setting", async () => {
      await invoke("set_ask_where_to_save", { enabled });
    });
  }

  function toggleCompletionNotice(enabled: boolean) {
    void run("Couldn't save the setting", async () => {
      await invoke("set_show_completion_notice", { enabled });
    });
  }

  function startRename(entry: DownloadEntry) {
    setRenamingId(entry.id);
    setRenameValue(entry.filename);
  }

  function commitRename(id: string) {
    setRenamingId(null);
    void run("Couldn't rename the file", async () => {
      await invoke("rename_download", { id, filename: renameValue });
    });
  }

  const hasFinished = entries.some((e) => e.state !== "active");

  return (
    <div>
      {error && (
        <p className="modal-error" role="alert" style={{ marginTop: 0 }}>
          {error}
        </p>
      )}

      <section className="panel">
        <div className="panel-head">
          <h2>Download settings</h2>
        </div>
        <div style={{ marginBottom: 10 }}>
          <div className="muted" style={{ marginBottom: 4 }}>
            Download folder
          </div>
          <div
            style={{
              display: "flex",
              gap: 8,
              alignItems: "center",
              flexWrap: "wrap",
            }}
          >
            <span style={{ fontSize: 13, wordBreak: "break-all", flex: 1 }}>
              {settings?.downloadDir ?? "…"}
            </span>
            <button type="button" onClick={changeFolder}>
              Change…
            </button>
          </div>
        </div>
        <label
          style={{
            display: "flex",
            gap: 8,
            alignItems: "center",
            marginBottom: 8,
            fontSize: 14,
          }}
        >
          <input
            type="checkbox"
            checked={settings?.askWhereToSave ?? false}
            onChange={(e) => toggleAskWhereToSave(e.target.checked)}
          />
          Ask where to save each file before downloading
        </label>
        <label
          style={{ display: "flex", gap: 8, alignItems: "center", fontSize: 14 }}
        >
          <input
            type="checkbox"
            checked={settings?.showCompletionNotice ?? true}
            onChange={(e) => toggleCompletionNotice(e.target.checked)}
          />
          Show a notice when downloads finish
        </label>
      </section>

      <section className="panel">
        <div className="panel-head">
          <h2>History</h2>
        </div>
        <p className="muted small" style={{ marginTop: 0 }}>
          Remove only clears the list — your files stay where they are.
        </p>
        <input
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search downloads"
          aria-label="Search downloads"
          style={{ width: "100%", marginBottom: 8 }}
        />

        {entries.length === 0 ? (
          <p className="muted">
            No downloads yet. Files you download in AppMaka will show up
            here.
          </p>
        ) : filtered.length === 0 ? (
          <p className="muted">No downloads match “{query.trim()}”.</p>
        ) : (
          <ul style={{ listStyle: "none", padding: 0, margin: 0 }}>
            {filtered.map((entry) => {
              const pct =
                entry.state === "active" &&
                entry.totalBytes != null &&
                entry.totalBytes > 0
                  ? Math.min(
                      100,
                      Math.round(
                        (100 * entry.receivedBytes) / entry.totalBytes
                      )
                    )
                  : null;
              const renaming = renamingId === entry.id;
              return (
                <li
                  key={entry.id}
                  style={{
                    padding: "10px 0",
                    borderTop: "1px solid rgba(128,128,128,0.25)",
                  }}
                >
                  {renaming ? (
                    <input
                      type="text"
                      value={renameValue}
                      autoFocus
                      onFocus={(e) => e.target.select()}
                      onChange={(e) => setRenameValue(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") commitRename(entry.id);
                        if (e.key === "Escape") setRenamingId(null);
                      }}
                      onBlur={() => setRenamingId(null)}
                      aria-label="New file name"
                      style={{ width: "100%", marginBottom: 4 }}
                    />
                  ) : (
                    <button
                      type="button"
                      className="text-button"
                      style={{
                        padding: 0,
                        fontSize: 14,
                        fontWeight: 600,
                        textAlign: "left",
                        wordBreak: "break-all",
                      }}
                      title="Rename"
                      onClick={() => startRename(entry)}
                    >
                      {entry.filename}
                    </button>
                  )}
                  {entry.state === "active" && (
                    <div
                      className="progress-track"
                      role="progressbar"
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={pct ?? undefined}
                      aria-label={`Download progress for ${entry.filename}`}
                      style={{ margin: "6px 0" }}
                    >
                      <div
                        className="progress-fill"
                        style={{ width: pct != null ? `${pct}%` : "100%", opacity: pct != null ? 1 : 0.35 }}
                      />
                    </div>
                  )}
                  <div
                    className="muted"
                    style={{ fontSize: 12, margin: "2px 0" }}
                  >
                    {statusLine(entry)}
                    {entry.note && (
                      <span style={{ display: "block" }}>{entry.note}</span>
                    )}
                  </div>
                  <div
                    className="muted small"
                    style={{ wordBreak: "break-all", marginBottom: 6 }}
                    title={entry.url}
                  >
                    {domainOf(entry.url)}
                    {fmtTime(entry.startedAt) && ` · ${fmtTime(entry.startedAt)}`}
                  </div>
                  <div
                    className="muted small"
                    style={{ wordBreak: "break-all", marginBottom: 6 }}
                  >
                    {entry.url}
                  </div>
                  <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
                    {entry.state === "complete" && (
                      <>
                        <button
                          type="button"
                          onClick={() =>
                            run("Couldn't open the file", async () => {
                              await invoke("open_download", { id: entry.id });
                            })
                          }
                        >
                          Open
                        </button>
                        <button
                          type="button"
                          onClick={() =>
                            run("Couldn't show the file", async () => {
                              await invoke("show_in_folder", {
                                id: entry.id,
                              });
                            })
                          }
                        >
                          Show in folder
                        </button>
                      </>
                    )}
                    {(entry.state === "failed" ||
                      entry.state === "interrupted") && (
                      <button
                        type="button"
                        onClick={() =>
                          run("Couldn't retry the download", async () => {
                            await invoke("retry_download", { id: entry.id });
                          })
                        }
                      >
                        Retry
                      </button>
                    )}
                    <button
                      type="button"
                      className="text-button"
                      onClick={() =>
                        run("Couldn't remove the download", async () => {
                          await invoke("remove_download", { id: entry.id });
                        })
                      }
                    >
                      Remove
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        )}

        <div style={{ display: "flex", gap: 8, marginTop: 12 }}>
          <button
            type="button"
            onClick={() =>
              run("Couldn't clear the list", async () => {
                await invoke("clear_finished");
              })
            }
            disabled={!hasFinished}
          >
            Clear finished
          </button>
        </div>
      </section>
    </div>
  );
}
