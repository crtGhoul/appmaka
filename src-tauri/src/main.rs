#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adblock;
mod clipboard;
mod custom_programs;
mod downloads;
mod favicon;
mod favicon_parse;
mod launcher;
mod launcher_ext;
mod hotkeys;
mod workspaces;
mod launcher_settings;
mod links;
mod msi_update;
mod page_title;
mod pin;
mod preview;
mod routines;
mod session;
mod store;
mod syscmd;
mod websearch;
mod windows;
/// Bare-Windows-key tap summon for the clipboard popup (v0.9.2).
/// The tap classifier is pure logic and compiles everywhere (unit-tested
/// on all platforms); only the WH_KEYBOARD_LL machinery inside is
/// cfg(windows). Nothing references the pure items on a non-test Linux
/// build, so dead_code is allowed there — and only there, so a genuinely
/// dead item still warns on Windows and in tests.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
mod winkey;
/// Slim native caption strips + the Esc+LMB close gesture for page windows
/// (v0.9.6, Windows only; no-op stubs elsewhere).
mod caption;
/// "Add to applications" for popup windows (v0.9.11): backend flow shared
/// by the dashboard button, the system-menu item and Ctrl+Shift+A.
mod popup_add;
/// Windows-only native popup tweaks (v0.9.11): system-menu item +
/// title-bar theme-color tint. The module itself is cfg'd out elsewhere.
#[cfg(windows)]
mod popup_chrome;
/// Debug-only E2E driver for the v0.9.11 popup revamp (Xvfb smoke).
/// Compiled out of release builds; inert without the env var.
#[cfg(debug_assertions)]
mod debug_e2e;
/// Clipboard image reading beyond the plugin's format list (v0.9.2):
/// direct DIB reads on Windows, image/bmp fallback on Linux.
mod clipboard_img;
/// Tabbed app windows (v0.10.0): one window, many apps, one live webview.
/// The pure tab logic compiles everywhere (unit-tested on all platforms);
/// only the Win32 strip painting/hook and the Linux strip window are
/// platform-gated inside.
mod tabs;

/// Copyable error dialogs (v0.11.0): central recent-errors ring buffer,
/// `appmaka:error` event for the dialog, copy-diagnostics commands.
mod errors;

/// Universal title-bar blending (v0.11.0): one fallback chain
/// (theme-color meta → live-DOM probe → default) for every window type.
mod tint;

use adblock::AdblockState;
use launcher::{LauncherState, NativeProgram};
use launcher_settings::{HotkeyStatus, LauncherSettings};
use preview::PreviewState;
use serde::Serialize;
use std::sync::Mutex;
use store::{Account, AppSettings, AppStore, WebApp};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use windows::WindowState;

#[tauri::command]
fn list_apps(store: State<'_, AppStore>) -> Result<Vec<WebApp>, String> {
    store.list()
}

#[tauri::command]
fn add_app(name: String, url: String, store: State<'_, AppStore>) -> Result<store::AddAppOutcome, String> {
    store.add_app(name, url)
}

#[tauri::command]
fn update_app(
    app_id: String,
    name: String,
    url: String,
    color: String,
    store: State<'_, AppStore>,
) -> Result<WebApp, String> {
    store.update_app(&app_id, name, url, color)
}

/// Rename an app (name only). Tauri exposes `id`/`name` as-is to JS.
#[tauri::command]
fn rename_app(id: String, name: String, store: State<'_, AppStore>) -> Result<WebApp, String> {
    store.rename_app(&id, name)
}

#[tauri::command]
fn remove_app(
    app: AppHandle,
    id: String,
    store: State<'_, AppStore>,
) -> Result<(), String> {
    // Windows lock the session files while a webview is alive, so close the
    // app's windows before the store deletes their session directories.
    windows::close_account_windows(&app, &id);
    store.remove_app(&id)
}

#[tauri::command]
fn update_app_settings(
    app: AppHandle,
    id: String,
    settings: AppSettings,
    store: State<'_, AppStore>,
) -> Result<AppSettings, String> {
    let updated = store.update_app_settings(&id, settings)?;
    // Push the adblock toggle to already-open windows; no rebuild needed.
    windows::set_app_adblock_enabled(&app, &id, updated.adblock_enabled);
    Ok(updated)
}

#[tauri::command]
async fn add_account(
    app: AppHandle,
    app_id: String,
    label: String,
    color: Option<String>,
) -> Result<Account, String> {
    // ASYNC ON PURPOSE: account creation fetches the site's og:image for
    // the tile thumbnail — a sync command would block the WebView2 IPC
    // thread while the HTTP fetch runs.
    let app_c = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = app_c.state::<AppStore>();
        store.add_account(&app_id, label, color)
    })
    .await
    .map_err(|e| format!("add account failed: {e}"))?
}

#[tauri::command]
fn remove_account(
    app: AppHandle,
    app_id: String,
    account_id: String,
    store: State<'_, AppStore>,
) -> Result<(), String> {
    windows::close_account_window(&app, &app_id, &account_id);
    store.remove_account(&app_id, &account_id)
}

/// Rename an account (label only). Tauri exposes the snake_case params as
/// camelCase to JS: `invoke("rename_account", { appId, accountId, label })`.
#[tauri::command]
fn rename_account(
    app_id: String,
    account_id: String,
    label: String,
    store: State<'_, AppStore>,
) -> Result<Account, String> {
    store.rename_account(&app_id, &account_id, label)
}

