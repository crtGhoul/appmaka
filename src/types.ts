/**
 * Shared types for the AppMaka library window.
 *
 * Backend contract (implemented by the Rust side — do not extend).
 *
 * IMPORTANT — invoke argument naming: Tauri converts Rust `snake_case`
 * command parameters to `camelCase` for JavaScript. So a command declared as
 * `fn add_account(app_id: String, ...)` MUST be invoked as
 * `invoke("add_account", { appId: ... })` — passing `app_id` fails at
 * RUNTIME with "missing required key appId", and TypeScript cannot catch it
 * (invoke args are not type-checked against the Rust signature). Struct
 * fields (e.g. Account.app_id) are different: they follow serde and stay
 * snake_case in JSON.
 */

export interface AppSettings {
  popup_policy: "block" | "allow";
  popup_allowlist: string[];
  adblock_enabled: boolean;
  auto_suspend_minutes: number;
  /** Minutes of idleness after which an account window is closed (0 = never). */
  auto_close_minutes: number;
}

export interface Account {
  id: string;
  app_id: string;
  label: string;
  color: string;
  session_dir: string;
  /** Locally cached og:image thumbnail for the account tile (may be null). */
  thumbnail: string | null;
  /**
   * Per-account popup policy override: "block" | "allow".
   * `null` (or missing on old records) = inherit the app's setting.
   */
  popup_policy?: "block" | "allow" | null;
  /**
   * Per-account idle-suspend override in minutes (0 = never).
   * `null`/missing = inherit the app's `auto_suspend_minutes`.
   */
  auto_suspend_minutes?: number | null;
  /**
   * Per-account idle-close override in minutes (0 = never).
   * `null`/missing = inherit the app's `auto_close_minutes`.
   */
  auto_close_minutes?: number | null;
  /**
   * Per-account adblock override. `null`/missing = inherit the app's
   * `adblock_enabled`.
   */
  adblock_enabled?: boolean | null;
  last_opened: number;
  created_at: number;
}

export interface WebApp {
  id: string;
  name: string;
  url: string;
  icon: string | null;
  color: string;
  settings: AppSettings;
  accounts: Account[];
  created_at: number;
}

export interface PlatformInfo {
  os: string;
  network_adblock: boolean;
  filter_lists_loaded: boolean;
  filter_lists_updated_at: number | null;
}

/** A native installed program found by the Start Menu / .desktop scan. */
export interface NativeProgram {
  id: string;
  name: string;
  exe_path: string;
  icon_path: string | null;
  /** True when the entry was added manually by the user (never wiped by rescans). */
  is_custom: boolean;
}

/** A program added manually by the user, stored in custom-programs.json. */
export interface CustomProgram {
  id: string;
  name: string;
  exe_path: string;
  icon_path: string | null;
}

/** Summon hotkey + run-at-startup + launcher panel look, stored in launcher.json on the backend. */
export interface LauncherSettings {
  hotkey: string;
  autostart: boolean;
  /** Launcher panel translucency, 0.3 (faint) .. 1.0 (solid). Backend always sends it (serde default). */
  panel_opacity: number;
  /**
   * Tagged ids of pinned tiles (`app:<id>`, `account:<id>`, `program:<id>`);
   * pins span all tile kinds, so the kind prefix disambiguates.
   */
  pinned: string[];
  /** Raw program ids (NOT tagged) hidden from the launcher grid. */
  hidden_programs: string[];
  /**
   * Launch frequency, keyed by the same tagged ids as `pinned`
   * (e.g. `app:<id>`), for usage-frequency ranking.
   */
  usage: Record<string, { count: number; last_used: number }>;
  /** Whether the first-run 101 overlay was shown/dismissed (never nag again). */
  seen_intro: boolean;
  /** Which monitor the launcher summons on. */
  monitor_mode: "cursor" | "primary";
  /** Whether to silently check for updates on startup (and every 24h). */
  auto_update_check: boolean;
  /** Web-search engine for the launcher's `?query` command. Backend always sends it (serde default). */
  search_engine: "duckduckgo" | "google";
  /**
   * Custom page cursor for app windows (v0.12.0). Backend always sends it
   * (serde default): "off" | "dot" | "ring" | "trail".
   */
  custom_cursor: "off" | "dot" | "ring" | "trail";
  /**
   * Whether the library's "Hidden programs" list is collapsed. Null when the
   * user never toggled it: the UI then defaults to collapsed whenever the
   * list is non-empty.
   */
  hidden_section_collapsed: boolean | null;
  /**
   * What to do with the previous session at startup (v0.9.5).
   * Backend always sends it (serde default): "restore" | "ask" | "fresh".
   */
  startup_mode: "restore" | "ask" | "fresh";
}

/** One-time session-restore offer for "Ask me" mode (v0.9.5). */
export interface SessionRestoreOffer {
  windowCount: number;
  names: string[];
  hasSearch: boolean;
  /** True when a stale restore sentinel forced ask-mode: the previous run
   *  died inside the restore window, so the UI explains why it didn't
   *  auto-restore (v0.9.7). */
  staleRestore: boolean;
}

