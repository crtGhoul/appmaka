import {
  mockConvertFileSrc,
  mockIPC,
  mockWindows,
} from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import type {
  Account,
  AppSettings,
  ClipboardListEntry,
  ClipboardSettings,
  LauncherSettings,
  NativeProgram,
  Routine,
  WebApp,
} from "./types";

const DEFAULT_APP_SETTINGS: AppSettings = {
  popup_policy: "block",
  popup_allowlist: [],
  adblock_enabled: true,
  auto_suspend_minutes: 30,
  auto_close_minutes: 30,
};

const DEFAULT_LAUNCHER_SETTINGS: LauncherSettings = {
  hotkey: "Alt+Space",
  autostart: false,
  panel_opacity: 0.55,
  dock_enabled: false,
  dock_edge: "bottom",
  dock_hotkey: "",
  dock_profiles: [],
  dock_active_profile: "",
  pinned: [],
  hidden_programs: [],
  usage: {},
  seen_intro: true,
  monitor_mode: "cursor",
  auto_update_check: false,
  auto_update_download: true,
  pending_update_version: null,
  search_engine: "duckduckgo",
  custom_cursor: "off",
  custom_cursor_color: "violet",
  open_as_tabbed: false,
  allow_popups_globally: false,
  accent: "indigo",
  theme: "system",
  tile_size: "cozy",
  tile_corners: "round",
  launcher_background: "frosted",
  show_badges: true,
  show_tile_ram: true,
  show_sleep_labels: true,
  dictation_compat: false,
  hidden_section_collapsed: null,
  startup_mode: "restore",
};

function readStorage<T>(key: string, fallback: T): T {
  try {
    const raw = window.localStorage.getItem(key);
    if (!raw) return fallback;
    return JSON.parse(raw) as T;
  } catch {
    return fallback;
  }
}

function writeStorage<T>(key: string, value: T): T {
  try {
    window.localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // ignore storage quota errors
  }
  return value;
}

interface TauriInternalsLike {
  invoke: (
    cmd: string,
    args?: Record<string, unknown>,
    options?: unknown
  ) => Promise<unknown>;
  transformCallback: (
    callback?: (data: unknown) => unknown,
    once?: boolean
  ) => number;
  unregisterCallback: (id: number) => void;
  runCallback: (id: number, data: unknown) => void;
  callbacks: Map<number, (data: unknown) => unknown>;
  convertFileSrc: (filePath: string, protocol?: string) => string;
  metadata: {
    currentWindow: { label: string };
    currentWebview: { windowLabel: string; label: string };
  };
  plugins?: {
    path?: {
      sep?: string;
      delimiter?: string;
    };
  };
}

interface TauriEventInternalsLike {
  unregisterListener: (event: string, id: number) => void;
}

declare global {
  interface Window {
    __APPMAKA_INTERNALS__?: TauriInternalsLike;
    __APPMAKA_EVENT_INTERNALS__?: TauriEventInternalsLike;
  }
}

function getHostTauriInternals(): TauriInternalsLike | undefined {
  return (window as unknown as { __TAURI_INTERNALS__?: TauriInternalsLike })
    .__TAURI_INTERNALS__;
}

function getHostTauriEventInternals(): TauriEventInternalsLike | undefined {
  return (
    window as unknown as {
      __TAURI_EVENT_PLUGIN_INTERNALS__?: TauriEventInternalsLike;
    }
  ).__TAURI_EVENT_PLUGIN_INTERNALS__;
}

function isNativeAppMakaWindow(): boolean {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) {
    return false;
  }
  try {
    if (window.self !== window.top) {
      return false;
    }
  } catch {
    return false;
  }
  const host = window.location.hostname;
  const port = window.location.port;
  const isTauriOrigin =
    window.location.protocol === "tauri:" ||
    host === "tauri.localhost" ||
    (host === "localhost" && port === "1420") ||
    (host === "127.0.0.1" && port === "1420");
  if (!isTauriOrigin) {
    return false;
  }
  const label =
    getHostTauriInternals()?.metadata?.currentWindow?.label ?? "";
  return (
    label === "main" ||
    label === "clipboard" ||
    label === "dock" ||
    label.startsWith("tabstrip-") ||
    label.startsWith("preview-")
  );
}

