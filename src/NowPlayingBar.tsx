import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { OpenAccountWindow } from "./types";

/**
 * Extend the Now Playing experience with a persistent media controller bar
 * and keyboard media-key handling (`MediaPlayPause`, `MediaTrackNext`,
 * `MediaTrackPrevious`).
 *
 * Shows:
 * - Track or video title (`pageTitle`, falling back to `appName`)
 * - Which web app (and account) is playing
 * - Previous, Play/Pause, and Next controls wired to `account_window_command`
 * - One-click focus to bring the playing app window to the front
 */
export function NowPlayingBar() {
  const [mediaWindows, setMediaWindows] = useState<OpenAccountWindow[]>([]);
  const [selectedLabel, setSelectedLabel] = useState<string | null>(null);
  const [optimisticPlaying, setOptimisticPlaying] = useState<
    Record<string, boolean>
  >({});

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<OpenAccountWindow[]>(
        "list_open_account_windows",
      );
      const active = list.filter(
        (w) =>
          w.appId &&
          w.accountId &&
          (w.playingAudio || Boolean(w.mediaPaused)),
      );
      setMediaWindows(active);
      setOptimisticPlaying((prev) => {
        const next: Record<string, boolean> = {};
        for (const w of active) {
          if (w.label in prev) {
            next[w.label] = prev[w.label];
          }
        }
        return next;
      });
    } catch {
      /* backend unavailable during teardown */
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => {
      void refresh();
    }, 2000);

    let unlisten: (() => void) | undefined;
    void listen("appmaka:media-changed", () => {
      void refresh();
    }).then((fn) => {
      unlisten = fn;
    });

    return () => {
      window.clearInterval(timer);
      if (unlisten) unlisten();
    };
  }, [refresh]);

  const current: OpenAccountWindow | undefined =
    mediaWindows.find((w) => w.label === selectedLabel) ??
    mediaWindows.find((w) => w.playingAudio) ??
    mediaWindows[0];

  const sendMediaCommand = useCallback(
    async (
      target: OpenAccountWindow,
      action: "media_play_pause" | "media_next" | "media_prev",
    ) => {
      if (action === "media_play_pause") {
        const curPlaying =
          optimisticPlaying[target.label] ?? target.playingAudio;
        setOptimisticPlaying((prev) => ({
          ...prev,
          [target.label]: !curPlaying,
        }));
      }
      try {
        const result = await invoke<boolean | null>("account_window_command", {
          appId: target.appId,
          accountId: target.accountId,
          action,
        });
        if (typeof result === "boolean") {
          setOptimisticPlaying((prev) => ({
            ...prev,
            [target.label]: result,
          }));
        }
      } catch {
        /* ignore if window closed */
      } finally {
        window.setTimeout(() => {
          void refresh();
        }, 250);
      }
    },
    [optimisticPlaying, refresh],
  );

  // Handle hardware media keys when the launcher/library window itself has focus.
  useEffect(() => {
    if (!current) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "MediaPlayPause") {
        e.preventDefault();
        void sendMediaCommand(current, "media_play_pause");
      } else if (e.key === "MediaTrackNext") {
        e.preventDefault();
        void sendMediaCommand(current, "media_next");
      } else if (e.key === "MediaTrackPrevious") {
        e.preventDefault();
        void sendMediaCommand(current, "media_prev");
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [current, sendMediaCommand]);

  if (!current) {
    return null;
  }

  const isPlaying = optimisticPlaying[current.label] ?? current.playingAudio;
  const rawTitle = (current.pageTitle || "").trim();
  const trackTitle = rawTitle.length > 0 ? rawTitle : current.appName;
  const appSource =
    current.accountLabel &&
    current.accountLabel.toLowerCase() !== "default" &&
    current.accountLabel.toLowerCase() !== "main"
      ? `${current.appName} · ${current.accountLabel}`
      : current.appName;

  const focusPlayingWindow = async () => {
    try {
      await invoke("open_account", {
        appId: current.appId,
        accountId: current.accountId,
      });
    } catch {
      /* ignore */
    }
  };

  return (
    <aside
      className={`now-playing-bar${isPlaying ? " is-playing" : " is-paused"}`}
      role="region"
      aria-label="Now playing"
    >
      <button
        type="button"
        className="now-playing-bar-info"
        onClick={() => void focusPlayingWindow()}
        title={`Switch to ${appSource}`}
      >
        <span className="now-playing-bar-eq" aria-hidden="true">
          <span />
          <span />
          <span />
        </span>
        <span className="now-playing-bar-text">
          <span className="now-playing-bar-title">{trackTitle}</span>
          <span className="now-playing-bar-app">
            {isPlaying ? "Playing in " : "Paused in "}
            <strong>{appSource}</strong>
          </span>
        </span>
      </button>

      {mediaWindows.length > 1 && (
        <select
          className="now-playing-bar-source"
          aria-label="Select media source window"
          value={current.label}
          onChange={(e) => setSelectedLabel(e.target.value)}
        >
          {mediaWindows.map((w) => (
            <option key={w.label} value={w.label}>
              {w.appName} ({w.pageTitle || w.accountLabel})
            </option>
          ))}
        </select>
      )}

      <div className="now-playing-bar-controls">
        <button
          type="button"
          className="now-playing-bar-btn"
          aria-label="Previous track"
          title="Previous track (Media Previous)"
          onClick={() => void sendMediaCommand(current, "media_prev")}
        >
          ⏮
        </button>
        <button
          type="button"
          className="now-playing-bar-btn is-primary"
          aria-label={isPlaying ? "Pause" : "Play"}
          title={
            isPlaying ? "Pause playback (Media Play/Pause)" : "Resume playback (Media Play/Pause)"
          }
          onClick={() => void sendMediaCommand(current, "media_play_pause")}
        >
          {isPlaying ? "⏸" : "▶"}
        </button>
        <button
          type="button"
          className="now-playing-bar-btn"
          aria-label="Next track"
          title="Next track (Media Next)"
          onClick={() => void sendMediaCommand(current, "media_next")}
        >
          ⏭
        </button>
      </div>
    </aside>
  );
}