/**
 * Runtime snapshot of whether the saved summon hotkey is actually
 * registered with the OS (from `get_hotkey_status`). `registered` is false
 * when startup registration failed — e.g. another app already owns it.
 */
export interface HotkeyStatus {
  hotkey: string;
  registered: boolean;
  error: string | null;
}

/** Result of the preview_start command: a signed-in-later throwaway session. */
export interface PreviewStart {
  id: string;
  url: string;
}

/**
 * Result of add_app / preview_add. `created` is false when the site was
 * already in the library — the backend never creates duplicates. For the
 * preview flow, the signed-in session is always adopted: as the first
 * account of a new app, or as a brand-new account on the existing app
 * (`added_account`, never None there). The quick-add form passes no
 * session, so `added_account` is None when it hits an existing app.
 */
export interface AddAppOutcome {
  app: WebApp;
  created: boolean;
  added_account: Account | null;
}

/**
 * Result of the preview_add command, delivered on the
 * `appmaka:preview-added` event. Serialized camelCase by the backend:
 * `addedAccount` is the account that adopted the preview's signed-in
 * session — the first account for a new app, or a new account when the
 * site was already in the library.
 */
export interface PreviewAddOutcome {
  app: WebApp;
  created: boolean;
  addedAccount: Account | null;
}

// ---------------------------------------------------------------------------
// v0.7.0 — launcher experience (owned by Worker C)
// ---------------------------------------------------------------------------

/** One open account window, from `list_open_account_windows`. */
export interface OpenAccountWindow {
  label: string;
  appId: string;
  accountId: string;
  appName: string;
  accountLabel: string;
  focused: boolean;
  /** v0.9.9: "Don't close this window" state. */
  pinned: boolean;
}

/** v0.10.0: one open tabbed window, from `list_tabbed_windows`. */
export interface TabSummary {
  appId: string;
  accountId: string;
  appName: string;
  accountLabel: string;
}

export interface TabbedWindowInfo {
  groupId: string;
  label: string;
  tabs: TabSummary[];
  active: number;
  tint: string | null;
  focused: boolean;
  pinned: boolean;
}

/**
 * Process memory snapshot, from `memory_snapshot`. Keys are camelCase in
 * JSON because Tauri serializes Rust `total_rss_kb` as `totalRssKb`.
 * All numbers are RSS in kibibytes; treat them as approximate.
 */
export interface MemorySnapshot {
  totalRssKb: number;
  mainRssKb: number;
  topChildren: Array<{ name: string; rssKb: number }>;
  /** False on platforms where memory numbers aren't available. */
  supported: boolean;
}

// ---------------------------------------------------------------------------
// v0.8.0 — routines ("morning stack")
// ---------------------------------------------------------------------------

/**
 * One item in a routine. `kind` is "account" (opens app_id + account_id in
 * an account window) or "program" (launches the native program_id).
 * Struct fields stay snake_case in JSON (serde) — unlike invoke args,
 * which are camelCase.
 */
export interface RoutineItem {
  kind: "account" | "program";
  app_id: string;
  account_id: string | null;
  program_id: string | null;
}

/** A named, hotkey-able set of things to open together. */
export interface Routine {
  id: string;
  name: string;
  /** Global hotkey like "Ctrl+Alt+M", or null for none. */
  hotkey: string | null;
  /** How the routine arranges its windows: "cascade" (overlap, default) or
   * "side_by_side" (tile as equal columns). Missing on old records means
   * cascade. */
  layout: RoutineLayout;
  items: RoutineItem[];
}

/** Routine window layout. Serialized snake_case in routines.json. */
export type RoutineLayout = "cascade" | "side_by_side";

// ---------------------------------------------------------------------------

/**
 * One clipboard history entry as returned by `list_clipboard`. Text entries
 * carry a 500-char `preview`; image entries carry `imagePath` (absolute PNG
 * path — the UI turns it into a loadable URL with convertFileSrc) and
 * dimensions. The full content is copied back by id.
 */
export interface ClipboardListEntry {
  id: string;
  kind: "text" | "image";
  preview: string;
  chars: number;
  truncated: boolean;
  /** v0.9.12: user-pinned; persisted across restarts. */
  pinned: boolean;
  createdAtMs: number;
  imagePath: string | null;
  width: number | null;
  height: number | null;
}

/** Clipboard settings payload from `get_clipboard_settings`. */
export interface ClipboardSettings {
  cap: number;
  hotkey: string;
  /** v0.9.2: summon by tapping the bare Windows key instead of a combo. */
  winTap: boolean;
  /** The Win-key tap needs a low-level keyboard hook: Windows only. */
  winTapSupported: boolean;
  /** v0.9.4: popup tab ("all" | "text" | "image"), persisted. */
  popupTab: string;
  /** v0.9.12: "Pinned only" filter switch, persisted. */
  popupPinnedOnly: boolean;
}
