//! Account windows: one isolated OS window per (app, account).
//!
//! Isolation mechanism: each account window is built with
//! `WebviewWindowBuilder::data_directory(account.session_dir)`. On Windows
//! that becomes the WebView2 user-data folder; on Linux the WebKitGTK data
//! dir. Never rely on the default data directory — it is shared and, on
//! Windows, lives next to the binary where it may not be writable.
//!
//! This module also owns popup policy (deny-by-default + contained OAuth
//! modals sharing the account's session), per-window activity tracking, and
//! the auto-suspend watcher (TrySuspend on Windows, no-op on Linux).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::adblock::AdblockState;
use crate::store::AppStore;

/// v0.15.0: crate-visible so the sleep module can stamp entries.
pub(crate) fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Millisecond clock for `opened_at`: session ordering needs finer grain
/// than seconds, since a restore can open several windows in one second.
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Window label for an account's main window. The suspend watcher and the
/// remove commands find windows by this prefix.
pub fn account_window_label(app_id: &str, account_id: &str) -> String {
    format!("acct-{app_id}-{account_id}")
}

/// The monitor holding the mouse cursor, falling back to the primary.
/// Same cursor-monitor logic as the launcher's positioning in main.rs;
/// shared by routine window tiling and the clipboard popup.
pub fn cursor_monitor(app: &AppHandle) -> Option<tauri::Monitor> {
    app.cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())
}

/// Logical top-left that centers a `w`x`h` window on the monitor under
/// the cursor (v0.13.18). Fresh windows used to always `.center()` —
/// the primary monitor — so opening an app while working on another
/// screen made the window "switch monitors". The user's ask: a window
/// stays where they are. None when no monitor can be determined;
/// callers fall back to `.center()`.
pub(crate) fn cursor_centered_position(app: &AppHandle, w: f64, h: f64) -> Option<(f64, f64)> {
    let m = cursor_monitor(app)?;
    let scale = m.scale_factor();
    let (mx, my) = (
        m.position().x as f64 / scale,
        m.position().y as f64 / scale,
    );
    let (mw, mh) = (
        m.size().width as f64 / scale,
        m.size().height as f64 / scale,
    );
    Some((mx + (mw - w) / 2.0, my + (mh - h) / 2.0))
}

/// Placement for a tiled routine window (v0.9.0): logical-pixel top-left
/// and logical-pixel size. The caller divides the physical monitor
/// geometry by the monitor's scale factor, so HiDPI comes out right.
#[derive(Debug, Clone, Copy)]
pub struct WindowPlacement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Navigate an open account window back/forward in its history.
///
/// Kept as a public command for compatibility (the visible floating toolbar
/// was removed in v0.8.4; Alt+Left / Alt+Right drive history directly in the
/// page). Async on purpose: never block an IPC thread on webview work
/// (same Windows deadlock rule as window creation).
/// JS: `invoke("account_nav", { appId, accountId, direction })`
/// where direction is `"back"` or `"forward"`.
#[tauri::command]
pub async fn account_nav(
    app: AppHandle,
    app_id: String,
    account_id: String,
    direction: String,
) -> Result<(), String> {
    let js = match direction.as_str() {
        "back" => "history.back();",
        "forward" => "history.forward();",
        _ => return Err("Unknown direction.".to_string()),
    };
    let label = account_window_label(&app_id, &account_id);
    let window = app
        .get_webview_window(&label)
        .ok_or_else(|| "That account window is not open.".to_string())?;
    window
        .eval(js)
        .map_err(|e| format!("Could not navigate: {e}"))?;
    Ok(())
}

struct TrackedWindow {
    app_id: String,
    // Read by list_open_account_windows; the allow goes away once the
    // coordinator registers that command in main.rs.
    #[allow(dead_code)]
    account_id: String,
    last_active: u64,
    /// Unix seconds when the window was opened. Session restore (v0.9.5)
    /// reopens windows in this order; unlike last_active it never moves.
    opened_at: u64,
    /// Flipped live by update_app_settings so open windows follow the toggle
    /// without a rebuild.
    adblock_enabled: Arc<AtomicBool>,
    /// Title without the suspend cue, restored on focus/resume (Windows).
    #[cfg(windows)]
    base_title: String,
    /// Latest live document.title (v0.15.0: feeds unread badges).
    page_title: String,
    /// Set when the user focuses the window: the badge clears until the
    /// next title change, which re-arms it.
    page_title_seen: bool,
    suspended: bool,
    /// Session-restore lazy load: the real URL, not yet navigated to. The
    /// window is built on about:blank and navigates here on first focus,
    /// so a 10-window restore doesn't spawn 10 renderers at once.
    pending_url: Option<String>,
    /// Last URL the webview was on (v0.16.4), captured at focus loss,
    /// before suspend, and before close — the moments it is both fresh
    /// and about to become unreadable. Persisted through session.json;
    /// a reopen prefers it over the app's home URL.
    last_url: Option<String>,
    /// Private/incognito window: uses an ephemeral session dir that is
    /// deleted on close; never written to session.json, never restored.
    is_private: bool,
    /// The ephemeral session dir for private windows (None for normal).
    /// Deleted when the window closes; orphans cleaned at startup.
    private_dir: Option<PathBuf>,
}

/// Live per-window bookkeeping, managed as Tauri state.
#[derive(Default)]
pub struct WindowState {
    inner: Mutex<HashMap<String, TrackedWindow>>,
    /// Last-visited URLs seeded by session restore (v0.16.4), keyed by
    /// window label. open_account_placed takes an entry one-shot as its
    /// window opens; leftovers (a restore entry whose window never
    /// opened) are inert — labels are per-account and the next restore
    /// re-seeds fresh values.
    restore_urls: Mutex<HashMap<String, String>>,
}

impl WindowState {
    /// v0.16.4: seed the URLs a session restore saved, for
    /// open_account_placed to consume as the windows open.
    pub(crate) fn seed_restore_urls(&self, urls: HashMap<String, String>) {
        if let Ok(mut guard) = self.restore_urls.lock() {
            guard.extend(urls);
        }
    }

    /// Take (one-shot) the restore-seeded URL for a window label.
    pub(crate) fn take_restore_url(&self, label: &str) -> Option<String> {
        self.restore_urls
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(label))
    }

    /// v0.15.0: does any tracked window belong to this account? Used by
    /// process attribution (procattr) so it never attributes browser
    /// processes to an account with nothing open.
    pub(crate) fn has_account_window(&self, app_id: &str, account_id: &str) -> bool {
        self.inner
            .lock()
            .map(|tracked| {
                tracked
                    .values()
                    .any(|t| t.app_id == app_id && t.account_id == account_id)
            })
            .unwrap_or(false)
    }
}

/// Maximum simultaneously-open account windows. Opening one more closes the
/// least-recently-used idle window first (never the focused one), bounding
/// worst-case webview RAM without any timers or configuration.
const MAX_OPEN_ACCOUNT_WINDOWS: usize = 6;

/// Enforce the LRU cap before opening a new account window.
fn enforce_account_window_cap(app: &AppHandle, winstate: &WindowState) {
    let lru: Option<String> = {
        let Ok(tracked) = winstate.inner.lock() else {
            return;
        };
        let mut labels: Vec<(&String, &TrackedWindow)> = tracked
            .iter()
            .filter(|(label, _)| label.starts_with("acct-"))
            .collect();
        if labels.len() < MAX_OPEN_ACCOUNT_WINDOWS {
            return;
        }
        // Pinned windows ("Don't close this window", v0.9.9) are never
        // evicted to make room; the next-oldest unpinned window goes. If
        // every window is pinned, the cap yields and the open proceeds.
        labels.retain(|(label, _)| !crate::pin::is_pinned(app, label));
        labels.sort_by_key(|(_, t)| t.last_active);
        labels.into_iter().map(|(l, _)| l.clone()).next()
    };
    let Some(label) = lru else {
        return;
    };
    // Fail closed: never evict the focused window to make room.
    if let Some(window) = app.get_webview_window(&label) {
        if window.is_focused().unwrap_or(true) {
            return;
        }
    }
    close_tracked_window(app, &label, CloseIntent::Background);
}

/// Move/resize an already-open window to a tiled placement (v0.9.0).
/// Best-effort: if the window manager refuses, the window simply stays
/// where it was. Also used by session restore (v0.9.5) to re-place a
/// reused window at its saved geometry.
pub(crate) fn apply_placement(window: &WebviewWindow, p: WindowPlacement) {
    use tauri::{LogicalPosition, LogicalSize, Position, Size};
    let _ = window.set_position(Position::Logical(LogicalPosition::new(p.x, p.y)));
    let _ = window.set_size(Size::Logical(LogicalSize::new(p.width, p.height)));
}

/// Pre-paint background for account windows: the app's dark-theme page
/// canvas (#16171b — styles.css, `:root[data-theme="dark"]`). The webview
/// paints this until the page's own first frame lands; without it,
/// WebView2 and WebKitGTK both default to white, so every window open —
/// and every wake from suspend — flashes white before content appears.
const ACCOUNT_WINDOW_BACKGROUND: tauri::utils::config::Color =
    tauri::utils::config::Color(0x16, 0x17, 0x1b, 0xff);

/// The one set of extra Chromium arguments every WebView2 window in
/// this app is created with (Windows only). WebView2 keys a browser
/// process by user data folder and refuses to create a webview whose
/// environment options differ from the ones already running for that
/// folder (ERROR_INVALID_STATE), so every builder that opens a window
/// on a session folder MUST pass the identical string — and a builder
/// that omits the call does not get "no arguments": wry substitutes
/// its own different default. v0.16.5 set these only on the account
/// and tabbed builders, which broke every contained popup and OAuth
/// window for an open account. The string lives in exactly one place
/// now so the sets cannot drift apart again. It also carries wry's
/// default feature disables explicitly (SmartScreen, the mini menu,
/// the PDF UI): custom args REPLACE wry's defaults rather than adding
/// to them, and v0.16.5 silently lost them on account windows. The
/// 256 MB per-page heap cap from that draft is gone — with the
/// low-memory target also pushing trims, heavy pages were being
/// squeezed from two directions at once.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const WEBVIEW2_BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,Translate,OptimizationHints,MediaRouter --enable-features=TrimOnMemoryPressure";

/// The stored last-visited URL a fresh open of this account should land
/// on, if one exists (v0.16.4). An in-progress session restore seeds
/// its saved URLs in memory (taken one-shot — a later manual reopen
/// must not inherit them); outside a restore, the session file is the
/// source (it still holds the entry after a crash, before any restore
/// ran). None means "nothing stored — use the app's home URL".
fn stored_last_url(
    app: &AppHandle,
    winstate: &WindowState,
    app_id: &str,
    account_id: &str,
    label: &str,
) -> Option<String> {
    winstate
        .take_restore_url(label)
        .or_else(|| crate::session::stored_account_last_url(app, app_id, account_id))
}

/// Open an account's window, or focus it if it is already open. The window is
/// lazily created here — nothing exists until the user opens the account.
pub fn open_account(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    winstate: &WindowState,
    app_id: &str,
    account_id: &str,
    private: bool,
) -> Result<(), String> {
    open_account_placed(app, store, adblock, winstate, app_id, account_id, None, false, private)
}