/// Edit an account's label, color, and per-account popup policy override.
/// Tauri exposes the snake_case params as camelCase to JS: invoke as
/// `invoke("update_account", { appId, accountId, label, color, popupPolicy })`
/// — `popupPolicy` is `null` for "use app setting", `"block"` or `"allow"`
/// for an override.
/// Per-account edit: label, color, popup-policy override, idle-timer
/// overrides, adblock override. Params stay flat (1:1 with the JS invoke
/// args) — hence the arity allow.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn update_account(
    app: AppHandle,
    app_id: String,
    account_id: String,
    label: String,
    color: String,
    popup_policy: Option<String>,
    auto_suspend_minutes: Option<u64>,
    auto_close_minutes: Option<u32>,
    adblock_enabled: Option<bool>,
    store: State<'_, AppStore>,
) -> Result<WebApp, String> {
    let updated = store.update_account(
        &app_id,
        &account_id,
        label,
        color,
        popup_policy,
        auto_suspend_minutes,
        auto_close_minutes,
        adblock_enabled,
    )?;
    // Push the account's effective adblock value to its open window, if
    // any — the per-account mirror of the app-level push in
    // update_app_settings. Timers need no push: the watchdog re-reads the
    // store every minute.
    windows::set_account_adblock_enabled(
        &app,
        &app_id,
        &account_id,
        updated.effective_adblock_enabled(&account_id),
    );
    Ok(updated)
}

/// Opens the account's window. ASYNC ON PURPOSE: on Windows,
/// `WebviewWindowBuilder::build()` deadlocks when called from a synchronous
/// Tauri command (the command body runs on a WebView2 IPC thread; see
/// wry#583 and the "Known issues" note on WebviewWindowBuilder::new).
/// An async command moves the blocking build onto a tokio worker thread so
/// the IPC thread stays free and WebView2 can complete initialization.
/// A sync version of this produced black, unclosable windows — do not
/// "simplify" this back to a sync fn.
#[tauri::command]
async fn open_account(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    winstate: State<'_, WindowState>,
    app_id: String,
    account_id: String,
) -> Result<(), String> {
    windows::open_account(&app, &store, &adblock, &winstate, &app_id, &account_id)
}

// ---------------------------------------------------------------------------
// v0.10.0: tabbed app windows — command wrappers live here (like
// open_account above); the core logic lives in tabs.rs.
// ---------------------------------------------------------------------------

/// Open a new tabbed window. Starts empty (about:blank); the user adds
/// tabs via +. Async: builds a window (never on a sync IPC thread).
/// JS: `invoke("open_tabbed_window", {})`
#[tauri::command]
async fn open_tabbed_window(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    tabstate: State<'_, tabs::TabState>,
) -> Result<tabs::TabInfo, String> {
    tabs::open_tabbed_window(&app, &store, &adblock, &tabstate, tabs::OpenTabbedParams::default())
}

/// Open a new tabbed window with the given app/account as the first tab.
/// JS: `invoke("open_app_in_tabbed_window", { appId, accountId })`
/// Async: builds a window (never on a sync IPC thread).
#[tauri::command]
async fn open_app_in_tabbed_window(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    tabstate: State<'_, tabs::TabState>,
    app_id: String,
    account_id: String,
) -> Result<tabs::TabInfo, String> {
    tabs::open_tabbed_window(
        &app,
        &store,
        &adblock,
        &tabstate,
        tabs::OpenTabbedParams {
            initial: vec![tabs::TabEntry {
                app_id,
                account_id,
                last_url: None,
            }],
            active: 0,
            placement: None,
            restore_id: None,
        },
    )
}

/// Switch the active tab (rebuilds the webview on the new tab's session).
/// JS: `invoke("switch_tab", { groupId, index })`
#[tauri::command]
async fn switch_tab(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    tabstate: State<'_, tabs::TabState>,
    group_id: String,
    index: usize,
) -> Result<tabs::TabInfo, String> {
    tabs::switch_tab(&app, &store, &adblock, &tabstate, &group_id, index)
}

/// Add an (app, account) tab and switch to it.
/// JS: `invoke("add_tab", { groupId, appId, accountId })`
#[tauri::command]
async fn add_tab(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    tabstate: State<'_, tabs::TabState>,
    group_id: String,
    app_id: String,
    account_id: String,
) -> Result<tabs::TabInfo, String> {
    tabs::add_tab(
        &app,
        &store,
        &adblock,
        &tabstate,
        &group_id,
        &app_id,
        &account_id,
    )
}

/// Close one tab. Closing the last tab closes the group.
/// JS: `invoke("close_tab", { groupId, index })`
#[tauri::command]
async fn close_tab(
    app: AppHandle,
    store: State<'_, AppStore>,
    adblock: State<'_, AdblockState>,
    tabstate: State<'_, tabs::TabState>,
    group_id: String,
    index: usize,
) -> Result<tabs::TabInfo, String> {
    tabs::close_tab(&app, &store, &adblock, &tabstate, &group_id, index)
}

/// Close a whole tabbed window (dashboard row / strip X). Pinned groups
/// get the one native confirm. Sync: closing never deadlocks.
/// JS: `invoke("close_tabbed_window", { groupId })`
#[tauri::command]
fn close_tabbed_window(
    app: AppHandle,
    tabstate: State<'_, tabs::TabState>,
    group_id: String,
) -> Result<(), String> {
    tabs::close_tabbed_window(&app, &tabstate, &group_id)
}

/// Dashboard rows: one per open tabbed window.
/// JS: `invoke("list_tabbed_windows")`
#[tauri::command]
fn list_tabbed_windows(app: AppHandle) -> Vec<tabs::TabbedWindowInfo> {
    tabs::list_tabbed_windows(&app)
}