if (typeof window !== "undefined") {
  const nativeTauri = isNativeAppMakaWindow()
    ? getHostTauriInternals()
    : undefined;
  const nativeEventTauri = isNativeAppMakaWindow()
    ? getHostTauriEventInternals()
    : undefined;

  mockWindows("main");
  mockConvertFileSrc("linux");

  let apps = readStorage<WebApp[]>("appmaka:apps", []);
  let programs = readStorage<NativeProgram[]>("appmaka:programs", []);
  let launcherSettings = {
    ...DEFAULT_LAUNCHER_SETTINGS,
    ...readStorage<Partial<LauncherSettings>>("appmaka:launcher", {}),
  };
  let routines = readStorage<Routine[]>("appmaka:routines", []);
  let workspaces = readStorage<{
    active_workspace_id: string | null;
    workspaces: Array<{
      id: string;
      name: string;
      hotkey: string | null;
      members: Array<{ app_id: string; account_id: string | null }>;
    }>;
  }>("appmaka:workspaces", { active_workspace_id: null, workspaces: [] });
  let cmdHotkeys = readStorage<
    Array<{
      id: string;
      app_id: string;
      account_id: string | null;
      hotkey: string;
    }>
  >("appmaka:cmdhotkeys", []);
  let linkConfig = readStorage<{
    optIn: boolean;
    rules: Array<{ domain: string; appId: string; accountId: string }>;
  }>("appmaka:links", { optIn: false, rules: [] });
  let clipboardSettings = readStorage<ClipboardSettings>(
    "appmaka:clipboard-settings",
    {
      cap: 100,
      hotkey: "Ctrl+Shift+V",
      winTap: false,
      winTapSupported: false,
      popupTab: "all",
      popupPinnedOnly: false,
    }
  );
  let clipboardEntries = readStorage<ClipboardListEntry[]>(
    "appmaka:clipboard-entries",
    []
  );
  let searchIndexCache: string | null = readStorage<string | null>(
    "appmaka:search-index",
    null
  );

  mockIPC(
    async (cmd, rawArgs) => {
      const args = (rawArgs ?? {}) as Record<string, unknown>;
      switch (cmd) {
        case "plugin:app|version":
          return "0.18.6";
        case "plugin:window|set_size":
        case "plugin:window|set_decorations":
        case "plugin:window|center":
        case "plugin:window|show":
        case "plugin:window|hide":
        case "plugin:window|set_focus":
          return null;
        case "platform_info":
          return {
            os: "linux",
            network_adblock: false,
            filter_lists_loaded: true,
            filter_lists_state: "loaded",
            filter_lists_updated_at: null,
          };
        case "list_apps":
          return apps;
        case "add_app": {
          const name = String(args.name ?? "").trim() || "Web App";
          const url = String(args.url ?? "").trim();
          const existing = apps.find((a) => a.url === url);
          if (existing) {
            return { app: existing, created: false, added_account: null };
          }
          const appId = `app-${Date.now()}`;
          const acct: Account = {
            id: `acct-${Date.now()}`,
            app_id: appId,
            label: "Main",
            color: "#6366f1",
            session_dir: appId,
            thumbnail: null,
            last_opened: 0,
            created_at: Math.floor(Date.now() / 1000),
          };
          const created: WebApp = {
            id: appId,
            name,
            url,
            icon: null,
            color: "#6366f1",
            settings: { ...DEFAULT_APP_SETTINGS },
            accounts: [acct],
            created_at: Math.floor(Date.now() / 1000),
          };
          apps = writeStorage("appmaka:apps", [...apps, created]);
          return { app: created, created: true, added_account: acct };
        }
        case "update_app": {
          const appId = String(args.appId ?? "");
          apps = writeStorage(
            "appmaka:apps",
            apps.map((a) =>
              a.id === appId
                ? {
                    ...a,
                    name: String(args.name ?? a.name),
                    url: String(args.url ?? a.url),
                    color: String(args.color ?? a.color),
                  }
                : a
            )
          );
          return apps.find((a) => a.id === appId) ?? null;
        }
        case "rename_app": {
          const id = String(args.id ?? "");
          apps = writeStorage(
            "appmaka:apps",
            apps.map((a) =>
              a.id === id ? { ...a, name: String(args.name ?? a.name) } : a
            )
          );
          return apps.find((a) => a.id === id) ?? null;
        }
        case "remove_app": {
          const id = String(args.id ?? "");
          apps = writeStorage(
            "appmaka:apps",
            apps.filter((a) => a.id !== id)
          );
          return null;
        }
        case "update_app_settings": {
          const id = String(args.id ?? "");
          const settings = args.settings as AppSettings;
          apps = writeStorage(
            "appmaka:apps",
            apps.map((a) => (a.id === id ? { ...a, settings } : a))
          );
          return settings;
        }
        case "add_account": {
          const appId = String(args.appId ?? "");
          const acct: Account = {
            id: `acct-${Date.now()}`,
            app_id: appId,
            label: String(args.label ?? "Account"),
            color: String(args.color ?? "#6366f1"),
            session_dir: `${appId}-${Date.now()}`,
            thumbnail: null,
            last_opened: 0,
            created_at: Math.floor(Date.now() / 1000),
          };
          apps = writeStorage(
            "appmaka:apps",
            apps.map((a) =>
              a.id === appId ? { ...a, accounts: [...a.accounts, acct] } : a
            )
          );
          return acct;
        }
        case "rename_account": {
          const appId = String(args.appId ?? "");
          const accountId = String(args.accountId ?? "");
          const label = String(args.label ?? "");
          let updatedAcct: Account | null = null;
          apps = writeStorage(
            "appmaka:apps",
            apps.map((a) =>
              a.id === appId
                ? {
                    ...a,
                    accounts: a.accounts.map((ac) => {
                      if (ac.id === accountId) {
                        updatedAcct = { ...ac, label };
                        return updatedAcct;
                      }
                      return ac;
                    }),
                  }
                : a
            )
          );
          return updatedAcct;
        }
        case "update_account": {
          const appId = String(args.appId ?? "");
          const accountId = String(args.accountId ?? "");
          apps = writeStorage(
            "appmaka:apps",
            apps.map((a) =>
              a.id === appId
                ? {
                    ...a,
                    accounts: a.accounts.map((ac) =>
                      ac.id === accountId
                        ? {
                            ...ac,
                            label: String(args.label ?? ac.label),
                            color: String(args.color ?? ac.color),
                            popup_policy:
                              (args.popupPolicy as "block" | "allow" | null) ??
                              null,
                            auto_suspend_minutes:
                              (args.autoSuspendMinutes as number | null) ??
                              null,
                            auto_close_minutes:
                              (args.autoCloseMinutes as number | null) ?? null,
                            adblock_enabled:
                              (args.adblockEnabled as boolean | null) ?? null,
                          }
                        : ac
                    ),
                  }
                : a
            )
          );
          return apps.find((a) => a.id === appId) ?? null;
        }
        case "remove_account": {
          const appId = String(args.appId ?? "");
          const accountId = String(args.accountId ?? "");
          apps = writeStorage(
            "appmaka:apps",
            apps.map((a) =>
              a.id === appId
                ? {
                    ...a,
                    accounts: a.accounts.filter((ac) => ac.id !== accountId),
                  }
                : a
            )
          );
          return null;
        }
        case "open_account":
        case "open_app_in_tabbed_window":
        case "open_tabbed_window":
        case "suspend_account":
        case "account_window_command":
        case "fix_account_window":
        case "forget_login":
        case "record_launch":
        case "hide_library":
          return null;
        case "show_library":
          void emit("appmaka:show-library", null);
          return null;
        case "fetch_page_title": {
          try {
            const u = new URL(String(args.url ?? ""));
            const host = u.hostname.replace(/^www\./, "");
            return host.charAt(0).toUpperCase() + host.slice(1);
          } catch {
            return "";
          }
        }
        case "fetch_favicon":
        case "clear_app_icon":
          return null;
        case "list_programs":
          return programs;
        case "rescan_programs":
          return programs.length;
        case "add_custom_program": {
          const created: NativeProgram = {
            id: `custom-${Date.now()}`,
            name: String(args.name ?? "Program"),
            exe_path: String(args.exePath ?? ""),
            icon_path: null,
            is_custom: true,
          };
          programs = writeStorage("appmaka:programs", [...programs, created]);
          return created;
        }
        case "remove_custom_program": {
          const programId = String(args.programId ?? "");
          programs = writeStorage(
            "appmaka:programs",
            programs.filter((p) => p.id !== programId)
          );
          return null;
        }
        case "pick_executable":
          return null;
        case "get_launcher_settings":
          return launcherSettings;
        case "get_hotkey_status":
          return {
            hotkey: launcherSettings.hotkey,
            registered: true,
            error: null,
          };
        case "validate_hotkey":
          return null;
        case "set_hotkey":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            hotkey: String(args.hotkey ?? launcherSettings.hotkey),
          });
          return null;
        case "set_autostart":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            autostart: Boolean(args.enabled),
          });
          return null;
        case "set_startup_mode":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            startup_mode: (args.mode as "restore" | "ask" | "fresh") ?? "restore",
          });
          return launcherSettings;
        case "set_panel_opacity":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            panel_opacity: Number(args.opacity ?? 0.55),
          });
          return launcherSettings;
        case "toggle_pin": {
          const itemId = String(args.itemId ?? "");
          const pinned = launcherSettings.pinned.includes(itemId)
            ? launcherSettings.pinned.filter((p) => p !== itemId)
            : [...launcherSettings.pinned, itemId];
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            pinned,
          });
          return launcherSettings;
        }
        case "set_program_hidden": {
          const programId = String(args.programId ?? "");
          const hidden = Boolean(args.hidden);
          const hidden_programs = hidden
            ? Array.from(new Set([...launcherSettings.hidden_programs, programId]))
            : launcherSettings.hidden_programs.filter((id) => id !== programId);
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            hidden_programs,
          });
          return launcherSettings;
        }
        case "set_monitor_mode":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            monitor_mode: (args.mode as "cursor" | "primary") ?? "cursor",
          });
          return launcherSettings;
        case "set_seen_intro":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            seen_intro: Boolean(args.seen),
          });
          return launcherSettings;
        case "set_auto_update_check":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            auto_update_check: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_auto_update_download":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            auto_update_download: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_pending_update_version":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            pending_update_version: (args.version as string | null) ?? null,
          });
          return launcherSettings;
        case "set_search_engine":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            search_engine:
              (args.engine as "duckduckgo" | "google") ?? "duckduckgo",
          });
          return launcherSettings;
        case "set_custom_cursor":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            custom_cursor:
              (args.style as LauncherSettings["custom_cursor"]) ?? "off",
          });
          return launcherSettings;
        case "set_custom_cursor_color":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            custom_cursor_color:
              (args.color as LauncherSettings["custom_cursor_color"]) ??
              "violet",
          });
          return launcherSettings;
        case "set_accent":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            accent: (args.accent as LauncherSettings["accent"]) ?? "indigo",
          });
          return launcherSettings;
        case "set_theme":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            theme: (args.theme as LauncherSettings["theme"]) ?? "system",
          });
          return launcherSettings;
        case "set_tile_size":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            tile_size: (args.size as LauncherSettings["tile_size"]) ?? "cozy",
          });
          return launcherSettings;
        case "set_tile_corners":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            tile_corners:
              (args.corners as LauncherSettings["tile_corners"]) ?? "round",
          });
          return launcherSettings;
        case "set_launcher_background":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            launcher_background:
              (args.background as LauncherSettings["launcher_background"]) ??
              "frosted",
          });
          return launcherSettings;
        case "set_show_badges":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            show_badges: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_show_tile_ram":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            show_tile_ram: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_show_sleep_labels":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            show_sleep_labels: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_dictation_compat":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            dictation_compat: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_open_as_tabbed":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            open_as_tabbed: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_allow_popups_globally":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            allow_popups_globally: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_hidden_section_collapsed":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            hidden_section_collapsed: Boolean(args.collapsed),
          });
          return launcherSettings;
        case "set_dock_enabled":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            dock_enabled: Boolean(args.enabled),
          });
          return launcherSettings;
        case "set_dock_edge":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            dock_edge: String(args.edge ?? "bottom"),
          });
          return launcherSettings;
        case "set_dock_hotkey":
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            dock_hotkey: String(args.hotkey ?? ""),
          });
          return {
            hotkey: launcherSettings.dock_hotkey,
            registered: true,
            error: null,
          };
        case "save_dock_profile": {
          const name = String(args.name ?? "").trim();
          const profile = {
            name,
            edge: launcherSettings.dock_edge,
            pins: [...launcherSettings.pinned],
          };
          const filtered = launcherSettings.dock_profiles.filter(
            (p) => p.name !== name
          );
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            dock_profiles: [...filtered, profile],
            dock_active_profile: name,
          });
          return launcherSettings;
        }
        case "apply_dock_profile": {
          const name = String(args.name ?? "");
          const found = launcherSettings.dock_profiles.find(
            (p) => p.name === name
          );
          if (found) {
            launcherSettings = writeStorage("appmaka:launcher", {
              ...launcherSettings,
              dock_edge: found.edge,
              pinned: [...found.pins],
              dock_active_profile: found.name,
            });
          }
          return launcherSettings;
        }
        case "delete_dock_profile": {
          const name = String(args.name ?? "");
          launcherSettings = writeStorage("appmaka:launcher", {
            ...launcherSettings,
            dock_profiles: launcherSettings.dock_profiles.filter(
              (p) => p.name !== name
            ),
            dock_active_profile:
              launcherSettings.dock_active_profile === name
                ? ""
                : launcherSettings.dock_active_profile,
          });
          return launcherSettings;
        }
        case "get_pending_session_restore":
          return null;
        case "restore_session":
          return 0;
        case "dismiss_session_restore":
          return null;
        case "list_open_account_windows":
        case "account_memory":
        case "sleep_state":
        case "list_tabbed_windows":
        case "get_recent_errors":
        case "list_user_scripts":
        case "reload_user_scripts":
          return [];
        case "set_user_script_enabled":
          return [];
        case "get_userscripts_dir":
          return null;
        case "memory_snapshot":
          return {
            totalRssKb: 0,
            mainRssKb: 0,
            topChildren: [],
            supported: false,
          };
        case "list_routines":
          return routines;
        case "save_routine": {
          const r = args.routine as Routine;
          const id = r.id || `routine-${Date.now()}`;
          const saved = { ...r, id };
          routines = writeStorage(
            "appmaka:routines",
            routines.some((x) => x.id === id)
              ? routines.map((x) => (x.id === id ? saved : x))
              : [...routines, saved]
          );
          return routines;
        }
        case "delete_routine": {
          const id = String(args.routineId ?? "");
          routines = writeStorage(
            "appmaka:routines",
            routines.filter((x) => x.id !== id)
          );
          return routines;
        }
        case "run_routine":
          return "Opened routine";
        case "list_workspaces":
          return workspaces;
        case "save_workspace": {
          const ws = args.workspace as {
            id: string;
            name: string;
            hotkey: string | null;
            members: Array<{ app_id: string; account_id: string | null }>;
          };
          const id = ws.id || `ws-${Date.now()}`;
          const saved = { ...ws, id };
          const list = workspaces.workspaces.some((x) => x.id === id)
            ? workspaces.workspaces.map((x) => (x.id === id ? saved : x))
            : [...workspaces.workspaces, saved];
          workspaces = writeStorage("appmaka:workspaces", {
            ...workspaces,
            workspaces: list,
          });
          return workspaces;
        }
        case "delete_workspace": {
          const id = String(args.workspaceId ?? "");
          workspaces = writeStorage("appmaka:workspaces", {
            active_workspace_id:
              workspaces.active_workspace_id === id
                ? null
                : workspaces.active_workspace_id,
            workspaces: workspaces.workspaces.filter((x) => x.id !== id),
          });
          return workspaces;
        }
        case "set_active_workspace": {
          const id = (args.workspaceId as string | null) ?? null;
          workspaces = writeStorage("appmaka:workspaces", {
            ...workspaces,
            active_workspace_id: id,
          });
          return workspaces;
        }
        case "create_workspace_from_open":
          return workspaces;
        case "list_cmdhotkeys":
          return cmdHotkeys;
        case "save_cmdhotkey": {
          const entry = {
            id: String(args.id || `cmd-${Date.now()}`),
            app_id: String(args.appId ?? ""),
            account_id: (args.accountId as string | null) ?? null,
            hotkey: String(args.hotkey ?? ""),
          };
          cmdHotkeys = writeStorage("appmaka:cmdhotkeys", [
            ...cmdHotkeys.filter((x) => x.id !== entry.id),
            entry,
          ]);
          return cmdHotkeys;
        }
        case "delete_cmdhotkey": {
          const id = String(args.id ?? "");
          cmdHotkeys = writeStorage(
            "appmaka:cmdhotkeys",
            cmdHotkeys.filter((x) => x.id !== id)
          );
          return cmdHotkeys;
        }
        case "get_binding_status":
          return null;
        case "get_link_config":
          return linkConfig;
        case "set_link_opt_in":
          linkConfig = writeStorage("appmaka:links", {
            ...linkConfig,
            optIn: Boolean(args.enabled),
          });
          return linkConfig;
        case "add_link_rule": {
          const rule = {
            domain: String(args.domain ?? ""),
            appId: String(args.appId ?? ""),
            accountId: String(args.accountId ?? ""),
          };
          linkConfig = writeStorage("appmaka:links", {
            ...linkConfig,
            rules: [
              ...linkConfig.rules.filter((r) => r.domain !== rule.domain),
              rule,
            ],
          });
          return linkConfig.rules;
        }
        case "remove_link_rule": {
          const domain = String(args.domain ?? "");
          linkConfig = writeStorage("appmaka:links", {
            ...linkConfig,
            rules: linkConfig.rules.filter((r) => r.domain !== domain),
          });
          return linkConfig.rules;
        }
        case "list_downloads":
          return [];
        case "get_download_dir":
          return "~/Downloads";
        case "get_download_settings":
          return { askWhereToSave: false, showCompletionNotice: true };
        case "set_ask_where_to_save":
        case "set_show_completion_notice":
          return {
            askWhereToSave: Boolean(args.ask),
            showCompletionNotice: Boolean(args.show ?? true),
          };
        case "get_clipboard_settings":
          return clipboardSettings;
        case "clipboard_hotkey_status":
          return {
            hotkey: clipboardSettings.hotkey,
            registered: true,
            error: null,
          };
        case "set_clipboard_hotkey":
          clipboardSettings = writeStorage("appmaka:clipboard-settings", {
            ...clipboardSettings,
            hotkey: String(args.hotkey ?? ""),
          });
          return clipboardSettings.hotkey;
        case "set_clipboard_cap":
          clipboardSettings = writeStorage("appmaka:clipboard-settings", {
            ...clipboardSettings,
            cap: Number(args.cap ?? 100),
          });
          return clipboardSettings.cap;
        case "set_clipboard_win_tap":
          clipboardSettings = writeStorage("appmaka:clipboard-settings", {
            ...clipboardSettings,
            winTap: Boolean(args.enabled),
          });
          return clipboardSettings.winTap;
        case "list_clipboard":
          return clipboardEntries;
        case "clear_clipboard":
          clipboardEntries = writeStorage("appmaka:clipboard-entries", []);
          return null;
        case "get_dictation_status":
          return {
            ready: false,
            binaryInstalled: false,
            model: "base.en",
            modelInstalled: false,
            recording: false,
            transcribing: false,
            hotkey: "",
            holdToTalk: false,
          };
        case "dictation_hotkey_status":
          return null;
        case "load_search_index":
          return searchIndexCache;
        case "save_search_index":
          searchIndexCache = writeStorage(
            "appmaka:search-index",
            String(args.contents ?? "")
          );
          return null;
        default:
          return null;
      }
    },
    { shouldMockEvents: true }
  );

  const mockInternals = window.__APPMAKA_INTERNALS__;
  const mockEventInternals = window.__APPMAKA_EVENT_INTERNALS__;

  if (nativeTauri && mockInternals) {
    const fallbackInvoke = mockInternals.invoke;
    const fallbackUnregisterListener = mockEventInternals?.unregisterListener;
    window.__APPMAKA_INTERNALS__ = {
      ...mockInternals,
      metadata: nativeTauri.metadata ?? mockInternals.metadata,
      convertFileSrc: nativeTauri.convertFileSrc
        ? nativeTauri.convertFileSrc.bind(nativeTauri)
        : mockInternals.convertFileSrc,
      transformCallback: nativeTauri.transformCallback
        ? nativeTauri.transformCallback.bind(nativeTauri)
        : mockInternals.transformCallback,
      unregisterCallback: nativeTauri.unregisterCallback
        ? nativeTauri.unregisterCallback.bind(nativeTauri)
        : mockInternals.unregisterCallback,
      runCallback: nativeTauri.runCallback
        ? nativeTauri.runCallback.bind(nativeTauri)
        : mockInternals.runCallback,
      callbacks: nativeTauri.callbacks ?? mockInternals.callbacks,
      plugins: nativeTauri.plugins ?? mockInternals.plugins,
      invoke: async (cmd, args, options) => {
        try {
          return await nativeTauri.invoke(cmd, args, options);
        } catch (err) {
          if (String(err).includes("not allowed by ACL")) {
            return fallbackInvoke(cmd, args, options);
          }
          throw err;
        }
      },
    };
    if (nativeEventTauri && mockEventInternals) {
      window.__APPMAKA_EVENT_INTERNALS__ = {
        unregisterListener: (event, id) => {
          try {
            nativeEventTauri.unregisterListener(event, id);
          } catch {
            fallbackUnregisterListener?.(event, id);
          }
        },
      };
    }
  } else if (mockInternals) {
    try {
      const w = window as unknown as Record<string, unknown>;
      if (!("__TAURI_INTERNALS__" in window)) {
        w.__TAURI_INTERNALS__ = mockInternals;
      }
      if (mockEventInternals && !("__TAURI_EVENT_PLUGIN_INTERNALS__" in window)) {
        w.__TAURI_EVENT_PLUGIN_INTERNALS__ = mockEventInternals;
      }
    } catch {
      // Host WebView froze __TAURI_INTERNALS__; @tauri-apps reads __APPMAKA_INTERNALS__.
    }
  }
}