/// `open_account` with an optional tiled placement (v0.9.0): routines using
/// the "side by side" layout position each window as one column. Placement
/// applies both to freshly built windows (via the builder, so there is no
/// visible jump) and to already-open windows (repositioned on focus).
#[allow(clippy::too_many_arguments)]
pub fn open_account_placed(
    app: &AppHandle,
    store: &AppStore,
    adblock: &AdblockState,
    winstate: &WindowState,
    app_id: &str,
    account_id: &str,
    placement: Option<WindowPlacement>,
    defer_navigation: bool,
    private: bool,
) -> Result<(), String> {
    let web_app = store.get(app_id)?;
    let account = web_app
        .accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| "Account not found.".to_string())?;

    let label = account_window_label(app_id, account_id);
    if let Some(window) = app.get_webview_window(&label) {
        // A bare set_focus() is not enough on Windows: it cannot restore a
        // minimized window, so the user would see "nothing opens". Unminimize
        // and show first, then focus.
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        // An open that returns Ok while nothing appears is worse than a
        // loud failure: the user clicks again and again with no signal.
        // A window that stays invisible after show/focus is one such
        // case, so record it where the settings error list can show it.
        // Err (visibility unknown) records nothing — only a definite
        // "not visible" earns an entry.
        if window.is_visible().map(|v| !v).unwrap_or(false) {
            crate::errors::record(
                app,
                "window-open",
                &format!("\"{}\" did not appear after focusing its window.", account.label),
                &format!("label {label}: show/focus returned but is_visible is false"),
                true,
            );
        }
        if let Some(p) = placement {
            apply_placement(&window, p);
        }
        // A restore may have seeded a last-visited URL for this window,
        // but it is already open — drop the seed so a later close and
        // reopen doesn't inherit a URL from before this session.
        let _ = winstate.take_restore_url(&label);
        // Already open: nothing changed, but keep the session file fresh.
        crate::session::write_session(app);
        return Ok(());
    }

    // Bound worst-case RAM: evict the least-recently-used idle account
    // window before creating a new one.
    enforce_account_window_cap(app, winstate);

    // The store validates URLs on write, so this only fails on hand-edited
    // apps.json — still no unwrap.
    let page_url: url::Url = web_app
        .url
        .parse()
        .map_err(|_| format!("App URL is not valid: {}", web_app.url))?;
    let title = format!("{} — {}", web_app.name, account.label);
    // Private windows get an ephemeral session dir (deleted on close) and
    // an unmistakable title suffix; normal windows use the stored path.
    let (session_dir, private_dir, title) = if private {
        let dir = new_private_session_dir(app)?;
        (dir.clone(), Some(dir), format!("{title} · Private"))
    } else {
        // The stored absolute path is the source of truth for the data directory.
        (store.session_dir_for(app_id, account_id)?, None, title)
    };

    // Per-account override wins; None means "inherit the app setting".
    let popup_policy = account
        .popup_policy
        .clone()
        .unwrap_or_else(|| web_app.settings.popup_policy.clone());
    // v0.16.4: reopen where the user left off. A stored last-visited
    // URL (see stored_last_url) beats the app's home URL as the
    // navigation target; a hand-edited session value that doesn't parse
    // falls back to home rather than failing the open.
    let target_url: url::Url = stored_last_url(app, winstate, app_id, account_id, &label)
        .as_deref()
        .and_then(|u| u.parse().ok())
        .unwrap_or_else(|| page_url.clone());
    // Lazy restore (v0.13.2): build on about:blank and navigate on first
    // focus, so a 10-window session restore doesn't spawn 10 renderers at
    // once. The title still shows the app name; the page loads when the
    // user first looks at it. The deferred target is the stored last
    // URL when there is one — restored windows land on their last page
    // too, not just fresh opens.
    let (initial_url, pending_url) = if defer_navigation {
        (
            url::Url::parse("about:blank").expect("about:blank parses"),
            Some(target_url.to_string()),
        )
    } else {
        (target_url.clone(), None)
    };
    let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(initial_url))
        .data_directory(session_dir.clone())
        .title(&title)
        // No white flash before first paint (see the const).
        .background_color(ACCOUNT_WINDOW_BACKGROUND);
    // v0.9.6 (Windows): frameless — the native title bar and its X go away,
    // replaced by our own slim caption strip (caption.rs). tao keeps border
    // resizing working on frameless windows. Linux keeps native decorations.
    #[cfg(windows)]
    {
        builder = builder.decorations(false);
        builder = builder.additional_browser_args(WEBVIEW2_BROWSER_ARGS);
    }
    // v0.9.0: tiled routines place the window at build time (no visible
    // jump); untiled opens keep the classic centered 1200x800.
    // WebviewWindowBuilder::position takes logical pixels directly.
    if let Some(p) = placement {
        builder = builder.position(p.x, p.y).inner_size(p.width, p.height);
    } else if let Some((x, y)) = cursor_centered_position(app, 1200.0, 800.0) {
        // v0.13.18: open on the monitor the user is actually on.
        builder = builder.position(x, y).inner_size(1200.0, 800.0);
    } else {
        builder = builder.inner_size(1200.0, 800.0).center();
    }
    let mut builder = builder
        .on_new_window(make_popup_handler(PopupContext {
            app: app.clone(),
            app_id: app_id.to_string(),
            account_id: account_id.to_string(),
            app_name: web_app.name.clone(),
            app_url: web_app.url.clone(),
            session_dir: session_dir.clone(),
            popup_policy,
            popup_allowlist: web_app.settings.popup_allowlist.clone(),
            parent_label: label.clone(),
            chain_allowed: false,
        }))
        // In-app download manager (v0.7.0): downloads stay in the account
        // window's own session instead of kicking out to the system browser.
        .on_download(crate::downloads::make_download_handler(app.clone()));
    // v0.11.0: universal title-bar blending — re-tint the caption strip
    // when the page navigates (theme-color can change per page). The hook
    // must never block the navigation decision: it only hands the URL to
    // a detached thread, and tint::request_retint dedupes + guards races.
    #[cfg(windows)]
    {
        let tint_app = app.clone();
        let tint_label = label.clone();
        builder = builder.on_navigation(move |nav_url: &url::Url| {
            let app = tint_app.clone();
            let lbl = tint_label.clone();
            let u = nav_url.clone();
            std::thread::Builder::new()
                .name(format!("appmaka-page-retint-{lbl}"))
                .spawn(move || {
                    crate::tint::request_retint(&app, &lbl, &u, crate::tint::TintTarget::PageStrip);
                    // v0.13.8: a window built before a cursor settings
                    // change re-runs its old init script on navigation;
                    // converge it to the saved style.
                    schedule_cursor_reapply(app.clone(), lbl.clone());
                })
                .ok();
            true
        });
    }
    // Non-Windows has no caption tint to re-run, but the cursor
    // convergence in that hook matters on every platform: a window
    // built before a cursor settings change must not keep its old
    // build-time cursor after the next navigation.
    #[cfg(not(windows))]
    {
        let nav_app = app.clone();
        let nav_label = label.clone();
        builder = builder.on_navigation(move |_nav_url: &url::Url| {
            schedule_cursor_reapply(nav_app.clone(), nav_label.clone());
            true
        });
    }
    // Cosmetic filtering: engine-generated hide selectors injected before
    // first paint. Skipped entirely when no engine is loaded (fail open).
    // The target=_blank shim is always injected: without it WebKitGTK drops
    // target=_blank link clicks before they ever reach on_new_window.
    builder = builder.initialization_script(TARGET_BLANK_SHIM_JS);
    // Back/forward keyboard nav (v0.7.0): bare webviews have no chrome, so
    // Alt+Left / Alt+Right get them here.
    builder = builder.initialization_script(NAV_KEYS_JS);
    // v0.13.5: F1 shortcut cheatsheet overlay.
    builder = builder.initialization_script(shortcuts_overlay_js(app));
    // v0.12.0: custom page cursor. Empty string when "off" — skipped.
    let cursor_js = saved_cursor_js(app);
    if !cursor_js.is_empty() {
        builder = builder.initialization_script(cursor_js);
    }
    // v0.18.0: Tampermonkey-Lite user scripts matched against the
    // URL this window opens on (restored last URL, else the app's
    // start URL).
    for js in crate::userscripts::user_scripts_init_scripts(app, target_url.as_str()) {
        builder = builder.initialization_script(js);
    }
    let css = adblock.cosmetic_css_for(&web_app.url);
    if !css.is_empty() {
        builder = builder.initialization_script(cosmetic_init_script(&css));
    }
    // v0.15.0: unread badges — track the live document.title per window.
    // The callback only touches the state mutex (no Win32 calls), so it
    // is safe on whatever thread Tauri invokes it from.
    {
        let title_app = app.clone();
        let title_label = label.clone();
        builder = builder.on_document_title_changed(move |_window, title: String| {
            if let Some(winstate) = title_app.try_state::<WindowState>() {
                if let Ok(mut tracked) = winstate.inner.lock() {
                    if let Some(t) = tracked.get_mut(&title_label) {
                        t.page_title = title;
                        t.page_title_seen = false;
                    }
                }
            }
        });
    }
    // v0.15.0: per-account RAM attribution — snapshot descendants before
    // the build so the browser process spawned for this data directory
    // can be claimed for the account.
    let pids_before = crate::procattr::descendant_pids();
    let window = builder.build().map_err(|e| {
        let msg = format!("could not open account window: {e}");
        // v0.11.0: recorded for Copy diagnostics. The launcher shows the
        // inline error itself, so no dialog here.
        crate::errors::record(
            app,
            "window-open",
            "Could not open the app window.",
            &msg,
            false,
        );
        msg
    })?;
    crate::procattr::capture_new_processes(
        app.clone(),
        app_id.to_string(),
        account_id.to_string(),
        pids_before,
    );
    // v0.9.6: on Windows keep the DWM drop shadow on the frameless window
    // and attach our caption strip. On other platforms the caption calls
    // are no-ops.
    #[cfg(windows)]
    let _ = window.set_shadow(true);
    crate::caption::page_window_opened(app, &label, &window);
    // v0.11.0: universal title-bar blending — tint the strip with the
    // site's color (theme-color meta → page background → default).
    #[cfg(windows)]
    crate::tint::request_retint(app, &label, &page_url, crate::tint::TintTarget::PageStrip);

    // Per-account adblock override wins; None means "inherit the app setting"
    // (v0.8.1). This seeds the flag the Windows network blocker reads; the
    // app-level Settings push skips overridden accounts, and per-account
    // edits push via set_account_adblock_enabled.
    let adblock_flag = Arc::new(AtomicBool::new(
        web_app.effective_adblock_enabled(account_id),
    ));
    {
        let mut tracked = winstate
            .inner
            .lock()
            .map_err(|e| format!("window state lock poisoned: {e}"))?;
        tracked.insert(
            label.clone(),
            TrackedWindow {
                app_id: app_id.to_string(),
                account_id: account_id.to_string(),
                last_active: unix_secs(),
                opened_at: unix_millis(),
                adblock_enabled: adblock_flag.clone(),
                #[cfg(windows)]
                base_title: title,
                page_title: String::new(),
                page_title_seen: true,
                suspended: false,
                pending_url,
                // The window opens (or will navigate, when deferred) to
                // the target, so that is its last-known URL until a
                // capture point refreshes it.
                last_url: Some(target_url.to_string()),
                is_private: private,
                private_dir,
            },
        );
    }

    // Focus in/out feeds the suspend watcher; focus also resumes a suspended
    // webview on Windows.
    let track_app = app.clone();
    let track_label = label.clone();
    window.on_window_event(move |event| {
        match event {
            WindowEvent::Focused(focused) => {
                if *focused {
                    // Read the suspend flag BEFORE touch_window runs:
                    // touch clears it on focus, and the badge decision
                    // below needs the state the window was in when the
                    // user arrived.
                    let was_suspended = track_app
                        .try_state::<WindowState>()
                        .and_then(|s| {
                            s.inner
                                .lock()
                                .ok()
                                .and_then(|m| m.get(&track_label).map(|t| t.suspended))
                        })
                        .unwrap_or(false);
                    touch_window(&track_app, &track_label, true);
                    // Lazy restore: first focus navigates the deferred URL.
                    navigate_pending_url(&track_app, &track_label);
                    // v0.15.0: visiting the window clears its unread badge
                    // until the next title change — unless the window was
                    // suspended (v0.16.4): the title and its count are
                    // exactly what was there when it slept, so the user
                    // hasn't seen any of it yet, and a frozen renderer
                    // may never fire a fresh title event after wake to
                    // correct a premature clear. The badge clears on
                    // the next real title change, or on a later focus
                    // while the window is awake.
                    if focus_clears_badge(was_suspended) {
                        if let Some(winstate) = track_app.try_state::<WindowState>() {
                            if let Ok(mut tracked) = winstate.inner.lock() {
                                if let Some(t) = tracked.get_mut(&track_label) {
                                    t.page_title_seen = true;
                                }
                            }
                        }
                    }
                } else {
                    touch_window(&track_app, &track_label, false);
                }
            }
            WindowEvent::Destroyed => {
                // Closed via the X button (not through close_tracked_window):
                // drop the tracked entry so it can't go stale — the watchers
                // tolerate staleness, but the session must not resurrect a
                // window the user closed — and persist the session. Private
                // windows get their ephemeral dir deleted here too.
                //
                // v0.18.3: take the entry under the lock, then DROP the
                // guard before calling prune. prune() queries
                // has_account_window(), which locks this same
                // WindowState mutex — calling it under the guard
                // self-deadlocks the thread (std Mutex is not reentrant)
                // and froze the whole app on every account-window close.
                let removed =
                    if let Some(winstate) = track_app.try_state::<WindowState>() {
                        if let Ok(mut tracked) = winstate.inner.lock() {
                            tracked.remove(&track_label)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                if let Some(t) = removed {
                    if let Some(dir) = t.private_dir {
                        delete_private_session_dir(&dir);
                    }
                    // v0.15.0: X-button close — same attribution
                    // cleanup as the command path (idempotent).
                    crate::procattr::prune(&track_app, &t.app_id, &t.account_id);
                }
                crate::session::write_session(&track_app);
                // v0.9.6: the caption strip dies with its window (no-op
                // off Windows).
                crate::caption::page_window_closed(&track_label);
            }
            // Geometry changes feed the session (v0.9.5), debounced so a
            // drag doesn't hammer the disk.
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                crate::session::schedule_session_write(&track_app);
                // v0.9.6: keep the caption strip seated above the window.
                crate::caption::page_window_moved(&track_label);
            }
            _ => {}
        }
    });

    #[cfg(windows)]
    crate::adblock::attach_network_blocking(&window, adblock, adblock_flag);
    #[cfg(windows)]
    attach_fullscreen_sync(&window);

    store.touch_account(app_id, account_id);
    // A window opened (or re-focused above): persist the session.
    crate::session::write_session(app);
    // v0.15.0: the account is awake again — drop its saved figure.
    crate::sleep::clear_sleep(app, app_id, account_id);
    Ok(())
}

/// Enter fullscreen on the monitor the CURSOR is on (v0.13.13, the
/// user's ask — plain set_fullscreen lands on the window's monitor,
/// which reads as "it jumped to my other screen" on multi-monitor
/// setups). The cursor is what the user is pointing at; when it can't
/// be read, fall back to the window-monitor behavior.
#[cfg(windows)]
pub(crate) fn enter_fullscreen_on_cursor_monitor(window: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = POINT::default();
    let pos = unsafe { GetCursorPos(&mut pt) }
        .is_ok()
        .then(|| tauri::PhysicalPosition::new(pt.x as f64, pt.y as f64));
    match pos {
        Some(p) => {
            let _ = window.set_fullscreen_on_monitor(p);
        }
        None => {
            let _ = window.set_fullscreen(true);
        }
    }
}

/// Page-requested fullscreen (v0.13.12, Windows): when a page calls
/// `requestFullscreen()` — a video player's fullscreen button — WebView2
/// flips its ContainsFullScreenElement state, but nothing resizes the
/// host window unless the host listens, and wry 0.57 never registers
/// for it, so the button silently did nothing. Mirror the controller's
/// state onto the window here. The caption strip hides itself while
/// fullscreen (caption.rs reposition plan) and returns on exit, exactly
/// like any other fullscreen transition. Fail-open: any COM error just
/// leaves the window without page-fullscreen sync.
#[cfg(windows)]
pub(crate) fn attach_fullscreen_sync(window: &tauri::WebviewWindow) {
    let app = window.app_handle().clone();
    let label = window.label().to_string();
    let _ = window.with_webview(move |platform| {
        use webview2_com::ContainsFullScreenElementChangedEventHandler;
        use webview2_com::Microsoft::Web::WebView2::Win32::*;
        let core = match unsafe { platform.controller().CoreWebView2() } {
            Ok(c) => c,
            Err(_) => return,
        };
        let handler = ContainsFullScreenElementChangedEventHandler::create(Box::new(
            move |sender: Option<ICoreWebView2>, _args: Option<windows_core::IUnknown>| {
                if let Some(core) = sender.as_ref() {
                    let mut fs = windows_core::BOOL::default();
                    if unsafe { core.ContainsFullScreenElement(&mut fs) }.is_ok() {
                        if let Some(w) = app.get_webview_window(&label) {
                            // v0.13.13: entering goes to the cursor's
                            // monitor (this handler registers after the
                            // runtime's own, so it has the final say on
                            // placement); exiting just restores.
                            if fs.as_bool() {
                                enter_fullscreen_on_cursor_monitor(&w);
                            } else {
                                let _ = w.set_fullscreen(false);
                            }
                        }
                    }
                }
                Ok(())
            },
        ));
        let mut token: i64 = 0;
        let _ = unsafe {
            core.add_ContainsFullScreenElementChanged(&handler, &mut token)
        };
    });
}

/// Wrap engine-generated hide CSS in a JSON-escaped <style> injection that
/// runs before the page's own scripts (initialization script timing).
/// Shared with the preview flow.
pub(crate) fn cosmetic_init_script(css: &str) -> String {
    // serde_json escaping keeps arbitrary selector text (quotes, backslashes)
    // from breaking out of the JS string literal.
    let json_css = serde_json::to_string(css).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(function(){{try{{var css={json_css};\
        var s=document.createElement('style');\
        s.setAttribute('data-appmaka','cosmetic');s.textContent=css;\
        var root=document.head||document.documentElement;\
        if(root){{root.appendChild(s);}}}}catch(e){{}}}})();"
    )
}

/// Custom page cursor (v0.12.0): a page-drawn pointer for app windows.
/// Returns an empty string when the style is "off" or unrecognized, so
/// callers can skip the injection entirely.
///
/// Why page-drawn: it stays visible even when a site hides the native
/// cursor via CSS or the OS cursor theme fails to render inside the
/// webview. Minimal RAM by construction — one rAF loop, transform-only
/// movement (no layout), a handful of divs for the trail.
///
/// Live-apply (v0.13.8): the script installs a controller on
/// `window.__appmakaCursor` with a `destroy()` that removes the style,
/// the dots, the listeners, and the rAF loop. Re-running the script
/// with the same style is a no-op; running it with a different style
/// destroys the old cursor first. That is what lets a settings change
/// (or a navigation in a window built before the change) swap cursors
/// in place instead of waiting for a window reopen.
/// The cursor script for the user's saved style ("" when off). Reads
/// the launcher-settings state every other builder already consults.
pub(crate) fn saved_cursor_js(app: &AppHandle) -> String {
    let style = app
        .try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.custom_cursor.clone()))
        .unwrap_or_default();
    let (hex, rgb) = resolve_cursor_colors(app);
    cursor_chrome_js(&style, &hex, &rgb)
}

/// Resolve the saved cursor color to (hex, "r,g,b") for the script
/// generator (v0.16.0). "accent" follows the launcher accent's bright
/// tone; anything unknown falls back to the brand violet.
pub(crate) fn resolve_cursor_colors(app: &AppHandle) -> (String, String) {
    let (color, accent) = app
        .try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()
        .and_then(|s| {
            s.lock()
                .ok()
                .map(|s| (s.custom_cursor_color.clone(), s.accent.clone()))
        })
        .unwrap_or_default();
    cursor_palette(&color, &accent)
}

fn cursor_palette(color: &str, accent: &str) -> (String, String) {
    let (hex, rgb): (&str, &str) = match color {
        "white" => ("#f8fafc", "248,250,252"),
        "sky" => ("#38bdf8", "56,189,248"),
        "lime" => ("#a3e635", "163,230,53"),
        "amber" => ("#fbbf24", "251,191,36"),
        "rose" => ("#fb7185", "251,113,133"),
        "accent" => {
            let hex = crate::launcher_settings::accent_text_hex(accent);
            return (hex.to_string(), hex_rgb(hex));
        }
        _ => ("#8b5cf6", "139,92,246"),
    };
    (hex.to_string(), rgb.to_string())
}

/// "#fb7185" -> "251,113,133" (for rgba() strings in generated JS).
fn hex_rgb(hex: &str) -> String {
    let h = hex.trim_start_matches('#');
    if h.len() != 6 {
        return "139,92,246".to_string();
    }
    let parse = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    format!("{},{},{}", parse(0), parse(2), parse(4))
}