#[tauri::command]
fn suspend_account(
    app: AppHandle,
    store: State<'_, AppStore>,
    app_id: String,
    account_id: String,
) -> Result<(), String> {
    windows::suspend_account_window(&app, &store, &app_id, &account_id)
}

#[derive(Serialize)]
struct PlatformInfo {
    os: String,
    /// True only where we hook requests at the network layer (Windows).
    network_adblock: bool,
    filter_lists_loaded: bool,
    filter_lists_updated_at: Option<u64>,
}

#[tauri::command]
fn platform_info(adblock: State<'_, AdblockState>) -> Result<PlatformInfo, String> {
    let (filter_lists_loaded, filter_lists_updated_at) = adblock.meta_snapshot();
    Ok(PlatformInfo {
        os: std::env::consts::OS.to_string(),
        network_adblock: cfg!(windows),
        filter_lists_loaded,
        filter_lists_updated_at,
    })
}

// ---------------------------------------------------------------------------
// Launcher commands
// ---------------------------------------------------------------------------

/// Programs found by the last scan (the startup scan runs in the background).
#[tauri::command]
fn list_programs(state: State<'_, LauncherState>) -> Vec<NativeProgram> {
    state.list()
}

/// Full rescan now; blocks a worker thread, not the UI. Returns the count.
#[tauri::command]
fn rescan_programs(state: State<'_, LauncherState>) -> usize {
    state.rescan()
}

/// Launch a program by id. The lookup is server-side, so the frontend can
/// never ask the backend to run an arbitrary path.
#[tauri::command]
fn launch_program(id: String, state: State<'_, LauncherState>) -> Result<(), String> {
    state.launch(&id)
}

/// Hide the main window without quitting (Escape with an empty search box).
#[tauri::command]
fn hide_library(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        w.hide().map_err(|e| e.to_string())
    } else {
        Ok(())
    }
}

/// Show the main window in library (management) view. The frontend listens
/// for the `appmaka:show-library` event and switches views accordingly.
#[tauri::command]
fn show_library(app: AppHandle) -> Result<(), String> {
    show_library_view(&app)
}

/// Best-effort page title for the quick-add flow. The frontend falls back to
/// a prettified domain name when this errors. ASYNC ON PURPOSE: the HTTP
/// fetch can take up to 10s, and a sync command would block the WebView2 IPC
/// thread in the meantime (same reason `fetch_favicon` is async).
#[tauri::command]
async fn fetch_page_title(url: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || page_title::fetch_page_title(&url))
        .await
        .map_err(|e| format!("title fetch failed: {e}"))?
}

/// Fetch the app's site icon, cache it locally, and store the path on the
/// app record. Best-effort: resolves to `None` when no usable icon is found,
/// and the UI keeps its fallbacks. ASYNC ON PURPOSE: the HTTP fetch can
/// take seconds, and blocking the WebView2 IPC thread of a sync command
/// would freeze the invoking webview in the meantime.
#[tauri::command]
async fn fetch_favicon(
    app: AppHandle,
    store: State<'_, AppStore>,
    app_id: String,
) -> Result<Option<String>, String> {
    let url = store.get(&app_id).map(|a| a.url)?;
    let page_url: url::Url = url.parse().map_err(|_| "App URL is not valid.".to_string())?;
    let favicons_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("favicons");
    let path = tauri::async_runtime::spawn_blocking(move || {
        favicon::download_icon(&page_url, &favicons_dir)
    })
    .await
    .map_err(|e| format!("favicon fetch failed: {e}"))?;
    match path {
        Some(p) => {
            let s = p.to_string_lossy().into_owned();
            store.set_app_icon(&app_id, Some(s.clone()))?;
            Ok(Some(s))
        }
        None => Ok(None),
    }
}

/// Save a user-uploaded logo for an app. The bytes must sniff as a real
/// image (PNG/JPEG/GIF/WebP/ICO) and be at most 2 MiB. Stored under
/// `<app-data>/favicons/` as `custom-<app_id>-<unix_ts>.<ext>` and recorded
/// on the app via `set_app_icon`, so tiles pick it up immediately.
/// Sync is fine: the payload is small and the I/O is a single local write.
#[tauri::command]
fn set_app_icon_data(
    app: AppHandle,
    store: State<'_, AppStore>,
    app_id: String,
    data: Vec<u8>,
    ext: String,
) -> Result<String, String> {
    const MAX_LOGO: usize = 2 * 1024 * 1024;
    if data.len() > MAX_LOGO {
        return Err("That image is too large (2 MiB max).".to_string());
    }
    let sniffed = favicon::sniff_extension(&data).ok_or_else(|| {
        "That file is not a supported image (PNG, JPEG, GIF, WebP, ICO).".to_string()
    })?;
    // The claimed extension is only honored when it names a real image
    // type; otherwise the sniffed type wins, so a renamed executable can
    // never land with a misleading extension.
    let ext = match ext.trim().to_lowercase().as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "gif" => "gif",
        "webp" => "webp",
        "ico" => "ico",
        _ => sniffed,
    };
    let favicons_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("favicons");
    std::fs::create_dir_all(&favicons_dir)
        .map_err(|e| format!("could not create favicons dir: {e}"))?;
    // App ids are backend-generated ("app-<millis>-<n>"), but sanitize
    // anyway so the filename can never escape the favicons dir.
    let safe_id: String = app_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = favicons_dir.join(format!("custom-{safe_id}-{ts}.{ext}"));
    std::fs::write(&dest, &data).map_err(|e| format!("could not save logo: {e}"))?;
    let s = dest.to_string_lossy().into_owned();
    store.set_app_icon(&app_id, Some(s.clone()))?;
    // Keep the icon/thumbnail cache bounded (v0.7.0).
    favicon::enforce_cache_cap(&favicons_dir);
    Ok(s)
}

