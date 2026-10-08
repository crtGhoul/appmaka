import { forwardRef, useCallback, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { evaluateExpression, formatCalcResult } from "./calc";
import { HotkeyCapture } from "./HotkeyCapture";
import type {
  Account,
  HotkeyStatus,
  LauncherSettings,
  NativeProgram,
  Routine,
  WebApp,
} from "./types";

function errMsg(err: unknown): string {
  return typeof err === "string" ? err : "Something went wrong.";
}

function hostOf(appUrl: string): string {
  try {
    return new URL(appUrl).hostname;
  } catch {
    return appUrl;
  }
}

/**
 * Subsequence fuzzy score. Higher is better, 0 means no match. Rewards
 * word-start matches and consecutive runs; shorter names win ties.
 */
export function fuzzyScore(query: string, text: string): number {
  const q = query.toLowerCase().trim();
  const t = text.toLowerCase();
  if (!q) return 0;
  let score = 0;
  let ti = 0;
  let lastMatch = -2;
  for (let qi = 0; qi < q.length; qi++) {
    const found = t.indexOf(q[qi], ti);
    if (found === -1) return 0;
    if (found === 0 || t[found - 1] === " " || t[found - 1] === "-" || t[found - 1] === "_") {
      score += 3;
    } else if (found === lastMatch + 1) {
      score += 2;
    } else {
      score += 1;
    }
    lastMatch = found;
    ti = found + 1;
  }
  score += Math.max(0, 20 - t.length);
  return score;
}

export type SearchResult =
  | { kind: "account"; id: string; title: string; context: string; score: number; app: WebApp; account: Account }
  | { kind: "app"; id: string; title: string; context: string; score: number; app: WebApp }
  | { kind: "program"; id: string; title: string; context: string; score: number; program: NativeProgram }
  | { kind: "routine"; id: string; title: string; context: string; score: number; routine: Routine }
  | { kind: "search"; id: string; title: string; context: string; score: number; query: string }
  | { kind: "workspace"; id: string; title: string; context: string; score: number; workspaceId: string };

/**
 * v0.9.3: pinned web searches. The tag is `search:<url-encoded query>` —
 * it rides the same pinned-id machinery as app:/account:/program: tags
 * (pin order, usage ranking, toggle_pin), and the tile re-runs the query
 * in the shared in-app search window.
 */
export function searchTag(query: string): string {
  return `search:${encodeURIComponent(query)}`;
}

/** Inverse of `searchTag`; null when the id is not a search tag. */
export function parseSearchTag(id: string): string | null {
  if (!id.startsWith("search:")) return null;
  try {
    return decodeURIComponent(id.slice("search:".length));
  } catch {
    return null;
  }
}

/**
 * v0.9.6: pinned workspaces. The tag is `workspace:<id>` — it rides the same
 * pinned-id machinery as app:/account:/program:/search: (pin order, usage
 * ranking, toggle_pin), and the tile opens every member window at once.
 */
export function workspaceTag(id: string): string {
  return `workspace:${id}`;
}

/** Inverse of `workspaceTag`; null when the id is not a workspace tag. */
export function parseWorkspaceTag(id: string): string | null {
  if (!id.startsWith("workspace:")) return null;
  return id.slice("workspace:".length);
}

/**
 * v0.9.6: resolve a pinned workspace's members to concrete (app, account)
 * open targets. A member naming an account opens just that account; a
 * whole-app member (`account_id` null) opens every account the app
 * currently has. Missing apps or accounts are skipped silently.
 */
export function resolveWorkspaceTargets<
  A extends { id: string; accounts: Array<{ id: string }> },
>(
  members: ReadonlyArray<{ app_id: string; account_id: string | null }>,
  apps: ReadonlyArray<A>
): Array<{ app: A; account: A["accounts"][number] }> {
  const targets: Array<{ app: A; account: A["accounts"][number] }> = [];
  for (const m of members) {
    const app = apps.find((a) => a.id === m.app_id);
    if (!app) continue;
    if (m.account_id) {
      const account = app.accounts.find((a) => a.id === m.account_id);
      if (account) targets.push({ app, account });
    } else {
      for (const account of app.accounts) targets.push({ app, account });
    }
  }
  return targets;
}

/**
 * Columns in the launcher folder grid. Must match `grid-template-columns`
 * in App.css — keyboard navigation moves by this many rows per Up/Down.
 */
export const GRID_COLUMNS = 6;

/** Max tiles shown when browsing with an empty query. Search caps at 25. */
const BROWSE_LIMIT = 48;

// ---------------------------------------------------------------------------
// v0.6.0: launcher settings fields that the backend now returns but
// types.ts does not declare yet (owned by another worker). Kept local here
// so this file compiles without touching ./types.
// ---------------------------------------------------------------------------

/** One entry of the launch-usage map: how often / how recently an item launched. */
export interface UsageEntry {
  count: number;
  last_used: number;
}

/**
 * Optional sorting/filtering input for browseAll / buildResults. All
 * optional: when omitted, the previous plain ordering is kept, so existing
 * callers (App.tsx) keep working unchanged.
 */
export interface LauncherSortOpts {
  /** Tagged ids ("app:<id>", "account:<id>", "program:<id>") in pin order. */
  pinned?: string[];
  /** Launch stats keyed by tagged id. */
  usage?: Record<string, UsageEntry>;
  /** Program ids the user hid from the launcher. */
  hiddenProgramIds?: string[];
  /**
   * v0.9.6: workspaces for materializing pinned workspace tiles. Minimal
   * shape on purpose — the full Workspace type lives in
   * WorkspacesSection.tsx and importing it here would cycle.
   */
  workspaces?: Array<{ id: string; name: string }>;
}

/** A program the user added manually (add_custom_program result). */
export interface CustomProgram {
  id: string;
  name: string;
  exe_path: string;
  icon_path: string | null;
}

/**
 * Manually added programs carry ids like "custom-<hash>" (see the backend
 * contract). `is_custom` on NativeProgram isn't in types.ts yet, so read it
 * defensively: an explicit flag wins, the id prefix is the fallback.
 */
export function isCustomProgram(program: NativeProgram): boolean {
  const flagged = (program as { is_custom?: unknown }).is_custom === true;
  return flagged || program.id.startsWith("custom-");
}

/**
 * Pinned-first ordering: pinned tiles first (in `pinned` array order), then
 * usage-ranked (higher count, tiebreak most-recent first), then the
 * incoming order (stable — browse stays alphabetical, search stays by
 * relevance). Hidden programs are filtered out.
 */
export function sortLauncherItems<T extends { id: string }>(
  items: T[],
  opts?: LauncherSortOpts
): T[] {
  if (!opts) return items;
  const hidden = new Set(opts.hiddenProgramIds ?? []);
  const visible =
    hidden.size === 0
      ? items
      : items.filter((it) => {
          if (it.id.startsWith("program:")) {
            return !hidden.has(it.id.slice("program:".length));
          }
          return true;
        });
  if (!opts.pinned?.length && !opts.usage) return visible;
  const pinRank = new Map((opts.pinned ?? []).map((id, i) => [id, i]));
  const usage = opts.usage ?? {};
  const rankOf = (it: T): [number, number, number] => {
    const p = pinRank.get(it.id);
    if (p !== undefined) return [0, p, 0];
    const u = usage[it.id];
    if (u) return [1, -u.count, -u.last_used];
    return [1, 0, 0];
  };
  return visible
    .map((it, index) => ({ it, index, rank: rankOf(it) }))
    .sort((a, b) => {
      for (let k = 0; k < 3; k++) {
        if (a.rank[k] !== b.rank[k]) return a.rank[k] - b.rank[k];
      }
      return a.index - b.index;
    })
    .map((x) => x.it);
}

/**
 * Record a successful launch for usage ranking. Fire-and-forget: usage
 * stats must never break or delay opening something. Call with the tagged
 * item id ("app:<id>" / "account:<id>" / "program:<id>").
 */
export function recordLaunch(itemId: string): void {
  invoke("record_launch", { itemId }).catch(() => {});
}

/**
 * Everything, for the phone-folder grid when no query is typed: web apps
 * (with their accounts right after each app), then installed programs —
 * alphabetical, capped. Like opening a folder on a phone home screen.
 */
export function browseAll(
  apps: WebApp[],
  programs: NativeProgram[],
  opts?: LauncherSortOpts
): SearchResult[] {
  const items: SearchResult[] = [];
  const sortedApps = [...apps].sort((a, b) => a.name.localeCompare(b.name));
  for (const app of sortedApps) {
    items.push({
      kind: "app",
      id: `app:${app.id}`,
      title: app.name,
      context: hostOf(app.url),
      score: 0,
      app,
    });
    const sortedAccounts = [...app.accounts].sort((a, b) =>
      a.label.localeCompare(b.label)
    );
    for (const account of sortedAccounts) {
      items.push({
        kind: "account",
        id: `account:${account.id}`,
        // App name first (the "Default" account label used to be the big
        // text, which confused users); the account label is the sublabel.
        title: app.name,
        context: account.label,
        score: 0,
        app,
        account,
      });
    }
  }
  const sortedPrograms = [...programs].sort((a, b) => a.name.localeCompare(b.name));
  for (const program of sortedPrograms) {
    items.push({
      kind: "program",
      id: `program:${program.id}`,
      title: program.name,
      context: "",
      score: 0,
      program,
    });
  }
  // v0.9.3: pinned web searches have no app/program behind them — their
  // only existence is the pinned tag, so they are materialized here.
  for (const pid of opts?.pinned ?? []) {
    const q = parseSearchTag(pid);
    if (q !== null && q.trim() !== "") {
      items.push({
        kind: "search",
        id: pid,
        title: q,
        context: "Web search",
        score: 0,
        query: q,
      });
    }
  }
  // v0.9.6: pinned workspaces, same treatment. A workspace deleted after
  // pinning drops silently instead of breaking the launcher.
  const wsById = new Map((opts?.workspaces ?? []).map((w) => [w.id, w]));
  for (const pid of opts?.pinned ?? []) {
    const wid = parseWorkspaceTag(pid);
    if (wid === null) continue;
    const ws = wsById.get(wid);
    if (!ws) continue;
    items.push({
      kind: "workspace",
      id: pid,
      title: ws.name,
      context: "Workspace",
      score: 0,
      workspaceId: ws.id,
    });
  }
  return sortLauncherItems(items, opts).slice(0, BROWSE_LIMIT);
}

/**
 * One ranked result list across accounts, web apps, and native programs.
 * Apps only appear as their own row when the query matches the app itself
 * (not just its accounts) — keeps the list short.
 */
export function buildResults(
  query: string,
  apps: WebApp[],
  programs: NativeProgram[],
  routines: Routine[],
  opts?: LauncherSortOpts
): SearchResult[] {
  const q = query.trim();
  if (!q) return [];
  const results: SearchResult[] = [];

  for (const app of apps) {
    const appScore = Math.max(fuzzyScore(q, app.name), fuzzyScore(q, hostOf(app.url)));
    if (appScore > 0) {
      results.push({
        kind: "app",
        id: `app:${app.id}`,
        title: app.name,
        context: hostOf(app.url),
        score: appScore,
        app,
      });
    }
    for (const account of app.accounts) {
      const score = Math.max(fuzzyScore(q, account.label), fuzzyScore(q, `${account.label} ${app.name}`));
      if (score > 0) {
        results.push({
          kind: "account",
          id: `account:${account.id}`,
          // Tile shows the app name big, the account label small — the
          // account label still participates in matching via the score.
          title: app.name,
          context: account.label,
          score: score + 1, // accounts edge out the bare app row
          app,
          account,
        });
      }
    }
  }

  for (const program of programs) {
    const score = fuzzyScore(q, program.name);
    if (score > 0) {
      results.push({
        kind: "program",
        id: `program:${program.id}`,
        title: program.name,
        context: "Program",
        score,
        program,
      });
    }
  }

  // v0.8.0: routines match on their name. The tile shows the routine name
  // plus "Routine — Enter to run".
  for (const routine of routines) {
    const score = fuzzyScore(q, routine.name);
    if (score > 0) {
      results.push({
        kind: "routine",
        id: `routine:${routine.id}`,
        title: routine.name,
        context: "Routine — Enter to run",
        score,
        routine,
      });
    }
  }

  // v0.9.3: pinned web searches stay discoverable while typing.
  for (const pid of opts?.pinned ?? []) {
    const sq = parseSearchTag(pid);
    if (sq === null || sq.trim() === "") continue;
    const sScore = fuzzyScore(q, sq);
    if (sScore > 0) {
      results.push({
        kind: "search",
        id: pid,
        title: sq,
        context: "Web search",
        score: sScore,
        query: sq,
      });
    }
  }
  // v0.9.6: pinned workspaces stay discoverable while typing too.
  const wsById = new Map((opts?.workspaces ?? []).map((w) => [w.id, w]));
  for (const pid of opts?.pinned ?? []) {
    const wid = parseWorkspaceTag(pid);
    if (wid === null) continue;
    const ws = wsById.get(wid);
    if (!ws) continue;
    const wScore = fuzzyScore(q, ws.name);
    if (wScore > 0) {
      results.push({
        kind: "workspace",
        id: pid,
        title: ws.name,
        context: "Workspace",
        score: wScore,
        workspaceId: ws.id,
      });
    }
  }

  results.sort((a, b) => b.score - a.score);
  return sortLauncherItems(results, opts).slice(0, 25);
}

/**
 * Phone-folder grid: one tile per result — icon with the name underneath.
 * Accounts show their app's icon with the account label (and the app name
 * as a quiet sub-label); programs show their extracted .exe icon.
 */
export function IconGrid({
  items,
  activeIndex,
  onHover,
  onActivate,
  renderIcon,
  pinnedIds,
  onTileContextMenu,
}: {
  items: SearchResult[];
  activeIndex: number;
  onHover: (i: number) => void;
  onActivate: (r: SearchResult) => void;
  renderIcon: (r: SearchResult) => React.ReactNode;
  /** Tagged ids that show the small pin badge. Optional. */
  pinnedIds?: ReadonlySet<string>;
  /** Right-click on a tile. Receives the result and the cursor position. Optional. */
  onTileContextMenu?: (r: SearchResult, x: number, y: number) => void;
}) {
  return (
    <div className="icon-grid" role="listbox" aria-label="Apps and programs">
      {items.map((r, i) => (
        <button
          key={r.id}
          type="button"
          role="option"
          aria-selected={i === activeIndex}
          className={`icon-tile${i === activeIndex ? " is-active" : ""}`}
          onMouseEnter={() => onHover(i)}
          onClick={() => onActivate(r)}
          onContextMenu={(e) => {
            e.preventDefault();
            onTileContextMenu?.(r, e.clientX, e.clientY);
          }}
        >
          {pinnedIds?.has(r.id) && (
            <span className="tile-pin" title="Pinned to top" aria-label="Pinned">
              <svg viewBox="0 0 16 16" aria-hidden="true" focusable="false">
                <path d="M8 1.5C5.5 1.5 3.5 3.5 3.5 6c0 3.2 4.5 8.5 4.5 8.5s4.5-5.3 4.5-8.5c0-2.5-2-4.5-4.5-4.5zm0 6.3a1.8 1.8 0 1 1 0-3.6 1.8 1.8 0 0 1 0 3.6z" />
              </svg>
            </span>
          )}
          <span className="tile-icon">{renderIcon(r)}</span>
          <span className="tile-label">{r.title}</span>
          {(r.kind === "account" || r.kind === "routine") && (
            <span className="tile-sub">{r.context}</span>
          )}
        </button>
      ))}
    </div>
  );
}

export function ProgramIcon({ program }: { program: NativeProgram }) {
  const [failed, setFailed] = useState(false);
  if (!program.icon_path || failed) {
    return (
      <span className="app-icon-fallback" aria-hidden="true">
        {program.name.charAt(0).toUpperCase() || "?"}
      </span>
    );
  }
  return (
    <img
      className="app-icon"
      src={convertFileSrc(program.icon_path)}
      alt=""
      loading="lazy"
      onError={() => setFailed(true)}
    />
  );
}

/**
 * v0.9.3: magnifier tile icon for pinned web searches. Inline SVG, quiet
 * stroke style matching the tile aesthetic — no emoji, one accent family.
 */
export function SearchTileIcon() {
  return (
    <svg
      viewBox="0 0 24 24"
      aria-hidden="true"
      focusable="false"
      width="34"
      height="34"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
    >
      <circle cx="11" cy="11" r="7" />
      <line x1="16.5" y1="16.5" x2="21" y2="21" />
    </svg>
  );
}

/**
 * v0.9.6: grid/stack tile icon for pinned workspaces. Inline SVG, quiet
 * stroke style matching the tile aesthetic — no emoji, one accent family.
 */
export function WorkspaceTileIcon() {
  return (
    <svg
      viewBox="0 0 24 24"
      aria-hidden="true"
      focusable="false"
      width="34"
      height="34"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
    >
      <rect x="3" y="3" width="8" height="8" rx="1.5" />
      <rect x="13" y="3" width="8" height="8" rx="1.5" />
      <rect x="3" y="13" width="8" height="8" rx="1.5" />
      <rect x="13" y="13" width="8" height="8" rx="1.5" />
    </svg>
  );
}

export function SearchBar({
  query,
  onQuery,
  onKeyDown,
  inputRef,
}: {
  query: string;
  onQuery: (q: string) => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  inputRef: React.RefObject<HTMLInputElement | null>;
}) {
  return (
    <div className="search-wrap">
      <input
        ref={inputRef}
        className="search-input"
        type="search"
        value={query}
        onChange={(e) => onQuery(e.target.value)}
        onKeyDown={onKeyDown}
        placeholder="Search apps, accounts, and programs…"
        aria-label="Search apps, accounts, and programs"
        autoComplete="off"
        spellCheck={false}
      />
      <span className="search-hint" aria-hidden="true">
        Arrow keys move · Enter opens · Esc clears, then hides
      </span>
      <span className="search-hint search-hint-sub" aria-hidden="true">
        = calculates · &gt; runs a system command · ? searches the web
      </span>
    </div>
  );
}

/**
 * Launcher preferences: summon hotkey, run at startup, program rescan.
 * The hotkey change is validated live — if the new key can't be registered
 * (already taken), the old one stays and the error names the key.
 */
export function LauncherSettingsPanel({
  settings,
  onSaved,
  programsCount,
  onProgramsRefreshed,
  onError,
  isWindows,
}: {
  settings: LauncherSettings;
  onSaved: (s: LauncherSettings) => void;
  programsCount: number;
  onProgramsRefreshed: (programs: NativeProgram[]) => void;
  onError: (msg: string) => void;
  /** v0.9.6: the frameless page windows + Esc gesture are Windows-only. */
  isWindows: boolean;
}) {
  const [hotkey, setHotkey] = useState(settings.hotkey);
  const [autostart, setAutostart] = useState(settings.autostart);
  const [searchEngine, setSearchEngine] = useState<"duckduckgo" | "google">(
    settings.search_engine ?? "duckduckgo"
  );
  const [customCursor, setCustomCursor] = useState<
    "off" | "dot" | "ring" | "trail"
  >(settings.custom_cursor ?? "off");
  const [startupMode, setStartupMode] = useState<"restore" | "ask" | "fresh">(
    settings.startup_mode ?? "restore"
  );
  const [opacity, setOpacity] = useState(
    Math.round((settings.panel_opacity ?? 0.55) * 100)
  );
  const [saving, setSaving] = useState(false);
  const [rescanning, setRescanning] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  // v0.11.0: "Copy diagnostics" feedback.
  const [diagCopied, setDiagCopied] = useState(false);
  // Whether the saved summon hotkey is actually registered with the OS.
  // Startup registration can fail silently (e.g. another app owns Alt+Space);
  // the banner below tells the user instead of showing a dead hotkey.
  const [hotkeyStatus, setHotkeyStatus] = useState<HotkeyStatus | null>(null);
  // Debounce slider drags so we save once per pause, not per tick.
  const opacityTimer = useRef<number | null>(null);

  useEffect(() => {
    let alive = true;
    void invoke<HotkeyStatus>("get_hotkey_status")
      .then((s) => {
        if (alive) setHotkeyStatus(s);
      })
      .catch(() => {
        /* older backend without the command; no banner */
      });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    return () => {
      if (opacityTimer.current !== null) {
        window.clearTimeout(opacityTimer.current);
      }
    };
  }, []);

  async function refreshHotkeyStatus() {
    try {
      setHotkeyStatus(await invoke<HotkeyStatus>("get_hotkey_status"));
    } catch {
      /* keep the last known status */
    }
  }

  async function handleHotkeySave(e: React.FormEvent) {
    e.preventDefault();
    setFormError(null);
    setSaving(true);
    try {
      await invoke("set_hotkey", { hotkey: hotkey.trim() });
      const updated = await invoke<LauncherSettings>("get_launcher_settings");
      onSaved(updated);
      setHotkey(updated.hotkey);
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
    } finally {
      await refreshHotkeyStatus();
      setSaving(false);
    }
  }

  async function handleAutostart(checked: boolean) {
    setFormError(null);
    try {
      await invoke("set_autostart", { enabled: checked });
      setAutostart(checked);
      onSaved({ ...settings, hotkey, autostart: checked });
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
    }
  }

  async function handleStartupMode(mode: "restore" | "ask" | "fresh") {
    if (mode === startupMode) return;
    setStartupMode(mode);
    setFormError(null);
    try {
      const updated = await invoke<LauncherSettings>("set_startup_mode", {
        mode,
      });
      onSaved(updated);
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
      setStartupMode(settings.startup_mode ?? "restore");
    }
  }

  async function handleSearchEngine(engine: "duckduckgo" | "google") {
    setSearchEngine(engine);
    setFormError(null);
    try {
      const updated = await invoke<LauncherSettings>("set_search_engine", {
        engine,
      });
      onSaved(updated);
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
      setSearchEngine(settings.search_engine ?? "duckduckgo");
    }
  }

  async function handleCustomCursor(style: "off" | "dot" | "ring" | "trail") {
    setCustomCursor(style);
    setFormError(null);
    try {
      const updated = await invoke<LauncherSettings>("set_custom_cursor", {
        style,
      });
      onSaved(updated);
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
      setCustomCursor(settings.custom_cursor ?? "off");
    }
  }

  async function handleOpacity(percent: number) {
    setOpacity(percent);
    setFormError(null);
    if (opacityTimer.current !== null) {
      window.clearTimeout(opacityTimer.current);
    }
    opacityTimer.current = window.setTimeout(() => {
      opacityTimer.current = null;
      void (async () => {
        try {
          const updated = await invoke<LauncherSettings>("set_panel_opacity", {
            opacity: percent / 100,
          });
          onSaved(updated);
        } catch (err) {
          const msg = errMsg(err);
          setFormError(msg);
          onError(msg);
        }
      })();
    }, 300);
  }

  async function handleRescan() {
    setFormError(null);
    setRescanning(true);
    try {
      await invoke<number>("rescan_programs");
      const list = await invoke<NativeProgram[]>("list_programs");
      onProgramsRefreshed(list);
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
    } finally {
      setRescanning(false);
    }
  }

  // v0.11.0: copy recent errors + version + OS for pasting into chat.
  async function handleCopyDiagnostics() {
    setFormError(null);
    try {
      await invoke("copy_diagnostics");
      setDiagCopied(true);
    } catch (err) {
      const msg = errMsg(err);
      setFormError(msg);
      onError(msg);
    }
  }

  return (
    <div className="launcher-settings">
      {hotkeyStatus && !hotkeyStatus.registered && (
        <div className="banner banner-error" role="alert">
          <strong>
            {hotkeyStatus.hotkey || "The summon hotkey"} couldn't be
            registered — another app is already using it. Pick a different
            hotkey below.
          </strong>
          {hotkeyStatus.error && (
            <>
              <br />
              <span className="muted small">{hotkeyStatus.error}</span>
            </>
          )}
        </div>
      )}
      <form className="inline-form" onSubmit={(e) => void handleHotkeySave(e)}>
        <label>
          <span>Summon hotkey</span>
          <HotkeyCapture
            value={hotkey}
            onChange={setHotkey}
            ariaLabel="Summon hotkey"
            placeholder="Alt+Space"
          />
        </label>
        <button type="submit" disabled={saving}>
          {saving ? "Saving…" : "Set hotkey"}
        </button>
        <span className="help">
          Press it anywhere to show or hide AppMaka. Click the field, then press the keys you want.
        </span>
      </form>

      <label className="check-row">
        <input
          type="checkbox"
          checked={autostart}
          onChange={(e) => void handleAutostart(e.target.checked)}
        />
        <span>Run AppMaka when I sign in</span>
      </label>

      <fieldset className="radio-group">
        <legend>On startup</legend>
        <label className="radio-row">
          <input
            type="radio"
            name="startup-mode"
            checked={startupMode === "restore"}
            onChange={() => void handleStartupMode("restore")}
          />
          <span>Restore last session</span>
        </label>
        <label className="radio-row">
          <input
            type="radio"
            name="startup-mode"
            checked={startupMode === "ask"}
            onChange={() => void handleStartupMode("ask")}
          />
          <span>Ask me</span>
        </label>
        <label className="radio-row">
          <input
            type="radio"
            name="startup-mode"
            checked={startupMode === "fresh"}
            onChange={() => void handleStartupMode("fresh")}
          />
          <span>Start fresh</span>
        </label>
      </fieldset>
      <span className="help">
        Restored windows are real windows and use memory like any open
        window. Your auto-suspend and auto-close settings still apply to
        them.
      </span>
      {isWindows && (
        <span className="help">
          Page windows have no close button: hold the left mouse button and
          press Esc to close one. Alt+F4 works too.
        </span>
      )}

      <label className="inline-form">
        <span>Web search for launcher commands</span>
        <select
          value={searchEngine}
          onChange={(e) =>
            void handleSearchEngine(e.target.value as "duckduckgo" | "google")
          }
          aria-label="Search engine for launcher commands"
        >
          <option value="duckduckgo">DuckDuckGo</option>
          <option value="google">Google</option>
        </select>
        <span className="help">
          Used by the launcher's ?query command. DuckDuckGo is the default.
        </span>
      </label>

      <label className="inline-form">
        <span>Custom cursor in app windows</span>
        <select
          value={customCursor}
          onChange={(e) =>
            void handleCustomCursor(
              e.target.value as "off" | "dot" | "ring" | "trail"
            )
          }
          aria-label="Custom cursor style for app windows"
        >
          <option value="off">Off (system cursor)</option>
          <option value="dot">Dot</option>
          <option value="ring">Ring</option>
          <option value="trail">Trail</option>
        </select>
        <span className="help">
          Draws a lightweight pointer inside app windows. Useful if the
          system cursor ever goes invisible there. Applies to windows opened
          after the change.
        </span>
      </label>

      <label className="slider-row">
        <span>
          Panel solidity <strong>{opacity}%</strong>
        </span>
        <input
          type="range"
          min={30}
          max={100}
          step={1}
          value={opacity}
          onChange={(e) => void handleOpacity(Number(e.target.value))}
          aria-label="Launcher panel solidity"
        />
        <span className="help">
          How solid the launcher panel looks. Lower is more see-through.
        </span>
      </label>

      <div className="inline-form">
        <button type="button" onClick={() => void handleRescan()} disabled={rescanning}>
          {rescanning ? "Scanning…" : "Rescan programs"}
        </button>
        <span className="help">
          {programsCount} installed {programsCount === 1 ? "program" : "programs"} indexed from
          the Start Menu and Desktop. Microsoft Store apps aren't listed yet.
        </span>
      </div>

      <div className="inline-form">
        <button type="button" onClick={() => void handleCopyDiagnostics()}>
          {diagCopied ? "Copied" : "Copy diagnostics"}
        </button>
        <span className="help">
          Copies recent errors, the app version, and your OS version — paste
          it straight into chat when reporting a problem.
        </span>
      </div>

      {formError && (
        <p className="form-error" role="alert">
          {formError}
        </p>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// v0.6.0 — launcher overlay additions
// ---------------------------------------------------------------------------

/**
 * Global right-click guard: suppresses the webview's native context menu
 * everywhere except inside editable fields, where native copy/paste must
 * keep working. Mount <ContextMenuGuard /> once near the app root.
 */
export function useContextMenuGuard() {
  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      const target = e.target;
      if (target instanceof HTMLElement) {
        if (target.closest("input, textarea, select")) return;
        const ce = target.closest("[contenteditable]");
        if (ce && ce.getAttribute("contenteditable") !== "false") return;
      }
      e.preventDefault();
    };
    // Capture phase so this runs before any tile-level handler.
    document.addEventListener("contextmenu", onContextMenu, true);
    return () => document.removeEventListener("contextmenu", onContextMenu, true);
  }, []);
}

/** Renders nothing; installs the global context-menu guard while mounted. */
export function ContextMenuGuard() {
  useContextMenuGuard();
  return null;
}

// --- Tile context menu -------------------------------------------------------

/** One row in the tile context menu. */
export interface MenuEntry {
  key: string;
  label: string;
  /** Destructive action (Remove) — styled in red. */
  danger?: boolean;
  /** Checkable row (v0.9.9): renders a ✓ when true. */
  checked?: boolean;
  submenu?: MenuEntry[];
  onSelect?: () => void | Promise<void>;
}

/**
 * Absolutely-positioned menu at the cursor. Clamps itself inside the
 * viewport, supports one level of submenu, and dismisses on Escape,
 * outside pointer-down, or focus leaving the menu.
 */
export function TileMenu({
  x,
  y,
  entries,
  onDismiss,
}: {
  x: number;
  y: number;
  entries: MenuEntry[];
  onDismiss: () => void;
}) {
  const rootRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState({ left: x, top: y });
  const [openSub, setOpenSub] = useState<string | null>(null);

  // Clamp inside the viewport once the menu has a measured size.
  useLayoutEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const margin = 8;
    let left = x;
    let top = y;
    if (left + rect.width > window.innerWidth - margin) {
      left = Math.max(margin, window.innerWidth - rect.width - margin);
    }
    if (top + rect.height > window.innerHeight - margin) {
      top = Math.max(margin, window.innerHeight - rect.height - margin);
    }
    setPos({ left, top });
  }, [x, y]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        onDismiss();
      }
    };
    const onDown = (e: PointerEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        onDismiss();
      }
    };
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("pointerdown", onDown, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("pointerdown", onDown, true);
    };
  }, [onDismiss]);

  // Focus the menu so Escape works even if nothing was clicked yet.
  useEffect(() => {
    rootRef.current?.focus();
  }, []);

  function handleBlur(e: React.FocusEvent) {
    if (!e.currentTarget.contains(e.relatedTarget as Node)) onDismiss();
  }

  function choose(entry: MenuEntry) {
    setOpenSub(null);
    try {
      void entry.onSelect?.();
    } finally {
      onDismiss();
    }
  }

  return (
    <div
      ref={rootRef}
      className="tile-menu"
      role="menu"
      tabIndex={-1}
      style={{ left: pos.left, top: pos.top }}
      onBlur={handleBlur}
    >
      <ul className="tile-menu-list">
        {entries.map((entry) => (
          <li key={entry.key} className="tile-menu-row">
            <button
              type="button"
              role="menuitem"
              className={`tile-menu-item${entry.danger ? " is-danger" : ""}`}
              aria-haspopup={entry.submenu ? "true" : undefined}
              aria-expanded={entry.submenu ? openSub === entry.key : undefined}
              onClick={() => (entry.submenu ? setOpenSub(entry.key) : choose(entry))}
              onMouseEnter={() => {
                if (entry.submenu) setOpenSub(entry.key);
              }}
              onFocus={() => {
                if (entry.submenu) setOpenSub(entry.key);
              }}
            >
              {entry.checked ? (
                <span>
                  <span className="tile-menu-check" aria-hidden="true">
                    ✓{" "}
                  </span>
                  {entry.label}
                </span>
              ) : (
                <span>{entry.label}</span>
              )}
              {entry.submenu && (
                <span className="tile-menu-caret" aria-hidden="true">
                  ▸
                </span>
              )}
            </button>
            {entry.submenu && openSub === entry.key && (
              <div className="tile-submenu" role="menu" aria-label={entry.label}>
                {entry.submenu.map((sub) => (
                  <button
                    key={sub.key}
                    type="button"
                    role="menuitem"
                    className={`tile-menu-item${sub.danger ? " is-danger" : ""}`}
                    onClick={() => choose(sub)}
                  >
                    <span>{sub.label}</span>
                  </button>
                ))}
              </div>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Host-provided handlers for tile menu actions. The host (App.tsx) owns
 * app/program state, so each action is a callback: it performs the backend
 * call and refreshes settings + list state afterwards.
 */
export interface TileMenuActions {
  onOpen: (r: SearchResult) => void;
  onOpenAccount: (app: WebApp, account: Account) => void;
  /**
   * v0.13.0: open an app/account in a new tabbed window (first tab).
   * Optional so hosts without tab support don't have to provide it.
   */
  onOpenAsTabbed?: (app: WebApp, account: Account) => void;
  onTogglePin: (itemId: string) => void | Promise<void>;
  /**
   * v0.9.9: "Don't close this window" for an open account window, by
   * window label (`acct-<appId>-<accountId>`). Optional so hosts without
   * window state don't have to provide it.
   */
  onToggleWindowPin?: (label: string) => void | Promise<void>;
  /**
   * v0.9.10: "Close window" for an open account window, by window label
   * (`acct-<appId>-<accountId>`). Optional like onToggleWindowPin.
   */
  onCloseWindow?: (label: string) => void | Promise<void>;
  onEditApp: (app: WebApp) => void;
  onRemoveApp: (app: WebApp) => void;
  onForgetLogin: (app: WebApp, account: Account) => void;
  onHideProgram: (programId: string) => void | Promise<void>;
  onRevealProgramLocation: (program: NativeProgram) => void | Promise<void>;
  onRemoveCustomProgram: (programId: string) => void | Promise<void>;
}

/**
 * Menu rows per tile kind:
 * - App: Open, Open account ▸ (its accounts), Pin/Unpin, Edit, Remove
 * - Account: Open, Pin/Unpin
 * - Program: Launch, Pin/Unpin, Open file location, Hide from launcher (or Remove if custom)
 */
export function buildTileMenuEntries(
  r: SearchResult,
  ctx: {
    isPinned: boolean;
    actions: TileMenuActions;
    /**
     * v0.9.9: open-window state for the "Don't close this window" entry,
     * by window label. Null/absent = window not open (or unknown) → the
     * entry is hidden.
     */
    windowPin?: (label: string) => { open: boolean; pinned: boolean } | null;
  }
): MenuEntry[] {
  const a = ctx.actions;
  const pin: MenuEntry = {
    key: "pin",
    label: ctx.isPinned ? "Unpin" : "Pin to top",
    onSelect: () => a.onTogglePin(r.id),
  };
  // v0.9.9: checkable "Don't close this window" for an open account
  // window. The word "pin" never appears in the UI for this feature —
  // it is a different concept from the tile "Pin to top" above.
  const dontCloseEntry = (appId: string, accountId: string): MenuEntry | null => {
    if (!a.onToggleWindowPin) return null;
    const label = `acct-${appId}-${accountId}`;
    const state = ctx.windowPin?.(label);
    if (!state?.open) return null;
    return {
      key: "dont-close",
      label: "Don't close this window",
      checked: state.pinned,
      onSelect: () => {
        void a.onToggleWindowPin?.(label);
      },
    };
  };
  // v0.9.10: "Close window" for an open account window. Same open-window
  // lookup as the "Don't close this window" entry above; pinned windows
  // get the standard confirm from the backend.
  const closeWindowEntry = (appId: string, accountId: string): MenuEntry | null => {
    if (!a.onCloseWindow) return null;
    const label = `acct-${appId}-${accountId}`;
    const state = ctx.windowPin?.(label);
    if (!state?.open) return null;
    return {
      key: "close-window",
      label: "Close window",
      onSelect: () => {
        void a.onCloseWindow?.(label);
      },
    };
  };
  if (r.kind === "app") {
    const entries: MenuEntry[] = [
      { key: "open", label: "Open", onSelect: () => a.onOpen(r) },
    ];
    const sortedAccounts = [...r.app.accounts].sort((x, y) =>
      x.label.localeCompare(y.label)
    );
    if (sortedAccounts.length > 0) {
      entries.push({
        key: "open-account",
        label: "Open account",
        submenu: sortedAccounts.map((acct) => ({
          key: `account:${acct.id}`,
          label: acct.label,
          onSelect: () => a.onOpenAccount(r.app, acct),
        })),
      });
      // v0.13.0: open in a new tabbed window instead of a plain page.
      if (a.onOpenAsTabbed) {
        entries.push({
          key: "open-tabbed",
          label: "Open as tabbed window",
          submenu: sortedAccounts.map((acct) => ({
            key: `tabbed:${acct.id}`,
            label: acct.label,
            onSelect: () => a.onOpenAsTabbed!(r.app, acct),
          })),
        });
      }
    }
    entries.push(
      pin,
      { key: "edit", label: "Edit", onSelect: () => a.onEditApp(r.app) },
      {
        key: "remove",
        label: "Remove",
        danger: true,
        onSelect: () => a.onRemoveApp(r.app),
      }
    );
    return entries;
  }
  if (r.kind === "account") {
    const acct = r.account;
    const entries: MenuEntry[] = [
      { key: "open", label: "Open", onSelect: () => a.onOpen(r) },
      pin,
    ];
    if (acct) {
      // v0.13.0: open this account in a new tabbed window.
      if (a.onOpenAsTabbed) {
        entries.splice(1, 0, {
          key: "open-tabbed",
          label: "Open as tabbed window",
          onSelect: () => a.onOpenAsTabbed!(r.app, acct),
        });
      }
      const dontClose = dontCloseEntry(r.app.id, acct.id);
      if (dontClose) entries.push(dontClose);
      const closeWindow = closeWindowEntry(r.app.id, acct.id);
      if (closeWindow) entries.push(closeWindow);
      entries.push({
        key: "forget-login",
        label: "Forget this login",
        danger: true,
        onSelect: () => a.onForgetLogin(r.app, acct),
      });
    }
    return entries;
  }
  if (r.kind === "routine") {
    return [{ key: "run", label: "Run", onSelect: () => a.onOpen(r) }];
  }
  if (r.kind === "search") {
    // v0.9.3: pinned web search — re-run it, or unpin. Nothing else
    // applies (no app to edit, no file to reveal).
    return [
      { key: "open", label: "Search again", onSelect: () => a.onOpen(r) },
      pin,
    ];
  }
  if (r.kind === "workspace") {
    // v0.9.6: pinned workspace — open every member window at once, or
    // unpin. Nothing else applies (managed in the library's Workspaces
    // section).
    return [
      { key: "open", label: "Open all", onSelect: () => a.onOpen(r) },
      pin,
    ];
  }
  const prog = r.program;
  const entries: MenuEntry[] = [
    { key: "launch", label: "Launch", onSelect: () => a.onOpen(r) },
    pin,
    {
      key: "open-location",
      label: "Open file location",
      onSelect: () => a.onRevealProgramLocation(prog),
    },
  ];
  if (isCustomProgram(prog)) {
    entries.push({
      key: "remove",
      label: "Remove",
      danger: true,
      onSelect: () => a.onRemoveCustomProgram(prog.id),
    });
  } else {
    entries.push({
      key: "hide",
      label: "Hide from launcher",
      onSelect: () => a.onHideProgram(prog.id),
    });
  }
  return entries;
}

/**
 * Owns the open/close state for the tile menu. Wire `openTileMenu` to
 * IconGrid's `onTileContextMenu` and render `tileMenuNode` next to the grid.
 */
export function useTileMenu(deps: {
  actions: TileMenuActions;
  isPinned: (itemId: string) => boolean;
  /**
   * v0.9.9: open-window state for the "Don't close this window" entry.
   * Same ref treatment as actions/isPinned so the host can keep it fresh.
   */
  windowPin?: (label: string) => { open: boolean; pinned: boolean } | null;
}) {
  const [target, setTarget] = useState<{
    r: SearchResult;
    x: number;
    y: number;
  } | null>(null);
  const close = useCallback(() => setTarget(null), []);
  const openFor = useCallback((r: SearchResult, x: number, y: number) => {
    setTarget({ r, x, y });
  }, []);
  const actionsRef = useRef(deps.actions);
  actionsRef.current = deps.actions;
  const isPinnedRef = useRef(deps.isPinned);
  isPinnedRef.current = deps.isPinned;
  const windowPinRef = useRef(deps.windowPin);
  windowPinRef.current = deps.windowPin;
  const node = target ? (
    <TileMenu
      x={target.x}
      y={target.y}
      entries={buildTileMenuEntries(target.r, {
        isPinned: isPinnedRef.current(target.r.id),
        actions: actionsRef.current,
        windowPin: windowPinRef.current
          ? (label) => windowPinRef.current!(label)
          : undefined,
      })}
      onDismiss={close}
    />
  ) : null;
  return { tileMenuNode: node, openTileMenu: openFor, closeTileMenu: close };
}

// --- Rescan ------------------------------------------------------------------

/**
 * Rescan button for the overlay (header + empty-results state). Runs
 * `rescan_programs`, then hands the refreshed list to the host.
 */
export function RescanButton({
  onRefreshed,
  onError,
  className,
  children,
}: {
  onRefreshed: (programs: NativeProgram[]) => void;
  onError?: (msg: string) => void;
  className?: string;
  children?: React.ReactNode;
}) {
  const [busy, setBusy] = useState(false);
  async function handle() {
    if (busy) return;
    setBusy(true);
    try {
      await invoke<number>("rescan_programs");
      const list = await invoke<NativeProgram[]>("list_programs");
      onRefreshed(list);
    } catch (err) {
      onError?.(errMsg(err));
    } finally {
      setBusy(false);
    }
  }
  return (
    <button
      type="button"
      className={className ?? "text-button"}
      disabled={busy}
      onClick={() => void handle()}
    >
      {busy ? "Scanning…" : (children ?? "Rescan programs")}
    </button>
  );
}

/**
 * Refresh the program list when the backend finishes a scan. The backend
 * emits `programs-scanned`; this complements (not replaces) the existing
 * ~8s re-poll in App.tsx.
 */
export function useProgramsScannedRefresh(
  onPrograms: (programs: NativeProgram[]) => void
) {
  const ref = useRef(onPrograms);
  ref.current = onPrograms;
  useEffect(() => {
    let off: (() => void) | undefined;
    listen("programs-scanned", () => {
      invoke<NativeProgram[]>("list_programs")
        .then((list) => ref.current(list))
        .catch(() => {});
    })
      .then((unlisten) => {
        off = unlisten;
      })
      .catch(() => {});
    return () => off?.();
  }, []);
}

/** Empty-results state with the "not finding it?" rescan affordance. */
export function NoMatchesHint({
  onProgramsRefreshed,
  onError,
}: {
  onProgramsRefreshed: (programs: NativeProgram[]) => void;
  onError?: (msg: string) => void;
}) {
  return (
    <p className="muted folder-hint">
      No matches. Not finding it?{" "}
      <RescanButton onRefreshed={onProgramsRefreshed} onError={onError}>
        Rescan
      </RescanButton>
    </p>
  );
}

// --- Manually add a program --------------------------------------------------

/**
 * Small modal: name + executable path, with a Browse button backed by the
 * `pick_executable` command. If the picker returns null (plugin missing),
 * the typed path is used as-is. Backend errors are shown honestly.
 */
export function AddProgramModal({
  open,
  onClose,
  onAdded,
}: {
  open: boolean;
  onClose: () => void;
  onAdded: (program: CustomProgram) => void;
}) {
  const [name, setName] = useState("");
  const [path, setPath] = useState("");
  const [busy, setBusy] = useState(false);
  const [browsing, setBrowsing] = useState(false);
  const [pickerNote, setPickerNote] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      setName("");
      setPath("");
      setError(null);
      setPickerNote(false);
    }
  }, [open ]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey, true);
    return () => document.removeEventListener("keydown", onKey, true);
  }, [open, onClose]);

  if (!open) return null;

  async function browse() {
    if (browsing) return;
    setBrowsing(true);
    setError(null);
    try {
      const picked = await invoke<string | null>("pick_executable");
      if (picked) {
        setPath(picked);
      } else {
        // No file picker available — the user can type the path instead.
        setPickerNote(true);
      }
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setBrowsing(false);
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const cleanName = name.trim();
    const cleanPath = path.trim();
    if (!cleanName) {
      setError("Give the program a name.");
      return;
    }
    if (!cleanPath) {
      setError("Enter the path to the program's executable.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const created = await invoke<CustomProgram>("add_custom_program", {
        name: cleanName,
        exePath: cleanPath,
      });
      onAdded(created);
      onClose();
    } catch (err) {
      setError(errMsg(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div
        className="modal-card"
        role="dialog"
        aria-modal="true"
        aria-label="Add a program"
      >
        <h2 className="modal-title">Add a program</h2>
        <p className="modal-sub">
          For programs the automatic scan misses — point AppMaka at the
          executable and it shows up in the launcher.
        </p>
        <form onSubmit={(e) => void submit(e)}>
          <label className="modal-field">
            <span>Name</span>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. QuicMic"
              maxLength={80}
              autoComplete="off"
              spellCheck={false}
              autoFocus
            />
          </label>
          <label className="modal-field">
            <span>Executable path</span>
            <div className="modal-path-row">
              <input
                value={path}
                onChange={(e) => setPath(e.target.value)}
                placeholder="C:\Program Files\QuicMic\quicmic.exe"
                autoComplete="off"
                spellCheck={false}
              />
              <button
                type="button"
                className="modal-browse"
                disabled={browsing}
                onClick={() => void browse()}
              >
                {browsing ? "…" : "Browse…"}
              </button>
            </div>
          </label>
          {pickerNote && (
            <p className="modal-note">
              The file picker isn't available — type the full path instead.
            </p>
          )}
          {error && (
            <p className="modal-error" role="alert">
              {error}
            </p>
          )}
          <div className="modal-actions">
            <button type="button" className="text-button" onClick={onClose}>
              Cancel
            </button>
            <button type="submit" className="modal-primary" disabled={busy}>
              {busy ? "Adding…" : "Add program"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

/** Button that opens the AddProgramModal. Place it in the overlay header. */
export function AddProgramButton({
  onAdded,
  className,
  children,
}: {
  onAdded: (program: CustomProgram) => void;
  className?: string;
  children?: React.ReactNode;
}) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        type="button"
        className={className ?? "text-button"}
        onClick={() => setOpen(true)}
      >
        {children ?? "Add program"}
      </button>
      <AddProgramModal
        open={open}
        onClose={() => setOpen(false)}
        onAdded={onAdded}
      />
    </>
  );
}

// --- Edit-app handoff ---------------------------------------------------------

/**
 * Window event fired when the launcher asks the library view to open the
 * Edit dialog for an app. App.tsx needs a one-line listener, e.g.:
 *
 *   window.addEventListener("appmaka:edit-app", (e) => {
 *     const id = (e as CustomEvent).detail?.appId as string | undefined;
 *     const app = apps.find((x) => x.id === id);
 *     if (app) setEditingApp(app);
 *   });
 */
export const EDIT_APP_EVENT = "appmaka:edit-app";

/**
 * Switch the main window back to the library view (through the existing
 * `show_library` command, which App.tsx already listens for) and ask it to
 * open the Edit dialog for the app. The dialog itself can only open from
 * App.tsx state, so this dispatches the event above for it to pick up.
 */
export async function requestEditApp(appId: string): Promise<void> {
  try {
    await invoke("show_library");
  } catch {
    // The event below still fires; the view just won't have switched.
  }
  window.dispatchEvent(
    new CustomEvent(EDIT_APP_EVENT, { detail: { appId } })
  );
}

// ---------------------------------------------------------------------------
// v0.7.0 — launcher command bar: built-in commands + quick-add
//
// Pure matching logic plus one small rows component. The overlay wiring
// (App.tsx, owned by the coordinator) is roughly:
//   const results = buildResults(query, apps, programs, sortOpts);
//   const command = matchLauncherCommand(query, results.length === 0);
//   ...render <LauncherCommandRows ref={cmdRef} command={command}
//        onDone={() => { setQuery(""); void invoke("hide_library"); }}
//        onError={setError} /> above/beside the grid when `command` is non-null,
//   and route Enter to cmdRef.current?.activate() while a command is shown.
// Prefix commands (=, >, ?) take precedence over app/program matches;
// quick-add only appears when nothing else matched.
// ---------------------------------------------------------------------------

/** Actions for `system_command`. Single words, so JS casing matches Rust. */
export type SystemAction = "lock" | "sleep" | "shutdown" | "restart";

const SYSTEM_ACTIONS: ReadonlySet<string> = new Set([
  "lock",
  "sleep",
  "shutdown",
  "restart",
]);

/** Trimmed input that looks like a URL, with or without the scheme. */
const URL_LIKE = /^(https?:\/\/)?[\w-]+(\.[\w-]+)+(:\d+)?(\/\S*)?$/;

export type LauncherCommand =
  /** `=2+2` — `value` is null when the expression isn't valid ("Not a calculation"). */
  | { kind: "calc"; expression: string; value: number | null }
  /** `>lock` etc. Shutdown/restart always go through an inline confirm. */
  | { kind: "system"; action: SystemAction }
  /** `>bogus` — hint row naming the known commands. */
  | { kind: "system-unknown"; text: string }
  /** `?query` — web search. */
  | { kind: "web-search"; query: string }
  /** URL-like input with no other matches — "Add <domain> as app…". */
  | { kind: "quick-add"; domain: string; url: string };

/**
 * Match the launcher input against the built-in commands. Pure — no
 * backend calls, ~0 RAM. `hasOtherResults` is whether the normal
 * app/program search already matched something (quick-add only shows
 * when it didn't).
 */
export function matchLauncherCommand(
  query: string,
  hasOtherResults: boolean
): LauncherCommand | null {
  const q = query.trim();
  if (!q) return null;
  const first = q[0];

  if (first === "=") {
    const expression = q.slice(1).trim();
    return {
      kind: "calc",
      expression,
      value: expression ? evaluateExpression(expression) : null,
    };
  }

  if (first === ">") {
    const word = (q.slice(1).trim().toLowerCase().split(/\s+/)[0] ?? "");
    if (SYSTEM_ACTIONS.has(word)) {
      return { kind: "system", action: word as SystemAction };
    }
    return { kind: "system-unknown", text: q.slice(1).trim() };
  }

  if (first === "?") {
    const rest = q.slice(1).trim();
    if (!rest) return null;
    return { kind: "web-search", query: rest };
  }

  if (!hasOtherResults && URL_LIKE.test(q)) {
    const url = /^https?:\/\//i.test(q) ? q : `https://${q}`;
    let domain = q;
    try {
      domain = new URL(url).hostname;
    } catch {
      /* keep the raw input as the label */
    }
    return { kind: "quick-add", domain, url };
  }

  return null;
}

// --- Backend calls (exact invoke signatures; camelCase per the AGENTS.md lesson) ---

/** invoke("system_command", { action }) — action is "lock"|"sleep"|"shutdown"|"restart". */
export async function runSystemCommand(action: SystemAction): Promise<void> {
  await invoke("system_command", { action });
}

/**
 * invoke("open_web_search", { query }) — v0.9.3: the search opens in the
 * shared in-app webview window ("web app"), not the external browser.
 * The backend picks the engine from the launcher settings (DuckDuckGo
 * default, Google selectable) and reuses the single search window.
 */
export async function openWebSearch(query: string): Promise<void> {
  await invoke("open_web_search", { query });
}

/** invoke("preview_start", { url }) — the existing preview/sign-in flow takes over. */
export async function quickAddFromLauncher(url: string): Promise<void> {
  await invoke("preview_start", { url });
}

/** Best-effort clipboard write — never throws (some webviews block it). */
export async function copyCalcResult(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    /* clipboard unavailable; the result is still visible in the row */
  }
}

export interface LauncherCommandRowsHandle {
  /** Activate the command row — the same thing clicking it does. */
  activate: () => void;
}

/**
 * The command rows for the launcher overlay. One row per matched command
 * (calc result, system command, web search, quick-add) or a quiet hint
 * row for unknown/invalid input.
 *
 * Confirm gating (logic-reviewed): the FIRST activation of a shutdown or
 * restart row only ARMS an inline confirm ("Shut down the PC?
 * [Shut down] [Cancel]") and moves focus to the confirm button.
 * `invoke("system_command", { action: "shutdown" | "restart" })` is called
 * solely from that confirm button's onClick — a single Enter can never
 * fire a destructive command. Lock/sleep/web-search/quick-add run on
 * activation directly.
 */
export const LauncherCommandRows = forwardRef<
  LauncherCommandRowsHandle,
  {
    command: LauncherCommand;
    /** Dismiss the overlay after a command ran (e.g. clear query + hide). */
    onDone: () => void;
    onError: (msg: string) => void;
    /** v0.9.3: pin the `?query` row as a launcher tile. */
    onPinSearch?: (query: string) => void;
    /** v0.9.3: true when this exact query is already pinned. */
    searchPinned?: boolean;
  }
>(function LauncherCommandRows({ command, onDone, onError, onPinSearch, searchPinned }, ref) {
  const [confirming, setConfirming] = useState(false);
  const confirmBtnRef = useRef<HTMLButtonElement | null>(null);

  // A new command (the user kept typing) disarms any pending confirm.
  useEffect(() => {
    setConfirming(false);
  }, [command]);

  const fire = useCallback(
    async (action: () => Promise<void>) => {
      try {
        await action();
        onDone();
      } catch (err) {
        onError(errMsg(err));
      }
    },
    [onDone, onError]
  );

  const activate = useCallback(() => {
    switch (command.kind) {
      case "calc":
        if (command.value === null) return; // hint row — nothing to do
        void copyCalcResult(formatCalcResult(command.value)).then(onDone, onDone);
        break;
      case "system":
        if (command.action === "shutdown" || command.action === "restart") {
          setConfirming(true);
          // Keyboard users: land focus on the confirm button so Enter works.
          requestAnimationFrame(() => confirmBtnRef.current?.focus());
        } else {
          void fire(() => runSystemCommand(command.action));
        }
        break;
      case "web-search":
        void fire(() => openWebSearch(command.query));
        break;
      case "quick-add":
        void fire(() => quickAddFromLauncher(command.url));
        break;
      default:
        break; // system-unknown: hint row — nothing to run
    }
  }, [command, fire, onDone]);

  useImperativeHandle(ref, () => ({ activate }), [activate]);

  const systemLabels: Record<SystemAction, { row: string; sub: string }> = {
    lock: { row: ">lock", sub: "Lock the PC" },
    sleep: { row: ">sleep", sub: "Put the PC to sleep" },
    shutdown: { row: ">shutdown", sub: "Shut down the PC" },
    restart: { row: ">restart", sub: "Restart the PC" },
  };

  function rows(): React.ReactNode {
    switch (command.kind) {
      case "calc":
        if (command.value === null) {
          return (
            <div className="cmd-row cmd-hint" role="status">
              <span className="cmd-title">Not a calculation</span>
              <span className="cmd-sub">Try something like =12*8 or =(3+4)/2</span>
            </div>
          );
        }
        return (
          <button type="button" className="cmd-row" onClick={activate}>
            <span className="cmd-title">
              {command.expression} = {formatCalcResult(command.value)}
            </span>
            <span className="cmd-sub">Enter copies the result</span>
          </button>
        );
      case "system": {
        const labels = systemLabels[command.action];
        const needsConfirm =
          command.action === "shutdown" || command.action === "restart";
        if (needsConfirm && confirming) {
          const verb = command.action === "shutdown" ? "Shut down" : "Restart";
          return (
            <div
              className="cmd-row cmd-confirm"
              role="alertdialog"
              aria-label={`${verb} confirmation`}
            >
              <span className="cmd-title">{verb} the PC?</span>
              <span className="cmd-confirm-actions">
                <button
                  type="button"
                  className="cmd-confirm-yes"
                  ref={confirmBtnRef}
                  onClick={() => void fire(() => runSystemCommand(command.action))}
                >
                  {verb}
                </button>
                <button
                  type="button"
                  className="text-button"
                  onClick={() => setConfirming(false)}
                >
                  Cancel
                </button>
              </span>
            </div>
          );
        }
        return (
          <button type="button" className="cmd-row" onClick={activate}>
            <span className="cmd-title">{labels.row}</span>
            <span className="cmd-sub">{labels.sub}</span>
          </button>
        );
      }
      case "system-unknown":
        return (
          <div className="cmd-row cmd-hint" role="status">
            <span className="cmd-title">Unknown command</span>
            <span className="cmd-sub">
              Try &gt;lock, &gt;sleep, &gt;shutdown or &gt;restart.
            </span>
          </div>
        );
      case "web-search":
        return (
          <div className="cmd-row-split">
            <button type="button" className="cmd-row" onClick={activate}>
              <span className="cmd-title">Search the web for &lsquo;{command.query}&rsquo;</span>
              <span className="cmd-sub">Opens as a web app</span>
            </button>
            {!searchPinned && onPinSearch && (
              <button
                type="button"
                className="cmd-pin-btn"
                onClick={() => onPinSearch(command.query)}
                title="Pin this search to the launcher"
              >
                Pin
              </button>
            )}
          </div>
        );
      case "quick-add":
        return (
          <button type="button" className="cmd-row" onClick={activate}>
            <span className="cmd-title">Add {command.domain} as app…</span>
            <span className="cmd-sub">Opens the sign-in flow to add it to your library</span>
          </button>
        );
    }
  }

  return (
    <div className="cmd-rows" aria-label="Commands">
      {rows()}
    </div>
  );
});