pub(crate) fn cursor_chrome_js(style: &str, hex: &str, rgb: &str) -> String {
    let dots: usize = match style {
        "dot" | "ring" | "halo" | "pulse" | "crosshair" | "diamond" | "arrow" | "plus"
        | "star" | "bullseye" | "blob" | "neon" | "ghost" | "firefly" => 1,
        "orbit" | "ripple" => 2,
        // v0.16.0: bubbles — a head with four orbiters of growing radius.
        "bubbles" => 5,
        "trail" | "comet" => 6,
        // v0.16.0: snake (slow-lerp trail) and rainbow (hue trail).
        "snake" | "rainbow" => 8,
        // v0.13.18: stardust — a longer, jittery particle trail.
        "stardust" => 10,
        _ => return String::new(),
    };
    // @C@ = color hex, @R@ = "r,g,b" for rgba(); substituted below.
    let shape_template = match style {
        "ring" | "pulse" => {
            "width:26px;height:26px;margin:-13px 0 0 -13px;\
             border:2px solid @C@;border-radius:50%;background:transparent;"
        }
        "halo" => {
            "width:12px;height:12px;margin:-6px 0 0 -6px;\
             border-radius:50%;background:@C@;\
             box-shadow:0 0 14px 5px rgba(@R@,0.55);"
        }
        // v0.13.18: crosshair — a thin reticle ring with a center dot.
        "crosshair" => {
            "width:24px;height:24px;margin:-12px 0 0 -12px;\
             border:1.5px solid @C@;border-radius:50%;\
             background:radial-gradient(circle,@C@ 2px,transparent 3px);"
        }
        // v0.16.0 shapes: single elements, clip-path / gradient drawn.
        "diamond" => {
            "width:12px;height:12px;margin:-6px 0 0 -6px;border-radius:2px;\
             background:@C@;rotate:45deg;"
        }
        "arrow" => {
            "width:15px;height:15px;margin:-1px 0 0 -1px;background:@C@;\
             clip-path:polygon(0 0,0 100%,30% 74%,47% 100%,61% 93%,45% 68%,76% 68%);"
        }
        "plus" => {
            "width:15px;height:15px;margin:-7px 0 0 -7px;background:@C@;\
             clip-path:polygon(35% 0,65% 0,65% 35%,100% 35%,100% 65%,65% 65%,\
             65% 100%,35% 100%,35% 65%,0 65%,0 35%,35% 35%);"
        }
        "star" => {
            "width:19px;height:19px;margin:-9px 0 0 -9px;background:@C@;\
             clip-path:polygon(50% 0,63% 37%,100% 50%,63% 63%,50% 100%,37% 63%,\
             0 50%,37% 37%);"
        }
        "bullseye" => {
            "width:21px;height:21px;margin:-10px 0 0 -10px;border-radius:50%;\
             background:radial-gradient(circle,@C@ 0 2px,transparent 3px 5px,\
             @C@ 6px 7px,transparent 8px);"
        }
        "blob" => {
            "width:14px;height:14px;margin:-7px 0 0 -7px;border-radius:50%;\
             background:@C@;box-shadow:0 0 14px 7px rgba(@R@,0.35);"
        }
        "neon" => {
            "width:10px;height:10px;margin:-5px 0 0 -5px;border-radius:50%;\
             background:@C@;box-shadow:0 0 6px 2px rgba(@R@,0.85),\
             0 0 20px 10px rgba(@R@,0.35);"
        }
        "ghost" => {
            "width:22px;height:22px;margin:-11px 0 0 -11px;border-radius:50%;\
             border:2px solid rgba(@R@,0.55);background:rgba(@R@,0.08);"
        }
        "firefly" => {
            "width:9px;height:9px;margin:-4px 0 0 -4px;border-radius:50%;\
             background:@C@;box-shadow:0 0 12px 5px rgba(@R@,0.55);"
        }
        // "dot" and the trail heads (trail/comet/snake/rainbow/bubbles)
        // share the dot look; followers are smaller and fade via the
        // per-dot opacity below.
        _ => {
            "width:8px;height:8px;margin:-4px 0 0 -4px;\
             border-radius:50%;background:@C@;"
        }
    };
    let shape_css = shape_template.replace("@C@", hex).replace("@R@", rgb);
    format!(
        r#"(function () {{
  try {{
    var STYLE = {style_json};
    // Identity includes the color (v0.16.1): a color-only change used
    // to hit the same-style guard below and silently no-op in open
    // windows. IDENT is what the guard and destroy() compare.
    var IDENT = STYLE + "|" + {color_hex_json};
    if (window.__appmakaCursor) {{
      if (window.__appmakaCursor.style === IDENT) return;
      try {{ window.__appmakaCursor.destroy(); }} catch (e) {{}}
    }}
    // The cursor:none style. initialization_script runs before <head>
    // exists, so target <html> first and re-inject into <head> once
    // available so page stylesheets can't override us. The CALL is
    // deferred until the dots exist (below): this style used to be
    // injected first, and any failure after it left the native cursor
    // hidden with no drawn replacement — no cursor at all.
    function injectStyle() {{
      if (document.querySelector("[data-appmaka=cursor]")) return;
      var s = document.createElement("style");
      s.setAttribute("data-appmaka", "cursor");
      s.textContent =
        "*,*::before,*::after,html,body,:root{{cursor:none!important}}" +
        // Frame boxes keep the native cursor: over an embed the top
        // document receives no mousemove at all, so suppressing the
        // native cursor there leaves no visible pointer, and the
        // drawn dot sits parked at the frame's edge. The iframe type
        // selector outranks the universal rule above.
        "iframe{{cursor:auto!important}}";
      (document.head || document.documentElement).appendChild(s);
    }}
    // Rollback helper: removes everything we injected so the native
    // cursor comes back. Used by destroy() and by the failure path.
    function removeChrome() {{
      var junk = document.querySelectorAll("[data-appmaka=cursor],[data-appmaka=cursor-dot]");
      for (var i = 0; i < junk.length; i++) junk[i].remove();
    }}
    var obs = null;
    if (!document.head) {{
      obs = new MutationObserver(function() {{
        if (document.head) {{ injectStyle(); if (obs) obs.disconnect(); }}
      }});
      obs.observe(document.documentElement, {{ childList: true }});
    }}

    // A live controller would have returned or destroyed itself at the
    // guard above, so any cursor chrome still in the DOM here is
    // orphaned (its controller global was lost) — sweep it, or the new
    // install would draw a second cursor next to the corpse.
    removeChrome();

    var N = {dots};
    var tx = -100, ty = -100, inside = false, dead = false, rafId = 0;
    var nodes = [];
    // Factored out of the install flow: the watchdog rebuilds the
    // dots once if a page strips them, and a rebuild must produce
    // exactly what install produced.
    function buildDots() {{
      // Sweep partial remains first, or the rebuilt set stacks on
      // top of the broken one and the count check never settles.
      var stale = document.querySelectorAll("[data-appmaka=cursor-dot]");
      for (var s = 0; s < stale.length; s++) stale[s].remove();
      nodes = [];
      for (var i = 0; i < N; i++) {{
        var d = document.createElement("div");
        d.setAttribute("data-appmaka", "cursor-dot");
        d.style.cssText =
          "position:fixed;left:0;top:0;z-index:2147483647;" +
          "pointer-events:none;will-change:transform;opacity:0;" +
          {shape_css_json};
        if (N > 1 && STYLE !== "ripple") {{
          var f = 1 - i / N;
          d.style.width = Math.max(3, Math.round(8 * f)) + "px";
          d.style.height = d.style.width;
          var m = -Math.max(3, Math.round(8 * f)) / 2;
          d.style.margin = m + "px 0 0 " + m + "px";
        }}
        if (STYLE === "ripple" && i === 1) {{
          // The ripple ring rides on the head and pulses outward in the
          // frame loop; it starts as a plain ring the size of "ring".
          d.style.cssText =
            "position:fixed;left:0;top:0;z-index:2147483647;" +
            "pointer-events:none;will-change:transform,opacity;opacity:0;" +
            "width:26px;height:26px;margin:-13px 0 0 -13px;" +
            "border:2px solid {color_hex};border-radius:50%;background:transparent;";
        }}
        if (STYLE === "rainbow") {{
          // Each dot gets its own hue, head first (v0.16.0).
          d.style.background = "hsl(" + Math.round(i * 360 / N) + ",90%,65%)";
        }}
        if (STYLE === "comet" && i === 0) {{
          d.style.boxShadow = "0 0 10px 3px rgba({color_rgb},0.6)";
        }}
        document.documentElement.appendChild(d);
        var op = N > 1 ? 0.9 * (1 - i / N) + 0.1 : 1;
        if (STYLE === "orbit") op = i === 0 ? 1 : 0.85;
        nodes.push({{ el: d, x: -100, y: -100, o: op }});
      }}
      // A mid-session rebuild re-seats the dots at the last known
      // pointer position instead of leaving them invisible at the
      // off-screen start until the next leave/enter cycle.
      if (inside) {{
        for (var j = 0; j < nodes.length; j++) {{
          nodes[j].x = tx; nodes[j].y = ty;
          nodes[j].el.style.opacity = nodes[j].o;
        }}
      }}
    }}
    buildDots();

    // Hide the native cursor only now that the drawn one exists:
    // any failure above this line leaves the native cursor untouched.
    injectStyle();

    function showAt(cx, cy) {{
      tx = cx; ty = cy;
      if (!inside) {{
        inside = true;
        for (var i = 0; i < nodes.length; i++) {{
          nodes[i].x = cx; nodes[i].y = cy;
          nodes[i].el.style.opacity = nodes[i].o;
        }}
      }}
    }}

    // mousemove is the primary tracker.
    function onMove(e) {{ showAt(e.clientX, e.clientY); }}
    // mouseenter on window fires when the pointer enters the webview from
    // outside (titlebar, edge) before any mousemove fires — this is the
    // fix for "cursor never appears until I wiggle it".
    function onEnter(e) {{ showAt(e.clientX, e.clientY); }}
    function onLeave() {{
      inside = false;
      for (var i = 0; i < nodes.length; i++) nodes[i].el.style.opacity = 0;
    }}
    document.addEventListener("mousemove", onMove, {{ passive: true }});
    window.addEventListener("mouseenter", onEnter, {{ passive: true }});
    document.addEventListener("mouseleave", onLeave);
    // The document can lose the pointer without a mouseleave (focus
    // moves to another window or the native caption strip): without a
    // second net the dots would sit frozen at their last position.
    window.addEventListener("blur", onLeave);

    // One rAF loop: head snaps, followers lerp (trail effect).
    // transform-only — no layout, no paint beyond the dot elements.
    // While the pointer is out the dots are invisible (onLeave zeroed
    // their opacity), so the loop re-queues without writing anything:
    // an idle page must not pay per-frame style writes.
    (function frame() {{
      if (dead) return;
      if (!inside) {{ rafId = requestAnimationFrame(frame); return; }}
      var now = performance.now();
      var px = tx, py = ty;
      for (var i = 0; i < nodes.length; i++) {{
        var n = nodes[i];
        if (i === 0) {{
          // The head sits exactly on the pointer: a wandering head
          // (the old firefly behavior) lies about where a click lands.
          n.x = px; n.y = py;
        }}
        else if (STYLE === "orbit" || STYLE === "bubbles") {{
          // Orbit: one follower, fixed radius. Bubbles: each follower
          // gets a wider, slower orbit of its own (v0.16.0).
          var a = STYLE === "orbit" ? now / 350 : now / (300 + i * 80) + i * 1.7;
          var r = STYLE === "orbit" ? 16 : 9 + i * 5;
          n.x = px + Math.cos(a) * r;
          n.y = py + Math.sin(a) * r;
        }}
        else if (STYLE === "ripple") {{ n.x = px; n.y = py; }}
        else if (STYLE === "stardust") {{
          n.x += (px - n.x) * 0.35 + Math.sin(now / 90 + i * 1.7) * 0.7;
          n.y += (py - n.y) * 0.35 + Math.cos(now / 110 + i * 2.3) * 0.7;
        }}
        else if (STYLE === "snake") {{
          // A lazier lerp than trail — the body swings wide (v0.16.0).
          n.x += (px - n.x) * 0.22; n.y += (py - n.y) * 0.22;
        }}
        else {{ n.x += (px - n.x) * 0.4; n.y += (py - n.y) * 0.4; }}
        var tr = "translate(" + n.x + "px," + n.y + "px)";
        if (i === 0 && STYLE === "pulse") {{
          tr += " scale(" + (1 + 0.16 * Math.sin(now / 260)).toFixed(3) + ")";
        }}
        if (STYLE === "ripple" && i === 1) {{
          var ph = (now % 900) / 900;
          tr += " scale(" + (0.4 + ph * 1.6).toFixed(3) + ")";
          n.el.style.opacity = inside ? ((1 - ph) * 0.75).toFixed(3) : 0;
        }}
        n.el.style.transform = tr;
        px = n.x; py = n.y;
      }}
      rafId = requestAnimationFrame(frame);
    }})();

    function destroy() {{
      dead = true;
      cancelAnimationFrame(rafId);
      clearInterval(wd);
      document.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseenter", onEnter);
      document.removeEventListener("mouseleave", onLeave);
      window.removeEventListener("blur", onLeave);
      if (obs) obs.disconnect();
      removeChrome();
      if (window.__appmakaCursor && window.__appmakaCursor.style === IDENT) {{
        window.__appmakaCursor = null;
      }}
    }}
    // Watchdog: pages and ad scripts sometimes remove foreign DOM.
    // If our style vanished, put it back. If dots are missing, the
    // first bad tick rebuilds them once — a page that strips foreign
    // nodes a single time (ad script, SPA bootstrap) shouldn't cost
    // the drawn cursor for the whole session. A second bad tick
    // means the page keeps stripping them: tear down to the native
    // cursor rather than fight it forever. Never sit cursor-less.
    var wdRepaired = false;
    var wd = setInterval(function() {{
      if (dead) return;
      if (document.querySelectorAll("[data-appmaka=cursor-dot]").length !== N) {{
        if (!wdRepaired) {{ wdRepaired = true; try {{ buildDots(); }} catch (e) {{ destroy(); }} return; }}
        destroy(); return;
      }}
      wdRepaired = false;
      if (!document.querySelector("[data-appmaka=cursor]")) injectStyle();
    }}, 2000);
    window.__appmakaCursor = {{ style: IDENT, destroy: destroy }};
  }} catch(e) {{
    // Fail visible: strip anything we managed to inject so the
    // native cursor returns instead of leaving no cursor at all.
    // Disconnect the head observer too: left armed, it would inject
    // the cursor:none style when <head> appears, resurrecting a
    // cursor-less page this catch just cleaned up. (`obs` is
    // function-scoped, so it is safe to read here even when the
    // failure happened before its assignment ran.)
    try {{ if (obs) obs.disconnect(); }} catch (e3) {{}}
    try {{
      var junk = document.querySelectorAll("[data-appmaka=cursor],[data-appmaka=cursor-dot]");
      for (var i = 0; i < junk.length; i++) junk[i].remove();
    }} catch (e2) {{}}
  }}
}})();"#,
        dots = dots,
        shape_css_json = serde_json::to_string(&shape_css).unwrap_or_default(),
        style_json = serde_json::to_string(style).unwrap_or_default(),
        color_hex = hex,
        color_rgb = rgb,
        color_hex_json = serde_json::to_string(hex).unwrap_or_default(),
    )
}

/// The script to eval into a live page so its cursor matches `style`
/// right now: the full install script for dot/ring/trail (it destroys
/// any previous controller first), or a teardown-only snippet for
/// "off" — `cursor_chrome_js` returns "" for off because build-time
/// callers skip injection, but a live page needs the old cursor gone.
pub(crate) fn cursor_live_apply_js(style: &str, hex: &str, rgb: &str) -> String {
    match style {
        "dot" | "ring" | "trail" | "halo" | "pulse" | "orbit" | "comet" | "crosshair"
        | "ripple" | "stardust" | "diamond" | "arrow" | "plus" | "star" | "bullseye"
        | "blob" | "neon" | "ghost" | "firefly" | "snake" | "rainbow" | "bubbles" => {
            cursor_chrome_js(style, hex, rgb)
        }
        _ => "(function(){try{if(window.__appmakaCursor&&window.__appmakaCursor.destroy)window.__appmakaCursor.destroy();}catch(e){}})();"
            .to_string(),
    }
}

/// Which open windows carry the custom cursor: the same set that gets
/// the script as an initialization script at build time — account,
/// tabbed, and search windows, plus contained popups/OAuth modals
/// (v0.13.18) and preview site webviews. Settings changes broadcast
/// only to these; anything else was built without the cursor.
fn cursor_window_label(label: &str) -> bool {
    label.starts_with("acct-")
        || label.starts_with("tabbed-")
        || label.starts_with("popup-")
        || label.starts_with("oauth-")
        || (label.starts_with("preview-") && label.ends_with("-site"))
        || label == crate::websearch::SEARCH_WINDOW_LABEL
}