/// Remove a custom (or fetched) logo: tiles fall back to the live
/// /favicon.ico and then the letter tile.
#[tauri::command]
fn clear_app_icon(store: State<'_, AppStore>, app_id: String) -> Result<(), String> {
    store.set_app_icon(&app_id, None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Preview sign-in commands
// ---------------------------------------------------------------------------

/// Open a preview window for `url`: the real site in a throwaway session so
/// the user can sign in, then adopt it via `preview_add`.
/// ASYNC ON PURPOSE — same Windows deadlock as `open_account`: window
/// creation must not run on the WebView2 IPC thread of a sync command.
#[tauri::command]
async fn preview_start(
    app: AppHandle,
    adblock: State<'_, AdblockState>,
    url: String,
) -> Result<preview::PreviewStart, String> {
    preview::start_preview(&app, &adblock, &url)
}

/// Abandon a preview: close its window and delete the throwaway session.
#[tauri::command]
fn preview_discard(app: AppHandle, preview_id: String) -> Result<(), String> {
    preview::discard_preview(&app, &preview_id)
}

/// Reload the preview's site window (the control strip's refresh button).
/// Tauri exposes `preview_id` as `previewId` to JS.
#[tauri::command]
fn preview_reload(app: AppHandle, preview_id: String) -> Result<(), String> {
    preview::reload_preview(&app, &preview_id)
}

/// Turn a preview into a real app: the signed-in throwaway session becomes
/// the new app's first account. The frontend sends `label` for the account
/// (empty = "Main"). Tauri exposes `preview_id` as `previewId` to JS.
/// When the URL is already in the library, no duplicate app is created —
/// the session becomes a NEW account on the existing app (user's label, or
/// "Account N"). The outcome carries `addedAccount` so the UI can open it.
/// ASYNC ON PURPOSE: adopting the session fetches the site's og:image for
/// the account thumbnail, and a sync command would block the WebView2 IPC
/// thread while the HTTP fetch runs (same reason `fetch_favicon` is async).
#[tauri::command]
async fn preview_add(
    app: AppHandle,
    preview_id: String,
    label: Option<String>,
) -> Result<preview::PreviewAddOutcome, String> {
    let app_c = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // State<'_, _> is not 'static, so re-resolve it inside the closure.
        let store = app_c.state::<AppStore>();
        preview::add_preview_as_app(&app_c, &store, &preview_id, label)
    })
    .await
    .map_err(|e| format!("preview add failed: {e}"))?
}

/// Show the main window in library (management) view and tell the frontend
/// to switch to it.
fn show_library_view(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        w.show().map_err(|e| e.to_string())?;
        w.set_focus().map_err(|e| e.to_string())?;
        app.emit("appmaka:show-library", ())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn get_launcher_settings(app: AppHandle) -> LauncherSettings {
    app.state::<Mutex<LauncherSettings>>()
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default()
}

/// Runtime snapshot of whether the saved summon hotkey is actually
/// registered with the OS. Lets the UI warn when startup registration
/// failed instead of showing the hotkey as if it were live.
#[tauri::command]
fn get_hotkey_status(state: State<'_, Mutex<HotkeyStatus>>) -> Result<HotkeyStatus, String> {
    state
        .lock()
        .map(|s| s.clone())
        .map_err(|e| format!("hotkey state poisoned: {e}"))
}

/// v0.8.1: syntax-check a hotkey string without registering it. Lets the
/// HotkeyCapture component reject a bad combination inline, right after the
/// user presses it, instead of waiting for save.
#[tauri::command]
fn validate_hotkey(hotkey: String) -> Result<(), String> {
    hotkeys::validate_hotkey_syntax(&hotkey)
}

#[tauri::command]
fn set_hotkey(app: AppHandle, hotkey: String) -> Result<(), String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    let result = launcher_settings::set_hotkey(&app, &mut settings, &hotkey);
    // Refresh the runtime status from ground truth so the settings banner
    // clears (or appears) correctly after every change attempt — including
    // the rollback path, where the old key should be live again.
    if let Some(hs) = app.try_state::<Mutex<HotkeyStatus>>() {
        if let Ok(mut s) = hs.lock() {
            let current = settings.hotkey.clone();
            let registered = app.global_shortcut().is_registered(current.as_str());
            s.hotkey = current;
            s.registered = registered;
            s.error = match &result {
                Ok(()) => None,
                // Rolled back fine and the saved key is live again.
                Err(_) if registered => None,
                Err(e) => Some(e.clone()),
            };
        }
    }
    result
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    launcher_settings::set_autostart(&app, &mut settings, enabled)
}

/// Change what AppMaka does with the previous session at startup (v0.9.5).
/// JS: `invoke("set_startup_mode", { mode })` — mode is
/// "restore" | "ask" | "fresh". Returns the updated settings.
#[tauri::command]
fn set_startup_mode(app: AppHandle, mode: String) -> Result<LauncherSettings, String> {
    let mode = match mode.trim().to_lowercase().as_str() {
        "restore" => launcher_settings::StartupMode::Restore,
        "ask" => launcher_settings::StartupMode::Ask,
        "fresh" => launcher_settings::StartupMode::Fresh,
        _ => return Err("Unknown startup option.".to_string()),
    };
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    settings.startup_mode = mode;
    launcher_settings::save(&app, &settings)?;
    Ok(settings.clone())
}

/// One-time-per-launch session-restore offer for "Ask me" mode.
/// Returns the offer once, then None for the rest of the process.
#[tauri::command]
fn get_pending_session_restore(app: AppHandle) -> Option<session::SessionRestoreOffer> {
    session::take_restore_offer(&app)
}

/// Reopen the saved session now: the launcher "Restore session" button
/// and the Ask-mode banner. Async on purpose — window creation never runs
/// on an IPC thread (wry#583), same rule as the open_account command.
/// Sentinel-guarded like auto-restore (v0.9.7): a crash here forces
/// ask-mode next launch instead of looping.
#[tauri::command]
async fn restore_session(app: AppHandle) -> Result<usize, String> {
    Ok(session::restore_session_manual(&app))
}

/// Dismiss the Ask-mode offer: forget the saved session.
#[tauri::command]
fn dismiss_session_restore(app: AppHandle) -> Result<(), String> {
    session::clear_session(&app);
    Ok(())
}

/// Change the launcher panel translucency. The backend clamps to
/// 0.3..=1.0; returns the updated settings so the UI paints immediately.
#[tauri::command]
fn set_panel_opacity(app: AppHandle, opacity: f32) -> Result<LauncherSettings, String> {
    let state = app.state::<Mutex<LauncherSettings>>();
    let mut settings = state
        .lock()
        .map_err(|e| format!("settings state poisoned: {e}"))?;
    launcher_settings::set_panel_opacity(&app, &mut settings, opacity)?;
    Ok(settings.clone())
}

/// Alt+Space (or the user's chosen key) toggles the window. Showing always
/// lands on the launcher (spotlight) view — the frontend resets via the
/// `appmaka:show-launcher` event. Hiding is just hiding.
///
/// Positioning honors the `monitor_mode` launcher setting: cursor mode
/// (default) centers the window on the monitor holding the mouse cursor;
/// primary mode uses the primary monitor. All math is in physical pixels
/// (cursor and monitor geometry are physical; monitor origins can be
/// negative on multi-monitor X11, which the formula handles). The position
/// is set BEFORE show() so there is no visible jump.
fn toggle_main_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let visible = w.is_visible().unwrap_or(false);
        if visible {
            let _ = w.hide();
        } else {
            position_on_target_monitor(app, &w);
            let _ = w.show();
            let _ = w.set_focus();
            let _ = app.emit("appmaka:show-launcher", ());
        }
    }
}

fn position_on_target_monitor(app: &AppHandle, w: &tauri::WebviewWindow) {
    use tauri::{PhysicalPosition, Position};
    let cursor_mode = app
        .try_state::<Mutex<LauncherSettings>>()
        .and_then(|s| {
            s.lock()
                .ok()
                .map(|s| s.monitor_mode == launcher_settings::MonitorMode::Cursor)
        })
        .unwrap_or(true);
    let monitor = if cursor_mode {
        app.cursor_position()
            .ok()
            .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
            .or_else(|| app.primary_monitor().ok().flatten())
    } else {
        app.primary_monitor().ok().flatten()
    };
    if let Some(m) = monitor {
        let mp = m.position();
        let ms = *m.size();
        // outer_size() is physical; fall back to the monitor size if unknown.
        let ws = w.outer_size().unwrap_or(ms);
        let x = mp.x + (ms.width as i32 - ws.width as i32) / 2;
        let y = mp.y + (ms.height as i32 - ws.height as i32) / 2;
        if w
            .set_position(Position::Physical(PhysicalPosition::new(x, y)))
            .is_ok()
        {
            return;
        }
    }
    let _ = w.center();
}