/// Push the cursor style into every open app window (settings-change
/// broadcast). Best-effort: a window mid-navigation may drop the eval;
/// the navigation re-apply below converges it on the next page load.
pub(crate) fn apply_cursor_live(app: &AppHandle, style: &str) {
    let (hex, rgb) = resolve_cursor_colors(app);
    let js = cursor_live_apply_js(style, &hex, &rgb);
    for (label, window) in app.webview_windows() {
        if cursor_window_label(&label) {
            let _ = window.eval(&js);
        }
    }
}

/// Re-apply the *current* saved cursor style shortly after a
/// navigation. A window built before a settings change re-runs its
/// build-time initialization script (old style) on every navigation;
/// this converges it to the saved style. Passes at widening delays,
/// because an eval that lands before the new document commits is
/// discarded with the old one — the same-style guard in the script
/// makes a redundant pass a no-op. Later passes cover slow commits
/// (VPN, consent interstitials); the final 9s pass (v0.16.3) catches
/// the slowest. Called from the
/// navigation hooks and from the search window's reuse path (all
/// platforms). Never blocks the caller: everything here runs on a
/// detached thread.
pub(crate) fn schedule_cursor_reapply(app: AppHandle, label: String) {
    let _ = std::thread::Builder::new()
        .name("appmaka-cursor-reapply".to_string())
        .spawn(move || {
            for delay_ms in [600, 2000, 4000, 9000] {
                std::thread::sleep(Duration::from_millis(delay_ms));
                let style = app
                    .try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()
                    .and_then(|s| s.lock().ok().map(|s| s.custom_cursor.clone()))
                    .unwrap_or_default();
                let Some(window) = app.get_webview_window(&label) else {
                    return;
                };
                let (hex, rgb) = resolve_cursor_colors(&app);
                let _ = window.eval(cursor_live_apply_js(&style, &hex, &rgb));
            }
        });
}

// ---------------------------------------------------------------------------
// Popup policy
// ---------------------------------------------------------------------------

static OAUTH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Counter for unique private session dir names.
static PRIVATE_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create an ephemeral session dir for a private window:
/// `<app_data>/private_sessions/.private-<millis>-<counter>/`.
/// The leading dot hides it on Unix; the whole tree is deleted when the
/// window closes, with startup orphan cleanup as the backstop.
fn new_private_session_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("could not resolve app data dir: {e}"))?
        .join("private_sessions");
    let n = PRIVATE_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!(".private-{}-{n}", unix_millis()));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create private session dir: {e}"))?;
    Ok(dir)
}

/// Best-effort delete of a private session dir. Called on window close;
/// failures are logged, never surfaced (the startup orphan sweep retries).
fn delete_private_session_dir(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir) {
        eprintln!("[appmaka] private session cleanup failed for {}: {e}", dir.display());
    }
}

/// Delete orphaned private session dirs left by crashes. Called once at
/// startup, mirroring cleanup_stale_previews.
pub(crate) fn cleanup_orphan_private_sessions(app: &AppHandle) {
    let Ok(base) = app.path().app_data_dir() else {
        return;
    };
    let dir = base.join("private_sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".private-") {
            delete_private_session_dir(&entry.path());
        }
    }
}

/// The ephemeral session dir for a private window label, if it is one.
pub(crate) fn private_dir_for(app: &AppHandle, label: &str) -> Option<PathBuf> {
    app.try_state::<WindowState>().and_then(|s| {
        s.inner.lock().ok().and_then(|g| {
            g.get(label)
                .filter(|t| t.is_private)
                .and_then(|t| t.private_dir.clone())
        })
    })
}

/// Throttle for blocked-popup notices: a popup-spamming page must not
/// flood the main window's banner. One notice per 60 seconds, globally.
static LAST_BLOCK_NOTICE_SECS: AtomicU64 = AtomicU64::new(0);

/// Tell the user a popup was blocked and how to allow it. Silent denial
/// leaves them clicking "Sign in with Google" wondering why nothing
/// happens; the notice names the host and points at the allowlist.
fn notify_popup_blocked(app: &AppHandle, app_name: &str, host: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_BLOCK_NOTICE_SECS.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 60 {
        return;
    }
    LAST_BLOCK_NOTICE_SECS.store(now, Ordering::Relaxed);
    let host = if host.is_empty() { "this site" } else { host };
    crate::popup_add::emit_notice(
        app,
        &format!(
            "Blocked a popup from {host} ({app_name}). To allow sign-in popups, add {host} to the popup allowlist in {app_name}'s app settings."
        ),
    );
}

/// Initialization script injected into every account window so plain
/// left-clicks on `target="_blank"` links reach the popup policy handler.
///
/// Why this exists: on WebKitGTK (wry 0.57, WebKitGTK 2.52) a
/// `target="_blank"` link click never emits the `create` signal that backs
/// `on_new_window` — verified by click test: the click lands (page JS runs)
/// but no new-window request is produced, so the link is silently dropped
/// even with policy Allow. `window.open()` does emit `create` and works.
/// The shim re-routes qualifying link clicks through `window.open`, which
/// goes through `make_popup_handler` and gets the same policy/allowlist
/// treatment. Modified clicks (Ctrl/Cmd/Shift/Alt, middle button) are left
/// alone. Runs in the page's main world but touches no page state and
/// exposes no IPC; `withGlobalTauri` stays false.
pub(crate) const TARGET_BLANK_SHIM_JS: &str = r#"(function () {
  document.addEventListener('click', function (e) {
    if (e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    var t = e.target;
    var a = (t && t.closest) ? t.closest('a[target="_blank"]') : null;
    if (!a || !a.href) return;
    e.preventDefault();
    e.stopPropagation();
    window.open(a.href, '_blank');
  }, true);
})();"#;

/// Back/forward keyboard navigation for account windows. They are bare
/// webviews with no browser chrome, so Alt+Left / Alt+Right would otherwise
/// do nothing — every browser reserves them for history navigation, and
/// users replacing their browser expect them to work. Capture phase +
/// preventDefault matches browser behavior (the browser consumes the keys
/// before the page even when focus is in a text field). Pure page JS: no
/// page state touched, no IPC, `withGlobalTauri` stays false.
pub(crate) const NAV_KEYS_JS: &str = r#"(function () {
  document.addEventListener('keydown', function (e) {
    if (!e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
    if (e.key === 'ArrowLeft') { e.preventDefault(); e.stopPropagation(); history.back(); }
    else if (e.key === 'ArrowRight') { e.preventDefault(); e.stopPropagation(); history.forward(); }
  }, true);
})();
"#;

/// F1 shortcut cheatsheet overlay (v0.13.5).
///
/// Press F1 inside any app window to show a floating card listing all
/// built-in AppMaka keyboard shortcuts. Press F1 again or Escape to dismiss.
/// Pure page JS — no IPC, no Tauri globals, no DOM pollution on other pages.
/// The F1 overlay script with its key names tinted in the user's accent
/// color (v0.13.17). The base script is a const; the accent hex is
/// swapped in per window build from the saved setting.
pub(crate) fn shortcuts_overlay_js(app: &AppHandle) -> String {
    // The settings live in managed state; the disk load is only a
    // fallback for calls made before the state is registered.
    let accent = app
        .try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.accent.clone()))
        .unwrap_or_else(|| crate::launcher_settings::load(app).accent);
    SHORTCUTS_OVERLAY_JS.replace("#a5b4fc", crate::launcher_settings::accent_text_hex(&accent))
}

pub(crate) const SHORTCUTS_OVERLAY_JS: &str = r#"(function () {
  try {
    var OVERLAY_ID = "__appmaka_shortcuts__";
    var SHORTCUTS = [
      ["Alt + ←",        "Go back"],
      ["Alt + →",        "Go forward"],
      ["F1",             "Show / hide this shortcuts list"],
      ["F5",             "Reload page"],
      ["F11",            "Full screen on / off"],
      ["Mouse to top",   "Show the window bar while full screen"],
      ["Ctrl + Shift + A", "Add this page to your apps"],
      ["Ctrl + +",       "Zoom in"],
      ["Ctrl + -",       "Zoom out"],
      ["Ctrl + 0",       "Reset zoom"],
      ["Ctrl + F",       "Find in page (browser default)"],
      ["Hold LMB + Esc", "Close this window"],
      ["Alt + F4",       "Close this window"],
    ];

    function buildOverlay() {
      var el = document.createElement("div");
      el.id = OVERLAY_ID;
      el.setAttribute("data-appmaka", "shortcuts-overlay");
      el.style.cssText = [
        "position:fixed",
        "top:48px",
        "right:16px",
        "z-index:2147483646",
        "background:rgba(20,20,28,0.97)",
        "color:#e2e2e8",
        "border:1px solid rgba(255,255,255,0.12)",
        "border-radius:10px",
        "padding:14px 18px 12px",
        "font:13px/1.6 system-ui,sans-serif",
        "box-shadow:0 8px 32px rgba(0,0,0,0.5)",
        "min-width:280px",
        "pointer-events:none",
        "user-select:none",
        "animation:__am_fadein 0.12s ease",
      ].join(";");

      var style = document.createElement("style");
      style.textContent =
        "@keyframes __am_fadein{from{opacity:0;transform:translateY(-6px)}to{opacity:1;transform:translateY(0)}}";
      el.appendChild(style);

      var title = document.createElement("div");
      title.textContent = "AppMaka Shortcuts";
      title.style.cssText = "font-weight:600;font-size:11px;text-transform:uppercase;letter-spacing:.07em;color:#8b8b9b;margin-bottom:8px;";
      el.appendChild(title);

      var table = document.createElement("table");
      table.style.cssText = "border-collapse:collapse;width:100%;";
      SHORTCUTS.forEach(function(row) {
        var tr = document.createElement("tr");
        var kd = document.createElement("td");
        kd.textContent = row[0];
        kd.style.cssText = "padding:2px 12px 2px 0;white-space:nowrap;color:#a5b4fc;font-family:ui-monospace,monospace;font-size:12px;";
        var vd = document.createElement("td");
        vd.textContent = row[1];
        vd.style.cssText = "padding:2px 0;color:#c9c9d4;";
        tr.appendChild(kd);
        tr.appendChild(vd);
        table.appendChild(tr);
      });
      el.appendChild(table);

      var hint = document.createElement("div");
      hint.textContent = "Press F1 or Esc to close";
      hint.style.cssText = "margin-top:8px;font-size:11px;color:#5a5a70;";
      el.appendChild(hint);

      return el;
    }

    function toggle() {
      var existing = document.getElementById(OVERLAY_ID);
      if (existing) { existing.remove(); return; }
      (document.body || document.documentElement).appendChild(buildOverlay());
    }

    document.addEventListener("keydown", function(e) {
      if (e.key === "F1") {
        e.preventDefault();
        e.stopPropagation();
        toggle();
        return;
      }
      if (e.key === "Escape" && document.getElementById(OVERLAY_ID)) {
        e.preventDefault();
        document.getElementById(OVERLAY_ID).remove();
      }
    }, true);
  } catch(e) {}
})();"#;

/// Everything the popup handler needs, captured by value (the handler is
/// `Fn`, so it can't borrow from the stack frame that creates the window).
/// Shared with the preview flow (`preview.rs`), which passes placeholder
/// ids since the app doesn't exist yet.
#[derive(Clone)]
pub(crate) struct PopupContext {
    pub(crate) app: AppHandle,
    pub(crate) app_id: String,
    pub(crate) account_id: String,
    pub(crate) app_name: String,
    pub(crate) app_url: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) popup_policy: String,
    pub(crate) popup_allowlist: Vec<String>,
    /// Label of the window this context's webview lives in, so a popup
    /// can be placed over its opener (v0.13.15). Empty when unknown —
    /// placement falls back to screen-center.
    pub(crate) parent_label: String,
    /// True when this context's window is itself an allowed popup:
    /// popups IT opens are chain-trusted and skip the root app's
    /// policy/allowlist re-check. Sign-in and checkout chains hop
    /// hosts per step (accounts → consent → captcha → IdP), and
    /// re-judging each hop against the first app's allowlist killed
    /// hop 2+ intermittently (his "pop ups dont open pop ups" report,
    /// 2026-10-17). The chain window already runs with the account's
    /// session; letting it open its flow windows widens nothing.
    /// Root contexts (account/tabbed/preview) always start false.
    pub(crate) chain_allowed: bool,
}

/// Build the `on_new_window` handler for an account window.
///
/// Policy "allow": every new-window request (window.open, target=_blank)
/// becomes a contained popup sharing the account's session directory, so
/// logins carry over. Policy "block": only allowlisted hosts get the
/// OAuth modal (with its auto-close script); everything else is denied
/// with a notice saying how to allow it.
///
/// How an allowed popup is created differs by platform — see
/// `open_contained` (Windows returns a real popup with a live
/// `window.opener`; other platforms deny and rebuild on a spawned
/// thread, the historically proven path there).
///
/// Also used by the preview flow with placeholder ids.
///
/// NOTE: the policy is captured when the account window is built. Changing
/// the per-account or app-level popup policy while the window is open has
/// no effect until the window is closed and reopened — the edit dialogs
/// must say so (frontend copy, coordinator-owned). The one exception is
/// the global override (`allow_popups_globally` in launcher settings),
/// which is read live on every request and applies immediately.
/// Popup close-lifecycle trace: one line per close step, appended to
/// app_data/popup-lifecycle.log (best-effort, capped) and echoed to
/// stderr. The next zombie-renderer report can be matched against this
/// file to see exactly which step never fired: created/spawned ->
/// close-requested -> destroyed -> wm-ncdestroy-subclass-removed
/// (Windows chrome only). eprintln alone is invisible on Windows GUI
/// builds, hence the file.
pub(crate) fn log_popup_lifecycle(app: &AppHandle, label: &str, step: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{secs} {label} {step}\n");
    eprintln!("[appmaka] popup-lifecycle {label} {step}");
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let path = dir.join("popup-lifecycle.log");
    // Cap: truncate once over ~256 KiB so the trace can't grow unbounded.
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 262_144 {
        let _ = std::fs::write(&path, "");
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Attach the close-lifecycle trace to a contained popup window.
fn attach_popup_lifecycle_trace(app: &AppHandle, window: &tauri::WebviewWindow, label: &str) {
    let log_app = app.clone();
    let log_label = label.to_string();
    window.on_window_event(move |event| {
        match event {
            tauri::WindowEvent::CloseRequested { .. } => {
                log_popup_lifecycle(&log_app, &log_label, "close-requested");
            }
            tauri::WindowEvent::Destroyed => {
                log_popup_lifecycle(&log_app, &log_label, "destroyed");
            }
            _ => {}
        }
    });
}

pub(crate) fn make_popup_handler(
    ctx: PopupContext,
) -> impl Fn(url::Url, tauri::webview::NewWindowFeatures) -> tauri::webview::NewWindowResponse<tauri::Wry>
       + Send
       + 'static {
    move |url: url::Url, features| {
        if popup_allowed_plain(
            ctx.chain_allowed,
            &ctx.popup_policy,
            global_popups_allowed(&ctx.app),
        ) {
            open_contained(&ctx, "popup", &url, &features)
        } else {
            // "block": only allowlisted hosts get a contained popup.
            let host = url.host_str().unwrap_or("").to_lowercase();
            if ctx
                .popup_allowlist
                .iter()
                .any(|h| h.eq_ignore_ascii_case(&host))
            {
                open_contained(&ctx, "oauth", &url, &features)
            } else {
                // Not silent: the user clicked something expecting a
                // window, so say what happened and how to allow it.
                notify_popup_blocked(&ctx.app, &ctx.app_name, &host);
                tauri::webview::NewWindowResponse::Deny
            }
        }
    }
}

/// Route an allowed popup to a window.
///
/// Windows (v0.13.15): build the window inside the handler and return it
/// as the popup itself (`NewWindowResponse::Create`), so the page keeps
/// a live `window.opener`. The old deny-and-rebuild severed that link,
/// which broke sign-in popups two ways: flows that open an initially
/// blank popup and navigate it afterwards had nothing to navigate (the
/// popup sat blank forever), and flows that postMessage their result
/// back to the opener (Google/Apple/Microsoft sign-in) never delivered
/// it — the user's "popups open blank and logins don't finish" report.
/// Building here is the designed flow, not the wry#583 trap: wry takes a
/// WebView2 deferral and dispatches this handler onto the message loop
/// precisely so Create can build (Microsoft's reentrancy guidance), and
/// Tauri's dispatcher executes window creation inline when already on
/// the main thread. If the build somehow fails, fall back to the legacy
/// spawned rebuild rather than swallowing the popup.
///
/// Other platforms keep deny + rebuild on a spawned thread: WebKitGTK
/// verified working end-to-end, and wry's Create there wants a
/// related-view the Tauri window API doesn't expose.
fn open_contained(
    ctx: &PopupContext,
    kind: &str,
    url: &url::Url,
    features: &tauri::webview::NewWindowFeatures,
) -> tauri::webview::NewWindowResponse<tauri::Wry> {
    #[cfg(windows)]
    {
        if let Some(window) = build_contained_window(ctx, kind, url, features) {
            return tauri::webview::NewWindowResponse::Create { window };
        }
    }
    #[cfg(not(windows))]
    let _ = features;
    match kind {
        "oauth" => spawn_oauth_modal(ctx, url),
        _ => spawn_popup_window(ctx, url),
    }
    tauri::webview::NewWindowResponse::Deny
}

/// Build a contained popup window synchronously for
/// `NewWindowResponse::Create` (Windows only — see `open_contained`).
/// Mirrors what the legacy spawned builder set up — same session
/// directory (so the same WebView2 environment and cookie jar), the
/// download hook, the title-bar tint machinery — plus the two things the
/// rebuild could never do: honor the size/position the page requested,
/// and attach this same popup policy to the new window so nested
/// popups in a sign-in chain behave identically instead of being
/// denied outright.
#[cfg(windows)]
fn build_contained_window(
    ctx: &PopupContext,
    kind: &str,
    url: &url::Url,
    features: &tauri::webview::NewWindowFeatures,
) -> Option<tauri::WebviewWindow> {
    let n = OAUTH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let label = format!("{kind}-{n}");
    let title = if kind == "oauth" {
        format!("{} — sign-in", ctx.app_name)
    } else {
        format!("{} — popup", ctx.app_name)
    };
    // Honor the size the page asked for, clamped to something sane.
    // (The rebuild always used 640x720, whatever the page requested.)
    let (mut w, mut h) = features
        .size()
        .map(|s| (s.width, s.height))
        .unwrap_or((640.0, 720.0));
    w = w.clamp(320.0, 1600.0);
    h = h.clamp(240.0, 1200.0);
    let mut nested_ctx = ctx.clone();
    nested_ctx.parent_label = label.clone();
    // This window exists because a popup was allowed; its own popups
    // are the next hop of the same flow (chain trust).
    nested_ctx.chain_allowed = true;
    let mut builder = WebviewWindowBuilder::new(&ctx.app, &label, WebviewUrl::External(url.clone()))
        .data_directory(ctx.session_dir.clone())
        .title(&title)
        .inner_size(w, h)
        .on_new_window(make_popup_handler(nested_ctx))
        .on_download(crate::downloads::make_download_handler(ctx.app.clone()));
    // Same session folder as the account/tabbed builders: the browser
    // arguments must match theirs exactly (see WEBVIEW2_BROWSER_ARGS).
    #[cfg(windows)]
    {
        builder = builder.additional_browser_args(WEBVIEW2_BROWSER_ARGS);
    }
    if kind == "oauth" {
        builder = builder.initialization_script(oauth_autoclose_script(&app_origin(&ctx.app_url)));
    }
    // v0.13.18: popups get the drawn cursor too — same reasoning as
    // previews (a sign-in popup with no visible mouse is a dead end).
    let cursor_js = saved_cursor_js(&ctx.app);
    if !cursor_js.is_empty() {
        builder = builder.initialization_script(cursor_js);
    }
    // Placement: where the page asked, else centered over the window
    // that opened it — the popup lands on the user's actual screen, not
    // blindly on the primary monitor.
    if let Some(pos) = features.position() {
        builder = builder.position(pos.x, pos.y);
    } else if let Some((x, y)) = centered_over_parent(ctx, w, h) {
        builder = builder.position(x, y);
    } else {
        builder = builder.center();
    }
    if kind == "popup" {
        let nav_app = ctx.app.clone();
        let nav_label = label.clone();
        builder = builder.on_navigation(move |nav_url: &url::Url| {
            let app = nav_app.clone();
            let lbl = nav_label.clone();
            let u = nav_url.clone();
            std::thread::Builder::new()
                .name(format!("appmaka-popup-retint-{lbl}"))
                .spawn(move || {
                    crate::popup_chrome::tint_popup_caption(&app, &lbl, &u);
                    schedule_cursor_reapply(app.clone(), lbl.clone());
                })
                .ok();
            true
        });
    }
    match builder.build() {
        Ok(window) => {
            if kind == "popup" {
                crate::popup_chrome::setup_popup_chrome(&ctx.app, &window, &label, url);
            }
            // Close-lifecycle trace (see log_popup_lifecycle): a popup
            // that never logs "destroyed" after its close is the zombie.
            attach_popup_lifecycle_trace(&ctx.app, &window, &label);
            log_popup_lifecycle(&ctx.app, &label, "created");
            // v0.13.18: page-requested fullscreen works in popups too.
            #[cfg(windows)]
            attach_fullscreen_sync(&window);
            Some(window)
        }
        Err(e) => {
            // Log it, but no dialog: open_contained falls back to the
            // spawned builder when this returns None, and that usually
            // succeeds — a "could not open" dialog in front of a popup
            // that did open is a false alarm.
            crate::errors::record(
                &ctx.app,
                "window-open",
                &format!("Could not open the {kind} window."),
                &format!("{kind} window failed for {}/{}: {e}", ctx.app_id, ctx.account_id),
                false,
            );
            None
        }
    }
}

/// Logical (x, y) that centers a `w`x`h` popup over the context's
/// parent window, or None when the parent can't be measured (the
/// builder's plain center() is the fallback).
#[cfg(windows)]
fn centered_over_parent(ctx: &PopupContext, w: f64, h: f64) -> Option<(f64, f64)> {
    let parent = ctx.app.get_webview_window(&ctx.parent_label)?;
    let pos = parent.outer_position().ok()?;
    let size = parent.outer_size().ok()?;
    let scale = parent.scale_factor().ok()?;
    Some((
        pos.x as f64 / scale + (size.width as f64 / scale - w) / 2.0,
        pos.y as f64 / scale + (size.height as f64 / scale - h) / 2.0,
    ))
}

/// The OAuth modal's best-effort auto-close script: poll until the
/// popup navigates back to the app's own origin, then close. Shared by
/// the spawned builder and the in-handler builder so both paths behave
/// identically.
fn oauth_autoclose_script(home_origin: &str) -> String {
    let json_home = serde_json::to_string(home_origin).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "(function(){{var home={json_home};\
        var t=setInterval(function(){{try{{\
        if(window.location.origin===home){{window.close();clearInterval(t);}}\
        }}catch(e){{}}}},1500);}})();"
    )
}

/// The global popup override from launcher settings, read live on
/// every new-window request so the Settings switch applies to
/// already-open windows immediately. Fail-closed: any trouble
/// reading the setting means no override.
fn global_popups_allowed(app: &AppHandle) -> bool {
    app.try_state::<Mutex<crate::launcher_settings::LauncherSettings>>()
        .and_then(|s| s.lock().ok().map(|s| s.allow_popups_globally))
        .unwrap_or(false)
}

/// Plain-popup decision: the per-app "allow" policy, or the global
/// override. (Under "block" without the override, allowlisted hosts
/// still get the OAuth modal instead — decided by the caller.)
fn popup_opens_plain(policy: &str, global_allow: bool) -> bool {
    policy == "allow" || global_allow
}

/// Plain-popup decision for a new-window request, chain-aware: a
/// window that is itself an allowed popup (chain_allowed) opens its
/// popups plain without re-judging policy — see PopupContext.
fn popup_allowed_plain(chain_allowed: bool, policy: &str, global_allow: bool) -> bool {
    chain_allowed || popup_opens_plain(policy, global_allow)
}

/// `scheme://host[:port]` of the app's URL, for the modal auto-close check.
/// Built from parsed parts so no quote characters can sneak in.
fn app_origin(app_url: &str) -> String {
    url::Url::parse(app_url)
        .map(|u| {
            let host = u.host_str().unwrap_or("");
            match u.port() {
                Some(p) => format!("{}://{host}:{p}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        })
        .unwrap_or_default()
}

/// Open a general popup as a plain contained window bound to the SAME
/// session directory, so logins carry over. Unlike the OAuth modal there is
/// no auto-close script: a general popup is user content that stays open
/// until the user closes it. (The old code routed "allow" popups through
/// the OAuth modal, whose auto-close script killed same-origin popups
/// within ~1.5 s — the "popups don't open even on Allow" bug.)
pub(crate) fn spawn_popup_window(ctx: &PopupContext, url: &url::Url) {
    let title = format!("{} — popup", ctx.app_name);
    let mut nested_ctx = ctx.clone();
    nested_ctx.chain_allowed = true;
    spawn_contained_window("popup", url, &title, None, "popup", nested_ctx);
}

/// Open an allowlisted popup as a small modal bound to the SAME session
/// directory, so an OAuth login lands in the right account's cookie jar.
/// The modal self-closes (best-effort) when navigation returns to the app's
/// origin; the user can always close it by hand.
fn spawn_oauth_modal(ctx: &PopupContext, url: &url::Url) {
    let autoclose = oauth_autoclose_script(&app_origin(&ctx.app_url));
    let title = format!("{} — sign-in", ctx.app_name);
    let log_ctx = format!("{}/{}", ctx.app_id, ctx.account_id);
    let mut nested_ctx = ctx.clone();
    nested_ctx.chain_allowed = true;
    spawn_contained_window("oauth", url, &title, Some(autoclose), &log_ctx, nested_ctx);
}

/// Shared contained-window builder for popups and OAuth modals.
///
/// SECURITY POSTURE: these windows load external site URLs only, never our
/// frontend bundle, and `withGlobalTauri` is false crate-wide, so no Tauri
/// IPC (`window.__TAURI__`) is ever injected into them — they cannot invoke
/// backend commands. The only scripts ever injected are our own static
/// snippets: the JSON-escaped OAuth auto-close script and the custom
/// cursor (the same one account windows get). Nested popups get the
/// standard policy handler with chain trust (popup_ctx) — this path
/// used to hard-deny them, which broke multi-hop sign-in chains
/// whenever it was the path taken.
fn spawn_contained_window(
    label_prefix: &str,
    url: &url::Url,
    title: &str,
    initialization_script: Option<String>,
    log_ctx: &str,
    popup_ctx: PopupContext,
) {
    let n = OAUTH_COUNTER.fetch_add(1, Ordering::Relaxed);
    let label = format!("{label_prefix}-{n}");
    let mut popup_ctx = popup_ctx;
    popup_ctx.parent_label = label.clone();
    // The window MUST be built on a dedicated thread, never on the calling
    // (WebView2 popup) thread and never via run_on_main_thread: on Windows,
    // WebviewWindowBuilder::build() deadlocks in a synchronous context
    // (wry#583) — and building it *on* the main thread self-deadlocks, since
    // build() waits for the event loop that is busy running the closure.
    // A plain spawned thread just blocks on the create-window round-trip
    // while the main thread and all WebView2 threads stay free. On Linux
    // the same pattern works: build() marshals creation onto the GTK main
    // loop from any thread. Everything the thread touches is cloned up
    // front ('static).
    let window_app = popup_ctx.app.clone();
    let url = url.clone();
    let session_dir = popup_ctx.session_dir.clone();
    let title = title.to_string();
    let log_ctx = log_ctx.to_string();
    let label_prefix = label_prefix.to_string();
    let _ = std::thread::Builder::new()
        .name(format!("appmaka-{label_prefix}-{n}"))
        .spawn(move || {
            let mut builder = WebviewWindowBuilder::new(
                &window_app,
                &label,
                WebviewUrl::External(url.clone()),
            )
            .data_directory(session_dir.clone())
            .title(&title)
            .inner_size(640.0, 720.0)
            // Nested popups: same policy handler as every other
            // window, chain-trusted (this window is an allowed popup).
            .on_new_window(make_popup_handler(popup_ctx))
            // Popup downloads go through the same in-app manager as every
            // other window (v0.9.8): without this they silently bypassed
            // the download folder, the history, and Mark of the Web.
            .on_download(crate::downloads::make_download_handler(window_app.clone()));
            // Same session folder as the account/tabbed builders: the
            // browser arguments must match theirs exactly (see
            // WEBVIEW2_BROWSER_ARGS).
            #[cfg(windows)]
            {
                builder = builder.additional_browser_args(WEBVIEW2_BROWSER_ARGS);
            }
            // v0.13.18: open on the monitor the user is actually on.
            if let Some((x, y)) = cursor_centered_position(&window_app, 640.0, 720.0) {
                builder = builder.position(x, y);
            }
            if let Some(script) = initialization_script {
                builder = builder.initialization_script(script);
            }
            // v0.13.18: drawn cursor in popups (see build_contained_window).
            let cursor_js = saved_cursor_js(&window_app);
            if !cursor_js.is_empty() {
                builder = builder.initialization_script(cursor_js);
            }
            // v0.9.11: re-tint the title bar when the popup navigates
            // (theme-color can change per page). The hook must never block
            // the navigation decision, so the work goes to a detached
            // thread; the tint itself is best-effort and silent.
            #[cfg(windows)]
            if label_prefix == "popup" {
                let nav_app = window_app.clone();
                let nav_label = label.clone();
                builder = builder.on_navigation(move |nav_url: &url::Url| {
                    let app = nav_app.clone();
                    let lbl = nav_label.clone();
                    let u = nav_url.clone();
                    std::thread::Builder::new()
                        .name(format!("appmaka-popup-retint-{lbl}"))
                        .spawn(move || {
                            crate::popup_chrome::tint_popup_caption(&app, &lbl, &u);
                            schedule_cursor_reapply(app.clone(), lbl.clone());
                        })
                        .ok();
                    true
                });
            }
            match builder.build() {
                Ok(_window) => {
                    // Close-lifecycle trace (see log_popup_lifecycle).
                    attach_popup_lifecycle_trace(&window_app, &_window, &label);
                    log_popup_lifecycle(&window_app, &label, "spawned");
                    // v0.13.18: page-requested fullscreen works in popups too.
                    #[cfg(windows)]
                    attach_fullscreen_sync(&_window);
                    // v0.9.11: "Add to applications…" system-menu item +
                    // title-bar tint. Windows-only; best-effort and silent.
                    #[cfg(windows)]
                    if label_prefix == "popup" {
                        crate::popup_chrome::setup_popup_chrome(
                            &window_app,
                            &_window,
                            &label,
                            &url,
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[appmaka] {label_prefix} window failed for {log_ctx}: {e}");
                    // v0.11.0: contained windows (popups, OAuth) have no
                    // other error surface — record and pop the dialog.
                    crate::errors::record(
                        &window_app,
                        "window-open",
                        &format!("Could not open the {label_prefix} window."),
                        &format!("{label_prefix} window failed for {log_ctx}: {e}"),
                        true,
                    );
                }
            }
        });
}

// ---------------------------------------------------------------------------
// Focus tracking / suspend
// ---------------------------------------------------------------------------

/// Navigate a lazily-restored window to its deferred URL on first focus.
/// Takes the pending URL out of the tracked entry (one-shot); a missing
/// window or entry is a silent no-op.
fn navigate_pending_url(app: &AppHandle, label: &str) {
    let url = {
        let winstate = match app.try_state::<WindowState>() {
            Some(s) => s,
            None => return,
        };
        let mut tracked = match winstate.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        match tracked.get_mut(label) {
            Some(t) => t.pending_url.take(),
            None => None,
        }
    };
    if let Some(url) = url {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.navigate(url.parse().unwrap_or_else(|_| {
                url::Url::parse("about:blank").expect("about:blank parses")
            }));
        }
    }
}

/// Snapshot the webview's live URL into the tracked entry (v0.16.4).
/// Runs at the moments the URL is both current and about to become
/// unreadable — focus loss, before suspend, before close — and the
/// value reaches disk on the next session write, so a reopen can
/// return to the page instead of the app's home URL. about:blank is
/// never stored: a lazily-restored window that was never focused has
/// no page worth returning to, and its navigation target is already
/// recorded in pending_url.
fn capture_last_url(app: &AppHandle, label: &str) {
    let Some(window) = app.get_webview_window(label) else {
        return;
    };
    let Ok(url) = window.url() else {
        return;
    };
    let url = url.to_string();
    if url == "about:blank" {
        return;
    }
    if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(mut tracked) = winstate.inner.lock() {
            if let Some(t) = tracked.get_mut(label) {
                t.last_url = Some(url);
            }
        }
    }
}

/// Whether a focus event clears the window's unread badge (v0.16.4).
/// A window that was suspended at focus time keeps its badge: its
/// renderer was frozen, so its title may never change again after
/// wake — clearing here could erase an unread count the user hasn't
/// seen, permanently. Pure so the rule is unit-testable.
fn focus_clears_badge(was_suspended: bool) -> bool {
    !was_suspended
}

fn touch_window(app: &AppHandle, label: &str, focused: bool) {
    if !focused {
        // Last reliable read of the page before the user moves on;
        // later state changes (suspend, close) may make it stale or
        // unreadable.
        capture_last_url(app, label);
    }
    if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(mut tracked) = winstate.inner.lock() {
            if let Some(t) = tracked.get_mut(label) {
                t.last_active = unix_secs();
                if focused {
                    t.suspended = false;
                }
            }
        }
    }
    #[cfg(windows)]
    if focused {
        resume_window(app, label);
    } else if let Some(window) = app.get_webview_window(label) {
        set_window_memory_target_low(&window, true);
    }
}

/// Set WebView2 memory target level to LOW (unfocused/idle) or NORMAL (focused).
/// Signals Chromium to drop unused decode caches, run V8 GC, and release working set.
#[cfg(windows)]
pub(crate) fn set_window_memory_target_low(window: &WebviewWindow, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows_core::Interface;

    let target = if low {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
    } else {
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
    };

    let _ = window.with_webview(move |platform| unsafe {
        if let Ok(core) = platform.controller().CoreWebView2() {
            if let Ok(core19) = core.cast::<ICoreWebView2_19>() {
                let _ = core19.SetMemoryUsageTargetLevel(target);
            }
        }
    });
}

#[cfg(not(windows))]
pub(crate) fn set_window_memory_target_low(_window: &WebviewWindow, _low: bool) {}

/// Restore the pre-suspend title and wake the renderer. WebView2 auto-resumes
/// a suspended page when its controller becomes visible again; the explicit
/// Resume() covers the case where visibility didn't flip.
#[cfg(windows)]
fn resume_window(app: &AppHandle, label: &str) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, ICoreWebView2_3, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows_core::Interface;

    let Some(window) = app.get_webview_window(label) else {
        return;
    };
    let title_opt = if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(tracked) = winstate.inner.lock() {
            tracked.get(label).map(|t| t.base_title.clone())
        } else {
            None
        }
    } else {
        None
    };
    if let Some(title) = title_opt {
        let _ = window.set_title(&title);
    }
    let _ = window.with_webview(|platform| unsafe {
        // Resume before showing (v0.16.4): a still-suspended controller
        // made visible first presents its frozen — possibly blank —
        // frame until Resume lands. Waking the renderer first lets the
        // first frame the user sees be live content.
        if let Ok(core) = platform.controller().CoreWebView2() {
            if let Ok(core19) = core.cast::<ICoreWebView2_19>() {
                let _ = core19.SetMemoryUsageTargetLevel(COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL);
            }
            if let Ok(core3) = core.cast::<ICoreWebView2_3>() {
                let _ = core3.Resume();
            }
        }
        let _ = platform.controller().SetIsVisible(true);
    });
}


fn suspend_idle_windows(app: &AppHandle) {
    let now = unix_secs();
    // Snapshot under the lock; the actual suspend calls happen outside it.
    // account_id rides along so per-account timer overrides (v0.8.1) resolve.
    let tracked: Vec<(String, String, String, u64, bool)> = match app.try_state::<WindowState>() {
        Some(winstate) => match winstate.inner.lock() {
            Ok(map) => map
                .iter()
                .map(|(label, t)| {
                    (
                        label.clone(),
                        t.app_id.clone(),
                        t.account_id.clone(),
                        t.last_active,
                        t.suspended,
                    )
                })
                .collect(),
            Err(_) => return,
        },
        None => return,
    };
    let Some(store) = app.try_state::<AppStore>() else {
        return;
    };
    for (label, app_id, account_id, last_active, suspended) in tracked {
        if suspended {
            continue;
        }
        let minutes = match store.get(&app_id) {
            Ok(a) => a.effective_auto_suspend_minutes(&account_id),
            Err(_) => continue, // app deleted under us; its windows are being closed
        };
        if minutes == 0 {
            continue;
        }
        if now.saturating_sub(last_active) < minutes * 60 {
            continue;
        }
        let Some(window) = app.get_webview_window(&label) else {
            continue;
        };
        // Fail closed: if focus state is unknown, don't suspend.
        if window.is_focused().unwrap_or(true) {
            continue;
        }
        suspend_one(app, &label, &window);
    }
}

/// Close account windows idle longer than their app's `auto_close_minutes`
/// (0 = never; default 30). Closing destroys the renderer and frees its
/// memory; the session directory on disk preserves the login so reopening
/// restores it seamlessly. Fail closed: focused windows (or unknown focus
/// state) are never auto-closed.
fn close_idle_windows(app: &AppHandle) {
    let now = unix_secs();
    let tracked: Vec<(String, String, String, u64)> = match app.try_state::<WindowState>() {
        Some(winstate) => match winstate.inner.lock() {
            Ok(map) => map
                .iter()
                .map(|(label, t)| {
                    (
                        label.clone(),
                        t.app_id.clone(),
                        t.account_id.clone(),
                        t.last_active,
                    )
                })
                .collect(),
            Err(_) => return,
        },
        None => return,
    };
    let Some(store) = app.try_state::<AppStore>() else {
        return;
    };
    for (label, app_id, account_id, last_active) in tracked {
        let minutes = match store.get(&app_id) {
            Ok(a) => a.effective_auto_close_minutes(&account_id),
            Err(_) => continue, // app deleted under us; its windows are being closed
        };
        if minutes == 0 {
            continue;
        }
        if now.saturating_sub(last_active) < minutes as u64 * 60 {
            continue;
        }
        let Some(window) = app.get_webview_window(&label) else {
            continue;
        };
        // Fail closed: if focus state is unknown, don't close.
        if window.is_focused().unwrap_or(true) {
            continue;
        }
        close_tracked_window(app, &label, CloseIntent::Background);
    }
}

/// Background watchdog: every 60s, suspend account windows idle longer than
/// their app's `auto_suspend_minutes` (0 = never), then close account windows
/// idle longer than their app's `auto_close_minutes` (default 30, 0 = never).
/// Suspended windows keep their session directory, so reopening/focusing
/// resumes the session. Closed windows are fully destroyed (renderer freed —
/// the real RAM win, and the only automatic reclaim on Linux); the session
/// directory on disk preserves the login, so reopening restores it.
///
/// NOTE (v0.9.9): an idle-window discard-to-blank ("Memory Saver") was
/// prototyped and then CUT before release. It could not meet the safety bar
/// — media playback, focused inputs, and unsaved form state are not
/// detectable from Rust without a JS bridge, and Tauri IPC is never exposed
/// to site windows — so it shipped as nothing rather than as a half-working
/// memory saver. The git history holds the prototype if a future release
/// finds a safe detection path.
pub fn start_suspend_watcher(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("appmaka-suspend".to_string())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            suspend_idle_windows(&app);
            close_idle_windows(&app);
        });
}