/// Build the tray icon: left-click toggles the launcher overlay, the menu
/// offers Show library / Rescan programs / Quit. Missing entirely on Linux
/// desktops without a tray (Wayland GNOME) — the app still works, just
/// without the icon.
fn build_tray(app: &mut tauri::App) -> Result<(), String> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let show = MenuItem::with_id(app.handle(), "tray-show", "Show library", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let rescan = MenuItem::with_id(
        app.handle(),
        "tray-rescan",
        "Rescan programs",
        true,
        None::<&str>,
    )
    .map_err(|e| e.to_string())?;
    let quit =
        MenuItem::with_id(app.handle(), "tray-quit", "Quit", true, None::<&str>)
            .map_err(|e| e.to_string())?;
    let menu = Menu::with_items(app.handle(), &[&show, &rescan, &quit])
        .map_err(|e| e.to_string())?;

    let mut builder = TrayIconBuilder::with_id("appmaka")
        .tooltip("AppMaka")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray-show" => {
                let _ = show_library_view(app);
            }
            "tray-rescan" => {
                let handle = app.clone();
                std::thread::Builder::new()
                    .name("appmaka-tray-rescan".to_string())
                    .spawn(move || {
                        if let Some(state) = handle.try_state::<LauncherState>() {
                            state.rescan();
                        }
                    })
                    .ok();
            }
            "tray-quit" => {
                // Pins never block Quit (v0.9.9): raise the flag before
                // anything else so no CloseRequested branch can divert.
                pin::set_shutting_down();
                // Persist the session explicitly at exit (v0.9.5). Every
                // open/close already writes it, so this is usually a no-op —
                // but Quit is the one path where nothing else runs after.
                session::write_session(app);
                // Clean shutdown: the restore sentinel must not survive a
                // deliberate quit, or the next launch would wrongly think
                // the previous run crashed mid-restore (v0.9.7).
                session::clear_restore_sentinel(app);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                toggle_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app).map_err(|e| e.to_string())?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state
                        == tauri_plugin_global_shortcut::ShortcutState::Pressed
                    {
                        // v0.9.11: fixed "add popup to applications"
                        // shortcut. Fires only when a popup window is
                        // focused; ignored silently anywhere else. The
                        // active flag guarantees this can never steal a
                        // user's own binding or the summon hotkey: those
                        // always win registration, leaving the flag false.
                        if crate::popup_add::add_shortcut_active()
                            && Some(shortcut.id())
                                == crate::popup_add::add_shortcut_id()
                        {
                            crate::popup_add::fire_from_hotkey(app);
                            return;
                        }
                        // Named hotkey bindings (routines / workspaces /
                        // per-command hotkeys) dispatch through the shared
                        // registry. Anything with no binding — the launcher
                        // summon hotkey — keeps the old behavior. The
                        // clipboard popup toggles its window directly in
                        // Rust so it works even when the main window's JS
                        // is busy.
                        if let Some((binding_id, kind)) =
                            crate::hotkeys::lookup_binding(app, shortcut.id())
                        {
                            match kind {
                                crate::hotkeys::HotkeyKind::Clipboard => {
                                    crate::clipboard::toggle_window(app);
                                }
                                _ => {
                                    crate::hotkeys::emit_hotkey_fired(
                                        app,
                                        &binding_id,
                                        &kind,
                                    );
                                }
                            }
                        } else {
                            toggle_main_window(app);
                        }
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Self-update (v0.6.0): verifies the free Tauri signature, not
        // Authenticode — works on unsigned builds. The feed 404s while the
        // repo is private; the frontend degrades gracefully.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // Clipboard history v1 (text only): OS read/write for the
        // background watcher and copy-back. All access is Rust-side;
        // the frontend never touches the plugin's JS API.
        .plugin(tauri_plugin_clipboard_manager::init())
        // File picker for the launcher's manual "add program".
        .plugin(tauri_plugin_dialog::init())
        // Deep links (v0.7.0): the link dispatcher can handle https URLs.
        .plugin(tauri_plugin_deep_link::init())
        // Open URLs/files in the OS default browser / file manager.
        .plugin(tauri_plugin_opener::init())
        // One instance only: a clicked link wakes the running app instead of
        // spawning a duplicate. The deep-link feature forwards the URL args
        // to the deep-link plugin before the (empty) callback runs.
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {}))
        .setup(|app| {
            let store =
                AppStore::load(app.handle()).map_err(std::io::Error::other)?;
            let adblock =
                AdblockState::new(app.handle()).map_err(std::io::Error::other)?;

            app.manage(store);
            app.manage(WindowState::default());
            app.manage(PreviewState::default());
            // Tabbed app windows (v0.10.0): live group bookkeeping.
            app.manage(tabs::TabState::default());
            // Copyable error dialogs (v0.11.0): recent-errors ring buffer.
            app.manage(Mutex::new(errors::ErrorLog::default()));
            // Session restore (v0.9.5): live search-window info for the
            // session file, plus the one-time "Ask me" offer flag.
            app.manage(session::SearchLiveState::default());
            app.manage(session::SessionAskConsumed::default());
            app.manage(session::SessionRestoreForced::default());
            // "Don't close this window" (v0.9.9): runtime pin map plus the
            // one-shot/dedupe sets for the confirm flow, seeded from the
            // saved session before any restore runs.
            app.manage(pin::PinState::default());
            app.manage(pin::ConfirmedCloses::default());
            app.manage(pin::PendingConfirms::default());
            pin::seed_from_session(app.handle());
            // Crash-loop sentinel (v0.9.7): a stale restore.inprogress
            // from a previous run forces ask-mode instead of
            // auto-restoring into the same crash; otherwise arm the
            // sentinel while auto-restore is on.
            session::check_startup_sentinel(app.handle());
            // In-app download manager (v0.7.0): account-window downloads are
            // intercepted natively so the webview session is preserved.
            app.manage(crate::downloads::DownloadState::load(app.handle()).map_err(std::io::Error::other)?);
            // Sweep preview temp dirs left behind by a crash or a window
            // closed by hand before "Add as app" / "Discard" ran.
            preview::cleanup_stale_previews(app.handle());

            // Filter lists download + engine compile happen on a background
            // thread so startup never waits on the network.
            let adblock_bg = adblock.clone();
            app.manage(adblock);
            std::thread::Builder::new()
                .name("appmaka-adblock".to_string())
                .spawn(move || adblock_bg.refresh_loop())
                .map_err(std::io::Error::other)?;

            // --- launcher ---
            let launcher_state =
                LauncherState::new(app.handle()).map_err(std::io::Error::other)?;
            app.manage(launcher_state);
            let settings = launcher_settings::load(app.handle());
            // Capture the registration outcome so the UI can warn when the
            // saved hotkey isn't actually live (e.g. another app already
            // owns Alt+Space) instead of showing it as if it worked.
            let hotkey_status =
                match launcher_settings::register_hotkey(app.handle(), &settings.hotkey) {
                    Ok(()) => HotkeyStatus {
                        hotkey: settings.hotkey.clone(),
                        registered: true,
                        error: None,
                    },
                    Err(e) => {
                        eprintln!("launcher hotkey: {e}");
                        // v0.11.0: recorded for Copy diagnostics. The
                        // settings banner shows it, so no dialog here.
                        crate::errors::record(
                            app.handle(),
                            "hotkey",
                            &format!(
                                "The launcher shortcut {} couldn't be registered.",
                                settings.hotkey
                            ),
                            &e,
                            false,
                        );
                        HotkeyStatus {
                            hotkey: settings.hotkey.clone(),
                            registered: false,
                            error: Some(e),
                        }
                    }
                };
            app.manage(Mutex::new(hotkey_status));
            if settings.autostart {
                if let Err(e) = app.handle().autolaunch().enable() {
                    eprintln!("launcher autostart: {e}");
                }
            }
            app.manage(Mutex::new(settings));
            // Named hotkey registry (v0.8.0): one registry for routine /
            // workspace / per-command hotkeys so the global-shortcut
            // handler can dispatch presses. register_all_saved reads
            // routines.json, workspaces.json and cmdhotkeys.json and
            // registers each saved hotkey best-effort; a failure is
            // recorded in the binding status (UI warns) and logged —
            // startup never crashes on a hotkey.
            crate::hotkeys::init_registry(app.handle());
            crate::hotkeys::register_all_saved(app.handle());
            // v0.9.11: fixed Ctrl+Shift+A for "add popup to applications".
            // Skipped silently when the keys are already claimed (a user's
            // own binding always wins).
            crate::popup_add::ensure_add_shortcut_registered(app.handle());
            // Debug-only E2E driver (Xvfb smoke): inert without the env var,
            // compiled out of release builds.
            #[cfg(debug_assertions)]
            crate::debug_e2e::maybe_run_popup_flow(app.handle());
            // v0.9.12 clipboard pinning E2E driver (Xvfb smoke).
            #[cfg(debug_assertions)]
            crate::debug_e2e::maybe_run_clipboard_flow(app.handle());
            // v0.10.0 tabbed-window E2E driver (Xvfb smoke).
            #[cfg(debug_assertions)]
            crate::debug_e2e::maybe_run_tabs_flow(app.handle());
            // v0.11.0 error-dialog + tint E2E drivers (Xvfb smoke).
            #[cfg(debug_assertions)]
            crate::debug_e2e::maybe_run_error_flow(app.handle());
            #[cfg(debug_assertions)]
            crate::debug_e2e::maybe_run_tint_flow(app.handle());
            // Clipboard history (v0.9.0): local text history, poll-based
            // watcher. One 600ms tick = one clipboard read + string
            // compare; ~nothing at idle.
            app.manage(crate::clipboard::load(app.handle()));
            crate::clipboard::ensure_summon_registered(app.handle());
            crate::clipboard::start_watcher(app.handle().clone());
            // First program scan runs in the background; results land in the
            // cache and are picked up by list_programs.
            {
                let handle = app.handle().clone();
                std::thread::Builder::new()
                    .name("appmaka-program-scan".to_string())
                    .spawn(move || {
                        if let Some(state) = handle.try_state::<LauncherState>() {
                            state.rescan();
                        }
                    })
                    .map_err(std::io::Error::other)?;
            }

            // Tray icon. Missing on tray-less Linux desktops; that is fine.
            if let Err(e) = build_tray(app) {
                eprintln!("tray: {e}");
            }

            windows::start_suspend_watcher(app.handle().clone());
            // v0.9.9 (Linux): one-shot reap of the launcher's spare ~55 MB
            // network process at startup. Fires after the grace period and
            // only when no account window (restored or otherwise) is open;
            // the process has zero webview-side consumers and WebKitGTK
            // respawns it on demand.
            #[cfg(target_os = "linux")]
            windows::schedule_network_process_reap(app.handle().clone());
            // Link dispatcher (v0.7.0): incoming https URLs go to the account
            // chosen by the user's domain rules, or a picker when no rule
            // matches. Best-effort registration so links reach the app even
            // without a perfect install; the user still picks their default
            // browser in OS settings.
            #[cfg(any(windows, target_os = "linux"))]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                if let Err(e) = app.deep_link().register_all() {
                    eprintln!("deep-link register_all: {e}");
                }
            }
            links::register_link_handler(app.handle());
            // Session restore (v0.9.5): reopen last run's windows when the
            // user chose "Restore last session". Delayed so startup finishes
            // first; window creation happens on this dedicated thread, never
            // the main or IPC threads (wry#583).
            {
                let handle = app.handle().clone();
                std::thread::Builder::new()
                    .name("appmaka-session-restore".to_string())
                    .spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        session::maybe_restore_on_launch(&handle);
                    })
                    .map_err(std::io::Error::other)?;
            }
            Ok(())
        })
        // Closing the main window hides it to the tray; Quit is via the
        // tray menu. The launcher is always one hotkey away.
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            } else if window.label() == crate::clipboard::WINDOW_LABEL {
                // The clipboard popup is reused across summons (rebuilds
                // cost a beat on the hotkey), so close requests just hide
                // it. Escape / focus-loss hide it from the frontend too.
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            } else if window.label().starts_with("preview-") {
                // A preview window closed by hand (X button on either the
                // site or the control window): close its sibling, drop the
                // session, delete the temp dir. preview_add/preview_discard
                // already removed their entries, so those paths no-op here.
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    preview::window_closed(window.app_handle(), window.label());
                }
            } else if !pin::is_shutting_down() {
                // Page windows (v0.9.9): Alt+F4 / the X button / a taskbar
                // close on a pinned window ("Don't close this window")
                // diverts to one confirm instead of closing. Unpinned
                // windows fall through untouched, so this branch is a
                // no-op for every window that was never pinned.
                //
                // LOGIC-ONLY on Linux: tao delivering CloseRequested for
                // Alt+F4 on a frameless Windows window was assumed from the
                // strip's contract, never click-tested with a handler
                // attached. Exact PC test steps are in the v0.9.9 report.
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let app = window.app_handle();
                    let label = window.label();
                    if pin::is_pinned(app, label) && !pin::take_confirmed(app, label) {
                        api.prevent_close();
                        pin::ask_then_close(app, label);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_apps,
            add_app,
            update_app,
            rename_app,
            remove_app,
            update_app_settings,
            add_account,
            rename_account,
            update_account,
            remove_account,
            open_account,
            suspend_account,
            platform_info,
            list_programs,
            rescan_programs,
            launch_program,
            hide_library,
            show_library,
            fetch_page_title,
            fetch_favicon,
            set_app_icon_data,
            clear_app_icon,
            preview_start,
            preview_discard,
            preview_add,
            preview_reload,
            get_launcher_settings,
            get_hotkey_status,
            set_hotkey,
            validate_hotkey,
            set_autostart,
            set_startup_mode,
            get_pending_session_restore,
            restore_session,
            dismiss_session_restore,
            set_panel_opacity,
            launcher_ext::toggle_pin,
            launcher_ext::set_program_hidden,
            launcher_ext::record_launch,
            launcher_ext::set_monitor_mode,
            launcher_ext::set_seen_intro,
            launcher_ext::set_auto_update_check,
            custom_programs::add_custom_program,
            custom_programs::remove_custom_program,
            custom_programs::pick_executable,
            // v0.7.0: RAM dashboard + forget-login
            windows::close_all_account_windows,
            windows::close_open_window,
            windows::list_open_account_windows,
            // v0.9.11: "Add to applications" for popup windows.
            popup_add::popup_add_to_applications,
            windows::memory_snapshot,
            windows::forget_login,
            // v0.9.9: "Don't close this window" pin commands
            pin::set_window_pinned,
            pin::window_pinned,
            // v0.10.0: tabbed app windows
            open_tabbed_window,
            open_app_in_tabbed_window,
            switch_tab,
            add_tab,
            close_tab,
            close_tabbed_window,
            list_tabbed_windows,
            // v0.11.0: copyable error dialogs
            errors::get_recent_errors,
            errors::copy_error_details,
            errors::copy_diagnostics,
            // v0.7.0: back/forward navigation command (the visible floating
            // toolbar was removed in v0.8.4; Alt+Left/Right drive history
            // in-page, and this command stays registered for compatibility)
            windows::account_nav,
            // v0.7.0: link dispatcher
            links::get_link_config,
            links::set_link_opt_in,
            links::add_link_rule,
            links::remove_link_rule,
            links::open_link_in_account,
            // v0.7.0: launcher system commands + open-in-browser
            syscmd::system_command,
            syscmd::open_url_in_browser,
            // v0.9.3: `?query` web search opens as an in-app web app window
            websearch::open_web_search,
            // v0.7.0: in-app download manager
            downloads::list_downloads,
            downloads::open_download,
            downloads::show_in_folder,
            downloads::remove_download,
            downloads::clear_finished,
            downloads::rename_download,
            downloads::retry_download,
            downloads::get_download_dir,
            downloads::set_download_dir,
            downloads::pick_download_dir,
            downloads::get_download_settings,
            downloads::set_ask_where_to_save,
            downloads::set_show_completion_notice,
            // v0.7.0: launcher search-engine setting
            launcher_settings::set_search_engine,
            launcher_settings::set_custom_cursor,
            launcher_settings::set_open_as_tabbed,
            // v0.8.0: hidden-programs collapse state (polish)
            launcher_settings::set_hidden_section_collapsed,
            // v0.8.0: centralized hotkey registry + per-command hotkeys
            hotkeys::list_cmdhotkeys,
            hotkeys::save_cmdhotkey,
            hotkeys::delete_cmdhotkey,
            hotkeys::get_binding_status,
            // v0.8.0: routines ("morning stack")
            routines::list_routines,
            routines::save_routine,
            routines::delete_routine,
            routines::run_routine,
            // v0.9.0: clipboard history (local; v0.9.1 added images)
            clipboard::list_clipboard,
            clipboard::copy_clipboard_entry,
            // v0.9.3: multi-select — several text entries, one payload
            clipboard::copy_clipboard_entries,
            clipboard::hide_clipboard_popup,
            clipboard::clear_clipboard,
            clipboard::get_clipboard_settings,
            clipboard::set_clipboard_cap,
            clipboard::set_clipboard_hotkey,
            clipboard::clipboard_hotkey_status,
            // v0.9.2: bare-Windows-key tap summon (opt-in, Windows only)
            clipboard::set_clipboard_win_tap,
            // v0.9.4: popup tab (All | Text | Images), persisted
            clipboard::set_clipboard_popup_tab,
            // v0.9.12: clipboard pinning + "Pinned only" filter, persisted
            clipboard::set_clipboard_pinned,
            clipboard::set_clipboard_pinned_only,
            // v0.8.2: MSI-aware self-update (install-type detection + MSI path)
            msi_update::get_install_type,
            msi_update::install_msi_update,
            // v0.8.0: workspaces
            workspaces::list_workspaces,
            workspaces::save_workspace,
            workspaces::delete_workspace,
            workspaces::set_active_workspace,
            workspaces::create_workspace_from_open,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