/// Suspend one window immediately (the `suspend_account` command path).
/// Validates the account first so typos fail loudly; a closed window is a
/// no-op success.
pub fn suspend_account_window(
    app: &AppHandle,
    store: &AppStore,
    app_id: &str,
    account_id: &str,
) -> Result<(), String> {
    let web_app = store.get(app_id)?;
    if !web_app.accounts.iter().any(|a| a.id == account_id) {
        return Err("Account not found.".to_string());
    }
    let label = account_window_label(app_id, account_id);
    let Some(window) = app.get_webview_window(&label) else {
        return Ok(());
    };
    if window.is_focused().unwrap_or(true) {
        return Ok(());
    }
    suspend_one(app, &label, &window);
    Ok(())
}

#[cfg(windows)]
fn suspend_one(app: &AppHandle, label: &str, window: &WebviewWindow) {
    // Record the page before the renderer freezes (v0.16.4): once
    // TrySuspend lands, the URL is not readable again until wake.
    capture_last_url(app, label);
    // WebView2 requires the controller to be hidden (IsVisible == false)
    // before TrySuspend succeeds (otherwise returns ERROR_INVALID_STATE).
    // Minimized, hidden, or already-hidden webviews are eligible.
    let can_suspend = window.is_minimized().unwrap_or(false)
        || !window.is_visible().unwrap_or(true)
        || webview_hidden(window);
    if !can_suspend {
        set_window_memory_target_low(window, true);
        return;
    }
    if try_suspend_webview(window) {
        let title_opt = if let Some(winstate) = app.try_state::<WindowState>() {
            if let Ok(mut tracked) = winstate.inner.lock() {
                if let Some(t) = tracked.get_mut(label) {
                    t.suspended = true;
                    // Visible cue that this window is asleep.
                    Some(format!("💤 {}", t.base_title))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(title) = title_opt {
            let _ = window.set_title(&title);
        }
    }
}

#[cfg(not(windows))]
fn suspend_one(_app: &AppHandle, _label: &str, _window: &WebviewWindow) {
    // No-op: TrySuspend is a WebView2-only API. Linux keeps the webview alive;
    // closing the window is the reclaim path there.
}

/// True when the WebView2 controller reports itself not visible.
#[cfg(windows)]
fn webview_hidden(window: &WebviewWindow) -> bool {
    use windows_core::BOOL;
    let hidden = Arc::new(AtomicBool::new(false));
    let out = hidden.clone();
    let _ = window.with_webview(move |platform| unsafe {
        let mut visible = BOOL(0);
        if platform.controller().IsVisible(&mut visible).is_ok() {
            out.store(!visible.as_bool(), Ordering::Relaxed);
        }
    });
    hidden.load(Ordering::Relaxed)
}

/// Best-effort TrySuspend. Returns whether WebView2 accepted the suspend.
#[cfg(windows)]
fn try_suspend_webview(window: &WebviewWindow) -> bool {
    use std::sync::mpsc;
    use std::time::Duration;
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_3;
    use windows_core::Interface;

    let (tx, rx) = mpsc::sync_channel(1);
    let queued = window.with_webview(move |platform| unsafe {
        let _ = platform.controller().SetIsVisible(false);
        if let Ok(core) = platform.controller().CoreWebView2() {
            // ICoreWebView2 -> ICoreWebView2_3 via QueryInterface.
            if let Ok(core3) = core.cast::<ICoreWebView2_3>() {
                // No completion handler: suspension is fire-and-forget, and
                // the title cue is applied by the caller on success.
                if core3.TrySuspend(None).is_ok() {
                    let _ = tx.send(true);
                    return;
                } else {
                    let _ = platform.controller().SetIsVisible(true);
                }
            }
        }
        let _ = tx.send(false);
    });
    if queued.is_err() {
        return false;
    }
    rx.recv_timeout(Duration::from_millis(1000)).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Window lifecycle helpers for the commands
// ---------------------------------------------------------------------------

/// Who is asking for a tracked window to close. The pin ("Don't close this
/// window", v0.9.9) treats them differently: Background reclaims skip
/// pinned windows silently (unattended — they cannot ask), while User
/// closes divert to the confirm.
/// Close-intent lives in pin.rs next to the plan it drives.
use crate::pin::{CloseIntent, ClosePlan};

pub(crate) fn close_tracked_window(app: &AppHandle, label: &str, intent: CloseIntent) {
    // Final URL read while the webview still exists (v0.16.4). Runs
    // before the pin plan on purpose: even a diverted or skipped close
    // leaves the freshest value behind for the next session write.
    capture_last_url(app, label);
    // v0.15.0: only automatic reclaims bank "Memory saved" — user closes
    // never count.
    let automatic = matches!(intent, CloseIntent::Background);
    // Pinned windows (v0.9.9): the plan is pure and unit-tested —
    // background reclaims skip silently, user closes ask first, and
    // shutdown always proceeds.
    match crate::pin::plan_close(
        intent,
        crate::pin::is_pinned(app, label),
        crate::pin::is_shutting_down(),
    ) {
        ClosePlan::Skip => return,
        ClosePlan::Proceed => {}
        ClosePlan::Confirm => {
            // Never block this thread on the confirm: blocking_show on
            // a command/IPC thread, or during window-event dispatch,
            // parks the caller while the dialog pumps its own loop —
            // the main-thread freeze class from the v0.16.x reports.
            // The helper thread in ask_then_close shows the dialog
            // instead; on approval it records the label in
            // ConfirmedCloses and closes the window, and the
            // CloseRequested handler lets a confirmed label through
            // while the Destroyed handler does the bookkeeping below.
            // A pre-approved label (batch close) skips the dialog.
            if !crate::pin::take_confirmed(app, label) {
                crate::pin::ask_then_close(app, label);
                return;
            }
        }
    }
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.close();
    }
    // v0.18.3: take the entry under the lock, then DROP the guard
    // before pruning. prune() queries has_account_window(), which
    // locks this same WindowState mutex — calling it under the guard
    // self-deadlocks the thread (std Mutex is not reentrant) and
    // froze the whole app on every account-window close.
    let removed = if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(mut tracked) = winstate.inner.lock() {
            tracked.remove(label)
        } else {
            None
        }
    } else {
        None
    };
    // Take the entry so a private window's ephemeral dir can be
    // deleted below; normal windows have nothing to clean.
    if let Some(t) = removed {
        if let Some(dir) = t.private_dir {
            delete_private_session_dir(&dir);
        }
        // v0.15.0: an automatic reclaim banks the last attributed
        // RAM as saved (attribution is still present here — prune
        // runs after). User closes never count.
        if automatic && label.starts_with("acct-") {
            crate::sleep::record_sleep(app, &t.app_id, &t.account_id);
        }
        // v0.15.0: drop per-account process attribution once the
        // account's last window is gone.
        crate::procattr::prune(app, &t.app_id, &t.account_id);
    }
    // The open set changed: persist the session. (The window's own
    // Destroyed handler will fire later and rewrite it again — harmless.)
    crate::session::write_session(app);
    // Windows needs no reaper here: WebView2 tears down a webview's renderer
    // processes when its controller is destroyed, and closing the window
    // destroys the controller (it owns the CoreWebView2), so the OS reclaims
    // the processes with the window close above. On Linux the renderers exit
    // but each account's WebKitNetworkProcess lingers — measured 2026-10-02
    // at ~12 MB each for 7+ minutes after the webviews were gone — hence the
    // reaper below. It stands down entirely while any account window (or a
    // popup child of one) is still open, since attribution would be unsafe.
    #[cfg(target_os = "linux")]
    if label.starts_with("acct-") || label.starts_with("popup-") || label.starts_with("oauth-")
    {
        schedule_network_process_reap(app.clone());
    }
}

/// Snapshot of tracked account windows that still have a live window, for
/// session persistence (v0.9.5): (label, app_id, account_id, opened_at,
/// last_url).
/// Cross-checked against live windows so a stale tracked entry (window
/// closed before its Destroyed handler ran) can never resurrect.
pub(crate) fn live_tracked_accounts(
    app: &AppHandle,
) -> Vec<(String, String, String, u64, Option<String>)> {
    let Some(winstate) = app.try_state::<WindowState>() else {
        return Vec::new();
    };
    let Ok(tracked) = winstate.inner.lock() else {
        return Vec::new();
    };
    tracked
        .iter()
        .filter(|(label, _)| label.starts_with("acct-"))
        .filter(|(label, _)| app.get_webview_window(label).is_some())
        // Private windows never persist: no session.json entry, no restore.
        .filter(|(_, t)| !t.is_private)
        .map(|(label, t)| {
            (
                label.clone(),
                t.app_id.clone(),
                t.account_id.clone(),
                t.opened_at,
                t.last_url.clone(),
            )
        })
        .collect()
}

/// Grace period before checking for orphaned network processes: WebKitGTK
/// usually exits them on its own within seconds, so only kill stragglers.
#[cfg(target_os = "linux")]
const NETWORK_PROCESS_REAP_GRACE: Duration = Duration::from_secs(90);

// kill(2) without the libc crate (which would need a Cargo.toml change):
// libc is always linked into a Rust binary, so a direct extern declaration
// is enough. Linux-only.
#[cfg(target_os = "linux")]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
#[cfg(target_os = "linux")]
const SIGTERM: i32 = 15;

/// Spawn the orphan-network-process reaper: sleep the grace period, then
/// SIGTERM our own lingering WebKitNetworkProcess children — but ONLY when
/// zero tracked account windows remain open AND no other non-main window
/// (popup child, preview) is still alive. With any webview alive, a network
/// process could still belong to it, so the reaper does nothing
/// (fail closed).
///
/// Also called once at startup (v0.9.9): the launcher's own spare ~55 MB
/// network process has zero webview-side consumers (no fetch/XHR/WebSocket
/// and no remote images in the launcher bundle; updater, favicons, and
/// adblock lists all go through Rust), and WebKitGTK respawns the network
/// process on demand if a future launcher feature ever needs it.
#[cfg(target_os = "linux")]
pub(crate) fn schedule_network_process_reap(app: AppHandle) {
    let _ = std::thread::Builder::new()
        .name("appmaka-netproc-reap".to_string())
        .spawn(move || {
            std::thread::sleep(NETWORK_PROCESS_REAP_GRACE);
            let tracked_open = match app.try_state::<WindowState>() {
                Some(winstate) => match winstate.inner.lock() {
                    Ok(tracked) => tracked.keys().any(|l| l.starts_with("acct-")),
                    // Lock poisoned: fail closed, do nothing.
                    Err(_) => true,
                },
                None => true,
            };
            // Popups are not tracked in WindowState, so also check live
            // windows directly: a still-open popup may own the lingering
            // network process.
            let any_window_open = app
                .webview_windows()
                .keys()
                .any(|l| l != "main");
            if tracked_open || any_window_open {
                return;
            }
            reap_orphan_network_processes();
        });
}

/// SIGTERM direct children of our own PID whose cmdline contains
/// "WebKitNetworkProcess". Each account window gets an isolated website
/// data dir, which spawns its own network process; once the last account
/// window is gone these are provably orphaned (their webviews are
/// destroyed) and safe to kill. Scoped strictly to our own children — never
/// a sibling process or the user's own.
#[cfg(target_os = "linux")]
fn reap_orphan_network_processes() {
    let self_pid = std::process::id();
    let Ok(proc) = std::fs::read_dir("/proc") else {
        return;
    };
    let mut killed = 0u32;
    for entry in proc.filter_map(|e| e.ok()) {
        let pid: i32 = match entry.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if !cmdline
            .windows(b"WebKitNetworkProcess".len())
            .any(|w| w == b"WebKitNetworkProcess")
        {
            continue;
        }
        // Direct child of our own PID only.
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let is_child = status.lines().any(|l| {
            l.strip_prefix("PPid:")
                .map(|v| v.trim() == self_pid.to_string())
                .unwrap_or(false)
        });
        if !is_child {
            continue;
        }
        // SAFETY: kill(2) with a PID verified above as our own direct child
        // and SIGTERM; libc is always linked.
        if unsafe { kill(pid, SIGTERM) } == 0 {
            killed += 1;
            eprintln!("[appmaka] reaped orphan WebKitNetworkProcess pid={pid}");
        } else {
            eprintln!("[appmaka] could not reap WebKitNetworkProcess pid={pid}");
        }
    }
    if killed > 0 {
        eprintln!("[appmaka] reaped {killed} orphan WebKitNetworkProcess(es)");
    }
}

/// Close every open window belonging to an app (used before remove_app).
/// User intent: pinned windows ask first (v0.9.9).
pub fn close_account_windows(app: &AppHandle, app_id: &str) {
    let prefix = format!("acct-{app_id}-");
    let labels: Vec<String> = app
        .webview_windows()
        .keys()
        .filter(|l| l.starts_with(&prefix))
        .cloned()
        .collect();
    for label in labels {
        close_tracked_window(app, &label, CloseIntent::User);
    }
}

/// Close one account's window if open (used before remove_account).
/// User intent: a pinned window asks first (v0.9.9).
pub fn close_account_window(app: &AppHandle, app_id: &str, account_id: &str) {
    close_tracked_window(
        app,
        &account_window_label(app_id, account_id),
        CloseIntent::User,
    );
}

/// Push a settings change to already-open windows of an app without rebuilds.
/// Windows whose account overrides adblock (v0.8.1) keep their override —
/// only inheriting accounts get the app-level push.
pub fn set_app_adblock_enabled(app: &AppHandle, app_id: &str, enabled: bool) {
    let overridden: std::collections::HashSet<String> = app
        .try_state::<AppStore>()
        .and_then(|s| s.get(app_id).ok())
        .map(|web_app| {
            web_app
                .accounts
                .iter()
                .filter(|a| a.adblock_enabled.is_some())
                .map(|a| account_window_label(app_id, &a.id))
                .collect()
        })
        .unwrap_or_default();
    if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(tracked) = winstate.inner.lock() {
            for (label, t) in tracked.iter() {
                if t.app_id == app_id && !overridden.contains(label) {
                    t.adblock_enabled.store(enabled, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Push one account's effective adblock value to its open window, if any.
/// Called after per-account edits in `update_account` — the per-account
/// mirror of `set_app_adblock_enabled`.
pub fn set_account_adblock_enabled(
    app: &AppHandle,
    app_id: &str,
    account_id: &str,
    enabled: bool,
) {
    let label = account_window_label(app_id, account_id);
    if let Some(winstate) = app.try_state::<WindowState>() {
        if let Ok(tracked) = winstate.inner.lock() {
            if let Some(t) = tracked.get(&label) {
                t.adblock_enabled.store(enabled, Ordering::Relaxed);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// v0.7.0 commands: RAM dashboard support
// ---------------------------------------------------------------------------

/// One open account window, for the RAM dashboard.
// Interim: the coordinator registers the v0.7.0 commands in main.rs; until
// then the dead_code allows keep `-D warnings` green. They are inert once
// the commands are referenced.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct OpenAccountInfo {
    pub label: String,
    pub app_id: String,
    pub account_id: String,
    pub app_name: String,
    pub account_label: String,
    pub focused: bool,
    /// v0.9.9: "Don't close this window" state, so the dashboard can
    /// render the toggle without a second round-trip.
    pub pinned: bool,
    /// v0.15.0: unread count parsed from the live page title. `None`
    /// means no badge — either the title carries no count or the user
    /// has visited the window since the last title change.
    pub unread: Option<u32>,
    /// v0.16.4: the webview is frozen in place via TrySuspend
    /// (Windows-only). A suspended window keeps its DOM and scroll and
    /// wakes on focus — unlike a closed ("sleeping") window, which is
    /// destroyed and must reload. Exposed so the UI can stop
    /// conflating the two states.
    pub suspended: bool,
}

/// Close every open account window. Returns how many were closed. Sessions
/// stay on disk, so reopening an account restores its login. Transient
/// popup children ("popup-*"/"oauth-*") are closed too but not counted —
/// leaving them open would defeat the RAM dashboard's "close all".
///
/// Async on purpose (v0.18.0): the batch pin confirm below blocks on a
/// native dialog whose show posts to the main-thread event loop, and
/// Tauri 2 runs *sync* commands on the main thread — a pinned close-all
/// could park the whole UI in that modal (the v0.13.1 strip deadlock,
/// one layer up). As an async command this runs on a runtime worker
/// thread, where blocking on the dialog is safe.
///
/// Pinned windows (v0.9.9, "Don't close this window") get ONE batch confirm
/// for the whole call, not one per window. "Keep open" (or dismiss) closes
/// the unpinned windows and leaves the pinned ones alone.
#[tauri::command]
#[allow(dead_code)]
pub async fn close_all_account_windows(app: AppHandle) -> Result<usize, String> {
    let labels: Vec<String> = match app.try_state::<WindowState>() {
        Some(winstate) => match winstate.inner.lock() {
            Ok(tracked) => tracked
                .keys()
                .filter(|l| l.starts_with("acct-"))
                .cloned()
                .collect(),
            Err(e) => return Err(format!("window state lock poisoned: {e}")),
        },
        None => Vec::new(),
    };
    // One-shot approvals land in ConfirmedCloses, so the per-window guard
    // inside close_tracked_window lets approved labels through without
    // re-asking.
    //
    // v0.10.0: tabbed windows own a live webview each, so the RAM
    // dashboard's "close all" includes them. Their pins key off
    // `tabbed:{group}` (stable across tab switches), folded into the same
    // one batch confirm.
    let tabbed: Vec<(String, String)> = app
        .try_state::<crate::tabs::TabState>()
        .map(|ts| crate::tabs::all_groups_for_close_all(&ts))
        .unwrap_or_default();
    let mut confirm_labels = labels.clone();
    confirm_labels.extend(tabbed.iter().map(|(_, key)| key.clone()));
    let approved = crate::pin::confirm_batch_close(&app, &confirm_labels);
    let mut closed = 0usize;
    for label in &labels {
        if crate::pin::is_pinned(&app, label) && !approved.contains(label) {
            continue;
        }
        close_tracked_window(&app, label, CloseIntent::User);
        closed += 1;
    }
    for (id, key) in &tabbed {
        if crate::pin::is_pinned(&app, key) && !approved.contains(key) {
            continue;
        }
        if let Some(ts) = app.try_state::<crate::tabs::TabState>() {
            crate::tabs::close_group(&app, &ts, id);
            closed += 1;
        }
    }
    // Popups are not tracked in WindowState; sweep them by live-window
    // label so no account-related webview survives a close-all.
    for (label, window) in app.webview_windows() {
        if label.starts_with("popup-") || label.starts_with("oauth-") {
            let _ = window.close();
        }
    }
    Ok(closed)
}

/// Whether a dashboard close request may address this label. The RAM
/// dashboard only lists account windows (`acct-*`) and the search window,
/// so the close command refuses anything else — a wrong label must never
/// be able to close the launcher itself. Pure for unit tests.
pub(crate) fn close_label_allowed(label: &str) -> bool {
    label.starts_with("acct-")
        || label.starts_with("popup-")
        || label == crate::websearch::SEARCH_WINDOW_LABEL
}

/// Close one open window by its exact label (v0.9.10: the RAM dashboard's
/// per-row close button, and the launcher tile menu's "Close window"
/// entry). The caller passes the label straight from
/// `list_open_account_windows`, so the mapping is exact by construction —
/// the row key IS the window label. Pin handling reuses the standard
/// User-intent flow: pinned windows get the one native confirm
/// ("This window is pinned. Close it anyway?"), unpinned close at once.
/// Async so a deferred pin confirm never parks command dispatch:
/// close_tracked_window routes Confirm through ask_then_close's helper
/// thread instead of blocking on the dialog (sync commands run on the
/// main thread in Tauri 2, and a dialog there would wait on its own
/// event loop).
#[tauri::command]
pub async fn close_open_window(app: AppHandle, label: String) -> Result<(), String> {
    if !close_label_allowed(&label) {
        return Err(format!("not a closeable window: {label}"));
    }
    close_tracked_window(&app, &label, CloseIntent::User);
    Ok(())
}

/// Enter fullscreen the way the user expects: on Windows, the monitor
/// under the cursor (v0.13.13); elsewhere, the window's own monitor.
pub(crate) fn enter_fullscreen_smart(window: &tauri::WebviewWindow) {
    #[cfg(windows)]
    enter_fullscreen_on_cursor_monitor(window);
    #[cfg(not(windows))]
    {
        let _ = window.set_fullscreen(true);
    }
}

/// v0.13.18: the dashboard's "Full screen" button — the discoverable
/// counterpart to F11 / a page's own fullscreen button. Targets the
/// same windows the dashboard lists (account windows, popups, the
/// search window) plus tabbed groups.
#[tauri::command]
pub fn toggle_window_fullscreen(app: AppHandle, label: String) -> Result<(), String> {
    if !(close_label_allowed(&label) || label.starts_with("tabbed-")) {
        return Err("That window can't be full-screened.".to_string());
    }
    let window = app
        .get_webview_window(&label)
        .ok_or_else(|| "That window is not open.".to_string())?;
    if window.is_fullscreen().unwrap_or(false) {
        let _ = window.set_fullscreen(false);
    } else {
        enter_fullscreen_smart(&window);
    }
    Ok(())
}

/// List every open account window with its app/account names and focus
/// state, for the RAM dashboard.
#[tauri::command]
#[allow(dead_code)]
pub fn list_open_account_windows(
    app: AppHandle,
    store: State<'_, AppStore>,
) -> Vec<OpenAccountInfo> {
    let tracked: Vec<(String, String, String, String, bool, bool)> = match app.try_state::<WindowState>() {
        Some(winstate) => match winstate.inner.lock() {
            Ok(map) => map
                .iter()
                .filter(|(l, _)| l.starts_with("acct-"))
                .map(|(l, t)| {
                    (
                        l.clone(),
                        t.app_id.clone(),
                        t.account_id.clone(),
                        t.page_title.clone(),
                        t.page_title_seen,
                        t.suspended,
                    )
                })
                .collect(),
            Err(_) => return Vec::new(),
        },
        None => return Vec::new(),
    };
    tracked
        .into_iter()
        .map(|(label, app_id, account_id, page_title, seen, suspended)| {
            let (app_name, account_label) = store
                .get(&app_id)
                .ok()
                .and_then(|a| {
                    a.accounts
                        .into_iter()
                        .find(|ac| ac.id == account_id)
                        .map(|ac| (a.name, ac.label))
                })
                .unwrap_or_else(|| {
                    ("Unknown app".to_string(), "Unknown account".to_string())
                });
            let focused = app
                .get_webview_window(&label)
                .and_then(|w| w.is_focused().ok())
                .unwrap_or(false);
            OpenAccountInfo {
                pinned: crate::pin::is_pinned(&app, &label),
                label,
                app_id,
                account_id,
                app_name,
                account_label,
                focused,
                // v0.15.0: unread badge — only when the title carries a
                // trusted count AND the user hasn't visited since.
                unread: if seen {
                    None
                } else {
                    crate::badges::parse_unread_count(&page_title)
                },
                suspended,
            }
        })
        // The search window has no tile, so the dashboard is its only
        // home for the "Don't close this window" toggle (v0.9.9).
        .chain(
            app.get_webview_window(crate::websearch::SEARCH_WINDOW_LABEL)
                .map(|w| {
                    let query =
                        crate::session::search_live_query(&app).unwrap_or_default();
                    let account_label = if query.trim().is_empty() {
                        "Web search".to_string()
                    } else {
                        query
                    };
                    OpenAccountInfo {
                        label: crate::websearch::SEARCH_WINDOW_LABEL.to_string(),
                        app_id: "websearch".to_string(),
                        account_id: String::new(),
                        app_name: "Web search".to_string(),
                        account_label,
                        focused: w.is_focused().unwrap_or(false),
                        pinned: crate::pin::is_pinned(
                            &app,
                            crate::websearch::SEARCH_WINDOW_LABEL,
                        ),
                        unread: None,
                        // The search window is never TrySuspend-frozen.
                        suspended: false,
                    }
                }),
        )
        // v0.9.11: popups are transient (never session-restored, never
        // pinned), but they were invisible to the launcher — a popup lost
        // behind other windows had no way back. List them with a live
        // URL read so the dashboard can show, focus (click), close, and
        // "add to applications" them.
        .chain(app.webview_windows().values().filter_map(|w| {
            let label = w.label().to_string();
            if !label.starts_with("popup-") {
                return None;
            }
            let url_str = w.url().map(|u| u.to_string()).unwrap_or_default();
            let app_name = url_str
                .parse::<url::Url>()
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .map(|h| prettified_host(&h))
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "Popup".to_string());
            Some(OpenAccountInfo {
                label,
                app_id: String::new(),
                account_id: String::new(),
                app_name,
                account_label: url_str,
                focused: w.is_focused().unwrap_or(false),
                pinned: false,
                unread: None,
                // Popups are never TrySuspend-frozen.
                suspended: false,
            })
        }))
        .collect()
}

/// Prettified host for a dashboard popup row: strip a leading `www.` and
/// capitalize. (Mirrors `preview::prettified_domain`, which works from a
/// full URL string.)
fn prettified_host(host: &str) -> String {
    let host = host.strip_prefix("www.").unwrap_or(host);
    let mut chars = host.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// One descendant process in the memory snapshot.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ChildMemory {
    pub name: String,
    pub rss_kb: u64,
}

/// Process-group memory snapshot: the main process plus all descendants
/// (WebKit/WebView2 helper processes), via the cross-platform sysinfo
/// crate. Sync is fine: a single process-list scan, no window ops.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct MemorySnapshot {
    /// RSS of the whole group (self + all descendants), KiB.
    pub total_rss_kb: u64,
    /// RSS of the main appmaka process alone, KiB.
    pub main_rss_kb: u64,
    /// Top 12 descendant processes by RSS.
    pub top_children: Vec<ChildMemory>,
    /// False when sysinfo could not read our own process (unexpected on the
    /// supported Linux/Windows targets).
    pub supported: bool,
}

#[tauri::command]
#[allow(dead_code)]
pub fn memory_snapshot() -> Result<MemorySnapshot, String> {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

    let empty = MemorySnapshot {
        total_rss_kb: 0,
        main_rss_kb: 0,
        top_children: Vec::new(),
        supported: false,
    };
    // Processes only, never tasks: sysinfo's default (`System::new_all()`)
    // enumerates every *thread* as a separate entry, and threads share
    // their process's whole address space — summing them multi-counts the
    // same RSS once per thread and the total explodes past physical RAM
    // (seen: 23 GB on an 8 GB box). `without_tasks()` keeps one entry per
    // real process.
    //
    // v0.13.1: refresh MEMORY ONLY. `ProcessRefreshKind::everything()` also
    // pulls disk I/O counters, user/SID lookups, and PEB reads (environ,
    // cmd, cwd, exe) for every process on the system — the SID lookups can
    // stall for minutes on domain-joined machines and the PEB reads can
    // block on protected processes, hanging this command (and the whole
    // "Reading memory numbers…" dashboard) for minutes. The process name
    // and parent PID come from the toolhelp snapshot itself, so memory is
    // all we need.
    let sys = System::new_with_specifics(
        RefreshKind::nothing().with_processes(
            ProcessRefreshKind::nothing()
                .with_memory()
                .without_tasks(),
        ),
    );
    let self_pid = Pid::from_u32(std::process::id());
    let Some(self_proc) = sys.process(self_pid) else {
        return Ok(empty);
    };
    // Descendants: every process whose ancestry reaches our own PID.
    let mut children: Vec<ChildMemory> = Vec::new();
    for (pid, proc_) in sys.processes() {
        if *pid == self_pid {
            continue;
        }
        let mut ancestor = proc_.parent();
        let mut is_descendant = false;
        while let Some(p) = ancestor {
            if p == self_pid {
                is_descendant = true;
                break;
            }
            ancestor = sys.process(p).and_then(|pp| pp.parent());
        }
        if is_descendant {
            children.push(ChildMemory {
                name: proc_.name().to_string_lossy().into_owned(),
                rss_kb: proc_.memory() / 1024,
            });
        }
    }
    let main_rss_kb = self_proc.memory() / 1024;
    let total_rss_kb = main_rss_kb + children.iter().map(|c| c.rss_kb).sum::<u64>();
    children.sort_by_key(|c| std::cmp::Reverse(c.rss_kb));
    children.truncate(12);
    Ok(MemorySnapshot {
        total_rss_kb,
        main_rss_kb,
        top_children: children,
        supported: true,
    })
}

/// Sign an account out everywhere: close its window first (Windows locks
/// the session files while a webview is alive), then wipe its session
/// directory and recreate it empty. The account record, its app, and every
/// other account are untouched — reopening the account starts a fresh
/// login. Tauri exposes the snake_case params as camelCase to JS:
/// `invoke("forget_login", { appId, accountId })`.
#[tauri::command]
#[allow(dead_code)]
pub fn forget_login(
    app: AppHandle,
    store: State<'_, AppStore>,
    app_id: String,
    account_id: String,
) -> Result<(), String> {
    // The user already confirmed forgetting: clear the pin so the pin
    // guard can't keep the window open against the session wipe below.
    crate::pin::set_pinned(&app, &account_window_label(&app_id, &account_id), false);
    close_account_window(&app, &app_id, &account_id);
    store.forget_account_session(&app_id, &account_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dashboard close button passes the row's exact label; the
    /// command must accept exactly the windows the dashboard can list
    /// (account windows, popups, and the search window) and refuse
    /// everything else. Refusing "main" is the critical case: a
    /// wrong-label close must never be able to kill the launcher itself.
    /// OAuth modals stay refused: they are transient sign-in windows the
    /// dashboard never lists.
    #[test]
    fn close_label_allows_only_dashboard_windows() {
        assert!(close_label_allowed("acct-app1-acct1"));
        assert!(close_label_allowed("acct-a-b"));
        assert!(close_label_allowed("popup-3"));
        assert!(close_label_allowed(crate::websearch::SEARCH_WINDOW_LABEL));
        assert!(!close_label_allowed("main"));
        assert!(!close_label_allowed(""));
        assert!(!close_label_allowed("oauth-3"));
        assert!(!close_label_allowed("xacct-app1-acct1"));
        assert!(!close_label_allowed("ACCT-app1-acct1"));
        assert!(!close_label_allowed("xpopup-3"));
    }

    /// The v0.16.4 badge rule: focusing an awake window clears its
    /// unread badge (the user is looking at it), but focusing a window
    /// that was suspended must not — its frozen renderer may never
    /// fire another title event, so a clear would erase an unseen
    /// count for good.
    #[test]
    fn focus_clears_badge_only_when_window_was_awake() {
        assert!(focus_clears_badge(false));
        assert!(!focus_clears_badge(true));
    }

    #[test]
    fn cursor_chrome_off_is_empty() {
        assert!(cursor_chrome_js("off", "#8b5cf6", "139,92,246").is_empty());
        assert!(cursor_chrome_js("", "#8b5cf6", "139,92,246").is_empty());
        assert!(cursor_chrome_js("bogus", "#8b5cf6", "139,92,246").is_empty());
    }

    #[test]
    fn cursor_chrome_styles() {
        for style in ["dot", "ring", "trail", "halo", "pulse", "orbit", "comet", "crosshair", "ripple", "stardust"] {
            let js = cursor_chrome_js(style, "#8b5cf6", "139,92,246");
            assert!(!js.is_empty(), "{style}");
            // Native cursor hidden; dots never intercept clicks.
            assert!(js.contains("cursor:none"), "{style}");
            assert!(js.contains("pointer-events:none"), "{style}");
            // Transform-only movement, one rAF loop.
            assert!(js.contains("requestAnimationFrame"), "{style}");
            assert!(js.contains("translate("), "{style}");
        }
        // Trail has followers; dot/ring are a single node.
        assert!(cursor_chrome_js("trail", "#8b5cf6", "139,92,246").contains("var N = 6"));
        assert!(cursor_chrome_js("dot", "#8b5cf6", "139,92,246").contains("var N = 1"));
        // v0.13.10 styles: halo glows, orbit has a satellite, comet is
        // a glowing trail.
        assert!(cursor_chrome_js("halo", "#8b5cf6", "139,92,246").contains("box-shadow"));
        assert!(cursor_chrome_js("halo", "#8b5cf6", "139,92,246").contains("var N = 1"));
        assert!(cursor_chrome_js("orbit", "#8b5cf6", "139,92,246").contains("var N = 2"));
        assert!(cursor_chrome_js("comet", "#8b5cf6", "139,92,246").contains("var N = 6"));
        assert!(cursor_chrome_js("comet", "#8b5cf6", "139,92,246").contains("boxShadow"));
        assert!(cursor_chrome_js("pulse", "#8b5cf6", "139,92,246").contains("scale("));
    }

    #[test]
    fn cursor_chrome_hardened_lifecycle() {
        // v0.16.3 hardening, one assert per failure mode:
        let js = cursor_chrome_js("trail", "#8b5cf6", "139,92,246");
        // Orphaned chrome from a lost controller is swept before the new
        // dots are created (definition + sweep + destroy = 3 calls).
        assert!(js.matches("removeChrome()").count() >= 3, "{js}");
        // Losing window focus hides the dots even when the document
        // never fires mouseleave, and destroy unregisters the net.
        assert!(js.contains("window.addEventListener(\"blur\", onLeave)"), "{js}");
        assert!(js.contains("window.removeEventListener(\"blur\", onLeave)"), "{js}");
        // The frame loop writes nothing while the pointer is out.
        assert!(js.contains("if (!inside) { rafId = requestAnimationFrame(frame); return; }"), "{js}");
        // The watchdog treats a partial trail as broken, not just total
        // removal.
        assert!(
            js.contains("querySelectorAll(\"[data-appmaka=cursor-dot]\").length !== N"),
            "{js}"
        );
    }

    #[test]
    fn cursor_chrome_iframe_and_watchdog_repair() {
        // v0.16.5: frame boxes keep the native cursor (over an embed
        // the top document is starved of mousemove, so suppressing it
        // there leaves no visible pointer), and the watchdog rebuilds
        // the dots once before tearing down to the native cursor.
        let js = cursor_chrome_js("dot", "#8b5cf6", "139,92,246");
        assert!(js.contains("iframe{cursor:auto!important}"), "{js}");
        assert!(js.contains("function buildDots()"), "{js}");
        assert!(js.contains("wdRepaired"), "{js}");
        // v0.16.6: a throwing watchdog rebuild destroys immediately,
        // and a failed install disarms the head observer. The
        // disconnect appears three times: the observer's own
        // fire-once callback, destroy(), and the install catch.
        assert!(
            js.contains("try { buildDots(); } catch (e) { destroy(); }"),
            "{js}"
        );
        assert_eq!(js.matches("if (obs) obs.disconnect();").count(), 3, "{js}");
    }

    #[test]
    fn webview2_browser_args_contract() {
        // Custom args replace wry's defaults rather than adding to
        // them, so the defaults ride along explicitly; and every
        // builder on a session folder passes this one string (the
        // v0.16.5 popup breakage was a second, different set).
        assert!(WEBVIEW2_BROWSER_ARGS.contains("msSmartScreenProtection"));
        assert!(WEBVIEW2_BROWSER_ARGS.contains("msWebOOUI"));
        assert!(WEBVIEW2_BROWSER_ARGS.contains("msPdfOOUI"));
        assert!(WEBVIEW2_BROWSER_ARGS.contains("TrimOnMemoryPressure"));
        // The 256 MB heap cap was dropped in v0.16.6 (see the const).
        assert!(!WEBVIEW2_BROWSER_ARGS.contains("max-old-space-size"));
    }

    #[test]
    fn cursor_chrome_styles_v016() {
        // The v0.16.0 batch installs like the originals and carries the
        // caller's color (no leftover brand violet when recolored).
        for style in [
            "diamond", "arrow", "plus", "star", "bullseye", "blob", "neon", "ghost",
            "firefly", "snake", "rainbow", "bubbles",
        ] {
            let js = cursor_chrome_js(style, "#fb7185", "251,113,133");
            assert!(!js.is_empty(), "{style}");
            assert!(js.contains("cursor:none"), "{style}");
            assert!(js.contains("__appmakaCursor"), "{style}");
        }
        assert!(cursor_chrome_js("snake", "#8b5cf6", "139,92,246").contains("var N = 8"));
        assert!(cursor_chrome_js("rainbow", "#8b5cf6", "139,92,246").contains("var N = 8"));
        assert!(cursor_chrome_js("bubbles", "#8b5cf6", "139,92,246").contains("var N = 5"));
        // Color substitution reaches shapes and glows.
        let rose = cursor_chrome_js("neon", "#fb7185", "251,113,133");
        assert!(rose.contains("#fb7185"));
        assert!(rose.contains("251,113,133"));
        assert!(!rose.contains("#8b5cf6"));
        // v0.16.1: the controller identity includes the color, so a
        // color-only live change reinstalls instead of no-oping on the
        // same-style guard.
        assert!(rose.contains("=== IDENT"));
        assert!(rose.contains("\"#fb7185\""));
        // Rainbow recolors its own dots in JS, hues present.
        assert!(cursor_chrome_js("rainbow", "#8b5cf6", "139,92,246").contains("hsl("));
    }

    #[test]
    fn cursor_palette_resolution() {
        assert_eq!(cursor_palette("violet", "indigo").0, "#8b5cf6");
        assert_eq!(cursor_palette("bogus", "indigo").0, "#8b5cf6");
        assert_eq!(cursor_palette("rose", "indigo").1, "251,113,133");
        // "accent" follows the accent's bright tone.
        assert_eq!(cursor_palette("accent", "rose").0, "#fda4af");
        assert_eq!(cursor_palette("accent", "rose").1, "253,164,175");
        assert_eq!(hex_rgb("#38bdf8"), "56,189,248");
    }

    #[test]
    fn cursor_scripts_register_a_destroyable_controller() {
        // Live-apply contract: every install script registers
        // window.__appmakaCursor with its style + destroy(), and
        // re-running with a different style destroys the old one.
        for style in ["dot", "ring", "trail", "halo", "pulse", "orbit", "comet", "crosshair", "ripple", "stardust"] {
            let js = cursor_chrome_js(style, "#8b5cf6", "139,92,246");
            assert!(js.contains("__appmakaCursor"), "{style}");
            assert!(js.contains("destroy"), "{style}");
            assert!(js.contains(&format!("\"{style}\"")), "{style}");
        }
    }

    #[test]
    fn cursor_install_fails_visible_never_cursorless() {
        // Robustness contract (his "sometimes I can't see my cursor"):
        // the native cursor is hidden only AFTER the drawn dots exist,
        // a failed install strips its own chrome in the catch, and a
        // watchdog restores/tears down if the page removes our DOM.
        let js = cursor_chrome_js("trail", "#8b5cf6", "139,92,246");
        let dots_at = js.find("nodes.push").unwrap();
        let hide_at = js.find("Hide the native cursor only now").unwrap();
        assert!(hide_at > dots_at, "native cursor hidden before dots exist");
        let catch_at = js.find("catch(e)").unwrap();
        assert!(
            js[catch_at..].contains("data-appmaka=cursor-dot"),
            "catch path must strip injected chrome"
        );
        assert!(js.contains("setInterval"), "watchdog missing");
        assert!(js.contains("clearInterval"), "destroy must stop the watchdog");
        assert!(js.contains("removeChrome()"), "shared rollback helper missing");
    }

    #[test]
    fn cursor_live_apply_off_is_teardown_only() {
        let js = cursor_live_apply_js("off", "#8b5cf6", "139,92,246");
        assert!(!js.is_empty());
        assert!(js.contains("__appmakaCursor"));
        assert!(js.contains("destroy"));
        // No install: no dots are created and the native cursor stays.
        assert!(!js.contains("cursor-dot"));
        assert!(!js.contains("cursor:none"));
        // A live style applies the full install script verbatim.
        assert_eq!(
            cursor_live_apply_js("ring", "#8b5cf6", "139,92,246"),
            cursor_chrome_js("ring", "#8b5cf6", "139,92,246")
        );
    }

    #[test]
    fn popup_opens_plain_matrix() {
        // Per-app allow always opens plain popups.
        assert!(popup_opens_plain("allow", false));
        assert!(popup_opens_plain("allow", true));
        // The global override opens them under a block policy too.
        assert!(popup_opens_plain("block", true));
        // Block without the override stays blocked (allowlist path).
        assert!(!popup_opens_plain("block", false));
        assert!(!popup_opens_plain("", false));
    }

    #[test]
    fn popup_chain_trust_matrix() {
        // Inside an allowed popup chain, hop 2+ opens plain even under
        // a block policy with no global override — the chain was
        // already vetted at hop 1 (his "pop ups dont open pop ups").
        assert!(popup_allowed_plain(true, "block", false));
        assert!(popup_allowed_plain(true, "", false));
        // Root windows are unchanged: no trust, policy decides.
        assert!(popup_allowed_plain(false, "allow", false));
        assert!(popup_allowed_plain(false, "block", true));
        assert!(!popup_allowed_plain(false, "block", false));
    }

    #[test]
    fn cursor_window_labels_cover_injected_windows_only() {
        assert!(cursor_window_label("acct-app1-acct1"));
        assert!(cursor_window_label("tabbed-group1-g0"));
        assert!(cursor_window_label(crate::websearch::SEARCH_WINDOW_LABEL));
        // v0.13.18 injected the cursor into contained windows and
        // previews too — the broadcast set must match the built set.
        assert!(cursor_window_label("popup-3"));
        assert!(cursor_window_label("oauth-1"));
        assert!(cursor_window_label("preview-abc-site"));
        assert!(!cursor_window_label("preview-abc-header"));
        assert!(!cursor_window_label("main"));
        assert!(!cursor_window_label("clipboard"));
    }
}


